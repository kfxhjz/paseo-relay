#![forbid(unsafe_code)]
//! Paseo Relay — a single-node, wire-compatible WebSocket relay for Paseo.
//!
//! The relay is deliberately untrusted from Paseo's perspective: payloads are
//! end-to-end encrypted and forwarded opaquely. This crate only routes frames.

pub mod capacity;
pub mod config;
pub mod connection;
pub mod handshake;
pub mod metrics;
pub mod protocol;
pub mod server;
pub mod session;
pub mod writer;
pub mod ws;

pub use config::Config;
pub use server::{serve, ServerHandle};

/// Crate version embedded at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Short git revision (or the `PASEO_RELAY_BUILD_REVISION` build-time override).
pub const BUILD_REVISION: &str = env!("PASEO_RELAY_BUILD_REVISION");
