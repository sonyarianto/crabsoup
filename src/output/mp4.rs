//! MP4 file output (Part H4): mux the tap into a `.mp4` recording.
//!
//! The audio is encoded with FDK-AAC on the raw transport (raw access
//! units, no ADTS) and, when a video marker is given, the shared video tap
//! with the H.264 encoder; ffmpeg-next's `mov` muxer interleaves them by
//! PTS into a seekable MP4. The only new FFmpeg surface is the container:
//! stream codecpars carry the AudioSpecificConfig (AAC) and the avcC
//! record (H.264), and packets are fed length-prefixed/raw exactly as
//! movenc writes them (`ff_isom_write_avcc`). The few setters ffmpeg-next
//! lacks (codec id, codecpar extradata) are a tiny FFI shim into the
//! `AVCodecContext`/`AVCodecParameters` structs — the crate rule reserves
//! `unsafe` for FFI.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, TryRecvError};

use ffmpeg::codec::packet::Flags;
use ffmpeg::ffi;
use ffmpeg_next as ffmpeg;

use crate::Result;
use crate::config::Mp4OutputConfig;
use crate::engine::tap::{AudioFrame, recv_frame_or_shutdown};
use crate::output::encoder::AacEncoder;
use crate::output::flv;
use crate::video::{EncodedAu, VideoEncoder, VideoFrame, VideoSpec};

/// The shared 90 kHz clock the video encoder timestamps on.
const CLOCK: u64 = 90_000;

/// Consumes frames from the engine tap, encodes AAC + H.264, and muxes them
/// into an MP4 file. The file is opened in [`Mp4Output::connect`] so a bad
/// path fails at startup, mirroring `FileOutput`.
pub struct Mp4Output {
    config: Mp4OutputConfig,
    rx: Receiver<Arc<AudioFrame>>,
    sample_rate: u32,
    chans: usize,
    video: Option<(Receiver<Arc<VideoFrame>>, VideoSpec)>,
    shutdown: Arc<AtomicBool>,
    muxer: Option<Muxer>,
}

impl Mp4Output {
    pub fn new(
        config: Mp4OutputConfig,
        rx: Receiver<Arc<AudioFrame>>,
        sample_rate: u32,
        chans: usize,
        video: Option<(Receiver<Arc<VideoFrame>>, VideoSpec)>,
    ) -> Self {
        Self {
            config,
            rx,
            sample_rate,
            chans,
            video,
            shutdown: Arc::new(AtomicBool::new(false)),
            muxer: None,
        }
    }

    /// Give the output a shared flag that stops the consume loop (used for
    /// graceful Ctrl-C shutdown).
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = flag;
    }

    /// Open the file, prepare the streams, and write the header. Audio-only
    /// recordings finish the header immediately; video recordings wait for
    /// the H.264 parameter sets (which only exist once the tap produces).
    pub fn connect(&mut self) -> Result<()> {
        let muxer = Muxer::open(
            &self.config,
            self.sample_rate,
            self.chans,
            self.video.take(),
        )?;
        self.muxer = Some(muxer);
        log::info!("mp4: recording to {}", self.config.path.display());
        Ok(())
    }

    /// Consume frames until the stream ends (senders dropped) or shutdown is
    /// requested, then drain both encoders and finalize the file with its
    /// trailer (the moov atom).
    pub fn run(&mut self) -> Result<()> {
        let mut muxer = self.muxer.take().ok_or("output.mp4 not connected")?;
        while let Some(frame) = recv_frame_or_shutdown(&self.rx, &self.shutdown) {
            muxer.push_audio(&frame.pcm)?;
        }
        muxer.finish()?;
        log::info!(
            "mp4 closed: {} ({} audio frames)",
            self.config.path.display(),
            muxer.audio_frames
        );
        Ok(())
    }
}

/// The container half of the output: the ffmpeg muxer plus the two
/// encoders. One thread owns it, so the raw `Output` handle is never
/// shared.
struct Muxer {
    output: ffmpeg::format::context::Output,
    audio_index: usize,
    video_index: Option<usize>,
    encoder: AacEncoder,
    /// Sample rate of the audio stream.
    sample_rate: u32,
    /// Total AAC frames encoded; each is `frame_size` samples, the PTS
    /// clock in the stream's `1/sample_rate` time base.
    audio_frames: u64,
    video: Option<VideoTrack>,
    /// The avcC record from the first H.264 access unit, awaited before the
    /// header can go out.
    video_avcc: Option<Vec<u8>>,
    /// Audio access units waiting for the deferred header.
    pending_audio: Vec<(i64, Vec<u8>)>,
    started: bool,
}

impl Muxer {
    fn open(
        config: &Mp4OutputConfig,
        sample_rate: u32,
        chans: usize,
        video: Option<(Receiver<Arc<VideoFrame>>, VideoSpec)>,
    ) -> Result<Self> {
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;
        let mut output = ffmpeg::format::output(&config.path)
            .map_err(|e| format!("mp4: open {}: {e}", config.path.display()))?;
        let encoder = AacEncoder::new_raw(sample_rate, chans as u16, config.bitrate)?;
        let asc = encoder.audio_specific_config().to_vec();
        let audio_index = add_audio_stream(&mut output, sample_rate, chans, config.bitrate, &asc)?;
        let (video_index, vtrack) = match video {
            Some((rx, spec)) => {
                let index = add_video_stream(&mut output, &spec)?;
                (Some(index), Some(VideoTrack::new(rx, spec)?))
            }
            None => (None, None),
        };
        let mut muxer = Self {
            output,
            audio_index,
            video_index,
            encoder,
            sample_rate,
            audio_frames: 0,
            video: vtrack,
            video_avcc: None,
            pending_audio: Vec::new(),
            started: false,
        };
        // Audio-only files need no deferral: the header can go out now.
        if video_index.is_none() {
            muxer.start_header()?;
        }
        Ok(muxer)
    }

    fn audio_pts_90k(&self) -> u64 {
        self.audio_frames * self.encoder.frame_size as u64 * CLOCK / self.sample_rate as u64
    }

    fn push_audio(&mut self, pcm: &[f32]) -> Result<()> {
        let aus = self.encoder.encode_aus(pcm);
        for au in aus {
            let pts = (self.audio_frames * self.encoder.frame_size as u64) as i64;
            self.write_audio(pts, &au)?;
            self.audio_frames += 1;
        }
        self.flush_video()
    }

    /// Write one raw AAC access unit. While the header waits on the H.264
    /// parameter sets, units are parked in `pending_audio` so the file still
    /// begins at t=0 when it is unblocked.
    fn write_audio(&mut self, pts: i64, au: &[u8]) -> Result<()> {
        if !self.started {
            if self.video_index.is_some() && self.video_avcc.is_none() {
                self.pending_audio.push((pts, au.to_vec()));
                return Ok(());
            }
            self.start_header()?;
        }
        self.write_audio_packet(pts, au)
    }

    fn write_audio_packet(&mut self, pts: i64, au: &[u8]) -> Result<()> {
        let mut packet = ffmpeg::packet::Packet::copy(au);
        packet.set_stream(self.audio_index);
        packet.set_pts(Some(pts));
        packet.set_dts(Some(pts));
        packet.set_flags(Flags::KEY);
        packet
            .write_interleaved(&mut self.output)
            .map_err(|e| format!("mp4: write audio: {e}"))?;
        Ok(())
    }

    /// Drain the video tap, encode, and mux every access unit the audio
    /// clock has caught up with. Periodic forced keyframes keep a long
    /// recording seekable.
    fn flush_video(&mut self) -> Result<()> {
        let limit = self.audio_pts_90k();
        let Some(v) = &mut self.video else {
            return Ok(());
        };
        loop {
            let frame = match v.rx.try_recv() {
                Ok(f) => f,
                Err(TryRecvError::Disconnected) => {
                    v.eof = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            };
            if v.frames_encoded > 0 && v.frames_encoded % v.keyframe_every == 0 {
                v.encoder.force_keyframe();
            }
            v.frames_encoded += 1;
            for au in v.encoder.push(&frame)? {
                v.pending.push(au);
            }
        }
        let mut ready = Vec::new();
        v.pending.retain(|au| {
            if au.pts_90k <= limit {
                ready.push(EncodedAu {
                    pts_90k: au.pts_90k,
                    data: au.data.clone(),
                });
                false
            } else {
                true
            }
        });
        for au in ready {
            self.write_video(&au)?;
        }
        Ok(())
    }

    /// Mux one access unit as a length-prefixed sample. The first unit
    /// carries SPS/PPS, which builds the avcC and unblocks the header.
    fn write_video(&mut self, au: &EncodedAu) -> Result<()> {
        let Some(index) = self.video_index else {
            return Ok(());
        };
        if !self.started {
            if self.video_avcc.is_none()
                && let Some((sps, pps)) = flv::parameter_sets(&au.data)
            {
                self.video_avcc = Some(flv::avcdcr(&sps, &pps));
            }
            self.start_header()?;
        }
        let mut packet = ffmpeg::packet::Packet::copy(&flv::avcc_nalus(&au.data));
        packet.set_stream(index);
        packet.set_pts(Some(au.pts_90k as i64));
        packet.set_dts(Some(au.pts_90k as i64));
        if au.is_idr() {
            packet.set_flags(Flags::KEY);
        }
        packet
            .write_interleaved(&mut self.output)
            .map_err(|e| format!("mp4: write video: {e}"))?;
        Ok(())
    }

    /// Complete the container header: stamp the H.264 avcC onto the stream
    /// codecpar (via FFI — ffmpeg-next offers no safe setter), write the
    /// header, and release any parked audio.
    fn start_header(&mut self) -> Result<()> {
        if let Some(index) = self.video_index {
            if let Some(avcc) = self.video_avcc.take() {
                set_stream_extradata(&mut self.output, index, &avcc);
            } else {
                log::warn!("mp4: no H.264 parameter sets; video stream has no avcC");
            }
        }
        self.output
            .write_header()
            .map_err(|e| format!("mp4: write header: {e}"))?;
        self.started = true;
        for (pts, data) in std::mem::take(&mut self.pending_audio) {
            self.write_audio_packet(pts, &data)?;
        }
        Ok(())
    }

    /// End of stream: drain the video tap and encoders, then finalize with
    /// the trailer (the moov atom).
    fn finish(&mut self) -> Result<()> {
        if let Some(v) = &mut self.video {
            loop {
                match v.rx.try_recv() {
                    Ok(frame) => {
                        for au in v.encoder.push(&frame)? {
                            v.pending.push(au);
                        }
                    }
                    Err(TryRecvError::Disconnected) => {
                        v.eof = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            for au in v.encoder.finish()? {
                v.pending.push(au);
            }
            let mut tail = std::mem::take(&mut v.pending);
            for au in tail.drain(..) {
                self.write_video(&au)?;
            }
        }
        if !self.started {
            self.start_header()?;
        }
        for au in self.encoder.finish_aus() {
            let pts = (self.audio_frames * self.encoder.frame_size as u64) as i64;
            self.write_audio_packet(pts, &au)?;
            self.audio_frames += 1;
        }
        self.output
            .write_trailer()
            .map_err(|e| format!("mp4: write trailer: {e}"))?;
        Ok(())
    }
}

/// The H.264 half of the pipeline: drains the video tap and holds access
/// units until the audio clock catches up.
struct VideoTrack {
    rx: Receiver<Arc<VideoFrame>>,
    encoder: VideoEncoder,
    /// Nominal frame duration on the 90 kHz clock (not written; used for
    /// nothing yet but documents the track cadence).
    #[allow(dead_code)]
    frame_dur_90k: u64,
    pending: Vec<EncodedAu>,
    eof: bool,
    frames_encoded: u64,
    /// Force an IDR every `keyframe_every` pictures (~2 s).
    keyframe_every: u64,
}

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
            pending: Vec::new(),
            eof: false,
            frames_encoded: 0,
            keyframe_every: (fps * 2.0).max(1.0) as u64,
        })
    }
}

// ---------------------------------------------------------------------------
// Codec parameter shims (FFI)
//
// ffmpeg-next's safe API exposes no setters for the codec id or for
// codecpar extradata, both of which the mov muxer needs (avcC / ASC). These
// are the crate's sanctioned FFI surface: the buffers are `av_malloc`'d so
// libav's own free paths reclaim them.
// ---------------------------------------------------------------------------

fn set_codec_id(ctx: &mut ffmpeg::codec::context::Context, id: ffi::AVCodecID) {
    unsafe {
        (*ctx.as_mut_ptr()).codec_id = id;
    }
}

/// Give a codec context `data` as extradata. `avcodec_parameters_from_context`
/// copies it into the stream codecpar when the stream is added.
fn set_extradata(ctx: &mut ffmpeg::codec::context::Context, data: &[u8]) {
    unsafe {
        let raw = ctx.as_mut_ptr();
        ffi::av_free((*raw).extradata as *mut _);
        let buf = ffi::av_malloc(data.len()) as *mut u8;
        if buf.is_null() {
            log::error!("mp4: av_malloc failed for extradata");
            return;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
        (*raw).extradata = buf;
        (*raw).extradata_size = data.len() as std::os::raw::c_int;
    }
}

/// Stamp a stream's codecpar extradata after the stream already exists —
/// used for the H.264 avcC, which only materializes with the first access
/// unit, before the header is written.
fn set_stream_extradata(output: &mut ffmpeg::format::context::Output, index: usize, data: &[u8]) {
    let Some(mut stream) = output.stream_mut(index) else {
        return;
    };
    unsafe {
        let par = (*stream.as_mut_ptr()).codecpar;
        if par.is_null() {
            return;
        }
        ffi::av_free((*par).extradata as *mut _);
        let buf = ffi::av_malloc(data.len()) as *mut u8;
        if buf.is_null() {
            log::error!("mp4: av_malloc failed for stream extradata");
            return;
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), buf, data.len());
        (*par).extradata = buf;
        (*par).extradata_size = data.len() as std::os::raw::c_int;
    }
}

/// Add the AAC stream: a codec context describing the FDK-AAC encoder's
/// output, stamped with the AudioSpecificConfig so movenc writes a valid
/// `esds` box.
fn add_audio_stream(
    output: &mut ffmpeg::format::context::Output,
    sample_rate: u32,
    chans: usize,
    bitrate: u32,
    asc: &[u8],
) -> Result<usize> {
    let ctx = ffmpeg::codec::context::Context::new();
    let mut audio = ctx
        .encoder()
        .audio()
        .map_err(|e| format!("mp4: audio codec type: {e}"))?;
    audio.set_rate(sample_rate as i32);
    let layout = if chans == 1 {
        ffmpeg::ChannelLayout::MONO
    } else {
        ffmpeg::ChannelLayout::STEREO
    };
    audio.set_channel_layout(layout);
    audio.set_bit_rate(bitrate as usize);
    set_codec_id(audio.as_mut(), ffi::AVCodecID::AV_CODEC_ID_AAC);
    set_extradata(audio.as_mut(), asc);
    let stream = output
        .add_stream_with(audio.as_ref())
        .map_err(|e| format!("mp4: add audio stream: {e}"))?;
    let index = stream.index();
    output
        .stream_mut(index)
        .expect("stream just added")
        .set_time_base(ffmpeg::Rational(1, sample_rate as i32));
    Ok(index)
}

/// Add the H.264 stream without extradata: the avcC needs SPS/PPS from the
/// first access unit, which only arrives during the run.
fn add_video_stream(
    output: &mut ffmpeg::format::context::Output,
    spec: &VideoSpec,
) -> Result<usize> {
    let ctx = ffmpeg::codec::context::Context::new();
    let mut video = ctx
        .encoder()
        .video()
        .map_err(|e| format!("mp4: video codec type: {e}"))?;
    video.set_width(spec.width);
    video.set_height(spec.height);
    video.set_format(ffmpeg::util::format::Pixel::YUV420P);
    set_codec_id(video.as_mut(), ffi::AVCodecID::AV_CODEC_ID_H264);
    let stream = output
        .add_stream_with(video.as_ref())
        .map_err(|e| format!("mp4: add video stream: {e}"))?;
    let index = stream.index();
    output
        .stream_mut(index)
        .expect("stream just added")
        .set_time_base(ffmpeg::Rational(1, CLOCK as i32));
    Ok(index)
}

#[cfg(test)]
mod tests {
    // mp4.rs is video-gated, so the video subscription is always real here.
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

    fn probe(path: &std::path::Path) -> String {
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
    }

    #[test]
    fn records_audio_only_mp4() {
        let path =
            std::env::temp_dir().join(format!("crabsoup-mp4-audio-{}.mp4", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Mp4OutputConfig {
            path: path.clone(),
            bitrate: 128_000,
            video: false,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = Mp4Output::new(cfg, rx, 44_100, 1, None);
        output.connect().expect("file opens");

        let handle = std::thread::spawn(move || output.run());
        sine_frames(&tx, 0.5);
        drop(tx);
        handle.join().expect("mp4 thread").expect("clean finish");

        assert!(path.exists(), "recording was not written");
        let out = probe(&path);
        assert!(
            out.contains("aac,audio"),
            "audio-only mp4 should carry aac audio: {out}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn records_av_mp4() {
        use crate::video::testutil::render_test_clip;

        let Some(clip) = render_test_clip("mp4") else {
            return;
        };
        let path = std::env::temp_dir().join(format!("crabsoup-mp4-av-{}.mp4", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let cfg = Mp4OutputConfig {
            path: path.clone(),
            bitrate: 128_000,
            video: true,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let (vtx, vrx) = mpsc::sync_channel(32);
        // Gate so every video frame is queued before the first audio frame,
        // mirroring the HLS interleave test.
        let (gate_tx, gate_rx) = mpsc::sync_channel::<()>(1);

        let producer = std::thread::spawn(move || {
            let mut decoder = crate::video::VideoDecoder::open(&clip).unwrap();
            let frames = decoder.decode_all().unwrap();
            for frame in &frames {
                vtx.send(Arc::new(frame.clone())).unwrap();
            }
            gate_tx.send(()).unwrap();
        });

        let mut output = Mp4Output::new(
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
        output.connect().expect("file opens");

        let handle = std::thread::spawn(move || output.run());
        gate_rx.recv().expect("video queued");
        sine_frames(&tx, 0.8);
        drop(tx);
        producer.join().expect("video producer");
        handle.join().expect("mp4 thread").expect("clean finish");

        assert!(path.exists(), "recording was not written");
        let out = probe(&path);
        assert!(
            out.contains("h264,video") && out.contains("aac,audio"),
            "A/V mp4 should carry h264 video and aac audio: {out}"
        );
        let _ = std::fs::remove_file(&path);
    }
}
