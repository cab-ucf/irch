//! irch on the desktop and on Android: the page in `ui/` over one command.

#[cfg(target_os = "android")]
mod android;

use irch_core::{Config, File, Id, Net, Node, api};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{broadcast::error::RecvError, watch};

type Started = Option<Result<Arc<Node>, String>>;

/// The node once it started, or why it could not.
struct App(watch::Sender<Started>);

async fn node(s: &App) -> Result<Arc<Node>, String> {
    let mut rx = s.0.subscribe();
    let v = rx.wait_for(Option::is_some).await.map_err(|e| e.to_string())?;
    v.clone().expect("checked")
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

#[tauri::command]
async fn irch(s: State<'_, App>, call: api::Call) -> Result<Value, String> {
    api::call(&node(&s).await?, call).await.map_err(err)
}

/// Become another device of the person behind `code`. A fresh
/// install only: this device's own identity is thrown away.
#[tauri::command]
async fn link(
    app: AppHandle,
    s: State<'_, App>,
    code: String,
) -> Result<(), String> {
    let n = node(&s).await?;
    if !n.spaces().is_empty() {
        return Err(
            "leave your spaces first: this device becomes someone else".into(),
        );
    }
    n.shutdown().await;
    s.0.send_replace(None);
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    for f in ["seed", "local", "spaces"] {
        let p = dir.join(f);
        let _ =
            std::fs::remove_file(&p).or_else(|_| std::fs::remove_dir_all(&p));
    }
    start(&app, Some(code)).await
}

/// Save an attachment: into Downloads, or on Android to the share sheet.
#[tauri::command]
async fn save(
    app: AppHandle,
    s: State<'_, App>,
    space: Id,
    file: File,
) -> Result<String, String> {
    let (n, p) = (node(&s).await?, app.path());
    #[cfg(target_os = "android")]
    let dir = p.app_cache_dir().map(|d| d.join("shared"));
    #[cfg(not(target_os = "android"))]
    let dir = p.download_dir().or_else(|_| p.home_dir());
    let dir = dir.map_err(|e| e.to_string())?;
    let f = n.save(&space, &file, &dir).await.map_err(err)?;
    #[cfg(target_os = "android")]
    if let Some(b) = app.try_state::<android::Bridge>() {
        b.share_file(&f).await;
    }
    Ok(f.display().to_string())
}

/// A QR code, as SVG, for codes shown to a camera.
#[tauri::command]
fn qr(text: String) -> Result<String, String> {
    let q = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    let mut svg = q.render::<qrcode::render::svg::Color>();
    Ok(svg.quiet_zone(true).min_dimensions(240, 240).build())
}

/// Android: the share sheet.
#[tauri::command]
async fn share(app: AppHandle, text: String) -> Result<(), String> {
    #[cfg(target_os = "android")]
    if let Some(b) = app.try_state::<android::Bridge>() {
        return Ok(b.share(&text).await);
    }
    let _ = (app, text);
    Err("no share sheet here".into())
}

/// Android: keep running in the background (desktops always do).
#[tauri::command]
async fn background(app: AppHandle, on: bool) {
    #[cfg(target_os = "android")]
    if let Some(b) = app.try_state::<android::Bridge>() {
        b.background(on).await;
    }
    let _ = (app, on);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let b = tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init());
    #[cfg(target_os = "android")]
    let b = b.plugin(android::init());
    let b = b.setup(|app| {
        app.manage(App(watch::channel(None).0));
        let h = app.handle().clone();
        tauri::async_runtime::spawn(async move { start(&h, None).await });
        Ok(())
    });
    let b = b.invoke_handler(tauri::generate_handler![
        irch, link, qr, save, share, background
    ]);
    let app = b.build(tauri::generate_context!()).expect("tauri builds");
    app.run(|app, e| {
        let n = || app.state::<App>().0.borrow().clone().and_then(Result::ok);
        match e {
            // Back in front: connections may have died while frozen.
            #[cfg(mobile)]
            tauri::RunEvent::Resumed => {
                if let Some(n) = n() {
                    tauri::async_runtime::spawn(
                        async move { n.resume().await },
                    );
                }
            }
            // Leave cleanly, so peers see us go now, not at a timeout.
            tauri::RunEvent::Exit => {
                if let Some(n) = n() {
                    let bye = tokio::time::timeout(
                        Duration::from_secs(2),
                        n.shutdown(),
                    );
                    let _ = tauri::async_runtime::block_on(bye);
                }
            }
            _ => {}
        }
    });
}

/// Start the node off the UI thread; failures reach the page as text.
async fn start(app: &AppHandle, link: Option<String>) -> Result<(), String> {
    let r = async {
        let dir = app.path().app_data_dir()?;
        log_to(&dir);
        #[cfg(target_os = "android")]
        android::context().await;
        let net =
            match (std::env::var("IRCH_RELAY"), std::env::var_os("IRCH_LAN")) {
                (Ok(r), _) => Net::Relay(
                    r.split(',').map(str::parse).collect::<Result<_, _>>()?,
                ),
                (_, Some(_)) => Net::Lan,
                _ => Net::Default,
            };
        let cfg = Config { dir, name: "anon".into(), net, link, serve: false };
        tokio::spawn(Node::start(cfg)).await?
    };
    let r = r.await.map_err(err);
    if let Ok(n) = &r {
        forward(app.clone(), n.clone());
        #[cfg(target_os = "android")]
        if let Some(b) = app.try_state::<android::Bridge>() {
            b.watch_network(n.clone()).await;
        }
    }
    let done = r.as_ref().map(drop).map_err(Clone::clone);
    app.state::<App>().0.send_replace(Some(r));
    done
}

/// Core events to the page; a lagging receiver means "refresh everything".
fn forward(app: AppHandle, n: Arc<Node>) {
    let mut rx = n.subscribe();
    tauri::async_runtime::spawn(async move {
        #[cfg(target_os = "android")]
        let mut seen = std::collections::HashMap::new();
        loop {
            let e = match rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => {
                    let _ = app.emit("irch", json!({ "t": "changed" }));
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            #[cfg(target_os = "android")]
            if let irch_core::Event::Changed { space } = &e {
                android::mentions(&app, &n, *space, &mut seen).await;
            }
            let _ = app.emit("irch", e);
        }
    });
}

/// Logs stay in the app's private directory, never the system log.
fn log_to(dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(dir);
    let Ok(f) = std::fs::File::create(dir.join("irch.log")) else { return };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "warn,n0_mainline=off".into());
    let log =
        tracing_subscriber::fmt().with_ansi(false).with_env_filter(filter);
    let _ = log.with_writer(std::sync::Mutex::new(f)).try_init();
}
