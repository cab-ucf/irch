//! The app's whole bridge: one JSON call in, one JSON value out.

use crate::op::{File, uncode};
use crate::{Ch, Id, Node, Op, Role, notes, state::Pos};
use anyhow::{Context, Result};
use data_encoding::BASE64;
use iroh::EndpointAddr;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// `File` carries bytes as base64.
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
    Pins {
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
    /// A conversation out of the list until its next message; kept.
    Hide {
        space: Id,
        on: bool,
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
    Store {
        space: Id,
        node: iroh::EndpointId,
        on: bool,
    },
    StoreLocal,
    /// Served from this device, left where it is.
    Seal {
        space: Id,
        path: String,
        name: String,
        password: Option<String>,
    },
    Offer {
        space: Id,
        path: String,
        name: Option<String>,
    },
    /// Files served from here, and stopping one.
    Sharing,
    Unshare {
        hash: Id,
    },
    /// What an invite is to, before joining.
    Peek {
        code: String,
    },
    /// This identity's devices.
    Devices,
    /// Name one of our devices or storage nodes.
    NameDevice {
        device: iroh::EndpointId,
        name: String,
    },
    /// A preview of a link in our message, made here as it is sent.
    Preview {
        space: Id,
        message: Id,
        url: String,
    },
    /// This identity is lost: retire it everywhere, for good.
    Burn,
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
    /// In `room`, else `space`, else everywhere.
    Search {
        query: String,
        space: Option<Id>,
        room: Option<Ch>,
    },
    Hangup,
    /// A call to us rings here.
    Ringing {
        space: Id,
        channel: Ch,
    },
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
                "version": env!("CARGO_PKG_VERSION"),
                "device": n.device().fmt_short().to_string(),
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
            let held = n.undelivered(&space);
            // A DM's: on the other's device yet?
            let me = n.id();
            let peer = n.view(&space, |s| s.peer(&me)).flatten();
            let reach = peer.and_then(|p| n.reach(&space, &p));
            let row = |m: crate::Message| {
                let mut v = json!(m);
                v["pending"] = held.contains(&m.id).into();
                if m.author == me && peer.is_some() {
                    v["delivered"] = reach.is_some_and(|r| m.ts <= r).into();
                }
                v
            };
            let page = n.view(&space, page).unwrap_or_default();
            json!(page.into_iter().map(row).collect::<Vec<_>>())
        }
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
        Call::Pins { space, channel } => {
            json!(n.view(&space, |s| s.pins(&channel)).unwrap_or_default())
        }
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
        Call::Hide { space, on } => json!(n.hide(&space, on)),
        Call::Publish { space, op } => json!(n.publish(&space, op).await?),
        Call::Ban { space, user, on } => json!(n.ban(&space, &user, on).await?),
        Call::Rekey { space } => json!(n.rekey(&space).await?),
        Call::Dm { space, user } => json!(n.dm(&space, &user).await?),
        Call::LinkCode => json!(n.link_code().await),
        Call::AddStorage { code } => json!(n.add_storage(&code).await?),
        Call::Store { space, node, on } => {
            json!(n.store(&space, &node, on).await?)
        }
        Call::Seal { space, path, name, password } => {
            let p = std::path::Path::new(&path);
            json!(n.seal_file(&space, p, &name, password.as_deref()).await?)
        }
        Call::Offer { space, path, name } => {
            let p = std::path::Path::new(&path);
            json!(n.offer(&space, p, name.as_deref()).await?)
        }
        Call::Sharing => {
            let row = |(hash, space, name, size)| json!({ "hash": hash, "space": space, "name": name, "size": size });
            json!(n.sharing().into_iter().map(row).collect::<Vec<_>>())
        }
        Call::Unshare { hash } => json!(n.unshare(&hash).await?),
        Call::Peek { code } => {
            let i = crate::Invite::decode(&code)?;
            let who = n.spaces().iter().find_map(|s| {
                n.view(s, |s| s.profile(&i.owner).map(str::to_string))?
            });
            json!({ "space": i.id(), "name": i.name, "owner": i.owner,
                "who": who, "dm": i.dm.is_some(),
                "joined": n.spaces().contains(&i.id()) })
        }
        Call::Devices => {
            let me = n.device();
            let row = |(d, at): (iroh::EndpointId, u64)| {
                json!({ "id": d, "device": d.fmt_short().to_string(),
                    "name": n.label(&d), "seen": at, "this": d == me })
            };
            json!(n.devices().into_iter().map(row).collect::<Vec<_>>())
        }
        Call::Burn => json!(n.burn().await?),
        Call::NameDevice { device, name } => {
            json!(n.name_device(device, &name).await?)
        }
        Call::Preview { space, message, url } => {
            json!(n.preview(&space, &message, &url).await?)
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
        Call::Ringing { space, channel } => {
            json!(n.ringing(&space, channel).await)
        }
        Call::SetLevel { space, level } => json!(n.set_level(&space, level)),
        // Null where there is no notes folder (Android).
        Call::Notes => json!(n.notes()),
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

fn space(n: &Node, id: &Id) -> Option<Value> {
    let me = n.id();
    let v = n.view(id, |s| {
        // A DM is named by who else is in it (members: no cost elsewhere).
        let others = s.dm.map(|_| s.members()).unwrap_or_default();
        let others: Vec<_> =
            others.into_iter().filter(|m| m.id != me).map(|m| m.name).collect();
        let name = match others.is_empty() {
            true => s.name().to_string(),
            false => others.join(", "),
        };
        let with = s.peer(&me);
        (
            name,
            with.is_some(),
            s.head(),
            s.role(&me) == Role::Admin,
            s.channels(),
            with,
            s.threads(),
        )
    })?;
    let (name, dm, owner, admin, chans, with, threads) = v;
    let mut calls = n.calls(id);
    let thread = |t: &crate::Thread| {
        let (unread, mentions) = n.unread(id, &t.id);
        let fresh = t.last + 7 * 86_400_000 > crate::now();
        (fresh || mentions > 0).then(|| {
            json!({
                "id": t.id, "name": t.name, "root": t.root, "count": t.count,
                "by": t.by, "alone": t.alone,
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
    // A DM call of ours: has their device got it, and does it ring?
    let call = n.in_call().filter(|c| c.0 == *id).map(|c| c.1);
    let rings = with.zip(call).is_some_and(|(w, c)| n.rings(id, &w, &c));
    let reach = with.and_then(|w| n.reach(id, &w));
    Some(json!({
        "id": id, "name": name, "dm": dm, "with": with, "owner": owner,
        "admin": admin, "peers": peers, "locked": locked, "channels": channels,
        "near": n.neighbors(id).into_iter()
            .map(|(d, who)| (d.fmt_short().to_string(), who))
            .map(|(device, who)| json!({ "device": device, "who": who }))
            .collect::<Vec<_>>(),
        "level": n.level(id),
        "reach": reach, "ringing": rings,
        "closed": n.hidden(id),
        "storage": n.stored(id).into_iter()
            .map(|(node, on)| json!({ "node": node, "on": on,
                "seen": n.synced(&node), "name": n.label(&node),
                "short": node.fmt_short().to_string() }))
            .collect::<Vec<_>>(),
    }))
}
