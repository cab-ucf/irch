//! Set reconciliation over one QUIC stream; gossip's safety net.
//!
//! ```text
//! A → B  Hello { space, root, token, devices }   root: hash of day buckets
//! B → A  None (same) | Some(B's buckets)    (unknown space: stream closed)
//! A → B  A's ids for every day that differs
//! B → A  ids B wants, then the items A lacks, then an empty frame
//! A → B  the items B wants, then an empty frame; B finishes, A closes
//! ```
//! Items are opaque envelopes, so peers that cannot read a space (storage
//! nodes) reconcile it too. Fingerprints hash sorted ids, never XOR them.
//! Lists arrive as maps and sets: whatever a peer repeats counts once.

use crate::Id;
use crate::node::{Node, Space};
use anyhow::{Result, anyhow};
use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

pub const ALPN: &[u8] = b"irch/sync/1";
const MAX_FRAME: usize = 64 << 20;
pub type Bucket = (u32, u32, [u8; 16]);

#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub space: Id,
    root: [u8; 32],
    pub token: Option<[u8; 16]>,
    // With the token: the devices a storage node may take this space from.
    pub devices: Vec<EndpointId>,
}

pub fn root(b: &[Bucket]) -> [u8; 32] {
    blake3::hash(&postcard::to_allocvec(b).expect("buckets")).into()
}

/// Reconcile `sp` with the peer behind `c`; returns how many ops were new.
pub async fn initiate(
    n: &Arc<Node>,
    sp: &Arc<Space>,
    c: &Connection,
    token: Option<[u8; 16]>,
) -> Result<usize> {
    let (mut tx, mut rx) = c.open_bi().await?;
    let mine = sp.buckets();
    let devices = token.map(|_| sp.devices()).unwrap_or_default();
    let hello = Hello { space: sp.id, root: root(&mine), token, devices };
    write(&mut tx, &hello).await?;
    let Some(theirs) = read::<Option<Vec<Bucket>>>(&mut rx).await? else {
        return Ok(0);
    };
    let mut days: BTreeMap<u32, (Option<Bucket>, Option<Bucket>)> =
        BTreeMap::new();
    mine.into_iter().for_each(|b| days.entry(b.0).or_default().0 = Some(b));
    theirs.into_iter().for_each(|b| days.entry(b.0).or_default().1 = Some(b));
    let diff = days.into_iter().filter(|(_, (a, b))| a != b).map(|(d, _)| d);
    let ids: BTreeMap<u32, Vec<Id>> = diff.map(|d| (d, sp.ids(d))).collect();
    write(&mut tx, &ids).await?;
    let want: BTreeSet<Id> = read(&mut rx).await?;
    let got = take(n, sp, &mut rx).await?;
    give(sp, &mut tx, &want.into_iter().collect::<Vec<_>>()).await?;
    tx.finish()?;
    // Closing now could drop what is still in flight: wait for B's end.
    let _ = rx.read_to_end(0).await;
    Ok(got)
}

/// Serve one session, then let its last bytes land before we go.
pub async fn respond(n: &Arc<Node>, c: &Connection) -> Result<()> {
    session(n, c).await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), c.closed()).await;
    Ok(())
}

async fn session(n: &Arc<Node>, c: &Connection) -> Result<()> {
    let (mut tx, mut rx) = c.accept_bi().await?;
    // From anyone, before we know the space: small (2,000 devices).
    let hello: Hello =
        postcard::from_bytes(&unframe(&mut rx, 64 << 10).await?)?;
    let sp = n
        .find(&hello, c.remote_id())
        .ok_or_else(|| anyhow!("unknown space"))?;
    let mine = sp.buckets();
    if root(&mine) == hello.root {
        write(&mut tx, &None::<()>).await?;
        return Ok(tx.finish()?);
    }
    write(&mut tx, &Some(mine)).await?;
    let keep = n.takes(&sp, &c.remote_id());
    let (mut want, mut have): (Vec<Id>, Vec<Id>) = (vec![], vec![]);
    for (day, theirs) in read::<BTreeMap<u32, BTreeSet<Id>>>(&mut rx).await? {
        let ours = sp.ids(day);
        let new = theirs.iter().filter(|i| ours.binary_search(i).is_err());
        want.extend(new.filter(|_| keep));
        have.extend(ours.into_iter().filter(|i| !theirs.contains(i)));
    }
    write(&mut tx, &want).await?;
    give(&sp, &mut tx, &have).await?;
    if keep {
        take(n, &sp, &mut rx).await?;
    }
    Ok(tx.finish()?)
}

async fn give(sp: &Space, tx: &mut SendStream, ids: &[Id]) -> Result<()> {
    for chunk in ids.chunks(256) {
        for b in sp.get(chunk) {
            frame(tx, &b).await?;
        }
    }
    frame(tx, &[]).await
}

/// Receive items up to the empty frame, ingesting 512 at a time.
async fn take(
    n: &Arc<Node>,
    sp: &Arc<Space>,
    rx: &mut RecvStream,
) -> Result<usize> {
    let (mut got, mut batch) = (0, vec![]);
    loop {
        let b = unframe(rx, crate::op::MAX_ENV).await?;
        let done = b.is_empty();
        if !done {
            batch.push(b);
        }
        if done || batch.len() == 512 {
            got += n.ingest(sp, std::mem::take(&mut batch), false);
        }
        if done {
            return Ok(got);
        }
    }
}

async fn write<T: Serialize>(tx: &mut SendStream, v: &T) -> Result<()> {
    frame(tx, &postcard::to_allocvec(v)?).await
}

async fn read<T: DeserializeOwned>(rx: &mut RecvStream) -> Result<T> {
    Ok(postcard::from_bytes(&unframe(rx, MAX_FRAME).await?)?)
}

pub(crate) async fn frame(tx: &mut SendStream, b: &[u8]) -> Result<()> {
    tx.write_all(&(b.len() as u32).to_be_bytes()).await?;
    Ok(tx.write_all(b).await?)
}

pub(crate) async fn unframe(
    rx: &mut RecvStream,
    max: usize,
) -> Result<Vec<u8>> {
    let mut len = [0; 4];
    rx.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    anyhow::ensure!(len <= max, "frame of {len} bytes");
    let mut b = vec![0; len];
    rx.read_exact(&mut b).await?;
    Ok(b)
}
