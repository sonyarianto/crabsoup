//! External-process pipeline (Liquidsoap `pipe`).
//!
//! Runs an outboard broadcast processor (e.g. Thimeo Stereo Tool) as a
//! pipeline stage: a writer thread pulls the child source and feeds the
//! subprocess's stdin as raw little-endian PCM; a reader thread decodes
//! stdout back into a bounded queue the audio side pulls from. Outboard
//! processors are closed-source, so a subprocess is the isolation
//! boundary — a crash kills the processor, not crabsoup — and no FFI or
//! `unsafe` is involved.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::Result;
use crate::source::AudioSource;

/// How many full buffers the reader may queue ahead of the consumer.
/// Bounded so a stalled consumer backpressures all the way to the writer.
const QUEUE_CHUNKS: usize = 8;
/// How long `next_buffer` waits for the process's output before reporting
/// silence. A stalled or dead process must not stall the engine; the poll
/// only fires while the queue is empty (the reader is normally ahead).
const POLL_MS: u64 = 50;
/// Consecutive silence polls (x [`POLL_MS`]) tolerated while draining
/// before the pipe ends anyway (a process that never closes stdout).
const DRAIN_STALL_LIMIT: u32 = 20;

const MODE_PROCESSING: u8 = 0;
const MODE_BYPASS: u8 = 1;
const MODE_DRAINING: u8 = 2;
const MODE_ENDED: u8 = 3;

/// Raw PCM layout exchanged with the subprocess.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcmFormat {
    /// 16-bit signed little-endian (the default most processors expect).
    S16Le,
    /// 24-bit signed little-endian.
    S24Le,
}

impl PcmFormat {
    fn bytes_per_sample(&self) -> usize {
        match self {
            PcmFormat::S16Le => 2,
            PcmFormat::S24Le => 3,
        }
    }

    /// Encode interleaved f32 samples as raw little-endian PCM. The i16
    /// clamp is the LAME path's (`output/encoder.rs`).
    fn encode(&self, samples: &[f32], out: &mut Vec<u8>) {
        match self {
            PcmFormat::S16Le => {
                for &s in samples {
                    out.extend_from_slice(&crate::output::encoder::clamp_i16(s).to_le_bytes());
                }
            }
            PcmFormat::S24Le => {
                for &s in samples {
                    let v = (s.clamp(-1.0, 1.0) * 8_388_607.0) as i32;
                    let b = v.to_le_bytes();
                    out.extend_from_slice(&b[..3]);
                }
            }
        }
    }

    /// Decode a whole number of little-endian frames into interleaved f32.
    fn decode(&self, bytes: &[u8], out: &mut Vec<f32>) {
        match self {
            PcmFormat::S16Le => {
                for c in bytes.chunks_exact(2) {
                    out.push(i16::from_le_bytes([c[0], c[1]]) as f32 / 32767.0);
                }
            }
            PcmFormat::S24Le => {
                for c in bytes.chunks_exact(3) {
                    let sign = if c[2] & 0x80 != 0 { 0xFF } else { 0 };
                    let v = i32::from_le_bytes([c[0], c[1], c[2], sign]);
                    out.push(v as f32 / 8_388_607.0);
                }
            }
        }
    }
}

/// `pipe` operator options.
#[derive(Clone, Copy, Debug)]
pub struct PipeConfig {
    pub format: PcmFormat,
    /// Fixed delay between restart attempts after the process dies —
    /// Icecast `reconnect`-style policy: retry forever, never block the
    /// pull loop on a respawn.
    pub restart_backoff_ms: u64,
}

impl Default for PipeConfig {
    fn default() -> Self {
        Self {
            format: PcmFormat::S16Le,
            restart_backoff_ms: 500,
        }
    }
}

/// State shared between the audio side, the writer (supervisor) thread and
/// the per-process reader thread.
struct Shared {
    /// [`MODE_*`] state machine.
    mode: AtomicU8,
    /// The child source ended and stdin was closed on purpose: a reader EOF
    /// now is a clean drain, not a death.
    ending: AtomicBool,
    /// The subprocess died unexpectedly (reader saw EOF or a write failed).
    died: AtomicBool,
    /// The live `Child`, kept so `Drop` can kill a torn process.
    child_proc: Mutex<Option<Child>>,
    /// The subprocess's stdin (writer thread only).
    stdin: Mutex<Option<ChildStdin>>,
    /// The reader's output queue (audio thread only).
    rx: Mutex<Option<mpsc::Receiver<Vec<f32>>>>,
}

/// Spawn `sh -c <command>` with piped stdin/stdout and start a reader
/// thread on its stdout, swapping in a fresh output queue. Called on first
/// start and on every restart.
fn spawn_reader(
    shared: &Arc<Shared>,
    command: &str,
    format: PcmFormat,
    channels: usize,
    chunk_samples: usize,
) -> Result<()> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn failed: {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin pipe")?;
    let stdout = child.stdout.take().ok_or("no stdout pipe")?;
    let (tx, rx) = mpsc::sync_channel(QUEUE_CHUNKS);
    *shared.stdin.lock().unwrap() = Some(stdin);
    *shared.rx.lock().unwrap() = Some(rx);
    *shared.child_proc.lock().unwrap() = Some(child);
    let reader_shared = shared.clone();
    std::thread::Builder::new()
        .name("pipe-reader".into())
        .spawn(move || reader_thread(stdout, tx, reader_shared, format, channels, chunk_samples))
        .map_err(|e| format!("reader thread failed: {e}"))?;
    Ok(())
}

/// Drain the process's stdout, decode complete frames, and push fixed-size
/// chunks into the queue. An unexpected EOF marks the process dead so the
/// supervisor restarts it; a clean EOF (stdin was closed on purpose) just
/// exits and lets the queue drain.
fn reader_thread(
    mut stdout: ChildStdout,
    tx: SyncSender<Vec<f32>>,
    shared: Arc<Shared>,
    format: PcmFormat,
    channels: usize,
    chunk_samples: usize,
) {
    let frame_bytes = channels * format.bytes_per_sample();
    let mut bytes: Vec<u8> = Vec::with_capacity(65_536);
    let mut pcm: Vec<f32> = Vec::new();
    let mut chunk: Vec<f32> = Vec::with_capacity(chunk_samples);
    let mut buf = [0u8; 16_384];
    loop {
        let n = match stdout.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        bytes.extend_from_slice(&buf[..n]);
        let complete = bytes.len() / frame_bytes * frame_bytes;
        format.decode(&bytes[..complete], &mut pcm);
        bytes.drain(..complete);
        for s in pcm.drain(..) {
            chunk.push(s);
            if chunk.len() == chunk_samples && !send_chunk(&tx, &shared, &mut chunk) {
                return;
            }
        }
    }
    if !shared.ending.load(Ordering::SeqCst) {
        log::warn!("pipe: subprocess closed stdout unexpectedly; bypassing");
        shared.died.store(true, Ordering::SeqCst);
        shared.mode.store(MODE_BYPASS, Ordering::SeqCst);
    }
}

/// Push one decoded chunk, waiting with backpressure until the consumer
/// drains — but abort if the pipe ended (the queue is never drained again).
fn send_chunk(tx: &SyncSender<Vec<f32>>, shared: &Shared, chunk: &mut Vec<f32>) -> bool {
    loop {
        if shared.mode.load(Ordering::SeqCst) == MODE_ENDED {
            return false;
        }
        match tx.try_send(std::mem::take(chunk)) {
            Ok(()) => return true,
            Err(mpsc::TrySendError::Full(v)) => {
                // Queue full: put the data back and wait for the consumer.
                *chunk = v;
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
}

/// Sleep in small slices so an ended pipe (drop) interrupts the backoff.
fn sleep_interruptible(shared: &Shared, ms: u64) -> bool {
    let mut left = ms;
    while left > 0 {
        if shared.mode.load(Ordering::SeqCst) == MODE_ENDED {
            return false;
        }
        let step = left.min(50);
        std::thread::sleep(Duration::from_millis(step));
        left -= step;
    }
    true
}

/// The supervisor: pulls the child, feeds the process, and owns the
/// restart-with-backoff policy. While the process is down the audio side
/// bypasses to the raw child; this thread only handles the resurrection.
fn supervisor_thread(
    command: &str,
    child: Arc<Mutex<Box<dyn AudioSource>>>,
    shared: Arc<Shared>,
    config: PipeConfig,
    channels: usize,
    frames_per_buffer: usize,
) {
    let chunk_samples = frames_per_buffer * channels;
    let mut pcm = vec![0f32; chunk_samples];
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        if shared.mode.load(Ordering::SeqCst) == MODE_ENDED {
            return;
        }
        if shared.died.load(Ordering::SeqCst) {
            shared.mode.store(MODE_BYPASS, Ordering::SeqCst);
            // Reap the dead process (bounded — it may have closed stdout
            // without exiting) so respawns never accumulate zombies.
            if let Some(mut old) = shared.child_proc.lock().unwrap().take() {
                for _ in 0..100 {
                    match old.try_wait() {
                        Ok(Some(_)) => break,
                        Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                        Err(_) => break,
                    }
                }
            }
            if !sleep_interruptible(&shared, config.restart_backoff_ms) {
                return;
            }
            match spawn_reader(&shared, command, config.format, channels, chunk_samples) {
                Ok(()) => {
                    log::info!("pipe: restarted subprocess");
                    shared.died.store(false, Ordering::SeqCst);
                    shared.mode.store(MODE_PROCESSING, Ordering::SeqCst);
                }
                Err(e) => log::warn!("pipe: restart failed: {e}"),
            }
            continue;
        }
        let n = {
            let mut child = child.lock().unwrap();
            let n = child.next_buffer(&mut pcm);
            if n == 0 && child.is_exhausted() {
                None
            } else {
                Some(n)
            }
        };
        let Some(n) = n else {
            // Child done: close stdin so the process can flush and exit.
            // The reader's EOF is now a clean drain and the audio side
            // plays out the queued audio before ending.
            shared.ending.store(true, Ordering::SeqCst);
            shared.mode.store(MODE_DRAINING, Ordering::SeqCst);
            *shared.stdin.lock().unwrap() = None;
            return;
        };
        if n == 0 {
            // Temporarily silent child: keep the pipe alive, do not spin.
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        bytes.clear();
        config.format.encode(&pcm[..n], &mut bytes);
        let written = {
            let mut stdin = shared.stdin.lock().unwrap();
            stdin.as_mut().map(|s| s.write_all(&bytes))
        };
        if !matches!(written, Some(Ok(()))) {
            log::warn!("pipe: writing to the subprocess failed; bypassing");
            shared.died.store(true, Ordering::SeqCst);
        }
    }
}

/// A source that runs its child through an external raw-PCM processor.
pub struct PipeSource {
    /// The wrapped child, shared with the writer thread (and pulled
    /// directly in bypass mode — which is why `pipe` never consumes it).
    child: Arc<Mutex<Box<dyn AudioSource>>>,
    shared: Arc<Shared>,
    /// A chunk larger than the consumer's buffer, held for the next pull.
    leftover: Vec<f32>,
    /// Consecutive silence polls while draining (see [`DRAIN_STALL_LIMIT`]).
    drain_stalls: u32,
}

impl PipeSource {
    /// Spawn the process and the bridge threads. A broken command is not an
    /// error: the pipe starts in bypass and the supervisor keeps retrying.
    pub fn spawn(
        command: &str,
        child: Arc<Mutex<Box<dyn AudioSource>>>,
        channels: usize,
        frames_per_buffer: usize,
        config: PipeConfig,
    ) -> Result<Self> {
        if command.trim().is_empty() {
            return Err("pipe: `process` must be a non-empty command".into());
        }
        let shared = Arc::new(Shared {
            mode: AtomicU8::new(MODE_BYPASS),
            ending: AtomicBool::new(false),
            died: AtomicBool::new(true),
            child_proc: Mutex::new(None),
            stdin: Mutex::new(None),
            rx: Mutex::new(None),
        });
        // The first spawn is synchronous so a broken command fails fast
        // into bypass instead of sitting silent until the first retry.
        match spawn_reader(
            &shared,
            command,
            config.format,
            channels,
            frames_per_buffer * channels,
        ) {
            Ok(()) => {
                shared.died.store(false, Ordering::SeqCst);
                shared.mode.store(MODE_PROCESSING, Ordering::SeqCst);
            }
            Err(e) => log::warn!("pipe: initial spawn failed ({e}); running bypassed"),
        }
        let writer_shared = shared.clone();
        let writer_child = child.clone();
        let command = command.to_string();
        std::thread::Builder::new()
            .name("pipe-writer".into())
            .spawn(move || {
                supervisor_thread(
                    &command,
                    writer_child,
                    writer_shared,
                    config,
                    channels,
                    frames_per_buffer,
                );
            })
            .map_err(|e| format!("pipe writer thread failed: {e}"))?;
        Ok(Self {
            child,
            shared,
            leftover: Vec::new(),
            drain_stalls: 0,
        })
    }
}

impl AudioSource for PipeSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        let mut filled = 0;
        if !self.leftover.is_empty() {
            let n = self.leftover.len().min(buffer.len());
            buffer[..n].copy_from_slice(&self.leftover[..n]);
            self.leftover.drain(..n);
            filled = n;
            if filled == buffer.len() {
                return filled;
            }
        }
        match self.shared.mode.load(Ordering::SeqCst) {
            MODE_ENDED => filled,
            MODE_BYPASS => {
                let mut child = self.child.lock().unwrap();
                let n = child.next_buffer(&mut buffer[filled..]);
                if n == 0 && child.is_exhausted() {
                    self.shared.mode.store(MODE_ENDED, Ordering::SeqCst);
                }
                filled + n
            }
            _ => {
                // PROCESSING or DRAINING: pull the queue.
                let rx = self.shared.rx.lock().unwrap();
                let got = match rx.as_ref() {
                    Some(rx) => rx.recv_timeout(Duration::from_millis(POLL_MS)),
                    None => Err(RecvTimeoutError::Timeout),
                };
                match got {
                    Ok(chunk) => {
                        self.drain_stalls = 0;
                        let room = buffer.len() - filled;
                        if chunk.len() > room {
                            self.leftover.extend_from_slice(&chunk[room..]);
                        }
                        let take = chunk.len().min(room);
                        buffer[filled..filled + take].copy_from_slice(&chunk[..take]);
                        filled += take;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if self.shared.mode.load(Ordering::SeqCst) == MODE_DRAINING {
                            self.drain_stalls += 1;
                            if self.drain_stalls >= DRAIN_STALL_LIMIT {
                                self.shared.mode.store(MODE_ENDED, Ordering::SeqCst);
                            }
                        }
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        if self.shared.mode.load(Ordering::SeqCst) == MODE_DRAINING {
                            self.shared.mode.store(MODE_ENDED, Ordering::SeqCst);
                        }
                    }
                }
                filled
            }
        }
    }

    fn is_exhausted(&self) -> bool {
        match self.shared.mode.load(Ordering::SeqCst) {
            MODE_ENDED => true,
            MODE_BYPASS => self.child.lock().unwrap().is_exhausted(),
            _ => false,
        }
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.child.lock().unwrap().remaining_seconds()
    }

    fn label(&self) -> Option<String> {
        self.child.lock().unwrap().label()
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.child.lock().unwrap().replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<crate::source::CrossfadeOverrides> {
        self.child.lock().unwrap().crossfade_overrides()
    }

    fn skip(&mut self) {
        self.child.lock().unwrap().skip();
    }
}

impl Drop for PipeSource {
    fn drop(&mut self) {
        // Stop everything and kill the subprocess so a torn engine never
        // leaves a processor orphaned (or a bridge thread parked forever).
        self.shared.ending.store(true, Ordering::SeqCst);
        self.shared.mode.store(MODE_ENDED, Ordering::SeqCst);
        if let Some(mut child) = self.shared.child_proc.lock().unwrap().take() {
            let _ = child.kill();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::SineSource;

    const FPBS: usize = 128;

    fn sine_child() -> Arc<Mutex<Box<dyn AudioSource>>> {
        Arc::new(Mutex::new(Box::new(SineSource::new(
            440.0, None, 0.5, 44100, 2,
        ))))
    }

    fn reference_sine() -> SineSource {
        SineSource::new(440.0, None, 0.5, 44100, 2)
    }

    /// Pull until the pipe produces audio (spawning + first chunk can take a
    /// few polls). The successful pull consumes exactly one chunk (the
    /// consumer buffer is chunk-sized), so callers align an independent
    /// reference by pulling it once before comparing.
    fn pull_nonzero(pipe: &mut PipeSource, buf: &mut [f32], tries: usize) -> usize {
        for _ in 0..tries {
            let n = pipe.next_buffer(buf);
            if n > 0 {
                return n;
            }
        }
        0
    }

    /// Compare the pipe's chunks against an independent reference sine.
    /// A zero pull (50 ms poll while the pipeline lags) is skipped without
    /// advancing the reference — chunks are never dropped by a timeout, so
    /// the concatenated non-zero pulls are always the child's contiguous
    /// samples and the reference stays in lockstep with them.
    fn assert_matches_reference(
        pipe: &mut PipeSource,
        buf: &mut [f32],
        reference: &mut SineSource,
        refbuf: &mut [f32],
        pulls: usize,
        tol: f32,
        label: &str,
    ) {
        let mut total = 0;
        for _ in 0..pulls {
            let n = pipe.next_buffer(buf);
            if n == 0 {
                continue;
            }
            let m = reference.next_buffer(refbuf);
            assert_eq!(n, m, "sample count mismatch");
            for (a, b) in buf[..n].iter().zip(&refbuf[..m]) {
                assert!((a - b).abs() <= tol, "{label}: {a} vs {b}");
            }
            total += n;
        }
        assert!(
            total >= pulls.saturating_sub(8) * buf.len(),
            "{label}: too little audio"
        );
    }

    #[test]
    fn s16le_cat_passthrough_preserves_audio() {
        let mut pipe =
            PipeSource::spawn("cat", sine_child(), 2, FPBS, PipeConfig::default()).unwrap();
        let mut buf = vec![0f32; FPBS * 2];
        assert!(
            pull_nonzero(&mut pipe, &mut buf, 200) > 0,
            "no audio from cat"
        );
        let mut reference = reference_sine();
        let mut refbuf = vec![0f32; FPBS * 2];
        reference.next_buffer(&mut refbuf); // the pipe already took chunk 1
        assert_matches_reference(
            &mut pipe,
            &mut buf,
            &mut reference,
            &mut refbuf,
            50,
            2.0 / 32767.0,
            "s16 round-trip",
        );
        assert!(!pipe.is_exhausted());
    }

    #[test]
    fn s24le_cat_passthrough_preserves_audio() {
        let cfg = PipeConfig {
            format: PcmFormat::S24Le,
            ..Default::default()
        };
        let mut pipe = PipeSource::spawn("cat", sine_child(), 2, FPBS, cfg).unwrap();
        let mut buf = vec![0f32; FPBS * 2];
        assert!(
            pull_nonzero(&mut pipe, &mut buf, 200) > 0,
            "no audio from cat"
        );
        let mut reference = reference_sine();
        let mut refbuf = vec![0f32; FPBS * 2];
        reference.next_buffer(&mut refbuf); // the pipe already took chunk 1
        assert_matches_reference(
            &mut pipe,
            &mut buf,
            &mut reference,
            &mut refbuf,
            50,
            2.0 / 8_388_607.0,
            "s24 round-trip",
        );
    }

    #[test]
    fn dead_subprocess_bypasses_and_keeps_audio_flowing() {
        // `head -c 512` passes exactly one 128-frame stereo s16 chunk then
        // exits; a 60 s backoff means no restart interferes during the test.
        let cfg = PipeConfig {
            restart_backoff_ms: 60_000,
            ..Default::default()
        };
        let mut pipe = PipeSource::spawn("head -c 512", sine_child(), 2, FPBS, cfg).unwrap();
        let mut buf = vec![0f32; FPBS * 2];
        assert!(
            pull_nonzero(&mut pipe, &mut buf, 200) > 0,
            "no processed audio"
        );
        // After the queue drains the pipe must fall back to the raw child
        // and keep producing the sine — never hanging, never exhausting.
        let mut total = 0;
        let mut non_silent = 0;
        for _ in 0..400 {
            let n = pipe.next_buffer(&mut buf);
            total += n;
            non_silent += buf[..n].iter().filter(|&&s| s.abs() > 0.01).count();
            assert!(
                !pipe.is_exhausted(),
                "pipe must not exhaust on a dead process"
            );
        }
        assert!(non_silent > 0, "bypass produced only silence");
        assert!(total > 0);
        assert_eq!(
            pipe.shared.mode.load(Ordering::SeqCst),
            MODE_BYPASS,
            "pipe did not reach bypass"
        );
    }

    #[test]
    fn a_dying_process_restarts_and_keeps_flowing() {
        // `head -c 512` dies after every two 128-frame chunks; a 10 ms
        // backoff keeps the supervisor resurrecting it. The point is that
        // death + restart never hangs or silences the stream.
        let cfg = PipeConfig {
            restart_backoff_ms: 10,
            ..Default::default()
        };
        let mut pipe = PipeSource::spawn("head -c 512", sine_child(), 2, FPBS, cfg).unwrap();
        let mut buf = vec![0f32; FPBS * 2];
        let mut total = 0;
        let mut non_silent = 0;
        for _ in 0..500 {
            let n = pipe.next_buffer(&mut buf);
            total += n;
            non_silent += buf[..n].iter().filter(|&&s| s.abs() > 0.01).count();
            assert!(
                !pipe.is_exhausted(),
                "pipe must not exhaust on a dying process"
            );
        }
        assert!(total > 0 && non_silent > 0, "audio stopped during restarts");
    }

    #[test]
    fn finite_child_drains_cleanly_and_exhausts() {
        // A 0.2 s sine through `cat`: the child exhausts, the pipe drains
        // the processed tail and only then reports exhausted (no hang).
        let child = Arc::new(Mutex::new(
            Box::new(SineSource::new(440.0, Some(0.2), 0.5, 44100, 2)) as Box<dyn AudioSource>,
        ));
        let mut pipe = PipeSource::spawn("cat", child, 2, FPBS, PipeConfig::default()).unwrap();
        let mut buf = vec![0f32; FPBS * 2];
        let expected_frames = (0.2 * 44100.0) as usize;
        let mut total = 0usize;
        let mut non_silent = 0;
        let mut guard = 0;
        while !pipe.is_exhausted() {
            let n = pipe.next_buffer(&mut buf);
            total += n;
            non_silent += buf[..n].iter().filter(|&&s| s.abs() > 0.01).count();
            guard += 1;
            assert!(guard < 400, "pipe never ended");
        }
        // Every complete chunk the child produced must have come through;
        // only the sub-buffer tail is dropped.
        assert!(
            total >= (expected_frames - FPBS) * 2,
            "drained only {total} of {} samples",
            expected_frames * 2
        );
        assert!(non_silent > 0, "no audio drained");
    }

    #[test]
    fn a_broken_command_bypasses_and_never_hangs() {
        let cfg = PipeConfig {
            restart_backoff_ms: 60_000,
            ..Default::default()
        };
        let mut pipe =
            PipeSource::spawn("no-such-command-xyz", sine_child(), 2, FPBS, cfg).unwrap();
        let mut buf = vec![0f32; FPBS * 2];
        // sh exits 127 immediately; the first chunk write fails and the pipe
        // lands in bypass with the raw child's audio.
        assert!(
            pull_nonzero(&mut pipe, &mut buf, 100) > 0,
            "no audio in bypass"
        );
        let mut non_silent = 0;
        for _ in 0..50 {
            let n = pipe.next_buffer(&mut buf);
            non_silent += buf[..n].iter().filter(|&&s| s.abs() > 0.01).count();
        }
        assert!(non_silent > 0, "bypass produced only silence");
        assert!(!pipe.is_exhausted());
        assert_eq!(
            pipe.shared.mode.load(Ordering::SeqCst),
            MODE_BYPASS,
            "pipe did not reach bypass"
        );
    }

    #[test]
    fn empty_command_is_rejected() {
        let err = PipeSource::spawn("", sine_child(), 2, FPBS, PipeConfig::default());
        assert!(err.is_err());
    }
}
