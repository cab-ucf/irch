// Calls: the page captures, encodes and decodes (WebCodecs); irch carries
// the frames over iroh, peer to peer. A frame is kind (0 audio, 1 video,
// 2 keyframe) ‖ codec ‖ timestamp (f64) ‖ data; frames heard arrive with
// the sender's 32-byte id in front.

const core = window.__TAURI__?.core;
const VIDEO = ["vp8", "avc1.42E01F"];
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
  level: 0, // how loud we are
  peers: new Map(), // id -> { canvas, video (last frame), level, ... }
  changed: () => {},
  // Why video fails here (the page says so, once): silent, it never shows.
  trouble: () => {},
};

// Software VP8 where it can be: some phones' hardware encoders take
// frames and never give any back.
const SOFT = { hardwareAcceleration: "prefer-software" };

export const supported = () => typeof AudioEncoder === "function" &&
  typeof VideoDecoder === "function" && !!navigator.mediaDevices;
export const canShare = () => !!navigator.mediaDevices?.getDisplayMedia;

let ctx, mic, aenc, venc, timer, tuned;

function send(kind, codec, chunk) {
  const b = new Uint8Array(10 + chunk.byteLength);
  b.set([kind, codec]);
  new DataView(b.buffer).setFloat64(2, chunk.timestamp, true);
  chunk.copyTo(b.subarray(10));
  quiet(core.invoke("media", b));
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
  call.changed();
  mic = await navigator.mediaDevices.getUserMedia({ audio: {
    echoCancellation: true, noiseSuppression: true, autoGainControl: true,
  } }).catch(() => null);
  call.mic = !!mic;
  if (!mic) return call.changed();
  aenc = new AudioEncoder({ output: (c) => send(0, 0, c), error: () => {} });
  aenc.configure({ codec: "opus", sampleRate: RATE, numberOfChannels: 1,
    bitrate: 32000 });
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
  await video(null);
  mic?.getTracks().forEach((t) => t.stop());
  [aenc, ctx].forEach(shut);
  for (const p of call.peers.values()) [p.adec, p.vdec].forEach(shut);
  call.peers.clear();
  call.changed();
  await quiet(rpc("hangup"));
}

/** Send the camera, the screen, or (null) no video. */
export async function video(kind) {
  clearInterval(timer);
  shut(venc);
  call.self?.srcObject?.getTracks().forEach((t) => t.stop());
  Object.assign(call, { self: null, cam: false, screen: false });
  if (kind && call.on) {
    const m = navigator.mediaDevices;
    const fps = kind === "screen" ? 10 : 15;
    // A phone held upright sends upright video.
    const tall = matchMedia("(pointer: coarse) and (orientation: portrait)");
    const [x, y] = tall.matches ? [480, 640] : [640, 480];
    const s = await (kind === "screen"
      ? m.getDisplayMedia({ video: { frameRate: fps } })
      : m.getUserMedia({ video: { facingMode: "user", width: { ideal: x },
        height: { ideal: y }, frameRate: fps } }));
    const v = document.createElement("video");
    Object.assign(v, { muted: true, playsInline: true, srcObject: s });
    await quiet(v.play());
    s.getVideoTracks()[0].onended = () => video(null);
    Object.assign(call, { self: v, [kind]: true });
    let codec = 0, n = 0, w = 0, h = 0, hw = SOFT, sent = 0;
    const ok = async (c) =>
      (await VideoEncoder.isConfigSupported(c).catch(() => ({}))).supported;
    pick: for (const x of [SOFT, {}]) {
      for (codec = 0; codec < VIDEO.length; codec++) {
        hw = x;
        if (await ok({ codec: VIDEO[codec], width: 640, height: 480, ...x })) {
          break pick;
        }
      }
    }
    if (codec === VIDEO.length) {
      call.trouble("this device cannot encode video (no VP8 or H.264)");
    }
    venc = new VideoEncoder({
      error: (e) => call.trouble(`video encoder: ${e.message}`),
      output: (c) => (sent++, send(c.type === "key" ? 2 : 1, codec + 1, c)),
    });
    // Frames from a canvas, where a video element gives none.
    const canvas = h0("canvas");
    const frame = (t) => {
      try {
        return new VideoFrame(v, { timestamp: t });
      } catch {
        [canvas.width, canvas.height] = [w, h];
        canvas.getContext("2d").drawImage(v, 0, 0, w, h);
        return new VideoFrame(canvas, { timestamp: t });
      }
    };
    timer = setInterval(() => {
      if (v.readyState < 2 || venc.encodeQueueSize > 2) return;
      try {
        if (v.videoWidth !== w || v.videoHeight !== h) {
          [w, h, n] = [v.videoWidth, v.videoHeight, 0];
          venc.configure({ codec: VIDEO[codec], width: w, height: h,
            bitrate: fps === 10 ? 1.5e6 : 6e5, framerate: fps, ...hw,
            latencyMode: "realtime", avc: { format: "annexb" } });
        }
        const f = frame(performance.now() * 1000);
        venc.encode(f, { keyFrame: n++ % (2 * fps) === 0 });
        f.close();
      } catch (e) {
        call.trouble(`video: ${e.message ?? e}`);
      }
    }, 1000 / fps);
    // Nothing out in a few seconds: say so, rather than send nothing.
    setTimeout(() => venc?.state === "configured" && !sent &&
      call.trouble("this device's video encoder gives no frames"), 5000);
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
  if (kind === 0) {
    if (p.adec.state === "configured") {
      p.adec.decode(new EncodedAudioChunk({ type: "key", timestamp, data }));
    }
    return;
  }
  if (kind === 2 && p.codec !== codec && VIDEO[codec - 1]) {
    try {
      p.vdec.configure({ codec: VIDEO[codec - 1] });
      Object.assign(p, { codec, key: false });
    } catch { return; }
  }
  if (p.codec !== codec || !(p.key ||= kind === 2)) return;
  const type = kind === 2 ? "key" : "delta";
  p.vdec.decode(new EncodedVideoChunk({ type, timestamp, data }));
}
