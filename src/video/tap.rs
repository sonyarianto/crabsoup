//! Video fan-out tap (Part H1 carrier).
//!
//! Mirror of the audio `EngineTap` publish side: a decode thread owns the
//! `VideoDecoder` and `publish`es each frame to every registered consumer;
//! a slow consumer drops frames via `try_send` instead of stalling the
//! decode or the other consumers. A/V sync is held at mux time by PTS, not
//! by forcing video through the audio frames. All methods take `&self` so
//! the tap can be shared between the decode thread and output consumers.

use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use super::frame::VideoFrame;

/// Maximum frames buffered per video consumer before it starts dropping.
const TAP_BOUND: usize = 4;

/// Fan-out publisher for decoded video frames.
pub struct VideoTap {
    consumers: Mutex<Vec<SyncSender<Arc<VideoFrame>>>>,
}

impl Default for VideoTap {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoTap {
    pub fn new() -> Self {
        Self {
            consumers: Mutex::new(Vec::new()),
        }
    }

    /// Subscribe a new consumer.
    pub fn register(&self) -> Receiver<Arc<VideoFrame>> {
        let (tx, rx) = sync_channel(TAP_BOUND);
        self.consumers.lock().unwrap().push(tx);
        rx
    }

    /// Publish one frame to every consumer, dropping for stalled ones.
    pub fn publish(&self, frame: Arc<VideoFrame>) {
        for tx in self.consumers.lock().unwrap().iter() {
            let _ = tx.try_send(frame.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn frame(pts_us: u64) -> Arc<VideoFrame> {
        Arc::new(VideoFrame::new(
            pts_us,
            2,
            2,
            vec![0; 4],
            vec![0; 1],
            vec![0; 1],
        ))
    }

    #[test]
    fn two_consumers_see_identical_frames() {
        let tap = VideoTap::new();
        let rx1 = tap.register();
        let rx2 = tap.register();
        // Drain both receivers per publish: each consumer's 4-slot channel
        // drops otherwise, and the recv below would block.
        for i in 0..20 {
            tap.publish(frame(i));
            assert_eq!(rx1.recv_timeout(Duration::from_secs(2)).unwrap().pts_us, i);
            assert_eq!(rx2.recv_timeout(Duration::from_secs(2)).unwrap().pts_us, i);
        }
    }

    #[test]
    fn stalled_consumer_drops_without_stalling_others() {
        let tap = VideoTap::new();
        let _stalled = tap.register(); // never read: fills and drops
        let rx2 = tap.register();
        // Drain rx2 per publish so its 4-slot channel never backs up; the
        // stalled consumer's channel fills and drops instead of blocking.
        for i in 0..100 {
            tap.publish(frame(i));
            let got = rx2
                .recv_timeout(Duration::from_secs(2))
                .expect("live consumer must receive every frame");
            assert_eq!(got.pts_us, i);
        }
    }
}
