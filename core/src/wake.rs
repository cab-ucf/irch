//! Waking a sleeping phone without Google: it holds one quiet TCP
//! connection to one of its owner's always-on irch servers (at a public
//! address, or where the server's router forwards the port), which sends
//! a byte every few minutes (the phone's kernel answers; irch sleeps) and
//! "wake" when anyone writes. Whoever writes asks the phone's servers, over
//! iroh, until one has it. TCP, because carriers forget idle UDP within a
//! minute or two, and idle TCP only after many; not iroh's relays, which
//! ping every 15 s.

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
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use tokio::time::timeout;
use tracing::warn;

pub const ALPN: &[u8] = b"irch/wake/1";
/// A connection's only traffic while nothing happens, server to phone.
pub const PING: Duration = Duration::from_secs(270);
/// Handshakes a server takes at once.
pub const GATE: usize = 64;
const CONTEXT: &[u8] = b"irch wake 1";
const SLOW: Duration = Duration::from_secs(15);
/// Servers a writer asks, at most.
const SERVERS: usize = 4;

/// One of a phone's servers: where to reach it, the key that must answer,
/// and the token (its storage code's) the phone proves it holds.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Target {
    pub at: Vec<String>,
    pub id: EndpointId,
    pub token: [u8; 16],
}

/// A phone's servers, and the secret a writer must know to wake it (its
/// spaces' members do: it is in the URL they are told).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Saved {
    pub secret: [u8; 16],
    pub targets: Vec<Target>,
}

/// What a phone publishes (as its push endpoint) for others to wake it.
pub fn url(
    device: EndpointId,
    secret: [u8; 16],
    to: &[EndpointAddr],
) -> String {
    code("wake", &(device, secret, to))
}

#[derive(Serialize, Deserialize)]
enum Ask {
    Wake(EndpointId, [u8; 16]),
    /// Where to connect: told only to who holds the token.
    Where([u8; 32]),
}

fn mac(token: &[u8; 16], parts: &[&[u8]]) -> blake3::Hash {
    let key = blake3::hash(token).into();
    blake3::keyed_hash(&key, &[&[CONTEXT], parts].concat().concat())
}

fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |x, (a, b)| x | (a ^ b)) == 0
}

/// A phone connected here, or lately.
#[derive(Debug)]
struct Phone {
    secret: [u8; 16],
    tx: Option<mpsc::Sender<()>>,
    /// Gone at, and whether rung since: told the moment it is back.
    gone: Option<(Instant, bool)>,
}

/// A server's side: its phones, and where it says it is.
#[derive(Default, Debug)]
pub struct Hub {
    phones: Mutex<HashMap<EndpointId, Phone>>,
    at: Mutex<Vec<String>>,
}

impl Hub {
    pub fn at(&self, at: Vec<String>) {
        *self.at.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    /// 1: woken now; 2: between connections, woken once back; 0: not here.
    fn wake(&self, device: &EndpointId, secret: &[u8; 16]) -> u8 {
        let mut p = self.phones.lock().unwrap_or_else(|e| e.into_inner());
        let Some(p) = p.get_mut(device).filter(|p| same(&p.secret, secret))
        else {
            return 0;
        };
        match (&p.tx, &mut p.gone) {
            (Some(tx), _) => {
                let _ = tx.try_send(());
                1
            }
            (_, Some((at, rung))) if at.elapsed() < 8 * SLOW => {
                *rung = true;
                2
            }
            _ => 0,
        }
    }

    /// A phone's connection: it proves its device key and the token, this
    /// server proves its key; then pings, and one wake, until it goes.
    pub async fn phone(
        &self,
        mut s: TcpStream,
        key: &SecretKey,
        token: &[u8; 16],
        gate: OwnedSemaphorePermit,
    ) -> Result<()> {
        s.set_nodelay(true)?;
        let mut hello = [0u8; 64];
        timeout(SLOW, s.read_exact(&mut hello)).await??;
        let device = EndpointId::from_bytes(&hello[..32].try_into()?)?;
        let nonce: [u8; 32] = crate::crypto::rand();
        let sig = key.sign(&[CONTEXT, &hello].concat());
        s.write_all(&[&nonce[..], &sig.to_bytes()].concat()).await?;
        let mut back = [0u8; 64 + 32 + 16];
        timeout(SLOW, s.read_exact(&mut back)).await??;
        let (sig, rest) = back.split_at(64);
        let (proof, secret) = rest.split_at(32);
        let me = key.public();
        let mine = [CONTEXT, b"phone", &nonce, me.as_bytes()];
        let sig = Signature::from_bytes(sig.try_into()?);
        device.verify(&mine.concat(), &sig).context("not that device")?;
        let want = mac(token, &[&nonce, device.as_bytes()]);
        ensure!(same(want.as_bytes(), proof), "not paired here");
        drop(gate);
        let secret: [u8; 16] = secret.try_into()?;
        let (tx, mut rx) = mpsc::channel(1);
        let mine = tx.clone();
        let lock = || self.phones.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut p = lock();
            p.retain(|_, p| p.gone.is_none_or(|g| g.0.elapsed() < PING * 3));
            let was =
                p.insert(device, Phone { secret, tx: Some(tx), gone: None });
            if was.and_then(|w| w.gone).is_some_and(|g| g.1) {
                let _ = p[&device].tx.as_ref().map(|t| t.try_send(()));
            }
        }
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
        // Still this connection's (not a newer one's): gone, for a while.
        let mut p = lock();
        if let Some(p) = p
            .get_mut(&device)
            .filter(|p| p.tx.as_ref().is_some_and(|t| t.same_channel(&mine)))
        {
            (p.tx, p.gone) = (None, Some((Instant::now(), false)));
        }
        end
    }
}

/// Over iroh: where this server takes phones, or wake one.
pub(crate) async fn answer(
    hub: &Hub,
    token: [u8; 16],
    c: Connection,
) -> Result<()> {
    let (mut tx, mut rx) = c.accept_bi().await?;
    let reply = match postcard::from_bytes(&rx.read_to_end(64).await?)? {
        Ask::Where(p) => {
            let want = mac(&token, &[b"where", c.remote_id().as_bytes()]);
            ensure!(same(want.as_bytes(), &p), "not paired here");
            let at = hub.at.lock().unwrap_or_else(|e| e.into_inner()).clone();
            postcard::to_allocvec(&at)?
        }
        Ask::Wake(d, secret) => vec![hub.wake(&d, &secret)],
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
pub async fn find(
    ep: &Endpoint,
    server: EndpointAddr,
    token: &[u8; 16],
) -> Result<Vec<String>> {
    let p = mac(token, &[b"where", ep.id().as_bytes()]);
    Ok(postcard::from_bytes(&ask(ep, server, &Ask::Where(p.into())).await?)?)
}

/// Wake the phone behind `url` through the first of its servers that has
/// it (a few, at a few addresses each, relays https only).
pub async fn ring(ep: &Endpoint, url: &str) -> Result<()> {
    type Url = (EndpointId, [u8; 16], Vec<EndpointAddr>);
    let (device, secret, servers): Url = crate::op::uncode("wake", url)?;
    let mut later = false;
    for s in servers.into_iter().take(SERVERS) {
        let ok = |a: &&iroh::TransportAddr| match a {
            iroh::TransportAddr::Relay(u) => u.scheme() == "https",
            _ => true,
        };
        let addrs = s.addrs.iter().filter(ok).take(8).cloned();
        let s = EndpointAddr::from_parts(s.id, addrs.collect::<Vec<_>>());
        match ask(ep, s, &Ask::Wake(device, secret)).await.as_deref() {
            Ok([1]) => return Ok(()),
            Ok([2]) => later = true,
            Ok(_) => {}
            Err(e) => warn!("wake: {e:#}"),
        }
    }
    ensure!(later, "no server of that phone has it");
    Ok(())
}

/// The phone's side: wait for a wake, holding one connection at a time,
/// to the first server that answers as itself. Silent for three pings,
/// closed, or refused: the next. None answering three rounds: an error
/// (the caller then stays connected itself).
pub async fn wait(key: &SecretKey, saved: &Saved) -> Result<()> {
    ensure!(!saved.targets.is_empty(), "no wake servers");
    for round in 1..=3 {
        for t in &saved.targets {
            for at in &t.at {
                match hold(key, &saved.secret, t, at).await {
                    Ok(()) => return Ok(()),
                    Err(e) => warn!("wake {at}: {e:#}"),
                }
            }
        }
        tokio::time::sleep(SLOW * round).await;
    }
    bail!("no wake server answers")
}

async fn hold(
    key: &SecretKey,
    secret: &[u8; 16],
    t: &Target,
    at: &str,
) -> Result<()> {
    let device = key.public();
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
    let mine = key.sign(&[CONTEXT, b"phone", theirs, t.id.as_bytes()].concat());
    let proof = mac(&t.token, &[theirs, device.as_bytes()]);
    let out = [&mine.to_bytes()[..], proof.as_bytes(), secret];
    s.write_all(&out.concat()).await?;
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
pub fn save(dir: &Path, saved: &Saved) -> Result<()> {
    crate::store::write(&dir.join("wake.json"), &serde_json::to_vec(saved)?)
}

/// What `save` kept, and the device key to hold connections with.
pub fn load(dir: &Path) -> Option<(SecretKey, Saved)> {
    let saved =
        serde_json::from_slice(&std::fs::read(dir.join("wake.json")).ok()?);
    let key: [u8; 32] =
        std::fs::read(dir.join("device")).ok()?.try_into().ok()?;
    Some((SecretKey::from_bytes(&key), saved.ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    /// A server on loopback taking phones with `token`.
    async fn server(hub: Arc<Hub>, key: SecretKey, token: [u8; 16]) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let at = l.local_addr().unwrap().to_string();
        let gate = Arc::new(Semaphore::new(GATE));
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let (hub, key) = (hub.clone(), key.clone());
                let g = gate.clone().acquire_owned().await.unwrap();
                tokio::spawn(
                    async move { hub.phone(s, &key, &token, g).await },
                );
            }
        });
        at
    }

    #[tokio::test]
    async fn only_its_owner_holds_it_and_only_its_server_wakes_it() {
        let (hub, key) = (Arc::new(Hub::default()), SecretKey::generate());
        let phone = SecretKey::generate();
        let (device, secret) = (phone.public(), [5; 16]);
        let at = server(hub.clone(), key.clone(), [7; 16]).await;
        let t = |id, token| Target { at: vec![at.clone()], id, token };
        let five = Duration::from_secs(5);
        // A wrong token: the server hangs up (not holds it).
        let bad = t(key.public(), [8; 16]);
        let wrong = timeout(five, hold(&phone, &secret, &bad, &at)).await;
        assert!(matches!(wrong, Ok(Err(_))));
        assert_eq!(hub.wake(&device, &secret), 0);
        // A server not the one paired: the phone hangs up.
        let other = SecretKey::generate().public();
        let fake = hold(&phone, &secret, &t(other, [7; 16]), &at).await;
        assert!(fake.unwrap_err().to_string().contains("not that server"));
        // Its own: held until woken, and only with its secret.
        let right = t(key.public(), [7; 16]);
        let p = phone.clone();
        let held = tokio::spawn(async move {
            hold(&p, &secret, &right, &right.at[0]).await
        });
        let held_here = || hub.phones.lock().unwrap().contains_key(&device);
        for _ in 0..100 {
            if held_here() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(hub.wake(&device, &[6; 16]), 0, "not without its secret");
        assert_eq!(hub.wake(&device, &secret), 1);
        assert!(timeout(five, held).await.unwrap().unwrap().is_ok());
        // Between connections: rung, then told once it is back.
        for _ in 0..100 {
            if hub.wake(&device, &secret) == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let right = t(key.public(), [7; 16]);
        let back = hold(&phone, &secret, &right, &right.at[0]);
        assert!(timeout(five, back).await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn another_device_cannot_take_its_place() {
        let (hub, key) = (Arc::new(Hub::default()), SecretKey::generate());
        let at = server(hub.clone(), key.clone(), [7; 16]).await;
        let (phone, thief) = (SecretKey::generate(), SecretKey::generate());
        // The thief holds the token, but not the phone's key.
        let mut s = TcpStream::connect(&at).await.unwrap();
        let hello = [phone.public().as_bytes(), &[0u8; 32][..]].concat();
        s.write_all(&hello).await.unwrap();
        let mut back = [0u8; 96];
        s.read_exact(&mut back).await.unwrap();
        let sig = thief.sign(&[CONTEXT, b"phone", &back[..32]].concat());
        let proof = mac(&[7; 16], &[&back[..32], phone.public().as_bytes()]);
        let out = [&sig.to_bytes()[..], proof.as_bytes(), &[5; 16]];
        s.write_all(&out.concat()).await.unwrap();
        let mut b = [0u8; 1];
        let n = timeout(Duration::from_secs(5), s.read(&mut b)).await;
        assert!(matches!(n, Ok(Ok(0)) | Ok(Err(_))), "hung up on");
        assert_eq!(hub.wake(&phone.public(), &[5; 16]), 0);
    }
}
