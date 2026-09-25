use std::{process::Stdio, time::Duration};

use tokio::{io::AsyncReadExt, net::TcpListener, process::Command, time::timeout};

#[tokio::test]
async fn redis_tls_starts_handshake_without_panicking() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut server = Command::new(env!("CARGO_BIN_EXE_server"))
        .args(["http", "redis", "--redis-connection-timeout-in-s", "1"])
        .env("REDIS_ENTERPRISE_HOST", "127.0.0.1")
        .env("REDIS_ENTERPRISE_PORT", port.to_string())
        .env("REDIS_TLS", "true")
        .env("REDIS_AUTH_MODE", "password")
        .env_remove("STATSIG_SERVER_SDK_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();

    let handshake = timeout(Duration::from_secs(10), async {
        tokio::select! {
            connection = listener.accept() => {
                let (mut connection, _) = connection?;
                let mut record_header = [0; 2];
                connection.read_exact(&mut record_header).await?;
                Ok::<_, std::io::Error>(Some(record_header))
            }
            status = server.wait() => {
                status?;
                Ok(None)
            }
        }
    })
    .await;

    let _ = server.start_kill();
    let output = server.wait_with_output().await.unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr)
        .lines()
        .take(12)
        .collect::<Vec<_>>()
        .join("\n");
    match handshake {
        Ok(Ok(Some(record_header))) => {
            assert_eq!(record_header, [0x16, 0x03], "server stderr:\n{stderr}");
        }
        Ok(Ok(None)) => panic!("Server exited before Redis TLS handshake; stderr:\n{stderr}"),
        Ok(Err(error)) => panic!("Redis TLS handshake failed: {error}; stderr:\n{stderr}"),
        Err(_) => panic!("Redis TLS handshake did not start; server stderr:\n{stderr}"),
    }
}
