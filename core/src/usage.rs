//! What the network cost, a line each time it changed: the node up, the
//! node down (how long, and what iroh sent and heard), a push woke it.
//! Written only then: logging costs no packets and wakes nothing. iroh's
//! own count: its UDP and what relays carried, and how often it probed;
//! not TLS or DNS around them (Android's count of the app has all).

use serde_json::{Value, json};
use std::io::Write;
use std::path::Path;

const KEEP: u64 = 256 << 10;

/// One event, `{"ts", "ev", ...}`; the oldest half goes once it is big.
pub fn note(dir: &Path, ev: &str, mut v: Value) {
    let f = dir.join("usage");
    if std::fs::metadata(&f).is_ok_and(|m| m.len() > KEEP) {
        let all = std::fs::read(&f).unwrap_or_default();
        let _ = std::fs::write(&f, &all[all.len() / 2..]);
    }
    v["ts"] = crate::now().into();
    v["ev"] = ev.into();
    let o = std::fs::OpenOptions::new().create(true).append(true).open(f);
    let _ = o.and_then(|mut o| writeln!(o, "{v}"));
}

/// Newest last; a line cut by trimming is skipped.
pub fn read(dir: &Path) -> Vec<Value> {
    let all = std::fs::read_to_string(dir.join("usage")).unwrap_or_default();
    all.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// What this endpoint has moved since it started.
pub fn counted(ep: &iroh::Endpoint) -> Value {
    let s = &ep.metrics().socket;
    json!({
        "out": s.send_ipv4.get() + s.send_ipv6.get() + s.send_relay.get(),
        "in": s.recv_data_ipv4.get() + s.recv_data_ipv6.get()
            + s.recv_data_relay.get(),
        "relay_out": s.send_relay.get(),
        "relay_in": s.recv_data_relay.get(),
        "datagrams_in": s.recv_datagrams.get(),
        "conns": s.num_conns_opened.get(),
        "probes": ep.metrics().net_report.reports.get(),
        "relay_conns": s.relay_conns_success.get(),
    })
}

#[test]
fn notes_and_trims() {
    let n = u64::from_le_bytes(crate::crypto::rand());
    let d = std::env::temp_dir().join(format!("irch-usage-{n}"));
    std::fs::create_dir_all(&d).unwrap();
    note(&d, "up", json!({}));
    note(&d, "down", json!({ "s": 3 }));
    let v = read(&d);
    let got = (v[0]["ev"].as_str(), v[1]["s"].as_u64());
    assert_eq!(got, (Some("up"), Some(3)));
    for _ in 0..8000 {
        note(&d, "woken", json!({ "pad": "x".repeat(40) }));
    }
    assert!(std::fs::metadata(d.join("usage")).unwrap().len() < 2 * KEEP);
    let _ = std::fs::remove_dir_all(d);
}
