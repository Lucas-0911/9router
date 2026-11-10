//! Native repositories over the unchanged upstream SQLite schema.
use crate::{Catalog, Result, Store, model};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, types::Type};
use serde_json::{Map, Value, json};
use std::{collections::HashSet, io};

const CONNECTION_COLUMNS: &str =
    "id,provider,authType,name,email,priority,isActive,data,createdAt,updatedAt";
const NODE_COLUMNS: &str = "id,type,name,data,createdAt,updatedAt";
const COMBO_COLUMNS: &str = "id,name,kind,models,createdAt,updatedAt";
const KEY_COLUMNS: &str = "id,key,name,machineId,isActive,createdAt,accessRestricted,accessAllow";
// ponytail: individual entities are capped at 2 MiB; raise this only for larger validated imports.
const MAX_JSON_BYTES: usize = 2 * 1024 * 1024;

fn invalid(message: &str) -> Box<dyn std::error::Error + Send + Sync> {
    io::Error::new(io::ErrorKind::InvalidInput, message).into()
}

fn bounded(text: &str, maximum: usize, empty: bool) -> Result<()> {
    if (!empty && text.is_empty()) || text.len() > maximum || text.contains('\0') {
        return Err(invalid("Invalid or overlong text field"));
    }
    Ok(())
}

fn object(value: &Value) -> Result<Map<String, Value>> {
    if serde_json::to_vec(value)?.len() > MAX_JSON_BYTES {
        return Err(invalid("Entity exceeds the storage size limit"));
    }
    value
        .as_object()
        .cloned()
        .ok_or_else(|| invalid("Entity must be an object"))
}

fn text_field(value: &Map<String, Value>, key: &str, required: bool, maximum: usize) -> Result<()> {
    match value.get(key) {
        Some(Value::String(text)) => bounded(text, maximum, !required),
        None | Some(Value::Null) if !required => Ok(()),
        _ => Err(invalid("Missing or invalid text field")),
    }
}

fn bool_field(value: &Map<String, Value>, key: &str) -> Result<()> {
    if value.get(key).is_some_and(|value| !value.is_boolean()) {
        return Err(invalid("Expected a boolean field"));
    }
    Ok(())
}

fn json_column(row: &Row<'_>, index: usize) -> rusqlite::Result<Value> {
    let raw: String = row.get(index)?;
    serde_json::from_str(&raw).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(error))
    })
}

fn object_column(row: &Row<'_>, index: usize) -> rusqlite::Result<Map<String, Value>> {
    let value = json_column(row, index)?;
    value.as_object().cloned().ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            Type::Text,
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Stored entity JSON is not an object",
            )
            .into(),
        )
    })
}

fn connection_row(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut result = object_column(row, 7)?;
    for (index, field) in [
        (0, "id"),
        (1, "provider"),
        (2, "authType"),
        (3, "name"),
        (4, "email"),
        (8, "createdAt"),
        (9, "updatedAt"),
    ] {
        result.insert(field.into(), row.get::<_, Option<String>>(index)?.into());
    }
    result.insert("priority".into(), row.get::<_, Option<i64>>(5)?.into());
    result.insert(
        "isActive".into(),
        Value::Bool(row.get::<_, Option<i64>>(6)? == Some(1)),
    );
    Ok(Value::Object(result))
}

fn node_row(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut result = object_column(row, 3)?;
    for (index, field) in [
        (0, "id"),
        (1, "type"),
        (2, "name"),
        (4, "createdAt"),
        (5, "updatedAt"),
    ] {
        result.insert(field.into(), row.get::<_, Option<String>>(index)?.into());
    }
    Ok(Value::Object(result))
}

fn combo_row(row: &Row<'_>) -> rusqlite::Result<Value> {
    let models = json_column(row, 3)?;
    string_list(&models)
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(3, Type::Text, error))?;
    Ok(
        json!({"id": row.get::<_, String>(0)?, "name": row.get::<_, String>(1)?,
        "kind": row.get::<_, Option<String>>(2)?, "models": models,
        "createdAt": row.get::<_, String>(4)?, "updatedAt": row.get::<_, String>(5)?}),
    )
}

fn normalized_access(value: &Value) -> Result<Value> {
    let value = value
        .as_object()
        .ok_or_else(|| invalid("Key access must be an object"))?;
    if value
        .keys()
        .any(|key| key != "restricted" && key != "allow")
    {
        return Err(invalid("Unknown key access field"));
    }
    let restricted = value
        .get("restricted")
        .and_then(Value::as_bool)
        .ok_or_else(|| invalid("Key access.restricted must be a boolean"))?;
    let empty = Vec::new();
    let allow = match value.get("allow") {
        Some(value) => value
            .as_array()
            .ok_or_else(|| invalid("Key access.allow must be an array"))?,
        None => &empty,
    };
    if allow.len() > 200 {
        return Err(invalid("Key access has too many entries"));
    }
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();
    for entry in allow {
        let entry = entry
            .as_str()
            .ok_or_else(|| invalid("Key access entries must be strings"))?
            .trim();
        if entry.encode_utf16().count() > 256 || entry.contains('\0') {
            return Err(invalid("Invalid or overlong key access entry"));
        }
        if restricted && !entry.is_empty() && seen.insert(entry.to_lowercase()) {
            normalized.push(entry);
        }
    }
    Ok(json!({"restricted": restricted, "allow": normalized}))
}

fn key_row(row: &Row<'_>) -> rusqlite::Result<Value> {
    let restricted = row.get::<_, Option<i64>>(6)? == Some(1);
    let raw: Option<String> = row.get(7)?;
    let allow: Value = match raw {
        Some(raw) if !raw.is_empty() => serde_json::from_str(&raw).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(7, Type::Text, Box::new(error))
        })?,
        _ => json!([]),
    };
    // Invalid restrictions are reported; no read or patch can broaden a corrupt restricted key.
    let access = normalized_access(&json!({"restricted": restricted, "allow": allow}))
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(7, Type::Text, error))?;
    Ok(
        json!({"id": row.get::<_, String>(0)?, "key": row.get::<_, String>(1)?,
        "name": row.get::<_, Option<String>>(2)?, "machineId": row.get::<_, Option<String>>(3)?,
        "isActive": row.get::<_, Option<i64>>(4)? == Some(1), "createdAt": row.get::<_, String>(5)?, "access": access}),
    )
}

fn record_id(connection: &Connection, input: &Map<String, Value>) -> Result<String> {
    if let Some(value) = input.get("id") {
        let id = value
            .as_str()
            .ok_or_else(|| invalid("Entity id must be a string"))?;
        bounded(id, 256, false)?;
        return Ok(id.to_owned());
    }
    Ok(connection.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?)
}

fn merge_record(
    mut existing: Map<String, Value>,
    patch: Map<String, Value>,
    id: &str,
    now: &str,
) -> Result<Map<String, Value>> {
    let created = existing.get("createdAt").cloned();
    existing.extend(patch);
    existing.insert("id".into(), id.into());
    if let Some(created) = created {
        existing.insert("createdAt".into(), created);
    } else {
        existing.entry("createdAt").or_insert_with(|| now.into());
    }
    existing.insert("updatedAt".into(), now.into());
    text_field(&existing, "createdAt", true, 64)?;
    object(&Value::Object(existing.clone()))?;
    Ok(existing)
}

fn extra_data(value: &Map<String, Value>, columns: &[&str]) -> Result<String> {
    let mut extra = value.clone();
    for field in columns {
        extra.remove(*field);
    }
    Ok(serde_json::to_string(&extra)?)
}

fn now(connection: &Connection) -> Result<String> {
    Ok(
        connection.query_row("SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now')", [], |row| {
            row.get(0)
        })?,
    )
}

fn string_list(value: &Value) -> Result<Vec<String>> {
    let entries = value
        .as_array()
        .ok_or_else(|| invalid("Models must be an array of strings"))?;
    if entries.len() > 2000 {
        return Err(invalid("Too many combo models"));
    }
    entries
        .iter()
        .map(|entry| {
            let entry = entry
                .as_str()
                .ok_or_else(|| invalid("Model must be a string"))?;
            bounded(entry, 4096, false)?;
            Ok(entry.to_owned())
        })
        .collect()
}

impl Store {
    /// Lists every connection, including disabled entries. SQL columns override JSON copies.
    pub fn connections(&self, provider: Option<&str>) -> Result<Vec<Value>> {
        if let Some(provider) = provider {
            bounded(provider, 256, false)?;
        }
        let mut query = self.connection.prepare(&format!(
            "SELECT {CONNECTION_COLUMNS} FROM providerConnections WHERE (?1 IS NULL OR provider=?1) ORDER BY priority IS NULL,priority,createdAt,id"))?;
        Ok(query
            .query_map([provider], connection_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Upserts by id and merges omitted fields atomically; an omitted id creates a new record.
    pub fn save_connection(&mut self, input: &Value) -> Result<Value> {
        let mut patch = object(input)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = record_id(&transaction, &patch)?;
        let existing = transaction
            .query_row(
                &format!("SELECT {CONNECTION_COLUMNS} FROM providerConnections WHERE id=?1"),
                [&id],
                connection_row,
            )
            .optional()?
            .map(|value| object(&value))
            .transpose()?
            .unwrap_or_default();
        if patch.get("testStatus").and_then(Value::as_str) == Some("active") {
            for field in ["lastError", "lastErrorAt"] {
                patch.entry(field).or_insert(Value::Null);
            }
            for field in ["errorCode", "rateLimitedUntil"] {
                patch.insert(field.into(), Value::Null);
            }
            patch.insert("backoffLevel".into(), json!(0));
            for field in existing.keys().filter(|key| key.starts_with("modelLock_")) {
                patch.insert(field.clone(), Value::Null);
            }
        }
        let mut value = merge_record(existing, patch, &id, &now(&transaction)?)?;
        value.entry("authType").or_insert(json!("oauth"));
        value.entry("isActive").or_insert(json!(true));
        text_field(&value, "provider", true, 256)?;
        text_field(&value, "authType", true, 64)?;
        for field in ["name", "email"] {
            text_field(&value, field, false, 1024)?;
        }
        for field in ["apiKey", "accessToken", "refreshToken", "idToken"] {
            text_field(&value, field, false, 256 * 1024)?;
        }
        bool_field(&value, "isActive")?;
        if value
            .get("providerSpecificData")
            .is_some_and(|data| !data.is_null() && !data.is_object())
        {
            return Err(invalid("providerSpecificData must be an object"));
        }
        if !value.contains_key("priority") {
            let maximum: Option<i64> = transaction.query_row(
                "SELECT MAX(priority) FROM providerConnections WHERE provider=?1",
                [value["provider"].as_str()],
                |row| row.get(0),
            )?;
            value.insert(
                "priority".into(),
                json!(
                    maximum
                        .unwrap_or(0)
                        .checked_add(1)
                        .ok_or_else(|| invalid("Priority overflow"))?
                ),
            );
        }
        if let Some(priority) = value.get("priority").filter(|priority| !priority.is_null())
            && priority.as_i64().is_none_or(|number| number < 0)
        {
            return Err(invalid("Priority must be a nonnegative integer or null"));
        }
        let data = extra_data(
            &value,
            &[
                "id",
                "provider",
                "authType",
                "name",
                "email",
                "priority",
                "isActive",
                "createdAt",
                "updatedAt",
            ],
        )?;
        transaction.execute(
            "INSERT INTO providerConnections(id,provider,authType,name,email,priority,isActive,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(id) DO UPDATE SET provider=excluded.provider,authType=excluded.authType,name=excluded.name,email=excluded.email,priority=excluded.priority,isActive=excluded.isActive,data=excluded.data,updatedAt=excluded.updatedAt",
            params![id, value["provider"].as_str(), value["authType"].as_str(), value.get("name").and_then(Value::as_str), value.get("email").and_then(Value::as_str), value["priority"].as_i64(), value["isActive"].as_bool(), data, value["createdAt"].as_str(), value["updatedAt"].as_str()],
        )?;
        let result = transaction.query_row(
            &format!("SELECT {CONNECTION_COLUMNS} FROM providerConnections WHERE id=?1"),
            [&id],
            connection_row,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn delete_connection(&mut self, id: &str) -> Result<bool> {
        bounded(id, 256, false)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute("DELETE FROM providerConnections WHERE id=?1", [id])? > 0;
        transaction.commit()?;
        Ok(deleted)
    }

    pub fn nodes(&self) -> Result<Vec<Value>> {
        let mut query = self.connection.prepare(&format!(
            "SELECT {NODE_COLUMNS} FROM providerNodes ORDER BY rowid"
        ))?;
        Ok(query
            .query_map([], node_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_node(&mut self, input: &Value) -> Result<Value> {
        let patch = object(input)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = record_id(&transaction, &patch)?;
        let existing = transaction
            .query_row(
                &format!("SELECT {NODE_COLUMNS} FROM providerNodes WHERE id=?1"),
                [&id],
                node_row,
            )
            .optional()?
            .map(|value| object(&value))
            .transpose()?
            .unwrap_or_default();
        let value = merge_record(existing, patch, &id, &now(&transaction)?)?;
        for field in ["type", "apiType"] {
            text_field(&value, field, false, 64)?;
        }
        for field in ["name", "prefix"] {
            text_field(&value, field, false, 512)?;
        }
        text_field(&value, "baseUrl", false, 8192)?;
        let data = extra_data(&value, &["id", "type", "name", "createdAt", "updatedAt"])?;
        transaction.execute(
            "INSERT INTO providerNodes(id,type,name,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET type=excluded.type,name=excluded.name,data=excluded.data,updatedAt=excluded.updatedAt",
            params![id, value.get("type").and_then(Value::as_str), value.get("name").and_then(Value::as_str), data, value["createdAt"].as_str(), value["updatedAt"].as_str()],
        )?;
        let result = transaction.query_row(
            &format!("SELECT {NODE_COLUMNS} FROM providerNodes WHERE id=?1"),
            [&id],
            node_row,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn delete_node(&mut self, id: &str) -> Result<bool> {
        bounded(id, 256, false)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute("DELETE FROM providerNodes WHERE id=?1", [id])? > 0;
        if deleted {
            let prefix = format!("{id}|");
            transaction.execute(
                "DELETE FROM kv WHERE scope='customModels' AND substr(key,1,length(?1))=?1",
                [&prefix],
            )?;
        }
        transaction.commit()?;
        Ok(deleted)
    }

    pub fn combos(&self) -> Result<Vec<Value>> {
        let mut query = self.connection.prepare(&format!(
            "SELECT {COMBO_COLUMNS} FROM combos ORDER BY createdAt,id"
        ))?;
        Ok(query
            .query_map([], combo_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn save_combo(&mut self, input: &Value) -> Result<Value> {
        let patch = object(input)?;
        if patch.keys().any(|key| {
            !["id", "name", "kind", "models", "createdAt", "updatedAt"].contains(&key.as_str())
        }) {
            return Err(invalid(
                "Combo contains fields not supported by the upstream schema",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = record_id(&transaction, &patch)?;
        let existing = transaction
            .query_row(
                &format!("SELECT {COMBO_COLUMNS} FROM combos WHERE id=?1"),
                [&id],
                combo_row,
            )
            .optional()?
            .map(|value| object(&value))
            .transpose()?
            .unwrap_or_default();
        let mut value = merge_record(existing, patch, &id, &now(&transaction)?)?;
        text_field(&value, "name", true, 512)?;
        text_field(&value, "kind", false, 64)?;
        value.entry("models").or_insert(json!([]));
        let models = serde_json::to_string(&string_list(&value["models"])?)?;
        transaction.execute(
            "INSERT INTO combos(id,name,kind,models,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET name=excluded.name,kind=excluded.kind,models=excluded.models,updatedAt=excluded.updatedAt",
            params![id, value["name"].as_str(), value.get("kind").and_then(Value::as_str), models, value["createdAt"].as_str(), value["updatedAt"].as_str()],
        )?;
        let result = transaction.query_row(
            &format!("SELECT {COMBO_COLUMNS} FROM combos WHERE id=?1"),
            [&id],
            combo_row,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn delete_combo(&mut self, id: &str) -> Result<bool> {
        bounded(id, 256, false)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute("DELETE FROM combos WHERE id=?1", [id])? > 0;
        transaction.commit()?;
        Ok(deleted)
    }

    pub fn aliases(&self) -> Result<Map<String, Value>> {
        let mut query = self
            .connection
            .prepare("SELECT key,value FROM kv WHERE scope='modelAliases' ORDER BY key")?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, json_column(row, 1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Replaces the alias map in one transaction. Invalid input leaves all aliases untouched.
    pub fn save_aliases(&mut self, aliases: &Map<String, Value>) -> Result<()> {
        object(&Value::Object(aliases.clone()))?;
        for (key, value) in aliases {
            bounded(key, 4096, false)?;
            if let Some(value) = value.as_str() {
                bounded(value, 4096, false)?;
                if value
                    .split_once('/')
                    .is_none_or(|(provider, model)| provider.is_empty() || model.is_empty())
                {
                    return Err(invalid("Alias must target provider/model"));
                }
            } else {
                let value = value
                    .as_object()
                    .ok_or_else(|| invalid("Alias must be a model string or object"))?;
                text_field(value, "provider", true, 256)?;
                text_field(value, "model", true, 4096)?;
            }
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Validate existing JSON before replacing it, including an explicit empty replacement.
        {
            let mut query =
                transaction.prepare("SELECT value FROM kv WHERE scope='modelAliases'")?;
            for value in query.query_map([], |row| json_column(row, 0))? {
                value?;
            }
        }
        transaction.execute("DELETE FROM kv WHERE scope='modelAliases'", [])?;
        for (key, value) in aliases {
            transaction.execute(
                "INSERT INTO kv(scope,key,value) VALUES('modelAliases',?1,?2)",
                params![key, serde_json::to_string(value)?],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn settings(&self) -> Result<Value> {
        let raw = self
            .connection
            .query_row("SELECT data FROM settings WHERE id=1", [], |row| {
                object_column(row, 0)
            })
            .optional()?
            .unwrap_or_default();
        Ok(merge_settings(raw))
    }

    pub fn save_settings(&mut self, input: &Value) -> Result<Value> {
        let patch = object(input)?;
        let defaults = default_settings();
        for (key, value) in &patch {
            if let Some(default) = defaults.get(key) {
                let valid = match default {
                    Value::Bool(_) => value.is_boolean(),
                    Value::String(_) => value.is_string(),
                    Value::Number(_) => value.as_u64().is_some(),
                    Value::Object(_) => value.is_object(),
                    _ => true,
                };
                if !valid {
                    return Err(invalid("Invalid settings field type"));
                }
            }
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut value = transaction
            .query_row("SELECT data FROM settings WHERE id=1", [], |row| {
                object_column(row, 0)
            })
            .optional()?
            .unwrap_or_default();
        value.extend(patch);
        object(&Value::Object(value.clone()))?;
        transaction.execute("INSERT INTO settings(id,data) VALUES(1,?1) ON CONFLICT(id) DO UPDATE SET data=excluded.data", [serde_json::to_string(&value)?])?;
        transaction.commit()?;
        Ok(merge_settings(value))
    }

    pub fn api_keys(&self) -> Result<Vec<Value>> {
        let mut query = self.connection.prepare(&format!(
            "SELECT {KEY_COLUMNS} FROM apiKeys ORDER BY createdAt,id"
        ))?;
        Ok(query
            .query_map([], key_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The caller supplies the key material; this repository never logs or derives credentials.
    pub fn save_api_key(&mut self, input: &Value) -> Result<Value> {
        let patch = object(input)?;
        if patch.keys().any(|key| {
            ![
                "id",
                "key",
                "name",
                "machineId",
                "isActive",
                "createdAt",
                "access",
            ]
            .contains(&key.as_str())
        }) {
            return Err(invalid(
                "API key contains fields not supported by the upstream schema",
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = record_id(&transaction, &patch)?;
        let existing = transaction
            .query_row(
                &format!("SELECT {KEY_COLUMNS} FROM apiKeys WHERE id=?1"),
                [&id],
                key_row,
            )
            .optional()?
            .map(|value| object(&value))
            .transpose()?
            .unwrap_or_default();
        let mut value = merge_record(existing, patch, &id, &now(&transaction)?)?;
        text_field(&value, "key", true, 8192)?;
        for field in ["name", "machineId"] {
            text_field(&value, field, false, 1024)?;
        }
        value.entry("isActive").or_insert(json!(true));
        bool_field(&value, "isActive")?;
        value
            .entry("access")
            .or_insert(json!({"restricted": false, "allow": []}));
        let access = normalized_access(&value["access"])?;
        transaction.execute(
            "INSERT INTO apiKeys(id,key,name,machineId,isActive,createdAt,accessRestricted,accessAllow) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET key=excluded.key,name=excluded.name,machineId=excluded.machineId,isActive=excluded.isActive,accessRestricted=excluded.accessRestricted,accessAllow=excluded.accessAllow",
            params![id, value["key"].as_str(), value.get("name").and_then(Value::as_str), value.get("machineId").and_then(Value::as_str), value["isActive"].as_bool(), value["createdAt"].as_str(), access["restricted"].as_bool(), serde_json::to_string(&access["allow"])?],
        )?;
        let result = transaction.query_row(
            &format!("SELECT {KEY_COLUMNS} FROM apiKeys WHERE id=?1"),
            [&id],
            key_row,
        )?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn delete_api_key(&mut self, id: &str) -> Result<bool> {
        bounded(id, 256, false)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = transaction.execute("DELETE FROM apiKeys WHERE id=?1", [id])? > 0;
        transaction.commit()?;
        Ok(deleted)
    }

    pub fn combo_models(&self, name: &str) -> Result<Option<Vec<String>>> {
        bounded(name, 4096, true)?;
        if name.contains('/') {
            return Ok(None);
        }
        let models = self
            .connection
            .query_row("SELECT models FROM combos WHERE name=?1", [name], |row| {
                json_column(row, 0)
            })
            .optional()?;
        let models = models.as_ref().map(string_list).transpose()?;
        Ok(models.filter(|models| !models.is_empty()))
    }

    /// App precedence: reserved provider prefixes, custom nodes, combos, aliases, inference.
    pub fn resolve_app_model(&self, catalog: &Catalog, input: &str) -> Result<model::ModelInfo> {
        bounded(input, 4096, true)?;
        let parsed = model::parse(catalog, Some(input));
        if !parsed.is_alias {
            let prefix = parsed.provider_alias.as_deref().unwrap_or("");
            if prefix == "xmtp" || prefix == "xiaomi-tokenplan" {
                return Ok(model::ModelInfo {
                    provider: Some("xiaomi-tokenplan".into()),
                    model: parsed.model,
                });
            }
            let reserved = catalog.provider_aliases.contains_key(prefix)
                || catalog
                    .providers
                    .iter()
                    .any(|provider| provider.id == prefix || provider.alias == prefix);
            if !prefix.is_empty() && !reserved {
                let nodes = self.nodes()?;
                for kind in [
                    "openai-compatible",
                    "anthropic-compatible",
                    "custom-embedding",
                ] {
                    if let Some(node) = nodes
                        .iter()
                        .find(|node| node["type"] == kind && node["prefix"] == prefix)
                    {
                        return Ok(model::ModelInfo {
                            provider: Some(
                                node["id"]
                                    .as_str()
                                    .ok_or_else(|| invalid("Stored node id is invalid"))?
                                    .into(),
                            ),
                            model: parsed.model,
                        });
                    }
                }
            }
            return Ok(model::ModelInfo {
                provider: parsed.provider,
                model: parsed.model,
            });
        }
        let combo = self
            .connection
            .query_row("SELECT 1 FROM combos WHERE name=?1", [input], |_| Ok(()))
            .optional()?
            .is_some();
        if combo {
            return Ok(model::ModelInfo {
                provider: None,
                model: parsed.model,
            });
        }
        Ok(model::resolve(catalog, Some(input), &self.aliases()?))
    }
}

fn default_settings() -> Map<String, Value> {
    let defaults: Value = serde_json::from_str(r#"{
        "cloudEnabled": false, "tunnelEnabled": false, "tunnelUrl": "", "tunnelProvider": "cloudflare",
        "tailscaleEnabled": false, "tailscaleUrl": "", "stickyRoundRobinLimit": 3,
        "providerStrategies": {}, "quotaVisibility": {}, "comboStrategy": "fallback", "comboStickyRoundRobinLimit": 1, "comboStrategies": {},
        "capacityAdapter": {"vision": {"enabled": true, "roundRobin": false, "models": []}, "pdf": {"enabled": false, "roundRobin": false, "models": []},
            "audioInput": {"enabled": true, "roundRobin": false, "models": []}, "videoInput": {"enabled": false, "roundRobin": false, "models": []}},
        "requireLogin": true, "requireApiKey": true, "tunnelDashboardAccess": true, "authMode": "password", "ssoType": "oidc",
        "oidcIssuerUrl": "", "oidcClientId": "", "oidcClientSecret": "", "oidcScopes": "openid profile email", "oidcLoginLabel": "Sign in with OIDC",
        "samlEntryPoint": "", "samlIssuer": "urn:9router:sp", "samlCert": "", "samlLoginLabel": "Sign in with SAML SSO", "samlAttributeEmail": "email", "samlAttributeName": "name",
        "enableObservability": false, "observabilityMaxRecords": 1000, "observabilityBatchSize": 20, "observabilityFlushIntervalMs": 5000, "observabilityMaxJsonSize": 5,
        "outboundProxyEnabled": false, "outboundProxyUrl": "", "outboundNoProxy": "", "mitmRouterBaseUrl": "http://localhost:20128", "dnsToolEnabled": {}, "rtkEnabled": true,
        "headroomEnabled": false, "headroomUrl": "http://localhost:8787", "headroomCompressUserMessages": false, "headroomTimeoutMs": 3000,
        "cavemanEnabled": false, "cavemanLevel": "full", "ponytailEnabled": false, "ponytailLevel": "full", "customPromptEnabled": false, "customPrompt": "",
        "pxpipeEnabled": false, "pxpipeAutoInstall": true, "pxpipeMinChars": 25000, "pxpipeTimeoutMs": 15000, "providerOverrides": {}
    }"#).expect("literal settings JSON");
    let mut defaults = defaults
        .as_object()
        .expect("literal settings object")
        .clone();
    if let Ok(url) = std::env::var("HEADROOM_URL") {
        defaults.insert("headroomUrl".into(), url.into());
    }
    defaults
}

fn merge_settings(raw: Map<String, Value>) -> Value {
    let mut merged = default_settings();
    merged.extend(raw);
    if let Some(capacities) = merged
        .get_mut("capacityAdapter")
        .and_then(Value::as_object_mut)
    {
        for capability in capacities.values_mut() {
            if let Some(models) = capability.get_mut("models").and_then(Value::as_array_mut) {
                for model in models {
                    if model == "oc/mimo-v2.5-free" {
                        *model = json!("oc/mimo-v2.6-flash-free");
                    }
                }
            }
        }
    }
    Value::Object(merged)
}
