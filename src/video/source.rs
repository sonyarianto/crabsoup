//! `video.video(path)` source wiring (Part H1).
//!
//! The video side of a media file: a dedicated decode thread pulls frames
//! from the `VideoDecoder`, paces them to their PTS, and publishes to the
//! shared `VideoTap`; the audio side of the same file plays through the
//! normal audio graph (`single`/playlist, decoded by symphonia). Outputs
//! that need video subscribe to the tap and interleave by PTS at mux time.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use super::ffi::VideoDecoder;
use super::tap::VideoTap;
use crate::Result;

/// Everything the engine needs to play one video file's video track.
pub struct VideoConfig {
    pub path: PathBuf,
    pub spec: VideoSpec,
}

/// Static properties of a video stream, read once at script evaluation.
#[derive(Clone, Copy, Debug)]
pub struct VideoSpec {
    pub width: u32,
    pub height: u32,
    /// Frames per second (rational, as a float).
    pub frame_rate: f64,
}

/// Stops the decode thread when dropped or explicitly.
pub struct VideoSourceHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for VideoSourceHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub struct VideoSource;

impl VideoSource {
    /// Open `path`, read its video stream's spec, and close without
    /// decoding — fail fast at script evaluation.
    pub fn validate(path: &Path) -> Result<VideoSpec> {
        let decoder = VideoDecoder::open(path)?;
        let fr = decoder.frame_rate();
        let spec = VideoSpec {
            width: decoder.width(),
            height: decoder.height(),
            frame_rate: if fr.0 > 0 && fr.1 > 0 {
                fr.0 as f64 / fr.1 as f64
            } else {
                25.0
            },
        };
        Ok(spec)
    }

    /// Spawn the decode thread for `config`, publishing PTS-paced frames to
    /// `tap`. Ends at end of file or when `stop` is set.
    pub fn spawn(
        config: &VideoConfig,
        tap: Arc<VideoTap>,
        stop: Arc<AtomicBool>,
    ) -> Result<VideoSourceHandle> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let path = config.path.clone();
        let tap = tap.clone();
        let engine_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("video-decode".into())
            .spawn(move || {
                let mut decoder = match VideoDecoder::open(&path) {
                    Ok(d) => d,
                    Err(e) => {
                        log::error!("video source {path:?}: {e}");
                        return;
                    }
                };
                let start = Instant::now();
                loop {
                    if thread_stop.load(Ordering::SeqCst) || engine_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    let frame = match decoder.read_frame() {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(e) => {
                            log::error!("video source {path:?}: {e}");
                            break;
                        }
                    };
                    let due = start + std::time::Duration::from_micros(frame.pts_us);
                    if let Some(remaining) = due.checked_duration_since(Instant::now()) {
                        std::thread::sleep(remaining);
                    }
                    tap.publish(Arc::new(frame));
                }
                log::info!("video source {path:?}: decode thread ended");
            })?;
        Ok(VideoSourceHandle {
            stop: stop_flag,
            thread: Some(thread),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::testutil::render_test_clip;
    use std::time::Duration;

    #[test]
    fn validate_reads_stream_spec_without_decoding() {
        let Some(path) = render_test_clip("validate") else {
            return;
        };
        let spec = VideoSource::validate(&path).expect("validate");
        assert_eq!((spec.width, spec.height), (320, 240));
        assert!(
            (24.0..=26.0).contains(&spec.frame_rate),
            "fps {}",
            spec.frame_rate
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn spawned_source_publishes_pts_paced_frames() {
        let Some(path) = render_test_clip("spawn") else {
            return;
        };
        let tap = Arc::new(VideoTap::new());
        let rx = tap.register();
        let spec = VideoSource::validate(&path).expect("validate");
        let cfg = VideoConfig { path, spec };
        let handle =
            VideoSource::spawn(&cfg, tap, Arc::new(AtomicBool::new(false))).expect("spawn");
        let mut frames: Vec<u64> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(f) => frames.push(f.pts_us),
                Err(_) => break,
            }
        }
        assert!(
            (24..=27).contains(&frames.len()),
            "expected ~25 frames over 1 s, got {}",
            frames.len()
        );
        assert!(
            frames.windows(2).all(|w| w[0] < w[1]),
            "pts must be strictly increasing"
        );
        drop(handle); // thread already ended at EOF; drop joins cleanly
        std::fs::remove_file(&cfg.path).ok();
    }
}
