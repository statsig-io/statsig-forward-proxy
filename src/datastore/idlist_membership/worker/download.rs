use bytes::{Bytes, BytesMut};
use futures::{stream, stream::BoxStream, StreamExt, TryStreamExt};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_RANGE, RANGE};

use super::parser::content_range_starts_at;

use super::WorkerError;

pub(super) fn build_download_request(
    http: &reqwest::Client,
    url: &str,
    range: &str,
    file_size: u64,
) -> Result<reqwest::RequestBuilder, WorkerError> {
    let mut url = reqwest::Url::parse(url)
        .map_err(|_| WorkerError::InvalidTask("invalid download URL".to_string()))?;
    // Match server-core: CDN ranges belong in the query/cache key; other
    // origins (including custom forward proxies) use standard Range headers.
    let is_cdn = url.host_str() == Some("api.statsigcdn.com");
    if is_cdn {
        let query: Vec<_> = url
            .query_pairs()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("range"))
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        url.set_query(None);
        url.query_pairs_mut()
            .extend_pairs(query)
            .append_pair("range", range);
    }
    let request = http.get(url).header(ACCEPT_ENCODING, "identity");
    Ok(if is_cdn {
        request
    } else {
        request
            .header(RANGE, format!("bytes={range}"))
            .header("statsig-id-list-file-size", file_size)
    })
}

pub(super) const DOWNLOAD_CHUNK_BYTES: u64 = 4 * 1024 * 1024;

pub(super) async fn download_file(
    http: &reqwest::Client,
    url: &str,
    start: u64,
    file_size: u64,
) -> Result<BoxStream<'static, Result<Bytes, WorkerError>>, WorkerError> {
    let request = build_download_request(http, url, &format!("{start}-"), file_size)?;
    if reqwest::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .as_deref()
        == Some("api.statsigcdn.com")
    {
        return Ok(chunked_stream(
            http.clone(),
            url.to_string(),
            start,
            file_size,
            DOWNLOAD_CHUNK_BYTES,
        ));
    }
    // Custom FP endpoints accept open-ended ranges only. Keep the SDK's
    // streaming/header path for them and direct blob origins.
    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(WorkerError::OriginStatus(status));
    }
    if start > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
        return Err(WorkerError::RangeIgnored);
    }
    if start > 0
        && !response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| content_range_starts_at(value, start))
    {
        return Err(WorkerError::InvalidFile(
            "unexpected Content-Range".to_string(),
        ));
    }
    check_encoding(&response)?;
    Ok(response.bytes_stream().map_err(WorkerError::from).boxed())
}

fn chunked_stream(
    http: reqwest::Client,
    url: String,
    start: u64,
    file_size: u64,
    chunk_bytes: u64,
) -> BoxStream<'static, Result<Bytes, WorkerError>> {
    stream::try_unfold(
        (http, url, start, None),
        move |(http, url, offset, snapshot_size)| async move {
            if snapshot_size.is_some_and(|total| offset >= total) {
                return Ok(None);
            }
            let end = offset
                .saturating_add(chunk_bytes - 1)
                .min(snapshot_size.unwrap_or(u64::MAX).saturating_sub(1));
            // Finish the HTTP body before yielding to Redis. Its per-request
            // deadline must not include time spent applying Redis batches.
            let (chunk, total) = download_chunk(&http, &url, offset, end, file_size).await?;
            let next = offset + chunk.len() as u64;
            Ok(Some((
                chunk,
                (http, url, next, Some(snapshot_size.unwrap_or(total))),
            )))
        },
    )
    .boxed()
}

fn check_encoding(response: &reqwest::Response) -> Result<(), WorkerError> {
    if response
        .headers()
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|encoding| !encoding.eq_ignore_ascii_case("identity"))
    {
        return Err(WorkerError::InvalidFile(
            "origin returned encoded bytes".to_string(),
        ));
    }
    Ok(())
}

pub(super) async fn download_chunk(
    http: &reqwest::Client,
    url: &str,
    start: u64,
    end: u64,
    file_size: u64,
) -> Result<(Bytes, u64), WorkerError> {
    if end < start {
        return Err(WorkerError::InvalidTask(
            "invalid download range".to_string(),
        ));
    }
    let response = build_download_request(http, url, &format!("{start}-{end}"), file_size)?
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        return Err(WorkerError::OriginStatus(status));
    }
    let (expected_len, total) = if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let value = response
            .headers()
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        let parsed = value
            .strip_prefix("bytes ")
            .and_then(|value| value.split_once('/'))
            .and_then(|(range, total)| Some((range.split_once('-')?.1, total)))
            .and_then(|(last, total)| {
                Some((last.parse::<u64>().ok()?, total.parse::<u64>().ok()?))
            });
        let (last, total) = parsed
            .filter(|(last, total)| {
                content_range_starts_at(value, start)
                    && *total > *last
                    && *last >= start
                    && *last == end.min(total - 1)
            })
            .ok_or_else(|| WorkerError::InvalidFile("unexpected Content-Range".to_string()))?;
        (Some(last - start + 1), Some(total))
    } else if status == reqwest::StatusCode::OK && start == 0 {
        // Small origins without Range support still work; never buffer an
        // unbounded response when the origin ignores our requested limit.
        (None, None)
    } else {
        return Err(WorkerError::RangeIgnored);
    };
    check_encoding(&response)?;
    let limit = end - start + 1;
    if response.content_length().is_some_and(|len| len > limit) {
        return Err(WorkerError::RangeIgnored);
    }
    let mut body = BytesMut::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len() as u64 + chunk.len() as u64 > limit {
            return Err(WorkerError::RangeIgnored);
        }
        body.extend_from_slice(&chunk);
    }
    if expected_len.is_some_and(|len| len != body.len() as u64) {
        return Err(WorkerError::InvalidFile(
            "truncated download chunk".to_string(),
        ));
    }
    let total = total.unwrap_or(body.len() as u64);
    Ok((body.freeze(), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cdn_uses_query_range_and_preserves_manifest_parameters() {
        for start in [0, 270] {
            let request = build_download_request(
                &reqwest::Client::builder().no_proxy().build().unwrap(),
                "https://api.statsigcdn.com/v1/download_id_list_file/company%2Ffile?sig=signed%2Bvalue&k=secret-test&Range=old&range=older",
                &format!("{start}-"),
                1024,
            )
            .unwrap()
            .build()
            .unwrap();
            let params: Vec<_> = request.url().query_pairs().collect();
            assert_eq!(
                request.url().path(),
                "/v1/download_id_list_file/company%2Ffile"
            );
            assert_eq!(params.len(), 3);
            assert!(params
                .iter()
                .any(|(k, v)| k == "sig" && v == "signed+value"));
            assert!(params.iter().any(|(k, v)| k == "k" && v == "secret-test"));
            assert!(params
                .iter()
                .any(|(k, v)| k == "range" && *v == format!("{start}-")));
            assert!(!request.headers().contains_key(RANGE));
            assert!(!request.headers().contains_key("statsig-id-list-file-size"));
            assert_eq!(request.headers()[ACCEPT_ENCODING], "identity");
        }
    }

    #[test]
    fn other_hosts_use_headers_without_changing_signed_query() {
        for url in [
            "https://idliststorage.blob.core.windows.net/idlists/company/file?sig=signed%2Bvalue",
            "https://customer-proxy.example/v1/download_id_list_file/company%2Ffile?sig=signed%2Bvalue",
            "https://api.statsigcdn.com.example.com/v1/download_id_list_file/file?sig=signed%2Bvalue",
            "https://api.statsigcdn.com@other.example/v1/download_id_list_file/file?sig=signed%2Bvalue",
        ] {
            let request = build_download_request(&reqwest::Client::builder().no_proxy().build().unwrap(), url, "270-", 1024)
                .unwrap()
                .build()
                .unwrap();
            let original = reqwest::Url::parse(url).unwrap();
            assert_eq!(request.url().host_str(), original.host_str());
            assert_eq!(request.url().path(), original.path());
            assert_eq!(request.url().query(), original.query());
            assert_eq!(request.headers()[RANGE], "bytes=270-");
            assert_eq!(request.headers()["statsig-id-list-file-size"], "1024");
            assert_eq!(request.headers()[ACCEPT_ENCODING], "identity");
        }
    }

    #[test]
    fn normalized_cdn_hostname_uses_query_range() {
        let request = build_download_request(
            &reqwest::Client::builder().no_proxy().build().unwrap(),
            "https://API.STATSIGCDN.COM/v1/download_id_list_file/file",
            "0-",
            1024,
        )
        .unwrap()
        .build()
        .unwrap();
        assert_eq!(request.url().query(), Some("range=0-"));
        assert!(!request.headers().contains_key(RANGE));
    }
}

#[cfg(test)]
#[path = "download_tests.rs"]
mod chunk_tests;
