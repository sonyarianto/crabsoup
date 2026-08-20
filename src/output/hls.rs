//! Live HLS output via the engine tap.
//!
//! Encodes the bus to AAC/ADTS and slices it into a sliding window of
//! MPEG-TS segments (`seg-000000.ts`, ...) plus a media playlist
//! (`playlist.m3u8`). No pacing of its own — the tap paces the stream, like
//! `FileOutput`. With video (Part H6), an optional `VideoTrack` encodes
//! frames from the shared video tap to H.264 and muxes them interleaved by
//! PTS. With `renditions` (G3.3) the tap fans into N independent AAC
//! encodes — one per rendition subdirectory — tied together by a variant
//! master playlist (`index.m3u8`). A web server must serve the directory;
//! crabsoup only writes it.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::json;

use crate::Result;
use crate::config::HlsOutputConfig;
use crate::engine::tap::{AudioFrame, recv_frame_or_shutdown};
use crate::output::encoder::{AacEncoder, Encoder};
use crate::output::mpegts::{MpegTsMuxer, split_adts};
#[cfg(feature = "video")]
use crate::video::{VideoEncoder, VideoFrame, VideoSpec, VideoTap, scale_frame};

/// One AAC frame is 1024 samples per channel on the 90 kHz HLS clock.
const AAC_FRAME_SAMPLES: u64 = 1024;
const CLOCK: u64 = 90_000;
const PLAYLIST: &str = "playlist.m3u8";
/// Variant master playlist, written next to the media playlists so clients
/// can point at `index.m3u8`: with `renditions` it lists every rendition's
/// subdirectory playlist, with video (Part H6) the single A/V stream.
const MASTER: &str = "index.m3u8";
/// Peak audio bitrate (128 kb/s AAC) plus video (1.5 Mb/s H.264).
#[cfg(feature = "video")]
const VARIANT_BANDWIDTH: u64 = 1_628_000;
/// `persist_at` state file format version.
const STATE_VERSION: u32 = 1;
/// Encoder bitrate of the classic single stream (no `renditions`).
const CLASSIC_BITRATE: u32 = 128_000;

/// What the video HLS path needs from the engine: the shared fan-out tap
/// plus the source's stream spec. The output subscribes one consumer per
/// rendition (classic: one) so every rendition can encode its own H.264
/// stream at its own resolution/bitrate. The unit type stands in on
/// non-video builds so the constructor signature stays uniform.
#[cfg(feature = "video")]
pub(crate) type HlsVideo = Option<(Arc<VideoTap>, VideoSpec)>;
#[cfg(not(feature = "video"))]
pub(crate) type HlsVideo = ();

/// Consumes frames from the engine tap, encodes AAC, and rotates HLS
/// segments. The encoders are created and the directory prepared in
/// [`HlsOutput::connect`] so a bad path fails at startup.
pub struct HlsOutput {
    config: HlsOutputConfig,
    rx: Receiver<Arc<AudioFrame>>,
    sample_rate: u32,
    chans: usize,
    shutdown: Arc<AtomicBool>,
    video: HlsVideo,
    /// The output streams, built at connect. One for the classic single
    /// stream (`subdir = ""`) or one per `renditions` entry.
    renditions: Vec<Rendition>,
    /// One video track per rendition (None for audio-only renditions), built
    /// at connect so encoder errors fail fast. Classic video is a single
    /// track at the source spec.
    #[cfg(feature = "video")]
    videos: Vec<Option<VideoTrack>>,
}

/// One output stream: its own encoder, segment state, window and playlist.
/// Either the classic top-level stream (empty `subdir`) or one ABR rendition
/// (`subdir` = the rendition's directory name).
struct Rendition {
    subdir: String,
    dir: PathBuf,
    encoder: AacEncoder,
    /// AAC frames fed so far — this rendition's own PTS timeline (every
    /// rendition encodes the same PCM but each counts independently).
    frames: u64,
    /// Completed segments still in the window, as `(sequence, duration,
    /// rendered file name)` — the name is stored because a custom
    /// `segment_name` template can include the segment's start time.
    closed: Vec<(u64, f64, String)>,
    /// Sequence number of the next segment to open.
    next_seq: u64,
    /// Segment rotations since the stream started; the run loop compares the
    /// sum across renditions to decide when to persist the state file.
    rotations: u64,
    /// Unix seconds when the stream started — the anchor for `{t}` in a
    /// custom `segment_name`.
    epoch: u64,
}

/// State of the segment currently being filled.
struct Segment {
    mux: MpegTsMuxer,
    bytes: Vec<u8>,
    seq: u64,
    /// PTS at which this segment's first sample plays.
    start_pts: u64,
    /// PTS just past the last sample added.
    end_pts: u64,
    /// Total ADTS frames muxed into this segment.
    frames: u64,
    /// The target window has elapsed, but rotation waits for a video
    /// keyframe so the next segment starts with an IDR.
    window_reached: bool,
}

impl Segment {
    fn new(seq: u64, start_pts: u64, has_video: bool) -> Self {
        let mut mux = MpegTsMuxer::new(has_video);
        let mut bytes = Vec::new();
        // Every segment carries its own PAT + PMT so a player joining
        // mid-window can resync without the previous segment.
        mux.write_program(&mut bytes);
        Self {
            mux,
            bytes,
            seq,
            start_pts,
            end_pts: start_pts,
            frames: 0,
            window_reached: false,
        }
    }
}

/// Persisted state of one rendition, as loaded from the `persist_at` file.
struct PersistRendition {
    next_seq: u64,
    closed: Vec<(u64, f64, String)>,
}

impl HlsOutput {
    pub fn new(
        config: HlsOutputConfig,
        rx: Receiver<Arc<AudioFrame>>,
        sample_rate: u32,
        chans: usize,
        video: HlsVideo,
    ) -> Self {
        Self {
            config,
            rx,
            sample_rate,
            chans,
            shutdown: Arc::new(AtomicBool::new(false)),
            video,
            renditions: Vec::new(),
            #[cfg(feature = "video")]
            videos: Vec::new(),
        }
    }

    /// Give the output a shared flag that stops the consume loop (used for
    /// graceful Ctrl-C shutdown).
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = flag;
    }

    /// Prepare the output directory: create it (and any rendition
    /// subdirectories), resume from `persist_at` state or clear stale
    /// segments/playlists, build the per-rendition encoders, and write the
    /// master playlist.
    pub fn connect(&mut self) -> Result<()> {
        fs::create_dir_all(&self.config.directory)
            .map_err(|e| format!("cannot create {}: {e}", self.config.directory.display()))?;
        let state = load_state(&self.config)?;
        let resuming = state.is_some();
        if !resuming {
            clear_directory(&self.config)?;
        }
        // One stream per rendition, or the classic single stream.
        let subdirs: Vec<String> = if self.config.renditions.is_empty() {
            vec![String::new()]
        } else {
            self.config
                .renditions
                .iter()
                .map(|r| r.name.clone())
                .collect()
        };
        let epoch = unix_seconds();
        self.renditions = subdirs
            .iter()
            .map(|sub| {
                let dir = rendition_dir(&self.config, sub);
                fs::create_dir_all(&dir)
                    .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                let bitrate = self
                    .config
                    .renditions
                    .iter()
                    .find(|r| &r.name == sub)
                    .map(|r| r.bitrate)
                    .unwrap_or(CLASSIC_BITRATE);
                let (next_seq, closed) = match &state {
                    Some(st) => st
                        .get(sub)
                        .map(|p| (p.next_seq, p.closed.clone()))
                        .unwrap_or((0, Vec::new())),
                    None => (0, Vec::new()),
                };
                let encoder = AacEncoder::new(self.sample_rate, self.chans as u16, bitrate)?;
                Ok(Rendition {
                    subdir: sub.clone(),
                    dir,
                    encoder,
                    frames: 0,
                    closed,
                    next_seq,
                    rotations: 0,
                    epoch,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        // One video track per rendition, subscribed to the shared tap
        // (classic: a single track at the source spec; renditions: one per
        // entry at its own resolution/bitrate). Built here so encoder errors
        // fail at startup.
        #[cfg(feature = "video")]
        {
            self.videos = if let Some((tap, spec)) = &self.video {
                let mut out = Vec::with_capacity(self.renditions.len());
                if self.config.renditions.is_empty() {
                    out.push(Some(VideoTrack::new(tap.register(), *spec)?));
                } else {
                    for r in &self.config.renditions {
                        let (w, h) = (
                            r.width.unwrap_or(spec.width),
                            r.height.unwrap_or(spec.height),
                        );
                        out.push(Some(VideoTrack::new_scaled(
                            tap.register(),
                            *spec,
                            w,
                            h,
                            r.video_bitrate,
                        )?));
                    }
                }
                out
            } else {
                (0..self.renditions.len()).map(|_| None).collect()
            };
        }
        if resuming {
            // Drop on-disk segments outside the retained window and rebuild
            // the live playlists so the directory matches the loaded state.
            for r in &self.renditions {
                prune_orphans(&self.config, r)?;
                write_playlist(r, false)?;
            }
        }
        // Variant master playlist: multi-rendition ABR lists every
        // rendition (with RESOLUTION/avc CODECS when video is enabled); the
        // classic single video stream gets its own single-variant master.
        if !self.config.renditions.is_empty() {
            // The source (effect-scaled) resolution resolves renditions that
            // leave width/height unset.
            #[cfg(feature = "video")]
            let source_size = self.video.as_ref().map(|(_, s)| (s.width, s.height));
            #[cfg(not(feature = "video"))]
            let source_size: Option<(u32, u32)> = None;
            write_master(&self.config, source_size)?;
        }
        // Classic single-stream video keeps its one-variant master; ABR
        // renditions get the multi-variant master above instead.
        #[cfg(feature = "video")]
        if self.config.renditions.is_empty()
            && let Some((_, spec)) = &self.video
        {
            write_master_playlist(&self.config.directory, spec)?;
        }
        log::info!("hls: segments to {}", self.config.directory.display());
        Ok(())
    }

    /// Consume frames until the stream ends (senders dropped) or shutdown is
    /// requested, then flush the encoder tail into the final segment and
    /// finalize the playlist with `#EXT-X-ENDLIST`. With `persist_at`, the
    /// state file is rewritten on every segment rotation so a kill
    /// mid-segment can be resumed without renumbering.
    pub fn run(&mut self) -> Result<()> {
        let cfg = self.config.clone();
        let sample_rate = self.sample_rate;
        let has_video = self.video_present();
        let mut renditions = std::mem::take(&mut self.renditions);
        #[cfg(feature = "video")]
        let mut videos = std::mem::take(&mut self.videos);

        // Each initial segment consumes its rendition's next sequence
        // number, so the first rotation opens seq+1 (never a renumber).
        let mut segs: Vec<Segment> = renditions
            .iter_mut()
            .map(|r| {
                let seg = Segment::new(r.next_seq, 0, has_video);
                r.next_seq += 1;
                seg
            })
            .collect();

        // Every rendition encodes the same PCM with its own encoder and
        // rotates its own segment window; a video rendition additionally
        // flushes its H.264 track into that window before each audio feed
        // (keyframe-aligned rotation, Part H6). The segments and tracks are
        // parallel Vecs so the rotation closure borrows `r`, `seg` and the
        // track disjointly.
        #[cfg(feature = "video")]
        let frame_dur = (AAC_FRAME_SAMPLES * CLOCK) / sample_rate as u64;
        #[cfg(not(feature = "video"))]
        #[cfg_attr(not(feature = "video"), allow(unused_variables))]
        let frame_dur: u64 = 0;
        let mut before_rot: u64 = renditions.iter().map(|r| r.rotations).sum();
        while let Some(frame) = recv_frame_or_shutdown(&self.rx, &self.shutdown) {
            for i in 0..renditions.len() {
                let (r, seg) = (&mut renditions[i], &mut segs[i]);
                #[cfg(feature = "video")]
                let defer = {
                    if let Some(v) = &mut videos[i] {
                        let audio_pts = r.frames.wrapping_mul(frame_dur);
                        let defer = v.alive();
                        v.flush_up_to(audio_pts, seg, &mut |s, pts| {
                            rotate(&cfg, r, s, pts)
                        })?;
                        defer
                    } else {
                        false
                    }
                };
                #[cfg(not(feature = "video"))]
                let defer = false;
                let adts = r.encoder.encode(&frame.pcm);
                feed(&cfg, sample_rate, r, seg, &adts, defer)?;
            }
            let after_rot: u64 = renditions.iter().map(|r| r.rotations).sum();
            if after_rot != before_rot {
                write_state(&cfg, &renditions)?;
                before_rot = after_rot;
            }
        }
        let total: usize = renditions.iter().map(|r| r.closed.len()).sum();
        for i in 0..renditions.len() {
            let (r, seg) = (&mut renditions[i], &mut segs[i]);
            #[cfg(feature = "video")]
            if let Some(v) = &mut videos[i] {
                v.flush_remaining(seg)?;
            }
            let tail = r.encoder.finish();
            feed(&cfg, sample_rate, r, seg, &tail, false)?;
            finish_segment(&cfg, r, seg)?;
            write_playlist(r, true)?;
        }
        write_state(&cfg, &renditions)?;
        log::info!(
            "hls closed: {total} segments in {}",
            cfg.directory.display()
        );
        Ok(())
    }

    #[cfg(feature = "video")]
    fn video_present(&self) -> bool {
        self.video.is_some()
    }

    #[cfg(not(feature = "video"))]
    fn video_present(&self) -> bool {
        let _ = &self.video;
        false
    }


}

/// Route ADTS frames into segments, closing a segment once its window
/// crosses `segment_seconds`. When `defer` is set (video is live), a window
/// crossing only marks `window_reached` — the actual rotation waits for the
/// video track to mux a keyframe into the next segment.
fn feed(
    cfg: &HlsOutputConfig,
    sample_rate: u32,
    r: &mut Rendition,
    seg: &mut Segment,
    adts: &[u8],
    defer: bool,
) -> Result<()> {
    if adts.is_empty() {
        return Ok(());
    }
    let frame_dur = (AAC_FRAME_SAMPLES * CLOCK) / sample_rate as u64;
    let window = (cfg.segment_seconds * CLOCK as f64) as u64;
    for frame in split_adts(adts) {
        let pts = r.frames.wrapping_mul(frame_dur);
        if seg.frames > 0 && pts.wrapping_sub(seg.start_pts) >= window {
            if !seg.window_reached {
                if defer {
                    seg.window_reached = true;
                } else {
                    rotate(cfg, r, seg, pts)?;
                }
            } else if !defer || pts.wrapping_sub(seg.start_pts) >= 2 * window {
                // No keyframe came: the video tap died (defer off) or is
                // stalled past one whole extra window. Rotate anyway
                // rather than grow the segment forever.
                rotate(cfg, r, seg, pts)?;
            }
        }
        seg.mux.push_audio(frame, pts, &mut seg.bytes);
        seg.end_pts = pts + frame_dur;
        seg.frames += 1;
        r.frames += 1;
    }
    Ok(())
}

/// Close `seg` and open the next one at `next_start_pts`. Called from both
/// the audio feed (immediate rotation) and the video track (once it holds a
/// keyframe to start the next segment).
fn rotate(
    cfg: &HlsOutputConfig,
    r: &mut Rendition,
    seg: &mut Segment,
    next_start_pts: u64,
) -> Result<()> {
    let has_video = seg.mux.has_video();
    finish_segment(cfg, r, seg)?;
    *seg = Segment::new(r.next_seq, next_start_pts, has_video);
    r.next_seq += 1;
    Ok(())
}

/// Write the finished segment to disk, trim the window to `retention`
/// segments, and rewrite the live playlist.
fn finish_segment(cfg: &HlsOutputConfig, r: &mut Rendition, seg: &mut Segment) -> Result<()> {
    if seg.frames == 0 {
        return Ok(());
    }
    let duration = (seg.end_pts - seg.start_pts) as f64 / CLOCK as f64;
    let name = render_segment_name(&cfg.segment_name, seg.seq, r.epoch + seg.start_pts / CLOCK);
    let path = r.dir.join(&name);
    fs::write(&path, &seg.bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
    r.closed.push((seg.seq, duration, name));
    r.rotations += 1;
    log::info!("hls segment {} closed ({duration:.2}s)", path.display());

    // Retention window: drop segments older than the last `retention`.
    let keep_below = r.closed.last().map(|(seq, _, _)| *seq).unwrap_or(0);
    let drop_below = keep_below.saturating_sub(cfg.retention as u64);
    let trimmed: Vec<String> = r
        .closed
        .iter()
        .filter(|(seq, _, _)| *seq < drop_below)
        .map(|(_, _, name)| name.clone())
        .collect();
    r.closed.retain(|(seq, _, _)| *seq >= drop_below);
    for name in trimmed {
        let _ = fs::remove_file(r.dir.join(name));
    }
    write_playlist(r, false)
}

/// Rewrite the rendition's `playlist.m3u8` describing the retained window
/// (or with `#EXT-X-ENDLIST` once the stream is over).
fn write_playlist(r: &Rendition, finalized: bool) -> Result<()> {
    let target = r
        .closed
        .iter()
        .map(|(_, d, _)| d.ceil() as u64)
        .max()
        .unwrap_or(1)
        .max(1);
    let first = r.closed.first().map(|(seq, _, _)| *seq).unwrap_or(0);
    let mut out = String::new();
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:3\n");
    out.push_str(&format!("#EXT-X-TARGETDURATION:{target}\n"));
    out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first}\n"));
    for (_, duration, name) in &r.closed {
        out.push_str(&format!("#EXTINF:{duration:.3},\n"));
        out.push_str(&format!("{name}\n"));
    }
    if finalized {
        out.push_str("#EXT-X-ENDLIST\n");
    }
    let path = r.dir.join(PLAYLIST);
    fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Render a segment file name from the template: `{n}` = zero-padded
/// sequence number, `{t}` = unix seconds of the segment's start.
fn render_segment_name(template: &str, seq: u64, t: u64) -> String {
    template
        .replace("{n}", &format!("{seq:06}"))
        .replace("{t}", &t.to_string())
}

/// Does `name` match the segment-name template (digits in place of `{n}` /
/// `{t}`)? Used by connect to clear stale segments and prune orphans on
/// resume.
fn name_matches_template(name: &str, template: &str) -> bool {
    let mut name_chars = name.chars().peekable();
    let mut t = template;
    while !t.is_empty() {
        if let Some(rest) = t.strip_prefix("{n}").or_else(|| t.strip_prefix("{t}")) {
            let mut any = false;
            while let Some(&c) = name_chars.peek() {
                if c.is_ascii_digit() {
                    any = true;
                    name_chars.next();
                } else {
                    break;
                }
            }
            if !any {
                return false;
            }
            t = rest;
        } else {
            let c = t.chars().next().expect("template non-empty");
            if name_chars.next() != Some(c) {
                return false;
            }
            t = &t[c.len_utf8()..];
        }
    }
    name_chars.next().is_none()
}

fn rendition_dir(cfg: &HlsOutputConfig, subdir: &str) -> PathBuf {
    if subdir.is_empty() {
        cfg.directory.clone()
    } else {
        cfg.directory.join(subdir)
    }
}

/// Fresh start: remove playlists, the master, and every segment file
/// (matching the name template) from the top level and each rendition
/// subdirectory.
fn clear_directory(cfg: &HlsOutputConfig) -> Result<()> {
    for dir in std::iter::once(cfg.directory.clone())
        .chain(cfg.renditions.iter().map(|r| cfg.directory.join(&r.name)))
    {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue; // subdirectory not created yet
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == PLAYLIST
                || name == MASTER
                || name_matches_template(&name, &cfg.segment_name)
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(())
}

/// Resume: remove on-disk segment files that are not part of the retained
/// window (stale files from before the crash that were already trimmed in
/// the persisted state).
fn prune_orphans(cfg: &HlsOutputConfig, r: &Rendition) -> Result<()> {
    let Ok(entries) = fs::read_dir(&r.dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name_matches_template(&name, &cfg.segment_name) {
            continue;
        }
        if !r.closed.iter().any(|(_, _, n)| *n == name) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

/// Write `index.m3u8` listing every rendition's media playlist (G3.3).
/// BANDWIDTH includes the TS container overhead (~10 %); video renditions
/// carry the combined audio+video bitrate, a RESOLUTION attribute, and the
/// avc codec tag. `source_size` is the (effect-scaled) source resolution,
/// which resolves renditions that leave width/height unset.
fn write_master(cfg: &HlsOutputConfig, source_size: Option<(u32, u32)>) -> Result<()> {
    let mut out = String::new();
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:3\n");
    for r in &cfg.renditions {
        let video = r.video_bitrate > 0 && source_size.is_some();
        let bandwidth = if video {
            (r.bitrate as f64 + r.video_bitrate as f64) * 1.1
        } else {
            r.bitrate as f64 * 1.1
        } as u64;
        let (w, h) = match (r.width, r.height, source_size) {
            (Some(w), Some(h), _) => (w, h),
            (None, None, Some((w, h))) => (w, h),
            // Validated at script evaluation; unreachable in practice.
            _ => (0, 0),
        };
        let codecs = if video {
            "avc1.42401f,mp4a.40.2"
        } else {
            "mp4a.40.2"
        };
        let inf = if video {
            format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},RESOLUTION={w}x{h},CODECS=\"{codecs}\",NAME=\"{}\"",
                r.name
            )
        } else {
            format!(
                "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"{codecs}\",NAME=\"{}\"",
                r.name
            )
        };
        out.push_str(&inf);
        out.push('\n');
        out.push_str(&format!("{}/playlist.m3u8\n", r.name));
    }
    let path = cfg.directory.join(MASTER);
    fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// Load the `persist_at` state file, if it exists and parses. `None` means a
/// fresh start (clear the directory); a missing or corrupt file logs a
/// warning and also starts fresh rather than failing startup.
fn load_state(cfg: &HlsOutputConfig) -> Result<Option<HashMap<String, PersistRendition>>> {
    let Some(path) = &cfg.persist_at else {
        return Ok(None);
    };
    if !path.exists() {
        return Ok(None);
    }
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("hls: cannot read persist state {}: {e}", path.display());
            return Ok(None);
        }
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            log::warn!(
                "hls: ignoring unreadable persist state {}: {e}",
                path.display()
            );
            return Ok(None);
        }
    };
    let Some(rends) = value.get("renditions").and_then(|v| v.as_object()) else {
        return Ok(None);
    };
    let mut out = HashMap::new();
    for (name, rv) in rends {
        let next_seq = rv.get("next_seq").and_then(|v| v.as_u64()).unwrap_or(0);
        let mut closed = Vec::new();
        if let Some(list) = rv.get("closed").and_then(|v| v.as_array()) {
            for item in list {
                let Some(arr) = item.as_array() else { continue };
                if arr.len() == 3
                    && let (Some(seq), Some(dur), Some(seg)) = (
                        arr[0].as_u64(),
                        arr[1].as_f64(),
                        arr[2].as_str(),
                    )
                {
                    closed.push((seq, dur, seg.to_string()));
                }
            }
        }
        out.insert(name.clone(), PersistRendition { next_seq, closed });
    }
    Ok(Some(out))
}

/// Write the `persist_at` state file: each rendition's next segment counter
/// and retained window. Written on every segment rotation so a kill
/// mid-segment resumes cleanly. The write goes to a temp sibling and is
/// renamed so a kill never leaves a partial state file behind.
fn write_state(cfg: &HlsOutputConfig, renditions: &[Rendition]) -> Result<()> {
    let Some(path) = &cfg.persist_at else {
        return Ok(());
    };
    let mut rends = serde_json::Map::new();
    for r in renditions {
        let closed: Vec<serde_json::Value> = r
            .closed
            .iter()
            .map(|(seq, dur, name)| json!([seq, dur, name]))
            .collect();
        rends.insert(
            r.subdir.clone(),
            json!({ "next_seq": r.next_seq, "closed": closed }),
        );
    }
    let state = json!({ "version": STATE_VERSION, "renditions": rends });
    let text = serde_json::to_string_pretty(&state)
        .map_err(|e| format!("hls persist state: {e}"))?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|e| format!("rename {}: {e}", path.display()))?;
    Ok(())
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Write `index.m3u8` pointing at the media playlist, describing the video
/// variant. CODECS is static for the fixed encoder settings (H.264
/// constrained baseline, AAC-LC).
#[cfg(feature = "video")]
fn write_master_playlist(dir: &std::path::Path, spec: &VideoSpec) -> Result<()> {
    let out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:3\n\
         #EXT-X-STREAM-INF:BANDWIDTH={VARIANT_BANDWIDTH},RESOLUTION={}x{},CODECS=\"avc1.42401f,mp4a.40.2\"\n\
         {PLAYLIST}\n",
        spec.width, spec.height
    );
    let path = dir.join(MASTER);
    fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// The H.264 half of the HLS pipeline (Part H6): drains the video tap,
/// encodes frames, and hands access units to the segment muxer in PTS
/// order. One-frame lookahead keeps a frame whose PTS is ahead of the
/// current audio PTS buffered until the audio catches up.
#[cfg(feature = "video")]
struct VideoTrack {
    rx: Receiver<Arc<VideoFrame>>,
    encoder: VideoEncoder,
    /// PTS at which the next frame will be published, so muxing can
    /// estimate frame durations for the segment clock.
    frame_dur_90k: u64,
    /// The lookahead frame, if it was pulled too early.
    next: Option<Arc<VideoFrame>>,
    /// Encoded access units awaiting their PTS.
    pending: Vec<crate::video::EncodedAu>,
    /// The tap's sender is gone and the channel drained.
    eof: bool,
    /// This track's encode size; frames at the source spec are rescaled to
    /// it before encoding.
    target: (u32, u32),
}

#[cfg(feature = "video")]
impl VideoTrack {
    /// Classic single stream: encode at the source spec, 1.5 Mb/s.
    fn new(rx: Receiver<Arc<VideoFrame>>, spec: VideoSpec) -> Result<Self> {
        Self::new_scaled(rx, spec, spec.width, spec.height, 1_500_000)
    }

    /// A rendition's video encode at `width`x`height` and `bitrate`.
    /// Frames arriving at a different size (the source spec) are rescaled
    /// with the pure-Rust bilinear resampler before encoding.
    fn new_scaled(
        rx: Receiver<Arc<VideoFrame>>,
        spec: VideoSpec,
        width: u32,
        height: u32,
        bitrate: u64,
    ) -> Result<Self> {
        let fps = spec.frame_rate.max(1.0);
        let encoder = VideoEncoder::h264(width, height, (fps * 1000.0) as i32, 1000, bitrate)?;
        Ok(Self {
            rx,
            encoder,
            frame_dur_90k: (CLOCK as f64 / fps) as u64,
            next: None,
            pending: Vec::new(),
            eof: false,
            target: (width, height),
        })
    }

    fn alive(&self) -> bool {
        !self.eof
    }

    /// Encode and mux every frame/AU whose PTS is at or before `up_to_pts`
    /// (the current audio PTS), keeping later material buffered. Once the
    /// segment's window is reached, the next picture is forced to a
    /// keyframe so the rotation that follows starts the new segment with
    /// an IDR.
    fn flush_up_to(
        &mut self,
        up_to_pts: u64,
        seg: &mut Segment,
        rotate: &mut dyn FnMut(&mut Segment, u64) -> Result<()>,
    ) -> Result<()> {
        loop {
            let frame = match self.next.take() {
                Some(f) => f,
                None => match self.rx.try_recv() {
                    Ok(f) => f,
                    Err(e) => {
                        if e == std::sync::mpsc::TryRecvError::Disconnected {
                            self.eof = true;
                        }
                        break;
                    }
                },
            };
            let pts = frame.pts_us * CLOCK / 1_000_000;
            if pts > up_to_pts {
                self.next = Some(frame);
                break;
            }
            // While the window is pending, every picture is forced to a
            // keyframe; the first one muxed rotates the segment. Keeping
            // the force on until the rotation lands makes a stalled tap
            // self-heal: the moment frames resume, the next picture is an
            // IDR and the cut happens there.
            if seg.window_reached {
                self.encoder.force_keyframe();
            }
            let (tw, th) = self.target;
            if frame.width == tw && frame.height == th {
                for au in self.encoder.push(&frame)? {
                    self.pending.push(au);
                }
            } else {
                let scaled = scale_frame(&frame, tw, th);
                for au in self.encoder.push(&scaled)? {
                    self.pending.push(au);
                }
            }
            self.mux_pending(up_to_pts, seg, rotate)?;
        }
        self.mux_pending(up_to_pts, seg, rotate)
    }

    /// End of stream: drain the tap (the decode thread has ended), encode
    /// the tail, and mux everything with no PTS limit.
    fn flush_remaining(&mut self, seg: &mut Segment) -> Result<()> {
        loop {
            let frame = match self.next.take() {
                Some(f) => f,
                None => match self.rx.try_recv() {
                    Ok(f) => f,
                    Err(e) => {
                        if e == std::sync::mpsc::TryRecvError::Disconnected {
                            self.eof = true;
                        }
                        break;
                    }
                },
            };
            let (tw, th) = self.target;
            if frame.width == tw && frame.height == th {
                for au in self.encoder.push(&frame)? {
                    self.pending.push(au);
                }
            } else {
                let scaled = scale_frame(&frame, tw, th);
                for au in self.encoder.push(&scaled)? {
                    self.pending.push(au);
                }
            }
        }
        for au in self.encoder.finish()? {
            self.pending.push(au);
        }
        let mut taken = std::mem::take(&mut self.pending);
        for au in taken.drain(..) {
            self.mux_au(&au, seg, &mut |_, _| Ok(()))?;
        }
        Ok(())
    }

    fn mux_pending(
        &mut self,
        up_to_pts: u64,
        seg: &mut Segment,
        rotate: &mut dyn FnMut(&mut Segment, u64) -> Result<()>,
    ) -> Result<()> {
        let mut ready = Vec::new();
        self.pending.retain(|au| {
            if au.pts_90k <= up_to_pts {
                ready.push(crate::video::EncodedAu {
                    pts_90k: au.pts_90k,
                    data: au.data.clone(),
                });
                false
            } else {
                true
            }
        });
        for au in ready {
            self.mux_au(&au, seg, rotate)?;
        }
        Ok(())
    }

    fn mux_au(
        &mut self,
        au: &crate::video::EncodedAu,
        seg: &mut Segment,
        rotate: &mut dyn FnMut(&mut Segment, u64) -> Result<()>,
    ) -> Result<()> {
        // A keyframe is the one safe place to cut: rotate here so the new
        // segment starts with SPS/PPS/IDR and players can join mid-stream.
        if seg.window_reached && au.is_idr() {
            rotate(seg, au.pts_90k)?;
        }
        seg.mux.push_video(&au.data, au.pts_90k, &mut seg.bytes);
        seg.end_pts = seg.end_pts.max(au.pts_90k + self.frame_dur_90k);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    // On non-video builds the HLS constructor takes a unit stand-in for the
    // video subscription; clippy flags the literal.
    #![allow(clippy::unit_arg, clippy::unused_unit)]
    use super::*;
    use std::path::Path;
    use std::sync::mpsc;
    #[cfg(feature = "video")]
    use std::time::Duration;

    #[cfg(not(feature = "video"))]
    fn no_video() -> HlsVideo {
        ()
    }
    #[cfg(feature = "video")]
    fn no_video() -> HlsVideo {
        None
    }

    fn sine_frames(tx: &mpsc::SyncSender<Arc<AudioFrame>>, seconds: f64) {
        let rate = 44_100.0;
        let mut phase = 0.0;
        let mut done = 0;
        let total = (seconds * rate) as usize;
        while done < total {
            let n = 4096.min(total - done);
            let mut pcm = Vec::with_capacity(n);
            for _ in 0..n {
                pcm.push((phase * 2.0 * std::f64::consts::PI * 440.0 / rate).sin() as f32 * 0.5);
                phase += 1.0;
            }
            let frame = Arc::new(AudioFrame {
                pcm,
                label: Some("test tone".into()),
                pool: None,
            });
            tx.send(frame).expect("tap channel");
            done += n;
        }
    }

    /// Like [`sine_frames`] but paced at real time — the video-tap consumer
    /// drains on the audio clock, so a burst would overflow the tap's
    /// drop-oldest channel and lose video frames.
    #[cfg(feature = "video")]
    fn paced_sine_frames(tx: &mpsc::SyncSender<Arc<AudioFrame>>, seconds: f64) {
        let rate = 44_100.0;
        let mut phase = 0.0;
        let mut done = 0;
        let total = (seconds * rate) as usize;
        while done < total {
            let n = 4096.min(total - done);
            let mut pcm = Vec::with_capacity(n);
            for _ in 0..n {
                pcm.push((phase * 2.0 * std::f64::consts::PI * 440.0 / rate).sin() as f32 * 0.5);
                phase += 1.0;
            }
            let frame = Arc::new(AudioFrame {
                pcm,
                label: Some("test tone".into()),
                pool: None,
            });
            tx.send(frame).expect("tap channel");
            done += n;
            std::thread::sleep(Duration::from_millis((n as f64 / rate * 1000.0) as u64));
        }
    }

    /// Sequence numbers referenced by a media playlist, in order.
    fn playlist_seqs(dir: &Path) -> Vec<u64> {
        fs::read_to_string(dir.join(PLAYLIST))
            .unwrap()
            .lines()
            .filter_map(|l| {
                let rest = l.strip_prefix("seg-")?;
                let n = rest.strip_suffix(".ts")?;
                n.parse().ok()
            })
            .collect()
    }

    #[test]
    fn writes_windowed_segments_and_playlist() {
        let dir = std::env::temp_dir().join("crabsoup-hls-test");
        let _ = fs::remove_dir_all(&dir);
        let cfg = HlsOutputConfig {
            directory: dir.clone(),
            segment_seconds: 1.0,
            retention: 4,
            video: false,
            renditions: Vec::new(),
            segment_name: "seg-{n}.ts".into(),
            persist_at: None,
            fallible: false,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = HlsOutput::new(cfg, rx, 44_100, 1, no_video());
        output.connect().expect("dir opens");

        let handle = std::thread::spawn(move || output.run());
        sine_frames(&tx, 3.5);
        drop(tx);
        handle.join().expect("hls thread").expect("clean finish");

        let playlist = fs::read_to_string(dir.join(PLAYLIST)).unwrap();
        assert!(playlist.starts_with("#EXTM3U\n"));
        assert!(playlist.contains("#EXT-X-VERSION:3"));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:"));
        assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
        assert!(playlist.contains("#EXTINF:"));
        assert!(playlist.trim_end().ends_with("#EXT-X-ENDLIST"));

        let segments: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".ts"))
            .collect();
        assert!(
            segments.len() >= 3,
            "expected several segments, got {}",
            segments.len()
        );
        for seg in &segments {
            let data = fs::read(seg.path()).unwrap();
            assert_eq!(data.len() % 188, 0, "segment not TS-packet aligned");
            assert_eq!(data[0], 0x47, "segment missing TS sync");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retention_trims_old_segments() {
        let dir = std::env::temp_dir().join("crabsoup-hls-retention");
        let _ = fs::remove_dir_all(&dir);
        let cfg = HlsOutputConfig {
            directory: dir.clone(),
            segment_seconds: 0.5,
            retention: 2,
            video: false,
            renditions: Vec::new(),
            segment_name: "seg-{n}.ts".into(),
            persist_at: None,
            fallible: false,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = HlsOutput::new(cfg, rx, 44_100, 1, no_video());
        output.connect().expect("dir opens");

        let handle = std::thread::spawn(move || output.run());
        sine_frames(&tx, 4.0);
        drop(tx);
        handle.join().expect("hls thread").expect("clean finish");

        let playlist = fs::read_to_string(dir.join(PLAYLIST)).unwrap();
        let media_seq = playlist
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert!(media_seq > 0, "window must have slid: {media_seq}");
        let segments = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".ts"))
            .count();
        assert!(
            segments <= 4,
            "retention=2 should cap window, got {segments}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_multi_rendition_playlists_and_master() {
        let dir = std::env::temp_dir().join("crabsoup-hls-abr");
        let _ = fs::remove_dir_all(&dir);
        let cfg = HlsOutputConfig {
            directory: dir.clone(),
            segment_seconds: 1.0,
            retention: 4,
            video: false,
            renditions: vec![
                crate::config::HlsRendition {
                    name: "64k".into(),
                    bitrate: 64_000,
                    video_bitrate: 0,
                    width: None,
                    height: None,
                },
                crate::config::HlsRendition {
                    name: "128k".into(),
                    bitrate: 128_000,
                    video_bitrate: 0,
                    width: None,
                    height: None,
                },
            ],
            segment_name: "seg-{n}.ts".into(),
            persist_at: None,
            fallible: false,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = HlsOutput::new(cfg, rx, 44_100, 1, no_video());
        output.connect().expect("dir opens");

        let handle = std::thread::spawn(move || output.run());
        sine_frames(&tx, 3.5);
        drop(tx);
        handle.join().expect("hls thread").expect("clean finish");

        // Variant master playlist: one STREAM-INF per rendition, pointing at
        // each subdirectory's media playlist. BANDWIDTH = bitrate + 10 %.
        let master = fs::read_to_string(dir.join(MASTER)).unwrap();
        assert!(master.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=70400,CODECS=\"mp4a.40.2\",NAME=\"64k\""
        ));
        assert!(master.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=140800,CODECS=\"mp4a.40.2\",NAME=\"128k\""
        ));
        assert!(master.contains("64k/playlist.m3u8"));
        assert!(master.contains("128k/playlist.m3u8"));
        assert!(master.trim_end().ends_with("128k/playlist.m3u8"));

        // Every rendition has its own segments and a finalized playlist.
        for name in ["64k", "128k"] {
            let sub = dir.join(name);
            let playlist = fs::read_to_string(sub.join(PLAYLIST)).unwrap();
            assert!(playlist.contains("#EXT-X-MEDIA-SEQUENCE:0"));
            assert!(
                playlist.trim_end().ends_with("#EXT-X-ENDLIST"),
                "{name} playlist must finalize"
            );
            let segments: Vec<_> = fs::read_dir(&sub)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".ts"))
                .collect();
            assert!(
                segments.len() >= 3,
                "{name}: expected segments, got {}",
                segments.len()
            );
            for seg in &segments {
                let data = fs::read(seg.path()).unwrap();
                assert_eq!(data.len() % 188, 0, "{name}: not TS-aligned");
                assert_eq!(data[0], 0x47, "{name}: missing TS sync");
            }
        }
        // Acceptance: ffprobe reads the master playlist and decodes a window
        // of each rendition (skips when ffprobe is absent).
        if std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            let out = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_name",
                    "-of",
                    "csv=p=0",
                ])
                .arg(dir.join(MASTER))
                .output()
                .expect("run ffprobe");
            assert!(
                out.status.success(),
                "ffprobe master failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(
                String::from_utf8_lossy(&out.stdout).contains("aac"),
                "master must resolve an AAC stream"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn custom_segment_name_is_used() {
        let dir = std::env::temp_dir().join("crabsoup-hls-name");
        let _ = fs::remove_dir_all(&dir);
        let cfg = HlsOutputConfig {
            directory: dir.clone(),
            segment_seconds: 1.0,
            retention: 4,
            video: false,
            renditions: Vec::new(),
            segment_name: "chunk-{t}.ts".into(),
            persist_at: None,
            fallible: false,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = HlsOutput::new(cfg, rx, 44_100, 1, no_video());
        output.connect().expect("dir opens");

        let handle = std::thread::spawn(move || output.run());
        sine_frames(&tx, 2.5);
        drop(tx);
        handle.join().expect("hls thread").expect("clean finish");

        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".ts"))
            .collect();
        assert!(!names.is_empty(), "expected timestamped segments");
        let playlist = fs::read_to_string(dir.join(PLAYLIST)).unwrap();
        for name in &names {
            let stem = name
                .strip_prefix("chunk-")
                .and_then(|s| s.strip_suffix(".ts"))
                .unwrap_or_default();
            assert!(
                !stem.is_empty() && stem.chars().all(|c| c.is_ascii_digit()),
                "expected chunk-<unix>.ts, got {name}"
            );
            assert!(playlist.contains(name.as_str()), "playlist missing {name}");
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persist_state_resumes_segment_numbering() {
        let dir = std::env::temp_dir().join("crabsoup-hls-persist");
        let state = std::env::temp_dir().join("crabsoup-hls-persist-state.json");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&state);
        let make_cfg = || HlsOutputConfig {
            directory: dir.clone(),
            segment_seconds: 0.5,
            retention: 4,
            video: false,
            renditions: Vec::new(),
            segment_name: "seg-{n}.ts".into(),
            persist_at: Some(state.clone()),
            fallible: false,
        };
        // Run 1 (a "live" run, then a kill): segments 0..N, state written on
        // every rotation.
        {
            let (tx, rx) = mpsc::sync_channel(8);
            let mut output = HlsOutput::new(make_cfg(), rx, 44_100, 1, no_video());
            output.connect().expect("dir opens");
            let handle = std::thread::spawn(move || output.run());
            sine_frames(&tx, 1.5);
            drop(tx);
            handle.join().expect("hls thread").expect("clean finish");
        }
        assert!(state.exists(), "state file must be written");
        // Regression: rotations must never renumber — a duplicate name would
        // have overwritten an existing segment file.
        let run1_text = fs::read_to_string(dir.join(PLAYLIST)).unwrap();
        let run1_names: Vec<&str> = run1_text
            .lines()
            .filter(|l| l.ends_with(".ts"))
            .collect();
        let mut unique = run1_names.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            run1_names.len(),
            "playlist must not repeat segment names: {run1_names:?}"
        );
        let run1: Vec<u64> = playlist_seqs(&dir);
        let last1 = *run1.last().expect("run 1 segments");
        assert!(last1 > 0, "run 1 must have closed several segments");

        // Run 2 (the restart): the directory is NOT cleared, the retained
        // window is preserved, and numbering continues at last1 + 1.
        {
            let (tx, rx) = mpsc::sync_channel(8);
            let mut output = HlsOutput::new(make_cfg(), rx, 44_100, 1, no_video());
            output.connect().expect("dir opens");
            let handle = std::thread::spawn(move || output.run());
            sine_frames(&tx, 0.8);
            drop(tx);
            handle.join().expect("hls thread").expect("clean finish");
        }
        let run2: Vec<u64> = playlist_seqs(&dir);
        assert!(
            run2.contains(&last1),
            "run 1's last segment must survive the restart: {run1:?} -> {run2:?}"
        );
        assert!(
            run2.contains(&(last1 + 1)),
            "numbering must continue at last+1: {run1:?} -> {run2:?}"
        );
        assert!(
            dir.join(format!("seg-{last1:06}.ts")).exists(),
            "run 1 segment file must survive"
        );
        let media_seq = fs::read_to_string(dir.join(PLAYLIST))
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:"))
            .unwrap()
            .parse::<u64>()
            .unwrap();
        assert_eq!(media_seq, run2[0], "media sequence = window start");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_file(&state);
    }

    #[cfg(feature = "video")]
    #[test]
    fn writes_video_renditions_with_master_resolutions() {
        use crate::video::{VideoSpec, testutil::render_test_clip};

        let Some(clip) = render_test_clip("hls-video-abr") else {
            return;
        };
        let dir = std::env::temp_dir().join("crabsoup-hls-video-abr");
        let _ = fs::remove_dir_all(&dir);
        // 360p rescales the 320x240 source down; "src" keeps the source
        // resolution (width/height unset) at a higher video bitrate.
        let cfg = HlsOutputConfig {
            directory: dir.clone(),
            segment_seconds: 0.5,
            retention: 4,
            video: true,
            renditions: vec![
                crate::config::HlsRendition {
                    name: "360p".into(),
                    bitrate: 64_000,
                    video_bitrate: 500_000,
                    width: Some(320),
                    height: Some(180),
                },
                crate::config::HlsRendition {
                    name: "src".into(),
                    bitrate: 128_000,
                    video_bitrate: 1_000_000,
                    width: None,
                    height: None,
                },
            ],
            segment_name: "seg-{n}.ts".into(),
            persist_at: None,
            fallible: false,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let tap = Arc::new(VideoTap::new());
        let pub_tap = tap.clone();
        let mut output = HlsOutput::new(
            cfg,
            rx,
            44_100,
            1,
            Some((
                tap,
                VideoSpec {
                    width: 320,
                    height: 240,
                    frame_rate: 25.0,
                },
            )),
        );
        output.connect().expect("dir opens");
        let handle = std::thread::spawn(move || output.run());
        // Audio paced at real time (the drain clock) while the video
        // producer publishes at source rate, so the tap never overflows.
        let audio = std::thread::spawn(move || {
            paced_sine_frames(&tx, 1.2);
            drop(tx);
        });
        let producer = std::thread::spawn(move || {
            let mut decoder = crate::video::VideoDecoder::open(&clip).unwrap();
            let frames = decoder.decode_all().unwrap();
            for frame in &frames {
                pub_tap.publish(Arc::new(frame.clone()));
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        audio.join().expect("audio producer");
        producer.join().expect("video producer");
        handle.join().expect("hls thread").expect("clean finish");

        // Master: per-rendition RESOLUTION + avc CODECS + combined bandwidth.
        let master = fs::read_to_string(dir.join(MASTER)).unwrap();
        assert!(master.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=620400,RESOLUTION=320x180,\
             CODECS=\"avc1.42401f,mp4a.40.2\",NAME=\"360p\""
        ));
        assert!(master.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=1240800,RESOLUTION=320x240,\
             CODECS=\"avc1.42401f,mp4a.40.2\",NAME=\"src\""
        ));
        // 360p: BANDWIDTH = (64000 + 500000) * 1.1 = 620400; src:
        // (128000 + 1000000) * 1.1 = 1240800.
        assert!(master.contains("360p/playlist.m3u8"));
        assert!(master.contains("src/playlist.m3u8"));

        // Each rendition has h264+aac segments; the scaled one decodes at
        // 320x180, the passthrough one at the source 320x240.
        let probe = |path: &std::path::Path| -> String {
            let out = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_type,codec_name,width,height",
                    "-of",
                    "csv=p=0",
                ])
                .arg(path)
                .output()
                .expect("run ffprobe");
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        for (name, res) in [("360p", "320,180"), ("src", "320,240")] {
            let sub = dir.join(name);
            let mut segments: Vec<_> = fs::read_dir(&sub)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().ends_with(".ts"))
                .collect();
            assert!(
                segments.len() >= 2,
                "{name}: expected segments, got {}",
                segments.len()
            );
            segments.sort_by_key(|e| e.file_name());
            let out = probe(&segments[0].path());
            assert!(
                out.contains("h264,video") && out.contains("aac,audio"),
                "{name} segment not A/V: {out}"
            );
            assert!(
                out.contains(res),
                "{name} segment must decode at {res}: {out}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "video")]
    #[test]
    fn interleaves_video_into_segments() {
        use crate::video::{VideoSpec, testutil::render_test_clip};

        let Some(clip) = render_test_clip("hls") else {
            return;
        };
        let dir = std::env::temp_dir().join("crabsoup-hls-video");
        let _ = fs::remove_dir_all(&dir);
        let cfg = HlsOutputConfig {
            directory: dir.clone(),
            segment_seconds: 0.5,
            retention: 4,
            video: true,
            renditions: Vec::new(),
            segment_name: "seg-{n}.ts".into(),
            persist_at: None,
            fallible: false,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let tap = Arc::new(VideoTap::new());
        let pub_tap = tap.clone();

        let mut output = HlsOutput::new(
            cfg,
            rx,
            44_100,
            1,
            Some((
                tap,
                VideoSpec {
                    width: 320,
                    height: 240,
                    frame_rate: 25.0,
                },
            )),
        );
        output.connect().expect("dir opens");

        let handle = std::thread::spawn(move || output.run());
        // Audio paced at real time (the drain clock) while the video
        // producer publishes at source rate, so the tap never overflows.
        let audio = std::thread::spawn(move || {
            paced_sine_frames(&tx, 1.2);
            drop(tx);
        });
        let producer = std::thread::spawn(move || {
            let mut decoder = crate::video::VideoDecoder::open(&clip).unwrap();
            let frames = decoder.decode_all().unwrap();
            for frame in &frames {
                pub_tap.publish(Arc::new(frame.clone()));
                std::thread::sleep(Duration::from_millis(40));
            }
        });
        audio.join().expect("audio producer");
        producer.join().expect("video producer");
        handle.join().expect("hls thread").expect("clean finish");

        // Every segment starts with an IDR picture (rotation waits for a
        // forced keyframe), so ffprobe tags them all h264+aac and players
        // can join mid-stream.
        let mut segments: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".ts"))
            .collect();
        assert!(segments.len() >= 2, "got {}", segments.len());
        segments.sort_by_key(|e| e.file_name());
        let probe = |path: &std::path::Path| -> String {
            let out = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-show_entries",
                    "stream=codec_type,codec_name",
                    "-of",
                    "csv=p=0",
                ])
                .arg(path)
                .output()
                .expect("run ffprobe");
            String::from_utf8_lossy(&out.stdout).to_string()
        };
        for seg in &segments {
            let out = probe(&seg.path());
            assert!(
                out.contains("h264,video") && out.contains("aac,audio"),
                "segment {} not keyframe-aligned A/V: {out}",
                seg.file_name().to_string_lossy()
            );
        }
        let master = fs::read_to_string(dir.join(MASTER)).unwrap();
        assert!(master.contains("#EXT-X-STREAM-INF:BANDWIDTH=1628000,RESOLUTION=320x240"));
        assert!(master.trim_end().ends_with(PLAYLIST));
        let _ = fs::remove_dir_all(&dir);
    }
}
