//! Headed browser login, using only app-owned Chromium profiles.
use crate::{Result, Store};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, Weak},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpStream,
    sync::watch,
    task::AbortHandle,
    time::{Instant, sleep, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);
const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const MAX_CREDENTIAL: usize = 256 * 1024;
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
pub const PROVIDERS: [&str; 6] = [
    "grok-web",
    "chatgpt-web",
    "gemini-web",
    "muse-web",
    "deepseek-web",
    "uc-web",
];

fn error(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::other(message).into()
}

struct Config {
    url: &'static str,
    domain: &'static str,
    required: &'static [&'static str],
    name: &'static str,
}

fn config(provider: &str) -> Option<Config> {
    let (url, domain, required, name): (_, _, &[&str], _) = match provider {
        "grok-web" => ("https://grok.com/", "grok.com", &["sso"], "Grok Web"),
        "chatgpt-web" => (
            "https://chatgpt.com/",
            "chatgpt.com",
            &["__Secure-next-auth.session-token"],
            "ChatGPT Web",
        ),
        "gemini-web" => (
            "https://gemini.google.com/",
            "google.com",
            &["__Secure-1PSID"],
            "Gemini Web",
        ),
        "muse-web" => ("https://muse.ai/", "muse.ai", &["hatch_sess"], "Muse Web"),
        "deepseek-web" => (
            "https://chat.deepseek.com/sign_in",
            "deepseek.com",
            &[],
            "DeepSeek Web",
        ),
        "uc-web" => (
            "https://uncensored.com/",
            "uncensored.com",
            &["__client", "__client_uat"],
            "Uncensored Web",
        ),
        _ => return None,
    };
    Some(Config {
        url,
        domain,
        required,
        name,
    })
}

fn sanitize_profile(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(40)
        .collect();
    if cleaned.is_empty() {
        "default".into()
    } else {
        cleaned
    }
}

fn cookie_value(jar: &BTreeMap<String, String>, name: &str) -> Option<String> {
    if let Some(value) = jar.get(name).filter(|value| !value.is_empty()) {
        return Some(value.clone());
    }
    let prefix = format!("{name}.");
    let chunks: BTreeMap<usize, &String> = jar
        .iter()
        .filter_map(|(key, value)| {
            let suffix = key.strip_prefix(&prefix)?;
            if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some((suffix.parse().ok()?, value))
        })
        .collect();
    if chunks.is_empty() || chunks.len() > 100 || chunks.keys().copied().ne(0..chunks.len()) {
        return None;
    }
    Some(chunks.values().map(|value| value.as_str()).collect())
}

fn harvest_credential(provider: &str, cookies: &[Value], storage: Option<&str>) -> Option<String> {
    let config = config(provider)?;
    if provider == "deepseek-web" {
        let raw = storage?;
        let parsed = serde_json::from_str::<Value>(raw).ok();
        let token = parsed
            .as_ref()
            .and_then(|value| value.get("value"))
            .and_then(Value::as_str)
            .unwrap_or(raw);
        return (token.trim().len() >= 20
            && token.len() <= MAX_CREDENTIAL
            && !token.chars().any(char::is_control))
        .then(|| token.to_owned());
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    let mut jar = BTreeMap::new();
    for cookie in cookies {
        let host = cookie
            .get("domain")?
            .as_str()?
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if host != config.domain && !host.ends_with(&format!(".{}", config.domain)) {
            continue;
        }
        if cookie
            .get("expires")
            .and_then(Value::as_f64)
            .is_some_and(|expires| expires > 0.0 && expires <= now)
        {
            continue;
        }
        let name = cookie.get("name")?.as_str()?;
        let value = cookie.get("value")?.as_str()?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&c))
            || value.chars().any(|c| c.is_control() || c == ';')
        {
            return None;
        }
        if !value.is_empty() {
            jar.insert(name.to_owned(), value.to_owned());
        }
    }
    for name in config.required {
        let value = cookie_value(&jar, name)?;
        if value.is_empty()
            || (*name == "__client_uat"
                && value
                    .parse::<f64>()
                    .ok()
                    .is_none_or(|value| !value.is_finite() || value <= 0.0))
        {
            return None;
        }
    }
    let header = jar
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join("; ");
    (header.len() <= MAX_CREDENTIAL).then_some(header)
}

/// Child ownership is the cleanup boundary, including cancellation and task aborts.
pub(crate) struct Browser(Child);

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn browser_executable() -> Option<PathBuf> {
    let mut candidates = ["NINEROUTER_BROWSER_BIN", "CHROME_PATH"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    candidates.extend(
        [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ]
        .into_iter()
        .map(PathBuf::from),
    );
    for variable in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(variable) {
            candidates.push(PathBuf::from(base).join("Google/Chrome/Application/chrome.exe"));
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}

impl Browser {
    pub(crate) async fn launch(
        profile_dir: &Path,
        url: &str,
        headless: bool,
    ) -> Result<(Self, Cdp)> {
        let executable = browser_executable().ok_or_else(|| {
            error("No Chrome/Chromium executable found — set CHROME_PATH or install Google Chrome")
        })?;
        Self::launch_using(profile_dir, url, headless, &executable).await
    }

    async fn launch_using(
        profile_dir: &Path,
        url: &str,
        headless: bool,
        executable: &Path,
    ) -> Result<(Self, Cdp)> {
        private_directory(profile_dir)?;
        let active_port = profile_dir.join("DevToolsActivePort");
        // This file belongs to our closed profile; never discover another running browser.
        match fs::remove_file(&active_port) {
            Ok(()) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(_) => return Err(error("Cannot reset browser debugging endpoint")),
        }
        let mut command = Command::new(executable);
        command
            .arg(format!("--user-data-dir={}", profile_dir.display()))
            .args([
                "--remote-debugging-port=0",
                "--remote-debugging-address=127.0.0.1",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-blink-features=AutomationControlled",
                "--new-window",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if headless {
            command.arg("--headless=new");
        }
        let mut browser = Self(
            command
                .arg(url)
                .spawn()
                .map_err(|_| error("Failed to start Chrome/Chromium"))?,
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if browser.0.try_wait()?.is_some() {
                return Err(error(
                    "Chrome/Chromium exited before opening its login window; close other windows using this app profile and try again",
                ));
            }
            if let Ok(contents) = fs::read_to_string(&active_port) {
                let mut lines = contents.lines();
                if let (Some(port), Some(path)) = (
                    lines
                        .next()
                        .and_then(|line| line.parse::<u16>().ok())
                        .filter(|port| *port != 0),
                    lines.next(),
                ) && path.starts_with("/devtools/browser/")
                    && path
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"/-_".contains(&c))
                {
                    let endpoint = format!("ws://127.0.0.1:{port}{path}");
                    if let Ok(cdp) = Cdp::connect(&endpoint).await {
                        return Ok((browser, cdp));
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        Err(error(
            "Chrome/Chromium did not expose its local debugging endpoint",
        ))
    }
}

fn private_directory(path: &Path) -> Result<()> {
    if fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err(error(
            "Browser profile must be an app-owned directory, not a symlink",
        ));
    }
    let mut options = fs::DirBuilder::new();
    options.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        options.mode(0o700);
    }
    options.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Minimal sequential CDP transport. No page/binding is shared with the user's browser.
pub(crate) struct Cdp {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
    session: Option<String>,
    target: Option<String>,
}

impl Cdp {
    async fn connect(endpoint: &str) -> Result<Self> {
        let (socket, _) = timeout(
            Duration::from_secs(3),
            tokio_tungstenite::connect_async(endpoint),
        )
        .await
        .map_err(|_| error("Browser connection timed out"))?
        .map_err(|_| error("Cannot connect to app-owned browser"))?;
        Ok(Self {
            socket,
            next_id: 0,
            session: None,
            target: None,
        })
    }

    async fn command(&mut self, method: &str, params: Value, page: bool) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let mut request = json!({"id":id, "method":method, "params":params});
        if page && let Some(session) = &self.session {
            request["sessionId"] = json!(session);
        }
        timeout(Duration::from_secs(60), async {
            self.socket
                .send(Message::Text(request.to_string().into()))
                .await
                .map_err(|_| error("Browser connection closed"))?;
            while let Some(message) = self.socket.next().await {
                let message = message.map_err(|_| error("Browser connection closed"))?;
                match message {
                    Message::Text(text) => {
                        let response: Value = serde_json::from_str(&text)
                            .map_err(|_| error("Invalid browser response"))?;
                        if response.get("id").and_then(Value::as_u64) != Some(id) {
                            continue;
                        }
                        if response.get("error").is_some() {
                            return Err(error("Browser command failed"));
                        }
                        return Ok(response.get("result").cloned().unwrap_or(Value::Null));
                    }
                    Message::Ping(payload) => {
                        self.socket
                            .send(Message::Pong(payload))
                            .await
                            .map_err(|_| error("Browser connection closed"))?;
                    }
                    Message::Close(_) => break,
                    _ => (),
                }
            }
            Err(error("Browser connection closed"))
        })
        .await
        .map_err(|_| error("Browser command timed out"))?
    }

    pub(crate) async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.command(method, params, true).await
    }

    pub(crate) async fn attach_page(&mut self, origin: &str) -> Result<bool> {
        let targets = self.command("Target.getTargets", json!({}), false).await?;
        let target = targets
            .get("targetInfos")
            .and_then(Value::as_array)
            .and_then(|targets| {
                targets.iter().find(|target| {
                    target.get("type").and_then(Value::as_str) == Some("page")
                        && target
                            .get("url")
                            .and_then(Value::as_str)
                            .and_then(|url| reqwest::Url::parse(url).ok())
                            .is_some_and(|url| url.origin().ascii_serialization() == origin)
                })
            });
        let Some(id) = target
            .and_then(|target| target.get("targetId"))
            .and_then(Value::as_str)
        else {
            return Ok(false);
        };
        // Reattach only when needed, to avoid leaking a new session each poll.
        if self.session.is_some() && self.target.as_deref() == Some(id) {
            return Ok(true);
        }
        if let Some(session) = self.session.take() {
            let _ = self
                .command(
                    "Target.detachFromTarget",
                    json!({"sessionId":session}),
                    false,
                )
                .await;
        }
        let result = self
            .command(
                "Target.attachToTarget",
                json!({"targetId":id,"flatten":true}),
                false,
            )
            .await?;
        self.session = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.target = Some(id.to_owned());
        Ok(self.session.is_some())
    }

    pub(crate) async fn evaluate(&mut self, expression: &str) -> Result<Value> {
        let response = self
            .call(
                "Runtime.evaluate",
                json!({"expression":expression, "returnByValue":true, "awaitPromise":true}),
            )
            .await?;
        if response.get("exceptionDetails").is_some() {
            return Err(error("Browser page evaluation failed"));
        }
        Ok(response
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    pub(crate) async fn cookies(&mut self) -> Result<Vec<Value>> {
        Ok(self
            .command("Storage.getCookies", json!({}), false)
            .await?
            .get("cookies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

struct Session {
    id: String,
    profile: String,
    status: &'static str,
    error: Option<String>,
    connection_id: Option<String>,
    email: Option<String>,
    cancel: watch::Sender<bool>,
}

impl Session {
    fn value(&self, provider: &str) -> Value {
        json!({"provider":provider,"status":self.status,"profile":self.profile,"error":self.error,"connectionId":self.connection_id,"email":self.email,"credentialPreview":null})
    }
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Session>,
    busy: HashMap<String, String>,
    tasks: HashMap<String, AbortHandle>,
}

struct Inner {
    store: Arc<Mutex<Store>>,
    data_dir: PathBuf,
    browser: Option<PathBuf>,
    client: reqwest::Client,
    state: Mutex<State>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            for task in state.tasks.values() {
                task.abort();
            }
        }
    }
}

#[derive(Clone)]
pub struct WebLoginService {
    inner: Arc<Inner>,
}

impl WebLoginService {
    pub fn new(store: Arc<Mutex<Store>>, data_dir: PathBuf) -> Self {
        Self::build(store, data_dir, None)
    }

    /// Explicit browser selection is also useful for a portable Chromium installation.
    pub fn using_browser(store: Arc<Mutex<Store>>, data_dir: PathBuf, executable: PathBuf) -> Self {
        Self::build(store, data_dir, Some(executable))
    }

    fn build(store: Arc<Mutex<Store>>, data_dir: PathBuf, browser: Option<PathBuf>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(UA)
            .build()
            .expect("static browser identity client configuration");
        Self {
            inner: Arc::new(Inner {
                store,
                data_dir,
                browser,
                client,
                state: Mutex::new(State::default()),
            }),
        }
    }

    pub fn profiles(&self) -> Vec<String> {
        let mut profiles = fs::read_dir(self.inner.data_dir.join("browser-profiles"))
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name().into_string().ok()?;
                (entry.file_type().ok()?.is_dir()
                    && name != "web-login"
                    && sanitize_profile(&name) == name)
                    .then_some(name)
            })
            .collect::<Vec<_>>();
        profiles.sort();
        profiles
    }

    pub fn status(&self, provider: &str) -> Option<Value> {
        self.inner
            .state
            .lock()
            .ok()?
            .sessions
            .get(provider)
            .map(|session| session.value(provider))
    }

    pub async fn start(&self, provider: &str, profile: &str) -> Result<Value> {
        if config(provider).is_none() {
            return Err(error("Web login is not supported for this provider"));
        }
        if profile.len() > 4096 || profile.contains('\0') {
            return Err(error("Invalid browser profile name"));
        }
        let profile = sanitize_profile(profile);
        private_directory(&self.inner.data_dir)?;
        private_directory(&self.inner.data_dir.join("browser-profiles"))?;
        private_directory(&self.inner.data_dir.join("browser-profiles").join(&profile))?;
        let mut state = self
            .inner
            .state
            .lock()
            .map_err(|_| error("Login state unavailable"))?;
        if let Some(session) = state
            .sessions
            .get(provider)
            .filter(|session| session.status == "running")
        {
            return Ok(session.value(provider));
        }
        if state.tasks.contains_key(provider) {
            return Err(error(
                "The previous login window is still closing — try again shortly",
            ));
        }
        if state.busy.contains_key(&profile) {
            return Err(error(
                "This app browser profile is already open — finish that login or choose another profile",
            ));
        }
        let (cancel, receiver) = watch::channel(false);
        let session = Session {
            id: uuid::Uuid::new_v4().to_string(),
            profile: profile.clone(),
            status: "running",
            error: None,
            connection_id: None,
            email: None,
            cancel,
        };
        let id = session.id.clone();
        let value = session.value(provider);
        state.sessions.insert(provider.to_owned(), session);
        state.busy.insert(profile.clone(), provider.to_owned());
        let weak = Arc::downgrade(&self.inner);
        let provider = provider.to_owned();
        let task_provider = provider.clone();
        let task = tokio::spawn(async move {
            run_session(weak, task_provider, profile, id, receiver).await;
        });
        state.tasks.insert(provider, task.abort_handle());
        Ok(value)
    }

    pub fn cancel(&self, provider: &str) -> bool {
        let Ok(mut state) = self.inner.state.lock() else {
            return false;
        };
        let Some(session) = state
            .sessions
            .get_mut(provider)
            .filter(|session| session.status == "running")
        else {
            return false;
        };
        session.status = "cancelled";
        let _ = session.cancel.send(true);
        true
    }

    pub fn cancel_all(&self) {
        if let Ok(mut state) = self.inner.state.lock() {
            for session in state
                .sessions
                .values_mut()
                .filter(|session| session.status == "running")
            {
                session.status = "cancelled";
                let _ = session.cancel.send(true);
            }
        }
    }

    /// Wait until cancellation has dropped all owned browser child guards.
    pub async fn shutdown(&self) {
        self.cancel_all();
        loop {
            if self
                .inner
                .state
                .lock()
                .map(|state| state.tasks.is_empty())
                .unwrap_or(true)
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
}

struct SessionTask {
    inner: Weak<Inner>,
    provider: String,
    profile: String,
    id: String,
}

impl Drop for SessionTask {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade()
            && let Ok(mut state) = inner.state.lock()
            && let Some(session) = state
                .sessions
                .get_mut(&self.provider)
                .filter(|session| session.id == self.id)
        {
            if session.status == "running" {
                session.status = "error";
                session.error = Some("Login ended unexpectedly — try again".into());
            }
            state.busy.remove(&self.profile);
            state.tasks.remove(&self.provider);
        }
    }
}

async fn run_session(
    inner: Weak<Inner>,
    provider: String,
    profile: String,
    id: String,
    mut cancelled: watch::Receiver<bool>,
) {
    let _task = SessionTask {
        inner: inner.clone(),
        provider: provider.clone(),
        profile: profile.clone(),
        id: id.clone(),
    };
    let result = tokio::select! {
        biased;
        _ = cancelled.changed() => Err(error("Login cancelled")),
        result = timeout(LOGIN_TIMEOUT, login(&inner, &provider, &profile)) => result.unwrap_or_else(|_| Err(error("Login window timed out after 5 minutes — try again"))),
    };
    if let Some(inner) = inner.upgrade()
        && let Ok(mut state) = inner.state.lock()
        && let Some(session) = state
            .sessions
            .get_mut(&provider)
            .filter(|session| session.id == id)
        && session.status != "cancelled"
    {
        match result {
            Ok((connection_id, email)) => {
                session.status = "success";
                session.connection_id = Some(connection_id);
                session.email = (!email.is_empty()).then_some(email);
            }
            Err(error) => {
                let message = error.to_string();
                session.status = if message.starts_with("Login window timed out") {
                    "timeout"
                } else {
                    "error"
                };
                session.error = Some(message);
            }
        }
    }
}

async fn login(inner: &Weak<Inner>, provider: &str, profile: &str) -> Result<(String, String)> {
    let (path, executable, client, store) = {
        let inner = inner
            .upgrade()
            .ok_or_else(|| error("Login service stopped"))?;
        (
            inner.data_dir.join("browser-profiles").join(profile),
            inner.browser.clone(),
            inner.client.clone(),
            Arc::clone(&inner.store),
        )
    };
    let credential = capture_browser(&path, provider, executable.as_deref()).await?;
    let identity = probe_identity(&client, provider, &credential).await;
    if identity.status == "invalid" {
        return Err(error(
            "The captured session was rejected by the provider — sign in again",
        ));
    }
    let inner = inner
        .upgrade()
        .ok_or_else(|| error("Login service stopped"))?;
    let mut state = inner
        .state
        .lock()
        .map_err(|_| error("Login state unavailable"))?;
    let session = state
        .sessions
        .get_mut(provider)
        .filter(|session| session.status == "running")
        .ok_or_else(|| error("Login cancelled"))?;
    let mut store = store
        .lock()
        .map_err(|_| error("Connection storage unavailable"))?;
    let connection = save_capture(&mut store, provider, profile, &credential, &identity.email)?;
    let connection_id = connection
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| error("Saved connection has no id"))?
        .to_owned();
    session.status = "success";
    session.connection_id = Some(connection_id.clone());
    session.email = (!identity.email.is_empty()).then(|| identity.email.clone());
    Ok((connection_id, identity.email))
}

async fn capture_browser(path: &Path, provider: &str, executable: Option<&Path>) -> Result<String> {
    let config = config(provider).ok_or_else(|| error("Unsupported web provider"))?;
    let (_browser, mut cdp) = match executable {
        Some(executable) => Browser::launch_using(path, config.url, false, executable).await?,
        None => Browser::launch(path, config.url, false).await?,
    };
    loop {
        sleep(POLL_INTERVAL).await;
        let cookies = cdp.cookies().await?;
        let storage = if provider == "deepseek-web"
            && cdp.attach_page("https://chat.deepseek.com").await?
        {
            cdp.evaluate("location.origin === 'https://chat.deepseek.com' ? localStorage.getItem('userToken') : null").await.ok().and_then(|value| value.as_str().map(str::to_owned))
        } else {
            None
        };
        if let Some(credential) = harvest_credential(provider, &cookies, storage.as_deref()) {
            let _ = cdp.command("Browser.close", json!({}), false).await;
            return Ok(credential);
        }
    }
}

fn save_capture(
    store: &mut Store,
    provider: &str,
    profile: &str,
    credential: &str,
    email: &str,
) -> Result<Value> {
    let config = config(provider).ok_or_else(|| error("Unsupported web provider"))?;
    let mut rows = store
        .connections(Some(provider))?
        .into_iter()
        .filter(|row| {
            row.pointer("/providerSpecificData/authMethod")
                .and_then(Value::as_str)
                == Some("web-login")
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.get("updatedAt")
            .and_then(Value::as_str)
            .cmp(&a.get("updatedAt").and_then(Value::as_str))
    });
    let existing = if !email.is_empty() {
        rows.iter()
            .find(|row| row.get("email").and_then(Value::as_str) == Some(email))
    } else if rows.len() == 1 {
        rows.first()
    } else {
        None
    };
    let mut specific = existing
        .and_then(|row| row.get("providerSpecificData"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    specific.insert("authMethod".into(), json!("web-login"));
    specific.insert("browserProfile".into(), json!(profile));
    if provider == "deepseek-web" {
        specific.insert("tokenSource".into(), json!("localStorage"));
    }
    let mut patch = json!({"provider":provider,"authType":"cookie","apiKey":credential,"name":if email.is_empty() {config.name} else {email}, "email":if email.is_empty() {existing.and_then(|row|row.get("email")).cloned().unwrap_or(Value::Null)} else {json!(email)}, "providerSpecificData":specific,"isActive":true,"testStatus":"active"});
    if let Some(existing) = existing {
        patch["id"] = existing["id"].clone();
    }
    store
        .save_connection(&patch)
        .map_err(|_| error("Failed to save the captured provider session"))
}

struct Identity {
    status: &'static str,
    email: String,
}
impl Identity {
    fn new(status: &'static str, email: &str) -> Self {
        Self {
            status,
            email: email.to_owned(),
        }
    }
}

async fn response_body(request: reqwest::RequestBuilder) -> Option<(u16, String)> {
    let response = request.send().await.ok()?;
    let status = response.status().as_u16();
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if body.len() + chunk.len() > 2 * 1024 * 1024 {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some((status, String::from_utf8(body).ok()?))
}

async fn probe_identity(client: &reqwest::Client, provider: &str, credential: &str) -> Identity {
    let unknown = || Identity::new("unknown", "");
    let invalid = || Identity::new("invalid", "");
    let request = match provider {
        "deepseek-web" => client
            .get("https://chat.deepseek.com/api/v0/users/current")
            .bearer_auth(credential)
            .header("Origin", "https://chat.deepseek.com")
            .header("Referer", "https://chat.deepseek.com/")
            .header("X-Client-Bundle-Id", "com.deepseek.chat")
            .header("X-Client-Platform", "web")
            .header("X-Client-Version", "2.0.0"),
        "chatgpt-web" => client
            .get("https://chatgpt.com/api/auth/session")
            .header("Cookie", credential),
        "grok-web" => client
            .get("https://grok.com/api/auth/session")
            .header("Cookie", credential)
            .header("Origin", "https://grok.com")
            .header("Referer", "https://grok.com/"),
        "gemini-web" => client
            .get("https://gemini.google.com/app")
            .header("Cookie", credential),
        "muse-web" => client
            .get("https://muse.ai/api/session")
            .header("Cookie", credential)
            .header("Origin", "https://muse.ai")
            .header("Referer", "https://muse.ai/thread/new"),
        "uc-web" => client
            .get("https://clerk.uncensored.com/v1/client?_clerk_js_version=5.127.1")
            .header("Cookie", credential)
            .header("Origin", "https://uncensored.com")
            .header("Referer", "https://uncensored.com/"),
        _ => return unknown(),
    }
    .header(
        "Accept",
        if provider == "gemini-web" {
            "text/html,application/xhtml+xml"
        } else {
            "application/json"
        },
    );
    let Some((status, text)) = response_body(request).await else {
        return unknown();
    };
    if status == 401 || status == 403 {
        return invalid();
    }
    if !(200..300).contains(&status) {
        return unknown();
    }
    if provider == "muse-web" {
        return Identity::new("valid", "");
    }
    if provider == "gemini-web" {
        let valid = text.split("\"SNlM0e\"").skip(1).any(|part| {
            part.trim_start()
                .strip_prefix(':')
                .map(str::trim_start)
                .and_then(|part| part.strip_prefix('"'))
                .is_some_and(|part| !part.starts_with('"'))
        });
        return Identity::new(if valid { "valid" } else { "invalid" }, "");
    }
    let Ok(json) = serde_json::from_str::<Value>(&text) else {
        return unknown();
    };
    match provider {
        "chatgpt-web" => json
            .pointer("/user/email")
            .and_then(Value::as_str)
            .filter(|email| !email.is_empty())
            .map(|email| Identity::new("valid", email))
            .unwrap_or_else(invalid),
        "grok-web" => {
            if json.get("status").and_then(Value::as_str) == Some("authenticated") {
                Identity::new(
                    "valid",
                    json.pointer("/session/email")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            } else {
                invalid()
            }
        }
        "deepseek-web" => {
            if json.get("code").and_then(Value::as_i64) == Some(40003)
                || json
                    .pointer("/data/biz_data/token")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            {
                invalid()
            } else {
                Identity::new(
                    "valid",
                    json.pointer("/data/biz_data/email")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )
            }
        }
        "uc-web" => {
            let payload = json
                .get("response")
                .or_else(|| json.get("client"))
                .unwrap_or(&json);
            let sessions = payload.get("sessions").and_then(Value::as_array);
            let session = sessions.and_then(|sessions| {
                sessions
                    .iter()
                    .find(|session| session.get("status").and_then(Value::as_str) == Some("active"))
                    .or_else(|| sessions.first())
            });
            let Some(sid) = session
                .and_then(|session| session.get("id"))
                .and_then(Value::as_str)
                .filter(|sid| !sid.is_empty())
            else {
                return invalid();
            };
            let mut url = reqwest::Url::parse("https://clerk.uncensored.com/v1/client/sessions/")
                .expect("static Clerk URL");
            url.path_segments_mut()
                .expect("Clerk URL supports paths")
                .pop_if_empty()
                .push(sid)
                .push("tokens");
            url.query_pairs_mut()
                .append_pair("_clerk_js_version", "5.127.1");
            let Some((status, text)) = response_body(
                client
                    .post(url)
                    .header("Cookie", credential)
                    .header("Origin", "https://uncensored.com")
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body(""),
            )
            .await
            else {
                return unknown();
            };
            if status == 401 || status == 403 {
                return invalid();
            }
            if !(200..300).contains(&status) {
                return unknown();
            }
            match serde_json::from_str::<Value>(&text) {
                Ok(value)
                    if value
                        .get("jwt")
                        .and_then(Value::as_str)
                        .is_some_and(|token| !token.is_empty()) =>
                {
                    Identity::new("valid", "")
                }
                Ok(_) => invalid(),
                Err(_) => unknown(),
            }
        }
        _ => unknown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn harvests_muse_session_and_optional_cookies_only_from_provider_domain() {
        let cookies = vec![
            json!({"name":"hatch_sess","value":"secret","domain":".muse.ai"}),
            json!({"name":"hatch_gw","value":"extra","domain":"www.muse.ai"}),
            json!({"name":"intruder","value":"bad","domain":"evilmuse.ai"}),
            json!({"name":"intruder2","value":"bad","domain":"muse.ai.evil.com"}),
        ];
        assert_eq!(
            harvest_credential("muse-web", &cookies, None).as_deref(),
            Some("hatch_gw=extra; hatch_sess=secret")
        );
        assert_eq!(harvest_credential("muse-web", &cookies[1..], None), None);
    }

    #[test]
    fn accepts_contiguous_session_chunks_and_rejects_gaps_or_header_injection() {
        let chunk = |name, value| json!({"name":name,"value":value,"domain":"chatgpt.com"});
        let cookies = vec![
            chunk("__Secure-next-auth.session-token.1", "two"),
            chunk("__Secure-next-auth.session-token.0", "one"),
        ];
        assert!(harvest_credential("chatgpt-web", &cookies, None).is_some());
        assert_eq!(harvest_credential("chatgpt-web", &cookies[..1], None), None);
        assert_eq!(
            harvest_credential(
                "chatgpt-web",
                &[chunk(
                    "__Secure-next-auth.session-token",
                    "bad\r\nCookie: injected"
                )],
                None
            ),
            None
        );
        let gap = vec![
            chunk("__Secure-next-auth.session-token.0", "one"),
            chunk("__Secure-next-auth.session-token.2", "three"),
        ];
        assert_eq!(harvest_credential("chatgpt-web", &gap, None), None);
    }

    #[test]
    fn deepseek_unwraps_tokens_and_clerk_rejects_anonymous_sessions() {
        let raw = "a-valid-token-with-at-least-twenty-characters";
        assert_eq!(
            harvest_credential("deepseek-web", &[], Some(raw)).as_deref(),
            Some(raw)
        );
        let wrapped = json!({"value": raw, "__version":"0"}).to_string();
        assert_eq!(
            harvest_credential("deepseek-web", &[], Some(&wrapped)).as_deref(),
            Some(raw)
        );
        assert_eq!(harvest_credential("deepseek-web", &[], Some("short")), None);
        let mut clerk = vec![
            json!({"name":"__client","value":"client","domain":".uncensored.com"}),
            json!({"name":"__client_uat","value":"0","domain":".uncensored.com"}),
        ];
        assert_eq!(harvest_credential("uc-web", &clerk, None), None);
        clerk[1]["value"] = json!("100");
        assert!(harvest_credential("uc-web", &clerk, None).is_some());
    }

    #[test]
    fn profiles_are_bounded_single_path_components() {
        assert_eq!(sanitize_profile(""), "default");
        assert_eq!(
            sanitize_profile("  My account/../../name  "),
            "My-account-------name"
        );
        assert_eq!(sanitize_profile(&"a".repeat(100)).len(), 40);
        assert!(!sanitize_profile("../a\\b").contains(['/', '\\', '.']));
    }

    #[test]
    fn capture_refreshes_only_matching_web_login_accounts() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = Store::create(&dir.path().join("router.db")).unwrap();
        let manual = store.save_connection(&json!({"provider":"muse-web","authType":"cookie","name":"Manual","apiKey":"unrelated"})).unwrap();
        let first = save_capture(
            &mut store,
            "muse-web",
            "first",
            "hatch_sess=first",
            "first@example.com",
        )
        .unwrap();
        store.save_connection(&json!({"id":first["id"],"priority":9,"providerSpecificData":{"authMethod":"web-login","custom":"preserved"}})).unwrap();
        let refreshed = save_capture(
            &mut store,
            "muse-web",
            "first",
            "hatch_sess=refreshed",
            "first@example.com",
        )
        .unwrap();
        assert_eq!(first["id"], refreshed["id"]);
        assert_eq!(refreshed["priority"], 9);
        assert_eq!(refreshed["providerSpecificData"]["custom"], "preserved");
        assert_eq!(refreshed["apiKey"], "hatch_sess=refreshed");
        let second = save_capture(
            &mut store,
            "muse-web",
            "second",
            "hatch_sess=second",
            "second@example.com",
        )
        .unwrap();
        assert_ne!(first["id"], second["id"]);
        let rows = store.connections(Some("muse-web")).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter().find(|row| row["id"] == manual["id"]).unwrap()["apiKey"],
            "unrelated"
        );
        let anonymous =
            save_capture(&mut store, "muse-web", "third", "hatch_sess=third", "").unwrap();
        assert_ne!(anonymous["id"], second["id"]);
        assert_eq!(store.connections(Some("muse-web")).unwrap().len(), 4);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_cdp_capture_saves_muse_and_closes_its_browser() {
        use std::os::unix::fs::PermissionsExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut closed = false;
            while let Some(Ok(Message::Text(message))) = socket.next().await {
                let message: Value = serde_json::from_str(&message).unwrap();
                let result = match message["method"].as_str().unwrap() {
                    "Storage.getCookies" => {
                        json!({"cookies":[{"domain":".muse.ai","name":"hatch_sess","value":"fixture-session"},{"domain":"muse.ai.evil.invalid","name":"ignore","value":"foreign"}]})
                    }
                    "Browser.close" => {
                        closed = true;
                        json!({})
                    }
                    method => panic!("Unexpected CDP method: {method}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":message["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
                if closed {
                    break;
                }
            }
            closed
        });
        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("fixture-browser");
        fs::write(&executable, format!("#!/bin/sh\nfor arg do\ncase \"$arg\" in --user-data-dir=*) profile=\"${{arg#--user-data-dir=}}\";; esac\ndone\nprintf '%s\\n%s\\n' '{port}' '/devtools/browser/fixture' > \"$profile/DevToolsActivePort\"\nprintf '%s' \"$$\" > \"$profile/owned-child.pid\"\nexec /bin/sleep 60\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let profile = dir.path().join("browser-profiles/fixture");
        let credential = capture_browser(&profile, "muse-web", Some(&executable))
            .await
            .unwrap();
        assert_eq!(credential, "hatch_sess=fixture-session");
        assert!(server.await.unwrap());
        let mut store = Store::create(&dir.path().join("router.db")).unwrap();
        let saved = save_capture(&mut store, "muse-web", "fixture", &credential, "").unwrap();
        assert_eq!(saved["authType"], "cookie");
        assert_eq!(saved["providerSpecificData"]["authMethod"], "web-login");
        assert_eq!(
            store.connections(Some("muse-web")).unwrap()[0]["apiKey"],
            credential
        );
        let pid = fs::read_to_string(profile.join("owned-child.pid")).unwrap();
        assert!(
            !Command::new("/bin/kill")
                .args(["-0", &pid])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }

    #[tokio::test]
    async fn deepseek_does_not_attach_to_lookalike_origins() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let Some(Ok(Message::Text(message))) = socket.next().await else {
                panic!("missing CDP command");
            };
            let message: Value = serde_json::from_str(&message).unwrap();
            assert_eq!(message["method"], "Target.getTargets");
            socket.send(Message::Text(json!({"id":message["id"],"result":{"targetInfos":[{"type":"page","url":"https://chat.deepseek.com.evil.invalid/","targetId":"evil"},{"type":"page","url":"http://chat.deepseek.com/","targetId":"insecure"}]}}).to_string().into())).await.unwrap();
        });
        let mut cdp = Cdp::connect(&format!("ws://{address}/devtools/browser/fixture"))
            .await
            .unwrap();
        assert!(!cdp.attach_page("https://chat.deepseek.com").await.unwrap());
        server.await.unwrap();
    }
}
