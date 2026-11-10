use nine_router_core::{
    Store,
    web_login::{PROVIDERS, WebLoginService},
};
use std::{
    fs,
    sync::{Arc, Mutex},
    time::Duration,
};

#[tokio::test]
async fn rejects_unknown_providers_and_unsafe_profile_directories() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(
        Store::create(&dir.path().join("router.db")).unwrap(),
    ));
    let login = WebLoginService::new(store, dir.path().join("data"));
    assert!(PROVIDERS.contains(&"muse-web"));
    assert!(login.start("unknown", "default").await.is_err());
    assert!(login.start("muse-web", "bad\0profile").await.is_err());
    assert!(login.status("muse-web").is_none());
    assert!(!login.cancel("muse-web"));
    #[cfg(unix)]
    {
        let profiles = dir.path().join("data/browser-profiles");
        fs::create_dir_all(&profiles).unwrap();
        std::os::unix::fs::symlink(dir.path(), profiles.join("escape")).unwrap();
        assert!(login.start("muse-web", "escape").await.is_err());
        assert!(login.profiles().is_empty());
        fs::remove_file(profiles.join("escape")).unwrap();
        fs::remove_dir(&profiles).unwrap();
        std::os::unix::fs::symlink(dir.path(), &profiles).unwrap();
        assert!(login.start("muse-web", "default").await.is_err());
    }
}

#[tokio::test]
#[ignore = "Opens installed headed Chrome in a fresh app-owned temporary profile"]
async fn real_chrome_opens_dedicated_login_profile_and_cancels() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Mutex::new(
        Store::create(&dir.path().join("router.db")).unwrap(),
    ));
    let login = WebLoginService::new(Arc::clone(&store), dir.path().join("data"));
    login.start("muse-web", "runtime-smoke").await.unwrap();
    let endpoint = dir
        .path()
        .join("data/browser-profiles/runtime-smoke/DevToolsActivePort");
    let launched = tokio::time::timeout(Duration::from_secs(25), async {
        while !endpoint.exists() {
            let status = login.status("muse-web").unwrap();
            assert_eq!(status["status"], "running", "{}", status["error"]);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(login.cancel("muse-web"));
    tokio::time::timeout(Duration::from_secs(5), login.shutdown())
        .await
        .unwrap();
    launched.unwrap();
    assert_eq!(login.status("muse-web").unwrap()["status"], "cancelled");
    assert!(store.lock().unwrap().connections(None).unwrap().is_empty());
    println!(
        "Observed real headed Chrome debugging endpoint in fresh app-owned profile; cancelled without authenticating or saving credentials"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn cancellation_closes_owned_child_releases_profile_and_never_saves_credentials() {
    use std::{
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
    };
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("fixture-browser");
    fs::write(&executable, "#!/bin/sh\nfor arg do\ncase \"$arg\" in --user-data-dir=*) profile=\"${arg#--user-data-dir=}\";; esac\ndone\nprintf '%s' \"$$\" > \"$profile/owned-child.pid\"\nexec /bin/sleep 60\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let store = Arc::new(Mutex::new(
        Store::create(&dir.path().join("router.db")).unwrap(),
    ));
    let login =
        WebLoginService::using_browser(Arc::clone(&store), dir.path().join("data"), executable);
    let result = login.start("muse-web", "My account").await.unwrap();
    assert_eq!(result["status"], "running");
    assert_eq!(result["profile"], "My-account");
    assert!(login.start("grok-web", "My account").await.is_err());
    let marker = dir
        .path()
        .join("data/browser-profiles/My-account/owned-child.pid");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = fs::read_to_string(marker).unwrap();
    assert!(login.cancel("muse-web"));
    tokio::time::timeout(Duration::from_secs(3), login.shutdown())
        .await
        .unwrap();
    assert_eq!(login.status("muse-web").unwrap()["status"], "cancelled");
    assert!(
        !Command::new("/bin/kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    assert!(store.lock().unwrap().connections(None).unwrap().is_empty());
    assert_eq!(login.profiles(), vec!["My-account"]);
    assert!(login.start("grok-web", "My account").await.is_ok());
    login.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn dropping_last_service_handle_terminates_owned_browser() {
    use std::{
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
    };
    let dir = tempfile::tempdir().unwrap();
    let executable = dir.path().join("fixture-browser");
    fs::write(&executable, "#!/bin/sh\nfor arg do\ncase \"$arg\" in --user-data-dir=*) profile=\"${arg#--user-data-dir=}\";; esac\ndone\nprintf '%s' \"$$\" > \"$profile/owned-child.pid\"\nexec /bin/sleep 60\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let store = Arc::new(Mutex::new(
        Store::create(&dir.path().join("router.db")).unwrap(),
    ));
    let login = WebLoginService::using_browser(store, dir.path().join("data"), executable);
    login.start("muse-web", "default").await.unwrap();
    let marker = dir
        .path()
        .join("data/browser-profiles/default/owned-child.pid");
    tokio::time::timeout(Duration::from_secs(3), async {
        while !marker.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = fs::read_to_string(marker).unwrap();
    drop(login);
    tokio::time::timeout(Duration::from_secs(3), async {
        while Command::new("/bin/kill")
            .args(["-0", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
