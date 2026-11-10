#![cfg_attr(
    all(feature = "desktop-app", target_os = "windows"),
    windows_subsystem = "windows"
)]
use nine_router_core::{
    Result, Store,
    http::{self, Gateway},
};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

fn data_directory() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NINE_ROUTER_DATA_DIR") {
        return Ok(path.into());
    }
    #[cfg(target_os = "windows")]
    let root = std::env::var_os("LOCALAPPDATA");
    #[cfg(not(target_os = "windows"))]
    let root = std::env::var_os("HOME");
    Ok(PathBuf::from(
        root.ok_or_else(|| io::Error::other("Set NINE_ROUTER_DATA_DIR to a writable directory"))?,
    )
    .join(".9router-rust"))
}

fn admin_token(directory: &Path) -> Result<String> {
    if let Ok(token) = std::env::var("NINE_ROUTER_ADMIN_TOKEN") {
        return Ok(token);
    }
    let path = directory.join("admin-token");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            Ok(token)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(io::Error::other("Admin token must be a private regular file").into());
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if fs::metadata(&path)?.permissions().mode() & 0o077 != 0 {
                    return Err(io::Error::other("Admin token file must have mode 0600").into());
                }
            }
            let mut token = String::new();
            fs::File::open(&path)?
                .take(8192)
                .read_to_string(&mut token)?;
            Ok(token.trim().to_owned())
        }
        Err(error) => Err(error.into()),
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("9Router: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let mut directory = data_directory()?;
    let mut listen: SocketAddr = "127.0.0.1:20128".parse()?;
    let mut headless = !cfg!(feature = "desktop-app");
    let mut import = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => directory = args.next().ok_or("--data-dir needs a path")?.into(),
            "--listen" => listen = args.next().ok_or("--listen needs IP:port")?.parse()?,
            "--headless" => headless = true,
            "--import-db" => {
                import = Some(PathBuf::from(
                    args.next()
                        .ok_or("--import-db needs a source database path")?,
                ))
            }
            "--help" | "-h" => {
                println!(
                    "9Router Rust {}\n  --data-dir PATH     Separate Rust data directory\n  --listen IP:PORT    Loopback address (default 127.0.0.1:20128)\n  --headless          Run API without desktop window\n  --import-db PATH    Explicitly import into a NEW database, then exit\n\nAdmin credential: NINE_ROUTER_ADMIN_TOKEN or DATA_DIR/admin-token",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            _ => return Err(format!("Unknown option: {arg}").into()),
        }
    }
    if !listen.ip().is_loopback() {
        return Err(
            "This Rust preview binds only to loopback; remote dashboard security is not yet ported"
                .into(),
        );
    }
    let database = directory.join("db/data.sqlite");
    if let Some(source) = import {
        Store::import(&source, &database)?;
        println!("Imported a consistent copy into {}", database.display());
        return Ok(());
    }
    let store = if database.exists() {
        Store::open(&database)?
    } else {
        Store::create(&database)?
    };
    let token = admin_token(&directory)?;
    let state = Gateway::new_in(store, token.clone(), directory.clone())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let listener = runtime.block_on(tokio::net::TcpListener::bind(listen))?;
    let url = format!("http://{}", listener.local_addr()?);
    println!("9Router Rust {} — {url}/v1", env!("CARGO_PKG_VERSION"));
    println!("Data: {}", directory.display());
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let workers = state.clone();
    let server = runtime.spawn(async move {
        axum::serve(listener, http::router(state))
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let result: Result<()> = if headless {
        runtime
            .block_on(tokio::signal::ctrl_c())
            .map_err(Into::into)
    } else {
        #[cfg(feature = "desktop-app")]
        {
            nine_router_core::desktop::run(url, token)
        }
        #[cfg(not(feature = "desktop-app"))]
        {
            Err("Rebuild with --features desktop-app for the desktop window".into())
        }
    };
    let _ = stop.send(());
    runtime.block_on(workers.shutdown());
    runtime.block_on(async {
        let mut server = server;
        match tokio::time::timeout(Duration::from_secs(3), &mut server).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(Ok(Err(error))) => Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
            Ok(Err(error)) => Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>),
            Err(_) => {
                server.abort();
                let _ = server.await;
                Ok(())
            }
        }
    })?;
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
