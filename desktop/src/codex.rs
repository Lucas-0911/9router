//! Codex Responses transport and loss-checked Chat conversion.
//! Defaults are snapshotted from the original executor; no JavaScript runtime runs.
use crate::Result;
use futures_util::StreamExt;
use reqwest::{
    Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, io, sync::OnceLock};

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
    if let Some(options) = body.get("stream_options").filter(|value| !value.is_null()) {
        fields(options, &["include_usage"])?;
        if options
            .get("include_usage")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(invalid("Chat include_usage must be a boolean"));
        }
    }
    if let Some(tools) = body.get("tools").filter(|value| !value.is_null()) {
        for tool in array(tools, "Chat tools must be an array")? {
            if tool["type"] != "function" {
                return Err(unsupported(
                    "Hosted tools require the native Responses endpoint",
                ));
            }
        }
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

struct StreamTool {
    item_id: String,
    id: String,
    name: String,
    arguments: String,
}
struct ChatStream {
    id: String,
    model: String,
    created: u64,
    started: bool,
    content: String,
    summary: String,
    refusal: String,
    tools: Vec<StreamTool>,
}
impl ChatStream {
    fn new(model: String) -> Self {
        Self {
            id: format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            model,
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            started: false,
            content: String::new(),
            summary: String::new(),
            refusal: String::new(),
            tools: Vec::new(),
        }
    }
    fn chunk(&self, delta: Value, finish: Value) -> Value {
        json!({"id":self.id,"object":"chat.completion.chunk","created":self.created,"model":self.model,"choices":[{"index":0,"delta":delta,"finish_reason":finish}]})
    }
    fn emit(&mut self, delta: Value, output: &mut Vec<Value>) {
        if !self.started {
            self.started = true;
            output.push(self.chunk(json!({"role":"assistant"}), Value::Null));
        }
        output.push(self.chunk(delta, Value::Null));
    }
    fn start_tool(&mut self, item: &Value, output: &mut Vec<Value>) -> Result<()> {
        let id = text(&item["call_id"], "Missing Codex tool call id")?;
        let name = text(&item["name"], "Missing Codex tool name")?;
        let item_id = item["id"].as_str().unwrap_or(id);
        if id.is_empty() || name.is_empty() || item_id.is_empty() {
            return Err(invalid("Empty Codex tool identity"));
        }
        if let Some(existing) = self
            .tools
            .iter()
            .find(|tool| tool.item_id == item_id || tool.id == id)
        {
            if existing.id != id || existing.name != name || existing.item_id != item_id {
                return Err(invalid("Codex tool identity changed during streaming"));
            }
            return Ok(());
        }
        let index = self.tools.len();
        self.tools.push(StreamTool {
            item_id: item_id.into(),
            id: id.into(),
            name: name.into(),
            arguments: String::new(),
        });
        self.emit(json!({"tool_calls":[{"index":index,"id":id,"type":"function","function":{"name":name,"arguments":""}}]}), output);
        Ok(())
    }
    fn event(&mut self, event: &Value, include_usage: bool) -> Result<Vec<Value>> {
        let mut output = Vec::new();
        match event["type"].as_str().unwrap_or("") {
            "response.created" => {
                if self.started || !event["response"].is_object() {
                    return Err(invalid("Invalid Codex creation event"));
                }
                if let Some(created) = event["response"]["created_at"].as_u64() {
                    self.created = created;
                }
                if let Some(model) = event["response"]["model"].as_str() {
                    self.model = model.into();
                }
            }
            "response.output_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.refusal.delta" => {
                let delta = text(&event["delta"], "Invalid Codex text delta")?;
                let key = match event["type"].as_str().unwrap() {
                    "response.output_text.delta" => {
                        self.content.push_str(delta);
                        "content"
                    }
                    "response.refusal.delta" => {
                        self.refusal.push_str(delta);
                        "refusal"
                    }
                    _ => {
                        self.summary.push_str(delta);
                        "reasoning_content"
                    }
                };
                if !delta.is_empty() {
                    self.emit(json!({key:delta}), &mut output);
                }
            }
            "response.output_item.added" => match event["item"]["type"].as_str() {
                Some("function_call") => self.start_tool(&event["item"], &mut output)?,
                Some("message" | "reasoning") => {}
                _ => return Err(unsupported("Codex Chat cannot stream this output item")),
            },
            "response.function_call_arguments.delta" => {
                let item_id = text(&event["item_id"], "Missing Codex argument item id")?;
                let index = self
                    .tools
                    .iter()
                    .position(|tool| tool.item_id == item_id)
                    .ok_or_else(|| invalid("Unknown Codex argument item id"))?;
                let delta = text(&event["delta"], "Invalid Codex argument delta")?;
                self.tools[index].arguments.push_str(delta);
                if !delta.is_empty() {
                    self.emit(
                        json!({"tool_calls":[{"index":index,"function":{"arguments":delta}}]}),
                        &mut output,
                    );
                }
            }
            "response.completed" => {
                let response = &event["response"];
                let chat = to_chat(response)?;
                if !self.started {
                    if let Some(created) = response["created_at"].as_u64() {
                        self.created = created;
                    }
                    if let Some(model) = response["model"].as_str() {
                        self.model = model.into();
                    }
                }
                let message = &chat["choices"][0]["message"];
                for key in ["content", "reasoning_content", "refusal"] {
                    let emitted = match key {
                        "content" => self.content.as_str(),
                        "reasoning_content" => self.summary.as_str(),
                        _ => self.refusal.as_str(),
                    };
                    let full = message[key].as_str().unwrap_or_default();
                    let suffix = full
                        .strip_prefix(emitted)
                        .ok_or_else(|| invalid("Codex final text differs from emitted deltas"))?;
                    if !suffix.is_empty() {
                        self.emit(json!({key:suffix}), &mut output);
                    }
                }
                let final_calls = response["output"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .collect::<Vec<_>>();
                if self
                    .tools
                    .iter()
                    .any(|tool| !final_calls.iter().any(|call| call["call_id"] == tool.id))
                {
                    return Err(invalid("Codex completion lost an emitted tool call"));
                }
                let mut ids = std::collections::HashSet::new();
                for item in final_calls {
                    if !ids.insert(text(&item["call_id"], "Missing Codex tool id")?) {
                        return Err(invalid("Duplicate Codex tool call id"));
                    }
                    self.start_tool(item, &mut output)?;
                    let index = self
                        .tools
                        .iter()
                        .position(|tool| item["call_id"] == tool.id)
                        .unwrap();
                    let arguments = text(&item["arguments"], "Invalid Codex tool arguments")?;
                    let suffix = arguments
                        .strip_prefix(&self.tools[index].arguments)
                        .ok_or_else(|| {
                            invalid("Codex final arguments differ from emitted deltas")
                        })?;
                    if !suffix.is_empty() {
                        self.emit(
                            json!({"tool_calls":[{"index":index,"function":{"arguments":suffix}}]}),
                            &mut output,
                        );
                    }
                }
                let mut metadata = json!({});
                for key in ["extra_content", "annotations"] {
                    if let Some(value) = message.get(key) {
                        metadata[key] = value.clone();
                    }
                }
                if !metadata.as_object().unwrap().is_empty() || !self.started {
                    self.emit(metadata, &mut output);
                }
                output.push(self.chunk(json!({}), chat["choices"][0]["finish_reason"].clone()));
                if include_usage && let Some(usage) = chat.get("usage") {
                    let mut last = self.chunk(json!({}), Value::Null);
                    last["choices"] = json!([]);
                    last["usage"] = usage.clone();
                    output.push(last);
                }
            }
            "response.queued"
            | "response.in_progress"
            | "response.output_item.done"
            | "response.content_part.added"
            | "response.content_part.done"
            | "response.output_text.done"
            | "response.refusal.done"
            | "response.function_call_arguments.done"
            | "response.reasoning_summary_part.added"
            | "response.reasoning_summary_part.done"
            | "response.reasoning_summary_text.done"
            | "response.output_text.annotation.added" => {}
            _ => return Err(unsupported("Unsupported Codex Chat stream event")),
        }
        Ok(output)
    }
}

/// Pull-based conversion: disconnect drops the owned upstream response immediately.
pub fn chat_stream(
    response: reqwest::Response,
    model: String,
    include_usage: bool,
) -> impl futures_util::Stream<Item = io::Result<Vec<u8>>> {
    futures_util::stream::unfold(
        (
            response.bytes_stream(),
            Sse::default(),
            ChatStream::new(model),
            VecDeque::new(),
            false,
        ),
        move |(mut source, mut parser, mut chat, mut pending, mut done)| async move {
            loop {
                if let Some(packet) = pending.pop_front() {
                    return Some((Ok(packet), (source, parser, chat, pending, done)));
                }
                if done {
                    return None;
                }
                let next = source.next().await;
                let eof = next.is_none();
                let events = match next {
                    Some(Ok(bytes)) => parser.events(&bytes, false),
                    Some(Err(_)) => Err(invalid("Codex stream interrupted")),
                    None => parser.events(&[], true),
                };
                let translated = events.and_then(|events| {
                    for event in events {
                        for chunk in chat.event(&event, include_usage)? {
                            pending.push_back(format!("data: {chunk}\n\n").into_bytes());
                        }
                        if event["type"] == "response.completed" {
                            done = true;
                            pending.push_back(b"data: [DONE]\n\n".to_vec());
                            break;
                        }
                    }
                    if eof && !done {
                        return Err(invalid("Codex stream ended before completion"));
                    }
                    Ok(())
                });
                if translated.is_err() {
                    pending.clear();
                    pending.push_back(b"data: {\"error\":{\"message\":\"Codex stream is invalid or incomplete\",\"type\":\"upstream_error\",\"code\":\"invalid_stream\"}}\n\n".to_vec());
                    done = true;
                }
            }
        },
    )
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
        Ok(self.events(bytes, eof)?.into_iter().find_map(|event| {
            (event["type"] == "response.completed").then(|| event["response"].clone())
        }))
    }
    fn events(&mut self, bytes: &[u8], eof: bool) -> Result<Vec<Value>> {
        let mut events = Vec::new();
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
                events.push(value);
            }
        }
        if eof {
            if !self.buffer.is_empty() {
                let remaining = String::from_utf8(std::mem::take(&mut self.buffer))
                    .map_err(|_| invalid("Invalid UTF-8 in Codex SSE"))?;
                if let Some(value) = self.line(&remaining)? {
                    events.push(value);
                }
            }
            if let Some(value) = self.line("")? {
                events.push(value);
            }
        }
        Ok(events)
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
            let mut value: Value =
                serde_json::from_str(&data).map_err(|_| invalid("Invalid JSON in Codex SSE"))?;
            if !value.is_object() {
                return Err(invalid("Codex SSE event must be an object"));
            }
            if value.get("type").is_none() {
                value["type"] = json!(event);
            }
            let kind = text(&value["type"], "Codex SSE event requires a type")?;
            if matches!(kind, "error" | "response.failed" | "response.incomplete") {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Codex returned a failed or incomplete response",
                )
                .into());
            }
            if kind == "response.completed"
                && (!value["response"].is_object() || value["response"]["status"] != "completed")
            {
                return Err(invalid("Invalid completed Codex response"));
            }
            return Ok(Some(value));
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
    fn chat_stream_options_and_unrepresentable_hosted_tools_are_rejected() {
        for options in [
            json!(true),
            json!({"include_usage":"yes"}),
            json!({"unknown":true}),
        ] {
            assert!(
                from_chat(&json!({"messages":[],"stream":true,"stream_options":options})).is_err()
            );
        }
        assert!(from_chat(&json!({"messages":[],"tools":[{"type":"image_generation"}]})).is_err());
        assert!(
            from_chat(
                &json!({"messages":[],"stream":true,"stream_options":{"include_usage":false}})
            )
            .is_ok()
        );
    }
    #[test]
    fn chat_stream_final_only_uses_upstream_metadata_and_preserves_refusals() {
        let mut response = completed();
        response["created_at"] = json!(456);
        response["output"][1]["content"]
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"refusal","refusal":"Cannot do that"}));
        let mut stream = ChatStream::new("request-alias".into());
        let chunks = stream
            .event(
                &json!({"type":"response.completed","response":response}),
                false,
            )
            .unwrap();
        assert!(chunks.iter().all(|chunk| chunk["model"] == "gpt-5.6-sol"
            && chunk["created"] == 456
            && chunk.get("usage").is_none()));
        assert_eq!(
            chunks
                .iter()
                .filter_map(|chunk| chunk["choices"][0]["delta"]["refusal"].as_str())
                .collect::<String>(),
            "Cannot do that"
        );
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
