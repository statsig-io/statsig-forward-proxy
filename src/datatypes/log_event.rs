use std::collections::HashMap;

use rocket::serde::json::{to_string, Value};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(rename = "userID")]
    pub user_id: Option<String>,
    #[serde(rename = "customIDs")]
    pub custom_ids: Option<Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "eventName")]
pub enum EventName {
    #[serde(rename = "statsig::config_exposure")]
    ConfigExposure,
    #[serde(rename = "statsig::gate_exposure")]
    GateExposure,
    #[serde(untagged)]
    Other {
        #[serde(rename = "eventName")]
        event_name: String,
    },
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEvent {
    #[serde(flatten)]
    pub event_name: EventName,
    pub user: Option<User>,
    pub time: Option<u64>,
    pub metadata: Option<HashMap<String, Value>>,
    pub statsig_metadata: Option<StatsigMetadata>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

impl LogEvent {
    pub fn get_metadata(&self, key: &str) -> String {
        return self
            .metadata
            .as_ref()
            .and_then(|m| m.get(key))
            .and_then(|v| to_string(v).ok())
            .unwrap_or("".to_string());
    }
}

#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatsigMetadata {
    #[serde(rename = "stableID")]
    pub stable_id: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PossiblyLogEvent {
    ValidLogEvent(LogEvent),
    InvalidLogEvent(HashMap<String, Value>),
}
#[skip_serializing_none]
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEventRequest {
    pub events: Vec<PossiblyLogEvent>,
    pub statsig_metadata: Option<StatsigMetadata>,
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
pub struct LogEventResponse {
    pub success: bool,
}
