//! Events through ntfy, a UnifiedPush server: one HTTPS POST each, sealed
//! with the space's key and padded, so every frame is `SIZE` B. ntfy holds
//! the socket, its keepalives and a 12 h cache; we keep no network of our
//! own. A desktop's inbox is a topic it streams, marked `#ev`: it hears
//! every event. A phone's is its UnifiedPush endpoint: woken by posts and
//! calls only, not by every read or keystroke.

use crate::crypto::{Key, kdf, open, rand, same, seal};
use crate::{Id, Net, store};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

/// Under ntfy's 4096 B message limit.
pub const SIZE: usize = 3072;
const BODY: usize = SIZE - 16 - 24 - 16 - 2;
/// ntfy says something every 45 s by default; four missed, reopened.
const QUIET: Duration = Duration::from_secs(180);

fn tag(key: &Key) -> [u8; 16] {
    kdf("irch ntfy tag", &[key])[..16].try_into().expect("16 of 32")
}

/// `b` for the members of the space `key` opens: tag ‖ nonce ‖ sealed.
pub fn frame(key: &Key, b: &[u8]) -> Option<Vec<u8>> {
    let n = u16::try_from(b.len()).ok().filter(|&n| n as usize <= BODY)?;
    let mut m = [&n.to_le_bytes()[..], b].concat();
    m.resize(BODY + 2, 0);
    let t = tag(key);
    Some([&t[..], &seal(&kdf("irch ntfy", &[key]), &t, &m)].concat())
}

/// The space of `keys` a frame is for, and what it says.
pub fn unframe(
    keys: impl IntoIterator<Item = (Id, Key)>,
    f: &[u8],
) -> Option<(Id, Vec<u8>)> {
    let (t, ct) = f.split_first_chunk::<16>()?;
    let (id, key) = keys.into_iter().find(|k| same(&tag(&k.1), t))?;
    let m = open(&kdf("irch ntfy", &[&key]), t, ct)?;
    let (n, b) = m.split_first_chunk::<2>()?;
    Some((id, b.get(..u16::from_le_bytes(*n) as usize)?.to_vec()))
}

/// Our inbox, `SERVER/upXXXXXXXXXXXX?up=1#ev` (a UnifiedPush topic: ntfy
/// takes binary), kept in DIR/ntfy. SERVER: DIR/ntfy-server, `IRCH_NTFY`,
/// else ntfy.sh on the default network; elsewhere, none.
pub fn inbox(dir: &Path, net: &Net) -> Option<String> {
    let given = std::fs::read_to_string(dir.join("ntfy-server")).ok();
    let given = given.or_else(|| std::env::var("IRCH_NTFY").ok());
    let dflt = matches!(net, Net::Default).then(|| "https://ntfy.sh".into());
    let server = given.or(dflt)?.trim().trim_end_matches('/').to_string();
    let f = dir.join("ntfy");
    let kept = std::fs::read_to_string(&f).ok();
    if let Some(k) = kept.filter(|k| k.starts_with(&format!("{server}/up"))) {
        return Some(k);
    }
    let t = data_encoding::BASE32_NOPAD.encode(&rand::<8>()).to_lowercase();
    let url = format!("{server}/up{}?up=1#ev", &t[..12]);
    store::write(&f, url.as_bytes()).ok()?;
    Some(url)
}

#[derive(Deserialize)]
struct Msg {
    id: String,
    event: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    encoding: String,
}

/// Each frame to `inbox`, forever: one stream, reopened where it broke.
pub async fn listen(inbox: String, on: impl Fn(Vec<u8>)) {
    let url = format!("{}/json", inbox.split('?').next().unwrap_or_default());
    let c = crate::node::client().read_timeout(QUIET).build();
    let c = c.expect("HTTP client");
    let (mut since, mut wait) = ((crate::now() / 1000).to_string(), 1);
    loop {
        let r = c.get(format!("{url}?since={since}")).send().await;
        if let Ok(mut r) = r.and_then(|r| r.error_for_status()) {
            let mut line = vec![];
            while let Ok(Some(b)) = r.chunk().await {
                wait = 1;
                line.extend_from_slice(&b);
                while let Some(i) = line.iter().position(|&x| x == b'\n') {
                    let l: Vec<u8> = line.drain(..=i).collect();
                    let Ok(m) = serde_json::from_slice::<Msg>(&l) else {
                        continue;
                    };
                    if m.event != "message" {
                        continue;
                    }
                    since = m.id;
                    on(match m.encoding.as_str() {
                        "base64" => data_encoding::BASE64
                            .decode(m.message.as_bytes())
                            .unwrap_or_default(),
                        _ => m.message.into_bytes(),
                    });
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(wait)).await;
        wait = (wait * 2).min(300);
    }
}

/// One frame to an inbox; whether ntfy took it.
pub async fn post(c: &reqwest::Client, url: &str, frame: Vec<u8>) -> bool {
    let r = c.post(url).header("TTL", "86400").header("Urgency", "high");
    let r = r.body(frame).send().await;
    r.is_ok_and(|r| r.status().is_success())
}

#[test]
fn frames_open_only_for_their_space_and_are_one_size() {
    let (a, b) = (Id([1; 32]), Id([2; 32]));
    let (ka, kb) = ([3u8; 32], [4u8; 32]);
    let f = frame(&ka, b"hello").unwrap();
    assert_eq!(f.len(), SIZE);
    assert_eq!(frame(&ka, &[0; 9]).unwrap().len(), SIZE);
    assert!(frame(&ka, &[0; BODY + 1]).is_none(), "too big");
    let got = unframe([(b, kb), (a, ka)], &f).unwrap();
    assert_eq!(got, (a, b"hello".to_vec()));
    assert!(unframe([(b, kb)], &f).is_none(), "another space");
    let mut bad = f.clone();
    bad[SIZE - 1] ^= 1;
    assert!(unframe([(a, ka)], &bad).is_none(), "tampered");
}
