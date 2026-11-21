//! The Navidrome / Subsonic server client.

mod client;
mod models;

pub use client::{Quality, Subsonic};
#[cfg(test)]
pub use models::ReplayGain;
pub use models::Song;
