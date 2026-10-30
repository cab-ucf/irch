//! Push servers: a storage node with a Firebase key (`fcm.json` beside its
//! data) wakes phones through Google the moment a member rings, as their
//! doorbell would, so no phone holds a connection open for it. A phone tells
//! it its Firebase token and a ring token per space and epoch (wake.rs);
//! members ring with those. Google is told nothing but "wake": the same
//! empty message every time. The phone, woken, fetches the sealed rings
//! from the push server, which holds them for it a while.

use crate::wake::{HEAD, MAX, News};
use crate::{Id, Net, node::lock, store};
use anyhow::{Context, Result, ensure};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ALPN: &[u8] = b"irch/push/2";
/// Ring tokens a phone may have here: one per space it is in.
const MOST: usize = 4096;
/// Rings held for a phone, at most, and for at most a day.
const HELD: usize = 64;
const DAY: Duration = Duration::from_secs(24 * 3600);
const SLOW: Duration = Duration::from_secs(15);

/// This build's push servers (`IRCH_PUSH`: endpoint ids), at irch's relay.
pub fn defaults() -> Vec<EndpointAddr> {
    let relay: iroh::RelayUrl = crate::node::RELAYS[0].parse().expect("url");
    let ids = option_env!("IRCH_PUSH").unwrap_or_default().split(',');
    let ids = ids.filter_map(|s| s.trim().parse::<EndpointId>().ok());
    let at = |id| {
        EndpointAddr::from_parts(id, [TransportAddr::Relay(relay.clone())])
    };
    ids.map(at).collect()
}

#[derive(Serialize, Deserialize)]
enum Ask {
    /// Our Firebase token, ring tokens, and the key to our mailbox.
    Register(String, Vec<[u8; 16]>, [u8; 16]),
    /// What the mailbox holds, emptied.
    Fetch([u8; 16]),
}

async fn ask(ep: &Endpoint, to: EndpointAddr, a: &Ask) -> Result<Vec<u8>> {
    let go = async {
        let c = ep.connect(to, ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(a)?).await?;
        tx.finish()?;
        let got = crate::sync::rest(&mut rx, HELD * (MAX + 64)).await?;
        c.close(0u8.into(), b"");
        Ok(got)
    };
    tokio::time::timeout(SLOW, go).await?
}

pub async fn register(
    ep: &Endpoint,
    to: EndpointAddr,
    fcm: &str,
    rings: &[[u8; 16]],
    mailbox: [u8; 16],
) -> Result<()> {
    let a = Ask::Register(fcm.into(), rings.to_vec(), mailbox);
    ensure!(ask(ep, to, &a).await? == [1], "no push here");
    Ok(())
}

/// What our push servers (as last registered) hold for us.
pub async fn fetch(ep: &Endpoint, dir: &Path) -> Vec<News> {
    let at: Vec<EndpointAddr> = store::load(&dir.join("pushers"));
    let Ok(mailbox) = crate::wake::mailbox(dir) else { return vec![] };
    let a = &Ask::Fetch(mailbox);
    let got = n0_future::join_all(at.into_iter().map(|to| ask(ep, to, a)));
    let got = got.await.into_iter().flatten();
    let got = got.flat_map(|b| postcard::from_bytes::<Vec<Vec<u8>>>(&b));
    let news = got.flatten().filter_map(|r| {
        let (space, raw) = r.split_first_chunk::<32>()?;
        Some((Id(*space), raw.to_vec()))
    });
    news.collect()
}

/// The same, with no node running: one brief endpoint of its own.
pub async fn fetch_asleep(dir: &Path, net: &Net) -> Result<Vec<News>> {
    let ep = crate::node::endpoint(net).bind().await?;
    let news = fetch(&ep, dir).await;
    ep.close().await;
    Ok(news)
}

type Phones = BTreeMap<EndpointId, (String, Vec<[u8; 16]>, [u8; 16])>;
type Boxes = HashMap<[u8; 16], VecDeque<(Instant, Vec<u8>)>>;

#[derive(Clone)]
pub struct Server(Arc<(PathBuf, Mutex<Phones>, Mutex<Boxes>, Google)>);

impl Server {
    pub fn open(dir: &Path) -> Result<Option<Self>> {
        let Ok(json) = std::fs::read(dir.join("fcm.json")) else {
            return Ok(None);
        };
        let google = Google::new(&json).context("fcm.json")?;
        let file = dir.join("phones");
        let phones = Mutex::new(store::load(&file));
        let boxes = Mutex::default();
        Ok(Some(Self(Arc::new((file, phones, boxes, google)))))
    }

    async fn asked(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let b = crate::sync::rest(&mut rx, 1024 + 17 * MOST).await?;
        let (file, phones, boxes, _) = &*self.0;
        let reply = match postcard::from_bytes(&b)? {
            Ask::Register(fcm, rings, mailbox) => {
                ensure!(fcm.len() <= 1024 && rings.len() <= MOST, "too big");
                let mut p = lock(phones);
                p.insert(c.remote_id(), (fcm, rings, mailbox));
                store::save(file, &*p)?;
                vec![1]
            }
            Ask::Fetch(mailbox) => {
                let held = lock(boxes).remove(&mailbox).unwrap_or_default();
                let held = held.into_iter().filter(|(t, _)| t.elapsed() < DAY);
                postcard::to_allocvec(&held.map(|x| x.1).collect::<Vec<_>>())?
            }
        };
        tx.write_all(&reply).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }

    /// A doorbell's ring, held for whichever phones hold its token, which
    /// Google then wakes.
    async fn ring(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let mut head = [0u8; HEAD];
        rx.read_exact(&mut head).await?;
        let raw = crate::sync::rest(&mut rx, MAX).await?;
        let (_, phones, boxes, google) = &*self.0;
        let to: Vec<(String, [u8; 16])> = lock(phones)
            .values()
            .filter(|p| p.1.iter().any(|t| t[..] == head[..16]))
            .map(|p| (p.0.clone(), p.2))
            .collect();
        let ring = [&head[16..48], &raw[..]].concat();
        let mut ok = false;
        for (fcm, mailbox) in to {
            {
                let mut b = lock(boxes);
                let held = b.entry(mailbox).or_default();
                held.retain(|(t, _)| t.elapsed() < DAY);
                held.truncate(HELD - 1);
                held.push_front((Instant::now(), ring.clone()));
            }
            let sent = google.wake(&fcm).await;
            ok |= sent.inspect_err(|e| tracing::warn!("fcm: {e:#}")).is_ok();
        }
        tx.write_all(&[ok as u8]).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }
}

impl std::fmt::Debug for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("push server")
    }
}

impl ProtocolHandler for Server {
    async fn accept(&self, c: Connection) -> Result<(), AcceptError> {
        let r = match c.alpn() {
            ALPN => self.asked(&c).await,
            _ => self.ring(&c).await,
        };
        r.map_err(|e| {
            AcceptError::from_err(std::io::Error::other(e.to_string()))
        })
    }
}

/// Firebase Cloud Messaging, as a service account: a signed JWT for an
/// hour's token, then one POST a message.
struct Google {
    project: String,
    email: String,
    token_uri: String,
    fcm: String,
    key: RsaKeyPair,
    web: reqwest::Client,
    token: tokio::sync::Mutex<(String, Instant)>,
}

#[derive(serde::Deserialize)]
struct Account {
    project_id: String,
    client_email: String,
    private_key: String,
    token_uri: String,
    // Not Google's: elsewhere only to test.
    #[serde(default = "fcm")]
    fcm_uri: String,
}

fn fcm() -> String {
    "https://fcm.googleapis.com".into()
}

impl Google {
    fn new(json: &[u8]) -> Result<Self> {
        let a: Account = serde_json::from_slice(json)?;
        let pem: String =
            a.private_key.lines().filter(|l| !l.starts_with("-----")).collect();
        let key = RsaKeyPair::from_pkcs8(&BASE64.decode(pem.as_bytes())?)
            .map_err(|e| anyhow::anyhow!("private_key: {e}"))?;
        Ok(Self {
            project: a.project_id,
            email: a.client_email,
            token_uri: a.token_uri,
            fcm: a.fcm_uri,
            key,
            web: crate::node::client().build()?,
            token: tokio::sync::Mutex::new((String::new(), Instant::now())),
        })
    }

    fn jwt(&self) -> Result<String> {
        let b64 = |v: serde_json::Value| {
            BASE64URL_NOPAD.encode(v.to_string().as_bytes())
        };
        let now = crate::now() / 1000;
        let claims = serde_json::json!({
            "iss": self.email, "iat": now, "exp": now + 3600,
            "scope": "https://www.googleapis.com/auth/firebase.messaging",
            "aud": self.token_uri,
        });
        let alg = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
        let msg = format!("{}.{}", b64(alg), b64(claims));
        let mut sig = vec![0; self.key.public().modulus_len()];
        let rng = ring::rand::SystemRandom::new();
        self.key
            .sign(&RSA_PKCS1_SHA256, &rng, msg.as_bytes(), &mut sig)
            .map_err(|_| anyhow::anyhow!("signing"))?;
        Ok(format!("{msg}.{}", BASE64URL_NOPAD.encode(&sig)))
    }

    async fn bearer(&self) -> Result<String> {
        let mut t = self.token.lock().await;
        if t.0.is_empty() || t.1 < Instant::now() {
            let form = "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type\
                %3Ajwt-bearer&assertion="
                .to_string()
                + &self.jwt()?;
            let r = self
                .web
                .post(&self.token_uri)
                .header("content-type", "application/x-www-form-urlencoded")
                .body(form)
                .send()
                .await?
                .error_for_status()?;
            #[derive(serde::Deserialize)]
            struct Got {
                access_token: String,
                expires_in: u64,
            }
            let g: Got = serde_json::from_slice(&r.bytes().await?)?;
            let left = Duration::from_secs(g.expires_in.saturating_sub(60));
            *t = (g.access_token, Instant::now() + left);
        }
        Ok(t.0.clone())
    }

    /// "Wake", and nothing more: the same each time, so Google learns only
    /// when. High priority wakes a phone from its deepest sleep; one key
    /// folds wakes waiting on a phone that is off into one.
    async fn wake(&self, to: &str) -> Result<()> {
        let m = serde_json::json!({ "message": {
            "token": to, "data": { "w": "" },
            "android": { "priority": "high", "collapse_key": "w" },
        }});
        let url =
            format!("{}/v1/projects/{}/messages:send", self.fcm, self.project);
        let b = self.bearer().await?;
        let r = self.web.post(url).bearer_auth(b);
        let r = r.header("content-type", "application/json");
        r.body(m.to_string()).send().await?.error_for_status()?;
        Ok(())
    }
}
