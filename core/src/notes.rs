//! Messages and threads into Markdown notes, linked back by irch://.

use crate::{Ch, Id, State};
use anyhow::{Result, ensure};
use std::fs;
use std::path::{Component, Path};

pub fn link(space: &Id, ch: &Ch, id: &Id) -> String {
    format!("irch://open/{space}/{ch}/{id}")
}

/// A `- [ ]` task, an event (not a task, with a time: loroh's calendar),
/// or a quote; with `thread`, its whole thread.
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
    let who = plain(&format!("{} in #{room}", m.name));
    let at = format!("[{who}]({})", link(space, &ch, &m.id));
    let first = |t: &str| {
        let l = t.lines().find(|l| !l.trim().is_empty());
        tame(l.unwrap_or("(attachment)"))
    };
    let due = due.filter(|d| d.chars().all(|c| "0123456789-T:".contains(c)));
    if todo {
        let due = due.map(|d| format!(" due:{d}")).unwrap_or_default();
        return Some(format!("- [ ] {} ({at}){due}\n", first(&m.text)));
    }
    if let Some(d) = due {
        return Some(format!("- {} ({at}) at:{d}\n", first(&m.text)));
    }
    let replies =
        if thread { st.messages(&m.id.thread(), None, 500) } else { vec![] };
    let mut s = if thread {
        format!("## {}\n\n", first(&m.text))
    } else {
        String::new()
    };
    for x in std::iter::once(&m).chain(&replies) {
        let text = tame(x.text.trim()).replace('\n', "\n  ");
        s += &format!("- **{}**: {text}\n", plain(&x.name));
    }
    Some(s + &format!("\n— {at}\n"))
}

/// A name as text, never Markdown: one line, its marks escaped.
fn plain(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '\n' | '\r' => o.push(' '),
            c if "\\`*_[]()<>!|~".contains(c) => o.extend(['\\', c]),
            c => o.push(c),
        }
    }
    o
}

/// Inert: no images (they load anywhere), no fences (plugins run them),
/// no list item or quote of its own (a task, or loroh's event), and no
/// `due:` or `at:` word (a time loroh would act on).
fn tame(s: &str) -> String {
    let line = |l: &str| {
        let l = l.trim();
        let rest = l.trim_start_matches(|c: char| c.is_ascii_digit());
        let n = l.len() - rest.len();
        let mark = match rest.chars().next() {
            Some('.' | ')') if n > 0 => Some(n),
            Some('-' | '+' | '*') if n == 0 => Some(0),
            _ => None,
        };
        let spaced = |m: &usize| {
            l[m + 1..].chars().next().is_none_or(char::is_whitespace)
        };
        let l = match mark.filter(spaced) {
            Some(m) => format!("{}\\{}", &l[..m], &l[m..]),
            None if l.starts_with('>') => format!("\\{l}"),
            None => l.into(),
        };
        let word = |w: &str| {
            let bare = w.trim_start_matches(['*', '_', '~', '`', '[', '(']);
            match bare.starts_with("due:") || bare.starts_with("at:") {
                true => w.replacen(':', " :", 1),
                false => w.into(),
            }
        };
        l.split_inclusive(char::is_whitespace).map(word).collect::<String>()
    };
    let s = s.replace("![", "\\![").replace("```", "\\`\\`\\`");
    let s = s.replace("~~~", "\\~~~");
    s.lines().map(line).collect::<Vec<_>>().join("\n")
}

pub fn list(vault: &Path) -> Vec<String> {
    fn walk(root: &Path, d: &Path, out: &mut Vec<String>) {
        for e in fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            // Not hidden, nor a link (which may lead out, or round).
            let link = e.file_type().is_ok_and(|t| t.is_symlink());
            if link || e.file_name().to_string_lossy().starts_with('.') {
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

/// Never hidden or outside `vault`; written whole so sync never sees half.
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
    let r = data_encoding::HEXLOWER.encode(&crate::crypto::rand::<8>());
    let tmp = p.with_file_name(format!(".{name}.{r}.irch"));
    fs::create_dir_all(p.parent().unwrap_or(vault))?;
    let old = fs::read_to_string(&p).unwrap_or_default();
    let gap = match () {
        _ if old.is_empty() || old.ends_with("\n\n") => "",
        _ if old.ends_with('\n') => "\n",
        _ => "\n\n",
    };
    // Our own new file, never one planted there (or what it links to).
    let mut f =
        fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    std::io::Write::write_all(&mut f, (old + gap + md).as_bytes())?;
    drop(f);
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
        Signed {
            id: Id([n; 32]),
            author: o,
            device,
            ts: n.into(),
            op,
            epoch: [0; 8],
        }
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
    let md = |id, thread, todo, due| {
        markdown(&st, &o, &Id([id; 32]), thread, todo, due)
    };
    let at = format!("[ann in #ideas]({})", link(&o, &ch, &root));
    assert_eq!(
        md(2, false, true, Some("2030-01-02")).unwrap(),
        format!("- [ ] zero-g crystals ({at}) due:2030-01-02\n")
    );
    assert_eq!(
        md(2, false, false, Some("2030-01-02T14:30")).unwrap(),
        format!("- zero-g crystals ({at}) at:2030-01-02T14:30\n"),
        "an event"
    );
    let thread = md(3, true, false, None).unwrap();
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
    assert_eq!(plain("a](x)\n![b"), "a\\]\\(x\\) \\!\\[b");
    assert_eq!(
        tame("![x](https://t/p.png)\n```dataviewjs"),
        "\\![x](https://t/p.png)\n\\`\\`\\`dataviewjs"
    );
    // No task, event or time of its own for loroh.
    let t = "- [ ] pay due:2030-01-01\n> 1. go **at:2030-01-01**\n*ok* 3.";
    let ok =
        "\\- [ ] pay due :2030-01-01\n\\> 1. go **at :2030-01-01**\n*ok* 3.";
    assert_eq!(tame(t), ok);
    // A link planted where the note's temporary file was: never written.
    #[cfg(unix)]
    {
        let out = dir.join("outside");
        fs::write(&out, "mine").unwrap();
        let lab = dir.join("Lab");
        std::os::unix::fs::symlink(&out, lab.join(".Ideas.md.irch")).unwrap();
        std::os::unix::fs::symlink(&lab, lab.join("loop")).unwrap();
        append(&dir, "Lab/Ideas.md", "c\n").unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "mine");
        assert_eq!(list(&dir), ["Lab/Ideas.md"], "links not followed");
    }
}
