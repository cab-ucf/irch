//! Android: `IrchPlugin.kt` sees network changes iroh cannot; `Push.kt` wakes.

extern crate irpush; // org.irpush.Native, for irpush's Android library

use anyhow::Context;
use irch_core::push;
use irch_core::wake::{self, News};
use irch_core::{Node, Notice};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString};
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

/// Via the browser: Play Protect takes apps that install apps for malware.
pub async fn upgrade(app: &tauri::AppHandle) -> anyhow::Result<String> {
    let url = format!("{}/download/irch.apk", irch_core::release::LATEST);
    let bridge = app.try_state::<Bridge>().context("no bridge")?;
    bridge.call("browse", Url { url: &url }).await;
    Ok("Your browser downloads irch: open it to install".into())
}

pub async fn notify_check(
    app: &tauri::AppHandle,
    ask: &str,
) -> anyhow::Result<serde_json::Value> {
    let b = app.try_state::<Bridge>().context("no bridge")?;
    // The app's own traffic and the battery, for Network use.
    if ask == "traffic" {
        let got = b.0.run_mobile_plugin_async("traffic", serde_json::json!({}));
        return Ok(got.await?);
    }
    let cmd = match ask {
        "fix" => Some("allowAlerts"),
        "test" => Some("testAlert"),
        "battery" => Some("batteryUse"),
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
struct Write<'a> {
    path: &'a str,
    uri: &'a str,
}

impl Bridge {
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

    pub async fn in_call(&self, on: bool) {
        self.call("inCall", serde_json::json!({ "on": on })).await;
    }

    pub async fn share(&self, text: &str) {
        self.call("share", Text { text }).await;
    }

    /// Where to save `name`, in Android's picker: `None` if put away.
    pub async fn save_as(&self, name: &str) -> anyhow::Result<Option<String>> {
        #[derive(serde::Deserialize)]
        struct Picked {
            uri: Option<String>,
        }
        let r = self
            .0
            .run_mobile_plugin_async::<Picked>("saveAs", Text { text: name });
        Ok(r.await?.uri)
    }

    /// The file at `path` to where `save_as` said, and gone from here.
    pub async fn write_to(
        &self,
        path: &std::path::Path,
        uri: &str,
    ) -> anyhow::Result<()> {
        let path = &path.to_string_lossy();
        let r = self
            .0
            .run_mobile_plugin_async::<()>("writeTo", Write { path, uri });
        Ok(r.await?)
    }

    /// False if no app opens it.
    pub async fn browse(&self, url: &str) -> bool {
        let r = self.0.run_mobile_plugin_async::<()>("browse", Url { url });
        r.await.is_ok()
    }

    async fn call(&self, cmd: &str, args: impl Serialize) {
        if let Err(e) = self.0.run_mobile_plugin_async::<()>(cmd, args).await {
            tracing::warn!("android {cmd}: {e}");
        }
    }
}

// tao sets ndk-context once per process, asserting nobody did before.
static OURS: AtomicBool = AtomicBool::new(false);
static TAO: AtomicBool = AtomicBool::new(false);

static NEWS: std::sync::Mutex<Vec<News>> = std::sync::Mutex::new(vec![]);

/// Notices as JSON: at once, or (`full`) after catching up.
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

/// A call declined from its notification: said by the node the ring
/// started, so its caller hangs up and our other devices stop ringing.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_decline(
    mut env: JNIEnv,
    _: JClass,
    space: JString,
    room: JString,
) {
    let (Some(s), Some(r)) = (text(&mut env, &space), text(&mut env, &room))
    else {
        return;
    };
    let (Ok(s), Ok(r)) = (s.parse(), r.parse()) else { return };
    tauri::async_runtime::block_on(async {
        if let Some(n) = crate::running() {
            n.decline(&s, r).await;
        }
    });
}

/// Another network: find relays now, not when a ping fails.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_moved(_: JNIEnv, _: JClass) {
    tauri::async_runtime::block_on(async {
        wake::moved().await;
        if let Some(n) = crate::running() {
            n.resume().await;
        }
    });
}

/// No push app: a service waits on the doorbell; a call it brings, shown
/// at once (the rest is caught up after).
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
        let r = rung(dir).await;
        if r.is_err() {
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        r
    })
}

fn text(env: &mut JNIEnv, s: &JString) -> Option<String> {
    (!s.is_null()).then(|| env.get_string(s).ok().map(Into::into))?
}

fn reply<T: Serialize + Default>(
    env: JNIEnv,
    ctx: JObject,
    f: impl Future<Output = anyhow::Result<T>>,
) -> jstring {
    hold(&env, &ctx);
    let r = tauri::async_runtime::block_on(f);
    let r =
        r.inspect_err(|e| tracing::warn!("wake: {e:#}")).unwrap_or_default();
    let json = serde_json::to_string(&r).unwrap_or_default();
    env.new_string(json).map_or(std::ptr::null_mut(), |s| s.into_raw())
}

/// Android's context for native code, where the activity's runtime has
/// not set it.
fn hold(env: &JNIEnv, ctx: &JObject) {
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
}

/// The activity is starting, and tao brings its own context.
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
    let hide = dir.join("private").exists();
    let v = told(dir, push, full).await?;
    Ok(v.into_iter().map(|x| crate::veil(x, hide)).collect())
}

async fn told(
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
    let spaces: Vec<_> = news.iter().map(|x| x.0).collect();
    news.into_iter().for_each(|x| n.deliver(x));
    if crate::FG.load(SeqCst) {
        return Ok(vec![]); // in sight: the app shows it
    }
    if !full {
        // A DM whose call ended stops ringing.
        let mut v = crate::alerts(&n, false);
        // Its caller hears that it rings here (the doorbell said it came).
        for x in v.iter().filter(|x| x.ring) {
            n.ringing(&x.space, x.room).await;
        }
        let ended =
            spaces.into_iter().filter(|s| n.loud(s) && n.calls(s).is_empty());
        v.extend(ended.map(|space| Notice {
            space,
            room: irch_core::state::DM,
            title: String::new(),
            body: String::new(),
            ring: false,
            hush: true,
        }));
        return Ok(v);
    }
    n.resume().await;
    let (mut rx, end) =
        (n.subscribe(), Instant::now() + Duration::from_secs(30));
    while Instant::now() < end
        && matches!(
            tokio::time::timeout(Duration::from_secs(4), rx.recv()).await,
            Ok(Ok(_))
        )
    {}
    let notices = crate::alerts(&n, false);
    // Ringing: up until it is answered (in sight) or ends.
    let ringing = || n.spaces().iter().any(|s| !n.calls(s).is_empty());
    for _ in 0..45 {
        if !ringing() || crate::FG.load(SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    crate::nap().await;
    Ok(notices)
}

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

/// Until the doorbell rings (or half an hour, to look again); `quiet` once
/// irpush carries our rings instead (they come to `Wake.ring`).
/// Never deaf: a running node is handed the news too.
async fn rung(dir: PathBuf) -> anyhow::Result<Shown> {
    static TOLD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let net = crate::config(dir.clone(), None, None)?.net;
    let route = irpush(&dir).await;
    let url = match &route {
        Some(r) => {
            wake::hush(&dir).await;
            r.clone()
        }
        None => wake::doorbell(&dir, &net).await?,
    };
    let told = TOLD.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if told.as_ref() != Some(&url) {
        node(dir.clone()).await?.set_push(Some(url.clone())).await;
        *TOLD.lock().unwrap_or_else(|e| e.into_inner()) = Some(url);
        // A minute to tell the spaces, then asleep (unless in sight).
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            crate::nap().await;
        });
    }
    if route.is_some() {
        return Ok(Shown { quiet: true, ..Default::default() });
    }
    let half = Duration::from_secs(30 * 60);
    let r = tokio::time::timeout(half, wake::rung(&dir, &net)).await;
    let Some(news) = r.ok().transpose()?.flatten() else {
        return Ok(Shown::default());
    };
    Ok(shown(&dir, news))
}

/// Our route through the phone's one irpush device (irpush's library, run
/// by whichever irpush app owns it), once a storage node we know took it
/// into its mesh (looked for at most every six hours).
async fn irpush(dir: &std::path::Path) -> Option<String> {
    static LOOKED: std::sync::Mutex<Option<Instant>> =
        std::sync::Mutex::new(None);
    if let Some(r) = push::shared_route(dir) {
        return Some(r);
    }
    {
        let mut l = LOOKED.lock().unwrap_or_else(|e| e.into_inner());
        if l.is_some_and(|t| t.elapsed() < Duration::from_secs(6 * 3600)) {
            return None;
        }
        *l = Some(Instant::now());
    }
    let n = node(dir.into()).await.ok()?;
    let r = n.irpush().await;
    if r.is_none() {
        crate::nap().await;
    }
    r
}

/// The phone's irpush device id, from irpush's library ("" not joined).
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_share(
    mut env: JNIEnv,
    _: JClass,
    id: JString,
) {
    push::share(text(&mut env, &id));
}

/// A library thread: the next storage node ticket to join with.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_joins<'l>(
    env: JNIEnv<'l>,
    _: JClass<'l>,
) -> jstring {
    let t = push::next_join();
    env.new_string(t).map_or(std::ptr::null_mut(), |s| s.into_raw())
}

#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_joined(
    _: JNIEnv,
    _: JClass,
    ok: jboolean,
) {
    push::joined(ok != 0);
}

/// What irpush brought for irch (`push::parse`): to its node if one runs,
/// else its notices and whether to catch up, as `Wake.rung` says them.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_ring<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    ctx: JObject<'l>,
    dir: JString<'l>,
    from: JString<'l>,
    data: JByteArray<'l>,
) -> jstring {
    let dir = text(&mut env, &dir).unwrap_or_default();
    let dir = irch_core::profile(std::path::Path::new(&dir));
    let from = text(&mut env, &from).and_then(|f| f.parse().ok());
    let b = env.convert_byte_array(&data).unwrap_or_default();
    reply(env, ctx, async move {
        if let (Some(n), Some(from)) = (crate::running(), from) {
            n.hear(from, &b);
            return Ok(Shown::default());
        }
        Ok(match Node::news(&dir, &b) {
            Some(Some(news)) => shown(&dir, news),
            Some(None) => Shown { catch: true, ..Shown::default() },
            None => Shown::default(),
        })
    })
}

/// What a ring brings, to show now, and whether to catch up for it. With
/// irch in use its node takes it; else no node starts (`Node::woken`).
#[derive(serde::Serialize, Default)]
struct Shown {
    show: Vec<Notice>,
    catch: bool,
    /// irpush carries our rings: no doorbell to wait on.
    quiet: bool,
}

fn shown(dir: &std::path::Path, news: News) -> Shown {
    if let Some(n) = crate::running() {
        n.deliver(news);
        return Shown::default();
    }
    let (n, catch) = Node::woken(dir, &news);
    // A call: the node starts as it rings, so it connects as it is answered.
    let catch = catch || n.as_ref().is_some_and(|x| x.ring);
    if catch {
        NEWS.lock().unwrap_or_else(|e| e.into_inner()).push(news);
    }
    let hide = dir.join("private").exists();
    let n = n.filter(|_| !crate::FG.load(SeqCst));
    let show = n.into_iter().map(|x| crate::veil(x, hide)).collect();
    Shown { show, catch, quiet: false }
}
