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
//! reserved and cannot be a `server.register` command.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
            let jingles = self.jingles.clone();
            let queue = self.queue.clone();
            let tx = self.tx.clone();
            let status = self.status.clone();
            let custom = self.custom_commands.clone();
            let event_tx = self.event_tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, &jingles, queue, tx, &status, &custom, &event_tx).await
                {
                    log::warn!("control port ({peer}): {e}");
                }
            });
        }
    }
}

async fn handle_connection(
    socket: TcpStream,
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

    reply(&mut writer, "welcome to the crabsoup control port (help for commands)").await?;

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
        Some(rest) if rest.is_empty() || rest.starts_with(char::is_whitespace) => (true, rest.trim()),
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
    /// `status`: current track + uptime.
    Status {
        playing: String,
        uptime_seconds: u64,
    },
    /// `uptime`.
    Uptime(u64),
    /// `queue.push <path>`: the queued path and the new queue length.
    Queued {
        path: String,
        length: usize,
    },
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
            CommandReply::Ok(msg) | CommandReply::Err(msg) | CommandReply::Custom(msg) => msg.clone(),
            CommandReply::Status {
                playing,
                uptime_seconds,
            } => format!("playing: {playing}\nuptime: {uptime_seconds}s"),
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
            CommandReply::Custom(msg) => serde_json::json!({ "ok": true, "reply": msg }).to_string(),
            CommandReply::Status {
                playing,
                uptime_seconds,
            } => serde_json::json!({ "ok": true, "playing": playing, "uptime_seconds": uptime_seconds })
                .to_string(),
            CommandReply::Uptime(secs) => serde_json::json!({ "ok": true, "uptime_seconds": secs }).to_string(),
            CommandReply::Queued { path, length } => {
                serde_json::json!({ "ok": true, "queued": path, "length": length }).to_string()
            }
            CommandReply::List { key, items, .. } => {
                let mut obj = serde_json::Map::new();
                obj.insert("ok".into(), serde_json::Value::Bool(true));
                obj.insert(
                    (*key).into(),
                    serde_json::Value::Array(items.iter().cloned().map(serde_json::Value::String).collect()),
                );
                serde_json::Value::Object(obj).to_string()
            }
            CommandReply::Playing(path) => serde_json::json!({ "ok": true, "playing": path }).to_string(),
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
                None => {
                    CommandResult::Reply(CommandReply::Err("ERROR: no request.queue source in script".into()))
                }
            },
            None => CommandResult::Reply(CommandReply::Err("usage: queue.push <path>".into())),
        },
        "queue.list" => match ctx.queue {
            Some(q) => CommandResult::Reply(CommandReply::List {
                key: "queue",
                empty: "queue empty",
                items: q.list().iter().map(|uri| uri.raw().to_string()).collect(),
            }),
            None => CommandResult::Reply(CommandReply::Err("ERROR: no request.queue source in script".into())),
        },
        "queue.clear" => match ctx.queue {
            Some(q) => {
                q.clear();
                CommandResult::Reply(CommandReply::Ok("queue cleared".into()))
            }
            None => CommandResult::Reply(CommandReply::Err("ERROR: no request.queue source in script".into())),
        },
        "queue.skip" => match ctx.queue {
            Some(q) => {
                q.request_skip();
                CommandResult::Reply(CommandReply::Ok("skipping queued track".into()))
            }
            None => CommandResult::Reply(CommandReply::Err("ERROR: no request.queue source in script".into())),
        },
        "status" => CommandResult::Reply(CommandReply::Status {
            playing: ctx.status.current(),
            uptime_seconds: ctx.status.uptime_seconds(),
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
            items: ctx.jingles.iter().map(|p| p.display().to_string()).collect(),
        }),
        "jingles.play" => {
            let path = match parts.next() {
                Some(arg) => match pick_jingle(ctx.jingles, arg) {
                    Ok(i) => ctx.jingles[i].clone(),
                    Err(e) => return CommandResult::Reply(CommandReply::Err(format!("ERROR: {e}"))),
                },
                None => {
                    if ctx.jingles.is_empty() {
                        return CommandResult::Reply(CommandReply::Err("ERROR: no jingles configured".into()));
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
                    Err(_) => CommandResult::Reply(CommandReply::Err("ERROR: custom command timed out".into())),
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
        return CommandResult::Reply(CommandReply::Err(format!("ERROR: jingle missing: {}", path.display())));
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

    fn jingles() -> Vec<PathBuf> {
        vec![
            PathBuf::from("jingles/a-intro.mp3"),
            PathBuf::from("jingles/b-sting.wav"),
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
            CommandResult::Reply(r) => assert!(r.text().contains("queued /tmp/a.mp3 (1)"), "{}", r.text()),
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
            CommandResult::Reply(r) => assert!(r.text().contains("skipping queued track"), "{}", r.text()),
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
        for cmd in ["queue.push /tmp/a.mp3", "queue.list", "queue.skip", "queue.clear"] {
            match dispatch(cmd, &ctx, &mut rng) {
                CommandResult::Reply(r) => {
                    assert!(r.text().contains("no request.queue source"), "{cmd}: {}", r.text())
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
            CommandResult::Reply(r) => assert!(r.text().contains("unknown command"), "{}", r.text()),
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
        assert_eq!(split_json_prefix("queue.push /tmp/a.mp3"), (false, "queue.push /tmp/a.mp3"));
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
                    assert!(!text.contains('\n'), "{cmd}: json must be single-line: {text}");
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
}
