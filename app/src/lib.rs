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

/// Files the person picked (desktop): the page may offer those, and what
/// it uploaded into irch's own folder, nothing else on this computer.
static PICKED: std::sync::Mutex<Vec<std::path::PathBuf>> =
    std::sync::Mutex::new(vec![]);

#[tauri::command]
async fn irch(
    app: AppHandle,
    s: State<'_, App>,
    call: api::Call,
) -> Result<Value, String> {
    if let api::Call::Offer { path, .. } = &call {
        let p = std::path::Path::new(path);
        let mut picked = PICKED.lock().unwrap_or_else(|e| e.into_inner());
        let chosen = picked.iter().position(|x| x == p);
        let ours = home(&app).map(|d| d.join("offered"));
        let ours = ours.is_ok_and(|d| p.parent() == Some(&d));
        if !ours && chosen.is_none() {
            return Err("only files you pick are shared".into());
        }
        chosen.map(|i| picked.remove(i));
    }
    api::call(&node(&s).await?, call).await.map_err(err)
}

/// Files to share, picked here (desktop): kept, by path, as they are.
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

/// In the tray (where the desktop has one): closing the window hides it.
#[cfg(desktop)]
static TRAY: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// The tray's icon and menu; at first, irch starts at login (NixOS: its
/// configuration says). Started at login: in the tray, no window.
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
        let _ = auto.enable(); // where irch is now: it may have moved
    }
    let on = auto.is_enabled().unwrap_or(false);
    // Linux: a tray only with the library it loads (else it aborts).
    #[cfg(target_os = "linux")]
    let lib = ["libayatana-appindicator3.so.1", "libappindicator3.so.1"]
        .iter()
        .any(|l| unsafe { libloading::Library::new(l) }.is_ok());
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
    // No tray to come back from: a window, whatever started it.
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
    stop(&s).await;
    let dir = home(&app).map_err(err)?;
    forget(&dir, &["seed", "local", "spaces"]);
    start(&app, Some(code)).await
}

/// Where irch keeps everything: profiles, and the first one's files.
fn root(app: &AppHandle) -> anyhow::Result<std::path::PathBuf> {
    Ok(app.path().app_data_dir()?)
}

/// The profile in use: its folder.
fn home(app: &AppHandle) -> anyhow::Result<std::path::PathBuf> {
    Ok(irch_core::profile(&root(app)?))
}

/// The node stopped (or one that never started, forgotten).
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

/// The profiles on this device, and which is in use ("" the first).
#[tauri::command]
fn profiles(app: AppHandle) -> Result<Value, String> {
    let r = root(&app).map_err(err)?;
    let mut all = vec![String::new()];
    let names = std::fs::read_dir(r.join("profiles")).into_iter().flatten();
    all.extend(names.flatten().map(|e| e.file_name().to_string_lossy().into()));
    let on = std::fs::read_to_string(r.join("profile")).unwrap_or_default();
    Ok(json!({ "current": on.trim(), "all": all }))
}

/// Another profile (a new one by a new name): its own identity, spaces
/// and storage nodes, as if another device. This one waits as it is.
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

/// This device forgets the profile in use: its identity, spaces, keys and
/// files. Others of your devices keep them; with none, they are gone.
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

/// A newer irch's version, if there is one (NixOS updates its own).
#[tauri::command]
async fn update(app: AppHandle) -> Result<Option<String>, String> {
    if NIXOS {
        return Ok(None);
    }
    newer(&app).await.map_err(err)
}

/// The newer irch, installed; then what to tell, if anything. Desktop: in
/// place, its signature checked against ours, then restart; installed
/// otherwise (cargo), the release page opens. Android: checked against
/// irch's key too, then the system installer, which takes only ours.
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
        browse(LATEST).map_err(err)?;
        Ok(Some("irch's release page is open: install it from there".into()))
    }
    #[cfg(target_os = "android")]
    android::upgrade(&app, percent(progress)).await.map_err(err)
}

/// Phones: what decides whether they are told of messages and calls, and
/// asking for what is missing. Desktops are told while irch runs: null.
#[tauri::command]
async fn notify_check(app: AppHandle, ask: String) -> Result<Value, String> {
    #[cfg(target_os = "android")]
    return android::notify_check(&app, &ask).await.map_err(err);
    #[cfg(desktop)]
    {
        let _ = (app, ask);
        Ok(Value::Null)
    }
}

/// Bytes as they arrive, as a percentage for the page (each once).
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
        #[cfg(desktop)]
        tray(app);
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
        profiles,
        use_profile,
        sign_out
    ]);
    let app = b.build(tauri::generate_context!()).expect("tauri builds");
    app.run(|app, e| {
        let n = || app.state::<App>().0.borrow().clone().and_then(Result::ok);
        match e {
            // macOS: the Dock's icon, clicked, with the window in the tray.
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show(app),
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
                // Inside the runtime: this thread has none, and a panic
                // here would end a restart (an update's) before it relaunches.
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

/// Start the node off the UI thread; failures reach the page as text.
async fn start(app: &AppHandle, link: Option<String>) -> Result<(), String> {
    let r = async {
        // Started by a push before the app opened: the same node.
        if let Some(n) = running().filter(|_| link.is_none()) {
            return Ok(n);
        }
        let dir = home(app)?;
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
