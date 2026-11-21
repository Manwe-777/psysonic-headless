//! The Psysonic Hub client: this box as a player on the hub.

mod client;
mod library;
mod protocol;
mod server_keys;

pub use client::{HubConfig, run};
pub use server_keys::server_key;
