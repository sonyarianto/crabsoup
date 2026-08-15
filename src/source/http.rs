//! Continuous relay/pull-stream source (Liquidsoap `input.http`): a network
//! thread GETs the URL and decodes the live body into an SPSC ring — the
//! same harbor-style bridge — so the `AudioSource` half never touches the
//! network or blocks on it. While connected the relay plays; while
//! disconnected (or between reconnects) `is_exhausted()` is `true`, so a
//! `fallback({relay, local})` takes over in the gap without script-side
//! handling: the "relay during syndicated hours, else local automation"
//! shape.
//!
//! The decode loop is a fresh per-connection attempt: `GET` (redirects
//! followed), sniff the first Ogg page to pick the native Opus path
//! (symphonia 0.5 has no Opus codec) or a symphonia probe hinted by the
//! `Content-Type`, then decode packets into the ring until the connection
//! ends (EOF or read error) — which triggers reconnect-with-backoff,
//! mirroring `IcecastOutput`'s reconnect philosophy.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use log::{debug, warn};
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::*};
use symphonia::core::audio::SignalSpec;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::source::opus::{OpusSource, PrependReader, sniff_stream};
use crate::source::{AudioSource, PcmConverter};

/// Jitter-buffer depth in seconds: how far the decode thread may run ahead
/// of the consumer before backpressure applies (and the drop-oldest cap on
/// pull, mirroring the harbor).
const JITTER_SECONDS: usize = 5;

/// A continuous relay: the network/decode thread fills the ring, the pull
/// side drains it. `connected` tracks whether a stream is currently being
/// decoded; `shutdown` ends the thread (set on drop).
pub struct HttpSource {
    consumer: HeapCons<f32>,
    connected: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    /// Drop-oldest cap in samples (live-latency bound).
    max_samples: usize,
    label: String,
}

impl HttpSource {
    /// Validate the URL, start the relay thread, and return the pull side.
    /// The first connection attempt happens on the thread, so a dead
    /// upstream surfaces as a disconnected (exhausted) source rather than a
    /// blocking call at script evaluation.
    pub fn spawn(
        url: &str,
        target: SignalSpec,
        frames_per_buffer: usize,
        timeout: Duration,
        backoff: Duration,
    ) -> crate::Result<Self> {
        crate::request::validate_relay_url(url)?;
        let chans = target.channels.count();
        let max_samples = JITTER_SECONDS * target.rate as usize * chans;
        let (prod, cons) = HeapRb::<f32>::new(2 * max_samples).split();
        let connected = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(AtomicBool::new(false));

        let label = crate::request::RequestUri::new(url).display();
        let t_url = url.to_string();
        let t_connected = connected.clone();
        let t_shutdown = shutdown.clone();
        let t_label = label.clone();
        std::thread::spawn(move || {
            let mut sink = Sink {
                producer: prod,
                shutdown: t_shutdown.clone(),
            };
            loop {
                if t_shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match run_connection(
                    &t_url,
                    target,
                    frames_per_buffer,
                    timeout,
                    &t_label,
                    &t_connected,
                    &mut sink,
                ) {
                    Ok(()) => debug!(
                        "input.http: {t_url} stream ended; reconnecting in {} ms",
                        backoff.as_millis()
                    ),
                    Err(e) => warn!(
                        "input.http: {t_url}: {e}; reconnecting in {} ms",
                        backoff.as_millis()
                    ),
                }
                t_connected.store(false, Ordering::SeqCst);
                interruptible_sleep(backoff, &t_shutdown);
            }
        });

        Ok(Self {
            consumer: cons,
            connected,
            shutdown,
            max_samples,
            label,
        })
    }
}

impl Drop for HttpSource {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
    }
}

impl AudioSource for HttpSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        // Drop-oldest cap: a fast connection may have raced ahead; keep only
        // the most recent window so relay latency stays bounded.
        let over = self
            .consumer
            .occupied_len()
            .saturating_sub(self.max_samples);
        if over > 0 {
            self.consumer.skip(over);
        }
        self.consumer.pop_slice(buffer)
    }

    fn is_exhausted(&self) -> bool {
        !self.connected.load(Ordering::SeqCst) && self.consumer.is_empty()
    }

    fn label(&self) -> Option<String> {
        Some(self.label.clone())
    }
}

/// Producer half of the relay handoff: the network/decode thread pushes
/// decoded PCM here; `HttpSource` (the audio thread) pulls it lock-free.
/// Backpressure on a full ring throttles the decode to the consumer's rate
/// (never silently drops the newest audio); the shutdown flag breaks the
/// wait so a dropped source never strands the thread.
struct Sink {
    producer: HeapProd<f32>,
    shutdown: Arc<AtomicBool>,
}

impl Sink {
    fn push_samples(&mut self, samples: &[f32]) {
        let mut rest = samples;
        while !rest.is_empty() {
            if self.shutdown.load(Ordering::SeqCst) {
                return;
            }
            let n = self.producer.push_slice(rest);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(2));
            }
            rest = &rest[n..];
        }
    }
}

/// One full connection: GET, probe/decode, push PCM until the stream ends.
/// Any exit (clean end, read error, probe failure) makes the caller
/// reconnect. Sets `connected` once a stream is actually decodable.
fn run_connection(
    url: &str,
    target: SignalSpec,
    frames_per_buffer: usize,
    timeout: Duration,
    label: &str,
    connected: &Arc<AtomicBool>,
    sink: &mut Sink,
) -> crate::Result<()> {
    let response = crate::request::http_get_stream(url, timeout, None)?;

    // Peek the first Ogg page: an OpusHead stream takes the native
    // `OpusSource` path (symphonia 0.5 has no Opus codec), anything else is
    // probed by symphonia. Either way the sniffed bytes are fed back so the
    // stream is never consumed past its head.
    let mut hint = Hint::new();
    if let Some(ct) = &response.content_type {
        hint.mime_type(ct);
    }
    let (is_opus, prefix, rest) = sniff_stream(response)?;
    let inner: Box<dyn Read + Send + Sync> = Box::new(PrependReader::new(prefix, rest));

    connected.store(true, Ordering::SeqCst);
    if is_opus {
        let mut src = OpusSource::open(inner, target, frames_per_buffer, label.to_string())?;
        loop {
            let mut scratch = vec![0f32; frames_per_buffer * target.channels.count()];
            let n = src.next_buffer(&mut scratch);
            if n == 0 {
                break Ok(()); // stream ended (cleanly or not): reconnect
            }
            sink.push_samples(&scratch[..n]);
        }
    } else {
        let mss = MediaSourceStream::new(
            Box::new(ReadOnlySource::new(inner)),
            MediaSourceStreamOptions::default(),
        );
        let mut probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;
        let track = probed
            .format
            .default_track()
            .cloned()
            .ok_or("no default audio track")?;
        let track_id = track.id;
        let mut decoder = symphonia::default::get_codecs()
            .make(&track.codec_params, &DecoderOptions::default())?;
        decode_symphonia(
            probed.format.as_mut(),
            decoder.as_mut(),
            track_id,
            target,
            sink,
        )
    }
}

/// Decode packets from a symphonia `FormatReader` into the ring until the
/// connection ends (clean EOF or a read error) — either way the caller
/// reconnects. Per-packet decode errors are skipped like `FileSource` does.
fn decode_symphonia(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
    target: SignalSpec,
    sink: &mut Sink,
) -> crate::Result<()> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::errors::Error as SErr;

    let mut converter = PcmConverter::new(target);
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            // IoError = socket closed / timed out; anything else = end of
            // stream. Both mean the connection is over.
            Err(SErr::IoError(_)) | Err(_) => return Ok(()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(e) => {
                warn!("input.http: skipping packet: {e}");
                continue;
            }
        };
        let spec = *decoded.spec();
        let frames = decoded.frames();
        if frames == 0 {
            continue;
        }
        let mut sample_buf = SampleBuffer::<f32>::new(frames as u64, spec);
        sample_buf.copy_interleaved_ref(decoded);
        let converted = converter.convert(sample_buf.samples(), &spec);
        sink.push_samples(&converted);
    }
}

/// Sleep for `dur`, waking early on shutdown so dropping a source stops its
/// relay thread promptly (pipe's interruptible-backoff pattern).
fn interruptible_sleep(dur: Duration, shutdown: &AtomicBool) {
    let step = Duration::from_millis(10);
    let mut elapsed = Duration::ZERO;
    while elapsed < dur {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(step.min(dur - elapsed));
        elapsed += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::time::Instant;

    /// A minimal RIFF/WAVE sine (PCM 16-bit stereo) — the same shape the
    /// `file.rs` tests generate.
    fn sine_wav(seconds: f64, rate: u32, freq: f64) -> Vec<u8> {
        let n = (seconds * rate as f64) as usize;
        let mut data = Vec::with_capacity(n * 4);
        for i in 0..n {
            let t = i as f64 / rate as f64;
            let s = (2.0 * std::f64::consts::PI * freq * t).sin() as f32 * 0.5;
            let sample = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            for _ in 0..2 {
                data.extend_from_slice(&sample.to_le_bytes());
            }
        }
        let mut out = Vec::new();
        let data_len = data.len() as u32;
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 4).to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    /// Serve `wav` once per connection, writing the body in two halves to
    /// exercise the streaming path (the reader must not wait for the whole
    /// body before decoding). Returns the URL.
    fn serve_wav_repeating(wav: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            for _ in 0..64 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\n\r\n",
                    wav.len()
                );
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                let mid = wav.len() / 2;
                if stream.write_all(&wav[..mid]).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
                let _ = stream.write_all(&wav[mid..]);
                let _ = stream.flush();
            }
        });
        format!("http://{addr}/relay.wav")
    }

    fn spec() -> SignalSpec {
        SignalSpec::new(
            44_100,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        )
    }

    /// Pull with brief sleeps (next_buffer is non-blocking) until `done`
    /// returns true or the deadline passes. Returns everything pulled.
    fn pull_until(
        src: &mut HttpSource,
        mut done: impl FnMut(&HttpSource, &[f32]) -> bool,
        deadline_secs: u64,
    ) -> Vec<f32> {
        let mut buf = vec![0f32; 2048 * 2];
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(deadline_secs);
        while Instant::now() < deadline {
            let n = src.next_buffer(&mut buf);
            got.extend_from_slice(&buf[..n]);
            if done(src, &got) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        got
    }

    #[test]
    fn relays_a_stream_and_reconnects_after_the_connection_drops() {
        let wav = sine_wav(0.25, 44_100, 440.0);
        let url = serve_wav_repeating(wav.clone());
        let mut src = HttpSource::spawn(
            &url,
            spec(),
            2048,
            Duration::from_secs(3),
            Duration::from_millis(10),
        )
        .expect("spawn");
        assert_eq!(src.label().as_deref(), Some("relay.wav"));

        // Track exhaustion separately: it is only observable in the gap
        // between bursts, while the pull-loop condition runs continuously.
        let samples_per_burst = 0.25 * 44_100.0 * 2.0;
        let mut saw_exhausted = false;
        let got = pull_until(
            &mut src,
            |src, got| {
                saw_exhausted |= src.is_exhausted();
                saw_exhausted && got.len() as f64 > samples_per_burst * 1.25
            },
            15,
        );
        assert!(
            saw_exhausted,
            "the relay must report exhausted in the reconnect gap"
        );
        assert!(
            got.len() as f64 > samples_per_burst,
            "must play more than one burst (reconnect), got {} samples",
            got.len()
        );
        // Audio, not silence: energy per sample is nonzero.
        let energy: f64 = got.iter().map(|&s| (s as f64).powi(2)).sum();
        assert!(energy / got.len().max(1) as f64 > 0.001, "energy={energy}");
    }

    #[test]
    fn exhausts_while_disconnected_and_recovers_when_the_server_returns() {
        // A listener that accepts but never answers: the GET blocks until
        // the (short) timeout, then the source reports disconnected.
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let url = format!("http://{addr}/x");
        let mut src = HttpSource::spawn(
            &url,
            spec(),
            2048,
            Duration::from_millis(200),
            Duration::from_millis(20),
        )
        .expect("spawn");

        // Until the server answers, the relay is exhausted (falls back).
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !src.is_exhausted() {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(src.is_exhausted(), "disconnected relay must be exhausted");

        // Now serve a WAV on that port: the source connects and plays.
        let wav = sine_wav(0.2, 44_100, 440.0);
        let wav_len = wav.len();
        std::thread::spawn(move || {
            let wav = wav;
            for _ in 0..16 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nContent-Length: {wav_len}\r\n\r\n"
                );
                if stream.write_all(head.as_bytes()).is_err() {
                    return;
                }
                if stream.write_all(&wav).is_err() {
                    return;
                }
                let _ = stream.flush();
            }
        });

        let samples_per_burst = 0.2 * 44_100.0 * 2.0;
        let got = pull_until(
            &mut src,
            |src, got| !src.is_exhausted() && got.len() as f64 >= samples_per_burst,
            15,
        );
        assert!(
            got.len() as f64 >= samples_per_burst,
            "must recover when the server appears, got {} samples",
            got.len()
        );
    }

    #[test]
    fn rejects_malformed_urls_at_spawn() {
        let out = HttpSource::spawn(
            "ftp://x.example/feed",
            spec(),
            2048,
            Duration::from_secs(3),
            Duration::from_millis(10),
        );
        let err = match out {
            Err(e) => e.to_string(),
            Ok(_) => panic!("must reject ftp://"),
        };
        assert!(err.contains("http"), "{err}");
        assert!(
            HttpSource::spawn(
                "http://",
                spec(),
                2048,
                Duration::from_secs(3),
                Duration::from_millis(10),
            )
            .is_err()
        );
    }
}
