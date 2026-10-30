//! Waking a sleeping phone without Google, router or server: a doorbell
//! endpoint idles on its home relay; writers ring it with a secret and the
//! sealed op, and it wakes, tells of it and catches up.
//!
//! Each space gets its own ring code, from the doorbell's secret, the space
//! and its epoch: after a rekey, whoever it left out rings nothing.

use crate::crypto::{kdf, same};
use crate::op::{code, uncode};
use crate::{Id, Net};
use anyhow::{Result, bail, ensure};
use iroh::endpoint::Incoming;
use iroh::{Endpoint, EndpointAddr, SecretKey, TransportAddr};
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;

pub const ALPN: &[u8] = b"irch/wake/1";
const SLOW: Duration = Duration::from_secs(15);
/// A space's id and one sealed op.
pub(crate) const MAX: usize = 64 * 1024;
/// token ‖ space ‖ epoch tag
pub(crate) const HEAD: usize = 16 + 32 + 8;

pub type News = (crate::Id, Vec<u8>);

type Bell = (Endpoint, [u8; 16]);

/// One per profile folder, bound once.
static BELLS: Mutex<Option<HashMap<PathBuf, Bell>>> = Mutex::const_new(None);

async fn bell(dir: &Path, net: &Net) -> Result<Bell> {
    let mut bells = BELLS.lock().await;
    let bells = bells.get_or_insert_default();
    if let Some(b) = bells.get(dir) {
        return Ok(b.clone());
    }
    let k = kept(dir)?;
    let key = SecretKey::from_bytes(&k[..32].try_into()?);
    let b = crate::node::endpoint(net).secret_key(key);
    let ep = b.alpns(vec![ALPN.to_vec()]).bind().await?;
    let bell = (ep, k[32..].try_into()?);
    bells.insert(dir.into(), bell.clone());
    Ok(bell)
}

/// The doorbell's key and secret, kept so its address and codes hold.
fn kept(dir: &Path) -> Result<Vec<u8>> {
    let f = dir.join("bell");
    match std::fs::read(&f) {
        Ok(k) if k.len() == 48 => Ok(k),
        _ => {
            std::fs::create_dir_all(dir)?;
            let k = crate::crypto::rand::<48>().to_vec();
            crate::store::write(&f, &k)?;
            Ok(k)
        }
    }
}

/// A space's ring token for a push server to know us by, from the same
/// secret: a rekey retires it as it does the doorbell's.
pub fn push_token(dir: &Path, space: &Id, tag: &[u8; 8]) -> Result<[u8; 16]> {
    let k = kept(dir)?;
    Ok(token(&k[32..].try_into()?, space, tag))
}

/// The key to our mailbox on push servers, from the same secret.
pub fn mailbox(dir: &Path) -> Result<[u8; 16]> {
    let k = kept(dir)?;
    Ok(kdf("irch mailbox", &[&k[32..]])[..16].try_into()?)
}

pub async fn moved() {
    let bells = BELLS.lock().await.clone().unwrap_or_default();
    for (ep, _) in bells.values() {
        ep.network_change().await;
    }
}

fn token(secret: &[u8; 16], space: &Id, tag: &[u8; 8]) -> [u8; 16] {
    let k = kdf("irch bell", &[secret, &space.0, tag]);
    k[..16].try_into().expect("16 of 32")
}

/// The ring code a space's members get, from our doorbell's own code.
pub fn for_space(bell: &str, space: &Id, tag: &[u8; 8]) -> Option<String> {
    let (at, secret): (EndpointAddr, [u8; 16]) = uncode("wake", bell).ok()?;
    Some(code("ring", &(at, *tag, token(&secret, space, tag))))
}

/// The epoch each space's ring code is for now; older codes ring nothing.
pub fn allow(dir: &Path, space: &Id, tag: &[u8; 8]) -> Result<()> {
    let f = dir.join("bells");
    let mut all: BTreeMap<Id, [u8; 8]> = crate::store::load(&f);
    if all.insert(*space, *tag) != Some(*tag) {
        crate::store::save(&f, &all)?;
    }
    Ok(())
}

/// Our doorbell, as our own code (with its secret; never posted).
pub async fn doorbell(dir: &Path, net: &Net) -> Result<String> {
    let (ep, secret) = bell(dir, net).await?;
    if matches!(net, Net::Default | Net::Relay(_)) {
        let _ = timeout(SLOW, ep.online()).await; // a relay, to be found by
    }
    // Through a relay, the relay alone: a phone's own addresses change with
    // every network, and each change would retell every space.
    let mut at = ep.addr();
    if at.addrs.iter().any(|a| a.is_relay()) {
        at.addrs.retain(|a| a.is_relay());
    }
    Ok(code("wake", &(at, secret)))
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
                    let bells = crate::store::load(&dir.join("bells"));
                    rings.spawn(timeout(SLOW, answer(inc, secret, bells)));
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

/// `Some` if rung with a space's current code, then its news.
async fn answer(
    inc: Incoming,
    secret: [u8; 16],
    bells: BTreeMap<Id, [u8; 8]>,
) -> Result<Option<Option<News>>> {
    let c = inc.await?;
    let (mut tx, mut rx) = c.accept_bi().await?;
    let mut head = [0u8; HEAD];
    rx.read_exact(&mut head).await?;
    let (got, rest) = head.split_at(16);
    let space = Id(rest[..32].try_into().expect("32"));
    let tag: [u8; 8] = rest[32..].try_into().expect("8");
    let now = bells.get(&space) == Some(&tag);
    let ok = now && same(got, &token(&secret, &space, &tag));
    let news = match ok {
        true => crate::sync::rest(&mut rx, MAX).await?,
        false => vec![],
    };
    tx.write_all(&[ok as u8]).await?;
    tx.finish()?;
    let _ = timeout(SLOW, c.closed()).await;
    Ok(ok.then(|| (!news.is_empty()).then_some((space, news))))
}

/// Relays https only: any member names them.
pub async fn ring(
    ep: &Endpoint,
    url: &str,
    space: &Id,
    raw: Option<&[u8]>,
) -> Result<()> {
    let (to, tag, token): (EndpointAddr, [u8; 8], [u8; 16]) =
        uncode("ring", url)?;
    // With a relay to meet at, no private address: a member could aim
    // every ringer at a LAN. Without one (LAN only), any.
    let relayed = to.addrs.iter().any(|a| a.is_relay());
    let ok = |a: &&TransportAddr| match a {
        TransportAddr::Relay(u) => u.scheme() == "https",
        TransportAddr::Ip(a) => !relayed || crate::node::public(&a.ip()),
        _ => false,
    };
    let addrs: Vec<_> = to.addrs.iter().filter(ok).take(8).cloned().collect();
    let go = async {
        let c =
            ep.connect(EndpointAddr::from_parts(to.id, addrs), ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        let mut m = [&token[..], &space.0, &tag].concat();
        if let Some(raw) = raw.filter(|r| r.len() <= MAX) {
            m.extend_from_slice(raw);
        }
        tx.write_all(&m).await?;
        tx.finish()?;
        let got = rx.read_to_end(1).await?;
        c.close(0u8.into(), b"");
        ensure!(got == [1], "not that doorbell's code, or an old one");
        Ok(())
    };
    timeout(SLOW, go).await?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rings_only_with_a_current_code() {
        let dir =
            std::env::temp_dir().join(format!("irch-bell-{}", crate::now()));
        let bell = doorbell(&dir, &Net::Direct).await.unwrap();
        assert_eq!(bell, doorbell(&dir, &Net::Direct).await.unwrap(), "kept");
        let ep = crate::node::endpoint(&Net::Direct).bind().await.unwrap();
        let (sp, other) = (Id([7; 32]), Id([8; 32]));
        let (old, new) =
            (for_space(&bell, &sp, &[1; 8]), for_space(&bell, &sp, &[2; 8]));
        let (old, new) = (old.unwrap(), new.unwrap());
        allow(&dir, &sp, &[1; 8]).unwrap();
        allow(&dir, &sp, &[2; 8]).unwrap();
        let d = dir.clone();
        let wait = tokio::spawn(async move { rung(&d, &Net::Direct).await });
        let raw = b"sealed".as_slice();
        assert!(ring(&ep, &old, &sp, None).await.is_err(), "rekeyed away");
        assert!(ring(&ep, &new, &other, None).await.is_err(), "another space");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!wait.is_finished(), "a wrong code rings nothing");
        ring(&ep, &new, &sp, Some(raw)).await.unwrap();
        let woke = timeout(Duration::from_secs(5), wait).await;
        let got = woke.unwrap().unwrap().unwrap().expect("what was written");
        assert_eq!((got.0, &got.1[..]), (sp, raw), "handed over");
        let _ = std::fs::remove_dir_all(dir);
    }
}
