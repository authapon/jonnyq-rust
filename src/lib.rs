//! jonnyq — an interactive coding agent CLI.
//!
//! Rust port of the Go `jonnyq` program, built on tokio. The module layout
//! mirrors the Go `internal/` packages:
//!
//! - [`config`]  — flag + `JONNYQ_*` env + `.env` file parsing
//!   ([`config::dotenv`]) with `flag > env > file > default` precedence
//! - [`llm`]     — provider interface + `ollama.rs`, `openai.rs`
//! - [`net`]     — the shared outbound HTTP clients + the SOCKS proxy setting
//! - [`tools`]   — read_file, write_file, edit_file, create_folder, web_search,
//!   web_fetch, read_pdf, read_pic, run_command, read_skill, ask_user
//! - [`agent`]   — tool-calling loop, context compaction, metrics, tool modes
//! - [`repl`]    — prompt loop + slash commands
//! - [`skill`]   — skill discovery
//! - [`coding`]  — /plan, /plan_for_human, /coding, /coding_phase, /autocoding
//! - [`watch`]   — /watchfile: poll the working directory for a magic word
//! - [`notify`]  — /ntfy: push notifications to ntfy.sh / a self-hosted server
//! - [`ui`]      — terminal colours + plain-text log
//! - [`input`]   — the one stdin reader, shared by the REPL and ask_user
//! - [`web`]     — `-web <port>`: the browser twin of the REPL — live console,
//!   prompts from the browser, the tool-approval modal, the files browser

pub mod agent;
pub mod coding;
pub mod config;
pub mod error;
pub mod input;
pub mod llm;
pub mod net;
pub mod notify;
pub mod repl;
pub mod skill;

#[cfg(test)]
mod testsupport;
pub mod tools;
pub mod ui;
pub mod watch;
pub mod web;
