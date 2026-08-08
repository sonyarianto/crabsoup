use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::source::AudioSource;

/// A pull-based view of the live DJ buffer.
///
/// The harbor's decode thread pushes converted PCM frames into a shared queue;
/// the mixer pulls them out. While the DJ is silent or disconnected the queue
/// is empty and [`AudioSource::next_buffer`] returns 0 frames (silence), which
/// lets the priority mixer fade back out to the playlist.
pub struct LiveSource {
    queue: Arc<Mutex<VecDeque<f32>>>,
    exhausted: Arc<AtomicBool>,
    max_frames: usize,
}

impl LiveSource {
    pub fn new(
        queue: Arc<Mutex<VecDeque<f32>>>,
        exhausted: Arc<AtomicBool>,
        max_frames: usize,
    ) -> Self {
        Self {
            queue,
            exhausted,
            max_frames,
        }
    }

    /// Push decoded samples, dropping the oldest samples beyond `max_frames`
    /// to keep live latency bounded.
    pub fn push_samples(&self, samples: &[f32]) {
        let mut q = self.queue.lock();
        q.extend(samples.iter().copied());
        let over = q.len().saturating_sub(self.max_frames);
        if over > 0 {
            q.drain(..over);
        }
    }
}

impl AudioSource for LiveSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let mut q = self.queue.lock();
        let n = buffer.len().min(q.len());
        for slot in buffer[..n].iter_mut() {
            *slot = q.pop_front().unwrap_or(0.0);
        }
        n
    }

    fn is_exhausted(&self) -> bool {
        self.exhausted.load(Ordering::Relaxed) && self.queue.lock().is_empty()
    }

    fn label(&self) -> Option<String> {
        Some("LIVE DJ".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_frames_then_silence_then_exhausted() {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        let ex = Arc::new(AtomicBool::new(false));
        let mut src = LiveSource::new(q.clone(), ex.clone(), 100);
        src.push_samples(&[1.0, 2.0, 3.0]);

        let mut buf = vec![0f32; 10];
        assert_eq!(src.next_buffer(&mut buf), 3);
        assert_eq!(&buf[..3], &[1.0, 2.0, 3.0]);
        assert!(!src.is_exhausted());

        ex.store(true, Ordering::Relaxed);
        assert!(src.is_exhausted());
        assert_eq!(src.next_buffer(&mut buf), 0);
    }

    #[test]
    fn drops_oldest_beyond_capacity() {
        let q = Arc::new(Mutex::new(VecDeque::new()));
        let ex = Arc::new(AtomicBool::new(false));
        let src = LiveSource::new(q.clone(), ex.clone(), 4);
        src.push_samples(&[1.0, 2.0, 3.0]);
        src.push_samples(&[4.0, 5.0]);
        assert_eq!(q.lock().iter().copied().collect::<Vec<_>>(), vec![2.0, 3.0, 4.0, 5.0]);
    }
}
