//! Into notes (loroh, Obsidian, nvim: plain Markdown): a message or a
//! thread as a quote or a task, with an irch:// link back to it.

use crate::{Ch, Id, State};
use anyhow::{Result, ensure};
use std::fs;
use std::path::{Component, Path};

pub fn link(space: &Id, ch: &Ch, id: &Id) -> String {
    format!("irch://open/{space}/{ch}/{id}")
}

/// `id` as Markdown: a `- [ ]` task, or a quote. With `thread`, the whole
/// thread it starts or sits in.
pub fn markdown(
    st: &State,
    space: &Id,
    id: &Id,
    thread: bool,
    todo: bool,
    due: Option<&str>,
) -> Option<String> {
    let (mut ch, mut m) = st.message(id)?;
    if let Some(t) = st.thread(&ch).filter(|_| thread) {
        (ch, m) = st.message(&t.root)?;
    }
    let rooms = st.rooms();
    let room = rooms.iter().find(|r| r.0 == ch).map_or("", |r| &r.1);
    let who = format!("{} in #{room}", m.name).replace(['[', ']'], "");
    let at = format!("[{who}]({})", link(space, &ch, &m.id));
    let first = |t: &str| {
        let l = t.lines().find(|l| !l.trim().is_empty());
        l.unwrap_or("(attachment)").trim().to_string()
    };
    if todo {
        let due =
            due.filter(|d| d.chars().all(|c| "0123456789-T:".contains(c)));
        let due = due.map(|d| format!(" due:{d}")).unwrap_or_default();
        return Some(format!("- [ ] {} ({at}){due}\n", first(&m.text)));
    }
    let replies =
        if thread { st.messages(&m.id.thread(), None, 500) } else { vec![] };
    let mut s = if thread {
        format!("## {}\n\n", first(&m.text))
    } else {
        String::new()
    };
    for x in std::iter::once(&m).chain(&replies) {
        let text = x.text.trim().replace('\n', "\n  ");
        s += &format!("- **{}**: {text}\n", x.name);
    }
    Some(s + &format!("\n— {at}\n"))
}

/// Every note in `vault`, shared folders included: relative, sorted.
pub fn list(vault: &Path) -> Vec<String> {
    fn walk(root: &Path, d: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            } else if p.is_dir() {
                walk(root, &p, out);
            } else if p.extension().is_some_and(|x| x == "md")
                && let Ok(r) = p.strip_prefix(root)
            {
                out.push(r.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let mut out = vec![];
    walk(vault, vault, &mut out);
    out.sort();
    out
}

/// Add `md` at the end of `file`, a note inside `vault` (never a hidden
/// path or one outside), written whole so sync never sees half of it.
pub fn append(vault: &Path, file: &str, md: &str) -> Result<()> {
    let rel = Path::new(file);
    let plain = |c: Component| match c {
        Component::Normal(s) => !s.to_string_lossy().starts_with('.'),
        _ => false,
    };
    let ok = rel.extension().is_some_and(|x| x == "md")
        && rel.components().all(plain);
    ensure!(ok, "notes are .md files inside the notes folder");
    let p = vault.join(rel);
    let name = p.file_name().unwrap_or_default().to_string_lossy();
    let tmp = p.with_file_name(format!(".{name}.irch"));
    fs::create_dir_all(p.parent().unwrap_or(vault))?;
    let old = fs::read_to_string(&p).unwrap_or_default();
    let gap = match () {
        _ if old.is_empty() || old.ends_with("\n\n") => "",
        _ if old.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    fs::write(&tmp, old + gap + md)?;
    Ok(fs::rename(tmp, p)?)
}

#[test]
fn messages_and_threads_become_markdown() {
    use crate::op::{Invite, Op, Signed};
    let (o, root) = (Id([1; 32]), Id([2; 32]));
    let mut st = State::new(&Invite::new(o, "lab", None));
    let ch = Ch([9; 8]);
    let op = |n: u8, op| {
        let device = iroh::SecretKey::from_bytes(&[n; 32]).public();
        Signed { id: Id([n; 32]), author: o, device, ts: n.into(), op }
    };
    let post = |channel, text: &str| Op::Post {
        channel,
        text: text.into(),
        reply: None,
        files: vec![],
    };
    st.extend([
        op(1, Op::Channel { id: ch, name: Some("ideas".into()) }),
        op(2, post(ch, "zero-g crystals\nwhy not")),
        op(3, post(root.thread(), "I can help")),
        op(4, Op::Profile { name: "ann".into(), kem: vec![] }),
    ]);
    let md = |id, thread, todo| {
        markdown(&st, &o, &Id([id; 32]), thread, todo, Some("2030-01-02"))
    };
    let at = format!("[ann in #ideas]({})", link(&o, &ch, &root));
    assert_eq!(
        md(2, false, true).unwrap(),
        format!("- [ ] zero-g crystals ({at}) due:2030-01-02\n")
    );
    let thread = md(3, true, false).unwrap();
    assert!(thread.starts_with("## zero-g crystals\n\n- **ann**: zero-g"));
    assert!(thread.contains("\n  why not\n- **ann**: I can help\n"));
    assert!(thread.ends_with(&format!("\n— {at}\n")));
    let dir = std::env::temp_dir().join(format!("irch-notes-{}", o.0[0]));
    let _ = fs::remove_dir_all(&dir);
    append(&dir, "Lab/Ideas.md", "a\n").unwrap();
    append(&dir, "Lab/Ideas.md", "b\n").unwrap();
    assert_eq!(
        fs::read_to_string(dir.join("Lab/Ideas.md")).unwrap(),
        "a\n\nb\n"
    );
    assert_eq!(list(&dir), ["Lab/Ideas.md"]);
    for bad in ["../x.md", ".siroh/key.md", "/etc/x.md", "x.txt"] {
        assert!(append(&dir, bad, "x").is_err(), "{bad}");
    }
}
