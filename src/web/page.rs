//! The browser console's single page: HTML, CSS (gruvbox dark) and JS in one
//! file, served inline.
//!
//! Kept as one `const` string rather than `include_str!`ed asset files so the
//! whole web interface stays a single module and the binary needs no build
//! step or asset embedding. The page is deliberately dependency-free (no CDN,
//! no bundler): it must work offline, from a local model, on a machine with no
//! internet — which is most of the machines jonnyq runs on.
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
}
* { box-sizing: border-box; }
html, body { height: 100%; margin: 0; }
body {
  background: var(--bg0h); color: var(--fg);
  font: 14px/1.45 ui-monospace, SFMono-Regular, Menlo, Consolas, "DejaVu Sans Mono", monospace;
  display: flex; flex-direction: column; overflow: hidden;
}
#top {
  display: flex; align-items: center; gap: 10px;
  padding: 6px 10px; background: var(--bg0); border-bottom: 1px solid var(--bg2);
  flex: 0 0 auto;
}
#top b { color: var(--accent); letter-spacing: .04em; }
#status { color: var(--fg4); font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
#conn { margin-left: auto; font-size: 12px; color: var(--fg4); white-space: nowrap; }
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
#console-out, .file-out {
  flex: 1 1 auto; overflow: auto; margin: 0; padding: 10px 12px;
  background: var(--bg0h); white-space: pre-wrap; overflow-wrap: break-word;
  font-size: 13px;
  tab-size: 4; scrollbar-width: thin; scrollbar-color: var(--bg3) var(--bg0h);
}
.file-out { white-space: pre; }
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
  font-family: inherit; font-size: 18px; line-height: 1.4;
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
</style>
</head>
<body>
<div id="top">
  <b>jonnyq</b>
  <span id="status">connecting&hellip;</span>
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

/* ------------------------------------------------------------------ utils */
const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));

function popup(title, body) {
  $("popup-title").textContent = title;
  $("popup-body").textContent = body;
  $("mask").classList.add("show");
}
$("popup-ok").onclick = () => $("mask").classList.remove("show");
$("mask").onclick = (e) => { if (e.target === $("mask")) $("mask").classList.remove("show"); };
document.addEventListener("keydown", (e) => { if (e.key === "Escape") $("mask").classList.remove("show"); });

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
function ansiToHtml(text) {
  let out = "", at = 0, open = [];
  SGR.lastIndex = 0;
  let m;
  while ((m = SGR.exec(text)) !== null) {
    out += span(open, text.slice(at, m.index));
    at = SGR.lastIndex;
    const codes = (m[1] === "" ? ["0"] : m[1].split(";")).map((c) => (c === "" ? 0 : +c));
    for (const c of codes) {
      if (c === 0) open = [];
      else if (c === 1) { if (!open.includes("c1")) open.push("c1"); }
      else if (c === 22) open = open.filter((k) => k !== "c1");
      else if ((c >= 30 && c <= 37) || (c >= 90 && c <= 97)) {
        open = open.filter((k) => !/^c(9?[0-9]m?|3[0-7])$/.test(k) || k === "c1");
        open.push("c" + c);
      } else if (c === 39) open = open.filter((k) => !/^c[39]/.test(k) || k === "c1");
    }
  }
  out += span(open, text.slice(at));
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
     for a day, keeping all of it in the DOM would eat the tab. */
  if (out.childElementCount > 6000 || out.textContent.length > 1_500_000) {
    const keep = Math.max(0, out.childElementCount - 4000);
    for (let i = 0; i < keep; i++) out.removeChild(out.firstChild);
    out.textContent = "[... earlier output trimmed; it is still in the transcript log ...]\n" + out.textContent;
  }
  if (autoScroll) scrollToBottom();
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
  send({ type: "prompt", text });
  ta.value = "";
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
        .catch((e) => { kids.style.display = "none"; tw.textContent = "\u25B8"; loaded = false; popup("Cannot list " + item.path, String(e)); });
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
    popup("Cannot list the working directory", String(e));
  }
}
$("refresh").addEventListener("click", refreshTree);

/* Opening a file: one request; the server decides whether it is text. A
   binary/image/PDF answer comes back as an error and is shown in the popup,
   which is what the feature asks for ("cannot be viewed"). */
async function openFile(path, name) {
  let res;
  try { res = await rpc({ type: "file", path }); }
  catch (e) { popup("Cannot open file", String(e)); return; }
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
function send(obj) { ws && ws.readyState === 1 && ws.send(JSON.stringify(obj)); }
function rpc(obj) {
  return new Promise((resolve, reject) => {
    const id = "r" + (++seq);
    const timer = setTimeout(() => { pending.delete(id); reject("timed out"); }, 20000);
    pending.set(id, { resolve, reject, timer });
    obj.id = id;
    if (!ws || ws.readyState !== 1) { clearTimeout(timer); pending.delete(id); reject("not connected"); return; }
    ws.send(JSON.stringify(obj));
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
    case "reset":
      /* the server asked for a reload: the transcript it holds is gone */
      if (consoleTab) consoleTab.el.querySelector("#console-out").textContent = "";
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
    setTimeout(connect, 1200);
  };
  ws.onerror = () => { try { ws.close(); } catch {} };
}

/* ---------------------------------------------------------------- boot */
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
