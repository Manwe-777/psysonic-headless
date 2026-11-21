//! A file that is still downloading, readable and seekable while it arrives (after Psysonic's
//! `RangedHttpSource`). The file downloads from the start; a read far ahead of that download (a
//! seek, a resume at a position, the Ogg demuxer looking at the end of the file) is fetched on its
//! own with a ranged request instead of waiting for the download to get there.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use symphonia::core::io::MediaSource;

/// How long a read waits without any progress before giving up (a stalled server).
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A read this far ahead of the download is a seek: fetch it rather than wait.
const SEEK_GAP: u64 = 512 * 1024;
/// The least a ranged fetch asks for (a seek often reads only a little).
const SEEK_WINDOW: u64 = 128 * 1024;
/// Read-ahead while playing from a fetched range...
const PREFETCH_WINDOW: u64 = 1024 * 1024;
/// ...started when this little of the fetched range is left.
const PREFETCH_REMAINING: u64 = 768 * 1024;

/// Fetches a byte range of the file in the background and writes it with [`Download::write_at`],
/// then calls [`Download::range_finished`] (also on failure).
pub trait RangeFetcher: Send + Sync {
    fn fetch(&self, download: Download, start: u64, end: u64);
}

#[derive(Default)]
struct State {
    data: Vec<u8>,
    /// Bytes downloaded in order from the start.
    linear: u64,
    /// Ranges fetched on their own, sorted and merged.
    filled: Vec<(u64, u64)>,
    /// Ranges being fetched.
    inflight: Vec<(u64, u64)>,
    /// The download from the start ended (fully, or with `error`).
    done: bool,
    error: Option<String>,
    /// Bumped on every arrival, so a waiting read knows something moved.
    progress: u64,
}

impl State {
    /// Where the bytes available from `pos` on stop.
    fn available_to(&self, pos: u64) -> u64 {
        let mut end = if pos < self.linear { self.linear } else { pos };
        loop {
            let next = self
                .filled
                .iter()
                .filter(|(s, e)| *s <= end && end < *e)
                .map(|(_, e)| *e)
                .max();
            match next {
                Some(e) if e > end => end = e,
                _ => return end,
            }
        }
    }

    fn requested(&self, pos: u64) -> bool {
        self.inflight.iter().any(|(s, e)| *s <= pos && pos < *e)
    }

    fn record_filled(&mut self, start: u64, end: u64) {
        self.filled.push((start, end));
        self.filled.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(self.filled.len());
        for (s, e) in self.filled.drain(..) {
            match merged.last_mut() {
                Some((_, last)) if s <= *last => *last = (*last).max(e),
                _ => merged.push((s, e)),
            }
        }
        self.filled = merged;
    }
}

struct Shared {
    state: Mutex<State>,
    arrived: Condvar,
    /// The total size, when the server said (Content-Length).
    len: Option<u64>,
    /// Set when the reader is dropped: downloads can stop.
    abandoned: AtomicBool,
}

/// The writing side: the download from the start and the ranged fetches write into it.
#[derive(Clone)]
pub struct Download {
    shared: Arc<Shared>,
}

impl Download {
    /// The next bytes of the download from the start.
    pub fn append(&self, chunk: &[u8]) {
        let mut state = self.shared.state.lock().unwrap();
        let at = state.linear as usize;
        write(&mut state.data, at, chunk);
        state.linear += chunk.len() as u64;
        state.progress += 1;
        drop(state);
        self.shared.arrived.notify_all();
    }

    pub fn finish(&self) {
        self.end(None);
    }

    pub fn fail(&self, error: String) {
        self.end(Some(error));
    }

    fn end(&self, error: Option<String>) {
        let mut state = self.shared.state.lock().unwrap();
        state.done = true;
        state.error = error;
        state.progress += 1;
        drop(state);
        self.shared.arrived.notify_all();
    }

    /// Bytes of a ranged fetch.
    pub fn write_at(&self, offset: u64, chunk: &[u8]) {
        let mut state = self.shared.state.lock().unwrap();
        write(&mut state.data, offset as usize, chunk);
        state.record_filled(offset, offset + chunk.len() as u64);
        state.progress += 1;
        drop(state);
        self.shared.arrived.notify_all();
    }

    /// A ranged fetch ended (whatever arrived of it is kept; the rest can be asked for again).
    pub fn range_finished(&self, start: u64, end: u64) {
        let mut state = self.shared.state.lock().unwrap();
        state.inflight.retain(|r| *r != (start, end));
        state.progress += 1;
        drop(state);
        self.shared.arrived.notify_all();
    }

    /// Nobody reads it any more (the track was skipped): stop downloading.
    pub fn abandoned(&self) -> bool {
        self.shared.abandoned.load(Ordering::Relaxed)
    }
}

fn write(data: &mut Vec<u8>, at: usize, chunk: &[u8]) {
    let end = at + chunk.len();
    if data.len() < end {
        data.resize(end, 0);
    }
    data[at..end].copy_from_slice(chunk);
}

/// The reading side, handed to the decoder.
pub struct StreamSource {
    shared: Arc<Shared>,
    pos: u64,
    ranges: Option<Arc<dyn RangeFetcher>>,
}

/// A new download of `len` bytes (if known): the side that fills it and the side that reads it.
/// With a `RangeFetcher` (the server takes ranged requests and said the size), reads far ahead of
/// the download are fetched on their own.
pub fn stream(len: Option<u64>, ranges: Option<Arc<dyn RangeFetcher>>) -> (Download, StreamSource) {
    let mut state = State::default();
    if let Some(len) = len {
        state.data = vec![0; len as usize];
    }
    let shared = Arc::new(Shared {
        state: Mutex::new(state),
        arrived: Condvar::new(),
        len,
        abandoned: AtomicBool::new(false),
    });
    let ranges = ranges.filter(|_| len.is_some());
    (
        Download { shared: shared.clone() },
        StreamSource { shared, pos: 0, ranges },
    )
}

impl StreamSource {
    /// Reading anywhere is cheap: far reads are fetched with ranged requests.
    pub fn random_access(&self) -> bool {
        self.ranges.is_some()
    }

    /// Asks for `[start, end)` unless it is there or being fetched (and stays clear of what is).
    fn request(&self, state: &mut State, start: u64, end: u64) {
        let (Some(fetcher), Some(len)) = (&self.ranges, self.shared.len) else {
            return;
        };
        let start = state.available_to(start);
        let mut end = end.max(start + SEEK_WINDOW).min(len);
        if let Some(next) = state
            .inflight
            .iter()
            .chain(&state.filled)
            .map(|(s, _)| *s)
            .filter(|s| *s > start)
            .min()
        {
            end = end.min(next);
        }
        if start >= end || state.requested(start) {
            return;
        }
        state.inflight.push((start, end));
        fetcher.fetch(
            Download {
                shared: self.shared.clone(),
            },
            start,
            end,
        );
    }

    /// Waits until at least one byte at the read position is there; returns where the available
    /// bytes end (`pos` itself at the end of the file).
    fn wait_for_data(&self) -> io::Result<u64> {
        let pos = self.pos;
        let mut state = self.shared.state.lock().unwrap();
        loop {
            if self.shared.len.is_some_and(|len| pos >= len) {
                return Ok(pos);
            }
            let available = state.available_to(pos);
            if available > pos {
                // Playing from a fetched range: fetch the next window before running out.
                if self.ranges.is_some() && available > state.linear + SEEK_GAP && available - pos < PREFETCH_REMAINING
                {
                    self.request(&mut state, available, available + PREFETCH_WINDOW);
                }
                return Ok(available);
            }
            if state.done && (pos >= state.linear || state.error.is_some()) && !state.requested(pos) {
                // The download ended: past what arrived is the end of the file (or its failure).
                let len_known_and_missing = self.shared.len.is_some_and(|len| pos < len);
                if let Some(e) = &state.error {
                    return Err(io::Error::other(e.clone()));
                }
                if !len_known_and_missing || self.ranges.is_none() {
                    return Ok(pos);
                }
            }
            if self.ranges.is_some() && (pos >= state.linear + SEEK_GAP || state.done) {
                self.request(&mut state, pos, pos + SEEK_WINDOW);
            }
            let seen = state.progress;
            let (next, timeout) = self.shared.arrived.wait_timeout(state, STALL_TIMEOUT).unwrap();
            state = next;
            if timeout.timed_out() && state.progress == seen {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the download stalled"));
            }
        }
    }
}

impl Read for StreamSource {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let available = self.wait_for_data()?;
        if available <= self.pos {
            return Ok(0); // end of the file
        }
        let state = self.shared.state.lock().unwrap();
        let start = self.pos as usize;
        let n = buf.len().min((available - self.pos) as usize);
        buf[..n].copy_from_slice(&state.data[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for StreamSource {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let target = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
            SeekFrom::End(d) => {
                let len = match self.shared.len {
                    Some(len) => len,
                    None => self.wait_until_done()?, // unknown size: the end is known once it all arrived
                };
                len as i128 + d as i128
            }
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before the start"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl StreamSource {
    fn wait_until_done(&self) -> io::Result<u64> {
        let mut state = self.shared.state.lock().unwrap();
        while !state.done {
            let seen = state.progress;
            let (next, timeout) = self.shared.arrived.wait_timeout(state, STALL_TIMEOUT).unwrap();
            state = next;
            if timeout.timed_out() && state.progress == seen {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "the download stalled"));
            }
        }
        match &state.error {
            Some(e) => Err(io::Error::other(e.clone())),
            None => Ok(state.linear),
        }
    }
}

impl MediaSource for StreamSource {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.shared.len
    }
}

impl Drop for StreamSource {
    fn drop(&mut self) {
        self.shared.abandoned.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves ranges from a file in memory, right away, and counts the requests.
    struct Ranges {
        file: Vec<u8>,
        asked: Mutex<Vec<(u64, u64)>>,
    }

    impl RangeFetcher for Ranges {
        fn fetch(&self, download: Download, start: u64, end: u64) {
            self.asked.lock().unwrap().push((start, end));
            let file = self.file.clone();
            std::thread::spawn(move || {
                download.write_at(start, &file[start as usize..end as usize]);
                download.range_finished(start, end);
            });
        }
    }

    fn file(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn reads_what_arrived_and_waits_for_the_rest() {
        let (download, mut source) = stream(Some(6), None);
        download.append(b"abc");
        let mut buf = [0u8; 8];
        assert_eq!(source.read(&mut buf).unwrap(), 3);
        let filler = download.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            filler.append(b"def");
            filler.finish();
        });
        assert_eq!(source.read(&mut buf).unwrap(), 3, "waited for the next bytes");
        assert_eq!(&buf[..3], b"def");
        assert_eq!(source.read(&mut buf).unwrap(), 0, "then the end of the file");
        t.join().unwrap();
    }

    #[test]
    fn a_read_far_ahead_is_fetched_on_its_own_instead_of_waiting_for_the_download() {
        let bytes = file(4 * 1024 * 1024);
        let ranges = Arc::new(Ranges {
            file: bytes.clone(),
            asked: Mutex::new(Vec::new()),
        });
        let (download, mut source) = stream(Some(bytes.len() as u64), Some(ranges.clone()));
        download.append(&bytes[..1000]); // the download from the start barely began, and stays there
        let mut buf = [0u8; 4096];
        source.seek(SeekFrom::End(-4096)).unwrap();
        let n = source.read(&mut buf).unwrap();
        assert_eq!(
            &buf[..n],
            &bytes[bytes.len() - 4096..][..n],
            "the end of the file, without the middle"
        );
        source.seek(SeekFrom::Start(3 * 1024 * 1024)).unwrap();
        let n = source.read(&mut buf).unwrap();
        assert_eq!(&buf[..n], &bytes[3 * 1024 * 1024..][..n]);
        let asked = ranges.asked.lock().unwrap();
        assert!(
            asked.iter().all(|(start, _)| *start >= 3 * 1024 * 1024),
            "nothing between the start and the read was fetched: {asked:?}"
        );
    }

    #[test]
    fn playing_on_from_a_fetched_range_fetches_ahead_and_never_runs_dry() {
        let bytes = file(6 * 1024 * 1024);
        let ranges = Arc::new(Ranges {
            file: bytes.clone(),
            asked: Mutex::new(Vec::new()),
        });
        let (download, mut source) = stream(Some(bytes.len() as u64), Some(ranges.clone()));
        download.append(&bytes[..1000]);
        let from = 2 * 1024 * 1024;
        source.seek(SeekFrom::Start(from as u64)).unwrap();
        let mut got = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        while got.len() < 3 * 1024 * 1024 {
            let n = source.read(&mut buf).unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(&got[..], &bytes[from..from + got.len()]);
    }

    #[test]
    fn nearby_reads_wait_for_the_download_instead_of_fetching() {
        let bytes = file(2 * 1024 * 1024);
        let ranges = Arc::new(Ranges {
            file: bytes.clone(),
            asked: Mutex::new(Vec::new()),
        });
        let (download, mut source) = stream(Some(bytes.len() as u64), Some(ranges.clone()));
        download.append(&bytes[..1000]);
        source.seek(SeekFrom::Start(2000)).unwrap();
        let filler = download.clone();
        let rest = bytes.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            filler.append(&rest[1000..]);
            filler.finish();
        });
        let mut buf = [0u8; 10];
        source.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &bytes[2000..2010]);
        assert!(ranges.asked.lock().unwrap().is_empty());
        t.join().unwrap();
    }

    #[test]
    fn a_failed_download_is_an_error_not_a_short_file() {
        let (download, mut source) = stream(None, None);
        download.append(b"ab");
        download.fail("connection reset".into());
        let mut buf = [0u8; 2];
        assert_eq!(source.read(&mut buf).unwrap(), 2, "what arrived is still read");
        assert!(source.read(&mut buf).is_err());
    }

    #[test]
    fn dropping_the_reader_tells_the_download_to_stop() {
        let (download, source) = stream(None, None);
        assert!(!download.abandoned());
        drop(source);
        assert!(download.abandoned());
    }
}
