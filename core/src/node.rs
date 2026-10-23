//! One iroh endpoint carries gossip (live) and sync (catch-up) for every
//! space. Every op, local or remote, takes the same path: append to the
//! space's log, open with a known epoch key, fold, notify.

use crate::crypto::{Key, Me, kem_open, kem_seal, rand};
use crate::op::{self, Env, File, Invite, Op, Role, code, tag, uncode};
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
pub const MAX_TEXT: usize = 4000;
pub const MAX_NAME: usize = 64;
pub const MAX_FILE: usize = 25 << 20;
const BLOB: &[u8] = b"irch/blob/1";

/// How to reach peers beyond the LAN.
#[derive(Clone, Debug, Default)]
pub enum Net {
    /// n0's relays and DNS, plus the Mainline DHT.
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
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
pub enum Event {
    Changed { space: Id },
    Typing { space: Id, channel: Ch, user: Id },
    Presence { space: Id },
    Synced { space: Id, count: usize },
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

pub struct Space {
    pub id: Id,
    inv: Option<Invite>,
    book: Mutex<Book>,
    tx: Mutex<Option<GossipSender>>,
    nbrs: Mutex<HashSet<EndpointId>>,
    seen: Mutex<HashMap<Id, Instant>>,
    task: OnceLock<AbortOnDropHandle<()>>,
}

struct Book {
    log: Log,
    idx: BTreeMap<u32, BTreeSet<Id>>,
    at: HashMap<Id, (u64, u32)>,
    keys: HashMap<[u8; 8], Key>,
    pending: HashMap<[u8; 8], Vec<(u64, bool, Env)>>,
    st: State,
    knocks: Vec<Invite>,
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
    /// Open what known keys open; keys learned on the way open more.
    fn fold(&mut self, sp: &Id, me: &Me, mut work: Vec<(u64, bool, Env)>) {
        let mut ops = vec![];
        while let Some((at, checked, e)) = work.pop() {
            let Some(key) = self.keys.get(&e.epoch) else {
                self.pending.entry(e.epoch).or_default().push((at, checked, e));
                continue;
            };
            let Some(s) = op::open(sp, key, &e, !checked) else { continue };
            if !checked && let Err(e) = self.log.check(at) {
                warn!("log: {e}");
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
                Op::Back { key } => Some(*key),
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
                work.extend(
                    self.pending.remove(&tag(&k)).into_iter().flatten(),
                );
            }
            ops.push(s);
        }
        self.st.extend(ops);
    }

    /// The key to seal with: the newest valid epoch, never one older than
    /// the invite's. `Err` if keys moved on without us.
    fn key(&self, inv: &Invite) -> Result<Key> {
        let valid = self.st.epochs();
        let newest =
            valid.last().filter(|_| valid.iter().any(|e| e.0 == tag(&inv.key)));
        match newest {
            None => Ok(inv.key),
            Some((t, _)) => self.keys.get(t).copied().ok_or_else(|| {
                anyhow!("keys changed without you: ask for a new invite")
            }),
        }
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
    ep: Endpoint,
    gossip: Gossip,
    router: OnceLock<Router>,
    lookup: MemoryLookup,
    spaces: RwLock<HashMap<Id, Arc<Space>>>,
    local: Mutex<Local>,
    name: Mutex<String>,
    events: broadcast::Sender<Event>,
    clock: AtomicU64,
    invisible: AtomicBool,
    serve: Option<[u8; 16]>,
}

/// Serves sync and blobs; the ALPN says which.
#[derive(Debug, Clone)]
struct Proto(Weak<Node>);

impl ProtocolHandler for Proto {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        let gone = || AcceptError::from_err(std::io::Error::other("closing"));
        let n = self.0.upgrade().ok_or_else(gone)?;
        let r = match c.alpn() == sync::ALPN {
            true => sync::respond(&n, &c).await,
            false => n.give(&c).await,
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
        let lookup = MemoryLookup::new();
        local.peers.values().for_each(|a| lookup.add_endpoint_info(a.clone()));
        let bind = async |port: u16| {
            let b = match &cfg.net {
                Net::Default => Endpoint::builder(presets::N0).address_lookup(
                    iroh_mainline_address_lookup::DhtAddressLookup::builder(),
                ),
                Net::Relay(urls) => Endpoint::builder(presets::Minimal)
                    .relay_mode(RelayMode::custom(urls.clone()))
                    .address_lookup(
                        iroh_mainline_address_lookup::DhtAddressLookup::builder(
                        ),
                    ),
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
        let gossip =
            Gossip::builder().max_message_size(GOSSIP_MAX).spawn(ep.clone());
        let own_inv = own_invite(&me, link.map(|l| l.1).unwrap_or_default());
        let n = Arc::new(Self {
            own: own_inv.id(),
            me,
            dir: cfg.dir,
            net: cfg.net,
            ep,
            gossip,
            router: OnceLock::new(),
            lookup,
            spaces: RwLock::default(),
            invisible: AtomicBool::new(local.invisible),
            local: Mutex::new(local),
            name: Mutex::new(cfg.name),
            events: broadcast::channel(1024).0,
            clock: AtomicU64::new(0),
            serve,
        });
        let router = Router::builder(n.ep.clone())
            .accept(iroh_gossip::ALPN, n.gossip.clone())
            .accept(sync::ALPN, Proto(Arc::downgrade(&n)))
            .accept(BLOB, Proto(Arc::downgrade(&n)))
            .spawn();
        let _ = n.router.set(router);
        n.persist();
        if serve.is_some() {
            let hosted = lock(&n.local).hosted.clone();
            for (id, peers) in hosted {
                n.open(
                    None,
                    id,
                    peers.into_iter().map(EndpointAddr::new).collect(),
                )?;
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
        tokio::spawn(self.clone().sync_with(sp.clone(), peer.into(), None));
    }

    fn seal(&self, sp: &Space, key: &Key, op: Op) -> (op::Signed, Vec<u8>) {
        let (me, dev) = (&self.me.user, self.ep.id());
        op::seal(&sp.id, key, me, dev, self.tick(), op)
    }

    fn persist(&self) {
        let l = lock(&self.local);
        if let Err(e) = store::save(&self.dir.join("local"), &*l) {
            warn!("saving local state: {e}");
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
        lock(&sp.book).key(inv).is_err()
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

    pub fn set_invisible(&self, on: bool) {
        self.invisible.store(on, Relaxed);
        lock(&self.local).invisible = on;
        self.persist();
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

    /// Per channel, the newest unread mention of us after `after`.
    pub fn mentions(&self, id: &Id, after: u64) -> Vec<(Ch, Message)> {
        let me = self.id();
        let chans = self.view(id, |s| s.channels()).unwrap_or_default();
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
        let (keys, pending, knocks) = (HashMap::new(), HashMap::new(), vec![]);
        let (idx, at) = (BTreeMap::new(), HashMap::new());
        let mut b = Book { log, idx, at, keys, pending, st, knocks };
        let mut work = vec![];
        for (o, checked, raw) in recs {
            let Ok(e) = postcard::from_bytes::<Env>(&raw) else { continue };
            let i = Id(blake3::hash(&raw).into());
            b.at.insert(i, (o, raw.len() as u32));
            b.idx.entry((e.ts / DAY) as u32).or_default().insert(i);
            work.push((o, checked, e));
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
        v.extend(lock(&self.local).hosted.get(&sp.id).into_iter().flatten());
        let me = self.ep.id();
        let mut seen = HashSet::new();
        v.retain(|p| *p != me && seen.insert(*p));
        v
    }

    /// Store, open and fold envelopes; returns how many were new.
    pub(crate) fn ingest(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        raws: Vec<Vec<u8>>,
        mine: bool,
    ) -> usize {
        let new = {
            let mut b = lock(&sp.book);
            let mut new = vec![];
            for raw in raws {
                let Ok(e) = postcard::from_bytes::<Env>(&raw) else { continue };
                let i = Id(blake3::hash(&raw).into());
                if e.live || e.ts > now() + DAY || b.at.contains_key(&i) {
                    continue;
                }
                let at = b.log.append(mine, &raw, mine);
                let Ok(at) = at.inspect_err(|e| warn!("log: {e}")) else {
                    continue;
                };
                b.at.insert(i, (at, raw.len() as u32));
                b.idx.entry((e.ts / DAY) as u32).or_default().insert(i);
                new.push((at, mine, e));
            }
            let n = new.len();
            if sp.inv.is_some() && n > 0 {
                b.fold(&sp.id, &self.me, new);
            }
            n
        };
        if new > 0 {
            self.emit(Event::Changed { space: sp.id });
            tokio::spawn(self.clone().effects(sp.clone()));
        }
        new
    }

    /// What ingesting may require: DMs to accept, spaces to open or close
    /// for our other devices, a leaking epoch to replace.
    async fn effects(self: Arc<Self>, sp: Arc<Space>) {
        let (knocks, leaks) = {
            let mut b = lock(&sp.book);
            let admin = b.st.role(&self.id()) == Role::Admin;
            (
                std::mem::take(&mut b.knocks),
                admin && b.st.leaks() && b.st.dm.is_none(),
            )
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
        if leaks {
            // Admins may race to fix it; a little jitter keeps that rare.
            let wait = 1000 + u16::from_le_bytes(rand()) as u64 % 9000;
            tokio::time::sleep(Duration::from_millis(wait)).await;
            if self.view(&sp.id, |s| s.leaks()) == Some(true)
                && let Err(e) = self.rekey(&sp.id).await
            {
                warn!("rekey: {e}");
            }
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
        let key = lock(&sp.book).key(inv)?;
        let (s, raw) = self.seal(sp, &key, op);
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
            Op::Rename { name: n } | Op::Channel { name: Some(n), .. } => {
                fits(n, MAX_NAME)
            }
            Op::React { emoji, .. } => fits(emoji, 32),
            Op::Role { role, .. } => *role != Role::Banned,
            Op::Edit { .. } | Op::Channel { .. } => true,
            _ => false,
        };
        ensure!(ok, "that cannot be sent (empty, too long, or not allowed)");
        ensure!(*id != self.own, "not a space");
        self.post(&self.get(id)?, op).await
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
        let stores = self.view(&self.own, |s| s.storage()).unwrap_or_default();
        for (addr, token) in stores {
            self.host(&sp, addr, token).await;
        }
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
        if let Ok(sp) = self.get(&inv.id()) {
            return Ok(sp.id);
        }
        self.enter(inv).await
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
        inv.key = lock(&sp.book).key(&inv)?;
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
        if on {
            self.rekey(id).await?;
        }
        Ok(())
    }

    /// A fresh epoch key for everyone not banned. Old invites stop
    /// working; history stays readable through `Back`.
    pub async fn rekey(self: &Arc<Self>, id: &Id) -> Result<()> {
        let sp = self.get(id)?;
        let inv = sp.inv.as_ref().ok_or_else(|| anyhow!("only hosted here"))?;
        let (new, old, boxes) = {
            let b = lock(&sp.book);
            ensure!(b.st.role(&self.id()) == Role::Admin, "only admins rekey");
            let (new, old): (Key, Key) = (rand(), b.key(inv)?);
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

    async fn signal(&self, sp: &Space, op: Op) {
        let Some(inv) = &sp.inv else { return };
        let Ok(key) = lock(&sp.book).key(inv) else { return };
        if self.invisible() {
            return;
        }
        if let Some(tx) = sp.tx() {
            let _ = tx.broadcast(self.seal(sp, &key, op).1.into()).await;
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
        for sp in self.all() {
            self.host(&sp, addr.clone(), token).await;
        }
        Ok(())
    }

    async fn host(
        self: &Arc<Self>,
        sp: &Arc<Space>,
        addr: EndpointAddr,
        token: [u8; 16],
    ) {
        self.lookup.add_endpoint_info(addr.clone());
        let listed =
            self.view(&sp.id, |s| s.hosts().iter().any(|h| h.id == addr.id));
        if listed == Some(false) {
            let _ =
                self.post(sp, Op::Host { node: addr.clone(), on: true }).await;
        }
        let id = addr.id;
        tokio::spawn(self.clone().sync_with(sp.clone(), addr, Some(token)));
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
        name: &str,
        data: &[u8],
    ) -> Result<File> {
        ensure!(data.len() <= MAX_FILE, "files are at most 25 MiB");
        let key = rand();
        let ct = crate::crypto::seal(&key, b"irch file", data);
        let hash = Id(blake3::hash(&ct).into());
        self.keep(&hash, &ct)?;
        let (name, size) =
            (name.chars().take(128).collect(), data.len() as u64);
        Ok(File { name, size, hash, key: Id(key) })
    }

    /// Store a blob here, and on our storage nodes.
    fn keep(self: &Arc<Self>, hash: &Id, ct: &[u8]) -> Result<()> {
        std::fs::create_dir_all(self.dir.join("blobs"))?;
        store::write(&self.blob(hash), ct)?;
        let stores = self.view(&self.own, |s| s.storage()).unwrap_or_default();
        for (addr, token) in stores {
            let (n, put) = (self.clone(), Ask::Put(token, ct.to_vec()));
            tokio::spawn(async move { n.ask(addr, &put).await });
        }
        Ok(())
    }

    /// An attachment's plaintext, from here or from anyone who has it.
    pub async fn file(
        self: &Arc<Self>,
        space: &Id,
        f: &File,
    ) -> Result<Vec<u8>> {
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
                self.keep(&f.hash, &ct)?;
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
        let data = self.file(space, f).await?;
        let name = Path::new(&f.name).file_name().and_then(|n| n.to_str());
        let name = name.unwrap_or_default().trim_start_matches('.');
        let name = if name.is_empty() { "file" } else { name };
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
                Ok(mut out) => return Ok(out.write_all(&data).map(|_| p)?),
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
        match postcard::from_bytes(&rx.read_to_end(MAX_FILE + 1024).await?)? {
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

    /// Serve a sync session: a space we have, or, for a storage node, one
    /// its owner asks it to keep.
    pub(crate) fn find(
        self: &Arc<Self>,
        h: &Hello,
        from: EndpointId,
    ) -> Option<Arc<Space>> {
        if let Ok(sp) = self.get(&h.space) {
            return Some(sp);
        }
        self.serve.filter(|t| h.token == Some(*t))?;
        lock(&self.local).hosted.insert(h.space, vec![from]);
        self.persist();
        self.open(None, h.space, vec![from.into()]).ok()
    }

    async fn sync_with(
        self: Arc<Self>,
        sp: Arc<Space>,
        peer: EndpointAddr,
        token: Option<[u8; 16]>,
    ) {
        let id = peer.id;
        let r = async {
            let c = self.ep.connect(peer, sync::ALPN).await?;
            let n = sync::initiate(&self, &sp, &c, token).await;
            c.close(0u32.into(), b"done");
            n
        };
        match r.await {
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
            n.clone().sync_with(sp.clone(), peer.into(), None).await;
            n.signal(&sp, Op::Here).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
            n.clone().sync_with(sp.clone(), peer.into(), None).await;
            let Some(info) = n.ep.remote_info(peer).await else { return };
            let a = EndpointAddr::from_parts(
                peer,
                info.into_addrs().map(|a| a.into_addr()),
            );
            let mut l = lock(&n.local);
            if let Some(h) =
                l.hosted.get_mut(&sp.id).filter(|h| !h.contains(&peer))
            {
                h.push(peer);
                h.drain(..h.len().saturating_sub(16));
            }
            if !a.is_empty() && l.peers.len() < 512 {
                l.peers.insert(peer, a);
            }
            drop(l);
            n.persist();
        });
    }

    fn signaled(self: &Arc<Self>, sp: &Arc<Space>, raw: &[u8]) {
        let Ok(e) = postcard::from_bytes::<Env>(raw) else { return };
        if !e.live {
            self.ingest(sp, vec![raw.to_vec()], false);
            return;
        }
        let key = lock(&sp.book).keys.get(&e.epoch).copied();
        let Some(s) = key.and_then(|k| op::open(&sp.id, &k, &e, true)) else {
            return;
        };
        if s.ts.abs_diff(now()) > 120_000 {
            return;
        }
        if sp.saw(s.author) {
            self.emit(Event::Presence { space: sp.id });
        }
        if let Op::Typing { channel } = s.op {
            self.emit(Event::Typing { space: sp.id, channel, user: s.author });
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
                    n.signaled(&s, &m.content);
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
