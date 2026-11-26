#![forbid(unsafe_code)]
//! Paseo Relay — a single-node, wire-compatible WebSocket relay for Paseo.
//!
//! The relay is deliberately untrusted from Paseo's perspective: payloads are
//! end-to-end encrypted and forwarded opaquely. This crate only routes frames.
//!
//! # API stability
//!
//! The supported library surface is [`Config`], [`serve`] and
//! [`ServerHandle`] (plus the [`config`] module they come from). The other
//! modules are public so the fuzz targets and benchmarks can reach the codec
//! and the validators, but they are implementation details: they are hidden
//! from the documentation and may change in any release.

pub mod config;

#[doc(hidden)]
pub mod capacity;
#[doc(hidden)]
pub mod connection;
#[doc(hidden)]
pub mod handshake;
#[doc(hidden)]
pub mod metrics;
#[doc(hidden)]
pub mod protocol;
#[doc(hidden)]
pub mod server;
#[doc(hidden)]
pub mod session;
#[doc(hidden)]
pub mod writer;
#[doc(hidden)]
pub mod ws;

pub use config::Config;
pub use server::{serve, ServerHandle};

/// Crate version embedded at build time.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Short git revision (or the `PASEO_RELAY_BUILD_REVISION` build-time override).
pub const BUILD_REVISION: &str = env!("PASEO_RELAY_BUILD_REVISION");
