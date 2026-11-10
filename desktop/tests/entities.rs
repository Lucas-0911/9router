use nine_router_core::{Catalog, Store};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

fn database() -> (tempfile::TempDir, std::path::PathBuf, Store) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.sqlite");
    let store = Store::create(&path).unwrap();
    (directory, path, store)
}

#[test]
fn connection_patches_preserve_credentials_and_order_inactive_and_null_priorities() {
    let (_directory, path, mut store) = database();
    for (id, priority, created) in [
        ("last", Value::Null, "2020-01-01"),
        ("second", json!(1), "2021-01-01"),
        ("first", json!(1), "2020-01-01"),
    ] {
        store
            .save_connection(
                &json!({"id": id, "provider": "openai", "authType": "apikey",
            "priority": priority, "createdAt": created, "isActive": false,
            "apiKey": "fixture-secret", "providerSpecificData": {"nested": true}}),
            )
            .unwrap();
    }
    let updated = store
        .save_connection(&json!({"id": "first", "name": "renamed"}))
        .unwrap();
    assert_eq!(updated["apiKey"], "fixture-secret");
    assert_eq!(updated["providerSpecificData"], json!({"nested": true}));
    assert_eq!(updated["isActive"], false);
    let rows = store.connections(Some("openai")).unwrap();
    assert_eq!(
        rows.iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["first", "second", "last"]
    );
    assert!(store.connections(Some("anthropic")).unwrap().is_empty());
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE providerConnections SET data=?1 WHERE id='first'",
            ["{corrupt"],
        )
        .unwrap();
    assert!(
        store
            .save_connection(&json!({"id": "first", "name": "lost"}))
            .is_err()
    );
    let name: String = Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT name FROM providerConnections WHERE id='first'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(name, "renamed");
    assert!(store.connections(None).is_err());
    assert!(store.delete_connection("second").unwrap());
    assert!(!store.delete_connection("second").unwrap());
}

#[test]
fn key_restrictions_survive_patch_and_bad_access_cannot_broaden_them() {
    let (_directory, path, mut store) = database();
    let key = store.save_api_key(&json!({"key": "sk-fixture", "name": "restricted", "machineId": "fixture",
        "isActive": false, "access": {"restricted": true, "allow": [" openai/gpt-4o ", "OPENAI/gpt-4o", "fast"]}})).unwrap();
    let id = key["id"].as_str().unwrap();
    assert_eq!(
        key["access"],
        json!({"restricted": true, "allow": ["openai/gpt-4o", "fast"]})
    );
    let patched = store
        .save_api_key(&json!({"id": id, "name": "renamed"}))
        .unwrap();
    assert_eq!(patched["access"], key["access"]);
    assert_eq!(patched["isActive"], false);
    assert_eq!(patched["key"], "sk-fixture");
    for access in [
        json!({"restricted": "false"}),
        json!({"restricted": true, "allow": [4]}),
        json!({"restricted": true, "allow": ["x".repeat(257)]}),
        json!({"restricted": false, "unexpected": true}),
    ] {
        assert!(
            store
                .save_api_key(&json!({"id": id, "access": access}))
                .is_err()
        );
    }
    assert_eq!(store.api_keys().unwrap()[0]["access"], key["access"]);
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE apiKeys SET accessAllow='{broken' WHERE id=?1", [id])
        .unwrap();
    assert!(store.api_keys().is_err());
    assert!(
        store
            .save_api_key(&json!({"id": id, "name": "lost"}))
            .is_err()
    );
    assert!(store.delete_api_key(id).unwrap());
    assert!(!store.delete_api_key(id).unwrap());
}

#[test]
fn custom_nodes_respect_reserved_prefixes_and_combo_precedes_alias() {
    let (_directory, _path, mut store) = database();
    let catalog = Catalog::load().unwrap();
    for (id, kind, prefix) in [
        ("reserved-cf", "openai-compatible", "cf"),
        ("reserved-hf", "openai-compatible", "hf"),
        ("reserved-xmtp", "openai-compatible", "xmtp"),
        ("anthropic-node", "anthropic-compatible", "mine"),
        ("openai-node", "openai-compatible", "mine"),
        ("embedding-node", "custom-embedding", "embed"),
    ] {
        store.save_node(&json!({"id": id, "type": kind, "name": id, "prefix": prefix, "baseUrl": "http://localhost:9999"})).unwrap();
    }
    for (input, provider, model) in [
        ("cf/model", Some("cloudflare-ai"), "model"),
        ("hf/a/b", Some("huggingface"), "a/b"),
        ("xmtp/mimo", Some("xiaomi-tokenplan"), "mimo"),
        ("mine/nested/model", Some("openai-node"), "nested/model"),
        ("embed/emb", Some("embedding-node"), "emb"),
        ("gpt-6.1", Some("codex"), "gpt-6.1"),
    ] {
        let resolved = store.resolve_app_model(&catalog, input).unwrap();
        assert_eq!(resolved.provider.as_deref(), provider, "{input}");
        assert_eq!(resolved.model.as_deref(), Some(model), "{input}");
    }
    store
        .save_aliases(
            json!({"fast": "anthropic/claude-sonnet-4-6", "empty": "openai/gpt-4o"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
    let combo = store
        .save_combo(
            &json!({"name": "fast", "models": ["openai/gpt-4o", "anthropic/claude-sonnet-4-6"]}),
        )
        .unwrap();
    let empty = store
        .save_combo(&json!({"name": "empty", "models": []}))
        .unwrap();
    assert_eq!(
        store.resolve_app_model(&catalog, "fast").unwrap().provider,
        None
    );
    assert_eq!(
        store.resolve_app_model(&catalog, "empty").unwrap().provider,
        None
    );
    assert_eq!(
        store.combo_models("fast").unwrap(),
        Some(vec![
            "openai/gpt-4o".into(),
            "anthropic/claude-sonnet-4-6".into()
        ])
    );
    assert_eq!(store.combo_models("empty").unwrap(), None);
    assert_eq!(store.combo_models("openai/fast").unwrap(), None);
    assert!(store.delete_combo(combo["id"].as_str().unwrap()).unwrap());
    assert!(store.delete_combo(empty["id"].as_str().unwrap()).unwrap());
    assert_eq!(
        store
            .resolve_app_model(&catalog, "fast")
            .unwrap()
            .provider
            .as_deref(),
        Some("anthropic")
    );
}

#[test]
fn node_delete_removes_only_its_literal_custom_model_prefix() {
    let (_directory, _path, mut store) = database();
    store.save_node(&json!({"id": "wild_%", "type": "custom-embedding", "prefix": "mine", "future": {"supported": true}})).unwrap();
    let patched = store
        .save_node(&json!({"id": "wild_%", "name": "renamed"}))
        .unwrap();
    assert_eq!(patched["future"], json!({"supported": true}));
    store
        .put_batch(&[
            ("customModels", "wild_%|m|llm", &json!({"id": "m"})),
            ("customModels", "wild_OTHER|m|llm", &json!({"id": "keep"})),
        ])
        .unwrap();
    assert!(store.delete_node("wild_%").unwrap());
    assert_eq!(store.get("customModels", "wild_%|m|llm").unwrap(), None);
    assert_eq!(
        store.get("customModels", "wild_OTHER|m|llm").unwrap(),
        Some(json!({"id": "keep"}))
    );
    assert!(store.nodes().unwrap().is_empty());
}

#[test]
fn settings_merge_defaults_and_alias_replacement_is_atomic() {
    let (_directory, path, mut store) = database();
    assert_eq!(store.settings().unwrap()["requireApiKey"], true);
    store
        .save_settings(&json!({"future": {"keep": true}, "requireApiKey": false,
        "capacityAdapter": {"vision": {"models": ["oc/mimo-v2.5-free"]}}}))
        .unwrap();
    store
        .save_settings(&json!({"customPrompt": "fixture prompt"}))
        .unwrap();
    let settings = store.settings().unwrap();
    assert_eq!(settings["requireApiKey"], false);
    assert_eq!(settings["future"], json!({"keep": true}));
    assert_eq!(
        settings["capacityAdapter"]["vision"]["models"][0],
        "oc/mimo-v2.6-flash-free"
    );
    store
        .save_aliases(
            json!({"first": "openai/gpt-4o", "removed": "anthropic/claude"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
    store
        .save_aliases(
            json!({"first": {"provider": "openai", "model": "gpt-4o"}})
                .as_object()
                .unwrap(),
        )
        .unwrap();
    assert_eq!(store.aliases().unwrap().len(), 1);
    assert!(
        store
            .save_aliases(json!({"bad": 5}).as_object().unwrap())
            .is_err()
    );
    assert_eq!(store.aliases().unwrap().len(), 1);
    Connection::open(&path)
        .unwrap()
        .execute("UPDATE settings SET data=?1 WHERE id=1", params!["{broken"])
        .unwrap();
    assert!(
        store
            .save_settings(&json!({"requireApiKey": false}))
            .is_err()
    );
    assert!(store.settings().is_err());
}

#[test]
fn invalid_types_and_overlong_identifiers_do_not_create_rows() {
    let (_directory, _path, mut store) = database();
    for input in [
        json!([]),
        json!({"provider": false}),
        json!({"provider": "x".repeat(257)}),
        json!({"provider": "openai", "isActive": 0}),
        json!({"provider": "openai", "priority": 1.2}),
    ] {
        assert!(store.save_connection(&input).is_err());
    }
    assert!(store.connections(None).unwrap().is_empty());
    assert!(store.save_node(&json!({"type": 1})).is_err());
    assert!(
        store
            .save_combo(&json!({"name": "bad", "models": [1]}))
            .is_err()
    );
    assert!(store.save_api_key(&json!({"name": "no-key"})).is_err());
    assert!(store.save_settings(&json!([])).is_err());
}

#[test]
fn sql_columns_are_authoritative_and_reactivation_clears_stale_health() {
    let (_directory, path, mut store) = database();
    store
        .save_connection(
            &json!({"id": "fixture", "provider": "openai", "isActive": false, "priority": 9}),
        )
        .unwrap();
    Connection::open(&path)
        .unwrap()
        .execute(
            "UPDATE providerConnections SET data=?1 WHERE id='fixture'",
            [
                json!({"id": "wrong", "provider": "wrong", "isActive": true, "priority": 1,
            "apiKey": "fixture-secret", "modelLock_gpt-4o": "tomorrow", "errorCode": 429,
            "lastError": "old error", "rateLimitedUntil": "tomorrow", "backoffLevel": 4})
                .to_string(),
            ],
        )
        .unwrap();
    let row = store.connections(None).unwrap().remove(0);
    assert_eq!(row["id"], "fixture");
    assert_eq!(row["provider"], "openai");
    assert_eq!(row["isActive"], false);
    assert_eq!(row["priority"], 9);
    let reset = store
        .save_connection(&json!({"id": "fixture", "testStatus": "active"}))
        .unwrap();
    for field in [
        "lastError",
        "lastErrorAt",
        "errorCode",
        "rateLimitedUntil",
        "modelLock_gpt-4o",
    ] {
        assert_eq!(reset[field], Value::Null, "{field}");
    }
    assert_eq!(reset["backoffLevel"], 0);
    assert_eq!(reset["apiKey"], "fixture-secret");
}

#[test]
fn corrupt_alias_json_blocks_whole_map_replacement() {
    let (_directory, path, mut store) = database();
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO kv(scope,key,value) VALUES('modelAliases','broken','{broken')",
            [],
        )
        .unwrap();
    assert!(store.aliases().is_err());
    assert!(
        store
            .save_aliases(json!({"new": "openai/gpt-4o"}).as_object().unwrap())
            .is_err()
    );
    let original: String = Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT value FROM kv WHERE scope='modelAliases' AND key='broken'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(original, "{broken");
}
