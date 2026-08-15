//! H.264 encoding for the HLS video path (Part H6).
//!
//! Wraps ffmpeg-next's libx264 encoder: YUV420P `VideoFrame`s in, Annex-B
//! access units out, timestamps on the 90 kHz HLS clock. Baseline profile
//! without B-frames keeps PTS == DTS, so the TS muxer needs no DTS field.

use ffmpeg::format::Pixel;
use ffmpeg_next as ffmpeg;

use super::frame::VideoFrame;
use crate::Result;

const PTS_90K: i64 = 90_000;

/// One encoded H.264 access unit with its presentation time.
pub struct EncodedAu {
    pub pts_90k: u64,
    /// Annex-B bytes (start-code-prefixed NALs).
    pub data: Vec<u8>,
}

pub struct VideoEncoder {
    encoder: ffmpeg::codec::encoder::Video,
    frame: ffmpeg::frame::Video,
    width: u32,
    height: u32,
}

impl VideoEncoder {
    /// Open a baseline-profile H.264 encoder for `width`x`height` at
    /// `fps_num/fps_den` frames per second.
    pub fn h264(width: u32, height: u32, fps_num: i32, fps_den: i32, bitrate: u64) -> Result<Self> {
        let codec =
            ffmpeg::encoder::find(ffmpeg::codec::Id::H264).ok_or("no H.264 encoder available")?;
        let mut video = ffmpeg::codec::context::Context::new_with_codec(codec)
            .encoder()
            .video()
            .map_err(|e| format!("h264 encoder: {e}"))?;
        video.set_width(width);
        video.set_height(height);
        video.set_format(Pixel::YUV420P);
        video.set_frame_rate(Some(ffmpeg::Rational(fps_num, fps_den)));
        video.set_time_base(ffmpeg::Rational(1, PTS_90K as i32));
        video.set_bit_rate(bitrate as usize);
        let mut opts = ffmpeg::Dictionary::new();
        opts.set("profile", "baseline");
        opts.set("preset", "ultrafast");
        opts.set("tune", "zerolatency");
        let encoder: ffmpeg::codec::encoder::Video = video
            .open_with(opts)
            .map_err(|e| format!("h264 open: {e}"))?;

        let mut frame = ffmpeg::frame::Video::empty();
        frame.set_format(Pixel::YUV420P);
        frame.set_width(width);
        frame.set_height(height);
        // Allocate the AVFrame's buffer here — ffmpeg-next's stride/data
        // accessors require it before the first send_frame. The unsafe is a
        // thin call into ffmpeg-next's own safe allocator.
        unsafe { frame.alloc(Pixel::YUV420P, width, height) };
        Ok(Self {
            encoder,
            frame,
            width,
            height,
        })
    }

    /// Encode one picture, returning any access units it produced.
    pub fn push(&mut self, video: &VideoFrame) -> Result<Vec<EncodedAu>> {
        fill_plane(
            &mut self.frame,
            0,
            &video.y,
            self.width as usize,
            self.height as usize,
        )?;
        fill_plane(
            &mut self.frame,
            1,
            &video.u,
            (self.width / 2) as usize,
            (self.height / 2) as usize,
        )?;
        fill_plane(
            &mut self.frame,
            2,
            &video.v,
            (self.width / 2) as usize,
            (self.height / 2) as usize,
        )?;
        let pts = (video.pts_us * PTS_90K as u64 / 1_000_000) as i64;
        self.frame.set_pts(Some(pts));
        self.encoder
            .send_frame(&self.frame)
            .map_err(|e| format!("h264 send frame: {e}"))?;
        self.drain()
    }

    /// Flush the encoder's tail after the last frame.
    pub fn finish(&mut self) -> Result<Vec<EncodedAu>> {
        self.encoder
            .send_eof()
            .map_err(|e| format!("h264 eof: {e}"))?;
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<EncodedAu>> {
        let mut out = Vec::new();
        let mut packet = ffmpeg::packet::Packet::empty();
        while let Ok(()) = self.encoder.receive_packet(&mut packet) {
            let pts = packet.pts().unwrap_or(0).max(0) as u64;
            out.push(EncodedAu {
                pts_90k: pts,
                data: annexb(packet.data().unwrap_or(&[])),
            });
        }
        Ok(out)
    }
}

/// Copy a tightly packed plane into `frame`, row by row (the encoder's
/// plane stride may exceed the plane width).
fn fill_plane(
    frame: &mut ffmpeg::frame::Video,
    plane: usize,
    src: &[u8],
    width: usize,
    height: usize,
) -> Result<()> {
    let stride = frame.stride(plane);
    let dst = frame.data_mut(plane);
    if stride == width {
        dst.copy_from_slice(src);
    } else {
        for row in 0..height {
            dst[row * stride..row * stride + width]
                .copy_from_slice(&src[row * width..(row + 1) * width]);
        }
    }
    Ok(())
}

/// Normalize the packet to Annex-B (start codes). libx264 emits Annex-B by
/// default; if a length-prefixed AVCC payload ever shows up, repack it.
fn annexb(data: &[u8]) -> Vec<u8> {
    if data.len() >= 4 && (data[0..4] == [0, 0, 0, 1] || data[0..3] == [0, 0, 1]) {
        return data.to_vec();
    }
    let mut out = Vec::with_capacity(data.len() + data.len() / 4);
    let mut i = 0;
    while i + 4 <= data.len() {
        let n = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
        if i + 4 + n > data.len() {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[i + 4..i + 4 + n]);
        i += 4 + n;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{VideoDecoder, testutil::render_test_clip};

    #[test]
    fn encodes_test_clip_to_annexb_access_units() {
        let Some(path) = render_test_clip("encode") else {
            return;
        };
        let mut decoder = VideoDecoder::open(&path).expect("decode testsrc");
        let frames = decoder.decode_all().expect("decode all frames");
        assert!(!frames.is_empty());

        let (fps_num, fps_den) = decoder.frame_rate();
        let mut encoder = VideoEncoder::h264(
            decoder.width(),
            decoder.height(),
            fps_num,
            fps_den,
            1_500_000,
        )
        .expect("open h264 encoder");

        let mut aus = Vec::new();
        for frame in &frames {
            aus.extend(encoder.push(frame).expect("encode frame"));
        }
        aus.extend(encoder.finish().expect("flush encoder"));
        // Every picture yields one access unit (baseline, no B-frames); the
        // first also carries the SPS/PPS parameter sets.
        assert_eq!(aus.len(), frames.len());
        // AUs arrive in presentation order on the 90 kHz clock.
        for pair in aus.windows(2) {
            assert!(pair[0].pts_90k < pair[1].pts_90k);
        }
        // First AU starts with SPS (7), PPS (8) and the IDR slice (5).
        let nals = |au: &[u8]| {
            let mut out = Vec::new();
            let mut i = 0;
            while i + 3 < au.len() {
                if au[i..i + 4] == [0, 0, 0, 1] {
                    out.push(au[i + 4] & 0x1f);
                    i += 4;
                } else if au[i..i + 3] == [0, 0, 1] {
                    out.push(au[i + 3] & 0x1f);
                    i += 3;
                } else {
                    i += 1;
                }
            }
            out
        };
        assert_eq!(nals(&aus[0].data)[0..2], [7, 8]);
        assert!(nals(&aus[0].data).contains(&5), "first picture is an IDR");
        // Every AU starts with an Annex-B start code.
        for au in &aus {
            assert_eq!(&au.data[0..4], &[0x00, 0x00, 0x00, 0x01]);
        }
        // Test clip is 1 s @ 25 fps → pts span ~0.96 s on the 90 kHz clock.
        assert!(aus.last().unwrap().pts_90k >= 80_000);
    }
}
