//! Soundcard capture (Liquidsoap `input.soundcard`) via cpal.
//!
//! cpal's input stream runs its callback on a realtime-priority audio thread,
//! so the callback never blocks or allocates: it only pushes decoded `f32`
//! samples into an SPSC ring (the same bridge shape `live/harbor.rs` uses
//! for the network DJ handoff). The [`AudioSource`] half drains that ring on
//! the pull thread, converts channels, and resamples to the bus spec — the
//! realtime work stays on the OS-managed cpal thread, the heavy conversion
//! stays off it.
//!
//! A small driver thread owns the `cpal::Stream` handle (streams cannot be
//! created portably off the thread that builds them, and must outlive the
//! pull loop), parks on `std::thread::park`, and is woken on drop.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::*};

use crate::Result;
use crate::resample::SincResampler;
use crate::source::{AudioSource, convert_channels};

/// Ring capacity in device *frames* (the ring itself is twice this, so a
/// briefly stalled pull absorbs without dropping; the pull-side cap below
/// keeps live latency bounded).
const RING_FRAMES: usize = 16 * 1024;
/// Stack scratch for the realtime callback: cpal delivers small buffers
/// (tens to a few hundred frames); 2048 samples covers any common period.
const CALLBACK_CHUNK: usize = 2048;

/// Clock-drift compensation (Part G2): the device's hardware sample clock
/// drifts against the bus pacing by tens of PPM over long runs, gradually
/// filling or draining the ring until under/overrun. A proportional control
/// loop on the ring fill nudges the conversion ratio to match: `ppm = gain
/// * error`, clamped. Gain 1e-5 [1/sample] gives a ~2 s time constant at
/// 44.1 kHz (1/(rate*gain)) with a steady-state fill offset of
/// `drift/gain` (~500 samples at 5000 PPM) — bounded well inside the ring.
///
/// The clamp is 1%, far above any real device clock (±20-200 PPM), so the
/// loop only ever hits it on pathological transients.
const DRIFT_GAIN: f64 = 1e-5;
const DRIFT_CLAMP: f64 = 0.01;
/// Smoothing for the fill error (exponentially weighted; ~100 pulls ≈ 4 s
/// at 44.1 kHz): the ring fill oscillates by a full pull's worth between
/// pulls, and that deterministic sawtooth must not jerk the ratio around —
/// a slow EWMA leaves only a few samples of leakage.
const DRIFT_SMOOTHING: f64 = 0.01;

/// Device selection for `input.soundcard({device = ...})`.
#[derive(Clone, Debug, Default)]
pub struct SoundcardInputConfig {
    /// Named device, or the default input device when `None`.
    pub device: Option<String>,
}

/// A pull-based view of the soundcard ring. The driver thread (spawned by
/// [`SoundcardInputSource::open`]) produces device-rate samples; this half
/// converts and resamples them into the bus spec on the pull thread.
pub struct SoundcardInputSource {
    consumer: HeapCons<f32>,
    bus_rate: u32,
    bus_channels: usize,
    device_rate: u32,
    device_channels: usize,
    /// Scratch for one pull's device-rate samples (sized on demand).
    scratch: Vec<f32>,
    /// Resampled output that did not fit the caller's buffer.
    pending: Vec<f32>,
    resampler: SincResampler,
    /// Smoothed ring-fill error vs. target, in samples (Part G2).
    avg_err: f64,
    /// Fractional pull debt in device frames: integer pops would bias
    /// consumption by up to one frame per pull (~500 PPM at 2048-frame
    /// pulls), which would slowly drift the ring off the target.
    consume_debt: f64,
    name: String,
    driver_shutdown: Arc<AtomicBool>,
    driver: Option<std::thread::JoinHandle<()>>,
}

impl SoundcardInputSource {
    /// Open the device and spawn the capture driver thread. The device open
    /// is synchronous (a bounded handshake) so a missing device fails fast.
    pub fn open(config: &SoundcardInputConfig, bus_rate: u32, bus_channels: usize) -> Result<Self> {
        let (producer, consumer) =
            HeapRb::<f32>::new(RING_FRAMES * 2 * bus_channels.max(2)).split();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(String, u32, usize)>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let driver_shutdown = shutdown.clone();
        let thread_shutdown = driver_shutdown.clone();
        let cfg = config.clone();
        let driver = std::thread::spawn(move || {
            let result = start_input_stream(&cfg, producer, thread_shutdown.clone());
            match result {
                Ok((name, rate, chans)) => {
                    let _ = ready_tx.send(Ok((name, rate, chans)));
                    // Park holding the Stream alive; woken by Drop. Process
                    // exit also kills the thread if the source leaks.
                    while !thread_shutdown.load(Ordering::Relaxed) {
                        std::thread::park();
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        });
        let (name, device_rate, device_channels) =
            match ready_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Ok(v)) => v,
                // The driver already exited on its error path; nothing to join.
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    // Handshake timed out: wake the parked driver so it drops
                    // the device stream instead of leaking a parked thread.
                    shutdown.store(true, Ordering::Relaxed);
                    driver.thread().unpark();
                    let _ = driver.join();
                    return Err("input.soundcard: timed out opening the device".into());
                }
            };
        Ok(Self {
            consumer,
            bus_rate,
            bus_channels,
            device_rate,
            device_channels,
            scratch: Vec::new(),
            pending: Vec::new(),
            resampler: SincResampler::new(24, device_rate, bus_rate, bus_channels),
            avg_err: 0.0,
            consume_debt: 0.0,
            name,
            driver_shutdown,
            driver: Some(driver),
        })
    }
}

/// Open the device and build the input stream. Runs on the driver thread, so
/// the `cpal::Stream` never needs to cross threads.
fn start_input_stream(
    config: &SoundcardInputConfig,
    producer: HeapProd<f32>,
    shutdown: Arc<AtomicBool>,
) -> Result<(String, u32, usize)> {
    let host = cpal::default_host();
    let device = match &config.device {
        Some(name) => host
            .input_devices()
            .map_err(|e| format!("input.soundcard: cannot enumerate devices: {e}"))?
            .find(|d| d.name().map(|n| n == *name).unwrap_or(false))
            .ok_or_else(|| format!("input.soundcard: no input device named {name:?}"))?,
        None => host
            .default_input_device()
            .ok_or_else(|| "input.soundcard: no default input device".to_string())?,
    };
    let name = device
        .name()
        .unwrap_or_else(|_| "soundcard input".to_string());
    let supported = device
        .default_input_config()
        .map_err(|e| format!("input.soundcard: no default input config: {e}"))?;
    let device_rate = supported.sample_rate().0;
    let device_channels = supported.channels() as usize;
    let stream_config = supported.config();
    match supported.sample_format() {
        cpal::SampleFormat::F32 => build_input::<f32>(&device, &stream_config, producer, shutdown)?,
        cpal::SampleFormat::I16 => build_input::<i16>(&device, &stream_config, producer, shutdown)?,
        cpal::SampleFormat::U16 => build_input::<u16>(&device, &stream_config, producer, shutdown)?,
        cpal::SampleFormat::I32 => build_input::<i32>(&device, &stream_config, producer, shutdown)?,
        cpal::SampleFormat::F64 => build_input::<f64>(&device, &stream_config, producer, shutdown)?,
        other => {
            return Err(format!(
                "input.soundcard: unsupported sample format {other:?} on {name:?}"
            )
            .into());
        }
    }
    .play()
    .map_err(|e| format!("input.soundcard: cannot start stream: {e}"))?;
    Ok((name, device_rate, device_channels))
}

/// Native sample -> f32, in the realtime callback (same scaling cpal/dasp
/// use; no trait needed from cpal's re-exports).
trait ToF32 {
    fn to_f32(self) -> f32;
}

impl ToF32 for f32 {
    fn to_f32(self) -> f32 {
        self
    }
}
impl ToF32 for i16 {
    fn to_f32(self) -> f32 {
        self as f32 / 32768.0
    }
}
impl ToF32 for u16 {
    fn to_f32(self) -> f32 {
        (self as f32 - 32768.0) / 32768.0
    }
}
impl ToF32 for i32 {
    fn to_f32(self) -> f32 {
        self as f32 / 2_147_483_648.0
    }
}
impl ToF32 for f64 {
    fn to_f32(self) -> f32 {
        self as f32
    }
}

/// Build (but do not play) an input stream for sample type `T`, converting
/// every sample to `f32` in the callback.
fn build_input<T>(
    device: &cpal::Device,
    stream_config: &cpal::StreamConfig,
    mut producer: HeapProd<f32>,
    shutdown: Arc<AtomicBool>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + ToF32,
{
    let err_fn = |e| log::error!("input.soundcard: stream error: {e}");
    let stream = device
        .build_input_stream(
            stream_config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                if shutdown.load(Ordering::Relaxed) {
                    return;
                }
                // Never block or allocate: convert in stack chunks and push
                // what fits. A full ring drops this chunk's tail — the
                // pull-side cap below bounds live latency anyway.
                let mut buf = [0f32; CALLBACK_CHUNK];
                for chunk in data.chunks(CALLBACK_CHUNK) {
                    let n = chunk.len();
                    for (dst, src) in buf[..n].iter_mut().zip(chunk) {
                        *dst = src.to_f32();
                    }
                    producer.push_slice(&buf[..n]);
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("input.soundcard: cannot open device: {e}"))?;
    Ok(stream)
}

impl Drop for SoundcardInputSource {
    fn drop(&mut self) {
        self.driver_shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.driver.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

impl AudioSource for SoundcardInputSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        // Pending resampled output from a previous over-full pull.
        let mut written = 0;
        let take = self.pending.len().min(buffer.len());
        buffer[..take].copy_from_slice(&self.pending[..take]);
        self.pending.drain(..take);
        written += take;
        if written == buffer.len() {
            return written;
        }
        let capacity = buffer.len() - written;

        // Clock-drift compensation (Part G2): estimate the device-clock
        // offset from the smoothed ring-fill error and pull (hence convert)
        // accordingly, so a fast/slow device clock never fills or drains
        // the ring over hours. Positive error (fill above target) means the
        // device is ahead: consume more.
        let cap_samples = self.consumer.capacity().get() / 2;
        let target = cap_samples as f64 / 2.0;
        let fill = self.consumer.occupied_len() as f64;
        self.avg_err = self.avg_err * (1.0 - DRIFT_SMOOTHING) + (fill - target) * DRIFT_SMOOTHING;
        let ppm = (self.avg_err * DRIFT_GAIN).clamp(-DRIFT_CLAMP, DRIFT_CLAMP);
        self.resampler.set_ppm(ppm * 1_000_000.0);

        // Drop-oldest cap: a stalled pull must not play ever-staler audio.
        // Keep only the most recent half of the ring (the live window), so
        // the cap stays consistent however the ring was sized.
        let over = self.consumer.occupied_len().saturating_sub(cap_samples);
        if over > 0 {
            self.consumer.skip(over);
        }

        // Pull just enough device frames to fill the remaining bus capacity
        // (scaled by the drift estimate). A fractional debt keeps the
        // long-run consumption equal to the fractional rate: integer pops
        // would bias it by up to a frame per pull.
        let want_frames = (capacity / self.bus_channels.max(1)) as f64 * self.device_rate as f64
            / self.bus_rate.max(1) as f64
            * (1.0 + ppm);
        self.consume_debt += want_frames;
        let want = self.consume_debt.floor().max(1.0) as usize * self.device_channels;
        if self.scratch.len() != want {
            self.scratch.resize(want, 0.0);
        }
        let n = self.consumer.pop_slice(&mut self.scratch);
        if n == 0 {
            return written;
        }
        self.consume_debt = (self.consume_debt - n as f64 / self.device_channels as f64)
            .clamp(0.0, 2.0 * cap_samples as f64);
        let device = &self.scratch[..n];
        let resampler = &mut self.resampler;
        self.pending.clear();
        if self.device_channels == self.bus_channels {
            if self.device_rate == self.bus_rate {
                // Exact passthrough: same rate + same channels, no filter.
                // The drift estimate is absorbed by pulling (1+ppm) per bus
                // sample; excess input is dropped here, never buffered (a
                // fast device would otherwise fill `pending` forever).
                self.pending.extend_from_slice(device);
                self.pending.truncate(capacity);
            } else {
                self.pending.extend_from_slice(resampler.resample(device));
            }
        } else {
            // Channel mismatch is the exception; convert_channels allocates.
            let converted = convert_channels(device, self.device_channels, self.bus_channels);
            if self.device_rate == self.bus_rate {
                self.pending.extend_from_slice(&converted);
                self.pending.truncate(capacity);
            } else {
                self.pending
                    .extend_from_slice(resampler.resample(&converted));
            }
        }
        let take = self.pending.len().min(capacity);
        buffer[written..written + take].copy_from_slice(&self.pending[..take]);
        self.pending.drain(..take);
        written += take;
        written
    }

    fn is_exhausted(&self) -> bool {
        // A live capture never ends while the device is open.
        false
    }

    fn label(&self) -> Option<String> {
        Some(self.name.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parts-based source (no cpal): the ring + resampler bridge with a
    /// synthetic producer at the device rate, so the conversion math is
    /// testable without hardware. `ring_samples` lets drift tests shrink the
    /// ring so a PPM-level mismatch shows up in seconds instead of hours.
    fn parts_source(
        device_rate: u32,
        device_channels: usize,
        bus_rate: u32,
        bus_channels: usize,
        ring_samples: usize,
    ) -> (SoundcardInputSource, HeapProd<f32>) {
        let (producer, consumer) = HeapRb::<f32>::new(ring_samples).split();
        let src = SoundcardInputSource {
            consumer,
            bus_rate,
            bus_channels,
            device_rate,
            device_channels,
            scratch: Vec::new(),
            pending: Vec::new(),
            resampler: SincResampler::new(24, device_rate, bus_rate, bus_channels),
            avg_err: 0.0,
            consume_debt: 0.0,
            name: "test input".into(),
            driver_shutdown: Arc::new(AtomicBool::new(false)),
            driver: None,
        };
        (src, producer)
    }

    #[test]
    fn same_rate_passthrough_returns_the_captured_samples() {
        let (mut src, mut producer) = parts_source(44_100, 2, 44_100, 2, RING_FRAMES * 2 * 2);
        let samples: Vec<f32> = (0..100).map(|i| i as f32 * 0.01).collect();
        producer.push_slice(&samples);
        let mut buf = vec![0f32; 64];
        let n = src.next_buffer(&mut buf);
        // The drift loop sees a ring far below target (100 of 44100) and
        // scales the pull down by the clamped PPM: 32 frames * 0.99 = 31.
        assert_eq!(n, 62);
        // The ring held 100 samples; the pull consumes the front.
        for (i, &s) in buf[..n].iter().enumerate() {
            assert!((s - i as f32 * 0.01).abs() < 1e-6, "sample {i}: {s}");
        }
    }

    /// Drift-simulation harness: a thread feeds the ring at
    /// `device_rate * (1 + skew/1e6)` samples/sec (the real device clock,
    /// anchored to wall time so sleep overshoot cannot skew the rate);
    /// this thread pulls `pull_frames` bus samples at the bus rate (the
    /// engine's wall clock). The ring is pre-filled to the drift loop's
    /// steady-state fill (`target + skew/gain`, in samples) so the test
    /// exercises the tracking regime, not the ~20 s saturation warmup.
    /// Returns the ring fill sampled every 0.5 s after `settle` seconds and
    /// the source's converged PPM estimate (from the resampler's step).
    #[allow(clippy::too_many_arguments)]
    fn run_drift(
        src: &mut SoundcardInputSource,
        mut producer: HeapProd<f32>,
        device_rate: u32,
        skew_ppm: f64,
        bus_rate: u32,
        pull_frames: usize,
        seconds: f64,
        settle: f64,
    ) -> (Vec<usize>, f64) {
        let target = producer.capacity().get() / 4;
        let steady = (target as f64 + skew_ppm / 10.0).max(0.0) as usize;
        let prefill = vec![0.5f32; steady.min(producer.capacity().get())];
        producer.push_slice(&prefill);
        let done = Arc::new(AtomicBool::new(false));
        let done_flag = done.clone();
        let produced_total = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let produced_flag = produced_total.clone();
        let device_rate = device_rate as f64;
        std::thread::spawn(move || {
            // Clock-anchored conversion: the device hands over samples at
            // `rate * (1 + skew)` per wall second. Sleeping a fixed 5 ms and
            // accumulating would be skewed by sleep overshoot (~2 %), which
            // would swamp a 1000 PPM test skew.
            let start = std::time::Instant::now();
            let mut produced = 0u64;
            while !done_flag.load(Ordering::Relaxed) {
                let t = start.elapsed().as_secs_f64();
                let target = (t * device_rate * (1.0 + skew_ppm / 1e6)) as u64;
                if target > produced {
                    let n = (target - produced) as usize;
                    let chunk = vec![0.5f32; n];
                    let mut rest = &chunk[..];
                    while !rest.is_empty() {
                        let k = producer.push_slice(rest);
                        if k == 0 {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        rest = &rest[k..];
                    }
                    produced = target;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            produced_flag.store(produced, Ordering::Relaxed);
        });

        let mut buf = vec![0f32; pull_frames];
        let pull_period = pull_frames as f64 / bus_rate as f64;
        let start = std::time::Instant::now();
        let mut next_due = Duration::ZERO;
        let mut fills = Vec::new();
        let mut last_sample = -1.0f64;
        let mut total_out = 0usize;
        let mut pulls = 0usize;
        loop {
            let elapsed = start.elapsed();
            if elapsed.as_secs_f64() >= seconds {
                break;
            }
            if elapsed >= next_due {
                let n = src.next_buffer(&mut buf);
                total_out += n;
                pulls += 1;
                next_due += Duration::from_secs_f64(pull_period);
            } else {
                std::thread::sleep(Duration::from_millis(1));
            }
            let t = elapsed.as_secs_f64();
            if t >= settle && t - last_sample >= 0.5 {
                last_sample = t;
                fills.push(src.consumer.occupied_len());
            }
        }
        done.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(20));
        let produced = produced_total.load(Ordering::Relaxed) as f64 / seconds;
        let bus_out_rate = total_out as f64 / seconds;
        let pull_rate = pulls as f64 / seconds;
        eprintln!(
            "drift trace: produced={produced:.0}/s bus_out={bus_out_rate:.0}/s pulls={pull_rate:.1}/s ppm={} fills={fills:?}",
            src.resampler.ppm()
        );
        (fills, src.resampler.ppm())
    }

    #[test]
    fn drift_control_tracks_a_fast_device_clock() {
        // 44.1 kHz device -> 48 kHz bus (resampler path), device clock
        // 1000 PPM fast (+44 samples/sec). Ring 32768 (target 8192 >> one
        // pull's ~1882 samples, like production): without compensation the
        // fill would drift over hours, never empty, never full — just
        // wrong. The loop must converge its PPM estimate to the skew and
        // hold the fill mid-ring, never near empty or the drop-oldest cap.
        let (mut src, producer) = parts_source(44_100, 1, 48_000, 1, 32_768);
        let (fills, ppm) = run_drift(&mut src, producer, 44_100, 1000.0, 48_000, 2048, 25.0, 8.0);
        assert!(
            (ppm - 1000.0).abs() < 600.0,
            "PPM estimate {ppm} must track the +1000 PPM skew"
        );
        // The fill saws between pulls (one pull's worth of input, ~1882);
        // assert the envelope: never near empty, never near the drop-oldest
        // cap (16 384). The converged PPM estimate is the real discriminator.
        let lo = *fills.iter().min().expect("fill samples");
        let hi = *fills.iter().max().expect("fill samples");
        assert!(lo > 100, "fill dipped near empty: {lo}");
        assert!(hi < 16_000, "fill {hi} hit the drop-oldest cap");
    }

    #[test]
    fn drift_control_tracks_a_slow_device_clock_in_passthrough() {
        // Same rate both sides (passthrough: want-scaling + truncate),
        // device 1000 PPM slow. The consumption must back off or the ring
        // drains to underrun; the PPM estimate converges negative.
        let (mut src, producer) = parts_source(44_100, 1, 44_100, 1, 32_768);
        let (fills, ppm) = run_drift(&mut src, producer, 44_100, -1000.0, 44_100, 2048, 25.0, 8.0);
        assert!(
            (ppm + 1000.0).abs() < 600.0,
            "PPM estimate {ppm} must track the -1000 PPM skew"
        );
        let lo = *fills.iter().min().expect("fill samples");
        let hi = *fills.iter().max().expect("fill samples");
        assert!(lo > 100, "fill dipped near empty: {lo}");
        assert!(hi < 16_000, "fill {hi} hit the drop-oldest cap");
    }

    #[test]
    fn upsamples_to_the_bus_rate() {
        // 22.05 kHz mono device -> 44.1 kHz stereo bus: half-rate doubles
        // the frame count and mono duplicates across channels. The ring is
        // fed in small chunks at capture pace (a full second pushed at once
        // would trip the drop-oldest latency cap, which is correct live
        // behaviour).
        let (mut src, mut producer) = parts_source(22_050, 1, 44_100, 2, RING_FRAMES * 2);
        let rate = 22_050.0;
        let mut i = 0usize;
        let mut total = 0usize;
        let mut peak = 0.0f32;
        let mut buf = vec![0f32; 4096 * 2];
        loop {
            // Top the ring up with the next ~0.2 s of device audio.
            while producer.occupied_len() < 4096 && i < 22_050 {
                let n = (22_050 - i).min(1024);
                let chunk: Vec<f32> = (0..n)
                    .map(|k| {
                        let t = (i + k) as f64 / rate;
                        (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32 * 0.5
                    })
                    .collect();
                producer.push_slice(&chunk);
                i += n;
            }
            if i >= 22_050 && producer.occupied_len() == 0 {
                break;
            }
            let n = src.next_buffer(&mut buf);
            if n > 0 {
                total += n;
                peak = peak.max(buf[..n].iter().fold(0.0, |m, &s| m.max(s.abs())));
            }
        }
        // One second of mono 22.05 kHz -> one second of stereo 44.1 kHz.
        assert!(
            total >= 87_000,
            "one second of stereo bus audio, got {total}"
        );
        assert!(peak > 0.3, "resampled tone collapsed: peak {peak}");
        assert!(peak < 0.6, "resampled tone overshot: peak {peak}");
    }

    #[test]
    fn drops_the_stale_window_when_the_consumer_lags() {
        let (mut src, mut producer) = parts_source(44_100, 2, 44_100, 2, RING_FRAMES * 2 * 2);
        // More than the pull-side cap: the stale front is skipped and only
        // the most recent window survives.
        let total = 33_000usize;
        let samples: Vec<f32> = (0..total).map(|i| i as f32).collect();
        producer.push_slice(&samples);
        let mut buf = vec![0f32; 16_384];
        let mut first: Option<f32> = None;
        let mut last: Option<f32> = None;
        loop {
            let n = src.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            if first.is_none() {
                first = Some(buf[0]);
            }
            last = Some(buf[n - 1]);
        }
        // cap = RING_FRAMES * 2 ch = 32768 samples: the oldest 232 are stale.
        assert_eq!(
            first,
            Some((total - RING_FRAMES * 2) as f32),
            "oldest dropped"
        );
        assert_eq!(last, Some((total - 1) as f32), "newest survives");
    }
}
