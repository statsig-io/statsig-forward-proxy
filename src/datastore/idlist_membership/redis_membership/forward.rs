use super::*;
use crate::datastore::idlist_membership::schema::state_fields;

impl RedisIdListMembershipFetcher {
    pub(super) async fn fetch_forward(
        &self,
        lookup_ids: Vec<String>,
        visible: &HashSet<String>,
    ) -> Result<ResolvedMemberships, IdListMembershipError> {
        if lookup_ids.is_empty() || visible.is_empty() {
            return Ok(empty_memberships(lookup_ids));
        }
        self.emit_pool_health();
        let read = async {
            let mut connection = self.pool.get().await.map_err(pool_get_err)?;
            let lists: Vec<_> = visible.iter().collect();
            let mut pipe = bb8_redis::redis::pipe();
            for list in &lists {
                pipe.cmd("HMGET")
                    .arg(self.schema.state_key(&self.skh, list))
                    .arg(&[state_fields::VERSION, state_fields::LOADING_VERSION]);
            }
            let versions: Vec<(Option<String>, Option<String>)> =
                pipe.query_async(&mut *connection).await?;
            let mut queried_lists = Vec::with_capacity(lists.len());
            let mut pipe = bb8_redis::redis::pipe();
            for (list, (version, loading)) in lists.iter().zip(versions) {
                // Keep the current generation during rotation. On a first load,
                // return memberships already present in the loading generation.
                let Some(version) = version
                    .filter(|v| !v.is_empty())
                    .or_else(|| loading.filter(|v| !v.is_empty()))
                else {
                    continue;
                };
                pipe.cmd("SMISMEMBER")
                    .arg(self.schema.fwd_key(&self.skh, list, &version))
                    .arg(&lookup_ids);
                queried_lists.push(list);
            }
            let mut result = empty_memberships(lookup_ids.clone());
            if !queried_lists.is_empty() {
                let replies: Vec<Vec<bool>> = pipe.query_async(&mut *connection).await?;
                for (list, memberships) in queried_lists.into_iter().zip(replies) {
                    for (id, member) in lookup_ids.iter().zip(memberships) {
                        if member {
                            result
                                .by_hash
                                .get_mut(id)
                                .expect("initialized ID")
                                .insert((**list).clone());
                        }
                    }
                }
            }
            for names in result.by_hash.values() {
                if names.is_empty() {
                    metric(
                        ProxyEventType::IdListResolverMiss,
                        OperationType::IncrByValue,
                        1,
                    );
                }
            }
            Ok(result)
        };
        match tokio::time::timeout(self.fetch_timeout, read).await {
            Ok(Ok(result)) => Ok(result),
            result => {
                metric(
                    ProxyEventType::IdListMembershipFetchFailed,
                    OperationType::IncrByValue,
                    1,
                );
                match result {
                    Ok(Err(error)) => Err(error),
                    Err(_) => Err(IdListMembershipError::Timeout(self.fetch_timeout)),
                    Ok(Ok(_)) => unreachable!(),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datastore::caching::redis_pool::create_redis_pool_from_env;
    use crate::datastore::idlist_membership::ListSyncState;

    #[tokio::test]
    async fn forward_only_returns_available_memberships_without_readiness_checks() {
        let Ok(pool) = create_redis_pool_from_env(5).await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let schema = IdListKeySchema::new(format!("forward-read-{}", uuid::Uuid::new_v4()));
        let skh = "company";
        let mut conn = pool.get().await.unwrap();
        for (list, members) in [
            ("allowed", vec!["A", "B"]),
            ("hidden", vec!["B"]),
            ("empty", vec![]),
            ("missing-set", vec![]),
        ] {
            let state = ListSyncState {
                version: "committed".into(),
                cardinality: Some(2),
                loading_version: Some("pending".into()),
                reconcile_required: true,
                ..Default::default()
            };
            let _: () = conn
                .hset_multiple(schema.state_key(skh, list), &state.to_hash_fields())
                .await
                .unwrap();
            if !members.is_empty() {
                let _: usize = conn
                    .sadd(schema.fwd_key(skh, list, "committed"), members)
                    .await
                    .unwrap();
            }
        }
        let loading = ListSyncState {
            loading_version: Some("pending".into()),
            reconcile_required: true,
            ..Default::default()
        };
        let _: () = conn
            .hset_multiple(schema.state_key(skh, "loading"), &loading.to_hash_fields())
            .await
            .unwrap();
        let _: usize = conn
            .sadd(schema.fwd_key(skh, "loading", "pending"), "C")
            .await
            .unwrap();
        let _: usize = conn
            .sadd(schema.fwd_key(skh, "allowed", "pending"), "C")
            .await
            .unwrap();
        // Corrupt out-of-scope data and unused readiness/cardinality metadata must not gate reads.
        let _: () = conn
            .set(schema.fwd_key(skh, "hidden", "committed"), "wrong-type")
            .await
            .unwrap();
        let _: () = conn
            .set(schema.member_key(skh, "A"), "wrong-type")
            .await
            .unwrap();
        let _: () = conn.set(schema.ready_key(skh), "invalid").await.unwrap();
        let _: () = conn
            .hset(
                schema.state_key(skh, "allowed"),
                state_fields::CARDINALITY,
                "invalid",
            )
            .await
            .unwrap();
        let fetcher = RedisIdListMembershipFetcher::new(
            schema.clone(),
            skh,
            pool.clone(),
            IdListMembershipConfig {
                enable_reverse_index: false,
                fetch_timeout: Duration::from_secs(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let visible = HashSet::from([
            "allowed".into(),
            "empty".into(),
            "missing-state".into(),
            "missing-set".into(),
            "loading".into(),
        ]);
        let result = fetcher
            .fetch_visible(["A", "B", "C", "unknown", "A"], &visible)
            .await
            .unwrap();
        assert_eq!(result.as_map().len(), 4);
        for id in ["A", "B"] {
            assert_eq!(result.get(id).unwrap(), &HashSet::from(["allowed".into()]));
        }
        assert_eq!(result.get("C").unwrap(), &HashSet::from(["loading".into()]));
        assert!(result.get("unknown").unwrap().is_empty());
        assert!(!conn.exists::<_, bool>(schema.dict_key(skh)).await.unwrap());
        let many_ids =
            std::iter::once("A".to_string()).chain((0..9000).map(|n| format!("missing-{n}")));
        let many = fetcher.fetch_visible(many_ids, &visible).await.unwrap();
        assert_eq!(many.as_map().len(), 9001);
        assert_eq!(many.get("A").unwrap(), &HashSet::from(["allowed".into()]));

        let _: () = conn
            .hset(
                schema.state_key(skh, "allowed"),
                state_fields::LOADING_VERSION,
                "committed",
            )
            .await
            .unwrap();
        assert_eq!(
            fetcher
                .fetch_visible(["A"], &visible)
                .await
                .unwrap()
                .get("A")
                .unwrap(),
            &HashSet::from(["allowed".into()])
        );
        let _: usize = conn
            .del(schema.fwd_key(skh, "allowed", "committed"))
            .await
            .unwrap();
        let _: usize = conn.del(schema.ready_key(skh)).await.unwrap();
        let result = fetcher.fetch_visible(["A", "C"], &visible).await.unwrap();
        assert!(result.get("A").unwrap().is_empty());
        assert_eq!(result.get("C").unwrap(), &HashSet::from(["loading".into()]));
        assert!(fetcher
            .fetch(["A"])
            .await
            .unwrap()
            .get("A")
            .unwrap()
            .is_empty());
        assert!(matches!(
            fetcher
                .fetch_visible(["A"], &HashSet::from(["hidden".into()]))
                .await,
            Err(IdListMembershipError::Redis(_))
        ));
        assert!(fetcher
            .fetch_visible(Vec::<String>::new(), &visible)
            .await
            .unwrap()
            .as_map()
            .is_empty());
    }

    #[tokio::test]
    async fn forward_only_pool_saturation_returns_timeout() {
        use crate::datastore::caching::redis_pool::{create_redis_pool, RedisEnvConfig};
        let Ok(mut config) = RedisEnvConfig::from_env() else {
            return;
        };
        config.redis_connection_pool_max_size = Some(1);
        config.redis_connection_pool_min_size = Some(1);
        let Ok(pool) = create_redis_pool(&config, 5).await else {
            eprintln!("Skipping — Redis unavailable");
            return;
        };
        let fetcher = RedisIdListMembershipFetcher::new(
            IdListKeySchema::new("timeout"),
            "company",
            pool.clone(),
            IdListMembershipConfig {
                enable_reverse_index: false,
                fetch_timeout: Duration::from_millis(10),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let _held = pool.get().await.unwrap();
        assert!(matches!(
            fetcher
                .fetch_visible(["A"], &HashSet::from(["list".into()]))
                .await,
            Err(IdListMembershipError::Timeout(_))
        ));
    }
}
