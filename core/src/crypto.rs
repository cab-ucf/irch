//! BLAKE3 derivations, XChaCha20-Poly1305 and, to reach one member, X-Wing
//! (ML-KEM-768 + X25519): post-quantum where it is stored.

use chacha20poly1305::{
    KeyInit, XChaCha20Poly1305,
    aead::{Aead, Payload},
};
use iroh::SecretKey;
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
