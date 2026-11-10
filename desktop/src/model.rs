//! Port of open-sse/services/model.js. App-specific nodes and combos live above this layer.
use crate::Catalog;
use serde::Serialize;
use serde_json::{Map, Value};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Parsed {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub is_alias: bool,
    pub provider_alias: Option<String>,
}

#[derive(Serialize)]
pub struct ModelInfo {
    pub provider: Option<String>,
    pub model: Option<String>,
}

fn provider(catalog: &Catalog, alias: &str) -> String {
    catalog
        .provider_aliases
        .get(alias)
        .map(String::as_str)
        .unwrap_or(alias)
        .to_owned()
}

pub fn parse(catalog: &Catalog, input: Option<&str>) -> Parsed {
    let Some(input) = input.filter(|value| !value.is_empty()) else {
        return Parsed {
            provider: None,
            model: None,
            is_alias: false,
            provider_alias: None,
        };
    };
    if let Some((prefix, model)) = input.split_once('/') {
        return Parsed {
            provider: Some(provider(catalog, prefix)),
            model: Some(model.to_owned()),
            is_alias: false,
            provider_alias: Some(prefix.to_owned()),
        };
    }
    Parsed {
        provider: None,
        model: Some(input.to_owned()),
        is_alias: true,
        provider_alias: None,
    }
}

fn alias(catalog: &Catalog, value: &Value) -> Option<ModelInfo> {
    let (prefix, model) = if let Some(text) = value.as_str() {
        text.split_once('/')?
    } else {
        let prefix = value.get("provider")?.as_str().filter(|s| !s.is_empty())?;
        let model = value.get("model")?.as_str().filter(|s| !s.is_empty())?;
        (prefix, model)
    };
    Some(ModelInfo {
        provider: Some(provider(catalog, prefix)),
        model: Some(model.to_owned()),
    })
}

pub fn resolve(catalog: &Catalog, input: Option<&str>, aliases: &Map<String, Value>) -> ModelInfo {
    let parsed = parse(catalog, input);
    if !parsed.is_alias {
        return ModelInfo {
            provider: parsed.provider,
            model: parsed.model,
        };
    }
    let model = parsed
        .model
        .as_deref()
        .expect("parse only marks nonempty names as aliases");
    if let Some(resolved) = aliases.get(model).and_then(|value| alias(catalog, value)) {
        return resolved;
    }
    if model == "grok-build" {
        return ModelInfo {
            provider: Some(provider(catalog, "gcli")),
            model: Some(model.to_owned()),
        };
    }
    ModelInfo {
        provider: Some(infer(model).to_owned()),
        model: parsed.model,
    }
}

fn infer(model: &str) -> &'static str {
    let name = model.to_lowercase();
    if name == "codex-auto-review"
        || ["gpt-5.", "gpt-6.", "gpt-6-", "gpt-daybreak-", "gpt-reserve"]
            .iter()
            .any(|prefix| name.starts_with(prefix))
    {
        return "codex";
    }
    if name.starts_with("claude-") {
        return "anthropic";
    }
    if let Some((region, rest)) = name.split_once('.')
        && ["us", "eu", "ap", "global"].contains(&region)
        && ["anthropic.", "meta.", "amazon.", "mistral.", "xai."]
            .iter()
            .any(|prefix| rest.starts_with(prefix))
    {
        return "bedrock";
    }
    if name.starts_with("gemini-") {
        return "gemini";
    }
    if name.starts_with("deepseek-") {
        return "openrouter";
    }
    "openai"
}
