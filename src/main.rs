use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use clap::{CommandFactory, Parser};

use crabsoup::engine::mixer::{MixCommand, PriorityMixer, StatusHandle};
use crabsoup::engine::tap::{AudioFrame, EngineTap, interruptible_sleep, recv_frame_or_shutdown};
use crabsoup::live::harbor::Harbor;
use crabsoup::output::file::FileOutput;
use crabsoup::output::hls::HlsOutput;
use crabsoup::output::icecast::IcecastOutput;
#[cfg(feature = "video")]
use crabsoup::output::mp4::Mp4Output;
#[cfg(feature = "rtmp")]
use crabsoup::output::rtmp::RtmpOutput;
use crabsoup::output::soundcard::SoundcardOutput;
use crabsoup::script::{self, ScriptResult};
use crabsoup::source::AudioSource;

#[derive(Parser)]
#[command(
    name = "crabsoup",
    version,
    about = "Liquidsoap-inspired audio streaming engine"
)]
struct Cli {
    /// Path to the .lua script (Lua). Required; running without it shows
    /// help.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Evaluate the script, print the resulting configuration, and exit.
    #[arg(long)]
    check: bool,
    /// Decode and mix but never broadcast, even if the script defines an
    /// `output.icecast`.
    #[arg(long)]
    preview: bool,
}

/// The first registered video track's spec — `video.video` first, then the
/// first `video.playlist`/`video.single` track, then the first
/// `video.slideshow` — shared by the video-enabled outputs (HLS and RTMP).
/// Effects (Part H3) may rescale the published frames, so the encoder opens
/// at the *scaled* spec. Defined in `script.rs` so it is unit-testable.
#[cfg(feature = "video")]
fn first_video_spec(result: &ScriptResult) -> Option<crabsoup::video::VideoSpec> {
    crabsoup::script::first_video_spec(result)
}

fn main() -> crabsoup::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("crabsoup=info"))
        .init();
    let cli = Cli::parse();
    let Some(config_path) = &cli.config else {
        Cli::command().print_help()?;
        return Ok(());
    };
    let src = std::fs::read_to_string(config_path)
        .map_err(|e| format!("failed to read script {}: {e}", config_path.display()))?;
    let (runtime, mut result) = script::run(&src).map_err(|e| format!("script error: {e}"))?;

    if cli.check {
        print_result(&result, cli.preview);
        return Ok(());
    }

    let spec = result.stream.signal_spec();
    let chans = spec.channels.count();
    let fpb = result.stream.frames_per_buffer;

    // Root source -> priority mixer (live DJ ducking / jingle overrides) ->
    // engine tap: one puller thread, N consumer taps.
    let root_source = result
        .root
        .take()
        .or_else(|| result.preview.take())
        .expect("script output checked by run()");
    let (tx, rx) = mpsc::channel();
    let root: Box<dyn AudioSource> = Box::new(PriorityMixer::new(
        root_source,
        rx,
        &result.mixer,
        spec,
        fpb,
    ));
    let mut tap = EngineTap::new(root, spec.rate, chans);

    let broadcast = if cli.preview {
        Vec::new()
    } else {
        result.outputs.clone()
    };

    // Shared status for the telnet `status`/`uptime` commands; the consuming
    // loops keep the current label fresh.
    let status = StatusHandle::new();
    let shutdown = Arc::new(AtomicBool::new(false));

    // Background tokio runtime: live harbor listener + Ctrl-C handler.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    if let Some(live_cfg) = &result.harbor {
        let harbor = Harbor::new(live_cfg.clone(), spec, tx.clone(), status.harbor_flag());
        rt.spawn(async move { harbor.run().await });
    }

    if let Some(ctl_cfg) = &result.control {
        let jingles = result.jingles.clone();
        let queue = result.request_queue.clone();
        let custom = Arc::new(result.custom_commands.clone());
        let event_tx = runtime.event_tx();
        let server = crabsoup::control::ControlServer::new(
            ctl_cfg.clone(),
            jingles.clone(),
            queue.clone(),
            tx.clone(),
            status.clone(),
            custom.clone(),
            event_tx.clone(),
        );
        rt.spawn(async move { server.run().await });

        if let Some(http_port) = ctl_cfg.http_port {
            let http = crabsoup::control::ControlHttpServer::new(
                ctl_cfg.host.clone(),
                http_port,
                jingles,
                queue,
                tx.clone(),
                status.clone(),
                custom,
                event_tx,
            );
            rt.spawn(async move { http.run().await });
        }
    }

    // Ctrl-C: set the flag and tell the mixer to stop.
    let ctrl_shutdown = shutdown.clone();
    let ctrl_tx = tx.clone();
    rt.spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                ctrl_shutdown.store(true, Ordering::SeqCst);
                let _ = ctrl_tx.send(MixCommand::Shutdown);
                log::info!("ctrl-c: shutdown requested");
            }
            Err(e) => log::error!("ctrl-c listener: {e}"),
        }
    });

    // One consuming thread per output; each keeps its own reconnect loop so
    // one stalled mount cannot affect the pull rate or the other outputs.
    let mut handles = Vec::new();
    for out_cfg in &broadcast {
        let out_rx = tap.register();
        let out_shutdown = shutdown.clone();
        let out_status = status.clone();
        let cfg = out_cfg.clone();
        let rate = spec.rate;
        let channels = chans;
        handles.push(std::thread::spawn(move || {
            let mut output = IcecastOutput::new(cfg, out_rx, rate, channels);
            output.set_shutdown(out_shutdown.clone());
            output.set_status(out_status);
            // Initial connection: retry until we reach the server, but stop
            // if the operator hits Ctrl-C while we wait.
            loop {
                match output.connect() {
                    Ok(()) => break,
                    Err(e) => {
                        log::error!("cannot connect to Icecast: {e}");
                        if out_shutdown.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        interruptible_sleep(
                            Duration::from_secs(output.reconnect_seconds()),
                            &out_shutdown,
                        );
                    }
                }
            }
            output.run()
        }));
    }

    if broadcast.is_empty() {
        let preview_rx = tap.register();
        let preview_shutdown = shutdown.clone();
        let preview_status = status.clone();
        handles.push(std::thread::spawn(move || {
            run_preview(preview_rx, preview_shutdown, preview_status)
        }));
    }

    // File outputs: the encoder and file are created up front so a bad path
    // fails fast at startup; the consumer thread then just drains the tap.
    let record = if cli.preview {
        Vec::new()
    } else {
        result.file_outputs.clone()
    };
    for cfg in &record {
        let mut output = FileOutput::new(cfg.clone(), tap.register(), spec.rate, chans);
        output.set_shutdown(shutdown.clone());
        output.connect().map_err(|e| format!("output.file: {e}"))?;
        handles.push(std::thread::spawn(move || output.run()));
    }

    // HLS outputs: the directory is prepared up front so a bad path fails
    // fast; the consumer thread then rotates the segment window.
    let hls = if cli.preview {
        Vec::new()
    } else {
        result.hls_outputs.clone()
    };
    for cfg in &hls {
        // Part H6: an HLS output marked `video` subscribes to the shared
        // video tap; the first registered track's (first playlist track's,
        // Part H7, or slideshow's, Part H2) spec drives the encoder.
        #[cfg(feature = "video")]
        let hls_video = if cfg.video {
            let tap = result.video_tap.clone().ok_or_else(|| {
                "output.hls({video = ...}) requires a video source in the script".to_string()
            })?;
            let spec = first_video_spec(&result)
                .ok_or_else(|| "output.hls video track has no spec".to_string())?;
            // The output subscribes one consumer per rendition (classic:
            // one), so per-rendition H.264 encodes share the same tap.
            Some((tap, spec))
        } else {
            None
        };
        #[cfg(not(feature = "video"))]
        let hls_video = ();
        let mut output = HlsOutput::new(cfg.clone(), tap.register(), spec.rate, chans, hls_video);
        output.set_shutdown(shutdown.clone());
        output.connect().map_err(|e| format!("output.hls: {e}"))?;
        handles.push(std::thread::spawn(move || output.run()));
    }

    // RTMP outputs (Part H5): publish FLV (AAC audio, optional H.264 video)
    // to an RTMP server. Connection is lazy with retries — an unreachable
    // server does not fail startup, the output just waits.
    #[cfg(feature = "rtmp")]
    let rtmp = if cli.preview {
        Vec::new()
    } else {
        result.rtmp_outputs.clone()
    };
    #[cfg(feature = "rtmp")]
    for cfg in &rtmp {
        // Part H6 model: an RTMP output marked `video` subscribes to the
        // shared video tap; the first registered track's spec drives the
        // H.264 encoder.
        #[cfg(feature = "video")]
        let rtmp_video = if cfg.video {
            let tap = result.video_tap.clone().ok_or_else(|| {
                "output.rtmp({video = ...}) requires a video source in the script".to_string()
            })?;
            let spec = first_video_spec(&result)
                .ok_or_else(|| "output.rtmp video track has no spec".to_string())?;
            Some((tap.register(), spec))
        } else {
            None
        };
        #[cfg(not(feature = "video"))]
        let rtmp_video = ();
        let mut output = RtmpOutput::new(cfg.clone(), tap.register(), spec.rate, chans, rtmp_video);
        let out_shutdown = shutdown.clone();
        output.set_shutdown(out_shutdown.clone());
        handles.push(std::thread::spawn(move || {
            loop {
                match output.connect() {
                    Ok(()) => break,
                    Err(e) => {
                        log::error!("cannot connect to RTMP: {e}");
                        if out_shutdown.load(Ordering::SeqCst) {
                            return Ok(());
                        }
                        interruptible_sleep(
                            Duration::from_secs(output.reconnect_seconds()),
                            &out_shutdown,
                        );
                    }
                }
            }
            output.run()
        }));
    }

    // MP4 outputs (Part H4): the file and streams are opened up front so a
    // bad path fails fast; the consumer thread then interleaves A/V into
    // the recording.
    #[cfg(feature = "video")]
    let mp4 = if cli.preview {
        Vec::new()
    } else {
        result.mp4_outputs.clone()
    };
    #[cfg(feature = "video")]
    for cfg in &mp4 {
        // Part H4: an MP4 output marked `video` subscribes to the shared
        // video tap; the first registered track's spec drives the H.264
        // encoder.
        let mp4_video = if cfg.video {
            let tap = result.video_tap.clone().ok_or_else(|| {
                "output.mp4({video = ...}) requires a video source in the script".to_string()
            })?;
            let spec = first_video_spec(&result)
                .ok_or_else(|| "output.mp4 video track has no spec".to_string())?;
            Some((tap.register(), spec))
        } else {
            None
        };
        let mut output = Mp4Output::new(cfg.clone(), tap.register(), spec.rate, chans, mp4_video);
        output.set_shutdown(shutdown.clone());
        output.connect().map_err(|e| format!("output.mp4: {e}"))?;
        handles.push(std::thread::spawn(move || output.run()));
    }

    // Soundcard outputs: the device and stream are opened up front so a
    // missing device fails fast; the consumer thread then just pumps frames
    // into the ring the realtime callback drains.
    let soundcard = if cli.preview {
        Vec::new()
    } else {
        result.soundcard_outputs.clone()
    };
    for cfg in &soundcard {
        let mut output = SoundcardOutput::new(cfg.clone(), tap.register(), spec.rate, chans);
        output.set_shutdown(shutdown.clone());
        output
            .connect()
            .map_err(|e| format!("output.soundcard: {e}"))?;
        handles.push(std::thread::spawn(move || output.run()));
    }

    // Video decode threads (Part H): one per `video.video(path)` track, one
    // per `video.playlist`/`video.single` sequence (Part H7) and one per
    // `video.slideshow` (Part H2), all publishing PTS-paced frames to the
    // shared tap; video outputs subscribe to the same tap. Handles stay
    // alive until process exit.
    #[cfg(feature = "video")]
    let mut video_handles = Vec::new();
    #[cfg(feature = "video")]
    if let Some(vtap) = &result.video_tap {
        for cfg in &result.video {
            let tap = vtap.clone();
            let stop = shutdown.clone();
            match crabsoup::video::VideoSource::spawn(cfg, tap, stop) {
                Ok(handle) => video_handles.push(handle),
                Err(e) => return Err(format!("video.video: {e}").into()),
            }
        }
        for cfg in &result.video_playlists {
            let tap = vtap.clone();
            let stop = shutdown.clone();
            match crabsoup::video::VideoSource::spawn_playlist(cfg, tap, stop) {
                Ok(handle) => video_handles.push(handle),
                Err(e) => return Err(format!("video.playlist: {e}").into()),
            }
        }
        for cfg in &result.video_slideshows {
            let tap = vtap.clone();
            let stop = shutdown.clone();
            match crabsoup::video::VideoSource::spawn_slideshow(cfg, tap, stop) {
                Ok(handle) => video_handles.push(handle),
                Err(e) => return Err(format!("video.slideshow: {e}").into()),
            }
        }
    }

    // The tap pulls on its own thread; the Lua-owning main thread runs the
    // script event loop.
    let tap_shutdown = shutdown.clone();
    let tap_handle = std::thread::spawn(move || tap.run(fpb, tap_shutdown));

    runtime.run_event_loop(&shutdown);

    let mut first_error: Option<String> = None;
    for handle in handles {
        match handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::error!("output thread error: {e}");
                if first_error.is_none() {
                    first_error = Some(e.to_string());
                }
            }
            Err(_) => log::error!("output thread panicked"),
        }
    }
    let _ = tap_handle.join();
    if let Some(msg) = first_error {
        return Err(msg.into());
    }
    Ok(())
}

fn print_result(result: &ScriptResult, preview: bool) {
    let mut lines = vec![
        format!(
            "stream: {} Hz, {} ch, {} frames/buffer",
            result.stream.sample_rate, result.stream.channels, result.stream.frames_per_buffer
        ),
        format!(
            "mixer: crossfade {}s, curve {}, duck {}s",
            result.mixer.crossfade_seconds, result.mixer.fade_curve, result.mixer.duck_seconds
        ),
        format!("jingles: {} file(s)", result.jingles.len()),
    ];
    if let Some(harbor) = &result.harbor {
        lines.push(format!("harbor: {}:{}", harbor.host, harbor.port));
    }
    if let Some(ctl) = &result.control {
        lines.push(format!("telnet: {}:{}", ctl.host, ctl.port));
    }
    for name in &result.custom_commands {
        lines.push(format!("custom command: {name}"));
    }
    if preview {
        lines.push("output: preview only (forced by --preview)".to_string());
    } else {
        for out in &result.outputs {
            lines.push(format!(
                "output: {} {:?} to {}:{}{}",
                out.protocol.name(),
                out.format,
                out.host,
                out.port,
                out.mount
            ));
        }
        for rec in &result.file_outputs {
            lines.push(format!(
                "record: {:?} to {}",
                rec.format,
                rec.path.display()
            ));
        }
        for hls in &result.hls_outputs {
            if hls.renditions.is_empty() {
                lines.push(format!(
                    "hls: AAC segments to {} ({:.1}s x {})",
                    hls.directory.display(),
                    hls.segment_seconds,
                    hls.retention
                ));
            } else {
                lines.push(format!(
                    "hls: {} ABR renditions to {} ({:.1}s x {})",
                    hls.renditions.len(),
                    hls.directory.display(),
                    hls.segment_seconds,
                    hls.retention
                ));
            }
        }
        for sc in &result.soundcard_outputs {
            lines.push(format!(
                "soundcard: output to {}",
                sc.device.as_deref().unwrap_or("(default)")
            ));
        }
        if result.outputs.is_empty()
            && result.file_outputs.is_empty()
            && result.hls_outputs.is_empty()
            && result.soundcard_outputs.is_empty()
        {
            lines.push("output: preview only".to_string());
        }
    }
    println!("{}", lines.join("\n"));
}

fn run_preview(
    rx: mpsc::Receiver<Arc<AudioFrame>>,
    shutdown: Arc<AtomicBool>,
    status: StatusHandle,
) -> crabsoup::Result<()> {
    let mut last: Option<String> = None;
    while let Some(frame) = recv_frame_or_shutdown(&rx, &shutdown) {
        let label = frame.label.as_deref().unwrap_or_default().to_string();
        status.set_current(&label);
        if Some(label.as_str()) != last.as_deref() {
            log::info!("now playing: {label}");
            last = Some(label);
        }
    }
    Ok(())
}
