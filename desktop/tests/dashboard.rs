use nine_router_core::{
    Store,
    http::{Gateway, router},
};
use serde_json::{Value, json};

#[tokio::test]
async fn original_dashboard_contract_preserves_account_and_model_workflows() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = Store::create(&directory.path().join("db.sqlite")).unwrap();
    store.save_settings(&json!({"password":"private-hash","oidcClientSecret":"private-oidc","enableTranslator":true})).unwrap();
    let account = store.save_connection(&json!({"provider":"muse-web","authType":"cookie","name":"Muse Web","apiKey":"hatch_sess=private-session","providerSpecificData":{"authMethod":"web-login"}})).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = router(
        Gateway::new(
            store,
            "dashboard-admin-012345678901234567890123456789".into(),
        )
        .unwrap(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::new();
    let get = |path: &str| {
        client
            .get(format!("{base}{path}"))
            .bearer_auth("dashboard-admin-012345678901234567890123456789")
    };
    let send = |method, path: &str, body: Value| {
        client
            .request(method, format!("{base}{path}"))
            .bearer_auth("dashboard-admin-012345678901234567890123456789")
            .json(&body)
    };
    let settings: Value = get("/api/settings")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        settings.get("password").is_none(),
        "original settings page must never receive the password hash"
    );
    assert!(settings.get("oidcClientSecret").is_none());
    let accounts: Value = get("/api/providers")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(accounts["connections"][0]["authType"], "cookie");
    assert!(accounts.to_string().find("private-session").is_none());
    let single: Value = get(&format!(
        "/api/providers/{}",
        account["id"].as_str().unwrap()
    ))
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(single["connection"]["id"], account["id"]);
    let methods: Value = get("/api/providers/web-login")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        methods["providers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == "muse-web")
    );
    let overrides: Value = get("/api/providers/muse-web/overrides")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(overrides["headers"], json!({}));
    assert!(
        send(
            reqwest::Method::PUT,
            "/api/providers/muse-web/overrides",
            json!({"headers":{"Authorization":"forbidden"}})
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_client_error()
    );
    let pools: Value = get("/api/proxy-pools?isActive=true")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(pools["proxyPools"], json!([]));
    let pool: Value = send(
        reqwest::Method::POST,
        "/api/proxy-pools",
        json!({"name":"Fixture proxy","proxyUrl":"http://127.0.0.1:9"}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(pool["proxyPool"]["name"], "Fixture proxy");
    let tunnel: Value = get("/api/tunnel/status")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tunnel["tunnel"]["running"], false);
    let key: Value = send(
        reqwest::Method::POST,
        "/api/keys",
        json!({"name":"Test client"}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert!(key["key"].as_str().unwrap().starts_with("sk-"));
    let keys: Value = get("/api/keys").send().await.unwrap().json().await.unwrap();
    assert_eq!(keys["keys"][0]["id"], key["id"]);
    let combo: Value = send(
        reqwest::Method::POST,
        "/api/combos",
        json!({"name":"test-fallback","models":["openai/gpt-4o"]}),
    )
    .send()
    .await
    .unwrap()
    .json()
    .await
    .unwrap();
    assert_eq!(combo["name"], "test-fallback");
    let custom = json!({"providerAlias":"oai","id":"fixture-model","name":"Fixture","type":"llm","caps":{"vision":true}});
    assert!(
        send(reqwest::Method::POST, "/api/models/custom", custom)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let custom: Value = get("/api/models/custom")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(custom["models"][0]["id"], "fixture-model");
    assert!(
        send(
            reqwest::Method::POST,
            "/api/models/disabled",
            json!({"providerAlias":"oai","ids":["fixture-model"]})
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_success()
    );
    let disabled: Value = get("/api/models/disabled?providerAlias=oai")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(disabled["ids"], json!(["fixture-model"]));
    assert!(
        get("/api/models/disabled?providerAlias=oai&id=fixture-model")
            .build()
            .is_ok()
    );
    assert!(
        send(
            reqwest::Method::DELETE,
            "/api/models/disabled?providerAlias=oai&id=fixture-model",
            json!({})
        )
        .send()
        .await
        .unwrap()
        .status()
        .is_success()
    );
    let disabled: Value = get("/api/models/disabled?providerAlias=oai")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(disabled["ids"], json!([]));
    server.abort();
}
