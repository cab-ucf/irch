//! Push servers: every storage node with a relay is one, through irpush.
//! A phone pairs with one it knows (its own first) once, then holds one
//! quiet socket, shared, with heartbeats as long as its network allows,
//! and gives it `irpush:DEVICE` and a ring token per space and epoch
//! (wake.rs). Members ring with those; each ring reaches the phone sealed
//! end to end (post-quantum), always the same size. The push server learns
//! when, and for which space, nothing else. No Google: no Firebase.

use crate::wake::{HEAD, MAX, News};
use crate::{Id, Net, node::lock, store};
use anyhow::{Result, ensure};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const ALPN: &[u8] = b"irch/push/4";
/// Ring tokens a phone may have here: one per space it is in.
const MOST: usize = 4096;
const SLOW: Duration = Duration::from_secs(15);
/// irpush's address for irch on a phone.
const APP: &str = "dev.irch.chat";

#[derive(Serialize, Deserialize)]
enum Ask {
    /// A one-time irpush ticket, to pair with.
    Ticket,
    /// `irpush:DEVICE` (paired here) and our ring tokens.
    Register(String, Vec<[u8; 16]>),
}

async fn ask(ep: &Endpoint, to: EndpointAddr, a: &Ask) -> Result<Vec<u8>> {
    let go = async {
        let c = ep.connect(to, ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(a)?).await?;
        tx.finish()?;
        let got = crate::sync::rest(&mut rx, 4096).await?;
        c.close(0u8.into(), b"");
        Ok(got)
    };
    tokio::time::timeout(SLOW, go).await?
}

/// A ticket to pair our irpush device with `to`.
pub async fn ticket(ep: &Endpoint, to: EndpointAddr) -> Result<String> {
    Ok(String::from_utf8(ask(ep, to, &Ask::Ticket).await?)?)
}

pub async fn register(
    ep: &Endpoint,
    to: EndpointAddr,
    route: &str,
    rings: &[[u8; 16]],
) -> Result<()> {
    let a = Ask::Register(route.into(), rings.to_vec());
    ensure!(ask(ep, to, &a).await? == [1], "not paired there");
    Ok(())
}

type Phones = BTreeMap<EndpointId, (String, Vec<[u8; 16]>)>;

#[derive(Clone)]
pub struct Server(Arc<(PathBuf, Mutex<Phones>, Arc<irpush::Node>)>);

impl Server {
    pub fn open(dir: &Path, net: &Net) -> Result<Option<Self>> {
        let Some(irpush) = host(dir, net)? else { return Ok(None) };
        let file = dir.join("phones");
        let phones = Mutex::new(store::load(&file));
        Ok(Some(Self(Arc::new((file, phones, irpush)))))
    }

    async fn asked(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let b = crate::sync::rest(&mut rx, 1024 + 17 * MOST + 64).await?;
        let (file, phones, irpush) = &*self.0;
        let reply = match postcard::from_bytes(&b)? {
            Ask::Ticket => irpush.ticket(irpush::Role::Device)?.into_bytes(),
            Ask::Register(to, rings) => {
                ensure!(to.len() <= 128 && rings.len() <= MOST, "too big");
                // Only a device paired here.
                let d = to.strip_prefix("irpush:").unwrap_or_default();
                let ok = irpush.peers().iter().any(|p| p.0.to_string() == d);
                if ok {
                    let mut p = lock(phones);
                    p.insert(c.remote_id(), (to, rings));
                    store::save(file, &*p)?;
                }
                vec![ok as u8]
            }
        };
        tx.write_all(&reply).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }

    /// A doorbell's ring, to whichever phones hold its token: as it is, or
    /// too big for one frame, the space alone, for the phone to catch up.
    async fn ring(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let mut head = [0u8; HEAD];
        rx.read_exact(&mut head).await?;
        let raw = crate::sync::rest(&mut rx, MAX).await?;
        let (_, phones, irpush) = &*self.0;
        let to: Vec<String> = lock(phones)
            .values()
            .filter(|p| p.1.iter().any(|t| t[..] == head[..16]))
            .map(|p| p.0.clone())
            .collect();
        let ring = [&head[16..48], &raw[..]].concat();
        let r =
            if ring.len() <= irpush::BODY { &ring[..] } else { &ring[..32] };
        let mut ok = false;
        for d in to.iter().filter_map(|t| t.strip_prefix("irpush:")) {
            let sent = d.parse().map_err(Into::into);
            let sent = sent.and_then(|d| irpush.push(d, &irpush::app(APP), r));
            ok |= sent.inspect_err(|e| tracing::warn!("push: {e:#}")).is_ok();
        }
        tx.write_all(&[ok as u8]).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }
}

/// The state key of an irpush node kept here, as the doorbell's are.
fn kek(dir: &Path) -> Result<[u8; 32]> {
    let f = dir.join("irpush.kek");
    if let Some(k) = std::fs::read(&f).ok().and_then(|k| k.try_into().ok()) {
        return Ok(k);
    }
    let k = crate::crypto::rand::<32>();
    store::write(&f, &k)?;
    Ok(k)
}

/// Where a storage node's irpush meets phones: `IRCH_PUSH_RELAY`, else
/// irch's relays (n0's last unless they are your own). LAN only, none.
fn relays(net: &Net) -> Option<Vec<irpush::Relay>> {
    let l = match (std::env::var("IRCH_PUSH_RELAY"), net) {
        (Ok(l), _) => l,
        (_, Net::Default) => crate::node::RELAYS[0].into(),
        (_, Net::Relay(u)) => format!("{},!n0", u[0]),
        _ => return None,
    };
    irpush::Relay::list(&l).ok().filter(|l| !l.is_empty())
}

/// A storage node's irpush host, running.
fn host(dir: &Path, net: &Net) -> Result<Option<Arc<irpush::Node>>> {
    let Some(relays) = relays(net) else { return Ok(None) };
    let (f, kek) = (dir.join("irpush"), kek(dir)?);
    let n = match f.exists() {
        true => irpush::Node::open(&f, kek, vec![])?,
        false => irpush::Node::create(&f, kek, relays.clone())?,
    };
    if n.relays() != relays {
        n.set_relays(relays)?;
    }
    let n = Arc::new(n);
    let run = n.clone();
    tokio::spawn(async move { run.run(|_, _, _| {}).await });
    Ok(Some(n))
}

/// A phone's irpush device: one per profile, its pushes as rings.
pub struct Device {
    pub node: Arc<irpush::Node>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<News>>,
    run: tokio::task::AbortHandle,
}

impl Drop for Device {
    fn drop(&mut self) {
        self.run.abort();
    }
}

impl Device {
    /// What a phone gives its push server.
    pub fn url(&self) -> String {
        format!("irpush:{}", self.node.id())
    }

    pub fn paired(&self) -> bool {
        !self.node.peers().is_empty()
    }

    /// The next ring that came as a push.
    pub async fn next(&self) -> Option<News> {
        self.rx.lock().await.recv().await
    }
}

static DEVICES: Mutex<BTreeMap<PathBuf, Arc<Device>>> =
    Mutex::new(BTreeMap::new());

/// Ours, running: kept from before, or joined now with `ticket`.
pub fn device(dir: &Path, ticket: Option<&str>) -> Result<Option<Arc<Device>>> {
    let mut all = lock(&DEVICES);
    if let Some(d) = all.get(dir) {
        return Ok(Some(d.clone()));
    }
    let (f, kek) = (dir.join("irpush"), kek(dir)?);
    let node = match (f.exists(), ticket) {
        (true, _) => irpush::Node::open(&f, kek, vec![])?,
        (false, Some(t)) => irpush::Node::join(&f, kek, t, vec![])?,
        (false, None) => return Ok(None),
    };
    let (node, (tx, rx)) =
        (Arc::new(node), tokio::sync::mpsc::unbounded_channel());
    let (run, me) = (node.clone(), irpush::app(APP));
    let on = move |_, app, b: &[u8]| {
        if let Some((sp, raw)) =
            b.split_first_chunk::<32>().filter(|_| app == me)
        {
            let _ = tx.send((Id(*sp), raw.to_vec()));
        }
    };
    let run = tokio::spawn(async move { run.run(on).await }).abort_handle();
    let d = Arc::new(Device { node, rx: rx.into(), run });
    all.insert(dir.into(), d.clone());
    Ok(Some(d))
}

/// Ours, never paired (its ticket ran out): gone, for a new one.
pub fn forget(dir: &Path) {
    lock(&DEVICES).remove(dir);
    let _ = std::fs::remove_file(dir.join("irpush"));
}

/// Heartbeat now (an alarm rang): seconds to the next, 0 with no device.
pub fn kick() -> u64 {
    let all = lock(&DEVICES);
    all.values().map(|d| d.node.kick().as_secs()).min().unwrap_or(0)
}

/// Another network (a stable id for each), or none (`irpush::OFFLINE`).
pub fn network(id: u64) {
    lock(&DEVICES).values().for_each(|d| d.node.network(id));
}

// ── Android: one device per phone, run by irpush's library for every app ──
//
// Its owner may be another app; the platform tells us its id, takes the
// tickets we get to join with (a pull: `next_join`), says how each went,
// and hands rings to the app itself. Ours is joined once a keeper's ticket
// went through (`irpush.joined` holds the id it was for).

static SHARED: Mutex<Option<Option<String>>> = Mutex::new(None);
static JOINS: (Mutex<Option<String>>, std::sync::Condvar) =
    (Mutex::new(None), std::sync::Condvar::new());
static JOINED: Mutex<Option<bool>> = Mutex::new(None);

/// The platform runs our device: its id, once known ("" not joined).
pub fn share(id: Option<String>) {
    *lock(&SHARED) = Some(id);
}

pub fn shared() -> bool {
    lock(&SHARED).is_some()
}

fn shared_id() -> Option<String> {
    lock(&SHARED).clone().flatten().filter(|i| !i.is_empty())
}

/// Our route, if a keeper's mesh took this device (no node needed).
pub fn shared_route(dir: &Path) -> Option<String> {
    let id = shared_id()?;
    let m = std::fs::read_to_string(dir.join("irpush.joined")).ok()?;
    (m == id).then(|| format!("irpush:{id}"))
}

/// Platform thread: the next ticket to join with.
pub fn next_join() -> String {
    let w = JOINS.1.wait_while(lock(&JOINS.0), |t| t.is_none());
    w.unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default()
}

/// Platform: how the last join went.
pub fn joined(ok: bool) {
    *lock(&JOINED) = Some(ok);
}

/// Join the shared device into `ticket`'s mesh; our route if it went.
pub async fn join_shared(dir: &Path, ticket: &str) -> Option<String> {
    *lock(&JOINED) = None;
    *lock(&JOINS.0) = Some(ticket.into());
    JOINS.1.notify_all();
    for _ in 0..300 {
        if let Some(ok) = lock(&JOINED).take() {
            let id = shared_id().filter(|_| ok)?;
            std::fs::write(dir.join("irpush.joined"), &id).ok()?;
            return Some(format!("irpush:{id}"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    None
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("push server")
    }
}

impl ProtocolHandler for Server {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        let r = match c.alpn() {
            ALPN => self.asked(&c).await,
            _ => self.ring(&c).await,
        };
        r.map_err(|e| {
            AcceptError::from_err(std::io::Error::other(e.to_string()))
        })
    }
}
