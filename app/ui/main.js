// irch: a Discord-style page over irch_core, through one Tauri command.
// The core is the only source of truth: its events schedule a refresh of
// what they touch, at most once a frame. Message rows are keyed and reused,
// the window is paged as you scroll and trimmed back at the bottom.
// User content only ever becomes text nodes; links are copied, not opened.

import { inline, markdown } from "./markdown.js";
import * as av from "./call.js";

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
  ring: "M6 3h3l2 5-2 1.5a11 11 0 0 0 5.5 5.5L16 13l5 2v3a2 2 0 0 1-2 " +
    "2C10.7 20 4 13.3 4 5a2 2 0 0 1 2-2",
  hangup: "M3 14.5c5-5 13-5 18 0l-2.5 2.5-3.5-2v-2.5a12 12 0 0 0-6 0V15l-3.5 " +
    "2z",
  mic: "M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3M5 11a7 7 0 0 0 " +
    "14 0M12 18v3",
  micOff: "M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3M5 11a7 7 0 0 " +
    "0 14 0M12 18v3M3 3l18 18",
  cam: "M3 7h12v10H3zM15 10.5 21 7v10l-6-3.5",
  camOff: "M3 7h12v10H3zM15 10.5 21 7v10l-6-3.5M3 3l18 18",
  screen: "M3 4h18v12H3zM8 20h8M12 16v4",
  speaker: "M4 9h4l5-4v14l-5-4H4zM16 9a4 4 0 0 1 0 6M19 6a8 8 0 0 1 0 12",
  thread: "M5 4v9a4 4 0 0 0 4 4h10M15 13l4 4-4 4",
  note: "M6 3h12v18l-6-4-6 4zM9 8h6M9 11h4",
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

const COLORS = ["#b44dff", "#ff2bd6", "#00b8d9", "#7c4dff",
  "#ff4d8d", "#00c983", "#ff8a3d", "#d24dff"];
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
  thread: null, // the thread open, or last open: { id, root, parent, msg… }
};

const cur = () => S.spaces.find((s) => s.id === S.sid);
const inThread = () => !!S.ch && S.thread?.id === S.ch;
// Where we read and write: a channel, or a thread (a channel of its own).
const chan = () => inThread() ? S.thread
  : cur()?.channels.find((c) => c.id === S.ch);
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
  if (!cur().channels.some((c) => c.id === S.thread?.parent)) S.thread = null;
  adopt();
  if (!chan() && cur().channels.length) return select(S.sid);
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
  if (S.jump && msgs.some((m) => m.id === S.jump)) jumpTo(S.jump);
  S.jump = null;
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
  const valid = (k) =>
    k && (s?.channels.some((x) => x.id === k) || k === S.thread?.id);
  if (!valid(ch)) ch = store.get("ch." + sid);
  if (!valid(ch)) ch = s?.channels[0]?.id ?? null;
  if (ch !== S.ch || !S.msgs.length) {
    rows = new Map();
    put($("list"));
    Object.assign(S, { msgs: [], limit: 50, more: true, newFrom: undefined,
      editing: null, stick: true });
    S.unreadAtOpen = s?.channels.find((x) => x.id === ch)?.unread ??
      (ch === S.thread?.id ? S.thread.unread : 0);
    cancelReply();
  }
  S.ch = ch;
  quiet(invoke("viewing", { space: sid ?? null, room: ch ?? null }));
  if (ch) store.set("ch." + sid, inThread() ? S.thread.parent : ch);
  document.body.classList.remove("drawer");
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
const mentions = (s) => s.channels.reduce((n, x) =>
  n + x.mentions + x.threads.reduce((m, t) => m + t.mentions, 0), 0);
const badge = (n) =>
  n ? h("span", { class: "badge" }, String(Math.min(n, 99))) : null;

/** One list, as weechat's: each space and its channels and threads,
 *  then direct messages. A space folds by its name. */
function renderSidebar() {
  put($("server-header"), h("span", { class: "brand" }, "irch"),
    h("button", { title: "Add a space (create or join)", onclick: addSpace },
      icon("plus")));
  const d = dms();
  put($("channels"), S.spaces.filter((s) => !s.dm).map(spaceTree),
    d.length > 0 && [h("div", { class: "space dms" },
      h("span", { class: "name" }, "Direct messages")), dmList()]);
  renderUserPanel();
}

function spaceTree(s) {
  const open = store.get("fold." + s.id) !== "1";
  const fold = () => {
    store.set("fold." + s.id, open ? "1" : "0");
    renderSidebar();
  };
  const menu = (e) => {
    e.stopPropagation();
    if (s.id !== S.sid) select(s.id);
    spaceMenu(e.currentTarget);
  };
  return [h("div", {
    class: "space" + (s.id === S.sid ? " here" : "") + (open ? " open" : "") +
      (!open && unread(s) ? " unread" : ""),
    onclick: fold,
  }, icon("chevron"), h("span", { class: "name" }, s.name),
  !open && badge(mentions(s)),
  h("button", { class: "gear", title: "Space menu", onclick: menu },
    icon("gear"))),
  open && channelList(s)];
}

function dmList() {
  return dms().map((d) => h("div", {
    class: `channel dm${d.id === S.sid ? " active" : ""}` +
      (unread(d) && d.id !== S.sid ? " unread" : ""),
    onclick: () => select(d.id),
  }, avatar(d.with ?? d.id, d.name, "xs"), h("span", { class: "name" }, d.name),
  d.channels.some((c) => c.call.length) && icon("speaker"),
  badge(d.id !== S.sid && mentions(d)),
  h("button", { class: "gear", title: `Call ${d.name}`, onclick: (e) => {
    e.stopPropagation();
    callIn(d.id);
  } }, icon("ring")),
  h("button", { class: "gear", title: "Close conversation", onclick: (e) => {
    e.stopPropagation();
    select(d.id);
    leaveSpace();
  } }, icon("x"))));
}

function channelList(s) {
  return [
    ...s.channels.map((ch) => {
      const active = s.id === S.sid && ch.id === S.ch;
      return h("div", {
        class: `channel${active ? " active" : ""}` +
          (ch.unread && !active ? " unread" : ""),
        onclick: () => select(s.id, ch.id),
      }, icon("hash"), h("span", { class: "name" }, ch.name),
      !active && badge(ch.mentions),
      h("button", {
        class: "gear", title: "New room (a thread here)",
        onclick: (e) => { e.stopPropagation(); newRoom(s, ch); },
      }, icon("plus")),
      s.admin && h("button", {
        class: "gear", title: "Edit Channel",
        onclick: (e) => { e.stopPropagation(); editChannel(ch); },
      }, icon("gear")), ch.call.length > 0 && icon("speaker"));
    }).flatMap((row, i) => [row, callers(s.channels[i].call),
      ...s.channels[i].threads.map((t) => threadRow(s, t))]),
  ];
}

const threadRow = (s, t) => h("div", {
  class: `channel thread${t.id === S.ch ? " active" : ""}` +
    (t.unread && t.id !== S.ch ? " unread" : ""),
  onclick: () => {
    S.sid = s.id;
    openThread(t.root);
  },
}, icon("thread"), h("span", { class: "name" }, t.name),
t.id !== S.ch && badge(t.mentions),
(isMe(t.by) || s.admin) && h("button", {
  class: "gear", title: "Rename",
  onclick: (e) => { e.stopPropagation(); renameRoom(s, t); },
}, icon("pencil")));

/** A newer irch: offered once at launch (`quietly`), or on request. */
async function updates(quietly) {
  let v;
  try {
    v = await invoke("update", { install: false });
  } catch (e) {
    return quietly || toast(`Could not check: ${e}`, true);
  }
  if (!v) return quietly || toast("irch is up to date");
  const ok = await modal((finish) => h("div", {},
    h("div", { class: "m-body" }, h("h2", {}, `irch ${v} is out`),
      h("p", { class: "sub" }, android
        ? "It downloads, then Android asks to install it over this one."
        : "It downloads, checks its signature, and irch restarts (or, if "
          + "irch came another way, its release page opens).")),
    foot(cancel(finish),
      h("button", { class: "btn", onclick: () => finish(true) }, "Update"))));
  if (!ok) return;
  toast(`Downloading irch ${v}…`);
  await invoke("update", { install: true })
    .then((said) => said && toast(said))
    .catch((e) => toast(`Update failed: ${e}`, true));
}

/** irch://open/SPACE/CHANNEL/MESSAGE, from a note: there, if we can;
 *  irch://call/SPACE/CHANNEL, a call answered from its notification, and
 *  irch://ring/…, the notification opened (answer or decline here). */
async function openUrl(url) {
  const c = /^irch:\/\/(call|ring)\/([0-9a-f]{64})\/([0-9a-f]{16})$/
    .exec(url);
  if (c) return c[1] === "call" ? answer(c[2], c[3]) : incoming(c[2], c[3]);
  if (!String(url).startsWith("irch://open/")) return;
  const [sid, ch, msg] = url.replace(/^irch:\/\/open\//, "").split("/");
  const s = S.spaces.find((x) => x.id === sid);
  if (!s || !/^[0-9a-f]{16}$/.test(ch) || !/^[0-9a-f]{64}$/.test(msg)) {
    return toast("That link is to a space you are not in", true);
  }
  S.jump = msg;
  if (s.channels.some((c) => c.id === ch)) return select(sid, ch);
  S.sid = sid;
  await openThread(ch);
}

/** A message's thread: a channel of its own, there once anyone writes. */
async function openThread(root) {
  const r = await rpc("thread", { space: S.sid, channel: root.slice(0, 16) });
  if (!r) return toast("Threads hang off messages in channels", true);
  const [t, msg] = r;
  S.thread = { unread: 0, mentions: 0, ...t, msg };
  adopt();
  select(S.sid, t.id);
}

/** The open thread and its row in the sidebar: one object, read as one. */
function adopt() {
  for (const c of cur()?.channels ?? []) {
    c.threads = c.threads.map((t) =>
      t.id === S.thread?.id ? Object.assign(S.thread, t) : t);
  }
}

/** Who is in a channel's call, under it. */
const callers = (ids) => ids.length > 0 && h("div", { class: "callers" },
  ids.map((id) => h("div", { class: "caller" },
    avatar(id, isMe(id) ? S.me.name : nameOf(id), "xs"),
    isMe(id) ? S.me.name : nameOf(id),
    isMe(id) && !av.call.mic && icon("micOff"))));

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
  const parent = inThread() &&
    s.channels.find((c) => c.id === S.thread.parent);
  const up = () => select(S.sid, S.thread.parent);
  const label = parent ? [icon("hash"),
    h("span", { class: "crumb", onclick: up }, parent.name),
    icon("thread"), h("span", {}, ch.name)]
    : s?.dm ? [h("b", { class: "at" }, "@"), h("span", {}, s.name)]
    : ch ? [icon("hash"), h("span", {}, ch.name)]
    : [h("span", {}, s ? "Syncing…" : "irch")];
  put($("chat-header").querySelector(".title"), label);
  const net = $("net");
  net.className = s?.peers ? "up" : "";
  put(net, s && [h("i", { class: "dot" }),
    `${s.peers} peer${s.peers === 1 ? "" : "s"}`]);
  const input = $("input");
  input.placeholder = parent ? `Reply in ${ch.name}`
    : s?.dm ? `Message @${s.name}`
    : ch ? `Message #${ch.name}` : "";
  input.disabled = !ch || s.locked;
  $("locked").classList.toggle("hidden", !s?.locked);
  for (const id of ["scroller", "composer", "members"]) {
    $(id).classList.toggle("hidden", !s || (id === "members" && s.dm));
  }
  $("members-btn").classList.toggle("hidden", !s || s.dm);
  $("empty").classList.toggle("hidden", !!s);
  renderCall();
  put($("welcome"),
    parent ? [h("div", { class: "big" }, icon("thread")),
      h("h1", {}, ch.name),
      h("p", {}, `A thread in #${parent.name}, on this message:`),
      ch.msg && h("div", { class: "starter" }, msgRow(ch.msg, {}))]
    : s?.dm ? [avatar(s.with ?? s.id, s.name, "big"), h("h1", {}, s.name),
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

// ---------- calls ----------

const callOf = (sid, cid) =>
  S.spaces.find((x) => x.id === sid)?.channels.find((c) => c.id === cid);
const here = () =>
  av.call.on?.space === S.sid && av.call.on?.channel === S.ch;

async function ring() {
  if (here()) return av.leave(rpc);
  if (!av.supported()) return toast("Calls need a newer system web view", 1);
  try {
    await av.join(S.sid, S.ch, rpc);
  } catch (e) {
    toast(String(e?.message ?? e), true);
    await av.leave(rpc);
  }
}

// ---------- alerts ----------

// Desktops sound alerts here (phones: the system's sound, the ringtone).
let audio;
function tone(notes, gain) {
  audio ??= new AudioContext();
  quiet(audio.resume());
  const t0 = audio.currentTime;
  for (const [f, at, len] of notes) {
    const o = audio.createOscillator();
    const g = audio.createGain();
    o.frequency.value = f;
    g.gain.setValueAtTime(gain, t0 + at);
    g.gain.exponentialRampToValueAtTime(1e-4, t0 + at + len);
    o.connect(g).connect(audio.destination);
    o.start(t0 + at);
    o.stop(t0 + at + len);
  }
}
const chime = () => tone([[880, 0, 0.12], [1320, 0.13, 0.25]], 0.15);
// A phone's ring: 440 and 480 Hz together, two seconds in every six.
const ringing = () => tone([[440, 0, 2], [480, 0, 2]], 0.07);

let calling = null;

/** Someone calls (a DM): answer or decline; after 45 s, missed. */
function incoming(sid, ch, who) {
  if (calling) return;
  const ids = callOf(sid, ch)?.call ?? [];
  who ??= ids.length ? nameOf(ids[0]) : "Someone";
  const rings = android ? 0 : setInterval(ringing, 6000);
  if (!android) ringing();
  const late = setTimeout(() => $("modal").close(), 45e3);
  calling = modal((finish) => h("div", {},
    h("div", { class: "m-body" }, h("h2", {}, who),
      h("p", { class: "sub" }, "is calling you")),
    foot(h("button", { class: "btn link", onclick: () => finish(false) },
      "Decline"),
    h("button", { class: "btn green", onclick: () => finish(true) },
      "Answer"))));
  calling.then((yes) => {
    clearInterval(rings);
    clearTimeout(late);
    calling = null;
    quiet(invoke("hush"));
    if (yes) answer(sid, ch);
  });
}

/** Call someone: their DM's call, which rings them. */
async function callIn(sid) {
  select(sid);
  if (!here()) await ring();
}

async function answer(sid, ch) {
  quiet(invoke("hush"));
  select(sid, ch);
  if (!here()) await ring();
}

async function camera(kind) {
  try {
    await av.video(av.call[kind] ? null : kind);
  } catch (e) {
    toast(String(e?.message ?? e), true);
  }
}

const mute = () => {
  av.call.mic = !av.call.mic;
  renderCall();
};

const control = (ic, title, onclick, cls = "") =>
  h("button", { title, onclick, class: cls }, icon(ic));

function tile(id) {
  const me = isMe(id);
  const p = av.call.peers.get(id);
  const live = me ? av.call.self : Date.now() - (p?.video ?? 0) < 3000;
  const level = me ? av.call.level
    : Date.now() - (p?.spoke ?? 0) < 300 ? p.level : 0;
  const name = me ? S.me.name : nameOf(id);
  const v = live && (me ? av.call.self : p.canvas);
  // Each tile is its video's shape: a phone's upright, a laptop's wide.
  const [w, ht] = me ? [v?.videoWidth, v?.videoHeight] : [v?.width, v?.height];
  return h("div", {
    class: "tile" + (level > 0.02 ? " speaking" : "") +
      (me && av.call.cam ? " mirror" : ""),
    style: w && ht ? `--ar: ${w} / ${ht}` : null,
  }, v || avatar(id, name, "big"),
  h("span", { class: "who" }, name, me && !av.call.mic && icon("micOff")));
}

function renderCall() {
  const on = av.call.on;
  const ch = on && callOf(on.space, on.channel);
  if (ch) av.keep(ch.call);
  const btn = $("call-btn");
  btn.classList.toggle("hidden", !chan() || inThread());
  btn.classList.toggle("live", here());
  btn.title = here() ? "Leave call" : "Join call";
  put(btn, icon(here() ? "hangup" : "ring"));
  $("call").classList.toggle("hidden", !here());
  const c = av.call;
  if (here()) {
    put($("call"), h("div", { class: "tiles" }),
      h("div", { class: "controls" },
        control(c.mic ? "mic" : "micOff", c.mic ? "Mute" : "Unmute", mute,
          c.mic ? "" : "off"),
        control(c.cam ? "cam" : "camOff", "Camera", () => camera("cam"),
          c.cam ? "on" : ""),
        av.canShare() && control("screen", "Share your screen",
          () => camera("screen"), c.screen ? "on" : ""),
        control("hangup", "Leave call", () => av.leave(rpc), "hangup")));
    renderTiles();
  }
  const s = on && S.spaces.find((x) => x.id === on.space);
  put($("voice"), on && h("div", { class: "voice" },
    h("div", { class: "names", onclick: () => select(on.space, on.channel) },
      h("b", {}, "Voice connected"),
      h("small", {}, s?.dm ? s.name : `#${ch?.name ?? "…"} · ${s?.name}`)),
    control(c.mic ? "mic" : "micOff", c.mic ? "Mute" : "Unmute", mute,
      "panel-btn" + (c.mic ? "" : " off")),
    control("hangup", "Leave call", () => av.leave(rpc), "panel-btn off")));
}

/** Just the tiles, often: controls stay put under the pointer. */
function renderTiles() {
  const on = av.call.on;
  const tiles = here() && $("call").querySelector(".tiles");
  if (tiles) put(tiles, (callOf(on.space, on.channel)?.call ?? []).map(tile));
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
      m.thread, cont, mentioned, editing, admin, authorAdmin]);
    items.push({ k: m.id, sig,
      make: () => msgRow(m, { cont, mentioned, editing, authorAdmin }) });
    prev = m;
  }
  reconcile($("list"), items);
  if (S.sel) $("list").querySelector(`[data-id="${S.sel}"]`)
    ?.classList.add("sel");
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
    const nick = h("span", {
      class: "author" + (authorAdmin ? " admin" : ""), onclick: card,
      title: authorAdmin ? `${m.name} (admin)` : m.name,
    }, m.name);
    if (!authorAdmin) nick.style.color = color(m.author);
    row.append(h("div", { class: "head" },
      h("span", { class: "time", title: at }, fmtTime(m.ts)), nick));
  }
  if (editing) row.append(editBox(m));
  else if (m.text || m.edited) row.append(content(m.text, m.edited));
  if (m.files.length) row.append(attachments(m.files));
  if (m.reactions.length) row.append(reactionsBar(m));
  if (m.thread && !inThread()) {
    row.append(h("div", { class: "thread-link", onclick: () =>
      openThread(m.id) }, icon("thread"),
    `${m.thread} ${m.thread === 1 ? "reply" : "replies"}`));
  }
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

// Block and inline tags a message may make: anything else is a span.
const TAGS = new Set(("p h1 h2 h3 h4 h5 h6 ul ol li blockquote strong em s " +
  "table thead tbody tr th td").split(" "));
const LANG = { rs: "rust", py: "python", js: "javascript", ts: "typescript",
  sh: "bash", shell: "bash", zsh: "bash", yml: "yaml", tex: "latex",
  "c++": "cpp", html: "markup", xml: "markup", svg: "markup", kt: "kotlin",
  rb: "ruby", jl: "julia", dockerfile: "docker", make: "makefile" };

function content(text, edited) {
  const div = h("div", { class: "content" });
  render(markdown(text), div);
  if (edited) {
    const last = div.lastElementChild;
    (last?.tagName === "P" ? last : div)
      .append(h("span", { class: "edited", title: "edited" }, "(edited)"));
  }
  return div;
}

/** markdown-it's tokens as elements; text only ever as text nodes. */
function render(tokens, root) {
  const stack = [root];
  const add = (...n) => stack.at(-1).append(...n);
  for (const t of tokens) {
    if (t.type === "inline") render(t.children ?? [], stack.at(-1));
    else if (t.nesting === 1) {
      const el = t.type === "link_open"
        ? link(t.attrGet("href") ?? "")
        : t.type === "spoiler_open" ? spoiler()
        : h(TAGS.has(t.tag) ? t.tag : "span", {});
      add(el);
      stack.push(el);
    } else if (t.nesting === -1) stack.pop();
    else if (t.type === "text") add(...tagged(t.content));
    else if (t.type === "code_inline") add(h("code", {}, t.content));
    else if (t.type === "fence" || t.type === "code_block") {
      add(code(t.content.replace(/\n$/, ""), t.info.trim().split(/\s/)[0]));
    } else if (t.type === "math") add(math(t.content, t.markup === "$$"));
    else if (t.type === "softbreak" || t.type === "hardbreak") add(h("br"));
    else if (t.type === "hr") add(h("hr"));
    else if (t.content) add(t.content);
  }
  return root;
}

// Links are copied, never opened: irch opens nothing on its own.
const link = (href) => h("span", { class: "link", title: `Copy ${href}`,
  onclick: () => copy(href, "Link copied") });

function spoiler() {
  const s = h("span", { class: "spoiler", title: "Spoiler" });
  s.addEventListener("click", () => s.classList.add("shown"));
  return s;
}

// @names, marked.
const AT = /((?<![\p{L}\p{N}_])@[\p{L}\p{N}_.-]*[\p{L}\p{N}_])/u;
const tagged = (text) => text.split(AT)
  .map((x, i) => i % 2 ? h("span", { class: "mention" }, x) : x);

/** Prism's token tree as spans (its HTML output is never used). */
function code(src, lang) {
  const P = window.Prism;
  const name = LANG[lang?.toLowerCase()] ?? lang?.toLowerCase();
  const g = name && P?.languages[name];
  const spans = (ts) => ts.map((t) => typeof t === "string" ? t
    : h("span", { class: ["tok", t.type, ...[t.alias ?? []].flat()].join(" ") },
      spans([t.content].flat())));
  return h("pre", { "data-lang": g ? name : null },
    h("code", {}, g ? spans(P.tokenize(src, g)) : src));
}

// Typst's MathML, rebuilt from a parse that runs nothing: math tags and
// math attributes only.
const NS = "http://www.w3.org/1998/Math/MathML";
const MTAGS = new Set(("math mi mn mo ms mtext mspace mrow mfrac msqrt " +
  "mroot msub msup msubsup munder mover munderover mtable mtr mtd mstyle " +
  "mpadded mphantom menclose mmultiscripts mprescripts none").split(" "));
const MATTRS = new Set(("display mathvariant lspace rspace stretchy " +
  "symmetric largeop movablelimits accent accentunder fence separator " +
  "form minsize maxsize columnalign rowalign columnspan rowspan " +
  "linethickness displaystyle scriptlevel width height depth voffset " +
  "notation class").split(" "));

function mathml(n) {
  if (n.nodeType === Node.TEXT_NODE) return document.createTextNode(n.data);
  if (n.namespaceURI !== NS || !MTAGS.has(n.localName)) return null;
  const el = document.createElementNS(NS, n.localName);
  for (const a of n.attributes) {
    if (MATTRS.has(a.name)) el.setAttribute(a.name, a.value);
  }
  el.append(...[...n.childNodes].map(mathml).filter(Boolean));
  return el;
}

const typeset = new Map(); // "1x^2" -> Promise of a <math>, or null

/** Typst math; LaTeX (Temml) when it has a \ or a {, or Typst refuses. */
function math(src, display) {
  const el = h(display ? "div" : "span", { class: "math" }, src);
  const tex = () => {
    try {
      window.temml.render(src, el, { displayMode: display, trust: false,
        throwOnError: true });
    } catch {
      el.className = "math bad";
      el.textContent = display ? `$$${src}$$` : `$${src}$`;
    }
  };
  if (/[\\{]/.test(src)) {
    tex();
    return el;
  }
  const k = `${+display}${src}`;
  if (!typeset.has(k)) {
    typeset.set(k, invoke("math", { src, display }).then((m) => {
      const doc = new DOMParser().parseFromString(m, "text/html");
      return mathml(doc.body.firstChild);
    }).catch(() => null));
  }
  typeset.get(k).then((m) => {
    if (!m) return tex();
    el.classList.add("typst");
    el.replaceChildren(m.cloneNode(true));
  });
  return el;
}

const inl = (tokens) => [...render(tokens, h("span", {})).childNodes];

// ---------- attachments ----------

const MIME = { png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg",
  gif: "image/gif", webp: "image/webp", avif: "image/avif", bmp: "image/bmp" };
const mime = (name) => MIME[name.split(".").pop().toLowerCase()];
const size = (n) => {
  const i = Math.min(4, Math.floor(Math.log2(Math.max(1, n)) / 10));
  return `${i ? (n / 2 ** (10 * i)).toFixed(1) : n} ${"BKMGT"[i]}${i ? "B" : ""}`;
};
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

// An offered file's key is all zeros: its sender's device serves it.
const offered = (f) => /^0+$/.test(f.key);
const fileChip = (f) => h("div", { class: "file-chip" }, icon("file"),
  h("div", { class: "meta", title: "Preview", onclick: () => preview(f) },
    h("b", {}, f.name), h("small", {}, size(f.size),
      offered(f) ? " · from its sender's device, while online" : "")),
  h("button", { title: android ? "Share" : "Save to Downloads",
    onclick: () => saveFile(f) }, icon("down")));

async function saveFile(f) {
  const at = await invoke("save", { space: S.sid, file: f })
    .catch((e) => toast(String(e), true));
  if (at && !android) toast(`Saved to ${at}`);
}

const MEDIA = { mp3: "audio/mpeg", ogg: "audio/ogg", oga: "audio/ogg",
  opus: "audio/ogg", wav: "audio/wav", m4a: "audio/mp4", flac: "audio/flac",
  mp4: "video/mp4", m4v: "video/mp4", webm: "video/webm",
  mov: "video/quicktime" };

/** Text as UTF-8, or null for anything binary. */
function utf8(b) {
  try {
    const t = new TextDecoder("utf-8", { fatal: true }).decode(b);
    return t.includes("\0") ? null : t;
  } catch {
    return null;
  }
}

/** A file, opened in irch: media plays, text and code show (Markdown
 *  rendered); anything else is for saving. */
async function preview(f) {
  if (mime(f.name)) return image(f).then((src) => lightbox(src, f));
  const ext = f.name.split(".").pop().toLowerCase();
  let b;
  try {
    const b64 = await rpc("file", { space: S.sid, file: f });
    b = Uint8Array.from(atob(b64), (c) => c.charCodeAt(0));
  } catch (e) {
    return toast(String(e?.message ?? e), true);
  }
  let body, url;
  if (MEDIA[ext]) {
    url = URL.createObjectURL(new Blob([b], { type: MEDIA[ext] }));
    const tag = MEDIA[ext].startsWith("audio") ? "audio" : "video";
    body = h(tag, { src: url, controls: true, autoplay: true });
  } else {
    const text = utf8(b.subarray(0, 512 << 10));
    if (text == null) return toast("No preview for this file: save it", true);
    body = ext === "md" ? content(text) : code(text, ext);
  }
  await modal((finish) => h("div", { class: "preview" },
    h("div", { class: "m-body" }, h("h2", {}, f.name), body),
    foot(h("button", { class: "btn grey", onclick: () => saveFile(f) },
      android ? "Share" : "Save"),
    h("button", { class: "btn", onclick: () => finish(null) }, "Close"))));
  if (url) URL.revokeObjectURL(url);
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

const SEALED = 25 << 20; // what storage nodes take
const PART = 4 << 20;
const b64 = (blob) => new Promise((ok) => {
  const r = new FileReader();
  r.onload = () => ok(r.result.slice(r.result.indexOf(",") + 1));
  r.readAsDataURL(blob);
});

/** Up to 25 MB, sealed for storage nodes (there while you are offline);
 *  larger, or `keep`, served from this device while it is online. */
async function addFiles(list, keep) {
  for (const file of list) {
    const here = keep || file.size > SEALED;
    if (here) toast(`Sharing ${file.name} from this device…`);
    const f = await (here ? offerFile(file) : sealFile(file))
      .catch(() => null);
    if (f) S.files.push(f);
    renderFilesBar();
  }
}

const sealFile = async (file) =>
  call("attach", { space: S.sid, name: file.name, data: await b64(file) });

/** Into irch's folder in parts (a page knows no paths), then offered. */
async function offerFile(file) {
  const id = randomId(16);
  let path;
  for (let at = 0; at === 0 || at < file.size; at += PART) {
    const part = file.slice(at, at + PART);
    const body = android ? { data: await b64(part) }
      : new Uint8Array(await part.arrayBuffer());
    path = await invoke("upload", body, { headers: { "x-upload": id } });
  }
  return call("offer", { path, name: file.name });
}

/** Desktops: files chosen by path stay where they are (any size, no copy). */
async function offerPaths() {
  const paths = await T.dialog.open({ multiple: true }).catch(() => null);
  for (const path of [paths ?? []].flat()) {
    toast(`Sharing ${path.split(/[\\/]/).pop()} from this device…`);
    const f = await call("offer", { path }).catch(() => null);
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
    !inThread() && tool("thread", "Reply in thread", () => openThread(m.id)),
    tool("note", "Add to notes", () => toNotes(m)),
    mine && tool("pencil", "Edit", () => startEdit(m)),
    (mine || cur()?.admin) &&
      tool("trash", "Delete (hold Shift to skip confirmation)",
        (e) => del(m, e), "danger"));
}

/** Into your notes (loroh's vault, a lab's shared folder in it) as a task
 *  or a quote, linked back here; on phones, onto the clipboard. */
async function toNotes(m) {
  const files = await rpc("notes").catch(() => null);
  const last = store.get("note") ?? "Inbox.md";
  const todo = h("select", {}, h("option", { value: "1" }, "A task"),
    h("option", { value: "" }, "A note (quoted)"));
  const whole = (inThread() || m.thread > 0) &&
    h("input", { type: "checkbox", checked: inThread() });
  const into = files &&
    h("input", { type: "text", value: last, maxLength: 200 });
  into?.setAttribute("list", "notes-list");
  const due = h("input", { type: "date" });
  todo.onchange = () => due.parentElement.classList.toggle("hidden",
    !todo.value);
  const add = async (finish) => {
    let file = into?.value.trim() || null;
    if (file && !file.endsWith(".md")) file += ".md";
    const md = await call("note", { space: S.sid, message: m.id,
      thread: !!whole?.checked, todo: !!todo.value, due: due.value || null,
      file });
    finish(true);
    if (file) {
      store.set("note", file);
      toast(`Added to ${file}`);
    } else await copy(md, "Copied as Markdown: paste it into your notes");
  };
  modal((finish) => h("div", {},
    h("div", { class: "m-body" }, h("h2", {}, "Add to notes"),
      h("p", { class: "sub" }, "Plain Markdown with a link back here, for " +
        "loroh (or any notes app). A shared folder (Lab/Ideas.md) reaches " +
        "everyone who shares it."),
      h("label", {}, "As"), todo,
      h("div", {}, h("label", {}, "Due (optional)"), due),
      whole && h("label", { class: "check" }, whole, "The whole thread"),
      into && [h("label", {}, "Into"), into, h("datalist", { id: "notes-list" },
        files.map((f) => h("option", { value: f })))]),
    foot(cancel(finish), h("button", { class: "btn",
      onclick: () => quiet(add(finish)) }, files ? "Add" : "Copy"))));
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
    ["Storage Nodes", "disk", () => storageFlow(s)],
    "-",
    ...[["All", "All messages"], ["Mentions", "Only @mentions"],
      ["Off", "Nothing"]].map(([level, text]) => [
      (s.level === level ? "● " : "○ ") + "Notify: " + text, "chat",
      () => quiet(call("set_level", { space: s.id, level })
        .then(() => schedule("side")))]),
    "-",
    ["Leave Space", "logout", leaveSpace, "danger"],
  ]);
}


/** Which of your storage nodes keep this space: encrypted, there while
 *  everyone is offline. */
function storageFlow(s) {
  const local = S.me.local_storage;
  const keep = (node, on) => quiet(call("store", { space: s.id, node, on }));
  const paired = s.storage.some((x) => x.node === local);
  modal((finish) => {
    const sw = (x) => h("label", { class: "switch" },
      h("span", {}, short(x.node),
        h("small", {}, x.node === local ? "This computer" : "Storage node")),
      h("input", { type: "checkbox", checked: x.on,
        onchange: (e) => keep(x.node, e.target.checked) }), h("i"));
    const here = local && !paired && h("button", { class: "btn",
      onclick: async () => {
        await quiet(call("store_local").then(() => keep(local, true)));
        finish(true);
      } }, "Keep it on this computer's storage node");
    return h("div", {},
      h("div", { class: "m-body" }, h("h2", {}, "Storage Nodes"),
        h("p", { class: "sub" }, "A storage node keeps this space " +
          "encrypted, so it waits there while everyone is offline."),
        s.storage.map(sw), here,
        !s.storage.length && !here && h("p", { class: "note" },
          "Pair one with its code in My Account, or run nixos.nix on " +
          "this computer.")),
      foot(h("button", { class: "btn grey", onclick: () => finish(null) },
        "Done")));
  });
}

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
    actions.push(act("Call", () => startDm(id, true), "green"));
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
    // A dialog closed just before this one fires its close event late,
    // on this one: only a close that left it closed counts.
    d.onclose = () => d.open || finish(null);
    // A click outside closes; never return false here, which would
    // cancel every click inside (checkboxes would not toggle).
    d.onclick = (e) => {
      if (e.target === d) finish(null);
    };
    d.showModal();
    // Phones: the keyboard only where typing is the point (one field).
    const f = d.querySelectorAll("input[type=text]:not([readonly]),textarea");
    if (!mobile() || f.length === 1) f[0]?.focus({ preventScroll: true });
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
    // Handled, so the same Enter does not also press the button that
    // gets the focus back when the dialog closes (and open it again).
    input.addEventListener("keydown", (e) => {
      if (e.key !== "Enter") return;
      e.preventDefault();
      submit();
    });
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

/** A code to hand on: copy, share (Android), and a QR code to scan; a
 *  secret one is never shared to other apps, and leaves the clipboard
 *  after a minute. */
async function showCode({ title, label, code, note, secret }) {
  const qr = await invoke("qr", { text: code }).catch(() => null);
  modal((finish) => {
    const b = h("button", { class: "btn", onclick: async () => {
      await copy(code);
      if (secret) setTimeout(() => clip?.writeText("").catch(() => {}), 60e3);
      b.textContent = secret ? "Copied for a minute" : "Copied";
      b.classList.add("green");
    } }, "Copy");
    const field = h("input",
      { class: "field mono", type: "text", value: code, readOnly: true });
    field.addEventListener("focus", () => field.select());
    const share = android && !secret && h("button", { class: "btn",
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

async function startDm(user, ring) {
  const id = await call("dm", { space: S.sid, user }).catch(() => null);
  if (id) await (ring ? open(id).then(() => callIn(id)) : open(id));
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
    const storage = h("input",
      { type: "text", placeholder: "irch:host:…", maxLength: 8192 });
    const save = async () => {
      const n = name.value.trim();
      if (n && n !== S.me.name) await quiet(call("set_name", { name: n }));
      const code = storage.value.trim();
      if (code) await quiet(call("add_storage", { code })
        .then(() => toast("Paired: each space's menu picks what it keeps")));
      S.me = await rpc("me");
      renderUserPanel();
      schedule("members", "msgs");
      finish(true);
    };
    name.addEventListener("keydown", (e) => e.key === "Enter" && save());
    // Switches act at once, like the eye in the user panel.
    const sw = (title, small, checked, change) => h("label",
      { class: "switch" }, h("span", {}, title, h("small", {}, small)),
      h("input", { type: "checkbox", checked,
        onchange: (e) => change(e.target.checked) }), h("i"));
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
        sw("Invisible", "Others see you offline; typing stays private.",
          S.me.invisible, (on) => setInvisible(on)),
        android && h("p", { class: "note" }, S.me.push
          ? "Notifications arrive while irch sleeps: whoever writes wakes " +
            "this phone through your push app (a ping with nothing in it)."
          : "To be notified while irch sleeps, install a UnifiedPush app " +
            "(ntfy: on F-Droid, or on Google Play, which uses Google's " +
            "connection and costs no battery), then reopen irch."),
        h("label", {}, "Storage node"), storage,
        h("p", { class: "note" }, "Paste the code `irch --serve` prints " +
          "on an always-on machine: it keeps your spaces' ciphertext, " +
          "never the keys, so messages wait there while everyone is offline."),
        h("button", {
          class: "choice", onclick: () => { finish(null); linkDevice(); },
        }, icon("phone"), "Link a device"),
        h("button", {
          class: "choice", onclick: () => { finish(null); updates(false); },
        }, icon("down"), "Check for updates"),
        h("p", { class: "note" }, "Your identity is a key on your devices. " +
          "Messages are end-to-end encrypted; read positions stay here.")),
      foot(cancel(finish),
        h("button", { class: "btn", onclick: save }, "Save")));
  });
}

async function linkDevice() {
  const code = await call("link_code").catch(() => null);
  if (code) showCode({ title: "Link a device", label: "Your link code", code,
    secret: true,
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

/** A room: a named thread in a channel, there before anyone replies. */
async function newRoom(s, ch) {
  select(s.id, ch.id);
  const name = (await ask({ title: "New Room", label: "Room name",
    sub: `A thread in #${ch.name}, listed under it.`, placeholder: "plans",
    ok: "Create Room" }))?.trim();
  if (!name) return;
  const id = await publish({ Post: { channel: ch.id, text: name,
    reply: null, files: [] } });
  if (!id) return;
  await publish({ Topic: { thread: id.slice(0, 16), name } });
  S.spaces = await rpc("spaces");
  await openThread(id);
}

async function renameRoom(s, t) {
  const name = (await ask({ title: "Rename Thread", label: "Thread name",
    value: t.name }))?.trim();
  if (name) await call("publish", { space: s.id,
    op: { Topic: { thread: t.id, name } } }).catch(() => {});
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

const toggleInvisible = () => setInvisible(!S.me.invisible);

async function setInvisible(on) {
  await quiet(call("set_invisible", { on }));
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

/** The composer's commands, as the terminal's: /nick, /join, /dm, ... */
const SLASH = {
  nick: async (name) => {
    if (!name) return toast("/nick NAME", true);
    await call("set_name", { name });
    S.me = await rpc("me");
    renderUserPanel();
    schedule("members", "msgs");
  },
  join: async (code) => {
    if (!code) return joinFlow();
    const id = await call("join", { code });
    if (id) await open(id);
  },
  dm: (who) => {
    const q = who.replace(/^@/, "").toLowerCase();
    const m = S.members.find((x) => x.name.toLowerCase() === q && !isMe(x.id));
    return m ? startDm(m.id) : toast(`Nobody called ${who} here`, true);
  },
  me: (text) => text && say(`_${text}_`),
  shrug: (text) => say(`${text} ¯\\\\_(ツ)\\_/¯`.trim()),
  thread: (text) => {
    const m = S.msgs.findLast((x) => !isMe(x.author)) ?? S.msgs.at(-1);
    if (!m || inThread()) return toast("Nothing to reply to here", true);
    return openThread(m.id).then(() => text && say(text));
  },
  invite: () => invitePeople(),
  leave: () => leaveSpace(),
  notes: () => S.msgs.at(-1) && toNotes(S.msgs.at(-1)),
  help: () => keysHelp(),
};

const say = (text) => {
  $("input").value = text;
  return send();
};

// ---------- vim ----------
// Esc in the composer is normal mode: keys act on a selected message.

const KEYS = [
  ["Esc", "out of the composer: pick messages (normal mode)"],
  ["j / k", "next / previous message"], ["gg / G", "first / last"],
  ["r", "reply"], ["t", "reply in its thread"], ["e", "edit (yours)"],
  ["dd", "delete"], ["y", "copy its text"], ["+", "react"],
  ["n", "add to notes"], ["i, a, Enter", "back to typing"],
  ["Ctrl+K", "jump to a channel or DM"], ["Alt+↑/↓", "previous / next channel"],
  ["/nick, /join, /dm, /me, /shrug, /thread, /invite, /leave, /notes",
    "in the composer"],
];

function keysHelp() {
  modal((finish) => h("div", {},
    h("div", { class: "m-body" }, h("h2", {}, "Keys"),
      h("table", { class: "keys" }, KEYS.map(([k, v]) =>
        h("tr", {}, h("td", {}, h("kbd", {}, k)), h("td", {}, v))))),
    foot(h("button", { class: "btn", onclick: () => finish(null) }, "OK"))));
}

const sel = () => S.msgs.find((m) => m.id === S.sel);

function pick(m) {
  $("list").querySelector(".msg.sel")?.classList.remove("sel");
  S.sel = m?.id ?? null;
  const el = m && $("list").querySelector(`[data-id="${m.id}"]`);
  el?.classList.add("sel");
  el?.scrollIntoView({ block: "nearest" });
}

/** A key in normal mode; true if it did something. */
function vim(e) {
  const k = e.key, m = sel(), i = S.msgs.indexOf(m), last = S.vimLast;
  S.vimLast = k;
  const at = (j) => pick(S.msgs[Math.max(0, Math.min(S.msgs.length - 1, j))]);
  const row = () => $("list").querySelector(`[data-id="${m.id}"]`);
  const act = {
    j: () => at(i + 1), k: () => at(i - 1), ArrowDown: () => at(i + 1),
    ArrowUp: () => at(i - 1), G: () => at(S.msgs.length - 1),
    g: () => last === "g" && at(0),
    r: () => m && (typeMode(), startReply(m)),
    t: () => m && !inThread() && openThread(m.id),
    e: () => m && isMe(m.author) && (typeMode(), startEdit(m)),
    d: () => m && last === "d" && (isMe(m.author) || cur()?.admin) && del(m),
    y: () => m && copy(m.text, "Copied"),
    "+": () => m && picker(row(), (em) => react(m, em, true)),
    n: () => m && toNotes(m),
    i: () => typeMode(), a: () => typeMode(), Enter: () => typeMode(),
    "?": () => keysHelp(),
  }[k];
  if (!act) return false;
  act();
  return true;
}

function typeMode() {
  pick(null);
  S.normal = false;
  $("input").focus({ preventScroll: true });
}

async function send() {
  const input = $("input");
  const text = input.value.trim();
  const s = cur();
  const ch = chan();
  const files = S.files;
  const cmd = /^\/(\w+)\s*([\s\S]*)$/.exec(text);
  if (cmd && SLASH[cmd[1]]) {
    input.value = "";
    autosize(input);
    return quiet(Promise.resolve(SLASH[cmd[1]](cmd[2].trim())));
  }
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
  $("call-btn").onclick = ring;
  av.call.changed = renderCall;
  // Tiles follow who speaks and whose video still arrives.
  setInterval(renderTiles, 400);
  const showMembers = (on) => {
    $("members").classList.toggle("hidden-panel", !on);
    $("members-btn").classList.toggle("on", on);
  };
  showMembers(store.get("members") === "1"); // chat first: shown on request
  // Phones: swipe right for channels (or away from members), left away
  // from channels (or to members), as in Discord. Not in text or code.
  let touch = null;
  addEventListener("touchstart", (e) => {
    const t = e.touches[0];
    const skip = e.touches.length > 1 || $("modal").open ||
      e.target.closest?.("textarea, input, pre");
    touch = skip ? null : [t.clientX, t.clientY];
  }, { passive: true });
  addEventListener("touchend", (e) => {
    const t = e.changedTouches[0];
    const [dx, dy] = touch ? [t.clientX - touch[0], t.clientY - touch[1]] : [];
    touch = null;
    if (!mobile() || !(Math.abs(dx) > 60 && Math.abs(dx) > 2 * Math.abs(dy))) {
      return;
    }
    const body = document.body.classList;
    const members = !$("members").classList.contains("hidden-panel");
    if (dx > 0 && members) showMembers(false);
    else if (dx > 0) body.add("drawer");
    else if (body.contains("drawer")) body.remove("drawer");
    else if (cur() && !cur().dm) showMembers(true);
  });
  $("members-btn").onclick = () => {
    const on = $("members").classList.contains("hidden-panel");
    showMembers(on);
    store.set("members", on ? "1" : "0");
  };
  $("attach-btn").append(icon("clip"));
  const choose = (keep) => ((S.keep = keep), $("picker").click());
  $("attach-btn").onclick = (e) => menu(e.currentTarget, [
    ["Upload: storage nodes keep it (to 25 MB)", "clip", () => choose(false)],
    ["Share from this device (any size)", "disk",
      () => (android ? choose(true) : offerPaths())],
  ]);
  $("picker").onchange = (e) => {
    addFiles([...e.target.files], S.keep);
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
    else if (inThread()) select(S.sid, S.thread.parent);
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
      else if (S.normal) {
        S.normal = false;
        pick(null);
      }
      else if (e.target === input && S.msgs.length) {
        input.blur(); // vim: normal mode, on the newest message
        S.normal = true;
        pick(S.msgs.at(-1));
      } else if (inThread()) select(S.sid, S.thread.parent);
      else {
        sc.scrollTop = sc.scrollHeight;
        markRead();
      }
    } else if (S.normal && !typing && !$("modal").open && !e.ctrlKey &&
      !e.metaKey && !e.altKey) {
      if (vim(e)) e.preventDefault();
    } else if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey &&
      !typing && !$("modal").open && !input.disabled) {
      input.focus({ preventScroll: true }); // type anywhere
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
    } else if (e.t === "call") {
      schedule("side");
    } else if (e.t === "typing" && !isMe(e.user)) {
      const k = `${e.space}|${e.channel}`;
      if (!S.typing.has(k)) S.typing.set(k, new Map());
      S.typing.get(k).set(e.user, Date.now() + 6000);
      renderTyping();
      setTimeout(renderTyping, 6100);
    }
  });
  await T.event.listen("open-url", ({ payload }) => openUrl(payload));
  await T.event.listen("alert", ({ payload: a }) => {
    if (a.ring) return incoming(a.space, a.room, a.title);
    if (!android) chime();
  });
  await loadSidebar();
  if (!S.spaces.length) renderHeader();
  const url = await invoke("opened").catch(() => null);
  if (url) openUrl(url);
  updates(true);
  $("boot").remove();
}

start();
