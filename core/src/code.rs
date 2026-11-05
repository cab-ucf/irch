//! Short codes (`k7m-x9q-p3a`) for an invite or a link code, good for a
//! minute and one try. The code, stretched (Argon2id), is a throwaway iroh
//! identity its giver answers as, found by its id like any other; the taker
//! proves the code and gets the long code sealed to a fresh X-Wing key.
//! Whoever sees that identity must brute-force 2^45 stretched codes within
//! the minute to learn it; one wrong proof burns it.

use crate::crypto::{kem_open, kem_seal, rand, same};
use crate::{Net, node::endpoint, sync::rest};
use anyhow::{Context, Result, bail, ensure};
use iroh::SecretKey;
use std::time::Duration;
use tokio::time::timeout;
use x_wing::{DecapsulationKey, Decapsulator, KeyExport};

const ALPN: &[u8] = b"irch/code/1";
pub const TTL: Duration = Duration::from_secs(60);
// Crockford's base32: no i, l, o, u to misread.
const ABC: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
const LEN: usize = 9;

/// A code's identity and proof, from the code as typed (any case, any
/// dashes or spaces; i and l read as 1, o as 0).
fn keys(code: &str) -> Result<(SecretKey, [u8; 32])> {
    let fix = |c: char| match c {
        'i' | 'l' => '1',
        'o' => '0',
        c => c,
    };
    let c: String = code
        .to_lowercase()
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(fix)
        .collect();
    ensure!(
        c.len() == LEN && c.bytes().all(|b| ABC.contains(&b)),
        "not a short code"
    );
    let mut k = [0u8; 64];
    argon2::Argon2::default()
        .hash_password_into(c.as_bytes(), b"irch short code 1", &mut k)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let (sk, proof) = k.split_at(32);
    Ok((SecretKey::from_bytes(sk.try_into()?), proof.try_into()?))
}

async fn stretched(code: String) -> Result<(SecretKey, [u8; 32])> {
    tokio::task::spawn_blocking(move || keys(&code)).await?
}

/// A short code for `long` (an invite or link code), answered for `TTL`.
pub async fn give(net: &Net, long: String) -> Result<String> {
    let r: [u8; LEN] = rand();
    let c: Vec<u8> = r.iter().map(|b| ABC[*b as usize % 32]).collect();
    let code = [&c[..3], &c[3..6], &c[6..]]
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .join("-");
    let (sk, proof) = stretched(code.clone()).await?;
    let ep = endpoint(net, None)
        .secret_key(sk)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    // Found by id: online, so a relay holds us when it is looked up.
    let _ = timeout(Duration::from_secs(10), ep.online()).await;
    tokio::spawn(async move {
        let one = async {
            let c = ep.accept().await?.await.ok()?;
            let (mut tx, mut rx) = c.accept_bi().await.ok()?;
            let b = rest(&mut rx, 32 + 2048).await.ok()?;
            let (got, pk) = b.split_at_checked(32)?;
            // One try: right or wrong, the code is spent.
            let sealed = same(got, &proof)
                .then(|| kem_seal(pk, ALPN, long.as_bytes()))??;
            tx.write_all(&sealed).await.ok()?;
            tx.finish().ok()?;
            let _ = timeout(Duration::from_secs(5), c.closed()).await;
            Some(())
        };
        let _ = timeout(TTL, one).await;
        ep.close().await;
    });
    Ok(code)
}

/// The long code a short one stands for, while its giver answers.
pub async fn take(net: &Net, code: &str) -> Result<String> {
    let (sk, proof) = stretched(code.into()).await?;
    let dk: DecapsulationKey = rand::<32>().into();
    let hello = [&proof[..], &dk.encapsulation_key().to_bytes()].concat();
    let ep = endpoint(net, None).bind().await?;
    // The giver homes on one of the relays we would: tried there, and by
    // its id (n0's DNS) where we look ids up.
    let relays = match net {
        Net::Default => {
            crate::node::RELAYS.iter().filter_map(|r| r.parse().ok()).collect()
        }
        Net::Relay(r) => r.clone(),
        _ => vec![],
    };
    let at = iroh::EndpointAddr::from_parts(
        sk.public(),
        relays.into_iter().map(iroh::TransportAddr::Relay),
    );
    let go = async {
        // Published a moment ago: looked up again until found.
        let c = loop {
            match ep.connect(at.clone(), ALPN).await {
                Ok(c) => break c,
                Err(_) => tokio::time::sleep(Duration::from_secs(2)).await,
            }
        };
        let (mut tx, mut rx) = c.open_bi().await?;
        tx.write_all(&hello).await?;
        tx.finish()?;
        let b = rest(&mut rx, 1 << 16).await?;
        let long = kem_open(&dk, ALPN, &b)
            .context("that code was used or is wrong")?;
        anyhow::Ok(String::from_utf8(long)?)
    };
    let r = timeout(Duration::from_secs(30), go).await;
    ep.close().await;
    match r {
        Ok(r) => r,
        Err(_) => bail!("nobody answers that code: wrong, or past its minute"),
    }
}

/// Given on one device, taken on another over a relay: the long code,
/// sealed to the taker; then spent.
#[tokio::test(flavor = "multi_thread")]
async fn a_short_code_hands_over_its_long_one() {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url = format!("http://{}/", relay.http_addr().unwrap());
    let net = Net::Relay(vec![url.parse().unwrap()]);
    let long = "irch:invite:abc".to_string();
    let code = give(&net, long.clone()).await.unwrap();
    assert_eq!(code.len(), 11, "xxx-xxx-xxx");
    assert_eq!(take(&net, &code.to_uppercase()).await.unwrap(), long);
    assert!(take(&net, &code).await.is_err(), "spent");
}

/// A wrong guess, even with the right id (seen published), gets nothing
/// and spends the code: one try.
#[tokio::test(flavor = "multi_thread")]
async fn one_wrong_proof_burns_a_code() {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let url: iroh::RelayUrl =
        format!("http://{}/", relay.http_addr().unwrap()).parse().unwrap();
    let net = Net::Relay(vec![url.clone()]);
    let code = give(&net, "irch:invite:abc".into()).await.unwrap();
    let (sk, _) = keys(&code).unwrap();
    let ep = endpoint(&net, None).bind().await.unwrap();
    let at = iroh::EndpointAddr::from_parts(
        sk.public(),
        [iroh::TransportAddr::Relay(url)],
    );
    let c = ep.connect(at, ALPN).await.unwrap();
    let (mut tx, mut rx) = c.open_bi().await.unwrap();
    let dk: DecapsulationKey = rand::<32>().into();
    let pk = dk.encapsulation_key().to_bytes();
    tx.write_all(&[&[0u8; 32][..], &pk].concat()).await.unwrap();
    tx.finish().unwrap();
    assert!(rest(&mut rx, 1 << 16).await.unwrap_or_default().is_empty());
    assert!(take(&net, &code).await.is_err(), "burnt");
}

#[test]
fn codes_read_as_typed() {
    let a = keys("k7m-x9q-p3a").unwrap();
    assert_eq!(a.0.public(), keys("K7M X9Q P3A").unwrap().0.public());
    assert_eq!(keys("o1l-000-111").unwrap().1, keys("011-000-111").unwrap().1);
    assert!(keys("k7m-x9q").is_err() && keys("k7m-x9q-p3u").is_err());
}
