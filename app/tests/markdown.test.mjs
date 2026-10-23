// node --test app/tests/*.test.mjs
import { test } from "node:test";
import assert from "node:assert/strict";
import { inline, markdown } from "../ui/markdown.js";

const txt = (v) => ({ t: "text", v });

test("inline styles nest and leave the rest as text", () => {
  assert.deepEqual(inline("a **bold _it_** d"), [
    txt("a "),
    { t: "b", c: [txt("bold "), { t: "i", c: [txt("it")] }] },
    txt(" d"),
  ]);
  assert.deepEqual(inline("`**x**` ~~y~~ __z__ ||s||"), [
    { t: "code", v: "**x**" },
    txt(" "),
    { t: "s", c: [txt("y")] },
    txt(" "),
    { t: "u", c: [txt("z")] },
    txt(" "),
    { t: "spoiler", c: [txt("s")] },
  ]);
});

test("html is never interpreted, only carried as text", () => {
  assert.deepEqual(inline("<img src=x onerror=alert(1)>"), [txt("<img src=x onerror=alert(1)>")]);
});

test("links stop before trailing punctuation; mentions need a boundary", () => {
  assert.deepEqual(inline("see https://iroh.computer/docs."), [
    txt("see "),
    { t: "link", v: "https://iroh.computer/docs" },
    txt("."),
  ]);
  assert.deepEqual(inline("hi @alice, mail a@b.c"), [
    txt("hi "),
    { t: "mention", v: "@alice" },
    txt(", mail a@b.c"),
  ]);
});

test("snake_case words are not italic", () => {
  assert.deepEqual(inline("snake_case_name"), [txt("snake_case_name")]);
  assert.deepEqual(inline("_yes_"), [{ t: "i", c: [txt("yes")] }]);
});

test("blocks: fences (with language, single-line, unterminated) and quotes", () => {
  assert.deepEqual(markdown("```rust\nfn x() {}\n```\n> **q**\nend"), [
    { t: "pre", v: "fn x() {}" },
    { t: "quote", c: [{ t: "b", c: [txt("q")] }] },
    { t: "line", c: [txt("end")] },
  ]);
  assert.deepEqual(markdown("```a```"), [{ t: "pre", v: "a" }]);
  assert.deepEqual(markdown("```\nopen"), [{ t: "pre", v: "open" }]);
});
