//! Crabsoup — a Liquidsoap-inspired audio streaming engine.
//!
//! Evaluates a `.lua` script (Lua) that builds a source graph and configures
//! services: gapless playlist, crossfades, live DJ ducking, jingles, and MP3 /
//! Opus broadcasting to an Icecast server.

pub mod config;
pub mod control;
pub mod engine;
pub mod live;
pub mod output;
pub mod request;
pub mod resample;
pub mod script;
pub mod source;

/// Shorthand for the crate-wide error type.
pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;
