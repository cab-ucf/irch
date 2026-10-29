// Frame: kind (0 audio, 1 video, 2 key, 3 codecs) ‖ codec ‖ f64 timestamp
// ‖ data; heard ones lead with the sender's 32-byte id. Each side says
// which codecs it decodes; a sender uses the newest every listener has.

const core = window.__TAURI__?.core;
// A codec's byte is its place here plus one; new ones only at the end.
const VIDEO = ["vp8", "avc1.42E01F", "vp09.00.10.08", "av01.0.04M.08"];
// AV1, then VP9: open, and the most picture for the bits.
const PREFER = [3, 2, 0, 1];
// What a listener from before codecs were said decodes.
const OLD = new Set([0, 1]);
const CAPS = 3;
const RATE = 48000;
const hex = (b) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
const shut = (x) => { try { x?.close(); } catch {} };
const rms = (s) => Math.sqrt(s.reduce((e, x) => e + x * x, 0) / s.length);
const quiet = (p) => p?.catch?.(() => {});
const h0 = (tag) => document.createElement(tag);

export const call = {
  on: null, // { space, channel }
  mic: true,
  cam: false,
  screen: false,
  self: null, // <video> of what we send
  level: 0,
  peers: new Map(), // id -> { canvas, video (last frame), level, ... }
  changed: () => {},
  // Why video fails, said once: silent, it would just never show.
  trouble: () => {},
};

// Some phones' hardware encoders take frames and never give any back.
const SOFT = { hardwareAcceleration: "prefer-software" };

export const supported = () => typeof AudioEncoder === "function" &&
  typeof VideoDecoder === "function" && !!navigator.mediaDevices;
export const canShare = () => !!navigator.mediaDevices?.getDisplayMedia;

let ctx, mic, aenc, venc, timer, tuned, said;
// Codecs this device decodes, once asked.
let decodes = null;

async function decodable() {
  const ok = async (codec) =>
    (await VideoDecoder.isConfigSupported({ codec }).catch(() => ({})))
      .supported;
  const all = await Promise.all(VIDEO.map(ok));
  return VIDEO.map((_, i) => i).filter((i) => all[i]);
}

/** Every few seconds: late joiners hear it too. */
function sayCodecs() {
  const b = new Uint8Array(10 + decodes.length);
  b[0] = CAPS;
  b.set(decodes.map((i) => i + 1), 10);
  media(b);
}

// Android's bridge turns bytes into JSON arrays of numbers (4x, slow to
// parse): base64 there, a third larger and quick.
const android = /Android/.test(navigator.userAgent);
const b64 = (b) => {
  let s = "";
  for (let i = 0; i < b.length; i += 0x8000) {
    s += String.fromCharCode(...b.subarray(i, i + 0x8000));
  }
  return btoa(s);
};
const media = (b) => quiet(core.invoke("media", android ? { data: b64(b) } : b));

function send(kind, codec, chunk) {
  const b = new Uint8Array(10 + chunk.byteLength);
  b.set([kind, codec]);
  new DataView(b.buffer).setFloat64(2, chunk.timestamp, true);
  chunk.copyTo(b.subarray(10));
  media(b);
}

export async function join(space, channel, rpc) {
  await leave(rpc);
  ctx = new AudioContext({ sampleRate: RATE });
  await ctx.audioWorklet.addModule("audio.js");
  quiet(ctx.resume());
  if (!tuned) {
    const ch = new core.Channel();
    ch.onmessage = heard;
    await core.invoke("tune", { to: ch });
    tuned = true;
  }
  await rpc("ring", { space, channel });
  call.on = { space, channel };
  call.since = Date.now();
  call.met = false;
  call.changed();
  decodes ??= await decodable();
  sayCodecs();
  said = setInterval(sayCodecs, 5000);
  mic = await navigator.mediaDevices.getUserMedia({ audio: {
    echoCancellation: true, noiseSuppression: true, autoGainControl: true,
  } }).catch(() => null);
  call.mic = !!mic;
  if (!mic) return call.changed();
  aenc = new AudioEncoder({ output: (c) => send(0, 0, c), error: () => {} });
  // In-band FEC: a lost packet is rebuilt from the next, not heard as a gap.
  aenc.configure({ codec: "opus", sampleRate: RATE, numberOfChannels: 1,
    bitrate: 32000, opus: { frameDuration: 20000, useinbandfec: true,
      packetlossperc: 10, application: "voip" } });
  const node = new AudioWorkletNode(ctx, "mic");
  node.port.onmessage = ({ data }) => {
    call.level = call.mic ? rms(data) : 0;
    if (!call.mic || aenc.state !== "configured") return;
    aenc.encode(new AudioData({ format: "f32-planar", sampleRate: RATE,
      numberOfChannels: 1, numberOfFrames: data.length, data,
      timestamp: ctx.currentTime * 1e6 }));
  };
  // Pulled through a silent output: some engines skip unconnected nodes.
  const mute = new GainNode(ctx, { gain: 0 });
  ctx.createMediaStreamSource(mic).connect(node).connect(mute)
    .connect(ctx.destination);
}

export async function leave(rpc) {
  if (!call.on) return;
  call.on = null;
  clearInterval(said);
  await video(null);
  mic?.getTracks().forEach((t) => t.stop());
  [aenc, ctx].forEach(shut);
  for (const p of call.peers.values()) [p.adec, p.vdec].forEach(shut);
  call.peers.clear();
  call.changed();
  await quiet(rpc("hangup"));
}

export async function video(kind) {
  clearInterval(timer);
  shut(venc);
  call.self?.srcObject?.getTracks().forEach((t) => t.stop());
  Object.assign(call, { self: null, cam: false, screen: false });
  if (kind && call.on) {
    const m = navigator.mediaDevices;
    const fps = kind === "screen" ? 10 : 15;
    // A phone held upright sends a face upright: 3:4, cropped if its
    // camera gives landscape anyway.
    const tall = matchMedia("(pointer: coarse) and (orientation: portrait)");
    const upright = () => kind === "cam" && tall.matches;
    const [x, y] = upright() ? [720, 960] : [640, 480];
    const s = await (kind === "screen"
      ? m.getDisplayMedia({ video: { frameRate: fps } })
      : m.getUserMedia({ video: { facingMode: "user", width: { ideal: x },
        height: { ideal: y }, aspectRatio: { ideal: x / y },
        frameRate: fps } }));
    const v = document.createElement("video");
    Object.assign(v, { muted: true, playsInline: true, srcObject: s });
    await quiet(v.play());
    s.getVideoTracks()[0].onended = () => video(null);
    Object.assign(call, { self: v, [kind]: true });
    let codec = -1, n = 0, w = 0, h = 0, sent = 0;
    const ok = async (c) =>
      (await VideoEncoder.isConfigSupported(c).catch(() => ({}))).supported;
    // Each codec this device encodes, in software if it can.
    const can = new Map();
    for (const i of PREFER) {
      for (const x of [SOFT, {}]) {
        if (await ok({ codec: VIDEO[i], width: 640, height: 480, ...x })) {
          can.set(i, x);
          break;
        }
      }
    }
    if (!can.size) call.trouble("this device cannot encode video");
    // One that failed here is not tried again this call.
    const bad = new Set();
    const pick = () => PREFER.find((i) => can.has(i) && !bad.has(i) &&
      [...call.peers.values()].every((p) => (p.caps ?? OLD).has(i))) ?? -1;
    const make = () => new VideoEncoder({
      error: (e) => {
        call.trouble(`video encoder (${VIDEO[codec]}): ${e.message}`);
        bad.add(codec);
        w = 0;
      },
      output: (c) => (sent++, send(c.type === "key" ? 2 : 1, codec + 1, c)),
    });
    venc = make();
    // Frames from a canvas, where a video element gives none.
    const canvas = h0("canvas");
    let sw = 0, sh = 0, crop = false;
    const frame = (t) => {
      try {
        if (crop) throw 0;
        return new VideoFrame(v, { timestamp: t });
      } catch {
        [canvas.width, canvas.height] = [w, h];
        const sx = (sw - w * sh / h) / 2;
        canvas.getContext("2d")
          .drawImage(v, Math.max(0, sx), 0, sw - 2 * Math.max(0, sx), sh,
            0, 0, w, h);
        return new VideoFrame(canvas, { timestamp: t });
      }
    };
    timer = setInterval(() => {
      // Someone joined who lacks this codec, or it failed: the next one.
      const best = pick();
      if (best !== codec) [codec, w] = [best, 0];
      if (codec < 0 || v.readyState < 2) return;
      if (venc.state === "closed") venc = make();
      if (venc.encodeQueueSize > 2) return;
      try {
        const turn = upright() && v.videoWidth > v.videoHeight;
        if (v.videoWidth !== sw || v.videoHeight !== sh || turn !== crop ||
          !w) {
          [sw, sh, crop, n, sent] = [v.videoWidth, v.videoHeight, turn, 0, 0];
          const even = (k) => Math.max(2, Math.round(k / 2) * 2);
          [w, h] = crop ? [even(sh * 3 / 4), even(sh)] : [even(sw), even(sh)];
          venc.configure({ codec: VIDEO[codec], width: w, height: h,
            bitrate: fps === 10 ? 1.5e6 : 6e5, framerate: fps,
            ...can.get(codec), latencyMode: "realtime",
            avc: { format: "annexb" } });
          const c = codec;
          setTimeout(() => codec !== c || sent || (bad.add(c),
            call.trouble(`${VIDEO[c]} gives no frames here`)), 5000);
        }
        const f = frame(performance.now() * 1000);
        venc.encode(f, { keyFrame: n++ % (2 * fps) === 0 });
        f.close();
      } catch (e) {
        call.trouble(`video: ${e.message ?? e}`);
      }
    }, 1000 / fps);
  }
  call.changed();
}

/** Drop peers no longer in the call, nor heard for a while. */
export function keep(ids) {
  for (const [id, p] of call.peers) {
    if (ids.includes(id) || Date.now() - p.heard < 5000) continue;
    [p.adec, p.vdec].forEach(shut);
    p.out.disconnect();
    call.peers.delete(id);
  }
}

function peer(id) {
  let p = call.peers.get(id);
  if (p) return p;
  p = { id, canvas: document.createElement("canvas"), video: 0, level: 0 };
  p.out = new AudioWorkletNode(ctx, "play");
  p.out.connect(ctx.destination);
  const adec = () => {
    p.adec = new AudioDecoder({ error: adec, output: (d) => {
      const s = new Float32Array(d.numberOfFrames);
      d.copyTo(s, { planeIndex: 0, format: "f32-planar" });
      d.close();
      [p.level, p.spoke] = [rms(s), Date.now()];
      p.out.port.postMessage(s, [s.buffer]);
    } });
    p.adec.configure({ codec: "opus", sampleRate: RATE, numberOfChannels: 1 });
  };
  const vdec = () => {
    Object.assign(p, { codec: 0, key: false });
    p.vdec = new VideoDecoder({ error: (e) => {
      call.trouble(`decoding video: ${e.message}`);
      vdec();
    }, output: (f) => {
      const c = p.canvas;
      if (c.width !== f.displayWidth) c.width = f.displayWidth;
      if (c.height !== f.displayHeight) c.height = f.displayHeight;
      c.getContext("2d").drawImage(f, 0, 0);
      f.close();
      p.video = Date.now();
    } });
  };
  adec();
  vdec();
  call.peers.set(id, p);
  call.changed();
  return p;
}

function heard(buf) {
  if (!call.on) return;
  const b = new Uint8Array(buf);
  const f = b.subarray(32);
  const [kind, codec] = f;
  const timestamp = new DataView(f.buffer, f.byteOffset + 2, 8)
    .getFloat64(0, true);
  const data = f.subarray(10);
  const p = peer(hex(b.subarray(0, 32)));
  p.heard = Date.now();
  if (kind === CAPS) {
    p.caps = new Set([...data].map((c) => c - 1));
    return;
  }
  if (kind === 0) {
    if (p.adec.state === "configured") {
      p.adec.decode(new EncodedAudioChunk({ type: "key", timestamp, data }));
    }
    return;
  }
  if (kind === 2 && p.codec !== codec && VIDEO[codec - 1]) {
    try {
      // Each frame shown as it comes, not held for reordering.
      p.vdec.configure({ codec: VIDEO[codec - 1], optimizeForLatency: true });
      Object.assign(p, { codec, key: false });
    } catch { return; }
  }
  if (p.codec !== codec || !(p.key ||= kind === 2)) return;
  const type = kind === 2 ? "key" : "delta";
  p.vdec.decode(new EncodedVideoChunk({ type, timestamp, data }));
}
