// irch: a Discord-style page over irch_core, through one Tauri command.
// The core is the only source of truth: its events schedule a refresh of
// what they touch, at most once a frame. Message rows are keyed and reused,
// the window is paged as you scroll and trimmed back at the bottom.
// User content only ever becomes text nodes; links are copied, not opened.

import { inline, markdown } from "./markdown.js";

const T = window.__TAURI__;
const invoke = T?.core?.invoke;
const clip = T?.clipboardManager;
const rpc = (cmd, args) => invoke("irch", { call: { cmd, ...args } });

// ---------- helpers ----------

const $ = (id) => document.getElementById(id);
const short = (id) => id.slice(0, 8);
const randomId = (n) =>
  [...crypto.getRandomValues(new Uint8Array(n))]
    .map((b) => b.toString(16).padStart(2, "0")).join("");
const mobile = () => matchMedia("(max-width: 760px)").matches;
const android = /Android/.test(navigator.userAgent);

function h(tag, props, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props ?? {})) {
    if (v == null || v === false) continue;
    if (k === "class") e.className = v;
    else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
    else if (k in e) e[k] = v;
    else e.setAttribute(k, v);
  }
  e.append(...kids.flat(Infinity).filter((x) => x != null && x !== false));
  return e;
}

/** replaceChildren that skips null/false, for inline conditionals. */
const put = (el, ...kids) =>
  el.replaceChildren(
    ...kids.flat(Infinity).filter((x) => x != null && x !== false));

const ICONS = {
  hash: "M9 3 7 21M17 3l-2 18M4 9h16M3 15h16",
  plus: "M12 5v14M5 12h14",
  chevron: "m6 9 6 6 6-6",
  x: "M6 6l12 12M18 6 6 18",
  users: "M17 20v-1a4 4 0 0 0-4-4H7a4 4 0 0 0-4 4v1M10 11a3.5 3.5 0 1 0 0-7 " +
    "3.5 3.5 0 0 0 0 7M21 20v-1a4 4 0 0 0-3-3.9M15.5 4.1a3.5 3.5 0 0 1 0 6.8",
  reply: "M10 8 5 12l5 4M5 12h9a5 5 0 0 1 5 5v1",
  smile: "M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18M8.5 14.5s1.3 1.5 3.5 1.5 " +
    "3.5-1.5 3.5-1.5M9 9.5h.01M15 9.5h.01",
  pencil: "M4 20h4L19 9l-4-4L4 16v4M13.5 6.5l4 4",
  trash: "M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3",
  gear: "M12 15.5a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7M12 2v3M12 19v3M2 " +
    "12h3M19 12h3M4.9 4.9 7 7M17 17l2.1 2.1M4.9 19.1 7 17M17 7l2.1-2.1",
  eye: "M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12M12 15a3 3 0 1 0 " +
    "0-6 3 3 0 0 0 0 6",
  eyeOff: "M2 12s3.6-7 10-7 10 7 10 7-3.6 7-10 7S2 12 2 12M12 15a3 3 0 1 0 " +
    "0-6 3 3 0 0 0 0 6M3 3l18 18",
  menu: "M4 6h16M4 12h16M4 18h16",
  lock: "M6 11h12v9H6zM8.5 11V8a3.5 3.5 0 0 1 7 0v3",
  link: "M10 14a4 4 0 0 0 5.7 0l3-3a4 4 0 0 0-5.7-5.7l-1 1M14 10a4 4 0 0 " +
    "0-5.7 0l-3 3a4 4 0 0 0 5.7 5.7l1-1",
  logout: "M15 4h4v16h-4M10 8l-4 4 4 4M6 12h10",
  userPlus: "M15 20v-1a4 4 0 0 0-4-4H7a4 4 0 0 0-4 4v1M9 11a3.5 3.5 0 1 0 " +
    "0-7 3.5 3.5 0 0 0 0 7M19 8v6M16 11h6",
  chat: "M4 5h16v11H8l-4 4z",
  key: "M15 9a3 3 0 1 0 0-.01M12.5 11.5 4 20M7 17l2 2M9 15l2 2",
  phone: "M8 3h8v18H8zM11 18h2",
  disk: "M4 6h16v5H4zM4 13h16v5H4zM7 8.5h.01M7 15.5h.01",
  clip: "M8 12.5 14 6.5a3 3 0 0 1 4.2 4.2l-7.6 7.6a5 5 0 0 1-7-7L11 3.8",
  file: "M6 3h8l4 4v14H6zM14 3v4h4",
  down: "M12 4v11M7 10l5 5 5-5M5 20h14",
};

function icon(name) {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("class", "icon");
  const p = document.createElementNS(ns, "path");
  p.setAttribute("d", ICONS[name]);
  svg.append(p);
  return svg;
}

const COLORS = ["#5865f2", "#3ba55c", "#faa61a", "#ed4245",
  "#eb459e", "#747f8d", "#1abc9c", "#9b59b6"];
const color = (id) => COLORS[parseInt(id.slice(0, 2), 16) % COLORS.length];

function avatar(id, name, cls = "", online) {
  const first = [...(name || "?").trim()][0] ?? "?";
  const a = h("div", { class: `avatar ${cls}` }, first.toUpperCase());
  a.style.background = color(id);
  if (online !== undefined) {
    a.append(h("i", { class: "status" + (online ? " on" : "") }));
  }
  return a;
}

const initials = (name) =>
  name.split(/\s+/).filter(Boolean).slice(0, 3).map((w) => [...w][0])
    .join("").toUpperCase() || "?";

const fmtTime = (ts) =>
  new Date(ts).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
// All timestamps in one 15-minute slot share a calendar day.
const days = new Map();
const fmtDay = (ts) => {
  const slot = Math.floor(ts / 9e5);
  if (!days.has(slot)) {
    days.set(slot, new Date(ts).toLocaleDateString([],
      { year: "numeric", month: "long", day: "numeric" }));
  }
  return days.get(slot);
};
function fmtStamp(ts) {
  const day = (d) =>
    new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const diff = Math.round((day(new Date()) - day(new Date(ts))) / 864e5);
  if (diff === 0) return `Today at ${fmtTime(ts)}`;
  if (diff === 1) return `Yesterday at ${fmtTime(ts)}`;
  return `${new Date(ts).toLocaleDateString()} ${fmtTime(ts)}`;
}

function toast(text, error) {
  const t = h("div", { class: "toast" + (error ? " error" : "") }, text);
  $("toasts").append(t);
  setTimeout(() => t.remove(), 3500);
}

async function call(cmd, args) {
  try {
    return await rpc(cmd, args);
  } catch (e) {
    toast(String(e), true);
    throw e;
  }
}
const quiet = (p) => p.catch(() => {});

async function copy(text, done) {
  await clip?.writeText(text);
  if (done) toast(done);
}

const store = {
  get: (k) => { try { return localStorage.getItem("irch." + k); } catch {} },
  set: (k, v) => { try { localStorage.setItem("irch." + k, v); } catch {} },
};

// ---------- state ----------

const S = {
  me: null, spaces: [], members: [], msgs: [], sid: null, ch: null,
  limit: 50, more: true, loading: false, reply: null, editing: null,
  newFrom: null, unreadAtOpen: 0, stick: false, lastTyping: 0, files: [],
  typing: new Map(), // "space|channel" -> Map(user -> expiry)
};

const cur = () => S.spaces.find((s) => s.id === S.sid);
const chan = () => cur()?.channels.find((c) => c.id === S.ch);
const dms = () => S.spaces.filter((s) => s.dm);
const memberOf = (id) => S.members.find((m) => m.id === id);
const nameOf = (id) => memberOf(id)?.name ?? short(id);
const isMe = (id) => id === S.me.id;

// ---------- refresh scheduling ----------

const need = new Set();
let frame = 0;
let chain = Promise.resolve();

function schedule(...what) {
  what.forEach((w) => need.add(w));
  if (!frame) frame = requestAnimationFrame(flush);
}

function flush() {
  frame = 0;
  const w = new Set(need);
  need.clear();
  chain = chain.then(async () => {
    if (w.has("side")) await loadSidebar();
    if (w.has("members")) await loadMembers();
    if (w.has("msgs")) await loadMessages();
  }).catch((e) => console.error(e));
}

async function loadSidebar() {
  S.spaces = await rpc("spaces");
  if (!cur()) {
    const saved = store.get("sid");
    const s = S.spaces.find((x) => x.id === saved && !x.dm) ??
      S.spaces.find((x) => !x.dm) ?? S.spaces[0];
    return select(s?.id ?? null);
  }
  if (!chan() && cur().channels.length) return select(S.sid);
  renderRail();
  renderSidebar();
  renderHeader();
}

async function loadMembers() {
  const s = cur();
  S.members = s ? await rpc("members", { space: s.id }) : [];
  renderMembers();
}

async function loadMessages() {
  const s = cur();
  const ch = chan();
  if (!s || !ch) {
    Object.assign(S, { msgs: [], more: false });
    return renderMessages();
  }
  if (nearBottom() && S.limit > 100) S.limit = 100; // trim at the bottom
  const msgs = await rpc("messages",
    { space: s.id, channel: ch.id, before: null, limit: S.limit });
  if (s.id !== S.sid || ch.id !== S.ch) return; // selection moved on
  S.msgs = msgs;
  S.more = msgs.length >= S.limit;
  if (S.newFrom === undefined) computeNewFrom();
  const last = msgs.at(-1);
  if (last) S.typing.get(`${S.sid}|${S.ch}`)?.delete(last.author);
  renderMessages();
  renderTyping();
  if (document.hasFocus() && nearBottom()) markRead();
}

async function loadOlder() {
  const s = cur();
  const ch = chan();
  const first = S.msgs[0];
  if (!s || !ch || !first || S.loading || !S.more) return;
  S.loading = true;
  try {
    const older = await rpc("messages", {
      space: s.id, channel: ch.id, before: [first.ts, first.id], limit: 50,
    });
    if (s.id !== S.sid || ch.id !== S.ch) return;
    const sc = $("scroller");
    const before = sc.scrollHeight;
    S.msgs = older.concat(S.msgs);
    S.limit = S.msgs.length;
    S.more = older.length === 50;
    renderMessages(true);
    sc.scrollTop += sc.scrollHeight - before;
  } finally {
    S.loading = false;
  }
}

function computeNewFrom() {
  S.newFrom = null;
  let n = S.unreadAtOpen;
  for (let i = S.msgs.length - 1; n && i >= 0; i--) {
    if (!isMe(S.msgs[i].author) && --n === 0) S.newFrom = S.msgs[i].id;
  }
}

function markRead() {
  const s = cur();
  const ch = chan();
  if (!s || !ch) return;
  quiet(rpc("mark_read", { space: s.id, channel: ch.id }));
  if (ch.unread) {
    ch.unread = ch.mentions = 0;
    renderRail();
    renderSidebar();
  }
}

function nearBottom() {
  const sc = $("scroller");
  return sc.scrollHeight - sc.scrollTop - sc.clientHeight < 80;
}

// ---------- selection ----------

function select(sid, ch) {
  S.sid = sid;
  const s = cur();
  if (sid && !s?.dm) store.set("sid", sid);
  const valid = (k) => k && s?.channels.some((x) => x.id === k);
  if (!valid(ch)) ch = store.get("ch." + sid);
  if (!valid(ch)) ch = s?.channels[0]?.id ?? null;
  if (ch !== S.ch || !S.msgs.length) {
    rows = new Map();
    put($("list"));
    Object.assign(S, { msgs: [], limit: 50, more: true, newFrom: undefined,
      editing: null, stick: true });
    S.unreadAtOpen = s?.channels.find((x) => x.id === ch)?.unread ?? 0;
    cancelReply();
  }
  S.ch = ch;
  if (ch) store.set("ch." + sid, ch);
  document.body.classList.remove("drawer");
  renderRail();
  renderSidebar();
  renderHeader();
  renderTyping();
  schedule("members", "msgs");
  if (!mobile()) setTimeout(() => $("input").focus({ preventScroll: true }));
}

function stepChannel(d) {
  const list = cur()?.channels ?? [];
  const i = list.findIndex((x) => x.id === S.ch);
  if (list.length) select(S.sid, list[(i + d + list.length) % list.length].id);
}

// ---------- rendering: rail, sidebar, header ----------

const unread = (s) => s.channels.some((x) => x.unread);
const mentions = (s) => s.channels.reduce((n, x) => n + x.mentions, 0);
const badge = (n) =>
  n ? h("span", { class: "badge" }, String(Math.min(n, 99))) : null;

function railItem({ label, active, unread: u, count, onclick, kids, cls }) {
  return h("div", {
    class: `server ${cls ?? ""}${active ? " active" : ""}${u ? " unread" : ""}`,
  },
  h("i", { class: "pill" }),
  h("button", {
    class: "icon-btn", "data-tip": label, onmouseenter: tip,
    onmouseleave: untip, onclick: () => { untip(); onclick(); },
  }, kids),
  badge(count));
}

function renderRail() {
  const d = dms();
  const inDm = !!cur()?.dm;
  const others = (s) =>
    s.channels.filter((x) => s.id !== S.sid || x.id !== S.ch);
  const dmCount = d.reduce((n, s) =>
    n + others(s).reduce((m, x) => m + x.unread, 0), 0);
  put($("rail"),
    d.length > 0 && railItem({
      label: "Direct Messages", active: inDm, unread: dmCount, count: dmCount,
      kids: icon("chat"), cls: "home", onclick: () => select(d[0].id),
    }),
    d.length > 0 && h("div", { class: "rail-sep" }),
    S.spaces.filter((s) => !s.dm).map((s) => {
      const rest = { ...s, channels: others(s) };
      return railItem({
        label: s.name, active: s.id === S.sid, unread: unread(rest),
        count: mentions(rest), kids: initials(s.name),
        onclick: () => select(s.id),
      });
    }),
    S.spaces.some((s) => !s.dm) && h("div", { class: "rail-sep" }),
    railItem({ label: "Add a Space", kids: icon("plus"), cls: "add",
      onclick: addSpace }),
  );
}

function tip(e) {
  const t = $("tooltip") ??
    document.body.appendChild(h("div", { id: "tooltip" }));
  const r = e.currentTarget.getBoundingClientRect();
  t.textContent = e.currentTarget.dataset.tip;
  t.style.left = `${r.right + 14}px`;
  t.style.top = `${r.top + r.height / 2 - 16}px`;
  t.classList.remove("hidden");
}
const untip = () => $("tooltip")?.classList.add("hidden");

function renderSidebar() {
  const s = cur();
  const hdr = $("server-header");
  const title = s?.dm ? "Direct Messages" : s?.name ?? "irch";
  put(hdr, h("span", {}, title), s && icon("chevron"));
  hdr.onclick = s ? () => (s.dm ? dmMenu : spaceMenu)(hdr) : null;
  put($("channels"), s?.dm ? dmList() : s ? channelList(s) : []);
  renderUserPanel();
}

function dmList() {
  return dms().map((d) => h("div", {
    class: `channel dm${d.id === S.sid ? " active" : ""}` +
      (unread(d) && d.id !== S.sid ? " unread" : ""),
    onclick: () => select(d.id),
  }, avatar(d.with ?? d.id, d.name, "xs"), h("span", { class: "name" }, d.name),
  badge(d.id !== S.sid && mentions(d))));
}

function channelList(s) {
  return [
    h("div", { class: "category" }, h("span", {}, "Text Channels"),
      s.admin && h("button",
        { title: "Create Channel", onclick: createChannel }, icon("plus"))),
    ...s.channels.map((ch) => {
      const active = ch.id === S.ch;
      return h("div", {
        class: `channel${active ? " active" : ""}` +
          (ch.unread && !active ? " unread" : ""),
        onclick: () => select(S.sid, ch.id),
      }, icon("hash"), h("span", { class: "name" }, ch.name),
      !active && badge(ch.mentions),
      s.admin && h("button", {
        class: "gear", title: "Edit Channel",
        onclick: (e) => { e.stopPropagation(); editChannel(ch); },
      }, icon("gear")));
    }),
  ];
}

function renderUserPanel() {
  const me = S.me;
  put($("user-panel"),
    h("div", { class: "who", title: "My Account", onclick: settings },
      avatar(me.id, me.name, "sm", !me.invisible),
      h("div", { class: "names" }, h("b", {}, me.name),
        h("small", {}, me.invisible ? "Invisible" : "Online"))),
    h("button", {
      class: "panel-btn", onclick: toggleInvisible,
      title: me.invisible ? "Go online" : "Go invisible",
    }, icon(me.invisible ? "eyeOff" : "eye")),
    h("button", { class: "panel-btn", title: "My Account", onclick: settings },
      icon("gear")));
}

function renderHeader() {
  const s = cur();
  const ch = chan();
  const label = s?.dm ? [h("b", { class: "at" }, "@"), h("span", {}, s.name)]
    : ch ? [icon("hash"), h("span", {}, ch.name)]
    : [h("span", {}, s ? "Syncing…" : "irch")];
  put($("chat-header").querySelector(".title"), label);
  const net = $("net");
  net.className = s?.peers ? "up" : "";
  put(net, s && [h("i", { class: "dot" }),
    `${s.peers} peer${s.peers === 1 ? "" : "s"}`]);
  const input = $("input");
  input.placeholder = s?.dm ? `Message @${s.name}`
    : ch ? `Message #${ch.name}` : "";
  input.disabled = !ch || s.locked;
  $("locked").classList.toggle("hidden", !s?.locked);
  for (const id of ["scroller", "composer", "members"]) {
    $(id).classList.toggle("hidden", !s || (id === "members" && s.dm));
  }
  $("members-btn").classList.toggle("hidden", !s || s.dm);
  $("empty").classList.toggle("hidden", !!s);
  put($("welcome"),
    s?.dm ? [avatar(s.with ?? s.id, s.name, "big"), h("h1", {}, s.name),
      h("p", {}, `The beginning of your messages with ${s.name}.`)]
    : ch ? [h("div", { class: "big" }, icon("hash")),
      h("h1", {}, `Welcome to #${ch.name}!`),
      h("p", {}, `This is the start of the #${ch.name} channel.`)]
    : s ? [h("h1", {}, s.name),
      h("p", {}, "Syncing… history arrives once a member is reachable.")]
    : [],
    s && h("div", { class: "e2e" }, icon("lock"),
      "End-to-end encrypted. Stored only on members' devices."));
}

function renderEmpty() {
  put($("empty"), h("div", {},
    h("h1", {}, "Welcome to irch"),
    h("p", {}, "Private, peer-to-peer chat. No servers, no accounts, no " +
      "phone number: devices talk directly over iroh, and everything is " +
      "end-to-end encrypted."),
    h("button", { class: "btn", onclick: createFlow }, "Create a space"), " ",
    h("button", { class: "btn grey", onclick: joinFlow },
      "Join with an invite"),
    h("p", { class: "note" }, "Already use irch elsewhere? ",
      h("span", { class: "link", onclick: linkFlow }, "Link this device"))));
}

// ---------- rendering: messages ----------

let rows = new Map(); // item key -> { el, sig }

function renderMessages(keepScroll) {
  const stick = !keepScroll && (S.stick || nearBottom());
  S.stick = false;
  const items = [];
  const admin = !!cur()?.admin;
  let prev = null;
  for (const m of S.msgs) {
    const day = fmtDay(m.ts);
    const newDay = !prev || fmtDay(prev.ts) !== day;
    if (newDay) items.push({ k: "d" + day, sig: "", make: () => divider(day) });
    const isNew = S.newFrom === m.id;
    if (isNew) {
      items.push({ k: "new", sig: "", make: () => divider("New", true) });
    }
    const cont = prev && !newDay && !isNew && !m.reply &&
      prev.author === m.author && m.ts - prev.ts < 7 * 60e3;
    const mentioned = mentionsMe(m);
    const editing = S.editing === m.id;
    const authorAdmin = memberOf(m.author)?.role === "Admin";
    const sig = JSON.stringify([m.text, m.edited, m.name, m.reactions, m.quote,
      cont, mentioned, editing, admin, authorAdmin]);
    items.push({ k: m.id, sig,
      make: () => msgRow(m, { cont, mentioned, editing, authorAdmin }) });
    prev = m;
  }
  reconcile($("list"), items);
  $("welcome").classList.toggle("hidden", S.more);
  if (stick) $("scroller").scrollTop = $("scroller").scrollHeight;
}

/** Keyed diff: unchanged rows are never touched; a new message costs one
 *  insertion, a page of history one per row. */
function reconcile(list, items) {
  const next = new Map();
  const els = items.map((it) => {
    let r = rows.get(it.k);
    if (!r || r.sig !== it.sig) r = { el: it.make(), sig: it.sig };
    next.set(it.k, r);
    return r.el;
  });
  for (const [k, r] of rows) if (next.get(k) !== r) r.el.remove();
  let at = list.firstChild;
  for (const el of els) {
    if (el === at) at = at.nextSibling;
    else list.insertBefore(el, at);
  }
  rows = next;
}

const divider = (text, isNew) =>
  h("div", { class: "divider" + (isNew ? " new" : "") }, h("span", {}, text));

function mentionsMe(m) {
  if (isMe(m.author)) return false;
  const tag = "@" + S.me.name.toLowerCase();
  return m.text.toLowerCase().includes(tag) ||
    (m.quote && isMe(m.quote.author));
}

function msgRow(m, { cont, mentioned, editing, authorAdmin }) {
  const row = h("div", {
    class: `msg${cont ? " cont" : ""}${mentioned ? " mentioned" : ""}` +
      (m.reply ? " has-reply" : ""),
    "data-id": m.id,
  });
  if (m.reply) row.append(replyRef(m));
  const at = new Date(m.ts).toLocaleString();
  if (cont) {
    row.append(h("span", { class: "time side-time", title: at },
      fmtTime(m.ts)));
  } else {
    const card = (e) => memberCard(m.author, e.currentTarget, "right");
    const av = avatar(m.author, m.name);
    av.addEventListener("click", card);
    row.append(av, h("div", { class: "head" },
      h("span", {
        class: "author" + (authorAdmin ? " admin" : ""), onclick: card,
      }, m.name),
      h("span", { class: "time", title: at }, fmtStamp(m.ts))));
  }
  if (editing) row.append(editBox(m));
  else if (m.text || m.edited) row.append(content(m.text, m.edited));
  if (m.files.length) row.append(attachments(m.files));
  if (m.reactions.length) row.append(reactionsBar(m));
  row.append(toolbar(m));
  return row;
}

function replyRef(m) {
  const q = m.quote;
  return h("div", { class: "reply-ref", onclick: () => jumpTo(m.reply) },
    q ? [avatar(q.author, q.name, "xs"), h("b", {}, "@" + q.name),
      h("span", {}, inl(inline(q.text.split("\n")[0])))]
      : h("span", {}, "Original message was deleted"));
}

function content(text, edited) {
  const div = h("div", { class: "content" });
  for (const b of markdown(text)) {
    if (b.t === "pre") div.append(h("pre", {}, h("code", {}, b.v)));
    else if (b.t === "quote") {
      div.append(h("div", { class: "quote" }, inl(b.c)));
    }
    else div.append(h("div", {}, b.c.length ? inl(b.c) : h("br")));
  }
  if (edited) {
    const last = div.lastChild;
    (last?.tagName === "DIV" ? last : div)
      .append(h("span", { class: "edited", title: "edited" }, "(edited)"));
  }
  return div;
}

function inl(nodes) {
  return nodes.map((n) => {
    switch (n.t) {
      case "text": return n.v;
      case "code": return h("code", {}, n.v);
      case "link": return h("span", {
        class: "link", title: "Copy link (irch never opens links itself)",
        onclick: () => copy(n.v, "Link copied"),
      }, n.v);
      case "mention": return h("span", { class: "mention" }, n.v);
      case "spoiler": {
        const s = h("span", { class: "spoiler", title: "Spoiler" }, inl(n.c));
        s.addEventListener("click", () => s.classList.add("shown"));
        return s;
      }
      default:
        return h({ b: "strong", i: "em", u: "u", s: "s" }[n.t], {}, inl(n.c));
    }
  });
}

// ---------- attachments ----------

const MIME = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg",
  gif: "image/gif", webp: "image/webp", avif: "image/avif", bmp: "image/bmp" };
const mime = (name) => MIME[name.split(".").pop().toLowerCase()];
const size = (n) => n < 1024 ? `${n} B` : n < 1 << 20 ? `${n >> 10} KB`
  : `${(n / (1 << 20)).toFixed(1)} MB`;
const images = new Map(); // hash -> Promise of a data: URL

function image(f) {
  if (!images.has(f.hash)) {
    images.set(f.hash, rpc("file", { space: S.sid, file: f })
      .then((b64) => `data:${mime(f.name)};base64,${b64}`)
      .catch((e) => (images.delete(f.hash), Promise.reject(e))));
  }
  return images.get(f.hash);
}

function attachments(files) {
  return h("div", { class: "attachments" }, files.map((f) => {
    if (!mime(f.name)) return fileChip(f);
    const img = h("img", { class: "attachment", alt: f.name, title: f.name });
    const box = h("div", { class: "image loading" }, img);
    image(f).then((src) => {
      const stick = nearBottom();
      img.onload = () => stick && ($("scroller").scrollTop = 1e9);
      img.src = src;
      box.classList.remove("loading");
      img.onclick = () => lightbox(src, f);
    }, () => box.replaceWith(fileChip(f)));
    return box;
  }));
}

const fileChip = (f) => h("div", { class: "file-chip" }, icon("file"),
  h("div", { class: "meta" }, h("b", {}, f.name), h("small", {}, size(f.size))),
  h("button", { title: android ? "Share" : "Save to Downloads",
    onclick: () => saveFile(f) }, icon("down")));

async function saveFile(f) {
  const at = await invoke("save", { space: S.sid, file: f })
    .catch((e) => toast(String(e), true));
  if (at && !android) toast(`Saved to ${at}`);
}

function lightbox(src, f) {
  modal((finish) => h("div", { class: "lightbox", onclick: () => finish(null) },
    h("img", { src, alt: f.name }),
    h("div", { class: "row" }, h("span", {}, f.name),
      h("button", { class: "btn grey", onclick: (e) => {
        e.stopPropagation();
        saveFile(f);
      } }, android ? "Share" : "Save"))));
}

async function addFiles(list) {
  for (const file of list) {
    if (file.size > 25 << 20) {
      toast(`${file.name} is over 25 MB`, true);
      continue;
    }
    const data = await new Promise((ok) => {
      const r = new FileReader();
      r.onload = () => ok(r.result.slice(r.result.indexOf(",") + 1));
      r.readAsDataURL(file);
    });
    const f = await call("attach", { name: file.name, data }).catch(() => null);
    if (f) S.files.push(f);
    renderFilesBar();
  }
}

function renderFilesBar() {
  const bar = $("files-bar");
  bar.classList.toggle("hidden", !S.files.length);
  put(bar, S.files.map((f, i) => h("span", { class: "pending" }, icon("clip"),
    f.name, h("button", { title: "Remove", onclick: () => {
      S.files.splice(i, 1);
      renderFilesBar();
    } }, icon("x")))));
}

function reactionsBar(m) {
  return h("div", { class: "reactions" },
    m.reactions.map(([emoji, users]) => {
      const mine = users.some(isMe);
      return h("button", {
        class: "reaction" + (mine ? " mine" : ""),
        title: users.map(nameOf).join(", "),
        onclick: () => react(m, emoji, !mine),
      }, emoji, h("b", {}, String(users.length)));
    }),
    h("button", {
      class: "reaction add", title: "Add Reaction",
      onclick: (e) => picker(e.currentTarget, (em) => react(m, em, true)),
    }, icon("smile")));
}

const tool = (ic, title, onclick, cls) =>
  h("button", { title, onclick, class: cls }, icon(ic));

function toolbar(m) {
  const mine = isMe(m.author);
  return h("div", { class: "toolbar" },
    tool("smile", "Add Reaction",
      (e) => picker(e.currentTarget, (em) => react(m, em, true))),
    tool("reply", "Reply", () => startReply(m)),
    mine && tool("pencil", "Edit", () => startEdit(m)),
    (mine || cur()?.admin) &&
      tool("trash", "Delete (hold Shift to skip confirmation)",
        (e) => del(m, e), "danger"));
}

function editBox(m) {
  const ta = h("textarea", { value: m.text, rows: 1, maxLength: 4000 });
  const save = async () => {
    const text = ta.value.trim();
    stopEdit();
    if (!text) return del(m);
    if (text !== m.text) await publish({ Edit: { target: m.id, text } });
  };
  ta.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.stopPropagation();
      stopEdit();
    } else if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      save();
    }
  });
  ta.addEventListener("input", () => autosize(ta));
  return h("div", { class: "edit-box" }, ta,
    h("small", {}, "escape to cancel • enter to save"));
}

function jumpTo(id) {
  const el = $("list").querySelector(`[data-id="${id}"]`);
  if (!el) return toast("That message is further back; scroll up to load it.");
  el.scrollIntoView({ block: "center", behavior: "smooth" });
  el.classList.add("target");
  setTimeout(() => el.classList.remove("target"), 1500);
}

function renderTyping() {
  const now = Date.now();
  const who = [...(S.typing.get(`${S.sid}|${S.ch}`)?.entries() ?? [])]
    .filter(([, until]) => until > now).map(([u]) => nameOf(u));
  const text = !who.length ? ""
    : who.length === 1 ? `${who[0]} is typing…`
    : who.length <= 3
      ? `${who.slice(0, -1).join(", ")} and ${who.at(-1)} are typing…`
    : "Several people are typing…";
  const dots = h("span", { class: "dots" }, h("i"), h("i"), h("i"));
  put($("typing"), text && [dots, text]);
}

// ---------- rendering: members ----------

function renderMembers() {
  const s = cur();
  if (!s || s.dm) return put($("members"));
  const ms = S.members;
  const groups = [
    ["Admins", ms.filter((m) => m.role === "Admin" && m.online)],
    ["Online", ms.filter((m) => m.role === "Member" && m.online)],
    ["Offline", ms.filter((m) => m.role !== "Banned" && !m.online)],
    ["Banned", s.admin ? ms.filter((m) => m.role === "Banned") : []],
  ];
  const shown = groups.filter(([, l]) => l.length);
  put($("members"), shown.flatMap(([title, list]) => [
    h("h3", {}, `${title} — ${list.length}`),
    ...list.map((m) => h("div", {
      class: `member ${m.online ? "on" : "off"} ${m.role.toLowerCase()}`,
      onclick: (e) => memberCard(m.id, e.currentTarget, "left"),
    }, avatar(m.id, m.name, "sm", m.online),
    h("span", { class: "name" }, m.name))),
  ]));
  // Names and admin colours in messages depend on members.
  if (S.msgs.length) renderMessages(true);
}

// ---------- popovers ----------

let closePop = null;

function popover(anchor, el, align = "below") {
  closePop?.();
  document.body.append(el);
  const r = anchor.getBoundingClientRect();
  const { offsetWidth: w, offsetHeight: hh } = el;
  let [x, y] = [r.left, r.bottom + 6];
  if (align === "right") [x, y] = [r.right + 8, r.top];
  if (align === "left") [x, y] = [r.left - w - 8, r.top];
  if (x + w > innerWidth - 8) x = Math.max(8, r.right - w);
  if (y + hh > innerHeight - 8) {
    y = Math.max(8, (align === "below" ? r.top - 6 : innerHeight - 8) - hh);
  }
  el.style.left = `${Math.max(8, x)}px`;
  el.style.top = `${Math.max(8, y)}px`;
  const outside = (e) => !el.contains(e.target) && close();
  const close = () => {
    el.remove();
    document.removeEventListener("mousedown", outside, true);
    if (closePop === close) closePop = null;
  };
  setTimeout(() => document.addEventListener("mousedown", outside, true));
  return (closePop = close);
}

const EMOJI = ("👍 👎 ❤️ 😂 😮 😢 😡 🎉 🔥 👀 🙏 ✅ ❌ 💯 🚀 🤔 😄 😍 🥳 😎 🙌 " +
  "👏 💀 🫡 😅 🤝 ⭐ 💡 🐛 ☕ 🍕 🌱").split(" ");

function picker(anchor, onPick) {
  let close;
  const pick = (em) => { close(); onPick(em); };
  const input = h("input",
    { placeholder: "Custom reaction (text or emoji)", maxLength: 16 });
  input.addEventListener("keydown",
    (e) => e.key === "Enter" && input.value.trim() && pick(input.value.trim()));
  close = popover(anchor, h("div", { class: "menu picker" }, input,
    h("div", { class: "grid" },
      EMOJI.map((em) => h("button", { onclick: () => pick(em) }, em)))));
  input.focus({ preventScroll: true });
}

function menu(anchor, items, align) {
  let close;
  const item = ([label, ic, fn, cls]) => h("button", {
    class: cls, onclick: () => { close(); fn(); },
  }, h("span", {}, label), icon(ic));
  close = popover(anchor, h("div", { class: "menu" },
    items.map((it) => it === "-" ? h("hr") : it && item(it))), align);
}

function spaceMenu(anchor) {
  const s = cur();
  menu(anchor, [
    ["Invite People", "userPlus", invitePeople, "accent"],
    s.admin && ["Create Channel", "plus", createChannel],
    s.admin && ["Rename Space", "pencil", renameSpace],
    s.admin && ["Reset Invite Links", "key", resetInvites],
    "-",
    ["Leave Space", "logout", leaveSpace, "danger"],
  ]);
}

const dmMenu = (anchor) =>
  menu(anchor, [["Close Conversation", "logout", leaveSpace, "danger"]]);

function memberCard(id, anchor, align) {
  const s = cur();
  const m = memberOf(id) ??
    { id, name: short(id), role: "Member", online: false };
  const ownerMe = s && isMe(s.owner);
  let close;
  const act = (label, fn, cls = "") => h("button",
    { class: "btn " + cls, onclick: () => { close(); fn(); } }, label);
  const role = (r) => () => publish({ Role: { user: id, role: r } });
  const ban = (on) => () => banFlow(m, on);
  const actions = [];
  if (!isMe(id)) {
    actions.push(act("Message", () => startDm(id)));
    const mention = () => insert($("input"), `@${m.name} `);
    if (!s?.dm) actions.push(act("Mention", mention, "grey"));
    if (s?.admin && id !== s.owner && (ownerMe || m.role !== "Admin")) {
      actions.push(m.role === "Banned" ? act("Unban", ban(false), "grey")
        : act("Ban", ban(true), "red"));
    }
    if (ownerMe && m.role !== "Banned") {
      actions.push(m.role === "Admin"
        ? act("Remove Admin", role("Member"), "grey")
        : act("Make Admin", role("Admin")));
    }
  }
  const banner = h("div", { class: "banner" });
  banner.style.background = color(id);
  close = popover(anchor, h("div", { class: "card" }, banner,
    h("div", { class: "body" },
      avatar(id, m.name, "", m.online),
      h("h2", {}, m.name),
      h("div", { class: "id", title: "Copy full ID",
        onclick: () => copy(id, "ID copied") }, short(id)),
      h("span", { class: "role" }, id === s?.owner ? "Owner" : m.role),
      actions.length && h("div", { class: "actions" }, actions))), align);
}

// ---------- modals ----------

function modal(build) {
  return new Promise((resolve) => {
    const d = $("modal");
    let done = false;
    const finish = (v) => {
      if (done) return;
      done = true;
      d.close();
      resolve(v);
    };
    d.replaceChildren(build(finish));
    d.onclose = () => finish(null);
    d.onclick = (e) => e.target === d && finish(null);
    d.showModal();
    d.querySelector("input:not([readonly]),textarea")
      ?.focus({ preventScroll: true });
  });
}

const foot = (...btns) =>
  h("div", { class: "m-foot" }, h("span", { class: "spacer" }), btns);
const cancel = (finish) =>
  h("button", { class: "btn link", onclick: () => finish(null) }, "Cancel");

function ask({ title, sub, label, value = "", ok = "Save", placeholder,
  maxLength = 64, danger }) {
  return modal((finish) => {
    const input = h("input", { type: "text", value, placeholder, maxLength });
    const submit = () => finish(input.value.trim());
    input.addEventListener("keydown", (e) => e.key === "Enter" && submit());
    return h("div", {},
      h("div", { class: "m-body" }, h("h2", {}, title),
        sub && h("p", { class: "sub" }, sub),
        label && h("label", {}, label), input),
      foot(cancel(finish),
        h("button",
          { class: "btn" + (danger ? " red" : ""), onclick: submit }, ok)));
  });
}

function confirmBox({ title, sub, ok }) {
  return modal((finish) => h("div", {},
    h("div", { class: "m-body" }, h("h2", {}, title),
      h("p", { class: "sub" }, sub)),
    foot(cancel(finish),
      h("button", { class: "btn red", onclick: () => finish(true) }, ok))));
}

/** A code to hand on: copy, share (Android), and a QR code to scan. */
async function showCode({ title, label, code, note }) {
  const qr = await invoke("qr", { text: code }).catch(() => null);
  modal((finish) => {
    const b = h("button", { class: "btn", onclick: async () => {
      await copy(code);
      b.textContent = "Copied";
      b.classList.add("green");
    } }, "Copy");
    const field = h("input",
      { class: "field mono", type: "text", value: code, readOnly: true });
    field.addEventListener("focus", () => field.select());
    const share = android && h("button", { class: "btn",
      onclick: () => quiet(invoke("share", { text: code })) }, "Share");
    if (share) b.classList.add("grey");
    const img = qr && h("img", { class: "qr", alt: "QR code",
      src: "data:image/svg+xml;base64," + btoa(qr) });
    return h("div", {},
      h("div", { class: "m-body" }, h("h2", {}, title), h("label", {}, label),
        h("div", { class: "row" }, field, b, share), img,
        h("p", { class: "note" }, note)),
      foot(h("button", { class: "btn grey", onclick: () => finish(null) },
        "Done")));
  });
}

async function addSpace() {
  untip();
  const v = await modal((finish) => h("div", { class: "m-body" },
    h("h2", {}, "Add a space"),
    h("p", { class: "sub" }, "Spaces live only on their members' devices. " +
      "There is nothing to sign up for."),
    h("button", { class: "choice", onclick: () => finish("create") },
      icon("plus"), "Create My Own"),
    h("button", { class: "choice", onclick: () => finish("join") },
      icon("link"), "Join with an Invite")));
  if (v === "create") createFlow();
  if (v === "join") joinFlow();
}

async function named() {
  if (S.me.name !== "anon") return true;
  const name = await ask({ title: "What should people call you?",
    sub: "Shown to the members of your spaces. You can change it later.",
    label: "Display name", ok: "Continue" });
  if (!name) return false;
  await call("set_name", { name });
  S.me = await rpc("me");
  renderUserPanel();
  return true;
}

async function createFlow() {
  if (!(await named())) return;
  const name = await ask({ title: "Customize your space",
    sub: "Give it a name. You can change it later.", label: "Space name",
    value: `${S.me.name}'s space`, ok: "Create" });
  if (!name) return;
  const id = await call("create", { name }).catch(() => null);
  if (id) await open(id);
}

async function joinFlow() {
  if (!(await named())) return;
  const code = await ask({ title: "Join a space",
    sub: "Paste an invite someone shared with you.", label: "Invite",
    placeholder: "irch:join:…", ok: "Join", maxLength: 8192 });
  if (!code) return;
  const id = await call("join", { code }).catch(() => null);
  if (id) {
    await open(id);
    toast("Joined. History syncs as soon as a member is reachable.");
  }
}

async function linkFlow() {
  const code = await ask({ title: "Link this device",
    sub: "On your other device, open My Account › Link a device and paste " +
      "its code here. This device becomes you, with all your spaces.",
    label: "Link code", placeholder: "irch:link:…", ok: "Link",
    maxLength: 8192 });
  if (!code) return;
  try {
    await invoke("link", { code });
  } catch (e) {
    return toast(String(e), true);
  }
  S.me = await rpc("me");
  renderUserPanel();
  toast("Linked. Your spaces arrive as your other devices are reached.");
  schedule("side");
}

async function open(id) {
  S.spaces = await rpc("spaces");
  select(id);
}

async function startDm(user) {
  const id = await call("dm", { space: S.sid, user }).catch(() => null);
  if (id) await open(id);
}

async function invitePeople() {
  const s = cur();
  const code = await call("invite", { space: s.id }).catch(() => null);
  if (code) showCode({ title: `Invite friends to ${s.name}`,
    label: "Send a space invite", code,
    note: "Anyone holding this invite can read and post until an admin " +
      "resets invite links. Share it privately, e.g. in person or over " +
      "Signal. It carries the space's key and a way to reach members' " +
      "devices; there is no server." });
}

async function resetInvites() {
  const ok = await confirmBox({ title: "Reset invite links",
    sub: "Everyone here gets a new key; every invite shared so far stops " +
      "working. Bans do this by themselves.", ok: "Reset" });
  if (ok) {
    quiet(call("rekey", { space: S.sid }).then(() => toast("New key sent")));
  }
}

async function banFlow(m, on) {
  if (on) {
    const ok = await confirmBox({ title: `Ban ${m.name}`,
      sub: "Their messages disappear for everyone, and the space moves to a " +
        "new key they never receive. Old invites stop working.", ok: "Ban" });
    if (!ok) return;
  }
  quiet(call("ban", { space: S.sid, user: m.id, on }));
}

function settings() {
  modal((finish) => {
    const name = h("input", { type: "text", value: S.me.name, maxLength: 64 });
    const invisible = h("input", { type: "checkbox", checked: S.me.invisible });
    const bg = android &&
      h("input", { type: "checkbox", checked: store.get("bg") === "1" });
    const storage = h("input",
      { type: "text", placeholder: "irch:host:…", maxLength: 8192 });
    const save = async () => {
      const n = name.value.trim();
      if (n && n !== S.me.name) await quiet(call("set_name", { name: n }));
      if (invisible.checked !== S.me.invisible) {
        await quiet(call("set_invisible", { on: invisible.checked }));
      }
      if (bg && bg.checked !== (store.get("bg") === "1")) {
        store.set("bg", bg.checked ? "1" : "0");
        await quiet(invoke("background", { on: bg.checked }));
      }
      const code = storage.value.trim();
      if (code) await quiet(call("add_storage", { code })
        .then(() => toast("Your spaces now also live on that storage node")));
      S.me = await rpc("me");
      renderUserPanel();
      schedule("members", "msgs");
      finish(true);
    };
    name.addEventListener("keydown", (e) => e.key === "Enter" && save());
    const sw = (input, title, small) => h("label", { class: "switch" },
      h("span", {}, title, h("small", {}, small)), input);
    return h("div", {},
      h("div", { class: "m-body" },
        h("h2", {}, "My Account"),
        h("label", {}, "Display name"), name,
        h("label", {}, "User ID (your public key)"),
        h("div", { class: "row" },
          h("input", { class: "field mono", type: "text", value: S.me.id,
            readOnly: true }),
          h("button", { class: "btn grey",
            onclick: () => copy(S.me.id, "ID copied") }, "Copy")),
        sw(invisible, "Invisible", "Send no presence or typing signals."),
        bg && sw(bg, "Stay connected in the background",
          "Receive messages and mentions while irch is closed. No server " +
          "can wake it, so it keeps running. Uses more battery."),
        h("label", {}, "Storage node"), storage,
        h("p", { class: "note" }, "Paste the code `irch --serve` prints " +
          "on an always-on machine: it keeps your spaces' ciphertext, " +
          "never the keys, so messages wait there while everyone is offline."),
        h("button", {
          class: "choice", onclick: () => { finish(null); linkDevice(); },
        }, icon("phone"), "Link a device"),
        h("p", { class: "note" }, "Your identity is a key on your devices. " +
          "Messages are end-to-end encrypted; read positions stay here.")),
      foot(cancel(finish),
        h("button", { class: "btn", onclick: save }, "Save")));
  });
}

async function linkDevice() {
  const code = await call("link_code").catch(() => null);
  if (code) showCode({ title: "Link a device", label: "Your link code", code,
    note: "Open irch on the new device, choose Link this device, and paste " +
      "or scan this. It becomes you: keep this code secret, and never " +
      "post it anywhere." });
}

function quickSwitch() {
  const all = S.spaces.flatMap((s) => s.channels.map((ch) => ({ s, ch })));
  modal((finish) => {
    const input = h("input",
      { type: "text", placeholder: "Where would you like to go?" });
    const list = h("div", { class: "qs-list" });
    let sel = 0;
    let hits = [];
    const label = ({ s, ch }) => s.dm ? `@${s.name}` : ch.name;
    const draw = () => {
      const q = input.value.toLowerCase();
      const hit = (x) => `${label(x)} ${x.s.name}`.toLowerCase().includes(q);
      hits = all.filter(hit).slice(0, 50);
      sel = Math.min(sel, Math.max(0, hits.length - 1));
      list.replaceChildren(...hits.map((x, i) => h("button", {
        class: i === sel ? "sel" : "", onclick: () => finish(x),
      }, icon(x.s.dm ? "chat" : "hash"), h("span", {}, label(x)),
      h("small", {}, x.s.dm ? "" : x.s.name))));
    };
    input.addEventListener("input", () => { sel = 0; draw(); });
    input.addEventListener("keydown", (e) => {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        const d = e.key === "ArrowDown" ? 1 : -1;
        sel = (sel + d + hits.length) % Math.max(1, hits.length);
        draw();
      } else if (e.key === "Enter" && hits[sel]) finish(hits[sel]);
    });
    draw();
    return h("div", { class: "m-body" },
      h("h2", {}, "Quick Switcher"), input, list);
  }).then((x) => x && select(x.s.id, x.ch.id));
}

// ---------- actions ----------

const publish = (op) => quiet(call("publish", { space: cur().id, op }));
const react = (m, emoji, on) => publish({ React: { target: m.id, emoji, on } });

async function del(m, e) {
  if (!e?.shiftKey) {
    const ok = await confirmBox({ title: "Delete Message",
      sub: "Are you sure you want to delete this message? Tip: hold Shift " +
        "when clicking delete to skip this.", ok: "Delete" });
    if (!ok) return;
  }
  publish({ Edit: { target: m.id, text: null } });
}

function startReply(m) {
  S.reply = m;
  renderReplyBar();
  $("input").focus({ preventScroll: true });
}

function cancelReply() {
  S.reply = null;
  renderReplyBar();
}

function renderReplyBar() {
  const bar = $("reply-bar");
  bar.classList.toggle("hidden", !S.reply);
  put(bar, S.reply && [h("span", {}, "Replying to ", h("b", {}, S.reply.name)),
    h("button", { title: "Cancel Reply", onclick: cancelReply }, icon("x"))]);
}

function startEdit(m) {
  S.editing = m.id;
  renderMessages(true);
  const ta = $("list").querySelector(".edit-box textarea");
  if (ta) {
    autosize(ta);
    ta.focus({ preventScroll: true });
    ta.setSelectionRange(ta.value.length, ta.value.length);
  }
}

function stopEdit() {
  S.editing = null;
  renderMessages(true);
  $("input").focus({ preventScroll: true });
}

async function createChannel() {
  const name = await ask({ title: "Create Channel", label: "Channel name",
    placeholder: "new-channel", ok: "Create Channel" });
  if (!name) return;
  const id = randomId(8);
  await publish({ Channel: { id, name: slug(name) } });
  S.spaces = await rpc("spaces");
  select(S.sid, id);
}

async function editChannel(ch) {
  const name = await ask({ title: "Edit Channel", label: "Channel name",
    sub: "Leave the name empty to delete the channel.", value: ch.name });
  if (name === null) return;
  if (name !== "") return publish({ Channel: { id: ch.id, name: slug(name) } });
  const ok = await confirmBox({ title: "Delete Channel",
    sub: `Delete #${ch.name} for everyone?`, ok: "Delete Channel" });
  if (ok) publish({ Channel: { id: ch.id, name: null } });
}

const slug = (s) => s.trim().toLowerCase().replace(/\s+/g, "-");

async function renameSpace() {
  const name = await ask({ title: "Rename Space", label: "Space name",
    value: cur().name });
  if (name) publish({ Rename: { name } });
}

async function leaveSpace() {
  const s = cur();
  const ok = await confirmBox(s.dm ? { title: `Close conversation`,
    sub: `Your messages with ${s.name} are deleted from your devices.`,
    ok: "Close" } : { title: `Leave '${s.name}'`,
    sub: `Leave ${s.name}? Its history is deleted from your devices, and ` +
      "you need a new invite to come back.", ok: "Leave Space" });
  if (!ok) return;
  await quiet(call("leave", { space: s.id }));
  S.sid = null;
  schedule("side");
}

async function toggleInvisible() {
  await quiet(call("set_invisible", { on: !S.me.invisible }));
  S.me = await rpc("me");
  renderUserPanel();
  schedule("members");
}

function autosize(t) {
  t.style.height = "auto";
  t.style.height = `${t.scrollHeight}px`;
}

function insert(t, text) {
  const { selectionStart: a, selectionEnd: b, value } = t;
  t.value = value.slice(0, a) + text + value.slice(b);
  t.setSelectionRange(a + text.length, a + text.length);
  t.focus({ preventScroll: true });
  autosize(t);
}

async function send() {
  const input = $("input");
  const text = input.value.trim();
  const s = cur();
  const ch = chan();
  const files = S.files;
  if ((!text && !files.length) || !s || !ch) return;
  const reply = S.reply?.id ?? null;
  input.value = "";
  S.files = [];
  renderFilesBar();
  autosize(input);
  cancelReply();
  S.lastTyping = 0;
  S.stick = true;
  try {
    const op = { Post: { channel: ch.id, text, reply, files } };
    await call("publish", { space: s.id, op });
  } catch {
    input.value = text;
    S.files = files;
    renderFilesBar();
  }
}

// ---------- wiring ----------

function wire() {
  $("menu-btn").append(icon("menu"));
  $("menu-btn").onclick = () => document.body.classList.toggle("drawer");
  $("members-btn").append(icon("users"));
  const showMembers = (on) => {
    $("members").classList.toggle("hidden-panel", !on);
    $("members-btn").classList.toggle("on", on);
  };
  showMembers(store.get("members") ? store.get("members") === "1" : !mobile());
  $("members-btn").onclick = () => {
    const on = $("members").classList.contains("hidden-panel");
    showMembers(on);
    store.set("members", on ? "1" : "0");
  };
  $("attach-btn").append(icon("clip"));
  $("attach-btn").onclick = () => $("picker").click();
  $("picker").onchange = (e) => {
    addFiles([...e.target.files]);
    e.target.value = "";
  };
  addEventListener("dragover", (e) => e.preventDefault());
  addEventListener("drop", (e) => {
    e.preventDefault();
    if (chan() && !cur()?.locked) addFiles([...e.dataTransfer.files]);
  });
  addEventListener("paste", (e) => {
    const files = [...(e.clipboardData?.files ?? [])];
    if (files.length && chan()) addFiles(files);
  });
  $("emoji-btn").append(icon("smile"));
  $("emoji-btn").onclick = (e) =>
    picker(e.currentTarget, (em) => insert($("input"), em));
  $("chat").addEventListener("click", (e) => {
    if (e.target === $("chat")) document.body.classList.remove("drawer");
  });
  // Android's Back (MainActivity.kt): close the topmost thing; with nothing
  // left to close, returning false sends irch to the background.
  window.irchBack = () => {
    const body = document.body.classList;
    if ($("modal").open) $("modal").close();
    else if (closePop) closePop();
    else if (S.editing) stopEdit();
    else if (S.reply) cancelReply();
    else if (mobile() && !$("members").classList.contains("hidden-panel")) {
      showMembers(false);
    }
    else if (mobile() && !body.contains("drawer")) body.add("drawer");
    else return false;
    return true;
  };

  const input = $("input");
  input.addEventListener("input", () => {
    autosize(input);
    const s = cur();
    const ch = chan();
    if (input.value.trim() && s && ch && Date.now() - S.lastTyping > 4000) {
      S.lastTyping = Date.now();
      quiet(rpc("typing", { space: s.id, channel: ch.id }));
    }
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      send();
    } else if (e.key === "ArrowUp" && !input.value) {
      const mine = S.msgs.findLast((m) => isMe(m.author));
      if (mine) {
        e.preventDefault();
        startEdit(mine);
      }
    }
  });

  const sc = $("scroller");
  sc.addEventListener("scroll", () => {
    if (sc.scrollTop < 300) loadOlder();
    if (nearBottom() && chan()?.unread) markRead();
  }, { passive: true });
  addEventListener("focus", () => nearBottom() && markRead());

  document.addEventListener("keydown", (e) => {
    const typing = /^(INPUT|TEXTAREA)$/.test(e.target.tagName);
    if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      quickSwitch();
    } else if (e.altKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
      e.preventDefault();
      stepChannel(e.key === "ArrowDown" ? 1 : -1);
    } else if (e.key === "Escape" && !$("modal").open) {
      if (closePop) closePop();
      else if (S.reply) cancelReply();
      else {
        sc.scrollTop = sc.scrollHeight;
        markRead();
      }
    } else if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey &&
      !typing && !$("modal").open && !input.disabled) {
      input.focus({ preventScroll: true }); // like Discord: type anywhere
    }
  });
}

async function start() {
  if (!invoke) {
    $("boot").textContent = "This page runs inside the irch app.";
    return;
  }
  try {
    S.me = await rpc("me");
  } catch (e) {
    put($("boot"), h("div", {}, h("p", {}, "irch could not start"),
      h("div", { class: "err" }, String(e))));
    return;
  }
  wire();
  renderEmpty();
  if (android && store.get("bg") === "1") {
    quiet(invoke("background", { on: true }));
  }
  // Member lists change rarely but can be large: at most every 2 s.
  let membersDue = 0;
  const members = () => {
    const wait = membersDue - Date.now();
    if (wait <= 0) {
      membersDue = Date.now() + 2000;
      schedule("members");
    } else if (!members.t) {
      members.t = setTimeout(() => ((members.t = 0), members()), wait);
    }
  };
  await T.event.listen("irch", ({ payload: e }) => {
    if (e.t === "changed") {
      schedule("side");
      if (!e.space || e.space === S.sid) {
        schedule("msgs");
        members();
      }
    } else if (e.t === "presence" && e.space === S.sid) {
      members();
    } else if (e.t === "typing" && !isMe(e.user)) {
      const k = `${e.space}|${e.channel}`;
      if (!S.typing.has(k)) S.typing.set(k, new Map());
      S.typing.get(k).set(e.user, Date.now() + 6000);
      renderTyping();
      setTimeout(renderTyping, 6100);
    }
  });
  await loadSidebar();
  if (!S.spaces.length) renderHeader();
  $("boot").remove();
}

start();
