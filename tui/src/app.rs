//! The terminal client: the core's state on screen, IRC-style commands.

use anyhow::{Result, anyhow, bail};
use crossterm::event::{Event as Term, EventStream, KeyCode, KeyEvent};
use crossterm::event::{KeyEventKind, KeyModifiers};
use irch_core::{Ch, Event, Id, Message, Node, Op, Role};
use n0_future::StreamExt;
use ratatui::layout::{Constraint::*, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const HELP: &str = "\
/create NAME     a new space        /join CODE   join with an invite
/invite          invite code        /leave       leave this space
/channel NAME    new channel        /rmchannel   delete this channel
/rename NAME     rename space       /nick NAME   your name everywhere
/reply TEXT      reply to the last message by someone else
/react EMOJI     react to it        /edit TEXT   edit your last message
/delete          delete your last   /dm USER     a direct message
/admin USER  /member USER  /ban USER  /unban USER  (admins)
/rekey           new key, old invites stop working (admins)
/link            become you on another device (keep it secret)
/store CODE      keep your spaces on a storage node
/file PATH TEXT  send a file        /save        save the last file here
/invisible       no presence or typing    /quit
Ctrl-N/P or Alt-Up/Down: channel   PgUp/PgDn: scroll   Esc: close";

type Typing = HashMap<(Id, Ch), HashMap<Id, Instant>>;

struct App {
    n: Arc<Node>,
    at: Option<(Id, Option<Ch>)>,
    input: String,
    status: String,
    notice: Option<String>,
    scroll: u16,
    typing: Typing,
    typed: Option<Instant>,
}

pub async fn run(n: Arc<Node>) -> Result<()> {
    let mut t = ratatui::init();
    let status = "/help for commands".into();
    let (input, notice, typing) = (String::new(), None, HashMap::new());
    let mut a = App {
        n,
        at: None,
        input,
        status,
        notice,
        scroll: 0,
        typing,
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
        match k.code {
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Char('n') if ctrl => self.step(1),
            KeyCode::Char('p') if ctrl => self.step(-1),
            KeyCode::Down if ctrl || alt => self.step(1),
            KeyCode::Up if ctrl || alt => self.step(-1),
            KeyCode::Char('u') if ctrl => self.input.clear(),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Esc => (self.notice, self.scroll) = (None, 0),
            KeyCode::Backspace => drop(self.input.pop()),
            KeyCode::Char(c) if !ctrl => {
                self.input.push(c);
                self.typed().await;
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.input);
                match self.submit(line.trim()).await {
                    Ok(quit) => return quit,
                    Err(e) => self.status = format!("{e:#}"),
                }
            }
            _ => {}
        }
        false
    }

    /// Tell the channel we are typing, at most every four seconds.
    async fn typed(&mut self) {
        let due = self.typed.is_none_or(|t| t.elapsed().as_secs() >= 4);
        if let Some((s, Some(c))) = self.at
            && due
            && !self.input.starts_with('/')
        {
            self.typed = Some(Instant::now());
            self.n.typing(&s, c).await;
        }
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

    /// True to quit.
    async fn submit(&mut self, line: &str) -> Result<bool> {
        let Some(cmd) = line.strip_prefix('/') else {
            if !line.is_empty() {
                let (channel, text) = (self.here()?.1, line.into());
                let files = vec![];
                self.post(Op::Post { channel, text, reply: None, files })
                    .await?;
                (self.scroll, self.typed) = (0, None);
            }
            return Ok(false);
        };
        let (cmd, arg) = cmd.split_once(' ').unwrap_or((cmd, ""));
        let (n, arg) = (self.n.clone(), arg.trim().to_string());
        let need =
            || if arg.is_empty() { bail!("/{cmd} what?") } else { Ok(()) };
        match cmd {
            "quit" | "q" => return Ok(true),
            "help" | "h" => self.notice = Some(HELP.into()),
            "create" => self.at = Some((n.create(&arg).await?, None)),
            "join" => {
                self.at = Some((n.join(&arg).await?, None));
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
            "leave" => n.leave(&self.space()?).await?,
            "nick" => n.set_name(&arg).await?,
            "rename" => self.post(Op::Rename { name: arg }).await?,
            "channel" => {
                need()?;
                let id = Ch(rand());
                self.post(Op::Channel { id, name: Some(arg) }).await?;
                self.at = self.at.map(|(s, _)| (s, Some(id)));
            }
            "rmchannel" => {
                let id = self.here()?.1;
                self.post(Op::Channel { id, name: None }).await?;
            }
            "edit" | "delete" => {
                let target = self.last(true)?;
                let text = (cmd == "edit").then_some(arg);
                self.post(Op::Edit { target, text }).await?;
            }
            "reply" => {
                let (reply, channel) =
                    (Some(self.last(false)?), self.here()?.1);
                let files = vec![];
                self.post(Op::Post { channel, text: arg, reply, files })
                    .await?;
            }
            "react" => {
                let target = self.last(false)?;
                self.post(Op::React { target, emoji: arg, on: true }).await?;
            }
            "file" => {
                let (path, text) = arg.split_once(' ').unwrap_or((&arg, ""));
                let data = std::fs::read(path)?;
                let name = std::path::Path::new(path).file_name();
                let name = name.and_then(|n| n.to_str()).unwrap_or("file");
                let files = vec![n.attach(name, &data).await?];
                let (channel, text) = (self.here()?.1, text.to_string());
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
                self.at = Some((n.dm(&self.space()?, &user).await?, None));
            }
            "link" => {
                self.notice = Some(format!(
                    "Start irch on your other device with\n\n  irch --link \
                     {}\n\nIt becomes you: keep this secret.",
                    n.link_code().await
                ));
            }
            "store" => {
                n.add_storage(&arg).await?;
                self.status = "your spaces now also live there".into();
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

    /// Sidebar rows: each space's channels, or the space while it syncs.
    fn rows(&self) -> Vec<(Id, Option<Ch>)> {
        let mut v = vec![];
        for s in self.n.spaces() {
            let chans = self.n.view(&s, |st| st.channels()).unwrap_or_default();
            if chans.is_empty() {
                v.push((s, None));
            }
            v.extend(chans.into_iter().map(|(c, _)| (s, Some(c))));
        }
        v
    }

    fn step(&mut self, d: isize) {
        let rows = self.rows();
        let i = rows.iter().position(|r| Some(*r) == self.at).unwrap_or(0);
        let i = (i as isize + d).rem_euclid(rows.len().max(1) as isize);
        (self.at, self.scroll) = (rows.get(i as usize).copied(), 0);
    }

    /// Keep the selection pointing at something that still exists.
    fn fix(&mut self) {
        let rows = self.rows();
        let space = self.at.map(|a| a.0);
        if !rows.iter().any(|r| Some(*r) == self.at) {
            let same = rows.iter().find(|r| Some(r.0) == space);
            self.at = same.or(rows.first()).copied();
        }
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
        let b = panel(title);
        f.render_widget(Paragraph::new(self.input.as_str()).block(b), input);
        let x = input.x + 1 + self.input.chars().count() as u16;
        f.set_cursor_position((x.min(input.right() - 2), input.y + 1));
        f.render_widget(
            Paragraph::new(self.status.as_str()).dark_gray(),
            status,
        );
        if let Some(text) = &self.notice {
            let area = center(f.area(), 74, 18);
            let b = panel(" esc to close ");
            let p = Paragraph::new(text.as_str()).wrap(Wrap { trim: false });
            f.render_widget(Clear, area);
            f.render_widget(p.block(b), area);
        }
    }

    fn channel_name(&self, s: &Id, c: &Ch) -> String {
        let chans = self.n.view(s, |st| st.channels()).unwrap_or_default();
        let name = chans.into_iter().find(|x| x.0 == *c).map(|x| x.1);
        let dm = self.n.view(s, |st| st.dm.is_some()) == Some(true);
        match (dm, name) {
            (true, _) => format!("@{}", self.space_name(s)),
            (_, n) => format!("#{}", n.unwrap_or_default()),
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
        let msgs = self.n.view(&s, |st| st.messages(&c, None, 300));
        let lines: Vec<Line> = msgs
            .unwrap_or_default()
            .iter()
            .flat_map(|m| self.lines(m))
            .collect();
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        let (w, h) =
            (area.width.saturating_sub(2), area.height.saturating_sub(2));
        let top = (p.line_count(w) as u16).saturating_sub(h + self.scroll);
        let b = panel(format!(" {} ", self.space_name(&s)));
        f.render_widget(p.scroll((top, 0)).block(b), area);
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

/// Ask the terminal to put `text` on the clipboard.
fn osc52(text: &str) {
    use std::io::Write;
    let b64 = data_encoding::BASE64.encode(text.as_bytes());
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b]52;c;{b64}\x07").and_then(|_| out.flush());
}

/// Neon edges, as in the app.
fn panel<'a>(title: impl Into<Line<'a>>) -> Block<'a> {
    Block::bordered().border_style(Color::Magenta).title(title)
}
