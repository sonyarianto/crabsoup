use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::{OutputConfig, OutputFormat};
use crate::output::encoder::{create_encoder, Encoder};
use crate::output::icecast_client::IcecastClient;
use crate::source::AudioSource;
use crate::Result;

enum SendResult {
    Sent,
    Dropped,
}

/// Pulls audio from a source, encodes it, and pushes the result to Icecast
/// with automatic reconnection.
pub struct IcecastOutput {
    config: OutputConfig,
    source: Box<dyn AudioSource>,
    sample_rate: u32,
    chans: usize,
    frames_per_buffer: usize,
    shout: Option<IcecastClient>,
    encoder: Option<Box<dyn Encoder>>,
    last_title: String,
    shutdown: Arc<AtomicBool>,
}

impl IcecastOutput {
    pub fn new(
        config: OutputConfig,
        source: Box<dyn AudioSource>,
        sample_rate: u32,
        chans: usize,
        frames_per_buffer: usize,
    ) -> Self {
        Self {
            config,
            source,
            sample_rate,
            chans,
            frames_per_buffer,
            shout: None,
            encoder: None,
            last_title: String::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Give the output a shared flag that stops the pump loop (used for
    /// graceful Ctrl-C shutdown).
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = flag;
    }

    pub fn reconnect_seconds(&self) -> u64 {
        self.config.reconnect_seconds
    }

    /// Establish the initial source connection (caller decides retry policy).
    pub fn connect(&mut self) -> Result<()> {
        self.encoder = Some(create_encoder(
            self.config.format,
            self.sample_rate,
            self.chans as u16,
            self.config.bitrate,
            &self.config.name,
        )?);
        self.shout = Some(IcecastClient::connect(&self.config, self.sample_rate, self.chans as u16)?);
        log::info!(
            "connected to Icecast {}:{} mount {} ({})",
            self.config.host,
            self.config.port,
            self.config.mount,
            self.format_name()
        );
        Ok(())
    }

    fn format_name(&self) -> &'static str {
        match self.config.format {
            OutputFormat::Mp3 => "MP3",
            OutputFormat::Opus => "Ogg/Opus",
        }
    }

    /// Re-establish the connection, discarding the old encoder (fresh headers).
    fn reconnect(&mut self) {
        self.shout = None;
        self.encoder = None;

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("shutdown requested during reconnect");
                return;
            }
            match self.connect() {
                Ok(()) => {
                    log::info!("reconnected to Icecast");
                    return;
                }
                Err(e) => {
                    log::error!(
                        "Icecast reconnect failed: {e}; retrying in {}s",
                        self.config.reconnect_seconds
                    );
                    std::thread::sleep(Duration::from_secs(self.config.reconnect_seconds));
                }
            }
        }
    }

    fn send_or_reconnect(&mut self, data: &[u8]) -> SendResult {
        if data.is_empty() {
            return SendResult::Sent;
        }
        let Some(shout) = self.shout.as_mut() else {
            self.reconnect();
            return SendResult::Dropped;
        };
        match shout.send(data) {
            Ok(()) => SendResult::Sent,
            Err(e) => {
                log::error!("Icecast send failed: {e}");
                self.reconnect();
                SendResult::Dropped
            }
        }
    }

    fn update_metadata(&mut self) {
        let title = self.source.label().unwrap_or_default();
        if title != self.last_title {
            if let Err(e) = IcecastClient::update_title(&self.config, &title) {
                log::warn!("icecast metadata update failed: {e}");
            }
            self.last_title = title.clone();
            log::info!("icecast: now playing {title}");
        }
    }

    /// Run the pump loop until the source is exhausted. Blocks the caller.
    pub fn run(&mut self) -> Result<()> {
        let mut buf = vec![0f32; self.frames_per_buffer * self.chans];
        // Wall-clock pacing: consume input no faster than real time so the
        // encoder and Icecast are fed at stream rate.
        let start = std::time::Instant::now();
        let mut frames_pulled = 0u64;

        loop {
            if self.shutdown.load(Ordering::SeqCst) {
                log::info!("shutdown requested, ending stream");
                break;
            }

            // Pull at real-time rate: sleep until the next buffer is due.
            let elapsed_us = start.elapsed().as_micros() as u64;
            let next_due_us = frames_pulled * 1_000_000 / self.sample_rate as u64;
            if elapsed_us < next_due_us {
                std::thread::sleep(Duration::from_micros(next_due_us - elapsed_us));
            }

            let n = self.source.next_buffer(&mut buf);
            if n == 0 && self.source.is_exhausted() {
                break;
            }
            if n == 0 {
                log::debug!("pump: source underflow, pacing");
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            frames_pulled += (n / self.chans) as u64;
            self.update_metadata();

            let encoded = self.encoder.as_mut().unwrap().encode(&buf[..n]);
            if encoded.is_empty() {
                // The encoder accumulates internally (LAME needs 1152-sample
                // frames); nothing to send yet. Back off briefly.
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
            match self.send_or_reconnect(&encoded) {
                SendResult::Sent => {}
                SendResult::Dropped => continue,
            }
        }

        // Flush encoder tail and close cleanly.
        if let Some(encoder) = self.encoder.as_mut() {
            let tail = encoder.finish();
            self.send_or_reconnect(&tail);
        }
        self.shout = None;
        Ok(())
    }
}
