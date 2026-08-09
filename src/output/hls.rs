//! Live HLS output via the engine tap.
//!
//! Encodes the bus to AAC/ADTS and slices it into a sliding window of
//! MPEG-TS segments (`seg-000000.ts`, ...) plus a media playlist
//! (`playlist.m3u8`). No pacing of its own — the tap paces the stream, like
//! `FileOutput`. A web server must serve the directory; crabsoup only
//! writes it.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::config::HlsOutputConfig;
use crate::engine::tap::AudioFrame;
use crate::output::encoder::{AacEncoder, Encoder};
use crate::output::mpegts::{MpegTsMuxer, split_adts};
use crate::Result;

/// One AAC frame is 1024 samples per channel on the 90 kHz HLS clock.
const AAC_FRAME_SAMPLES: u64 = 1024;
const CLOCK: u64 = 90_000;
const PLAYLIST: &str = "playlist.m3u8";

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
}

impl Segment {
    fn new(seq: u64, start_pts: u64) -> Self {
        let mut mux = MpegTsMuxer::new();
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
        }
    }
}

impl HlsOutput {
    pub fn new(
        config: HlsOutputConfig,
        rx: Receiver<Arc<AudioFrame>>,
        sample_rate: u32,
        chans: usize,
    ) -> Self {
        Self {
            config,
            rx,
            sample_rate,
            chans,
            shutdown: Arc::new(AtomicBool::new(false)),
            closed: Vec::new(),
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
            if name == PLAYLIST || (name.starts_with("seg-") && name.ends_with(".ts")) {
                let _ = fs::remove_file(entry.path());
            }
        }
        log::info!("hls: segments to {}", self.config.directory.display());
        Ok(())
    }

    /// Consume frames until the stream ends (senders dropped) or shutdown is
    /// requested, then flush the encoder tail into the final segment and
    /// finalize the playlist with `#EXT-X-ENDLIST`.
    pub fn run(&mut self) -> Result<()> {
        let mut encoder = AacEncoder::new(self.sample_rate, self.chans as u16, 128_000)?;
        let mut seg = Segment::new(0, 0);
        let mut frames_total: u64 = 0;

        while let Ok(frame) = self.rx.recv() {
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("shutdown requested, ending hls output");
                break;
            }
            let adts = encoder.encode(&frame.pcm);
            frames_total = self.feed(&adts, &mut seg, frames_total)?;
        }
        let tail = encoder.finish();
        frames_total = self.feed(&tail, &mut seg, frames_total)?;
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

    /// Route ADTS frames into segments, closing a segment once its window
    /// crosses `segment_seconds`.
    fn feed(
        &mut self,
        adts: &[u8],
        seg: &mut Segment,
        frames_total: u64,
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
                self.close_segment(seg)?;
                *seg = Segment::new(seg.seq + 1, pts);
            }
            seg.mux.push_audio(frame, pts, &mut seg.bytes);
            seg.end_pts = pts + frame_dur;
            seg.frames += 1;
            count += 1;
        }
        Ok(count)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = HlsOutput::new(cfg, rx, 44_100, 1);
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
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = HlsOutput::new(cfg, rx, 44_100, 1);
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
        assert!(segments <= 4, "retention=2 should cap window, got {segments}");
        let _ = fs::remove_dir_all(&dir);
    }
}
