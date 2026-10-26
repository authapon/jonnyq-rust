//! The browser console's single page: HTML, CSS (gruvbox dark) and JS in one
//! file, served inline.
//!
//! Kept as one `const` string rather than `include_str!`ed asset files so the
//! whole web interface stays a single module and the binary needs no build
//! step or asset embedding. The page is deliberately dependency-free (no CDN,
//! no bundler): it must work offline, from a local model, on a machine with no
//! internet — which is most of the machines jonnyq runs on.
//!
//! The one exception is the typefaces: the top bar offers nine Thai prose faces
//! and eighteen monospaced code faces from Google Fonts. That stylesheet is
//! *optional* in the strongest sense — it is requested in a way that cannot
//! block the first paint, so a machine with no internet draws the page
//! immediately and every family falls back to a locally installed copy of the
//! same name or to the generic tail of its stack (a sans-serif for prose, a
//! monospace for the terminal panes and the file views, whose cells and columns
//! depend on it). Nothing on the page waits for it and no script comes from it,
//! so offline behaviour is unchanged apart from which face the text is drawn in.
//!
//! Layout, per the feature:
//!
//! * centre: a tab strip (always a `console` tab, plus one per opened file)
//!   over the panel for the selected tab — the console is an ANSI-rendered
//!   transcript with the prompt textarea under it; a file tab is its text,
//!   syntax-highlighted by the scanner near `openFile` in the same palette, with
//!   a **Download** in its head that saves the file itself
//! * right: the working directory as a lazily expanded tree; clicking a file
//!   opens it in a new tab, clicking a non-text file pops up a refusal — which
//!   offers the file as a download, since a PDF you cannot read is often one you
//!   still want
//!
//! The prompt textarea is disabled unless the session says it is ready for a
//! prompt, and is resizable (`resize: vertical` plus a draggable splitter).
//! Beside it sits **Stop**, enabled exactly while the session says a turn is in
//! flight; pressing it sends one `stop` message, which cancels the same token
//! the terminal's Ctrl-C cancels (see [`crate::web::WebConsole::request_stop`]).

/// Where [`page_harness.js`] expects the page's own script to be inserted. A
/// comment in the harness, so the file parses on its own.
pub const PAGE_SCRIPT: &str = "@@PAGE_SCRIPT@@";

/// The harness itself, with the splice point still in it. The page's terminal
/// tests splice and run all of it; the web e2e test in [`crate::web`] uses only
/// the half above [`PAGE_SCRIPT`] — the fake DOM — so it can boot the page and
/// drive it with frames that came off a real pty.
pub const PAGE_HARNESS: &str = include_str!("page_harness.js");

/// The console page, with the version spliced into the tab title, the top bar's
/// brand and the terminal emulator's XTVERSION answer — all from
/// `CARGO_PKG_VERSION`, i.e. the `Cargo.toml` the binary was built from, so the
/// page can never advertise a build other than the one serving it.
pub const INDEX_HTML: &str = concat!(
    r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>jonnyq console v"##,
    env!("CARGO_PKG_VERSION"),
    r##"</title>
<!-- The typefaces, from Google Fonts: the nine Thai prose faces the console and
     the prompt box are drawn in, and the eighteen monospaced faces the terminal
     panes and the file views are drawn in. One request for both, because the
     page asks for a stylesheet, not per face, and each family is one `family=`
     parameter on the same URL.

     Optional by design, and fetched without blocking the paint: with
     `media="print"` the browser never waits on this stylesheet before drawing
     the page, so a machine with no internet (or one whose proxy blackholes the
     request) paints immediately and redraws when the faces arrive — or never,
     in which case every family below resolves to a locally installed copy of
     the same name, or to the generic family each stack ends in. Nothing on the
     page waits for it, and nothing is executed from it. -->
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link rel="stylesheet" media="print" onload="this.media='all'" href="https://fonts.googleapis.com/css2?family=Athiti:wght@400;700&amp;family=IBM+Plex+Sans+Thai+Looped:wght@400;700&amp;family=K2D:wght@400;700&amp;family=Kodchasan:wght@400;700&amp;family=Noto+Sans+Thai+Looped:wght@400;700&amp;family=Noto+Serif+Thai:wght@400;700&amp;family=Playpen+Sans+Thai:wght@400;700&amp;family=Thasadith:wght@400;700&amp;family=Trirong:wght@400;700&amp;family=Source+Code+Pro:wght@400;700&amp;family=Roboto+Mono:wght@400;700&amp;family=JetBrains+Mono:wght@400;700&amp;family=IBM+Plex+Mono:wght@400;700&amp;family=Space+Mono:wght@400;700&amp;family=Fira+Code:wght@400;700&amp;family=Google+Sans+Code:wght@400;700&amp;family=Ubuntu+Mono:wght@400;700&amp;family=Ubuntu+Sans+Mono:wght@400;700&amp;family=Anonymous+Pro:wght@400;700&amp;family=Nova+Mono:wght@400;700&amp;family=Syne+Mono:wght@400;700&amp;family=Iosevka+Charon+Mono:wght@400;700&amp;family=Sono:wght@400;700&amp;family=Intel+One+Mono:wght@400;700&amp;family=SUSE+Mono:wght@400;700&amp;family=Cascadia+Mono:wght@400;700&amp;family=Libertinus+Mono:wght@400;700&amp;display=swap">
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
     The mono pair belongs to the panes that are grids of cells — the terminal
     tabs and the file views — and is deliberately separate: a cell grid needs
     a monospaced face whatever the prose face is set to, and 12pt reads better
     in a shell than the 14pt that suits prose. Both are overwritten from
     localStorage on boot, before the first paint, and every family in each
     stack resolves to a locally installed copy or to the generic tail if the
     stylesheet above never arrived. */
  --ui-font: "IBM Plex Sans Thai Looped", ui-sans-serif, system-ui, "Segoe UI", sans-serif;
  --ui-size: 14pt;
  --mono-font: "IBM Plex Mono", ui-monospace, SFMono-Regular, Menlo, Consolas, "DejaVu Sans Mono", monospace;
  --mono-size: 12pt;
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
/* The version of the build serving the page, in the brand itself rather than
   somewhere the reader has to know to look: it is the first thing worth knowing
   about a console, and it is written in from Cargo.toml when the page is built,
   so it can never name a different build from the one answering. It sits inside
   the brand's `<b>` and overrides the accent colour, the letter-spacing and the
   bold that rule gives its children — a version number is a footnote to the
   name, not the name — and it is short enough that the status line, which is
   the bar's flexible element, never has to give up room for it. */
#top b #ver { color: var(--fg4); font-weight: normal; letter-spacing: 0; font-size: 11px; }
/* The status line is the session's live ctx figure, so it gets the space a
   long model name cannot take from it: it shrinks and ellipsises instead of
   pushing the type controls and the connection light out of the bar. */
#status {
  color: var(--fg4); font-size: 12px; white-space: nowrap;
  flex: 1 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis;
}
/* The bar carries two pairs of controls — the face and size of prose, and the
   face and size of code — so the second pair is separated from the first by a
   little air rather than by a rule that would fight the gruvbox bar.

   The block is shrinkable and wraps within itself: four controls and two font
   names are wider than the bar of a half-screen window, and a control that has
   been pushed off the edge is a control that cannot be reached. So the status
   line gives up its room first (it ellipsises), then the pickers narrow, then
   they move to a second row — which is why nothing here is `flex: 0 0 auto`. */
#type { display: flex; align-items: center; gap: 4px; flex: 0 1 auto; min-width: 0; flex-wrap: wrap; justify-content: flex-end; }
#type label { font-size: 11px; color: var(--fg4); }
#type label.grp { margin-left: 8px; }
#font, #size, #code, #code-size {
  background: var(--bg1); color: var(--fg1); border: 1px solid var(--bg2);
  border-radius: 3px; font: inherit; font-size: 12px; padding: 1px 3px; outline: none;
}
#font, #code { max-width: 14em; min-width: 7em; flex: 0 1 auto; }
#size, #code-size { width: 4.5em; flex: 0 0 auto; }
#font:focus, #size:focus, #code:focus, #code-size:focus { border-color: var(--bg4); }
#font:hover, #size:hover, #code:hover, #code-size:hover { border-color: var(--bg4); }
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
   lines up, aligned `=` signs, side-by-side diffs. So it takes the *code* pair
   — the monospaced face and the code size — rather than the prose face the
   console is drawn in. Every face the code picker offers is monospaced, so a
   file view keeps its columns whichever one is chosen; the prose faces are
   proportional and cannot tile columns, which is why they are not offered to
   the panes that are grids. */
.file-out {
  white-space: pre;
  font-family: var(--mono-font);
  font-size: var(--mono-size);
}
.file-head {
  flex: 0 0 auto; display: flex; align-items: center; gap: 8px;
  padding: 4px 12px; background: var(--bg0);
  border-bottom: 1px solid var(--bg2); color: var(--fg4); font-size: 12px;
}
/* The path, the size and the language on the left; the button pushed to the
   right by the spacer, so the header reads like the terminal head it sits
   beside rather than as a sentence with a control glued to it. */
.file-head .file-info { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.file-head .spacer { flex: 1 1 auto; }
.file-head .dl, #popup-dl {
  background: var(--bg1); color: var(--fg1); border: 1px solid var(--bg2);
  border-radius: 4px; padding: 2px 8px; font: inherit; cursor: pointer; flex: 0 0 auto;
}
.file-head .dl:hover, #popup-dl:hover { border-color: var(--accent); color: var(--accent); }
/* A download with nothing to download it of: the popup is shared with every
   other notice, so the button hides rather than lies. */
.hide { display: none !important; }
/* --- syntax colours, one class per role, in the same gruvbox dark palette the
       console's SGR classes are drawn from (gruvbox's own role mapping: comment
       gray, statement red, type yellow, string green, constant purple,
       identifier blue, special aqua — in the bright variants, which is what
       gruvbox uses on a dark background). A file view is source code, so these
       are not a second theme to keep in step with the first: they are the same
       seven hues as the ANSI block below, and they reuse its variables. */
.s-com { color: var(--f12); font-style: italic; }   /* a comment, dimmed and set apart */
.s-kw { color: var(--f0); }        /* a reserved word, and Markdown's heading marks */
.s-type { color: var(--f2); }      /* a type, a section header, a SCREAMING config word */
.s-str { color: #b8bb26; }         /* a string — bright green, as the console's c92 */
.s-key { color: var(--f6); }       /* the name left of a `:` or an `=` */
.s-num { color: var(--f4); }       /* a literal */
.s-const { color: var(--f4); }     /* and a name that reads as one */
.s-fn { color: var(--f3); }        /* a call */
.s-var { color: var(--f4); }       /* $VARIABLE, ${...}, a shell flag, a YAML anchor */
.s-pre { color: var(--f6); }       /* a preprocessor line, an at-rule, a #hex colour */
.s-tag { color: var(--f6); }       /* a markup tag */
.s-attr { color: var(--f3); }      /* and its attributes */
.s-op { color: var(--f7); }        /* the brackets around markup, deliberately quiet */
/* A fenced code block inside a README: a shade darker than the pane behind it,
   which is how gruvbox's own `hlsla` renders a code block on bg0. */
.s-code {
  color: var(--fg1); background: var(--bg0h);
  border-radius: 2px; padding: 0;
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
  background: var(--bg1); color: var(--fg); border: 1px solid var(--bg2);
  border-radius: 4px; padding: 3px 12px; font: inherit; font-size: 12px; cursor: pointer;
}
#send:hover:enabled { border-color: var(--accent); color: var(--accent); }
#send:disabled { color: var(--fg4); cursor: progress; }
/* The two buttons under the textarea share a row so Stop sits next to Send
   rather than stacking a second right-aligned line under it. */
#prompt-row { display: flex; align-items: center; justify-content: flex-end; gap: 8px; }
/* Stop is the browser's Ctrl-C, so it is drawn as the destructive control it
   is (gruvbox red) and only ever looks pressable while a turn is in flight:
   the server's busy flag drives `disabled`, and a disabled one is greyed like
   the textarea it belongs to, so there is no "why did nothing happen". */
#stop {
  background: var(--bg1); color: var(--f0); border: 1px solid var(--f0);
  border-radius: 4px; padding: 3px 12px; font: inherit; font-size: 12px; cursor: pointer;
}
#stop:hover:enabled { background: var(--f0); color: var(--bg0h); }
#stop:disabled { color: var(--fg4); border-color: var(--bg2); cursor: progress; }

/* --- terminal tabs ---
   A terminal tab is a pane of cells, so the face is the monospaced one the
   picker's code list offers, whatever the prose face is set to (proportionate
   glyphs cannot tile a grid), and the size is the code size the file views take
   too. Both are applied here rather than by script: measureCell() then reads the
   numbers back out of getComputedStyle, so the type controls and the grid cannot
   disagree — and a face that finishes downloading late is caught by the same
   poll that catches a window resize. */
.term-head {
  flex: 0 0 auto; display: flex; align-items: center; gap: 8px;
  padding: 4px 10px; background: var(--bg0); border-bottom: 1px solid var(--bg2);
  color: var(--fg4); font-size: 12px; white-space: nowrap; overflow: hidden;
}
.term-head .dir { color: var(--f3); overflow: hidden; text-overflow: ellipsis; }
.term-head .shell { color: var(--fg4); }
.term-head .spacer { flex: 1 1 auto; }
.term-head .gone { color: var(--f0); }
.term-head button {
  background: var(--bg1); color: var(--fg1); border: 1px solid var(--bg2);
  border-radius: 4px; padding: 2px 8px; font: inherit; cursor: pointer; flex: 0 0 auto;
}
.term-head button:hover { border-color: var(--accent); color: var(--accent); }
/* Keystrokes need a real input surface, and a div is not one: a browser only
   starts an IME composition, honours a keyboard layout's dead keys, or offers a
   soft keyboard on a phone when there is a text field to type into. So the pane
   keeps a hidden textarea — the same trick every browser terminal uses — and it
   sits beside the pane rather than inside it, because an element inside a
   scrolling container gets the pane's scroll yanked to it when it is focused. */
.term-input {
  position: absolute; right: 2px; bottom: 2px; width: 1px; height: 1px;
  padding: 0; margin: 0; border: 0; outline: none; resize: none; overflow: hidden;
  background: transparent; color: transparent; caret-color: transparent;
  font: inherit; opacity: 0;
}
.term-out {
  flex: 1 1 auto; overflow: auto; position: relative; outline: none;
  background: var(--bg0h); padding: 6px 8px; cursor: text;
  font: var(--mono-size)/1.25 var(--mono-font);
  scrollbar-width: thin; scrollbar-color: var(--bg3) var(--bg0h);
  /* The pane does its own keeping-still. A line that scrolls off the top of the
     screen moves into the scrollback above it, which leaves every row of output
     exactly where it was — but the browser cannot see that, it only sees rows
     inserted above the viewport, and its own scroll anchoring would "correct" a
     position that was never wrong. It also has no idea which of the pane's
     changes the reader should be kept anchored to, so it gets switched off and
     the code below says what happens on each one. */
  overflow-anchor: none;
}
/* The scrollback: the rows that have already scrolled past the top of the
   screen, in their own element above the grid so the grid can be repainted
   without touching them.

   A row is a `div` of its own, and the two properties that matter are both
   about geometry: `white-space: pre` because a shell's column-aligned output is
   made of spaces that a default `div` collapses, and the fixed height because a
   row with nothing on it has no line box and would otherwise collapse to
   nothing — which the pane's scroll arithmetic, measured in rows, would read as
   a jump. Face, size and leading come from the pane by inheritance, so a row
   tiles exactly like a row of the grid under it, and `line-height` is a fixed
   length rather than a keyword, which is what makes "one row = one height" true
   for every row in the block rather than merely usually true. */
.term-hist { white-space: pre; }
.term-hist-row { white-space: pre; min-height: 1.25em; }
.panel:focus-within .term-out { box-shadow: inset 0 0 0 1px var(--bg4); }
/* The grid has to be *told* the face, not left to inherit it. The browser's own
   stylesheet puts `font-family: monospace` on `pre`, and an inherited value
   loses to a UA declaration every time — a `pre` that is merely descended from
   a styled pane draws in the UA's monospace whatever the code picker says. The
   pane above is a `div`, so it was never affected; the grid is the element that
   actually draws the cells, and it silently kept the same face through every
   change the picker made.

   Hence the same two properties here as on .term-out, spelled identically: the
   span that measures the cell is a `span` and inherits from the pane, so if the
   grid were drawn in a different face from the measure, the cell width sent to
   the shell would not be the width the glyphs are drawn at, and a grid of cells
   stops being a grid. */
.term-grid {
  margin: 0; white-space: pre;
  font: var(--mono-size)/1.25 var(--mono-font);
}
/* The cell measure, in the pane's own font metrics: one long word plus a line
   break, measured by scrollWidth/scrollHeight. Hidden by having no layout of
   its own — it must not change the size of the thing whose size it gives. */
.term-size {
  position: absolute; left: 0; top: 0; visibility: hidden; height: 0; width: 0;
  overflow: hidden; white-space: pre; pointer-events: none;
}
.tv { background: var(--fg4); color: var(--bg0h); }
.b30 { background: var(--bg3); } .b90 { background: var(--f12); }
.b31 { background: var(--red); }  .b91 { background: var(--f0); }
.b32 { background: var(--green); } .b92 { background: #b8bb26; }
.b33 { background: var(--yellow); } .b93 { background: var(--f2); }
.b34 { background: var(--blue); }  .b94 { background: var(--f3); }
.b35 { background: var(--purple); } .b95 { background: var(--f4); }
.b36 { background: var(--aqua); }  .b96 { background: var(--f6); }
.b37 { background: var(--fg4); }   .b97 { background: var(--fg); }
.ti { font-style: italic; } .tu { text-decoration: underline; }
.ts { text-decoration: line-through; }
/* The tab itself is named after the folder the shell is in, so several open at
   once tell you which is which; the dot is the one bit of state worth colour
   for: orange while the shell is running, grey once it is not. */
.tab .dot { color: var(--accent); }
.tab.dead .dot { color: var(--bg4); }

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
/* The two buttons share the row so a Download sits beside OK rather than
   stacking a second line under it, and stays right-aligned like the rest of the
   page's button rows. */
#popup-row { display: flex; align-items: center; justify-content: flex-end; gap: 8px; }

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
/* The same Stop, in the row of the popup that covers it. A question is the one
   thing that leaves a session blocked with the prompt box underneath the mask,
   so "cancel this turn" has to be reachable from in front of the mask too —
   otherwise the only way out of an unwanted question is a terminal that a
   headless session does not have. Cancelling closes the popup from the server
   (`ask_close`), so this is not a second way to hide a question: the question
   goes away because nothing is asking it any more. */
#ask-stop {
  flex: 0 0 auto; background: var(--bg1); color: var(--f0); border: 1px solid var(--f0);
  border-radius: 4px; padding: 4px 14px; font: inherit; font-size: 13px; cursor: pointer;
}
#ask-stop:hover:enabled { background: var(--f0); color: var(--bg0h); }
#ask-stop:disabled { color: var(--fg4); border-color: var(--bg2); cursor: progress; }
</style>
</head>
<body>
<div id="top">
  <b>jonnyq<span id="ver" title="the build serving this page"> v"##,
    env!("CARGO_PKG_VERSION"),
    r##"</span></b>
  <span id="status">connecting&hellip;</span>
  <span id="type">
    <label for="font">text</label>
    <select id="font" title="typeface of the console, the prompt box and the bar (a file view and a terminal pane stay monospaced so their columns line up)"></select>
    <input id="size" type="number" min="8" max="40" step="1" title="text size in pt, for the console and the prompt box" aria-label="text size in pt">
    <label for="code" class="grp">code</label>
    <select id="code" title="monospaced typeface of the terminal panes and the file views"></select>
    <input id="code-size" type="number" min="8" max="40" step="1" title="code size in pt, for the terminal panes and the file views" aria-label="code size in pt">
    <button class="reset" id="type-reset" title="back to IBM Plex Sans Thai Looped at 14pt and IBM Plex Mono at 12pt">reset</button>
  </span>
  <span id="conn">offline</span>
</div>
<div id="main">
  <div id="centre">
    <div id="tabs"></div>
    <div id="panels"></div>
  </div>
  <div id="side">
    <div id="side-head"><span class="wd" id="wd">&hellip;</span></div>
    <div id="tree"></div>
  </div>
</div>
<div id="mask"><div id="popup">
  <h3 id="popup-title"></h3><pre id="popup-body"></pre>
  <div id="popup-row">
    <button class="ok hide" id="popup-dl" title="save this file">Download</button>
    <button class="ok" id="popup-ok">OK</button>
  </div>
</div></div>
<div id="ask-mask"><div id="ask" role="dialog" aria-modal="true" aria-labelledby="ask-title">
  <h3 id="ask-title">Question from the model</h3>
  <pre id="ask-q"></pre>
  <div id="ask-more"></div>
  <textarea id="ask-answer" spellcheck="false" wrap="soft"
    placeholder="Type your answer. Enter sends, Shift-Enter adds a newline."></textarea>
  <div id="ask-row">
    <span id="ask-note"></span>
    <button id="ask-stop" title="cancel the turn that is asking, like Ctrl-C in the terminal">Stop</button>
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
let busy = false;              /* a turn is in flight, so there is something to stop */
let autoScroll = true;         /* follow the transcript unless scrolled up      */
let consoleTab = null;
let treeRefresh = null;        /* the listing in flight, so two are never asked for */

/* ------------------------------------------------------------ typography
   Two pickers, because the page draws two kinds of text. Nine Thai faces from
   Google Fonts, in the order the user asked for them, set the console, the
   prompt box and the chrome, with IBM Plex Sans Thai Looped as the default.
   Eighteen monospaced faces, also from Google Fonts and also in the order
   asked for, set the terminal panes and the file views, with IBM Plex Mono as
   the default — those two surfaces are grids of cells, and a proportional face
   cannot tile a grid, so the lists are deliberately separate.

   The value of each is the CSS family name: if the stylesheet above never
   arrived (no internet) the name still resolves to a locally installed copy of
   the same face, and otherwise to the generic family its stack ends in —
   choosing a font is never a network operation.

   All four settings live in one localStorage key, so a reload or a second tab
   keeps what was picked, and they are written to four custom properties on
   :root rather than to each element: one write moves every surface, including
   a tab opened later, with no per-element bookkeeping. */
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
/* Every one of these is monospaced at Google Fonts (checked against the fonts
   it serves), which is what makes it safe to offer them for a pane whose cells
   have to line up. */
const MONO_FONTS = [
  "Source Code Pro",
  "Roboto Mono",
  "JetBrains Mono",
  "IBM Plex Mono",
  "Space Mono",
  "Fira Code",
  "Google Sans Code",
  "Ubuntu Mono",
  "Ubuntu Sans Mono",
  "Anonymous Pro",
  "Nova Mono",
  "Syne Mono",
  "Iosevka Charon Mono",
  "Sono",
  "Intel One Mono",
  "SUSE Mono",
  "Cascadia Mono",
  "Libertinus Mono",
];
const DEFAULT_FONT = "IBM Plex Sans Thai Looped";
const DEFAULT_MONO_FONT = "IBM Plex Mono";
const DEFAULT_SIZE = 14;                  /* pt */
const DEFAULT_MONO_SIZE = 12;             /* pt */
const SIZE_MIN = 8, SIZE_MAX = 40;
const LS = "jonnyq.type";                /* the one localStorage key */

function clampSize(v, dflt) {
  const n = Math.round(parseFloat(v));
  if (!Number.isFinite(n)) return dflt;
  return Math.min(SIZE_MAX, Math.max(SIZE_MIN, n));
}

function storedType() {
  try {
    const raw = localStorage.getItem(LS);
    if (!raw) return null;
    const v = JSON.parse(raw);
    /* A stored value from an older page carries only `font` and `size`; the
       code pair falls back to its defaults rather than poisoning the stack. An
       unknown family name is dropped for the same reason. */
    const font = FONTS.includes(v && v.font) ? v.font : DEFAULT_FONT;
    const code = MONO_FONTS.includes(v && v.code) ? v.code : DEFAULT_MONO_FONT;
    return {
      font, code,
      size: clampSize(v && v.size, DEFAULT_SIZE),
      codeSize: clampSize(v && v.codeSize, DEFAULT_MONO_SIZE),
    };
  } catch { return null; }    /* private mode, or a hand-edited value */
}

function storeType(t) {
  try { localStorage.setItem(LS, JSON.stringify(t)); } catch {}
}

let typeface = storedType() || {
  font: DEFAULT_FONT, size: DEFAULT_SIZE,
  code: DEFAULT_MONO_FONT, codeSize: DEFAULT_MONO_SIZE,
};

function applyType() {
  const r = document.documentElement.style;
  /* Quote the family name: "Noto Serif Thai" is three words, and an unquoted
     multi-word family in a custom property does not match. */
  r.setProperty("--ui-font", '"' + typeface.font.replace(/"/g, "") + '", ui-sans-serif, system-ui, sans-serif');
  r.setProperty("--ui-size", typeface.size + "pt");
  /* The code stack ends in the generic monospace, not in a sans-serif: a face
     that never downloaded must still draw monospaced, or the terminal's cells
     and a file's columns stop lining up. */
  r.setProperty("--mono-font", '"' + typeface.code.replace(/"/g, "") + '", ui-monospace, SFMono-Regular, Menlo, Consolas, "DejaVu Sans Mono", monospace');
  r.setProperty("--mono-size", typeface.codeSize + "pt");
  /* A pane's cell measure is whatever the face it is drawn in says it is, so
     every shell is told its new size — now if it can be measured now, and by
     the fit poll if the pane is hidden and measures nothing. */
  refitTerms();
}

/* Re-measure every live shell, because its cell is whatever the code face says
   it is. termTabs is the shells rather than the tabs, so a shell that has not
   finished opening is not measured from a pane that measures nothing — it is
   measured by termStart when its session lands. termFit sends nothing unless
   the numbers moved, so this is free when the change did not affect the grid. */
function refitTerms() {
  for (const t of termTabs.values()) termFit(t);
}

function syncTypeControls() {
  $("font").value = typeface.font;
  $("size").value = typeface.size;
  $("code").value = typeface.code;
  $("code-size").value = typeface.codeSize;
}

/* One `<select>` of font names, each option drawn in its own face so the list
   is a preview of what picking it does rather than a list of names to guess at.
   The generic tail differs by list: a code option previews as monospace, so an
   undownloaded face still looks like what it will be. */
function fillFontSelect(sel, faces, generic) {
  for (const f of faces) {
    const o = document.createElement("option");
    o.value = f;
    o.textContent = f;
    o.style.fontFamily = '"' + f + '", ' + generic;
    sel.appendChild(o);
  }
}

function buildTypeControls() {
  fillFontSelect($("font"), FONTS, "sans-serif");
  fillFontSelect($("code"), MONO_FONTS, "monospace");
  $("font").addEventListener("change", (e) => {
    const v = e.currentTarget.value;
    typeface = { ...typeface, font: FONTS.includes(v) ? v : DEFAULT_FONT };
    applyType(); syncTypeControls(); storeType(typeface);
  });
  $("code").addEventListener("change", (e) => {
    const v = e.currentTarget.value;
    typeface = { ...typeface, code: MONO_FONTS.includes(v) ? v : DEFAULT_MONO_FONT };
    applyType(); syncTypeControls(); storeType(typeface);
  });
  /* One number field per pair. The input is trusted only after clamping, and
     the field is then rewritten to what was actually applied, so "3" cannot sit
     in a box that is drawing 8pt. */
  const sizeBox = $("size"), codeBox = $("code-size");
  const commit = (key, box, dflt) => {
    typeface = { ...typeface, [key]: clampSize(box.value, dflt) };
    applyType(); syncTypeControls(); storeType(typeface);
  };
  sizeBox.addEventListener("change", () => commit("size", sizeBox, DEFAULT_SIZE));
  codeBox.addEventListener("change", () => commit("codeSize", codeBox, DEFAULT_MONO_SIZE));
  for (const [key, box, dflt] of [["size", sizeBox, DEFAULT_SIZE], ["codeSize", codeBox, DEFAULT_MONO_SIZE]])
    box.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); commit(key, box, dflt); } });
  $("type-reset").addEventListener("click", () => {
    typeface = {
      font: DEFAULT_FONT, size: DEFAULT_SIZE,
      code: DEFAULT_MONO_FONT, codeSize: DEFAULT_MONO_SIZE,
    };
    applyType(); syncTypeControls(); storeType(typeface);
  });
  applyType();
  syncTypeControls();
  /* A webfont that lands after the first paint changes the cell the grid was
     measured in. The fit poll catches it within its tick anyway, but the face's
     own ready promise says when the faces have arrived, so a pane retiles as
     soon as it can rather than up to a poll interval later. */
  if (document.fonts && document.fonts.ready && document.fonts.ready.then)
    document.fonts.ready.then(refitTerms, () => {});
}

/* ------------------------------------------------------------------ utils */
const esc = (s) => s.replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" }[c]));

/* A rejection here is usually a plain string ("jonnyq is not connected yet"),
   sometimes an Error from a refused path; the popup must read like a sentence
   either way, not like "Error: ..." or "[object Object]". */
const why = (e) => (e && e.message ? e.message : String(e));

/* The notice box: what it says, and — when the notice is about a file the pane
   cannot draw — the file it is about. `path` is optional; only the "cannot be
   viewed" answer passes one, and every other notice (a failed prompt, a stopped
   turn, a refused folder) gets the box with the Download button hidden, because
   a button that downloads nothing is a button that lies. */
let popupPath = "";

function popup(title, body, path) {
  $("popup-title").textContent = title;
  $("popup-body").textContent = body;
  popupPath = path || "";
  $("popup-dl").classList.toggle("hide", !popupPath);
  $("mask").classList.add("show");
}
$("popup-ok").onclick = () => $("mask").classList.remove("show");
$("popup-dl").onclick = () => downloadFile(popupPath);
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
  /* The popup's Stop follows the same flag as the prompt box's, and has to be
     re-synced here: the popup can appear without a ready/info frame ever
     having been seen (a reload mid-ask, or a question arriving in a tab that
     was opened while nothing ran). */
  syncStop();
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
  /* Stop belongs in this row because the mask covers the prompt box: a session
     blocked on a question would otherwise be uncancellable from the browser,
     and in a headless session there is no terminal to press Ctrl-C at. It does
     not hide or dismiss anything — cancelling ends the turn, and the server's
     ask_close takes the popup away with the question it was holding. */
  $("ask-stop").addEventListener("click", (e) => sendStop(e.currentTarget));
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

/* ---------------------------------------------------- terminal emulation
   A terminal tab is a window onto a PTY, and PTY output is a stream of cursor
   movement, erase and colour sequences — so the page has to be a terminal, not
   a `<pre>`. This is that emulation: it takes characters, keeps a screen of
   `rows` lines of `cols` cells, and hands back HTML.

   It is deliberately not a general xterm. It is what the programs people
   actually open in a terminal tab emit — a shell with readline, `ls --color`,
   `less`, `vim`, `htop`, a REPL — and it was written against captures of those
   from a real PTY, then diffed against an independent emulator's screen at
   several points in each capture. What it does not implement it swallows
   rather than draws, and what a program *asks* it answers, because a program
   that queries the terminal and gets nothing just sits there (that silence is
   the one failure mode that makes a pane look hung rather than wrong). */

/* One cell's blank: a space with no attributes. */
function blankLine(cols) {
  const ch = new Array(cols);
  const at = new Array(cols);
  for (let i = 0; i < cols; i++) { ch[i] = " "; at[i] = ""; }
  /* `cont`: this row runs on to the next one, so the two are one logical line
     drawn across two rows. It is what makes the scrollback re-wrappable — and
     it lives on the row rather than on the screen, because a row keeps its
     identity when it scrolls up into the history. */
  return { ch, at, cont: false };
}

/* DEC special graphics (the ACS line-drawing set), used by `less`, `vim` and
   anything else that draws a box with `ESC ( 0` instead of Unicode. */
const DEC_GRAPHICS = {
  "`": "\u25C6", a: "\u2592", f: "\u00B0", g: "\u00B1", j: "\u2518", k: "\u2510",
  l: "\u250C", m: "\u2514", n: "\u253C", o: "\u23BA", p: "\u23BB", q: "\u2500",
  r: "\u23DC", s: "\u23DD", t: "\u251C", u: "\u2524", v: "\u2534", w: "\u252C",
  x: "\u2502", y: "\u2264", z: "\u2265", "{": "\u03C0", "|": "\u2260",
  "}": "\u00A3", "~": "\u00B7",
};

/* The 256-colour palette above the sixteen the page already has classes for:
   indexes 16..255 are the xterm 6x6x6 cube and its greyscale ramp, and apps
   that emit `38;5;N` expect those exact values. 0..15 are left to the CSS
   classes so the terminal takes the gruvbox colours the rest of the page uses. */
function xterm256(n) {
  if (n < 16) return null;                    /* the palette the page themes */
  if (n >= 232) {
    const v = 8 + (n - 232) * 10;
    return "#" + [v, v, v].map((x) => x.toString(16).padStart(2, "0")).join("");
  }
  const i = n - 16;
  const lv = [0, 95, 135, 175, 215, 255];
  const r = lv[Math.floor(i / 36)], g = lv[Math.floor(i / 6) % 6], b = lv[i % 6];
  return "#" + [r, g, b].map((x) => x.toString(16).padStart(2, "0")).join("");
}

/* How many scrolled-past rows a pane remembers. A terminal emulator keeps a
   scrollback for exactly this reason and every one of them bounds it: a `cat`
   of a 200 MB log must not turn the tab into a 200 MB DOM. 2000 rows is deep
   enough to read a build's output from the top and shallow enough that drawing
   it is never the expensive part of a frame. */
const TERM_HIST = 2000;

/* How far below the cap the scrollback is cut when it overflows. Trimming one
   row at a time would mean re-rendering two thousand rows for every row the
   next `cat` pushes out, so a whole block goes at once and the redraw is
   amortised to one in TERM_HIST_TRIM rows. */
const TERM_HIST_TRIM = 250;

class TermScreen {
  constructor(cols, rows) {
    this.cols = Math.max(1, cols | 0);
    this.rows = Math.max(1, rows | 0);
    this.saved = null;      /* cursor saved by ESC 7 / CSI s */
    this.alt = null;        /* the main screen, while the alt one is showing */
    this.reply = "";        /* bytes the shell asked for, owed back to it */
    this.reset();
  }

  /* Back to a blank screen and initial state — what `reset` and DECRC-from-
     nothing mean, and what the page does when a tab is opened. */
  reset() {
    this.lines = [];
    for (let r = 0; r < this.rows; r++) this.lines.push(blankLine(this.cols));
    /* The scrollback: rows that have gone past the top of the screen. They are
       kept here rather than re-derived from the screen, because by the time the
       reader scrolls up the shell has overwritten those cells. Rows are the
       same row objects the screen drew, never copies, so colour survives the
       trip, and `cont` says which ones were soft-wrapped, so they can be
       re-wrapped at a new width.

       A live screen being reset loses its scrollback with it (`RIS` is the
       shell saying "forget everything"), and the pane has to be told; a fresh
       screen has nothing to say. */
    this.histRebuild = !!(this.hist && this.hist.length);
    this.hist = [];
    this.histAdd = [];        /* rows drawn since the pane last looked           */
    this.histDrop = 0;        /* rows lost to the cap, likewise                  */
    this.x = 0; this.y = 0;
    this.top = 0; this.bottom = this.rows - 1;
    this.attr = "";
    this.wrap = false;        /* a pending wrap: the next glyph starts a line */
    this.visible = true;
    this.decckm = false;      /* application cursor keys (vim needs this) */
    this.bracketed = false;   /* bracketed paste mode */
    this.charset = false;     /* DEC special graphics in use */
    this.sync = false;        /* synchronized output: repaint at the frame end */
    this.tabs = 8;
    this.state = "ground";
    this.priv = ""; this.inter = ""; this.params = "";
    this.osc = "";
    this.altActive = false;
  }

  /* Grow or shrink the window, keeping the top of the screen — the way a
     terminal does, so scrolling a long `cargo` build up and then resizing does
     not throw the output away. */
  resize(cols, rows) {
    cols = Math.max(1, cols | 0); rows = Math.max(1, rows | 0);
    if (cols === this.cols && rows === this.rows) return;
    const rescale = (lines) => {
      const out = [];
      for (let r = 0; r < rows; r++) {
        const src = lines[r];
        const ln = blankLine(cols);
        if (src) {
          for (let c = 0; c < cols; c++) {
            ln.ch[c] = src.ch[c] !== undefined ? src.ch[c] : " ";
            ln.at[c] = src.at[c] !== undefined ? src.at[c] : "";
          }
        }
        out.push(ln);
      }
      return out;
    };
    this.lines = rescale(this.lines);
    if (this.alt) this.alt.lines = rescale(this.alt.lines);
    /* The scrollback is re-told at the new width rather than cut and padded
       like the screen: the rows that scrolled past are the ones the reader went
       back to read, and the tail of a wrapped line is the interesting part of
       it. A height-only change leaves it exactly as it was — those rows were
       already drawn at this width. */
    if (cols !== this.cols) { this.rewrapHist(cols); this.histRebuild = true; }
    this.cols = cols; this.rows = rows;
    this.top = 0; this.bottom = rows - 1;
    this.x = Math.min(this.x, cols - 1);
    this.y = Math.min(this.y, rows - 1);
    this.wrap = false;
  }

  /* --------------------------------------------------------- scrollback */

  /* The scrollback retold at a new width. Rows that were soft-wrapped are
     joined back into the line they were and re-split, so a line that took two
     rows at 80 columns takes one at 160 and three at 60 — which is what the
     reader who widened the pane to read it expects. A row that ended with a
     hard line break stays a row of its own, however short it is. */
  rewrapHist(cols) {
    if (!this.hist.length || this.cols === cols) return;
    const rows = [];
    let ch = [], at = [];
    for (const ln of this.hist) {
      for (let c = 0; c < this.cols; c++) {
        ch.push(ln.ch[c] !== undefined ? ln.ch[c] : " ");
        at.push(ln.at[c] !== undefined ? ln.at[c] : "");
      }
      if (!ln.cont) { splitRow(rows, ch, at, cols); ch = []; at = []; }
    }
    if (ch.length) splitRow(rows, ch, at, cols);
    this.hist = rows;
    this.histRebuild = true;
  }

  /* Everything that scrolled past, as the pane asked for it: `rebuild` when
     the whole scrollback has to be drawn again (a resize changed the width it
     is drawn at, or it was thrown away), otherwise the rows appended at the
     end and the count of rows lost off the front. That split is what lets the
     pane keep both ends cheaply, instead of redrawing two thousand rows per
     keystroke. `null` when nothing has changed since it last looked. */
  takeHist() {
    if (this.histRebuild) {
      this.histRebuild = false;
      this.histAdd.length = 0;
      this.histDrop = 0;
      return { rebuild: this.hist.map((ln) => this.rowHtml(ln, -1)) };
    }
    if (!this.histAdd.length && !this.histDrop) return null;
    const out = { add: this.histAdd.splice(0), drop: this.histDrop };
    this.histDrop = 0;
    return out;
  }

  /* One row out of the top of the scroll region and into the reader's reach —
     except while the alternate screen is up or a scroll region other than the
     whole screen is set, which is what a real terminal does and the reason
     `vim` and `htop` do not fill the scrollback with their redraws. */
  keepHist(ln) {
    /* Only a full-screen scroll is scrollback material. While the alternate
       screen is up (a `vim`, a `less`) or a scroll region short of the whole
       screen is set (`htop`'s status line), the row that leaves the top is
       somebody's redraw, not output that went past — which is precisely why
       those programs do not fill a real terminal's scrollback either. */
    if (this.altActive || this.top !== 0 || this.bottom !== this.rows - 1) return;
    this.hist.push(ln);
    this.histAdd.push(this.rowHtml(ln, -1));
    /* Past the cap the oldest rows go, counted rather than re-rendered: the
       pane drops them off the front of its own element, which is a splice and
       not a redraw of two thousand rows. The overshoot (TERM_HIST_TRIM) keeps
       the splice off the common path. */
    if (this.hist.length > TERM_HIST + TERM_HIST_TRIM) {
      const cut = this.hist.length - TERM_HIST;
      this.hist.splice(0, cut);
      this.histDrop += cut;
    }
  }

  /* The scrollback is gone (a full reset, or `CSI 3 J`, which is the sequence
     that means "and throw away the history"). */
  dropHist() {
    if (!this.hist.length && !this.histAdd.length) return;
    this.hist.length = 0;
    this.histAdd.length = 0;
    this.histDrop = 0;
    this.histRebuild = true;
  }

  /* ---------------------------------------------------------------- feed */

  /* One chunk of shell output. Kept state makes a sequence that ends at a
     chunk boundary resume on the next call rather than being drawn as text. */
  write(s) {
    for (const ch of s) {
      const code = ch.codePointAt(0);
      if (this.state === "ground") {
        if (code === 0x1b) { this.state = "esc"; continue; }
        if (code === 0x07) continue;                             /* BEL */
        if (code === 0x08) { this.backspace(); continue; }
        if (code === 0x09) { this.tab(); continue; }
        if (code === 0x0a || code === 0x0b || code === 0x0c) { this.lineFeed(); continue; }
        if (code === 0x0d) { this.x = 0; this.wrap = false; continue; }
        if (code === 0x0e || code === 0x0f) { this.charset = code === 0x0e; continue; }
        if (code < 0x20) continue;                     /* other C0: ignored */
        this.put(ch, code);
        continue;
      }
      if (this.state === "esc") {
        this.state = "ground";
        if (ch === "[") {
          this.priv = ""; this.inter = ""; this.params = "";
          this.state = "csi"; continue;
        }
        if (ch === "]") { this.osc = ""; this.state = "osc"; continue; }
        if (ch === "P" || ch === "^" || ch === "_") { this.state = "dcs"; continue; }
        if (ch === "X" || ch === "Z") { this.state = "skip2"; continue; }
        switch (ch) {
          case "7": this.save(); break;
          case "8": this.restore(); break;
          case "D": this.lineFeed(); break;
          case "E": this.x = 0; this.lineFeed(); break;
          case "M": this.reverseIndex(); break;
          case "c": this.reset(); break;
          case "=": case ">": this.appKeypad = false; break;
          case "(": case ")": case "*": case "+": case "-": case ".":
            this.state = "charset"; break;
          default: break;                              /* unknown: swallowed */
        }
        continue;
      }
      if (this.state === "skip2") { this.state = "ground"; continue; }
      if (this.state === "charset") {                    /* ESC ( 0 and friends */
        this.charset = (ch === "0");
        this.state = "ground";
        continue;
      }
      if (this.state === "osc") {
        if (ch === "\u0007") { this.oscDone(this.osc); this.state = "ground"; continue; }
        if (ch === "\u001b") { this.state = "osc_esc"; continue; }
        this.osc += ch;
        continue;
      }
      if (this.state === "osc_esc") {              /* OSC terminated by ST */
        if (ch === "\\") this.oscDone(this.osc);
        this.state = "ground";
        continue;
      }
      if (this.state === "dcs") {
        if (ch === "\u0007") { this.state = "ground"; continue; }
        if (ch === "\u001b") { this.state = "dcs_esc"; continue; }
        continue;
      }
      if (this.state === "dcs_esc") { this.state = "ground"; continue; }
      if (this.state === "csi") {
        /* xterm's own byte classes: 0x30-0x3F are parameters (which is where
           `?`, `<`, `=`, `>` and `:` live), 0x20-0x2F are intermediates (`$`
           among them — DECRQM), 0x40-0x7E end the sequence. */
        const code = ch.codePointAt(0);
        if (code >= 0x30 && code <= 0x3f) {
          if ((ch === "?" || ch === "<" || ch === "=" || ch === ">") && this.params === "" && this.inter === "") {
            this.priv = ch;
          } else {
            this.params += ch === ":" ? ";" : ch;
            /* A parameter past the point where nothing sane is left: stop
               collecting rather than grow a buffer a hostile shell chose. */
            if (this.params.length > 64) { this.state = "ground"; continue; }
          }
          continue;
        }
        if (code >= 0x20 && code <= 0x2f) { this.inter += ch; continue; }
        if (code >= 0x40 && code <= 0x7e) { this.csi(ch); this.state = "ground"; }
        else this.state = "ground";                    /* anything else cancels */
        continue;
      }
    }
    /* A chunk that ends mid-sequence leaves the parser where it was; the page
       simply sends the rest. */
  }

  /* The bytes the shell asked the terminal for (a cursor position report, a
     device attributes reply, a colour query) and that have to go back down the
     pty, or an application that queries waits for an answer that never comes. */
  takeOutput() { const r = this.reply; this.reply = ""; return r; }

  /* ------------------------------------------------------------- writing */

  put(ch, code) {
    /* The numeric test first: a combining mark never lives below U+0300, and
       the overwhelmingly common character is ASCII, so the property test (a
       regex match, per glyph) is never reached for it. */
    if (code >= 0x0300 && /\p{M}/u.test(ch)) {
      /* A mark belongs to the glyph the cursor sits on — the one it just wrote,
         or the one under it while a wrap is pending and the cursor is still on
         the last cell, which is what xterm and pyte both do. Thai tone marks
         and vowel signs stay on their consonant instead of taking a cell each. */
      const at = this.wrap ? this.x : this.x - 1;
      if (at >= 0) {
        const ln = this.lines[this.y];
        ln.ch[at] += ch;
        ln.at[at] = this.attr;
        return;
      }
    }
    if (this.charset && DEC_GRAPHICS[ch] !== undefined) ch = DEC_GRAPHICS[ch];
    if (this.wrap) {
      /* The row just filled up and this glyph is going on the next one: mark
         the row it came from as *continued*, which is the only way the
         scrollback can later tell a line that ran on from a line that ended
         with a real Enter. Without it a resize would join two separate
         commands into one. */
      const prev = this.lines[this.y];
      if (prev) prev.cont = true;
      this.x = 0; this.lineFeed(); this.wrap = false;
    }
    const ln = this.lines[this.y];
    ln.ch[this.x] = ch;
    ln.at[this.x] = this.attr;
    /* Writing here decides the row's own ending, so a redraw over a row that
       once wrapped (a prompt short enough to fit, over the wrapped long one it
       replaces) stops claiming a continuation it no longer has. */
    ln.cont = false;
    if (this.x >= this.cols - 1) this.wrap = true;    /* deferred wrap */
    else this.x++;
  }

  backspace() {
    if (this.wrap) { this.wrap = false; return; }
    if (this.x > 0) this.x--;
  }

  tab() {
    this.wrap = false;
    const next = Math.min(this.cols - 1, (Math.floor(this.x / this.tabs) + 1) * this.tabs);
    this.x = next;
  }

  lineFeed() {
    this.wrap = false;
    if (this.y === this.bottom) this.scrollUp(this.top);
    else if (this.y < this.rows - 1) this.y++;
  }

  reverseIndex() {
    this.wrap = false;
    if (this.y === this.top) this.scrollDown(this.top);
    else if (this.y > 0) this.y--;
  }

  /* Scroll one row out of the region. When the region is the whole screen the
     row that leaves the top is what the reader scrolled up to find, so it goes
     into the scrollback rather than being thrown away. */
  scrollUp(top) {
    const [t, b] = top <= this.bottom ? [top, this.bottom] : [0, this.rows - 1];
    const gone = this.lines.splice(t, 1)[0];     /* the region's top line leaves */
    if (gone && t === 0) this.keepHist(gone);
    this.lines.splice(b, 0, blankLine(this.cols));
  }

  scrollDown(top) {
    const [t, b] = top <= this.bottom ? [top, this.bottom] : [0, this.rows - 1];
    this.lines.splice(t, 0, blankLine(this.cols));
    this.lines.splice(b + 1, 1);              /* and its bottom line leaves */
  }

  blank(y) { return this.lines[y]; }

  eraseRange(y, from, to) {
    const ln = this.blank(y);
    for (let c = from; c <= to && c < this.cols; c++) {
      if (c < 0) continue;
      ln.ch[c] = " ";
      ln.at[c] = this.attr;      /* EDA/EL use the current background, as xterm does */
    }
  }

  /* Erase in line: 0 to the end, 1 to the start, 2 the whole line. readline's
     own redraw is a CR plus this, so the completion menu that replaces what you
     typed depends on it being right. */
  eraseLine(mode) {
    if (mode === 1) this.eraseRange(this.y, 0, this.x);
    else if (mode === 2) this.eraseRange(this.y, 0, this.cols - 1);
    else this.eraseRange(this.y, this.x, this.cols - 1);
  }

  /* Erase in display, same three directions over the whole screen. */
  eraseDisplay(mode) {
    if (mode === 1) {
      this.eraseLine(1);
      for (let r = 0; r < this.y; r++) this.eraseRange(r, 0, this.cols - 1);
    } else if (mode === 2 || mode === 3) {
      for (let r = 0; r < this.rows; r++) this.eraseRange(r, 0, this.cols - 1);
      /* xterm's 3 also clears the scrollback, which is the one erase a shell
         can be *asked* to do (`clear` on many of them, `reset` on all) and the
         one the reader would notice missing. */
      if (mode === 3) { this.wrap = false; this.dropHist(); }
    } else {
      this.eraseLine(0);
      for (let r = this.y + 1; r < this.rows; r++) this.eraseRange(r, 0, this.cols - 1);
    }
  }

  insertLines(n) {
    if (this.y < this.top || this.y > this.bottom) return;
    for (let i = 0; i < n; i++) {
      this.lines.splice(this.y, 0, blankLine(this.cols));
      this.lines.splice(this.bottom + 1, 1);
    }
    this.forgetWrap(this.top, this.bottom);
    this.wrap = false;
  }

  deleteLines(n) {
    if (this.y < this.top || this.y > this.bottom) return;
    for (let i = 0; i < n; i++) {
      this.lines.splice(this.y, 1);
      this.lines.splice(this.bottom + 1, 0, blankLine(this.cols));
    }
    this.forgetWrap(this.top, this.bottom);
    this.wrap = false;
  }

  /* Rows the program has just shuffled cannot be trusted to say what continues
     what: `cont` is a claim about the row *below* this one, and line
     insertion/deletion moves rows relative to each other. Better to lose a
     join in the scrollback's re-wrap than to weld two unrelated lines together,
     so the claim is dropped over the region that moved. */
  forgetWrap(from, to) {
    for (let r = from; r <= to && r < this.lines.length; r++) this.lines[r].cont = false;
  }

  /* ------------------------------------------------------------ sequences */

  /* CSI parameters, with the defaults an absent parameter means. A parameter
     of exactly 0 is kept as 0, because the erase modes read it as a direction;
     the places where 0 has to mean 1 clamp it themselves. */
  p(i, dflt) {
    const raw = this.params.split(";")[i];
    if (raw === undefined || raw === "") return dflt;
    const n = parseInt(raw, 10);
    return Number.isFinite(n) && n > 0 ? n : (n === 0 ? 0 : dflt);
  }

  csi(final) {
    /* Anything with a private marker the page does not implement is dropped
       rather than echoed; with an intermediate it is usually a query this page
       answers as "no" or does not answer at all. */
    const priv = this.priv, inter = this.inter;
    if (inter === "$") {
      /* DECRQM: "is mode N set?" Answer not-recognised for everything the page
         does not implement, which is the truthful answer and unblocks the
         applications that ask (fish asks about synchronized output). */
      if (final === "p") this.reply += "\u001b[?" + this.p(0, 0) + ";0$y";
      return;
    }
    if (priv === "?") {
      if (final === "h" || final === "l") { this.decMode(final === "h"); return; }
      if (final === "u") {                      /* kitty keyboard protocol query */
        this.reply += "\u001b[?0u";
        return;
      }
      if (final === "r") { this.top = 0; this.bottom = this.rows - 1; return; }
      return;
    }
    if (priv === ">") {
      if (final === "c") { this.reply += "\u001b[>1;1985;0c"; return; }   /* secondary DA */
      if (final === "q") {                    /* XTVERSION: who am I */
        this.reply += "\u001bP>|jonnyq console v"##,
    env!("CARGO_PKG_VERSION"),
    r##"\u001b\\";
        return;
      }
      return;
    }
    switch (final) {
      case "m": this.sgr(); return;
      case "H": case "f":
        this.y = Math.min(this.rows, Math.max(1, this.p(0, 1))) - 1;
        this.x = Math.min(this.cols, Math.max(1, this.p(1, 1))) - 1;
        this.wrap = false;
        return;
      case "A": this.y = Math.max(this.top, this.y - Math.max(1, this.p(0, 1))); this.wrap = false; return;
      case "B": this.y = Math.min(this.bottom, this.y + Math.max(1, this.p(0, 1))); this.wrap = false; return;
      case "C": this.x = Math.min(this.cols - 1, this.x + Math.max(1, this.p(0, 1))); this.wrap = false; return;
      case "D": this.x = Math.max(0, this.x - Math.max(1, this.p(0, 1))); this.wrap = false; return;
      case "E": this.x = 0; this.y = Math.min(this.bottom, this.y + Math.max(1, this.p(0, 1))); return;
      case "F": this.x = 0; this.y = Math.max(this.top, this.y - Math.max(1, this.p(0, 1))); return;
      case "G": this.x = Math.min(this.cols - 1, Math.max(1, this.p(0, 1)) - 1); this.wrap = false; return;
      case "d": this.y = Math.min(this.rows, Math.max(1, this.p(0, 1))) - 1; this.wrap = false; return;
      case "J": this.eraseDisplay(this.p(0, 0)); return;
      case "K": this.eraseLine(this.p(0, 0)); return;
      case "L": this.insertLines(Math.max(1, this.p(0, 1))); return;
      case "M": this.deleteLines(Math.max(1, this.p(0, 1))); return;
      case "P": {                                     /* delete chars */
        const n = Math.max(1, this.p(0, 1)), ln = this.blank(this.y);
        for (let c = this.x; c < this.cols; c++) {
          const src = c + n;
          ln.ch[c] = src < this.cols ? ln.ch[src] : " ";
          ln.at[c] = src < this.cols ? ln.at[src] : "";
        }
        for (let c = Math.max(0, this.cols - n); c < this.cols; c++) { ln.ch[c] = " "; ln.at[c] = ""; }
        this.wrap = false;
        return;
      }
      case "@": {                                     /* insert blanks */
        const n = Math.max(1, this.p(0, 1)), ln = this.blank(this.y);
        for (let c = this.cols - 1; c >= this.x; c--) {
          const src = c - n;
          ln.ch[c] = src >= this.x ? ln.ch[src] : " ";
          ln.at[c] = src >= this.x ? ln.at[src] : "";
        }
        this.wrap = false;
        return;
      }
      case "X": {                                     /* erase chars */
        const n = Math.max(1, this.p(0, 1));
        this.eraseRange(this.y, this.x, this.x + n - 1);
        return;
      }
      case "r": {                                     /* DECSTBM */
        const t = Math.max(1, this.p(0, 1)) - 1;
        const b = Math.min(this.rows, Math.max(1, this.p(1, this.rows))) - 1;
        if (t < b) { this.top = t; this.bottom = b; this.x = 0; this.y = this.top; }
        return;
      }
      case "s": this.save(); return;
      case "u": this.restore(); return;
      case "h": case "l": return;                     /* non-private modes: ignored */
      case "n": {                                     /* DSR */
        if (this.p(0, 0) === 6) this.reply += "\u001b[" + (this.y + 1) + ";" + (this.x + 1) + "R";
        else if (this.p(0, 0) === 5) this.reply += "\u001b[0n";
        return;
      }
      case "c": this.reply += "\u001b[?62;22c"; return;   /* primary DA, VT220 */
      case "t": return;                               /* window ops: ignored */
      default: return;
    }
  }

  decMode(on) {
    const want = (n) => this.params.split(";").includes(String(n));
    /* Every mode this page understands is answered here; the ones it does not
       are accepted and ignored, so an application that sets one is not left
       waiting for a mode report. */
    if (want(25)) this.visible = on;
    else if (want(1)) this.decckm = on;
    else if (want(2004) || want(2000)) this.bracketed = on;
    else if (want(1049) || want(1047) || want(47)) this.altScreen(on, want(1049));
    else if (want(2026)) this.sync = on;             /* synchronized output */
  }

  /* The alternate screen: `vim`, `less`, `man` and `htop` draw over it and come
     back to what was there. `1049` also clears on entry and restores the cursor. */
  altScreen(on, clears) {
    if (on && !this.altActive) {
      this.alt = {
        lines: this.lines, top: this.top, bottom: this.bottom,
        attr: this.attr, saved: this.saved,
      };
      this.altActive = true;
      this.lines = [];
      for (let r = 0; r < this.rows; r++) this.lines.push(blankLine(this.cols));
      if (clears) { this.x = 0; this.y = 0; this.attr = ""; }
      this.top = 0; this.bottom = this.rows - 1;
      this.wrap = false;
    } else if (!on && this.altActive) {
      this.lines = this.alt.lines;
      this.top = this.alt.top; this.bottom = this.alt.bottom;
      this.attr = this.alt.attr; this.saved = this.alt.saved;
      if (this.lines.length !== this.rows) {
        this.lines = [];
        for (let r = 0; r < this.rows; r++) this.lines.push(blankLine(this.cols));
      }
      this.altActive = false;
      this.alt = null;
      this.x = Math.min(this.x, this.cols - 1);
      this.y = Math.min(this.y, this.rows - 1);
      this.wrap = false;
    }
  }

  oscDone(body) {
    /* Colour queries (`OSC 10 ?` asks for the foreground colour) have to be
       answered, or a shell that checks before painting waits; every other OSC
       is a title or a hyperlink and is dropped. */
    const q = body.match(/^(\d+);\?$/);
    if (!q) return;
    const n = +q[1];
    const rgb = (hex) => {
      const v = hex.replace("#", "");
      return "rgb:" + v.slice(0, 2) + "/" + v.slice(2, 4) + "/" + v.slice(4, 6);
    };
    if (n === 10) this.reply += "\u001b]10;" + rgb("#ebdbb2") + "\u001b\\";
    else if (n === 11) this.reply += "\u001b]11;" + rgb("#1d2021") + "\u001b\\";
    else if (n === 4 || n === 12) this.reply += "\u001b]" + n + ";?\u001b\\";
  }

  save() { this.saved = { x: this.x, y: this.y, attr: this.attr }; }

  restore() {
    if (!this.saved) return;
    this.x = Math.min(this.saved.x, this.cols - 1);
    this.y = Math.min(this.saved.y, this.rows - 1);
    this.attr = this.saved.attr;
    this.wrap = false;
  }

  /* ----------------------------------------------------------------- SGR */
  sgr() {
    const raw = this.params === "" ? ["0"] : this.params.split(";");
    const codes = raw.map((c) => (c === "" ? 0 : +c));
    let a = parseAttr(this.attr);
    for (let i = 0; i < codes.length; i++) {
      const c = codes[i];
      if (c === 0) a = { b: 0, dim: 0, i: 0, u: 0, inv: 0, strike: 0, fg: "", bg: "" };
      else if (c === 1) a.b = 1;
      else if (c === 2) a.dim = 1;
      else if (c === 3) a.i = 1;
      else if (c === 4) a.u = 1;
      else if (c === 5 || c === 6) a.blink = 1;
      else if (c === 7) a.inv = 1;
      else if (c === 8) a.strike = 1;
      else if (c === 22) { a.b = 0; a.dim = 0; }
      else if (c === 23) a.i = 0;
      else if (c === 24) a.u = 0;
      else if (c === 25) a.blink = 0;
      else if (c === 27) a.inv = 0;
      else if (c === 28) a.strike = 0;
      else if (c === 39) a.fg = "";
      else if (c === 49) a.bg = "";
      else if (c === 38 || c === 48) {
        const key = c === 38 ? "fg" : "bg";
        const mode = codes[i + 1];
        if (mode === 5 && codes.length > i + 2) {
          const n = codes[i + 2];
          /* 0..15 are the colours the page already themes, so they go through
             the same classes the rest of the console uses — a program asking
             for red gets gruvbox red, not the xterm default. */
          a[key] = n < 16 ? "c" + (n < 8 ? 30 + n : 90 + n - 8) : "p" + n;
          i += 2;
        } else if (mode === 2 && codes.length > i + 4) {
          const [r, g, b] = [codes[i + 2], codes[i + 3], codes[i + 4]];
          a[key] = "#" + [r, g, b].map((x) => (x & 255).toString(16).padStart(2, "0")).join("");
          i += 4;
        }
      } else if (c >= 30 && c <= 37) a.fg = "c" + c;
      else if (c >= 40 && c <= 47) a.bg = "c" + (c + 10);
      else if (c >= 90 && c <= 97) a.fg = "c" + c;
      else if (c >= 100 && c <= 107) a.bg = "c" + c;
    }
    this.attr = attrKey(a);
  }

  /* -------------------------------------------------------------- render */

  /* One row as HTML: cells grouped into a span per run of equal attributes,
     escaped so shell output can never be markup. `cursor` is the column the
     block sits on, or -1 for a row no cursor is on — which is every row of the
     scrollback, since there is only ever one cursor and it is on the screen. */
  rowHtml(ln, cursor) {
    let out = "";
    let c = 0;
    while (c < this.cols) {
      const here = c === cursor;
      const a = ln.at[c];
      let j = c;
      const text = [];
      /* One run: equal attributes, and the cursor's cell always stands alone
         so its block can be drawn over exactly one glyph. */
      while (j < this.cols && ln.at[j] === a) {
        const atCursor = j === cursor;
        if (atCursor !== here) break;
        text.push(ln.ch[j]);
        j++;
        if (here) break;
      }
      const body = esc(text.join(""));
      const cls = classesFor(a, here);
      const style = styleFor(a);
      if (body === "" && !cls && !style) { /* nothing to draw */ }
      else if (cls || style) {
        out += "<span" + (cls ? ' class="' + cls + '"' : "") +
               (style ? ' style="' + style + '"' : "") + ">" + body + "</span>";
      } else out += body;
      c = j;
    }
    return out;
  }

  /* The screen as HTML: one line per row, the cursor drawn as a block over the
     glyph it sits on. The scrollback is not here — it is drawn once per row
     into its own element by `takeHist`, because it changes one row at a time
     and repainting two thousand rows per keystroke is not a frame. */
  render() {
    let out = "";
    for (let r = 0; r < this.lines.length; r++) {
      if (r) out += "\n";
      const cursor = r === this.y && this.visible ? this.x : -1;
      out += this.rowHtml(this.lines[r], cursor);
    }
    return out;
  }

  /* Plain text, for a test or for copying the pane: what is on the screen. The
     scrollback is deliberately not in here — `histText()` is the one that
     reaches above the top, and a test that reads the screen wants the screen. */
  text() {
    return this.lines.map((ln) => ln.ch.join("").replace(/\s+$/, "")).join("\n").replace(/\n+$/, "");
  }

  /* The scrollback as text, oldest row first: what the reader who scrolled up
     is looking at. Wrapped rows are joined, so a line that took two rows reads
     as the line it was. */
  histText() {
    const rows = [];
    let line = "";
    for (const ln of this.hist) {
      line += ln.ch.join("").replace(/\s+$/, "");
      if (!ln.cont) { rows.push(line); line = ""; }
    }
    if (line) rows.push(line);
    return rows.join("\n");
  }
}

/* A logical line's cells split into rows of `cols`, the way the screen splits
   one: the last row is kept even when it is empty, because a line that ends
   short still occupies the row it was written on. */
function splitRow(rows, ch, at, cols) {
  if (!ch.length) { rows.push(blankLine(cols)); return; }
  for (let i = 0; i < ch.length; i += cols) {
    const ln = blankLine(cols);
    for (let c = 0; c < cols; c++) {
      ln.ch[c] = ch[i + c] !== undefined ? ch[i + c] : " ";
      ln.at[c] = at[i + c] !== undefined ? at[i + c] : "";
    }
    ln.cont = i + cols < ch.length;
    rows.push(ln);
  }
}

function parseAttr(key) {
  const a = { b: 0, dim: 0, i: 0, u: 0, inv: 0, strike: 0, blink: 0, fg: "", bg: "" };
  if (!key) return a;
  for (const t of key.split(" ")) {
    if (t === "b") a.b = 1; else if (t === "d") a.dim = 1;
    else if (t === "i") a.i = 1; else if (t === "u") a.u = 1;
    else if (t === "v") a.inv = 1; else if (t === "s") a.strike = 1;
    else if (t === "k") a.blink = 1;
    else if (t.startsWith("f")) a.fg = t.slice(1);
    else if (t.startsWith("g")) a.bg = t.slice(1);
  }
  return a;
}

/* A stable key for one set of attributes, so equal attributes compare equal and
   the renderer can group runs. */
function attrKey(a) {
  const bits = [];
  if (a.b) bits.push("b");
  if (a.dim) bits.push("d");
  if (a.i) bits.push("i");
  if (a.u) bits.push("u");
  if (a.inv) bits.push("v");
  if (a.strike) bits.push("s");
  if (a.blink) bits.push("k");
  if (a.fg) bits.push("f" + a.fg);
  if (a.bg) bits.push("g" + a.bg);
  return bits.join(" ");
}

/* Classes for the attributes, from the page's own palette; `pN` is a 256-colour
   index the page has no class for, and `styleFor` turns it into a colour. */
function classesFor(attr, cursor) {
  const a = parseAttr(attr);
  const cls = [];
  if (a.b) cls.push("c1");
  if (a.i) cls.push("ti");
  if (a.u) cls.push("tu");
  if (a.strike) cls.push("ts");
  if (a.inv || cursor) cls.push("tv");
  if (a.fg && a.fg.startsWith("c")) cls.push(a.fg);
  if (a.bg && a.bg.startsWith("c")) cls.push("b" + a.bg);
  return cls.join(" ");
}

function styleFor(attr) {
  const a = parseAttr(attr);
  const out = [];
  const colour = (v, prop) => {
    if (!v || v.startsWith("c")) return;
    if (v.startsWith("p")) { const h = xterm256(+v.slice(1)); if (h) out.push(prop + ":" + h); return; }
    if (v.startsWith("#")) out.push(prop + ":" + v);
  };
  if (a.fg) colour(a.fg, "color");
  if (a.bg) colour(a.bg, "background-color");
  if (a.dim) out.push("opacity:.6");
  return out.join(";");
}


/* ------------------------------------------------------------- terminals
   A terminal tab is a PTY on the server with an interactive shell in it, and
   this is the window onto it. Bytes travel both ways over the console's own
   websocket: shell output arrives as binary frames (output is arbitrary bytes
   and a JSON string is UTF-8-only), and everything else — open, keystrokes,
   resize, close — is JSON like the rest of the protocol. See
   [`crate::web::term`] for the other end.

   Three things make this more than a text box:

   * the *screen*, because PTY output is cursor movement and erase, not lines
     (`TermScreen` above);
   * the *size*, because a terminal has to tell the shell how big it is before
     `vim` wraps a line wrong — measured from the pane's own pixels, in the
     font's own cell width, so the grid and the CSS cannot disagree;
   * the *answers*, because a program that asks the terminal a question (where
     is the cursor, what colours do you use, do you support this mode) waits
     for a reply. `TermScreen` collects those and `termFlush` sends them back. */

/* A tab's shell, by the id the server gave it: one websocket carries every
   terminal tab, and a binary output frame names the shell it belongs to. */
const termTabs = new Map();

/* Longest paste accepted in one message, matching the server's cap. A paste
   bigger than this is sent in pieces rather than refused. */
const TERM_MAX_WRITE = 64 * 1024;

/* How often the pane is looked at for a size change. ResizeObserver does the
   real work where the browser has it; this is the fallback for one that does
   not, and the interval is deliberately slow because it is a layout read. */
const TERM_FIT_POLL = 400;

/* Everything a pane needs that is not the shell: its screen, its size as last
   told to the shell, and the handles to let go of when the tab closes. Called
   from `makeTab`, so a tab is wired before anything can happen to it. */
function wireTerm(t) {
  const out = t.el.querySelector(".term-out");
  const ta = t.el.querySelector(".term-input");
  const me = t.term = {
    session: null,        /* the server's id, until `term_open` answers */
    screen: new TermScreen(80, 24),
    cols: 0, rows: 0,     /* what the shell was last told */
    dead: false,
    pending: "",          /* bytes owed to the shell (a query reply) */
    queue: "",            /* bytes for the shell, waiting on its id */
    out: out,
    grid: t.el.querySelector(".term-grid"),
    hist: t.el.querySelector(".term-hist"),
    size: t.el.querySelector(".term-size"),
    input: ta,
    dirty: false,
    /* The pane's own units: how tall one row of the grid is in pixels, and how
       many rows of scrollback are in the document above it. Both are needed to
       talk about the reader's position in rows rather than in pixels, which is
       the only unit that survives a font change. */
    cellH: 0,
    histRows: 0,
  };
  /* The textarea is what has focus — a browser only starts an IME composition,
   honours a layout's dead keys, or raises a soft keyboard on a phone when there
   is a text field to type into, and the pane it belongs to is what gets clicked.
   It sits beside the pane rather than inside it because an element inside a
   scrolling container has the pane's scroll yanked to it when focused. */
  ta.addEventListener("keydown", (e) => termKey(t, e));
  ta.addEventListener("compositionstart", () => { termComposing = true; });
  ta.addEventListener("compositionend", () => { termComposing = false; });
  ta.addEventListener("input", (e) => termInput(t, e));
  ta.addEventListener("paste", (e) => termPaste(t, e));
  ta.addEventListener("copy", (e) => termCopy(t, e));
  ta.addEventListener("blur", () => { termComposing = false; });
  out.addEventListener("mousedown", (e) => {
    /* A middle-click paste of the X selection arrives as a `paste` event on
       Firefox and as nothing at all on Chrome; keeping the input focused is
       the part that is always in our hands. */
    if (e.shiftKey) return;
    /* A press on the pane's own scrollbar belongs to the scrollbar. The bar is
       drawn inside the element's box, so it is this same handler that sees the
       press, and cancelling it cancels the drag before it starts — leaving a
       scroll bar that is visible and a pane that only the wheel can move.
       `offsetX/Y` are measured against the padding box, so anything past the
      client width/height is the bar rather than the text. */
    if (e.offsetX >= out.clientWidth || e.offsetY >= out.clientHeight) return;
    e.preventDefault();
    ta.focus();
  });
  /* Every click in the pane means "I am typing into this shell now". */
  t.el.addEventListener("focusin", () => { if (!t.term.dead) ta.focus(); });
  t.el.querySelector(".new").addEventListener("click", () => termOpen(t.path));
  t.el.querySelector(".kill").addEventListener("click", () => termKill(t));
  if (window.ResizeObserver) {
    const ro = new ResizeObserver(() => termFit(t));
    ro.observe(out);
    t.term.ro = ro;
  } else {
    t.term.timer = setInterval(() => termFit(t), TERM_FIT_POLL);
  }
}

/* Open a shell, in the working directory or in the folder asked for (a terminal
   opened from a folder in the file tree). */
function termOpen(path) {
  const t = makeTab("terminal", "term", path || null);
  select(t.id);
  termStart(t);
  return t;
}

/* Another shell in a tab whose shell has exited: the same tab, the same pane,
   the scrollback kept (what the last command printed is still worth reading),
   and a screen cleared by the new shell's own prompt rather than by this page
   deciding what to erase. */
function termRestart(t) {
  const me = t.term;
  if (!me || !me.dead) return;
  me.dead = false;
  me.session = null;
  me.queue = "";
  me.pending = "";
  t.tab.classList.remove("dead");
  if (me.note) { me.note.remove(); me.note = null; }
  termStart(t);
}

async function termStart(t) {
  const me = t.term;
  try {
    const m = await rpc({ type: "term_open", path: t.path || "" });
    if (!m.ok) { termDead(t, m.error || "cannot start a shell"); return; }
    me.session = m.session;
    termTabs.set(m.session, t);
    /* Whatever the shell asked for before it had a name, sent now. */
    if (me.queue) { const q = me.queue; me.queue = ""; termSend(t, q); }
    t.path = m.dir;
    t.el.querySelector(".dir").textContent = m.dir || "";
    t.el.querySelector(".dir").title = m.dir || "";
    t.el.querySelector(".shell").textContent = m.shell || "";
    t.tab.querySelector(".name").textContent = "sh:" + shortDir(m.dir);
    termDraw(t);
    termFit(t, true);
    const out = t.el.querySelector(".term-out");
    out.focus();
    /* A shell that is already gone by the time the answer lands (a `$SHELL`
       that exits immediately) has nothing else to say, so say it here. */
    if (me.dead) return;
  } catch (e) {
    termDead(t, "cannot start a shell: " + (e && e.message ? e.message : e));
  }
}

function shortDir(d) {
  if (!d) return "?";
  const parts = String(d).replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || d;
}

/* Tell the shell how big its window is, from the pane's own pixels. `force`
   re-sends the size the shell already has, which is what a tab that was just
   shown needs: a hidden pane measures 0, and the size sent before then was
   measured from nothing. */
function termFit(t, force) {
  const me = t.term;
  if (!me || me.dead) return;
  const out = t.el.querySelector(".term-out");
  const size = me.size;
  /* The cell's width and height, measured in the pane's own font: 80 of one
     glyph (a long word, so no kerning or line-break rules interfere) and 8
     lines of the same leading. `scrollWidth` of a `white-space: pre` span is
     the exact advance of those 80 glyphs. */
  size.textContent = "M".repeat(80) + "\n".repeat(8) + "M";
  const cs = getComputedStyle(out);
  const w = size.scrollWidth / 80;
  const h = size.scrollHeight / 9;
  if (!(w > 0) || !(h > 0)) return;
  /* The row height in pixels, which is what the scrollback's rows are `high`
     in: the reader's position and the correction for a row entering or leaving
     the history are both counted in rows, so the pane needs the one number that
     turns one into the other. Measured here rather than from the grid, because
     this is the code that already measures the cell. */
  me.cellH = h;
  const padX = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
  const padY = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom);
  /* The scroll bar takes room the grid cannot use, and it is only there once
     the content overflows — hence clientWidth rather than offsetWidth. */
  const cols = Math.max(20, Math.floor((out.clientWidth - padX) / w));
  const rows = Math.max(4, Math.floor((out.clientHeight - padY) / h));
  if (cols === me.cols && rows === me.rows && !force) return;
  me.cols = cols; me.rows = rows;
  me.screen.resize(cols, rows);
  termDraw(t);
  if (!me.session) return;
  rpc({ type: "term_resize", session: me.session, cols, rows })
    .catch(() => {});        /* a shell that went in the meantime is reported by term_exit */
}

/* Queue bytes for the shell. */
function termFeed(t, bytes) {
  const me = t.term;
  me.pending += bytes;
  if (!me.flush) me.flush = setTimeout(() => termFlush(t), 0);
}

/* Send what the screen collected. A query answer goes as one `term_in` like any
   other input, because to the pty it is exactly that: the bytes a terminal
   would have put back on the wire. */
function termFlush(t) {
  const me = t.term;
  me.flush = null;
  if (!me.pending) return;
  const data = me.pending;
  me.pending = "";
  if (me.dead || !me.session) return;
  termSend(t, data);
}

/* Bytes for the shell.

   A program that queries the terminal does it in its first microseconds, which
   can be before the `term_open` reply that carries the shell's id has landed.
   Those bytes are queued rather than dropped, and flushed by `termStart`: a
   page that threw the question away would leave the program waiting for an
   answer only this page can give. */
const TERM_QUEUE_MAX = 256 * 1024;

function termSend(t, str) {
  const me = t.term;
  if (!me || me.dead) return;
  if (!me.session) {
    if (me.queue.length < TERM_QUEUE_MAX) me.queue += str;
    return;                       /* termStart flushes it when the id arrives */
  }
  const bytes = new TextEncoder().encode(str);
  /* A paste longer than one message is sent in pieces, in order, rather than
     refused: the shell reads a stream and cannot tell the difference. */
  for (let i = 0; i < bytes.length; i += TERM_MAX_WRITE) {
    const part = bytes.subarray(i, Math.min(bytes.length, i + TERM_MAX_WRITE));
    let bin = "";
    for (let j = 0; j < part.length; j++) bin += String.fromCharCode(part[j]);
    rpc({ type: "term_in", session: me.session, data: btoa(bin) }).catch(() => {});
  }
}

/* Keystrokes. The rule is what a terminal emulator does: a printable character
   is sent as UTF-8 text and everything else is a control byte or an escape
   sequence, with the *modifier* deciding which — because Alt-2 is a character
   on some layouts and Meta-2 is a tab everywhere, and `e.key` knows the
   difference in a way `e.keyCode` never did. */
function termKey(t, e) {
  const me = t.term;
  if (me.dead) {
    /* Enter in a pane whose shell has exited starts another one, the way
       pressing Return at a closed terminal window does. */
    if (e.key === "Enter") { e.preventDefault(); termRestart(t); }
    return;
  }
  /* The page's own Alt-<n> tab switch wins over the shell wanting Alt-1. */
  if (e.altKey && (e.ctrlKey || e.metaKey)) return;
  if (e.altKey && !e.ctrlKey && !e.metaKey && /^[0-9]$/.test(e.key)) return;
  if (e.key === "Tab") { e.preventDefault(); termSend(t, "\t"); termDone(t); return; }
  if (e.key === "Enter") {
    /* The IME guard is not decoration: with Thai input, Enter commits a
       composition, and sending a CR then would run a half-typed command. */
    if (e.isComposing || termComposing) { e.preventDefault(); return; }
    e.preventDefault();
    termSend(t, e.ctrlKey || e.altKey ? "\x1b\r" : "\r");
    termDone(t);
    return;
  }
  if (e.key === "Backspace") { e.preventDefault(); termSend(t, "\x7f"); termDone(t); return; }
  if (e.key === "Escape") { e.preventDefault(); termSend(t, "\x1b"); termDone(t); return; }
  /* Shift-PgUp / Shift-PgDn is the one keyboard route to the scrollback, and
     it is the convention every desktop terminal uses (the plain keys stay the
     shell's, because `less` and `man` are the reason anyone presses them). */
  if (e.shiftKey && (e.key === "PageUp" || e.key === "PageDown")) {
    e.preventDefault();
    termScroll(me, (e.key === "PageUp" ? -1 : 1) * Math.max(1, me.rows - 2));
    return;
  }
  /* The names are the DOM's (`ArrowUp`, not `Up`) — a map keyed by what the
     escape sequence wants, not by what the browser calls the key, is a map
     that silently sends nothing for every arrow. */
  const arrow = { ArrowUp: "A", ArrowDown: "B", ArrowRight: "C", ArrowLeft: "D",
                  Home: "H", End: "F", PageUp: "5~", PageDown: "6~",
                  Insert: "2~", Delete: "3~" }[e.key];
  if (arrow) {
    e.preventDefault();
    termSend(t, termCsi(arrow, e, me.screen.decckm));
    termDone(t);
    return;
  }
  if (e.key === "F1" || e.key === "F2" || e.key === "F3" || e.key === "F4") {
    e.preventDefault();
    const app = me.screen.decckm ? "\x1bO" : "\x1b[";
    termSend(t, app + "PQRS"[+e.key[1] - 1]);
    termDone(t);
    return;
  }
  /* A selection and Ctrl-C means copy, which is what the pane's `copy` handler
     does; with no selection the same keys are the shell's interrupt. */
  if (e.ctrlKey && (e.key === "c" || e.key === "C") && String(window.getSelection())) {
    return;
  }
  if (e.ctrlKey && !e.altKey && !e.metaKey && e.key.length === 1) {
    /* Ctrl-C, Ctrl-D, Ctrl-R and the rest are control bytes: the same thing the
       terminal sends, so the line discipline turns Ctrl-C into SIGINT for the
       foreground group exactly as it does at a real terminal. */
    const c = e.key.toUpperCase().charCodeAt(0);
    if (c >= 64 && c <= 95) {
      e.preventDefault();
      termSend(t, String.fromCharCode(c === 63 ? 127 : c & 0x1f));
      termDone(t);
      return;
    }
  }
  if (e.ctrlKey || e.metaKey) return;      /* the browser's (Ctrl-R reload and friends) */
  if (e.key.length > 1) return;            /* a named key with no mapping */
  if (e.altKey) {
    /* Alt plus a character is escape-then-character, the convention every
       terminal uses and what `M-x` means in an editor that has one. AltGr —
       Ctrl-Alt on the layouts that need it for real characters — is excluded
       above, so this cannot eat the character AltGr is there to produce. */
    e.preventDefault();
    termSend(t, "\x1b" + e.key);
    termDone(t);
    return;
  }
  /* Printable keys are not sent from keydown at all: the field's own `input`
     event carries exactly the same character, and it is the one event that
     also fires for a dead key or a composed one. Sending here as well would
     type every ordinary letter twice. */
  /* Nothing else is preventDefault()ed, so the field keeps collecting what the
     layout produced and `input` sends it. */
}

function termCsi(code, e, decckm) {
  /* xterm's modifier encoding: Shift 2, Alt 4, Ctrl 8, plus one. It is what
     makes Shift-Right a selection and Ctrl-Right a word jump in programs that
     ask for it, and harmless in the ones that do not. */
  const mod = 1 + (e.shiftKey ? 1 : 0) + (e.altKey ? 4 : 0) + (e.ctrlKey ? 8 : 0);
  if (code.endsWith("~")) {
    /* PageUp/PageDown/Insert/Delete: `CSI n ~`, with the modifier as the second
       parameter. */
    return mod === 1 ? "\x1b[" + code : "\x1b[" + code.slice(0, -1) + ";" + mod + "~";
  }
  /* Arrows and Home/End: `ESC O` in application cursor keys (DECCKM, which
     `vim` and `less` switch on and expect the keys to follow) and `ESC [`
     otherwise. A cursor key that ignores DECCKM is a `vim` whose arrow keys
     leave it in normal mode. */
  const csi = decckm ? "\x1bO" : "\x1b[";
  return mod === 1 ? csi + code : "\x1b[1;" + mod + code;
}

/* Whether an IME is composing right now, across the whole page: only one
   surface can hold a composition at a time, and a tab that lost focus has lost
   it with it. */
let termComposing = false;

/* What the invisible field collected. On a normal keystroke this is the
   character the layout produced (identical to what `keydown` sends, so it is
   thrown away); on an IME composition or a dead key it is the only place the
   committed string appears — a Thai word chosen from a candidate list arrives
   here and nowhere else. */
function termInput(t, e) {
  const me = t.term;
  const added = String(e.data || me.input.value || "");
  me.input.value = "";
  if (me.dead || !added) return;
  termComposing = false;
  termSend(t, added);
  termDone(t);
}

function termDone(t) {
  const out = t.el.querySelector(".term-out");
  out.scrollTop = out.scrollHeight;         /* typing always shows what you typed */
}

/* Nudge the pane through its scrollback, in rows rather than pixels: a page
   that scrolls by a fixed number of pixels moves a different amount of text
   every time the code face or size changes, and the reader is asking for lines. */
function termScroll(me, rows) {
  const out = me.out;
  if (!out) return;
  const step = rows * (me.cellH > 0 ? me.cellH : 16);
  out.scrollTop = Math.max(0, Math.min(out.scrollHeight - out.clientHeight,
                                       out.scrollTop + step));
}

function termPaste(t, e) {
  const text = (e.clipboardData || window.clipboardData).getData("text");
  if (!text) return;
  e.preventDefault();
  const me = t.term;
  if (me.dead) return;
  /* Bracketed paste is a promise the shell asked for: the pasted text arrives
     between the markers so a shell can decide not to run it the way it would
     run typed keys. Without the mode, the text is typed as-is. */
  termSend(t, me.screen.bracketed ? "\x1b[200~" + text + "\x1b[201~" : text);
  termDone(t);
}

function termCopy(t, e) {
  const sel = String(window.getSelection());
  if (!sel) return;
  e.preventDefault();
  e.clipboardData.setData("text/plain", sel);
}

/* Ctrl-C belongs to the shell while the pane has focus, and to nothing while it
   does not. The copy case is the one that has to keep working, and it does: a
   selection means the user is not typing, and the pane's own keydown never
   sees Ctrl-C (it is not a printable key and the handler returns early on
   ctrlKey for anything but a control byte). */
function termKill(t) {
  const me = t.term;
  if (!me || !me.session) return;
  rpc({ type: "term_close", session: me.session }).catch(() => {});
}

/* The tab is going: the shell goes with it, now rather than at the server's
   next poll of a connection that is still open because other tabs are using it. */
function termClose(t) {
  const me = t.term;
  if (!me) return;
  if (me.ro) me.ro.disconnect();
  if (me.timer) clearInterval(me.timer);
  if (me.flush) clearTimeout(me.flush);
  if (me.note) { me.note.remove(); me.note = null; }
  if (me.session) {
    termTabs.delete(me.session);
    rpc({ type: "term_close", session: me.session }).catch(() => {});
  }
}

/* The shell is gone. The tab stays — it is the record that there *was* a shell
   there, and the pane still shows what it was showing when it went. */
function termDead(t, why) {
  const me = t.term;
  if (me.dead) return;
  me.dead = true;
  if (me.session) termTabs.delete(me.session);
  me.session = null;
  t.tab.classList.add("dead");
  const head = t.el.querySelector(".term-head");
  head.querySelector(".shell").textContent = "";
  const note = document.createElement("span");
  note.className = "gone";
  note.textContent = (why || "shell exited") + " — Enter for another";
  head.querySelector(".spacer").after(note);
  me.note = note;
  /* Keystrokes belong to nobody now, so the pane stops looking like a target. */
  if (me.input) me.input.blur();
  termDraw(t);
}

/* One output frame: `[ id length ][ session id ][ output ]`. A frame whose
   length byte lies, or that names a tab that has closed, is dropped — there is
   nothing to answer it with, and a shell that outlived its tab is reported by
   the server rather than by this page. */
function termFrame(buf) {
  const all = new Uint8Array(buf);
  if (!all.length) return;
  const n = all[0];
  if (n + 1 > all.length) return;
  let id = "";
  for (let i = 1; i <= n; i++) id += String.fromCharCode(all[i]);
  const t = termTabs.get(id);
  if (!t) return;
  termOutput(t, all.subarray(1 + n));
}

/* Shell output. The screen is drawn and the reader's position dealt with by
   `termDraw`'s frame — including the "am I at the bottom, or have I scrolled up
   to read something" question, which is now a question about two different
   parts of the pane (the scrollback above and the screen below) and belongs with
   the code that knows where each of them starts. */
function termOutput(t, bytes) {
  const me = t.term;
  if (me.dead) return;
  let text = "";
  try { text = new TextDecoder("utf-8", { fatal: false }).decode(bytes); }
  catch { text = ""; }
  me.screen.write(text);
  const back = me.screen.takeOutput();
  if (back) termFeed(t, back);
  termDraw(t);
}

/* Draw the screen, and the scrollback above it. Coalesced into one frame: a
   `cat` of a big file arrives in many chunks and repainting per chunk is a
   layout per chunk for something the browser can only show once per frame
   anyway.

   The rAF callback re-reads `t.term` rather than closing over its contents, and
   the grid over `me.grid`, because this runs after the tab may have been
   closed: painting into a detached node is harmless, but reading a screen the
   tab no longer has is a TypeError in a callback nobody can see coming. */
function termDraw(t) {
  const me = t.term;
  if (!me || me.dirty) return;
  me.dirty = true;
  const grid = me.grid;
  requestAnimationFrame(() => {
    const now = t.term;
    if (now) now.dirty = false;
    if (!grid) return;
    if (!now) { grid.innerHTML = ""; return; }
    /* The scrollback first — the rows it gains and loses are what can move the
       reader's view, and where they land decides what the pane has to do about
       it — then the screen below it, then the keeping-still. */
    const keep = termHistSync(now, now.screen.takeHist());
    grid.innerHTML = now.screen.render();
    termFollow(now, keep);
  });
}

/* One row of scrollback, as a node of its own. Per row rather than one string
   for the whole scrollback because the only two things that ever happen to it
   are "append one" and "drop some off the front", and both are splices when
   each row is its own node — whereas `innerHTML =` of two thousand rows is a
   parse of the entire scrollback for every line the shell prints. */
function termHistRow(html) {
  const d = document.createElement("div");
  d.className = "term-hist-row";
  d.innerHTML = html;
  return d;
}

/* Put the scrollback's rows into the pane: whole redraw when the width changed
   (every row was measured in the old cell) or the history was thrown away,
   otherwise append what is new and drop what is past the cap.
   `me.cellH` and `me.histRows` are the pane's own units of distance, and the
   reader's position is measured in them, which is what makes the correction
   below arithmetic rather than a guess.

   Returns what the pane owes the reader for having done it — whether they were
   at the bottom, and how many rows the document under them moved by — or null
   when the scrollback did not change at all. The pane's scroll position is read
   *before* the DOM is touched: appending is what makes the browser lay the pane
   out again, so a position read afterwards is a position already moved. */
function termHistSync(me, h) {
  const el = me.hist, out = me.out;
  if (!el || !h) return null;
  const hgt = me.cellH;
  const was = out ? out.scrollTop : 0;
  const atBottom = !out || termAtBottom(out);
  /* Rows above the screen, before this frame: the boundary between "the reader
     is reading the scrollback" and "the reader is looking at the screen". */
  const before = me.histRows;
  let added = 0, dropped = 0;
  if (h.rebuild) {
    while (el.firstChild) el.removeChild(el.firstChild);
    for (const row of h.rebuild) el.appendChild(termHistRow(row));
    me.histRows = h.rebuild.length;
  } else {
    added = h.add.length;
    dropped = h.drop || 0;
    for (const row of h.add) el.appendChild(termHistRow(row));
    for (let i = 0; i < dropped && el.firstChild; i++) el.removeChild(el.firstChild);
    me.histRows += added - dropped;
  }
  /* Rows enter the scrollback at its foot, so they push the *screen* down and
     leave everything above untouched: a reader who has scrolled up to read what
     went past sees them appear below their view and is not moved at all, while
     a reader looking at the screen has their text pushed down by exactly the
     rows added. Rows the cap takes off the front are the mirror image — they sat
     above everything, so the document slid up when they went. */
  let rows = 0;
  if (!h.rebuild && hgt > 0) rows = -dropped + (was >= before * hgt ? added : 0);
  return { atBottom: atBottom, rows: rows };
}

/* Keep the reader looking at what they were looking at, after a frame that
   changed how much scrollback there is. The pane sets `overflow-anchor: none`,
   so this is the only thing deciding what happens to a scrolled-up reader — the
   browser's own anchoring is disabled precisely because it cannot tell a row
   that entered the history from a row the reader asked to see. A rebuild (a
   re-wrap at a new width) gets no correction: the rows themselves are different,
   and a real terminal moves a scrolled-up reader's text on reflow too. */
function termFollow(me, keep) {
  const out = me.out;
  if (!out) return;
  /* Nothing entered or left the scrollback this frame, so nothing moved under
     the reader: only one who was already following gets pinned to the new
     bottom, and one who scrolled up is left where they are. */
  if (!keep) { if (termAtBottom(out)) out.scrollTop = out.scrollHeight; return; }
  if (keep.atBottom) { out.scrollTop = out.scrollHeight; return; }
  if (!keep.rows || !(me.cellH > 0)) return;
  out.scrollTop = Math.max(0, out.scrollTop + keep.rows * me.cellH);
}

/* At the bottom, within a couple of rows: the same tolerance the console tab
   uses, so a pane the reader is following does not unstick itself over a
   rounding difference. */
function termAtBottom(out) {
  return out.scrollTop + out.clientHeight >= out.scrollHeight - 30;
}

/* ---------------------------------------------------------------- tabs */
function makeTab(name, kind, path) {
  const id = "t" + (++seq);
  const tab = document.createElement("div");
  tab.className = "tab";
  tab.dataset.id = id;
  tab.innerHTML = '<span class="dot"></span><span class="name"></span>' +
    (kind === "console" ? "" : '<span class="x" title="close">&times;</span>');
  /* The console tab has nothing to signal: it is never closed and its tab has
     no close button to be a click target, so the dot is dropped there rather
     than sitting in the strip as decoration that means nothing. */
  if (kind === "console") tab.querySelector(".dot").remove();
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
      '<div id="prompt-row">' +
      '<button id="stop" title="cancel the turn in flight, like Ctrl-C in the terminal">Stop</button>' +
      '<button id="term" title="open a shell in the working directory">Terminal</button>' +
      '<button id="send">Send (Enter)</button></div></div>';
  } else if (kind === "term") {
    panel.innerHTML =
      '<div class="term-head"><span class="dir"></span><span class="shell"></span>' +
      '<span class="spacer"></span><button class="new">New</button>' +
      '<button class="kill">Kill</button></div>' +
      '<div class="term-out"><div class="term-hist"></div>' +
      '<pre class="term-grid"></pre>' +
      '<span class="term-size" aria-hidden="true"></span></div>' +
      '<textarea class="term-input" tabindex="0" spellcheck="false" wrap="off" ' +
      'autocapitalize="off" autocorrect="off" autocomplete="off" ' +
      'aria-label="terminal input"></textarea>';
  } else {
    /* A file view keeps its own monospaced face (source is read by its
       columns) and takes only the chosen size — the CSS on .file-out says so,
       so nothing here has to know about the picker. */
    panel.innerHTML =
      '<div class="file-head"><span class="file-info"></span><span class="spacer"></span>' +
      '<button class="dl" title="save a copy of this file to your computer">Download</button></div>' +
      '<pre class="file-out"></pre>';
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
  else if (kind === "term") wireTerm(t);
  else {
    /* The file view's two controls: shift-wheel scrolls sideways through long
       lines, like the terminal panes people expect, and Download hands the file
       itself to the browser — which is the only way to get at a file the pane
       cannot draw, and a quicker way to keep a copy than select-and-copy. */
    t.el.querySelector(".file-out").addEventListener("wheel", (e) => {
      if (e.shiftKey) { t.el.querySelector(".file-out").scrollLeft += e.deltaY; e.preventDefault(); }
    });
    t.el.querySelector(".dl").addEventListener("click", () => downloadFile(t.path));
  }
  return t;
}

function closeTab(t) {
  if (t.kind === "console") return;           /* the console always stays */
  if (t.kind === "term") termClose(t);        /* the shell goes with the tab */
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
  /* A pane measured while hidden reports 0, so the size of the one just shown
     is taken now — the window it had until then was a guess. */
  if (t && t.kind === "term") termFit(t, true);
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
  /* Not `sendStop` itself: as a listener it would be handed the click event as
     its button argument. */
  panel.querySelector("#stop").addEventListener("click", () => sendStop());
  /* The shell is jonnyq's, not the model's: nothing here goes through a turn,
     and the tab stays when the console's session ends. */
  panel.querySelector("#term").addEventListener("click", () => termOpen(null));
  makeResizable(panel);
}

/* The browser's Ctrl-C. One request, and the session's own `ready`/`busy`
   frames do the rest: the console shows `[cancelled]` because a cancelled turn
   prints it, exactly as it does when the human presses Ctrl-C at the terminal.
   Nothing here invents a message of its own — the transcript is the only
   account of what happened, and a browser that claimed "stopped" while the
   terminal said otherwise would be the page's word against jonnyq's. */
function sendStop(btn) {
  if (!busy) return;             /* nothing to cancel; the button is greyed anyway */
  /* Greyed while the request is in flight, so one click cannot send three
     cancels; the `ready` frame that a stopping turn produces re-enables it,
     and a turn that will not be cancelled is told so below. Either Stop button
     may be the one pressed - the prompt box's or the ask popup's - and both
     label themselves, because the turn they are cancelling is one turn. */
  const pressed = btn || (consoleTab && consoleTab.el.querySelector("#stop"));
  if (pressed) {
    pressed.disabled = true;
    pressed.textContent = "stopping\u2026";
  }
  rpc({ type: "stop" }).then((m) => {
    /* The reply carries the server's own figure for whether work is still
       going: a turn that unwinds cleanly says no, one that is slow to notice
       says yes, and that is the difference between greying the button out and
       leaving it pressable, because pressing it again is the right move when
       the first one did not take. */
    busy = !!m.busy;
    /* Say so only when the session really is idle. A press that lands while a
       turn is still unwinding (its token already cancelled, `busy` not yet
       dropped) arrived late rather than found nothing, and telling someone who
       just stopped a turn that there was nothing to stop would be a lie about
       the thing they just did. */
    if (!m.stopped && !m.busy) {
      popup("Nothing to stop", "The turn had already finished. It may have stopped on its own a moment ago - the console shows how it ended.");
    }
    restoreStop();
  }, (e) => {
    popup("The stop request was not sent", why(e));
    restoreStop();
  });
}

/* Put both Stop buttons back. Restored from `busy` rather than from the `ready`
   frame that may or may not follow: a turn that never watches its token stays
   working, and then the only correct next move is to press the button again. */
function restoreStop() {
  const t = consoleTab;
  const btn = t ? t.el.querySelector("#stop") : null;
  if (btn) btn.textContent = "Stop";
  const askBtn = $("ask-stop");
  if (askBtn) askBtn.textContent = "Stop";
  syncStop();
}

function sendPrompt() {
  const t = consoleTab; if (!t) return;
  const ta = t.el.querySelector("#prompt");
  const text = ta.value;
  if (!text.trim()) return;
  if (!ready) { popup("jonnyq is busy", busy ? "A turn is in progress. Stop it with the Stop button (the browser's Ctrl-C), or wait for it to finish." : "The session is not waiting for a prompt right now - wait for the turn in progress to finish (or press Ctrl-C in the terminal to cancel it)."); return; }
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

function setReady(state, note, busyNow) {
  ready = !!state;
  /* `busy` comes from the frame's own flag rather than from `!ready`: "not
     ready" also means no model yet, still connecting, or the link dropped, and
     none of those has anything to cancel. Absent (a call from boot or the
     reconnect path, which has no server state to report) means not working. */
  busy = !!busyNow;
  const t = consoleTab; if (!t) return;
  const ta = t.el.querySelector("#prompt");
  const btn = t.el.querySelector("#send");
  const noteEl = t.el.querySelector("#prompt-note");
  ta.disabled = !ready;
  ta.placeholder = ready ? "Type a prompt, or a /command. Enter sends, Shift-Enter adds a newline."
                        : "not ready - jonnyq is working";
  btn.disabled = !ready;
  syncStop();
  noteEl.textContent = promptNote(note);
  if (ready && document.activeElement !== ta) ta.focus();
}

/* The greyed textarea already says it is not for typing; the line under it
   says what the session is doing instead and - when it is a turn - that the
   Stop button beside Send will end it. The busy case replaces the server's
   note rather than appending to it: the note is what the loop said when it
   last reached its prompt ("jonnyq is waiting for a prompt"), which is exactly
   the opposite of what is happening while a turn runs. */
function promptNote(note) {
  if (busy) return "jonnyq is working - Stop cancels the turn, like Ctrl-C in the terminal";
  return note || (ready ? "" : "waiting for the current turn to finish");
}

/* Stop is Send's mirror image: lit exactly while there is a turn to cancel,
   and never while a stop request is already on its way (sendStop owns that
   in-flight label, so a frame arriving mid-request must not undo it). Split out
   so the status frames can re-sync it too — an `info` frame carries `busy`
   alongside the status line, so a tab that missed a `ready` frame corrects
   itself on the next tick instead of sitting on a stale button. The popup's
   copy is driven from the same flag: it is the same turn being cancelled. */
function syncStop() {
  const t = consoleTab;
  const stop = t ? t.el.querySelector("#stop") : null;
  if (stop && stop.textContent !== "stopping\u2026") {
    stop.disabled = !busy;
    stop.title = busy
      ? "cancel the turn in flight, like Ctrl-C in the terminal"
      : "nothing is running - this cancels the turn in flight, like Ctrl-C in the terminal";
  }
  const askStop = $("ask-stop");
  if (askStop && askStop.textContent !== "stopping\u2026") askStop.disabled = !busy;
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

/* -------------------------------------------------------------- the tree
   The listing keeps itself current — there is no refresh button, and the panel
   is meant to read like the folder rather than like a snapshot of it taken at
   page load. The state it is redrawn from, rather than from a request:

     treeOpen    the folders the reader has expanded. A redraw that forgot these
                 would fold the tree back to one line every few seconds, which
                 is worse than the button it replaced.
     treeNodes   the listing each folder on screen was drawn from. Comparing a
                 folder's fresh listing with the one already drawn is what lets
                 an unchanged poll cost nothing — and a rebuild is not free: it
                 loses the panel's scroll position and yanks rows out from under
                 a click in progress. So a redraw happens only when some folder
                 actually gained, lost or renamed an entry.
     treeFails   why a folder on screen could not be listed, shown in its place.
     treeDrawn   whether anything is on screen, so the first listing always
                 draws even when it happens to equal the empty cache.
     treeGen     retired by `resetTree`, to tell a late answer from a current one. */
const TREE_POLL_MS = 2500;     /* how often the folder is re-listed        */
const treeOpen = new Set();    /* folder paths shown expanded              */
const treeNodes = new Map();   /* path -> the listing it was drawn from    */
const treeFails = new Map();   /* path -> why that listing could not be had */
const treeLoading = new Set(); /* listings asked for and not yet answered  */
let treeErr = "";              /* the failure already explained in a popup */
let treeDrawn = false;         /* is this tree on screen at all?           */
/* Bumped whenever the tree is thrown away, so an answer that arrives *after*
   that — a listing asked for just before the session moved to another working
   directory — is discarded instead of redrawing the old folder's rows. */
let treeGen = 0;
/* The working directory the rows on screen belong to, as last heard from the
   server. Null until the first `info`, which is what tells a greeting apart
   from a session that has actually moved elsewhere. */
let treeWd = null;

function treeSelection() {
  const t = byId(active);
  document.querySelectorAll("#tree .row").forEach((r) => {
    r.classList.toggle("sel", !!(t && t.kind === "file" && r.dataset.p === t.path));
  });
}

/* What a listing looks like: each entry's name and whether it is a folder, in
   the order the server sent them. Size is deliberately left out — no row shows
   one, so a file being written to is not a visible change, and rebuilding the
   panel for it would throw away the scroll position for nothing. */
function treeSig(node) {
  return (node.entries || []).map((e) => (e.dir ? "d" : "f") + e.name).join("\n");
}

/* Caches a listing and reports whether the screen would change. A folder seen
   for the first time counts as a change: drawing it is the whole point. */
function setTree(path, node) {
  const was = treeNodes.get(path);
  treeNodes.set(path, node);
  return !was || treeSig(was) !== treeSig(node);
}

const parentOf = (p) => {
  const i = p.lastIndexOf("/");
  return i < 0 ? "" : p.slice(0, i);
};

/* Forgets expanded folders that no longer exist, and listings of folders that
   have been folded up again, so a folder deleted mid-session does not stay
   expanded forever and a long session does not accumulate listings. Returns
   whether what is on screen is now wrong. */
function pruneTree() {
  let dropped = false;
  for (const p of [...treeOpen]) {
    const here = treeNodes.get(parentOf(p));
    if (!here) continue;                       /* its parent is not listed yet */
    const name = p.slice(p.lastIndexOf("/") + 1);
    const still = (here.entries || []).some((e) => e.dir && e.name === name);
    if (!still) { treeOpen.delete(p); treeNodes.delete(p); treeFails.delete(p); dropped = true; }
  }
  for (const p of [...treeNodes.keys()]) {
    if (p !== "" && !treeOpen.has(p)) { treeNodes.delete(p); treeFails.delete(p); }
  }
  return dropped;
}

/* Redraws the panel from the cache, keeping its scroll position: a poll that
   refreshed a folder half-way down the list must not send the reader back to
   the top of it. */
function drawTree() {
  const tree = $("tree");
  const top = tree.scrollTop;
  const box = treeNodeEl("");
  box.classList.remove("kids");
  box.style.marginLeft = "0";
  tree.textContent = "";
  tree.appendChild(box);
  tree.scrollTop = top;
  treeDrawn = true;
  treeSelection();
}

/* One folder's rows, from the cache. An open folder with no listing cached yet
   draws nothing and asks for one — the answer redraws (see `askTree`), which
   is why this is synchronous rather than returning a promise: patching one
   subtree in place cannot say where its parent's rows went. */
function treeNodeEl(path) {
  const box = document.createElement("div");
  box.className = "kids";
  const node = treeNodes.get(path);
  if (!node) {
    /* A folder asked for but not answered with a listing: say why in the tree
       rather than leaving an empty gap that looks like an empty folder. The
       next tick asks again (the poll lists every visible folder), so a folder
       that was briefly unreadable fills itself in by itself. */
    askTree(path);
    const failure = treeFails.get(path);
    if (failure) {
      const e = document.createElement("div");
      e.className = "row err";
      e.textContent = failure;
      box.appendChild(e);
    }
    return box;
  }
  for (const item of node.entries || []) {
    const row = document.createElement("div");
    row.className = "row " + (item.dir ? "dir" : "file");
    row.dataset.p = item.path;
    const open = item.dir && treeOpen.has(item.path);
    const tw = document.createElement("span"); tw.className = "tw";
    tw.textContent = item.dir ? (open ? "\u25BE" : "\u25B8") : "";  /* ▾ ▸ */
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
    row.addEventListener("click", () => {
      if (treeOpen.has(item.path)) treeOpen.delete(item.path);
      else treeOpen.add(item.path);
      drawTree();
    });
    if (open) box.appendChild(treeNodeEl(item.path));
  }
  if (!node.entries || !node.entries.length) {
    const e = document.createElement("div");
    e.className = "row"; e.style.color = "var(--fg4)";
    e.textContent = "(empty)";
    box.appendChild(e);
  }
  return box;
}

/* Why a folder could not be listed, remembered against it so `treeNodeEl` can
   say so inside the folder. Reports whether that is news — only news is worth
   a redraw, or a permanently unreadable folder would rebuild the panel (and
   lose the scroll position) on every tick. */
function noteTreeFail(path, why_) {
  const note = "cannot list: " + (why_ || "no listing");
  const was = treeFails.get(path);
  treeFails.set(path, note);
  return was !== note;
}

/* One folder's listing into the cache, then a redraw if it differs. A failure
   is drawn inside that folder (see `treeNodeEl`) rather than popped up: the
   panel is polled, and a box per tick about a folder nobody can read is not
   information, it is a shut-up. The rows that were there stay either way. */
function askTree(path) {
  if (treeLoading.has(path)) return;
  treeLoading.add(path);
  const gen = treeGen;
  const still = () => gen === treeGen;
  requestTree(path).then(
    (n) => { if (!still()) return; treeFails.delete(path); if (setTree(path, n)) drawTree(); },
    /* `drawTree` runs inside this handler, while `path` is still registered as
       loading, so the redraw's own `askTree` cannot start a second request. */
    (e) => { if (still() && noteTreeFail(path, why(e))) drawTree(); },
  ).finally(() => { if (still()) treeLoading.delete(path); });
}

async function refreshTree() {
  /* One listing at a time: boot, the greeting's `reset`, the poll and a click
     on a folder all ask for the same thing, and two parallel listings of one
     folder only make the later answer's renderer win at random. */
  if (treeRefresh) return treeRefresh;
  treeRefresh = (async () => {
    /* The root, plus every folder the reader can currently see: a folded-up
       folder has nothing on screen to keep current, and its listing is dropped
       by pruneTree until it is opened again. */
    const want = [...new Set(["", ...treeOpen])];
    /* All of them are marked as in flight before the first request goes out —
       and stay marked until after the redraw below — so neither a folder
       clicked open mid-poll nor the redraw of a folder that *failed* can ask
       for a listing that is being had, or was just refused. The next tick asks
       again, which is the retry. */
    for (const p of want) treeLoading.add(p);
    const gen = treeGen;
    try {
      const got = await Promise.all(want.map(async (p) => {
        try { return [p, await requestTree(p), null]; }
        catch (e) { return [p, null, e]; }
      }));
      /* Answers that arrive after the tree was thrown away are dropped whole —
         not just left undrawn: caching them would let the next redraw of the
         new working directory paint rows out of the old one's listing. */
      if (gen !== treeGen) return;
      let changed = false, failed = null;
      for (const [p, node, err] of got) {
        if (!node) {
          /* The working directory itself being unreadable is the one failure
             worth a popup: there is nothing to draw in its place, so the rows
             already on screen are kept and the reason is said out loud. */
          if (p === "") { failed = err; continue; }
          treeNodes.delete(p);
          if (noteTreeFail(p, why(err))) changed = true;
          continue;
        }
        treeFails.delete(p);
        if (setTree(p, node)) changed = true;
      }
      if (pruneTree()) changed = true;
      if (changed || !treeDrawn) drawTree();
      /* Drawn first, then reported: the rows that were already up stay up, and
         the folder that could not be read says so inside itself. Only the
         working directory failing outright gets a popup, since there is
         nothing on screen to put the reason into — and `treeErr` is left
         holding the note so the next tick does not repeat the box. */
      if (failed) throw failed;
      treeErr = "";
    } catch (e) {
      treeError(why(e));
    } finally {
      /* The in-flight marks go with this poll's own generation: a newer one has
         set its own, and clearing those would let a folder clicked open right
         now ask for a listing that is already on its way. */
      if (gen === treeGen) for (const p of want) treeLoading.delete(p);
      treeRefresh = null;
    }
  })();
  return treeRefresh;
}

/* Throws the tree away, to be drawn again by the next listing. Used when the
   session's working directory changes, so a new project is never drawn mixed in
   with the old one's expanded folders and cached rows. The caller follows it
   with a `refreshTree`, which is what fills the panel back in. */
function resetTree() {
  treeGen++;
  treeOpen.clear();
  treeNodes.clear();
  treeFails.clear();
  /* Nothing is in flight *for this tree* any more — an answer still on its way
     belongs to the generation just retired, and the generation check drops it.
     Leaving the marks set would block that path from ever being asked again. */
  treeLoading.clear();
  treeDrawn = false;
  $("tree").textContent = "";
}

/* One popup per distinct failure, because a poll that keeps failing would
   otherwise stack a box per tick about something no popup can fix. The note
   is cleared by the next listing that works. */
function treeError(note) {
  const msg = note || "cannot list this folder";
  if (treeErr === msg) return;
  treeErr = msg;
  popup("Cannot list the working directory", msg);
}

/* The tick. Skipped while offline — every request would wait out its connect
   deadline and then say so, and `onclose`'s reconnect plus the boot listing
   already cover the way back — and in a hidden tab, which nobody is reading
   and which browsers throttle into a stall anyway. */
function treeTick() {
  if (!OPEN() || document.hidden) return;
  refreshTree();
}
setInterval(treeTick, TREE_POLL_MS);
/* Coming back to the tab should show the folder as it is now rather than as it
   was when you left, so the wait for the next tick is skipped. */
document.addEventListener("visibilitychange", () => { if (!document.hidden) treeTick(); });

/* ------------------------------------------------------- syntax highlighting
   A file tab used to show the file's bytes in one flat wash of fg, which is
   fine until you open a source file and spend a minute working out which of
   the near-identical lines was the comment. So the page colours source itself,
   in the same gruvbox dark palette the console is drawn in.

   It is a scanner, not a parser, and it is written here rather than fetched
   from a CDN for the same reason the rest of the page is written here: the
   colouring has to work against a local model on a machine with no internet,
   which is most of the machines jonnyq runs on, and a highlighter that arrives
   by network request is a highlighter that is sometimes simply absent.

   Three rules it never breaks, because a highlighter that lies is worse than no
   highlighter at all:

   * The text it draws is the file's text. Every token is a slice of the input,
     emitted in order with nothing dropped, added or reordered, so selecting
     and copying out of the pane still gives you the file. The page's node
     tests check that by stripping the markup back off and comparing.
   * Everything is escaped before it is wrapped (see `esc`), so a file that
     contains a tag is still only ever text.
   * A string ends at its closing quote, at the end of the line, or at the end
     of the file, whichever comes first, and a comment ends at its close marker
     or at the end of the file. Never at "whatever the rest of the file looks
     like" — otherwise one stray apostrophe in a sentence of prose turns every
     line under it pink. */

/* Past this many characters a file is shown plain. Highlighting is one pass
   over the text and, after merging runs of one colour, roughly one `<span>` per
   token; the server will send up to 2 MB, and colouring a file that size would
   freeze the tab for a second to no purpose. A file that big is being searched,
   not read. */
const HL_MAX = 400 * 1000;

/* Extension to language, deliberately by suffix only: the tree already knows
   the name, and a file called `.env` or `Cargo.lock` is worth knowing. An
   extension that is not here is drawn unhighlighted rather than guessed at —
   colouring a file with the wrong rules is how you come to distrust the right
   ones. The empty values are the "we know what this is and there is nothing to
   colour" cases, said out loud instead of left as a miss. */
const HL_EXT = {
  rs: "rust", toml: "toml",
  py: "python", pyw: "python", python: "python",
  js: "js", mjs: "js", cjs: "js", jsx: "js",
  ts: "ts", tsx: "ts",
  json: "json", geojson: "json", ndjson: "json",
  yaml: "yaml", yml: "yaml", lock: "yaml",
  md: "md", markdown: "md", mdx: "md",
  sh: "sh", bash: "sh", zsh: "sh", ksh: "sh", fish: "sh",
  html: "html", htm: "html", xml: "html", xsl: "html", svg: "html",
  css: "css", scss: "css", sass: "css", less: "css",
  c: "c", h: "c", cpp: "c", cxx: "c", cc: "c", hpp: "c", hh: "c", ino: "c", m: "c",
  go: "go", java: "java", kt: "java", kts: "java", scala: "java",
  rb: "rb", php: "php", lua: "lua", zig: "zig", sql: "sql", pl: "perl",
  ini: "ini", conf: "ini", cfg: "ini", properties: "ini", env: "env",
  service: "ini", desktop: "ini", gitconfig: "ini", jsonc: "jsonc",
  txt: "", log: "", csv: "", tsv: "",
};

/* Files known by name rather than by suffix, because they have no suffix, or
   start with a dot and so have no stem to look at. */
const HL_FILE = {
  dockerfile: "docker", containerfile: "docker", makefile: "ini",
  ".gitignore": "ini", ".gitattributes": "ini", ".editorconfig": "ini",
  ".env": "env", ".env.example": "env", ".stylelintrc": "ini",
  "cargo.lock": "yaml", "package.json": "json", "go.mod": "ini",
};

/* A `#!` line is the other evidence worth trusting: an extensionless script is
   a shell, Python or Perl script, and the first line says which. */
const HL_SHEBANG = {
  sh: "sh", bash: "sh", zsh: "sh", ksh: "sh", fish: "sh", ash: "sh", dash: "sh",
  python: "python", python3: "python", python2: "python",
  node: "js", ruby: "rb", php: "php", lua: "lua", perl: "perl", awk: "sh",
};

/* What the pane's header calls a language. */
const HL_TITLE = {
  rust: "Rust", toml: "TOML", python: "Python", js: "JavaScript", ts: "TypeScript",
  json: "JSON", jsonc: "JSON", yaml: "YAML", md: "Markdown", sh: "shell",
  docker: "Dockerfile", html: "HTML", css: "CSS", c: "C", go: "Go", java: "Java",
  rb: "Ruby", php: "PHP", lua: "Lua", zig: "Zig", sql: "SQL", perl: "Perl",
  ini: "INI", env: "env file",
};

/* The opening of a markup comment, in two pieces: written as one literal, the
   three characters that switch a browser's inline-script tokenizer into its
   escaped state are in the page, and while they are balanced here there is no
   reason to depend on that. */
const HL_CMARK = "<" + "!--";

/* One entry per language: the comment markers, the quote characters, the
   reserved words, and a `mode` for the few things a shared scanner cannot know
   on its own (JSON and YAML keys, HTML tags, Markdown headings, `=`-separated
   config keys). `upper` means a Capitalised name is a type and a SCREAMING one
   is a constant — true of every language here except shell, Markdown and
   config files, where it would colour half the file.

   The keyword lists are reserved words, not the standard library: colouring
   `Vec` and `HashMap` because they are Capitalised is enough, and a list of
   every built-in is a list that is permanently out of date. */
const HL_LANG = {
  rust: {
    line: "//", open: "/*", close: "*/", nest: true, q: "\"'", raw: true, upper: true,
    kw: "as async await break const continue dyn else enum extern false fn for if impl " +
        "in let match mod move mut pub ref return self Self static struct super trait " +
        "true type unsafe use where while crate abstract do final macro override " +
        "typeof unsized virtual yield try",
    types: "i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 usize isize f32 f64 bool char str",
  },
  /* TOML is a config file with three extra literals: `[table]` headers and
     `key = value`, with `#` comments. */
  toml: { line: "#", q: "\"'", upper: true, mode: "ini", kw: "true false inf nan" },
  env: { line: "#", q: "\"'", mode: "eq" },
  ini: { line: "#;", q: "\"'", mode: "ini", kw: "true false yes no on off null" },
  python: {
    line: "#", q: "\"'", triple: ['"""', "'''"], upper: true, pre: "@",
    kw: "and or not if elif else for while def class return lambda yield try except " +
        "finally raise with as import from pass break continue global nonlocal assert " +
        "del in is None True False async await match case",
  },
  js: {
    line: "//", open: "/*", close: "*/", q: "\"'`", upper: true,
    kw: "var let const function class extends new this typeof instanceof in of if else " +
        "for while do switch case default break continue return try catch finally throw " +
        "async await yield delete void static get set super export import from as null " +
        "undefined true false debugger with",
  },
  ts: {
    line: "//", open: "/*", close: "*/", q: "\"'`", upper: true,
    kw: "var let const function class extends new this typeof instanceof in of if else " +
        "for while do switch case default break continue return try catch finally throw " +
        "async await yield delete void static get set super export import from as null " +
        "undefined true false debugger with interface type enum implements public " +
        "private protected readonly namespace declare abstract keyof infer satisfies " +
        "any string number boolean void unknown never is",
  },
  /* C, C++ and everything that borrowed their shape. */
  c: {
    line: "//", open: "/*", close: "*/", q: "\"'", upper: true, pre: "#",
    kw: "auto break case char const continue default do double else enum extern float " +
        "for goto if inline int long register return short signed sizeof static struct " +
        "switch typedef union unsigned void volatile while bool true false new delete " +
        "class public private protected template namespace this using virtual override " +
        "final try catch throw nullptr",
    types: "size_t ssize_t int8_t uint8_t int16_t uint16_t int32_t uint32_t int64_t " +
           "uint64_t size_type string vector map pair bool",
  },
  go: {
    line: "//", open: "/*", close: "*/", q: "\"'`", upper: true,
    kw: "break case chan const continue default defer else fallthrough for func go goto " +
        "if import interface map package return select struct switch type var " +
        "nil true false string int int8 int16 int32 int64 uint float32 float64 byte " +
        "rune bool error any",   /* its types are reserved words in Go, so listed there */
  },
  java: {
    line: "//", open: "/*", close: "*/", q: "\"'", upper: true,
    kw: "abstract assert boolean break byte case catch char class const continue default " +
        "do double else enum extends final finally float for goto if implements import " +
        "instanceof int interface long native new package private protected public " +
        "return short static strictfp super switch synchronized this throw throws " +
        "transient try void volatile while var record sealed null true false",
  },
  rb: {
    line: "#", open: "=begin", close: "=end", q: "\"'", upper: true,
    kw: "alias and begin break case class def defined do else elsif end ensure false " +
        "for if in module next nil not or redo rescue retry return self super then true " +
        "undef unless until when while yield require attr_accessor",
  },
  php: {
    line: "//#", open: "/*", close: "*/", q: "\"'", upper: true,
    kw: "abstract and array as break callable case catch class clone const continue " +
        "declare default do echo else elseif empty enddeclare endfor endforeach endif " +
        "endswitch endwhile extends final finally fn for foreach function global goto " +
        "if implements include instanceof insteadof interface isset list namespace new " +
        "or print private protected public require return static switch throw trait try " +
        "unset use var while xor yield null true false",
  },
  lua: {
    line: "--", open: "--[[", close: "]]", q: "\"'", upper: true,
    kw: "and break do else elseif end false for function goto if in local nil not or " +
        "repeat return then until while",
  },
  zig: {
    line: "//", open: "/*", close: "*/", q: "\"'", upper: true,
    kw: "const var fn return struct enum union switch case if else while for break " +
        "continue pub use extern export async await try catch defer errdefer orelse " +
        "and or true false null undefined anytype comptime inline callconv",
  },
  perl: {
    line: "#", q: "\"'", upper: true,
    kw: "my our local sub if elsif else unless while until for foreach do return " +
        "use no package require begin end last next redo",
  },
  sh: {
    line: "#", q: "\"'", pre: "$@", upper: false, mode: "sh",
    kw: "if then else elif fi for while until do done case esac function in return " +
        "exit break continue select time exec trap set unset shift local readonly " +
        "declare export source eval getopts",
  },
  sql: {
    line: "--", open: "/*", close: "*/", q: "\"'`", ci: true,
    kw: "select from where insert into values update delete create table drop alter " +
        "add column primary key foreign references index view join inner left right " +
        "full outer on group by having order asc desc limit offset union all distinct " +
        "as and or not null is in like between exists case when then else end count " +
        "sum avg min max coalesce cast begin commit rollback transaction grant revoke",
  },
  /* JSON has no comments and no keywords worth the name, but the name on the
     left of a colon is the only structure in the file, so it gets its own
     colour. `keys` is what lets the scanner tell a name from a value. */
  json: { q: "\"", keys: true, kw: "true false null" },
  jsonc: { q: "\"", keys: true, line: "//", open: "/*", close: "*/", kw: "true false null" },
  yaml: {
    line: "#", q: "\"'", keys: true, mode: "yaml", kw: "true false null yes no on off",
  },
  md: { mode: "md", open: HL_CMARK, close: "-->" },
  html: { open: HL_CMARK, close: "-->", mode: "html" },
  css: { line: "", open: "/*", close: "*/", q: "\"'", mode: "css", pre: "@#" },
  /* A Dockerfile is a config file that shouts: `#` comments and INSTRUCTIONS. */
  docker: {
    line: "#", q: "\"'", mode: "ini", ci: true,
    kw: "FROM RUN CMD COPY ADD ENV EXPOSE ARG LABEL USER WORKDIR ONBUILD STOPSIGNAL " +
        "HEALTHCHECK MAINTAINER VOLUME ENTRYPOINT",
  },
};

/* The scanner's two sticky patterns, at page scope so the state is one that is
   always set immediately before use — the same discipline as `SGR`. */
/* Both are written with `{0,}` rather than `*` at the end: a regex literal
   ending in a star is the two characters that close a block comment, and the
   page is one long comment-and-string mix where that has bitten before. */
const HL_WORD_RE = /[A-Za-z_$][A-Za-z0-9_$]{0,}/y;
const HL_NUM_RE = /[0-9][0-9a-fA-FxXoObBeE._]*(?:[eE][+-]?[0-9][0-9._]*)?[a-zA-Z_]{0,}/y;

/* Is this character part of a word? Tells `r#"..."#` from the `r` at the end of
   an identifier, and a leading `$` from one inside a name. */
const HL_WORDY = /[A-Za-z0-9_$]/;

/* Lazily-built sets: the table above keeps the words in the compact form a human
   can proof-read, and the lookup wants a Set. */
function hlKw(lang) {
  if (!lang._kw) lang._kw = new Set((lang.kw || "").split(" "));
  return lang._kw;
}
/* Type names that are not Capitalised — Rust's `u32`, `bool` — and so are
   invisible to the shape rules. */
function hlTypes(lang) {
  if (!lang._types) lang._types = new Set((lang.types || "").split(" "));
  return lang._types;
}

/* Which language a file is written in, from its name, and failing that from its
   `#!` line. `""` means "not one of ours" and the pane stays plain text. */
function hlLangOf(name, src) {
  const base = (name || "").toLowerCase();
  if (HL_FILE[base]) return HL_FILE[base];
  const dot = base.lastIndexOf(".");
  /* `.env` has no stem, but the thing after the dot is still its extension. */
  const ext = dot > 0 ? base.slice(dot + 1) : dot === 0 ? base.slice(1) : "";
  if (ext && ext in HL_EXT) return HL_EXT[ext];
  if ((src || "").slice(0, 2) === "#!") {
    const nl = src.indexOf("\n");
    const line = src.slice(0, nl < 0 ? Math.min(120, src.length) : nl);
    for (const w of line.split(/[\s/]+/)) if (HL_SHEBANG[w]) return HL_SHEBANG[w];
    return "sh";
  }
  return "";
}

/* The one colour a word gets, in the order that matters: a reserved word, then
   a constant or a type, then anything followed by an argument list — which is
   how a call is recognised in every language here without a symbol table. */
function hlWord(src, i, j, lang) {
  const w = src.slice(i, j);
  const words = hlKw(lang);
  if (words.has(w) || (lang.ci && words.has(w.toLowerCase()))) return "s-kw";
  if (hlTypes(lang).has(w)) return "s-type";
  if (lang.upper) {
    /* A constant before a type: `MAX_LEN` is shouting, and the type pattern
       with its underscore would otherwise claim it. */
    if (w.length > 1 && /^[A-Z][A-Z0-9_]*$/.test(w)) return "s-const";
    if (w.length > 1 && /^[A-Z][A-Za-z0-9_]*$/.test(w)) return "s-type";
  }
  let k = j;
  while (k < src.length && (src[k] === " " || src[k] === "\t")) k++;
  /* A call — or, with the bang, a macro call, which is how `println!` and
     `format!` are written and would otherwise be the only plain word on a
     line that is otherwise all colour. No language here uses `identifier!(`
     for anything else.

     Not in prose or config: "see the notes (below)" is not a call, and neither
     is `Description=jonnyq (web)`, and a colour that appears for no reason is
     the kind that makes the rest of the file untrustworthy. */
  const prose = lang.mode === "md" || lang.mode === "ini" || lang.mode === "eq";
  if (!prose && (src[k] === "(" || (src[k] === "!" && src[k + 1] === "("))) return "s-fn";
  return "";
}

/* The first non-space character at or after `j`. */
const HL_NEXT = (src, j) => {
  while (j < src.length && (src[j] === " " || src[j] === "\t")) j++;
  return src[j] || "";
};

/* Is `i` at the start of a line, ignoring indentation? Bounded by the indent, so
   it never walks the file. */
function hlAtBol(src, i) {
  let p = i - 1;
  while (p >= 0 && (src[p] === " " || src[p] === "\t")) p--;
  return p < 0 || src[p] === "\n";
}

/* How many unclosed `{` precede `i` on its line. TOML writes an inline table as
   `features = { version = "1" }`, and gruvbox draws those nested keys dimmer
   than the ones that start a line. The walk stops at the line start or after
   HL_INLINE_LOOKBACK characters — whichever comes first, so a file that is one
   enormous line cannot make the scan quadratic the way an unbounded one would. */
const HL_INLINE_LOOKBACK = 200;

function hlInlineDepth(src, i) {
  let p = i, depth = 0, seen = 0;
  while (p > 0 && seen++ < HL_INLINE_LOOKBACK) {
    p--;
    const c = src[p];
    if (c === "\n") break;
    if (c === "{") depth++;
    else if (c === "}") depth--;
  }
  return depth;
}

/* Tokenise one file into `[class, text]` pairs whose texts concatenate back into
   the source. Comments and strings are matched by their markers; everything
   else is words, numbers and punctuation. */
function hlTok(src, lang) {
  const line = lang.line || "", open = lang.open || "", close = lang.close || "";
  const q = lang.q || "", triple = lang.triple || [], n = src.length;
  const keys = !!lang.keys;
  const toks = [];
  let i = 0, done = 0;

  /* Emit one token, taking the plain text in front of it with it so the slices
     stay contiguous — which is what makes the pane's selection the file. */
  const emit = (cls, from, to) => {
    if (to <= from) return;
    if (from > done) toks.push(["", src.slice(done, from)]);
    toks.push([cls, src.slice(from, to)]);
    done = to;
  };

  while (i < n) {
    const c = src[i];

    /* A block comment, nested where the language nests them (Rust, C, JS),
       because a doc comment containing an example of itself is a thing. */
    if (open && c === open[0] && src.startsWith(open, i)) {
      const at = i;
      let j = i + open.length, depth = 1;
      while (j < n && depth > 0) {
        if (lang.nest && src.startsWith(open, j)) { depth++; j += open.length; continue; }
        if (src.startsWith(close, j)) { depth--; j += close.length; continue; }
        j++;
      }
      j = Math.min(j, n);
      emit("s-com", at, j);
      i = j;
      continue;
    }

    /* A line comment runs to the end of the line and never further. */
    if (line && line.indexOf(c) >= 0 && src.startsWith(line, i)) {
      let e = src.indexOf("\n", i);
      if (e < 0) e = n;
      emit("s-com", i, e);
      i = e;
      continue;
    }

    /* Markup, in the one mode that has it: the tag name, its attributes and the
       quoted values, and nothing else about it. */
    if (lang.mode === "html" && c === "<" && /[A-Za-z!/]/.test(src[i + 1] || "")) {
      let j = i + 1;
      emit("s-op", i, j);
      /* A closing tag's slash and a declaration's bang belong to the marker. */
      if (src[j] === "/" || src[j] === "!") j++;
      const tagAt = j;
      while (j < n && /[A-Za-z0-9:_-]/.test(src[j])) j++;
      if (j > tagAt) emit("s-tag", tagAt, j);
      while (j < n && src[j] !== ">") {
        const d = src[j];
        if (d === '"' || d === "'") {
          let k = j + 1;
          while (k < n && src[k] !== d && src[k] !== "\n") k++;
          emit("s-str", j, Math.min(k + 1, n));
          j = Math.min(k + 1, n);
          continue;
        }
        if (HL_WORDY.test(d)) {
          const k0 = j;
          while (j < n && /[A-Za-z0-9:_-]/.test(src[j])) j++;
          emit("s-attr", k0, j);
          continue;
        }
        j++;
      }
      if (j < n) { emit("s-op", j, j + 1); j++; }
      i = j;
      continue;
    }

    /* A triple-quoted string is the one multi-line string any of these
       languages has, so it is tried before the single quote it starts with. */
    if (triple.length) {
      const t = triple.find((s) => src.startsWith(s, i));
      if (t) {
        const at = i;
        let k = src.indexOf(t, i + t.length);
        if (k < 0) k = n - t.length;
        const to = Math.max(Math.min(k + t.length, n), at);
        emit("s-str", at, to);
        i = to;
        continue;
      }
    }

    /* Rust's raw strings: `r"..."`, and the hashed forms with any number of
       hashes. Without the hash count, a raw string containing a quote — every
       JSON example ever pasted into a Rust test — ends early and takes the rest
       of the line with it. */
    if (lang.raw && c === "r" && !HL_WORDY.test(src[i - 1] || "")) {
      let j = i + 1, h = 0;
      while (src[j] === "#") { h++; j++; }
      if (src[j] === '"') {
        const end = '"' + "#".repeat(h);
        let k = j + 1;
        while (k < n && !src.startsWith(end, k)) k++;
        const to = Math.min(k + end.length, n);
        emit("s-str", i, to);
        i = to;
        continue;
      }
    }

    /* An ordinary string: same quote in, backslash escapes, out at the line
       end. `keys` is what turns a JSON or YAML name into something other than
       its values. */
    if (q.indexOf(c) >= 0) {
      let j = i + 1;
      while (j < n) {
        const d = src[j];
        if (d === "\\" && j + 1 < n) { j += 2; continue; }
        if (d === c) { j++; break; }
        if (d === "\n") break;
        j++;
      }
      emit(keys && HL_NEXT(src, j) === ":" ? "s-key" : "s-str", i, j);
      i = j;
      continue;
    }

    /* The line-anchored rules: Markdown's headings, list bullets and block
       quotes. */
    if (lang.mode === "md" && hlAtBol(src, i) && (c === "#" || c === ">")) {
      let e = src.indexOf("\n", i);
      if (e < 0) e = n;
      emit(c === "#" ? "s-kw" : "s-com", i, e);
      i = e;
      continue;
    }
    if (lang.mode === "md" && (c === "-" || c === "*" || c === "+") &&
        /[ \t]/.test(src[i + 1] || "") && hlAtBol(src, i)) {
      emit("s-kw", i, i + 1);
      i++;
      continue;
    }
    /* A Markdown fenced code block: three or more backticks at the start of a
       line, closed by a line of the same or a longer run. Matched as a block
       because the inline rule below cannot tell a fence from a span — without
       this, a README with two triple backticks and an odd number of threes
       colours everything under it green. */
    if (lang.mode === "md" && c === "`" && hlAtBol(src, i)) {
      let run = 0;
      while (src[i + run] === "`") run++;
      if (run >= 3) {
        const lineEnd = (j) => { const e = src.indexOf("\n", j); return e < 0 ? n : e; };
        let j = lineEnd(i);
        while (j < n) {
          const start = j + 1;
          if (start >= n) { j = n; break; }
          let k = start;
          while (k < n && (src[k] === " " || src[k] === "\t")) k++;
          let r = 0;
          while (k + r < n && src[k + r] === "`") r++;
          if (r >= run) {
            /* A closing fence: backticks and nothing else on its line. */
            let e = k + r;
            while (e < n && (src[e] === " " || src[e] === "\t")) e++;
            if (e >= n || src[e] === "\n") { j = e; break; }
          }
          j = lineEnd(start);
        }
        emit("s-code", i, Math.min(j, n));
        i = Math.min(j, n);
        continue;
      }
    }
    if (lang.mode === "md" && c === "`") {
      let k = src.indexOf("`", i + 1);
      const nl = src.indexOf("\n", i);
      if (k < 0 || (nl >= 0 && k > nl)) k = nl < 0 ? n - 1 : nl - 1;
      emit("s-str", i, Math.min(k + 1, n));
      i = Math.min(k + 1, n);
      continue;
    }

    /* A config file: `[section]` headers and the `key` left of an `=`, which is
       all the structure the format has. */
    if ((lang.mode === "ini" || lang.mode === "eq") && (c === "[" || HL_WORDY.test(c))) {
      if (c === "[") {
        /* A section header sits alone at the start of its line; a bracket that
           opens mid-line is an array or inline-table *value* — TOML's
           `features = ["regex"]` — and belongs to the string rules below. */
        const e = hlAtBol(src, i) ? src.indexOf("]", i) : -1;
        const lim = src.indexOf("\n", i);
        if (e >= 0 && (lim < 0 || e < lim)) { emit("s-type", i, e + 1); i = e + 1; continue; }
        i++;                                    /* not a section header after all */
        continue;
      }
      const k0 = i;
      while (i < n && (HL_WORDY.test(src[i]) || src[i] === "." || src[i] === "-")) i++;
      const eq = HL_NEXT(src, i) === "=";
      emit(eq ? (hlInlineDepth(src, k0) ? "s-const" : "s-key") : hlWord(src, k0, i, lang), k0, i);
      continue;
    }

    /* `$VAR`, `${VAR}` and `@decorator`; in CSS the same rule takes `#hex`. */
    if (lang.pre && lang.pre.indexOf(c) >= 0) {
      let j = i + 1;
      if (src[j] === "{") {
        const e = src.indexOf("}", j), lim = src.indexOf("\n", i);
        if (e >= 0 && (lim < 0 || e < lim)) { emit("s-var", i, e + 1); i = e + 1; continue; }
      }
      while (j < n && (HL_WORDY.test(src[j]) || (c === "@" && src[j] === ".") ||
             (c === "#" && lang.mode === "css" && /[0-9a-fA-F]/.test(src[j])))) j++;
      if (j > i + 1) {
        emit(lang.mode === "css" ? "s-pre" : "s-var", i, j);
        i = j;
        continue;
      }
    }

    /* YAML's document markers and anchors. */
    if (lang.mode === "yaml" && hlAtBol(src, i) &&
        (src.startsWith("---", i) || src.startsWith("...", i))) {
      emit("s-pre", i, i + 3);
      i += 3;
      continue;
    }
    if (lang.mode === "yaml" && (c === "&" || c === "*") && !HL_WORDY.test(src[i - 1] || "")) {
      let j = i + 1;
      while (j < n && HL_WORDY.test(src[j])) j++;
      if (j > i + 1) { emit("s-pre", i, j); i = j; continue; }
    }

    /* A shell flag: `-x`, `--force`. */
    if (lang.mode === "sh" && c === "-" && /[A-Za-z-]/.test(src[i + 1] || "") &&
        !HL_WORDY.test(src[i - 1] || "")) {
      let j = i + 1;
      while (j < n && /[A-Za-z0-9-]/.test(src[j])) j++;
      emit("s-var", i, j);
      i = j;
      continue;
    }

    /* A number, with its radix, its separators and its unit: `0x1f`, `1_000`,
       `1.5e-3`, `14pt`. */
    if (c >= "0" && c <= "9") {
      HL_NUM_RE.lastIndex = i;
      const m = HL_NUM_RE.exec(src);
      if (m) { emit("s-num", i, i + m[0].length); i += m[0].length; continue; }
    }

    /* A word. An unquoted YAML key is the one word whose colour the generic
       rule cannot know, so it is decided here, where the colon is visible. */
    if (/[A-Za-z_$]/.test(c)) {
      HL_WORD_RE.lastIndex = i;
      const m = HL_WORD_RE.exec(src);
      if (m) {
        const j = i + m[0].length;
        let cls = hlWord(src, i, j, lang);
        if (!cls && keys && lang.mode === "yaml" && hlAtBol(src, i) &&
            HL_NEXT(src, j) === ":") cls = "s-key";
        emit(cls, i, j);
        i = j;
        continue;
      }
    }
    i++;
  }
  if (done < n) toks.push(["", src.slice(done)]);
  return toks;
}

/* Tokens to HTML. Runs of one colour are merged first, which on prose is the
   difference between ten thousand spans and a few hundred, and the reason the
   cap can be as high as it is. */
function hlRender(src, lang) {
  const toks = hlTok(src, lang);
  let out = "", cls = null, text = "";
  for (const [c, t] of toks) {
    if (!t) continue;
    if (c !== cls) { out += hlSpan(cls, text); cls = c; text = t; } else text += t;
  }
  return out + hlSpan(cls, text);
}
function hlSpan(cls, text) {
  const body = esc(text);
  if (!body) return "";
  return cls ? '<span class="' + cls + '">' + body + "</span>" : body;
}

/* The pane's own job: pick the language, colour the file if it is small enough
   to be worth colouring, and hand back the language so the header can say what
   the pane is showing (`""` when it stayed plain text). */
function hlPaint(t, name, content) {
  const out = t.el.querySelector(".file-out");
  const lang = hlLangOf(name, content);
  if (!lang || !HL_LANG[lang] || content.length > HL_MAX) {
    out.textContent = content;
    return "";
  }
  out.innerHTML = hlRender(content, HL_LANG[lang]);
  return lang;
}

/* The one route that hands out a file's own bytes (see the server's
   `serve_download`). `downloadFile` is the only caller, and the browser does
   the rest: this is a plain navigation to an `attachment`, so no fetch, no blob
   and no cap on what can be saved. */
const downloadUrl = (path) => "/file?path=" + encodeURIComponent(path);

/* Save a copy. An `<a download>` click rather than a `window.open`, because the
   point is that nothing opens: a tab that navigated away from the console to
   show a PDF would have thrown away the session you were reading. */
function downloadFile(path) {
  if (!path) return;
  const a = document.createElement("a");
  a.href = downloadUrl(path);
  a.download = path.slice(path.lastIndexOf("/") + 1);
  a.rel = "noopener";
  document.body.appendChild(a);
  a.click();
  a.remove();
}

/* Opening a file: one request; the server decides whether it is text. A
   binary/image/PDF answer comes back as an error, which is the one case with
   nothing to show — so the popup says why and offers the file as a download
   instead, since "cannot be viewed" is not a reason you cannot want it. */
async function openFile(path, name) {
  let res;
  try { res = await rpc({ type: "file", path }); }
  catch (e) { popup("Cannot open file", why(e)); return; }
  if (!res.ok) {
    popup("This file cannot be viewed", (res.error || "not a text file") + "\n\n" + path, path);
    return;
  }
  let t = tabs.find((x) => x.kind === "file" && x.path === path);
  if (t) { select(t.id); return; }
  t = makeTab(name, "file", path);
  const head = t.el.querySelector(".file-info");
  const content = res.content || "";
  /* Colour it first, then label it: the header says which language it was read
     as, so "no colours here" is never a mystery. */
  const lang = hlPaint(t, name, content);
  head.textContent = path + "  \u00B7  " + (res.size || 0) + " bytes" +
    (res.truncated ? "  \u00B7  truncated" : "") + (res.lines ? "  \u00B7  " + res.lines + " lines" : "") +
    (lang ? "  \u00B7  " + (HL_TITLE[lang] || lang) : "");
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
  /* Shell output is a binary frame: arbitrary bytes, and a JSON string is
     UTF-8-only. `[ id length ][ session id ][ output ]`, because one socket
     carries several tabs and a binary frame has no other way to say which. */
  if (typeof ev.data !== "string") { termFrame(ev.data); return; }
  let m; try { m = JSON.parse(ev.data); } catch { return; }
  if (m.id && pending.has(m.id)) {
    const p = pending.get(m.id);
    clearTimeout(p.timer); pending.delete(m.id);
    p.resolve(m);
    return;
  }
  switch (m.type) {
    case "console": appendConsole(m.data || ""); break;
    case "ready": setReady(m.ready, m.note || "", m.busy); break;
    case "info": {
      $("status").textContent = m.status || "";
      /* The status tick carries the same busy flag the ready frames do, so a
         tab that was not looking when a turn started or ended re-syncs the Stop
         button here rather than waiting for a state change it has already
         missed (a reconnect, or a frame dropped by a full queue). */
      const wasBusy = busy;
      busy = !!m.busy;
      if (busy !== wasBusy) syncStop();
      const wd = m.work_dir || "";
      const wdEl = $("wd");
      /* A different working directory is a different tree — the cached
         listings and the expanded paths belong to the one being replaced. Only
         a *change* counts: `treeWd` starts as null, so the greeting's first
         info does not throw away the listing boot already asked for. */
      if (treeWd !== null && treeWd !== wd) { resetTree(); refreshTree(); }
      treeWd = wd;
      wdEl.textContent = wd;
      wdEl.title = wd;
      break;
    }
    case "term_exit": {
      const t = termTabs.get(m.session);
      if (t) termDead(t, m.code === null || m.code === undefined
        ? "shell exited" : "shell exited (" + m.code + ")");
      break;
    }
    case "term_reset":
      /* A new connection: the shells the old one owned are gone with it, so
         every terminal tab this page has is a pane over nothing. */
      for (const t of tabs) if (t.kind === "term") termDead(t, "connection lost");
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
  ws.binaryType = "arraybuffer";      /* so shell output arrives as bytes, not a Blob */
  ws.onopen = () => { $("conn").textContent = "connected"; $("conn").className = "on"; };
  ws.onmessage = onMessage;
  ws.onclose = () => {
    $("conn").textContent = "offline - reconnecting";
    $("conn").className = "off";
    setReady(false, "connection to jonnyq lost");
    /* The shells died with the socket: a shell nobody can type into is not one
       worth keeping, so the server took them down. Their tabs say so. */
    for (const t of tabs) if (t.kind === "term") termDead(t, "connection lost");
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
"##
);

#[cfg(test)]
mod tests {
    use super::{INDEX_HTML, PAGE_SCRIPT};

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

    /// The version is on the page in three places — the tab title, the top bar's
    /// brand and the terminal emulator's XTVERSION answer — and it is the build
    /// that is serving them, because all three are written by `concat!` from
    /// `CARGO_PKG_VERSION`. A page that advertised another version would send
    /// someone off to read the docs of a release they do not have.
    #[test]
    fn the_page_names_the_build_it_was_served_by() {
        let want = env!("CARGO_PKG_VERSION");
        assert!(
            INDEX_HTML.contains(&format!("<title>jonnyq console v{want}</title>")),
            "the tab title must carry the version"
        );
        let bar = INDEX_HTML
            .split("<div id=\"top\">")
            .nth(1)
            .and_then(|s| s.split("</b>").next())
            .expect("the top bar's brand");
        assert!(
            bar.contains("jonnyq") && bar.contains("id=\"ver\"") && bar.contains(want),
            "the brand shows the version of the build: {bar}"
        );
        // The emulator answers XTVERSION with it too, so a shell inside a
        // terminal tab can be asked which console it is running in.
        assert!(
            INDEX_HTML.contains(&format!("jonnyq console v{want}")),
            "XTVERSION must answer with the version"
        );
        // The version is styled as a footnote to the brand rather than as part
        // of it — and it sits inside the `<b>`, which sets the accent colour,
        // the letter-spacing and the bold it has to override.
        assert!(
            INDEX_HTML.contains("#ver { color: var(--fg4);"),
            "the version needs its own quiet rule"
        );
        // And the figure is not a hand-copied string that can go stale: every
        // version number on the page is one of these three splices.
        let numbers = regex::Regex::new(r"\bv\d+\.\d+\.\d+\b")
            .expect("regex")
            .find_iter(INDEX_HTML)
            .count();
        assert_eq!(
            numbers, 3,
            "the page may carry the version in its title, its brand and its \
             XTVERSION answer, and nowhere else"
        );
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

    /// The nine prose faces the human asked for, in the order asked for, with
    /// IBM Plex Sans Thai Looped as the default — and the stylesheet that offers
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

    /// The eighteen monospaced faces the human asked for, in the order asked
    /// for, with IBM Plex Mono as the default — the list the terminal panes and
    /// the file views draw in.
    ///
    /// They are offered to those surfaces only, because every one of them is
    /// monospaced (each was checked against the face Google Fonts serves: one
    /// advance width across the printable ASCII range, at 400 and at 700). A
    /// cell grid cannot be drawn in a proportional face, so the prose list is
    /// not offered to a pane and this list is not offered to the console.
    #[test]
    fn the_eighteen_mono_faces_are_offered_with_the_named_default() {
        let want = [
            "Source Code Pro",
            "Roboto Mono",
            "JetBrains Mono",
            "IBM Plex Mono",
            "Space Mono",
            "Fira Code",
            "Google Sans Code",
            "Ubuntu Mono",
            "Ubuntu Sans Mono",
            "Anonymous Pro",
            "Nova Mono",
            "Syne Mono",
            "Iosevka Charon Mono",
            "Sono",
            "Intel One Mono",
            "SUSE Mono",
            "Cascadia Mono",
            "Libertinus Mono",
        ];
        assert_eq!(want.len(), 18);
        for f in want {
            assert!(
                INDEX_HTML.contains(&format!("\"{f}\"")),
                "missing {f} in the code list"
            );
            assert!(
                INDEX_HTML.contains(&f.replace(' ', "+")),
                "{f} is not requested from Google Fonts"
            );
        }
        assert!(INDEX_HTML.contains("const DEFAULT_MONO_FONT = \"IBM Plex Mono\""));
        assert!(
            INDEX_HTML.contains("--mono-font: \"IBM Plex Mono\""),
            "the CSS default code face must match DEFAULT_MONO_FONT"
        );

        // The list is exactly those eighteen, in that order, and none of them
        // leaks into the prose list (and vice versa): the two pickers offer two
        // different kinds of face.
        let list = INDEX_HTML
            .split("const MONO_FONTS = [")
            .nth(1)
            .and_then(|s| s.split("];").next())
            .expect("MONO_FONTS");
        let at = |f: &str| {
            list.find(&format!("\"{f}\""))
                .unwrap_or_else(|| panic!("{f} is not in MONO_FONTS"))
        };
        let mut last = 0;
        for f in want {
            let here = at(f);
            assert!(here > last, "{f} is out of the order it was asked for in");
            last = here;
        }
        assert_eq!(
            list.matches('"').count() / 2,
            want.len(),
            "MONO_FONTS must hold exactly the eighteen asked for"
        );
        let prose = INDEX_HTML
            .split("const FONTS = [")
            .nth(1)
            .and_then(|s| s.split("];").next())
            .expect("FONTS");
        for f in want {
            assert!(
                !prose.contains(&format!("\"{f}\"")),
                "a monospaced face must not be offered for prose: {f}"
            );
        }
    }

    /// Code text defaults to 12pt, applies to the terminal panes and the file
    /// views through `--mono-size`, and is set by its own bounded number field.
    #[test]
    fn the_code_size_defaults_to_12pt_and_reaches_the_panes() {
        assert!(INDEX_HTML.contains("const DEFAULT_MONO_SIZE = 12"));
        assert!(INDEX_HTML.contains("--mono-size: 12pt"));
        assert!(
            INDEX_HTML.contains("<input id=\"code-size\" type=\"number\" min=\"8\" max=\"40\""),
            "the code size needs its own bounded field"
        );
        // The terminal pane and the file view are the two cell grids, and both
        // take the code size rather than the prose size.
        let css = INDEX_HTML
            .split("<style>")
            .nth(1)
            .and_then(|s| s.split("</style>").next())
            .expect("the style block");
        let term = css
            .split("\n.term-out {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the .term-out rule");
        assert!(
            term.contains("var(--mono-size)") && term.contains("var(--mono-font)"),
            "a terminal pane takes the code face and size: {term}"
        );
        let file = css
            .split("\n.file-out {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the .file-out rule");
        assert!(
            file.contains("var(--mono-size)") && file.contains("var(--mono-font)"),
            "a file view takes the code face and size: {file}"
        );
        // And the code size is applied as a point size, like the prose size.
        let apply = INDEX_HTML
            .split("function applyType()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("applyType");
        assert!(
            apply.contains("setProperty(\"--mono-size\", typeface.codeSize + \"pt\")"),
            "the code size is a point size: {apply}"
        );
    }

    /// The terminal pane's scrollback lives in a `div` above the grid, and the
    /// three things that make it work are all things the JS tests cannot see:
    /// the fake DOM has no layout, so it cannot notice that an empty row of
    /// history collapsed to zero height, that a `div` swallowed the spaces a
    /// column-aligned `ls` is made of, or that the browser was left free to
    /// "correct" a reader who had scrolled up.
    #[test]
    fn the_scrollback_is_a_block_of_rows_the_browser_cannot_reshape() {
        let css = INDEX_HTML
            .split("<style>")
            .nth(1)
            .and_then(|s| s.split("</style>").next())
            .expect("the style block");
        let row = css
            .split("\n.term-hist-row {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the .term-hist-row rule");
        /* One row, one height — and one the pane can count in. A `div` with
        nothing on it has no line box, so without a height of its own a blank
        line of history is zero pixels and every row below it slides up. */
        assert!(
            row.contains("min-height:") && row.contains("white-space: pre"),
            "a scrollback row needs its own height and its spaces kept: {row}"
        );
        let pane = css
            .split("\n.term-out {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the .term-out rule");
        /* The pane decides what a scrolled-up reader is anchored to; the
        browser's own idea cannot tell a row that entered the history from a
        row the reader asked to see, and would move them for the wrong one. */
        assert!(
            pane.contains("overflow-anchor: none"),
            "the pane keeps its reader still itself: {pane}"
        );

        // The scrollback is a real element in the pane, above the grid it
        // shares the pane's face with.
        let markup = INDEX_HTML
            .split("kind === \"term\"")
            .nth(1)
            .and_then(|s| s.split("panel.innerHTML =").nth(1))
            .and_then(|s| s.split("';").next())
            .expect("the terminal panel's markup");
        let hist = markup
            .find("term-hist")
            .expect("a terminal pane needs its scrollback element");
        assert!(
            hist < markup.find("term-grid").expect("the grid"),
            "the scrollback sits above the grid, not below it: {markup}"
        );

        // And the rows are drawn one at a time: the pane appends and drops, it
        // never re-parses the whole history per line of output.
        let sync = INDEX_HTML
            .split("function termHistSync(me, h)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("termHistSync");
        assert!(
            sync.contains("appendChild") && sync.contains("removeChild"),
            "rows are spliced in and out: {sync}"
        );
        assert!(
            !sync.contains("innerHTML =") || sync.contains("termHistRow"),
            "the scrollback is never rebuilt wholesale per frame: {sync}"
        );
    }

    /// Reaching the scrollback has to work with the pointer as well as the
    /// keyboard, and all three ways that can fail are in the page's own markup
    /// rather than in its logic — so the JS harness cannot see any of them.
    ///
    /// A pane that never overflows has nothing to scroll, so it must be allowed
    /// to overflow. A press on the bar is delivered to the pane's own press
    /// handler, because the bar is drawn inside the element's box; a handler that
    /// cancels every press cancels the drag before it starts, leaving a bar that
    /// is visible and useless. And the keyboard's route in must not be the
    /// shell's: `less` and `man` are the reason PgUp belongs to the program.
    #[test]
    fn a_reader_reaches_the_scrollback_with_the_pointer_and_the_keyboard() {
        let css = INDEX_HTML
            .split("<style>")
            .nth(1)
            .and_then(|s| s.split("</style>").next())
            .expect("the style block");
        let pane = css
            .split("\n.term-out {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the .term-out rule");
        assert!(
            pane.contains("overflow: auto"),
            "the pane must be allowed to overflow, or there is nothing to scroll: {pane}"
        );

        // The press handler spares the bars before it cancels anything.
        let press = INDEX_HTML
            .split("out.addEventListener(\"mousedown\"")
            .nth(1)
            .and_then(|s| s.split("\n  });").next())
            .expect("the pane's press handler");
        let spare = press
            .find("e.preventDefault()")
            .expect("the handler cancels a press, so it must check first");
        assert!(
            press[..spare].contains("e.offsetX >= out.clientWidth")
                && press[..spare].contains("e.offsetY >= out.clientHeight"),
            "a press past the pane's client box is its scrollbar, and must be let \
             through before the cancel that would stop its drag: {press}"
        );

        // Shift-PgUp/PgDn page the pane, and the check comes before the key map
        // that would otherwise hand both keys to the shell.
        let key = INDEX_HTML
            .split("function termKey(t, e)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("termKey");
        let paging = key
            .find("termScroll")
            .expect("the pane's keys must be able to reach its scrollback");
        let to_shell = key
            .find("PageUp: \"5~\"")
            .expect("PgUp alone stays the shell's key");
        assert!(
            paging < to_shell,
            "Shift-PgUp has to be taken before the map that sends it to the shell"
        );
        assert!(
            key.contains("e.shiftKey && (e.key === \"PageUp\" || e.key === \"PageDown\")"),
            "and only with Shift, so a program's paging keys are untouched"
        );

        // Paging is in rows, so a bigger code size pages the same text rather
        // than the same number of pixels.
        let scroll = INDEX_HTML
            .split("function termScroll(me, rows)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("termScroll");
        assert!(
            scroll.contains("me.cellH") && scroll.contains("out.scrollTop + step"),
            "the step is a count of rows times the row height: {scroll}"
        );
        assert!(
            scroll.contains("out.scrollHeight - out.clientHeight"),
            "and it cannot run past either end: {scroll}"
        );
    }

    /// The element that draws the terminal's cells is a `pre`, and a `pre` left
    /// to inherit draws in the browser's own monospace: an inherited value loses
    /// to a user-agent declaration, so `.term-out`'s font never reached the text
    /// and picking a new code face changed the pane's *padding* and the measure
    /// span while the glyphs stayed exactly as they were. The grid must therefore
    /// name the two properties itself.
    ///
    /// It must also name them *identically* to the pane. The span that measures
    /// a cell is a `span` and so inherits from the pane; if the grid were drawn
    /// in another face, the cell width told to the shell would not be the width
    /// the glyphs are drawn at, and the columns would drift the moment a face
    /// with different metrics was picked.
    #[test]
    fn the_terminal_grid_names_the_code_face_itself_because_a_pre_inherits_last() {
        let css = INDEX_HTML
            .split("<style>")
            .nth(1)
            .and_then(|s| s.split("</style>").next())
            .expect("the style block");
        let grid = css
            .split("\n.term-grid {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the .term-grid rule");
        assert!(
            grid.contains("var(--mono-font)") && grid.contains("var(--mono-size)"),
            "a `pre` must be told the code face, or the UA's own monospace wins: {grid}"
        );

        // And the grid's font must be the pane's font, character for character:
        // the measure span inherits from the pane, so a grid drawn in a face
        // with different metrics would retile the shell to the wrong size.
        let font_of = |rule: &str| -> String {
            let at = rule
                .find("font:")
                .unwrap_or_else(|| panic!("the rule declares no font: {rule}"));
            rule[at + "font:".len()..]
                .split(';')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        let pane = css
            .split("\n.term-out {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the .term-out rule");
        assert_eq!(
            font_of(pane),
            font_of(grid),
            "the grid is drawn in the same face and leading the cell is measured in"
        );
    }

    /// Two font names and four fields are wider than the top bar of a
    /// half-screen window, and a control pushed off the edge is one that cannot
    /// be reached. So nothing in the type block may be unshrinkable, the pickers
    /// have a floor well below their full width, and the block wraps rather than
    /// overflow.
    #[test]
    fn the_type_controls_cannot_be_pushed_off_the_bar() {
        let css = INDEX_HTML
            .split("<style>")
            .nth(1)
            .and_then(|s| s.split("</style>").next())
            .expect("the style block");
        let typeRule = css
            .split("\n#type {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the #type rule");
        assert!(
            typeRule.contains("flex-wrap: wrap"),
            "the block must wrap within itself: {typeRule}"
        );
        assert!(
            !typeRule.contains("flex: 0 0 auto"),
            "an unshrinkable block cannot give up room: {typeRule}"
        );
        let pickers = css
            .split("\n#font, #code {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the picker rule");
        assert!(
            pickers.contains("min-width:") && !pickers.contains("flex: 0 0 auto"),
            "the pickers must shrink: {pickers}"
        );
    }

    /// A terminal pane is a grid of cells, so changing its face changes the cell
    /// — and the shell has to be told. The page re-measures every live shell
    /// when either code control is applied.
    #[test]
    fn a_code_font_change_retiles_the_live_shells() {
        let apply = INDEX_HTML
            .split("function applyType()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("applyType");
        assert!(
            apply.contains("refitTerms()"),
            "applying the code face must re-measure the panes: {apply}"
        );
        let refit = INDEX_HTML
            .split("function refitTerms()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("refitTerms");
        assert!(
            refit.contains("termTabs.values()"),
            "the live shells are the thing to re-measure: {refit}"
        );
        assert!(refit.contains("termFit(t)"), "{refit}");
        // A face that arrives after the first paint is a change of cell too.
        assert!(
            INDEX_HTML.contains("document.fonts.ready.then(refitTerms"),
            "a late-landing webfont must retile the panes too"
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
        // Out-of-range and non-numeric input must be clamped, not obeyed. Each
        // pair clamps to its own default, so an empty code box falls back to
        // 12pt rather than to the prose size.
        let clamp = INDEX_HTML
            .split("function clampSize(v, dflt)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("clampSize");
        assert!(clamp.contains("Math.min") && clamp.contains("Math.max"));
        assert!(clamp.contains("Number.isFinite"));
        assert!(
            clamp.contains("return dflt"),
            "a blank field takes its pair's default: {clamp}"
        );
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

    /// The console, the prompt box and the chrome take the chosen prose face and
    /// the prose size; the file views and the terminal panes take the chosen
    /// code face and the code size, because source and a cell grid are read by
    /// their columns. Neither kind of face is applied to the other's surfaces.
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
            file.contains("var(--mono-font)"),
            "a file view must draw in the code face: {file}"
        );
        assert!(
            !file.contains("--ui-font"),
            "a file view must not take the prose face, which is proportional: {file}"
        );
        // The prose surfaces take the prose pair, and nothing takes both.
        assert!(css.contains("#console-out, .file-out"));
        assert!(css.contains("font-size: var(--ui-size)"));
        assert!(
            css.contains("#console-out { font-family: var(--ui-font); }"),
            "the console is the thing the picker was asked for"
        );
        // Both custom properties have a generic tail in :root, so a face that
        // never downloaded still draws — monospaced for the code pair, which is
        // what keeps a file's columns and a terminal's cells lined up.
        let root = css
            .split(":root {")
            .nth(1)
            .and_then(|s| s.split("}").next())
            .expect("the :root rule");
        for line in root.lines() {
            if line.contains("--mono-font:") {
                assert!(
                    line.trim_end().ends_with("monospace;"),
                    "the code stack must end in a generic monospace: {line}"
                );
            }
            if line.contains("--ui-font:") {
                assert!(
                    line.contains("sans-serif"),
                    "the prose stack must end in a generic: {line}"
                );
            }
        }
        // And the font field is labelled for what it now is, next to the code
        // picker rather than alone.
        assert!(INDEX_HTML.contains("id=\"code\""));
        assert!(INDEX_HTML.contains("id=\"code-size\""));
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

    /// Two listings of the same folder racing each other (boot, the greeting's
    /// `reset`, the poll and a click on a folder) would both render, so the
    /// second clears what the first drew. A listing in flight must be reused,
    /// not duplicated.
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
        // The poll marks every folder it is about to ask for as in flight up
        // front, so neither a folder clicked open mid-poll nor the redraw of a
        // folder that just failed can start a second request for it.
        assert!(
            refresh.contains("treeLoading.add(p)"),
            "the listed folders must be registered before the requests go out: {refresh}"
        );
    }

    /// The tree has no refresh button any more — it keeps itself current. So the
    /// page must actually ask again on its own: on a timer, and immediately when
    /// the tab comes back from being hidden. A timer-less panel would be a
    /// strictly worse replacement for the button that was removed.
    #[test]
    fn the_tree_relists_itself_without_a_button() {
        assert!(
            !INDEX_HTML.contains("id=\"refresh\""),
            "the tree must not grow a refresh button back"
        );
        let head = INDEX_HTML
            .split("<div id=\"side-head\">")
            .nth(1)
            .and_then(|s| s.split("</div>").next())
            .expect("the tree header");
        assert!(
            !head.contains("<button"),
            "the tree header is the working directory only: {head}"
        );

        let tick = INDEX_HTML
            .split("function treeTick()")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("treeTick");
        assert!(
            tick.contains("refreshTree()"),
            "the tick does the listing: {tick}"
        );
        // Offline: every request would wait out its connect deadline first.
        // Hidden: nobody is reading it, and browsers throttle it anyway.
        assert!(
            tick.contains("OPEN()") && tick.contains("document.hidden"),
            "the tick stays quiet while offline or hidden: {tick}"
        );
        assert!(
            INDEX_HTML.contains("setInterval(treeTick, TREE_POLL_MS)"),
            "the tick has to run on a timer"
        );
        assert!(
            INDEX_HTML.contains("\"visibilitychange\"") && INDEX_HTML.contains("treeTick()"),
            "coming back to the tab has to list it again at once"
        );
        // And the interval is a sane one: a request per folder per tick, so
        // something sub-second would machine-gun the disk, while minutes is not
        // "automatic".
        let ms: u32 = INDEX_HTML
            .split("const TREE_POLL_MS = ")
            .nth(1)
            .and_then(|s| s.split(';').next())
            .and_then(|s| s.trim().parse().ok())
            .expect("TREE_POLL_MS");
        assert!(
            (1000..=10_000).contains(&ms),
            "poll every {ms}ms is not a sane cadence"
        );
    }

    /// Polling only pays for itself if an unchanged listing costs nothing: a
    /// rebuild throws away the panel's scroll position and yanks rows out from
    /// under a click in progress. So the fresh listing is compared against the
    /// one drawn, and only a difference redraws. Size is deliberately *not*
    /// part of the comparison — no row shows a size, so a file being written to
    /// is not a visible change.
    #[test]
    fn an_unchanged_poll_does_not_rebuild_the_tree() {
        let sig = INDEX_HTML
            .split("function treeSig(node)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("treeSig");
        assert!(
            sig.contains("e.name") && sig.contains("e.dir"),
            "the shape of a listing is its entries' names and kinds: {sig}"
        );
        assert!(
            !sig.contains("size"),
            "a file's size is not something a row shows, so it must not force a redraw: {sig}"
        );

        let set = INDEX_HTML
            .split("function setTree(path, node)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("setTree");
        assert!(
            set.contains("treeSig(was) !== treeSig(node)"),
            "setTree must report whether the listing changed: {set}"
        );

        let draw = INDEX_HTML
            .split("function drawTree()")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("drawTree");
        assert!(
            draw.contains("scrollTop"),
            "a redraw keeps the panel's scroll position: {draw}"
        );

        // Every redraw path goes through that comparison.
        for (name, body) in [
            ("askTree", INDEX_HTML.split("function askTree(path)").nth(1)),
            (
                "refreshTree",
                INDEX_HTML.split("async function refreshTree()").nth(1),
            ),
        ] {
            let body = body
                .and_then(|s| s.split("\n}\n").next())
                .unwrap_or_else(|| panic!("{name}"));
            assert!(
                body.contains("setTree(") && body.contains("drawTree()"),
                "{name} may only redraw through setTree: {body}"
            );
        }
    }

    /// What makes automatic listing tolerable instead of maddening: the folders
    /// the reader expanded are state of their own, so a redraw re-expands them,
    /// an open folder is polled (a folded one is not), and one that vanishes
    /// from disk folds itself up instead of leaving rows for a folder that is
    /// gone. Without this, an update every few seconds would fold the whole tree
    /// back to one line — worse than the button it replaced.
    #[test]
    fn a_redraw_keeps_the_readers_own_folders_open() {
        assert!(
            INDEX_HTML.contains("const treeOpen = new Set()"),
            "expanded folders have to be remembered"
        );
        let draw = INDEX_HTML
            .split("function treeNodeEl(path)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("treeNodeEl");
        assert!(
            draw.contains("treeOpen.has(item.path)"),
            "a redraw draws the children of an open folder: {draw}"
        );
        assert!(
            draw.contains("treeOpen.add(item.path)") && draw.contains("treeOpen.delete(item.path)"),
            "a click is what decides whether a folder is open: {draw}"
        );

        let prune = INDEX_HTML
            .split("function pruneTree()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("pruneTree");
        assert!(
            prune.contains("treeOpen.delete(p)"),
            "a folder that is gone stops being expanded: {prune}"
        );
        assert!(
            prune.contains("treeOpen.has(p)"),
            "listings of folded-up folders are dropped, so the poll stays cheap: {prune}"
        );

        let refresh = INDEX_HTML
            .split("async function refreshTree()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("refreshTree");
        assert!(
            refresh.contains("...treeOpen"),
            "every visible folder is re-listed, not only the root: {refresh}"
        );
    }

    /// A folder that cannot be read is the one thing polling could turn into a
    /// popup per tick, which is noise rather than information. So it explains
    /// itself inside the tree where its rows would have been, and only the
    /// working directory failing outright earns a popup — said once per distinct
    /// reason, not once per tick.
    #[test]
    fn a_folder_that_cannot_be_read_says_so_once() {
        let ask = INDEX_HTML
            .split("function askTree(path)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("askTree");
        assert!(
            ask.contains("noteTreeFail(path"),
            "a failed folder is remembered, not swallowed: {ask}"
        );
        assert!(
            !ask.contains("popup("),
            "a folder's own failure must not pop up while the panel polls: {ask}"
        );

        let draw = INDEX_HTML
            .split("function treeNodeEl(path)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("treeNodeEl");
        assert!(
            draw.contains("treeFails.get(path)"),
            "the reason is drawn inside the folder: {draw}"
        );

        let err = INDEX_HTML
            .split("function treeError(note)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("treeError");
        assert!(
            err.contains("treeErr === msg"),
            "the same failure may not stack a popup per tick: {err}"
        );
        let note = INDEX_HTML
            .split("function noteTreeFail(path, why_)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("noteTreeFail");
        assert!(
            note.contains("was !== note"),
            "only news is worth a redraw, or an unreadable folder would rebuild the panel every tick: {note}"
        );
    }

    /// Automatic listing means answers arrive at times nobody chose: a folder's
    /// listing can come back after the session has moved to another working
    /// directory, and drawing it would put the old folder's rows back over the
    /// new one's. So every answer carries the generation it was asked under.
    /// And the greeting's first `info` must *not* count as a move, or the very
    /// first load throws away the listing boot just asked for.
    #[test]
    fn a_late_listing_cannot_redraw_a_discarded_tree() {
        assert!(
            INDEX_HTML.contains("let treeGen = 0"),
            "the tree needs a generation counter"
        );
        let ask = INDEX_HTML
            .split("function askTree(path)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("askTree");
        assert!(
            ask.contains("const gen = treeGen") && ask.contains("still()"),
            "a listing must know whether the tree it belongs to still exists: {ask}"
        );
        let refresh = INDEX_HTML
            .split("async function refreshTree()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("refreshTree");
        assert!(
            refresh.contains("gen !== treeGen"),
            "a poll answered after a reset must be dropped: {refresh}"
        );

        let info = INDEX_HTML
            .split("case \"info\": {")
            .nth(1)
            .and_then(|s| s.split("\n    }").next())
            .expect("the info handler");
        assert!(
            info.contains("treeWd !== null"),
            "the greeting's first info is not a change of working directory: {info}"
        );
        assert!(
            info.contains("resetTree()") && info.contains("refreshTree()"),
            "a real change clears the tree and lists the new folder: {info}"
        );
    }

    /// The page must be executable JavaScript, not merely well-balanced tags: it
    /// is served inline, and a syntax error there is a console whose tree simply
    /// stops updating. Checked by V8 rather than eyeballed.
    #[test]
    fn the_pages_script_is_valid_javascript() {
        let script = INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let status = std::process::Command::new("node")
            .arg("--check")
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .as_mut()
                    .expect("stdin")
                    .write_all(script.as_bytes())?;
                child.wait_with_output()
            });
        // Skipped where node is not installed (a checkout without a JS
        // toolchain still builds and runs jonnyq); asserted wherever it is.
        let Ok(out) = status else {
            eprintln!("node is not installed, skipping the syntax check");
            return;
        };
        assert!(
            out.status.success(),
            "the page's script does not parse:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    /// The browser's answer box for `ask_user` (`-web <port>`): the question, a
    /// field to answer it with, and a Send button — plus the two messages the
    /// server uses to open and take back the popup. Without this an `ask_user`
    /// call in a headless session had no way to be answered at all.
    #[test]
    fn the_ask_user_popup_can_be_answered() {
        for needle in [
            "id=\"ask-mask\"",     /* the overlay                        */
            "id=\"ask-q\"",        /* the question itself                */
            "id=\"ask-answer\"",   /* the answer field                   */
            "id=\"ask-send\"",     /* and the button that sends it       */
            "type: \"answer\"",    /* the message the answer goes over   */
            "case \"ask\":",       /* the server opening a question      */
            "case \"ask_close\":", /* and taking it back                 */
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
        assert!(
            send.contains("ask: q.id"),
            "the answer must carry its id: {send}"
        );
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
        assert!(
            !render.contains("asks.splice"),
            "rendering may not drop it: {render}"
        );

        // No hide control, no hidden flag, no reminder badge to bring a hidden
        // question back — so nothing can strand a question out of reach.
        for gone in ["ask-hide", "askHidden", "ask-badge", "askShown"] {
            assert!(
                !INDEX_HTML.contains(gone),
                "{gone} must be gone from the page"
            );
        }
        let wire = INDEX_HTML
            .split("function wireAsk()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("wireAsk");
        assert!(
            !wire.contains("asks.splice"),
            "nothing here may drop it: {wire}"
        );
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
        assert!(
            render.contains("asks.length - 1"),
            "the count of those behind: {render}"
        );
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
        assert!(
            onclose.contains("asks.length = 0"),
            "stale questions are dropped: {onclose}"
        );
        assert!(
            onclose.contains("renderAsk()"),
            "and the popup is redrawn: {onclose}"
        );
    }

    /// **Stop** (the browser's Ctrl-C): a button next to Send, live only while
    /// a turn is in flight, that sends one `stop` message and leaves the
    /// transcript to say how the turn ended.
    ///
    /// The state it lights from is the server's `busy` flag rather than
    /// `!ready`, because "not ready" has several reasons and only one of them
    /// has something to cancel — a button driven by the textarea's disabled
    /// state would offer to stop a session that has no model set, or one whose
    /// link has dropped.
    #[test]
    fn stop_is_the_pages_ctrl_c() {
        for needle in [
            "id=\"stop\"",    /* the button itself                 */
            "sendStop",       /* what pressing it does             */
            "type: \"stop\"", /* the message it sends              */
            "case \"ready\": setReady(m.ready, m.note || \"\", m.busy)",
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }
        // It sits with Send, and is greyed like it: `disabled` is what keeps a
        // press from doing nothing at all.
        let row = INDEX_HTML
            .split("id=\"prompt-row\"")
            .nth(1)
            .and_then(|s| s.split(";").next())
            .expect("the button row");
        assert!(
            row.contains("id=\"stop\"") && row.contains("id=\"send\""),
            "Stop belongs beside Send: {row}"
        );
        assert!(
            INDEX_HTML.contains("#stop:disabled"),
            "and it has a greyed-out look"
        );

        // The same button has to exist inside the ask popup: the mask covers
        // the prompt box, so a session blocked on a question would otherwise be
        // uncancellable from the browser — and a headless session has no
        // terminal to press Ctrl-C at.
        assert!(
            INDEX_HTML.contains("id=\"ask-stop\""),
            "the popup must be able to stop the turn that is asking"
        );
        let wire = INDEX_HTML
            .split("function wireAsk()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("wireAsk");
        assert!(
            wire.contains("ask-stop") && wire.contains("sendStop"),
            "and wired to the same request: {wire}"
        );

        let stop = INDEX_HTML
            .split("function sendStop(btn)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("sendStop");
        assert!(
            stop.contains("rpc({ type: \"stop\" })"),
            "it must send: {stop}"
        );
        // A press with nothing running is refused locally, not sent, so the
        // button cannot be mistaken for something that just happened to do
        // nothing.
        assert!(
            stop.contains("if (!busy) return"),
            "idle press is a no-op: {stop}"
        );
        // A reply saying nothing was stopped says so, rather than going quiet.
        assert!(
            stop.contains("Nothing to stop"),
            "a late press must explain itself: {stop}"
        );
    }

    /// The button's lit/greyed state comes from the server's `busy` flag, on
    /// every frame that carries it — and while a stop request is in flight the
    /// in-flight label must survive a frame that arrives in between, or the
    /// button would flicker back to "Stop" while the turn is unwinding.
    #[test]
    fn the_stop_button_tracks_the_servers_busy_flag() {
        let set = INDEX_HTML
            .split("function setReady(state, note, busyNow)")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("setReady");
        assert!(
            set.contains("busy = !!busyNow"),
            "ready frames carry the flag: {set}"
        );
        assert!(set.contains("syncStop()"), "and redraw the button: {set}");

        let sync = INDEX_HTML
            .split("function syncStop()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("syncStop");
        assert!(
            sync.contains("stop.disabled = !busy"),
            "lit exactly while a turn runs: {sync}"
        );
        assert!(
            sync.contains("stopping\\u2026"),
            "a request in flight keeps its own label: {sync}"
        );

        // The status tick re-syncs it too, so a tab that missed a `ready` frame
        // (a reconnect, a dropped frame) is not left on a stale button.
        let info = INDEX_HTML
            .split("case \"info\": {")
            .nth(1)
            .and_then(|s| s.split("\n    }").next())
            .expect("the info handler");
        assert!(
            info.contains("!!m.busy") && info.contains("syncStop()"),
            "the status line re-reads the flag too: {info}"
        );
    }

    /// The terminal tab is JS in a string, so Rust cannot test it directly —
    /// this runs it. `page_harness.js` is the fake DOM, the page's own
    /// `<script>` spliced in, and the assertions after it; node executes the
    /// whole thing and prints `N passed, M failed`.
    ///
    /// Spliced rather than duplicated, so a test cannot pass while the shipped
    /// page is broken: the emulator, the key handler, the frame decoder and the
    /// resize are the ones in [`INDEX_HTML`], in the page's own scope.
    /// A file tab is coloured by the page now, and the two ways that can be
    /// quietly wrong are both invisible to the JS: a token class with no CSS
    /// rule draws in default colour and looks like a file with no colours at
    /// all, and a language named by the extension table but missing from the
    /// language table would throw the moment such a file was opened.
    #[test]
    fn every_syntax_class_has_a_colour_and_every_language_a_definition() {
        let script = INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let css = INDEX_HTML
            .split("<style>")
            .nth(1)
            .and_then(|s| s.split("</style>").next())
            .expect("the style block");

        /* The classes the scanner can emit: every `emit("s-…"` and the class
        strings the role helpers hand back. */
        let classes = regex::Regex::new(r#""(s-[a-z]+)""#).expect("regex");
        let mut seen: Vec<String> = classes
            .captures_iter(script)
            .map(|m| m[1].to_string())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen.sort();
        seen.dedup();
        assert!(seen.len() >= 8, "the scanner emits {seen:?}");
        for cls in &seen {
            let rule = format!(".{cls} {{");
            assert!(
                css.contains(&rule),
                "the scanner emits {cls} but the CSS has no `{rule}` rule, so it \
                 would draw in the file view's default colour"
            );
        }

        /* The languages the extension and shebang tables can name. */
        let names = regex::Regex::new(r#": "([a-z]+)""#).expect("regex");
        let table = |head: &str| -> String {
            script
                .split(head)
                .nth(1)
                .and_then(|s| s.split("};").next())
                .unwrap_or("")
                .to_string()
        };
        let mut langs: Vec<String> = Vec::new();
        for head in [
            "const HL_EXT = {",
            "const HL_FILE = {",
            "const HL_SHEBANG = {",
        ] {
            let body = table(head);
            assert!(!body.is_empty(), "missing {head}");
            for m in names.captures_iter(&body) {
                langs.push(m[1].to_string());
            }
        }
        langs.sort();
        langs.dedup();
        /* The empty ones are the deliberate "nothing to colour" entries. */
        let defined = table("const HL_LANG = {");
        for lang in &langs {
            let rule = format!("{lang}: {{");
            assert!(
                defined.contains(&rule),
                "a file can be detected as {lang} but HL_LANG has no entry for it"
            );
        }
        /* And the header's friendly name exists for every defined language, so
        the pane never says `undefined`. */
        let titles = table("const HL_TITLE = {");
        for m in regex::Regex::new(r"(?m)^  ([a-z]+): \{")
            .expect("regex")
            .captures_iter(&defined)
        {
            assert!(
                titles.contains(&format!("{}:", &m[1])),
                "HL_TITLE does not know the language {}",
                &m[1]
            );
        }
    }

    /// The file view must never be *told* to show markup: the content goes
    /// through `esc` on its way in, whatever the language. Pinned here as well
    /// as in the JS because this is the one place the feature could regress into
    /// an injection in the page.
    #[test]
    fn a_file_view_is_escaped_on_its_way_in() {
        let script = INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let render = script
            .split("function hlRender(src, lang)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("hlRender");
        assert!(render.contains("hlSpan("), "it goes through the wrapper");
        let span = script
            .split("function hlSpan(cls, text) {")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("hlSpan");
        assert!(
            span.contains("esc(text)"),
            "every token is escaped before it is wrapped: {span}"
        );
        assert!(
            !span.contains("innerHTML"),
            "the wrapper builds a string; the pane is written once, by hlPaint"
        );
        /* And the plain-text path is textContent, so a file the page will not
        colour is still never markup. */
        let paint = script
            .split("function hlPaint(t, name, content) {")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("hlPaint");
        assert!(paint.contains("out.textContent = content"), "{paint}");
        assert!(
            paint.contains("content.length > HL_MAX"),
            "and a file too big to be worth colouring is capped rather than frozen: {paint}"
        );
    }

    /// A file tab offers a **Download**, and so does the popup that says a file
    /// cannot be viewed — the two places a reader finds a file they may want a
    /// copy of. Pinned on the page side (the button exists, is wired, points at
    /// the route and carries the tab's own path); the route answering with the
    /// file's bytes is the server's own tests.
    #[test]
    fn a_file_can_be_saved_from_the_page_that_shows_it() {
        for needle in [
            "class=\"dl\"",    /* the button in a file tab's head */
            "id=\"popup-dl\"", /* and the one in the notice box     */
            "Download",        /* spelled out for the reader        */
            "downloadFile",    /* what a press does                 */
            "/file?path=",     /* the route that hands out the file */
        ] {
            assert!(INDEX_HTML.contains(needle), "missing {needle}");
        }

        /* One anchor to the route, its path percent-encoded so a name with a
        slash, a space or a Thai word in it survives being a URL. */
        let url = INDEX_HTML
            .split("const downloadUrl = ")
            .nth(1)
            .and_then(|s| s.split("\n").next())
            .expect("downloadUrl");
        assert!(
            url.contains("encodeURIComponent(path)"),
            "the path must be encoded: {url}"
        );
        let dl = INDEX_HTML
            .split("function downloadFile(path)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("downloadFile");
        assert!(dl.contains("createElement(\"a\")"), "a link, {dl}");
        assert!(
            dl.contains("a.download ="),
            "so the browser saves it instead of navigating the console away: {dl}"
        );
        assert!(
            !dl.contains("window.open") && !dl.contains("location.href"),
            "nothing may open a window or move this tab off the console: {dl}"
        );
        assert!(
            dl.contains("if (!path) return"),
            "and it does nothing to nothing: {dl}"
        );

        /* The button is wired where the tab is built, from the tab's own path —
        so several file tabs each save the file they show. */
        let wire = INDEX_HTML
            .split("if (kind === \"console\") wireConsole(panel);")
            .nth(1)
            .and_then(|s| s.split("\n  return t;").next())
            .expect("the wiring of a new tab");
        assert!(
            wire.contains(".dl") && wire.contains("downloadFile(t.path)"),
            "a file tab's own button, its own file: {wire}"
        );

        /* The notice box is shared by everything on the page that can go wrong,
        so the button hides unless the notice named a file: a Download with
        nothing to download is a button that lies. */
        let box_ = INDEX_HTML
            .split("function popup(title, body, path)")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("popup");
        assert!(
            box_.contains("classList.toggle(\"hide\", !popupPath)"),
            "the button shows only for a notice that is about a file: {box_}"
        );
        assert!(
            INDEX_HTML.contains("popupPath = path || \"\";"),
            "and a notice that passed no path clears the last one's"
        );
        assert!(
            INDEX_HTML.contains("popup(\"This file cannot be viewed\", (res.error || \"not a text file\") + \"\\n\\n\" + path, path)"),
            "the refusal of a file passes the file, so it can be had as a download"
        );
    }

    #[test]
    fn the_terminal_emulation_and_the_tab_are_tested_through_node() {
        let harness = include_str!("page_harness.js");
        let script = INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let joined = harness.replace(PAGE_SCRIPT, script);
        assert!(
            !joined.contains(PAGE_SCRIPT),
            "the harness lost its splice point"
        );

        let dir = crate::testsupport::TempDir::new("page-harness");
        let js = dir.path().join("harness.js");
        if let Err(e) = std::fs::write(&js, joined.as_bytes()) {
            panic!("cannot write the harness: {e}");
        }
        let run = std::process::Command::new("node")
            .arg(&js)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        // Skipped where node is not installed; asserted wherever it is.
        let Ok(child) = run else {
            eprintln!("node is not installed, skipping the terminal page tests");
            return;
        };
        let out = match child.wait_with_output() {
            Ok(o) => o,
            // The JS itself is checked elsewhere by `node --check`; a harness
            // that cannot start is the toolchain's problem, not the page's.
            Err(e) => {
                eprintln!("cannot run node ({e}), skipping the terminal page tests");
                return;
            }
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "the terminal page tests failed (exit {:?})\n{}{}\nharness written to {}",
            out.status.code(),
            stdout,
            stderr,
            js.display()
        );
        // A harness that ran nothing is not a pass: the count is checked, not
        // just the exit status, so a silently-empty suite cannot look green.
        let counts = stdout
            .lines()
            .last()
            .unwrap_or("")
            .split_whitespace()
            .next()
            .and_then(|n| n.parse::<usize>().ok())
            .unwrap_or(0);
        assert!(
            counts >= 60,
            "the terminal page tests ran only {counts} assertions:\n{stdout}"
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
