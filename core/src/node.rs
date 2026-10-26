//! One iroh endpoint carries gossip (live) and sync (catch-up) for every
//! space. Every op, local or remote, takes the same path: open with a
//! known epoch key, check, append to the space's log, fold, notify.

use crate::call::{self, Call};
use crate::crypto::{Key, Me, kem_open, kem_seal, rand};
use crate::op::{self, Env, File, Invite, Op, Role, Signed, code, tag, uncode};
use crate::state::{Message, State};
use crate::store::{self, Log};
use crate::sync::{self, Bucket, Hello};
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::{debug, warn};

const DAY: u64 = 86_400_000;
const GOSSIP_MAX: usize = 128 << 10;
const BEAT: Duration = Duration::from_secs(60);
const ONLINE: Duration = Duration::from_secs(150);
// Someone in a call says so every 15 s.
const CALLER: Duration = Duration::from_secs(45);
const RELAYS: [&str; 2] =
    ["https://iroh.cxn.center./", "https://use1-1.relay.n0.iroh.link./"];
pub const MAX_TEXT: usize = 4000;
pub const MAX_NAME: usize = 64;
pub const MAX_FILE: usize = 25 << 20;
const BLOB: &[u8] = b"irch/blob/1";
const PARKED: usize = 32 << 20;
const LOCKED: &str = "keys changed without you: ask for a new invite";

/// How to reach peers beyond the LAN.
#[derive(Clone, Debug, Default)]
pub enum Net {
    /// The lab's relay, then n0's US East one (as irp and tydig); n0's
    /// DNS, plus the Mainline DHT.
    #[default]
    Default,
    /// Your own relays, plus the Mainline DHT; nothing of n0's.
    Relay(Vec<RelayUrl>),
    /// This network only, found by mDNS.
    Lan,
    /// Only addresses from codes and the address book (tests).
    Direct,
}

pub struct Config {
    pub dir: PathBuf,
    pub name: String,
    pub net: Net,
    /// Become another device of the person behind this link code.
    pub link: Option<String>,
    /// Keep other people's ciphertext: a storage node.
    pub serve: bool,
    /// Where "Add to notes" writes: a Markdown vault (loroh's, say).
    pub notes: Option<PathBuf>,
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

/// This device's own state, never shared.
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

/// Notifications, a separate file from `Local` (postcard is positional:
/// a field added there would reset it).
#[derive(Serialize, Deserialize, Default)]
struct Prefs {
    level: BTreeMap<Id, Level>,
    // Per space, everything up to here was notified (or seen) already.
    told: BTreeMap<Id, u64>,
    // This device's UnifiedPush endpoint.
    push: Option<String>,
}

/// What a space notifies about.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub enum Level {
    #[default]
    All,
    Mentions,
    Off,
}

/// A notification to show: one per channel or thread, its newest.
#[derive(Serialize, Debug, Clone)]
pub struct Notice {
    pub space: Id,
    pub room: Ch,
    pub title: String,
    pub body: String,
    /// A call in a DM: ring, like a phone.
    pub ring: bool,
}

pub struct Space {
    pub id: Id,
    inv: Option<Invite>,
    book: Mutex<Book>,
    tx: Mutex<Option<GossipSender>>,
    nbrs: Mutex<HashSet<EndpointId>>,
    seen: Mutex<HashMap<Id, Instant>>,
    // Who is in which channel's call, on which device, last heard when.
    calls: Mutex<HashMap<Id, (Ch, EndpointId, Instant)>>,
    busy: Mutex<HashSet<EndpointId>>,
    task: OnceLock<AbortOnDropHandle<()>>,
}

struct Book {
    log: Log,
    idx: BTreeMap<u32, BTreeSet<Id>>,
    at: HashMap<Id, (u64, u32)>,
    keys: HashMap<[u8; 8], Key>,
    // What no key we hold opens yet, in memory only: past `PARKED` bytes
    // from the network it is dropped, and sync brings it again.
    pending: HashMap<Id, (Env, Src)>,
    parked: usize,
    // Per epoch, the older keys its holders learn through `Back`.
    backs: HashMap<[u8; 8], Vec<[u8; 8]>>,
    st: State,
    knocks: Vec<Invite>,
}

/// Where an envelope is: in the log (checked once, or not yet), or only
/// in memory until it opens and checks out (ours need no check).
enum Src {
    Log(bool),
    Net(Vec<u8>, bool),
}

fn named(n: &str) -> Result<&str> {
    let n = n.trim();
    ensure!(!n.is_empty() && n.len() <= MAX_NAME, "names are 1 to 64 bytes");
    Ok(n)
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
        Self { log, idx, at, keys, pending, parked: 0, backs, st, knocks }
    }

    fn has(&self, i: &Id) -> bool {
        self.at.contains_key(i) || self.pending.contains_key(i)
    }

    fn index(&mut self, i: Id, ts: u64, at: u64, len: usize) {
        self.at.insert(i, (at, len as u32));
        self.idx.entry((ts / DAY) as u32).or_default().insert(i);
    }

    /// Append to the log (flushed to disk, for ours), and to the day
    /// index that sync compares.
    fn store(
        &mut self,
        i: Id,
        ts: u64,
        raw: &[u8],
        checked: bool,
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

    /// Open what known keys open, keeping what is new once it checks out;
    /// keys learned on the way open more. Returns how many were kept.
    fn fold(
        &mut self,
        sp: &Id,
        me: &Me,
        mut work: Vec<(Id, Env, Src)>,
    ) -> usize {
        let (mut ops, mut kept) = (vec![], 0);
        while let Some((i, e, src)) = work.pop() {
            let Some(&key) = self.keys.get(&e.epoch) else {
                self.park(i, e, src);
                continue;
            };
            let trusted = matches!(src, Src::Log(true) | Src::Net(_, true));
            let Some(s) = op::open(sp, &key, &e, !trusted) else { continue };
            if let Src::Net(raw, ours) = &src {
                if !self.store(i, e.ts, raw, true, *ours) {
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
                    .filter(|k| tag(k) == *epoch),
                Op::Back { key } => {
                    self.backs.entry(e.epoch).or_default().push(tag(key));
                    Some(*key)
                }
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
                    let inv = inv
                        .and_then(|b| postcard::from_bytes::<Invite>(&b).ok());
                    self.knocks.extend(inv.filter(|i| {
                        i.dm == Some(me.id()) && i.owner == s.author
                    }));
                    None
                }
                _ => None,
            };
            if let Some(k) = key
                && self.keys.insert(tag(&k), k).is_none()
            {
                work.extend(self.unpark(&tag(&k)));
            }
            ops.push(s);
        }
        self.st.extend(ops);
        kept
    }

    fn park(&mut self, i: Id, e: Env, src: Src) {
        if let Src::Net(raw, _) = &src {
            if self.parked > PARKED {
                self.pending.retain(|_, (_, s)| matches!(s, Src::Log(_)));
                self.parked = 0;
            }
            self.parked += raw.len();
        }
        self.pending.insert(i, (e, src));
    }

    fn unpark(&mut self, t: &[u8; 8]) -> Vec<(Id, Env, Src)> {
        let out = self.pending.extract_if(|_, (e, _)| e.epoch == *t);
        let v: Vec<_> = out.map(|(i, (e, s))| (i, e, s)).collect();
        for (_, _, s) in &v {
            if let Src::Net(raw, _) = s {
                self.parked -= raw.len();
            }
        }
        v
    }

    /// A key from outside the log (a fresh invite): open what waited.
    fn learn(&mut self, sp: &Id, me: &Me, k: Key) -> bool {
        let new = self.keys.insert(tag(&k), k).is_none();
        if new {
            let w = self.unpark(&tag(&k));
            self.fold(sp, me, w);
        }
        new
    }

    /// The key to seal with, and whether a newer epoch left us out (then
    /// the newest we hold). Never an epoch older than our invite's: until
    /// that one shows up, the invite's key.
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

    /// Keys of valid epochs that holders of the current one cannot reach
    /// through `Back`: a lost rekey race, a demoted admin's epoch.
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
    // Endpoints we woke: when last, and whether one more is held back.
    wake: Mutex<HashMap<String, (Option<Instant>, bool)>>,
    // Calls notified about (space, caller, channel), while they last.
    rang: Mutex<HashSet<(Id, Id, Ch)>>,
    http: OnceLock<reqwest::Client>,
    name: Mutex<String>,
    events: broadcast::Sender<Event>,
    clock: AtomicU64,
    invisible: AtomicBool,
    serve: Option<[u8; 16]>,
    // A storage node: the phones holding a wake connection here.
    phones: crate::wake::Hub,
    // Files offered from here, and those fetched (served on to members).
    files: Option<FsStore>,
    call: Mutex<Option<Call>>,
    media: call::Media,
}

/// Public addresses first: a phone tries them in turn.
fn wake_order(
    mut ips: Vec<std::net::IpAddr>,
) -> impl Iterator<Item = std::net::IpAddr> {
    let private = |ip: &std::net::IpAddr| match ip {
        std::net::IpAddr::V4(a) => {
            a.is_private() || a.is_loopback() || a.is_link_local()
        }
        std::net::IpAddr::V6(a) => {
            a.is_loopback()
                || (a.segments()[0] & 0xfe00) == 0xfc00
                || (a.segments()[0] & 0xffc0) == 0xfe80
        }
    };
    ips.sort_by_key(|ip| private(ip));
    ips.dedup();
    ips.into_iter()
}

/// Serves sync and blobs; the ALPN says which.
#[derive(Debug, Clone)]
struct Proto(Weak<Node>);

impl ProtocolHandler for Proto {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        let gone = || AcceptError::from_err(std::io::Error::other("closing"));
        let n = self.0.upgrade().ok_or_else(gone)?;
        let r = match c.alpn() {
            sync::ALPN => sync::respond(&n, &c).await,
            crate::wake::ALPN => crate::wake::answer(&n.phones, c).await,
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

/// A blob by hash, or (to a storage node) one to keep.
#[derive(Serialize, Deserialize)]
enum Ask {
    Get(Id),
    Put([u8; 16], Vec<u8>),
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

/// Offered files go to the devices of people in a space with us, no one
/// else.
fn gate(n: std::sync::Weak<Node>) -> iroh_blobs::provider::events::EventSender {
    use iroh_blobs::provider::events::*;
    let mask =
        EventMask { connected: ConnectMode::Intercept, ..EventMask::DEFAULT };
    let (tx, mut rx) = EventSender::channel(32, mask);
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if let ProviderMessage::ClientConnected(m) = m {
                let n = n.upgrade().zip(m.endpoint_id);
                let ok = n.is_some_and(|(n, d)| n.member_device(&d));
                let r = if ok { Ok(()) } else { Err(AbortReason::Permission) };
                let _ = m.tx.send(r).await;
            }
        }
    });
    tx
}

// Phones skip the Mainline DHT, which keeps a routing table alive:
// relays and n0's DNS find peers for them.
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
        let lookup = MemoryLookup::new();
        local.peers.values().for_each(|a| lookup.add_endpoint_info(a.clone()));
        let bind = async |port: u16| {
            let b = match &cfg.net {
                Net::Default => dht(Endpoint::builder(presets::N0).relay_mode(
                    RelayMode::custom(
                        RELAYS.map(|r| r.parse().expect("relay url")),
                    ),
                )),
                Net::Relay(urls) => dht(Endpoint::builder(presets::Minimal)
                    .relay_mode(RelayMode::custom(urls.clone()))),
                Net::Lan => Endpoint::builder(presets::Minimal)
                    .relay_mode(RelayMode::Disabled)
                    .address_lookup(
                        iroh_mdns_address_lookup::MdnsAddressLookup::builder(),
                    ),
                Net::Direct => Endpoint::builder(presets::Minimal)
                    .relay_mode(RelayMode::Disabled),
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
        let files = match serve {
            None => Some(FsStore::load(cfg.dir.join("files")).await?),
            Some(_) => None,
        };
        let gossip =
            Gossip::builder().max_message_size(GOSSIP_MAX).spawn(ep.clone());
        let own_inv = own_invite(&me, link.map(|l| l.1).unwrap_or_default());
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
            wake: Mutex::default(),
            rang: Mutex::default(),
            http: OnceLock::new(),
            name: Mutex::new(cfg.name),
            events: broadcast::channel(1024).0,
            clock: AtomicU64::new(0),
            serve,
            phones: Default::default(),
            files,
            call: Mutex::new(None),
            media: broadcast::channel(512).0,
        });
        let mut router = Router::builder(n.ep.clone());
        if let Some(f) = &n.files {
            let gate = Some(gate(Arc::downgrade(&n)));
            router =
                router.accept(iroh_blobs::ALPN, BlobsProtocol::new(f, gate));
        }
        let router = router
            .accept(iroh_gossip::ALPN, n.gossip.clone())
            .accept(sync::ALPN, Proto(Arc::downgrade(&n)))
            .accept(crate::wake::ALPN, Proto(Arc::downgrade(&n)))
            .accept(BLOB, Proto(Arc::downgrade(&n)))
            .accept(call::ALPN, Proto(Arc::downgrade(&n)))
            .spawn();
        let _ = n.router.set(router);
        n.persist();
        if serve.is_some() {
            let hosted = lock(&n.local).hosted.clone();
            for (id, peers) in hosted {
                let peers = peers.into_iter().take(16).map(EndpointAddr::new);
                n.open(None, id, peers.collect())?;
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

    /// Catch up with `peer` in the background.
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

    /// What to notify about that was not yet: per channel or thread, the
    /// newest message by someone else (or mentioning us, by the space's
    /// level), unread here. History from before a space's first ask stays
    /// quiet. The app and a push wake share the marks: never twice.
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
                out.push(Notice { space, room, title, body, ring: false });
            }
            // Calls by others: a DM's rings, a channel's says it started;
            // each once while it lasts, none while we are in one.
            let calls = self.get(&id).map(|sp| self.callers(&sp, None));
            for (_, ch, u) in calls.unwrap_or_default() {
                let fresh = u != me && lock(&self.rang).insert((id, u, ch));
                live.insert((id, u, ch));
                let tell = level != Level::Off && (dm || level == Level::All);
                if !fresh || !tell || self.in_call().is_some() {
                    continue;
                }
                // Named here, or in any space we share (a new DM may not
                // have their profile yet).
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
                out.push(Notice { space, room, title, body, ring: dm });
            }
            let p = &mut lock(&self.prefs).told;
            p.insert(id, mark.max(newest.unwrap_or(0)));
        }
        lock(&self.rang).retain(|c| live.contains(c));
        self.save_prefs();
        out
    }

    /// Whether others can wake this device (it has a push endpoint).
    pub fn pushed(&self) -> bool {
        lock(&self.prefs).push.is_some()
    }

    /// A storage node: take phones' wake connections on `listen`; they
    /// reach it at `at`, else at each address it has, with that port.
    pub async fn serve_wakes(
        self: &Arc<Self>,
        listen: std::net::SocketAddr,
        mut at: Vec<String>,
    ) -> Result<()> {
        let token = self.serve.context("only storage nodes take phones")?;
        let l = tokio::net::TcpListener::bind(listen).await?;
        if at.is_empty() {
            let port = l.local_addr()?.port();
            let ips = self.addr().await.ip_addrs().map(|a| a.ip()).collect();
            at = wake_order(ips)
                .map(|ip| (ip, port).into())
                .map(|a: std::net::SocketAddr| a.to_string())
                .collect();
        }
        self.phones.at(at);
        let n = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Ok((s, _)) = l.accept().await {
                let Some(n) = n.upgrade() else { return };
                tokio::spawn(async move {
                    let key = n.ep.secret_key().clone();
                    if let Err(e) = n.phones.phone(s, &key, &token).await {
                        debug!("wake connection: {e:#}");
                    }
                });
            }
        });
        Ok(())
    }

    /// A phone: which of its owner's servers take wake connections, kept
    /// for its background service, and its spaces told to wake it there.
    /// None reachable now: as it was.
    pub async fn wake_setup(self: &Arc<Self>) -> Vec<crate::wake::Target> {
        let paired = self.view(&self.own, |s| s.storage()).unwrap_or_default();
        let (mut targets, mut servers) = (vec![], vec![]);
        for (addr, token) in paired {
            let at = crate::wake::find(&self.ep, addr.clone()).await;
            if let Some(at) = at.ok().filter(|a| !a.is_empty()) {
                targets.push(crate::wake::Target { at, id: addr.id, token });
                servers.push(addr);
            }
        }
        if !targets.is_empty() {
            let _ = crate::wake::save(&self.dir, self.ep.id(), &targets);
            let url = crate::wake::url(self.ep.id(), &servers);
            self.set_push(Some(url)).await;
        }
        targets
    }

    /// Where others wake this device when they write (UnifiedPush), or
    /// none: told every space we are in, sealed like any op.
    pub async fn set_push(self: &Arc<Self>, url: Option<String>) {
        let old = std::mem::replace(&mut lock(&self.prefs).push, url.clone());
        self.save_prefs();
        if old != url {
            for id in self.spaces() {
                let _ = self.tell_push(&id).await;
            }
        }
    }

    async fn tell_push(self: &Arc<Self>, id: &Id) -> Result<()> {
        let Some(url) = lock(&self.prefs).push.clone() else { return Ok(()) };
        let (me, dev) = (self.id(), self.ep.id());
        let has = self.view(id, |s| s.push(&me, &dev).map(str::to_string));
        if has.flatten().as_deref() == Some(&url) {
            return Ok(());
        }
        self.post(&self.get(id)?, Op::Push { url: Some(url) }).await?;
        Ok(())
    }

    /// We wrote in `id`: wake its members' sleeping devices (a ping with
    /// nothing in it; they fetch from us), each at most every 30 s, a
    /// held-back one when that is up.
    fn wake(self: &Arc<Self>, id: &Id) {
        let dev = self.ep.id();
        let urls = self.view(id, |s| s.pushes()).unwrap_or_default();
        // https only (a member names the URL: never our LAN); tests, local.
        // Or a phone's own wake servers, asked over iroh.
        let ok = |u: &String| {
            u.starts_with("https://")
                || u.starts_with("irch:wake:")
                || cfg!(debug_assertions) && u.starts_with("http://127.0.0.1:")
        };
        for (_, url) in urls.into_iter().filter(|u| u.0 != dev && ok(&u.1)) {
            let wait = {
                let mut w = lock(&self.wake);
                let (last, held) =
                    w.entry(url.clone()).or_insert((None, false));
                let since =
                    last.map_or(Duration::MAX, |t: Instant| t.elapsed());
                // A push service is spared; our own servers are not.
                let gap = if url.starts_with("irch:wake:") { 2 } else { 30 };
                let wait = Duration::from_secs(gap).saturating_sub(since);
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
                if url.starts_with("irch:wake:") {
                    if let Err(e) = crate::wake::ring(&n.ep, &url).await {
                        warn!("wake: {e:#}");
                    }
                    return;
                }
                let http = n.http.get_or_init(http);
                let r = http.post(&url).header("TTL", "86400");
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

    /// Spaces a storage node keeps, with how many envelopes each.
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
        lock(&sp.book).key(inv).1
    }

    pub fn peers(&self, id: &Id) -> usize {
        self.get(id).map_or(0, |s| lock(&s.nbrs).len())
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

    /// Peers see the change at once: gone, or here again.
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

    /// Per channel or thread, the newest unread mention of us after `after`.
    pub fn mentions(&self, id: &Id, after: u64) -> Vec<(Ch, Message)> {
        let me = self.id();
        let chans = self.view(id, |s| s.rooms()).unwrap_or_default();
        let hit = |ch: Ch| {
            let since = self.since(id, &ch).max(after);
            Some((ch, self.view(id, |s| s.last_mention(&ch, since, &me))??))
        };
        chans.into_iter().filter_map(|(ch, _)| hit(ch)).collect()
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
            calls: Mutex::default(),
            busy: Mutex::default(),
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
        l.hosted.remove(id);
        drop(l);
        self.persist();
        self.emit(Event::Changed { space: *id });
    }

    /// Peers worth dialing for a space: hosts, recent devices, invite peers.
    fn bootstrap(&self, sp: &Space) -> Vec<EndpointId> {
        let b = lock(&sp.book);
        let hosts = b.st.hosts();
        hosts.iter().for_each(|a| self.lookup.add_endpoint_info(a.clone()));
        let mut v: Vec<_> = hosts.into_iter().map(|a| a.id).collect();
        v.extend(b.st.devices(16));
        v.extend(sp.inv.iter().flat_map(|i| i.peers.iter().map(|a| a.id)));
        let l = lock(&self.local);
        v.extend(l.hosted.get(&sp.id).into_iter().flatten().take(16));
        drop(l);
        let me = self.ep.id();
        let mut seen = HashSet::new();
        v.retain(|p| *p != me && seen.insert(*p));
        v
    }

    /// Open, check, store and fold envelopes; returns how many were new.
    pub(crate) fn ingest(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        raws: Vec<Vec<u8>>,
        mine: bool,
    ) -> usize {
        let new = {
            let mut b = lock(&sp.book);
            let mut fresh = vec![];
            for raw in raws {
                let Ok(e) = postcard::from_bytes::<Env>(&raw) else { continue };
                let i = Id(blake3::hash(&raw).into());
                if !e.live && e.ts <= now() + DAY && !b.has(&i) {
                    fresh.push((i, e, raw));
                }
            }
            match sp.inv {
                // A storage node keeps, unopened, what vouched devices send.
                None => fresh
                    .into_iter()
                    .filter(|(i, e, r)| b.store(*i, e.ts, r, false, false))
                    .count(),
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

    /// What ingesting may require: DMs to accept, spaces to open or close
    /// for our other devices, a leaking epoch to replace, keys to hand on.
    async fn effects(self: Arc<Self>, sp: Arc<Space>) {
        let (knocks, fix) = {
            let mut b = lock(&sp.book);
            let admin = b.st.role(&self.id()) == Role::Admin;
            let locked = sp.inv.as_ref().is_none_or(|i| b.key(i).1);
            let lost = !locked && !b.orphans().is_empty();
            let fix = admin && b.st.dm.is_none() && (b.st.leaks() || lost);
            (std::mem::take(&mut b.knocks), fix)
        };
        for inv in knocks {
            let known =
                self.view(&self.own, |s| s.spaces().contains_key(&inv.id()));
            if known == Some(false)
                && let Err(e) = self.enter(inv).await
            {
                warn!("accepting a DM: {e}");
            }
        }
        if sp.id == self.own {
            self.reconcile().await;
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

    /// Replace an epoch someone banned may hold; else hand on, under the
    /// current one, keys it cut off.
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

    /// Seal, store, fold and broadcast an op.
    async fn post(self: &Arc<Self>, sp: &Arc<Space>, op: Op) -> Result<Id> {
        let inv = sp.inv.as_ref().ok_or_else(|| anyhow!("only hosted here"))?;
        let (key, ts) = {
            let b = lock(&sp.book);
            let (key, locked) = b.key(inv);
            // Left out of a newer epoch, an admin can still ban and rekey.
            // Both date after what they must supersede, however far ahead.
            let fix = matches!(op, Op::Role { .. } | Op::Rekey { .. });
            ensure!(!locked || fix, LOCKED);
            let after = if fix { b.st.horizon() + 1 } else { 0 };
            (key, self.tick().max(after))
        };
        let (s, raw) = self.seal(sp, &key, ts, op);
        self.ingest(sp, vec![raw.clone()], true);
        match sp.tx().filter(|_| raw.len() <= GOSSIP_MAX) {
            Some(tx) => drop(tx.broadcast(raw.into()).await),
            None => sp.nbrs().into_iter().for_each(|p| self.sync(sp, p)),
        }
        Ok(s.id)
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
            Op::Rename { name: n }
            | Op::Channel { name: Some(n), .. }
            | Op::Topic { name: n, .. } => fits(n, MAX_NAME),
            Op::React { emoji, .. } => fits(emoji, 32),
            Op::Role { role, .. } => *role != Role::Banned,
            Op::Edit { .. } | Op::Channel { .. } => true,
            _ => false,
        };
        ensure!(ok, "that cannot be sent (empty, too long, or not allowed)");
        ensure!(*id != self.own, "not a space");
        let role = matches!(op, Op::Role { .. });
        let msg = matches!(op, Op::Post { .. });
        let r = self.post(&self.get(id)?, op).await?;
        if msg {
            self.wake(id);
        }
        // Demoting whoever set the current epoch can bring back an older
        // one: replace it now, not after a jittered while.
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

    /// A new space you own, with a `#general`.
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
        }
        Ok(sp.id)
    }

    /// Our address once we have one (a relay, or else an IP), for codes.
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

    /// An invite with the current key and a few peers to reach.
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
        Ok(())
    }

    /// Ban (and lock out with a new key) or unban a member.
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

    /// A fresh epoch key for everyone not banned. Old invites stop
    /// working; history stays readable through `Back`.
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
        self.post(&self.get(via)?, Op::Knock { sealed }).await?;
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

    /// Tell members you are typing (at most every few seconds).
    pub async fn typing(&self, id: &Id, channel: Ch) {
        if let Ok(sp) = self.get(id) {
            self.signal(&sp, Op::Typing { channel }).await;
        }
    }

    /// Live ops; invisible, only leaving and calls (joining one shows).
    async fn signal(&self, sp: &Space, op: Op) {
        let Some(inv) = &sp.inv else { return };
        let (key, locked) = lock(&sp.book).key(inv);
        let quiet = !matches!(op, Op::Call { .. } | Op::Gone);
        if locked || quiet && self.invisible() {
            return;
        }
        if let Some(tx) = sp.tx() {
            let raw = self.seal(sp, &key, self.tick(), op).1;
            let _ = tx.broadcast(raw.into()).await;
        }
    }

    /// Join a channel's call (leaving any other), or with `None` leave.
    pub async fn call(self: &Arc<Self>, to: Option<(Id, Ch)>) -> Result<()> {
        let old = lock(&self.call).take();
        if let Some(c) = old {
            let sp = self.get(&c.space);
            sp.iter().for_each(|sp| self.emit(Event::Call { space: sp.id }));
            if let Ok(sp) = sp {
                self.signal(&sp, Op::Call { channel: None }).await;
            }
        }
        let Some((id, ch)) = to else { return Ok(()) };
        let sp = self.get(&id)?;
        ensure!(!self.locked(&id), LOCKED);
        let run = u64::from_le_bytes(rand());
        *lock(&self.call) = Some(Call::new(id, ch, run));
        self.signal(&sp, Op::Call { channel: Some(ch) }).await;
        self.emit(Event::Call { space: id });
        self.wake(&id); // sleeping phones: someone is calling
        tokio::spawn(self.clone().ring(run));
        Ok(())
    }

    /// Notes in the notes folder, if there is one.
    pub fn notes(&self) -> Option<Vec<String>> {
        self.notes.as_deref().map(crate::notes::list)
    }

    /// Add Markdown to a note in the notes folder.
    pub fn note(&self, file: &str, md: &str) -> Result<()> {
        let dir = self.notes.as_deref().context("no notes folder here")?;
        crate::notes::append(dir, file, md)
    }

    /// The call we are in.
    pub fn in_call(&self) -> Option<(Id, Ch)> {
        lock(&self.call).as_ref().map(|c| (c.space, c.channel))
    }

    /// Who is in each channel's call, us included.
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

    /// One encoded frame from the page, to everyone in our call.
    pub fn send_media(&self, f: &[u8]) {
        lock(&self.call).iter_mut().for_each(|c| c.send(f));
    }

    /// Frames from everyone else in our call.
    pub fn media(&self) -> broadcast::Receiver<(Id, Vec<u8>)> {
        self.media.subscribe()
    }

    /// Who says they are in a call (`ch`'s only, if given), on which
    /// device: members not banned, heard from lately.
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

    /// While in a call: say so every 15 s, and keep one connection to each
    /// device in it (the lower id dials) and none to anyone else.
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
            if i % 15 == 0 {
                self.signal(&sp, Op::Call { channel: Some(ch) }).await;
            }
            let want = self.callers(&sp, Some(ch));
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
                    let user = self.hello(&conn).await?;
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

    /// A device dialing into our call, if someone says it is in it.
    async fn answer(&self, conn: Connection) {
        let Some((id, ch)) = self.in_call() else { return };
        let Ok(sp) = self.get(&id) else { return };
        let d = conn.remote_id();
        if self.callers(&sp, Some(ch)).iter().any(|w| w.0 == d)
            && let Some(user) = self.hello(&conn).await
        {
            self.admit(user, conn);
        }
    }

    async fn hello(&self, conn: &Connection) -> Option<Id> {
        call::hello(conn, &self.me.user, self.ep.id()).await
    }

    /// Into our call: a device whose user proved it theirs and said, from
    /// it, that they are in the call.
    fn admit(&self, user: Id, conn: Connection) {
        let Some((id, ch)) = self.in_call() else { return };
        let Ok(sp) = self.get(&id) else { return };
        if !self.callers(&sp, Some(ch)).contains(&(conn.remote_id(), ch, user))
        {
            return;
        }
        let mut c = lock(&self.call);
        if let Some(c) = c.as_mut().filter(|c| (c.space, c.channel) == (id, ch))
        {
            c.add(user, conn, self.media.clone());
        }
    }

    /// Everything another device needs to become you. Keep it secret.
    pub async fn link_code(&self) -> String {
        let mut peers = vec![self.addr().await];
        peers.extend(self.view(&self.own, |s| s.hosts()).unwrap_or_default());
        code("link", &(self.me.seed, peers))
    }

    /// What a storage node hands to the people it stores for.
    pub async fn storage_code(&self) -> Option<String> {
        let token = self.serve?;
        Some(code("host", &(self.addr().await, token)))
    }

    /// Keep every space you are in on a storage node.
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

    /// Our storage nodes, each with whether it keeps `space`.
    pub fn stored(&self, space: &Id) -> Vec<(EndpointId, bool)> {
        let all = *space == self.own;
        let v = self.view(&self.own, |s| {
            let v = s.storage().into_iter().map(|(a, _)| a.id);
            v.map(|n| (n, all || s.keeps(&n, space))).collect()
        });
        v.unwrap_or_default()
    }

    /// Our storage nodes that keep `space` (our own space: all of them).
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
        let me = self.id();
        let list = self.view(&sp.id, |s| {
            s.role(&me) == Role::Admin
                && !s.hosts().iter().any(|h| h.id == addr.id)
        });
        if list == Some(true) {
            let _ =
                self.post(sp, Op::Host { node: addr.clone(), on: true }).await;
        }
        let id = addr.id;
        self.sync(sp, addr);
        if let Some(tx) = sp.tx() {
            let _ = tx.join_peers(vec![id]).await;
        }
    }

    fn blob(&self, h: &Id) -> PathBuf {
        self.dir.join("blobs").join(h.to_string())
    }

    /// Seal a file and keep its ciphertext; send it in an `Op::Post`.
    pub async fn attach(
        self: &Arc<Self>,
        space: &Id,
        name: &str,
        data: &[u8],
    ) -> Result<File> {
        ensure!(data.len() <= MAX_FILE, "files are at most 25 MiB");
        let key = rand();
        let ct = crate::crypto::seal(&key, b"irch file", data);
        let hash = Id(blake3::hash(&ct).into());
        self.keep(space, &hash, &ct)?;
        let (name, size) =
            (name.chars().take(128).collect(), data.len() as u64);
        Ok(File { name, size, hash, key: Id(key) })
    }

    /// Store a blob here, and on our storage nodes that keep `space`.
    fn keep(self: &Arc<Self>, space: &Id, hash: &Id, ct: &[u8]) -> Result<()> {
        std::fs::create_dir_all(self.dir.join("blobs"))?;
        store::write(&self.blob(hash), ct)?;
        for (addr, token) in self.stores(space) {
            let (n, put) = (self.clone(), Ask::Put(token, ct.to_vec()));
            tokio::spawn(async move { n.ask(addr, &put).await });
        }
        Ok(())
    }

    /// A file left where it is, served from this device to members while it
    /// is online: any size, no copy, not sealed (QUIC encrypts the way),
    /// checked chunk by chunk by whoever fetches it. `name`: else the path's.
    pub async fn offer(&self, path: &Path, name: Option<&str>) -> Result<File> {
        use iroh_blobs::api::blobs::{AddPathOptions, ImportMode};
        let files = self.files.as_ref().context("storage nodes offer none")?;
        let path = std::path::absolute(path)?;
        let size = std::fs::metadata(&path)?.len();
        let (format, mode) = (Default::default(), ImportMode::TryReference);
        let opts = AddPathOptions { path: path.clone(), format, mode };
        let tag = files.add_path_with_opts(opts).with_tag().await?;
        let base = path.file_name().and_then(|n| n.to_str());
        let name = name.or(base).unwrap_or("file").chars().take(128).collect();
        let hash = Id(*tag.hash.as_bytes());
        Ok(File { name, size, hash, key: crate::op::OFFERED })
    }

    /// An offered file, from whichever member has it (its sender, first),
    /// resumed where a try left off; served on to members from here.
    async fn pull(&self, space: &Id, f: &File) -> Result<&FsStore> {
        let files = self.files.as_ref().context("storage nodes fetch none")?;
        let hash = iroh_blobs::Hash::from_bytes(f.hash.0);
        if !files.has(hash).await? {
            let sp = self.get(space)?;
            let mut peers: Vec<EndpointId> = self.bootstrap(&sp);
            peers.extend(sp.nbrs());
            let got = files.downloader(&self.ep).download(hash, peers).await;
            got.ok().context(
                "not reachable now: ask whoever sent it to open irch",
            )?;
        }
        Ok(files)
    }

    async fn export(&self, space: &Id, f: &File, to: &Path) -> Result<()> {
        use iroh_blobs::api::blobs::{ExportMode, ExportOptions};
        let files = self.pull(space, f).await?;
        let hash = iroh_blobs::Hash::from_bytes(f.hash.0);
        let (mode, target) = (ExportMode::TryReference, to.to_path_buf());
        std::fs::remove_file(to)?;
        files.export_with_opts(ExportOptions { hash, mode, target }).await?;
        Ok(())
    }

    /// Whether `d` is a device of someone in a space with us.
    fn member_device(&self, d: &EndpointId) -> bool {
        let has =
            |id: &Id| self.view(id, |s| s.devices(usize::MAX).contains(d));
        self.spaces().iter().any(|id| has(id) == Some(true))
    }

    /// An attachment's plaintext, from here or from anyone who has it.
    pub async fn file(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
    ) -> Result<Vec<u8>> {
        if f.offered() {
            ensure!(f.size <= MAX_FILE as u64, "too big to open here: save it");
            let hash = iroh_blobs::Hash::from_bytes(f.hash.0);
            let files = self.pull(space, f).await?;
            return Ok(files.get_bytes(hash).await?.to_vec());
        }
        let ct = match std::fs::read(self.blob(&f.hash)) {
            Ok(ct) => ct,
            Err(_) => {
                let sp = self.get(space)?;
                let (mut peers, mut got, get) =
                    (sp.nbrs(), None, Ask::Get(f.hash));
                peers.extend(self.bootstrap(&sp));
                for p in peers {
                    let ask = self.ask(p.into(), &get);
                    let wait =
                        tokio::time::timeout(Duration::from_secs(20), ask);
                    if let Ok(Ok(b)) = wait.await
                        && blake3::hash(&b) == f.hash.0
                    {
                        got = Some(b);
                        break;
                    }
                }
                let ct = got.context("nobody reachable has that file now")?;
                self.keep(space, &f.hash, &ct)?;
                ct
            }
        };
        crate::crypto::open(&f.key.0, b"irch file", &ct).context("damaged file")
    }

    /// An attachment written into `dir`, never over anything already there.
    pub async fn save(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
        dir: &Path,
    ) -> Result<PathBuf> {
        use std::io::{ErrorKind::AlreadyExists, Write};
        let data = match f.offered() {
            true => vec![],
            false => self.file(space, f).await?,
        };
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
                    if f.offered() {
                        drop(out);
                        let got = self.export(space, f, &p).await;
                        got.inspect_err(|_| drop(std::fs::remove_file(&p)))?;
                    }
                    // Windows: from the internet, like a browser download.
                    #[cfg(windows)]
                    let _ = std::fs::write(
                        format!("{}:Zone.Identifier", p.display()),
                        "[ZoneTransfer]\r\nZoneId=3\r\n",
                    );
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
        let b = rx.read_to_end(MAX_FILE + 1024).await?;
        c.close(0u32.into(), b"done");
        Ok(b)
    }

    /// Answer a blob request.
    async fn give(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        // Only a storage node takes a file; others read a 64-byte ask.
        let max = if self.serve.is_some() { MAX_FILE + 1024 } else { 64 };
        match postcard::from_bytes(&rx.read_to_end(max).await?)? {
            Ask::Get(h) => {
                tx.write_all(&std::fs::read(self.blob(&h)).unwrap_or_default())
                    .await?
            }
            Ask::Put(t, b) if self.serve == Some(t) => {
                std::fs::create_dir_all(self.dir.join("blobs"))?;
                store::write(&self.blob(&Id(blake3::hash(&b).into())), &b)?;
            }
            Ask::Put(..) => bail!("not a storage node of yours"),
        }
        tx.finish()?;
        let _ = tokio::time::timeout(Duration::from_secs(10), c.closed()).await;
        Ok(())
    }

    /// The space a sync session asks for. Anyone may read one we have; a
    /// storage node's owner (its token) has it keep a new one, and names
    /// the devices it may take that one from.
    pub(crate) fn find(
        self: &Arc<Self>,
        h: &Hello,
        from: EndpointId,
    ) -> Option<Arc<Space>> {
        let sp = self.get(&h.space).ok();
        if !self.serve.is_some_and(|t| h.token == Some(t)) {
            return sp;
        }
        let ok: Vec<_> =
            std::iter::once(from).chain(h.devices.clone()).collect();
        let mut l = lock(&self.local);
        if l.hosted.get(&h.space) != Some(&ok) {
            l.hosted.insert(h.space, ok);
            drop(l);
            self.persist();
        }
        sp.or_else(|| self.open(None, h.space, vec![from.into()]).ok())
    }

    /// Whether `sp` takes ops from `peer`. Members check every op; a
    /// storage node cannot, so it takes only from devices vouched for.
    pub(crate) fn takes(&self, sp: &Space, peer: &EndpointId) -> bool {
        let l = lock(&self.local);
        sp.inv.is_some()
            || l.hosted.get(&sp.id).is_some_and(|v| v.contains(peer))
    }

    async fn sync_with(self: Arc<Self>, sp: Arc<Space>, peer: EndpointAddr) {
        let id = peer.id;
        if !self.takes(&sp, &id) || !lock(&sp.busy).insert(id) {
            return;
        }
        // Our storage nodes that keep it get their token, and whom to
        // take from.
        let stores = self.stores(&sp.id);
        let token = stores.into_iter().find(|s| s.0.id == id).map(|s| s.1);
        let r = async {
            let c = self.ep.connect(peer, sync::ALPN).await?;
            let n = sync::initiate(&self, &sp, &c, token).await;
            c.close(0u32.into(), b"done");
            n
        };
        let r = r.await;
        lock(&sp.busy).remove(&id);
        match r {
            Ok(0) => {}
            Ok(count) => self.emit(Event::Synced { space: sp.id, count }),
            Err(e) => debug!("sync with {}: {e:#}", id.fmt_short()),
        }
    }

    /// A new direct neighbor: catch up with it, and remember how to reach it.
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
            let mut l = lock(&n.local);
            if !a.is_empty() && l.peers.len() < 512 {
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
            // A storage node takes nothing from gossip, which it cannot
            // check: it fetches what it lacks from whoever passed it on.
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
            let s = b
                .keys
                .get(&e.epoch)
                .and_then(|k| op::open(&sp.id, k, &e, true));
            // Presence, typing and calls only from who holds the key now.
            s.filter(|s| !b.st.stale(&s.epoch, &(s.ts, s.id), &s.author))
        };
        let Some(s) = s else { return };
        if s.ts.abs_diff(now()) > 120_000 {
            return;
        }
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
            // A device back from sleep says hello: tell it of our call now,
            // not at the next 15 s reminder, or it misses the ring.
            Op::Here => {
                if let Some((_, ch)) = self.in_call().filter(|c| c.0 == sp.id) {
                    let (n, sp) = (self.clone(), sp.clone());
                    let call = Op::Call { channel: Some(ch) };
                    tokio::spawn(async move { n.signal(&sp, call).await });
                }
            }
            Op::Call { channel } => {
                let mut c = lock(&sp.calls);
                let was = match channel {
                    Some(ch) => {
                        c.insert(s.author, (ch, s.device, Instant::now()))
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

    /// Woken, in front again, or on another network: re-probe, re-dial,
    /// catch up now rather than at the next heartbeat.
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
    }
}

/// One gossip subscription per space, renewed if it ever closes.
async fn pump(n: Arc<Node>, sp: Weak<Space>, boot: Vec<EndpointId>) {
    loop {
        let Some(s) = sp.upgrade() else { return };
        let peers = boot.iter().copied().chain(n.bootstrap(&s)).collect();
        let topic =
            n.gossip.subscribe(TopicId::from_bytes(s.id.0), peers).await;
        let Ok(topic) = topic else {
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        };
        let (tx, mut rx) = topic.split();
        *lock(&s.tx) = Some(tx);
        drop(s);
        while let Some(ev) = rx.next().await {
            let Some(s) = sp.upgrade() else { return };
            match ev {
                Ok(Gossiped::Received(m)) => {
                    n.neighbor(&s, m.delivered_from);
                    n.signaled(&s, &m.content, m.delivered_from);
                }
                Ok(Gossiped::NeighborUp(p)) => n.neighbor(&s, p),
                Ok(Gossiped::NeighborDown(p)) => {
                    lock(&s.nbrs).remove(&p);
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

/// Presence every minute; re-dial spaces we are alone in; reconcile with
/// a neighbor every few minutes; a late tick means we slept: resume.
/// For push endpoints and updates: TLS as iroh sets it up, roots compiled
/// in.
pub fn http() -> reqwest::Client {
    #[cfg(not(target_os = "android"))]
    let ca = iroh::tls::CaTlsConfig::system();
    #[cfg(target_os = "android")]
    let ca = iroh::tls::CaTlsConfig::embedded();
    let tls = ca.client_config(iroh::tls::default_provider()).expect("TLS");
    let b = reqwest::Client::builder().tls_backend_preconfigured(tls);
    // Stalls end a request, not its length: an update can be large.
    let b = b.connect_timeout(Duration::from_secs(20));
    b.read_timeout(Duration::from_secs(30)).build().expect("HTTP client")
}

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
