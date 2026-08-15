//! Single-puller engine tap.
//!
//! One thread owns the root source and pulls at wall-clock pace; every
//! output is a pure consumer of a bounded channel. A stalled output drops
//! frames instead of stalling the engine or the other outputs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::source::AudioSource;

/// Maximum frames buffered per tap before a slow consumer starts dropping.
const TAP_BOUND: usize = 4;

/// One buffer of mixed audio published to every output.
pub struct AudioFrame {
    pub pcm: Vec<f32>,
    pub label: Option<Arc<str>>,
    /// Returns `pcm` to the shared pool when the last consumer drops us.
    pub(crate) pool: Option<Arc<FramePool>>,
}

/// Sleep for `dur`, waking early on shutdown (used by reconnect loops so
/// Ctrl-C is never delayed by a retry backoff).
pub fn interruptible_sleep(dur: Duration, shutdown: &AtomicBool) {
    let deadline = Instant::now() + dur;
    while Instant::now() < deadline {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Block for the next frame, waking every 100 ms to re-check `shutdown`.
/// Consumer loops use this so they exit on Ctrl-C instead of blocking
/// forever in `recv()` once the puller has stopped publishing frames.
pub fn recv_frame_or_shutdown(
    rx: &Receiver<Arc<AudioFrame>>,
    shutdown: &AtomicBool,
) -> Option<Arc<AudioFrame>> {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return None;
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(frame) => return Some(frame),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return None,
        }
    }
}

impl Drop for AudioFrame {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.recycle(&mut self.pcm);
        }
    }
}

/// Recycled PCM buffers shared between the puller and in-flight frames.
///
/// The puller pops a preallocated idle buffer (`4 * tap_count + 2` of
/// them); frames return theirs via [`AudioFrame`]'s `Drop` once every
/// consumer is done. If the pool ever runs dry (stalled consumers holding
/// frames), the puller falls back to a fresh `Vec` — allocation only on that
/// degraded path, never on the steady-state one.
pub(crate) struct FramePool {
    idle: Mutex<Vec<Vec<f32>>>,
    max_idle: usize,
}

impl FramePool {
    fn new(buffer_capacity: usize, max_idle: usize) -> Self {
        let mut idle = Vec::with_capacity(max_idle);
        for _ in 0..max_idle {
            let mut buf = Vec::with_capacity(buffer_capacity);
            buf.resize(buffer_capacity, 0.0);
            idle.push(buf);
        }
        Self {
            idle: Mutex::new(idle),
            max_idle,
        }
    }

    fn pop(&self) -> Option<Vec<f32>> {
        self.idle.lock().unwrap().pop()
    }

    fn recycle(&self, buf: &mut Vec<f32>) {
        let mut idle = self.idle.lock().unwrap();
        if idle.len() < self.max_idle {
            idle.push(std::mem::take(buf));
        }
    }
}

/// Owns the root source and publishes one wall-clock-paced frame per buffer
/// to every registered tap.
pub struct EngineTap {
    root: Box<dyn AudioSource>,
    taps: Vec<SyncSender<Arc<AudioFrame>>>,
    sample_rate: u32,
    chans: usize,
    last_label: Option<Arc<str>>,
}

impl EngineTap {
    pub fn new(root: Box<dyn AudioSource>, sample_rate: u32, chans: usize) -> Self {
        Self {
            root,
            taps: Vec::new(),
            sample_rate,
            chans,
            last_label: None,
        }
    }

    /// Register a new output consumer.
    pub fn register(&mut self) -> Receiver<Arc<AudioFrame>> {
        let (tx, rx) = sync_channel(TAP_BOUND);
        self.taps.push(tx);
        rx
    }

    /// Pull at wall-clock pace and publish one frame per buffer to all taps.
    /// Ends when `shutdown` is set or the root source exhausts; dropping the
    /// senders then ends every consumer's `recv` loop. Always sets the
    /// `shutdown` flag before returning: the engine is over, and the
    /// Lua-owning main loop uses the flag to exit its event loop.
    pub fn run(&mut self, fpb: usize, shutdown: Arc<AtomicBool>) {
        let buffer_capacity = fpb * self.chans;
        let max_idle = self.taps.len() * TAP_BOUND + 2;
        let pool = Arc::new(FramePool::new(buffer_capacity, max_idle));
        let start = Instant::now();
        let mut frames_pulled = 0u64;

        loop {
            if shutdown.load(Ordering::SeqCst) {
                log::info!("engine tap: shutdown requested");
                break;
            }
            let elapsed_us = start.elapsed().as_micros() as u64;
            let next_due_us = frames_pulled * 1_000_000 / self.sample_rate as u64;
            if elapsed_us < next_due_us {
                std::thread::sleep(Duration::from_micros(next_due_us - elapsed_us));
            }

            let mut pcm = pool.pop().unwrap_or_else(|| vec![0f32; buffer_capacity]);
            pcm.resize(buffer_capacity, 0.0);

            let n = self.root.next_buffer(&mut pcm);
            if n == 0 && self.root.is_exhausted() {
                log::info!("engine tap: root exhausted, ending stream");
                break;
            }
            if n == 0 {
                pool.recycle(&mut pcm);
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            frames_pulled += (n / self.chans) as u64;
            pcm.truncate(n);

            // Cache the label: consecutive frames share one `Arc<str>`.
            let label = self.root.label().map(Arc::from);
            if label != self.last_label {
                self.last_label = label;
            }

            let frame = Arc::new(AudioFrame {
                pcm,
                label: self.last_label.clone(),
                pool: Some(pool.clone()),
            });
            for tx in &self.taps {
                if let Err(e) = tx.try_send(frame.clone()) {
                    log::debug!("tap: consumer slow, dropping frame ({e:?})");
                }
            }
        }
        shutdown.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSource {
        value: f32,
        total_frames: usize,
        pos_frames: usize,
        label: String,
    }

    impl AudioSource for FakeSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            if self.pos_frames >= self.total_frames {
                return 0;
            }
            let frames = buffer.len().min(self.total_frames - self.pos_frames);
            buffer[..frames].fill(self.value);
            self.pos_frames += frames;
            frames
        }
        fn is_exhausted(&self) -> bool {
            self.pos_frames >= self.total_frames
        }
        fn label(&self) -> Option<String> {
            Some(self.label.clone())
        }
    }

    fn fake() -> Box<dyn AudioSource> {
        Box::new(FakeSource {
            value: 0.25,
            total_frames: 100_000,
            pos_frames: 0,
            label: "track one".into(),
        })
    }

    #[test]
    fn two_consumers_see_identical_frames() {
        let mut tap = EngineTap::new(fake(), 44_100, 2);
        let rx1 = tap.register();
        let rx2 = tap.register();

        let shutdown = Arc::new(AtomicBool::new(false));
        let tap_shutdown = shutdown.clone();
        let handle = std::thread::spawn(move || tap.run(10, tap_shutdown));

        for i in 0..20 {
            let f1 = rx1.recv().expect("tap 1 frame");
            let f2 = rx2.recv().expect("tap 2 frame");
            assert_eq!(f1.pcm, f2.pcm, "frame {i} pcm differs");
            assert_eq!(f1.label, f2.label, "frame {i} label differs");
            assert_eq!(f1.pcm.len(), 20, "10 frames x 2 chans");
            assert!(f1.pcm.iter().all(|&s| (s - 0.25).abs() < 1e-6));
        }

        shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap();
    }

    #[test]
    fn a_stalled_consumer_drops_frames_without_stalling_others() {
        let mut tap = EngineTap::new(fake(), 44_100, 1);
        let _stalled = tap.register(); // never read: fills and drops
        let rx2 = tap.register();

        let shutdown = Arc::new(AtomicBool::new(false));
        let tap_shutdown = shutdown.clone();
        let handle = std::thread::spawn(move || tap.run(10, tap_shutdown));

        for _ in 0..30 {
            let frame = rx2
                .recv_timeout(Duration::from_secs(2))
                .expect("live tap must keep producing");
            assert_eq!(frame.pcm.len(), 10);
            assert!(frame.pcm.iter().all(|&s| (s - 0.25).abs() < 1e-6));
        }

        shutdown.store(true, Ordering::SeqCst);
        handle.join().unwrap();
    }
}
