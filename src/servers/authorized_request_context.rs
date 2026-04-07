use parking_lot::RwLock;
use rocket::http::{HeaderMap, Status};
use rocket::request::{self, FromRequest, Outcome, Request};
use std::collections::HashMap;
use std::sync::Arc;

use crate::servers::normalized_path::NormalizedPath;
use crate::servers::sdk_key_normalizer::extract_download_id_list_file_sdk_key;
use crate::utils::compress_encoder::{encoding_priority, CompressionEncoder, ParsedAcceptEncoding};
use crate::utils::request_helper::{does_request_accept_deltas, does_request_supports_proto};

#[derive(Debug)]
pub struct AuthError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloadIdListFileRequestError {
    MissingSdkKey,
    ConflictingSdkKey,
    MissingFileId,
    InvalidRange,
    InvalidIdListFileSize,
}

impl DownloadIdListFileRequestError {
    pub fn body(self) -> &'static str {
        match self {
            DownloadIdListFileRequestError::MissingSdkKey => "Missing statsig-api-key",
            DownloadIdListFileRequestError::ConflictingSdkKey => {
                "Signed query sdk key does not match statsig-api-key header"
            }
            DownloadIdListFileRequestError::MissingFileId => "Missing id list file id",
            DownloadIdListFileRequestError::InvalidRange => {
                "Range must be absent or formatted as bytes=<start>-"
            }
            DownloadIdListFileRequestError::InvalidIdListFileSize => {
                "statsig-id-list-file-size must be a valid unsigned integer"
            }
        }
    }
}

type CacheKey = (
    String,
    NormalizedPath,
    Vec<CompressionEncoder>,
    bool,           /* supports_proto */
    bool,           /* accept_deltas */
    Option<String>, /* file_id (stable single id list file identity) */
);
pub struct AuthorizedRequestContextCache(
    Arc<RwLock<HashMap<CacheKey, Arc<AuthorizedRequestContext>>>>,
);

impl Default for AuthorizedRequestContextCache {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthorizedRequestContextCache {
    pub fn new() -> Self {
        Self(Arc::new(RwLock::new(HashMap::new())))
    }

    pub fn get_or_insert(
        &self,
        sdk_key: String,
        path: NormalizedPath,
        encodings: Vec<CompressionEncoder>,
        supports_proto: bool,
        accept_deltas: bool,
        file_id: Option<String>,
    ) -> Arc<AuthorizedRequestContext> {
        let encodings = canonicalize_encodings(encodings);
        let key = (
            sdk_key.clone(),
            path.clone(),
            encodings.clone(),
            supports_proto,
            accept_deltas,
            file_id.clone(),
        );
        {
            let read_lock = self.0.read();
            if let Some(context) = read_lock.get(&key) {
                return context.clone();
            }
        }

        let mut write_lock = self.0.write();
        write_lock
            .entry(key)
            .or_insert_with(|| {
                Arc::new(
                    AuthorizedRequestContext::new(sdk_key, path, encodings)
                        .with_request_capabilities(supports_proto, accept_deltas)
                        .with_id_list_request(file_id, None, None),
                )
            })
            .clone()
    }
}

pub struct AuthorizedRequestContextWrapper(pub Arc<AuthorizedRequestContext>);

impl AuthorizedRequestContextWrapper {
    pub fn inner(&self) -> Arc<AuthorizedRequestContext> {
        self.0.clone()
    }
}

pub enum DownloadIdListFileRequestGuard {
    Authorized(Arc<AuthorizedRequestContext>),
    Invalid(DownloadIdListFileRequestError),
}

pub(crate) fn parse_id_list_file_id(path: &str) -> Option<String> {
    path.strip_prefix("/v1/download_id_list_file/")
        .and_then(|rest| rest.split('/').next())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn parse_required_id_list_file_id(path: &str) -> Result<String, DownloadIdListFileRequestError> {
    parse_id_list_file_id(path).ok_or(DownloadIdListFileRequestError::MissingFileId)
}

fn parse_strict_id_list_file_size(
    headers: &HeaderMap<'_>,
) -> Result<Option<u64>, DownloadIdListFileRequestError> {
    match headers.get_one("statsig-id-list-file-size") {
        Some(value) => value
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| DownloadIdListFileRequestError::InvalidIdListFileSize),
        None => Ok(None),
    }
}

fn parse_strict_id_list_file_range_start(
    headers: &HeaderMap<'_>,
) -> Result<Option<u64>, DownloadIdListFileRequestError> {
    let Some(value) = headers.get_one("Range") else {
        return Ok(None);
    };

    let range = value
        .trim()
        .strip_prefix("bytes=")
        .ok_or(DownloadIdListFileRequestError::InvalidRange)?;

    if range.is_empty() || range.contains(',') {
        return Err(DownloadIdListFileRequestError::InvalidRange);
    }

    let (start, end) = range
        .split_once('-')
        .ok_or(DownloadIdListFileRequestError::InvalidRange)?;

    if start.is_empty() || !end.is_empty() {
        return Err(DownloadIdListFileRequestError::InvalidRange);
    }

    start
        .parse::<u64>()
        .map(Some)
        .map_err(|_| DownloadIdListFileRequestError::InvalidRange)
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for DownloadIdListFileRequestGuard {
    type Error = DownloadIdListFileRequestError;

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let normalized_path = NormalizedPath::from(request.uri().path().as_str());
        if normalized_path != NormalizedPath::V1DownloadIdListFile {
            return Outcome::Error((Status::InternalServerError, Self::Error::MissingFileId));
        }

        let headers = request.headers();
        let raw_query = request.uri().query().map(|q| q.as_str().to_string());
        let header_sdk_key = headers.get_one("statsig-api-key");
        let signed_sdk_key = raw_query
            .as_deref()
            .and_then(extract_download_id_list_file_sdk_key);
        let sdk_key = match (header_sdk_key, signed_sdk_key) {
            (Some(header_sdk_key), Some(signed_sdk_key)) => {
                if header_sdk_key != signed_sdk_key {
                    return Outcome::Success(Self::Invalid(
                        DownloadIdListFileRequestError::ConflictingSdkKey,
                    ));
                }
                signed_sdk_key
            }
            (Some(header_sdk_key), None) => header_sdk_key.to_string(),
            (None, Some(signed_sdk_key)) => signed_sdk_key,
            (None, None) => {
                return Outcome::Success(Self::Invalid(
                    DownloadIdListFileRequestError::MissingSdkKey,
                ));
            }
        };
        let raw_path = request.uri().path().as_str().to_string();
        let file_id = match parse_required_id_list_file_id(&raw_path) {
            Ok(file_id) => file_id,
            Err(error) => return Outcome::Success(Self::Invalid(error)),
        };
        let range_start = match parse_strict_id_list_file_range_start(headers) {
            Ok(range_start) => range_start,
            Err(error) => return Outcome::Success(Self::Invalid(error)),
        };
        let id_list_size = match parse_strict_id_list_file_size(headers) {
            Ok(id_list_size) => id_list_size,
            Err(error) => return Outcome::Success(Self::Invalid(error)),
        };

        let encodings = ParsedAcceptEncoding::from_header_values(headers.get("Accept-Encoding"))
            .acceptable_encodings();
        let supports_proto = does_request_supports_proto(request);
        let accept_deltas = does_request_accept_deltas(request);

        Outcome::Success(Self::Authorized(Arc::new(
            AuthorizedRequestContext::new(sdk_key, normalized_path, encodings)
                .with_request_capabilities(supports_proto, accept_deltas)
                .with_raw_request(Some(raw_path), raw_query)
                .with_id_list_request(Some(file_id), range_start, id_list_size),
        )))
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for AuthorizedRequestContextWrapper {
    type Error = AuthError;

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let normalized_path = match request.guard::<NormalizedPath>().await {
            Outcome::Success(path) => path,
            Outcome::Error((status, _)) => return Outcome::Error((status, AuthError)),
            Outcome::Forward(status) => return Outcome::Forward(status),
        };

        let cache = request
            .rocket()
            .state::<Arc<AuthorizedRequestContextCache>>()
            .unwrap();
        let headers = request.headers();
        let file_id = (normalized_path == NormalizedPath::V1DownloadIdListFile)
            .then(|| request.uri().path().as_str())
            .and_then(parse_id_list_file_id);
        let encodings = ParsedAcceptEncoding::from_header_values(headers.get("Accept-Encoding"))
            .acceptable_encodings();
        let supports_proto = does_request_supports_proto(request);
        let accept_deltas = does_request_accept_deltas(request)
            || normalized_path == NormalizedPath::V2DownloadConfigSpecsDeltas;

        match headers.get_one("statsig-api-key") {
            Some(sdk_key) => {
                Outcome::Success(AuthorizedRequestContextWrapper(cache.get_or_insert(
                    sdk_key.to_string(),
                    normalized_path,
                    encodings,
                    supports_proto,
                    accept_deltas,
                    file_id,
                )))
            }
            None => Outcome::Error((Status::BadRequest, AuthError)),
        }
    }
}

#[derive(Debug)]
pub struct AuthorizedRequestContext {
    pub sdk_key: String,
    pub path: NormalizedPath,
    pub use_lcut: bool,
    pub supports_proto: bool,
    pub accept_deltas: bool,
    pub encodings: Vec<CompressionEncoder>,
    // Preserved only for endpoints where SFP must forward the exact signed path/query to upstream.
    pub raw_path: Option<String>,
    pub raw_query: Option<String>,
    pub file_id: Option<String>,
    pub range_start: Option<u64>,
    pub id_list_size: Option<u64>,
}

impl AuthorizedRequestContext {
    pub fn new(sdk_key: String, path: NormalizedPath, encodings: Vec<CompressionEncoder>) -> Self {
        let encodings = canonicalize_encodings(encodings);
        let use_lcut = path == NormalizedPath::V1DownloadConfigSpecs
            || path == NormalizedPath::V2DownloadConfigSpecs
            || path == NormalizedPath::V2DownloadConfigSpecsDeltas;

        AuthorizedRequestContext {
            sdk_key,
            path,
            use_lcut,
            supports_proto: false,
            accept_deltas: false,
            encodings,
            raw_path: None,
            raw_query: None,
            file_id: None,
            range_start: None,
            id_list_size: None,
        }
    }

    pub fn with_request_capabilities(mut self, supports_proto: bool, accept_deltas: bool) -> Self {
        self.supports_proto = supports_proto;
        self.accept_deltas = accept_deltas;
        self
    }

    pub fn with_raw_request(mut self, raw_path: Option<String>, raw_query: Option<String>) -> Self {
        self.raw_path = raw_path;
        self.raw_query = raw_query;
        self
    }

    pub fn with_id_list_request(
        mut self,
        file_id: Option<String>,
        range_start: Option<u64>,
        id_list_size: Option<u64>,
    ) -> Self {
        self.file_id = file_id;
        self.range_start = range_start;
        self.id_list_size = id_list_size;
        self
    }
}

fn canonicalize_encodings(mut encodings: Vec<CompressionEncoder>) -> Vec<CompressionEncoder> {
    encodings.sort_by_key(|encoding| encoding_priority(*encoding));
    encodings.dedup();

    encodings.retain(|encoding| {
        matches!(
            encoding,
            CompressionEncoder::StatsigBrotli | CompressionEncoder::Gzip
        )
    });

    if encodings.is_empty() {
        return vec![CompressionEncoder::PlainText];
    }
    encodings
}

impl std::fmt::Display for AuthorizedRequestContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}|{}|{:?}|{}|{}",
            self.sdk_key,
            self.path.as_str(),
            self.encodings,
            self.supports_proto,
            self.accept_deltas,
        )
    }
}

// Keep volatile signed URL path/query data out of identity. Include the required ID-list file size
// so foreground fetch locks/backoff do not suppress a newer file, but leave range_start out because
// ID-list file refreshes fetch/store whole bodies and use range_start only when slicing responses.
impl PartialEq for AuthorizedRequestContext {
    fn eq(&self, other: &Self) -> bool {
        self.sdk_key == other.sdk_key
            && self.path == other.path
            && self.encodings == other.encodings
            && self.supports_proto == other.supports_proto
            && self.accept_deltas == other.accept_deltas
            && self.file_id == other.file_id
            && self.id_list_size == other.id_list_size
    }
}

impl Eq for AuthorizedRequestContext {}

impl std::hash::Hash for AuthorizedRequestContext {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.sdk_key.hash(state);
        self.path.hash(state);
        self.encodings.hash(state);
        self.supports_proto.hash(state);
        self.accept_deltas.hash(state);
        self.file_id.hash(state);
        self.id_list_size.hash(state);
    }
}

#[cfg(test)]
mod tests_cache_behavior {
    use super::*;
    use std::collections::HashSet;

    fn make_rc(raw_path: Option<&str>, raw_query: Option<&str>) -> AuthorizedRequestContext {
        AuthorizedRequestContext::new(
            "sdk-key-test".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
        )
        .with_raw_request(raw_path.map(str::to_string), raw_query.map(str::to_string))
        .with_id_list_request(raw_path.and_then(parse_id_list_file_id), None, None)
    }

    #[test]
    fn raw_query_does_not_affect_equality_and_hashing_behavior() {
        let rc_a = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            Some("sv=2020-10-02&se=2026-01-27T00%3A23%3A42Z&sr=b&sp=r&sig=abc%3D&k=secret-test"),
        );
        let rc_b = make_rc(
            Some("/v1/download_id_list_file/file_123"),
            Some("sv=2020-10-02&se=2026-01-27T00%3A23%3A42Z&sr=b&sp=r&sig=def%3D&k=secret-test"),
        );

        assert_eq!(rc_a, rc_b);

        let mut set = HashSet::new();
        set.insert(rc_a);
        set.insert(rc_b);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn cache_reuses_same_key_for_stable_single_file_identity() {
        let cache = AuthorizedRequestContextCache::new();

        let ctx_1 = cache.get_or_insert(
            "sdk-key-test".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
            false,
            false,
            Some("file_123".to_string()),
        );
        let ctx_1_same = cache.get_or_insert(
            "sdk-key-test".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
            false,
            false,
            Some("file_123".to_string()),
        );
        let ctx_2 = cache.get_or_insert(
            "sdk-key-test".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
            false,
            false,
            Some("file_123".to_string()),
        );

        assert!(Arc::ptr_eq(&ctx_1, &ctx_1_same));
        assert!(Arc::ptr_eq(&ctx_1, &ctx_2));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        parse_required_id_list_file_id, parse_strict_id_list_file_range_start,
        parse_strict_id_list_file_size, AuthorizedRequestContext, AuthorizedRequestContextCache,
        DownloadIdListFileRequestError, DownloadIdListFileRequestGuard,
    };
    use crate::servers::normalized_path::NormalizedPath;
    use crate::utils::compress_encoder::{CompressionEncoder, ParsedAcceptEncoding};
    use rocket::http::uri::fmt::Path;
    use rocket::http::{Header, HeaderMap, Status};
    use rocket::local::blocking::Client;
    use rocket::routes;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::Arc;

    #[rocket::get("/download_id_list_file/<_tail..>")]
    fn download_id_list_file_guard_result(
        _tail: rocket::http::uri::Segments<'_, Path>,
        guard: DownloadIdListFileRequestGuard,
    ) -> (Status, &'static str) {
        match guard {
            DownloadIdListFileRequestGuard::Authorized(_) => (Status::Ok, "authorized"),
            DownloadIdListFileRequestGuard::Invalid(error) => (Status::BadRequest, error.body()),
        }
    }

    #[test]
    fn parse_required_id_list_file_id_requires_non_empty_file_id() {
        assert_eq!(
            parse_required_id_list_file_id("/v1/download_id_list_file/").unwrap_err(),
            DownloadIdListFileRequestError::MissingFileId
        );
    }

    #[test]
    fn parse_strict_id_list_file_size_rejects_invalid_values() {
        let mut headers = HeaderMap::new();
        headers.add(Header::new("statsig-id-list-file-size", "abc"));

        assert_eq!(
            parse_strict_id_list_file_size(&headers).unwrap_err(),
            DownloadIdListFileRequestError::InvalidIdListFileSize
        );
    }

    #[test]
    fn parse_strict_id_list_file_size_accepts_valid_values() {
        let mut headers = HeaderMap::new();
        headers.add(Header::new("statsig-id-list-file-size", "270"));

        assert_eq!(parse_strict_id_list_file_size(&headers).unwrap(), Some(270));
    }

    #[test]
    fn parse_strict_id_list_file_range_start_accepts_start_only_ranges() {
        let mut headers = HeaderMap::new();
        headers.add(Header::new("Range", "bytes=270-"));

        assert_eq!(
            parse_strict_id_list_file_range_start(&headers).unwrap(),
            Some(270)
        );
    }

    #[test]
    fn parse_strict_id_list_file_range_start_rejects_suffix_ranges() {
        let mut headers = HeaderMap::new();
        headers.add(Header::new("Range", "bytes=-10"));

        assert_eq!(
            parse_strict_id_list_file_range_start(&headers).unwrap_err(),
            DownloadIdListFileRequestError::InvalidRange
        );
    }

    #[test]
    fn parse_strict_id_list_file_range_start_rejects_explicit_end_ranges() {
        let mut headers = HeaderMap::new();
        headers.add(Header::new("Range", "bytes=0-10"));

        assert_eq!(
            parse_strict_id_list_file_range_start(&headers).unwrap_err(),
            DownloadIdListFileRequestError::InvalidRange
        );
    }

    #[test]
    fn parse_strict_id_list_file_range_start_rejects_multi_ranges() {
        let mut headers = HeaderMap::new();
        headers.add(Header::new("Range", "bytes=0-,10-"));

        assert_eq!(
            parse_strict_id_list_file_range_start(&headers).unwrap_err(),
            DownloadIdListFileRequestError::InvalidRange
        );
    }

    #[test]
    fn parse_strict_id_list_file_range_start_rejects_invalid_units() {
        let mut headers = HeaderMap::new();
        headers.add(Header::new("Range", "items=270-"));

        assert_eq!(
            parse_strict_id_list_file_range_start(&headers).unwrap_err(),
            DownloadIdListFileRequestError::InvalidRange
        );
    }

    #[test]
    fn parse_strict_id_list_file_range_start_rejects_non_numeric_starts() {
        let mut headers = HeaderMap::new();
        headers.add(Header::new("Range", "bytes=abc-"));

        assert_eq!(
            parse_strict_id_list_file_range_start(&headers).unwrap_err(),
            DownloadIdListFileRequestError::InvalidRange
        );
    }

    #[test]
    fn download_id_list_file_request_guard_reports_invalid_range() {
        let rocket = rocket::build().mount("/v1", routes![download_id_list_file_guard_result]);
        let client = Client::tracked(rocket).expect("client should build");

        let response = client
            .get("/v1/download_id_list_file/file_123")
            .header(Header::new("statsig-api-key", "secret-key"))
            .header(Header::new("Range", "bytes=-10"))
            .dispatch();

        assert_eq!(response.status(), Status::BadRequest);
        assert_eq!(
            response.into_string().as_deref(),
            Some("Range must be absent or formatted as bytes=<start>-")
        );
    }

    #[test]
    fn download_id_list_file_request_guard_uses_header_sdk_key() {
        let rocket = rocket::build().mount("/v1", routes![download_id_list_file_guard_result]);
        let client = Client::tracked(rocket).expect("client should build");

        let response = client
            .get("/v1/download_id_list_file/file_123")
            .header(Header::new("statsig-api-key", "secret-key"))
            .dispatch();

        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_string().as_deref(), Some("authorized"));
    }

    #[test]
    fn download_id_list_file_request_guard_uses_signed_query_sdk_key() {
        let rocket = rocket::build().mount("/v1", routes![download_id_list_file_guard_result]);
        let client = Client::tracked(rocket).expect("client should build");

        let response = client
            .get("/v1/download_id_list_file/file_123?k=secret-key")
            .dispatch();

        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.into_string().as_deref(), Some("authorized"));
    }

    #[test]
    fn download_id_list_file_request_guard_rejects_conflicting_header_and_signed_query_sdk_keys() {
        let rocket = rocket::build().mount("/v1", routes![download_id_list_file_guard_result]);
        let client = Client::tracked(rocket).expect("client should build");

        let response = client
            .get("/v1/download_id_list_file/file_123?k=signed-secret")
            .header(Header::new("statsig-api-key", "header-secret"))
            .dispatch();

        assert_eq!(response.status(), Status::BadRequest);
        assert_eq!(
            response.into_string().as_deref(),
            Some("Signed query sdk key does not match statsig-api-key header")
        );
    }

    #[test]
    fn download_id_list_file_request_guard_requires_sdk_key_when_header_and_query_are_missing() {
        let rocket = rocket::build().mount("/v1", routes![download_id_list_file_guard_result]);
        let client = Client::tracked(rocket).expect("client should build");

        let response = client.get("/v1/download_id_list_file/file_123").dispatch();

        assert_eq!(response.status(), Status::BadRequest);
        assert_eq!(
            response.into_string().as_deref(),
            Some("Missing statsig-api-key")
        );
    }

    #[test]
    fn authorized_request_context_hash_and_eq_ignore_encoding_order() {
        let rc1 = AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![
                CompressionEncoder::Brotli,
                CompressionEncoder::Gzip,
                CompressionEncoder::Brotli,
            ],
        );
        let rc2 = AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip, CompressionEncoder::Brotli],
        );

        assert_eq!(rc1, rc2);

        let mut h1 = DefaultHasher::new();
        rc1.hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        rc2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn cache_get_or_insert_reuses_context_for_permuted_encodings() {
        let cache = AuthorizedRequestContextCache::new();
        let rc1 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Brotli, CompressionEncoder::Gzip],
            false,
            false,
            None,
        );
        let rc2 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip, CompressionEncoder::Brotli],
            false,
            false,
            None,
        );

        assert!(
            Arc::ptr_eq(&rc1, &rc2),
            "cache should reuse same Arc for equivalent encoding sets"
        );
    }

    #[test]
    fn cache_get_or_insert_uses_file_id_only_for_single_file_requests() {
        let cache = AuthorizedRequestContextCache::new();
        let rc1 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
            false,
            false,
            Some("file_123".to_string()),
        );
        let rc2 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
            false,
            false,
            Some("file_123".to_string()),
        );

        assert!(Arc::ptr_eq(&rc1, &rc2));
        assert_eq!(rc1.range_start, None);
        assert_eq!(rc2.range_start, None);
        assert_eq!(rc1.id_list_size, None);
        assert_eq!(rc2.id_list_size, None);
    }

    #[test]
    fn authorized_request_context_equality_uses_required_id_list_size_not_range_start() {
        let rc1 = AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
        )
        .with_raw_request(
            Some("/v1/download_id_list_file/file_123".to_string()),
            Some("k=secret-key-a".to_string()),
        )
        .with_id_list_request(Some("file_123".to_string()), Some(270), Some(512));
        let rc2 = AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
        )
        .with_raw_request(
            Some("/v1/download_id_list_file/file_123".to_string()),
            Some("k=secret-key-b".to_string()),
        )
        .with_id_list_request(Some("file_123".to_string()), Some(1024), Some(512));

        assert_eq!(rc1, rc2);

        let mut h1 = DefaultHasher::new();
        rc1.hash(&mut h1);
        let mut h2 = DefaultHasher::new();
        rc2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());

        let newer_size_context = AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadIdListFile,
            vec![CompressionEncoder::PlainText],
        )
        .with_raw_request(
            Some("/v1/download_id_list_file/file_123".to_string()),
            Some("k=secret-key-c".to_string()),
        )
        .with_id_list_request(Some("file_123".to_string()), Some(270), Some(2048));

        assert_ne!(rc1, newer_size_context);
    }

    #[test]
    fn cache_get_or_insert_reuses_context_for_gzip_plaintext_variants() {
        let cache = AuthorizedRequestContextCache::new();
        let rc1 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip, CompressionEncoder::PlainText],
            false,
            false,
            None,
        );
        let rc2 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Identity, CompressionEncoder::Gzip],
            false,
            false,
            None,
        );

        assert!(
            Arc::ptr_eq(&rc1, &rc2),
            "implicit plain/identity fallbacks should not create a new Arc"
        );
        assert_eq!(rc1.encodings, vec![CompressionEncoder::Gzip]);
    }

    #[test]
    fn cache_get_or_insert_reuses_get_id_lists_context_for_gzip_plaintext_variants() {
        let cache = AuthorizedRequestContextCache::new();
        let rc1 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1GetIdLists,
            vec![CompressionEncoder::Gzip, CompressionEncoder::PlainText],
            false,
            false,
            None,
        );
        let rc2 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1GetIdLists,
            vec![CompressionEncoder::Identity, CompressionEncoder::Gzip],
            false,
            false,
            None,
        );

        assert!(
            Arc::ptr_eq(&rc1, &rc2),
            "gzip-capable get_id_lists requests should share the same cached context"
        );
        assert_eq!(rc1.encodings, vec![CompressionEncoder::Gzip]);
    }

    #[test]
    fn cache_get_or_insert_reuses_context_for_plaintext_and_identity_variants() {
        let cache = AuthorizedRequestContextCache::new();
        let rc1 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::PlainText],
            false,
            false,
            None,
        );
        let rc2 = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Identity],
            false,
            false,
            None,
        );

        assert!(
            Arc::ptr_eq(&rc1, &rc2),
            "plain_text and identity should share the same cache key"
        );
        assert_eq!(rc1.encodings, vec![CompressionEncoder::PlainText]);
    }

    #[test]
    fn cache_get_or_insert_keeps_no_accept_encoding_as_plain_text() {
        let cache = AuthorizedRequestContextCache::new();
        let rc = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![],
            false,
            false,
            None,
        );

        assert_eq!(rc.encodings, vec![CompressionEncoder::PlainText]);
    }

    #[test]
    fn cache_get_or_insert_ignores_unsupported_br_zstd_and_deflate() {
        let cache = AuthorizedRequestContextCache::new();
        let rc = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![
                CompressionEncoder::Brotli,
                CompressionEncoder::Zstd,
                CompressionEncoder::Deflate,
            ],
            false,
            false,
            None,
        );

        assert_eq!(rc.encodings, vec![CompressionEncoder::PlainText]);
    }

    #[test]
    fn cache_get_or_insert_keeps_statsig_brotli_distinct_from_gzip_only() {
        let cache = AuthorizedRequestContextCache::new();
        let gzip_only = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::Gzip],
            false,
            false,
            None,
        );
        let gzip_and_statsig_br = cache.get_or_insert(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::StatsigBrotli, CompressionEncoder::Gzip],
            false,
            false,
            None,
        );

        assert!(
            !Arc::ptr_eq(&gzip_only, &gzip_and_statsig_br),
            "statsig-br capability must remain distinct from gzip-only requests"
        );
        assert_eq!(
            gzip_and_statsig_br.encodings,
            vec![CompressionEncoder::StatsigBrotli, CompressionEncoder::Gzip]
        );
    }

    #[test]
    fn parsed_accept_encoding_preserves_current_request_context_key_behavior() {
        let rc = AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            ParsedAcceptEncoding::from_raw_value("gzip;q=0, statsig-br, identity, foo")
                .acceptable_encodings(),
        );

        assert_eq!(rc.encodings, vec![CompressionEncoder::StatsigBrotli]);
    }
}
