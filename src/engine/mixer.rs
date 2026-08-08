use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

use symphonia::core::audio::SignalSpec;

use crate::config::MixerConfig;
use crate::source::file::FileSource;
use crate::source::{AudioSource, SilenceSource, SourceProvider};

/// State of a crossfade that was promoted to the active source before the
/// fade had completed (e.g. the outgoing track ended early). The active source
/// keeps ramping in to full gain over the remainder of the window.
struct Tail {
    start_gain: f64,
    remaining: usize,
    total: usize,
}

/// A gapless track-to-track crossfade mixer.
///
/// Holds the currently-playing source and lazily preloads the *next* source
/// (from a [`SourceProvider`]) as soon as the active track enters the final
/// `crossfade_seconds`. Both sources are then mixed with a gain ramp:
///
/// ```text
/// MixedSample = SampleA * GainA + SampleB * GainB
/// ```
///
/// where `GainA: 1 -> 0` and `GainB: 0 -> 1` over the overlap window.
pub struct CrossfadeMixer {
    provider: Box<dyn SourceProvider>,
    active: Box<dyn AudioSource>,
    next: Option<Box<dyn AudioSource>>,
    active_label: String,
    next_label: Option<String>,
    crossfade_frames: usize,
    crossfade_seconds: f64,
    curve: f64,
    channels: usize,
    fade_pos: usize,
    tail: Option<Tail>,
    started: bool,
}

impl CrossfadeMixer {
    pub fn new(
        provider: Box<dyn SourceProvider>,
        config: &MixerConfig,
        sample_rate: u32,
        channels: usize,
    ) -> Self {
        Self {
            provider,
            active: Box::new(SilenceSource::new()),
            next: None,
            active_label: String::new(),
            next_label: None,
            crossfade_frames: (config.crossfade_seconds * sample_rate as f64).max(1.0) as usize,
            crossfade_seconds: config.crossfade_seconds,
            curve: config.fade_curve,
            channels,
            fade_pos: 0,
            tail: None,
            started: false,
        }
    }

    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        if self.provider.has_next() {
            let (src, label) = self.provider.next_source();
            self.active = src;
            self.active_label = label;
        }
    }

    fn preload_next(&mut self) {
        if self.next.is_none() && self.provider.has_next() {
            let (src, label) = self.provider.next_source();
            log::info!("crossfade: preloading next track");
            self.next = Some(src);
            self.next_label = Some(label);
            self.fade_pos = 0;
            self.tail = None;
        }
    }
}

impl AudioSource for CrossfadeMixer {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        loop {
            self.ensure_started();

            // Preload the next source once the active track nears its end.
            if self.next.is_none() {
                let due = match self.active.remaining_seconds() {
                    Some(rem) => rem <= self.crossfade_seconds,
                    None => self.active.is_exhausted(),
                };
                if due {
                    self.preload_next();
                }
            }

            let wanted = buffer.len();
            let mut a = vec![0f32; wanted];
            let mut b = vec![0f32; wanted];
            let n_a = self.active.next_buffer(&mut a);

            // The active track ended before a successor was preloaded (e.g. it
            // consumed its last data mid-fade or `remaining_seconds` was
            // unavailable). Pull the next track immediately instead of
            // stalling on silence.
            if n_a == 0 && self.active.is_exhausted() && self.next.is_none() {
                if self.provider.has_next() {
                    let (src, label) = self.provider.next_source();
                    log::info!("crossfade: track ended early, advancing to {label}");
                    self.active = src;
                    self.active_label = label;
                    continue;
                }
                return 0;
            }

        // Tail ramp: a crossfade was promoted mid-way; keep fading the new
            // active source in to full gain, frame by frame.
            if let Some(tail) = self.tail.as_mut() {
            let chans = self.channels;
            let frames = n_a / chans;
            for f in 0..frames {
                let progress = 1.0 - tail.remaining as f64 / tail.total.max(1) as f64;
                let ramp = tail.start_gain + (1.0 - tail.start_gain) * progress;
                for ch in 0..chans {
                    buffer[f * chans + ch] = (a[f * chans + ch] as f64 * ramp) as f32;
                }
                tail.remaining = tail.remaining.saturating_sub(1);
            }
            if tail.remaining == 0 {
                self.tail = None;
            }
            return n_a;
        }

        if let Some(next) = self.next.as_mut() {
                let n_b = next.next_buffer(&mut b);

            let out_len = n_a.max(n_b);
                let chans = self.channels;
            let frames_out = out_len / chans;
            let cf = self.crossfade_frames.max(1) as f64;
            for i in 0..out_len {
                let f = i / chans;
                let t = ((self.fade_pos + f) as f64 / cf).clamp(0.0, 1.0);
                let gain_b = t.powf(self.curve);
                let gain_a = (1.0 - t).powf(self.curve);
                buffer[i] = (a[i] as f64 * gain_a + b[i] as f64 * gain_b) as f32;
            }
            self.fade_pos += frames_out;

                if self.active.is_exhausted() {
                    let promoted = self.next.take().expect("next must exist");
                self.active = promoted;
                self.active_label = self.next_label.take().unwrap_or_default();
                if self.fade_pos < self.crossfade_frames {
                        let remaining = self.crossfade_frames - self.fade_pos;
                        let t_end = self.fade_pos as f64 / cf;
                        let gain_b = t_end.powf(self.curve);
                        self.tail = Some(Tail {
                            start_gain: gain_b,
                            remaining,
                            total: remaining,
                        });
                    }
                }
                return out_len;
            }

            // No crossfade in progress: plain passthrough.
            buffer[..n_a].copy_from_slice(&a[..n_a]);
            return n_a;
        }
    }

    fn is_exhausted(&self) -> bool {
        if !self.started {
            return !self.provider.has_next();
        }
        self.active.is_exhausted()
            && self.next.as_ref().map(|n| n.is_exhausted()).unwrap_or(true)
            && !self.provider.has_next()
    }

    fn label(&self) -> Option<String> {
        Some(self.active_label.clone())
    }
}

/// Commands sent to the [`PriorityMixer`] from the live harbor / jingle trigger.
pub enum MixCommand {
    SetLive(Box<dyn AudioSource>),
    ClearLive,
    PlayJingle(PathBuf),
    Shutdown,
}

/// A priority mixer combining the playlist (via [`CrossfadeMixer`]) with live
/// DJ input and one-shot jingles.
///
/// When a DJ connects the output fades from the playlist into the live source
/// over `duck_seconds`; when the DJ disconnects it fades back into whatever the
/// playlist is playing. Jingles fade over the music the same way but lose to a
/// live DJ.
pub struct PriorityMixer {
    main: Box<dyn AudioSource>,
    rx: Receiver<MixCommand>,
    live: Option<Box<dyn AudioSource>>,
    jingle: Option<Box<dyn AudioSource>>,
    /// Current override gain in `[0, 1]` (0 = music, 1 = override).
    gain: f64,
    rising: bool,
    falling: bool,
    override_started: bool,
    duck_step_per_frame: f64,
    target_spec: SignalSpec,
    frames_per_buffer: usize,
    shutdown: bool,
}

impl PriorityMixer {
    pub fn new(
        main: Box<dyn AudioSource>,
        rx: Receiver<MixCommand>,
        config: &MixerConfig,
        target_spec: SignalSpec,
        frames_per_buffer: usize,
    ) -> Self {
        Self {
            main,
            rx,
            live: None,
            jingle: None,
            gain: 0.0,
            rising: false,
            falling: false,
            override_started: false,
            duck_step_per_frame: 1.0 / (config.duck_seconds * target_spec.rate as f64).max(1.0),
            target_spec,
            frames_per_buffer,
            shutdown: false,
        }
    }

    /// True after a [`MixCommand::Shutdown`] was received.
    pub fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    fn drain_commands(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(MixCommand::SetLive(src)) => {
                    log::info!("priority mixer: DJ connected");
                    self.live = Some(src);
                    self.override_started = false;
                    self.jingle = None;
                    self.rising = true;
                    self.falling = false;
                }
                Ok(MixCommand::ClearLive) => {
                    log::info!("priority mixer: DJ disconnected");
                    self.rising = false;
                    self.falling = true;
                }
                Ok(MixCommand::PlayJingle(path)) => {
                    if self.live.is_some() {
                        log::info!("priority mixer: ignoring jingle while DJ is live");
                        continue;
                    }
                    match FileSource::open(&path, self.target_spec, self.frames_per_buffer) {
                        Ok(src) => {
                            log::info!("priority mixer: playing jingle {}", path.display());
                            self.jingle = Some(Box::new(src));
                            self.override_started = false;
                            self.rising = true;
                            self.falling = false;
                        }
                        Err(e) => log::warn!("failed to open jingle {}: {e}", path.display()),
                    }
                }
                Ok(MixCommand::Shutdown) => {
                    log::info!("priority mixer: shutdown requested");
                    self.shutdown = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }

    fn read_override(&mut self, out: &mut [f32]) -> usize {
        if let Some(src) = self.live.as_mut() {
            return src.next_buffer(out);
        }
        if let Some(src) = self.jingle.as_mut() {
            return src.next_buffer(out);
        }
        0
    }

    fn step_gain(&mut self, n_out: usize) {
        let frames = n_out / self.target_spec.channels.count();
        let step = self.duck_step_per_frame * frames as f64;
        if self.rising && self.override_started {
            self.gain = (self.gain + step).min(1.0);
            if self.gain >= 1.0 {
                self.rising = false;
            }
        } else if self.falling {
            self.gain = (self.gain - step).max(0.0);
            if self.gain <= 0.0 {
                self.falling = false;
                self.live = None;
                self.jingle = None;
                self.override_started = false;
            }
        }
    }
}

impl AudioSource for PriorityMixer {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        self.drain_commands();

        let wanted = buffer.len();
        let mut m = vec![0f32; wanted];
        let mut o = vec![0f32; wanted];

        let n_m = self.main.next_buffer(&mut m);
        let n_o = self.read_override(&mut o);
        if n_o > 0 {
            self.override_started = true;
        }

        // An override that has finished fades back out. Jingles end naturally;
        // a live source ends when the DJ disconnects (ClearLive is sent by the
        // harbor, but this also covers any tail of buffered audio still being
        // drained at that point).
        let override_ended = match self.live.as_ref() {
            Some(l) => l.is_exhausted(),
            None => self.jingle.as_ref().map(|j| j.is_exhausted()).unwrap_or(false),
        };
        if override_ended && self.gain > 0.0 && !self.falling {
            self.rising = false;
            self.falling = true;
        }

        let out_len = n_m.max(n_o);
        for i in 0..out_len {
            buffer[i] =
                (m[i] as f64 * (1.0 - self.gain) + o[i] as f64 * self.gain) as f32;
        }
        self.step_gain(out_len);
        out_len
    }

    fn is_exhausted(&self) -> bool {
        self.main.is_exhausted()
    }

    fn label(&self) -> Option<String> {
        let override_live = self.gain > 0.5 && self.live.is_some();
        if override_live {
            Some("LIVE DJ".into())
        } else if let Some(j) = self.jingle.as_ref() {
            if self.gain > 0.5 {
                j.label()
            } else {
                self.main.label()
            }
        } else {
            self.main.label()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    const RATE: usize = 100;
    const CHANS: usize = 2;

    struct FakeSource {
        value: f32,
        total_frames: usize,
        pos_frames: usize,
    }

    impl AudioSource for FakeSource {
        fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
            let want = buffer.len() / CHANS;
            let avail = self.total_frames.saturating_sub(self.pos_frames);
            let n_frames = avail.min(want);
            let n = n_frames * CHANS;
            buffer[..n].fill(self.value);
            self.pos_frames += n_frames;
            n
        }
        fn is_exhausted(&self) -> bool {
            self.pos_frames >= self.total_frames
        }
        fn remaining_seconds(&self) -> Option<f64> {
            Some((self.total_frames - self.pos_frames) as f64 / RATE as f64)
        }
        fn label(&self) -> Option<String> {
            Some(format!("src({})", self.value))
        }
    }

    struct FakeProvider {
        sources: Vec<Box<dyn AudioSource>>,
    }

    impl FakeProvider {
        fn new(values: Vec<(f32, usize)>) -> Self {
            let sources = values
                .into_iter()
                .map(|(v, total)| -> Box<dyn AudioSource> {
                    Box::new(FakeSource {
                        value: v,
                        total_frames: total,
                        pos_frames: 0,
                    })
                })
                .collect();
            Self { sources }
        }
    }

    impl SourceProvider for FakeProvider {
        fn next_source(&mut self) -> (Box<dyn AudioSource>, String) {
            let src = self.sources.remove(0);
            let label = src.label().unwrap();
            (src, label)
        }
        fn has_next(&self) -> bool {
            !self.sources.is_empty()
        }
    }

    fn mixer_config(crossfade: f64) -> MixerConfig {
        MixerConfig {
            crossfade_seconds: crossfade,
            fade_curve: 1.0,
            duck_seconds: 1.0,
        }
    }

    #[test]
    fn seamless_crossfade_with_gain_ramp() {
        // Track A: 1.0s (100 frames), track B: value 2.0.
        // Crossfade window: 0.2s (20 frames), buffers of 10 frames.
        let provider = Box::new(FakeProvider::new(vec![(1.0, 100), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);

        let mut buf = vec![0f32; 10 * CHANS];
        let n = mix.next_buffer(&mut buf);
        assert_eq!(n, 20);

        // Buffer 1: remaining 0.9s > 0.2s -> passthrough A.
        assert!((buf[0] - 1.0).abs() < 1e-6);

        // Buffers 2..=8: passthrough A (remaining 0.8 -> 0.2).
        for _ in 0..7 {
            mix.next_buffer(&mut buf);
        }
        assert!((buf[0] - 1.0).abs() < 1e-6);

        // Buffer 9: remaining == 0.2s -> preload B, fade begins (t=0).
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);

        // Buffer 10: t=0.5 -> 0.5*A + 0.5*B = 1.5.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.5).abs() < 1e-6);

        // Buffer 11: t=1.0 -> B only = 2.0, A now exhausted and promoted.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);

        // Buffer 12: B continues at full gain.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn tail_ramp_when_track_ends_mid_fade() {
        // A is only one buffer long (10 frames) while the crossfade window is
        // 20 frames, so it is promoted while the fade is only half done.
        let provider = Box::new(FakeProvider::new(vec![(1.0, 10), (2.0, 1000)]));
        let cfg = mixer_config(0.2);
        let mut mix = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);

        let mut buf = vec![0f32; 10 * CHANS];
        mix.next_buffer(&mut buf);
        // A.remaining = 0.1s <= 0.2s -> preload B. Frame 0: t=0 -> 1.0.
        assert!((buf[0] - 1.0).abs() < 1e-6);
        // Frame 9 (samples 18/19): t=0.45 -> 0.55*A + 0.45*B = 0.55 + 0.90 = 1.45.
        assert!((buf[18] - 1.45).abs() < 1e-6);
        assert!((buf[19] - 1.45).abs() < 1e-6);
        // A exhausted -> promoted with tail(start_gain=0.5, remaining=10).

        // Tail ramp 0.5 -> 1.0 across the remaining 10 frames.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6); // 2.0 * 0.5
        assert!((buf[18] - 1.9).abs() < 1e-6); // 2.0 * (0.5 + 0.5*0.9)
        assert!((buf[19] - 1.9).abs() < 1e-6);

        // B at full gain.
        mix.next_buffer(&mut buf);
        assert!((buf[0] - 2.0).abs() < 1e-6);
    }

    #[test]
    fn priority_mixer_fades_dj_in_and_out() {
        let (tx, rx) = mpsc::channel();
        let main = Box::new(FakeSource {
            value: 1.0,
            total_frames: 100_000,
            pos_frames: 0,
        });
        let cfg = mixer_config(0.1);
        let cfg = MixerConfig {
            duck_seconds: 0.1,
            ..cfg
        };
        let spec = symphonia::core::audio::SignalSpec::new(RATE as u32, symphonia::core::audio::Channels::FRONT_LEFT | symphonia::core::audio::Channels::FRONT_RIGHT);
        let mut pm = PriorityMixer::new(main, rx, &cfg, spec, 10);

        // Main only.
        let mut buf = vec![0f32; 10 * CHANS];
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);

        // DJ connects (value 3.0).
        let dj = FakeSource {
            value: 3.0,
            total_frames: 100_000,
            pos_frames: 0,
        };
        tx.send(MixCommand::SetLive(Box::new(dj))).unwrap();

        // duck_seconds=0.1 -> duck_frames=10, one buffer is 10 frames.
        // The gain is stepped *after* mixing, so the first buffer after the
        // command still mixes at gain 0 (playlist), then full takeover.
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 3.0).abs() < 1e-6);

        // DJ disconnects: the buffer after ClearLive still mixes at gain 1.0
        // (pre-step), then the gain drops to 0 and the playlist returns.
        tx.send(MixCommand::ClearLive).unwrap();
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 3.0).abs() < 1e-6);
        pm.next_buffer(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn plays_a_real_jingle_file() {

        use std::path::PathBuf;
        let jingle = PathBuf::from("jingles/mrwashingt0n-radio-for-all-trance-505921.mp3");
        if !jingle.exists() {
            return; // not available in CI-less env; skip
        }
        let provider = Box::new(FakeProvider::new(vec![(0.5, 100000)]));
        let cfg = mixer_config(0.2);
        let cross = CrossfadeMixer::new(provider, &cfg, RATE as u32, CHANS);
        let (tx, rx) = mpsc::channel();
        let spec = symphonia::core::audio::SignalSpec::new(RATE as u32, symphonia::core::audio::Channels::FRONT_LEFT | symphonia::core::audio::Channels::FRONT_RIGHT);
        let mut pm = PriorityMixer::new(Box::new(cross), rx, &cfg, spec, 100);
        tx.send(MixCommand::PlayJingle(jingle)).unwrap();
        // Duck ramp is 1.0s (10 buffers), jingle is ~12s (124 buffers):
        // buffers 20..120 are pure jingle (music fully ducked).
        let mut buf = vec![0f32; 100 * CHANS];
        let mut seen_label = false;
        for i in 0..140 {
            pm.next_buffer(&mut buf);
            if let Some(l) = pm.label() {
                if l.contains("mrwashingt0n") {
                    seen_label = true;
                }
            }
            if (20..120).contains(&i) {
                let e: f32 = buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32;
                if i == 40 {
                    assert!(e > 0.01, "jingle window silent (energy {e})");
                }
            }
        }
        assert!(seen_label, "mixer never reached jingle label");
    }

    #[test]
    fn jingle_reaches_the_opus_encoder_end_to_end() {
        use std::path::PathBuf;
        let jingle = PathBuf::from("jingles/mrwashingt0n-radio-for-all-trance-505921.mp3");
        if !jingle.exists() {
            return; // repo-checkout only
        }
        // Playlist of the real media dir -> crossfade -> priority mixer -> opus.
        let spec = symphonia::core::audio::SignalSpec::new(
            44100,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let media = std::path::Path::new("media");
        let mut files: Vec<_> = std::fs::read_dir(media)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "mp3").unwrap_or(false))
            .collect();
        files.sort();
        let playlist =
            crate::source::playlist::Playlist::new(files, false, true, spec, 4096, None);
        let cfg = mixer_config(0.2);
        let cross = CrossfadeMixer::new(Box::new(playlist), &cfg, 44100, 2);
        let (tx, rx) = mpsc::channel();
        let mut pm = PriorityMixer::new(Box::new(cross), rx, &cfg, spec, 4096);
        tx.send(MixCommand::PlayJingle(jingle)).unwrap();

        let mut enc =
            crate::output::encoder::create_encoder(crate::config::OutputFormat::Opus, 44100, 2, 128_000, "e2e")
                .unwrap();
        let mut buf = vec![0f32; 4096 * 2];
        let mut all = Vec::new();
        let mut jingle_audio = false;
        for _ in 0..600 {
            pm.next_buffer(&mut buf);
            let out = enc.encode(&buf);
            if out.is_empty() {
                continue;
            }
            let jingle_active = pm
                .label()
                .map(|l| l.contains("mrwashingt0n"))
                .unwrap_or(false);
            if jingle_active {
                let e: f32 = buf.iter().map(|v| v * v).sum::<f32>() / buf.len() as f32;
                if e > 0.005 {
                    jingle_audio = true;
                }
            }
            all.extend_from_slice(&out);
        }
        all.extend_from_slice(&enc.finish());
        assert!(jingle_audio, "no audible jingle audio reached the encoder");
        assert!(all.len() > 100_000);
        if let Some(out) = std::env::var_os("CRABSOUP_DUMP") {
            std::fs::write(out, &all).unwrap();
        }
    }
}
