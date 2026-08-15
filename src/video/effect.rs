//! Video effects (Part H3): `video.scale` and `video.fade`, applied on the
//! source render threads so every output (HLS, RTMP, future MP4) gets the
//! processed frames for free.
//!
//! Pure Rust — no new FFmpeg/FFI surface: scaling is a fixed-point bilinear
//! YUV420P resampler, fades blend whole planes toward black (the H2
//! crossfade pattern, reused and made public). Effects are accumulated onto
//! a source config at script evaluation (Lua `video.scale`/`video.fade`
//! wrap a `video.*` marker) and applied per published frame, with the fade
//! windows measured on the source's own timeline (`pts_us` / stream
//! duration).

use super::frame::VideoFrame;
use super::source::VideoSpec;

/// Per-source effects accumulated at script evaluation (Part H3).
#[derive(Clone, Copy, Debug, Default)]
pub struct VideoEffects {
    /// Target size when `video.scale` was applied; `None` = passthrough.
    pub scale: Option<(u32, u32)>,
    /// Fade-in from black over the first `fade_in_seconds` of the stream.
    pub fade_in_seconds: f64,
    /// Fade-out to black over the last `fade_out_seconds` of the stream.
    /// Ignored for looping sources, which have no end to fade into.
    pub fade_out_seconds: f64,
}

impl VideoEffects {
    pub fn is_empty(&self) -> bool {
        self.scale.is_none() && self.fade_in_seconds <= 0.0 && self.fade_out_seconds <= 0.0
    }

    /// The spec an encoder should open at: `video.scale` changes the
    /// published resolution, so outputs must open at the scaled size.
    pub fn scaled_spec(&self, spec: VideoSpec) -> VideoSpec {
        match self.scale {
            Some((w, h)) => VideoSpec {
                width: w,
                height: h,
                ..spec
            },
            None => spec,
        }
    }

    /// Apply scale then fade to `frame`. `duration_us` is the stream's
    /// total length (for the fade-out window); `None` disables fade-out
    /// (looping sources).
    pub fn apply(&self, frame: &VideoFrame, duration_us: Option<u64>) -> VideoFrame {
        let scaled = match self.scale {
            Some((w, h)) => scale_frame(frame, w, h),
            None => frame.clone(),
        };
        let k = self.fade_alpha(scaled.pts_us, duration_us);
        if k >= 256 {
            scaled
        } else {
            blend_to_black(&scaled, k)
        }
    }

    /// 0..=256 blend weight for the frame at `pts_us`: 0 = fully black,
    /// 256 = the untouched picture. Fade-in ramps 0 -> 256 over the first
    /// window, fade-out 256 -> 0 over the last.
    fn fade_alpha(&self, pts_us: u64, duration_us: Option<u64>) -> u32 {
        let mut k = 256u32;
        if self.fade_in_seconds > 0.0 {
            let win = (self.fade_in_seconds * 1_000_000.0) as u64;
            if pts_us < win {
                k = (pts_us * 256 / win.max(1)) as u32;
            }
        }
        if self.fade_out_seconds > 0.0
            && let Some(dur) = duration_us
        {
            let win = (self.fade_out_seconds * 1_000_000.0) as u64;
            if dur >= win && pts_us >= dur - win {
                // Seconds left at the tail: 0 at the very end.
                let into = dur - pts_us;
                k = k.min((into * 256 / win.max(1)) as u32);
            }
        }
        k.min(256)
    }
}

/// Rescale a YUV420P picture to `width`x`height` with fixed-point bilinear
/// interpolation, per plane.
pub fn scale_frame(frame: &VideoFrame, width: u32, height: u32) -> VideoFrame {
    let (w, h) = (width as usize, height as usize);
    let (sw, sh) = (frame.width as usize, frame.height as usize);
    VideoFrame::new(
        frame.pts_us,
        w as u32,
        h as u32,
        scale_plane(&frame.y, sw, sh, w, h),
        scale_plane(&frame.u, sw / 2, sh / 2, w / 2, h / 2),
        scale_plane(&frame.v, sw / 2, sh / 2, w / 2, h / 2),
    )
}

/// Bilinear resample of one tightly packed plane. Source coordinates are
/// half-pixel centered (`(x + 0.5) * sw / dw - 0.5`) so a downscale really
/// averages, with the subpixel fraction as a fixed-point 0..=256 weight —
/// the whole pass is integer arithmetic and deterministic across platforms.
fn scale_plane(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    if dw == sw && dh == sh {
        return src.to_vec();
    }
    let mut out = vec![0u8; dw * dh];
    let sy0 = sh.saturating_sub(1);
    let sx0 = sw.saturating_sub(1);
    for y in 0..dh {
        let pos = (2 * y + 1) as u64 * sh as u64;
        let (sy, wy) = split_axis(pos, dh, sy0);
        let row = sy * sw;
        let row2 = (sy + 1).min(sy0) * sw;
        for x in 0..dw {
            let pos = (2 * x + 1) as u64 * sw as u64;
            let (sx, wx) = split_axis(pos, dw, sx0);
            let tl = src[row + sx] as u32;
            let tr = src[row + (sx + 1).min(sx0)] as u32;
            let bl = src[row2 + sx] as u32;
            let br = src[row2 + (sx + 1).min(sx0)] as u32;
            let top = (tl * (256 - wx) + tr * wx) >> 8;
            let bot = (bl * (256 - wx) + br * wx) >> 8;
            out[y * dw + x] = ((top * (256 - wy) + bot * wy) >> 8) as u8;
        }
    }
    out
}

/// Split a `(2x + 1) * span` product into the source index and its
/// 0..=256 subpixel weight — clamped so the sample lands in-bounds.
fn split_axis(pos: u64, out_len: usize, last: usize) -> (usize, u32) {
    let den = (2 * out_len) as u64;
    let pos = pos.max(den / 2).saturating_sub(den / 2);
    let (s, f) = (pos / den, pos % den);
    let s = (s as usize).min(last);
    let w = (f * 256 / den) as u32;
    (s, w)
}

/// Crossfade `prev` into `curr` by `alpha` (0..=256, 256 = fully `curr`),
/// writing whole planes. All frames share one resolution, so planes blend
/// element-wise. Shared with the H2 slideshow transition.
pub fn blend_planes(
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
            *d = ((*p as u32 * b + *c as u32 * a) >> 8) as u8;
        }
    };
    mix(dst_y, &prev.y, &curr.y);
    mix(dst_u, &prev.u, &curr.u);
    mix(dst_v, &prev.v, &curr.v);
}

/// Blend `frame` toward black (Y = 0, U/V = 128) by `alpha` (0..=256, 256 =
/// the untouched picture). Fade-in/out targets, not a crossfade: there is no
/// second picture to cross into, so the base is black.
pub fn blend_to_black(frame: &VideoFrame, alpha: u32) -> VideoFrame {
    let a = alpha;
    let b = 256 - alpha;
    let mix_luma =
        |src: &[u8]| -> Vec<u8> { src.iter().map(|&p| ((p as u32 * a) >> 8) as u8).collect() };
    let mix_chroma = |src: &[u8]| -> Vec<u8> {
        src.iter()
            .map(|&p| ((p as u32 * a + 128 * b) >> 8) as u8)
            .collect()
    };
    VideoFrame::new(
        frame.pts_us,
        frame.width,
        frame.height,
        mix_luma(&frame.y),
        mix_chroma(&frame.u),
        mix_chroma(&frame.v),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, y: u8, u: u8, v: u8) -> VideoFrame {
        VideoFrame::new(
            0,
            w,
            h,
            vec![y; (w * h) as usize],
            vec![u; (w / 2 * h / 2) as usize],
            vec![v; (w / 2 * h / 2) as usize],
        )
    }

    #[test]
    fn scale_keeps_plane_layout_and_value() {
        let f = solid(320, 240, 128, 90, 190);
        let scaled = scale_frame(&f, 640, 480);
        assert_eq!((scaled.width, scaled.height), (640, 480));
        assert_eq!(
            scaled.plane_sizes(),
            (640 * 480, 320 * 240, 320 * 240),
            "YUV420P planes follow the new size"
        );
        // Bilinear resampling of a flat field is the field itself.
        assert!(scaled.y.iter().all(|&p| p == 128));
        assert!(scaled.u.iter().all(|&p| p == 90));
        assert!(scaled.v.iter().all(|&p| p == 190));
        assert_eq!(scaled.pts_us, f.pts_us, "PTS passes through");
    }

    #[test]
    fn scale_down_interpolates_pixels() {
        // Two-tone 2x2 (left half dark, right half bright), downscaled to
        // 1x1 must land exactly halfway: the output pixel averages all four.
        let mut y = vec![0u8; 4];
        y[0] = 0;
        y[1] = 255;
        y[2] = 0;
        y[3] = 255;
        let f = VideoFrame::new(0, 2, 2, y, vec![128; 1], vec![128; 1]);
        let scaled = scale_frame(&f, 1, 1);
        assert_eq!(scaled.y[0], 127, "bilinear center of 0/255 is ~128");
    }

    #[test]
    fn fade_alpha_windows() {
        let eff = VideoEffects {
            scale: None,
            fade_in_seconds: 2.0,
            fade_out_seconds: 2.0,
        };
        // Fade-in: 0 at the very start, ramping to full by 2 s.
        assert_eq!(eff.fade_alpha(0, Some(10_000_000)), 0);
        assert_eq!(eff.fade_alpha(1_000_000, Some(10_000_000)), 128);
        assert_eq!(eff.fade_alpha(2_000_000, Some(10_000_000)), 256);
        // Steady middle: untouched.
        assert_eq!(eff.fade_alpha(5_000_000, Some(10_000_000)), 256);
        // Fade-out over the last 2 s of a 10 s stream.
        assert_eq!(eff.fade_alpha(9_000_000, Some(10_000_000)), 128);
        assert_eq!(eff.fade_alpha(10_000_000, Some(10_000_000)), 0);
        // No duration -> no fade-out window.
        assert_eq!(eff.fade_alpha(9_000_000, None), 256);
    }

    #[test]
    fn fade_only_fade_out_needs_duration() {
        let eff = VideoEffects {
            scale: None,
            fade_in_seconds: 0.0,
            fade_out_seconds: 1.0,
        };
        // A 1 s stream with a 1 s fade-out fades over its whole length:
        // full at the start of the window, black at the very end.
        assert_eq!(eff.fade_alpha(0, Some(1_000_000)), 256);
        assert_eq!(eff.fade_alpha(500_000, Some(1_000_000)), 128);
        assert_eq!(eff.fade_alpha(1_000_000, Some(1_000_000)), 0);
        assert_eq!(eff.fade_alpha(0, None), 256, "looping: fade-out skipped");
    }

    #[test]
    fn apply_scales_and_fades_to_black() {
        let eff = VideoEffects {
            scale: Some((160, 120)),
            fade_in_seconds: 1.0,
            fade_out_seconds: 0.0,
        };
        let f = solid(320, 240, 200, 100, 150);
        // First frame: scaled down and fully black (fade-in at t=0).
        let first = eff.apply(&f, Some(10_000_000));
        assert_eq!((first.width, first.height), (160, 120));
        assert_eq!(first.y[0], 0);
        assert_eq!(first.u[0], 128);
        assert_eq!(first.v[0], 128);
        // Well past the fade window: only scaled, colors preserved.
        let mid = eff.apply(&f, Some(10_000_000));
        let _ = mid; // fade_alpha is pts-based; exercise a mid-pts frame:
        let mut mid_frame = f;
        mid_frame.pts_us = 5_000_000;
        let mid = eff.apply(&mid_frame, Some(10_000_000));
        assert_eq!(mid.y[0], 200);
        assert_eq!(mid.u[0], 100);
        assert_eq!(mid.v[0], 150);
    }

    #[test]
    fn blend_to_black_endpoints() {
        let f = solid(4, 4, 200, 40, 220);
        let black = blend_to_black(&f, 0);
        assert_eq!(black.y[0], 0);
        assert_eq!(black.u[0], 128);
        assert_eq!(black.v[0], 128);
        let full = blend_to_black(&f, 256);
        assert_eq!(full.y, f.y);
        assert_eq!(full.u, f.u);
        assert_eq!(full.v, f.v);
        // Halfway: luma 100, chroma halfway between 40 and 128.
        let half = blend_to_black(&f, 128);
        assert_eq!(half.y[0], 100);
        assert_eq!(half.u[0], 84);
        assert_eq!(half.v[0], 174);
    }
}
