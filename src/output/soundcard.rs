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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::{traits::*, HeapCons, HeapProd, HeapRb};

use crate::config::SoundcardOutputConfig;
use crate::engine::tap::AudioFrame;
use crate::resample::SincResampler;
use crate::Result;

/// Ring capacity in device frames (double-buffered against the callback).
const RING_FRAMES: usize = 16 * 1024;
/// Stack scratch for the realtime callback.
const CALLBACK_CHUNK: usize = 2048;

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
    /// the scratch, resampler, and producer live at once.
    fn push_frame(&mut self, pcm: &[f32]) {
        let producer = match self.producer.as_mut() {
            Some(p) => p,
            None => return,
        };
        let resampler = self.resampler.as_mut().unwrap();
        let channel_buf = &mut self.channel_buf;
        convert_to_device(pcm, self.bus_channels, self.device_channels, channel_buf);
        let out = resampler.resample(channel_buf);
        let mut rest = out;
        while !rest.is_empty() {
            let n = producer.push_slice(rest);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
            rest = &rest[n..];
        }
    }

    /// Consume frames from the tap until the stream ends (senders dropped)
    /// or shutdown is requested. Dropping the driver thread at the end
    /// closes the device.
    pub fn run(&mut self) -> Result<()> {
        while let Ok(frame) = self.rx.recv() {
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("shutdown requested, stopping soundcard output");
                break;
            }
            self.push_frame(&frame.pcm);
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
    let name = device.name().unwrap_or_else(|_| "soundcard output".to_string());
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
            .into())
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
fn convert_to_device(samples: &[f32], bus_channels: usize, device_channels: usize, out: &mut Vec<f32>) {
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
                        *out = if i < n { T::from_f32(buf[i]) } else { T::from_f32(0.0) };
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
    use super::*;

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
