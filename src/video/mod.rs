//! Video pipeline (Part H) — behind the `video` cargo feature.
//!
//! The carrier decision (H1): video does NOT ride in `AudioFrame`. Frames
//! are YUV420P pictures with a PTS, published on their own fan-out tap
//! (`VideoTap`) by a dedicated decode thread; outputs that need video
//! subscribe to both taps and interleave by PTS at mux time. The audio pull
//! chain is untouched.

pub mod effect;
pub mod encode;
pub mod ffi;
pub mod frame;
pub mod source;
pub mod tap;
#[cfg(test)]
pub mod testutil;

pub use effect::{VideoEffects, blend_planes, scale_frame};
pub use encode::{EncodedAu, VideoEncoder};
pub use ffi::VideoDecoder;
pub use frame::VideoFrame;
pub use source::{
    SlideshowConfig, SlideshowTrack, VideoConfig, VideoPlaylistConfig, VideoSource,
    VideoSourceHandle, VideoSpec,
};
pub use tap::VideoTap;
