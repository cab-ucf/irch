//! Ops: small signed facts, sealed under a space's epoch key. On the wire
//! and on disk an op is an [`Env`]: epoch tag, time, and ciphertext that
//! hides author, device, signature and content.

use crate::crypto::{self, Key, kdf};
use crate::{Ch, Id};
use anyhow::{Context, Result, anyhow};
use data_encoding::BASE32_NOPAD_NOCASE as B32;
use iroh::{EndpointAddr, EndpointId, PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// The largest envelope: a rekey of a few thousand members.
pub const MAX_ENV: usize = 4 << 20;

/// Append variants only: postcard tags are positions.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Op {
    /// Your name here, and the key that rekeys reach you with.
    Profile {
        name: String,
        kem: Vec<u8>,
    },
    Rename {
        name: String,
    },
    /// Create, rename, or (`None`) delete a channel.
    Channel {
        id: Ch,
        name: Option<String>,
    },
    Role {
        user: Id,
        role: Role,
    },
    Post {
        channel: Ch,
        text: String,
        reply: Option<Id>,
    },
    /// Edit, or (`None`) delete.
    Edit {
        target: Id,
        text: Option<String>,
    },
    React {
        target: Id,
        emoji: String,
        on: bool,
    },
    /// A new epoch key, boxed to every member left.
    Rekey {
        epoch: [u8; 8],
        boxes: Vec<(Id, Vec<u8>)>,
    },
    /// Sealed under a new epoch: the key it replaced, so history stays.
    Back {
        key: Key,
    },
    /// An always-on peer that keeps this space's ciphertext.
    Host {
        node: EndpointAddr,
        on: bool,
    },
    /// A direct-message invite, readable only by its recipient.
    Knock {
        sealed: Vec<u8>,
    },
    /// Your own space: what you joined and left, on every device.
    Joined {
        invite: Invite,
    },
    Left {
        space: Id,
    },
    Storage {
        addr: EndpointAddr,
        token: [u8; 16],
        on: bool,
    },
    Typing {
        channel: Ch,
    },
    Here,
}

impl Op {
    /// Gossip only: never stored, synced or folded.
    pub fn live(&self) -> bool {
        matches!(self, Op::Typing { .. } | Op::Here)
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub enum Role {
    #[default]
    Member,
    Admin,
    Banned,
}

/// Everything needed to enter a space: shared out of band.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Invite {
    pub owner: Id,
    pub nonce: [u8; 16],
    pub key: Key,
    pub name: String,
    /// A direct-message space: who it is with.
    pub dm: Option<Id>,
    pub peers: Vec<EndpointAddr>,
}

impl Invite {
    pub fn new(owner: Id, name: &str, dm: Option<Id>) -> Self {
        let (nonce, key, peers) = (crypto::rand(), crypto::rand(), vec![]);
        Self { owner, nonce, key, name: name.into(), dm, peers }
    }

    /// Bound to the owner: a forged owner is another, empty space.
    pub fn id(&self) -> Id {
        Id(kdf("irch space", &[&self.owner.0, &self.nonce]))
    }

    pub fn encode(&self) -> String {
        code("join", self)
    }

    pub fn decode(s: &str) -> Result<Self> {
        uncode("join", s)
    }
}

pub fn code<T: Serialize>(kind: &str, v: &T) -> String {
    let b = postcard::to_allocvec(v).expect("encodes");
    format!("irch:{kind}:{}", B32.encode(&b).to_lowercase())
}

pub fn uncode<T: DeserializeOwned>(kind: &str, s: &str) -> Result<T> {
    let body = s.trim().strip_prefix(&format!("irch:{kind}:"));
    let body = body.ok_or_else(|| anyhow!("not an irch {kind} code"))?;
    let b = B32.decode(body.as_bytes()).context("damaged code")?;
    postcard::from_bytes(&b).context("damaged code")
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Env {
    pub epoch: [u8; 8],
    pub ts: u64,
    pub live: bool,
    pub ct: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct Inner {
    author: Id,
    device: EndpointId,
    sig: Signature,
    op: Op,
}

/// An opened, verified op.
#[derive(Clone, Debug, PartialEq)]
pub struct Signed {
    pub id: Id,
    pub author: Id,
    pub device: EndpointId,
    pub ts: u64,
    pub op: Op,
}

pub fn tag(key: &Key) -> [u8; 8] {
    kdf("irch epoch", &[key])[..8].try_into().expect("8 bytes")
}

fn tbs(space: &Id, ts: u64, device: &EndpointId, op: &Op) -> Vec<u8> {
    postcard::to_allocvec(&("irch op", space, ts, device, op)).expect("op")
}

fn aad(space: &Id, e: &Env) -> Vec<u8> {
    postcard::to_allocvec(&(space, e.epoch, e.ts, e.live)).expect("aad")
}

fn op_id(author: &Id, tbs: &[u8]) -> Id {
    Id(blake3::hash(&[&author.0[..], tbs].concat()).into())
}

pub fn seal(
    space: &Id,
    key: &Key,
    user: &SecretKey,
    device: EndpointId,
    ts: u64,
    op: Op,
) -> (Signed, Vec<u8>) {
    let msg = tbs(space, ts, &device, &op);
    let author = Id(*user.public().as_bytes());
    let mut e = Env { epoch: tag(key), ts, live: op.live(), ct: vec![] };
    let inner = Inner { author, device, sig: user.sign(&msg), op };
    let pt = postcard::to_allocvec(&inner).expect("op");
    e.ct = crypto::seal(key, &aad(space, &e), &pt);
    let id = op_id(&author, &msg);
    let s = Signed { id, author, device, ts, op: inner.op };
    (s, postcard::to_allocvec(&e).expect("env"))
}

/// Decrypt and, unless re-reading what we already checked, verify.
pub fn open(space: &Id, key: &Key, e: &Env, verify: bool) -> Option<Signed> {
    let pt = crypto::open(key, &aad(space, e), &e.ct)?;
    let i: Inner = postcard::from_bytes(&pt).ok()?;
    let msg = tbs(space, e.ts, &i.device, &i.op);
    let pk = PublicKey::from_bytes(&i.author.0).ok()?;
    if i.op.live() != e.live || verify && pk.verify(&msg, &i.sig).is_err() {
        return None;
    }
    let (id, author, device, op) =
        (op_id(&i.author, &msg), i.author, i.device, i.op);
    Some(Signed { id, author, device, ts: e.ts, op })
}

#[test]
fn sealed() {
    let (sk, dev) = (SecretKey::generate(), SecretKey::generate().public());
    let inv = Invite::new(Id(*sk.public().as_bytes()), "x", None);
    assert_eq!(Invite::decode(&inv.encode()).unwrap(), inv);
    let (sp, key) = (inv.id(), inv.key);
    let op = Op::Post { channel: Ch([1; 8]), text: "hi".into(), reply: None };
    let (s, raw) = seal(&sp, &key, &sk, dev, 7, op);
    let e: Env = postcard::from_bytes(&raw).unwrap();
    assert_eq!(open(&sp, &key, &e, true), Some(s));
    assert!(open(&Id([9; 32]), &key, &e, true).is_none());
    assert!(open(&sp, &crypto::rand(), &e, true).is_none());
    assert!(open(&sp, &key, &Env { ts: 8, ..e }, true).is_none());
}
