use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use base64::{prelude::BASE64_STANDARD, Engine};
use rocket::http::{ContentType, Status};
use rocket::request::{self, FromRequest, Outcome};
use rocket::response::{Responder, Response};
use rocket::{post, routes, Request, Route, State};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::datastore::caching::redis_pool::RedisPool;
use crate::datastore::idlist_membership::{
    hash_server_key, IdListKeySchema, IdListMembershipConfig, IdListMembershipError,
    RedisIdListMembershipFetcher,
};
use crate::datatypes::gzip_data::LoggedBodyJSON;
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{EventStat, OperationType, ProxyEvent, ProxyEventType};
use crate::servers::authorized_request_context::AuthorizedRequestContext;
use crate::servers::normalized_path::NormalizedPath;
use crate::utils::compress_encoder::{CompressionEncoder, ParsedAcceptEncoding};
use crate::utils::rate_limiter::RateLimiter;
use crate::utils::response_compression::{
    compress_response_payload, content_encoding_header_value, preferred_response_compression,
};

mod company_resolver;
use crate::datastore::idlist_membership::namespaces::IdListNamespaces;
use company_resolver::CompanyMembershipPool;

pub const DEFAULT_MAX_LOOKUPS: usize = 100;
pub const DEFAULT_RATE_LIMIT_RPS_PER_KEY: u64 = 0;
const ID_LISTS_ENRICHMENT: &str = "id_lists";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserEnrichmentConfig {
    pub max_lookups: usize,
    pub rate_limit_rps_per_key: u64,
}

impl Default for UserEnrichmentConfig {
    fn default() -> Self {
        Self {
            max_lookups: DEFAULT_MAX_LOOKUPS,
            rate_limit_rps_per_key: DEFAULT_RATE_LIMIT_RPS_PER_KEY,
        }
    }
}

impl UserEnrichmentConfig {
    pub fn from_env() -> Result<Self, String> {
        let defaults = Self::default();
        let max_lookups = env_usize("USER_ENRICHMENT_MAX_LOOKUPS", defaults.max_lookups)?;
        if max_lookups == 0 {
            return Err("USER_ENRICHMENT_MAX_LOOKUPS must be greater than zero".to_string());
        }

        Ok(Self {
            max_lookups,
            rate_limit_rps_per_key: env_u64(
                "USER_ENRICHMENT_RATE_LIMIT_RPS_PER_KEY",
                defaults.rate_limit_rps_per_key,
            )?,
        })
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .map_err(|_| format!("{name} must be a non-negative integer, got '{value}'")),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(format!("failed reading {name}: {error}")),
    }
}

fn env_usize(name: &str, default: usize) -> Result<usize, String> {
    env_u64(name, default as u64).and_then(|value| {
        usize::try_from(value).map_err(|_| format!("{name} is too large, got '{value}'"))
    })
}

#[derive(Debug, Deserialize)]
pub struct UserEnrichmentRequest {
    #[serde(default)]
    id_list_lookups: Vec<String>,
    #[serde(default)]
    user: Option<EnrichmentUser>,
    enrichments: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct EnrichmentUser {
    #[serde(rename = "userID", alias = "user_id", default)]
    user_id: Option<String>,
    #[serde(rename = "customIDs", alias = "custom_ids", default)]
    custom_ids: HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserEnrichmentResponse {
    pub id_lists: BTreeMap<String, Vec<String>>,
    pub unsupported_enrichments: Vec<String>,
}

#[async_trait]
trait IdListMembershipResolver: Send + Sync {
    async fn resolve(
        &self,
        lookup_ids: Vec<String>,
    ) -> Result<HashMap<String, HashSet<String>>, IdListMembershipError>;
}

#[async_trait]
impl IdListMembershipResolver for RedisIdListMembershipFetcher {
    async fn resolve(
        &self,
        lookup_ids: Vec<String>,
    ) -> Result<HashMap<String, HashSet<String>>, IdListMembershipError> {
        self.fetch(lookup_ids)
            .await
            .map(|resolved| resolved.into_map())
    }
}

struct KeyContext {
    key_hash: String,
    resolver: Arc<dyn IdListMembershipResolver>,
    rate_limiter: Option<Mutex<RateLimiter>>,
}

pub struct UserEnrichmentService {
    keys: HashMap<String, Arc<KeyContext>>,
    config: UserEnrichmentConfig,
}

impl UserEnrichmentService {
    pub async fn from_redis(
        namespaces: Arc<IdListNamespaces>,
        schema: IdListKeySchema,
        pool: RedisPool,
        membership_config: IdListMembershipConfig,
        config: UserEnrichmentConfig,
    ) -> Result<Arc<Self>, IdListMembershipError> {
        let companies =
            CompanyMembershipPool::new(namespaces.clone(), schema, pool, membership_config);
        let mut keys = HashMap::with_capacity(namespaces.server_keys.len());
        for server_key in namespaces.server_keys.iter().cloned() {
            let key_hash = hash_server_key(&server_key);
            let resolver = companies.resolver(server_key.clone());
            keys.insert(
                server_key,
                Arc::new(KeyContext {
                    key_hash,
                    resolver,
                    rate_limiter: rate_limiter(config.rate_limit_rps_per_key),
                }),
            );
        }
        Ok(Arc::new(Self { keys, config }))
    }

    #[cfg(test)]
    fn with_resolvers(
        resolvers: impl IntoIterator<Item = (String, Arc<dyn IdListMembershipResolver>)>,
        config: UserEnrichmentConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            keys: resolvers
                .into_iter()
                .map(|(key, resolver)| {
                    (
                        key.clone(),
                        Arc::new(KeyContext {
                            key_hash: hash_server_key(&key),
                            resolver,
                            rate_limiter: rate_limiter(config.rate_limit_rps_per_key),
                        }),
                    )
                })
                .collect(),
            config,
        })
    }
}

fn rate_limiter(rate: u64) -> Option<Mutex<RateLimiter>> {
    (rate > 0).then(|| Mutex::new(RateLimiter::new(rate as f64)))
}

pub struct UserEnrichmentRequestContext {
    key: Arc<KeyContext>,
    encodings: Vec<CompressionEncoder>,
    started: Instant,
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for UserEnrichmentRequestContext {
    type Error = ();

    async fn from_request(request: &'r Request<'_>) -> request::Outcome<Self, Self::Error> {
        let Some(service) = request.rocket().state::<Arc<UserEnrichmentService>>() else {
            return Outcome::Error((Status::InternalServerError, ()));
        };
        let Some(server_key) = request.headers().get_one("statsig-api-key") else {
            emit_request_metric("missing", Status::Unauthorized, false);
            return Outcome::Error((Status::Unauthorized, ()));
        };
        let Some(key) = service.keys.get(server_key).cloned() else {
            emit_request_metric(&hash_server_key(server_key), Status::Unauthorized, false);
            return Outcome::Error((Status::Unauthorized, ()));
        };

        if let Some(limiter) = &key.rate_limiter {
            if !limiter.lock().await.try_acquire() {
                emit_request_metric(&key.key_hash, Status::TooManyRequests, false);
                return Outcome::Error((Status::TooManyRequests, ()));
            }
        }

        let encodings =
            ParsedAcceptEncoding::from_header_values(request.headers().get("Accept-Encoding"))
                .acceptable_encodings();
        Outcome::Success(Self {
            key,
            encodings,
            started: Instant::now(),
        })
    }
}

pub struct UserEnrichmentHttpResponse {
    status: Status,
    data: Vec<u8>,
    encoding: CompressionEncoder,
}

impl UserEnrichmentHttpResponse {
    fn json<T: Serialize>(status: Status, value: &T, encodings: &[CompressionEncoder]) -> Self {
        let data = serde_json::to_vec(value)
            .unwrap_or_else(|_| br#"{"error":"failed to serialize response"}"#.to_vec());
        let requested = preferred_response_compression(encodings);
        let (data, encoding) = compress_response_payload(&data, requested)
            .map(|data| (data, requested))
            .unwrap_or((data, CompressionEncoder::PlainText));
        Self {
            status,
            data,
            encoding,
        }
    }

    fn error(status: Status, message: &'static str, encodings: &[CompressionEncoder]) -> Self {
        Self::json(status, &serde_json::json!({ "error": message }), encodings)
    }
}

impl<'r> Responder<'r, 'static> for UserEnrichmentHttpResponse {
    fn respond_to(self, _request: &'r Request<'_>) -> Result<Response<'static>, Status> {
        let mut builder = Response::build();
        builder.status(self.status).header(ContentType::JSON);
        if let Some(value) = content_encoding_header_value(self.encoding) {
            builder.raw_header("Content-Encoding", value);
        }
        builder
            .sized_body(self.data.len(), Cursor::new(self.data))
            .ok()
    }
}

#[post("/user_enrichment", data = "<body>")]
pub async fn post_user_enrichment(
    context: UserEnrichmentRequestContext,
    service: &State<Arc<UserEnrichmentService>>,
    body: LoggedBodyJSON<UserEnrichmentRequest>,
) -> UserEnrichmentHttpResponse {
    let request = body.into_inner();
    let (lookups, unsupported, resolve_id_lists) = prepare_request(request);
    emit_lookups_metric(&context.key.key_hash, lookups.len());

    if lookups.len() > service.config.max_lookups {
        let status = Status::PayloadTooLarge;
        emit_completion_metrics(&context, status);
        return UserEnrichmentHttpResponse::error(
            status,
            "too many id list lookups",
            &context.encodings,
        );
    }

    let memberships = if resolve_id_lists && !lookups.is_empty() {
        match context.key.resolver.resolve(lookups).await {
            Ok(memberships) => memberships,
            Err(error) => {
                eprintln!("[SFP] User enrichment membership resolution failed: {error}");
                let status = Status::ServiceUnavailable;
                emit_completion_metrics(&context, status);
                return UserEnrichmentHttpResponse::error(
                    status,
                    "id list resolution unavailable",
                    &context.encodings,
                );
            }
        }
    } else {
        HashMap::new()
    };

    let id_lists = memberships
        .into_iter()
        .map(|(lookup, names)| {
            let mut names: Vec<_> = names.into_iter().collect();
            names.sort();
            (lookup, names)
        })
        .collect();
    let response = UserEnrichmentResponse {
        id_lists,
        unsupported_enrichments: unsupported,
    };
    emit_completion_metrics(&context, Status::Ok);
    UserEnrichmentHttpResponse::json(Status::Ok, &response, &context.encodings)
}

fn prepare_request(request: UserEnrichmentRequest) -> (Vec<String>, Vec<String>, bool) {
    let mut seen = HashSet::new();
    let mut lookups = Vec::new();
    for lookup in request.id_list_lookups {
        if seen.insert(lookup.clone()) {
            lookups.push(lookup);
        }
    }
    if let Some(user) = request.user {
        if let Some(user_id) = user.user_id {
            let lookup = hash_lookup_id(&user_id);
            if seen.insert(lookup.clone()) {
                lookups.push(lookup);
            }
        }
        let mut custom_ids: Vec<_> = user.custom_ids.into_iter().collect();
        custom_ids.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, id) in custom_ids {
            let lookup = hash_lookup_id(&id);
            if seen.insert(lookup.clone()) {
                lookups.push(lookup);
            }
        }
    }

    let mut unsupported_seen = HashSet::new();
    let unsupported = request
        .enrichments
        .as_ref()
        .into_iter()
        .flatten()
        .filter(|enrichment| enrichment.as_str() != ID_LISTS_ENRICHMENT)
        .filter(|enrichment| unsupported_seen.insert((*enrichment).clone()))
        .cloned()
        .collect();
    let resolve_id_lists = request.enrichments.as_ref().is_none_or(|requested| {
        requested
            .iter()
            .any(|enrichment| enrichment == ID_LISTS_ENRICHMENT)
    });
    (lookups, unsupported, resolve_id_lists)
}

pub fn hash_lookup_id(id: &str) -> String {
    BASE64_STANDARD
        .encode(Sha256::digest(id.as_bytes()))
        .chars()
        .take(8)
        .collect()
}

fn metric_context(key_hash: &str) -> Arc<AuthorizedRequestContext> {
    Arc::new(AuthorizedRequestContext::new(
        key_hash.to_string(),
        NormalizedPath::V1UserEnrichment,
        Vec::new(),
    ))
}

fn emit_request_metric(key_hash: &str, status: Status, success: bool) {
    let context = metric_context(key_hash);
    ProxyEventObserver::publish_event(
        ProxyEvent::new_with_rc(
            if success {
                ProxyEventType::UserEnrichmentRequestSuccess
            } else {
                ProxyEventType::UserEnrichmentRequestFailed
            },
            &context,
        )
        .with_status_code(status.code)
        .with_stat(EventStat {
            operation_type: OperationType::IncrByValue,
            value: 1,
        }),
    );
}

fn emit_lookups_metric(key_hash: &str, count: usize) {
    let context = metric_context(key_hash);
    ProxyEventObserver::publish_event(
        ProxyEvent::new_with_rc(ProxyEventType::UserEnrichmentLookupsPerRequest, &context)
            .with_stat(EventStat {
                operation_type: OperationType::Distribution,
                value: count.try_into().unwrap_or(i64::MAX),
            }),
    );
}

fn emit_completion_metrics(context: &UserEnrichmentRequestContext, status: Status) {
    emit_request_metric(&context.key.key_hash, status, status.class().is_success());
    let metric_context = metric_context(&context.key.key_hash);
    ProxyEventObserver::publish_event(
        ProxyEvent::new_with_rc(ProxyEventType::UserEnrichmentLatencyMs, &metric_context)
            .with_stat(EventStat {
                operation_type: OperationType::Timing,
                value: context
                    .started
                    .elapsed()
                    .as_millis()
                    .try_into()
                    .unwrap_or(i64::MAX),
            }),
    );
}

pub fn endpoint_routes(enabled: bool) -> Vec<Route> {
    if enabled {
        routes![post_user_enrichment]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use rocket::http::{ContentType, Header};
    use rocket::local::asynchronous::Client;
    use std::io::Write;

    use super::*;

    struct MockResolver {
        memberships: HashMap<String, HashSet<String>>,
        calls: AtomicUsize,
        fail: bool,
    }

    impl MockResolver {
        fn new(memberships: HashMap<String, HashSet<String>>) -> Arc<Self> {
            Arc::new(Self {
                memberships,
                calls: AtomicUsize::new(0),
                fail: false,
            })
        }

        fn failing() -> Arc<Self> {
            Arc::new(Self {
                memberships: HashMap::new(),
                calls: AtomicUsize::new(0),
                fail: true,
            })
        }
    }

    #[async_trait]
    impl IdListMembershipResolver for MockResolver {
        async fn resolve(
            &self,
            lookup_ids: Vec<String>,
        ) -> Result<HashMap<String, HashSet<String>>, IdListMembershipError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(IdListMembershipError::Timeout(Duration::from_millis(1)));
            }
            Ok(lookup_ids
                .into_iter()
                .map(|lookup| {
                    let names = self.memberships.get(&lookup).cloned().unwrap_or_default();
                    (lookup, names)
                })
                .collect())
        }
    }

    async fn client(
        resolvers: Vec<(String, Arc<dyn IdListMembershipResolver>)>,
        config: UserEnrichmentConfig,
    ) -> Client {
        Client::tracked(
            rocket::build()
                .manage(UserEnrichmentService::with_resolvers(resolvers, config))
                .mount("/v1", endpoint_routes(true)),
        )
        .await
        .expect("client")
    }

    async fn post<'a>(
        client: &'a Client,
        key: Option<&str>,
        body: &str,
    ) -> rocket::local::asynchronous::LocalResponse<'a> {
        let mut request = client
            .post("/v1/user_enrichment")
            .header(ContentType::JSON)
            .body(body);
        if let Some(key) = key {
            request.add_header(Header::new("statsig-api-key", key.to_string()));
        }
        request.dispatch().await
    }

    #[test]
    fn server_side_hashing_matches_shared_parity_fixture() {
        let fixture = [
            ("", "47DEQpj8"),
            ("user-123", "/N7G301E"),
            ("amplitude-id", "N5nv/hPX"),
        ];
        for (id, expected) in fixture {
            assert_eq!(hash_lookup_id(id), expected, "fixture mismatch for {id:?}");
        }
    }

    #[test]
    fn request_union_hashes_user_and_echoes_unknown_enrichments() {
        let (lookups, unsupported, resolve) = prepare_request(UserEnrichmentRequest {
            id_list_lookups: vec!["existing".into(), "existing".into()],
            user: Some(EnrichmentUser {
                user_id: Some("user-123".into()),
                custom_ids: HashMap::from([
                    ("deviceID".into(), "device-1".into()),
                    ("stableID".into(), "user-123".into()),
                ]),
            }),
            enrichments: Some(vec!["future".into(), "id_lists".into(), "future".into()]),
        });
        assert_eq!(
            lookups,
            vec![
                "existing".to_string(),
                hash_lookup_id("user-123"),
                hash_lookup_id("device-1")
            ]
        );
        assert_eq!(unsupported, vec!["future"]);
        assert!(resolve);
    }

    #[tokio::test]
    async fn hashes_only_returns_memberships_and_missing_entries() {
        let resolver = MockResolver::new(HashMap::from([(
            "known".into(),
            HashSet::from(["vip".into(), "employee_ids".into()]),
        )]));
        let client = client(
            vec![("secret".into(), resolver.clone())],
            UserEnrichmentConfig::default(),
        )
        .await;
        let response = post(
            &client,
            Some("secret"),
            r#"{"id_list_lookups":["known","missing"]}"#,
        )
        .await;
        assert_eq!(response.status(), Status::Ok);
        let body: UserEnrichmentResponse =
            serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
        assert_eq!(body.id_lists["known"], vec!["employee_ids", "vip"]);
        assert!(body.id_lists["missing"].is_empty());
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn user_only_request_hashes_all_user_identifiers() {
        let user_hash = hash_lookup_id("user-123");
        let device_hash = hash_lookup_id("device-123");
        let resolver = MockResolver::new(HashMap::from([
            (user_hash.clone(), HashSet::from(["users".into()])),
            (device_hash.clone(), HashSet::from(["devices".into()])),
        ]));
        let client = client(
            vec![("secret".into(), resolver.clone())],
            UserEnrichmentConfig::default(),
        )
        .await;
        let response = post(
            &client,
            Some("secret"),
            r#"{"user":{"userID":"user-123","customIDs":{"deviceID":"device-123"}}}"#,
        )
        .await;
        let body: UserEnrichmentResponse =
            serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
        assert_eq!(body.id_lists[&user_hash], vec!["users"]);
        assert_eq!(body.id_lists[&device_hash], vec!["devices"]);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn empty_or_filtered_requests_do_not_call_resolver() {
        let resolver = MockResolver::new(HashMap::new());
        let client = client(
            vec![("secret".into(), resolver.clone())],
            UserEnrichmentConfig::default(),
        )
        .await;
        let empty = post(&client, Some("secret"), "{}").await;
        assert_eq!(empty.status(), Status::Ok);

        let filtered = post(
            &client,
            Some("secret"),
            r#"{"id_list_lookups":["known"],"enrichments":["segments"]}"#,
        )
        .await;
        assert_eq!(filtered.status(), Status::Ok);
        let body: UserEnrichmentResponse =
            serde_json::from_str(&filtered.into_string().await.unwrap()).unwrap();
        assert!(body.id_lists.is_empty());
        assert_eq!(body.unsupported_enrichments, vec!["segments"]);
        assert_eq!(resolver.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auth_isolated_by_server_key_namespace() {
        let first = MockResolver::new(HashMap::from([(
            "same".into(),
            HashSet::from(["first".into()]),
        )]));
        let second = MockResolver::new(HashMap::from([(
            "same".into(),
            HashSet::from(["second".into()]),
        )]));
        let client = client(
            vec![("key-1".into(), first), ("key-2".into(), second)],
            UserEnrichmentConfig::default(),
        )
        .await;

        for (key, expected) in [("key-1", "first"), ("key-2", "second")] {
            let response = post(&client, Some(key), r#"{"id_list_lookups":["same"]}"#).await;
            let body: UserEnrichmentResponse =
                serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
            assert_eq!(body.id_lists["same"], vec![expected]);
        }
        assert_eq!(
            post(&client, None, "{}").await.status(),
            Status::Unauthorized
        );
        assert_eq!(
            post(&client, Some("unknown"), "{}").await.status(),
            Status::Unauthorized
        );
        assert_eq!(
            post(&client, Some("unknown"), "not-json").await.status(),
            Status::Unauthorized,
            "authentication must reject before parsing the body"
        );
    }

    #[tokio::test]
    async fn lookup_limit_and_rate_limit_are_enforced() {
        let resolver = MockResolver::new(HashMap::new());
        let limited = client(
            vec![("secret".into(), resolver)],
            UserEnrichmentConfig {
                max_lookups: 1,
                rate_limit_rps_per_key: 0,
            },
        )
        .await;
        assert_eq!(
            post(
                &limited,
                Some("secret"),
                r#"{"id_list_lookups":["one","two"]}"#
            )
            .await
            .status(),
            Status::PayloadTooLarge
        );

        let rate_limited = client(
            vec![("secret".into(), MockResolver::new(HashMap::new()))],
            UserEnrichmentConfig {
                max_lookups: 100,
                rate_limit_rps_per_key: 1,
            },
        )
        .await;
        assert_eq!(
            post(&rate_limited, Some("secret"), "{}").await.status(),
            Status::Ok
        );
        assert_eq!(
            post(&rate_limited, Some("secret"), "{}").await.status(),
            Status::TooManyRequests
        );
    }

    #[tokio::test]
    async fn resolver_failure_returns_503() {
        let client = client(
            vec![("secret".into(), MockResolver::failing())],
            UserEnrichmentConfig::default(),
        )
        .await;
        assert_eq!(
            post(&client, Some("secret"), r#"{"id_list_lookups":["known"]}"#)
                .await
                .status(),
            Status::ServiceUnavailable
        );
    }

    #[tokio::test]
    async fn gzip_request_and_response_round_trip() {
        let client = client(
            vec![("secret".into(), MockResolver::new(HashMap::new()))],
            UserEnrichmentConfig::default(),
        )
        .await;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(br#"{"id_list_lookups":["known"]}"#)
            .unwrap();
        let request_body = encoder.finish().unwrap();
        let response = client
            .post("/v1/user_enrichment")
            .header(ContentType::JSON)
            .header(Header::new("statsig-api-key", "secret"))
            .header(Header::new("Content-Encoding", "gzip"))
            .header(Header::new("Accept-Encoding", "gzip"))
            .body(request_body)
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.headers().get_one("Content-Encoding"), Some("gzip"));
        let bytes = response.into_bytes().await.unwrap();
        let mut decoded = String::new();
        GzDecoder::new(bytes.as_slice())
            .read_to_string(&mut decoded)
            .unwrap();
        let body: UserEnrichmentResponse = serde_json::from_str(&decoded).unwrap();
        assert!(body.id_lists["known"].is_empty());
    }

    #[test]
    fn route_is_absent_when_disabled() {
        assert!(endpoint_routes(false).is_empty());
        assert_eq!(endpoint_routes(true).len(), 1);
    }

    /// HTTP with Redis-backed resolvers. Skips without `REDIS_ENTERPRISE_*`.
    mod redis_integration {
        use bb8_redis::redis::AsyncCommands;
        use uuid::Uuid;

        use crate::datastore::caching::redis_pool::{
            create_redis_pool, create_redis_pool_from_env, RedisEnvConfig, RedisPool,
        };
        use crate::datastore::idlist_membership::{hash_company_id, DictClient};

        use super::*;

        async fn pool_or_skip() -> Option<RedisPool> {
            create_redis_pool_from_env(5).await.ok()
        }

        fn membership_config(fetch_timeout: Duration) -> IdListMembershipConfig {
            IdListMembershipConfig {
                fetch_timeout,
                dict_refresh_interval: Duration::from_secs(60 * 60),
                ..IdListMembershipConfig::default()
            }
        }

        async fn seed_membership(
            pool: &RedisPool,
            schema: &IdListKeySchema,
            server_key: &str,
            lookup: &str,
            lists: &[&str],
        ) {
            let skh = hash_company_id(server_key);
            let dict = DictClient::new(pool.clone(), schema.clone());
            let mut tokens = Vec::with_capacity(lists.len());
            for list in lists {
                tokens.push(dict.get_or_allocate_token(&skh, list).await.unwrap());
            }
            let count = dict.snapshot(&skh).await.unwrap().token_to_name.len();
            let mut connection = pool.get().await.unwrap();
            let _: () = connection.set(schema.ready_key(&skh), count).await.unwrap();
            let _: usize = connection
                .sadd(schema.member_key(&skh, lookup), tokens)
                .await
                .unwrap();
        }

        async fn rocket_from_redis(
            pool: RedisPool,
            schema: IdListKeySchema,
            server_keys: Vec<String>,
            fetch_timeout: Duration,
        ) -> Client {
            let mut resolvers: Vec<(String, Arc<dyn IdListMembershipResolver>)> = Vec::new();
            for key in server_keys {
                let resolver = RedisIdListMembershipFetcher::new(
                    schema.clone(),
                    hash_company_id(&key),
                    pool.clone(),
                    membership_config(fetch_timeout),
                )
                .await
                .unwrap();
                resolvers.push((key, resolver));
            }
            Client::tracked(
                rocket::build()
                    .manage(UserEnrichmentService::with_resolvers(
                        resolvers,
                        UserEnrichmentConfig::default(),
                    ))
                    .mount("/v1", endpoint_routes(true)),
            )
            .await
            .expect("client")
        }

        #[tokio::test]
        async fn hashes_only_reads_memberships_and_missing_entries() {
            let Some(pool) = pool_or_skip().await else {
                eprintln!("Skipping — Redis unavailable");
                return;
            };
            let schema = IdListKeySchema::new(format!("ue-{}", Uuid::new_v4()));
            let key = "secret";
            seed_membership(&pool, &schema, key, "known", &["vip", "employee_ids"]).await;
            let client =
                rocket_from_redis(pool, schema, vec![key.into()], Duration::from_millis(500)).await;
            let response = post(
                &client,
                Some(key),
                r#"{"id_list_lookups":["known","missing","known"]}"#,
            )
            .await;
            assert_eq!(response.status(), Status::Ok);
            let body: UserEnrichmentResponse =
                serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
            assert_eq!(body.id_lists["known"], vec!["employee_ids", "vip"]);
            assert!(body.id_lists["missing"].is_empty());
        }

        #[tokio::test]
        async fn user_and_lookup_union_hashes_and_isolates_namespaces() {
            let Some(pool) = pool_or_skip().await else {
                eprintln!("Skipping — Redis unavailable");
                return;
            };
            let schema = IdListKeySchema::new(format!("ue-{}", Uuid::new_v4()));
            let first = "key-1";
            let second = "key-2";
            let user_hash = hash_lookup_id("user-123");
            let device_hash = hash_lookup_id("device-123");
            seed_membership(&pool, &schema, first, "prehashed", &["from_lookup"]).await;
            seed_membership(&pool, &schema, first, &user_hash, &["from_user"]).await;
            seed_membership(&pool, &schema, first, &device_hash, &["from_device"]).await;
            seed_membership(&pool, &schema, first, "same", &["first"]).await;
            seed_membership(&pool, &schema, second, "same", &["second"]).await;

            let client = rocket_from_redis(
                pool,
                schema,
                vec![first.into(), second.into()],
                Duration::from_millis(500),
            )
            .await;

            let union = post(
                &client,
                Some(first),
                r#"{"id_list_lookups":["prehashed"],"user":{"userID":"user-123","customIDs":{"deviceID":"device-123"}}}"#,
            )
            .await;
            let body: UserEnrichmentResponse =
                serde_json::from_str(&union.into_string().await.unwrap()).unwrap();
            assert_eq!(body.id_lists["prehashed"], vec!["from_lookup"]);
            assert_eq!(body.id_lists[&user_hash], vec!["from_user"]);
            assert_eq!(body.id_lists[&device_hash], vec!["from_device"]);
            assert_eq!(user_hash, "/N7G301E");

            for (key, expected) in [(first, "first"), (second, "second")] {
                let response = post(&client, Some(key), r#"{"id_list_lookups":["same"]}"#).await;
                let isolated: UserEnrichmentResponse =
                    serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
                assert_eq!(isolated.id_lists["same"], vec![expected]);
            }
            assert_eq!(
                post(&client, None, "{}").await.status(),
                Status::Unauthorized
            );
            assert_eq!(
                post(&client, Some("unknown"), r#"{"id_list_lookups":["same"]}"#)
                    .await
                    .status(),
                Status::Unauthorized
            );
        }

        #[tokio::test]
        async fn initial_sync_and_missing_dictionary_return_200_with_available_data() {
            let Some(pool) = pool_or_skip().await else {
                return;
            };
            let schema = IdListKeySchema::new(format!("ue-{}", Uuid::new_v4()));
            let key = "secret";
            let skh = hash_company_id(key);
            let body = r#"{"id_list_lookups":["known","missing"]}"#;
            let client = rocket_from_redis(
                pool.clone(),
                schema.clone(),
                vec![key.into()],
                Duration::from_millis(500),
            )
            .await;
            let response = post(&client, Some(key), body).await;
            assert_eq!(response.status(), Status::Ok);
            let result: UserEnrichmentResponse =
                serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
            assert!(result.id_lists.values().all(Vec::is_empty));
            seed_membership(&pool, &schema, key, "known", &["vip"]).await;
            for missing_dictionary in [false, true] {
                let mut conn = pool.get().await.unwrap();
                let _: usize = conn.del(schema.ready_key(&skh)).await.unwrap();
                if missing_dictionary {
                    let _: usize = conn.del(schema.dict_key(&skh)).await.unwrap();
                }
                let client = rocket_from_redis(
                    pool.clone(),
                    schema.clone(),
                    vec![key.into()],
                    Duration::from_millis(500),
                )
                .await;
                let response = post(&client, Some(key), body).await;
                assert_eq!(response.status(), Status::Ok);
                let result: UserEnrichmentResponse =
                    serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
                assert_eq!(
                    result.id_lists["known"],
                    if missing_dictionary {
                        vec![]
                    } else {
                        vec!["vip"]
                    }
                );
                assert!(result.id_lists["missing"].is_empty());
            }
        }

        #[tokio::test]
        async fn redis_timeout_returns_503_without_fabricating_memberships() {
            let Ok(mut redis_config) = RedisEnvConfig::from_env() else {
                eprintln!("Skipping — Redis config unavailable");
                return;
            };
            redis_config.redis_connection_pool_max_size = Some(1);
            redis_config.redis_connection_pool_min_size = Some(1);
            let Ok(pool) = create_redis_pool(&redis_config, 5).await else {
                eprintln!("Skipping — Redis unavailable");
                return;
            };
            let schema = IdListKeySchema::new(format!("ue-{}", Uuid::new_v4()));
            let key = "secret";
            seed_membership(&pool, &schema, key, "known", &["vip"]).await;
            let client = rocket_from_redis(
                pool.clone(),
                schema,
                vec![key.into()],
                Duration::from_millis(10),
            )
            .await;
            let _held = pool.get().await.unwrap();
            assert_eq!(
                post(&client, Some(key), r#"{"id_list_lookups":["known"]}"#)
                    .await
                    .status(),
                Status::ServiceUnavailable
            );
        }
    }
}
