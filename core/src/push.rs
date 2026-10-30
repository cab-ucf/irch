//! Push servers: a storage node with a Firebase key (`fcm.json` beside its
//! data) wakes phones through Google the moment a member rings, as their
//! doorbell would, so no phone holds a connection open for it. A phone tells
//! it its Firebase token and a ring token per space and epoch (wake.rs);
//! members ring with those; Google is handed only the sealed ring.

use crate::wake::{HEAD, MAX};
use crate::{node::lock, store};
use anyhow::{Context, Result, ensure};
use data_encoding::{BASE64, BASE64URL_NOPAD};
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const ALPN: &[u8] = b"irch/push/1";
/// Ring tokens a phone may have here: one per space it is in.
const MOST: usize = 4096;
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

/// Tells a push server our Firebase token and ring tokens.
pub async fn register(
    ep: &Endpoint,
    to: EndpointAddr,
    fcm: &str,
    rings: &[[u8; 16]],
) -> Result<()> {
    let go = async {
        let c = ep.connect(to, ALPN).await?;
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&postcard::to_allocvec(&(fcm, rings))?).await?;
        tx.finish()?;
        let ok = rx.read_to_end(1).await?;
        c.close(0u8.into(), b"");
        ensure!(ok == [1], "no push here");
        Ok(())
    };
    tokio::time::timeout(SLOW, go).await?
}

type Phones = BTreeMap<EndpointId, (String, Vec<[u8; 16]>)>;

#[derive(Clone)]
pub struct Server(Arc<(PathBuf, Mutex<Phones>, Google)>);

impl Server {
    pub fn open(dir: &Path) -> Result<Option<Self>> {
        let Ok(json) = std::fs::read(dir.join("fcm.json")) else {
            return Ok(None);
        };
        let google = Google::new(&json).context("fcm.json")?;
        let file = dir.join("phones");
        let phones = Mutex::new(store::load(&file));
        Ok(Some(Self(Arc::new((file, phones, google)))))
    }

    async fn register(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let b = crate::sync::rest(&mut rx, 1024 + 17 * MOST).await?;
        let phone: (String, Vec<[u8; 16]>) = postcard::from_bytes(&b)?;
        ensure!(phone.0.len() <= 1024 && phone.1.len() <= MOST, "too big");
        let (file, phones, _) = &*self.0;
        {
            let mut p = lock(phones);
            p.insert(c.remote_id(), phone);
            store::save(file, &*p)?;
        }
        tx.write_all(&[1]).await?;
        tx.finish()?;
        let _ = tokio::time::timeout(SLOW, c.closed()).await;
        Ok(())
    }

    /// A doorbell's ring, for whichever phones hold its token.
    async fn ring(&self, c: &Connection) -> Result<()> {
        let (mut tx, mut rx) = c.accept_bi().await?;
        let mut head = [0u8; HEAD];
        rx.read_exact(&mut head).await?;
        let raw = crate::sync::rest(&mut rx, MAX).await?;
        let (_, phones, google) = &*self.0;
        let to: Vec<String> = lock(phones)
            .values()
            .filter(|p| p.1.iter().any(|t| t[..] == head[..16]))
            .map(|p| p.0.clone())
            .collect();
        // The space and its sealed ring, if Google's 4 KB take it.
        let mut r = head[16..48].to_vec();
        if raw.len() <= 2800 {
            r.extend_from_slice(&raw);
        }
        let live =
            postcard::from_bytes::<crate::op::Env>(&raw).is_ok_and(|e| e.live);
        let mut ok = false;
        for t in to {
            let sent = google.send(&t, &BASE64.encode(&r), live).await;
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
            ALPN => self.register(&c).await,
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

    /// High priority: it wakes a phone from its deepest sleep. A call's
    /// ring is no use after a minute; other news keeps a day.
    async fn send(&self, to: &str, r: &str, live: bool) -> Result<()> {
        let ttl = if live { "60s" } else { "86400s" };
        let m = serde_json::json!({ "message": {
            "token": to, "data": { "r": r },
            "android": { "priority": "high", "ttl": ttl },
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
