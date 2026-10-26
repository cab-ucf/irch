//! Several nodes on this machine, talking over real QUIC.

use irch_core::{Ch, Config, Event, Id, Invite, Net, Node, Op};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::broadcast::Receiver;

// A clock alone is not enough: macOS's ticks in microseconds, and tests
// start nodes together.
fn dir(name: &str) -> std::path::PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let t = t.unwrap().as_nanos();
    std::env::temp_dir().join(format!("irch-test-{t}-{n}-{name}"))
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
        notes: None,
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
async fn an_old_invite_stays_dead_after_a_reset() {
    let a = node("a").await;
    let id = a.create("club").await.unwrap();
    let ch = general(&a, &id);
    let old = a.invite(&id).await.unwrap();
    a.rekey(&id).await.unwrap(); // Reset invite links
    let e = node("e").await;
    // Its Profile goes out under the old key, unless the reset reaches it
    // first (a fast machine), and joining is refused.
    if let Err(err) = e.join(&old).await {
        assert!(err.to_string().contains("ask for a new invite"), "{err}");
    }
    until("e learns it is out", || e.locked(&id)).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let member = |u: Id| a.view(&id, |s| s.kem(&u).is_some()) == Some(true);
    assert!(!member(e.id()), "a stale key makes no member");
    a.rekey(&id).await.unwrap(); // was boxed to every member with a kem
    say(&a, &id, ch, "secret").await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(e.locked(&id) && texts(&e, &id, &ch).is_empty());
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
async fn a_dm_call_rings_once() {
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("hub").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    say(&a, &dm, irch_core::state::DM, "psst").await;
    until("b in the DM", || b.spaces().contains(&dm)).await;
    b.notices(); // the message, told
    a.call(Some((dm, irch_core::state::DM))).await.unwrap();
    until("b sees the call", || !b.calls(&dm).is_empty()).await;
    let rings = |n: &Node| n.notices().into_iter().filter(|n| n.ring).count();
    assert_eq!(rings(&b), 1, "a DM call rings");
    assert_eq!(rings(&b), 0, "once");
    assert_eq!(rings(&a), 0, "not the caller");
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

#[tokio::test(flavor = "multi_thread")]
async fn calls_carry_frames_between_members_only() {
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("home").await.unwrap();
    let ch = general(&a, &id);
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let (mut at_a, mut at_b) = (a.media(), b.media());
    a.call(Some((id, ch))).await.unwrap();
    b.call(Some((id, ch))).await.unwrap();
    let n = |x: &Node| x.calls(&id).get(&ch).map_or(0, Vec::len);
    until("both in the call", || n(&a) == 2 && n(&b) == 2).await;
    // Video goes in groups, from a keyframe; audio as datagrams.
    let hear = async |from: &Arc<Node>, rx: &mut Receiver<_>, tries| {
        for _ in 0..tries {
            from.send_media(&[2, 9]);
            from.send_media(&[1, 8]);
            from.send_media(&[0, 7]);
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut got = vec![];
            while let Ok((who, f)) = rx.try_recv() {
                got.push((who, f));
            }
            if got.iter().any(|(w, f)| *w == from.id() && f == &[0, 7])
                && got.iter().any(|(w, f)| *w == from.id() && f == &[1, 8])
            {
                return true;
            }
        }
        false
    };
    assert!(hear(&a, &mut at_b, 200).await, "b hears a");
    assert!(hear(&b, &mut at_a, 200).await, "a hears b");

    a.ban(&id, &b.id(), true).await.unwrap();
    until("b out of a's call", || n(&a) == 1).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    while at_a.try_recv().is_ok() {}
    assert!(!hear(&b, &mut at_a, 40).await, "a banned member is unheard");
    a.call(None).await.unwrap();
    assert!(a.calls(&id).is_empty() && a.in_call().is_none());
}

/// b's phone sleeps: a, writing, wakes it through the endpoint b told the
/// space (a ping with nothing in it), and b then notifies once.
#[tokio::test(flavor = "multi_thread")]
async fn writing_wakes_a_sleeping_device() {
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A push server (ntfy, say) that counts what it is asked to deliver.
    let pushes = Arc::new(AtomicUsize::new(0));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/up", l.local_addr().unwrap());
    let p = pushes.clone();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = l.accept().await {
            let mut b = [0; 4096];
            let n = c.read(&mut b).await.unwrap_or(0);
            if b[..n].starts_with(b"POST /up") && b[..n].ends_with(b"\r\n\r\n1")
            {
                p.fetch_add(1, Relaxed);
            }
            let ok = b"HTTP/1.1 201 Created\r\ncontent-length: 0\r\n\r\n";
            let _ = c.write_all(ok).await;
        }
    });
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("lab").await.unwrap();
    let ch = general(&a, &id);
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    b.set_push(Some(url.clone())).await;
    let told = |n: &Node| n.view(&id, |s| s.pushes().len()) == Some(1);
    until("a knows where to wake b", || told(&a)).await;
    assert!(b.notices().is_empty(), "history stays quiet");

    say(&a, &id, ch, "@b the samples are in").await;
    until("a woke b", || pushes.load(Relaxed) == 1).await;
    until("b has it", || !texts(&b, &id, &ch).is_empty()).await;
    let n = b.notices();
    assert_eq!(n.len(), 1);
    assert_eq!((n[0].room, &*n[0].body), (ch, "@b the samples are in"));
    assert!(n[0].title.starts_with("a in #general"));
    assert!(b.notices().is_empty(), "once");

    say(&a, &id, ch, "and the scope is free").await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(pushes.load(Relaxed), 1, "held back for 30 s, not dropped");
    b.set_level(&id, irch_core::Level::Mentions);
    until("b has it", || texts(&b, &id, &ch).len() == 2).await;
    assert!(b.notices().is_empty(), "mentions only");
}
