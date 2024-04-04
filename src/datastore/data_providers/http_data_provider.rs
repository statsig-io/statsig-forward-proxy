use std::sync::Arc;

use super::{DataProviderRequestResult, DataProviderResult, DataProviderTrait};
use crate::observers::EventStat;
use crate::observers::OperationType;
use crate::observers::{proxy_event_observer::ProxyEventObserver, ProxyEvent, ProxyEventType};
use reqwest::Client;
pub trait DataProviderObserver {
    fn update(&self, key: &str, data: &str);
}

pub struct HttpDataProvider {
    http_client: reqwest::Client,
    url: String,
}

impl HttpDataProvider {
    pub fn new(url: &str) -> Self {
        HttpDataProvider {
            http_client: Client::builder()
                .pool_idle_timeout(None)
                .build()
                .expect("We must have an http client"),
            url: url.to_string(),
        }
    }
}

use async_trait::async_trait;
use tokio::time::Instant;
#[async_trait]
impl DataProviderTrait for HttpDataProvider {
    async fn get(&self, key: &str, lcut: u64) -> DataProviderResult {
        let start_time = Instant::now();
        let url = match lcut == 0 {
            true => format!("{}/v1/download_config_specs/{}.json", self.url, key),
            false => format!(
                "{}/v1/download_config_specs/{}.json?sinceTime={}",
                self.url, key, lcut
            ),
        };
        let response = self
            .http_client
            .get(url)
            .send()
            .await
            .expect("We must have a response");
        let did_succeed = response.status().is_success();
        let headers = response.headers().clone();
        let raw_bytes = response
            .bytes()
            .await
            .expect("Body should always be bytes for DCS");
        // Warning: If we ever decide to touch the String and not just return it
        // we should stop using from_utf8_unchecked
        let body = unsafe { String::from_utf8_unchecked(raw_bytes.into()) };
        let duration = start_time.elapsed();
        let ms: i64 =
            match i64::try_from(duration.as_secs() * 1000 + (duration.subsec_millis() as u64)) {
                Ok(ms) => ms,
                Err(_err) => -2,
            };
        if did_succeed {
            // TODO: This should be more robust
            if body == "{\"has_updates\":false}" {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::HttpDataProviderNoData, key.to_string())
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
                                    key.to_string(),
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
                    ProxyEvent::new(ProxyEventType::HttpDataProviderGotData, key.to_string())
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
            eprintln!("Failed to get data from http provider: {:?}", body);
            ProxyEventObserver::publish_event(
                ProxyEvent::new(ProxyEventType::HttpDataProviderError, key.to_string())
                    .with_lcut(lcut)
                    .with_stat(EventStat {
                        operation_type: OperationType::Distribution,
                        value: ms,
                    }),
            )
            .await;
            DataProviderResult {
                result: DataProviderRequestResult::Error,
                data: None,
            }
        }
    }
}
