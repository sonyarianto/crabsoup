use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use ringbuf::{HeapCons, HeapProd, traits::*};

use crate::source::AudioSource;

/// A pull-based view of the live DJ buffer.
///
/// The harbor's decode thread pushes converted PCM frames through the SPSC
/// ring (the [`LiveSink`] half); the mixer pulls them out. While the DJ is
/// silent or disconnected the ring is empty and [`AudioSource::next_buffer`]
/// returns 0 frames (silence), which lets the priority mixer fade back out
/// to the playlist.
pub struct LiveSource {
    consumer: HeapCons<f32>,
    exhausted: Arc<AtomicBool>,
    /// Max samples played before the oldest are dropped (live latency cap).
    max_samples: usize,
}

impl LiveSource {
    pub fn new(consumer: HeapCons<f32>, exhausted: Arc<AtomicBool>, max_samples: usize) -> Self {
        Self {
            consumer,
            exhausted,
            max_samples,
        }
    }
}

impl AudioSource for LiveSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        // Drop-oldest cap: the DJ's stream may have raced ahead (a fast
        // upload, a briefly stalled consumer); keep only the most recent
        // `max_samples` window so live latency stays bounded.
        let over = self.consumer.occupied_len().saturating_sub(self.max_samples);
        if over > 0 {
            self.consumer.skip(over);
        }
        self.consumer.pop_slice(buffer)
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Relaxed) && self.consumer.is_empty()
    }

    fn label(&self) -> Option<String> {
        Some("LIVE DJ".into())
    }
}

/// Producer half of the live DJ handoff: the harbor's decode thread pushes
/// decoded PCM here; [`LiveSource`] (the audio thread) pulls it lock-free.
/// The ring is sized at twice the drop-oldest cap by the caller, so brief
/// consumer stalls are absorbed without ever blocking; when even that
/// headroom is exhausted the producer applies backpressure (waits for the
/// consumer to drain) instead of dropping audio — a fast `curl -T` upload
/// throttles to real time and plays completely, as the old
/// `Mutex<VecDeque>` did, rather than losing the middle of the file.
pub struct LiveSink {
    producer: HeapProd<f32>,
}

impl LiveSink {
    pub fn new(producer: HeapProd<f32>) -> Self {
        Self { producer }
    }

    /// Push decoded samples; blocks (brief sleeps) while the ring is full
    /// so no audio is ever dropped. The consumer pulls at real time, so
    /// this throttles an overrunning producer to the consumer's rate.
    pub fn push_samples(&mut self, samples: &[f32]) {
        let mut rest = samples;
        while !rest.is_empty() {
            let n = self.producer.push_slice(rest);
            if n == 0 {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
            rest = &rest[n..];
        }
    }

    /// Samples still in the ring waiting for the consumer to drain.
    pub fn buffered(&self) -> usize {
        self.producer.occupied_len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ringbuf::HeapRb;

    #[test]
    fn returns_frames_then_silence_then_exhausted() {
        let (prod, cons) = HeapRb::<f32>::new(200).split();
        let ex = Arc::new(AtomicBool::new(false));
        let mut src = LiveSource::new(cons, ex.clone(), 100);
        let mut sink = LiveSink::new(prod);
        sink.push_samples(&[1.0, 2.0, 3.0]);

        let mut buf = vec![0f32; 10];
        assert_eq!(src.next_buffer(&mut buf), 3);
        assert_eq!(&buf[..3], &[1.0, 2.0, 3.0]);
        assert!(!src.is_exhausted());

        ex.store(true, Ordering::Relaxed);
        assert!(src.is_exhausted());
        assert_eq!(src.next_buffer(&mut buf), 0);
    }

    #[test]
    fn producer_blocks_instead_of_dropping_newest() {
        // Ring capacity 2 * cap; the sink must never lose the newest audio
        // when the ring is full — it waits for the consumer instead of
        // dropping (the old non-blocking sink would silently lose it).
        let (prod, cons) = HeapRb::<f32>::new(2 * 16).split();
        let mut sink = LiveSink::new(prod);
        let mut src = LiveSource::new(cons, Arc::new(AtomicBool::new(false)), 16);
        let total = 10_000usize;
        let producer = std::thread::spawn(move || {
            let mut sink = sink;
            for i in 0..total {
                sink.push_samples(&[i as f32]);
            }
        });
        // Drain concurrently so the producer always makes progress.
        let mut got = Vec::new();
        let mut buf = vec![0f32; 16];
        while got.last().copied() != Some((total - 1) as f32) {
            let n = src.next_buffer(&mut buf);
            got.extend_from_slice(&buf[..n]);
        }
        producer.join().unwrap();
        // The newest sample made it through the full ring.
        assert_eq!(got.last(), Some(&(total as f32 - 1.0)));
        // Drop-oldest only ever removes from the front: in-order.
        assert!(got.windows(2).all(|w| w[1] >= w[0]));
    }

    #[test]
    fn consumer_drops_oldest_beyond_capacity() {
        let (prod, cons) = HeapRb::<f32>::new(200).split();
        let ex = Arc::new(AtomicBool::new(false));
        let mut sink = LiveSink::new(prod);
        let mut src = LiveSource::new(cons, ex.clone(), 4);
        sink.push_samples(&[1.0, 2.0, 3.0]);
        sink.push_samples(&[4.0, 5.0, 6.0]);

        // Six queued, cap 4: the pull skips the two oldest and returns the
        // most recent window (same drop-oldest semantics as before).
        let mut buf = vec![0f32; 6];
        assert_eq!(src.next_buffer(&mut buf), 4);
        assert_eq!(&buf[..4], &[3.0, 4.0, 5.0, 6.0]);
        // Ring drained but the DJ is still connected.
        assert!(!src.is_exhausted());
    }
}
