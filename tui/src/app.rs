use anyhow::{Context, Result, anyhow, bail, ensure};
use crossterm::event::{Event as Term, EventStream, KeyCode, KeyEvent};
use crossterm::event::{KeyEventKind, KeyModifiers};
use irch_core::{Ch, Event, Id, Message, Node, Op, Role, notes};
use n0_future::StreamExt;
use ratatui::layout::{Constraint::*, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const HELP: &str = "\
NORMAL                                  INSERT (i a I A o, or r t e)
j k ↓ ↑   newer / older message (5k)    Enter    send
G  gg     latest / oldest               Esc ^c   back, the draft kept
^d ^u     ten messages  ^f ^b  twenty   ↑        (empty) edit your last
J K gt gT next / previous channel       ^w ^u    delete word / to start
H L       previous / next space         ^a ^e    start / end
l Enter   open its thread   h  back     ^n ^p    channel, from anywhere
r reply   t new thread   e edit   dd delete   y copy   p pin   + react
Y         copy its link (none picked: the channel's), for tydig or notes
/ n N     find here, next, previous     ^k       jump to a channel or DM
x         close a DM (kept; back with its next message)
:         a command   ?  this   esc q  close this   ZZ :q  quit

:create NAME  :join CODE  :invite  :dm USER  :b NAME (go)  :nick NAME
:leave        leave a space; in a DM, delete it from your devices
:close        put a DM away, kept     :invisible  no presence or typing
:channel NAME  :rmchannel  :rename NAME  :room NAME (a named thread)
:reply TEXT  :thread TEXT  :react EMOJI  :edit TEXT  :delete
:search WORDS  in this space          :note [FILE]  :todo [FILE]
:file PATH TEXT  send a file   :share PATH TEXT  from here   :save
:admin :member :ban :unban USER  :rekey  :link  :store CODE|local|here";

type Typing = HashMap<(Id, Ch), HashMap<Id, Instant>>;
type At = Option<(Id, Option<Ch>)>;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    Insert,
    Cmd,
    Find,
    Jump,
}

/// What Enter does with the draft.
#[derive(Clone, Copy, PartialEq)]
enum Draft {
    New,
    Reply(Id),
    Thread(Id),
    Edit(Id),
}

enum Ask {
    Delete(Id),
    Leave(Id),
}

/// A line with a cursor, edited with readline's keys.
#[derive(Default)]
struct Field {
    text: String,
    at: usize,
}

impl Field {
    fn byte(&self) -> usize {
        self.text.char_indices().nth(self.at).map_or(self.text.len(), |c| c.0)
    }

    fn len(&self) -> usize {
        self.text.chars().count()
    }

    fn set(&mut self, s: &str) {
        (self.text, self.at) = (s.into(), s.chars().count());
    }

    fn take(&mut self) -> String {
        self.at = 0;
        std::mem::take(&mut self.text)
    }

    /// False if `k` is not one of its keys.
    fn key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let b = self.byte();
        match k.code {
            KeyCode::Char('a') if ctrl => self.at = 0,
            KeyCode::Char('e') if ctrl => self.at = self.len(),
            KeyCode::Home => self.at = 0,
            KeyCode::End => self.at = self.len(),
            KeyCode::Left => self.at = self.at.saturating_sub(1),
            KeyCode::Right => self.at = (self.at + 1).min(self.len()),
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..b);
                self.at = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let head = self.text[..b].trim_end();
                let cut = head.char_indices().rev().find(|c| c.1 == ' ');
                let cut = cut.map_or(0, |c| c.0 + 1);
                self.at -= self.text[cut..b].chars().count();
                self.text.drain(cut..b);
            }
            KeyCode::Backspace if self.at > 0 => {
                self.at -= 1;
                self.text.remove(self.byte());
            }
            KeyCode::Delete if self.at < self.len() => {
                self.text.remove(b);
            }
            KeyCode::Backspace | KeyCode::Delete => {}
            KeyCode::Char(c) if !ctrl => {
                self.text.insert(b, c);
                self.at += 1;
            }
            _ => return false,
        }
        true
    }
}

struct App {
    n: Arc<Node>,
    at: At,
    mode: Mode,
    back: Mode,
    draft: Draft,
    field: Field,
    prompt: Field,
    drafts: HashMap<(Id, Ch), String>,
    sel: Option<Id>,
    top: Cell<usize>,
    count: usize,
    pend: Option<char>,
    ask: Option<Ask>,
    found: String,
    pick: usize,
    status: String,
    notice: Option<String>,
    typing: Typing,
    typed: Option<Instant>,
}

pub async fn run(n: Arc<Node>) -> Result<()> {
    let mut t = ratatui::init();
    let mut a = App {
        n,
        at: None,
        mode: Mode::Insert,
        back: Mode::Insert,
        draft: Draft::New,
        field: Field::default(),
        prompt: Field::default(),
        drafts: HashMap::new(),
        sel: None,
        top: Cell::new(0),
        count: 0,
        pend: None,
        ask: None,
        found: String::new(),
        pick: 0,
        status: "Esc for vim keys, ? or :help for all".into(),
        notice: None,
        typing: HashMap::new(),
        typed: None,
    };
    let r = a.run(&mut t).await;
    ratatui::restore();
    r
}

impl App {
    async fn run(&mut self, t: &mut DefaultTerminal) -> Result<()> {
        let (mut keys, mut events) = (EventStream::new(), self.n.subscribe());
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            self.fix();
            if let Some((s, Some(c))) = self.at {
                self.n.mark_read(&s, &c);
            }
            t.draw(|f| self.draw(f))?;
            tokio::select! {
                k = keys.next() => match k {
                    Some(Ok(Term::Key(k))) if k.kind == KeyEventKind::Press => {
                        if self.key(k).await {
                            return Ok(());
                        }
                    }
                    Some(Err(e)) => return Err(e.into()),
                    None => return Ok(()),
                    _ => {}
                },
                e = events.recv() => match e {
                    Ok(Event::Typing { space, channel, user }) => {
                        let until = Instant::now() + Duration::from_secs(6);
                        let at = (space, channel);
                        self.typing.entry(at).or_default().insert(user, until);
                    }
                    Ok(Event::Synced { count, .. }) => {
                        self.status = format!("caught up on {count} ops");
                    }
                    _ => {}
                },
                _ = tick.tick() => {}
            }
        }
    }

    /// True to quit.
    async fn key(&mut self, k: KeyEvent) -> bool {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if let Some(a) = self.ask.take() {
            let yes = k.code == KeyCode::Char('y');
            self.status = match (yes, a) {
                (false, _) => "kept".into(),
                (_, Ask::Delete(target)) => {
                    let r = self.post(Op::Edit { target, text: None }).await;
                    r.map_or_else(|e| format!("{e:#}"), |_| "deleted".into())
                }
                (_, Ask::Leave(s)) => {
                    let r = self.n.leave(&s).await;
                    r.map_or_else(|e| format!("{e:#}"), |_| "gone".into())
                }
            };
            return false;
        }
        let other = self.mode != Mode::Jump;
        match k.code {
            KeyCode::Char('n') | KeyCode::Down if ctrl && other || alt => {
                self.step(1, false);
            }
            KeyCode::Char('p') | KeyCode::Up if ctrl && other || alt => {
                self.step(-1, false);
            }
            KeyCode::Char('k') if ctrl => self.ask_for(Mode::Jump, ""),
            _ => {
                let r = match self.mode {
                    Mode::Normal => self.normal(k).await,
                    Mode::Insert => self.insert(k).await,
                    _ => self.prompted(k).await,
                };
                match r {
                    Ok(quit) => return quit,
                    Err(e) => self.status = format!("{e:#}"),
                }
            }
        }
        false
    }

    async fn normal(&mut self, k: KeyEvent) -> Result<bool> {
        use KeyCode::*;
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if let Char(d @ '0'..='9') = k.code
            && (d != '0' || self.count > 0)
        {
            self.count = self.count * 10 + d as usize - '0' as usize;
            return Ok(false);
        }
        let n = std::mem::take(&mut self.count).max(1) as isize;
        match (self.pend.take(), k.code) {
            (_, Char('c')) if ctrl => return Ok(true),
            (_, Char('d')) if ctrl => self.walk(10 * n),
            (_, Char('u')) if ctrl => self.walk(-10 * n),
            (_, Char('f') | Char('b')) if !ctrl => {}
            (_, Char('f')) | (_, PageDown) => self.walk(20 * n),
            (_, Char('b')) | (_, PageUp) => self.walk(-20 * n),
            (_, _) if ctrl => {}
            (Some('g'), Char('g')) => {
                self.sel = self.msgs().first().map(|m| m.id)
            }
            (Some('g'), Char('t')) => self.step(n, false),
            (Some('g'), Char('T')) => self.step(-n, false),
            (Some('Z'), Char('Z' | 'Q')) => return Ok(true),
            (Some('d'), Char('d')) => {
                self.ask = Some(Ask::Delete(self.target(true)?));
                self.status = "Delete this message, for everyone? y/n".into();
            }
            (_, Char(c @ ('g' | 'd' | 'Z'))) => self.pend = Some(c),
            (_, Char('j') | Down) => self.walk(n),
            (_, Char('k') | Up) => self.walk(-n),
            (_, Char('G')) => self.sel = None,
            (_, Char('J')) => self.step(n, false),
            (_, Char('K')) => self.step(-n, false),
            (_, Char('L')) => self.step(n, true),
            (_, Char('H')) => self.step(-n, true),
            (_, Char('l') | Enter) => self.open()?,
            (_, Char('h') | Backspace) => self.back()?,
            (_, Char('i' | 'a')) => self.mode = Mode::Insert,
            (_, Char('I')) => (self.mode, self.field.at) = (Mode::Insert, 0),
            (_, Char('A')) => {
                (self.mode, self.field.at) = (Mode::Insert, self.field.len());
            }
            (_, Char('o')) => {
                (self.mode, self.draft) = (Mode::Insert, Draft::New)
            }
            (_, Char('r')) => {
                self.draft = Draft::Reply(self.target(false)?);
                self.mode = Mode::Insert;
            }
            (_, Char('t')) => {
                let (s, c) = self.here()?;
                let thread = self.n.view(&s, |st| st.thread(&c).is_some());
                ensure!(thread != Some(true), "threads hang off channels");
                self.draft = Draft::Thread(self.target(false)?);
                self.mode = Mode::Insert;
            }
            (_, Char('e')) => self.edit(self.target(true)?)?,
            (_, Char('y')) => {
                osc52(&self.chosen().context("j or k picks one")?.text);
                self.status = "copied, if your terminal allows".into();
            }
            (_, Char('Y')) => return self.submit("url").await,
            (_, Char('p')) => {
                let m = self.chosen().context("j or k picks one")?;
                self.post(Op::Pin { target: m.id, on: !m.pinned }).await?;
            }
            (_, Char('+')) => self.ask_for(Mode::Cmd, "react "),
            (_, Char(':')) => self.ask_for(Mode::Cmd, ""),
            (_, Char('/')) => self.ask_for(Mode::Find, ""),
            (_, Char('n')) => self.find(-1)?,
            (_, Char('N')) => self.find(1)?,
            (_, Char('x')) => return self.submit("close").await,
            (_, Char('?')) => self.notice = Some(HELP.into()),
            (_, Char('q') | Esc) if self.notice.is_some() => self.notice = None,
            (_, Char('q')) => self.status = "ZZ or :q quits".into(),
            _ => {}
        }
        Ok(false)
    }

    async fn insert(&mut self, k: KeyEvent) -> Result<bool> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => self.mode = Mode::Normal,
            KeyCode::Char('c') if ctrl => self.mode = Mode::Normal,
            KeyCode::Up if self.field.text.is_empty() => {
                self.edit(self.last(true)?)?
            }
            KeyCode::PageUp => self.walk(-20),
            KeyCode::PageDown => self.walk(20),
            KeyCode::Enter => {
                let (line, draft) = (self.field.take(), self.draft);
                self.draft = Draft::New;
                let r = self.send(line.trim(), draft).await;
                if r.is_err() {
                    (self.draft, self.field) = (draft, Field::default());
                    self.field.set(&line);
                }
                return r;
            }
            _ => {
                if self.field.key(k) && matches!(k.code, KeyCode::Char(_)) {
                    self.typed().await;
                }
            }
        }
        Ok(false)
    }

    async fn prompted(&mut self, k: KeyEvent) -> Result<bool> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let jump = self.mode == Mode::Jump;
        match k.code {
            KeyCode::Esc => self.mode = self.back,
            KeyCode::Backspace if self.prompt.text.is_empty() => {
                self.mode = self.back;
            }
            KeyCode::Down | KeyCode::Tab if jump => self.pick += 1,
            KeyCode::Char('n') if jump && ctrl => self.pick += 1,
            KeyCode::Up | KeyCode::BackTab if jump => {
                self.pick = self.pick.saturating_sub(1);
            }
            KeyCode::Char('p') if jump && ctrl => {
                self.pick = self.pick.saturating_sub(1);
            }
            KeyCode::Enter => {
                let (mode, q) = (self.mode, self.prompt.take());
                self.mode =
                    if mode == Mode::Cmd { Mode::Normal } else { self.back };
                match mode {
                    Mode::Cmd => return self.submit(&q).await,
                    Mode::Find => {
                        (self.found, self.sel) = (q, None);
                        self.find(-1)?;
                    }
                    _ => {
                        let m = self.matches(&q);
                        let to = m.get(self.pick % m.len().max(1));
                        self.go(Some(*to.context("nothing called that")?));
                    }
                }
            }
            _ => {
                if self.prompt.key(k) {
                    self.pick = 0;
                }
            }
        }
        Ok(false)
    }

    fn ask_for(&mut self, mode: Mode, start: &str) {
        if !matches!(self.mode, Mode::Cmd | Mode::Find | Mode::Jump) {
            self.back = self.mode;
        }
        (self.mode, self.pick) = (mode, 0);
        self.prompt.set(start);
    }

    async fn send(&mut self, line: &str, draft: Draft) -> Result<bool> {
        if let Some(cmd) = line.strip_prefix('/') {
            return self.submit(cmd).await;
        }
        if line.is_empty() {
            return Ok(false);
        }
        let ((s, c), text, files) = (self.here()?, line.to_string(), vec![]);
        self.post(match draft {
            Draft::Edit(target) => Op::Edit { target, text: Some(text) },
            Draft::Reply(id) => {
                Op::Post { channel: c, text, reply: Some(id), files }
            }
            Draft::Thread(id) => {
                Op::Post { channel: id.thread(), text, reply: None, files }
            }
            Draft::New => Op::Post { channel: c, text, reply: None, files },
        })
        .await?;
        if let Draft::Thread(id) = draft {
            self.go(Some((s, Some(id.thread()))));
        }
        (self.sel, self.typed) = (None, None);
        Ok(false)
    }

    fn edit(&mut self, id: Id) -> Result<()> {
        let m = self.msgs().into_iter().find(|m| m.id == id);
        self.field.set(&m.context("not here")?.text);
        (self.draft, self.mode) = (Draft::Edit(id), Mode::Insert);
        Ok(())
    }

    async fn typed(&mut self) {
        let due = self.typed.is_none_or(|t| t.elapsed().as_secs() >= 4);
        if let Some((s, Some(c))) = self.at
            && due
            && !self.field.text.starts_with('/')
        {
            self.typed = Some(Instant::now());
            self.n.typing(&s, c).await;
        }
    }

    /// Where we are; a draft stays with its channel.
    fn go(&mut self, at: At) {
        if at == self.at {
            return;
        }
        if let Some((s, Some(c))) = self.at
            && !self.field.text.is_empty()
        {
            self.drafts.insert((s, c), self.field.take());
        }
        (self.at, self.sel, self.draft) = (at, None, Draft::New);
        let d = at.and_then(|(s, c)| self.drafts.remove(&(s, c?)));
        self.field.set(&d.unwrap_or_default());
    }

    fn msgs(&self) -> Vec<Message> {
        let Ok((s, c)) = self.here() else { return vec![] };
        self.n.view(&s, |st| st.messages(&c, None, 300)).unwrap_or_default()
    }

    fn chosen(&self) -> Option<Message> {
        self.msgs().into_iter().find(|m| Some(m.id) == self.sel)
    }

    /// The picked message, else the last (yours, or someone else's).
    fn target(&self, mine: bool) -> Result<Id> {
        match self.chosen() {
            Some(m) if mine && m.author != self.n.id() => bail!("not yours"),
            Some(m) => Ok(m.id),
            None => self.last(mine),
        }
    }

    /// Down (newer) or up `d` messages; past the newest, follow it.
    fn walk(&mut self, d: isize) {
        let ms = self.msgs();
        let i = ms.iter().position(|m| Some(m.id) == self.sel);
        let i = (i.unwrap_or(ms.len()) as isize + d).max(0) as usize;
        self.sel = ms.get(i).map(|m| m.id);
    }

    fn open(&mut self) -> Result<()> {
        let (s, m) =
            (self.space()?, self.chosen().context("j or k picks one")?);
        ensure!(m.thread > 0, "no thread on it: t starts one");
        self.go(Some((s, Some(m.id.thread()))));
        Ok(())
    }

    fn back(&mut self) -> Result<()> {
        let (s, c) = self.here()?;
        let t = self.n.view(&s, |st| st.thread(&c)).flatten();
        let t = t.context("not in a thread")?;
        self.go(Some((s, Some(t.parent))));
        self.sel = Some(t.root);
        Ok(())
    }

    /// The next match of the last search: older (-1) or newer (1).
    fn find(&mut self, d: isize) -> Result<()> {
        ensure!(!self.found.is_empty(), "/ searches here");
        let (q, ms) = (self.found.to_lowercase(), self.msgs());
        let i = ms.iter().position(|m| Some(m.id) == self.sel);
        let i = i.unwrap_or(ms.len()) as isize;
        let hit = |m: &&Message| m.text.to_lowercase().contains(&q);
        let m = if d < 0 {
            ms[..i as usize].iter().rev().find(hit)
        } else {
            ms.get(i as usize + 1..).unwrap_or_default().iter().find(hit)
        };
        self.sel = Some(m.with_context(|| format!("no more {q:?} here"))?.id);
        Ok(())
    }

    fn matches(&self, q: &str) -> Vec<(Id, Option<Ch>)> {
        let q = q.to_lowercase();
        let rows = self.rows().into_iter().filter(|r| {
            let name = self.label(r).to_lowercase();
            q.split_whitespace().all(|w| name.contains(w))
        });
        rows.collect()
    }

    fn label(&self, (s, c): &(Id, Option<Ch>)) -> String {
        let c = c.map(|c| self.channel_name(s, &c)).unwrap_or_default();
        format!("{} {}", self.space_name(s), c.trim())
    }

    fn here(&self) -> Result<(Id, Ch)> {
        let (s, c) =
            self.at.ok_or_else(|| anyhow!("/create or /join first"))?;
        Ok((s, c.ok_or_else(|| anyhow!("no channel here yet"))?))
    }

    fn space(&self) -> Result<Id> {
        self.at.map(|a| a.0).ok_or_else(|| anyhow!("/create or /join first"))
    }

    fn last(&self, mine: bool) -> Result<Id> {
        let ((s, c), me) = (self.here()?, self.n.id());
        let msgs = self.n.view(&s, |st| st.messages(&c, None, 200));
        let m = msgs.unwrap_or_default().into_iter().rev();
        let mut m = m.filter(|m| (m.author == me) == mine).map(|m| m.id);
        m.next().ok_or_else(|| anyhow!("no such message here"))
    }

    fn user(&self, q: &str) -> Result<Id> {
        let q = q.trim_start_matches('@').to_lowercase();
        let ms =
            self.n.view(&self.space()?, |s| s.members()).unwrap_or_default();
        let hit = |m: &&irch_core::Member| {
            m.name.to_lowercase() == q || m.id.to_string().starts_with(&q)
        };
        match ms.iter().filter(hit).collect::<Vec<_>>()[..] {
            [m] => Ok(m.id),
            [] => bail!("nobody called {q} here"),
            _ => bail!("{q} is ambiguous: use the start of their id"),
        }
    }

    async fn post(&self, op: Op) -> Result<()> {
        self.n.publish(&self.space()?, op).await.map(drop)
    }

    /// A command, without its `:` or `/`; true to quit.
    async fn submit(&mut self, cmd: &str) -> Result<bool> {
        let (cmd, arg) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let (n, arg) = (self.n.clone(), arg.trim().to_string());
        let need =
            || if arg.is_empty() { bail!("/{cmd} what?") } else { Ok(()) };
        match cmd {
            "quit" | "q" | "qa" | "wq" => return Ok(true),
            "help" | "h" => self.notice = Some(HELP.into()),
            "create" => self.go(Some((n.create(&arg).await?, None))),
            "b" => {
                let to = self.matches(&arg).first().copied();
                self.go(Some(to.context("nothing called that")?));
            }
            "join" => {
                self.go(Some((n.join(&arg).await?, None)));
                self.status = "joined: history arrives from any member".into();
            }
            "invite" => {
                let code = n.invite(&self.space()?).await?;
                osc52(&code);
                self.notice = Some(format!(
                    "Invite, copied if your terminal allows. Anyone \
                     holding it can read and post until /rekey; share it \
                     privately.\n\n{code}"
                ));
            }
            "leave" => {
                let s = self.space()?;
                self.status = match self.dm(&s) {
                    true => {
                        "Delete this conversation from your devices? \
                             They keep theirs. y/n"
                    }
                    false => {
                        "Leave this space? Its history goes from your \
                              devices; coming back takes an invite. y/n"
                    }
                }
                .into();
                self.ask = Some(Ask::Leave(s));
            }
            "close" => {
                let s = self.space()?;
                ensure!(self.dm(&s), "only a DM closes; :leave leaves a space");
                n.hide(&s, true);
                self.go(None);
                self.status =
                    "closed: back with its next message, or :dm".into();
            }
            "nick" => n.set_name(&arg).await?,
            "rename" => self.post(Op::Rename { name: arg }).await?,
            "channel" => {
                need()?;
                let id = Ch(rand());
                self.post(Op::Channel { id, name: Some(arg) }).await?;
                self.go(self.at.map(|(s, _)| (s, Some(id))));
            }
            "thread" => {
                need()?;
                let (s, c) = self.here()?;
                let room = |c| n.view(&s, |st| st.thread(&c)).flatten();
                ensure!(room(c).is_none(), "threads hang off channels");
                let channel = self.target(false)?.thread();
                let (text, files) = (arg, vec![]);
                self.post(Op::Post { channel, text, reply: None, files })
                    .await?;
                self.go(Some((s, Some(channel))));
            }
            "search" => {
                need()?;
                let (s, _) = self.here()?;
                let found = |st: &irch_core::State| {
                    let rooms = st.rooms();
                    let name = |c: &Ch| {
                        let r = rooms.iter().find(|r| r.0 == *c);
                        r.map_or("a thread".into(), |r| r.1.clone())
                    };
                    let hits = st.search(&arg, None, 20).into_iter();
                    let line = |(c, m): (Ch, irch_core::Message)| {
                        let text = m.text.lines().next().unwrap_or_default();
                        format!("{} <{}> {}", name(&c), m.name, text)
                    };
                    hits.map(line).collect::<Vec<_>>().join("\n")
                };
                let got = n.view(&s, found).unwrap_or_default();
                self.notice = Some(if got.is_empty() {
                    format!("Nothing here says {arg:?}.")
                } else {
                    format!("Newest first, in this space:\n\n{got}")
                });
            }
            "room" => {
                need()?;
                let (s, c) = self.here()?;
                let thread = match n.view(&s, |st| st.thread(&c)).flatten() {
                    Some(t) => t.id,
                    None => {
                        let (text, files) = (arg.clone(), vec![]);
                        let op =
                            Op::Post { channel: c, text, reply: None, files };
                        n.publish(&s, op).await?.thread()
                    }
                };
                self.post(Op::Topic { thread, name: arg }).await?;
                self.go(Some((s, Some(thread))));
            }
            "note" | "todo" => {
                let (s, c) = self.here()?;
                let id = self.target(false)?;
                let thread = n.view(&s, |st| st.thread(&c).is_some());
                let (thread, todo) = (thread == Some(true), cmd == "todo");
                let md = |st: &irch_core::State| {
                    notes::markdown(st, &s, &id, thread, todo, None)
                };
                let md = n.view(&s, md).flatten().context("nothing here")?;
                let mut file =
                    if arg.is_empty() { "Inbox".into() } else { arg };
                if !file.ends_with(".md") {
                    file += ".md";
                }
                n.note(&file, &md)?;
                self.status = format!("added to {file}");
            }
            "rmchannel" => {
                let id = self.here()?.1;
                self.post(Op::Channel { id, name: None }).await?;
            }
            "edit" | "delete" => {
                let target = self.target(true)?;
                let text = (cmd == "edit").then_some(arg);
                self.post(Op::Edit { target, text }).await?;
            }
            "reply" => {
                let (reply, channel) =
                    (Some(self.target(false)?), self.here()?.1);
                let files = vec![];
                self.post(Op::Post { channel, text: arg, reply, files })
                    .await?;
            }
            "react" => {
                need()?;
                let target = self.target(false)?;
                self.post(Op::React { target, emoji: arg, on: true }).await?;
            }
            "file" => {
                let (path, text) = arg.split_once(' ').unwrap_or((&arg, ""));
                let p = std::path::Path::new(path);
                let name =
                    p.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                let (space, channel) = self.here()?;
                let files = vec![n.seal_file(&space, p, name, None).await?];
                let text = text.to_string();
                self.post(Op::Post { channel, text, reply: None, files })
                    .await?;
            }
            "save" => {
                let (s, c) = self.here()?;
                let msgs = n.view(&s, |st| st.messages(&c, None, 200));
                let mut f = msgs.unwrap_or_default().into_iter().rev();
                let f = f.find_map(|m| m.files.last().cloned());
                let f = f.ok_or_else(|| anyhow!("no file here"))?;
                let p = n.save(&s, &f, &std::env::current_dir()?).await?;
                self.status = format!("saved {}", p.display());
            }
            "admin" | "member" => {
                let role =
                    if cmd == "admin" { Role::Admin } else { Role::Member };
                self.post(Op::Role { user: self.user(&arg)?, role }).await?;
            }
            "ban" | "unban" => {
                let s = self.space()?;
                n.ban(&s, &self.user(&arg)?, cmd == "ban").await?;
            }
            "rekey" => {
                n.rekey(&self.space()?).await?;
                self.status = "new key: earlier invites no longer work".into();
            }
            "dm" => {
                let user = self.user(&arg)?;
                self.go(Some((n.dm(&self.space()?, &user).await?, None)));
            }
            "url" => {
                let (s, c) = self.here()?;
                let m = self.chosen().map(|m| format!("/{}", m.id));
                osc52(&format!("irch://open/{s}/{c}{}", m.unwrap_or_default()));
                self.status = "its link, copied if your terminal allows".into();
            }
            "link" => {
                self.notice = Some(format!(
                    "Start irch on your other device with\n\n  irch --link \
                     {}\n\nIt becomes you: keep this secret.",
                    n.link_code().await
                ));
            }
            "share" => {
                let (path, text) = arg.split_once(' ').unwrap_or((&arg, ""));
                let files =
                    vec![n.offer(std::path::Path::new(path), None).await?];
                let (channel, text) = (self.here()?.1, text.to_string());
                self.post(Op::Post { channel, text, reply: None, files })
                    .await?;
                self.status = "shared from this machine while it runs".into();
            }
            "store" if arg == "local" => {
                let code = irch_core::local_storage().ok_or_else(|| {
                    anyhow!("no storage node on this computer")
                })?;
                n.add_storage(&code).await?;
                self.status = "paired: /store here keeps a space there".into();
            }
            "store" if arg == "here" => {
                let id = self.space()?;
                let v = n.stored(&id);
                ensure!(!v.is_empty(), "/store CODE or /store local first");
                for (node, _) in v {
                    n.store(&id, &node, true).await?;
                }
                self.status = "this space now also lives there".into();
            }
            "store" => {
                n.add_storage(&arg).await?;
                self.status = "paired: /store here keeps a space there".into();
            }
            "invisible" => {
                n.set_invisible(!n.invisible()).await;
                let on = if n.invisible() { "invisible" } else { "visible" };
                self.status = on.into();
            }
            _ => bail!("no /{cmd}: try /help"),
        }
        Ok(false)
    }

    /// Channels, each with its threads of this week; a syncing space alone.
    fn rows(&self) -> Vec<(Id, Option<Ch>)> {
        let mut v = vec![];
        let week = irch_core::now().saturating_sub(7 * 86_400_000);
        for s in self.n.spaces() {
            if self.n.hidden(&s) && self.at.map(|a| a.0) != Some(s) {
                continue; // a closed DM
            }
            let rooms = self.n.view(&s, |st| (st.channels(), st.threads()));
            let (chans, threads) = rooms.unwrap_or_default();
            if chans.is_empty() {
                v.push((s, None));
            }
            for (c, _) in chans {
                v.push((s, Some(c)));
                let t = threads.iter().filter(|t| t.parent == c);
                v.extend(t.filter(|t| t.last > week).map(|t| (s, Some(t.id))));
            }
        }
        v
    }

    /// To the next channel, or space.
    fn step(&mut self, d: isize, space: bool) {
        let mut rows = self.rows();
        if space {
            rows.dedup_by_key(|r| r.0);
        }
        let here = |r: &(Id, Option<Ch>)| match space {
            true => Some(r.0) == self.at.map(|a| a.0),
            false => Some(*r) == self.at,
        };
        let i = rows.iter().position(here).unwrap_or(0) as isize;
        let i = (i + d).rem_euclid(rows.len().max(1) as isize);
        self.go(rows.get(i as usize).copied());
    }

    fn fix(&mut self) {
        let rows = self.rows();
        if !rows.iter().any(|r| Some(*r) == self.at) {
            let space = self.at.map(|a| a.0);
            let same = rows.iter().find(|r| Some(r.0) == space);
            self.go(same.or(rows.first()).copied());
        }
    }

    fn dm(&self, s: &Id) -> bool {
        self.n.view(s, |st| st.dm.is_some()) == Some(true)
    }

    fn draw(&self, f: &mut Frame) {
        let [main, status] =
            Layout::vertical([Min(1), Length(1)]).areas(f.area());
        let [side, chat, people] =
            Layout::horizontal([Length(24), Min(20), Length(22)]).areas(main);
        let [msgs, typing, input] =
            Layout::vertical([Min(1), Length(1), Length(3)]).areas(chat);
        self.sidebar(f, side);
        self.messages(f, msgs);
        self.members(f, people);
        let who = self.typing_line();
        f.render_widget(Paragraph::new(who).italic().dark_gray(), typing);
        let title = match self.here() {
            Ok((s, _)) if self.n.locked(&s) => {
                " keys moved on without you: ask for an invite ".into()
            }
            Ok((s, c)) => format!(" {} ", self.channel_name(&s, &c)),
            Err(_) => " /help ".into(),
        };
        let who = |id: Id| {
            let m = self.msgs().into_iter().find(|m| m.id == id);
            m.map(|m| m.name).unwrap_or_default()
        };
        let title = match self.draft {
            Draft::New => title,
            Draft::Reply(id) => format!(" ↳ reply to {} ", who(id)),
            Draft::Thread(id) => format!(" ↳ a thread on {}'s ", who(id)),
            Draft::Edit(_) => " editing (Esc keeps it) ".into(),
        };
        // The cursor in view: the line scrolls under it.
        let w = input.width.saturating_sub(3) as usize;
        let skip = self.field.at.saturating_sub(w);
        let text: String = self.field.text.chars().skip(skip).collect();
        f.render_widget(Paragraph::new(text).block(panel(title)), input);
        let (mode, color) = match self.mode {
            Mode::Insert => (" INSERT ", Color::Cyan),
            _ => (" NORMAL ", Color::Magenta),
        };
        let prompt = match self.mode {
            Mode::Cmd => Some(":"),
            Mode::Find => Some("/"),
            Mode::Jump => Some("jump to: "),
            _ => None,
        };
        let line = match prompt {
            Some(p) => Line::from(format!("{p}{}", self.prompt.text)),
            None => Line::from(vec![
                Span::styled(mode, Style::new().fg(Color::Black).bg(color)),
                Span::raw(format!(" {}", self.status)).dark_gray(),
            ]),
        };
        f.render_widget(Paragraph::new(line), status);
        match prompt {
            Some(p) => {
                let x = (p.chars().count() + self.prompt.at) as u16;
                f.set_cursor_position((status.x + x, status.y));
            }
            None if self.mode == Mode::Insert => {
                let x = input.x + 1 + (self.field.at - skip) as u16;
                f.set_cursor_position((x.min(input.right() - 2), input.y + 1));
            }
            None => {}
        }
        if self.mode == Mode::Jump {
            let m = self.matches(&self.prompt.text);
            let pick = self.pick % m.len().max(1);
            let lines: Vec<Line> = (m.iter().enumerate().take(12))
                .map(|(i, r)| {
                    let l = Line::from(format!(" {}", self.label(r)));
                    if i == pick { l.black().on_magenta() } else { l }
                })
                .collect();
            let area = center(f.area(), 48, lines.len() as u16 + 2);
            f.render_widget(Clear, area);
            let p = Paragraph::new(lines).block(panel(" ↑↓ Enter "));
            f.render_widget(p, area);
        }
        if let Some(text) = &self.notice {
            let area = center(f.area(), 76, text.lines().count() as u16 + 2);
            let b = panel(" esc to close ");
            let p = Paragraph::new(text.as_str()).wrap(Wrap { trim: false });
            f.render_widget(Clear, area);
            f.render_widget(p.block(b), area);
        }
    }

    /// Once a row a frame: this room alone, not every thread's.
    fn channel_name(&self, s: &Id, c: &Ch) -> String {
        let named = |st: &irch_core::State| {
            let chan = st.channels().into_iter().find(|x| x.0 == *c);
            (st.thread(c).map(|t| t.name), chan.map(|x| x.1), st.dm.is_some())
        };
        match self.n.view(s, named).unwrap_or_default() {
            (Some(t), ..) => format!("  ↳ {t}"),
            (_, _, true) => format!("@{}", self.space_name(s)),
            (_, n, _) => format!("#{}", n.unwrap_or_default()),
        }
    }

    fn space_name(&self, s: &Id) -> String {
        let me = self.n.id();
        let v = self.n.view(s, |st| match st.peer(&me) {
            Some(p) => st.display_name(&p),
            None => st.name().to_string(),
        });
        v.unwrap_or_default()
    }

    fn sidebar(&self, f: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = vec![];
        let mut last = None;
        for (s, c) in self.rows() {
            if last != Some(s) {
                let peers = self.n.peers(&s);
                let dot = if peers > 0 { "●" } else { "○" };
                let name = format!("{dot} {}", self.space_name(&s));
                lines.push(Line::from(name).bold().magenta());
                last = Some(s);
            }
            let Some(c) = c else { continue };
            let (unread, mentions) = self.n.unread(&s, &c);
            let mut l = format!("  {}", self.channel_name(&s, &c));
            if mentions > 0 {
                l += &format!(" ({mentions})");
            }
            // Calls happen in the app; here, who is in one.
            let calls = self.n.calls(&s).get(&c).map_or(0, Vec::len);
            if calls > 0 {
                l += &format!(" 🔊{calls}");
            }
            let mut style = Style::new();
            if Some((s, Some(c))) == self.at {
                style = style.black().on_magenta();
            } else if unread > 0 {
                style = style.bold().white();
            } else {
                style = style.dark_gray();
            }
            lines.push(Line::styled(l, style));
        }
        let b = panel(" irch ");
        f.render_widget(Paragraph::new(lines).block(b), area);
    }

    fn messages(&self, f: &mut Frame, area: Rect) {
        let Ok((s, c)) = self.here() else {
            let hi = "Welcome to irch.\n\n/create NAME starts a space, \
                      /join CODE enters one.\n/link puts you on another \
                      device. /help lists everything.";
            let p = Paragraph::new(hi).wrap(Wrap { trim: false });
            return f.render_widget(p.block(panel("")), area);
        };
        let (w, h) =
            (area.width.saturating_sub(2), area.height.saturating_sub(2));
        let (mut lines, mut row, mut span) = (vec![], 0, None);
        // In a thread, what it hangs off first.
        let root = |st: &irch_core::State| st.message(&st.thread(&c)?.root);
        if let Some((_, m)) = self.n.view(&s, root).flatten() {
            let ls = self.lines(&Message { thread: 0, ..m }).into_iter();
            lines =
                ls.map(|l| Line::from(format!(" {l}")).dark_gray()).collect();
            lines.push(Line::from("─".repeat(w as usize)).dark_gray());
            let p = Paragraph::new(lines.clone()).wrap(Wrap { trim: false });
            row = p.line_count(w);
        }
        for m in self.msgs() {
            let picked = Some(m.id) == self.sel;
            let bar = Span::raw(if picked { "▌" } else { " " }).magenta();
            let ls: Vec<Line> = (self.lines(&m).into_iter())
                .map(|mut l| {
                    l.spans.insert(0, bar.clone());
                    l
                })
                .collect();
            let rows = Paragraph::new(ls.clone()).wrap(Wrap { trim: false });
            let rows = rows.line_count(w);
            if picked {
                span = Some((row, row + rows));
            }
            row += rows;
            lines.extend(ls);
        }
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        let (total, h) = (p.line_count(w), h as usize);
        let top = match span {
            None => total.saturating_sub(h),
            Some((a, b)) => {
                let t = self.top.get().max(b.saturating_sub(h)).min(a);
                t.min(total.saturating_sub(h))
            }
        };
        self.top.set(top);
        let b = panel(format!(" {} ", self.space_name(&s)));
        f.render_widget(p.scroll((top as u16, 0)).block(b), area);
    }

    fn lines(&self, m: &Message) -> Vec<Line<'static>> {
        let mut out = vec![];
        if let Some(q) = &m.quote {
            let q = format!(
                "  ↳ {}: {}",
                q.name,
                q.text.lines().next().unwrap_or("")
            );
            out.push(Line::from(q).dark_gray());
        }
        let time =
            chrono::DateTime::from_timestamp_millis(m.ts as i64).map(|t| {
                t.with_timezone(&chrono::Local).format("%H:%M").to_string()
            });
        let me = m.author == self.n.id();
        let color = if me { Color::Cyan } else { hue(&m.author) };
        let mut head = vec![
            Span::raw(time.unwrap_or_default() + " ").dark_gray(),
            Span::styled(m.name.clone(), Style::new().fg(color).bold()),
            Span::raw(": "),
        ];
        let mut text = m.text.lines().map(|l| l.to_string());
        head.push(Span::raw(text.next().unwrap_or_default()));
        if m.edited {
            head.push(Span::raw(" (edited)").dark_gray());
        }
        out.push(Line::from(head));
        out.extend(text.map(|l| Line::from(format!("      {l}"))));
        for f in &m.files {
            let l = format!("      📎 {} ({})", f.name, size(f.size));
            out.push(Line::from(l).cyan());
        }
        if !m.reactions.is_empty() {
            let r: Vec<_> = m
                .reactions
                .iter()
                .map(|(e, u)| format!("{e} {}", u.len()))
                .collect();
            out.push(Line::from(format!("      {}", r.join("  "))).dark_gray());
        }
        if m.thread > 0 {
            let l = format!("      ↳ {} in its thread", m.thread);
            out.push(Line::from(l).cyan());
        }
        out
    }

    fn members(&self, f: &mut Frame, area: Rect) {
        let Ok(s) = self.space() else {
            return f.render_widget(panel(""), area);
        };
        let on = self.n.online(&s);
        let ms = self.n.view(&s, |st| st.members()).unwrap_or_default();
        let lines: Vec<Line> = ms
            .iter()
            .map(|m| {
                let mark = match m.role {
                    Role::Admin => "★ ",
                    Role::Banned => "✗ ",
                    Role::Member => "  ",
                };
                let l = Line::from(format!("{mark}{}", m.name));
                if on.contains(&m.id) { l.white() } else { l.dark_gray() }
            })
            .collect();
        let b = panel(format!(" {} online ", on.len()));
        f.render_widget(Paragraph::new(lines).block(b), area);
    }

    fn typing_line(&self) -> String {
        let Ok((s, c)) = self.here() else { return String::new() };
        let now = Instant::now();
        let who = self.typing.get(&(s, c)).into_iter().flatten();
        let who: Vec<_> = who
            .filter(|(_, t)| **t > now)
            .filter_map(|(u, _)| self.n.view(&s, |st| st.display_name(u)))
            .collect();
        match who.len() {
            0 => String::new(),
            1 => format!("{} is typing…", who[0]),
            _ => format!("{} are typing…", who.join(", ")),
        }
    }
}

fn size(n: u64) -> String {
    match n {
        n if n < 1 << 10 => format!("{n} B"),
        n if n < 1 << 20 => format!("{} KB", n >> 10),
        n => format!("{:.1} MB", n as f64 / (1 << 20) as f64),
    }
}

fn hue(id: &Id) -> Color {
    use Color::*;
    let c = [Magenta, LightMagenta, Cyan, LightBlue, LightRed];
    c[id.0[0] as usize % c.len()]
}

fn center(r: Rect, w: u16, h: u16) -> Rect {
    let (w, h) = (w.min(r.width), h.min(r.height));
    Rect::new(r.x + (r.width - w) / 2, r.y + (r.height - h) / 2, w, h)
}

fn rand() -> [u8; 8] {
    irch_core::crypto::rand()
}

fn osc52(text: &str) {
    use std::io::Write;
    let b64 = data_encoding::BASE64.encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07").and_then(|_| out.flush());
}

fn panel<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::bordered().border_style(Color::Magenta).title(title)
}
