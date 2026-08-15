//! `video.video(path)` / `video.playlist(...)` / `video.single(path)` source
//! wiring (Parts H1 + H7) and `video.slideshow(...)` (Part H2).
//!
//! The video side of a media file: a dedicated decode thread pulls frames
//! from the `VideoDecoder`, paces them to their PTS, and publishes to the
//! shared `VideoTap`; the audio side of the same file plays through the
//! normal audio graph (`single`/playlist, decoded by symphonia). Outputs
//! that need video subscribe to the tap and interleave by PTS at mux time.
//! A playlist plays one file at a time on a single decode thread, carrying
//! an accumulated PTS offset across tracks so the published timeline stays
//! continuous (no jumps back when the next file starts at PTS 0). A
//! slideshow is the same idea over still images decoded once at script
//! evaluation, with an optional crossfade between pictures.

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

/// One slideshow picture: an image decoded to YUV420P at script evaluation
/// (Part H2). All tracks share one resolution (enforced at eval) so the
/// crossfade can blend whole planes.
#[derive(Clone)]
pub struct SlideshowTrack {
    pub path: PathBuf,
    pub frame: VideoFrame,
}

/// `video.slideshow(...)` config (Part H2): still images rendered to
/// PTS-paced frames, with an optional crossfade between pictures.
#[derive(Clone)]
pub struct SlideshowConfig {
    pub tracks: Vec<SlideshowTrack>,
    /// The slideshow's own spec: image size plus the chosen `fps` (images
    /// carry no frame rate of their own). Handed to video outputs.
    pub spec: VideoSpec,
    /// Wall-clock seconds each image is shown.
    pub seconds_per_image: f64,
    /// Crossfade duration into each image (0 when `transition = "none"`).
    pub transition_seconds: f64,
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

    /// Decode `path` as a still image: returns the first frame (Part H2).
    /// Used at script evaluation by `video.slideshow`, which keeps the
    /// decoded picture so the render thread can never fail mid-run.
    pub fn decode_image(path: &Path) -> Result<VideoFrame> {
        let mut decoder = VideoDecoder::open(path)?;
        let frames = decoder.decode_all()?;
        frames
            .into_iter()
            .next()
            .ok_or_else(|| format!("no frames decoded from {path:?}").into())
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

    /// Spawn the render thread for a slideshow: each image is published
    /// once per output frame slot for `seconds_per_image`, crossfading into
    /// the previous image over `transition_seconds` (Part H2). PTS is
    /// paced like the playlist — accumulated across tracks, so the
    /// timeline never jumps back at a picture switch.
    pub fn spawn_slideshow(
        config: &SlideshowConfig,
        tap: Arc<VideoTap>,
        stop: Arc<AtomicBool>,
    ) -> Result<VideoSourceHandle> {
        if config.tracks.is_empty() {
            return Err("video.slideshow: no tracks".into());
        }
        let stop_flag = Arc::new(AtomicBool::new(false));
        let thread_stop = stop_flag.clone();
        let tap = tap.clone();
        let engine_stop = stop.clone();
        let tracks = config.tracks.clone();
        let spec = config.spec;
        let seconds_per_image = config.seconds_per_image;
        let transition_seconds = config.transition_seconds;
        let shuffle = config.shuffle;
        let loop_playlist = config.loop_playlist;
        let seed = config.seed;
        let thread = std::thread::Builder::new()
            .name("video-slideshow".into())
            .spawn(move || {
                let fps = spec.frame_rate.max(1.0);
                let frame_us = (1_000_000.0 / fps) as u64;
                let frames_per_image = ((seconds_per_image * fps).round() as u64).max(1);
                let transition_frames =
                    ((transition_seconds * fps).round() as u64).min(frames_per_image);
                let mut order: Vec<usize> = (0..tracks.len()).collect();
                if shuffle {
                    shuffle_indices(&mut order, seed);
                }
                let mut rng = seed.map(SmallRng::seed_from_u64);
                let mut index = 0usize;
                let mut offset_us: u64 = 0;
                // Scratch planes for transition blending (uniform size
                // enforced at script evaluation).
                let (y_len, u_len, v_len) = tracks[0].frame.plane_sizes();
                let mut blend_y = vec![0u8; y_len];
                let mut blend_u = vec![0u8; u_len];
                let mut blend_v = vec![0u8; v_len];
                let mut prev: Option<VideoFrame> = None;
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
                    let show = &tracks[order[index]].frame;
                    for f in 0..frames_per_image {
                        if thread_stop.load(Ordering::SeqCst) || engine_stop.load(Ordering::SeqCst)
                        {
                            break;
                        }
                        let pts = offset_us + f * frame_us;
                        let due = start + Duration::from_micros(pts);
                        if let Some(remaining) = due.checked_duration_since(Instant::now()) {
                            std::thread::sleep(remaining);
                        }
                        // Crossfade window at the start of each picture but
                        // the first: blend prev into show by alpha, where
                        // alpha = 256 means fully `show`.
                        let (y, u, v) = match (&prev, f < transition_frames) {
                            (Some(p), true) => {
                                let alpha = ((f + 1) * 256 / transition_frames) as u32;
                                blend_planes(
                                    &mut blend_y,
                                    &mut blend_u,
                                    &mut blend_v,
                                    p,
                                    show,
                                    alpha,
                                );
                                (blend_y.clone(), blend_u.clone(), blend_v.clone())
                            }
                            _ => (show.y.clone(), show.u.clone(), show.v.clone()),
                        };
                        tap.publish(Arc::new(VideoFrame::new(
                            pts,
                            spec.width,
                            spec.height,
                            y,
                            u,
                            v,
                        )));
                    }
                    prev = Some(show.clone());
                    offset_us = start.elapsed().as_micros() as u64;
                    index += 1;
                }
                log::info!("video.slideshow: render thread ended");
            })?;
        Ok(VideoSourceHandle {
            stop: stop_flag,
            thread: Some(thread),
        })
    }
}

/// Crossfade `prev` into `curr` by `alpha` (0..=256, 256 = fully `curr`),
/// writing whole planes. All three frames share one resolution (enforced
/// at script evaluation), so planes can be blended element-wise.
fn blend_planes(
    dst_y: &mut [u8],
    dst_u: &mut [u8],
    dst_v: &mut [u8],
    prev: &VideoFrame,
    curr: &VideoFrame,
    alpha: u32,
) {
    let a = alpha;
    let b = 256 - alpha;
    let mix = |dst: &mut [u8], from: &[u8], to: &[u8]| {
        for (d, (p, c)) in dst.iter_mut().zip(from.iter().zip(to.iter())) {
            *d = (((*p as u32) * b + (*c as u32) * a) >> 8) as u8;
        }
    };
    mix(dst_y, &prev.y, &curr.y);
    mix(dst_u, &prev.u, &curr.u);
    mix(dst_v, &prev.v, &curr.v);
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
    use crate::video::testutil::{render_test_clip, render_test_solid};
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

    fn slideshow_cfg(
        paths: Vec<PathBuf>,
        seconds_per_image: f64,
        transition_seconds: f64,
        loop_playlist: bool,
    ) -> SlideshowConfig {
        let tracks: Vec<SlideshowTrack> = paths
            .into_iter()
            .map(|p| SlideshowTrack {
                frame: VideoSource::decode_image(&p).expect("decode image"),
                path: p,
            })
            .collect();
        let (w, h) = (tracks[0].frame.width, tracks[0].frame.height);
        SlideshowConfig {
            tracks,
            spec: VideoSpec {
                width: w,
                height: h,
                frame_rate: 25.0,
            },
            seconds_per_image,
            transition_seconds,
            shuffle: false,
            loop_playlist,
            seed: None,
        }
    }

    /// Like [`collect`], but keeps the frames so tests can inspect pixels.
    fn collect_frames(rx: &Receiver<Arc<VideoFrame>>, seconds: u64) -> Vec<Arc<VideoFrame>> {
        let mut frames = Vec::new();
        let mut idle = 0;
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(f) => {
                    frames.push(f);
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
    fn slideshow_shows_each_image_for_its_duration() {
        let Some(black) = render_test_solid("ss-a", 320, 240, "black") else {
            return;
        };
        let Some(white) = render_test_solid("ss-b", 320, 240, "white") else {
            return;
        };
        let tap = Arc::new(VideoTap::new());
        let rx = tap.register();
        // 0.5 s at 25 fps rounds to 13 frames per image, no transition.
        let cfg = slideshow_cfg(vec![black.clone(), white.clone()], 0.5, 0.0, false);
        let handle = VideoSource::spawn_slideshow(&cfg, tap, Arc::new(AtomicBool::new(false)))
            .expect("spawn");
        let frames = collect_frames(&rx, 4);
        assert!(
            (24..=28).contains(&frames.len()),
            "expected ~26 frames over two 0.5 s images, got {}",
            frames.len()
        );
        assert!(
            frames.windows(2).all(|w| w[0].pts_us < w[1].pts_us),
            "pts must be strictly increasing across the image switch"
        );
        let first = frames.first().unwrap().clone();
        let last = frames.last().unwrap().clone();
        assert_eq!(first.y, black_frame(&black).y, "first half shows image A");
        assert_eq!(last.y, black_frame(&white).y, "second half shows image B");
        assert!(first.y.iter().all(|&p| p < 128), "image A is dark");
        assert!(last.y.iter().all(|&p| p >= 128), "image B is bright");
        drop(handle);
        std::fs::remove_file(&black).ok();
        std::fs::remove_file(&white).ok();
    }

    /// Re-decode a solid image so tests can compare exact plane bytes.
    fn black_frame(path: &Path) -> VideoFrame {
        VideoSource::decode_image(path).expect("decode reference")
    }

    #[test]
    fn slideshow_crossfades_between_images() {
        let Some(black) = render_test_solid("ss-xf-a", 320, 240, "black") else {
            return;
        };
        let Some(white) = render_test_solid("ss-xf-b", 320, 240, "white") else {
            return;
        };
        let tap = Arc::new(VideoTap::new());
        let rx = tap.register();
        // 0.4 s crossfade = 10 frames at 25 fps, then the pure picture.
        let cfg = slideshow_cfg(vec![black.clone(), white.clone()], 1.0, 0.4, false);
        let handle = VideoSource::spawn_slideshow(&cfg, tap, Arc::new(AtomicBool::new(false)))
            .expect("spawn");
        let frames = collect_frames(&rx, 4);
        assert!(
            frames.len() >= 45,
            "expected ~50 frames over two 1 s images, got {}",
            frames.len()
        );
        let dark = black_frame(&black).y[0];
        let bright = black_frame(&white).y[0];
        // 25 frames of image A (first image, no transition), then the
        // 10-frame crossfade into image B, then pure white.
        assert!(frames[..24].iter().all(|f| f.y[0] == dark), "image A pure");
        let blend: Vec<u8> = frames[25..35].iter().map(|f| f.y[0]).collect();
        assert!(
            blend[..9]
                .iter()
                .enumerate()
                .all(|(i, &v)| v > dark && v < bright && (i == 0 || v >= blend[i - 1])),
            "crossfade must ramp from black to white, got {blend:?}"
        );
        assert_eq!(blend.last().unwrap(), &bright, "crossfade ends fully white");
        assert_eq!(frames[35].y[0], bright, "transition ends, pure picture");
        drop(handle);
        std::fs::remove_file(&black).ok();
        std::fs::remove_file(&white).ok();
    }

    #[test]
    fn looping_slideshow_restarts_without_pt_jump() {
        let Some(image) = render_test_solid("ss-loop", 320, 240, "black") else {
            return;
        };
        let tap = Arc::new(VideoTap::new());
        let rx = tap.register();
        let cfg = slideshow_cfg(vec![image.clone()], 0.5, 0.0, true);
        let handle = VideoSource::spawn_slideshow(&cfg, tap, Arc::new(AtomicBool::new(false)))
            .expect("spawn");
        let frames = collect_frames(&rx, 3);
        // 3 s at 25 fps spans ~5 loops of the 0.5 s image.
        assert!(
            frames.len() >= 44,
            "looping slideshow must restart the image, got {} frames",
            frames.len()
        );
        assert!(
            frames.windows(2).all(|w| w[0].pts_us < w[1].pts_us),
            "pts must keep increasing across loop boundaries"
        );
        drop(handle);
        std::fs::remove_file(&image).ok();
    }
}
