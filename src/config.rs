use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Root configuration, mirroring `crabsoup.yaml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Audio pipeline parameters (the PCM bus shared by every source).
    pub stream: StreamConfig,
    /// Crossfade and ducking behaviour.
    pub mixer: MixerConfig,
    /// Local media playback.
    pub playlist: PlaylistConfig,
    /// Icecast encoder / broadcaster (optional).
    pub output: Option<OutputConfig>,
    /// Live DJ harbor listener (optional).
    pub live: Option<LiveConfig>,
}

/// The PCM bus: every source is resampled/converted to this spec.
#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PlaylistConfig {
    /// Recursively scanned directory of audio files.
    pub directory: Option<PathBuf>,
    /// Explicit file list (appended to any `directory` results).
    pub files: Vec<PathBuf>,
    /// Restart the playlist from the top when exhausted.
    pub loop_playlist: bool,
    /// Randomise the playback order once at startup.
    pub shuffle: bool,
}

impl Default for PlaylistConfig {
    fn default() -> Self {
        Self {
            directory: None,
            files: Vec::new(),
            loop_playlist: true,
            shuffle: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    pub host: String,
    pub port: u16,
    /// Icecast mount point, e.g. `/radio.mp3`.
    pub mount: String,
    pub source_user: String,
    pub source_password: String,
    pub format: OutputFormat,
    /// Encoder bitrate in bits per second.
    pub bitrate: u32,
    pub name: String,
    pub description: String,
    pub genre: String,
    /// Seconds to wait between failed reconnect attempts.
    pub reconnect_seconds: u64,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 8000,
            mount: "/crabsoup.mp3".into(),
            source_user: "source".into(),
            source_password: "hackme".into(),
            format: OutputFormat::Mp3,
            bitrate: 192_000,
            name: "Crabsoup".into(),
            description: "Crabsoup stream".into(),
            genre: "Various".into(),
            reconnect_seconds: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    #[default]
    Mp3,
    Opus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiveConfig {
    /// Bind address for the DJ harbor listener.
    pub host: String,
    pub port: u16,
    /// Mount path a DJ must PUT to, e.g. `/live`.
    pub mount: String,
    /// Password DJs authenticate with (source-protocol Basic auth).
    pub password: String,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8005,
            mount: "/live".into(),
            password: "dj".into(),
        }
    }
}

impl Config {
    /// Load and parse a YAML config file.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read config {}: {e}", path.display()))?;
        Self::from_yaml(&raw)
    }

    /// Parse YAML, applying defaults for any missing sections.
    pub fn from_yaml(raw: &str) -> Result<Self> {
        serde_yaml::from_str(raw).map_err(|e| format!("invalid config: {e}").into())
    }

    /// Resolve the full ordered list of media files to play.
    pub fn media_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(dir) = &self.playlist.directory {
            collect_audio(dir, &mut out);
        }
        out.extend(self.playlist.files.iter().cloned());
        out.sort();
        out.dedup();
        out
    }

    /// The `SignalSpec` every source is normalised to.
    pub fn signal_spec(&self) -> symphonia::core::audio::SignalSpec {
        use symphonia::core::audio::Channels;
        let chans = match self.stream.channels {
            1 => Channels::FRONT_CENTRE,
            2 => Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
            n => {
                log::warn!("unsupported channel count {n}, falling back to stereo");
                Channels::FRONT_LEFT | Channels::FRONT_RIGHT
            }
        };
        symphonia::core::audio::SignalSpec::new(self.stream.sample_rate, chans)
    }
}

const AUDIO_EXTS: &[&str] = &["mp3", "wav", "flac", "ogg", "opus", "oga", "m4a", "aac", "wma"];

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config_with_defaults() {
        let raw = r#"
playlist:
  directory: ./media
"#;
        let cfg = Config::from_yaml(raw).unwrap();
        assert_eq!(cfg.stream.sample_rate, 44100);
        assert_eq!(cfg.stream.channels, 2);
        assert!(cfg.playlist.loop_playlist);
        assert!(cfg.output.is_none());
        assert!(cfg.live.is_none());
    }

    #[test]
    fn parses_output_format() {
        let raw = r#"
output:
  host: icecast.example.com
  port: 8000
  mount: /radio.opus
  source_password: secret
  format: opus
  bitrate: 128000
"#;
        let cfg = Config::from_yaml(raw).unwrap();
        let out = cfg.output.unwrap();
        assert_eq!(out.format, OutputFormat::Opus);
        assert_eq!(out.bitrate, 128_000);
    }

    #[test]
    fn signal_spec_is_stereo() {
        let cfg = Config::default();
        let spec = cfg.signal_spec();
        assert_eq!(spec.rate, 44100);
        assert_eq!(spec.channels.count(), 2);
    }
}
