use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use futures_util::{Stream, stream};
use nine_router_core::{
    Store,
    http::{Gateway, router},
};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

#[tokio::test]
async fn web_oauth_accounts_dispatch_without_api_keys_and_google_envelopes_are_unwrapped() {
    let fixture=Router::new().route("/codex/responses",post(|headers:HeaderMap,axum::Json(body):axum::Json<Value>|async move {
        assert_eq!(headers["authorization"],"Bearer codex-login-token");
        assert_eq!(headers["chatgpt-account-id"],"fixture-account");
        assert_eq!(body["stream"],true);
        assert_eq!(body["store"],false);
        assert!(body["input"].is_array());
        let completed=json!({"type":"response.completed","response":{"id":"resp_fixture","status":"completed","model":"gpt-5","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"OAuth accepted","annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":2}}});
        ([("content-type","text/event-stream")],format!("event: response.completed\r\ndata: {completed}\r\n\r\n"))
    })).route("/v1internal:generateContent",post(|headers:HeaderMap,axum::Json(body):axum::Json<Value>|async move {
        assert_eq!(headers["authorization"],"Bearer google-login-token");
        assert_eq!(body["project"],"fixture-project");
        assert_eq!(body["model"],"gemini-2.5-flash");
        assert!(body["request"]["contents"].is_array());
        axum::Json(json!({"response":{"candidates":[{"content":{"role":"model","parts":[{"text":"Web login works"}]},"finishReason":"STOP"}]}}))
    })).route("/v1internal:streamGenerateContent",post(|headers:HeaderMap|async move {
        assert_eq!(headers["authorization"],"Bearer google-login-token");
        let event=format!("data: {}\r\n\r\n",json!({"response":{"candidates":[{"content":{"parts":[{"text":"Xin chào"}]}}]}}));
        ([("content-type","text/event-stream")],Body::from_stream(stream::iter(event.as_bytes().chunks(2).map(|b|Ok::<_,Infallible>(Bytes::copy_from_slice(b))).collect::<Vec<_>>())))
    })).route("/v1/messages",post(|headers:HeaderMap|async move {
        assert_eq!(headers["authorization"],"Bearer claude-login-token");assert!(!headers.contains_key("x-api-key"));
        axum::Json(json!({"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":"OAuth accepted"}],"model":"claude-sonnet-5","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":2}}))
    }));
    let (upstream, fixture_task) = serve(fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    store.save_connection(&json!({"provider":"gemini-cli","authType":"oauth","accessToken":"google-login-token","projectId":"fixture-project","providerSpecificData":{"baseUrl":format!("{upstream}/v1internal")}})).unwrap();
    store.save_connection(&json!({"provider":"claude","authType":"oauth","accessToken":"claude-login-token","providerSpecificData":{"baseUrl":format!("{upstream}/v1")}})).unwrap();
    store.save_connection(&json!({"provider":"codex","authType":"oauth","accessToken":"codex-login-token","providerSpecificData":{"baseUrl":format!("{upstream}/codex/responses"),"chatgptAccountId":"fixture-account"}})).unwrap();
    let (base, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let client = reqwest::Client::new();
    for model in ["gc/gemini-2.5-flash", "cc/claude-sonnet-5", "codex/gpt-5"] {
        let response = client
            .post(format!("{base}/v1/chat/completions"))
            .bearer_auth(ADMIN)
            .json(&json!({"model":model,"messages":[{"role":"user","content":"Hello"}]}))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body: Value = response.json().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert!(body["choices"][0]["message"]["content"].is_string());
    }
    let response = client
        .post(format!(
            "{base}/v1beta/models/gc/gemini-2.5-flash:streamGenerateContent"
        ))
        .bearer_auth(ADMIN)
        .json(&json!({"contents":[{"role":"user","parts":[{"text":"Hello"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let text = response.text().await.unwrap();
    assert!(text.contains("Xin chào"));
    assert!(!text.contains("\"response\""));
    for stream in [false, true] {
        let response = client
            .post(format!("{base}/v1/responses"))
            .bearer_auth(ADMIN)
            .json(&json!({"model":"codex/gpt-5","input":"Hello","stream":stream}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        if stream {
            assert!(
                response
                    .text()
                    .await
                    .unwrap()
                    .contains("event: response.completed")
            );
        } else {
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["status"], "completed");
            assert_eq!(body["output"][0]["content"][0]["text"], "OAuth accepted");
        }
    }
    server.abort();
    fixture_task.abort();
}

const ADMIN: &str = "test-admin-only-012345678901234567890123456789";

async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, handle)
}

fn store_at(directory: &tempfile::TempDir) -> Store {
    Store::create(&directory.path().join("data.sqlite")).unwrap()
}
fn connection(store: &mut Store, key: &str, priority: i64, url: &str) -> Value {
    store.save_connection(&json!({"provider":"openai","authType":"apikey","name":format!("Fixture {priority}"),"apiKey":key,"priority":priority,"isActive":true,
        "providerSpecificData":{"baseUrl":format!("{url}/v1")}})).unwrap()
}

#[tokio::test]
async fn auth_management_redaction_and_restricted_alias_combo_resolution() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    connection(&mut store, "provider-private-key", 1, "http://127.0.0.1:9");
    store
        .save_aliases(&serde_json::from_value(json!({"allowed-alias":"openai/gpt-4o"})).unwrap())
        .unwrap();
    store
        .save_combo(&json!({"name":"Team","models":["openai/gpt-4o"]}))
        .unwrap();
    store.save_api_key(&json!({"name":"limited","key":"limited-key","access":{"restricted":true,"allow":["allowed-alias","Team"]}})).unwrap();
    let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{url}/api/settings"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        client
            .get(format!("{url}/api/settings"))
            .bearer_auth("limited-key")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let connections = client
        .get(format!("{url}/api/providers"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!connections.contains("provider-private-key"));
    let models: Value = client
        .get(format!("{url}/v1/models"))
        .bearer_auth("limited-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<_> = models["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert!(ids.contains(&"openai/gpt-4o"));
    assert!(ids.contains(&"Team"));
    assert!(!ids.contains(&"openai/gpt-4.1"));
    for model in ["openai/gpt-4.1", "team"] {
        let response = client
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth("limited-key")
            .json(&json!({"model":model,"messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }
    let allowed = client
        .post(format!("{url}/v1/chat/completions"))
        .header("x-goog-api-key", "limited-key")
        .json(&json!({"model":"allowed-alias","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        allowed.status(),
        502,
        "allowed alias reaches the isolated failing upstream"
    );
    server.abort();
}

#[tokio::test]
async fn retry_accounts_preserves_request_fields_and_never_forwards_client_auth() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = attempts.clone();
    let fixture = Router::new().route("/v1/chat/completions",post(move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
        let seen = seen.clone(); async move {
            seen.fetch_add(1,Ordering::SeqCst);
            assert_eq!(body["model"],"gpt-4o"); assert_eq!(body["temperature"],0.2);
            assert_eq!(body["messages"][0]["content"][0]["text"],"hello");
            assert!(!headers.contains_key("x-forwarded-for"));
            match headers["authorization"].to_str().unwrap() {
                "Bearer bad-provider-key" => (StatusCode::TOO_MANY_REQUESTS, axum::Json(json!({"error":"secret must not escape"}))).into_response(),
                "Bearer good-provider-key" => axum::Json(json!({"id":"native-fixture","choices":[{"message":{"role":"assistant","content":"ok","tool_calls":[{"id":"tool-1"}]}}],"usage":{"prompt_tokens":3,"completion_tokens":1},"vendor_extension":{"retained":true}})).into_response(),
                _ => panic!("wrong upstream credential"),
            }
        }
    }));
    let (upstream, fixture_task) = serve(fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    connection(&mut store, "bad-provider-key", 1, &upstream);
    connection(&mut store, "good-provider-key", 2, &upstream);
    let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let client = reqwest::Client::new();
    let body = json!({"model":"openai/gpt-4o","temperature":0.2,"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}],"stream":false});
    for _ in 0..2 {
        let response = client
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth(ADMIN)
            .header("x-forwarded-for", "attacker-controlled")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let value: Value = response.json().await.unwrap();
        assert_eq!(value["vendor_extension"]["retained"], true);
        assert_eq!(
            value["choices"][0]["message"]["tool_calls"][0]["id"],
            "tool-1"
        );
    }
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        3,
        "cooled-down first account should be skipped on second call"
    );
    server.abort();
    fixture_task.abort();
}

struct PendingBody {
    first: bool,
    dropped: Arc<tokio::sync::Notify>,
}
impl Stream for PendingBody {
    type Item = Result<Bytes, Infallible>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.first {
            self.first = false;
            Poll::Ready(Some(Ok(Bytes::from_static(
                b"data: {\"delta\":\"first\"}\n\n",
            ))))
        } else {
            Poll::Pending
        }
    }
}
impl Drop for PendingBody {
    fn drop(&mut self) {
        self.dropped.notify_one();
    }
}

#[tokio::test]
async fn streaming_reaches_client_before_completion_and_disconnect_cancels_upstream() {
    let dropped = Arc::new(tokio::sync::Notify::new());
    let fixture_dropped = dropped.clone();
    let fixture = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let dropped = fixture_dropped.clone();
            async move {
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(PendingBody {
                        first: true,
                        dropped,
                    }),
                )
            }
        }),
    );
    let (upstream, fixture_task) = serve(fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    connection(&mut store, "stream-provider-key", 1, &upstream);
    let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let mut response=reqwest::Client::new().post(format!("{url}/v1/chat/completions")).bearer_auth(ADMIN)
        .json(&json!({"model":"openai/gpt-4o","stream":true,"messages":[{"role":"user","content":"test"}]})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), response.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        "data: {\"delta\":\"first\"}\n\n"
    );
    drop(response);
    tokio::time::timeout(Duration::from_secs(3), dropped.notified())
        .await
        .expect("upstream response body must be dropped after downstream disconnect");
    server.abort();
    fixture_task.abort();
}

#[tokio::test]
async fn sse_is_byte_preserving_and_unsupported_adapters_do_not_dispatch() {
    const SSE: &str = "event: thinking\ndata: {\"signature\":\"opaque\"}\n\ndata: {\"tool_calls\":[{\"arguments\":\"{\\\"x\\\":1}\"}]}\n\ndata: [DONE]\n\n";
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let fixture = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                (
                    [("content-type", "text/event-stream")],
                    Body::from_stream(stream::iter(
                        SSE.as_bytes()
                            .chunks(3)
                            .map(|part| Ok::<_, Infallible>(Bytes::copy_from_slice(part))),
                    )),
                )
            }
        }),
    );
    let (upstream, fixture_task) = serve(fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    connection(&mut store, "fixture-key", 1, &upstream);
    store.save_connection(&json!({"provider":"codex","authType":"oauth","name":"not-ported","accessToken":"must-never-send","providerSpecificData":{"baseUrl":format!("{upstream}/v1")}})).unwrap();
    let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth(ADMIN)
        .json(&json!({"model":"openai/gpt-4o","messages":[],"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), SSE);
    let response = client
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth(ADMIN)
        .json(&json!({"model":"codex/gpt-5","messages":[],"stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 501);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    server.abort();
    fixture_task.abort();
}

#[tokio::test]
async fn combo_advances_after_model_rejection_and_global_proxy_never_goes_direct() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let fixture=Router::new().route("/v1/chat/completions",post(move |axum::Json(body): axum::Json<Value>| {let seen=seen.clone(); async move {
        seen.fetch_add(1,Ordering::SeqCst);
        if body["model"]=="retired" { (StatusCode::NOT_FOUND,axum::Json(json!({"error":"retired"}))).into_response() }
        else { axum::Json(json!({"choices":[{"message":{"role":"assistant","content":"fallback worked"}}]})).into_response() }
    }}));
    let (upstream, fixture_task) = serve(fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    connection(&mut store, "first-key", 1, &upstream);
    connection(&mut store, "second-key", 2, &upstream);
    store
        .save_combo(&json!({"name":"fallback","models":["openai/retired","openai/current"]}))
        .unwrap();
    let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth(ADMIN)
        .json(&json!({"model":"fallback","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "skip other accounts for the rejected model"
    );
    assert_eq!(
        client
            .put(format!("{url}/api/settings"))
            .bearer_auth(ADMIN)
            .json(&json!({"outboundProxyEnabled":true,"outboundProxyUrl":"http://127.0.0.1:9"}))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    let response = client
        .post(format!("{url}/v1/chat/completions"))
        .bearer_auth(ADMIN)
        .json(&json!({"model":"fallback","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 501);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.abort();
    fixture_task.abort();
}

#[tokio::test]
async fn concurrent_single_alias_updates_do_not_replace_other_entries() {
    let temp = tempfile::tempdir().unwrap();
    let store = store_at(&temp);
    let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let client = reqwest::Client::new();
    let mut requests = Vec::new();
    for i in 0..30 {
        let client = client.clone();
        let url = url.clone();
        requests.push(tokio::spawn(async move {
            client
                .put(format!("{url}/api/models/alias"))
                .bearer_auth(ADMIN)
                .json(&json!({"alias":format!("alias-{i}"),"model":"openai/gpt-4o"}))
                .send()
                .await
                .unwrap()
                .status()
        }));
    }
    for request in requests {
        assert_eq!(request.await.unwrap(), 200);
    }
    let result: Value = client
        .get(format!("{url}/api/models/alias"))
        .bearer_auth(ADMIN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(result["aliases"].as_object().unwrap().len(), 30);
    for target in [json!(""), json!("no-provider"), json!({})] {
        assert_eq!(
            client
                .put(format!("{url}/api/models/alias"))
                .bearer_auth(ADMIN)
                .json(&json!({"alias":"invalid","model":target}))
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
    }
    server.abort();
}

#[tokio::test]
async fn tauri_preflight_and_auth_errors_have_cors_but_other_origins_do_not() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    store
        .save_api_key(&json!({"key":"query-key","name":"Gemini client"}))
        .unwrap();
    let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let client = reqwest::Client::new();
    let response = client
        .request(reqwest::Method::OPTIONS, format!("{url}/api/providers"))
        .header("origin", "tauri://localhost")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "tauri://localhost"
    );
    let response = client
        .get(format!("{url}/api/providers"))
        .header("origin", "tauri://localhost")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.headers()["access-control-allow-origin"],
        "tauri://localhost"
    );
    let response = client
        .get(format!("{url}/api/providers"))
        .header("origin", "https://untrusted.example")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(
        !response
            .headers()
            .contains_key("access-control-allow-origin")
    );
    assert_eq!(
        client
            .get(format!("{url}/v1/models?key=query-key"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{url}/api/settings?key=query-key"))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    server.abort();
}

#[tokio::test]
async fn native_claude_gemini_and_responses_dispatch_to_correct_endpoints_and_headers() {
    let fixture=Router::new()
        .route("/v1/messages",post(|headers:HeaderMap,axum::Json(body):axum::Json<Value>|async move {
            assert_eq!(headers["x-api-key"],"anthropic-secret");assert_eq!(headers["anthropic-version"],"2023-06-01");
            assert_eq!(body["model"],"claude-test");
            axum::Json(json!({"id":"msg-fixture","type":"message","role":"assistant","model":"claude-test","content":[{"type":"text","text":"hello"}],"stop_reason":"end_turn","stop_sequence":null,"usage":{"input_tokens":2,"output_tokens":1}}))
        }))
        .route("/models/gemini-test:generateContent",post(|headers:HeaderMap,axum::Json(body):axum::Json<Value>|async move {
            assert_eq!(headers["x-goog-api-key"],"gemini-secret");assert!(body.get("model").is_none());assert!(body.get("stream").is_none());
            axum::Json(json!({"candidates":[{"content":{"role":"model","parts":[{"text":"gemini"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1,"totalTokenCount":3}}))
        }))
        .route("/v1/responses",post(|headers:HeaderMap,axum::Json(body):axum::Json<Value>|async move {
            assert_eq!(headers["authorization"],"Bearer openai-secret");assert_eq!(body["input"],"hello");assert_eq!(body["model"],"gpt-test");
            axum::Json(json!({"id":"resp-fixture","object":"response","output":[{"type":"message","content":[{"type":"output_text","text":"responses"}]}],"extension":"preserved"}))
        }));
    let (upstream, fixture_task) = serve(fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    for (provider, key, base) in [
        ("anthropic", "anthropic-secret", "/v1"),
        ("gemini", "gemini-secret", "/models"),
        ("openai", "openai-secret", "/v1"),
    ] {
        store.save_connection(&json!({"provider":provider,"authType":"apikey","name":provider,"apiKey":key,"providerSpecificData":{"baseUrl":format!("{upstream}{base}")}})).unwrap();
    }
    let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let client = reqwest::Client::new();
    for (path, body) in [
        (
            "/v1/messages",
            json!({"model":"anthropic/claude-test","max_tokens":10,"messages":[{"role":"user","content":"hello"}]}),
        ),
        (
            "/v1beta/models/gemini/gemini-test:generateContent",
            json!({"contents":[{"role":"user","parts":[{"text":"hello"}]}]}),
        ),
        (
            "/v1/responses",
            json!({"model":"openai/gpt-test","input":"hello"}),
        ),
        (
            "/v1/chat/completions",
            json!({"model":"anthropic/claude-test","max_tokens":10,"messages":[{"role":"user","content":"hello"}],"stream":false}),
        ),
    ] {
        let response = client
            .post(format!("{url}{path}"))
            .bearer_auth(ADMIN)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "native or converted protocol path {path}"
        );
    }
    server.abort();
    fixture_task.abort();
}
