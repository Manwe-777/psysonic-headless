//! A file that is still downloading, readable (and seekable) while it arrives: reads wait only for
//! bytes that are not there yet, so playback starts after the first chunk instead of the whole file.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use symphonia::core::io::MediaSource;

/// How long a read waits for the download to move before giving up (a stalled server).
const STALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct State {
    data: Vec<u8>,
    done: bool,
    error: Option<String>,
}

/// The download side: the task that fetches the file appends to it.
#[derive(Clone)]
pub struct Download {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    arrived: Condvar,
    /// The total size, when the server said (Content-Length).
    len: Option<u64>,
    /// Set when the reader is dropped: the download can stop.
    abandoned: AtomicBool,
}

impl Download {
    pub fn append(&self, chunk: &[u8]) {
        self.shared.state.lock().unwrap().data.extend_from_slice(chunk);
        self.shared.arrived.notify_all();
    }

    pub fn finish(&self) {
        self.shared.state.lock().unwrap().done = true;
        self.shared.arrived.notify_all();
    }

    pub fn fail(&self, error: String) {
        let mut state = self.shared.state.lock().unwrap();
        state.error = Some(error);
        state.done = true;
        drop(state);
        self.shared.arrived.notify_all();
    }

    /// Nobody reads it any more (the track was skipped): stop downloading.
    pub fn abandoned(&self) -> bool {
        self.shared.abandoned.load(Ordering::Relaxed)
    }
}

/// The reading side, handed to the decoder.
pub struct StreamSource {
    shared: Arc<Shared>,
    pos: u64,
}

/// A new download of `len` bytes (if known): the side that fills it and the side that reads it.
pub fn stream(len: Option<u64>) -> (Download, StreamSource) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        arrived: Condvar::new(),
        len,
        abandoned: AtomicBool::new(false),
    });
    (Download { shared: shared.clone() }, StreamSource { shared, pos: 0 })
}

impl StreamSource {
    /// Waits until byte `upto` has arrived or the download ended; returns the bytes available.
    fn wait_for(&self, upto: u64) -> io::Result<u64> {
        let mut state = self.shared.state.lock().unwrap();
        loop {
            let have = state.data.len() as u64;
            if have >= upto || state.done {
                if let Some(e) = &state.error
                    && have < upto
                {
                    return Err(io::Error::other(e.clone()));
                }
                return Ok(have);
            }
            let (next, timeout) = self.shared.arrived.wait_timeout(state, STALL_TIMEOUT).unwrap();
            state = next;
            if timeout.timed_out() && (state.data.len() as u64) == have && !state.done {
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
        let have = self.wait_for(self.pos + 1)?;
        if self.pos >= have {
            return Ok(0); // end of the file
        }
        let state = self.shared.state.lock().unwrap();
        let start = self.pos as usize;
        let n = buf.len().min(state.data.len() - start);
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
                    None => self.wait_for(u64::MAX)?, // unknown size: the end is known once it all arrived
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

    #[test]
    fn reads_what_arrived_and_waits_for_the_rest() {
        let (download, mut source) = stream(Some(6));
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
    fn seeking_ahead_waits_for_those_bytes() {
        let (download, mut source) = stream(Some(4));
        download.append(b"wxyz");
        download.finish();
        source.seek(SeekFrom::End(-1)).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(source.read(&mut buf).unwrap(), 1);
        assert_eq!(buf[0], b'z');
    }

    #[test]
    fn a_failed_download_is_an_error_not_a_short_file() {
        let (download, mut source) = stream(None);
        download.append(b"ab");
        download.fail("connection reset".into());
        let mut buf = [0u8; 2];
        assert_eq!(source.read(&mut buf).unwrap(), 2, "what arrived is still read");
        assert!(source.read(&mut buf).is_err());
    }

    #[test]
    fn dropping_the_reader_tells_the_download_to_stop() {
        let (download, source) = stream(None);
        assert!(!download.abandoned());
        drop(source);
        assert!(download.abandoned());
    }
}
