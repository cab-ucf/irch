// Discord-flavoured markdown as a token tree. Pure: no DOM, no HTML strings,
// so it is unit-testable and rendering can only ever create text nodes.
//
// Blocks: ```code```, > quote, plain lines.
// Inline: `code`, ||spoiler||, **bold**, __underline__, *italic*/_italic_,
//         ~~strike~~, http(s) links, @mentions.

const INLINE = [
  ["code", /`([^`\n]+)`/],
  ["spoiler", /\|\|(.+?)\|\|/],
  ["b", /\*\*(.+?)\*\*/],
  ["u", /__(.+?)__/],
  ["i", /\*([^*\s](?:[^*]*[^*\s])?)\*|\b_([^_\s](?:[^_]*[^_\s])?)_\b/],
  ["s", /~~(.+?)~~/],
  ["link", /https?:\/\/[^\s<>]*[^\s<>.,:;"')\]!?]/],
  ["mention", /(?<![\p{L}\p{N}_])@[\p{L}\p{N}_.-]*[\p{L}\p{N}_]/u],
];

/** Inline tokens: {t:"text"|"code"|"link"|"mention", v} or
 *  {t:"b"|"i"|"u"|"s"|"spoiler", c:[...]}. */
export function inline(text) {
  const out = [];
  while (text) {
    let best = null;
    for (const [t, re] of INLINE) {
      const m = re.exec(text);
      if (m && (!best || m.index < best.m.index)) best = { t, m };
    }
    if (!best) {
      out.push({ t: "text", v: text });
      break;
    }
    const { t, m } = best;
    if (m.index) out.push({ t: "text", v: text.slice(0, m.index) });
    const inner = m[1] ?? m[2];
    if (t === "code") out.push({ t, v: inner });
    else if (t === "link" || t === "mention") out.push({ t, v: m[0] });
    else out.push({ t, c: inline(inner) });
    text = text.slice(m.index + m[0].length);
  }
  return out;
}

/** Block tokens: {t:"pre", v} | {t:"quote", c} | {t:"line", c}. */
export function markdown(src) {
  const blocks = [];
  const lines = String(src).split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const one = /^```(.+)```$/.exec(line);
    if (one) blocks.push({ t: "pre", v: one[1] });
    else if (line.startsWith("```")) {
      const body = [];
      while (++i < lines.length && !lines[i].startsWith("```")) {
        body.push(lines[i]);
      }
      blocks.push({ t: "pre", v: body.join("\n") });
    } else if (line.startsWith("> ")) {
      blocks.push({ t: "quote", c: inline(line.slice(2)) });
    } else blocks.push({ t: "line", c: inline(line) });
  }
  return blocks;
}
