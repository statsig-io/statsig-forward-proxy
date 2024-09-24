use crate::datatypes::log_event::{
    EventName, LogEvent, LogEventRequest, PossiblyLogEvent, StatsigMetadata,
};
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{EventStat, OperationType, ProxyEvent, ProxyEventType};
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use chrono::{DateTime, Timelike, Utc};
use futures::future::join_all;
use std::hash::BuildHasher;
use std::{
    collections::{hash_map::RandomState, HashSet},
    sync::Arc,
};

use reqwest::StatusCode;
use rocket::{
    http::Status,
    serde::json::{to_string, Value},
};
use tokio::sync::RwLock;

pub struct LogEventStore {
    url: String,
    http_client: reqwest::Client,
    random_state: RandomState,
    dedupe_cache: Arc<RwLock<HashSet<u64>>>,
    dedupe_cache_limit: usize,
}

impl LogEventStore {
    pub fn new(base_url: &str, http_client: reqwest::Client, dedupe_cache_limit: usize) -> Self {
        LogEventStore {
            url: format!("{}/v1/log_event", base_url),
            http_client,
            random_state: RandomState::new(),
            dedupe_cache: Arc::new(RwLock::new(HashSet::new())),
            dedupe_cache_limit,
        }
    }

    pub async fn log_event(
        &self,
        mut data: LogEventRequest,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Result<String, Status> {
        let batch_in_size = data.events.len();
        data.events = join_all(data.events.into_iter().map(|e| {
            self.process_event(&request_context.sdk_key, data.statsig_metadata.clone(), e)
        }))
        .await
        .into_iter()
        .flatten()
        .collect();
        let batch_out_size = data.events.len();
        let data_string = to_string(&data).map_err(|_| Status::new(500))?;
        // todo: ungzip + deduplicate + batching + regzip
        let response = self
            .http_client
            .post(&self.url)
            .header("statsig-api-key", &request_context.sdk_key)
            .header("statsig-event-count", batch_out_size)
            .body(data_string)
            .send()
            .await;
        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::LogEventStoreDeduped, request_context)
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: (batch_in_size - batch_out_size) as i64,
                }),
        );
        match response {
            Ok(res) => res.text().await,
            Err(e) => Err(e),
        }
        .map_err(|e| Status {
            code: e
                .status()
                .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
                .as_u16(),
        })
    }

    // returns true if this is a new event
    async fn check_key(&self, event: String) -> bool {
        let key = self.random_state.hash_one(event);
        let mut lock = self.dedupe_cache.write().await;
        if lock.len() >= self.dedupe_cache_limit {
            ProxyEventObserver::publish_event(
                ProxyEvent::new(ProxyEventType::LogEventStoreDedupeCacheCleared).with_stat(
                    EventStat {
                        operation_type: OperationType::IncrByValue,
                        value: 1,
                    },
                ),
            );
            lock.clear();
        }
        lock.insert(key)
    }

    async fn process_event(
        &self,
        sdk_key: &str,
        global_metadata: Option<StatsigMetadata>,
        mut event: PossiblyLogEvent,
    ) -> Option<PossiblyLogEvent> {
        match event {
            PossiblyLogEvent::ValidLogEvent(ref mut e) => {
                if e.statsig_metadata.is_none() {
                    e.statsig_metadata = global_metadata;
                }
                match Self::compute_key(sdk_key, e) {
                    Some(s) if self.check_key(s.clone()).await => Some(event),
                    None => Some(event),
                    _ => None,
                }
            }
            PossiblyLogEvent::InvalidLogEvent(_) => Some(event),
        }
    }

    pub fn compute_key(sdk_key: &str, event: &LogEvent) -> Option<String> {
        if let EventName::Other { .. } = event.event_name {
            return None;
        }
        // TODO: add global statsig_metadata and user
        let mut user_key = format!(
            "k:{};u:{};s:{};",
            sdk_key,
            event
                .user
                .as_ref()
                .and_then(|user| user.user_id.as_ref())
                .map_or("", |s| s.as_str()),
            event
                .statsig_metadata
                .as_ref()
                .and_then(|metadata| metadata.stable_id.as_ref())
                .map_or("", |s| s.as_str())
        );

        if let Some(user) = &event.user {
            if let Some(Value::Object(map)) = &user.custom_ids {
                for (k, v) in map.iter() {
                    user_key.push_str(&format!("{}:{};", k, v));
                }
            }
        }

        let mut exposure_key = match event.event_name {
            EventName::ConfigExposure => {
                let config_name = event.get_metadata("config");
                let rule_id = event.get_metadata("ruleID");
                format!("n:${config_name};u:{user_key}r:{rule_id}")
            }
            EventName::GateExposure => {
                let gate_name = event.get_metadata("gate");
                let rule_id = event.get_metadata("ruleID");
                let value = event.get_metadata("gateValue");
                format!("n:{gate_name};u:{user_key}r:{rule_id};v:{value}")
            }
            _ => return None,
        };

        let time = event
            .time
            .and_then(|t| t.try_into().ok())
            .and_then(DateTime::from_timestamp_millis)
            .unwrap_or(Utc::now());
        let time_millis = round_to_minute(time)
            .and_then(|dt| dt.timestamp_millis().try_into().ok())
            .or(event.time);
        match time_millis {
            Some(time) => exposure_key.push_str(&format!(";t{}", time)),
            None => return None, // shouldn't ever happen, but just say unique if it does
        }
        Some(exposure_key)
    }
}

fn round_to_minute(dt: DateTime<Utc>) -> Option<DateTime<Utc>> {
    dt.with_second(0).and_then(|dt| dt.with_nanosecond(0))
}
