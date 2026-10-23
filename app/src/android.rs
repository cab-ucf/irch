//! Android, through `IrchPlugin.kt` in `gen/android`: network changes
//! (which iroh cannot see from native code there), mention notifications,
//! the share sheet, and the opt-in background service.

use irch_core::Node;
use serde::Serialize;
use std::sync::Arc;
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
struct Mention<'a> {
    tag: &'a str,
    title: &'a str,
    body: &'a str,
}

#[derive(Serialize)]
struct Text<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct FilePath<'a> {
    path: &'a str,
}

#[derive(Serialize)]
struct On {
    on: bool,
}

impl Bridge {
    /// Every network change: re-probe, re-dial, catch up.
    pub async fn watch_network(&self, n: Arc<Node>) {
        let channel = Channel::new(move |_| {
            let n = n.clone();
            tauri::async_runtime::spawn(async move { n.resume().await });
            Ok(())
        });
        self.call("watchNetwork", Watch { channel }).await;
    }

    /// Shown only while irch is in the background.
    pub async fn mention(&self, tag: &str, title: &str, body: &str) {
        self.call("mention", Mention { tag, title, body }).await;
    }

    pub async fn share(&self, text: &str) {
        self.call("share", Text { text }).await;
    }

    pub async fn share_file(&self, path: &std::path::Path) {
        let path = &path.to_string_lossy();
        self.call("shareFile", FilePath { path }).await;
    }

    pub async fn background(&self, on: bool) {
        self.call("background", On { on }).await;
    }

    async fn call(&self, cmd: &str, args: impl Serialize) {
        if let Err(e) = self.0.run_mobile_plugin_async::<()>(cmd, args).await {
            tracing::warn!("android {cmd}: {e}");
        }
    }
}
