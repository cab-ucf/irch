//! Push servers: every storage node with a relay is one, through irpush.
//! A phone pairs with one it knows (its own first) once, then holds one
//! quiet socket, shared, with heartbeats as long as its network allows,
//! and gives it `irpush:DEVICE` and a ring token per space and epoch
//! (wake.rs). Members ring with those; each ring reaches the phone sealed
//! end to end (post-quantum), always the same size. The push server learns
//! when, and for which space, nothing else. No Google: no Firebase.

use crate::wake::{HEAD, MAX};
use crate::{Id, Net, node::lock, store};
use anyhow::{Result, ensure};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ALPN: &[u8] = b"irch/push/4";
/// Ring tokens a phone may have here: one per space it is in.
const MOST: usize = 4096;
/// Phones kept here, and ring tokens in all: past either, the one longest
/// unregistered goes.
const PHONES: usize = 1024;
const RINGS: usize = 1 << 16;
const SLOW: Duration = Duration::from_secs(15);
/// irpush's address for irch on a phone.
const APP: &str = "dev.irch.chat";

#[derive(Serialize, Deserialize)]
enum Ask {
    /// A one-time irpush ticket, to pair with.
    Ticket,
    /// `irpush:DEVICE` (paired here) and our ring tokens.
    Register(String, Vec<[u8; 16]>),
    /// `irpush:DEVICE` (paired here): our irpush relays, `url#token,…`,
    /// which its iroh relays through too.
    Relays(String),
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

/// Our best relay, and whether it is quiet (irpush's, pinged hours apart):
/// a storage node's, or irch's first; not a stock one.
pub fn relay(dir: &Path) -> (String, bool) {
    let r = std::fs::read_to_string(dir.join("relays")).ok();
    let first = r.as_deref().and_then(|r| r.split(',').next());
    let url = first.and_then(|f| f.split('#').next());
    let url = url.filter(|u| u.starts_with("http"));
    let url = url.unwrap_or(crate::node::RELAYS[0]);
    let stock = crate::node::RELAYS[1..]
        .iter()
        .any(|s| s.parse::<iroh::RelayUrl>().ok() == url.parse().ok());
    (url.split('#').next().unwrap_or(url).into(), !stock)
}

/// A storage node's irpush relay, checked, kept in DIR/push-relay.
pub fn set_relay(dir: &Path, l: &str) -> Result<()> {
    ensure!(!irpush::Relay::list(l)?.is_empty(), "no relay given");
    std::fs::create_dir_all(dir)?;
    store::write(&dir.join("push-relay"), l.trim().as_bytes())
}

/// A ticket to pair our irpush device with `to`.
pub async fn ticket(ep: &Endpoint, to: EndpointAddr) -> Result<String> {
    Ok(String::from_utf8(ask(ep, to, &Ask::Ticket).await?)?)
}

/// The irpush relays of `to`, where `route` is paired.
pub async fn relays_of(
    ep: &Endpoint,
    to: EndpointAddr,
    route: &str,
) -> Result<String> {
    let l = String::from_utf8(ask(ep, to, &Ask::Relays(route.into())).await?)?;
    ensure!(!irpush::Relay::list(&l)?.is_empty(), "no relays there");
    Ok(l)
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

type Phone = (String, Vec<[u8; 16]>);

/// Each phone's route and tokens (kept on disk), whose each token is, and
/// when each last registered (since we started; before, oldest).
#[derive(Default)]
struct Phones {
    by: BTreeMap<EndpointId, Phone>,
    rings: HashMap<[u8; 16], HashSet<EndpointId>>,
    at: HashMap<EndpointId, Instant>,
}

impl Phones {
    fn set(&mut self, d: EndpointId, p: Option<Phone>) {
        for t in self.by.remove(&d).into_iter().flat_map(|p| p.1) {
            if let Some(s) = self.rings.get_mut(&t)
                && s.remove(&d)
                && s.is_empty()
            {
                self.rings.remove(&t);
            }
        }
        self.at.remove(&d);
        let Some(p) = p else { return };
        for t in &p.1 {
            self.rings.entry(*t).or_default().insert(d);
        }
        self.at.insert(d, Instant::now());
        self.by.insert(d, p);
    }
}

/// Devices this storage node admitted to sync: members it keeps for.
pub type Served = Arc<Mutex<HashSet<EndpointId>>>;

#[derive(Clone)]
pub struct Server(Arc<(PathBuf, Mutex<Phones>, Arc<irpush::Node>, Served)>);

impl Server {
    pub fn open(dir: &Path, net: &Net, s: Served) -> Result<Option<Self>> {
        let Some(irpush) = host(dir, net)? else { return Ok(None) };
        let file = dir.join("phones");
        let mut phones = Phones::default();
        let kept: BTreeMap<EndpointId, Phone> = store::load(&file);
        kept.into_iter().for_each(|(d, p)| phones.set(d, Some(p)));
        phones.at.clear();
        Ok(Some(Self(Arc::new((file, Mutex::new(phones), irpush, s)))))
    }

    async fn asked(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let b = crate::sync::rest(&mut rx, 1024 + 17 * MOST + 64).await?;
        let (file, phones, irpush, served) = &*self.0;
        // Only a device paired here.
        let paired = |to: &str| {
            let d = to.strip_prefix("irpush:").unwrap_or_default();
            irpush.peers().iter().any(|p| p.0.to_string() == d)
        };
        let ours = lock(served).contains(&c.remote_id());
        let reply = match postcard::from_bytes(&b)? {
            Ask::Ticket => irpush.ticket(irpush::Role::Device)?.into_bytes(),
            // Paired, or a member's device we sync with: whatever its push.
            Ask::Relays(to) => match ours || paired(&to) {
                true => irpush::Relay::show(&irpush.relays()).into_bytes(),
                false => vec![],
            },
            Ask::Register(to, rings) => {
                ensure!(to.len() <= 128 && rings.len() <= MOST, "too big");
                let ok = paired(&to);
                if ok {
                    // A second apart each, so none rewrites the file flat out.
                    let (d, second) = (c.remote_id(), Duration::from_secs(1));
                    let last = lock(phones).at.get(&d).copied();
                    let wait = last.map(|t| second.saturating_sub(t.elapsed()));
                    tokio::time::sleep(wait.unwrap_or_default()).await;
                    let mut p = lock(phones);
                    p.set(d, Some((to, rings)));
                    while p.by.len() > PHONES || p.rings.len() > RINGS {
                        let old = p.by.keys().min_by_key(|d| p.at.get(*d));
                        let old = *old.expect("some");
                        p.set(old, None);
                    }
                    store::save(file, &p.by)?;
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
        let (_, phones, irpush, _) = &*self.0;
        let t: [u8; 16] = head[..16].try_into()?;
        let to: Vec<String> = {
            let p = lock(phones);
            let to = p.rings.get(&t).into_iter().flatten();
            to.filter_map(|d| Some(p.by.get(d)?.0.clone())).collect()
        };
        let ring = [&[OP][..], &head[16..48], &raw[..]].concat();
        let r =
            if ring.len() <= irpush::BODY { &ring[..] } else { &ring[..33] };
        let mut ok = false;
        for d in to.iter().filter_map(|t| t.strip_prefix("irpush:")) {
            let sent = d.parse().map_err(Into::into);
            let sent = sent.and_then(|d| irpush.push(d, &irpush::app(APP), r));
            ok |= sent.inspect_err(|e| tracing::warn!("push: {e:#}")).is_ok();
        }
        // 2: taken, on its way through Google (not on the phone yet).
        tx.write_all(&[if ok { 2 } else { 0 }]).await?;
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

/// Where a storage node's irpush meets phones, and so the relay its phones
/// use for everything: "https://irpush.you.example#TOKEN", from the systemd
/// credential `push-relay` (kept out of the Nix store), DIR/push-relay
/// (`irch --install --push-relay`) or `IRCH_PUSH_RELAY`; else irch's relays
/// (n0's last unless they are your own). LAN only, none.
fn relays(dir: &Path, net: &Net) -> Option<Vec<irpush::Relay>> {
    let read = |d: &Path| std::fs::read_to_string(d.join("push-relay")).ok();
    let cred = std::env::var_os("CREDENTIALS_DIRECTORY")
        .and_then(|d| read(Path::new(&d)))
        .or_else(|| read(dir));
    let given = cred.or_else(|| std::env::var("IRCH_PUSH_RELAY").ok());
    let l = match (given, net) {
        (Some(l), _) => l.trim().to_string(),
        (_, Net::Default) => crate::node::RELAYS[0].into(),
        (_, Net::Relay(u)) => format!("{},!n0", u[0]),
        _ => return None,
    };
    irpush::Relay::list(&l).ok().filter(|l| !l.is_empty())
}

/// A storage node's irpush host, running.
fn host(dir: &Path, net: &Net) -> Result<Option<Arc<irpush::Node>>> {
    let Some(relays) = relays(dir, net) else { return Ok(None) };
    let (f, kek) = (dir.join("irpush"), kek(dir)?);
    let n = match f.exists() {
        true => irpush::Node::open(&f, kek, vec![])?,
        false => irpush::Node::create(&f, kek, relays.clone())?,
    };
    if n.relays() != relays {
        n.set_relays(relays)?;
    }
    Ok(Some(run(dir, n)))
}

// ── Lanes: every device's one irpush node, and what reaches irch on it ──
//
// Each device holds one irpush node (a storage node's host, a phone's
// device, a desktop's device or its own lone host), reachable by its id
// on its relay. irch sends and hears there under its one app id, as
// `kind ‖ space ‖ rest`: an op (a message, a ring: one datagram, sealed
// and signed as ever), its ack, or a dial (start iroh, a link is coming).
// Pushes from a storage node come the same way, queued for a sleeper.

/// What a frame carries: an op (empty: catch up), an ack (its ts), a dial.
pub const OP: u8 = 0;
pub const ACK: u8 = 1;
pub const DIAL: u8 = 2;
/// What came: the sender's irpush id and its frame.
pub type Heard = (EndpointId, Vec<u8>);
type Ear = tokio::sync::mpsc::UnboundedSender<Heard>;
static EARS: Mutex<BTreeMap<PathBuf, Ear>> = Mutex::new(BTreeMap::new());

struct Lane(Arc<irpush::Node>, tokio::task::AbortHandle);

impl Drop for Lane {
    fn drop(&mut self) {
        self.1.abort();
    }
}

static LANES: Mutex<BTreeMap<PathBuf, Lane>> = Mutex::new(BTreeMap::new());

/// `n` running as the lane of `dir`, in place of any before.
fn run(dir: &Path, n: irpush::Node) -> Arc<irpush::Node> {
    let (n, on) = (Arc::new(n), ear(dir));
    let r = n.clone();
    let task = tokio::spawn(async move { r.run(on).await }).abort_handle();
    lock(&LANES).insert(dir.into(), Lane(n.clone(), task));
    n
}

fn ear(dir: &Path) -> impl Fn(EndpointId, irpush::App, &[u8]) + use<> {
    let (dir, me) = (dir.to_path_buf(), irpush::app(APP));
    move |from, app, d| {
        let mut e = lock(&EARS);
        let gone = app == me
            && e.get(&dir).is_some_and(|e| e.send((from, d.to_vec())).is_err());
        if gone {
            e.remove(&dir);
        }
    }
}

/// `kind ‖ space ‖ rest`, or an older ring's `space ‖ op`, for a space
/// `ours` knows.
pub fn parse(b: &[u8], ours: impl Fn(&Id) -> bool) -> Option<(u8, Id, &[u8])> {
    let new =
        b.split_first().filter(|(k, _)| **k <= DIAL).and_then(|(k, r)| {
            let (s, rest) = r.split_first_chunk::<32>()?;
            ours(&Id(*s)).then_some((*k, Id(*s), rest))
        });
    new.or_else(|| {
        let (s, rest) = b.split_first_chunk::<32>()?;
        ours(&Id(*s)).then_some((OP, Id(*s), rest))
    })
}

/// What reaches `dir`'s lane from now on, here alone: the node's, or the
/// app's while the node sleeps.
pub fn listen(dir: &Path) -> tokio::sync::mpsc::UnboundedReceiver<Heard> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    lock(&EARS).insert(dir.into(), tx);
    rx
}

/// Whether someone hears `dir`'s lane.
pub fn heard(dir: &Path) -> bool {
    lock(&EARS).get(dir).is_some_and(|e| !e.is_closed())
}

/// A desktop's lane, if it has none: its device, else a lone host on our
/// relay (pinged half-hourly) until a storage node's mesh takes it.
pub fn ours(dir: &Path, net: &Net) -> Result<()> {
    let url = match net {
        Net::Default => relay(dir).0,
        Net::Relay(u) => u[0].to_string(),
        _ => return Ok(()),
    };
    if lane(dir).is_some() || device(dir, None)?.is_some() {
        return Ok(());
    }
    let (f, kek) = (dir.join("lane"), kek(dir)?);
    let n = match f.exists() {
        true => irpush::Node::open(&f, kek, vec![])?,
        false => irpush::Node::create(&f, kek, irpush::Relay::list(&url)?)?,
    };
    run(dir, n);
    Ok(())
}

/// The irpush node that carries our frames, if any: ours, or the phone's
/// shared one when it runs in our process.
pub fn lane(dir: &Path) -> Option<Arc<irpush::Node>> {
    let ours = lock(&LANES).get(dir).map(|l| l.0.clone());
    ours.or_else(irpush::shared)
}

/// Ours, as members get it (`irch:ev:ID@RELAY`), while on a relay.
pub fn lane_at(dir: &Path) -> Option<String> {
    let l = lane(dir)?;
    Some(format!("irch:ev:{}@{}", l.id(), l.on().first()?))
}

/// A device's events address, from its push.
pub fn lane_of(push: &str) -> Option<(EndpointId, String)> {
    push.split_whitespace().find_map(|t| {
        let (id, at) = t.strip_prefix("irch:ev:")?.split_once('@')?;
        Some((id.parse().ok()?, at.into()))
    })
}

/// One frame to `to` on `relay`; whether it left.
pub fn event(dir: &Path, to: EndpointId, relay: &str, b: &[u8]) -> bool {
    lane(dir).is_some_and(|l| l.event(to, relay, &irpush::app(APP), b))
}

/// Our irpush device (a storage node's mesh took it), running: kept from
/// before, or joined now with `ticket`. It replaces a lone lane.
pub fn device(
    dir: &Path,
    ticket: Option<&str>,
) -> Result<Option<Arc<irpush::Node>>> {
    let f = dir.join("irpush");
    let dev = |l: &Arc<irpush::Node>| l.role() == irpush::Role::Device;
    if let Some(l) = lane(dir).filter(dev) {
        return Ok(Some(l));
    }
    let kek = kek(dir)?;
    let n = match (f.exists(), ticket) {
        (true, _) => irpush::Node::open(&f, kek, vec![])?,
        (false, Some(t)) => irpush::Node::join(&f, kek, t, vec![])?,
        (false, None) => return Ok(None),
    };
    Ok(Some(run(dir, n)))
}

/// What a phone gives its push server.
pub fn url(d: &irpush::Node) -> String {
    format!("irpush:{}", d.id())
}

/// Our device into one more push server's mesh: rings get through while
/// any one of them is up. Whether it joined.
pub async fn also(dir: &Path, ticket: &str) -> bool {
    if shared() {
        return join_shared(dir, ticket).await.is_some();
    }
    let Ok(Some(d)) = device(dir, None) else { return false };
    let had = d.peers().len();
    if d.add(ticket).is_err() {
        return false;
    }
    for _ in 0..80 {
        if d.peers().len() > had {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

/// Ours, never paired (its ticket ran out): gone, for a new one.
pub fn forget(dir: &Path) {
    lock(&LANES).remove(dir);
    let _ = std::fs::remove_file(dir.join("irpush"));
}

/// Heartbeat now (an alarm rang): seconds to the next, 0 with no lane.
pub fn kick() -> u64 {
    let all = lock(&LANES);
    all.values().map(|l| l.0.kick().as_secs()).min().unwrap_or(0)
}

/// Another network (a stable id for each), or none (`irpush::OFFLINE`).
pub fn network(id: u64) {
    lock(&LANES).values().for_each(|l| l.0.network(id));
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

#[test]
fn a_storage_nodes_relay_is_checked_kept_and_told() {
    let n = u64::from_le_bytes(crate::crypto::rand());
    let d = std::env::temp_dir().join(format!("irch-relay-{n}"));
    assert!(set_relay(&d, "not a url").is_err());
    let r = "https://irpush.example./".to_string();
    set_relay(&d, &r).unwrap();
    assert_eq!(std::fs::read_to_string(d.join("push-relay")).unwrap(), r);
    assert_eq!(relay(&d), (crate::node::RELAYS[0].into(), true), "irch's own");
    let stock = crate::node::RELAYS[1];
    std::fs::write(d.join("relays"), stock).unwrap();
    assert_eq!(relay(&d), (stock.into(), false), "a stock one");
    std::fs::write(d.join("relays"), format!("{r},https://b.example./"))
        .unwrap();
    assert_eq!(relay(&d), ("https://irpush.example./".into(), true));
    let _ = std::fs::remove_dir_all(d);
}
