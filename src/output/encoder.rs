//! Rust-side encoders that turn the raw PCM bus into MP3 (via LAME) or
//! Ogg/Opus (via libopus + a built-in Ogg muxer). libshout then transports the
//! encoded bytes to Icecast.

use std::ffi::{c_int, c_uint, c_void};
use std::ptr;

use crate::Result;
use crate::config::OutputFormat;
use crate::output::ogg_mux::{OggMuxer, opus_head_packet, opus_tags_packet};
use crate::resample::SincResampler;

/// Uniform interface for a PCM -> stream-format encoder.
pub trait Encoder: Send {
    /// `audio/mpeg` or `audio/ogg` (used for logging and mime hints).
    fn content_type(&self) -> &'static str;

    /// Encode interleaved `f32` PCM (in the stream spec) into encoded bytes.
    fn encode(&mut self, pcm: &[f32]) -> Vec<u8>;

    /// Drain any trailing encoder / muxer state at end of stream.
    fn finish(&mut self) -> Vec<u8>;

    /// Emit an in-stream title update, returning the bytes to send to the
    /// server (empty when the format has no in-stream mechanism).
    fn set_title(&mut self, _title: &str) -> Vec<u8> {
        Vec::new()
    }
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
            sample_rate,
            channels,
            bitrate,
            title,
        )?)),
        OutputFormat::Aac => Ok(Box::new(AacEncoder::new(sample_rate, channels, bitrate)?)),
    }
}

pub(crate) fn clamp_i16(s: f32) -> i16 {
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
unsafe extern "C" {
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
    resampler: SincResampler,
    mux: OggMuxer,
    /// Accumulated 48 kHz interleaved PCM awaiting a full Opus frame.
    pcm: Vec<i16>,
    granule: i64,
    /// Header pages (OpusHead + OpusTags) still queued; replaced by
    /// `set_title` while pending and emitted with the first encoded audio.
    pending_headers: Option<(Vec<u8>, Vec<u8>)>,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u16, bitrate: u32, title: &str) -> Result<Self> {
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
            log::warn!(
                "Opus only supports mono/stereo; encoding {} channels",
                channels
            );
        }

        let resampler = SincResampler::new(24, sample_rate, OPUS_SAMPLE_RATE, channels as usize);

        let mut mux = OggMuxer::new(OPUS_SERIAL);
        // Headers go out as their own pages before any audio.
        mux.write_packet(&opus_head_packet(channels.min(2)), 0);
        mux.flush();
        let head_page = mux.take_output();
        mux.write_packet(&opus_tags_packet(title), 0);
        mux.flush();
        let tags_page = mux.take_output();

        Ok(Self {
            encoder,
            channels: channels as usize,
            resampler,
            mux,
            pcm: Vec::new(),
            granule: 0,
            pending_headers: Some((head_page, tags_page)),
        })
    }

    fn encode_frames(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some((head, tags)) = self.pending_headers.take() {
            out.extend_from_slice(&head);
            out.extend_from_slice(&tags);
        }
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
                // One page per 20 ms packet so encoded audio reaches the
                // server promptly instead of accumulating in the page buffer.
                self.mux.flush();
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

    /// Replace the queued OpusTags header title. Icecast parses OpusTags
    /// only as stream headers (and only on 2.5+), so once the headers are
    /// emitted any later call is a no-op: mid-stream comment pages would be
    /// forwarded to listeners as audio, and URL updates are rejected.
    fn set_title(&mut self, title: &str) -> Vec<u8> {
        if self.pending_headers.is_some() {
            let mut mux = OggMuxer::new(OPUS_SERIAL);
            mux.write_packet(&opus_head_packet(self.channels.min(2) as u16), 0);
            mux.flush();
            let head_page = mux.take_output();
            mux.write_packet(&opus_tags_packet(title), 0);
            mux.flush();
            let tags_page = mux.take_output();
            self.pending_headers = Some((head_page, tags_page));
        }
        Vec::new()
    }
}

// ---------------------------------------------------------------------------
// AAC (FDK-AAC FFI, ADTS transport)
// ---------------------------------------------------------------------------

// Opaque FDK-AAC encoder handle; only ever used behind a pointer.
#[repr(C)]
struct AacEncoderRaw {
    _unused: [u8; 1],
}

// Error / result codes (aacenc_lib.h).
const AACENC_OK: c_int = 0x0000;
const AACENC_ENCODE_EOF: c_int = 0x0080;
// Parameter IDs (AACENC_PARAM).
const AACENC_AOT: c_uint = 0x0100;
const AACENC_BITRATE: c_uint = 0x0101;
const AACENC_SAMPLERATE: c_uint = 0x0103;
const AACENC_CHANNELMODE: c_uint = 0x0106;
const AACENC_TRANSMUX: c_uint = 0x0300;
// Parameter values.
const AOT_AAC_LC: c_uint = 2;
const AOT_AAC_HE: c_uint = 5; // MPEG-4 HE-AAC (AAC-LC core + SBR, "AAC+")
const MODE_1: c_uint = 1; // mono
const MODE_2: c_uint = 2; // stereo
const TT_MP4_ADTS: c_uint = 2;
/// Raw access units (no framing): FLV/RTMP carry raw AAC plus the
/// AudioSpecificConfig in a separate sequence header (Part H5).
const TT_MP4_RAW: c_uint = 0;
// Buffer identifiers (AACENC_BufferIdentifier).
const IN_AUDIO_DATA: c_int = 0;
const OUT_BITSTREAM_DATA: c_int = 3;

#[repr(C)]
struct AacBufDesc {
    num_bufs: c_int,
    bufs: *mut *mut c_void,
    buffer_identifiers: *mut c_int,
    buf_sizes: *mut c_int,
    buf_el_sizes: *mut c_int,
}

#[repr(C)]
struct AacInArgs {
    num_in_samples: c_int,
    num_anc_bytes: c_int,
}

#[repr(C)]
struct AacOutArgs {
    num_out_bytes: c_int,
    num_in_samples: c_int,
    num_anc_bytes: c_int,
    bit_res_state: c_int,
}

#[repr(C)]
struct AacInfoStruct {
    max_out_buf_bytes: c_uint,
    max_anc_bytes: c_uint,
    in_buf_fill_level: c_uint,
    input_channel_mask: c_uint,
    frame_size: c_uint,
    encoder_delay: c_uint,
    encoder_nb_samples: c_uint,
    conf_buf: [u8; 64],
    conf_size: c_uint,
}

#[link(name = "fdk-aac")]
unsafe extern "C" {
    fn aacEncOpen(handle: *mut *mut AacEncoderRaw, modules: c_uint, max_channels: c_uint) -> c_int;
    fn aacEncClose(handle: *mut *mut AacEncoderRaw) -> c_int;
    fn aacEncoder_SetParam(handle: *mut AacEncoderRaw, param: c_uint, value: c_uint) -> c_int;
    fn aacEncEncode(
        handle: *mut AacEncoderRaw,
        in_desc: *const AacBufDesc,
        out_desc: *const AacBufDesc,
        in_args: *const AacInArgs,
        out_args: *mut AacOutArgs,
    ) -> c_int;
    fn aacEncInfo(handle: *mut AacEncoderRaw, info: *mut AacInfoStruct) -> c_int;
}

pub struct AacEncoder {
    handle: *mut AacEncoderRaw,
    out_buf_size: usize,
    /// Samples per encoded frame per channel (1024 for AAC-LC).
    pub frame_size: u32,
    /// AudioSpecificConfig reported by `aacEncInfo` (2 bytes for LC) —
    /// sent as the FLV AAC sequence header (Part H5).
    pub asc: Vec<u8>,
}

unsafe impl Send for AacEncoder {}

impl AacEncoder {
    /// AAC-LC — the profile used for Icecast and HLS.
    pub fn new(sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        Self::new_with(sample_rate, channels, bitrate, AOT_AAC_LC, TT_MP4_ADTS)
    }

    /// HE-AAC ("AAC+", SBR) — the profile SHOUTcast v2 expects from
    /// `audio/aacp` sources.
    pub fn new_he_aac(sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        Self::new_with(sample_rate, channels, bitrate, AOT_AAC_HE, TT_MP4_ADTS)
    }

    /// AAC-LC with raw transport (no ADTS framing): FLV/RTMP publishing.
    pub fn new_raw(sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        Self::new_with(sample_rate, channels, bitrate, AOT_AAC_LC, TT_MP4_RAW)
    }

    fn new_with(
        sample_rate: u32,
        channels: u16,
        bitrate: u32,
        aot: c_uint,
        transport: c_uint,
    ) -> Result<Self> {
        let mut handle = ptr::null_mut();
        let status = unsafe { aacEncOpen(&mut handle, 0, channels.max(1) as c_uint) };
        if status != AACENC_OK || handle.is_null() {
            return Err(format!("aacEncOpen failed: {status:#x}").into());
        }
        let ch_mode = if channels == 1 { MODE_1 } else { MODE_2 };
        if channels > 2 {
            log::warn!(
                "AAC only supports mono/stereo; encoding {} channels",
                channels
            );
        }
        let ok = unsafe {
            aacEncoder_SetParam(handle, AACENC_AOT, aot)
                | aacEncoder_SetParam(handle, AACENC_SAMPLERATE, sample_rate)
                | aacEncoder_SetParam(handle, AACENC_CHANNELMODE, ch_mode)
                | aacEncoder_SetParam(handle, AACENC_BITRATE, bitrate)
                | aacEncoder_SetParam(handle, AACENC_TRANSMUX, transport)
        };
        if ok != AACENC_OK {
            unsafe { aacEncClose(&mut handle) };
            return Err(format!("aacEncoder_SetParam failed: {ok:#x}").into());
        }
        // Trigger the internal (re)configuration before querying info.
        let status = unsafe {
            aacEncEncode(
                handle,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null_mut(),
            )
        };
        if status != AACENC_OK {
            unsafe { aacEncClose(&mut handle) };
            return Err(format!("aacEncEncode init failed: {status:#x}").into());
        }
        let mut info: AacInfoStruct = unsafe { std::mem::zeroed() };
        let status = unsafe { aacEncInfo(handle, &mut info) };
        if status != AACENC_OK {
            unsafe { aacEncClose(&mut handle) };
            return Err(format!("aacEncInfo failed: {status:#x}").into());
        }
        // FDK's documented max; clamp so small configs stay usable.
        let out_buf_size = (info.max_out_buf_bytes as usize).max(8192);
        Ok(Self {
            handle,
            out_buf_size,
            frame_size: info.frame_size.max(1),
            asc: info.conf_buf[..info.conf_size as usize].to_vec(),
        })
    }

    /// The AudioSpecificConfig for the configured stream — the payload of
    /// the FLV/RTMP AAC sequence header.
    pub fn audio_specific_config(&self) -> &[u8] {
        &self.asc
    }

    /// Run one `aacEncEncode` call. FDK consumes at most one frame's worth of
    /// input per call (`nSamplesToRead - nSamplesRead`); the caller must loop
    /// and feed any leftover input, using the reported `numInSamples`.
    fn encode_call(&mut self, pcm: &[i16], eof: bool) -> (Vec<u8>, c_int, c_int) {
        let mut out = vec![0u8; self.out_buf_size];
        let mut in_ptr: *mut c_void = if eof {
            ptr::null_mut()
        } else {
            pcm.as_ptr() as *mut c_void
        };
        let mut in_id = IN_AUDIO_DATA;
        let mut in_size = pcm.len().saturating_mul(2) as c_int;
        let mut in_el = 2;
        let in_desc = AacBufDesc {
            num_bufs: 1,
            bufs: &mut in_ptr,
            buffer_identifiers: &mut in_id,
            buf_sizes: &mut in_size,
            buf_el_sizes: &mut in_el,
        };
        let mut out_ptr: *mut c_void = out.as_mut_ptr() as *mut c_void;
        let mut out_id = OUT_BITSTREAM_DATA;
        let mut out_size = out.len() as c_int;
        let mut out_el = 1;
        let out_desc = AacBufDesc {
            num_bufs: 1,
            bufs: &mut out_ptr,
            buffer_identifiers: &mut out_id,
            buf_sizes: &mut out_size,
            buf_el_sizes: &mut out_el,
        };
        // numInSamples == -1 drains the encoder's internal buffer.
        let in_args = AacInArgs {
            num_in_samples: if eof { -1 } else { pcm.len() as c_int },
            num_anc_bytes: 0,
        };
        let mut out_args = AacOutArgs {
            num_out_bytes: 0,
            num_in_samples: 0,
            num_anc_bytes: 0,
            bit_res_state: 0,
        };
        let status = unsafe {
            aacEncEncode(
                self.handle,
                if eof { ptr::null() } else { &in_desc },
                &out_desc,
                &in_args,
                &mut out_args,
            )
        };
        if status != AACENC_OK && status != AACENC_ENCODE_EOF {
            log::warn!("aacEncEncode failed: {status:#x}");
        }
        out.truncate(out_args.num_out_bytes.max(0) as usize);
        (out, status, out_args.num_in_samples.max(0))
    }
    /// Encode `pcm`, returning one chunk per `aacEncEncode` call (each is
    /// one access unit on the raw transport, one ADTS frame on ADTS).
    pub fn encode_aus(&mut self, pcm: &[f32]) -> Vec<Vec<u8>> {
        let mut i16buf = Vec::with_capacity(pcm.len());
        for &s in pcm {
            i16buf.push(clamp_i16(s));
        }
        let mut out = Vec::new();
        let mut remaining: &[i16] = &i16buf;
        let mut guard = 0;
        while !remaining.is_empty() {
            let (chunk, status, consumed) = self.encode_call(remaining, false);
            if !chunk.is_empty() {
                out.push(chunk);
            }
            if status != AACENC_OK {
                break;
            }
            if consumed == 0 {
                log::warn!("aacEncEncode consumed no input; giving up");
                break;
            }
            remaining = &remaining[consumed as usize..];
            guard += 1;
            if guard > 1_000_000 {
                log::warn!("aacEncEncode loop guard hit");
                break;
            }
        }
        out
    }
}

impl AacEncoder {
    /// Drain the encoder tail as one access unit per chunk. FDK-AAC drains
    /// via repeated calls with numInSamples == -1 until it reports
    /// AACENC_ENCODE_EOF. The MP4 output (Part H4) needs the frames
    /// separated to assign each its own PTS; the `Encoder` trait's
    /// `finish` concatenates them instead.
    pub fn finish_aus(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut guard = 0;
        loop {
            let (chunk, status, _) = self.encode_call(&[], true);
            if !chunk.is_empty() {
                out.push(chunk);
            }
            if status == AACENC_ENCODE_EOF {
                break;
            }
            guard += 1;
            if guard > 1_000_000 {
                log::warn!("aacEncEncode flush loop guard hit");
                break;
            }
        }
        out
    }
}

impl Encoder for AacEncoder {
    fn content_type(&self) -> &'static str {
        "audio/aac"
    }

    fn encode(&mut self, pcm: &[f32]) -> Vec<u8> {
        self.encode_aus(pcm).concat()
    }

    fn finish(&mut self) -> Vec<u8> {
        self.finish_aus().concat()
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { aacEncClose(&mut self.handle) };
            self.handle = ptr::null_mut();
        }
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
    #[test]
    fn opus_set_title_replaces_initial_tags_only() {
        // Before the headers are flushed, set_title changes the initial title.
        let mut enc = OpusEncoder::new(44100, 2, 128_000, "initial").unwrap();
        enc.set_title("the real first track");
        let pcm = vec![0f32; 4410 * 2];
        let all = [enc.encode(&pcm), enc.encode(&pcm), enc.finish()].concat();
        assert!(String::from_utf8_lossy(&all).contains("title=the real first track"));
        // Exactly one OpusTags packet: the header. set_title never injects
        // mid-stream pages.
        assert_eq!(String::from_utf8_lossy(&all).matches("OpusTags").count(), 1);
        // Page sequence numbers are contiguous.
        let pages = split_pages(&all);
        for (i, p) in pages.iter().enumerate() {
            let seq = u32::from_le_bytes(p[18..22].try_into().unwrap());
            assert_eq!(seq, i as u32, "seq at page {i}");
        }
        // After the first encode, set_title is a no-op.
        enc.set_title("later");
        let all = [enc.encode(&pcm), enc.finish()].concat();
        assert!(!String::from_utf8_lossy(&all).contains("title=later"));
    }

    #[test]
    fn aac_encoder_produces_adts_stream() {
        let mut enc = AacEncoder::new(44100, 2, 128_000).unwrap();
        let mut pcm = vec![0f32; 44100 * 2]; // 1 second
        for (i, s) in pcm.iter_mut().enumerate() {
            let t = i as f64 / 44100.0;
            *s = (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 * 0.5;
        }
        let mut bytes = enc.encode(&pcm);
        bytes.extend_from_slice(&enc.finish());
        assert!(!bytes.is_empty());
        // ADTS frames start with the 0xFFF sync word.
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1] >> 4, 0xF);
    }

    #[test]
    fn he_aac_encoder_produces_adts_stream() {
        let mut enc = AacEncoder::new_he_aac(44100, 2, 64_000).unwrap();
        let mut pcm = vec![0f32; 44100 * 2]; // 1 second
        for (i, s) in pcm.iter_mut().enumerate() {
            let t = i as f64 / 44100.0;
            *s = (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 * 0.5;
        }
        let mut bytes = enc.encode(&pcm);
        bytes.extend_from_slice(&enc.finish());
        assert!(!bytes.is_empty());
        assert_eq!(bytes[0], 0xFF);
        assert_eq!(bytes[1] >> 4, 0xF);
    }

    #[test]
    fn he_aac_stream_carries_sbr_and_decodes() {
        // HE-AAC is AAC-LC plus SBR. In ADTS the SBR shows up as the half-rate
        // core signal (dualrate SBR at 44.1 kHz output -> 22050 Hz, index 7)
        // and the 2048-sample frame, and the stream must decode cleanly.
        // Requires ffmpeg; skipped when absent.
        if std::process::Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_err()
        {
            return;
        }
        let mut enc = AacEncoder::new_he_aac(44100, 2, 64_000).unwrap();
        let mut pcm = vec![0f32; 44100 * 2]; // 1 second
        for (i, s) in pcm.iter_mut().enumerate() {
            let t = i as f64 / 44100.0;
            *s = (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 * 0.5;
        }
        let mut bytes = enc.encode(&pcm);
        bytes.extend_from_slice(&enc.finish());
        assert!(!bytes.is_empty());

        // ADTS fixed header: byte[2] = profile(2) | sf_index(4) | ...
        assert_eq!(bytes[0], 0xFF);
        let sf_index = (bytes[2] >> 2) & 0x0F;
        assert_eq!(
            sf_index, 7,
            "HE-AAC ADTS must signal the 22050 Hz core (index 7), got {sf_index}"
        );

        let path = std::env::temp_dir().join("crabsoup_he_aac_test.aac");
        let _ = std::fs::write(&path, &bytes);
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&path)
            .args(["-f", "null", "-"])
            .output()
            .expect("ffmpeg runs");
        let _ = std::fs::remove_file(&path);
        assert!(
            out.status.success(),
            "HE-AAC stream failed to decode: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn split_pages(bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut pages = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            assert_eq!(&bytes[i..i + 4], b"OggS");
            let nsegs = bytes[i + 26] as usize;
            let segs = &bytes[i + 27..i + 27 + nsegs];
            let body_len: usize = segs.iter().map(|&s| s as usize).sum();
            let page = &bytes[i..i + 27 + nsegs + body_len];
            pages.push(page.to_vec());
            i += page.len();
        }
        assert_eq!(i, bytes.len());
        pages
    }
}

#[test]
fn opus_streams_a_real_48k_mp3_file() {
    // Decode the repo's 48 kHz MP3 through FileSource, encode to Opus, and
    // verify the whole-file stream is valid Ogg/Opus. This exercises the
    // FileSource->OpusEncoder path (downsample 48k -> 44.1k, then upsample
    // 44.1k -> 48k) across many packets.
    let media = std::path::Path::new("media");
    if !media.is_dir() {
        return; // media/ only exists in the repo checkout
    }
    use crate::source::AudioSource;
    let mut files: Vec<_> = std::fs::read_dir(media)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "mp3").unwrap_or(false))
        .collect();
    files.sort();
    let Some(file) = files.first() else { return };

    let spec = symphonia::core::audio::SignalSpec::new(
        44100,
        symphonia::core::audio::Channels::FRONT_LEFT
            | symphonia::core::audio::Channels::FRONT_RIGHT,
    );
    let mut src = crate::source::file::FileSource::open(file, spec, 4096).unwrap();
    let mut enc = OpusEncoder::new(44100, 2, 128_000, "test").unwrap();
    let mut buf = vec![0f32; 4096 * 2];
    let mut all = Vec::new();
    let mut guards = 0u32;
    while guards < 1_000_000 {
        let n = src.next_buffer(&mut buf);
        if n == 0 {
            assert!(src.is_exhausted(), "0 before EOF");
            break;
        }
        all.extend_from_slice(&enc.encode(&buf[..n]));
        guards += 1;
    }
    all.extend_from_slice(&enc.finish());
    // 191+ seconds at 128 kbps is ~3 MB; assert we got a real stream.
    assert!(all.len() > 500_000, "only {} bytes encoded", all.len());
    // The stream must begin with OggS and an OpusHead page.
    assert_eq!(&all[0..4], b"OggS");
    assert!(String::from_utf8_lossy(&all).contains("OpusHead"));
    // Persist for manual inspection with external tools (ffprobe, curl -T).
    if let Some(out) = std::env::var_os("CRABSOUP_DUMP") {
        std::fs::write(out, &all).unwrap();
    }
}
