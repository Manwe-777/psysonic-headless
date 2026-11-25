//! The Psysonic Hub client: this box as a player on the hub.

mod client;
mod library;
mod protocol;
mod queue_edits;
mod server_keys;

pub use client::{HubConfig, run};
pub use server_keys::{address_hash, server_key};
