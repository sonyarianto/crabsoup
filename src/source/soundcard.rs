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

        // Drop-oldest cap: a stalled pull must not play ever-staler audio.
        // Keep only the most recent half of the ring (the live window), so
        // the cap stays consistent however the ring was sized.
        let cap_samples = self.consumer.capacity().get() / 2;
        let over = self.consumer.occupied_len().saturating_sub(cap_samples);
        if over > 0 {
            self.consumer.skip(over);
        }

        // Pull just enough device frames to fill the remaining bus capacity
        // (plus rounding margin); the excess stays in the resampler's ring.
        let want_frames = (capacity / self.bus_channels.max(1)) as f64 * self.device_rate as f64
            / self.bus_rate.max(1) as f64;
        let want = (want_frames.ceil() as usize + 1) * self.device_channels;
        if self.scratch.len() != want {
            self.scratch.resize(want, 0.0);
        }
        let n = self.consumer.pop_slice(&mut self.scratch);
        if n == 0 {
            return written;
        }
        let device = &self.scratch[..n];
        let resampler = &mut self.resampler;
        self.pending.clear();
        if self.device_channels == self.bus_channels {
            if self.device_rate == self.bus_rate {
                // Exact passthrough: same rate + same channels, no filter.
                self.pending.extend_from_slice(device);
            } else {
                self.pending.extend_from_slice(resampler.resample(device));
            }
        } else {
            // Channel mismatch is the exception; convert_channels allocates.
            let converted = convert_channels(device, self.device_channels, self.bus_channels);
            if self.device_rate == self.bus_rate {
                self.pending.extend_from_slice(&converted);
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
    /// testable without hardware.
    fn parts_source(
        device_rate: u32,
        device_channels: usize,
        bus_rate: u32,
        bus_channels: usize,
    ) -> (SoundcardInputSource, HeapProd<f32>) {
        let (producer, consumer) = HeapRb::<f32>::new(RING_FRAMES * 2 * device_channels).split();
        let src = SoundcardInputSource {
            consumer,
            bus_rate,
            bus_channels,
            device_rate,
            device_channels,
            scratch: Vec::new(),
            pending: Vec::new(),
            resampler: SincResampler::new(24, device_rate, bus_rate, bus_channels),
            name: "test input".into(),
            driver_shutdown: Arc::new(AtomicBool::new(false)),
            driver: None,
        };
        (src, producer)
    }

    #[test]
    fn same_rate_passthrough_returns_the_captured_samples() {
        let (mut src, mut producer) = parts_source(44_100, 2, 44_100, 2);
        let samples: Vec<f32> = (0..100).map(|i| i as f32 * 0.01).collect();
        producer.push_slice(&samples);
        let mut buf = vec![0f32; 64];
        let n = src.next_buffer(&mut buf);
        assert_eq!(n, 64);
        // The ring held 100 samples; a 64-sample pull consumes the front.
        for (i, &s) in buf[..n].iter().enumerate() {
            assert!((s - i as f32 * 0.01).abs() < 1e-6, "sample {i}: {s}");
        }
    }

    #[test]
    fn upsamples_to_the_bus_rate() {
        // 22.05 kHz mono device -> 44.1 kHz stereo bus: half-rate doubles
        // the frame count and mono duplicates across channels. The ring is
        // fed in small chunks at capture pace (a full second pushed at once
        // would trip the drop-oldest latency cap, which is correct live
        // behaviour).
        let (mut src, mut producer) = parts_source(22_050, 1, 44_100, 2);
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
        let (mut src, mut producer) = parts_source(44_100, 2, 44_100, 2);
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
