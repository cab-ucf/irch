//! Waking a sleeping phone without Google: it holds one quiet TCP
//! connection to one of its owner's always-on irch servers, which sends a
//! byte every few minutes (the phone's kernel answers; irch sleeps) and
//! "wake" when anyone writes. Whoever writes asks the phone's servers, over
//! iroh, until one has it. TCP, because carriers forget idle UDP within a
//! minute or two, and idle TCP only after many.

use crate::op::code;
use anyhow::{Context, Result, bail, ensure};
use iroh::endpoint::Connection;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::warn;

pub const ALPN: &[u8] = b"irch/wake/1";
/// A connection's only traffic while nothing happens, server to phone.
pub const PING: Duration = Duration::from_secs(270);
const CONTEXT: &[u8] = b"irch wake 1";
const SLOW: Duration = Duration::from_secs(15);

/// One of a phone's servers: where to reach it, the key that must answer,
/// and the token (its storage code's) the phone proves it holds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Target {
    pub at: Vec<String>,
    pub id: EndpointId,
    pub token: [u8; 16],
}

/// What a phone publishes (as its push endpoint) for others to wake it.
pub fn url(device: EndpointId, servers: &[EndpointAddr]) -> String {
    code("wake", &(device, servers))
}

#[derive(Serialize, Deserialize)]
enum Ask {
    Wake(EndpointId),
    Where,
}

fn proof(token: &[u8; 16], nonce: &[u8], device: &EndpointId) -> blake3::Hash {
    let key = blake3::hash(token).into();
    blake3::keyed_hash(&key, &[CONTEXT, nonce, device.as_bytes()].concat())
}

/// A server's side: the phones connected here, and where it says it is.
#[derive(Default, Debug)]
pub struct Hub {
    phones: Mutex<HashMap<EndpointId, mpsc::Sender<()>>>,
    woke: Mutex<HashMap<EndpointId, Instant>>,
    at: Mutex<Vec<String>>,
}

impl Hub {
    pub fn at(&self, at: Vec<String>) {
        *self.at.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    /// Tell `device` to wake, if it is connected here (twice a second at
    /// most: a flood wakes it once).
    fn wake(&self, device: &EndpointId) -> bool {
        let phones = self.phones.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tx) = phones.get(device) else { return false };
        let mut w = self.woke.lock().unwrap_or_else(|e| e.into_inner());
        let soon = w.get(device).is_some_and(|t| t.elapsed() < SLOW / 30);
        if !soon {
            w.insert(*device, Instant::now());
            let _ = tx.try_send(());
        }
        true
    }

    /// A phone's connection: it proves its token, this server proves its
    /// key; then pings, and wakes, until it goes.
    pub async fn phone(
        &self,
        mut s: TcpStream,
        key: &SecretKey,
        token: &[u8; 16],
    ) -> Result<()> {
        s.set_nodelay(true)?;
        let mut hello = [0u8; 64];
        timeout(SLOW, s.read_exact(&mut hello)).await??;
        let device = EndpointId::from_bytes(&hello[..32].try_into()?)?;
        let nonce: [u8; 32] = crate::crypto::rand();
        let sig = key.sign(&[CONTEXT, &hello].concat());
        s.write_all(&[&nonce[..], &sig.to_bytes()].concat()).await?;
        let mut mac = [0u8; 32];
        timeout(SLOW, s.read_exact(&mut mac)).await??;
        ensure!(proof(token, &nonce, &device) == mac, "not paired here");
        let (tx, mut rx) = mpsc::channel(1);
        let lock = || self.phones.lock().unwrap_or_else(|e| e.into_inner());
        lock().insert(device, tx.clone());
        let (mut r, mut w) = s.into_split();
        let mut b = [0u8; 1];
        let end = loop {
            let byte = tokio::select! {
                _ = rx.recv() => 1,
                _ = tokio::time::sleep(PING) => 0,
                _ = r.read(&mut b) => break Ok(()), // gone (it never speaks)
            };
            if let Err(e) = w.write_all(&[byte]).await {
                break Err(e.into());
            }
        };
        let mut p = lock();
        if p.get(&device).is_some_and(|t| t.same_channel(&tx)) {
            p.remove(&device);
        }
        end
    }
}

/// Over iroh: where this server's wake port is, or wake a phone here.
pub(crate) async fn answer(hub: &Hub, c: Connection) -> Result<()> {
    let (mut tx, mut rx) = c.accept_bi().await?;
    let reply = match postcard::from_bytes(&rx.read_to_end(64).await?)? {
        Ask::Where => {
            let at = hub.at.lock().unwrap_or_else(|e| e.into_inner()).clone();
            postcard::to_allocvec(&at)?
        }
        Ask::Wake(d) => vec![hub.wake(&d) as u8],
    };
    tx.write_all(&reply).await?;
    tx.finish()?;
    let _ = timeout(SLOW, c.closed()).await;
    Ok(())
}

async fn ask(ep: &Endpoint, to: EndpointAddr, q: &Ask) -> Result<Vec<u8>> {
    let go = async {
        let c = ep.connect(to, ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(q)?).await?;
        tx.finish()?;
        let got = rx.read_to_end(4096).await?;
        c.close(0u8.into(), b"");
        anyhow::Ok(got)
    };
    timeout(SLOW, go).await?
}

/// Where `server` takes phones' connections (none: it does not).
pub async fn find(ep: &Endpoint, server: EndpointAddr) -> Result<Vec<String>> {
    Ok(postcard::from_bytes(&ask(ep, server, &Ask::Where).await?)?)
}

/// Wake the phone behind `url` (from `url`): through the first of its
/// servers that has it.
pub async fn ring(ep: &Endpoint, url: &str) -> Result<()> {
    let (device, servers): (EndpointId, Vec<EndpointAddr>) =
        crate::op::uncode("wake", url)?;
    for s in servers {
        match ask(ep, s, &Ask::Wake(device)).await {
            Ok(b) if b == [1] => return Ok(()),
            Ok(_) => {}
            Err(e) => warn!("wake: {e:#}"),
        }
    }
    bail!("no server of that phone has it")
}

/// The phone's side: wait for a wake, holding one connection at a time,
/// to the first server that answers as itself. Silent for three pings,
/// closed, or refused: the next. None answering three rounds: an error
/// (the caller then stays connected itself).
pub async fn wait(device: EndpointId, targets: &[Target]) -> Result<()> {
    ensure!(!targets.is_empty(), "no wake servers");
    for round in 1..=3 {
        for t in targets {
            for at in &t.at {
                match hold(device, t, at).await {
                    Ok(()) => return Ok(()),
                    Err(e) => warn!("wake {at}: {e:#}"),
                }
            }
        }
        tokio::time::sleep(SLOW * round).await;
    }
    bail!("no wake server answers")
}

async fn hold(device: EndpointId, t: &Target, at: &str) -> Result<()> {
    let mut s = timeout(SLOW, TcpStream::connect(at)).await??;
    let nonce: [u8; 32] = crate::crypto::rand();
    let hello = [device.as_bytes(), &nonce[..]].concat();
    s.write_all(&hello).await?;
    let mut back = [0u8; 96];
    timeout(SLOW, s.read_exact(&mut back)).await??;
    let (theirs, sig) = back.split_at(32);
    let sig = Signature::from_bytes(sig.try_into()?);
    t.id.verify(&[CONTEXT, &hello].concat(), &sig)
        .context("not that server")?;
    s.write_all(proof(&t.token, theirs, &device).as_bytes()).await?;
    let mut b = [0u8; 1];
    loop {
        match timeout(PING * 3, s.read(&mut b)).await?? {
            0 => bail!("closed"),
            _ if b[0] == 1 => return Ok(()),
            _ => {}
        }
    }
}

/// The phone's servers, kept where its background service finds them
/// without starting irch.
pub fn save(dir: &Path, device: EndpointId, t: &[Target]) -> Result<()> {
    let f = dir.join("wake.json");
    if t.is_empty() {
        let _ = std::fs::remove_file(f);
        return Ok(());
    }
    Ok(std::fs::write(f, serde_json::to_vec(&(device, t))?)?)
}

pub fn load(dir: &Path) -> Option<(EndpointId, Vec<Target>)> {
    serde_json::from_slice(&std::fs::read(dir.join("wake.json")).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A server on loopback taking one phone with `token`.
    async fn server(hub: Arc<Hub>, key: SecretKey, token: [u8; 16]) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let (hub, key) = (hub.clone(), key.clone());
                tokio::spawn(async move { hub.phone(s, &key, &token).await });
            }
        });
        at
    }

    #[tokio::test]
    async fn only_its_owner_holds_it_and_only_its_server_wakes_it() {
        let (hub, key) = (Arc::new(Hub::default()), SecretKey::generate());
        let device = SecretKey::generate().public();
        let at = server(hub.clone(), key.clone(), [7; 16]).await;
        let t = |id, token| Target { at: vec![at.clone()], id, token };
        // A wrong token: the server hangs up (not holds it).
        let bad = t(key.public(), [8; 16]);
        let wrong = timeout(Duration::from_secs(5), hold(device, &bad, &at));
        let wrong = wrong.await;
        assert!(matches!(wrong, Ok(Err(_))) && !hub.wake(&device));
        // A server not the one paired: the phone hangs up.
        let other = SecretKey::generate().public();
        let fake = hold(device, &t(other, [7; 16]), &at).await;
        assert!(fake.unwrap_err().to_string().contains("not that server"));
        // Its own: held until woken.
        let right = t(key.public(), [7; 16]);
        let phone =
            tokio::spawn(async move { hold(device, &right, &at).await });
        for _ in 0..100 {
            if hub.wake(&device) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let woke = timeout(Duration::from_secs(5), phone).await;
        assert!(woke.unwrap().unwrap().is_ok());
    }
}
