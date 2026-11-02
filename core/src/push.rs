//! Push servers: every storage node with a relay is one. It wakes phones
//! the moment a member rings, as their doorbell would, through irpush: a
//! phone pairs with it once, then holds one quiet socket, shared, with
//! heartbeats as long as its network allows, and each ring comes sealed
//! end to end (post-quantum), always the same size. Given a Firebase
//! project too (its service account's key, `fcm.json`, and
//! `google-services.json`), it rings phones through Google instead, so
//! they hold nothing open. Phones find it among the storage nodes they know
//! (theirs, and their spaces'), and give it their route (`irpush:DEVICE`,
//! or a Firebase token), a ring token per space and epoch, and a key
//! (wake.rs). Members ring with those: the push server learns when, and
//! for which space, nothing else.

use crate::crypto::Key;
use crate::wake::{HEAD, MAX, News};
use crate::{Id, Net, node::lock, store};
use anyhow::{Context, Result, ensure};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ALPN: &[u8] = b"irch/push/3";
/// Ring tokens a phone may have here: one per space it is in.
const MOST: usize = 4096;
/// Every push's sealed size: a short message or a call fits whole.
const SIZE: usize = 2048;
const SLOW: Duration = Duration::from_secs(15);
/// irpush's address for irch on a phone.
const APP: &str = "dev.irch.chat";

/// A Firebase project's public half: what a phone needs for a token.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Firebase {
    pub app: String,
    pub key: String,
    pub project: String,
    pub sender: String,
}

#[derive(Serialize, Deserialize)]
enum Ask {
    Firebase,
    /// Our route (a Firebase token, or `irpush:DEVICE` once paired), ring
    /// tokens, and the key pushes are sealed to.
    Register(String, Vec<[u8; 16]>, Key),
    /// A one-time irpush ticket, to pair with.
    Ticket,
}

async fn ask(ep: &Endpoint, to: EndpointAddr, a: &Ask) -> Result<Vec<u8>> {
    let go = async {
        let c = ep.connect(to, ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(a)?).await?;
        tx.finish()?;
        let got = crate::sync::rest(&mut rx, 4096).await?;
        c.close(0u8.into(), b"");
        Ok(got)
    };
    tokio::time::timeout(SLOW, go).await?
}

/// Its Firebase project, if `to` is a push server.
pub async fn firebase(ep: &Endpoint, to: EndpointAddr) -> Result<Firebase> {
    Ok(postcard::from_bytes(&ask(ep, to, &Ask::Firebase).await?)?)
}

/// A ticket to pair our irpush device with `to`, if it pushes that way.
pub async fn ticket(ep: &Endpoint, to: EndpointAddr) -> Result<String> {
    Ok(String::from_utf8(ask(ep, to, &Ask::Ticket).await?)?)
}

pub async fn register(
    ep: &Endpoint,
    to: EndpointAddr,
    fcm: &str,
    rings: &[[u8; 16]],
    key: Key,
) -> Result<()> {
    let a = Ask::Register(fcm.into(), rings.to_vec(), key);
    ensure!(ask(ep, to, &a).await? == [1], "no push here");
    Ok(())
}

/// A ring (the space, then its sealed op) sealed again to a phone, padded
/// to one size; too big, the space alone, for the phone to catch up.
pub fn seal(key: &Key, ring: &[u8]) -> Vec<u8> {
    let ring = if ring.len() + 2 <= SIZE { ring } else { &ring[..32] };
    let mut b = (ring.len() as u16).to_le_bytes().to_vec();
    b.extend_from_slice(ring);
    b.resize(SIZE, 0);
    crate::crypto::seal(key, b"irch push", &b)
}

pub fn open(key: &Key, b: &[u8]) -> Option<News> {
    let b = crate::crypto::open(key, b"irch push", b)?;
    let (len, rest) = b.split_first_chunk::<2>()?;
    let ring = rest.get(..u16::from_le_bytes(*len) as usize)?;
    let (space, raw) = ring.split_first_chunk::<32>()?;
    Some((Id(*space), raw.to_vec()))
}

type Phones = BTreeMap<EndpointId, (String, Vec<[u8; 16]>, Key)>;

struct Inner {
    file: PathBuf,
    phones: Mutex<Phones>,
    google: Option<(Google, Firebase)>,
    irpush: Option<Arc<irpush::Node>>,
}

#[derive(Clone)]
pub struct Server(Arc<Inner>);

impl Server {
    pub fn open(dir: &Path, net: &Net) -> Result<Option<Self>> {
        let google = match (
            std::fs::read(dir.join("fcm.json")),
            std::fs::read(dir.join("google-services.json")),
        ) {
            (Ok(key), Ok(app)) => Some((
                Google::new(&key).context("fcm.json")?,
                app_of(&app).context("google-services.json")?,
            )),
            _ => None,
        };
        let irpush = host(dir, net)?;
        if google.is_none() && irpush.is_none() {
            return Ok(None);
        }
        let file = dir.join("phones");
        let phones = Mutex::new(store::load(&file));
        Ok(Some(Self(Arc::new(Inner { file, phones, google, irpush }))))
    }

    async fn asked(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let b = crate::sync::rest(&mut rx, 1024 + 17 * MOST + 64).await?;
        let s = &*self.0;
        let reply = match postcard::from_bytes(&b)? {
            Ask::Firebase => {
                let g = s.google.as_ref().context("no Firebase here")?;
                postcard::to_allocvec(&g.1)?
            }
            Ask::Register(to, rings, key) => {
                ensure!(to.len() <= 1024 && rings.len() <= MOST, "too big");
                // irpush: only a device paired here.
                let ok = match to.strip_prefix("irpush:") {
                    Some(d) => s.irpush.as_ref().is_some_and(|n| {
                        n.peers().iter().any(|p| p.0.to_string() == d)
                    }),
                    None => s.google.is_some(),
                };
                if ok {
                    let mut p = lock(&s.phones);
                    p.insert(c.remote_id(), (to, rings, key));
                    store::save(&s.file, &*p)?;
                }
                vec![ok as u8]
            }
            Ask::Ticket => {
                let n = s.irpush.as_ref().context("no irpush here")?;
                n.ticket(irpush::Role::Device)?.into_bytes()
            }
        };
        tx.write_all(&reply).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }

    /// A doorbell's ring, to whichever phones hold its token.
    async fn ring(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let mut head = [0u8; HEAD];
        rx.read_exact(&mut head).await?;
        let raw = crate::sync::rest(&mut rx, MAX).await?;
        let s = &*self.0;
        let to: Vec<(String, Key)> = lock(&s.phones)
            .values()
            .filter(|p| p.1.iter().any(|t| t[..] == head[..16]))
            .map(|p| (p.0.clone(), p.2))
            .collect();
        let ring = [&head[16..48], &raw[..]].concat();
        let mut ok = false;
        for (to, key) in to {
            let sent = match (to.strip_prefix("irpush:"), &s.irpush, &s.google)
            {
                // Sealed end to end already: the ring as it is, or too big
                // for one frame, the space alone, for the phone to catch up.
                (Some(d), Some(n), _) => {
                    let fits = ring.len() <= irpush::BODY;
                    let r = if fits { &ring[..] } else { &ring[..32] };
                    d.parse()
                        .map_err(Into::into)
                        .and_then(|d| n.push(d, &irpush::app(APP), r))
                }
                (None, _, Some((g, _))) => {
                    g.send(&to, &BASE64.encode(&seal(&key, &ring))).await
                }
                _ => continue,
            };
            ok |= sent.inspect_err(|e| tracing::warn!("push: {e:#}")).is_ok();
        }
        tx.write_all(&[ok as u8]).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }
}

/// The state key of an irpush node kept here, as the doorbell's are.
fn kek(dir: &Path) -> Result<[u8; 32]> {
    let f = dir.join("irpush.kek");
    if let Some(k) = std::fs::read(&f).ok().and_then(|k| k.try_into().ok()) {
        return Ok(k);
    }
    let k = crate::crypto::rand::<32>();
    store::write(&f, &k)?;
    Ok(k)
}

/// Where a storage node's irpush meets phones: `IRCH_PUSH_RELAY`, else
/// irch's relays (n0's last unless they are your own). LAN only, none.
fn relays(net: &Net) -> Option<Vec<irpush::Relay>> {
    let l = match (std::env::var("IRCH_PUSH_RELAY"), net) {
        (Ok(l), _) => l,
        (_, Net::Default) => crate::node::RELAYS[0].into(),
        (_, Net::Relay(u)) => format!("{},!n0", u[0]),
        _ => return None,
    };
    irpush::Relay::list(&l).ok().filter(|l| !l.is_empty())
}

/// A storage node's irpush host, running.
fn host(dir: &Path, net: &Net) -> Result<Option<Arc<irpush::Node>>> {
    let Some(relays) = relays(net) else { return Ok(None) };
    let (f, kek) = (dir.join("irpush"), kek(dir)?);
    let n = match f.exists() {
        true => irpush::Node::open(&f, kek, vec![])?,
        false => irpush::Node::create(&f, kek, relays.clone())?,
    };
    if n.relays() != relays {
        n.set_relays(relays)?;
    }
    let n = Arc::new(n);
    let run = n.clone();
    tokio::spawn(async move { run.run(|_, _, _| {}).await });
    Ok(Some(n))
}

/// A phone's irpush device: one per profile, its pushes as rings.
pub struct Device {
    pub node: Arc<irpush::Node>,
    rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<News>>,
    run: tokio::task::AbortHandle,
}

impl Drop for Device {
    fn drop(&mut self) {
        self.run.abort();
    }
}

impl Device {
    /// What a phone gives its push server.
    pub fn url(&self) -> String {
        format!("irpush:{}", self.node.id())
    }

    pub fn paired(&self) -> bool {
        !self.node.peers().is_empty()
    }

    /// The next ring that came as a push.
    pub async fn next(&self) -> Option<News> {
        self.rx.lock().await.recv().await
    }
}

static DEVICES: Mutex<BTreeMap<PathBuf, Arc<Device>>> =
    Mutex::new(BTreeMap::new());

/// Ours, running: kept from before, or joined now with `ticket`.
pub fn device(dir: &Path, ticket: Option<&str>) -> Result<Option<Arc<Device>>> {
    let mut all = lock(&DEVICES);
    if let Some(d) = all.get(dir) {
        return Ok(Some(d.clone()));
    }
    let (f, kek) = (dir.join("irpush"), kek(dir)?);
    let node = match (f.exists(), ticket) {
        (true, _) => irpush::Node::open(&f, kek, vec![])?,
        (false, Some(t)) => irpush::Node::join(&f, kek, t, vec![])?,
        (false, None) => return Ok(None),
    };
    let (node, (tx, rx)) =
        (Arc::new(node), tokio::sync::mpsc::unbounded_channel());
    let (run, me) = (node.clone(), irpush::app(APP));
    let on = move |_, app, b: &[u8]| {
        if let Some((sp, raw)) =
            b.split_first_chunk::<32>().filter(|_| app == me)
        {
            let _ = tx.send((Id(*sp), raw.to_vec()));
        }
    };
    let run = tokio::spawn(async move { run.run(on).await }).abort_handle();
    let d = Arc::new(Device { node, rx: rx.into(), run });
    all.insert(dir.into(), d.clone());
    Ok(Some(d))
}

/// Ours, never paired (its ticket ran out): gone, for a new one.
pub fn forget(dir: &Path) {
    lock(&DEVICES).remove(dir);
    let _ = std::fs::remove_file(dir.join("irpush"));
}

/// Heartbeat now (an alarm rang): seconds to the next, 0 with no device.
pub fn kick() -> u64 {
    let all = lock(&DEVICES);
    all.values().map(|d| d.node.kick().as_secs()).min().unwrap_or(0)
}

/// Another network (a stable id for each), or none (`irpush::OFFLINE`).
pub fn network(id: u64) {
    lock(&DEVICES).values().for_each(|d| d.node.network(id));
}

/// From `google-services.json`: the Android app `dev.irch.chat`'s settings.
fn app_of(json: &[u8]) -> Result<Firebase> {
    let v: serde_json::Value = serde_json::from_slice(json)?;
    let p = &v["project_info"];
    let us = |c: &&serde_json::Value| {
        c["client_info"]["android_client_info"]["package_name"]
            == "dev.irch.chat"
    };
    let c = v["client"].as_array().and_then(|c| c.iter().find(us));
    let c = c.context("no Android app dev.irch.chat in it")?;
    let s = |v: &serde_json::Value| v.as_str().unwrap_or_default().to_string();
    Ok(Firebase {
        app: s(&c["client_info"]["mobilesdk_app_id"]),
        key: s(&c["api_key"][0]["current_key"]),
        project: s(&p["project_id"]),
        sender: s(&p["project_number"]),
    })
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

    /// High priority wakes a phone from its deepest sleep. Kept by Google
    /// for a phone that is off as long as it keeps any (four weeks); past
    /// that, irch catches up when opened.
    async fn send(&self, to: &str, r: &str) -> Result<()> {
        let m = serde_json::json!({ "message": {
            "token": to, "data": { "r": r },
            "android": { "priority": "high", "ttl": "2419200s" },
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
