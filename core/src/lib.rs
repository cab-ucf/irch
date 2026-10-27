//! irch: a space is a grow-only set of sealed ops on members' devices,
//! folded deterministically, reconciled over iroh.

pub mod api;
mod call;
pub mod crypto;
mod node;
pub mod notes;
pub mod op;
pub mod release;
pub mod state;
mod store;
mod sync;
pub mod wake;

pub use node::{Config, Event, Level, Net, Node, Notice, http};
pub use op::{File, Invite, Op, Role};
pub use state::{Member, Message, State, Thread};

use data_encoding::HEXLOWER_PERMISSIVE as HEX;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{fmt, str::FromStr};

macro_rules! bytes {
    ($($t:ident $n:literal),*) => {$(
        /// Raw bytes on the wire, hex in JSON.
        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
        pub struct $t(pub [u8; $n]);
        impl fmt::Display for $t {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(&HEX.encode(&self.0))
            }
        }
        impl fmt::Debug for $t {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str(&HEX.encode(&self.0[..4]))
            }
        }
        impl FromStr for $t {
            type Err = anyhow::Error;
            fn from_str(s: &str) -> anyhow::Result<Self> {
                let v = HEX.decode(s.trim().as_bytes())?;
                Ok(Self(v.try_into().map_err(|_| anyhow::anyhow!("bad id"))?))
            }
        }
        impl Serialize for $t {
            fn serialize<S: Serializer>(&self, s: S)
            -> Result<S::Ok, S::Error> {
                if s.is_human_readable() {
                    s.collect_str(self)
                } else {
                    self.0.serialize(s)
                }
            }
        }
        impl<'de> Deserialize<'de> for $t {
            fn deserialize<D: Deserializer<'de>>(d: D)
            -> Result<Self, D::Error> {
                if d.is_human_readable() {
                    let s = String::deserialize(d)?;
                    s.parse().map_err(de::Error::custom)
                } else {
                    <[u8; $n]>::deserialize(d).map(Self)
                }
            }
        }
    )*};
}
bytes!(Id 32, Ch 8);

impl Id {
    /// Named by the id's prefix, so everyone starting it at once agrees.
    pub fn thread(&self) -> Ch {
        Ch(self.0[..8].try_into().expect("8 of 32"))
    }
}

/// This computer's storage node code, from `irch --install` or nixos.nix.
pub fn local_storage() -> Option<String> {
    // /run is root's on Linux; elsewhere (C:\\run) anyone could plant one.
    let run = cfg!(target_os = "linux").then(|| "/run/irch/code".into());
    let p = std::env::var_os("IRCH_CODE_FILE").map(Into::into).or(run);
    let mine = dirs::data_dir().map(|d| d.join("irch-serve").join("code"));
    let read = |p| std::fs::read_to_string(p).ok();
    let code = p.and_then(read).or_else(|| mine.and_then(read))?;
    let code = code.trim().to_string();
    (!code.is_empty()).then_some(code)
}

/// Its own folder, or `root` itself for the first (from before profiles).
pub fn profile(root: &std::path::Path) -> std::path::PathBuf {
    let n = std::fs::read_to_string(root.join("profile")).unwrap_or_default();
    match n.trim() {
        "" => root.into(),
        n => root.join("profiles").join(n),
    }
}

/// What signing out takes away.
pub const MINE: [&str; 11] = [
    "seed",
    "device",
    "local",
    "prefs",
    "closed",
    "spaces",
    "files",
    "blobs",
    "offered",
    "wake.json",
    "irch.log",
];

/// Unix time in milliseconds.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}
