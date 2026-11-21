//! Hides seekability while the format is probed (from Psysonic's `decode/source_probe.rs`).
//!
//! Symphonia 0.6 scans for trailing metadata whenever the source is seekable with a known length,
//! and that scan seeks to the end: on a stream still downloading it would wait for the whole file
//! before the first sample plays. Formats demuxed from the start re-check `is_seekable()` when
//! seeking, so the source pretends to be non-seekable for the probe and becomes seekable again
//! afterwards. MP4 is left out: its demuxer needs seeking to find the `moov` box.

use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use symphonia::core::io::MediaSource;

pub struct ProbeSeekGate {
    pub inner: Box<dyn MediaSource>,
    pub seekable: Arc<AtomicBool>,
}

impl Read for ProbeSeekGate {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for ProbeSeekGate {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for ProbeSeekGate {
    fn is_seekable(&self) -> bool {
        self.seekable.load(Ordering::Relaxed) && self.inner.is_seekable()
    }

    fn byte_len(&self) -> Option<u64> {
        self.inner.byte_len()
    }
}
