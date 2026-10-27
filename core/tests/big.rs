//! Big files: sealed in chunks, kept by storage nodes past their limit only
//! with their password. Its own process: the limit is set by environment.

use irch_core::{Config, Net, Node};
use std::sync::Arc;
use std::time::Duration;

async fn start(name: &str, serve: bool, big: Option<&str>) -> Arc<Node> {
    let t = std::time::UNIX_EPOCH.elapsed().unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("irch-big-{t}-{name}"));
    let (net, notes, link) = (Net::Direct, None, None);
    let (name, big) = (name.into(), big.map(Into::into));
    Node::start(Config { dir, name, net, link, serve, notes, big })
        .await
        .unwrap()
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..400 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn past_the_limit_with_the_password() {
    // 1 MiB stands for 5 GB.
    unsafe { std::env::set_var("IRCH_TEST_MAX_FILE", "1048576") };
    let s = start("s", true, Some("open sesame")).await;
    let (a, b) = (start("a", false, None).await, start("b", false, None).await);
    let id = a.create("lab").await.unwrap();
    a.add_storage(&s.storage_code().await.unwrap()).await.unwrap();
    let (addr, _): (iroh::EndpointAddr, [u8; 16]) =
        irch_core::op::uncode("host", &s.storage_code().await.unwrap())
            .unwrap();
    a.store(&id, &addr.id, true).await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();

    // Past what opens in memory too: sealed and opened in chunks, on disk.
    let data: Vec<u8> =
        (0..(25u32 << 20) + 70_000).map(|i| (i * 31 % 251) as u8).collect();
    let src = std::env::temp_dir()
        .join(format!("irch-big-src-{}", std::process::id()));
    std::fs::write(&src, &data).unwrap();
    let err = a.seal_file(&id, &src, "scan.tif", None).await.unwrap_err();
    assert!(err.to_string().contains("password"), "{err}");
    assert!(a.seal_file(&id, &src, "scan.tif", Some("guess")).await.is_err());
    let f =
        a.seal_file(&id, &src, "scan.tif", Some("open sesame")).await.unwrap();
    assert_eq!(f.size, data.len() as u64);
    let small = a.attach(&id, "note.txt", b"fits").await.unwrap();

    // Kept by the storage node: b gets both with a gone.
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    tokio::time::sleep(Duration::from_secs(3)).await; // s fetches from a
    a.shutdown().await;
    assert!(b.file(&id, &f).await.is_err(), "too big to open in memory");
    let out = std::env::temp_dir()
        .join(format!("irch-big-out-{}", std::process::id()));
    let p = b.save(&id, &f, &out).await.unwrap();
    assert!(std::fs::read(&p).unwrap() == data, "whole, from storage");
    assert_eq!(b.file(&id, &small).await.unwrap(), b"fits");
}
