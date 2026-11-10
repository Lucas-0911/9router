//! The webview receives its loopback credentials through local IPC, never a URL.
use serde::Serialize;
use tauri::{State, Url, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct BackendConfig {
    backend_url: String,
    admin_token: String,
}

fn is_app_url(url: &Url) -> bool {
    (url.scheme() == "tauri" && url.host_str() == Some("localhost"))
        || (matches!(url.scheme(), "http" | "https") && url.host_str() == Some("tauri.localhost"))
}

#[tauri::command]
fn backend_config(
    window: WebviewWindow,
    state: State<'_, BackendConfig>,
) -> std::result::Result<BackendConfig, &'static str> {
    if window.label() != "main" || !window.url().is_ok_and(|url| is_app_url(&url)) {
        return Err("Only the local application window may access backend credentials");
    }
    Ok(state.inner().clone())
}

#[tauri::command]
async fn open_external(window: WebviewWindow, url: String) -> std::result::Result<(), String> {
    if window.label() != "main" || !window.url().is_ok_and(|u| is_app_url(&u)) {
        return Err("Only the local application may open a login page".into());
    }
    let parsed = Url::parse(&url).map_err(|_| "Invalid browser URL")?;
    if parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || url.len() > 16384
    {
        return Err("Browser links must use HTTPS without embedded credentials".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        let mut command = std::process::Command::new("/usr/bin/open");
        #[cfg(target_os = "windows")]
        let mut command = {
            let mut c = std::process::Command::new("rundll32");
            c.arg("url.dll,FileProtocolHandler");
            c
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        let mut command = std::process::Command::new("xdg-open");
        command
            .arg(parsed.as_str())
            .status()
            .map_err(|_| "Cannot open the default browser".to_owned())?
            .success()
            .then_some(())
            .ok_or_else(|| "The default browser could not open this page".to_owned())
    })
    .await
    .map_err(|_| "Browser launch failed".to_owned())?
}

/// Call on the main thread after starting the backend on a background runtime.
pub fn run(backend_url: String, admin_token: String) -> crate::Result<()> {
    let url = Url::parse(&backend_url)?;
    if url.scheme() != "http"
        || !matches!(url.host_str(), Some("127.0.0.1" | "[::1]"))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
        || admin_token.is_empty()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Desktop backend must use a loopback HTTP origin and an admin token",
        )
        .into());
    }
    tauri::Builder::default()
        .manage(BackendConfig {
            backend_url: backend_url.trim_end_matches('/').to_owned(),
            admin_token,
        })
        .invoke_handler(tauri::generate_handler![backend_config, open_external])
        .setup(|app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("9Router")
                .inner_size(1180.0, 780.0)
                .min_inner_size(780.0, 560.0)
                .on_navigation(is_app_url)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())?;
    Ok(())
}
