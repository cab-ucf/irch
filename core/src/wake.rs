//! Waking a sleeping phone without Google, router or server: a doorbell
//! endpoint idles on its home relay; writers ring it with a secret and the
//! sealed op, and it wakes, tells of it and catches up.

use crate::Net;
use crate::op::code;
use anyhow::{Result, bail, ensure};
use iroh::endpoint::Incoming;
use iroh::{Endpoint, EndpointAddr, SecretKey, TransportAddr};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;

pub const ALPN: &[u8] = b"irch/wake/1";
const SLOW: Duration = Duration::from_secs(15);
/// A space's id and one sealed op.
const MAX: usize = 64 * 1024;

pub type News = (crate::Id, Vec<u8>);

type Bell = (Endpoint, [u8; 16]);

/// One per profile folder, bound once.
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
    // Kept, so the address holds.
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

pub async fn moved() {
    let bells = BELLS.lock().await.clone().unwrap_or_default();
    for (ep, _) in bells.values() {
        ep.network_change().await;
    }
}

pub async fn doorbell(dir: &Path, net: &Net) -> Result<String> {
    let (ep, secret) = bell(dir, net).await?;
    if matches!(net, Net::Default | Net::Relay(_)) {
        let _ = timeout(SLOW, ep.online()).await; // a relay, to be found by
    }
    Ok(code("wake", &(ep.addr(), secret)))
}

/// Until rung with the secret; with news, unless an older irch rang bare.
/// Ringers are heard at once: one that stalls holds up nobody, and past
/// 32 at once the rest are refused.
pub async fn rung(dir: &Path, net: &Net) -> Result<Option<News>> {
    let (ep, secret) = bell(dir, net).await?;
    let mut rings = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            inc = ep.accept() => {
                let Some(inc) = inc else { bail!("doorbell closed") };
                if rings.len() >= 32 {
                    inc.refuse();
                } else {
                    rings.spawn(timeout(SLOW, answer(inc, secret)));
                }
            }
            // Matched inside: a wrong ring must not stall the next one.
            Some(r) = rings.join_next() => {
                if let Ok(Ok(Ok(Some(news)))) = r {
                    return Ok(news);
                }
            }
        }
    }
}

/// `Some` if rung with the secret, then its news.
async fn answer(
    inc: Incoming,
    secret: [u8; 16],
) -> Result<Option<Option<News>>> {
    let c = inc.await?;
    let (mut tx, mut rx) = c.accept_bi().await?;
    let mut got = [0u8; 16];
    rx.read_exact(&mut got).await?;
    let ok = same(&got, &secret);
    let news = match ok {
        true => rx.read_to_end(MAX).await?,
        false => vec![],
    };
    tx.write_all(&[ok as u8]).await?;
    tx.finish()?;
    let _ = timeout(SLOW, c.closed()).await;
    let news = (news.len() > 32).then(|| {
        let id = crate::Id(news[..32].try_into().expect("32"));
        (id, news[32..].to_vec())
    });
    Ok(ok.then_some(news))
}

/// Relays https only: any member names them, so none is trusted.
pub async fn ring(ep: &Endpoint, url: &str, news: Option<&News>) -> Result<()> {
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
        let mut m = secret.to_vec();
        if let Some((id, raw)) = news.filter(|n| 32 + n.1.len() <= MAX) {
            m.extend_from_slice(&id.0);
            m.extend_from_slice(raw);
        }
        tx.write_all(&m).await?;
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
        assert!(ring(&ep, &wrong, None).await.is_err());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!wait.is_finished(), "a wrong secret rings nothing");
        let news = (crate::Id([7; 32]), b"sealed".to_vec());
        ring(&ep, &url, Some(&news)).await.unwrap();
        let woke = timeout(Duration::from_secs(5), wait).await;
        let got = woke.unwrap().unwrap().unwrap().expect("what was written");
        assert_eq!((got.0.0, got.1), (news.0.0, news.1), "handed over");
        let _ = std::fs::remove_dir_all(dir);
    }
}
