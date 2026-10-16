//! Local file recording via the engine tap.
//!
//! Mirrors `IcecastOutput` minus the network: a pure tap consumer that
//! encodes each frame and writes the bytes out, with no pacing of its own —
//! the tap paces the stream.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};

use crate::Result;
use crate::config::{FileOutputConfig, OutputFormat};
use crate::engine::tap::{AudioFrame, recv_frame_or_shutdown};
use crate::output::encoder::{Encoder, create_encoder};

/// Consumes frames from the engine tap, encodes them, and writes the result
/// to a local file (truncating any existing one). The encoder is created and
/// the file opened in [`FileOutput::connect`] so a bad path fails at startup.
pub struct FileOutput {
    config: FileOutputConfig,
    rx: Receiver<Arc<AudioFrame>>,
    sample_rate: u32,
    chans: usize,
    file: Option<File>,
    encoder: Option<Box<dyn Encoder>>,
    shutdown: Arc<AtomicBool>,
}

impl FileOutput {
    pub fn new(
        config: FileOutputConfig,
        rx: Receiver<Arc<AudioFrame>>,
        sample_rate: u32,
        chans: usize,
    ) -> Self {
        Self {
            config,
            rx,
            sample_rate,
            chans,
            file: None,
            encoder: None,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Give the output a shared flag that stops the consume loop (used for
    /// graceful Ctrl-C shutdown).
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = flag;
    }

    /// Create the encoder and open (truncate) the destination file.
    pub fn connect(&mut self) -> Result<()> {
        if self.file.is_some() {
            return Ok(());
        }
        let encoder = create_encoder(
            self.config.format,
            self.sample_rate,
            self.chans as u16,
            self.config.bitrate,
            &self.config.path.to_string_lossy(),
        )?;
        let file = File::create(&self.config.path)
            .map_err(|e| format!("cannot create {}: {e}", self.config.path.display()))?;
        self.encoder = Some(encoder);
        self.file = Some(file);
        log::info!(
            "recording {:?} to {}",
            self.config.format,
            self.config.path.display()
        );
        Ok(())
    }

    /// Consume frames until the stream ends (senders dropped) or shutdown is
    /// requested, then flush the encoder tail and close the file.
    pub fn run(&mut self) -> Result<()> {
        while let Some(frame) = recv_frame_or_shutdown(&self.rx, &self.shutdown) {
            let encoded = self.encoder.as_mut().unwrap().encode(&frame.pcm);
            if let Some(file) = self.file.as_mut() {
                file.write_all(&encoded)
                    .map_err(|e| format!("write {}: {e}", self.config.path.display()))?;
            }
        }

        if let (Some(encoder), Some(file)) = (self.encoder.as_mut(), self.file.as_mut()) {
            let tail = encoder.finish();
            file.write_all(&tail)
                .map_err(|e| format!("write {}: {e}", self.config.path.display()))?;
        }
        self.file = None;
        log::info!("recording closed: {}", self.config.path.display());
        Ok(())
    }
}

/// A copy of an `output.icecast` mount's encoded stream to a file, started and stopped from the
/// control port: the audio is encoded once, for the broadcast. A writer thread does the file IO,
/// so a slow disk cannot hold up the broadcast.
#[derive(Clone)]
pub struct Recording {
    format: OutputFormat,
    tx: SyncSender<Chunk>,
    path: Arc<Mutex<Option<PathBuf>>>,
}

enum Chunk {
    Start(File),
    Bytes(Vec<u8>),
    Stop(SyncSender<()>),
}

/// About ten seconds of a 256 kbps stream in 1024-frame buffers: room for a disk that stalls.
const RECORDING_QUEUE: usize = 512;

impl Recording {
    /// `format` is that of the output whose stream is copied.
    pub fn new(format: OutputFormat) -> Self {
        let (tx, rx) = sync_channel(RECORDING_QUEUE);
        let path = Arc::new(Mutex::new(None));
        let writer_path = path.clone();
        std::thread::spawn(move || write_chunks(rx, writer_path));
        Self { format, tx, path }
    }

    /// Start copying the stream to `path`, truncating it.
    pub fn start(&self, path: &Path) -> Result<()> {
        // An Ogg stream's headers come once, at its start: a copy from the middle does not play.
        if self.format == OutputFormat::Opus {
            return Err("an Ogg/Opus stream cannot be recorded from its middle".into());
        }
        if let Some(current) = self.path() {
            return Err(format!("already recording to {}", current.display()).into());
        }
        let file =
            File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        self.tx.send(Chunk::Start(file)).map_err(|_| "the recording writer stopped")?;
        log::info!("recording to {}", path.display());
        *self.path.lock().unwrap() = Some(path.to_path_buf());
        Ok(())
    }

    /// Close the file once everything copied so far is written.
    pub fn stop(&self) -> Result<()> {
        let path = self.path.lock().unwrap().take().ok_or("not recording")?;
        let (ack, written) = sync_channel(1);
        self.tx.send(Chunk::Stop(ack)).map_err(|_| "the recording writer stopped")?;
        written.recv().map_err(|_| "the recording writer stopped")?;
        log::info!("recording closed: {}", path.display());
        Ok(())
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.path.lock().unwrap().clone()
    }

    /// Called by the recorded output with each piece of its encoded stream.
    pub fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() || self.path.lock().unwrap().is_none() {
            return;
        }
        if self.tx.try_send(Chunk::Bytes(bytes.to_vec())).is_err() {
            log::warn!("recording: the disk is behind, {} bytes left out", bytes.len());
        }
    }
}

/// Runs until every clone of its [`Recording`] is dropped; a failed write ends the recording.
fn write_chunks(rx: Receiver<Chunk>, path: Arc<Mutex<Option<PathBuf>>>) {
    let mut file = None;
    for chunk in rx {
        match chunk {
            Chunk::Start(new) => file = Some(new),
            Chunk::Bytes(bytes) => {
                if let Some(open) = file.as_mut()
                    && let Err(e) = open.write_all(&bytes)
                {
                    log::error!("recording to {:?} stopped: {e}", path.lock().unwrap().take());
                    file = None;
                }
            }
            Chunk::Stop(ack) => {
                file = None;
                let _ = ack.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputFormat;
    #[cfg(feature = "mp3")]
    use crate::source::AudioSource;
    #[cfg(feature = "mp3")]
    use crate::source::file::FileSource;
    use std::sync::mpsc;

    fn sine_frames(tx: &mpsc::SyncSender<Arc<AudioFrame>>, seconds: f64) {
        let rate = 44_100.0;
        let mut phase = 0.0;
        let total = (seconds * rate) as usize;
        let mut done = 0;
        while done < total {
            let n = 4096.min(total - done);
            let mut pcm = Vec::with_capacity(n);
            for _ in 0..n {
                pcm.push((phase * 2.0 * std::f64::consts::PI * 440.0 / rate).sin() as f32 * 0.5);
                phase += 1.0;
            }
            let frame = Arc::new(AudioFrame {
                pcm,
                label: Some("test tone".into()),
                pool: None,
            });
            tx.send(frame).expect("tap channel");
            done += n;
        }
    }

    #[cfg(feature = "mp3")]
    #[test]
    fn records_mp3_that_decodes_back_with_symphonia() {
        let path = std::env::temp_dir().join("crabsoup-c1.mp3");
        let _ = std::fs::remove_file(&path);
        let cfg = FileOutputConfig {
            path: path.clone(),
            format: OutputFormat::Mp3,
            bitrate: 64_000,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = FileOutput::new(cfg, rx, 44_100, 1);
        output.connect().expect("file opens");

        let handle = std::thread::spawn(move || output.run());
        sine_frames(&tx, 0.5);
        drop(tx);
        handle.join().expect("record thread").expect("clean finish");

        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        assert!(len > 1_000, "recorded file too small: {len} bytes");
        let magic = std::fs::read(&path).unwrap();
        assert!(
            magic.starts_with(b"ID3") || magic[0] == 0xff,
            "expected MP3 magic bytes"
        );

        // Decode the file back and require real audio.
        let spec = symphonia::core::audio::SignalSpec::new(
            44100,
            symphonia::core::audio::Channels::FRONT_LEFT,
        );
        let mut src = FileSource::open(&path, spec, 4096).expect("decodes");
        let mut buf = vec![0f32; 44100];
        let n = src.next_buffer(&mut buf);
        assert!(n > 0, "decoded no audio");
        assert!(
            buf[..n].iter().any(|&s| s.abs() > 0.1),
            "decoded audio must not be silence"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_recording_holds_only_the_stream_copied_between_start_and_stop() {
        let path = std::env::temp_dir().join("crabsoup-recording.aac");
        let recording = Recording::new(OutputFormat::Aac);

        recording.write(b"before ");
        recording.start(&path).expect("recording starts");
        recording.write(b"during ");
        recording.write(b"the set");
        recording.stop().expect("recording stops");
        recording.write(b" after");

        assert_eq!(std::fs::read(&path).unwrap(), b"during the set");
        assert_eq!(recording.path(), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_ogg_opus_stream_cannot_be_recorded_from_its_middle() {
        let recording = Recording::new(OutputFormat::Opus);

        let started = recording.start(&std::env::temp_dir().join("crabsoup-recording.ogg"));

        assert!(started.is_err());
    }

    #[test]
    fn records_opus_as_an_ogg_stream() {
        let path = std::env::temp_dir().join("crabsoup-c1.ogg");
        let _ = std::fs::remove_file(&path);
        let cfg = FileOutputConfig {
            path: path.clone(),
            format: OutputFormat::Opus,
            bitrate: 64_000,
        };
        let (tx, rx) = mpsc::sync_channel(8);
        let mut output = FileOutput::new(cfg, rx, 44_100, 1);
        output.connect().expect("file opens");

        let handle = std::thread::spawn(move || output.run());
        sine_frames(&tx, 0.2);
        drop(tx);
        handle.join().expect("record thread").expect("clean finish");

        let data = std::fs::read(&path).unwrap();
        assert!(data.starts_with(b"OggS"), "expected Ogg magic bytes");
        assert!(data.len() > 500, "recorded file too small");
        let _ = std::fs::remove_file(&path);
    }
}
