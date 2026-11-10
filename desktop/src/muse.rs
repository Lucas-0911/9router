//! Muse Web uses the user's browser session, never an API key.
use crate::{
    Result,
    web_login::{Browser, Cdp},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::{Instant, sleep};

const SITE: &str = "https://muse.ai";
const THREAD_URL: &str = "https://muse.ai/thread/new";
const MAX_MEDIA: usize = 32 * 1024 * 1024;

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn error(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::other(message).into()
}
fn script(name: &str) -> String {
    // Generated directly from the original provider's DOM expressions.
    serde_json::from_str::<Value>(include_str!("../muse-dom.json"))
        .expect("checked Muse DOM snapshot")[name]
        .as_str()
        .expect("DOM expression")
        .to_owned()
}
fn template(name: &str, values: &[(&str, &str)]) -> Result<String> {
    let mut expression = script(name);
    for (key, value) in values {
        expression =
            expression.replace(&serde_json::to_string(key)?, &serde_json::to_string(value)?);
    }
    Ok(expression)
}

fn cookie_jar(raw: &str) -> Result<BTreeMap<String, String>> {
    let mut jar = BTreeMap::new();
    for entry in raw.split(';') {
        let Some((name, value)) = entry.trim().split_once('=') else {
            continue;
        };
        if [
            "path", "domain", "expires", "max-age", "secure", "httponly", "samesite",
        ]
        .contains(&name.to_ascii_lowercase().as_str())
        {
            continue;
        }
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || value.chars().any(char::is_control)
        {
            return Err(error("Invalid Muse session cookie"));
        }
        if !value.is_empty() {
            jar.insert(name.to_owned(), value.to_owned());
        }
    }
    if !jar.contains_key("hatch_sess") {
        return Err(error(
            "Muse Web requires browser login; open Login via Browser on its provider page",
        ));
    }
    Ok(jar)
}
fn cookie_header(jar: &BTreeMap<String, String>) -> String {
    jar.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ")
}

async fn bounded(response: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(error("Upstream response exceeds the size limit"));
    }
    let mut stream = response.bytes_stream();
    let mut out = Vec::new();
    while let Some(part) = stream.next().await {
        let part = part?;
        if out.len() + part.len() > max {
            return Err(error("Upstream response exceeds the size limit"));
        }
        out.extend_from_slice(&part);
    }
    Ok(out)
}

pub(crate) async fn renew(client: &reqwest::Client, raw: &str) -> Result<String> {
    renew_at(client, raw, SITE).await
}
async fn renew_at(client: &reqwest::Client, raw: &str, site: &str) -> Result<String> {
    let mut jar = cookie_jar(raw)?;
    let response=client.get(format!("{site}/api/session")).header("cookie",cookie_header(&jar))
        .header("origin",site).header("referer",format!("{site}/thread/new")).header("accept","application/json")
        .header("user-agent","Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36")
        .timeout(Duration::from_secs(15)).send().await?;
    if matches!(response.status().as_u16(), 401 | 403) {
        return Err(error(
            "Muse Web session expired; sign in again with Login via Browser",
        ));
    }
    if !response.status().is_success() {
        return Err(error("Muse Web session renewal failed"));
    }
    for line in response.headers().get_all("set-cookie") {
        if let Ok(line) = line.to_str()
            && let Some((key, value)) = line.split(';').next().unwrap_or_default().split_once('=')
        {
            // Never let a Set-Cookie attribute become a Cookie header injection.
            if !key.is_empty()
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
                && !value.chars().any(char::is_control)
            {
                if value.is_empty() {
                    jar.remove(key);
                } else {
                    jar.insert(key.to_owned(), value.to_owned());
                }
            }
        }
    }
    let body: Value =
        serde_json::from_slice(&bounded(response, 2 * 1024 * 1024).await?).unwrap_or(Value::Null);
    if body["vm_id"].is_string() && body["vm_state"] != "DISABLED" {
        let _=client.post(format!("{site}/api/hatch/vm/wake")).header("cookie",cookie_header(&jar)).header("origin",site).header("referer",format!("{site}/thread/new"))
            .json(&json!({"vm_id":body["vm_id"],"retry_count":0,"connect_attempt_id":uuid::Uuid::new_v4().to_string()})).timeout(Duration::from_secs(15)).send().await;
    }
    if !jar.contains_key("hatch_sess") {
        return Err(error(
            "Muse Web session expired; sign in again with Login via Browser",
        ));
    }
    Ok(cookie_header(&jar))
}

pub(crate) fn flatten(messages: &[Value]) -> Result<(String, Vec<String>)> {
    let mut turns = Vec::new();
    let mut images = Vec::new();
    for message in messages {
        let fields = message
            .as_object()
            .ok_or_else(|| error("Invalid Muse chat message"))?;
        if fields
            .iter()
            .any(|(key, value)| !["role", "content"].contains(&key.as_str()) && !value.is_null())
        {
            return Err(error(
                "Muse Web cannot represent message tools, reasoning or extra fields",
            ));
        }
        let role = message["role"]
            .as_str()
            .filter(|role| ["system", "developer", "user", "assistant"].contains(role))
            .ok_or_else(|| error("Unsupported Muse chat role"))?;
        let text = if let Some(text) = message["content"].as_str() {
            text.to_owned()
        } else if let Some(parts) = message["content"].as_array() {
            let mut text = Vec::new();
            for part in parts {
                match part["type"].as_str() {
                    Some("text" | "input_text" | "output_text") => {
                        text.push(
                            part["text"]
                                .as_str()
                                .ok_or_else(|| error("Invalid Muse text block"))?,
                        );
                    }
                    Some("image_url" | "input_image") => {
                        let value = part["image_url"]
                            .as_str()
                            .or_else(|| part["image_url"]["url"].as_str())
                            .ok_or_else(|| error("Invalid Muse image block"))?;
                        images.push(value.to_owned());
                    }
                    _ => return Err(error("Unsupported Muse content block")),
                }
            }
            text.join("\n")
        } else {
            return Err(error("Muse message content must be text or content blocks"));
        };
        if !text.trim().is_empty() {
            turns.push((role, text.trim().to_owned()));
        }
    }
    if turns.is_empty() {
        return Err(error("No user message found"));
    }
    if turns.len() == 1 && turns[0].0 == "user" {
        return Ok((turns.remove(0).1, images));
    }
    let mut blocks = turns
        .into_iter()
        .map(|(role, text)| {
            format!(
                "[{}]\n{text}",
                match role {
                    "system" | "developer" => "System",
                    "user" => "User",
                    "assistant" => "Assistant",
                    "tool" => "Tool",
                    other => other,
                }
            )
        })
        .collect::<Vec<_>>();
    blocks.push("[Instruction]\nContinue the conversation above and reply as the Assistant to the last User message. Output only the reply.".into());
    Ok((blocks.join("\n\n"), images))
}

pub(crate) struct Thread {
    cdp: Cdp,
    _browser: Browser,
    _directory: tempfile::TempDir,
    base: Value,
    emitted: String,
    stable: u32,
    started: Instant,
}
impl Thread {
    pub(crate) async fn open(directory: &Path, cookie: &str) -> Result<Self> {
        std::fs::create_dir_all(directory)?;
        let temporary = tempfile::Builder::new()
            .prefix("muse-runtime-")
            .tempdir_in(directory)?;
        let (browser, mut cdp) = Browser::launch(temporary.path(), "about:blank", true).await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cdp.attach_page("null").await? {
            if Instant::now() > deadline {
                return Err(error("Muse browser did not open a page"));
            }
            sleep(Duration::from_millis(100)).await;
        }
        let cookies=cookie_jar(cookie)?.into_iter().map(|(name,value)|json!({"name":name,"value":value,"domain":".muse.ai","path":"/","secure":true,"httpOnly":false,"sameSite":"Lax"})).collect::<Vec<_>>();
        cdp.call("Network.setCookies", json!({"cookies":cookies}))
            .await?;
        cdp.call("Page.navigate", json!({"url":THREAD_URL})).await?;
        let mut thread = Self {
            cdp,
            _browser: browser,
            _directory: temporary,
            base: Value::Null,
            emitted: String::new(),
            stable: 0,
            started: Instant::now(),
        };
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            let state = match thread.cdp.evaluate(&script("PAGE_STATE")).await {
                Ok(state) => state,
                Err(_) if Instant::now() < deadline => {
                    sleep(Duration::from_millis(250)).await;
                    continue;
                }
                Err(_) => return Err(error("Muse Web chat page could not be loaded")),
            };
            if state["ready"] == true {
                break;
            }
            if state["hasInput"] != true
                && state["head"].as_str().is_some_and(|s| {
                    let lower = s.to_ascii_lowercase();
                    [
                        "log in",
                        "sign in",
                        "create an account",
                        "use another account",
                    ]
                    .iter()
                    .any(|hint| lower.contains(hint))
                })
            {
                return Err(error(
                    "Muse Web session expired; sign in again with Login via Browser",
                ));
            }
            if Instant::now() > deadline {
                return Err(error(
                    "Muse Web chat page did not become ready within 45 seconds",
                ));
            }
            sleep(Duration::from_millis(250)).await;
        }
        thread.base = thread.state().await?;
        Ok(thread)
    }
    async fn state(&mut self) -> Result<Value> {
        let state = self.cdp.evaluate(&script("CHAT_STATE")).await?;
        let lower = state["tail"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if [
            "out of credits",
            "usage limit",
            "limit reached",
            "token limit",
        ]
        .iter()
        .any(|s| lower.contains(s))
        {
            return Err(error("Muse Web account quota has been reached"));
        }
        Ok(state)
    }
    pub(crate) async fn send(&mut self, prompt: &str, images: &[String]) -> Result<()> {
        if images.len() > 8 {
            return Err(error("Muse Web supports up to eight input images"));
        }
        for (index, image) in images.iter().enumerate() {
            let (mime, b64) = image_data(image).await?;
            let filename = format!(
                "input_{index}.{}",
                mime.strip_prefix("image/")
                    .unwrap_or("png")
                    .replace("jpeg", "jpg")
            );
            let result = self
                .cdp
                .evaluate(&template(
                    "attachFile",
                    &[
                        ("__9ROUTER_BASE64__", &b64),
                        ("__9ROUTER_MIME__", &mime),
                        ("__9ROUTER_FILENAME__", &filename),
                    ],
                )?)
                .await?;
            if result["ok"] != true {
                return Err(error("Muse Web could not attach the input image"));
            }
        }
        if !images.is_empty() {
            sleep(Duration::from_secs(1)).await;
        }
        let result = self
            .cdp
            .evaluate(&template("fillInput", &[("__9ROUTER_TEXT__", prompt)])?)
            .await?;
        if result["ok"] != true {
            return Err(error("Muse Web chat input could not be filled"));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.cdp.evaluate(&script("CLICK_SEND")).await? == "clicked" {
                self.started = Instant::now();
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
        self.cdp.call("Input.dispatchKeyEvent",json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"text":"\r"})).await?;
        self.cdp
            .call(
                "Input.dispatchKeyEvent",
                json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13}),
            )
            .await?;
        sleep(Duration::from_millis(500)).await;
        if self.cdp.evaluate(&script("INPUT_EMPTY")).await? != true {
            return Err(error("Muse Web did not submit the prompt"));
        }
        self.started = Instant::now();
        Ok(())
    }
    pub(crate) async fn delta(&mut self) -> Result<Option<String>> {
        loop {
            if self.started.elapsed() > Duration::from_secs(300) {
                return Err(error("Muse Web assistant reply timed out"));
            }
            sleep(Duration::from_millis(300)).await;
            let state = self.state().await?;
            if state["approval"] == true {
                self.cdp.evaluate(&script("DENY_APPROVALS")).await?;
                continue;
            }
            let text = state["lastText"].as_str().unwrap_or_default();
            let is_new = state["agentCount"].as_u64().unwrap_or(0)
                > self.base["agentCount"].as_u64().unwrap_or(0)
                || (!text.is_empty() && state["lastText"] != self.base["lastText"]);
            if !is_new || text.is_empty() {
                if self.started.elapsed() > Duration::from_secs(45) {
                    return Err(error("Muse Web did not return a first token"));
                }
                continue;
            }
            if text != self.emitted {
                let delta = text
                    .strip_prefix(&self.emitted)
                    .ok_or_else(|| error("Muse Web edited previously emitted text"))?
                    .to_owned();
                self.emitted = text.to_owned();
                self.stable = 0;
                return Ok(Some(delta));
            }
            self.stable += 1;
            if state["generating"] != true && self.stable >= 4 {
                return Ok(None);
            }
        }
    }
    pub(crate) async fn complete(&mut self) -> Result<String> {
        while self.delta().await?.is_some() {}
        Ok(self.emitted.clone())
    }
    pub(crate) async fn media(&mut self, kind: &str) -> Result<Value> {
        let base_attachments = self.base["attachments"].as_array().map_or(0, Vec::len);
        let limit = Duration::from_secs(if kind == "video" { 600 } else { 240 });
        let grace = Duration::from_secs(if kind == "video" { 90 } else { 8 });
        let started = Instant::now();
        let mut text_done = None;
        let mut asked = false;
        loop {
            sleep(Duration::from_millis(600)).await;
            let state = self.state().await?;
            if state["approval"] == true {
                self.cdp.evaluate(&script("DENY_APPROVALS")).await?;
                text_done = None;
                continue;
            }
            let attachments = state["attachments"]
                .as_array()
                .map_or(&[][..], Vec::as_slice);
            let fresh = attachments.iter().skip(base_attachments);
            if let Some(attachment) = fresh
                .clone()
                .rfind(|a| a["kind"] == kind && a["src"].as_str().is_some_and(|s| !s.is_empty()))
            {
                return self.download(attachment, kind).await;
            }
            let busy = state["generating"] == true
                || fresh
                    .clone()
                    .any(|a| a["tid"].as_str().is_some_and(|s| s.contains(kind)));
            if started.elapsed() > limit && (!busy || started.elapsed() > limit.mul_f64(2.5)) {
                return Err(error("Muse Web media generation timed out"));
            }
            if fresh
                .clone()
                .any(|a| a["tid"].as_str().is_some_and(|s| s.contains(kind)))
            {
                text_done = None;
                continue;
            }
            if state["agentCount"].as_u64().unwrap_or(0)
                > self.base["agentCount"].as_u64().unwrap_or(0)
                && state["lastText"].as_str().is_some_and(|s| !s.is_empty())
                && state["generating"] != true
            {
                let done = text_done.get_or_insert_with(Instant::now);
                if done.elapsed() > grace {
                    if !asked {
                        self.base["agentCount"] = state["agentCount"].clone();
                        self.send(&format!("Please attach the final {kind} file here in the chat, as a single attachment."),&[]).await?;
                        asked = true;
                        text_done = None;
                        continue;
                    }
                    let probe = self.cdp.evaluate(&script("LAST_BUBBLE_MEDIA")).await?;
                    if let Some(url) = probe["links"]
                        .as_array()
                        .and_then(|links| pick_media(links, kind))
                    {
                        return self.download(&json!({"src":url}), kind).await;
                    }
                    return Err(error(
                        "Muse Web replied without a generated media attachment",
                    ));
                }
            } else {
                text_done = None;
            }
        }
    }
    async fn download(&mut self, attachment: &Value, kind: &str) -> Result<Value> {
        let url = attachment["src"]
            .as_str()
            .ok_or_else(|| error("Muse Web attachment URL is missing"))?;
        if !valid_media_url(url) {
            return Err(error("Muse Web attachment URL is invalid"));
        }
        let local = url.starts_with("blob:https://muse.ai/")
            || reqwest::Url::parse(url).is_ok_and(|u| {
                u.host_str() == Some("muse.ai") && u.port_or_known_default() == Some(443)
            });
        if !local {
            let (mime, b64) = public_media(url, &format!("{kind}/")).await?;
            return Ok(
                json!({"b64":b64,"mime":mime,"width":attachment["w"],"height":attachment["h"]}),
            );
        }
        let quoted = serde_json::to_string(url)?;
        let expression=template("fetchMedia",&[("__9ROUTER_URL__",url)])?
            .replace(&format!("fetch({quoted})"),&format!("fetch({quoted}, {{redirect:'error',credentials:'same-origin'}})"))
            .replace("const b = await r.blob();", "if (!r.ok) throw Error('download failed'); const reader=r.body.getReader(); const parts=[]; let total=0; for (;;) { const {done,value}=await reader.read(); if(done) break; total+=value.length; if(total>33554432) { await reader.cancel(); throw Error('media too large'); } parts.push(value); } const b=new Blob(parts,{type:r.headers.get('content-type')||''});");
        let result = self.cdp.evaluate(&expression).await?;
        let b64 = result["b64"]
            .as_str()
            .filter(|b| b.len() <= MAX_MEDIA * 4 / 3 + 4)
            .ok_or_else(|| {
                error("Muse Web attachment could not be downloaded within the size limit")
            })?;
        if result["ok"] != true
            || STANDARD.decode(b64).is_err()
            || !result["mime"]
                .as_str()
                .is_some_and(|mime| mime.starts_with(&format!("{kind}/")))
        {
            return Err(error("Muse Web attachment download failed"));
        }
        Ok(
            json!({"b64":b64,"mime":result["mime"].as_str().filter(|s|!s.is_empty()).unwrap_or(if kind=="video"{"video/mp4"}else{"image/png"}),"width":attachment["w"],"height":attachment["h"]}),
        )
    }
}

fn valid_media_url(raw: &str) -> bool {
    if raw.starts_with("blob:https://muse.ai/") {
        return true;
    }
    reqwest::Url::parse(raw).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none()
    })
}
fn pick_media<'a>(links: &'a [Value], kind: &str) -> Option<&'a str> {
    let exts = if kind == "video" {
        &[".mp4", ".webm", ".mov", ".m4v"][..]
    } else {
        &[".png", ".jpg", ".jpeg", ".webp", ".gif", ".avif"][..]
    };
    links
        .iter()
        .filter_map(Value::as_str)
        .filter(|s| valid_media_url(s))
        .filter_map(|url| {
            let lower = url.to_ascii_lowercase();
            let path = lower.split(['?', '#']).next().unwrap_or_default();
            let score = if lower.starts_with("blob:") || exts.iter().any(|e| path.ends_with(e)) {
                3
            } else if [
                "videodelivery.net",
                "cloudflarestream.com",
                "cloudflarestorage.com",
                "amazonaws.com",
                "storage.googleapis.com",
                "blob.core.windows.net",
                "musecdn",
                "cdn.muse",
                "muse-cdn",
            ]
            .iter()
            .any(|h| lower.contains(h))
            {
                2
            } else if [
                "/media/",
                "/download",
                "/dl/",
                "/video",
                "/attachment",
                "/files/",
            ]
            .iter()
            .any(|p| lower.contains(p))
            {
                1
            } else {
                0
            };
            (score > 0).then_some((score, url))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, url)| url)
}
async fn image_data(raw: &str) -> Result<(String, String)> {
    if let Some(data) = raw.strip_prefix("data:") {
        let (mime, b64) = data
            .split_once(";base64,")
            .ok_or_else(|| error("Input image must be base64 encoded"))?;
        if !mime.starts_with("image/")
            || b64.len() > MAX_MEDIA * 4 / 3 + 4
            || STANDARD.decode(b64).is_err()
        {
            return Err(error("Invalid input image"));
        }
        return Ok((mime.to_owned(), b64.to_owned()));
    }
    public_media(raw, "image/").await
}

async fn public_media(raw: &str, expected: &str) -> Result<(String, String)> {
    let url = reqwest::Url::parse(raw)?;
    if url.scheme() != "https" || !url.username().is_empty() || url.password().is_some() {
        return Err(error("Input image URL must use HTTPS"));
    }
    let host = url.host_str().ok_or_else(|| error("Invalid image host"))?;
    let addresses = tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443)))
        .await?
        .collect::<Vec<_>>();
    if addresses.is_empty()
        || addresses.iter().any(|address| match address.ip() {
            std::net::IpAddr::V4(ip) => {
                ip.is_private()
                    || ip.is_loopback()
                    || ip.is_link_local()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.is_broadcast()
                    || ip.octets()[0] == 0
            }
            std::net::IpAddr::V6(ip) => {
                ip.is_loopback()
                    || ip.is_unique_local()
                    || ip.is_unicast_link_local()
                    || ip.is_unspecified()
                    || ip.is_multicast()
                    || ip.to_ipv4_mapped().is_some()
            }
        })
    {
        return Err(error("Input image URLs must resolve to public addresses"));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve_to_addrs(host, &addresses)
        .timeout(Duration::from_secs(30))
        .build()?;
    let response = client
        .get(url)
        .header("accept", format!("{expected}*"))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(error("Input image download failed"));
    }
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .to_owned();
    if !mime.starts_with(expected) {
        return Err(error("Media URL returned an unexpected content type"));
    }
    Ok((mime, STANDARD.encode(bounded(response, MAX_MEDIA).await?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn upstream_prompt_cookie_and_media_rules() {
        assert_eq!(
            flatten(&[json!({"role":"user","content":"Hello"})]).unwrap(),
            ("Hello".into(), vec![])
        );
        let (text,images)=flatten(&[json!({"role":"system","content":"Be precise"}),json!({"role":"user","content":[{"type":"text","text":"Describe"},{"type":"image_url","image_url":{"url":"https://example.org/x.png"}}]})]).unwrap();
        assert_eq!(
            text,
            "[System]\nBe precise\n\n[User]\nDescribe\n\n[Instruction]\nContinue the conversation above and reply as the Assistant to the last User message. Output only the reply."
        );
        assert_eq!(images, vec!["https://example.org/x.png"]);
        assert!(flatten(&[]).is_err());
        assert!(flatten(&[json!({"role":"user","content":[{"type":"input_audio","input_audio":{"data":"hidden"}}]})]).is_err());
        assert!(
            flatten(&[json!({"role":"assistant","content":"Text","tool_calls":[{"id":"call1"}]})])
                .is_err()
        );
        assert!(
            flatten(&[json!({"role":"assistant","content":"Text","reasoning_content":"private"})])
                .is_err()
        );
        let jar = cookie_jar("hatch_sess=a=b; Path=/; hatch_gw=old").unwrap();
        assert_eq!(jar.len(), 2);
        assert_eq!(jar["hatch_sess"], "a=b");
        assert!(cookie_jar("hatch_gw=only").is_err());
        assert!(cookie_jar("hatch_sess=bad\r\nheader").is_err());
        assert_eq!(
            pick_media(
                &[
                    json!("https://example.org/readme"),
                    json!("https://cdn.example.org/x.mp4?signature=123")
                ],
                "video"
            ),
            Some("https://cdn.example.org/x.mp4?signature=123")
        );
        assert!(!valid_media_url("javascript:alert(1)"));
        let filled = template(
            "fillInput",
            &[("__9ROUTER_TEXT__", "\");throw Error('injection');//")],
        )
        .unwrap();
        assert!(filled.contains("\\\""));
    }
    #[tokio::test]
    async fn session_renewal_rotates_cookies_without_leaking_them() {
        use axum::{Router, http::HeaderMap, routing::get};
        let app = Router::new().route(
            "/api/session",
            get(|headers: HeaderMap| async move {
                assert!(
                    headers["cookie"]
                        .to_str()
                        .unwrap()
                        .contains("hatch_sess=valid")
                );
                (
                    [("set-cookie", "hatch_gw=rotated; Path=/; HttpOnly")],
                    axum::Json(json!({"status":"assigned","vm_state":"DISABLED"})),
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cookie = renew_at(
            &reqwest::Client::new(),
            "hatch_sess=valid; hatch_gw=stale",
            &base,
        )
        .await
        .unwrap();
        assert_eq!(cookie, "hatch_gw=rotated; hatch_sess=valid");
        server.abort();
    }

    #[tokio::test]
    #[ignore = "Requires an installed Chrome/Chromium executable; uses only a disposable local page"]
    async fn real_chrome_runs_original_selectors_and_releases_owned_runtime() {
        let directory = tempfile::tempdir().unwrap();
        let (browser, mut cdp) = Browser::launch(directory.path(), "about:blank", true)
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cdp.attach_page("null").await.unwrap() {
            assert!(Instant::now() < deadline);
            sleep(Duration::from_millis(50)).await;
        }
        cdp.evaluate(r#"document.body.innerHTML='<textarea></textarea><button aria-label="Send">Send</button>';document.querySelector('button').onclick=()=>{let prompt=document.querySelector('textarea').value;document.querySelector('textarea').value='';let b=document.createElement('div');b.className='hatch-agent-bubble-bg';b.textContent='Answer: '+prompt;document.body.appendChild(b)};true"#).await.unwrap();
        assert_eq!(
            cdp.evaluate(&script("PAGE_STATE")).await.unwrap()["ready"],
            true
        );
        let base = cdp.evaluate(&script("CHAT_STATE")).await.unwrap();
        let mut thread = Thread {
            cdp,
            _browser: browser,
            _directory: directory,
            base,
            emitted: String::new(),
            stable: 0,
            started: Instant::now(),
        };
        thread.send("fixture prompt", &[]).await.unwrap();
        assert_eq!(thread.complete().await.unwrap(), "Answer: fixture prompt");
        thread
            .cdp
            .evaluate(r#"document.querySelector('button').setAttribute('aria-label','Stop')"#)
            .await
            .unwrap();
        thread.stable = 12;
        assert!(
            tokio::time::timeout(Duration::from_millis(800), thread.delta())
                .await
                .is_err(),
            "A stable reply must not finish while Muse is generating"
        );
        thread
            .cdp
            .evaluate(r#"document.querySelector('button').setAttribute('aria-label','Send')"#)
            .await
            .unwrap();
        assert!(thread.delta().await.unwrap().is_none());
        let data = "data:image/png;base64,aGVsbG8=";
        assert_eq!(
            image_data(data).await.unwrap(),
            ("image/png".into(), "aGVsbG8=".into())
        );
        assert!(image_data("https://127.0.0.1/private-image").await.is_err());
        thread
            .cdp
            .evaluate(
                r#"document.querySelector('.hatch-agent-bubble-bg').textContent='out of credits'"#,
            )
            .await
            .unwrap();
        assert!(
            thread
                .state()
                .await
                .unwrap_err()
                .to_string()
                .contains("quota")
        );
    }
}
