//! RTMP publishing (Part H5): FLV bytes over librtmp.
//!
//! Mirrors the Icecast output: a pure consumer of the engine tap. The
//! encoder runs on the raw transport (FLV carries raw AAC plus the
//! AudioSpecificConfig in a sequence header), and the FLV framing lives in
//! `super::flv` so the byte stream is testable without a server. Video
//! (Part H) subscribes to the shared video tap and interleaves by PTS, the
//! same model as the HLS segmenter. The `unsafe` surface is the thin
//! librtmp binding below — everything else is safe Rust.

use std::ffi::{c_char, c_int};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
#[cfg(all(feature = "rtmp", feature = "video"))]
use std::sync::mpsc::TryRecvError;
use std::time::Duration;

use crate::Result;
use crate::config::RtmpOutputConfig;
use crate::engine::tap::AudioFrame;
use crate::output::encoder::AacEncoder;
use crate::output::flv;

// librtmp FFI (rtmp.h), same shape as the LAME/FDK bindings in encoder.rs.
// RTMP is a handle passed to every call; never dereferenced here.

#[repr(C)]
struct RtmpRaw {
    _unused: [u8; 1],
}

#[link(name = "rtmp")]
unsafe extern "C" {
    fn RTMP_Alloc() -> *mut RtmpRaw;
    fn RTMP_Init(r: *mut RtmpRaw);
    fn RTMP_SetupURL(r: *mut RtmpRaw, url: *const c_char) -> c_int;
    fn RTMP_Connect(r: *mut RtmpRaw, ctx: *mut core::ffi::c_void) -> c_int;
    fn RTMP_ConnectStream(r: *mut RtmpRaw, seek_time: c_int) -> c_int;
    fn RTMP_EnableWrite(r: *mut RtmpRaw);
    fn RTMP_Write(r: *mut RtmpRaw, buf: *const c_char, size: c_int) -> c_int;
    fn RTMP_Close(r: *mut RtmpRaw);
    fn RTMP_Free(r: *mut RtmpRaw);
}

/// Anything the output can push bytes into: the real librtmp session in
/// production, a byte collector in tests.
pub trait RtmpSink: Send {
    fn write(&mut self, data: &[u8]) -> Result<()>;
}

/// An established librtmp publishing session. Ownership of the `RTMP`
/// handle never leaves this struct; `Drop` tears the connection down.
pub struct RtmpSession {
    rtmp: *mut RtmpRaw,
}

// The handle is only ever passed to C, never shared — Send is safe.
unsafe impl Send for RtmpSession {}

impl RtmpSession {
    /// Parse `url`, connect, open the publishing stream and switch to
    /// write mode. Returns a session that sends raw FLV bytes.
    pub fn connect(url: &str) -> Result<Self> {
        let rtmp = unsafe { RTMP_Alloc() };
        if rtmp.is_null() {
            return Err("RTMP_Alloc failed".into());
        }
        unsafe { RTMP_Init(rtmp) };
        let c_url = match std::ffi::CString::new(url) {
            Ok(u) => u,
            Err(_) => {
                unsafe { RTMP_Free(rtmp) };
                return Err(format!("RTMP url {url:?} contains a NUL byte").into());
            }
        };
        let fail = |msg: String| {
            unsafe { RTMP_Free(rtmp) };
            Err(msg.into())
        };
        if unsafe { RTMP_SetupURL(rtmp, c_url.as_ptr()) } == 0 {
            return fail(format!("RTMP_SetupURL failed for {url}"));
        }
        // librtmp gates the whole publish dance (ReleaseStream, FCPublish,
        // and the `publish` command itself) on the write flag — it must be
        // set before connecting, or the server never registers the stream
        // and quietly ignores every FLV message we send.
        unsafe { RTMP_EnableWrite(rtmp) };
        if unsafe { RTMP_Connect(rtmp, ptr::null_mut()) } == 0 {
            return fail(format!("RTMP_Connect failed for {url}"));
        }
        // FALSE (0) on failure — blocks until the publish status arrives.
        if unsafe { RTMP_ConnectStream(rtmp, 0) } == 0 {
            unsafe { RTMP_Close(rtmp) };
            return fail(format!("RTMP_ConnectStream failed for {url}"));
        }
        Ok(Self { rtmp })
    }
}

impl RtmpSink for RtmpSession {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        let written = unsafe {
            RTMP_Write(
                self.rtmp,
                data.as_ptr() as *const c_char,
                data.len() as c_int,
            )
        };
        if written != data.len() as c_int {
            return Err(format!("RTMP_Write: {written} of {} bytes", data.len()).into());
        }
        Ok(())
    }
}

impl Drop for RtmpSession {
    fn drop(&mut self) {
        if !self.rtmp.is_null() {
            unsafe { RTMP_Close(self.rtmp) };
            unsafe { RTMP_Free(self.rtmp) };
            self.rtmp = ptr::null_mut();
        }
    }
}

// The video half of the pipeline (Part H6 model, no segment rotation):
// drains the video tap, encodes to H.264, and holds access units until the
// audio clock catches up, then hands them to the FLV muxer in PTS order.

#[cfg(all(feature = "rtmp", feature = "video"))]
type RtmpVideo = Option<(
    Receiver<Arc<crate::video::VideoFrame>>,
    crate::video::VideoSpec,
)>;
#[cfg(not(feature = "video"))]
type RtmpVideo = ();

#[cfg(all(feature = "rtmp", feature = "video"))]
struct RtmpVideoTrack {
    rx: Receiver<Arc<crate::video::VideoFrame>>,
    encoder: crate::video::VideoEncoder,
    pending: Vec<crate::video::EncodedAu>,
    sent_seq: bool,
    eof: bool,
}

#[cfg(all(feature = "rtmp", feature = "video"))]
impl RtmpVideoTrack {
    fn new(
        rx: Receiver<Arc<crate::video::VideoFrame>>,
        spec: crate::video::VideoSpec,
    ) -> Result<Self> {
        let fps = spec.frame_rate.max(1.0);
        let encoder = crate::video::VideoEncoder::h264(
            spec.width,
            spec.height,
            (fps * 1000.0) as i32,
            1000,
            1_500_000,
        )?;
        Ok(Self {
            rx,
            encoder,
            pending: Vec::new(),
            sent_seq: false,
            eof: false,
        })
    }

    /// Drain the tap, encode, and emit every FLV tag whose PTS is at or
    /// before the current audio timestamp. The first access unit carries
    /// SPS/PPS, which the sequence header needs, so it is deferred until
    /// they arrive.
    fn flush(&mut self, audio_ts_ms: u64, out: &mut Vec<u8>) -> Result<()> {
        loop {
            let frame = match self.rx.try_recv() {
                Ok(f) => f,
                Err(TryRecvError::Disconnected) => {
                    self.eof = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            };
            for au in self.encoder.push(&frame)? {
                self.pending.push(au);
            }
        }
        let limit = audio_ts_ms;
        let mut ready = Vec::new();
        self.pending.retain(|au| {
            if au.pts_90k / 90 <= limit {
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
            let ts_ms = (au.pts_90k / 90) as u32;
            if !self.sent_seq
                && let Some((sps, pps)) = flv::parameter_sets(&au.data)
            {
                out.extend(flv::video_sequence_header(&flv::avcdcr(&sps, &pps)));
                self.sent_seq = true;
            }
            out.extend(flv::video_tag(
                ts_ms,
                au.is_idr(),
                &flv::avcc_nalus(&au.data),
            ));
        }
        Ok(())
    }
}

/// Consumes frames from the engine tap, encodes them to raw AAC + H.264,
/// muxes them into FLV, and pushes the bytes to an RTMP server with
/// automatic reconnection.
pub struct RtmpOutput {
    config: RtmpOutputConfig,
    rx: Receiver<Arc<AudioFrame>>,
    sample_rate: u32,
    chans: usize,
    video: RtmpVideo,
    sink: Option<Box<dyn RtmpSink>>,
    encoder: Option<AacEncoder>,
    shutdown: Arc<AtomicBool>,
}

impl RtmpOutput {
    #[cfg_attr(not(feature = "video"), allow(unused_variables))]
    pub fn new(
        config: RtmpOutputConfig,
        rx: Receiver<Arc<AudioFrame>>,
        sample_rate: u32,
        chans: usize,
        video: RtmpVideo,
    ) -> Self {
        Self {
            config,
            rx,
            sample_rate,
            chans,
            video,
            sink: None,
            encoder: None,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Give the output a shared flag that stops the pump loop (used for
    /// graceful Ctrl-C shutdown).
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = flag;
    }

    /// Seconds the reconnect loop waits between attempts.
    pub fn reconnect_seconds(&self) -> u64 {
        self.config.reconnect_seconds
    }

    /// Establish the initial connection (caller decides retry policy).
    pub fn connect(&mut self) -> Result<()> {
        self.encoder = Some(AacEncoder::new_raw(
            self.sample_rate,
            self.chans as u16,
            self.config.bitrate,
        )?);
        let session = RtmpSession::connect(&self.config.url)?;
        self.connect_to(Box::new(session))
    }

    /// Wire a sink in without touching the network: the test hook behind
    /// [`Self::connect`] (and the reconnect path).
    fn connect_to(&mut self, sink: Box<dyn RtmpSink>) -> Result<()> {
        if self.encoder.is_none() {
            self.encoder = Some(AacEncoder::new_raw(
                self.sample_rate,
                self.chans as u16,
                self.config.bitrate,
            )?);
        }
        self.sink = Some(sink);
        log::info!("connected to RTMP {}", self.config.url);
        Ok(())
    }

    /// Re-establish the connection, discarding the old encoder (fresh FLV
    /// headers) and the video track's state (fresh sequence header).
    fn reconnect(&mut self) {
        self.sink = None;
        self.encoder = None;
        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("shutdown requested during RTMP reconnect");
                return;
            }
            match self.connect() {
                Ok(()) => {
                    log::info!("reconnected to RTMP");
                    return;
                }
                Err(e) => {
                    log::error!(
                        "RTMP reconnect failed: {e}; retrying in {}s",
                        self.config.reconnect_seconds
                    );
                    std::thread::sleep(Duration::from_secs(self.config.reconnect_seconds));
                }
            }
        }
    }

    fn send_or_reconnect(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let Some(sink) = self.sink.as_mut() else {
            self.reconnect();
            return;
        };
        if let Err(e) = sink.write(data) {
            log::error!("RTMP send failed: {e}");
            self.reconnect();
        }
    }

    /// Consume frames from the tap until the stream ends (senders dropped)
    /// or shutdown is requested.
    pub fn run(&mut self) -> Result<()> {
        let has_video = self.video_present();
        let (v_w, v_h, v_fps) = self.video_spec();
        let mut out = Vec::new();
        out.extend(flv::header(has_video));
        out.extend(flv::metadata_tag(
            v_w,
            v_h,
            v_fps,
            has_video,
            self.config.bitrate,
        ));
        self.send_or_reconnect(&out);

        // The tap receiver and spec become a stateful track (encoder,
        // pending access units, sequence-header latch) for the run.
        #[cfg(all(feature = "rtmp", feature = "video"))]
        let mut vtrack = match self.video.take() {
            Some((rx, spec)) => Some(RtmpVideoTrack::new(rx, spec)?),
            None => None,
        };

        let mut audio_ts_ms: u64 = 0;
        let mut sent_audio_seq = false;
        while let Ok(frame) = self.rx.recv() {
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("shutdown requested, ending RTMP stream");
                break;
            }
            let encoder = self.encoder.as_mut().unwrap();
            let aus = encoder.encode_aus(&frame.pcm);
            if aus.is_empty() {
                continue;
            }
            let au_ms = (encoder.frame_size as u64 * 1000 / self.sample_rate as u64).max(1);
            out.clear();
            if !sent_audio_seq {
                out.extend(flv::audio_tag(0, true, encoder.audio_specific_config()));
                sent_audio_seq = true;
            }
            for au in aus {
                out.extend(flv::audio_tag(audio_ts_ms as u32, false, &au));
                audio_ts_ms += au_ms;
            }
            #[cfg(all(feature = "rtmp", feature = "video"))]
            if let Some(v) = &mut vtrack {
                v.flush(audio_ts_ms, &mut out)?;
            }
            self.send_or_reconnect(&out);
        }

        // Drain the video tail with no PTS bound, then finish cleanly.
        #[cfg(all(feature = "rtmp", feature = "video"))]
        if let Some(v) = &mut vtrack {
            out.clear();
            let mut spins = 0;
            while !v.eof && !self.shutdown.load(Ordering::SeqCst) && spins < 200 {
                v.flush(u64::MAX, &mut out)?;
                if !out.is_empty() {
                    self.send_or_reconnect(&out);
                    out.clear();
                }
                spins += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
            for au in v.encoder.finish()? {
                let ts_ms = (au.pts_90k / 90) as u32;
                if !v.sent_seq
                    && let Some((sps, pps)) = flv::parameter_sets(&au.data)
                {
                    out.extend(flv::video_sequence_header(&flv::avcdcr(&sps, &pps)));
                    v.sent_seq = true;
                }
                out.extend(flv::video_tag(
                    ts_ms,
                    au.is_idr(),
                    &flv::avcc_nalus(&au.data),
                ));
            }
            self.send_or_reconnect(&out);
        }
        self.sink = None;
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

    #[cfg(feature = "video")]
    fn video_spec(&self) -> (u32, u32, f64) {
        match &self.video {
            Some((_, spec)) => (spec.width, spec.height, spec.frame_rate),
            None => (0, 0, 0.0),
        }
    }

    #[cfg(not(feature = "video"))]
    fn video_spec(&self) -> (u32, u32, f64) {
        (0, 0, 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// Test sink: collects every byte the output would have sent.
    #[derive(Default)]
    struct CaptureSink {
        bytes: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl RtmpSink for CaptureSink {
        fn write(&mut self, data: &[u8]) -> Result<()> {
            self.bytes.lock().unwrap().extend_from_slice(data);
            Ok(())
        }
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

    fn config() -> RtmpOutputConfig {
        RtmpOutputConfig {
            url: "rtmp://localhost/test".into(),
            bitrate: 128_000,
            reconnect_seconds: 1,
            video: false,
        }
    }

    #[test]
    fn publishes_valid_flv_with_audio_only() {
        let (tx, rx) = mpsc::sync_channel(8);
        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        #[cfg(feature = "video")]
        let mut output = RtmpOutput::new(config(), rx, 44_100, 2, None);
        #[cfg(not(feature = "video"))]
        let mut output = RtmpOutput::new(config(), rx, 44_100, 2, ());
        output
            .connect_to(Box::new(CaptureSink {
                bytes: bytes.clone(),
            }))
            .expect("connect");
        let handle = std::thread::spawn(move || output.run());
        sine_frames(&tx, 0.2);
        drop(tx);
        handle.join().unwrap().expect("run finishes");

        let flv = bytes.lock().unwrap().clone();
        assert_eq!(&flv[..9], &[b'F', b'L', b'V', 0x01, 0x04, 0, 0, 0, 9]);
        // onMetaData script tag first, then an AAC sequence header, then
        // raw AAC access units.
        assert!(String::from_utf8_lossy(&flv).contains("onMetaData"));
        let mut i = 9 + 4; // past header + PreviousTagSize0
        let mut saw_seq = false;
        let mut tags = 0;
        while i + 11 < flv.len() {
            let kind = flv[i];
            let size = u32::from_be_bytes([0, flv[i + 1], flv[i + 2], flv[i + 3]]) as usize;
            assert!(i + 11 + size + 4 <= flv.len(), "tag out of bounds");
            if kind == flv::TAG_AUDIO {
                tags += 1;
                assert_eq!(flv[i + 11], flv::AAC_HEADER);
                if flv[i + 12] == 0 {
                    assert!(!saw_seq, "only one sequence header");
                    saw_seq = true;
                    assert_eq!(
                        flv[i + 13..i + 13 + size - 2],
                        [0x12, 0x10],
                        "AAC-LC AudioSpecificConfig"
                    );
                } else {
                    assert_eq!(flv[i + 12], 1, "raw access unit");
                }
            }
            i += 11 + size + 4;
        }
        assert!(saw_seq, "AAC sequence header must precede raw audio");
        assert!(tags >= 5, "one 0.2 s burst yields ~9 tags, got {tags}");
        // Timestamps must be monotonically non-decreasing.
        let mut last_ms = 0u32;
        let mut i = 9 + 4;
        while i + 11 < flv.len() {
            let size = u32::from_be_bytes([0, flv[i + 1], flv[i + 2], flv[i + 3]]) as usize;
            let ts = u32::from_be_bytes([0, flv[i + 4], flv[i + 5], flv[i + 6]]);
            assert!(ts >= last_ms, "timestamps must not go backwards");
            last_ms = ts;
            i += 11 + size + 4;
        }
    }

    #[cfg(feature = "video")]
    #[test]
    fn publishes_flv_with_interleaved_h264() {
        use crate::video::{VideoSpec, testutil::render_test_clip};

        let Some(clip) = render_test_clip("rtmp") else {
            return;
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let (vtx, vrx) = mpsc::sync_channel(32);
        // Gate: the audio side runs at full speed, so make sure every video
        // frame is queued before the first audio frame.
        let (gate_tx, gate_rx) = mpsc::sync_channel::<()>(1);
        let clip_for_producer = clip.clone();
        let producer = std::thread::spawn(move || {
            let mut decoder = crate::video::VideoDecoder::open(&clip_for_producer).unwrap();
            let frames = decoder.decode_all().unwrap();
            for frame in &frames {
                vtx.send(Arc::new(frame.clone())).unwrap();
            }
            gate_tx.send(()).unwrap();
        });

        let bytes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut output = RtmpOutput::new(
            config(),
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
        output
            .connect_to(Box::new(CaptureSink {
                bytes: bytes.clone(),
            }))
            .expect("connect");
        let handle = std::thread::spawn(move || output.run());
        gate_rx.recv().expect("video queued");
        sine_frames(&tx, 0.8);
        drop(tx);
        producer.join().expect("video producer");
        handle.join().unwrap().expect("run finishes");

        let flv = bytes.lock().unwrap().clone();
        assert_eq!(flv[4], 0x05, "audio+video flags");
        let mut saw_video_seq = false;
        let mut video_tags = 0;
        let mut audio_tags = 0;
        let mut i = 13;
        while i + 11 < flv.len() {
            let kind = flv[i];
            let size = u32::from_be_bytes([0, flv[i + 1], flv[i + 2], flv[i + 3]]) as usize;
            assert!(i + 11 + size + 4 <= flv.len(), "tag out of bounds");
            match kind {
                flv::TAG_VIDEO => {
                    video_tags += 1;
                    assert_eq!(flv[i + 11] & 0x0F, 7, "AVC codec id");
                    if flv[i + 12] == 0 {
                        assert!(!saw_video_seq, "only one video sequence header");
                        saw_video_seq = true;
                        let dcr = &flv[i + 16..i + 11 + size];
                        assert_eq!(dcr[0], 1, "AVCDecoderConfigurationRecord");
                        assert!(dcr.len() > 20, "record carries SPS/PPS");
                    } else {
                        assert_eq!(flv[i + 12], 1, "NALU packet type");
                        assert_eq!(flv[i + 13], 0, "composition time (no B-frames)");
                        // 4-byte length prefix per NAL, sizes add up.
                        let mut p = i + 16;
                        let end = i + 11 + size;
                        while p + 4 <= end {
                            let n = u32::from_be_bytes(flv[p..p + 4].try_into().unwrap()) as usize;
                            assert!(p + 4 + n <= end, "NAL length in bounds");
                            p += 4 + n;
                        }
                        assert_eq!(p, end, "NAL data consumes the whole tag");
                    }
                }
                flv::TAG_AUDIO => audio_tags += 1,
                _ => {}
            }
            i += 11 + size + 4;
        }
        assert!(saw_video_seq, "video sequence header required");
        assert!(video_tags >= 10, "1 s of 25 fps video, got {video_tags}");
        assert!(audio_tags >= 5, "0.8 s of audio, got {audio_tags}");
        if let Some(out) = std::env::var_os("CRABSOUP_DUMP") {
            std::fs::write(out, &flv).unwrap();
        }
        let _ = std::fs::remove_file(&clip);
    }
}
