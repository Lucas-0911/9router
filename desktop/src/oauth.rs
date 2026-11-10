//! Native implementations of the upstream OAuthModal wire contract. Client descriptors
//! are public upstream application configuration, captured by scripts/snapshot-oauth.mjs.
use crate::Store;
use axum::{
    Router,
    extract::{Query, State},
    response::Html,
    routing::get,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use reqwest::{Client, RequestBuilder, Url};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const TTL: Duration = Duration::from_secs(600);
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub struct Error {
    pub status: u16,
    pub message: String,
}
impl Error {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
fn internal(_: impl fmt::Display) -> Error {
    Error::new(500, "OAuth storage operation failed")
}
fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}
fn nonempty(value: &Value, key: &str) -> Result<String> {
    let s = text(value, key);
    if s.is_empty() || s.len() > 256 * 1024 || s.contains('\0') {
        Err(Error::new(400, format!("Missing or invalid {key}")))
    } else {
        Ok(s.to_owned())
    }
}
fn scope(config: &Value) -> String {
    config["scopes"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_else(|| text(config, "scope").to_owned())
}
fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}
fn epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn jwt(token: &str) -> Value {
    token
        .split('.')
        .nth(1)
        .and_then(|s| URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}
fn endpoint(raw: &str) -> Result<Url> {
    let u = Url::parse(raw).map_err(|_| Error::new(400, "Invalid OAuth endpoint"))?;
    let local = u.host_str().is_some_and(|h| {
        h == "localhost"
            || h == "[::1]"
            || h.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if (!u.username().is_empty())
        || u.password().is_some()
        || u.fragment().is_some()
        || !(u.scheme() == "https" || (u.scheme() == "http" && local))
    {
        return Err(Error::new(
            400,
            "OAuth endpoints require HTTPS or loopback HTTP, without credentials",
        ));
    }
    Ok(u)
}
fn redirect(raw: &str) -> Result<String> {
    let u = endpoint(raw)?;
    if !u
        .host_str()
        .is_some_and(|h| h == "localhost" || h == "127.0.0.1" || h == "[::1]")
        || u.scheme() != "http"
        || u.query().is_some()
    {
        return Err(Error::new(
            400,
            "OAuth redirect must be a local HTTP callback",
        ));
    }
    Ok(u.to_string())
}
fn url(raw: &str, pairs: &[(&str, String)]) -> Result<String> {
    let mut u = endpoint(raw)?;
    for (k, v) in pairs {
        u.query_pairs_mut().append_pair(k, v);
    }
    Ok(u.into())
}
fn summary(v: &Value) -> Value {
    json!({"id":v["id"],"provider":v["provider"],"email":v["email"],"displayName":v["displayName"]})
}

#[derive(Clone)]
struct Session {
    provider: String,
    config: Value,
    redirect: String,
    verifier: String,
    expires: Instant,
    status: Value,
    device: Value,
    in_flight: bool,
}
#[derive(Clone)]
pub struct Service {
    store: Arc<Mutex<Store>>,
    client: Client,
    contract: Arc<Value>,
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    listeners: Arc<Mutex<HashMap<String, tokio::task::AbortHandle>>>,
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
}
impl Service {
    pub fn new(store: Arc<Mutex<Store>>, client: Client) -> Self {
        Self::with_contract(
            store,
            client,
            serde_json::from_str(include_str!("../oauth-contract.json"))
                .expect("checked public OAuth contract"),
        )
    }
    /// The supplied contract is trusted application configuration, never an HTTP payload.
    pub fn with_contract(store: Arc<Mutex<Store>>, client: Client, contract: Value) -> Self {
        Self {
            store,
            client,
            contract: Arc::new(contract),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            listeners: Arc::new(Mutex::new(HashMap::new())),
            refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
    fn descriptor(&self, provider: &str) -> Result<Value> {
        self.contract["providers"]
            .get(provider)
            .cloned()
            .ok_or_else(|| Error::new(400, format!("Unknown OAuth provider: {provider}")))
    }
    fn insert(&self, key: String, session: Session) -> Result<()> {
        let mut sessions = self.sessions.lock().map_err(internal)?;
        sessions.retain(|_, s| s.expires > Instant::now());
        if sessions.len() >= 128 {
            return Err(Error::new(429, "Too many pending login sessions"));
        }
        sessions.insert(key, session);
        Ok(())
    }
    fn session(&self, key: &str, provider: Option<&str>) -> Result<Session> {
        self.sessions
            .lock()
            .map_err(internal)?
            .get(key)
            .filter(|s| s.expires > Instant::now() && provider.is_none_or(|p| p == s.provider))
            .cloned()
            .ok_or_else(|| Error::new(400, "OAuth session expired or invalid; restart login"))
    }
    pub fn callback_status(&self, state: &str) -> Result<Value> {
        Ok(self
            .sessions
            .lock()
            .map_err(internal)?
            .get(state)
            .filter(|s| s.expires > Instant::now())
            .map(|s| s.status.clone())
            .unwrap_or(json!({"status":"unknown"})))
    }
    pub async fn handle(
        &self,
        provider: &str,
        action: &str,
        method: &str,
        query: &HashMap<String, String>,
        body: Value,
    ) -> Result<Value> {
        let provider = if provider == "kimi-coding" {
            "kimi"
        } else {
            provider
        };
        if provider.len() > 128
            || query.values().any(|v| v.len() > 8192)
            || serde_json::to_vec(&body).map_err(internal)?.len() > MAX_RESPONSE
        {
            return Err(Error::new(400, "OAuth request exceeds size limit"));
        }
        match (method, action) {
            ("GET", "authorize") => self.authorize(provider, query).await,
            ("GET", "device-code") => self.device_code(provider, query).await,
            ("POST", "exchange") => self.exchange(provider, &body).await,
            ("POST", "poll") => self.poll(provider, &body).await,
            ("GET", "poll-status") => {
                let state = query
                    .get("state")
                    .ok_or_else(|| Error::new(400, "Missing state"))?;
                self.session(state, Some(provider))?;
                self.callback_status(state)
            }
            ("GET", "start-proxy") => self.start_proxy(provider, query).await,
            ("GET", "stop-proxy") | ("POST", "cancel") => {
                self.cancel(provider)?;
                Ok(json!({"success":true}))
            }
            ("POST", "manual-code") if provider == "xai" => {
                let state = nonempty(&body, "state")?;
                let s = self.session(&state, Some(provider))?;
                self.exchange(provider,&json!({"code":body["code"],"state":state,"codeVerifier":s.verifier,"redirectUri":s.redirect})).await
            }
            ("GET", "ide-status") => Ok(json!({"installed":false,"path":null})),
            _ => Err(Error::new(
                501,
                format!(
                    "Native {provider} {action} is not implemented; no credentials were imported"
                ),
            )),
        }
    }
    /// Stop native listeners and invalidate attempts before desktop shutdown.
    pub fn shutdown(&self) {
        if let Ok(mut listeners) = self.listeners.lock() {
            for (_, task) in listeners.drain() {
                task.abort();
            }
        }
        if let Ok(mut sessions) = self.sessions.lock() {
            sessions.clear();
        }
    }
    fn cancel(&self, provider: &str) -> Result<()> {
        if let Some(task) = self.listeners.lock().map_err(internal)?.remove(provider) {
            task.abort()
        }
        let mut sessions = self.sessions.lock().map_err(internal)?;
        for s in sessions
            .values_mut()
            .filter(|s| s.provider == provider && s.status["status"] != "done")
        {
            s.status = json!({"status":"error","error":"Login cancelled"});
            s.in_flight = false;
        }
        Ok(())
    }
    async fn read(&self, request: RequestBuilder) -> Result<(u16, Value)> {
        let response = request
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|_| Error::new(502, "OAuth provider request failed or timed out"))?;
        self.decode(response).await
    }
    async fn decode(&self, response: reqwest::Response) -> Result<(u16, Value)> {
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(Error::new(502, "OAuth provider response too large"));
        }
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| Error::new(502, "OAuth provider response interrupted"))?
        {
            if bytes.len() + chunk.len() > MAX_RESPONSE {
                return Err(Error::new(502, "OAuth provider response too large"));
            }
            bytes.extend_from_slice(&chunk)
        }
        let v = serde_json::from_slice(&bytes).map_err(|_| {
            Error::new(
                502,
                format!("OAuth provider returned invalid JSON (HTTP {status})"),
            )
        })?;
        Ok((status, v))
    }
    async fn required(&self, request: RequestBuilder) -> Result<Value> {
        let (status, v) = self.read(request).await?;
        if !(200..300).contains(&status) {
            let code = text(&v, "error");
            let safe = if !code.is_empty()
                && code.len() < 80
                && code.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                format!(": {code}")
            } else {
                String::new()
            };
            return Err(Error::new(
                502,
                format!("OAuth provider rejected request (HTTP {status}){safe}"),
            ));
        }
        Ok(v)
    }
    async fn form(&self, config: &Value, fields: Value, json_body: bool) -> Result<Value> {
        let raw = text(config, "tokenUrl");
        let mut req = self
            .client
            .post(endpoint(raw)?)
            .header("Accept", "application/json");
        if json_body {
            req = req.json(&fields)
        } else {
            req = req.form(&fields)
        }
        if text(config, "authStyle") == "basic" {
            req = req.basic_auth(text(config, "clientId"), Some(text(config, "clientSecret")))
        }
        self.required(req).await
    }
    async fn authorize(&self, provider: &str, query: &HashMap<String, String>) -> Result<Value> {
        let desc = self.descriptor(provider)?;
        let flow = text(&desc, "flowType");
        if matches!(
            provider,
            "cursor" | "zed" | "xiaomi-mimo" | "trae" | "windsurf"
        ) {
            return Err(Error::new(
                501,
                format!(
                    "{provider} requires a specialized native login adapter; use another configured connection"
                ),
            ));
        }
        let mut config = desc["config"].clone();
        let redirect = redirect(
            query
                .get("redirect_uri")
                .map(String::as_str)
                .unwrap_or("http://localhost:8080/callback"),
        )?;
        if provider == "gitlab" {
            let base = query
                .get("baseUrl")
                .cloned()
                .unwrap_or_else(|| text(&config, "defaultBaseUrl").to_owned());
            let base = endpoint(&base)?
                .to_string()
                .trim_end_matches('/')
                .to_owned();
            let client = query
                .get("clientId")
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::new(400, "GitLab OAuth clientId is required"))?;
            for (key, path) in [
                ("authorizeUrl", "authorizeUrlPath"),
                ("tokenUrl", "tokenUrlPath"),
                ("userInfoUrl", "userInfoUrlPath"),
            ] {
                config[key] = json!(format!("{base}{}", text(&config, path)))
            }
            config["baseUrl"] = json!(base);
            config["clientId"] = json!(client);
            config["clientSecret"] = query
                .get("clientSecret")
                .cloned()
                .unwrap_or_default()
                .into();
        }
        if provider == "xai"
            && let Ok(discovered) = self
                .required(self.client.get(endpoint(text(&config, "discoveryUrl"))?))
                .await
        {
            for (field, key) in [
                ("authorization_endpoint", "authorizeUrl"),
                ("token_endpoint", "tokenUrl"),
            ] {
                if let Ok(u) = endpoint(text(&discovered, field))
                    && u.scheme() == "https"
                    && u.host_str()
                        .is_some_and(|h| h == "x.ai" || h.ends_with(".x.ai"))
                {
                    config[key] = json!(u.to_string())
                }
            }
        }
        let state = random();
        let verifier = if provider == "xai" {
            format!("{}{}", random(), random())
        } else {
            random()
        };
        let challenge = challenge(&verifier);
        let mut fields = vec![
            ("client_id", text(&config, "clientId").to_owned()),
            ("response_type", "code".to_owned()),
            ("redirect_uri", redirect.clone()),
            ("scope", scope(&config)),
            ("state", state.clone()),
        ];
        let authorize = if flow == "device_code" {
            Value::Null
        } else {
            match provider {
                "iflow" => {
                    fields = vec![
                        (
                            "loginMethod",
                            text(&config["extraParams"], "loginMethod").to_owned(),
                        ),
                        ("type", text(&config["extraParams"], "type").to_owned()),
                        ("redirect", redirect.clone()),
                        ("state", state.clone()),
                        ("client_id", text(&config, "clientId").to_owned()),
                    ]
                }
                "cline" | "clinepass" => {
                    fields = vec![
                        ("client_type", "extension".to_owned()),
                        ("callback_url", redirect.clone()),
                        ("redirect_uri", redirect.clone()),
                    ]
                }
                "kimchi" => fields = vec![("callback", redirect.clone()), ("state", state.clone())],
                "gemini-cli" | "antigravity" => fields.extend([
                    ("access_type", "offline".to_owned()),
                    ("prompt", "consent".to_owned()),
                ]),
                "claude" => fields.push(("code", "true".to_owned())),
                "xai" => fields.extend([
                    ("nonce", uuid::Uuid::new_v4().simple().to_string()),
                    ("plan", "generic".into()),
                    ("referrer", "cli-proxy-api".into()),
                ]),
                _ => {}
            }
            if flow == "authorization_code_pkce" {
                fields.extend([
                    ("code_challenge", challenge.clone()),
                    ("code_challenge_method", "S256".into()),
                ]);
            }
            if provider != "iflow"
                && let Some(extra) = config["extraParams"].as_object()
            {
                for (k, v) in extra {
                    if let Some(v) = v.as_str() {
                        fields.push((k, v.to_owned()))
                    }
                }
            }
            let raw = if provider == "kimchi" {
                format!(
                    "{}/cli-auth",
                    text(&config, "webAppUrl").trim_end_matches('/')
                )
            } else {
                text(&config, "authorizeUrl").to_owned()
            };
            let built = url(&raw, &fields)?;
            // Codex/xAI use encodeURIComponent (%20) rather than URLSearchParams (+).
            json!(if matches!(provider, "codex" | "xai") {
                built.replace('+', "%20")
            } else {
                built
            })
        };
        self.insert(
            state.clone(),
            Session {
                provider: provider.into(),
                config,
                redirect: redirect.clone(),
                verifier: verifier.clone(),
                expires: Instant::now() + TTL,
                status: json!({"status":"pending"}),
                device: Value::Null,
                in_flight: false,
            },
        )?;
        let mut result = json!({"authUrl":authorize,"state":state,"codeVerifier":verifier,"codeChallenge":challenge,"redirectUri":redirect,"flowType":flow,"callbackPath":if provider=="codex"{"/auth/callback"}else{"/callback"}});
        if provider == "codex" {
            result["fixedPort"] = json!(1455)
        } else if provider == "xai" {
            result["fixedPort"] = json!(56121)
        }
        Ok(result)
    }
    fn claim(&self, key: &str, provider: &str) -> Result<Session> {
        let mut sessions = self.sessions.lock().map_err(internal)?;
        let s = sessions
            .get_mut(key)
            .filter(|s| s.provider == provider && s.expires > Instant::now())
            .ok_or_else(|| Error::new(400, "OAuth session expired or invalid; restart login"))?;
        if s.in_flight || s.status["status"] != "pending" {
            return Err(Error::new(
                409,
                "OAuth session was already used or cancelled",
            ));
        }
        s.in_flight = true;
        Ok(s.clone())
    }
    fn finish(&self, key: &str, provider: &str, tokens: Result<Value>) -> Result<Value> {
        let mut sessions = self.sessions.lock().map_err(internal)?;
        let s = sessions
            .get_mut(key)
            .filter(|s| {
                s.provider == provider
                    && s.expires > Instant::now()
                    && s.in_flight
                    && s.status["status"] == "pending"
            })
            .ok_or_else(|| Error::new(409, "OAuth session was cancelled or expired"))?;
        s.in_flight = false;
        match tokens {
            Ok(mut value) => {
                value["provider"] = json!(provider);
                value["authType"] = json!("oauth");
                value["testStatus"] = json!("active");
                let mut store = self.store.lock().map_err(internal)?;
                // Preserve upstream account deduplication without merging two identities.
                if let Some(email) = value["email"].as_str().filter(|s| !s.is_empty())
                    && let Some(existing) = store
                        .connections(Some(provider))
                        .map_err(internal)?
                        .iter()
                        .find(|c| c["email"] == email && c["authType"] == "oauth")
                {
                    value["id"] = existing["id"].clone();
                }
                let connection = store.save_connection(&value).map_err(internal)?;
                let connection = summary(&connection);
                s.status = json!({"status":"done","connection":connection});
                s.verifier.clear();
                s.device = Value::Null;
                Ok(json!({"success":true,"connection":connection}))
            }
            Err(e) => {
                s.status = json!({"status":"error","error":e.message});
                s.verifier.clear();
                s.device = Value::Null;
                Err(e)
            }
        }
    }
    pub async fn callback(&self, query: &HashMap<String, String>) -> Result<Value> {
        let state = query
            .get("state")
            .filter(|s| !s.is_empty() && s.len() < 256)
            .ok_or_else(|| Error::new(400, "Missing OAuth state"))?;
        let s = self.session(state, None)?;
        if query.contains_key("error") {
            self.claim(state, &s.provider)?;
            return self.finish(
                state,
                &s.provider,
                Err(Error::new(
                    400,
                    "Authorization denied or cancelled by provider",
                )),
            );
        }
        let code = query
            .get("code")
            .or_else(|| query.get("token"))
            .ok_or_else(|| Error::new(400, "No authorization code in callback"))?;
        self.exchange(
            &s.provider,
            &json!({"code":code,"state":state,"redirectUri":s.redirect,"codeVerifier":s.verifier}),
        )
        .await
    }
    async fn exchange(&self, provider: &str, body: &Value) -> Result<Value> {
        let mut code = nonempty(body, "code")?;
        let mut state = text(body, "state").to_owned();
        if provider == "claude"
            && let Some((c, s)) = code.split_once('#')
        {
            if !state.is_empty() && state != s {
                return Err(Error::new(400, "OAuth state mismatch"));
            }
            state = s.into();
            code = c.into();
        }
        if state.is_empty() && matches!(provider, "cline" | "clinepass" | "kimchi") {
            let verifier = text(body, "codeVerifier");
            let sessions = self.sessions.lock().map_err(internal)?;
            state = sessions
                .iter()
                .find(|(_, s)| {
                    s.provider == provider
                        && s.verifier == verifier
                        && !verifier.is_empty()
                        && s.status["status"] == "pending"
                })
                .map(|(k, _)| k.clone())
                .unwrap_or_default();
        }
        let s = self.session(&state, Some(provider))?;
        if s.redirect != text(body, "redirectUri") || s.verifier != text(body, "codeVerifier") {
            return Err(Error::new(400, "OAuth redirect or PKCE verifier mismatch"));
        }
        self.claim(&state, provider)?;
        let result = self.exchange_token(provider, &s, &code, &state).await;
        self.finish(&state, provider, result)
    }
    async fn exchange_token(
        &self,
        provider: &str,
        s: &Session,
        code: &str,
        state: &str,
    ) -> Result<Value> {
        let config = &s.config;
        if provider == "kimchi" {
            self.required(
                self.client
                    .get(endpoint(text(config, "validationUrl"))?)
                    .bearer_auth(code),
            )
            .await?;
            let user = self.user_info(config, code).await;
            return Ok(
                json!({"accessToken":code,"refreshToken":null,"email":user["email"],"displayName":user["name"],"providerSpecificData":{"authMethod":"browser_token","userId":user["id"],"username":user["username"]}}),
            );
        }
        if matches!(provider, "cline" | "clinepass") {
            let decoded = STANDARD
                .decode(code)
                .or_else(|_| URL_SAFE_NO_PAD.decode(code));
            let encoded = decoded
                .ok()
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
            let raw = if let Some(data) = encoded.filter(|v| v["accessToken"].is_string()) {
                data
            } else {
                let endpoint_url = config["tokenExchangeUrl"]
                    .as_str()
                    .or(config["tokenUrl"].as_str())
                    .unwrap_or("");
                let data=self.required(self.client.post(endpoint(endpoint_url)?).json(&json!({"grant_type":"authorization_code","code":code,"client_type":"extension","redirect_uri":s.redirect}))).await?;
                data.get("data").cloned().unwrap_or(data)
            };
            let mut result = json!({"accessToken":raw["accessToken"],"refreshToken":raw["refreshToken"],"email":raw["email"].as_str().or(raw["userInfo"]["email"].as_str()),"providerSpecificData":{"firstName":raw["firstName"],"lastName":raw["lastName"]}});
            if let Some(expires) = raw.get("expiresAt") {
                result["expiresAt"] = expires.clone()
            } else {
                result["expiresIn"] = json!(3600)
            }
            nonempty(&result, "accessToken")?;
            return Ok(result);
        }
        let mut fields = json!({"grant_type":"authorization_code","client_id":config["clientId"],"code":code,"redirect_uri":s.redirect});
        if text(config, "codeChallengeMethod") == "S256"
            || matches!(provider, "codex" | "xai" | "claude" | "gitlab")
        {
            fields["code_verifier"] = json!(s.verifier)
        }
        if matches!(provider, "gemini-cli" | "antigravity" | "iflow" | "gitlab")
            && !text(config, "clientSecret").is_empty()
        {
            fields["client_secret"] = config["clientSecret"].clone()
        }
        if provider == "claude" {
            fields["state"] = json!(state)
        }
        let mut config = config.clone();
        if provider == "iflow" {
            config["authStyle"] = json!("basic")
        }
        let tokens = self.form(&config, fields, provider == "claude").await?;
        self.map_tokens(provider, &tokens, &config, &Value::Null)
            .await
    }
    fn device_headers(
        &self,
        req: RequestBuilder,
        provider: &str,
        device: &Value,
    ) -> RequestBuilder {
        match provider {
            "kimi" => req
                .header("X-Msh-Platform", "9router")
                .header("X-Msh-Version", env!("CARGO_PKG_VERSION"))
                .header(
                    "X-Msh-Device-Name",
                    std::env::var("COMPUTERNAME")
                        .or_else(|_| std::env::var("HOSTNAME"))
                        .unwrap_or_else(|_| "9Router".into()),
                )
                .header(
                    "X-Msh-Device-Model",
                    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
                )
                .header("X-Msh-Device-Id", text(device, "_kimiDeviceId")),
            "muse" => req.header("x-api-version", "1.0.0"),
            "grok-cli" => req.header("User-Agent", "grok-cli/1.0.44"),
            _ => req,
        }
    }
    fn buddy_headers(&self, req: RequestBuilder, provider: &str, config: &Value) -> RequestBuilder {
        req.header("User-Agent", text(config, "userAgent"))
            .header("X-Requested-With", "XMLHttpRequest")
            .header(
                "X-Domain",
                if provider == "codebuddy-cn" {
                    "copilot.tencent.com"
                } else {
                    "www.codebuddy.ai"
                },
            )
            .header("X-No-Authorization", "true")
            .header("X-No-User-Id", "true")
            .header("X-No-Enterprise-Id", "true")
            .header("X-No-Department-Info", "true")
            .header("X-Product", "SaaS")
    }
    async fn device_code(&self, provider: &str, query: &HashMap<String, String>) -> Result<Value> {
        let desc = self.descriptor(provider)?;
        if desc["flowType"] != "device_code" {
            return Err(Error::new(
                400,
                "Provider does not support device code flow",
            ));
        }
        let mut config = desc["config"].clone();
        let verifier = random();
        let challenge = challenge(&verifier);
        let mut extra = json!({});
        let mut data = match provider {
            "qoder" | "qoder-cn" => {
                let nonce = uuid::Uuid::new_v4().to_string();
                let machine = uuid::Uuid::new_v4().to_string();
                json!({"device_code":nonce,"user_code":nonce[..8].to_uppercase(),"verification_uri":config["loginUrl"],"verification_uri_complete":url(text(&config,"loginUrl"),&[("challenge",challenge.clone()),("challenge_method","S256".into()),("machine_id",machine.clone()),("nonce",nonce.clone())])?,"expires_in":300,"interval":2,"_qoderNonce":nonce,"_qoderMachineId":machine})
            }
            "kiro" => {
                let region = query
                    .get("region")
                    .map(String::as_str)
                    .unwrap_or("us-east-1");
                valid_region(region)?;
                let base = format!("https://oidc.{region}.amazonaws.com");
                let register=self.required(self.client.post(endpoint(&format!("{base}/client/register"))?).json(&json!({"clientName":config["clientName"],"clientType":config["clientType"],"scopes":config["scopes"],"grantTypes":config["grantTypes"],"issuerUrl":config["issuerUrl"]}))).await?;
                let start = query
                    .get("start_url")
                    .cloned()
                    .unwrap_or_else(|| text(&config, "startUrl").into());
                endpoint(&start)?;
                let d=self.required(self.client.post(endpoint(&format!("{base}/device_authorization"))?).json(&json!({"clientId":register["clientId"],"clientSecret":register["clientSecret"],"startUrl":start}))).await?;
                config["tokenUrl"] = json!(format!("{base}/token"));
                json!({"device_code":d["deviceCode"],"user_code":d["userCode"],"verification_uri":d["verificationUri"],"verification_uri_complete":d["verificationUriComplete"],"expires_in":d["expiresIn"],"interval":d["interval"].as_u64().unwrap_or(5),"_clientId":register["clientId"],"_clientSecret":register["clientSecret"],"_region":region,"_authMethod":if query.get("auth_method").is_some_and(|s|s=="idc"){"idc"}else{"builder-id"},"_startUrl":start})
            }
            "kilocode" => {
                let d = self
                    .required(
                        self.client
                            .post(endpoint(text(&config, "initiateUrl"))?)
                            .header("Content-Type", "application/json"),
                    )
                    .await?;
                json!({"device_code":d["code"],"user_code":d["code"],"verification_uri":d["verificationUrl"],"verification_uri_complete":d["verificationUrl"],"expires_in":d["expiresIn"].as_u64().unwrap_or(300),"interval":3})
            }
            "codebuddy-cn" | "codebuddy-intl" => {
                let raw = url(
                    text(&config, "stateUrl"),
                    &[("platform", text(&config, "platform").to_owned())],
                )?;
                let d = self
                    .required(self.buddy_headers(
                        self.client.post(endpoint(&raw)?).json(&json!({})),
                        provider,
                        &config,
                    ))
                    .await?;
                if d["code"] != 0 {
                    return Err(Error::new(502, "CodeBuddy did not return a login state"));
                }
                json!({"device_code":d["data"]["state"],"verification_uri":d["data"]["authUrl"],"user_code":"","interval":config["pollInterval"].as_u64().unwrap_or(5000)/1000,"_isCodeBuddy":true})
            }
            "glm" => {
                extra["_zcodePollToken"] = json!(random());
                let d = self
                    .required(
                        self.client
                            .post(endpoint(text(&config, "cliInitUrl"))?)
                            .bearer_auth(text(&extra, "_zcodePollToken"))
                            .json(&json!({"provider":config["providerId"]})),
                    )
                    .await?;
                let d = business(d)?;
                json!({"device_code":d["flow_id"],"verification_uri":d["authorize_url"],"expires_in":relative(&d["expires_at"]).unwrap_or(300),"interval":d["poll_interval_sec"].as_u64().unwrap_or(3),"_zcodePollToken":extra["_zcodePollToken"]})
            }
            _ => {
                let mut fields = json!({"client_id":config["clientId"]});
                let scopes = scope(&config);
                if !scopes.is_empty() {
                    fields["scope"] = json!(scopes)
                }
                if !text(&config, "referrer").is_empty() {
                    fields["referrer"] = config["referrer"].clone()
                }
                if provider.starts_with("minimax-code") {
                    fields["audience"] = config["audience"].clone();
                    fields["code_challenge"] = json!(challenge);
                    fields["code_challenge_method"] = json!("S256")
                }
                if provider == "kimi" {
                    extra["_kimiDeviceId"] = json!(uuid::Uuid::new_v4().to_string())
                }
                let request = self
                    .client
                    .post(endpoint(text(&config, "deviceCodeUrl"))?)
                    .header("Accept", "application/json")
                    .form(&fields);
                let mut d = self
                    .required(self.device_headers(request, provider, &extra))
                    .await?;
                if provider == "kimi" {
                    let uri = config["authorizeDeviceUrl"]
                        .as_str()
                        .unwrap_or("https://www.kimi.com/code/authorize_device");
                    if !d["verification_uri"].is_string() {
                        d["verification_uri"] = json!(uri)
                    }
                    if !d["verification_uri_complete"].is_string() {
                        d["verification_uri_complete"] = json!(url(
                            uri,
                            &[("user_code", text(&d, "user_code").to_owned())]
                        )?)
                    }
                    d["_kimiDeviceId"] = extra["_kimiDeviceId"].clone()
                }
                if provider.starts_with("minimax-code") {
                    let by_user = !d["device_code"].is_string();
                    if by_user {
                        d["device_code"] = d["user_code"].clone()
                    }
                    d["_minimaxPollByUser"] = json!(by_user);
                    if !d["verification_uri"].is_string() {
                        d["verification_uri"] = d["verification_uri_complete"]
                            .as_str()
                            .or(d["verification_url"].as_str())
                            .unwrap_or("")
                            .into()
                    }
                    let expiry = d["expires_in"]
                        .as_u64()
                        .or_else(|| {
                            let n = d["expired_in"].as_u64()?;
                            Some(if n >= 1_000_000_000_000 {
                                n.saturating_div(1000).saturating_sub(epoch()).max(1)
                            } else {
                                n
                            })
                        })
                        .unwrap_or(600);
                    d["expires_in"] = json!(expiry);
                    let interval = d["interval"].as_u64().unwrap_or(5);
                    d["interval"] = json!(if by_user && interval >= 60 {
                        interval / 1000
                    } else {
                        interval
                    });
                }
                d
            }
        };
        let code = nonempty(&data, "device_code")?;
        for field in ["verification_uri", "verification_uri_complete"] {
            if let Some(raw) = data[field].as_str().filter(|s| !s.is_empty()) {
                endpoint(raw)?;
            }
        }
        data["codeVerifier"] = json!(verifier);
        let expires = data["expires_in"].as_u64().unwrap_or(600).clamp(1, 3600);
        self.insert(
            format!("device:{provider}:{code}"),
            Session {
                provider: provider.into(),
                config,
                redirect: String::new(),
                verifier,
                expires: Instant::now() + Duration::from_secs(expires),
                status: json!({"status":"pending"}),
                device: data.clone(),
                in_flight: false,
            },
        )?;
        if let Some(public) = data.as_object_mut() {
            public.retain(|key, _| {
                [
                    "device_code",
                    "user_code",
                    "verification_uri",
                    "verification_uri_complete",
                    "expires_in",
                    "interval",
                    "codeVerifier",
                ]
                .contains(&key.as_str())
            });
        }
        Ok(data)
    }
    async fn poll(&self, provider: &str, body: &Value) -> Result<Value> {
        let code = nonempty(body, "deviceCode")?;
        let key = format!("device:{provider}:{code}");
        let s = self.session(&key, Some(provider))?;
        if text(body, "codeVerifier") != s.verifier {
            return Err(Error::new(400, "Device session verifier mismatch"));
        }
        self.claim(&key, provider)?;
        let result = self.poll_token(provider, &s, &code).await;
        match result {
            Ok(tokens) if !text(&tokens, "access_token").is_empty() => {
                let mapped = self
                    .map_tokens(provider, &tokens, &s.config, &s.device)
                    .await;
                match self.finish(&key, provider, mapped) {
                    Ok(v) => Ok(v),
                    Err(e) => Ok(
                        json!({"success":false,"error":"exchange_failed","errorDescription":e.message,"pending":false,"fatal":true}),
                    ),
                }
            }
            Ok(tokens) => {
                let error = tokens["error"].as_str().unwrap_or("authorization_pending");
                let pending = matches!(error, "authorization_pending" | "slow_down");
                let mut sessions = self.sessions.lock().map_err(internal)?;
                if let Some(s) = sessions.get_mut(&key) {
                    s.in_flight = false;
                    if !pending {
                        s.status = json!({"status":"error","error":error});
                        s.device = Value::Null;
                    }
                }
                Ok(
                    json!({"success":false,"error":error,"errorDescription":tokens["error_description"],"pending":pending}),
                )
            }
            Err(e) => {
                let _ = self.finish(&key, provider, Err(Error::new(e.status, e.message.clone())));
                Ok(
                    json!({"success":false,"error":"poll_failed","errorDescription":e.message,"pending":false,"fatal":true}),
                )
            }
        }
    }
    async fn poll_token(&self, provider: &str, s: &Session, code: &str) -> Result<Value> {
        let c = &s.config;
        let d = &s.device;
        match provider {
            "qoder" | "qoder-cn" => {
                let raw = url(
                    text(c, "deviceTokenUrl"),
                    &[
                        ("nonce", code.into()),
                        ("verifier", s.verifier.clone()),
                        ("challenge_method", "S256".into()),
                    ],
                )?;
                let response = self
                    .client
                    .get(endpoint(&raw)?)
                    .header("User-Agent", "Go-http-client/2.0")
                    .timeout(Duration::from_secs(15))
                    .send()
                    .await
                    .map_err(|_| Error::new(502, "Qoder poll failed"))?;
                if matches!(response.status().as_u16(), 202 | 404) {
                    return Ok(json!({"error":"authorization_pending"}));
                }
                if !response.status().is_success() {
                    return Err(Error::new(502, "Qoder rejected device-token polling"));
                }
                let raw = self.decode(response).await?.1;
                nonempty(&raw, "token")?;
                Ok(
                    json!({"access_token":raw["token"],"refresh_token":raw["refresh_token"],"expires_in":relative(&raw["expires_at"]).or(raw["expires_in"].as_u64()).unwrap_or(30*86400).max(86400),"_qoderUserId":raw["user_id"]}),
                )
            }
            "kiro" => {
                let raw=self.read(self.client.post(endpoint(text(c,"tokenUrl"))?).json(&json!({"clientId":d["_clientId"],"clientSecret":d["_clientSecret"],"deviceCode":code,"grantType":GRANT}))).await?.1;
                if raw["accessToken"].is_string() {
                    Ok(
                        json!({"access_token":raw["accessToken"],"refresh_token":raw["refreshToken"],"expires_in":raw["expiresIn"],"profile_arn":raw["profileArn"]}),
                    )
                } else {
                    Ok(raw)
                }
            }
            "kilocode" => {
                let raw = format!(
                    "{}/{}",
                    text(c, "pollUrlBase").trim_end_matches('/'),
                    safe_segment(code)?
                );
                let response = self
                    .client
                    .get(endpoint(&raw)?)
                    .send()
                    .await
                    .map_err(|_| Error::new(502, "Kilo poll failed"))?;
                let status = response.status().as_u16();
                match status {
                    202 => Ok(json!({"error":"authorization_pending"})),
                    403 => Ok(json!({"error":"access_denied"})),
                    410 => Ok(json!({"error":"expired_token"})),
                    _ => {
                        if !(200..300).contains(&status) {
                            return Err(Error::new(502, "Kilo rejected device-token polling"));
                        }
                        let raw = self.decode(response).await?.1;
                        if raw["status"] == "approved" && raw["token"].is_string() {
                            Ok(json!({"access_token":raw["token"],"_userEmail":raw["userEmail"]}))
                        } else {
                            Ok(json!({"error":"authorization_pending"}))
                        }
                    }
                }
            }
            "codebuddy-cn" | "codebuddy-intl" => {
                let raw = url(text(c, "tokenUrl"), &[("state", code.into())])?;
                let raw = self
                    .required(self.buddy_headers(self.client.get(endpoint(&raw)?), provider, c))
                    .await?;
                if raw["code"] == 0 && raw["data"]["accessToken"].is_string() {
                    let d = &raw["data"];
                    Ok(
                        json!({"access_token":d["accessToken"],"refresh_token":d["refreshToken"],"expires_in":d["expiresIn"].as_u64().unwrap_or(86400)}),
                    )
                } else if raw["code"] == 11217 {
                    Ok(json!({"error":"authorization_pending"}))
                } else {
                    Ok(json!({"error":"access_denied"}))
                }
            }
            "glm" => self.poll_glm(c, d, code).await,
            _ => {
                let mut fields =
                    json!({"grant_type":GRANT,"client_id":c["clientId"],"device_code":code});
                if provider.starts_with("minimax-code") {
                    fields["code_verifier"] = json!(s.verifier);
                    if d["_minimaxPollByUser"] == true {
                        fields.as_object_mut().unwrap().remove("device_code");
                        fields["user_code"] = json!(code)
                    }
                }
                let request = self.device_headers(
                    self.client
                        .post(endpoint(text(c, "tokenUrl"))?)
                        .header("Accept", "application/json")
                        .form(&fields),
                    provider,
                    d,
                );
                let mut raw = self.read(request).await?.1;
                if provider.starts_with("minimax-code")
                    && let Some(status) = raw["status"].as_str()
                {
                    raw = json!({"error":match status{"pending"=>"authorization_pending","slow_down"=>"slow_down","denied"|"access_denied"=>"access_denied","expired"|"expired_token"=>"expired_token",_=>"unexpected_status"}})
                }
                Ok(raw)
            }
        }
    }
    async fn user_info(&self, config: &Value, token: &str) -> Value {
        let raw = text(config, "userInfoUrl");
        if raw.is_empty() {
            return json!({});
        }
        match endpoint(raw) {
            Ok(u) => self
                .required(
                    self.client
                        .get(u)
                        .header("Accept", "application/json")
                        .bearer_auth(token),
                )
                .await
                .unwrap_or(json!({})),
            Err(_) => json!({}),
        }
    }
    async fn map_tokens(
        &self,
        provider: &str,
        tokens: &Value,
        config: &Value,
        device: &Value,
    ) -> Result<Value> {
        let access = nonempty(tokens, "access_token")?;
        let mut value = json!({"accessToken":access});
        for (a, b) in [
            ("refresh_token", "refreshToken"),
            ("expires_in", "expiresIn"),
            ("scope", "scope"),
            ("id_token", "idToken"),
        ] {
            if let Some(v) = tokens.get(a).filter(|v| !v.is_null()) {
                value[b] = v.clone()
            }
        }
        match provider {
            "codex" => {
                let id = jwt(text(tokens, "id_token"));
                let a = jwt(&access);
                let auth = &id["https://api.openai.com/auth"];
                value["email"] = id["email"]
                    .as_str()
                    .or(a["email"].as_str())
                    .map(str::to_owned)
                    .into();
                value["providerSpecificData"] = json!({"chatgptAccountId":auth["chatgpt_account_id"].as_str().or(id["account_id"].as_str()),"chatgptPlanType":auth["chatgpt_plan_type"].as_str().or(id["plan_type"].as_str())});
            }
            "xai" | "grok-cli" => {
                let id = jwt(text(tokens, "id_token"));
                let a = jwt(&access);
                let user = if provider == "grok-cli" {
                    self.required(
                        self.client
                            .get(endpoint("https://cli-chat-proxy.grok.com/v1/user")?)
                            .bearer_auth(&access)
                            .header("x-xai-token-auth", "xai-grok-cli")
                            .header("x-grok-client-version", "1.0.44")
                            .header("User-Agent", "grok-cli/1.0.44"),
                    )
                    .await
                    .unwrap_or(json!({}))
                } else {
                    json!({})
                };
                value["email"] = id["email"]
                    .as_str()
                    .or(id["preferred_username"].as_str())
                    .or(a["email"].as_str())
                    .or(user["email"].as_str())
                    .map(str::to_owned)
                    .into();
                value["providerSpecificData"] = json!({"idToken":tokens["id_token"]});
                if provider == "grok-cli" {
                    let p = &mut value["providerSpecificData"];
                    p["authMethod"] = json!("device_code");
                    p["email"] = user["email"].clone();
                    p["userId"] = user["userId"]
                        .as_str()
                        .or(user["principalId"].as_str())
                        .map(str::to_owned)
                        .into();
                    p["hasGrokCodeAccess"] = user["hasGrokCodeAccess"].clone();
                    p["subscriptionTier"] = user["subscriptionTier"].clone();
                    let name = format!("{} {}", text(&user, "firstName"), text(&user, "lastName"));
                    if !name.trim().is_empty() {
                        value["displayName"] = json!(name.trim())
                    }
                }
            }
            "gemini-cli" | "antigravity" => {
                let user = self.user_info(config, &access).await;
                value["email"] = user["email"].clone();
                let raw = config["loadCodeAssistEndpoint"]
                    .as_str()
                    .unwrap_or("https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist");
                let mut body = json!({"metadata":google_metadata()});
                if provider == "gemini-cli" {
                    body["mode"] = json!(1)
                }
                let mut request = self
                    .client
                    .post(endpoint(raw)?)
                    .bearer_auth(&access)
                    .json(&body);
                if provider == "antigravity" {
                    request = request
                        .header("User-Agent", text(config, "loadCodeAssistUserAgent"))
                        .header("x-request-source", "local")
                }
                let loaded = self.required(request).await.unwrap_or(json!({}));
                let project = loaded["cloudaicompanionProject"]
                    .as_str()
                    .or(loaded["cloudaicompanionProject"]["id"].as_str())
                    .unwrap_or("");
                value["projectId"] = json!(project);
                if provider == "antigravity" && !project.is_empty() {
                    let tier = loaded["allowedTiers"]
                        .as_array()
                        .and_then(|a| {
                            a.iter()
                                .find(|t| t["isDefault"] == true && t["id"].is_string())
                        })
                        .map(|t| text(t, "id"))
                        .unwrap_or("legacy-tier")
                        .to_owned();
                    if let Ok(u) = endpoint(text(config, "onboardUserEndpoint")) {
                        let this = self.clone();
                        let access = access.clone();
                        let ua = text(config, "loadCodeAssistUserAgent").to_owned();
                        tokio::spawn(async move {
                            for _ in 0..10 {
                                let response=this.required(this.client.post(u.clone()).bearer_auth(&access).header("User-Agent",&ua).header("x-request-source","local").json(&json!({"tierId":tier,"metadata":google_metadata()}))).await;
                                match response {
                                    Ok(data) if data["done"] == true => break,
                                    Err(_) => break,
                                    _ => tokio::time::sleep(Duration::from_secs(5)).await,
                                }
                            }
                        });
                    }
                }
            }
            "github" => {
                let request = |raw: &str| -> Result<RequestBuilder> {
                    Ok(self
                        .client
                        .get(endpoint(raw)?)
                        .bearer_auth(&access)
                        .header("Accept", "application/json")
                        .header("X-GitHub-Api-Version", text(config, "apiVersion"))
                        .header("User-Agent", text(config, "userAgent")))
                };
                let copilot = self
                    .required(request(text(config, "copilotTokenUrl"))?)
                    .await
                    .unwrap_or(json!({}));
                let user = self
                    .required(request(text(config, "userInfoUrl"))?)
                    .await
                    .unwrap_or(json!({}));
                value["name"] = user["login"]
                    .as_str()
                    .or(user["name"].as_str())
                    .map(str::to_owned)
                    .into();
                value["displayName"] = user["name"]
                    .as_str()
                    .or(user["login"].as_str())
                    .map(str::to_owned)
                    .into();
                value["email"] = user["email"].clone();
                value["providerSpecificData"] = json!({"copilotToken":copilot["token"],"copilotTokenExpiresAt":copilot["expires_at"],"githubUserId":user["id"],"githubLogin":user["login"],"githubName":user["name"],"githubEmail":user["email"]});
            }
            "iflow" => {
                let raw = url(
                    text(config, "userInfoUrl"),
                    &[("accessToken", access.clone())],
                )?;
                let user = self
                    .required(
                        self.client
                            .get(endpoint(&raw)?)
                            .header("Accept", "application/json"),
                    )
                    .await?;
                if user["success"] != true || text(&user["data"], "apiKey").trim().is_empty() {
                    return Err(Error::new(502, "iFlow account did not return an API key"));
                }
                let user = &user["data"];
                let email = user["email"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .or(user["phone"].as_str().filter(|s| !s.trim().is_empty()))
                    .ok_or_else(|| Error::new(502, "iFlow account has no email or phone"))?;
                value["apiKey"] = user["apiKey"].clone();
                value["email"] = json!(email);
                value["displayName"] = user["nickname"]
                    .as_str()
                    .or(user["name"].as_str())
                    .map(str::to_owned)
                    .into();
            }
            "gitlab" => {
                let user = self.user_info(config, &access).await;
                value["providerSpecificData"] = json!({"username":user["username"],"email":user["email"].as_str().or(user["public_email"].as_str()),"name":user["name"],"baseUrl":config["baseUrl"],"clientId":config["clientId"],"clientSecret":config["clientSecret"],"authKind":"oauth"});
            }
            "kimi" => {
                value["providerSpecificData"] =
                    json!({"authMethod":"device_code","deviceId":device["_kimiDeviceId"]})
            }
            "kiro" => {
                let mut profile = tokens["profile_arn"].clone();
                if profile.is_null() {
                    let p=self.required(self.client.post(endpoint("https://codewhisperer.us-east-1.amazonaws.com/ListAvailableProfiles")?).bearer_auth(&access).json(&json!({"maxResults":10}))).await.unwrap_or(json!({}));
                    profile = p["profiles"]
                        .as_array()
                        .and_then(|a| a.iter().find_map(|p| p["arn"].as_str()))
                        .map(str::to_owned)
                        .into();
                }
                value["email"] = jwt(&access)["email"].clone();
                value["providerSpecificData"] = json!({"profileArn":profile,"clientId":device["_clientId"],"clientSecret":device["_clientSecret"],"region":device["_region"],"authMethod":device["_authMethod"],"startUrl":device["_startUrl"]});
            }
            "qoder" | "qoder-cn" => {
                let user = self.user_info(config, &access).await;
                let id = tokens["_qoderUserId"].as_str().unwrap_or("");
                let email = user["email"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .or_else(|| (!id.is_empty()).then(|| format!("qoder-user-{id}")));
                value["email"] = email.into();
                value["displayName"] = user["name"]
                    .as_str()
                    .or(user["username"].as_str())
                    .map(str::to_owned)
                    .into();
                value["providerSpecificData"] = json!({"authMethod":"device","userId":id,"machineId":device["_qoderMachineId"],"organizationId":user["organization_id"]});
            }
            "kilocode" => {
                value["refreshToken"] = Value::Null;
                value["email"] = tokens["_userEmail"].clone();
                let raw = format!(
                    "{}/api/profile",
                    text(config, "apiBaseUrl").trim_end_matches('/')
                );
                let user = self
                    .required(self.client.get(endpoint(&raw)?).bearer_auth(&access))
                    .await
                    .unwrap_or(json!({}));
                if let Some(org) = user["organizations"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|o| o.get("id"))
                {
                    value["providerSpecificData"] = json!({"orgId":org})
                }
            }
            "nous" => {
                let agent = self
                    .required(
                        self.client
                            .post(endpoint(text(config, "agentKeyUrl"))?)
                            .bearer_auth(&access)
                            .json(&json!({"min_ttl_seconds":7200})),
                    )
                    .await
                    .unwrap_or(json!({}));
                value["apiKey"] = agent["api_key"].as_str().unwrap_or(&access).into();
                value["providerSpecificData"] = json!({"inferenceBaseUrl":config["inferenceBaseUrl"],"agentKeyId":agent["key_id"],"agentKeyExpiresAt":agent["expires_at"]});
            }
            "muse" => {
                let key_url = config["keyUrl"]
                    .as_str()
                    .unwrap_or("https://api.meta.ai/muse-code/key");
                let mut result = Err(Error::new(502, "Muse key mint failed"));
                for attempt in 0..3 {
                    let (status, payload) = self
                        .read(
                            self.client
                                .post(endpoint(key_url)?)
                                .bearer_auth(&access)
                                .header("x-api-version", "1.0.0")
                                .json(&json!({"onboard":true})),
                        )
                        .await?;
                    if (status == 429 || status >= 500) && attempt < 2 {
                        tokio::time::sleep(Duration::from_secs(5 * (attempt + 1))).await;
                        continue;
                    }
                    result = if (200..300).contains(&status) {
                        Ok(payload)
                    } else {
                        Err(Error::new(
                            502,
                            format!("Muse key mint failed (HTTP {status})"),
                        ))
                    };
                    break;
                }
                let key = result?;
                if key["is_subs_active"] == false {
                    return Err(Error::new(403, "Muse subscription is inactive"));
                }
                let api_key = nonempty(&key, "api_key")?;
                value["accessToken"] = json!(api_key);
                value["refreshToken"] = Value::Null;
                value.as_object_mut().unwrap().remove("expiresIn");
                value["email"] = key["user_email"]
                    .as_str()
                    .map(|e| e.trim().to_lowercase())
                    .into();
                value["providerSpecificData"] = json!({"authMethod":"device_code","oauthAccessToken":access,"subscriptionTier":key["subs_tier_name"]});
            }
            "glm" => {
                let user = &tokens["_zcodeUser"];
                value["refreshToken"] = Value::Null;
                value["email"] = user["email"].clone();
                value["displayName"] = user["name"]
                    .as_str()
                    .or(user["email"].as_str())
                    .map(str::to_owned)
                    .into();
                value["providerSpecificData"] = json!({"authMethod":"cli_poll","username":user["name"],"userId":user["user_id"],"zcodeJwtToken":tokens["_zcodeJwtToken"],"zaiBusinessToken":tokens["_zaiBusinessToken"],"zaiRefreshToken":tokens["_zaiRefreshToken"]});
            }
            _ => {}
        }
        self.timestamps(&mut value)?;
        Ok(value)
    }
    fn timestamps(&self, value: &mut Value) -> Result<()> {
        let store = self.store.lock().map_err(internal)?;
        let now: String = store
            .connection
            .query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |r| {
                r.get(0)
            })
            .map_err(internal)?;
        value["lastRefreshAt"] = json!(now);
        if let Some(seconds) = value["expiresIn"].as_u64() {
            if seconds > 10 * 366 * 86400 {
                return Err(Error::new(502, "OAuth provider returned an invalid expiry"));
            }
            let expires: String = store
                .connection
                .query_row(
                    "SELECT strftime('%Y-%m-%dT%H:%M:%fZ',?1,'unixepoch')",
                    [(epoch() + seconds) as i64],
                    |r| r.get(0),
                )
                .map_err(internal)?;
            value["expiresAt"] = json!(expires)
        }
        Ok(())
    }
    async fn poll_glm(&self, c: &Value, d: &Value, code: &str) -> Result<Value> {
        let raw = format!(
            "{}/{}",
            text(c, "cliPollUrl").trim_end_matches('/'),
            safe_segment(code)?
        );
        let payload = business(
            self.required(
                self.client
                    .get(endpoint(&raw)?)
                    .bearer_auth(text(d, "_zcodePollToken")),
            )
            .await?,
        )?;
        if payload["status"] == "failed" {
            return Ok(json!({"error":"access_denied"}));
        }
        if payload["status"] != "ready" {
            return Ok(json!({"error":"authorization_pending"}));
        }
        let provider = text(c, "providerId");
        let nested = &payload[provider];
        let token = nested["access_token"]
            .as_str()
            .or(nested["accessToken"].as_str())
            .or(payload["access_token"].as_str())
            .or(payload["accessToken"].as_str())
            .ok_or_else(|| Error::new(502, "ZCode login did not return an account token"))?;
        let login = business(
            self.required(
                self.client
                    .post(endpoint(text(c, "businessLoginUrl"))?)
                    .json(&json!({"token":token})),
            )
            .await?,
        )?;
        let business_token = login["access_token"]
            .as_str()
            .or(login["accessToken"].as_str())
            .ok_or_else(|| Error::new(502, "Z.ai account did not return a business token"))?;
        let base = text(c, "apiBaseUrl").trim_end_matches('/');
        let raw = format!("{base}/api/biz/customer/getCustomerInfo");
        let customer = business(
            self.required(self.client.get(endpoint(&raw)?).bearer_auth(business_token))
                .await?,
        )?;
        let organizations = customer["organizations"]
            .as_array()
            .ok_or_else(|| Error::new(502, "No Z.ai organization available"))?;
        let candidates: Vec<_> = organizations
            .iter()
            .filter(|o| {
                o["organizationId"].is_string()
                    && o["projects"].as_array().is_some_and(|a| {
                        a.iter()
                            .any(|p| p["projectType"] != "2" && p["projectType"] != 2)
                    })
            })
            .collect();
        let org = candidates
            .iter()
            .find(|o| default_name(text(o, "organizationName")))
            .or(candidates.first())
            .ok_or_else(|| Error::new(502, "No personal Z.ai project available"))?;
        let projects: Vec<_> = org["projects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["projectType"] != "2" && p["projectType"] != 2)
            .collect();
        let project = projects
            .iter()
            .find(|p| default_name(text(p, "projectName")))
            .or(projects.first())
            .ok_or_else(|| Error::new(502, "No personal Z.ai project available"))?;
        let raw = format!(
            "{base}/api/biz/v1/organization/{}/projects/{}/api_keys",
            safe_segment(text(org, "organizationId"))?,
            safe_segment(text(project, "projectId"))?
        );
        let keys = business(
            self.required(self.client.get(endpoint(&raw)?).bearer_auth(business_token))
                .await?,
        )?;
        let key = keys
            .as_array()
            .and_then(|a| a.iter().find(|k| k["name"] == c["planApiKeyName"]))
            .cloned();
        let key = match key {
            Some(k) => k,
            None => business(
                self.required(
                    self.client
                        .post(endpoint(&raw)?)
                        .bearer_auth(business_token)
                        .json(&json!({"name":c["planApiKeyName"]})),
                )
                .await?,
            )?,
        };
        let key = nonempty(&key, "apiKey")?;
        let copy = format!("{raw}/copy/{}", safe_segment(&key)?);
        let copied = business(
            self.required(
                self.client
                    .get(endpoint(&copy)?)
                    .bearer_auth(business_token),
            )
            .await?,
        )?;
        let secret = nonempty(&copied, "secretKey")?;
        Ok(
            json!({"access_token":format!("{key}.{secret}"),"_zcodeJwtToken":payload["token"],"_zaiBusinessToken":business_token,"_zaiRefreshToken":nested["refresh_token"].as_str().or(nested["refreshToken"].as_str()).or(payload["refresh_token"].as_str()),"_zcodeUser":payload["user"]}),
        )
    }
    async fn start_proxy(&self, provider: &str, query: &HashMap<String, String>) -> Result<Value> {
        let port = match provider {
            "codex" => 1455,
            "xai" => 56121,
            _ => {
                return Err(Error::new(
                    501,
                    format!("Native {provider} callback adapter is not implemented"),
                ));
            }
        };
        let state = query
            .get("state")
            .ok_or_else(|| Error::new(400, "Missing state"))?;
        let s = self.session(state, Some(provider))?;
        if query.get("code_verifier") != Some(&s.verifier)
            || query.get("redirect_uri") != Some(&s.redirect)
        {
            return Err(Error::new(400, "OAuth proxy session mismatch"));
        }
        let callback =
            Url::parse(&s.redirect).map_err(|_| Error::new(400, "Invalid callback URI"))?;
        let path = if provider == "codex" {
            "/auth/callback"
        } else {
            "/callback"
        };
        if callback.port_or_known_default() != Some(port) || callback.path() != path {
            return Err(Error::new(400, "Incorrect fixed-port callback URI"));
        }
        {
            let mut listeners = self.listeners.lock().map_err(internal)?;
            listeners.retain(|_, task| !task.is_finished());
            if listeners.contains_key(provider) {
                return Ok(json!({"success":true,"port":port,"serverSide":true}));
            }
        }
        let listener = match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
        {
            Ok(l) => l,
            Err(_) => return Ok(json!({"success":false,"reason":"port_busy","serverSide":false})),
        };
        let app = Router::new()
            .route(path, get(native_callback))
            .with_state(self.clone());
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async { tokio::time::sleep(TTL).await })
                .await;
        });
        self.listeners
            .lock()
            .map_err(internal)?
            .insert(provider.into(), task.abort_handle());
        Ok(json!({"success":true,"port":port,"serverSide":true}))
    }
    /// Refreshes are serialized, reread from disk, and persisted before another
    /// refresh can start; single-use rotating refresh tokens cannot be reused.
    pub async fn refresh_connection(&self, id: &str) -> crate::Result<Value> {
        self.refresh(id)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
    async fn refresh(&self, id: &str) -> Result<Value> {
        let _guard = self.refresh_lock.lock().await;
        let connection = self
            .store
            .lock()
            .map_err(internal)?
            .connections(None)
            .map_err(internal)?
            .into_iter()
            .find(|v| v["id"] == id)
            .ok_or_else(|| Error::new(404, "Connection not found"))?;
        let provider = nonempty(&connection, "provider")?;
        let mut config = self.descriptor(&provider)?["config"].clone();
        if matches!(
            provider.as_str(),
            "qoder" | "qoder-cn" | "muse" | "glm" | "kilocode" | "kimchi" | "zed"
        ) {
            return Err(Error::new(
                409,
                "This provider requires a new sign-in when credentials expire",
            ));
        }
        if provider == "github" && text(&connection, "refreshToken").is_empty() {
            let mut value = self
                .map_tokens(
                    &provider,
                    &json!({"access_token":connection["accessToken"]}),
                    &config,
                    &Value::Null,
                )
                .await?;
            value["id"] = json!(id);
            return self
                .store
                .lock()
                .map_err(internal)?
                .save_connection(&value)
                .map_err(internal);
        }
        let refresh = nonempty(&connection, "refreshToken")?;
        if provider == "gitlab" {
            let p = &connection["providerSpecificData"];
            let base = endpoint(text(p, "baseUrl"))?
                .to_string()
                .trim_end_matches('/')
                .to_owned();
            config["tokenUrl"] = json!(format!("{base}{}", text(&config, "tokenUrlPath")));
            config["clientId"] = p["clientId"].clone();
            config["clientSecret"] = p["clientSecret"].clone()
        }
        if let Some(raw) = config["refreshUrl"].as_str() {
            config["tokenUrl"] = json!(raw)
        }
        let tokens = match provider.as_str() {
            "kiro" => {
                let p = &connection["providerSpecificData"];
                let region = p["region"].as_str().unwrap_or("us-east-1");
                valid_region(region)?;
                let raw = format!("https://oidc.{region}.amazonaws.com/token");
                let t=self.required(self.client.post(endpoint(&raw)?).json(&json!({"clientId":p["clientId"],"clientSecret":p["clientSecret"],"refreshToken":refresh,"grantType":"refresh_token"}))).await?;
                json!({"access_token":t["accessToken"],"refresh_token":t["refreshToken"],"expires_in":t["expiresIn"]})
            }
            "cline" | "clinepass" => {
                let t=self.required(self.client.post(endpoint(text(&config,"refreshUrl"))?).json(&json!({"refreshToken":refresh,"grantType":"refresh_token","clientType":"extension"}))).await?;
                let t = t.get("data").unwrap_or(&t);
                json!({"access_token":t["accessToken"],"refresh_token":t["refreshToken"],"expires_in":t["expiresIn"].as_u64().or(t["expires_in"].as_u64()).unwrap_or(3600)})
            }
            "codebuddy-cn" | "codebuddy-intl" => {
                let request = self.buddy_headers(
                    self.client
                        .post(endpoint(text(&config, "refreshUrl"))?)
                        .header("X-Refresh-Token", &refresh)
                        .header("X-Auth-Refresh-Source", "plugin")
                        .json(&json!({})),
                    &provider,
                    &config,
                );
                let t = self.required(request).await?;
                if t["code"] != 0 {
                    return Err(Error::new(502, "CodeBuddy rejected token refresh"));
                }
                let t = &t["data"];
                json!({"access_token":t["accessToken"],"refresh_token":t["refreshToken"],"expires_in":t["expiresIn"]})
            }
            _ => {
                let mut fields = json!({"grant_type":"refresh_token","refresh_token":refresh,"client_id":config["clientId"]});
                if !text(&config, "clientSecret").is_empty() && provider != "claude" {
                    fields["client_secret"] = config["clientSecret"].clone()
                }
                if provider.starts_with("minimax-code") {
                    fields["scope"] = config["scope"].clone();
                    fields["audience"] = config["audience"].clone()
                }
                let json_body = matches!(provider.as_str(), "claude" | "codex");
                let mut req = self
                    .client
                    .post(endpoint(text(&config, "tokenUrl"))?)
                    .header("Accept", "application/json");
                req = if json_body {
                    req.json(&fields)
                } else {
                    req.form(&fields)
                };
                if provider == "iflow" {
                    req = req.basic_auth(
                        text(&config, "clientId"),
                        Some(text(&config, "clientSecret")),
                    )
                }
                if provider == "nous" {
                    req = req.header("x-nous-refresh-token", &refresh)
                }
                let device =
                    json!({"_kimiDeviceId":connection["providerSpecificData"]["deviceId"]});
                self.required(self.device_headers(req, &provider, &device))
                    .await?
            }
        };
        nonempty(&tokens, "access_token")?;
        let mut patch = if provider == "nous" {
            self.map_tokens(&provider, &tokens, &config, &Value::Null)
                .await?
        } else {
            json!({})
        };
        for (a, b) in [
            ("access_token", "accessToken"),
            ("refresh_token", "refreshToken"),
            ("expires_in", "expiresIn"),
            ("id_token", "idToken"),
            ("scope", "scope"),
        ] {
            if let Some(v) = tokens
                .get(a)
                .filter(|v| !v.is_null() && v.as_str() != Some(""))
            {
                patch[b] = v.clone()
            }
        }
        patch["id"] = json!(id);
        patch["testStatus"] = json!("active");
        self.timestamps(&mut patch)?;
        self.store
            .lock()
            .map_err(internal)?
            .save_connection(&patch)
            .map_err(internal)
    }
}
async fn native_callback(
    State(service): State<Service>,
    Query(query): Query<HashMap<String, String>>,
) -> Html<&'static str> {
    if service.callback(&query).await.is_ok() {
        Html(
            "<!doctype html><meta charset=utf-8><meta name=referrer content=no-referrer><title>9Router sign-in</title><h1>Sign-in complete</h1><p>Return to 9Router. You can close this tab.</p>",
        )
    } else {
        Html(
            "<!doctype html><meta charset=utf-8><meta name=referrer content=no-referrer><title>9Router sign-in</title><h1>Sign-in could not complete</h1><p>Return to 9Router for details or restart sign-in.</p>",
        )
    }
}
fn valid_region(region: &str) -> Result<()> {
    let p: Vec<_> = region.split('-').collect();
    if p.len() != 3
        || p[0].len() != 2
        || !p[0].bytes().all(|b| b.is_ascii_lowercase())
        || p[1].is_empty()
        || !p[1].bytes().all(|b| b.is_ascii_lowercase())
        || p[2].is_empty()
        || p[2].len() > 2
        || !p[2].bytes().all(|b| b.is_ascii_digit())
    {
        return Err(Error::new(400, "Invalid AWS region"));
    }
    Ok(())
}
fn safe_segment(s: &str) -> Result<String> {
    if s.is_empty() || s.len() > 4096 {
        return Err(Error::new(400, "Invalid endpoint path identifier"));
    }
    let mut u = Url::parse("https://localhost/").unwrap();
    u.path_segments_mut().unwrap().push(s);
    Ok(u.path()[1..].to_owned())
}
fn relative(v: &Value) -> Option<u64> {
    let n = v
        .as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))?;
    let seconds = if n > 1_000_000_000_000 { n / 1000 } else { n };
    Some(seconds.saturating_sub(epoch()).max(1))
}
fn business(v: Value) -> Result<Value> {
    let code = &v["code"];
    if !code.is_null() && code != 0 && code != 200 && code != "0" && code != "200"
        || v["success"] == false
    {
        return Err(Error::new(
            502,
            "Provider account API returned a business error",
        ));
    }
    Ok(v.get("data").cloned().unwrap_or(v))
}
fn default_name(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n == "default" || n.contains("默认机构") || n.contains("默认项目")
}
fn google_metadata() -> Value {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => 2,
        ("macos", _) => 1,
        ("linux", "aarch64") => 4,
        ("linux", _) => 3,
        ("windows", _) => 5,
        _ => 0,
    };
    json!({"ideType":9,"platform":platform,"pluginType":2})
}
