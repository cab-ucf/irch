//! Signed ops, sealed under a space's epoch key. An [`Env`] shows only the
//! epoch tag and time; author, device and signature are ciphertext too.

use crate::crypto::{self, Key, kdf};
use crate::{Ch, Id};
use anyhow::{Context, Result, anyhow};
use data_encoding::BASE32_NOPAD_NOCASE as B32;
use iroh::{EndpointAddr, EndpointId, PublicKey, SecretKey, Signature};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// A rekey of a few thousand members.
pub const MAX_ENV: usize = 4 << 20;

/// Append variants only: postcard tags are positions.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub enum Op {
    // `kem`: the key rekeys reach you by.
    Profile { name: String, kem: Vec<u8> },
    Rename { name: String },
    Channel { id: Ch, name: Option<String> }, // `None` deletes
    Role { user: Id, role: Role },
    Post { channel: Ch, text: String, reply: Option<Id>, files: Vec<File> },
    Edit { target: Id, text: Option<String> }, // `None` deletes
    React { target: Id, emoji: String, on: bool },
    Rekey { epoch: [u8; 8], boxes: Vec<(Id, Vec<u8>)> },
    // Under a new epoch, the key it replaced: history stays readable.
    Back { key: Key },
    // An always-on peer keeping this space's ciphertext.
    Host { node: EndpointAddr, on: bool },
    // A direct-message invite only its recipient opens.
    Knock { sealed: Vec<u8> },
    // In your own space: what you joined and left.
    Joined { invite: Invite },
    Left { space: Id },
    Storage { addr: EndpointAddr, token: [u8; 16], on: bool },
    Typing { channel: Ch },
    Here,
    // `None`: in no call. `Gone`: invisible, or quit.
    Call { channel: Option<Ch> },
    Gone,
    // Where to wake the signing device (UnifiedPush).
    Push { url: Option<String> },
    // Own space: whether our storage node keeps a space (always its own).
    Store { node: EndpointId, space: Id, on: bool },
    Topic { thread: Ch, name: String },
    Order { channels: Vec<Ch> },
    Pin { target: Id, on: bool },
    // What any member hands a storage node listed by `Host` to keep the
    // space's small files: derived from its token, opening nothing else.
    Hold { node: EndpointId, token: [u8; 16] },
}

impl Op {
    /// Gossip only: never stored, synced or folded.
    pub fn live(&self) -> bool {
        matches!(
            self,
            Op::Typing { .. } | Op::Here | Op::Call { .. } | Op::Gone
        )
    }
}

/// Unseen, or turning text around: what lets one name pass for another.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

pub fn name(n: &str) -> String {
    let mut n: String = n.chars().filter(|c| !hidden(*c)).collect();
    while n.len() > 64 {
        n.pop();
    }
    n.trim().to_string()
}

/// Ciphertext stored apart by BLAKE3 hash; the key only in the sealed post.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct File {
    pub name: String,
    pub size: u64,
    pub hash: Id,
    pub key: Id,
}

/// An unsealed file's key: its sender's devices serve it.
pub const OFFERED: Id = Id([0; 32]);

impl File {
    pub fn offered(&self) -> bool {
        self.key == OFFERED
    }

    /// Safe to show and save: nothing hidden, nothing Windows reserves.
    pub fn safe(mut self) -> Self {
        let bad = |c: char| hidden(c) || "<>:\"/\\|?*".contains(c);
        let n: String = self.name.chars().filter(|c| !bad(*c)).collect();
        let n = n.trim_matches(|c| c == '.' || c == ' ');
        let stem = n.split('.').next().unwrap_or_default().to_uppercase();
        let dev = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
            || stem.len() == 4
                && ["COM", "LPT"].iter().any(|p| stem.starts_with(p))
                && stem.as_bytes()[3].is_ascii_digit();
        self.name = match n {
            "" => "file".into(),
            n if dev => format!("_{n}"),
            n => n.into(),
        };
        self
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub enum Role {
    #[default]
    Member,
    Admin,
    Banned,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Invite {
    pub owner: Id,
    pub nonce: [u8; 16],
    pub key: Key,
    pub name: String,
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

#[derive(Clone, Debug, PartialEq)]
pub struct Signed {
    pub id: Id,
    pub author: Id,
    pub device: EndpointId,
    pub ts: u64,
    pub op: Op,
    pub epoch: [u8; 8],
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
    let s = Signed { id, author, device, ts, op: inner.op, epoch: e.epoch };
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
    Some(Signed { id, author, device, ts: e.ts, op, epoch: e.epoch })
}

#[test]
fn sealed() {
    let (sk, dev) = (SecretKey::generate(), SecretKey::generate().public());
    let inv = Invite::new(Id(*sk.public().as_bytes()), "x", None);
    assert_eq!(Invite::decode(&inv.encode()).unwrap(), inv);
    let (sp, key) = (inv.id(), inv.key);
    let (channel, text) = (Ch([1; 8]), "hi".into());
    let op = Op::Post { channel, text, reply: None, files: vec![] };
    let (s, raw) = seal(&sp, &key, &sk, dev, 7, op);
    let e: Env = postcard::from_bytes(&raw).unwrap();
    assert_eq!(open(&sp, &key, &e, true), Some(s));
    assert!(open(&Id([9; 32]), &key, &e, true).is_none());
    assert!(open(&sp, &crypto::rand(), &e, true).is_none());
    assert!(open(&sp, &key, &Env { ts: 8, ..e }, true).is_none());
}

#[cfg(test)]
mod file_tests {
    use super::{File, Id};

    #[test]
    fn names_say_what_they_are() {
        let f = |n: &str| {
            let (hash, key) = (Id([0; 32]), Id([0; 32]));
            File { name: n.into(), size: 0, hash, key }.safe().name
        };
        assert_eq!(f("photo\u{202e}gnp.exe"), "photognp.exe");
        assert_eq!(f("../../.ssh/a:b|c\n"), "sshabc");
        assert_eq!(f("con.txt"), "_con.txt");
        assert_eq!(f("COM1"), "_COM1");
        assert_eq!(f(" . "), "file");
        assert_eq!(f("notes.md"), "notes.md");
        assert_eq!(super::name(" a\u{202e}dmin\u{200b} "), "admin");
        assert_eq!(super::name(&"é".repeat(40)).len(), 64);
    }
}
