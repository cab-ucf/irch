//! irch on the desktop and on Android: the page in `ui/` over one command.

#[cfg(target_os = "android")]
mod android;

use anyhow::Context;
use irch_core::{Ch, Config, File, Id, Net, Node, Notice, api};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tauri::ipc::{Channel, InvokeBody, InvokeResponseBody as Body, Request};
use tauri::webview::{PermissionKind as Kind, PermissionResponse};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{broadcast::error::RecvError, watch};

type Started = Option<Result<Arc<Node>, String>>;

/// The node, whoever started it: the app, or a push wake (android.rs).
static NODE: std::sync::Mutex<Option<Arc<Node>>> = std::sync::Mutex::new(None);
/// irch is in sight (phones; desktops always are).
#[cfg(target_os = "android")]
static FG: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// The room on screen: its messages need no notification while in sight.
static VIEW: std::sync::Mutex<Option<(Id, Ch)>> = std::sync::Mutex::new(None);

/// Android, with no push app to wake it: a service keeps it running.
#[cfg(target_os = "android")]
static LIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Android: in sight, the node runs; out of it for a minute, it stops
/// (no keepalives: they wake the radio every few seconds). Pushes wake it.
#[cfg(target_os = "android")]
async fn seen(app: &AppHandle, fg: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    // Only a change counts: "in sight" at launch, the app is starting.
    if FG.swap(fg, SeqCst) == fg {
        return;
    }
    if fg {
        let shown = app.state::<App>().0.borrow().clone();
        match (running(), shown.and_then(Result::ok)) {
            (Some(n), Some(m)) if Arc::ptr_eq(&n, &m) => n.resume().await,
            _ => drop(start(app, None).await),
        }
    } else {
        tokio::time::sleep(Duration::from_secs(60)).await;
        if !FG.load(SeqCst) {
            app.state::<App>().0.send_replace(None);
            nap().await;
        }
    }
}

/// Stop the node, out of sight and out of any call; it starts again on
/// return, or on a push.
#[cfg(target_os = "android")]
async fn nap() {
    use std::sync::atomic::Ordering::SeqCst;
    let awake = FG.load(SeqCst) || LIVE.load(SeqCst);
    let n = running().filter(|n| n.in_call().is_none() && !awake);
    if let Some(n) = n {
        *NODE.lock().unwrap_or_else(|e| e.into_inner()) = None;
        n.shutdown().await;
    }
}

fn running() -> Option<Arc<Node>> {
    NODE.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn adopt(n: Arc<Node>) {
    *NODE.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
}

fn config(
    dir: std::path::PathBuf,
    link: Option<String>,
    notes: Option<std::path::PathBuf>,
) -> anyhow::Result<Config> {
    let net = match (std::env::var("IRCH_RELAY"), std::env::var_os("IRCH_LAN"))
    {
        (Ok(r), _) => {
            Net::Relay(r.split(',').map(str::parse).collect::<Result<_, _>>()?)
        }
        (_, Some(_)) => Net::Lan,
        _ => Net::Default,
    };
    let (name, serve) = ("anon".into(), false);
    Ok(Config { dir, name, net, link, serve, notes })
}

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
    *NODE.lock().unwrap_or_else(|e| e.into_inner()) = None;
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

/// One encoded frame from the page, to everyone in our call. Android's IPC
/// carries no bytes: there they arrive as a JSON array of numbers.
#[tauri::command]
async fn media(s: State<'_, App>, req: Request<'_>) -> Result<(), String> {
    let n = node(&s).await?;
    match req.body() {
        InvokeBody::Raw(f) => n.send_media(f),
        InvokeBody::Json(v) => {
            n.send_media(&Vec::deserialize(v).map_err(|e| e.to_string())?)
        }
    }
    Ok(())
}

/// A picked file in parts (a page knows no paths), appended to its copy in
/// irch's folder, which is then offered; the reply is that copy's path.
/// Bytes, or base64 where IPC carries none (Android).
#[tauri::command]
fn upload(app: AppHandle, req: Request<'_>) -> Result<String, String> {
    use std::io::Write;
    let r = || {
        let id = req.headers().get("x-upload").and_then(|v| v.to_str().ok());
        let hex = |i: &&str| {
            i.len() == 32 && i.bytes().all(|b| b.is_ascii_hexdigit())
        };
        let id = id.filter(hex).context("no upload id")?;
        let dir = app.path().app_data_dir()?.join("offered");
        std::fs::create_dir_all(&dir)?;
        let part = match req.body() {
            InvokeBody::Raw(b) => b.clone(),
            InvokeBody::Json(v) => {
                let b64 = v["data"].as_str().context("no data")?;
                data_encoding::BASE64.decode(b64.as_bytes())?
            }
        };
        let p = dir.join(id);
        let mut f =
            std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
        f.write_all(&part)?;
        anyhow::Ok(p.to_string_lossy().into())
    };
    r().map_err(err)
}

/// Frames from our call, to the page: the sender's id, then the frame.
#[tauri::command]
async fn tune(s: State<'_, App>, to: Channel<Body>) -> Result<(), String> {
    let mut rx = node(&s).await?.media();
    tauri::async_runtime::spawn(async move {
        loop {
            match rx.recv().await {
                Ok((who, f)) => {
                    if to.send(Body::Raw([&who.0[..], &f].concat())).is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
    Ok(())
}

use irch_core::release::LATEST;

/// Built by package.nix: NixOS keeps it up to date.
const NIXOS: bool = option_env!("IRCH_NIXOS").is_some();

/// The latest release's version, if newer than this irch.
async fn newer(app: &AppHandle) -> anyhow::Result<Option<String>> {
    irch_core::release::newer(&app.package_info().version.to_string()).await
}

/// A storage node on this computer, always on and kept up to date: the
/// released terminal client, checked, installed as this user's service.
#[tauri::command]
async fn serve_here(app: AppHandle) -> Result<(), String> {
    let run = async {
        anyhow::ensure!(cfg!(desktop), "phones sleep: run one on a computer");
        anyhow::ensure!(!NIXOS, "on NixOS: services.irch.enable = true");
        let dir = app.path().app_local_data_dir()?.join("bin");
        std::fs::create_dir_all(&dir)?;
        let exe = dir.join(format!("irch{}", std::env::consts::EXE_SUFFIX));
        let bin = irch_core::release::fetch(&irch_core::release::cli()).await?;
        irch_core::release::put(&exe, &bin)?;
        let out = tokio::task::spawn_blocking(move || {
            std::process::Command::new(exe).arg("--install").output()
        });
        let out = out.await??;
        let say = String::from_utf8_lossy(&out.stderr);
        anyhow::ensure!(out.status.success(), "{}", say.trim());
        Ok(())
    };
    run.await.map_err(err)
}

/// A newer irch, if there is one; with `install`, install it (then, what
/// to tell). Desktop: in place, its signature checked against ours, then
/// restart; installed otherwise (nix, cargo), the release page opens.
/// Android: the system installer, which takes only our signing key.
#[tauri::command]
async fn update(
    app: AppHandle,
    install: bool,
) -> Result<Option<String>, String> {
    #[cfg(desktop)]
    {
        use tauri_plugin_updater::UpdaterExt;
        if NIXOS {
            return Ok(None);
        }
        let Some(v) = newer(&app).await.map_err(err)? else {
            return Ok(None);
        };
        if !install {
            return Ok(Some(v));
        }
        if let Ok(up) = app.updater()
            && let Ok(Some(u)) = up.check().await
            && u.download_and_install(|_, _| {}, || {}).await.is_ok()
        {
            app.restart();
        }
        browse(LATEST).map_err(err)?;
        Ok(Some(format!("irch {v}: its release page is open")))
    }
    #[cfg(target_os = "android")]
    android::update(&app, install).await.map_err(err)
}

/// The system's browser, at `url`.
#[cfg(desktop)]
fn browse(url: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    let open = "explorer";
    #[cfg(target_os = "macos")]
    let open = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let open = "xdg-open";
    std::process::Command::new(open).arg(url).spawn()?;
    Ok(())
}

/// The irch:// link irch was opened with, if any.
#[tauri::command]
fn opened(app: AppHandle) -> Option<String> {
    use tauri_plugin_deep_link::DeepLinkExt;
    let urls = app.deep_link().get_current().ok().flatten()?;
    urls.first().map(|u| u.to_string())
}

/// A QR code, as SVG, for codes shown to a camera.
#[tauri::command]
fn qr(text: String) -> Result<String, String> {
    let q = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    let mut svg = q.render::<qrcode::render::svg::Color>();
    Ok(svg.quiet_zone(true).min_dimensions(240, 240).build())
}

/// Typst math as MathML, off the event loop: the page checks the tags.
#[tauri::command]
async fn math(src: String, display: bool) -> Result<String, String> {
    let m = move || irch_math::mathml(&src, display);
    tauri::async_runtime::spawn_blocking(m).await.map_err(|e| e.to_string())?
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Calls ask for the camera, the microphone or the screen only after
    // a click on irch's own page; the operating system may still ask too.
    let b = tauri::Builder::default();
    // One irch per device: a second launch (an irch:// link from a note,
    // say) hands its link to the first and leaves.
    #[cfg(desktop)]
    let b = b.plugin(tauri_plugin_single_instance::init(|app, _, _| {
        if let Some(w) = app.get_webview_window("main") {
            let _ = (w.unminimize(), w.set_focus());
        }
    }));
    #[cfg(desktop)]
    let b = b
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_dialog::init());
    let b = b
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .on_permission_request(|_, kind| match kind {
            Kind::Camera | Kind::Microphone | Kind::DisplayCapture => {
                PermissionResponse::Allow
            }
            _ => PermissionResponse::Default,
        });
    #[cfg(not(target_os = "android"))]
    let b = b.plugin(tauri_plugin_notification::init());
    #[cfg(target_os = "android")]
    let b = b.plugin(android::init());
    let b = b.setup(|app| {
        app.manage(App(watch::channel(None).0));
        // WebKitGTK keeps getUserMedia off unless asked.
        #[cfg(target_os = "linux")]
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.with_webview(|w| {
                use webkit2gtk::{SettingsExt, WebViewExt};
                if let Some(s) = w.inner().settings() {
                    s.set_enable_media_stream(true);
                }
            });
        }
        use tauri_plugin_deep_link::DeepLinkExt;
        #[cfg(any(target_os = "linux", all(debug_assertions, windows)))]
        let _ = app.deep_link().register_all();
        let h = app.handle().clone();
        app.deep_link().on_open_url(move |e| {
            for u in e.urls() {
                let _ = h.emit("open-url", u.to_string());
            }
        });
        let h = app.handle().clone();
        tauri::async_runtime::spawn(async move {
            #[cfg(target_os = "android")]
            if let Some(b) = h.try_state::<android::Bridge>() {
                b.watch_network(h.clone()).await;
            }
            start(&h, None).await
        });
        Ok(())
    });
    let b = b.invoke_handler(tauri::generate_handler![
        irch, link, qr, math, save, share, media, tune, opened, update,
        viewing, hush, upload, serve_here
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
        // Started by a push before the app opened: the same node.
        if let Some(n) = running().filter(|_| link.is_none()) {
            return Ok(n);
        }
        let dir = app.path().app_data_dir()?;
        log_to(&dir);
        // Desktop: notes go to loroh's vault (LOROH_VAULT, else ~/notes).
        let notes =
            std::env::var_os("LOROH_VAULT").map(Into::into).or_else(|| {
                let home = app.path().home_dir().ok();
                home.filter(|_| cfg!(desktop)).map(|h| h.join("notes"))
            });
        let n = tokio::spawn(Node::start(config(dir, link, notes)?)).await??;
        adopt(n.clone());
        anyhow::Ok(n)
    };
    let r = r.await.map_err(err);
    if let Ok(n) = &r {
        forward(app.clone(), n.clone());
    }
    let done = r.as_ref().map(drop).map_err(Clone::clone);
    app.state::<App>().0.send_replace(Some(r));
    done
}

/// Core events to the page; a lagging receiver means "refresh everything".
/// Each alert also goes to the page, which sounds it.
fn forward(app: AppHandle, n: Arc<Node>) {
    use irch_core::Event::{Call, Changed};
    // Weak: a node stopped (on Android, out of sight) goes, and this ends.
    let (mut rx, n) = (n.subscribe(), Arc::downgrade(&n));
    tauri::async_runtime::spawn(async move {
        loop {
            let e = match rx.recv().await {
                Ok(e) => e,
                Err(RecvError::Lagged(_)) => {
                    let _ = app.emit("irch", json!({ "t": "changed" }));
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            let news = matches!(e, Changed { .. } | Call { .. });
            if let Some(n) = n.upgrade().filter(|_| news) {
                for x in alerts(&n, front(&app)) {
                    let _ = app.emit("alert", &x);
                    notify(&app, &x).await;
                }
            }
            let _ = app.emit("irch", e);
        }
    });
}

/// What to tell of: all but messages in the room on screen.
fn alerts(n: &Node, front: bool) -> Vec<Notice> {
    let view = *VIEW.lock().unwrap_or_else(|e| e.into_inner());
    let shown = |x: &Notice| front && view == Some((x.space, x.room));
    n.notices().into_iter().filter(|x| x.ring || !shown(x)).collect()
}

/// A call answered or declined in the page: Android's ringing stops.
#[tauri::command]
async fn hush(app: AppHandle) {
    #[cfg(target_os = "android")]
    if let Some(b) = app.try_state::<android::Bridge>() {
        b.hush().await;
    }
    let _ = app;
}

/// The room on screen, or none.
#[tauri::command]
fn viewing(space: Option<Id>, room: Option<Ch>) {
    *VIEW.lock().unwrap_or_else(|e| e.into_inner()) = space.zip(room);
}

#[cfg(target_os = "android")]
fn front(_: &AppHandle) -> bool {
    FG.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(target_os = "android"))]
fn front(app: &AppHandle) -> bool {
    app.webview_windows().values().any(|w| w.is_focused().unwrap_or(false))
}

/// Android: the system's sound and vibration, a call's ringtone.
#[cfg(target_os = "android")]
async fn notify(app: &AppHandle, x: &Notice) {
    if let Some(b) = app.try_state::<android::Bridge>() {
        b.alert(x).await;
    }
}

/// A desktop's banner, while no irch window is in front (the page sounds
/// it either way).
#[cfg(not(target_os = "android"))]
async fn notify(app: &AppHandle, x: &Notice) {
    use tauri_plugin_notification::NotificationExt;
    if !front(app) {
        let b = app.notification().builder().title(&x.title).body(&x.body);
        let _ = b.show();
    }
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
