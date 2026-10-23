//! A space's state: a deterministic fold over its set of ops, so every
//! replica holding the same set agrees, whatever the delivery order.
//! Control ops (names, channels, roles, epochs) are re-folded in `(ts, id)`
//! order, so authority is judged at each op's place in that order.

use crate::op::{Invite, Op, Role, Signed};
use crate::{Ch, Id};
use iroh::{EndpointAddr, EndpointId};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::mem::take;

/// Position of an op: author clock, then id. Also the paging cursor.
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
}

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Quote {
    pub author: Id,
    pub name: String,
    pub text: String,
}

/// Latest reaction write per (user, emoji): last writer wins.
type Reacts = HashMap<(Id, String), (Pos, bool)>;

struct Post {
    author: Id,
    text: String,
    reply: Option<Id>,
}

#[derive(Default)]
pub struct State {
    pub owner: Id,
    pub dm: Option<Id>,
    hint: String,
    control: BTreeMap<Pos, (Id, Op)>,
    posts: HashMap<Ch, BTreeMap<Pos, Post>>,
    index: HashMap<Id, (Ch, Pos)>,
    edits: HashMap<Id, BTreeMap<Pos, (Id, Option<String>)>>,
    reacts: HashMap<Id, Reacts>,
    authors: HashMap<Id, u64>,
    devices: HashMap<EndpointId, u64>,
    // Derived from `control` by `refold`:
    name: Option<String>,
    channels: BTreeMap<Ch, String>,
    profiles: HashMap<Id, (String, Vec<u8>)>,
    roles: HashMap<Id, Role>,
    epochs: Vec<([u8; 8], Vec<Id>)>,
    epoch_at: Option<Pos>,
    bans: Vec<(Pos, Id)>,
    hosts: BTreeMap<EndpointId, Option<EndpointAddr>>,
    spaces: BTreeMap<Id, Option<Invite>>,
    storage: BTreeMap<EndpointId, Option<(EndpointAddr, [u8; 16])>>,
}

impl State {
    pub fn new(inv: &Invite) -> Self {
        let (owner, dm, hint) = (inv.owner, inv.dm, inv.name.clone());
        let mut s = Self { owner, dm, hint, ..Default::default() };
        s.refold();
        s
    }

    /// Fold ops in, re-folding control ops once. True if anything changed.
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
        let t = self.authors.entry(s.author).or_default();
        *t = (*t).max(s.ts);
        let t = self.devices.entry(s.device).or_default();
        *t = (*t).max(s.ts);
        match s.op {
            Op::Post { channel, text, reply } => {
                self.index.insert(s.id, (channel, at));
                let p = Post { author: s.author, text, reply };
                let ch = self.posts.entry(channel).or_default();
                (ch.insert(at, p).is_none(), false)
            }
            Op::Edit { target, text } => {
                let e = self.edits.entry(target).or_default();
                (e.insert(at, (s.author, text)).is_none(), false)
            }
            Op::React { target, emoji, on } => {
                let r = self.reacts.entry(target).or_default();
                match r.get(&(s.author, emoji.clone())) {
                    Some((p, _)) if *p >= at => (false, false),
                    _ => (
                        r.insert((s.author, emoji), (at, on)) != Some((at, on)),
                        false,
                    ),
                }
            }
            Op::Back { .. }
            | Op::Knock { .. }
            | Op::Typing { .. }
            | Op::Here => (false, false),
            op => {
                let new = self.control.insert(at, (s.author, op)).is_none();
                (new, new)
            }
        }
    }

    fn refold(&mut self) {
        (self.name, self.channels) = (None, BTreeMap::new());
        (self.profiles, self.roles) = (HashMap::new(), HashMap::new());
        (self.epochs, self.hosts) = (vec![], BTreeMap::new());
        (self.epoch_at, self.bans) = (None, vec![]);
        (self.spaces, self.storage) = (BTreeMap::new(), BTreeMap::new());
        if self.dm.is_some() {
            self.channels.insert(DM, "dm".into());
        }
        let control = take(&mut self.control);
        for (at, (a, op)) in &control {
            let role = self.role(a);
            let (admin, own) = (role == Role::Admin, *a == self.owner);
            match op {
                _ if role == Role::Banned => {}
                Op::Profile { name, kem } => {
                    self.profiles.insert(*a, (name.clone(), kem.clone()));
                }
                Op::Rename { name } if admin => self.name = Some(name.clone()),
                Op::Channel { id, name } if admin && self.dm.is_none() => {
                    match name {
                        Some(n) => drop(self.channels.insert(*id, n.clone())),
                        None => drop(self.channels.remove(id)),
                    }
                }
                Op::Role { user, role: r } if admin && *user != self.owner => {
                    let admins =
                        *r == Role::Admin || self.role(user) == Role::Admin;
                    if own || !admins {
                        self.roles.insert(*user, *r);
                        if *r == Role::Banned {
                            self.bans.push((*at, *user));
                        }
                    }
                }
                Op::Rekey { epoch, boxes } if admin => {
                    let to = boxes.iter().map(|b| b.0).collect();
                    self.epochs.push((*epoch, to));
                    self.epoch_at = Some(*at);
                }
                Op::Host { node, on } => {
                    drop(self.hosts.insert(node.id, on.then(|| node.clone())))
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
                _ => {}
            }
        }
        self.control = control;
    }

    /// Who a direct-message space is with, seen by `me`.
    pub fn peer(&self, me: &Id) -> Option<Id> {
        self.dm.map(|d| if *me == self.owner { d } else { self.owner })
    }

    pub fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.hint)
    }

    pub fn role(&self, user: &Id) -> Role {
        match self.roles.get(user) {
            _ if *user == self.owner => Role::Admin,
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
        (!k.is_empty() && self.role(user) != Role::Banned).then_some(k)
    }

    pub fn channels(&self) -> Vec<(Ch, String)> {
        let mut v: Vec<_> =
            self.channels.iter().map(|(i, n)| (*i, n.clone())).collect();
        v.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
        v
    }

    /// Everyone with a profile or a role.
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

    /// Valid epochs, oldest first, with who received each.
    pub fn epochs(&self) -> &[([u8; 8], Vec<Id>)] {
        &self.epochs
    }

    /// Someone banned may hold the newest epoch's key: time to rekey.
    pub fn leaks(&self) -> bool {
        let banned = |u: &Id| self.role(u) == Role::Banned;
        let to = self.epochs.last().map(|e| &e.1[..]).unwrap_or(&[]);
        let since = |at: &Pos| self.epoch_at.is_none_or(|e| *at > e);
        to.iter().any(banned)
            || self.bans.iter().any(|(at, u)| since(at) && banned(u))
    }

    pub fn hosts(&self) -> Vec<EndpointAddr> {
        self.hosts.values().flatten().cloned().collect()
    }

    pub fn spaces(&self) -> &BTreeMap<Id, Option<Invite>> {
        &self.spaces
    }

    pub fn storage(&self) -> Vec<(EndpointAddr, [u8; 16])> {
        self.storage.values().flatten().cloned().collect()
    }

    /// Devices heard from, most recent first.
    pub fn devices(&self, n: usize) -> Vec<EndpointId> {
        let mut v: Vec<_> =
            self.devices.iter().map(|(d, t)| (*t, *d)).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.into_iter().take(n).map(|(_, d)| d).collect()
    }

    /// Up to `n` visible messages before `before` (or newest), oldest first.
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

    /// Text after edits by its author; `None` if deleted or hidden.
    fn resolve(&self, id: &Id, p: &Post) -> Option<(String, bool)> {
        if self.role(&p.author) == Role::Banned {
            return None;
        }
        let edits =
            self.edits.get(id).into_iter().flat_map(|e| e.values().rev());
        let ok = |(by, t): &&(Id, Option<String>)| {
            *by == p.author || t.is_none() && self.role(by) == Role::Admin
        };
        match edits.clone().find(ok) {
            Some((_, None)) => None,
            Some((_, Some(t))) => Some((t.clone(), true)),
            None => Some((p.text.clone(), false)),
        }
    }

    fn post(&self, id: &Id) -> Option<&Post> {
        let (ch, at) = self.index.get(id)?;
        self.posts.get(ch)?.get(at)
    }

    fn view(&self, &(ts, id): &Pos, p: &Post) -> Option<Message> {
        let (text, edited) = self.resolve(&id, p)?;
        let quote = p.reply.and_then(|r| {
            let q = self.post(&r)?;
            let (text, _) = self.resolve(&r, q)?;
            Some(Quote {
                author: q.author,
                name: self.display_name(&q.author),
                text,
            })
        });
        let mut reactions: BTreeMap<String, Vec<Id>> = BTreeMap::new();
        for ((u, emoji), (_, on)) in self.reacts.get(&id).into_iter().flatten()
        {
            if *on && self.role(u) != Role::Banned {
                reactions.entry(emoji.clone()).or_default().push(*u);
            }
        }
        reactions.values_mut().for_each(|v| v.sort());
        let (author, name) = (p.author, self.display_name(&p.author));
        let reactions = reactions.into_iter().collect();
        let reply = p.reply;
        Some(Message {
            id,
            author,
            name,
            ts,
            text,
            edited,
            reply,
            quote,
            reactions,
        })
    }

    pub fn latest(&self, ch: &Ch) -> Option<u64> {
        Some(self.posts.get(ch)?.last_key_value()?.0.0)
    }

    /// Posts by others after `since` (up to 100), and mentions of `me`.
    pub fn unread(&self, ch: &Ch, since: u64, me: &Id) -> (usize, usize) {
        let tag = self.tag(me);
        self.after(ch, since, me).take(100).fold((0, 0), |(n, m), (_, p)| {
            (n + 1, m + self.mentions(p, me, &tag) as usize)
        })
    }

    /// The newest mention of `me` after `since`: what a notification shows.
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
            .filter(|(_, p)| {
                p.author != *me && self.role(&p.author) != Role::Banned
            })
    }

    fn tag(&self, me: &Id) -> String {
        format!("@{}", self.display_name(me).to_lowercase())
    }

    fn mentions(&self, p: &Post, me: &Id, tag: &str) -> bool {
        let to_me = p
            .reply
            .and_then(|r| self.post(&r))
            .is_some_and(|q| q.author == *me);
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
        Signed { id: Id([n; 32]), author, device, ts: n as u64, op }
    }

    fn post(text: &str, reply: Option<u8>) -> Op {
        let reply = reply.map(|n| Id([n; 32]));
        Op::Post { channel: CH, text: text.into(), reply }
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
}
