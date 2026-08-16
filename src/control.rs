//! Liquidsoap-style telnet control port.
//!
//! Connects with `telnet <host> <port>` and issues one command per line.
//! Commands:
//!
//! - `jingles.list`            — list available jingles
//! - `jingles.play`            — play a random jingle
//! - `jingles.play <n>`        — play jingle at index `n`
//! - `jingles.play <substr>`   — play the jingle whose name contains `substr`
//! - `skip`                    — skip the current track
//! - `status`                  — current track + uptime
//! - `uptime`                  — seconds since startup
//! - `shutdown`                — stop the app (like Ctrl-C)
//! - `exit` / `quit`           — close the connection
//! - `<name> [args...]`        — any command registered with `server.register`
//! - `help`                    — list commands
//!
//! Prefix any command with `json ` for a machine-readable reply: each is a
//! single line of JSON, `{"ok": true, ...}` on success and
//! `{"ok": false, "error": "..."}` on failure (custom Lua commands wrap
//! their reply in `{"ok": true, "reply": "..."}`). The name `json` is
//! reserved and cannot be a `server.register` command. `banner = false` on
//! `server.telnet` skips the text welcome line so machine clients get
//! replies from byte zero.
//!
//! `server.telnet({http_port = N})` also serves the same command surface
//! over HTTP on the same host: `GET /status`, `GET /uptime`, `GET /queue`,
//! `GET /jingles`, and `POST /cmd` with a JSON body
//! `{"command": "..."}`. Every response reuses the JSON envelope above.
//!
//! `server.telnet({ws_port = N})` serves it over WebSocket (RFC 6455) on
//! the same host: each text frame is one command (either a bare command
//! line like `status` or the HTTP-style JSON `{"command": "..."}`), the
//! reply is the JSON envelope as the next text frame, and `ping` gets a
//! `pong`. Hand-rolled framing (SHA-1 handshake + masked frame codec) so
//! the control surface stays dependency-free; browsers and `wscat` work
//! as-is.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use base64::Engine as _;
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use sha1::{Digest, Sha1};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::config::ControlConfig;
use crate::engine::mixer::{MixCommand, StatusHandle};
use crate::script::ScriptEvent;
use crate::source::request::RequestQueue;

/// Telnet command server. Owns one `mpsc::Sender` into the priority mixer.
pub struct ControlServer {
    config: ControlConfig,
    jingles: Vec<PathBuf>,
    queue: Option<Arc<RequestQueue>>,
    tx: mpsc::Sender<MixCommand>,
    status: StatusHandle,
    custom_commands: Arc<Vec<String>>,
    event_tx: mpsc::Sender<ScriptEvent>,
}

impl ControlServer {
    pub fn new(
        config: ControlConfig,
        jingles: Vec<PathBuf>,
        queue: Option<Arc<RequestQueue>>,
        tx: mpsc::Sender<MixCommand>,
        status: StatusHandle,
        custom_commands: Arc<Vec<String>>,
        event_tx: mpsc::Sender<ScriptEvent>,
    ) -> Self {
        Self {
            config,
            jingles,
            queue,
            tx,
            status,
            custom_commands,
            event_tx,
        }
    }

    /// Run the accept loop forever. Must be spawned onto a tokio runtime.
    pub async fn run(self) {
        let addr = format!("{}:{}", self.config.host, self.config.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("control port: failed to bind {addr}: {e}");
                return;
            }
        };
        log::info!(
            "control port listening on {addr} ({} jingle(s), {} custom command(s))",
            self.jingles.len(),
            self.custom_commands.len()
        );

        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    log::warn!("control port: accept failed: {e}");
                    continue;
                }
            };
            let banner = self.config.banner;
            let jingles = self.jingles.clone();
            let queue = self.queue.clone();
            let tx = self.tx.clone();
            let status = self.status.clone();
            let custom = self.custom_commands.clone();
            let event_tx = self.event_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(
                    socket, banner, &jingles, queue, tx, &status, &custom, &event_tx,
                )
                .await
                {
                    log::warn!("control port ({peer}): {e}");
                }
            });
        }
    }
}

/// Minimal HTTP/1.1 status/control endpoint on the same host as the
/// telnet port (`server.telnet({http_port = N})`). Routes:
///
/// - `GET /status`, `GET /uptime`, `GET /queue`, `GET /jingles`
/// - `POST /cmd` with a JSON body `{"command": "..."}` — any control
///   command, with the same reply as `json <command>` on telnet
///
/// Every response is the JSON envelope; application errors (unknown
/// command, bad usage) are HTTP 400, unknown routes 404, wrong methods
/// 405. The response body is the single-line JSON from [`CommandReply::json`],
/// so the contract is identical to the telnet JSON mode.
pub struct ControlHttpServer {
    host: String,
    port: u16,
    jingles: Vec<PathBuf>,
    queue: Option<Arc<RequestQueue>>,
    tx: mpsc::Sender<MixCommand>,
    status: StatusHandle,
    custom_commands: Arc<Vec<String>>,
    event_tx: mpsc::Sender<ScriptEvent>,
}

impl ControlHttpServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: String,
        port: u16,
        jingles: Vec<PathBuf>,
        queue: Option<Arc<RequestQueue>>,
        tx: mpsc::Sender<MixCommand>,
        status: StatusHandle,
        custom_commands: Arc<Vec<String>>,
        event_tx: mpsc::Sender<ScriptEvent>,
    ) -> Self {
        Self {
            host,
            port,
            jingles,
            queue,
            tx,
            status,
            custom_commands,
            event_tx,
        }
    }

    /// Run the accept loop forever. Must be spawned onto a tokio runtime.
    pub async fn run(self) {
        let addr = format!("{}:{}", self.host, self.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("control http: failed to bind {addr}: {e}");
                return;
            }
        };
        log::info!("control http listening on {addr}");

        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    log::warn!("control http: accept failed: {e}");
                    continue;
                }
            };
            let jingles = self.jingles.clone();
            let queue = self.queue.clone();
            let tx = self.tx.clone();
            let status = self.status.clone();
            let custom = self.custom_commands.clone();
            let event_tx = self.event_tx.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    handle_http(socket, &jingles, queue, tx, &status, &custom, &event_tx).await
                {
                    log::warn!("control http ({peer}): {e}");
                }
            });
        }
    }
}

const MAX_HTTP_HEADER: usize = 16 * 1024;
const MAX_HTTP_BODY: usize = 64 * 1024;

/// Serve one HTTP request and close the connection (no keep-alive).
async fn handle_http(
    mut socket: TcpStream,
    jingles: &[PathBuf],
    queue: Option<Arc<RequestQueue>>,
    tx: mpsc::Sender<MixCommand>,
    status: &StatusHandle,
    custom: &[String],
    event_tx: &mpsc::Sender<ScriptEvent>,
) -> Result<(), String> {
    let mut rng = SmallRng::from_entropy();
    let (method, path, body) = match read_http_request(&mut socket).await {
        Ok(req) => req,
        Err((code, body)) => {
            write_http(&mut socket, code, &body).await?;
            return Ok(());
        }
    };
    let ctx = DispatchCtx {
        jingles,
        queue: queue.as_deref(),
        tx: &tx,
        status,
        custom,
        event_tx,
    };
    let (code, body) = http_route(&method, &path, &body, &ctx, &mut rng);
    write_http(&mut socket, code, &body).await
}

/// Read a raw request head (up to and including `\r\n\r\n`). Returns the
/// head text and any bytes already read past it (the start of the body),
/// or the HTTP error response to send.
async fn read_head_block<S: tokio::io::AsyncRead + Unpin>(
    socket: &mut S,
) -> Result<(String, Vec<u8>), (u16, String)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let head_end = loop {
        if buf.len() > MAX_HTTP_HEADER {
            return Err((431, http_error("request header too large")));
        }
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = socket
            .read(&mut chunk)
            .await
            .map_err(|e| (400, http_error(&format!("read error: {e}"))))?;
        if n == 0 {
            return Err((400, http_error("connection closed mid-request")));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    Ok((head, buf[head_end + 4..].to_vec()))
}

/// Read a full request (header block + Content-Length body). Returns
/// `(method, path, body)` or the HTTP error response to send.
async fn read_http_request(
    socket: &mut TcpStream,
) -> Result<(String, String, String), (u16, String)> {
    let (head, mut body) = read_head_block(socket).await?;
    let (method, path) = match parse_request_head(&head) {
        Ok(p) => p,
        Err((code, msg)) => return Err((code, http_error(&msg))),
    };
    if let Some(len) = content_length(&head) {
        if len > MAX_HTTP_BODY {
            return Err((413, http_error("request body too large")));
        }
        while body.len() < len {
            let mut chunk = [0u8; 2048];
            let n = socket
                .read(&mut chunk)
                .await
                .map_err(|e| (400, http_error(&format!("read error: {e}"))))?;
            if n == 0 {
                return Err((400, http_error("connection closed mid-body")));
            }
            body.extend_from_slice(&chunk[..n]);
        }
        body.truncate(len);
    }
    Ok((method, path, String::from_utf8_lossy(&body).into_owned()))
}

/// Parse the request line of a header block into `(method, path)`.
fn parse_request_head(head: &str) -> Result<(String, String), (u16, String)> {
    let line = head.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(method), Some(path), Some(_version)) => Ok((method.into(), path.into())),
        _ => Err((400, "malformed request line".into())),
    }
}

/// Content-Length header value, if present (case-insensitive).
fn content_length(head: &str) -> Option<usize> {
    head.lines().skip(1).find_map(|l| {
        let (name, value) = l.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            value.trim().parse().ok()
        } else {
            None
        }
    })
}

/// Route a parsed request to the control command surface and render the
/// JSON response. `http_route` is sync so tests can call it directly.
fn http_route(
    method: &str,
    path: &str,
    body: &str,
    ctx: &DispatchCtx,
    rng: &mut SmallRng,
) -> (u16, String) {
    let command: String = match (method, path) {
        ("GET", "/status") => "status".into(),
        ("GET", "/uptime") => "uptime".into(),
        ("GET", "/queue") => "queue.list".into(),
        ("GET", "/jingles") => "jingles.list".into(),
        ("POST", "/cmd") => match serde_json::from_str::<serde_json::Value>(body) {
            Ok(v) => match v.get("command").and_then(serde_json::Value::as_str) {
                Some(c) => c.to_string(),
                None => return (400, http_error("missing \"command\" string field")),
            },
            Err(e) => return (400, http_error(&format!("invalid JSON body: {e}"))),
        },
        ("GET" | "POST", _) => return (404, http_error(&format!("not found: {method} {path}"))),
        _ => return (405, http_error(&format!("method not allowed: {method}"))),
    };
    match dispatch(&command, ctx, rng) {
        CommandResult::Reply(r) => {
            let code = if matches!(&r, CommandReply::Err(_)) {
                400
            } else {
                200
            };
            (code, r.json())
        }
        // `exit`/`quit` close the telnet connection; over HTTP just ack.
        CommandResult::Exit => (200, r#"{"ok":true,"message":"bye"}"#.into()),
    }
}

fn http_error(msg: &str) -> String {
    serde_json::json!({ "ok": false, "error": msg }).to_string()
}

async fn write_http(socket: &mut TcpStream, code: u16, body: &str) -> Result<(), String> {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket
        .write_all(head.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    socket
        .write_all(body.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))
}

/// WebSocket (RFC 6455) control endpoint on the same host as the telnet
/// port (`server.telnet({ws_port = N})`). Each text frame is one command —
/// either a bare command line (`status`) or the HTTP-style JSON
/// `{"command": "..."}` — and the reply is the JSON envelope as the next
/// text frame. Handshake + frame codec are hand-rolled (SHA-1 accept key,
/// masked client frames) so the control surface stays dependency-free.
pub struct ControlWsServer {
    host: String,
    port: u16,
    jingles: Vec<PathBuf>,
    queue: Option<Arc<RequestQueue>>,
    tx: mpsc::Sender<MixCommand>,
    status: StatusHandle,
    custom_commands: Arc<Vec<String>>,
    event_tx: mpsc::Sender<ScriptEvent>,
}

impl ControlWsServer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: String,
        port: u16,
        jingles: Vec<PathBuf>,
        queue: Option<Arc<RequestQueue>>,
        tx: mpsc::Sender<MixCommand>,
        status: StatusHandle,
        custom_commands: Arc<Vec<String>>,
        event_tx: mpsc::Sender<ScriptEvent>,
    ) -> Self {
        Self {
            host,
            port,
            jingles,
            queue,
            tx,
            status,
            custom_commands,
            event_tx,
        }
    }

    /// Run the accept loop forever. Must be spawned onto a tokio runtime.
    pub async fn run(self) {
        let addr = format!("{}:{}", self.host, self.port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => l,
            Err(e) => {
                log::error!("control ws: failed to bind {addr}: {e}");
                return;
            }
        };
        log::info!("control ws listening on {addr}");

        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    log::warn!("control ws: accept failed: {e}");
                    continue;
                }
            };
            let jingles = self.jingles.clone();
            let queue = self.queue.clone();
            let tx = self.tx.clone();
            let status = self.status.clone();
            let custom = self.custom_commands.clone();
            let event_tx = self.event_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_ws_connection(
                    socket, &jingles, queue, tx, &status, &custom, &event_tx,
                )
                .await
                {
                    log::warn!("control ws ({peer}): {e}");
                }
            });
        }
    }
}

/// RFC 6455 GUID appended to the key before SHA-1 for `Sec-WebSocket-Accept`.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Reply to a WebSocket `close` with the same code (1000 = normal).
const WS_CLOSE_NORMAL: u16 = 1000;
/// A control command and its reply are tiny; cap frames at the HTTP body
/// limit so a misbehaving client cannot make us buffer unboundedly.
const MAX_WS_FRAME: usize = MAX_HTTP_BODY;

/// `Sec-WebSocket-Accept` for a handshake key (RFC 6455 §4.2.2):
/// `base64(SHA1(key + WS_GUID))`.
fn ws_accept(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// Case-insensitive header lookup in a request head block.
fn ws_header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

/// Encode a server->client frame (server frames are never masked).
fn encode_ws_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 10);
    out.push(0x80 | opcode); // FIN + opcode
    let len = payload.len();
    if len < 126 {
        out.push(len as u8);
    } else if len <= 0xFFFF {
        out.push(126);
        out.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(len as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// One parsed frame: `(opcode, fin, unmasked payload, bytes consumed)`.
type WsFrame = (u8, bool, Vec<u8>, usize);

/// Parse one frame off the front of `buf`. `Ok(None)` = need more bytes;
/// `Err(code)` = protocol violation (close with that code). Client frames
/// must be masked and are unmasked here; control frames may not be
/// fragmented.
fn parse_ws_frame(buf: &[u8]) -> Result<Option<WsFrame>, u16> {
    if buf.len() < 2 {
        return Ok(None);
    }
    let b0 = buf[0];
    let b1 = buf[1];
    if b0 & 0x70 != 0 {
        return Err(1002); // RSV bits set, no extensions negotiated
    }
    let fin = b0 & 0x80 != 0;
    let opcode = b0 & 0x0F;
    let masked = b1 & 0x80 != 0;
    let len7 = (b1 & 0x7F) as usize;
    if opcode >= 0x8 && (len7 > 125 || !fin) {
        return Err(1002); // control frames are short and unfragmented
    }
    let mut idx = 2;
    let len = match len7 {
        0..=125 => len7,
        126 => {
            if buf.len() < 4 {
                return Ok(None);
            }
            let l = u16::from_be_bytes([buf[2], buf[3]]) as usize;
            idx = 4;
            l
        }
        127 => {
            if buf.len() < 10 {
                return Ok(None);
            }
            let l = u64::from_be_bytes(buf[2..10].try_into().unwrap());
            idx = 10;
            usize::try_from(l).map_err(|_| 1009u16)?
        }
        _ => unreachable!(),
    };
    if len > MAX_WS_FRAME {
        return Err(1009);
    }
    let key_len = if masked { 4 } else { 0 };
    let total = idx + key_len + len;
    if buf.len() < total {
        return Ok(None);
    }
    let mut payload = buf[idx + key_len..total].to_vec();
    if masked {
        let key = &buf[idx..idx + 4];
        for (i, b) in payload.iter_mut().enumerate() {
            *b ^= key[i % 4];
        }
    }
    Ok(Some((opcode, fin, payload, total)))
}

/// One text frame is one command: a bare line (`status`) or the HTTP-style
/// JSON `{"command": "..."}`.
fn ws_command(payload: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload)
        && let Some(c) = v.get("command").and_then(serde_json::Value::as_str)
    {
        return c.to_string();
    }
    payload.trim().to_string()
}

/// Send a `close` frame with `code` and finish the connection.
async fn ws_close(socket: &mut TcpStream, code: u16) -> Result<(), String> {
    socket
        .write_all(&encode_ws_frame(0x8, &code.to_be_bytes()))
        .await
        .map_err(|e| format!("write: {e}"))
}

/// Serve one WebSocket connection: handshake, then a frame loop where each
/// text frame is dispatched like a telnet line and answered with the JSON
/// envelope. `exit`/`quit` and a client `close` end the connection.
async fn handle_ws_connection(
    mut socket: TcpStream,
    jingles: &[PathBuf],
    queue: Option<Arc<RequestQueue>>,
    tx: mpsc::Sender<MixCommand>,
    status: &StatusHandle,
    custom: &[String],
    event_tx: &mpsc::Sender<ScriptEvent>,
) -> Result<(), String> {
    let (head, rest) = match read_head_block(&mut socket).await {
        Ok(h) => h,
        Err((code, body)) => {
            write_http(&mut socket, code, &body).await?;
            return Ok(());
        }
    };
    if !head.starts_with("GET ") {
        write_http(&mut socket, 405, &http_error("websocket upgrade must be GET")).await?;
        return Ok(());
    }
    if !ws_header(&head, "Upgrade").is_some_and(|v| v.eq_ignore_ascii_case("websocket")) {
        write_http(&mut socket, 400, &http_error("missing Upgrade: websocket")).await?;
        return Ok(());
    }
    let key = match ws_header(&head, "Sec-WebSocket-Key") {
        Some(k) => k.to_string(),
        None => {
            write_http(&mut socket, 400, &http_error("missing Sec-WebSocket-Key")).await?;
            return Ok(());
        }
    };
    let accept = ws_accept(&key);
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    socket
        .write_all(resp.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;

    let mut rng = SmallRng::from_entropy();
    // A client may pipeline its first frame right after the handshake in
    // the same TCP segment; those bytes came back from `read_head_block`.
    let mut buf = rest;
    let mut chunk = [0u8; 4096];
    loop {
        match parse_ws_frame(&buf) {
            Ok(Some((opcode, fin, payload, consumed))) => {
                buf.drain(..consumed);
                match opcode {
                    0x8 => {
                        let code = if payload.len() >= 2 {
                            u16::from_be_bytes([payload[0], payload[1]])
                        } else {
                            WS_CLOSE_NORMAL
                        };
                        ws_close(&mut socket, code).await?;
                        return Ok(());
                    }
                    0x9 => {
                        socket
                            .write_all(&encode_ws_frame(0xA, &payload))
                            .await
                            .map_err(|e| format!("write: {e}"))?;
                    }
                    0xA => {}
                    0x1 => {
                        if !fin {
                            ws_close(&mut socket, 1003).await?;
                            return Ok(());
                        }
                        let command = ws_command(&String::from_utf8_lossy(&payload));
                        if command.is_empty() {
                            let reply = CommandReply::Err("usage: <command>".into()).json();
                            socket
                                .write_all(&encode_ws_frame(0x1, reply.as_bytes()))
                                .await
                                .map_err(|e| format!("write: {e}"))?;
                            continue;
                        }
                        let ctx = DispatchCtx {
                            jingles,
                            queue: queue.as_deref(),
                            tx: &tx,
                            status,
                            custom,
                            event_tx,
                        };
                        match dispatch(&command, &ctx, &mut rng) {
                            CommandResult::Reply(r) => {
                                let reply = r.json();
                                socket
                                    .write_all(&encode_ws_frame(0x1, reply.as_bytes()))
                                    .await
                                    .map_err(|e| format!("write: {e}"))?;
                            }
                            CommandResult::Exit => {
                                ws_close(&mut socket, WS_CLOSE_NORMAL).await?;
                                return Ok(());
                            }
                        }
                    }
                    // continuation or binary: a control channel is text-only.
                    _ => {
                        ws_close(&mut socket, 1003).await?;
                        return Ok(());
                    }
                }
            }
            Ok(None) => {
                let n = socket
                    .read(&mut chunk)
                    .await
                    .map_err(|e| format!("read: {e}"))?;
                if n == 0 {
                    return Ok(());
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(code) => {
                ws_close(&mut socket, code).await?;
                return Ok(());
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    socket: TcpStream,
    banner: bool,
    jingles: &[PathBuf],
    queue: Option<Arc<RequestQueue>>,
    tx: mpsc::Sender<MixCommand>,
    status: &StatusHandle,
    custom: &[String],
    event_tx: &mpsc::Sender<ScriptEvent>,
) -> Result<(), String> {
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    let mut rng = SmallRng::from_entropy();

    if banner {
        reply(
            &mut writer,
            "welcome to the crabsoup control port (help for commands)",
        )
        .await?;
    }

    loop {
        line.clear();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            return Ok(());
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        let (json_mode, cmd) = split_json_prefix(cmd);
        if cmd.is_empty() {
            reply(
                &mut writer,
                &CommandReply::Err("usage: json <command>".into()).json(),
            )
            .await?;
            continue;
        }
        let ctx = DispatchCtx {
            jingles,
            queue: queue.as_deref(),
            tx: &tx,
            status,
            custom,
            event_tx,
        };
        match dispatch(cmd, &ctx, &mut rng) {
            CommandResult::Reply(r) => {
                let text = if json_mode { r.json() } else { r.text() };
                reply(&mut writer, &text).await?;
            }
            CommandResult::Exit => return Ok(()),
        }
    }
}

/// `json <command>` selects a single-line JSON reply for that line; the
/// name `json` is reserved (cannot be a `server.register` command).
fn split_json_prefix(cmd: &str) -> (bool, &str) {
    match cmd.strip_prefix("json") {
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => {
            (true, rest.trim())
        }
        _ => (false, cmd),
    }
}

enum CommandResult {
    Reply(CommandReply),
    Exit,
}

/// Structured outcome of a control command, rendered to either the
/// human-readable telnet protocol (`text`) or single-line JSON (`json`).
/// Text output stays byte-identical to the legacy protocol; JSON output is
/// a single object per reply: `{"ok": true, ...}` or
/// `{"ok": false, "error": "..."}`.
enum CommandReply {
    /// Simple success message rendered verbatim ("skipping", help text).
    Ok(String),
    /// Error message rendered verbatim ("ERROR: ...", "usage: ...",
    /// "unknown command: ...").
    Err(String),
    /// Opaque reply from a `server.register` Lua handler.
    Custom(String),
    /// `status`: current track + uptime + live-DJ (harbor) state.
    Status {
        playing: String,
        uptime_seconds: u64,
        harbor_connected: bool,
    },
    /// `uptime`.
    Uptime(u64),
    /// `queue.push <path>`: the queued path and the new queue length.
    Queued { path: String, length: usize },
    /// `queue.list` / `jingles.list`: items under `key`; `empty` is the
    /// text-mode reply when there are none.
    List {
        key: &'static str,
        empty: &'static str,
        items: Vec<String>,
    },
    /// `jingles.play ...`: the chosen jingle path.
    Playing(String),
}

impl CommandReply {
    fn text(&self) -> String {
        match self {
            CommandReply::Ok(msg) | CommandReply::Err(msg) | CommandReply::Custom(msg) => {
                msg.clone()
            }
            CommandReply::Status {
                playing,
                uptime_seconds,
                harbor_connected,
            } => format!("playing: {playing}\nuptime: {uptime_seconds}s\nlive: {harbor_connected}"),
            CommandReply::Uptime(secs) => format!("uptime: {secs}s"),
            CommandReply::Queued { path, length } => format!("queued {path} ({length})"),
            CommandReply::List { items, empty, .. } => {
                if items.is_empty() {
                    (*empty).to_string()
                } else {
                    items
                        .iter()
                        .enumerate()
                        .map(|(i, item)| format!("{i}: {item}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            CommandReply::Playing(path) => format!("playing {path}"),
        }
    }

    fn json(&self) -> String {
        match self {
            CommandReply::Ok(msg) => serde_json::json!({ "ok": true, "message": msg }).to_string(),
            CommandReply::Err(msg) => serde_json::json!({ "ok": false, "error": msg }).to_string(),
            CommandReply::Custom(msg) => {
                serde_json::json!({ "ok": true, "reply": msg }).to_string()
            }
            CommandReply::Status {
                playing,
                uptime_seconds,
                harbor_connected,
            } => serde_json::json!({
                "ok": true,
                "playing": playing,
                "uptime_seconds": uptime_seconds,
                "harbor_connected": harbor_connected
            })
            .to_string(),
            CommandReply::Uptime(secs) => {
                serde_json::json!({ "ok": true, "uptime_seconds": secs }).to_string()
            }
            CommandReply::Queued { path, length } => {
                serde_json::json!({ "ok": true, "queued": path, "length": length }).to_string()
            }
            CommandReply::List { key, items, .. } => {
                let mut obj = serde_json::Map::new();
                obj.insert("ok".into(), serde_json::Value::Bool(true));
                obj.insert(
                    (*key).into(),
                    serde_json::Value::Array(
                        items
                            .iter()
                            .cloned()
                            .map(serde_json::Value::String)
                            .collect(),
                    ),
                );
                serde_json::Value::Object(obj).to_string()
            }
            CommandReply::Playing(path) => {
                serde_json::json!({ "ok": true, "playing": path }).to_string()
            }
        }
    }
}

/// Everything `dispatch` needs besides the raw command line. Kept in one
/// struct so call sites (connection task, tests) stay short.
struct DispatchCtx<'a> {
    jingles: &'a [PathBuf],
    queue: Option<&'a RequestQueue>,
    tx: &'a mpsc::Sender<MixCommand>,
    status: &'a StatusHandle,
    custom: &'a [String],
    event_tx: &'a mpsc::Sender<ScriptEvent>,
}

fn dispatch(cmd: &str, ctx: &DispatchCtx, rng: &mut SmallRng) -> CommandResult {
    let mut parts = cmd.split_whitespace();
    let verb = parts.next().unwrap_or("");
    match verb {
        "help" => CommandResult::Reply(CommandReply::Ok(help_text().into())),
        "exit" | "quit" => CommandResult::Exit,
        "skip" => {
            log::info!("control port: skip requested");
            let _ = ctx.tx.send(MixCommand::Skip);
            CommandResult::Reply(CommandReply::Ok("skipping".into()))
        }
        "queue.push" => match parts.next() {
            Some(path) => match ctx.queue {
                Some(q) => {
                    q.push(crate::request::RequestUri::new(path));
                    CommandResult::Reply(CommandReply::Queued {
                        path: path.into(),
                        length: q.len(),
                    })
                }
                None => CommandResult::Reply(CommandReply::Err(
                    "ERROR: no request.queue source in script".into(),
                )),
            },
            None => CommandResult::Reply(CommandReply::Err("usage: queue.push <path>".into())),
        },
        "queue.list" => match ctx.queue {
            Some(q) => CommandResult::Reply(CommandReply::List {
                key: "queue",
                empty: "queue empty",
                items: q.list().iter().map(|uri| uri.raw().to_string()).collect(),
            }),
            None => CommandResult::Reply(CommandReply::Err(
                "ERROR: no request.queue source in script".into(),
            )),
        },
        "queue.clear" => match ctx.queue {
            Some(q) => {
                q.clear();
                CommandResult::Reply(CommandReply::Ok("queue cleared".into()))
            }
            None => CommandResult::Reply(CommandReply::Err(
                "ERROR: no request.queue source in script".into(),
            )),
        },
        "queue.skip" => match ctx.queue {
            Some(q) => {
                q.request_skip();
                CommandResult::Reply(CommandReply::Ok("skipping queued track".into()))
            }
            None => CommandResult::Reply(CommandReply::Err(
                "ERROR: no request.queue source in script".into(),
            )),
        },
        "status" => CommandResult::Reply(CommandReply::Status {
            playing: ctx.status.current(),
            uptime_seconds: ctx.status.uptime_seconds(),
            harbor_connected: ctx.status.harbor_connected(),
        }),
        "uptime" => CommandResult::Reply(CommandReply::Uptime(ctx.status.uptime_seconds())),
        "shutdown" => {
            log::info!("control port: shutdown requested");
            let _ = ctx.tx.send(MixCommand::Shutdown);
            CommandResult::Reply(CommandReply::Ok("shutting down".into()))
        }
        "jingles.list" => CommandResult::Reply(CommandReply::List {
            key: "jingles",
            empty: "no jingles configured",
            items: ctx
                .jingles
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
        }),
        "jingles.play" => {
            let path = match parts.next() {
                Some(arg) => match pick_jingle(ctx.jingles, arg) {
                    Ok(i) => ctx.jingles[i].clone(),
                    Err(e) => {
                        return CommandResult::Reply(CommandReply::Err(format!("ERROR: {e}")));
                    }
                },
                None => {
                    if ctx.jingles.is_empty() {
                        return CommandResult::Reply(CommandReply::Err(
                            "ERROR: no jingles configured".into(),
                        ));
                    }
                    let idx = rng.gen_range(0..ctx.jingles.len());
                    ctx.jingles[idx].clone()
                }
            };
            play_jingle(&path, ctx.tx)
        }
        _ => match ctx.custom.iter().position(|n| *n == verb) {
            Some(index) => {
                let args = parts.collect::<Vec<_>>().join(" ");
                let (reply_tx, reply_rx) = mpsc::channel();
                let event = ScriptEvent::Custom {
                    index,
                    args,
                    reply: reply_tx,
                };
                if ctx.event_tx.send(event).is_err() {
                    return CommandResult::Reply(CommandReply::Err(
                        "ERROR: script event loop is not running".into(),
                    ));
                }
                match reply_rx.recv_timeout(Duration::from_secs(5)) {
                    Ok(Ok(text)) => CommandResult::Reply(CommandReply::Custom(text)),
                    Ok(Err(e)) => CommandResult::Reply(CommandReply::Err(format!("ERROR: {e}"))),
                    Err(_) => CommandResult::Reply(CommandReply::Err(
                        "ERROR: custom command timed out".into(),
                    )),
                }
            }
            None => CommandResult::Reply(CommandReply::Err(format!(
                "unknown command: {cmd} (help for commands)"
            ))),
        },
    }
}

fn help_text() -> &'static str {
    "commands: jingles.list | jingles.play [n|substr] | queue.push <path> | queue.list | queue.clear | queue.skip | skip | status | uptime | shutdown | <custom commands> | json <command> | exit | help"
}

fn pick_jingle(jingles: &[PathBuf], arg: &str) -> Result<usize, String> {
    if let Ok(idx) = arg.parse::<usize>() {
        return jingles
            .get(idx)
            .map(|_| idx)
            .ok_or_else(|| format!("jingle index out of range (0..{})", jingles.len()));
    }
    let needle = arg.to_ascii_lowercase();
    jingles
        .iter()
        .position(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_ascii_lowercase().contains(&needle))
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("no jingle matching \"{arg}\""))
}

fn play_jingle(path: &Path, tx: &mpsc::Sender<MixCommand>) -> CommandResult {
    if !path.exists() {
        return CommandResult::Reply(CommandReply::Err(format!(
            "ERROR: jingle missing: {}",
            path.display()
        )));
    }
    let _ = tx.send(MixCommand::PlayJingle(path.to_path_buf()));
    log::info!("control port: playing jingle {}", path.display());
    CommandResult::Reply(CommandReply::Playing(path.display().to_string()))
}

async fn reply(writer: &mut tokio::net::tcp::OwnedWriteHalf, text: &str) -> Result<(), String> {
    writer
        .write_all(text.as_bytes())
        .await
        .map_err(|e| format!("write: {e}"))?;
    writer
        .write_all(b"\n")
        .await
        .map_err(|e| format!("write: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn jingles() -> Vec<PathBuf> {
        vec![
            PathBuf::from("internal/jingles/a-intro.mp3"),
            PathBuf::from("internal/jingles/b-sting.wav"),
        ]
    }

    #[test]
    fn pick_by_index() {
        assert_eq!(pick_jingle(&jingles(), "0").unwrap(), 0);
        assert_eq!(pick_jingle(&jingles(), "1").unwrap(), 1);
        assert!(pick_jingle(&jingles(), "2").is_err());
    }

    #[test]
    fn pick_by_substring_is_case_insensitive() {
        assert_eq!(pick_jingle(&jingles(), "STING").unwrap(), 1);
        assert_eq!(pick_jingle(&jingles(), "intro").unwrap(), 0);
        assert!(pick_jingle(&jingles(), "bass").is_err());
    }

    #[test]
    fn skip_sends_the_mix_command() {
        let (tx, rx) = mpsc::channel();
        let status = StatusHandle::new();
        let ctx = DispatchCtx {
            jingles: &jingles(),
            queue: None,
            tx: &tx,
            status: &status,
            custom: &[],
            event_tx: &mpsc::channel().0,
        };
        let reply = dispatch("skip", &ctx, &mut SmallRng::from_entropy());
        match reply {
            CommandResult::Reply(r) => {
                assert_eq!(r.text(), "skipping");
                let v: serde_json::Value = serde_json::from_str(&r.json()).unwrap();
                assert_eq!(v, serde_json::json!({ "ok": true, "message": "skipping" }));
            }
            CommandResult::Exit => panic!("skip must reply"),
        }
        assert!(matches!(rx.try_recv(), Ok(MixCommand::Skip)));
    }

    #[test]
    fn status_and_uptime_report_engine_state() {
        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        status.set_current("some track");
        let event_tx = mpsc::channel().0;
        let ctx = DispatchCtx {
            jingles: &jingles(),
            queue: None,
            tx: &tx,
            status: &status,
            custom: &[],
            event_tx: &event_tx,
        };
        match dispatch("status", &ctx, &mut SmallRng::from_entropy()) {
            CommandResult::Reply(r) => {
                assert!(r.text().contains("playing: some track"));
                assert!(r.text().contains("uptime: "));
                assert!(r.text().contains("live: false"));
                let v: serde_json::Value = serde_json::from_str(&r.json()).unwrap();
                assert_eq!(v["harbor_connected"], serde_json::Value::Bool(false));
            }
            CommandResult::Exit => panic!("status must reply"),
        }
        // Toggling the shared harbor flag is visible in the reply.
        status.harbor_flag().store(true, Ordering::SeqCst);
        match dispatch("status", &ctx, &mut SmallRng::from_entropy()) {
            CommandResult::Reply(r) => {
                assert!(r.text().contains("live: true"));
                let v: serde_json::Value = serde_json::from_str(&r.json()).unwrap();
                assert_eq!(v["harbor_connected"], serde_json::Value::Bool(true));
            }
            CommandResult::Exit => panic!("status must reply"),
        }
        match dispatch("uptime", &ctx, &mut SmallRng::from_entropy()) {
            CommandResult::Reply(r) => assert!(r.text().starts_with("uptime: ")),
            CommandResult::Exit => panic!("uptime must reply"),
        }
    }

    #[test]
    fn queue_commands_push_list_clear_and_skip() {
        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        let queue = Arc::new(RequestQueue::new());
        let mut rng = SmallRng::from_entropy();
        let event_tx = mpsc::channel().0;
        let ctx = DispatchCtx {
            jingles: &jingles(),
            queue: Some(&queue),
            tx: &tx,
            status: &status,
            custom: &[],
            event_tx: &event_tx,
        };

        match dispatch("queue.push /tmp/a.mp3", &ctx, &mut rng) {
            CommandResult::Reply(r) => {
                assert!(r.text().contains("queued /tmp/a.mp3 (1)"), "{}", r.text())
            }
            CommandResult::Exit => panic!("queue.push must reply"),
        }
        match dispatch("queue.push /tmp/b.mp3", &ctx, &mut rng) {
            CommandResult::Reply(r) => assert!(r.text().contains("(2)"), "{}", r.text()),
            CommandResult::Exit => panic!("queue.push must reply"),
        }
        match dispatch("queue.list", &ctx, &mut rng) {
            CommandResult::Reply(r) => {
                assert!(r.text().contains("0: /tmp/a.mp3"));
                assert!(r.text().contains("1: /tmp/b.mp3"));
            }
            CommandResult::Exit => panic!("queue.list must reply"),
        }
        match dispatch("queue.skip", &ctx, &mut rng) {
            CommandResult::Reply(r) => {
                assert!(r.text().contains("skipping queued track"), "{}", r.text())
            }
            CommandResult::Exit => panic!("queue.skip must reply"),
        }
        match dispatch("queue.clear", &ctx, &mut rng) {
            CommandResult::Reply(r) => assert!(r.text().contains("cleared"), "{}", r.text()),
            CommandResult::Exit => panic!("queue.clear must reply"),
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn queue_commands_require_a_configured_queue() {
        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        let mut rng = SmallRng::from_entropy();
        let event_tx = mpsc::channel().0;
        let ctx = DispatchCtx {
            jingles: &jingles(),
            queue: None,
            tx: &tx,
            status: &status,
            custom: &[],
            event_tx: &event_tx,
        };
        for cmd in [
            "queue.push /tmp/a.mp3",
            "queue.list",
            "queue.skip",
            "queue.clear",
        ] {
            match dispatch(cmd, &ctx, &mut rng) {
                CommandResult::Reply(r) => {
                    assert!(
                        r.text().contains("no request.queue source"),
                        "{cmd}: {}",
                        r.text()
                    )
                }
                CommandResult::Exit => panic!("{cmd} must reply"),
            }
        }
    }

    #[test]
    fn custom_command_routes_through_the_event_channel() {
        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        let custom = vec!["ping".to_string(), "greet".to_string()];
        let (event_tx, event_rx) = mpsc::channel();
        let handler = std::thread::spawn(move || {
            while let Ok(ScriptEvent::Custom { index, args, reply }) = event_rx.recv() {
                let text = match (index, args.as_str()) {
                    (0, "") => "pong".to_string(),
                    (0, a) => format!("pong {a}"),
                    (1, a) => format!("hello {a}"),
                    _ => unreachable!(),
                };
                let _ = reply.send(Ok(text));
            }
        });
        let mut rng = SmallRng::from_entropy();
        let ctx = DispatchCtx {
            jingles: &jingles(),
            queue: None,
            tx: &tx,
            status: &status,
            custom: &custom,
            event_tx: &event_tx,
        };

        match dispatch("ping", &ctx, &mut rng) {
            CommandResult::Reply(r) => {
                assert_eq!(r.text(), "pong");
                let v: serde_json::Value = serde_json::from_str(&r.json()).unwrap();
                assert_eq!(v, serde_json::json!({ "ok": true, "reply": "pong" }));
            }
            CommandResult::Exit => panic!("ping must reply"),
        }
        match dispatch("greet world", &ctx, &mut rng) {
            CommandResult::Reply(r) => assert_eq!(r.text(), "hello world"),
            CommandResult::Exit => panic!("greet must reply"),
        }
        drop(event_tx);
        let _ = handler.join();
    }

    #[test]
    fn unregistered_commands_are_still_unknown() {
        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        let event_tx = mpsc::channel().0;
        let ctx = DispatchCtx {
            jingles: &jingles(),
            queue: None,
            tx: &tx,
            status: &status,
            custom: &[],
            event_tx: &event_tx,
        };
        match dispatch("ping", &ctx, &mut SmallRng::from_entropy()) {
            CommandResult::Reply(r) => {
                assert!(r.text().contains("unknown command"), "{}", r.text())
            }
            CommandResult::Exit => panic!("must reply"),
        }
    }

    #[test]
    fn split_json_prefix_reserves_the_json_name() {
        assert_eq!(split_json_prefix("json"), (true, ""));
        assert_eq!(split_json_prefix("json status"), (true, "status"));
        assert_eq!(split_json_prefix("json   queue.list"), (true, "queue.list"));
        assert_eq!(split_json_prefix("status"), (false, "status"));
        assert_eq!(split_json_prefix("jsonify"), (false, "jsonify"));
        assert_eq!(
            split_json_prefix("queue.push /tmp/a.mp3"),
            (false, "queue.push /tmp/a.mp3")
        );
    }

    #[test]
    fn json_replies_are_single_line_and_parseable() {
        use serde_json::Value;

        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        status.set_current("some track");
        let queue = Arc::new(RequestQueue::new());
        queue.push(crate::request::RequestUri::new("/tmp/a.mp3"));
        queue.push(crate::request::RequestUri::new("/tmp/b.mp3"));
        let mut rng = SmallRng::from_entropy();
        let event_tx = mpsc::channel().0;
        let ctx = DispatchCtx {
            jingles: &jingles(),
            queue: Some(&queue),
            tx: &tx,
            status: &status,
            custom: &[],
            event_tx: &event_tx,
        };

        // Structured fields round-trip exactly.
        match dispatch("queue.list", &ctx, &mut rng) {
            CommandResult::Reply(r) => {
                let v: Value = serde_json::from_str(&r.json()).unwrap();
                assert_eq!(v["queue"], serde_json::json!(["/tmp/a.mp3", "/tmp/b.mp3"]));
            }
            CommandResult::Exit => panic!(),
        }
        match dispatch("status", &ctx, &mut rng) {
            CommandResult::Reply(r) => {
                let v: Value = serde_json::from_str(&r.json()).unwrap();
                assert_eq!(v["playing"], "some track");
                assert!(v["uptime_seconds"].is_number());
            }
            CommandResult::Exit => panic!(),
        }

        let cases = [
            ("status", "playing"),
            ("uptime", "uptime_seconds"),
            ("queue.list", "queue"),
            ("jingles.list", "jingles"),
            ("skip", "message"),
            ("queue.push", "error"),
            ("bogus", "error"),
        ];
        for (cmd, field) in cases {
            let line = format!("json {cmd}");
            let (json_mode, rest) = split_json_prefix(&line);
            assert!(json_mode);
            match dispatch(rest, &ctx, &mut rng) {
                CommandResult::Reply(r) => {
                    let text = r.json();
                    assert!(
                        !text.contains('\n'),
                        "{cmd}: json must be single-line: {text}"
                    );
                    let v: Value = serde_json::from_str(&text).expect("valid json");
                    let obj = v.as_object().expect("json object");
                    assert!(obj.contains_key("ok"), "{cmd}: missing ok: {text}");
                    assert!(obj.contains_key(field), "{cmd}: missing {field}: {text}");
                }
                CommandResult::Exit => panic!("{cmd} must reply"),
            }
        }

        // queue.push reports the queued path and the new length.
        match dispatch("queue.push /tmp/c.mp3", &ctx, &mut rng) {
            CommandResult::Reply(r) => {
                let v: Value = serde_json::from_str(&r.json()).unwrap();
                assert_eq!(v["queued"], "/tmp/c.mp3");
                assert_eq!(v["length"], 3);
                assert!(v["ok"].as_bool().unwrap());
            }
            CommandResult::Exit => panic!(),
        }
    }

    #[test]
    fn json_escapes_quotes_newlines_and_backslashes() {
        use serde_json::Value;

        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        status.set_current("a \"quoted\" title \\ with \n newline");
        let ctx = DispatchCtx {
            jingles: &[],
            queue: None,
            tx: &tx,
            status: &status,
            custom: &[],
            event_tx: &mpsc::channel().0,
        };
        match dispatch("status", &ctx, &mut SmallRng::from_entropy()) {
            CommandResult::Reply(r) => {
                let text = r.json();
                assert!(!text.contains('\n'), "must stay single-line: {text}");
                let v: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v["playing"], "a \"quoted\" title \\ with \n newline");
            }
            CommandResult::Exit => panic!(),
        }
    }

    #[test]
    fn parse_request_head_reads_method_and_path() {
        assert_eq!(
            parse_request_head("GET /status HTTP/1.1\r\nHost: localhost\r\n").unwrap(),
            ("GET".into(), "/status".into())
        );
        assert_eq!(
            parse_request_head("POST /cmd HTTP/1.1\r\nContent-Length: 5\r\n").unwrap(),
            ("POST".into(), "/cmd".into())
        );
        assert!(parse_request_head("garbage").is_err());
        assert!(parse_request_head("GET /status").is_err()); // missing version
    }

    #[test]
    fn content_length_is_case_insensitive_and_optional() {
        assert_eq!(
            content_length("GET / HTTP/1.1\r\nContent-Length: 12\r\n"),
            Some(12)
        );
        assert_eq!(
            content_length("GET / HTTP/1.1\r\ncontent-length: 4\r\n"),
            Some(4)
        );
        assert_eq!(content_length("GET / HTTP/1.1\r\nHost: x\r\n"), None);
        assert_eq!(
            content_length("GET / HTTP/1.1\r\nContent-Length: nope\r\n"),
            None
        );
    }

    #[test]
    fn http_route_serves_status_and_commands() {
        use serde_json::Value;

        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        status.set_current("on air");
        let queue = Arc::new(RequestQueue::new());
        queue.push(crate::request::RequestUri::new("/tmp/a.mp3"));
        let j = jingles();
        let mut rng = SmallRng::from_entropy();
        let event_tx = mpsc::channel().0;
        let ctx = DispatchCtx {
            jingles: &j,
            queue: Some(&queue),
            tx: &tx,
            status: &status,
            custom: &[],
            event_tx: &event_tx,
        };

        let (code, body) = http_route("GET", "/status", "", &ctx, &mut rng);
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["playing"], "on air");

        let (code, body) = http_route("GET", "/queue", "", &ctx, &mut rng);
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["queue"], serde_json::json!(["/tmp/a.mp3"]));

        let (code, body) = http_route("GET", "/jingles", "", &ctx, &mut rng);
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            v["jingles"],
            serde_json::json!(["internal/jingles/a-intro.mp3", "internal/jingles/b-sting.wav"])
        );

        let (code, body) = http_route("POST", "/cmd", r#"{"command":"skip"}"#, &ctx, &mut rng);
        assert_eq!(code, 200);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["message"], "skipping");

        // App-level errors (unknown command, missing jingle file) are 400.
        let (code, body) = http_route("POST", "/cmd", r#"{"command":"bogus"}"#, &ctx, &mut rng);
        assert_eq!(code, 400);
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["ok"], false);
        assert!(v["error"].as_str().unwrap().contains("unknown command"));
        let (code, body) = http_route(
            "POST",
            "/cmd",
            r#"{"command":"jingles.play 0"}"#,
            &ctx,
            &mut rng,
        );
        assert_eq!(code, 400);
        assert!(body.contains("jingle missing"));

        // Malformed bodies, unknown routes, wrong methods.
        let (code, body) = http_route("POST", "/cmd", "not json", &ctx, &mut rng);
        assert_eq!(code, 400);
        assert!(body.contains("invalid JSON"));
        let (code, body) = http_route("POST", "/cmd", "{}", &ctx, &mut rng);
        assert_eq!(code, 400);
        assert!(body.contains("command"));
        let (code, _) = http_route("GET", "/nope", "", &ctx, &mut rng);
        assert_eq!(code, 404);
        let (code, _) = http_route("PUT", "/status", "", &ctx, &mut rng);
        assert_eq!(code, 405);

        // `exit` is a telnet concept; over HTTP it just acks.
        let (code, body) = http_route("POST", "/cmd", r#"{"command":"exit"}"#, &ctx, &mut rng);
        assert_eq!(code, 200);
        assert!(body.contains("bye"));
    }

    #[test]
    fn ws_accept_matches_rfc_vector() {
        // RFC 6455 §1.3 worked example.
        assert_eq!(
            ws_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn ws_frame_roundtrip_and_masking() {
        // Unmasked server->client frame round-trips.
        let bytes = encode_ws_frame(0x1, b"{\"ok\":true}");
        assert_eq!(bytes[0], 0x81, "FIN + text opcode");
        assert_eq!(bytes[1], 11, "payload len 11, no mask bit");
        match parse_ws_frame(&bytes).unwrap() {
            Some((opcode, fin, payload, consumed)) => {
                assert_eq!(opcode, 0x1);
                assert!(fin);
                assert_eq!(payload, b"{\"ok\":true}");
                assert_eq!(consumed, bytes.len());
            }
            None => panic!("complete frame must parse"),
        }

        // A masked client frame is unmasked on parse. Hand-build one: mask
        // bit set, 4-byte key `0x11 0x22 0x33 0x44`, payload xor the key.
        let payload = b"status";
        let key = [0x11, 0x22, 0x33, 0x44];
        let mut masked = vec![0x81, 0x80 | payload.len() as u8];
        masked.extend_from_slice(&key);
        masked.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        match parse_ws_frame(&masked).unwrap() {
            Some((0x1, true, out, consumed)) => {
                assert_eq!(out, payload);
                assert_eq!(consumed, masked.len());
            }
            _ => panic!("masked frame must parse unmasked"),
        }
    }

    #[test]
    fn ws_frame_extended_lengths_and_control_limits() {
        // 126-byte payload uses the 16-bit extended length.
        let big = vec![b'x'; 126];
        let bytes = encode_ws_frame(0x1, &big);
        assert_eq!(bytes[1], 126);
        match parse_ws_frame(&bytes).unwrap() {
            Some((0x1, true, out, _)) => assert_eq!(out, big),
            _ => panic!("126-len frame must parse"),
        }

        // 65536-byte payload uses the 64-bit extended length.
        let huge = vec![b'y'; 65_536];
        let bytes = encode_ws_frame(0x1, &huge);
        assert_eq!(bytes[1], 127);
        match parse_ws_frame(&bytes).unwrap() {
            Some((0x1, true, out, _)) => assert_eq!(out, huge),
            _ => panic!("64-bit-len frame must parse"),
        }

        // Control frames must be short and unfragmented; RSV bits are a
        // protocol error (close 1002). Fragmented *data* frames parse fine
        // at the codec level (the connection loop closes 1003 on them).
        let mut bad = vec![0x81, 0x05]; // len 5 is fine but RSV1 set
        bad[0] |= 0x40;
        assert_eq!(parse_ws_frame(&bad).unwrap_err(), 1002);
        let mut fragmented_ping = vec![0x09, 0x02]; // ping, FIN=0
        fragmented_ping.extend_from_slice(b"hi");
        assert_eq!(parse_ws_frame(&fragmented_ping).unwrap_err(), 1002);
        let mut fragmented_text = vec![0x01, 0x05]; // text, FIN=0
        fragmented_text.extend_from_slice(b"hello");
        assert!(parse_ws_frame(&fragmented_text).unwrap().is_some());
        // Oversized frame -> close 1009.
        let mut huge_len = vec![0x81, 127, 0, 0, 0, 0, 0, 0, 0, 0];
        let _ = &mut huge_len[..0];
        let bytes = encode_ws_frame(0x1, &vec![0u8; MAX_WS_FRAME + 1]);
        // 64-bit length header declares the real size; parsing it alone
        // reports 1009 before needing the payload.
        let header = &bytes[..10];
        assert_eq!(parse_ws_frame(header).unwrap_err(), 1009);
    }

    #[test]
    fn ws_command_accepts_bare_and_json() {
        assert_eq!(ws_command("status"), "status");
        assert_eq!(ws_command("  skip  "), "skip");
        assert_eq!(ws_command(r#"{"command":"status"}"#), "status");
        assert_eq!(ws_command(r#"{"command":"jingles.play 0"}"#), "jingles.play 0");
        // Bare text that happens to be JSON (e.g. a malformed envelope)
        // falls through to the raw line.
        assert_eq!(ws_command("not json"), "not json");
    }

    #[test]
    fn ws_end_to_end_dispatch() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, _rx) = mpsc::channel();
            let status = StatusHandle::new();
            status.set_current("ws track");
            let jingles = jingles();
            let custom: Vec<String> = vec![];
            let (event_tx, _event_rx) = mpsc::channel();
            let server = tokio::spawn(async move {
                let (socket, _peer) = listener.accept().await.unwrap();
                handle_ws_connection(socket, &jingles, None, tx, &status, &custom, &event_tx)
                    .await
            });

            let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
            // Handshake with a masked `status` frame pipelined in the same
            // segment (the reply must still arrive after the 101).
            let payload = b"status";
            let key = [0xde, 0xad, 0xbe, 0xef];
            let mut frame = vec![0x81, 0x80 | payload.len() as u8];
            frame.extend_from_slice(&key);
            frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
            let mut handshake = b"GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
                                Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                                Sec-WebSocket-Version: 13\r\n\r\n"
                .to_vec();
            handshake.extend_from_slice(&frame);
            client.write_all(&handshake).await.unwrap();

            let mut head = Vec::new();
            let mut chunk = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = client.read(&mut chunk).await.unwrap();
                head.extend_from_slice(&chunk[..n]);
            }
            let resp = String::from_utf8_lossy(&head);
            assert!(resp.starts_with("HTTP/1.1 101"), "{resp}");
            assert!(
                resp.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="),
                "{resp}"
            );

            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            let reply = loop {
                if let Ok(Some((0x1, true, payload, _))) = parse_ws_frame(&buf) {
                    break payload;
                }
                let n = client.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
            };
            let v: serde_json::Value = serde_json::from_slice(&reply).unwrap();
            assert_eq!(v["ok"], true);
            assert_eq!(v["playing"], "ws track");

            // Ping -> pong keeps the connection alive; `exit` closes it.
            client.write_all(&encode_ws_frame(0x9, b"ping")).await.unwrap();
            let mut pong = Vec::new();
            let got_pong = loop {
                if let Ok(Some((0xA, true, p, _))) = parse_ws_frame(&pong) {
                    break p;
                }
                let n = client.read(&mut chunk).await.unwrap();
                pong.extend_from_slice(&chunk[..n]);
            };
            assert_eq!(got_pong, b"ping");

            client
                .write_all(&encode_ws_frame(0x1, b"exit"))
                .await
                .unwrap();
            let mut close_buf = Vec::new();
            let closed = loop {
                if let Ok(Some((0x8, true, p, _))) = parse_ws_frame(&close_buf) {
                    break p;
                }
                let n = client.read(&mut chunk).await.unwrap();
                close_buf.extend_from_slice(&chunk[..n]);
            };
            assert_eq!(closed, WS_CLOSE_NORMAL.to_be_bytes());
            server.await.unwrap().unwrap();
        });
    }
}
