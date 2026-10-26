//! irch's releases: which is newest, and its files, each checked against
//! the key every irch trusts (app/tauri.conf.json) before anything runs it.

use anyhow::{Context, Result, ensure};
use data_encoding::BASE64;
use minisign_verify::{PublicKey, Signature};
use std::path::Path;

pub const LATEST: &str = "https://github.com/cab-ucf/irch/releases/latest";

const KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDg1Q0MxRkNENzAzQjdGOEYKUldTUGZ6dHd6Ui9NaGFrUkw2Y0E3MHBOMDNuL25NQ2RUdm9ZNVVIQzJJY1p1aGNSKzQxUlFJOTUK";

async fn get(name: &str) -> Result<Vec<u8>> {
    let url = format!("{LATEST}/download/{name}");
    let got = crate::http().get(url).send().await?.error_for_status()?;
    Ok(got.bytes().await?.to_vec())
}

/// The latest release's version, if newer than `ours`.
pub async fn newer(ours: &str) -> Result<Option<String>> {
    let json: serde_json::Value =
        serde_json::from_slice(&get("latest.json").await?)?;
    let v = json["version"].as_str().context("no version")?;
    let v = v.trim_start_matches('v').to_string();
    let n = |s: &str| {
        s.split('.').map(|x| x.parse().unwrap_or(0)).collect::<Vec<u64>>()
    };
    Ok((n(&v) > n(ours)).then_some(v))
}

/// A release's file, if its signature is ours.
pub async fn fetch(name: &str) -> Result<Vec<u8>> {
    let sig = get(&format!("{name}.sig")).await?;
    let bin = get(name).await?;
    check(&bin, &sig)?;
    Ok(bin)
}

/// Whether `sig` (a `.sig` file, as releases carry) signs `bin` with
/// irch's key.
pub fn check(bin: &[u8], sig: &[u8]) -> Result<()> {
    let text = |b: &[u8]| {
        Ok::<_, anyhow::Error>(String::from_utf8(BASE64.decode(b)?)?)
    };
    let key = PublicKey::decode(&text(KEY.as_bytes())?)?;
    let sig = Signature::decode(&text(sig.trim_ascii())?)?;
    key.verify(bin, &sig, false).context("not signed by irch")
}

/// This computer's terminal client and storage node, as released.
pub fn cli() -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        _ => "unknown-linux-musl",
    };
    let (cpu, exe) = (std::env::consts::ARCH, std::env::consts::EXE_SUFFIX);
    format!("irch-{cpu}-{os}{exe}")
}

/// The latest CLI in place of `exe`, if newer than `ours`.
pub async fn replace(exe: &Path, ours: &str) -> Result<Option<String>> {
    let Some(v) = newer(ours).await? else { return Ok(None) };
    put(exe, &fetch(&cli()).await?)?;
    Ok(Some(v))
}

/// `bin` at `exe`, swapped in whole, even while it runs (a running
/// Windows .exe is moved aside first).
pub fn put(exe: &Path, bin: &[u8]) -> Result<()> {
    let new = exe.with_extension("new");
    std::fs::write(&new, bin)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
    }
    if cfg!(windows) && exe.exists() {
        let old = exe.with_extension("old");
        let _ = std::fs::remove_file(&old);
        std::fs::rename(exe, old)?;
    }
    std::fs::rename(&new, exe)?;
    ensure!(exe.exists(), "not replaced");
    Ok(())
}
