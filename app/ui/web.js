// irch in a browser: what the app's shell gives the page, from your own
// node over HTTP (irch --web; tui/src/web.rs). Calls are POSTs; events
// and call frames stream down while the page is in sight or in a call,
// and nothing is said otherwise: no polls, no pings. Opened at the node
// (http://127.0.0.1:4724/#<token>), the token is the fragment; through
// irweb's page, its worker adds it.
(() => {
  if (window.__TAURI__) return;
  const token = /^#([0-9a-f]{64})$/.exec(location.hash)?.[1];
  const head = token ? { "irweb-access": token } : {};
  const bytes = (b) => b?.data == null ? b
    : Uint8Array.from(atob(b.data), (c) => c.charCodeAt(0));
  const post = async (cmd, body, headers) => {
    const r = await fetch(`api/${cmd}`, { method: "POST", body,
      headers: { ...head, ...headers } });
    const t = await r.text();
    if (!r.ok) throw t || `${r.status}`;
    return JSON.parse(t);
  };

  // [len u32][bytes] off a response, for as long as it lasts.
  async function* frames(rd) {
    let buf = new Uint8Array(0), n;
    for (;;) {
      while (buf.length >= 4 && buf.length >= 4 +
        (n = new DataView(buf.buffer, buf.byteOffset).getUint32(0))) {
        yield buf.slice(4, 4 + n);
        buf = buf.subarray(4 + n);
      }
      const { value, done } = await rd.read();
      if (done) return;
      const t = new Uint8Array(buf.length + value.length);
      t.set(buf);
      t.set(value, buf.length);
      buf = t;
    }
  }
  // Open while wanted; cut, again after a pause (the node may restart).
  const streams = new Map();
  function stream(path, each) {
    const old = streams.get(path); // a new call's tune replaces the last
    if (old) old.dead = true, old.stop?.();
    const s = { each, stop: null };
    streams.set(path, s);
    const run = async () => {
      const c = new AbortController();
      s.stop = () => c.abort();
      try {
        const r = await fetch(path, { headers: head, signal: c.signal });
        if (r.status === 403) return; // a wrong token stays wrong
        if (r.ok) for await (const f of frames(r.body.getReader())) each(f);
      } catch {}
      if (c.signal.aborted || s.dead) return;
      s.stop = null;
      setTimeout(() => !s.stop && !s.dead && wanted() && run(), 3e3);
    };
    s.run = run;
    if (wanted()) run();
  }
  // Hidden and not in a call: let go; seen again, the events say
  // "changed" first, so the page catches up.
  let sent = 0;
  const wanted = () => !document.hidden || Date.now() - sent < 5e3;
  document.addEventListener("visibilitychange", () => {
    for (const s of streams.values()) {
      if (wanted() && !s.stop && !s.dead) s.run();
      else if (!wanted()) s.stop?.(), s.stop = null;
    }
  });

  const heard = new Map();
  stream("api/events", (f) => {
    const { event, payload } = JSON.parse(new TextDecoder().decode(f));
    for (const cb of heard.get(event) ?? []) cb({ payload });
    if (event === "alert" && document.hidden &&
      window.Notification?.permission === "granted") {
      new Notification(payload.title, { body: payload.body });
    }
  });
  document.addEventListener("click", () =>
    window.Notification?.permission === "default" &&
      Notification.requestPermission(), { once: true });

  // Call frames, in order: one POST in flight, the rest in the next.
  let q = [], busy = false;
  async function media(b) {
    sent = Date.now();
    if (q.length < 256) q.push(bytes(b));
    if (busy) return;
    busy = true;
    while (q.length) {
      const all = q, out = new Uint8Array(
        all.reduce((n, f) => n + 4 + f.length, 0));
      q = [];
      let at = 0;
      for (const f of all) {
        new DataView(out.buffer).setUint32(at, f.length);
        out.set(f, at + 4);
        at += 4 + f.length;
      }
      await post("media", out).catch(() => {});
    }
    busy = false;
  }

  const no = (what) => () => { throw what; };
  const calls = {
    irch: ({ call }) => post("irch", JSON.stringify(call)),
    upload: (b, o) => post("upload", bytes(b), o?.headers),
    media,
    tune: ({ to }) => stream("api/tune", (f) => to.onmessage(f.buffer)),
    // Fetched whole, then the browser's own download.
    save: async ({ space, file }) => {
      const b = bytes({ data: await calls.irch({ call: { cmd: "file",
        space, file } }) });
      const a = document.createElement("a");
      a.href = URL.createObjectURL(new Blob([b]));
      a.download = file.name;
      a.click();
      setTimeout(() => URL.revokeObjectURL(a.href), 6e4);
      return file.name;
    },
    open_link: ({ url }) => void open(url, "_blank", "noopener,noreferrer"),
    pick: () => [],
    private: () => false,
    opened: () => null,
    update: () => null,
    viewing: () => null,
    in_call: () => null,
    hush: () => null,
    log: () => null,
    profiles: no("profiles are the node's own"),
    link: no("link the node itself: irch --link"),
  };
  window.__TAURI__ = {
    core: {
      invoke: async (cmd, args, o) => {
        if (!calls[cmd]) throw `not in a browser (${cmd})`;
        return calls[cmd](args, o);
      },
      Channel: class { onmessage = () => {}; },
    },
    event: {
      listen: async (name, cb) => {
        if (!heard.has(name)) heard.set(name, new Set());
        heard.get(name).add(cb);
        return () => heard.get(name).delete(cb);
      },
    },
    clipboardManager: { writeText: (t) => navigator.clipboard.writeText(t) },
  };
})();
