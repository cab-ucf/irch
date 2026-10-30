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
        big: None,
    };
    Node::start(cfg).await.unwrap()
}

/// Pair `a` with storage node `s`, which then keeps `spaces` too.
async fn pair(a: &Arc<Node>, s: &Node, spaces: &[Id]) -> String {
    let code = s.storage_code().await.unwrap();
    a.add_storage(&code).await.unwrap();
    let (addr, _): (iroh::EndpointAddr, [u8; 16]) =
        irch_core::op::uncode("host", &code).unwrap();
    for id in spaces {
        a.store(id, &addr.id, true).await.unwrap();
    }
    code
}

async fn node(name: &str) -> Arc<Node> {
    start(&dir(name), name, None, false).await
}

async fn until(what: &str, f: impl Fn() -> bool) {
    // Forty seconds: a busy machine runs every test at once.
    for _ in 0..800 {
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
    let code = pair(&a, &s, &[id]).await;
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
    a.hide(&dm, true);
    assert!(a.hidden(&dm) && a.spaces().contains(&dm), "closed, not gone");
    say(&b, &dm, irch_core::state::DM, "hey").await;
    until("a got the answer", || {
        texts(&a, &dm, &irch_core::state::DM).len() == 2
    })
    .await;
    assert!(!a.hidden(&dm), "back with its next message");
    a.hide(&dm, true);
    a.dm(&id, &b.id()).await.unwrap();
    assert!(!a.hidden(&dm), "back when opened");
}

/// A DM's very first message is news, though it came before b was in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_new_dm_notifies() {
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("hub").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let _ = b.notices(); // the hub's history: quiet
    let dm = a.dm(&id, &b.id()).await.unwrap();
    say(&a, &dm, irch_core::state::DM, "are you there?").await;
    until("b got the DM", || !texts(&b, &dm, &irch_core::state::DM).is_empty())
        .await;
    let n = b.notices();
    assert_eq!(
        n.iter().map(|n| &*n.body).collect::<Vec<_>>(),
        ["are you there?"]
    );
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
    b.call(Some((id, general(&b, &id)))).await.unwrap(); // call waiting
    a.call(Some((dm, irch_core::state::DM))).await.unwrap();
    until("b sees the call", || !b.calls(&dm).is_empty()).await;
    // The DM's rings (b's call in the hub, a space of two, rings for a).
    let rings = |n: &Node| {
        n.notices().into_iter().filter(|x| x.ring && x.space == dm).count()
    };
    assert_eq!(rings(&b), 1, "a DM call rings");
    assert_eq!(rings(&b), 0, "once");
    assert_eq!(rings(&a), 0, "not the caller");
    // The caller hears that b's device has it, and that it rings there.
    let dmc = irch_core::state::DM;
    until("a knows b has it", || a.reach(&dm, &b.id()).is_some()).await;
    assert!(!a.rings(&dm, &b.id(), &dmc), "not yet said");
    // Said again and again, as the app does, until the DM's gossip forms.
    for _ in 0..80 {
        b.ringing(&dm, dmc).await;
        if a.rings(&dm, &b.id(), &dmc) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(a.rings(&dm, &b.id(), &dmc), "a hears it ring");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ring_tells_a_sleeping_phone_what_rang() {
    use irch_core::{state::DM, wake};
    let db = dir("b");
    let a = node("a").await;
    let b = start(&db, "b", None, false).await;
    let id = a.create("lab").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    until("b in the DM", || b.spaces().contains(&dm)).await;
    let url = wake::doorbell(&db, &Net::Direct).await.unwrap();
    b.set_push(Some(url.clone())).await;
    // Each space has its own ring code, made from b's doorbell.
    let ring = |p: &(iroh::EndpointId, String)| p.1.starts_with("irch:ring:");
    let told = |s: &Id| {
        a.view(s, |st| st.pushes_of(None).iter().any(ring)) == Some(true)
    };
    until("a knows b's doorbell", || told(&id) && told(&dm)).await;
    b.notices(); // asked once: what comes next is news
    b.shutdown().await;
    drop(b);

    let rang = |d: std::path::PathBuf| {
        tokio::spawn(async move { wake::rung(&d, &Net::Direct).await })
    };
    let ten = Duration::from_secs(10);
    let phone = rang(db.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&a, &dm, DM, "lunch?").await;
    let said = tokio::time::timeout(ten, phone).await.unwrap().unwrap();
    let said = said.unwrap().expect("the ring is the message");
    let phone = rang(db.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.call(Some((dm, DM))).await.unwrap();
    let call = tokio::time::timeout(ten, phone).await.unwrap().unwrap();
    let call = call.unwrap().expect("the ring is the call");
    let phone = rang(db.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.call(None).await.unwrap();
    let ended = tokio::time::timeout(ten, phone).await.unwrap().unwrap();
    let ended = ended.unwrap().expect("hanging up rings too");

    // Up again, b tells of both at once: a call is said only every 15 s,
    // so it can only have come with the ring.
    let b = start(&db, "b", None, false).await;
    let rings = |n: &Node| n.notices().into_iter().filter(|n| n.ring).count();
    assert_eq!(rings(&b), 0, "not yet");
    b.deliver(said);
    b.deliver(call);
    let n = b.notices();
    assert!(n.iter().any(|n| n.ring), "the call rings: {n:?}");
    b.deliver(ended);
    assert!(b.calls(&dm).is_empty(), "and stops when hung up");
    until("the message, told", || {
        texts(&b, &dm, &DM).contains(&"lunch?".to_string())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_phone_finds_its_storage_node_back() {
    let (db, ds) = (dir("b"), dir("s"));
    let a = node("a").await;
    let s = start(&ds, "s", None, true).await;
    let b = start(&db, "b", None, false).await;
    let id = a.create("lab").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    pair(&a, &s, &[id]).await;
    until("s keeps it", || s.held().iter().any(|h| h.0 == id)).await;
    let hosted = || b.view(&id, |st| !st.hosts().is_empty()) == Some(true);
    until("b knows the storage node", hosted).await;
    for n in [&a, &b, &s] {
        n.shutdown().await;
    }
    drop((a, b, s));
    tokio::time::sleep(Duration::from_secs(6)).await; // their ports, free

    // b is back first, and no one answers; then the storage node, which
    // cannot know where b is now: b must try again.
    let b = start(&db, "b", None, false).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(b.peers(&id), 0, "no one to find");
    let _s = start(&ds, "s", None, true).await;
    until("b finds it again", || b.peers(&id) > 0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_keeps_only_the_spaces_chosen() {
    let s = start(&dir("s"), "s", None, true).await;
    let a = node("a").await;
    let (kept, not) =
        (a.create("lab").await.unwrap(), a.create("x").await.unwrap());
    say(&a, &kept, general(&a, &kept), "keep me").await;
    say(&a, &not, general(&a, &not), "not me").await;
    let code = pair(&a, &s, &[kept]).await;
    let has = |id: Id| s.held().iter().any(|h| h.0 == id);
    until("the chosen space is kept", || has(kept)).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(!has(not), "a space not chosen stays off the storage node");
    let (addr, _): (iroh::EndpointAddr, [u8; 16]) =
        irch_core::op::uncode("host", &code).unwrap();
    a.store(&not, &addr.id, true).await.unwrap();
    until("and once chosen, kept too", || has(not)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn offered_files_go_from_device_to_members_only() {
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("home").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    // Past what storage nodes take: kept here, served from here.
    let big: Vec<u8> = (0..30u32 << 20).map(|i| (i % 251) as u8).collect();
    let src = dir("src").join("big.bin");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(&src, &big).unwrap();
    let f = a.offer(&id, &src, None).await.unwrap();
    assert!(f.offered() && f.size == big.len() as u64 && f.name == "big.bin");
    // Said to be a byte, to open in place: refused before any of it comes.
    let tiny = irch_core::File { size: 1, ..f.clone() };
    assert!(b.file(&id, &tiny).await.is_err(), "not the size it says");
    let saved = b.save(&id, &f, &dir("saved")).await.unwrap();
    assert!(std::fs::read(&saved).unwrap() == big, "b has it, whole");
    // x knows the space, the hash and a's address, but holds no key.
    let mut inv = Invite::decode(&a.invite(&id).await.unwrap()).unwrap();
    inv.key = [7; 32];
    let x = node("x").await;
    x.join(&inv.encode()).await.unwrap();
    let out = dir("x");
    assert!(x.save(&id, &f, &out).await.is_err(), "no member, no file");
    assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0, "nothing left");
    // b edits what it saved: what b serves on is still what a shared.
    std::fs::write(&saved, vec![0; big.len()]).unwrap();
    let c = node("c").await;
    c.join(&b.invite(&id).await.unwrap()).await.unwrap();
    let knows = || b.view(&id, |s| s.kem(&c.id()).is_some()) == Some(true);
    until("b knows c", knows).await;
    a.shutdown().await;
    let got = c.save(&id, &f, &dir("c")).await.unwrap();
    assert!(std::fs::read(got).unwrap() == big, "from b, as shared");
    // Stopped: no longer listed, nor served from b to this space's others.
    assert!(b.sharing().iter().any(|x| x.0 == f.hash && x.1 == id));
    b.unshare(&f.hash).await.unwrap();
    assert!(b.sharing().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_node_and_restart() {
    let s = start(&dir("s"), "s", None, true).await;
    let (a, bdir) = (node("a").await, dir("b"));
    let b = start(&bdir, "b", None, false).await;
    let id = a.create("home").await.unwrap();
    let ch = general(&a, &id);
    pair(&a, &s, &[id]).await;
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
    pair(&a, &s, &[id]).await;
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
    pair(&a, &s, &[id]).await;
    let code = a.invite(&id).await.unwrap();
    b.join(&code).await.unwrap();
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
    let f = a.attach(&id, "cat.png", &data).await.unwrap();
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
    let told = |n: &Node| n.view(&id, |s| s.pushes_of(None).len()) == Some(1);
    until("a knows where to wake b", || told(&a)).await;
    assert!(b.notices().is_empty(), "history stays quiet");
    let named = |n: &Node| n.view(&id, |s| s.profile(&a.id()).is_some());
    until("b knows a by name", || named(&b) == Some(true)).await;
    // Later than that first ask, not in its very millisecond.
    tokio::time::sleep(Duration::from_millis(2)).await;

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

/// b's phone sleeps: its node stopped, only its doorbell left, which a
/// rings when it writes. No server, no router, nothing but iroh.
#[tokio::test(flavor = "multi_thread")]
async fn writing_rings_a_sleeping_phone() {
    use irch_core::wake;
    let db = dir("b");
    let a = node("a").await;
    let b = start(&db, "b", None, false).await;
    let id = a.create("lab").await.unwrap();
    let ch = general(&a, &id);
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let url = wake::doorbell(&db, &Net::Direct).await.unwrap();
    b.set_push(Some(url.clone())).await;
    let ring = |p: &(iroh::EndpointId, String)| p.1.starts_with("irch:ring:");
    let told = |n: &Node| n.view(&id, |s| s.pushes_of(None).iter().any(ring));
    until("a knows b's doorbell", || told(&a) == Some(true)).await;
    // Its key too, which a DM to it needs (it travels apart).
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let b_id = b.id();
    b.shutdown().await;
    drop(b);

    let d = db.clone();
    let phone = tokio::spawn(async move { wake::rung(&d, &Net::Direct).await });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!phone.is_finished(), "it sleeps");
    say(&a, &id, ch, "lunch?").await;
    let woke = tokio::time::timeout(Duration::from_secs(10), phone).await;
    assert!(woke.expect("rung within seconds").unwrap().is_ok());

    // Asleep again: a new DM (and a call in it) rings it too, though b's
    // doorbell is told only to spaces b is in.
    let d = db.clone();
    let phone = tokio::spawn(async move { wake::rung(&d, &Net::Direct).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!phone.is_finished());
    a.dm(&id, &b_id).await.unwrap();
    let woke = tokio::time::timeout(Duration::from_secs(10), phone).await;
    assert!(woke.expect("a new DM rings").unwrap().is_ok());
}

/// A call to a sleeping phone shows the moment its doorbell rings, from
/// what its node left at its nap: no node starts first. Hung up, it stops.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_shows_on_a_sleeping_phone_at_once() {
    use irch_core::{state::DM, wake};
    let db = dir("b");
    let a = node("a").await;
    let b = start(&db, "b", None, false).await;
    let id = a.create("hub").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    until("b in the DM", || b.spaces().contains(&dm)).await;
    let url = wake::doorbell(&db, &Net::Direct).await.unwrap();
    b.set_push(Some(url)).await;
    let ring = |p: &(iroh::EndpointId, String)| p.1.starts_with("irch:ring:");
    let told = || a.view(&dm, |s| s.pushes_of(None).iter().any(ring));
    until("a knows b's doorbell in the DM", || told() == Some(true)).await;
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);

    let d = db.clone();
    let phone = tokio::spawn(async move { wake::rung(&d, &Net::Direct).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let t = std::time::Instant::now();
    a.call(Some((dm, DM))).await.unwrap();
    let news = phone.await.unwrap().unwrap().expect("the call, sealed");
    let (n, node_needed) = Node::woken(&db, &news);
    let (n, ms) = (n.expect("shown at once"), t.elapsed().as_millis());
    assert!(!node_needed, "no node needed");
    assert!(ms < 2000, "{ms} ms");
    assert!(n.ring && !n.hush, "rings");
    assert_eq!((n.space, n.room), (dm, DM));
    assert_eq!((&n.title[..], &n.body[..]), ("a", "is calling you"));

    let d = db.clone();
    let phone = tokio::spawn(async move { wake::rung(&d, &Net::Direct).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    a.call(None).await.unwrap();
    let news = phone.await.unwrap().unwrap().expect("its end");
    assert!(Node::woken(&db, &news).0.expect("hushed").hush);
}

/// A message to a sleeping phone shows from its ring alone and is kept
/// unopened: once b starts again, it is there before any peer is, and not
/// told a second time.
#[tokio::test(flavor = "multi_thread")]
async fn a_message_shows_and_keeps_on_a_sleeping_phone() {
    use irch_core::wake;
    let db = dir("b");
    let a = node("a").await;
    let b = start(&db, "b", None, false).await;
    let id = a.create("hub").await.unwrap();
    let ch = general(&a, &id);
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let url = wake::doorbell(&db, &Net::Direct).await.unwrap();
    b.set_push(Some(url)).await;
    let ring = |p: &(iroh::EndpointId, String)| p.1.starts_with("irch:ring:");
    let told = || a.view(&id, |s| s.pushes_of(None).iter().any(ring));
    until("a knows b's doorbell", || told() == Some(true)).await;
    // Whom its phonebook names: else the node starts, to learn who.
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);

    let d = db.clone();
    let phone = tokio::spawn(async move { wake::rung(&d, &Net::Direct).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    say(&a, &id, ch, "lunch?").await;
    let news = phone.await.unwrap().unwrap().expect("the message, sealed");
    let (n, node_needed) = Node::woken(&db, &news);
    let n = n.expect("shown at once");
    assert_eq!((&n.title[..], &n.body[..]), ("a in #general (hub)", "lunch?"));
    assert!(!n.ring && n.room == ch && !node_needed, "kept, no node needed");

    a.shutdown().await;
    drop(a);
    let b = start(&db, "b", None, false).await;
    assert_eq!(texts(&b, &id, &ch), ["lunch?"], "from its own log");
    assert!(b.notices().is_empty(), "told once");
}

/// Google's token and send endpoints, as plain http here: each message
/// sent comes out of the receiver, request and all.
async fn fake_google() -> (String, tokio::sync::mpsc::UnboundedReceiver<String>)
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut c, _)) = l.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let (mut b, mut buf) = (vec![], [0; 4096]);
                let mut want = usize::MAX;
                while b.len() < want {
                    let n = c.read(&mut buf).await.unwrap();
                    b.extend_from_slice(&buf[..n]);
                    let end = b.windows(4).position(|w| w == b"\r\n\r\n");
                    if let Some(i) = end.filter(|_| want == usize::MAX) {
                        let head =
                            String::from_utf8_lossy(&b[..i]).to_lowercase();
                        let len = head.lines().find_map(|l| {
                            l.strip_prefix("content-length:")?
                                .trim()
                                .parse()
                                .ok()
                        });
                        want = i + 4 + len.unwrap_or(0);
                    }
                }
                let req = String::from_utf8_lossy(&b).to_string();
                let reply = match req.starts_with("POST /token") {
                    true => r#"{"access_token":"t","expires_in":3600}"#,
                    false => {
                        let _ = tx.send(req);
                        "{}"
                    }
                };
                let r = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\
                     connection: close\r\n\r\n{reply}",
                    reply.len()
                );
                c.write_all(r.as_bytes()).await.unwrap();
            });
        }
    });
    (url, rx)
}

/// A storage node with a Firebase project rings a sleeping phone through
/// Google: the phone finds it among its storage nodes, registers, and each
/// ring lands sealed to it, a call or a message alike in size.
#[tokio::test(flavor = "multi_thread")]
async fn a_push_server_rings_a_phone_through_google() {
    use irch_core::{push, state::DM, wake};
    let (google, mut sent) = fake_google().await;
    let sd = dir("s");
    std::fs::create_dir_all(&sd).unwrap();
    let account = serde_json::json!({
        "project_id": "p", "client_email": "irch@p",
        "private_key": include_str!("data/test-key.pem"),
        "token_uri": format!("{google}/token"), "fcm_uri": google,
    });
    let app = serde_json::json!({
        "project_info": { "project_id": "p", "project_number": "42" },
        "client": [{ "api_key": [{ "current_key": "AIza" }], "client_info": {
            "mobilesdk_app_id": "1:42:android:7",
            "android_client_info": { "package_name": "dev.irch.chat" } } }],
    });
    std::fs::write(sd.join("fcm.json"), account.to_string()).unwrap();
    std::fs::write(sd.join("google-services.json"), app.to_string()).unwrap();
    let s = start(&sd, "s", None, true).await;
    let db = dir("b");
    let (a, b) = (node("a").await, start(&db, "b", None, false).await);
    let id = a.create("hub").await.unwrap();
    let ch = general(&a, &id);
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    pair(&b, &s, &[]).await;
    let (f, at) = b.firebase().await.expect("its storage node pushes");
    assert_eq!(
        (&f.app[..], &f.sender[..], at.len()),
        ("1:42:android:7", "42", 1)
    );
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    until("b in the DM", || b.spaces().contains(&dm)).await;
    b.set_push(Some("fcm:TOKEN".into())).await;
    let ring = |p: &(iroh::EndpointId, String)| p.1.starts_with("irch:ring:");
    let told = |sp| a.view(&sp, |s| s.pushes_of(None).iter().any(ring));
    until("a knows b's push server", || told(dm) == Some(true)).await;
    until("in the hub too", || told(id) == Some(true)).await;
    let named = || b.view(&id, |s| s.profile(&a.id()).is_some()) == Some(true);
    until("b knows a", named).await;
    b.shutdown().await;
    drop(b);

    let key = wake::push_key(&db).unwrap();
    let mut next = async || {
        let req = tokio::time::timeout(Duration::from_secs(10), sent.recv());
        let req = req.await.expect("sent within seconds").unwrap();
        assert!(req.starts_with("POST /v1/projects/p/messages:send"), "{req}");
        assert!(req.to_lowercase().contains("authorization: bearer t"));
        let body = &req[req.find("\r\n\r\n").unwrap() + 4..];
        let m: serde_json::Value = serde_json::from_str(body).unwrap();
        let m = &m["message"];
        assert_eq!(
            (&m["token"], &m["android"]["priority"]),
            (&"TOKEN".into(), &"high".into())
        );
        assert_eq!(m["data"].as_object().unwrap().len(), 1, "the ring alone");
        m["data"]["r"].as_str().unwrap().to_string()
    };
    a.call(Some((dm, DM))).await.unwrap();
    let call = next().await;
    let open = |r: &str| {
        let b = data_encoding::BASE64.decode(r.as_bytes()).unwrap();
        let news = push::open(&key, &b).expect("sealed to b");
        Node::woken(&db, &news).0.expect("shown from the phonebook")
    };
    let n = open(&call);
    assert!(n.ring && n.space == dm);
    assert_eq!((&n.title[..], &n.body[..]), ("a", "is calling you"));
    say(&a, &id, ch, "hi").await;
    let hi = next().await;
    assert_eq!(hi.len(), call.len(), "a message looks as a call does");
    assert_eq!(open(&hi).body, "hi");
}

/// A member's photo waits on the space's storage node, though it is not
/// theirs: everyone sees it while its sender is away.
#[tokio::test(flavor = "multi_thread")]
async fn a_photo_waits_on_the_spaces_storage_node() {
    let s = start(&dir("s"), "s", None, true).await;
    let (a, b, c) = (node("a").await, node("b").await, node("c").await);
    let id = a.create("lab").await.unwrap();
    pair(&a, &s, &[id]).await;
    for n in [&b, &c] {
        n.join(&a.invite(&id).await.unwrap()).await.unwrap();
    }
    let held = || b.view(&id, |st| st.holds().len()) == Some(1);
    until("b learns how s keeps small files", held).await;
    let f = b.attach(&id, "cat.png", b"a photo").await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await; // s fetches it
    b.shutdown().await;
    drop(b);
    assert_eq!(c.file(&id, &f).await.unwrap(), b"a photo", "from s");
}

/// A call in a fresh DM: both sides gossip in it, the callee sees the
/// call, and once it answers, each hears the other.
#[tokio::test(flavor = "multi_thread")]
async fn a_dm_call_connects_both_ways() {
    use irch_core::state::DM;
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("hub").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let dm = a.dm(&id, &b.id()).await.unwrap();
    until("b in the DM", || b.spaces().contains(&dm)).await;
    until("a gossips in the DM", || a.peers(&dm) > 0).await;
    until("b gossips in the DM", || b.peers(&dm) > 0).await;
    let (mut at_a, mut at_b) = (a.media(), b.media());
    a.call(Some((dm, DM))).await.unwrap();
    until("b sees the call", || !b.calls(&dm).is_empty()).await;
    b.call(Some((dm, DM))).await.unwrap();
    let n = |x: &Node| x.calls(&dm).get(&DM).map_or(0, Vec::len);
    until("both in the call", || n(&a) == 2 && n(&b) == 2).await;
    let hear = async |from: &Arc<Node>, rx: &mut Receiver<_>| {
        for _ in 0..200 {
            from.send_media(&[0, 7]);
            tokio::time::sleep(Duration::from_millis(50)).await;
            while let Ok((who, f)) = rx.try_recv() {
                if who == from.id() && f == [0, 7] {
                    return true;
                }
            }
        }
        false
    };
    assert!(hear(&a, &mut at_b).await, "b hears a");
    assert!(hear(&b, &mut at_a).await, "a hears b");
}

/// A DM made while b's phone sleeps: its doorbell wakes it, it takes the
/// DM, sleeps again; opened later, b and a meet in it.
#[tokio::test(flavor = "multi_thread")]
async fn a_dm_taken_asleep_connects_later() {
    use irch_core::wake;
    let db = dir("b");
    let a = node("a").await;
    let b = start(&db, "b", None, false).await;
    let id = a.create("lab").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let url = wake::doorbell(&db, &Net::Direct).await.unwrap();
    b.set_push(Some(url)).await;
    let ring = |p: &(iroh::EndpointId, String)| p.1.starts_with("irch:ring:");
    until("a knows b's doorbell", || {
        a.view(&id, |s| s.pushes_of(None).iter().any(ring)) == Some(true)
    })
    .await;
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    let b_id = b.id();
    b.shutdown().await;
    drop(b);
    // Asleep: a opens a DM; b's doorbell takes the knock.
    let d = db.clone();
    let phone = tokio::spawn(async move { wake::rung(&d, &Net::Direct).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let dm = a.dm(&id, &b_id).await.unwrap();
    let news = tokio::time::timeout(Duration::from_secs(10), phone).await;
    let news = news.unwrap().unwrap().unwrap().expect("the knock");
    // Woken for a moment, as Android's service does, then asleep again.
    let b = start(&db, "b", None, false).await;
    b.deliver(news);
    until("b takes the DM", || b.spaces().contains(&dm)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    b.shutdown().await;
    drop(b);
    tokio::time::sleep(Duration::from_secs(1)).await;
    // Opened: both in the DM's gossip.
    let b = start(&db, "b", None, false).await;
    until("b gossips in the DM", || b.peers(&dm) > 0).await;
    until("a gossips in the DM", || a.peers(&dm) > 0).await;
}

/// Both open a DM with the other at once: they end up in one, the same.
#[tokio::test(flavor = "multi_thread")]
async fn two_dms_at_once_become_one() {
    use irch_core::state::DM;
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("hub").await.unwrap();
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let ka = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    let kb = || b.view(&id, |s| s.kem(&a.id()).is_some()) == Some(true);
    until("keys both ways", || ka() && kb()).await;
    let (ia, ib) = (a.id(), b.id());
    let (da, db) = tokio::join!(a.dm(&id, &ib), b.dm(&id, &ia));
    assert_ne!(da.unwrap(), db.unwrap(), "two DMs, as before");
    // Asked again, each names the same one, and both are in it.
    let same = async || {
        for _ in 0..400 {
            let (x, y) = (a.dm(&id, &ib).await, b.dm(&id, &ia).await);
            if let (Ok(x), Ok(y)) = (x, y)
                && x == y
                && a.peers(&x) > 0
            {
                return Some(x);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        None
    };
    let dm = same().await.expect("one DM for both");
    say(&a, &dm, DM, "found you").await;
    until("b reads it", || texts(&b, &dm, &DM) == ["found you"]).await;
}

/// A call in a small space's channel rings like a DM's.
#[tokio::test(flavor = "multi_thread")]
async fn a_small_spaces_call_rings() {
    let (a, b) = (node("a").await, node("b").await);
    let id = a.create("friends").await.unwrap();
    let ch = general(&a, &id);
    b.join(&a.invite(&id).await.unwrap()).await.unwrap();
    let keyed = || a.view(&id, |s| s.kem(&b.id()).is_some()) == Some(true);
    until("b's key", keyed).await;
    until("b gossips", || b.peers(&id) > 0).await;
    b.notices();
    a.call(Some((id, ch))).await.unwrap();
    until("b sees the call", || !b.calls(&id).is_empty()).await;
    let rings: Vec<_> = b.notices().into_iter().filter(|n| n.ring).collect();
    assert_eq!(rings.len(), 1, "it rings");
    assert!(b.loud(&id));
}
