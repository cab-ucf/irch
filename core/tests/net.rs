//! Several nodes on this machine, talking over real QUIC.

use irch_core::{Ch, Config, Event, Id, Invite, Net, Node, Op};
use std::sync::Arc;
use std::time::Duration;

fn dir(name: &str) -> std::path::PathBuf {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    std::env::temp_dir()
        .join(format!("irch-test-{}-{name}", t.unwrap().as_nanos()))
}

async fn start(
    dir: &std::path::Path,
    name: &str,
    link: Option<String>,
    serve: bool,
) -> Arc<Node> {
    let filter = tracing_subscriber::EnvFilter::from_default_env();
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
    let cfg = Config {
        dir: dir.into(),
        name: name.into(),
        net: Net::Direct,
        link,
        serve,
    };
    Node::start(cfg).await.unwrap()
}

async fn node(name: &str) -> Arc<Node> {
    start(&dir(name), name, None, false).await
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

fn general(n: &Node, id: &Id) -> Ch {
    n.view(id, |s| s.channels()[0].0).unwrap()
}

fn texts(n: &Node, id: &Id, ch: &Ch) -> Vec<String> {
    let m = n.view(id, |s| s.messages(ch, None, 100)).unwrap_or_default();
    m.into_iter().map(|m| m.text).collect()
}

async fn say(n: &Arc<Node>, id: &Id, ch: Ch, text: &str) -> Id {
    let op =
        Op::Post { channel: ch, text: text.into(), reply: None, files: vec![] };
    n.publish(id, op).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn join_sync_and_live() {
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("home").await.unwrap();
    let ch = general(&a, &id);
    let hello = say(&a, &id, ch, "hello").await;
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    until("history", || texts(&b, &id, &ch) == ["hello"]).await;
    assert_eq!(b.view(&id, |s| s.name().to_string()).unwrap(), "home");
    assert_eq!(
        b.unread(&id, &ch),
        (0, 0),
        "history before joining starts read"
    );

    let reply = Op::Post {
        channel: ch,
        text: "@a hi".into(),
        reply: Some(hello),
        files: vec![],
    };
    b.publish(&id, reply).await.unwrap();
    until("live reply", || texts(&a, &id, &ch).len() == 2).await;
    let m = a.view(&id, |s| s.messages(&ch, None, 9)).unwrap();
    assert_eq!(m[1].quote.as_ref().unwrap().text, "hello");
    assert_eq!(a.unread(&id, &ch), (1, 1));
    assert_eq!(a.mentions(&id, 0).len(), 1);
    a.mark_read(&id, &ch);
    assert_eq!(a.unread(&id, &ch), (0, 0));

    let react = Op::React { target: hello, emoji: "🎉".into(), on: true };
    b.publish(&id, react).await.unwrap();
    until("reaction", || {
        let m = a.view(&id, |s| s.messages(&ch, None, 9)).unwrap();
        m[0].reactions == [("🎉".to_string(), vec![b.id()])]
    })
    .await;
    let names = a.view(&id, |s| {
        s.members().into_iter().map(|m| m.name).collect::<Vec<_>>()
    });
    assert_eq!(names.unwrap(), ["a", "b"]);

    let mut ev = b.subscribe();
    a.typing(&id, ch).await;
    let typing = async {
        loop {
            if let Ok(Event::Typing { user, .. }) = ev.recv().await {
                return user;
            }
        }
    };
    let who =
        tokio::time::timeout(Duration::from_secs(10), typing).await.unwrap();
    assert_eq!(who, a.id());
    let edit = Op::Edit { target: hello, text: Some("hello!".into()) };
    assert!(b.publish(&id, edit.clone()).await.is_ok(), "anyone may send it");
    a.publish(&id, edit).await.unwrap();
    until("edit", || texts(&b, &id, &ch)[0] == "hello!").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ban_rekeys_and_history_survives() {
    let (a, b, c) = (node("a").await, node("b").await, node("c").await);
    let id = a.create("club").await.unwrap();
    let ch = general(&a, &id);
    say(&a, &id, ch, "before").await;
    let code = a.invite(&id).await.unwrap();
    b.join(&code).await.unwrap();
    c.join(&code).await.unwrap();
    let keyed = |u: Id| a.view(&id, |s| s.kem(&u).is_some()) == Some(true);
    until("profiles", || keyed(b.id()) && keyed(c.id())).await;

    a.ban(&id, &b.id(), true).await.unwrap();
    until("b locked out", || b.locked(&id)).await;
    assert!(!c.locked(&id));
    say(&a, &id, ch, "after").await;
    until("c reads on", || texts(&c, &id, &ch) == ["before", "after"]).await;
    assert!(
        b.publish(
            &id,
            Op::Post {
                channel: ch,
                text: "x".into(),
                reply: None,
                files: vec![]
            }
        )
        .await
        .is_err()
    );
    assert!(!a.invite(&id).await.unwrap().is_empty());

    let d = node("d").await;
    d.join(&a.invite(&id).await.unwrap()).await.unwrap();
    until("d sees all", || texts(&d, &id, &ch) == ["before", "after"]).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(texts(&b, &id, &ch), ["before"], "b never reads the new epoch");

    a.ban(&id, &b.id(), false).await.unwrap();
    assert!(b.locked(&id), "unbanned, b still lacks the key");
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    assert!(!b.locked(&id), "a fresh invite carries it");
    until("b reads on", || texts(&b, &id, &ch) == ["before", "after"]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_id_alone_gets_nothing_stored() {
    let s = start(&dir("s"), "s", None, true).await;
    let a = node("a").await;
    let id = a.create("home").await.unwrap();
    say(&a, &id, general(&a, &id), "kept").await;
    let code = s.storage_code().await.unwrap();
    a.add_storage(&code).await.unwrap();
    let held =
        |n: &Node| n.held().iter().find(|h| h.0 == id).map_or(0, |h| h.1);
    until("storage holds it", || held(&s) == held(&a)).await;
    let before = held(&a);
    // x knows the space and the storage node, but not the key.
    let (store, _): (iroh::EndpointAddr, [u8; 16]) =
        irch_core::op::uncode("host", &code).unwrap();
    let mut inv = Invite::decode(&a.invite(&id).await.unwrap()).unwrap();
    (inv.key, inv.peers) = ([7; 32], vec![store]);
    let x = node("x").await;
    x.join(&inv.encode()).await.unwrap();
    say(&x, &id, Ch([1; 8]), "junk").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!((held(&a), held(&s)), (before, before));
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_messages() {
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("hub").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    until("b's key", || {
        a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true)
    })
    .await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    assert_eq!(a.dm(&id, &b.id()).await.unwrap(), dm, "one DM per pair");
    say(&a, &dm, irch_core::state::DM, "psst").await;
    until("b got the DM", || texts(&b, &dm, &irch_core::state::DM) == ["psst"])
        .await;
    assert!(b.spaces().contains(&dm));
    say(&b, &dm, irch_core::state::DM, "hey").await;
    until("a got the answer", || {
        texts(&a, &dm, &irch_core::state::DM).len() == 2
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_node_and_restart() {
    let s = start(&dir("s"), "s", None, true).await;
    let (a, bdir) = (node("a").await, dir("b"));
    let b = start(&bdir, "b", None, false).await;
    let id = a.create("home").await.unwrap();
    let ch = general(&a, &id);
    a.add_storage(&s.storage_code().await.unwrap()).await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    until("b joined", || {
        a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true)
    })
    .await;
    b.shutdown().await;
    drop(b);

    say(&a, &id, ch, "while you were away").await;
    let held =
        |n: &Node| n.held().iter().find(|h| h.0 == id).map_or(0, |h| h.1);
    until("storage holds it all", || held(&s) == held(&a)).await;
    a.shutdown().await;
    drop(a);

    let b = start(&bdir, "b", None, false).await;
    until("b caught up from storage", || {
        texts(&b, &id, &ch).contains(&"while you were away".to_string())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn link_a_device() {
    let a = node("a").await;
    let id = a.create("home").await.unwrap();
    let ch = general(&a, &id);
    say(&a, &id, ch, "mine").await;
    let a2 = start(&dir("a2"), "phone", Some(a.link_code().await), false).await;
    assert_eq!(a2.id(), a.id());
    until("a2 has the space", || texts(&a2, &id, &ch) == ["mine"]).await;
    assert_eq!(a2.name(), "a");
    say(&a2, &id, ch, "from my phone").await;
    until("a sees it", || texts(&a, &id, &ch).len() == 2).await;
    let other = a2.create("second").await.unwrap();
    until("a joins it too", || a.spaces().contains(&other)).await;
    a2.leave(&other).await.unwrap();
    until("a leaves too", || !a.spaces().contains(&other)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn link_while_offline_through_storage() {
    let s = start(&dir("s"), "s", None, true).await;
    let a = node("a").await;
    let id = a.create("home").await.unwrap();
    let ch = general(&a, &id);
    say(&a, &id, ch, "kept safe").await;
    a.add_storage(&s.storage_code().await.unwrap()).await.unwrap();
    let held =
        |n: &Node, id: &Id| n.held().iter().find(|h| h.0 == *id).map(|h| h.1);
    until("storage holds both spaces", || {
        s.held().len() == 2 && held(&s, &id) == held(&a, &id)
    })
    .await;
    let code = a.link_code().await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.shutdown().await;
    let a2 = start(&dir("a2"), "phone", Some(code), false).await;
    until("a2 restored from storage", || texts(&a2, &id, &ch) == ["kept safe"])
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_live_on_storage_too() {
    let s = start(&dir("s"), "s", None, true).await;
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("home").await.unwrap();
    let ch = general(&a, &id);
    a.add_storage(&s.storage_code().await.unwrap()).await.unwrap();
    let code = a.invite(&id).await.unwrap();
    b.join(&code).await.unwrap();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
    let f = a.attach("cat.png", &data).await.unwrap();
    let files = vec![f.clone()];
    let op = Op::Post { channel: ch, text: String::new(), reply: None, files };
    a.publish(&id, op).await.unwrap();
    let first = |n: &Node| {
        let m = n.view(&id, |s| s.messages(&ch, None, 9)).unwrap_or_default();
        m.into_iter().find_map(|m| m.files.first().cloned())
    };
    until("b sees the file", || first(&b).is_some()).await;
    assert_eq!(first(&b).unwrap(), f);
    assert_eq!(b.file(&id, &f).await.unwrap(), data, "fetched from a");
    let out = dir("saved");
    let evil = irch_core::File { name: "../../.cat.png".into(), ..f.clone() };
    let p1 = b.save(&id, &evil, &out).await.unwrap();
    let p2 = b.save(&id, &evil, &out).await.unwrap();
    assert_eq!(
        (p1.parent(), p1.file_name()),
        (Some(&*out), Some("cat.png".as_ref()))
    );
    assert_eq!(p2.file_name(), Some("cat (1).png".as_ref()));
    assert_eq!(std::fs::read(p2).unwrap(), data);
    let held = |n: &Node| n.held().iter().find(|h| h.0 == id).map(|h| h.1);
    until("storage caught up", || held(&s) == held(&a)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    a.shutdown().await;
    b.shutdown().await;
    let c = node("c").await;
    c.join(&code).await.unwrap();
    until("c sees the file", || first(&c).is_some()).await;
    assert_eq!(c.file(&id, &f).await.unwrap(), data, "fetched from storage");
}
