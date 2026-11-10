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
}

impl Gateway {
    pub fn new(store: Store, admin_token: String) -> Result<Arc<Self>> {
        if admin_token.len() < 32 || admin_token.chars().any(char::is_whitespace) {
            return Err(io::Error::other(
                "Admin token must contain at least 32 non-whitespace characters",
            )
            .into());
        }
        Ok(Arc::new(Self {
            store: Arc::new(Mutex::new(store)),
            catalog: Arc::new(Catalog::load()?),
            admin_token,
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(90))
                .no_proxy()
                .build()?,
            cooldowns: Mutex::new(HashMap::new()),
        }))
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
    management(state, request).await
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

async fn management(state: Arc<Gateway>, request: Request) -> ApiResult<Response> {
    let path = request.uri().path().trim_end_matches('/').to_owned();
    let method = request.method().clone();
    if path == "/api/status" && method == Method::GET {
        return Ok(axum::Json(json!({"version":env!("CARGO_PKG_VERSION"),"engine":"rust","featureComplete":false,
            "implemented":["sqlite","model-resolution","connections","aliases","combos","api-keys","generic-api-key-transports","same-protocol-streaming"],
            "remaining":["oauth-browser-flows","specialized-executors","cross-protocol-streaming","media","sso","tunnels-mitm","rtk","mcp","full-upstream-ui-parity"]})).into_response());
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
            "/api/provider-nodes" => json!({"nodes":state.db(|s| s.nodes()).await?}),
            "/api/combos" => json!({"combos":state.db(|s| s.combos()).await?}),
            "/api/keys" => {
                let mut keys = state.db(|s| s.api_keys()).await?;
                for key in &mut keys {
                    key.as_object_mut()
                        .expect("stored key object")
                        .remove("key");
                }
                json!({"apiKeys":keys})
            }
            "/api/models/alias" => json!({"aliases":state.db(|s| s.aliases()).await?}),
            "/api/settings" => state.db(|s| s.settings()).await?,
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
        "/api/combos" => json!({"combo":state.db(move |s| s.save_combo(&body)).await?}),
        "/api/keys" => {
            if body.get("key").is_none() && body.get("id").is_none() {
                body["key"] = json!(format!(
                    "sk-{}{}",
                    uuid::Uuid::new_v4().simple(),
                    uuid::Uuid::new_v4().simple()
                ));
            }
            json!({"apiKey":state.db(move |s| s.save_api_key(&body)).await?})
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
            state.db(move |s| s.save_settings(&body)).await?;
            json!({"success":true})
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
    for candidate in candidates {
        if rejected_models.contains(&(candidate.provider.clone(), candidate.model.clone())) {
            continue;
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
        let transformed = match protocol::request(&body, source, target, &candidate.model) {
            Ok(body) => body,
            Err(_) => {
                last_error = ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    "Request contains fields not yet supported by the protocol adapter",
                );
                continue;
            }
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
        let value = serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::new(StatusCode::BAD_GATEWAY, "Upstream returned invalid JSON")
        })?;
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
        .is_some_and(|s| s != "apikey" && s != "none")
    {
        return Err(ApiError::unsupported());
    }
    if credentials["providerSpecificData"]["connectionProxyEnabled"] == true
        || credentials["providerSpecificData"]["proxyPoolId"].is_string()
    {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Connection proxy selection has not yet been ported",
        ));
    }
    let provider = catalog.provider(&candidate.provider);
    let config = provider.and_then(|p| p.transport.as_ref());
    let node = candidate.node.as_ref();
    let custom_type = node.and_then(|n| n["type"].as_str());
    if node.is_none()
        && (config.is_none()
            || config.is_some_and(|c| c["executor"] != "default")
            || !provider.is_some_and(|p| p.service_kinds.iter().any(|k| k == "llm")))
    {
        return Err(ApiError::unsupported());
    }
    let format = match custom_type
        .unwrap_or_else(|| provider.and_then(|p| p.format.as_deref()).unwrap_or(""))
    {
        "openai" | "openai-compatible" => Format::OpenAi,
        "claude" | "anthropic-compatible" => Format::Claude,
        "gemini" => Format::Gemini,
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
    if target == Format::Gemini {
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
    let auth = config.and_then(|c| c.get("auth"));
    if auth
        .and_then(|a| a["hooks"].as_array())
        .is_some_and(|hooks| !hooks.is_empty())
    {
        return Err(ApiError::unsupported());
    }
    let api_key = credentials["apiKey"].as_str().unwrap_or_default();
    if !api_key.is_empty() {
        let desc = auth.and_then(|a| {
            if a["combined"] == true {
                Some(a)
            } else {
                a.get("apiKey")
            }
        });
        let name = desc
            .and_then(|d| d["header"].as_str())
            .unwrap_or(if target == Format::Claude {
                "x-api-key"
            } else if target == Format::Gemini {
                "x-goog-api-key"
            } else {
                "authorization"
            });
        let scheme = desc.and_then(|d| d["scheme"].as_str()).unwrap_or(
            if target == Format::OpenAi || target == Format::Responses {
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
            "Provider connection has no API key",
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
