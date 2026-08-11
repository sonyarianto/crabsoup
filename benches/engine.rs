//! Criterion benchmarks for the hot paths: mixers, resampler, effects
//! chain, and encode path. Baselines are recorded in ROADMAP.md.
//!
//! Run with `cargo bench --bench engine` (and record numbers via the
//! `--save-baseline <name>` / `--load-baseline <name>` pair for later
//! comparisons).

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::mpsc;

use criterion::{BenchmarkId, Criterion, Throughput, black_box};
use parking_lot::Mutex;
use ringbuf::traits::*;
use symphonia::core::audio::{Channels, SignalSpec};

use crabsoup::config::MixerConfig;
use crabsoup::engine::effects::{Agc, Amplify, Compressor, EffectSource};
use crabsoup::engine::mixer::{CrossfadeMixer, PriorityMixer, SmartFade};
use crabsoup::output::encoder::{AacEncoder, Encoder, Mp3Encoder, OpusEncoder};
use crabsoup::resample::SincResampler;
use crabsoup::source::{AudioSource, SineSource, SourceProvider};

const RATE: u32 = 44_100;
const CHANS: usize = 2;
const FPB: usize = 4096; // frames per buffer; 8192 interleaved samples
const BUF: usize = FPB * CHANS;
/// Typical decoded packet size (symphonia MP3 frame: 1152 frames stereo).
const CHUNK: usize = 1152 * CHANS;
/// Live harbor's drop-oldest cap: 5 s of stereo audio (matches
/// `MAX_LIVE_FRAMES` in `src/live/harbor.rs`).
const CAP: usize = 5 * RATE as usize * CHANS;

/// A continuous source filling the buffer with a constant — isolates mixer /
/// ramp arithmetic from sine generation cost.
struct ConstSource {
    value: f32,
    remaining: Option<f64>,
}

impl ConstSource {
    fn new(value: f32, remaining: Option<f64>) -> Self {
        Self { value, remaining }
    }
}

impl AudioSource for ConstSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        buffer.fill(self.value);
        buffer.len()
    }
    fn is_exhausted(&self) -> bool {
        false
    }
    fn remaining_seconds(&self) -> Option<f64> {
        self.remaining
    }
}

struct FakeProvider {
    sources: Vec<Box<dyn AudioSource>>,
}

impl FakeProvider {
    fn new(count: usize, remaining: Option<f64>) -> Self {
        Self {
            sources: (0..count)
                .map(|i| -> Box<dyn AudioSource> {
                    Box::new(ConstSource::new(0.05 + i as f32 * 0.01, remaining))
                })
                .collect(),
        }
    }
}

impl SourceProvider for FakeProvider {
    fn next_source(&mut self) -> (Box<dyn AudioSource>, String) {
        let src = self.sources.remove(0);
        (src, format!("src({})", self.sources.len()))
    }
    fn has_next(&self) -> bool {
        !self.sources.is_empty()
    }
}

fn mixers(c: &mut Criterion) {
    let spec = SignalSpec::new(RATE, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
    let cfg = MixerConfig::default();
    let mut group = c.benchmark_group("mixers");
    group.throughput(Throughput::Elements(BUF as u64));

    group.bench_function("crossfade/passthrough", |b| {
        let mut mixer = CrossfadeMixer::new(
            Box::new(FakeProvider::new(2, None)),
            &cfg,
            RATE,
            CHANS,
        );
        let mut buf = vec![0.0f32; BUF];
        b.iter(|| {
            mixer.next_buffer(&mut buf);
            black_box(&buf);
        });
    });

    group.bench_function("crossfade/mixing", |b| {
        // ConstSource reports remaining = 1.0 s < crossfade window, so the
        // mixer preloads the next track and mixes every buffer (worst case).
        let mut mixer = CrossfadeMixer::new(
            Box::new(FakeProvider::new(2, Some(1.0))),
            &cfg,
            RATE,
            CHANS,
        );
        let mut buf = vec![0.0f32; BUF];
        mixer.next_buffer(&mut buf);
        b.iter(|| {
            mixer.next_buffer(&mut buf);
            black_box(&buf);
        });
    });

    group.bench_function("priority/passthrough", |b| {
        let (tx, rx) = mpsc::channel();
        drop(tx);
        let mut mixer = PriorityMixer::new(
            Box::new(ConstSource::new(0.2, None)),
            rx,
            &cfg,
            spec,
            FPB,
        );
        let mut buf = vec![0.0f32; BUF];
        b.iter(|| {
            mixer.next_buffer(&mut buf);
            black_box(&buf);
        });
    });

    group.bench_function("priority/ducking", |b| {
        // A live override is ramped in/out every buffer: each call toggles
        // the fade state via a fresh SetLive command.
        let (tx, rx) = mpsc::channel();
        let mut mixer = PriorityMixer::new(
            Box::new(ConstSource::new(0.2, None)),
            rx,
            &cfg,
            spec,
            FPB,
        );
        let mut buf = vec![0.0f32; BUF];
        b.iter(|| {
            tx.send(crabsoup::engine::mixer::MixCommand::SetLive(
                Box::new(ConstSource::new(0.4, None)),
            ))
            .unwrap();
            mixer.next_buffer(&mut buf);
            black_box(&buf);
        });
    });
}

fn smart_crossfade(c: &mut Criterion) {
    let cfg = MixerConfig::default();
    let smart = SmartFade {
        fade_out: 2.0,
        fade_mid: 1.0,
        threshold_db: -30.0,
    };
    let mut group = c.benchmark_group("smart_crossfade");
    group.throughput(Throughput::Elements(BUF as u64));

    group.bench_function("passthrough+measuring", |b| {
        // Level-aware mode with no transition in sight: every buffer is
        // passthrough plus the rolling tail-level accumulation (per-sample
        // sum of squares + VecDeque window eviction) — the hot-path cost
        // D5 adds on top of the plain crossfade passthrough row.
        let mut mixer = CrossfadeMixer::new(Box::new(FakeProvider::new(2, None)), &cfg, RATE, CHANS)
            .with_smart_fade(smart);
        let mut buf = vec![0.0f32; BUF];
        b.iter(|| {
            mixer.next_buffer(&mut buf);
            black_box(&buf);
        });
    });

    group.bench_function("mixing (always crossfading)", |b| {
        // ConstSource reports remaining = 1.0 s < the smart `fade_out`
        // margin, so the mixer preloads and mixes every buffer; the tail
        // measurement pauses during the fade (worst case, same as the
        // plain crossfade/mixing row but through the smart branch).
        let mut mixer = CrossfadeMixer::new(
            Box::new(FakeProvider::new(2, Some(1.0))),
            &cfg,
            RATE,
            CHANS,
        )
        .with_smart_fade(smart);
        let mut buf = vec![0.0f32; BUF];
        mixer.next_buffer(&mut buf);
        b.iter(|| {
            mixer.next_buffer(&mut buf);
            black_box(&buf);
        });
    });
}

fn live_handoff(c: &mut Criterion) {
    // The harbor's decode thread pushes decoded PCM chunks and the audio
    // thread pulls 8192-sample buffers, with a 5 s drop-oldest cap. A
    // synthetic high-rate producer drives both implementations through the
    // identical workload: 8 chunk-pushes then one buffer-pull per iter
    // (net +1024 samples, so the queue reaches steady state under the cap).
    let chunk = vec![0.05f32; CHUNK];
    let mut group = c.benchmark_group("live_handoff");
    group.throughput(Throughput::Elements(BUF as u64));

    group.bench_function("mutex_vecdeque", |b| {
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let mut buf = vec![0.0f32; BUF];
        b.iter(|| {
            {
                let mut q = queue.lock();
                for _ in 0..8 {
                    q.extend(chunk.iter().copied());
                }
                let over = q.len().saturating_sub(CAP);
                if over > 0 {
                    q.drain(..over);
                }
            }
            let mut q = queue.lock();
            let n = BUF.min(q.len());
            for slot in buf[..n].iter_mut() {
                *slot = q.pop_front().unwrap_or(0.0);
            }
            black_box(n);
        });
    });

    group.bench_function("spsc_ring", |b| {
        let (mut prod, mut cons) = ringbuf::HeapRb::<f32>::new(2 * CAP).split();
        let mut buf = vec![0.0f32; BUF];
        b.iter(|| {
            // Producer: bulk push (drop-newest only past the 2x headroom).
            for _ in 0..8 {
                prod.push_slice(&chunk);
            }
            // Consumer: enforce the drop-oldest window, then bulk pop.
            let over = cons.occupied_len().saturating_sub(CAP);
            cons.skip(over);
            let n = cons.pop_slice(&mut buf);
            black_box(n);
        });
    });
}

fn effects(c: &mut Criterion) {
    let mut group = c.benchmark_group("effects");
    group.throughput(Throughput::Elements(BUF as u64));

    group.bench_function("compressor+agc+amplify", |b| {
        let child: Box<dyn AudioSource> = Box::new(SineSource::new(440.0, None, 0.5, RATE, CHANS));
        let compressed: Box<dyn AudioSource> = Box::new(EffectSource::new(
            child,
            Compressor::new(-12.0, 2.0, 0.05, 0.25, 0.0, RATE),
            CHANS,
        ));
        let agced: Box<dyn AudioSource> = Box::new(EffectSource::new(
            compressed,
            Agc::new(-14.0, 0.5, 0.1, 12.0, 12.0, RATE),
            CHANS,
        ));
        let mut chain = EffectSource::new(agced, Amplify::new(0.9), CHANS);
        let mut buf = vec![0.0f32; BUF];
        b.iter(|| {
            chain.next_buffer(&mut buf);
            black_box(&buf);
        });
    });
}

fn resampler(c: &mut Criterion) {
    let mut group = c.benchmark_group("resampler");
    group.throughput(Throughput::Elements(BUF as u64));

    for (name, to_rate) in [("44k_to_48k", 48_000), ("48k_to_44k1", 44_100)] {
        group.bench_with_input(BenchmarkId::new("sinc16", name), &to_rate, |b, &to_rate| {
            let mut rs = SincResampler::new(0, RATE, to_rate, CHANS);
            let input = vec![0.0f32; BUF];
            b.iter(|| {
                let out = rs.resample(&input);
                black_box(out);
            });
        });
    }
}

fn encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode");
    // Audio throughput consumed per encode call (8192 interleaved samples).
    group.throughput(Throughput::Elements(BUF as u64));

    let pcm = vec![0.0f32; BUF];
    let encoders: [(&str, Box<dyn Encoder>); 3] = [
        ("mp3", Box::new(Mp3Encoder::new(RATE, CHANS as u16, 192_000).unwrap())),
        (
            "opus",
            Box::new(OpusEncoder::new(48_000, CHANS as u16, 128_000, "bench").unwrap()),
        ),
        ("aac", Box::new(AacEncoder::new(RATE, CHANS as u16, 128_000).unwrap())),
    ];
    for (name, mut enc) in encoders {
        group.bench_function(name, |b| {
            b.iter(|| {
                let out = enc.encode(&pcm);
                black_box(&out);
            });
        });
    }
}

criterion::criterion_group!(benches, mixers, smart_crossfade, live_handoff, effects, resampler, encode);
criterion::criterion_main!(benches);
