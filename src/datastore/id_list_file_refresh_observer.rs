use super::config_spec_store::ConfigSpecForCompany;
use super::data_providers::{DataProviderRequestResult, FullRequestContext, ResponseContext};
use super::id_list_file_store::IdListFileStore;
use super::id_list_manifest::parse_id_list_manifest;
use crate::observers::HttpDataProviderObserverTrait;
use crate::servers::authorized_request_context::{parse_id_list_file_id, AuthorizedRequestContext};
use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::CompressionEncoder;
use async_trait::async_trait;
use futures::stream::{self, StreamExt};
use reqwest::Url;
use std::sync::Arc;

const MAX_CONCURRENT_ID_LIST_FILE_REFRESHES: usize = 8;

pub struct IdListFileRefreshObserver {
    id_list_file_store: Arc<IdListFileStore>,
}

impl IdListFileRefreshObserver {
    pub fn new(id_list_file_store: Arc<IdListFileStore>) -> Self {
        Self { id_list_file_store }
    }

    fn download_request_contexts_from_manifest(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) -> Vec<Arc<AuthorizedRequestContext>> {
        let manifest = match parse_id_list_manifest(&response_context.body) {
            Ok(manifest) => manifest,
            Err(e) => {
                eprintln!("Failed to parse get_id_lists manifest for id-list file refresh: {e}");
                return Vec::new();
            }
        };

        let sdk_key = request_context.sdk_key.clone();
        manifest
            .values()
            .filter_map(|entry| {
                match request_context_from_manifest_url(&sdk_key, &entry.url, entry.size) {
                    Ok(context) => Some(context),
                    Err(e) => {
                        eprintln!(
                            "Skipping id-list file refresh for {} / {}: {}",
                            entry.name, entry.file_id, e
                        );
                        None
                    }
                }
            })
            .filter(|context| !self.id_list_file_store.is_fresh_enough(context))
            .collect()
    }

    fn spawn_refreshes(&self, download_request_contexts: Vec<Arc<AuthorizedRequestContext>>) {
        if download_request_contexts.is_empty() {
            return;
        }

        let id_list_file_store = Arc::clone(&self.id_list_file_store);
        tokio::spawn(async move {
            stream::iter(download_request_contexts)
                .for_each_concurrent(MAX_CONCURRENT_ID_LIST_FILE_REFRESHES, |request_context| {
                    let id_list_file_store = Arc::clone(&id_list_file_store);
                    async move {
                        id_list_file_store
                            .ensure_fresh_enough(&request_context)
                            .await;
                    }
                })
                .await;
        });
    }
}

#[async_trait]
impl HttpDataProviderObserverTrait for IdListFileRefreshObserver {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        let request_context = &request_context.authorized_request_context;
        if request_context.path != NormalizedPath::V1GetIdLists
            || response_context.result_type != DataProviderRequestResult::DataAvailable
        {
            return;
        }

        let download_request_contexts =
            self.download_request_contexts_from_manifest(request_context, response_context);
        self.spawn_refreshes(download_request_contexts);
    }

    async fn get(
        &self,
        _request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        None
    }
}

fn request_context_from_manifest_url(
    sdk_key: &str,
    url: &str,
    id_list_size: u64,
) -> Result<Arc<AuthorizedRequestContext>, String> {
    let url = Url::parse(url).map_err(|e| format!("manifest URL could not be parsed: {e}"))?;
    let raw_path = url.path().to_string();
    let raw_query = url.query().map(str::to_string);
    let (raw_path, file_id) = if let Some(file_id) = parse_id_list_file_id(&raw_path) {
        (raw_path, file_id)
    } else if let Some(file_id) = parse_id_list_file_id_from_blob_path(&raw_path) {
        (format!("/v1/download_id_list_file/{file_id}"), file_id)
    } else {
        return Err("manifest URL does not target /v1/download_id_list_file/<file_id> or /idlists/<file_id>".to_string());
    };

    Ok(Arc::new(
        AuthorizedRequestContext::new(
            sdk_key.to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
        )
        .with_raw_request(Some(raw_path), raw_query)
        .with_id_list_request(Some(file_id), Some(0), Some(id_list_size)),
    ))
}

fn parse_id_list_file_id_from_blob_path(path: &str) -> Option<String> {
    path.strip_prefix("/idlists/")
        .and_then(|rest| rest.split('/').next())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_url_context_preserves_signed_path_query_and_file_cache_key() {
        let request_context = request_context_from_manifest_url(
            "secret-test",
            "https://api.statsigcdn.com/v1/download_id_list_file/company%2Ffile_123?sv=2020&sig=signed%3D&k=secret-test",
            1024,
        )
        .expect("manifest URL should build a request context");

        assert_eq!(request_context.sdk_key, "secret-test");
        assert_eq!(
            request_context.raw_path.as_deref(),
            Some("/v1/download_id_list_file/company%2Ffile_123")
        );
        assert_eq!(
            request_context.raw_query.as_deref(),
            Some("sv=2020&sig=signed%3D&k=secret-test")
        );
        assert_eq!(
            request_context.file_id.as_deref(),
            Some("company%2Ffile_123")
        );
        assert_eq!(request_context.range_start, Some(0));
        assert_eq!(request_context.id_list_size, Some(1024));
        assert_eq!(request_context.path, NormalizedPath::V1DownloadIdListFile);
        assert_eq!(
            request_context.encodings,
            vec![CompressionEncoder::PlainText]
        );
    }

    #[test]
    fn manifest_blob_url_context_maps_blob_path_to_local_download_path_and_file_cache_key() {
        let manifest_url = "https://idliststorage.blob.core.windows.net/idlists/company%2Ffile_123?sv=2020&sig=signed%3D&k=secret-test";
        let request_context = request_context_from_manifest_url("secret-test", manifest_url, 1024)
            .expect("blob manifest URL should build a request context");

        assert_eq!(
            request_context.raw_path.as_deref(),
            Some("/v1/download_id_list_file/company%2Ffile_123")
        );
        assert_eq!(
            request_context.raw_query.as_deref(),
            Some("sv=2020&sig=signed%3D&k=secret-test")
        );
        assert_eq!(
            request_context.file_id.as_deref(),
            Some("company%2Ffile_123")
        );
        assert_eq!(request_context.range_start, Some(0));
        assert_eq!(request_context.id_list_size, Some(1024));
        assert_eq!(request_context.path, NormalizedPath::V1DownloadIdListFile);
    }

    #[test]
    fn manifest_url_context_rejects_unservable_urls() {
        let error =
            request_context_from_manifest_url("secret-test", "https://example.com/blob", 1024)
                .unwrap_err();

        assert!(error.contains("/v1/download_id_list_file"));
    }
}
