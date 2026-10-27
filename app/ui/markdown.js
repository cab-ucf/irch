// Tokens, never HTML strings: user content only ever becomes text nodes.

const md = globalThis.markdownit({ html: false, linkify: true, breaks: true })
  .disable("image");

// Pandoc's rule, so "$5 and $6" stays text: no space inside the dollars,
// no digit right after the closing one.
md.inline.ruler.after("escape", "math", (state, silent) => {
  const { src, pos } = state;
  if (src[pos] !== "$") return false;
  const d = src.startsWith("$$", pos) ? "$$" : "$";
  const from = pos + d.length;
  const end = src.indexOf(d, from);
  if (end <= from || /\d/.test(src[end + d.length] ?? "")) return false;
  if (d === "$" && (/\s/.test(src[from]) || /\s/.test(src[end - 1]))) {
    return false;
  }
  if (!silent) {
    const t = state.push("math", "", 0);
    [t.content, t.markup] = [src.slice(from, end), d];
  }
  state.pos = end + d.length;
  return true;
});

// markdown-it's "text" rule, stopping at "|" too, so ||spoilers|| parse.
md.inline.ruler.at("text", (state, silent) => {
  let pos = state.pos;
  const stop = /[\n!#$%&*+\-:<=>@[\\\]^_`{}~|]/;
  while (pos < state.posMax && !stop.test(state.src[pos])) pos++;
  if (pos === state.pos) return false;
  if (!silent) state.pending += state.src.slice(state.pos, pos);
  state.pos = pos;
  return true;
});

md.inline.ruler.after("escape", "spoiler", (state, silent) => {
  const { src, pos } = state;
  if (!src.startsWith("||", pos)) return false;
  const end = src.indexOf("||", pos + 2);
  if (end <= pos + 2) return false;
  if (!silent) {
    state.push("spoiler_open", "span", 1);
    const max = state.posMax;
    [state.pos, state.posMax] = [pos + 2, end];
    state.md.inline.tokenize(state);
    state.posMax = max;
    state.push("spoiler_close", "span", -1);
  }
  state.pos = end + 2;
  return true;
});

/** Block tokens, with inline ones as `children` (markdown-it's shape). */
export const markdown = (src) => md.parse(String(src), {});

/** Inline tokens only: a reply's quoted first line. */
export const inline = (src) => md.parseInline(String(src), {})[0].children;
