//! The app's whole bridge: one JSON call in, one JSON value out.

use crate::{Ch, Id, Node, Op, Role, op::File, state::Pos};
use anyhow::Result;
use data_encoding::BASE64;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

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
    /// A file to send, base64; returns what goes in `Post.files`.
    Attach {
        name: String,
        data: String,
    },
    /// An attachment's bytes, base64.
    File {
        space: Id,
        file: File,
    },
}

pub async fn call(n: &Arc<Node>, c: Call) -> Result<Value> {
    Ok(match c {
        Call::Me => {
            let (id, name, invisible) = (n.id(), n.name(), n.invisible());
            json!({ "id": id, "name": name, "invisible": invisible })
        }
        Call::SetName { name } => json!(n.set_name(&name).await?),
        Call::SetInvisible { on } => json!(n.set_invisible(on)),
        Call::Spaces => json!(
            n.spaces().iter().filter_map(|s| space(n, s)).collect::<Vec<_>>()
        ),
        Call::Members { space } => {
            let on = n.online(&space);
            let ms = n.view(&space, |s| s.members()).unwrap_or_default();
            let row = |m: crate::Member| {
                let online = on.contains(&m.id);
                let (id, name, role) = (m.id, m.name, m.role);
                json!({ "id": id, "name": name, "role": role, "online": online })
            };
            json!(ms.into_iter().map(row).collect::<Vec<_>>())
        }
        Call::Messages { space, channel, before, limit } => {
            let page =
                |s: &crate::State| s.messages(&channel, before, limit.min(500));
            json!(n.view(&space, page).unwrap_or_default())
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
        Call::Publish { space, op } => json!(n.publish(&space, op).await?),
        Call::Ban { space, user, on } => json!(n.ban(&space, &user, on).await?),
        Call::Rekey { space } => json!(n.rekey(&space).await?),
        Call::Dm { space, user } => json!(n.dm(&space, &user).await?),
        Call::LinkCode => json!(n.link_code().await),
        Call::AddStorage { code } => json!(n.add_storage(&code).await?),
        Call::Attach { name, data } => {
            json!(n.attach(&name, &BASE64.decode(data.as_bytes())?).await?)
        }
        Call::File { space, file } => {
            json!(BASE64.encode(&n.file(&space, &file).await?))
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
        )
    })?;
    let (name, dm, owner, admin, chans, with) = v;
    let chan = |(c, name): (Ch, String)| {
        let (unread, mentions) = n.unread(id, &c);
        json!({ "id": c, "name": name, "unread": unread, "mentions": mentions })
    };
    let channels: Vec<_> = chans.into_iter().map(chan).collect();
    let (peers, locked) = (n.peers(id), n.locked(id));
    Some(json!({
        "id": id, "name": name, "dm": dm, "with": with, "owner": owner,
        "admin": admin, "peers": peers, "locked": locked, "channels": channels,
    }))
}
