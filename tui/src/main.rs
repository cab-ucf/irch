mod api;
mod app;
mod service;

use anyhow::Result;
use clap::Parser;
use irch_core::{Config, Net, Node};
use std::path::PathBuf;
use std::time::Duration;

/// Private, peer-to-peer community chat
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Your name, if this is a new identity
    #[arg(long, default_value = "anon")]
    name: String,
    /// Where keys and history live
    #[arg(long)]
    dir: Option<PathBuf>,
    /// Your own iroh relay instead of n0's (repeatable)
    #[arg(long)]
    relay: Vec<iroh::RelayUrl>,
    /// This network only, found by mDNS: no relays, no DHT
    #[arg(long)]
    lan: bool,
    /// Become another device of the person behind this link code
    #[arg(long)]
    link: Option<String>,
    /// Keep other people's ciphertext: a storage node
    #[arg(long)]
    serve: bool,
    /// With --serve: files past 5 GB are kept for whoever gives this
    /// (or IRCH_BIG_PASSWORD, kept out of `ps`)
    #[arg(long)]
    big_password: Option<String>,
    /// No interface: an always-on device of yours
    #[arg(long)]
    headless: bool,
    /// No interface: JSON calls on stdin, answers and events on stdout
    #[arg(long)]
    api: bool,
    /// Keep a storage node running here, from now on (as you)
    #[arg(long)]
    install: bool,
    /// Stop the storage node --install keeps
    #[arg(long)]
    uninstall: bool,
    /// Replace this irch with the latest release, if newer
    #[arg(long)]
    update: bool,
}

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Only release builds (IRCH_RELEASE, from release.yml) update themselves.
async fn updated(exe: &std::path::Path) {
    if option_env!("IRCH_RELEASE").is_none() {
        return std::future::pending().await;
    }
    let mut wait = Duration::from_secs(60);
    loop {
        tokio::time::sleep(wait).await;
        wait = Duration::from_secs(6 * 3600);
        match irch_core::release::replace(exe, VERSION).await {
            Ok(Some(v)) => return eprintln!("irch {v}: restarting"),
            Ok(None) => {}
            Err(e) => eprintln!("update: {e:#}"),
        }
    }
}

/// Ctrl-C, or a service manager's stop (SIGTERM): either leaves cleanly.
async fn stopped() {
    #[cfg(unix)]
    let term = async {
        let k = tokio::signal::unix::SignalKind::terminate();
        match tokio::signal::unix::signal(k) {
            Ok(mut s) => drop(s.recv().await),
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = term => {}
    }
}

fn restart(exe: &std::path::Path) -> Result<()> {
    let args = std::env::args_os().skip(1);
    let mut cmd = std::process::Command::new(exe);
    cmd.args(args);
    #[cfg(unix)]
    return Err(std::os::unix::process::CommandExt::exec(&mut cmd).into());
    #[cfg(not(unix))]
    {
        cmd.spawn()?;
        std::process::exit(0)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let c = Cli::parse();
    // Before any update: Linux names a replaced binary "… (deleted)".
    let exe = std::env::current_exe()?;
    if c.install {
        return service::install(&exe);
    }
    if c.uninstall {
        return service::uninstall();
    }
    if c.update {
        let v = irch_core::release::replace(&exe, VERSION).await?;
        println!(
            "{}",
            v.map_or("irch is up to date".into(), |v| {
                format!("irch {v} is in place")
            })
        );
        return Ok(());
    }
    let net = match (c.lan, c.relay.is_empty()) {
        (true, _) => Net::Lan,
        (_, false) => Net::Relay(c.relay),
        _ => Net::Default,
    };
    let home = if c.serve { "irch-serve" } else { "irch" };
    let dir = c
        .dir
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_default().join(home));
    let (api, ui) = (c.api, !(c.serve || c.headless || c.api));
    let log = tracing_subscriber::fmt().with_ansi(false).with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "warn,n0_mainline=off".into()),
    );
    if ui {
        std::fs::create_dir_all(&dir)?;
        let f = std::fs::File::create(dir.join("irch.log"))?;
        log.with_writer(std::sync::Mutex::new(f)).init();
    } else {
        log.with_writer(std::io::stderr).init();
    }
    let (name, link, serve) = (c.name, c.link, c.serve);
    let notes = std::env::var_os("LOROH_VAULT").map(Into::into);
    let notes = notes.or_else(|| dirs::home_dir().map(|h| h.join("notes")));
    let code_file = dir.join("code");
    let big =
        c.big_password.or_else(|| std::env::var("IRCH_BIG_PASSWORD").ok());
    let n =
        Node::start(Config { dir, name, net, link, serve, notes, big }).await?;
    if let Some(code) = n.storage_code().await {
        std::fs::write(code_file, &code)?;
        // For nixos.nix; unprinted, so no wider-read journal holds it.
        match std::env::var_os("IRCH_CODE_FILE") {
            Some(p) => std::fs::write(p, &code)?,
            None => {
                println!("Storage code (give it to the people you store for):");
                println!("{code}");
            }
        }
    }
    let mut fresh = false;
    if ui {
        app::run(n.clone()).await?;
    } else {
        tokio::select! {
            r = api::run(n.clone()), if api => r?,
            () = stopped() => {}
            () = updated(&exe), if !api => fresh = true,
        }
    }
    n.shutdown().await;
    if fresh {
        restart(&exe)?;
    }
    Ok(())
}
