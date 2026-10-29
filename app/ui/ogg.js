// Ogg Opus (RFC 7845), both ways: voice messages any player opens.

const CRC = new Uint32Array(256).map((_, i) => {
  let c = i << 24;
  for (let k = 0; k < 8; k++) c = c & 0x80000000 ? (c << 1) ^ 0x04c11db7 : c << 1;
  return c >>> 0;
});
const crc = (b) => b.reduce((c, x) => ((c << 8) ^ CRC[(c >>> 24) ^ x]) >>> 0, 0);

const SERIAL = 0x69726368; // "irch"
const PRESKIP = 312; // libopus at 48 kHz

function page(packets, seq, granule, flags) {
  const lace = packets.flatMap((p) =>
    [...Array(Math.floor(p.length / 255)).fill(255), p.length % 255]);
  const b = new Uint8Array(27 + lace.length +
    packets.reduce((n, p) => n + p.length, 0));
  const v = new DataView(b.buffer);
  b.set([0x4f, 0x67, 0x67, 0x53, 0, flags]);
  v.setBigUint64(6, BigInt(granule), true);
  v.setUint32(14, SERIAL, true);
  v.setUint32(18, seq, true);
  b[26] = lace.length;
  b.set(lace, 27);
  let at = 27 + lace.length;
  for (const p of packets) b.set(p, (at += p.length) - p.length);
  v.setUint32(22, crc(b), true);
  return b;
}

const text = (s) => new TextEncoder().encode(s);

/** Opus packets of 20 ms, mono, 48 kHz, as an .ogg file. */
export function ogg(packets) {
  const head = new Uint8Array(19);
  head.set(text("OpusHead"));
  const v = new DataView(head.buffer);
  head[8] = 1; // version
  head[9] = 1; // channels
  v.setUint16(10, PRESKIP, true);
  v.setUint32(12, 48000, true);
  const tags = new Uint8Array([...text("OpusTags"), 4, 0, 0, 0,
    ...text("irch"), 0, 0, 0, 0]);
  const out = [page([head], 0, 0, 2), page([tags], 1, 0, 0)];
  // At most 50 packets (a second) a page: few headers, short waits.
  for (let i = 0, seq = 2; i < packets.length || seq === 2; i += 50, seq++) {
    const some = packets.slice(i, i + 50);
    const last = i + 50 >= packets.length;
    const granule = (i + some.length) * 960;
    out.push(page(some, seq, granule, last ? 4 : 0));
  }
  return new Blob(out, { type: "audio/ogg" });
}

/** Its Opus packets and channels, or null if it is not Ogg Opus. */
export function unogg(b) {
  const packets = [];
  let part = [];
  for (let at = 0; at + 27 <= b.length;) {
    if (b[at] !== 0x4f || b[at + 1] !== 0x67) return null;
    const n = b[at + 26];
    let data = at + 27 + n;
    for (let i = 0; i < n; i++) {
      const len = b[at + 27 + i];
      part.push(b.subarray(data, data + len));
      data += len;
      if (len < 255) {
        const p = new Uint8Array(part.reduce((k, x) => k + x.length, 0));
        part.reduce((k, x) => (p.set(x, k), k + x.length), 0);
        packets.push(p);
        part = [];
      }
    }
    at = data;
  }
  const head = packets.shift();
  if (!head || new TextDecoder().decode(head.subarray(0, 8)) !== "OpusHead") {
    return null;
  }
  packets.shift(); // OpusTags
  const skip = new DataView(head.buffer, head.byteOffset).getUint16(10, true);
  return { channels: head[9], skip, packets };
}
