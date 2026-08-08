//! Liquidsoap-style telnet control port.
//!
//! Connects with `telnet <host> <port>` and issues one command per line.
//! Commands:
//!
//! - `jingles.list`            — list available jingles
//! - `jingles.play`            — play a random jingle
//! - `jingles.play <n>`        — play jingle at index `n`
//! - `jingles.play <substr>`   — play the jingle whose name contains `substr`
//! - `shutdown`                — stop the app (like Ctrl-C)
//! - `exit` / `quit`           — close the connection
//! - `help`                    — list commands

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use crate::config::ControlConfig;
use crate::engine::mixer::MixCommand;

/// Telnet command server. Owns one `mpsc::Sender` into the priority mixer.
pub struct ControlServer {
    config: ControlConfig,
    jingles: Vec<PathBuf>,
    tx: mpsc::Sender<MixCommand>,
}

impl ControlServer {
    pub fn new(
        config: ControlConfig,
        jingles: Vec<PathBuf>,
        tx: mpsc::Sender<MixCommand>,
    ) -> Self {
        Self {
            config,
            jingles,
            tx,
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
            let tx = self.tx.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(socket, &jingles, tx).await {
                    log::warn!("control port ({peer}): {e}");
                }
            });
        }
    }
}

async fn handle_connection(
    socket: TcpStream,
    jingles: &[PathBuf],
    tx: mpsc::Sender<MixCommand>,
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
        match dispatch(cmd, jingles, &mut rng, &tx) {
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
    rng: &mut SmallRng,
    tx: &mpsc::Sender<MixCommand>,
) -> CommandResult {
    let mut parts = cmd.split_whitespace();
    let verb = parts.next().unwrap_or("");
    match verb {
        "help" => CommandResult::Reply(help_text().to_string()),
        "exit" | "quit" => CommandResult::Exit,
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
    "commands: jingles.list | jingles.play [n|substr] | shutdown | exit | help"
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
}
