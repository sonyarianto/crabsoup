//! Request queue (Liquidsoap `request.queue`): a FIFO of file paths pushed
//! at runtime via the telnet port, played ahead of the playlist when
//! non-empty.
//!
//! The queue state is shared: the control port pushes paths and requests
//! skips; the [`RequestQueueSource`] (pulled on the tap thread) pops them.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use symphonia::core::audio::SignalSpec;

use crate::source::file::FileSource;
use crate::source::AudioSource;

/// Shared FIFO state. Methods used by the control port (`push`/`list`/
/// `clear`/`request_skip`) and by the source (`pop`/`take_skip`).
pub struct RequestQueue {
    inner: Mutex<QueueState>,
}

#[derive(Default)]
struct QueueState {
    paths: VecDeque<PathBuf>,
    /// One-shot skip requested by the control port; consumed when a queued
    /// track is actually playing.
    skip: bool,
}

impl RequestQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(QueueState::default()),
        }
    }

    /// Append a path at the end of the queue.
    pub fn push(&self, path: PathBuf) {
        self.inner.lock().unwrap().paths.push_back(path);
    }

    /// Number of queued (not yet playing) paths.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().paths.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy of the queued paths, oldest first.
    pub fn list(&self) -> Vec<PathBuf> {
        self.inner.lock().unwrap().paths.iter().cloned().collect()
    }

    /// Drop every queued path (the playing track is unaffected).
    pub fn clear(&self) {
        self.inner.lock().unwrap().paths.clear();
    }

    /// Tell the source to skip the queued track it is playing.
    pub fn request_skip(&self) {
        self.inner.lock().unwrap().skip = true;
    }

    /// Pop the next path, dropping any pending skip (a skip only applies to
    /// a track already playing).
    fn pop(&self) -> Option<PathBuf> {
        let mut st = self.inner.lock().unwrap();
        st.skip = false;
        st.paths.pop_front()
    }

    /// True if a skip was requested; always consumes the request.
    fn take_skip(&self) -> bool {
        let mut st = self.inner.lock().unwrap();
        std::mem::take(&mut st.skip)
    }
}

impl Default for RequestQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// Plays queued files FIFO. Exhausts whenever the queue is empty; a control
/// `queue.skip` drops the track currently playing (if any) and advances.
pub struct RequestQueueSource {
    queue: Arc<RequestQueue>,
    target: SignalSpec,
    frames_per_buffer: usize,
    current: Option<FileSource>,
    current_path: Option<PathBuf>,
}

impl RequestQueueSource {
    pub fn new(
        queue: Arc<RequestQueue>,
        target: SignalSpec,
        frames_per_buffer: usize,
    ) -> Self {
        Self {
            queue,
            target,
            frames_per_buffer,
            current: None,
            current_path: None,
        }
    }

    /// The path currently playing (metadata label), if any.
    pub fn current_path(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }
}

impl AudioSource for RequestQueueSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        loop {
            if let Some(src) = self.current.as_mut() {
                if self.queue.take_skip() {
                    log::info!("request queue: skipping {}", self.current_path.as_deref().unwrap().display());
                    self.current = None;
                    continue;
                }
                let n = src.next_buffer(buffer);
                if n > 0 {
                    return n;
                }
                if src.is_exhausted() {
                    log::info!("request queue: finished {}", self.current_path.as_deref().unwrap().display());
                    self.current = None;
                    continue;
                }
                return 0;
            }
            let Some(path) = self.queue.pop() else {
                return 0;
            };
            match FileSource::open(&path, self.target, self.frames_per_buffer) {
                Ok(src) => {
                    self.current = Some(src);
                    self.current_path = Some(path.clone());
                    log::info!("request queue: playing {}", path.display());
                }
                Err(e) => log::warn!("request queue: cannot play {}: {e}", path.display()),
            }
        }
    }

    fn is_exhausted(&self) -> bool {
        self.current.is_none() && self.queue.is_empty()
    }

    fn label(&self) -> Option<String> {
        self.current_path
            .as_ref()
            .map(|p| p.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn queue_is_a_fifo() {
        let q = RequestQueue::new();
        q.push("/a.mp3".into());
        q.push("/b.mp3".into());
        assert_eq!(q.len(), 2);
        assert_eq!(q.list(), vec![PathBuf::from("/a.mp3"), PathBuf::from("/b.mp3")]);
        assert_eq!(q.pop(), Some(PathBuf::from("/a.mp3")));
        assert_eq!(q.pop(), Some(PathBuf::from("/b.mp3")));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn clear_drops_pending_paths() {
        let q = RequestQueue::new();
        q.push("/a.mp3".into());
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn skip_only_applies_to_a_playing_track() {
        let q = RequestQueue::new();
        // Requested while nothing is queued: must not skip a future push.
        q.request_skip();
        q.push("/a.mp3".into());
        // pop() discards the stale skip so /a.mp3 plays normally.
        assert_eq!(q.pop(), Some(PathBuf::from("/a.mp3")));
        assert!(!q.take_skip(), "stale skip leaked into the next track");

        // Requested while a track is playing: take_skip reports it once.
        q.request_skip();
        assert!(q.take_skip());
        assert!(!q.take_skip());
    }

    #[test]
    fn plays_a_pushed_real_file() {
        // A short jingle (~12 s) so the test can drain the track fully.
        let real = PathBuf::from("jingles/mrwashingt0n-simple-radio-jingle-501090.mp3");
        if !real.exists() {
            return;
        }
        let q = Arc::new(RequestQueue::new());
        q.push(real.clone());
        let mut src = RequestQueueSource::new(
            q,
            SignalSpec::new(44_100, symphonia::core::audio::Channels::FRONT_CENTRE),
            4096,
        );
        assert!(src.label().is_none(), "no label before a track starts");
        let mut buf = vec![0f32; 4096];
        assert!(src.next_buffer(&mut buf) > 0, "file must start playing");
        assert_eq!(src.label(), Some(real.display().to_string()));
        let start = Instant::now();
        let mut total = 0usize;
        while total < 44_100 * 60 {
            let n = src.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
        }
        assert!(
            src.is_exhausted(),
            "queue not exhausted after {total} samples ({:?})",
            start.elapsed()
        );
        // The last label persists, like every other source (metadata hooks
        // fire on *changes*).
        assert_eq!(src.label(), Some(real.display().to_string()));
    }
}
