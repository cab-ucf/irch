//! BLAKE3 derivations, XChaCha20-Poly1305 and, to reach one member, X-Wing
//! (ML-KEM-768 + X25519): post-quantum where it is stored.

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305,
    aead::{Aead, Payload},
};
use iroh::SecretKey;
use std::io::{self, Read, Write};
use x_wing::{
    Decapsulate, DecapsulationKey, Decapsulator, Encapsulate, EncapsulationKey,
};
use x_wing::{KeyExport, kem::Ciphertext};

pub type Key = [u8; 32];
const KEM_CT: usize = 1120;

pub fn kdf(ctx: &str, parts: &[&[u8]]) -> Key {
    let mut h = blake3::Hasher::new_derive_key(ctx);
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

pub fn rand<const N: usize>() -> [u8; N] {
    let mut b = [0; N];
    getrandom::fill(&mut b).expect("os randomness");
    b
}

fn aead(key: &Key) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new(key.into())
}

/// nonce ‖ ciphertext
pub fn seal(key: &Key, aad: &[u8], msg: &[u8]) -> Vec<u8> {
    let n: [u8; 24] = rand();
    let ct = aead(key).encrypt(&n.into(), Payload { msg, aad });
    [&n[..], &ct.expect("in memory")].concat()
}

pub fn open(key: &Key, aad: &[u8], b: &[u8]) -> Option<Vec<u8>> {
    let (n, msg) = b.split_at_checked(24)?;
    let n: [u8; 24] = n.try_into().ok()?;
    aead(key).decrypt(&n.into(), Payload { msg, aad }).ok()
}

/// A file's sealed form begins so (an older one is `seal`ed whole).
pub const STREAM: &[u8; 8] = b"irch-s1\0";
const CHUNK: usize = 64 << 10;

/// Its size, sealed: the head, and a tag per chunk (an empty file has one).
pub fn sealed_len(size: u64) -> u64 {
    8 + size + 16 * size.div_ceil(CHUNK as u64).max(1)
}

/// STREAM: chunk `i` under nonce `i`, the last one marked, so any size
/// passes in constant memory and a file cut short does not open.
fn nonce(i: u64, last: bool) -> [u8; 24] {
    let mut n = [0; 24];
    n[..8].copy_from_slice(&i.to_le_bytes());
    n[8] = last as u8;
    n
}

fn chunk(r: &mut impl Read, n: usize) -> io::Result<Vec<u8>> {
    let mut v = Vec::with_capacity(n);
    r.take(n as u64).read_to_end(&mut v)?;
    Ok(v)
}

/// `key` is the file's own, never reused.
pub fn seal_stream(
    key: &Key,
    mut r: impl Read,
    mut w: impl Write,
) -> io::Result<()> {
    w.write_all(STREAM)?;
    let mut cur = chunk(&mut r, CHUNK)?;
    for i in 0.. {
        let next = match cur.len() == CHUNK {
            true => chunk(&mut r, CHUNK)?,
            false => vec![],
        };
        let p = Payload { msg: &cur, aad: b"irch file" };
        let ct = aead(key).encrypt(&nonce(i, next.is_empty()).into(), p);
        w.write_all(&ct.map_err(|_| io::Error::other("seal"))?)?;
        if next.is_empty() {
            break;
        }
        cur = next;
    }
    w.flush()
}

pub fn open_stream(
    key: &Key,
    mut r: impl Read,
    mut w: impl Write,
) -> io::Result<()> {
    let bad = || io::Error::other("damaged file");
    let mut head = [0; 8];
    r.read_exact(&mut head)?;
    if head != *STREAM {
        return Err(bad());
    }
    let mut cur = chunk(&mut r, CHUNK + 16)?;
    for i in 0.. {
        let next = match cur.len() == CHUNK + 16 {
            true => chunk(&mut r, CHUNK + 16)?,
            false => vec![],
        };
        let p = Payload { msg: &cur, aad: b"irch file" };
        let pt = aead(key).decrypt(&nonce(i, next.is_empty()).into(), p);
        w.write_all(&pt.map_err(|_| bad())?)?;
        if next.is_empty() {
            break;
        }
        cur = next;
    }
    w.flush()
}

/// X-Wing ciphertext ‖ `msg` sealed under the shared secret.
pub fn kem_seal(pk: &[u8], aad: &[u8], msg: &[u8]) -> Option<Vec<u8>> {
    let (ct, ss) = EncapsulationKey::try_from(pk).ok()?.encapsulate();
    Some([&ct[..], &seal(&ss.into(), aad, msg)].concat())
}

pub fn kem_open(
    dk: &DecapsulationKey,
    aad: &[u8],
    b: &[u8],
) -> Option<Vec<u8>> {
    let (ct, rest) = b.split_at_checked(KEM_CT)?;
    let ct = Ciphertext::<x_wing::XWingKem>::try_from(ct).ok()?;
    open(&dk.decapsulate(&ct).into(), aad, rest)
}

pub struct Me {
    pub seed: Key,
    pub user: SecretKey,
    pub kem: DecapsulationKey,
}

impl Me {
    pub fn new(seed: Key) -> Self {
        let user = SecretKey::from_bytes(&kdf("irch user", &[&seed]));
        let kem = kdf("irch kem", &[&seed]).into();
        Self { seed, user, kem }
    }

    pub fn id(&self) -> crate::Id {
        crate::Id(*self.user.public().as_bytes())
    }

    pub fn pk(&self) -> Vec<u8> {
        self.kem.encapsulation_key().to_bytes().to_vec()
    }
}

#[test]
fn boxes() {
    let me = Me::new(rand());
    let b = kem_seal(&me.pk(), b"ad", b"hi").unwrap();
    assert_eq!(kem_open(&me.kem, b"ad", &b).unwrap(), b"hi");
    assert!(kem_open(&me.kem, b"da", &b).is_none());
    assert!(kem_open(&Me::new(rand()).kem, b"ad", &b).is_none());
    let k = rand();
    assert_eq!(open(&k, b"", &seal(&k, b"", b"x")).unwrap(), b"x");
}

#[test]
fn streams() {
    let k = rand();
    for n in [0, 1, CHUNK, CHUNK + 1, 3 * CHUNK + 5] {
        let pt: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let mut ct = vec![];
        seal_stream(&k, &pt[..], &mut ct).unwrap();
        assert_eq!(ct.len() as u64, sealed_len(n as u64));
        let mut back = vec![];
        open_stream(&k, &ct[..], &mut back).unwrap();
        assert_eq!(back, pt);
        if n > CHUNK {
            let cut = &ct[..8 + CHUNK + 16];
            assert!(open_stream(&k, cut, &mut vec![]).is_err(), "cut short");
        }
        let mut bent = ct.clone();
        *bent.last_mut().unwrap() ^= 1;
        assert!(open_stream(&k, &bent[..], &mut vec![]).is_err());
    }
}
