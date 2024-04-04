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
            "[Debug][Event: {:?}] sdk_key: {}, lcut: {}, stat: {:?}",
            event.event_type,
            event.sdk_key,
            match event.lcut {
                Some(lcut) => lcut.to_string(),
                None => "None".to_string(),
            },
            event.stat
        );
    }
}
