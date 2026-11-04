//! A minimal in-process Redis stand-in for tests.
//!
//! It speaks just enough RESP to let a real redis-rs client complete its handshake: it parses
//! inbound command arrays, records them, answers `PING` with `+PONG` and everything else with
//! `+OK`. That is enough to assert the exact `AUTH` arguments each auth mode puts on the wire,
//! which is otherwise only observable against a live Redis.

use std::sync::Arc;

use parking_lot::Mutex;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener,
    },
};

/// How the stand-in answers application commands (`PING`/`AUTH` stay healthy so the pool can connect).
#[derive(Clone, Debug)]
pub enum FakeRedisMode {
    /// Current default: every non-PING command is `+OK`.
    AlwaysOk,
    /// `SET NX` miss (`$-1`), so lock acquire returns `Ok(false)`.
    SetNxMiss,
    /// Named command (case-insensitive) replies `-ERR injected`.
    ErrorOn(String),
}

pub struct FakeRedis {
    pub port: u16,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FakeRedis {
    pub async fn start() -> Self {
        Self::start_with(FakeRedisMode::AlwaysOk).await
    }

    pub async fn start_with(mode: FakeRedisMode) -> Self {
        Self::start_with_options(mode, 1).await
    }

    pub async fn start_with_expire_reply(expire_reply: i64) -> Self {
        Self::start_with_options(FakeRedisMode::AlwaysOk, expire_reply).await
    }

    async fn start_with_options(mode: FakeRedisMode, expire_reply: i64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&commands);
        let mode = Arc::new(mode);

        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                let mode = Arc::clone(&mode);
                let (read_half, write_half) = socket.into_split();
                tokio::spawn(serve(
                    BufReader::new(read_half),
                    write_half,
                    recorded,
                    mode,
                    expire_reply,
                ));
            }
        });

        FakeRedis { port, commands }
    }

    /// Every command received across every connection, in arrival order.
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().clone()
    }

    pub fn clear_commands(&self) {
        self.commands.lock().clear();
    }

    /// Atomic TTL refresh scripts. This fake records arguments but does not execute Lua.
    pub fn refresh_commands(&self) -> Vec<Vec<String>> {
        self.commands
            .lock()
            .iter()
            .filter(|command| {
                command
                    .first()
                    .is_some_and(|first| first.eq_ignore_ascii_case("EVAL"))
            })
            .cloned()
            .collect()
    }

    /// Just the `AUTH` commands, which is what the auth modes differ on.
    pub fn auth_commands(&self) -> Vec<Vec<String>> {
        self.commands
            .lock()
            .iter()
            .filter(|command| {
                command
                    .first()
                    .is_some_and(|name| name.eq_ignore_ascii_case("AUTH"))
            })
            .cloned()
            .collect()
    }

    pub fn received_command(&self, name: &str) -> bool {
        self.commands
            .lock()
            .iter()
            .any(|command| command.first().is_some_and(|first| first == name))
    }
}

async fn serve(
    mut reader: BufReader<OwnedReadHalf>,
    mut writer: OwnedWriteHalf,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
    mode: Arc<FakeRedisMode>,
    expire_reply: i64,
) {
    while let Some(command) = read_command(&mut reader).await {
        let name = command.first().map(|name| name.to_ascii_uppercase());
        let injected_error = matches!(mode.as_ref(), FakeRedisMode::ErrorOn(target) if name.as_deref() == Some(target.to_ascii_uppercase().as_str()));
        let reply = if !injected_error && matches!(name.as_deref(), Some("EXPIRE" | "EVAL")) {
            format!(":{expire_reply}\r\n")
        } else {
            reply_for(&command, &mode).to_string()
        };
        commands.lock().push(command);
        if writer.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
    }
}

fn reply_for(command: &[String], mode: &FakeRedisMode) -> &'static str {
    let name = command
        .first()
        .map(|n| n.to_ascii_uppercase())
        .unwrap_or_default();
    match name.as_str() {
        "PING" => return "+PONG\r\n",
        // Keep the redis-rs handshake healthy regardless of fail mode.
        "AUTH" | "HELLO" | "CLIENT" | "SELECT" | "INFO" => return "+OK\r\n",
        _ => {}
    }
    match mode {
        FakeRedisMode::AlwaysOk => "+OK\r\n",
        FakeRedisMode::SetNxMiss if name == "SET" => "$-1\r\n",
        FakeRedisMode::SetNxMiss => "+OK\r\n",
        FakeRedisMode::ErrorOn(target) if name == target.to_ascii_uppercase() => {
            "-ERR injected\r\n"
        }
        FakeRedisMode::ErrorOn(_) => "+OK\r\n",
    }
}

/// Reads one RESP array of bulk strings, which is how clients send commands.
async fn read_command(reader: &mut BufReader<OwnedReadHalf>) -> Option<Vec<String>> {
    let mut header = String::new();
    if reader.read_line(&mut header).await.ok()? == 0 {
        return None;
    }
    let argument_count: usize = header.trim().strip_prefix('*')?.parse().ok()?;

    let mut arguments = Vec::with_capacity(argument_count);
    for _ in 0..argument_count {
        let mut length_line = String::new();
        if reader.read_line(&mut length_line).await.ok()? == 0 {
            return None;
        }
        let length: usize = length_line.trim().strip_prefix('$')?.parse().ok()?;

        // Read the payload plus its trailing CRLF.
        let mut payload = vec![0u8; length + 2];
        reader.read_exact(&mut payload).await.ok()?;
        payload.truncate(length);
        arguments.push(String::from_utf8_lossy(&payload).to_string());
    }
    Some(arguments)
}
