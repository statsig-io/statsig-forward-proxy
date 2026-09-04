use base64::{prelude::BASE64_STANDARD, Engine};
use bb8_redis::redis::{AsyncCommands, ConnectionAddr};
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    fmt,
    io::{Cursor, Read, Write},
    sync::Arc,
    time::Duration,
};

use crate::{
    datastore::{
        caching::redis_auth::{
            shared_entra_provider, EntraOauthConfig, RedisCredentials,
            StatsigRedisConnectionManager, DEFAULT_ENTRA_AUTHORITY_HOST, MIN_REFRESH_MARGIN,
        },
        config_spec_store::ConfigSpecForCompany,
        data_providers::{
            http_data_provider::ResponsePayload, DataProviderRequestResult, FullRequestContext,
            ResponseContext,
        },
    },
    observers::{
        proxy_event_observer::ProxyEventObserver, HttpDataProviderObserverTrait, ProxyEvent,
        ProxyEventType,
    },
    servers::authorized_request_context::AuthorizedRequestContext,
    utils::compress_encoder::CompressionEncoder,
};

use crate::observers::EventStat;
use crate::observers::OperationType;

use bb8_redis::redis::RedisError;
use bytes::Bytes;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub struct RedisCache {
    connection: Option<bb8::Pool<StatsigRedisConnectionManager>>,
    hash_cache: Arc<RwLock<HashMap<String, String>>>,
    uuid: String,
    leader_key_ttl: i64,
    check_lcut: bool,
    redis_cache_ttl_in_s: i64,
    double_write_cache_for_legacy_key: bool,
    /// Optional namespace prefix for all Redis keys (REDIS_KEY_PREFIX), e.g. "sfp-np:".
    key_prefix: String,
}

#[derive(Deserialize, Clone)]
pub struct RedisEnvConfig {
    pub redis_enterprise_user: Option<String>,
    pub redis_enterprise_password: Option<String>,
    pub redis_enterprise_host: String,
    pub redis_enterprise_port: String,
    pub redis_connection_pool_max_size: Option<u32>,
    pub redis_connection_pool_min_size: Option<u32>,
    pub redis_memorystore: Option<bool>,
    pub redis_tls: Option<bool>,
    pub redis_key_prefix: Option<String>,
    /// `password` (default) or `oauth`. See [`parse_auth_mode`].
    pub redis_auth_mode: Option<String>,
    pub redis_oauth_tenant_id: Option<String>,
    pub redis_oauth_client_id: Option<String>,
    pub redis_oauth_client_secret: Option<String>,
    pub redis_oauth_scope: Option<String>,
    pub redis_oauth_username: Option<String>,
    /// Deserialized as a string, not a `u64`, and parsed only in the OAuth branch. `envy` runs
    /// over the whole struct before `REDIS_AUTH_MODE` has been read, so a typed field here would
    /// let a stale or malformed `REDIS_OAUTH_*` value panic a password-mode deployment that is
    /// documented to ignore these entirely. See [`oauth_seconds_from_env`].
    pub redis_oauth_refresh_margin_in_s: Option<String>,
    pub redis_oauth_authority_host: Option<String>,
    /// Opt out of the TLS requirement that OAuth mode otherwise enforces. A string for the same
    /// reason as `redis_oauth_refresh_margin_in_s`. See [`oauth_transport_is_plaintext`].
    pub redis_oauth_allow_plaintext: Option<String>,
}

impl fmt::Debug for RedisEnvConfig {
    /// Hand-written so the two secrets in this struct cannot leak into logs.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedisEnvConfig")
            .field("redis_enterprise_user", &self.redis_enterprise_user)
            .field(
                "redis_enterprise_password",
                &self
                    .redis_enterprise_password
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field("redis_enterprise_host", &self.redis_enterprise_host)
            .field("redis_enterprise_port", &self.redis_enterprise_port)
            .field(
                "redis_connection_pool_max_size",
                &self.redis_connection_pool_max_size,
            )
            .field(
                "redis_connection_pool_min_size",
                &self.redis_connection_pool_min_size,
            )
            .field("redis_memorystore", &self.redis_memorystore)
            .field("redis_tls", &self.redis_tls)
            .field("redis_key_prefix", &self.redis_key_prefix)
            .field("redis_auth_mode", &self.redis_auth_mode)
            .field("redis_oauth_tenant_id", &self.redis_oauth_tenant_id)
            .field("redis_oauth_client_id", &self.redis_oauth_client_id)
            .field(
                "redis_oauth_client_secret",
                &self
                    .redis_oauth_client_secret
                    .as_ref()
                    .map(|_| "<redacted>"),
            )
            .field("redis_oauth_scope", &self.redis_oauth_scope)
            .field("redis_oauth_username", &self.redis_oauth_username)
            .field(
                "redis_oauth_refresh_margin_in_s",
                &self.redis_oauth_refresh_margin_in_s,
            )
            .field(
                "redis_oauth_authority_host",
                &self.redis_oauth_authority_host,
            )
            .field(
                "redis_oauth_allow_plaintext",
                &self.redis_oauth_allow_plaintext,
            )
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedisAuthMode {
    Password,
    Oauth,
}

const REDIS_LEADER_KEY: &str = "statsig_forward_proxy::leader";

/// Parses `REDIS_AUTH_MODE`. Unset or empty means `password`, so existing deployments keep the
/// static-credential path with no Entra code running at all.
fn parse_auth_mode(raw: Option<&str>) -> Result<RedisAuthMode, String> {
    match raw.map(|mode| mode.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("password") => Ok(RedisAuthMode::Password),
        Some("oauth") => Ok(RedisAuthMode::Oauth),
        Some(other) => Err(format!(
            "Unsupported REDIS_AUTH_MODE '{other}'. Expected 'password' or 'oauth'."
        )),
    }
}

/// bb8 replenishes dropped connections in the background only up to `min_idle`
/// (`Internals::wanted()` computes `min_idle - available`), not up to the number retired. If
/// `min_idle` sits far below the working set, token rotation pushes reconnects back onto the
/// request path, which is exactly what OAuth mode is designed to avoid.
fn min_idle_is_too_low_for_oauth(min_idle: u32, max_size: u32) -> bool {
    min_idle * 2 < max_size
}

fn warn_if_min_idle_too_low_for_oauth(min_idle: u32, max_size: u32) {
    if min_idle_is_too_low_for_oauth(min_idle, max_size) {
        eprintln!(
            "REDIS_AUTH_MODE=oauth with REDIS_CONNECTION_POOL_MIN_SIZE={min_idle} and \
             REDIS_CONNECTION_POOL_MAX_SIZE={max_size}. Connections retired on token rotation are \
             replenished in the background only up to the min size, so consider raising \
             REDIS_CONNECTION_POOL_MIN_SIZE closer to your working set to keep reconnects off the \
             request path."
        );
    }
}

/// Whether OAuth mode would send tokens over an unencrypted connection.
///
/// In OAuth mode the access token *is* the Redis password, so an unencrypted connection puts a
/// live Entra credential on the wire in cleartext on every handshake. That is a step worse than
/// the static-password mode this sits alongside: the token is a bearer credential for the scope
/// it was issued against, not just this cache.
fn oauth_transport_is_plaintext(config: &RedisEnvConfig) -> bool {
    !config.redis_tls.unwrap_or(false)
}

fn warn_if_oauth_transport_is_plaintext(config: &RedisEnvConfig) {
    if oauth_transport_is_plaintext(config) {
        eprintln!(
            "REDIS_AUTH_MODE=oauth with REDIS_OAUTH_ALLOW_PLAINTEXT=true and REDIS_TLS unset or \
             false. Entra access tokens will be sent to Redis in cleartext. This is only safe if \
             something else encrypts the hop, such as a TLS-terminating sidecar reached over \
             loopback."
        );
    }
}

/// Collects the `REDIS_OAUTH_*` settings, reporting every missing required variable at once so a
/// misconfiguration takes one restart to diagnose rather than four. Also enforces the settings
/// outside the `REDIS_OAUTH_*` namespace that OAuth mode constrains, namely transport encryption.
fn entra_config_from_env(config: &RedisEnvConfig) -> Result<EntraOauthConfig, String> {
    let required = [
        ("REDIS_OAUTH_TENANT_ID", &config.redis_oauth_tenant_id),
        ("REDIS_OAUTH_CLIENT_ID", &config.redis_oauth_client_id),
        (
            "REDIS_OAUTH_CLIENT_SECRET",
            &config.redis_oauth_client_secret,
        ),
        ("REDIS_OAUTH_SCOPE", &config.redis_oauth_scope),
    ];
    let missing: Vec<&str> = required
        .iter()
        .filter(|(_, value)| value.as_deref().is_none_or(|v| v.trim().is_empty()))
        .map(|(name, _)| *name)
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "REDIS_AUTH_MODE=oauth requires {}",
            missing.join(", ")
        ));
    }

    // Azure requires TLS for Entra-authenticated Redis, so this is not a restriction on any
    // supported deployment. The escape hatch exists for a sidecar that terminates TLS itself,
    // where the proxy's own hop really is loopback, and it has to be deliberate rather than the
    // consequence of forgetting to set REDIS_TLS.
    let allow_plaintext = oauth_flag_from_env(
        "REDIS_OAUTH_ALLOW_PLAINTEXT",
        &config.redis_oauth_allow_plaintext,
    )?
    .unwrap_or(false);
    if oauth_transport_is_plaintext(config) && !allow_plaintext {
        return Err(
            "REDIS_AUTH_MODE=oauth requires REDIS_TLS=true. The Entra access token is sent to \
             Redis as the AUTH password, so without TLS it crosses the network in cleartext on \
             every handshake. If another layer already encrypts the connection, such as a \
             TLS-terminating sidecar reached over loopback, set REDIS_OAUTH_ALLOW_PLAINTEXT=true \
             to acknowledge that."
                .to_string(),
        );
    }

    let authority_host = config
        .redis_oauth_authority_host
        .as_deref()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .unwrap_or(DEFAULT_ENTRA_AUTHORITY_HOST);
    if !is_secure_authority_host(authority_host) {
        return Err(format!(
            "REDIS_OAUTH_AUTHORITY_HOST must be an https:// URL, got '{authority_host}'. The \
             client secret and the returned access token are sent to this host."
        ));
    }

    let refresh_margin = oauth_seconds_from_env(
        "REDIS_OAUTH_REFRESH_MARGIN_IN_S",
        &config.redis_oauth_refresh_margin_in_s,
    )?
    .map(Duration::from_secs);
    if let Some(margin) = refresh_margin {
        if margin < MIN_REFRESH_MARGIN {
            return Err(format!(
                "REDIS_OAUTH_REFRESH_MARGIN_IN_S must be at least {}, got {}. A smaller margin \
                 mints the next token only once connections are already being refused for being \
                 too close to expiry.",
                MIN_REFRESH_MARGIN.as_secs(),
                margin.as_secs()
            ));
        }
    }

    Ok(EntraOauthConfig {
        // Trimmed because a value sourced from a file-backed Kubernetes secret routinely carries
        // a trailing newline, which would otherwise corrupt the token URL and the credentials.
        tenant_id: trimmed_or_default(&config.redis_oauth_tenant_id),
        client_id: trimmed_or_default(&config.redis_oauth_client_id),
        client_secret: trimmed_or_default(&config.redis_oauth_client_secret),
        scope: trimmed_or_default(&config.redis_oauth_scope),
        username: config
            .redis_oauth_username
            .as_deref()
            .map(str::trim)
            .filter(|username| !username.is_empty())
            .map(str::to_string),
        refresh_margin,
        authority_host: authority_host.to_string(),
    })
}

fn trimmed_or_default(value: &Option<String>) -> String {
    value.as_deref().unwrap_or_default().trim().to_string()
}

fn trimmed_non_empty(value: &Option<String>) -> Option<&str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Parses one of the OAuth-only settings that is not naturally a string.
///
/// These are held as strings on [`RedisEnvConfig`] and parsed here, in the OAuth branch, so that
/// a malformed value cannot take down a password-mode deployment. Parsing them by hand also buys
/// an error that names the variable and shows the offending value, which `envy` does not.
fn oauth_seconds_from_env(name: &str, raw: &Option<String>) -> Result<Option<u64>, String> {
    let Some(value) = trimmed_non_empty(raw) else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| format!("{name} must be a whole number of seconds, got '{value}'"))
}

/// Accepts the spellings an operator is likely to reach for, matching how [`parse_auth_mode`]
/// treats `REDIS_AUTH_MODE`.
fn oauth_flag_from_env(name: &str, raw: &Option<String>) -> Result<Option<bool>, String> {
    let Some(value) = trimmed_non_empty(raw) else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => Ok(Some(true)),
        "false" | "0" | "no" => Ok(Some(false)),
        _ => Err(format!("{name} must be true or false, got '{value}'")),
    }
}

/// The authority is the host we hand the client secret to and receive the access token from, so
/// an override must be TLS-protected. Every sovereign-cloud Entra endpoint is https, so a
/// plaintext value is always a misconfiguration rather than a legitimate deployment.
///
/// Enforced here at the environment boundary rather than inside [`EntraTokenProvider`], so tests
/// can still construct an [`EntraOauthConfig`] pointing at a local mock endpoint over plain HTTP.
fn is_secure_authority_host(host: &str) -> bool {
    host.trim().to_ascii_lowercase().starts_with("https://")
}

/// Resolves the `(username, password)` pair handed to redis-rs `ConnectionInfo`.
///
/// redis-rs only issues `AUTH` when `password.is_some()`, so a username-only ACL user with an
/// empty password (configured as `None`) would otherwise have AUTH skipped entirely
/// (https://github.com/redis-rs/redis-rs/issues/1713). To support Redis Enterprise ACLs that
/// authenticate with an empty password, we force `Some("")` when a user is set but no password is
/// configured, which makes redis-rs send `AUTH <user> ""`.
fn resolve_redis_auth(
    user: &str,
    configured_password: Option<&str>,
) -> (Option<String>, Option<String>) {
    let username = if user.is_empty() {
        None
    } else {
        Some(user.to_string())
    };
    let password = match (configured_password, user.is_empty()) {
        (Some(password), _) => Some(password.to_string()),
        (None, false) => Some(String::new()),
        (None, true) => None,
    };
    (username, password)
}

/// Prepends `prefix` to `key`, normalizing to a single colon separator. An empty (or
/// whitespace-only) prefix returns the key unchanged (no extra allocation) so behavior matches
/// the unprefixed default.
fn apply_key_prefix(prefix: &str, key: String) -> String {
    let prefix = prefix.trim();
    if prefix.is_empty() {
        key
    } else if prefix.ends_with(':') {
        format!("{prefix}{key}")
    } else {
        format!("{prefix}:{key}")
    }
}

use async_trait::async_trait;
#[async_trait]
impl HttpDataProviderObserverTrait for RedisCache {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        request_context: &Arc<FullRequestContext>,
        response_context: &Arc<ResponseContext>,
    ) {
        self.update_impl(
            self.get_redis_key(
                &request_context.authorized_request_context,
                Some(&response_context.body),
                false,
            )
            .await,
            &response_context.result_type,
            &request_context.authorized_request_context,
            response_context.lcut,
            &response_context.body,
        )
        .await;

        if self.double_write_cache_for_legacy_key {
            self.update_impl(
                self.get_redis_key(
                    &request_context.authorized_request_context,
                    Some(&response_context.body),
                    true,
                )
                .await,
                &response_context.result_type,
                &request_context.authorized_request_context,
                response_context.lcut,
                &response_context.body,
            )
            .await;
        }
    }

    async fn get(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        let connection = self.connection.as_ref()?.get().await;
        let redis_key = self.get_redis_key(request_context, None, false).await;
        match connection {
            Ok(mut conn) => {
                let mut pipe = redis::pipe();
                pipe.atomic();
                let res: Result<(Option<u64>, Vec<u8>), RedisError> = pipe
                    .hget(&redis_key, "lcut")
                    .hget(&redis_key, "config")
                    .query_async::<(Option<u64>, Vec<u8>)>(&mut *conn)
                    .await;
                match res {
                    Ok((lcut, data)) => {
                        if data.is_empty() {
                            ProxyEventObserver::publish_event(
                                ProxyEvent::new_with_rc(
                                    ProxyEventType::RedisCacheReadMiss,
                                    request_context,
                                )
                                .with_stat(EventStat {
                                    operation_type: OperationType::IncrByValue,
                                    value: 1,
                                }),
                            );
                            None
                        } else {
                            ProxyEventObserver::publish_event(
                                ProxyEvent::new_with_rc(
                                    ProxyEventType::RedisCacheReadSucceed,
                                    request_context,
                                )
                                .with_lcut(lcut.unwrap_or(0))
                                .with_stat(EventStat {
                                    operation_type: OperationType::IncrByValue,
                                    value: 1,
                                }),
                            );
                            // TODO: Rethink the decision here
                            match request_context
                                .encodings
                                .contains(&CompressionEncoder::Gzip)
                            {
                                true => {
                                    let mut compressed = Vec::new();
                                    let mut encoder =
                                        GzEncoder::new(&mut compressed, Compression::best());
                                    if let Err(e) = encoder.write_all(&data) {
                                        eprintln!("Failed to gzip data from redis: {e:?}");
                                        return None;
                                    }
                                    if let Err(e) = encoder.finish() {
                                        eprintln!("Failed to gzip data from redis: {e:?}");
                                        return None;
                                    }
                                    if compressed.is_empty() {
                                        eprintln!("Compressed data from redis is empty.");
                                        return None;
                                    }
                                    Some(Arc::new(ConfigSpecForCompany {
                                        config: Arc::new(ResponsePayload {
                                            encoding: Arc::new(CompressionEncoder::Gzip),
                                            data: Arc::from(Bytes::from(compressed)),
                                            use_proto: false,
                                        }),
                                        lcut: lcut.unwrap_or(0),
                                    }))
                                }
                                false => Some(Arc::new(ConfigSpecForCompany {
                                    config: Arc::new(ResponsePayload {
                                        encoding: Arc::new(CompressionEncoder::PlainText),
                                        data: Arc::from(Bytes::from(data)),
                                        use_proto: false,
                                    }),
                                    lcut: lcut.unwrap_or(0),
                                })),
                            }
                        }
                    }
                    Err(e) => {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new_with_rc(
                                ProxyEventType::RedisCacheReadFailed,
                                request_context,
                            )
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                        );
                        eprintln!("Failed to get key from redis: {e:?}");
                        None
                    }
                }
            }
            Err(e) => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new_with_rc(ProxyEventType::RedisCacheReadFailed, request_context)
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                );
                eprintln!("Failed to get connection to redis: {e:?}");
                None
            }
        }
    }
}

impl RedisCache {
    pub async fn new(
        leader_key_ttl: i64,
        uuid: &str,
        check_lcut: bool,
        redis_cache_ttl_in_s: i64,
        double_write_cache_for_legacy_key: bool,
        redis_connection_timeout_in_s: u64,
    ) -> Self {
        let config = envy::from_env::<RedisEnvConfig>().expect("Malformed config");
        let auth_mode =
            parse_auth_mode(config.redis_auth_mode.as_deref()).expect("Malformed config");

        let redis_port: u16 = config
            .redis_enterprise_port
            .parse()
            .expect("Invalid REDIS_ENTERPRISE_PORT");
        let redis_host = config.redis_enterprise_host.clone();
        let redis_addr = if config.redis_tls.is_some_and(|x| x) {
            ConnectionAddr::TcpTls {
                host: redis_host,
                port: redis_port,
                insecure: false,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(redis_host, redis_port)
        };

        let max_size = config.redis_connection_pool_max_size.unwrap_or(10);
        let min_idle = config.redis_connection_pool_min_size.unwrap_or(1);

        // `None` means we could not obtain credentials and must run without a datastore, which is
        // the same posture as a failed pool build.
        let credentials = match auth_mode {
            RedisAuthMode::Password => {
                let user = config.redis_enterprise_user.clone().unwrap_or_default();
                let (username, password) =
                    resolve_redis_auth(&user, config.redis_enterprise_password.as_deref());
                Some(RedisCredentials::Static { username, password })
            }
            RedisAuthMode::Oauth => {
                let oauth_config = entra_config_from_env(&config).expect("Malformed config");
                warn_if_oauth_transport_is_plaintext(&config);
                warn_if_min_idle_too_low_for_oauth(min_idle, max_size);
                // Reuses the connection-timeout budget so getting a datastore up is bounded the
                // same way whether the obstacle is Entra or Redis itself.
                let startup_timeout = Duration::from_secs(redis_connection_timeout_in_s);
                match shared_entra_provider(oauth_config, startup_timeout).await {
                    Ok(provider) => Some(RedisCredentials::Oauth(provider)),
                    Err(e) => {
                        eprintln!(
                            "Failed to acquire an Entra token for redis on startup. Will continue to run without DataStore. Error: {e}"
                        );
                        None
                    }
                }
            }
        };

        let redis_pool = match credentials {
            Some(credentials) => {
                let manager = StatsigRedisConnectionManager::new(redis_addr, credentials.clone());
                let mut builder = bb8::Pool::builder()
                    .connection_timeout(Duration::from_secs(redis_connection_timeout_in_s))
                    .retry_connection(true)
                    .max_size(max_size)
                    .min_idle(min_idle);
                // Only override bb8's default lifetime in OAuth mode, where the cap keeps
                // connections that sit idle across a token rotation from going stale.
                if let RedisCredentials::Oauth(provider) = &credentials {
                    builder = builder.max_lifetime(provider.max_connection_lifetime());
                }
                let pool = builder
                    .build(manager)
                    .await
                    .map_err(|e| {
                        eprintln!(
                            "Failed to create redis connection pool on startup. Will continue to run without DataStore. Error: {e:?}"
                        );
                    })
                    .ok();

                // Only start minting tokens once there is a pool that will consume them.
                if pool.is_some() {
                    if let RedisCredentials::Oauth(provider) = &credentials {
                        provider.spawn_refresh_task();
                    }
                }
                pool
            }
            None => None,
        };

        RedisCache {
            connection: redis_pool,
            hash_cache: Arc::new(RwLock::new(HashMap::new())),
            uuid: uuid.to_string(),
            leader_key_ttl,
            check_lcut,
            redis_cache_ttl_in_s,
            double_write_cache_for_legacy_key,
            key_prefix: config.redis_key_prefix.unwrap_or_default(),
        }
    }

    /// Prepends the configured `REDIS_KEY_PREFIX` (if any) to `key`, normalizing a single
    /// trailing colon separator (e.g. `sfp-np` and `sfp-np:` both produce `sfp-np:<key>`).
    fn prefixed_key(&self, key: String) -> String {
        apply_key_prefix(&self.key_prefix, key)
    }

    fn leader_key(&self) -> String {
        self.prefixed_key(REDIS_LEADER_KEY.to_string())
    }

    async fn get_redis_key(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
        response_payload: Option<&Arc<ResponsePayload>>,
        use_legacy: bool,
    ) -> String {
        // Key should match SDK
        // New key schema uses the SDK key prefix (first 20 chars) instead of a hash
        // Key looks like: "statsig|{path}|{compression_encoding}|{sdk_key_prefix}"
        // For compression encoding, we only write plain text until we add support to decompress from sdk side
        let encoding = match response_payload {
            Some(payload) => match payload.use_proto {
                true => CompressionEncoder::StatsigBrotli,
                false => CompressionEncoder::PlainText,
            },
            _ => CompressionEncoder::PlainText,
        };
        let sdk_key = if use_legacy {
            self.hash_key(&request_context.sdk_key).await
        } else {
            let mut sdk_key_prefix = request_context.sdk_key.clone();
            sdk_key_prefix.truncate(20);
            sdk_key_prefix
        };

        self.prefixed_key(format!(
            "statsig|{}|{}|{}",
            request_context.path.as_str().trim_end_matches('/'),
            encoding,
            sdk_key
        ))
    }

    async fn hash_key(&self, key: &str) -> String {
        if self.hash_cache.read().contains_key(key) {
            return self
                .hash_cache
                .read()
                .get(key)
                .expect("Must have key")
                .to_string();
        }

        // Hash key so that we aren't loading a bunch of sdk keys
        // into memory
        let hashed_key = BASE64_STANDARD.encode(Sha256::digest(key)).to_string();
        self.hash_cache
            .write()
            .insert(key.to_string(), hashed_key.clone());
        hashed_key
    }

    async fn update_impl(
        &self,
        redis_key: String,
        result: &DataProviderRequestResult,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        data: &Arc<ResponsePayload>,
    ) {
        if result == &DataProviderRequestResult::DataAvailable {
            let connection = match self.connection.as_ref() {
                Some(conn) => conn.get().await,
                None => return,
            };
            match connection {
                Ok(mut conn) => {
                    let leader_key = self.leader_key();
                    let mut pipe = redis::pipe();
                    pipe.atomic();
                    let should_update = match pipe
                        .ttl(&leader_key)
                        .set_nx(&leader_key, self.uuid.clone())
                        .get(&leader_key)
                        .hget(&redis_key, "lcut")
                        .query_async::<(i32, i32, String, Option<String>)>(&mut *conn)
                        .await
                    {
                        Ok(query_result) => {
                            let is_leader = query_result.2 == self.uuid;

                            // In case there was a crash without cleaning up the leader key,
                            // validate on startup, and set expiry if needed. This is best
                            // effort, so we don't check result
                            if query_result.0 == -1 && !is_leader {
                                pipe.expire::<&str>(leader_key.as_str(), self.leader_key_ttl)
                                    .query_async::<i32>(&mut *conn)
                                    .await
                                    .ok();
                            }

                            if self.check_lcut {
                                let should_update = query_result
                                    .3
                                    .as_deref()
                                    .and_then(|existing_lcut| existing_lcut.parse::<u64>().ok())
                                    .is_none_or(|existing_lcut| existing_lcut < lcut);
                                is_leader && should_update
                            } else {
                                is_leader
                            }
                        }
                        Err(e) => {
                            println!("error checking if leader: {e:?}");
                            false
                        }
                    };

                    if !request_context.use_lcut || should_update {
                        // TODO update here
                        // We only store uncompressed json/proto data to redis for right now
                        let data_to_write = match *data.encoding {
                            CompressionEncoder::Gzip => {
                                let mut decoder = GzDecoder::new(Cursor::new(&**data.data));
                                let mut decompressed = Vec::new();
                                match decoder.read_to_end(&mut decompressed) {
                                    Ok(_) => decompressed,
                                    Err(e) => {
                                        ProxyEventObserver::publish_event(
                                            ProxyEvent::new_with_rc(
                                                ProxyEventType::RedisCacheWriteFailed,
                                                request_context,
                                            )
                                            .with_lcut(lcut)
                                            .with_stat(EventStat {
                                                operation_type: OperationType::IncrByValue,
                                                value: 1,
                                            }),
                                        );
                                        eprintln!("Failed to decode gzipped data before writing to redis: {e:?}");
                                        return;
                                    }
                                }
                            }
                            CompressionEncoder::PlainText => data.data.to_vec(),
                            CompressionEncoder::Brotli => {
                                let cursor = Cursor::new(&**data.data);
                                let mut decompressed = Vec::new();
                                let mut reader = brotli::Decompressor::new(cursor, 4096);
                                match reader.read_to_end(&mut decompressed) {
                                    Ok(_) => decompressed,
                                    Err(e) => {
                                        ProxyEventObserver::publish_event(
                                            ProxyEvent::new_with_rc(
                                                ProxyEventType::RedisCacheWriteFailed,
                                                request_context,
                                            )
                                            .with_lcut(lcut)
                                            .with_stat(EventStat {
                                                operation_type: OperationType::IncrByValue,
                                                value: 1,
                                            }),
                                        );
                                        eprintln!("Failed to decode br data before writing to redis: {e:?}");
                                        return;
                                    }
                                }
                            }
                            CompressionEncoder::StatsigBrotli => data.data.to_vec(),
                            CompressionEncoder::Deflate
                            | CompressionEncoder::Compress
                            | CompressionEncoder::Identity
                            | CompressionEncoder::Zstd => data.data.to_vec(),
                        };
                        // We currently only support writing data to redis as plain_text
                        match pipe
                            .hset(&redis_key, "encoding", "plain_text")
                            .hset(&redis_key, "lcut", lcut)
                            .hset(&redis_key, "config", data_to_write)
                            .expire(&redis_key, self.redis_cache_ttl_in_s)
                            .expire(&leader_key, self.leader_key_ttl)
                            .query_async::<()>(&mut *conn)
                            .await
                        {
                            Ok(_) => {
                                ProxyEventObserver::publish_event(
                                    ProxyEvent::new_with_rc(
                                        ProxyEventType::RedisCacheWriteSucceed,
                                        request_context,
                                    )
                                    .with_lcut(lcut)
                                    .with_stat(EventStat {
                                        operation_type: OperationType::IncrByValue,
                                        value: 1,
                                    }),
                                );
                            }
                            Err(e) => {
                                ProxyEventObserver::publish_event(
                                    ProxyEvent::new_with_rc(
                                        ProxyEventType::RedisCacheWriteFailed,
                                        request_context,
                                    )
                                    .with_lcut(lcut)
                                    .with_stat(EventStat {
                                        operation_type: OperationType::IncrByValue,
                                        value: 1,
                                    }),
                                );
                                eprintln!("Failed to set key in redis: {e:?}");
                            }
                        }
                    } else {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new_with_rc(
                                ProxyEventType::RedisCacheWriteSkipped,
                                request_context,
                            )
                            .with_lcut(lcut)
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                        );
                    }
                }
                Err(e) => {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new_with_rc(
                            ProxyEventType::RedisCacheWriteFailed,
                            request_context,
                        )
                        .with_lcut(lcut)
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                    );
                    eprintln!("Failed to get connection to redis, failed to update key: {e:?}");
                }
            }
        } else if result == &DataProviderRequestResult::Unauthorized {
            let connection = match self.connection.as_ref() {
                Some(conn) => conn.get().await,
                None => return,
            };
            match connection {
                Ok(mut conn) => match conn.del(&redis_key).await {
                    Ok(()) => {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new_with_rc(
                                ProxyEventType::RedisCacheDeleteSucceed,
                                request_context,
                            )
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                        );
                    }
                    Err(e) => {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new_with_rc(
                                ProxyEventType::RedisCacheDeleteFailed,
                                request_context,
                            )
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                        );
                        eprintln!("Failed to delete key in redis: {e:?}");
                    }
                },
                Err(e) => {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new_with_rc(
                            ProxyEventType::RedisCacheDeleteFailed,
                            request_context,
                        )
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                    );
                    eprintln!("Failed to get connection to redis, failed to delete key: {e:?}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_key_prefix, entra_config_from_env, is_secure_authority_host,
        min_idle_is_too_low_for_oauth, oauth_transport_is_plaintext, parse_auth_mode,
        resolve_redis_auth, RedisAuthMode, RedisEnvConfig, DEFAULT_ENTRA_AUTHORITY_HOST,
        MIN_REFRESH_MARGIN, REDIS_LEADER_KEY,
    };
    use std::time::Duration;

    fn oauth_env_config() -> RedisEnvConfig {
        RedisEnvConfig {
            redis_enterprise_user: None,
            redis_enterprise_password: None,
            redis_enterprise_host: "cache.example.com".to_string(),
            redis_enterprise_port: "6380".to_string(),
            redis_connection_pool_max_size: None,
            redis_connection_pool_min_size: None,
            redis_memorystore: None,
            redis_tls: Some(true),
            redis_key_prefix: None,
            redis_auth_mode: Some("oauth".to_string()),
            redis_oauth_tenant_id: Some("tenant".to_string()),
            redis_oauth_client_id: Some("client".to_string()),
            redis_oauth_client_secret: Some("secret".to_string()),
            redis_oauth_scope: Some("https://example.onmicrosoft.com/app/.default".to_string()),
            redis_oauth_username: None,
            redis_oauth_refresh_margin_in_s: None,
            redis_oauth_authority_host: None,
            redis_oauth_allow_plaintext: None,
        }
    }

    #[test]
    fn auth_user_and_password_sends_both() {
        let (username, password) = resolve_redis_auth("acl-user", Some("hunter2"));
        assert_eq!(username, Some("acl-user".to_string()));
        assert_eq!(password, Some("hunter2".to_string()));
    }

    #[test]
    fn auth_user_without_password_forces_empty_password() {
        // The redis-rs #1713 case: username-only ACL user with an empty password must still
        // send AUTH, so password must be Some("") rather than None.
        let (username, password) = resolve_redis_auth("acl-user", None);
        assert_eq!(username, Some("acl-user".to_string()));
        assert_eq!(password, Some(String::new()));
    }

    #[test]
    fn auth_password_only_has_no_username() {
        let (username, password) = resolve_redis_auth("", Some("hunter2"));
        assert_eq!(username, None);
        assert_eq!(password, Some("hunter2".to_string()));
    }

    #[test]
    fn auth_no_credentials_is_unauthenticated() {
        let (username, password) = resolve_redis_auth("", None);
        assert_eq!(username, None);
        assert_eq!(password, None);
    }

    #[test]
    fn prefix_empty_returns_key_unchanged() {
        assert_eq!(
            apply_key_prefix("", "statsig|foo".to_string()),
            "statsig|foo"
        );
        assert_eq!(
            apply_key_prefix("   ", "statsig|foo".to_string()),
            "statsig|foo"
        );
    }

    #[test]
    fn prefix_without_colon_inserts_separator() {
        assert_eq!(
            apply_key_prefix("sfp-np", "statsig|foo".to_string()),
            "sfp-np:statsig|foo"
        );
    }

    #[test]
    fn prefix_with_trailing_colon_is_not_doubled() {
        assert_eq!(
            apply_key_prefix("sfp-np:", "statsig|foo".to_string()),
            "sfp-np:statsig|foo"
        );
    }

    #[test]
    fn prefix_is_trimmed_before_applying() {
        assert_eq!(
            apply_key_prefix("  sfp-np  ", "statsig|foo".to_string()),
            "sfp-np:statsig|foo"
        );
    }

    #[test]
    fn prefix_applies_to_leader_key() {
        assert_eq!(
            apply_key_prefix("sfp-np:", REDIS_LEADER_KEY.to_string()),
            format!("sfp-np:{REDIS_LEADER_KEY}")
        );
    }

    #[test]
    fn auth_mode_defaults_to_password() {
        // Existing deployments set nothing, and must not take the Entra path.
        assert_eq!(parse_auth_mode(None), Ok(RedisAuthMode::Password));
        assert_eq!(parse_auth_mode(Some("")), Ok(RedisAuthMode::Password));
        assert_eq!(
            parse_auth_mode(Some("password")),
            Ok(RedisAuthMode::Password)
        );
    }

    #[test]
    fn auth_mode_parsing_is_case_and_whitespace_insensitive() {
        assert_eq!(parse_auth_mode(Some("  OAuth ")), Ok(RedisAuthMode::Oauth));
        assert_eq!(
            parse_auth_mode(Some(" Password")),
            Ok(RedisAuthMode::Password)
        );
    }

    #[test]
    fn auth_mode_rejects_unknown_values() {
        // Better to fail startup than to silently fall back to a different auth mode.
        let error = parse_auth_mode(Some("mtls")).expect_err("expected an error");
        assert!(error.contains("mtls"), "{error}");
    }

    #[test]
    fn entra_config_reads_all_oauth_settings() {
        let config = entra_config_from_env(&oauth_env_config()).expect("config should be valid");
        assert_eq!(config.tenant_id, "tenant");
        assert_eq!(config.client_id, "client");
        assert_eq!(config.client_secret, "secret");
        assert_eq!(config.scope, "https://example.onmicrosoft.com/app/.default");
        assert_eq!(config.authority_host, DEFAULT_ENTRA_AUTHORITY_HOST);
        // GEICO authenticates as the default user, so AUTH must be single-argument.
        assert_eq!(config.username, None);
        assert_eq!(config.refresh_margin, None);
    }

    #[test]
    fn entra_config_reports_every_missing_variable() {
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_tenant_id = None;
        env_config.redis_oauth_scope = Some("   ".to_string());

        let error = entra_config_from_env(&env_config).expect_err("expected an error");
        assert!(error.contains("REDIS_OAUTH_TENANT_ID"), "{error}");
        assert!(error.contains("REDIS_OAUTH_SCOPE"), "{error}");
        assert!(!error.contains("REDIS_OAUTH_CLIENT_ID"), "{error}");
    }

    #[test]
    fn entra_config_treats_blank_username_as_default_user() {
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_username = Some("  ".to_string());
        let config = entra_config_from_env(&env_config).expect("config should be valid");
        assert_eq!(config.username, None);
    }

    #[test]
    fn entra_config_keeps_explicit_username_and_margin() {
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_username = Some(" object-id ".to_string());
        env_config.redis_oauth_refresh_margin_in_s = Some("300".to_string());

        let config = entra_config_from_env(&env_config).expect("config should be valid");
        assert_eq!(config.username, Some("object-id".to_string()));
        assert_eq!(config.refresh_margin, Some(Duration::from_secs(300)));
    }

    #[test]
    fn entra_config_requires_tls_because_the_token_is_the_password() {
        // Unset is the case that matters: forgetting REDIS_TLS should not quietly put a live
        // Entra credential on the wire in cleartext.
        for tls in [None, Some(false)] {
            let mut env_config = oauth_env_config();
            env_config.redis_tls = tls;

            let error = entra_config_from_env(&env_config).expect_err("expected an error");
            assert!(error.contains("REDIS_TLS"), "{error}");
            assert!(error.contains("REDIS_OAUTH_ALLOW_PLAINTEXT"), "{error}");
        }
    }

    #[test]
    fn entra_config_allows_plaintext_only_when_explicitly_acknowledged() {
        // The sidecar case: something else encrypts the hop, so the operator opts in by name.
        let mut env_config = oauth_env_config();
        env_config.redis_tls = None;
        env_config.redis_oauth_allow_plaintext = Some("true".to_string());

        entra_config_from_env(&env_config).expect("an explicit opt-in should be honored");
    }

    #[test]
    fn the_plaintext_opt_in_is_inert_when_tls_is_on() {
        // It must not read as "turn TLS off"; it only waives the requirement.
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_allow_plaintext = Some("true".to_string());

        entra_config_from_env(&env_config).expect("config should be valid");
        assert!(!oauth_transport_is_plaintext(&env_config));
    }

    #[test]
    fn entra_config_rejects_a_plaintext_authority_host() {
        // The client secret and the access token both travel to this host.
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_authority_host =
            Some("http://login.microsoftonline.com".to_string());

        let error = entra_config_from_env(&env_config).expect_err("expected an error");
        assert!(error.contains("REDIS_OAUTH_AUTHORITY_HOST"), "{error}");
        assert!(error.contains("https"), "{error}");
    }

    #[test]
    fn entra_config_rejects_an_authority_host_without_a_scheme() {
        // Would otherwise fail later as an opaque relative-URL transport error.
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_authority_host = Some("login.microsoftonline.com".to_string());

        let error = entra_config_from_env(&env_config).expect_err("expected an error");
        assert!(error.contains("REDIS_OAUTH_AUTHORITY_HOST"), "{error}");
    }

    #[test]
    fn entra_config_accepts_a_sovereign_cloud_authority_host() {
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_authority_host =
            Some("  https://login.microsoftonline.us  ".to_string());

        let config = entra_config_from_env(&env_config).expect("config should be valid");
        assert_eq!(config.authority_host, "https://login.microsoftonline.us");
    }

    #[test]
    fn entra_config_falls_back_to_the_default_authority_host() {
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_authority_host = Some("   ".to_string());

        let config = entra_config_from_env(&env_config).expect("config should be valid");
        assert_eq!(config.authority_host, DEFAULT_ENTRA_AUTHORITY_HOST);
        assert!(is_secure_authority_host(DEFAULT_ENTRA_AUTHORITY_HOST));
    }

    #[test]
    fn authority_host_scheme_check_is_case_insensitive() {
        assert!(is_secure_authority_host(
            "HTTPS://login.microsoftonline.com"
        ));
        assert!(!is_secure_authority_host(
            "HTTP://login.microsoftonline.com"
        ));
        // A scheme that merely starts with the right letters must not slip through.
        assert!(!is_secure_authority_host(
            "https:/login.microsoftonline.com"
        ));
    }

    #[test]
    fn entra_config_rejects_a_refresh_margin_below_the_floor() {
        // A margin at or under the hard-expiry guard mints the next token only once checkouts
        // are already being refused, so the tail of every token becomes an outage window.
        for margin in [0, 30, 59] {
            let mut env_config = oauth_env_config();
            env_config.redis_oauth_refresh_margin_in_s = Some(margin.to_string());

            let error = entra_config_from_env(&env_config)
                .expect_err("expected a margin of {margin} to be rejected");
            assert!(error.contains("REDIS_OAUTH_REFRESH_MARGIN_IN_S"), "{error}");
        }
    }

    #[test]
    fn entra_config_accepts_the_smallest_allowed_refresh_margin() {
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_refresh_margin_in_s = Some(MIN_REFRESH_MARGIN.as_secs().to_string());

        let config = entra_config_from_env(&env_config).expect("config should be valid");
        assert_eq!(config.refresh_margin, Some(MIN_REFRESH_MARGIN));
    }

    #[test]
    fn oauth_only_values_are_not_parsed_until_oauth_mode_is_selected() {
        // The whole reason these are strings on the struct: `envy` deserializes every field
        // before REDIS_AUTH_MODE is read, so a typed field would panic a password-mode
        // deployment over a value the docs promise is ignored. A stale entry in a shared
        // ConfigMap is the realistic way to hit this.
        let env = [
            ("REDIS_ENTERPRISE_HOST", "cache.example.com"),
            ("REDIS_ENTERPRISE_PORT", "6380"),
            ("REDIS_AUTH_MODE", "password"),
            ("REDIS_OAUTH_REFRESH_MARGIN_IN_S", "not-a-number"),
            ("REDIS_OAUTH_ALLOW_PLAINTEXT", "maybe"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_string(), value.to_string()));

        let config = envy::from_iter::<_, RedisEnvConfig>(env)
            .expect("password mode must tolerate malformed OAuth values");

        assert_eq!(
            parse_auth_mode(config.redis_auth_mode.as_deref()),
            Ok(RedisAuthMode::Password)
        );
    }

    #[test]
    fn entra_config_rejects_an_unparsable_refresh_margin_by_name() {
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_refresh_margin_in_s = Some("60s".to_string());

        let error = entra_config_from_env(&env_config).expect_err("expected an error");
        assert!(error.contains("REDIS_OAUTH_REFRESH_MARGIN_IN_S"), "{error}");
        assert!(
            error.contains("60s"),
            "the error should show the value: {error}"
        );
    }

    #[test]
    fn entra_config_rejects_an_unparsable_plaintext_flag_by_name() {
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_allow_plaintext = Some("maybe".to_string());

        let error = entra_config_from_env(&env_config).expect_err("expected an error");
        assert!(error.contains("REDIS_OAUTH_ALLOW_PLAINTEXT"), "{error}");
        assert!(
            error.contains("maybe"),
            "the error should show the value: {error}"
        );
    }

    #[test]
    fn oauth_flags_accept_the_usual_spellings() {
        for raw in ["true", "TRUE", " True ", "1", "yes"] {
            let mut env_config = oauth_env_config();
            env_config.redis_tls = None;
            env_config.redis_oauth_allow_plaintext = Some(raw.to_string());

            entra_config_from_env(&env_config)
                .unwrap_or_else(|e| panic!("'{raw}' should waive the TLS requirement: {e}"));
        }
    }

    #[test]
    fn a_blank_oauth_value_reads_as_unset() {
        // A Kubernetes ConfigMap entry with no value arrives as an empty string, which should
        // mean "not configured" rather than a parse failure.
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_refresh_margin_in_s = Some("  ".to_string());
        env_config.redis_oauth_allow_plaintext = Some(String::new());

        let config = entra_config_from_env(&env_config).expect("config should be valid");
        assert_eq!(config.refresh_margin, None);
    }

    #[test]
    fn entra_config_trims_values_from_file_backed_secrets() {
        // A secret created from a file routinely carries a trailing newline. It passes the
        // presence check, so it has to be trimmed before it reaches the token URL.
        let mut env_config = oauth_env_config();
        env_config.redis_oauth_tenant_id = Some("tenant\n".to_string());
        env_config.redis_oauth_client_id = Some("  client  ".to_string());
        env_config.redis_oauth_client_secret = Some("secret\r\n".to_string());
        env_config.redis_oauth_scope = Some("\thttps://example.com/app/.default\n".to_string());

        let config = entra_config_from_env(&env_config).expect("config should be valid");
        assert_eq!(config.tenant_id, "tenant");
        assert_eq!(config.client_id, "client");
        assert_eq!(config.client_secret, "secret");
        assert_eq!(config.scope, "https://example.com/app/.default");
    }

    #[test]
    fn min_idle_warning_tracks_pool_size() {
        assert!(min_idle_is_too_low_for_oauth(1, 10));
        assert!(!min_idle_is_too_low_for_oauth(5, 10));
        assert!(!min_idle_is_too_low_for_oauth(10, 10));
    }
}
