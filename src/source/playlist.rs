use log::{info, warn};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use std::sync::Arc;

use symphonia::core::audio::SignalSpec;

use crate::request::{RequestConfig, RequestUri, resolve};
use crate::source::request::{RequestQueue, UPCOMING_WINDOW};
use crate::source::{AudioSource, SilenceSource, SourceProvider};

/// A scheduled queue of media requests (local files or `http://` URLs).
/// Exposes each item as its own source via [`SourceProvider`], so the mixer
/// can preload the *next* track for a crossfade while the current one still
/// plays (a URL download happens inside that preload, on the puller thread).
#[derive(Clone)]
pub struct Playlist {
    queue: Vec<RequestUri>,
    next_index: usize,
    loop_playlist: bool,
    request: RequestConfig,
    target: SignalSpec,
    frames_per_buffer: usize,
    rng: Option<SmallRng>,
    /// `playlist({requests = true})`: queued requests play before the next track, through the same
    /// crossfade.
    requests: Option<Arc<RequestQueue>>,
}

impl Playlist {
    /// `seed` makes shuffle deterministic (used by tests).
    pub fn new(
        mut files: Vec<RequestUri>,
        shuffle: bool,
        loop_playlist: bool,
        request: RequestConfig,
        target: SignalSpec,
        frames_per_buffer: usize,
        seed: Option<u64>,
    ) -> Self {
        if shuffle {
            // Sorted first so a seeded shuffle is reproducible; an unshuffled playlist keeps its order.
            files.sort();
            let mut rng = seed
                .map(SmallRng::seed_from_u64)
                .unwrap_or_else(SmallRng::from_entropy);
            for i in (1..files.len()).rev() {
                let j = rng.gen_range(0..=i);
                files.swap(i, j);
            }
        }
        Self {
            queue: files,
            next_index: 0,
            loop_playlist,
            request,
            target,
            frames_per_buffer,
            rng: seed.map(SmallRng::seed_from_u64),
            requests: None,
        }
    }

    pub fn with_requests(mut self, requests: Option<Arc<RequestQueue>>) -> Self {
        self.requests = requests;
        self.publish_upcoming();
        self
    }

    fn publish_upcoming(&self) {
        let Some(requests) = &self.requests else {
            return;
        };
        let len = self.queue.len();
        let ahead = if self.loop_playlist { len } else { len.saturating_sub(self.next_index) };
        requests.set_upcoming(
            (0..ahead.min(UPCOMING_WINDOW))
                .map(|i| self.queue[(self.next_index + i) % len].clone())
                .collect(),
        );
    }

    /// Number of distinct tracks available.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Index of the track that would be handed out next.
    pub fn peek_index(&self) -> usize {
        self.next_index
    }

    fn take_uri(&mut self) -> Option<RequestUri> {
        let uri = self.pick_uri()?;
        if let Some(requests) = &self.requests {
            requests.set_incoming(uri.clone());
        }
        Some(uri)
    }

    fn pick_uri(&mut self) -> Option<RequestUri> {
        if let Some(uri) = self.requests.as_ref().and_then(|r| r.pop()) {
            return Some(uri);
        }
        if let Some(rotation) = self.requests.as_ref().and_then(|r| r.take_rotation()) {
            self.queue = rotation;
            self.next_index = 0;
        }
        if self.queue.is_empty() {
            return None;
        }
        let mut uri = self.advance();
        // Bounded, so dropping every track still plays one.
        for _ in 1..self.queue.len() {
            if !self.requests.as_ref().is_some_and(|r| r.take_dropped(&uri)) {
                break;
            }
            if !self.loop_playlist && self.next_index >= self.queue.len() {
                self.publish_upcoming();
                return None;
            }
            uri = self.advance();
        }
        self.publish_upcoming();
        Some(uri)
    }

    fn advance(&mut self) -> RequestUri {
        let uri = self.queue[self.next_index % self.queue.len()].clone();
        self.next_index += 1;
        if self.next_index >= self.queue.len() && self.loop_playlist {
            self.next_index = 0;
            // Re-shuffle for a fresh loop cycle.
            if let Some(rng) = self.rng.as_mut() {
                for i in (1..self.queue.len()).rev() {
                    let j = rng.gen_range(0..=i);
                    self.queue.swap(i, j);
                }
            }
        }
        uri
    }
}

impl SourceProvider for Playlist {
    fn next_source(&mut self) -> (Box<dyn AudioSource>, String) {
        let Some(uri) = self.take_uri() else {
            let src: Box<dyn AudioSource> = Box::new(SilenceSource::new());
            return (src, "empty playlist".into());
        };

        let label = uri.display();
        match resolve(&uri, &self.request, self.target, self.frames_per_buffer) {
            Ok(source) => {
                info!("loading track: {} ({})", uri.display(), label);
                (source, label)
            }
            Err(e) => {
                warn!("failed to load {}: {e}", uri.display());
                (Box::new(SilenceSource::new()), label)
            }
        }
    }

    fn has_next(&self) -> bool {
        self.requests.as_ref().is_some_and(|r| r.has_pending())
            || !self.queue.is_empty() && (self.loop_playlist || self.next_index < self.queue.len())
    }
}

/// A plain sequential playlist source: each track plays to exhaustion, then
/// the next is pulled. Unlike the [`crate::engine::mixer::CrossfadeMixer`]
/// there is no preload and no overlap — a `rotate`'s children must be
/// non-crossfading so the *top-level* [`crate::source::overlap::OverlapSource`]
/// can fade between tracks without fighting a second fade layer (the
/// `crossfade = false` playlist option).
pub struct PlaylistSource {
    provider: Playlist,
    current: Option<Box<dyn AudioSource>>,
    current_label: String,
    started: bool,
}

impl PlaylistSource {
    pub fn new(provider: Playlist) -> Self {
        Self {
            provider,
            current: None,
            current_label: String::new(),
            started: false,
        }
    }
}

impl AudioSource for PlaylistSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        loop {
            if self.current.is_none() && self.provider.has_next() {
                let (src, label) = self.provider.next_source();
                self.current = Some(src);
                self.current_label = label;
                self.started = true;
            }
            let Some(current) = self.current.as_mut() else {
                return 0;
            };
            let n = current.next_buffer(buffer);
            if n > 0 {
                return n;
            }
            if current.is_exhausted() {
                self.current = None;
                continue;
            }
            return 0;
        }
    }

    fn is_exhausted(&self) -> bool {
        !self.provider.has_next() && self.current.is_none()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.current.as_ref().and_then(|c| c.remaining_seconds())
    }

    fn label(&self) -> Option<String> {
        self.started.then(|| self.current_label.clone())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.current.as_ref().and_then(|c| c.replaygain_db())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::Channels;

    fn spec() -> SignalSpec {
        SignalSpec::new(44100, Channels::FRONT_LEFT | Channels::FRONT_RIGHT)
    }

    fn paths(names: &[&str]) -> Vec<RequestUri> {
        names.iter().map(|n| RequestUri::new(n)).collect()
    }

    fn looping(names: &[&str], queue: &Arc<RequestQueue>) -> Playlist {
        Playlist::new(paths(names), false, true, RequestConfig::default(), spec(), 1024, None)
            .with_requests(Some(queue.clone()))
    }

    fn once(names: &[&str], queue: &Arc<RequestQueue>) -> Playlist {
        Playlist::new(paths(names), false, false, RequestConfig::default(), spec(), 1024, None)
            .with_requests(Some(queue.clone()))
    }

    fn upcoming(queue: &RequestQueue) -> Vec<String> {
        queue.upcoming().iter().map(RequestUri::raw).collect()
    }

    #[test]
    fn an_unshuffled_playlist_keeps_the_given_order() {
        let mut pl = Playlist::new(
            paths(&["c.wav", "a.wav", "b.wav"]),
            false,
            false,
            RequestConfig::default(),
            spec(),
            1024,
            None,
        );
        assert_eq!(pl.take_uri(), Some(RequestUri::new("c.wav")));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("a.wav")));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("b.wav")));
    }

    #[test]
    fn queued_requests_play_before_the_next_playlist_track() {
        let queue = Arc::new(RequestQueue::new());
        let mut pl = looping(&["a.wav", "b.wav"], &queue);
        assert_eq!(pl.take_uri(), Some(RequestUri::new("a.wav")));
        queue.push(RequestUri::new("now.wav"));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("now.wav")));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("b.wav")));
    }

    #[test]
    fn upcoming_lists_the_next_playlist_tracks_wrapping_around_a_loop() {
        let queue = Arc::new(RequestQueue::new());
        let mut pl = looping(&["a.wav", "b.wav", "c.wav"], &queue);
        pl.take_uri();
        pl.take_uri();
        assert_eq!(upcoming(&queue), ["c.wav", "a.wav", "b.wav"]);
    }

    #[test]
    fn a_dropped_track_is_skipped_once_and_no_longer_upcoming() {
        let queue = Arc::new(RequestQueue::new());
        let mut pl = looping(&["a.wav", "b.wav", "c.wav"], &queue);
        queue.drop_upcoming("b.wav");
        assert_eq!(upcoming(&queue), ["a.wav", "c.wav"]);
        assert_eq!(pl.take_uri(), Some(RequestUri::new("a.wav")));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("c.wav")));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("a.wav")));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("b.wav")));
    }

    #[test]
    fn a_non_looping_playlist_ends_when_its_last_track_is_dropped() {
        let queue = Arc::new(RequestQueue::new());
        let mut pl = once(&["a.wav", "b.wav"], &queue);
        queue.drop_upcoming("b.wav");
        assert_eq!(pl.take_uri(), Some(RequestUri::new("a.wav")));
        assert_eq!(pl.take_uri(), None);
        assert!(!pl.has_next());
    }

    #[test]
    fn a_finished_non_looping_playlist_still_plays_requests_and_a_new_rotation() {
        let queue = Arc::new(RequestQueue::new());
        let mut pl = once(&["a.wav"], &queue);
        pl.take_uri();
        queue.push(RequestUri::new("now.wav"));
        assert!(pl.has_next());
        assert_eq!(pl.take_uri(), Some(RequestUri::new("now.wav")));
        queue.set_rotation(paths(&["x.wav"]));
        assert!(pl.has_next());
        assert_eq!(pl.take_uri(), Some(RequestUri::new("x.wav")));
    }

    #[test]
    fn a_new_rotation_is_upcoming_at_once_and_plays_from_the_next_pick() {
        let queue = Arc::new(RequestQueue::new());
        let mut pl = looping(&["a.wav", "b.wav"], &queue);
        assert_eq!(pl.take_uri(), Some(RequestUri::new("a.wav")));
        queue.set_rotation(paths(&["x.wav", "y.wav"]));
        assert_eq!(upcoming(&queue), ["x.wav", "y.wav"]);
        assert_eq!(pl.take_uri(), Some(RequestUri::new("x.wav")));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("y.wav")));
        assert_eq!(pl.take_uri(), Some(RequestUri::new("x.wav")));
    }

    #[test]
    fn the_last_track_handed_out_is_the_incoming_one() {
        let queue = Arc::new(RequestQueue::new());
        let mut pl = looping(&["a.wav", "b.wav"], &queue);
        pl.take_uri();
        queue.push(RequestUri::new("now.wav"));
        pl.take_uri();
        assert_eq!(queue.incoming(), Some(RequestUri::new("now.wav")));
    }

    #[test]
    fn provides_every_track_then_stops() {
        let mut pl = Playlist::new(
            paths(&["a.wav", "b.wav", "c.wav"]),
            false,
            false,
            RequestConfig::default(),
            spec(),
            4096,
            None,
        );
        let mut seen = Vec::new();
        while pl.has_next() {
            let (_, label) = pl.next_source();
            seen.push(label);
        }
        assert_eq!(seen, vec!["a", "b", "c"]);
        assert!(!pl.has_next());
    }

    #[test]
    fn loops_forever_without_loop_flag_disabled_but_respects_loop() {
        let mut pl = Playlist::new(
            paths(&["a.wav"]),
            false,
            true,
            RequestConfig::default(),
            spec(),
            4096,
            None,
        );
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (_, label) = pl.next_source();
            seen.push(label);
        }
        assert_eq!(seen, vec!["a", "a", "a"]);
    }

    #[test]
    fn shuffle_is_deterministic_with_seed() {
        let a = Playlist::new(
            paths(&["a", "b", "c", "d", "e"]),
            true,
            false,
            RequestConfig::default(),
            spec(),
            4096,
            Some(42),
        );
        let b = Playlist::new(
            paths(&["a", "b", "c", "d", "e"]),
            true,
            false,
            RequestConfig::default(),
            spec(),
            4096,
            Some(42),
        );
        let order = |pl: &mut Playlist| {
            let mut v = Vec::new();
            while pl.has_next() {
                let (_, l) = pl.next_source();
                v.push(l);
            }
            v
        };
        assert_eq!(order(&mut a.clone()), order(&mut b.clone()));
    }
}
