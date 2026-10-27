//! Android, through `IrchPlugin.kt` in `gen/android`: network changes
//! (which iroh cannot see from native code there), notifications, the share
//! sheet; and through `Push.kt`, push wakes when no activity is running.

use anyhow::Context;
use irch_core::wake::{self, News};
use irch_core::{Node, Notice};
use jni::JNIEnv;
use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jstring};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};
use tauri::ipc::Channel;
use tauri::plugin::{Builder, PluginHandle, TauriPlugin};
use tauri::{Manager, Wry};

pub struct Bridge(PluginHandle<Wry>);

pub fn init() -> TauriPlugin<Wry> {
    Builder::new("irch")
        .setup(|app, api| {
            let h =
                api.register_android_plugin("dev.irch.chat", "IrchPlugin")?;
            app.manage(Bridge(h));
            Ok(())
        })
        .build()
}

#[derive(Serialize)]
struct Watch {
    channel: Channel,
}

#[derive(Serialize)]
struct Text<'a> {
    text: &'a str,
}

/// The latest release's APK, from the browser: Android installs it over
/// this one only if irch's key signed it. irch itself never fetches or
/// installs apps (Play Protect takes an app that does for malware).
pub async fn upgrade(app: &tauri::AppHandle) -> anyhow::Result<String> {
    let url = format!("{}/download/irch.apk", crate::LATEST);
    let bridge = app.try_state::<Bridge>().context("no bridge")?;
    bridge.call("browse", Url { url: &url }).await;
    Ok("Your browser downloads irch: open it to install".into())
}

/// What decides whether this phone is told, after `ask`: "fix" (Android
/// asks for what is missing), "background" (to run unrestricted), "test"
/// (a notification, now), or nothing.
pub async fn notify_check(
    app: &tauri::AppHandle,
    ask: &str,
) -> anyhow::Result<serde_json::Value> {
    let b = app.try_state::<Bridge>().context("no bridge")?;
    let cmd = match ask {
        "fix" => Some("allowAlerts"),
        "background" => Some("askBackground"),
        "test" => Some("testAlert"),
        _ => None,
    };
    if let Some(cmd) = cmd {
        b.call(cmd, serde_json::json!({})).await;
    }
    let got = b.0.run_mobile_plugin_async("alerts", serde_json::json!({}));
    Ok(got.await?)
}

#[derive(Serialize)]
struct Url<'a> {
    url: &'a str,
}

#[derive(Serialize)]
struct FilePath<'a> {
    path: &'a str,
}

impl Bridge {
    /// Every network change: re-probe, re-dial, catch up. And irch in
    /// sight or out of it: the node wakes, or soon sleeps. Once.
    pub async fn watch_network(&self, app: tauri::AppHandle) {
        static ONCE: AtomicBool = AtomicBool::new(false);
        if ONCE.swap(true, SeqCst) {
            return;
        }
        let channel = Channel::new(move |b| {
            let v: serde_json::Value = b.deserialize().unwrap_or_default();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                match v["fg"].as_bool() {
                    Some(fg) => crate::seen(&app, fg).await,
                    None => {
                        if let Some(n) = crate::running() {
                            n.resume().await
                        }
                    }
                }
            });
            Ok(())
        });
        self.call("watchNetwork", Watch { channel }).await;
    }

    pub async fn alert(&self, x: &Notice) {
        self.call("alert", x).await;
    }

    pub async fn hush(&self) {
        self.call("hush", serde_json::json!({})).await;
    }

    pub async fn share(&self, text: &str) {
        self.call("share", Text { text }).await;
    }

    pub async fn share_file(&self, path: &std::path::Path) {
        let path = &path.to_string_lossy();
        self.call("shareFile", FilePath { path }).await;
    }

    async fn call(&self, cmd: &str, args: impl Serialize) {
        if let Err(e) = self.0.run_mobile_plugin_async::<()>(cmd, args).await {
            tracing::warn!("android {cmd}: {e}");
        }
    }
}

// We gave ndk-context Android's context (no activity had); the activity's
// runtime (tao) sets it once per process and asserts nobody did before.
static OURS: AtomicBool = AtomicBool::new(false);
static TAO: AtomicBool = AtomicBool::new(false);

/// What rang, not yet taken in.
static NEWS: std::sync::Mutex<Vec<News>> = std::sync::Mutex::new(vec![]);

/// `Wake.run`: a push or a ring came. Start the node if irch was not
/// running (or wake it), take in what rang, and return what to notify
/// about, as JSON: at once, or (`full`) after catching up. While the app
/// runs, it notifies itself and this returns `[]`.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_run<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    ctx: JObject<'l>,
    dir: JString<'l>,
    push: JString<'l>,
    full: jboolean,
) -> jstring {
    let (dir, push) = (text(&mut env, &dir), text(&mut env, &push));
    let dir =
        irch_core::profile(std::path::Path::new(&dir.unwrap_or_default()));
    let wake = wake(dir, push, full != 0);
    reply(env, ctx, wake)
}

/// `Wake.moved`: another network. The doorbell (and the node, if up)
/// find their relay again now, not when a ping fails.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_moved(_: JNIEnv, _: JClass) {
    tauri::async_runtime::block_on(async {
        wake::moved().await;
        if let Some(n) = crate::running() {
            n.resume().await;
        }
    });
}

/// `Wake.rung`: no push app to wake irch, so a service waits on its
/// doorbell, again and again; rung, it catches up (`Wake.run`) holding
/// the CPU awake, which this, waiting, never does.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_rung<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    ctx: JObject<'l>,
    dir: JString<'l>,
) -> jstring {
    let dir = text(&mut env, &dir).unwrap_or_default();
    let dir = irch_core::profile(std::path::Path::new(&dir));
    reply(env, ctx, async {
        if let Err(e) = rung(dir).await {
            tracing::warn!("doorbell: {e:#}");
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        Ok(vec![])
    })
}

fn text(env: &mut JNIEnv, s: &JString) -> Option<String> {
    (!s.is_null()).then(|| env.get_string(s).ok().map(Into::into))?
}

/// Run `f` with Android's context set, and hand back its notices as JSON.
fn reply(
    env: JNIEnv,
    ctx: JObject,
    f: impl Future<Output = anyhow::Result<Vec<Notice>>>,
) -> jstring {
    if !TAO.load(SeqCst)
        && !OURS.load(SeqCst)
        && let (Ok(vm), Ok(g)) = (env.get_java_vm(), env.new_global_ref(ctx))
    {
        let (vm, c) =
            (vm.get_java_vm_pointer().cast(), g.as_obj().as_raw().cast());
        unsafe { ndk_context::initialize_android_context(vm, c) };
        std::mem::forget(g);
        OURS.store(true, SeqCst);
    }
    let r = tauri::async_runtime::block_on(f);
    let r =
        r.inspect_err(|e| tracing::warn!("wake: {e:#}")).unwrap_or_default();
    let json = serde_json::to_string(&r).unwrap_or_else(|_| "[]".into());
    env.new_string(json).map_or(std::ptr::null_mut(), |s| s.into_raw())
}

/// `Wake.release`: the activity is about to start; it brings its own.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_release(_: JNIEnv, _: JClass) {
    TAO.store(true, SeqCst);
    if OURS.swap(false, SeqCst) {
        unsafe { ndk_context::release_android_context() };
    }
}

async fn wake(
    dir: PathBuf,
    push: Option<String>,
    full: bool,
) -> anyhow::Result<Vec<Notice>> {
    let n = node(dir).await?;
    if let Some(p) = push {
        n.set_push((!p.is_empty()).then_some(p)).await;
    }
    let news =
        std::mem::take(&mut *NEWS.lock().unwrap_or_else(|e| e.into_inner()));
    news.into_iter().for_each(|x| n.deliver(x));
    if crate::FG.load(SeqCst) {
        return Ok(vec![]); // in sight: the app shows it
    }
    if !full {
        return Ok(crate::alerts(&n, false)); // what rang, told now
    }
    n.resume().await;
    // Catch up until a few quiet seconds, at most half a minute.
    let (mut rx, end) =
        (n.subscribe(), Instant::now() + Duration::from_secs(30));
    while Instant::now() < end
        && matches!(
            tokio::time::timeout(Duration::from_secs(4), rx.recv()).await,
            Ok(Ok(_))
        )
    {}
    let notices = crate::alerts(&n, false);
    crate::nap().await; // and back to sleep, unless irch came into sight
    Ok(notices)
}

/// The running node, or one started here (no activity, so no app).
async fn node(dir: PathBuf) -> anyhow::Result<std::sync::Arc<Node>> {
    static ONE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _one = ONE.lock().await;
    if let Some(n) = crate::running() {
        return Ok(n);
    }
    crate::log_to(&dir);
    let n = Node::start(crate::config(dir, None, None)?).await?;
    crate::adopt(n.clone());
    Ok(n)
}

/// Until the doorbell rings (wake.rs), or half an hour passes (in case a
/// ring was missed: catch up anyway): out of sight, no node at all, and
/// the doorbell told to our spaces once.
async fn rung(dir: PathBuf) -> anyhow::Result<()> {
    static TOLD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let net = crate::config(dir.clone(), None, None)?.net;
    let url = wake::doorbell(&dir, &net).await?;
    let told = TOLD.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if told.as_ref() != Some(&url) {
        node(dir.clone()).await?.set_push(Some(url.clone())).await;
        *TOLD.lock().unwrap_or_else(|e| e.into_inner()) = Some(url);
    }
    while crate::FG.load(SeqCst) {
        // In sight: the app shows what arrives.
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    crate::nap().await;
    let half = Duration::from_secs(30 * 60);
    if let Ok(news) = tokio::time::timeout(half, wake::rung(&dir, &net)).await {
        NEWS.lock().unwrap_or_else(|e| e.into_inner()).extend(news?);
    }
    Ok(())
}
