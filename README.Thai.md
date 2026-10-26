# jonnyq

**Interactive coding agent CLI** — REPL ที่เขียนด้วย Rust ซึ่งคุยกับ LLM (เครื่องตัวเองหรือ remote) ให้ model เรียก tool (อ่าน/เขียนไฟล์, รันคำสั่ง, ค้นเว็บ) วนซ้ำจนงานเสร็จ และ automate การสร้างโปรเจกต์ทั้งชิ้นจากไฟล์ `requirements.md` ได้

ภาษา: [English](README.md) · **ไทย**

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

1. [แอปนี้คืออะไร](#แอปนี้คืออะไร)
2. [หลักการการทำงาน](#หลักการการทำงาน)
3. [ความต้องการของระบบ](#ความต้องการของระบบ)
4. [การติดตั้ง](#การติดตั้ง)
5. [เริ่มต้นใช้งาน](#เริ่มต้นใช้งาน)
6. [การตั้งค่า](#การตั้งค่า)
7. [Slash commands](#slash-commands)
8. [Tools](#tools)
9. [การทำงานอัตโนมัติ: `/plan` → `.progress` → `/coding`](#การทำงานอัตโนมัติ-plan--progress--coding)
10. [Skills](#skills)
11. [เวบคอนโซล](#เวบคอนโซล)
12. [การแจ้งเตือน (ntfy)](#การแจ้งเตือน-ntfy)
13. [ไฟล์ที่โปรแกรมสร้างระหว่างรัน](#ไฟล์ที่โปรแกรมสร้างระหว่างรัน)
14. [ความปลอดภัย](#ความปลอดภัย)
15. [License](#license)

---

## แอปนี้คืออะไร

- Agent บน terminal แบบโต้ตอบ: พิมพ์คำสั่ง → model คิด → เรียก tool → ตอบ โดยวนซ้ำจนงานเสร็จ
- รองรับ **Ollama** (native API) และ endpoint ที่เป็น **OpenAI-compatible** (OpenAI, LM Studio, llama.cpp server, vLLM, OpenRouter, …)
- automate งานเขียนโค้ดด้วยคำสั่ง `/plan`, `/plan_for_human`, `/coding`, `/coding_phase`, `/autocoding`
- รันบน [`tokio`](https://tokio.rs/) เป็น pure Rust ไม่ต้องมี C toolchain

## หลักการการทำงาน

**agent loop** — ทุก turn จะส่งบทสนทนา + tool spec ไปให้ provider → stream thinking/answer ออกจอ → รัน tool ที่ model เรียก → ส่งผลลัพธ์กลับ → วนซ้ำจนได้คำตอบสุดท้าย

**context compaction** — เมื่อ request โตเกิน ~60% ของ context window จะสรุปช่วงเก่าของบทสนทนาเป็น summary แล้วเก็บข้อความท้ายสุดไว้ทั้งดุ้น (default 6 ข้อความ) เพื่อให้ agent ยังเห็น file path / error / output ที่กำลังทำงานอยู่จริง การตรวจจะเกิดต้นทุก turn, หลังทุกรอบ tool call และท้าย turn summary ที่ใช้ไม่ได้ (ว่าง, มีแต่ thinking, ตัดห้วน) จะ**ไม่มีทาง**แทนที่ history — jonnyq จะคงของเดิมไว้พร้อมพิมพ์เหตุผลออกจอ

**2 โหมด tool**

| โหมด | พฤติกรรม |
|---|---|
| `native` (default) | ส่ง tool spec ตาม protocol ของ provider และรับ tool calls แบบ structured ผล tool ถูกส่งกลับด้วย `role: "tool"` |
| `prompt` | ไม่ส่ง tool spec เลย — ยัดรายชื่อ tool + JSON schema เข้า system prompt แล้วให้ model ตอบเป็น fenced block ` ```tool ``` ` ใช้เมื่อ function calling ของ backend ไม่เสถียร |

**ตัวกันเกินขอบเขต** — ผล tool ถูก cap ก่อนเข้า history (default 64,000 ตัวอักษร เก็บ head 70% + tail 30%), จำกัด tool call ต่อ turn (default 50), มีตัวตรวจการพิมพ์วนซ้ำเพื่อยกเลิกรอบที่ติด loop และ workflow การเขียนโค้ดจะหยุดเมื่อ task เดิมค้างครบ 5 รอบ หรือ 15 รอบติดที่ไม่มี task ไหนถูกติ๊ก

**การแสดงผล** — ทุก section มี timestamp, สถานะ context และสถิติของ request กำกับ และถูก mirror ลง transcript log แบบข้อความล้วน (default `output.txt`)

## ความต้องการของระบบ

| รายการ | จำเป็น? | หมายเหตุ |
|---|---|---|
| Rust toolchain | ตอน build | ต้องมี `cargo`; พัฒนา/ทดสอบบน rustc 1.99 (แนะนำ 1.85 ขึ้นไป) |
| LLM server | ตอนใช้งาน | [Ollama](https://ollama.com) หรือ server แบบ OpenAI-compatible |
| poppler-utils | เฉพาะ `read_pdf` | `pdftotext` + `pdftoppm` ต้องอยู่บน `PATH` |
| bash | เฉพาะ `run_command` | รันคำสั่งผ่าน `bash -c` (Windows ใช้ WSL/Git Bash) |
| searxng instance | ไม่บังคับ | ถ้าไม่ตั้ง URL จะปิด `web_search` อัตโนมัติ |
| C compiler | ไม่จำเป็น | TLS ใช้ rustls (pure Rust) |

```bash
sudo apt install poppler-utils     # Debian/Ubuntu
sudo dnf install poppler-utils     # Fedora
brew install poppler               # macOS
```

## การติดตั้ง

```bash
cd jonnyq-rust
cargo build --release
./target/release/jonnyq -h
```

รันโดยไม่ต้อง build เอง หรือติดตั้งเข้า `PATH`:

```bash
cargo run --release -- -model qwen3:14b
cargo install --path .                     # → ~/.cargo/bin/jonnyq
cp target/release/jonnyq ~/.local/bin/     # หรือคัดลอก binary เอง
```

`[profile.release]` ตั้ง `strip = true` release binary จึงมีขนาดประมาณ **7 MB**

cross-compile ได้ทันที (ไม่มี C binding) ขอแค่มี linker ของแพลตฟอร์มปลายทาง หรือใช้ [`cross`](https://github.com/cross-rs/cross):

```bash
rustup target add x86_64-unknown-linux-musl aarch64-apple-darwin x86_64-pc-windows-gnu
cargo build --release --target aarch64-apple-darwin
cross build --release --target aarch64-unknown-linux-gnu
```

ผลลัพธ์อยู่ที่ `target/<triple>/release/jonnyq`

## เริ่มต้นใช้งาน

```bash
# 1) เปิด model server
ollama pull qwen3:14b && ollama serve

# 2) รัน jonnyq จากโฟลเดอร์ project
cd ~/dev/my-project
jonnyq -model qwen3:14b -context 32768
```

หรือเก็บการตั้งค่าไว้ในไฟล์:

```bash
cp .env.example .env     # template ที่มีทุก key พร้อมคำอธิบาย
$EDITOR .env             # JONNYQ_MODEL, JONNYQ_CONTEXT_SIZE, ...
jonnyq                   # เทียบเท่า: jonnyq -model ... -context ...
```

ฝั่ง OpenAI-compatible:

```bash
jonnyq -provider openai:http://localhost:1234/v1 -model qwen3-14b          # LM Studio
jonnyq -provider openai:https://api.openai.com/v1 -key sk-... -model gpt-4o-mini
```

รูปแบบ provider string คือ `name:url` โดย `name` เป็น `ollama` หรือ `openai` เท่านั้น

ถ้ายังไม่ตั้ง model โปรแกรมจะรับเฉพาะ slash command:

```
> /list_model          # ดูรายชื่อ model ที่ provider มี
> /model qwen3:14b
> /context 32768
> ช่วยอธิบายโครงสร้างโปรเจกต์นี้
> /exit
```

**พิมพ์หลายบรรทัด** — จบบรรทัดด้วย `\` เพื่อต่อบรรทัด หรือพิมพ์ `"""` บรรทัดเดียวเพื่อเปิด/ปิดบล็อก ส่วน `/prompt <file>` ใช้ส่งเนื้อหาไฟล์เป็น prompt

**คีย์แก้ไข** — พรอมป์เป็น line-editor เต็มตัว: ↑/↓ เรียกคำสั่งเก่า (เก็บที่ `$HOME/.jonnyq_history` ใช้ร่วมกันข้าม session ได้), `Tab` autocomplete slash command / ค่าของอาร์กิวเมนต์ / ชื่อไฟล์, `Ctrl-A/E/W/U` ตามสไตล์ readline, `Ctrl-Z` suspend

**Ctrl-C ยกเลิกรอบปัจจุบัน** (agent turn, คำสั่งที่กำลังรัน, หรือการรอคำตอบของ `ask_user`) โดยไม่ปิดโปรแกรม — บนเวบคอนโซล (`-web`) ปุ่ม **Stop** ทำหน้าที่เดียวกับ Ctrl-C พอดี — `Ctrl-D` ตอนบรรทัดว่าง, `/exit` หรือ `/bye` ใช้ ออกจากโปรแกรม

**ตัวอย่าง workflow:**

```bash
$EDITOR requirements.md
jonnyq -model qwen3:14b -ntfy-url https://ntfy.sh/my-secret-topic
# > /plan_for_human     # สร้าง .progress พร้อมคู่มือตรวจทีละ phase สำหรับคน
# > /coding_phase       # ทำ 1 phase แล้วหยุด + ส่งวิธีตรวจ
# > /coding_phase       # phase ถัดไป
# > /autocoding         # หรือให้ทำรวดเดียวจบ
```

## การตั้งค่า

ลำดับความสำคัญ: **CLI flag > ไฟล์ `.env` > environment variable (`JONNYQ_*`) > ค่า default**

jonnyq อ่านไฟล์ `.env` ในโฟลเดอร์ที่รันให้อัตโนมัติตอน start (และเมื่อ `/env reload`) ชั้นไฟล์จะชนะ env var ที่ export ไว้ เพราะค่าในไฟล์คือการตั้งค่าที่ตั้งใจไว้สำหรับ project นั้น ส่วน env var เป็นค่าสำรองของ key ที่ไฟล์ไม่ได้กำหนด ชื่อ env var = ชื่อ flag พิมพ์ใหญ่เติม prefix `JONNYQ_` (`-context` → `JONNYQ_CONTEXT_SIZE`)

syntax ของไฟล์รองรับ comment, `export KEY=value`, single/double quotes (escape `\n`, `\t`), `$VAR` / `${VAR:-fallback}`, `$$` สำหรับ `$` หนึ่งตัว และ key ซ้ำ = ค่าสุดท้ายชนะ บรรทัดที่พังเป็นแค่ warning ส่วน quote ปิดไม่ครบเป็น error ตอนเริ่ม

| Flag | Env var / key ใน `.env` | Default | ความหมาย |
|---|---|---|---|
| `-provider` | `JONNYQ_PROVIDER` | `ollama:http://localhost:11434` | provider รูปแบบ `name:url` (`ollama` หรือ `openai`) |
| `-key` | `JONNYQ_KEY` | (ว่าง) | API key ส่งเป็น `Authorization: Bearer` |
| `-model` | `JONNYQ_MODEL` | (ว่าง — **ต้องตั้ง**) | ชื่อ model |
| `-context` | `JONNYQ_CONTEXT_SIZE` | `16348` | ขนาด context window (Ollama รับต่อเป็น `num_ctx`) |
| `-output` | `JONNYQ_OUTPUT_FILE` | `output.txt` | ไฟล์ transcript log (append, ข้อความล้วน) |
| `-searxng-url` | `JONNYQ_SEARXNG_URL` | (ว่าง — ปิด `web_search`) | URL ของ searxng instance |
| `-searxng-max-results` | `JONNYQ_SEARXNG_MAX_RESULTS` | `10` | จำนวนผลลัพธ์สูงสุด |
| `-searxng-concurrency` | `JONNYQ_SEARXNG_CONCURRENCY` | `4` | จำนวนการค้นหาพร้อมกัน |
| `-searxng-timeout-sec` | `JONNYQ_SEARXNG_TIMEOUT_SEC` | `30` | timeout การค้นหา (วินาที) |
| `-fetch-concurrency` | `JONNYQ_FETCH_CONCURRENCY` | `5` | จำนวน `web_fetch` พร้อมกัน |
| `-fetch-timeout-sec` | `JONNYQ_FETCH_TIMEOUT_SEC` | `30` | timeout การโหลดเว็บ (วินาที) |
| `-pdf-max-pages` | `JONNYQ_PDF_MAX_PAGES` | `50` | จำนวนหน้า PDF สูงสุดที่ `read_pdf` อ่าน |
| `-pdf-dpi` | `JONNYQ_PDF_DPI` | `150` | DPI ตอน render หน้า PDF เป็น PNG |
| `-run-command-timeout-sec` | `JONNYQ_RUN_COMMAND_TIMEOUT_SEC` | `300` | timeout ของ `run_command` (วินาที) |
| `-history-size` | `JONNYQ_HISTORY_SIZE` | `200` | จำนวน prompt ที่จำไว้ให้ ↑/↓ |
| `-compact-headroom-percent` | `JONNYQ_COMPACT_HEADROOM_PERCENT` | `60` | % ของ window ที่จะ trigger compact |
| `-compact-target-percent` | `JONNYQ_COMPACT_TARGET_PERCENT` | `25` | % ของ window ที่ summary ใช้ได้ (เพดาน 1,000 tokens) |
| `-compact-keep-recent` | `JONNYQ_COMPACT_KEEP_RECENT` | `6` | จำนวนข้อความท้ายที่เก็บไว้ทั้งดุ้นหลัง compact |
| `-summary-transcript-cap` | `JONNYQ_SUMMARY_TRANSCRIPT_CAP` | `48000` | เพดาน transcript ที่ส่งไปขอสรุป |
| `-max-tool-result-chars` | `JONNYQ_MAX_TOOL_RESULT_CHARS` | `64000` | เพดานขนาดผล tool หนึ่งตัวก่อนเข้า history |
| `-max-tool-calls-per-turn` | `JONNYQ_MAX_TOOL_CALLS_PER_TURN` | `50` | budget tool call ต่อ turn |
| `-watch-magic-word` | `JONNYQ_WATCH_MAGIC_WORD` | `AI!` | magic word ของ `/watchfile` |
| `-watch-poll-interval-sec` | `JONNYQ_WATCH_POLL_INTERVAL_SEC` | `1` | ความถี่ poll ของ `/watchfile` (วินาที) |
| `-ntfy-url` | `JONNYQ_NTFY_URL` | (ว่าง — ไม่แจ้งเตือน) | ntfy topic URL เต็ม ๆ ใช้ทั้งส่ง notification และ**รับคำตอบ `ask_user`** |
| `-socks-proxy` | `JONNYQ_SOCKS_PROXY` | (ว่าง — ไปอ่าน `ALL_PROXY`/`HTTPS_PROXY`) | SOCKS proxy ของทุก request (model, web tools, ntfy): `socks5h://host:port`, `socks5`, `socks4a`, `socks4` หรือ `host:port`; `off` = เชื่อมตรง; `auto` = เชื่อ env |
| `-no-proxy` | `JONNYQ_NO_PROXY` | `localhost,127.0.0.1,::1` | host/CIDR ที่ข้าม proxy (คั่นด้วย `,`) |
| `-tool-mode` | `JONNYQ_TOOL_MODE` | `native` | `native` หรือ `prompt` |
| `-skill-path` | `JONNYQ_SKILL_PATH` | (ว่าง) | ไดเรกทอรีของ skill คั่นด้วย `;` |
| `-web` | `JONNYQ_WEB_PORT` | `0` (ไม่มีเวบ interface) | port ของเวบคอนโซลบน `http://127.0.0.1:<port>` |
| `-thinking=true\|false` | `JONNYQ_THINKING` | `true` | เปิด/ปิด thinking (ส่งเฉพาะ Ollama) |
| `-env-file` | `JONNYQ_ENV_FILE` | (ว่าง = `./.env`) | เลือกไฟล์ dotenv อื่น หรือ `off`/`none`/`no` เพื่อปิด — ตั้งผ่าน flag/env เท่านั้น ตั้งในไฟล์ไม่ได้ |

หมายเหตุ:

- ทุก flag รับ value ยกเว้น `-thinking` (รับแบบ inline `-thinking=false` เท่านั้น) และ `-h`/`-help` — การ parse หยุดที่ argument แรกที่ไม่ใช่ flag หรือที่ `--`; ตั้ง flag ซ้ำ = ค่าสุดท้ายชนะ; `-h` พิมพ์ usage แล้วออกด้วย exit code 1
- `-thinking` ถูกส่งเฉพาะ Ollama (`{"think": true|false}`) — ฝั่ง `openai` jonnyq แค่แสดงผล `reasoning_content` ถ้า server ส่งมา จึงต้องปิดที่ฝั่ง server
- credential ใน proxy URL ถูก mask เป็น `user:***` ในทุก output และ log
- `-skill-path` คั่นด้วย `;` เท่านั้น (ไม่ใช่ `:`) และต้อง escape เป็น `\;` หรือครอบ single quotes ใน shell

## Slash commands

พิมพ์ `/help` (หรือ `/?`) เพื่อดูรายการนี้ในโปรแกรม

| คำสั่ง | ความหมาย |
|---|---|
| `/?`, `/help` | แสดงคำสั่งทั้งหมด |
| `/provider <name>:<url>` | เปลี่ยน provider (มีผลทันที) |
| `/key <key>` | ตั้ง API key |
| `/model <model>` | ตั้ง model |
| `/list_model` | แสดงรายชื่อ model ที่ provider มี |
| `/context <size>` | ตั้ง context size |
| `/think <true\|false>` | เปิด/ปิด thinking |
| `/compact` | สรุป history ทันที ไม่ต้องรอให้ถึง trigger |
| `/history` | ดู context usage รายข้อความและยอดรวม |
| `/run_command_timeout <seconds>` | เปลี่ยน timeout ของ `run_command` ระหว่างรัน |
| `/prompt <file>` | ส่งเนื้อหาไฟล์เป็น prompt |
| `/plan` | สร้าง `.progress` จาก `requirements.md` หรือ reconcile เมื่อ requirement เปลี่ยน — ไม่เขียนโค้ด |
| `/plan_for_human` | เหมือน `/plan` แต่เขียน `> How to run:` / `> Human check:` เพิ่ม เพื่อให้คนตรวจงานจริงได้ทีละ phase |
| `/coding` | ทำ task ถัดไปที่ยังไม่เสร็จใน `.progress` 1 ข้อแล้วหยุด |
| `/coding_phase [<เลข>]` | ทำทุก task ของ 1 phase (ล้าง history ก่อนทุก task) แล้วหยุด; เช่น `/coding_phase 3` |
| `/autocoding` | เรียก `/plan` ถ้าจำเป็น แล้วทำทุก task ที่เหลือรวดเดียว |
| `/watchfile [<คำ>\|off]` | จับตาโฟลเดอร์หา magic word แล้วส่งบรรทัดนั้นเป็น prompt |
| `/ntfy [<topic url>\|off]` | ดู/ตั้ง/ปิด ntfy topic |
| `/proxy [<socks url>\|off\|auto]` | ดู/ตั้ง proxy ของ traffic ทั้งหมด มีผลตั้งแต่ request ถัดไป |
| `/proxy no-proxy <hosts>` | แก้เฉพาะ bypass list |
| `/toolmode [native\|prompt]` | ดู/สลับโหมด tool |
| `/env [path\|off]` | ดูว่า `.env` ไฟล์ไหนถูกใช้ หรือเปลี่ยน/ปิดไฟล์ |
| `/env list` | ตารางทุก setting: ค่าที่ใช้จริง + ที่มา (`flag`/`file`/`env`/`default`) |
| `/env reload [path\|off]` | อ่านไฟล์ `.env` ใหม่แล้ว apply เข้า session ที่กำลังรัน |

`/env reload` apply model, context, thinking, tool mode, ไฟล์ output, provider/key, proxy, timeout ของ searxng/fetch/pdf, tool-call budget, skill path, ntfy topic และ watch settings โดยไม่ต้อง restart — flag ที่ใส่ตอน start ถูก replay จึงยังชนะเสมอ

## Tools

| Tool | Parameters | หมายเหตุ |
|---|---|---|
| `read_file` | `path` | อ่านไฟล์ text ใน working dir เท่านั้น cap 2 MB |
| `write_file` | `path`, `content` | สร้าง/เขียนทับ สร้างโฟลเดอร์ parents ให้เอง |
| `edit_file` | `path`, `old_string`, `new_string`, `replace_all?` | แทนที่แบบ exact match — error เมื่อไม่พบหรือพบซ้ำโดยไม่ตั้ง `replace_all` |
| `create_folder` | `path` | สร้างไดเรกทอรีพร้อม parents |
| `run_command` | `command` | รัน `bash -c` ใน working dir รวม stdout+stderr, timeout ฆ่าทั้ง process group |
| `web_search` | `query` | ค้นผ่าน searxng JSON API — ปิดอยู่จนกว่าจะตั้ง URL |
| `web_fetch` | `url` | โหลด URL แล้ว strip HTML เหลือ text cap 3 MB |
| `read_pdf` | `path` | ใช้ `pdftotext`; PDF ที่ไม่มี text จะ render เป็น PNG ให้ `read_pic` ต่อ |
| `read_pic` | `path` | อ่าน image → base64 data URI cap 10 MB |
| `read_skill` | `name` | อ่านเนื้อหาเต็มของ skill ที่อยู่ใน system prompt |
| `ask_user` | `prompt` | ถามมนุษย์แล้วรอคำตอบ — บน terminal (คำถามสีฟ้า คำตอบขาวสว่าง), ใน popup บนเวบ (`-web`) และผ่าน notification ถ้าตั้ง ntfy โดยตอบด้วย `answer: <ข้อความ>` ใครตอบก่อนได้ก่อน และ channel ที่แพ้จะถูกปิดให้ |

ทุก path tool ถูก confine ให้อยู่ใน working directory (กันทั้ง `../` traversal และ symlink escape)

## การทำงานอัตโนมัติ: `/plan` → `.progress` → `/coding`

เขียน `requirements.md` เอง แล้ว:

| | `/plan` | `/coding` | `/coding_phase` | `/autocoding` |
|---|---|---|---|---|
| สร้าง `.progress` | ได้ | ไม่ได้ | ไม่ได้ | ได้ (เรียก `/plan`) |
| ขอบเขตที่ทำ | ไม่เขียนโค้ด | 1 task | 1 phase | ทุก task |
| ล้าง history ต่อ task | – | ไม่ได้ | ได้ | ได้ |
| หยุดเมื่อ | เขียน plan เสร็จ | จบ 1 task | จบ phase | ครบทุก task |

`.progress` คือ checklist แบบ GitHub checkbox จัดกลุ่มเป็น phase และถูก sync กับ `requirements.md` ด้วย SHA-256 ใน `.progress.hash` — ถ้า requirement เปลี่ยน jonnyq จะ **reconcile** plan เดิมแทนการ generate ใหม่ทั้งหมด:

```markdown
# Project Title

> How to run: `cargo run` → http://localhost:3000, login with demo/demo123

## Phase 1: Scaffolding & config

> Human check: รัน `cargo build` -> ต้องไม่มี error, เห็น binary ใน target/debug

- [ ] Init Cargo.toml with tokio + serde dependencies (REQ-001)
- [x] Implement the tool registry
```

ระหว่างเขียนโค้ด system prompt จะเข้าโหมด autonomous ซึ่งห้ามติ๊ก task ว่าเสร็จถ้าไม่ได้รัน build/test จริงในรอบนั้น และการตรวจความคืบหน้าดูจากเนื้อหาจริงในไฟล์ ทำให้การอ้างสำเร็จแบบไม่แก้ไฟล์ถูกจับได้และจบด้วย error

`/watchfile` คือ pair programming โดยไม่ต้องออกจาก editor — เริ่มด้วย `/watchfile` แล้วพิมพ์ magic word ลงในโค้ด:

```rust
// AI! เพิ่ม error handling กรณีหาไฟล์ไม่เจอ พร้อมเขียน test
let data = std::fs::read(path).unwrap();
```

watcher (poll mtime ไม่ใช้ fsnotify) จะจับบรรทัดนั้น ส่งเป็น prompt และ agent จะลบบรรทัด trigger ทิ้งเมื่อทำงานเสร็จ

## Skills

Skill คือความรู้ที่เขียนเป็น markdown ให้ model ดึงมาใช้เมื่อจำเป็น ระบบ discover skill ใหม่ทุก turn และสรุป (ชื่อ + บรรทัดแรก) เข้า system prompt model จะเรียก `read_skill` เมื่อต้องการเนื้อหาเต็ม

```
~/my-skills/
├── react/
│   └── SKILL.md        ← skill "react"
└── postgres.md         ← skill "postgres" (ไฟล์เดี่ยว)
```

```bash
jonnyq -skill-path '~/my-skills;~/shared-skills'
# หรือ JONNYQ_SKILL_PATH=$HOME/my-skills;$HOME/shared-skills   (ใน .env)
```

## เวบคอนโซล

```bash
jonnyq -model qwen3:14b -web 8080     # แล้วเปิด http://127.0.0.1:8080
```

`-web <port>` แสดง session เดียวกันบนเวบไปด้วยพร้อมกับ terminal — ทำงานได้ทั้งสอง
หน้าต่างพร้อมกัน และไม่มีทางทำให้รันสอง turn ซ้อนกัน หน้าเวบใช้ websocket เส้นเดียว
ไม่มี script หรือ bundler ของตัวเอง จึงทำงานกับ model ในเครื่องแบบ offline ได้
สิ่งเดียวที่เรียกจากภายนอกคือ stylesheet ของ Google Fonts ที่อยู่บน font picker สอง
ตัวบนแถบบน — ถ้าดึงไม่เจอก็จะ render ด้วย font เดียวที่ติดตั้งไว้ในเครื่อง หรือ generic
ตัวท้ายของ stack นั้น (sans-serif สำหรับข้อความธรรมดา, mono สำหรับ code) แทน

| ส่วน | ทำงานอะไร |
|---|---|
| **แถบบน** | `model · ctx … · mode` ของ session ซึ่งอัปเดตตาม turn ที่กำลังรัน พร้อม control เลือก font และขนาดตัวอักษรสองคู่ (ข้อความธรรมดา / code) ตามที่อธิบายด้านล่าง |
| **แท็บ console** (กลาง) | สะท้อนทุกไบต์ที่ jonnyq พิมพ์ออก stdout แบบสด ๆ และ render สีแบบ gruvbox dark — ข้อความเดียวกับ สีเดียวกับ terminal |
| **แท็บไฟล์** | เปิดได้หลายไฟล์ source ในแท็บถูก syntax highlight ด้วย palette gruvbox dark ชุดเดียวกับ console — คลิกแท็บเพื่อสลับดู console หรือไฟล์ไหนก็ได้ และหัวของทุกแท็บมีปุ่ม **Download** เพื่อบันทึกไฟล์นั้นลงเครื่อง |
| **ช่องพิมพ์ prompt** (ใต้ console) | พิมพ์ prompt หรือ `/command` — Enter ส่ง, Shift-Enter ขึ้นบรรทัดใหม่ ปรับขนาดได้โดยลากแถบด้านบนหรือมุมของช่อง จะถูก disable เมื่อ session รับ prompt ไม่ได้ (ยังไม่มี model, มี turn กำลังรัน, หรือกำลัง exit) พร้อมแสดงสาเหตุไว้ข้าง ๆ |
| **ปุ่ม Stop** (ข้างปุ่ม Send) | Ctrl-C ของเวบ: ยกเลิกรอบที่กำลังรัน และจะเปิดใช้เฉพาะตอนที่มีรอบให้หยุดเท่านั้น — เป็น token ตัวเดียวกับที่ Ctrl-C บน terminal กด จึงตัด stream ของ model, ฆ่า process group ของคำสั่งที่กำลังรัน, ถอนคำถาม `ask_user` ที่ค้างอยู่ และจบรอบด้วย `[cancelled]` โดย session ยังอยู่ต่อ popup มี Stop ของตัวเองด้วย เพราะกล่องคำถามบังช่องพิมพ์ prompt อยู่ |
| **popup ของ `ask_user`** | เมื่อ model เรียก `ask_user` คำถามจะเด้งขึ้นมาทับหน้าเวบ พร้อมช่องพิมพ์คำตอบและปุ่ม **Send answer** — ดูด้านล่าง |
| **tree** (ขวา) | รายการ working directory (folder ก่อนไฟล์) **ที่อัปเดตตัวเองได้** — ไฟล์ที่ถูกสร้าง/ลบ/เปลี่ยนชื่อ จะโผล่ใน panel เองโดยไม่ต้องคลิกอะไร และ folder ที่คุณขยายไว้ก็จะคงขยายอยู่เหมือนเดิม ขยาย folder เพื่อดูรายการข้างใน คลิกไฟล์ text เพื่อเปิดในแท็บใหม่ ส่วน PDF/image/binary จะขึ้น popup บอกว่าเปิดดูไม่ได้เพราะอะไร |

**font และ size** แถบบนมี control สองคู่ เพราะหน้าเวบมีตัวอักษรสองชนิด `text` เลือก
font ของ console, ช่องพิมพ์ prompt และแถบบน จาก font ไทยบน Google Fonts 9 แบบ
(default คือ IBM Plex Sans Thai Looped) และช่องตัวเลขข้างกันตั้งขนาดตัวอักษรเป็น pt
(8–40, default **14pt**) ส่วน `code` เลือก font ของ terminal pane และแท็บไฟล์ จาก
font mono บน Google Fonts 18 แบบ — Source Code Pro, Roboto Mono, JetBrains Mono,
IBM Plex Mono (default), Space Mono, Fira Code, Google Sans Code, Ubuntu Mono,
Ubuntu Sans Mono, Anonymous Pro, Nova Mono, Syne Mono, Iosevka Charon Mono, Sono,
Intel One Mono, SUSE Mono, Cascadia Mono และ Libertinus Mono — และช่องตัวเลขของมัน
ตั้งขนาดของส่วนนั้น (8–40, default **12pt**) ทั้งสองรายชื่อแยกกันโดยตั้งใจ: terminal
pane และแท็บไฟล์คือ grid ของ cell ซึ่ง font แบบ proportional วางให้ตรงกันไม่ได้
ทั้งสี่ค่าถูกจำไว้ต่อบราวเซอร์ จึง reload หรือเปิดแท็บใหม่แล้วได้หน้าตาเดิม กด `reset`
เพื่อกลับมาเป็น IBM Plex Sans Thai Looped 14pt และ IBM Plex Mono 12pt การเลือก font
ไม่ใช่ operation ที่ต้องต่อเน็ต: มันคือชื่อ CSS family หนึ่งชื่อ ซึ่งจะ resolve เป็น
font ที่ดาวน์โหลดมา, font เดียวที่ติดตั้งไว้ในเครื่อง, หรือ generic ตัวท้ายของ stack
นั้น — ซึ่งเป็น mono สำหรับ code ดังนั้นเครื่องที่ไม่เคยดึง stylesheet มาได้เลยก็ยังคง
มี column และ cell ที่ตรงกัน

**แถว status เป็นของสด** ตัวเลข `ctx` บนแถบบนคือ context ที่ session ใช้อยู่ตอนนี้
ไม่ใช่ค่าที่ snapshot ตอน connect: มันถูกอ่านใหม่ทุก 2 วินาที, ตอน start และ end ของ
ทุก turn, และทุกครั้งที่ slash command เปลี่ยน model หรือ window — และจะส่ง frame ก็ต่อ
เมื่อข้อความเป็นค่าใหม่จริงเท่านั้น

**tree อัปเดตตัวเองได้ ไม่มีปุ่ม refresh แล้ว** panel จะ list working directory ใหม่
ทุก 2.5 วินาที พร้อม folder ที่คุณขยายไว้ทุกตัว (list เฉพาะ folder ที่แสดงอยู่บนจอ —
folder ที่หุบไว้ไม่มีอะไรรอให้อัปเดต และจะถูก list เมื่อคุณขยายมัน) และ list ทันที
เมื่อกลับเข้ามาที่ tab หลังซ่อนไว้ ถ้า listing ไม่มีอะไรเปลี่ยนจะไม่วาดใหม่เลย:
scroll position และ folder ที่ขยายไว้จึงอยู่ครบหลังทุก tick จะวาดใหม่ก็ต่อเมื่อ
folder นั้นมี entry เพิ่ม/หาย/เปลี่ยนชื่อจริง ๆ (ขนาดไฟล์ไม่นับ เพราะไม่มี row ไหน
แสดงขนาด) folder ที่ถูกลบไปจาก disk จะหุบตัวเอง ไม่ทิ้งแถวค้างไว้; folder ที่อ่านไม่ได้
จะขึ้นข้อความ *cannot list: …* ไว้ใน folder นั้นแทน popup (เพราะ panel พยายามเรื่อย ๆ
popup ทุก tick คือความรำคาญไม่ใช่ข้อมูล) โดยมีแค่กรณี working directory อ่านไม่ได้อย่างเดียว
ที่ขึ้น popup และจะขึ้นแค่ครั้งเดียวต่อสาเหตุ ไม่ใช่ทุกครั้งที่ต้อง poll ส่วน tab ที่ซ่อนอยู่
หรือตอน offline จะไม่ poll เลย จึงไม่เปลืองเมื่อปิดฝา notebook และจะดึงรายการต่อเองเมื่อ connect ใหม่

**แท็บไฟล์มี syntax highlight** เปิดไฟล์จาก tree แล้ว source จะถูกวาดด้วย palette
gruvbox dark ชุดเดียวกับ console — comment มืดลงและเอียง, keyword แดง, type เหลือง,
string เขียว, ส่วน key (ชื่อทางซ้ายของ `:` หรือ `=`) และ literal ก็มีสีของตัวเอง ภาษาดูจาก
extension ของไฟล์, ดูจากชื่อไฟล์ในกรณีที่ extension บอกอะไรไม่ได้ (`Dockerfile`,
`Makefile`, `Cargo.lock`, `.env`) และถ้ายังไม่ได้อีกก็ดูจากบรรทัด `#!` — script ที่ไม่มี
extension จึงยังถูกรู้จักผ่าน interpreter ที่มันเรียก ครอบคลุม Rust, TOML, Python,
JavaScript, TypeScript, JSON, YAML, Markdown, shell, Dockerfile, HTML, CSS, C/C++, Go,
Java, Ruby, PHP, Lua, Zig, Perl, SQL และ config แบบ INI ส่วนไฟล์อื่นจะแสดงแบบไร้สีไปเลย
ไม่เดา เพราะการแต้มสีไฟล์ด้วยกฎที่ผิดคือเหตุผลที่ทำให้คนไม่เชื่อสีที่ถูกต้อง

สิ่งที่ใช้เป็น scanner ไม่ใช่ parser และถูกเขียนฝังไว้ในหน้าเวบ ไม่ได้ดึงจาก CDN —
เหตุผลเดียวกับที่ส่วนอื่นของหน้าเวบไม่มี dependency: สีต้องอยู่ได้บนเครื่องที่ offline
สามข้อที่มันไม่ทิ้งแน่นอน: ข้อความที่ pane แสดงคือตัวของไฟล์เอง token ต่อกันตามลำดับ
จึง block selection แล้ว copy จากแท็บไฟล์ก็ยังได้ไฟล์ครบทุกตัวอักษร; ทุกส่วนถูก escape
ก่อนถูกห่อด้วย span ดังนั้นไฟล์ที่มี `<script>` อยู่ข้างในก็เป็นแค่ข้อความเสมอ; และ
string/comment จะปิดที่ marker ปิด, ที่ปลายบรรทัด หรือที่ปลายไฟล์ แล้วแต่อย่างไหนมาก่อน
— apostrophe หนึ่งตัวในประโยคข้อความจึงไม่ทาสียกย่อหน้าข้างล่างทั้งย่อหน้า fenced code
block ในไฟล์ Markdown จะถูกวาดเป็นก้อนข้อความที่พื้นเข้มกว่ารอบข้างแทนที่จะหยิบกฎของภาษาที่
ระบุไว้หลัง fence มาใช้ซ้ำ เพราะ fence คือส่วนของ document ไม่ใช่หน้าต่างไปยังไฟล์อื่น
ไฟล์ใหญ่เกิน
400 KB จะแสดงแบบไร้สี เพราะขนาดนั้นคือไฟล์ที่คนกำลัง search ไม่ใช่กำลังอ่าน และการทำ
span ต่อ token จะ freeze tab ไปหนึ่งวินาทีโดยเปล่าประโยชน์ header ของทุกแท็บไฟล์จะบอก
ด้วยว่าอ่านไฟล์นั้นเป็นภาษาอะไร จึงไม่ต้องเดาว่าไฟล์ที่ไร้สีคือ feature พังหรือไร้สีโดยเจตนา

**บันทึกไฟล์ลงเครื่องได้** หัวของทุกแท็บไฟล์มีปุ่ม **Download** และ popup ที่บอกว่า
ไฟล์นี้ดูไม่ได้ก็มีเช่นกัน — PDF ที่อ่านใน pane ไม่ได้เลยมักเป็นไฟล์ที่ยังอยากเก็บไว้อยู่ดี
ทั้งสองที่เพียงส่ง link ให้ browser ไปที่ `GET /file?path=…` ซึ่งตอบด้วยไบต์ของตัวไฟล์เอง
แบบ attachment จึงเป็น box บันทึกของ browser ที่ทำงานต่อเอง: ไม่ต้อง copy & paste,
ไม่มีการจัดรูปแบบใหม่ และ tab console ก็อยู่หน้าเดิมแทนที่จะ navigate ไปเปิดไฟล์นั้น
การ download เจตนาไม่ให้ใช้กฎเดียวกับที่ file view ใช้ — ไม่มีการตรวจว่าเป็น text หรือไม่
เพราะ PDF, image และ archive ที่ view ปฏิเสธคือส่วนใหญ่ของคนอยากเก็บไว้ และไม่มีเพดาน
2 MB เพราะ archive ที่ถูกตัดครึ่งคือไฟล์เสีย; ไบต์ถูกส่งแบบ stream ดังนั้นไฟล์ใหญ่จึง
กินหน่วยความจำเท่าหนึ่ง chunk ไม่ใช่เท่าขนาดไฟล์ ส่วนที่ถูกรักษาไว้คือกฎเดียวกับที่ tree
ใช้: ชื่อไฟล์ต้องอยู่ภายใน working directory และมีเฉพาะ page ที่ serve จาก loopback
เท่านั้นที่ขอได้ (page ที่ถูกเรียกจาก site อื่นถูกปฏิเสธ — ดู paragraph เรื่อง listener ด้านล่าง)

**แท็บ terminal เลื่อนดูย้อนหลังได้** ทุก pane เก็บ scrollback ของ 2000 บรรทัด
ล่าสุด จึงอ่าน `cargo build` ที่เลื่อนผ่านไปนานแล้วได้: หมุน wheel, ลาก scrollbar
ของ pane, หรือกด `Shift-PgUp`/`Shift-PgDn` เพื่อเลื่อนทีละหน้าจอ — คีย์ธรรมดายังเป็น
ของ shell ตามเดิม เพราะ `less` และ `man` คือเหตุผลที่คนกดมัน คีย์ที่คุณส่งไปหรือ
พิมพ์ลงไปจะพา pane กลับมาที่ prompt เสมอ สิ่งที่คุณ scroll ขึ้นไปอ่านจะไม่ถูกขยับ
เมื่อ shell พิมพ์เพิ่ม — output ใหม่เข้า *ใต้* สายตาที่อ่านอยู่ ไม่ใช่ทับมัน — จึงไม่มี
คำสั่งยาว ๆ ตัวไหนดึง view กลับลงไปข้างล่าง และ pane ที่อยู่ที่ bottom แล้วเท่านั้นที่
ตาม output ไป การ resize จะ re-wrap scrollback ใหม่ทั้งก้อนที่ width ใหม่
บรรทัดที่เคยต้องใช้สองแถวตอน pane แคบจึงอ่านเป็นบรรทัดเดียวตอน pane กว้าง
`Ctrl-C` ในแท็บ terminal ยังเป็น interrupt ของ shell เสมอ ไม่ได้เป็นวิธี scroll

prompt ที่พิมพ์บนเวบจะถูกเข้าคิวให้ loop ของ REPL เป็นตัวอ่านเอง นี่คือเหตุผลที่
คำตอบโผล่ทั้งสองหน้าต่าง และทำให้ prompt จากเวบ, prompt ที่พิมพ์ และ trigger ของ
`/watchfile` ไม่รันซ้อนกัน textarea จะ freeze ระหว่าง turn และกลับมาพิมพ์ได้เมื่อจบ
การเปิดหรือ reload แท็บจะ replay บทสนทนาที่ผ่านมา **พร้อมสีครบ** เพราะ replay อ่านจาก
สิ่งที่ถูกส่งให้ terminal ไม่ใช่จากไฟล์ log ที่ถูก strip ANSI แล้ว จึงเห็นบทสนทนาเดิม
ในสีเดียวกับที่ terminal แสดง

`jonnyq -web 8080 < /dev/null` (หรือ session ที่ stdin ถูกปิดด้วย Ctrl-D) จะรันต่อ
แบบมีแต่เวบแทนที่จะ exit ด้วย EOF — prompt ที่ส่งมาจะถูก echo กลับเข้า console และ
`/exit` ในแท็บจะปิด session

**หยุด turn จากหน้าเวบ** ปุ่ม **Stop** ใต้ช่องพิมพ์ prompt ข้างปุ่ม Send คือ Ctrl-C
ของเวบ: มันส่ง message `stop` ไปกด cancellation token ตัวเดียวกับที่ SIGINT ของ
terminal กด จึงไม่มีการยกเลิกอีกแบบที่อ่อนกว่า — stream ของ model ถูกตัด, process
group ทั้งหมดของ `run_command` ถูกฆ่า, คำถาม `ask_user` ที่ค้างอยู่ถูกถอนกลับ และ
turn จบแบบเดียวกันด้วยการพิมพ์ `[cancelled]` ลง transcript โดย session ยังอยู่ต่อ
เพื่อรับ prompt ถัดไป ปุ่มจะ enabled เฉพาะตอน session บอกว่ามี turn กำลังรันจริง
(ไม่ใช่แค่ตอน textarea เป็นสีเทา ซึ่งรวมถึง "ยังไม่มี model" กับ "connection หลุด"
ด้วย) และค่านี้ถูกส่งมากับ frame `ready`, `info`, `stop` เช่นเดียวกับ field `busy`
ของ `/health` จึงแท็บที่ reload หรือ reconnect ก็ตั้งปุ่มได้ถูกทันที กดซ้ำได้เสมอ
— turn ที่ยังไม่รู้ตัวจากการกดครั้งแรกหยุดได้อีกครั้ง — และการกดที่มาถึงหลัง turn
จบแล้วจะบอกว่า *Nothing to stop* แทนที่จะแกล้งทำว่าหยุดอะไรสักอย่าง

**ตอบ `ask_user` บนเวบ** คำถามที่ model ถามจะถูกแสดงเป็น popup ทับหน้าเวบ: ตัว
คำถาม, ช่องพิมพ์คำตอบ และปุ่ม **Send answer** (Enter ส่ง, Shift-Enter ขึ้นบรรทัด
ใหม่) terminal ก็พิมพ์คำถามเดียวกันไว้ด้วย จึงตอบที่ไหนก็ได้ — popup บนเวบ, keyboard
หรือ ntfy เป็นสาม channel ที่แข่งกันตอบ และคำตอบที่มาก่อนชนะเสมอ เมื่อมีคำตอบแล้ว
channel อื่นถูกรวบให้ทันที: popup จะปิดตัวเองพร้อมเขียนว่า *answered elsewhere*
(จึงไม่มีกล่องหายไปเฉย ๆ โดยไม่มีเหตุผล) และคำตอบที่ส่งไปถึงคำถามที่ถูกตอบไปแล้วจะถูก
ปฏิเสธแทนที่จะเข้าคิวรอ การ reload หรือ reconnect จะได้คำถามที่ยังไม่มีใครตอบกลับ
มาจาก greeting จึง refresh กลางรันยาว ๆ แล้วไม่เสียคำถามไป คำถามค้างพร้อมกันหลายข้อ
ได้ (turn ที่ถูก cancel จะทิ้งคำถามเดิมไว้): popup จะแสดงเฉพาะข้อแรก ข้างหลังนับ
จำนวนไว้ให้ และจะเลื่อนไปถามข้อถัดไปเมื่อแต่ละข้อถูกตอบ ไม่มีปุ่มซ่อน popup อีกแล้ว
(ทั้งปุ่ม hide และ Escape) เพราะมันเอาช่องป้อนคำตอบหายไปจากจอโดยไม่มีทางกลับ
ทั้งที่ jonnyq ยังค้างรอคำตอบอยู่ — จึงมีเฉพาะ server ที่ปิดกล่องได้ด้วย `ask_close`
ปุ่ม **Stop** ในกล่องนี้ไม่ใช่ทางเลี่ยงคำถาม: มันยกเลิก turn ที่ถามอยู่ และคำถามจะ
หายไปเพราะไม่มีใครถามอีก — ถ้าไม่มีปุ่มนี้ session ที่ค้างรอคำถามจะหยุดจากเวบไม่ได้
เลย เพราะ popup บังช่องพิมพ์ prompt ทับมิด
ใน session แบบไม่มี terminal popup นี้คือ channel เดียว จึงเป็นสิ่งทำให้ `-web`
ใช้งานได้ตามลำพัง

listener ผูกกับ `127.0.0.1` และตอบเฉพาะ request ที่ `Host` (และ `Origin` เมื่อ
browser ส่งมา) เป็น loopback จึงเป็น interface ในเครื่องเท่านั้น `GET /health` ตอบ
สถานะ session เป็น JSON รวมถึง `asks` (จำนวนคำถาม `ask_user` ที่ยังไม่มีใครตอบ
คือ jonnyq กำลังรอมนุษย์ ไม่ใช่กำลังทำงาน) และ `busy` (มี turn กำลังรันอยู่หรือไม่
— นั่นคือมีอะไรให้ `stop` ยกเลิกหรือเปล่า) ส่วน `GET /file?path=…` ซึ่งเป็นปลายทางของ
ปุ่ม **Download** ก็ยึดกฎเดียวกันบวกกับกฎของ tree: ชื่อไฟล์ต้อง resolve อยู่ภายใน
working directory และ browser ที่บอกว่าถูกเรียกจาก site อื่น (`Sec-Fetch-Site:
cross-site`) จะถูกปฏิเสธ เพื่อไม่ให้ page ของคนนอกหย่อนไฟล์ลง folder Downloads ของคุณได้
ส่วนนี้**ไม่ใช่** wall กันผู้ที่รันเป็น user เดียวกัน: แท็บเวบใดๆ ก็อ่านไฟล์ใน working
directory ได้ และการเปลี่ยน model / provider / proxy / ntfy topic ยังต้องพิมพ์ที่
keyboard เท่านั้น (ของบนหน้าเวบมีไว้เลือก font, ขนาดตัวอักษร, บันทึกไฟล์ที่ tree แสดงอยู่
และหยุด turn เท่านั้น)

## การแจ้งเตือน (ntfy)

ตั้ง topic URL เต็ม ๆ ด้วย `-ntfy-url` / `JONNYQ_NTFY_URL` / `/ntfy <url>` (ว่าง = ปิด) jonnyq จะ push ข้อความเมื่อ `/plan`, `/coding`, `/coding_phase` หรือ `/autocoding` จบ และส่ง priority สูง + tag `warning` เมื่อ turn ล้มเหลวหรือ stall โดยเนื้อหา error มีบรรทัด `Reason:` บอกสาเหตุที่น่าจะเป็น สรุปงานคำนวณจากไฟล์ `.progress` โดยตรง (ไม่เรียก model เพิ่ม)

topic เดียวกันใช้เป็นขากลับได้ด้วย: `ask_user` จะ push คำถามออกไปแล้ว subscribe รอคำตอบ จึงตอบจากมือถือได้ (`answer: <ข้อความ>` หรือ quick reply) — ใครตอบก่อนได้ก่อน ไม่ว่าจะเป็น keyboard, popup บนเวบ (`-web`) หรือมือถือ และ channel ที่ตอบแพ้จะถูกปิดให้ (popup ปิด, prompt ที่ terminal ไม่ค้างอยู่)

## ไฟล์ที่โปรแกรมสร้างระหว่างรัน

| ไฟล์ | เมื่อไร | หมายเหตุ |
|---|---|---|
| `output.txt` | เสมอ (`-output`) | transcript log แบบข้อความล้วน append — มีใน .gitignore แล้ว |
| `.env` | เมื่อคุณสร้าง | settings ของ project — ถูก ignore ไว้ เริ่มจาก `.env.example` |
| `$HOME/.jonnyq_history` | ทุกครั้งที่กด Enter ส่ง prompt | ใช้ร่วมกันข้าม session ได้ ย้ายที่ด้วยการเปลี่ยน `HOME` |
| `.progress`, `.progress.hash`, `.progress.retry` | คำสั่งกลุ่ม coding | `.progress` ควร commit ส่วนอีกสองไฟล์เป็น state local — ถูก ignore |
| `.jonnyq/pdf_pages/**.png` | `read_pdf` เจอ PDF ที่ไม่มี text | cache ลบได้เสมอ |

## ความปลอดภัย

- **path confinement** — tool กลุ่มไฟล์ resolve ทีละ component พร้อม symlink ทุกขั้น จึงอ่าน/เขียนภายนอก working directory ไม่ได้เลย
- **`run_command` รันด้วยสิทธิ์เต็มของคุณและไม่ถามก่อน** — มี denylist กันคำสั่งทำลายล้าง ~20 pattern (`rm -rf /`, fork bomb, `mkfs`, เขียนลง `/dev/sd*`, `curl … | sh`, …) ซึ่งเป็น best-effort **ไม่ใช่ sandbox**
- **ntfy ไม่มีการยืนยันตัวตน** — ใครที่รู้ topic ก็ตอบ `ask_user` ได้ จึงควรใช้ชื่อ topic สุ่มยาว ๆ และไม่ใส่ secret ลงใน prompt
- **resource caps** — อ่านไฟล์ 2 MB · `web_fetch` 3 MB · `read_pic` 10 MB · จำกัดจำนวนหน้า PDF และ tool call ต่อ turn
- **API key** ถูกส่งเฉพาะ header `Authorization` และไม่เคยถูกพิมพ์ออกจอหรือลง log

## License

ยังไม่ระบุใน repo (`Cargo.toml` ไม่มี field `license`) — ติดต่อผู้ดูแลโปรเจกต์ก่อนนำไปเผยแพร่
