use std::{
    collections::HashMap,
    io::{Cursor, Read, Write},
    sync::Arc,
};

use bb8_redis::{redis::AsyncCommands, RedisConnectionManager};
use parking_lot::RwLock;
use redis::aio::MultiplexedConnection;

use crate::{
    datastore::{
        config_spec_store::ConfigSpecForCompany,
        data_providers::{http_data_provider::ResponsePayload, DataProviderRequestResult},
    },
    observers::{
        proxy_event_observer::ProxyEventObserver, HttpDataProviderObserverTrait, ProxyEvent,
        ProxyEventType,
    },
    servers::authorized_request_context::AuthorizedRequestContext,
};

use crate::observers::EventStat;
use crate::observers::OperationType;

use bb8_redis::redis::RedisError;
use bytes::Bytes;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub struct RedisCache {
    key_prefix: String,
    connection: bb8::Pool<RedisConnectionManager>,
    hash_cache: Arc<RwLock<HashMap<String, String>>>,
    uuid: String,
    leader_key_ttl: i64,
    check_lcut: bool,
    redis_cache_ttl_in_s: i64,
    double_write_cache_for_legacy_key: bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RedisEnvConfig {
    pub redis_enterprise_password: Option<String>,
    pub redis_enterprise_host: String,
    pub redis_enterprise_port: String,
    pub redis_connection_pool_max_size: Option<u32>,
    pub redis_connection_pool_min_size: Option<u32>,
    pub redis_memorystore: Option<bool>,
    pub redis_tls: Option<bool>,
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
        result: &DataProviderRequestResult,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        data: &Arc<ResponsePayload>,
    ) {
        self.update_impl(
            self.get_redis_key(request_context).await,
            result,
            request_context,
            lcut,
            data,
        )
        .await;

        if self.double_write_cache_for_legacy_key {
            self.update_impl(
                format!(
                    "{}::{}",
                    self.key_prefix,
                    self.hash_key(&request_context.sdk_key).await
                ),
                result,
                request_context,
                lcut,
                data,
            )
            .await;
        }
    }

    async fn get(
        &self,
        request_context: &Arc<AuthorizedRequestContext>,
    ) -> Option<Arc<ConfigSpecForCompany>> {
        let connection = self.connection.get().await;
        let redis_key = self.get_redis_key(request_context).await;
        match connection {
            Ok(mut conn) => {
                let mut pipe = redis::pipe();
                pipe.atomic();
                let res: Result<(Option<u64>, Vec<u8>), RedisError> = pipe
                    .hget(&redis_key, "lcut")
                    .hget(&redis_key, "config")
                    .query_async::<MultiplexedConnection, (Option<u64>, Vec<u8>)>(&mut *conn)
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
                                .with_stat(EventStat {
                                    operation_type: OperationType::IncrByValue,
                                    value: 1,
                                }),
                            );
                            // Only decompress before writing for legacy use case
                            match request_context.use_gzip && self.double_write_cache_for_legacy_key
                            {
                                true => {
                                    let mut compressed = Vec::new();
                                    let mut encoder =
                                        GzEncoder::new(&mut compressed, Compression::best());
                                    if let Err(e) = encoder.write_all(&data) {
                                        eprintln!("Failed to gzip data from redis: {:?}", e);
                                        return None;
                                    }
                                    if let Err(e) = encoder.finish() {
                                        eprintln!("Failed to gzip data from redis: {:?}", e);
                                        return None;
                                    }
                                    Some(Arc::new(ConfigSpecForCompany {
                                        config: Arc::new(ResponsePayload {
                                            encoding: Arc::new(Some("gzip".to_string())),
                                            data: Arc::from(Bytes::from(compressed)),
                                        }),
                                        lcut: lcut.unwrap_or(0),
                                    }))
                                }
                                false => Some(Arc::new(ConfigSpecForCompany {
                                    config: Arc::new(ResponsePayload {
                                        encoding: Arc::new(None),
                                        data: Arc::from(Bytes::from(data)),
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
                        eprintln!("Failed to get key from redis: {:?}", e);
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
                eprintln!("Failed to get connection to redis: {:?}", e);
                None
            }
        }
    }
}

impl RedisCache {
    pub async fn new(
        key_prefix: String,
        leader_key_ttl: i64,
        uuid: &str,
        check_lcut: bool,
        redis_cache_ttl_in_s: i64,
        double_write_cache_for_legacy_key: bool,
    ) -> Self {
        let config = envy::from_env::<RedisEnvConfig>().expect("Malformed config");
        let protocol = match config.redis_tls.is_some_and(|x| x) {
            true => "rediss",
            false => "redis",
        };
        let password = match config.redis_enterprise_password {
            Some(password) => format!("{}@", password),
            None => "".to_string(),
        };
        let redis_url = format!(
            "{protocol}://{password}{host}:{port}",
            protocol = protocol,
            password = password,
            host = config.redis_enterprise_host,
            port = config.redis_enterprise_port
        );
        let redis_manager = RedisConnectionManager::new(redis_url)
            .expect("Failed to create redis connection manager");
        let redis_pool = bb8::Pool::builder()
            .max_size(config.redis_connection_pool_max_size.unwrap_or(10))
            .min_idle(config.redis_connection_pool_min_size.unwrap_or(1))
            .build(redis_manager)
            .await
            .expect("Failed to create redis connection pool on startup");

        RedisCache {
            key_prefix,
            connection: redis_pool,
            hash_cache: Arc::new(RwLock::new(HashMap::new())),
            uuid: uuid.to_string(),
            leader_key_ttl,
            check_lcut,
            redis_cache_ttl_in_s,
            double_write_cache_for_legacy_key,
        }
    }

    async fn get_redis_key(&self, request_context: &Arc<AuthorizedRequestContext>) -> String {
        format!(
            "statsig|{}|{}|{}",
            request_context.path,
            request_context.use_gzip,
            self.hash_key(&request_context.sdk_key).await
        )
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
        let hashed_key = format!("{:x}", Sha256::digest(key));
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
            let connection: Result<
                bb8::PooledConnection<RedisConnectionManager>,
                bb8::RunError<RedisError>,
            > = self.connection.get().await;
            match connection {
                Ok(mut conn) => {
                    let mut pipe = redis::pipe();
                    pipe.atomic();
                    let should_update = match pipe
                        .ttl(REDIS_LEADER_KEY)
                        .set_nx(REDIS_LEADER_KEY, self.uuid.clone())
                        .get(REDIS_LEADER_KEY)
                        .hget(&redis_key, "lcut")
                        .query_async::<MultiplexedConnection, (i32, i32, String, Option<String>)>(
                            &mut *conn,
                        )
                        .await
                    {
                        Ok(query_result) => {
                            let is_leader = query_result.2 == self.uuid;

                            // Incase there was a crash without cleaning up the leader key
                            // validate on startup, and set expiry if needed. THis is best
                            // effort, so we don't check result
                            if query_result.0 == -1 && !is_leader {
                                pipe.expire::<&str>(REDIS_LEADER_KEY, self.leader_key_ttl)
                                    .query_async::<MultiplexedConnection, i32>(&mut *conn)
                                    .await
                                    .ok();
                            }

                            if self.check_lcut && query_result.3.is_some() {
                                let should_update =
                                    query_result.3.expect("exists").parse().unwrap_or(0) < lcut;
                                is_leader && should_update
                            } else {
                                is_leader
                            }
                        }
                        Err(e) => {
                            println!("error checking if leader: {:?}", e);
                            false
                        }
                    };

                    if !request_context.use_lcut || should_update {
                        // Only decompress before writing for legacy use case
                        let data_to_write = match request_context.use_gzip
                            && self.double_write_cache_for_legacy_key
                        {
                            true => {
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
                                            .with_stat(EventStat {
                                                operation_type: OperationType::IncrByValue,
                                                value: 1,
                                            }),
                                        );
                                        eprintln!("Failed to decode gzipped data before writing to redis: {:?}", e);
                                        return;
                                    }
                                }
                            }
                            false => data.data.to_vec(),
                        };

                        // We currently only support writing data to redis as plain_text
                        match pipe
                            .hset(&redis_key, "encoding", "plain_text")
                            .hset(&redis_key, "lcut", lcut)
                            .hset(&redis_key, "config", data_to_write)
                            .expire(&redis_key, self.redis_cache_ttl_in_s)
                            .expire(REDIS_LEADER_KEY, self.leader_key_ttl)
                            .query_async::<MultiplexedConnection, ()>(&mut *conn)
                            .await
                        {
                            Ok(_) => {
                                ProxyEventObserver::publish_event(
                                    ProxyEvent::new_with_rc(
                                        ProxyEventType::RedisCacheWriteSucceed,
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
                                        ProxyEventType::RedisCacheWriteFailed,
                                        request_context,
                                    )
                                    .with_stat(EventStat {
                                        operation_type: OperationType::IncrByValue,
                                        value: 1,
                                    }),
                                );
                                eprintln!("Failed to set key in redis: {:?}", e);
                            }
                        }
                    } else {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new_with_rc(
                                ProxyEventType::RedisCacheWriteSkipped,
                                request_context,
                            )
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
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                    );
                    eprintln!(
                        "Failed to get connection to redis, failed to update key: {:?}",
                        e
                    );
                }
            }
        } else if result == &DataProviderRequestResult::Unauthorized {
            let connection = self.connection.get().await;
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
                        eprintln!("Failed to delete key in redis: {:?}", e);
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
                    eprintln!(
                        "Failed to get connection to redis, failed to delete key: {:?}",
                        e
                    );
                }
            }
        }
    }
}
