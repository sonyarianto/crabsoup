use std::io::{Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ringbuf::{HeapRb, traits::*};
use symphonia::core::audio::{SampleBuffer, SignalSpec};
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream, MediaSourceStreamOptions};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::config::LiveConfig;
use crate::engine::mixer::MixCommand;
use crate::live::source::{LiveSink, LiveSource};
use crate::source::{AudioSource, PcmConverter};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_LIVE_FRAMES: usize = 5 * 44100 * 2; // ~5 s of stereo audio
/// Upper bound on how long a fast upload's buffered tail may hold the
/// decode thread open after the stream ends. The ring caps the tail at
/// `MAX_LIVE_FRAMES` and drains at real time, so this only bites a stalled
/// consumer.
const DRAIN_WAIT_SECS: u64 = 15;

/// The live DJ harbor: an Icecast source-protocol listener.
///
/// A DJ connects with `PUT /<mount>`, authenticates with Basic auth and then
/// streams an encoded audio stream (MP3 / Ogg / etc.). We decode it to the
/// target PCM spec, feed it through a [`LiveSource`] into the [`PriorityMixer`],
/// and fade the playlist out. When the DJ disconnects we fade back in.
pub struct Harbor {
    config: LiveConfig,
    target: SignalSpec,
    tx: mpsc::Sender<MixCommand>,
    occupied: Arc<AtomicBool>,
}

impl Harbor {
    pub fn new(
        config: LiveConfig,
        target: SignalSpec,
        tx: mpsc::Sender<MixCommand>,
    ) -> Self {
        Self {
            config,
            target,
            tx,
            occupied: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Run the accept loop forever. Must be spawned onto a tokio runtime.
    pub async fn run(self) {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("live harbor: failed to bind {addr}: {e}");
                return;
            }
        };
        log::info!(
            "live harbor listening on {addr} (mount {}, password protected)",
            self.config.mount
        );

        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    log::warn!("live harbor: accept failed: {e}");
                    continue;
                }
            };
            let config = self.config.clone();
            let target = self.target;
            let tx = self.tx.clone();
            let occupied = self.occupied.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, config, target, tx, occupied).await {
                    log::warn!("live harbor ({peer}): {e}");
                }
            });
        }
    }
}

/// Handle a single source-protocol connection.
async fn handle_connection(
    socket: TcpStream,
    config: LiveConfig,
    target: SignalSpec,
    tx: mpsc::Sender<MixCommand>,
    occupied: Arc<AtomicBool>,
) -> Result<(), String> {
    let mut socket = socket;
    socket.set_nodelay(true).map_err(|e| e.to_string())?;

    // Read the header block (up to CRLFCRLF).
    let header = read_header(&mut socket).await?;

    // Parse the request.
    let request = match parse_source_request(&header) {
        Ok(r) => r,
        Err(code) => {
            respond(&mut socket, code, reason(code)).await?;
            return Ok(());
        }
    };

    // Mount match.
    if request.path != config.mount {
        respond(&mut socket, 404, "Not Found").await?;
        return Ok(());
    }

    // Auth (source protocol Basic auth, password match).
    let auth_ok = request
        .authorization
        .as_deref()
        .map(|cred| basic_password_matches(cred, &config.password))
        .unwrap_or(false);
    if !auth_ok {
        respond(&mut socket, 401, "Unauthorized").await?;
        return Ok(());
    }

    // Only one DJ at a time.
    if occupied.swap(true, Ordering::SeqCst) {
        respond(&mut socket, 403, "Forbidden").await?;
        return Ok(());
    }

    log::info!("live harbor: DJ connected to {}", config.mount);

    // A bounded upload (`curl -T file`, Content-Length present) must not see
    // the final 200 before its body: curl aborts the rest of the transfer the
    // moment a complete response arrives mid-upload. Infinite streaming
    // sources (ffmpeg, ices) need the 200 promptly or they refuse to start,
    // so they keep receiving it immediately. The decode thread sends the
    // deferred 200 when the bounded body has been fully consumed.
    let respond_at_end = request.content_length.is_some();
    if !respond_at_end {
        respond(&mut socket, 200, "OK").await?;
    }

    // Convert to a blocking stream *before* announcing the DJ: if this fails
    // we roll back the occupied flag. tokio leaves the fd non-blocking, and a
    // non-blocking read returning EAGAIN (slow DJ upload) is read by symphonia
    // as end-of-stream, so the decode thread must use blocking reads.
    let std_stream = match socket.into_std() {
        Ok(s) => match s.set_nonblocking(false) {
            Ok(()) => s,
            Err(e) => {
                occupied.store(false, Ordering::SeqCst);
                return Err(format!("switching to blocking mode: {e}"));
            }
        },
        Err(e) => {
            occupied.store(false, Ordering::SeqCst);
            return Err(format!("converting to blocking stream: {e}"));
        }
    };

    // Hand the connection over to a decode thread. The SPSC ring (sized at
    // twice the drop-oldest cap) hands PCM from the decode thread to the
    // mixer's LiveSource lock-free; the consumer enforces the window.
    let (prod, cons) = HeapRb::<f32>::new(2 * MAX_LIVE_FRAMES).split();
    let exhausted = Arc::new(AtomicBool::new(false));
    let live = Box::new(LiveSource::new(cons, exhausted.clone(), MAX_LIVE_FRAMES));
    let _ = tx.send(MixCommand::SetLive(live));

    thread::spawn(move || {
        decode_live_stream(
            std_stream,
            request.content_type.clone(),
            request.chunked,
            respond_at_end,
            target,
            LiveSink::new(prod),
            exhausted,
            tx,
            occupied,
        );
    });

    Ok(())
}

/// Decode a DJ's encoded stream to target PCM and push it into the shared
/// queue. When `respond_at_end` (bounded upload such as `curl -T file`), the
/// final 200 is written only after the body has been fully consumed —
/// sending it earlier makes curl abort the upload; infinite streaming
/// sources get their 200 at connect instead and never hit this path.
#[allow(clippy::too_many_arguments)]
fn decode_live_stream(
    stream: std::net::TcpStream,
    content_type: Option<String>,
    chunked: bool,
    respond_at_end: bool,
    target: SignalSpec,
    mut sink: LiveSink,
    exhausted: Arc<AtomicBool>,
    tx: mpsc::Sender<MixCommand>,
    occupied: Arc<AtomicBool>,
) {
    decode_live_stream_inner(stream, content_type, chunked, respond_at_end, target, &mut sink);
    log::debug!("live harbor: decode done, buffered={}", sink.buffered());
    // A fast upload (e.g. `curl -T`) can land several seconds of audio in
    // the ring before the mixer processes `SetLive`; a `ClearLive` sent
    // here would then drop the whole tail in one command drain. Wait for
    // the consumer to drain the ring first — the mixer auto-fades once the
    // source drains, so `ClearLive` only confirms it. Bounded so a stalled
    // consumer (ring full of audio nobody pulls) can't wedge the decode
    // thread forever; the ring drains at real time, so 15 s covers the
    // worst-case tail.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(DRAIN_WAIT_SECS);
    while sink.buffered() > 0 && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    log::debug!("live harbor: drain-wait done, buffered={}", sink.buffered());
    finish(&exhausted, &tx, &occupied);
}

#[allow(clippy::too_many_arguments)]
fn decode_live_stream_inner(
    mut stream: std::net::TcpStream,
    content_type: Option<String>,
    chunked: bool,
    respond_at_end: bool,
    target: SignalSpec,
    sink: &mut LiveSink,
) {
    fn done(respond_at_end: bool, stream: &mut std::net::TcpStream) {
        if respond_at_end {
            let _ = std::io::Write::write_all(stream, OK_RESPONSE);
        }
    }
    let Ok(reader_sock) = stream.try_clone() else {
        log::warn!("live harbor: cannot clone live socket");
        return;
    };
    let inner: Box<dyn Read + Send + Sync> = if chunked {
        Box::new(ChunkedReader::new(reader_sock))
    } else {
        Box::new(reader_sock)
    };

    // Peek the first Ogg page: an OpusHead stream takes the native
    // `OpusSource` path (symphonia 0.5 has no Opus codec), anything else is
    // probed by symphonia as before. Either way the sniffed bytes are fed
    // back so the stream is never consumed past its head.
    let (is_opus, prefix, rest) = match crate::source::opus::sniff_stream(inner) {
        Ok(t) => t,
        Err(e) => {
            log::warn!("live harbor: sniffing DJ stream: {e}");
            done(respond_at_end, &mut stream);
            return;
        }
    };
    if is_opus {
        decode_opus_live(
            crate::source::opus::PrependReader::new(prefix, rest),
            target,
            sink,
        );
        done(respond_at_end, &mut stream);
        return;
    }
    let inner: Box<dyn Read + Send + Sync> =
        Box::new(crate::source::opus::PrependReader::new(prefix, rest));

    let src = NonSeekableSource { inner };
    let mss = MediaSourceStream::new(Box::new(src), MediaSourceStreamOptions::default());

    let hint = hint_for_content_type(content_type.as_deref());
    let probed = match symphonia::default::get_probe().format(
        &hint,
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    ) {
        Ok(p) => p,
        Err(e) => {
            log::error!("live harbor: cannot probe DJ stream: {e}");
            done(respond_at_end, &mut stream);
            return;
        }
    };

    let mut format = probed.format;
    let Some(track) = format.default_track().cloned() else {
        log::error!("live harbor: DJ stream has no default audio track");
        done(respond_at_end, &mut stream);
        return;
    };
    let track_id = track.id;
    let mut decoder = match symphonia::default::get_codecs().make(
        &track.codec_params,
        &DecoderOptions::default(),
    ) {
        Ok(d) => d,
        Err(e) => {
            log::error!("live harbor: cannot create decoder: {e}");
            done(respond_at_end, &mut stream);
            return;
        }
    };

    let mut converter = PcmConverter::new(target);

    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(e) if matches!(e, symphonia::core::errors::Error::IoError(_)) => {
                log::info!("live harbor: DJ stream ended ({e})");
                break;
            }
            Err(e) => {
                log::warn!("live harbor: packet error, skipping: {e}");
                continue;
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(decoded) => {
                let frames = decoded.frames();
                if frames == 0 {
                    continue;
                }
                let spec = *decoded.spec();
                let mut sbuf = SampleBuffer::<f32>::new(frames as u64, spec);
                sbuf.copy_interleaved_ref(decoded);
                let converted = converter.convert(sbuf.samples(), &spec);
                sink.push_samples(&converted);
            }
            Err(e) => {
                log::warn!("live harbor: decode error, skipping: {e}");
            }
        }
    }

    // Flush the resampler tail.
    let tail = converter.flush();
    if !tail.is_empty() {
        sink.push_samples(&tail);
    }
    done(respond_at_end, &mut stream);
}

fn finish(
    exhausted: &Arc<AtomicBool>,
    tx: &mpsc::Sender<MixCommand>,
    occupied: &Arc<AtomicBool>,
) {
    exhausted.store(true, Ordering::SeqCst);
    // Always fade back out; the priority mixer also auto-fades once the live
    // source drains, so a ClearLive with buffered audio just starts the fade
    // and the tail plays out gracefully.
    let _ = tx.send(MixCommand::ClearLive);
    occupied.store(false, Ordering::SeqCst);
    log::info!("live harbor: DJ disconnected");
}

/// Decode a DJ's Opus stream natively (audiopus) and push target-spec PCM
/// into the live sink. Uses the same [`OpusSource`] as file playback.
fn decode_opus_live<R: Read + Send>(reader: R, target: SignalSpec, sink: &mut LiveSink) {
    let mut src = match crate::source::opus::OpusSource::open(
        reader,
        target,
        target.rate as usize,
        "LIVE DJ (opus)".into(),
    ) {
        Ok(s) => s,
        Err(e) => {
            log::error!("live harbor: cannot decode opus dj stream: {e}");
            return;
        }
    };
    let mut pushed = 0usize;
    let mut buf = vec![0f32; MAX_LIVE_FRAMES];
    loop {
        let n = src.next_buffer(&mut buf);
        if n == 0 {
            if src.is_exhausted() {
                break;
            }
            continue;
        }
        sink.push_samples(&buf[..n]);
        pushed += n;
    }
    log::debug!("live harbor: opus dj decode pushed {pushed} samples");
}

/// A read-only, non-seekable adapter so symphonia can consume a TCP stream.
struct NonSeekableSource<R: Read + Send + Sync> {
    inner: R,
}

impl<R: Read + Send + Sync> Read for NonSeekableSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Send + Sync> Seek for NonSeekableSource<R> {
    fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::other("live source is not seekable"))
    }
}

impl<R: Read + Send + Sync> MediaSource for NonSeekableSource<R> {
    fn is_seekable(&self) -> bool {
        false
    }

    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// De-chunks a `Transfer-Encoding: chunked` body, yielding the raw payload.
///
/// Chunked framing is `hex-size[;ext]\r\n <data> \r\n ... 0\r\n <trailers> \r\n`;
/// this strips the size lines and CRLFs as the client streams the data.
struct ChunkedReader<R: Read> {
    inner: R,
    remaining: usize,
    done: bool,
}

impl<R: Read> ChunkedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            remaining: 0,
            done: false,
        }
    }

    /// Read bytes up to and including the next `\n` (discarding it).
    fn read_until_lf(&mut self, buf: &mut Vec<u8>) -> std::io::Result<()> {
        loop {
            let mut byte = [0u8; 1];
            match self.inner.read(&mut byte) {
                Ok(0) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "unexpected EOF in chunk header",
                    ))
                }
                Ok(_) => {}
                Err(e) => return Err(e),
            }
            if byte[0] == b'\n' {
                return Ok(());
            }
            buf.push(byte[0]);
        }
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.done || out.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let size = loop {
                let mut line = Vec::new();
                self.read_until_lf(&mut line)?;
                while matches!(line.last(), Some(b'\r') | Some(b'\n')) {
                    line.pop();
                }
                // The size may carry chunk extensions: "1a;name=value".
                let hex = line
                    .split(|&b| b == b';')
                    .next()
                    .unwrap_or_default()
                    .iter()
                    .map(|&b| b as char)
                    .collect::<String>();
                let hex = hex.trim();
                if hex.is_empty() {
                    // Some clients send a stray blank line before the first
                    // chunk size line; skip it rather than fail the stream.
                    continue;
                }
                break usize::from_str_radix(hex, 16).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("invalid chunk size: {hex}"),
                    )
                })?;
            };
            if size == 0 {
                // Terminal chunk. Any trailers are ignored; we are done.
                self.done = true;
                return Ok(0);
            }
            self.remaining = size;
        }

        let want = self.remaining.min(out.len());
        let n = self.inner.read(&mut out[..want])?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "unexpected EOF inside chunk data",
            ));
        }
        self.remaining -= n;
        if self.remaining == 0 {
            // Consume the CRLF after each chunk's data.
            self.read_until_lf(&mut Vec::new())?;
        }
        Ok(n)
    }
}

/// Parsed source-protocol request.
#[derive(Debug)]
struct SourceRequest {
    path: String,
    authorization: Option<String>,
    content_type: Option<String>,
    /// `Content-Length` when the client sent one (a bounded upload such as
    /// `curl -T file`). `None` for infinite streaming sources (ffmpeg etc.).
    content_length: Option<u64>,
    /// `Transfer-Encoding: chunked` — the body must be de-chunked before
    /// decoding (ffmpeg's `-method PUT` uses this).
    chunked: bool,
}

fn parse_source_request(header: &[u8]) -> Result<SourceRequest, u16> {
    let text = String::from_utf8_lossy(header);
    let mut lines = text.lines();
    let request_line = lines.next().ok_or(400u16)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    if !method.eq_ignore_ascii_case("PUT") {
        return Err(405);
    }
    let target = parts.next().unwrap_or("").to_string();
    let path = target.split('?').next().unwrap_or("").to_string();

    let mut authorization = None;
    let mut content_type = None;
    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_ascii_lowercase();
            let val = v.trim().to_string();
            match key.as_str() {
                "authorization" => authorization = Some(val),
                "content-type" => content_type = Some(val),
                "content-length" => content_length = val.parse::<u64>().ok(),
                "transfer-encoding" => chunked = val.to_ascii_lowercase().contains("chunked"),
                _ => {}
            }
        }
    }

    Ok(SourceRequest {
        path,
        authorization,
        content_type,
        content_length,
        chunked,
    })
}

/// Decode `Basic <base64>` credentials and check the password part.
fn basic_password_matches(credential: &str, password: &str) -> bool {
    let b64 = credential
        .strip_prefix("Basic ")
        .or_else(|| credential.strip_prefix("basic "));
    let Some(b64) = b64 else {
        return false;
    };
    let Ok(decoded) = BASE64.decode(b64.trim()) else {
        return false;
    };
    let Ok(text) = String::from_utf8(decoded) else {
        return false;
    };
    text.rsplit_once(':')
        .map(|(_, pass)| pass == password)
        .unwrap_or(false)
}

fn hint_for_content_type(content_type: Option<&str>) -> Hint {
    let mut hint = Hint::new();
    let ct = content_type.unwrap_or("").to_ascii_lowercase();
    if ct.contains("mpeg") || ct.contains("mp3") {
        hint.with_extension("mp3");
    } else if ct.contains("ogg") || ct.contains("opus") {
        hint.with_extension("ogg");
    } else if ct.contains("aac") {
        hint.with_extension("aac");
    }
    hint
}

const OK_RESPONSE: &[u8] =
    b"HTTP/1.0 200 OK\r\nServer: Crabsoup Harbor\r\nContent-Length: 0\r\n\r\n";

/// Read bytes up to and including CRLFCRLF, or until the size limit.
async fn read_header(socket: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        match socket.read_exact(&mut byte).await {
            Ok(_) => {}
            Err(e) => return Err(format!("reading request header: {e}")),
        }
        buf.push(byte[0]);
        if buf.len() > MAX_HEADER_BYTES {
            return Err("request header too large".into());
        }
        if buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" {
            return Ok(buf);
        }
    }
}

async fn respond(socket: &mut TcpStream, code: u16, reason: &str) -> Result<(), String> {
    let body = format!("HTTP/1.0 {code} {reason}\r\nServer: Crabsoup Harbor\r\nContent-Length: 0\r\n\r\n");
    socket
        .write_all(body.as_bytes())
        .await
        .map_err(|e| format!("sending response: {e}"))
}

fn reason(code: u16) -> &'static str {
    match code {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        200 => "OK",
        _ => "Error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_put_request() {
        let header = b"PUT /live HTTP/1.1\r\nAuthorization: Basic c291cmNlOnNlY3JldA==\r\nContent-Type: application/mpeg\r\nContent-Length: 466982\r\n\r\n";
        let req = parse_source_request(header).unwrap();
        assert_eq!(req.path, "/live");
        assert_eq!(req.authorization.as_deref(), Some("Basic c291cmNlOnNlY3JldA=="));
        assert_eq!(req.content_type.as_deref(), Some("application/mpeg"));
        assert_eq!(req.content_length, Some(466982));
        assert!(!req.chunked);
    }

    #[test]
    fn parses_chunked_put_request() {
        let header = b"PUT /live HTTP/1.1\r\nTransfer-Encoding: chunked\r\nContent-Type: audio/mpeg\r\n\r\n";
        let req = parse_source_request(header).unwrap();
        assert!(req.chunked);
    }

    #[test]
    fn rejects_non_put() {
        let header = b"GET /live HTTP/1.1\r\n\r\n";
        assert_eq!(parse_source_request(header).unwrap_err(), 405);
    }

    #[test]
    fn basic_auth_password_match() {
        // "source:secret"
        assert!(basic_password_matches("Basic c291cmNlOnNlY3JldA==", "secret"));
        assert!(!basic_password_matches("Basic c291cmNlOnNlY3JldA==", "nope"));
        assert!(!basic_password_matches("Bearer xyz", "secret"));
        assert!(!basic_password_matches("Basic not-base64!!", "secret"));
    }

    #[test]
    fn hint_from_content_type() {
        let _ = hint_for_content_type(Some("application/mpeg"));
        let _ = hint_for_content_type(Some("audio/ogg"));
        let _ = hint_for_content_type(None);
    }

    #[test]
    fn header_reader_finds_crlfcrlf() {
        // Simulate the read loop logic over an in-memory slice.
        let header = b"PUT /live HTTP/1.1\r\n\r\n";
        let mut buf = Vec::new();
        for b in header {
            buf.push(*b);
            if buf.len() >= 4 && buf[buf.len() - 4..] == *b"\r\n\r\n" {
                assert_eq!(&buf, header);
                return;
            }
        }
        panic!("delimiter not found");
    }

    #[test]
    fn chunked_reader_strips_framing() {
        // 46 bytes, then 2 bytes, then the terminal chunk.
        let framed: &[u8] = b"2e\r\n0123456789abcdefghijklmnopqrstuvwxyz0123456789\r\n\
2\r\nzz\r\n0\r\n\r\n";
        let mut reader = ChunkedReader::new(framed);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(
            out,
            b"0123456789abcdefghijklmnopqrstuvwxyz0123456789zz"
        );
    }

    #[test]
    fn chunked_reader_handles_small_reads_and_extensions() {
        let framed: &[u8] = b"4;foo=bar\r\nabcd\r\n3\r\nefg\r\n0\r\n\r\n";
        let mut reader = ChunkedReader::new(framed);
        let mut out = Vec::new();
        let mut buf = [0u8; 2];
        loop {
            let n = reader.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, b"abcdefg");
    }

    #[test]
    fn chunked_reader_skips_stray_blank_line_before_first_chunk() {
        // ffmpeg's `-headers` with a trailing CRLFCRLF emits an extra `\r\n`
        // before the first chunk size line; the reader must tolerate it.
        let framed: &[u8] = b"\r\n4\r\nWiki\r\n0\r\n\r\n";
        let mut reader = ChunkedReader::new(framed);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"Wiki");
    }

    #[test]
    fn chunked_reader_rejects_garbage_size_line() {
        let mut reader = ChunkedReader::new(b"zz\r\nWiki\r\n0\r\n\r\n".as_slice());
        let mut out = Vec::new();
        let err = reader.read_to_end(&mut out).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }
}
