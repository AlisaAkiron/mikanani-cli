//! qBittorrent: the WebUI client, saved connection profiles, and where
//! their passwords are kept.

pub mod client;
pub mod password;
pub mod profile;
pub mod secret;

pub use client::QbtClient;
