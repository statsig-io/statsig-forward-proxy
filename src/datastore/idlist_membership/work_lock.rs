use bb8_redis::redis::{RedisError, Script};

use crate::datastore::caching::redis_pool::{pool_get_err, RedisPool};

/// Renewable Redis work lock using `SET NX EX` for acquire and Lua-guarded renew/release.
///
/// Same primitive as evaluation-proxy's cohort lock: only the holder whose ID matches the
/// stored value may renew or release the lock.
pub struct RedisWorkLock {
    pool: RedisPool,
    holder_id: String,
}

/// Lua: renew only if current value == holder_id → EXPIRE.
static RENEW_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
    return redis.call('EXPIRE', KEYS[1], ARGV[2])
else
    return 0
end
"#;

/// Lua: release only if current value == holder_id → DEL.
static RELEASE_SCRIPT: &str = r#"
if redis.call('GET', KEYS[1]) == ARGV[1] then
    return redis.call('DEL', KEYS[1])
else
    return 0
end
"#;

impl RedisWorkLock {
    pub fn new(pool: RedisPool, holder_id: impl Into<String>) -> Self {
        Self {
            pool,
            holder_id: holder_id.into(),
        }
    }

    pub fn holder_id(&self) -> &str {
        &self.holder_id
    }

    /// Attempts to acquire the lock. Returns `true` if this holder now owns the lock.
    pub async fn try_acquire(&self, key: &str, ttl_s: u64) -> Result<bool, RedisError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let acquired: Option<String> = bb8_redis::redis::cmd("SET")
            .arg(key)
            .arg(&self.holder_id)
            .arg("NX")
            .arg("EX")
            .arg(ttl_s)
            .query_async(&mut *conn)
            .await?;
        Ok(acquired.is_some())
    }

    /// Extends the lock TTL if this holder still owns it. Returns `false` after loss/expiry.
    pub async fn renew(&self, key: &str, ttl_s: u64) -> Result<bool, RedisError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let script = Script::new(RENEW_SCRIPT);
        let renewed: i32 = script
            .key(key)
            .arg(&self.holder_id)
            .arg(ttl_s)
            .invoke_async(&mut *conn)
            .await?;
        Ok(renewed == 1)
    }

    /// Releases the lock if this holder still owns it.
    pub async fn release(&self, key: &str) -> Result<(), RedisError> {
        let mut conn = self.pool.get().await.map_err(pool_get_err)?;
        let script = Script::new(RELEASE_SCRIPT);
        let _: i32 = script
            .key(key)
            .arg(&self.holder_id)
            .invoke_async(&mut *conn)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::caching::redis_pool::create_redis_pool_from_env;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    fn test_holder_id(label: &str) -> String {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        format!("{millis}-{label}-{}", Uuid::new_v4())
    }

    async fn test_pool() -> Option<RedisPool> {
        match create_redis_pool_from_env(5).await {
            Ok(pool) => Some(pool),
            Err(e) => {
                eprintln!("Skipping Redis integration test — pool unavailable: {e}");
                None
            }
        }
    }

    #[tokio::test]
    async fn acquire_and_release() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let lock = RedisWorkLock::new(pool.clone(), test_holder_id("a"));
        let key = format!("test:work_lock:{}", Uuid::new_v4());

        assert!(lock.try_acquire(&key, 30).await.unwrap());
        lock.release(&key).await.unwrap();

        // After release, another holder can acquire
        let lock2 = RedisWorkLock::new(pool, test_holder_id("b"));
        assert!(lock2.try_acquire(&key, 30).await.unwrap());
    }

    #[tokio::test]
    async fn second_holder_fails_while_held() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock1 = RedisWorkLock::new(pool.clone(), test_holder_id("a"));
        let lock2 = RedisWorkLock::new(pool, test_holder_id("b"));

        assert!(lock1.try_acquire(&key, 30).await.unwrap());
        assert!(!lock2.try_acquire(&key, 30).await.unwrap());
        lock1.release(&key).await.unwrap();
    }

    #[tokio::test]
    async fn renew_extends_lock() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock = RedisWorkLock::new(pool, test_holder_id("a"));

        assert!(lock.try_acquire(&key, 10).await.unwrap());
        assert!(lock.renew(&key, 30).await.unwrap());
        lock.release(&key).await.unwrap();
    }

    #[tokio::test]
    async fn renew_returns_false_after_loss() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock1 = RedisWorkLock::new(pool.clone(), test_holder_id("a"));
        let lock2 = RedisWorkLock::new(pool, test_holder_id("b"));

        assert!(lock1.try_acquire(&key, 30).await.unwrap());
        // Simulate takeover: lock2 forcefully sets (not via try_acquire which would fail)
        // Instead, release then acquire by lock2
        lock1.release(&key).await.unwrap();
        assert!(lock2.try_acquire(&key, 30).await.unwrap());
        // lock1 can no longer renew
        assert!(!lock1.renew(&key, 30).await.unwrap());
    }

    #[tokio::test]
    async fn release_only_deletes_own_lock() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock1 = RedisWorkLock::new(pool.clone(), test_holder_id("a"));
        let lock2 = RedisWorkLock::new(pool.clone(), test_holder_id("b"));

        assert!(lock1.try_acquire(&key, 30).await.unwrap());
        // lock2 release is a no-op (Lua compare fails)
        lock2.release(&key).await.unwrap();
        // lock1 still holds it
        assert!(!lock2.try_acquire(&key, 30).await.unwrap());
        lock1.release(&key).await.unwrap();
        assert!(lock2.try_acquire(&key, 30).await.unwrap());
    }

    #[tokio::test]
    async fn ttl_expiry_allows_reacquire() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock1 = RedisWorkLock::new(pool.clone(), test_holder_id("a"));
        let lock2 = RedisWorkLock::new(pool, test_holder_id("b"));

        assert!(lock1.try_acquire(&key, 1).await.unwrap());
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(lock2.try_acquire(&key, 30).await.unwrap());
    }

    #[tokio::test]
    async fn race_exactly_one_winner() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let n = 20;
        let mut handles = Vec::new();
        for i in 0..n {
            let pool = pool.clone();
            let key = key.clone();
            handles.push(tokio::spawn(async move {
                let lock = RedisWorkLock::new(pool, test_holder_id(&format!("worker-{i}")));
                lock.try_acquire(&key, 30).await.unwrap()
            }));
        }
        let results: Vec<bool> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(results.iter().filter(|&&w| w).count(), 1);
    }

    #[tokio::test]
    async fn same_holder_cannot_reacquire_while_held() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let lock = RedisWorkLock::new(pool, test_holder_id("a"));
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        assert!(lock.try_acquire(&key, 30).await.unwrap());
        assert!(!lock.try_acquire(&key, 30).await.unwrap());
        lock.release(&key).await.unwrap();
    }

    #[tokio::test]
    async fn renew_extends_ttl() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock = RedisWorkLock::new(pool.clone(), test_holder_id("a"));
        assert!(lock.try_acquire(&key, 10).await.unwrap());

        let mut conn = pool.get().await.unwrap();
        let ttl_before: i64 = bb8_redis::redis::cmd("TTL")
            .arg(&key)
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(ttl_before > 0 && ttl_before <= 10, "ttl={ttl_before}");

        assert!(lock.renew(&key, 30).await.unwrap());
        let ttl_after: i64 = bb8_redis::redis::cmd("TTL")
            .arg(&key)
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(
            ttl_after > ttl_before && ttl_after <= 30,
            "ttl_before={ttl_before} ttl_after={ttl_after}"
        );
        lock.release(&key).await.unwrap();
    }

    #[tokio::test]
    async fn renew_and_release_missing_key_are_noops() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock = RedisWorkLock::new(pool, test_holder_id("a"));
        assert!(!lock.renew(&key, 30).await.unwrap());
        lock.release(&key).await.unwrap();
        assert!(lock.try_acquire(&key, 30).await.unwrap());
        lock.release(&key).await.unwrap();
    }

    #[tokio::test]
    async fn renew_returns_false_after_expiry() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock = RedisWorkLock::new(pool, test_holder_id("a"));
        assert!(lock.try_acquire(&key, 1).await.unwrap());
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        assert!(!lock.renew(&key, 30).await.unwrap());
    }

    #[tokio::test]
    async fn double_release_does_not_delete_new_holder() {
        let Some(pool) = test_pool().await else {
            return;
        };
        let key = format!("test:work_lock:{}", Uuid::new_v4());
        let lock1 = RedisWorkLock::new(pool.clone(), test_holder_id("a"));
        let lock2 = RedisWorkLock::new(pool, test_holder_id("b"));
        assert!(lock1.try_acquire(&key, 30).await.unwrap());
        lock1.release(&key).await.unwrap();
        assert!(lock2.try_acquire(&key, 30).await.unwrap());
        lock1.release(&key).await.unwrap();
        assert!(!lock1.try_acquire(&key, 30).await.unwrap());
        lock2.release(&key).await.unwrap();
    }
}
