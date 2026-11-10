//! Codex Responses transport and loss-checked, non-streaming Chat conversion.
//! Defaults are snapshotted from the original executor; no JavaScript runtime runs.
use crate::Result;
use futures_util::StreamExt;
use reqwest::{
    Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io, sync::OnceLock};

const MAX_RESPONSE: usize = 32 * 1024 * 1024;
fn invalid(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}
fn unsupported(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::new(io::ErrorKind::Unsupported, message).into()
}
fn contract() -> &'static Value {
    static VALUE: OnceLock<Value> = OnceLock::new();
    VALUE.get_or_init(|| {
        serde_json::from_str(include_str!("../codex-contract.json"))
            .expect("checked public Codex contract")
    })
}
fn text<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value.as_str().ok_or_else(|| invalid(field))
}
fn fields(value: &Value, allowed: &[&str]) -> Result<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("Expected a JSON object"))?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(unsupported(
            "Codex conversion contains an unsupported field",
        ));
    }
    Ok(())
}
fn array<'a>(value: &'a Value, field: &str) -> Result<&'a Vec<Value>> {
    value.as_array().ok_or_else(|| invalid(field))
}
fn insert(headers: &mut HeaderMap, name: &str, value: &str) -> Result<()> {
    headers.insert(
        HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| invalid("Invalid Codex header name"))?,
        HeaderValue::from_str(value).map_err(|_| invalid("Invalid Codex header value"))?,
    );
    Ok(())
}

pub struct Prepared {
    pub url: String,
    pub headers: HeaderMap,
    pub body: Value,
}

/// Only connection configuration can select an endpoint; request URLs are ignored.
pub fn prepare(connection: &Value, body: &Value) -> Result<Prepared> {
    let mut body = body
        .as_object()
        .cloned()
        .ok_or_else(|| invalid("Codex request must be an object"))?;
    if body.get("_compact") == Some(&json!(true)) {
        return Err(unsupported(
            "Codex compact requests are not supported by this adapter",
        ));
    }
    let token = connection["accessToken"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("Codex requires an OAuth access token"))?;
    let defaults = contract();
    let base = connection["providerSpecificData"]["baseUrl"]
        .as_str()
        .unwrap_or_else(|| defaults["transport"]["baseUrl"].as_str().unwrap());
    let url = Url::parse(base).map_err(|_| invalid("Invalid Codex endpoint"))?;
    if !matches!(url.scheme(), "https" | "http")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || (url.scheme() == "http"
            && !url.host_str().is_some_and(|host| {
                host == "localhost"
                    || host
                        .trim_matches(['[', ']'])
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            }))
    {
        return Err(invalid(
            "Codex endpoint must be HTTPS or loopback HTTP without credentials, query or fragment",
        ));
    }
    let requested = text(
        body.get("model").unwrap_or(&Value::Null),
        "Codex model must be a string",
    )?
    .to_owned();
    if requested.is_empty() || requested.chars().any(char::is_control) {
        return Err(invalid("Invalid Codex model"));
    }
    let levels = [
        "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
    ];
    let mut model = requested.as_str();
    let mut suffix = None;
    if let Some((base, level)) = model
        .rsplit_once('(')
        .filter(|(_, level)| level.ends_with(')'))
    {
        let level = level.trim_end_matches(')').trim();
        if levels.contains(&level) {
            model = base.trim();
            suffix = Some(level);
        }
    } else {
        for level in levels {
            if let Some(base) = model.strip_suffix(&format!("-{level}")) {
                model = base;
                suffix = Some(level);
                break;
            }
        }
    }
    let metadata = defaults["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["id"] == model);
    let model = metadata
        .and_then(|entry| entry["upstreamModelId"].as_str())
        .unwrap_or(model)
        .to_owned();
    let mut input = match body.remove("input") {
        Some(Value::String(value)) => vec![
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":if value.trim().is_empty(){"..."}else{&value}}]}),
        ],
        Some(Value::Array(value)) => value,
        Some(Value::Null) | None => Vec::new(),
        _ => return Err(invalid("Codex input must be a string or array")),
    };
    if input.is_empty() {
        input.push(
            json!({"type":"message","role":"user","content":[{"type":"input_text","text":"..."}]}),
        );
    }
    let has_search = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| tools.iter().any(|tool| tool["type"] == "web_search"));
    let lite = metadata.is_some_and(|entry| entry["responsesLite"] == true) && !has_search;
    input.retain(|item| item["type"] != "item_reference" && !item.as_str().is_some_and(server_id));
    for item in &mut input {
        let item = item
            .as_object_mut()
            .ok_or_else(|| invalid("Codex input items must be objects"))?;
        if item.get("role").is_some_and(|role| role == "system") {
            item.insert("role".into(), json!("developer"));
        }
        let keep_id = lite
            && item.get("role").is_some_and(|v| v == "developer")
            && item
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.starts_with("msg_"));
        if !keep_id
            && item
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(server_id)
        {
            item.remove("id");
        }
        if let Some(content) = item.get_mut("content").and_then(Value::as_array_mut) {
            for part in content {
                if part["type"] == "image_url" {
                    let source = part["image_url"]
                        .as_str()
                        .or_else(|| part["image_url"]["url"].as_str())
                        .ok_or_else(|| invalid("Invalid Codex image"))?;
                    if !source.starts_with("data:image/") {
                        return Err(unsupported(
                            "Codex remote images require a separately validated image fetch adapter",
                        ));
                    }
                    *part = json!({"type":"input_image","image_url":source,"detail":part["image_url"]["detail"].as_str().unwrap_or("auto")});
                }
            }
        }
    }
    if let Some(tools) = body.get_mut("tools").filter(|v| !v.is_null()) {
        for tool in tools
            .as_array_mut()
            .ok_or_else(|| invalid("Codex tools must be an array"))?
        {
            normalize_tool(tool)?;
        }
    }
    let session = ["prompt_cache_key", "session_id", "conversation_id"]
        .iter()
        .find_map(|key| {
            body.get(*key)
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
        })
        .map(str::to_owned)
        .unwrap_or_else(|| {
            // ponytail: fallback cache identity is per connection; add client-session headers when exposed by the gateway.
            format!(
                "{:x}",
                Sha256::digest(format!(
                    "codex:{}",
                    connection["id"].as_str().unwrap_or("default")
                ))
            )[..32]
                .to_owned()
        });
    if session.len() > 256 || session.chars().any(char::is_control) {
        return Err(invalid("Invalid Codex cache session id"));
    }
    let instructions = body
        .get("instructions")
        .filter(|v| !v.is_null())
        .map(|value| text(value, "Codex instructions must be text"))
        .transpose()?
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| defaults["instructions"].as_str().unwrap())
        .to_owned();
    let mut reasoning = body
        .remove("reasoning")
        .filter(|v| !v.is_null())
        .unwrap_or(json!({}));
    if !reasoning.is_object() {
        return Err(invalid("Codex reasoning must be an object"));
    }
    let effort = reasoning["effort"]
        .as_str()
        .or_else(|| body.get("reasoning_effort").and_then(Value::as_str))
        .or(suffix)
        .unwrap_or(if lite { "medium" } else { "low" });
    let supported = metadata.and_then(|entry| entry["thinkingLevels"].as_array());
    let effort = if lite && matches!(effort, "none" | "minimal") {
        "low"
    } else if effort == "ultra" {
        if supported.is_some_and(|levels| levels.iter().any(|v| v == "max")) {
            "max"
        } else {
            "xhigh"
        }
    } else if effort == "max" && !supported.is_some_and(|levels| levels.iter().any(|v| v == "max"))
    {
        "xhigh"
    } else {
        effort
    };
    if !levels.contains(&effort) {
        return Err(invalid("Invalid Codex reasoning effort"));
    }
    reasoning["effort"] = json!(effort);
    if lite {
        reasoning["context"] = json!("all_turns");
        if !input.iter().any(|item| item["type"] == "additional_tools") {
            input.insert(0, json!({"type":"message","role":"developer","content":[{"type":"input_text","text":instructions}]}));
            input.insert(0, json!({"type":"additional_tools","role":"developer","tools":body.get("tools").cloned().unwrap_or(json!([]))}));
        }
        body.insert("instructions".into(), json!(""));
        body.insert("tools".into(), Value::Null);
        body.entry("tool_choice").or_insert(json!("auto"));
        body.insert("parallel_tool_calls".into(), json!(false));
    } else {
        body.insert("instructions".into(), json!(instructions));
        body.remove("parallel_tool_calls");
        reasoning
            .as_object_mut()
            .unwrap()
            .entry("summary")
            .or_insert(json!("auto"));
    }
    let encrypted = reasoning["effort"] != "none";
    body.insert("reasoning".into(), reasoning);
    if encrypted {
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    body.insert("input".into(), json!(input));
    body.insert("model".into(), json!(model));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    body.insert("prompt_cache_key".into(), json!(session));
    if body.get("service_tier").is_some_and(|v| v == "fast") {
        body.insert("service_tier".into(), json!("priority"));
    }
    if body.get("service_tier").is_some_and(|v| v != "priority") {
        body.remove("service_tier");
    }
    let allow = [
        "model",
        "input",
        "instructions",
        "tools",
        "tool_choice",
        "stream",
        "store",
        "reasoning",
        "service_tier",
        "include",
        "prompt_cache_key",
        "client_metadata",
        "text",
        "parallel_tool_calls",
    ];
    body.retain(|key, _| allow.contains(&key.as_str()));
    let mut headers = HeaderMap::new();
    for (key, value) in defaults["transport"]["headers"].as_object().unwrap() {
        insert(&mut headers, key, value.as_str().unwrap())?;
    }
    insert(&mut headers, "content-type", "application/json")?;
    insert(&mut headers, "accept", "text/event-stream")?;
    insert(&mut headers, "authorization", &format!("Bearer {token}"))?;
    insert(&mut headers, "session_id", &session)?;
    if lite {
        insert(
            &mut headers,
            "x-openai-internal-codex-responses-lite",
            "true",
        )?;
    }
    if let Some(account) = ["workspaceId", "chatgptAccountId", "accountId"]
        .iter()
        .find_map(|key| {
            connection["providerSpecificData"][*key]
                .as_str()
                .filter(|s| !s.is_empty())
        })
    {
        insert(&mut headers, "ChatGPT-Account-ID", account)?;
    }
    Ok(Prepared {
        url: url.to_string(),
        headers,
        body: Value::Object(body),
    })
}
fn server_id(value: &str) -> bool {
    ["rs_", "fc_", "resp_", "msg_"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}
fn normalize_tool(tool: &mut Value) -> Result<()> {
    match tool["type"].as_str() {
        Some("function") => {
            let function = tool
                .get("function")
                .filter(|v| v.is_object())
                .unwrap_or(tool);
            let name = text(&function["name"], "Codex function tool requires a name")?;
            if name.is_empty() || name.len() > 128 {
                return Err(invalid("Invalid Codex function name"));
            }
            let mut normalized = json!({"type":"function","name":name,"parameters":function.get("parameters").cloned().unwrap_or(json!({"type":"object","properties":{}}))});
            for key in ["description", "strict"] {
                if let Some(value) = function.get(key) {
                    normalized[key] = value.clone();
                }
            }
            strip_patterns(&mut normalized["parameters"]);
            *tool = normalized;
        }
        Some(
            "custom"
            | "namespace"
            | "image_generation"
            | "web_search"
            | "web_search_preview"
            | "file_search"
            | "computer"
            | "computer_use_preview"
            | "code_interpreter"
            | "mcp"
            | "local_shell"
            | "tool_search",
        ) => {}
        _ => return Err(unsupported("Unsupported Codex tool type")),
    }
    Ok(())
}
fn strip_patterns(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(strip_patterns),
        Value::Object(values) => {
            let unsupported =
                values
                    .get("pattern")
                    .and_then(Value::as_str)
                    .is_some_and(|pattern| {
                        let mut backslashes = 0;
                        let bytes = pattern.as_bytes();
                        for (index, &byte) in bytes.iter().enumerate() {
                            if byte == b'\\' {
                                backslashes += 1;
                                continue;
                            }
                            if backslashes % 2 == 1
                                && matches!(byte, b'p' | b'P')
                                && bytes.get(index + 1) == Some(&b'{')
                            {
                                return true;
                            }
                            backslashes = 0;
                        }
                        false
                    });
            if unsupported {
                values.remove("pattern");
            }
            for value in values.values_mut() {
                strip_patterns(value);
            }
        }
        _ => {}
    }
}

/// Convert only Chat content that has an unambiguous Responses representation.
pub fn from_chat(body: &Value) -> Result<Value> {
    fields(
        body,
        &[
            "model",
            "messages",
            "stream",
            "max_tokens",
            "max_completion_tokens",
            "max_output_tokens",
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "logprobs",
            "top_logprobs",
            "n",
            "seed",
            "user",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "reasoning_effort",
            "reasoning",
            "prompt_cache_key",
            "metadata",
            "response_format",
            "service_tier",
            "instructions",
            "store",
            "stream_options",
        ],
    )?;
    if body.get("n").is_some_and(|value| value != 1) {
        return Err(unsupported("Codex supports one response candidate"));
    }
    let mut input = Vec::new();
    for message in array(&body["messages"], "Chat messages must be an array")? {
        fields(
            message,
            &[
                "role",
                "content",
                "tool_calls",
                "tool_call_id",
                "reasoning_content",
                "reasoning_details",
                "extra_content",
                "refusal",
                "annotations",
            ],
        )?;
        if message
            .get("reasoning_details")
            .is_some_and(|v| !v.is_null() && v.as_array().is_none_or(|v| !v.is_empty()))
            || message.get("refusal").is_some_and(|v| !v.is_null())
            || message
                .get("annotations")
                .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
        {
            return Err(unsupported(
                "Codex cannot replay foreign reasoning, refusals or annotations",
            ));
        }
        let role = text(&message["role"], "Chat role must be text")?;
        if !["system", "developer", "user", "assistant", "tool"].contains(&role) {
            return Err(unsupported("Unsupported Chat role for Codex"));
        }
        let mut reasoning_summary = String::new();
        if let Some(extra) = message.get("extra_content").filter(|v| !v.is_null()) {
            fields(extra, &["openai"])?;
            fields(&extra["openai"], &["responses_reasoning"])?;
            for item in array(
                &extra["openai"]["responses_reasoning"],
                "Invalid native Responses reasoning replay",
            )? {
                if role != "assistant" || item["type"] != "reasoning" {
                    return Err(unsupported("Foreign reasoning cannot be replayed to Codex"));
                }
                if let Some(summary) = item["summary"].as_array() {
                    for part in summary {
                        reasoning_summary
                            .push_str(text(&part["text"], "Invalid reasoning summary")?);
                    }
                }
                input.push(item.clone());
            }
        }
        if message
            .get("reasoning_content")
            .is_some_and(|v| v.as_str().is_none_or(|s| s != reasoning_summary))
        {
            return Err(unsupported(
                "Codex reasoning replay must include its original native items",
            ));
        }
        if role == "tool" {
            let call = text(
                &message["tool_call_id"],
                "Tool result requires tool_call_id",
            )?;
            if call.is_empty() {
                return Err(invalid("Empty tool call id"));
            }
            input.push(json!({"type":"function_call_output","call_id":call,"output":text(&message["content"],"Tool result content must be text")?}));
            continue;
        }
        let mut content = Vec::new();
        match &message["content"] {
            Value::Null => {}
            Value::String(value) => content.push(
                json!({"type":if role=="assistant"{"output_text"}else{"input_text"},"text":value}),
            ),
            Value::Array(parts) => {
                for part in parts {
                    match part["type"].as_str() {
                        Some("text") => {
                            fields(part, &["type", "text"])?;
                            content.push(json!({"type":if role=="assistant"{"output_text"}else{"input_text"},"text":text(&part["text"],"Invalid Chat text block")?}));
                        }
                        Some("image_url") if role == "user" => {
                            fields(part, &["type", "image_url"])?;
                            content.push(part.clone());
                        }
                        _ => return Err(unsupported("Unsupported Chat content block for Codex")),
                    }
                }
            }
            _ => return Err(invalid("Invalid Chat message content")),
        }
        if !content.is_empty() {
            input.push(json!({"type":"message","role":if role=="system"{"developer"}else{role},"content":content}));
        }
        if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
            if role != "assistant" {
                return Err(invalid("Only assistant messages may call tools"));
            }
            for call in array(calls, "Invalid Chat tool calls")? {
                fields(call, &["id", "type", "function"])?;
                fields(&call["function"], &["name", "arguments"])?;
                if call["type"] != "function" {
                    return Err(unsupported("Unsupported Chat tool call type"));
                }
                input.push(json!({"type":"function_call","call_id":text(&call["id"],"Missing tool call id")?,"name":text(&call["function"]["name"],"Missing function name")?,"arguments":text(&call["function"]["arguments"],"Function arguments must be text")?}));
            }
        }
    }
    let mut out = body.clone();
    out.as_object_mut().unwrap().remove("messages");
    out["input"] = json!(input);
    if let Some(choice) = out.get("tool_choice").filter(|v| v.is_object()).cloned() {
        if choice["type"] != "function" {
            return Err(unsupported("Unsupported Chat tool choice"));
        }
        out["tool_choice"] = json!({"type":"function","name":text(&choice["function"]["name"],"Invalid Chat tool choice")?});
    }
    if let Some(format) = out.as_object_mut().unwrap().remove("response_format") {
        out["text"] = match format["type"].as_str() {
            Some("text" | "json_object") => json!({"format":format}),
            Some("json_schema") => {
                let mut schema = format["json_schema"]
                    .as_object()
                    .cloned()
                    .ok_or_else(|| invalid("Invalid Chat JSON schema"))?;
                schema.insert("type".into(), json!("json_schema"));
                json!({"format":schema})
            }
            _ => return Err(unsupported("Unsupported Chat response format")),
        };
    }
    Ok(out)
}

/// Preserve signed native reasoning separately so Chat clients can replay it.
pub fn to_chat(response: &Value) -> Result<Value> {
    if response["status"] != "completed" {
        return Err(unsupported("Codex response is not complete"));
    }
    let mut content = String::new();
    let mut refusal = String::new();
    let mut calls = Vec::new();
    let mut reasoning = Vec::new();
    let mut summary = String::new();
    let mut annotations = Vec::new();
    for item in array(&response["output"], "Codex output must be an array")? {
        match item["type"].as_str() {
            Some("message") => {
                if item["role"]!="assistant" { return Err(unsupported("Unsupported Codex output role")); }
                for part in array(&item["content"],"Invalid Codex message content")? {
                    match part["type"].as_str() {
                        Some("output_text") => {
                            content.push_str(text(&part["text"],"Invalid Codex output text")?);
                            if let Some(values)=part.get("annotations") { annotations.extend(array(values,"Invalid Codex annotations")?.iter().cloned()); }
                        }
                        Some("refusal") => refusal.push_str(text(&part["refusal"],"Invalid Codex refusal")?),
                        _ => return Err(unsupported("Codex output cannot be expressed as Chat content")),
                    }
                }
            }
            Some("function_call") => calls.push(json!({"id":text(&item["call_id"],"Missing Codex call id")?,"type":"function","function":{"name":text(&item["name"],"Missing Codex function name")?,"arguments":text(&item["arguments"],"Invalid Codex function arguments")?}})),
            Some("reasoning") => {
                reasoning.push(item.clone());
                if let Some(parts)=item["summary"].as_array() { for part in parts { summary.push_str(text(&part["text"],"Invalid Codex reasoning summary")?); } }
            }
            _ => return Err(unsupported("Hosted or custom Codex output requires the native Responses endpoint")),
        }
    }
    let mut message = json!({"role":"assistant","content":if content.is_empty() && !calls.is_empty(){Value::Null}else{json!(content)}});
    if !calls.is_empty() {
        message["tool_calls"] = json!(calls);
    }
    if !refusal.is_empty() {
        message["refusal"] = json!(refusal);
    }
    if !annotations.is_empty() {
        message["annotations"] = json!(annotations);
    }
    if !summary.is_empty() {
        message["reasoning_content"] = json!(summary);
    }
    if !reasoning.is_empty() {
        message["extra_content"] = json!({"openai":{"responses_reasoning":reasoning}});
    }
    let mut out = json!({"id":response["id"],"object":"chat.completion","created":response.get("created_at").cloned().unwrap_or(json!(0)),"model":response["model"],"choices":[{"index":0,"message":message,"finish_reason":if calls.is_empty(){"stop"}else{"tool_calls"}}]});
    if let Some(usage) = response.get("usage").filter(|v| !v.is_null()) {
        let input = usage["input_tokens"]
            .as_u64()
            .ok_or_else(|| invalid("Invalid Codex input usage"))?;
        let output = usage["output_tokens"]
            .as_u64()
            .ok_or_else(|| invalid("Invalid Codex output usage"))?;
        out["usage"] = json!({"prompt_tokens":input,"completion_tokens":output,"total_tokens":input.checked_add(output).ok_or_else(||invalid("Invalid Codex token usage"))?});
        for (from, to) in [
            ("input_tokens_details", "prompt_tokens_details"),
            ("output_tokens_details", "completion_tokens_details"),
        ] {
            if let Some(value) = usage.get(from) {
                out["usage"][to] = value.clone();
            }
        }
    }
    Ok(out)
}

/// No detached reader task: dropping this future also drops/cancels the HTTP body.
pub async fn aggregate(response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success()
        || !response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Codex did not return a successful SSE response",
        )
        .into());
    }
    let mut stream = response.bytes_stream();
    let mut parser = Sse::default();
    while let Some(bytes) = stream.next().await {
        let bytes = bytes?;
        if let Some(value) = parser.push(&bytes, false)? {
            return Ok(value);
        }
    }
    if let Some(value) = parser.push(&[], true)? {
        return Ok(value);
    }
    Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "Codex stream ended without response.completed",
    )
    .into())
}
#[derive(Default)]
struct Sse {
    buffer: Vec<u8>,
    data: Vec<String>,
    event: String,
    total: usize,
}
impl Sse {
    fn push(&mut self, bytes: &[u8], eof: bool) -> Result<Option<Value>> {
        self.total = self
            .total
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("Codex response exceeds the size limit"))?;
        if self.total > MAX_RESPONSE {
            return Err(invalid("Codex response exceeds the size limit"));
        }
        self.buffer.extend_from_slice(bytes);
        loop {
            let end = self.buffer.iter().position(|b| matches!(b, b'\r' | b'\n'));
            let Some(end) = end else {
                break;
            };
            if self.buffer[end] == b'\r' && end + 1 == self.buffer.len() && !eof {
                break;
            }
            let count = end
                + 1
                + usize::from(
                    self.buffer[end] == b'\r' && self.buffer.get(end + 1) == Some(&b'\n'),
                );
            let line = std::str::from_utf8(&self.buffer[..end])
                .map_err(|_| invalid("Invalid UTF-8 in Codex SSE"))?
                .to_owned();
            self.buffer.drain(..count);
            if let Some(value) = self.line(&line)? {
                return Ok(Some(value));
            }
        }
        if eof {
            if !self.buffer.is_empty() {
                let remaining = String::from_utf8(std::mem::take(&mut self.buffer))
                    .map_err(|_| invalid("Invalid UTF-8 in Codex SSE"))?;
                if let Some(value) = self.line(&remaining)? {
                    return Ok(Some(value));
                }
            }
            return self.line("");
        }
        Ok(None)
    }
    fn line(&mut self, line: &str) -> Result<Option<Value>> {
        if line.is_empty() {
            let event = std::mem::take(&mut self.event);
            if self.data.is_empty() {
                return Ok(None);
            }
            let data = std::mem::take(&mut self.data).join("\n");
            if data == "[DONE]" {
                return Ok(None);
            }
            let value: Value =
                serde_json::from_str(&data).map_err(|_| invalid("Invalid JSON in Codex SSE"))?;
            let kind = value["type"].as_str().unwrap_or(&event);
            if matches!(kind, "error" | "response.failed" | "response.incomplete") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Codex returned a failed or incomplete response",
                )
                .into());
            }
            if kind == "response.completed" {
                if !value["response"].is_object() || value["response"]["status"] != "completed" {
                    return Err(invalid("Invalid completed Codex response"));
                }
                return Ok(Some(value["response"].clone()));
            }
        } else if !line.starts_with(':') {
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            if field == "event" {
                self.event = value.into();
            }
            if field == "data" {
                self.data.push(value.into());
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, response::Response, routing::get};
    use futures_util::stream;
    use std::{
        convert::Infallible,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    fn account() -> Value {
        json!({"id":"fixture-account","provider":"codex","accessToken":"disposable-access-token","providerSpecificData":{"chatgptAccountId":"fixture-workspace"}})
    }
    fn completed() -> Value {
        json!({"id":"resp_fixture","object":"response","created_at":123,"status":"completed","model":"gpt-5.6-sol","output":[
        {"id":"rs_fixture","type":"reasoning","encrypted_content":"opaque-signed-native-reasoning","summary":[{"type":"summary_text","text":"Checked the arithmetic."}]},
        {"id":"msg_fixture","type":"message","role":"assistant","content":[{"type":"output_text","text":"Xin chào 🌏","annotations":[]}]},
        {"type":"function_call","call_id":"call_fixture","name":"lookup","arguments":"{\"value\":42}"}
    ],"usage":{"input_tokens":12,"output_tokens":5,"total_tokens":17,"input_tokens_details":{"cached_tokens":8},"output_tokens_details":{"reasoning_tokens":2}}})
    }
    #[test]
    fn prepare_matches_original_codex_defaults_and_lite_prefix() {
        let prepared=prepare(&account(),&json!({"model":"gpt-5.6-sol-review(high)","input":[{"type":"message","id":"msg_server","role":"system","content":"Keep this instruction"}],"stream":false,"store":true,"max_output_tokens":100,"temperature":0.5,"service_tier":"fast","tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object","properties":{"value":{"type":"string","pattern":"\\p{L}+"},"pattern":{"type":"string","pattern":"[a-z]+"}}}}}]})).unwrap();
        assert_eq!(
            prepared.url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            prepared.headers["authorization"],
            "Bearer disposable-access-token"
        );
        assert_eq!(prepared.headers["chatgpt-account-id"], "fixture-workspace");
        assert_eq!(prepared.body["model"], "gpt-5.6-sol");
        assert_eq!(prepared.body["input"][0]["role"], "developer");
        assert!(prepared.body["input"][0].get("id").is_none());
        assert_eq!(prepared.body["stream"], true);
        assert_eq!(prepared.body["store"], false);
        assert_eq!(prepared.body["service_tier"], "priority");
        assert_eq!(prepared.body["reasoning"]["effort"], "high");
        assert!(prepared.body.get("temperature").is_none());
        assert!(prepared.body.get("max_output_tokens").is_none());
        assert!(
            prepared.body["tools"][0]["parameters"]["properties"]["value"]
                .get("pattern")
                .is_none()
        );
        assert_eq!(
            prepared.body["tools"][0]["parameters"]["properties"]["pattern"]["pattern"],
            "[a-z]+"
        );
        assert_eq!(prepared.body["instructions"], contract()["instructions"]);
        let lite = prepare(
            &account(),
            &json!({"model":"gpt-6.1-sol(max)","input":"Hello"}),
        )
        .unwrap();
        assert_eq!(
            lite.headers["x-openai-internal-codex-responses-lite"],
            "true"
        );
        assert_eq!(lite.body["input"][0]["type"], "additional_tools");
        assert_eq!(lite.body["reasoning"]["context"], "all_turns");
        assert_eq!(lite.body["reasoning"]["effort"], "max");
        assert_eq!(lite.body["tools"], Value::Null);
        assert_eq!(lite.body["instructions"], "");
    }
    #[test]
    fn endpoint_and_header_boundaries_reject_unsafe_configuration() {
        for endpoint in [
            "file:///tmp/secret",
            "http://example.com/responses",
            "https://user:password@example.com/",
            "https://example.com/?token=secret",
            "https://example.com/#fragment",
        ] {
            let mut account = account();
            account["providerSpecificData"]["baseUrl"] = json!(endpoint);
            assert!(prepare(&account, &json!({"model":"gpt-5.6-sol","input":"Hello"})).is_err());
        }
        let mut account = account();
        account["accessToken"] = json!("secret\r\nx-injected: true");
        assert!(prepare(&account, &json!({"model":"gpt-5.6-sol","input":"Hello"})).is_err());
        let prepared = prepare(
            &self::account(),
            &json!({"model":"gpt-5.6-sol","input":"Hello","baseUrl":"https://attacker.example/"}),
        )
        .unwrap();
        assert_eq!(
            prepared.url,
            "https://chatgpt.com/backend-api/codex/responses"
        );
    }
    #[test]
    fn chat_roundtrip_preserves_text_functions_native_reasoning_and_usage() {
        let response = completed();
        let chat = to_chat(&response).unwrap();
        assert_eq!(chat["choices"][0]["message"]["content"], "Xin chào 🌏");
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chat["usage"]["prompt_tokens_details"]["cached_tokens"], 8);
        assert_eq!(
            chat["usage"]["completion_tokens_details"]["reasoning_tokens"],
            2
        );
        let request=from_chat(&json!({"model":"gpt-5.6-sol","messages":[chat["choices"][0]["message"].clone(),{"role":"tool","tool_call_id":"call_fixture","content":"42"}]})).unwrap();
        assert_eq!(request["input"][0], response["output"][0]);
        assert_eq!(request["input"][1]["content"][0]["text"], "Xin chào 🌏");
        assert_eq!(request["input"][2]["arguments"], "{\"value\":42}");
        assert_eq!(request["input"][3]["type"], "function_call_output");
        assert!(from_chat(&json!({"messages":[{"role":"assistant","content":"x","reasoning_content":"unsigned private thinking"}]})).is_err());
        let mut image = response;
        image["output"] = json!([{"type":"image_generation_call","result":"opaque"}]);
        assert!(to_chat(&image).is_err());
    }
    #[test]
    fn parser_handles_arbitrary_byte_splits_comments_crlf_and_multiline_json() {
        let response = completed();
        let body = format!(
            ": ping\r\nevent: response.completed\r\ndata: {{\"type\":\"response.completed\",\r\ndata: \"response\":{response}}}\r\n\r\n"
        );
        for size in [1, 2, 3, 7, 31, 1024] {
            let mut parser = Sse::default();
            let mut result = None;
            for chunk in body.as_bytes().chunks(size) {
                result = result.or(parser.push(chunk, false).unwrap());
            }
            assert_eq!(result, Some(response.clone()));
        }
        let mut parser = Sse::default();
        assert!(
            parser
                .push(
                    b"event: response.incomplete\ndata: {\"type\":\"response.incomplete\"}\n\n",
                    false
                )
                .is_err()
        );
        let mut parser = Sse::default();
        assert!(
            parser
                .push(b"data: {\"type\":\"response.failed\"}\n\n", false)
                .is_err()
        );
    }
    #[tokio::test]
    async fn aggregate_uses_loopback_http_and_preserves_final_response() {
        let value = completed();
        let expected = value.clone();
        let app=Router::new().route("/",get(move || { let value=value.clone(); async move {
            let text=format!(": keepalive\ndata: {{\"type\":\"response.completed\",\"response\":{value}}}\n\n");
            let bytes=text.into_bytes().into_iter().map(|byte|Ok::<_,Infallible>(vec![byte])).collect::<Vec<_>>();
            Response::builder().header("content-type","text/event-stream").body(Body::from_stream(stream::iter(bytes))).unwrap()
        }}));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let response = reqwest::Client::new().get(url).send().await.unwrap();
        assert_eq!(aggregate(response).await.unwrap(), expected);
        server.abort();
    }
    #[tokio::test]
    async fn aggregate_reports_missing_completion_and_cancels_owned_reader() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let watch = dropped.clone();
        let app = Router::new()
            .route(
                "/short",
                get(|| async {
                    Response::builder()
                        .header("content-type", "text/event-stream")
                        .body(Body::from("data: [DONE]\n\n"))
                        .unwrap()
                }),
            )
            .route(
                "/pending",
                get(move || {
                    let guard = Dropped(watch.clone());
                    async move {
                        let stream = stream::unfold(guard, |guard| async move {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            Some((Ok::<_, Infallible>(b": keepalive\n\n".as_slice()), guard))
                        });
                        Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(Body::from_stream(stream))
                            .unwrap()
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        assert!(
            aggregate(client.get(format!("{url}/short")).send().await.unwrap())
                .await
                .is_err()
        );
        let response = client.get(format!("{url}/pending")).send().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), aggregate(response))
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            while !dropped.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        server.abort();
    }
}
