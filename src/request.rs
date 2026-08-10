//! URI resolution for media requests: local paths play directly, `http://`
//! URLs are downloaded to a temp file first (download-then-play, per the
//! Phase 7 roadmap — no streaming decode yet).
//!
//! The HTTP client is a minimal protocol-level `GET` on `std::net` (no TLS,
//! no external crates): status line + headers, `Content-Length` or
//! chunked bodies (or connection-close delimited), and redirect following.

use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use log::warn;

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

/// A media item: a local file path or an HTTP(S) URL. URLs are downloaded to
/// a temp file when resolved.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RequestUri {
    Local(PathBuf),
    Url(String),
}

impl RequestUri {
    pub fn new(uri: &str) -> Self {
        if uri.starts_with("http://") || uri.starts_with("https://") {
            Self::Url(uri.to_string())
        } else {
            Self::Local(uri.into())
        }
    }

    /// The value as given (full path or full URL) for queue listings.
    pub fn raw(&self) -> String {
        match self {
            Self::Local(path) => path.display().to_string(),
            Self::Url(url) => url.clone(),
        }
    }

    /// Short display label for status/metadata (last path segment).
    pub fn display(&self) -> String {
        match self {
            Self::Local(path) => path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string()),
            Self::Url(url) => url
                .split(['/', '?'])
                .next_back()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| url.to_string()),
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
        RequestUri::Local(path) => {
            let src = crate::source::file::FileSource::open(path, target, frames_per_buffer)?;
            Ok(Box::new(src))
        }
        RequestUri::Url(url) => {
            let tmp = temp_path(url);
            download(url, &tmp, config)?;
            let src = crate::source::file::FileSource::open(&tmp, target, frames_per_buffer)?;
            Ok(Box::new(DownloadSource::new(
                Box::new(src),
                tmp,
                uri.display(),
            )))
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

/// Download `url` to `dest`, retrying with a short backoff.
fn download(url: &str, dest: &Path, config: &RequestConfig) -> crate::Result<()> {
    if url.starts_with("https://") {
        return Err("https is not supported yet (use a plain http:// URL)".into());
    }
    if let Some(dir) = dest.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut last_err: Option<String> = None;
    for attempt in 0..=config.retries {
        match http_get(url, dest, config.timeout()) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e.to_string());
                warn!("request: download of {url} failed (attempt {}/{})", attempt + 1, config.retries + 1);
                std::thread::sleep(config.backoff());
            }
        }
    }
    let _ = std::fs::remove_file(dest);
    Err(last_err.unwrap_or_else(|| "download failed".into()).into())
}

/// A parsed `http://host[:port]/path` URL.
struct HttpUrl {
    host: String,
    port: u16,
    path: String,
    addr: SocketAddr,
}

impl HttpUrl {
    fn parse(url: &str) -> crate::Result<Self> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("not an http:// URL: {url}"))?;
        let (authority, path) = match rest.split_once('/') {
            Some((a, p)) => (a, format!("/{p}")),
            None => (rest, "/".to_string()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) => {
                (h, p.parse::<u16>().map_err(|_| format!("bad port in {url}"))?)
            }
            _ => (authority, 80),
        };
        if host.is_empty() {
            return Err(format!("bad URL (empty host): {url}").into());
        }
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&(host, port))
            .map_err(|e| format!("cannot resolve {host}: {e}"))?
            .next()
            .ok_or_else(|| format!("cannot resolve {host}"))?;
        Ok(Self { host: host.to_string(), port, path: path.to_string(), addr })
    }

    /// Join a (possibly relative) `Location` header against this URL.
    fn join(&self, location: &str) -> String {
        if location.starts_with("http://") || location.starts_with("https://") {
            return location.to_string();
        }
        if location.starts_with('/') {
            return format!("http://{}:{}{}", self.host, self.port, location);
        }
        // Relative to the current path's directory.
        let base = self.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("/");
        let base = if base.is_empty() { "/" } else { base };
        format!("http://{}:{}{}/{}", self.host, self.port, base, location)
    }
}

/// Perform one `GET` (no retries) writing the body to `dest`.
fn http_get(url: &str, dest: &Path, timeout: Duration) -> crate::Result<()> {
    let mut target = HttpUrl::parse(url)?;
    for _ in 0..4 {
        let mut stream = TcpStream::connect_timeout(&target.addr, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
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
            read_chunked(&mut reader, &mut file)?;
        } else if let Some(len) = content_length {
            std::io::copy(&mut reader.by_ref().take(len), &mut file)?;
        } else {
            std::io::copy(&mut reader, &mut file)?;
        }
        return Ok(());
    }
    Err("too many redirects".into())
}

/// Decode a chunked body into `file`.
fn read_chunked(reader: &mut BufReader<TcpStream>, file: &mut File) -> crate::Result<()> {
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
        std::io::copy(&mut reader.by_ref().take(size), file)?;
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// Drain the request bytes (headers end at `\r\n\r\n`) so the response
    /// socket closes cleanly instead of RSTing.
    fn drain_request(stream: &mut TcpStream) {
        let mut buf = [0u8; 1024];
        let mut seen = 0usize;
        while seen < 4 {
            match stream.read(&mut buf[..1]) {
                Ok(0) | Err(_) => return,
                Ok(1) => {
                    seen = if buf[0] == b"\r\n\r\n"[seen] { seen + 1 } else { 0 };
                }
                Ok(_) => unreachable!(),
            }
        }
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
    fn http_get_downloads_a_content_length_body() {
        let body = b"RIFFxxxx\x00WAVEtest-bytes";
        let url = serve("200 OK", vec![("Content-Length".into(), body.len().to_string())], body);
        let dest = std::env::temp_dir().join("crabsoup-test-dl.bin");
        let _ = std::fs::remove_file(&dest);
        http_get(&url, &dest, Duration::from_secs(5)).expect("download");
        let got = std::fs::read(&dest).expect("read");
        assert_eq!(got, body);
        let _ = std::fs::remove_file(&dest);
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
        http_get(&url, &dest, Duration::from_secs(5)).expect("download");
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
                    let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: /target\r\nContent-Length: 0\r\n\r\n");
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
        http_get(&url, &dest, Duration::from_secs(5)).expect("download");
        assert_eq!(std::fs::read(&dest).expect("read"), b"target");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn http_get_reports_http_errors() {
        let url = serve("404 Not Found", vec![], b"");
        let dest = std::env::temp_dir().join("crabsoup-test-404.bin");
        let _ = std::fs::remove_file(&dest);
        let err = http_get(&url, &dest, Duration::from_secs(5)).expect_err("must fail");
        assert!(err.to_string().contains("404"), "{err}");
    }

    #[test]
    fn request_uri_classifies_and_displays() {
        assert_eq!(
            RequestUri::new("media/a.mp3"),
            RequestUri::Local("media/a.mp3".into())
        );
        assert_eq!(
            RequestUri::new("http://x.example/track.mp3"),
            RequestUri::Url("http://x.example/track.mp3".into())
        );
        assert_eq!(
            RequestUri::new("http://x.example/track.mp3").display(),
            "track.mp3"
        );
        assert_eq!(RequestUri::new("media/a.mp3").display(), "a");
    }

    #[test]
    fn temp_path_is_stable_per_url_and_unique_per_call() {
        let a = temp_path("http://x/a.mp3");
        let b = temp_path("http://x/a.mp3");
        assert_ne!(a, b, "counter must disambiguate same-URL downloads");
        assert!(a.to_str().unwrap().contains("crabsoup-requests"));
    }

    #[test]
    fn https_is_rejected_with_a_clear_message() {
        let config = RequestConfig::default();
        let dest = std::env::temp_dir().join("crabsoup-test-https.bin");
        let _ = std::fs::remove_file(&dest);
        let err = download("https://example.com/a.mp3", &dest, &config).expect_err("must fail");
        assert!(err.to_string().contains("https is not supported"), "{err}");
    }

    #[test]
    fn download_retries_then_fails() {
        // Point at a port with nothing listening: connect fails fast, and the
        // retry loop must still surface an error.
        let config = RequestConfig { timeout_secs: 1, retries: 1 };
        let dest = std::env::temp_dir().join("crabsoup-test-refused.bin");
        let _ = std::fs::remove_file(&dest);
        let err = download("http://127.0.0.1:1/x.mp3", &dest, &config).expect_err("must fail");
        assert!(!err.to_string().is_empty());
    }
}