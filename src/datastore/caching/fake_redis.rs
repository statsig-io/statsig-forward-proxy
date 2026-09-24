//! A minimal in-process Redis stand-in for tests.
//!
//! It speaks just enough RESP to let a real redis-rs client complete its handshake: it parses
//! inbound command arrays, records them, and returns canned replies for `PING`, `EXPIRE`,
//! and `EVAL` (without executing Lua), with `+OK` for other commands. That is enough to assert the exact `AUTH` arguments each auth mode puts on the wire,
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

pub struct FakeRedis {
    pub port: u16,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FakeRedis {
    pub async fn start() -> Self {
        Self::start_with_expire_reply(1).await
    }

    /// `EXPIRE` and `EVAL` answer with `expire_reply` instead of `+OK`. This controls
    /// result classification and wire assertions; the opt-in real-Redis regression verifies
    /// the script itself, including mismatched versions.
    pub async fn start_with_expire_reply(expire_reply: i64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&commands);

        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                let (read_half, write_half) = socket.into_split();
                tokio::spawn(serve(
                    BufReader::new(read_half),
                    write_half,
                    recorded,
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

    /// Just the `AUTH` commands, which is what the auth modes differ on.
    pub fn auth_commands(&self) -> Vec<Vec<String>> {
        self.commands_named("AUTH")
    }

    /// Atomic TTL refresh scripts. This fake records arguments but does not execute Lua.
    pub fn refresh_commands(&self) -> Vec<Vec<String>> {
        self.commands_named("EVAL")
    }

    fn commands_named(&self, name: &str) -> Vec<Vec<String>> {
        self.commands
            .lock()
            .iter()
            .filter(|command| {
                command
                    .first()
                    .is_some_and(|first| first.eq_ignore_ascii_case(name))
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
    expire_reply: i64,
) {
    while let Some(command) = read_command(&mut reader).await {
        let reply = match command.first().map(|name| name.to_ascii_uppercase()) {
            Some(name) if name == "PING" => "+PONG\r\n".to_string(),
            Some(name) if name == "EXPIRE" || name == "EVAL" => format!(":{expire_reply}\r\n"),
            _ => "+OK\r\n".to_string(),
        };
        commands.lock().push(command);
        if writer.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
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
