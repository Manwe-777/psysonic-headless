//! Decoding, as in Psysonic: Symphonia with every format it supports plus Opus through libopus,
//! read from a stream that is still downloading.

mod codec;
mod decoder;
mod probe_gate;
mod stream;

pub use decoder::Decoder;
pub use stream::{Download, RangeFetcher, StreamSource, stream};
