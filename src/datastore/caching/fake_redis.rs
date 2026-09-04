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

pub struct FakeRedis {
    pub port: u16,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
}

impl FakeRedis {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("local addr").port();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&commands);

        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let recorded = Arc::clone(&recorded);
                let (read_half, write_half) = socket.into_split();
                tokio::spawn(serve(BufReader::new(read_half), write_half, recorded));
            }
        });

        FakeRedis { port, commands }
    }

    /// Every command received across every connection, in arrival order.
    pub fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().clone()
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
) {
    while let Some(command) = read_command(&mut reader).await {
        let reply = match command.first().map(|name| name.to_ascii_uppercase()) {
            Some(name) if name == "PING" => "+PONG\r\n",
            _ => "+OK\r\n",
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
