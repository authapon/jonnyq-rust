//! The browser console's single page: HTML, CSS (gruvbox dark) and JS in one
//! file, served inline.
//!
//! Kept as one `const` string rather than `include_str!`ed asset files so the
//! whole web interface stays a single module and the binary needs no build
//! step or asset embedding. The page is deliberately dependency-free (no CDN,
//! no bundler): it must work offline, from a local model, on a machine with no
//! internet — which is most of the machines jonnyq runs on.
//!
//! The one exception is the Thai typefaces: the top bar offers nine of them
//! from Google Fonts. That stylesheet is *optional* in the strongest sense — it
//! is requested in a way that cannot block the first paint, so a machine with no
//! internet draws the page immediately and every family falls back to a locally
//! installed copy of the same name or to the browser's own sans-serif. Nothing
//! on the page waits for it and no script comes from it, so offline behaviour is
//! unchanged apart from which face the text is drawn in.
//!
//! Layout, per the feature:
//!
//! * centre: a tab strip (always a `console` tab, plus one per opened file)
//!   over the panel for the selected tab — the console is an ANSI-rendered
//!   transcript with the prompt textarea under it; a file tab is its text
//! * right: the working directory as a lazily expanded tree; clicking a file
//!   opens it in a new tab, clicking a non-text file pops up a refusal
//!
//! The prompt textarea is disabled unless the session says it is ready for a
//! prompt, and is resizable (`resize: vertical` plus a draggable splitter).

pub const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>jonnyq console</title>
<!-- The Thai typefaces, from Google Fonts. Optional by design, and fetched
     without blocking the paint: with `media="print"` the browser never waits on
     this stylesheet before drawing the page, so a machine with no internet (or
     one whose proxy blackholes the request) paints immediately and redraws when
     the faces arrive — or never, in which case every family below resolves to a
     locally installed copy of the same name, or to the browser's sans-serif.
     Nothing on the page waits for it, and nothing is executed from it. -->
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" media="print" onload="this.media='all'" href="https://fonts.googleapis.com/css2?family=Athiti:wght@400;700&amp;family=IBM+Plex+Sans+Thai+Looped:wght@400;700&amp;family=K2D:wght@400;700&amp;family=Kodchasan:wght@400;700&amp;family=Noto+Sans+Thai+Looped:wght@400;700&amp;family=Noto+Serif+Thai:wght@400;700&amp;family=Playpen+Sans+Thai:wght@400;700&amp;family=Thasadith:wght@400;700&amp;family=Trirong:wght@400;700&amp;display=swap">
<style>
/* gruvbox dark (predniam), the palette the terminal output is written in. */
:root {
  --bg0h: #1d2021; --bg0: #282828; --bg1: #3c3836; --bg2: #504945;
  --bg3: #665c54; --bg4: #7c6f64;
  --fg: #ebdbb2; --fg4: #a89984; --fg1: #bdae93;
  --red: #cc241d;    --green: #98971a;   --yellow: #d79921;  --blue: #458588;
  --purple: #b16286; --aqua: #689d6a;    --gray: #7c6f64;
  --f0: #fb4934;     --f2: #fabb2f;      --f3: #83a598;      --f4: #d3869b;
  --f6: #8ec07c;     --f7: #b8a288;      --f12: #928374;
  --accent: #fe8019;
  /* What the top bar's controls write: the face the console, the prompt box
     and the chrome are drawn in, and the text size every one of them uses.
     Both are overwritten from localStorage on boot, before the first paint. */
  --ui-font: "IBM Plex Sans Thai Looped", ui-sans-serif, system-ui, "Segoe UI", sans-serif;
  --ui-size: 14pt;
}
* { box-sizing: border-box; }
html, body { height: 100%; margin: 0; }
body {
  background: var(--bg0h); color: var(--fg);
  font: 14px/1.45 var(--ui-font);
  display: flex; flex-direction: column; overflow: hidden;
}
#top {
  display: flex; align-items: center; gap: 10px;
  padding: 6px 10px; background: var(--bg0); border-bottom: 1px solid var(--bg2);
  flex: 0 0 auto;
}
#top b { color: var(--accent); letter-spacing: .04em; }
/* The status line is the session's live ctx figure, so it gets the space a
   long model name cannot take from it: it shrinks and ellipsises instead of
   pushing the type controls and the connection light out of the bar. */
#status {
  color: var(--fg4); font-size: 12px; white-space: nowrap;
  flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis;
}
#type { display: flex; align-items: center; gap: 4px; flex: 0 0 auto; }
#type label { font-size: 11px; color: var(--fg4); }
#font, #size {
  background: var(--bg1); color: var(--fg1); border: 1px solid var(--bg2);
  border-radius: 3px; font: inherit; font-size: 12px; padding: 1px 3px; outline: none;
}
#font { max-width: 14em; }
#size { width: 4.5em; }
#font:focus, #size:focus { border-color: var(--bg4); }
#font:hover, #size:hover { border-color: var(--bg4); }
#type .reset {
  background: none; border: 1px solid var(--bg2); color: var(--fg4);
  border-radius: 3px; cursor: pointer; font: inherit; font-size: 11px; padding: 1px 6px;
}
#type .reset:hover { color: var(--accent); border-color: var(--accent); }
#conn { font-size: 12px; color: var(--fg4); white-space: nowrap; }
#conn.on { color: var(--green); } #conn.off { color: var(--f0); }
#main { flex: 1 1 auto; display: flex; min-height: 0; }
#centre { flex: 1 1 auto; display: flex; flex-direction: column; min-width: 0; min-height: 0; }

/* --- tabs --- */
#tabs {
  display: flex; align-items: flex-end; gap: 2px; padding: 4px 6px 0 6px;
  background: var(--bg0); border-bottom: 1px solid var(--bg2);
  overflow-x: auto; flex: 0 0 auto; scrollbar-width: thin;
}
.tab {
  display: flex; align-items: center; gap: 6px; padding: 4px 9px;
  background: var(--bg1); color: var(--fg1); border: 1px solid var(--bg2);
  border-bottom: none; border-radius: 4px 4px 0 0; cursor: pointer;
  white-space: nowrap; font-size: 13px; max-width: 22em;
}
.tab:hover { background: var(--bg2); }
.tab.active { background: var(--bg0h); color: var(--fg); border-color: var(--bg3); border-bottom: 1px solid var(--bg0h); margin-bottom: -1px; }
.tab .name { overflow: hidden; text-overflow: ellipsis; }
.tab .x { color: var(--fg4); padding: 0 2px; border-radius: 3px; }
.tab .x:hover { color: var(--f0); background: var(--bg3); }

/* --- panels --- */
#panels { flex: 1 1 auto; position: relative; min-height: 0; }
.panel { position: absolute; inset: 0; display: none; flex-direction: column; min-height: 0; }
.panel.active { display: flex; }
/* The console, the file views and the prompt box take their size — and, for
   the two the human reads as prose, their face — from the two properties the
   top bar's pickers write on :root. That is the whole mechanism: applyType()
   sets two properties and every text area follows, including a file tab opened
   later, with no per-element bookkeeping. */
#console-out, .file-out {
  flex: 1 1 auto; overflow: auto; margin: 0; padding: 10px 12px;
  background: var(--bg0h); white-space: pre-wrap; overflow-wrap: break-word;
  font-size: var(--ui-size); line-height: 1.5;
  tab-size: 4; scrollbar-width: thin; scrollbar-color: var(--bg3) var(--bg0h);
}
#console-out { font-family: var(--ui-font); }
/* A file tab shows source, and source is read by its columns: indentation that
   lines up, aligned `=` signs, side-by-side diffs. The faces the picker offers
   are proportional, so a file view keeps a monospaced face and takes only the
   size — the console keeps the chosen face because its text is prose. */
.file-out {
  white-space: pre;
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, "DejaVu Sans Mono", monospace;
}
.file-head {
  flex: 0 0 auto; padding: 4px 12px; background: var(--bg0);
  border-bottom: 1px solid var(--bg2); color: var(--fg4); font-size: 12px;
}
/* the console keeps its own scroll position when you tab away and back */
.console-wrap { flex: 1 1 auto; display: flex; flex-direction: column; min-height: 0; }
#prompt-box {
  flex: 0 0 auto; border-top: 1px solid var(--bg2); background: var(--bg0);
  padding: 6px 10px 8px 10px; display: flex; flex-direction: column; gap: 4px;
  min-height: 44px;
}
#prompt-hint { font-size: 12px; color: var(--fg4); display: flex; gap: 8px; align-items: baseline; }
#prompt-hint .mode { color: var(--f3); }
#splitter { flex: 0 0 auto; height: 5px; cursor: row-resize; background: var(--bg0); }
#splitter:hover { background: var(--bg2); }
#prompt {
  width: 100%; min-height: 3em; height: 5.5em; max-height: 70vh; resize: vertical;
  background: var(--bg1); color: var(--fg); border: 1px solid var(--bg2);
  border-radius: 4px; padding: 6px 8px; outline: none;
  font-family: var(--ui-font); font-size: var(--ui-size); line-height: 1.5;
  scrollbar-width: thin; scrollbar-color: var(--bg3) var(--bg1);
}
#prompt:focus { border-color: var(--bg4); }
#prompt:disabled { color: var(--fg4); background: var(--bg0); cursor: progress; }
#send {
  align-self: flex-end; background: var(--bg1); color: var(--fg); border: 1px solid var(--bg2);
  border-radius: 4px; padding: 3px 12px; font: inherit; font-size: 12px; cursor: pointer;
}
#send:hover:enabled { border-color: var(--accent); color: var(--accent); }
#send:disabled { color: var(--fg4); cursor: progress; }

/* --- ANSI colours, one class per SGR code so bold can pick the bright
       variant the way a terminal does --- */
.c0,.c9m { color: var(--fg1); }  .c1 { font-weight: bold; }
.c30 { color: var(--bg3); } .c90 { color: var(--f12); }
.c31 { color: var(--red); }    .c91 { color: var(--f0); }
.c32 { color: var(--green); }  .c92 { color: #b8bb26; }
.c33 { color: var(--yellow); } .c93 { color: var(--f2); }
.c34 { color: var(--blue); }   .c94 { color: var(--f3); }
.c35 { color: var(--purple); } .c95 { color: var(--f4); }
.c36 { color: var(--aqua); }   .c96 { color: var(--f6); }
.c37 { color: var(--fg4); }    .c97 { color: var(--fg); }
/* the "there is more above" line a trimmed console leaves behind — its own
   element so trimming never has to rewrite the spans around it */
.trim { color: var(--f12); font-style: italic; }

/* --- the file tree --- */
#side {
  flex: 0 0 19em; min-width: 12em; max-width: 50vw; display: flex;
  flex-direction: column; border-left: 1px solid var(--bg2); background: var(--bg0);
}
#side-head {
  padding: 5px 10px; font-size: 12px; color: var(--fg4);
  border-bottom: 1px solid var(--bg2); display: flex; align-items: center; gap: 8px;
}
#side-head .wd { color: var(--fg1); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
#side-head button {
  margin-left: auto; background: none; border: 1px solid var(--bg2); color: var(--fg4);
  border-radius: 3px; cursor: pointer; font: inherit; font-size: 11px; padding: 1px 6px;
}
#side-head button:hover { color: var(--accent); border-color: var(--accent); }
#tree { flex: 1 1 auto; overflow: auto; padding: 6px 4px 20px 4px; scrollbar-width: thin; }
.row { display: flex; align-items: center; gap: 4px; padding: 1px 4px; border-radius: 3px; cursor: pointer; white-space: nowrap; }
.row:hover { background: var(--bg1); }
.row.sel { background: var(--bg2); }
.row .tw { width: 1em; color: var(--f12); flex: 0 0 1em; text-align: center; }
.row .ic { flex: 0 0 1em; text-align: center; }
.row.dir .ic { color: var(--f3); }
.row.file .ic { color: var(--f7); }
.row .nm { overflow: hidden; text-overflow: ellipsis; }
.row.dir .nm { color: var(--f3); }
.row.err { color: var(--f0); }
.kids { margin-left: 1em; }

/* --- popup for files that cannot be shown --- */
#mask {
  position: fixed; inset: 0; background: rgba(29,32,33,.72);
  display: none; align-items: center; justify-content: center; z-index: 20;
}
#mask.show { display: flex; }
#popup {
  background: var(--bg0); border: 1px solid var(--bg3); border-radius: 6px;
  padding: 16px 18px; max-width: 34em; box-shadow: 0 6px 30px rgba(0,0,0,.6);
}
#popup h3 { margin: 0 0 8px 0; font-size: 14px; color: var(--f0); }
#popup pre { margin: 0; white-space: pre-wrap; overflow-wrap: anywhere; color: var(--fg1); font: inherit; }
#popup .ok {
  margin-top: 14px; background: var(--bg1); color: var(--fg); border: 1px solid var(--bg2);
  border-radius: 4px; padding: 4px 16px; font: inherit; cursor: pointer;
}
#popup .ok:hover { border-color: var(--accent); color: var(--accent); }

/* --- the ask_user popup: the browser's answer field ---
   The question the model is blocked on, the text to answer it with, and a
   Send button. It sits above the file popup (a higher z-index) because it is
   the one thing on this page that a running session is waiting for, and it is
   dismissed by the *server* the moment the terminal or ntfy answers first, so
   a tab never sits on a question jonnyq has already had. */
#ask-mask {
  position: fixed; inset: 0; background: rgba(29,32,33,.8);
  display: none; align-items: center; justify-content: center; z-index: 25;
  padding: 3vh 3vw;
}
#ask-mask.show { display: flex; }
#ask {
  background: var(--bg0); border: 1px solid var(--f3); border-radius: 6px;
  padding: 14px 16px 16px 16px; width: 100%; max-width: 44em;
  box-shadow: 0 6px 30px rgba(0,0,0,.6);
  display: flex; flex-direction: column; gap: 8px;
}
#ask h3 { margin: 0; font-size: 13px; color: var(--f3); letter-spacing: .02em; }
#ask-q {
  margin: 0; padding: 8px 10px; background: var(--bg0h); border: 1px solid var(--bg2);
  border-radius: 4px; color: var(--fg); white-space: pre-wrap; overflow-wrap: anywhere;
  font-family: var(--ui-font); font-size: var(--ui-size); line-height: 1.5;
  max-height: 40vh; overflow: auto; scrollbar-width: thin;
}
#ask-more { font-size: 12px; color: var(--f12); }
#ask-answer {
  width: 100%; min-height: 3em; height: 4.5em; max-height: 40vh; resize: vertical;
  background: var(--bg1); color: var(--fg); border: 1px solid var(--bg2);
  border-radius: 4px; padding: 6px 8px; outline: none;
  font-family: var(--ui-font); font-size: var(--ui-size); line-height: 1.5;
  scrollbar-width: thin; scrollbar-color: var(--bg3) var(--bg1);
}
#ask-answer:focus { border-color: var(--bg4); }
#ask-answer:disabled { color: var(--fg4); background: var(--bg0); cursor: progress; }
#ask-row { display: flex; align-items: center; gap: 10px; }
#ask-note { flex: 1 1 auto; min-width: 0; font-size: 12px; color: var(--f0); }
#ask-send {
  flex: 0 0 auto; background: var(--bg1); color: var(--fg); border: 1px solid var(--f3);
  border-radius: 4px; padding: 4px 14px; font: inherit; font-size: 13px; cursor: pointer;
}
#ask-send:hover:enabled { border-color: var(--accent); color: var(--accent); }
#ask-send:disabled { color: var(--fg4); cursor: progress; border-color: var(--bg2); }
</style>
</head>
<body>
<div id="top">
  <b>jonnyq</b>
  <span id="status">connecting&hellip;</span>
  <span id="type">
    <label for="font">font</label>
    <select id="font" title="typeface of the console and the prompt box (a file view stays monospaced so its columns line up)"></select>
    <label for="size">size</label>
    <input id="size" type="number" min="8" max="40" step="1" title="text size in pt, for the console, the file views and the prompt box" aria-label="text size in pt">
    <button class="reset" id="type-reset" title="back to IBM Plex Sans Thai Looped at 14pt">reset</button>
  </span>
  <span id="conn">offline</span>
</div>
<div id="main">
  <div id="centre">
    <div id="tabs"></div>
    <div id="panels"></div>
  </div>
  <div id="side">
    <div id="side-head"><span class="wd" id="wd">&hellip;</span><button id="refresh" title="reload the tree">refresh</button></div>
    <div id="tree"></div>
  </div>
</div>
<div id="mask"><div id="popup">
  <h3 id="popup-title"></h3><pre id="popup-body"></pre><button class="ok" id="popup-ok">OK</button>
</div></div>
<div id="ask-mask"><div id="ask" role="dialog" aria-modal="true" aria-labelledby="ask-title">
  <h3 id="ask-title">Question from the model</h3>
  <pre id="ask-q"></pre>
  <div id="ask-more"></div>
  <textarea id="ask-answer" spellcheck="false" wrap="soft"
    placeholder="Type your answer. Enter sends, Shift-Enter adds a newline."></textarea>
  <div id="ask-row">
    <span id="ask-note"></span>
    <button id="ask-send">Send answer</button>
  </div>
</div></div>
<script>
"use strict";
/* ------------------------------------------------------------------ state */
const $ = (id) => document.getElementById(id);
let ws = null;                 /* the one live socket                          */
let seq = 0;                   /* request ids for file/tree calls              */
const pending = new Map();     /* id -> resolve, awaiting a response          */
const tabs = [];               /* {id, name, kind, path, el, tab}             */
let active = null;             /* tab id                                        */
let ready = false;             /* the session will accept a prompt              */
let autoScroll = true;         /* follow the transcript unless scrolled up      */
let consoleTab = null;
let treeRefresh = null;        /* the listing in flight, so two are never asked for */

/* ------------------------------------------------------------ typography
   Nine Thai faces from Google Fonts, in the order the user asked for them,
   with IBM Plex Sans Thai Looped as the default. The value is the CSS family
   name: if the stylesheet above never arrived (no internet) the name still
   resolves to a locally installed copy of the same face, and otherwise to the
   generic sans-serif the body falls back to — choosing a font is never a
   network operation.

   Both settings live in localStorage, so a reload or a second tab keeps what
   was picked, and they are written to two custom properties on :root rather
   than to each element: one write moves the console, the prompt box, every
   file tab and the chrome, including tabs opened later. A file view takes the
   size only — it names a monospaced face for itself so source keeps its
   columns. */
const FONTS = [
  "IBM Plex Sans Thai Looped",
  "Playpen Sans Thai",
  "K2D",
  "Kodchasan",
  "Noto Serif Thai",
  "Noto Sans Thai Looped",
  "Athiti",
  "Thasadith",
  "Trirong",
];
const DEFAULT_FONT = "IBM Plex Sans Thai Looped";
const DEFAULT_SIZE = 14;                  /* pt */
const SIZE_MIN = 8, SIZE_MAX = 40;
const LS = "jonnyq.type";                /* the one localStorage key */

function clampSize(v) {
  const n = Math.round(parseFloat(v));
  if (!Number.isFinite(n)) return DEFAULT_SIZE;
  return Math.min(SIZE_MAX, Math.max(SIZE_MIN, n));
}

function storedType() {
  try {
    const raw = localStorage.getItem(LS);
    if (!raw) return null;
    const v = JSON.parse(raw);
    const font = FONTS.includes(v && v.font) ? v.font : DEFAULT_FONT;
    return { font, size: clampSize(v && v.size) };
  } catch { return null; }    /* private mode, or a hand-edited value */
}

function storeType(t) {
  try { localStorage.setItem(LS, JSON.stringify(t)); } catch {}
}

let typeface = storedType() || { font: DEFAULT_FONT, size: DEFAULT_SIZE };

function applyType() {
  const r = document.documentElement.style;
  /* Quote the family name: "Noto Serif Thai" is three words, and an unquoted
     multi-word family in a custom property does not match. */
  r.setProperty("--ui-font", '"' + typeface.font.replace(/"/g, "") + '", ui-sans-serif, system-ui, sans-serif');
  r.setProperty("--ui-size", typeface.size + "pt");
}

function syncTypeControls() {
  $("font").value = typeface.font;
  $("size").value = typeface.size;
}

function buildTypeControls() {
  const sel = $("font");
  for (const f of FONTS) {
    const o = document.createElement("option");
    o.value = f;
    o.textContent = f;
    /* Preview the face in its own option, so the list is a menu of what the
       console will look like rather than a list of names to guess at. */
    o.style.fontFamily = '"' + f + '", sans-serif';
    sel.appendChild(o);
  }
  sel.addEventListener("change", () => {
    typeface = { font: FONTS.includes(sel.value) ? sel.value : DEFAULT_FONT, size: typeface.size };
    applyType(); syncTypeControls(); storeType(typeface);
  });
  const box = $("size");
  const commit = () => {
    /* The number input is trusted only after clamping, and the field is then
       rewritten to what was actually applied, so "3" cannot sit in a box that
       is drawing 8pt. */
    typeface = { font: typeface.font, size: clampSize(box.value) };
    applyType(); syncTypeControls(); storeType(typeface);
  };
  box.addEventListener("change", commit);
  box.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); commit(); } });
  $("type-reset").addEventListener("click", () => {
    typeface = { font: DEFAULT_FONT, size: DEFAULT_SIZE };
    applyType(); syncTypeControls(); storeType(typeface);
  });
  applyType();
  syncTypeControls();
}

/* ------------------------------------------------------------------ utils */
const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));

/* A rejection here is usually a plain string ("jonnyq is not connected yet"),
   sometimes an Error from a refused path; the popup must read like a sentence
   either way, not like "Error: ..." or "[object Object]". */
const why = (e) => (e && e.message ? e.message : String(e));

function popup(title, body) {
  $("popup-title").textContent = title;
  $("popup-body").textContent = body;
  $("mask").classList.add("show");
}
$("popup-ok").onclick = () => $("mask").classList.remove("show");
$("mask").onclick = (e) => { if (e.target === $("mask")) $("mask").classList.remove("show"); };
document.addEventListener("keydown", (e) => { if (e.key === "Escape") $("mask").classList.remove("show"); });

/* ------------------------------------------------------- ask_user popup
   The browser's half of `ask_user`: the question the model is blocked on, an
   answer field, and a Send button. The server owns the lifecycle: it opens the
   popup (`ask`) and closes it (`ask_close`) the moment the question is answered
   in a tab, at the terminal, or over ntfy, or when the turn that asked it is
   cancelled. So a tab can never sit on a question jonnyq has already had, and a
   reload gets the question back from the greeting instead of losing it.

   Questions queue (a cancelled turn can leave an earlier one open): only the
   front one is popped up, the rest are counted above the answer field, and the
   popup hands itself over as each is answered. Nothing on the page hides or
   dismisses it: the popup is the question jonnyq is blocked on, and it used to
   have a "hide" button (Escape did the same) that took the answer field off
   screen and left the human with no way back to it. Only the server's
   `ask_close` takes it away. */
const asks = [];                 /* [{id, question}] awaiting an answer, front first */
const askDrafts = new Map();     /* question id -> text typed but not sent           */
let askBusy = false;             /* an answer is in flight, so no double send         */
let askedByUs = null;            /* the id this tab sent, so its close says "sent"    */

function renderAsk() {
  const q = asks[0] || null;
  /* The popup is on screen exactly while a question is unanswered. There is no
     "hidden" flag any more - it was the thing that stranded a question in a tab
     whose answer field had been tucked away. */
  $("ask-mask").classList.toggle("show", !!q);
  if (!q) return;
  $("ask-q").textContent = q.question;
  const behind = asks.length - 1;
  $("ask-more").textContent = behind
    ? behind + (behind === 1 ? " more question behind this one" : " more questions behind this one")
    : "";
  const ta = $("ask-answer");
  ta.value = askDrafts.get(q.id) || "";
  ta.disabled = askBusy;
  $("ask-send").disabled = askBusy;
  if (document.activeElement !== ta) ta.focus();
}

/* The line beside Send: the answer goes over the socket and jonnyq may be
   mid-turn, so the popup reports what happened instead of guessing. */
function askNote(text, color) {
  const n = $("ask-note");
  n.textContent = text || "";
  n.style.color = color || "var(--f0)";
}

/* Every `ask` message: a question nobody has answered yet, or the same one
   re-sent to a tab that reloaded (the greeting replays what is pending). */
function onAskMessage(m) {
  if (!m.id) return;
  const known = asks.find((a) => a.id === m.id);
  if (known) known.question = m.question || "";
  else asks.push({ id: m.id, question: m.question || "" });
  askNote("");
  /* Queued behind another question: it is counted, not popped up. A question
     re-sent for this tab (a reload, or the front one having just been answered)
     is shown again, because the human has not seen it for a while. */
  if (m.open === false && asks[0].id !== m.id) { renderAsk(); return; }
  renderAsk();
}

function onAskClose(m) {
  const i = asks.findIndex((a) => a.id === m.id);
  const ours = askedByUs === m.id;
  askedByUs = null;
  if (i >= 0) asks.splice(i, 1);
  askDrafts.delete(m.id);
  if (i === 0) {
    /* A popup that disappears without this tab having sent anything was
       answered at the keyboard or over ntfy - say so rather than leaving a
       human staring at a box that vanished for no visible reason. Our own
       answer already left "sent" in that spot, so leave that alone. */
    if (!ours) askNote(asks.length ? "" : "answered elsewhere", "var(--f12)");
  }
  renderAsk();
}

function sendAsk() {
  const q = asks[0];
  if (!q || askBusy) return;
  const ta = $("ask-answer");
  askedByUs = q.id;
  askBusy = true;
  ta.disabled = true;
  $("ask-send").disabled = true;
  askNote("sending\u2026", "var(--fg4)");
  rpc({ type: "answer", ask: q.id, text: ta.value }).then((m) => {
    askBusy = false;
    if (m.ok) {
      /* jonnyq takes the answer on the socket and then broadcasts `ask_close`,
         which is what takes the popup away. */
      askDrafts.delete(q.id);
      askNote("sent", "var(--green)");
      return;
    }
    askedByUs = null;
    askNote(m.error || "jonnyq did not accept the answer");
    ta.disabled = false;
    $("ask-send").disabled = false;
  }, (e) => {
    askedByUs = null;
    askBusy = false;
    askNote("the answer was not sent: " + why(e));
    ta.disabled = false;
    $("ask-send").disabled = false;
    ta.focus();
  });
}

function wireAsk() {
  const ta = $("ask-answer");
  ta.addEventListener("keydown", (e) => {
    /* Enter sends, Shift/Ctrl-Enter is a newline (the textarea edits itself),
       and the IME guard matters for Thai input, where Enter commits a
       composition. */
    if (e.key !== "Enter" || e.isComposing) return;
    if (e.shiftKey || e.ctrlKey || e.metaKey) return;
    e.preventDefault();
    sendAsk();
  });
  ta.addEventListener("input", () => { if (asks[0]) askDrafts.set(asks[0].id, ta.value); });
  $("ask-send").addEventListener("click", sendAsk);
  /* Nothing here hides or closes the box, and no key does either. It holds the
     question jonnyq is blocked on, and every way of tucking it away was a way
     of losing the answer field with it: the server's `ask_close` is the only
     thing that takes the popup off screen. (Escape still dismisses the *file*
     popup, which is handled up with that popup.) */
}

/* ------------------------------------------------------------ ANSI -> HTML
   The console mirror carries the same escape sequences the terminal got, so
   the browser has to interpret the SGR ones: fg colours (30-37, bright
   90-97), bold (which selects the bright variant of a colour, as gruvbox
   schemes do) and reset. Cursor/erase sequences are dropped rather than
   emulated — the transcript is append-only, and the line editor's in-place
   redraws would need a screen model to be worth the trouble.
   Everything is escaped before it is wrapped, so output the model printed can
   never inject markup into this page. */
const SGR = /\x1b\[([0-9;]*)m/g;

/* The attributes a terminal would still be holding when the next byte arrives.
   Output is mirrored in chunks, so a chunk routinely ends with a colour still
   switched on (streamed answer text does this on every token); rendering each
   chunk as if it started in default colour is what makes text go grey
   mid-sentence. The state therefore lives outside the function, and is cleared
   only by a reset sequence or by the server's `reset`. */
let sgrOpen = [];

function applySgr(codes) {
  for (const c of codes) {
    if (c === 0) sgrOpen = [];
    else if (c === 1) { if (!sgrOpen.includes("c1")) sgrOpen.push("c1"); }
    else if (c === 22) sgrOpen = sgrOpen.filter((k) => k !== "c1");
    else if ((c >= 30 && c <= 37) || (c >= 90 && c <= 97)) {
      sgrOpen = sgrOpen.filter((k) => !/^c(9?[0-9]m?|3[0-7])$/.test(k) || k === "c1");
      sgrOpen.push("c" + c);
    } else if (c === 39) sgrOpen = sgrOpen.filter((k) => !/^c[39]/.test(k) || k === "c1");
  }
}

function ansiToHtml(text) {
  let out = "", at = 0, m;
  SGR.lastIndex = 0;
  while ((m = SGR.exec(text)) !== null) {
    out += span(sgrOpen, text.slice(at, m.index));
    at = SGR.lastIndex;
    applySgr((m[1] === "" ? ["0"] : m[1].split(";")).map((c) => (c === "" ? 0 : +c)));
  }
  out += span(sgrOpen, text.slice(at));
  return out;
}
function span(classes, text) {
  const body = esc(text);
  if (!body) return "";
  const keep = classes.filter((c) => c !== "c0");
  return keep.length ? '<span class="' + keep.join(" ") + '">' + body + "</span>" : body;
}

/* ---------------------------------------------------------------- tabs */
function makeTab(name, kind, path) {
  const id = "t" + (++seq);
  const tab = document.createElement("div");
  tab.className = "tab";
  tab.dataset.id = id;
  tab.innerHTML = '<span class="name"></span>' + (kind === "file" ? '<span class="x" title="close">&times;</span>' : "");
  tab.querySelector(".name").textContent = name;
  const panel = document.createElement("div");
  panel.className = "panel";
  panel.dataset.id = id;
  if (kind === "console") {
    panel.innerHTML =
      '<div class="console-wrap"><pre id="console-out"></pre></div>' +
      '<div id="splitter" title="drag to resize"></div>' +
      '<div id="prompt-box">' +
      '<div id="prompt-hint"><span class="mode"></span><span id="prompt-note"></span></div>' +
      '<textarea id="prompt" spellcheck="false" wrap="soft"></textarea>' +
      '<button id="send">Send (Enter)</button></div>';
  } else {
    /* A file view keeps its own monospaced face (source is read by its
       columns) and takes only the chosen size — the CSS on .file-out says so,
       so nothing here has to know about the picker. */
    panel.innerHTML = '<div class="file-head"></div><pre class="file-out"></pre>';
  }
  $("tabs").appendChild(tab);
  $("panels").appendChild(panel);
  const t = { id, name, kind, path, el: panel, tab };
  tabs.push(t);
  tab.addEventListener("click", (e) => {
    if (e.target.classList.contains("x")) { closeTab(t); return; }
    select(t.id);
  });
  if (kind === "console") wireConsole(panel);
  else t.el.querySelector(".file-out").addEventListener("wheel", (e) => {
    /* horizontal scroll with shift, like the terminal panes people expect */
    if (e.shiftKey) { t.el.querySelector(".file-out").scrollLeft += e.deltaY; e.preventDefault(); }
  });
  return t;
}

function closeTab(t) {
  if (t.kind === "console") return;           /* the console always stays */
  const i = tabs.indexOf(t);
  if (i >= 0) tabs.splice(i, 1);
  t.tab.remove();
  t.el.remove();
  if (active === t.id) select(tabs.length ? tabs[Math.max(0, i - 1)].id : null);
  treeSelection();
}

function select(id) {
  active = id;
  for (const t of tabs) {
    t.tab.classList.toggle("active", t.id === id);
    t.el.classList.toggle("active", t.id === id);
  }
  const t = byId(id);
  if (t && t.kind === "console") { autoScroll = true; scrollToBottom(); }
  treeSelection();
}
const byId = (id) => tabs.find((t) => t.id === id) || null;

/* ------------------------------------------------------------ console */
function consoleOut() { return consoleTab ? consoleTab.el.querySelector("#console-out") : null; }
function scrollToBottom() { const o = consoleOut(); if (o) o.scrollTop = o.scrollHeight; }

function appendConsole(text) {
  const out = consoleOut();
  if (!out) return;
  out.insertAdjacentHTML("beforeend", ansiToHtml(text));
  /* Truncate from the front once the transcript gets silly: the same
     conversation is in the log file and, for a session that has been running
     for a day, keeping all of it in the DOM would eat the tab.
     Which is also why this removes nodes rather than rebuilding: assigning the
     whole transcript back as text would replace every colour span with one
     plain text node and turn the console grey in a single statement — and a
     reload of a long session is exactly when the limit is crossed, so that
     shortcut was a second way to lose the theme. */
  consoleChars += text.length;
  if (out.childElementCount > TRIM_AT || consoleChars > TRIM_CHARS) trimConsole(out);
  if (autoScroll) scrollToBottom();
}

const TRIM_AT = 6000, TRIM_KEEP = 4000, TRIM_CHARS = 1_500_000;
let trimMark = null;
/* Running total of what has been appended, so the size test above stays O(1).
   Reading `out.textContent.length` on every chunk is a walk of the whole
   transcript, and streamed output calls this once per token. Re-measured on the
   rare occasion a trim happens, and zeroed when the console is emptied. */
let consoleChars = 0;

function trimConsole(out) {
  /* The mark is kept: it is the console's way of saying the conversation above
     this line exists and lives in the log file. Trimming it away on the second
     pass would leave a transcript that looks like it starts from nothing. */
  while (out.childElementCount > TRIM_KEEP) {
    const victim = out.firstChild === trimMark ? out.firstChild.nextSibling : out.firstChild;
    if (!victim) break;
    out.removeChild(victim);
  }
  if (!trimMark) {
    trimMark = document.createElement("div");
    trimMark.className = "trim";
    trimMark.textContent = "[... earlier output trimmed; it is still in the transcript log ...]";
  }
  if (out.firstChild !== trimMark) out.insertBefore(trimMark, out.firstChild);
  consoleChars = out.textContent.length;
}

function wireConsole(panel) {
  const out = panel.querySelector("#console-out");
  out.addEventListener("scroll", () => {
    autoScroll = out.scrollTop + out.clientHeight >= out.scrollHeight - 30;
  });
  const ta = panel.querySelector("#prompt");
  const btn = panel.querySelector("#send");
  ta.addEventListener("keydown", (e) => {
    /* Enter sends, Shift-Enter (and Ctrl-Enter) is a newline. The IME guard
       matters for Thai/Japanese input, where Enter commits a composition. */
    if (e.key !== "Enter" || e.isComposing) return;
    if (e.shiftKey || e.ctrlKey || e.metaKey) {
      e.preventDefault();
      const s = ta.selectionStart, en = ta.selectionEnd, v = ta.value;
      ta.value = v.slice(0, s) + "\n" + v.slice(en);
      ta.selectionStart = ta.selectionEnd = s + 1;
      return;
    }
    e.preventDefault();
    sendPrompt();
  });
  btn.addEventListener("click", sendPrompt);
  makeResizable(panel);
}

function sendPrompt() {
  const t = consoleTab; if (!t) return;
  const ta = t.el.querySelector("#prompt");
  const text = ta.value;
  if (!text.trim()) return;
  if (!ready) { popup("jonnyq is busy", "The session is not waiting for a prompt right now - wait for the turn in progress to finish (or press Ctrl-C in the terminal to cancel it)."); return; }
  /* The box clears straight away so a second Enter cannot send the same text
     twice, and the answer comes back as an ack (`ok`, or why it was refused).
     A prompt that never reached the socket - the link dropped in the moment
     between - is put back rather than lost, but only if the box is still empty,
     so nothing the human typed since is ever overwritten. */
  const draft = text;
  ta.value = "";
  rpc({ type: "prompt", text: draft }).then((m) => {
    if (m.ok) return;
    popup("The prompt was not queued", m.error || "jonnyq did not accept it");
    if (!ta.value.trim()) ta.value = draft;
  }, (e) => {
    popup("The prompt was not queued", why(e));
    if (!ta.value.trim()) ta.value = draft;
  });
}

function setReady(state, note) {
  ready = !!state;
  const t = consoleTab; if (!t) return;
  const ta = t.el.querySelector("#prompt");
  const btn = t.el.querySelector("#send");
  const noteEl = t.el.querySelector("#prompt-note");
  ta.disabled = !ready;
  ta.placeholder = ready ? "Type a prompt, or a /command. Enter sends, Shift-Enter adds a newline."
                        : "not ready - jonnyq is working";
  btn.disabled = !ready;
  noteEl.textContent = note || (ready ? "" : "waiting for the current turn to finish");
  if (ready) { noteEl.textContent = note || ""; if (document.activeElement !== ta) ta.focus(); }
  else if (note) noteEl.textContent = note;
}

/* Drag the bar above the textarea, or use its own resize handle: both change
   the same height, and the height survives a tab switch because it lives on
   the textarea element. */
function makeResizable(panel) {
  const bar = panel.querySelector("#splitter");
  const ta = panel.querySelector("#prompt");
  let dragging = false, startY = 0, startH = 0;
  bar.addEventListener("mousedown", (e) => {
    dragging = true; startY = e.clientY; startH = ta.offsetHeight;
    e.preventDefault();
  });
  window.addEventListener("mousemove", (e) => {
    if (!dragging) return;
    const h = Math.min(window.innerHeight * 0.8, Math.max(2.2 * parseFloat(getComputedStyle(ta).lineHeight || "20"), startH - (e.clientY - startY)));
    ta.style.height = h + "px";
  });
  window.addEventListener("mouseup", () => { dragging = false; });
}

/* -------------------------------------------------------------- the tree */
function treeSelection() {
  const t = byId(active);
  document.querySelectorAll("#tree .row").forEach((r) => {
    r.classList.toggle("sel", !!(t && t.kind === "file" && r.dataset.p === t.path));
  });
}

function renderTree(node, parent) {
  const box = document.createElement("div");
  box.className = "kids";
  for (const item of node.entries || []) {
    const row = document.createElement("div");
    row.className = "row " + (item.dir ? "dir" : "file");
    row.dataset.p = item.path;
    const tw = document.createElement("span"); tw.className = "tw";
    tw.textContent = item.dir ? "\u25B8" : "";           /* ▸ */
    const ic = document.createElement("span"); ic.className = "ic";
    ic.textContent = item.dir ? "\uD83D\uDCC1" : "\uD83D\uDCC4";   /* 📁 📄 */
    const nm = document.createElement("span"); nm.className = "nm";
    nm.textContent = item.name;
    if (item.dir && item.error) { nm.textContent = item.name + " (" + item.error + ")"; row.classList.add("err"); }
    row.append(tw, ic, nm);
    box.appendChild(row);
    if (!item.dir) {
      row.addEventListener("click", () => openFile(item.path, item.name));
      continue;
    }
    const kids = document.createElement("div");
    kids.className = "kids";
    kids.style.display = "none";
    box.appendChild(kids);
    let loaded = !!item.children;
    if (loaded) kids.appendChild(nodeEl(item.children));
    row.addEventListener("click", () => {
      const open = kids.style.display !== "none";
      if (open) { kids.style.display = "none"; tw.textContent = "\u25B8"; return; }
      kids.style.display = "";
      tw.textContent = "\u25BE";                                            /* ▾ */
      if (loaded) return;
      loaded = true;
      requestTree(item.path).then((n) => { kids.textContent = ""; kids.appendChild(nodeEl(n)); })
        .catch((e) => { kids.style.display = "none"; tw.textContent = "\u25B8"; loaded = false; popup("Cannot list " + item.path, why(e)); });
    });
  }
  if (!node.entries || !node.entries.length) {
    const e = document.createElement("div");
    e.className = "row"; e.style.color = "var(--fg4)";
    e.textContent = "(empty)";
    box.appendChild(e);
  }
  if (parent) parent.appendChild(box);
  return box;
}
const nodeEl = (n) => renderTree(n, null);

async function refreshTree() {
  /* One refresh at a time: boot, the greeting's `reset` and the refresh button
     all ask for the same thing, and two parallel listings of the same folder
     only make the second answer's renderer win at random. */
  if (treeRefresh) return treeRefresh;
  treeRefresh = (async () => {
    try {
      const root = await requestTree("");
      const tree = $("tree");
      tree.textContent = "";
      const box = renderTree(root, null);
      box.classList.remove("kids");
      box.style.marginLeft = "0";
      tree.appendChild(box);
      treeSelection();
    } catch (e) {
      popup("Cannot list the working directory", why(e));
    } finally {
      treeRefresh = null;
    }
  })();
  return treeRefresh;
}
$("refresh").addEventListener("click", refreshTree);

/* Opening a file: one request; the server decides whether it is text. A
   binary/image/PDF answer comes back as an error and is shown in the popup,
   which is what the feature asks for ("cannot be viewed"). */
async function openFile(path, name) {
  let res;
  try { res = await rpc({ type: "file", path }); }
  catch (e) { popup("Cannot open file", why(e)); return; }
  if (!res.ok) { popup("This file cannot be viewed", (res.error || "not a text file") + "\n\n" + path); return; }
  let t = tabs.find((x) => x.kind === "file" && x.path === path);
  if (t) { select(t.id); return; }
  t = makeTab(name, "file", path);
  const head = t.el.querySelector(".file-head");
  head.textContent = path + "  \u00B7  " + (res.size || 0) + " bytes" +
    (res.truncated ? "  \u00B7  truncated" : "") + (res.lines ? "  \u00B7  " + res.lines + " lines" : "");
  t.el.querySelector(".file-out").textContent = res.content || "";
  select(t.id);
}

/* ------------------------------------------------------------- websocket */
const OPEN = () => !!ws && ws.readyState === 1;

/* Wait until the socket is usable. Opening a WebSocket is asynchronous - the
   handshake (TCP, HTTP/1.1 upgrade, TLS when behind a proxy) has not finished
   when boot code runs - so anything that sends has to wait for `onopen`
   instead of giving up because the socket is still CONNECTING. Resolves false
   if it does not open within the deadline, so callers can say something
   better than hanging. */
function whenOpen(deadline) {
  return new Promise((resolve) => {
    if (OPEN()) { resolve(true); return; }
    const until = Date.now() + (deadline || 15000);
    /* Poll rather than bind to one socket's `open`: a failing socket is
       replaced by the reconnect loop, so listeners on the socket that existed
       when we got here would fire for a connection this page has already
       thrown away. The loop's own `onopen` gives the fast path. */
    const tick = () => {
      if (OPEN()) { resolve(true); return; }
      if (Date.now() >= until) { resolve(false); return; }
      setTimeout(tick, 60);
    };
    tick();
  });
}

function rpc(obj) {
  return new Promise((resolve, reject) => {
    const id = "r" + (++seq);
    const timer = setTimeout(() => { pending.delete(id); reject("timed out"); }, 20000);
    pending.set(id, { resolve, reject, timer });
    obj.id = id;
    const giveUp = (reason) => { clearTimeout(timer); pending.delete(id); reject(reason); };
    if (OPEN()) { ws.send(JSON.stringify(obj)); return; }
    whenOpen().then((up) => {
      if (!up) { giveUp("jonnyq is not connected yet"); return; }
      if (!OPEN()) { giveUp("the connection to jonnyq closed"); return; }
      ws.send(JSON.stringify(obj));
    });
  });
}
/* A folder's entries. `rpc` resolves the whole reply, so the node has to be
   taken out of it here - and a refused path (outside the working directory,
   unreadable) has to become an exception rather than an empty folder, or the
   tree would silently show nothing where it should say why. */
async function requestTree(path) {
  const m = await rpc({ type: "tree", path });
  if (!m.ok) throw new Error(m.error || "cannot list this folder");
  return m.node;
}

function onMessage(ev) {
  let m; try { m = JSON.parse(ev.data); } catch { return; }
  if (m.id && pending.has(m.id)) {
    const p = pending.get(m.id);
    clearTimeout(p.timer); pending.delete(m.id);
    p.resolve(m);
    return;
  }
  switch (m.type) {
    case "console": appendConsole(m.data || ""); break;
    case "ready": setReady(m.ready, m.note || ""); break;
    case "info":
      $("status").textContent = m.status || "";
      $("wd").textContent = m.work_dir || "";
      $("wd").title = m.work_dir || "";
      break;
    case "notice": popup(m.title || "jonnyq", m.body || ""); break;
    case "ask": onAskMessage(m); break;
    case "ask_close": onAskClose(m); break;
    case "reset":
      /* the server asked for a reload: the transcript it holds is gone — and so
         is the colour state that went with it, or the first line after the
         reload would be drawn in the colour the last chunk before it left on */
      if (consoleTab) {
        const o = consoleTab.el.querySelector("#console-out");
        o.textContent = "";
        trimMark = null;
        consoleChars = 0;
      }
      sgrOpen = [];
      refreshTree();
      break;
  }
}

function connect() {
  const proto = location.protocol === "https:" ? "wss:" : "ws:";
  ws = new WebSocket(proto + "//" + location.host + "/ws");
  ws.onopen = () => { $("conn").textContent = "connected"; $("conn").className = "on"; };
  ws.onmessage = onMessage;
  ws.onclose = () => {
    $("conn").textContent = "offline - reconnecting";
    $("conn").className = "off";
    setReady(false, "connection to jonnyq lost");
    /* Drop the questions this tab will not hear the endings of: the greeting
       of the next connection replays whatever jonnyq is still waiting for, so
       anything answered or withdrawn while we were offline disappears instead
       of lingering as a popup nobody will read. */
    asks.length = 0;
    askDrafts.clear();
    askBusy = false;
    renderAsk();
    setTimeout(connect, 1200);
  };
  ws.onerror = () => { try { ws.close(); } catch {} };
}

/* ---------------------------------------------------------------- boot */
buildTypeControls();
wireAsk();
consoleTab = makeTab("console", "console", null);
select(consoleTab.id);
setReady(false, "connecting");
connect();
refreshTree();
window.addEventListener("keydown", (e) => {
  /* Alt-1 / Alt-2 / ... switch tabs, like a terminal multiplexer. */
  if (!e.altKey || e.ctrlKey || e.metaKey) return;
  const n = parseInt(e.key, 10);
  if (n >= 1 && n <= tabs.length) { select(tabs[n - 1].id); e.preventDefault(); }
});
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::INDEX_HTML;

    /// The page is served inline, so a broken tag or an unbalanced brace is a
    /// runtime bug nobody sees until the browser is open. These are the cheap
    /// structural checks.
    #[test]
    fn page_is_a_complete_document() {
        assert!(INDEX_HTML.starts_with("<!DOCTYPE html>"));
        assert!(INDEX_HTML.trim_end().ends_with("</html>"));
        for tag in ["<style>", "</style>", "<script>", "</script>", "/ws"] {
            assert!(INDEX_HTML.contains(tag), "missing {tag}");
        }
    }

    #[test]
    fn page_implements_the_required_pieces() {
        for needle in [
            "console",                    /* the console tab */
            "id=\"prompt\"",              /* the prompt textarea */
            "resize: vertical",           /* and it is resizable */
            "ta.disabled",                /* and disabled while not ready */
            "id=\"tree\"",                /* the file tree */
            "gruvbox",                    /* the theme is named in a comment */
            "--bg0h: #1d2021",            /* gruvbox dark background */
            "--fg: #ebdbb2",              /* gruvbox dark foreground */
            "ansiToHtml",                 /* terminal output is rendered, not stripped */
            "This file cannot be viewed", /* the non-text popup */
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
    }

    /// The nine faces the human asked for, in the order asked for, with IBM
    /// Plex Sans Thai Looped as the default — and the stylesheet that offers
    /// them. The list lives in the page rather than in the server, so the
    /// choice is one write to a CSS property and never a request.
    #[test]
    fn the_nine_thai_faces_are_offered_with_the_named_default() {
        let want = [
            "Playpen Sans Thai",
            "K2D",
            "Kodchasan",
            "Noto Serif Thai",
            "Noto Sans Thai Looped",
            "IBM Plex Sans Thai Looped",
            "Athiti",
            "Thasadith",
            "Trirong",
        ];
        for f in want {
            assert!(
                INDEX_HTML.contains(&format!("\"{f}\"")),
                "missing {f} in the list"
            );
            assert!(
                INDEX_HTML.contains(&f.replace(' ', "+")),
                "{f} is not requested from Google Fonts"
            );
        }
        assert!(INDEX_HTML.contains("const DEFAULT_FONT = \"IBM Plex Sans Thai Looped\""));
        // And the CSS default matches, so the page looks right before any
        // stored choice is applied.
        assert!(
            INDEX_HTML.contains("--ui-font: \"IBM Plex Sans Thai Looped\""),
            "the CSS default face must match DEFAULT_FONT"
        );
    }

    /// Text size defaults to 14pt and is applied as a point size (not pixels)
    /// to the console, the file views and the prompt box, from a number field
    /// that refuses nonsense.
    #[test]
    fn the_text_size_defaults_to_14pt_and_is_clamped() {
        assert!(INDEX_HTML.contains("const DEFAULT_SIZE = 14"));
        assert!(INDEX_HTML.contains("--ui-size: 14pt"));
        assert!(INDEX_HTML.contains("var(--ui-size)"));
        assert!(INDEX_HTML.contains("\"pt\""));
        assert!(
            INDEX_HTML.contains("<input id=\"size\" type=\"number\" min=\"8\" max=\"40\""),
            "the size field needs a bounded range"
        );
        // Out-of-range and non-numeric input must be clamped, not obeyed.
        let clamp = INDEX_HTML
            .split("function clampSize(v)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("clampSize");
        assert!(clamp.contains("Math.min") && clamp.contains("Math.max"));
        assert!(clamp.contains("Number.isFinite"));
        assert!(INDEX_HTML.contains("SIZE_MIN = 8"));
        assert!(INDEX_HTML.contains("SIZE_MAX = 40"));
    }

    /// Loading a webfont must never hold up the console. The stylesheet is
    /// requested under `media="print"` and promoted to `all` once it arrives,
    /// which is how a browser is told not to block first paint on it — the
    /// difference between an offline session that opens instantly and one that
    /// sits there until the request times out.
    #[test]
    fn the_font_stylesheet_cannot_block_the_page() {
        let link = INDEX_HTML
            .split("<link rel=\"stylesheet\"")
            .nth(1)
            .and_then(|s| s.split(">").next())
            .expect("the fonts stylesheet link");
        assert!(
            link.contains("media=\"print\""),
            "must not block paint: {link}"
        );
        assert!(
            link.contains("onload=\"this.media='all'\""),
            "must apply once loaded: {link}"
        );
        assert!(
            link.contains("display=swap"),
            "text must render before the faces land: {link}"
        );
    }

    /// The colour the terminal showed has to survive a reload, and most of that
    /// work is in the page: it renders the SGR codes itself. Two ways this used
    /// to go grey, both pinned here.
    ///
    /// First, the escape state was local to one call, so a chunk that ends with
    /// the colour still switched on — which streamed answer text does on every
    /// token — left the next chunk, which contains no escape at all, to be drawn
    /// in default colour. The state now lives outside the function.
    #[test]
    fn the_ansi_state_carries_across_chunks() {
        let fn_body = INDEX_HTML
            .split("function ansiToHtml(text)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("ansiToHtml");
        assert!(
            fn_body.contains("sgrOpen"),
            "the renderer must read the attributes left over from the last chunk"
        );
        assert!(
            !fn_body.contains("let out = \"\", at = 0, open = []"),
            "a per-call `open` array is what reset the colour between chunks"
        );
        // Declared once, at page scope, and cleared only by a reset or by the
        // server's `reset` — not by the arrival of the next chunk.
        assert!(INDEX_HTML.contains("let sgrOpen = [];"));
        assert!(
            INDEX_HTML.contains("sgrOpen = [];"),
            "something must clear it on `reset`, or a reload would inherit the \
             colour of whatever the previous session last printed"
        );
    }

    /// Second way the console went grey: the front-trim, which crosses over on
    /// exactly the reload of a long session, rebuilt the transcript by assigning
    /// `textContent` — and one assignment replaces every colour span with a
    /// single plain text node. Trimming removes nodes now, and the "there is
    /// more above" line is its own element rather than a prefix glued on.
    #[test]
    fn trimming_the_transcript_cannot_erase_the_colours() {
        let body = INDEX_HTML
            .split("function appendConsole(text)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("appendConsole");
        assert!(
            !body.contains("out.textContent ="),
            "assigning textContent replaces every colour span with one text node"
        );
        assert!(body.contains("trimConsole("), "and must delegate the trim");
        let trim = INDEX_HTML
            .split("function trimConsole(out)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("trimConsole");
        assert!(
            trim.contains("removeChild"),
            "the trim has to drop nodes, not rewrite them: {trim}"
        );
        assert!(
            !trim.contains("out.textContent ="),
            "not even to prepend the marker: {trim}"
        );
    }

    /// Reading the transcript's size on every streamed chunk is a walk of the
    /// whole console, and streamed output calls `appendConsole` once per token.
    /// The limit is now decided from a running total.
    #[test]
    fn the_console_does_not_measure_itself_on_every_chunk() {
        let body = INDEX_HTML
            .split("function appendConsole(text)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("appendConsole");
        assert!(
            !body.contains("out.textContent.length >"),
            "the size test must not walk the DOM per token"
        );
        assert!(body.contains("consoleChars += text.length"));
    }

    /// The console, the prompt box and the chrome take the chosen face; a file
    /// view keeps a monospaced face and takes only the size, because source is
    /// read by its columns. All of them take the chosen size.
    #[test]
    fn a_file_view_keeps_its_columns() {
        let css = INDEX_HTML
            .split("<style>")
            .nth(1)
            .and_then(|s| s.split("</style>").next())
            .expect("the style block");
        // Split on the rule that *starts* a line: "#console-out, .file-out {"
        // also ends with those characters, and matching it would check the
        // shared rule instead of the file view's own.
        let file = css
            .split("\n.file-out {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the .file-out rule");
        assert!(
            file.contains("monospace"),
            "a file view must stay monospaced: {file}"
        );
        assert!(
            !file.contains("--ui-font"),
            "a file view must not take the prose face: {file}"
        );
        // Both still take the size, and the console takes the face too.
        assert!(css.contains("#console-out, .file-out"));
        assert!(css.contains("font-size: var(--ui-size)"));
        assert!(
            css.contains("#console-out { font-family: var(--ui-font); }"),
            "the console is the thing the picker was asked for"
        );
    }

    /// A chosen face that never downloaded is not an error: the CSS family name
    /// simply resolves to a locally installed copy or to the generic fallback,
    /// so the page must never depend on the stylesheet having arrived.
    #[test]
    fn the_font_choice_survives_no_internet() {
        let apply = INDEX_HTML
            .split("function applyType()")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("applyType");
        assert!(
            apply.contains("setProperty(\"--ui-font\""),
            "the face is applied through the fallback-carrying property: {apply}"
        );
        assert!(
            apply.contains("sans-serif"),
            "the stack must end in a generic family: {apply}"
        );
        // Multi-word family names need quoting or they do not match.
        assert!(
            apply.contains("'\"' + typeface.font"),
            "family name must be quoted: {apply}"
        );
        // Nothing is fetched at choice time: the stylesheet is referenced once,
        // from the head, and no script reaches for the network.
        assert_eq!(
            INDEX_HTML.matches("fonts.googleapis.com").count(),
            1,
            "the fonts stylesheet must appear only in the head"
        );
        assert!(
            !INDEX_HTML.contains("fetch("),
            "the page must not fetch anything"
        );
    }

    /// Both settings are remembered per browser, so a reload or a second tab
    /// opens with the same face and size.
    #[test]
    fn the_type_choice_is_remembered() {
        assert!(INDEX_HTML.contains("localStorage.getItem("));
        assert!(INDEX_HTML.contains("localStorage.setItem("));
        assert!(INDEX_HTML.contains("const LS = \"jonnyq.type\""));
        // A hand-edited or private-mode value must not break the page.
        let stored = INDEX_HTML
            .split("function storedType()")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("storedType");
        assert!(stored.contains("try") && stored.contains("catch"));
        // And an unknown stored family falls back rather than poisoning the stack.
        assert!(stored.contains("FONTS.includes"));
    }

    /// The tree reply is the whole message (`{type, ok, path, node}`), so the
    /// page has to reach into it: passing the envelope to `renderTree` renders
    /// an empty tree without any error, which is exactly the bug this pins.
    #[test]
    fn the_tree_reply_is_unwrapped_before_rendering() {
        let script = INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let tree = script
            .split("async function requestTree")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("requestTree");
        assert!(
            tree.contains(".node"),
            "requestTree must return m.node: {tree}"
        );
        assert!(
            tree.contains("m.ok"),
            "a refused path must throw, not look empty: {tree}"
        );
    }

    /// The page's first tree request happens at boot, before the WebSocket
    /// handshake has finished - `readyState` is still CONNECTING. An `rpc` that
    /// refuses instead of waiting for `open` produced
    /// "Cannot list the working directory / not connected" on every first load
    /// of a perfectly healthy session, which is what this pins.
    #[test]
    fn a_request_waits_for_the_socket_to_open() {
        let script = INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let rpc = script
            .split("function rpc(obj)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("rpc");
        assert!(
            rpc.contains("whenOpen"),
            "rpc must wait for the handshake instead of rejecting: {rpc}"
        );
        let wait = script
            .split("function whenOpen")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("whenOpen");
        assert!(
            wait.contains("readyState") || wait.contains("OPEN()"),
            "whenOpen must look at the socket state: {wait}"
        );
        assert!(
            !rpc.contains("reject(\"not connected\")"),
            "a page that has not finished connecting is not an error: {rpc}"
        );
        /* The waiter has to give up eventually, or a page opened against a
        dead port would spin forever instead of saying so. */
        assert!(
            wait.contains("until") && wait.contains("setTimeout"),
            "whenOpen needs a deadline: {wait}"
        );
    }

    /// Two listings of the same folder racing each other (boot plus the
    /// greeting's `reset`) would both render, so the second clears what the
    /// first drew. A refresh in flight must be reused, not duplicated.
    #[test]
    fn the_tree_is_listed_once_at_a_time() {
        let script = INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let refresh = script
            .split("async function refreshTree")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("refreshTree");
        assert!(
            refresh.contains("treeRefresh"),
            "refreshTree must collapse concurrent calls: {refresh}"
        );
    }

    /// The browser's answer box for `ask_user` (`-web <port>`): the question, a
    /// field to answer it with, and a Send button — plus the two messages the
    /// server uses to open and take back the popup. Without this an `ask_user`
    /// call in a headless session had no way to be answered at all.
    #[test]
    fn the_ask_user_popup_can_be_answered() {
        for needle in [
            "id=\"ask-mask\"",         /* the overlay                        */
            "id=\"ask-q\"",            /* the question itself                */
            "id=\"ask-answer\"",       /* the answer field                   */
            "id=\"ask-send\"",         /* and the button that sends it       */
            "type: \"answer\"",        /* the message the answer goes over   */
            "case \"ask\":",           /* the server opening a question      */
            "case \"ask_close\":",     /* and taking it back                 */
            "sendAsk",
            "onAskMessage",
            "onAskClose",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
        // The answer has to name the question it belongs to, or a reply to a
        // withdrawn question would be taken for the one currently on screen.
        let send = INDEX_HTML
            .split("function sendAsk()")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("sendAsk");
        assert!(send.contains("ask: q.id"), "the answer must carry its id: {send}");
        assert!(send.contains("rpc("), "and go over the socket: {send}");
    }

    /// A popup answered at the terminal or over ntfy is closed by the server;
    /// the tab says so, because a box that disappears while you are reading it
    /// looks like a bug otherwise.
    ///
    /// And nothing on the page may hide it. There used to be a "hide" button,
    /// and Escape did the same thing: both uncovered the console but left the
    /// tab holding a question whose answer field they had just taken away, so a
    /// human who used them could no longer answer at all. The popup is the thing
    /// jonnyq is blocked on, so it stays up until the server says it has been
    /// answered.
    #[test]
    fn an_answered_question_is_explained_and_the_box_cannot_be_hidden() {
        let close = INDEX_HTML
            .split("function onAskClose(m)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("onAskClose");
        assert!(
            close.contains("answered elsewhere"),
            "a popup closed by someone else must say so: {close}"
        );

        // The answer field is reachable for as long as a question is open: the
        // overlay shows from nothing but "a question is waiting".
        let render = INDEX_HTML
            .split("function renderAsk()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("renderAsk");
        assert!(
            render.contains(r#"$("ask-mask").classList.toggle("show", !!q)"#),
            "the popup shows while a question is open, with no other condition: {render}"
        );
        assert!(!render.contains("asks.splice"), "rendering may not drop it: {render}");

        // No hide control, no hidden flag, no reminder badge to bring a hidden
        // question back — so nothing can strand a question out of reach.
        for gone in ["ask-hide", "askHidden", "ask-badge", "askShown"] {
            assert!(!INDEX_HTML.contains(gone), "{gone} must be gone from the page");
        }
        let wire = INDEX_HTML
            .split("function wireAsk()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("wireAsk");
        assert!(!wire.contains("asks.splice"), "nothing here may drop it: {wire}");
        assert!(
            !wire.contains("ask-mask"),
            "no click and no key may take the popup away: {wire}"
        );

        // Escape still dismisses the *file* popup, and only that one.
        let esc = INDEX_HTML
            .split("if (e.key === \"Escape\")")
            .nth(1)
            .and_then(|s| s.split("});").next())
            .expect("the Escape handler");
        assert!(
            esc.contains(r#"$("mask").classList.remove("show")"#) && !esc.contains("ask-mask"),
            "Escape closes the file popup only: {esc}"
        );
    }

    /// Several questions can be open at once (a cancelled turn leaves the
    /// earlier one waiting), and only the front one is worth covering the
    /// screen with — the rest are counted, and the popup hands itself over as
    /// each is answered.
    #[test]
    fn queued_questions_wait_their_turn() {
        let msg = INDEX_HTML
            .split("function onAskMessage(m)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("onAskMessage");
        assert!(msg.contains("asks.push"), "questions queue: {msg}");
        assert!(
            msg.contains("m.open === false"),
            "a question behind another is not popped up: {msg}"
        );
        let render = INDEX_HTML
            .split("function renderAsk()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("renderAsk");
        assert!(render.contains("asks.length - 1"), "the count of those behind: {render}");
    }

    /// A reconnect must not leave the page holding questions that were answered
    /// while it was offline: the next greeting replays only what jonnyq is
    /// still waiting for, so the stale list is cleared when the link drops.
    #[test]
    fn a_reconnect_does_not_keep_stale_questions() {
        let onclose = INDEX_HTML
            .split("ws.onclose = () => {")
            .nth(1)
            .and_then(|s| s.split("\n  };").next())
            .expect("the socket's close handler");
        assert!(onclose.contains("asks.length = 0"), "stale questions are dropped: {onclose}");
        assert!(onclose.contains("renderAsk()"), "and the popup is redrawn: {onclose}");
    }

    /// The script keeps its block comments balanced: `*/` inside one would
    /// silently comment out the rest of the page.
    #[test]
    fn script_comments_are_balanced() {
        let script = INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let opens = script.matches("/*").count();
        let closes = script.matches("*/").count();
        assert_eq!(
            opens, closes,
            "unbalanced block comments in the page script"
        );
        assert!(!script.contains("/* */ */"));
    }
}
