//! The browser console's terminal emulation, driven by node.
//!
//! `page.rs` ships the page as one inline string, including the terminal
//! emulator a terminal tab needs — and a string of JS cannot be unit-tested by
//! Rust. So this file is the test: the Rust side
//! ([`crate::web::page::the_terminal_emulation_and_the_tab_are_tested_through_node`])
//! splits this file at PAGE_SCRIPT, splices the page's own `<script>` in, and
//! runs the result through node. What is tested is therefore the code as it
//! ships, in the scope it ships in: a renamed function, a key map keyed by the
//! wrong name, or a sequence that stops parsing fails here rather than in a
//! browser nobody has open.
//!
//! The DOM is a hand-built fake, deliberately tiny: enough that the page's
//! script can boot (it builds its controls, makes the console tab and tries to
//! connect on load) and that a terminal tab can be wired, typed into, resized,
//! fed a binary output frame and closed. The geometry is faked to exact numbers
//! — an 800x480 pane, a cell measured at 8x16 — so a test can say the shell has
//! to be told 100x30 instead of trusting whatever font the machine has.
//!
//! Everything above the splice runs *before* the page's script, so the page's
//! `connect()` finds the fake socket; everything below runs after, so the
//! assertions can reach the page's own top-level bindings. The two halves share
//! one scope with it, which is the point: names are looked up in the shipped
//! page or the test does not compile.

"use strict";
/* The page's own <script>, extracted from INDEX_HTML and run in node against a
   fake DOM, so the terminal code is tested as it ships rather than as a copy.
   Written by hand here; the real driver is the Rust test
   `the_terminal_page_code_is_tested_through_node`, which extracts the script
   from the built page and runs it through this file. */

/* ---- the smallest DOM that lets the page's script boot ------------------- */
const seen = { sent: [], selected: [], focus: [] };
let selection = "";

function node(name, cls, id) {
  const n = {
    nodeName: (name || "div").toUpperCase(),
    id: id || "",
    className: cls || "",
    textContent: "",
    innerHTML: "",
    title: "",
    value: "",
    dataset: {},
    style: { setProperty() {}, removeProperty() {} },
    classList: {
      _s: new Set((cls || "").split(" ")),
      add(c) { this._s.add(c); },
      remove(c) { this._s.delete(c); },
      contains(c) { return this._s.has(c); },
      toggle(c, on) { if (on) this._s.add(c); else this._s.delete(c); },
    },
    children: [],
    listeners: {},
    clientWidth: 800, clientHeight: 480,
    offsetWidth: 800, offsetHeight: 480,
    scrollWidth: 640, scrollHeight: 128,     /* 80x8 by 8x16: a cell is 8x16 */
    scrollTop: 0, scrollLeft: 0,
    childElementCount: 0,
    appendChild(c) { this.children.push(c); return c; },
    append(...c) { this.children.push(...c); },
    /* A child inserted *after* this node, as the DOM's `after()` does — with
       the parent pointer, so `remove()` below really detaches it and a test can
       count what is left. */
    after(c) {
      c.parentNode = this;
      this.children.push(c);
    },
    remove() {
      const p = this.parentNode;
      if (!p) return;
      const i = p.children.indexOf(this);
      if (i >= 0) p.children.splice(i, 1);
      this.parentNode = null;
    },
    querySelector(sel) { return this.querySelectorImpl ? this.querySelectorImpl(sel) : node("div"); },
    querySelectorAll() { return []; },
    addEventListener(type, fn) { (this.listeners[type] = this.listeners[type] || []).push(fn); },
    removeEventListener() {},
    insertAdjacentHTML(where, html) { this.innerHTML += html; this.childElementCount++; },
    focus() { seen.focus.push(this); },
    getContext: undefined,
    /* The pane's own geometry, as the fit code reads it. */
    getBoundingClientRect() { return { width: this.clientWidth, height: this.clientHeight }; },
  };
  return n;
}

const elById = {};
function getEl(id) {
  if (!elById[id]) {
    elById[id] = node("div", "", id);
    elById[id].appendChild = function (c) { this.children.push(c); return c; };
  }
  return elById[id];
}

/* A terminal panel is not a generic node: it has a head, a grid and a size
   span, and the pane itself is what keystrokes and geometry belong to. */
function termPanel() {
  const grid = node("pre", "term-grid");
  const size = node("span", "term-size");
  size.scrollWidth = 640; size.scrollHeight = 144;   /* 80 Ms of 8px; 9 lines of 16 */
  const out = node("div", "term-out");
  out.clientWidth = 800; out.clientHeight = 480;
  out.querySelectorImpl = (sel) =>
    sel === ".term-grid" ? grid : sel === ".term-size" ? size : out;
  const head = node("div", "term-head");
  const headParts = { ".dir": node("span", "dir"), ".shell": node("span", "shell"), ".spacer": node("span", "spacer") };
  head.querySelector = (sel) => headParts[sel] || node("button");
  /* The hidden field the page types into: a real element with focus, a value
     and its own listeners, because the whole point of it is that the browser
     hands keystrokes — and IME compositions — to *it*, not to the pane. */
  const ta = node("textarea", "term-input");
  ta.value = "";
  ta.hasFocus = false;
  ta.focus = function () { this.hasFocus = true; seen.focus.push(this); };
  ta.blur = function () { this.hasFocus = false; };
  const panel = node("div", "panel");
  const parts = { ".term-out": out, ".term-grid": grid, ".term-size": size,
                  ".term-head": head, ".term-input": ta };
  panel.querySelector = (sel) => parts[sel] || node("button");
  panel.listeners = {};
  panel.addEventListener = function (type, fn) { (this.listeners[type] = this.listeners[type] || []).push(fn); };
  return { panel, out, grid, size, head, headParts, ta };
}

globalThis.document = {
  documentElement: { style: { setProperty() {} } },
  getElementById: getEl,
  createElement: (name) => node(name),
  querySelectorAll: () => [],
  addEventListener: () => {},
};
globalThis.location = { protocol: "http:", host: "localhost:0" };
globalThis.localStorage = { getItem: () => null, setItem: () => {}, removeItem: () => {} };
globalThis.getComputedStyle = () => ({ paddingLeft: "0px", paddingRight: "0px", paddingTop: "0px", paddingBottom: "0px" });
globalThis.requestAnimationFrame = (fn) => { fn(); return 0; };
globalThis.addEventListener = () => {};
globalThis.getSelection = () => ({ toString: () => selection });
globalThis.window = globalThis;
globalThis.window.getSelection = globalThis.getSelection;
globalThis.window.addEventListener = () => {};
globalThis.window.clipboardData = null;
/* No ResizeObserver: the page then falls back to its own poll, which a test can
   call directly instead of waiting on one. */
globalThis.ResizeObserver = undefined;

class FakeSocket {
  constructor(url) { FakeSocket.last = this; this.url = url; this.readyState = 0; this.sent = []; }
  send(s) { this.sent.push(s); seen.sent.push(s); }
  close() { this.readyState = 3; }
  addEventListener() {}
}
FakeSocket.OPEN = 1;
globalThis.WebSocket = FakeSocket;

globalThis.btoa = (bin) => Buffer.from(bin, "binary").toString("base64");
globalThis.atob = (b64) => Buffer.from(b64, "base64").toString("binary");


/* ------------------------------------------------------------------ splice
   The page's own <script> goes on the next line, inserted by the Rust test:
   above it the fakes, below it the assertions, and all of it one scope. */
@@PAGE_SCRIPT@@

/* ---- run the page's own script ------------------------------------------- */

/* ---- assertions ---------------------------------------------------------- */
let pass = 0, fail = 0;
function ok(cond, name, extra) {
  if (cond) { pass++; return; }
  fail++;
  console.log("FAIL " + name + (extra === undefined ? "" : "\n  " + extra));
}
function eq(got, want, name) {
  ok(got === want, name, "got " + JSON.stringify(got) + ", want " + JSON.stringify(want));
}

/* The page's own live socket, so a test can watch what it sends. */
const live = () => {
  if (ws && ws.readyState === 1) return ws;
  ws = { readyState: 1, sent: [], send(s) { this.sent.push(s); seen.sent.push(s); }, close() {} };
  return ws;
};

/* ---- 1. the emulator, through the page's TermScreen ---------------------- */
const row = (t, r) => t.lines[r].ch.join("").replace(/\s+$/, "");
let t = new TermScreen(10, 4);
t.write("hello");
eq(row(t, 0), "hello", "plain text lands on the first row");
t = new TermScreen(5, 3);
t.write("abcdefgh");
eq(t.text(), "abcde\nfgh", "a line wraps at the last column");
t = new TermScreen(20, 4);
t.write("abc\rdef");
eq(row(t, 0), "def", "carriage return overwrites");
t = new TermScreen(20, 4);
t.write("abc\b\bX");
eq(row(t, 0), "aXc", "backspace steps back");
t = new TermScreen(20, 4);
t.write("0123456789\x1b[2D\x1b[K");
eq(row(t, 0), "01234567", "erase to end of line, which is how readline redraws");
t = new TermScreen(10, 3);
t.write("aaaa\r\nbbbb\r\ncccc\x1b[3;1H\x1b[1J");
eq(t.text(), "\n\n ccc", "clear from the top up takes the cursor cell too, as xterm does");
t = new TermScreen(10, 3);
t.write("aaaa\r\nbbbb\r\ncccc\x1b[2;3H\x1b[0J");
eq(t.text(), "aaaa\nbb", "clear from the cursor down");
t = new TermScreen(20, 3);
t.write("one\r\ntwo\r\nthree\r\nfour");
eq(t.text(), "two\nthree\nfour", "a line feed at the bottom scrolls the screen");
t = new TermScreen(20, 6);
t.write("\x1b[3;5HX");
eq(row(t, 2)[4], "X", "cursor addressing");
t.write("\x1b[100C\x1b[100B");
eq(t.x + ":" + t.y, "19:5", "cursor movement clamps instead of running off");
t = new TermScreen(10, 4);
t.write("a\r\nb\r\nc\x1b[2;1H\x1b[1L");
eq(t.text(), "a\n\nb\nc", "insert line");
t = new TermScreen(10, 4);
t.write("a\r\nb\r\nc\r\nd\x1b[2;1H\x1b[1M");
eq(t.text(), "a\nc\nd", "delete line");
t = new TermScreen(20, 5);
t.write("\x1b[2;4r\x1b[4;1HX\r\nY");
eq(row(t, 2), "X", "a scroll region scrolls inside itself");
eq(row(t, 0), "", "and leaves the rest of the screen alone");
t = new TermScreen(20, 3);
t.write("\x1b[31mred\x1b[0m normal");
eq(t.lines[0].at[0], "fc31", "colour is remembered per cell");
eq(t.lines[0].at[6], "", "and reset clears it");
t = new TermScreen(20, 3);
t.write("\x1b[38;2;255;128;0mX");
eq(t.lines[0].at[0], "f#ff8000", "truecolour");
t = new TermScreen(20, 3);
t.write("\x1b[38;5;196mX");
ok(t.render().includes("color:#ff0000"), "a 256-colour index is drawn as a colour");

t = new TermScreen(20, 4);
t.write("ab\x1b[6n");
eq(t.takeOutput(), "\x1b[1;3R", "a cursor position request is answered");
t = new TermScreen(20, 4);
t.write("\x1b[c");
ok(t.takeOutput().startsWith("\x1b[?"), "a device attributes request is answered");
t = new TermScreen(20, 4);
t.write("\x1b[?10;13645$p");
eq(t.takeOutput(), "\x1b[?10;0$y", "a mode request is answered not-set (fish asks)");
t = new TermScreen(20, 4);
t.write("\x1b]10;?\x07");
ok(t.takeOutput().startsWith("\x1b]10;rgb:"), "a colour request is answered");

t = new TermScreen(20, 4);
t.write("main\x1b[?1049h\x1b[2Jalt\x1b[?1049l");
eq(t.text(), "main", "leaving the alternate screen comes back to what was there");
t = new TermScreen(20, 4);
t.write("main\x1b[?1049h\x1b[2Jalt");
eq(t.text(), "alt", "and the alternate screen is what shows while it runs");
t = new TermScreen(20, 4);
t.write("\x1b[?1h");
eq(t.decckm, true, "application cursor keys are remembered");
t = new TermScreen(20, 4);
t.write("\x1b[?2004h");
eq(t.bracketed, true, "bracketed paste is remembered");

t = new TermScreen(20, 4);
t.write("\x1b[31");
t.write("mX");
eq(t.lines[0].at[0], "fc31", "a sequence split across two chunks still parses");
t = new TermScreen(20, 4);
t.write("keep\x1b]0;title is not drawn\x07rest");
eq(t.text(), "keeprest", "a window title never reaches the screen");
t = new TermScreen(20, 4);
t.write("a\x1bPqm:DCS data\x1b\\b");
eq(t.text(), "ab", "a device control string is swallowed");
t = new TermScreen(20, 4);
t.write("x\x00y\x07z");
eq(t.text(), "xyz", "NUL and BEL are not drawn");
t = new TermScreen(20, 4);
t.write("\x1b(B a\x1b(0lqqk\x1b(B b");
ok(row(t, 0).includes("\u250C\u2500\u2500\u2510"), "the line-drawing set is drawn as lines");

t = new TermScreen(20, 3);
t.write("\u0e21\u0e48");
eq(row(t, 0), "\u0e21\u0e48", "a Thai tone mark stays on its consonant");
eq(t.x, 1, "and takes no cell of its own");
t = new TermScreen(4, 3);
t.write("abcd\u0e49");
eq(row(t, 0), "abcd\u0e49", "a mark after a full line joins the cell under the cursor");

t = new TermScreen(20, 4);
t.write("hello\r\nsecond");
t.resize(30, 6);
eq(t.text(), "hello\nsecond", "resizing keeps what was on screen");
eq(t.render().split("\n").length, 6, "and the rendered screen is the new height");

t = new TermScreen(40, 4);
t.write("<script>alert(1)</script>\x1b[31m<img src=x onerror=y>\x1b[0m");
ok(!t.render().includes("<script"), "output is never markup");
ok(!t.render().includes("onerror"), "not even inside a colour");
ok(!/\x1b/.test(t.render()), "no escape bytes survive into the HTML");

/* ---- 2. a real terminal tab, through the page's own code ----------------- */
const { panel, out, grid, size, head, headParts } = termPanel();
const tab = { id: "T1", kind: "term", path: null, el: panel, tab: node("div", "tab"), term: null };
tab.tab.querySelector = () => node("span", "name");
wireTerm(tab);
eq(typeof tab.term, "object", "wiring a terminal tab gives it a shell state");
ok(tab.term.screen instanceof TermScreen, "with a screen of its own");

/* The server's answer to term_open, as the page would have received it. */
termTabs.set("t1", tab);
tab.term.session = "t1";
const key = (k, extra) => Object.assign({
  key: k, ctrlKey: false, altKey: false, metaKey: false, shiftKey: false,
  isComposing: false, charCode: 0, preventDefault() { this.defaultPrevented = true; },
}, extra || {});

/* Keystrokes reach the shell as the bytes a terminal would put on the wire. */
function type(ev) {
  const s = live();
  s.sent.length = 0;
  for (const fn of tab.term.input.listeners.keydown) fn(ev);
  const msg = s.sent.map((m) => JSON.parse(m)).find((m) => m.type === "term_in");
  return msg ? Buffer.from(msg.data, "base64").toString("utf8") : null;
}
/* What the invisible field's `input` event sends — the printable path, which
   is the only one that can carry a composed or dead-key character. */
function input(ch) {
  const s = live();
  s.sent.length = 0;
  tab.term.input.value = ch;
  for (const fn of tab.term.input.listeners.input) fn({ data: ch });
  const msg = s.sent.map((m) => JSON.parse(m)).find((m) => m.type === "term_in");
  return msg ? Buffer.from(msg.data, "base64").toString("utf8") : null;
}
eq(type(key("a")), null, "a printable key is not sent from keydown (input does that)");
eq(input("a"), "a", "a printable character is sent as itself, once");
eq(type(key("Enter")), "\r", "Enter is a carriage return, which is what a cooked tty wants");
eq(type(key("Backspace")), "\x7f", "Backspace is DEL");
eq(type(key("Escape")), "\x1b", "Escape is ESC");
eq(type(key("c", { ctrlKey: true })), "\x03", "Ctrl-C is the control byte, so SIGINT happens in the pty");
eq(type(key("d", { ctrlKey: true })), "\x04", "Ctrl-D likewise");
eq(type(key("ArrowUp")), "\x1b[A", "an arrow is CSI A");
eq(type(key("ArrowRight", { shiftKey: true })), "\x1b[1;2C", "with the modifier encoded");
eq(type(key("PageDown")), "\x1b[6~", "a tildes key");
tab.term.screen.decckm = true;
eq(type(key("ArrowUp")), "\x1bOA", "and application cursor keys while a program asks for them (vim)");
tab.term.screen.decckm = false;
eq(type(key("1", { altKey: true })), null, "Alt-1 belongs to the page's tab switch, not to the shell");
eq(type(key("f", { altKey: true })), "\x1bf", "Alt plus a letter is escape-then-letter (M-x)");
eq(type(key("a", { metaKey: true })), null, "a Meta chord is the browser's");
eq(type(key("F5")), null, "and an unmapped key sends nothing");
eq(type(key("ก", { isComposing: true })), null, "a key composing Thai is not sent as a keystroke");
selection = "copied text";
eq(type(key("c", { ctrlKey: true })), null, "Ctrl-C with a selection is a copy, not an interrupt");
selection = "";

/* The IME case: a Thai word chosen from a candidate list. keydown says
   `Process` and sends nothing; the committed string arrives on `input`. */
eq(type(key("Process", { isComposing: true })), null, "a key mid-composition sends nothing");
eq(type(key("Enter", { isComposing: true })), null, "not even Enter, which would run half a command");
eq(input("\u0e21\u0e48\u0e21"), "\u0e21\u0e48\u0e21", "the committed Thai word is sent as UTF-8");
eq(tab.term.input.value, "", "and the field is left empty, so the next key starts clean");

/* Paste, in and out of bracketed mode. */
function paste(text) {
  const s = live(); s.sent.length = 0;
  const ev = {
    preventDefault() {}, clipboardData: { getData: () => text },
  };
  for (const fn of tab.term.input.listeners.paste) fn(ev);
  return s.sent.map((m) => JSON.parse(m)).filter((m) => m.type === "term_in")
    .map((m) => Buffer.from(m.data, "base64").toString("utf8")).join("");
}
eq(paste("ls\n"), "ls\n", "a paste is typed");
tab.term.screen.bracketed = true;
eq(paste("ls\n"), "\x1b[200~ls\n\x1b[201~", "and bracketed when the shell asked for it");
tab.term.screen.bracketed = false;

/* A paste the server would refuse is sent in pieces instead. */
const huge = "x".repeat(64 * 1024 + 10);
const parts = paste(huge);
eq(parts.length, huge.length, "a long paste keeps every byte");
eq(live().sent.filter((m) => JSON.parse(m).type === "term_in").length, 2, "in two messages");

/* Output, as the server sends it: a binary frame naming its shell. */
function frameFor(id, bytes) {
  const b = Buffer.from(bytes, "binary");
  const a = Buffer.alloc(1 + id.length + b.length);
  a[0] = id.length;
  Buffer.from(id).copy(a, 1);
  b.copy(a, 1 + id.length);
  return a.buffer.slice(a.byteOffset, a.byteOffset + a.length);
}
termFrame(frameFor("t1", "hello\r\n"));
ok(grid.innerHTML.includes("hello"), "output is drawn on the pane");
/* A query the shell made has to find its way back down the pty. */
termFrame(frameFor("t1", "\x1b[6n"));
/* The answer is queued rather than sent inline (a frame can carry several
   queries, and one message per reply would be silly), so the harness runs the
   queue the way the browser would run its timer. */
live().sent.length = 0;
termFlush(tab);
const back = live().sent.map((m) => JSON.parse(m)).filter((m) => m.type === "term_in");
ok(back.length >= 1, "a terminal query is answered back to the shell");
const reply = back.map((m) => Buffer.from(m.data, "base64").toString("utf8")).join("");
ok(/^\x1b\[[0-9]+;[0-9]+R$/.test(reply), "with a real cursor report", JSON.stringify(reply.slice(0, 40)));
eq(tab.term.pending, "", "and the queue is empty afterwards");
live().sent.length = 0;
termFrame(frameFor("nope", "x"));
eq(live().sent.length, 0, "a frame for a shell this page does not have is dropped");

/* The pane's size, measured from its own pixels, is what the shell is told. */
live().sent.length = 0;
termFit(tab, true);
const resize = live().sent.map((m) => JSON.parse(m)).filter((m) => m.type === "term_resize").pop();
ok(!!resize, "the pane's size is sent to the shell");
eq(resize.cols, 100, "columns from the pane's width and the font's cell width");
eq(resize.rows, 30, "rows from the pane's height and the line height");
eq(tab.term.cols + ":" + tab.term.rows, "100:30", "and remembered as what the shell was told");
const before = live().sent.length;
termFit(tab);
eq(live().sent.length, before, "an unchanged pane does not resize the shell again");

/* The shell going away is said out loud, not left as a silent pane. */
termDead(tab, "shell exited (1)");
eq(tab.term.dead, true, "a dead tab does not accept input");
eq(type(key("a")), null, "keystrokes go nowhere");
eq(paste("ls"), "", "a paste goes nowhere");
ok(headParts[".spacer"].children.length > 0, "the tab says the shell is gone");
ok(tab.tab.classList.contains("dead"), "and the tab strip marks it");
ok(tab.term.input.value === "" && !tab.term.input.hasFocus, "and the pane gives up focus");
ok(String(headParts[".spacer"].children[0].textContent).includes("Enter"),
   "the pane says how to get another shell");

/* Enter in a dead pane: another shell, in the same tab. */
tab.term.input.value = "";
let opened = 0;
const realRpc = rpc;
rpc = async (obj) => { if (obj.type === "term_open") opened++; return realRpc(obj); };
eq(type(key("a")), null, "a dead pane ignores typing");
eq(type(key("Enter")), null, "but Enter asks for another shell");
eq(opened, 1, "one new shell was asked for");
eq(tab.term.dead, false, "and the tab is alive again");
eq(tab.tab.classList.contains("dead"), false, "the tab strip stopped marking it");
eq(headParts[".spacer"].children.length, 0, "and the note is gone");
rpc = realRpc;

termClose(tab);
ok(!termTabs.has("t1"), "closing the tab takes the shell's entry with it");

/* ---- 3. the file view's syntax highlighting ------------------------------ */

/* The one property a highlighter may never break: the pane still holds the
   file. Every token is a slice of the input, in order, with nothing dropped,
   added or reordered — so a select-and-copy out of the tab gives you the file,
   and the colours cannot hide a line from you. Checked by concatenating the
   tokens back and by stripping the markup off the rendered HTML. */
const HL_SAMPLES = {
  rust: "// a comment\nuse std::fmt;\n#[derive(Debug)]\npub struct Thing<T> { n: u32 }\n" +
        "impl Thing {\n    /// doc /* nested */ still doc\n    pub fn new() -> Self { Self { n: 0x1F_ff } }\n}\n" +
        "const MAX_LEN: usize = 1_000;\nlet s = \"a \\\"quoted\\\" string\";\n" +
        "let r = r#\"{\"raw\": \"not closed\"}\"#;\nlet c = 'x';\nmatch x { Some(v) => v, None => 0 }\n",
  python: "#!/usr/bin/env python3\nimport os\n@decorator\ndef f(a, b=1):\n" +
          "    \"\"\"doc\n    line two\"\"\"\n    s = 'it\\'s'\n    t = \"don't\"\n" +
          "    x = 0xff + 1.5e-3\n    return {'k': v} if a else None  # trailing\n",
  js: ["/* head */", "import { x } from \"./y.js\";", "const re = /a[b/c]d/;",
       "const t = `tpl ${x + 1} tail`;", "class A extends B { constructor() { super(); } }",
       "if (a === \"s\") f(1, 2);", ""].join("\n"),
  json: "{\n  \"name\": \"jonnyq\",\n  \"deps\": { \"a\": 1, \"b\": true },\n  \"list\": [null, 1.5e3]\n}\n",
  yaml: "# comment\n---\napp:\n  name: \"jonnyq\"\n  on: true\n  ref: &anchor\n  other: *anchor\n...",
  toml: "[package]\nname = \"jonnyq\"   # trailing\n[profile.release]\nstrip = true\n",
  sh: "#!/bin/sh\n# a comment\nfor f in *.rs; do\n  grep -n --color=auto \"$PAT\" \"$f\" | head -3\ndone\n" +
      "if [ -x \"$HOME/bin\" ]; then export PATH=$HOME/bin:$PATH; fi\n",
  md: ["# Title", "", "some *text* here, don't colour the rest", "", "- one", "- two", "",
       "> quoted", "", "inline `code` here", "", "[link](http://x.y)", ""].join("\n"),
  html: "<!DOCTYPE html>\n<html lang=\"en\">\n<head><title>a & b</title></head>\n" +
        "<body class=\"x\" data-n=3></body>\n</html>\n",
  css: "/* gruvbox dark */\n:root { --bg0h: #1d2021; --fg: #ebdbb2; }\n" +
       ".file-out {\n  white-space: pre;\n  font-family: ui-monospace, monospace;\n}\n" +
       "@media (max-width: 600px) { body { display: none } }\n",
  c: "#include <stdio.h>\n/* a comment */\nstatic int f(const char *s) { return printf(\"%s\\n\", s); }\n",
  go: "package main\n\nimport \"fmt\"\n\nfunc main() { // hi\n\tconst s = \"tab\\there\"\n\tfmt.Println(s, 0x10)\n}\n",
  sql: "-- list\nSELECT a, b FROM t WHERE x = 'lit' AND y IS NOT NULL;\n",
  ini: "[unit]\nDescription=jonnyq\nExecStart=/usr/bin/jonnyq -web 8080 ; comment\n",
  env: "# comment\nexport KEY=\"value with spaces\"   # trailing\nOTHER=${KEY:-fallback}\n",
  docker: "FROM rust:1.83 AS build\nRUN cargo build --release   # comment\n" +
          "ENV A=1 B=2\nENTRYPOINT [\"jonnyq\"]\n",
};
for (const [lang, src] of Object.entries(HL_SAMPLES)) {
  const toks = hlTok(src, HL_LANG[lang]);
  eq(toks.map((x) => x[1]).join(""), src, lang + ": the tokens are the file, in order");
  const html = hlRender(src, HL_LANG[lang]);
  eq(html.replace(/<\/?span[^>]*>/g, ""), esc(src), lang + ": the pane's text is the file, escaped");
  ok(!/<[a-zA-Z][^>]*>/.test(html) || !/<[a-zA-Z][^>]*>/.test(esc(src)),
     lang + ": nothing in the file becomes markup");
}

/* Which language a file is read as: the name first, then a `#!` line, and
   otherwise nothing at all — colouring a file by a wrong guess is worse than
   leaving it plain. */
eq(hlLangOf("main.rs", ""), "rust", "a .rs file is Rust");
eq(hlLangOf("Cargo.lock", ""), "yaml", "and Cargo.lock is listed by name");
eq(hlLangOf("Cargo.toml", ""), "toml", "and .toml by extension");
eq(hlLangOf(".env", ""), "env", "a dotfile's extension is what follows the dot");
eq(hlLangOf("Dockerfile", ""), "docker", "a nameless file is known by its name");
eq(hlLangOf("no_such_ext.abcd", ""), "", "an unknown extension is not guessed at");
eq(hlLangOf("notes.txt", ""), "", "and a plain text file is knowingly plain");
eq(hlLangOf("build", "#!/bin/sh\nls\n"), "sh", "a shebang decides for an extensionless file");
eq(hlLangOf("tool", "#!/usr/bin/env python3\n"), "python", "even through `env`");

/* The roles that carry meaning, as the classes the CSS colours. */
const hlc = (src, lang, needle) => {
  const hit = hlTok(src, HL_LANG[lang]).find((x) => x[1] === needle);
  return hit ? hit[0] : "(none)";
};
eq(hlc("// go", "rust", "// go"), "s-com", "a line comment is a comment");
eq(hlc("/* a /* b */ c */", "rust", "/* a /* b */ c */"), "s-com",
   "a nested block comment closes at its own close marker");
eq(hlc('r#"a "b" c"#', "rust", 'r#"a "b" c"#'), "s-str",
   "a raw string holds the quotes inside it");
eq(hlc("fn", "rust", "fn"), "s-kw", "a reserved word is a keyword");
eq(hlc("Gruvbox", "rust", "Gruvbox"), "s-type", "a Capitalised name is a type");
eq(hlc("MAX_LEN", "rust", "MAX_LEN"), "s-const", "a SCREAMING name is a constant");
eq(hlc("usize", "rust", "usize"), "s-type", "a lowercase primitive type is a type");
eq(hlc("println", "rust", "println"), "", "a bare name is nothing special");
eq(hlc('println!("x")', "rust", "println"), "s-fn",
   "a bang-call is a call too, which is how Rust's macros are written");
eq(hlc('  println("x")', "rust", "println"), "s-fn", "a name before a call is a call");
eq(hlc("0x1F_ff", "rust", "0x1F_ff"), "s-num", "a hex literal is a number");
eq(hlc('"""doc"""', "python", '"""doc"""'), "s-str", "a triple-quoted string is one string");
eq(hlc("#!/bin/sh", "sh", "#!/bin/sh"), "s-com", "a shebang is a comment");
eq(hlc('"unclosed', "python", '"unclosed'), "s-str",
   "an unterminated string is one token and stops at the line");
eq(hlc("--force", "sh", "--force"), "s-var", "a long flag");
eq(hlc("name:", "yaml", "name"), "s-key", "an unquoted YAML key is a key");
eq(hlc('"name":', "json", '"name"'), "s-key", "a JSON key is a key");
eq(hlc('"value"', "json", '"value"'), "s-str", "and a JSON value is a string");
eq(hlc("[package]", "toml", "[package]"), "s-type", "a TOML section header");
eq(hlc('features = ["regex"]', "toml", '"regex"'), "s-str",
   "but a bracket mid-line is an array value, not a section header");
eq(hlc("[package]", "toml", "package"), "(none)",
   "and the header is one token, not a name that can be mistaken for a key");
eq(hlc("strip = true", "toml", "strip"), "s-key", "a TOML key");
eq(hlc('tokio = { version = "1", features = ["rt"] }', "toml", "version"), "s-const",
   "a key inside an inline table is drawn as a value, as gruvbox draws it");
eq(hlc('tokio = { version = "1", features = ["rt"] }', "toml", "tokio"), "s-key",
   "while the key that starts the line stays a key");
eq(hlc('KEY="v"', "env", "KEY"), "s-key", "an env file's variable name");
eq(hlc("FROM", "docker", "FROM"), "s-kw", "a Dockerfile instruction");
eq(hlc("SELECT", "sql", "SELECT"), "s-kw", "SQL keywords, however they are cased");
eq(hlc("select", "sql", "select"), "s-kw", "in both cases");
eq(hlc("#1d2021", "css", "#1d2021"), "s-pre", "a CSS hex colour");
eq(hlc("@media", "css", "@media"), "s-pre", "an at-rule");
eq(hlc("<html lang=\"en\">", "html", "html"), "s-tag", "a markup tag");
eq(hlc("<html lang=\"en\">", "html", "lang"), "s-attr", "and its attributes");
eq(hlc("# Title", "md", "# Title"), "s-kw", "a Markdown heading");
eq(hlc("> quoted", "md", "> quoted"), "s-com", "a block quote");
eq(hlc("see the notes (below)", "md", "notes"), "",
   "a word before a bracket in prose is not a function call");

/* A README's fences are the likeliest thing to fool the scanner: they are long
   runs of backticks with no closing pair, in a file full of apostrophes. */
{
  const md = "text\n\n```rust\nlet s = \"unclosed;\n```\n\nmore text\n";
  const toks = hlTok(md, HL_LANG.md);
  const fence = toks.find((x) => x[0] === "s-code");
  ok(fence !== undefined, "a fenced block is one token");
  eq(fence[1], "```rust\nlet s = \"unclosed;\n```", "and it runs fence to fence");
  const after = toks.slice(toks.indexOf(fence) + 1).map((x) => x[1]).join("");
  eq(after, "\n\nmore text\n", "the text after the closing fence is text again");
  ok(!toks.some((x) => x[0] === "s-str"), "nothing inside it is mistaken for a string");
}
{
  /* An odd number of fences: the block runs to the end of the file, which is
     what an unclosed fence is, and does not leave the pane half-coloured. */
  const md = "a\n```\nx\n```\nb\n```\nc\n";
  const toks = hlTok(md, HL_LANG.md);
  const code = toks.filter((x) => x[0] === "s-code").map((x) => x[1]).join("|");
  eq(code, "```\nx\n```|```\nc\n", "two fences pair and a third takes the rest");
}
{
  /* A fence of three or more, closed by a longer run, and inline spans still
     work when they are not at the start of a line. */
  const md = "````\nhas ``` inside\n````\nand `code` here\n";
  const toks = hlTok(md, HL_LANG.md);
  eq(toks.find((x) => x[0] === "s-code")[1], "````\nhas ``` inside\n````",
     "a longer fence closes only on a run at least as long");
  ok(toks.some((x) => x[0] === "s-str" && x[1] === "`code`"),
     "and an inline span mid-line is still a span");
}

/* The bug that makes a naive highlighter untrustworthy: one apostrophe in a
   sentence of prose must not colour everything under it. */
ok(!hlTok(HL_SAMPLES.md, HL_LANG.md).some((x) => x[0] === "s-str" && x[1].includes("colour the rest")),
   "an apostrophe in prose colours nothing");
eq(hlTok("/* never closed\nmore\ntext", HL_LANG.c).length, 1,
   "an unclosed block comment runs to the end of the file, which is what it is");

/* A script tag in a file is text, in every language including HTML. */
ok(!/<script/.test(hlRender("const s = \"<script>alert(1)</script>\";", HL_LANG.js)),
   "a script tag inside a string is not markup");
{
  /* The one property that matters, checked over every language: the pane's HTML
     may contain no markup except the spans the highlighter emitted, whatever the
     file contains. A quote, a bracket or a comment marker that could break out
     of a span would show up here. */
  const hostile = "a<evil> \"q\" <script>alert(1)</script> `x` 'y' <!-- c --> [[b]] [l](h\"t) \\\" /* o */";
  for (const name of Object.keys(HL_LANG)) {
    const h = hlRender(hostile, HL_LANG[name]);
    const bare = h.replace(/<\/?span(?: class="s-[a-z]+")?>/g, "");
    ok(!bare.includes("<") && !bare.includes(">"), name + ": only the highlighter's own spans are markup");
    const back = bare.replace(/&lt;/g, "<").replace(/&gt;/g, ">").replace(/&amp;/g, "&");
    eq(back, hostile, name + ": and the text is the file's");
  }
}
{
  const h = hlRender("<b>x</b>", HL_LANG.html);
  ok(h.includes("&lt;") && h.includes("&gt;") && !/<b>/.test(h),
     "and in HTML mode the tag is escaped before it is wrapped");
}

/* The pane's own entry point: colour when there is a language and the file is
   small enough to read, plain text otherwise, and say which happened. */
function filePanel() {
  const out = node("pre", "file-out");
  const head = node("div", "file-head");
  const panel = node("div", "panel");
  panel.querySelector = (sel) => (sel === ".file-out" ? out : head);
  return { panel, out, head };
}
{
  const p = filePanel();
  const tab = { id: "F1", kind: "file", path: "src/main.rs", el: p.panel, tab: node("div", "tab") };
  eq(hlPaint(tab, "main.rs", HL_SAMPLES.rust), "rust", "a file view says what it read the file as");
  ok(p.out.innerHTML.includes("<span class=\"s-com\">"), "and draws the comment in its colour");
  ok(!p.out.innerHTML.includes('class=""'), "and never wraps nothing in a span");
  eq(p.out.innerHTML.replace(/<\/?span[^>]*>/g, ""), esc(HL_SAMPLES.rust),
     "and the pane still holds the file's text");

  const plain = filePanel();
  const t2 = { id: "F2", kind: "file", path: "notes.txt", el: plain.panel, tab: node("div", "tab") };
  eq(hlPaint(t2, "notes.txt", "a note, don't you know"), "",
     "a file with no rules of its own is not coloured");
  eq(plain.out.textContent, "a note, don't you know", "and is set as text, not as markup");

  /* Past the cap, a file is being searched rather than read, and building a
     span per token would freeze the tab to no purpose. */
  const big = filePanel();
  const t3 = { id: "F3", kind: "file", path: "big.rs", el: big.panel, tab: node("div", "tab") };
  /* Just over HL_MAX, and compared by length: a failure that prints the
     string would bury the rest of the run. */
  const hugeSrc = "let x = 1;\n".repeat(Math.floor(HL_MAX / 10) + 3);
  ok(hugeSrc.length > HL_MAX, "the sample really is over the cap");
  eq(hlPaint(t3, "big.rs", hugeSrc), "", "a file over the cap is not highlighted");
  eq(big.out.textContent.length, hugeSrc.length, "and arrives whole, as text");
}

/* Highlighting must be a pass over the file, not over itself: no input here
   makes it spin, and a file the size the server can send is coloured in the
   time a pane may be blocked for. */
{
  const t0 = Date.now();
  const many = HL_SAMPLES.rust.repeat(2000);
  hlRender(many, HL_LANG.rust);
  ok(Date.now() - t0 < 3000, "highlighting " + many.length + " characters took " +
     (Date.now() - t0) + "ms");
}

console.log(pass + " passed, " + fail + " failed");
process.exit(fail ? 1 : 0);
