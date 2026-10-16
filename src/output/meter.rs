//! `output.meter`: EBU R128 loudness and the per-channel spectrum of what goes on air,
//! served at the meter's path on the HTTP control port.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use ebur128::{EbuR128, Mode};
use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use tokio::sync::broadcast;

use crate::Result;
use crate::engine::tap::{AudioFrame, recv_frame_or_shutdown};

const BANDS: usize = 16;
const MIN_HZ: f64 = 30.0;
const MAX_HZ: f64 = 10_000.0;
const WINDOW: usize = 4096;
const SPECTRUM_PER_SECOND: u32 = 30;
const LEVELS_PER_SECOND: u32 = 10;
// Short-term loudness sums its whole 3 s window at each reading: once a second is enough
// for a 3 s average.
const SHORT_TERM_EVERY: usize = 10;
// A reload of the dashboard keeps the integrated loudness: it starts over after this long
// with nobody watching.
const IDLE_RESET: Duration = Duration::from_secs(10);

/// Server-sent events: `levels` ten times a second, `spectrum` thirty times a second,
/// measured only while someone listens.
#[derive(Clone)]
pub struct Meter {
    events: broadcast::Sender<Arc<str>>,
}

impl Default for Meter {
    fn default() -> Self {
        Self {
            events: broadcast::channel(64).0,
        }
    }
}

impl Meter {
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.events.subscribe()
    }

    pub fn run(
        &self,
        rx: Receiver<Arc<AudioFrame>>,
        rate: u32,
        chans: usize,
        shutdown: Arc<AtomicBool>,
    ) -> Result<()> {
        let mut analysis: Option<Analysis> = None;
        let mut idle_since: Option<Instant> = None;
        while let Some(frame) = recv_frame_or_shutdown(&rx, &shutdown) {
            if self.events.receiver_count() == 0 {
                if idle_since.get_or_insert_with(Instant::now).elapsed() > IDLE_RESET {
                    analysis = None;
                }
                continue;
            }
            idle_since = None;
            let analysis = match analysis.as_mut() {
                Some(analysis) => analysis,
                None => analysis.insert(Analysis::new(rate, chans)?),
            };
            for event in analysis.add(&frame.pcm)? {
                // No receiver left since the count above: the next frame finds out.
                let _ = self.events.send(event);
            }
        }
        Ok(())
    }
}

struct Analysis {
    loudness: EbuR128,
    chans: usize,
    rings: Vec<Vec<f32>>,
    frames: usize,
    spectrum_every: usize,
    levels_every: usize,
    true_peak: f64,
    levels_sent: usize,
    short_term: f64,
    fft: Arc<dyn Fft<f32>>,
    hann: Vec<f32>,
    edges: Vec<usize>,
    scratch: Vec<Complex<f32>>,
}

impl Analysis {
    fn new(rate: u32, chans: usize) -> Result<Self> {
        let mode = Mode::M | Mode::S | Mode::I | Mode::LRA | Mode::TRUE_PEAK | Mode::HISTOGRAM;
        let edges = (0..=BANDS)
            .map(|i| {
                let hz = MIN_HZ * (MAX_HZ / MIN_HZ).powf(i as f64 / BANDS as f64);
                (hz * WINDOW as f64 / rate as f64).round() as usize
            })
            .collect();
        Ok(Self {
            loudness: EbuR128::new(chans as u32, rate, mode)?,
            chans,
            rings: vec![vec![0.0; WINDOW]; chans],
            frames: 0,
            spectrum_every: (rate / SPECTRUM_PER_SECOND) as usize,
            levels_every: (rate / LEVELS_PER_SECOND) as usize,
            true_peak: 0.0,
            levels_sent: 0,
            short_term: f64::NEG_INFINITY,
            fft: FftPlanner::new().plan_fft_forward(WINDOW),
            hann: (0..WINDOW)
                .map(|i| {
                    0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (WINDOW - 1) as f32).cos()
                })
                .collect(),
            edges,
            scratch: vec![Complex::default(); WINDOW],
        })
    }

    /// Feeds interleaved PCM and returns the events due within it, in order.
    fn add(&mut self, pcm: &[f32]) -> Result<Vec<Arc<str>>> {
        let mut events = Vec::new();
        let mut rest = pcm;
        while !rest.is_empty() {
            let to_spectrum = self.spectrum_every - self.frames % self.spectrum_every;
            let to_levels = self.levels_every - self.frames % self.levels_every;
            let frames = (rest.len() / self.chans).min(to_spectrum).min(to_levels);
            let (chunk, after) = rest.split_at(frames * self.chans);
            rest = after;
            self.loudness.add_frames_f32(chunk)?;
            for ch in 0..self.chans {
                self.true_peak = self.true_peak.max(self.loudness.prev_true_peak(ch as u32)?);
            }
            for (i, frame) in chunk.chunks_exact(self.chans).enumerate() {
                for (ring, sample) in self.rings.iter_mut().zip(frame) {
                    ring[(self.frames + i) % WINDOW] = *sample;
                }
            }
            self.frames += frames;
            if self.frames.is_multiple_of(self.levels_every) {
                events.push(self.levels()?);
            }
            if self.frames.is_multiple_of(self.spectrum_every) && self.frames >= WINDOW {
                events.push(self.spectrum());
            }
        }
        Ok(events)
    }

    /// Loudness in LUFS (null while too quiet to measure; short-term read once a second), its
    /// range in LU, and the louder channel's true peak since the last event in dBFS (null in
    /// silence).
    fn levels(&mut self) -> Result<Arc<str>> {
        if self.levels_sent.is_multiple_of(SHORT_TERM_EVERY) {
            self.short_term = self.loudness.loudness_shortterm()?;
        }
        self.levels_sent += 1;
        let data = serde_json::json!({
            "momentary": self.loudness.loudness_momentary()?,
            "shortTerm": self.short_term,
            "integrated": self.loudness.loudness_global()?,
            "range": self.loudness.loudness_range()?,
            "truePeak": 20.0 * self.true_peak.log10(),
        });
        self.true_peak = 0.0;
        Ok(format!("event: levels\ndata: {data}\n\n").into())
    }

    /// Each channel's band levels (0-255) over the last [`WINDOW`] frames.
    fn spectrum(&mut self) -> Arc<str> {
        let start = self.frames % WINDOW;
        let mut channels = Vec::with_capacity(self.chans);
        for ring in &self.rings {
            for (i, bin) in self.scratch.iter_mut().enumerate() {
                *bin = Complex::new(ring[(start + i) % WINDOW] * self.hann[i], 0.0);
            }
            self.fft.process(&mut self.scratch);
            let bands: Vec<u8> = (0..BANDS)
                .map(|band| {
                    let (low, high) = (self.edges[band], self.edges[band + 1]);
                    let peak = (low..=low.max(high.saturating_sub(1)))
                        .map(|bin| self.scratch[bin].norm())
                        .fold(0.0f32, f32::max);
                    let db = 20.0 * (peak / (WINDOW / 4) as f32 + 1e-9).log10();
                    (((db + 66.0) / 60.0).clamp(0.0, 1.0) * 255.0).round() as u8
                })
                .collect();
            channels.push(bands);
        }
        format!("event: spectrum\ndata: {}\n\n", serde_json::json!(channels)).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two seconds of a stereo 1 kHz sine at `amplitude`, interleaved.
    fn tone(amplitude: f32) -> Vec<f32> {
        (0..88_200)
            .flat_map(|i| {
                let s =
                    amplitude * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 44_100.0).sin();
                [s, s]
            })
            .collect()
    }

    fn last(events: &[Arc<str>], kind: &str) -> serde_json::Value {
        let event = events
            .iter()
            .rev()
            .find(|e| e.starts_with(&format!("event: {kind}\n")))
            .unwrap();
        serde_json::from_str(event.lines().nth(1).unwrap().trim_start_matches("data: ")).unwrap()
    }

    #[test]
    fn a_tone_at_minus_6_dbfs_on_both_channels_reads_minus_6_lufs_and_peaks_at_minus_6() {
        let mut analysis = Analysis::new(44_100, 2).unwrap();

        let events = analysis.add(&tone(0.5)).unwrap();

        let levels = last(&events, "levels");
        assert!(
            (levels["momentary"].as_f64().unwrap() + 6.0).abs() < 0.2,
            "{levels}"
        );
        assert!(
            (levels["truePeak"].as_f64().unwrap() + 6.0).abs() < 0.2,
            "{levels}"
        );
    }

    #[test]
    fn silence_reads_as_no_loudness_and_no_peak() {
        let mut analysis = Analysis::new(44_100, 2).unwrap();

        let events = analysis.add(&tone(0.0)).unwrap();

        let levels = last(&events, "levels");
        assert!(levels["momentary"].is_null(), "{levels}");
        assert!(levels["truePeak"].is_null(), "{levels}");
    }

    #[test]
    fn a_1_khz_tone_lights_the_band_holding_1_khz_the_most() {
        let mut analysis = Analysis::new(44_100, 2).unwrap();

        let events = analysis.add(&tone(0.5)).unwrap();

        // 1 kHz sits in band 9: 30 Hz x (10 kHz / 30 Hz)^(9/16) ~ 789 Hz to ~ 1.13 kHz.
        let left: Vec<u64> = last(&events, "spectrum")[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap())
            .collect();
        let loudest = (0..BANDS).max_by_key(|&band| left[band]).unwrap();
        assert_eq!(loudest, 9, "{left:?}");
    }
}
