//! FFmpeg video decode (Part H1) — the crate's only FFmpeg touch point.
//!
//! All `unsafe` lives inside the `ffmpeg-next` crate; this module calls its
//! safe API only, converting decoded frames to YUV420P `VideoFrame`s. The
//! decoder is pull-style: one decode thread owns it and pushes frames into a
//! `VideoTap`, so a slow video decode can never stall the audio pull chain.

use std::collections::VecDeque;
use std::path::Path;

use ffmpeg::media::Type;
use ffmpeg::software::scaling::{Context as Scaler, Flags as ScalingFlags};
use ffmpeg::util::format::Pixel;
use ffmpeg_next as ffmpeg;

use super::frame::VideoFrame;
use crate::Result;

/// Decode the video stream of a media file into YUV420P frames.
pub struct VideoDecoder {
    ictx: ffmpeg::format::context::Input,
    stream_index: usize,
    decoder: ffmpeg::decoder::Video,
    scaler: Option<Scaler>,
    tb_num: i64,
    tb_den: i64,
    /// Frames decoded but not yet returned (B-frame reordering).
    pending: VecDeque<VideoFrame>,
    drained: bool,
}

impl VideoDecoder {
    /// Open `path` and locate its best video stream. Fails if there is none.
    pub fn open(path: &Path) -> Result<Self> {
        ffmpeg::init().map_err(|e| format!("ffmpeg init: {e}"))?;
        let ictx = ffmpeg::format::input(path).map_err(|e| format!("open {path:?}: {e}"))?;
        let stream = ictx
            .streams()
            .best(Type::Video)
            .ok_or_else(|| format!("no video stream in {path:?}"))?;
        let stream_index = stream.index();
        let tb = stream.time_base();
        let ctx = ffmpeg::codec::context::Context::from_parameters(stream.parameters())
            .map_err(|e| format!("video codec params: {e}"))?;
        let decoder = ctx
            .decoder()
            .video()
            .map_err(|e| format!("open video decoder: {e}"))?;
        // The decoder opens lazily on the first `send_packet`.
        let scaler = Scaler::get(
            decoder.format(),
            decoder.width(),
            decoder.height(),
            Pixel::YUV420P,
            decoder.width(),
            decoder.height(),
            ScalingFlags::BILINEAR,
        )
        .map_err(|e| format!("swscale init: {e}"))?;
        Ok(Self {
            ictx,
            stream_index,
            decoder,
            scaler: Some(scaler),
            tb_num: tb.0 as i64,
            tb_den: tb.1 as i64,
            pending: VecDeque::new(),
            drained: false,
        })
    }

    /// Width of the decoded pictures.
    pub fn width(&self) -> u32 {
        self.decoder.width()
    }

    /// Height of the decoded pictures.
    pub fn height(&self) -> u32 {
        self.decoder.height()
    }

    /// Nominal frame rate of the stream (numerator, denominator).
    pub fn frame_rate(&self) -> (i32, i32) {
        let fr = self.decoder.frame_rate().unwrap_or(ffmpeg::Rational(0, 0));
        (fr.0, fr.1)
    }

    /// Duration of the container in microseconds, when the file carries
    /// one — the fade-out window anchor for `video.fade` (Part H3).
    pub fn duration_us(&self) -> Option<u64> {
        let d = self.ictx.duration();
        (d > 0).then_some(d as u64)
    }

    /// Decode the whole file into frames, oldest first.
    pub fn decode_all(&mut self) -> Result<Vec<VideoFrame>> {
        let mut frames = Vec::new();
        let mut scaler = self.scaler.take().ok_or("decoder already drained")?;
        let mut frame = ffmpeg::frame::Video::empty();
        for (stream, packet) in self.ictx.packets() {
            if stream.index() != self.stream_index {
                continue;
            }
            self.decoder
                .send_packet(&packet)
                .map_err(|e| format!("send packet: {e}"))?;
            while Self::receive_one(&mut self.decoder, &mut frame)? {
                let pts = frame.pts().unwrap_or(0);
                let mut out = ffmpeg::frame::Video::empty();
                scaler
                    .run(&frame, &mut out)
                    .map_err(|e| format!("scale frame: {e}"))?;
                frames.push(copy_planes(pts, self.tb_num, self.tb_den, &out));
            }
        }
        self.drain_remaining(&mut scaler, &mut frames)?;
        self.scaler = Some(scaler);
        self.drained = true;
        Ok(frames)
    }

    fn drain_remaining(&mut self, scaler: &mut Scaler, frames: &mut Vec<VideoFrame>) -> Result<()> {
        self.decoder
            .send_eof()
            .map_err(|e| format!("send eof: {e}"))?;
        let mut frame = ffmpeg::frame::Video::empty();
        while let Ok(()) = self.decoder.receive_frame(&mut frame) {
            let pts = frame.pts().unwrap_or(0);
            let mut out = ffmpeg::frame::Video::empty();
            scaler
                .run(&frame, &mut out)
                .map_err(|e| format!("scale frame: {e}"))?;
            frames.push(copy_planes(pts, self.tb_num, self.tb_den, &out));
        }
        Ok(())
    }

    /// Pull the next frame, decoding more input as needed. Returns `None` at
    /// end of stream.
    pub fn read_frame(&mut self) -> Result<Option<VideoFrame>> {
        if let Some(f) = self.pending.pop_front() {
            return Ok(Some(f));
        }
        let mut scaler = self.scaler.take().ok_or("decoder already drained")?;
        let mut out = ffmpeg::frame::Video::empty();
        let mut result = None;
        for (stream, packet) in self.ictx.packets() {
            if stream.index() != self.stream_index {
                continue;
            }
            self.decoder
                .send_packet(&packet)
                .map_err(|e| format!("send packet: {e}"))?;
            let mut frame = ffmpeg::frame::Video::empty();
            while Self::receive_one(&mut self.decoder, &mut frame)? {
                let pts = frame.pts().unwrap_or(0);
                scaler
                    .run(&frame, &mut out)
                    .map_err(|e| format!("scale frame: {e}"))?;
                let f = copy_planes(pts, self.tb_num, self.tb_den, &out);
                if result.is_none() {
                    result = Some(f);
                } else {
                    self.pending.push_back(f);
                }
            }
            if result.is_some() {
                break;
            }
        }
        if result.is_none() && !self.drained {
            let mut frames = Vec::new();
            self.drain_remaining(&mut scaler, &mut frames)?;
            self.drained = true;
            self.pending.extend(frames);
            result = self.pending.pop_front();
        }
        self.scaler = Some(scaler);
        Ok(result)
    }

    /// Receive one frame: `Ok(true)` = frame ready, `Ok(false)` = needs more
    /// input or stream over, `Err` = decode failure. Takes the decoder, not
    /// `&mut self`, so callers can hold the packets iterator alive.
    fn receive_one(
        decoder: &mut ffmpeg::decoder::Video,
        frame: &mut ffmpeg::frame::Video,
    ) -> Result<bool> {
        match decoder.receive_frame(frame) {
            Ok(()) => Ok(true),
            Err(ffmpeg::Error::Other { errno: 11 }) => Ok(false),
            Err(ffmpeg::Error::Eof) => Ok(false),
            Err(e) => Err(format!("receive frame: {e}").into()),
        }
    }
}

fn copy_planes(pts: i64, tb_num: i64, tb_den: i64, frame: &ffmpeg::frame::Video) -> VideoFrame {
    let width = frame.width();
    let height = frame.height();
    let copy_plane = |plane: usize, plane_w: usize, plane_h: usize| -> Vec<u8> {
        let stride = frame.stride(plane);
        let data = frame.data(plane);
        let mut out = Vec::with_capacity(plane_w * plane_h);
        for row in 0..plane_h {
            out.extend_from_slice(&data[row * stride..row * stride + plane_w]);
        }
        out
    };
    VideoFrame::new(
        pts_us(pts, tb_num, tb_den),
        width,
        height,
        copy_plane(0, width as usize, height as usize),
        copy_plane(1, (width / 2) as usize, (height / 2) as usize),
        copy_plane(2, (width / 2) as usize, (height / 2) as usize),
    )
}

fn pts_us(pts: i64, tb_num: i64, tb_den: i64) -> u64 {
    (pts * tb_num * 1_000_000 / tb_den).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::testutil::render_test_clip;

    #[test]
    fn decodes_testsrc_to_yuv420p_frames() {
        let Some(path) = render_test_clip("decode_all") else {
            return;
        };
        let mut decoder = VideoDecoder::open(&path).expect("open");
        assert_eq!((decoder.width(), decoder.height()), (320, 240));
        let frames = decoder.decode_all().expect("decode");
        // 1 s at 25 fps, with room for encode timing jitter.
        assert!(
            (24..=27).contains(&frames.len()),
            "expected ~25 frames, got {}",
            frames.len()
        );
        for f in &frames {
            assert_eq!(
                f.plane_sizes(),
                (320 * 240, 160 * 120, 160 * 120),
                "YUV420P plane sizes at 320x240"
            );
        }
        assert!(
            frames.windows(2).all(|w| w[0].pts_us < w[1].pts_us),
            "presentation timestamps must be strictly increasing"
        );
        assert_eq!(frames[0].pts_us, 0, "first frame starts at t=0");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn read_frame_pulls_sequentially() {
        let Some(path) = render_test_clip("read_frame") else {
            return;
        };
        let mut decoder = VideoDecoder::open(&path).expect("open");
        let mut count = 0;
        let mut last_pts = 0;
        while let Some(f) = decoder.read_frame().expect("read") {
            assert!(f.pts_us >= last_pts);
            last_pts = f.pts_us;
            count += 1;
        }
        assert!(
            (24..=27).contains(&count),
            "expected ~25 frames, got {count}"
        );
        assert!(decoder.read_frame().expect("post-eof read").is_none());
        std::fs::remove_file(&path).ok();
    }
}
