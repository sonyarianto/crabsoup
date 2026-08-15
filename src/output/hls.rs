//! Live HLS output via the engine tap.
//!
//! Encodes the bus to AAC/ADTS and slices it into a sliding window of
//! MPEG-TS segments (`seg-000000.ts`, ...) plus a media playlist
//! (`playlist.m3u8`). No pacing of its own — the tap paces the stream, like
//! `FileOutput`. With video (Part H6), an optional `VideoTrack` encodes
//! frames from the shared video tap to H.264 and muxes them interleaved by
//! PTS. A web server must serve the directory; crabsoup only writes it.

use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;

use crate::Result;
use crate::config::HlsOutputConfig;
use crate::engine::tap::AudioFrame;
use crate::output::encoder::{AacEncoder, Encoder};
use crate::output::mpegts::{MpegTsMuxer, split_adts};
#[cfg(feature = "video")]
use crate::video::{VideoEncoder, VideoFrame, VideoSpec};

/// One AAC frame is 1024 samples per channel on the 90 kHz HLS clock.
const AAC_FRAME_SAMPLES: u64 = 1024;
const CLOCK: u64 = 90_000;
const PLAYLIST: &str = "playlist.m3u8";
/// Variant master playlist, written when video is enabled so clients can
/// point at `index.m3u8` and get the A/V stream.
const MASTER: &str = "index.m3u8";
/// Peak audio bitrate (128 kb/s AAC) plus video (1.5 Mb/s H.264).
#[cfg(feature = "video")]
const VARIANT_BANDWIDTH: u64 = 1_628_000;

/// What the video HLS path needs from the engine: the shared tap's
/// subscriber plus the track's stream spec. The unit type stands in on
/// non-video builds so the constructor signature stays uniform.
#[cfg(feature = "video")]
pub(crate) type HlsVideo = Option<(Receiver<Arc<VideoFrame>>, VideoSpec)>;
#[cfg(not(feature = "video"))]
pub(crate) type HlsVideo = ();

/// Consumes frames from the engine tap, encodes AAC, and rotates HLS
/// segments. The encoder is created and the directory prepared in
/// [`HlsOutput::connect`] so a bad path fails at startup.
pub struct HlsOutput {
    config: HlsOutputConfig,
    rx: Receiver<Arc<AudioFrame>>,
    sample_rate: u32,
    chans: usize,
    shutdown: Arc<AtomicBool>,
    /// Completed segments still in the window, as `(sequence, duration)`.
    closed: Vec<(u64, f64)>,
    video: HlsVideo,
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
            closed: Vec::new(),
            video,
        }
    }

    /// Give the output a shared flag that stops the consume loop (used for
    /// graceful Ctrl-C shutdown).
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = flag;
    }

    /// Prepare the output directory: create it if missing and clear stale
    /// segments/playlist from a previous run.
    pub fn connect(&mut self) -> Result<()> {
        fs::create_dir_all(&self.config.directory)
            .map_err(|e| format!("cannot create {}: {e}", self.config.directory.display()))?;
        for entry in fs::read_dir(&self.config.directory)
            .map_err(|e| format!("read {}: {e}", self.config.directory.display()))?
        {
            let Ok(entry) = entry else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == PLAYLIST
                || name == MASTER
                || (name.starts_with("seg-") && name.ends_with(".ts"))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
        #[cfg(feature = "video")]
        if let Some((_, spec)) = &self.video {
            write_master_playlist(&self.config.directory, spec)?;
        }
        log::info!("hls: segments to {}", self.config.directory.display());
        Ok(())
    }

    /// Consume frames until the stream ends (senders dropped) or shutdown is
    /// requested, then flush the encoder tail into the final segment and
    /// finalize the playlist with `#EXT-X-ENDLIST`.
    pub fn run(&mut self) -> Result<()> {
        let mut encoder = AacEncoder::new(self.sample_rate, self.chans as u16, 128_000)?;
        let has_video = self.video_present();
        #[cfg(feature = "video")]
        let frame_dur = (AAC_FRAME_SAMPLES * CLOCK) / self.sample_rate as u64;
        #[cfg(feature = "video")]
        let mut video = match self.video.take() {
            Some((rx, spec)) => Some(VideoTrack::new(rx, spec)?),
            None => None,
        };
        #[cfg(not(feature = "video"))]
        #[cfg_attr(not(feature = "video"), allow(unused_variables, unused_mut))]
        let mut video: Option<()> = None;
        #[cfg_attr(not(feature = "video"), allow(unused_variables))]
        let mut seg = Segment::new(0, 0, has_video);
        let mut frames_total: u64 = 0;

        while let Ok(frame) = self.rx.recv() {
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("shutdown requested, ending hls output");
                break;
            }
            #[cfg(feature = "video")]
            let audio_pts = frames_total.wrapping_mul(frame_dur);
            #[cfg(feature = "video")]
            let defer = video.as_ref().is_some_and(|v| v.alive());
            #[cfg(not(feature = "video"))]
            let defer = false;
            #[cfg(feature = "video")]
            if let Some(v) = &mut video {
                v.flush_up_to(audio_pts, &mut seg, &mut |s, pts| self.rotate(s, pts))?;
            }
            let adts = encoder.encode(&frame.pcm);
            frames_total = self.feed(&adts, &mut seg, frames_total, defer)?;
        }
        #[cfg(feature = "video")]
        if let Some(v) = &mut video {
            v.flush_remaining(&mut seg)?;
        }
        let tail = encoder.finish();
        // The stream is ending; no more rotations can be deferred.
        frames_total = self.feed(&tail, &mut seg, frames_total, false)?;
        let _ = frames_total;

        self.close_segment(&mut seg)?;
        self.write_playlist(true)?;
        log::info!(
            "hls closed: {} segments in {}",
            self.closed.len(),
            self.config.directory.display()
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

    /// Route ADTS frames into segments, closing a segment once its window
    /// crosses `segment_seconds`. When `defer` is set (video is live), a
    /// window crossing only marks `window_reached` — the actual rotation
    /// waits for the video track to mux a keyframe into the next segment.
    fn feed(
        &mut self,
        adts: &[u8],
        seg: &mut Segment,
        frames_total: u64,
        defer: bool,
    ) -> Result<u64> {
        if adts.is_empty() {
            return Ok(frames_total);
        }
        let mut count = frames_total;
        let frame_dur = (AAC_FRAME_SAMPLES * CLOCK) / self.sample_rate as u64;
        let window = (self.config.segment_seconds * CLOCK as f64) as u64;
        for frame in split_adts(adts) {
            let pts = count.wrapping_mul(frame_dur);
            if seg.frames > 0 && pts.wrapping_sub(seg.start_pts) >= window {
                if !seg.window_reached {
                    if defer {
                        seg.window_reached = true;
                    } else {
                        self.rotate(seg, pts)?;
                    }
                } else if !defer || pts.wrapping_sub(seg.start_pts) >= 2 * window {
                    // No keyframe came: the video tap died (defer off) or is
                    // stalled past one whole extra window. Rotate anyway
                    // rather than grow the segment forever.
                    self.rotate(seg, pts)?;
                }
            }
            seg.mux.push_audio(frame, pts, &mut seg.bytes);
            seg.end_pts = pts + frame_dur;
            seg.frames += 1;
            count += 1;
        }
        Ok(count)
    }

    /// Close `seg` and open the next one at `next_start_pts`. Called from
    /// both the audio feed (immediate rotation) and the video track (once
    /// it holds a keyframe to start the next segment).
    fn rotate(&mut self, seg: &mut Segment, next_start_pts: u64) -> Result<()> {
        let has_video = seg.mux.has_video();
        self.close_segment(seg)?;
        *seg = Segment::new(seg.seq + 1, next_start_pts, has_video);
        Ok(())
    }

    /// Write the finished segment to disk, trim the window to `retention`
    /// segments, and rewrite the live playlist.
    fn close_segment(&mut self, seg: &mut Segment) -> Result<()> {
        if seg.frames == 0 {
            return Ok(());
        }
        let duration = (seg.end_pts - seg.start_pts) as f64 / CLOCK as f64;
        let path = self.config.directory.join(segment_name(seg.seq));
        fs::write(&path, &seg.bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
        self.closed.push((seg.seq, duration));
        log::info!("hls segment seg-{:06}.ts closed ({duration:.2}s)", seg.seq);

        // Retention window: drop segments older than the last `retention`.
        let keep_below = self.closed.last().map(|(seq, _)| *seq).unwrap_or(0);
        let drop_below = keep_below.saturating_sub(self.config.retention as u64);
        let trimmed: Vec<u64> = self
            .closed
            .iter()
            .filter(|(seq, _)| *seq < drop_below)
            .map(|(seq, _)| *seq)
            .collect();
        self.closed.retain(|(seq, _)| *seq >= drop_below);
        for seq in trimmed {
            let _ = fs::remove_file(self.config.directory.join(segment_name(seq)));
        }
        self.write_playlist(false)
    }

    /// Rewrite `playlist.m3u8` describing the retained window (or with
    /// `#EXT-X-ENDLIST` once the stream is over).
    fn write_playlist(&mut self, finalized: bool) -> Result<()> {
        let target = self
            .closed
            .iter()
            .map(|(_, d)| d.ceil() as u64)
            .max()
            .unwrap_or(1)
            .max(1);
        let first = self.closed.first().map(|(seq, _)| *seq).unwrap_or(0);
        let mut out = String::new();
        out.push_str("#EXTM3U\n");
        out.push_str("#EXT-X-VERSION:3\n");
        out.push_str(&format!("#EXT-X-TARGETDURATION:{target}\n"));
        out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{first}\n"));
        for (seq, duration) in &self.closed {
            out.push_str(&format!("#EXTINF:{duration:.3},\n"));
            out.push_str(&format!("{}\n", segment_name(*seq)));
        }
        if finalized {
            out.push_str("#EXT-X-ENDLIST\n");
        }
        let path = self.config.directory.join(PLAYLIST);
        fs::write(&path, out).map_err(|e| format!("write {}: {e}", path.display()))?;
        Ok(())
    }
}

fn segment_name(seq: u64) -> String {
    format!("seg-{seq:06}.ts")
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
}

#[cfg(feature = "video")]
impl VideoTrack {
    fn new(rx: Receiver<Arc<VideoFrame>>, spec: VideoSpec) -> Result<Self> {
        let fps = spec.frame_rate.max(1.0);
        let encoder = VideoEncoder::h264(
            spec.width,
            spec.height,
            (fps * 1000.0) as i32,
            1000,
            1_500_000,
        )?;
        Ok(Self {
            rx,
            encoder,
            frame_dur_90k: (CLOCK as f64 / fps) as u64,
            next: None,
            pending: Vec::new(),
            eof: false,
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
            for au in self.encoder.push(&frame)? {
                self.pending.push(au);
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
            for au in self.encoder.push(&frame)? {
                self.pending.push(au);
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
    use std::sync::mpsc;

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

    #[test]
    fn writes_windowed_segments_and_playlist() {
        let dir = std::env::temp_dir().join("crabsoup-hls-test");
        let _ = fs::remove_dir_all(&dir);
        let cfg = HlsOutputConfig {
            directory: dir.clone(),
            segment_seconds: 1.0,
            retention: 4,
            video: false,
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
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let (vtx, vrx) = mpsc::sync_channel(32);
        // "frames sent" gate: the audio side runs at full speed, so make
        // sure every video frame is queued before the first audio frame.
        let (gate_tx, gate_rx) = mpsc::sync_channel::<()>(1);

        let producer = std::thread::spawn(move || {
            let mut decoder = crate::video::VideoDecoder::open(&clip).unwrap();
            let frames = decoder.decode_all().unwrap();
            for frame in &frames {
                vtx.send(Arc::new(frame.clone())).unwrap();
            }
            gate_tx.send(()).unwrap();
        });

        let mut output = HlsOutput::new(
            cfg,
            rx,
            44_100,
            1,
            Some((
                vrx,
                VideoSpec {
                    width: 320,
                    height: 240,
                    frame_rate: 25.0,
                },
            )),
        );
        output.connect().expect("dir opens");

        let handle = std::thread::spawn(move || output.run());
        // 0.8 s of audio, 1.0 s of video: every full segment overlaps video.
        gate_rx.recv().expect("video queued");
        sine_frames(&tx, 0.8);
        drop(tx);
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
