//! The app's whole bridge: one JSON call in, one JSON value out.

use crate::op::{File, uncode};
use crate::{Ch, Id, Node, Op, Role, notes, state::Pos};
use anyhow::{Context, Result};
use data_encoding::BASE64;
use iroh::EndpointAddr;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// `Attach` takes a file as base64 and returns what goes in `Post.files`;
/// `File` returns an attachment's bytes as base64.
#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Call {
    Me,
    SetName {
        name: String,
    },
    SetInvisible {
        on: bool,
    },
    Spaces,
    Members {
        space: Id,
    },
    Messages {
        space: Id,
        channel: Ch,
        before: Option<Pos>,
        limit: usize,
    },
    Thread {
        space: Id,
        channel: Ch,
    },
    MarkRead {
        space: Id,
        channel: Ch,
    },
    Typing {
        space: Id,
        channel: Ch,
    },
    Create {
        name: String,
    },
    Join {
        code: String,
    },
    Invite {
        space: Id,
    },
    Leave {
        space: Id,
    },
    Publish {
        space: Id,
        op: Op,
    },
    Ban {
        space: Id,
        user: Id,
        on: bool,
    },
    Rekey {
        space: Id,
    },
    Dm {
        space: Id,
        user: Id,
    },
    LinkCode,
    AddStorage {
        code: String,
    },
    Attach {
        space: Id,
        name: String,
        data: String,
    },
    Store {
        space: Id,
        node: iroh::EndpointId,
        on: bool,
    },
    /// Pair with this computer's storage node (nixos.nix).
    StoreLocal,
    /// A file served from this device, left where it is.
    Offer {
        path: String,
        name: Option<String>,
    },
    File {
        space: Id,
        file: File,
    },
    Ring {
        space: Id,
        channel: Ch,
    },
    Notes,
    Note {
        space: Id,
        message: Id,
        thread: bool,
        todo: bool,
        due: Option<String>,
        file: Option<String>,
    },
    /// Messages with every word of `query`: in `room` of `space`, in
    /// `space`, or (neither) everywhere.
    Search {
        query: String,
        space: Option<Id>,
        room: Option<Ch>,
    },
    Hangup,
    SetLevel {
        space: Id,
        level: crate::Level,
    },
}

pub async fn call(n: &Arc<Node>, c: Call) -> Result<Value> {
    Ok(match c {
        Call::Me => {
            let (id, name, invisible) = (n.id(), n.name(), n.invisible());
            let call =
                n.in_call().map(|(s, c)| json!({ "space": s, "channel": c }));
            json!({
                "id": id, "name": name, "invisible": invisible, "call": call,
                "push": n.pushed(),
                "local_storage": crate::local_storage()
                    .and_then(|c| uncode::<(EndpointAddr, [u8; 16])>("host", &c).ok())
                    .map(|(a, _)| a.id),
            })
        }
        Call::SetName { name } => json!(n.set_name(&name).await?),
        Call::SetInvisible { on } => json!(n.set_invisible(on).await),
        Call::Spaces => json!(
            n.spaces().iter().filter_map(|s| space(n, s)).collect::<Vec<_>>()
        ),
        Call::Members { space } => {
            let on = n.online(&space);
            let ms = n.view(&space, |s| s.members()).unwrap_or_default();
            let row = |m: crate::Member| {
                let mut v = json!(m);
                v["online"] = on.contains(&m.id).into();
                v
            };
            json!(ms.into_iter().map(row).collect::<Vec<_>>())
        }
        Call::Messages { space, channel, before, limit } => {
            let page =
                |s: &crate::State| s.messages(&channel, before, limit.min(500));
            json!(n.view(&space, page).unwrap_or_default())
        }
        // The thread, and the message it hangs off.
        Call::Search { query, space, room } => {
            let ids = space.map_or_else(|| n.spaces(), |s| vec![s]);
            let mut v: Vec<_> = ids
                .iter()
                .filter_map(|id| {
                    let hits =
                        n.view(id, |s| s.search(&query, room.as_ref(), 50));
                    Some(hits?.into_iter().map(move |(c, m)| (*id, c, m)))
                })
                .flatten()
                .collect();
            v.sort_by_key(|(_, _, m)| std::cmp::Reverse(m.ts));
            v.truncate(50);
            let row = |(space, channel, message)| json!({ "space": space, "channel": channel, "message": message });
            json!(v.into_iter().map(row).collect::<Vec<_>>())
        }
        Call::Thread { space, channel } => json!(
            n.view(&space, |s| s.thread(&channel).map(|t| {
                let m = s.message(&t.root).map(|m| m.1);
                (t, m)
            }))
            .flatten()
        ),
        Call::MarkRead { space, channel } => {
            json!(n.mark_read(&space, &channel))
        }
        Call::Typing { space, channel } => {
            json!(n.typing(&space, channel).await)
        }
        Call::Create { name } => json!(n.create(&name).await?),
        Call::Join { code } => json!(n.join(&code).await?),
        Call::Invite { space } => json!(n.invite(&space).await?),
        Call::Leave { space } => json!(n.leave(&space).await?),
        Call::Publish { space, op } => json!(n.publish(&space, op).await?),
        Call::Ban { space, user, on } => json!(n.ban(&space, &user, on).await?),
        Call::Rekey { space } => json!(n.rekey(&space).await?),
        Call::Dm { space, user } => json!(n.dm(&space, &user).await?),
        Call::LinkCode => json!(n.link_code().await),
        Call::AddStorage { code } => json!(n.add_storage(&code).await?),
        Call::Attach { space, name, data } => {
            let data = BASE64.decode(data.as_bytes())?;
            json!(n.attach(&space, &name, &data).await?)
        }
        Call::Store { space, node, on } => {
            json!(n.store(&space, &node, on).await?)
        }
        Call::Offer { path, name } => {
            json!(n.offer(std::path::Path::new(&path), name.as_deref()).await?)
        }
        Call::StoreLocal => {
            let code =
                crate::local_storage().context("none on this computer")?;
            json!(n.add_storage(&code).await?)
        }
        Call::File { space, file } => {
            json!(BASE64.encode(&n.file(&space, &file).await?))
        }
        Call::Ring { space, channel } => {
            json!(n.call(Some((space, channel))).await?)
        }
        Call::Hangup => json!(n.call(None).await?),
        Call::SetLevel { space, level } => json!(n.set_level(&space, level)),
        // Notes in the notes folder, or null where there is none (Android).
        Call::Notes => json!(n.notes()),
        // The Markdown, added to `file` if given.
        Call::Note { space, message, thread, todo, due, file } => {
            let md = |s: &crate::State| {
                notes::markdown(
                    s,
                    &space,
                    &message,
                    thread,
                    todo,
                    due.as_deref(),
                )
            };
            let md = n.view(&space, md).flatten().context("no such message")?;
            if let Some(f) = file {
                n.note(&f, &md)?;
            }
            json!(md)
        }
    })
}

/// Sidebar row: a space, its channels, unread and mention counts.
fn space(n: &Node, id: &Id) -> Option<Value> {
    let me = n.id();
    let v = n.view(id, |s| {
        let others: Vec<_> =
            s.members().into_iter().filter(|m| m.id != me).collect();
        let name = match others.is_empty() || s.dm.is_none() {
            true => s.name().to_string(),
            false => others
                .iter()
                .map(|m| m.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        };
        let with = s.peer(&me);
        (
            name,
            with.is_some(),
            s.owner,
            s.role(&me) == Role::Admin,
            s.channels(),
            with,
            s.threads(),
        )
    })?;
    let (name, dm, owner, admin, chans, with, threads) = v;
    let mut calls = n.calls(id);
    // Threads active this week, or that mention us.
    let thread = |t: &crate::Thread| {
        let (unread, mentions) = n.unread(id, &t.id);
        let fresh = t.last + 7 * 86_400_000 > crate::now();
        (fresh || mentions > 0).then(|| {
            json!({
                "id": t.id, "name": t.name, "root": t.root, "count": t.count,
                "by": t.by,
                "unread": unread, "mentions": mentions,
            })
        })
    };
    let chan = |(c, name): (Ch, String)| {
        let (unread, mentions) = n.unread(id, &c);
        let call = calls.remove(&c).unwrap_or_default();
        let t = threads.iter().filter(|t| t.parent == c);
        let threads: Vec<_> = t.filter_map(thread).collect();
        json!({
            "id": c, "name": name, "unread": unread, "mentions": mentions,
            "call": call, "threads": threads,
        })
    };
    let channels: Vec<_> = chans.into_iter().map(chan).collect();
    let (peers, locked) = (n.peers(id), n.locked(id));
    Some(json!({
        "id": id, "name": name, "dm": dm, "with": with, "owner": owner,
        "admin": admin, "peers": peers, "locked": locked, "channels": channels,
        "near": n.neighbors(id).into_iter()
            .map(|(d, who)| (d.fmt_short().to_string(), who))
            .map(|(device, who)| json!({ "device": device, "who": who }))
            .collect::<Vec<_>>(),
        "level": n.level(id),
        "storage": n.stored(id).into_iter()
            .map(|(node, on)| json!({ "node": node, "on": on }))
            .collect::<Vec<_>>(),
    }))
}
