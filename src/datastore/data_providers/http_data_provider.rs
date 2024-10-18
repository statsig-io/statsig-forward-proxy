use std::sync::Arc;

use super::request_builder::RequestBuilderTrait;
use super::{DataProviderRequestResult, DataProviderResult, DataProviderTrait};
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{proxy_event_observer::ProxyEventObserver, ProxyEvent, ProxyEventType};
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use bytes::Bytes;
use reqwest::header::HeaderMap;

#[derive(Debug)]
pub struct ResponsePayload {
    pub encoding: Arc<Option<String>>,
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
            Ok(response) => response,
            Err(err) => {
                return self
                    .handle_error(err.to_string(), start_time, request_context, lcut)
                    .await
            }
        };

        let status = response.status();
        let headers = response.headers().clone();

        let (body, bytes) = match response.bytes().await {
            Ok(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), bytes),
            Err(err) => {
                return self
                    .handle_error(err.to_string(), start_time, request_context, lcut)
                    .await
            }
        };

        if !status.is_success() {
            return self
                .handle_error(body, start_time, request_context, lcut)
                .await;
        }

        if !request_builder
            .is_an_update(&body, &request_context.sdk_key)
            .await
        {
            return self.handle_no_data(lcut, start_time, request_context).await;
        }

        let since_time = self
            .parse_since_time(&headers, lcut, start_time, request_context)
            .await;
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
            start_time,
            request_context,
        )
        .await
    }
}

impl HttpDataProvider {
    async fn handle_error(
        &self,
        err_msg: String,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> DataProviderResult {
        eprintln!("Failed to get data from http provider: {:?}", err_msg);
        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::HttpDataProviderError, request_context)
                .with_lcut(lcut)
                .with_stat(EventStat {
                    operation_type: OperationType::Distribution,
                    value: ms,
                }),
        );

        DataProviderResult {
            result: if err_msg.contains("401") || err_msg.contains("403") {
                DataProviderRequestResult::Unauthorized
            } else {
                DataProviderRequestResult::Error
            },
            data: None,
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
            data: None,
        }
    }

    async fn parse_since_time(
        &self,
        headers: &HeaderMap,
        lcut: u64,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> u64 {
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
        result: (Option<String>, Bytes),
        since_time: u64,
        start_time: Instant,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> DataProviderResult {
        let duration = start_time.elapsed();
        let ms = duration.as_millis() as i64;

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::HttpDataProviderGotData, request_context)
                .with_lcut(since_time)
                .with_stat(EventStat {
                    operation_type: OperationType::Distribution,
                    value: ms,
                }),
        );

        DataProviderResult {
            result: DataProviderRequestResult::DataAvailable,
            data: Some((
                Arc::new(ResponsePayload {
                    encoding: Arc::from(result.0),
                    data: Arc::from(result.1),
                }),
                since_time,
            )),
        }
    }
}
