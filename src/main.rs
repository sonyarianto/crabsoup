use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use crabsoup::engine::mixer::{MixCommand, PriorityMixer, StatusHandle};
use crabsoup::live::harbor::Harbor;
use crabsoup::output::icecast::IcecastOutput;
use crabsoup::script::{self, ScriptResult};
use crabsoup::source::AudioSource;

#[derive(Parser)]
#[command(name = "crabsoup", version, about = "Liquidsoap-inspired audio streaming engine")]
struct Cli {
    /// Path to the .lua script (Lua).
    #[arg(short, long, default_value = "crabsoup.lua")]
    config: PathBuf,
    /// Evaluate the script, print the resulting configuration, and exit.
    #[arg(long)]
    check: bool,
    /// Decode and mix but never broadcast, even if the script defines an
    /// `output.icecast`.
    #[arg(long)]
    preview: bool,
}

fn main() -> crabsoup::Result<()> {
    env_logger::init();
    let cli = Cli::parse();

    let src = std::fs::read_to_string(&cli.config)
        .map_err(|e| format!("failed to read script {}: {e}", cli.config.display()))?;
    let mut result = script::run(&src).map_err(|e| format!("script error: {e}"))?;

    if cli.check {
        print_result(&result, cli.preview);
        return Ok(());
    }

    let spec = result.stream.signal_spec();
    let chans = spec.channels.count();
    let fpb = result.stream.frames_per_buffer;

    // Root source -> priority mixer (live DJ ducking / jingle overrides).
    let root_source = result
        .root
        .take()
        .or_else(|| result.preview.take())
        .expect("script output checked by run()");
    let (tx, rx) = mpsc::channel();

    // `--preview` forces the preview path regardless of the script's output.
    let broadcast = result.output.take().filter(|_| !cli.preview);
    let mut root: Box<dyn AudioSource> = Box::new(PriorityMixer::new(
        root_source,
        rx,
        &result.mixer,
        spec,
        fpb,
    ));

    // Background tokio runtime: live harbor listener + Ctrl-C handler.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Shared status for the telnet `status`/`uptime` commands; the pump loop
    // keeps the current label fresh.
    let status = StatusHandle::new();

    if let Some(live_cfg) = &result.harbor {
        let harbor = Harbor::new(live_cfg.clone(), spec, tx.clone());
        rt.spawn(async move { harbor.run().await });
    }

    if let Some(ctl_cfg) = &result.control {
        let jingles = result.jingles.clone();
        let server = crabsoup::control::ControlServer::new(
            ctl_cfg.clone(),
            jingles,
            tx.clone(),
            status.clone(),
        );
        rt.spawn(async move { server.run().await });
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

    match &broadcast {
        Some(out_cfg) => {
            let mut output =
                IcecastOutput::new(out_cfg.clone(), root, spec.rate, chans, fpb);
            output.set_shutdown(shutdown.clone());
            output.set_status(status.clone());

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
            if cli.preview {
                log::info!("--preview: decoding and mixing, not broadcasting");
            } else {
                log::warn!(
                    "no output.icecast in script; running in preview mode \
                     (decoding but not broadcasting)"
                );
            }
            run_preview(&mut *root, spec.rate, chans, fpb, shutdown, &status)
        }
    }
}

/// `--check` output: a human-readable summary of the script result.
fn print_result(result: &ScriptResult, preview: bool) {
    let stream = &result.stream;
    let mixer = &result.mixer;
    println!(
        "stream: {} Hz, {} ch, {} frames/buffer",
        stream.sample_rate, stream.channels, stream.frames_per_buffer
    );
    println!(
        "mixer: crossfade {:.1}s, curve {}, duck {:.1}s",
        mixer.crossfade_seconds, mixer.fade_curve, mixer.duck_seconds
    );
    println!("jingles: {} file(s)", result.jingles.len());
    if let Some(h) = &result.harbor {
        println!("harbor: {}:{}{}", h.host, h.port, h.mount);
    }
    if let Some(c) = &result.control {
        println!("telnet: {}:{}", c.host, c.port);
    }
    if preview {
        println!("output: preview only (forced by --preview)");
    } else {
        match &result.output {
            Some(out) => println!(
                "output: {:?} to {}:{}{}",
                out.format, out.host, out.port, out.mount
            ),
            None => println!("output: preview only"),
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
    status: &StatusHandle,
) -> crabsoup::Result<()> {
    let interval = Duration::from_secs_f64(fpb as f64 / sample_rate as f64);
    let mut buf = vec![0f32; fpb * chans];
    let mut last = None;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            log::info!("preview: shutdown requested");
            return Ok(());
        }
        let n = root.next_buffer(&mut buf);
        if n == 0 && root.is_exhausted() {
            log::info!("preview: source ended");
            return Ok(());
        }
        let label = root.label().unwrap_or_default();
        status.set_current(&label);
        if Some(label.as_str()) != last.as_deref() {
            log::info!("now playing: {label}");
            last = Some(label);
        }
        std::thread::sleep(interval);
    }
}
