use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use bb8_redis::redis::{AsyncCommands, RedisError};
use dashmap::DashMap;
use tokio::sync::Mutex;

use crate::datastore::caching::redis_pool::{
    create_redis_pool_at, pool_get_err, redis_address_from_uri, RedisEnvConfig, RedisPool,
};
use crate::observers::proxy_event_observer::ProxyEventObserver;
use crate::observers::{EventStat, OperationType, ProxyEvent, ProxyEventType};

use super::{DictClient, DictSnapshot, IdListKeySchema};

mod forward;

pub const DEFAULT_IDLIST_REDIS_POOL_SIZE: u32 = 32;
pub const DEFAULT_IDLIST_FETCH_TIMEOUT_MS: u64 = 50;
pub const DEFAULT_IDLIST_DICT_REFRESH_S: u64 = 60;
const UNKNOWN_TOKEN_RETRY_S: u64 = 30;
const UNKNOWN_TOKEN_CACHE_SIZE: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdListMembershipConfig {
    pub enable_reverse_index: bool,
    pub pool_size: u32,
    pub read_uri: Option<String>,
    pub fetch_timeout: Duration,
    pub dict_refresh_interval: Duration,
}

impl Default for IdListMembershipConfig {
    fn default() -> Self {
        Self {
            enable_reverse_index: true,
            pool_size: DEFAULT_IDLIST_REDIS_POOL_SIZE,
            read_uri: None,
            fetch_timeout: Duration::from_millis(DEFAULT_IDLIST_FETCH_TIMEOUT_MS),
            dict_refresh_interval: Duration::from_secs(DEFAULT_IDLIST_DICT_REFRESH_S),
        }
    }
}

impl IdListMembershipConfig {
    pub fn from_env() -> Result<Self, String> {
        let defaults = Self::default();
        let read_uri = match std::env::var("IDLIST_REDIS_READ_URI") {
            Ok(value) if value.trim().is_empty() => None,
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => return Err(format!("failed reading IDLIST_REDIS_READ_URI: {error}")),
        };
        let config = Self {
            enable_reverse_index: super::reverse_index_enabled_from_env()?,
            pool_size: env_u32("IDLIST_REDIS_POOL_SIZE", defaults.pool_size)?,
            read_uri,
            fetch_timeout: Duration::from_millis(env_u64(
                "IDLIST_FETCH_TIMEOUT_MS",
                defaults.fetch_timeout.as_millis() as u64,
            )?),
            dict_refresh_interval: Duration::from_secs(env_u64(
                "IDLIST_DICT_REFRESH_IN_S",
                defaults.dict_refresh_interval.as_secs(),
            )?),
        };
        if config.pool_size == 0
            || config.fetch_timeout.is_zero()
            || config.dict_refresh_interval.is_zero()
        {
            return Err("IDLIST_REDIS_POOL_SIZE, IDLIST_FETCH_TIMEOUT_MS, and \
                 IDLIST_DICT_REFRESH_IN_S must be greater than zero"
                .to_string());
        }
        Ok(config)
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

fn env_u32(name: &str, default: u32) -> Result<u32, String> {
    env_u64(name, u64::from(default)).and_then(|value| {
        u32::try_from(value).map_err(|_| format!("{name} is too large, got '{value}'"))
    })
}

/// Builds the fetcher's dedicated read pool.
///
/// The optional URI changes only the network address. Authentication and TLS validation still
/// use the shared Redis pool implementation, while sync writes continue using their primary pool.
pub async fn create_idlist_read_pool_from_env(
    connection_timeout_in_s: u64,
) -> Result<RedisPool, String> {
    let membership_config = IdListMembershipConfig::from_env()?;
    let mut redis_config =
        RedisEnvConfig::from_env().map_err(|error| format!("Malformed Redis config: {error}"))?;
    redis_config.redis_connection_pool_max_size = Some(membership_config.pool_size);
    redis_config.redis_connection_pool_min_size = Some(
        redis_config
            .redis_connection_pool_min_size
            .unwrap_or(1)
            .min(membership_config.pool_size),
    );
    let address = membership_config
        .read_uri
        .as_deref()
        .map(redis_address_from_uri)
        .transpose()
        .map_err(|error| error.to_string())?;
    create_redis_pool_at(&redis_config, connection_timeout_in_s, address)
        .await
        .map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResolvedMemberships {
    by_hash: HashMap<String, HashSet<String>>,
}

impl ResolvedMemberships {
    pub fn get(&self, lookup_id: &str) -> Option<&HashSet<String>> {
        self.by_hash.get(lookup_id)
    }

    pub fn as_map(&self) -> &HashMap<String, HashSet<String>> {
        &self.by_hash
    }

    pub fn into_map(self) -> HashMap<String, HashSet<String>> {
        self.by_hash
    }
}

#[derive(Debug)]
pub enum IdListMembershipError {
    Redis(RedisError),
    Timeout(Duration),
}

impl fmt::Display for IdListMembershipError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redis(error) => write!(formatter, "idlist membership Redis read failed: {error}"),
            Self::Timeout(timeout) => {
                write!(
                    formatter,
                    "idlist membership Redis read exceeded {timeout:?}"
                )
            }
        }
    }
}

impl std::error::Error for IdListMembershipError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Redis(error) => Some(error),
            Self::Timeout(_) => None,
        }
    }
}

impl From<RedisError> for IdListMembershipError {
    fn from(value: RedisError) -> Self {
        Self::Redis(value)
    }
}

/// Resolves one company's idlist namespace. The caller supplies the company hash
/// and the requesting key's visible lists for forward-only lookups.
///
/// Memberships are never cached. Reverse mode caches only the token dictionary;
/// forward-only mode reads committed versions and their sets on every request.
pub struct RedisIdListMembershipFetcher {
    enable_reverse_index: bool,
    schema: IdListKeySchema,
    skh: Arc<str>,
    pool: RedisPool,
    dict_cache: Arc<ArcSwap<DictSnapshot>>,
    dict_update_lock: Arc<Mutex<()>>,
    unknown_tokens: Arc<DashMap<u64, Instant>>,
    fetch_timeout: Duration,
    dict_refresh_interval: Duration,
}

impl RedisIdListMembershipFetcher {
    pub async fn new(
        schema: IdListKeySchema,
        skh: impl Into<Arc<str>>,
        pool: RedisPool,
        config: IdListMembershipConfig,
    ) -> Result<Arc<Self>, IdListMembershipError> {
        let skh = skh.into();
        let snapshot = if config.enable_reverse_index {
            load_dictionary(&schema, &skh, &pool, config.fetch_timeout).await?
        } else {
            DictSnapshot::default()
        };
        let fetcher = Arc::new(Self {
            enable_reverse_index: config.enable_reverse_index,
            schema,
            skh,
            pool,
            dict_cache: Arc::new(ArcSwap::from_pointee(snapshot)),
            dict_update_lock: Arc::new(Mutex::new(())),
            unknown_tokens: Arc::new(DashMap::with_capacity(UNKNOWN_TOKEN_CACHE_SIZE)),
            fetch_timeout: config.fetch_timeout,
            dict_refresh_interval: config.dict_refresh_interval,
        });
        if config.enable_reverse_index {
            fetcher.spawn_dictionary_refresh_loop();
        }
        Ok(fetcher)
    }

    /// Restricts forward-only reads to the authenticated key's visible lists.
    pub async fn fetch_visible<I, S>(
        &self,
        lookup_ids: I,
        visible: &HashSet<String>,
    ) -> Result<ResolvedMemberships, IdListMembershipError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        if self.enable_reverse_index {
            self.fetch(lookup_ids).await
        } else {
            self.fetch_forward(dedupe_lookup_ids(lookup_ids), visible)
                .await
        }
    }

    /// Resolves every distinct lookup ID in one non-atomic Redis pipeline.
    ///
    /// Unknown IDs and tokens map to empty sets, including during synchronization.
    /// Redis and pool failures are returned to the caller.
    pub async fn fetch<I, S>(
        &self,
        lookup_ids: I,
    ) -> Result<ResolvedMemberships, IdListMembershipError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let lookup_ids = dedupe_lookup_ids(lookup_ids);
        if lookup_ids.is_empty() {
            return Ok(empty_memberships(lookup_ids));
        }
        if !self.enable_reverse_index {
            // Without an explicit visibility scope, no forward lists may be returned.
            return Ok(empty_memberships(lookup_ids));
        }

        self.emit_pool_health();
        let read = async {
            let mut connection = self.pool.get().await.map_err(pool_get_err)?;
            let mut pipeline = bb8_redis::redis::pipe();
            for lookup_id in &lookup_ids {
                pipeline
                    .cmd("SMEMBERS")
                    .arg(self.schema.member_key(&self.skh, lookup_id));
            }
            let tokens = pipeline
                .query_async::<Vec<Vec<u64>>>(&mut *connection)
                .await?;
            Ok::<_, IdListMembershipError>(tokens)
        };
        let token_sets = match tokio::time::timeout(self.fetch_timeout, read).await {
            Ok(Ok(token_sets)) => token_sets,
            Ok(Err(error)) => {
                metric(
                    ProxyEventType::IdListMembershipFetchFailed,
                    OperationType::IncrByValue,
                    1,
                );
                return Err(error);
            }
            Err(_) => {
                metric(
                    ProxyEventType::IdListMembershipFetchFailed,
                    OperationType::IncrByValue,
                    1,
                );
                return Err(IdListMembershipError::Timeout(self.fetch_timeout));
            }
        };

        let dictionary = self.dict_cache.load_full();
        let mut by_hash = HashMap::with_capacity(lookup_ids.len());
        for (lookup_id, tokens) in lookup_ids.into_iter().zip(token_sets) {
            if tokens.is_empty() {
                metric(
                    ProxyEventType::IdListResolverMiss,
                    OperationType::IncrByValue,
                    1,
                );
            }
            let mut names = HashSet::with_capacity(tokens.len());
            for token in tokens {
                if let Some(name) = dictionary.token_to_name.get(&token) {
                    names.insert(name.clone());
                } else {
                    self.schedule_targeted_refresh(token);
                }
            }
            by_hash.insert(lookup_id, names);
        }
        Ok(ResolvedMemberships { by_hash })
    }

    pub async fn refresh_dictionary_now(&self) -> Result<(), IdListMembershipError> {
        let _guard = self.dict_update_lock.lock().await;
        match load_dictionary(&self.schema, &self.skh, &self.pool, self.fetch_timeout).await {
            Ok(snapshot) => {
                self.dict_cache.store(Arc::new(snapshot));
                metric(
                    ProxyEventType::IdListDictRefreshSuccess,
                    OperationType::IncrByValue,
                    1,
                );
                Ok(())
            }
            Err(error) => {
                metric(
                    ProxyEventType::IdListDictRefreshFailed,
                    OperationType::IncrByValue,
                    1,
                );
                Err(error)
            }
        }
    }

    fn spawn_dictionary_refresh_loop(self: &Arc<Self>) {
        let fetcher = Arc::downgrade(self);
        let interval = self.dict_refresh_interval;
        tokio::spawn(async move {
            tokio::time::sleep(interval).await;
            loop {
                let Some(fetcher) = fetcher.upgrade() else {
                    break;
                };
                if let Err(error) = fetcher.refresh_dictionary_now().await {
                    eprintln!("[SFP] Failed to refresh idlist dictionary: {error}");
                }
                drop(fetcher);
                tokio::time::sleep(interval).await;
            }
        });
    }

    fn schedule_targeted_refresh(&self, token: u64) {
        let now = Instant::now();
        if self.unknown_tokens.get(&token).is_some_and(|last_attempt| {
            now.saturating_duration_since(*last_attempt)
                < Duration::from_secs(UNKNOWN_TOKEN_RETRY_S)
        }) {
            return;
        }
        self.unknown_tokens.insert(token, now);
        if self.unknown_tokens.len() > UNKNOWN_TOKEN_CACHE_SIZE {
            self.unknown_tokens.retain(|_, last_attempt| {
                now.saturating_duration_since(*last_attempt)
                    < Duration::from_secs(UNKNOWN_TOKEN_RETRY_S)
            });
        }

        let pool = self.pool.clone();
        let dictionary = Arc::clone(&self.dict_cache);
        let update_lock = Arc::clone(&self.dict_update_lock);
        let unknown_tokens = Arc::clone(&self.unknown_tokens);
        let dict_key = self.schema.dict_key(&self.skh);
        let timeout = self.fetch_timeout;
        tokio::spawn(async move {
            let result = tokio::time::timeout(timeout, async {
                let mut connection = pool.get().await.map_err(pool_get_err)?;
                connection
                    .hget::<_, _, Option<String>>(dict_key, DictClient::token_field(token))
                    .await
            })
            .await;
            match result {
                Ok(Ok(Some(name))) => {
                    let _guard = update_lock.lock().await;
                    let mut next = (**dictionary.load()).clone();
                    next.name_to_token.insert(name.clone(), token);
                    next.token_to_name.insert(token, name);
                    dictionary.store(Arc::new(next));
                    unknown_tokens.remove(&token);
                    metric(
                        ProxyEventType::IdListDictTargetedRefresh,
                        OperationType::IncrByValue,
                        1,
                    );
                }
                Ok(Ok(None)) => {}
                Ok(Err(_)) | Err(_) => {
                    unknown_tokens.remove(&token);
                    metric(
                        ProxyEventType::IdListDictRefreshFailed,
                        OperationType::IncrByValue,
                        1,
                    );
                }
            }
        });
    }

    fn emit_pool_health(&self) {
        let state = self.pool.state();
        metric(
            ProxyEventType::IdListReadPoolConnections,
            OperationType::Gauge,
            i64::from(state.connections),
        );
        metric(
            ProxyEventType::IdListReadPoolIdleConnections,
            OperationType::Gauge,
            i64::from(state.idle_connections),
        );
    }
}

fn dedupe_lookup_ids<I, S>(lookup_ids: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut seen = HashSet::new();
    lookup_ids
        .into_iter()
        .map(Into::into)
        .filter(|lookup_id| seen.insert(lookup_id.clone()))
        .collect()
}

fn empty_memberships(lookup_ids: Vec<String>) -> ResolvedMemberships {
    ResolvedMemberships {
        by_hash: lookup_ids
            .into_iter()
            .map(|lookup_id| (lookup_id, HashSet::new()))
            .collect(),
    }
}

async fn load_dictionary(
    schema: &IdListKeySchema,
    skh: &str,
    pool: &RedisPool,
    timeout: Duration,
) -> Result<DictSnapshot, IdListMembershipError> {
    let dictionary = DictClient::new(pool.clone(), schema.clone());
    match tokio::time::timeout(timeout, dictionary.snapshot(skh)).await {
        Ok(result) => result.map_err(IdListMembershipError::Redis),
        Err(_) => Err(IdListMembershipError::Timeout(timeout)),
    }
}

fn metric(event_type: ProxyEventType, operation_type: OperationType, value: i64) {
    ProxyEventObserver::publish_event(ProxyEvent::new(event_type).with_stat(EventStat {
        operation_type,
        value,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::caching::redis_pool::{
        create_redis_pool, create_redis_pool_from_env, RedisEnvConfig,
    };
    use uuid::Uuid;

    async fn pool_or_skip() -> Option<RedisPool> {
        create_redis_pool_from_env(5).await.ok()
    }

    async fn single_connection_pool() -> Option<RedisPool> {
        let Ok(mut redis_config) = RedisEnvConfig::from_env() else {
            return None;
        };
        redis_config.redis_connection_pool_max_size = Some(1);
        redis_config.redis_connection_pool_min_size = Some(1);
        create_redis_pool(&redis_config, 5).await.ok()
    }

    fn test_config() -> IdListMembershipConfig {
        IdListMembershipConfig {
            fetch_timeout: Duration::from_millis(500),
            dict_refresh_interval: Duration::from_secs(60 * 60),
            ..IdListMembershipConfig::default()
        }
    }

    fn tight_timeout_config() -> IdListMembershipConfig {
        IdListMembershipConfig {
            fetch_timeout: Duration::from_millis(50),
            dict_refresh_interval: Duration::from_secs(60 * 60),
            ..IdListMembershipConfig::default()
        }
    }

    async fn synced_fetcher(
        schema: IdListKeySchema,
        skh: impl Into<Arc<str>>,
        pool: RedisPool,
        config: IdListMembershipConfig,
    ) -> Result<Arc<RedisIdListMembershipFetcher>, IdListMembershipError> {
        let skh = skh.into();
        let count = DictClient::new(pool.clone(), schema.clone())
            .snapshot(&skh)
            .await?
            .token_to_name
            .len();
        let _: () = pool
            .get()
            .await
            .unwrap()
            .set(schema.ready_key(&skh), count)
            .await?;
        RedisIdListMembershipFetcher::new(schema, skh, pool, config).await
    }

    #[test]
    fn dedupe_preserves_first_seen_order() {
        assert_eq!(
            dedupe_lookup_ids(["hash-b", "hash-a", "hash-b", "hash-c", "hash-a"]),
            vec!["hash-b", "hash-a", "hash-c"]
        );
    }

    #[test]
    fn empty_memberships_retains_every_lookup() {
        let resolved = empty_memberships(vec!["a".into(), "b".into()]);
        assert_eq!(resolved.as_map().len(), 2);
        assert!(resolved.get("a").unwrap().is_empty());
        assert!(resolved.get("b").unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_readiness_and_dictionary_return_available_memberships() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let fetcher =
            RedisIdListMembershipFetcher::new(schema.clone(), "skh", pool.clone(), test_config())
                .await
                .unwrap();

        assert!(fetcher
            .fetch(Vec::<String>::new())
            .await
            .unwrap()
            .as_map()
            .is_empty());
        assert!(fetcher
            .fetch(["hash-a"])
            .await
            .unwrap()
            .get("hash-a")
            .unwrap()
            .is_empty());
        let token = DictClient::new(pool.clone(), schema.clone())
            .get_or_allocate_token("skh", "available")
            .await
            .unwrap();
        fetcher.refresh_dictionary_now().await.unwrap();
        let mut conn = pool.get().await.unwrap();
        let _: usize = conn
            .sadd(schema.member_key("skh", "known"), &[token, 9999])
            .await
            .unwrap();
        for ready in [None, Some("0"), Some("invalid")] {
            if let Some(ready) = ready {
                let _: () = conn.set(schema.ready_key("skh"), ready).await.unwrap();
            } else {
                let _: usize = conn.del(schema.ready_key("skh")).await.unwrap();
            }
            let result = fetcher.fetch(["known", "missing"]).await.unwrap();
            assert_eq!(
                result.get("known").unwrap(),
                &HashSet::from(["available".into()])
            );
            assert!(result.get("missing").unwrap().is_empty());
        }
        let _: usize = conn.del(schema.dict_key("skh")).await.unwrap();
        fetcher.refresh_dictionary_now().await.unwrap();
        let result = fetcher.fetch(["known", "missing"]).await.unwrap();
        assert!(result.as_map().values().all(HashSet::is_empty));
    }

    #[tokio::test]
    async fn resolves_memberships_and_observes_live_member_updates() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        let dict = DictClient::new(pool.clone(), schema.clone());
        let alpha = dict.get_or_allocate_token(skh, "alpha").await.unwrap();
        let beta = dict.get_or_allocate_token(skh, "beta").await.unwrap();
        let mut connection = pool.get().await.unwrap();
        let _: usize = connection
            .sadd(schema.member_key(skh, "known"), &[alpha, beta])
            .await
            .unwrap();
        drop(connection);

        let fetcher = synced_fetcher(schema.clone(), skh, pool.clone(), test_config())
            .await
            .unwrap();
        let resolved = fetcher.fetch(["known", "missing", "known"]).await.unwrap();
        assert_eq!(
            resolved.get("known").unwrap(),
            &HashSet::from(["alpha".to_string(), "beta".to_string()])
        );
        assert!(resolved.get("missing").unwrap().is_empty());
        assert_eq!(resolved.as_map().len(), 2);

        let _: usize = pool
            .get()
            .await
            .unwrap()
            .srem(schema.member_key(skh, "known"), beta)
            .await
            .unwrap();
        let updated = fetcher.fetch(["known"]).await.unwrap();
        assert_eq!(
            updated.get("known").unwrap(),
            &HashSet::from(["alpha".to_string()])
        );
    }

    #[tokio::test]
    async fn unknown_token_triggers_targeted_dictionary_refresh() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        let dict = DictClient::new(pool.clone(), schema.clone());
        dict.get_or_allocate_token(skh, "existing").await.unwrap();
        let fetcher = synced_fetcher(schema.clone(), skh, pool.clone(), test_config())
            .await
            .unwrap();

        let new_token = dict.get_or_allocate_token(skh, "new-list").await.unwrap();
        let _: usize = pool
            .get()
            .await
            .unwrap()
            .sadd(schema.member_key(skh, "new-member"), new_token)
            .await
            .unwrap();

        assert!(fetcher
            .fetch(["new-member"])
            .await
            .unwrap()
            .get("new-member")
            .unwrap()
            .is_empty());

        for _ in 0..50 {
            if fetcher
                .fetch(["new-member"])
                .await
                .unwrap()
                .get("new-member")
                .unwrap()
                .contains("new-list")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("targeted dictionary refresh did not become visible");
    }

    #[tokio::test]
    async fn pool_saturation_surfaces_timeout_instead_of_empty_membership() {
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
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        DictClient::new(pool.clone(), schema.clone())
            .get_or_allocate_token(skh, "alpha")
            .await
            .unwrap();
        let fetcher = synced_fetcher(
            schema,
            skh,
            pool.clone(),
            IdListMembershipConfig {
                fetch_timeout: Duration::from_millis(10),
                dict_refresh_interval: Duration::from_secs(60 * 60),
                ..IdListMembershipConfig::default()
            },
        )
        .await
        .unwrap();

        let _only_connection = pool.get().await.unwrap();
        let error = fetcher.fetch(["known"]).await.unwrap_err();
        assert!(matches!(error, IdListMembershipError::Timeout(_)));
    }

    #[tokio::test]
    async fn empty_lookups_short_circuit_without_redis_checkout() {
        let Some(pool) = single_connection_pool().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        DictClient::new(pool.clone(), schema.clone())
            .get_or_allocate_token(skh, "alpha")
            .await
            .unwrap();
        let fetcher = synced_fetcher(schema, skh, pool.clone(), tight_timeout_config())
            .await
            .unwrap();

        let _held = pool.get().await.unwrap();
        let resolved = fetcher.fetch(Vec::<String>::new()).await.unwrap();
        assert!(resolved.as_map().is_empty());
    }

    #[tokio::test]
    async fn empty_dictionary_still_surfaces_redis_timeouts() {
        let Some(pool) = single_connection_pool().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let fetcher = synced_fetcher(schema, "skh", pool.clone(), tight_timeout_config())
            .await
            .unwrap();

        let _held = pool.get().await.unwrap();
        assert!(matches!(
            fetcher.fetch(["hash-a"]).await,
            Err(IdListMembershipError::Timeout(_))
        ));
    }

    #[tokio::test]
    async fn resolution_matrix_covers_member_non_member_missing_and_orphan_token() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        let dict = DictClient::new(pool.clone(), schema.clone());
        let alpha = dict.get_or_allocate_token(skh, "alpha").await.unwrap();
        let _beta = dict.get_or_allocate_token(skh, "beta").await.unwrap();
        let mut connection = pool.get().await.unwrap();
        let _: usize = connection
            .sadd(schema.member_key(skh, "member"), &[alpha])
            .await
            .unwrap();
        let _: usize = connection
            .sadd(schema.member_key(skh, "orphan"), &[9_001u64])
            .await
            .unwrap();
        drop(connection);

        let fetcher = synced_fetcher(schema, skh, pool, test_config())
            .await
            .unwrap();
        let resolved = fetcher
            .fetch(["member", "missing", "orphan"])
            .await
            .unwrap();

        assert_eq!(
            resolved.get("member").unwrap(),
            &HashSet::from(["alpha".to_string()]),
            "member of alpha must not also resolve beta just because beta exists in the dict"
        );
        assert!(resolved.get("missing").unwrap().is_empty());
        assert!(
            resolved.get("orphan").unwrap().is_empty(),
            "tokens absent from the dictionary must not invent list names"
        );
        assert_eq!(resolved.as_map().len(), 3);
    }

    #[tokio::test]
    async fn escaped_lookup_ids_round_trip_through_member_keys() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        let lookup_id = "hash::with::colons";
        let dict = DictClient::new(pool.clone(), schema.clone());
        let token = dict.get_or_allocate_token(skh, "alpha").await.unwrap();
        let _: usize = pool
            .get()
            .await
            .unwrap()
            .sadd(schema.member_key(skh, lookup_id), token)
            .await
            .unwrap();

        let fetcher = synced_fetcher(schema, skh, pool, test_config())
            .await
            .unwrap();
        assert_eq!(
            fetcher
                .fetch([lookup_id])
                .await
                .unwrap()
                .get(lookup_id)
                .unwrap(),
            &HashSet::from(["alpha".to_string()])
        );
    }

    #[tokio::test]
    async fn server_key_hashes_are_isolated() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let dict = DictClient::new(pool.clone(), schema.clone());
        let token_a = dict.get_or_allocate_token("skh-a", "alpha").await.unwrap();
        let token_b = dict.get_or_allocate_token("skh-b", "beta").await.unwrap();
        let mut connection = pool.get().await.unwrap();
        let _: usize = connection
            .sadd(schema.member_key("skh-a", "shared"), token_a)
            .await
            .unwrap();
        let _: usize = connection
            .sadd(schema.member_key("skh-b", "shared"), token_b)
            .await
            .unwrap();
        drop(connection);

        let fetcher_a = synced_fetcher(schema.clone(), "skh-a", pool.clone(), test_config())
            .await
            .unwrap();
        let fetcher_b = synced_fetcher(schema, "skh-b", pool, test_config())
            .await
            .unwrap();

        assert_eq!(
            fetcher_a
                .fetch(["shared"])
                .await
                .unwrap()
                .get("shared")
                .unwrap(),
            &HashSet::from(["alpha".to_string()])
        );
        assert_eq!(
            fetcher_b
                .fetch(["shared"])
                .await
                .unwrap()
                .get("shared")
                .unwrap(),
            &HashSet::from(["beta".to_string()])
        );
    }

    #[tokio::test]
    async fn live_add_is_visible_on_the_next_fetch() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        let dict = DictClient::new(pool.clone(), schema.clone());
        let token = dict.get_or_allocate_token(skh, "alpha").await.unwrap();
        let fetcher = synced_fetcher(schema.clone(), skh, pool.clone(), test_config())
            .await
            .unwrap();

        assert!(fetcher
            .fetch(["later"])
            .await
            .unwrap()
            .get("later")
            .unwrap()
            .is_empty());

        let _: usize = pool
            .get()
            .await
            .unwrap()
            .sadd(schema.member_key(skh, "later"), token)
            .await
            .unwrap();
        assert_eq!(
            fetcher
                .fetch(["later"])
                .await
                .unwrap()
                .get("later")
                .unwrap(),
            &HashSet::from(["alpha".to_string()])
        );
    }

    #[tokio::test]
    async fn wrongtype_member_key_returns_redis_error_not_empty() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        DictClient::new(pool.clone(), schema.clone())
            .get_or_allocate_token(skh, "alpha")
            .await
            .unwrap();
        let _: () = pool
            .get()
            .await
            .unwrap()
            .set(schema.member_key(skh, "broken"), "not-a-set")
            .await
            .unwrap();

        let fetcher = synced_fetcher(schema, skh, pool, test_config())
            .await
            .unwrap();
        let error = fetcher.fetch(["broken"]).await.unwrap_err();
        assert!(
            matches!(error, IdListMembershipError::Redis(_)),
            "expected Redis error, got {error:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_fetches_complete_within_timeout() {
        let Some(pool) = pool_or_skip().await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("test-{}", Uuid::new_v4()));
        let skh = "server-key-hash";
        let dict = DictClient::new(pool.clone(), schema.clone());
        let alpha = dict.get_or_allocate_token(skh, "alpha").await.unwrap();
        let mut connection = pool.get().await.unwrap();
        for lookup_id in ["h1", "h2", "h3"] {
            let _: usize = connection
                .sadd(schema.member_key(skh, lookup_id), alpha)
                .await
                .unwrap();
        }
        drop(connection);

        let fetcher = std::sync::Arc::new(
            synced_fetcher(schema, skh, pool, test_config())
                .await
                .unwrap(),
        );
        let mut joins = Vec::new();
        for _ in 0..128 {
            let fetcher = std::sync::Arc::clone(&fetcher);
            joins.push(tokio::spawn(async move {
                fetcher.fetch(["h1", "h2", "h3"]).await
            }));
        }
        for join in joins {
            let resolved = join.await.unwrap().expect("fetch timed out or failed");
            assert_eq!(resolved.as_map().len(), 3);
            for lookup_id in ["h1", "h2", "h3"] {
                assert_eq!(
                    resolved.get(lookup_id).unwrap(),
                    &HashSet::from(["alpha".to_string()])
                );
            }
        }
    }

    #[tokio::test]
    #[ignore]
    async fn smoke_fetch_memberships_from_proxy_synced_redis() {
        let company_id = std::env::var("STATSIG_TEST_COMPANY_ID")
            .expect("STATSIG_TEST_COMPANY_ID must be set for the smoke test");
        let skh = crate::datastore::idlist_membership::hash_company_id(&company_id);
        let schema = IdListKeySchema::new(
            crate::datastore::caching::redis_pool::redis_key_prefix_from_env(),
        );
        let pool = create_redis_pool_from_env(5)
            .await
            .expect("local Redis must be reachable");
        let snapshot = DictClient::new(pool.clone(), schema.clone())
            .snapshot(&skh)
            .await
            .unwrap();
        assert!(
            !snapshot.token_to_name.is_empty(),
            "dictionary is empty; proxy has not synced idlists yet"
        );

        let pattern = schema.member_key(&skh, "*");
        let mut connection = pool.get().await.unwrap();
        let mut cursor: u64 = 0;
        let mut member_keys = Vec::new();
        loop {
            let (next, batch): (u64, Vec<String>) = bb8_redis::redis::cmd("SCAN")
                .arg(cursor)
                .arg("MATCH")
                .arg(&pattern)
                .arg("COUNT")
                .arg(200)
                .query_async(&mut *connection)
                .await
                .unwrap();
            member_keys.extend(batch);
            cursor = next;
            if cursor == 0 || member_keys.len() >= 8 {
                break;
            }
        }
        drop(connection);
        assert!(
            !member_keys.is_empty(),
            "no member keys under {pattern}; worker has not fanned out yet"
        );

        let needle = schema.member_key(&skh, "");
        let lookup_ids: Vec<String> = member_keys
            .iter()
            .filter_map(|key| {
                let escaped = key.strip_prefix(&needle)?;
                crate::datastore::idlist_membership::schema::unescape_component(escaped)
            })
            .take(5)
            .collect();
        assert!(!lookup_ids.is_empty());

        let fetcher = RedisIdListMembershipFetcher::new(schema, skh, pool, test_config())
            .await
            .unwrap();
        let unknown = format!("missing-{}", Uuid::new_v4());
        let mut query = lookup_ids.clone();
        query.push(unknown.clone());
        let resolved = fetcher.fetch(query).await.unwrap();
        assert!(resolved.get(&unknown).unwrap().is_empty());
        let known_names: HashSet<_> = snapshot.token_to_name.values().cloned().collect();
        let mut matched = 0usize;
        for lookup_id in &lookup_ids {
            let names = resolved.get(lookup_id).expect("lookup present");
            assert!(
                names.iter().all(|name| known_names.contains(name)),
                "fetcher returned a name not in the dictionary for {lookup_id}"
            );
            if !names.is_empty() {
                matched += 1;
            }
        }
        assert!(
            matched > 0,
            "fetched member keys but all translated to empty name sets"
        );
        eprintln!(
            "smoke ok: dict_lists={} sampled_hashes={} hashes_with_names={}",
            snapshot.token_to_name.len(),
            lookup_ids.len(),
            matched
        );
    }
}
