use std::sync::Arc;

use dashmap::DashMap;
use once_cell::sync::Lazy;
use tokio::time::Instant;

use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{EventStat, OperationType, ProxyEvent, ProxyEventType};
use crate::servers::authorized_request_context::AuthorizedRequestContext;

static BACKGROUND_REQUEST_INTERVAL_TRACKER: Lazy<BackgroundRequestIntervalTracker> =
    Lazy::new(BackgroundRequestIntervalTracker::new);

pub(super) fn publish_completed_request_interval(request_context: &Arc<AuthorizedRequestContext>) {
    BACKGROUND_REQUEST_INTERVAL_TRACKER.publish_completed_request_interval(request_context);
}

struct BackgroundRequestIntervalTracker {
    completed_at_by_context: DashMap<Arc<AuthorizedRequestContext>, Instant>,
}

impl BackgroundRequestIntervalTracker {
    fn new() -> Self {
        Self {
            completed_at_by_context: DashMap::new(),
        }
    }

    fn publish_completed_request_interval(&self, request_context: &Arc<AuthorizedRequestContext>) {
        let Some(interval_ms) = self.record_completed_request_at(request_context, Instant::now())
        else {
            return;
        };

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(
                ProxyEventType::BackgroundDataProviderRequestInterval,
                request_context,
            )
            .with_stat(EventStat {
                operation_type: OperationType::Distribution,
                value: interval_ms,
            }),
        );
    }

    fn record_completed_request_at(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
        completed_at: Instant,
    ) -> Option<i64> {
        self.completed_at_by_context
            .insert(Arc::clone(request_context), completed_at)
            .map(|previous_completed_at| {
                let interval_ms = completed_at
                    .saturating_duration_since(previous_completed_at)
                    .as_millis();
                i64::try_from(interval_ms).unwrap_or(i64::MAX)
            })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::time::{Duration, Instant};

    use crate::servers::authorized_request_context::AuthorizedRequestContext;
    use crate::servers::normalized_path::NormalizedPath;
    use crate::utils::compress_encoder::CompressionEncoder;

    use super::BackgroundRequestIntervalTracker;

    #[test]
    fn record_completed_request_returns_interval_after_first_completion() {
        let request_context = Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip],
        ));
        let tracker = BackgroundRequestIntervalTracker::new();
        let first_completion = Instant::now();
        let second_completion = first_completion + Duration::from_millis(1234);

        assert_eq!(
            tracker.record_completed_request_at(&request_context, first_completion),
            None
        );
        assert_eq!(
            tracker.record_completed_request_at(&request_context, second_completion),
            Some(1234)
        );
    }
}
