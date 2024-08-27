use std::sync::Arc;

use super::request_builder::RequestBuilderTrait;
use super::{DataProviderRequestResult, DataProviderResult, DataProviderTrait};
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{proxy_event_observer::ProxyEventObserver, ProxyEvent, ProxyEventType};
use crate::servers::http_server::AuthorizedRequestContext;
use reqwest::header::HeaderMap;

pub trait DataProviderObserver {
    fn update(&self, key: &str, data: &str);
}

pub struct HttpDataProvider {
    http_client: reqwest::Client,
}

impl HttpDataProvider {
    pub fn new(http_client: reqwest::Client) -> Self {
        HttpDataProvider { http_client }
    }
}

use async_trait::async_trait;

use tokio::time::Instant;
#[async_trait]
impl DataProviderTrait for HttpDataProvider {
    async fn get(
        &self,
        request_builder: &Arc<dyn RequestBuilderTrait>,
        request_context: &AuthorizedRequestContext,
        lcut: u64,
    ) -> DataProviderResult {
        let start_time = Instant::now();
        let mut err_msg: String = String::new();
        let mut body: String = String::new();
        let mut headers: HeaderMap = HeaderMap::new();
        let mut is_unauthorized = false;
        match request_builder
            .make_request(&self.http_client, request_context, lcut)
            .await
        {
            Ok(response) => {
                headers = response.headers().clone();
                let status_code = response.status().as_u16();
                let did_succeed = response.status().is_success();
                match response.bytes().await {
                    Ok(raw_bytes) => {
                        // Warning: If we ever decide to touch the String and not just return it
                        // we should stop using from_utf8_unchecked
                        if did_succeed {
                            body = unsafe { String::from_utf8_unchecked(raw_bytes.into()) };
                        } else {
                            is_unauthorized = status_code == 401 || status_code == 403;
                            err_msg = unsafe { String::from_utf8_unchecked(raw_bytes.into()) };
                        }
                    }
                    Err(err) => {
                        err_msg = err.to_string();
                    }
                }
            }
            Err(err) => {
                err_msg = err.to_string();
            }
        }

        let duration = start_time.elapsed();
        let ms: i64 =
            match i64::try_from(duration.as_secs() * 1000 + (duration.subsec_millis() as u64)) {
                Ok(ms) => ms,
                Err(_err) => -2,
            };
        if err_msg.is_empty() {
            if !request_builder
                .is_an_update(&body, &request_context.sdk_key)
                .await
            {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::HttpDataProviderNoData, request_context)
                        .with_lcut(lcut)
                        .with_stat(EventStat {
                            operation_type: OperationType::Distribution,
                            value: ms,
                        }),
                )
                .await;
                DataProviderResult {
                    result: DataProviderRequestResult::NoDataAvailable,
                    data: Some((Arc::new(body), lcut)),
                }
            } else {
                let since_time = match headers.contains_key("x-since-time") {
                    true => match headers["x-since-time"]
                        .to_str()
                        .expect("We must have a value")
                        // If we fail to parse, pretend that there is no
                        // new data.
                        .parse::<u64>()
                    {
                        Ok(value) => value,
                        Err(_) => {
                            ProxyEventObserver::publish_event(
                                ProxyEvent::new(
                                    ProxyEventType::HttpDataProviderNoDataDueToBadLcut,
                                    request_context,
                                )
                                .with_lcut(lcut)
                                .with_stat(EventStat {
                                    operation_type: OperationType::Distribution,
                                    value: ms,
                                }),
                            )
                            .await;
                            return DataProviderResult {
                                result: DataProviderRequestResult::NoDataAvailable,
                                data: None,
                            };
                        }
                    },
                    false => 0,
                };

                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::HttpDataProviderGotData, request_context)
                        .with_lcut(since_time)
                        .with_stat(EventStat {
                            operation_type: OperationType::Distribution,
                            value: ms,
                        }),
                )
                .await;
                DataProviderResult {
                    result: DataProviderRequestResult::DataAvailable,
                    data: Some((Arc::new(body), since_time)),
                }
            }
        } else {
            eprintln!("Failed to get data from http provider: {:?}", err_msg);
            ProxyEventObserver::publish_event(
                ProxyEvent::new(ProxyEventType::HttpDataProviderError, request_context)
                    .with_lcut(lcut)
                    .with_stat(EventStat {
                        operation_type: OperationType::Distribution,
                        value: ms,
                    }),
            )
            .await;
            if is_unauthorized {
                DataProviderResult {
                    result: DataProviderRequestResult::Unauthorized,
                    data: None,
                }
            } else {
                DataProviderResult {
                    result: DataProviderRequestResult::Error,
                    data: None,
                }
            }
        }
    }
}
