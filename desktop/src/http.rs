//! Native gateway. Unsupported adapters fail explicitly before credentials leave the process.
use crate::{
    Catalog, Result, Store,
    protocol::{self, Format},
};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::any,
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const MAX_REQUEST: usize = 8 * 1024 * 1024;
const MAX_RESPONSE: usize = 32 * 1024 * 1024;

pub struct Gateway {
    store: Arc<Mutex<Store>>,
    catalog: Arc<Catalog>,
    admin_token: String,
    client: reqwest::Client,
    cooldowns: Mutex<HashMap<(String, String), Instant>>,
    web_login: crate::web_login::WebLoginService,
    oauth: crate::oauth::Service,
    data_dir: PathBuf,
}

impl Gateway {
    pub fn new(store: Store, admin_token: String) -> Result<Arc<Self>> {
        let directory = PathBuf::from(store.connection.path().unwrap_or("data.sqlite"))
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned();
        Self::new_in(store, admin_token, directory)
    }

    pub fn new_in(store: Store, admin_token: String, data_dir: PathBuf) -> Result<Arc<Self>> {
        if admin_token.len() < 32 || admin_token.chars().any(char::is_whitespace) {
            return Err(io::Error::other(
                "Admin token must contain at least 32 non-whitespace characters",
            )
            .into());
        }
        let store = Arc::new(Mutex::new(store));
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(90))
            .no_proxy()
            .build()?;
        Ok(Arc::new(Self {
            web_login: crate::web_login::WebLoginService::new(store.clone(), data_dir.clone()),
            oauth: crate::oauth::Service::new(store.clone(), client.clone()),
            store,
            catalog: Arc::new(Catalog::load()?),
            admin_token,
            client,
            cooldowns: Mutex::new(HashMap::new()),
            data_dir,
        }))
    }

    pub async fn shutdown(&self) {
        self.web_login.shutdown().await;
        self.oauth.shutdown();
    }

    async fn db<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Store) -> Result<T> + Send + 'static,
    ) -> ApiResult<T> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = store
                .lock()
                .map_err(|_| io::Error::other("Storage lock failed"))?;
            operation(&mut store)
        })
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|error| {
            if error
                .downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::InvalidInput)
            {
                ApiError::new(StatusCode::BAD_REQUEST, "Invalid record fields")
            } else if error.downcast_ref::<io::Error>().is_some_and(|e|e.kind()==io::ErrorKind::PermissionDenied) {
                ApiError::new(StatusCode::FORBIDDEN,"This API key is not allowed to use the requested model")
            } else if matches!(error.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(cause, _)) if cause.code == rusqlite::ErrorCode::ConstraintViolation) {
                ApiError::new(StatusCode::CONFLICT, "Record conflicts with an existing identifier or name")
            } else {
                ApiError::internal()
            }
        })
    }
}

#[derive(Clone)]
enum Principal {
    Admin,
    Client(Value),
}

type ApiResult<T> = std::result::Result<T, ApiError>;
struct ApiError(StatusCode, String);
impl ApiError {
    fn new(status: StatusCode, message: &str) -> Self {
        Self(status, message.to_owned())
    }
    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Storage operation failed; existing data is preserved",
        )
    }
    fn unsupported() -> Self {
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            "This capability has not yet been ported to the Rust engine",
        )
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.0,
            axum::Json(
                json!({"error":{"message":self.1,"type":"gateway_error","code":self.0.as_u16()}}),
            ),
        )
            .into_response()
    }
}

pub fn router(state: Arc<Gateway>) -> Router {
    Router::new()
        .fallback(any(dispatch))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state)
}

fn inference_path(path: &str) -> bool {
    path.starts_with("/v1/")
        || path.starts_with("/api/v1/")
        || path.starts_with("/v1beta/")
        || path.starts_with("/api/v1beta/")
}

async fn authorize(
    State(state): State<Arc<Gateway>>,
    mut request: Request,
    next: Next,
) -> Response {
    let origin = request.headers().get("origin").cloned();
    let allowed_origin = origin.as_ref().is_some_and(|o| {
        matches!(
            o.to_str(),
            Ok("tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost")
        )
    });
    let mut response = async {
        if request.method() == Method::OPTIONS {
            if !allowed_origin {
                return ApiError::new(StatusCode::FORBIDDEN, "Origin is not allowed")
                    .into_response();
            }
            return StatusCode::NO_CONTENT.into_response();
        }
        if request.uri().path() == "/health" && request.method() == Method::GET {
            return axum::Json(json!({"status":"ok"})).into_response();
        }
        if request.uri().path() == "/callback" && request.method() == Method::GET {
            let query=query_fields(request.uri().query());
            return match state.oauth.callback(&query).await {
                Ok(_)=>axum::response::Html("<!doctype html><meta charset=utf-8><title>9Router</title><p>Đăng nhập thành công. Bạn có thể đóng trang này và trở lại 9Router.</p>").into_response(),
                Err(error)=>(StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST),axum::response::Html("<!doctype html><meta charset=utf-8><title>9Router</title><p>Không thể hoàn tất phiên đăng nhập này. Vui lòng trở lại 9Router và thử lại.</p>")).into_response(),
            };
        }
        let bearer = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        let query_token = request
            .uri()
            .query()
            .and_then(|query| reqwest::Url::parse(&format!("http://localhost/?{query}")).ok())
            .and_then(|url| {
                url.query_pairs()
                    .find(|(key, _)| key == "key")
                    .map(|(_, value)| value.into_owned())
            });
        let token = bearer
            .or_else(|| {
                request
                    .headers()
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
            })
            .or_else(|| {
                request
                    .headers()
                    .get("x-goog-api-key")
                    .and_then(|v| v.to_str().ok())
            })
            .or(query_token.as_deref());
        let Some(token) = token.filter(|token| !token.is_empty()) else {
            return ApiError::new(StatusCode::UNAUTHORIZED, "A valid API key is required")
                .into_response();
        };
        let principal = if token == state.admin_token {
            Principal::Admin
        } else {
            if !inference_path(request.uri().path()) {
                return ApiError::new(
                    StatusCode::FORBIDDEN,
                    "Management requires the local admin token",
                )
                .into_response();
            }
            let token = token.to_owned();
            match state
                .db(move |store| {
                    Ok(store
                        .api_keys()?
                        .into_iter()
                        .find(|key| key["key"] == token && key["isActive"] == true))
                })
                .await
            {
                Ok(Some(key)) => Principal::Client(key),
                Ok(None) => {
                    return ApiError::new(StatusCode::UNAUTHORIZED, "A valid API key is required")
                        .into_response();
                }
                Err(error) => return error.into_response(),
            }
        };
        request.extensions_mut().insert(principal);
        next.run(request).await
    }
    .await;
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    if allowed_origin {
        cors_headers(
            response.headers_mut(),
            origin.expect("allowed origin exists"),
        );
    }
    response
}

fn cors_headers(headers: &mut HeaderMap, origin: HeaderValue) {
    headers.insert("access-control-allow-origin", origin);
    headers.insert("vary", HeaderValue::from_static("Origin"));
    headers.insert(
        "access-control-allow-methods",
        HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"),
    );
    headers.insert("access-control-allow-headers", HeaderValue::from_static("authorization, content-type, x-api-key, x-goog-api-key, anthropic-version, anthropic-beta"));
}

async fn dispatch(State(state): State<Arc<Gateway>>, request: Request) -> ApiResult<Response> {
    let path = request.uri().path().to_owned();
    let principal = request
        .extensions()
        .get::<Principal>()
        .cloned()
        .expect("authorized middleware");
    if inference_path(&path) {
        if request.method() == Method::GET && (path == "/v1/models" || path == "/api/v1/models") {
            return models(state, principal).await;
        }
        if request.method() != Method::POST {
            return Err(ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "POST is required",
            ));
        }
        return inference(state, principal, request).await;
    }
    // Dashboard components consume the upstream {error: string} contract.
    Ok(match management(state, request).await {
        Ok(response) => response,
        Err(error) => (error.0, axum::Json(json!({"error":error.1}))).into_response(),
    })
}

async fn json_body(request: Request) -> ApiResult<Value> {
    let bytes = to_bytes(request.into_body(), MAX_REQUEST)
        .await
        .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "Request body exceeds 8 MiB"))?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Expected a JSON object"))?;
    if !body.is_object() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Expected a JSON object",
        ));
    }
    Ok(body)
}

fn safe_connection(mut value: Value) -> Value {
    fn redact(value: &mut Value) {
        if let Some(object) = value.as_object_mut() {
            let names: Vec<_> = object
                .keys()
                .filter(|key| {
                    let name = key.to_ascii_lowercase();
                    [
                        "key",
                        "token",
                        "secret",
                        "password",
                        "cookie",
                        "credential",
                        "authorization",
                    ]
                    .iter()
                    .any(|part| name.contains(part))
                })
                .cloned()
                .collect();
            for name in names {
                object.remove(&name);
            }
            for child in object.values_mut() {
                redact(child);
            }
        } else if let Some(array) = value.as_array_mut() {
            for child in array {
                redact(child);
            }
        }
    }
    let has_key = value
        .get("apiKey")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    redact(&mut value);
    value["hasApiKey"] = json!(has_key);
    value
}

fn query_fields(query: Option<&str>) -> HashMap<String, String> {
    reqwest::Url::parse(&format!("http://localhost/?{}", query.unwrap_or_default()))
        .map(|url| {
            url.query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn oauth_error(error: crate::oauth::Error) -> ApiError {
    ApiError::new(
        StatusCode::from_u16(error.status).unwrap_or(StatusCode::BAD_REQUEST),
        &error.message,
    )
}

fn safe_settings(mut value: Value) -> Value {
    let has_password = value["password"].as_str().is_some_and(|p| !p.is_empty());
    let oidc_configured = ["oidcIssuerUrl", "oidcClientId", "oidcClientSecret"]
        .iter()
        .all(|k| value[k].as_str().is_some_and(|v| !v.is_empty()));
    for key in ["password", "oidcClientSecret", "mitmSudoEncrypted"] {
        value.as_object_mut().expect("settings object").remove(key);
    }
    value["hasPassword"] = json!(has_password);
    value["oidcConfigured"] = json!(oidc_configured);
    value
}

fn required_text(body: &Value, key: &str, max: usize) -> ApiResult<String> {
    body[key]
        .as_str()
        .filter(|s| !s.trim().is_empty() && s.len() <= max && !s.chars().any(char::is_control))
        .map(str::to_owned)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                &format!("A valid {key} is required"),
            )
        })
}

async fn model_management(
    state: Arc<Gateway>,
    path: &str,
    method: Method,
    query: HashMap<String, String>,
    request: Request,
) -> ApiResult<Response> {
    let result = match (path, method) {
        ("/api/models/custom", Method::GET) => {
            json!({"models":state.db(|s|Ok(s.scope("customModels")?.into_values().collect::<Vec<_>>())).await?})
        }
        ("/api/models/custom", Method::POST) => {
            let body = json_body(request).await?;
            let provider = required_text(&body, "providerAlias", 256)?;
            let id = required_text(&body, "id", 2048)?;
            let kind = body["type"].as_str().unwrap_or("llm").to_owned();
            if provider.contains('|')
                || id.contains('|')
                || ![
                    "llm",
                    "embedding",
                    "image",
                    "video",
                    "tts",
                    "stt",
                    "imageToText",
                    "webSearch",
                    "webFetch",
                    "music",
                ]
                .contains(&kind.as_str())
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Invalid custom model type or identifier",
                ));
            }
            let added = state
                .db(move |s| {
                    let key = format!("{provider}|{id}|{kind}");
                    let previous = s.get("customModels", &key)?;
                    let added = previous.is_none();
                    let mut value = previous.unwrap_or_else(
                        || json!({"providerAlias":provider,"id":id,"type":kind,"name":id}),
                    );
                    if let Some(name) = body["name"]
                        .as_str()
                        .filter(|n| !n.is_empty() && n.len() <= 2048)
                    {
                        value["name"] = json!(name);
                    }
                    if let Some(caps) = body["caps"].as_object() {
                        let caps: serde_json::Map<_, _> = caps
                            .iter()
                            .filter(|(key, value)| {
                                value.is_boolean()
                                    && [
                                        "vision",
                                        "search",
                                        "reasoning",
                                        "tools",
                                        "json",
                                        "streaming",
                                    ]
                                    .contains(&key.as_str())
                            })
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect();
                        if !caps.is_empty() {
                            value["caps"] = json!(caps);
                        }
                    }
                    if let Some(transport) = body["transport"].as_str().filter(|_| kind == "stt")
                        && [
                            "openai-multipart",
                            "openai-json",
                            "deepgram",
                            "assemblyai",
                            "gemini",
                            "speechmatics",
                        ]
                        .contains(&transport)
                    {
                        value["transport"] = json!(transport);
                    }
                    s.put_batch(&[("customModels", &key, &value)])?;
                    Ok(added)
                })
                .await?;
            json!({"success":true,"added":added})
        }
        ("/api/models/custom", Method::DELETE) => {
            let provider = query
                .get("providerAlias")
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "providerAlias required"))?;
            let id = query
                .get("id")
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "id required"))?;
            let key = format!(
                "{}|{}|{}",
                provider,
                id,
                query.get("type").map(String::as_str).unwrap_or("llm")
            );
            state.db(move |s| s.remove("customModels", &key)).await?;
            json!({"success":true})
        }
        ("/api/models/disabled", Method::GET) => {
            if let Some(provider) = query.get("providerAlias").cloned() {
                json!({"ids":state.db(move |s|Ok(s.get("disabledModels",&provider)?.unwrap_or(json!([])))).await?})
            } else {
                json!({"disabled":state.db(|s|s.scope("disabledModels")).await?})
            }
        }
        ("/api/models/disabled", method) if method == Method::POST || method == Method::DELETE => {
            let enable = method == Method::DELETE;
            let body = if enable {
                json!({"providerAlias":query.get("providerAlias"),"ids":query.get("id").map(|id|vec![id])})
            } else {
                json_body(request).await?
            };
            let provider = required_text(&body, "providerAlias", 256)?;
            let ids = if enable && body["ids"].is_null() {
                Vec::new()
            } else {
                body["ids"]
                    .as_array()
                    .filter(|a| a.len() <= 10000)
                    .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "ids must be an array"))?
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .filter(|s| !s.is_empty() && s.len() <= 4096)
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                ApiError::new(StatusCode::BAD_REQUEST, "Invalid model id")
                            })
                    })
                    .collect::<ApiResult<Vec<_>>>()?
            };
            state
                .db(move |s| {
                    let mut current = s
                        .get("disabledModels", &provider)?
                        .unwrap_or(json!([]))
                        .as_array()
                        .ok_or_else(|| io::Error::other("Corrupt disabled model data"))?
                        .clone();
                    if enable {
                        current.retain(|v| !ids.is_empty() && !ids.iter().any(|id| v == id));
                    } else {
                        for id in ids {
                            let value = json!(id);
                            if !current.contains(&value) {
                                current.push(value);
                            }
                        }
                    }
                    if current.is_empty() {
                        s.remove("disabledModels", &provider)
                    } else {
                        s.put_batch(&[("disabledModels", &provider, &json!(current))])
                    }
                })
                .await?;
            json!({"success":true})
        }
        ("/api/models", Method::GET) => {
            let (aliases, disabled, custom) = state
                .db(|s| {
                    Ok((
                        s.aliases()?,
                        s.scope("disabledModels")?,
                        s.scope("customModels")?,
                    ))
                })
                .await?;
            let mut models = Vec::new();
            for provider in &state.catalog.providers {
                if !provider.service_kinds.is_empty()
                    && !provider.service_kinds.iter().any(|k| k == "llm")
                {
                    continue;
                }
                for model in &provider.models {
                    let Some(id) = model["id"].as_str() else {
                        continue;
                    };
                    if disabled
                        .get(&provider.alias)
                        .or_else(|| disabled.get(&provider.id))
                        .and_then(Value::as_array)
                        .is_some_and(|ids| ids.iter().any(|v| v == id))
                    {
                        continue;
                    }
                    let full = format!("{}/{id}", provider.id);
                    let routed = format!("{}/{id}", provider.alias);
                    let mut value = model.clone();
                    value["provider"] = json!(provider.id);
                    value["model"] = json!(id);
                    value["fullModel"] = json!(full);
                    value["routedModel"] = json!(routed);
                    value["alias"] = aliases.get(&full).cloned().unwrap_or(json!(id));
                    models.push(value);
                }
            }
            for mut model in custom.into_values() {
                if model["type"] != "llm" {
                    continue;
                }
                let full = format!(
                    "{}/{}",
                    model["providerAlias"].as_str().unwrap_or_default(),
                    model["id"].as_str().unwrap_or_default()
                );
                if models.iter().any(|m| m["routedModel"] == full) {
                    continue;
                }
                model["provider"] = model["providerAlias"].clone();
                model["model"] = model["id"].clone();
                model["fullModel"] = json!(full);
                model["routedModel"] = json!(full);
                model["alias"] = aliases
                    .get(&full)
                    .cloned()
                    .unwrap_or_else(|| model["id"].clone());
                models.push(model);
            }
            json!({"models":models})
        }
        ("/api/models", Method::PUT) => {
            let body = json_body(request).await?;
            let model = required_text(&body, "model", 4096)?;
            let alias = required_text(&body, "alias", 4096)?;
            let saved_model = model.clone();
            let saved_alias = alias.clone();
            state
                .db(move |s| {
                    let mut aliases = s.aliases()?;
                    if aliases
                        .iter()
                        .any(|(key, value)| value == &alias && key != &model)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Alias already in use",
                        )
                        .into());
                    }
                    aliases.insert(model, json!(alias));
                    s.save_aliases(&aliases)
                })
                .await?;
            json!({"success":true,"model":saved_model,"alias":saved_alias})
        }
        ("/api/models/availability", Method::GET) => {
            let unavailable=state.db(|s|Ok(s.connections(None)?.into_iter().filter(|c|c["testStatus"]=="unavailable").map(|c|json!({"provider":c["provider"],"model":"__all","status":"unavailable","connectionId":c["id"],"connectionName":c["name"],"lastError":c["lastError"]})).collect::<Vec<_>>())).await?;
            json!({"models":unavailable,"unavailableCount":unavailable.len()})
        }
        ("/api/models/availability", Method::POST) => {
            let body = json_body(request).await?;
            if body["action"] != "clearCooldown" {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid action"));
            }
            let provider = required_text(&body, "provider", 256)?;
            let model = required_text(&body, "model", 4096)?;
            let cooldown_model = model.clone();
            let ids=state.db(move |s|{let mut ids=Vec::new();for c in s.connections(Some(&provider))? {let id=c["id"].clone();s.save_connection(&json!({"id":id,format!("modelLock_{model}"):null,"testStatus":"active"}))?;ids.push(id);}Ok(ids)}).await?;
            state
                .cooldowns
                .lock()
                .map_err(|_| ApiError::internal())?
                .retain(|(id, model), _| model != &cooldown_model || !ids.iter().any(|v| v == id));
            json!({"ok":true})
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::METHOD_NOT_ALLOWED,
                "Method not allowed",
            ));
        }
    };
    Ok(axum::Json(result).into_response())
}

async fn management(state: Arc<Gateway>, request: Request) -> ApiResult<Response> {
    let path = request.uri().path().trim_end_matches('/').to_owned();
    let method = request.method().clone();
    let query = query_fields(request.uri().query());
    if let Some(provider) = path
        .strip_prefix("/api/providers/")
        .and_then(|p| p.strip_suffix("/overrides"))
    {
        let provider = state
            .catalog
            .provider_aliases
            .get(provider)
            .map(String::as_str)
            .unwrap_or(provider)
            .to_owned();
        let builtin = state
            .catalog
            .provider(&provider)
            .and_then(|p| p.transport.as_ref())
            .and_then(|t| t.get("headers"))
            .cloned()
            .unwrap_or(json!({}));
        if method == Method::GET {
            let saved = state
                .db(move |s| {
                    Ok(s.settings()?["providerOverrides"][&provider]["headers"]
                        .as_object()
                        .cloned()
                        .unwrap_or_default())
                })
                .await?;
            return Ok(
                axum::Json(json!({"headers":saved,"builtinHeaders":builtin})).into_response(),
            );
        }
        if method == Method::PUT {
            let body = json_body(request).await?;
            let mut headers = serde_json::Map::new();
            if let Some(input) = body.get("headers").filter(|v| !v.is_null()) {
                let input = input.as_object().filter(|h| h.len() <= 20).ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "headers must be an object with at most 20 entries",
                    )
                })?;
                for (name, value) in input {
                    if value.is_null() || value == "" {
                        continue;
                    }
                    let value = value
                        .as_str()
                        .filter(|v| v.len() <= 8192 && !v.chars().any(char::is_control))
                        .ok_or_else(|| {
                            ApiError::new(StatusCode::BAD_REQUEST, "Invalid header value")
                        })?;
                    if name.is_empty()
                        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                        || [
                            "host",
                            "content-length",
                            "content-type",
                            "connection",
                            "transfer-encoding",
                            "authorization",
                            "cookie",
                            "x-api-key",
                            "x-goog-api-key",
                            "proxy-authorization",
                        ]
                        .contains(&name.to_ascii_lowercase().as_str())
                    {
                        return Err(ApiError::new(
                            StatusCode::BAD_REQUEST,
                            "Header cannot be overridden",
                        ));
                    }
                    headers.insert(name.to_owned(), json!(value));
                }
            }
            let saved = headers.clone();
            state
                .db(move |s| {
                    let mut overrides = s.settings()?["providerOverrides"]
                        .as_object()
                        .cloned()
                        .unwrap_or_default();
                    if headers.is_empty() {
                        overrides.remove(&provider);
                    } else {
                        overrides.insert(provider, json!({"headers":headers}));
                    }
                    s.save_settings(&json!({"providerOverrides":overrides}))
                })
                .await?;
            return Ok(axum::Json(json!({"headers":saved})).into_response());
        }
        return Err(ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "Method not allowed",
        ));
    }
    if path == "/api/tunnel/status" && method == Method::GET {
        let settings = state.db(|s| s.settings()).await?;
        if settings["tunnelEnabled"] == true || settings["tailscaleEnabled"] == true {
            return Err(ApiError::new(
                StatusCode::NOT_IMPLEMENTED,
                "The native tunnel runtime has not yet been ported",
            ));
        }
        return Ok(axum::Json(json!({"tunnel":{"enabled":false,"settingsEnabled":false,"tunnelUrl":"","shortId":"","publicUrl":"","running":false},"tailscale":{"enabled":false,"settingsEnabled":false,"tunnelUrl":settings["tailscaleUrl"],"running":false,"loggedIn":false},"download":{"downloading":false,"progress":0}})).into_response());
    }
    if path == "/api/providers/web-login" {
        return match method {
            Method::GET=>Ok(axum::Json(if let Some(provider)=query.get("provider") {
                state.web_login.status(provider).ok_or_else(||ApiError::new(StatusCode::NOT_FOUND,"No login session for this provider"))?
            } else {json!({"providers":crate::web_login::PROVIDERS,"profiles":state.web_login.profiles()})}).into_response()),
            Method::POST=>{
                let body=json_body(request).await?;
                let provider=required_text(&body,"provider",128)?;
                if !crate::web_login::PROVIDERS.contains(&provider.as_str()) {return Err(ApiError::new(StatusCode::BAD_REQUEST,"Web login not supported for this provider"));}
                let profile=body["profile"].as_str().unwrap_or("default");
                let result=state.web_login.start(&provider,profile).await.map_err(|e|ApiError::new(StatusCode::CONFLICT,&e.to_string()))?;
                Ok((StatusCode::ACCEPTED,axum::Json(result)).into_response())
            },
            Method::DELETE=>{
                let provider=query.get("provider").ok_or_else(||ApiError::new(StatusCode::BAD_REQUEST,"provider required"))?;
                Ok(axum::Json(json!({"cancelled":state.web_login.cancel(provider)})).into_response())
            },
            _=>Err(ApiError::new(StatusCode::METHOD_NOT_ALLOWED,"Method not allowed")),
        };
    }
    if path == "/api/oauth/callback-status" && method == Method::GET {
        let state_key = query
            .get("state")
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "state required"))?;
        return Ok(axum::Json(
            state
                .oauth
                .callback_status(state_key)
                .map_err(oauth_error)?,
        )
        .into_response());
    }
    if let Some(action) = path.strip_prefix("/api/oauth/") {
        let (provider, action) = action
            .split_once('/')
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "OAuth action required"))?;
        let body = if [Method::POST, Method::PUT, Method::PATCH].contains(&method) {
            json_body(request).await?
        } else {
            json!({})
        };
        return Ok(axum::Json(
            state
                .oauth
                .handle(provider, action, method.as_str(), &query, body)
                .await
                .map_err(oauth_error)?,
        )
        .into_response());
    }
    if path == "/api/health" || path == "/api/init" {
        return Ok(axum::Json(json!({"ok":true})).into_response());
    }
    if path == "/api/machine-id" && method == Method::GET {
        let machine_id = state
            .db(|s| {
                if let Some(value) = s.get("desktop", "machineId")? {
                    return Ok(value);
                }
                let value = json!(uuid::Uuid::new_v4().simple().to_string());
                s.put_batch(&[("desktop", "machineId", &value)])?;
                Ok(value)
            })
            .await?;
        return Ok(axum::Json(json!({"machineId":machine_id})).into_response());
    }
    if path == "/api/version" && method == Method::GET {
        return Ok(axum::Json(json!({"currentVersion":env!("CARGO_PKG_VERSION"),"version":env!("CARGO_PKG_VERSION"),"hasUpdate":false,"engine":"rust"})).into_response());
    }
    if path == "/api/auth/status" && method == Method::GET {
        return Ok(axum::Json(json!({"authenticated":true,"requireLogin":false,"authMode":"desktop","displayName":"Local desktop","loginMethod":"Desktop","hasPassword":false,"oidcConfigured":false,"samlConfigured":false})).into_response());
    }
    if path == "/api/locale" && method == Method::POST {
        let body = json_body(request).await?;
        let locale = body["locale"]
            .as_str()
            .filter(|l| {
                matches!(
                    *l,
                    "en" | "vi"
                        | "zh"
                        | "zh-CN"
                        | "zh-TW"
                        | "ja"
                        | "ko"
                        | "es"
                        | "fr"
                        | "de"
                        | "ru"
                        | "pt"
                )
            })
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid locale"))?
            .to_owned();
        let saved = locale.clone();
        state
            .db(move |s| s.put_batch(&[("desktop", "locale", &json!(saved))]))
            .await?;
        return Ok(axum::Json(json!({"success":true,"locale":locale})).into_response());
    }
    if matches!(
        path.as_str(),
        "/api/models/custom" | "/api/models/disabled" | "/api/models" | "/api/models/availability"
    ) {
        return model_management(state, &path, method, query, request).await;
    }
    if path == "/api/models/alias" && method == Method::DELETE {
        let alias = query
            .get("alias")
            .cloned()
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Alias required"))?;
        state
            .db(move |s| {
                let mut aliases = s.aliases()?;
                aliases.remove(&alias);
                s.save_aliases(&aliases)
            })
            .await?;
        return Ok(axum::Json(json!({"success":true})).into_response());
    }
    if path == "/api/status" && method == Method::GET {
        return Ok(axum::Json(json!({"version":env!("CARGO_PKG_VERSION"),"engine":"rust","featureComplete":false,
            "implemented":["sqlite","model-resolution","connections","aliases","combos","api-keys","original-28-screen-ui","native-browser-login","oauth-pkce-device-flows","oauth-refresh","generic-transports","gemini-cli","codex-responses","muse-web-chat-media","same-protocol-streaming"],
            "remaining":["specialized-provider-auth-and-executors","cross-protocol-streaming","full-management-api-parity","sso","usage-pricing-quota","proxy-transports","tunnels-mitm","rtk","mcp","cli-integrations","authenticated-provider-verification"]})).into_response());
    }
    if (path == "/api/providers" || path == "/api/providers/connections") && method == Method::GET {
        let connections = state.db(|store| store.connections(None)).await?;
        return Ok(axum::Json(json!({"providers":state.catalog.providers,"connections":connections.into_iter().map(safe_connection).collect::<Vec<_>>()})).into_response());
    }
    let id_prefixes = [
        "/api/providers/connections/",
        "/api/providers/",
        "/api/provider-nodes/",
        "/api/combos/",
        "/api/keys/",
        "/api/proxy-pools/",
    ];
    let (collection, id) = if let Some(prefix) = id_prefixes
        .into_iter()
        .find(|prefix| path.starts_with(prefix))
    {
        (
            prefix.trim_end_matches('/'),
            Some(path[prefix.len()..].to_owned()),
        )
    } else {
        (path.as_str(), None)
    };
    if id
        .as_ref()
        .is_some_and(|id| id.is_empty() || id.contains('/') || id.len() > 256)
    {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid record id"));
    }
    if method == Method::DELETE {
        let id =
            id.ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Record id is required"))?;
        let deleted = match collection {
            "/api/providers/connections" | "/api/providers" => {
                state.db(move |s| s.delete_connection(&id)).await?
            }
            "/api/provider-nodes" => state.db(move |s| s.delete_node(&id)).await?,
            "/api/combos" => state.db(move |s| s.delete_combo(&id)).await?,
            "/api/keys" => state.db(move |s| s.delete_api_key(&id)).await?,
            "/api/proxy-pools" => state.db(move |s| s.delete_proxy_pool(&id)).await?,
            _ => return Err(ApiError::unsupported()),
        };
        return if deleted {
            Ok(axum::Json(json!({"success":true})).into_response())
        } else {
            Err(ApiError::new(StatusCode::NOT_FOUND, "Record not found"))
        };
    }
    if method == Method::GET {
        let result = match collection {
            "/api/proxy-pools" => {
                let connections = state.db(|s| s.connections(None)).await?;
                let mut pools = state.db(|s| s.proxy_pools()).await?;
                if let Some(active) = query
                    .get("isActive")
                    .filter(|v| matches!(v.as_str(), "true" | "false"))
                {
                    pools.retain(|p| p["isActive"] == json!(active == "true"));
                }
                if query.get("includeUsage").is_some_and(|v| v == "true") {
                    for pool in &mut pools {
                        pool["boundConnectionCount"] = json!(
                            connections
                                .iter()
                                .filter(|c| c["providerSpecificData"]["proxyPoolId"] == pool["id"])
                                .count()
                        );
                    }
                }
                if let Some(id) = id {
                    json!({"proxyPool":pools.into_iter().find(|p|p["id"]==id).ok_or_else(||ApiError::new(StatusCode::NOT_FOUND,"Proxy pool not found"))?})
                } else {
                    json!({"proxyPools":pools})
                }
            }
            "/api/providers" | "/api/providers/connections" => {
                let id = id.ok_or_else(|| {
                    ApiError::new(StatusCode::BAD_REQUEST, "Connection id required")
                })?;
                let connection = state
                    .db(move |s| Ok(s.connections(None)?.into_iter().find(|c| c["id"] == id)))
                    .await?
                    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Connection not found"))?;
                json!({"connection":safe_connection(connection)})
            }
            "/api/provider-nodes" => {
                let nodes = state.db(|s| s.nodes()).await?;
                if let Some(id) = id {
                    json!({"node":nodes.into_iter().find(|n|n["id"]==id).ok_or_else(||ApiError::new(StatusCode::NOT_FOUND,"Node not found"))?})
                } else {
                    json!({"nodes":nodes})
                }
            }
            "/api/combos" => {
                let combos = state.db(|s| s.combos()).await?;
                if let Some(id) = id {
                    combos
                        .into_iter()
                        .find(|c| c["id"] == id)
                        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Combo not found"))?
                } else {
                    json!({"combos":combos})
                }
            }
            "/api/keys" => {
                let keys = state.db(|s| s.api_keys()).await?;
                json!({"keys":keys,"apiKeys":keys})
            }
            "/api/models/alias" => json!({"aliases":state.db(|s| s.aliases()).await?}),
            "/api/settings" => safe_settings(state.db(|s| s.settings()).await?),
            _ => return Err(unimplemented_route(&state.catalog, &path)),
        };
        return Ok(axum::Json(result).into_response());
    }
    if ![Method::POST, Method::PUT, Method::PATCH].contains(&method) {
        return Err(ApiError::unsupported());
    }
    let mut body = json_body(request).await?;
    if let Some(id) = id {
        body["id"] = json!(id);
    }
    let result = match collection {
        "/api/proxy-pools" => {
            json!({"proxyPool":state.db(move |s|s.save_proxy_pool(&body)).await?})
        }
        "/api/providers" | "/api/providers/connections" => {
            if let Some(input) = body.get("provider").and_then(Value::as_str) {
                let id = state
                    .catalog
                    .provider_aliases
                    .get(input)
                    .map(String::as_str)
                    .unwrap_or(input)
                    .to_owned();
                let nodes = state.db(|s| s.nodes()).await?;
                if state.catalog.provider(&id).is_none() && !nodes.iter().any(|n| n["id"] == id) {
                    return Err(ApiError::new(StatusCode::BAD_REQUEST, "Unknown provider"));
                }
                body["provider"] = json!(id);
            }
            json!({"connection":safe_connection(state.db(move |s| s.save_connection(&body)).await?)})
        }
        "/api/provider-nodes" => json!({"node":state.db(move |s| s.save_node(&body)).await?}),
        "/api/combos" => state.db(move |s| s.save_combo(&body)).await?,
        "/api/keys" => {
            if body.get("key").is_none() && body.get("id").is_none() {
                body["key"] = json!(format!(
                    "sk-{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                ));
            }
            let key = state.db(move |s| s.save_api_key(&body)).await?;
            let mut response = key.clone();
            response["apiKey"] = key;
            response
        }
        "/api/models/alias" => {
            if let (Some(alias), Some(model)) = (body["alias"].as_str(), body.get("model")) {
                let alias = alias.to_owned();
                let model = model.clone();
                state
                    .db(move |s| {
                        let mut aliases = s.aliases()?;
                        aliases.insert(alias, model);
                        s.save_aliases(&aliases)
                    })
                    .await?;
            } else {
                let aliases = body.as_object().expect("validated object").clone();
                state.db(move |s| s.save_aliases(&aliases)).await?;
            }
            json!({"success":true})
        }
        "/api/settings" => {
            if body.get("newPassword").is_some() {
                return Err(ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Dashboard password changes are not yet available in the native engine",
                ));
            }
            for key in ["password", "currentPassword", "mitmSudoEncrypted"] {
                body.as_object_mut().expect("object").remove(key);
            }
            safe_settings(state.db(move |s| s.save_settings(&body)).await?)
        }
        _ => return Err(unimplemented_route(&state.catalog, &path)),
    };
    Ok((
        if method == Method::POST {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        axum::Json(result),
    )
        .into_response())
}

fn unimplemented_route(catalog: &Catalog, path: &str) -> ApiError {
    if catalog.routes.iter().any(|route| route["path"] == path) {
        ApiError::unsupported()
    } else {
        ApiError::new(StatusCode::NOT_FOUND, "Route not found")
    }
}

fn allowed(
    store: &Store,
    catalog: &Catalog,
    principal: &Principal,
    requested: &str,
) -> Result<bool> {
    let Principal::Client(key) = principal else {
        return Ok(true);
    };
    if key["access"]["restricted"] != true {
        return Ok(true);
    }
    let combos = store.combos()?;
    let info = store.resolve_app_model(catalog, requested)?;
    let requested_combo = store.combo_models(requested)?.is_some() || info.provider.is_none();
    for entry in key["access"]["allow"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let entry_combo = combos.iter().any(|combo| {
            combo["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(entry))
        });
        if requested_combo {
            if entry_combo && entry.eq_ignore_ascii_case(requested) {
                return Ok(true);
            }
        } else if !entry_combo {
            let target = store.resolve_app_model(catalog, entry)?;
            if let (Some(p), Some(m), Some(tp), Some(tm)) =
                (&info.provider, &info.model, &target.provider, &target.model)
                && p.eq_ignore_ascii_case(tp)
                && m.eq_ignore_ascii_case(tm)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

async fn models(state: Arc<Gateway>, principal: Principal) -> ApiResult<Response> {
    let catalog = state.catalog.clone();
    let data = state.db(move |store| {
        let connections = store.connections(None)?;
        let active: HashSet<_> = connections.iter().filter(|c| c["isActive"] == true).filter_map(|c| c["provider"].as_str()).collect();
        let mut entries = Vec::new();
        for provider in &catalog.providers {
            if !active.contains(provider.id.as_str()) { continue; }
            for model in &provider.models {
                if let Some(name) = model["id"].as_str() {
                    let id = format!("{}/{name}", provider.alias);
                    if allowed(store, &catalog, &principal, &id)? { entries.push(json!({"id":id,"object":"model","created":0,"owned_by":provider.id,"name":model["name"],"kind":model["kind"]})); }
                }
            }
        }
        let aliases = store.aliases()?;
        let combos = store.combos()?;
        for name in aliases.keys().map(String::as_str).chain(combos.iter().filter_map(|c| c["name"].as_str())) {
            if allowed(store, &catalog, &principal, name)? { entries.push(json!({"id":name,"object":"model","created":0,"owned_by":"local"})); }
        }
        Ok(entries)
    }).await?;
    Ok(axum::Json(json!({"object":"list","data":data})).into_response())
}

struct Candidate {
    provider: String,
    model: String,
    connection: Value,
    node: Option<Value>,
    headers: Value,
}

fn expand(
    store: &Store,
    catalog: &Catalog,
    name: &str,
    stack: &mut Vec<String>,
    output: &mut Vec<Candidate>,
) -> Result<()> {
    if stack.len() >= 8 || stack.iter().any(|s| s == name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Combo contains a cycle or exceeds eight nested levels",
        )
        .into());
    }
    if let Some(models) = store.combo_models(name)? {
        stack.push(name.to_owned());
        for model in models {
            expand(store, catalog, &model, stack, output)?;
        }
        stack.pop();
        return Ok(());
    }
    let info = store.resolve_app_model(catalog, name)?;
    if let (Some(provider), Some(model)) = (info.provider, info.model) {
        let node = store
            .nodes()?
            .into_iter()
            .find(|node| node["id"] == provider);
        for connection in store
            .connections(Some(&provider))?
            .into_iter()
            .filter(|c| c["isActive"] == true)
        {
            if output.len() >= 256 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Too many fallback candidates",
                )
                .into());
            }
            output.push(Candidate {
                provider: provider.clone(),
                model: model.clone(),
                connection,
                node: node.clone(),
                headers: store.settings()?["providerOverrides"][&provider]["headers"].clone(),
            });
        }
    }
    Ok(())
}

async fn inference(
    state: Arc<Gateway>,
    principal: Principal,
    request: Request,
) -> ApiResult<Response> {
    let path = request.uri().path().trim_start_matches("/api").to_owned();
    if matches!(
        path.as_str(),
        "/v1/images/generations" | "/v1/videos/generations"
    ) {
        return muse_media(state, principal, request, path == "/v1/videos/generations").await;
    }
    let source = match path.as_str() {
        "/v1/chat/completions" => Format::OpenAi,
        "/v1/messages" => Format::Claude,
        "/v1/responses" => Format::Responses,
        p if p.starts_with("/v1beta/models/") || p.starts_with("/v1/models/") => Format::Gemini,
        _ => return Err(ApiError::unsupported()),
    };
    let client_headers = request.headers().clone();
    let mut body = json_body(request).await?;
    let mut stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    if body.get("stream").is_some_and(|v| !v.is_boolean()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "stream must be a boolean",
        ));
    }
    if source == Format::Gemini {
        let (name, action) = path
            .split_once("/models/")
            .and_then(|(_, rest)| rest.rsplit_once(':'))
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid Gemini model action"))?;
        stream = match action {
            "streamGenerateContent" => true,
            "generateContent" => false,
            _ => return Err(ApiError::unsupported()),
        };
        body["model"] = json!(name);
    }
    let requested = body["model"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.chars().any(char::is_control))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "A nonempty model string is required",
            )
        })?
        .to_owned();
    let catalog = state.catalog.clone();
    let (permitted, proxy_required, candidates) = state
        .db(move |store| {
            if !allowed(store, &catalog, &principal, &requested)? {
                return Ok((false, false, Vec::new()));
            }
            if store.settings()?["outboundProxyEnabled"] == true {
                return Ok((true, true, Vec::new()));
            }
            let mut candidates = Vec::new();
            expand(
                store,
                &catalog,
                &requested,
                &mut Vec::new(),
                &mut candidates,
            )?;
            Ok((true, false, candidates))
        })
        .await?;
    if !permitted {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "This API key is not allowed to use the requested model",
        ));
    }
    if proxy_required {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Global outbound proxy is configured but its Rust transport is not yet ported",
        ));
    }
    if candidates.is_empty() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "No active connection is configured for this model",
        ));
    }
    let mut last_error = ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "All matching accounts are cooling down",
    );
    let mut rejected_models = HashSet::new();
    for mut candidate in candidates {
        if rejected_models.contains(&(candidate.provider.clone(), candidate.model.clone())) {
            continue;
        }
        if let Err(error) = connection_proxy(&candidate.connection) {
            last_error = error;
            continue;
        }
        if candidate.provider == "muse-web" {
            match muse_chat(state.clone(), &candidate, &body, source, stream).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    last_error = error;
                    continue;
                }
            }
        }
        let expiry = candidate.connection["expiresAt"].clone();
        let refresh_due=candidate.connection["authType"]=="oauth" && !expiry.is_null() && state.db(move |s| {
            let due=s.connection.query_row("SELECT CASE WHEN ?1 NOT GLOB '*[^0-9.]*' AND length(?1)>0 THEN (CAST(?1 AS REAL) / CASE WHEN CAST(?1 AS REAL)>100000000000 THEN 1000 ELSE 1 END) <= unixepoch('now')+60 ELSE datetime(?1)<=datetime('now','+60 seconds') END",[expiry.as_str().map(str::to_owned).unwrap_or_else(||expiry.to_string())],|row|row.get::<_,Option<bool>>(0))?;
            Ok(due.unwrap_or(false))
        }).await?;
        if refresh_due && let Some(id) = candidate.connection["id"].as_str() {
            let id = id.to_owned();
            match state.oauth.refresh_connection(&id).await {
                Ok(connection) => candidate.connection = connection,
                Err(_) => {
                    last_error = ApiError::new(
                        StatusCode::UNAUTHORIZED,
                        "Provider login expired; sign in again from the provider page",
                    );
                    continue;
                }
            }
        }
        let cooldown_key = (
            candidate.connection["id"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
            candidate.model.clone(),
        );
        {
            let mut cooldowns = state.cooldowns.lock().map_err(|_| ApiError::internal())?;
            cooldowns.retain(|_, until| *until > Instant::now());
            if cooldowns.contains_key(&cooldown_key) {
                continue;
            }
        }
        let (url, target, headers, transformed) = if candidate.provider == "codex" {
            if !matches!(source, Format::Responses | Format::OpenAi)
                || (stream && source != Format::Responses)
            {
                last_error = ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Codex supports native Responses streams and non-streaming Chat requests",
                );
                continue;
            }
            let native = if source == Format::OpenAi {
                crate::codex::from_chat(&body)
            } else {
                Ok(body.clone())
            };
            let prepared = native.and_then(|mut native| {
                native["model"] = json!(candidate.model);
                crate::codex::prepare(&candidate.connection, &native)
            });
            let mut prepared = match prepared {
                Ok(prepared) => prepared,
                Err(_) => {
                    last_error = ApiError::new(
                        StatusCode::NOT_IMPLEMENTED,
                        "Request contains fields not yet supported by the Codex adapter",
                    );
                    continue;
                }
            };
            apply_headers(&mut prepared.headers, &candidate.headers)?;
            (
                reqwest::Url::parse(&prepared.url).map_err(|_| ApiError::unsupported())?,
                Format::Responses,
                prepared.headers,
                prepared.body,
            )
        } else {
            let (url, target, headers) =
                match transport(&state.catalog, &candidate, source, stream, &client_headers) {
                    Ok(config) => config,
                    Err(error) => {
                        last_error = error;
                        continue;
                    }
                };
            if stream && source != target {
                last_error = ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Cross-protocol streaming is not yet ported; use the provider's native endpoint",
                );
                continue;
            }
            let mut transformed = match protocol::request(&body, source, target, &candidate.model) {
                Ok(body) => body,
                Err(_) => {
                    last_error = ApiError::new(
                        StatusCode::NOT_IMPLEMENTED,
                        "Request contains fields not yet supported by the protocol adapter",
                    );
                    continue;
                }
            };
            if candidate.provider == "gemini-cli" {
                transformed = json!({"project":candidate.connection["projectId"],"model":candidate.model,"request":transformed});
            }
            (url, target, headers, transformed)
        };
        let response = match state
            .client
            .post(url)
            .headers(headers)
            .json(&transformed)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                last_error = ApiError::new(
                    if error.is_timeout() {
                        StatusCode::GATEWAY_TIMEOUT
                    } else {
                        StatusCode::BAD_GATEWAY
                    },
                    "Upstream connection failed",
                );
                state
                    .cooldowns
                    .lock()
                    .map_err(|_| ApiError::internal())?
                    .insert(cooldown_key, Instant::now() + Duration::from_secs(5));
                continue;
            }
        };
        let status = response.status();
        if !status.is_success() {
            let retry = matches!(status.as_u16(), 401 | 403 | 408 | 409 | 425 | 429)
                || status.is_server_error();
            let seconds = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(if status.as_u16() == 429 { 60 } else { 5 })
                .clamp(1, 3600);
            last_error = ApiError::new(
                if status.is_redirection() {
                    StatusCode::BAD_GATEWAY
                } else {
                    status
                },
                "Upstream rejected the request",
            );
            if !retry {
                rejected_models.insert((candidate.provider.clone(), candidate.model.clone()));
                continue;
            }
            state
                .cooldowns
                .lock()
                .map_err(|_| ApiError::internal())?
                .insert(cooldown_key, Instant::now() + Duration::from_secs(seconds));
            continue;
        }
        if stream {
            if !response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|s| s.starts_with("text/event-stream"))
            {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "Upstream did not return an SSE stream",
                ));
            }
            if candidate.provider == "gemini-cli" {
                return Ok((
                    status,
                    [
                        ("content-type", "text/event-stream"),
                        ("x-accel-buffering", "no"),
                    ],
                    gemini_cli_stream(response),
                )
                    .into_response());
            }
            // ponytail: same-protocol SSE is relayed byte-for-byte; add translators before enabling cross-protocol streams.
            // Body owns the upstream stream: backpressure is pull-based and disconnect drops it.
            let stream = response
                .bytes_stream()
                .map(|chunk| chunk.map_err(|_| io::Error::other("Upstream stream interrupted")));
            return Ok((
                status,
                [
                    ("content-type", "text/event-stream"),
                    ("x-accel-buffering", "no"),
                ],
                Body::from_stream(stream),
            )
                .into_response());
        }
        if candidate.provider == "codex" {
            let native = crate::codex::aggregate(response).await.map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "Codex returned an invalid or incomplete response",
                )
            })?;
            let value = if source == Format::OpenAi {
                crate::codex::to_chat(&native).map_err(|_| {
                    ApiError::new(
                        StatusCode::NOT_IMPLEMENTED,
                        "Codex response cannot be represented by this Chat adapter",
                    )
                })?
            } else {
                native
            };
            return Ok((status, axum::Json(value)).into_response());
        }
        let mut bytes = Vec::new();
        let mut chunks = response.bytes_stream();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|_| {
                ApiError::new(StatusCode::BAD_GATEWAY, "Upstream response interrupted")
            })?;
            if bytes.len() + chunk.len() > MAX_RESPONSE {
                return Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "Upstream JSON exceeds 32 MiB",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let mut value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::new(StatusCode::BAD_GATEWAY, "Upstream returned invalid JSON")
        })?;
        if candidate.provider == "gemini-cli"
            && let Some(response) = value.get("response")
        {
            value = response.clone();
        }
        let translated =
            protocol::response(&value, target, source, &candidate.model).map_err(|_| {
                ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Upstream response contains unsupported protocol fields",
                )
            })?;
        return Ok((status, axum::Json(translated)).into_response());
    }
    Err(last_error)
}

fn muse_error(error: Box<dyn std::error::Error + Send + Sync>) -> ApiError {
    let message = error.to_string();
    let status = if message.contains("expired") || message.contains("requires browser login") {
        StatusCode::UNAUTHORIZED
    } else if message.contains("quota") {
        StatusCode::TOO_MANY_REQUESTS
    } else if message.contains("timed out") {
        StatusCode::GATEWAY_TIMEOUT
    } else if message.contains("No Chrome") {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::BAD_GATEWAY
    };
    ApiError::new(status, &message)
}

fn gemini_cli_stream(response: reqwest::Response) -> Body {
    let stream = futures_util::stream::try_unfold(
        (response.bytes_stream(), Vec::<u8>::new()),
        |(mut source, mut pending)| async move {
            loop {
                if let Some((index, length)) = pending
                    .windows(2)
                    .position(|w| w == b"\n\n")
                    .map(|i| (i, 2))
                    .into_iter()
                    .chain(
                        pending
                            .windows(4)
                            .position(|w| w == b"\r\n\r\n")
                            .map(|i| (i, 4)),
                    )
                    .min_by_key(|(i, _)| *i)
                {
                    let frame = pending.drain(..index + length).collect::<Vec<_>>();
                    let frame = std::str::from_utf8(&frame)
                        .map_err(|_| io::Error::other("Invalid UTF-8 in Gemini stream"))?;
                    let payload = frame
                        .lines()
                        .filter_map(|line| {
                            line.strip_prefix("data:")
                                .map(|s| s.strip_prefix(' ').unwrap_or(s))
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    if payload.is_empty() {
                        continue;
                    }
                    let value: Value = serde_json::from_str(&payload)
                        .map_err(|_| io::Error::other("Invalid JSON in Gemini stream"))?;
                    let value = value.get("response").unwrap_or(&value);
                    let output = format!("data: {value}\n\n");
                    return Ok::<_, io::Error>(Some((output, (source, pending))));
                }
                match source.next().await {
                    Some(Ok(bytes)) => {
                        if pending.len() + bytes.len() > MAX_RESPONSE {
                            return Err(io::Error::other("Gemini SSE event exceeds32MiB"));
                        }
                        pending.extend_from_slice(&bytes);
                    }
                    Some(Err(_)) => return Err(io::Error::other("Gemini stream interrupted")),
                    None => {
                        return if pending.iter().all(u8::is_ascii_whitespace) {
                            Ok(None)
                        } else {
                            Err(io::Error::other("Gemini stream ended during an SSE event"))
                        };
                    }
                }
            }
        },
    );
    Body::from_stream(stream)
}

async fn muse_thread(
    state: &Arc<Gateway>,
    candidate: &Candidate,
) -> ApiResult<crate::muse::Thread> {
    let raw = candidate.connection["apiKey"].as_str().unwrap_or_default();
    let cookie = crate::muse::renew(&state.client, raw)
        .await
        .map_err(muse_error)?;
    let id = candidate.connection["id"].clone();
    let saved_cookie = cookie.clone();
    state
        .db(move |s| s.save_connection(&json!({"id":id,"apiKey":saved_cookie})))
        .await?;
    crate::muse::Thread::open(&state.data_dir, &cookie)
        .await
        .map_err(muse_error)
}

async fn muse_chat(
    state: Arc<Gateway>,
    candidate: &Candidate,
    body: &Value,
    source: Format,
    stream: bool,
) -> ApiResult<Response> {
    for key in ["tools", "functions"] {
        if let Some(tools) = body.get(key)
            && !tools.is_null()
            && tools.as_array().is_none_or(|a| !a.is_empty())
        {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Muse Web does not support function tools",
            ));
        }
    }
    if stream && source != Format::OpenAi {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Muse Web streaming currently requires the OpenAI chat endpoint",
        ));
    }
    let chat = protocol::request(body, source, Format::OpenAi, &candidate.model).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Request cannot be represented as a Muse Web chat",
        )
    })?;
    let messages = chat["messages"]
        .as_array()
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "messages required"))?;
    let (prompt, images) = crate::muse::flatten(messages)
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, &error.to_string()))?;
    let mut thread = muse_thread(&state, candidate).await?;
    thread.send(&prompt, &images).await.map_err(muse_error)?;
    let model = candidate.model.clone();
    let created = crate::muse::now();
    let id = format!("chatcmpl-muw-{}", uuid::Uuid::new_v4().simple());
    if stream {
        let output = futures_util::stream::unfold(Some(thread), move |thread| {
            let model = model.clone();
            let id = id.clone();
            async move {
                let mut thread = thread?;
                let (event, next) = match thread.delta().await {
                    Ok(Some(delta)) => (
                        format!(
                            "data: {}\n\n",
                            json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{"content":delta},"finish_reason":null}]})
                        ),
                        Some(thread),
                    ),
                    Ok(None) => (
                        format!(
                            "data: {}\n\ndata: [DONE]\n\n",
                            json!({"id":id,"object":"chat.completion.chunk","created":created,"model":model,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
                        ),
                        None,
                    ),
                    Err(error) => (
                        format!(
                            "data: {}\n\ndata: [DONE]\n\n",
                            json!({"error":{"message":error.to_string()}})
                        ),
                        None,
                    ),
                };
                Some((Ok::<_, std::convert::Infallible>(event), next))
            }
        });
        return Ok((
            [
                ("content-type", "text/event-stream"),
                ("cache-control", "no-cache"),
                ("x-accel-buffering", "no"),
            ],
            Body::from_stream(output),
        )
            .into_response());
    }
    let text = thread.complete().await.map_err(muse_error)?;
    let result = json!({"id":id,"object":"chat.completion","created":created,"model":model,"choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop","logprobs":null}],"usage":{"prompt_tokens":0,"completion_tokens":0,"total_tokens":0}});
    let translated = protocol::response(&result, Format::OpenAi, source, &candidate.model)
        .map_err(|_| ApiError::unsupported())?;
    Ok(axum::Json(translated).into_response())
}

async fn muse_media(
    state: Arc<Gateway>,
    principal: Principal,
    request: Request,
    video: bool,
) -> ApiResult<Response> {
    let body = json_body(request).await?;
    let model = required_text(&body, "model", 4096)?;
    let prompt = required_text(&body, "prompt", 1024 * 1024)?;
    let catalog = state.catalog.clone();
    let selected = model.clone();
    let (proxy_required, candidates) = state
        .db(move |s| {
            if !allowed(s, &catalog, &principal, &selected)? {
                return Err(
                    io::Error::new(io::ErrorKind::PermissionDenied, "Model access denied").into(),
                );
            }
            let proxy_required = s.settings()?["outboundProxyEnabled"] == true;
            let mut candidates = Vec::new();
            expand(s, &catalog, &selected, &mut Vec::new(), &mut candidates)?;
            Ok((proxy_required, candidates))
        })
        .await?;
    if proxy_required {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Global outbound proxy is configured but its Rust transport is not yet ported",
        ));
    }
    let count = body["n"].as_u64().unwrap_or(1);
    if !(1..=4).contains(&count) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "n must be between 1 and 4",
        ));
    }
    let size = body["size"].as_str().unwrap_or_default();
    let seconds = body["seconds"]
        .as_u64()
        .or_else(|| body["duration"].as_u64())
        .unwrap_or(0);
    let images = match body.get("image").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => vec![s.clone()],
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "image must be a data URL or HTTPS URL",
            ));
        }
        None => Vec::new(),
    };
    let kind = if video { "video" } else { "image" };
    let mut hints = Vec::new();
    if !size.is_empty() {
        hints.push(format!("aspect ratio {size}"));
    }
    if seconds > 0 {
        hints.push(format!("{seconds} seconds long"));
    }
    let verb = if video && !images.is_empty() {
        "Animate the attached image into a video"
    } else if video {
        "Generate a video"
    } else {
        "Generate an image"
    };
    let prompt = format!(
        "{verb}{}: {prompt}",
        if hints.is_empty() {
            String::new()
        } else {
            format!(" ({})", hints.join(", "))
        }
    );
    let mut last = ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "No active Muse Web account is connected",
    );
    for candidate in candidates {
        if let Err(error) = connection_proxy(&candidate.connection) {
            last = error;
            continue;
        }
        if candidate.provider != "muse-web" {
            last = ApiError::unsupported();
            continue;
        }
        let mut media = Vec::new();
        let mut failed = false;
        for _ in 0..count {
            let result = async {
                let mut thread = muse_thread(&state, &candidate).await?;
                thread.send(&prompt, &images).await.map_err(muse_error)?;
                thread.media(kind).await.map_err(muse_error)
            }
            .await;
            match result {
                Ok(value) => media.push(value),
                Err(error) => {
                    last = error;
                    failed = true;
                    break;
                }
            }
        }
        if failed {
            continue;
        }
        return Ok(axum::Json(if video {
            json!({"created":crate::muse::now(),"data":media.into_iter().map(|v|json!({"b64_json":v["b64"],"mime_type":v["mime"],"width":v["width"],"height":v["height"]})).collect::<Vec<_>>()})
        }else {
            json!({"created":crate::muse::now(),"data":media.into_iter().map(|v|json!({"b64_json":v["b64"],"mime_type":v["mime"]})).collect::<Vec<_>>()})
        }).into_response());
    }
    Err(last)
}

fn transport(
    catalog: &Catalog,
    candidate: &Candidate,
    source: Format,
    stream: bool,
    client_headers: &HeaderMap,
) -> ApiResult<(reqwest::Url, Format, HeaderMap)> {
    let credentials = &candidate.connection;
    if credentials["authType"]
        .as_str()
        .is_some_and(|s| !matches!(s, "apikey" | "none" | "oauth"))
    {
        return Err(ApiError::unsupported());
    }
    connection_proxy(credentials)?;
    let provider = catalog.provider(&candidate.provider);
    let config = provider.and_then(|p| p.transport.as_ref());
    let node = candidate.node.as_ref();
    let custom_type = node.and_then(|n| n["type"].as_str());
    if node.is_none()
        && (config.is_none()
            || config
                .is_some_and(|c| c["executor"] != "default" && candidate.provider != "gemini-cli")
            || !provider.is_some_and(|p| {
                p.service_kinds.is_empty() || p.service_kinds.iter().any(|k| k == "llm")
            }))
    {
        return Err(ApiError::unsupported());
    }
    let format = match custom_type
        .unwrap_or_else(|| provider.and_then(|p| p.format.as_deref()).unwrap_or(""))
    {
        "openai" | "openai-compatible" => Format::OpenAi,
        "claude" | "anthropic-compatible" => Format::Claude,
        "gemini" | "gemini-cli" => Format::Gemini,
        "openai-responses" => Format::Responses,
        _ => return Err(ApiError::unsupported()),
    };
    let mut target = if candidate.provider == "openai" && source == Format::Responses {
        Format::Responses
    } else {
        format
    };
    let base_override = node
        .and_then(|n| n["baseUrl"].as_str())
        .or_else(|| credentials["providerSpecificData"]["baseUrl"].as_str())
        .filter(|s| !s.is_empty());
    let mut base = base_override
        .or_else(|| config.and_then(|c| c["baseUrl"].as_str()))
        .ok_or_else(ApiError::unsupported)?
        .trim_end_matches('/')
        .to_owned();
    if base.contains("{accountId}") {
        let account = credentials["providerSpecificData"]["accountId"]
            .as_str()
            .filter(|s| {
                !s.is_empty()
                    && s.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            })
            .ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "A valid accountId is required")
            })?;
        base = base.replace("{accountId}", account);
    }
    if base_override.is_some() && format != Format::Gemini {
        if node.is_some_and(|n| n["apiType"] == "responses") {
            target = Format::Responses;
        }
        let suffix = match target {
            Format::OpenAi => "/chat/completions",
            Format::Claude => "/messages",
            Format::Responses => "/responses",
            Format::Gemini => unreachable!(),
        };
        if !base.ends_with(suffix) {
            base.push_str(suffix);
        }
    } else if candidate.provider == "openai" && source == Format::Responses {
        base = base.trim_end_matches("/chat/completions").to_owned() + "/responses";
        target = Format::Responses;
    }
    let mut url = reqwest::Url::parse(&base)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Invalid provider endpoint URL"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(url.scheme(), "http" | "https")
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Provider endpoints must use HTTP(S), without URL credentials or fragments",
        ));
    }
    if url.scheme() == "http"
        && !url.host_str().is_some_and(|host| {
            host == "localhost"
                || host == "[::1]"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Non-local provider endpoints require HTTPS",
        ));
    }
    if candidate.provider == "gemini-cli" {
        url.set_path(&format!(
            "{}:{}",
            url.path().trim_end_matches('/'),
            if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            }
        ));
        if stream {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
    } else if target == Format::Gemini {
        let model = &candidate.model;
        if model
            .chars()
            .any(|c| c.is_control() || matches!(c, '?' | '#' | '%'))
            || model.split('/').any(|s| s == "." || s == "..")
        {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Invalid Gemini model id",
            ));
        }
        url.set_path(&format!(
            "{}/{}:{}",
            url.path().trim_end_matches('/'),
            model,
            if stream {
                "streamGenerateContent"
            } else {
                "generateContent"
            }
        ));
        if stream {
            url.query_pairs_mut().append_pair("alt", "sse");
        }
    } else if let Some(suffix) = config
        .and_then(|c| c["urlSuffix"].as_str())
        .filter(|_| base_override.is_none())
    {
        url = reqwest::Url::parse(&(url.to_string() + suffix))
            .map_err(|_| ApiError::unsupported())?;
    }
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    if stream {
        headers.insert("accept", HeaderValue::from_static("text/event-stream"));
    }
    if candidate.provider == "gemini-cli" {
        headers.insert(
            "x-goog-api-client",
            HeaderValue::from_static("google-genai-sdk/1.41.0 gl-node/v22.19.0"),
        );
        let user_agent = format!(
            "GeminiCLI/0.34.0/{} ({}; {}; terminal)",
            candidate.model,
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        headers.insert(
            "user-agent",
            HeaderValue::try_from(user_agent).map_err(|_| {
                ApiError::new(StatusCode::BAD_REQUEST, "Invalid Gemini model identifier")
            })?,
        );
    }
    if let Some(overrides) = config.and_then(|c| c["headers"].as_object()) {
        for (name, value) in overrides {
            if let Some(value) = value.as_str() {
                let name =
                    HeaderName::try_from(name.as_str()).map_err(|_| ApiError::unsupported())?;
                let value = HeaderValue::try_from(value).map_err(|_| ApiError::unsupported())?;
                headers.insert(name, value);
            }
        }
    }
    apply_headers(&mut headers, &candidate.headers)?;
    let auth = config.and_then(|c| c.get("auth"));
    if auth
        .and_then(|a| a["hooks"].as_array())
        .is_some_and(|hooks| !hooks.is_empty())
    {
        return Err(ApiError::unsupported());
    }
    let oauth = credentials["authType"] == "oauth";
    let api_key = credentials[if oauth { "accessToken" } else { "apiKey" }]
        .as_str()
        .unwrap_or_default();
    if !api_key.is_empty() {
        let desc = auth.and_then(|a| {
            if a["combined"] == true {
                Some(a)
            } else {
                a.get(if oauth { "oauth" } else { "apiKey" })
            }
        });
        let name = desc.and_then(|d| d["header"].as_str()).unwrap_or(if oauth {
            "authorization"
        } else if target == Format::Claude {
            "x-api-key"
        } else if target == Format::Gemini {
            "x-goog-api-key"
        } else {
            "authorization"
        });
        let scheme = desc.and_then(|d| d["scheme"].as_str()).unwrap_or(
            if oauth || target == Format::OpenAi || target == Format::Responses {
                "bearer"
            } else {
                "raw"
            },
        );
        let value = if scheme == "bearer" {
            format!("Bearer {api_key}")
        } else {
            api_key.to_owned()
        };
        headers.insert(
            HeaderName::try_from(name).map_err(|_| ApiError::unsupported())?,
            HeaderValue::try_from(value)
                .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "Invalid credential header"))?,
        );
    } else if credentials["authType"] != "none" {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            if oauth {
                "Provider login is missing its access token; sign in again"
            } else {
                "Provider connection has no API key"
            },
        ));
    }
    if target == Format::Claude {
        if !headers.contains_key("anthropic-version") {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        }
        if let Some(beta) = client_headers.get("anthropic-beta") {
            headers.insert("anthropic-beta", beta.clone());
        }
    }
    Ok((url, target, headers))
}

fn connection_proxy(connection: &Value) -> ApiResult<()> {
    if connection["providerSpecificData"]["connectionProxyEnabled"] == true
        || connection["providerSpecificData"]["proxyPoolId"].is_string()
    {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Connection proxy selection has not yet been ported",
        ));
    }
    Ok(())
}

fn apply_headers(headers: &mut HeaderMap, value: &Value) -> ApiResult<()> {
    if let Some(overrides) = value.as_object() {
        if overrides.len() > 20 {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Too many provider override headers",
            ));
        }
        for (name, value) in overrides {
            if [
                "host",
                "content-length",
                "content-type",
                "connection",
                "transfer-encoding",
                "authorization",
                "cookie",
                "x-api-key",
                "x-goog-api-key",
                "proxy-authorization",
            ]
            .contains(&name.to_ascii_lowercase().as_str())
            {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Credential and routing headers cannot be overridden",
                ));
            }
            let value = value.as_str().filter(|s| s.len() <= 8192).ok_or_else(|| {
                ApiError::new(StatusCode::BAD_REQUEST, "Invalid override header value")
            })?;
            headers.insert(
                HeaderName::try_from(name).map_err(|_| {
                    ApiError::new(StatusCode::BAD_REQUEST, "Invalid override header name")
                })?,
                HeaderValue::try_from(value).map_err(|_| {
                    ApiError::new(StatusCode::BAD_REQUEST, "Invalid override header value")
                })?,
            );
        }
    }
    Ok(())
}
