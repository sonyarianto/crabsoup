//! Crabsoup — a Liquidsoap-inspired audio streaming engine.
//!
//! Reads a YAML configuration, schedules a gapless playlist, mixes it with
//! live DJ input and jingles using crossfades, and pushes the encoded result
//! (MP3 or Opus) to an Icecast server.

pub mod config;
pub mod control;
pub mod engine;
pub mod live;
pub mod output;
pub mod resample;
pub mod source;

/// Shorthand for the crate-wide error type.
pub type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;
