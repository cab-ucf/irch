//! Releases, every file checked against irch's key (app/tauri.conf.json).

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

/// As it says; its files' signatures then say whether that is so.
pub async fn latest() -> Result<String> {
    let json: serde_json::Value =
        serde_json::from_slice(&get("latest.json").await?)?;
    let v = json["version"].as_str().context("no version")?;
    Ok(v.trim_start_matches('v').to_string())
}

pub async fn newer(ours: &str) -> Result<Option<String>> {
    let v = latest().await?;
    let n = |s: &str| {
        s.split('.').map(|x| x.parse().unwrap_or(0)).collect::<Vec<u64>>()
    };
    Ok((n(&v) > n(ours)).then_some(v))
}

pub async fn fetch(name: &str, version: &str) -> Result<Vec<u8>> {
    let sig = get(&format!("{name}.sig")).await?;
    let bin = get(name).await?;
    check(&bin, &sig, name, version)?;
    Ok(bin)
}

/// Signed as `name` of `version`: no other file, no older release.
pub fn check(bin: &[u8], sig: &[u8], name: &str, version: &str) -> Result<()> {
    signed(KEY, bin, sig, name, version)
}

fn signed(
    key: &str,
    bin: &[u8],
    sig: &[u8],
    name: &str,
    version: &str,
) -> Result<()> {
    let text = |b: &[u8]| {
        Ok::<_, anyhow::Error>(String::from_utf8(BASE64.decode(b)?)?)
    };
    let key = PublicKey::decode(&text(key.as_bytes())?)?;
    let sig = Signature::decode(&text(sig.trim_ascii())?)?;
    key.verify(bin, &sig, false).context("not signed by irch")?;
    let says = |f: String| sig.trusted_comment().split('\t').any(|x| x == f);
    ensure!(
        says(format!("file:{name}")) && says(format!("version:{version}")),
        "signed by irch, but as another file or release"
    );
    Ok(())
}

pub fn cli() -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "windows" => "pc-windows-msvc",
        _ => "unknown-linux-musl",
    };
    let (cpu, exe) = (std::env::consts::ARCH, std::env::consts::EXE_SUFFIX);
    format!("irch-{cpu}-{os}{exe}")
}

pub async fn replace(exe: &Path, ours: &str) -> Result<Option<String>> {
    let Some(v) = newer(ours).await? else { return Ok(None) };
    put(exe, &fetch(&cli(), &v).await?)?;
    Ok(Some(v))
}

/// Swapped in whole, even while running (Windows: the .exe moved aside).
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

#[test]
fn signed_as_this_file_of_this_release_only() {
    // A throwaway key's signature of "hi\n" as f.bin, version 1.2.3.
    let key = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDk4M0Y2QjBBREZFQjlBMTUKUldRVm11dmZDbXMvbUlxNWJKQU9xcG1UdFNMcGxDcnBXcmtzeFBvdmZvZVpsczR0Q1FPaDhqZlcK";
    let sig = b"dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRVm11dmZDbXMvbUhCM2ZNYTYxS0pGNDB3dm1MNEZOTGRpWFJSTnU5ZHV2bDdVQkRSVWFqTkhxdDIzYW9nRlRRRHhHbjBxTVRJUkQxMmxqYXl0TDZzQnlqVGpjVFRDeUEwPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkzMDU3MTEwCWZpbGU6Zi5iaW4JdmVyc2lvbjoxLjIuMwpnWW5VRTF5MndMUmtYbk5qNnhqUW0ycm0ydWU0QlFzOHZiL3pwUlFLQTBaWGo2QW42djFSUTRtazRQMTA0TGorMFBUcmNsVlRXQ1VLSE55ZW5ES1VCdz09Cg==";
    assert!(signed(key, b"hi\n", sig, "f.bin", "1.2.3").is_ok());
    assert!(signed(key, b"hi\n", sig, "g.bin", "1.2.3").is_err());
    assert!(signed(key, b"hi\n", sig, "f.bin", "1.2.4").is_err());
    assert!(signed(key, b"ho\n", sig, "f.bin", "1.2.3").is_err());
    assert!(check(b"hi\n", sig, "f.bin", "1.2.3").is_err(), "not irch's key");
}
