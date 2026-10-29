//! Calls: a full mesh over iroh; the page does WebCodecs. Audio goes as
//! datagrams, video as a stream per group, newest first (Media over QUIC).

use crate::sync::{frame, unframe};
use crate::{Ch, Id};
use iroh::endpoint::{Connection, SendStream};
use iroh::{EndpointId, PublicKey, SecretKey, Signature};
use n0_future::task::AbortOnDropHandle;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use tokio::sync::{broadcast, mpsc};

pub const ALPN: &[u8] = b"irch/av/1";
// A screen's keyframe fits; 128 held for a slow page stay within 128 MiB.
const MAX_FRAME: usize = 1 << 20;
// A frame's first byte: audio, video, a keyframe opening a group, or the
// codecs its sender decodes (a datagram, like audio).
const AUDIO: u8 = 0;
const KEY: u8 = 2;
const CODECS: u8 = 3;

pub type Media = broadcast::Sender<(Id, Vec<u8>)>;

pub struct Call {
    pub space: Id,
    pub channel: Ch,
    pub run: u64,
    peers: HashMap<EndpointId, Peer>,
}

struct Peer {
    conn: Connection,
    video: mpsc::Sender<Vec<u8>>,
    // A frame was dropped: hold video until the next keyframe.
    skip: bool,
    _io: [AbortOnDropHandle<()>; 2],
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.conn.close(0u32.into(), b"bye");
    }
}

impl Call {
    pub fn new(space: Id, channel: Ch, run: u64) -> Self {
        Self { space, channel, run, peers: HashMap::new() }
    }

    pub fn has(&self, d: &EndpointId) -> bool {
        self.peers.contains_key(d)
    }

    /// Drops closed connections too, so they are dialed again.
    pub fn retain(&mut self, keep: impl Fn(&EndpointId) -> bool) {
        self.peers.retain(|d, p| keep(d) && p.conn.close_reason().is_none());
    }

    pub fn add(&mut self, user: Id, conn: Connection, out: Media) {
        let (tx, rx) = mpsc::channel(16);
        let r = tokio::spawn(hear(conn.clone(), user, out));
        let w = tokio::spawn(show(conn.clone(), rx));
        let _io = [AbortOnDropHandle::new(r), AbortOnDropHandle::new(w)];
        let p = Peer { conn, video: tx, skip: false, _io };
        self.peers.insert(p.conn.remote_id(), p);
    }

    pub fn send(&mut self, f: &[u8]) {
        for p in self.peers.values_mut() {
            match f.first() {
                None => {}
                Some(&(AUDIO | CODECS)) => {
                    drop(p.conn.send_datagram(f.to_vec().into()))
                }
                Some(&k) => {
                    p.skip &= k != KEY;
                    p.skip = p.skip || p.video.try_send(f.to_vec()).is_err();
                }
            }
        }
    }
}

/// Who is at the far end: each user signs their device id, which QUIC
/// vouches for, so frames are a member's only from their device.
pub async fn hello(
    c: &Connection,
    me: &SecretKey,
    at: EndpointId,
) -> Option<Id> {
    let sig = me.sign(&[ALPN, at.as_bytes()].concat()).to_bytes();
    let mut s = c.open_uni().await.ok()?;
    s.write_all(&[me.public().as_bytes(), &sig[..]].concat()).await.ok()?;
    s.finish().ok()?;
    let b = c.accept_uni().await.ok()?.read_to_end(96).await.ok()?;
    let (user, sig) = b.split_first_chunk::<32>()?;
    let sig = Signature::from_bytes(sig.try_into().ok()?);
    let msg = [ALPN, c.remote_id().as_bytes()].concat();
    PublicKey::from_bytes(user).ok()?.verify(&msg, &sig).ok()?;
    Some(Id(*user))
}

/// Each keyframe opens a stream outranking the last, left to finish or not.
async fn show(conn: Connection, mut rx: mpsc::Receiver<Vec<u8>>) {
    let (mut s, mut group) = (None::<SendStream>, 0);
    while let Some(f) = rx.recv().await {
        if f[0] == KEY {
            if let Some(mut old) = s.take() {
                let _ = old.finish();
            }
            group += 1;
            s = conn.open_uni().await.ok();
            s.iter().for_each(|s| drop(s.set_priority(group)));
        }
        if let Some(st) = s.as_mut()
            && frame(st, &f).await.is_err()
        {
            s = None;
        }
    }
}

/// Frames of a group a newer one overtook are dropped, never shown late.
async fn hear(conn: Connection, user: Id, out: Media) {
    let newest = Arc::new(AtomicU64::new(0));
    loop {
        tokio::select! {
            d = conn.read_datagram() => match d {
                Ok(d) => drop(out.send((user, d.to_vec()))),
                Err(_) => return,
            },
            s = conn.accept_uni() => {
                let Ok(mut s) = s else { return };
                let (out, newest) = (out.clone(), newest.clone());
                let me = s.id().index() + 1;
                tokio::spawn(async move {
                    while let Ok(f) = unframe(&mut s, MAX_FRAME).await {
                        if newest.fetch_max(me, Relaxed) > me {
                            return;
                        }
                        let _ = out.send((user, f));
                    }
                });
            }
        }
    }
}
