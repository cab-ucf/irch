//! An order-free fold of a space's ops. Control re-folds in `(ts, id)`
//! order; authors pick their times, so only current admins' rekeys count.

use crate::op::{File, Invite, Op, Role, Signed};
use crate::{Ch, Id};
use iroh::{EndpointAddr, EndpointId};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::mem::take;

/// Author clock, then id; also the paging cursor.
pub type Pos = (u64, Id);
pub const DM: Ch = Ch([0; 8]);

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Member {
    pub id: Id,
    pub name: String,
    pub role: Role,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Message {
    pub id: Id,
    pub author: Id,
    pub name: String,
    pub ts: u64,
    pub text: String,
    pub edited: bool,
    pub reply: Option<Id>,
    pub quote: Option<Quote>,
    pub reactions: Vec<(String, Vec<Id>)>,
    pub files: Vec<File>,
    pub pinned: bool,
    pub thread: usize,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Thread {
    pub id: Ch,
    pub root: Id,
    pub parent: Ch,
    pub by: Id,
    pub name: String,
    pub count: usize,
    pub last: u64,
    /// Only its starter replied: they may still delete it.
    pub alone: bool,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Quote {
    pub author: Id,
    pub name: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Epoch {
    pub tag: [u8; 8],
    pub to: Vec<Id>,
    pub at: Pos,
    pub by: Id,
}

type Reacts = HashMap<(Id, String), (Pos, bool)>;

struct Post {
    author: Id,
    text: String,
    reply: Option<Id>,
    files: Vec<File>,
}

#[derive(Default)]
pub struct State {
    /// Who made it (its id is bound to them); `head` owns it now.
    pub owner: Id,
    head: Id,
    // Identities retired, and from when.
    burned: HashMap<Id, Pos>,
    pub dm: Option<Id>,
    hint: String,
    control: BTreeMap<Pos, (Id, Op)>,
    posts: HashMap<Ch, BTreeMap<Pos, Post>>,
    index: HashMap<Id, (Ch, Pos)>,
    // Re-sealed by others, one op may come under several epochs.
    sealed: HashMap<Id, Vec<[u8; 8]>>,
    pushes: HashMap<(Id, EndpointId), (Pos, Option<String>)>,
    roots: HashMap<Ch, Id>,
    edits: HashMap<Id, BTreeMap<Pos, (Id, Option<String>)>>,
    topics: HashMap<Ch, BTreeMap<Pos, (Id, String)>>,
    reacts: HashMap<Id, Reacts>,
    pins: HashMap<Id, (Pos, Id, bool)>,
    authors: HashMap<Id, u64>,
    devices: HashMap<(EndpointId, Id), u64>,
    // Derived from `control` by `refold`:
    name: Option<String>,
    channels: BTreeMap<Ch, String>,
    order: Vec<Ch>,
    profiles: HashMap<Id, (String, Vec<u8>)>,
    profiled: HashMap<Id, Pos>,
    roles: HashMap<Id, Role>,
    epochs: Vec<Epoch>,
    bans: Vec<(Pos, Id)>,
    hosts: BTreeMap<EndpointId, Option<EndpointAddr>>,
    holds: BTreeMap<EndpointId, [u8; 16]>,
    spaces: BTreeMap<Id, Option<Invite>>,
    storage: BTreeMap<EndpointId, Option<(EndpointAddr, [u8; 16])>>,
    stored: BTreeMap<(EndpointId, Id), bool>,
}

impl State {
    pub fn new(inv: &Invite) -> Self {
        let (owner, dm, hint) = (inv.owner, inv.dm, inv.name.clone());
        let head = owner;
        let mut s = Self { owner, head, dm, hint, ..Default::default() };
        s.refold();
        s
    }

    pub fn extend(&mut self, ops: impl IntoIterator<Item = Signed>) -> bool {
        let (mut new, mut control) = (false, false);
        for s in ops {
            let (n, c) = self.insert(s);
            (new, control) = (new | n, control | c);
        }
        if control {
            self.refold();
        }
        new
    }

    fn insert(&mut self, s: Signed) -> (bool, bool) {
        let at = (s.ts, s.id);
        let tags = self.sealed.entry(s.id).or_default();
        if !tags.contains(&s.epoch) {
            tags.push(s.epoch);
        }
        let t = self.authors.entry(s.author).or_default();
        *t = (*t).max(s.ts);
        // Never later than now: a date set ahead does not rank it first.
        let t = self.devices.entry((s.device, s.author)).or_default();
        *t = (*t).max(s.ts.min(crate::now()));
        match s.op {
            Op::Post { channel, text, reply, files } => {
                self.index.insert(s.id, (channel, at));
                self.roots.insert(s.id.thread(), s.id);
                let files = files.into_iter().map(File::safe).collect();
                let p = Post { author: s.author, text, reply, files };
                let ch = self.posts.entry(channel).or_default();
                (ch.insert(at, p).is_none(), false)
            }
            Op::Edit { target, text } => {
                let e = self.edits.entry(target).or_default();
                (e.insert(at, (s.author, text)).is_none(), false)
            }
            Op::Topic { thread, name } => {
                let t = self.topics.entry(thread).or_default();
                let name = crate::op::name(&name);
                (t.insert(at, (s.author, name)).is_none(), false)
            }
            Op::React { target, emoji, on } => {
                let r = self.reacts.entry(target).or_default();
                // 20 kinds per person and message: no wall of reactions.
                let mine = r.iter().filter(|((a, _), v)| *a == s.author && v.1);
                let full = on && mine.count() >= 20;
                match r.get(&(s.author, emoji.clone())) {
                    None if full => (false, false),
                    Some((p, _)) if *p >= at => (false, false),
                    _ => (
                        r.insert((s.author, emoji), (at, on)) != Some((at, on)),
                        false,
                    ),
                }
            }
            Op::Pin { target, on } => match self.pins.get(&target) {
                Some((p, ..)) if *p >= at => (false, false),
                _ => {
                    let v = (at, s.author, on);
                    (self.pins.insert(target, v) != Some(v), false)
                }
            },
            Op::Push { url } => match self.pushes.get(&(s.author, s.device)) {
                Some((p, _)) if *p >= at => (false, false),
                _ => {
                    let k = (s.author, s.device);
                    (self.pushes.insert(k, (at, url)).is_none(), false)
                }
            },
            Op::Back { .. }
            | Op::Knock { .. }
            | Op::Typing { .. }
            | Op::Here
            | Op::Call { .. }
            | Op::Gone => (false, false),
            op => {
                let new = self.control.insert(at, (s.author, op)).is_none();
                (new, new)
            }
        }
    }

    fn refold(&mut self) {
        (self.name, self.channels) = (None, BTreeMap::new());
        self.order = vec![];
        (self.profiles, self.roles) = (HashMap::new(), HashMap::new());
        self.profiled = HashMap::new();
        (self.epochs, self.hosts, self.bans) =
            (vec![], BTreeMap::new(), vec![]);
        self.holds = BTreeMap::new();
        (self.spaces, self.storage) = (BTreeMap::new(), BTreeMap::new());
        self.stored = BTreeMap::new();
        (self.head, self.burned) = (self.owner, HashMap::new());
        if self.dm.is_some() {
            self.channels.insert(DM, "dm".into());
        }
        let control = take(&mut self.control);
        for (at, (a, op)) in &control {
            let role = self.role(a);
            let (admin, own) = (role == Role::Admin, *a == self.owner);
            let head = *a == self.head;
            match op {
                _ if role == Role::Banned || self.stale_at(at, a) => {}
                Op::Burn => drop(self.burned.insert(*a, *at)),
                Op::Owner { user }
                    if head && *user != *a && !self.out(user) =>
                {
                    self.roles.insert(*a, Role::Admin);
                    self.roles.remove(user);
                    self.head = *user;
                }
                Op::Profile { name, kem } => {
                    let n = crate::op::name(name);
                    self.profiles.insert(*a, (n, kem.clone()));
                    self.profiled.insert(*a, *at);
                }
                Op::Rename { name } if admin => {
                    self.name = Some(crate::op::name(name))
                }
                Op::Channel { id, name } if admin && self.dm.is_none() => {
                    match name {
                        Some(n) => {
                            let n = crate::op::name(n);
                            drop(self.channels.insert(*id, n))
                        }
                        None => drop(self.channels.remove(id)),
                    }
                }
                Op::Order { channels } if admin => {
                    self.order = channels.clone()
                }
                Op::Role { user, role: r } if admin && *user != self.head => {
                    let admins =
                        *r == Role::Admin || self.role(user) == Role::Admin;
                    if head || !admins {
                        self.roles.insert(*user, *r);
                        if *r == Role::Banned {
                            self.bans.push((*at, *user));
                        }
                    }
                }
                Op::Rekey { epoch, boxes } if admin => {
                    let to = boxes.iter().map(|b| b.0).collect();
                    self.epochs.push(Epoch {
                        tag: *epoch,
                        to,
                        at: *at,
                        by: *a,
                    });
                }
                // Admins' to list: invites send newcomers there first.
                Op::Host { node, on } if admin => {
                    drop(self.hosts.insert(node.id, on.then(|| node.clone())))
                }
                Op::Hold { node, token } if admin => {
                    self.holds.insert(*node, *token);
                }
                Op::Joined { invite } if own => {
                    self.spaces.insert(invite.id(), Some(invite.clone()));
                }
                Op::Left { space } if own => {
                    drop(self.spaces.insert(*space, None))
                }
                Op::Storage { addr, token, on } if own => {
                    let v = on.then(|| (addr.clone(), *token));
                    self.storage.insert(addr.id, v);
                }
                Op::Store { node, space, on } if own => {
                    self.stored.insert((*node, *space), *on);
                }
                _ => {}
            }
        }
        self.control = control;
        // Told under a key a later epoch left them out of: none, however dated.
        let cut = self.profiled.iter().filter(|(a, at)| {
            let tags = self.sealed.get(&at.1).map_or(&[][..], |t| t);
            tags.iter().all(|t| self.left_out(t, a))
        });
        let cut: Vec<Id> = cut.map(|(a, _)| *a).collect();
        for a in cut {
            self.profiles.entry(a).and_modify(|p| p.1.clear());
        }
    }

    fn left_out(&self, epoch: &[u8; 8], a: &Id) -> bool {
        let es = &self.epochs;
        let from = es.iter().find(|e| e.tag == *epoch).map(|e| e.at);
        es.iter().any(|e| Some(e.at) > from && !e.to.contains(a))
    }

    /// Sealed after a newer epoch left `a` out: an old invite, a banned member
    /// renamed. One given the new key, writing offline, stays.
    pub fn stale(&self, epoch: &[u8; 8], at: &Pos, a: &Id) -> bool {
        let es = &self.epochs;
        let Some(from) = es.iter().find(|e| e.tag == *epoch) else {
            return false;
        };
        let since = |e: &&Epoch| e.at > from.at && e.at < *at;
        es.iter().filter(since).any(|e| !e.to.contains(a))
    }

    fn stale_at(&self, at: &Pos, a: &Id) -> bool {
        let tags = self.sealed.get(&at.1);
        self.burned.get(a).is_some_and(|b| at > b)
            || tags.is_some_and(|t| t.iter().all(|t| self.stale(t, at, a)))
    }

    /// Banned, or retired: no keys, no wakes, no calls, no say.
    pub fn out(&self, u: &Id) -> bool {
        self.role(u) == Role::Banned || self.burned.contains_key(u)
    }

    /// Signed by an identity after it was retired.
    pub fn burned_after(&self, at: &Pos, a: &Id) -> bool {
        self.burned.get(a).is_some_and(|b| at > b)
    }

    /// Who owns it now.
    pub fn head(&self) -> Id {
        self.head
    }

    pub fn push(&self, a: &Id, d: &EndpointId) -> Option<&str> {
        self.pushes.get(&(*a, *d))?.1.as_deref()
    }

    pub fn pushes_of(&self, who: Option<&Id>) -> Vec<(EndpointId, String)> {
        let ok = |((a, _), (at, _)): &(&(Id, EndpointId), &(Pos, _))| {
            !self.out(a) && !self.stale_at(at, a)
        };
        // A few per member: anyone may claim any device.
        let p = self.pushes.iter().filter(ok);
        let mut p: Vec<_> =
            p.filter(|((a, _), _)| who.is_none_or(|w| a == w)).collect();
        p.sort_unstable_by_key(|x| std::cmp::Reverse(x.1.0));
        let mut n: HashMap<Id, usize> = HashMap::new();
        p.retain(|((a, _), _)| {
            let c = n.entry(*a).or_default();
            *c += 1;
            *c <= 4
        });
        p.into_iter()
            .filter_map(|((_, d), (_, u))| Some((*d, u.clone()?)))
            .collect()
    }

    pub fn peer(&self, me: &Id) -> Option<Id> {
        self.dm.map(|d| if *me == self.owner { d } else { self.owner })
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.hint)
    }

    pub fn role(&self, user: &Id) -> Role {
        match self.roles.get(user) {
            _ if *user == self.head => Role::Admin,
            r => r.copied().unwrap_or_default(),
        }
    }

    pub fn display_name(&self, user: &Id) -> String {
        let name = self.profiles.get(user).map(|p| p.0.clone());
        name.unwrap_or_else(|| user.to_string()[..8].into())
    }

    pub fn profile(&self, user: &Id) -> Option<&str> {
        Some(&self.profiles.get(user)?.0)
    }

    pub fn kem(&self, user: &Id) -> Option<&[u8]> {
        let k = &self.profiles.get(user)?.1;
        (!k.is_empty() && !self.out(user)).then_some(k)
    }

    pub fn channels(&self) -> Vec<(Ch, String)> {
        let at = |c: &Ch| self.order.iter().position(|o| o == c);
        let mut v: Vec<_> =
            self.channels.iter().map(|(i, n)| (*i, n.clone())).collect();
        // An admin's order; channels it leaves out after, by name.
        v.sort_by_key(|(c, n)| (at(c).unwrap_or(usize::MAX), n.clone(), *c));
        v
    }

    pub fn pins(&self, ch: &Ch) -> Vec<Message> {
        let mut v: Vec<_> = (self.pins.iter())
            .filter_map(|(id, (at, ..))| {
                let (c, m) = self.message(id)?;
                (c == *ch && m.pinned).then_some((*at, m))
            })
            .collect();
        v.sort_by_key(|p| std::cmp::Reverse(p.0));
        v.into_iter().map(|p| p.1).collect()
    }

    /// Only on a message in a channel, never in a thread.
    pub fn thread(&self, ch: &Ch) -> Option<Thread> {
        let root = *self.roots.get(ch)?;
        let (parent, (since, _)) = *self.index.get(&root)?;
        self.channels.contains_key(&parent).then_some(())?;
        let posts = self.posts.get(ch).into_iter().flatten();
        let seen: Vec<_> =
            posts.filter(|(k, p)| self.resolve(&k.1, p).is_some()).collect();
        let (count, last) =
            seen.iter().fold((0, since), |(n, _), (k, _)| (n + 1, k.0));
        let by = self.post(&root)?.author;
        let named = self.topics.get(ch).into_iter().flat_map(|t| t.iter());
        let may = |(at, (a, _)): &(&Pos, &(Id, String))| {
            let role = self.role(a);
            !self.stale_at(at, a)
                && role != Role::Banned
                && (*a == by || role == Role::Admin)
        };
        let topic = named.rev().find(may).map(|t| t.1);
        // An empty name deletes: an admin's, or its starter's while alone.
        if let Some((a, n)) = topic
            && n.is_empty()
            && (self.role(a) == Role::Admin
                || seen.iter().all(|(_, p)| p.author == by))
        {
            return None;
        }
        let alone = seen.iter().all(|(_, p)| p.author == by);
        let topic = topic.map(|t| t.1.clone()).filter(|n| !n.is_empty());
        let text = self.post(&root).and_then(|p| self.resolve(&root, p));
        let line = text
            .as_ref()
            .and_then(|t| t.0.lines().find(|l| !l.trim().is_empty()))
            .map(|l| l.chars().take(48).collect());
        let name = topic.or(line).unwrap_or("Thread".into());
        Some(Thread { id: *ch, root, parent, by, name, count, last, alone })
    }

    pub fn threads(&self) -> Vec<Thread> {
        let chs: HashSet<_> =
            self.posts.keys().chain(self.topics.keys()).collect();
        let t = chs.into_iter().filter_map(|c| self.thread(c));
        let named = |t: &Thread| self.topics.contains_key(&t.id);
        let mut v: Vec<_> = t.filter(|t| t.count > 0 || named(t)).collect();
        v.sort_by_key(|t| std::cmp::Reverse(t.last));
        v
    }

    pub fn rooms(&self) -> Vec<(Ch, String)> {
        let t = self.threads().into_iter().map(|t| (t.id, t.name));
        self.channels().into_iter().chain(t).collect()
    }

    pub fn members(&self) -> Vec<Member> {
        let ids = self.profiles.keys().chain(self.roles.keys());
        let mut v: Vec<_> = ids
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|id| Member {
                id: *id,
                name: self.display_name(id),
                role: self.role(id),
            })
            .collect();
        v.sort_by_key(|m| m.name.to_lowercase());
        v
    }

    pub fn epochs(&self) -> &[Epoch] {
        &self.epochs
    }

    /// Newest first; a demoted admin's rekeys stop counting, however dated.
    pub fn live(&self) -> impl Iterator<Item = &Epoch> + Clone {
        self.epochs.iter().rev().filter(|e| self.role(&e.by) == Role::Admin)
    }

    pub fn epoch(&self) -> Option<&Epoch> {
        self.live().next()
    }

    /// Someone banned may hold the current key: time to rekey.
    pub fn leaks(&self) -> bool {
        let banned = |u: &Id| self.role(u) == Role::Banned;
        let e = self.epoch();
        let after = |at: &Pos| e.is_none_or(|e| *at > e.at);
        e.is_some_and(|e| e.to.iter().any(|u| self.out(u)))
            || self.bans.iter().any(|(at, u)| after(at) && banned(u))
    }

    /// What a ban or rekey must outdate: the epoch and standing bans.
    pub fn horizon(&self) -> u64 {
        let banned = |u: &Id| self.role(u) == Role::Banned;
        let bans = self.bans.iter().filter(|(_, u)| banned(u));
        let e = self.epoch().map(|e| e.at.0);
        let burns = self.burned.values().map(|at| at.0);
        bans.map(|(at, _)| at.0).chain(e).chain(burns).max().unwrap_or(0)
    }

    pub fn hosts(&self) -> Vec<EndpointAddr> {
        self.hosts.values().flatten().cloned().collect()
    }

    /// Hosts, with what keeps a small file there.
    pub fn holds(&self) -> Vec<(EndpointAddr, [u8; 16])> {
        let hold = |(id, a): (&EndpointId, &Option<EndpointAddr>)| {
            Some((a.clone()?, *self.holds.get(id)?))
        };
        self.hosts.iter().filter_map(hold).collect()
    }

    pub fn hold(&self, node: &EndpointId) -> Option<[u8; 16]> {
        self.holds.get(node).copied()
    }

    pub fn spaces(&self) -> &BTreeMap<Id, Option<Invite>> {
        &self.spaces
    }

    pub fn storage(&self) -> Vec<(EndpointAddr, [u8; 16])> {
        self.storage.values().flatten().cloned().collect()
    }

    pub fn keeps(&self, node: &EndpointId, space: &Id) -> bool {
        self.stored.get(&(*node, *space)).copied().unwrap_or(false)
    }

    pub fn owner_of(&self, d: &EndpointId) -> Option<Id> {
        let mine =
            self.devices.iter().filter(|((x, a), _)| x == d && !self.out(a));
        mine.max_by_key(|(_, t)| **t).map(|((_, a), _)| *a)
    }

    /// `user`'s devices, and when each last wrote (never ahead of now).
    pub fn devices_of(&self, user: &Id) -> Vec<(EndpointId, u64)> {
        let mine = self.devices.iter().filter(|((_, a), _)| a == user);
        let mut v: Vec<_> = mine.map(|((d, _), t)| (*d, *t)).collect();
        v.sort_by_key(|x| std::cmp::Reverse(x.1));
        v
    }

    pub fn devices(&self, n: usize) -> Vec<EndpointId> {
        let ok = |((_, a), _): &(&(EndpointId, Id), _)| !self.out(a);
        let v = self.devices.iter().filter(ok).map(|((d, _), t)| (*t, *d));
        let mut v: Vec<_> = v.collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        let mut seen = HashSet::new();
        v.retain(|(_, d)| seen.insert(*d));
        v.into_iter().take(n).map(|(_, d)| d).collect()
    }

    pub fn messages(
        &self,
        ch: &Ch,
        before: Option<Pos>,
        n: usize,
    ) -> Vec<Message> {
        let Some(posts) = self.posts.get(ch) else { return vec![] };
        let range = posts.range(..before.unwrap_or((u64::MAX, Id([255; 32]))));
        let mut v: Vec<_> =
            range.rev().filter_map(|(k, p)| self.view(k, p)).take(n).collect();
        v.reverse();
        v
    }

    pub fn search(
        &self,
        q: &str,
        room: Option<&Ch>,
        n: usize,
    ) -> Vec<(Ch, Message)> {
        let q = q.to_lowercase();
        let words: Vec<_> = q.split_whitespace().collect();
        let here = |c: &Ch| {
            let thread = self.thread(c);
            let there = self.channels.contains_key(c) || thread.is_some();
            there
                && room.is_none_or(|r| {
                    c == r || thread.is_some_and(|t| t.parent == *r)
                })
        };
        let posts = self.posts.iter().filter(|(c, _)| here(c));
        let mut hits: Vec<_> = posts
            .flat_map(|(c, ps)| ps.iter().map(move |(k, p)| (*c, k, p)))
            .filter(|(_, k, p)| {
                let Some((t, _)) = self.resolve(&k.1, p) else { return false };
                let names = p.files.iter().map(|f| f.name.as_str());
                let all = [t].into_iter().chain(names);
                let all = all.collect::<Vec<_>>().join(" ").to_lowercase();
                !words.is_empty() && words.iter().all(|w| all.contains(w))
            })
            .collect();
        hits.sort_by(|a, b| b.1.cmp(a.1));
        let view = |(c, k, p): (Ch, &Pos, &Post)| Some((c, self.view(k, p)?));
        hits.into_iter().filter_map(view).take(n).collect()
    }

    fn resolve<'a>(&'a self, id: &Id, p: &'a Post) -> Option<(&'a str, bool)> {
        if self.role(&p.author) == Role::Banned {
            return None;
        }
        let mut edits =
            self.edits.get(id).into_iter().flat_map(|e| e.iter().rev());
        let ok = |(at, (by, t)): &(&Pos, &(Id, Option<String>))| {
            !self.stale_at(at, by)
                && (*by == p.author
                    || t.is_none() && self.role(by) == Role::Admin)
        };
        match edits.find(ok).map(|e| e.1) {
            Some((_, None)) => None,
            Some((_, Some(t))) => Some((t, true)),
            None => Some((&p.text, false)),
        }
    }

    pub fn message(&self, id: &Id) -> Option<(Ch, Message)> {
        let (ch, at) = self.index.get(id)?;
        Some((*ch, self.view(at, self.posts.get(ch)?.get(at)?)?))
    }

    fn post(&self, id: &Id) -> Option<&Post> {
        let (ch, at) = self.index.get(id)?;
        self.posts.get(ch)?.get(at)
    }

    fn view(&self, &(ts, id): &Pos, p: &Post) -> Option<Message> {
        if self.stale_at(&(ts, id), &p.author) {
            return None;
        }
        let (text, edited) = self.resolve(&id, p)?;
        let quote = p.reply.and_then(|r| {
            let q = self.post(&r)?;
            let (text, _) = self.resolve(&r, q)?;
            Some(Quote {
                author: q.author,
                name: self.display_name(&q.author),
                text: text.into(),
            })
        });
        let mut reactions: BTreeMap<String, Vec<Id>> = BTreeMap::new();
        for ((u, emoji), (at, on)) in self.reacts.get(&id).into_iter().flatten()
        {
            if *on && self.role(u) != Role::Banned && !self.stale_at(at, u) {
                reactions.entry(emoji.clone()).or_default().push(*u);
            }
        }
        reactions.values_mut().for_each(|v| v.sort());
        let (author, name) = (p.author, self.display_name(&p.author));
        let reactions = reactions.into_iter().collect();
        let (reply, files) = (p.reply, p.files.clone());
        let pinned = self.pins.get(&id).is_some_and(|(at, by, on)| {
            *on && self.role(by) != Role::Banned && !self.stale_at(at, by)
        });
        // Only a thread with replies costs its count.
        let tc = id.thread();
        let t = self.posts.get(&tc).and_then(|_| self.thread(&tc));
        let thread = t.map_or(0, |t| t.count);
        Some(Message {
            id,
            author,
            name,
            ts,
            text: text.into(),
            edited,
            reply,
            quote,
            reactions,
            files,
            pinned,
            thread,
        })
    }

    pub fn latest(&self, ch: &Ch) -> Option<u64> {
        Some(self.posts.get(ch)?.last_key_value()?.0.0)
    }

    /// (posts by others, mentions of `me`) after `since`, up to 100.
    pub fn unread(&self, ch: &Ch, since: u64, me: &Id) -> (usize, usize) {
        let tag = self.tag(me);
        self.after(ch, since, me).take(100).fold((0, 0), |(n, m), (_, p)| {
            (n + 1, m + self.mentions(p, me, &tag) as usize)
        })
    }

    pub fn last_from(&self, ch: &Ch, since: u64, me: &Id) -> Option<Message> {
        self.after(ch, since, me).find_map(|(k, p)| self.view(k, p))
    }

    pub fn last_mention(
        &self,
        ch: &Ch,
        since: u64,
        me: &Id,
    ) -> Option<Message> {
        let tag = self.tag(me);
        let mut hits = self
            .after(ch, since, me)
            .filter(|(_, p)| self.mentions(p, me, &tag));
        hits.find_map(|(k, p)| self.view(k, p))
    }

    fn after(
        &self,
        ch: &Ch,
        since: u64,
        me: &Id,
    ) -> impl Iterator<Item = (&Pos, &Post)> {
        let posts = self.posts.get(ch).into_iter();
        posts
            .flat_map(move |p| {
                p.range((since.saturating_add(1), Id::default())..).rev()
            })
            .filter(|(at, p)| {
                p.author != *me
                    && self.role(&p.author) != Role::Banned
                    && !self.stale_at(at, &p.author)
            })
    }

    fn tag(&self, me: &Id) -> String {
        format!("@{}", self.display_name(me).to_lowercase())
    }

    fn mentions(&self, p: &Post, me: &Id, tag: &str) -> bool {
        let reply = p.reply.and_then(|r| self.post(&r));
        let to_me = self.dm.is_some() || reply.is_some_and(|q| q.author == *me);
        p.author != *me && (to_me || p.text.to_lowercase().contains(tag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    const CH: Ch = Ch([9; 8]);

    fn people() -> [Id; 3] {
        [1, 2, 3].map(|n| Id([n; 32]))
    }

    fn op(n: u8, author: Id, op: Op) -> Signed {
        let device = SecretKey::from_bytes(&[n; 32]).public();
        Signed {
            id: Id([n; 32]),
            author,
            device,
            ts: n as u64,
            op,
            epoch: [0; 8],
        }
    }

    fn post(text: &str, reply: Option<u8>) -> Op {
        let reply = reply.map(|n| Id([n; 32]));
        Op::Post { channel: CH, text: text.into(), reply, files: vec![] }
    }

    fn history() -> Vec<Signed> {
        let [o, a, b] = people();
        let role = |user, role| Op::Role { user, role };
        let edit =
            |t: &str| Op::Edit { target: Id([4; 32]), text: Some(t.into()) };
        let react =
            |on| Op::React { target: Id([4; 32]), emoji: "+".into(), on };
        let name = |n: &str| Op::Profile { name: n.into(), kem: vec![1] };
        vec![
            op(1, o, Op::Channel { id: CH, name: Some("general".into()) }),
            op(2, o, Op::Rename { name: "home".into() }),
            op(3, a, name("alice")),
            op(4, a, post("hi", None)),
            op(5, b, post("spam", None)),
            op(6, a, edit("hello")),
            op(7, b, edit("pwned")),
            op(8, b, Op::Rename { name: "bob's".into() }),
            op(9, o, role(a, Role::Admin)),
            op(10, a, role(b, Role::Banned)),
            op(11, a, role(o, Role::Banned)),
            op(12, o, post("@Alice welcome", Some(4))),
            op(13, o, react(true)),
            op(14, a, react(true)),
            op(15, b, react(true)),
            op(16, o, react(false)),
            op(17, b, name("bob")),
            op(18, a, Op::Rekey { epoch: [7; 8], boxes: vec![(o, vec![])] }),
            op(19, b, Op::Rekey { epoch: [8; 8], boxes: vec![] }),
        ]
    }

    #[test]
    fn fold_is_order_free_and_checks_authority() {
        let [o, a, b] = people();
        let inv = Invite { owner: o, ..Invite::new(o, "hint", None) };
        let (mut fwd, mut rev) = (State::new(&inv), State::new(&inv));
        for s in history() {
            assert!(fwd.extend([s.clone()]));
            assert!(!fwd.extend([s]));
        }
        rev.extend(history().into_iter().rev());
        for st in [&fwd, &rev] {
            assert_eq!(st.name(), "home");
            assert_eq!(st.channels(), vec![(CH, "general".into())]);
            let roles = [o, a, b].map(|u| st.role(&u));
            assert_eq!(roles, [Role::Admin, Role::Admin, Role::Banned]);
            let m = st.messages(&CH, None, 10);
            assert_eq!(m.len(), 2);
            assert_eq!(
                (&*m[0].name, &*m[0].text, m[0].edited),
                ("alice", "hello", true)
            );
            assert_eq!(m[0].reactions, vec![("+".into(), vec![a])]);
            assert_eq!(m[1].quote.as_ref().unwrap().text, "hello");
            assert_eq!(st.unread(&CH, 4, &a), (1, 1));
            assert_eq!(st.last_mention(&CH, 4, &a).unwrap().text, m[1].text);
            assert_eq!(st.last_mention(&CH, 4, &o), None);
            assert_eq!(st.epochs().len(), 1, "a banned member cannot rekey");
            assert!(st.kem(&b).is_none() && st.display_name(&b) != "bob");
            assert!(!st.leaks());
        }
    }

    #[test]
    fn deletes_hide_and_pages_have_no_gaps() {
        let [o, a, _] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        st.extend((1..=25).map(|n| op(n, o, post(&n.to_string(), None))));
        let (mut seen, mut before) = (vec![], None);
        loop {
            let page = st.messages(&CH, before, 10);
            let Some(first) = page.first() else { break };
            before = Some((first.ts, first.id));
            seen.splice(0..0, page.iter().map(|m| m.ts));
        }
        assert_eq!(seen, (1..=25).collect::<Vec<u64>>());
        let del = |n| Op::Edit { target: Id([n; 32]), text: None };
        st.extend([op(30, a, del(25)), op(31, o, post("re", Some(24)))]);
        assert_eq!(st.messages(&CH, None, 1)[0].text, "re");
        st.extend([op(32, o, del(24))]);
        let m = st.messages(&CH, None, 2);
        assert_eq!((&*m[0].text, m[1].quote.is_none()), ("25", true));
    }

    #[test]
    fn threads_hang_off_messages_in_channels() {
        let [o, a, _] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let root = Id([2; 32]);
        let reply = |channel, text: &str| Op::Post {
            channel,
            text: text.into(),
            reply: None,
            files: vec![],
        };
        // Replies may arrive before their message, or the channel.
        st.extend([
            op(3, a, reply(root.thread(), "first")),
            op(4, o, reply(root.thread(), "@alice second")),
            op(5, a, reply(Id([3; 32]).thread(), "a thread in a thread")),
            op(7, a, Op::Profile { name: "alice".into(), kem: vec![] }),
        ]);
        assert!(st.threads().is_empty());
        st.extend([
            op(1, o, Op::Channel { id: CH, name: Some("general".into()) }),
            op(2, o, post("\nplans for\nthe weekend", None)),
        ]);
        let t = st.thread(&root.thread()).unwrap();
        assert_eq!((t.root, t.parent, &*t.name), (root, CH, "plans for"));
        assert_eq!((t.count, t.last), (2, 4));
        assert_eq!(st.threads(), vec![t.clone()]);
        assert_eq!(st.messages(&CH, None, 9)[0].thread, 2);
        assert_eq!(st.messages(&t.id, None, 9).len(), 2);
        assert_eq!(st.unread(&t.id, 0, &a), (1, 1));
        assert_eq!(st.rooms().last(), Some(&(t.id, t.name)));
        st.extend([op(6, o, Op::Channel { id: CH, name: None })]);
        assert!(st.threads().is_empty() && st.thread(&root.thread()).is_none());
    }

    #[test]
    fn an_admin_orders_channels_and_anyone_pins() {
        let [o, a, _] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let (x, y) = (Ch([1; 8]), Ch([2; 8]));
        let names = |st: &State| {
            st.channels().into_iter().map(|c| c.1).collect::<Vec<_>>()
        };
        st.extend([
            op(1, o, Op::Channel { id: CH, name: Some("general".into()) }),
            op(2, o, Op::Channel { id: x, name: Some("b".into()) }),
            op(3, o, Op::Channel { id: y, name: Some("a".into()) }),
        ]);
        assert_eq!(names(&st), ["a", "b", "general"], "by name");
        st.extend([op(4, o, Op::Order { channels: vec![CH, x] })]);
        assert_eq!(names(&st), ["general", "b", "a"], "theirs, then the rest");
        st.extend([op(5, a, Op::Order { channels: vec![y] })]);
        assert_eq!(names(&st), ["general", "b", "a"], "not a member's");

        st.extend([op(6, a, post("hi", None))]);
        let pinned = |st: &State| st.pins(&CH).len();
        st.extend([op(7, a, Op::Pin { target: Id([6; 32]), on: true })]);
        assert_eq!(pinned(&st), 1);
        assert!(st.message(&Id([6; 32])).unwrap().1.pinned);
        st.extend([op(8, o, Op::Pin { target: Id([6; 32]), on: false })]);
        assert_eq!(pinned(&st), 0, "newest wins");
    }

    #[test]
    fn threads_are_named_by_their_starter_or_an_admin() {
        let [o, a, b] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let topic = |name: &str| Op::Topic {
            thread: Id([2; 32]).thread(),
            name: name.into(),
        };
        st.extend([
            op(1, o, Op::Channel { id: CH, name: Some("general".into()) }),
            op(2, a, post("a room", None)),
        ]);
        assert!(st.threads().is_empty());
        let name = |st: &State| st.threads().first().map(|t| t.name.clone());
        st.extend([op(3, a, topic("plans"))]);
        assert_eq!(name(&st).as_deref(), Some("plans"));
        st.extend([op(4, b, topic("mine"))]);
        assert_eq!(name(&st).as_deref(), Some("plans"));
        st.extend([op(5, o, topic("ours"))]);
        assert_eq!(name(&st).as_deref(), Some("ours"));
        assert_eq!(st.threads()[0].by, a);
        // An empty name deletes: the starter's while alone, an admin's always.
        let t = Id([2; 32]).thread();
        let say = |n, who, text: &str| {
            op(
                n,
                who,
                Op::Post {
                    channel: t,
                    text: text.into(),
                    reply: None,
                    files: vec![],
                },
            )
        };
        st.extend([say(6, a, "mine"), op(7, a, topic(""))]);
        assert!(
            st.threads().is_empty() && st.search("mine", None, 9).is_empty()
        );
        st.extend([say(8, b, "and mine"), op(9, a, topic(""))]);
        assert_eq!(st.threads().len(), 1, "b's reply stands");
        st.extend([op(10, o, topic(""))]);
        assert!(st.threads().is_empty() && st.rooms().iter().all(|r| r.0 != t));
        st.extend([op(11, o, topic("back"))]);
        assert_eq!(name(&st).as_deref(), Some("back"));
    }

    #[test]
    fn search_finds_words_in_a_room_or_anywhere() {
        let [o, a, _] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let other = Ch([9; 8]);
        let say = |channel, text: &str| Op::Post {
            channel,
            text: text.into(),
            reply: None,
            files: vec![],
        };
        st.extend([
            op(1, o, Op::Channel { id: CH, name: Some("general".into()) }),
            op(2, o, Op::Channel { id: other, name: Some("off".into()) }),
            op(3, a, say(CH, "Lunch at noon?")),
            op(4, o, say(Id([3; 32]).thread(), "noon works, lunch it is")),
            op(5, a, say(other, "lunch again")),
            op(
                6,
                a,
                Op::Edit { target: Id([5; 32]), text: Some("dinner".into()) },
            ),
        ]);
        let ids = |v: Vec<(Ch, Message)>| {
            v.iter().map(|m| m.1.id.0[0]).collect::<Vec<_>>()
        };
        assert_eq!(ids(st.search("LUNCH noon", None, 9)), [4, 3]);
        assert_eq!(ids(st.search("lunch", Some(&CH), 9)), [4, 3]);
        assert_eq!(
            ids(st.search("lunch", Some(&Id([3; 32]).thread()), 9)),
            [4]
        );
        assert_eq!(ids(st.search("dinner", None, 9)), [5]);
        assert!(st.search("  ", None, 9).is_empty());
    }

    #[test]
    fn a_replaced_key_writes_only_for_who_got_the_new_one() {
        let [o, a, x] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let sealed = |n, author, epoch, o| Signed { epoch, ..op(n, author, o) };
        let rekey = |t, to: &[Id]| Op::Rekey {
            epoch: [t; 8],
            boxes: to.iter().map(|u| (*u, vec![])).collect(),
        };
        let name = |n: &str| Op::Profile { name: n.into(), kem: vec![1] };
        let base = vec![
            sealed(1, o, [0; 8], rekey(0, &[])),
            sealed(
                2,
                o,
                [0; 8],
                Op::Channel { id: CH, name: Some("g".into()) },
            ),
            sealed(3, a, [0; 8], name("a")),
            sealed(5, o, [0; 8], rekey(1, &[o, a])), // invites reset
            sealed(6, x, [0; 8], name("x")),         // an old invite, after
            sealed(7, x, [0; 8], post("x was here", None)),
            sealed(8, a, [0; 8], post("a, offline", None)),
            sealed(9, o, [1; 8], post("o", None)),
        ];
        st.extend(base.clone());
        assert!(st.kem(&a).is_some() && st.kem(&x).is_none());
        let m: Vec<_> =
            st.messages(&CH, None, 9).into_iter().map(|m| m.text).collect();
        assert_eq!(m, ["a, offline", "o"]);
        assert!(st.stale(&[0; 8], &(10, Id([10; 32])), &x));
        assert!(!st.stale(&[1; 8], &(10, Id([10; 32])), &x));
        // Re-sealed under the old key by someone else: shown either way.
        let n = Id([9; 32]);
        let (np, nk) = (sealed(20, n, [1; 8], post("n", None)), name("n"));
        let older = |s: &Signed| Signed { epoch: [0; 8], ..s.clone() };
        for (one, two) in [(np.clone(), older(&np)), (older(&np), np.clone())] {
            let mut st2 = State::new(&Invite::new(o, "", None));
            st2.extend(base.clone());
            let nn = sealed(19, n, [1; 8], nk.clone());
            st2.extend([one, two, nn.clone(), older(&nn)]);
            let m = st2.messages(&CH, None, 9);
            assert!(m.iter().any(|m| m.text == "n") && st2.kem(&n).is_some());
        }
        st.extend([sealed(4, x, [0; 8], name("a"))]);
        assert!(st.kem(&x).is_none(), "backdated, still left out");
    }

    #[test]
    fn hosts_are_admins_and_devices_stay_their_owners() {
        let [o, a, b] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let host = |n: u8| Op::Host {
            node: iroh::EndpointAddr::new(
                SecretKey::from_bytes(&[n; 32]).public(),
            ),
            on: true,
        };
        let theirs = Signed { ts: 9, ..op(4, b, post("mine now", None)) };
        let theirs = Signed { device: op(3, a, host(0)).device, ..theirs };
        st.extend([
            op(1, o, Op::Channel { id: CH, name: Some("g".into()) }),
            op(2, a, host(20)),
            op(3, a, post("hi", None)),
            op(5, o, host(21)),
            theirs,
            op(6, o, Op::Role { user: b, role: Role::Banned }),
        ]);
        let hosts: Vec<_> = st.hosts().into_iter().map(|h| h.id).collect();
        assert_eq!(hosts, [SecretKey::from_bytes(&[21; 32]).public()]);
        assert!(st.devices(9).contains(&op(3, a, host(0)).device));
    }

    #[test]
    fn epochs_count_while_their_setter_is_an_admin() {
        let [o, a, b] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let rekey = |t, to: &[Id]| Op::Rekey {
            epoch: [t; 8],
            boxes: to.iter().map(|u| (*u, vec![])).collect(),
        };
        let role = |user, role| Op::Role { user, role };
        st.extend([
            op(1, o, rekey(1, &[])),
            op(2, o, role(a, Role::Admin)),
            op(3, a, rekey(2, &[a])),
            op(4, o, role(a, Role::Member)),
        ]);
        assert_eq!(st.epochs().len(), 2);
        assert_eq!(st.epoch().unwrap().tag, [1; 8], "a, demoted, is out");
        // b, banned, got a far-ahead epoch: only a later rekey ends the leak.
        st.extend([op(5, o, role(a, Role::Admin)), op(200, a, rekey(3, &[b]))]);
        st.extend([op(6, o, role(b, Role::Banned))]);
        assert!(st.leaks() && st.horizon() == 200);
        st.extend([op(201, o, rekey(4, &[a]))]);
        assert!(!st.leaks() && st.epoch().unwrap().tag == [4; 8]);
        st.extend([op(240, o, role(a, Role::Banned))]);
        assert!(st.leaks() && st.horizon() == 240);
        st.extend([op(241, o, rekey(5, &[]))]);
        assert!(!st.leaks());
    }

    #[test]
    fn owners_hand_on_and_the_lost_are_retired() {
        let [o, a, b] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let rekey = |t, to: &[Id]| Op::Rekey {
            epoch: [t; 8],
            boxes: to.iter().map(|u| (*u, vec![])).collect(),
        };
        let name = |n: &str| Op::Profile { name: n.into(), kem: vec![1] };
        st.extend([
            op(1, o, Op::Channel { id: CH, name: Some("g".into()) }),
            op(2, a, name("a")),
            op(3, b, name("b")),
            op(4, o, rekey(1, &[o, a, b])),
            op(5, o, Op::Owner { user: a }),
            op(6, o, Op::Role { user: b, role: Role::Admin }),
        ]);
        assert_eq!(st.head(), a);
        assert_eq!(st.role(&o), Role::Admin, "the old owner stays an admin");
        assert_eq!(st.role(&b), Role::Member, "only the owner makes admins");
        st.extend([op(7, a, Op::Role { user: b, role: Role::Admin })]);
        assert_eq!(st.role(&b), Role::Admin, "and the new one may");
        // b's phone is stolen: b retires the identity; the thief writes on.
        st.extend([
            op(8, b, post("before", None)),
            op(9, b, Op::Burn),
            op(10, b, post("thief", None)),
            op(11, b, name("not b")),
        ]);
        let texts: Vec<_> =
            st.messages(&CH, None, 9).into_iter().map(|m| m.text).collect();
        assert_eq!(texts, ["before"]);
        assert!(st.kem(&b).is_none() && st.display_name(&b) == "b");
        assert!(st.leaks(), "b holds the key: time to rekey");
        assert!(st.horizon() >= 9);
        st.extend([op(12, a, rekey(2, &[o, a]))]);
        assert!(!st.leaks());
    }

    #[test]
    fn reactions_stop_at_twenty_kinds_a_person() {
        let [o, a, _] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        st.extend([
            op(1, o, Op::Channel { id: CH, name: Some("g".into()) }),
            op(2, o, post("hi", None)),
        ]);
        let react = |n: u8| Op::React {
            target: Id([2; 32]),
            emoji: n.to_string(),
            on: true,
        };
        st.extend((10..40).map(|n| op(n, a, react(n))));
        assert_eq!(st.messages(&CH, None, 1)[0].reactions.len(), 20);
    }

    #[test]
    fn a_thread_says_whether_its_starter_alone_replied() {
        let [o, a, _] = people();
        let mut st = State::new(&Invite::new(o, "", None));
        let t = Id([2; 32]).thread();
        let say = |ch, text: &str| Op::Post {
            channel: ch,
            text: text.into(),
            reply: None,
            files: vec![],
        };
        st.extend([
            op(1, o, Op::Channel { id: CH, name: Some("g".into()) }),
            op(2, a, say(CH, "root")),
            op(3, a, say(t, "mine")),
        ]);
        assert!(st.thread(&t).unwrap().alone);
        st.extend([op(4, o, say(t, "theirs"))]);
        assert!(!st.thread(&t).unwrap().alone);
    }

    #[test]
    fn every_dm_message_is_a_mention() {
        let [o, a, _] = people();
        let mut st = State::new(&Invite::new(o, "", Some(a)));
        st.extend([op(1, o, post("hey", None))]);
        assert_eq!(st.unread(&CH, 0, &a), (1, 1));
    }
}
