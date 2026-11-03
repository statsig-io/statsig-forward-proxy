use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::datastore::idlist_membership::worker::parser::CompleteLineDecoder;

async fn origin(
    responses: Vec<String>,
) -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let task = tokio::spawn(async move {
        for response in responses {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let size = socket.read(&mut buffer).await.unwrap();
                if size == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..size]);
            }
            captured
                .lock()
                .unwrap()
                .push(String::from_utf8(request).unwrap());
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    (
        format!("http://{address}/idlist?sig=preserve"),
        requests,
        task,
    )
}

fn partial(start: u64, end: u64, total: u64, body: &str) -> String {
    format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn bounded_chunks_carry_split_records_and_complete_line_checkpoints() {
    let (url, requests, task) = origin(vec![
        partial(0, 3, 12, "+alp"),
        partial(4, 7, 20, "ha\n+"),
        partial(8, 11, 20, "bet\n"),
    ])
    .await;
    let address = reqwest::Url::parse(&url)
        .unwrap()
        .socket_addrs(|| None)
        .unwrap()[0];
    let http = reqwest::Client::builder()
        .no_proxy()
        .resolve("api.statsigcdn.com", address)
        .build()
        .unwrap();
    let mut cdn_url = reqwest::Url::parse(&url).unwrap();
    cdn_url.set_host(Some("api.statsigcdn.com")).unwrap();
    let mut stream = chunked_stream(http, cdn_url.to_string(), 0, 12, 4);
    let mut decoder = CompleteLineDecoder::new(0);
    let mut members = Vec::new();
    let mut checkpoints = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        for decoded in decoder.push(&chunk).unwrap() {
            checkpoints.push(decoded.end_offset);
            members.push(decoded.change.unwrap().member);
        }
    }
    assert_eq!(members, ["alpha", "bet"]);
    assert_eq!(checkpoints, [7, 12]);
    assert!(!decoder.has_partial_line());
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    for (request, start) in requests.iter().zip([0, 4, 8]) {
        assert!(request.starts_with(&format!(
            "GET /idlist?sig=preserve&range={start}-{} HTTP/1.1",
            start + 3
        )));
        assert!(!request.contains("\r\nrange:"));
    }
}

#[tokio::test]
async fn custom_proxy_keeps_the_sdk_open_ended_range() {
    let (url, requests, task) = origin(vec![partial(7, 11, 12, "+bet\n")]).await;
    let mut stream = download_file(&http(), &url, 7, 12).await.unwrap();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        body.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(body, b"+bet\n");
    task.await.unwrap();
    let requests = requests.lock().unwrap();
    assert!(requests[0].contains("\r\nrange: bytes=7-\r\n"));
    assert!(requests[0].contains("\r\nstatsig-id-list-file-size: 12\r\n"));
}

#[tokio::test]
async fn final_chunk_clamps_to_origin_size() {
    let (url, _, task) = origin(vec![partial(8, 11, 12, "bet\n")]).await;
    let (body, total) = download_chunk(&http(), &url, 8, 100, 12).await.unwrap();
    assert_eq!(body.as_ref(), b"bet\n");
    assert_eq!(total, 12);
    task.await.unwrap();
}

#[tokio::test]
async fn rejects_bad_ranges_encoding_truncation_and_origin_errors() {
    for response in [
        partial(1, 3, 12, "bad"),
        partial(0, 4, 12, "extra"),
        partial(0, 3, 3, "+alp"),
        partial(0, 3, 12, "+al"),
        partial(0, 3, 12, "+alp")
            .replace("Content-Length", "Content-Encoding: gzip\r\nContent-Length"),
        "HTTP/1.1 206 Partial Content\r\nContent-Length: 4\r\nConnection: close\r\n\r\n+alp"
            .to_string(),
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string(),
    ] {
        let (url, _, task) = origin(vec![response]).await;
        assert!(download_chunk(&http(), &url, 0, 3, 12).await.is_err());
        task.await.unwrap();
    }
}

#[tokio::test]
async fn ignored_range_never_buffers_more_than_the_requested_chunk() {
    for start in [0, 1] {
        let response = "HTTP/1.1 200 OK\r\nContent-Length: 5000000000\r\nConnection: close\r\n\r\n";
        let (url, _, task) = origin(vec![response.to_string()]).await;
        assert!(matches!(
            download_chunk(&http(), &url, start, start + 3, 12).await,
            Err(WorkerError::RangeIgnored)
        ));
        task.await.unwrap();
    }
    let response = "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\ntoo much body";
    let (url, _, task) = origin(vec![response.to_string()]).await;
    assert!(matches!(
        download_chunk(&http(), &url, 0, 3, 12).await,
        Err(WorkerError::RangeIgnored)
    ));
    task.await.unwrap();
}

#[tokio::test]
async fn transport_errors_keep_the_cause_without_signed_url_credentials() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_millis(30))
        .build()
        .unwrap();
    let error = download_chunk(
        &http,
        &format!("http://{address}/idlist?k=secret-sensitive&sig=signed-sensitive"),
        0,
        3,
        12,
    )
    .await
    .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("timeout=true"));
    assert!(message.contains("timed out"));
    assert!(!message.contains("sensitive"));
    assert!(!message.contains("http://"));
    task.abort();
}
