//! The `annotated` wrapper: per-track follower tracks (`append`/`prepend`).
//!
//! The wrapper sits between a child source and the mixer and inserts a
//! one-shot track at every child track boundary: the current track's
//! `append` follower plays after it ends, the next track's `prepend`
//! follower plays before it starts. Followers come from the `annotate:`
//! prefix on the track's request URI, or from the operator's defaults when
//! the track carries none; the literal `"false"` inhibits a default.
//!
//! A child like `sequence` or `request.queue` advances to its next track
//! inside a single `next_buffer` call, so the boundary is detected by the
//! child's label changing during a pull: the first buffer of the new track
//! is staged and held back while the followers play.

use crate::request::{RequestConfig, RequestUri, resolve};
use crate::source::{AudioSource, CrossfadeOverrides};

/// The per-track follower setting for one direction: the track's own
/// `append`/`prepend` annotation, falling back to the operator default,
/// with `"false"` meaning "no follower for this track".
fn follower_setting(
    overrides: &Option<CrossfadeOverrides>,
    append: bool,
    default: &Option<String>,
) -> Option<String> {
    let cue = overrides.as_ref().and_then(|o| {
        if append {
            o.append.clone()
        } else {
            o.prepend.clone()
        }
    });
    match cue {
        None => default.clone(),
        Some(v) if v == "false" => None,
        Some(v) => Some(v),
    }
}

/// Insert the `append`/`prepend` follower tracks of a wrapped source.
pub struct FollowSource {
    child: Box<dyn AudioSource>,
    default_append: Option<String>,
    default_prepend: Option<String>,
    request: RequestConfig,
    target: symphonia::core::audio::SignalSpec,
    frames_per_buffer: usize,
    /// A follower currently playing (the last track's append, or the
    /// final append when the child has ended).
    one_shot: Option<Box<dyn AudioSource>>,
    /// A resolved prepend waiting to play before the child's next track.
    awaiting_prepend: Option<Box<dyn AudioSource>>,
    /// The new track's first buffer, held while followers play (the
    /// boundary-crossing pull's output).
    staged: Option<Vec<f32>>,
    /// Label of the child's current track; `None` before the first pull.
    last_label: Option<String>,
    /// The append follower of the child's current track, played when the
    /// track ends.
    cur_append: Option<String>,
}

impl FollowSource {
    pub fn new(
        child: Box<dyn AudioSource>,
        default_append: Option<String>,
        default_prepend: Option<String>,
        request: RequestConfig,
        target: symphonia::core::audio::SignalSpec,
        frames_per_buffer: usize,
    ) -> Self {
        Self {
            child,
            default_append,
            default_prepend,
            request,
            target,
            frames_per_buffer,
            one_shot: None,
            awaiting_prepend: None,
            staged: None,
            last_label: None,
            cur_append: None,
        }
    }

    /// Resolve a follower request URI, logging and skipping on failure.
    fn resolve(&self, uri: &str) -> Option<Box<dyn AudioSource>> {
        match resolve(&RequestUri::new(uri), &self.request, self.target, self.frames_per_buffer) {
            Ok(src) => {
                log::info!("annotated: follower {uri}");
                Some(src)
            }
            Err(e) => {
                log::warn!("annotated: cannot play follower {uri}: {e}");
                None
            }
        }
    }

    /// Capture the follower cues for the boundary that just passed and
    /// queue the followers: the old track's `append` first, then the new
    /// track's `prepend`. `label` is the child's label for the new track.
    fn queue_followers(&mut self, label: Option<String>) -> bool {
        let overrides = self.child.crossfade_overrides();
        let old_append = self.cur_append.take();
        self.cur_append = follower_setting(&overrides, true, &self.default_append);
        let new_prepend = follower_setting(&overrides, false, &self.default_prepend);
        self.last_label = label;
        if let Some(uri) = old_append
            && let Some(src) = self.resolve(&uri)
        {
            self.one_shot = Some(src);
            return true;
        }
        if let Some(uri) = new_prepend
            && let Some(src) = self.resolve(&uri)
        {
            self.awaiting_prepend = Some(src);
            return true;
        }
        false
    }

    /// True while a follower is queued or playing (an exhausted child must
    /// not end the stream yet).
    fn follower_playing(&self) -> bool {
        self.one_shot.is_some() || self.awaiting_prepend.is_some()
    }
}

impl AudioSource for FollowSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        loop {
            // Play queued followers first: the old track's append, then
            // the new track's prepend.
            if let Some(one) = self.one_shot.as_mut() {
                let n = one.next_buffer(buffer);
                if n > 0 {
                    return n;
                }
                self.one_shot = None;
            }
            if let Some(prepend) = self.awaiting_prepend.as_mut() {
                let n = prepend.next_buffer(buffer);
                if n > 0 {
                    return n;
                }
                self.awaiting_prepend = None;
            }
            // The new track's first buffer was held behind the followers.
            if let Some(staged) = self.staged.take() {
                let n = staged.len();
                buffer[..n].copy_from_slice(&staged);
                return n;
            }
            // Pull the child. A label change during the pull means the
            // child crossed into its next track (or just started): stage
            // that buffer and let the followers play first.
            let n = self.child.next_buffer(buffer);
            let after = self.child.label();
            if after != self.last_label {
                self.queue_followers(after);
                if n > 0 {
                    self.staged = Some(buffer[..n].to_vec());
                }
                continue;
            }
            if n > 0 {
                return n;
            }
            // The child is truly done: play its final append once, then end.
            if self.child.is_exhausted() {
                if let Some(uri) = self.cur_append.take()
                    && let Some(src) = self.resolve(&uri)
                {
                    self.one_shot = Some(src);
                    continue;
                }
                return 0;
            }
            return 0;
        }
    }

    fn is_exhausted(&self) -> bool {
        self.child.is_exhausted() && !self.follower_playing() && self.staged.is_none()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.child.remaining_seconds()
    }

    fn label(&self) -> Option<String> {
        if let Some(one) = &self.one_shot {
            return one.label();
        }
        if let Some(prepend) = &self.awaiting_prepend {
            return prepend.label();
        }
        self.child.label()
    }

    fn next_label(&self) -> Option<String> {
        self.child.next_label()
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.child.replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<crate::source::CrossfadeOverrides> {
        self.child.crossfade_overrides()
    }

    fn skip(&mut self) {
        self.one_shot = None;
        self.awaiting_prepend = None;
        self.staged = None;
        self.child.skip();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follower_setting_merges_cues_and_defaults() {
        let none = None;
        let plain = Some(CrossfadeOverrides {
            fade_in: None,
            fade_out: None,
            start_next: None,
            append: None,
            prepend: None,
        });
        let inhibited = Some(CrossfadeOverrides {
            fade_in: None,
            fade_out: None,
            start_next: None,
            append: Some("false".into()),
            prepend: None,
        });
        let overridden = Some(CrossfadeOverrides {
            fade_in: None,
            fade_out: None,
            start_next: None,
            append: Some("other.wav".into()),
            prepend: None,
        });
        let default = Some("stinger.mp3".into());
        assert_eq!(follower_setting(&none, true, &default), default);
        assert_eq!(follower_setting(&plain, true, &default), default);
        assert_eq!(follower_setting(&inhibited, true, &default), None);
        assert_eq!(
            follower_setting(&overridden, true, &default).as_deref(),
            Some("other.wav")
        );
        assert_eq!(follower_setting(&plain, false, &None), None);
    }
}