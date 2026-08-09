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
//! - `help`                    — list commands

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::config::ControlConfig;
use crate::engine::mixer::{MixCommand, StatusHandle};
use crate::source::request::RequestQueue;

/// Telnet command server. Owns one `mpsc::Sender` into the priority mixer.
pub struct ControlServer {
    config: ControlConfig,
    jingles: Vec<PathBuf>,
    queue: Option<Arc<RequestQueue>>,
    tx: mpsc::Sender<MixCommand>,
    status: StatusHandle,
}

impl ControlServer {
    pub fn new(
        config: ControlConfig,
        jingles: Vec<PathBuf>,
        queue: Option<Arc<RequestQueue>>,
        tx: mpsc::Sender<MixCommand>,
        status: StatusHandle,
    ) -> Self {
        Self {
            config,
            jingles,
            queue,
            tx,
            status,
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
            "control port listening on {addr} ({} jingle(s))",
            self.jingles.len()
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
            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, &jingles, queue, tx, &status).await {
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
        match dispatch(cmd, jingles, queue.as_deref(), &mut rng, &tx, status) {
            CommandResult::Reply(text) => reply(&mut writer, &text).await?,
            CommandResult::Exit => return Ok(()),
        }
    }
}

enum CommandResult {
    Reply(String),
    Exit,
}

fn dispatch(
    cmd: &str,
    jingles: &[PathBuf],
    queue: Option<&RequestQueue>,
    rng: &mut SmallRng,
    tx: &mpsc::Sender<MixCommand>,
    status: &StatusHandle,
) -> CommandResult {
    let mut parts = cmd.split_whitespace();
    let verb = parts.next().unwrap_or("");
    match verb {
        "help" => CommandResult::Reply(help_text().to_string()),
        "exit" | "quit" => CommandResult::Exit,
        "skip" => {
            log::info!("control port: skip requested");
            let _ = tx.send(MixCommand::Skip);
            CommandResult::Reply("skipping".into())
        }
        "queue.push" => match parts.next() {
            Some(path) => match queue {
                Some(q) => {
                    q.push(crate::request::RequestUri::new(path));
                    CommandResult::Reply(format!("queued {path} ({})", q.len()))
                }
                None => CommandResult::Reply("ERROR: no request.queue source in script".into()),
            },
            None => CommandResult::Reply("usage: queue.push <path>".into()),
        },
        "queue.list" => match queue {
            Some(q) => {
                let lines: Vec<String> = q
                    .list()
                    .iter()
                    .enumerate()
                    .map(|(i, uri)| format!("{i}: {}", uri.raw()))
                    .collect();
                if lines.is_empty() {
                    CommandResult::Reply("queue empty".into())
                } else {
                    CommandResult::Reply(lines.join("\n"))
                }
            }
            None => CommandResult::Reply("ERROR: no request.queue source in script".into()),
        },
        "queue.clear" => match queue {
            Some(q) => {
                q.clear();
                CommandResult::Reply("queue cleared".into())
            }
            None => CommandResult::Reply("ERROR: no request.queue source in script".into()),
        },
        "queue.skip" => match queue {
            Some(q) => {
                q.request_skip();
                CommandResult::Reply("skipping queued track".into())
            }
            None => CommandResult::Reply("ERROR: no request.queue source in script".into()),
        },
        "status" => CommandResult::Reply(format!(
            "playing: {}\nuptime: {}s",
            status.current(),
            status.uptime_seconds()
        )),
        "uptime" => CommandResult::Reply(format!("uptime: {}s", status.uptime_seconds())),
        "shutdown" => {
            log::info!("control port: shutdown requested");
            let _ = tx.send(MixCommand::Shutdown);
            CommandResult::Reply("shutting down".into())
        }
        "jingles.list" => CommandResult::Reply(list_jingles(jingles)),
        "jingles.play" => {
            let path = match parts.next() {
                Some(arg) => match pick_jingle(jingles, arg) {
                    Ok(i) => jingles[i].clone(),
                    Err(e) => return CommandResult::Reply(format!("ERROR: {e}")),
                },
                None => {
                    if jingles.is_empty() {
                        return CommandResult::Reply("ERROR: no jingles configured".into());
                    }
                    let idx = rng.gen_range(0..jingles.len());
                    jingles[idx].clone()
                }
            };
            play_jingle(&path, tx)
        }
        _ => CommandResult::Reply(format!("unknown command: {cmd} (help for commands)")),
    }
}

fn help_text() -> &'static str {
    "commands: jingles.list | jingles.play [n|substr] | queue.push <path> | queue.list | queue.clear | queue.skip | skip | status | uptime | shutdown | exit | help"
}

fn list_jingles(jingles: &[PathBuf]) -> String {
    if jingles.is_empty() {
        return "no jingles configured".into();
    }
    let lines: Vec<String> = jingles
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{i}: {}", p.display()))
        .collect();
    lines.join("\n")
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
        return CommandResult::Reply(format!("ERROR: jingle missing: {}", path.display()));
    }
    let _ = tx.send(MixCommand::PlayJingle(path.to_path_buf()));
    log::info!("control port: playing jingle {}", path.display());
    CommandResult::Reply(format!("playing {}", path.display()))
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
        let reply = dispatch("skip", &jingles(), None, &mut SmallRng::from_entropy(), &tx, &status);
        match reply {
            CommandResult::Reply(text) => assert_eq!(text, "skipping"),
            CommandResult::Exit => panic!("skip must reply"),
        }
        assert!(matches!(rx.try_recv(), Ok(MixCommand::Skip)));
    }

    #[test]
    fn status_and_uptime_report_engine_state() {
        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        status.set_current("some track");
        match dispatch("status", &jingles(), None, &mut SmallRng::from_entropy(), &tx, &status) {
            CommandResult::Reply(text) => {
                assert!(text.contains("playing: some track"));
                assert!(text.contains("uptime: "));
            }
            CommandResult::Exit => panic!("status must reply"),
        }
        match dispatch("uptime", &jingles(), None, &mut SmallRng::from_entropy(), &tx, &status) {
            CommandResult::Reply(text) => assert!(text.starts_with("uptime: ")),
            CommandResult::Exit => panic!("uptime must reply"),
        }
    }

    #[test]
    fn queue_commands_push_list_clear_and_skip() {
        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        let queue = Arc::new(RequestQueue::new());
        let mut rng = SmallRng::from_entropy();

        match dispatch("queue.push /tmp/a.mp3", &jingles(), Some(&queue), &mut rng, &tx, &status) {
            CommandResult::Reply(text) => assert!(text.contains("queued /tmp/a.mp3 (1)"), "{text}"),
            CommandResult::Exit => panic!("queue.push must reply"),
        }
        match dispatch("queue.push /tmp/b.mp3", &jingles(), Some(&queue), &mut rng, &tx, &status) {
            CommandResult::Reply(text) => assert!(text.contains("(2)"), "{text}"),
            CommandResult::Exit => panic!("queue.push must reply"),
        }
        match dispatch("queue.list", &jingles(), Some(&queue), &mut rng, &tx, &status) {
            CommandResult::Reply(text) => {
                assert!(text.contains("0: /tmp/a.mp3"));
                assert!(text.contains("1: /tmp/b.mp3"));
            }
            CommandResult::Exit => panic!("queue.list must reply"),
        }
        match dispatch("queue.skip", &jingles(), Some(&queue), &mut rng, &tx, &status) {
            CommandResult::Reply(text) => assert!(text.contains("skipping queued track"), "{text}"),
            CommandResult::Exit => panic!("queue.skip must reply"),
        }
        match dispatch("queue.clear", &jingles(), Some(&queue), &mut rng, &tx, &status) {
            CommandResult::Reply(text) => assert!(text.contains("cleared"), "{text}"),
            CommandResult::Exit => panic!("queue.clear must reply"),
        }
        assert!(queue.is_empty());
    }

    #[test]
    fn queue_commands_require_a_configured_queue() {
        let (tx, _rx) = mpsc::channel();
        let status = StatusHandle::new();
        let mut rng = SmallRng::from_entropy();
        for cmd in ["queue.push /tmp/a.mp3", "queue.list", "queue.skip", "queue.clear"] {
            match dispatch(cmd, &jingles(), None, &mut rng, &tx, &status) {
                CommandResult::Reply(text) => {
                    assert!(text.contains("no request.queue source"), "{cmd}: {text}")
                }
                CommandResult::Exit => panic!("{cmd} must reply"),
            }
        }
    }
}
