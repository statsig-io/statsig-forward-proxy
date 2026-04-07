use flate2::read::GzDecoder;
use once_cell::sync::Lazy;
use std::fmt::Write;
use std::io::Read;
use std::str::FromStr;
use std::sync::Arc;

use super::request_builder::{RequestBuilderOutcome, RequestBuilderTrait};
use super::{DataProviderRequestResult, DataProviderResult, DataProviderTrait};
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{proxy_event_observer::ProxyEventObserver, ProxyEvent, ProxyEventType};
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::CompressionEncoder;
use bytes::Bytes;
use regex::Regex;
use reqwest::header::HeaderMap;

#[derive(Debug)]
pub struct ResponsePayload {
    pub encoding: Arc<CompressionEncoder>,
    pub use_proto: bool,
    pub data: Arc<Bytes>,
}

pub trait DataProviderObserver {
    fn update(&self, key: &str, data: &str);
}

pub struct HttpDataProvider {}

use async_trait::async_trait;

use tokio::time::Instant;

#[async_trait]
impl DataProviderTrait for HttpDataProvider {
    async fn get(
        &self,
        http_client: &reqwest::Client,
        request_builder: &Arc<dyn RequestBuilderTrait>,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> DataProviderResult {
        let start_time = Instant::now();

        let response = match request_builder
            .make_request(http_client, request_context, lcut)
            .await
        {
            Ok(RequestBuilderOutcome::Response(response)) => response,
            Ok(RequestBuilderOutcome::NoDataAvailable { status_code }) => {
                return self
                    .handle_no_data(lcut, start_time, request_context, status_code)
                    .await;
            }
            Err(err) if err.is_connect() => match request_builder
                .make_request(http_client, request_context, lcut)
                .await
            {
                Ok(RequestBuilderOutcome::Response(response)) => response,
                Ok(RequestBuilderOutcome::NoDataAvailable { status_code }) => {
                    return self
                        .handle_no_data(lcut, start_time, request_context, status_code)
                        .await;
                }
                Err(err) => {
                    return self
                        .handle_error(
                            Self::make_useful_error_message(&err),
                            start_time,
                            request_context,
                            lcut,
                            None,
                            None,
                        )
                        .await
                }
            },
            Err(err) => {
                return self
                    .handle_error(
                        Self::make_useful_error_message(&err),
                        start_time,
                        request_context,
                        lcut,
                        None,
                        None,
                    )
                    .await
            }
        };

        let status = response.status();
        let headers = response.headers().clone();

        if status == reqwest::StatusCode::NO_CONTENT {
            return self
                .handle_no_data(lcut, start_time, request_context, status.as_u16())
                .await;
        }

        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(err) => {
                return self
                    .handle_error(
                        Self::make_useful_error_message(&err),
                        start_time,
                        request_context,
                        lcut,
                        Some(status.as_u16()),
                        None,
                    )
                    .await
            }
        };
        if !status.is_success() {
            let response_encoding = Self::header_value(&headers, "content-encoding");
            let content_type = Self::header_value(&headers, "content-type");
            let body_preview =
                Self::format_error_body_preview(bytes.as_ref(), response_encoding.as_deref());
            let body = format!(
                "status={} content_type={} response_encoding={} body_len={} body_preview={}",
                status.as_u16(),
                content_type.as_deref().unwrap_or("unknown"),
                response_encoding.as_deref().unwrap_or("none"),
                bytes.len(),
                body_preview
            );

            return self
                .handle_error(
                    body,
                    start_time,
                    request_context,
                    lcut,
                    Some(status.as_u16()),
                    response_encoding,
                )
                .await;
        }

        if !request_builder
            .is_an_update(bytes.as_ref(), &headers, request_context)
            .await
        {
            return self
                .handle_no_data(lcut, start_time, request_context, status.as_u16())
                .await;
        }

        let since_time = self.parse_since_time(&headers, lcut, start_time, request_context);
        let content_encoding =
            headers
                .get("content-encoding")
                .and_then(|value| match value.to_str() {
                    Ok(encoding) => Some(encoding.to_string()),
                    Err(_e) => None,
                });
        let use_proto = self.parse_use_proto(&headers);
        self.handle_success(
            (use_proto, content_encoding, bytes),
            since_time,
            start_time,
            request_context,
            status.as_u16(),
        )
        .await
    }
}

static SECRET_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(secret-[a-zA-Z0-9]+)").unwrap());
static REDACTED_STR: Lazy<Arc<str>> = Lazy::new(|| Arc::from("REDACTED"));

impl HttpDataProvider {
    // See https://github.com/seanmonstar/reqwest/discussions/2342 for why we need this
    fn make_useful_error_message(mut err: &(dyn std::error::Error + 'static)) -> String {
        let mut s = format!("{err}");
        while let Some(src) = err.source() {
            let _ = write!(s, "\\nCaused by: {src}");
            err = src;
        }
        s
    }

    async fn handle_error(
        &self,
        err_msg: String,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        status_code: Option<u16>,
        response_encoding: Option<String>,
    ) -> DataProviderResult {
        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;
        let mut event =
            ProxyEvent::new_with_rc(ProxyEventType::HttpDataProviderError, request_context)
                .with_lcut(lcut)
                .with_stat(EventStat {
                    operation_type: OperationType::Distribution,
                    value: ms,
                });
        if let Some(status_code) = status_code {
            event = event.with_status_code(status_code);
        }
        if let Some(response_encoding) = response_encoding {
            event = event.with_response_encoding(response_encoding);
        }

        let redacted = SECRET_REGEX.replace_all(
            &err_msg,
            event
                .get_sdk_key()
                .unwrap_or(Arc::clone(&REDACTED_STR))
                .to_string(),
        );
        eprintln!("Failed to get data from http provider. {redacted}");
        ProxyEventObserver::publish_event(event);

        DataProviderResult {
            result: match status_code {
                Some(401 | 403) => DataProviderRequestResult::Unauthorized,
                Some(400..=499) => DataProviderRequestResult::ClientError,
                _ => DataProviderRequestResult::Error,
            },
            body: None,
            lcut: 0,
        }
    }

    fn header_value(headers: &HeaderMap, key: &str) -> Option<String> {
        headers
            .get(key)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    fn decompress_error_body(data: &[u8], response_encoding: Option<&str>) -> Result<Vec<u8>, ()> {
        match response_encoding
            .map(|encoding| encoding.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("gzip") => {
                let mut decoder = GzDecoder::new(data);
                let mut decompressed = Vec::new();
                decoder.read_to_end(&mut decompressed).map_err(|_| ())?;
                Ok(decompressed)
            }
            Some("br") | Some("statsig-br") => {
                let mut decoder = brotli::Decompressor::new(data, 4096);
                let mut decompressed = Vec::new();
                decoder.read_to_end(&mut decompressed).map_err(|_| ())?;
                Ok(decompressed)
            }
            None | Some("") => Ok(data.to_vec()),
            _ => Err(()),
        }
    }

    fn format_error_body_preview(data: &[u8], response_encoding: Option<&str>) -> String {
        const MAX_PREVIEW_LEN: usize = 256;
        const MAX_HEX_BYTES: usize = 32;

        let decoded = match Self::decompress_error_body(data, response_encoding) {
            Ok(decoded) => decoded,
            Err(_) => {
                return format!(
                    "<unable_to_decode encoding={}>",
                    response_encoding.unwrap_or("unknown")
                )
            }
        };

        if let Ok(text) = std::str::from_utf8(&decoded) {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return "<empty>".to_string();
            }

            let preview: String = trimmed.chars().take(MAX_PREVIEW_LEN).collect();
            let suffix = if trimmed.chars().count() > MAX_PREVIEW_LEN {
                "...<truncated>"
            } else {
                ""
            };
            return format!("{preview}{suffix}");
        }

        let hex_preview = decoded
            .iter()
            .take(MAX_HEX_BYTES)
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let suffix = if decoded.len() > MAX_HEX_BYTES {
            " ...<truncated>"
        } else {
            ""
        };

        format!("<non_utf8 hex={hex_preview}{suffix}>")
    }

    async fn handle_no_data(
        &self,
        lcut: u64,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
        status_code: u16,
    ) -> DataProviderResult {
        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::HttpDataProviderNoData, request_context)
                .with_lcut(lcut)
                .with_status_code(status_code)
                .with_stat(EventStat {
                    operation_type: OperationType::Distribution,
                    value: ms,
                }),
        );

        DataProviderResult {
            result: DataProviderRequestResult::NoDataAvailable,
            body: None,
            lcut: 0,
        }
    }

    fn parse_since_time(
        &self,
        headers: &HeaderMap,
        lcut: u64,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> u64 {
        if !request_context.use_lcut {
            return lcut;
        }

        let parsed_from_x_since_time = headers
            .get("x-since-time")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if let Some(parsed_lcut) = parsed_from_x_since_time {
            return parsed_lcut;
        }

        let parsed_from_blob_metadata = headers
            .get("x-ms-meta-lcut")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        if let Some(parsed_lcut) = parsed_from_blob_metadata {
            return parsed_lcut;
        }

        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(
                ProxyEventType::HttpDataProviderNoDataDueToBadLcut,
                request_context,
            )
            .with_lcut(lcut)
            .with_stat(EventStat {
                operation_type: OperationType::Distribution,
                value: ms,
            }),
        );

        0
    }

    fn parse_use_proto(&self, headers: &HeaderMap) -> bool {
        let content_type = headers.get("Content-Type").map(|s| {
            let st = s.to_str();
            st.unwrap_or("")
        });

        content_type.is_some_and(|c| c.contains("application/octet-stream"))
    }

    async fn handle_success(
        &self,
        (use_proto, encoding_str, data): (bool, Option<String>, Bytes),
        since_time: u64,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
        status_code: u16,
    ) -> DataProviderResult {
        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;
        let encoding = match encoding_str.clone() {
            Some(encoding_unwrapped) => CompressionEncoder::from_str(&encoding_unwrapped)
                .unwrap_or(CompressionEncoder::PlainText),
            None => CompressionEncoder::PlainText,
        };

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::HttpDataProviderGotData, request_context)
                .with_lcut(since_time)
                .with_response_encoding(encoding.to_string())
                .with_status_code(status_code)
                .with_stat(EventStat {
                    operation_type: OperationType::Distribution,
                    value: ms,
                }),
        );
        Self::publish_download_id_list_file_size_metric(request_context, data.len() as u64);

        DataProviderResult {
            result: DataProviderRequestResult::DataAvailable,
            body: Some(Arc::new(ResponsePayload {
                use_proto,
                encoding: Arc::from(encoding),
                data: Arc::from(data),
            })),
            lcut: since_time,
        }
    }

    fn publish_download_id_list_file_size_metric(
        request_context: &Arc<AuthorizedRequestContext>,
        fetched_blob_len: u64,
    ) {
        if request_context.path != NormalizedPath::V1DownloadIdListFile {
            return;
        }

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::DownloadIdListFileSizeBytes, request_context)
                .with_stat(EventStat {
                    operation_type: OperationType::Gauge,
                    value: fetched_blob_len.min(i64::MAX as u64) as i64,
                }),
        );
    }
}
