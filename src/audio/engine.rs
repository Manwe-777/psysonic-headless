use std::io::Cursor;
use std::num::NonZero;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rodio::source::EmptyCallback;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{info, warn};

use super::devices::find_device;

/// What the engine tells the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineEvent {
    /// The track appended with this token played to its end.
    Ended { token: u64 },
}

/// A decoded track, ready to play. Decoding happens before the engine is locked.
pub struct LoadedTrack {
    pub token: u64,
    pub sample_rate: u32,
    gain: f32,
    source: Decoder<Cursor<Vec<u8>>>,
}

impl LoadedTrack {
    /// Decodes a whole file held in memory. `hint` is its file extension, if known.
    pub fn decode(token: u64, bytes: Vec<u8>, hint: Option<&str>, gain: f32) -> Result<Self> {
        let len = bytes.len() as u64;
        let mut builder = Decoder::builder()
            .with_data(Cursor::new(bytes))
            .with_byte_len(len)
            .with_seekable(true)
            .with_gapless(true);
        if let Some(hint) = hint {
            builder = builder.with_hint(hint);
        }
        let source = builder.build().context("could not decode the track")?;
        Ok(Self {
            token,
            sample_rate: source.sample_rate().get(),
            gain,
            source,
        })
    }
}

struct Output {
    _sink: MixerDeviceSink,
    player: Player,
    sample_rate: u32,
}

struct Inner {
    output: Option<Output>,
    volume: f32,
}

/// Plays decoded tracks back to back on one output device. Tracks queued with [`Engine::append`]
/// follow the current one without a gap; the device is reopened when a track's sample rate differs
/// from the open one, so it plays at its own rate (no resampling) when the device supports it.
pub struct Engine {
    device: Option<String>,
    events: UnboundedSender<EngineEvent>,
    inner: Mutex<Inner>,
}

impl Engine {
    pub fn new(device: Option<String>, volume: f32, events: UnboundedSender<EngineEvent>) -> Self {
        Self {
            device,
            events,
            inner: Mutex::new(Inner { output: None, volume }),
        }
    }

    fn open(&self, sample_rate: u32, volume: f32) -> Result<Output> {
        let device = match &self.device {
            Some(name) => find_device(name)?,
            None => {
                use rodio::cpal::traits::HostTrait;
                rodio::cpal::default_host()
                    .default_output_device()
                    .ok_or_else(|| anyhow!("no output device"))?
            }
        };
        let rate = NonZero::new(sample_rate).ok_or_else(|| anyhow!("a track without a sample rate"))?;
        let mut sink = match DeviceSinkBuilder::from_device(device.clone())?
            .with_sample_rate(rate)
            .open_stream()
        {
            Ok(sink) => sink,
            Err(e) => {
                // The device can't run at this rate: play at its own rate, resampled.
                warn!("the output device does not take {sample_rate} Hz ({e}); resampling");
                DeviceSinkBuilder::from_device(device)?
                    .open_sink_or_fallback()
                    .context("could not open the output device")?
            }
        };
        sink.log_on_drop(false);
        let opened = sink.config().sample_rate().get();
        info!(rate = opened, "output open");
        let player = Player::connect_new(sink.mixer());
        player.set_volume(volume);
        // Keyed by the rate it was opened for, even after a fallback: later tracks at that rate reuse it.
        Ok(Output {
            _sink: sink,
            player,
            sample_rate,
        })
    }

    fn push(&self, output: &Output, track: LoadedTrack) {
        let token = track.token;
        output.player.append(track.source.amplify(track.gain));
        let events = self.events.clone();
        output.player.append(EmptyCallback::new(Box::new(move || {
            let _ = events.send(EngineEvent::Ended { token });
        })));
    }

    /// Stops whatever plays and plays this track now.
    pub fn play_now(&self, track: LoadedTrack) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        let reuse = inner
            .output
            .as_ref()
            .is_some_and(|o| o.sample_rate == track.sample_rate);
        if reuse {
            inner.output.as_ref().unwrap().player.clear();
        } else {
            inner.output = None; // close the old device before opening it at the new rate
            inner.output = Some(self.open(track.sample_rate, inner.volume)?);
        }
        let output = inner.output.as_ref().unwrap();
        self.push(output, track);
        output.player.play();
        Ok(())
    }

    /// Queues a track right after the current one, for a gapless change. Returns the track back when
    /// it can't follow seamlessly (nothing plays, or another sample rate): play it with `play_now`
    /// once the current track ends.
    pub fn append(&self, track: LoadedTrack) -> Option<LoadedTrack> {
        let inner = self.inner.lock().unwrap();
        match inner.output.as_ref() {
            Some(output) if output.sample_rate == track.sample_rate && !output.player.empty() => {
                self.push(output, track);
                None
            }
            _ => Some(track),
        }
    }

    /// Drops everything queued and playing.
    pub fn stop(&self) {
        if let Some(output) = self.inner.lock().unwrap().output.as_ref() {
            output.player.clear();
        }
    }

    pub fn pause(&self) {
        if let Some(output) = self.inner.lock().unwrap().output.as_ref() {
            output.player.pause();
        }
    }

    pub fn resume(&self) {
        if let Some(output) = self.inner.lock().unwrap().output.as_ref() {
            output.player.play();
        }
    }

    pub fn is_paused(&self) -> bool {
        self.inner
            .lock()
            .unwrap()
            .output
            .as_ref()
            .is_none_or(|o| o.player.is_paused())
    }

    /// Seeks within the current track.
    pub fn seek(&self, position: Duration) -> Result<()> {
        if let Some(output) = self.inner.lock().unwrap().output.as_ref() {
            output
                .player
                .try_seek(position)
                .map_err(|e| anyhow!("could not seek: {e}"))?;
        }
        Ok(())
    }

    /// The position in the current track.
    pub fn position(&self) -> Duration {
        self.inner
            .lock()
            .unwrap()
            .output
            .as_ref()
            .map(|o| o.player.get_pos())
            .unwrap_or_default()
    }

    /// Volume from 0.0 to 1.0.
    pub fn set_volume(&self, volume: f32) {
        let mut inner = self.inner.lock().unwrap();
        inner.volume = volume.clamp(0.0, 1.0);
        if let Some(output) = inner.output.as_ref() {
            output.player.set_volume(inner.volume);
        }
    }

    pub fn volume(&self) -> f32 {
        self.inner.lock().unwrap().volume
    }
}
