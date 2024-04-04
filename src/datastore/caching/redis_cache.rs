use std::{collections::HashMap, sync::Arc};

use bb8_redis::{redis::AsyncCommands, RedisConnectionManager};
use redis::aio::MultiplexedConnection;
use tokio::sync::RwLock;

use crate::{
    datastore::data_providers::DataProviderRequestResult,
    observers::{
        proxy_event_observer::ProxyEventObserver, NewDcsObserverTrait, ProxyEvent, ProxyEventType,
    },
};

use crate::observers::EventStat;
use crate::observers::OperationType;

use bb8_redis::redis::RedisError;
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub struct RedisCache {
    connection: bb8::Pool<RedisConnectionManager>,
    hash_cache: Arc<RwLock<HashMap<String, String>>>,
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

impl RedisCache {
    pub async fn new() -> Self {
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
            connection: redis_pool,
            hash_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn hash_key(&self, key: &str) -> String {
        if self.hash_cache.read().await.contains_key(key) {
            return self
                .hash_cache
                .read()
                .await
                .get(key)
                .expect("Must have key")
                .to_string();
        }

        // Hash key so that we aren't loading a bunch of sdk keys
        // into memory
        let hashed_key = format!("statsig::{:x}", Sha256::digest(key));
        self.hash_cache
            .write()
            .await
            .insert(key.to_string(), hashed_key.clone());
        hashed_key
    }
}

use async_trait::async_trait;
#[async_trait]
impl NewDcsObserverTrait for RedisCache {
    fn force_notifier_to_wait_for_update(&self) -> bool {
        false
    }

    async fn update(
        &self,
        result: &DataProviderRequestResult,
        key: &str,
        lcut: u64,
        data: &Arc<String>,
    ) {
        if result == &DataProviderRequestResult::DataAvailable {
            let connection = self.connection.get().await;
            let redis_key = self.hash_key(key).await;
            match connection {
                Ok(mut conn) => {
                    let mut pipe = redis::pipe();
                    pipe.atomic();
                    let should_update = match pipe
                        .hget(&redis_key, "lcut")
                        .query_async::<MultiplexedConnection, Vec<String>>(&mut *conn)
                        .await
                    {
                        Ok(stored_lcut) => {
                            if stored_lcut.len() != 1 {
                                false
                            } else {
                                stored_lcut[0].parse().unwrap_or(0) < lcut
                            }
                        }
                        Err(e) => {
                            println!("error: {:?}", e);
                            true
                        }
                    };

                    if should_update {
                        match pipe
                            .hset(&redis_key, "lcut", lcut)
                            .hset(&redis_key, "config", data.to_string())
                            .query_async::<MultiplexedConnection, ()>(&mut *conn)
                            .await
                        {
                            Ok(_) => {
                                ProxyEventObserver::publish_event(
                                    ProxyEvent::new(
                                        ProxyEventType::RedisCacheWriteSucceed,
                                        key.to_string(),
                                    )
                                    .with_lcut(lcut)
                                    .with_stat(EventStat {
                                        operation_type: OperationType::IncrByValue,
                                        value: 1,
                                    }),
                                )
                                .await;
                            }
                            Err(e) => {
                                ProxyEventObserver::publish_event(
                                    ProxyEvent::new(
                                        ProxyEventType::RedisCacheWriteFailed,
                                        key.to_string(),
                                    )
                                    .with_lcut(lcut)
                                    .with_stat(EventStat {
                                        operation_type: OperationType::IncrByValue,
                                        value: 1,
                                    }),
                                )
                                .await;
                                eprintln!("Failed to set key in redis: {:?}", e);
                            }
                        }
                    } else {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(
                                ProxyEventType::RedisCacheWriteSkipped,
                                key.to_string(),
                            )
                            .with_lcut(lcut)
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                        )
                        .await;
                    }
                }
                Err(e) => {
                    ProxyEventObserver::publish_event(
                        ProxyEvent::new(ProxyEventType::RedisCacheWriteFailed, key.to_string())
                            .with_lcut(lcut)
                            .with_stat(EventStat {
                                operation_type: OperationType::IncrByValue,
                                value: 1,
                            }),
                    )
                    .await;
                    eprintln!("Failed to get connection to redis: {:?}", e);
                }
            }
        }
    }

    async fn get(&self, key: &str) -> Option<Arc<String>> {
        let connection = self.connection.get().await;
        match connection {
            Ok(mut conn) => {
                let res: Result<Vec<String>, RedisError> =
                    conn.hget(self.hash_key(key).await, "config").await;
                match res {
                    Ok(data) => {
                        if data.is_empty() {
                            ProxyEventObserver::publish_event(
                                ProxyEvent::new(
                                    ProxyEventType::RedisCacheReadMiss,
                                    key.to_string(),
                                )
                                .with_stat(EventStat {
                                    operation_type: OperationType::IncrByValue,
                                    value: 1,
                                }),
                            )
                            .await;
                            None
                        } else {
                            ProxyEventObserver::publish_event(
                                ProxyEvent::new(
                                    ProxyEventType::RedisCacheReadSucceed,
                                    key.to_string(),
                                )
                                .with_stat(EventStat {
                                    operation_type: OperationType::IncrByValue,
                                    value: 1,
                                }),
                            )
                            .await;
                            Some(Arc::new(data.first().expect("Must have data").to_owned()))
                        }
                    }
                    Err(e) => {
                        ProxyEventObserver::publish_event(
                            ProxyEvent::new(ProxyEventType::RedisCacheReadFailed, key.to_string())
                                .with_stat(EventStat {
                                    operation_type: OperationType::IncrByValue,
                                    value: 1,
                                }),
                        )
                        .await;
                        eprintln!("Failed to get key from redis: {:?}", e);
                        None
                    }
                }
            }
            Err(e) => {
                ProxyEventObserver::publish_event(
                    ProxyEvent::new(ProxyEventType::RedisCacheReadFailed, key.to_string())
                        .with_stat(EventStat {
                            operation_type: OperationType::IncrByValue,
                            value: 1,
                        }),
                )
                .await;
                eprintln!("Failed to get connection to redis: {:?}", e);
                None
            }
        }
    }
}
