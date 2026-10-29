//! irch on desktops and phones: the page in `ui/` over one command.

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

/// Shared with push wakes (android.rs).
static NODE: std::sync::Mutex<Option<Arc<Node>>> = std::sync::Mutex::new(None);
/// In sight; never in a process Android started for the service alone.
#[cfg(target_os = "android")]
static FG: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The room on screen: no notifications for it while in sight.
static VIEW: std::sync::Mutex<Option<(Id, Ch)>> = std::sync::Mutex::new(None);

/// Out of sight a minute, the node stops: keepalives would wake the radio.
#[cfg(target_os = "android")]
async fn seen(app: &AppHandle, fg: bool) {
    use std::sync::atomic::Ordering::SeqCst;
    // Only a change counts: at launch, the app starts the node itself.
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

#[cfg(target_os = "android")]
async fn nap() {
    use std::sync::atomic::Ordering::SeqCst;
    let n = running().filter(|n| n.in_call().is_none() && !FG.load(SeqCst));
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
    Ok(Config { dir, name, net, link, serve, notes, big: None })
}

struct App(watch::Sender<Started>);

async fn node(s: &App) -> Result<Arc<Node>, String> {
    let mut rx = s.0.subscribe();
    let v = rx.wait_for(Option::is_some).await.map_err(|e| e.to_string())?;
    v.clone().expect("checked")
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// The page may offer only these (desktop) and its own uploads.
static PICKED: std::sync::Mutex<Vec<std::path::PathBuf>> =
    std::sync::Mutex::new(vec![]);

#[tauri::command]
async fn irch(
    app: AppHandle,
    s: State<'_, App>,
    call: api::Call,
) -> Result<Value, String> {
    let mut upload = None;
    if let api::Call::Offer { path, .. } | api::Call::Seal { path, .. } = &call
    {
        let p = std::path::Path::new(path);
        let mut picked = PICKED.lock().unwrap_or_else(|e| e.into_inner());
        let chosen = picked.iter().position(|x| x == p);
        let ours = home(&app).map(|d| d.join("offered"));
        let ours = ours.is_ok_and(|d| p.parent() == Some(&d));
        if !ours && chosen.is_none() {
            return Err("only files you pick are shared".into());
        }
        chosen.map(|i| picked.remove(i));
        // Sealed, an upload's plain copy goes.
        let seal = matches!(call, api::Call::Seal { .. });
        upload = (ours && seal).then(|| p.to_path_buf());
    }
    let r = api::call(&node(&s).await?, call).await.map_err(err);
    if let Some(p) = upload {
        let _ = std::fs::remove_file(p);
    }
    r
}

#[cfg(desktop)]
#[tauri::command]
async fn pick(app: AppHandle) -> Vec<String> {
    use tauri_plugin_dialog::DialogExt;
    let got = tauri::async_runtime::spawn_blocking(move || {
        app.dialog().file().blocking_pick_files()
    });
    let got = got.await.ok().flatten().unwrap_or_default();
    let got: Vec<_> =
        got.into_iter().filter_map(|f| f.into_path().ok()).collect();
    PICKED.lock().unwrap_or_else(|e| e.into_inner()).extend(got.clone());
    got.iter().map(|p| p.to_string_lossy().into()).collect()
}

#[cfg(mobile)]
#[tauri::command]
fn pick() -> Vec<String> {
    vec![]
}

#[cfg(desktop)]
static TRAY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Starts at login by default, except on NixOS, whose config decides.
#[cfg(desktop)]
fn tray(app: &tauri::App) {
    use tauri::menu::{CheckMenuItem, Menu, MenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
    use tauri_plugin_autostart::ManagerExt;
    let auto = app.autolaunch();
    let first = root(app.handle()).ok().map(|r| r.join("autostart"));
    if let Some(f) = first.filter(|f| !f.exists() && !NIXOS) {
        let _ = auto.enable().and_then(|()| Ok(std::fs::write(f, "")?));
    } else if auto.is_enabled().unwrap_or(false) {
        let _ = auto.enable(); // irch may have moved
    }
    let on = auto.is_enabled().unwrap_or(false);
    // Linux: a tray only with the library it loads (else it aborts).
    #[cfg(target_os = "linux")]
    let lib = ["libayatana-appindicator3.so.1", "libappindicator3.so.1"]
        .iter()
        .any(|l| unsafe { libloading::Library::new(*l) }.is_ok());
    #[cfg(not(target_os = "linux"))]
    let lib = true;
    let built = lib.then(|| {
        let open =
            MenuItem::with_id(app, "open", "Open irch", true, None::<&str>)?;
        let login = CheckMenuItem::with_id(
            app,
            "login",
            "Start at login",
            !NIXOS,
            on,
            None::<&str>,
        )?;
        let quit =
            MenuItem::with_id(app, "quit", "Quit irch", true, None::<&str>)?;
        let menu = Menu::with_items(app, &[&open, &login, &quit])?;
        let icon = app.default_window_icon().cloned().ok_or(
            tauri::Error::InvalidIcon(std::io::Error::other("no icon")),
        )?;
        tauri::tray::TrayIconBuilder::with_id("irch")
            .icon(icon)
            .tooltip("irch")
            .menu(&menu)
            .show_menu_on_left_click(false)
            .on_menu_event(move |app, e| match e.id.as_ref() {
                "open" => show(app),
                "login" => {
                    let a = app.autolaunch();
                    let on = a.is_enabled().unwrap_or(false);
                    let _ = if on { a.disable() } else { a.enable() };
                    let _ = login.set_checked(a.is_enabled().unwrap_or(false));
                }
                "quit" => app.exit(0),
                _ => {}
            })
            .on_tray_icon_event(|t, e| {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } = e
                {
                    show(t.app_handle());
                }
            })
            .build(app)
    });
    let tray = built.is_some_and(|t| {
        t.inspect_err(|e| tracing::warn!("tray: {e}")).is_ok()
    });
    TRAY.store(tray, std::sync::atomic::Ordering::SeqCst);
    // Hidden only with a tray to come back from.
    let hidden = std::env::args().any(|a| a == "--hidden");
    if hidden
        && tray
        && let Some(w) = app.get_webview_window("main")
    {
        let _ = w.hide();
    }
}

#[cfg(desktop)]
fn show(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = (w.show(), w.unminimize(), w.set_focus());
    }
}

/// Fresh installs only: this device's identity is thrown away.
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
    stop(&s).await;
    let dir = home(&app).map_err(err)?;
    forget(&dir, &["seed", "local", "spaces"]);
    start(&app, Some(code)).await
}

fn root(app: &AppHandle) -> anyhow::Result<std::path::PathBuf> {
    Ok(app.path().app_data_dir()?)
}

fn home(app: &AppHandle) -> anyhow::Result<std::path::PathBuf> {
    Ok(irch_core::profile(&root(app)?))
}

async fn stop(s: &App) {
    if let Ok(n) = node(s).await {
        n.shutdown().await;
    }
    s.0.send_replace(None);
    *NODE.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn forget(dir: &std::path::Path, files: &[&str]) {
    for f in files {
        let p = dir.join(f);
        let _ =
            std::fs::remove_file(&p).or_else(|_| std::fs::remove_dir_all(&p));
    }
}

/// "" is the first profile.
#[tauri::command]
fn profiles(app: AppHandle) -> Result<Value, String> {
    let r = root(&app).map_err(err)?;
    let mut all = vec![String::new()];
    let names = std::fs::read_dir(r.join("profiles")).into_iter().flatten();
    all.extend(names.flatten().map(|e| e.file_name().to_string_lossy().into()));
    let on = std::fs::read_to_string(r.join("profile")).unwrap_or_default();
    Ok(json!({ "current": on.trim(), "all": all }))
}

#[tauri::command]
async fn use_profile(
    app: AppHandle,
    s: State<'_, App>,
    name: String,
) -> Result<(), String> {
    let ok = |c: char| c.is_alphanumeric() || " _-".contains(c);
    let name = name.trim();
    if name.len() > 32 || !name.chars().all(ok) || name.starts_with(['-', ' '])
    {
        return Err("a profile's name: letters, digits, spaces, - and _".into());
    }
    stop(&s).await;
    let r = root(&app).map_err(err)?;
    let set = match name {
        "" => std::fs::remove_file(r.join("profile")).or(Ok(())),
        n => std::fs::create_dir_all(r.join("profiles").join(n))
            .and_then(|()| std::fs::write(r.join("profile"), n)),
    };
    set.map_err(|e| e.to_string())?;
    start(&app, None).await
}

/// Only your other devices keep what this forgets.
#[tauri::command]
async fn sign_out(app: AppHandle, s: State<'_, App>) -> Result<(), String> {
    stop(&s).await;
    let (r, dir) = (root(&app).map_err(err)?, home(&app).map_err(err)?);
    if dir == r {
        forget(&r, &irch_core::MINE);
    } else {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(r.join("profile"));
    }
    start(&app, None).await
}

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
    #[cfg(target_os = "ios")]
    let dir = p.document_dir(); // Files › irch
    #[cfg(desktop)]
    let dir = p.download_dir().or_else(|_| p.home_dir());
    let dir = dir.map_err(|e| e.to_string())?;
    let f = n.save(&space, &file, &dir).await.map_err(err)?;
    #[cfg(target_os = "android")]
    if let Some(b) = app.try_state::<android::Bridge>() {
        b.share_file(&f).await;
    }
    Ok(f.display().to_string())
}

/// Android's IPC carries no bytes: frames come as JSON arrays of numbers.
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

/// A picked file in parts, as a page knows no paths; base64 on Android.
#[tauri::command]
fn upload(app: AppHandle, req: Request<'_>) -> Result<String, String> {
    use std::io::Write;
    let r = || {
        let id = req.headers().get("x-upload").and_then(|v| v.to_str().ok());
        let hex = |i: &&str| {
            i.len() == 32 && i.bytes().all(|b| b.is_ascii_hexdigit())
        };
        let id = id.filter(hex).context("no upload id")?;
        let dir = home(&app)?.join("offered");
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

/// Each frame to the page: the sender's id, then the frame.
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

/// Built by package.nix: NixOS keeps it up to date.
const NIXOS: bool = option_env!("IRCH_NIXOS").is_some();

async fn newer(app: &AppHandle) -> anyhow::Result<Option<String>> {
    irch_core::release::newer(&app.package_info().version.to_string()).await
}

/// A storage node here: the released CLI, as this user's service.
#[tauri::command]
async fn serve_here(app: AppHandle) -> Result<(), String> {
    let run = async {
        anyhow::ensure!(cfg!(desktop), "phones sleep: run one on a computer");
        anyhow::ensure!(!NIXOS, "on NixOS: services.irch.enable = true");
        let dir = app.path().app_local_data_dir()?.join("bin");
        std::fs::create_dir_all(&dir)?;
        let exe = dir.join(format!("irch{}", std::env::consts::EXE_SUFFIX));
        let (cli, v) =
            (irch_core::release::cli(), irch_core::release::latest());
        let bin = irch_core::release::fetch(&cli, &v.await?).await?;
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

#[tauri::command]
async fn update(app: AppHandle) -> Result<Option<String>, String> {
    // The App Store, TestFlight or AltStore updates an iPhone's.
    if NIXOS || cfg!(target_os = "ios") {
        return Ok(None);
    }
    newer(&app).await.map_err(err)
}

/// In place, signature checked; else (cargo installs) the release page.
#[tauri::command]
async fn upgrade(
    app: AppHandle,
    progress: Channel<u32>,
) -> Result<Option<String>, String> {
    #[cfg(desktop)]
    {
        use tauri_plugin_updater::UpdaterExt;
        if let Ok(up) = app.updater()
            && let Ok(Some(u)) = up.check().await
            && u.download_and_install(percent(progress), || {}).await.is_ok()
        {
            app.restart();
        }
        browse(irch_core::release::LATEST).map_err(err)?;
        Ok(Some("irch's release page is open: install it from there".into()))
    }
    #[cfg(target_os = "android")]
    {
        let _ = progress;
        android::upgrade(&app).await.map(Some).map_err(err)
    }
    #[cfg(target_os = "ios")]
    {
        let _ = (app, progress);
        Ok(None)
    }
}

#[tauri::command]
async fn notify_check(app: AppHandle, ask: String) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    return android::notify_check(&app, &ask).await.map_err(err);
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, ask);
        Ok(Value::Null)
    }
}

#[cfg(desktop)]
fn percent(to: Channel<u32>) -> impl FnMut(usize, Option<u64>) {
    let (mut got, mut last) = (0u64, u32::MAX);
    move |n, total| {
        got += n as u64;
        let p = total.map_or(0, |t| (got * 100 / t.max(1)) as u32);
        if p != last {
            last = p;
            let _ = to.send(p);
        }
    }
}

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

#[tauri::command]
fn opened(app: AppHandle) -> Option<String> {
    use tauri_plugin_deep_link::DeepLinkExt;
    let urls = app.deep_link().get_current().ok().flatten()?;
    urls.first().map(|u| u.to_string())
}

/// Into loroh, which shows it and asks first: `loroh://add`.
#[tauri::command]
async fn to_loroh(
    app: AppHandle,
    file: String,
    text: String,
) -> Result<(), String> {
    let mut u = tauri::Url::parse("loroh://add").map_err(|e| e.to_string())?;
    u.query_pairs_mut().append_pair("file", &file).append_pair("text", &text);
    #[cfg(desktop)]
    {
        let _ = app;
        browse(u.as_str()).map_err(err)
    }
    #[cfg(target_os = "android")]
    {
        let b = app.try_state::<android::Bridge>().ok_or("no bridge")?;
        let opened = b.browse(u.as_str()).await;
        opened.then_some(()).ok_or_else(|| "loroh is not installed".into())
    }
    // No loroh there: the page copies it instead.
    #[cfg(target_os = "ios")]
    {
        let _ = (app, u);
        Err("no loroh on iPhones".into())
    }
}

/// A web link, opened only after the page showed it in full.
#[tauri::command]
async fn open_link(app: AppHandle, url: String) -> Result<(), String> {
    let u = tauri::Url::parse(&url).map_err(|e| e.to_string())?;
    if !matches!(u.scheme(), "https" | "http" | "mailto") {
        return Err("only web and mail links open".into());
    }
    #[cfg(desktop)]
    {
        let _ = app;
        browse(u.as_str()).map_err(err)
    }
    #[cfg(target_os = "android")]
    {
        let b = app.try_state::<android::Bridge>().ok_or("no bridge")?;
        b.browse(u.as_str()).await.then_some(()).ok_or("no app opens it".into())
    }
    #[cfg(target_os = "ios")]
    {
        let _ = (app, u);
        Err("no browser to hand it to here: it is copied".into())
    }
}

/// Banners that say only that something came: on, off, or as it is.
#[tauri::command]
fn private(app: AppHandle, on: Option<bool>) -> Result<bool, String> {
    let f = home(&app).map_err(err)?.join("private");
    let _ = match on {
        Some(true) => std::fs::write(&f, ""),
        Some(false) => std::fs::remove_file(&f),
        None => Ok(()),
    };
    Ok(f.exists())
}

fn hidden(app: &AppHandle) -> bool {
    home(app).is_ok_and(|d| d.join("private").exists())
}

#[tauri::command]
fn log(msg: String) {
    tracing::warn!("page: {}", msg.chars().take(500).collect::<String>());
}

#[tauri::command]
fn qr(text: String) -> Result<String, String> {
    let q = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    let mut svg = q.render::<qrcode::render::svg::Color>();
    Ok(svg.quiet_zone(true).min_dimensions(240, 240).build())
}

/// MathML; the page checks the tags.
#[tauri::command]
async fn math(src: String, display: bool) -> Result<String, String> {
    let m = move || irch_math::mathml(&src, display);
    tauri::async_runtime::spawn_blocking(m).await.map_err(|e| e.to_string())?
}

#[tauri::command]
async fn share(app: AppHandle, text: String) -> Result<(), String> {
    #[cfg(target_os = "android")]
    if let Some(b) = app.try_state::<android::Bridge>() {
        b.share(&text).await;
        return Ok(());
    }
    let _ = (app, text);
    Err("no share sheet here".into())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Media allowed: the page asks only on a click; the OS may ask too.
    let b = tauri::Builder::default();
    // One irch per device: a second launch hands its link to the first.
    #[cfg(desktop)]
    let b = b.plugin(tauri_plugin_single_instance::init(|app, _, _| show(app)));
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
    #[cfg(desktop)]
    let b = b
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        // Closed, irch stays in the tray: still told of messages and calls.
        .on_window_event(|w, e| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = e
                && TRAY.load(std::sync::atomic::Ordering::SeqCst)
            {
                api.prevent_close();
                let _ = w.hide();
            }
        });
    let b = b.setup(|app| {
        app.manage(App(watch::channel(None).0));
        #[cfg(target_os = "android")]
        FG.store(true, std::sync::atomic::Ordering::SeqCst);
        #[cfg(desktop)]
        tray(app);
        // WebKitGTK keeps getUserMedia off unless asked.
        #[cfg(target_os = "linux")]
        if let Some(w) = app.get_webview_window("main") {
            let _ = w.with_webview(|w| {
                use webkit2gtk::{SettingsExt, WebViewExt};
                if let Some(s) = w.inner().settings() {
                    s.set_enable_media_stream(true);
                    if cfg!(debug_assertions)
                        && std::env::var_os("IRCH_TEST_JS").is_some()
                    {
                        s.set_enable_mock_capture_devices(true);
                        s.set_enable_write_console_messages_to_stdout(true);
                    }
                }
            });
        }
        #[cfg(debug_assertions)]
        if let (Some(f), Some(w)) =
            (std::env::var_os("IRCH_TEST_JS"), app.get_webview_window("main"))
        {
            let js = std::fs::read_to_string(f)?;
            tauri::async_runtime::spawn(async move {
                tokio::time::sleep(Duration::from_secs(6)).await;
                let _ = w.eval(&js);
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
            #[cfg(target_os = "ios")]
            {
                use tauri_plugin_notification::NotificationExt;
                let _ = h.notification().request_permission();
            }
            start(&h, None).await
        });
        Ok(())
    });
    let b = b.invoke_handler(tauri::generate_handler![
        irch,
        link,
        qr,
        math,
        save,
        share,
        media,
        tune,
        opened,
        update,
        upgrade,
        viewing,
        hush,
        upload,
        serve_here,
        notify_check,
        pick,
        log,
        profiles,
        use_profile,
        sign_out,
        to_loroh,
        open_link,
        private
    ]);
    let app = b.build(tauri::generate_context!()).expect("tauri builds");
    app.run(|app, e| {
        let n = || app.state::<App>().0.borrow().clone().and_then(Result::ok);
        match e {
            // macOS: the Dock's icon, clicked, with the window in the tray.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show(app),
            // Connections may have died while frozen.
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
                // No runtime here; a panic would stop an update's restart.
                if let Some(n) = n() {
                    let bye = async {
                        let two = Duration::from_secs(2);
                        let _ = tokio::time::timeout(two, n.shutdown()).await;
                    };
                    tauri::async_runtime::block_on(bye);
                }
            }
            _ => {}
        }
    });
}

async fn start(app: &AppHandle, link: Option<String>) -> Result<(), String> {
    let r = async {
        // A push may have started it already.
        if let Some(n) = running().filter(|_| link.is_none()) {
            return Ok(n);
        }
        let dir = home(app)?;
        log_to(&dir);
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

/// Core events to the page; lagging, it refreshes everything.
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

fn alerts(n: &Node, front: bool) -> Vec<Notice> {
    let view = *VIEW.lock().unwrap_or_else(|e| e.into_inner());
    let shown = |x: &Notice| front && view == Some((x.space, x.room));
    n.notices().into_iter().filter(|x| x.ring || !shown(x)).collect()
}

#[tauri::command]
async fn hush(app: AppHandle) {
    #[cfg(target_os = "android")]
    if let Some(b) = app.try_state::<android::Bridge>() {
        b.hush().await;
    }
    let _ = app;
}

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

/// With private banners, who and what stay in the app.
fn veiled(app: &AppHandle, x: &Notice) -> Notice {
    veil(x.clone(), hidden(app))
}

fn veil(mut x: Notice, hide: bool) -> Notice {
    if hide {
        x.title = "irch".into();
        x.body = if x.ring { "Incoming call" } else { "New message" }.into();
    }
    x
}

#[cfg(target_os = "android")]
async fn notify(app: &AppHandle, x: &Notice) {
    if let Some(b) = app.try_state::<android::Bridge>() {
        b.alert(&veiled(app, x)).await;
    }
}

/// A banner unless in front (the page chimes); a call also raises irch.
#[cfg(not(target_os = "android"))]
async fn notify(app: &AppHandle, x: &Notice) {
    use tauri_plugin_notification::NotificationExt;
    let x = &veiled(app, x);
    let sound = match () {
        _ if cfg!(target_os = "macos") => "NSUserNotificationDefaultSoundName",
        _ if cfg!(windows) => "IM",
        _ => "message-new-instant",
    };
    // Linux's servers read a body as markup: a message's <a> stays text.
    let esc = |s: &str| {
        let s = s.replace('&', "&amp;").replace('<', "&lt;");
        s.replace('>', "&gt;")
    };
    let body =
        if cfg!(target_os = "linux") { esc(&x.body) } else { x.body.clone() };
    if x.ring || !front(app) {
        let b = app.notification().builder().title(&x.title).body(body);
        if let Err(e) = b.sound(sound).show() {
            tracing::warn!("notification: {e}");
        }
    }
    #[cfg(desktop)]
    if x.ring {
        show(app);
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
