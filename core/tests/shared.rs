//! Android: the phone's one irpush device is run by irpush's library for
//! every app (its owner may be another app, in another app's mesh). irch
//! joins it into its storage node's mesh through the platform, and rings
//! reach it there. A process of its own: `push::share` is process-wide.

use irch_core::{Config, Net, Node, Op};
use std::sync::Arc;
use std::time::Duration;

fn dir(name: &str) -> std::path::PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    let t = t.unwrap().as_nanos();
    let d = std::env::temp_dir().join(format!("irch-shared-{t}-{name}"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

async fn start(dir: &std::path::Path, name: &str, serve: bool) -> Arc<Node> {
    let cfg = Config {
        dir: dir.into(),
        name: name.into(),
        net: Net::Direct,
        link: None,
        serve,
        notes: None,
        big: None,
    };
    Node::start(cfg).await.unwrap()
}

async fn until(what: &str, f: impl Fn() -> bool) {
    for _ in 0..800 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_device_joins_the_storage_nodes_mesh_too() {
    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    let mut cfg = ServerConfig::default();
    cfg.relay = Some(RelayConfig::new(([127, 0, 0, 1], 0)));
    let relay = Server::spawn(cfg).await.unwrap();
    let at = format!("http://{}", relay.http_addr().unwrap());
    let r = irpush::Relay::parse(&at).unwrap();
    // SAFETY: storage nodes read it as they start; nothing else here does.
    unsafe { std::env::set_var("IRCH_PUSH_RELAY", &at) };

    // Another app's host (a pasa desktop, say) owns the phone's device.
    let key = irpush::random32;
    let other = irpush::Node::create(dir("o").join("s"), key(), vec![r]);
    let other = Arc::new(other.unwrap());
    let t = other.ticket(irpush::Role::Device).unwrap();
    let dev = irpush::Node::join(dir("d").join("s"), key(), &t, vec![]);
    let dev = Arc::new(dev.unwrap());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for n in [other.clone(), dev.clone()] {
        let tx = tx.clone();
        tokio::spawn(async move {
            let on = move |_, _, d: &[u8]| drop(tx.send(d.to_vec()));
            n.run(on).await
        });
    }
    until("the device joins the other mesh", || dev.has(&other.id())).await;

    // The platform: irpush's library, as it serves irch.
    irch_core::push::share(Some(dev.id().to_string()));
    let d = dev.clone();
    std::thread::spawn(move || {
        loop {
            let t = irch_core::push::next_join();
            let host = t[8..72].parse().unwrap();
            d.add(&t).unwrap();
            let end = std::time::Instant::now() + Duration::from_secs(30);
            while !d.has(&host) && std::time::Instant::now() < end {
                std::thread::sleep(Duration::from_millis(100));
            }
            irch_core::push::joined(d.has(&host));
        }
    });

    let s = start(&dir("s"), "s", true).await;
    let db = dir("b");
    let (a, b) =
        (start(&dir("a"), "a", false).await, start(&db, "b", false).await);
    let id = a.create("hub").await.unwrap();
    let ch = a.view(&id, |s| s.channels()[0].0).unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    b.add_storage(&s.storage_code().await.unwrap()).await.unwrap();
    let url = b.irpush().await.expect("joined its storage node's mesh");
    assert_eq!(url, format!("irpush:{}", dev.id()), "the one device");
    assert!(dev.has(&other.id()), "still in the other app's mesh");
    b.set_push(Some(url)).await;
    assert!(b.pushing(), "its storage node took the device");
    let ring = |p: &(iroh::EndpointId, String)| p.1.starts_with("irch:ring:");
    let told = || a.view(&id, |s| s.pushes_of(None).iter().any(ring));
    until("a knows b's push server", || told() == Some(true)).await;
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);

    let op =
        Op::Post { channel: ch, text: "hi".into(), reply: None, files: vec![] };
    a.publish(&id, op).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(10), rx.recv());
    let got = got.await.expect("pushed within seconds").unwrap();
    let (sp, raw) = got.split_first_chunk::<32>().unwrap();
    let news = (irch_core::Id(*sp), raw.to_vec());
    let n = Node::woken(&db, &news).0.expect("shown from the phonebook");
    assert_eq!((n.space, &n.body[..]), (id, "hi"));
}
