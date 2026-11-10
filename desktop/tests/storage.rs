use nine_router_core::{Catalog, Store};
use rusqlite::{Connection, params};
use serde_json::json;
use std::fs;

#[test]
fn schema_and_catalog_match_the_complete_upstream_snapshot() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("db/data.sqlite");
    let store = Store::create(&path).unwrap();
    let catalog = Catalog::load().unwrap();
    assert_eq!(catalog.providers.len(), 143);
    assert_eq!(catalog.routes.len(), 175);
    assert_eq!(catalog.pages.len(), 28);
    assert_eq!(store.table_names().unwrap(), catalog.tables);
    let openai = catalog.provider("openai").unwrap();
    assert!(openai.models.iter().any(|model| model["id"] == "gpt-5.5"));
    assert_eq!(catalog.provider("muse-web").unwrap().category, "webCookie");
    assert!(
        catalog.provider("aws-polly").unwrap().format.is_none(),
        "Media-only providers do not have a chat transport"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o077, 0);
    }
}

#[test]
fn creation_and_import_never_overwrite_an_existing_file() {
    let temporary = tempfile::tempdir().unwrap();
    let existing = temporary.path().join("keep.sqlite");
    fs::write(&existing, b"precious data").unwrap();
    assert!(Store::create(&existing).is_err());
    let source = temporary.path().join("source.sqlite");
    let _source_store = Store::create(&source).unwrap();
    assert!(Store::import(&source, &existing).is_err());
    assert_eq!(fs::read(existing).unwrap(), b"precious data");
}

#[test]
fn incompatible_columns_are_rejected_before_import_creates_a_destination() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("incompatible.sqlite");
    let _store = Store::create(&source).unwrap();
    Connection::open(&source)
        .unwrap()
        .execute("ALTER TABLE kv RENAME COLUMN value TO incompatible", [])
        .unwrap();
    let destination = temporary.path().join("must-not-exist.sqlite");
    assert!(Store::import(&source, &destination).is_err());
    assert!(!destination.exists());
}

#[test]
fn a_failed_batch_rolls_back_and_corrupt_json_is_preserved() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("data.sqlite");
    let mut store = Store::create(&path).unwrap();
    let first = json!("openai/gpt-5.5");
    store
        .put_batch(&[("modelAliases", "coding", &first)])
        .unwrap();
    assert!(
        store
            .put_batch(&[
                ("modelAliases", "coding", &json!("changed")),
                ("", "invalid", &json!(true))
            ])
            .is_err()
    );
    assert_eq!(store.get("modelAliases", "coding").unwrap(), Some(first));
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO kv(scope,key,value) VALUES('settings','broken','{broken')",
            [],
        )
        .unwrap();
    assert!(store.get("settings", "broken").is_err());
    let original: String = Connection::open(path)
        .unwrap()
        .query_row(
            "SELECT value FROM kv WHERE scope='settings' AND key='broken'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(original, "{broken");
}

#[test]
fn explicit_import_copies_live_wal_data_without_modifying_the_source() {
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("source.sqlite");
    let _source_store = Store::create(&source).unwrap();
    let connection = Connection::open(&source).unwrap();
    connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    let secret = json!({"apiKey": "fixture-only-secret", "providerSpecificData": {"nested": true}});
    connection.execute(
        "INSERT INTO providerConnections(id,provider,authType,data,createdAt,updatedAt) VALUES(?1,?2,?3,?4,?5,?5)",
        params!["fixture", "openai", "apikey", secret.to_string(), "2026-11-10T00:00:00Z"],
    ).unwrap();
    let original = fs::read(&source).unwrap();
    let wal_path = source.with_extension("sqlite-wal");
    let original_wal = fs::read(&wal_path).unwrap();
    let destination = temporary.path().join("imported.sqlite");
    let _imported = Store::import(&source, &destination).unwrap();
    let value: String = Connection::open(destination)
        .unwrap()
        .query_row(
            "SELECT data FROM providerConnections WHERE id='fixture'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&value).unwrap(),
        secret
    );
    assert_eq!(fs::read(source).unwrap(), original);
    assert_eq!(fs::read(wal_path).unwrap(), original_wal);
}

#[cfg(unix)]
#[test]
fn a_failed_backup_never_publishes_a_partial_database() {
    if let (Ok(source), Ok(destination)) = (
        std::env::var("NINE_ROUTER_TEST_IMPORT_SOURCE"),
        std::env::var("NINE_ROUTER_TEST_IMPORT_DESTINATION"),
    ) {
        assert!(Store::import(source.as_ref(), destination.as_ref()).is_err());
        assert!(!std::path::Path::new(&destination).exists());
        return;
    }
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().join("large.sqlite");
    let destination = temporary.path().join("must-not-exist.sqlite");
    let mut store = Store::create(&source).unwrap();
    store
        .put_batch(&[("fixture", "large", &json!("x".repeat(1024 * 1024)))])
        .unwrap();
    drop(store);
    let outcome = std::process::Command::new("/bin/sh")
        .args([
            "-c",
            "ulimit -c 0; trap '' XFSZ; ulimit -f 256; exec \"$@\"",
            "sh",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_failed_backup_never_publishes_a_partial_database",
            "--nocapture",
        ])
        .env("NINE_ROUTER_TEST_IMPORT_SOURCE", &source)
        .env("NINE_ROUTER_TEST_IMPORT_DESTINATION", &destination)
        .output()
        .unwrap();
    assert!(
        outcome.status.success(),
        "{}",
        String::from_utf8_lossy(&outcome.stdout)
    );
    assert!(!destination.exists());
}
