use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use crabsoup::config::Config;
use crabsoup::engine::mixer::{CrossfadeMixer, MixCommand, PriorityMixer};
use crabsoup::live::harbor::Harbor;
use crabsoup::output::icecast::IcecastOutput;
use crabsoup::source::playlist::Playlist;
use crabsoup::source::AudioSource;

#[derive(Parser)]
#[command(name = "crabsoup", version, about = "Liquidsoap-inspired audio streaming engine")]
struct Cli {
    /// Path to the YAML configuration file.
    #[arg(short, long, default_value = "crabsoup.yaml")]
    config: PathBuf,
    /// Validate the configuration and exit.
    #[arg(long)]
    check: bool,
}

fn main() -> crabsoup::Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let config = Config::load(&cli.config)?;
    if cli.check {
        println!("{config:#?}");
        return Ok(());
    }

    let media = config.media_files();
    if media.is_empty() {
        return Err("playlist is empty: no audio files configured".into());
    }
    log::info!("loaded {} media file(s)", media.len());

    let spec = config.signal_spec();
    let chans = spec.channels.count();
    let fpb = config.stream.frames_per_buffer;

    // Playlist -> crossfade mixer -> priority (live/jingle) mixer.
    let playlist = Playlist::new(
        media,
        config.playlist.shuffle,
        config.playlist.loop_playlist,
        spec,
        fpb,
        None,
    );
    let crossfade = CrossfadeMixer::new(Box::new(playlist), &config.mixer, spec.rate, chans);

    let (tx, rx) = mpsc::channel();
    let pm = PriorityMixer::new(Box::new(crossfade), rx, &config.mixer, spec, fpb);
    let mut root: Box<dyn AudioSource> = Box::new(pm);

    // Background tokio runtime: live harbor listener + Ctrl-C handler.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    if let Some(live_cfg) = &config.live {
        let harbor = Harbor::new(live_cfg.clone(), spec, tx.clone());
        rt.spawn(async move { harbor.run().await });
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let ctrl_shutdown = shutdown.clone();
    let ctrl_tx = tx.clone();
    rt.spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        log::info!("Ctrl-C received, shutting down");
        ctrl_shutdown.store(true, Ordering::SeqCst);
        let _ = ctrl_tx.send(MixCommand::Shutdown);
    });

    match &config.output {
        Some(out_cfg) => {
            let mut output =
                IcecastOutput::new(out_cfg.clone(), root, spec.rate, chans, fpb);
            output.set_shutdown(shutdown.clone());

            // Initial connection with a bounded retry loop so Ctrl-C works
            // before the first successful connect.
            loop {
                match output.connect() {
                    Ok(()) => break,
                    Err(e) => {
                        log::error!("cannot connect to Icecast: {e}");
                        if shutdown.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        std::thread::sleep(Duration::from_secs(output.reconnect_seconds()));
                    }
                }
            }
            output.run()
        }
        None => {
            log::warn!(
                "no `output` section in config; running in preview mode \
                 (decoding but not broadcasting)"
            );
            run_preview(&mut *root, spec.rate, chans, fpb, shutdown)
        }
    }
}

/// Preview mode: pull audio from the mixer and log the current label, but do
/// not encode or broadcast.
fn run_preview(
    root: &mut dyn AudioSource,
    sample_rate: u32,
    chans: usize,
    fpb: usize,
    shutdown: Arc<AtomicBool>,
) -> crabsoup::Result<()> {
    let interval = Duration::from_secs_f64(fpb as f64 / sample_rate as f64);
    let mut buf = vec![0f32; fpb * chans];
    let mut last = None;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            log::info!("preview: shutdown requested");
            return Ok(());
        }
        root.next_buffer(&mut buf);
        let label = root.label().unwrap_or_default();
        if Some(label.as_str()) != last.as_deref() {
            log::info!("now playing: {label}");
            last = Some(label);
        }
        std::thread::sleep(interval);
    }
}
