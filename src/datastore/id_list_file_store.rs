use super::config_spec_store::ConfigSpecForCompany;
use super::data_providers::background_data_provider::{foreground_fetch, BackgroundDataProvider};
use super::data_providers::{DataProviderRequestResult, FullRequestContext, ResponseContext};
use crate::observers::HttpDataProviderObserverTrait;
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use async_trait::async_trait;
use bytes::Bytes;
use moka::sync::Cache;
use std::num::NonZeroUsize;
use std::sync::Arc;

const DEFAULT_ID_LIST_FILE_CACHE_CAPACITY: usize = 512;

pub struct IdListFileStore {
    store: Cache<String, Arc<Bytes>>,
    background_data_provider: Arc<BackgroundDataProvider>,
}

impl IdListFileStore {
    pub fn new(background_data_provider: Arc<BackgroundDataProvider>) -> Self {
        Self::with_capacity(
            background_data_provider,
            NonZeroUsize::new(DEFAULT_ID_LIST_FILE_CACHE_CAPACITY)
                .expect("DEFAULT_ID_LIST_FILE_CACHE_CAPACITY must be non-zero"),
        )
    }

    fn with_capacity(
        background_data_provider: Arc<BackgroundDataProvider>,
        capacity: NonZeroUsize,
    ) -> Self {
        Self {
            store: Cache::new(capacity.get() as u64),
            background_data_provider,
        }
    }

    fn cached_body_satisfies_request(bytes: &Bytes, rc: &AuthorizedRequestContext) -> bool {
        let cached_len = bytes.len() as u64;

        match rc.range_start {
            // Without a size hint, a ranged request can only be served from cache if the cache can
            // satisfy the requested suffix. Otherwise revalidate in case origin has appended bytes.
            Some(start) if rc.id_list_size.is_none() => cached_len > start,
            // With a size hint, a ranged request can only be served from cache if we have at least
            // one byte beyond the requested start offset, or if the request starts exactly at the
            // manifest's known size and the cache matches it.
            Some(start) => {
                let cached_body_can_satisfy_range = cached_len > start
                    || (cached_len == start && rc.id_list_size == Some(cached_len));

                cached_body_can_satisfy_range
                    && rc
                        .id_list_size
                        .is_none_or(|id_list_size| cached_len >= id_list_size)
            }
            // Between layered SFP hops we fetch the full body, so a no-range request can reuse the
            // cached file when there is no caller size hint or when the cache is at least as new.
            None => rc
                .id_list_size
                .is_none_or(|id_list_size| cached_len >= id_list_size),
        }
    }

    fn get_cached_hit(&self, rc: &AuthorizedRequestContext) -> Option<Arc<Bytes>> {
        self.get_stored_body(rc)
            .filter(|hit| Self::cached_body_satisfies_request(hit.as_ref(), rc))
    }

    fn get_stored_body(&self, rc: &AuthorizedRequestContext) -> Option<Arc<Bytes>> {
        let cache_key = rc.file_id.as_deref()?;
        self.store.get(cache_key)
    }

    pub fn is_fresh_enough(&self, rc: &AuthorizedRequestContext) -> bool {
        self.get_cached_hit(rc).is_some()
    }

    pub async fn ensure_fresh_enough(&self, rc: &Arc<AuthorizedRequestContext>) -> bool {
        if self.is_fresh_enough(rc) {
            return true;
        }

        if rc.file_id.is_none() {
            return false;
        }

        foreground_fetch(Arc::clone(&self.background_data_provider), rc, 0, false).await;
        self.get_cached_hit(rc).is_some()
    }

    pub async fn get_or_fetch(&self, rc: &Arc<AuthorizedRequestContext>) -> Option<Arc<Bytes>> {
        let previous_body = self.get_stored_body(rc);
        if let Some(hit) = previous_body
            .as_ref()
            .filter(|hit| Self::cached_body_satisfies_request(hit.as_ref(), rc))
        {
            return Some(Arc::clone(hit));
        }

        rc.file_id.as_ref()?;

        foreground_fetch(Arc::clone(&self.background_data_provider), rc, 0, false).await;
        self.get_stored_body(rc)
    }
}

#[async_trait]
impl HttpDataProviderObserverTrait for IdListFileStore {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        true
    }

    async fn update(
        &self,
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        let cache_key = match request_context.authorized_request_context.file_id.clone() {
            Some(key) => key,
            None => return,
        };

        match response_context.result_type {
            DataProviderRequestResult::DataAvailable | DataProviderRequestResult::Error => {
                self.store
                    .insert(cache_key, Arc::clone(&response_context.body.data));
            }
            DataProviderRequestResult::Unauthorized => {
                self.store.invalidate(&cache_key);
            }
            _ => {}
        }
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::data_providers::background_data_provider::BackgroundPollDispatchConfig;
    use crate::datastore::data_providers::http_data_provider::HttpDataProvider;
    use crate::datastore::data_providers::HttpClientConfig;
    use crate::datastore::sdk_key_store::SdkKeyStore;
    use crate::servers::normalized_path::NormalizedPath;
    use crate::utils::compress_encoder::CompressionEncoder;
    use std::num::NonZeroUsize;

    fn make_rc(
        raw_path: Option<&str>,
        raw_query: Option<&str>,
        file_id: Option<&str>,
        range_start: Option<u64>,
        id_list_size: Option<u64>,
    ) -> Arc<AuthorizedRequestContext> {
        Arc::new(
            AuthorizedRequestContext::new(
                "sdk-key-test".to_string(),
                NormalizedPath::V1DownloadIdListFile,
                vec![CompressionEncoder::PlainText],
            )
            .with_raw_request(raw_path.map(str::to_string), raw_query.map(str::to_string))
            .with_id_list_request(
                file_id.map(str::to_string),
                range_start,
                id_list_size,
            ),
        )
    }

    fn make_background_data_provider() -> Arc<BackgroundDataProvider> {
        Arc::new(BackgroundDataProvider::new(
            Arc::new(HttpDataProvider {}),
            10,
            Arc::new(SdkKeyStore::new()),
            false,
            HttpClientConfig::default(),
            BackgroundPollDispatchConfig::new(1, 0),
        ))
    }

    #[test]
    fn file_id_is_used_as_cache_key_when_present() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/3wHgh0FhoQH0p7RvCFqgqJ%2F3zrTq6LjhhQpuTjegJ1X8P"),
            None,
            Some("3wHgh0FhoQH0p7RvCFqgqJ%2F3zrTq6LjhhQpuTjegJ1X8P"),
            None,
            Some(0),
        );
        assert_eq!(
            rc.file_id.as_deref(),
            Some("3wHgh0FhoQH0p7RvCFqgqJ%2F3zrTq6LjhhQpuTjegJ1X8P")
        );
    }

    #[test]
    fn file_id_cache_key_ignores_range_size_and_query() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            Some("sv=2020-10-02&range=270-&sig=abc%3D&k=secret-test"),
            Some("file_123"),
            Some(270),
            Some(512),
        );
        assert_eq!(rc.file_id.as_deref(), Some("file_123"));
    }

    #[test]
    fn file_id_cache_key_ignores_volatile_signed_query_params() {
        let rc_a = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            Some("sv=2020-10-02&sig=abc%3D&k=secret-test"),
            Some("file_123"),
            Some(270),
            Some(270),
        );
        let rc_b = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            Some("sv=2020-10-02&sig=def%3D&k=secret-test"),
            Some("file_123"),
            Some(1024),
            Some(270),
        );

        assert_eq!(rc_a.file_id, rc_b.file_id);
    }

    #[test]
    fn file_id_cache_key_is_none_when_file_id_is_missing() {
        let rc = make_rc(Some("/v1/download_id_list_file/"), None, None, None, None);
        assert_eq!(rc.file_id, None);
    }

    #[test]
    fn cached_body_satisfies_request_rejects_stale_short_body() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(6),
            Some(6),
        );

        assert!(!IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_accepts_equal_range_boundary_when_size_matches_cache() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(3),
            Some(3),
        );

        assert!(IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_rejects_equal_range_boundary_without_size_hint() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(3),
            None,
        );

        assert!(!IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_accepts_cached_suffix_for_range() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(2),
            Some(2),
        );

        assert!(IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_rejects_ranged_request_when_size_hint_is_newer_than_cache() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(2),
            Some(4),
        );

        assert!(!IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_accepts_full_refresh_when_size_matches_cache() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(0),
            Some(3),
        );

        assert!(IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_accepts_full_refresh_when_cache_is_newer_than_manifest() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(0),
            Some(3),
        );

        assert!(IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abcdef"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_accepts_cached_body_when_headers_are_missing() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            None,
            None,
        );

        assert!(IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_accepts_size_only_match() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            None,
            Some(3),
        );

        assert!(IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_accepts_size_only_when_cache_is_newer_than_client() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            None,
            Some(2),
        );

        assert!(IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_rejects_size_only_when_cache_is_older_than_client() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            None,
            Some(4),
        );

        assert!(!IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn cached_body_satisfies_request_rejects_full_refresh_when_cache_is_older_than_manifest() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(0),
            Some(4),
        );

        assert!(!IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[tokio::test]
    async fn ensure_fresh_enough_skips_when_cached_file_satisfies_manifest_size() {
        let store = IdListFileStore::with_capacity(
            make_background_data_provider(),
            NonZeroUsize::new(2).unwrap(),
        );
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(0),
            Some(3),
        );
        store
            .store
            .insert("file_123".to_string(), Arc::new(Bytes::from_static(b"abc")));

        assert!(store.ensure_fresh_enough(&rc).await);
    }

    #[tokio::test]
    async fn ensure_fresh_enough_skips_when_file_id_is_missing() {
        let store = IdListFileStore::with_capacity(
            make_background_data_provider(),
            NonZeroUsize::new(2).unwrap(),
        );
        let rc = make_rc(
            Some("/v1/download_id_list_file/"),
            None,
            None,
            Some(0),
            Some(3),
        );

        assert!(!store.ensure_fresh_enough(&rc).await);
    }

    #[test]
    fn cached_body_satisfies_request_forces_revalidation_when_size_is_missing() {
        let rc = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            None,
            Some("file_123"),
            Some(1),
            None,
        );

        assert!(IdListFileStore::cached_body_satisfies_request(
            &Bytes::from_static(b"abc"),
            rc.as_ref()
        ));
    }

    #[test]
    fn file_id_cache_key_returns_none_for_missing_or_invalid_raw_path() {
        let no_raw_path = make_rc(None, None, None, None, None);
        assert_eq!(no_raw_path.file_id, None);

        let empty_file_id = make_rc(Some("/v1/download_id_list_file/"), None, None, None, None);
        assert_eq!(empty_file_id.file_id, None);

        let wrong_prefix = make_rc(Some("/v1/get_id_lists"), None, None, None, None);
        assert_eq!(wrong_prefix.file_id, None);
    }

    #[test]
    fn moka_admission_keeps_existing_entries_when_capacity_is_reached() {
        let store = IdListFileStore::with_capacity(
            make_background_data_provider(),
            NonZeroUsize::new(2).unwrap(),
        );
        store
            .store
            .insert("file_1".to_string(), Arc::new(Bytes::from_static(b"a")));
        store
            .store
            .insert("file_2".to_string(), Arc::new(Bytes::from_static(b"b")));
        store.store.run_pending_tasks();
        store
            .store
            .insert("file_3".to_string(), Arc::new(Bytes::from_static(b"c")));
        store
            .store
            .insert("file_4".to_string(), Arc::new(Bytes::from_static(b"d")));

        store.store.run_pending_tasks();

        assert_eq!(store.store.entry_count(), 2);
        assert!(store.store.contains_key("file_1"));
        assert!(store.store.contains_key("file_2"));
        assert!(!store.store.contains_key("file_3"));
        assert!(!store.store.contains_key("file_4"));
    }
}
