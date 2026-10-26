# jonnyq

**Interactive coding agent CLI.** A Rust REPL that talks to a local or remote LLM, lets the model call tools (read/write files, run commands, search the web) in a loop until the task is done, and can drive an entire project from a `requirements.md` file.

Language: **English** · [ไทย](README.Thai.md)

```
~/dev/my-project$ jonnyq -model qwen3:14b
qwen3:14b (32768 token) > fix the file-not-found bug in main.rs
Thinking (2026-10-24 10:23:41) | ctx ~1,910/32,768 (~5%) left ~30,858 | +0.4s | last req 1,120 in/380 out, 22.4 tok/s, 17.0s
...
Tool call (2026-10-24 10:23:45) | ctx ~2,140/32,768 (~6%) left ~30,628 | +4.1s | req 2,010 in/96 out, 24.1 tok/s, 4.0s
read_file({"path": "src/main.rs"})
Answer (2026-10-24 10:23:52) | ctx ~3,020/32,768 (~9%) left ~29,748 | +11.2s | last req 2,480 in/410 out, 23.7 tok/s, 7.3s
...
[preload=n/a prompt_eval=1.2s generate=41.6s token_in=8210 token_out=1042 total_token=9252 tok/s=25.05 wall=52.8s | ctx 3,020/32,768 (9%) left 29,748]
```

---

## Contents

1. [What it is](#what-it-is)
2. [How it works](#how-it-works)
3. [Requirements](#requirements)
4. [Installation](#installation)
5. [Quick start](#quick-start)
6. [Configuration](#configuration)
7. [Slash commands](#slash-commands)
8. [Tools](#tools)
9. [Automated coding: `/plan` → `.progress` → `/coding`](#automated-coding-plan--progress--coding)
10. [Skills](#skills)
11. [Web console](#web-console)
12. [Notifications (ntfy)](#notifications-ntfy)
13. [Files created at runtime](#files-created-at-runtime)
14. [Security notes](#security-notes)
15. [License](#license)

---

## What it is

- An interactive terminal agent: you type a request, the model thinks, calls tools, and answers — repeating until the job is finished.
- Works with **Ollama** (native API) and any **OpenAI-compatible** endpoint (OpenAI, LM Studio, llama.cpp server, vLLM, OpenRouter, …).
- Prompt-driven project automation: `/plan`, `/plan_for_human`, `/coding`, `/coding_phase`, `/autocoding`.
- Runs on [`tokio`](https://tokio.rs/); pure Rust, no C toolchain required.

## How it works

**Agent loop.** Each turn sends the conversation plus the tool specs to the provider, streams thinking/answer to the screen, executes any tool calls the model requests, feeds the results back, and repeats until the model produces a final answer.

**Context compaction.** When the request grows past ~60% of the context window, the older part of the conversation is summarized and the last 6 messages are kept verbatim, so the agent still sees the exact file paths, errors and output it is working on. Compaction runs at the start of every turn, after each tool round, and at end of turn. Bad summaries (empty, thinking-only, truncated) never replace the history — the previous history is kept and the reason is printed.

**Two tool modes.**

| Mode | Behaviour |
|---|---|
| `native` (default) | Sends the provider's tool spec and reads structured tool calls from the response; results return as `role: "tool"` messages. |
| `prompt` | Sends no native tool spec; the tool list + JSON schemas go into the system prompt and the model answers with a fenced ` ```tool ``` ` JSON block. Use it when a backend's function calling is unreliable. |

**Guard rails.** Tool results are capped before entering history (default 64,000 chars, head 70% + tail 30%), tool calls per turn are limited (default 50), a repeated-output detector aborts loops, and the coding workflow stops after 5 stalled rounds on the same task or 15 rounds with no progress at all.

**Output.** Every section is printed with a timestamp, context usage and per-request timing, and mirrored to a plain-text transcript log (`output.txt` by default).

## Requirements

| Item | Required? | Notes |
|---|---|---|
| Rust toolchain | To build | `cargo`; developed and tested on rustc 1.99 (1.85+ recommended) |
| LLM server | To run | [Ollama](https://ollama.com) or any OpenAI-compatible server |
| poppler-utils | Only `read_pdf` | `pdftotext` + `pdftoppm` must be on `PATH` |
| bash | Only `run_command` | commands run through `bash -c` (on Windows use WSL/Git Bash) |
| searxng instance | Optional | `web_search` is disabled automatically when no URL is set |
| C compiler | Not needed | TLS comes from rustls (pure Rust) |

```bash
sudo apt install poppler-utils     # Debian/Ubuntu
sudo dnf install poppler-utils     # Fedora
brew install poppler               # macOS
```

## Installation

```bash
cd jonnyq-rust
cargo build --release
./target/release/jonnyq -h
```

Run without building, or install into `PATH`:

```bash
cargo run --release -- -model qwen3:14b
cargo install --path .                     # → ~/.cargo/bin/jonnyq
cp target/release/jonnyq ~/.local/bin/     # or copy the binary yourself
```

`[profile.release]` sets `strip = true`, so the release binary is about **7 MB**.

Cross-compiling works out of the box (no C bindings) as long as you have a linker for the target, or use [`cross`](https://github.com/cross-rs/cross):

```bash
rustup target add x86_64-unknown-linux-musl aarch64-apple-darwin x86_64-pc-windows-gnu
cargo build --release --target aarch64-apple-darwin
cross build --release --target aarch64-unknown-linux-gnu
```

Output lands in `target/<triple>/release/jonnyq`.

## Quick start

```bash
# 1) start the model server
ollama pull qwen3:14b && ollama serve

# 2) run jonnyq from your project folder
cd ~/dev/my-project
jonnyq -model qwen3:14b -context 32768
```

Or keep the settings in a file:

```bash
cp .env.example .env     # template with every key documented
$EDITOR .env             # JONNYQ_MODEL, JONNYQ_CONTEXT_SIZE, ...
jonnyq                   # same as: jonnyq -model ... -context ...
```

OpenAI-compatible backends:

```bash
jonnyq -provider openai:http://localhost:1234/v1 -model qwen3-14b          # LM Studio
jonnyq -provider openai:https://api.openai.com/v1 -key sk-... -model gpt-4o-mini
```

The provider string is always `name:url`, where `name` is `ollama` or `openai`.

Until a model is set, jonnyq accepts slash commands only:

```
> /list_model          # show models available on the provider
> /model qwen3:14b
> /context 32768
> explain this project's structure
> /exit
```

**Multi-line input** — end a line with `\` to continue, or type `"""` on its own line to start/end a fenced block. `/prompt <file>` sends a file's content as the prompt.

**Editing keys** — the prompt is a full line editor: arrow keys recall previous commands (saved in `$HOME/.jonnyq_history`, shared across sessions), `Tab` completes slash commands, command arguments and file names, `Ctrl-A/E/W/U` work as in readline, `Ctrl-Z` suspends.

**Ctrl-C cancels the current turn** (agent turn, running command, or a pending `ask_user` question) without quitting — in the browser console (`-web`) the **Stop** button is the same cancellation. `Ctrl-D` on an empty line, `/exit` or `/bye` quits.

**Example workflow:**

```bash
$EDITOR requirements.md
jonnyq -model qwen3:14b -ntfy-url https://ntfy.sh/my-secret-topic
# > /plan_for_human     # build .progress with a per-phase human checklist
# > /coding_phase       # do one phase, stop, send the verification steps
# > /coding_phase       # next phase
# > /autocoding         # or do everything in one run
```

## Configuration

Precedence: **CLI flag > `.env` file > environment variable (`JONNYQ_*`) > default**.

The `.env` file in the current directory is loaded automatically at start (and on `/env reload`). It wins over exported environment variables, because it holds the intended settings of that project; env vars are the fallback for keys the file does not define. An env var name is the flag name uppercased with the `JONNYQ_` prefix (`-context` → `JONNYQ_CONTEXT_SIZE`).

The file supports comments, `export KEY=value`, single/double quotes (`\n`, `\t` escapes), `$VAR` / `${VAR:-fallback}` expansion, `$$` for a literal `$`, and last-key-wins for duplicates. A malformed line is a warning; an unterminated quote is a startup error.

| Flag | Env var / `.env` key | Default | Meaning |
|---|---|---|---|
| `-provider` | `JONNYQ_PROVIDER` | `ollama:http://localhost:11434` | provider as `name:url` (`ollama` or `openai`) |
| `-key` | `JONNYQ_KEY` | (empty) | API key, sent as `Authorization: Bearer` |
| `-model` | `JONNYQ_MODEL` | (empty — **must be set**) | model name |
| `-context` | `JONNYQ_CONTEXT_SIZE` | `16348` | context window (passed to Ollama as `num_ctx`) |
| `-output` | `JONNYQ_OUTPUT_FILE` | `output.txt` | transcript log (append, plain text) |
| `-searxng-url` | `JONNYQ_SEARXNG_URL` | (empty — disables `web_search`) | searxng instance URL |
| `-searxng-max-results` | `JONNYQ_SEARXNG_MAX_RESULTS` | `10` | max search results |
| `-searxng-concurrency` | `JONNYQ_SEARXNG_CONCURRENCY` | `4` | parallel searches |
| `-searxng-timeout-sec` | `JONNYQ_SEARXNG_TIMEOUT_SEC` | `30` | search timeout (seconds) |
| `-fetch-concurrency` | `JONNYQ_FETCH_CONCURRENCY` | `5` | parallel `web_fetch` requests |
| `-fetch-timeout-sec` | `JONNYQ_FETCH_TIMEOUT_SEC` | `30` | fetch timeout (seconds) |
| `-pdf-max-pages` | `JONNYQ_PDF_MAX_PAGES` | `50` | max PDF pages read by `read_pdf` |
| `-pdf-dpi` | `JONNYQ_PDF_DPI` | `150` | DPI when rendering PDF pages to PNG |
| `-run-command-timeout-sec` | `JONNYQ_RUN_COMMAND_TIMEOUT_SEC` | `300` | `run_command` timeout (seconds) |
| `-history-size` | `JONNYQ_HISTORY_SIZE` | `200` | prompts kept for ↑/↓ recall |
| `-compact-headroom-percent` | `JONNYQ_COMPACT_HEADROOM_PERCENT` | `60` | % of the window that triggers compaction |
| `-compact-target-percent` | `JONNYQ_COMPACT_TARGET_PERCENT` | `25` | % of the window the summary may use (hard cap 1,000 tokens) |
| `-compact-keep-recent` | `JONNYQ_COMPACT_KEEP_RECENT` | `6` | trailing messages kept verbatim after compaction |
| `-summary-transcript-cap` | `JONNYQ_SUMMARY_TRANSCRIPT_CAP` | `48000` | max transcript sent to the summarization request |
| `-max-tool-result-chars` | `JONNYQ_MAX_TOOL_RESULT_CHARS` | `64000` | max size of one tool result before it enters history |
| `-max-tool-calls-per-turn` | `JONNYQ_MAX_TOOL_CALLS_PER_TURN` | `50` | tool-call budget per turn |
| `-watch-magic-word` | `JONNYQ_WATCH_MAGIC_WORD` | `AI!` | trigger word for `/watchfile` |
| `-watch-poll-interval-sec` | `JONNYQ_WATCH_POLL_INTERVAL_SEC` | `1` | `/watchfile` poll interval (seconds) |
| `-ntfy-url` | `JONNYQ_NTFY_URL` | (empty — no notifications) | full ntfy topic URL; used for notifications **and** `ask_user` replies |
| `-socks-proxy` | `JONNYQ_SOCKS_PROXY` | (empty — falls back to `ALL_PROXY`/`HTTPS_PROXY`) | SOCKS proxy for every request (model, web tools, ntfy): `socks5h://host:port`, `socks5`, `socks4a`, `socks4` or `host:port`; `off` = direct; `auto` = trust the environment |
| `-no-proxy` | `JONNYQ_NO_PROXY` | `localhost,127.0.0.1,::1` | comma-separated hosts/CIDRs that bypass the proxy |
| `-tool-mode` | `JONNYQ_TOOL_MODE` | `native` | `native` or `prompt` |
| `-skill-path` | `JONNYQ_SKILL_PATH` | (empty) | skill directories, separated by `;` |
| `-web` | `JONNYQ_WEB_PORT` | `0` (no web interface) | port for the browser console on `http://127.0.0.1:<port>` |
| `-thinking=true\|false` | `JONNYQ_THINKING` | `true` | enable/disable model thinking (sent to Ollama only) |
| `-env-file` | `JONNYQ_ENV_FILE` | (empty = `./.env`) | pick another dotenv file, or `off`/`none`/`no` to disable; settable from flag/env only, never inside the file |

Notes:

- Every flag takes a value except `-thinking` (inline `-thinking=false` only) and `-h`/`-help`. Parsing stops at the first non-flag argument or at `--`; a repeated flag keeps the last value. `-h` prints usage and exits with code 1.
- `-thinking` is only forwarded to Ollama (`{"think": true|false}`). With the `openai` provider jonnyq just renders `reasoning_content` when the endpoint sends it — turn thinking off server-side.
- Credentials in a proxy URL are masked as `user:***` in all output and logs.
- `-skill-path` is separated by `;` only (not `:`), and needs `\;` or single quotes in a shell.

## Slash commands

`/help` (or `/?`) lists these in the program.

| Command | Description |
|---|---|
| `/?`, `/help` | Show all commands |
| `/provider <name>:<url>` | Switch provider (effective immediately) |
| `/key <key>` | Set the API key |
| `/model <model>` | Set the model |
| `/list_model` | List models available on the provider |
| `/context <size>` | Set the context size |
| `/think <true\|false>` | Toggle thinking |
| `/compact` | Summarize the history now, without waiting for the threshold |
| `/history` | Per-message and total context usage |
| `/run_command_timeout <seconds>` | Change the command timeout at runtime |
| `/prompt <file>` | Send a file's content as the prompt |
| `/plan` | Create `.progress` from `requirements.md`, or reconcile it if the requirements changed — writes no code |
| `/plan_for_human` | Like `/plan`, but also writes `> How to run:` / `> Human check:` notes so a person can verify each phase by using the app |
| `/coding` | Do the next unfinished task in `.progress`, then stop |
| `/coding_phase [<n>]` | Do all tasks of one phase (fresh history per task), then stop; `/coding_phase 3` picks a phase explicitly |
| `/autocoding` | Run `/plan` if needed, then every remaining task in one go |
| `/watchfile [<word>\|off]` | Watch the working tree for a magic word and turn that line into a prompt |
| `/ntfy [<topic url>\|off]` | Show/set/disable the ntfy topic |
| `/proxy [<socks url>\|off\|auto]` | Show/set the proxy for all traffic, from the next request |
| `/proxy no-proxy <hosts>` | Change the proxy bypass list only |
| `/toolmode [native\|prompt]` | Show/switch the tool mode |
| `/env [path\|off]` | Show which `.env` file is in use, or switch/disable it |
| `/env list` | Every setting with its effective value and source (`flag`/`file`/`env`/`default`) |
| `/env reload [path\|off]` | Re-read the dotenv file and apply it to the running session |
| `/exit`, `/bye` | Quit |

`/env reload` applies model, context, thinking, tool mode, output file, provider/key, proxy, searxng/fetch/pdf timeouts, tool-call budget, skill path, ntfy topic and watch settings without a restart. Flags given on the command line are replayed, so they keep winning.

## Tools

| Tool | Parameters | Notes |
|---|---|---|
| `read_file` | `path` | Text files inside the working directory, capped at 2 MB |
| `write_file` | `path`, `content` | Create/overwrite, parent directories created |
| `edit_file` | `path`, `old_string`, `new_string`, `replace_all?` | Exact-match replacement; errors when not found or ambiguous |
| `create_folder` | `path` | Create a directory with parents |
| `run_command` | `command` | `bash -c` in the working dir; stdout+stderr merged; timeout kills the whole process group |
| `web_search` | `query` | searxng JSON API — disabled unless a URL is configured |
| `web_fetch` | `url` | Load a URL and strip HTML to text, capped at 3 MB |
| `read_pdf` | `path` | `pdftotext`; scanned PDFs are rendered to PNG for `read_pic` |
| `read_pic` | `path` | Image → base64 data URI, capped at 10 MB |
| `read_skill` | `name` | Read the full body of a skill listed in the system prompt |
| `ask_user` | `prompt` | Ask the human a question and wait — on the terminal (blue question, bright-white answer), in the browser popup (`-web`), and, if ntfy is set, through the notification answered with `answer: <text>`. First answer wins; the other channels are closed |

All path tools are confined to the working directory (`../` traversal and symlink escapes are rejected).

## Automated coding: `/plan` → `.progress` → `/coding`

Write `requirements.md` yourself, then:

| | `/plan` | `/coding` | `/coding_phase` | `/autocoding` |
|---|---|---|---|---|
| Creates `.progress` | Yes | No | No | Yes (calls `/plan`) |
| Work done | none | 1 task | 1 phase | all tasks |
| Clears history per task | – | No | Yes | Yes |
| Stops when | plan written | 1 task done | phase done | everything done |

`.progress` is a GitHub-checkbox checklist grouped into phases, kept in sync with `requirements.md` by a SHA-256 hash in `.progress.hash` — changed requirements reconcile the existing plan instead of regenerating it:

```markdown
# Project Title

> How to run: `cargo run` → http://localhost:3000, login with demo/demo123

## Phase 1: Scaffolding & config

> Human check: run `cargo build` -> no errors, binary in target/debug

- [ ] Init Cargo.toml with tokio + serde dependencies (REQ-001)
- [x] Implement the tool registry
```

While coding, the system prompt switches to an autonomous mode that forbids marking a task done without actually running the build/tests in that round. Task completion is verified against the real file content, so false "done" claims stall the loop and surface an error.

`/watchfile` is pair programming without leaving the editor: start it with `/watchfile`, then drop a magic word into the code —

```rust
// AI! add error handling for the missing-file case, plus a test
let data = std::fs::read(path).unwrap();
```

the watcher (mtime polling, no fsnotify) picks the line up, sends it as a prompt, and the agent removes the trigger line when it is finished.

## Skills

A skill is a markdown note the model can pull in on demand. Skills are rediscovered every turn and summarized (name + first line) into the system prompt; the model calls `read_skill` when it needs the full text.

```
~/my-skills/
├── react/
│   └── SKILL.md        ← skill "react"
└── postgres.md         ← skill "postgres" (single file)
```

```bash
jonnyq -skill-path '~/my-skills;~/shared-skills'
# or JONNYQ_SKILL_PATH=$HOME/my-skills;$HOME/shared-skills   (in .env)
```

## Web console

```bash
jonnyq -model qwen3:14b -web 8080     # then open http://127.0.0.1:8080
```

`-web <port>` serves the same session in the browser, next to the terminal — both
are live at the same time, and neither can make the other run two turns at once.
The page has one websocket and no scripts or bundler of its own, so it works
against a local model on a machine with no internet; the only thing it reaches
out for is the Google Fonts stylesheet behind the font picker, and a page that
cannot fetch it simply draws the text in the same face installed locally, or in
the browser's own sans-serif.

| Part | What it does |
|---|---|
| **top bar** | `model · ctx … · mode` for the session, updated as the turn runs, plus the **font** and **size** pickers described below |
| **console tab** (centre) | Every byte jonnyq prints to stdout, mirrored live and rendered in gruvbox dark colours — the same text, in the same colours, as the terminal |
| **terminal tabs** | A real shell in the working directory, in the browser — the **Terminal** button beside Send opens one; see below |
| **file tabs** | One per opened file; click a tab to switch between the console and any number of files |
| **prompt box** (below the console) | Type a prompt or a `/command`. Enter sends, Shift-Enter adds a newline. Drag the bar above it, or its own corner, to resize. It is greyed out whenever the session cannot take a prompt — no model yet, a turn in flight, or the session exiting — with the reason next to it |
| **Stop button** (next to Send) | The browser's Ctrl-C: it cancels the turn in flight and is enabled only while there is one. It presses the same cancellation as Ctrl-C at the terminal, so the model's stream is dropped, a running command's process group is killed and a pending `ask_user` question is withdrawn — and the session survives to take the next prompt, with `[cancelled]` in the transcript. The popup carries its own copy, since a question covers the prompt box |
| **ask popup** | When the model calls `ask_user`, the question pops up over the page with its own answer field and a **Send answer** button — see below |
| **tree** (right) | The working directory, folders first, **kept up to date by itself** — a file created, deleted or renamed appears in the panel without any clicking, and the folders you had expanded stay expanded. Expand a folder to list it; click a text file to open it in a new tab. A PDF, image or binary pops up why it cannot be shown |

**Font and size.** The top bar's `font` picker offers nine Thai faces from Google
Fonts — IBM Plex Sans Thai Looped (the default), Playpen Sans Thai, K2D,
Kodchasan, Noto Serif Thai, Noto Sans Thai Looped, Athiti, Thasadith and Trirong
— and `size` sets the text size in pt (8–40, default **14pt**). The face applies to
the console, the prompt box and the bar itself; a file view keeps a monospaced
face so source keeps its columns, and takes the size. Both settings are remembered
per browser, so a reload or a second tab opens the way you left it, and `reset`
goes back to 14pt IBM Plex Sans Thai Looped. Picking a face is never a network
operation: it is one CSS family name, which resolves to the downloaded face, to a
locally installed copy of it, or to the generic fallback.

**The status line is live.** The `ctx` figure in the bar is the session's current
context use, not a snapshot of connect time: it is re-read on a two-second
ticker, at the start and end of every turn, and whenever a slash command changes
the model or the window, and a frame is only sent when the line actually changed.

**The tree stays current by itself.** There is no refresh button: the panel
re-lists the working directory, plus every folder you have expanded, on a
two-and-a-half-second tick, and again the moment the tab becomes visible after
being hidden. Only the folders *on screen* are re-listed — a folded-up folder has
nothing to keep current, and is listed when you open it. A listing that is
unchanged draws nothing, so the panel is not rebuilt every few seconds: your
scroll position and your expanded folders survive a poll, and a redraw happens
only when a folder gained, lost or renamed an entry (size alone is not a visible
change, since no row shows one). A folder that disappears from disk folds itself
up instead of leaving dead rows behind. A folder that cannot be read says so
inside itself — *cannot list: …* where its rows would be — rather than in a
popup, because the panel keeps trying and a box per tick is not information; the
working directory failing outright is the one case that does pop up, and it says
it once per distinct reason rather than once per tick. A hidden tab is not polled,
and neither is an offline one, so closing the laptop lid costs nothing and a
reconnect picks the listing back up.

**Terminal tabs.** The **Terminal** button under the prompt box opens a shell in
the browser, in the session's working directory — a real PTY with a real
interactive shell behind it, so `git`, `cargo`, `less`, `vim` and tab completion
all behave as they do at the terminal, colours and cursor movement included. It
is the same shell you would have had in that folder: it reads your own profile
and starts in `$SHELL` (falling back to `/bin/sh`), with `TERM=xterm-256color`
and its own window size.

* **Many at once.** Every tab is its own shell, so `cargo build` in one can run
  while you read a log in another. `Alt-1`…`Alt-9` switch tabs, and each tab has
  **New** (another shell) and **Kill**. `Ctrl-C` in a terminal tab interrupts the
  *shell's* command, not the agent's turn — the console's **Stop** button is the
  one that cancels a turn.
* **Where it starts.** In the working directory, and in a subfolder of it on
  request: `term_open` takes a path, and a path that would climb out of the
  working directory is refused rather than opened. A shell is never started
  somewhere the session itself cannot see.
* **Resizing.** The pane measures itself, so the shell is told how big its window
  is and a full-screen program redraws rather than wrapping wrong. Shrinking the
  browser window resizes the shell.
* **When it ends.** Typing `exit`, or a **Kill**, closes that one shell; the tab
  says the shell is gone and Enter starts another in the same tab, keeping the
  scrollback. Closing the browser tab, reloading the page or ending the session
  closes the shells with it — no orphaned process is left holding the folder.
* **What runs there is yours.** There is no sandbox: a shell in the browser is a
  shell on the machine, with the same rights as the one that started jonnyq. The
  page is served on `127.0.0.1` and the terminal is off unless `-web` is given,
  so the console port is the trust boundary — do not expose it to a network you
  do not control.

A prompt typed in the browser goes onto a queue that the REPL's own loop reads,
which is why the answer appears in *both* windows, and why a browser prompt, a
typed prompt and a `/watchfile` trigger are serialised against each other. The
textarea freezes while a turn runs and thaws when it finishes. Opening or
reloading a tab replays the transcript so far **in the same colours** — the replay
comes from what the terminal was sent, not from the ANSI-stripped log file — so a
second browser window shows the conversation that already happened, coloured the
way the terminal showed it.

`jonnyq -web 8080 < /dev/null` (or a session whose stdin was closed with Ctrl-D)
keeps running as a browser-only session instead of exiting on EOF — the prompt
echoes into the console, and `/exit` in a tab ends the session.

**Stopping a turn from the page.** **Stop**, under the prompt box beside Send, is
the browser's Ctrl-C: it sends one `stop` message and presses the very
cancellation token the terminal's SIGINT presses, so there is no second, weaker
kind of cancel — the model's stream is dropped, a `run_command` child has its
whole process group killed, a pending `ask_user` question is withdrawn, and the
turn ends the same way with `[cancelled]` in the transcript and the session
alive for the next prompt. The button is enabled exactly while the session
reports a turn in flight (not merely while the textarea is greyed out, which also
covers *no model yet* and *the link dropped*), and that flag travels on the
`ready`, `info` and `stop` frames as well as on `/health` as `busy`, so a tab that
reloads or reconnects gets the button right immediately. Pressing it twice is
fine — a turn that has not noticed the first one can be stopped at again — and a
press that arrives after the turn had already finished says *Nothing to stop*
rather than pretending. A turn that ignores the cancellation is not the page's
problem to explain: the console shows what the turn is still doing.

**Answering `ask_user` from the page.** A question the model asks is shown as a
popup: the question, an answer field, and **Send answer** (Enter sends,
Shift-Enter adds a newline). The terminal prints the same question, so you can
answer in either place — the browser popup, the keyboard or an ntfy reply are
three channels raced against each other and the first answer wins. Whoever wins,
the others are told: the popup closes the moment the answer arrives anywhere
(and says *answered elsewhere*, so a box never vanishes without a reason), and an
answer to a question that has already been answered is refused rather than
queued. A reload or a reconnect gets any still-unanswered question back from the
greeting, so refreshing the page in the middle of a long run does not lose it.
Several questions can be open at once (a cancelled turn leaves its question
waiting): only the front one covers the screen, the rest are counted, and the
popup hands itself over as each is answered. Nothing on the page hides or
dismisses the box — it holds the question jonnyq is blocked on, and the "hide"
button (and Escape) that used to tuck it away only ever took the answer field
away with it — so the server's `ask_close` is the only thing that clears it. Its
own **Stop** is not a way of ducking a question: it cancels the turn that is
asking, and the question goes away because nobody is asking it any more — without
it, a session blocked on a question could not be stopped from the browser at all,
since the popup covers the prompt box. In a headless session the popup is the
only channel, so it is also what makes `-web` usable without a terminal at all.

The listener binds to `127.0.0.1` and answers only requests whose `Host` (and
`Origin`, when a browser sends one) names loopback, so it is a local interface
and nothing else. `GET /health` reports the session as JSON, including `asks` —
how many `ask_user` questions are unanswered, i.e. that jonnyq is waiting for a
human rather than working — and `busy`, whether a turn is in flight and so
whether there is anything for a `stop` message to cancel. It is *not* a
boundary against something already running as you: any browser tab can read the
working directory. Changing the model, provider, proxy or ntfy topic stays under
the keyboard — the page's controls cover typeface, text size and stopping a turn,
nothing else.

## Notifications (ntfy)

Set a full topic URL with `-ntfy-url` / `JONNYQ_NTFY_URL` / `/ntfy <url>` (empty disables everything). jonnyq pushes a message when `/plan`, `/coding`, `/coding_phase` or `/autocoding` finishes, and a high-priority `warning` message when a turn fails or stalls — the failure text includes a `Reason:` line explaining the likely cause. Summaries are computed from `.progress` directly, not by calling the model.

The same topic is also an inbound channel: `ask_user` pushes the question and subscribes for the reply, so you can answer from your phone (`answer: <text>`, or plain quick reply). Whoever answers first wins — keyboard, browser popup (`-web`) or phone; the losing channels are told, so the popup closes and the terminal prompt is not left hanging.

## Files created at runtime

| File | When | Notes |
|---|---|---|
| `output.txt` | Always (`-output`) | Plain-text transcript log, append mode; already git-ignored |
| `.env` | If you create it | Project settings; git-ignored, start from `.env.example` |
| `$HOME/.jonnyq_history` | Every submitted prompt | Shared across sessions; move it by changing `HOME` |
| `.progress`, `.progress.hash`, `.progress.retry` | Coding commands | `.progress` is meant to be committed; the other two are local state and ignored |
| `.jonnyq/pdf_pages/**.png` | `read_pdf` on a scanned PDF | Cache, safe to delete |

## Security notes

- **Path confinement** — file tools resolve each component and its symlinks, so nothing outside the working directory can be read or written.
- **`run_command` runs with your full permissions and asks nothing** — a best-effort denylist blocks ~20 destructive patterns (`rm -rf /`, fork bombs, `mkfs`, writes to `/dev/sd*`, `curl … | sh`, …) but it is not a sandbox.
- **ntfy has no authentication** — anyone who knows the topic can answer `ask_user`. Use a long random topic name, and never put secrets in prompts.
- **Resource caps** — 2 MB per file read, 3 MB per fetch, 10 MB per image, bounded PDF pages and tool calls per turn.
- **API keys** are only sent in the `Authorization` header and are never echoed to the screen or the log.

## License

Not declared yet (`Cargo.toml` has no `license` field) — contact the maintainer before redistributing.
