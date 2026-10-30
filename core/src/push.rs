//! Push servers: a storage node given a Firebase project (its service
//! account's key, `fcm.json`, and `google-services.json`) wakes phones
//! through Google the moment a member rings, as their doorbell would, so no
//! phone holds a connection open. Phones find it among the storage nodes
//! they know (theirs, and their spaces'), learn the project from it, and
//! give it their Firebase token, a ring token per space and epoch, and a
//! key (wake.rs). Members ring with those; Google carries the ring sealed
//! to that key, always the same size: when, and nothing else.

use crate::crypto::Key;
use crate::wake::{HEAD, MAX, News};
use crate::{Id, node::lock, store};
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
    /// Our Firebase token, ring tokens, and the key pushes are sealed to.
    Register(String, Vec<[u8; 16]>, Key),
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

#[derive(Clone)]
pub struct Server(Arc<(PathBuf, Mutex<Phones>, Google, Firebase)>);

impl Server {
    pub fn open(dir: &Path) -> Result<Option<Self>> {
        let (Ok(key), Ok(app)) = (
            std::fs::read(dir.join("fcm.json")),
            std::fs::read(dir.join("google-services.json")),
        ) else {
            return Ok(None);
        };
        let google = Google::new(&key).context("fcm.json")?;
        let firebase = app_of(&app).context("google-services.json")?;
        let file = dir.join("phones");
        let phones = Mutex::new(store::load(&file));
        Ok(Some(Self(Arc::new((file, phones, google, firebase)))))
    }

    async fn asked(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let b = crate::sync::rest(&mut rx, 1024 + 17 * MOST + 64).await?;
        let (file, phones, _, firebase) = &*self.0;
        let reply = match postcard::from_bytes(&b)? {
            Ask::Firebase => postcard::to_allocvec(firebase)?,
            Ask::Register(fcm, rings, key) => {
                ensure!(fcm.len() <= 1024 && rings.len() <= MOST, "too big");
                let mut p = lock(phones);
                p.insert(c.remote_id(), (fcm, rings, key));
                store::save(file, &*p)?;
                vec![1]
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
        let (_, phones, google, _) = &*self.0;
        let to: Vec<(String, Key)> = lock(phones)
            .values()
            .filter(|p| p.1.iter().any(|t| t[..] == head[..16]))
            .map(|p| (p.0.clone(), p.2))
            .collect();
        let ring = [&head[16..48], &raw[..]].concat();
        let mut ok = false;
        for (fcm, key) in to {
            let r = BASE64.encode(&seal(&key, &ring));
            let sent = google.send(&fcm, &r).await;
            ok |= sent.inspect_err(|e| tracing::warn!("fcm: {e:#}")).is_ok();
        }
        tx.write_all(&[ok as u8]).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }
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
