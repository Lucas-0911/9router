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
        .invoke_handler(tauri::generate_handler![backend_config])
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
