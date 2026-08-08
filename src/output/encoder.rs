//! Rust-side encoders that turn the raw PCM bus into MP3 (via LAME) or
//! Ogg/Opus (via libopus + a built-in Ogg muxer). libshout then transports the
//! encoded bytes to Icecast.

use std::ffi::c_int;
use std::ptr;

use crate::config::OutputFormat;
use crate::output::ogg_mux::{OggMuxer, opus_head_packet, opus_tags_packet};
use crate::resample::LinearResampler;
use crate::Result;

/// Uniform interface for a PCM -> stream-format encoder.
pub trait Encoder: Send {
    /// `audio/mpeg` or `audio/ogg` (used for logging and mime hints).
    fn content_type(&self) -> &'static str;

    /// Encode interleaved `f32` PCM (in the stream spec) into encoded bytes.
    fn encode(&mut self, pcm: &[f32]) -> Vec<u8>;

    /// Drain any trailing encoder / muxer state at end of stream.
    fn finish(&mut self) -> Vec<u8>;
}

pub fn create_encoder(
    format: OutputFormat,
    sample_rate: u32,
    channels: u16,
    bitrate: u32,
    title: &str,
) -> Result<Box<dyn Encoder>> {
    match format {
        OutputFormat::Mp3 => Ok(Box::new(Mp3Encoder::new(sample_rate, channels, bitrate)?)),
        OutputFormat::Opus => Ok(Box::new(OpusEncoder::new(
            sample_rate, channels, bitrate, title,
        )?)),
    }
}

fn clamp_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

// ---------------------------------------------------------------------------
// MP3 (LAME FFI)
// ---------------------------------------------------------------------------

// Opaque LAME handle; only ever used behind a pointer.
#[repr(C)]
struct LameFlagsRaw {
    _unused: [u8; 1],
}

#[link(name = "mp3lame")]
extern "C" {
    fn lame_init() -> *mut LameFlagsRaw;
    fn lame_close(gf: *mut LameFlagsRaw);
    fn lame_set_in_samplerate(gf: *mut LameFlagsRaw, rate: c_int) -> c_int;
    fn lame_set_num_channels(gf: *mut LameFlagsRaw, channels: c_int) -> c_int;
    fn lame_set_brate(gf: *mut LameFlagsRaw, kbps: c_int) -> c_int;
    fn lame_set_quality(gf: *mut LameFlagsRaw, quality: c_int) -> c_int;
    fn lame_set_VBR(gf: *mut LameFlagsRaw, mode: c_int) -> c_int;
    fn lame_set_bWriteVbrTag(gf: *mut LameFlagsRaw, write: c_int) -> c_int;
    fn lame_init_params(gf: *mut LameFlagsRaw) -> c_int;
    fn lame_encode_buffer_interleaved(
        gf: *mut LameFlagsRaw,
        pcm: *const i16,
        samples: c_int,
        mp3buf: *mut u8,
        mp3buf_size: c_int,
    ) -> c_int;
    fn lame_encode_flush(gf: *mut LameFlagsRaw, mp3buf: *mut u8, size: c_int) -> c_int;
}

const VBR_OFF: c_int = 0;

pub struct Mp3Encoder {
    gf: *mut LameFlagsRaw,
    channels: usize,
}

unsafe impl Send for Mp3Encoder {}

impl Mp3Encoder {
    pub fn new(sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        let gf = unsafe { lame_init() };
        if gf.is_null() {
            return Err("lame_init failed".into());
        }
        let kbps = (bitrate / 1000).clamp(32, 320) as c_int;

        let ok = unsafe {
            lame_set_in_samplerate(gf, sample_rate as c_int)
                | lame_set_num_channels(gf, channels as c_int)
                | lame_set_brate(gf, kbps)
                | lame_set_quality(gf, 5)
                | lame_set_VBR(gf, VBR_OFF)
                | lame_set_bWriteVbrTag(gf, 0)
                | lame_init_params(gf)
        };
        if ok != 0 {
            unsafe { lame_close(gf) };
            return Err("lame_init_params failed".into());
        }
        Ok(Self {
            gf,
            channels: channels as usize,
        })
    }
}

impl Encoder for Mp3Encoder {
    fn content_type(&self) -> &'static str {
        "audio/mpeg"
    }

    fn encode(&mut self, pcm: &[f32]) -> Vec<u8> {
        let frames = pcm.len() / self.channels;
        if frames == 0 {
            return Vec::new();
        }
        let mut i16buf = Vec::with_capacity(pcm.len());
        for &s in pcm {
            i16buf.push(clamp_i16(s));
        }

        // LAME's recommended output buffer sizing.
        let buf_size = 7200 + frames + frames * self.channels / 4;
        let mut out = vec![0u8; buf_size];
        let written = unsafe {
            lame_encode_buffer_interleaved(
                self.gf,
                i16buf.as_ptr(),
                frames as c_int,
                out.as_mut_ptr(),
                out.len() as c_int,
            )
        };
        if written <= 0 {
            log::warn!("lame encode returned {written}");
            return Vec::new();
        }
        out.truncate(written as usize);
        out
    }

    fn finish(&mut self) -> Vec<u8> {
        let mut out = vec![0u8; 7200 + 1024];
        let written = unsafe { lame_encode_flush(self.gf, out.as_mut_ptr(), out.len() as c_int) };
        if written <= 0 {
            return Vec::new();
        }
        out.truncate(written as usize);
        out
    }
}

impl Drop for Mp3Encoder {
    fn drop(&mut self) {
        if !self.gf.is_null() {
            unsafe { lame_close(self.gf) };
            self.gf = ptr::null_mut();
        }
    }
}

// ---------------------------------------------------------------------------
// Opus (libopus via `audiopus`) + Ogg muxing
// ---------------------------------------------------------------------------

const OPUS_SAMPLE_RATE: u32 = 48_000;
/// 20 ms frame at 48 kHz (960 samples per channel).
const OPUS_FRAME_SAMPLES: usize = 960;
const OPUS_SERIAL: u32 = 0x43_41_42_43; // "CABC"

pub struct OpusEncoder {
    encoder: audiopus::coder::Encoder,
    channels: usize,
    /// Resamples the stream rate to the Opus-required 48 kHz.
    resampler: LinearResampler,
    mux: OggMuxer,
    /// Accumulated 48 kHz interleaved PCM awaiting a full Opus frame.
    pcm: Vec<i16>,
    granule: i64,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(
        sample_rate: u32,
        channels: u16,
        bitrate: u32,
        title: &str,
    ) -> Result<Self> {
        use audiopus::coder::Encoder;
        use audiopus::{Application, Bitrate, Channels, SampleRate};

        let ch = match channels {
            1 => Channels::Mono,
            _ => Channels::Stereo,
        };
        let mut encoder = Encoder::new(SampleRate::Hz48000, ch, Application::Audio)
            .map_err(|e| format!("failed to create opus encoder: {e}"))?;
        encoder
            .set_bitrate(Bitrate::BitsPerSecond(bitrate as i32))
            .map_err(|e| format!("failed to set opus bitrate: {e}"))?;
        if channels > 2 {
            log::warn!("Opus only supports mono/stereo; encoding {} channels", channels);
        }

        let resampler = LinearResampler::new(24, sample_rate, OPUS_SAMPLE_RATE, channels as usize);

        let mut mux = OggMuxer::new(OPUS_SERIAL);
        // Headers go out as their own pages before any audio.
        mux.write_packet(&opus_head_packet(channels.min(2)), 0);
        mux.flush();
        mux.write_packet(&opus_tags_packet(title), 0);
        mux.flush();

        Ok(Self {
            encoder,
            channels: channels as usize,
            resampler,
            mux,
            pcm: Vec::new(),
            granule: 0,
        })
    }

    fn encode_frames(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        let need = OPUS_FRAME_SAMPLES * self.channels;
        let mut encode_buf = vec![0u8; 1276];

        while self.pcm.len() >= need {
            let written = self
                .encoder
                .encode(&self.pcm[..need], &mut encode_buf)
                .map_err(|e| log::error!("opus encode failed: {e}"))
                .unwrap_or(0);
            if written > 0 {
                self.granule += OPUS_FRAME_SAMPLES as i64;
                self.mux.write_packet(&encode_buf[..written], self.granule);
            }
            self.pcm.drain(..need);
        }

        out.extend_from_slice(&self.mux.take_output());
        out
    }
}

impl Encoder for OpusEncoder {
    fn content_type(&self) -> &'static str {
        "audio/ogg"
    }

    fn encode(&mut self, pcm: &[f32]) -> Vec<u8> {
        if pcm.is_empty() {
            return Vec::new();
        }
        // Resample stream rate -> 48 kHz, then convert to i16 for libopus.
        let resampled = self.resampler.resample(pcm).to_vec();
        let mut samples = Vec::with_capacity(resampled.len());
        for &s in &resampled {
            samples.push(clamp_i16(s));
        }
        self.pcm.extend_from_slice(&samples);
        self.encode_frames()
    }

    fn finish(&mut self) -> Vec<u8> {
        // Encode any partial frame (linear interpolation has no tail), then EOS.
        let mut out = self.encode_frames();
        self.mux.finish();
        out.extend_from_slice(&self.mux.take_output());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mp3_encoder_produces_frames() {
        let mut enc = Mp3Encoder::new(44100, 2, 128_000).unwrap();
        let mut pcm = vec![0f32; 4410 * 2];
        for (i, s) in pcm.iter_mut().enumerate() {
            let t = i as f64 / 44100.0;
            *s = (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 * 0.5;
        }
        let out = enc.encode(&pcm);
        assert!(!out.is_empty());
        // MP3 frames start with a sync word.
        assert_eq!(out[0] >> 5, 0b111);
    }

    #[test]
    fn opus_encoder_produces_ogg_stream() {
        let mut enc = OpusEncoder::new(44100, 2, 128_000, "test").unwrap();
        let mut pcm = vec![0f32; 44100 * 2]; // 1 second
        for (i, s) in pcm.iter_mut().enumerate() {
            let t = i as f64 / 44100.0;
            *s = (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 * 0.5;
        }
        let mut bytes = enc.encode(&pcm);
        bytes.extend_from_slice(&enc.finish());
        assert!(!bytes.is_empty());
        // Stream starts with "OggS" (OpusHead) and contains "OpusTags".
        assert_eq!(&bytes[0..4], b"OggS");
        let s = String::from_utf8_lossy(&bytes);
        assert!(s.contains("OpusHead"));
        assert!(s.contains("OpusTags"));
    }
}
