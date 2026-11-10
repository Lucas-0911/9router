//! Strict, non-streaming adapters for the common Chat, Messages and generateContent subset.
//! OpenAI-compatible `reasoning_details` retains native signatures for replay. Foreign
//! signatures, provider-only features and cross-format streams fail instead of being erased.
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    io,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    OpenAi,
    Claude,
    Gemini,
    Responses,
}

fn unsupported<T>(message: impl AsRef<str>) -> crate::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!("unsupported protocol conversion: {}", message.as_ref()),
    )
    .into())
}
fn object<'a>(
    value: &'a Value,
    path: &str,
    keys: &[&str],
) -> crate::Result<&'a Map<String, Value>> {
    let Some(map) = value.as_object() else {
        return unsupported(format!("{path} must be an object"));
    };
    for key in map.keys() {
        if !keys.contains(&key.as_str()) {
            return unsupported(format!("{path}.{key}"));
        }
    }
    Ok(map)
}
fn array<'a>(value: &'a Value, path: &str) -> crate::Result<&'a Vec<Value>> {
    value.as_array().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported protocol conversion: {path} must be an array"),
        )
        .into()
    })
}
fn string<'a>(value: &'a Value, path: &str) -> crate::Result<&'a str> {
    value.as_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported protocol conversion: {path} must be a string"),
        )
        .into()
    })
}
fn name<'a>(value: &'a Value, path: &str) -> crate::Result<&'a str> {
    let s = string(value, path)?;
    if s.is_empty() {
        return unsupported(format!("{path} must not be empty"));
    }
    Ok(s)
}
fn copy(from: &Value, to: &mut Value, fields: &[(&str, &str)]) {
    for (source, target) in fields {
        if let Some(value) = from.get(source) {
            to[*target] = value.clone();
        }
    }
}
fn number(value: &Value, path: &str) -> crate::Result<u64> {
    value.as_u64().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported protocol conversion: {path} must be a non-negative integer"),
        )
        .into()
    })
}
fn count(value: &Value, key: &str) -> crate::Result<u64> {
    value
        .get(key)
        .map(|v| number(v, key))
        .transpose()
        .map(|n| n.unwrap_or(0))
}
fn sum(a: u64, b: u64) -> crate::Result<u64> {
    a.checked_add(b).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported protocol conversion: token count overflow",
        )
        .into()
    })
}
fn args(value: &Value) -> crate::Result<Value> {
    let parsed: Value =
        serde_json::from_str(string(value, "function.arguments")?).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported protocol conversion: function.arguments is not valid JSON",
            )
        })?;
    if !parsed.is_object() {
        return unsupported("function.arguments must encode an object");
    }
    Ok(parsed)
}
fn data_uri(url: &str) -> crate::Result<(&str, &str)> {
    let Some((mime, data)) = url
        .strip_prefix("data:")
        .and_then(|s| s.split_once(";base64,"))
    else {
        return unsupported("image requires an image data URI");
    };
    if !mime.starts_with("image/")
        || mime.len() == 6
        || data.is_empty()
        || data.len() % 4 != 0
        || data.len() - data.trim_end_matches('=').len() > 2
        || data.trim_end_matches('=').contains('=')
        || data
            .bytes()
            .any(|b| !b.is_ascii_alphanumeric() && !b"+/=".contains(&b))
    {
        return unsupported("invalid base64 image data URI");
    }
    Ok((mime, data))
}
fn image_url(source: &Value) -> crate::Result<Value> {
    object(
        source,
        "image.source",
        &["type", "media_type", "data", "url"],
    )?;
    let url = match source["type"].as_str() {
        Some("base64") => {
            let url = format!(
                "data:{};base64,{}",
                string(&source["media_type"], "image.media_type")?,
                string(&source["data"], "image.data")?
            );
            data_uri(&url)?;
            url
        }
        Some("url") => {
            let url = string(&source["url"], "image.url")?;
            if !url.starts_with("https://") && !url.starts_with("http://") {
                return unsupported("image URL scheme");
            }
            url.to_owned()
        }
        _ => return unsupported("image source type"),
    };
    Ok(json!({"type":"image_url","image_url":{"url":url}}))
}
fn openai_image(part: &Value, to: Format) -> crate::Result<Value> {
    object(part, "content.image_url", &["type", "image_url"])?;
    object(&part["image_url"], "image_url", &["url", "detail"])?;
    if part["image_url"].get("detail").is_some_and(|v| v != "auto") {
        return unsupported("image_url.detail");
    }
    let url = string(&part["image_url"]["url"], "image_url.url")?;
    if url.starts_with("data:") {
        let (mime, data) = data_uri(url)?;
        return Ok(if to == Format::Claude {
            json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}})
        } else {
            json!({"inlineData":{"mimeType":mime,"data":data}})
        });
    }
    if to == Format::Claude && (url.starts_with("https://") || url.starts_with("http://")) {
        return Ok(json!({"type":"image","source":{"type":"url","url":url}}));
    }
    unsupported(
        "remote image URLs require native Claude/OpenAI; Gemini conversion accepts inline images",
    )
}
fn text_parts(content: &Value) -> crate::Result<Vec<Value>> {
    if let Some(text) = content.as_str() {
        return Ok(vec![json!({"type":"text","text":text})]);
    }
    if content.is_null() {
        return Ok(Vec::new());
    }
    Ok(array(content, "content")?.clone())
}
fn compact(parts: Vec<Value>) -> Value {
    if parts.len() == 1 && parts[0]["type"] == "text" {
        parts[0]["text"].clone()
    } else if parts.is_empty() {
        Value::Null
    } else {
        Value::Array(parts)
    }
}
fn tool_content(content: &Value) -> crate::Result<Value> {
    if content.is_string() {
        return Ok(content.clone());
    }
    for part in array(content, "tool result content")? {
        object(part, "tool result part", &["type", "text"])?;
        if part["type"] != "text" {
            return unsupported("non-text tool result content");
        }
        string(&part["text"], "tool result text")?;
    }
    Ok(content.clone())
}
fn choice(value: &Value, from: Format, to: Format) -> crate::Result<Value> {
    let normalized = if from == Format::OpenAi {
        if let Some(s) = value.as_str() {
            s.to_owned()
        } else {
            object(value, "tool_choice", &["type", "function"])?;
            object(&value["function"], "tool_choice.function", &["name"])?;
            if value["type"] != "function" {
                return unsupported("tool_choice type");
            }
            format!(
                "tool:{}",
                name(&value["function"]["name"], "tool_choice.function.name")?
            )
        }
    } else {
        object(value, "tool_choice", &["type", "name"])?;
        match value["type"].as_str() {
            Some("tool") => format!("tool:{}", name(&value["name"], "tool_choice.name")?),
            Some("any") => "required".into(),
            Some("auto") => "auto".into(),
            Some("none") => "none".into(),
            _ => return unsupported("tool_choice type"),
        }
    };
    if let Some(tool) = normalized.strip_prefix("tool:") {
        return Ok(if to == Format::OpenAi {
            json!({"type":"function","function":{"name":tool}})
        } else if to == Format::Claude {
            json!({"type":"tool","name":tool})
        } else {
            json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":[tool]}})
        });
    }
    match normalized.as_str() {
        "auto" | "none" | "required" => Ok(if to == Format::OpenAi {
            json!(normalized)
        } else if to == Format::Claude {
            json!({"type": if normalized == "required" { "any" } else { &normalized }})
        } else {
            json!({"functionCallingConfig":{"mode": match normalized.as_str() {"auto"=>"AUTO", "none"=>"NONE", _=>"ANY"}}})
        }),
        _ => unsupported("tool_choice"),
    }
}
fn openai_tool(tool: &Value, to: Format) -> crate::Result<Value> {
    object(tool, "tools[]", &["type", "function"])?;
    if tool.get("type").is_some_and(|t| t != "function") {
        return unsupported("non-function tool");
    }
    let function = &tool["function"];
    object(
        function,
        "tool.function",
        &["name", "description", "parameters", "strict"],
    )?;
    name(&function["name"], "tool.function.name")?;
    if function.get("strict").is_some_and(|v| v != false) {
        return unsupported("strict function schema");
    }
    let schema = function
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type":"object","properties":{}}));
    if !schema.is_object() {
        return unsupported("function parameters schema must be an object");
    }
    let mut out = if to == Format::Claude {
        json!({"name":function["name"],"input_schema":schema})
    } else {
        json!({"name":function["name"],"parametersJsonSchema":schema})
    };
    if let Some(description) = function.get("description") {
        string(description, "tool description")?;
        out["description"] = description.clone();
    }
    Ok(out)
}

/// Same-format requests are lossless apart from selecting the resolved model.
pub fn request(body: &Value, from: Format, to: Format, model: &str) -> crate::Result<Value> {
    if !body.is_object() {
        return unsupported("request body must be an object");
    }
    if from == to {
        let mut out = body.clone();
        if to == Format::Gemini {
            out.as_object_mut().unwrap().remove("model");
            out.as_object_mut().unwrap().remove("stream");
        } else {
            out["model"] = json!(model);
        }
        return Ok(out);
    }
    if body["stream"] == true {
        return unsupported("cross-format streaming");
    }
    if from == Format::Responses || to == Format::Responses {
        return unsupported("Responses cross-format requests");
    }
    let canonical = match from {
        Format::OpenAi => body.clone(),
        Format::Claude => claude_request(body, model)?,
        Format::Gemini => gemini_request(body, model)?,
        _ => unreachable!(),
    };
    if to == Format::OpenAi {
        validate_openai(&canonical)?;
        return Ok(canonical);
    }
    from_openai_request(&canonical, to, model)
}

fn validate_openai(body: &Value) -> crate::Result<()> {
    object(
        body,
        "request",
        &[
            "model",
            "messages",
            "stream",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "stop",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "n",
        ],
    )?;
    if body.get("stream").is_some_and(|v| !v.is_boolean()) {
        return unsupported("stream must be boolean");
    }
    if body.get("n").is_some_and(|v| v != 1) {
        return unsupported("multiple output candidates");
    }
    if body.get("max_tokens").is_some() && body.get("max_completion_tokens").is_some() {
        return unsupported("both token limit fields supplied");
    }
    for key in ["max_tokens", "max_completion_tokens"] {
        if let Some(value) = body.get(key)
            && number(value, key)? == 0
        {
            return unsupported("token limit must be positive");
        }
    }
    for key in ["temperature", "top_p"] {
        if body.get(key).is_some_and(|v| !v.is_number()) {
            return unsupported(format!("{key} must be numeric"));
        }
    }
    if let Some(stop) = body.get("stop")
        && !stop.is_string()
    {
        for entry in array(stop, "stop")? {
            string(entry, "stop[]")?;
        }
    }
    if body
        .get("parallel_tool_calls")
        .is_some_and(|v| !v.is_boolean())
    {
        return unsupported("parallel_tool_calls must be boolean");
    }
    let mut pending = BTreeSet::new();
    let mut conversation_started = false;
    for message in array(&body["messages"], "messages")? {
        object(
            message,
            "messages[]",
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
        if message.get("refusal").is_some_and(|v| !v.is_null())
            || message
                .get("annotations")
                .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        {
            return unsupported("response refusal or annotations");
        }
        let role = string(&message["role"], "message.role")?;
        match role {
            "system" if !conversation_started => {}
            "user" | "assistant" | "tool" => conversation_started = true,
            _ => return unsupported("message role or non-prefix system message"),
        }
        if role != "tool" && !pending.is_empty() {
            return unsupported("missing tool results before next message");
        }
        if role == "tool" {
            let id = name(&message["tool_call_id"], "tool_call_id")?;
            if !pending.remove(id) {
                return unsupported("orphan or duplicate tool result");
            }
            tool_content(&message["content"])?;
        } else if message.get("tool_call_id").is_some() {
            return unsupported("tool_call_id on non-tool message");
        }
        if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
            if role != "assistant" {
                return unsupported("tool_calls on non-assistant message");
            }
            for call in array(calls, "tool_calls")? {
                object(
                    call,
                    "tool_calls[]",
                    &["id", "type", "function", "extra_content"],
                )?;
                if call["type"] != "function" {
                    return unsupported("non-function tool call");
                }
                let id = name(&call["id"], "tool call id")?;
                if !pending.insert(id.to_owned()) {
                    return unsupported("duplicate tool call id within a turn");
                }
                object(
                    &call["function"],
                    "tool_call.function",
                    &["name", "arguments"],
                )?;
                name(&call["function"]["name"], "tool function name")?;
                args(&call["function"]["arguments"])?;
            }
        }
        if role != "assistant"
            && (message.get("reasoning_content").is_some()
                || message.get("reasoning_details").is_some())
        {
            return unsupported("reasoning on non-assistant message");
        }
    }
    if !pending.is_empty()
        && body["messages"]
            .as_array()
            .unwrap()
            .last()
            .is_some_and(|m| m["role"] != "assistant")
    {
        return unsupported("incomplete parallel tool results");
    }
    Ok(())
}

fn signed_text_parts(
    message: &Value,
    visible: &[Value],
    to: Format,
) -> crate::Result<Option<Vec<Value>>> {
    let Some(extra) = message.get("extra_content") else {
        return Ok(None);
    };
    if message["role"] != "assistant" {
        return unsupported("signed visible text requires assistant role");
    }
    object(extra, "message.extra_content", &["google"])?;
    object(
        &extra["google"],
        "message.extra_content.google",
        &["text_parts"],
    )?;
    let native = array(&extra["google"]["text_parts"], "signed visible text parts")?;
    if to != Format::Gemini {
        return unsupported("Google visible text signatures require Gemini replay");
    }
    if native.len() != visible.len() {
        return unsupported("signed visible text no longer matches content");
    }
    for (native, plain) in native.iter().zip(visible) {
        object(
            native,
            "signed visible text part",
            &["text", "thoughtSignature", "thought"],
        )?;
        object(plain, "visible text part", &["type", "text"])?;
        if plain["type"] != "text"
            || native["text"] != plain["text"]
            || native.get("thought").is_some_and(|v| v != false)
        {
            return unsupported("signed visible text no longer matches content");
        }
        string(&native["text"], "signed visible text")?;
        if let Some(signature) = native.get("thoughtSignature") {
            string(signature, "visible text signature")?;
        }
    }
    Ok(Some(native.clone()))
}

fn from_openai_request(body: &Value, to: Format, model: &str) -> crate::Result<Value> {
    validate_openai(body)?;
    let mut out = if to == Format::Claude {
        // ponytail: default 4096 tokens; add catalog/model ceilings when native capability policy lands.
        json!({"model":model,"max_tokens":4096,"messages":[]})
    } else {
        json!({"contents":[]})
    };
    if to == Format::Claude {
        copy(
            body,
            &mut out,
            &[
                ("max_tokens", "max_tokens"),
                ("max_completion_tokens", "max_tokens"),
                ("temperature", "temperature"),
                ("top_p", "top_p"),
                ("stream", "stream"),
            ],
        );
        if let Some(stop) = body.get("stop") {
            out["stop_sequences"] = if stop.is_string() {
                json!([stop])
            } else {
                stop.clone()
            };
        }
    } else {
        let mut config = json!({});
        copy(
            body,
            &mut config,
            &[
                ("max_tokens", "maxOutputTokens"),
                ("max_completion_tokens", "maxOutputTokens"),
                ("temperature", "temperature"),
                ("top_p", "topP"),
            ],
        );
        if let Some(stop) = body.get("stop") {
            config["stopSequences"] = if stop.is_string() {
                json!([stop])
            } else {
                stop.clone()
            };
        }
        if !config.as_object().unwrap().is_empty() {
            out["generationConfig"] = config;
        }
    }
    if let Some(tools) = body.get("tools") {
        let converted = array(tools, "tools")?
            .iter()
            .map(|tool| openai_tool(tool, to))
            .collect::<crate::Result<Vec<_>>>()?;
        out["tools"] = if to == Format::Claude {
            json!(converted)
        } else {
            json!([{"functionDeclarations":converted}])
        };
    }
    if let Some(value) = body.get("tool_choice") {
        out[if to == Format::Claude {
            "tool_choice"
        } else {
            "toolConfig"
        }] = choice(value, Format::OpenAi, to)?;
    }
    if let Some(parallel) = body.get("parallel_tool_calls") {
        if to == Format::Gemini && parallel == false {
            return unsupported("Gemini cannot enforce parallel_tool_calls=false");
        }
        if to == Format::Claude {
            if out.get("tool_choice").is_none() {
                out["tool_choice"] = json!({"type":"auto"});
            }
            out["tool_choice"]["disable_parallel_tool_use"] = json!(parallel == false);
        }
    }
    let mut system = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    let mut call_order = BTreeMap::new();
    let mut call_names = BTreeMap::<String, (String, String)>::new();
    let mut used_ids = BTreeSet::new();
    for message in array(&body["messages"], "messages")? {
        let role = message["role"].as_str().unwrap();
        if role == "system" {
            for part in text_parts(&message["content"])? {
                object(&part, "system content", &["type", "text"])?;
                if part["type"] != "text" {
                    return unsupported("non-text system content");
                }
                string(&part["text"], "system text")?;
                system.push(if to == Format::Claude {
                    part
                } else {
                    json!({"text":part["text"]})
                });
            }
            continue;
        }
        let mut parts = Vec::new();
        if role == "tool" {
            let content = tool_content(&message["content"])?;
            let original = message["tool_call_id"].as_str().unwrap();
            if to == Format::Claude {
                parts.push(json!({"type":"tool_result","tool_use_id":original,"content":content}));
            } else {
                let Some((id, function)) = call_names.remove(original) else {
                    return unsupported("tool result without preceding call");
                };
                let text = if let Some(text) = content.as_str() {
                    text.to_owned()
                } else {
                    content
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|part| part["text"].as_str().unwrap())
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let result: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!(text));
                parts.push(json!({"functionResponse":{"id":id,"name":function,"response":if result.is_object() { result } else { json!({"result":result}) }}}));
            }
        } else {
            parts.extend(reasoning_parts(message, to)?);
            let visible = text_parts(&message["content"])?;
            let signed_visible = signed_text_parts(message, &visible, to)?;
            for part in if signed_visible.is_some() {
                Vec::new()
            } else {
                visible
            } {
                match part["type"].as_str() {
                    Some("text") => {
                        object(&part, "content.text", &["type", "text"])?;
                        string(&part["text"], "content.text")?;
                        parts.push(if to == Format::Claude {
                            part
                        } else {
                            json!({"text":part["text"]})
                        });
                    }
                    Some("image_url") if role == "user" => parts.push(openai_image(&part, to)?),
                    _ => return unsupported("content block type or image on assistant message"),
                }
            }
            if let Some(visible) = signed_visible {
                parts.extend(visible);
            }
            if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
                for call in array(calls, "tool_calls")? {
                    let original = call["id"].as_str().unwrap();
                    let function = call["function"]["name"].as_str().unwrap();
                    let input = args(&call["function"]["arguments"])?;
                    if to == Format::Claude {
                        if call.get("extra_content").is_some() {
                            return unsupported("foreign tool thought signature on Claude request");
                        }
                        parts.push(
                            json!({"type":"tool_use","id":original,"name":function,"input":input}),
                        );
                    } else {
                        let mut id = original.to_owned();
                        let mut suffix = 1;
                        while !used_ids.insert(id.clone()) {
                            id = format!("{original}_{suffix}");
                            suffix += 1;
                        }
                        let mut part =
                            json!({"functionCall":{"id":id,"name":function,"args":input}});
                        if let Some(extra) = call.get("extra_content") {
                            object(extra, "tool extra_content", &["google"])?;
                            object(
                                &extra["google"],
                                "tool extra_content.google",
                                &["thought_signature"],
                            )?;
                            string(
                                &extra["google"]["thought_signature"],
                                "tool thought_signature",
                            )?;
                            part["thoughtSignature"] = extra["google"]["thought_signature"].clone();
                        }
                        call_order.insert(id.clone(), call_order.len());
                        call_names.insert(original.to_owned(), (id, function.to_owned()));
                        parts.push(part);
                    }
                }
            }
        }
        let native_role = if to == Format::Claude {
            if role == "assistant" {
                "assistant"
            } else {
                "user"
            }
        } else if role == "assistant" {
            "model"
        } else {
            "user"
        };
        let key = if to == Format::Claude {
            "content"
        } else {
            "parts"
        };
        if to == Format::Gemini
            && role == "tool"
            && let Some(previous) = messages.last_mut().filter(|m| {
                m["role"] == "user"
                    && m["parts"].as_array().is_some_and(|p| {
                        !p.is_empty() && p.iter().all(|p| p.get("functionResponse").is_some())
                    })
            })
        {
            previous["parts"].as_array_mut().unwrap().extend(parts);
        } else {
            messages.push(json!({"role":native_role,key:parts}));
        }
    }
    if to == Format::Gemini {
        for message in &mut messages {
            message["parts"]
                .as_array_mut()
                .unwrap()
                .sort_by_key(|part| {
                    part["functionResponse"]["id"]
                        .as_str()
                        .and_then(|id| call_order.get(id))
                        .copied()
                        .unwrap_or(usize::MAX)
                });
        }
    }
    out[if to == Format::Claude {
        "messages"
    } else {
        "contents"
    }] = json!(messages);
    if !system.is_empty() {
        out[if to == Format::Claude {
            "system"
        } else {
            "systemInstruction"
        }] = if to == Format::Claude {
            json!(system)
        } else {
            json!({"parts":system})
        };
    }
    Ok(out)
}

fn reasoning_parts(message: &Value, to: Format) -> crate::Result<Vec<Value>> {
    let text = message
        .get("reasoning_content")
        .filter(|v| !v.is_null())
        .map(|v| string(v, "reasoning_content"))
        .transpose()?
        .unwrap_or("");
    let Some(details) = message.get("reasoning_details").filter(|v| !v.is_null()) else {
        if text.is_empty() {
            return Ok(Vec::new());
        }
        if to == Format::Claude {
            return unsupported("Claude replay requires original signed thinking blocks");
        }
        return Ok(vec![json!({"thought":true,"text":text})]);
    };
    let mut parts = Vec::new();
    let mut combined = String::new();
    for detail in array(details, "reasoning_details")? {
        object(
            detail,
            "reasoning_details[]",
            &["type", "format", "text", "signature", "data"],
        )?;
        match (detail["format"].as_str(), detail["type"].as_str(), to) {
            (Some("anthropic"), Some("reasoning.text"), Format::Claude) => {
                let text = string(&detail["text"], "reasoning detail text")?;
                name(&detail["signature"], "thinking signature")?;
                combined.push_str(text);
                parts.push(
                    json!({"type":"thinking","thinking":text,"signature":detail["signature"]}),
                );
            }
            (Some("anthropic"), Some("reasoning.encrypted"), Format::Claude) => {
                name(&detail["data"], "redacted thinking data")?;
                parts.push(json!({"type":"redacted_thinking","data":detail["data"]}));
            }
            (Some("google"), Some("reasoning.text"), Format::Gemini) => {
                let text = string(&detail["text"], "reasoning detail text")?;
                combined.push_str(text);
                let mut part = json!({"thought":true,"text":text});
                if let Some(signature) = detail.get("signature") {
                    string(signature, "thought signature")?;
                    part["thoughtSignature"] = signature.clone();
                }
                parts.push(part);
            }
            _ => {
                return unsupported(
                    "foreign or unrecognized reasoning detail; native signatures cannot cross vendors",
                );
            }
        }
    }
    if !text.is_empty() && text != combined {
        return unsupported("reasoning_content does not match reasoning_details");
    }
    Ok(parts)
}
fn set_reasoning(message: &mut Value, details: Vec<Value>) {
    if !details.is_empty() {
        let text = details
            .iter()
            .filter_map(|d| d["text"].as_str())
            .collect::<String>();
        if !text.is_empty() {
            message["reasoning_content"] = json!(text);
        }
        message["reasoning_details"] = json!(details);
    }
}
fn claude_messages(message: &Value) -> crate::Result<Vec<Value>> {
    object(message, "Claude message", &["role", "content"])?;
    let role = string(&message["role"], "Claude message role")?;
    if !["user", "assistant"].contains(&role) {
        return unsupported("Claude message role");
    }
    if message["content"].is_string() {
        return Ok(vec![message.clone()]);
    }
    let mut parts = Vec::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    let mut reasoning = Vec::new();
    for block in array(&message["content"], "Claude content")? {
        let kind = block["type"].as_str();
        if (!calls.is_empty() && kind != Some("tool_use"))
            || (!parts.is_empty()
                && matches!(kind, Some("thinking" | "redacted_thinking" | "tool_result")))
        {
            return unsupported("Claude block ordering cannot be represented by Chat messages");
        }
        match kind {
            Some("text") => {
                object(block, "Claude text", &["type", "text"])?;
                string(&block["text"], "Claude text")?;
                parts.push(block.clone());
            }
            Some("image") if role == "user" => {
                object(block, "Claude image", &["type", "source"])?;
                parts.push(image_url(&block["source"])?);
            }
            Some("tool_use") if role == "assistant" => {
                object(block, "Claude tool_use", &["type", "id", "name", "input"])?;
                name(&block["id"], "tool_use.id")?;
                name(&block["name"], "tool_use.name")?;
                if !block["input"].is_object() {
                    return unsupported("tool_use.input must be an object");
                }
                calls.push(json!({"id":block["id"],"type":"function","function":{"name":block["name"],"arguments":serde_json::to_string(&block["input"])?}}));
            }
            Some("tool_result") if role == "user" => {
                object(
                    block,
                    "Claude tool_result",
                    &["type", "tool_use_id", "content", "is_error"],
                )?;
                if block.get("is_error").is_some_and(|v| v != false) {
                    return unsupported("tool_result.is_error");
                }
                name(&block["tool_use_id"], "tool_result.tool_use_id")?;
                results.push(json!({"role":"tool","tool_call_id":block["tool_use_id"],"content":tool_content(&block["content"])?}));
            }
            Some("thinking") if role == "assistant" => {
                object(block, "Claude thinking", &["type", "thinking", "signature"])?;
                string(&block["thinking"], "thinking")?;
                name(&block["signature"], "thinking signature")?;
                reasoning.push(json!({"type":"reasoning.text","format":"anthropic","text":block["thinking"],"signature":block["signature"]}));
            }
            Some("redacted_thinking") if role == "assistant" => {
                object(block, "Claude redacted_thinking", &["type", "data"])?;
                name(&block["data"], "redacted thinking data")?;
                reasoning.push(
                    json!({"type":"reasoning.encrypted","format":"anthropic","data":block["data"]}),
                );
            }
            _ => return unsupported("Claude block type, cache controls, or role for block"),
        }
    }
    let mut out = json!({"role":role,"content":compact(parts)});
    if !calls.is_empty() {
        out["tool_calls"] = json!(calls);
    }
    set_reasoning(&mut out, reasoning);
    if !out["content"].is_null()
        || out.get("tool_calls").is_some()
        || out.get("reasoning_details").is_some()
        || results.is_empty()
    {
        results.push(out);
    }
    Ok(results)
}
fn claude_request(body: &Value, model: &str) -> crate::Result<Value> {
    object(
        body,
        "Claude request",
        &[
            "model",
            "messages",
            "system",
            "max_tokens",
            "temperature",
            "top_p",
            "stop_sequences",
            "stream",
            "tools",
            "tool_choice",
        ],
    )?;
    let mut out = json!({"model":model,"messages":[]});
    copy(
        body,
        &mut out,
        &[
            ("max_tokens", "max_tokens"),
            ("temperature", "temperature"),
            ("top_p", "top_p"),
            ("stop_sequences", "stop"),
            ("stream", "stream"),
        ],
    );
    let mut messages = Vec::new();
    if let Some(system) = body.get("system") {
        for part in text_parts(system)? {
            object(&part, "Claude system", &["type", "text"])?;
            if part["type"] != "text" {
                return unsupported("non-text Claude system block");
            }
            string(&part["text"], "system text")?;
            messages.push(json!({"role":"system","content":part["text"]}));
        }
    }
    for message in array(&body["messages"], "Claude messages")? {
        messages.extend(claude_messages(message)?);
    }
    out["messages"] = json!(messages);
    if let Some(tools) = body.get("tools") {
        let mut converted = Vec::new();
        for tool in array(tools, "Claude tools")? {
            object(
                tool,
                "Claude tool",
                &["name", "description", "input_schema"],
            )?;
            name(&tool["name"], "tool name")?;
            if !tool["input_schema"].is_object() {
                return unsupported("tool input_schema must be an object");
            }
            let mut function = json!({"name":tool["name"],"parameters":tool["input_schema"]});
            copy(tool, &mut function, &[("description", "description")]);
            converted.push(json!({"type":"function","function":function}));
        }
        out["tools"] = json!(converted);
    }
    if let Some(value) = body.get("tool_choice") {
        object(
            value,
            "Claude tool_choice",
            &["type", "name", "disable_parallel_tool_use"],
        )?;
        let mut clean = value.clone();
        if let Some(disable) = clean
            .as_object_mut()
            .unwrap()
            .remove("disable_parallel_tool_use")
        {
            if !disable.is_boolean() {
                return unsupported("disable_parallel_tool_use must be boolean");
            }
            out["parallel_tool_calls"] = json!(disable == false);
        }
        out["tool_choice"] = choice(&clean, Format::Claude, Format::OpenAi)?;
    }
    Ok(out)
}

// Gemini's native Schema type names are uppercase; JSON Schema fields are otherwise retained.
fn gemini_schema(value: &Value) -> crate::Result<Value> {
    let Some(map) = value.as_object() else {
        return unsupported("Gemini schema must be an object");
    };
    let mut out = map.clone();
    if out.contains_key("propertyOrdering") {
        return unsupported("Gemini schema.propertyOrdering");
    }
    if let Some(typ) = map.get("type") {
        let typ = string(typ, "Gemini schema.type")?.to_ascii_lowercase();
        if ![
            "object", "array", "string", "number", "integer", "boolean", "null",
        ]
        .contains(&typ.as_str())
        {
            return unsupported("Gemini schema type");
        }
        out.insert("type".into(), json!(typ));
    }
    if let Some(properties) = map.get("properties") {
        let Some(properties) = properties.as_object() else {
            return unsupported("Gemini schema.properties");
        };
        let mut converted = Map::new();
        for (name, schema) in properties {
            converted.insert(name.clone(), gemini_schema(schema)?);
        }
        out.insert("properties".into(), Value::Object(converted));
    }
    for key in ["items", "additionalProperties"] {
        if let Some(schema) = map.get(key).filter(|v| v.is_object()) {
            out.insert(key.into(), gemini_schema(schema)?);
        }
    }
    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(schemas) = map.get(key) {
            out.insert(
                key.into(),
                Value::Array(
                    array(schemas, key)?
                        .iter()
                        .map(gemini_schema)
                        .collect::<crate::Result<Vec<_>>>()?,
                ),
            );
        }
    }
    if let Some(nullable) = out.remove("nullable") {
        if !nullable.is_boolean() {
            return unsupported("Gemini schema.nullable");
        }
        if nullable == true {
            return Ok(json!({"anyOf":[out,{"type":"null"}]}));
        }
    }
    Ok(Value::Object(out))
}
#[derive(Default)]
struct GeminiCalls {
    next: usize,
    // Names may repeat in parallel. Consume replies in call order, retaining real IDs.
    pending: VecDeque<(String, String)>,
    used: BTreeSet<String>,
}
fn gemini_messages(content: &Value, state: &mut GeminiCalls) -> crate::Result<Vec<Value>> {
    object(content, "Gemini content", &["role", "parts"])?;
    let role = match content
        .get("role")
        .map(|v| string(v, "Gemini content role"))
        .transpose()?
        .unwrap_or("user")
    {
        "model" => "assistant",
        "user" => "user",
        _ => return unsupported("Gemini content role"),
    };
    let mut parts = Vec::new();
    let mut calls = Vec::new();
    let mut results = Vec::new();
    let mut reasoning = Vec::new();
    let mut visible_native = Vec::new();
    let mut signed_visible = false;
    for part in array(&content["parts"], "Gemini parts")? {
        object(
            part,
            "Gemini part",
            &[
                "text",
                "thought",
                "thoughtSignature",
                "inlineData",
                "fileData",
                "functionCall",
                "functionResponse",
            ],
        )?;
        let variants = [
            "text",
            "inlineData",
            "fileData",
            "functionCall",
            "functionResponse",
        ]
        .iter()
        .filter(|key| part.get(**key).is_some())
        .count();
        if variants != 1 {
            return unsupported("Gemini part must have exactly one content field");
        }
        if (!calls.is_empty() && part.get("functionCall").is_none())
            || (!parts.is_empty()
                && (part["thought"] == true || part.get("functionResponse").is_some()))
        {
            return unsupported("Gemini part ordering cannot be represented by Chat messages");
        }
        if let Some(text) = part.get("text") {
            string(text, "Gemini text")?;
            if part["thought"] == true {
                if role != "assistant" {
                    return unsupported("Gemini thought on user message");
                }
                let mut detail = json!({"type":"reasoning.text","format":"google","text":text});
                if let Some(signature) = part.get("thoughtSignature") {
                    string(signature, "thought signature")?;
                    detail["signature"] = signature.clone();
                }
                reasoning.push(detail);
            } else {
                if part.get("thought").is_some_and(|v| v != false) {
                    return unsupported("invalid Gemini thought flag");
                }
                if let Some(signature) = part.get("thoughtSignature") {
                    if role != "assistant" {
                        return unsupported("signed Gemini text on user message");
                    }
                    string(signature, "visible text thought signature")?;
                    signed_visible = true;
                }
                visible_native.push(part.clone());
                parts.push(json!({"type":"text","text":text}));
            }
        } else if let Some(call) = part.get("functionCall") {
            if role != "assistant" || part.get("thought").is_some() {
                return unsupported("Gemini functionCall role or thought flag");
            }
            object(call, "Gemini functionCall", &["id", "name", "args"])?;
            let function = name(&call["name"], "functionCall.name")?;
            let input = call.get("args").cloned().unwrap_or_else(|| json!({}));
            if !input.is_object() {
                return unsupported("functionCall.args must be an object");
            }
            let id = if let Some(id) = call.get("id") {
                name(id, "functionCall.id")?.to_owned()
            } else {
                state.next += 1;
                format!("call_{}_{}", function, state.next)
            };
            if !state.used.insert(id.clone()) {
                return unsupported("duplicate Gemini functionCall id");
            }
            state.pending.push_back((id.clone(), function.to_owned()));
            let mut converted = json!({"id":id,"type":"function","function":{"name":function,"arguments":serde_json::to_string(&input)?}});
            if let Some(signature) = part.get("thoughtSignature") {
                string(signature, "tool thought signature")?;
                converted["extra_content"] = json!({"google":{"thought_signature":signature}});
            }
            calls.push(converted);
        } else if let Some(result) = part.get("functionResponse") {
            if role != "user"
                || part.get("thoughtSignature").is_some()
                || part.get("thought").is_some()
            {
                return unsupported("Gemini functionResponse role or thought metadata");
            }
            object(
                result,
                "Gemini functionResponse",
                &["id", "name", "response"],
            )?;
            let function = name(&result["name"], "functionResponse.name")?;
            if !result["response"].is_object() {
                return unsupported("functionResponse.response must be an object");
            }
            let specified = result
                .get("id")
                .map(|v| name(v, "functionResponse.id"))
                .transpose()?;
            let index = state
                .pending
                .iter()
                .position(|(id, n)| n == function && specified.is_none_or(|value| value == id));
            let Some(index) = index else {
                return unsupported("Gemini functionResponse without matching call");
            };
            let (id, _) = state.pending.remove(index).unwrap();
            results.push(json!({"role":"tool","tool_call_id":id,"content":serde_json::to_string(&result["response"])?}));
        } else {
            if role != "user"
                || part.get("thoughtSignature").is_some()
                || part.get("thought").is_some()
            {
                return unsupported("Gemini image role or thought metadata");
            }
            let source = if let Some(data) = part.get("inlineData") {
                object(data, "Gemini inlineData", &["mimeType", "data"])?;
                json!({"type":"base64","media_type":data["mimeType"],"data":data["data"]})
            } else {
                let data = &part["fileData"];
                object(data, "Gemini fileData", &["mimeType", "fileUri"])?;
                if !string(&data["mimeType"], "fileData.mimeType")?.starts_with("image/") {
                    return unsupported("non-image Gemini fileData");
                }
                json!({"type":"url","url":data["fileUri"]})
            };
            parts.push(image_url(&source)?);
        }
    }
    let mut out = json!({"role":role,"content":compact(parts)});
    if !calls.is_empty() {
        out["tool_calls"] = json!(calls);
    }
    set_reasoning(&mut out, reasoning);
    if signed_visible {
        out["extra_content"] = json!({"google":{"text_parts":visible_native}});
    }
    if !out["content"].is_null()
        || out.get("tool_calls").is_some()
        || out.get("reasoning_details").is_some()
        || results.is_empty()
    {
        results.push(out);
    }
    Ok(results)
}
fn gemini_request(body: &Value, model: &str) -> crate::Result<Value> {
    object(
        body,
        "Gemini request",
        &[
            "model",
            "contents",
            "systemInstruction",
            "generationConfig",
            "tools",
            "toolConfig",
            "stream",
        ],
    )?;
    let mut out = json!({"model":model,"messages":[]});
    copy(body, &mut out, &[("stream", "stream")]);
    if let Some(config) = body.get("generationConfig") {
        object(
            config,
            "generationConfig",
            &[
                "maxOutputTokens",
                "temperature",
                "topP",
                "stopSequences",
                "candidateCount",
            ],
        )?;
        copy(
            config,
            &mut out,
            &[
                ("maxOutputTokens", "max_tokens"),
                ("temperature", "temperature"),
                ("topP", "top_p"),
                ("stopSequences", "stop"),
                ("candidateCount", "n"),
            ],
        );
    }
    let mut messages = Vec::new();
    if let Some(system) = body.get("systemInstruction") {
        object(system, "systemInstruction", &["role", "parts"])?;
        for part in array(&system["parts"], "systemInstruction.parts")? {
            object(part, "systemInstruction part", &["text"])?;
            string(&part["text"], "systemInstruction text")?;
            messages.push(json!({"role":"system","content":part["text"]}));
        }
    }
    let mut state = GeminiCalls::default();
    for content in array(&body["contents"], "Gemini contents")? {
        messages.extend(gemini_messages(content, &mut state)?);
    }
    out["messages"] = json!(messages);
    if let Some(tools) = body.get("tools") {
        let mut converted = Vec::new();
        for tool in array(tools, "Gemini tools")? {
            object(tool, "Gemini tool", &["functionDeclarations"])?;
            for function in array(&tool["functionDeclarations"], "functionDeclarations")? {
                object(
                    function,
                    "function declaration",
                    &["name", "description", "parameters", "parametersJsonSchema"],
                )?;
                name(&function["name"], "function name")?;
                if function.get("parameters").is_some()
                    && function.get("parametersJsonSchema").is_some()
                {
                    return unsupported("both Gemini parameter schema fields");
                }
                let schema = if let Some(schema) = function.get("parametersJsonSchema") {
                    schema.clone()
                } else if let Some(schema) = function.get("parameters") {
                    gemini_schema(schema)?
                } else {
                    json!({"type":"object","properties":{}})
                };
                if !schema.is_object() {
                    return unsupported("function schema must be an object");
                }
                let mut converted_function = json!({"name":function["name"],"parameters":schema});
                copy(
                    function,
                    &mut converted_function,
                    &[("description", "description")],
                );
                converted.push(json!({"type":"function","function":converted_function}));
            }
        }
        out["tools"] = json!(converted);
    }
    if let Some(config) = body.get("toolConfig") {
        object(config, "toolConfig", &["functionCallingConfig"])?;
        let function = &config["functionCallingConfig"];
        object(
            function,
            "functionCallingConfig",
            &["mode", "allowedFunctionNames"],
        )?;
        let mode = string(&function["mode"], "functionCallingConfig.mode")?;
        let mut converted = match mode {
            "AUTO" => json!("auto"),
            "NONE" => json!("none"),
            "ANY" => json!("required"),
            _ => return unsupported("Gemini function calling mode"),
        };
        if let Some(names) = function.get("allowedFunctionNames") {
            let names = array(names, "allowedFunctionNames")?;
            if mode != "ANY" || names.len() != 1 {
                return unsupported("allowedFunctionNames requires one forced function");
            }
            name(&names[0], "allowed function name")?;
            converted = json!({"type":"function","function":{"name":names[0]}});
        }
        out["tool_choice"] = converted;
    }
    Ok(out)
}

fn openai_usage(raw: &Value) -> crate::Result<Value> {
    object(
        raw,
        "OpenAI usage",
        &[
            "prompt_tokens",
            "completion_tokens",
            "total_tokens",
            "prompt_tokens_details",
            "completion_tokens_details",
        ],
    )?;
    for key in ["prompt_tokens", "completion_tokens", "total_tokens"] {
        number(&raw[key], key)?;
    }
    if let Some(details) = raw.get("prompt_tokens_details") {
        object(
            details,
            "prompt_tokens_details",
            &["cached_tokens", "cache_creation_tokens", "audio_tokens"],
        )?;
        for value in details.as_object().unwrap().values() {
            number(value, "prompt token detail")?;
        }
    }
    if let Some(details) = raw.get("completion_tokens_details") {
        object(
            details,
            "completion_tokens_details",
            &[
                "reasoning_tokens",
                "audio_tokens",
                "accepted_prediction_tokens",
                "rejected_prediction_tokens",
            ],
        )?;
        for value in details.as_object().unwrap().values() {
            number(value, "completion token detail")?;
        }
    }
    for (details, keys) in [
        ("prompt_tokens_details", &["audio_tokens"][..]),
        (
            "completion_tokens_details",
            &[
                "audio_tokens",
                "accepted_prediction_tokens",
                "rejected_prediction_tokens",
            ][..],
        ),
    ] {
        for key in keys {
            if count(&raw[details], key)? != 0 {
                return unsupported(format!("{details}.{key}"));
            }
        }
    }
    Ok(raw.clone())
}
const GEMINI_USAGE_DIAGNOSTICS: &[&str] = &[
    "promptTokensDetails",
    "candidatesTokensDetails",
    "cacheTokensDetails",
    "toolUsePromptTokensDetails",
    "toolUsePromptTokenCount",
    "trafficType",
];
fn gemini_usage_diagnostics(raw: &Value) -> crate::Result<Value> {
    let mut diagnostics = json!({});
    for key in GEMINI_USAGE_DIAGNOSTICS {
        if let Some(value) = raw.get(*key) {
            if *key == "trafficType" {
                string(value, "usage trafficType")?;
            } else if *key == "toolUsePromptTokenCount" {
                number(value, key)?;
            } else {
                for detail in array(value, key)? {
                    object(detail, "usage modality detail", &["modality", "tokenCount"])?;
                    name(&detail["modality"], "token modality")?;
                    number(&detail["tokenCount"], "modality tokenCount")?;
                }
            }
            diagnostics[*key] = value.clone();
        }
    }
    Ok(diagnostics)
}
fn response_metadata(body: &Value) -> crate::Result<Value> {
    let metadata = body
        .get("extra_content")
        .cloned()
        .unwrap_or_else(|| json!({}));
    object(
        &metadata,
        "response.extra_content",
        &["google", "anthropic"],
    )?;
    if let Some(google) = metadata.get("google") {
        object(google, "response.extra_content.google", &["usage_metadata"])?;
        let raw = &google["usage_metadata"];
        object(raw, "retained usage metadata", GEMINI_USAGE_DIAGNOSTICS)?;
        gemini_usage_diagnostics(raw)?;
    }
    if let Some(anthropic) = metadata.get("anthropic") {
        object(
            anthropic,
            "response.extra_content.anthropic",
            &["stop_sequence"],
        )?;
        string(&anthropic["stop_sequence"], "matched stop sequence")?;
    }
    Ok(metadata)
}

fn native_usage(raw: &Value, from: Format) -> crate::Result<Value> {
    let (prompt, completion, total, cached, created, reasoning) = if from == Format::Claude {
        object(
            raw,
            "Claude usage",
            &[
                "input_tokens",
                "output_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ],
        )?;
        let cached = count(raw, "cache_read_input_tokens")?;
        let created = count(raw, "cache_creation_input_tokens")?;
        let prompt = sum(sum(count(raw, "input_tokens")?, cached)?, created)?;
        let completion = count(raw, "output_tokens")?;
        (
            prompt,
            completion,
            sum(prompt, completion)?,
            cached,
            created,
            0,
        )
    } else {
        object(
            raw,
            "Gemini usage",
            &[
                "promptTokenCount",
                "candidatesTokenCount",
                "thoughtsTokenCount",
                "totalTokenCount",
                "cachedContentTokenCount",
                "promptTokensDetails",
                "candidatesTokensDetails",
                "cacheTokensDetails",
                "toolUsePromptTokensDetails",
                "toolUsePromptTokenCount",
                "trafficType",
            ],
        )?;
        let prompt = count(raw, "promptTokenCount")?;
        let reasoning = count(raw, "thoughtsTokenCount")?;
        let supplied_total = raw
            .get("totalTokenCount")
            .map(|v| number(v, "totalTokenCount"))
            .transpose()?;
        let visible = if let Some(v) = raw.get("candidatesTokenCount") {
            number(v, "candidatesTokenCount")?
        } else {
            supplied_total
                .unwrap_or(0)
                .checked_sub(sum(prompt, reasoning)?)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Unsupported,
                        "unsupported protocol conversion: inconsistent Gemini token counts",
                    )
                })?
        };
        let completion = sum(visible, reasoning)?;
        (
            prompt,
            completion,
            supplied_total.unwrap_or(sum(prompt, completion)?),
            count(raw, "cachedContentTokenCount")?,
            0,
            reasoning,
        )
    };
    let mut out =
        json!({"prompt_tokens":prompt,"completion_tokens":completion,"total_tokens":total});
    if cached > 0 || created > 0 {
        out["prompt_tokens_details"] = json!({});
        if cached > 0 {
            out["prompt_tokens_details"]["cached_tokens"] = json!(cached);
        }
        if created > 0 {
            out["prompt_tokens_details"]["cache_creation_tokens"] = json!(created);
        }
    }
    if reasoning > 0 {
        out["completion_tokens_details"] = json!({"reasoning_tokens":reasoning});
    }
    Ok(out)
}
fn to_native_usage(raw: &Value, to: Format) -> crate::Result<Value> {
    openai_usage(raw)?;
    let prompt = count(raw, "prompt_tokens")?;
    let output = count(raw, "completion_tokens")?;
    let total = count(raw, "total_tokens")?;
    let cached = count(&raw["prompt_tokens_details"], "cached_tokens")?;
    let created = count(&raw["prompt_tokens_details"], "cache_creation_tokens")?;
    let reasoning = count(&raw["completion_tokens_details"], "reasoning_tokens")?;
    if to == Format::Claude {
        if total != sum(prompt, output)? || reasoning > 0 {
            return unsupported("Claude usage cannot retain total/reasoning token detail");
        }
        let Some(input) = prompt.checked_sub(sum(cached, created)?) else {
            return unsupported("cache token count exceeds prompt tokens");
        };
        let mut out = json!({"input_tokens":input,"output_tokens":output});
        if cached > 0 {
            out["cache_read_input_tokens"] = json!(cached);
        }
        if created > 0 {
            out["cache_creation_input_tokens"] = json!(created);
        }
        Ok(out)
    } else {
        if created > 0 {
            return unsupported("Gemini usage cannot retain cache creation token detail");
        }
        let Some(visible) = output.checked_sub(reasoning) else {
            return unsupported("reasoning tokens exceed completion tokens");
        };
        let mut out = json!({"promptTokenCount":prompt,"candidatesTokenCount":visible,"totalTokenCount":total});
        if reasoning > 0 {
            out["thoughtsTokenCount"] = json!(reasoning);
        }
        if cached > 0 {
            out["cachedContentTokenCount"] = json!(cached);
        }
        Ok(out)
    }
}
fn finish(value: &Value, from: Format, calls: bool) -> crate::Result<&'static str> {
    let value = string(value, "finish reason")?;
    match (from, value) {
        (Format::OpenAi, "stop")
        | (Format::Claude, "end_turn" | "stop_sequence")
        | (Format::Gemini, "STOP") => Ok(if calls { "tool_calls" } else { "stop" }),
        (Format::OpenAi, "length")
        | (Format::Claude, "max_tokens")
        | (Format::Gemini, "MAX_TOKENS") => Ok("length"),
        (Format::OpenAi, "tool_calls") | (Format::Claude, "tool_use") => Ok("tool_calls"),
        (Format::OpenAi, "content_filter")
        | (Format::Claude, "refusal")
        | (Format::Gemini, "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT") => {
            Ok("content_filter")
        }
        _ => unsupported(format!("finish reason {value}")),
    }
}
fn canonical_response(body: &Value, from: Format, model: &str) -> crate::Result<Value> {
    if from == Format::OpenAi {
        object(
            body,
            "OpenAI response",
            &[
                "id",
                "object",
                "created",
                "model",
                "choices",
                "usage",
                "system_fingerprint",
                "service_tier",
                "extra_content",
            ],
        )?;
        let choices = array(&body["choices"], "response choices")?;
        if choices.len() != 1 {
            return unsupported("response requires exactly one choice");
        }
        let choice = &choices[0];
        object(
            choice,
            "response choice",
            &["index", "message", "finish_reason", "logprobs"],
        )?;
        if choice.get("logprobs").is_some_and(|v| !v.is_null()) {
            return unsupported("response logprobs");
        }
        let message = &choice["message"];
        if message["role"] != "assistant" {
            return unsupported("response role must be assistant");
        }
        validate_openai(&json!({"messages":[message]}))?;
        finish(
            &choice["finish_reason"],
            from,
            message
                .get("tool_calls")
                .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty())),
        )?;
        if let Some(usage) = body.get("usage").filter(|v| !v.is_null()) {
            openai_usage(usage)?;
        }
        response_metadata(body)?;
        return Ok(body.clone());
    }
    let mut metadata = response_metadata(body)?;
    let (id, message, reason, usage) = if from == Format::Claude {
        object(
            body,
            "Claude response",
            &[
                "id",
                "type",
                "role",
                "model",
                "content",
                "stop_reason",
                "stop_sequence",
                "usage",
                "extra_content",
            ],
        )?;
        if body["role"] != "assistant" || body.get("type").is_some_and(|v| v != "message") {
            return unsupported("Claude response type or role");
        }
        if let Some(sequence) = body.get("stop_sequence").filter(|v| !v.is_null()) {
            string(sequence, "matched Claude stop_sequence")?;
            if body["stop_reason"] != "stop_sequence" {
                return unsupported("stop sequence metadata contradicts stop reason");
            }
            metadata["anthropic"] = json!({"stop_sequence":sequence});
        }
        let mut converted =
            claude_messages(&json!({"role":"assistant","content":body["content"]}))?;
        if converted.len() != 1 {
            return unsupported("Claude response message count");
        }
        (
            body.get("id").cloned(),
            converted.remove(0),
            body["stop_reason"].clone(),
            body.get("usage").cloned(),
        )
    } else {
        // Safety/rating and transport metadata are diagnostic, not generated content.
        object(
            body,
            "Gemini response",
            &[
                "responseId",
                "modelVersion",
                "candidates",
                "usageMetadata",
                "promptFeedback",
                "createTime",
                "extra_content",
            ],
        )?;
        if body
            .get("promptFeedback")
            .is_some_and(|v| v.get("blockReason").is_some())
        {
            return unsupported("Gemini blocked prompt response");
        }
        let candidates = array(&body["candidates"], "Gemini candidates")?;
        if candidates.len() != 1 {
            return unsupported("response requires exactly one Gemini candidate");
        }
        let candidate = &candidates[0];
        object(
            candidate,
            "Gemini candidate",
            &[
                "index",
                "content",
                "finishReason",
                "safetyRatings",
                "avgLogprobs",
                "finishMessage",
                "tokenCount",
            ],
        )?;
        let mut state = GeminiCalls::default();
        let mut converted = gemini_messages(&candidate["content"], &mut state)?;
        if converted.len() != 1 || converted[0]["role"] != "assistant" {
            return unsupported("Gemini response must contain one model message");
        }
        (
            body.get("responseId").cloned(),
            converted.remove(0),
            candidate["finishReason"].clone(),
            body.get("usageMetadata").cloned(),
        )
    };
    let calls = message
        .get("tool_calls")
        .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()));
    let reason = finish(&reason, from, calls)?;
    let mut out = json!({"id":id.unwrap_or_else(||json!("chatcmpl-native")),"object":"chat.completion","created":0,"model":model,"choices":[{"index":0,"message":message,"finish_reason":reason}]});
    if let Some(usage) = usage {
        out["usage"] = native_usage(&usage, from)?;
        if from == Format::Gemini {
            let diagnostics = gemini_usage_diagnostics(&usage)?;
            if !diagnostics.as_object().unwrap().is_empty() {
                metadata["google"] = json!({"usage_metadata":diagnostics});
            }
        }
    }
    if !metadata.as_object().unwrap().is_empty() {
        out["extra_content"] = metadata;
    }
    Ok(out)
}

/// Response conversion is deliberately non-streaming. Native same-format payloads pass through.
pub fn response(body: &Value, from: Format, to: Format, model: &str) -> crate::Result<Value> {
    if from == to {
        return Ok(body.clone());
    }
    if from == Format::Responses || to == Format::Responses {
        return unsupported("Responses cross-format responses");
    }
    let canonical = canonical_response(body, from, model)?;
    if to == Format::OpenAi {
        return Ok(canonical);
    }
    let choice = &canonical["choices"][0];
    let native = from_openai_request(&json!({"messages":[choice["message"]]}), to, model)?;
    let reason = finish(&choice["finish_reason"], Format::OpenAi, false)?;
    let mut out = if to == Format::Claude {
        json!({"id":canonical["id"],"type":"message","role":"assistant","model":model,"content":native["messages"][0]["content"],
            "stop_reason":match reason {"stop"=>"end_turn","length"=>"max_tokens","tool_calls"=>"tool_use",_=>"refusal"},"stop_sequence":null})
    } else {
        json!({"responseId":canonical["id"],"modelVersion":model,"candidates":[{"index":0,"content":native["contents"][0],
            "finishReason":match reason {"length"=>"MAX_TOKENS","content_filter"=>"SAFETY",_=>"STOP"}}]})
    };
    if let Some(usage) = canonical.get("usage").filter(|v| !v.is_null()) {
        out[if to == Format::Claude {
            "usage"
        } else {
            "usageMetadata"
        }] = to_native_usage(usage, to)?;
    }
    let metadata = response_metadata(&canonical)?;
    if let Some(sequence) = metadata["anthropic"].get("stop_sequence") {
        if reason != "stop" {
            return unsupported("matched stop sequence contradicts finish reason");
        }
        if to == Format::Claude {
            out["stop_reason"] = json!("stop_sequence");
            out["stop_sequence"] = sequence.clone();
        }
    }
    if to == Format::Gemini
        && let Some(diagnostics) = metadata["google"].get("usage_metadata")
    {
        if out.get("usageMetadata").is_none() {
            out["usageMetadata"] = json!({});
        }
        for (key, value) in diagnostics.as_object().unwrap() {
            out["usageMetadata"][key] = value.clone();
        }
    }
    if !metadata.as_object().unwrap().is_empty() {
        out["extra_content"] = metadata;
    }
    Ok(out)
}
