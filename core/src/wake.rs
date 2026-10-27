//! Waking a sleeping phone without Google, a router or a server: out of
//! sight, irch keeps only a doorbell, an iroh endpoint of its own with no
//! connection open (iroh keeps one quiet link to its home relay, through
//! which anyone reaches it by key). Its spaces are told where it is and a
//! secret; whoever writes rings it, and it wakes to catch up.

use crate::Net;
use crate::op::code;
use anyhow::{Result, bail, ensure};
use iroh::{Endpoint, EndpointAddr, SecretKey, TransportAddr};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;

pub const ALPN: &[u8] = b"irch/wake/1";
const SLOW: Duration = Duration::from_secs(15);

type Bell = (Endpoint, [u8; 16]);

/// Doorbells by folder: one per profile, bound once.
static BELLS: Mutex<Option<HashMap<PathBuf, Bell>>> = Mutex::const_new(None);

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |x, (a, b)| x | (a ^ b)) == 0
}

async fn bell(dir: &Path, net: &Net) -> Result<Bell> {
    let mut bells = BELLS.lock().await;
    let bells = bells.get_or_insert_default();
    if let Some(b) = bells.get(dir) {
        return Ok(b.clone());
    }
    // Its key and the secret that rings it: kept, so the address holds.
    let f = dir.join("bell");
    let k = match std::fs::read(&f) {
        Ok(k) if k.len() == 48 => k,
        _ => {
            std::fs::create_dir_all(dir)?;
            let k = crate::crypto::rand::<48>().to_vec();
            crate::store::write(&f, &k)?;
            k
        }
    };
    let key = SecretKey::from_bytes(&k[..32].try_into()?);
    let b = crate::node::endpoint(net).secret_key(key);
    let ep = b.alpns(vec![ALPN.to_vec()]).bind().await?;
    let bell = (ep, k[32..].try_into()?);
    bells.insert(dir.into(), bell.clone());
    Ok(bell)
}

/// This device's doorbell, as its spaces are told it (its push URL).
pub async fn doorbell(dir: &Path, net: &Net) -> Result<String> {
    let (ep, secret) = bell(dir, net).await?;
    if matches!(net, Net::Default | Net::Relay(_)) {
        let _ = timeout(SLOW, ep.online()).await; // a relay, to be found by
    }
    Ok(code("wake", &(ep.addr(), secret)))
}

/// Until someone rings who knows the secret.
pub async fn rung(dir: &Path, net: &Net) -> Result<()> {
    let (ep, secret) = bell(dir, net).await?;
    loop {
        let Some(inc) = ep.accept().await else { bail!("doorbell closed") };
        let answer = async {
            let c = inc.await?;
            let (mut tx, mut rx) = c.accept_bi().await?;
            let mut got = [0u8; 16];
            rx.read_exact(&mut got).await?;
            let ok = same(&got, &secret);
            tx.write_all(&[ok as u8]).await?;
            tx.finish()?;
            let _ = timeout(SLOW, c.closed()).await;
            anyhow::Ok(ok)
        };
        if let Ok(Ok(true)) = timeout(SLOW, answer).await {
            return Ok(());
        }
    }
}

/// Ring the doorbell behind `url` (at a few of its addresses, relays
/// https only: whoever names them is a member, not trusted).
pub async fn ring(ep: &Endpoint, url: &str) -> Result<()> {
    let (to, secret): (EndpointAddr, [u8; 16]) =
        crate::op::uncode("wake", url)?;
    let ok = |a: &&TransportAddr| match a {
        TransportAddr::Relay(u) => u.scheme() == "https",
        _ => true,
    };
    let addrs: Vec<_> = to.addrs.iter().filter(ok).take(8).cloned().collect();
    let go = async {
        let c =
            ep.connect(EndpointAddr::from_parts(to.id, addrs), ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&secret).await?;
        tx.finish()?;
        let got = rx.read_to_end(1).await?;
        c.close(0u8.into(), b"");
        ensure!(got == [1], "not that doorbell's secret");
        Ok(())
    };
    timeout(SLOW, go).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rings_only_with_its_secret() {
        let dir =
            std::env::temp_dir().join(format!("irch-bell-{}", crate::now()));
        let url = doorbell(&dir, &Net::Direct).await.unwrap();
        assert_eq!(url, doorbell(&dir, &Net::Direct).await.unwrap(), "kept");
        let ep = crate::node::endpoint(&Net::Direct).bind().await.unwrap();
        let (to, _): (EndpointAddr, [u8; 16]) =
            crate::op::uncode("wake", &url).unwrap();
        let wrong = code("wake", &(to, [0u8; 16]));
        let d = dir.clone();
        let wait = tokio::spawn(async move { rung(&d, &Net::Direct).await });
        assert!(ring(&ep, &wrong).await.is_err());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!wait.is_finished(), "a wrong secret rings nothing");
        ring(&ep, &url).await.unwrap();
        let woke = timeout(Duration::from_secs(5), wait).await;
        assert!(woke.unwrap().unwrap().is_ok());
        let _ = std::fs::remove_dir_all(dir);
    }
}
