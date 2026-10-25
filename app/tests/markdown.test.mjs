// node --test app/tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";

globalThis.markdownit = createRequire(import.meta.url)(
  "../ui/vendor/markdown-it.min.js");
const { inline, markdown } = await import("../ui/markdown.js");

const types = (src) => markdown(src).map((t) => t.type);
const kids = (src) => markdown(src).flatMap((t) => t.children ?? [])
  .map((t) => [t.type, t.content]);

test("blocks: headings, lists, quotes, tables, fences with a language", () => {
  assert.deepEqual(types("# T\n\n- a\n- b"), ["heading_open", "inline",
    "heading_close", "bullet_list_open", "list_item_open", "paragraph_open",
    "inline", "paragraph_close", "list_item_close", "list_item_open",
    "paragraph_open", "inline", "paragraph_close", "list_item_close",
    "bullet_list_close"]);
  const f = markdown("```rust\nfn main() {}\n```")[0];
  assert.deepEqual([f.type, f.info, f.content], ["fence", "rust",
    "fn main() {}\n"]);
  assert.ok(types("| a | b |\n|---|---|\n| 1 | 2 |").includes("table_open"));
  assert.ok(types("> said").includes("blockquote_open"));
});

test("html is never interpreted, only carried as text", () => {
  const tag = "<img src=x onerror=alert(1)>";
  assert.ok(!types(tag).some((t) => t.startsWith("html")));
  assert.deepEqual(kids(tag), [["text", tag]]);
  assert.ok(!types("![x](https://e.com/a.png)").includes("image"));
});

test("math: $inline$, $$display$$; prices stay text", () => {
  assert.deepEqual(kids("so $x^2$ and $$\\frac{a}{b}$$"), [["text", "so "],
    ["math", "x^2"], ["text", " and "], ["math", "\\frac{a}{b}"]]);
  assert.deepEqual(kids("$5 and $6"), [["text", "$5 and $6"]]);
  assert.deepEqual(kids("costs $5."), [["text", "costs $5."]]);
});

test("spoilers nest inline styles; links keep their target", () => {
  const seen = (src) => inline(src).filter((t) => t.type !== "text" ||
    t.content).map((t) => t.content || t.type);
  assert.deepEqual(seen("a ||**x**|| b"), ["a ", "spoiler_open",
    "strong_open", "x", "strong_close", "spoiler_close", " b"]);
  assert.deepEqual(seen("a | b || c"), ["a | b || c"]);
  const l = inline("see https://iroh.computer/docs.");
  assert.equal(l[1].attrGet("href"), "https://iroh.computer/docs");
  assert.equal(inline("[iroh](https://iroh.computer)")[0].attrGet("href"),
    "https://iroh.computer");
});
