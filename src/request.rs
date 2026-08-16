//! URI resolution for media requests: local paths play directly, `http://`
//! and `https://` URLs are downloaded to a temp file first
//! (download-then-play, per the Phase 7 roadmap — no streaming decode yet).
//!
//! The HTTP client is a minimal protocol-level `GET`: status line + headers,
//! `Content-Length` or chunked bodies (or connection-close delimited), and
//! redirect following. `http://` rides a plain `TcpStream`; `https://` wraps
//! the same socket in a `rustls` client (Part F1) and feeds the *identical*
//! byte stream through the same parsing — a wrap-the-transport change, not a
//! rewrite of the HTTP logic.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use log::warn;

use crate::source::cue_cut::CueCutSource;

/// Download settings, configurable through `set("request_timeout", ...)` /
/// `set("request_retries", ...)`.
#[derive(Clone, Copy, Debug)]
pub struct RequestConfig {
    /// Per-attempt connect/read timeout.
    pub timeout_secs: u64,
    /// Number of retries after a failed attempt.
    pub retries: u32,
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            retries: 2,
        }
    }
}

impl RequestConfig {
    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    fn backoff(&self) -> Duration {
        Duration::from_millis(500)
    }
}

/// Per-track cue points, from an `annotate:` URI prefix or the `cue_cut`
/// operator. `cue_in`/`cue_out` bound the audible window (absolute seconds
/// into the track); `fade_in`/`fade_out` override the global crossfade for
/// this track (Part D step 2 — parsed here, consumed by `CrossfadeMixer`
/// later).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TrackCues {
    /// Skip this many seconds into the track before audio starts.
    pub cue_in: f64,
    /// End the track this many seconds in (early exhaustion); `None` = play
    /// to the natural end.
    pub cue_out: Option<f64>,
    /// Per-track crossfade fade-in override in seconds, if set.
    pub fade_in: Option<f64>,
    /// Per-track crossfade fade-out override in seconds, if set.
    pub fade_out: Option<f64>,
    /// Per-track linear gain multiplier (the `amplify` annotation), if
    /// set. The annotation accepts a plain factor (`"0.7"`) or dB with the
    /// `dB` suffix (`"-8.2 dB"`); both land here as a linear multiplier.
    pub amplify: Option<f64>,
    /// Per-track `start_next` override in seconds, if set: how early the
    /// next track starts relative to this track's end.
    pub start_next: Option<f64>,
    /// Per-track `append` follower, if set: a request URI played after
    /// this track ends. The literal `"false"` inhibits the `annotated`
    /// operator's default append.
    pub append: Option<String>,
    /// Per-track `prepend` follower, if set: a request URI played before
    /// this track starts. The literal `"false"` inhibits the `annotated`
    /// operator's default prepend.
    pub prepend: Option<String>,
    /// On-air title override (the `title` annotation), if set. Overrides
    /// the label the source would otherwise derive from tags or filename.
    pub title: Option<String>,
}

/// A media item: a local file path or an HTTP(S) URL, with optional per-track
/// cue points from an `annotate:` prefix. URLs are downloaded to a temp file
/// when resolved.
#[derive(Clone, Debug)]
pub enum RequestUri {
    Local(PathBuf, Option<TrackCues>),
    Url(String, Option<TrackCues>),
}

impl RequestUri {
    pub fn new(uri: &str) -> Self {
        let (bare, cues) = parse_annotate(uri);
        if bare.starts_with("http://") || bare.starts_with("https://") {
            Self::Url(bare.to_string(), cues)
        } else {
            Self::Local(bare.into(), cues)
        }
    }

    /// The value as given (full path or full URL) for queue listings.
    pub fn raw(&self) -> String {
        match self {
            Self::Local(path, _) => path.display().to_string(),
            Self::Url(url, _) => url.clone(),
        }
    }

    /// Short display label for status/metadata (last path segment).
    pub fn display(&self) -> String {
        match self {
            Self::Local(path, _) => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            Self::Url(url, _) => url
                .split(['/', '?'])
                .next_back()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| url.to_string()),
        }
    }
}

impl PartialEq for RequestUri {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Local(a, ca), Self::Local(b, cb)) => a == b && ca == cb,
            (Self::Url(a, ca), Self::Url(b, cb)) => a == b && ca == cb,
            _ => false,
        }
    }
}

impl Eq for RequestUri {}

impl PartialOrd for RequestUri {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RequestUri {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Local(a, ca), Self::Local(b, cb)) => a.cmp(b).then_with(|| cmp_cues(ca, cb)),
            (Self::Url(a, ca), Self::Url(b, cb)) => a.cmp(b).then_with(|| cmp_cues(ca, cb)),
            (Self::Local(..), Self::Url(..)) => Ordering::Less,
            (Self::Url(..), Self::Local(..)) => Ordering::Greater,
        }
    }
}

fn cmp_cues(a: &Option<TrackCues>, b: &Option<TrackCues>) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => cmp_opt(&x.cue_out, &y.cue_out)
            .then_with(|| cmp_opt(&x.fade_in, &y.fade_in))
            .then_with(|| cmp_opt(&x.fade_out, &y.fade_out))
            .then_with(|| x.cue_in.total_cmp(&y.cue_in))
            .then_with(|| x.append.cmp(&y.append))
            .then_with(|| x.prepend.cmp(&y.prepend))
            .then_with(|| x.title.cmp(&y.title)),
    }
}

fn cmp_opt(a: &Option<f64>, b: &Option<f64>) -> std::cmp::Ordering {
    match (a, b) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.total_cmp(y),
    }
}

/// Parse the `annotate:` prefix: `key="value",key="value":<uri>`.
/// Recognized keys are `cue_in`, `cue_out`, `fade_in`,
/// `fade_out`, `start_next`, `amplify`; other keys are ignored (they carry
/// arbitrary metadata in Liquidsoap). Returns the bare URI and any cue
/// points. Malformed prefixes fall back to the whole string as a plain URI
/// with no cues.
fn parse_annotate(uri: &str) -> (String, Option<TrackCues>) {
    let Some(rest) = uri.strip_prefix("annotate:") else {
        return (uri.to_string(), None);
    };
    let mut cues = TrackCues::default();
    let mut found = false;
    let mut cursor = 0usize;
    let bytes = rest.as_bytes();
    loop {
        // Key: alphanumerics + underscore.
        let key_start = cursor;
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_alphanumeric() || bytes[cursor] == b'_')
        {
            cursor += 1;
        }
        if key_start == cursor {
            return (uri.to_string(), None); // no key: malformed
        }
        let key = &rest[key_start..cursor];
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            return (uri.to_string(), None);
        }
        cursor += 1;
        if cursor >= bytes.len() || bytes[cursor] != b'"' {
            return (uri.to_string(), None);
        }
        cursor += 1;
        let val_start = cursor;
        while cursor < bytes.len() && bytes[cursor] != b'"' {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return (uri.to_string(), None); // unterminated value
        }
        let value = &rest[val_start..cursor];
        cursor += 1; // closing quote
        // The title annotation is free-form text for on-air metadata;
        // parsed first so a numeric-looking title is not taken for a cue.
        if key == "title" {
            if !value.is_empty() {
                cues.title = Some(value.to_string());
                found = true;
            }
        // Only finite, normalized values become cues: `inf` would saturate
        // the sample skip to usize::MAX (silent endless skip) and NaN/-0.0
        // break the Eq/Ord consistency of `TrackCues` (total_cmp vs ==).
        } else if let Ok(v) = value.parse::<f64>()
            && v.is_finite()
        {
            let v = if v == 0.0 { 0.0 } else { v }; // -0.0 == 0.0, same in total_cmp
            match key {
                "cue_in" => {
                    cues.cue_in = v;
                    found = true;
                }
                "cue_out" => {
                    cues.cue_out = Some(v);
                    found = true;
                }
                "fade_in" => {
                    cues.fade_in = Some(v);
                    found = true;
                }
                "fade_out" => {
                    cues.fade_out = Some(v);
                    found = true;
                }
                "start_next" => {
                    cues.start_next = Some(v);
                    found = true;
                }
                "amplify" => {
                    cues.amplify = Some(v);
                    found = true;
                }
                // A follower named like a number still works: the value is
                // kept verbatim as a request URI.
                "append" => {
                    cues.append = Some(value.to_string());
                    found = true;
                }
                "prepend" => {
                    cues.prepend = Some(value.to_string());
                    found = true;
                }
                _ => {} // unknown annotate key: carried but ignored
            }
        } else if key == "append" || key == "prepend" {
            // The follower annotations take a request URI (or the literal
            // `"false"` to inhibit a default follower), not a number.
            if !value.is_empty() {
                if key == "append" {
                    cues.append = Some(value.to_string());
                } else {
                    cues.prepend = Some(value.to_string());
                }
                found = true;
            }
        } else if key == "amplify" {
            // The annotation also accepts decibels with the `dB` suffix
            // ("-8.2 dB" — spaces do not matter), which plain f64 parse
            // rejects. Convert to a linear multiplier.
            let trimmed = value.trim();
            let bare = trimmed
                .get(..trimmed.len().saturating_sub(2))
                .filter(|_| trimmed.ends_with("dB") || trimmed.ends_with("db"));
            if let Some(bare) = bare
                && let Ok(db) = bare.trim().parse::<f64>()
                && db.is_finite()
            {
                cues.amplify = Some(crate::engine::effects::db_to_gain(db as f32) as f64);
                found = true;
            }
        }
        // Separator: ',' continues metadata, ':' starts the URI.
        if cursor >= bytes.len() {
            return (uri.to_string(), None);
        }
        match bytes[cursor] {
            b',' => cursor += 1,
            b':' => {
                let bare = &rest[cursor + 1..];
                if bare.is_empty() {
                    return (uri.to_string(), None);
                }
                return (bare.to_string(), found.then_some(cues));
            }
            _ => return (uri.to_string(), None),
        }
    }
}

/// A [`FileSource`]-backed source that owns a downloaded temp file and
/// removes it when dropped (the temp-file lifecycle lives here so a playlist
/// loop that re-requests the same URL re-downloads it). The label stays the
/// caller's requested name rather than the temp filename.
pub struct DownloadSource {
    inner: Box<dyn crate::source::AudioSource>,
    tmp: Option<PathBuf>,
    name: String,
}

impl DownloadSource {
    pub fn new(inner: Box<dyn crate::source::AudioSource>, tmp: PathBuf, name: String) -> Self {
        Self {
            inner,
            tmp: Some(tmp),
            name,
        }
    }
}

impl Drop for DownloadSource {
    fn drop(&mut self) {
        if let Some(path) = &self.tmp {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl crate::source::AudioSource for DownloadSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        self.inner.next_buffer(buffer)
    }

    fn is_exhausted(&self) -> bool {
        self.inner.is_exhausted()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.inner.remaining_seconds()
    }

    fn label(&self) -> Option<String> {
        Some(self.name.clone())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.inner.replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<crate::source::CrossfadeOverrides> {
        self.inner.crossfade_overrides()
    }

    fn skip(&mut self) {
        self.inner.skip();
    }
}

/// Wrap a resolved source in [`CueCutSource`] when the request carries cue
/// points or fade overrides, in [`TrackGainSource`] when it carries an
/// `amplify` annotation, and in [`LabelOverrideSource`] when it carries a
/// `title` annotation; otherwise return it unchanged.
fn apply_cues(
    src: Box<dyn crate::source::AudioSource>,
    cues: Option<TrackCues>,
    target: symphonia::core::audio::SignalSpec,
) -> Box<dyn crate::source::AudioSource> {
    let title = cues.as_ref().and_then(|c| c.title.clone());
    let gain = cues.as_ref().and_then(|c| c.amplify);
    let mut out = match cues {
        Some(c)
            if c.cue_in > 0.0
                || c.cue_out.is_some()
                || c.fade_in.is_some()
                || c.fade_out.is_some()
                || c.start_next.is_some()
                || c.append.is_some()
                || c.prepend.is_some() =>
        {
            Box::new(CueCutSource::new(
                src,
                c,
                target.rate,
                target.channels.count(),
            ))
        }
        _ => src,
    };
    if let Some(title) = title {
        out = Box::new(LabelOverrideSource::new(out, title));
    }
    if let Some(g) = gain {
        out = Box::new(crate::source::amplify::TrackGainSource::new(out, g as f32));
    }
    out
}

/// Override a source's label with an `annotate:title` value. The wrapped
/// source keeps the audio path and per-track cues; only the on-air metadata
/// changes.
struct LabelOverrideSource {
    inner: Box<dyn crate::source::AudioSource>,
    label: String,
}

impl LabelOverrideSource {
    fn new(inner: Box<dyn crate::source::AudioSource>, label: String) -> Self {
        Self { inner, label }
    }
}

impl crate::source::AudioSource for LabelOverrideSource {
    fn next_buffer(&mut self, buffer: &mut [f32]) -> usize {
        self.inner.next_buffer(buffer)
    }

    fn is_exhausted(&self) -> bool {
        self.inner.is_exhausted()
    }

    fn remaining_seconds(&self) -> Option<f64> {
        self.inner.remaining_seconds()
    }

    fn label(&self) -> Option<String> {
        Some(self.label.clone())
    }

    fn replaygain_db(&self) -> Option<f32> {
        self.inner.replaygain_db()
    }

    fn crossfade_overrides(&self) -> Option<crate::source::CrossfadeOverrides> {
        self.inner.crossfade_overrides()
    }

    fn skip(&mut self) {
        self.inner.skip();
    }
}

/// Resolve a request to a playable source, downloading first if needed.
/// Failures (network, probe) surface as errors and the caller decides what
/// to play instead.
pub fn resolve(
    uri: &RequestUri,
    config: &RequestConfig,
    target: symphonia::core::audio::SignalSpec,
    frames_per_buffer: usize,
) -> crate::Result<Box<dyn crate::source::AudioSource>> {
    match uri {
        RequestUri::Local(path, cues) => {
            let src = open_audio(path, target, frames_per_buffer)?;
            Ok(apply_cues(src, cues.clone(), target))
        }
        RequestUri::Url(url, cues) => {
            let tmp = temp_path(url);
            download(url, &tmp, config, None)?;
            let src = open_audio(&tmp, target, frames_per_buffer)?;
            let src = Box::new(DownloadSource::new(src, tmp, uri.display()));
            Ok(apply_cues(src, cues.clone(), target))
        }
    }
}

/// Open a local file for playback: symphonia's [`FileSource`] first (MP3 /
/// Vorbis / AAC-in-ADTS), falling back to the native Opus path when the
/// probe fails (symphonia 0.5 has no Opus codec). The original probe error
/// is reported when neither can read the file.
pub fn open_audio(
    path: &std::path::Path,
    target: symphonia::core::audio::SignalSpec,
    frames_per_buffer: usize,
) -> crate::Result<Box<dyn crate::source::AudioSource>> {
    match crate::source::file::FileSource::open(path, target, frames_per_buffer) {
        Ok(src) => Ok(Box::new(src)),
        Err(symphonia_err) => {
            let label = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string());
            match crate::source::opus::OpusSource::open(
                Box::new(std::fs::File::open(path)?),
                target,
                frames_per_buffer,
                label,
            ) {
                Ok(src) => Ok(Box::new(src)),
                Err(_) => Err(symphonia_err),
            }
        }
    }
}

/// FNV-1a hash for a stable temp filename.
fn hash_url(url: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in url.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Temp-file destination for a URL download. Names are stable per URL plus a
/// per-process counter, so concurrent downloads of the same URL cannot
/// collide while replaying the same URL in a loop always overwrites.
fn temp_path(url: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir()
        .join("crabsoup-requests")
        .join(format!("{:016x}-{n}.part", hash_url(url)))
}

/// Download `url` to `dest`, retrying with a short backoff. `tls_roots`
/// overrides the default webpki roots (tests use a self-signed local server;
/// production callers pass `None`).
fn download(
    url: &str,
    dest: &Path,
    config: &RequestConfig,
    tls_roots: Option<Arc<rustls::RootCertStore>>,
) -> crate::Result<()> {
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut last_err: Option<String> = None;
    for attempt in 0..=config.retries {
        match http_get(url, dest, config.timeout(), tls_roots.as_ref()) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e.to_string());
                warn!(
                    "request: download of {url} failed (attempt {}/{})",
                    attempt + 1,
                    config.retries + 1
                );
                std::thread::sleep(config.backoff());
            }
        }
    }
    let _ = std::fs::remove_file(dest);
    Err(last_err.unwrap_or_else(|| "download failed".into()).into())
}

/// URL scheme: decides the default port and whether the transport is TLS.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

impl Scheme {
    fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }

    fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }
}

/// A parsed `http(s)://host[:port]/path` URL.
struct HttpUrl {
    scheme: Scheme,
    host: String,
    port: u16,
    path: String,
    /// Every address the host resolved to; connection attempts try each in
    /// order — a host can resolve `::1` before `127.0.0.1` and only one
    /// listener is up.
    addrs: Vec<std::net::SocketAddr>,
}

impl HttpUrl {
    fn parse(url: &str) -> crate::Result<Self> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (Scheme::Https, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (Scheme::Http, rest)
        } else {
            return Err(format!("not an http(s):// URL: {url}").into());
        };
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a, format!("/{p}")),
            None => (rest, "/".to_string()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => (
                h,
                p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?,
            ),
            _ => (authority, scheme.default_port()),
        };
        if host.is_empty() {
            return Err(format!("bad URL (empty host): {url}").into());
        }
        let addrs: Vec<std::net::SocketAddr> =
            std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
                .map_err(|e| format!("cannot resolve {host}: {e}"))?
                .collect();
        if addrs.is_empty() {
            return Err(format!("cannot resolve {host}").into());
        }
        Ok(Self {
            scheme,
            host: host.to_string(),
            port,
            path: path.to_string(),
            addrs,
        })
    }

    /// Join a (possibly relative) `Location` header against this URL,
    /// preserving the scheme (a redirect may cross `http` <-> `https`).
    fn join(&self, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            return location.to_string();
        }
        let scheme = self.scheme.as_str();
        if location.starts_with('/') {
            return format!("{scheme}://{}:{}{}", self.host, self.port, location);
        }
        // Relative to the current path's directory.
        let base = self.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("/");
        let base = if base.is_empty() { "/" } else { base };
        format!(
            "{scheme}://{}:{}{}/{}",
            self.host, self.port, base, location
        )
    }
}

/// One `GET` hop's byte stream: plain TCP or a rustls-wrapped TLS session.
/// Both expose the same `Read + Write`, so the status/header/body parsing
/// below never knows which transport carried the bytes.
enum Transport {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.read(buf),
            Transport::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Transport::Plain(s) => s.write(buf),
            Transport::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Transport::Plain(s) => s.flush(),
            Transport::Tls(s) => s.flush(),
        }
    }
}

/// The Mozilla webpki root store, built once. Kept in an `Arc` so a TLS
/// connect clones a cheap pointer instead of deep-copying ~150 roots.
fn default_root_store() -> &'static Arc<rustls::RootCertStore> {
    static STORE: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();
    STORE.get_or_init(|| {
        let mut store = rustls::RootCertStore::empty();
        store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(store)
    })
}

/// Connect to `target` and (for `https`) complete the TLS handshake.
/// `tls_roots` overrides the default trust store (tests only).
fn connect_transport(
    target: &HttpUrl,
    timeout: Duration,
    tls_roots: Option<&Arc<rustls::RootCertStore>>,
) -> crate::Result<Transport> {
    let mut last_err = None;
    let mut tcp = None;
    for addr in &target.addrs {
        match TcpStream::connect_timeout(addr, timeout) {
            Ok(s) => {
                tcp = Some(s);
                break;
            }
            Err(e) => last_err = Some(e),
        }
    }
    let Some(tcp) = tcp else {
        return Err(last_err.unwrap().into());
    };
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    match target.scheme {
        Scheme::Http => Ok(Transport::Plain(tcp)),
        Scheme::Https => {
            let config = rustls::ClientConfig::builder()
                .with_root_certificates(match tls_roots {
                    Some(store) => store.clone(),
                    None => default_root_store().clone(),
                })
                .with_no_client_auth();
            let server_name = rustls::pki_types::ServerName::try_from(target.host.clone())
                .map_err(|_| format!("invalid TLS hostname {}", target.host))?;
            let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
                .map_err(|e| format!("TLS handshake with {} failed: {e}", target.host))?;
            Ok(Transport::Tls(Box::new(rustls::StreamOwned::new(
                conn, tcp,
            ))))
        }
    }
}

/// Cheap shape validation of a relay URL (scheme + non-empty host), used by
/// `input.http` to fail fast at script evaluation. No DNS resolution — the
/// reconnect loop re-resolves per attempt.
pub fn validate_relay_url(url: &str) -> crate::Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(format!("not an http(s):// URL: {url}").into());
    }
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or(""))
        .unwrap_or("");
    if host.is_empty() {
        return Err(format!("bad URL (empty host): {url}").into());
    }
    Ok(())
}

/// One `GET` (no retries) writing the body to `dest`. Each redirect
/// hop re-opens the transport (the scheme may have changed), then runs the
/// same status/header/body parsing over it.
fn http_get(
    url: &str,
    dest: &Path,
    timeout: Duration,
    tls_roots: Option<&Arc<rustls::RootCertStore>>,
) -> crate::Result<()> {
    let mut target = HttpUrl::parse(url)?;
    for _ in 0..4 {
        let mut stream = connect_transport(&target, timeout, tls_roots)?;
        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: crabsoup/0.1\r\nConnection: close\r\nAccept: */*\r\n\r\n",
            target.path, target.host
        )?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        let mut status_words = status_line.split_whitespace();
        let _protocol = status_words.next();
        let code: u16 = status_words
            .next()
            .ok_or_else(|| format!("malformed status line: {status_line:?}"))?
            .parse()
            .map_err(|_| format!("malformed status line: {status_line:?}"))?;

        let mut chunked = false;
        let mut content_length: Option<u64> = None;
        let mut location: Option<String> = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.to_ascii_lowercase().as_str() {
                "transfer-encoding" if value.eq_ignore_ascii_case("chunked") => chunked = true,
                "content-length" => {
                    content_length = value.parse().ok();
                }
                "location" => location = Some(value.to_string()),
                _ => {}
            }
        }

        if (300..400).contains(&code) {
            let Some(loc) = location else {
                return Err(format!("redirect {code} without Location").into());
            };
            target = HttpUrl::parse(&target.join(&loc))?;
            continue;
        }
        if code != 200 {
            return Err(format!("HTTP {code} for {url}").into());
        }

        let mut file = File::create(dest)?;
        if chunked {
            read_chunked(&mut reader, &mut file, u64::MAX)?;
        } else if let Some(len) = content_length {
            std::io::copy(&mut reader.by_ref().take(len), &mut file)?;
        } else {
            std::io::copy(&mut reader, &mut file)?;
        }
        return Ok(());
    }
    Err("too many redirects".into())
}

/// One-shot `GET` returning the response body as a UTF-8 string (the Lua
/// `http_get` helper — Deezco-style track listings are small JSON).
/// Follows redirects exactly like [`http_get`]; bodies past
/// `HTTP_GET_STRING_CAP` bytes are rejected so a misbehaving endpoint
/// cannot balloon memory, and non-UTF-8 bodies fail the conversion.
pub fn http_get_string(
    url: &str,
    timeout: Duration,
    tls_roots: Option<&Arc<rustls::RootCertStore>>,
) -> crate::Result<String> {
    const HTTP_GET_STRING_CAP: usize = 16 * 1024 * 1024;
    let mut target = HttpUrl::parse(url)?;
    for _ in 0..4 {
        let mut stream = connect_transport(&target, timeout, tls_roots)?;
        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: crabsoup/0.1\r\nConnection: close\r\nAccept: */*\r\n\r\n",
            target.path, target.host
        )?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        let mut status_words = status_line.split_whitespace();
        let _protocol = status_words.next();
        let code: u16 = status_words
            .next()
            .ok_or_else(|| format!("malformed status line: {status_line:?}"))?
            .parse()
            .map_err(|_| format!("malformed status line: {status_line:?}"))?;

        let mut chunked = false;
        let mut content_length: Option<u64> = None;
        let mut location: Option<String> = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.to_ascii_lowercase().as_str() {
                "transfer-encoding" if value.eq_ignore_ascii_case("chunked") => chunked = true,
                "content-length" => content_length = value.parse().ok(),
                "location" => location = Some(value.to_string()),
                _ => {}
            }
        }

        if (300..400).contains(&code) {
            let Some(loc) = location else {
                return Err(format!("redirect {code} without Location").into());
            };
            target = HttpUrl::parse(&target.join(&loc))?;
            continue;
        }
        if code != 200 {
            return Err(format!("HTTP {code} for {url}").into());
        }

        let mut body = Vec::new();
        if chunked {
            read_chunked(&mut reader, &mut body, HTTP_GET_STRING_CAP as u64)?;
        } else if let Some(len) = content_length {
            if len as usize > HTTP_GET_STRING_CAP {
                return Err(format!("body of {len} bytes exceeds the 16 MiB cap").into());
            }
            body.reserve(len as usize);
            std::io::copy(&mut reader.by_ref().take(len), &mut body)?;
        } else {
            std::io::copy(&mut reader.by_ref().take(HTTP_GET_STRING_CAP as u64 + 1), &mut body)?;
            if body.len() > HTTP_GET_STRING_CAP {
                return Err("body exceeds the 16 MiB cap".into());
            }
        }
        return String::from_utf8(body).map_err(|e| format!("non-UTF-8 body: {e}").into());
    }
    Err("too many redirects".into())
}

/// A live HTTP response body: the status line and headers are already
/// parsed, and `Read` yields the body — content-length bounded, chunked, or
/// connection-close delimited (the Part G1 relay path, unlike
/// [`http_get`]'s download-then-play). Reading to EOF consumes the
/// connection; the caller decides when to reconnect.
pub struct HttpResponse {
    reader: BufReader<Transport>,
    /// Remaining body bytes from `Content-Length`, if declared.
    remaining: Option<u64>,
    /// Decoding a `Transfer-Encoding: chunked` body.
    chunked: bool,
    /// Bytes left in the current chunk.
    chunk_left: u64,
    /// The `Content-Type` header, if any (relays use it as a format hint).
    pub content_type: Option<String>,
}

impl Read for HttpResponse {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.chunked {
            if self.chunk_left == 0 {
                let mut size_line = String::new();
                if self.reader.read_line(&mut size_line)? == 0 {
                    return Ok(0);
                }
                // A chunk extension (`;ext=...`) may follow the size.
                let size_str = size_line.split(';').next().unwrap_or("").trim();
                let size = u64::from_str_radix(size_str, 16).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("bad chunk size {size_str:?}"),
                    )
                })?;
                if size == 0 {
                    // Trailer section; skip to the blank line.
                    loop {
                        let mut trailer = String::new();
                        if self.reader.read_line(&mut trailer)? == 0
                            || trailer.trim_end().is_empty()
                        {
                            break;
                        }
                    }
                    return Ok(0);
                }
                self.chunk_left = size;
            }
            let take = buf.len().min(self.chunk_left as usize);
            let n = self.reader.read(&mut buf[..take])?;
            self.chunk_left -= n as u64;
            if self.chunk_left == 0 {
                let mut crlf = [0u8; 2];
                let mut got = 0;
                while got < 2 {
                    let r = self.reader.read(&mut crlf[got..])?;
                    if r == 0 {
                        break;
                    }
                    got += r;
                }
            }
            Ok(n)
        } else if let Some(left) = self.remaining.as_mut() {
            if *left == 0 {
                return Ok(0);
            }
            let take = buf.len().min(*left as usize);
            let n = self.reader.read(&mut buf[..take])?;
            *left -= n as u64;
            Ok(n)
        } else {
            self.reader.read(buf)
        }
    }
}

/// One `GET` (no retries) returning the response as a live stream instead
/// of writing the body to a file — the relay/pull-source path. Redirects
/// are followed (up to 4, re-opening the transport per hop) and only the
/// final response is returned; `Icy-MetaData: 0` asks Icecast/DNAS not to
/// interleave in-stream metadata with the audio.
pub fn http_get_stream(
    url: &str,
    timeout: Duration,
    tls_roots: Option<&Arc<rustls::RootCertStore>>,
) -> crate::Result<HttpResponse> {
    let mut target = HttpUrl::parse(url)?;
    for _ in 0..4 {
        let mut stream = connect_transport(&target, timeout, tls_roots)?;
        write!(
            stream,
            "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: crabsoup/0.1\r\nConnection: close\r\nAccept: */*\r\nIcy-MetaData: 0\r\n\r\n",
            target.path, target.host
        )?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        let mut status_line = String::new();
        reader.read_line(&mut status_line)?;
        let mut status_words = status_line.split_whitespace();
        let _protocol = status_words.next();
        let code: u16 = status_words
            .next()
            .ok_or_else(|| format!("malformed status line: {status_line:?}"))?
            .parse()
            .map_err(|_| format!("malformed status line: {status_line:?}"))?;

        let mut chunked = false;
        let mut content_length: Option<u64> = None;
        let mut content_type: Option<String> = None;
        let mut location: Option<String> = None;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.to_ascii_lowercase().as_str() {
                "transfer-encoding" if value.eq_ignore_ascii_case("chunked") => chunked = true,
                "content-length" => {
                    content_length = value.parse().ok();
                }
                "content-type" => content_type = Some(value.to_string()),
                "location" => location = Some(value.to_string()),
                _ => {}
            }
        }

        if (300..400).contains(&code) {
            let Some(loc) = location else {
                return Err(format!("redirect {code} without Location").into());
            };
            target = HttpUrl::parse(&target.join(&loc))?;
            continue;
        }
        if code != 200 {
            return Err(format!("HTTP {code} for {url}").into());
        }
        return Ok(HttpResponse {
            reader,
            remaining: if chunked { None } else { content_length },
            chunked,
            chunk_left: 0,
            content_type,
        });
    }
    Err("too many redirects".into())
}

/// One-shot `POST` of a JSON body (the track-change webhook). Reuses the
/// same transport as `http_get`; no redirects are followed (a webhook
/// target is a fixed backend URL), and the response body is discarded
/// beyond its status code.
pub fn http_post_json(
    url: &str,
    body: &str,
    timeout: Duration,
    tls_roots: Option<&Arc<rustls::RootCertStore>>,
) -> crate::Result<()> {
    let target = HttpUrl::parse(url)?;
    let mut stream = connect_transport(&target, timeout, tls_roots)?;
    write!(
        stream,
        "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: crabsoup/0.1\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
        target.path,
        target.host,
        body.len(),
        body
    )?;
    stream.flush()?;

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let mut status_words = status_line.split_whitespace();
    let _protocol = status_words.next();
    let code: u16 = status_words
        .next()
        .ok_or_else(|| format!("malformed status line: {status_line:?}"))?
        .parse()
        .map_err(|_| format!("malformed status line: {status_line:?}"))?;
    if !(200..300).contains(&code) {
        return Err(format!("webhook POST to {url} returned HTTP {code}").into());
    }
    Ok(())
}

/// Decode a chunked body into `dest`.
fn read_chunked(
    reader: &mut BufReader<Transport>,
    dest: &mut impl Write,
    cap: u64,
) -> crate::Result<()> {
    let mut total = 0u64;
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line)? == 0 {
            return Err("truncated chunked body".into());
        }
        // A chunk extension (`;ext=...`) may follow the size.
        let size_str = size_line.split(';').next().unwrap_or("").trim();
        let size = u64::from_str_radix(size_str, 16)
            .map_err(|_| format!("bad chunk size {size_str:?}"))?;
        if size == 0 {
            // Trailer section; skip to the blank line.
            loop {
                let mut trailer = String::new();
                if reader.read_line(&mut trailer)? == 0 || trailer.trim_end().is_empty() {
                    break;
                }
            }
            return Ok(());
        }
        total += size;
        if total > cap {
            return Err(format!("body exceeds the {cap} byte cap").into());
        }
        std::io::copy(&mut reader.by_ref().take(size), dest)?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    /// Drain the request bytes (headers end at `\r\n\r\n`) so the response
    /// socket closes cleanly instead of RSTing. Works over either transport.
    fn drain_request(stream: &mut impl Read) {
        let mut buf = [0u8; 1024];
        let mut seen = 0usize;
        while seen < 4 {
            match stream.read(&mut buf[..1]) {
                Ok(0) | Err(_) => return,
                Ok(1) => {
                    seen = if buf[0] == b"\r\n\r\n"[seen] {
                        seen + 1
                    } else {
                        0
                    };
                }
                Ok(_) => unreachable!(),
            }
        }
    }

    /// A self-signed `localhost` cert/key pair for the TLS test servers.
    fn test_cert() -> (
        rustls::pki_types::CertificateDer<'static>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ) {
        let certified =
            rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).expect("rcgen cert");
        (
            certified.cert.der().clone(),
            rustls::pki_types::PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into()),
        )
    }

    /// Serve a canned HTTPS response on an ephemeral port with the given
    /// cert/key. Returns the `https://localhost:PORT/...` URL.
    fn serve_tls(
        status: &'static str,
        headers: Vec<(String, String)>,
        body: &'static [u8],
        cert: &rustls::pki_types::CertificateDer<'static>,
        key: &rustls::pki_types::PrivateKeyDer<'static>,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server_config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key.clone_key())
                .expect("server config"),
        );
        thread::spawn(move || {
            for _ in 0..8 {
                let Ok((tcp, _)) = listener.accept() else {
                    return;
                };
                let Ok(conn) = rustls::ServerConnection::new(server_config.clone()) else {
                    continue;
                };
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                drain_request(&mut tls);
                let mut response = format!("HTTP/1.1 {status}\r\n");
                for (k, v) in headers.iter() {
                    response.push_str(&format!("{k}: {v}\r\n"));
                }
                response.push_str("\r\n");
                let _ = tls.write_all(response.as_bytes());
                let _ = tls.write_all(body);
                let _ = tls.flush();
            }
        });
        format!("https://localhost:{}/test.mp3", addr.port())
    }

    /// Serve a canned HTTP response on an ephemeral port. Returns the URL.
    fn serve(status: &'static str, headers: Vec<(String, String)>, body: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            for _ in 0..8 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                drain_request(&mut stream);
                let mut response = format!("HTTP/1.1 {status}\r\n");
                for (k, v) in headers.iter() {
                    response.push_str(&format!("{k}: {v}\r\n"));
                }
                response.push_str("\r\n");
                stream.write_all(response.as_bytes()).expect("write head");
                stream.write_all(body).expect("write body");
                stream.flush().expect("flush");
            }
        });
        format!("http://{addr}/test.mp3")
    }

    #[test]
    fn http_get_string_reads_the_body_and_follows_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let final_url = format!("http://{addr}/playlists/1");
        thread::spawn(move || {
            let mut hops = 0usize;
            for _ in 0..8 {
                let (mut stream, _) = listener.accept().expect("accept");
                drain_request(&mut stream);
                hops += 1;
                if hops == 1 {
                    stream
                        .write_all(b"HTTP/1.1 302 Found\r\nLocation: /tracks.json\r\n\r\n")
                        .expect("redirect");
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 15\r\n\r\n{\"title\":\"A\"}\r\n",
                        )
                        .expect("final");
                }
                stream.flush().expect("flush");
            }
        });
        let body =
            http_get_string(&final_url, Duration::from_secs(5), None).expect("get succeeds");
        assert_eq!(body, "{\"title\":\"A\"}\r\n");
    }

    #[test]
    fn http_get_string_rejects_non_2xx_and_caps_the_body() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .expect("write");
            stream.flush().expect("flush");
        });
        let url = format!("http://{addr}/missing");
        let err = http_get_string(&url, Duration::from_secs(5), None).expect_err("get fails");
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[test]
    fn http_get_downloads_a_content_length_body() {
        let body = b"RIFFxxxx\x00WAVEtest-bytes";
        let url = serve(
            "200 OK",
            vec![("Content-Length".into(), body.len().to_string())],
            body,
        );
        let dest = std::env::temp_dir().join("crabsoup-test-dl.bin");
        let _ = std::fs::remove_file(&dest);
        http_get(&url, &dest, Duration::from_secs(5), None).expect("download");
        let got = std::fs::read(&dest).expect("read");
        assert_eq!(got, body);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn http_post_json_posts_the_body_and_accepts_2xx() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .expect("write");
            stream.flush().expect("flush");
        });
        let url = format!("http://{addr}/hook");
        http_post_json(&url, r#"{"title":"x"}"#, Duration::from_secs(5), None)
            .expect("post succeeds");
    }

    #[test]
    fn http_post_json_reports_non_2xx() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            drain_request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                .expect("write");
            stream.flush().expect("flush");
        });
        let url = format!("http://{addr}/hook");
        let err =
            http_post_json(&url, r#"{}"#, Duration::from_secs(5), None).expect_err("post fails");
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[test]
    fn http_get_reads_a_chunked_body() {
        let body = b"chunk-one";
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            for _ in 0..8 {
                let (mut stream, _) = listener.accept().expect("accept");
                drain_request(&mut stream);
                let hex = format!("{:x}\r\n", body.len());
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
                let _ = stream.write_all(hex.as_bytes());
                let _ = stream.write_all(body);
                let _ = stream.write_all(b"\r\n0\r\n\r\n");
                let _ = stream.flush();
            }
        });
        let url = format!("http://{addr}/test.mp3");
        let dest = std::env::temp_dir().join("crabsoup-test-chunked.bin");
        let _ = std::fs::remove_file(&dest);
        http_get(&url, &dest, Duration::from_secs(5), None).expect("download");
        let got = std::fs::read(&dest).expect("read");
        assert_eq!(got, body);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn http_get_follows_redirects() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        thread::spawn(move || {
            for _ in 0..8 {
                let (mut stream, _) = listener.accept().expect("accept");
                let mut req = Vec::new();
                let mut tmp = [0u8; 1024];
                loop {
                    match stream.read(&mut tmp) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            req.extend_from_slice(&tmp[..n]);
                            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                    }
                }
                let request = String::from_utf8_lossy(&req).to_string();
                if request.starts_with("GET /start.mp3 ") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\n\r\n",
                    );
                } else if request.starts_with("GET /target ") {
                    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\ntarget");
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n");
                }
                let _ = stream.flush();
            }
        });
        let url = format!("http://{addr}/start.mp3");
        let dest = std::env::temp_dir().join("crabsoup-test-redir.bin");
        let _ = std::fs::remove_file(&dest);
        http_get(&url, &dest, Duration::from_secs(5), None).expect("download");
        assert_eq!(std::fs::read(&dest).expect("read"), b"target");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn http_get_reports_http_errors() {
        let url = serve("404 Not Found", vec![], b"");
        let dest = std::env::temp_dir().join("crabsoup-test-404.bin");
        let _ = std::fs::remove_file(&dest);
        let err = http_get(&url, &dest, Duration::from_secs(5), None).expect_err("must fail");
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn request_uri_classifies_and_displays() {
        assert_eq!(
            RequestUri::new("internal/media/a.mp3"),
            RequestUri::Local("internal/media/a.mp3".into(), None)
        );
        assert_eq!(
            RequestUri::new("http://x.example/track.mp3"),
            RequestUri::Url("http://x.example/track.mp3".into(), None)
        );
        assert_eq!(
            RequestUri::new("https://x.example/track.mp3"),
            RequestUri::Url("https://x.example/track.mp3".into(), None)
        );
        assert_eq!(
            RequestUri::new("https://x.example/track.mp3").display(),
            "track.mp3"
        );
        assert_eq!(
            RequestUri::new("http://x.example/track.mp3").display(),
            "track.mp3"
        );
        assert_eq!(RequestUri::new("internal/media/a.mp3").display(), "a");
    }

    #[test]
    fn annotate_prefix_parses_cue_points_and_strips_to_the_uri() {
        let uri = RequestUri::new("annotate:cue_in=\"30\",cue_out=\"180\":internal/media/a.mp3");
        assert_eq!(
            uri,
            RequestUri::Local(
                "internal/media/a.mp3".into(),
                Some(TrackCues {
                    cue_in: 30.0,
                    cue_out: Some(180.0),
                    fade_in: None,
                    fade_out: None,
                    amplify: None,start_next: None,
append: None,
prepend: None,
title: None,
                })
            )
        );
        assert_eq!(uri.raw(), "internal/media/a.mp3");
        assert_eq!(uri.display(), "a");
    }

    #[test]
    fn annotate_prefix_handles_http_uris_and_ignores_unknown_keys() {
        let uri =
            RequestUri::new("annotate:genre=\"intro\",cue_in=\"5\":http://x.example/track.mp3");
        assert_eq!(
            uri,
            RequestUri::Url(
                "http://x.example/track.mp3".into(),
                Some(TrackCues {
                    cue_in: 5.0,
                    cue_out: None,
                    fade_in: None,
                    fade_out: None,
                    amplify: None,start_next: None,
append: None,
prepend: None,
title: None,
                })
            )
        );
    }

    #[test]
    fn annotate_title_parses_into_cues_even_when_numeric() {
        let uri = RequestUri::new("annotate:title=\"Lea\":http://x.example/track.mp3");
        assert_eq!(
            uri,
            RequestUri::Url(
                "http://x.example/track.mp3".into(),
                Some(TrackCues {
                    cue_in: 0.0,
                    cue_out: None,
                    fade_in: None,
                    fade_out: None,
                    amplify: None,start_next: None,
append: None,
prepend: None,
title: Some("Lea".to_string()),
                })
            )
        );
        let numeric = RequestUri::new("annotate:title=\"123\":internal/media/a.mp3");
        assert_eq!(
            numeric,
            RequestUri::Local(
                "internal/media/a.mp3".into(),
                Some(TrackCues {
                    cue_in: 0.0,
                    cue_out: None,
                    fade_in: None,
                    fade_out: None,
                    amplify: None,start_next: None,
append: None,
prepend: None,
title: Some("123".to_string()),
                })
            )
        );
    }

    #[test]
    fn fade_only_annotate_carries_the_overrides() {
        // No cue points, just fade overrides: parsed and reported for the
        // CrossfadeMixer (step 2).
        let uri = RequestUri::new("annotate:fade_in=\"2\",fade_out=\"3\":internal/media/a.mp3");
        assert_eq!(
            uri,
            RequestUri::Local(
                "internal/media/a.mp3".into(),
                Some(TrackCues {
                    cue_in: 0.0,
                    cue_out: None,
                    fade_in: Some(2.0),
                    fade_out: Some(3.0),
                    amplify: None,start_next: None,
append: None,
prepend: None,
title: None,
                })
            )
        );
    }

    #[test]
    fn amplify_annotation_parses_linear_and_db_values() {
        let linear = RequestUri::new("annotate:amplify=\"0.5\":internal/media/a.mp3");
        assert_eq!(
            linear,
            RequestUri::Local(
                "internal/media/a.mp3".into(),
                Some(TrackCues {
                    cue_in: 0.0,
                    cue_out: None,
                    fade_in: None,
                    fade_out: None,
                    amplify: Some(0.5),start_next: None,
append: None,
prepend: None,
title: None,
                })
            )
        );
        // dB values (spaces do not matter) land as linear multipliers.
        let db = RequestUri::new("annotate:amplify=\"-8.2 dB\":internal/media/a.mp3");
        let db_gain = match db {
            RequestUri::Local(_, Some(c)) => c.amplify.expect("dB gain parsed"),
            _ => panic!("dB annotation must yield cues"),
        };
        let expected = crate::engine::effects::db_to_gain(-8.2) as f64;
        assert!(
            (db_gain - expected).abs() < 1e-6,
            "expected {expected}, got {db_gain}"
        );
        // Unknown/ill-formed values are ignored like any other key.
        let bad = RequestUri::new("annotate:amplify=\"loud\":internal/media/a.mp3");
        assert_eq!(bad, RequestUri::Local("internal/media/a.mp3".into(), None));
    }

    #[test]
    fn start_next_annotation_parses_into_cues() {
        let uri = RequestUri::new("annotate:start_next=\"1\":internal/media/a.mp3");
        assert_eq!(
            uri,
            RequestUri::Local(
                "internal/media/a.mp3".into(),
                Some(TrackCues {
                    cue_in: 0.0,
                    cue_out: None,
                    fade_in: None,
                    fade_out: None,
                    amplify: None,
                    start_next: Some(1.0),
                    append: None,
                    prepend: None,
                    title: None,
                })
            )
        );
        // Non-finite values are rejected like every other cue.
        let bad = RequestUri::new("annotate:start_next=\"inf\":internal/media/a.mp3");
        assert_eq!(bad, RequestUri::Local("internal/media/a.mp3".into(), None));
    }

    #[test]
    fn append_and_prepend_annotations_parse_into_cues() {
        let uri = RequestUri::new(
            "annotate:append=\"internal/jingles/stinger.mp3\",prepend=\"false\":internal/media/a.mp3",
        );
        assert_eq!(
            uri,
            RequestUri::Local(
                "internal/media/a.mp3".into(),
                Some(TrackCues {
                    cue_in: 0.0,
                    cue_out: None,
                    fade_in: None,
                    fade_out: None,
                    amplify: None,
                    start_next: None,
                    append: Some("internal/jingles/stinger.mp3".into()),
                    prepend: Some("false".into()),
                    title: None,
                })
            )
        );
        // A follower named like a number is kept verbatim, not parsed.
        let RequestUri::Local(_, cues) = RequestUri::new("annotate:append=\"0.5\":internal/media/a.mp3")
        else {
            panic!("local uri expected")
        };
        assert_eq!(cues.as_ref().and_then(|c| c.append.as_deref()), Some("0.5"));
    }

    #[test]
    fn malformed_annotate_prefix_falls_back_to_a_plain_uri() {
        // No separating ':' after the metadata: treat as a plain path.
        let uri = RequestUri::new("annotate:cue_in=\"30\"");
        assert_eq!(
            uri,
            RequestUri::Local("annotate:cue_in=\"30\"".into(), None)
        );
        // Unknown keys only: no cues (metadata is not acted on).
        let uri = RequestUri::new("annotate:genre=\"x\":/path/a.mp3");
        assert_eq!(uri, RequestUri::Local("/path/a.mp3".into(), None));
    }

    #[test]
    fn non_finite_cue_values_are_ignored() {
        // `inf`/`NaN` would corrupt the sample-count math (and break the
        // Eq/Ord consistency of TrackCues), so they must not become cues.
        for bad in ["inf", "-inf", "NaN"] {
            let uri = RequestUri::new(&format!("annotate:cue_in=\"{bad}\":internal/media/a.mp3"));
            assert_eq!(
                uri,
                RequestUri::Local("internal/media/a.mp3".into(), None),
                "{bad} must not be accepted as a cue value"
            );
        }
        // A good value next to a bad one still applies.
        let uri = RequestUri::new("annotate:cue_in=\"inf\",cue_out=\"5\":internal/media/a.mp3");
        assert_eq!(
            uri,
            RequestUri::Local(
                "internal/media/a.mp3".into(),
                Some(TrackCues {
                    cue_in: 0.0,
                    cue_out: Some(5.0),
                    fade_in: None,
                    fade_out: None,
                    amplify: None,start_next: None,
append: None,
prepend: None,
title: None,
                })
            )
        );
    }

    #[test]
    fn temp_path_is_stable_per_url_and_unique_per_call() {
        let a = temp_path("http://x/a.mp3");
        let b = temp_path("http://x/a.mp3");
        assert_ne!(a, b, "counter must disambiguate same-URL downloads");
        assert!(a.to_str().unwrap().contains("crabsoup-requests"));
    }

    #[test]
    fn https_downloads_a_body_from_a_local_tls_server() {
        // Replaces the old "https is not supported" rejection: the same HTTP
        // parsing now runs over a rustls-wrapped transport, verified against
        // a local server with a self-signed cert (trusted via an injected
        // root store, so the test needs no live internet).
        let (cert, key) = test_cert();
        let body = b"TLS-encrypted-bytes";
        let url = serve_tls(
            "200 OK",
            vec![("Content-Length".into(), body.len().to_string())],
            body,
            &cert,
            &key,
        );
        let mut store = rustls::RootCertStore::empty();
        store.add(cert).expect("trust the self-signed cert");
        let dest = std::env::temp_dir().join("crabsoup-test-https.bin");
        let _ = std::fs::remove_file(&dest);
        http_get(&url, &dest, Duration::from_secs(5), Some(&Arc::new(store)))
            .expect("tls download");
        let got = std::fs::read(&dest).expect("read");
        assert_eq!(got, body);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn https_redirect_to_http_swaps_the_transport() {
        // A 302 from the TLS server pointing at a plain http server: the
        // redirect-follow loop must re-open the transport per hop instead of
        // assuming the scheme stays constant.
        let (cert, key) = test_cert();
        let body = b"redirected-target";
        let http_url = serve(
            "200 OK",
            vec![("Content-Length".into(), body.len().to_string())],
            body,
        );
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server_config = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert.clone()], key.clone_key())
                .expect("server config"),
        );
        thread::spawn(move || {
            for _ in 0..8 {
                let Ok((tcp, _)) = listener.accept() else {
                    return;
                };
                let Ok(conn) = rustls::ServerConnection::new(server_config.clone()) else {
                    continue;
                };
                let mut tls = rustls::StreamOwned::new(conn, tcp);
                drain_request(&mut tls);
                let _ = tls.write_all(
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {http_url}\r\nContent-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                );
                let _ = tls.flush();
            }
        });
        let mut store = rustls::RootCertStore::empty();
        store.add(cert).expect("trust the self-signed cert");
        let dest = std::env::temp_dir().join("crabsoup-test-https-redir.bin");
        let _ = std::fs::remove_file(&dest);
        let url = format!("https://localhost:{}/start.mp3", addr.port());
        http_get(&url, &dest, Duration::from_secs(5), Some(&Arc::new(store)))
            .expect("follow the redirect");
        assert_eq!(std::fs::read(&dest).expect("read"), body);
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn download_retries_then_fails() {
        // Point at a port with nothing listening: connect fails fast, and the
        // retry loop must still surface an error.
        let config = RequestConfig {
            timeout_secs: 1,
            retries: 1,
        };
        let dest = std::env::temp_dir().join("crabsoup-test-refused.bin");
        let _ = std::fs::remove_file(&dest);
        let err =
            download("http://127.0.0.1:1/x.mp3", &dest, &config, None).expect_err("must fail");
        assert!(!err.to_string().is_empty());
    }

    fn opus_test_bytes(seconds: f64) -> Vec<u8> {
        use crate::output::encoder::{Encoder, OpusEncoder};
        let mut enc = OpusEncoder::new(44_100, 2, 128_000, "test").unwrap();
        let frames = (seconds * 44_100.0) as usize;
        let mut out = Vec::new();
        let mut pcm = Vec::with_capacity(1024);
        for f in 0..frames {
            let v = (f as f64 * 2.0 * std::f64::consts::PI * 440.0 / 44_100.0).sin() as f32 * 0.5;
            pcm.extend_from_slice(&[v, v]);
            if pcm.len() >= 1024 {
                out.extend_from_slice(&enc.encode(&pcm));
                pcm.clear();
            }
        }
        if !pcm.is_empty() {
            out.extend_from_slice(&enc.encode(&pcm));
        }
        out.extend_from_slice(&enc.finish());
        out
    }

    #[test]
    fn open_audio_plays_an_opus_file() {
        let dest = std::env::temp_dir().join("crabsoup-test-opus.opus");
        std::fs::write(&dest, opus_test_bytes(1.0)).expect("write");
        let spec = symphonia::core::audio::SignalSpec::new(
            44_100,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let mut src = open_audio(&dest, spec, 4096).expect("open opus file");
        let _ = std::fs::remove_file(&dest);
        assert_eq!(src.label().as_deref(), Some("crabsoup-test-opus"));
        let mut buf = vec![0f32; 4096 * 2];
        let mut total = 0usize;
        let mut energy = 0f64;
        loop {
            let n = src.next_buffer(&mut buf);
            if n == 0 {
                break;
            }
            total += n;
            for &s in &buf[..n] {
                energy += (s as f64).powi(2);
            }
        }
        assert!(total > 80_000, "total={total}");
        assert!(energy / total as f64 > 0.01, "energy={energy}");
        assert!(src.is_exhausted());
    }

    #[test]
    fn open_audio_reports_the_original_error_for_undecodable_files() {
        let dest = std::env::temp_dir().join("crabsoup-test-garbage.mp3");
        std::fs::write(&dest, b"not audio at all").expect("write");
        let spec = symphonia::core::audio::SignalSpec::new(
            44_100,
            symphonia::core::audio::Channels::FRONT_LEFT
                | symphonia::core::audio::Channels::FRONT_RIGHT,
        );
        let err = match open_audio(&dest, spec, 4096) {
            Ok(_) => panic!("must fail"),
            Err(e) => e,
        };
        let _ = std::fs::remove_file(&dest);
        assert!(!err.to_string().is_empty());
    }
}
