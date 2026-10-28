# jonnyq

**Interactive coding agent CLI.** A Rust REPL that talks to a local or remote LLM, lets the model call tools (read/write files, run commands, search the web) in a loop until the task is done, and can drive an entire project from a `requirements.md` file.

Language: **English** · [ไทย](README.Thai.md)

```
~/dev/my-project$ jonnyq -model qwen3:14b
jonnyq v1.1.1
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
3. [Version](#version)
4. [Requirements](#requirements)
5. [Installation](#installation)
6. [Quick start](#quick-start)
7. [Configuration](#configuration)
8. [Slash commands](#slash-commands)
9. [Tools](#tools)
10. [Sharing what the agents know](#sharing-what-the-agents-know)
11. [Automated coding: `/plan` → `.context/progress.md` → `/coding`](#automated-coding-plan--contextprogressmd--coding)
12. [Skills](#skills)
13. [Web console](#web-console)
14. [Notifications (ntfy)](#notifications-ntfy)
15. [Files created at runtime](#files-created-at-runtime)
16. [Security notes](#security-notes)
17. [License](#license)

---

## What it is

- An interactive terminal agent: you type a request, the model thinks, calls tools, and answers — repeating until the job is finished.
- **Four agents on one model** — JONNY (anything), JACKY (planning), JOEY (coding) and JOY (testing/review) — switching with `/jonny`, `/jacky`, `/joey`, `/joy`. Each keeps its own conversation in its own history file, so asking JOY to review does not spend JOEY's context.
- Works with **Ollama** (native API) and any **OpenAI-compatible** endpoint (OpenAI, LM Studio, llama.cpp server, vLLM, OpenRouter, …).
- Prompt-driven project automation: `/plan`, `/plan_for_human`, `/coding`, `/coding_phase`, `/autocoding`.
- Says which build it is: the version from `Cargo.toml` is printed above the first prompt, heads `-h` and `/help`, is repeated by `/version`, and is on the web console's title bar and in its `/health` JSON.
- Runs on [`tokio`](https://tokio.rs/); pure Rust, no C toolchain required.

## How it works

**Agent loop.** Each turn sends the conversation plus the tool specs to the provider, streams thinking/answer to the screen, executes any tool calls the model requests, feeds the results back, and repeats until the model produces a final answer.

**Context compaction.** Compaction is per agent, and it accounts for the whole request — system prompt, tool specs and the messages — not just the history. When a request grows past 50% of the context window, the older part of that agent's conversation is summarized and the last 10 messages are kept verbatim, so the agent still sees the exact file paths, errors and output it is working on. The summary itself is capped well below the window it is meant to free. Compaction runs at the start of every turn, after each tool round, and at end of turn. Bad summaries (empty, thinking-only, truncated) never replace the history — the previous history is kept and the reason is printed.

**The team.** One model, one terminal, four agents. JONNY answers plain prompts, JACKY writes the plan, JOEY works the checklist and JOY checks the result; each is activated by name (`/joey`) or implicitly by the command that belongs to it (`/coding` is JOEY's). They share the tools, the model and the transcript, but each keeps its own history in `.context/<name>.md`, and only one of them is "active" — the one a plain prompt goes to. Switching agents is free: nothing is re-sent, and no context is spent on the others. Every agent knows what the other three are for; see [Sharing what the agents know](#sharing-what-the-agents-know). Each one also has a character of its own — JONNY cheerful, JACKY meticulous, JOEY hard on his own work, JOY beautiful and very strict — which is written into the top of its system message. It changes how an agent approaches its work, never what it is allowed to touch: the lane rules are stated first and stay binding.

**Joy's verdict.** `/plan` and the coding commands end with a review, and the verdict is her plain-text answer: `APPROVE`, or `NEEDS FIX` followed by numbered findings that go straight back to the agent that has to act on them (up to `/review_rounds` times). Her brief depends on the author: a plan is audited for coverage of the requirements, an implementation for correctness and for staying inside its task's scope (see [Automated coding](#automated-coding-plan--contextprogressmd--coding)). A review that states no verdict is never read as a pass — jonnyq says so and leaves the work unverified. Two rules keep that honest with reasoning models: the verdict is read from **that turn's answer** (never from an older turn in her history, and never from her thinking channel), and an answer that opens with prose but states no verdict on its own line earns one short follow-up asking for the verdict alone — her review is still in her context, so nothing is re-run.

> JACKY used to be called JACKSON. Nothing else changed — same agent, same lane, and a `.context/jackson.md` left by an older session is renamed to `.context/jacky.md` on the first start, so its memory comes along. `/jackson` still works as a synonym for `/jacky`; it is simply not listed any more, since one name per agent is the point.

**Two tool modes.**

| Mode | Behaviour |
|---|---|
| `native` (default) | Sends the provider's tool spec and reads structured tool calls from the response; results return as `role: "tool"` messages. |
| `prompt` | Sends no native tool spec; the tool list + JSON schemas go into the system prompt and the model answers with a fenced ` ```tool ``` ` JSON block. Use it when a backend's function calling is unreliable. |

**Guard rails.** Tool results are capped before entering history (default 64,000 chars, head 70% + tail 30%), tool calls per turn are limited (default 50), a repeated-output detector aborts loops, and the coding workflow stops after 5 stalled rounds on the same task or 15 rounds with no progress at all.

**Output.** Every section is printed with a timestamp, context usage and per-request timing, and mirrored to a plain-text transcript log (`.context/transcript.md` by default). Each line is labelled with the agent that produced it (`[joey] Thinking …`). The REPL prints its version (`jonnyq v1.1.1`) above the first prompt, so a transcript and a screenshot carry the build they came from.

## Version

jonnyq is at **v1.1.1**. The version lives in exactly one place — the `version` field of `Cargo.toml` — and everything that reports it reads it from there at compile time, so the number cannot drift from the binary you built. Where it shows up:

| Where | What you see |
|---|---|
| Terminal, at startup | `jonnyq v1.1.1` above the first prompt, and in the transcript log |
| `jonnyq -h` / `-help` | The version as the first line of the usage output (also printed after a bad flag) |
| `/version` | The same line, any time during a session |
| `/help`, `/?` | The version above the slash command list |
| Web console | In the browser tab's title (`jonnyq console v1.1.1`) and in the top bar beside the **jonnyq** brand; the terminal emulator in a terminal tab answers an XTVERSION query with it too |
| `GET /health` | `"version": "1.1.1"` in the JSON, so a script can identify a running session |

`-h` still exits with code 1, like a Go `flag` program: it is a usage request, not a command. There is no `-version` flag — use `-h`, `/version`, or `curl 127.0.0.1:<port>/health` for a `-web` session.

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
./target/release/jonnyq -h     # version on the first line, then every flag; exits 1, like Go's flag
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
# > /plan_for_human     # build .context/progress.md with a per-phase human checklist
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
| `-context-dir` | `JONNYQ_CONTEXT_DIR` | `.context` | folder holding the agents' histories, the checklist and the transcript log |
| `-output` | `JONNYQ_OUTPUT_FILE` | `<context-dir>/transcript.md` | transcript log (append, plain text) |
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
| `-compact-headroom-percent` | `JONNYQ_COMPACT_HEADROOM_PERCENT` | `50` | % of the window that triggers compaction |
| `-compact-target-percent` | `JONNYQ_COMPACT_TARGET_PERCENT` | `20` | % of the window the summary may use (hard cap 1,000 tokens) |
| `-compact-keep-recent` | `JONNYQ_COMPACT_KEEP_RECENT` | `10` | trailing messages kept verbatim after compaction |
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
| `-verify=true\|false` | `JONNYQ_VERIFY` | `true` | have JOY review what `/plan` and the coding commands produced, and hand her findings back for a fix round |
| `-review-rounds <n>` | `JONNYQ_REVIEW_ROUNDS` | `1` | fix/re-review cycles a command may run before the findings are just reported |
| `-thinking=true\|false` | `JONNYQ_THINKING` | `true` | enable/disable model thinking (sent to Ollama only) |
| `-env-file` | `JONNYQ_ENV_FILE` | (empty = `./.env`) | pick another dotenv file, or `off`/`none`/`no` to disable; settable from flag/env only, never inside the file |

Notes:

- Every flag takes a value except `-thinking` (inline `-thinking=false` only) and `-h`/`-help`. Parsing stops at the first non-flag argument or at `--`; a repeated flag keeps the last value. `-h` prints the version and the usage and exits with code 1.
- `-thinking` is only forwarded to Ollama (`{"think": true|false}`). With the `openai` provider jonnyq just renders `reasoning_content` when the endpoint sends it — turn thinking off server-side.
- Credentials in a proxy URL are masked as `user:***` in all output and logs.
- `-skill-path` is separated by `;` only (not `:`), and needs `\;` or single quotes in a shell.

## Slash commands

`/help` (or `/?`) lists these in the program.

| Command | Description |
|---|---|
| `/?`, `/help` | Show all commands, with the version on top |
| `/provider <name>:<url>` | Switch provider (effective immediately) |
| `/key <key>` | Set the API key |
| `/model <model>` | Set the model |
| `/list_model` | List models available on the provider |
| `/context <size>` | Set the context size |
| `/think <true\|false>` | Toggle thinking |
| `/compact` | Summarize the active agent's history now, without waiting for the threshold |
| `/history` | Per-message and total context usage, per agent |
| `/run_command_timeout <seconds>` | Change the command timeout at runtime |
| `/prompt <file>` | Send a file's content as the prompt |
| `/plan` | JACKY: create `.context/progress.md` from `requirements.md`, or reconcile it if the requirements changed — writes no code |
| `/plan_for_human` | JACKY: like `/plan`, but also writes `> How to run:` / `> Human check:` notes so a person can verify each phase by using the app |
| `/coding` | JOEY: do the next unfinished task in `.context/progress.md`, then stop |
| `/coding_phase [<n>]` | JOEY: do all tasks of one phase (fresh history per task), then stop; `/coding_phase 3` picks a phase explicitly |
| `/autocoding` | JOEY: run `/plan` if needed, then every remaining task in one go |
| `/watchfile [<word>\|off]` | Watch the working tree for a magic word and turn that line into a prompt |
| `/ntfy [<topic url>\|off]` | Show/set/disable the ntfy topic |
| `/proxy [<socks url>\|off\|auto]` | Show/set the proxy for all traffic, from the next request |
| `/proxy no-proxy <hosts>` | Change the proxy bypass list only |
| `/toolmode [native\|prompt]` | Show/switch the tool mode |
| `/env [path\|off]` | Show which `.env` file is in use, or switch/disable it |
| `/env list` | Every setting with its effective value and source (`flag`/`file`/`env`/`default`) |
| `/env reload [path\|off]` | Re-read the dotenv file and apply it to the running session |
| `/agent <jonny\|jacky\|joey\|joy>` | Switch the active agent — the one a plain prompt goes to |
| `/jonny` | JONNY — the main agent: general work, and anything not below |
| `/jacky` | JACKY — the planner: `/plan` and `/plan_for_human` |
| `/joey` | JOEY — the coder: `/coding`, `/coding_phase`, `/autocoding` |
| `/joy` | JOY — testing and verification: reviews what JACKY and JOEY produced, and may not edit files herself |
| `/team` | The roster: who is on it, who is active, and what each one remembers (message count, context use, history file) |
| `/verify [on\|off]` | Whether JOY reviews what `/plan` and the coding commands produced, handing her findings back for a fix round (default on) |
| `/review_rounds [n]` | Fix/re-review cycles one command may run before the findings are just reported (default 1) |
| `/finish [notes]` | The active agent writes a hand-off summary — what is done, what changed, how to verify, what is left — into the transcript, then every history is saved |
| `/version` | Print the version (`jonnyq v…`) — the same figure `-h`, the startup line and the web console show |
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

## Sharing what the agents know

Four conversations are only worth having if they do not each have to rediscover the same project. So every agent's system prompt carries the roster — what JONNY, JACKY, JOEY and JOY are each for, which commands each owns, and where each one's context file lives — and every agent may **read** any of those files:

```
> read_file .context/joey.md      # what the coder actually found and changed
> run_command grep -n 'REQ-012' .context/   # search the whole team's memory
```

That is the cheap way across: the planner can use what the coder learned without paying for it in its own context, and JOY can check a claim against what the author's history actually says instead of guessing. The system prompt tells them to do this rather than re-derive or assume.

Reading is one-way, and enforced rather than requested. A history file is that agent's memory exactly as jonnyq persists it, and it is read back at the next start — an edited or deleted one comes back as a false memory, or as none. So `write_file`, `edit_file` and `create_folder` refuse any agent's history file (including the writer's own), and `run_command` refuses the ordinary ways of overwriting one from the shell — `> .context/joey.md`, `rm .context/jacky.md`, `sed -i … .context/`, `tee`, `truncate`, `cp … .context/joey.md`, `git checkout .context/…`. Refusals say why and point at `read_file`, so the agent retries by reading instead of by guessing. `cat`, `grep`, `head`, `wc`, `diff`, `ls`, and copying a history file *out* to somewhere else (`cp .context/joey.md /tmp/`) are reads and keep working — a guard that made `grep` awkward would just train the agents to stop reading, which is the whole point of the shared files. `progress.md` and its sidecars in the same folder are task-list files, not memory, and stay writable by the coding commands.

## Automated coding: `/plan` → `.context/progress.md` → `/coding`

Write `requirements.md` yourself, then:

| | `/plan` | `/coding` | `/coding_phase` | `/autocoding` |
|---|---|---|---|---|
| Creates `.context/progress.md` | Yes | No | No | Yes (calls `/plan`) |
| Work done | none | 1 task | 1 phase | all tasks |
| Clears history per task | – | No | Yes | Yes |
| Stops when | plan written | 1 task done | phase done | everything done |

`.context/progress.md` is a GitHub-checkbox checklist grouped into phases, kept in sync with `requirements.md` by a SHA-256 hash in `.context/progress.hash` — changed requirements reconcile the existing plan instead of regenerating it:

```markdown
# Project Title

> How to run: `cargo run` → http://localhost:3000, login with demo/demo123

## Phase 1: Scaffolding & config

> Human check: run `cargo build` -> no errors, binary in target/debug

- [ ] Init Cargo.toml with tokio + serde dependencies (REQ-001)
- [x] Implement the tool registry
```

While coding, the system prompt switches to an autonomous mode that forbids marking a task done without actually running the checks for that task in that round. Task completion is verified against the real file content, so false "done" claims stall the loop and surface an error.

Both coding commands carry an explicit scope rule, and it is worth knowing why: an early session was told *"create the project root file `index.html`"* and wrote the entire calculator into it, then spent the rest of the turn inventing a DOM shim to test logic no task had asked for yet. Two things in the old prompt caused that — it named no boundary, and it demanded "the real build and test commands **for this project**", which a task with no behaviour yet can only satisfy by first writing behaviour. So a task line is now the whole of JOEY's assignment: implement exactly what it says, leave later tasks alone, and verify only what this task delivers — for a file/skeleton task, "the file exists with exactly the required content" *is* the verification. Going beyond the task is treated as a defect even when it works, because the reviewer can no longer tell task from surplus and later tasks lose their verification. JOY checks for exactly that, and a finding of "this is task 6's work" is fixed by removing the surplus, not by adding more.

Joy's review briefs differ by author, for the same reason: a plan has no build to re-run. Reviewing **JACKY's plan** is a coverage audit — she walks `requirements.md` requirement by requirement and reports any requirement, edge case or constraint no task delivers, anything a coder would have to guess, any task too broad to verify in one sitting, any task that could be ticked without proving anything. A plan that can be followed to the letter and still yields an incomplete app is a failed plan, and that is what she is there to catch before `/coding` spends an hour on it. Reviewing **JOEY's change** is a correctness-and-scope check: re-run the build and tests, then confirm the change delivers exactly the task line it was ticked against.

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
out for is the Google Fonts stylesheet behind the two font pickers, and a page
that cannot fetch it simply draws the text in the same face installed locally, or
in the generic tail of the stack — a monospace for code, so columns and cells stay
lined up either way.

| Part | What it does |
|---|---|
| **top bar** | `agent · model · ctx … · mode` for the session — the agent a prompt goes to now, so a `/jonny`-`/joey` switch, a `/model`, a `/context` or a compaction all move the bar, which is re-read on a ticker rather than frozen at connect — the build's version beside the **jonnyq** brand, plus the **text** and **code** font and size controls described below |
| **console tab** (centre) | Every byte jonnyq prints to stdout, mirrored live and rendered in gruvbox dark colours — the same text, in the same colours, as the terminal |
| **terminal tabs** | A real shell in the working directory, in the browser — the **Terminal** button beside Send opens one; see below |
| **file tabs** | One per opened file, with its source syntax-highlighted in the same gruvbox dark palette; click a tab to switch between the console and any number of files. The head of each has a **Download** that saves that file to your computer |
| **prompt box** (below the console) | Type a prompt or a `/command`. Enter sends, Shift-Enter adds a newline. Drag the bar above it, or its own corner, to resize. It is greyed out whenever the session cannot take a prompt — no model yet, a turn in flight, or the session exiting — with the reason next to it |
| **Stop button** (next to Send) | The browser's Ctrl-C: it cancels the turn in flight and is enabled only while there is one. It presses the same cancellation as Ctrl-C at the terminal, so the model's stream is dropped, a running command's process group is killed and a pending `ask_user` question is withdrawn — and the session survives to take the next prompt, with `[cancelled]` in the transcript. The popup carries its own copy, since a question covers the prompt box |
| **ask popup** | When the model calls `ask_user`, the question pops up over the page with its own answer field and a **Send answer** button — see below |
| **tree** (right) | The working directory, folders first, **kept up to date by itself** — a file created, deleted or renamed appears in the panel without any clicking, and the folders you had expanded stay expanded. Expand a folder to list it; click a text file to open it in a new tab. A PDF, image or binary pops up why it cannot be shown — and offers it as a download |

**Font and size.** The top bar has two pairs of controls, because the page draws
two kinds of text. `text` picks the face of the console, the prompt box and the bar
itself from nine Thai faces on Google Fonts (IBM Plex Sans Thai Looped is the
default) and its number field sets that text's size in pt (8–40, default **14pt**).
`code` picks the face of the terminal panes and the file views from eighteen
monospaced faces — Source Code Pro, Roboto Mono, JetBrains Mono, IBM Plex Mono (the
default), Space Mono, Fira Code, Google Sans Code, Ubuntu Mono, Ubuntu Sans Mono,
Anonymous Pro, Nova Mono, Syne Mono, Iosevka Charon Mono, Sono, Intel One Mono, SUSE
Mono, Cascadia Mono and Libertinus Mono — and its field sets their size (8–40,
default **12pt**). The two lists are separate on purpose: a terminal pane and a file
view are grids of cells, and a proportional face cannot tile one. All four settings
are remembered per browser, so a reload or a second tab opens the way you left it,
and `reset` goes back to 14pt IBM Plex Sans Thai Looped and 12pt IBM Plex Mono.
Picking a face is never a network operation: it is one CSS family name, which
resolves to the downloaded face, to a locally installed copy of it, or to the generic
tail of the stack — a monospace for code, so a machine that never fetched the
stylesheet still gets lined-up columns and cells.

**The status line is live.** The bar reads `agent · model · ctx … · mode`, and all
four figures are the session's current state rather than a snapshot of connect
time: the agent name follows a `/jonny`-`/joy` switch (or a `/plan` handing over
to JACKY), and the context figure belongs to whichever agent is active — a
prompt to JOEY spends JOEY's window, not the team's. The line is re-read on a
two-second ticker, at the start and end of every turn, and whenever a slash
command changes the model, the window or the agent, and a frame is only sent when
the line actually changed.

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

**File tabs are syntax-highlighted.** Open a file from the tree and its source
is drawn in the same gruvbox dark palette as the console: comments dimmed and
italic, reserved words red, types yellow, strings green, keys and literals
picked out. The language comes from the file's extension, from its name where an
extension would not say (`Dockerfile`, `Makefile`, `Cargo.lock`, `.env`), and
failing both from its `#!` line — so an extensionless script is still read as
the interpreter it asks for. Rust, TOML, Python, JavaScript, TypeScript, JSON,
YAML, Markdown, shell, Dockerfile, HTML, CSS, C/C++, Go, Java, Ruby, PHP, Lua,
Zig, Perl, SQL and INI-style config are covered; anything else is shown plain
rather than guessed at, because colouring a file by the wrong rules is how you
come to distrust the right ones.

It is a scanner, not a parser, and it is written into the page rather than
fetched from a CDN — the same reason the rest of the page has no dependencies:
the colouring has to be there on a machine with no internet. Three properties it
never gives up: the text the pane shows is the file's text, token for token and
in order, so selecting and copying out of a file tab still gives you the file;
everything is escaped before it is wrapped, so a file containing a `<script>`
tag is only ever text; and a string or comment ends at its close marker, its
line, or the end of the file — one apostrophe in a sentence of prose does not
turn everything under it pink. Inside a Markdown file a fenced code block is
shown as a block set off from the prose rather than re-read as the language its
fence names — the fence is part of the document, not a window into another file.
Files over 400 KB are shown plain: past that a
file is being searched rather than read, and building a span per token would
freeze the tab for a second to no purpose. The header of every file tab says
which language it was read as, so an unhighlighted file never leaves you
wondering whether the feature is broken.

**A file can be saved.** The head of every file tab has a **Download** button,
and so does the popup that says a file cannot be viewed — a PDF you cannot read
in the pane is usually one you still want. Both hand the browser a link to
`GET /file?path=…`, which answers with the file's own bytes offered as an
attachment, so the browser's own save box does the rest: no copy-and-paste, no
reformatting, and the console tab stays where it was instead of navigating to
the file. A download is deliberately *not* the rules the file view goes through
— no text check, because the PDF, image and archive the view refuses are most of
what you would want to keep, and no 2 MB cap, because a truncated archive is a
corrupt one; the bytes are streamed, so a large file costs a chunk of memory
rather than its own size. What is held to is the same rule the tree is: the name
must stay inside the working directory, and only a page served to loopback can
ask at all.

**Terminal tabs.** The **Terminal** button under the prompt box opens a shell in
the browser, in the session's working directory — a real PTY with a real
interactive shell behind it, so `git`, `cargo`, `less`, `vim` and tab completion
all behave as they do at the terminal, colours and cursor movement included. It
is the same shell you would have had in that folder: it reads your own profile
and starts in `$SHELL` (falling back to `/bin/sh`), with `TERM=xterm-256color`
and its own window size.

* **Scrolling back.** Every terminal pane keeps a scrollback of the last 2000
  rows, so a `cargo build` that flew past is still readable: scroll with the
  wheel, drag the pane's scroll bar, or press `Shift-PgUp`/`Shift-PgDn` to page
  through it a screen at a time — the plain keys stay the shell's, because `less`
  and `man` are the reason anyone presses them. Any key you send brings the pane
  back to the prompt. What you have scrolled up to read is not moved by the shell
  printing more — new output arrives *below* it — so a long-running command never
  yanks the view away, and only a pane that was already at the bottom follows the
  output. Redrawing after a resize re-wraps the scrollback to the new width, so a
  line that needed two rows in a narrow pane reads as one line in a wide one.
  `Ctrl-C` in a terminal tab stays the shell's interrupt; it is never a way to
  scroll.
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
way the terminal showed it. That holds across a restart too: the coloured copy of
the transcript is written beside the log (`.context/.transcript.ansi`), so quitting
jonnyq and starting it again greets the page with the earlier session in the
colours it was shown in, not in grey.

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
and nothing else. `GET /health` reports the session as JSON, including `version`
— the build behind the port, the same figure the page's title bar shows — `asks`,
how many `ask_user` questions are unanswered, i.e. that jonnyq is waiting for a
human rather than working — and `busy`, whether a turn is in flight and so
whether there is anything for a `stop` message to cancel. `GET /file?path=…`,
which is what the **Download** buttons hit, keeps the same rule plus the tree's:
the name must resolve inside the working directory, and a browser that says it
is being sent from another site (`Sec-Fetch-Site: cross-site`) is refused, so a
stranger's page cannot drop a file into your Downloads folder. It is *not* a
boundary against something already running as you: any browser tab can read the
working directory. Changing the model, provider, proxy or ntfy topic stays under
the keyboard — the page's controls cover typeface, text size, saving a file the
tree already listed and stopping a turn, nothing else.

## Notifications (ntfy)

Set a full topic URL with `-ntfy-url` / `JONNYQ_NTFY_URL` / `/ntfy <url>` (empty disables everything). jonnyq pushes a message when `/plan`, `/coding`, `/coding_phase` or `/autocoding` finishes, and a high-priority `warning` message when a turn fails or stalls — the failure text includes a `Reason:` line explaining the likely cause. Summaries are computed from `.context/progress.md` directly, not by calling the model.

The same topic is also an inbound channel: `ask_user` pushes the question and subscribes for the reply, so you can answer from your phone (`answer: <text>`, or plain quick reply). Whoever answers first wins — keyboard, browser popup (`-web`) or phone; the losing channels are told, so the popup closes and the terminal prompt is not left hanging.

## Files created at runtime

Everything jonnyq writes about a session (as opposed to your project) lives in one
folder, `.context/` (`-context-dir` / `JONNYQ_CONTEXT_DIR`):

| File | When | Notes |
|---|---|---|
| `.context/jonny.md`, `jacky.md`, `joey.md`, `joy.md` | Every turn | Each agent's conversation, reloaded at startup so a session resumes where it stopped |
| `.context/transcript.md` | Always (`-output`) | Plain-text transcript log, append mode; the whole folder is git-ignored |
| `.context/.transcript.ansi` | Always (besides the log) | The same transcript *with* its terminal colour codes, bounded to ~1 MB — what lets a web console opened after a restart replay the earlier session in colour instead of grey. Follows `-output` (same folder, same stem, `.ansi`), recreated if deleted, git-ignored |
| `.context/progress.md` | `/plan` | The task checklist — the one file in the folder meant to be committed |
| `.context/progress.hash`, `.context/progress.retry` | Coding commands | Local state: the requirements hash and the stall counter |
| `.env` | If you create it | Project settings; git-ignored, start from `.env.example` |
| `$HOME/.jonnyq_history` | Every submitted prompt | Shared across sessions; move it by changing `HOME` |
| `.jonnyq/pdf_pages/**.png` | `read_pdf` on a scanned PDF | Cache, safe to delete |

## Security notes

- **Path confinement** — file tools resolve each component and its symlinks, so nothing outside the working directory can be read or written.
- **`run_command` runs with your full permissions and asks nothing** — a best-effort denylist blocks ~20 destructive patterns (`rm -rf /`, fork bombs, `mkfs`, writes to `/dev/sd*`, `curl … | sh`, …) but it is not a sandbox.
- **Agent memory is shared read-only** — any agent can read any agent's history file; the write tools and the ordinary shell write patterns refuse to change one. Best-effort like the denylist (a shell command can still be written cleverly enough to reach the file) — what it stops is an agent's default instincts, which is where the damage would come from. See [Sharing what the agents know](#sharing-what-the-agents-know).
- **ntfy has no authentication** — anyone who knows the topic can answer `ask_user`. Use a long random topic name, and never put secrets in prompts.
- **Resource caps** — 2 MB per file read, 3 MB per fetch, 10 MB per image, bounded PDF pages and tool calls per turn.
- **API keys** are only sent in the `Authorization` header and are never echoed to the screen or the log.

## License

[MIT License](LICENSE) — Copyright (c) 2026 Authapon Kongwan.

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and associated documentation files (the "Software"), to deal in the Software without restriction, including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the following conditions: the above copyright notice and this permission notice shall be included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
