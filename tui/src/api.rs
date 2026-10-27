use irch_core::{Node, api};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader, stdin};
use tokio::sync::broadcast::error::RecvError::Closed;
use tokio::task::JoinSet;

/// A call a line in (`{"id": 1, "cmd": "spaces"}`), its answer a line out
/// (`{"id": 1, "ok": …}` or `"err"`), and every event as `{"event": …}`.
pub async fn run(n: Arc<Node>) -> anyhow::Result<()> {
    let mut ev = n.subscribe();
    tokio::spawn(async move {
        loop {
            match ev.recv().await {
                Ok(e) => println!("{}", json!({ "event": e })),
                Err(Closed) => break,
                Err(_) => {}
            }
        }
    });
    let (mut lines, mut all) =
        (BufReader::new(stdin()).lines(), JoinSet::new());
    while let Some(l) = lines.next_line().await? {
        while all.try_join_next().is_some() {}
        let n = n.clone();
        all.spawn(async move {
            let mut v: Value = serde_json::from_str(&l).unwrap_or_default();
            let id = v.as_object_mut().and_then(|o| o.remove("id"));
            let r = match serde_json::from_value(v) {
                Ok(c) => api::call(&n, c).await,
                Err(e) => Err(e.into()),
            };
            let out = match r {
                Ok(ok) => json!({ "id": id, "ok": ok }),
                Err(e) => json!({ "id": id, "err": format!("{e:#}") }),
            };
            println!("{out}");
        });
    }
    while all.join_next().await.is_some() {}
    Ok(())
}
