# jonnyq

**Interactive coding agent CLI** — REPL เขียนด้วย Rust (port จากเวอร์ชัน Go) รันบน [`tokio`](https://tokio.rs/) รองรับ provider แบบ **Ollama** (native API) และ **OpenAI-compatible** (SSE) พร้อม tool-calling loop, การ compact conversation history อัตโนมัติ, และชุดคำสั่ง `/plan` `/plan_for_human` `/coding` `/coding_phase` `/autocoding` สำหรับ automate การเขียนโค้ดจาก `requirements.md`

```
~/dev/my-project$ jonnyq -model qwen3:14b
qwen3:14b (32768 token) > ช่วยแก้บั๊กตอนอ่านไฟล์ไม่เจอใน main.rs
Thinking (2026-10-24 10:23:41) | ctx ~1,910/32,768 (~5%) left ~30,858 | +0.4s | last req 1,120 in/380 out, 22.4 tok/s, 17.0s
...
Tool call (2026-10-24 10:23:45) | ctx ~2,140/32,768 (~6%) left ~30,628 | +4.1s | req 2,010 in/96 out, 24.1 tok/s, 4.0s
read_file({"path": "src/main.rs"})
Answer (2026-10-24 10:23:52) | ctx ~3,020/32,768 (~9%) left ~29,748 | +11.2s | last req 2,480 in/410 out, 23.7 tok/s, 7.3s
...
[preload=n/a prompt_eval=1.2s generate=41.6s token_in=8210 token_out=1042 total_token=9252 tok/s=25.05 wall=52.8s | ctx 3,020/32,768 (9%) left 29,748]
```

---

## สารบัญ

1. [ความสามารถ](#1-ความสามารถ)
2. [ความต้องการของระบบ](#2-ความต้องการของระบบ)
3. [การติดตั้ง / Build](#3-การ-install--build)
4. [การเริ่มต้นใช้งาน (Quick start)](#4-การเริ่มต้นใช้งาน-quick-start)
5. [การตั้งค่า (CLI flag / ไฟล์ `.env` / environment variable)](#5-การตั้งค่า-cli-flag--ไฟล์-env--environment-variable)
6. [Slash commands](#6-slash-commands)
7. [การพิมพ์ prompt หลายบรรทัด / Ctrl-C](#7-การพิมพ์-prompt-หลายบรรทัด--ctrl-c)
8. [Tools ที่ model เรียกใช้ได้](#8-tools-ที่-model-เรียกใช้ได้)
9. [โหมดเรียกใช้ tool: `native` vs `prompt`](#9-โหมดเรียกใช้-tool-native-vs-prompt)
10. [การทำงานอัตโนมัติ: `/plan` → `.progress` → `/coding`](#10-การทำงานอัตโนมัติ-plan--progress--coding)
11. [`/watchfile` — pair programming ผ่าน magic word](#11-watchfile--pair-programming-ผ่าน-magic-word)
12. [การแจ้งเตือนผ่าน ntfy](#12-การแจ้งเตือนผ่าน-ntfy)
13. [Skills](#13-skills)
14. [การแสดงผลบน terminal และ context usage](#14-การแสดงผลบน-terminal-และ-context-usage)
15. [สถาปัตยกรรมและโครงสร้างโค้ด](#15-สถาปัตยกรรมและโครงสร้างโค้ด)
16. [การทดสอบ](#16-การทดสอบ)
17. [ความปลอดภัย (โดยตั้งใจ)](#17-ความปลอดภัย-โดยตั้งใจ)
18. [ไฟล์ที่โปรแกรมสร้างระหว่างรัน](#18-ไฟล์ที่โปรแกรมสร้างระหว่างรัน)
19. [Troubleshooting](#19-troubleshooting)
20. [ความแตกต่างจากเวอร์ชัน Go](#20-ความแตกต่างจากเวอร์ชัน-go)

---

## 1. ความสามารถ

### ตัว REPL และ provider

- **ตั้งค่าได้ 3 ทาง** — CLI flag, ไฟล์ **`.env`** และ environment variable `JONNYQ_*` (โหลด `.env` อัตโนมัติ ไม่ต้อง source/export) ตามลำดับ precedence `flag > .env > env > default`; พิมพ์ `/env list` เพื่อดูว่าแต่ละค่ามาจากชั้นไหน และ `/env reload` เพื่อแก้ไฟล์แล้วใช้ค่าใหม่ได้ทันทีโดยไม่ต้อง restart
- **REPL loop** รับ prompt ต่อเนื่องจนกว่าจะสั่ง `/exit` / `/bye` หรือกด Ctrl-D — กด **Ctrl-C เพื่อยกเลิกรอบที่ทำงานอยู่ได้โดยไม่ปิดโปรแกรม**
- รองรับ 2 provider:
  - `ollama` — native `/api/chat` (NDJSON streaming) ได้ **metric เวลาจริงจาก server** (load / prompt_eval / generate) พร้อมส่ง `think: true|false` และ `options.num_ctx` ตามการตั้งค่า
  - `openai` — ทุก endpoint ที่เป็น OpenAI-compatible (`/chat/completions` แบบ SSE) เช่น OpenAI, LM Studio, llama.cpp server, vLLM, OpenRouter — อ่าน `reasoning_content` เป็น thinking และ **จับเวลาเองฝั่ง client** (ค่าขึ้นต้นด้วย `~`) เพราะ endpoint กลุ่มนี้รายงานแค่จำนวน token
- prompt ที่รับคำสั่ง (`>`) แสดงชื่อ model และ context size กำกับ เช่น `ornith-1.5-35b-a3b (100000 token) >`
- `/list_model` ดึงรายชื่อ model จาก provider (`/api/tags` ของ Ollama, `/models` ของ OpenAI-compatible)

### agent loop

- **tool-calling loop** ส่ง conversation + tools spec ไป provider → stream thinking/answer ออกจอ → รัน tool ที่ model เรียก → ส่งผลลัพธ์กลับ → วนจนได้คำตอบสุดท้าย
- **context compaction อัตโนมัติ** — เมื่อ history (ประเมินแบบหยาบ ~4 ตัวอักษร/token) เกิน **~60%** ของ context size จะสรุป history ทั้งหมดเหลือข้อความเดียว (บังคับให้ summary ไม่เกิน **~25%** ของ budget เดิม) ตรวจทั้ง **หลังทุกรอบ tool call** (ไม่ใช่รอจบ turn) และหลังจบ turn
- **ตัดการคิดวนซ้ำ** (`RepeatDetector`) — ถ้าบรรทัดเดิม (>20 ตัวอักษร) เกิดซ้ำครบ 3 ครั้งในหน้าต่าง 8 บรรทัดล่าสุด จะยกเลิก request นั้นทันที บันทึกข้อความสั้น ๆ แทนขยะที่วนซ้ำ แล้วปล่อยให้กลไก retry/stall จัดการต่อ (turn จบปกติ ไม่ error)
- **budget tool call ต่อ turn** (`-max-tool-calls-per-turn`, default 50) เป็น safety valve กัน model วนเรียก tool ไม่รู้จบ
- system prompt ทุกโหมดกำชับให้ model **คิดแบบเด็ดขาดและกระชับ** (ตัดสินใจแล้วลงมือทำ ไม่วนคิดเรื่องเดิม แต่ต้องยังตรวจสอบสิ่งที่ไม่แน่ใจด้วย tool ก่อนสรุป)

### automation

- แยก planning ออกจากการเขียนโค้ด: `/plan` (สร้าง/reconcile `.progress` อย่างเดียว) · `/coding` (ทำ 1 task แล้วหยุด) · `/coding_phase` (ทำจบ 1 phase แล้วหยุด) · `/autocoding` (plan แล้วไล่ทำทุก task รวดเดียว)
- **Incremental planning** — เก็บ SHA-256 ของ `requirements.md` ใน `.progress.hash` ถ้า requirement เปลี่ยน จะ **reconcile** `.progress` ต่อยอด (เพิ่ม task ใหม่ / uncheck task ที่ไม่ตรงกับ requirement หรือโค้ดจริงแล้ว) โดยไม่เริ่ม plan ใหม่ทั้งหมด
- **`/plan_for_human`** + คู่มือตรวจสอบสำหรับมนุษย์ (`> How to run:` / `> Human check:` ใน `.progress`)
- **AUTONOMOUS CODING MODE** — ระหว่าง `/coding`/`/coding_phase`/`/autocoding` system prompt จะเข้มงวดขึ้น: ห้ามนับ task ว่าเสร็จถ้าไม่ได้รัน build/test จริงในรอบนั้นแล้วเห็นผลผ่านจริง
- **ตัวกันวน 2 ชั้น**: task เดิมค้างซ้ำครบ 5 รอบ (`MAX_STALL_ROUNDS`) และ 15 รอบติดที่ไม่มี task ไหนถูกติ๊กเลย (`MAX_NO_PROGRESS_ROUNDS`) → หยุดพร้อม error พร้อมข้อความบอกสาเหตุที่ต่างกันตามสิ่งที่ตรวจพบจริงในไฟล์
- **`/watchfile`** — poll working directory หา magic word (default `AI!`) แล้วส่งบรรทัดนั้นเป็น prompt ให้ agent
- **แจ้งเตือนผ่าน [ntfy.sh](https://ntfy.sh)** (หรือ self-hosted) เมื่อ `/plan`/`/coding`/`/coding_phase`/`/autocoding` จบ ทั้งสำเร็จและ error (error มาพร้อม `Reason:` อธิบายสาเหตุที่น่าจะเป็น) — และ**คุยกับผู้ใช้กลับได้**: tool `ask_user` (ติดตั้งเสมอ) ตั้งคำถามขึ้นมากลางรอบแล้ว**รอคำตอบ** — คำถามสีฟ้า คำตอบสีขาวสว่าง, push คำถามออก ntfy แล้วตอบกลับจากมือถือก็ได้ ([หัวข้อ 8.1](#81-ask_user--ถามผู้ใช้แล้วรอคำตอบ))
- **Skills** — โฟลเดอร์/ไฟล์ markdown ที่ถูกสรุปชื่อ+คำอธิบายเข้าสู่ system prompt ทุก turn ให้ model `read_skill` เมื่อต้องใช้

---

## 2. ความต้องการของระบบ

| รายการ | จำเป็น? | หมายเหตุ |
|---|---|---|
| **Rust toolchain** (แนะนำ 1.85 ขึ้นไป) | จำเป็นตอน build | ต้องมี `cargo` — feature ที่ crate นี้ใช้ตรง ๆ คือ `std::sync::LazyLock` (stable ตั้งแต่ 1.80) + edition 2021; `Cargo.lock` ไม่ได้ประกาศ `requires-rust` ของ dependencies ไว้ จึงแนะนำเวอร์ชันใหม่ไว้ก่อน — พัฒนาและทดสอบจริงบน **rustc 1.99** |
| **LLM server** | จำเป็นตอนใช้งาน | [Ollama](https://ollama.com) หรือ server แบบ OpenAI-compatible (LM Studio, llama.cpp server, vLLM ฯลฯ) |
| **poppler-utils** | เฉพาะ `read_pdf` | `pdftotext` + `pdftoppm` ต้องอยู่บน `PATH` (ไม่ได้ผูก PDF library เข้า binary) |
| **bash** | เฉพาะ `run_command` | `run_command` เรียก `bash -c` — บน Windows ต้องมี bash (WSL/Git Bash) บน PATH |
| **searxng instance** | เฉพาะ `web_search` (ไม่บังคับ) | ถ้าไม่ตั้ง URL จะปิด `web_search` โดยอัตโนมัติ |
| **C compiler (gcc/clang)** | ไม่จำเป็น | ตัว binary ใช้ rustls (pure Rust) ไม่ใช้ cgo — `ldd` ยืนยันว่า linking แค่ libc/libm/libgcc_s |

```bash
# poppler-utils
sudo apt install poppler-utils     # Debian/Ubuntu
sudo dnf install poppler-utils     # Fedora
brew install poppler               # macOS
```

---

## 3. การ Install / Build

โปรเจกต์เป็น pure Rust (ไม่มี cgo) build ได้ตรงไปตรงมา

### Build สำหรับเครื่องปัจจุบัน

```bash
cd jonnyq-rust
cargo build --release
./target/release/jonnyq -h
```

### รันผ่าน cargo โดยไม่ต้อง build เอง

```bash
cargo run --release -- -model qwen3:14b
# flag หลายตัวคั่นตามปกติ
cargo run --release -- -provider openai:http://localhost:1234/v1 -model qwen3-14b -context 32768
```

### ติดตั้งเข้า `PATH`

```bash
cargo install --path .          # → ~/.cargo/bin/jonnyq
# หรือคัดลอก binary เอง
cp target/release/jonnyq ~/.local/bin/
```

### Build แบบ debug (compile เร็ว แต่ runtime ช้ากว่า/binary ใหญ่กว่า)

```bash
cargo build            # → target/debug/jonnyq
```

> `[profile.release]` ตั้ง `strip = true` ไว้แล้ว — release binary จึงถูกตัด debug symbol เหลือประมาณ **7 MB** (debug ~82 MB)

### Cross-compile ไปแพลตฟอร์มอื่น

ไม่มี cgo จึงติดตั้ง target แล้ว build ได้เลย (ต้องมี linker ของแพลตฟอร์มปลายทาง — ใช้ [`cross`](https://github.com/cross-rs/cross) ถ้าไม่อยากยุ่งกับ toolchain ของ target)

```bash
# ติดตั้ง target เพิ่ม (กรณีใช้ rustup)
rustup target add x86_64-unknown-linux-musl aarch64-apple-darwin x86_64-pc-windows-gnu

cargo build --release --target x86_64-unknown-linux-musl   # Linux static (musl)
cargo build --release --target aarch64-apple-darwin        # macOS Apple Silicon
cargo build --release --target x86_64-pc-windows-gnu       # Windows (หมายเหตุ: run_command ต้องมี bash)

# หรือให้ cross (docker-based) จัดการ linker ให้
cross build --release --target aarch64-unknown-linux-gnu
```

ผลลัพธ์อยู่ที่ `target/<triple>/release/jonnyq`

> ทุกคำสั่งใน README นี้ build/run/test จริงบน **Linux x86_64 (Arch Linux, rustc 1.99)** — เป้าหมาย cross-compile อื่นยังไม่ได้ทดสอบจริงใน repo นี้

---

## 4. การเริ่มต้นใช้งาน (Quick start)

```bash
# 1) เปิด Ollama (หรือ server OpenAI-compatible ตัวอื่น) แล้ว load model
ollama pull qwen3:14b
ollama serve

# 2) เข้าโฟลเดอร์ project แล้วรัน jonnyq
cd ~/dev/my-project
jonnyq -model qwen3:14b -context 32768

# หรือเก็บการตั้งค่าไว้ในไฟล์ .env แล้วรันสั้น ๆ (jonnyq อ่านไฟล์ให้เอง)
cp <path ไปยัง repo>/jonnyq-rust/.env.example .env   # template อย่างเป็นทางการ
$EDITOR .env                         # ใส่ JONNYQ_MODEL / JONNYQ_CONTEXT_SIZE ...
jonnyq                               # เทียบเท่า: jonnyq -model ... -context ...
```

> ค่าทั้งสามทางทับกันได้ตามลำดับ `CLI flag > .env > env var > default` — ดูรายละเอียด (syntax ของไฟล์ `.env`, การเลือกไฟล์/ปิดการใช้งาน, `/env reload`) ใน [หัวข้อ 5](#5-การตั้งค่า-cli-flag--ไฟล์-env--environment-variable)

ถ้ายังไม่ตั้ง model โปรแกรมจะเตือน `model is not set; use /model <name> ...` และรับเฉพาะ slash commands — ตั้ง model ก่อนด้วย `/model` แล้วพิมพ์คำสั่งได้เลย

```
> /list_model                                  # เช็ครายชื่อ model ที่มี
> /model qwen3:14b                             # ตั้ง model
> /context 32768                               # ตั้ง context size
> ช่วยอธิบายโครงสร้างโปรเจกต์นี้
> /exit
```

ใช้ผู้ให้บริการแบบ OpenAI-compatible (OpenAI / LM Studio / llama.cpp / OpenRouter):

```bash
# LM Studio (context ฝั่ง server ต้องตั้งให้ตรงกันเอง)
jonnyq -provider openai:http://localhost:1234/v1 -model qwen3-14b

# OpenAI API
jonnyq -provider openai:https://api.openai.com/v1 -key sk-... -model gpt-4o-mini
```

> **หมายเหตุ:** รูปแบบ provider string คือ `name:url` — `name` เป็น `ollama` หรือ `openai` เท่านั้น (เป็นอย่างอื่นจะ error ตั้งแต่ตอนเริ่ม)

**ตัวอย่าง workflow จริง (plan → ทำเองทีละ task → ตรวจเอง):**

```bash
cd ~/dev/my-project
# สร้าง requirements.md ด้วยตัวเองก่อนเริ่ม
$EDITOR requirements.md

jonnyq -model qwen3:14b -ntfy-url https://ntfy.sh/my-secret-topic
# > /plan_for_human      # สร้าง .progress แบบแบ่ง phase พร้อมคู่มือตรวจสำหรับคน
# > /coding_phase        # ทำจบ 1 phase แล้วหยุด + ส่งข้อความบอกวิธีตรวจ
# (เปิดแอปตรวจเองตามคู่มือ)
# > /coding_phase        # ทำ phase ถัดไป
# > /autocoding          # หรือให้ทำทุกอย่างรวดเดียวให้จบ
```

---

## 5. การตั้งค่า (CLI flag / ไฟล์ `.env` / environment variable)

ลำดับความสำคัญ: **CLI flag > ไฟล์ `.env` > env var (`JONNYQ_*`) > ค่า default**

```
CLI flag            -model qwen3:14b        ← ชนะทุกชั้น
      ↓
.env file           JONNYQ_MODEL=qwen3:14b  ← อ่านอัตโนมัติจากโฟลเดอร์ที่รัน
      ↓                                        ชนะ env var ที่ export ทับไว้
env var             JONNYQ_MODEL=qwen3:14b  ← เป็นค่าสำรองเมื่อไฟล์ไม่มี key นี้
      ↓
built-in default    context = 16348
```

jonnyq **โหลดไฟล์ `.env` ให้เอง** ตั้งแต่ startup — ไม่ต้อง `source`/`export` อีกต่อไป ถ้าในโฟลเดอร์ที่รันไม่มี `.env` ก็ทำงานเหมือนเดิมทุกประการ (env var + flag) และปิดการโหลดไฟล์ได้เสมอด้วย `-env-file off`

เหตุผลที่ไฟล์ `.env` ชนะ environment variable: ค่าใน `.env` คือการตั้งค่าของ project นั้น ๆ ที่ตั้งใจไว้ชัดเจน ส่วน env var มักเป็นค่าที่ export ทิ้งไว้ใน shell (หรือจาก project อื่น) จึงเหมาะเป็น "ค่าสำรอง" มากกว่า — ผลที่ตามมาคือ `export JONNYQ_MODEL` ทับไว้จะไม่บังค่า `JONNYQ_MODEL` ในไฟล์ (ถ้าอยาก override จริงให้ใส่ flag `-model` หรือ comment key ในไฟล์ทิ้ง) และ env var ที่ export ไว้จะมีผลกับ key ที่ไฟล์ไม่ได้กำหนดเท่านั้น; พิมพ์ `/env list` เมื่อไรก็ได้เพื่อดูว่าแต่ละค่ามาจากชั้นไหน

ชื่อ env var = ชื่อ flag แปลงตัวพิมพ์ใหญ่ แล้วเติม prefix `JONNYQ_` (เช่น `-context` → `JONNYQ_CONTEXT_SIZE`, `-searxng-url` → `JONNYQ_SEARXNG_URL`) — ใช้ได้ทั้งเป็น environment variable และ key ในไฟล์ `.env`

| Flag | Env var / key ใน `.env` | Default | ความหมาย |
|---|---|---|---|
| `-provider` | `JONNYQ_PROVIDER` | `ollama:http://localhost:11434` | provider รูปแบบ `name:url` (`ollama` หรือ `openai`) |
| `-key` | `JONNYQ_KEY` | (ว่าง) | provider API key (ส่งเป็น `Authorization: Bearer`) |
| `-model` | `JONNYQ_MODEL` | (ว่าง — **ต้องตั้ง**ก่อนถึงจะสั่งงานได้) | ชื่อ model |
| `-context` | `JONNYQ_CONTEXT_SIZE` | `16348` | ขนาด context (Ollama ถูกส่งต่อเป็น `num_ctx`) |
| `-output` | `JONNYQ_OUTPUT_FILE` | `output.txt` | ไฟล์ transcript log (โหมด append, ข้อความล้วน ไม่มี ANSI) |
| `-searxng-url` | `JONNYQ_SEARXNG_URL` | (ว่าง — **ปิด** `web_search`) | URL ของ searxng instance |
| `-searxng-max-results` | `JONNYQ_SEARXNG_MAX_RESULTS` | `10` | จำนวนผลลัพธ์สูงสุด |
| `-searxng-concurrency` | `JONNYQ_SEARXNG_CONCURRENCY` | `4` | จำนวนการค้นหาพร้อมกันสูงสุด |
| `-searxng-timeout-sec` | `JONNYQ_SEARXNG_TIMEOUT_SEC` | `30` | timeout (วินาที) |
| `-fetch-concurrency` | `JONNYQ_FETCH_CONCURRENCY` | `5` | จำนวนการโหลดเว็บ (`web_fetch`) พร้อมกันสูงสุด |
| `-fetch-timeout-sec` | `JONNYQ_FETCH_TIMEOUT_SEC` | `30` | timeout (วินาที) |
| `-pdf-max-pages` | `JONNYQ_PDF_MAX_PAGES` | `50` | จำนวนหน้า PDF สูงสุดที่ `read_pdf` อ่าน |
| `-pdf-dpi` | `JONNYQ_PDF_DPI` | `150` | DPI ตอน render หน้า PDF เป็น PNG (กรณี PDF ไม่มี text) |
| `-run-command-timeout-sec` | `JONNYQ_RUN_COMMAND_TIMEOUT_SEC` | `300` | timeout ของ `run_command` (วินาที) — ปรับระหว่างรันได้ด้วย `/run_command_timeout` |
| `-max-tool-calls-per-turn` | `JONNYQ_MAX_TOOL_CALLS_PER_TURN` | `50` | จำนวน tool call สูงสุดใน 1 turn (safety valve กัน tool-call loop) |
| `-watch-magic-word` | `JONNYQ_WATCH_MAGIC_WORD` | `AI!` | magic word ที่ `/watchfile` ใช้ค้นหา |
| `-watch-poll-interval-sec` | `JONNYQ_WATCH_POLL_INTERVAL_SEC` | `1` | ความถี่ poll ของ `/watchfile` (วินาที, ขั้นต่ำ 1) |
| `-ntfy-url` | `JONNYQ_NTFY_URL` | (ว่าง — ปิดการแจ้งเตือน) | ntfy topic URL เต็ม ๆ เช่น `https://ntfy.sh/my-topic` — ใช้ทั้งส่ง notification และ**รับคำตอบของ `ask_user`** จากมือถือ |
| `-tool-mode` | `JONNYQ_TOOL_MODE` | `native` | `native` หรือ `prompt` (case-insensitive; ค่าอื่น → error ตอนเริ่ม) |
| `-skill-path` | `JONNYQ_SKILL_PATH` | (ว่าง) | ไดเรกทอรีของ skill **คั่นด้วย `;`** ได้หลายอัน |
| `-thinking=true\|false` | `JONNYQ_THINKING` | `true` | เปิด/ปิด model thinking (มีผลกับ Ollama — ดูหมายเหตุด้านล่าง) |
| `-env-file` | `JONNYQ_ENV_FILE` | (ว่าง = ใช้ `.env` ในโฟลเดอร์ที่รัน) | เลือกไฟล์ dotenv, `off`/`none`/`no` = ปิดการโหลด — **อ่านจาก env var/flag เท่านั้น ตั้งในไฟล์ไม่ได้** |

### 5.1 ไฟล์ `.env` (dotenv)

#### ไฟล์ที่ถูกอ่าน

| ต้องการ | ทำอย่างไร |
|---|---|
| ใช้ไฟล์ `.env` ในโฟลเดอร์ที่รัน (ค่า default) | ไม่ต้องทำอะไร — มีไฟล์ก็อ่าน ไม่มีก็ข้าม |
| ระบุไฟล์เอง | `jonnyq -env-file prod.env` หรือ `JONNYQ_ENV_FILE=prod.env jonnyq` (ขึ้นต้นด้วย `~/` ถูก expand เป็น home dir; ถ้าไฟล์ที่ระบุ **ไม่มีอยู่** → config error พร้อม exit 1 เพื่อกันพิมพ์ path ผิดแล้วเงียบ) |
| ไม่อ่านไฟล์เลย | `jonnyq -env-file off` (`none`/`no` ได้เหมือนกัน) |
| ไฟล์ template | repo มี `.env.example` พร้อมทุก key + คำอธิบาย — `cp .env.example .env` แล้วแก้ (`.gitignore` กัน `.env`, `.env.*`, `.envrc` ไว้แล้ว และ unignore `.env.example`) |

#### Syntax ที่รองรับ

```bash
# comment ทั้งบรรทัด (เว้น space หน้า # ก็ได้)
JONNYQ_MODEL=qwen3:14b              # inline comment (ต้องมี space คั่น)
export JONNYQ_CONTEXT_SIZE=32768    # ใส่ export นำหน้าได้ (แชร์ไฟล์กับ shell rc ได้)
JONNYQ_KEY="sk-abc def"             # double quotes: ครอบค่าที่มี space/#
JONNYQ_URL='https://x/y$NOTREAL'    # single quotes: literal ทุกอย่าง
JONNYQ_NTFY_URL=$NOTIFY_URL         # $VAR / ${VAR} / ${VAR:-fallback} ถูก expand
JONNYQ_SKILL_PATH=$HOME/skills      #   lookup จาก key ก่อนหน้าในไฟล์ก่อน แล้วค่อย environment
JONNYQ_PASS="pass$$word"            # $$ = ตัวอักษร $ (และ $FOO ที่ไม่มีจริง = คงข้อความเดิม)
MULTI="บรรทัดหนึ่ง\nอีกบรรทัด"      # double quotes รองรับ \n \t \\ \" และขึ้นบรรทัดใหม่ได้
JONNYQ_KEY=                         # KEY= ว่าง = ตั้งค่าเป็น "" (เหมือน export ว่าง)
DUP=first
DUP=second                          # key ซ้ำ = ค่าสุดท้ายชนะ
```

กติกาที่ควรทราบ:

- **ค่าที่ไม่ quote** จะถูก trim space หัว/ท้าย; `#` ที่ขึ้นต้น value หรือตามหลัง whitespace เท่านั้นถึงเป็น comment — ดังนั้น `JONNYQ_URL=http://host/#frag` และ `pass#word` ยังได้ค่าครบ
- **`$VAR` expand** จาก key ที่ประกาศ**ก่อนหน้า**ในไฟล์เดียวกันก่อน แล้วค่อย environment จริง (เพราะชั้นไฟล์ชนะชั้น env การขยายค่าจึงยึดตามไฟล์เป็นหลัก); `${VAR:-fallback}` ระบุค่าสำรอง; ถ้าหาไม่เจอจะ **คงข้อความ `$VAR` เดิมไว้** (ไม่ทำให้กลายเป็นค่าว่าง — จุดสำคัญคือ API key ที่มี `$` อยู่ในค่าต้องไม่พัง) และ `$$` = `$` หนึ่งตัว
- **คั่น `skill-path` ด้วย `;` เท่านั้น** (ไม่ใช่ `:` — path Windows `C:\\skills` จะพัง)
- **บรรทัดที่พัง** (ไม่มี `=`, ชื่อ key ผิดรูปอย่าง `9BAD=x`) → print `warning: <file>:<line>: ...` ไป stderr แล้วข้ามบรรทัดนั้น ค่าอื่นในไฟล์ยังใช้ได้ปกติ
- **quote ปิดไม่ครบ** → เป็น error ตั้งแต่เริ่ม (`unterminated double-quoted value`) เพราะอ่านต่อแล้วจะไม่รู้ตำแหน่งที่ value จบ
- `JONNYQ_*` ตัวอื่นที่ไม่รู้จัก / key ที่ไม่ใช่ของ jonnyq (เช่น `PATH=...`, `OTHER_APP=1`) ถูกเก็บไว้ให้ `$VAR` อ้างอิงได้แต่ **ไม่มีผลเป็น settings** — ตั้งค่าอื่นปนในไฟล์เดียวกันได้
- ค่า int ที่ parse ไม่ได้ (เช่น `JONNYQ_CONTEXT_SIZE=abc`) → fallback เป็น default เหมือนตอนใช้เป็น env var; `JONNYQ_TOOL_MODE`/`JONNYQ_PROVIDER` ที่ผิดรูปแบบยังเป็น config error เหมือนเดิม
- **`JONNYQ_ENV_FILE` ในไฟล์ `.env` ถูก ignore พร้อม warning** — มันกำหนดไฟล์ที่ควรอ่านไม่ได้ (circular)

#### ดู / แก้ค่าระหว่างรัน: `/env`

| คำสั่ง | ผล |
|---|---|
| `/env` | บอกว่ากำลังใช้ไฟล์ไหน (มีกี่ settings) หรือทำไมถึงไม่มี |
| `/env list` | ตารางทุก setting: ชื่อ env var, ค่าที่ใช้จริง และ **มาจากชั้นไหน** (`flag`/`file`/`env`/`default`) |
| `/env reload [path\|off]` | อ่านไฟล์ใหม่แล้ว **apply เข้า session ที่กำลังรันทันที** (ไม่มี arg = ไฟล์เดิม) |
| `/env <path>` / `/env off` | ทางลัดของ `/env reload <path>` / `/env reload off` |

แก้ไฟล์ `.env` เสร็จแล้ว `/env reload` ได้เลย — ไม่ต้อง restart ค่าที่ถูก apply ให้ทันที: `model`, `context`, `thinking`, `tool-mode`, `output` (ย้ายไฟล์ log), `searxng-*`, `fetch-*`, `pdf-*`, `run-command-timeout-sec`, `max-tool-calls-per-turn`, `skill-path`, `ntfy-url` (ถาม/ตอบกับ `ask_user` ก็ตามด้วย), `watch-magic-word`, `watch-poll-interval-sec` (ถ้า `/watchfile` กำลังทำงานอยู่ก็เปลี่ยนแบบ live) และ `provider`/`key` (สร้าง client ใหม่เมื่อเปลี่ยนจริง)

ข้อควรรู้เกี่ยวกับ `/env reload`:

- **flag ที่ใส่ตอน start ชนะเสมอ** — คำสั่งเดิมถูก replay ทุกครั้งที่ reload (`jonnyq -context 4096` แล้วแก้ไฟล์เป็น `32768` → reload แล้วยังได้ `4096`; เอา flag ออกแล้ว reload = ค่าจากไฟล์จะเริ่มมีผล)
- ค่าที่ `export` ทับไว้ใน shell **แพ้ค่าในไฟล์** — env var จะถูกใช้เฉพาะกับ key ที่ไฟล์ไม่ได้กำหนด (ถ้าเคย export ทับไว้แล้วย้ายมาใส่ในไฟล์ ไฟล์คือค่าที่ได้) อาการ "แก้ env var แล้วไม่เปลี่ยน" ส่วนใหญ่มีสาเหตุจาก key เดียวกันถูกตั้งใน `.env` ด้วย — `/env list` บอกได้ทันทีว่าค่านั้นมาจากชั้นไหน
- reload ที่ **fail** (path ผิด, `tool-mode` ผิด) → print error แล้ว session เดิมยังทำงานต่อได้เต็มที่ ไม่มีการ apply ครึ่ง ๆ กลาง ๆ
- ค่า env var ที่ export ไว้ใน shell "ลบ" ด้วย reload ไม่ได้ (jonnyq ไม่แก้ environment ของ process ตัวเอง) — สำหรับ key ที่ไม่ได้ตั้งในไฟล์ การจะปลดมันต้อง unexport ใน shell แล้ว reload ใหม่อีกรอบ (หรือใส่ key นั้นลงไฟล์เพื่อให้ค่าในไฟล์ชนะแทน)

#### ตัวอย่าง

```bash
# ผ่าน .env (แนะนำ — ต่อ project, มี .gitignore กัน key หลุด)
cp .env.example .env && $EDITOR .env
jonnyq

# ผ่าน env var (ใส่ใน .bashrc / .env.sh ได้) — มีผลกับ key ที่ .env ไม่ได้กำหนด
export JONNYQ_MODEL=qwen3:14b
export JONNYQ_CONTEXT_SIZE=32768
jonnyq

# ผ่าน flag — ชนะทั้งสองอย่าง
jonnyq -provider openai:https://api.openai.com/v1 -key sk-... -model gpt-4o-mini \
       -context 128000 -thinking=false

# ปิด thinking ชั่วคราว / เปลี่ยน tool mode
jonnyq -thinking=false -tool-mode prompt

# ใช้ profile อื่น / ปิด dotenv
jonnyq -env-file ~/.config/jonnyq/prod.env
JONNYQ_ENV_FILE=ci.env jonnyq -model qwen3:14b
jonnyq -env-file off        # อ่านเฉพาะ env var + flag เหมือนเวอร์ชันก่อน

# วิธีเดิมก็ยังให้ผลเท่าเดิม (source เข้า environment แล้ว jonnyq อ่านไฟล์เดิม
# ซ้ำอีกชั้น — ค่าสุดท้ายเหมือนกันทุก key)
set -a; . ./.env; set +a; jonnyq
```

> **หมายเหตุ `-thinking`** — ค่านี้ถูกส่งเฉพาะ Ollama (`{"think": true|false}`) ส่วน provider `openai` jonnyq ไม่ส่ง flag ใดๆ — เพียงแค่แสดงผล field `reasoning_content` (ถ้า endpoint ส่งมา) เป็น section Thinking ให้

### รายละเอียดการ parse flag (เข้ากันกับเวอร์ชัน Go)

- ทุก flag รับ value **ยกเว้น** `-thinking` (รับแบบ inline `-thinking=false` เท่านั้น) และ `-h`/`-help`
- `-thinking` แบบเดี่ยว ๆ หมายถึง `true`; `-thinking false` **จะไม่** consume argument ถัดไป (เหมือน Go's flag package) — `false` จะกลายเป็น positional argument ที่ถูกทิ้ง
- การ parse **หยุดที่ argument แรกที่ไม่ใช่ flag** (`jonnyq foo -model x` จะไม่ได้ตั้ง model) และ `--` ก็หยุด parse เช่นกัน
- ตั้ง flag ซ้ำ → **ค่าสุดท้ายชนะ**
- flag ไม่รู้จัก → error `flag provided but not defined: -xxx`; ขาด value → `flag needs an argument: -model`
- **`-h` / `-help` ทำให้โปรแกรมออกด้วย exit code 1** (หลังพิมพ์ usage) — เหมือน binary เวอร์ชัน Go
- ความแตกต่างจาก Go version: flag/package ของ Go อ่านเฉพาะ env var — repo นี้เพิ่มชั้น `.env` (`-env-file`, `JONNYQ_ENV_FILE`) ซึ่ง **อยู่เหนือ env var** (flag > `.env` > env var > default) และคำสั่ง `/env` เข้ามาเหนือโครงสร้างเดิม

---

## 6. Slash commands

พิมพ์ `/help` (หรือ `/?`) เพื่อแสดงรายการนี้ในโปรแกรม

| คำสั่ง | ความหมาย |
|---|---|
| `/?`, `/help` | แสดงคำสั่งทั้งหมด |
| `/provider <name>:<url>` | ตั้ง provider (มีผลทันที — สร้าง provider ใหม่) |
| `/key <key>` | ตั้ง provider API key |
| `/model <model>` | ตั้ง model |
| `/list_model` | แสดงรายชื่อ model ที่ provider ปัจจุบันมี |
| `/context <size>` | ตั้ง context size (จำนวนเต็มบวก) |
| `/think <true\|false>` | เปิด/ปิด thinking |
| `/run_command_timeout <seconds>` | ตั้ง timeout ของ `run_command` ระหว่างที่กำลังรันอยู่ |
| `/prompt <file>` | อ่านเนื้อหาไฟล์มาเป็น prompt แล้วส่งเลย เหมือนพิมพ์เอง |
| `/plan` | สร้าง `.progress` จาก `requirements.md` ถ้ายังไม่มี หรือ reconcile ถ้า requirement เปลี่ยน (เช็คความสอดคล้องกับโค้ดจริงด้วย) — **ไม่เขียนโค้ดใด ๆ** |
| `/plan_for_human` | เหมือน `/plan` แต่ออกแบบให้ **มนุษย์ตรวจผลงานเองผ่านการใช้งานจริง** — เขียน `> How to run:` และ `> Human check:` เก็บไว้ใน `.progress` แล้วส่งวิธีตรวจทาง ntfy |
| `/coding` | ทำ task แรกที่ยังไม่เสร็จใน `.progress` **แค่ 1 ข้อแล้วหยุด** (ต้องมี `.progress` ก่อน) |
| `/coding_phase [<เลข phase>]` | ทำ task ทั้งหมดของ **1 phase** รวดเดียว (ล้าง history ก่อนแต่ละ task) แล้วหยุดเมื่อจบ phase — ไม่ระบุเลข = phase ถัดไปที่มี task ค้าง, ระบุเลข เช่น `/coding_phase 3` หรือ `/coding_phase phase 3` |
| `/autocoding` | เรียก `/plan` ให้อัตโนมัติถ้าจำเป็น แล้วไล่ทำ **ทุก task** ใน `.progress` รวดเดียว (ล้าง history ก่อนทุก task เพื่อให้ prompt เล็กและเร็วที่สุด) |
| `/watchfile [<word>\|off]` | ไม่มี arg = toggle เริ่ม/หยุด ด้วย magic word ปัจจุบัน · ใส่ word = ตั้ง/เริ่ม watch (ถ้า watch อยู่จะเปลี่ยนคำแบบ live) · `off`/`stop` = หยุด |
| `/ntfy [<topic url>\|off]` | ไม่มี arg = แสดงค่าปัจจุบัน · ใส่ URL = ตั้ง/เปิด · `off` = ปิด — มีผลกับ `/plan`/`/coding` รอบถัดไปและกับคำถาม/คำตอบของ `ask_user` ทันที |
| `/toolmode [native\|prompt]` | ไม่มี arg = แสดงโหมดปัจจุบัน · `native`/`prompt` = สลับโหมด (มีผลรอบถัดไปทันที) |
| `/env [path\|off]` | ไม่มี arg = บอกว่า settings มาจากไฟล์ `.env` ไฟล์ไหน · ใส่ path หรือ `off` = เปลี่ยนไฟล์/ปิดแล้ว reload เลย |
| `/env list` | ตารางทุก setting: ค่าที่ใช้จริง + มาจากชั้นไหน (`flag`/`env`/`file`/`default`) — key ถูก mask เป็น `****` |
| `/env reload [path\|off]` | อ่านไฟล์ `.env` ใหม่แล้ว apply เข้า session ทันที (ดู [หัวข้อ 5.1](#51-ไฟล์-env-dotenv)) |
| `/exit`, `/bye` | ออกจากโปรแกรม |

- ถ้ายังไม่ได้ตั้ง model จะ **รับเฉพาะ slash command** เท่านั้น (พิมพ์ prompt ปกติจะเตือน `no model set; type /model <name> first`)
- คำสั่งไม่รู้จัก → `unknown command /xxx; try /help`
- สถานะที่ `/model`, `/context`, `/think`, `/toolmode`, `/ntfy`, `/run_command_timeout` ตั้งจะถูกบันทึกกลับเข้าสู่ config ด้วย จึงอ่านค่าได้ตรงกันทุกครั้ง
- `/env reload` จะ **replay flag จาก command line ตอน start** ทำให้ flag ชนะค่าในไฟล์เหมือนตอนเริ่มเสมอ ส่วนค่าที่สั่งเปลี่ยนด้วย slash command ระหว่างรันจะถูกค่าจากไฟล์/flag ทับ (นั่นคือจุดประสงค์ของ reload)

---

## 7. การพิมพ์ prompt หลายบรรทัด / Ctrl-C

### 1) ต่อบรรทัดด้วย `\`

จบทุกบรรทัดที่ต้องต่อกับ backslash แล้ว Enter — บรรทัดสุดท้ายไม่ต้องมี `\` (backslash ถูกตัดออก, บรรทัดถูกเชื่อมด้วย `\n`)

```
> แก้บั๊กใน main.rs \
ให้ handle error ตอนเปิดไฟล์ไม่เจอด้วย
```

### 2) บล็อกด้วย `"""`

พิมพ์ `"""` บรรทัดเดียวเพื่อเริ่มบล็อก พิมพ์เนื้อหาได้หลายบรรทัด แล้วพิมพ์ `"""` อีกครั้งเพื่อจบ (เหมาะกับการวางโค้ด/ข้อความที่มี backslash)

```
> """
ช่วยรีวิวโค้ดต่อไปนี้:

fn main() {
    let s = "a\\b";
}
"""
```

### 3) `/prompt <file>`

อ่าน prompt ยาว ๆ จากไฟล์แทนการพิมพ์ (ส่งเข้า agent loop ปกติ)

### Ctrl-C

กด **Ctrl-C** เพื่อยกเลิกรอบการทำงานปัจจุบันได้โดยไม่ปิด REPL — ครอบคลุมทั้ง turn ของ agent, stream ที่กำลังค้าง, process ของ `run_command` (ฆ่าทั้ง process group), stream ของ `/plan`/`/coding`/`/coding_phase`/`/autocoding` และ **การรอคำตอบของ `ask_user`** (ทั้งที่พิมพ์และที่ subscribe ntfy ไว้) เมื่อถูกยกเลิกจะแสดง `[cancelled]` และ **ไม่ส่ง** ntfy notification (ผู้ใช้ยกเลิกเองไม่ใช่ error)

Ctrl-D หรือ `/exit` / `/bye` = ออกจากโปรแกรม (หยุด watcher ให้อัตโนมัติ) — ถ้ากำลังถูก `ask_user` ถามอยู่ Ctrl-D หมายถึง "ไม่มีคำตอบ" (model ได้ข้อความว่าตอบว่าง แล้วไปต่อ) ไม่ใช่การปิดโปรแกรม

---

## 8. Tools ที่ model เรียกใช้ได้

**11 tools** — 10 ตัวถูก register ใน `src/main.rs` และ **`ask_user` ถูกติดตั้งเสมอ** ผ่าน `Registry::install_always` (model จะเห็นผ่าน tools spec ในโหมด `native` หรือผ่าน system prompt ในโหมด `prompt`)

| Tool | Parameters | พฤติกรรม / จำกัด |
|---|---|---|
| `read_file` | `path` | อ่านไฟล์ text ภายใน working dir เท่านั้น — cap **2 MB** (เกินจะถูกตัดแล้วบอกท้าย output) |
| `write_file` | `path`, `content` | สร้าง/เขียนทับไฟล์, สร้างไดเรกทอรี parents ให้เอง |
| `edit_file` | `path`, `old_string`, `new_string`, `replace_all?` | แทนที่ข้อความที่ match **แบบ exact** — error ถ้าไม่พบ หรือพบ >1 ครั้งโดยไม่ได้ตั้ง `replace_all` |
| `create_folder` | `path` | สร้างไดเรกทอรีพร้อม parents |
| `web_search` | `query` | ค้นผ่าน searxng (`/search?q=..&format=json`) — **ปิดจนกว่าจะตั้ง** `-searxng-url`; ควบคุม concurrency ด้วย semaphore |
| `web_fetch` | `url` | โหลด URL แล้ว strip HTML markup เหลือเฉพาะ text (User-Agent `jonnyq/1.0`) — รับเฉพาะ `http(s)://`, cap body **3 MB** |
| `read_pdf` | `path` | เรียก `pdftotext -layout`; ถ้าไม่มี text ที่ extract ได้ (เช่น PDF scan) จะ render หน้าเป็น PNG ด้วย `pdftoppm` ไปที่ `.jonnyq/pdf_pages/<ชื่อไฟล์แบบไม่มีนามสกุล>/page-*.png` แล้วบอกให้ไปใช้ `read_pic` ต่อ |
| `read_pic` | `path` | อ่าน image file → base64 **data URI** — cap **10 MB** |
| `run_command` | `command` | รัน `bash -c <command>` ใน working dir, รวม stdout+stderr เป็น buffer เดียว, timeout ตามค่าที่ตั้ง (ฆ่าทั้ง process group), ตรวจ denylist ก่อนรัน |
| `read_skill` | `name` | อ่านเนื้อหาเต็มของ skill ตามชื่อ (รายชื่อกับคำอธิบายอยู่ใน system prompt แล้ว) |
| **`ask_user`** | `prompt` (รับ `question` แทนได้) | **ถามมนุษย์แล้วรอคำตอบ** — prompt สีฟ้า, คำตอบสีขาวสว่าง; ถ้าตั้ง ntfy จะ push คำถามออกไปด้วย และตอบกลับจากมือถือได้ (ดู [หัวข้อ 8.1](#81-ask_user--ถามผู้ใช้แล้วรอคำตอบ)) |

**ข้อควรทราบ:** ทุก path tool ผ่าน `tools::guard::resolve_path` ซึ่ง walk ทีละ component พร้อม resolve symlink จึงกันทั้ง path traversal (`../`) และ symlink escape — อยู่นอก working dir เมื่อไรเป็น error ทันที

**หมายเหตุ `read_pic`:** เช่นเดียวกับเวอร์ชัน Go, ผลลัพธ์ถูกส่งกลับเป็น **plain text** tool result (base64 data URI) — ยังไม่ได้ต่อเข้า multimodal message format ของ provider (Ollama `images` / OpenAI `image_url`)

### 8.1 `ask_user` — ถามผู้ใช้แล้วรอคำตอบ

tool ที่ให้ model **หยุดถามคน** กลางรอบแล้วเอาคำตอบป้อนกลับเข้าบทสนทนา เหมาะกับจุดที่ตัดสินใจเองไม่ได้จริง ๆ: ทางเลือกออกแบบที่สมเหตุสมผลได้หลายทาง, credential/secret ที่หายไป, การยืนยัน action ที่ทำลายล้าง, requirement ที่สรุปจากไฟล์ไม่ได้ (description ของ tool กำชับไว้แล้วว่าอย่าใช้กับสิ่งที่ model ตรวจสอบเองได้ด้วย tool อื่น)

#### default tool — ใช้ได้เสมอ

`ask_user` ไม่ได้ถูก register ใน `main.rs` เหมือนตัวอื่น แต่ติดตั้งผ่าน `Registry::install_always` จึงมีอยู่**ในทุก registry** แม้ registry ที่ไม่ได้ register tool อื่นเลย (test registry ก็ติดตั้งด้วยวิธีเดียวกัน — มี test ยืนยันว่า registry ว่าง ๆ ยัง dispatch `ask_user` ได้) และไม่พึ่ง service ใด ๆ: ไม่มี ntfy ก็ทำงานได้ที่ terminal

#### บน terminal

หน้าตาจริงบน terminal (คำอธิบายสีเขียนไว้ท้ายแต่ละบรรทัด)

```
Tool call (2026-10-24 18:58:35) | ctx ~36/250,000 (~0%) left ~249,964 | +0.0s | req 20 in/8 out, 40.0 tok/s, 0.0s
ask_user({"prompt": "Should I use sqlite or postgres?"})

Question from the model:                                  ← bold, bright blue  (COLOR_BRIGHT_BLUE)
Should I use sqlite or postgres?                          ← blue               (COLOR_BLUE)
[waiting for your answer - type below and press Enter]     ← เทา (meta)
Your answer> postgres                                     ← bright white       (COLOR_BRIGHT_WHITE)
```

* **คำถามเป็นสีฟ้าทั้งก้อน** (รองรับหลายบรรทัด) — `COLOR_BLUE` ตัวคำถาม, `COLOR_BRIGHT_BLUE` + bold สำหรับหัวข้อ
* **สิ่งที่ผู้ใช้พิมพ์เป็นสีขาวสว่าง** — `COLOR_BRIGHT_WHITE` + bold; jonnyq ไม่ได้ echo แทน terminal จึง**ปล่อย attribute ค้างไว้** หลังพิมพ์ `Your answer> ` (`Writer::colored_keep`) เพื่อให้ keystroke ของผู้ใช้เองตกเป็นสีขาวสว่าง แล้ว `reset_color()` เมื่ออ่านจบ
 * หัวข้อ/คำถาม/`Your answer> `/คำตอบถูก mirror ลง transcript log ด้วย (log เป็นข้อความล้วน ไม่มีสี — จึงไม่มี ANSI ตกค้างในไฟล์)
* ตอบหนึ่งบรรทัดแล้ว Enter:
  * ข้อความปกติ → `Answer from terminal:\n<ข้อความ>`
  * Enter ว่าง ๆ หรือ **Ctrl-D** (หมด input) → `(the user sent an empty reply from terminal; treat it as "no further input")` — Ctrl-D ระหว่างถูกถามแปลว่า "ไม่มีคำตอบ" ไม่ใช่ "ปิดโปรแกรม"
  * **Ctrl-C** → ยกเลิกรอบนั้น (`context canceled`) ทั้งการรอพิมพ์และการ subscribe ntfy
* ถ้า ntfy เปิดอยู่ hint จะเขียนว่า `... type below, or reply to the ntfy notification with "answer: <text>"` และถ้า stdin ไม่ใช่ terminal จะเหลือแค่เวอร์ชัน ntfy (ไม่พิมพ์ `Your answer> ` ที่ไม่มีใครพิมพ์ตามได้)

#### ผ่าน ntfy (ถ้าตั้งค่า)

เมื่อตั้ง `-ntfy-url` / `JONNYQ_NTFY_URL` / `/ntfy <topic url>` คำถามจะถูก **push ออกไปด้วย priority สูง + tag warning** (title `jonnyq: your answer is needed`) พร้อมวิธีตอบ แล้ว jonnyq จะ subscribe topic นั้น (`<topic>/json?since=<วินาทีที่ถาม`) เพื่อรอคำตอบ:

```
Title: jonnyq: your answer is needed      (Priority: high, Tags: warning)

Which port should the server listen on?
1) 3000  2) 8080

jonnyq is waiting for your answer.
Reply to this notification with:
    answer: <your answer>
(or send the answer on its own - the next message published to this topic is taken as the answer)
```

ตอบกลับจากมือถือ (quick reply ของแอป ntfy, `curl -d`, หรือ publish จาก web UI) — แล้ว jonnyq Echo กลับที่ terminal:

```
Your answer>                                  ← ยังรออยู่
You (via ntfy)> 8080, and use postgres       ← bright white
```

* **ตอบทางไหนก่อนได้ก่อน** — พิมพ์ที่ keyboard หรือตอบจากมือถือ; ไม่ต้องรอกัน (select ทั้งสอง channel พร้อมกัน)
* วิธีตอบ: `answer: <ข้อความ>` (รองรับ `Answer:` / `ANSWER:` / `reply:` / `Reply:` / `a:` / `A:` และพิมพ์คำตอบต่อบรรทัดถัดไปได้) หรือไม่ใส่ prefix เลยก็ถือทั้งข้อความนั้นเป็นคำตอบ — เข้ากับการ quick reply ของแอป ntfy และ `curl -d <topic> -d "sqlite"`
* jonnyq **ไม่รับคำถามของตัวเองเป็นคำตอบ**: message ที่ title ตรงกับคำถาม (หรือ body ตรงกัน) ถูกข้ามเสมอ และ subscribe ตั้งแต่ instant ที่ถาม ข้อความเก่าใน cache ของ topic จึงไม่กลายเป็นคำตอบ
* ผลที่ส่งเข้า model ระบุที่มาเสมอ — `Answer from ntfy:` หรือ `Answer from terminal:` — model จึงแยกแยะคำตอบจากมือถือออกจากคำตอบที่พิมพ์
* คำถามยาวเกิน 2000 bytes ถูกตัดบน character boundary (ปลอดภัยสำหรับไทย/UTF-8) ก่อน push เพราะ ntfy จำกัด ~4 KB — terminal เห็นคำถามเต็มเสมอ
* ntfy **ล่ม/subscribe ไม่ได้** → warning `ntfy answer channel unavailable (...) - still waiting for you to type an answer below` แล้วรอพิมพ์ต่อ (ไม่ retry ถี่); ถ้า ntfy เป็นช่องทางเดียว (stdin ไม่ใช่ terminal) → error พร้อมเหตุผล ไม่ค้าง
* `/ntfy <url>`, `/ntfy off` และ `/env reload` มีผลกับ `ask_user` ทันที (tool อ่านค่า topic ใหม่ทุกครั้งที่ถาม)
* ⚠️ ntfy ไม่มีการยืนยันตัวคนตอบ — ดู [หัวข้อ 17](#17-ความปลอดภัย-โดยตั้งใจ)

#### เมื่อไปถึงตัวผู้ใช้ไม่ได้เลย

ถ้า stdin ไม่ใช่ terminal (`jonnyq < prompts.txt`, CI) และไม่มี ntfy topic — tool จะ **error ตรง ๆ** (`ask_user has no way to reach the user: stdin is not a terminal and no ntfy topic is set ...`) ไม่ hang รอ input ที่ไม่มีใครพิมพ์ได้

**stdin เป็นของส่วนกลาง:** REPL และ `ask_user` อ่านจาก stream เดียวกัน (`src/input.rs`) จึงไม่มีทางแย่งกันกิน type-ahead; `ask_user` อ่านเฉพาะตอนกำลังรอ tool call เท่านั้น



---

## 9. โหมดเรียกใช้ tool: `native` vs `prompt`

ตั้งต้นด้วย `-tool-mode` / `JONNYQ_TOOL_MODE`, เปลี่ยนกลางคันได้ด้วย `/toolmode`

### `native` (default)

ส่ง tools spec ไปกับทุก request และรับ tool calls เป็น structured `ToolCalls` event ตาม protocol ของ provider — ผล tool ถูกส่งกลับด้วย `role: "tool"` + `tool_call_id`

### `prompt`

สำหรับ model/backend ที่ native function-calling ไม่เสถียร (ส่ง argument ผิด/ขาด field ซ้ำ ๆ):

- **ไม่ส่ง native tools spec ให้ provider เลย** (การส่งพร้อมกับให้ model พิมพ์ call เป็น text มักไปกระตุ้น grammar-constraint ของ backend ที่พังอยู่แล้ว — ตรงข้ามกับที่โหมดนี้ต้องการหลีกเลี่ยง) แล้วเขียนรายชื่อ tool + JSON schema ลงใน system prompt แทน
- model ต้องตอบกลับด้วย fenced block รูปแบบนี้ (อย่างเดียวใน response นั้น):

  ````
  ```tool
  {"name": "read_file", "arguments": {"path": "data.txt"}}
  ```
  ````

- ผล tool ถูกส่งกลับเป็น **plain user message** (`Tool result (<tool>): <output>`) แทน `tool` role เพราะ model ที่ต้องพึ่งโหมดนี้มักไม่เข้าใจ convention นั้น และบาง backend อาจปฏิเสธ request ที่มี tool role โดยไม่เคยประกาศ tools
- JSON ผิดรูปแบบหรือขาด `"name"` → ได้ error message ที่ชัดเจนกลับไปในรอบถัดไปทันที (ไม่ถูกส่งผ่าน `Registry::call` ซึ่งจะบอกแค่ "unknown tool")

> **หมายเหตุ UX:** เนื่องจาก answer ถูก stream ออกจอ real-time อยู่แล้ว fenced block ดิบ ๆ ที่ model พิมพ์จะโผล่ปนอยู่ใน section **Answer** ตามปกติ จากนั้นจึงมี section **Tool call** แสดงผล parse/เรียกจริงอีกที — trade-off ที่ยอมรับได้เพื่อคง real-time streaming ไว้ทุกโหมด

---

## 10. การทำงานอัตโนมัติ: `/plan` → `.progress` → `/coding`

### ไฟล์ที่เกี่ยวข้อง

| ไฟล์ | หน้าที่ |
|---|---|
| `requirements.md` | **input ที่คุณเขียน** — ข้อกำหนดของงาน (จะใส่ ID เช่น `REQ-012` ไว้ด้วยก็ได้) |
| `.progress` | checklist งานแบบ GitHub checkbox ที่ model สร้าง/อัปเดต |
| `.progress.hash` | SHA-256 ของ `requirements.md` ณ ครั้งล่าสุดที่ generate/reconcile — ใช้ตัดสินว่าต้อง reconcile หรือไม่ต้อง |
| `.progress.retry` | สถานะ retry ของ `/coding` (task ไหน / กี่ครั้ง / แตะไฟล์หรือยัง) — เก็บข้าม invocation เพราะ `/coding` ไม่มี loop ในหน่วยความจำ |

### รูปแบบ `.progress`

```markdown
# Project Title

> How to run: `cargo run` → http://localhost:3000, login with demo/demo123

## Phase 1: Scaffolding & config

> Human check: รัน `cargo build` แล้วต้องไม่มี error -> เห็น binary ที่ target/debug

- [ ] Init Cargo.toml with tokio + serde dependencies (REQ-001)
- [ ] Add CLI flag parsing module

## Phase 2: Core logic

- [ ] Implement the tool registry
- [x] Implement context compaction
```

- เส้น `- [ ]` / `- [x]` เท่านั้นที่ถูกนับเป็น task (parser `^(- \[([ xX])\])\s*(.+)$` ข้ามบรรทัดอื่นทั้งหมด) — หัวข้อ `#`/`##` และบรรทัด blockquote `> ` จึง **ไม่ใช่** task และถูกรักษาไว้ตอน reconcile
- task **ถูกสั่งให้เขียนเป็นภาษาอังกฤษเสมอ** แม้ `requirements.md` จะเขียนด้วยภาษาอื่น (บาง model เขียนภาษาอื่นได้ไม่ดีเท่า; ชื่อ/label เฉพาะเจาะจงจาก requirement เก็บเป็นภาษาเดิมได้)
- prompt ตอน plan กำชับให้แตกงานให้ **ละเอียดที่สุด** (task เล็ก ตรวจแยกได้) และจัดเป็น phase ตามลำดับ dependency (scaffolding → data → core logic → API → UI → testing/verification)

### เปรียบเทียบ 4 คำสั่ง

| | `/plan` | `/coding` | `/coding_phase` | `/autocoding` |
|---|---|---|---|---|
| สร้าง `.progress` เอง | ✅ (generate หรือ reconcile) | ❌ (ต้อง create ก่อน) | ❌ (ต้องใช้ก่อน) | ✅ (เรียก `/plan` อัตโนมัติ) |
| ขอบเขตที่ทำ | ไม่เขียนโค้ดเลย | 1 task | 1 phase | ทุก task |
| ล้าง history ก่อนแต่ละ task | – | ❌ | ✅ | ✅ |
| AUTONOMOUS CODING MODE | – | ✅ | ✅ | ✅ |
| หยุดเมื่อ | จบ plan | จบ 1 task | จบ phase | ครบทุก task |
| แจ้ง ntfy | ✅ | ✅ (พร้อม phase guide ถ้าจบ phase) | ✅ ทุกจบ phase | ✅ ครั้งเดียวเมื่อครบทั้งหมด |
| Ctrl-C ยกเลิกได้ | ✅ | ✅ | ✅ | ✅ (หยุดที่ขอบ round — หลัง task ปัจจุบันจบ loop เจอ token ถูก cancel จะออกทันที) |

### ตัวกันวน (safety valves)

| กลไก | ค่า | พฤติกรรม |
|---|---|---|
| Same-task stall | **5** รอบ (`MAX_STALL_ROUNDS`) | task เดิมยัง `- [ ]` ซ้ำ 5 รอบ → หยุดพร้อม error + ส่ง ntfy `stalled` |
| No-progress rounds | **15** รอบ (`MAX_NO_PROGRESS_ROUNDS`) | 15 รอบติดที่ **ไม่มี task ไหนในไฟล์ถูกติ๊กเพิ่มเลย** (ต่อให้ text ของ `next` เปลี่ยนทุกรอบ) → หยุดทันที กันการวนได้เรื่อย ๆ ไม่มีที่สิ้นสุด |

ข้อความเตือนตอน retry **ต่างกัน 2 แบบ** ตามสิ่งที่ตรวจพบจริงจากการเทียบเนื้อหา `.progress` ก่อน/หลังรอบก่อนหน้า:

1. รอบก่อน **ไม่ได้เรียก `edit_file`/`write_file` แตะ `.progress` เลย** (แค่พิมพ์ยืนยันในคำตอบ หรือ echo ข้อความ "✓ passed" ปลอมผ่าน `run_command`) → เตือนตรง ๆ ว่าการบรรยายไม่เท่ากับการแก้ไฟล์ ต้องเรียก tool จริง
2. แตะไฟล์แล้วแต่ task ยังไม่ถูกติ๊ก (เช่น `edit_file` ไม่ match exact text หรือ `write_file` เขียนทับทั้งไฟล์จาก context เก่า) → สั่งให้ `read_file` เช็คของจริงก่อนแก้ต่อ

### `/plan_for_human`

ออกแบบให้ **คนตรวจผลงานเองได้ด้วยการใช้งานจริง** (ผ่าน frontend เป็นหลัก):

- `> How to run: <คำสั่งรัน, URL, ข้อมูลตัวอย่าง, บัญชีทดสอบ>` ไว้ใต้ชื่อโปรเจกต์
- `> Human check: <ขั้นตอนตรวจ> -> <ผลที่ควรเห็น>` ไว้ใต้ทุก `## Phase`
- task สุดท้ายของ phase สุดท้ายจะสั่งให้ model ทบทวน/แก้บันทึกเหล่านี้ให้ตรงกับของที่สร้างจริง (URL, port, คำสั่ง, ชื่อปุ่ม ฯลฯ)
- ข้อความแจ้งเตือน (ภาษาอังกฤษ) **ดึงจากไฟล์โดยตรง ไม่เรียก model เพิ่ม**: สรุปจำนวน task + `HOW TO RUN` + `HOW TO VERIFY (per phase)` + เวลา/สถิติ (guide ยาวเกิน ~3,000 ไบต์จะถูกตัดและชี้ให้ดู `.progress` เพราะ ntfy จำกัดข้อความราว 4 KB)
- ถ้า model ไม่เขียนบันทึกตามรูปแบบ จะสั่งให้เติมอีก 1 รอบ และถ้ายังไม่มีจะแนบ `WARNING` ในข้อความ (ไม่ถือเป็น error — plan ยังใช้ได้)
- โปรเจกต์ที่ไม่มี frontend จะถอยไปใช้สิ่งที่คนสังเกตได้ใกล้ที่สุด (Swagger, curl, CLI commands พร้อมผลลัพธ์ที่คาดหวัง)

> **ข้อจำกัด:** model เขียนคู่มือจาก **ความตั้งใจ** ของมัน ไม่ได้เปิดเบราว์เซอร์ทดสอบเอง — คนต้องลองทำตามขั้นตอนจริง

---

## 11. `/watchfile` — pair programming ผ่าน magic word

เปิดการใช้งาน:

```
> /watchfile            # toggle ด้วย magic word ปัจจุบัน (default "AI!")
> /watchfile TODO!      # ตั้ง magic word เป็น "TODO!" และเริ่ม watch
> /watchfile            # (กำลัง watch อยู่) → หยุด
> /watchfile off        # หยุดอย่างชัดเจน
```

จากนั้นแก้ไฟล์โค้ดปกติ แล้วพิมพ์บรรทัดที่มี magic word:

```rust
// AI! เพิ่ม error handling กรณีหาไฟล์ไม่เจอ แล้วเขียน test ครอบคลุมด้วย
let data = std::fs::read(path).unwrap();
```

watcher (poll ทุก `-watch-poll-interval-sec` วินาที — **ไม่พึ่ง fsnotify**, walk เองด้วย `std::fs::read_dir`) จะจับการเปลี่ยน mtime แล้ว scan หาบรรทัดที่ประกอบด้วย magic word, emit trigger และ REPL จะ:

1. พิมพ์ `[watchfile] src/main.rs:12: // AI! ...`
2. ส่ง prompt ให้อ่านโค้ดรอบ ๆ บรรทัดนั้น แล้วทำตามคำสั่ง
3. **แก้/ลบบรรทัด trigger เอง** หลังทำงานเสร็จ เพื่อไม่ให้ fire ซ้ำ

**กฎการข้าม (skip):** dotfile/dot-directory ทั้งหมด (`.git`, `.progress*`, `.jonnyq/`) · `node_modules` · `vendor` · **ไฟล์ log ของตัวเอง (`-output`)** — กัน transcript สะท้อน trigger กลับเข้ามาแล้ว fire ตัวเองซ้ำไม่รู้จบ

**พฤติกรรมอื่น:**

- บรรทัดที่เคย trigger แล้วจะไม่ trigger ซ้ำ **จนกว่าเนื้อหาบรรทัดนั้นจะเปลี่ยน**
- **เปลี่ยน magic word ได้แบบ live** ขณะ watch อยู่ โดยไม่ต้อง restart
- trigger ถูกส่งเข้า channel (buffer 32) แล้ว merged เข้ากับ input loop เดียวกับ stdin ผ่าน `tokio::select!` → ไม่มี concurrent access ต่อ Agent/UI; ถ้า channel เต็ม trigger จะถูก drop (บรรทัดนั้นถูก mark ว่าเห็นแล้ว — แก้ไฟล์ใหม่ถ้าต้องการให้ fire จริง)

---

## 12. การแจ้งเตือนผ่าน ntfy

ตั้ง topic URL เต็ม ๆ (ntfy.sh หรือ self-hosted ก็ได้) ผ่าน `-ntfy-url` / `JONNYQ_NTFY_URL` / `/ntfy <topic url>` (ค่าว่าง = ปิด)

ntfy เป็น **ขาลง** (push) ก็จริง แต่ jonnyq ใช้อันเดียวนี้เป็น **ขาขึ้น** ได้ด้วย: tool `ask_user` subscribe topic เดียวกันเพื่อรับคำตอบจากมือถือ ([หัวข้อ 8.1](#81-ask_user--ถามผู้ใช้แล้วรอคำตอบ)) — สรุปข้อความที่ส่งออกแต่ละประเภท:

| สถานการณ์ | Title | ความสำคัญ |
|---|---|---|
| `/plan` / `/plan_for_human` / `/coding` / `/coding_phase` / `/autocoding` จบ | `jonnyq: /<cmd> complete` (หรือ `... phase complete`, `... all tasks complete`) | ปกติ |
| turn / งานล้มเหลวหรือ stall | `jonnyq: ... failed`, `jonnyq: error` | สูง + tag `warning` |
| **`ask_user` ยกคำถามขึ้นถาม** | `jonnyq: your answer is needed` | สูง + tag `warning` |

### แจ้งเมื่อทำงานจบ (สำเร็จ)

```
Title: jonnyq: /coding complete

Completed: "add error handling to the file loader"
6/12 tasks done

Started: 2026-10-24 10:15:03 +0700
Finished: 2026-10-24 10:17:41 +0700
Duration: 2m38s
Stats: preload=1.2s prompt_eval=340ms generate=95.4s token_in=4521 token_out=812 total_token=5333 tok/s=8.51
```

- เมื่อ `/coding` ทำ task สุดท้ายของ **phase** เสร็จ เนื้อหา notification จะขึ้นต้นด้วย `PHASE COMPLETE: <phase>` + วิธีตรวจของ phase นั้น + วิธีรัน (title `jonnyq: /coding phase complete`) และเมื่อเป็น task สุดท้ายของ **ทั้งงาน** จะส่ง guide ทั้งหมด (title `jonnyq: /coding all tasks complete`); `/coding_phase` แจ้งทุกครั้งที่จบ phase (รายการ task ที่ทำใน phase, cap 20 ชื่อ) ถ้า `.progress` ไม่มีบันทึกวิธีตรวจจะแนะนำให้ใช้ `/plan_for_human`
- `/autocoding` แจ้ง **ครั้งเดียว** เมื่อครบทุก task (ไม่แจ้งทีละ task เพราะเรียก `/plan` ภายในอยู่ ถ้าแจ้งด้วยจะรัวข้อความทุก task)
- เนื้อหาสรุปคำนวณจาก `.progress` โดยตรง (**ไม่เรียก model มาสรุปเพิ่ม** — เร็วกว่าและไม่เสี่ยง hallucinate)
- กรณีไม่มีการเรียก model จริง (เช่น task ครบหมดแล้ว) ค่า `Stats:` จะเป็น `n/a` ทั้งหมดแทนที่จะโชว์ตัวเลขค้างจาก turn ก่อนหน้า

### แจ้งเมื่อ error / stall (priority สูง, tag `warning`)

```
Title: jonnyq: /coding failed

Error: ollama: unexpected status 404 Not Found: model not found

Reason: The provider could not find the endpoint or model. Check that the model name is
correct and installed (/list_model) and that the provider URL is right.

Started: ...  Finished: ...  Duration: ...
```

`Reason:` ถูกจับคู่จากข้อความ error แบบ heuristic (`src/notify/explain.rs`) เช่น: server ไม่ได้รัน/URL ผิด (`connection refused`), API key ผิด (401/403), ไม่พบ model (404), rate limit (429), context เกิน (400 / `context length`), server ล่ม/overloaded (5xx), connection หลุดกลางทาง, TLS/certificate, timeout, model วนเรียก tool เกิน budget, task ค้างไม่คืบหน้า, ไม่มี `requirements.md`/`.progress`, disk เต็ม, permission denied ฯลฯ — ไม่รู้จัก → บอกว่าไม่ทราบสาเหตุให้ดู terminal/output log

- **ทุก turn ที่จบด้วย error** (prompt ปกติ, `/prompt`, watchfile trigger ฯลฯ) จะถูก notify ด้วย — ยกเว้น Ctrl-C และกรณี `/plan`/`/coding`/`/coding_phase`/`/autocoding` ที่ส่งข้อความละเอียดกว่าไปแล้ว (ไม่ส่งซ้ำ)
- **ส่ง notification ไม่สำเร็จ → แสดงแค่ warning, ไม่ทำให้งานล้มเหลว**

### ป้าย `generate=` และค่าที่วัดเอง

- `generate=` คือเวลา generate **ทั้งหมด** (รวมช่วงคิดและคำตอบ) ไม่ใช่แค่ช่วงคิด
- provider `ollama` → server รายงานเวลาจริง
- provider `openai` (รายงานแค่จำนวน token) → jonnyq **จับเวลาเองขณะอ่าน stream** แล้วใส่ `~` นำหน้า: `prompt_eval=~` = เวลาจนได้ token แรก (รวมโหลด model + prompt + network แยกกันไม่ได้ จึง `preload=n/a`), `generate=~` = token แรกถึง token สุดท้าย, `tok/s=~` = token ÷ เวลา generate; ถ้า endpoint ไม่ส่ง `usage` มาจะประเมิน token จากจำนวนตัวอักษร (~4 ตัว/token) และแสดง `token_out=~N` (in/total = `n/a`) ค่าเหล่านี้แม่นพอสมควรกับ server ใน local (LM Studio, llama.cpp) แต่จะคลาดเคลื่อนถ้ามี network/proxy คั่น

---

## 13. Skills

Skill คือความรู้/คำแนะนำที่พร้อมใช้ ซึ่งคุณเขียนเป็น markdown แล้วให้ model เลือกอ่านเองเมื่อจำเป็น ระบบ skill ถูก discover ใหม่ **ทุกรอบ turn** และสรุป (ชื่อ + บรรทัดแรกของเนื้อหา) เข้าไปใน system prompt

### รูปแบบ

```
~/my-skills/
├── react/
│   └── SKILL.md        ← skill "react"
├── postgres.md         ← skill "postgres" (ไฟล์เดี่ยว)
└── notes.txt           ← ถูกข้าม (ไม่ใช่ .md)
```

- **โฟลเดอร์ที่มี `SKILL.md`** → skill ชื่อเดียวกับโฟลเดอร์
- **ไฟล์ `*.md` ที่อยู่ตรง ๆ** → skill ชื่อเดียวกับไฟล์ (ตัด `.md`)
- **description** = บรรทัดแรกที่ไม่ว่างและไม่ใช่ heading (ตัด `#` ออก) truncate ที่ 200 bytes (ไม่แบ่งตัวอักษร multi-byte)
- directory ที่อ่านไม่ได้ (ไม่มีอยู่/permission) ถูกข้ามเงียบ ๆ

### เปิดใช้งาน

```bash
jonnyq -skill-path ~/my-skills\;~/shared-skills
# หรือ env var
export JONNYQ_SKILL_PATH="$HOME/my-skills;$HOME/shared-skills"
# หรือใน .env ($HOME ถูก expand ให้เอง)
JONNYQ_SKILL_PATH=$HOME/my-skills;$HOME/shared-skills
```

> **คั่นด้วย `;` เท่านั้น** — `:` ไม่ได้ถูกใช้เป็น separator (กัน path แบบ Windows เช่น `C:\\skills` พัง) ถ้าใส่ `:` ทั้งสอง path จะกลายเป็น path เดียวที่อ่านไม่เจอ (directory ที่อ่านไม่ได้จะถูกข้ามเงียบ ๆ) ใน shell ต้อง escape เป็น `\;` หรือครอบทั้งค่าด้วย single quotes ไม่งั้น `;` จะถูก shell ตีความเป็นตัวคั่นคำสั่ง

เมื่อเห็นรายการ skill ใน system prompt, model จะเรียก `read_skill {"name": "react"}` เพื่อดึงเนื้อหาเต็มมาใช้

---

## 14. การแสดงผลบน terminal และ context usage

### สีและ section

| สี | ความหมาย |
|---|---|
| ขาว | prompt ของ user |
| **น้ำเงิน / น้ำเงินสว่าง** | คำถามที่ `ask_user` ยกขึ้นมาถาม ([หัวข้อ 8.1](#81-ask_user--ถามผู้ใช้แล้วรอคำตอบ)) |
| **ขาวสว่าง (bright white)** | คำตอบที่ผู้ใช้พิมพ์/ตอบกลับผ่าน ntfy |
| เขียว | thinking |
| แดง | tool call |
| เหลือง | answer |
| เทา | metric ท้าย turn / ข้อความ meta |

แต่ละ section มีบรรทัดว่างคั่น และมีหัวข้อตัวหนาสีสดกำกับ **พร้อม timestamp และสถานะ context** เช่น:

```
Tool call (2026-10-24 10:24:10) | ctx 7,100/16,348 (43%) left 9,248 | +25.1s | req 1,420 in/380 out, 22.4 tok/s, 17.0s
```

- หัว **`Tool call`** ใช้ป้าย `req` — พิมพ์หลัง request ที่สั่ง tool นั้นจบพอดี จึงเป็นสถิติของ request นั้นจริง
- หัว **`Thinking`/`Answer`** ใช้ป้าย `last req` — request ปัจจุบันยังไม่จบ จึงเป็นของรอบก่อนหน้า (section แรกของ turn จะยังไม่มีส่วนนี้)
- เวลา `+...` ของ `/autocoding`/`/coding_phase` **นับต่อ task** เพราะแต่ละ task คือหนึ่ง turn

### ตัวเลข context

- `Thinking (10:24:31) | ctx ~7,600/16,348 (~46%) left ~8,748` — ใช้ `prompt_tokens` จริงจาก provider รอบล่าสุดเป็นฐาน แล้วบวกส่วนที่เพิ่มมาตั้งแต่่นั้น (~4 ตัวอักษร/token); ค่าประมาณมี `~` นำหน้า, ถ้า backend ไม่ส่ง usage จะประเมินทั้งหมดจาก system message + history
- บรรทัด metric ท้าย turn `| ctx 7,600/16,348 (46%) left 8,748` — เป็นค่า **จริง** (prompt+completion token ของ request สุดท้าย) เมื่อ provider รายงาน
- **สีเตือน**: เทา = ปกติ, **เหลือง = ตั้งแต่ 60%** (จุดที่จะ compact), **แดง = ตั้งแต่ 85%**
- อิงค่า context ที่ตั้งไว้ — provider `openai` (เช่น LM Studio) **ต้องตั้ง context ฝั่ง server ให้ตรงกันเอง**
- ตัวเลขที่หัว section คือค่า **ณ ตอน section เริ่ม** — ไม่นับ token ที่กำลัง stream อยู่

### transcript log

ทุกอย่างที่พิมพ์ออกจอถูก mirror ไปยังไฟล์ `-output` (default `output.txt`) ในโหมด **append, ข้อความล้วน (ANSI ถูก strip)** — system prompt กำชับ model ไว้แล้วว่าไฟล์นี้ (อ้างชื่อไฟล์จริงตามที่ตั้งค่า) เป็น log ของโปรแกรมเท่านั้น ไม่ต้องอ่านหรือสนใจ; `/watchfile` ก็ข้ามไฟล์นี้เช่นกัน

เมื่อ `/env reload` แล้ว `JONNYQ_OUTPUT_FILE` เปลี่ยน jonnyq จะ **สลับไปเขียนไฟล์ใหม่ทันที** (append mode, สร้างไฟล์ถ้ายังไม่มี) พร้อมอัปเดต path ที่บอก model และรายการข้ามของ `/watchfile` ตาม — ถ้าเปิดไฟล์ใหม่ไม่สำเร็จจะ warning แล้วเขียนไฟล์เดิมต่อ ไม่ทำให้ session พัง

---

## 15. สถาปัตยกรรมและโครงสร้างโค้ด

```
jonnyq-rust/
├── Cargo.toml              name = "jonnyq", edition 2021, [profile.release] strip=true
├── Cargo.lock
└── src/
    ├── main.rs             entry point: load config → open log → build tool registry
    │                        (10 tools + install_always(ask_user)) → build Agent →
    │                        hand control to REPL on the tokio runtime
    ├── lib.rs              crate root (module layout mirrors Go's internal/)
    ├── config.rs           CLI flag + JONNYQ_* env + .env parsing (FlagParser port ของ Go flag)
    │                        → precedence flag > file > env > default, Source provenance,
    │                          reload_with() สำหรับ /env reload (replay flag ตอน start)
    │   └── dotenv.rs        dotenv parser (comment/quote/export/$VAR expand, warnings)
    ├── error.rs            AgentError { message, notified } — `notified` กัน notify ซ้ำ
    ├── input.rs            stdin ที่อ่านเป็นบรรทัด — จุดอ่าน input เดียวของทั้งโปรแกรม
    │                        (REPL + ask_user ใช้ stream ร่วมกัน จึงไม่แย่ง type-ahead),
    │                        read_line_cancellable (Ctrl-C), read_input (multi-line)
    ├── ui.rs               สี terminal + plain-text log, UiState snapshot, section headers,
    │                        ContextStatus, format helpers (Go Duration.String compatible),
    │                        coloredln/colored_keep/reset_color (สีของ ask_user)
    ├── repl.rs             prompt loop, read_input (multi-line), slash commands (รวม /env),
    │                        Ctrl-C → CancellationToken, watcher trigger merging,
    │                        reload_env() = push ค่าใหม่เข้า agent/tools/watcher แบบ live
    ├── agent.rs            tool-calling loop, system prompt, context compaction, metrics,
    │                        ToolCallMode, RepeatDetector (in agent/repeat.rs),
    │                        prompt-mode tool parsing (agent/prompttools.rs)
    ├── skill.rs            skill discovery (dir/SKILL.md และ *.md)
    ├── coding/             /plan, /plan_for_human, /coding, /coding_phase, /autocoding
    │   ├── mod.rs           .progress parsing (checkbox regex), requirements hash, retry
    │   │                     state, stall valves, task/plan/reconcile prompts
    │   ├── phase.rs         /coding_phase: phase selection + shared task loop
    │   ├── human.rs         /plan_for_human: parse "> How to run:" / "> Human check:"
    │   │                     notes → notification text
    │   └── port_tests.rs    port parity tests (generate/reconcile/stall/ntfy)
    ├── watch.rs            /watchfile: poll loop, magic word, skip rules,
    │                        interval/exclude ปรับได้ระหว่างรัน (shared mutex)
    ├── notify/             ntfy publisher (mod.rs) + heuristic error explainer (explain.rs),
    │                        stream_messages() = subscribe topic แบบ JSON stream (ขาลงของ
    │                        ask_user: รับคำตอบจากมือถือ)
    ├── testsupport.rs      [cfg(test)] only: TestServer (HTTP/1.1 on localhost, รวมถึง staged/
    │                        open chunked stream สำหรับ ntfy), stub providers, temp dirs,
    │                        input_lock()/DelayedLines/SilentLines — เทียบเท่า httptest.NewServer ของ Go
    ├── llm/
    │   ├── mod.rs
    │   ├── types.rs         Provider trait (chat / list_models), ChatEvent, ChatRequest,
    │   │                     Message, ToolSpec, Usage, new_provider()
    │   ├── ollama.rs        POST /api/chat (NDJSON stream), GET /api/tags, think + num_ctx
    │   └── openai.rs        POST /chat/completions (SSE, include_usage), GET /models,
    │                         StreamTimer (client-side timing)
    └── tools/
        ├── mod.rs           Tool trait (spec / call / configure), Registry, CallCtx,
        │                     ResizeableLimiter (concurrency ที่เปลี่ยนค่าระหว่างรันได้)
        ├── guard.rs         resolve_path — confine to working dir (symlink-safe)
        ├── file.rs          read_file (2MB cap), write_file, edit_file, create_folder
        ├── run_command.rs   bash -c, process-group kill, stdout/stderr merged, WaitDelay 5s,
        │                     timeout อ่านใหม่ทุก call (ปรับได้ด้วย /run_command_timeout, /env)
        ├── denylist.rs      20 regex patterns กันคำสั่งทำลายล้าง
        ├── search.rs        web_search (searxng JSON API, disabled if no url)
        ├── web_fetch.rs     web_fetch (regex-based HTML → text, 3MB cap)
        ├── pdf.rs           read_pdf via pdftotext/pdftoppm (poppler-utils)
        ├── pic.rs           read_pic (base64 data URI, 10MB cap)
        ├── skill_tool.rs    read_skill
        └── ask_user.rs      ask_user (default tool — Registry::install_always): คำถามสีฟ้า
                              คำตอบสีขาวสว่าง, ntfy two-way (push คำถาม + subscribe คำตอบ),
                              parse_ntfy_answer / build_ntfy_message / truncate_utf8
```

### Dependencies

รายการด้านล่างคือที่ประกาศใน `Cargo.toml` พร้อมเวอร์ชันที่ resolve จริงใน `Cargo.lock` ณ ตอน build ครั้งล่าสุดใน repo นี้

| crate | เวอร์ชัน | ใช้ทำอะไร |
|---|---|---|
| `tokio` (features: rt-multi-thread, macros, sync, time, process, io-util, signal, fs, io-std) | 1.54 | async runtime, subprocess, signal handling |
| `tokio-util` | 0.7 | `CancellationToken` (Ctrl-C / repeat cutoff) |
| `reqwest` (default-features = false; features: rustls-tls, stream, json) | 0.12 | HTTP client — TLS แบบ pure Rust (rustls 0.23) ไม่ใช้ cgo/OpenSSL |
| `serde` + derive, `serde_json` | 1.0 / 1.0 | config & JSON wire format, tool parameter schemas |
| `regex` | 1.13 | denylist, HTML-to-text, checkbox parsing, phase headings, ANSI strip |
| `futures-util` | 0.3 | อ่าน byte stream แบบ async |
| `async-trait` | 0.1 | `Provider` / `Tool` traits |
| `chrono` | 0.4 | timestamp ใน system prompt และหัวข้อ section |
| `base64` | 0.22 | `read_pic` data URI |
| `sha2` | 0.10 | hash `requirements.md` (`.progress.hash`) |
| `percent-encoding` | 2.3 | query encoding ของ searxng |
| `html-escape` | 0.2 | decode HTML entity ใน `web_fetch` |
| `libc` | 0.2 | `kill(-pid, SIGKILL)` ฆ่าทั้ง process group |
| `anyhow` | 1.0 | error plumbing (`From<anyhow::Error> for AgentError`) |

### การยกเลิกงาน (cancellation model)

```
Ctrl-C (tokio::signal) → cancels the in-flight turn token
    └─ run_turn's loop checks it between rounds
    └─ chat_cancel = cancel.child_token() → provider aborts the HTTP stream
         ├─ run_command selects on it while waiting → kill_process_group()
         └─ ask_user selects on it too → a half-typed answer is abandoned and
              the ntfy subscription task is torn down (turn = "context canceled")
Repeat loop detected → cancels chat_cancel (child ONLY)
    └─ the turn survives and ends normally; retry/stall logic takes over
```

---

## 16. การทดสอบ

```bash
cd jonnyq-rust
cargo test --release          # 242 tests (แนะนำ --release เพราะบาง test จับเวลา timeout)
cargo test                     # debug build
cargo test -- --nocapture      # ดู stdout
cargo test repl::              # รันเฉพาะ module ใด module หนึ่ง
cargo test --lib               # lib tests เท่านั้น
cargo test -- --test-threads=1 # ถ้า test ที่แก้ env var รบกวนกัน (test ที่ต้องใช้ env สะอาดล็อกผ่าน crate::testsupport::clean_env();
                               # test ที่ swap stdin stream ของ ask_user ล็อกผ่าน input_lock() เช่นกัน)
```

สิ่งที่ test ครอบคลุม (ไม่ต้องมี network หรือ model จริง):

- **config** — default, flag ชนะ .env/env var, parse `-thinking`, provider validation, unknown flag, case-insensitive `tool-mode`
- **dotenv** (`config::dotenv`) — comment/inline comment, `export`, single/double quotes (+escape, multi-line), `$VAR`/`${VAR}`/`${VAR:-x}`/`$$` (key ในไฟล์ชนะ env var ตอน expand), key ซ้ำ, CRLF, `~` ใน path, บรรทัดพัง → warning, quote ปิดไม่ครบ → error
- **config precedence / provenance** — `flag > file > env var > default` ทุกชั้น (รวมเคส `KEY=` ว่างในไฟล์ที่ชนะ env var และ env var ที่ยังใช้ได้กับ key ที่ไฟล์ไม่กำหนด), `-env-file off|none|no`, เลือกไฟล์ผ่าน `JONNYQ_ENV_FILE`, path ที่ไม่มี → error, `Source` ที่ `/env list` แสดง (mask `JONNYQ_KEY`), snapshot ตาราง flag/env ที่ตรงกับเอกสาร
- **repl `/env`** — report/list/reload (model, context, thinking, tool-mode, run_command timeout, max tool calls, skill path, magic word), file ที่หาย→คงของเดิม, `off`→ยกเลิกค่าจากไฟล์, flag ตอน start ชนะหลัง reload
- **tools** — `Registry::configure` reach ทุก tool, `ask_user` ถูก `install_always` เข้า registry ที่ว่างเปล่าและ dispatch ได้จริง, `ResizeableLimiter` cap concurrency + grow แล้วปล่อย waiter ทันที + cancel ได้
- **providers** — `TestServer` จำลอง `/api/chat`, `/chat/completions`, `/api/tags`, `/models`; ยืนยัน path/body ที่ส่ง, client-side timing, cancellation
- **coding** — generate/reconcile `.progress`, stall/`MAX_NO_PROGRESS_ROUNDS`, ntfy payloads (ผ่าน stub server), phase selection
- **tools** — run_command timeout + ฆ่า backgrounded grandchild, denylist, path guard
- **`ask_user`** — parse คำตอบ ntfy (prefix / no prefix / ข้อความว่าง), truncation บน char boundary (ไทย 3 bytes), ตั้งค่า prompt เป็นสีฟ้า/คำตอบสีขาวสว่าง (ตรวจ escape sequence ผ่าน `ui::styled` เพราะ log strip ANSI), คำตอบที่พิมพ์จริงผ่าน stdin stream (trim, EOF = empty answer), ตอบผ่าน topic (stub server แบบ chunked: push คำถามก่อน → subscribe `?since=…` → รับ `answer: …`), **ไม่รับคำถามตัวเองเป็นคำตอบ**, คำตอบก่อนชนะทั้งสองทิศทาง, ntfy ตาย → fallback ไป terminal พร้อม warning, ntfy เป็น channel เดียวที่ตาย → error พร้อมเหตุผล, Ctrl-C ระหว่างรอ ntfy
- **notify subscription** — `stream_messages` อ่าน event จาก stream ที่ยังเปิดค้างได้ (ไม่ถูก request timeout ฆ่า), `?since=` ถูกส่งไปจริง, cancel แล้ว reader หยุด, ไม่มี topic → error
- **watch** — fires once per line change, skip dot-dirs/`node_modules`/`vendor`/own log, live magic-word update
- **ui** — section headers, estimated/exact ctx readouts, `format_usage` output

```bash
cargo clippy --all-targets     # lint
cargo fmt                      # format
```

---

## 17. ความปลอดภัย (โดยตั้งใจ)

- **path confinement** — `read_file`/`write_file`/`edit_file`/`create_folder`/`read_pdf`/`read_pic` ทำงานได้เฉพาะใน working directory: `resolve_path` walk ทีละ component พร้อม resolve symlink แต่ละขั้น จึงกันทั้ง `../` traversal และ symlink ที่วางไว้ใน working dir เพื่อ escape
- **`run_command` ไม่มี confirmation prompt** (by design) แต่ตรวจ denylist **20 patterns** ก่อนรัน: `rm -rf /` (~, `$HOME`, `--no-preserve-root`), fork bomb, `mkfs`, `fdisk`, `parted`, `wipefs`, `dd of=/dev/...`, เขียนตรงลง `/dev/sdX`//`/dev/nvmeXnY`, `shutdown|reboot|poweroff|halt`, `init 0|6`, `systemctl poweroff`, `useradd|userdel|usermod|passwd|visudo`, เขียนทับ `/etc/passwd|shadow|sudoers`, `crontab -r`, `iptables -F`, `curl|wget ... | sh`, `chmod -R ... /`, `chown -R ... /`
  > ⚠️ **denylist เป็น best-effort ไม่ใช่ sandbox** — คำสั่งยังรันผ่าน `bash -c` ด้วยสิทธิ์เต็มของผู้เรียก ไม่ match = รันตามปกติ
- **`ask_user` เชื่อทุกคนที่รู้ topic** — ntfy ไม่มี concept ของการ "ตอบกลับเฉพาะคน" และไม่มี authentication: ข้อความใดก็ตามที่ถูก publish เข้า topic หลังจังหวะที่คำถามถูกถาม จะถูกถือเป็นคำตอบทันที จึงควรใช้ **topic ชื่อ random ยาว ๆ ที่คนอื่นคาดไม่ถึง** (ถ้ากังวลประเด็นนี้ อย่าตั้ง `-ntfy-url` — jonnyq จะรับคำตอบจาก keyboard อย่างเดียว) และคำถามที่ push ออกไปอาจถูก cache/แสดงบน server ของ ntfy — **อย่าใส่ secret ลงในข้อความ prompt**
- **resource caps** ป้องกัน context/หน่วยความจำระเบิด: `read_file` 2 MB · `web_fetch` 3 MB · `read_pic` 10 MB · `read_pdf` `-pdf-max-pages` · tool calls `-max-tool-calls-per-turn`
- **API key** ถูกส่งเฉพาะใน header `Authorization: Bearer` เมื่อค่าไม่ว่างเปล่า และ jonnyq ไม่พิมพ์ค่า key ออกจอ (ยืนยันด้วยคำว่า `key set` อย่างเดียว)
- **process isolation** — child ของ `run_command` เป็น process-group leader (`process_group(0)`); timeout/cancel → `kill(-pid, SIGKILL)` ทั้งกลุ่ม (กัน grandchild ที่ inherit stdout/stderr pipe ค้าง pipe ทำให้ hang เกิน timeout) พร้อม WaitDelay 5s เป็น backstop

---

## 18. ไฟล์ที่โปรแกรมสร้างระหว่างรัน

| ไฟล์/ไดเรกทอรี | เมื่อไร | หมายเหตุ |
|---|---|---|
| `output.txt` | เสมอ (`-output`) | transcript log แบบ append — ถูก ignore ไว้แล้ว; ย้ายไฟล์ได้ระหว่างรันถ้า `JONNYQ_OUTPUT_FILE` เปลี่ยนแล้ว `/env reload` |
| `.env` (ถ้าคุณสร้าง) | อ่านตอน start และทุกครั้งที่ `/env reload` | settings ของ project — **ถูก ignore ไว้แล้ว** (กัน key หลุด) ใช้ `.env.example` เป็นจุดเริ่มต้น |
| `.progress`, `.progress.hash`, `.progress.retry` | `/plan`, `/coding`, `/coding_phase`, `/autocoding` | `.progress` **ไม่ถูก ignore** โดยตั้งใจ (เป็นเอกสารงานที่ควร commit) ส่วน `.progress.hash`/`.progress.retry` เป็น state เครื่อง local — ถูก ignore ไว้แล้ว |
| `.jonnyq/pdf_pages/<stem>/page-*.png` | `read_pdf` เจอ PDF ที่ extract text ไม่ได้ | cache — ถูก ignore ไว้แล้ว; ลบทิ้งได้เสมอ |

repo นี้มี **`.gitignore` ใช้งานจริงอยู่ที่ root ของ crate** แล้ว — ครอบคลุม `/target/`, `/jonnyq`, `/jonnyq.exe`, `/output.txt`, `/.progress.hash`, `/.progress.retry`, `/.jonnyq/` และ editor/OS cruft; ทุก pattern **anchor ที่ root** (นำหน้าด้วย `/`) จึงไม่พลาดไป ignore ไฟล์ชื่อเดียวกันใน `src/` หรือ `tests/fixtures/` — ดูหมายเหตุอธิบายแต่ละบรรทัดในไฟล์

repo ยังมี **`.env.example`** เป็น template ของ `JONNYQ_*` ทุกตัวแปรพร้อม default (`.gitignore` กัน `.env` / `.env.*` / `.envrc` ไว้แต่ unignore `.env.example` และ `.env.template` ให้ commit ได้ — กัน API key จาก `-key`/`JONNYQ_KEY` หลุดเข้า git) — ต่างจากเวอร์ชันก่อน: **jonnyq อ่านไฟล์ `.env` ให้เองแล้ว** จึงไม่ต้อง `source`/`export` อีก (`.env` ที่หายไปก็ไม่ error — แค่ไม่มี settings จากชั้นนั้น)

---

## 19. Troubleshooting

| อาการ | สาเหตุที่เป็นไปได้ / วิธีแก้ |
|---|---|
| `config error: invalid provider "..."` | `-provider` ต้องเป็น `name:url` และ `name` ต้องเป็น `ollama` หรือ `openai` เท่านั้น |
| `config error: flag provided but not defined: -xxx` | สะกด flag ผิด / ใช้ flag ที่มีในเวอร์ชัน Go แล้วแต่ยังไม่ได้ port — ดู `-h` |
| `-h` ออกด้วย exit code 1 | **เป็นพฤติกรรมที่ตั้งใจให้เหมือน Go binary** — usage ถูกพิมพ์ออกมาก่อนเสมอ |
| แก้ `.env` แล้วค่าไม่เปลี่ยน | มี flag ที่ตรงกันตั้งอยู่ (flag ชนะไฟล์ — `/env list` จะขึ้น source ว่า `flag`) หรือแก้ env var แทนที่จะแก้ไฟล์, หรือ key ยังถูก comment/สะกดผิด/ตั้งซ้ำในไฟล์ (key สุดท้ายชนะ) — ดูคอลัมน์ source ด้วย `/env list`; ถ้าจะเอา flag ออกจริงต้อง start ใหม่ (flag ถูก replay ทุก reload) |
| แก้ env var (`export JONNYQ_...`) แล้วค่าไม่เปลี่ยน | **ชั้นไฟล์ชนะ env var** — ถ้า key เดียวกันมีอยู่ใน `.env` ด้วย ค่าจากไฟล์คือค่าที่ได้; comment key ในไฟล์ออก หรือ unexport แล้ว reload; `/env list` บอก source ได้ทันที |
| `config error: env file "..." does not exist` | path หลัง `-env-file`/`JONNYQ_ENV_FILE` ผิด — path ที่ระบุเองถือว่าตั้งใจใช้ จึง error; ถ้าอยากให้มีไฟล์เมื่อไหร่ก็ใช้ตอนนั้น ปล่อยว่าง (default) แล้ว jonnyq จะข้าม `.env` ที่ไม่มีให้เงียบ ๆ หรือ `-env-file off` เพื่อปิด |
| `warning: .env:3: ignoring line without '='` | บรรทัดในไฟล์ตกหล่น (เช่น ลืม `=` หรือ continuation) — ค่าอื่นในไฟล์ยังถูกใช้งานตามปกติ |
| `warning: <file> defines no JONNYQ_* settings` | ไฟล์ที่ระบุไม่มี key ของ jonnyq เลย (มักสะกดผิด เช่น `MODEL=` ต้องเป็น `JONNYQ_MODEL=`) |
| ค่าในไฟล์ถูก expand เพี้ยน | `$VAR` ถูก expand เสมอใน unquoted/double-quoted value — ใช้ `'single quotes'` ให้เป็น literal หรือ `$$` สำหรับ `$` หนึ่งตัว |
| `connection refused` | LLM server ยังไม่ได้รัน หรือ host/port ใน `-provider`/`/provider`ผิด — `/list_model` ช่วยตรวจได้ |
| `unexpected status 404` / `model not found` | ชื่อ model ผิดหรือไม่ถูก pull — เช็คด้วย `/list_model` |
| `unexpected status 400` / `context length` | บทสนทนาเกิน context ของ model — `/context` ให้เล็กลง หรือเริ่ม session ใหม่ |
| `unexpected status 401/403` | API key หาย/ผิด/หมดอายุ — ตั้งด้วย `/key` หรือ `-key` |
| `stream ended without a completion event` | connection หลุดกลางทาง — server อาจ crash/OOM (context ใหญ่เกิน RAM) หรือ proxy ตัด connection |
| `tool call budget exceeded for this turn (limit 50)` | model วนเรียก tool — เพิ่ม `-max-tool-calls-per-turn`, แตก task ให้เล็กลง หรือ `/toolmode prompt` |
| `model did not create .progress` | model ตอบเป็น text แทนการเรียก `write_file` หรือ arguments ผิดรูปแบบ — ลองอีกครั้ง, `/toolmode prompt`, หรือเปลี่ยน model ที่เก่งกว่า |
| `.progress not found` | รัน `/plan` หรือ `/plan_for_human` ก่อน `/coding`/`/coding_phase` |
| `Task "..." made no progress after 5 attempts` | model อ้างสำเร็จโดยไม่แก้ไฟล์ / build-test ไม่ผ่าน — เปิด `.progress` + `requirements.md` มาแก้หรือแตก task เอง |
| `web_search is disabled: no searxng url configured` | ต้องตั้ง `-searxng-url` / `JONNYQ_SEARXNG_URL` ก่อน |
| `read_pdf requires poppler-utils (pdftotext not found on PATH)` | ติดตั้ง poppler-utils (ดูหัวข้อ 2) |
| `path "..." is outside the working directory` | design — รัน jonnyq จากโฟลเดอร์ project ที่ถูกต้อง (หรือย้ายไฟล์) |
| `refused to run: command matches denylisted pattern: ...` | denylist กันคำสั่งทำลายล้าง (หัวข้อ 17) |
| thinking ไม่หายแม้ตั้ง `/think false` | provider `openai` ไม่รับ flag นี้ — jonnyq เพียงแค่ stream `reasoning_content` ถ้า server ส่งมา; ปิดที่ฝั่ง server |
| สีไม่ออกใน pipe/redirect | ทำตามมาตรฐาน — `Writer` ตรวจ `stdout().is_terminal()` ก่อนส่ง ANSI; log ไฟล์ strip ANSI เสมอ |
| `/watchfile` ไม่ fire | ข้าม dotfile/`.git`/`node_modules`/`vendor`/output log; ตรวจ magic word ตรงกัน, ตั้ง `-watch-poll-interval-sec`, บรรทัดเดิมนั้นจะไม่ fire ซ้ำถ้าเนื้อหาไม่เปลี่ยน |
| notification ไม่มา | `/ntfy` ตั้ง topic URL หรือยัง (ค่าว่าง = ปิด), topic มี subscriber หรือเปล่า, ข้อความเกิน ~4 KB (ntfy limit) |
| `ask_user has no way to reach the user` | stdin ไม่ใช่ terminal (`jonnyq < prompts.txt`, CI, รันผ่าน scheduler) และไม่มี ntfy topic — ตั้ง topic ด้วย `/ntfy <url>` หรือรันจาก terminalจริง |
| `ask_user could not reach the user over ntfy (...)` | topic unreachable (URL ผิด / server ล่ม / network) ขณะที่ **ไม่มี terminal ให้พิมพ์** — ดู path หลัง error, ลอง `/ntfy <url>` ใหม่ หรือไปตอบที่ terminal (ถ้านtfy ยังเปิดอยู่แต่แค่ subscribe ไม่สำเร็จ jonnyq จะ warning แล้วรอพิมพ์ที่ terminal ต่อ) |
| ตอบ ntfy แล้ว jonnyq ไม่ขยับ | ต้องตอบเข้า **topic เดียวกับที่ jonnyq ตั้งไว้** (topic สาธารณะชนกับคนอื่นได้ง่าย — ตั้ง topic สุ่มเป็นของคุณเอง), ตรวจว่าข้อความไม่ได้ว่างเปล่า (`keepalive`/`clear` ไม่ใช่คำตอบ), และข้อความที่มี title เป็นคำถามของ jonnyq เองจะถูกข้าม |
| `Your answer> ` ของ `ask_user` รับ input ไม่ได้ | มี test/โปรแกรมอื่นถือ stdin อยู่ (jonnyq มีจุดอ่าน input เดียว) หรือ stdin ถูก redirect — กด Ctrl-D เพื่อตอบ "ไม่มีคำตอบ" แล้ว model จะไปต่อ |

---

## 20. ความแตกต่างจากเวอร์ชัน Go

Repo นี้เป็น **Rust port** ของ `jonnyq` (Go) — โครงสร้าง module mirror `internal/` ของ Go ทุก package; ดู `jonnyq/README.md` สำหรับต้นฉบับ

| หัวข้อ | Go | Rust (repo นี้) |
|---|---|---|
| async / concurrency | goroutines + unbuffered channel | `tokio` tasks + `mpsc` + `tokio::select!` |
| cancellation | `context.Context` | `tokio_util::sync::CancellationToken` (child token ต่อ request) |
| CLI parsing | `flag` package | `FlagParser` ใน `config.rs` ที่ port พฤติกรรมมาแบบ 1:1 (bool flag inline-only, หยุดที่ positional, exit 1 เมื่อ `-h`) |
| HTTP | `net/http` stdlib | `reqwest` + rustls |
| tests | `httptest.NewServer` | `src/testsupport.rs` — HTTP/1.1 server บน tokio (no extra deps) |
| fs events | ไม่ใช้ (stdlib poll) | ไม่ใช้ (walk + mtime poll ด้วย `std::fs`) — เหมือนกัน |
| การตั้งค่า | flag + env var เท่านั้น | เหมือนเดิม **บวก** ชั้น dotenv (`.env` โหลดอัตโนมัติ, `-env-file`, `/env list`, `/env reload`) ซึ่งแทรกขึ้น**เหนือ** env var → `flag > .env > env var > default`; parser เขียนเองใน `config/dotenv.rs` ไม่เพิ่ม dependency |
| คุยกับผู้ใช้ | ไม่มี — model ตอบแล้วจบรอบเท่านั้น | **`ask_user`** (default tool): ตั้งคำถามสีฟ้า รอคำตอบสีขาวสว่าง และ two-way ผ่าน ntfy (push คำถาม + subscribe คำตอบกลับ) — ส่วน ntfy ของ Go เป็นขาออกอย่างเดียว |
| ค่าที่ปรับได้ระหว่างรัน | `/model` `/context` `/think` `/toolmode` `/ntfy` `/run_command_timeout` | เพิ่มอีกชุดผ่าน `/env reload`: `max-tool-calls-per-turn`, `skill-path`, `pdf-*`, `searxng-*`, `fetch-*`, `output` (ย้าย log), `watch-*` — ทำได้เพราะ tunables ของ agent/tools/watcher ย้ายไปอยู่ใน atomic/lock |
| binary | `CGO_ENABLED=0`, 1 file | `strip = true`, ~7 MB, no cgo (rustls) |

---

## License

ยังไม่ระบุใน repo (`Cargo.toml` ไม่มี field `license`) — ติดต่อกับผู้ดูแลโปรเจกต์ก่อนนำไปเผยแพร่
