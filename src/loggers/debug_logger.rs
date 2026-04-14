use crate::observers::{ProxyEvent, ProxyEventObserverTrait};
use async_trait::async_trait;

pub struct DebugLogger {}

impl Default for DebugLogger {
    fn default() -> Self {
        Self::new()
    }
}

impl DebugLogger {
    pub fn new() -> Self {
        DebugLogger {}
    }
}

#[async_trait]
impl ProxyEventObserverTrait for DebugLogger {
    async fn handle_event(&self, event: &ProxyEvent) {
        println!(
            "[Debug][Event: {:?}] sdk_key: {:?}, lcut: {}, stat: {:?}, path: {:?}, accept-encodings: {:?}, response-encoding: {:?}, status_code: {:?}, sdk_type: {:?}, sdk_version: {:?}, client_service: {:?}, response_payload_type: {:?}",
            event.event_type,
            event.get_sdk_key(),
            match event.lcut {
                Some(lcut) => lcut.to_string(),
                None => "None".to_string(),
            },
            event.stat,
            event.get_path(),
            event.get_accept_encodings(),
            event.response_encoding,
            event.status_code,
            event.sdk_type,
            event.sdk_version,
            event.service,
            event.response_payload_type
        );
    }
}
