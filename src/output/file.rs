//! Local file recording via the engine tap.
//!
//! Mirrors `IcecastOutput` minus the network: a pure tap consumer that
//! encodes each frame and writes the bytes out, with no pacing of its own —
//! the tap paces the stream.

use std::fs::File;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;

use crate::config::FileOutputConfig;
use crate::engine::tap::AudioFrame;
use crate::output::encoder::{create_encoder, Encoder};
use crate::Result;

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
        while let Ok(frame) = self.rx.recv() {
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("shutdown requested, ending recording");
                break;
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputFormat;
    use crate::source::file::FileSource;
    use crate::source::AudioSource;
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
        let spec =
            symphonia::core::audio::SignalSpec::new(44100, symphonia::core::audio::Channels::FRONT_LEFT);
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
