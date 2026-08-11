//! Request queue (Liquidsoap `request.queue`): a FIFO of media requests
//! (local paths or `http://` URLs) pushed at runtime via the telnet port,
//! played ahead of the playlist when non-empty.
//!
//! The queue state is shared: the control port pushes paths and requests
//! skips; the [`RequestQueueSource`] (pulled on the tap thread) pops them
//! and resolves each one (downloading URLs to a temp file).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use symphonia::core::audio::SignalSpec;

use crate::request::{resolve, RequestConfig, RequestUri};
use crate::source::AudioSource;

/// Shared FIFO state. Methods used by the control port (`push`/`list`/
/// `clear`/`request_skip`) and by the source (`pop`/`take_skip`).
pub struct RequestQueue {
    inner: Mutex<QueueState>,
}

#[derive(Default)]
struct QueueState {
    requests: VecDeque<RequestUri>,
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

    /// Append a request at the end of the queue.
    pub fn push(&self, uri: RequestUri) {
        self.inner.lock().unwrap().requests.push_back(uri);
    }

    /// Number of queued (not yet playing) requests.
    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().requests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy of the queued requests, oldest first.
    pub fn list(&self) -> Vec<RequestUri> {
        self.inner.lock().unwrap().requests.iter().cloned().collect()
    }

    /// Drop every queued request (a playing track is unaffected).
    pub fn clear(&self) {
        self.inner.lock().unwrap().requests.clear();
    }

    /// Tell the source to skip the queued track it is playing.
    pub fn request_skip(&self) {
        self.inner.lock().unwrap().skip = true;
    }

    /// Pop the next request, dropping any pending skip (a skip only applies
    /// to a track already playing).
    fn pop(&self) -> Option<RequestUri> {
        let mut st = self.inner.lock().unwrap();
        st.skip = false;
        st.requests.pop_front()
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

/// Plays queued requests FIFO. Exhausts whenever the queue is empty; a
/// control `queue.skip` drops the track currently playing (if any) and
/// advances.
pub struct RequestQueueSource {
    queue: Arc<RequestQueue>,
    request: RequestConfig,
    target: SignalSpec,
    frames_per_buffer: usize,
    current: Option<Box<dyn AudioSource>>,
    current_uri: Option<RequestUri>,
}

impl RequestQueueSource {
    pub fn new(
        queue: Arc<RequestQueue>,
        request: RequestConfig,
        target: SignalSpec,
        frames_per_buffer: usize,
    ) -> Self {
        Self {
            queue,
            request,
            target,
            frames_per_buffer,
            current: None,
            current_uri: None,
        }
    }

    /// The URI currently playing (metadata label), if any.
    pub fn current_uri(&self) -> Option<&RequestUri> {
        self.current_uri.as_ref()
    }
}

impl AudioSource for RequestQueueSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        loop {
            if let Some(src) = self.current.as_mut() {
                if self.queue.take_skip() {
                    log::info!(
                        "request queue: skipping {}",
                        self.current_uri.as_ref().map(|u| u.display()).unwrap_or_default()
                    );
                    self.current = None;
                    continue;
                }
                let n = src.next_buffer(buffer);
                if n > 0 {
                    return n;
                }
                if src.is_exhausted() {
                    log::info!(
                        "request queue: finished {}",
                        self.current_uri.as_ref().map(|u| u.display()).unwrap_or_default()
                    );
                    self.current = None;
                    continue;
                }
                return 0;
            }
            let Some(uri) = self.queue.pop() else {
                return 0;
            };
            match resolve(&uri, &self.request, self.target, self.frames_per_buffer) {
                Ok(src) => {
                    self.current = Some(src);
                    self.current_uri = Some(uri.clone());
                    log::info!("request queue: playing {}", uri.display());
                }
                Err(e) => {
                    log::warn!("request queue: cannot play {}: {e}", uri.display());
                    // Drop the bad request and move on to the next one.
                    continue;
                }
            }
        }
    }

    fn is_exhausted(&self) -> bool {
        self.current.is_none() && self.queue.is_empty()
    }

    fn label(&self) -> Option<String> {
        self.current_uri
            .as_ref()
            .map(|uri| uri.display())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.current.as_ref().and_then(|c| c.replaygain_db())
    }

    fn crossfade_overrides(&self) -> Option<(Option<f64>, Option<f64>)> {
        self.current.as_ref().and_then(|c| c.crossfade_overrides())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn queue_is_a_fifo() {
        let q = RequestQueue::new();
        q.push(RequestUri::new("/a.mp3"));
        q.push(RequestUri::new("/b.mp3"));
        assert_eq!(q.len(), 2);
        assert_eq!(
            q.list(),
            vec![RequestUri::new("/a.mp3"), RequestUri::new("/b.mp3")]
        );
        assert_eq!(q.pop(), Some(RequestUri::new("/a.mp3")));
        assert_eq!(q.pop(), Some(RequestUri::new("/b.mp3")));
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn clear_drops_pending_paths() {
        let q = RequestQueue::new();
        q.push(RequestUri::new("/a.mp3"));
        q.clear();
        assert!(q.is_empty());
        assert_eq!(q.pop(), None);
    }

    #[test]
    fn skip_only_applies_to_a_playing_track() {
        let q = RequestQueue::new();
        // Requested while nothing is queued: must not skip a future push.
        q.request_skip();
        q.push(RequestUri::new("/a.mp3"));
        // pop() discards the stale skip so /a.mp3 plays normally.
        assert_eq!(q.pop(), Some(RequestUri::new("/a.mp3")));
        assert!(!q.take_skip(), "stale skip leaked into the next track");

        // Requested while a track is playing: take_skip reports it once.
        q.request_skip();
        assert!(q.take_skip());
        assert!(!q.take_skip());
    }

    #[test]
    fn plays_a_pushed_real_file() {
        // A short jingle (~12 s) so the test can drain the track fully.
        let real = RequestUri::new("jingles/mrwashingt0n-simple-radio-jingle-501090.mp3");
        let RequestUri::Local(path, _) = &real else {
            return;
        };
        if !path.exists() {
            return;
        }
        let q = Arc::new(RequestQueue::new());
        q.push(real.clone());
        let mut src = RequestQueueSource::new(
            q,
            RequestConfig::default(),
            SignalSpec::new(44_100, symphonia::core::audio::Channels::FRONT_CENTRE),
            4096,
        );
        assert!(src.label().is_none(), "no label before a track starts");
        let mut buf = vec![0f32; 4096];
        assert!(src.next_buffer(&mut buf) > 0, "file must start playing");
        assert_eq!(src.label(), Some(real.display()));
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
        assert_eq!(src.label(), Some(real.display()));
    }
}
