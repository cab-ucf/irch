//! One iroh endpoint: gossip (live) and sync (catch-up) for every space.
//! Every op, local or remote: open, check, append to the log, fold, notify.

use crate::call::{self, Call};
use crate::crypto::{Key, Me, kem_open, kem_seal, rand, same};
use crate::op::{self, Env, File, Invite, Op, Role, Signed, code, tag, uncode};
use crate::state::State;
use crate::store::{self, Log};
use crate::sync::{self, Admit, Bucket, Hello};
use crate::{Ch, Id, now};
use anyhow::{Context, Result, anyhow, bail, ensure};
use iroh::address_lookup::MemoryLookup;
use iroh::endpoint::{BindOpts, Connection, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{
    Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey,
};
use iroh_blobs::BlobsProtocol;
use iroh_blobs::store::fs::FsStore;
use iroh_gossip::api::{Event as Gossiped, GossipSender};
use iroh_gossip::{Gossip, TopicId};
use n0_future::StreamExt;
use n0_future::task::AbortOnDropHandle;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{
    AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed,
};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::sync::broadcast;
use tracing::{debug, warn};

const DAY: u64 = 86_400_000;
/// How far ahead a clock may run: later ops wait (sync brings them again).
const SKEW: u64 = 600_000;
const GOSSIP_MAX: usize = 128 << 10;
const BEAT: Duration = Duration::from_secs(60);
const ONLINE: Duration = Duration::from_secs(150);
// Someone in a call says so every 15 s.
const CALLER: Duration = Duration::from_secs(45);
const RELAYS: [&str; 2] =
    ["https://iroh.cxn.center./", "https://use1-1.relay.n0.iroh.link./"];
pub use crate::op::{MAX_NAME, MAX_TEXT};
/// What storage nodes keep without their password.
pub const MAX_FILE: u64 = 5 << 30;
/// `MAX_FILE`, or in tests (debug builds) IRCH_TEST_MAX_FILE.
fn limit() -> u64 {
    let test = std::env::var("IRCH_TEST_MAX_FILE")
        .ok()
        .filter(|_| cfg!(debug_assertions));
    test.and_then(|v| v.parse().ok()).unwrap_or(MAX_FILE)
}

/// What opens in memory, to preview; larger files are saved.
const PREVIEW: usize = 25 << 20;
const BLOB: &[u8] = b"irch/blob/1";
const PARKED: usize = 32 << 20;
const LOCKED: &str = "keys changed without you: ask for a new invite";

#[derive(Clone, Debug, Default)]
pub enum Net {
    /// The lab's relay, then n0's US East; n0's DNS and the Mainline DHT.
    #[default]
    Default,
    /// Your own relays and the Mainline DHT; nothing of n0's.
    Relay(Vec<RelayUrl>),
    Lan,
    /// Only addresses from codes and the address book (tests).
    Direct,
}

pub struct Config {
    pub dir: PathBuf,
    pub name: String,
    pub net: Net,
    /// Become another device of whoever made this link code.
    pub link: Option<String>,
    /// Be a storage node: keep others' ciphertext.
    pub serve: bool,
    /// A Markdown vault for "Add to notes".
    pub notes: Option<PathBuf>,
    /// A storage node's password, which keeps files past `MAX_FILE`.
    pub big: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Event {
    Changed { space: Id },
    Typing { space: Id, channel: Ch, user: Id },
    Presence { space: Id },
    Synced { space: Id, count: usize },
    Call { space: Id },
}

#[derive(Serialize, Deserialize, Default)]
struct Local {
    port: u16,
    invisible: bool,
    read: BTreeMap<(Id, Ch), u64>,
    joined: BTreeMap<Id, u64>,
    peers: BTreeMap<EndpointId, EndpointAddr>,
    hosted: BTreeMap<Id, Vec<EndpointId>>,
    token: Option<[u8; 16]>,
}

/// Apart from `Local`: postcard is positional, a new field resets it.
#[derive(Serialize, Deserialize, Default)]
struct Prefs {
    level: BTreeMap<Id, Level>,
    // Per space, notified (or seen) up to here.
    told: BTreeMap<Id, u64>,
    push: Option<String>,
}

/// A storage node's word for one space, from its owners: where members
/// meet, which epochs it keeps, and each owner's devices it takes from.
#[derive(Serialize, Deserialize, Default)]
struct Host {
    room: [u8; 32],
    tags: BTreeSet<[u8; 8]>,
    users: BTreeMap<Id, Vec<EndpointId>>,
}

/// A file served from here, to the members of the space it went to;
/// `space` none once its sender stopped sharing it.
#[derive(Serialize, Deserialize, Clone)]
struct Shared {
    space: Option<Id>,
    tag: Vec<u8>,
    name: String,
    size: u64,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub enum Level {
    #[default]
    All,
    Mentions,
    Off,
}

#[derive(Serialize, Debug, Clone)]
pub struct Notice {
    pub space: Id,
    pub room: Ch,
    pub title: String,
    pub body: String,
    pub ring: bool,
    /// Its call rang, and ended: stop ringing.
    pub hush: bool,
}

pub struct Space {
    pub id: Id,
    inv: Option<Invite>,
    book: Mutex<Book>,
    tx: Mutex<Option<GossipSender>>,
    nbrs: Mutex<HashSet<EndpointId>>,
    seen: Mutex<HashMap<Id, Instant>>,
    // Live ops already taken, so none is taken twice.
    heard: Mutex<HashMap<Id, Instant>>,
    calls: Mutex<HashMap<Id, (Ch, EndpointId, Instant)>>,
    // Syncing with now; true: asked again meanwhile, so once more after.
    busy: Mutex<HashMap<EndpointId, bool>>,
    // Devices our storage nodes were last told they may take from.
    vouched: AtomicUsize,
    // The gossip topic we are in, and a nudge to move to a new one.
    room: Mutex<[u8; 32]>,
    moved: Arc<Notify>,
    // Our ops written while nobody else was reachable.
    undelivered: Mutex<HashSet<Id>>,
    task: OnceLock<AbortOnDropHandle<()>>,
}

struct Book {
    log: Log,
    idx: BTreeMap<u32, BTreeSet<Id>>,
    at: HashMap<Id, (u64, u32)>,
    keys: HashMap<[u8; 8], Key>,
    // Unopened yet; past `PARKED` bytes dropped (sync brings it again).
    pending: HashMap<Id, (Env, Src)>,
    parked: usize,
    // Per epoch, the older keys `Back` gives its holders.
    backs: HashMap<[u8; 8], Vec<[u8; 8]>>,
    // Learned once an admin's op stands (maybe after more history opens).
    offered: Vec<Offered>,
    st: State,
    knocks: Vec<Invite>,
    // A member said our key is one a later epoch left us out of.
    out: bool,
}

/// (author, (ts, id), epoch, key, via `Back` rather than `Rekey`).
type Offered = (Id, (u64, Id), [u8; 8], Key, bool);

/// In the log (0, or the version it checked as), or held till it opens
/// (ours: no check).
enum Src {
    Log(u8),
    Net(Vec<u8>, bool),
}

fn named(n: &str) -> Result<&str> {
    let n = n.trim();
    ensure!(!n.is_empty() && n.len() <= MAX_NAME, "names are 1 to 64 bytes");
    Ok(n)
}

/// One irch to a folder: a second says so, rather than wait on the first.
async fn hold(dir: &Path) -> Result<std::fs::File> {
    let f = std::fs::File::create(dir.join("lock"))?;
    for _ in 0..50 {
        if f.try_lock().is_ok() {
            return Ok(f);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("another irch is using {}", dir.display())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn rekey_aad(space: &Id, epoch: &[u8; 8]) -> Vec<u8> {
    [&space.0[..], epoch].concat()
}

impl Book {
    fn new(log: Log, st: State) -> Self {
        let (idx, at, keys) = (BTreeMap::new(), HashMap::new(), HashMap::new());
        let (pending, backs, knocks) = (HashMap::new(), HashMap::new(), vec![]);
        Self {
            log,
            idx,
            at,
            keys,
            pending,
            parked: 0,
            backs,
            offered: vec![],
            st,
            knocks,
            out: false,
        }
    }

    fn has(&self, i: &Id) -> bool {
        self.at.contains_key(i) || self.pending.contains_key(i)
    }

    fn index(&mut self, i: Id, ts: u64, at: u64, len: usize) {
        self.at.insert(i, (at, len as u32));
        self.idx.entry((ts / DAY) as u32).or_default().insert(i);
    }

    /// Append to the log (flushed, for ours) and the day index sync compares.
    fn store(
        &mut self,
        i: Id,
        ts: u64,
        raw: &[u8],
        checked: u8,
        flush: bool,
    ) -> bool {
        if self.at.contains_key(&i) {
            return false;
        }
        let at = self.log.append(checked, raw, flush);
        let Ok(at) = at.inspect_err(|e| warn!("log: {e}")) else {
            return false;
        };
        self.index(i, ts, at, raw.len());
        true
    }

    /// Open what checks out; learned keys open more. Returns how many new.
    fn fold(
        &mut self,
        sp: &Id,
        me: &Me,
        mut work: Vec<(Id, Env, Src)>,
    ) -> usize {
        let (mut ops, mut kept) = (vec![], 0);
        loop {
            while let Some((i, e, src)) = work.pop() {
                let Some(&key) = self.keys.get(&e.epoch) else {
                    self.park(i, e, src);
                    continue;
                };
                let known = match src {
                    Src::Log(0) | Src::Net(_, false) => None,
                    Src::Log(v) => Some(v),
                    Src::Net(_, true) => Some(2),
                };
                let Some((s, v)) = op::open(sp, &key, &e, known) else {
                    continue;
                };
                // Banned, cut off or retired: dropped, or it fills every
                // member's disk.
                let at = (s.ts, s.id);
                if self.st.role(&s.author) == Role::Banned
                    || self.st.stale(&e.epoch, &at, &s.author)
                    || self.st.burned_after(&at, &s.author)
                {
                    continue;
                }
                if let Src::Net(raw, ours) = &src {
                    if !self.store(i, e.ts, raw, v, *ours) {
                        continue;
                    }
                    kept += 1;
                }
                let key = match &s.op {
                    Op::Rekey { epoch, boxes } => boxes
                        .iter()
                        .find(|b| b.0 == me.id())
                        .and_then(|b| {
                            kem_open(&me.kem, &rekey_aad(sp, epoch), &b.1)
                        })
                        .and_then(|k| Key::try_from(k).ok())
                        .filter(|k| tag(k) == *epoch)
                        .map(|k| (k, false)),
                    Op::Back { key } => Some((*key, true)),
                    Op::Knock { .. }
                        if self.st.role(&s.author) == Role::Banned
                            || self.st.stale(
                                &e.epoch,
                                &(s.ts, s.id),
                                &s.author,
                            ) =>
                    {
                        None
                    }
                    Op::Knock { sealed } => {
                        let inv = kem_open(&me.kem, &sp.0, sealed);
                        let inv = inv.and_then(|b| {
                            postcard::from_bytes::<Invite>(&b).ok()
                        });
                        self.knocks.extend(inv.filter(|i| {
                            i.dm == Some(me.id()) && i.owner == s.author
                        }));
                        None
                    }
                    _ => None,
                };
                if let Some((k, back)) = key {
                    if self.offered.len() >= 1024 {
                        self.offered.remove(0);
                    }
                    self.offered.push((s.author, at, e.epoch, k, back));
                }
                ops.push(s);
            }
            self.st.extend(std::mem::take(&mut ops));
            let st = &self.st;
            let good =
                |(a, at, ep, ..): &(Id, (u64, Id), [u8; 8], Key, bool)| {
                    st.role(a) == Role::Admin && !st.stale(ep, at, a)
                };
            let good: Vec<_> =
                self.offered.extract_if(.., |o| good(o)).collect();
            for (_, _, under, k, back) in good {
                if back {
                    self.backs.entry(under).or_default().push(tag(&k));
                }
                if self.keys.insert(tag(&k), k).is_none() {
                    work.extend(self.unpark(&tag(&k)));
                }
            }
            if work.is_empty() {
                return kept;
            }
        }
    }

    fn park(&mut self, i: Id, e: Env, src: Src) {
        if let Src::Net(raw, _) = &src {
            if self.parked > PARKED {
                self.pending.retain(|_, (_, s)| matches!(s, Src::Log(_)));
                self.parked = 0;
            }
            // A map entry too, not just its size.
            self.parked += raw.len() + 256;
        }
        self.pending.insert(i, (e, src));
    }

    fn unpark(&mut self, t: &[u8; 8]) -> Vec<(Id, Env, Src)> {
        let out = self.pending.extract_if(|_, (e, _)| e.epoch == *t);
        let v: Vec<_> = out.map(|(i, (e, s))| (i, e, s)).collect();
        for (_, _, s) in &v {
            if let Src::Net(raw, _) = s {
                self.parked -= raw.len() + 256;
            }
        }
        v
    }

    /// A key from an invite: one an admin set, or any while rekeys have
    /// left us out (its own `Rekey` may be sealed under a key we lack).
    fn learn(&mut self, sp: &Id, me: &Me, k: Key) -> bool {
        let set = self.st.epochs().iter().any(|e| e.tag == tag(&k));
        let out =
            self.st.epoch().is_some_and(|e| !self.keys.contains_key(&e.tag));
        let new = (set || out) && self.keys.insert(tag(&k), k).is_none();
        if new {
            let w = self.unpark(&tag(&k));
            self.fold(sp, me, w);
        }
        new
    }

    /// Seal key, and if a newer epoch left us out; never older than our invite.
    fn key(&self, inv: &Invite) -> (Key, bool) {
        let st = &self.st;
        if !st.epochs().iter().any(|e| e.tag == tag(&inv.key)) {
            return (inv.key, false);
        }
        let newest = st.epoch().map(|e| e.tag);
        let held =
            st.live().find_map(|e| Some((e.tag, *self.keys.get(&e.tag)?)));
        held.map_or((inv.key, true), |(t, k)| (k, Some(t) != newest))
    }

    /// Valid epochs' keys `Back` misses: lost rekey races, demoted admins'.
    fn orphans(&self) -> Vec<Key> {
        let Some(cur) = self.st.epoch() else { return vec![] };
        let (mut seen, mut todo) = (HashSet::from([cur.tag]), vec![cur.tag]);
        while let Some(t) = todo.pop() {
            let older = self.backs.get(&t).into_iter().flatten().copied();
            todo.extend(older.filter(|b| seen.insert(*b)));
        }
        let lost = self.st.epochs().iter().filter(|e| !seen.contains(&e.tag));
        lost.filter_map(|e| self.keys.get(&e.tag).copied()).collect()
    }
}

impl Space {
    pub fn buckets(&self) -> Vec<Bucket> {
        let b = lock(&self.book);
        let fp = |ids: &BTreeSet<Id>| {
            let mut h = blake3::Hasher::new();
            for i in ids {
                h.update(&i.0);
            }
            h.finalize().as_bytes()[..16].try_into().expect("16")
        };
        b.idx.iter().map(|(d, ids)| (*d, ids.len() as u32, fp(ids))).collect()
    }

    pub fn ids(&self, day: u32) -> Vec<Id> {
        let b = lock(&self.book);
        b.idx.get(&day).map(|s| s.iter().copied().collect()).unwrap_or_default()
    }

    pub fn get(&self, ids: &[Id]) -> Vec<Vec<u8>> {
        let mut b = lock(&self.book);
        let at: Vec<_> =
            ids.iter().filter_map(|i| b.at.get(i).copied()).collect();
        at.into_iter().filter_map(|(o, l)| b.log.read(o, l).ok()).collect()
    }

    /// Devices of members not banned: whom a storage node may take from.
    pub fn devices(&self) -> Vec<EndpointId> {
        lock(&self.book).st.devices(usize::MAX)
    }

    fn tx(&self) -> Option<GossipSender> {
        lock(&self.tx).clone()
    }

    fn nbrs(&self) -> Vec<EndpointId> {
        lock(&self.nbrs).iter().copied().collect()
    }

    /// Heard live; true if they were not online already.
    fn saw(&self, user: Id) -> bool {
        let last = lock(&self.seen).insert(user, Instant::now());
        last.is_none_or(|t| t.elapsed() > ONLINE)
    }
}

pub struct Node {
    me: Me,
    own: Id,
    dir: PathBuf,
    net: Net,
    notes: Option<PathBuf>,
    ep: Endpoint,
    gossip: Gossip,
    router: OnceLock<Router>,
    lookup: MemoryLookup,
    spaces: RwLock<HashMap<Id, Arc<Space>>>,
    local: Mutex<Local>,
    prefs: Mutex<Prefs>,
    // Closed conversations, and when: hidden until something newer.
    closed: Mutex<BTreeMap<Id, u64>>,
    // Endpoints we woke: when last, and whether one more is held back.
    wake: Mutex<HashMap<String, (Option<Instant>, bool)>>,
    rang: Mutex<HashSet<(Id, Id, Ch)>>,
    push: OnceLock<reqwest::Client>,
    name: Mutex<String>,
    events: broadcast::Sender<Event>,
    clock: AtomicU64,
    invisible: AtomicBool,
    serve: Option<[u8; 16]>,
    big: Option<String>,
    files: FsStore,
    held: Mutex<Option<std::fs::File>>,
    // Per space, the day and what it took then (storage nodes).
    budget: Mutex<BTreeMap<Id, (u64, u64)>>,
    hosts: Mutex<BTreeMap<Id, Host>>,
    // Devices whose users proved them theirs (sync, calls).
    verified: Mutex<HashMap<EndpointId, Id>>,
    shared: Mutex<BTreeMap<Id, Shared>>,
    // When each peer last synced with us, either way.
    synced: Mutex<HashMap<EndpointId, u64>>,
    call: Mutex<Option<Call>>,
    media: call::Media,
}

/// As far as its address says: not a LAN's, a carrier's 100.64/10, nor
/// one of those hidden in IPv6 (::ffff:10.0.0.1, 64:ff9b::10.0.0.1).
pub(crate) fn public(ip: &std::net::IpAddr) -> bool {
    use std::net::IpAddr::{V4, V6};
    match ip {
        V4(a) => {
            let o = a.octets();
            !(a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_multicast()
                || a.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && o[1] & 0xc0 == 64)
                || (o[0] == 198 && o[1] & 0xfe == 18))
        }
        V6(a) => {
            let s = a.segments();
            let nat64 = s[..6] == [0x64, 0xff9b, 0, 0, 0, 0];
            let v4 = std::net::Ipv4Addr::from_bits(a.to_bits() as u32);
            match a.to_ipv4_mapped() {
                Some(v4) => public(&V4(v4)),
                None if nat64 => public(&V4(v4)),
                None => {
                    !(a.is_loopback()
                        || a.is_unspecified()
                        || a.is_multicast()
                        || (s[0] & 0xfe00) == 0xfc00
                        || (s[0] & 0xffc0) == 0xfe80)
                }
            }
        }
    }
}

pub(crate) fn endpoint(net: &Net) -> iroh::endpoint::Builder {
    let b = match net {
        Net::Default => Endpoint::builder(presets::N0).relay_mode(
            RelayMode::custom(RELAYS.map(|r| r.parse().expect("relay url"))),
        ),
        Net::Relay(urls) => Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::custom(urls.clone())),
        Net::Lan => Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .address_lookup(
                iroh_mdns_address_lookup::MdnsAddressLookup::builder(),
            ),
        Net::Direct => {
            Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled)
        }
    };
    // BBRv3, as loss is the norm: at 0.1% and 50 ms Cubic moves 1.1 MB/s a
    // stream, BBRv3 23; a 2 MiB window, 16 MB/s at 130 ms (default: 10).
    let cc = noq_proto::congestion::Bbr3Config::default();
    let b = b.transport_config(
        iroh::endpoint::QuicTransportConfig::builder()
            .stream_receive_window((2u32 << 20).into())
            .congestion_controller_factory(Arc::new(cc))
            .build(),
    );
    // Desktops: the network's proxy and system roots, as any other https.
    let b = b.proxy_from_env();
    #[cfg(not(target_os = "android"))]
    let b = b.ca_tls_config(iroh::tls::CaTlsConfig::system());
    b
}

#[derive(Debug, Clone)]
struct Proto(Weak<Node>);

impl ProtocolHandler for Proto {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        let gone = || AcceptError::from_err(std::io::Error::other("closing"));
        let n = self.0.upgrade().ok_or_else(gone)?;
        let r = match c.alpn() {
            sync::ALPN => sync::respond(&n, &c).await,
            call::ALPN => {
                n.answer(c).await;
                Ok(())
            }
            _ => n.give(&c).await,
        };
        r.map_err(|e| {
            AcceptError::from_err(std::io::Error::other(e.to_string()))
        })
    }
}

#[derive(Serialize, Deserialize)]
enum Ask {
    Get(Id),
    Put([u8; 16], Vec<u8>),
    // Fetch a sealed file from the asker and keep it (a password past 5 GB).
    Keep([u8; 16], Id, Option<String>),
    // A member's small sealed file, for a space this storage node keeps.
    Hold(Id, [u8; 16], Id),
}

/// A storage node's key for one space's small files: members have it.
pub fn hold_token(token: &[u8; 16], space: &Id) -> [u8; 16] {
    let mut h = blake3::Hasher::new_derive_key("irch hold 1");
    h.update(token).update(&space.0);
    h.finalize().as_bytes()[..16].try_into().expect("16 of 32")
}

fn secret(path: &Path, link: Option<Key>) -> Result<Key> {
    if let Ok(b) = std::fs::read(path) {
        return b
            .try_into()
            .map_err(|_| anyhow!("{} is damaged", path.display()));
    }
    let k = link.unwrap_or_else(rand);
    store::write(path, &k)?;
    Ok(k)
}

fn own_invite(me: &Me, peers: Vec<EndpointAddr>) -> Invite {
    let k = crate::crypto::kdf("irch own space", &[&me.seed]);
    let nonce = k[..16].try_into().expect("16");
    let key = crate::crypto::kdf("irch own key", &[&me.seed]);
    Invite { owner: me.id(), nonce, key, name: String::new(), dm: None, peers }
}

/// Each file asked for, to whom it may go: an offered one only to the
/// members of the space it was posted in.
fn gate(n: std::sync::Weak<Node>) -> iroh_blobs::provider::events::EventSender {
    use iroh_blobs::provider::events::*;
    let mask = EventMask {
        connected: ConnectMode::Intercept,
        get: RequestMode::Intercept,
        get_many: RequestMode::Disabled,
        ..EventMask::DEFAULT
    };
    let (tx, mut rx) = EventSender::channel(32, mask);
    tokio::spawn(async move {
        let mut who = HashMap::new();
        while let Some(m) = rx.recv().await {
            match m {
                ProviderMessage::ClientConnected(m) => {
                    if let Some(d) = m.endpoint_id {
                        who.insert(m.connection_id, d);
                    }
                    let _ = m.tx.send(Ok(())).await;
                }
                ProviderMessage::ConnectionClosed(m) => {
                    who.remove(&m.inner.connection_id);
                }
                ProviderMessage::GetRequestReceived(m) => {
                    let d = who.get(&m.inner.connection_id);
                    let h = Id(*m.inner.request.hash.as_bytes());
                    let n = n.upgrade().zip(d);
                    let ok = n.is_some_and(|(n, d)| n.may_get(d, &h));
                    let r =
                        if ok { Ok(()) } else { Err(AbortReason::Permission) };
                    let _ = m.tx.send(r).await;
                }
                _ => {}
            }
        }
    });
    tx
}

// Phones skip the Mainline DHT's always-on routing table; relays suffice.
fn dht(b: iroh::endpoint::Builder) -> iroh::endpoint::Builder {
    #[cfg(not(target_os = "android"))]
    let b = b.address_lookup(
        iroh_mainline_address_lookup::DhtAddressLookup::builder(),
    );
    b
}

impl Node {
    pub async fn start(cfg: Config) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&cfg.dir)?;
        #[cfg(unix)]
        std::fs::set_permissions(
            &cfg.dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )?;
        let held = hold(&cfg.dir).await?;
        let link: Option<(Key, Vec<EndpointAddr>)> =
            cfg.link.as_deref().map(|c| uncode("link", c)).transpose()?;
        let me =
            Me::new(secret(&cfg.dir.join("seed"), link.as_ref().map(|l| l.0))?);
        ensure!(
            link.as_ref().is_none_or(|l| l.0 == me.seed),
            "already someone else"
        );
        let dev =
            SecretKey::from_bytes(&secret(&cfg.dir.join("device"), None)?);
        let mut local: Local = store::load(&cfg.dir.join("local"));
        let prefs: Prefs = store::load(&cfg.dir.join("prefs"));
        let closed = store::load(&cfg.dir.join("closed"));
        let lookup = MemoryLookup::new();
        local.peers.values().for_each(|a| lookup.add_endpoint_info(a.clone()));
        let bind = async |port: u16| {
            let b = match &cfg.net {
                Net::Default | Net::Relay(_) => dht(endpoint(&cfg.net)),
                _ => endpoint(&cfg.net),
            };
            let v6 = BindOpts::default().set_is_required(false);
            let b = b.secret_key(dev.clone()).address_lookup(lookup.clone());
            let b = b
                .clear_ip_transports()
                .bind_addr((Ipv4Addr::UNSPECIFIED, port))?;
            let b = b.bind_addr_with_opts((Ipv6Addr::UNSPECIFIED, port), v6)?;
            anyhow::Ok(b.bind().await?)
        };
        // The same port as last time: peers that know only it still reach us.
        let ep = match bind(local.port).await {
            Ok(ep) => ep,
            Err(e) if local.port != 0 => {
                warn!("port {} unavailable ({e:#})", local.port);
                bind(0).await?
            }
            Err(e) => return Err(e),
        };
        local.port = ep
            .bound_sockets()
            .iter()
            .find(|a| a.is_ipv4())
            .map_or(0, |a| a.port());
        let serve = cfg.serve.then(|| *local.token.get_or_insert_with(rand));
        let files = FsStore::load(cfg.dir.join("files")).await?;
        let gossip =
            Gossip::builder().max_message_size(GOSSIP_MAX).spawn(ep.clone());
        let own_inv = own_invite(&me, link.map(|l| l.1).unwrap_or_default());
        let mut hosts: BTreeMap<Id, Host> = store::load(&cfg.dir.join("hosts"));
        let budget = store::load(&cfg.dir.join("budget"));
        let shared = store::load(&cfg.dir.join("shared"));
        // From before owners were told apart: one list, no one's.
        for (id, d) in std::mem::take(&mut local.hosted) {
            hosts.entry(id).or_default().users.insert(Id::default(), d);
        }
        let n = Arc::new(Self {
            own: own_inv.id(),
            me,
            dir: cfg.dir,
            net: cfg.net,
            notes: cfg.notes,
            ep,
            gossip,
            router: OnceLock::new(),
            lookup,
            spaces: RwLock::default(),
            invisible: AtomicBool::new(local.invisible),
            local: Mutex::new(local),
            prefs: Mutex::new(prefs),
            closed: Mutex::new(closed),
            wake: Mutex::default(),
            rang: Mutex::default(),
            push: OnceLock::new(),
            name: Mutex::new(cfg.name),
            events: broadcast::channel(1024).0,
            clock: AtomicU64::new(0),
            serve,
            big: cfg.big,
            files,
            held: Mutex::new(Some(held)),
            budget: Mutex::new(budget),
            hosts: Mutex::new(hosts),
            verified: Mutex::default(),
            shared: Mutex::new(shared),
            synced: Mutex::default(),
            call: Mutex::new(None),
            media: broadcast::channel(128).0,
        });
        let gate = Some(gate(Arc::downgrade(&n)));
        let router = Router::builder(n.ep.clone())
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&n.files, gate))
            .accept(iroh_gossip::ALPN, n.gossip.clone())
            .accept(sync::ALPN, Proto(Arc::downgrade(&n)))
            .accept(BLOB, Proto(Arc::downgrade(&n)))
            .accept(call::ALPN, Proto(Arc::downgrade(&n)))
            .spawn();
        let _ = n.router.set(router);
        n.persist();
        n.save_hosts();
        if serve.is_some() {
            let ids: Vec<Id> = lock(&n.hosts).keys().copied().collect();
            for id in ids {
                let peers = n.owners(&id).into_iter().take(16);
                n.open(None, id, peers.map(EndpointAddr::new).collect())?;
            }
        } else {
            let own = n.open(Some(own_inv), n.own, vec![])?;
            let named = n.view(&n.own, |s| s.profile(&n.id()).is_some());
            if cfg.link.is_none() && named == Some(false) {
                let (name, kem) = (n.name(), n.me.pk());
                n.post(&own, Op::Profile { name, kem }).await?;
            }
            n.reconcile().await;
        }
        tokio::spawn(heartbeat(Arc::downgrade(&n)));
        Ok(n)
    }

    /// Who you are, on every device.
    pub fn id(&self) -> Id {
        self.me.id()
    }

    pub fn name(&self) -> String {
        lock(&self.name).clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    fn emit(&self, e: Event) {
        let _ = self.events.send(e);
    }

    fn sync(self: &Arc<Self>, sp: &Arc<Space>, peer: impl Into<EndpointAddr>) {
        tokio::spawn(self.clone().sync_with(sp.clone(), peer.into()));
    }

    fn seal(
        &self,
        sp: &Space,
        key: &Key,
        ts: u64,
        op: Op,
    ) -> (Signed, Vec<u8>) {
        let (me, dev) = (&self.me.user, self.ep.id());
        op::seal(&sp.id, key, me, dev, ts, op)
    }

    fn persist(&self) {
        let l = lock(&self.local);
        if let Err(e) = store::save(&self.dir.join("local"), &*l) {
            warn!("saving local state: {e}");
        }
    }

    /// Out of sight, kept: back with its next message, or when opened.
    pub fn hide(&self, id: &Id, on: bool) {
        let last = self.last(id).unwrap_or_default();
        let mut c = lock(&self.closed);
        let _ = if on { c.insert(*id, last) } else { c.remove(id) };
        if let Err(e) = store::save(&self.dir.join("closed"), &*c) {
            warn!("saving closed: {e}");
        }
        drop(c);
        self.emit(Event::Changed { space: *id });
    }

    pub fn hidden(&self, id: &Id) -> bool {
        let t = lock(&self.closed).get(id).copied();
        t.is_some_and(|t| self.last(id).unwrap_or_default() <= t)
    }

    fn last(&self, id: &Id) -> Option<u64> {
        let last = |s: &State| {
            s.channels().iter().filter_map(|(c, _)| s.latest(c)).max()
        };
        self.view(id, last).flatten()
    }

    fn save_prefs(&self) {
        let p = lock(&self.prefs);
        if let Err(e) = store::save(&self.dir.join("prefs"), &*p) {
            warn!("saving prefs: {e}");
        }
    }

    pub fn level(&self, id: &Id) -> Level {
        lock(&self.prefs).level.get(id).copied().unwrap_or_default()
    }

    pub fn set_level(&self, id: &Id, l: Level) {
        lock(&self.prefs).level.insert(*id, l);
        self.save_prefs();
    }

    /// Per channel or thread, the newest unread (by level), once across wakes.
    pub fn notices(&self) -> Vec<Notice> {
        let (me, mut out, mut live) = (self.id(), vec![], HashSet::new());
        for id in self.spaces() {
            let (level, mark) = {
                let mut p = lock(&self.prefs);
                let l = p.level.get(&id).copied().unwrap_or_default();
                (l, *p.told.entry(id).or_insert_with(now))
            };
            let Some((name, dm, rooms, newest)) = self.view(&id, |s| {
                let rooms = s.rooms();
                let newest = rooms.iter().filter_map(|r| s.latest(&r.0)).max();
                (s.name().to_string(), s.dm.is_some(), rooms, newest)
            }) else {
                continue;
            };
            for (ch, room) in rooms.iter().filter(|_| level != Level::Off) {
                let since = self.since(&id, ch).max(mark);
                let m = self.view(&id, |s| match level {
                    Level::All => s.last_from(ch, since, &me),
                    _ => s.last_mention(ch, since, &me),
                });
                let Some(m) = m.flatten() else { continue };
                let title = match dm {
                    true => m.name.clone(),
                    false => format!("{} in #{room} ({name})", m.name),
                };
                let body =
                    if m.text.is_empty() { "(file)".into() } else { m.text };
                let (space, room) = (id, *ch);
                out.push(Notice {
                    space,
                    room,
                    title,
                    body,
                    ring: false,
                    hush: false,
                });
            }
            // Others' calls, each once while it lasts; not the one we are in.
            let calls = self.get(&id).map(|sp| self.callers(&sp, None));
            for (_, ch, u) in calls.unwrap_or_default() {
                let fresh = u != me && lock(&self.rang).insert((id, u, ch));
                live.insert((id, u, ch));
                let tell = level != Level::Off && (dm || level == Level::All);
                if !fresh || !tell || self.in_call() == Some((id, ch)) {
                    continue;
                }
                // Named in any space we share: a new DM may lack their profile.
                let name_in = |sp: &Id| {
                    self.view(sp, |s| s.profile(&u).map(str::to_string))
                };
                let mut all = std::iter::once(id).chain(self.spaces());
                let who = all.find_map(|sp| name_in(&sp).flatten());
                let who = who.unwrap_or_else(|| u.to_string()[..8].into());
                let room = rooms.iter().find(|r| r.0 == ch);
                let room = room.map_or("call".into(), |r| r.1.to_string());
                let (title, body) = match dm {
                    true => (who, "is calling you".into()),
                    false => (
                        format!("{who} in #{room} ({name})"),
                        "started a call".into(),
                    ),
                };
                let (space, room) = (id, ch);
                out.push(Notice {
                    space,
                    room,
                    title,
                    body,
                    ring: dm,
                    hush: false,
                });
            }
            let p = &mut lock(&self.prefs).told;
            p.insert(id, mark.max(newest.unwrap_or(0).min(now())));
        }
        lock(&self.rang).retain(|c| live.contains(c));
        self.save_prefs();
        out
    }

    pub fn pushed(&self) -> bool {
        lock(&self.prefs).push.is_some()
    }

    /// Our UnifiedPush endpoint, or none: sealed to every space we are in.
    pub async fn set_push(self: &Arc<Self>, url: Option<String>) {
        let old = std::mem::replace(&mut lock(&self.prefs).push, url.clone());
        self.save_prefs();
        if old != url {
            for id in self.spaces() {
                let _ = self.tell_push(&id).await;
            }
        }
    }

    /// Our push, as this space's members should have it: a doorbell's
    /// code made for the space and its epoch, retired by the next rekey.
    async fn tell_push(self: &Arc<Self>, id: &Id) -> Result<()> {
        let Some(url) = lock(&self.prefs).push.clone() else { return Ok(()) };
        let sp = self.get(id)?;
        let Some(inv) = &sp.inv else { return Ok(()) };
        let url = match url.starts_with("irch:wake:") {
            true => {
                let t = tag(&lock(&sp.book).key(inv).0);
                crate::wake::allow(&self.dir, id, &t)?;
                crate::wake::for_space(&url, id, &t).context("bad doorbell")?
            }
            false => url,
        };
        let (me, dev) = (self.id(), self.ep.id());
        let has = self.view(id, |s| s.push(&me, &dev).map(str::to_string));
        if has.flatten().as_deref() == Some(&url) {
            return Ok(());
        }
        self.post(&sp, Op::Push { url: Some(url) }).await?;
        Ok(())
    }

    /// Wake sleeping devices: a doorbell gets `raw` until it answers; a push
    /// service an empty ping (they fetch from us), at most every 30 s.
    fn wake(self: &Arc<Self>, id: &Id, raw: Option<Vec<u8>>) {
        self.wake_who(id, None, raw);
    }

    fn wake_who(
        self: &Arc<Self>,
        id: &Id,
        who: Option<&Id>,
        raw: Option<Vec<u8>>,
    ) {
        let dev = self.ep.id();
        let urls = self.view(id, |s| s.pushes_of(who)).unwrap_or_default();
        let (news, id) = (Arc::new(raw), *id);
        for (_, url) in
            urls.into_iter().filter(|u| u.0 != dev && pushable(&u.1))
        {
            if url.starts_with("irch:ring:") {
                let (n, news) = (Arc::downgrade(self), news.clone());
                tokio::spawn(async move {
                    // No answer (a phone between networks): retry for a minute.
                    for wait in [0, 3, 10, 45] {
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        let Some(n) = n.upgrade() else { return };
                        let raw = news.as_deref();
                        let r = crate::wake::ring(&n.ep, &url, &id, raw);
                        match r.await {
                            Ok(()) => return,
                            Err(e) => warn!("wake: {e:#}"),
                        }
                    }
                });
                continue;
            }
            let wait = {
                let mut w = lock(&self.wake);
                let (last, held) =
                    w.entry(url.clone()).or_insert((None, false));
                let since =
                    last.map_or(Duration::MAX, |t: Instant| t.elapsed());
                let wait = Duration::from_secs(30).saturating_sub(since);
                match (wait.is_zero(), *held) {
                    (_, true) => continue,
                    (true, _) => *last = Some(Instant::now()),
                    (false, _) => *held = true,
                }
                wait
            };
            let n = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(wait).await;
                let Some(n) = n.upgrade() else { return };
                if !wait.is_zero() {
                    lock(&n.wake)
                        .insert(url.clone(), (Some(Instant::now()), false));
                }
                let local = url.starts_with("http://").then(http);
                let c = local.as_ref().unwrap_or(n.push.get_or_init(push_http));
                let r = c.post(&url).header("TTL", "86400");
                let r = r.header("Urgency", "high").body("1").send().await;
                if let Err(e) = r {
                    warn!("push: {e}");
                }
            });
        }
    }

    fn get(&self, id: &Id) -> Result<Arc<Space>> {
        let m = self.spaces.read().unwrap_or_else(|e| e.into_inner());
        m.get(id).cloned().ok_or_else(|| anyhow!("not in that space"))
    }

    fn all(&self) -> Vec<Arc<Space>> {
        let m = self.spaces.read().unwrap_or_else(|e| e.into_inner());
        m.values().cloned().collect()
    }

    /// Spaces you are in, by name; DMs last.
    pub fn spaces(&self) -> Vec<Id> {
        let mut v: Vec<_> = (self.all().into_iter())
            .filter(|s| s.id != self.own && s.inv.is_some())
            .filter_map(|s| {
                self.view(&s.id, |st| {
                    (st.dm.is_some(), st.name().to_lowercase(), s.id)
                })
            })
            .collect();
        v.sort();
        v.into_iter().map(|v| v.2).collect()
    }

    pub fn held(&self) -> Vec<(Id, usize)> {
        let all = self.all().into_iter().filter(|s| s.id != self.own);
        all.map(|s| (s.id, lock(&s.book).at.len())).collect()
    }

    pub fn view<R>(&self, id: &Id, f: impl FnOnce(&State) -> R) -> Option<R> {
        self.get(id).ok().map(|s| f(&lock(&s.book).st))
    }

    /// Keys moved on without us: we can read the past only.
    pub fn locked(&self, id: &Id) -> bool {
        let Ok(sp) = self.get(id) else { return false };
        let Some(inv) = &sp.inv else { return false };
        let b = lock(&sp.book);
        // A DM with someone who retired their identity: a thief may read.
        let me = self.me.id();
        let gone = b.st.peer(&me).is_some_and(|p| b.st.out(&p));
        b.out || gone || b.key(inv).1
    }

    pub fn peers(&self, id: &Id) -> usize {
        self.get(id).map_or(0, |s| lock(&s.nbrs).len())
    }

    /// Connected devices here, each a member's, a storage node's, or unknown.
    pub fn neighbors(&self, id: &Id) -> Vec<(EndpointId, String)> {
        let Ok(sp) = self.get(id) else { return vec![] };
        let near: Vec<_> = lock(&sp.nbrs).iter().copied().collect();
        let mine = self.view(&self.own, |s| s.storage()).unwrap_or_default();
        let b = lock(&sp.book);
        let host = |d: &EndpointId| {
            b.st.hosts().iter().any(|h| h.id == *d)
                || mine.iter().any(|(a, _)| a.id == *d)
        };
        let who = |d: &EndpointId| match b.st.owner_of(d) {
            Some(u) => b.st.display_name(&u),
            None if host(d) => "a storage node".into(),
            None => "a device not known yet".into(),
        };
        near.into_iter().map(|d| (d, who(&d))).collect()
    }

    /// Members heard from in the last few minutes, and us unless invisible.
    pub fn online(&self, id: &Id) -> HashSet<Id> {
        let Ok(sp) = self.get(id) else { return HashSet::new() };
        let seen = lock(&sp.seen);
        let mut v: HashSet<_> = seen
            .iter()
            .filter(|(_, t)| t.elapsed() < ONLINE)
            .map(|(u, _)| *u)
            .collect();
        if !self.invisible() {
            v.insert(self.id());
        }
        v
    }

    pub fn invisible(&self) -> bool {
        self.invisible.load(Relaxed)
    }

    pub async fn set_invisible(&self, on: bool) {
        self.invisible.store(on, Relaxed);
        lock(&self.local).invisible = on;
        self.persist();
        for sp in self.all() {
            self.signal(&sp, if on { Op::Gone } else { Op::Here }).await;
        }
    }

    fn since(&self, id: &Id, ch: &Ch) -> u64 {
        let l = lock(&self.local);
        l.read.get(&(*id, *ch)).or(l.joined.get(id)).copied().unwrap_or(0)
    }

    pub fn mark_read(&self, id: &Id, ch: &Ch) {
        let latest = self.view(id, |s| s.latest(ch)).flatten().unwrap_or(0);
        let latest = latest.min(now());
        if self.since(id, ch) < latest {
            lock(&self.local).read.insert((*id, *ch), latest);
            self.persist();
        }
    }

    /// `(unread, mentions)` since `ch` was last read here.
    pub fn unread(&self, id: &Id, ch: &Ch) -> (usize, usize) {
        let (since, me) = (self.since(id, ch), self.id());
        self.view(id, |s| s.unread(ch, since, &me)).unwrap_or_default()
    }

    /// Strictly increasing wall-clock ms.
    fn tick(&self) -> u64 {
        let t = now();
        let prev = self.clock.fetch_max(t, Relaxed);
        if prev >= t { self.clock.fetch_add(1, Relaxed) + 1 } else { t }
    }

    fn open(
        self: &Arc<Self>,
        inv: Option<Invite>,
        id: Id,
        peers: Vec<EndpointAddr>,
    ) -> Result<Arc<Space>> {
        if let Ok(sp) = self.get(&id) {
            return Ok(sp);
        }
        let (log, recs) = Log::open(&self.log(&id))?;
        let st = inv.as_ref().map(State::new).unwrap_or_default();
        let mut b = Book::new(log, st);
        let mut work = vec![];
        for (o, checked, raw) in recs {
            let Ok(e) = postcard::from_bytes::<Env>(&raw) else { continue };
            let i = Id(blake3::hash(&raw).into());
            b.index(i, e.ts, o, raw.len());
            work.push((i, e, Src::Log(checked)));
        }
        if let Some(i) = &inv {
            b.keys.insert(tag(&i.key), i.key);
            b.fold(&id, &self.me, work);
        }
        let boot = peers.iter().chain(inv.iter().flat_map(|i| &i.peers));
        boot.filter(|a| a.id != self.ep.id())
            .for_each(|a| self.lookup.add_endpoint_info(a.clone()));
        let (book, tx, nbrs) =
            (Mutex::new(b), Mutex::new(None), Mutex::default());
        let sp = Arc::new(Space {
            id,
            inv,
            book,
            tx,
            nbrs,
            seen: Mutex::default(),
            heard: Mutex::default(),
            calls: Mutex::default(),
            busy: Mutex::default(),
            vouched: AtomicUsize::new(0),
            room: Mutex::new([0; 32]),
            moved: Arc::default(),
            undelivered: Mutex::default(),
            task: OnceLock::new(),
        });
        {
            let mut m = self.spaces.write().unwrap_or_else(|e| e.into_inner());
            if let Some(sp) = m.get(&id) {
                return Ok(sp.clone());
            }
            m.insert(id, sp.clone());
        }
        let boot: Vec<_> = peers.iter().map(|a| a.id).collect();
        let task = tokio::spawn(pump(self.clone(), Arc::downgrade(&sp), boot));
        let _ = sp.task.set(AbortOnDropHandle::new(task));
        self.bootstrap(&sp).into_iter().take(4).for_each(|p| self.sync(&sp, p));
        self.emit(Event::Changed { space: id });
        tokio::spawn(self.clone().effects(sp.clone()));
        Ok(sp)
    }

    fn log(&self, id: &Id) -> PathBuf {
        self.dir.join("spaces").join(format!("{id}.log"))
    }

    fn close(&self, id: &Id) {
        drop(self.spaces.write().unwrap_or_else(|e| e.into_inner()).remove(id));
        let _ = std::fs::remove_file(self.log(id));
        let mut l = lock(&self.local);
        l.read.retain(|(s, _), _| s != id);
        l.joined.remove(id);
        drop(l);
        if lock(&self.hosts).remove(id).is_some() {
            self.save_hosts();
        }
        self.persist();
        if lock(&self.closed).contains_key(id) {
            self.hide(id, false);
        }
        self.emit(Event::Changed { space: *id });
    }

    fn bootstrap(&self, sp: &Space) -> Vec<EndpointId> {
        let b = lock(&sp.book);
        let hosts = b.st.hosts();
        hosts.iter().for_each(|a| self.lookup.add_endpoint_info(a.clone()));
        let mut v: Vec<_> = hosts.into_iter().map(|a| a.id).collect();
        v.extend(b.st.devices(16));
        v.extend(sp.inv.iter().flat_map(|i| i.peers.iter().map(|a| a.id)));
        v.extend(self.owners(&sp.id).into_iter().take(16));
        let me = self.ep.id();
        let mut seen = HashSet::new();
        v.retain(|p| *p != me && seen.insert(*p));
        v
    }

    pub(crate) fn ingest(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        raws: Vec<Vec<u8>>,
        mine: bool,
    ) -> usize {
        // A storage node keeps only the epochs its owners named.
        let tags = sp.inv.is_none().then(|| {
            let h = lock(&self.hosts);
            h.get(&sp.id).map(|h| h.tags.clone()).unwrap_or_default()
        });
        let new = {
            let mut b = lock(&sp.book);
            let mut fresh = vec![];
            for raw in raws {
                let Ok(e) = postcard::from_bytes::<Env>(&raw) else { continue };
                let i = Id(blake3::hash(&raw).into());
                if !e.live && e.ts <= now() + SKEW && !b.has(&i) {
                    fresh.push((i, e, raw));
                }
            }
            match sp.inv {
                // A storage node keeps, unopened, what vouched devices send,
                // within the day's budget.
                None => {
                    let tags = tags.unwrap_or_default();
                    let ok =
                        |e: &Env| tags.is_empty() || tags.contains(&e.epoch);
                    let fits =
                        |r: &[u8]| self.budget(&sp.id, r.len() as u64).is_ok();
                    let keep =
                        fresh.into_iter().filter(|(_, e, r)| ok(e) && fits(r));
                    keep.filter(|(i, e, r)| b.store(*i, e.ts, r, 0, false))
                        .count()
                }
                Some(_) => {
                    let w = fresh
                        .into_iter()
                        .map(|(i, e, r)| (i, e, Src::Net(r, mine)));
                    b.fold(&sp.id, &self.me, w.collect())
                }
            }
        };
        if new > 0 {
            self.emit(Event::Changed { space: sp.id });
            tokio::spawn(self.clone().effects(sp.clone()));
        }
        new
    }

    /// DMs to accept, spaces to open or close, epochs to replace, keys to pass.
    async fn effects(self: Arc<Self>, sp: Arc<Space>) {
        // A new key: a new room, which our storage nodes hear of at once.
        if self.rehome(&sp) && sp.inv.is_some() {
            self.stores(&sp.id).into_iter().for_each(|s| self.sync(&sp, s.0));
            // Our ring code too: the old one, which those cut off hold, dies.
            if sp.id != self.own && !self.locked(&sp.id) {
                let _ = self.tell_push(&sp.id).await;
            }
        }
        let (knocks, fix) = {
            let mut b = lock(&sp.book);
            let admin = b.st.role(&self.id()) == Role::Admin;
            let locked = sp.inv.as_ref().is_none_or(|i| b.key(i).1);
            let lost = !locked && !b.orphans().is_empty();
            let fix = admin && b.st.dm.is_none() && (b.st.leaks() || lost);
            (std::mem::take(&mut b.knocks), fix)
        };
        let me = self.id();
        for inv in knocks {
            let known =
                self.view(&self.own, |s| s.spaces().contains_key(&inv.id()));
            // One DM per person: a flood of knocks opens no more.
            let with = |sp: &Arc<Space>| {
                self.view(&sp.id, |s| s.peer(&me) == Some(inv.owner))
                    == Some(true)
            };
            if known != Some(false) || self.all().iter().any(with) {
                continue;
            }
            // All news: its first message came before we were in it.
            lock(&self.local).joined.insert(inv.id(), 0);
            lock(&self.prefs).told.insert(inv.id(), 0);
            if let Err(e) = self.enter(inv).await {
                warn!("accepting a DM: {e}");
            }
        }
        if sp.id == self.own {
            self.reconcile().await;
        }
        // Members came or went: our storage nodes hear at once whom to take.
        let n = sp.devices().len();
        if sp.inv.is_some() && sp.vouched.swap(n, Relaxed) != n {
            self.stores(&sp.id).into_iter().for_each(|s| self.sync(&sp, s.0));
        }
        if fix {
            // Admins may race to fix it; a little jitter keeps that rare.
            let wait = 1000 + u16::from_le_bytes(rand()) as u64 % 9000;
            tokio::time::sleep(Duration::from_millis(wait)).await;
            if let Err(e) = self.repair(&sp).await {
                warn!("rekey: {e}");
            }
        }
    }

    /// Replace an epoch a banned member may hold; else hand on keys it cut off.
    async fn repair(self: &Arc<Self>, sp: &Arc<Space>) -> Result<()> {
        let (leaks, lost) = {
            let b = lock(&sp.book);
            (b.st.leaks(), b.orphans())
        };
        if leaks {
            return self.rekey(&sp.id).await;
        }
        for key in lost {
            self.post(sp, Op::Back { key }).await?;
        }
        Ok(())
    }

    /// As an admin, rekey now if someone banned may hold the current key.
    async fn secure(self: &Arc<Self>, id: &Id) -> Result<()> {
        let me = self.id();
        match self.view(id, |s| s.leaks() && s.role(&me) == Role::Admin) {
            Some(true) => self.rekey(id).await,
            _ => Ok(()),
        }
    }

    /// Open and close spaces to match our own space's record.
    async fn reconcile(self: &Arc<Self>) {
        let me = self.id();
        let name = self.view(&self.own, |s| s.profile(&me).map(str::to_string));
        if let Some(name) = name.flatten() {
            *lock(&self.name) = name;
        }
        let want =
            self.view(&self.own, |s| s.spaces().clone()).unwrap_or_default();
        for (id, inv) in want {
            match inv {
                Some(inv) if self.get(&id).is_err() => {
                    lock(&self.local).joined.entry(id).or_insert_with(now);
                    let mine = self.get(&self.own).map(|o| self.bootstrap(&o));
                    let mine =
                        mine.unwrap_or_default().into_iter().map(Into::into);
                    if let Err(e) = self.open(Some(inv), id, mine.collect()) {
                        warn!("opening {id:?}: {e}");
                    }
                }
                None if self.get(&id).is_ok() => self.close(&id),
                _ => {}
            }
        }
        self.persist();
    }

    async fn post(self: &Arc<Self>, sp: &Arc<Space>, op: Op) -> Result<Id> {
        Ok(self.send(sp, op).await?.0)
    }

    async fn send(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        op: Op,
    ) -> Result<(Id, Vec<u8>)> {
        let inv = sp.inv.as_ref().ok_or_else(|| anyhow!("only hosted here"))?;
        let (key, ts) = {
            let b = lock(&sp.book);
            let (key, locked) = b.key(inv);
            // Locked out, admins can still ban and rekey, dated after the rest.
            let fix = matches!(op, Op::Role { .. } | Op::Rekey { .. });
            ensure!(!locked || fix, LOCKED);
            let me = self.me.id();
            let gone = b.st.peer(&me).is_some_and(|p| b.st.out(&p));
            ensure!(!gone || op.live(), "they retired this identity");
            let after = if fix { b.st.horizon() + 1 } else { 0 };
            (key, self.tick().max(after))
        };
        let (s, raw) = self.seal(sp, &key, ts, op);
        self.ingest(sp, vec![raw.clone()], true);
        if sp.nbrs().is_empty() {
            lock(&sp.undelivered).insert(s.id);
        }
        match sp.tx().filter(|_| raw.len() <= GOSSIP_MAX) {
            Some(tx) => drop(tx.broadcast(raw.clone().into()).await),
            None => sp.nbrs().into_iter().for_each(|p| self.sync(sp, p)),
        }
        Ok((s.id, raw))
    }

    /// Publish a user-level op; the fold enforces who may do what.
    pub async fn publish(self: &Arc<Self>, id: &Id, op: Op) -> Result<Id> {
        let fits = |t: &str, max| !t.trim().is_empty() && t.len() <= max;
        let ok = match &op {
            Op::Post { text: t, files, .. } => {
                let empty = t.is_empty() && !files.is_empty();
                (empty || fits(t, MAX_TEXT)) && files.len() <= 10
            }
            Op::Edit { text: Some(t), .. } => fits(t, MAX_TEXT),
            Op::Rename { name: n } | Op::Channel { name: Some(n), .. } => {
                fits(n, MAX_NAME)
            }
            // Named nothing: deleted.
            Op::Topic { name: n, .. } => n.is_empty() || fits(n, MAX_NAME),
            Op::React { emoji, .. } => fits(emoji, 32),
            Op::Role { role, .. } => *role != Role::Banned,
            Op::Order { channels } => channels.len() <= 500,
            Op::Edit { .. } | Op::Channel { .. } | Op::Pin { .. } => true,
            Op::Owner { .. } => true,
            _ => false,
        };
        let ok = ok && op::fits(&op);
        ensure!(ok, "that cannot be sent (empty, too long, or not allowed)");
        ensure!(*id != self.own, "not a space");
        let role = matches!(op, Op::Role { .. });
        let msg = matches!(op, Op::Post { .. });
        let (r, raw) = self.send(&self.get(id)?, op).await?;
        if msg {
            self.wake(id, Some(raw));
        }
        // A demotion can revive an older epoch: replace it now, not jittered.
        if role {
            self.secure(id).await?;
        }
        Ok(r)
    }

    async fn enter(self: &Arc<Self>, inv: Invite) -> Result<Id> {
        let id = inv.id();
        ensure!(self.serve.is_none(), "storage nodes do not join spaces");
        let own = self.get(&self.own)?;
        let mut mine = inv.clone();
        mine.peers.insert(0, self.ep.addr());
        self.post(&own, Op::Joined { invite: mine }).await?;
        lock(&self.local).joined.entry(id).or_insert_with(now);
        let sp = self.open(Some(inv), id, vec![])?;
        let (name, kem) = (self.name(), self.me.pk());
        self.post(&sp, Op::Profile { name, kem }).await?;
        let _ = self.tell_push(&id).await;
        Ok(id)
    }

    pub async fn create(self: &Arc<Self>, name: &str) -> Result<Id> {
        let name = named(name)?;
        let inv = Invite::new(self.id(), name, None);
        let (epoch, id) = (tag(&inv.key), self.enter(inv).await?);
        let sp = self.get(&id)?;
        self.post(&sp, Op::Rekey { epoch, boxes: vec![] }).await?;
        self.post(&sp, Op::Rename { name: name.into() }).await?;
        let general =
            Op::Channel { id: Ch(rand()), name: Some("general".into()) };
        self.post(&sp, general).await?;
        Ok(id)
    }

    pub async fn join(self: &Arc<Self>, code: &str) -> Result<Id> {
        let inv = Invite::decode(code)?;
        let Ok(sp) = self.get(&inv.id()) else { return self.enter(inv).await };
        // Already in: a fresh invite carries the key a rekey left us out of.
        if lock(&sp.book).learn(&sp.id, &self.me, inv.key) {
            let mut mine = inv.clone();
            mine.peers.insert(0, self.ep.addr());
            self.post(&self.get(&self.own)?, Op::Joined { invite: mine })
                .await?;
            for a in inv.peers {
                self.lookup.add_endpoint_info(a.clone());
                self.sync(&sp, a);
            }
            self.emit(Event::Changed { space: sp.id });
            tokio::spawn(self.clone().effects(sp.clone()));
        }
        Ok(sp.id)
    }

    async fn addr(&self) -> EndpointAddr {
        let relay = !matches!(self.net, Net::Lan | Net::Direct);
        for _ in 0..100 {
            let a = self.ep.addr();
            if a.addrs.iter().any(|t| t.is_relay() || !relay && t.is_ip()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut a = self.ep.addr();
        // With a relay to meet at, keep IP addresses out of shared codes.
        if a.relay_urls().next().is_some() {
            a.addrs.retain(|a| a.is_relay());
        }
        a
    }

    pub async fn invite(&self, id: &Id) -> Result<String> {
        let sp = self.get(id)?;
        let mut inv =
            sp.inv.clone().ok_or_else(|| anyhow!("only hosted here"))?;
        let me = self.addr().await;
        let (key, locked) = lock(&sp.book).key(&inv);
        ensure!(!locked, LOCKED);
        inv.key = key;
        inv.name = self.view(id, |s| s.name().to_string()).unwrap_or_default();
        let hosts = self.view(id, |s| s.hosts()).unwrap_or_default();
        let others = self.bootstrap(&sp).into_iter().map(EndpointAddr::new);
        let mut seen = HashSet::new();
        let all = std::iter::once(me).chain(hosts).chain(others);
        inv.peers = all.filter(|a| seen.insert(a.id)).take(6).collect();
        Ok(inv.encode())
    }

    pub async fn leave(self: &Arc<Self>, id: &Id) -> Result<()> {
        ensure!(*id != self.own, "not a space");
        let own = self.get(&self.own)?;
        self.post(&own, Op::Left { space: *id }).await?;
        self.close(id);
        // Its files stop being served from here.
        let mine = self.sharing().into_iter().filter(|f| f.1 == *id);
        for (h, ..) in mine.collect::<Vec<_>>() {
            let _ = self.unshare(&h).await;
        }
        Ok(())
    }

    /// This identity is lost (a stolen phone): say so in every space, for
    /// good. Admins rekey it out; nothing it signs later counts.
    pub async fn burn(self: &Arc<Self>) -> Result<()> {
        ensure!(self.serve.is_none(), "storage nodes have no identity");
        for sp in self.all().into_iter().filter(|s| s.inv.is_some()) {
            if let Err(e) = self.send(&sp, Op::Burn).await {
                warn!("retiring in {:?}: {e:#}", sp.id);
            }
        }
        Ok(())
    }

    /// This identity's devices, newest first: (device, last wrote).
    pub fn devices(&self) -> Vec<(EndpointId, u64)> {
        let me = self.id();
        self.view(&self.own, |s| s.devices_of(&me)).unwrap_or_default()
    }

    pub fn device(&self) -> EndpointId {
        self.ep.id()
    }

    /// When a storage node or peer last synced with us.
    pub fn synced(&self, d: &EndpointId) -> Option<u64> {
        lock(&self.synced).get(d).copied()
    }

    pub async fn ban(
        self: &Arc<Self>,
        id: &Id,
        user: &Id,
        on: bool,
    ) -> Result<()> {
        let role = if on { Role::Banned } else { Role::Member };
        self.post(&self.get(id)?, Op::Role { user: *user, role }).await?;
        self.secure(id).await
    }

    /// Old invites stop working; history stays readable through `Back`.
    pub async fn rekey(self: &Arc<Self>, id: &Id) -> Result<()> {
        let sp = self.get(id)?;
        let inv = sp.inv.as_ref().ok_or_else(|| anyhow!("only hosted here"))?;
        let (new, old, boxes) = {
            let b = lock(&sp.book);
            ensure!(b.st.role(&self.id()) == Role::Admin, "only admins rekey");
            let (new, old): (Key, Key) = (rand(), b.key(inv).0);
            let aad = rekey_aad(id, &tag(&new));
            let to = b.st.members().into_iter().filter_map(|m| {
                Some((m.id, kem_seal(b.st.kem(&m.id)?, &aad, &new)?))
            });
            (new, old, to.collect())
        };
        lock(&sp.book).keys.insert(tag(&new), new);
        self.post(&sp, Op::Rekey { epoch: tag(&new), boxes }).await?;
        self.post(&sp, Op::Back { key: old }).await?;
        Ok(())
    }

    /// A direct-message space with `user`, announced to them in `via`.
    pub async fn dm(self: &Arc<Self>, via: &Id, user: &Id) -> Result<Id> {
        ensure!(*user != self.id(), "that is you");
        let me = self.id();
        for sp in self.all() {
            if self.view(&sp.id, |s| s.peer(&me) == Some(*user)) == Some(true) {
                self.hide(&sp.id, false);
                return Ok(sp.id);
            }
        }
        let them = self.view(via, |s| {
            (s.kem(user).map(<[u8]>::to_vec), s.display_name(user))
        });
        let (Some(pk), name) = them.context("not in that space")? else {
            bail!("they have not shared a key yet");
        };
        let mut inv = Invite::new(me, &name, Some(*user));
        inv.peers = vec![self.addr().await];
        let sealed = kem_seal(&pk, &via.0, &postcard::to_allocvec(&inv)?)
            .context("bad key")?;
        let (epoch, id) = (tag(&inv.key), self.enter(inv).await?);
        self.post(&self.get(&id)?, Op::Rekey { epoch, boxes: vec![] }).await?;
        let knock = self.send(&self.get(via)?, Op::Knock { sealed }).await?;
        // Their sleeping phones: woken to take it, and what follows.
        self.wake_who(via, Some(user), Some(knock.1));
        Ok(id)
    }

    pub async fn set_name(self: &Arc<Self>, name: &str) -> Result<()> {
        let name = named(name)?;
        *lock(&self.name) = name.into();
        for sp in self.all().into_iter().filter(|s| s.inv.is_some()) {
            let op = Op::Profile { name: name.into(), kem: self.me.pk() };
            self.post(&sp, op).await?;
        }
        Ok(())
    }

    pub async fn typing(&self, id: &Id, channel: Ch) {
        if let Ok(sp) = self.get(id) {
            self.signal(&sp, Op::Typing { channel }).await;
        }
    }

    /// Live ops; invisible, only leaving and calls. Returns the sealed op.
    async fn signal(&self, sp: &Space, op: Op) -> Option<Vec<u8>> {
        let inv = sp.inv.as_ref()?;
        let (key, locked) = lock(&sp.book).key(inv);
        let quiet = !matches!(op, Op::Call { .. } | Op::Gone);
        if locked || quiet && self.invisible() {
            return None;
        }
        let raw = self.seal(sp, &key, self.tick(), op).1;
        if let Some(tx) = sp.tx() {
            let _ = tx.broadcast(raw.clone().into()).await;
        }
        Some(raw)
    }

    /// What a ring brought (wake.rs): taken as if gossiped.
    pub fn deliver(self: &Arc<Self>, news: crate::wake::News) {
        if let Ok(sp) = self.get(&news.0) {
            self.signaled(&sp, &news.1, self.ep.id());
        }
    }

    /// Join a channel's call (leaving any other), or with `None` leave.
    pub async fn call(self: &Arc<Self>, to: Option<(Id, Ch)>) -> Result<()> {
        let old = lock(&self.call).take();
        if let Some(c) = old {
            let sp = self.get(&c.space);
            sp.iter().for_each(|sp| self.emit(Event::Call { space: sp.id }));
            if let Ok(sp) = sp {
                let raw = self.signal(&sp, Op::Call { channel: None }).await;
                // A DM's phones, ringing: hung up, they stop.
                if self.view(&sp.id, |s| s.dm.is_some()) == Some(true) {
                    self.wake(&sp.id, raw);
                }
            }
        }
        let Some((id, ch)) = to else { return Ok(()) };
        let sp = self.get(&id)?;
        ensure!(!self.locked(&id), LOCKED);
        let run = u64::from_le_bytes(rand());
        *lock(&self.call) = Some(Call::new(id, ch, run));
        let raw = self.signal(&sp, Op::Call { channel: Some(ch) }).await;
        self.emit(Event::Call { space: id });
        self.wake(&id, raw); // sleeping phones: someone is calling
        tokio::spawn(self.clone().ring(run));
        Ok(())
    }

    pub fn notes(&self) -> Option<Vec<String>> {
        self.notes.as_deref().map(crate::notes::list)
    }

    pub fn note(&self, file: &str, md: &str) -> Result<()> {
        let dir = self.notes.as_deref().context("no notes folder here")?;
        crate::notes::append(dir, file, md)
    }

    pub fn in_call(&self) -> Option<(Id, Ch)> {
        lock(&self.call).as_ref().map(|c| (c.space, c.channel))
    }

    pub fn calls(&self, id: &Id) -> HashMap<Ch, Vec<Id>> {
        let Ok(sp) = self.get(id) else { return HashMap::new() };
        let mut v: HashMap<Ch, Vec<Id>> = HashMap::new();
        for (d, ch, u) in self.callers(&sp, None) {
            if d != self.ep.id() {
                v.entry(ch).or_default().push(u);
            }
        }
        if let Some((_, ch)) = self.in_call().filter(|c| c.0 == *id) {
            v.entry(ch).or_default().insert(0, self.id());
        }
        v
    }

    pub fn send_media(&self, f: &[u8]) {
        lock(&self.call).iter_mut().for_each(|c| c.send(f));
    }

    pub fn media(&self) -> broadcast::Receiver<(Id, Vec<u8>)> {
        self.media.subscribe()
    }

    /// Who says they are in a call (`ch`'s, if given): members, heard lately.
    fn callers(&self, sp: &Space, ch: Option<Ch>) -> Vec<(EndpointId, Ch, Id)> {
        // Keys moved on without us (a ban, say): out of every call.
        if self.locked(&sp.id) {
            return vec![];
        }
        let calls = lock(&sp.calls).clone();
        let ok = |u: &Id| self.view(&sp.id, |s| s.role(u) != Role::Banned);
        let live = |c: &Ch, t: &Instant| {
            t.elapsed() < CALLER && ch.is_none_or(|ch| ch == *c)
        };
        let v = calls
            .into_iter()
            .filter(|(u, (c, _, t))| live(c, t) && ok(u) == Some(true));
        v.map(|(u, (c, d, _))| (d, c, u)).collect()
    }

    /// In a call: say so every 15 s; one link per device in it, lower id dials.
    async fn ring(self: Arc<Self>, run: u64) {
        let mut t = tokio::time::interval(Duration::from_secs(1));
        for i in 1u64.. {
            t.tick().await;
            let cur = {
                let c = lock(&self.call);
                c.as_ref()
                    .filter(|c| c.run == run)
                    .map(|c| (c.space, c.channel))
            };
            let Some((id, ch)) = cur else { return };
            let Ok(sp) = self.get(&id) else { return };
            let want = self.callers(&sp, Some(ch));
            if i % 15 == 0 {
                let raw =
                    self.signal(&sp, Op::Call { channel: Some(ch) }).await;
                // A DM still ringing: wake its phones again; late ones hear it.
                let dm = self.view(&id, |s| s.dm.is_some()) == Some(true);
                let alone = want.iter().all(|w| w.2 == self.id());
                if dm && alone && i <= 45 {
                    self.wake(&id, raw);
                }
            }
            let dial: HashSet<_> = {
                let mut c = lock(&self.call);
                let Some(c) = c.as_mut().filter(|c| c.run == run) else {
                    return;
                };
                c.retain(|d| want.iter().any(|w| w.0 == *d));
                let me = self.ep.id();
                let new = want.into_iter().map(|w| w.0);
                new.filter(|d| *d > me && !c.has(d)).collect()
            };
            for d in dial {
                let hi = async {
                    let conn = self.ep.connect(d, call::ALPN).await.ok()?;
                    let user = self.greet(&conn).await?;
                    Some((user, conn))
                };
                let ten = Duration::from_secs(10);
                if let Ok(Some((user, conn))) =
                    tokio::time::timeout(ten, hi).await
                {
                    self.admit(user, conn);
                }
            }
        }
    }

    async fn answer(&self, conn: Connection) {
        let Some((id, ch)) = self.in_call() else { return };
        let Ok(sp) = self.get(&id) else { return };
        let d = conn.remote_id();
        if self.callers(&sp, Some(ch)).iter().any(|w| w.0 == d)
            && let Some(user) = self.greet(&conn).await
        {
            self.admit(user, conn);
        }
    }

    async fn greet(&self, conn: &Connection) -> Option<Id> {
        call::hello(conn, &self.me.user, self.ep.id()).await
    }

    /// A device its user proved theirs and said, from it, is in the call.
    fn admit(&self, user: Id, conn: Connection) {
        let Some((id, ch)) = self.in_call() else { return };
        let Ok(sp) = self.get(&id) else { return };
        if !self.callers(&sp, Some(ch)).contains(&(conn.remote_id(), ch, user))
        {
            return;
        }
        self.bind(conn.remote_id(), user);
        let mut c = lock(&self.call);
        if let Some(c) = c.as_mut().filter(|c| (c.space, c.channel) == (id, ch))
        {
            c.add(user, conn, self.media.clone());
        }
    }

    /// Everything another device needs to become you: keep it secret.
    pub async fn link_code(&self) -> String {
        let mut peers = vec![self.addr().await];
        peers.extend(self.view(&self.own, |s| s.hosts()).unwrap_or_default());
        code("link", &(self.me.seed, peers))
    }

    pub async fn storage_code(&self) -> Option<String> {
        let token = self.serve?;
        Some(code("host", &(self.addr().await, token)))
    }

    pub async fn add_storage(self: &Arc<Self>, code: &str) -> Result<()> {
        let (addr, token): (EndpointAddr, [u8; 16]) = uncode("host", code)?;
        let own = self.get(&self.own)?;
        let op = Op::Storage { addr: addr.clone(), token, on: true };
        self.post(&own, op).await?;
        self.host(&own, addr).await;
        Ok(())
    }

    /// Whether our storage node `node` keeps `space` too.
    pub async fn store(
        self: &Arc<Self>,
        space: &Id,
        node: &EndpointId,
        on: bool,
    ) -> Result<()> {
        let paired = self.view(&self.own, |s| s.storage()).unwrap_or_default();
        let (addr, _) = paired
            .into_iter()
            .find(|s| s.0.id == *node)
            .context("not one of your storage nodes")?;
        let (own, sp) = (self.get(&self.own)?, self.get(space)?);
        let (node, space) = (*node, *space);
        self.post(&own, Op::Store { node, space, on }).await?;
        let me = self.id();
        let admin = self.view(&space, |s| s.role(&me) == Role::Admin);
        match on {
            true => self.host(&sp, addr).await,
            false if admin == Some(true) => {
                let _ =
                    self.post(&sp, Op::Host { node: addr, on: false }).await;
            }
            false => {}
        }
        Ok(())
    }

    pub fn stored(&self, space: &Id) -> Vec<(EndpointId, bool)> {
        let all = *space == self.own;
        let v = self.view(&self.own, |s| {
            let v = s.storage().into_iter().map(|(a, _)| a.id);
            v.map(|n| (n, all || s.keeps(&n, space))).collect()
        });
        v.unwrap_or_default()
    }

    fn stores(&self, space: &Id) -> Vec<(EndpointAddr, [u8; 16])> {
        let all = *space == self.own;
        let v = self.view(&self.own, |s| {
            let mut v = s.storage();
            v.retain(|(a, _)| all || s.keeps(&a.id, space));
            v
        });
        v.unwrap_or_default()
    }

    async fn host(self: &Arc<Self>, sp: &Arc<Space>, addr: EndpointAddr) {
        self.lookup.add_endpoint_info(addr.clone());
        self.list(sp, &addr).await;
        let id = addr.id;
        self.sync(sp, addr);
        if let Some(tx) = sp.tx() {
            let _ = tx.join_peers(vec![id]).await;
        }
    }

    /// An admin names our storage node in the space, and how members have
    /// it keep their small files.
    async fn list(self: &Arc<Self>, sp: &Arc<Space>, addr: &EndpointAddr) {
        let me = self.id();
        let token = self.stores(&sp.id).into_iter().find(|s| s.0.id == addr.id);
        let token = token.map(|s| hold_token(&s.1, &sp.id));
        let (list, hold) = self
            .view(&sp.id, |s| {
                let admin = s.role(&me) == Role::Admin;
                let listed = s.hosts().iter().any(|h| h.id == addr.id);
                let held = token.is_none() || s.hold(&addr.id) == token;
                (admin && !listed, admin && !held && sp.id != self.own)
            })
            .unwrap_or_default();
        if list {
            let _ =
                self.post(sp, Op::Host { node: addr.clone(), on: true }).await;
        }
        if let Some(token) = token.filter(|_| hold) {
            let _ = self.post(sp, Op::Hold { node: addr.id, token }).await;
        }
    }

    fn blob(&self, h: &Id) -> PathBuf {
        self.dir.join("blobs").join(h.to_string())
    }

    /// Seal a file (in chunks, from disk) and keep its ciphertext here and
    /// on this user's storage nodes; send it in an `Op::Post`.
    pub async fn seal_file(
        self: &Arc<Self>,
        space: &Id,
        path: &Path,
        name: &str,
        password: Option<&str>,
    ) -> Result<File> {
        use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
        let size = std::fs::metadata(path)?.len();
        let key: Key = rand();
        let out = self.scratch("sealed")?;
        let (from, to) = (path.to_path_buf(), out.clone());
        tokio::task::spawn_blocking(move || {
            let r = std::io::BufReader::new(std::fs::File::open(from)?);
            let w = std::io::BufWriter::new(std::fs::File::create(to)?);
            crate::crypto::seal_stream(&key, r, w)
        })
        .await??;
        let (format, mode) = (Default::default(), ImportMode::TryReference);
        let opts = AddPathOptions { path: out, format, mode };
        let tag = self.files.add_path_with_opts(opts).with_tag().await?;
        let hash = Id(*tag.hash.as_bytes());
        self.keep(space, &hash, password).await?;
        if size <= PREVIEW as u64 {
            let (n, space) = (self.clone(), *space);
            tokio::spawn(async move { n.hold(&space, &hash).await });
        }
        let name = name.chars().take(128).collect();
        Ok(File { name, size, hash, key: Id(key) })
    }

    /// `seal_file`, from memory.
    pub async fn attach(
        self: &Arc<Self>,
        space: &Id,
        name: &str,
        data: &[u8],
    ) -> Result<File> {
        let tmp = self.scratch("in")?;
        std::fs::write(&tmp, data)?;
        let f = self.seal_file(space, &tmp, name, None).await;
        let _ = std::fs::remove_file(tmp);
        f
    }

    /// A fresh path in our sealed files' folder.
    fn scratch(&self, ext: &str) -> Result<PathBuf> {
        let dir = self.dir.join("sealed");
        std::fs::create_dir_all(&dir)?;
        Ok(dir.join(format!("{}.{ext}", Id(rand()))))
    }

    /// This user's storage nodes fetch `hash` from here. One that refuses
    /// it (past 5 GB, without its password) fails it; one offline does not.
    async fn keep(
        self: &Arc<Self>,
        space: &Id,
        hash: &Id,
        password: Option<&str>,
    ) -> Result<()> {
        for (addr, token) in self.stores(space) {
            let ask = Ask::Keep(token, *hash, password.map(Into::into));
            let wait = Duration::from_secs(30);
            match tokio::time::timeout(wait, self.ask(addr, &ask)).await {
                Ok(Ok(no)) if !no.is_empty() => {
                    bail!("{}", String::from_utf8_lossy(&no))
                }
                Ok(Err(e)) => warn!("storage node: {e:#}"),
                _ => {}
            }
        }
        Ok(())
    }

    /// The space's storage nodes (not this user's: `keep`) fetch a small
    /// file from here, so it shows for everyone while we are away.
    async fn hold(self: &Arc<Self>, space: &Id, hash: &Id) {
        let mine: Vec<_> =
            self.stores(space).into_iter().map(|s| s.0.id).collect();
        let hosts = self.view(space, |s| s.holds()).unwrap_or_default();
        for (addr, token) in
            hosts.into_iter().filter(|h| !mine.contains(&h.0.id))
        {
            let ask = Ask::Hold(*space, token, *hash);
            let ask = self.ask(addr, &ask);
            match tokio::time::timeout(Duration::from_secs(30), ask).await {
                Ok(Ok(no)) if !no.is_empty() => {
                    warn!("storage node: {}", String::from_utf8_lossy(&no))
                }
                Ok(Err(e)) => warn!("storage node: {e:#}"),
                _ => {}
            }
        }
    }

    /// One of this user's storage nodes.
    fn mine(&self, d: &EndpointId) -> bool {
        let s =
            self.view(&self.own, |s| s.storage().iter().any(|a| a.0.id == *d));
        s == Some(true)
    }

    /// Served from here to members while online: any size, no copy, unsealed
    /// (QUIC encrypts), checked chunk by chunk. `name`: else the path's.
    /// Served to `space`'s members only, until we stop sharing it.
    pub async fn offer(
        &self,
        space: &Id,
        path: &Path,
        name: Option<&str>,
    ) -> Result<File> {
        use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
        self.get(space)?;
        let files = &self.files;
        let path = std::path::absolute(path)?;
        let size = std::fs::metadata(&path)?.len();
        let (format, mode) = (Default::default(), ImportMode::TryReference);
        let opts = AddPathOptions { path: path.clone(), format, mode };
        let tag = files.add_path_with_opts(opts).with_tag().await?;
        let base = path.file_name().and_then(|n| n.to_str());
        let name: String =
            name.or(base).unwrap_or("file").chars().take(128).collect();
        let hash = Id(*tag.hash.as_bytes());
        let space = Some(*space);
        let (tag, n) = (tag.name.0.to_vec(), name.clone());
        lock(&self.shared).insert(hash, Shared { space, tag, name: n, size });
        self.save_shared();
        Ok(File { name, size, hash, key: crate::op::OFFERED })
    }

    /// Passed on from here too, to that space's members alone.
    fn passed(&self, space: &Id, f: &File) {
        let mut s = lock(&self.shared);
        if s.contains_key(&f.hash) {
            return;
        }
        let (space, name, size) = (Some(*space), f.name.clone(), f.size);
        s.insert(f.hash, Shared { space, tag: vec![], name, size });
        drop(s);
        self.save_shared();
    }

    fn save_shared(&self) {
        let s = lock(&self.shared);
        if let Err(e) = store::save(&self.dir.join("shared"), &*s) {
            warn!("saving shared files: {e}");
        }
    }

    /// What this device serves, as (hash, space, name, size).
    pub fn sharing(&self) -> Vec<(Id, Id, String, u64)> {
        let s = lock(&self.shared);
        let on = s.iter().filter_map(|(h, f)| Some((*h, f.space?, f)));
        on.map(|(h, sp, f)| (h, sp, f.name.clone(), f.size)).collect()
    }

    /// Stop serving an offered file; its members can no longer fetch it.
    pub async fn unshare(&self, h: &Id) -> Result<()> {
        let tag = {
            let mut s = lock(&self.shared);
            let f = s.get_mut(h).context("not shared from here")?;
            f.space = None;
            std::mem::take(&mut f.tag)
        };
        self.save_shared();
        if !tag.is_empty() {
            self.files.tags().delete(tag).await?;
        }
        Ok(())
    }

    /// A blob's real size, proven by its hash, from the first that has it.
    async fn verified(&self, peers: &[EndpointId], h: &Id) -> Option<u64> {
        let hash = iroh_blobs::Hash::from_bytes(h.0);
        for p in peers {
            // One that answers slowly, or never, gives way to the next.
            let size = async {
                let c = self.ep.connect(*p, iroh_blobs::ALPN).await.ok()?;
                let got =
                    iroh_blobs::get::request::get_verified_size(&c, &hash);
                Some(got.await.ok()?.0)
            };
            let ten = Duration::from_secs(10);
            if let Ok(Some(n)) = tokio::time::timeout(ten, size).await {
                return Some(n);
            }
        }
        None
    }

    /// Blob `h`, `len` bytes, from whoever has it (sender, members, storage
    /// nodes), resumed; served on.
    async fn pull(
        self: &Arc<Self>,
        space: &Id,
        h: &Id,
        len: u64,
    ) -> Result<&FsStore> {
        let hash = iroh_blobs::Hash::from_bytes(h.0);
        if !self.files.has(hash).await? {
            let sp = self.get(space)?;
            let mut peers: Vec<EndpointId> = self.bootstrap(&sp);
            peers.extend(sp.nbrs());
            peers.extend(self.stores(space).into_iter().map(|s| s.0.id));
            // Who we are, proven to each first: files go to members only.
            for p in peers.iter().take(4) {
                let s = self.clone().sync_with(sp.clone(), (*p).into());
                let _ = tokio::time::timeout(Duration::from_secs(10), s).await;
            }
            // Real size first: a post may claim 1 byte.
            let size = self.verified(&peers, h).await;
            ensure!(size == Some(len), "not reachable now, or not as said");
            // A link may drop mid-way (relay losses): asked again, it resumes.
            let got =
                self.files.downloader(&self.ep).download(hash, peers.repeat(3));
            got.await.ok().context(
                "not reachable now: ask whoever sent it to open irch",
            )?;
        }
        Ok(&self.files)
    }

    /// A copy (a reflink where it can): referenced instead, what the user
    /// later writes there is what members asking for its hash would get.
    async fn export(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
        to: &Path,
    ) -> Result<()> {
        let files = self.pull(space, &f.hash, f.size).await?;
        self.passed(space, f);
        std::fs::remove_file(to)?;
        files.export(iroh_blobs::Hash::from_bytes(f.hash.0), to).await?;
        Ok(())
    }

    /// A storage node keeping one of our spaces.
    fn hosting(&self, d: &EndpointId) -> bool {
        let has =
            |id: &Id| self.view(id, |s| s.hosts().iter().any(|a| a.id == *d));
        self.spaces().iter().any(|id| has(id) == Some(true))
    }

    fn member_device(&self, d: &EndpointId) -> bool {
        let has =
            |id: &Id| self.view(id, |s| s.devices(usize::MAX).contains(d));
        self.spaces().iter().any(|id| has(id) == Some(true))
    }

    /// An attachment's plaintext, in memory (a preview).
    pub async fn file(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
    ) -> Result<Vec<u8>> {
        ensure!(f.size <= PREVIEW as u64, "too big to open here: save it");
        let hash = iroh_blobs::Hash::from_bytes(f.hash.0);
        if f.offered() {
            let files = self.pull(space, &f.hash, f.size).await?;
            self.passed(space, f);
            return Ok(files.get_bytes(hash).await?.to_vec());
        }
        let len = crate::crypto::sealed_len(f.size);
        let ct = match std::fs::read(self.blob(&f.hash)) {
            Ok(ct) => ct,
            Err(_) => match self.pull(space, &f.hash, len).await {
                Ok(files) => {
                    let (n, h) = (self.clone(), (*space, f.hash));
                    tokio::spawn(async move {
                        let _ = n.keep(&h.0, &h.1, None).await;
                        n.hold(&h.0, &h.1).await;
                    });
                    files.get_bytes(hash).await?.to_vec()
                }
                Err(_) => self.legacy(space, f).await?,
            },
        };
        if !ct.starts_with(crate::crypto::STREAM) {
            let pt = crate::crypto::open(&f.key.0, b"irch file", &ct);
            return pt.context("damaged file");
        }
        let mut pt = vec![];
        crate::crypto::open_stream(&f.key.0, &ct[..], &mut pt)?;
        Ok(pt)
    }

    /// Sealed whole, by an older irch: asked for by hash.
    async fn legacy(&self, space: &Id, f: &File) -> Result<Vec<u8>> {
        let sp = self.get(space)?;
        let (mut peers, get) = (sp.nbrs(), Ask::Get(f.hash));
        peers.extend(self.bootstrap(&sp));
        for p in peers {
            let ask = self.ask(p.into(), &get);
            let wait = tokio::time::timeout(Duration::from_secs(20), ask);
            if let Ok(Ok(b)) = wait.await
                && blake3::hash(&b) == f.hash.0
            {
                std::fs::create_dir_all(self.dir.join("blobs"))?;
                store::write(&self.blob(&f.hash), &b)?;
                return Ok(b);
            }
        }
        bail!("nobody reachable has that file now")
    }

    /// A sealed file too big to preview, opened straight into `to`.
    async fn unseal(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
        to: &Path,
    ) -> Result<()> {
        let len = crate::crypto::sealed_len(f.size);
        let files = self.pull(space, &f.hash, len).await?;
        let tmp = self.scratch("out")?;
        files.export(iroh_blobs::Hash::from_bytes(f.hash.0), &tmp).await?;
        let (key, to, t) = (f.key.0, to.to_path_buf(), tmp.clone());
        let r = tokio::task::spawn_blocking(move || {
            let r = std::io::BufReader::new(std::fs::File::open(t)?);
            let w = std::io::BufWriter::new(std::fs::File::create(to)?);
            crate::crypto::open_stream(&key, r, w)
        })
        .await;
        let _ = std::fs::remove_file(tmp);
        Ok(r??)
    }

    /// An attachment written into `dir`, never over anything already there.
    pub async fn save(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
        dir: &Path,
    ) -> Result<PathBuf> {
        use std::io::{ErrorKind::AlreadyExists, Write};
        let small = !f.offered() && f.size <= PREVIEW as u64;
        let data = if small { self.file(space, f).await? } else { vec![] };
        let name = &f.clone().safe().name;
        let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
        let dot = if ext.is_empty() { "" } else { "." };
        std::fs::create_dir_all(dir)?;
        for i in 0.. {
            let n = match i {
                0 => name.to_string(),
                i => format!("{stem} ({i}){dot}{ext}"),
            };
            let p = dir.join(n);
            match std::fs::File::create_new(&p) {
                Ok(mut out) => {
                    out.write_all(&data)?;
                    if !small {
                        drop(out);
                        let got = match f.offered() {
                            true => self.export(space, f, &p).await,
                            false => self.unseal(space, f, &p).await,
                        };
                        got.inspect_err(|_| drop(std::fs::remove_file(&p)))?;
                    }
                    // From the internet, like a browser download: Windows
                    // and macOS check it before it runs.
                    #[cfg(windows)]
                    let _ = std::fs::write(
                        format!("{}:Zone.Identifier", p.display()),
                        "[ZoneTransfer]\r\nZoneId=3\r\n",
                    );
                    #[cfg(target_os = "macos")]
                    let _ = std::process::Command::new("/usr/bin/xattr")
                        .args(["-w", "com.apple.quarantine", "0081;0;irch;"])
                        .arg(&p)
                        .status();
                    return Ok(p);
                }
                Err(e) if e.kind() == AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        unreachable!()
    }

    async fn ask(&self, peer: EndpointAddr, a: &Ask) -> Result<Vec<u8>> {
        let c = self.ep.connect(peer, BLOB).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(a)?).await?;
        tx.finish()?;
        let b = sync::rest(&mut rx, PREVIEW + 1024).await?;
        c.close(0u32.into(), b"done");
        Ok(b)
    }

    /// Blob `h`, fetched from `from` in the background, kept.
    fn fetch(&self, h: &Id, from: EndpointId) {
        let (files, ep) = (self.files.clone(), self.ep.clone());
        let hash = iroh_blobs::Hash::from_bytes(h.0);
        tokio::spawn(async move {
            let got = files.downloader(&ep).download(hash, vec![from; 3]);
            if let Err(e) = got.await {
                warn!("keeping a file: {e:#}");
            }
        });
    }

    /// A device the space's owner vouched for (not banned since), or soon
    /// will: a new member's first photo may come first.
    async fn vouched(&self, sp: &Id, d: &EndpointId) -> bool {
        for _ in 0..50 {
            if self.get(sp).is_ok_and(|s| self.takes(&s, d)) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        false
    }

    /// Members' small files a storage node takes for a space: 25 MB each,
    /// 4 GB a day, so no member fills its disk.
    fn budget(&self, sp: &Id, size: u64) -> Result<(), &'static str> {
        if size > crate::crypto::sealed_len(PREVIEW as u64) {
            return Err("over 25 MB: the sender's own storage node keeps it");
        }
        let (mut b, day) = (lock(&self.budget), now() / DAY);
        let used = b.entry(*sp).or_insert((day, 0));
        if used.0 != day {
            *used = (day, 0);
        }
        if used.1 + size > 4 << 30 {
            return Err("this space's files for today are kept: try tomorrow");
        }
        used.1 += size;
        Ok(())
    }

    async fn give(&self, c: &Connection) -> Result<()> {
        let ((mut tx, mut rx), from) = (c.accept_bi().await?, c.remote_id());
        // A file's worth is read only after the head (and token), by storage.
        let mut head = [0u8; 17];
        rx.read_exact(&mut head).await?;
        let max = match (head[0], self.serve) {
            (1 | 2, Some(t)) if same(&head[1..], &t) => PREVIEW + 16,
            (1 | 2, _) => bail!("not a storage node of yours"),
            (3, Some(_)) => 128,
            _ => 64,
        };
        let rest = sync::rest(&mut rx, max).await?;
        match postcard::from_bytes(&[&head[..], &rest].concat())? {
            Ask::Get(h) if self.may_get(&from, &h) => {
                tx.write_all(&std::fs::read(self.blob(&h)).unwrap_or_default())
                    .await?
            }
            Ask::Get(_) => {}
            Ask::Put(t, b) if self.serve.is_some_and(|s| same(&s, &t)) => {
                std::fs::create_dir_all(self.dir.join("blobs"))?;
                store::write(&self.blob(&Id(blake3::hash(&b).into())), &b)?;
            }
            Ask::Keep(t, h, pw) if self.serve.is_some_and(|s| same(&s, &t)) => {
                let size = self.verified(&[from], &h).await;
                let size = size.context("not reachable, or not there")?;
                let same = |a: &str, b: &str| {
                    blake3::hash(a.as_bytes()) == blake3::hash(b.as_bytes())
                };
                let open = self.big.as_deref().zip(pw.as_deref());
                if size > crate::crypto::sealed_len(limit())
                    && !open.is_some_and(|(a, b)| same(a, b))
                {
                    let no = "over 5 GB: storage nodes keep it only with their password";
                    tx.write_all(no.as_bytes()).await?;
                } else {
                    self.fetch(&h, from);
                }
            }
            Ask::Hold(sp, t, h)
                if self
                    .serve
                    .is_some_and(|s| same(&hold_token(&s, &sp), &t)) =>
            {
                ensure!(self.vouched(&sp, &from).await, "not vouched for");
                let hash = iroh_blobs::Hash::from_bytes(h.0);
                if !self.files.has(hash).await? {
                    let size = self.verified(&[from], &h).await;
                    let size = size.context("not reachable, or not there")?;
                    match self.budget(&sp, size) {
                        Ok(()) => self.fetch(&h, from),
                        Err(no) => tx.write_all(no.as_bytes()).await?,
                    }
                }
            }
            Ask::Put(..) | Ask::Keep(..) => {
                bail!("not a storage node of yours")
            }
            Ask::Hold(..) => bail!("not a space this storage node keeps"),
        }
        tx.finish()?;
        let _ = tokio::time::timeout(Duration::from_secs(10), c.closed()).await;
        Ok(())
    }

    /// What we tell `to` when we ask: our room, and who we are, signed.
    pub(crate) fn hello(
        &self,
        sp: &Space,
        to: &EndpointId,
        root: [u8; 32],
        token: Option<[u8; 16]>,
    ) -> Hello {
        let room = self.room(sp);
        let me = &self.me.user;
        let who = sp.inv.as_ref().map(|_| sync::sign(me, &sp.id, &room, to));
        let (devices, tags) = match token {
            Some(_) => {
                let t =
                    lock(&sp.book).keys.keys().copied().take(4096).collect();
                (sp.devices(), t)
            }
            None => (vec![], vec![]),
        };
        Hello { space: sp.id, root, room, who, token, devices, tags }
    }

    /// Whom to answer: a member, who holds a key no later epoch cut them
    /// off from; a storage node, only devices its owners vouched for. An
    /// owner (its token) has it keep a new space.
    pub(crate) fn find(
        self: &Arc<Self>,
        h: &Hello,
        from: EndpointId,
    ) -> (Option<Arc<Space>>, Admit) {
        let who = sync::who(h, &self.ep.id());
        if let Some(u) = who {
            self.bind(from, u);
        }
        let sp = self.get(&h.space).ok();
        let owner = h.token.zip(self.serve).is_some_and(|(a, b)| same(&a, &b));
        if owner {
            self.vouch(h, from, who);
            let sp =
                sp.or_else(|| self.open(None, h.space, vec![from.into()]).ok());
            return (sp, Admit::Yes);
        }
        let Some(sp) = sp else { return (None, Admit::No) };
        let a = self.admits(&sp, h, from, who);
        (Some(sp), a)
    }

    fn admits(
        &self,
        sp: &Space,
        h: &Hello,
        d: EndpointId,
        who: Option<Id>,
    ) -> Admit {
        let yes = |ok: bool| if ok { Admit::Yes } else { Admit::No };
        // A storage node: a vouched device, an owner's (signed), or one
        // holding the key its owners write with now (a newcomer).
        if sp.inv.is_none() {
            let hs = lock(&self.hosts);
            let host = hs.get(&sp.id);
            let owner = who.is_some_and(|u| {
                host.is_some_and(|h| h.users.contains_key(&u))
            });
            let now =
                host.is_some_and(|x| x.room != [0; 32] && x.room == h.room);
            drop(hs);
            return yes(owner || now || self.takes(sp, &d));
        }
        if self.mine(&d) {
            return Admit::Yes;
        }
        let b = lock(&sp.book);
        if b.st.hosts().iter().any(|a| a.id == d) {
            return Admit::Yes;
        }
        let Some(u) = who.filter(|u| !b.st.out(u)) else { return Admit::No };
        let end = (u64::MAX, Id([255; 32]));
        let k = b.keys.iter().find(|(_, k)| sync::room(&sp.id, k) == h.room);
        match k {
            Some((t, _)) if b.st.stale(t, &end, &u) => Admit::Out,
            Some(_) => Admit::Yes,
            None => Admit::No,
        }
    }

    /// A member told us our key was replaced without us (or no longer).
    pub(crate) fn cut(&self, sp: &Space, out: bool) {
        let was = std::mem::replace(&mut lock(&sp.book).out, out);
        if was != out {
            self.emit(Event::Changed { space: sp.id });
        }
    }

    fn bind(&self, d: EndpointId, u: Id) {
        let mut v = lock(&self.verified);
        if v.len() >= 4096 {
            v.clear();
        }
        v.insert(d, u);
    }

    /// An owner's devices, room and epochs for a space this node keeps.
    fn vouch(&self, h: &Hello, from: EndpointId, who: Option<Id>) {
        let user = who.unwrap_or(Id(*from.as_bytes()));
        let ds = std::iter::once(from).chain(h.devices.iter().copied());
        let ds: Vec<_> = ds.take(4096).collect();
        let mut hs = lock(&self.hosts);
        let e = hs.entry(h.space).or_default();
        let was = (e.room, e.tags.len(), e.users.get(&user) == Some(&ds));
        e.users.insert(user, ds);
        if h.room != [0; 32] {
            e.room = h.room;
        }
        e.tags.extend(h.tags.iter().take(4096));
        while e.tags.len() > 4096 {
            e.tags.pop_first();
        }
        let moved = was.0 != e.room;
        let changed = was != (e.room, e.tags.len(), true);
        drop(hs);
        if changed {
            self.save_hosts();
        }
        if moved && let Ok(sp) = self.get(&h.space) {
            self.rehome(&sp);
        }
    }

    fn save_hosts(&self) {
        if let Err(e) =
            store::save(&self.dir.join("hosts"), &*lock(&self.hosts))
        {
            warn!("saving hosts: {e}");
        }
    }

    /// Every device a storage node's owners vouched for in a space.
    fn owners(&self, id: &Id) -> Vec<EndpointId> {
        let h = lock(&self.hosts);
        let all =
            h.get(id).into_iter().flat_map(|h| h.users.values().flatten());
        let mut v: Vec<_> = all.copied().collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Members check every op; a storage node cannot: vouched devices only.
    pub(crate) fn takes(&self, sp: &Space, peer: &EndpointId) -> bool {
        let h = lock(&self.hosts);
        let ours = |h: &Host| h.users.values().any(|v| v.contains(peer));
        sp.inv.is_some() || h.get(&sp.id).is_some_and(ours)
    }

    /// Where its members meet now: a member's current key, or what a
    /// storage node's owners last said.
    pub(crate) fn room(&self, sp: &Space) -> [u8; 32] {
        match &sp.inv {
            Some(inv) => sync::room(&sp.id, &lock(&sp.book).key(inv).0),
            None => lock(&self.hosts).get(&sp.id).map_or([0; 32], |h| h.room),
        }
    }

    /// Moved to a new room: true, and the gossip follows.
    fn rehome(&self, sp: &Space) -> bool {
        let r = self.room(sp);
        let moved = std::mem::replace(&mut *lock(&sp.room), r) != r;
        if moved {
            sp.moved.notify_one();
        }
        moved
    }

    /// A sync with `peer` went through: what we wrote reached someone.
    pub(crate) fn reached(&self, sp: &Space, peer: EndpointId) {
        lock(&self.synced).insert(peer, now());
        if !std::mem::take(&mut *lock(&sp.undelivered)).is_empty() {
            self.emit(Event::Changed { space: sp.id });
        }
    }

    /// Ours, not yet on any other device.
    pub fn undelivered(&self, id: &Id) -> HashSet<Id> {
        self.get(id).map(|s| lock(&s.undelivered).clone()).unwrap_or_default()
    }

    /// May `d` have blob `h`? Sealed ones: any member's proven device;
    /// offered ones: only members of the space they went to.
    fn may_get(&self, d: &EndpointId, h: &Id) -> bool {
        if self.serve.is_some() || self.mine(d) || self.hosting(d) {
            return true;
        }
        let Some(u) = lock(&self.verified).get(d).copied() else {
            return false;
        };
        let member = |id: &Id| {
            self.view(id, |s| s.profile(&u).is_some() && !s.out(&u))
                == Some(true)
        };
        let shared = lock(&self.shared).get(h).map(|s| s.space);
        match shared {
            Some(sp) => sp.is_some_and(|sp| member(&sp)),
            None => self.spaces().iter().any(member),
        }
    }

    async fn sync_with(self: Arc<Self>, sp: Arc<Space>, peer: EndpointAddr) {
        let id = peer.id;
        if !self.takes(&sp, &id) || lock(&sp.busy).insert(id, true).is_some() {
            return;
        }
        let again = || {
            let mut b = lock(&sp.busy);
            let again = b.insert(id, false) == Some(true);
            if !again {
                b.remove(&id);
            }
            again
        };
        while again() {
            let stores = self.stores(&sp.id);
            let token = stores.into_iter().find(|s| s.0.id == id).map(|s| s.1);
            let r = async {
                let c = self.ep.connect(peer.clone(), sync::ALPN).await?;
                let n = sync::initiate(&self, &sp, &c, token).await;
                c.close(0u32.into(), b"done");
                n
            };
            let r = tokio::time::timeout(sync::LIMIT, r).await;
            match r.unwrap_or_else(|e| Err(e.into())) {
                Ok(count) => {
                    self.reached(&sp, id);
                    if count > 0 {
                        self.emit(Event::Synced { space: sp.id, count });
                    }
                }
                Err(e) => debug!("sync with {}: {e:#}", id.fmt_short()),
            }
        }
    }

    fn neighbor(self: &Arc<Self>, sp: &Arc<Space>, peer: EndpointId) {
        if !lock(&sp.nbrs).insert(peer) {
            return;
        }
        self.emit(Event::Changed { space: sp.id });
        let (n, sp) = (self.clone(), sp.clone());
        tokio::spawn(async move {
            // Twice: ops broadcast while we were joining reach neither side.
            n.clone().sync_with(sp.clone(), peer.into()).await;
            n.signal(&sp, Op::Here).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            n.clone().sync_with(sp.clone(), peer.into()).await;
            let Some(info) = n.ep.remote_info(peer).await else { return };
            let a = EndpointAddr::from_parts(
                peer,
                info.into_addrs().map(|a| a.into_addr()),
            );
            // Members and storage nodes only, the latest 512: anyone may join.
            let host =
                n.view(&sp.id, |s| s.hosts().iter().any(|h| h.id == peer));
            let ok = n.member_device(&peer) || host == Some(true);
            let keep = !a.is_empty() && ok;
            let mut l = lock(&n.local);
            if keep {
                if l.peers.len() >= 512 && !l.peers.contains_key(&peer) {
                    l.peers.pop_first();
                }
                l.peers.insert(peer, a);
            }
            drop(l);
            n.persist();
        });
    }

    fn signaled(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        raw: &[u8],
        from: EndpointId,
    ) {
        let Ok(e) = postcard::from_bytes::<Env>(raw) else { return };
        if sp.inv.is_none() {
            // A storage node can't check gossip: it fetches from who passed it.
            let i = Id(blake3::hash(raw).into());
            if !e.live && !lock(&sp.book).has(&i) {
                self.sync(sp, from);
            }
            return;
        }
        if !e.live {
            self.ingest(sp, vec![raw.to_vec()], false);
            return;
        }
        let s = {
            let b = lock(&sp.book);
            let s = b.keys.get(&e.epoch);
            let s = s.and_then(|k| op::open(&sp.id, k, &e, None)).map(|s| s.0);
            // Presence, typing and calls only from who holds the key now,
            // unbanned (a ban's rekey may wait for an admin).
            s.filter(|s| {
                !b.st.out(&s.author)
                    && !b.st.stale(&s.epoch, &(s.ts, s.id), &s.author)
            })
        };
        let Some(s) = s else { return };
        // Recent, and once: an old ring passed on again rings nothing.
        let mut heard = lock(&sp.heard);
        heard.retain(|_, t| t.elapsed() < Duration::from_secs(240));
        if s.ts.abs_diff(now()) > 120_000
            || heard.insert(s.id, Instant::now()).is_some()
        {
            return;
        }
        drop(heard);
        let gone = s.op == Op::Gone;
        if gone && lock(&sp.seen).remove(&s.author).is_some()
            || !gone && sp.saw(s.author)
        {
            self.emit(Event::Presence { space: sp.id });
        }
        match s.op {
            Op::Typing { channel } => {
                let user = s.author;
                self.emit(Event::Typing { space: sp.id, channel, user });
            }
            // Back from sleep: tell it of our call now, or it misses the ring.
            Op::Here => {
                if let Some((_, ch)) = self.in_call().filter(|c| c.0 == sp.id) {
                    let (n, sp) = (self.clone(), sp.clone());
                    let call = Op::Call { channel: Some(ch) };
                    tokio::spawn(async move { n.signal(&sp, call).await });
                }
            }
            Op::Call { channel } => {
                // Aged as said: a ring retried late is not still ringing.
                let age = Duration::from_millis(now().saturating_sub(s.ts));
                let at = Instant::now().checked_sub(age);
                let mut c = lock(&sp.calls);
                let was = match channel {
                    Some(ch) => {
                        let at = at.unwrap_or_else(Instant::now);
                        c.insert(s.author, (ch, s.device, at))
                    }
                    None => c.remove(&s.author),
                };
                drop(c);
                if was.map(|w| w.0) != channel {
                    self.emit(Event::Call { space: sp.id });
                }
            }
            _ => {}
        }
    }

    /// Woken, in front, or on a new network: re-probe, re-dial, catch up now.
    pub async fn resume(self: &Arc<Self>) {
        self.ep.network_change().await;
        for sp in self.all() {
            let peers = self.bootstrap(&sp);
            peers.iter().take(3).for_each(|p| self.sync(&sp, *p));
            if let Some(tx) = sp.tx() {
                let _ = tx.join_peers(peers).await;
            }
            self.signal(&sp, Op::Here).await;
        }
    }

    pub async fn shutdown(&self) {
        let call = lock(&self.call).take();
        if let Some(sp) = call.and_then(|c| self.get(&c.space).ok()) {
            self.signal(&sp, Op::Call { channel: None }).await;
        }
        for sp in self.all() {
            self.signal(&sp, Op::Gone).await;
        }
        self.spaces.write().unwrap_or_else(|e| e.into_inner()).clear();
        if let Some(r) = self.router.get() {
            let _ = r.shutdown().await;
        }
        lock(&self.held).take();
    }
}

/// One gossip subscription per space, renewed if it ever closes.
/// The topic is the room: a rekey moves it, leaving whoever it cut off.
async fn pump(n: Arc<Node>, sp: Weak<Space>, boot: Vec<EndpointId>) {
    loop {
        let Some(s) = sp.upgrade() else { return };
        let (room, moved) = (n.room(&s), s.moved.clone());
        *lock(&s.room) = room;
        // A storage node not yet told where: wait to be.
        if room == [0; 32] {
            drop(s);
            moved.notified().await;
            continue;
        }
        let peers = boot.iter().copied().chain(n.bootstrap(&s)).collect();
        let topic = n.gossip.subscribe(TopicId::from_bytes(room), peers).await;
        let Ok(topic) = topic else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        let (tx, mut rx) = topic.split();
        *lock(&s.tx) = Some(tx.clone());
        drop(s);
        // Alone (a dial failed, say): dial again soon, backing off to a minute.
        let mut wait = Duration::from_secs(2);
        loop {
            let next = tokio::select! {
                () = moved.notified() => break,
                r = tokio::time::timeout(wait, rx.next()) => r,
            };
            let ev = match next {
                Ok(Some(ev)) => ev,
                Ok(None) => break,
                Err(_) => {
                    let Some(s) = sp.upgrade() else { return };
                    if s.nbrs().is_empty() {
                        let _ = tx.join_peers(n.bootstrap(&s)).await;
                    }
                    wait = (wait * 2).min(BEAT);
                    continue;
                }
            };
            let Some(s) = sp.upgrade() else { return };
            match ev {
                Ok(Gossiped::Received(m)) => {
                    n.neighbor(&s, m.delivered_from);
                    n.signaled(&s, &m.content, m.delivered_from);
                }
                Ok(Gossiped::NeighborUp(p)) => n.neighbor(&s, p),
                Ok(Gossiped::NeighborDown(p)) => {
                    let mut nb = lock(&s.nbrs);
                    nb.remove(&p);
                    if nb.is_empty() {
                        wait = Duration::from_secs(2);
                    }
                    drop(nb);
                    n.emit(Event::Changed { space: s.id });
                }
                Ok(Gossiped::Lagged) | Err(_) => break,
            }
        }
        if let Some(s) = sp.upgrade() {
            lock(&s.nbrs).clear();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// TLS as iroh sets it up, roots compiled in.
pub fn http() -> reqwest::Client {
    client().build().expect("HTTP client")
}

fn client() -> reqwest::ClientBuilder {
    #[cfg(not(target_os = "android"))]
    let ca = iroh::tls::CaTlsConfig::system();
    #[cfg(target_os = "android")]
    let ca = iroh::tls::CaTlsConfig::embedded();
    let tls = ca.client_config(iroh::tls::default_provider()).expect("TLS");
    let b = reqwest::Client::builder().tls_backend_preconfigured(tls);
    // Stalls end a request, not its length: an update can be large.
    let b = b.connect_timeout(Duration::from_secs(20));
    b.read_timeout(Duration::from_secs(30))
}

/// Push URLs, which members name: public https only, never redirected.
fn push_http() -> reqwest::Client {
    let b = client().redirect(reqwest::redirect::Policy::none());
    let b = b.https_only(true).dns_resolver(Public);
    b.build().expect("HTTP client")
}

/// https to a public address (members name it), tests' local, or iroh.
fn pushable(u: &str) -> bool {
    let ip = || {
        let u = reqwest::Url::parse(u).ok()?;
        u.host_str()?.trim_matches(['[', ']']).parse().ok()
    };
    u.starts_with("https://") && ip().is_none_or(|a| public(&a))
        || u.starts_with("irch:ring:")
        || cfg!(debug_assertions) && u.starts_with("http://127.0.0.1:")
}

struct Public;

impl reqwest::dns::Resolve for Public {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let all = tokio::net::lookup_host((host, 0)).await?;
            let v: Vec<_> = all.filter(|a| public(&a.ip())).collect();
            if v.is_empty() {
                return Err("no public address".into());
            }
            Ok(Box::new(v.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Presence; re-dial if alone; reconcile; a late tick means we slept.
async fn heartbeat(n: Weak<Node>) {
    let mut t = tokio::time::interval(BEAT);
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last = now();
    for i in 1u64.. {
        t.tick().await;
        let Some(n) = n.upgrade() else { return };
        if now().saturating_sub(last) > 2 * BEAT.as_millis() as u64 {
            n.resume().await;
        }
        last = now();
        for sp in n.all() {
            let nb = sp.nbrs();
            match (nb.get(i as usize % nb.len().max(1)), sp.tx()) {
                (None, Some(tx)) => drop(tx.join_peers(n.bootstrap(&sp)).await),
                (Some(p), _) if i % 2 == 0 => n.sync(&sp, *p),
                _ => {}
            }
            n.signal(&sp, Op::Here).await;
            n.emit(Event::Presence { space: sp.id });
            // Pairings from before `Hold`, and admins since made.
            if i % 10 == 1 {
                for (addr, _) in n.stores(&sp.id) {
                    n.list(&sp, &addr).await;
                }
            }
        }
        // What storage took today, kept across restarts.
        if n.serve.is_some() {
            let b = lock(&n.budget);
            let _ = store::save(&n.dir.join("budget"), &*b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book(name: &str, owner: Id) -> (Book, PathBuf) {
        let p = std::env::temp_dir().join(format!("irch-{name}-{}", now()));
        let st = State::new(&Invite::new(owner, "", None));
        (Book::new(Log::open(&p).unwrap().0, st), p)
    }

    #[test]
    fn what_no_key_opens_stays_off_disk_and_bounded() {
        let (mut b, p) = book("park", Id([1; 32]));
        let me = Me::new(rand());
        for n in 0..40 {
            let e =
                Env { epoch: [n; 8], ts: 0, live: false, ct: vec![n; 1 << 20] };
            let raw = postcard::to_allocvec(&e).unwrap();
            b.fold(
                &Id([2; 32]),
                &me,
                vec![(Id([n; 32]), e, Src::Net(raw, false))],
            );
        }
        assert!(b.parked <= PARKED + (2 << 20) && b.at.is_empty());
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn pushes_go_out_only_to_public_https() {
        assert!(pushable("https://ntfy.sh/up1") && pushable("irch:ring:x"));
        for u in [
            "http://ntfy.sh/up1",
            "https://10.0.0.8/up",
            "https://[::1]:8080/up",
            "https://[fd00::1]/up",
            "https://100.64.0.1/up",
            "https://169.254.169.254/latest",
            "https://[::ffff:10.0.0.8]/up",
            "https://[::ffff:127.0.0.1]/up",
            "https://[64:ff9b::a00:8]/up",
            "https://224.0.0.1/up",
            "https://0.1.2.3/up",
            "file:///etc/passwd",
        ] {
            assert!(!pushable(u), "{u}");
        }
    }

    #[test]
    fn only_admins_hand_on_keys() {
        let (o, x) = (Me::new(rand()), Me::new(rand()));
        let inv = Invite::new(o.id(), "", None);
        let (sp, k0, dev) = (inv.id(), inv.key, SecretKey::generate().public());
        let p = std::env::temp_dir().join(format!("irch-keys-{}", now()));
        let mut b = Book::new(Log::open(&p).unwrap().0, State::new(&inv));
        b.keys.insert(tag(&k0), k0);
        let (kb, kx): (Key, Key) = (rand(), rand());
        let (ch, n) = (Ch([3; 8]), |t: &str| t.to_string());
        let post = |t| Op::Post {
            channel: ch,
            text: n(t),
            reply: None,
            files: vec![],
        };
        let boxed = kem_seal(&o.pk(), &rekey_aad(&sp, &tag(&kx)), &kx).unwrap();
        let ops = [
            (&o, k0, 1, Op::Channel { id: ch, name: Some(n("g")) }),
            // Someone holding the key, not an admin, mints one of their own.
            (&x, k0, 2, Op::Back { key: kx }),
            (
                &x,
                k0,
                3,
                Op::Rekey { epoch: tag(&kx), boxes: vec![(o.id(), boxed)] },
            ),
            (&x, kx, 4, post("x, under its own key")),
            (&o, k0, 5, Op::Back { key: kb }),
            (&o, kb, 6, post("o, under a key handed on")),
        ];
        let work = ops.into_iter().map(|(who, k, ts, op)| {
            let (s, raw) = op::seal(&sp, &k, &who.user, dev, ts, op);
            (s.id, postcard::from_bytes(&raw).unwrap(), Src::Log(0))
        });
        b.fold(&sp, &o, work.rev().collect());
        assert!(
            b.keys.contains_key(&tag(&kb)) && !b.keys.contains_key(&tag(&kx))
        );
        // Nor from an invite pasted in: only an epoch an admin set.
        assert!(!b.learn(&sp, &o, kx) && !b.keys.contains_key(&tag(&kx)));
        let texts: Vec<_> =
            b.st.messages(&ch, None, 9).into_iter().map(|m| m.text).collect();
        assert_eq!(texts, ["o, under a key handed on"]);
        std::fs::remove_file(p).unwrap();
    }

    #[test]
    fn keys_a_rekey_race_cut_off_are_handed_on() {
        let o = Id([1; 32]);
        let (mut b, p) = book("race", o);
        let k: [Key; 3] = [rand(), rand(), rand()];
        let device = SecretKey::from_bytes(&[1; 32]).public();
        b.st.extend((0..3).map(|n| Signed {
            id: Id([n as u8; 32]),
            author: o,
            device,
            ts: n as u64,
            op: Op::Rekey { epoch: tag(&k[n]), boxes: vec![] },
            epoch: [0; 8],
        }));
        b.keys.extend(k.map(|k| (tag(&k), k)));
        // Both rekeys replaced the first key; the second lost the race.
        b.backs.insert(tag(&k[1]), vec![tag(&k[0])]);
        b.backs.insert(tag(&k[2]), vec![tag(&k[0])]);
        assert_eq!(b.orphans(), [k[1]]);
        b.backs.get_mut(&tag(&k[2])).unwrap().push(tag(&k[1]));
        assert!(b.orphans().is_empty());
        std::fs::remove_file(p).unwrap();
    }
}
