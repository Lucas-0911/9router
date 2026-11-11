use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use futures_util::{Stream, StreamExt, stream};
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

#[tokio::test]
async fn codex_chat_streams_before_completion_and_preserves_parallel_tools_reasoning_and_usage() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let fixture = Router::new().route("/responses", post(move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
        let gate = gate.clone();
        async move {
            assert_eq!(headers["authorization"], "Bearer browser-login-token");
            assert_eq!(body["stream"], true);
            assert!(body.get("stream_options").is_none());
            let events = [
                json!({"type":"response.created","response":{"id":"resp_stream","model":"gpt-5","created_at":123}}),
                json!({"type":"response.reasoning_summary_text.delta","delta":"Thinking"}),
                json!({"type":"response.output_text.delta","delta":"Xin chào 🌏"}),
                json!({"type":"response.output_item.added","item":{"id":"fc_first","type":"function_call","call_id":"call_first","name":"first","arguments":""}}),
                json!({"type":"response.output_item.added","item":{"id":"fc_second","type":"function_call","call_id":"call_second","name":"second","arguments":""}}),
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_second","delta":"{\"b\":2}"}),
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_first","delta":"{\"a\":"}),
            ];
            let prefix = events.iter().map(|v| format!("event: {}\r\ndata: {v}\r\n\r\n", v["type"].as_str().unwrap())).collect::<String>();
            let first = stream::iter(prefix.as_bytes().chunks(3).map(|v| Ok::<_,Infallible>(Bytes::copy_from_slice(v))).collect::<Vec<_>>());
            let last = stream::once(async move {
                gate.notified().await;
                let done = json!({"type":"response.completed","response":{"id":"resp_stream","status":"completed","created_at":123,"model":"gpt-5","output":[
                    {"id":"rs_fixture","type":"reasoning","encrypted_content":"signed-native-reasoning","summary":[{"type":"summary_text","text":"Thinking"}]},
                    {"type":"message","role":"assistant","content":[{"type":"output_text","text":"Xin chào 🌏","annotations":[{"type":"url_citation","url":"https://example.test/"}]}]},
                    {"id":"fc_first","type":"function_call","call_id":"call_first","name":"first","arguments":"{\"a\":1}"},
                    {"id":"fc_second","type":"function_call","call_id":"call_second","name":"second","arguments":"{\"b\":2}"}
                ],"usage":{"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}}});
                Ok::<_,Infallible>(Bytes::from(format!("data: {done}\n\ndata: [DONE]\n\n")))
            });
            ([("content-type","text/event-stream")], Body::from_stream(first.chain(last)))
        }
    }));
    let (upstream, fixture_task) = serve(fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    store.save_connection(&json!({"provider":"codex","authType":"oauth","accessToken":"browser-login-token","providerSpecificData":{"baseUrl":format!("{upstream}/responses")}})).unwrap();
    let (base, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    let mut response = reqwest::Client::new().post(format!("{base}/v1/chat/completions")).bearer_auth(ADMIN)
        .json(&json!({"model":"codex/gpt-5","messages":[{"role":"user","content":"Hello"}],"stream":true,"stream_options":{"include_usage":true}})).send().await.unwrap();
    assert_eq!(response.status(), 200);
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !String::from_utf8_lossy(&bytes).contains("Xin chào") {
            bytes.extend_from_slice(
                &response
                    .chunk()
                    .await
                    .unwrap()
                    .expect("text must arrive before completion"),
            );
        }
    })
    .await
    .expect("Codex Chat must stream, without waiting for response.completed");
    release.notify_one();
    bytes.extend_from_slice(&response.bytes().await.unwrap());
    let wire = String::from_utf8(bytes).unwrap();
    assert!(wire.ends_with("data: [DONE]\n\n"), "{wire}");
    let chunks = wire
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(|data| serde_json::from_str::<Value>(data).unwrap())
        .collect::<Vec<_>>();
    assert!(
        chunks
            .iter()
            .all(|c| c["object"] == "chat.completion.chunk")
    );
    assert!(
        chunks
            .iter()
            .all(|c| c["id"] == chunks[0]["id"] && c["created"] == 123 && c["model"] == "gpt-5")
    );
    let deltas = chunks
        .iter()
        .map(|c| &c["choices"][0]["delta"])
        .collect::<Vec<_>>();
    assert_eq!(
        deltas.iter().filter(|d| d["role"] == "assistant").count(),
        1
    );
    assert_eq!(
        deltas
            .iter()
            .filter_map(|d| d["content"].as_str())
            .collect::<String>(),
        "Xin chào 🌏"
    );
    assert_eq!(
        deltas
            .iter()
            .filter_map(|d| d["reasoning_content"].as_str())
            .collect::<String>(),
        "Thinking"
    );
    assert!(deltas.iter().any(
        |d| d["extra_content"]["openai"]["responses_reasoning"][0]["encrypted_content"]
            == "signed-native-reasoning"
    ));
    assert!(
        deltas
            .iter()
            .any(|d| d["annotations"][0]["url"] == "https://example.test/")
    );
    for (index, id, name, arguments) in [
        (0, "call_first", "first", "{\"a\":1}"),
        (1, "call_second", "second", "{\"b\":2}"),
    ] {
        let calls = deltas
            .iter()
            .filter_map(|d| d["tool_calls"].as_array())
            .flatten()
            .filter(|c| c["index"] == index)
            .collect::<Vec<_>>();
        assert_eq!(
            calls
                .iter()
                .filter_map(|c| c["id"].as_str())
                .collect::<String>(),
            id
        );
        assert_eq!(
            calls
                .iter()
                .filter_map(|c| c["function"]["name"].as_str())
                .collect::<String>(),
            name
        );
        assert_eq!(
            calls
                .iter()
                .filter_map(|c| c["function"]["arguments"].as_str())
                .collect::<String>(),
            arguments
        );
    }
    let finishes = chunks
        .iter()
        .filter(|c| !c["choices"][0]["finish_reason"].is_null())
        .collect::<Vec<_>>();
    assert_eq!(finishes.len(), 1);
    assert_eq!(finishes[0]["choices"][0]["finish_reason"], "tool_calls");
    assert!(finishes[0].get("usage").is_none());
    let usages = chunks
        .iter()
        .filter(|chunk| chunk.get("usage").is_some())
        .collect::<Vec<_>>();
    assert_eq!(usages.len(), 1);
    assert_eq!(usages[0]["choices"], json!([]));
    assert_eq!(usages[0]["usage"]["total_tokens"], 15);
    assert_eq!(
        usages[0]["usage"]["prompt_tokens_details"]["cached_tokens"],
        3
    );
    assert_eq!(
        usages[0]["usage"]["completion_tokens_details"]["reasoning_tokens"],
        2
    );
    server.abort();
    fixture_task.abort();
}

#[tokio::test]
async fn codex_chat_stream_failures_never_report_success_or_expose_upstream_secrets() {
    let fixture = Router::new().route("/responses", post(|axum::Json(body): axum::Json<Value>| async move {
        let mode = body["input"][0]["content"][0]["text"].as_str().unwrap();
        let completed = json!({"type":"response.completed","response":{"status":"completed","model":"gpt-5","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"final only","annotations":[]}]}],"usage":{"input_tokens":1,"output_tokens":2}}});
        let wire = match mode {
            "failed" => "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"provider-private-secret\"}}}\n\n".into(),
            "incomplete" => "data: {\"type\":\"response.incomplete\"}\n\n".into(),
            "malformed" => "data: {not-json}\n\n".into(),
            "eof" => "data: [DONE]\n\n".into(),
            "unknown" => "data: {\"type\":\"response.unknown_output.delta\",\"delta\":\"opaque\"}\n\n".into(),
            "edited" => format!("data: {{\"type\":\"response.output_text.delta\",\"delta\":\"original\"}}\n\ndata: {completed}\n\n"),
            "final-only" => format!("data: {completed}\n\n"),
            _ => unreachable!(),
        };
        ([("content-type","text/event-stream")], wire)
    }));
    let (upstream, fixture_task) = serve(fixture).await;
    let temp = tempfile::tempdir().unwrap();
    let mut store = store_at(&temp);
    store.save_connection(&json!({"provider":"codex","authType":"oauth","accessToken":"provider-private-secret","providerSpecificData":{"baseUrl":format!("{upstream}/responses")}})).unwrap();
    let (base, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
    for mode in [
        "failed",
        "incomplete",
        "malformed",
        "eof",
        "unknown",
        "edited",
        "final-only",
    ] {
        let response = reqwest::Client::new().post(format!("{base}/v1/chat/completions")).bearer_auth(ADMIN)
            .json(&json!({"model":"codex/gpt-5","messages":[{"role":"user","content":mode}],"stream":true})).send().await.unwrap();
        assert_eq!(response.status(), 200);
        let wire = response.text().await.unwrap();
        assert!(!wire.contains("provider-private-secret"));
        if mode == "final-only" {
            assert!(wire.contains("\"content\":\"final only\""), "{wire}");
            assert!(wire.ends_with("data: [DONE]\n\n"));
            assert!(!wire.contains("\"usage\""));
        } else {
            assert!(
                wire.contains("\"code\":\"invalid_stream\""),
                "{mode}: {wire}"
            );
            assert!(
                !wire.contains("[DONE]") && !wire.contains("\"finish_reason\":\"stop\""),
                "{wire}"
            );
        }
    }
    server.abort();
    fixture_task.abort();
}

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
    first: Option<&'static [u8]>,
    dropped: Arc<tokio::sync::Notify>,
}
impl Stream for PendingBody {
    type Item = Result<Bytes, Infallible>;
    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(first) = self.first.take() {
            Poll::Ready(Some(Ok(Bytes::from_static(first))))
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
    for (model, first) in [
        (
            "openai/gpt-4o",
            b"data: {\"delta\":\"first\"}\n\n".as_slice(),
        ),
        (
            "codex/gpt-5",
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"first\"}\n\n".as_slice(),
        ),
    ] {
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
                            first: Some(first),
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
        store.save_connection(&json!({"provider":"codex","authType":"oauth","accessToken":"browser-login-token","providerSpecificData":{"baseUrl":format!("{upstream}/v1/chat/completions")}})).unwrap();
        let (url, server) = serve(router(Gateway::new(store, ADMIN.into()).unwrap())).await;
        let mut response = reqwest::Client::new()
            .post(format!("{url}/v1/chat/completions"))
            .bearer_auth(ADMIN)
            .json(
                &json!({"model":model,"stream":true,"messages":[{"role":"user","content":"test"}]}),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !String::from_utf8_lossy(&bytes).contains("first") {
                bytes.extend_from_slice(&response.chunk().await.unwrap().unwrap());
            }
        })
        .await
        .unwrap();
        if model.starts_with("openai/") {
            assert_eq!(bytes, first);
        }
        drop(response);
        tokio::time::timeout(Duration::from_secs(3), dropped.notified())
            .await
            .expect("upstream response body must be dropped after downstream disconnect");
        server.abort();
        fixture_task.abort();
    }
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
        .post(format!("{url}/v1/messages"))
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
