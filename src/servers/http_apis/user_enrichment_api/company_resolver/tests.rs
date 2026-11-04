use super::*;
use crate::datastore::caching::redis_pool::create_redis_pool_from_env;
use crate::datastore::id_list_manifest::{IdListManifest, IdListManifestEntry};
use crate::datastore::idlist_membership::{hash_company_id, DictClient};
use bb8_redis::redis::AsyncCommands;

#[tokio::test]
async fn forward_only_company_storage_queries_only_each_keys_visible_lists() {
    use crate::datastore::idlist_membership::ListSyncState;
    let Ok(pool) = create_redis_pool_from_env(5).await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let ns = Arc::new(IdListNamespaces::new(vec!["key-a".into(), "key-b".into()]));
    for key in ["key-a", "key-b"] {
        ns.company_ids.set_for_test(key, "company");
    }
    ns.manifests.store(Arc::new(HashMap::from([
        ("key-a".into(), manifest(&["shared", "only-a"])),
        ("key-b".into(), manifest(&["shared", "only-b"])),
    ])));
    let schema = IdListKeySchema::new(format!("forward-company-{}", uuid::Uuid::new_v4()));
    let hash = hash_company_id("company");
    let mut conn = pool.get().await.unwrap();
    let _: () = conn.set(schema.ready_key(&hash), 3).await.unwrap();
    for list in ["shared", "only-a", "only-b"] {
        let state = ListSyncState {
            version: "v".into(),
            cardinality: Some(1),
            ..Default::default()
        };
        let _: () = conn
            .hset_multiple(schema.state_key(&hash, list), &state.to_hash_fields())
            .await
            .unwrap();
        let _: usize = conn
            .sadd(schema.fwd_key(&hash, list, "v"), "user")
            .await
            .unwrap();
    }
    let companies = CompanyMembershipPool::new(
        ns.clone(),
        schema.clone(),
        pool.clone(),
        IdListMembershipConfig {
            enable_reverse_index: false,
            fetch_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        },
    );
    for (key, own) in [("key-a", "only-a"), ("key-b", "only-b")] {
        let result = companies
            .resolver(key.into())
            .resolve(vec!["user".into()])
            .await
            .unwrap();
        assert_eq!(result["user"], HashSet::from(["shared".into(), own.into()]));
    }
    assert_eq!(companies.fetchers.len(), 1);
    ns.manifests.rcu(|current| {
        let mut next = (**current).clone();
        next.insert("key-a".into(), manifest(&["shared"]));
        next
    });
    // A corrupt list outside the updated key scope must not affect that key.
    let _: () = conn
        .set(schema.fwd_key(&hash, "only-a", "v"), "wrong-type")
        .await
        .unwrap();
    assert_eq!(
        companies
            .resolver("key-a".into())
            .resolve(vec!["user".into()])
            .await
            .unwrap()["user"],
        HashSet::from(["shared".into()])
    );
}

fn manifest(names: &[&str]) -> IdListManifest {
    names
        .iter()
        .map(|name| {
            (
                name.to_string(),
                IdListManifestEntry {
                    name: name.to_string(),
                    size: 0,
                    url: "https://example.com".into(),
                    file_id: "file".into(),
                    creation_time: 1,
                },
            )
        })
        .collect()
}

#[tokio::test]
async fn company_storage_filters_each_key_and_shares_one_fetcher() {
    let Ok(pool) = create_redis_pool_from_env(5).await else {
        return;
    };
    let ns = Arc::new(IdListNamespaces::new(vec![
        "key-a".into(),
        "key-b".into(),
        "key-c".into(),
    ]));
    ns.company_ids.set_for_test("key-a", "company");
    ns.company_ids.set_for_test("key-b", "company");
    ns.company_ids.set_for_test("key-c", "other-company");
    ns.manifests.store(Arc::new(HashMap::from([
        ("key-a".into(), manifest(&["shared", "only-a"])),
        ("key-b".into(), manifest(&["shared", "only-b"])),
        ("key-c".into(), manifest(&["shared"])),
    ])));
    let schema = IdListKeySchema::new(format!("company-read-{}", uuid::Uuid::new_v4()));
    let hash = hash_company_id("company");
    let dict = DictClient::new(pool.clone(), schema.clone());
    let mut tokens = Vec::new();
    for name in ["shared", "only-a", "only-b"] {
        tokens.push(dict.get_or_allocate_token(&hash, name).await.unwrap());
    }
    let mut conn = pool.get().await.unwrap();
    let _: () = conn.set(schema.ready_key(&hash), 3).await.unwrap();
    let _: usize = conn
        .sadd(schema.member_key(&hash, "user"), tokens)
        .await
        .unwrap();
    drop(conn);
    let companies = CompanyMembershipPool::new(
        ns.clone(),
        schema,
        pool,
        IdListMembershipConfig {
            fetch_timeout: std::time::Duration::from_secs(1),
            ..Default::default()
        },
    );
    for (key, own) in [("key-a", "only-a"), ("key-b", "only-b")] {
        let result = companies
            .resolver(key.into())
            .resolve(vec!["user".into()])
            .await
            .unwrap();
        assert_eq!(
            result["user"],
            HashSet::from(["shared".to_string(), own.to_string()])
        );
    }
    assert_eq!(companies.fetchers.len(), 1);
    assert!(companies
        .resolver("key-c".into())
        .resolve(vec!["user".into()])
        .await
        .unwrap()["user"]
        .is_empty());
    // A scope change takes effect on the next request, without refreshing the dictionary.
    ns.manifests.rcu(|current| {
        let mut next = (**current).clone();
        next.insert("key-a".into(), manifest(&["shared"]));
        next.remove("key-b");
        next
    });
    let result = companies
        .resolver("key-a".into())
        .resolve(vec!["user".into()])
        .await
        .unwrap();
    assert_eq!(result["user"], HashSet::from(["shared".to_string()]));
    assert!(companies
        .resolver("key-b".into())
        .resolve(vec!["user".into()])
        .await
        .unwrap()["user"]
        .is_empty());
}

#[tokio::test]
async fn missing_company_and_empty_visibility_return_no_memberships_without_redis_reads() {
    let Ok(pool) = create_redis_pool_from_env(5).await else {
        return;
    };
    let ns = Arc::new(IdListNamespaces::new(vec!["key".into()]));
    ns.manifests
        .store(Arc::new(HashMap::from([("key".into(), manifest(&[]))])));
    let companies = CompanyMembershipPool::new(
        ns.clone(),
        IdListKeySchema::new("empty"),
        pool,
        Default::default(),
    );
    let resolver = companies.resolver("key".into());
    assert!(resolver.resolve(vec!["user".into()]).await.unwrap()["user"].is_empty());
    ns.company_ids.set_for_test("key", "company");
    assert!(resolver.resolve(vec!["user".into()]).await.unwrap()["user"].is_empty());
    assert!(companies.fetchers.is_empty());
}

#[tokio::test]
async fn endpoint_returns_available_visible_memberships_during_sync() {
    use rocket::http::{ContentType, Header, Status};
    use rocket::local::asynchronous::Client;
    let Ok(pool) = create_redis_pool_from_env(5).await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let ns = Arc::new(IdListNamespaces::new(vec!["key".into()]));
    ns.company_ids.set_for_test("key", "company");
    ns.manifests.store(Arc::new(HashMap::from([(
        "key".into(),
        manifest(&["allowed", "missing"]),
    )])));
    let schema = IdListKeySchema::new(format!("company-http-{}", uuid::Uuid::new_v4()));
    let hash = hash_company_id("company");
    let dict = DictClient::new(pool.clone(), schema.clone());
    let mut tokens = Vec::new();
    let mut conn = pool.get().await.unwrap();
    for name in ["allowed", "hidden"] {
        tokens.push(dict.get_or_allocate_token(&hash, name).await.unwrap());
    }
    let _: usize = conn
        .sadd(schema.member_key(&hash, "user"), tokens)
        .await
        .unwrap();
    for (ready, missing_dictionary) in [
        (None, false),
        (Some("0"), false),
        (Some("invalid"), false),
        (None, true),
    ] {
        if let Some(ready) = ready {
            let _: () = conn.set(schema.ready_key(&hash), ready).await.unwrap();
        } else {
            let _: usize = conn.del(schema.ready_key(&hash)).await.unwrap();
        }
        if missing_dictionary {
            let _: usize = conn.del(schema.dict_key(&hash)).await.unwrap();
        }
        let service = UserEnrichmentService::from_redis(
            ns.clone(),
            schema.clone(),
            pool.clone(),
            IdListMembershipConfig {
                fetch_timeout: std::time::Duration::from_secs(1),
                ..Default::default()
            },
            Default::default(),
        )
        .await
        .unwrap();
        let client = Client::tracked(
            rocket::build()
                .manage(service)
                .mount("/v1", endpoint_routes(true)),
        )
        .await
        .unwrap();
        let response = client
            .post("/v1/user_enrichment")
            .header(ContentType::JSON)
            .header(Header::new("statsig-api-key", "key"))
            .body(r#"{"id_list_lookups":["user","unknown"]}"#)
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Ok);
        let response: UserEnrichmentResponse =
            serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
        let expected = if missing_dictionary {
            vec![]
        } else {
            vec!["allowed"]
        };
        assert_eq!(response.id_lists["user"], expected);
        assert!(response.id_lists["unknown"].is_empty());
    }
}

#[tokio::test]
async fn forward_only_endpoint_returns_available_visible_memberships_during_sync() {
    use crate::datastore::idlist_membership::ListSyncState;
    use rocket::http::{ContentType, Header, Status};
    use rocket::local::asynchronous::Client;
    let Ok(pool) = create_redis_pool_from_env(5).await else {
        eprintln!("Skipping — Redis unavailable");
        return;
    };
    let ns = Arc::new(IdListNamespaces::new(vec!["key".into()]));
    ns.company_ids.set_for_test("key", "company");
    ns.manifests.store(Arc::new(HashMap::from([(
        "key".into(),
        manifest(&["allowed", "loading", "recovering", "missing"]),
    )])));
    let schema = IdListKeySchema::new(format!("company-http-{}", uuid::Uuid::new_v4()));
    let hash = hash_company_id("company");
    let dict = DictClient::new(pool.clone(), schema.clone());
    let mut tokens = Vec::new();
    let mut conn = pool.get().await.unwrap();
    for name in ["allowed", "hidden", "loading", "recovering"] {
        tokens.push(dict.get_or_allocate_token(&hash, name).await.unwrap());
        let state = ListSyncState {
            version: if name == "loading" { "" } else { "v" }.into(),
            loading_version: Some("v".into()),
            reconcile_required: true,
            cardinality: Some(10),
            ..Default::default()
        };
        let _: () = conn
            .hset_multiple(schema.state_key(&hash, name), &state.to_hash_fields())
            .await
            .unwrap();
        let _: usize = conn
            .sadd(schema.fwd_key(&hash, name, "v"), "user")
            .await
            .unwrap();
    }
    let _: usize = conn
        .sadd(schema.member_key(&hash, "user"), tokens)
        .await
        .unwrap();
    let _: () = conn
        .hset(
            schema.dict_key(&hash),
            crate::datastore::idlist_membership::REVERSE_INDEX_DISABLED,
            1u8,
        )
        .await
        .unwrap();
    for (ready, missing_dictionary) in [
        (None, false),
        (Some("0"), false),
        (Some("invalid"), false),
        (None, true),
    ] {
        if let Some(ready) = ready {
            let _: () = conn.set(schema.ready_key(&hash), ready).await.unwrap();
        } else {
            let _: usize = conn.del(schema.ready_key(&hash)).await.unwrap();
        }
        if missing_dictionary {
            let _: usize = conn.del(schema.dict_key(&hash)).await.unwrap();
        }
        let service = UserEnrichmentService::from_redis(
            ns.clone(),
            schema.clone(),
            pool.clone(),
            IdListMembershipConfig {
                enable_reverse_index: false,
                fetch_timeout: std::time::Duration::from_secs(1),
                ..Default::default()
            },
            Default::default(),
        )
        .await
        .unwrap();
        let client = Client::tracked(
            rocket::build()
                .manage(service)
                .mount("/v1", endpoint_routes(true)),
        )
        .await
        .unwrap();
        let response = client
            .post("/v1/user_enrichment")
            .header(ContentType::JSON)
            .header(Header::new("statsig-api-key", "key"))
            .body(r#"{"id_list_lookups":["user","unknown"]}"#)
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::Ok);
        let response: UserEnrichmentResponse =
            serde_json::from_str(&response.into_string().await.unwrap()).unwrap();
        let expected = vec!["allowed", "loading", "recovering"];
        assert_eq!(response.id_lists["user"], expected);
        assert!(response.id_lists["unknown"].is_empty());
    }
}
