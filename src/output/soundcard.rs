//! Soundcard playback (Liquidsoap `output.soundcard`) via cpal.
//!
//! A tap-consumer thread resamples bus frames to the device rate, converts
//! channels, and pushes into an SPSC ring; cpal's output callback (realtime,
//! never blocking or allocating) drains the ring and writes the device.
//! Underruns write silence. The resampling happens on the consumer thread,
//! never inside the realtime callback.
//!
//! `cpal::Stream` is `!Send` on some platforms (ALSA holds a raw pointer),
//! so a small driver thread owns the stream: it opens the device, builds the
//! stream with the ring's consumer half, then parks (the same shape as
//! `src/source/soundcard.rs`). Only the `HeapProd` half crosses threads.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::*};

use crate::Result;
use crate::config::SoundcardOutputConfig;
use crate::engine::tap::{AudioFrame, recv_frame_or_shutdown};
use crate::resample::SincResampler;

/// Ring capacity in device frames (double-buffered against the callback).
const RING_FRAMES: usize = 16 * 1024;
/// Stack scratch for the realtime callback.
const CALLBACK_CHUNK: usize = 2048;

/// Clock-drift compensation (Part G2): same proportional loop as the input
/// side, but driving the resampler step the other way — a device clock that
/// runs fast drains the ring faster, so the fill error goes negative and the
/// step shrinks (more output per input). The converged PPM estimate is the
/// negative of the skew (an output device 1000 PPM fast reads -1000).
/// Parameters are shared with `src/source/soundcard.rs`.
const DRIFT_GAIN: f64 = 1e-5;
const DRIFT_CLAMP: f64 = 0.01;
const DRIFT_SMOOTHING: f64 = 0.01;

/// Consumes frames from the engine tap and plays them on the default (or a
/// named) soundcard. The device and stream are created in [`SoundcardOutput::connect`]
/// so a missing device fails at startup, before the tap pulls anything.
pub struct SoundcardOutput {
    config: SoundcardOutputConfig,
    rx: Receiver<Arc<AudioFrame>>,
    bus_rate: u32,
    bus_channels: usize,
    device_rate: u32,
    device_channels: usize,
    producer: Option<HeapProd<f32>>,
    /// Reusable channel-conversion scratch.
    channel_buf: Vec<f32>,
    resampler: Option<SincResampler>,
    /// Smoothed ring-fill error vs. target, in samples (Part G2).
    avg_err: f64,
    shutdown: Arc<AtomicBool>,
    driver_shutdown: Arc<AtomicBool>,
    driver: Option<std::thread::JoinHandle<()>>,
}

impl SoundcardOutput {
    pub fn new(
        config: SoundcardOutputConfig,
        rx: Receiver<Arc<AudioFrame>>,
        bus_rate: u32,
        bus_channels: usize,
    ) -> Self {
        Self {
            config,
            rx,
            bus_rate,
            bus_channels,
            device_rate: 0,
            device_channels: 0,
            producer: None,
            channel_buf: Vec::new(),
            resampler: None,
            avg_err: 0.0,
            shutdown: Arc::new(AtomicBool::new(false)),
            driver_shutdown: Arc::new(AtomicBool::new(false)),
            driver: None,
        }
    }

    /// Give the output a shared flag that stops the consume loop.
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = flag;
    }

    /// Open the device and build the output stream (fail fast at startup).
    /// The stream itself lives on a parked driver thread (`cpal::Stream` is
    /// not `Send`); the ring's producer half is handed back for `run()`.
    pub fn connect(&mut self) -> Result<()> {
        if self.driver.is_some() {
            return Ok(());
        }
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(String, u32, usize, HeapProd<f32>)>>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let driver_shutdown = shutdown.clone();
        let thread_shutdown = driver_shutdown.clone();
        let cfg = self.config.clone();
        let driver = std::thread::spawn(move || match start_output_stream(&cfg) {
            Ok((name, rate, chans, producer)) => {
                let _ = ready_tx.send(Ok((name, rate, chans, producer)));
                // Park holding the Stream alive; woken by Drop.
                while !thread_shutdown.load(Ordering::Relaxed) {
                    std::thread::park();
                }
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        });
        let (name, device_rate, device_channels, producer) =
            match ready_rx.recv_timeout(Duration::from_secs(10)) {
                Ok(Ok(v)) => v,
                // The driver already exited on its error path.
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    shutdown.store(true, Ordering::Relaxed);
                    driver.thread().unpark();
                    let _ = driver.join();
                    return Err("output.soundcard: timed out opening the device".into());
                }
            };
        if !matches!(
            (self.bus_channels, device_channels),
            (1, 1) | (2, 2) | (1, 2) | (2, 1)
        ) {
            // Wake the driver so it drops the device stream before we error.
            shutdown.store(true, Ordering::Relaxed);
            driver.thread().unpark();
            let _ = driver.join();
            return Err(format!(
                "output.soundcard: device has {device_channels} channels (bus has {}); \
                 set(\"channels\", 2) or pick a stereo device",
                self.bus_channels
            )
            .into());
        }
        self.device_rate = device_rate;
        self.device_channels = device_channels;
        self.producer = Some(producer);
        self.resampler = Some(SincResampler::new(
            24,
            self.bus_rate,
            device_rate,
            device_channels,
        ));
        self.driver_shutdown = driver_shutdown;
        self.driver = Some(driver);
        log::info!(
            "output.soundcard: playing on {name:?} ({} Hz, {} ch)",
            device_rate,
            device_channels
        );
        Ok(())
    }

    /// Resample + convert one tap frame and push it to the ring, applying
    /// backpressure (the cpal callback drains at real time, so this paces
    /// the consumer thread — it can never stall the tap, whose channel is
    /// bounded and drops for slow consumers). Disjoint field borrows keep
    /// the scratch, resampler, and producer live at once. Returns the
    /// sample count pushed (for the drift harness's production audit).
    fn push_frame(&mut self, pcm: &[f32]) -> usize {
        let producer = match self.producer.as_mut() {
            Some(p) => p,
            None => return 0,
        };
        let resampler = self.resampler.as_mut().unwrap();

        // Clock-drift compensation (Part G2): estimate the device-clock
        // offset from the smoothed ring-fill error and nudge the resampling
        // step. Negative error (fill below target) means the device is
        // ahead of the bus clock: shrink the step to produce more.
        let cap_samples = producer.capacity().get() / 2;
        let target = cap_samples as f64 / 2.0;
        let fill = producer.occupied_len() as f64;
        self.avg_err = self.avg_err * (1.0 - DRIFT_SMOOTHING) + (fill - target) * DRIFT_SMOOTHING;
        let ppm = (self.avg_err * DRIFT_GAIN).clamp(-DRIFT_CLAMP, DRIFT_CLAMP);
        resampler.set_ppm(ppm * 1_000_000.0);

        let channel_buf = &mut self.channel_buf;
        convert_to_device(pcm, self.bus_channels, self.device_channels, channel_buf);
        let out = resampler.resample(channel_buf);
        let mut rest = out;
        let mut pushed = 0usize;
        while !rest.is_empty() {
            let n = producer.push_slice(rest);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
            pushed += n;
            rest = &rest[n..];
        }
        pushed
    }

    /// Consume frames from the tap until the stream ends (senders dropped)
    /// or shutdown is requested. Dropping the driver thread at the end
    /// closes the device.
    pub fn run(&mut self) -> Result<()> {
        while let Some(frame) = recv_frame_or_shutdown(&self.rx, &self.shutdown) {
            let _ = self.push_frame(&frame.pcm);
        }
        log::info!("output.soundcard: stopped");
        Ok(())
    }
}

impl Drop for SoundcardOutput {
    fn drop(&mut self) {
        self.driver_shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.driver.take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
    }
}

/// Open the device and build the playing output stream on the driver thread.
/// Returns the device name, rate, channel count, and the ring's producer
/// half (the only `Send` piece the caller needs).
fn start_output_stream(
    config: &SoundcardOutputConfig,
) -> Result<(String, u32, usize, HeapProd<f32>)> {
    let host = cpal::default_host();
    let device = match &config.device {
        Some(name) => host
            .output_devices()
            .map_err(|e| format!("output.soundcard: cannot enumerate devices: {e}"))?
            .find(|d| d.name().map(|n| n == *name).unwrap_or(false))
            .ok_or_else(|| format!("output.soundcard: no output device named {name:?}"))?,
        None => host
            .default_output_device()
            .ok_or_else(|| "output.soundcard: no default output device".to_string())?,
    };
    let name = device
        .name()
        .unwrap_or_else(|_| "soundcard output".to_string());
    let supported = device
        .default_output_config()
        .map_err(|e| format!("output.soundcard: no default output config: {e}"))?;
    let device_rate = supported.sample_rate().0;
    let device_channels = supported.channels() as usize;
    let (producer, consumer) = HeapRb::<f32>::new(RING_FRAMES * 2 * device_channels).split();
    let stream_config = supported.config();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => build_output::<f32>(&device, &stream_config, consumer)?,
        cpal::SampleFormat::I16 => build_output::<i16>(&device, &stream_config, consumer)?,
        cpal::SampleFormat::U16 => build_output::<u16>(&device, &stream_config, consumer)?,
        cpal::SampleFormat::I32 => build_output::<i32>(&device, &stream_config, consumer)?,
        cpal::SampleFormat::F64 => build_output::<f64>(&device, &stream_config, consumer)?,
        other => {
            return Err(format!(
                "output.soundcard: unsupported sample format {other:?} on {name:?}"
            )
            .into());
        }
    };
    stream
        .play()
        .map_err(|e| format!("output.soundcard: cannot start stream: {e}"))?;
    Ok((name, device_rate, device_channels, producer))
}

/// Convert one bus frame to device channels into the reusable `out` vec:
/// (1->2) duplicates, (2->1) averages, matching counts copy straight
/// through. Channel combos are validated in `connect`.
fn convert_to_device(
    samples: &[f32],
    bus_channels: usize,
    device_channels: usize,
    out: &mut Vec<f32>,
) {
    out.clear();
    out.reserve(samples.len() * device_channels / bus_channels.max(1));
    match (bus_channels, device_channels) {
        (1, 1) | (2, 2) => out.extend_from_slice(samples),
        (1, 2) => {
            for &s in samples {
                out.extend_from_slice(&[s, s]);
            }
        }
        (2, 1) => {
            for c in samples.chunks(2) {
                out.push((c[0] + c[1]) * 0.5);
            }
        }
        _ => unreachable!("channel combos validated in connect"),
    }
}

/// f32 -> native sample, in the realtime callback (same scaling cpal/dasp
/// use; `as` casts saturate, so out-of-range f32 clamps).
trait FromF32 {
    fn from_f32(v: f32) -> Self;
}

impl FromF32 for f32 {
    fn from_f32(v: f32) -> Self {
        v
    }
}
impl FromF32 for i16 {
    fn from_f32(v: f32) -> Self {
        (v * 32767.0).round() as i16
    }
}
impl FromF32 for u16 {
    fn from_f32(v: f32) -> Self {
        ((v * 32767.0).round() + 32768.0) as u16
    }
}
impl FromF32 for i32 {
    fn from_f32(v: f32) -> Self {
        (v * 2_147_483_647.0).round() as i32
    }
}
impl FromF32 for f64 {
    fn from_f32(v: f32) -> Self {
        v as f64
    }
}

/// Build an output stream for sample type `T`, draining the ring in the
/// realtime callback (silence on underrun).
fn build_output<T>(
    device: &cpal::Device,
    stream_config: &cpal::StreamConfig,
    mut consumer: HeapCons<f32>,
) -> Result<cpal::Stream>
where
    T: cpal::SizedSample + FromF32,
{
    let err_fn = |e| log::error!("output.soundcard: stream error: {e}");
    let stream = device
        .build_output_stream(
            stream_config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                // Never block or allocate: pop in stack chunks, silence for
                // the rest (underrun).
                let mut buf = [0f32; CALLBACK_CHUNK];
                for chunk in data.chunks_mut(CALLBACK_CHUNK) {
                    let n = consumer.pop_slice(&mut buf[..chunk.len()]);
                    for (i, out) in chunk.iter_mut().enumerate() {
                        *out = if i < n {
                            T::from_f32(buf[i])
                        } else {
                            T::from_f32(0.0)
                        };
                    }
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| format!("output.soundcard: cannot open device: {e}"))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::mpsc::Sender;
    use std::time::Instant;

    use super::*;

    /// A parts-based output (no cpal): the resampler + ring bridge with
    /// synthetic pacing, so the conversion and drift math are testable
    /// without hardware.
    fn parts_output(
        bus_rate: u32,
        bus_channels: usize,
        device_rate: u32,
        device_channels: usize,
        ring_samples: usize,
    ) -> (SoundcardOutput, HeapCons<f32>, Sender<Arc<AudioFrame>>) {
        let (producer, consumer) = HeapRb::<f32>::new(ring_samples).split();
        let (tx, rx) = mpsc::channel();
        let out = SoundcardOutput {
            config: SoundcardOutputConfig::default(),
            rx,
            bus_rate,
            bus_channels,
            device_rate,
            device_channels,
            producer: Some(producer),
            channel_buf: Vec::new(),
            resampler: Some(SincResampler::new(
                24,
                bus_rate,
                device_rate,
                device_channels,
            )),
            avg_err: 0.0,
            shutdown: Arc::new(AtomicBool::new(false)),
            driver_shutdown: Arc::new(AtomicBool::new(false)),
            driver: None,
        };
        (out, consumer, tx)
    }

    /// Drift-simulation harness: a drain thread pops at
    /// `device_rate * (1 + skew/1e6)` samples/sec (the real device clock,
    /// wall-clock anchored); the caller feeds `push_frame` at the bus rate.
    /// The ring is pre-filled to the drift loop's steady-state fill
    /// (`target - skew/gain`, since the output estimate is the negative of
    /// the skew). Returns the ring fill sampled every 0.5 s after `settle`
    /// seconds and the output's converged PPM estimate.
    fn run_output_drift(
        out: &mut SoundcardOutput,
        mut consumer: HeapCons<f32>,
        skew_ppm: f64,
        seconds: f64,
        settle: f64,
    ) -> (Vec<usize>, f64) {
        let target = consumer.capacity().get() / 4;
        let steady = (target as f64 - skew_ppm / 10.0).max(0.0) as usize;
        // Pre-fill through the producer half, temporarily taken out.
        let mut prod = out.producer.take().unwrap();
        let prefill = vec![0.5f32; steady.min(consumer.capacity().get())];
        prod.push_slice(&prefill);
        out.producer = Some(prod);

        let done = Arc::new(AtomicBool::new(false));
        let done_flag = done.clone();
        let drained_total = Arc::new(AtomicU64::new(0));
        let drained_flag = drained_total.clone();
        let device_rate = out.device_rate as f64;
        std::thread::spawn(move || {
            let start = Instant::now();
            let mut drained = 0u64;
            let mut buf = [0f32; 512];
            while !done_flag.load(Ordering::Relaxed) {
                let t = start.elapsed().as_secs_f64();
                let target_total = (t * device_rate * (1.0 + skew_ppm / 1e6)) as u64;
                if target_total > drained {
                    let mut left = (target_total - drained) as usize;
                    while left > 0 {
                        let n = consumer.pop_slice(&mut buf[..left.min(512)]);
                        if n == 0 {
                            break; // underrun: silence, like the callback
                        }
                        left -= n;
                    }
                    drained = target_total;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            drained_flag.store(drained, Ordering::Relaxed);
        });

        let bus_rate = out.bus_rate as f64;
        let start = Instant::now();
        let mut fed = 0usize;
        let mut pushed_total = 0usize;
        let chunk = vec![0.5f32; 512];
        let mut fills = Vec::new();
        let mut last_sample = -1.0f64;
        loop {
            let elapsed = start.elapsed();
            let t = elapsed.as_secs_f64();
            if t >= seconds {
                break;
            }
            let target_fed = (t * bus_rate) as usize;
            while fed < target_fed {
                // Even-length chunks only: an odd sample count would drop
                // half a frame inside the 2-channel resampler and skew the
                // production audit by ~1 %.
                let mut n = (target_fed - fed).min(chunk.len());
                n &= !1;
                if n == 0 {
                    break;
                }
                pushed_total += out.push_frame(&chunk[..n]);
                fed += n;
            }
            if t >= settle && t - last_sample >= 0.5 {
                last_sample = t;
                fills.push(out.producer.as_ref().unwrap().occupied_len());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        done.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(20));
        let drained = drained_total.load(Ordering::Relaxed) as f64 / seconds;
        let ppm = out.resampler.as_ref().unwrap().ppm();
        eprintln!(
            "output drift trace: drained={drained:.0}/s fed={:.0}/s pushed={:.0}/s ppm={ppm} fills={fills:?}",
            fed as f64 / seconds,
            pushed_total as f64 / seconds
        );
        (fills, ppm)
    }

    #[test]
    fn drift_control_tracks_a_fast_device_clock() {
        // Bus 48 kHz -> device 44.1 kHz (resampler path), device clock
        // 1000 PPM fast. Without compensation the ring would drain to
        // underrun; the loop must converge its PPM estimate to -1000 (the
        // output's estimate is the negative of the skew) and hold the fill
        // mid-ring.
        let (mut out, consumer, _tx) = parts_output(48_000, 2, 44_100, 2, 32_768);
        let (fills, ppm) = run_output_drift(&mut out, consumer, 1000.0, 25.0, 8.0);
        assert!(
            (ppm + 1000.0).abs() < 600.0,
            "PPM estimate {ppm} must track the +1000 PPM skew"
        );
        let lo = *fills.iter().min().expect("fill samples");
        let hi = *fills.iter().max().expect("fill samples");
        assert!(lo > 100, "fill dipped near empty: {lo}");
        assert!(hi < 16_000, "fill {hi} hit the ring capacity");
    }

    #[test]
    fn drift_control_tracks_a_slow_device_clock() {
        // Same rate both sides, device clock 1000 PPM slow. Without
        // compensation the ring would fill up and the feed would stall on
        // backpressure; the estimate converges to +1000.
        let (mut out, consumer, _tx) = parts_output(44_100, 2, 44_100, 2, 32_768);
        let (fills, ppm) = run_output_drift(&mut out, consumer, -1000.0, 25.0, 8.0);
        assert!(
            (ppm - 1000.0).abs() < 600.0,
            "PPM estimate {ppm} must track the -1000 PPM skew"
        );
        let lo = *fills.iter().min().expect("fill samples");
        let hi = *fills.iter().max().expect("fill samples");
        assert!(lo > 100, "fill dipped near empty: {lo}");
        assert!(hi < 16_000, "fill {hi} hit the ring capacity");
    }

    #[test]
    fn convert_to_device_covers_the_supported_combinations() {
        let mut out = Vec::new();
        convert_to_device(&[0.25, -0.5], 2, 2, &mut out);
        assert_eq!(out, vec![0.25, -0.5]);

        convert_to_device(&[0.25, -0.5], 1, 2, &mut out);
        assert_eq!(out, vec![0.25, 0.25, -0.5, -0.5]);

        convert_to_device(&[0.25, -0.5, 1.0, 0.0], 2, 1, &mut out);
        assert_eq!(out, vec![-0.125, 0.5]);
    }
}
