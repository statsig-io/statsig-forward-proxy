use once_cell::sync::Lazy;
use std::fmt::Write;
use std::str::FromStr;
use std::sync::Arc;

use super::request_builder::RequestBuilderTrait;
use super::{DataProviderRequestResult, DataProviderResult, DataProviderTrait};
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{proxy_event_observer::ProxyEventObserver, ProxyEvent, ProxyEventType};
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use crate::utils::compress_encoder::CompressionEncoder;
use bytes::Bytes;
use regex::Regex;
use reqwest::header::HeaderMap;

#[derive(Debug)]
pub struct ResponsePayload {
    pub encoding: Arc<CompressionEncoder>,
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
        zstd_dict_id: &Option<Arc<str>>,
    ) -> DataProviderResult {
        let start_time = Instant::now();

        let response = match request_builder
            .make_request(http_client, request_context, lcut, zstd_dict_id)
            .await
        {
            Ok(response) => response,
            Err(err) => {
                return self
                    .handle_error(
                        Self::make_useful_error_message(&err),
                        start_time,
                        request_context,
                        lcut,
                    )
                    .await
            }
        };

        let status = response.status();
        let headers = response.headers().clone();

        let (body, bytes) = match response.bytes().await {
            Ok(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), bytes),
            Err(err) => {
                return self
                    .handle_error(
                        Self::make_useful_error_message(&err),
                        start_time,
                        request_context,
                        lcut,
                    )
                    .await
            }
        };

        if !status.is_success() {
            return self
                .handle_error(body, start_time, request_context, lcut)
                .await;
        }

        if !request_builder
            .is_an_update(&body, &headers, &request_context.sdk_key)
            .await
        {
            return self.handle_no_data(lcut, start_time, request_context).await;
        }

        let since_time = self.parse_since_time(&headers, lcut, start_time, request_context);
        let compressed_with_dict = self.parse_zstd_dict_id(&headers, request_context);
        let content_encoding =
            headers
                .get("content-encoding")
                .and_then(|value| match value.to_str() {
                    Ok(encoding) => Some(encoding.to_string()),
                    Err(_e) => None,
                });
        self.handle_success(
            (content_encoding, bytes),
            since_time,
            &compressed_with_dict,
            start_time,
            request_context,
        )
        .await
    }
}

static SECRET_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"(secret-[a-zA-Z0-9]+)").unwrap());
static REDACTED_STR: Lazy<Arc<str>> = Lazy::new(|| Arc::from("REDACTED"));

impl HttpDataProvider {
    // See https://github.com/seanmonstar/reqwest/discussions/2342 for why we need this
    fn make_useful_error_message(mut err: &(dyn std::error::Error + 'static)) -> String {
        let mut s = format!("{}", err);
        while let Some(src) = err.source() {
            let _ = write!(s, "\\nCaused by: {}", src);
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
    ) -> DataProviderResult {
        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;
        let event = ProxyEvent::new_with_rc(ProxyEventType::HttpDataProviderError, request_context)
            .with_lcut(lcut)
            .with_stat(EventStat {
                operation_type: OperationType::Distribution,
                value: ms,
            });
        eprintln!(
            "Failed to get data from http provider: {:?}",
            SECRET_REGEX.replace_all(
                &err_msg,
                event
                    .get_sdk_key()
                    .unwrap_or(Arc::clone(&REDACTED_STR))
                    .to_string()
            )
        );
        ProxyEventObserver::publish_event(event);

        DataProviderResult {
            result: if err_msg.contains("401") || err_msg.contains("403") {
                DataProviderRequestResult::Unauthorized
            } else {
                DataProviderRequestResult::Error
            },
            body: None,
            lcut: 0,
            zstd_dict_id: None,
        }
    }

    async fn handle_no_data(
        &self,
        lcut: u64,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> DataProviderResult {
        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::HttpDataProviderNoData, request_context)
                .with_lcut(lcut)
                .with_stat(EventStat {
                    operation_type: OperationType::Distribution,
                    value: ms,
                }),
        );

        DataProviderResult {
            result: DataProviderRequestResult::NoDataAvailable,
            body: None,
            lcut: 0,
            zstd_dict_id: None,
        }
    }

    fn parse_zstd_dict_id(
        &self,
        headers: &HeaderMap,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<str>> {
        if !request_context.use_dict_id {
            return None;
        }

        let zstd_dict_id: Option<Arc<str>> = headers
            .get("x-compression-dict")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| match value.is_empty() {
                true => None,
                false => Some(Arc::from(value.to_string())),
            });

        zstd_dict_id
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

        headers
            .get("x-since-time")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_else(|| {
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
            })
    }

    async fn handle_success(
        &self,
        (encoding_str, data): (Option<String>, Bytes),
        since_time: u64,
        zstd_dict_id: &Option<Arc<str>>,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> DataProviderResult {
        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;
        let encoding = match encoding_str {
            Some(encoding_unwrapped) => CompressionEncoder::from_str(&encoding_unwrapped)
                .unwrap_or(CompressionEncoder::PlainText),
            None => CompressionEncoder::PlainText,
        };

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::HttpDataProviderGotData, request_context)
                .with_lcut(since_time)
                .with_zstd_dict_id(zstd_dict_id.clone())
                .with_stat(EventStat {
                    operation_type: OperationType::Distribution,
                    value: ms,
                }),
        );

        DataProviderResult {
            result: DataProviderRequestResult::DataAvailable,
            body: Some(Arc::new(ResponsePayload {
                encoding: Arc::from(encoding),
                data: Arc::from(data),
            })),
            lcut: since_time,
            zstd_dict_id: zstd_dict_id.clone(),
        }
    }
}
