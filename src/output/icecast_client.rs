//! Native Icecast / SHOUTcast source-protocol client.
//!
//! Replaces libshout: for Icecast a single authenticated `SOURCE` request (no
//! capability probes, no `!POKE`, no 401-then-200) plus a plain
//! `/admin/metadata` GET for titles. SHOUTcast (v1 and v2) speaks the legacy
//! ICY source protocol — the password as the first line plus `icy-*` headers
//! — because the DNAS v2 accepts ICY sources on both its source ports and the
//! native "uvox2" handshake is undocumented (and encrypted). Titles go out as
//! `/admin.cgi?mode=updinfo` GETs with the source password, exactly like
//! libshout's ICY protocol. The pump loop in `icecast.rs` provides real-time
//! pacing, so this client is a dumb transport.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine as _;

use crate::Result;
use crate::config::{OutputConfig, OutputFormat, OutputProtocol};

const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// SHOUTcast v1 answers with a bare `OK2` line and then waits for audio, so
/// the response head is bounded by a short read timeout instead of the blank
/// line that terminates an HTTP head.
const HEAD_TIMEOUT: Duration = Duration::from_secs(3);

/// An established source connection to Icecast or SHOUTcast.
#[derive(Debug)]
pub struct IcecastClient {
    stream: TcpStream,
}

impl IcecastClient {
    /// Establish a source connection using the configured protocol.
    pub fn connect(config: &OutputConfig, sample_rate: u32, channels: u16) -> Result<Self> {
        match config.protocol {
            OutputProtocol::Icecast => Self::connect_icecast(config, sample_rate, channels),
            OutputProtocol::ShoutcastV1 => Self::connect_shoutcast(config, sample_rate, false),
            OutputProtocol::ShoutcastV2 => Self::connect_shoutcast(config, sample_rate, true),
        }
    }

    /// Open the mount with a single authenticated `SOURCE` request.
    fn connect_icecast(config: &OutputConfig, sample_rate: u32, channels: u16) -> Result<Self> {
        let mut stream = TcpStream::connect((config.host.as_str(), config.port))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;

        let content_type = match config.format {
            OutputFormat::Mp3 => "audio/mpeg",
            OutputFormat::Opus => "audio/ogg",
            OutputFormat::Aac => "audio/aac",
        };
        let request = format!(
            "SOURCE {} HTTP/1.0\r\n\
             Authorization: Basic {}\r\n\
             User-Agent: Crabsoup/0.1\r\n\
             Content-Type: {}\r\n\
             ice-name: {}\r\n\
             ice-description: {}\r\n\
             ice-genre: {}\r\n\
             ice-audio-info: samplerate={sample_rate};channels={channels};bitrate={}\r\n\
             ice-public: 0\r\n\
             \r\n",
            config.mount,
            basic_auth(&config.source_user, &config.source_password),
            content_type,
            config.name,
            config.description,
            config.genre,
            config.bitrate / 1000,
        );
        stream.write_all(request.as_bytes())?;

        let (status, message, _) = read_response_head(&mut stream)?;
        if !(200..300).contains(&status) {
            return Err(format!(
                "Icecast rejected source on {}: HTTP {status} {message}",
                config.mount
            )
            .into());
        }
        Ok(Self { stream })
    }

    /// SHOUTcast legacy ICY handshake: the password is the first line,
    /// followed by `icy-*` headers with LF line endings. The DNAS replies
    /// with a bare `OK2` line (often followed by `icy-caps:`) or an
    /// HTTP-style head. When `v2` is set, a mount of `/stream/N` selects that
    /// stream by appending `:#N` to the password (the DNAS's documented way
    /// for ICY sources to target a stream on a v2 DNAS).
    fn connect_shoutcast(config: &OutputConfig, sample_rate: u32, v2: bool) -> Result<Self> {
        require_shoutcast_format(config, !v2)?;
        let mut password = config.source_password.clone();
        if v2 && let Some(sid) = stream_id_from_mount(&config.mount) {
            password.push_str(&format!(":#{sid}"));
        }
        let mut stream = TcpStream::connect((config.host.as_str(), config.port))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;

        let request = format!(
            "{}\n\
             icy-name: {}\n\
             icy-pub: 0\n\
             icy-genre: {}\n\
             icy-br: {}\n\
             icy-sr: {sample_rate}\n\
             \n",
            password,
            config.name,
            config.genre,
            config.bitrate / 1000,
        );
        stream.write_all(request.as_bytes())?;

        let head = read_icy_head(&mut stream)?;
        let text = String::from_utf8_lossy(&head);
        let first_line = text.lines().next().unwrap_or_default();
        if !first_line.contains("OK") {
            return Err(format!("SHOUTcast rejected the source: {first_line}").into());
        }
        Ok(Self { stream })
    }

    /// Send encoded stream bytes.
    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        self.stream.write_all(data)?;
        Ok(())
    }

    /// Update the "now playing" title via `/admin/metadata` on a fresh
    /// connection (the source connection must stay clean). Icecast replies
    /// HTTP 200 even when it rejects the update, so the response body is
    /// checked for the rejection message.
    pub fn update_title(config: &OutputConfig, title: &str) -> Result<()> {
        let params = format!(
            "mode=updinfo&charset=UTF-8&mount={}&song={}",
            percent_encode(config.mount.as_bytes()),
            percent_encode(title.as_bytes())
        );
        let request = format!(
            "GET /admin/metadata?{params} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             User-Agent: Crabsoup/0.1\r\n\
             Authorization: Basic {}\r\n\
             \r\n",
            config.host,
            config.port,
            basic_auth(&config.source_user, &config.source_password),
        );
        let mut stream = TcpStream::connect((config.host.as_str(), config.port))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.write_all(request.as_bytes())?;
        let (status, _, body) = read_response_head(&mut stream)?;
        if !(200..300).contains(&status) {
            return Err(format!("Icecast metadata update failed: HTTP {status}").into());
        }
        let text = String::from_utf8_lossy(&body);
        if text.contains("will not accept") {
            return Err(format!("Icecast refused the title update: {}", text.trim()).into());
        }
        Ok(())
    }

    /// Update the "now playing" title via SHOUTcast's `/admin.cgi` endpoint
    /// (the `mode=updinfo` method ICY sources use; the source password rides
    /// in the query string). Verified against DNAS 2.6.1.
    pub fn update_icy_title(config: &OutputConfig, title: &str) -> Result<()> {
        let params = format!(
            "mode=updinfo&pass={}&song={}",
            percent_encode(config.source_password.as_bytes()),
            percent_encode(title.as_bytes())
        );
        let request = format!(
            "GET /admin.cgi?{params} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             User-Agent: Crabsoup/0.1\r\n\
             \r\n",
            config.host, config.port,
        );
        let mut stream = TcpStream::connect((config.host.as_str(), config.port))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        stream.write_all(request.as_bytes())?;
        let (status, message, _) = read_response_head(&mut stream)?;
        if !(200..300).contains(&status) {
            return Err(
                format!("SHOUTcast metadata update failed: HTTP {status} {message}").into(),
            );
        }
        Ok(())
    }
}

/// Reject formats the SHOUTcast version cannot carry before connecting.
/// v1 is MP3-only; v2 adds AAC ("AAC+", HE-AAC).
fn require_shoutcast_format(config: &OutputConfig, v1: bool) -> Result<()> {
    let allowed = matches!(
        (v1, config.format),
        (true, OutputFormat::Mp3) | (false, OutputFormat::Mp3 | OutputFormat::Aac)
    );
    if !allowed {
        let hint = if v1 {
            "SHOUTcast v1 only supports MP3; set format = \"mp3\""
        } else {
            "SHOUTcast v2 supports MP3 or AAC; set format = \"mp3\" or \"aac\""
        };
        return Err(format!("{hint} (got {:?})", config.format).into());
    }
    Ok(())
}

/// Stream id implied by a v2 mount path of `/stream/N` (the DNAS's URL for
/// named streams); `None` for the default stream.
fn stream_id_from_mount(mount: &str) -> Option<u32> {
    mount.strip_prefix("/stream/")?.parse().ok()
}

/// Read a SHOUTcast response head. The DNAS replies with either an HTTP-style
/// head (terminated by a blank line) or a bare `OK2` line followed by silence
/// while it waits for audio, so the read is bounded by a short timeout
/// instead of a terminator.
fn read_icy_head(stream: &mut TcpStream) -> Result<Vec<u8>> {
    stream.set_read_timeout(Some(HEAD_TIMEOUT))?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if head.len() >= 65536 || head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
        match stream.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => head.push(byte[0]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break;
            }
            Err(e) => return Err(e.into()),
        }
    }
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    Ok(head)
}

fn basic_auth(user: &str, password: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
}

/// Read the response head (up to `\r\n\r\n`), then drain the body promised by
/// `Content-Length`. Returns `(status, reason phrase, body)`.
fn read_response_head(stream: &mut TcpStream) -> Result<(u16, String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") && buf.len() < 65536 {
        if stream.read(&mut byte)? == 0 {
            break;
        }
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf);
    let status_line = head.lines().next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let _version = parts.next();
    let status: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let message = parts.next().unwrap_or_default().trim().to_string();

    let mut body = Vec::new();
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("content-length")
            && let Ok(len) = v.trim().parse::<usize>()
            && len > 0
        {
            let mut out = vec![0u8; len];
            let _ = stream.read_exact(&mut out);
            body = out;
        }
    }
    Ok((status, message, body))
}

/// RFC 3986 percent-encoding (unreserved chars pass through, everything else
/// becomes uppercase `%XX`).
fn percent_encode(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(input.len());
    for &b in input {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0xf) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{Shutdown, TcpListener};
    use std::thread;

    /// Accept one connection, capture the request head, reply, then read the
    /// request body until the client closes.
    struct FakeIcecast {
        listener: TcpListener,
    }

    impl FakeIcecast {
        fn new() -> Self {
            Self {
                listener: TcpListener::bind(("127.0.0.1", 0)).unwrap(),
            }
        }

        fn port(&self) -> u16 {
            self.listener.local_addr().unwrap().port()
        }

        /// Returns `(request head, request body)`. `half_close` shuts down
        /// the server's write side right after the reply, modelling a bare
        /// `OK2` response (no blank-line terminator).
        fn serve_once(self, reply: &'static str, half_close: bool) -> (String, Vec<u8>) {
            let (mut conn, _) = self.listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") && !head.ends_with(b"\n\n") {
                if conn.read(&mut byte).unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            conn.write_all(reply.as_bytes()).unwrap();
            if half_close {
                conn.shutdown(Shutdown::Write).unwrap();
            }
            let mut body = Vec::new();
            let _ = conn.read_to_end(&mut body);
            (String::from_utf8_lossy(&head).into_owned(), body)
        }
    }

    fn test_config(port: u16) -> OutputConfig {
        OutputConfig {
            host: "127.0.0.1".into(),
            port,
            mount: "/test.opus".into(),
            source_user: "source".into(),
            source_password: "hackme".into(),
            format: OutputFormat::Opus,
            ..Default::default()
        }
    }

    fn shoutcast_config(port: u16, protocol: OutputProtocol) -> OutputConfig {
        OutputConfig {
            host: "127.0.0.1".into(),
            port,
            mount: "/stream/1".into(),
            source_password: "hackme".into(),
            protocol,
            format: OutputFormat::Mp3,
            ..Default::default()
        }
    }

    #[test]
    fn connect_sends_single_authenticated_source_request() {
        let server = FakeIcecast::new();
        let port = server.port();
        let capture = thread::spawn(move || {
            server.serve_once("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n", false)
        });
        let mut client = IcecastClient::connect(&test_config(port), 44100, 2).expect("connects");
        client.send(b"encoded-bytes").expect("sends");
        let (head, body) = capture.join().unwrap();

        assert!(head.starts_with("SOURCE /test.opus HTTP/1.0\r\n"), "{head}");
        assert!(
            head.contains("Authorization: Basic c291cmNlOmhhY2ttZQ==\r\n"),
            "{head}"
        );
        assert!(head.contains("Content-Type: audio/ogg\r\n"), "{head}");
        assert!(head.contains("ice-name: Crabsoup\r\n"), "{head}");
        assert!(head.contains("ice-public: 0\r\n"), "{head}");
        assert_eq!(body, b"encoded-bytes");
    }

    #[test]
    fn connect_rejects_bad_credentials() {
        let server = FakeIcecast::new();
        let port = server.port();
        thread::spawn(move || {
            server.serve_once(
                "HTTP/1.0 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
                false,
            )
        });
        let err = IcecastClient::connect(&test_config(port), 44100, 2).unwrap_err();
        assert!(err.to_string().contains("401"), "{err}");
        assert!(err.to_string().contains("/test.opus"), "{err}");
    }

    #[test]
    fn update_title_sends_encoded_metadata() {
        let server = FakeIcecast::new();
        let port = server.port();
        let capture = thread::spawn(move || {
            server.serve_once("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n", false)
        });
        IcecastClient::update_title(&test_config(port), "café & trance").expect("updates");
        let (head, _) = capture.join().unwrap();

        assert!(
            head.starts_with(
                "GET /admin/metadata?mode=updinfo&charset=UTF-8&mount=%2Ftest.opus&song=caf%C3%A9%20%26%20trance "
            ),
            "{head}"
        );
        assert!(
            head.contains("Authorization: Basic c291cmNlOmhhY2ttZQ==\r\n"),
            "{head}"
        );
    }

    #[test]
    fn update_title_detects_icecast_rejection() {
        // Icecast answers HTTP 200 even when refusing (Opus mounts); only the
        // body reveals the rejection.
        let body = "<?xml version=\"1.0\"?><iceresponse><message>Mountpoint will not accept URL updates</message><return>1</return></iceresponse>";
        let reply = format!(
            "HTTP/1.0 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let server = FakeIcecast::new();
        let port = server.port();
        thread::spawn(move || server.serve_once(Box::leak(reply.into_boxed_str()), false));
        let err = IcecastClient::update_title(&test_config(port), "track").unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
        assert!(err.to_string().contains("will not accept"), "{err}");
    }

    #[test]
    fn shoutcast_v1_handshake_sends_password_and_icy_headers() {
        let server = FakeIcecast::new();
        let port = server.port();
        let capture = thread::spawn(move || server.serve_once("OK2\r\nicy-caps:11\r\n\r\n", false));
        let mut client = IcecastClient::connect(
            &shoutcast_config(port, OutputProtocol::ShoutcastV1),
            44100,
            2,
        )
        .expect("connects");
        client.send(b"encoded-bytes").expect("sends");
        let (head, body) = capture.join().unwrap();

        assert!(head.starts_with("hackme\n"), "{head}");
        assert!(head.contains("icy-name: Crabsoup\n"), "{head}");
        assert!(head.contains("icy-pub: 0\n"), "{head}");
        assert!(head.contains("icy-genre: Various\n"), "{head}");
        assert!(head.contains("icy-br: 192\n"), "{head}");
        assert!(head.contains("icy-sr: 44100\n"), "{head}");
        assert_eq!(body, b"encoded-bytes");
    }

    #[test]
    fn shoutcast_v1_accepts_bare_ok2_reply() {
        let server = FakeIcecast::new();
        let port = server.port();
        let capture = thread::spawn(move || server.serve_once("OK2\r\n", true));
        let mut client = IcecastClient::connect(
            &shoutcast_config(port, OutputProtocol::ShoutcastV1),
            44100,
            2,
        )
        .expect("connects");
        client.send(b"audio").expect("sends");
        let (head, body) = capture.join().unwrap();
        assert!(head.starts_with("hackme\n"), "{head}");
        assert_eq!(body, b"audio");
    }

    #[test]
    fn shoutcast_v2_uses_icy_handshake_with_stream_selection() {
        let server = FakeIcecast::new();
        let port = server.port();
        let capture = thread::spawn(move || server.serve_once("OK2\r\nicy-caps:11\r\n\r\n", false));
        let mut cfg = shoutcast_config(port, OutputProtocol::ShoutcastV2);
        // The DNAS's named-stream path selects stream id 2 via password:#2.
        cfg.mount = "/stream/2".into();
        IcecastClient::connect(&cfg, 44100, 2).expect("connects");
        let (head, _) = capture.join().unwrap();

        assert!(head.starts_with("hackme:#2\n"), "{head}");
        assert!(head.contains("icy-name: Crabsoup\n"), "{head}");
        assert!(head.contains("icy-br: 192\n"), "{head}");
    }

    #[test]
    fn shoutcast_v2_default_mount_keeps_plain_password() {
        let server = FakeIcecast::new();
        let port = server.port();
        let capture = thread::spawn(move || server.serve_once("OK2\r\nicy-caps:11\r\n\r\n", false));
        let mut cfg = shoutcast_config(port, OutputProtocol::ShoutcastV2);
        cfg.mount = "/".into();
        IcecastClient::connect(&cfg, 44100, 2).expect("connects");
        let (head, _) = capture.join().unwrap();
        assert!(head.starts_with("hackme\n"), "{head}");
    }

    #[test]
    fn shoutcast_rejects_unsupported_formats() {
        // Opus is not a SHOUTcast format for either version.
        let mut cfg = shoutcast_config(0, OutputProtocol::ShoutcastV1);
        cfg.format = OutputFormat::Opus;
        let err = IcecastClient::connect(&cfg, 44100, 2).unwrap_err();
        assert!(err.to_string().contains("mp3"), "{err}");

        let mut cfg = shoutcast_config(0, OutputProtocol::ShoutcastV2);
        cfg.format = OutputFormat::Opus;
        let err = IcecastClient::connect(&cfg, 44100, 2).unwrap_err();
        assert!(err.to_string().contains("mp3"), "{err}");

        // v1 predates AAC; only v2 carries it.
        let mut cfg = shoutcast_config(0, OutputProtocol::ShoutcastV1);
        cfg.format = OutputFormat::Aac;
        let err = IcecastClient::connect(&cfg, 44100, 2).unwrap_err();
        assert!(err.to_string().contains("mp3"), "{err}");
    }

    #[test]
    fn update_icy_title_sends_admin_cgi_request() {
        let server = FakeIcecast::new();
        let port = server.port();
        let capture = thread::spawn(move || {
            server.serve_once("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n", false)
        });
        let mut cfg = shoutcast_config(port, OutputProtocol::ShoutcastV1);
        cfg.source_password = "hackme".into();
        IcecastClient::update_icy_title(&cfg, "café & trance").expect("updates");
        let (head, _) = capture.join().unwrap();

        assert!(
            head.starts_with(
                "GET /admin.cgi?mode=updinfo&pass=hackme&song=caf%C3%A9%20%26%20trance "
            ),
            "{head}"
        );
        assert!(!head.contains("Authorization"), "{head}");
    }

    #[test]
    fn stream_id_from_mount_parses_named_streams() {
        assert_eq!(stream_id_from_mount("/stream/1"), Some(1));
        assert_eq!(stream_id_from_mount("/stream/42"), Some(42));
        assert_eq!(stream_id_from_mount("/"), None);
        assert_eq!(stream_id_from_mount("/stream"), None);
        assert_eq!(stream_id_from_mount("/stream/abc"), None);
    }

    #[test]
    fn percent_encode_leaves_unreserved_alone() {
        assert_eq!(percent_encode(b"abc-._~123"), "abc-._~123");
        assert_eq!(percent_encode(b"/caf\xc3\xa9.ogg"), "%2Fcaf%C3%A9.ogg");
        assert_eq!(percent_encode(b" "), "%20");
    }
}
