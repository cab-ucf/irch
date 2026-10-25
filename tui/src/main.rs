//! irch in a terminal; or headless, as a storage node or an always-on
//! device of yours.

mod app;

use anyhow::Result;
use clap::Parser;
use irch_core::{Config, Net, Node};
use std::path::PathBuf;

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
    /// No interface: an always-on device of yours
    #[arg(long)]
    headless: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let c = Cli::parse();
    let net = match (c.lan, c.relay.is_empty()) {
        (true, _) => Net::Lan,
        (_, false) => Net::Relay(c.relay),
        _ => Net::Default,
    };
    let home = if c.serve { "irch-serve" } else { "irch" };
    let dir = c
        .dir
        .unwrap_or_else(|| dirs::data_dir().unwrap_or_default().join(home));
    let ui = !(c.serve || c.headless);
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
    let n = Node::start(Config { dir, name, net, link, serve, notes }).await?;
    if let Some(code) = n.storage_code().await {
        println!("Storage code (give it to the people you store for):");
        println!("{code}");
    }
    if ui {
        app::run(n.clone()).await?;
    } else {
        tokio::signal::ctrl_c().await?;
    }
    n.shutdown().await;
    Ok(())
}
