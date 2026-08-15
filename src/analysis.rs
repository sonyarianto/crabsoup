//! Offline audio analysis: BPM (tempo) and musical key estimation.
//!
//! Both run FFT-based DSP over a fully decoded mono buffer, so they are
//! analysis-time only (no bus/effect involvement). BPM: a spectral-flux
//! onset envelope (Hann-windowed magnitude spectra, positive frame
//! differences) is autocorrelated over the lag range that maps to
//! 60–200 BPM; the best mean-subtracted correlation lag gives the tempo,
//! with an octave-suppression step preferring the fundamental period.
//! Key: magnitude spectra fold into a 12-bin chroma (bins mapped to pitch
//! classes by their centre frequency, per-window L1-normalised), which is
//! correlated against all 24 rotations of the Krumhansl–Kessler major/minor
//! profiles; the best rotation names the key.

use std::path::Path;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const BPM_MIN: f32 = 60.0;
const BPM_MAX: f32 = 200.0;
const ONSET_WIN: usize = 1024;
const ONSET_HOP: usize = 512;
const CHROMA_WIN: usize = 4096;
const CHROMA_HOP: usize = 2048;
const CHROMA_FLOOR_HZ: f32 = 55.0;
const CHROMA_CEIL_HZ: f32 = 4_000.0;

/// Krumhansl–Kessler key profiles, indexed from C.
const KK_MAJOR: [f32; 12] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const KK_MINOR: [f32; 12] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];
const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// Decode `path` to interleaved f32 samples at its native rate.
/// `ctx` prefixes error strings (operator name).
pub fn decode_file(path: &str, ctx: &str) -> crate::Result<(Vec<f32>, u32, usize)> {
    let file = std::fs::File::open(path).map_err(|e| format!("{ctx}: {path}: {e}"))?;
    let mut hint = Hint::new();
    if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let mss = MediaSourceStream::new(
        Box::new(symphonia::core::io::ReadOnlySource::new(file)),
        MediaSourceStreamOptions::default(),
    );
    let mut probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("{ctx}: {path}: cannot probe: {e}"))?;
    let track = probed
        .format
        .default_track()
        .cloned()
        .ok_or_else(|| format!("{ctx}: {path}: no audio track"))?;
    let track_id = track.id;
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("{ctx}: {path}: {e}"))?;

    let mut interleaved: Vec<f32> = Vec::new();
    let mut spec = None;
    while let Ok(packet) = probed.format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                log::warn!("{ctx}: {path}: skipping packet: {e}");
                continue;
            }
        };
        let s = *decoded.spec();
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let mut buf = SampleBuffer::<f32>::new(frames as u64, s);
        buf.copy_interleaved_ref(decoded);
        interleaved.extend_from_slice(buf.samples());
        if spec.is_none() {
            spec = Some(s);
        }
    }
    let spec = spec.ok_or_else(|| format!("{ctx}: {path}: no audio decoded"))?;
    Ok((interleaved, spec.rate, spec.channels.count().max(1)))
}

fn to_mono(interleaved: &[f32], channels: usize) -> Vec<f32> {
    if channels == 1 {
        return interleaved.to_vec();
    }
    interleaved
        .chunks_exact(channels)
        .map(|f| f.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// Spectral-flux onset envelope: sum of positive magnitude differences
/// between successive Hann-windowed spectra.
fn onset_envelope(mono: &[f32]) -> Vec<f32> {
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(ONSET_WIN);
    let mut scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let mut window = vec![Complex::new(0.0, 0.0); ONSET_WIN];
    let mut prev_mag = vec![0.0f32; ONSET_WIN / 2];
    let mut flux = Vec::new();
    let mut frame = 0;
    while frame + ONSET_WIN <= mono.len() {
        for (i, slot) in window.iter_mut().enumerate() {
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / ONSET_WIN as f32).cos();
            *slot = Complex::new(mono[frame + i] * w, 0.0);
        }
        fft.process_with_scratch(&mut window, &mut scratch);
        let mut sum = 0.0f32;
        for (k, slot) in window[..ONSET_WIN / 2].iter().enumerate() {
            let mag = slot.norm();
            sum += (mag - prev_mag[k]).max(0.0);
            prev_mag[k] = mag;
        }
        flux.push(sum);
        frame += ONSET_HOP;
    }
    flux
}

/// Estimate the tempo of an interleaved buffer in BPM.
pub fn bpm(interleaved: &[f32], channels: usize, rate: u32) -> Result<f32, String> {
    let mono = to_mono(interleaved, channels);
    let flux = onset_envelope(&mono);
    if flux.len() < 64 {
        return Err("bpm: signal too short to analyse".into());
    }
    let hop_rate = rate as f32 / ONSET_HOP as f32;
    let min_lag = (hop_rate * 60.0 / BPM_MAX).round().max(1.0) as usize;
    let max_lag = (hop_rate * 60.0 / BPM_MIN)
        .ceil()
        .min(flux.len() as f32 - 1.0) as usize;
    let mean = flux.iter().sum::<f32>() / flux.len() as f32;

    let mut best = (0usize, 0.0f32);
    for lag in min_lag..=max_lag {
        let (mut num, mut den_a, mut den_b) = (0.0f32, 0.0f32, 0.0f32);
        for i in 0..flux.len() - lag {
            let a = flux[i] - mean;
            let b = flux[i + lag] - mean;
            num += a * b;
            den_a += a * a;
            den_b += b * b;
        }
        if den_a < 1e-6 || den_b < 1e-6 {
            continue;
        }
        let score = num / (den_a * den_b).sqrt();
        if score > best.1 {
            best = (lag, score);
        }
    }
    if best.1 < 0.1 {
        return Err("bpm: no steady tempo found".into());
    }
    // Octave suppression: if an exact divisor of the winning lag correlates
    // almost as well, the signal is pulsing at the faster rate too.
    let mut lag = best.0;
    let mut k = 2;
    while lag.is_multiple_of(k) {
        let d = lag / k;
        let score = autocorr_at(&flux, mean, d);
        if score > best.1 * 0.85 {
            lag = d;
        } else {
            break;
        }
        k *= 2;
    }
    Ok(hop_rate * 60.0 / lag as f32)
}

fn autocorr_at(flux: &[f32], mean: f32, lag: usize) -> f32 {
    let (mut num, mut den_a, mut den_b) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..flux.len() - lag {
        let a = flux[i] - mean;
        let b = flux[i + lag] - mean;
        num += a * b;
        den_a += a * a;
        den_b += b * b;
    }
    if den_a < 1e-6 || den_b < 1e-6 {
        return 0.0;
    }
    num / (den_a * den_b).sqrt()
}

/// Normalised 12-bin chromagram of a mono buffer.
fn chromagram(mono: &[f32], rate: u32) -> [f32; 12] {
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(CHROMA_WIN);
    let mut scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let mut window = vec![Complex::new(0.0, 0.0); CHROMA_WIN];
    let mut chroma = [0.0f32; 12];
    let mut frames = 0u32;
    let mut frame = 0;
    while frame + CHROMA_WIN <= mono.len() {
        for (i, slot) in window.iter_mut().enumerate() {
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / CHROMA_WIN as f32).cos();
            *slot = Complex::new(mono[frame + i] * w, 0.0);
        }
        fft.process_with_scratch(&mut window, &mut scratch);
        let mut frame_chroma = [0.0f32; 12];
        for (k, slot) in window[1..CHROMA_WIN / 2].iter().enumerate() {
            let f = (k + 1) as f32 * rate as f32 / CHROMA_WIN as f32;
            if !(CHROMA_FLOOR_HZ..=CHROMA_CEIL_HZ).contains(&f) {
                continue;
            }
            let pc = (12.0 * (f / 440.0).log2() + 69.0).round() as i32;
            frame_chroma[pc.rem_euclid(12) as usize] += slot.norm();
        }
        let l1 = frame_chroma.iter().sum::<f32>();
        if l1 > 1e-9 {
            for (c, v) in chroma.iter_mut().zip(&frame_chroma) {
                *c += v / l1;
            }
            frames += 1;
        }
        frame += CHROMA_HOP;
    }
    if frames > 0 {
        for c in &mut chroma {
            *c /= frames as f32;
        }
    }
    chroma
}

/// Estimate the musical key of an interleaved buffer, e.g. `"A major"`.
pub fn key(interleaved: &[f32], channels: usize, rate: u32) -> Result<String, String> {
    let mono = to_mono(interleaved, channels);
    let chroma = chromagram(&mono, rate);
    if chroma.iter().all(|&c| c == 0.0) {
        return Err("key: signal is silent".into());
    }
    let mut best = (0usize, 0.0f32);
    for root in 0..12 {
        for (mode, profile) in [(0, &KK_MAJOR), (1, &KK_MINOR)] {
            let mut score = 0.0f32;
            for i in 0..12 {
                score += chroma[i] * profile[(i + 12 - root) % 12];
            }
            if score > best.1 {
                best = (root + mode * 12, score);
            }
        }
    }
    let root = best.0 % 12;
    let mode = if best.0 / 12 == 0 { "major" } else { "minor" };
    Ok(format!("{} {}", NOTE_NAMES[root], mode))
}

/// Synthetic PCM16 WAV fixtures shared by tests and benches: a click track
/// at a given BPM (40 ms of 1 kHz per beat) and a chord-progression "song"
/// in a major key.
pub mod fixtures {
    /// Mono PCM16 WAV at 44.1 kHz with a click (40 ms of 1 kHz) every
    /// `60/bpm` seconds.
    pub fn click_wav_bytes(bpm: f64, seconds: f64) -> Vec<u8> {
        let rate = 44_100u32;
        let total = (seconds * rate as f64) as usize;
        let click_len = (0.04 * rate as f64) as usize;
        let period = (60.0 / bpm * rate as f64).round() as usize;
        let mut samples: Vec<i16> = vec![0; total];
        let mut t = 0usize;
        while t + click_len < total {
            for i in 0..click_len {
                let x = 2.0 * std::f32::consts::PI * 1_000.0 * i as f32 / rate as f32;
                samples[t + i] = (0.8 * x.sin() * i16::MAX as f32) as i16;
            }
            t += period;
        }
        wav16_mono(&samples, rate)
    }

    /// Note frequency from a MIDI note number.
    fn midi_freq(n: u8) -> f32 {
        440.0 * 2f32.powf((n as f32 - 69.0) / 12.0)
    }

    /// A synthetic "song" in a major key: the I–vi–IV–V progression (2 s
    /// per chord) with an eighth-note melody that anchors the tonic, so the
    /// chroma is unambiguous. `tonic` is a MIDI note number.
    pub fn song_wav_bytes(tonic: u8, seconds: f64) -> Vec<u8> {
        let rate = 44_100u32;
        let total = (seconds * rate as f64) as usize;
        let major = [0, 4, 7];
        let submediant = [9, 12, 16];
        let subdominant = [5, 9, 12];
        let dominant = [7, 11, 14];
        let chords = [major, submediant, subdominant, dominant];
        let melody_degrees = [0, 2, 4, 7, 9, 11, 12, 11];
        let note_len = 0.5 * rate as f64;
        let mut samples: Vec<i16> = vec![0; total];
        for (i, sample) in samples.iter_mut().enumerate() {
            let t = i as f32 / rate as f32;
            let secs = t as f64;
            let chord = chords[(secs as usize / 2) % 4];
            let mut x = 0.0f32;
            for d in chord {
                x += 0.12 * (2.0 * std::f32::consts::PI * midi_freq(tonic + d) * t).sin();
            }
            let m = melody_degrees[((secs / note_len) as usize) % 8];
            let amp = if m == 0 { 0.3 } else { 0.2 };
            x += amp * (2.0 * std::f32::consts::PI * midi_freq(tonic + m) * t).sin();
            *sample = (x * i16::MAX as f32) as i16;
        }
        wav16_mono(&samples, rate)
    }

    pub fn wav16_mono(samples: &[i16], rate: u32) -> Vec<u8> {
        let data_len = (samples.len() * 2) as u32;
        let mut out = Vec::with_capacity(44 + data_len as usize);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
pub mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn bpm_of_a_120bpm_click_track() {
        let wav = click_wav_bytes(120.0, 30.0);
        let (samples, rate, chans) = decode_file_from(&wav);
        let b = bpm(&samples, chans, rate).unwrap();
        assert!((b - 120.0).abs() < 2.0, "tempo {b}");
    }

    #[test]
    fn bpm_of_a_90bpm_click_track() {
        let wav = click_wav_bytes(90.0, 30.0);
        let (samples, rate, chans) = decode_file_from(&wav);
        let b = bpm(&samples, chans, rate).unwrap();
        assert!((b - 90.0).abs() < 2.0, "tempo {b}");
    }

    #[test]
    fn bpm_needs_enough_audio() {
        let wav = click_wav_bytes(120.0, 0.2);
        let (samples, rate, chans) = decode_file_from(&wav);
        let err = bpm(&samples, chans, rate).unwrap_err();
        assert!(err.contains("bpm"), "{err}");
    }

    #[test]
    fn bpm_rejects_silence() {
        let (samples, rate, chans) =
            decode_file_from(&wav16_mono(&vec![0i16; 30 * 44_100], 44_100));
        let err = bpm(&samples, chans, rate).unwrap_err();
        assert!(err.contains("no steady tempo"), "{err}");
    }

    #[test]
    fn key_of_an_a_major_song() {
        let wav = song_wav_bytes(69, 16.0);
        let (samples, rate, chans) = decode_file_from(&wav);
        assert_eq!(key(&samples, chans, rate).unwrap(), "A major");
    }

    #[test]
    fn key_of_a_c_major_song() {
        let wav = song_wav_bytes(60, 16.0);
        let (samples, rate, chans) = decode_file_from(&wav);
        assert_eq!(key(&samples, chans, rate).unwrap(), "C major");
    }

    #[test]
    fn key_rejects_silence() {
        let (samples, rate, chans) = decode_file_from(&wav16_mono(&vec![0i16; 44_100 * 2], 44_100));
        let err = key(&samples, chans, rate).unwrap_err();
        assert!(err.contains("key"), "{err}");
    }

    /// Symmetric to `decode_file` but from an in-memory WAV (probe accepts
    /// any extension hint; pass no hint — symphonia probes WAV fine).
    fn decode_file_from(wav: &[u8]) -> (Vec<f32>, u32, usize) {
        let mss = MediaSourceStream::new(
            Box::new(std::io::Cursor::new(wav.to_vec())),
            MediaSourceStreamOptions::default(),
        );
        let mut probed = symphonia::default::get_probe()
            .format(
                &Hint::new(),
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            )
            .expect("probe wav");
        let track = probed.format.default_track().cloned().unwrap();
        let track_id = track.id;
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())
            .unwrap();
        let mut interleaved = Vec::new();
        let mut spec = None;
        while let Ok(packet) = probed.format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }
            let decoded = decoder.decode(&packet).unwrap();
            let s = *decoded.spec();
            let mut buf = SampleBuffer::<f32>::new(decoded.frames() as u64, s);
            buf.copy_interleaved_ref(decoded);
            interleaved.extend_from_slice(buf.samples());
            if spec.is_none() {
                spec = Some(s);
            }
        }
        let spec = spec.unwrap();
        (interleaved, spec.rate, spec.channels.count().max(1))
    }
}
