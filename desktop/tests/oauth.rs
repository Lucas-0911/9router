use axum::{Json, Router, body::Bytes, http::HeaderMap, routing::post};
use nine_router_core::{Store, oauth::Service};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

fn query(values: &[(&str, &str)]) -> HashMap<String, String> {
    values
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[tokio::test]
async fn state_pkce_replay_refresh_and_cancellation() {
    let token_calls = Arc::new(Mutex::new(Vec::<HashMap<String, String>>::new()));
    let calls = token_calls.clone();
    let app = Router::new().route("/token", post(move |headers: HeaderMap, body: Bytes| {
        let calls = calls.clone();
        async move {
            let content_type = headers["content-type"].to_str().unwrap();
            let form: HashMap<String, String> = if content_type == "application/json" {
                serde_json::from_slice(&body).unwrap()
            } else {
                let encoded = std::str::from_utf8(&body).unwrap();
                reqwest::Url::parse(&format!("http://localhost/?{encoded}")).unwrap()
                    .query_pairs().map(|(k,v)| (k.into_owned(),v.into_owned())).collect()
            };
            calls.lock().unwrap().push(form.clone());
            if form.get("grant_type").is_some_and(|v| v == "refresh_token") {
                assert_eq!(content_type, "application/json");
                assert_eq!(form["refresh_token"], "initial-refresh");
                Json(json!({"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}))
            } else {
                assert_eq!(content_type, "application/x-www-form-urlencoded");
                assert_eq!(form["code"], "test-code");
                assert_eq!(form["client_id"], "test-client");
                assert_eq!(form["code_verifier"].len(), 64);
                Json(json!({"access_token":"issued-access","refresh_token":"initial-refresh","expires_in":3600}))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(
        Store::create(&temp.path().join("db.sqlite")).unwrap(),
    ));
    let service = Service::with_contract(
        store.clone(),
        reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap(),
        json!({"providers":{"codex":{"flowType":"authorization_code_pkce","config":{"clientId":"test-client","authorizeUrl":format!("http://{address}/authorize"),"tokenUrl":format!("http://{address}/token"),"scope":"openid offline_access"}}}}),
    );
    let params = query(&[("redirect_uri", "http://localhost:20128/callback")]);
    let auth = service
        .handle("codex", "authorize", "GET", &params, Value::Null)
        .await
        .unwrap();
    assert_ne!(auth["state"], auth["codeVerifier"]);
    assert_eq!(auth["codeChallenge"].as_str().unwrap().len(), 43);
    let body = json!({"code":"test-code","state":"wrong-state","redirectUri":auth["redirectUri"],"codeVerifier":auth["codeVerifier"]});
    assert!(
        service
            .handle("codex", "exchange", "POST", &HashMap::new(), body)
            .await
            .is_err()
    );
    assert!(token_calls.lock().unwrap().is_empty());
    let body = json!({"code":"test-code","state":auth["state"],"redirectUri":auth["redirectUri"],"codeVerifier":auth["codeVerifier"]});
    let result = service
        .handle("codex", "exchange", "POST", &HashMap::new(), body.clone())
        .await
        .unwrap();
    assert_eq!(result["success"], true);
    assert!(result.to_string().find("issued-access").is_none());
    assert!(
        service
            .handle("codex", "exchange", "POST", &HashMap::new(), body)
            .await
            .is_err()
    );
    let id = result["connection"]["id"].as_str().unwrap();
    let refreshed = service.refresh_connection(id).await.unwrap();
    assert_eq!(refreshed["accessToken"], "rotated-access");
    assert_eq!(refreshed["refreshToken"], "rotated-refresh");
    assert_eq!(token_calls.lock().unwrap().len(), 2);
    let auth = service
        .handle("codex", "authorize", "GET", &params, Value::Null)
        .await
        .unwrap();
    service
        .handle("codex", "stop-proxy", "GET", &HashMap::new(), Value::Null)
        .await
        .unwrap();
    let callback = query(&[
        ("code", "cancelled-code"),
        ("state", auth["state"].as_str().unwrap()),
    ]);
    assert!(service.callback(&callback).await.is_err());
    assert_eq!(store.lock().unwrap().connections(None).unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn native_callback_stores_once_and_status_contains_no_secrets() {
    let app=Router::new().route("/token",post(||async {Json(json!({"access_token":"secret-access","refresh_token":"secret-refresh","expires_in":60}))}));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(
        Store::create(&temp.path().join("db.sqlite")).unwrap(),
    ));
    let service = Service::with_contract(
        store.clone(),
        reqwest::Client::new(),
        json!({"providers":{"claude":{"flowType":"authorization_code_pkce","config":{"clientId":"client","authorizeUrl":format!("http://{address}/authorize"),"tokenUrl":format!("http://{address}/token"),"scopes":["user:inference"]}}}}),
    );
    let auth = service
        .handle(
            "claude",
            "authorize",
            "GET",
            &query(&[("redirect_uri", "http://localhost:20128/callback")]),
            Value::Null,
        )
        .await
        .unwrap();
    let callback = query(&[("code", "code"), ("state", auth["state"].as_str().unwrap())]);
    let result = service.callback(&callback).await.unwrap();
    assert_eq!(result["success"], true);
    let status = service
        .callback_status(auth["state"].as_str().unwrap())
        .unwrap();
    assert_eq!(status["status"], "done");
    assert!(!status.to_string().contains("secret-"));
    assert!(service.callback(&callback).await.is_err());
    assert_eq!(store.lock().unwrap().connections(None).unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn device_pending_slowdown_success_replay_and_server_owned_metadata() {
    let polls = Arc::new(Mutex::new(0));
    let calls = polls.clone();
    let app = Router::new()
        .route("/device", post(|body: Bytes| async move {
            let encoded = std::str::from_utf8(&body).unwrap();
            let url = reqwest::Url::parse(&format!("http://localhost/?{encoded}")).unwrap();
            let fields: HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(fields["client_id"], "mcode-public");
            assert_eq!(fields["code_challenge_method"], "S256");
            assert_eq!(fields["code_challenge"].len(), 43);
            Json(json!({"user_code":"USER-CODE","verification_uri":"https://example.com/verify","expired_in":600,"interval":5000,"_clientSecret":"registration-secret","_zcodePollToken":"poll-secret"}))
        }))
        .route("/token", post(move |body: Bytes| {
            let calls = calls.clone();
            async move {
                let encoded = std::str::from_utf8(&body).unwrap();
                let url = reqwest::Url::parse(&format!("http://localhost/?{encoded}")).unwrap();
                let fields: HashMap<_, _> = url.query_pairs().into_owned().collect();
                assert_eq!(fields["user_code"], "USER-CODE");
                assert!(!fields.contains_key("device_code"));
                assert_eq!(fields["code_verifier"].len(), 64);
                let mut count = calls.lock().unwrap();
                *count += 1;
                Json(match *count {
                    1 => json!({"status":"pending"}),
                    2 => json!({"status":"slow_down"}),
                    _ => json!({"access_token":"device-secret","refresh_token":"device-refresh","expires_in":3600}),
                })
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(
        Store::create(&temp.path().join("db.sqlite")).unwrap(),
    ));
    let service = Service::with_contract(
        store.clone(),
        reqwest::Client::new(),
        json!({"providers":{"minimax-code":{"flowType":"device_code","config":{"clientId":"mcode-public","scope":"agent.default","audience":"agent-backend","deviceCodeUrl":format!("http://{address}/device"),"tokenUrl":format!("http://{address}/token")}}}}),
    );
    let data = service
        .handle(
            "minimax-code",
            "device-code",
            "GET",
            &HashMap::new(),
            Value::Null,
        )
        .await
        .unwrap();
    assert_eq!(data["device_code"], "USER-CODE");
    assert_eq!(data["interval"], 5);
    assert!(data.get("_clientSecret").is_none());
    assert!(data.get("_zcodePollToken").is_none());
    let bad = json!({"deviceCode":data["device_code"],"codeVerifier":"wrong"});
    assert!(
        service
            .handle("minimax-code", "poll", "POST", &HashMap::new(), bad)
            .await
            .is_err()
    );
    assert_eq!(*polls.lock().unwrap(), 0);
    // Caller metadata cannot change the server-owned device-code variant.
    let body = json!({"deviceCode":data["device_code"],"codeVerifier":data["codeVerifier"],"extraData":{"_minimaxPollByUser":false}});
    for error in ["authorization_pending", "slow_down"] {
        let result = service
            .handle(
                "minimax-code",
                "poll",
                "POST",
                &HashMap::new(),
                body.clone(),
            )
            .await
            .unwrap();
        assert_eq!(result["error"], error);
        assert_eq!(result["pending"], true);
        assert!(store.lock().unwrap().connections(None).unwrap().is_empty());
    }
    let result = service
        .handle(
            "minimax-code",
            "poll",
            "POST",
            &HashMap::new(),
            body.clone(),
        )
        .await
        .unwrap();
    assert_eq!(result["success"], true);
    assert!(!result.to_string().contains("device-secret"));
    assert!(
        service
            .handle("minimax-code", "poll", "POST", &HashMap::new(), body)
            .await
            .is_err()
    );
    assert_eq!(*polls.lock().unwrap(), 3);
    assert_eq!(store.lock().unwrap().connections(None).unwrap().len(), 1);
    service.shutdown();
    server.abort();
}

#[tokio::test]
async fn authorize_rejects_insecure_endpoints_and_shutdown_invalidates_attempts() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(
        Store::create(&temp.path().join("db.sqlite")).unwrap(),
    ));
    let make = |endpoint: &str| {
        Service::with_contract(
            store.clone(),
            reqwest::Client::new(),
            json!({"providers":{"codex":{"flowType":"authorization_code_pkce","config":{"clientId":"test","authorizeUrl":endpoint,"tokenUrl":"http://localhost:1/token"}}}}),
        )
    };
    let service = make("http://example.com/authorize");
    assert!(
        service
            .handle("codex", "authorize", "GET", &HashMap::new(), Value::Null)
            .await
            .is_err()
    );
    let service = make("https://example.com/authorize");
    for redirect in [
        "https://example.com/callback",
        "http://localhost.evil.test/callback",
        "http://user:secret@localhost:1234/callback",
    ] {
        assert!(
            service
                .handle(
                    "codex",
                    "authorize",
                    "GET",
                    &query(&[("redirect_uri", redirect)]),
                    Value::Null
                )
                .await
                .is_err()
        );
    }
    let auth = service
        .handle("codex", "authorize", "GET", &HashMap::new(), Value::Null)
        .await
        .unwrap();
    service.shutdown();
    assert_eq!(
        service
            .callback_status(auth["state"].as_str().unwrap())
            .unwrap()["status"],
        "unknown"
    );
    let body = json!({"state":auth["state"],"codeVerifier":auth["codeVerifier"],"redirectUri":auth["redirectUri"],"code":"cancelled"});
    assert!(
        service
            .handle("codex", "exchange", "POST", &HashMap::new(), body)
            .await
            .is_err()
    );
    assert!(store.lock().unwrap().connections(None).unwrap().is_empty());
}
