use async_trait::async_trait;

use parking_lot::RwLock;
use reqwest::header::{HeaderMap, HeaderValue};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc};
use tokio::time::{Duration, Instant};

use crate::{
    datastore::data_providers::dcs_blob_url_generator::{DcsBlobUrlGenerator, DcsBlobVersion},
    observers::{
        http_data_provider_observer::HttpDataProviderObserver,
        proxy_event_observer::ProxyEventObserver, EventStat, HttpDataProviderObserverTrait,
        OperationType, ProxyEvent, ProxyEventType,
    },
    servers::{
        authorized_request_context::AuthorizedRequestContext, normalized_path::NormalizedPath,
    },
    utils::compress_encoder::{format_compression_encodings, CompressionEncoder},
};

use once_cell::sync::Lazy;

type RequestBuilderCache = Lazy<Arc<RwLock<HashMap<NormalizedPath, Arc<dyn RequestBuilderTrait>>>>>;

static REQUEST_BUILDERS: RequestBuilderCache = Lazy::new(|| Arc::new(RwLock::new(HashMap::new())));
const SFP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub enum RequestBuilderOutcome {
    Response(reqwest::Response),
    NoDataAvailable { status_code: u16 },
}

fn maybe_outbound_accept_encoding(encodings: &[CompressionEncoder]) -> Option<String> {
    if !encodings
        .iter()
        .any(|encoding| *encoding != CompressionEncoder::PlainText)
    {
        return None;
    }

    let value = format_compression_encodings(encodings);
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn build_idlist_url(base_url: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    format!("{trimmed}/v1/get_id_lists")
}

pub struct CachedRequestBuilders {}

impl CachedRequestBuilders {
    pub fn add_request_builder(
        path: NormalizedPath,
        request_builder: Arc<dyn RequestBuilderTrait>,
    ) {
        let mut lock = REQUEST_BUILDERS.write();
        lock.insert(path, request_builder);
    }

    pub fn get_request_builder(path: &NormalizedPath) -> Arc<dyn RequestBuilderTrait> {
        let lock = REQUEST_BUILDERS.read();
        lock.get(path).cloned().unwrap_or_else(|| {
            eprintln!("No request builder found for path: {}", path.as_str());
            Arc::new(NoopRequestBuilder {})
        })
    }
}

#[async_trait]
pub trait RequestBuilderTrait: Send + Sync + 'static {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> Result<RequestBuilderOutcome, reqwest::Error>;
    async fn is_an_update(
        &self,
        body: &[u8],
        headers: &reqwest::header::HeaderMap,
        rc: &Arc<AuthorizedRequestContext>,
    ) -> bool;
    fn get_observers(&self) -> Arc<HttpDataProviderObserver>;
    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send>;
    async fn should_make_request(&self, rc: &Arc<AuthorizedRequestContext>) -> bool;
}

pub struct NoopRequestBuilder {}

#[async_trait]
impl RequestBuilderTrait for NoopRequestBuilder {
    async fn make_request(
        &self,
        _http_client: &reqwest::Client,
        _request_context: &Arc<AuthorizedRequestContext>,
        _lcut: u64,
    ) -> Result<RequestBuilderOutcome, reqwest::Error> {
        unimplemented!()
    }

    async fn is_an_update(
        &self,
        _body: &[u8],
        _headers: &reqwest::header::HeaderMap,
        _rc: &Arc<AuthorizedRequestContext>,
    ) -> bool {
        unimplemented!()
    }

    fn get_observers(&self) -> Arc<HttpDataProviderObserver> {
        unimplemented!()
    }

    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send> {
        unimplemented!()
    }

    async fn should_make_request(&self, _rc: &Arc<AuthorizedRequestContext>) -> bool {
        false
    }
}

pub struct DcsRequestBuilder {
    pub base_url: String,
    pub http_observers: Arc<HttpDataProviderObserver>,
    pub backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    pub dcs_blob_config: Option<DcsBlobConfig>,
}

pub struct DcsBlobConfig {
    pub url_generator: Arc<DcsBlobUrlGenerator>,
    pub company_id: String,
}

impl DcsRequestBuilder {
    pub fn new(
        base_url: String,
        http_observers: Arc<HttpDataProviderObserver>,
        backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
        dcs_blob_config: Option<DcsBlobConfig>,
    ) -> DcsRequestBuilder {
        DcsRequestBuilder {
            base_url,
            http_observers,
            backup_cache,
            dcs_blob_config,
        }
    }

    fn build_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> reqwest::RequestBuilder {
        if let Some(blob_request) = self.build_blob_body_request(http_client, request_context) {
            return blob_request;
        }

        self.build_cdn_request(http_client, request_context, lcut)
    }

    fn build_cdn_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> reqwest::RequestBuilder {
        let url = format!(
            "{}{}{}.json",
            self.base_url, request_context.path, request_context.sdk_key
        );

        let mut query_params = HashMap::new();
        let mut headers = HeaderMap::new();
        if lcut > 0 {
            query_params.insert("sinceTime", lcut.to_string());
        }
        if request_context.supports_proto {
            query_params.insert("supports_proto", "true".to_string());
            headers.insert("statsig-supports-proto", HeaderValue::from_static("true"));
        }

        headers.insert("x-sfp-version", HeaderValue::from_static(SFP_VERSION));

        let mut request = http_client.get(url).query(&query_params).headers(headers);

        if let Some(accept_encoding) = maybe_outbound_accept_encoding(&request_context.encodings) {
            request = request.header(reqwest::header::ACCEPT_ENCODING, accept_encoding);
        }

        request
    }

    fn build_blob_body_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<reqwest::RequestBuilder> {
        let version = match request_context.path {
            NormalizedPath::V1DownloadConfigSpecs => DcsBlobVersion::V1,
            NormalizedPath::V2DownloadConfigSpecs => DcsBlobVersion::V2,
            _ => return None,
        };

        self.dcs_blob_config.as_ref().and_then(|config| {
            config
                .url_generator
                .build_download_url(version, &config.company_id, &request_context.sdk_key)
                .map(|url| {
                    let mut request = http_client.get(url).header("x-sfp-version", SFP_VERSION);
                    if let Some(accept_encoding) =
                        maybe_outbound_accept_encoding(&request_context.encodings)
                    {
                        request = request.header(reqwest::header::ACCEPT_ENCODING, accept_encoding);
                    }
                    request
                })
                .map_err(|error| {
                    eprintln!("Failed to build signed blob URL for DCS body request: {error}");
                })
                .ok()
        })
    }

    fn build_blob_metadata_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<reqwest::RequestBuilder> {
        let version = match request_context.path {
            NormalizedPath::V1DownloadConfigSpecs => DcsBlobVersion::V1,
            NormalizedPath::V2DownloadConfigSpecs => DcsBlobVersion::V2,
            _ => return None,
        };

        self.dcs_blob_config.as_ref().and_then(|config| {
            config
                .url_generator
                .build_download_url(version, &config.company_id, &request_context.sdk_key)
                .map(|url| http_client.head(url).header("x-sfp-version", SFP_VERSION))
                .map_err(|error| {
                    eprintln!("Failed to build signed blob URL for DCS metadata request: {error}");
                })
                .ok()
        })
    }

    fn parse_blob_metadata_lcut(headers: &HeaderMap) -> Option<u64> {
        headers
            .get("x-ms-meta-lcut")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
    }

    fn should_skip_blob_download(current_lcut: u64, blob_lcut: u64) -> bool {
        current_lcut > 0 && blob_lcut <= current_lcut
    }

    fn should_fallback_to_cdn(status: reqwest::StatusCode) -> bool {
        status == reqwest::StatusCode::NOT_FOUND
    }

    fn publish_dcs_fetch_source_event(
        request_context: &Arc<AuthorizedRequestContext>,
        source: &str,
        status_code: u16,
    ) {
        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(ProxyEventType::DcsFetchSource, request_context)
                .with_status_code(status_code)
                .with_service(source.to_string())
                .with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
        );
    }

    async fn should_skip_download_using_metadata(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> bool {
        if !request_context.use_lcut || lcut == 0 {
            return false;
        }

        let Some(metadata_request) = self.build_blob_metadata_request(http_client, request_context)
        else {
            return false;
        };

        let response = match metadata_request.send().await {
            Ok(response) => response,
            Err(error) => {
                eprintln!("Failed to fetch DCS blob metadata: {error}");
                return false;
            }
        };

        if !response.status().is_success() {
            return false;
        }

        let Some(blob_lcut) = Self::parse_blob_metadata_lcut(response.headers()) else {
            return false;
        };

        Self::should_skip_blob_download(lcut, blob_lcut)
    }
}

#[async_trait]
impl RequestBuilderTrait for DcsRequestBuilder {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
    ) -> Result<RequestBuilderOutcome, reqwest::Error> {
        if request_context.use_lcut
            && lcut > 0
            && self
                .should_skip_download_using_metadata(http_client, request_context, lcut)
                .await
        {
            return Ok(RequestBuilderOutcome::NoDataAvailable { status_code: 204 });
        }

        if let Some(blob_request) = self.build_blob_body_request(http_client, request_context) {
            let blob_response = blob_request.send().await?;
            if Self::should_fallback_to_cdn(blob_response.status()) {
                let cdn_response = self
                    .build_cdn_request(http_client, request_context, lcut)
                    .send()
                    .await?;
                Self::publish_dcs_fetch_source_event(
                    request_context,
                    "cdn_fallback",
                    cdn_response.status().as_u16(),
                );
                return Ok(RequestBuilderOutcome::Response(cdn_response));
            }
            Self::publish_dcs_fetch_source_event(
                request_context,
                "blob",
                blob_response.status().as_u16(),
            );
            return Ok(RequestBuilderOutcome::Response(blob_response));
        }

        let cdn_response = self
            .build_request(http_client, request_context, lcut)
            .send()
            .await?;
        Self::publish_dcs_fetch_source_event(
            request_context,
            "cdn",
            cdn_response.status().as_u16(),
        );
        Ok(RequestBuilderOutcome::Response(cdn_response))
    }

    async fn is_an_update(
        &self,
        _body: &[u8],
        headers: &reqwest::header::HeaderMap,
        _rc: &Arc<AuthorizedRequestContext>,
    ) -> bool {
        // If this header is not present, we default to "Yes, this is an update"
        // Otherwise, this header will be "true" if this is NOT an update.
        headers
            .get("x-cache-hit")
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| value == "false")
    }

    fn get_observers(&self) -> Arc<HttpDataProviderObserver> {
        Arc::clone(&self.http_observers)
    }

    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send> {
        Arc::clone(&self.backup_cache)
    }

    async fn should_make_request(&self, _rc: &Arc<AuthorizedRequestContext>) -> bool {
        true
    }
}

pub struct IdlistRequestBuilder {
    pub base_url: String,
    pub http_observers: Arc<HttpDataProviderObserver>,
    pub backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    last_request_by_key: RwLock<HashMap<Arc<AuthorizedRequestContext>, Instant>>,
    last_response_hash: RwLock<HashMap<Arc<AuthorizedRequestContext>, String>>,
}

pub struct IdListFileRequestBuilder {
    pub base_url: String,
    pub http_observers: Arc<HttpDataProviderObserver>,
    // Disabled for download single id list file; we do not serve stale fallback payloads here.
    pub backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
}

impl IdListFileRequestBuilder {
    pub fn new(
        base_url: String,
        http_observers: Arc<HttpDataProviderObserver>,
        backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    ) -> Self {
        Self {
            base_url,
            http_observers,
            backup_cache,
        }
    }

    fn build_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> reqwest::RequestBuilder {
        let raw_path = request_context
            .raw_path
            .as_deref()
            .unwrap_or(request_context.path.as_str());
        let mut url = format!("{}{}", self.base_url, raw_path);
        if let Some(q) = request_context.raw_query.as_deref() {
            url.push('?');
            url.push_str(q);
        }

        let mut request = http_client.get(url).header("x-sfp-version", SFP_VERSION);

        // Preserve explicit full-refresh semantics across SFP hops, but do not forward non-zero
        // ranges because layer-to-layer cache updates store whole bodies.
        if request_context.range_start == Some(0) {
            request = request.header(reqwest::header::RANGE, "bytes=0-");
        }

        if let Some(id_list_size) = request_context.id_list_size {
            request = request.header("statsig-id-list-file-size", id_list_size.to_string());
        }

        request
    }
}

#[async_trait]
impl RequestBuilderTrait for IdListFileRequestBuilder {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        _lcut: u64,
    ) -> Result<RequestBuilderOutcome, reqwest::Error> {
        self.build_request(http_client, request_context)
            .send()
            .await
            .map(RequestBuilderOutcome::Response)
    }

    async fn is_an_update(
        &self,
        _body: &[u8],
        _headers: &reqwest::header::HeaderMap,
        _rc: &Arc<AuthorizedRequestContext>,
    ) -> bool {
        true
    }

    fn get_observers(&self) -> Arc<HttpDataProviderObserver> {
        Arc::clone(&self.http_observers)
    }

    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send> {
        Arc::clone(&self.backup_cache)
    }

    async fn should_make_request(&self, _rc: &Arc<AuthorizedRequestContext>) -> bool {
        true
    }
}

impl IdlistRequestBuilder {
    pub fn new(
        base_url: String,
        http_observers: Arc<HttpDataProviderObserver>,
        backup_cache: Arc<dyn HttpDataProviderObserverTrait + Sync + Send>,
    ) -> IdlistRequestBuilder {
        IdlistRequestBuilder {
            base_url,
            http_observers,
            backup_cache,
            last_request_by_key: RwLock::new(HashMap::new()),
            last_response_hash: RwLock::new(HashMap::new()),
        }
    }

    fn build_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> reqwest::RequestBuilder {
        let url = build_idlist_url(&self.base_url);

        let mut request = http_client
            .post(url)
            .header("x-sfp-version", SFP_VERSION)
            .header("statsig-api-key", request_context.sdk_key.clone())
            .body("{}".to_string());

        if request_context
            .encodings
            .contains(&CompressionEncoder::Gzip)
        {
            request = request.header(
                reqwest::header::ACCEPT_ENCODING,
                CompressionEncoder::Gzip.to_string(),
            );
        }

        request
    }
}

#[async_trait]
impl RequestBuilderTrait for IdlistRequestBuilder {
    async fn make_request(
        &self,
        http_client: &reqwest::Client,
        request_context: &Arc<AuthorizedRequestContext>,
        _lcut: u64,
    ) -> Result<RequestBuilderOutcome, reqwest::Error> {
        match self
            .build_request(http_client, request_context)
            .send()
            .await
        {
            Ok(response) => {
                let status_code = response.status().as_u16();
                // If unauthorized, remove key from last response hash such that
                // we will reload data into memory if for some reason the key is
                // re-authorized
                if status_code == 401 || status_code == 403 {
                    self.last_response_hash
                        .write()
                        .remove(Arc::as_ref(request_context));
                }
                Ok(RequestBuilderOutcome::Response(response))
            }
            Err(e) => Err(e),
        }
    }

    async fn is_an_update(
        &self,
        body: &[u8],
        _headers: &reqwest::header::HeaderMap,
        rc: &Arc<AuthorizedRequestContext>,
    ) -> bool {
        let hash = format!("{:x}", Sha256::digest(body));
        let mut wlock = self.last_response_hash.write();
        let mut is_an_update = true;
        if let Some(old_hash) = wlock.get(rc) {
            is_an_update = hash != *old_hash;
        }

        if is_an_update {
            wlock.insert(Arc::clone(rc), hash);
        }

        is_an_update
    }

    fn get_observers(&self) -> Arc<HttpDataProviderObserver> {
        Arc::clone(&self.http_observers)
    }

    fn get_backup_cache(&self) -> Arc<dyn HttpDataProviderObserverTrait + Sync + Send> {
        Arc::clone(&self.backup_cache)
    }

    async fn should_make_request(&self, rc: &Arc<AuthorizedRequestContext>) -> bool {
        let mut wlock = self.last_request_by_key.write();
        match wlock.get_mut(rc) {
            Some(last_request) => {
                if last_request.elapsed() > Duration::from_secs(60) {
                    wlock.insert(Arc::clone(rc), Instant::now());
                    return true;
                }

                return false;
            }
            None => {
                wlock.insert(Arc::clone(rc), Instant::now());
                return true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_idlist_url, maybe_outbound_accept_encoding, DcsBlobConfig, DcsRequestBuilder,
        IdListFileRequestBuilder, IdlistRequestBuilder,
    };
    use crate::datastore::caching::disabled_cache;
    use crate::datastore::data_providers::dcs_blob_url_generator::DcsBlobUrlGenerator;
    use crate::observers::http_data_provider_observer::HttpDataProviderObserver;
    use crate::servers::{
        authorized_request_context::AuthorizedRequestContext, normalized_path::NormalizedPath,
    };
    use crate::utils::compress_encoder::CompressionEncoder;
    use std::sync::Arc;

    fn make_request_context(path: NormalizedPath) -> Arc<AuthorizedRequestContext> {
        Arc::new(
            AuthorizedRequestContext::new(
                "secret-test".to_string(),
                path,
                vec![CompressionEncoder::Gzip],
            )
            .with_request_capabilities(true, false),
        )
    }

    fn make_id_list_file_request_context(
        range_start: Option<u64>,
        id_list_size: Option<u64>,
    ) -> Arc<AuthorizedRequestContext> {
        Arc::new(
            AuthorizedRequestContext::new(
                "secret-test".to_string(),
                NormalizedPath::V1DownloadIdListFile,
                vec![CompressionEncoder::PlainText],
            )
            .with_raw_request(
                Some("/v1/download_id_list_file/file_123".to_string()),
                Some("k=secret-test&sig=signed-value".to_string()),
            )
            .with_id_list_request(
                Some("file_123".to_string()),
                range_start,
                id_list_size,
            ),
        )
    }

    #[test]
    fn outbound_accept_encoding_omits_plain_text_only() {
        let value = maybe_outbound_accept_encoding(&[CompressionEncoder::PlainText]);
        assert_eq!(value, None);
    }

    #[test]
    fn outbound_accept_encoding_omits_empty_encodings() {
        let value = maybe_outbound_accept_encoding(&[]);
        assert_eq!(value, None);
    }

    #[test]
    fn outbound_accept_encoding_sets_header_for_compression() {
        let value = maybe_outbound_accept_encoding(&[
            CompressionEncoder::Gzip,
            CompressionEncoder::PlainText,
        ]);
        assert_eq!(value, Some("gzip,plain_text".to_string()));
    }

    #[test]
    fn build_idlist_url_handles_trailing_slashes() {
        let value = build_idlist_url("https://api.statsigcdn.com///");
        assert_eq!(value, "https://api.statsigcdn.com/v1/get_id_lists");
    }

    #[test]
    fn build_idlist_url_uses_override_base_url() {
        let value = build_idlist_url("https://override.example.com");
        assert_eq!(value, "https://override.example.com/v1/get_id_lists");
    }

    #[test]
    fn dcs_request_builder_does_not_set_per_request_timeout_override() {
        let builder = DcsRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
            None,
        );
        let http_client = reqwest::Client::new();
        let request_context = make_request_context(NormalizedPath::V1DownloadConfigSpecs);
        let request = builder
            .build_request(&http_client, &request_context, 123)
            .build()
            .expect("request should build");

        assert_eq!(request.timeout(), None);
    }

    #[test]
    fn dcs_request_builder_uses_cdn_url() {
        let builder = DcsRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
            None,
        );
        let http_client = reqwest::Client::new();
        let request_context = make_request_context(NormalizedPath::V1DownloadConfigSpecs);
        let request = builder
            .build_request(&http_client, &request_context, 123)
            .build()
            .expect("request should build");

        let request_url = request.url().as_str();
        assert!(request_url
            .starts_with("https://api.statsigcdn.com/v1/download_config_specs/secret-test.json?"));
        assert!(request_url.contains("sinceTime=123"));
        assert!(request_url.contains("supports_proto=true"));
    }

    #[test]
    fn dcs_request_builder_includes_since_time_for_direct_delta_polling() {
        let builder = DcsRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
            None,
        );
        let http_client = reqwest::Client::new();
        let request_context = make_request_context(NormalizedPath::V2DownloadConfigSpecsDeltas);
        let request = builder
            .build_request(&http_client, &request_context, 123)
            .build()
            .expect("request should build");

        let request_url = request.url().as_str();
        assert!(request_url.starts_with(
            "https://api.statsigcdn.com/v2/download_config_specs_deltas/secret-test.json?"
        ));
        assert!(request_url.contains("sinceTime=123"));
        assert!(request_url.contains("supports_proto=true"));
    }

    #[test]
    fn dcs_request_builder_uses_signed_blob_url_for_metadata_precheck() {
        let blob_url_generator = DcsBlobUrlGenerator::from_connection_string(
            "DefaultEndpointsProtocol=https;AccountName=idliststorage;AccountKey=ZmFrZS1rZXk=;EndpointSuffix=core.windows.net",
        )
        .expect("connection string should parse");
        let builder = DcsRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
            Some(DcsBlobConfig {
                url_generator: Arc::new(blob_url_generator),
                company_id: "50aWbk2p4R76rNX9lN5VUw".to_string(),
            }),
        );
        let http_client = reqwest::Client::new();
        let request_context = make_request_context(NormalizedPath::V1DownloadConfigSpecs);
        let request = builder
            .build_blob_metadata_request(&http_client, &request_context)
            .expect("metadata request should build")
            .build()
            .expect("request should build");

        assert_eq!(request.method(), reqwest::Method::HEAD);
        assert!(request.url().as_str().starts_with(
            "https://idliststorage.blob.core.windows.net/dcs-v1/50aWbk2p4R76rNX9lN5VUw/secret-test"
        ));
        assert!(request.url().as_str().contains("sig="));
    }

    #[test]
    fn dcs_request_builder_uses_signed_blob_url_for_body_download() {
        let blob_url_generator = DcsBlobUrlGenerator::from_connection_string(
            "DefaultEndpointsProtocol=https;AccountName=idliststorage;AccountKey=ZmFrZS1rZXk=;EndpointSuffix=core.windows.net",
        )
        .expect("connection string should parse");
        let builder = DcsRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
            Some(DcsBlobConfig {
                url_generator: Arc::new(blob_url_generator),
                company_id: "50aWbk2p4R76rNX9lN5VUw".to_string(),
            }),
        );
        let http_client = reqwest::Client::new();
        let request_context = make_request_context(NormalizedPath::V1DownloadConfigSpecs);
        let request = builder
            .build_request(&http_client, &request_context, 123)
            .build()
            .expect("request should build");

        assert!(request.url().as_str().starts_with(
            "https://idliststorage.blob.core.windows.net/dcs-v1/50aWbk2p4R76rNX9lN5VUw/secret-test"
        ));
        assert!(request.url().as_str().contains("sig="));
        assert!(!request.url().as_str().contains("sinceTime="));
        assert!(!request.url().as_str().contains("supports_proto=true"));
    }

    #[test]
    fn dcs_request_builder_skips_blob_download_when_blob_lcut_is_not_newer() {
        assert!(DcsRequestBuilder::should_skip_blob_download(100, 100));
        assert!(DcsRequestBuilder::should_skip_blob_download(100, 99));
    }

    #[test]
    fn dcs_request_builder_does_not_skip_blob_download_when_blob_lcut_is_newer() {
        assert!(!DcsRequestBuilder::should_skip_blob_download(100, 101));
    }

    #[test]
    fn dcs_request_builder_falls_back_to_cdn_only_on_404() {
        assert!(DcsRequestBuilder::should_fallback_to_cdn(
            reqwest::StatusCode::NOT_FOUND
        ));
        assert!(!DcsRequestBuilder::should_fallback_to_cdn(
            reqwest::StatusCode::UNAUTHORIZED
        ));
        assert!(!DcsRequestBuilder::should_fallback_to_cdn(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        ));
    }

    #[test]
    fn idlist_request_builder_does_not_set_per_request_timeout_override() {
        let builder = IdlistRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
        );
        let http_client = reqwest::Client::new();
        let request_context = make_request_context(NormalizedPath::V1GetIdLists);
        let request = builder
            .build_request(&http_client, &request_context)
            .build()
            .expect("request should build");

        assert_eq!(request.timeout(), None);
    }

    #[test]
    fn idlist_request_builder_sets_accept_encoding_for_gzip_requests() {
        let builder = IdlistRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
        );
        let http_client = reqwest::Client::new();
        let request_context = Arc::new(AuthorizedRequestContext::new(
            "secret-test".to_string(),
            NormalizedPath::V1GetIdLists,
            vec![CompressionEncoder::Gzip, CompressionEncoder::PlainText],
        ));
        let request = builder
            .build_request(&http_client, &request_context)
            .build()
            .expect("request should build");

        assert_eq!(
            request
                .headers()
                .get(reqwest::header::ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
    }

    #[test]
    fn idlist_request_builder_does_not_forward_statsig_brotli() {
        let builder = IdlistRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
        );
        let http_client = reqwest::Client::new();
        let request_context = Arc::new(AuthorizedRequestContext::new(
            "secret-test".to_string(),
            NormalizedPath::V1GetIdLists,
            vec![CompressionEncoder::StatsigBrotli, CompressionEncoder::Gzip],
        ));
        let request = builder
            .build_request(&http_client, &request_context)
            .build()
            .expect("request should build");

        assert_eq!(
            request
                .headers()
                .get(reqwest::header::ACCEPT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("gzip")
        );
    }

    #[test]
    fn id_list_file_request_builder_forwards_size_hint_but_not_nonzero_range() {
        let builder = IdListFileRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
        );
        let http_client = reqwest::Client::new();
        let request_context = make_id_list_file_request_context(Some(270), Some(1024));
        let request = builder
            .build_request(&http_client, &request_context)
            .build()
            .expect("request should build");

        assert_eq!(
            request.url().as_str(),
            "https://api.statsigcdn.com/v1/download_id_list_file/file_123?k=secret-test&sig=signed-value"
        );
        assert!(request.headers().get(reqwest::header::RANGE).is_none());
        assert_eq!(
            request
                .headers()
                .get("statsig-id-list-file-size")
                .and_then(|value| value.to_str().ok()),
            Some("1024")
        );
    }

    #[test]
    fn id_list_file_request_builder_forwards_full_refresh_range_and_size_hint() {
        let builder = IdListFileRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
        );
        let http_client = reqwest::Client::new();
        let request_context = make_id_list_file_request_context(Some(0), Some(1024));
        let request = builder
            .build_request(&http_client, &request_context)
            .build()
            .expect("request should build");

        assert_eq!(
            request
                .headers()
                .get(reqwest::header::RANGE)
                .and_then(|value| value.to_str().ok()),
            Some("bytes=0-")
        );
        assert_eq!(
            request
                .headers()
                .get("statsig-id-list-file-size")
                .and_then(|value| value.to_str().ok()),
            Some("1024")
        );
    }

    #[test]
    fn id_list_file_request_builder_omits_cache_headers_when_absent() {
        let builder = IdListFileRequestBuilder::new(
            "https://api.statsigcdn.com".to_string(),
            Arc::new(HttpDataProviderObserver::new()),
            Arc::new(disabled_cache::DisabledCache::default()),
        );
        let http_client = reqwest::Client::new();
        let request_context = make_id_list_file_request_context(None, None);
        let request = builder
            .build_request(&http_client, &request_context)
            .build()
            .expect("request should build");

        assert!(request.headers().get(reqwest::header::RANGE).is_none());
        assert!(request.headers().get("statsig-id-list-file-size").is_none());
    }
}
