//! Native persistence, routing, protocols and desktop runtime for the 9Router migration.
//! Provider metadata is a reference, not a claim that its adapter has been ported.
pub mod codex;
#[cfg(feature = "desktop-app")]
pub mod desktop;
pub mod entities;
pub mod http;
pub mod model;
mod muse;
pub mod oauth;
pub mod protocol;
pub mod web_login;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension,
    backup::{Backup, StepResult},
    params,
};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    error::Error,
    fs::{DirBuilder, OpenOptions},
    io,
    path::Path,
    time::{Duration, Instant},
};

pub type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Deserialize, serde::Serialize)]
pub struct Provider {
    pub id: String,
    pub alias: String,
    pub name: String,
    pub category: String,
    pub models: Vec<Value>,
    #[serde(rename = "serviceKinds")]
    pub service_kinds: Vec<String>,
    pub format: Option<String>,
    #[serde(default)]
    pub transport: Option<Value>,
}

#[derive(Deserialize)]
pub struct Catalog {
    pub revision: String,
    pub providers: Vec<Provider>,
    pub tables: Vec<String>,
    pub columns: BTreeMap<String, Vec<String>>,
    #[serde(rename = "providerAliases")]
    pub provider_aliases: BTreeMap<String, String>,
    pub routes: Vec<Value>,
    pub pages: Vec<String>,
}

impl Catalog {
    pub fn load() -> Result<Self> {
        Ok(serde_json::from_str(include_str!(
            "../upstream-contract.json"
        ))?)
    }

    pub fn provider(&self, id: &str) -> Option<&Provider> {
        self.providers.iter().find(|provider| provider.id == id)
    }
}

pub struct Store {
    connection: Connection,
}

impl Store {
    fn prepare_parent(path: &Path) -> Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            let mut directories = DirBuilder::new();
            directories.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directories.mode(0o700);
            }
            directories.create(parent)?;
        }
        Ok(())
    }

    /// Reserve a new private file atomically. Existing files are never replaced.
    pub fn create(path: &Path) -> Result<Self> {
        Self::prepare_parent(path)?;
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)?;
        let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let transaction = connection.transaction()?;
        transaction.execute_batch(include_str!("../upstream-schema.sql"))?;
        transaction.commit()?;
        Ok(Self { connection })
    }

    /// Opening a database never creates missing tables or runs an implicit migration.
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        let store = Self { connection };
        store.validate_schema()?;
        Ok(store)
    }

    /// Explicit, consistent SQLite backup. The source is opened read-only, including WAL data.
    pub fn import(source: &Path, destination: &Path) -> Result<Self> {
        let source_connection =
            Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let source_store = Self {
            connection: source_connection,
        };
        source_store.validate_schema()?;
        Self::prepare_parent(destination)?;
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temporary = tempfile::NamedTempFile::new_in(parent)?;
        let mut destination_store = Self {
            connection: Connection::open_with_flags(
                temporary.path(),
                OpenFlags::SQLITE_OPEN_READ_WRITE,
            )?,
        };
        {
            let backup = Backup::new(&source_store.connection, &mut destination_store.connection)?;
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Database import timed out; source is unchanged",
                    )
                    .into());
                }
                match backup.step(256)? {
                    StepResult::Done => break,
                    StepResult::Busy | StepResult::Locked => {
                        std::thread::sleep(Duration::from_millis(10))
                    }
                    _ => {}
                }
            }
        }
        destination_store.validate_schema()?;
        destination_store
            .connection
            .close()
            .map_err(|(_, error)| error)?;
        temporary
            .persist_noclobber(destination)
            .map_err(|error| error.error)?;
        Self::open(destination)
    }

    pub fn table_names(&self) -> Result<Vec<String>> {
        let mut query = self.connection.prepare(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        Ok(query
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    fn validate_schema(&self) -> Result<()> {
        let names = self.table_names()?;
        if Catalog::load()?
            .tables
            .iter()
            .any(|name| !names.contains(name))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Database is missing upstream 9Router tables",
            )
            .into());
        }
        for (table, expected) in Catalog::load()?.columns {
            let mut query = self
                .connection
                .prepare("SELECT name FROM pragma_table_info(?1)")?;
            let actual = query
                .query_map([&table], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if expected.iter().any(|column| !actual.contains(column)) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Database table {table} is missing upstream columns"),
                )
                .into());
            }
        }
        let integrity: String = self
            .connection
            .query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        if integrity != "ok" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Database integrity check failed",
            )
            .into());
        }
        Ok(())
    }

    pub fn get(&self, scope: &str, key: &str) -> Result<Option<Value>> {
        let raw: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM kv WHERE scope=?1 AND key=?2",
                params![scope, key],
                |row| row.get(0),
            )
            .optional()?;
        // Corrupt JSON is reported and preserved, never replaced with an empty default.
        Ok(raw.map(|text| serde_json::from_str(&text)).transpose()?)
    }

    pub fn scope(&self, scope: &str) -> Result<BTreeMap<String, Value>> {
        let mut query = self
            .connection
            .prepare("SELECT key,value FROM kv WHERE scope=?1 ORDER BY key")?;
        let rows = query.query_map([scope], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.map(|row| {
            let (key, raw) = row?;
            Ok((key, serde_json::from_str(&raw)?))
        })
        .collect()
    }

    pub fn remove(&mut self, scope: &str, key: &str) -> Result<()> {
        self.connection.execute(
            "DELETE FROM kv WHERE scope=?1 AND key=?2",
            params![scope, key],
        )?;
        Ok(())
    }

    pub fn put_batch(&mut self, values: &[(&str, &str, &Value)]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        for (scope, key, value) in values {
            if scope.is_empty()
                || key.is_empty()
                || scope.len() > 256
                || key.len() > 4096
                || scope.contains('\0')
                || key.contains('\0')
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Invalid storage scope or key",
                )
                .into());
            }
            transaction.execute(
                "INSERT INTO kv(scope,key,value) VALUES(?1,?2,?3) ON CONFLICT(scope,key) DO UPDATE SET value=excluded.value",
                params![scope, key, serde_json::to_string(value)?],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }
}
