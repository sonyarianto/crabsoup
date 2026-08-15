//! `video.video(path)` / `video.playlist(...)` / `video.single(path)` source
//! wiring (Parts H1 + H7).
//!
//! The video side of a media file: a dedicated decode thread pulls frames
//! from the `VideoDecoder`, paces them to their PTS, and publishes to the
//! shared `VideoTap`; the audio side of the same file plays through the
//! normal audio graph (`single`/playlist, decoded by symphonia). Outputs
//! that need video subscribe to the tap and interleave by PTS at mux time.
//! A playlist plays one file at a time on a single decode thread, carrying
//! an accumulated PTS offset across tracks so the published timeline stays
//! continuous (no jumps back when the next file starts at PTS 0).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use super::ffi::VideoDecoder;
use super::frame::VideoFrame;
use super::tap::VideoTap;
use crate::Result;

/// Everything the engine needs to play one video file's video track.
pub struct VideoConfig {
    pub path: PathBuf,
    pub spec: VideoSpec,
}

/// A sequence of video files played one at a time by a single decode
/// thread (Part H7). Tracks should share one resolution: outputs open
/// their encoders at the first track's spec.
pub struct VideoPlaylistConfig {
    pub tracks: Vec<VideoConfig>,
    pub shuffle: bool,
    pub loop_playlist: bool,
    /// Seeded RNG for deterministic shuffle (used by tests).
    pub seed: Option<u64>,
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
                    let due = start + Duration::from_micros(frame.pts_us);
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

    /// Spawn the decode thread for a playlist: one file at a time, in order
    /// (shuffled per cycle when `shuffle`), looping when `loop_playlist`.
    /// Frames carry a PTS offset accumulated across tracks, so the timeline
    /// published to the tap never jumps back at a track switch.
    pub fn spawn_playlist(
        config: &VideoPlaylistConfig,
        tap: Arc<VideoTap>,
        stop: Arc<AtomicBool>,
    ) -> Result<VideoSourceHandle> {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let tap = tap.clone();
        let engine_stop = stop.clone();
        let paths: Vec<PathBuf> = config.tracks.iter().map(|t| t.path.clone()).collect();
        let shuffle = config.shuffle;
        let loop_playlist = config.loop_playlist;
        let seed = config.seed;
        let thread = std::thread::Builder::new()
            .name("video-playlist".into())
            .spawn(move || {
                let mut order: Vec<usize> = (0..paths.len()).collect();
                if shuffle {
                    shuffle_indices(&mut order, seed);
                }
                let mut rng = seed.map(SmallRng::seed_from_u64);
                let mut index = 0usize;
                let mut offset_us: u64 = 0;
                let start = Instant::now();
                loop {
                    if thread_stop.load(Ordering::SeqCst) || engine_stop.load(Ordering::SeqCst) {
                        break;
                    }
                    if index >= order.len() {
                        if !loop_playlist {
                            break;
                        }
                        index = 0;
                        if let (true, Some(r)) = (shuffle, &mut rng) {
                            shuffle_indices_rng(&mut order, r);
                        }
                    }
                    let path = &paths[order[index]];
                    let mut decoder = match VideoDecoder::open(path) {
                        Ok(d) => d,
                        Err(e) => {
                            log::warn!("video playlist {}: {e}", path.display());
                            index += 1;
                            continue;
                        }
                    };
                    loop {
                        if thread_stop.load(Ordering::SeqCst) || engine_stop.load(Ordering::SeqCst)
                        {
                            break;
                        }
                        let frame = match decoder.read_frame() {
                            Ok(Some(f)) => f,
                            Ok(None) => break,
                            Err(e) => {
                                log::warn!("video playlist {}: {e}", path.display());
                                break;
                            }
                        };
                        let pts = offset_us + frame.pts_us;
                        let due = start + Duration::from_micros(pts);
                        if let Some(remaining) = due.checked_duration_since(Instant::now()) {
                            std::thread::sleep(remaining);
                        }
                        tap.publish(Arc::new(VideoFrame::new(
                            pts,
                            frame.width,
                            frame.height,
                            frame.y,
                            frame.u,
                            frame.v,
                        )));
                    }
                    // The published timeline is wall-clock paced, so the
                    // next track simply continues from now.
                    offset_us = start.elapsed().as_micros() as u64;
                    index += 1;
                }
                log::info!("video playlist: decode thread ended");
            })?;
        Ok(VideoSourceHandle {
            stop: stop_flag,
            thread: Some(thread),
        })
    }
}

/// Fisher-Yates, deterministically seeded when `seed` is set.
fn shuffle_indices(order: &mut [usize], seed: Option<u64>) {
    let mut rng = seed
        .map(SmallRng::seed_from_u64)
        .unwrap_or_else(SmallRng::from_entropy);
    shuffle_indices_rng(order, &mut rng);
}

fn shuffle_indices_rng(order: &mut [usize], rng: &mut SmallRng) {
    for i in (1..order.len()).rev() {
        let j = rng.gen_range(0..=i);
        order.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::testutil::render_test_clip;
    use std::sync::mpsc::Receiver;
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

    fn playlist_cfg(
        paths: Vec<PathBuf>,
        shuffle: bool,
        loop_playlist: bool,
    ) -> VideoPlaylistConfig {
        let tracks = paths
            .into_iter()
            .map(|p| {
                let spec = VideoSource::validate(&p).expect("validate");
                VideoConfig { path: p, spec }
            })
            .collect();
        VideoPlaylistConfig {
            tracks,
            shuffle,
            loop_playlist,
            seed: None,
        }
    }

    /// Collect `rx` for up to `seconds`, returning the pts timeline. Stops
    /// early once two 100 ms polls come back empty — frame pacing is 40 ms,
    /// so 200 ms of silence means the stream is really over (and a
    /// mid-track-switch pause can never trip it).
    fn collect(rx: &Receiver<Arc<VideoFrame>>, seconds: u64) -> Vec<u64> {
        let mut frames = Vec::new();
        let mut idle = 0;
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(f) => {
                    frames.push(f.pts_us);
                    idle = 0;
                }
                Err(_) => {
                    idle += 1;
                    if idle >= 2 && !frames.is_empty() {
                        break;
                    }
                }
            }
        }
        frames
    }

    #[test]
    fn playlist_plays_tracks_in_order_with_continuous_pts() {
        let Some(a) = render_test_clip("pl-a") else {
            return;
        };
        let Some(b) = render_test_clip("pl-b") else {
            return;
        };
        let tap = Arc::new(VideoTap::new());
        let rx = tap.register();
        let cfg = playlist_cfg(vec![a.clone(), b.clone()], false, false);
        let handle = VideoSource::spawn_playlist(&cfg, tap, Arc::new(AtomicBool::new(false)))
            .expect("spawn");
        let frames = collect(&rx, 4);
        // Two 1 s clips at 25 fps, plus a brief track-switch gap.
        assert!(
            (46..=58).contains(&frames.len()),
            "expected ~50 frames over two tracks, got {}",
            frames.len()
        );
        assert!(
            frames.windows(2).all(|w| w[0] < w[1]),
            "pts must be strictly increasing across the track switch"
        );
        // The inter-track gap is the EOF->next-open turnaround, far below
        // one frame time (40 ms at 25 fps would be 40_000 us).
        let max_gap = frames.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        assert!(
            max_gap < 500_000,
            "track switch must stay near-continuous, largest gap {max_gap} us"
        );
        drop(handle);
        std::fs::remove_file(&a).ok();
        std::fs::remove_file(&b).ok();
    }

    #[test]
    fn looping_playlist_restarts_without_pt_jump() {
        let Some(a) = render_test_clip("pl-loop") else {
            return;
        };
        let tap = Arc::new(VideoTap::new());
        let rx = tap.register();
        let cfg = playlist_cfg(vec![a.clone()], false, true);
        let handle = VideoSource::spawn_playlist(&cfg, tap, Arc::new(AtomicBool::new(false)))
            .expect("spawn");
        let frames = collect(&rx, 3);
        // 3 s at 25 fps spans ~2 full loops of the 1 s clip.
        assert!(
            frames.len() >= 44,
            "looping playlist must restart the track, got {} frames",
            frames.len()
        );
        assert!(
            frames.windows(2).all(|w| w[0] < w[1]),
            "pts must keep increasing across loop boundaries"
        );
        drop(handle);
        std::fs::remove_file(&a).ok();
    }

    #[test]
    fn playlist_skips_broken_track() {
        let Some(a) = render_test_clip("pl-skip") else {
            return;
        };
        let tap = Arc::new(VideoTap::new());
        let rx = tap.register();
        let missing = std::env::temp_dir().join("crabsoup-does-not-exist.mp4");
        // Skip validation here: the whole point is a track that fails to
        // open in the decode thread.
        let spec = VideoSource::validate(&a).expect("validate");
        let cfg = VideoPlaylistConfig {
            tracks: vec![
                VideoConfig {
                    path: a.clone(),
                    spec,
                },
                VideoConfig {
                    path: missing,
                    spec,
                },
            ],
            shuffle: false,
            loop_playlist: false,
            seed: None,
        };
        let handle = VideoSource::spawn_playlist(&cfg, tap, Arc::new(AtomicBool::new(false)))
            .expect("spawn");
        let frames = collect(&rx, 4);
        assert!(
            (24..=27).contains(&frames.len()),
            "broken track must be skipped, got {} frames",
            frames.len()
        );
        drop(handle);
        std::fs::remove_file(&a).ok();
    }
}
