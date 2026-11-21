//! A rodio source decoding with Symphonia, adapted from Psysonic's `psysonic-audio/src/decode`
//! (`decoder.rs`, `decoder_streaming.rs`): gapless trimming where the container reports the
//! encoder gap, corrupt frames dropped instead of ending the track, and accurate seeking.

use std::num::{NonZeroU16, NonZeroU32};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use rodio::Source;
use symphonia::core::audio::{AudioSpec, GenericAudioBufferRef};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, SeekedTo};
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, Timestamp};
use tracing::{debug, warn};

use super::codec::registry;
use super::probe_gate::ProbeSeekGate;

/// Max retries for packet-read errors while refining a seek.
const DECODE_MAX_RETRIES: usize = 3;
/// Consecutive undecodable frames before a stream is given up (a few corrupt MP3 frames are
/// dropped silently, like VLC does).
const MAX_CONSECUTIVE_DECODE_ERRORS: usize = 100;
/// Read-ahead of the stream reader.
const READ_AHEAD: usize = 512 * 1024;

/// Codecs whose encoder delay and padding Symphonia reads from the container and trims itself.
/// MP3 carries them in the Xing/LAME header; Symphonia applies them only with gapless decoding on.
fn should_use_builtin_gapless(codec_name: &str) -> bool {
    codec_name == "mp3"
}

/// Whether the demuxer reported an encoder gap to trim (a delay or padding).
fn encoder_gap_reported(delay: Option<u32>, padding: Option<u32>) -> bool {
    delay.unwrap_or(0) > 0 || padding.unwrap_or(0) > 0
}

/// Whether a format hint names an MP4 container (its demuxer needs to seek during the probe).
fn is_mp4(hint: Option<&str>) -> bool {
    matches!(hint, Some("mp4" | "m4a" | "m4b" | "alac"))
}

/// Containers whose demuxer needs to seek during the probe to be seekable later: Ogg records the
/// end of its byte range there (without it every seek fails), AIFF may have to find `COMM` past
/// the audio data.
fn seeks_while_probing(hint: Option<&str>) -> bool {
    matches!(hint, Some("ogg" | "oga" | "opus" | "aif" | "aiff" | "aifc"))
}

pub struct Decoder {
    decoder: Box<dyn AudioDecoder>,
    current_frame_offset: usize,
    format: Box<dyn FormatReader>,
    total_duration: Option<Time>,
    /// Interleaved samples of the packet being played.
    buffer: Vec<f32>,
    spec: AudioSpec,
    /// Symphonia already removed the encoder delay and padding.
    builtin_gapless: bool,
    consecutive_decode_errors: usize,
}

impl Decoder {
    /// Probes and opens a stream. `hint` is the container's file extension (`flac`, `ogg`, `m4a`...),
    /// if known. `random_access`: reading anywhere in the source is cheap (a cached file, or ranged
    /// requests), so the probe may seek. Blocks until enough of the stream has arrived to know its
    /// format.
    pub fn open(media: Box<dyn MediaSource>, hint: Option<&str>, random_access: bool) -> Result<Self, String> {
        let gate_needed = !is_mp4(hint) && !(random_access && seeks_while_probing(hint));
        let gate = gate_needed.then(|| Arc::new(AtomicBool::new(false)));
        let media: Box<dyn MediaSource> = match &gate {
            Some(seekable) => Box::new(ProbeSeekGate {
                inner: media,
                seekable: seekable.clone(),
            }),
            None => media,
        };
        let mss = MediaSourceStream::new(media, MediaSourceStreamOptions { buffer_len: READ_AHEAD });
        let mut probe_hint = Hint::new();
        if let Some(ext) = hint {
            probe_hint.with_extension(ext);
        }
        let mut format = symphonia::default::get_probe()
            .probe(&probe_hint, mss, FormatOptions::default(), MetadataOptions::default())
            .map_err(|e| format!("format probe failed: {e}"))?;
        // The probe is done: seeking is fine again (for scrubbing).
        if let Some(seekable) = &gate {
            seekable.store(true, std::sync::atomic::Ordering::Relaxed);
        }

        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.as_ref().and_then(|c| c.audio()).is_some())
            .ok_or("the stream holds no audio track")?;
        let track_id = track.id;
        let (delay, padding) = (track.delay, track.padding);
        let audio_params = track
            .codec_params
            .as_ref()
            .and_then(|c| c.audio())
            .ok_or("the track has no audio codec parameters")?
            .clone();
        let codec_name = registry()
            .get_audio_decoder(audio_params.codec)
            .map(|d| d.codec.info.short_name)
            .unwrap_or("?");
        debug!(codec = codec_name, rate = ?audio_params.sample_rate, "stream opened");

        // Only a frame count the container measured: with the probe gate installed Symphonia can't
        // have estimated one from the MP3 bitrate (it does so only on a seekable source).
        let mp3_frame_count_may_be_estimated = should_use_builtin_gapless(codec_name) && gate.is_none();
        let total_duration = track
            .time_base
            .zip(track.num_frames)
            .and_then(|(base, frames)| Timestamp::try_from(frames).ok().and_then(|ts| base.calc_time(ts)))
            .filter(|t| t.as_secs_f64() > 0.0)
            .filter(|_| !mp3_frame_count_may_be_estimated);
        let builtin_gapless = should_use_builtin_gapless(codec_name) && encoder_gap_reported(delay, padding);

        let mut decoder = registry()
            .make_audio_decoder(&audio_params, &AudioDecoderOptions::default().gapless(builtin_gapless))
            .map_err(|e| format!("codec init failed ({codec_name}): {e}"))?;

        // Decode up to the first packet that holds audio (with trimming on, the first MP3 packet can
        // decode to nothing).
        let mut errors = 0;
        let (spec, buffer) = loop {
            let packet = match format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => break Self::buffer_at_end(decoder.last_decoded())?,
                Err(symphonia::core::errors::Error::IoError(ref io))
                    if io.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break Self::buffer_at_end(decoder.last_decoded())?;
                }
                Err(e) => return Err(format!("reading audio packets failed: {e}")),
            };
            if packet.track_id != track_id {
                continue;
            }
            match decoder.decode(&packet) {
                Ok(d) => {
                    let buffer = Self::make_buffer(&d);
                    if !buffer.is_empty() {
                        break (d.spec().clone(), buffer);
                    }
                }
                Err(symphonia::core::errors::Error::DecodeError(msg)) => {
                    errors += 1;
                    warn!("dropped a corrupt frame while opening: {msg}");
                    if errors >= MAX_CONSECUTIVE_DECODE_ERRORS {
                        return Err("too many consecutive decode errors".into());
                    }
                }
                Err(e) => return Err(format!("decode error: {e}")),
            }
        };

        Ok(Self {
            decoder,
            current_frame_offset: 0,
            format,
            total_duration,
            buffer,
            spec,
            builtin_gapless,
            consecutive_decode_errors: 0,
        })
    }

    /// The end of the stream during opening: fine if something was decoded, a failure otherwise
    /// (a truncated stream must not become a silent "successful" track).
    fn buffer_at_end(last: GenericAudioBufferRef<'_>) -> Result<(AudioSpec, Vec<f32>), String> {
        let buffer = Self::make_buffer(&last);
        if buffer.is_empty() {
            return Err("end of stream before any audio could be decoded".into());
        }
        Ok((last.spec().clone(), buffer))
    }

    fn make_buffer(decoded: &GenericAudioBufferRef<'_>) -> Vec<f32> {
        let mut buffer = Vec::new();
        decoded.copy_to_vec_interleaved(&mut buffer);
        buffer
    }

    /// After a coarse seek, decodes up to the exact requested timestamp.
    fn refine_position(&mut self, seek_res: SeekedTo) -> Result<(), String> {
        let mut samples_to_pass: u64 = seek_res
            .required_ts
            .get()
            .saturating_sub(seek_res.actual_ts.get())
            .max(0) as u64;
        let packet = loop {
            let candidate = match self.format.next_packet().map_err(|e| format!("refine seek: {e}"))? {
                Some(p) => p,
                // End of stream while refining: drop what was decoded before the seek.
                None => {
                    self.buffer.clear();
                    self.current_frame_offset = 0;
                    return Ok(());
                }
            };
            if candidate.dur.get() > samples_to_pass {
                break candidate;
            }
            samples_to_pass -= candidate.dur.get();
        };

        let packet_trim_start = packet.trim_start.get();
        let mut retried = false;
        let mut decoded = self.decoder.decode(&packet);
        for _ in 0..DECODE_MAX_RETRIES {
            if decoded.is_err() {
                let Some(p) = self.format.next_packet().map_err(|e| format!("refine retry: {e}"))? else {
                    break;
                };
                retried = true;
                decoded = self.decoder.decode(&p);
            }
        }

        // The packet's duration counts frames before trimming; the decoder removed `trim_start` of them
        // from the front (only when it trims: the Ogg demuxer reports Opus pre-skip regardless).
        let offset_frames = if retried {
            0
        } else if self.builtin_gapless {
            samples_to_pass.saturating_sub(packet_trim_start)
        } else {
            samples_to_pass
        };

        let decoded = decoded.map_err(|e| format!("refine decode: {e}"))?;
        self.spec = decoded.spec().clone();
        self.buffer = Self::make_buffer(&decoded);
        self.current_frame_offset = offset_frames as usize * self.spec.channels().count();
        Ok(())
    }
}

impl Iterator for Decoder {
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        if self.current_frame_offset >= self.buffer.len() {
            // Until a decodable packet or the end: corrupt frames are dropped, not fatal.
            loop {
                let packet = self.format.next_packet().ok()??;
                match self.decoder.decode(&packet) {
                    Ok(decoded) => {
                        self.consecutive_decode_errors = 0;
                        self.spec = decoded.spec().clone();
                        self.buffer = Self::make_buffer(&decoded);
                        self.current_frame_offset = 0;
                        // A packet emptied by gapless trimming is not the end of the stream.
                        if self.buffer.is_empty() {
                            continue;
                        }
                        break;
                    }
                    Err(symphonia::core::errors::Error::DecodeError(msg)) => {
                        self.consecutive_decode_errors += 1;
                        if self.consecutive_decode_errors == 1 || self.consecutive_decode_errors.is_multiple_of(10) {
                            debug!("dropped corrupt frame #{}: {msg}", self.consecutive_decode_errors);
                        }
                        if self.consecutive_decode_errors >= MAX_CONSECUTIVE_DECODE_ERRORS {
                            warn!("{MAX_CONSECUTIVE_DECODE_ERRORS} undecodable frames in a row, stopping the track");
                            return None;
                        }
                    }
                    Err(_) => return None, // read error or fatal codec error: end of the track
                }
            }
        }
        let sample = *self.buffer.get(self.current_frame_offset)?;
        self.current_frame_offset += 1;
        Some(sample)
    }
}

impl Source for Decoder {
    #[inline]
    fn current_span_len(&self) -> Option<usize> {
        // The span's size, never 0 (rodio reads 0 as "finished") and never None (rodio reads that as
        // an infinite span and would pin the rate/channel converters): an empty buffer says one frame.
        if self.buffer.is_empty() {
            return Some(self.channels().get() as usize);
        }
        Some(self.buffer.len())
    }

    #[inline]
    fn channels(&self) -> rodio::ChannelCount {
        NonZeroU16::new(self.spec.channels().count() as u16).unwrap_or(NonZeroU16::MIN)
    }

    #[inline]
    fn sample_rate(&self) -> rodio::SampleRate {
        NonZeroU32::new(self.spec.rate()).unwrap_or(NonZeroU32::MIN)
    }

    #[inline]
    fn total_duration(&self) -> Option<Duration> {
        self.total_duration
            .map(|t| Duration::from_secs_f64(t.as_secs_f64().max(0.0)))
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), rodio::source::SeekError> {
        let seek_beyond_end = self
            .total_duration()
            .is_some_and(|dur| dur.saturating_sub(pos).as_millis() < 1);
        let target_secs = if seek_beyond_end {
            // Step back a little: some demuxers can't seek to the exact end.
            let total = self
                .total_duration
                .map(|t| t.as_secs_f64())
                .unwrap_or_else(|| pos.as_secs_f64());
            (total - 0.0001).max(0.0)
        } else {
            pos.as_secs_f64()
        };
        let time = Time::try_from_secs_f64(target_secs).unwrap_or(Time::ZERO);
        let to_skip = self.current_frame_offset % self.channels().get() as usize;

        // Symphonia's Ogg demuxer can panic while seeking some streams. This runs on the audio output
        // thread, where an escaping panic would take the whole player down: contain it, and let the
        // seek fail instead.
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let seek_res = self
                .format
                .seek(SeekMode::Accurate, SeekTo::Time { time, track_id: None })
                .map_err(|e| e.to_string())?;
            // A decoder must be reset when the next packet is discontinuous (after a seek).
            self.decoder.reset();
            self.refine_position(seek_res)?;
            Ok::<(), String>(())
        }));
        match outcome {
            Ok(Ok(())) => {
                self.current_frame_offset += to_skip;
                Ok(())
            }
            Ok(Err(e)) => Err(rodio::source::SeekError::Other(Arc::new(std::io::Error::other(e)))),
            Err(_) => Err(rodio::source::SeekError::Other(Arc::new(std::io::Error::other(
                "the demuxer panicked while seeking (contained)",
            )))),
        }
    }
}
