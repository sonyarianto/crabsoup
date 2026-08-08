use std::path::PathBuf;

use log::{info, warn};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use symphonia::core::audio::SignalSpec;

use crate::source::file::FileSource;
use crate::source::{AudioSource, SilenceSource, SourceProvider};

/// A scheduled queue of media files. Exposes each file as its own source via
/// [`SourceProvider`], so the mixer can preload the *next* track for a
/// crossfade while the current one still plays.
#[derive(Clone)]
pub struct Playlist {
    queue: Vec<PathBuf>,
    next_index: usize,
    loop_playlist: bool,
    target: SignalSpec,
    frames_per_buffer: usize,
    rng: Option<SmallRng>,
}

impl Playlist {
    /// `seed` makes shuffle deterministic (used by tests).
    pub fn new(
        mut files: Vec<PathBuf>,
        shuffle: bool,
        loop_playlist: bool,
        target: SignalSpec,
        frames_per_buffer: usize,
        seed: Option<u64>,
    ) -> Self {
        files.sort();
        if shuffle {
            let mut rng = seed.map(SmallRng::seed_from_u64).unwrap_or_else(SmallRng::from_entropy);
            for i in (1..files.len()).rev() {
                let j = rng.gen_range(0..=i);
                files.swap(i, j);
            }
        }
        Self {
            queue: files,
            next_index: 0,
            loop_playlist,
            target,
            frames_per_buffer,
            rng: seed.map(SmallRng::seed_from_u64),
        }
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

    fn take_path(&mut self) -> Option<PathBuf> {
        if self.queue.is_empty() {
            return None;
        }
        let path = self.queue[self.next_index % self.queue.len()].clone();
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
        Some(path)
    }
}

impl SourceProvider for Playlist {
    fn next_source(&mut self) -> (Box<dyn AudioSource>, String) {
        let Some(path) = self.take_path() else {
            let src: Box<dyn AudioSource> = Box::new(SilenceSource::new());
            return (src, "empty playlist".into());
        };

        let label = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into());

        match FileSource::open(&path, self.target, self.frames_per_buffer) {
            Ok(source) => {
                info!("loading track: {} ({})", path.display(), label);
                (Box::new(source), label)
            }
            Err(e) => {
                warn!("failed to open {}: {e}", path.display());
                (Box::new(SilenceSource::new()), label)
            }
        }
    }

    fn has_next(&self) -> bool {
        !self.queue.is_empty() && (self.loop_playlist || self.next_index < self.queue.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symphonia::core::audio::Channels;

    fn spec() -> SignalSpec {
        SignalSpec::new(44100, Channels::FRONT_LEFT | Channels::FRONT_RIGHT)
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn provides_every_track_then_stops() {
        let mut pl = Playlist::new(paths(&["a.wav", "b.wav", "c.wav"]), false, false, spec(), 4096, None);
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
        let mut pl = Playlist::new(paths(&["a.wav"]), false, true, spec(), 4096, None);
        let mut seen = Vec::new();
        for _ in 0..3 {
            let (_, label) = pl.next_source();
            seen.push(label);
        }
        assert_eq!(seen, vec!["a", "a", "a"]);
    }

    #[test]
    fn shuffle_is_deterministic_with_seed() {
        let a = Playlist::new(paths(&["a", "b", "c", "d", "e"]), true, false, spec(), 4096, Some(42));
        let b = Playlist::new(paths(&["a", "b", "c", "d", "e"]), true, false, spec(), 4096, Some(42));
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
