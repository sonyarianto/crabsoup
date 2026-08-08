//! Native Icecast source-protocol client.
//!
//! Replaces libshout: a single authenticated `SOURCE` request (no capability
//! probes, no `!POKE`, no 401-then-200) plus a plain `/admin/metadata` GET
//! for titles. The pump loop in `icecast.rs` provides real-time pacing, so
//! this client is a dumb transport.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use base64::Engine as _;

use crate::config::{OutputConfig, OutputFormat};
use crate::Result;

const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// An established source connection to Icecast.
#[derive(Debug)]
pub struct IcecastClient {
    stream: TcpStream,
}

impl IcecastClient {
    /// Open the mount with a single authenticated `SOURCE` request.
    pub fn connect(config: &OutputConfig, sample_rate: u32, channels: u16) -> Result<Self> {
        let mut stream = TcpStream::connect((config.host.as_str(), config.port))?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;

        let content_type = match config.format {
            OutputFormat::Mp3 => "audio/mpeg",
            OutputFormat::Opus => "audio/ogg",
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
    use std::net::TcpListener;
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

        /// Returns `(request head, request body)`.
        fn serve_once(self, reply: &'static str) -> (String, Vec<u8>) {
            let (mut conn, _) = self.listener.accept().unwrap();
            conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                if conn.read(&mut byte).unwrap() == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            conn.write_all(reply.as_bytes()).unwrap();
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

    #[test]
    fn connect_sends_single_authenticated_source_request() {
        let server = FakeIcecast::new();
        let port = server.port();
        let capture = thread::spawn(move || {
            server.serve_once("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n")
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
            server.serve_once("HTTP/1.0 401 Unauthorized\r\nContent-Length: 0\r\n\r\n")
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
            server.serve_once("HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n")
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
        thread::spawn(move || server.serve_once(Box::leak(reply.into_boxed_str())));
        let err = IcecastClient::update_title(&test_config(port), "track").unwrap_err();
        assert!(err.to_string().contains("refused"), "{err}");
        assert!(err.to_string().contains("will not accept"), "{err}");
    }

    #[test]
    fn percent_encode_leaves_unreserved_alone() {
        assert_eq!(percent_encode(b"abc-._~123"), "abc-._~123");
        assert_eq!(percent_encode(b"/caf\xc3\xa9.ogg"), "%2Fcaf%C3%A9.ogg");
        assert_eq!(percent_encode(b" "), "%20");
    }
}
