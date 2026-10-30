//! Android: `IrchPlugin.kt` sees network changes iroh cannot; `Push.kt` wakes.

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
    let cmd = match ask {
        "fix" => Some("allowAlerts"),
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

    pub async fn share_file(&self, path: &std::path::Path) {
        let path = &path.to_string_lossy();
        self.call("shareFile", FilePath { path }).await;
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

/// Until the doorbell rings (or half an hour, to look again). Never deaf:
/// a running node is handed the news too.
async fn rung(dir: PathBuf) -> anyhow::Result<Shown> {
    static TOLD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let net = crate::config(dir.clone(), None, None)?.net;
    let url = wake::doorbell(&dir, &net).await?;
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
    let half = Duration::from_secs(30 * 60);
    let Ok(news) = tokio::time::timeout(half, wake::rung(&dir, &net)).await
    else {
        return Ok(Shown::default());
    };
    let Some(news) = news? else { return Ok(Shown::default()) };
    Ok(shown(&dir, news))
}

/// What a ring brings, to show now, and whether to catch up for it. With
/// irch in use its node takes it; else no node starts (`Node::woken`).
#[derive(serde::Serialize, Default)]
struct Shown {
    show: Vec<Notice>,
    catch: bool,
}

fn shown(dir: &std::path::Path, news: News) -> Shown {
    if let Some(n) = crate::running() {
        n.deliver(news);
        return Shown::default();
    }
    let (n, catch) = Node::woken(dir, &news);
    if catch {
        NEWS.lock().unwrap_or_else(|e| e.into_inner()).push(news);
    }
    let hide = dir.join("private").exists();
    let n = n.filter(|_| !crate::FG.load(SeqCst));
    Shown { show: n.into_iter().map(|x| crate::veil(x, hide)).collect(), catch }
}

/// Firebase: our token to the push servers. False if none took it.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_fcm<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    ctx: JObject<'l>,
    dir: JString<'l>,
    token: JString<'l>,
) -> jboolean {
    hold(&env, &ctx);
    let (dir, token) = (text(&mut env, &dir), text(&mut env, &token));
    let dir =
        irch_core::profile(std::path::Path::new(&dir.unwrap_or_default()));
    let ok = tauri::async_runtime::block_on(async {
        let n = node(dir).await?;
        n.set_push(Some(format!("fcm:{}", token.unwrap_or_default()))).await;
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            crate::nap().await;
        });
        anyhow::Ok(n.pushing())
    });
    ok.unwrap_or(false).into()
}

/// What Google brought: a ring sealed to us (push.rs), opened here.
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_irch_chat_Wake_rang<'l>(
    mut env: JNIEnv<'l>,
    _: JClass<'l>,
    ctx: JObject<'l>,
    dir: JString<'l>,
    data: JString<'l>,
) -> jstring {
    use data_encoding::BASE64;
    let (dir, data) = (text(&mut env, &dir), text(&mut env, &data));
    let dir =
        irch_core::profile(std::path::Path::new(&dir.unwrap_or_default()));
    let b =
        BASE64.decode(data.unwrap_or_default().as_bytes()).unwrap_or_default();
    reply(env, ctx, async move {
        let key = wake::push_key(&dir)?;
        let news = irch_core::push::open(&key, &b).context("not for us")?;
        Ok(shown(&dir, news))
    })
}

/// A push server's Firebase project, found among our storage nodes as
/// irch starts, for Android to take a token from (IrchPlugin.kt).
pub async fn firebase(app: tauri::AppHandle, n: std::sync::Arc<Node>) {
    let Some((f, _)) = n.firebase().await else { return };
    if let Some(b) = app.try_state::<Bridge>() {
        b.call("firebase", f).await;
    }
}
