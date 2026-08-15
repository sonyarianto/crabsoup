use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::Result;
use crate::config::{OutputConfig, OutputFormat, OutputProtocol};
use crate::engine::mixer::StatusHandle;
use crate::engine::tap::{AudioFrame, interruptible_sleep, recv_frame_or_shutdown};
use crate::output::encoder::{AacEncoder, Encoder, create_encoder};
use crate::output::icecast_client::IcecastClient;

enum SendResult {
    Sent,
    Dropped,
}

/// Consumes frames from the engine tap, encodes them, and pushes the result
/// to Icecast with automatic reconnection.
///
/// This is a pure consumer: the tap owns the pull loop and paces the stream,
/// so a stalled connection drops frames here instead of stalling the engine
/// or the other outputs.
pub struct IcecastOutput {
    config: OutputConfig,
    rx: Receiver<Arc<AudioFrame>>,
    sample_rate: u32,
    chans: usize,
    shout: Option<IcecastClient>,
    encoder: Option<Box<dyn Encoder>>,
    last_title: String,
    shutdown: Arc<AtomicBool>,
    status: Option<StatusHandle>,
}

impl IcecastOutput {
    pub fn new(
        config: OutputConfig,
        rx: Receiver<Arc<AudioFrame>>,
        sample_rate: u32,
        chans: usize,
    ) -> Self {
        Self {
            config,
            rx,
            sample_rate,
            chans,
            shout: None,
            encoder: None,
            last_title: String::new(),
            shutdown: Arc::new(AtomicBool::new(false)),
            status: None,
        }
    }

    /// Give the output a shared flag that stops the pump loop (used for
    /// graceful Ctrl-C shutdown).
    pub fn set_shutdown(&mut self, flag: Arc<AtomicBool>) {
        self.shutdown = flag;
    }

    /// Expose the current track title to the control port via [`StatusHandle`].
    pub fn set_status(&mut self, status: StatusHandle) {
        self.status = Some(status);
    }

    pub fn reconnect_seconds(&self) -> u64 {
        self.config.reconnect_seconds
    }

    /// Establish the initial source connection (caller decides retry policy).
    pub fn connect(&mut self) -> Result<()> {
        self.encoder = Some(match (self.config.protocol, self.config.format) {
            // SHOUTcast v2 exposes AAC as "AAC+" (`audio/aacp`): HE-AAC with
            // SBR rather than the plain AAC-LC used for Icecast and HLS.
            (OutputProtocol::ShoutcastV2, OutputFormat::Aac) => Box::new(AacEncoder::new_he_aac(
                self.sample_rate,
                self.chans as u16,
                self.config.bitrate,
            )?),
            _ => create_encoder(
                self.config.format,
                self.sample_rate,
                self.chans as u16,
                self.config.bitrate,
                &self.config.name,
            )?,
        });
        self.shout = Some(IcecastClient::connect(
            &self.config,
            self.sample_rate,
            self.chans as u16,
            &self.shutdown,
        )?);
        log::info!(
            "connected to {} {}:{} mount {} ({})",
            self.config.protocol.name(),
            self.config.host,
            self.config.port,
            self.config.mount,
            self.format_name()
        );
        Ok(())
    }

    fn format_name(&self) -> &'static str {
        match (self.config.protocol, self.config.format) {
            (OutputProtocol::ShoutcastV1 | OutputProtocol::ShoutcastV2, OutputFormat::Aac) => {
                "AAC+ (HE-AAC)"
            }
            (_, OutputFormat::Mp3) => "MP3",
            (_, OutputFormat::Opus) => "Ogg/Opus",
            (_, OutputFormat::Aac) => "AAC",
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
                    interruptible_sleep(
                        Duration::from_secs(self.config.reconnect_seconds),
                        &self.shutdown,
                    );
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

    fn update_metadata(&mut self, frame: &AudioFrame) {
        let title = frame.label.as_deref().unwrap_or_default().to_string();
        if let Some(status) = &self.status {
            status.set_current(&title);
        }
        if title != self.last_title {
            self.last_title = title.clone();
            log::info!("{}: now playing {title}", self.config.protocol.name());
            match self.config.protocol {
                OutputProtocol::Icecast => match self.config.format {
                    // Opus mounts reject Icecast's URL metadata endpoint, and
                    // Icecast parses OpusTags only as stream headers (2.5+);
                    // the pre-flush set_title below gets the first track into
                    // them.
                    OutputFormat::Opus => {
                        if let Some(encoder) = self.encoder.as_mut() {
                            let tags = encoder.set_title(&title);
                            if !tags.is_empty() {
                                self.send_or_reconnect(&tags);
                            }
                        }
                    }
                    OutputFormat::Mp3 => {
                        // A metadata update is a fresh HTTP round-trip to
                        // the server; done inline it stalls the output
                        // thread, the tap channel overflows and frames are
                        // dropped mid-crossfade. Run it on a background
                        // thread instead — updates are minutes apart, so
                        // they cannot overtake each other.
                        let cfg = self.config.clone();
                        let shutdown = self.shutdown.clone();
                        std::thread::spawn(move || {
                            if let Err(e) = IcecastClient::update_title(&cfg, &title, &shutdown) {
                                log::warn!("icecast metadata update failed: {e}");
                            }
                        });
                    }
                    // ADTS has no in-stream title mechanism; nothing to send.
                    OutputFormat::Aac => {}
                },
                // SHOUTcast updates titles via the DNAS's /admin.cgi
                // updinfo endpoint (the DNAS does not parse in-stream ICY
                // metadata from sources).
                OutputProtocol::ShoutcastV1 | OutputProtocol::ShoutcastV2 => {
                    let cfg = self.config.clone();
                    let shutdown = self.shutdown.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = IcecastClient::update_icy_title(&cfg, &title, &shutdown) {
                            log::warn!("shoutcast metadata update failed: {e}");
                        }
                    });
                }
            }
        }
    }

    /// Consume frames from the tap until the stream ends (senders dropped)
    /// or shutdown is requested.
    pub fn run(&mut self) -> Result<()> {
        while let Some(frame) = recv_frame_or_shutdown(&self.rx, &self.shutdown) {
            self.update_metadata(&frame);

            let encoded = self.encoder.as_mut().unwrap().encode(&frame.pcm);
            if encoded.is_empty() {
                // The encoder accumulates internally (LAME needs 1152-sample
                // frames); nothing to send yet.
                continue;
            }
            self.send_or_reconnect(&encoded);
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
