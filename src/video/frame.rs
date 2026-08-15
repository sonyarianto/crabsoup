//! Video frames for the Part H pipeline.
//!
//! Pure Rust: no FFmpeg types leak past `super::ffi`. A `VideoFrame` is a
//! YUV420P picture plus its presentation timestamp, carried in its own
//! `Arc` on a dedicated tap (`super::tap::VideoTap`) — video never rides in
//! `AudioFrame`, so the audio hot path is untouched.

/// One decoded YUV420P picture.
pub struct VideoFrame {
    /// Presentation timestamp in microseconds since stream start.
    pub pts_us: u64,
    pub width: u32,
    pub height: u32,
    /// Luma plane, `width * height` bytes.
    pub y: Vec<u8>,
    /// U plane, `(width / 2) * (height / 2)` bytes.
    pub u: Vec<u8>,
    /// V plane, `(width / 2) * (height / 2)` bytes.
    pub v: Vec<u8>,
}

impl VideoFrame {
    pub fn new(pts_us: u64, width: u32, height: u32, y: Vec<u8>, u: Vec<u8>, v: Vec<u8>) -> Self {
        Self {
            pts_us,
            width,
            height,
            y,
            u,
            v,
        }
    }

    /// Sizes of the three planes, in order (y, u, v).
    pub fn plane_sizes(&self) -> (usize, usize, usize) {
        (self.y.len(), self.u.len(), self.v.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yuv420p_planes_match_dimensions() {
        let (w, h) = (320u32, 240u32);
        let f = VideoFrame::new(
            0,
            w,
            h,
            vec![0; (w * h) as usize],
            vec![0; (w / 2 * h / 2) as usize],
            vec![0; (w / 2 * h / 2) as usize],
        );
        assert_eq!(
            f.plane_sizes(),
            (
                (w * h) as usize,
                (w / 2 * h / 2) as usize,
                (w / 2 * h / 2) as usize
            )
        );
    }
}
