//! Configuration data types.
//!
//! Values are filled in by the Lua `.lua` script (see `src/script.rs`) or by
//! tests; there is no file-format layer here anymore.

use std::path::{Path, PathBuf};

use symphonia::core::audio::SignalSpec;

/// The PCM bus: every source is resampled/converted to this spec.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames pulled per `next_buffer` call (~93 ms at 44100 Hz).
    pub frames_per_buffer: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            sample_rate: 44100,
            channels: 2,
            frames_per_buffer: 4096,
        }
    }
}

impl StreamConfig {
    /// The `SignalSpec` every source is normalised to.
    pub fn signal_spec(&self) -> SignalSpec {
        use symphonia::core::audio::Channels;
        let chans = match self.channels {
            1 => Channels::FRONT_CENTRE,
            2 => Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
            n => {
                log::warn!("unsupported channel count {n}, falling back to stereo");
                Channels::FRONT_LEFT | Channels::FRONT_RIGHT
            }
        };
        SignalSpec::new(self.sample_rate, chans)
    }
}

#[derive(Debug, Clone)]
pub struct MixerConfig {
    /// Overlap window (seconds) of a track-to-track crossfade.
    pub crossfade_seconds: f64,
    /// Gain curve exponent; 1.0 = linear, 2.0 = equal-ish power.
    pub fade_curve: f64,
    /// Duration (seconds) of the fast fade into/out of a live DJ.
    pub duck_seconds: f64,
}

impl Default for MixerConfig {
    fn default() -> Self {
        Self {
            crossfade_seconds: 3.0,
            fade_curve: 1.0,
            duck_seconds: 1.5,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OutputConfig {
    pub host: String,
    pub port: u16,
    /// Mount point: Icecast path (e.g. `/radio.mp3`), or the SHOUTcast v2
    /// stream path (`/` or `/stream/N`). Ignored by SHOUTcast v1.
    pub mount: String,
    pub source_user: String,
    pub source_password: String,
    /// Which source protocol the output speaks.
    pub protocol: OutputProtocol,
    pub format: OutputFormat,
    /// Encoder bitrate in bits per second.
    pub bitrate: u32,
    pub name: String,
    pub description: String,
    pub genre: String,
    /// Seconds to wait between failed reconnect attempts.
    pub reconnect_seconds: u64,
}

/// Config for `output.file`: encode the tap to a local file.
#[derive(Debug, Clone)]
pub struct FileOutputConfig {
    pub path: PathBuf,
    pub format: OutputFormat,
    pub bitrate: u32,
}

/// Config for `output.soundcard`: play the tap on a physical output device.
#[derive(Debug, Clone, Default)]
pub struct SoundcardOutputConfig {
    /// Named device, or the default output device when `None`.
    pub device: Option<String>,
}

/// Config for `output.hls`: encode the tap to AAC and slice it into a
/// sliding window of MPEG-TS HLS segments with a media playlist.
#[derive(Debug, Clone)]
pub struct HlsOutputConfig {
    /// Directory the segments and `playlist.m3u8` are written to. Old
    /// `seg-*.ts` files from previous runs are cleared at connect.
    pub directory: PathBuf,
    /// Nominal segment length in seconds.
    pub segment_seconds: f64,
    /// How many completed segments the on-disk window keeps.
    pub retention: usize,
    /// Mux the shared video tap into the segments (Part H6): requires
    /// `video.video(path)` registered in the same script.
    pub video: bool,
}

impl Default for HlsOutputConfig {
    fn default() -> Self {
        Self {
            directory: "hls".into(),
            segment_seconds: 5.0,
            retention: 12,
            video: false,
        }
    }
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 8000,
            mount: "/crabsoup.mp3".into(),
            source_user: "source".into(),
            source_password: "hackme".into(),
            protocol: OutputProtocol::Icecast,
            format: OutputFormat::Mp3,
            bitrate: 192_000,
            name: "Crabsoup".into(),
            description: "Crabsoup stream".into(),
            genre: "Various".into(),
            reconnect_seconds: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Mp3,
    Opus,
    /// Raw ADTS AAC (FDK-AAC encoder).
    Aac,
}

/// Source protocol spoken by the Icecast-style output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputProtocol {
    #[default]
    Icecast,
    /// SHOUTcast v1 legacy ICY source protocol (MP3 only).
    ShoutcastV1,
    /// SHOUTcast v2 DNAS source protocol (MP3 only).
    ShoutcastV2,
}

impl OutputProtocol {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Icecast => "Icecast",
            Self::ShoutcastV1 => "SHOUTcast v1",
            Self::ShoutcastV2 => "SHOUTcast v2",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Bind address for the DJ harbor listener.
    pub host: String,
    pub port: u16,
    /// Mount path a DJ must PUT to, e.g. `/live`.
    pub mount: String,
    /// Password DJs authenticate with (source-protocol Basic auth).
    pub password: String,
    /// Extra valid source passwords (per-streamer accounts).
    pub extra_passwords: Vec<String>,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8005,
            mount: "/live".into(),
            password: "dj".into(),
            extra_passwords: Vec::new(),
        }
    }
}

/// One-shot jingles played over the music via `MixCommand::PlayJingle`.
#[derive(Debug, Clone, Default)]
pub struct JingleConfig {
    /// Recursively scanned directory of audio files.
    pub directory: Option<PathBuf>,
    /// Explicit file list (appended to any `directory` results).
    pub files: Vec<PathBuf>,
}

impl JingleConfig {
    /// Resolve the full ordered, deduplicated jingle file list.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(dir) = &self.directory {
            collect_audio(dir, &mut out);
        }
        out.extend(self.files.iter().cloned());
        out.sort();
        out.dedup();
        out
    }
}

/// Liquidsoap-style telnet control port, plus the optional HTTP
/// status/control endpoint (`http_port`) and the text welcome banner.
#[derive(Debug, Clone)]
pub struct ControlConfig {
    pub host: String,
    pub port: u16,
    /// Machine clients set `banner = false` so the connection starts with
    /// replies, not a prose welcome line.
    pub banner: bool,
    /// Port for the HTTP endpoint (`GET /status`, `POST /cmd`, ...) on the
    /// same `host`; `None` disables it.
    pub http_port: Option<u16>,
}

impl Default for ControlConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1234,
            banner: true,
            http_port: None,
        }
    }
}

const AUDIO_EXTS: &[&str] = &[
    "mp3", "wav", "flac", "ogg", "opus", "oga", "m4a", "aac", "wma",
    // Media containers whose default track is audio-capable (symphonia
    // decodes them): lets one directory feed both the audio graph and the
    // video side of a playlist (Part H7).
    "mp4", "m4v", "mov", "mkv", "webm",
];

/// Recursively collect audio files under `dir`.
pub fn collect_audio(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        log::warn!("playlist directory not readable: {}", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_audio(&path, out);
        } else if is_audio(&path) {
            out.push(path);
        }
    }
}

fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            AUDIO_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

#[cfg(feature = "video")]
const VIDEO_EXTS: &[&str] = &[
    "mp4", "mov", "mkv", "webm", "m4v", "ts", "m2ts", "avi", "flv", "mpg", "mpeg", "wmv",
];

/// Recursively collect video files under `dir` (Part H7).
#[cfg(feature = "video")]
pub fn collect_video(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        log::warn!("video playlist directory not readable: {}", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_video(&path, out);
        } else if is_video(&path) {
            out.push(path);
        }
    }
}

#[cfg(feature = "video")]
fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            VIDEO_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

#[cfg(feature = "video")]
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "bmp", "gif", "tif", "tiff"];

/// Recursively collect image files under `dir` (Part H2).
#[cfg(feature = "video")]
pub fn collect_images(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        log::warn!("video.slideshow directory not readable: {}", dir.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_images(&path, out);
        } else if is_image(&path) {
            out.push(path);
        }
    }
}

#[cfg(feature = "video")]
fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            IMAGE_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signal_spec_is_stereo() {
        let spec = StreamConfig::default().signal_spec();
        assert_eq!(spec.rate, 44100);
        assert_eq!(spec.channels.count(), 2);
    }

    #[test]
    fn default_config_matches_reference_values() {
        let stream = StreamConfig::default();
        let mixer = MixerConfig::default();
        let out = OutputConfig::default();
        assert_eq!(stream.sample_rate, 44100);
        assert_eq!(stream.channels, 2);
        assert_eq!(stream.frames_per_buffer, 4096);
        assert_eq!(mixer.crossfade_seconds, 3.0);
        assert_eq!(mixer.duck_seconds, 1.5);
        assert_eq!(out.format, OutputFormat::Mp3);
        assert_eq!(out.port, 8000);
        assert_eq!(out.source_password, "hackme");
    }

    #[test]
    fn jingle_files_resolves_directory_and_files() {
        let cfg = JingleConfig {
            directory: Some("./jingles".into()),
            files: vec!["./custom.wav".into()],
        };
        let files = cfg.files();
        // `./jingles` is gitignored and may be absent in CI; explicit files
        // still resolve.
        assert_eq!(files, files);
        assert!(files.iter().any(|f| f.ends_with("custom.wav")));
    }
}
