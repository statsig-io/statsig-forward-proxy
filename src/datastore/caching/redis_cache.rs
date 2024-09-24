use std::{collections::HashMap, sync::Arc};

use bb8_redis::{redis::AsyncCommands, RedisConnectionManager};
use parking_lot::RwLock;
use redis::aio::MultiplexedConnection;

use crate::{
    datastore::data_providers::DataProviderRequestResult,
    observers::{
        proxy_event_observer::ProxyEventObserver, HttpDataProviderObserverTrait, ProxyEvent,
        ProxyEventType,
    },
    servers::authorized_request_context::AuthorizedRequestContext,
};

use crate::observers::EventStat;
use crate::observers::OperationType;

use bb8_redis::redis::RedisError;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub struct RedisCache {
    key_prefix: String,
    connection: bb8::Pool<RedisConnectionManager>,
    hash_cache: Arc<RwLock<HashMap<String, String>>>,
    uuid: String,
    leader_key_ttl: i64,
    check_lcut: bool,
    clear_external_datastore_on_unauthorized: bool,
    redis_cache_ttl_in_s: i64,
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

impl RedisCache {
    pub async fn new(
        key_prefix: String,
        leader_key_ttl: i64,
        uuid: &str,
        check_lcut: bool,
        clear_external_datastore_on_unauthorized: bool,
        redis_cache_ttl_in_s: i64,
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
            clear_external_datastore_on_unauthorized,
            redis_cache_ttl_in_s,
        }
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
        let hashed_key = format!("{}::{:x}", self.key_prefix, Sha256::digest(key));
        self.hash_cache
            .write()
            .insert(key.to_string(), hashed_key.clone());
        hashed_key
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
        result: &DataProviderRequestResult,
        request_context: &Arc<AuthorizedRequestContext>,
        lcut: u64,
        data: &Arc<str>,
    ) {
        if result == &DataProviderRequestResult::DataAvailable {
            let connection: Result<
                bb8::PooledConnection<RedisConnectionManager>,
                bb8::RunError<RedisError>,
            > = self.connection.get().await;
            // TODO: This will be a problem if we start using DCS v2 with the forward
            //       proxy because the redis data adapter currently has no way
            //       to differentiate between the DCS v1 and DCS v2.
            //
            //       So for now, to keep functionality, continue using just
            //       the sdk key.
            let redis_key = self.hash_key(&request_context.sdk_key).await;
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

                    if should_update {
                        match pipe
                            .hset(&redis_key, "lcut", lcut)
                            .hset(&redis_key, "config", data.to_string())
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
        } else if result == &DataProviderRequestResult::Unauthorized
            && self.clear_external_datastore_on_unauthorized
        {
            let connection = self.connection.get().await;
            let redis_key = self.hash_key(&request_context.to_string()).await;
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

    async fn get(&self, request_context: &Arc<AuthorizedRequestContext>) -> Option<Arc<str>> {
        let connection = self.connection.get().await;
        match connection {
            Ok(mut conn) => {
                let res: Result<Vec<String>, RedisError> = conn
                    .hget(self.hash_key(&request_context.to_string()).await, "config")
                    .await;
                match res {
                    Ok(data) => {
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
                            Some(Arc::from(data.first().expect("Must have data").to_owned()))
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
