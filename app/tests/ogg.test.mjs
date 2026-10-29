// node --test app/tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { ogg, unogg } from "../ui/ogg.js";

test("Opus packets go into Ogg and come back out whole", async () => {
  // Over 255 bytes, empty, and past a page's fifty.
  const packets = Array.from({ length: 123 },
    (_, i) => new Uint8Array(i % 7 === 0 ? 600 + i : i).fill(i));
  const b = new Uint8Array(await ogg(packets).arrayBuffer());
  assert.equal(new TextDecoder().decode(b.subarray(0, 4)), "OggS");
  const o = unogg(b);
  assert.deepEqual([o.channels, o.skip], [1, 312]);
  assert.deepEqual(o.packets, packets);
  assert.equal(unogg(new Uint8Array(40)), null, "not Ogg");
});
