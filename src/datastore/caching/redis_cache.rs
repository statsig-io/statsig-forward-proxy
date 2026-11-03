use base64::{prelude::BASE64_STANDARD, Engine};
use bb8_redis::redis::AsyncCommands;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    io::{Cursor, Read, Write},
    sync::Arc,
};

use crate::{
    datastore::{
        caching::redis_pool::{
            apply_key_prefix, create_redis_pool, RedisEnvConfig, RedisPool, RedisPoolError,
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
use sha2::{Digest, Sha256};

// Compare strings to preserve the full u64 LCUT range (Lua numbers are doubles).
const REFRESH_TTL_SCRIPT: &str = r#"
if redis.call('HGET', KEYS[1], 'lcut') == ARGV[1] then
    return redis.call('EXPIRE', KEYS[1], ARGV[2])
end
return 0
"#;

#[derive(Clone, Default)]
struct ActiveCacheKeys {
    keys: Vec<String>,
    lcut: u64,
}

pub struct RedisCache {
    connection: Option<RedisPool>,
    hash_cache: Arc<RwLock<HashMap<String, String>>>,
    /// Redis keys derived from the last payload seen for each request context. An unchanged
    /// origin response carries no body, so this is how `touch` retains the actual plain/proto
    /// encoding and every enabled key schema instead of guessing a different key. The expected
    /// LCUT is retained even if the write fails, so stale Redis data cannot be renewed.
    active_cache_keys: Arc<RwLock<HashMap<Arc<AuthorizedRequestContext>, ActiveCacheKeys>>>,
    uuid: String,
    leader_key_ttl: i64,
    check_lcut: bool,
    redis_cache_ttl_in_s: i64,
    double_write_cache_for_legacy_key: bool,
    /// Optional namespace prefix for all Redis keys (REDIS_KEY_PREFIX), e.g. "sfp-np:".
    key_prefix: String,
}

const REDIS_LEADER_KEY: &str = "statsig_forward_proxy::leader";

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
        let redis_keys = self
            .get_redis_keys_for_payload(
                &request_context.authorized_request_context,
                &response_context.body,
            )
            .await;

        if response_context.result_type == DataProviderRequestResult::DataAvailable {
            self.active_cache_keys.write().insert(
                Arc::clone(&request_context.authorized_request_context),
                ActiveCacheKeys {
                    keys: redis_keys.clone(),
                    lcut: response_context.lcut,
                },
            );
        }

        for redis_key in redis_keys {
            self.update_impl(
                redis_key,
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

    async fn touch(&self, request_context: &Arc<AuthorizedRequestContext>) {
        let redis_keys = self
            .active_cache_keys
            .read()
            .get(request_context)
            .cloned()
            .unwrap_or_default();
        // A context can report no update before this cache observer has seen its first payload.
        // There is no key to refresh in that case, and checking out a connection would let no-op
        // tasks occupy the bounded refresh capacity during a Redis outage.
        if redis_keys.keys.is_empty() {
            return;
        }
        let Some(pool) = self.connection.as_ref() else {
            return;
        };
        match pool.get().await {
            Ok(mut conn) => {
                self.refresh_ttl(
                    &mut *conn,
                    &redis_keys.keys,
                    redis_keys.lcut,
                    request_context,
                )
                .await;
            }
            Err(e) => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new_with_rc(
                        ProxyEventType::RedisCacheTtlRefreshFailed,
                        request_context,
                    )
                    .with_stat(EventStat {
                        operation_type: OperationType::IncrByValue,
                        value: 1,
                    }),
                );
                eprintln!("Failed to get connection to redis, failed to refresh ttl: {e:?}");
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
        let config = RedisEnvConfig::from_env().expect("Malformed config");
        let redis_pool = match create_redis_pool(&config, redis_connection_timeout_in_s).await {
            Ok(pool) => Some(pool),
            Err(RedisPoolError::Build(error)) => {
                eprintln!(
                    "Failed to create redis connection pool on startup. Will continue to run without DataStore. Error: {error}"
                );
                None
            }
            Err(RedisPoolError::OauthToken(error)) => {
                eprintln!(
                    "Failed to acquire an Entra token for redis on startup. Will continue to run without DataStore. Error: {error}"
                );
                None
            }
            Err(error) => panic!("{error}"),
        };

        RedisCache {
            connection: redis_pool,
            hash_cache: Arc::new(RwLock::new(HashMap::new())),
            active_cache_keys: Arc::new(RwLock::new(HashMap::new())),
            uuid: uuid.to_string(),
            leader_key_ttl,
            check_lcut,
            redis_cache_ttl_in_s,
            double_write_cache_for_legacy_key,
            key_prefix: config.key_prefix().to_string(),
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

    /// Renews the full TTL only when Redis holds the version confirmed by the origin.
    ///
    /// The TTL is here to reap keys that no proxy maintains anymore, so it has to be measured
    /// from the last time a proxy confirmed the config was live rather than from the last time
    /// the config changed. Callers must only reach this after a successful origin poll, which is
    /// what keeps a torn-down deployment or a rotated SDK key expiring on schedule.
    ///
    /// Deliberately not gated on leadership: each pod tracks only the SDK keys it has served, so
    /// the leader may never poll a key that another pod is responsible for keeping warm. `EXPIRE`
    /// is O(1) and idempotent, which makes one call per key per poll the cheaper trade against a
    /// key that nobody refreshes.
    /// Returns the event it published, so callers and tests can tell the three outcomes apart.
    async fn refresh_ttl<C: bb8_redis::redis::aio::ConnectionLike + Send>(
        &self,
        conn: &mut C,
        redis_keys: &[String],
        expected_lcut: u64,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> ProxyEventType {
        // A missing or mismatched LCUT is a miss: never extend an older backup after a
        // failed write. An unchanged poll has no payload with which to repair the entry.
        // Recovery comes from the next cold start, whose foreground fetch asks for a full payload.
        if redis_keys.is_empty() {
            let event = ProxyEventType::RedisCacheTtlRefreshMissed;
            ProxyEventObserver::publish_event(
                ProxyEvent::new_with_rc(event, request_context).with_stat(EventStat {
                    operation_type: OperationType::IncrByValue,
                    value: 1,
                }),
            );
            return event;
        }

        let mut pipe = redis::pipe();
        for redis_key in redis_keys {
            // One script per key also works when legacy/new keys use different cluster slots.
            pipe.cmd("EVAL")
                .arg(REFRESH_TTL_SCRIPT)
                .arg(1)
                .arg(redis_key)
                .arg(expected_lcut)
                .arg(self.redis_cache_ttl_in_s);
        }
        let result = pipe.query_async::<Vec<i64>>(conn).await;

        let event = match result {
            Ok(ref refreshed)
                if refreshed.len() == redis_keys.len()
                    && refreshed.iter().all(|result| *result == 1) =>
            {
                ProxyEventType::RedisCacheTtlRefreshed
            }
            Ok(_) => ProxyEventType::RedisCacheTtlRefreshMissed,
            Err(e) => {
                eprintln!("Failed to refresh ttl for key in redis: {e:?}");
                ProxyEventType::RedisCacheTtlRefreshFailed
            }
        };

        ProxyEventObserver::publish_event(
            ProxyEvent::new_with_rc(event, request_context).with_stat(EventStat {
                operation_type: OperationType::IncrByValue,
                value: 1,
            }),
        );

        event
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

    async fn get_redis_keys_for_payload(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
        response_payload: &Arc<ResponsePayload>,
    ) -> Vec<String> {
        let mut redis_keys = vec![
            self.get_redis_key(request_context, Some(response_payload), false)
                .await,
        ];
        if self.double_write_cache_for_legacy_key {
            redis_keys.push(
                self.get_redis_key(request_context, Some(response_payload), true)
                    .await,
            );
        }
        redis_keys
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
                        // A successful data response can be skipped when this pod is not the writer
                        // or the returned LCUT is not newer. Only renew the confirmed version.
                        self.refresh_ttl(
                            &mut *conn,
                            std::slice::from_ref(&redis_key),
                            lcut,
                            request_context,
                        )
                        .await;

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

    use super::{apply_key_prefix, RedisCache, REDIS_LEADER_KEY};
    use crate::datastore::caching::fake_redis::FakeRedis;
    use crate::datastore::caching::redis_auth::{RedisCredentials, StatsigRedisConnectionManager};
    use crate::datastore::data_providers::http_data_provider::ResponsePayload;
    use crate::datastore::data_providers::{
        DataProviderRequestResult, FullRequestContext, ResponseContext,
    };
    use crate::observers::{HttpDataProviderObserverTrait, ProxyEventType};
    use crate::servers::authorized_request_context::AuthorizedRequestContext;
    use crate::servers::normalized_path::NormalizedPath;
    use crate::utils::compress_encoder::CompressionEncoder;
    use bb8_redis::redis::ConnectionAddr;
    use bytes::Bytes;
    use parking_lot::RwLock;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::time::Duration;

    const TEST_TTL_IN_S: i64 = 64800;

    /// A cache wired to `port` with no auth, which is all the fake server understands.
    async fn cache_for(port: u16, key_prefix: &str) -> RedisCache {
        cache_for_with_options(port, key_prefix, false).await
    }

    async fn cache_for_with_options(
        port: u16,
        key_prefix: &str,
        double_write_cache_for_legacy_key: bool,
    ) -> RedisCache {
        let manager = StatsigRedisConnectionManager::new(
            ConnectionAddr::Tcp("127.0.0.1".to_string(), port),
            RedisCredentials::Static {
                username: None,
                password: None,
            },
        );
        let pool = bb8::Pool::builder()
            .max_size(1)
            .build(manager)
            .await
            .expect("pool should build");

        RedisCache {
            connection: Some(pool),
            hash_cache: Arc::new(RwLock::new(HashMap::new())),
            active_cache_keys: Arc::new(RwLock::new(HashMap::new())),
            uuid: "test-uuid".to_string(),
            leader_key_ttl: 70,
            check_lcut: true,
            redis_cache_ttl_in_s: TEST_TTL_IN_S,
            double_write_cache_for_legacy_key,
            key_prefix: key_prefix.to_string(),
        }
    }

    fn request_context() -> Arc<AuthorizedRequestContext> {
        Arc::new(AuthorizedRequestContext::new(
            "secret-key".to_string(),
            NormalizedPath::V1DownloadConfigSpecs,
            vec![CompressionEncoder::PlainText],
        ))
    }

    fn response_payload(use_proto: bool) -> Arc<ResponsePayload> {
        Arc::new(ResponsePayload {
            encoding: Arc::new(if use_proto {
                CompressionEncoder::StatsigBrotli
            } else {
                CompressionEncoder::PlainText
            }),
            data: Arc::from(Bytes::from_static(b"{}")),
            use_proto,
        })
    }

    async fn remember_payload_keys(
        cache: &RedisCache,
        request_context: &Arc<AuthorizedRequestContext>,
        payload: &Arc<ResponsePayload>,
    ) -> Vec<String> {
        let redis_keys = cache
            .get_redis_keys_for_payload(request_context, payload)
            .await;
        cache.active_cache_keys.write().insert(
            Arc::clone(request_context),
            super::ActiveCacheKeys {
                keys: redis_keys.clone(),
                lcut: 123,
            },
        );
        redis_keys
    }

    #[tokio::test]
    async fn touch_refreshes_the_cache_key_ttl() {
        // The STADXP-447 case: a config that never changes is never written, so `touch` is the
        // only thing that keeps its entry from aging out of the backup cache.
        let redis = FakeRedis::start().await;
        let cache = cache_for(redis.port, "").await;
        let request_context = request_context();
        let expected_keys =
            remember_payload_keys(&cache, &request_context, &response_payload(false)).await;

        cache.touch(&request_context).await;

        assert_eq!(
            redis.refresh_commands(),
            vec![vec![
                "EVAL".to_string(),
                super::REFRESH_TTL_SCRIPT.to_string(),
                "1".to_string(),
                expected_keys[0].clone(),
                "123".to_string(),
                TEST_TTL_IN_S.to_string()
            ]]
        );
    }

    #[tokio::test]
    async fn touch_without_retained_keys_returns_without_using_redis() {
        let redis = FakeRedis::start().await;
        let cache = cache_for(redis.port, "").await;
        // Hold the pool's only connection. If touch tries to check one out before noticing that
        // no keys were retained, it will block here until the pool timeout.
        let _held_connection = cache
            .connection
            .as_ref()
            .expect("pool")
            .get()
            .await
            .expect("connection");
        redis.clear_commands();

        tokio::time::timeout(Duration::from_millis(100), cache.touch(&request_context()))
            .await
            .expect("an empty key set must return before pool checkout");

        assert!(
            redis.commands().is_empty(),
            "a context without a retained payload must not use Redis"
        );
    }

    #[tokio::test]
    async fn touch_refreshes_the_prefixed_key() {
        // A refresh aimed at the unprefixed key would silently do nothing for deployments that
        // set REDIS_KEY_PREFIX, which is exactly the configuration GEICO runs.
        let redis = FakeRedis::start().await;
        let cache = cache_for(redis.port, "sfp-np:").await;
        let request_context = request_context();
        remember_payload_keys(&cache, &request_context, &response_payload(false)).await;

        cache.touch(&request_context).await;

        let refreshed_key = redis
            .refresh_commands()
            .first()
            .and_then(|command| command.get(3).cloned())
            .expect("a refresh should have been sent");
        assert!(
            refreshed_key.starts_with("sfp-np:"),
            "expected the prefixed key, got {refreshed_key}"
        );
    }

    #[tokio::test]
    async fn touch_refreshes_proto_and_legacy_keys_that_were_written() {
        let redis = FakeRedis::start().await;
        let cache = cache_for_with_options(redis.port, "", true).await;
        let request_context = request_context();
        let payload = response_payload(true);
        let expected_keys = cache
            .get_redis_keys_for_payload(&request_context, &payload)
            .await;

        cache
            .update(
                &Arc::new(FullRequestContext {
                    authorized_request_context: Arc::clone(&request_context),
                }),
                &Arc::new(ResponseContext {
                    result_type: DataProviderRequestResult::DataAvailable,
                    lcut: 123,
                    request_since_time: 0,
                    body: payload,
                }),
            )
            .await;
        redis.clear_commands();

        cache.touch(&request_context).await;

        assert_eq!(expected_keys.len(), 2);
        assert!(
            expected_keys.iter().all(|key| key.contains("|statsig-br|")),
            "both schemas must retain the payload's proto encoding: {expected_keys:?}"
        );
        assert_eq!(
            redis.refresh_commands(),
            expected_keys
                .into_iter()
                .map(|key| vec![
                    "EVAL".to_string(),
                    super::REFRESH_TTL_SCRIPT.to_string(),
                    "1".to_string(),
                    key,
                    "123".to_string(),
                    TEST_TTL_IN_S.to_string()
                ])
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn refresh_ttl_reports_success_when_the_key_was_still_present() {
        let redis = FakeRedis::start_with_expire_reply(1).await;
        let cache = cache_for(redis.port, "").await;
        let mut conn = cache
            .connection
            .as_ref()
            .expect("pool")
            .get()
            .await
            .expect("connection");

        let event = cache
            .refresh_ttl(
                &mut *conn,
                &["statsig|test".to_string()],
                123,
                &request_context(),
            )
            .await;

        assert_eq!(event, ProxyEventType::RedisCacheTtlRefreshed);
    }

    #[tokio::test]
    async fn refresh_ttl_reports_a_miss_when_the_key_is_already_gone() {
        // EXPIRE answering 0 is the one signal that the backup cache has already been evicted,
        // which used to be entirely silent.
        let redis = FakeRedis::start_with_expire_reply(0).await;
        let cache = cache_for(redis.port, "").await;
        let mut conn = cache
            .connection
            .as_ref()
            .expect("pool")
            .get()
            .await
            .expect("connection");

        let event = cache
            .refresh_ttl(
                &mut *conn,
                &["statsig|test".to_string()],
                123,
                &request_context(),
            )
            .await;

        assert_eq!(event, ProxyEventType::RedisCacheTtlRefreshMissed);
    }

    #[tokio::test]
    async fn refresh_ttl_does_not_report_an_empty_key_set_as_success() {
        let redis = FakeRedis::start().await;
        let cache = cache_for(redis.port, "").await;
        let mut conn = cache
            .connection
            .as_ref()
            .expect("pool")
            .get()
            .await
            .expect("connection");

        let event = cache
            .refresh_ttl(&mut *conn, &[], 123, &request_context())
            .await;

        assert_eq!(event, ProxyEventType::RedisCacheTtlRefreshMissed);
    }

    #[tokio::test]
    async fn skipped_write_still_refreshes_the_ttl() {
        // The fake server cannot answer the leader-election pipeline, so the leader check fails
        // and the otherwise successful data response is declined.
        let redis = FakeRedis::start().await;
        let cache = cache_for(redis.port, "").await;
        let payload = response_payload(false);

        cache
            .update_impl(
                "statsig|test".to_string(),
                &DataProviderRequestResult::DataAvailable,
                &request_context(),
                1000,
                &payload,
            )
            .await;

        assert_eq!(
            redis.refresh_commands(),
            vec![vec![
                "EVAL".to_string(),
                super::REFRESH_TTL_SCRIPT.to_string(),
                "1".to_string(),
                "statsig|test".to_string(),
                "1000".to_string(),
                TEST_TTL_IN_S.to_string()
            ]]
        );
    }

    /// Run with TEST_REDIS_PORT pointing to an isolated Redis with HSET disabled:
    /// redis-server --port <port> --save "" --appendonly no --rename-command HSET ""
    #[tokio::test]
    #[ignore = "requires isolated Redis with HSET disabled and TEST_REDIS_PORT"]
    async fn failed_write_and_skipped_write_never_renew_an_older_backup() {
        use bb8_redis::redis::AsyncCommands;
        let port = std::env::var("TEST_REDIS_PORT").unwrap().parse().unwrap();
        let cache = cache_for_with_options(port, "ttl-regression", true).await;
        let context = request_context();
        let payload = response_payload(true);
        let keys = cache.get_redis_keys_for_payload(&context, &payload).await;
        let pool = cache.connection.as_ref().unwrap();
        {
            let mut conn = pool.get().await.unwrap();
            for key in &keys {
                redis::cmd("HMSET")
                    .arg(key)
                    .arg("lcut")
                    .arg(100)
                    .arg("config")
                    .arg("version A")
                    .query_async::<()>(&mut *conn)
                    .await
                    .unwrap();
                conn.expire::<_, ()>(key, 60).await.unwrap();
            }
        }
        // Redis accepts the leader check, but rejects the HSETs in the write transaction.
        // The observer must remember B even though Redis still contains A.
        cache
            .update(
                &Arc::new(FullRequestContext {
                    authorized_request_context: Arc::clone(&context),
                }),
                &Arc::new(ResponseContext {
                    result_type: DataProviderRequestResult::DataAvailable,
                    lcut: 200,
                    request_since_time: 100,
                    body: Arc::clone(&payload),
                }),
            )
            .await;
        for _ in 0..3 {
            cache.touch(&context).await;
        }
        {
            let mut conn = pool.get().await.unwrap();
            for key in &keys {
                assert_eq!(conn.hget::<_, _, u64>(key, "lcut").await.unwrap(), 100);
                assert!(conn.ttl::<_, i64>(key).await.unwrap() <= 60);
                assert_eq!(
                    cache
                        .refresh_ttl(&mut *conn, std::slice::from_ref(key), 200, &context)
                        .await,
                    ProxyEventType::RedisCacheTtlRefreshMissed
                );
            }
            // Force the other branch: a successful origin payload on a non-writer pod.
            conn.set_ex::<_, _, ()>(cache.leader_key(), "other-pod", 60)
                .await
                .unwrap();
        }
        for key in &keys {
            cache
                .update_impl(
                    key.clone(),
                    &DataProviderRequestResult::DataAvailable,
                    &context,
                    200,
                    &payload,
                )
                .await;
        }
        let mut conn = pool.get().await.unwrap();
        for key in &keys {
            assert!(conn.ttl::<_, i64>(key).await.unwrap() <= 60);
            // A matching version renews; a newer version is conservatively left alone too.
            for (cached, expected, event) in [
                (200_u64, 200_u64, ProxyEventType::RedisCacheTtlRefreshed),
                (201, 200, ProxyEventType::RedisCacheTtlRefreshMissed),
                (
                    u64::MAX,
                    u64::MAX - 1,
                    ProxyEventType::RedisCacheTtlRefreshMissed,
                ),
                (u64::MAX, u64::MAX, ProxyEventType::RedisCacheTtlRefreshed),
            ] {
                redis::cmd("HMSET")
                    .arg(key)
                    .arg("lcut")
                    .arg(cached)
                    .query_async::<()>(&mut *conn)
                    .await
                    .unwrap();
                conn.expire::<_, ()>(key, 60).await.unwrap();
                assert_eq!(
                    cache
                        .refresh_ttl(&mut *conn, std::slice::from_ref(key), expected, &context)
                        .await,
                    event
                );
                let ttl: i64 = conn.ttl(key).await.unwrap();
                if event == ProxyEventType::RedisCacheTtlRefreshed {
                    assert!(ttl > TEST_TTL_IN_S - 5);
                } else {
                    assert!(ttl <= 60);
                }
            }
            conn.del::<_, ()>(key).await.unwrap();
            assert_eq!(
                cache
                    .refresh_ttl(&mut *conn, std::slice::from_ref(key), 200, &context)
                    .await,
                ProxyEventType::RedisCacheTtlRefreshMissed
            );
            assert!(!conn.exists::<_, bool>(key).await.unwrap());
        }
    }

    #[test]
    fn prefix_applies_to_leader_key() {
        assert_eq!(
            apply_key_prefix("sfp-np:", REDIS_LEADER_KEY.to_string()),
            format!("sfp-np:{REDIS_LEADER_KEY}")
        );
    }
}
