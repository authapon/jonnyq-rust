//! Loads jonnyq settings from CLI flags, a dotenv file (`.env` by default)
//! and `JONNYQ_*` environment variables, in that order of precedence.
//!
//! Port of Go `internal/config`, extended with the dotenv layer in
//! [`dotenv`]:
//!
//! ```text
//! CLI flag  >  .env file  >  process env var  >  built-in default
//! ```
//!
//! The dotenv file is read once, when [`load`] runs. A running session can
//! re-read it with the `/env` slash command, which rebuilds a Config through
//! the same code path ([`reload_with`]) and pushes whatever the live session
//! is able to change onto the agent/tools.

use crate::error::{AgentError, R};

pub mod dotenv;

pub use dotenv::Dotenv;

pub const ENV_PREFIX: &str = "JONNYQ_";

pub const DEFAULT_PROVIDER: &str = "ollama:http://localhost:11434";
pub const DEFAULT_CONTEXT_SIZE: i64 = 16348;
pub const DEFAULT_OUTPUT_FILE: &str = "output.txt";
pub const DEFAULT_SEARXNG_MAX_RESULTS: i64 = 10;
pub const DEFAULT_SEARXNG_CONCURRENCY: i64 = 4;
pub const DEFAULT_SEARXNG_TIMEOUT_SEC: i64 = 30;
pub const DEFAULT_FETCH_CONCURRENCY: i64 = 5;
pub const DEFAULT_FETCH_TIMEOUT_SEC: i64 = 30;
pub const DEFAULT_PDF_MAX_PAGES: i64 = 50;
pub const DEFAULT_PDF_DPI: i64 = 150;
pub const DEFAULT_THINKING: bool = true;
pub const DEFAULT_RUN_COMMAND_TIMEOUT_SEC: i64 = 300;
/// `-history-size` / `JONNYQ_HISTORY_SIZE`: how many prompts the `> ` prompt
/// keeps for ↑/↓ recall.
pub const DEFAULT_HISTORY_SIZE: i64 = 200;
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: i64 = 50;
pub const DEFAULT_WATCH_MAGIC_WORD: &str = "AI!";
pub const DEFAULT_WATCH_POLL_INTERVAL_SEC: i64 = 1;
pub const DEFAULT_NTFY_URL: &str = "";
pub const DEFAULT_TOOL_CALL_MODE: &str = "native";
/// `-web` / `JONNYQ_WEB_PORT`: port of the browser console on 127.0.0.1. Zero
/// (the default) runs no server, so a session is terminal-only exactly as
/// before the feature existed.
pub const DEFAULT_WEB_PORT: i64 = 0;

/// `-no-proxy` / `JONNYQ_NO_PROXY`: the bypass list of an explicitly
/// configured SOCKS proxy (see [`crate::net::DEFAULT_NO_PROXY`]).
pub const DEFAULT_NO_PROXY: &str = crate::net::DEFAULT_NO_PROXY;

/// Context-compaction defaults (see `agent::CompactTunables` and the
/// `JONNYQ_COMPACT_*` settings): the fraction of the context window at which
/// history gets summarized, the fraction the summary may occupy, how many
/// recent messages survive a compaction verbatim, and the character ceiling
/// on the transcript sent to be summarized.
pub const DEFAULT_COMPACT_HEADROOM_PERCENT: i64 = 60;
pub const DEFAULT_COMPACT_TARGET_PERCENT: i64 = 25;
pub const DEFAULT_COMPACT_KEEP_RECENT: i64 = 6;
pub const DEFAULT_SUMMARY_TRANSCRIPT_CAP: i64 = 48_000;

/// Ceiling on how many characters one tool result may contribute to the
/// conversation history (`JONNYQ_MAX_TOOL_RESULT_CHARS`) - see
/// `tools::cap_tool_result`.
pub const DEFAULT_MAX_TOOL_RESULT_CHARS: i64 = 64_000;

const USAGE: &str = "Usage: jonnyq [flags]

Flags:
  -provider name:url            AI provider (ollama or openai), e.g. ollama:http://localhost:11434
  -key <key>                    provider API key
  -model <name>                 model name to use
  -context <size>               context size (default 16348)
  -output <file>                transcript log output file (default output.txt)
  -searxng-url <url>            searxng base URL for web_search
  -searxng-max-results <n>      max web_search results (default 10)
  -searxng-concurrency <n>      web_search concurrency (default 4)
  -searxng-timeout-sec <n>      web_search timeout seconds (default 30)
  -fetch-concurrency <n>        web_fetch concurrency (default 5)
  -fetch-timeout-sec <n>        web_fetch timeout seconds (default 30)
  -pdf-max-pages <n>            max PDF pages to read (default 50)
  -pdf-dpi <n>                  PDF render DPI (default 150)
  -run-command-timeout-sec <n>  run_command timeout seconds (default 300)
  -history-size <n>             prompts kept for up/down recall at the prompt
                                (default 200)
  -max-tool-calls-per-turn <n>  max tool calls per turn (default 50)
  -compact-headroom-percent <n> percent of the context window that triggers
                                compaction (default 60)
  -compact-target-percent <n>   percent of the window the summary may use
                                (default 25)
  -compact-keep-recent <n>      messages kept verbatim through a compaction
                                (default 6)
  -summary-transcript-cap <n>   max characters sent to be summarized
                                (default 48000)
  -max-tool-result-chars <n>    per-tool-result character cap on history
                                (default 64000)
  -watch-magic-word <word>      magic word /watchfile scans for (default \"AI!\")
  -watch-poll-interval-sec <n>  /watchfile polling interval seconds (default 1)
  -ntfy-url <url>               ntfy topic URL for /plan and /coding notifications
  -socks-proxy <url|off>        SOCKS proxy for every request (model, web_search,
                                web_fetch, ntfy): socks5h://host:port, socks5,
                                socks4a, socks4 or bare host:port (= socks5h);
                                off = always direct; auto = follow ALL_PROXY etc.
  -no-proxy <hosts>             hosts that bypass -socks-proxy (comma-separated;
                                default localhost,127.0.0.1,::1)
  -tool-mode native|prompt      tool-calling mode (default native)
  -skill-path <paths>           semicolon-separated skill directories
  -web <port>                   serve the web console on http://127.0.0.1:<port>
                                (0 = no web interface, the default)
  -thinking=true|false          enable model thinking (default true)
  -env-file <path|off>          dotenv file to read settings from (default .env
                                in the working directory; off/none/no disables it)

Settings are read in this order, each layer overriding the previous one:
  CLI flag  >  .env file  >  JONNYQ_* environment variable  >  default

The .env file uses KEY=value lines (comments, quotes and $VAR expansion
supported; see the .env.example shipped with jonnyq). The file wins over the
environment, so a value exported in the shell is only a fallback for what the
project's .env does not say.

Environment variables are the flag name uppercased with a JONNYQ_ prefix, e.g.
JONNYQ_MODEL, JONNYQ_CONTEXT_SIZE, JONNYQ_ENV_FILE.
";

/// Which layer supplied a setting's final value. Reported by `/env list` so
/// it is always obvious why a value is what it is - the usual cause of
/// "I changed it in the environment but nothing happened" being a leftover
/// command-line flag or a key in the `.env` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// `-flag value` on the command line.
    Flag,
    /// The dotenv file (`.env` by default).
    File,
    /// A `JONNYQ_*` variable in the process environment.
    Env,
    /// jonnyq's built-in default.
    Default,
}

impl Source {
    pub fn label(&self) -> &'static str {
        match self {
            Source::Flag => "flag",
            Source::File => "file",
            Source::Env => "env",
            Source::Default => "default",
        }
    }
}

/// One row of `/env list`: a setting, the value in effect and where it came
/// from.
pub struct Setting {
    /// The CLI flag name, without the leading dash (e.g. `context`).
    pub flag: &'static str,
    /// The environment variable / dotenv key (e.g. `JONNYQ_CONTEXT_SIZE`).
    pub env: String,
    /// The value in effect, in display form.
    pub value: String,
    pub source: Source,
    /// True for secrets (`-key`): [`Setting::display`] masks the value.
    pub secret: bool,
}

impl Setting {
    /// The value as it should be printed - masked when it is a secret, but
    /// still distinguishable from "unset".
    pub fn display(&self) -> String {
        if self.secret {
            return if self.value.is_empty() {
                "(unset)".into()
            } else {
                "****".into()
            };
        }
        if self.value.is_empty() {
            "(unset)".to_string()
        } else {
            self.value.clone()
        }
    }
}

/// Resolves settings through the precedence chain, remembering which layer
/// answered. `flag` is the CLI flag name, `env` the `JONNYQ_*` suffix.
struct Resolver<'a> {
    given: &'a [(String, String)],
    dot: Option<&'a Dotenv>,
}

impl<'a> Resolver<'a> {
    /// The winning raw string and its layer, or `None` when no layer sets it.
    ///
    /// The dotenv file is checked *before* the process environment: a setting
    /// in the file is a deliberate, project-local choice and overrides what a
    /// shell happens to have `export`ed (which is usually a stale leftover
    /// from another project). A key the file sets to an empty value counts as
    /// set, so `JONNYQ_MODEL=` in the file does override an exported
    /// `JONNYQ_MODEL` - exactly like an exported empty variable did before.
    fn raw(&self, flag: &str, env: &str) -> Option<(String, Source)> {
        if let Some((_, v)) = self.given.iter().find(|(n, _)| n == flag) {
            return Some((v.clone(), Source::Flag));
        }
        let key = format!("{}{}", ENV_PREFIX, env);
        if let Some(v) = self.dot.and_then(|d| d.get(&key)) {
            return Some((v.to_string(), Source::File));
        }
        if let Ok(v) = std::env::var(&key) {
            return Some((v, Source::Env));
        }
        None
    }

    fn string(&self, flag: &str, env: &str, def: &str) -> (String, Source) {
        self.raw(flag, env)
            .unwrap_or_else(|| (def.to_string(), Source::Default))
    }

    fn int(&self, flag: &str, env: &str, def: i64) -> (i64, Source) {
        match self.raw(flag, env) {
            // An unparseable number falls back to the default value but keeps
            // reporting the layer it was set in, exactly as the env-only
            // version did.
            Some((v, src)) => (v.trim().parse::<i64>().unwrap_or(def), src),
            None => (def, Source::Default),
        }
    }

    fn bool(&self, flag: &str, env: &str, def: bool) -> (bool, Source) {
        match self.raw(flag, env) {
            // Go strconv.ParseBool accepts: 1, t, T, TRUE, true, True, 0, f, F, FALSE, false, False
            Some((v, src)) => match v.trim() {
                "1" | "t" | "T" | "TRUE" | "true" | "True" => (true, src),
                "0" | "f" | "F" | "FALSE" | "false" | "False" => (false, src),
                _ => (def, src),
            },
            None => (def, Source::Default),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub provider_name: String, // "ollama" or "openai"
    pub provider_url: String,
    pub key: String,
    pub model: String,
    pub context_size: i64,
    pub output_file: String,

    pub searxng_url: String,
    pub searxng_max_results: i64,
    pub searxng_concurrency: i64,
    pub searxng_timeout_sec: i64,

    pub fetch_concurrency: i64,
    pub fetch_timeout_sec: i64,

    pub pdf_max_pages: i64,
    pub pdf_dpi: i64,

    pub run_command_timeout_sec: i64,
    /// How many submitted prompts the `> ` prompt remembers for ↑/↓.
    pub history_size: i64,
    pub max_tool_calls_per_turn: i64,

    /// Context-compaction tuning, applied to the agent at startup and by
    /// `/env reload`. See `agent::CompactTunables`.
    pub compact_headroom_percent: i64,
    pub compact_target_percent: i64,
    pub compact_keep_recent: i64,
    pub summary_transcript_cap: i64,
    /// `tools::set_max_tool_result_chars` ceiling.
    pub max_tool_result_chars: i64,

    pub watch_magic_word: String,
    pub watch_poll_interval_sec: i64,

    /// ntfy.sh (or self-hosted) topic URL /plan and /coding post a completion
    /// summary to, e.g. "https://ntfy.sh/my-topic". Empty disables
    /// notifications.
    pub ntfy_url: String,

    /// SOCKS proxy for every outbound request, as typed: empty = follow the
    /// usual `ALL_PROXY`/`HTTPS_PROXY` environment variables, `off` = always
    /// direct, otherwise a `socks5h://host:port` style URL. See [`crate::net`],
    /// and the `/proxy` slash command for changing it mid-session.
    pub socks_proxy: String,

    /// Comma-separated hosts that bypass [`Self::socks_proxy`]
    /// (`-no-proxy` / `JONNYQ_NO_PROXY`). Defaults to loopback, so a local
    /// model server is never tunnelled by accident.
    pub no_proxy: String,

    /// "native" (default) or "prompt" - see agent::ToolCallMode. Validated in
    /// load/set_tool_call_mode so callers can rely on it being one of the two.
    pub tool_call_mode: String,

    pub skill_paths: Vec<String>,

    /// `-web` / `JONNYQ_WEB_PORT`: port of the browser console, bound to
    /// 127.0.0.1. Zero means no web interface at all.
    pub web_port: i64,

    pub thinking: bool,

    /// Where each setting's final value came from, keyed by the `JONNYQ_*`
    /// suffix (e.g. `CONTEXT_SIZE`). Settings set through a slash command
    /// instead (`/model`, `/context`, ...) are reported as `default` until the
    /// next reload, since a slash command is neither a flag, an env var, nor a
    /// file.
    pub(crate) sources: std::collections::HashMap<String, Source>,

    /// The dotenv selector as given by `-env-file` / `JONNYQ_ENV_FILE`: empty
    /// = look for [`.env`](dotenv::DEFAULT_ENV_FILE) in the working directory,
    /// `off`/`none`/`no` = read no file at all, anything else = that path.
    /// `/env` changes it for the rest of the session.
    pub env_file: String,

    /// The parsed dotenv file the settings above were partly taken from
    /// (empty when nothing was loaded). Kept so `/env` can report and re-read
    /// it without touching the process environment.
    pub dotenv: Dotenv,

    /// The raw CLI arguments this config was built from. `/env reload`
    /// replays them so flags keep winning over the file after a reload.
    pub startup_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider_name: "ollama".into(),
            provider_url: "http://localhost:11434".into(),
            key: String::new(),
            model: String::new(),
            context_size: DEFAULT_CONTEXT_SIZE,
            output_file: DEFAULT_OUTPUT_FILE.into(),
            searxng_url: String::new(),
            searxng_max_results: DEFAULT_SEARXNG_MAX_RESULTS,
            searxng_concurrency: DEFAULT_SEARXNG_CONCURRENCY,
            searxng_timeout_sec: DEFAULT_SEARXNG_TIMEOUT_SEC,
            fetch_concurrency: DEFAULT_FETCH_CONCURRENCY,
            fetch_timeout_sec: DEFAULT_FETCH_TIMEOUT_SEC,
            pdf_max_pages: DEFAULT_PDF_MAX_PAGES,
            pdf_dpi: DEFAULT_PDF_DPI,
            run_command_timeout_sec: DEFAULT_RUN_COMMAND_TIMEOUT_SEC,
            history_size: DEFAULT_HISTORY_SIZE,
            max_tool_calls_per_turn: DEFAULT_MAX_TOOL_CALLS_PER_TURN,
            compact_headroom_percent: DEFAULT_COMPACT_HEADROOM_PERCENT,
            compact_target_percent: DEFAULT_COMPACT_TARGET_PERCENT,
            compact_keep_recent: DEFAULT_COMPACT_KEEP_RECENT,
            summary_transcript_cap: DEFAULT_SUMMARY_TRANSCRIPT_CAP,
            max_tool_result_chars: DEFAULT_MAX_TOOL_RESULT_CHARS,
            watch_magic_word: DEFAULT_WATCH_MAGIC_WORD.into(),
            watch_poll_interval_sec: DEFAULT_WATCH_POLL_INTERVAL_SEC,
            ntfy_url: DEFAULT_NTFY_URL.into(),
            socks_proxy: String::new(),
            no_proxy: DEFAULT_NO_PROXY.into(),
            tool_call_mode: DEFAULT_TOOL_CALL_MODE.into(),
            skill_paths: Vec::new(),
            web_port: DEFAULT_WEB_PORT,
            thinking: DEFAULT_THINKING,
            sources: std::collections::HashMap::new(),
            env_file: String::new(),
            dotenv: Dotenv::empty(),
            startup_args: Vec::new(),
        }
    }
}

impl Config {
    /// Parses a "name:url" string (e.g. "ollama:http://localhost:11434")
    /// into provider_name/provider_url.
    pub fn set_provider(&mut self, s: &str) -> R<()> {
        let parts: Vec<&str> = s.splitn(2, ':').collect();
        if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
            return Err(AgentError::new(format!(
                "invalid provider {:?}, expected format name:url",
                s
            )));
        }
        let name = parts[0].to_lowercase();
        if name != "ollama" && name != "openai" {
            return Err(AgentError::new(format!(
                "unsupported provider {:?}, expected ollama or openai",
                name
            )));
        }
        self.provider_name = name;
        self.provider_url = parts[1].to_string();
        Ok(())
    }

    /// Validates and sets tool_call_mode ("native" or "prompt",
    /// case-insensitive).
    pub fn set_tool_call_mode(&mut self, s: &str) -> R<()> {
        let mode = s.trim().to_lowercase();
        if mode != "native" && mode != "prompt" {
            return Err(AgentError::new(format!(
                "invalid tool-mode {:?}, expected native or prompt",
                s
            )));
        }
        self.tool_call_mode = mode;
        Ok(())
    }

    /// The proxy settings as [`crate::net`] wants them, validating
    /// [`Self::socks_proxy`] - so a malformed `-socks-proxy` is a config error
    /// at startup (and a rejected `/proxy`), never a mysterious failure on the
    /// first request.
    pub fn proxy_config(&self) -> R<crate::net::ProxyConfig> {
        crate::net::ProxyConfig::parse(&self.socks_proxy, &self.no_proxy)
    }

    /// Validates `s` and stores it as `socks_proxy` (see
    /// [`crate::net::ProxyConfig::parse`] for the accepted spellings). The
    /// value is kept normalised (`host:port` -> `socks5h://host:port`) so
    /// `/env list` and a later reload see what is actually in effect.
    pub fn set_socks_proxy(&mut self, s: &str) -> R<()> {
        let p = crate::net::ProxyConfig::parse(s, &self.no_proxy)?;
        self.socks_proxy = crate::net::setting_value(&p);
        Ok(())
    }

    /// The proxy settings of a config that must not fail - for reporting
    /// (startup chatter, `/proxy` with no argument). An invalid value can only
    /// have arrived through a mutation that skipped the checks, and is then
    /// described as unset rather than being silently honoured.
    pub fn proxy_config_or_default(&self) -> crate::net::ProxyConfig {
        self.proxy_config()
            .unwrap_or_else(|_| crate::net::ProxyConfig::system())
    }

    /// Path of the dotenv file that supplied settings, or `None` when no file
    /// was read (nothing found, or dotenv disabled).
    pub fn dotenv_path(&self) -> Option<String> {
        if self.dotenv.has_file() {
            Some(self.dotenv.path())
        } else {
            None
        }
    }

    /// How many `JONNYQ_*` settings the loaded file actually defines.
    pub fn dotenv_setting_count(&self) -> usize {
        self.dotenv
            .vars()
            .iter()
            .filter(|(k, _)| k.starts_with(ENV_PREFIX))
            .count()
    }

    /// One-line description of where settings came from, for `/env` and
    /// startup chatter.
    pub fn dotenv_summary(&self) -> String {
        match self.dotenv_path() {
            Some(p) => format!(
                "{} ({} JONNYQ_* setting{}) from this file",
                p,
                self.dotenv_setting_count(),
                if self.dotenv_setting_count() == 1 {
                    ""
                } else {
                    "s"
                }
            ),
            None if is_disabled_selector(&self.env_file) => {
                "dotenv file disabled (-env-file off)".to_string()
            }
            None => format!(
                "no {} file found; using environment variables only",
                dotenv::DEFAULT_ENV_FILE
            ),
        }
    }

    /// Every setting with its value in effect and the layer that supplied it,
    /// in the order they are documented - the table `/env list` prints.
    ///
    /// The value is the current one (so a `/model` typed since startup shows
    /// up), while `source` reflects how the value arrived at startup.
    pub fn settings(&self) -> Vec<Setting> {
        let skill_paths = self.skill_paths.join(";");
        let rows: Vec<(&'static str, &str, String, Source, bool)> = vec![
            (
                "provider",
                "PROVIDER",
                format!("{}:{}", self.provider_name, self.provider_url),
                self.src("PROVIDER"),
                false,
            ),
            ("key", "KEY", self.key.clone(), self.src("KEY"), true),
            (
                "model",
                "MODEL",
                self.model.clone(),
                self.src("MODEL"),
                false,
            ),
            (
                "context",
                "CONTEXT_SIZE",
                self.context_size.to_string(),
                self.src("CONTEXT_SIZE"),
                false,
            ),
            (
                "output",
                "OUTPUT_FILE",
                self.output_file.clone(),
                self.src("OUTPUT_FILE"),
                false,
            ),
            (
                "thinking",
                "THINKING",
                self.thinking.to_string(),
                self.src("THINKING"),
                false,
            ),
            (
                "searxng-url",
                "SEARXNG_URL",
                self.searxng_url.clone(),
                self.src("SEARXNG_URL"),
                false,
            ),
            (
                "searxng-max-results",
                "SEARXNG_MAX_RESULTS",
                self.searxng_max_results.to_string(),
                self.src("SEARXNG_MAX_RESULTS"),
                false,
            ),
            (
                "searxng-concurrency",
                "SEARXNG_CONCURRENCY",
                self.searxng_concurrency.to_string(),
                self.src("SEARXNG_CONCURRENCY"),
                false,
            ),
            (
                "searxng-timeout-sec",
                "SEARXNG_TIMEOUT_SEC",
                self.searxng_timeout_sec.to_string(),
                self.src("SEARXNG_TIMEOUT_SEC"),
                false,
            ),
            (
                "fetch-concurrency",
                "FETCH_CONCURRENCY",
                self.fetch_concurrency.to_string(),
                self.src("FETCH_CONCURRENCY"),
                false,
            ),
            (
                "fetch-timeout-sec",
                "FETCH_TIMEOUT_SEC",
                self.fetch_timeout_sec.to_string(),
                self.src("FETCH_TIMEOUT_SEC"),
                false,
            ),
            (
                "pdf-max-pages",
                "PDF_MAX_PAGES",
                self.pdf_max_pages.to_string(),
                self.src("PDF_MAX_PAGES"),
                false,
            ),
            (
                "pdf-dpi",
                "PDF_DPI",
                self.pdf_dpi.to_string(),
                self.src("PDF_DPI"),
                false,
            ),
            (
                "run-command-timeout-sec",
                "RUN_COMMAND_TIMEOUT_SEC",
                self.run_command_timeout_sec.to_string(),
                self.src("RUN_COMMAND_TIMEOUT_SEC"),
                false,
            ),
            (
                "history-size",
                "HISTORY_SIZE",
                self.history_size.to_string(),
                self.src("HISTORY_SIZE"),
                false,
            ),
            (
                "compact-headroom-percent",
                "COMPACT_HEADROOM_PERCENT",
                self.compact_headroom_percent.to_string(),
                self.src("COMPACT_HEADROOM_PERCENT"),
                false,
            ),
            (
                "compact-target-percent",
                "COMPACT_TARGET_PERCENT",
                self.compact_target_percent.to_string(),
                self.src("COMPACT_TARGET_PERCENT"),
                false,
            ),
            (
                "compact-keep-recent",
                "COMPACT_KEEP_RECENT",
                self.compact_keep_recent.to_string(),
                self.src("COMPACT_KEEP_RECENT"),
                false,
            ),
            (
                "summary-transcript-cap",
                "SUMMARY_TRANSCRIPT_CAP",
                self.summary_transcript_cap.to_string(),
                self.src("SUMMARY_TRANSCRIPT_CAP"),
                false,
            ),
            (
                "max-tool-result-chars",
                "MAX_TOOL_RESULT_CHARS",
                self.max_tool_result_chars.to_string(),
                self.src("MAX_TOOL_RESULT_CHARS"),
                false,
            ),
            (
                "max-tool-calls-per-turn",
                "MAX_TOOL_CALLS_PER_TURN",
                self.max_tool_calls_per_turn.to_string(),
                self.src("MAX_TOOL_CALLS_PER_TURN"),
                false,
            ),
            (
                "watch-magic-word",
                "WATCH_MAGIC_WORD",
                self.watch_magic_word.clone(),
                self.src("WATCH_MAGIC_WORD"),
                false,
            ),
            (
                "watch-poll-interval-sec",
                "WATCH_POLL_INTERVAL_SEC",
                self.watch_poll_interval_sec.to_string(),
                self.src("WATCH_POLL_INTERVAL_SEC"),
                false,
            ),
            (
                "ntfy-url",
                "NTFY_URL",
                self.ntfy_url.clone(),
                self.src("NTFY_URL"),
                false,
            ),
            (
                "socks-proxy",
                "SOCKS_PROXY",
                crate::net::redact_url(&self.socks_proxy),
                self.src("SOCKS_PROXY"),
                false,
            ),
            (
                "no-proxy",
                "NO_PROXY",
                self.no_proxy.clone(),
                self.src("NO_PROXY"),
                false,
            ),
            (
                "tool-mode",
                "TOOL_MODE",
                self.tool_call_mode.clone(),
                self.src("TOOL_MODE"),
                false,
            ),
            (
                "skill-path",
                "SKILL_PATH",
                skill_paths,
                self.src("SKILL_PATH"),
                false,
            ),
            (
                "web",
                "WEB_PORT",
                self.web_port.to_string(),
                self.src("WEB_PORT"),
                false,
            ),
            (
                "env-file",
                "ENV_FILE",
                self.env_file.clone(),
                self.src_env_file(),
                false,
            ),
        ];
        rows.into_iter()
            .map(|(flag, env, value, source, secret)| Setting {
                flag,
                env: format!("{ENV_PREFIX}{env}"),
                value,
                source,
                secret,
            })
            .collect()
    }

    fn src(&self, env: &str) -> Source {
        self.sources.get(env).copied().unwrap_or(Source::Default)
    }

    /// `-env-file` is not a `JONNYQ_ENV_FILE`-style lookup in the table: when
    /// it was not given as a flag, the layer that "supplied" it is the
    /// auto-discovered `.env`, or the environment variable that named the file.
    fn src_env_file(&self) -> Source {
        match self.sources.get("ENV_FILE") {
            Some(s) => *s,
            // Nothing named a file; the path shown is the auto-discovered one
            // (or empty when there is none).
            None if self.dotenv_path().is_some() => Source::File,
            None => Source::Default,
        }
    }
}

/// True when a selector turns dotenv loading off.
fn is_disabled_selector(sel: &str) -> bool {
    dotenv::DISABLE_VALUES
        .iter()
        .any(|v| v.eq_ignore_ascii_case(sel.trim()))
}

/// Sentinel error message meaning "-h/-help was requested" (Go's
/// `flag.ErrHelp` equivalent, which the Go version surfaces as a config
/// error too).
pub(crate) const HELP_SENTINEL: &str = "flag: help requested";

/// Every accepted flag name, so an unknown one is rejected the way Go's
/// flag package rejects it ("flag provided but not defined").
const KNOWN_FLAGS: &[&str] = &[
    "provider",
    "key",
    "model",
    "context",
    "output",
    "searxng-url",
    "searxng-max-results",
    "searxng-concurrency",
    "searxng-timeout-sec",
    "fetch-concurrency",
    "fetch-timeout-sec",
    "pdf-max-pages",
    "pdf-dpi",
    "run-command-timeout-sec",
    "history-size",
    "compact-headroom-percent",
    "compact-target-percent",
    "compact-keep-recent",
    "summary-transcript-cap",
    "max-tool-result-chars",
    "max-tool-calls-per-turn",
    "watch-magic-word",
    "watch-poll-interval-sec",
    "ntfy-url",
    "socks-proxy",
    "no-proxy",
    "tool-mode",
    "skill-path",
    "web",
    "thinking",
    "env-file",
];

/// A tiny stand-in for Go's `flag` package: every flag takes a value except
/// `-thinking` (which only accepts `=value` inline) and `-h`/`-help`. Parsing
/// stops at the first positional argument, like Go's.
struct FlagParser<'a> {
    args: &'a [String],
    i: usize,
    /// name -> provided value (flags not present keep their env/dotenv value)
    given: Vec<(String, String)>,
    bool_flags: &'a [&'a str],
}

impl<'a> FlagParser<'a> {
    fn new(args: &'a [String], bool_flags: &'a [&'a str]) -> Self {
        Self {
            args,
            i: 0,
            given: Vec::new(),
            bool_flags,
        }
    }

    fn take_value(&mut self, name: &str, inline: Option<&str>) -> Result<String, AgentError> {
        if let Some(v) = inline {
            return Ok(v.to_string());
        }
        if self.i + 1 < self.args.len() {
            self.i += 1;
            Ok(self.args[self.i].clone())
        } else {
            Err(AgentError::new(format!(
                "flag needs an argument: -{}",
                name
            )))
        }
    }

    fn parse(&mut self) -> Result<(), AgentError> {
        while self.i < self.args.len() {
            let arg = self.args[self.i].clone();
            // Go's flag package stops at the first non-flag argument and
            // leaves it (and everything after) in Args(); jonnyq never looks
            // at those, so positional arguments are simply ignored here. A
            // lone "--" ends parsing too, "-" alone is not a flag.
            if !arg.starts_with('-') || arg == "-" || arg == "--" {
                if arg == "--" {
                    self.i += 1;
                }
                break;
            }
            let body = arg.trim_start_matches('-');
            let (name, inline) = match body.split_once('=') {
                Some((n, v)) => (n.to_string(), Some(v.to_string())),
                None => (body.to_string(), None),
            };
            if name == "h" || name == "help" {
                return Err(AgentError::new(HELP_SENTINEL));
            }
            if !KNOWN_FLAGS.iter().any(|f| *f == name) {
                return Err(AgentError::new(format!(
                    "flag provided but not defined: -{}",
                    name
                )));
            }
            let is_bool = self.bool_flags.iter().any(|b| *b == name);
            let value = if is_bool {
                // A bool flag only takes a value when it is given inline
                // (`-thinking=false`); `-thinking false` leaves "false" as a
                // positional argument, exactly like Go.
                match inline {
                    Some(v) => v,
                    None => "true".to_string(),
                }
            } else {
                self.take_value(&name, inline.as_deref())?
            };
            // Repeating a flag: last value wins (Go's flag sets the value on
            // the same target each time it is parsed).
            match self.given.iter_mut().find(|(n, _)| *n == name) {
                Some(entry) => entry.1 = value,
                None => self.given.push((name.clone(), value)),
            }
            self.i += 1;
        }
        Ok(())
    }
}

/// Builds a Config from a CLI argument list and the dotenv file `forced_env`
/// selects (`None` = take the selector from `-env-file` / `JONNYQ_ENV_FILE`).
///
/// Returns the config plus non-fatal warnings (unparsable dotenv lines, an
/// explicitly named file with no `JONNYQ_*` setting in it) for the caller to
/// surface - `load` prints them to stderr, the REPL's `/env` prints them to
/// the terminal.
fn build(args: &[String], forced_env: Option<&str>) -> R<(Config, Vec<String>)> {
    let bool_flags = ["thinking"];
    let mut parser = FlagParser::new(args, &bool_flags);
    match parser.parse() {
        Ok(()) => {}
        // Go's flag.ContinueOnError prints the offending message and the
        // usage to stderr, then returns the error to the caller (main),
        // which reports it as a config error and exits 1 - including for
        // -h/-help, which is why `-h` here exits non-zero just like the Go
        // binary does.
        Err(e) if e.message == HELP_SENTINEL => {
            // Go's flag prints just the usage for -h/-help (no error line)
            // and still returns ErrHelp to the caller.
            eprint!("{}", USAGE);
            return Err(e);
        }
        Err(e) => {
            eprintln!("{}", e.message);
            eprint!("{}", USAGE);
            return Err(e);
        }
    }
    let given = parser.given.clone();

    let mut warnings = Vec::new();

    // Which dotenv file to read: the selector passed in by /env wins, then
    // the flag, then the process environment. Only the process environment is
    // consulted here - a JONNYQ_ENV_FILE inside the file itself would be
    // circular, and dotenv::parse warns about it.
    let flag_env_file = given
        .iter()
        .rev()
        .find(|(n, _)| n == "env-file")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let selector = match forced_env {
        Some(s) => s.to_string(),
        None => {
            if !flag_env_file.trim().is_empty() {
                flag_env_file.clone()
            } else {
                std::env::var(format!("{}ENV_FILE", ENV_PREFIX)).unwrap_or_default()
            }
        }
    };
    let selected_explicitly = !selector.trim().is_empty();
    // Which layer named the file (the flag already lost to /env's argument if
    // forced_env is set; report the argument as a flag - it behaves like one).
    let env_file_source = match forced_env {
        Some(s) if !s.trim().is_empty() => Some(Source::Flag),
        None if !flag_env_file.trim().is_empty() => Some(Source::Flag),
        None if selected_explicitly => Some(Source::Env),
        _ => None,
    };
    let dotenv = Dotenv::resolve(&selector)?;
    if let Some(d) = &dotenv {
        warnings.extend(d.warnings.iter().cloned());
        if selected_explicitly && d.vars().iter().all(|(k, _)| !k.starts_with(ENV_PREFIX)) {
            warnings.push(format!(
                "{} defines no {}_* settings - check the path and the key names",
                d.path(),
                ENV_PREFIX.trim_end_matches('_')
            ));
        }
    }

    let mut c = Config::default();
    c.env_file = selector;
    c.startup_args = args.to_vec();

    let r = Resolver {
        given: &given,
        dot: dotenv.as_ref(),
    };
    // Where each setting's value came from, filled in alongside the
    // assignments below and stored on the config at the end (a closure
    // capturing `c` mutably would fight every `c.x = ...` for the borrow).
    let mut sources: std::collections::HashMap<String, Source> = std::collections::HashMap::new();

    let (provider_str, src) = r.string("provider", "PROVIDER", DEFAULT_PROVIDER);
    sources.insert("PROVIDER".to_string(), src);
    c.set_provider(&provider_str)?;
    let (key, src) = r.string("key", "KEY", "");
    sources.insert("KEY".to_string(), src);
    c.key = key;
    let (model, src) = r.string("model", "MODEL", "");
    sources.insert("MODEL".to_string(), src);
    c.model = model;
    let (n, src) = r.int("context", "CONTEXT_SIZE", DEFAULT_CONTEXT_SIZE);
    sources.insert("CONTEXT_SIZE".to_string(), src);
    c.context_size = n;
    let (v, src) = r.string("output", "OUTPUT_FILE", DEFAULT_OUTPUT_FILE);
    sources.insert("OUTPUT_FILE".to_string(), src);
    c.output_file = v;
    let (v, src) = r.string("searxng-url", "SEARXNG_URL", "");
    sources.insert("SEARXNG_URL".to_string(), src);
    c.searxng_url = v;
    let (n, src) = r.int(
        "searxng-max-results",
        "SEARXNG_MAX_RESULTS",
        DEFAULT_SEARXNG_MAX_RESULTS,
    );
    sources.insert("SEARXNG_MAX_RESULTS".to_string(), src);
    c.searxng_max_results = n;
    let (n, src) = r.int(
        "searxng-concurrency",
        "SEARXNG_CONCURRENCY",
        DEFAULT_SEARXNG_CONCURRENCY,
    );
    sources.insert("SEARXNG_CONCURRENCY".to_string(), src);
    c.searxng_concurrency = n;
    let (n, src) = r.int(
        "searxng-timeout-sec",
        "SEARXNG_TIMEOUT_SEC",
        DEFAULT_SEARXNG_TIMEOUT_SEC,
    );
    sources.insert("SEARXNG_TIMEOUT_SEC".to_string(), src);
    c.searxng_timeout_sec = n;
    let (n, src) = r.int(
        "fetch-concurrency",
        "FETCH_CONCURRENCY",
        DEFAULT_FETCH_CONCURRENCY,
    );
    sources.insert("FETCH_CONCURRENCY".to_string(), src);
    c.fetch_concurrency = n;
    let (n, src) = r.int(
        "fetch-timeout-sec",
        "FETCH_TIMEOUT_SEC",
        DEFAULT_FETCH_TIMEOUT_SEC,
    );
    sources.insert("FETCH_TIMEOUT_SEC".to_string(), src);
    c.fetch_timeout_sec = n;
    let (n, src) = r.int("pdf-max-pages", "PDF_MAX_PAGES", DEFAULT_PDF_MAX_PAGES);
    sources.insert("PDF_MAX_PAGES".to_string(), src);
    c.pdf_max_pages = n;
    let (n, src) = r.int("pdf-dpi", "PDF_DPI", DEFAULT_PDF_DPI);
    sources.insert("PDF_DPI".to_string(), src);
    c.pdf_dpi = n;
    let (n, src) = r.int(
        "run-command-timeout-sec",
        "RUN_COMMAND_TIMEOUT_SEC",
        DEFAULT_RUN_COMMAND_TIMEOUT_SEC,
    );
    sources.insert("RUN_COMMAND_TIMEOUT_SEC".to_string(), src);
    c.run_command_timeout_sec = n;
    let (n, src) = r.int("history-size", "HISTORY_SIZE", DEFAULT_HISTORY_SIZE);
    sources.insert("HISTORY_SIZE".to_string(), src);
    c.history_size = n;
    let (n, src) = r.int(
        "compact-headroom-percent",
        "COMPACT_HEADROOM_PERCENT",
        DEFAULT_COMPACT_HEADROOM_PERCENT,
    );
    sources.insert("COMPACT_HEADROOM_PERCENT".to_string(), src);
    c.compact_headroom_percent = n;
    let (n, src) = r.int(
        "compact-target-percent",
        "COMPACT_TARGET_PERCENT",
        DEFAULT_COMPACT_TARGET_PERCENT,
    );
    sources.insert("COMPACT_TARGET_PERCENT".to_string(), src);
    c.compact_target_percent = n;
    let (n, src) = r.int(
        "compact-keep-recent",
        "COMPACT_KEEP_RECENT",
        DEFAULT_COMPACT_KEEP_RECENT,
    );
    sources.insert("COMPACT_KEEP_RECENT".to_string(), src);
    c.compact_keep_recent = n;
    let (n, src) = r.int(
        "summary-transcript-cap",
        "SUMMARY_TRANSCRIPT_CAP",
        DEFAULT_SUMMARY_TRANSCRIPT_CAP,
    );
    sources.insert("SUMMARY_TRANSCRIPT_CAP".to_string(), src);
    c.summary_transcript_cap = n;
    let (n, src) = r.int(
        "max-tool-result-chars",
        "MAX_TOOL_RESULT_CHARS",
        DEFAULT_MAX_TOOL_RESULT_CHARS,
    );
    sources.insert("MAX_TOOL_RESULT_CHARS".to_string(), src);
    c.max_tool_result_chars = n;
    let (n, src) = r.int(
        "max-tool-calls-per-turn",
        "MAX_TOOL_CALLS_PER_TURN",
        DEFAULT_MAX_TOOL_CALLS_PER_TURN,
    );
    sources.insert("MAX_TOOL_CALLS_PER_TURN".to_string(), src);
    c.max_tool_calls_per_turn = n;
    let (v, src) = r.string(
        "watch-magic-word",
        "WATCH_MAGIC_WORD",
        DEFAULT_WATCH_MAGIC_WORD,
    );
    sources.insert("WATCH_MAGIC_WORD".to_string(), src);
    c.watch_magic_word = v;
    let (n, src) = r.int(
        "watch-poll-interval-sec",
        "WATCH_POLL_INTERVAL_SEC",
        DEFAULT_WATCH_POLL_INTERVAL_SEC,
    );
    sources.insert("WATCH_POLL_INTERVAL_SEC".to_string(), src);
    c.watch_poll_interval_sec = n;
    let (v, src) = r.string("ntfy-url", "NTFY_URL", DEFAULT_NTFY_URL);
    sources.insert("NTFY_URL".to_string(), src);
    c.ntfy_url = v;
    // The proxy pair: `no-proxy` is stored as given (its own default lists the
    // loopback hosts) and `socks-proxy` is validated + normalised through
    // ProxyConfig, so a bad URL is a config error here rather than a failed
    // model request later.
    let (v, src) = r.string("no-proxy", "NO_PROXY", DEFAULT_NO_PROXY);
    sources.insert("NO_PROXY".to_string(), src);
    c.no_proxy = v.trim().to_string();
    let (v, src) = r.string("socks-proxy", "SOCKS_PROXY", "");
    sources.insert("SOCKS_PROXY".to_string(), src);
    let proxy = crate::net::ProxyConfig::parse(&v, &c.no_proxy)?;
    c.socks_proxy = crate::net::setting_value(&proxy);
    let (v, src) = r.string("tool-mode", "TOOL_MODE", DEFAULT_TOOL_CALL_MODE);
    sources.insert("TOOL_MODE".to_string(), src);
    c.set_tool_call_mode(&v)?;
    let (b, src) = r.bool("thinking", "THINKING", DEFAULT_THINKING);
    sources.insert("THINKING".to_string(), src);
    c.thinking = b;

    let (n, src) = r.int("web", "WEB_PORT", DEFAULT_WEB_PORT);
    sources.insert("WEB_PORT".to_string(), src);
    c.web_port = n.clamp(0, 65_535);
    let (skill_path_str, src) = r.string("skill-path", "SKILL_PATH", "");
    sources.insert("SKILL_PATH".to_string(), src);
    if !skill_path_str.is_empty() {
        c.skill_paths = split_paths(&skill_path_str);
    }
    if let Some(src) = env_file_source {
        sources.insert("ENV_FILE".to_string(), src);
    }

    // Moved in last: the resolver above borrows it.
    c.dotenv = dotenv.unwrap_or_else(Dotenv::empty);
    c.sources = sources;

    Ok((c, warnings))
}

/// Splits a `;`-separated path list (the `-skill-path` / `JONNYQ_SKILL_PATH`
/// form), trimming blanks and dropping empty entries. `;` is the only
/// separator on purpose: `:` would mangle `C:\skills` style Windows paths.
fn split_paths(s: &str) -> Vec<String> {
    s.split(';')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Builds a Config from CLI args, the dotenv file the selector names and the
/// process environment, with flags taking precedence over the file and the
/// file taking precedence over environment variables. `args` excludes the
/// program name.
///
/// Non-fatal warnings (bad lines in the dotenv file, etc.) are printed to
/// stderr; use [`load_with`] to collect them instead.
pub fn load(args: &[String]) -> R<Config> {
    let (cfg, warnings) = load_with(args)?;
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    Ok(cfg)
}

/// [`load`] without the stderr output: also returns the warnings it printed.
pub fn load_with(args: &[String]) -> R<(Config, Vec<String>)> {
    build(args, None)
}

/// Rebuilds a Config from a dotenv selector, replaying the CLI args recorded
/// at startup so flags keep beating the file, and the file still beats the
/// process environment as well. Used by the `/env` slash command.
pub fn reload_with(selector: &str, startup_args: &[String]) -> R<(Config, Vec<String>)> {
    let (mut c, warnings) = build(startup_args, Some(selector))?;
    // Keep the original command line so a later /env reload replays it too.
    c.startup_args = startup_args.to_vec();
    Ok((c, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// load() reads process env, so tests that assert env/dotenv-dependent
    /// behaviour use the shared crate::testsupport::clean_env() guard.
    use crate::testsupport::clean_env;

    /// load() with a clean environment *and* no dotenv file - what the Go
    /// test gets from `go test` in a shell that has no JONNYQ_* set. Without
    /// `-env-file off` a developer's own ./JONNYQ_env file (or `.env`, which
    /// the README tells users to create) would leak into these assertions.
    fn load_clean(args: &[String]) -> Config {
        let _g = clean_env();
        load(&with_off(args)).unwrap()
    }

    /// Prepends `-env-file off` so no `.env` in the crate directory can
    /// influence the result.
    fn with_off(args: &[String]) -> Vec<String> {
        let mut a = vec!["-env-file".to_string(), "off".to_string()];
        a.extend_from_slice(args);
        a
    }

    /// load() with a clean environment and a caller-chosen dotenv selector
    /// (a temp file path, or `off`).
    fn load_dotenv_off(args: &[String], env_file: &str) -> Config {
        let _g = clean_env();
        let mut a = vec!["-env-file".to_string(), env_file.to_string()];
        a.extend_from_slice(args);
        load(&a).unwrap()
    }

    /// Writes a dotenv file into a throwaway directory and returns its path.
    fn temp_dotenv(tag: &str, body: &str) -> (TempEnvFile, String) {
        let dir = std::env::temp_dir().join(format!("jonnyq-cfg-{}-{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.env");
        std::fs::write(&path, body).unwrap();
        let p = path.to_string_lossy().into_owned();
        (TempEnvFile(dir), p)
    }

    struct TempEnvFile(std::path::PathBuf);
    impl Drop for TempEnvFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn load_defaults() {
        let cfg = load_clean(&[]);
        assert_eq!(cfg.provider_name, "ollama");
        assert_eq!(cfg.provider_url, "http://localhost:11434");
        assert_eq!(cfg.context_size, DEFAULT_CONTEXT_SIZE);
        assert!(cfg.thinking);
        assert_eq!(cfg.run_command_timeout_sec, DEFAULT_RUN_COMMAND_TIMEOUT_SEC);
        assert_eq!(cfg.max_tool_calls_per_turn, DEFAULT_MAX_TOOL_CALLS_PER_TURN);
        assert_eq!(cfg.watch_magic_word, DEFAULT_WATCH_MAGIC_WORD);
        assert_eq!(cfg.output_file, DEFAULT_OUTPUT_FILE);
        assert!(cfg.skill_paths.is_empty());
        assert_eq!(cfg.tool_call_mode, DEFAULT_TOOL_CALL_MODE);
        // Nothing loaded, and nothing was asked for.
        assert_eq!(cfg.dotenv_path(), None);
        assert_eq!(cfg.dotenv_setting_count(), 0);
    }

    #[test]
    fn max_tool_calls_flag_and_env() {
        let cfg = load_dotenv_off(
            &["-max-tool-calls-per-turn".to_string(), "10".to_string()],
            "off",
        );
        assert_eq!(cfg.max_tool_calls_per_turn, 10);

        let _g = clean_env();
        _g.set("JONNYQ_MAX_TOOL_CALLS_PER_TURN", "25");
        let cfg = load(&with_off(&[])).unwrap();
        assert_eq!(cfg.max_tool_calls_per_turn, 25);
    }

    #[test]
    fn env_overrides_default() {
        let _g = clean_env();
        _g.set("JONNYQ_MODEL", "llama3");
        _g.set("JONNYQ_CONTEXT_SIZE", "8192");
        let cfg = load(&with_off(&[])).unwrap();
        assert_eq!(cfg.model, "llama3");
        assert_eq!(cfg.context_size, 8192);
    }

    #[test]
    fn flag_overrides_env() {
        let _g = clean_env();
        _g.set("JONNYQ_MODEL", "llama3");
        let cfg = load(&with_off(&["-model".to_string(), "qwen".to_string()])).unwrap();
        assert_eq!(cfg.model, "qwen");
    }

    #[test]
    fn skill_path_splitting() {
        let cfg = load_clean(&["-skill-path".to_string(), "/a/skills;/b/skills".to_string()]);
        assert_eq!(
            cfg.skill_paths,
            vec!["/a/skills".to_string(), "/b/skills".to_string()]
        );
        assert_eq!(
            split_paths(" /a ;; /b ;"),
            vec!["/a".to_string(), "/b".to_string()]
        );
        // ';' only: ':' must survive inside a path (Windows drive letters).
        assert_eq!(split_paths("C:/skills"), vec!["C:/skills".to_string()]);
    }

    #[test]
    fn invalid_provider_rejected() {
        assert!(load(&with_off(&["-provider".to_string(), "bogus".to_string()])).is_err());
        assert!(load(&with_off(&[
            "-provider".to_string(),
            "azure:http://x".to_string()
        ]))
        .is_err());
        assert!(load(&with_off(&[
            "-provider".to_string(),
            "http://x".to_string()
        ]))
        .is_err());
    }

    #[test]
    fn unknown_flag_rejected() {
        // Go: "flag provided but not defined: -nope"
        assert!(load(&with_off(&["-nope".to_string()])).is_err());
    }

    #[test]
    fn missing_flag_value_rejected() {
        // Go: "flag needs an argument: -model"
        assert!(load(&with_off(&["-model".to_string()])).is_err());
    }

    #[test]
    fn positional_argument_stops_parsing_like_go() {
        // Go's flag package stops at the first non-flag argument; everything
        // after it is ignored (jonnyq never reads flag.Args()).
        let cfg = load_clean(&[
            "something".to_string(),
            "-model".to_string(),
            "ignored".to_string(),
        ]);
        assert_eq!(cfg.model, "");
    }

    #[test]
    fn tool_mode_defaults_native() {
        assert_eq!(load_clean(&[]).tool_call_mode, DEFAULT_TOOL_CALL_MODE);
    }

    // --- SOCKS proxy -------------------------------------------------------

    #[test]
    fn proxy_defaults_to_unset_with_a_loopback_bypass() {
        let cfg = load_clean(&[]);
        // Unset = "let the environment decide", not "connect directly".
        assert_eq!(cfg.socks_proxy, "");
        assert_eq!(
            cfg.proxy_config_or_default().mode,
            crate::net::ProxyMode::System
        );
        // Loopback is bypassed by default so a local model server is never
        // tunnelled by accident.
        assert_eq!(cfg.no_proxy, DEFAULT_NO_PROXY);
        assert_eq!(cfg.proxy_config_or_default().no_proxy, DEFAULT_NO_PROXY);
    }

    #[test]
    fn socks_proxy_flag_normalizes_and_applies_to_every_layer() {
        // A bare host:port is shorthand for socks5h (remote DNS), stored
        // normalised so /env list and a reload show what is in effect.
        let cfg = load_clean(&["-socks-proxy".to_string(), "127.0.0.1:1080".to_string()]);
        assert_eq!(cfg.socks_proxy, "socks5h://127.0.0.1:1080");
        assert!(cfg.proxy_config_or_default().enabled());
        assert_eq!(cfg.proxy_config_or_default().no_proxy, DEFAULT_NO_PROXY);

        // The bypass list is a setting of its own.
        let cfg = load_clean(&[
            "-socks-proxy".to_string(),
            "socks5://proxy:1080".to_string(),
            "-no-proxy".to_string(),
            "intranet,10.0.0.0/8".to_string(),
        ]);
        assert_eq!(cfg.socks_proxy, "socks5://proxy:1080");
        assert_eq!(cfg.no_proxy, "intranet,10.0.0.0/8");
        assert_eq!(
            cfg.proxy_config_or_default().describe(),
            "socks5://proxy:1080 (direct for: intranet,10.0.0.0/8)"
        );

        // off means off, and it survives the round trip through the stored value.
        let cfg = load_clean(&["-socks-proxy".to_string(), "off".to_string()]);
        assert_eq!(cfg.socks_proxy, "off");
        assert_eq!(
            cfg.proxy_config_or_default().mode,
            crate::net::ProxyMode::Direct
        );
        // ...as does auto/system, which hands the decision back to env vars.
        let cfg = load_clean(&["-socks-proxy".to_string(), "auto".to_string()]);
        assert_eq!(cfg.socks_proxy, "");
        assert_eq!(
            cfg.proxy_config_or_default().mode,
            crate::net::ProxyMode::System
        );
    }

    #[test]
    fn socks_proxy_env_and_dotenv() {
        let _g = clean_env();
        _g.set("JONNYQ_SOCKS_PROXY", "socks5h://env-proxy:1080");
        _g.set("JONNYQ_NO_PROXY", "internal.example");
        let cfg = load(&with_off(&[])).unwrap();
        assert_eq!(cfg.socks_proxy, "socks5h://env-proxy:1080");
        assert_eq!(cfg.no_proxy, "internal.example");
        // The flag still wins.
        let cfg = load(&with_off(&["-socks-proxy".to_string(), "off".to_string()])).unwrap();
        assert_eq!(
            cfg.proxy_config_or_default().mode,
            crate::net::ProxyMode::Direct
        );

        let (_d, path) = temp_dotenv(
            "proxy",
            "JONNYQ_SOCKS_PROXY=socks4a://file-proxy:1080\nJONNYQ_NO_PROXY=\n",
        );
        let cfg = load(&["-env-file".to_string(), path]).unwrap();
        assert_eq!(cfg.socks_proxy, "socks4a://file-proxy:1080");
        // An empty bypass list in the file is a decision, not a default: with a
        // proxy set, everything (loopback included) is tunnelled.
        assert_eq!(cfg.no_proxy, "");
        assert!(!cfg
            .proxy_config_or_default()
            .describe()
            .contains("direct for"));
    }

    #[test]
    fn invalid_socks_proxy_is_a_config_error() {
        // Same class of failure as a malformed -provider: refuse at startup
        // rather than failing the first model request.
        for bad in [
            "http://proxy:3128",
            "ftp://x:21",
            "socks5://proxy:99999",
            "socks5://",
        ] {
            let e = load(&with_off(&["-socks-proxy".to_string(), bad.to_string()])).unwrap_err();
            assert!(
                e.message.contains("SOCKS")
                    || e.message.contains("scheme")
                    || e.message.contains("port")
                    || e.message.contains("host"),
                "{bad}: {}",
                e.message
            );
        }
        // A bad value in the file fails the same way.
        let (_d, path) = temp_dotenv("bad-proxy", "JONNYQ_SOCKS_PROXY=http://proxy:3128\n");
        let e = load_dotenv_off_err(&path);
        assert!(e.message.contains("not a SOCKS proxy"), "{}", e.message);
    }

    #[test]
    fn proxy_credentials_never_reach_the_settings_table() {
        // /env list prints values, so the row must not carry a password.
        let cfg = load_clean(&[
            "-socks-proxy".to_string(),
            "socks5h://u:sup3rs3cr3t@proxy:1080".to_string(),
        ]);
        assert_eq!(
            cfg.socks_proxy, "socks5h://u:sup3rs3cr3t@proxy:1080",
            "the value itself is kept"
        );
        let row = cfg
            .settings()
            .into_iter()
            .find(|s| s.flag == "socks-proxy")
            .unwrap();
        assert_eq!(row.display(), "socks5h://u:***@proxy:1080");
    }

    #[test]
    fn tool_mode_flag_and_env() {
        let cfg = load_dotenv_off(&["-tool-mode".to_string(), "prompt".to_string()], "off");
        assert_eq!(cfg.tool_call_mode, "prompt");

        let _g = clean_env();
        _g.set("JONNYQ_TOOL_MODE", "prompt");
        assert_eq!(load(&with_off(&[])).unwrap().tool_call_mode, "prompt");
        // flag beats env
        assert_eq!(
            load(&with_off(&["-tool-mode".to_string(), "native".to_string()]))
                .unwrap()
                .tool_call_mode,
            "native"
        );
    }

    #[test]
    fn tool_mode_case_insensitive() {
        let cfg = load_clean(&["-tool-mode".to_string(), "PROMPT".to_string()]);
        assert_eq!(cfg.tool_call_mode, "prompt");
    }

    #[test]
    fn invalid_tool_mode_rejected() {
        assert!(load(&with_off(&["-tool-mode".to_string(), "bogus".to_string()])).is_err());
    }

    #[test]
    fn thinking_flag_forms() {
        // -thinking=true / -thinking=false set it; a bare -thinking means
        // true. Go's flag package does NOT let a bool flag consume the next
        // argument, so "-thinking false" leaves thinking at its default and
        // pushes "false" into the (unread) positional args.
        assert!(!load_clean(&["-thinking=false".to_string()]).thinking);
        assert!(load_clean(&["-thinking=true".to_string()]).thinking);
        assert!(load_clean(&["-thinking".to_string()]).thinking);
        assert!(load_clean(&["-thinking".to_string(), "false".to_string()]).thinking);
    }

    #[test]
    fn provider_flag_sets_both_name_and_url() {
        let cfg = load_clean(&[
            "-provider".to_string(),
            "openai:https://api.openai.com/v1".to_string(),
        ]);
        assert_eq!(cfg.provider_name, "openai");
        assert_eq!(cfg.provider_url, "https://api.openai.com/v1");
    }

    // --- dotenv file ------------------------------------------------------

    #[test]
    fn dotenv_file_supplies_settings() {
        let (_d, path) = temp_dotenv(
            "basic",
            concat!(
                "# a comment\n",
                "JONNYQ_MODEL=from-file\n",
                "JONNYQ_CONTEXT_SIZE=32768\n",
                "JONNYQ_THINKING=false\n",
                "JONNYQ_TOOL_MODE=prompt\n",
                "JONNYQ_SKILL_PATH=/a/skills;/b/skills\n",
                "SOMETHING_ELSE=ignored\n",
            ),
        );
        let cfg = load_dotenv_off(&[], &path);
        assert_eq!(cfg.model, "from-file");
        assert_eq!(cfg.context_size, 32768);
        assert!(!cfg.thinking);
        assert_eq!(cfg.tool_call_mode, "prompt");
        assert_eq!(
            cfg.skill_paths,
            vec!["/a/skills".to_string(), "/b/skills".to_string()]
        );
        // Only JONNYQ_* keys count towards the summary.
        assert_eq!(cfg.dotenv_setting_count(), 5);
        assert_eq!(cfg.dotenv_path().as_deref(), Some(path.as_str()));
        assert_eq!(cfg.env_file, path);
        assert!(
            cfg.dotenv_summary().contains("settings.env"),
            "{}",
            cfg.dotenv_summary()
        );
    }

    #[test]
    fn dotenv_missing_value_yields_empty_string() {
        // `JONNYQ_MODEL=` means "set, but empty" - the same as exporting an
        // empty variable, so the model stays unset.
        let (_d, path) = temp_dotenv("empty", "JONNYQ_MODEL=\n");
        let cfg = load_dotenv_off(&[], &path);
        assert_eq!(cfg.model, "");
    }

    #[test]
    fn dotenv_file_beats_env_var() {
        // clean_env(): an ambient JONNYQ_CONTEXT_SIZE would be reported as the
        // source layer for that row (which is exactly the precedence this test
        // is about, and the fixture does not set it).
        let _g = clean_env();
        let (_d, path) = temp_dotenv(
            "precedence",
            "JONNYQ_MODEL=from-file\nJONNYQ_CONTEXT_SIZE=111\n",
        );
        _g.set("JONNYQ_MODEL", "from-env");
        let cfg = load(&["-env-file".to_string(), path.clone()]).unwrap();
        assert_eq!(
            cfg.model, "from-file",
            "the file must win over the process env"
        );
        // ...and the environment still fills in what the file does not set.
        _g.set("JONNYQ_NTFY_URL", "https://env-only/x");
        let cfg = load(&["-env-file".to_string(), path]).unwrap();
        assert_eq!(cfg.ntfy_url, "https://env-only/x");
        assert_eq!(cfg.context_size, 111);
    }

    #[test]
    fn empty_value_in_dotenv_still_beats_env_var() {
        // `JONNYQ_MODEL=` in the file means "set, but empty" - it outranks an
        // exported value, the same way an exported empty variable outranks an
        // unset one.
        let _g = clean_env();
        let (_d, path) = temp_dotenv("empty-beats-env", "JONNYQ_MODEL=\n");
        _g.set("JONNYQ_MODEL", "from-env");
        let cfg = load(&["-env-file".to_string(), path]).unwrap();
        assert_eq!(cfg.model, "");
        let row = cfg
            .settings()
            .into_iter()
            .find(|s| s.flag == "model")
            .unwrap();
        assert_eq!(row.source, Source::File);
    }

    #[test]
    fn env_var_used_when_file_is_silent() {
        // A key the file does not mention at all still comes from the
        // environment - the file is a layer, not a replacement for env vars.
        let _g = clean_env();
        let (_d, path) = temp_dotenv("partial", "JONNYQ_MODEL=from-file\n");
        _g.set("JONNYQ_CONTEXT_SIZE", "222");
        let cfg = load(&["-env-file".to_string(), path]).unwrap();
        assert_eq!(cfg.model, "from-file");
        assert_eq!(cfg.context_size, 222);
        let row = cfg
            .settings()
            .into_iter()
            .find(|s| s.flag == "context")
            .unwrap();
        assert_eq!(row.source, Source::Env);
    }

    #[test]
    fn flag_beats_dotenv_file_beats_env_var() {
        let (_d, path) = temp_dotenv("all-three", "JONNYQ_MODEL=from-file\n");
        let _g = clean_env();
        _g.set("JONNYQ_MODEL", "from-env");
        let cfg = load(&[
            "-env-file".to_string(),
            path,
            "-model".to_string(),
            "from-flag".to_string(),
        ])
        .unwrap();
        assert_eq!(cfg.model, "from-flag");
    }

    #[test]
    fn env_file_selected_by_env_var() {
        // clean_env(): an ambient JONNYQ_MODEL would outrank the file.
        let _g = clean_env();
        let (_d, path) = temp_dotenv("via-env", "JONNYQ_MODEL=via-env-var\n");
        _g.set("JONNYQ_ENV_FILE", &path);
        let cfg = load(&[]).unwrap();
        assert_eq!(cfg.model, "via-env-var");
    }

    #[test]
    fn env_file_off_disables_dotenv() {
        let (_d, path) = temp_dotenv("disable", "JONNYQ_MODEL=from-file\n");
        // Explicitly pointing at the file works...
        assert_eq!(load_dotenv_off(&[], &path).model, "from-file");
        // ...and 'off' (also 'none'/'no') reads nothing at all.
        assert_eq!(load_dotenv_off(&[], "off").model, "");
        assert_eq!(load_dotenv_off(&[], "NONE").model, "");
        assert_eq!(load_dotenv_off(&[], "no").model, "");
        let cfg = load_dotenv_off(&[], "off");
        assert!(
            cfg.dotenv_summary().contains("disabled"),
            "{}",
            cfg.dotenv_summary()
        );
    }

    #[test]
    fn missing_env_file_is_an_error() {
        let _g = clean_env();
        let e = load(&[
            "-env-file".to_string(),
            "/nonexistent/jonnyq-settings.env".to_string(),
        ])
        .unwrap_err();
        assert!(e.message.contains("does not exist"), "{}", e.message);
    }

    #[test]
    fn bad_dotenv_lines_warn_without_failing() {
        // One clean_env() per test: the lock it holds is not reentrant.
        let _g = clean_env();
        let (_d, path) = temp_dotenv("warn", "this line has no equals sign\nJONNYQ_MODEL=ok\n");
        let (cfg, warnings) = load_with(&["-env-file".to_string(), path.clone()]).unwrap();
        assert_eq!(cfg.model, "ok");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("without '='"), "{:?}", warnings[0]);

        // An explicitly named file full of unrelated keys gets a hint.
        let (_d2, p2) = temp_dotenv("warn2", "OTHER_APP=1\n");
        let (_, warnings) = load_with(&["-env-file".to_string(), p2]).unwrap();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("defines no JONNYQ_* settings")),
            "{warnings:?}"
        );
        // ...while a disabled selector with no file at all stays quiet.
        let (cfg, warnings) = build(&[], Some("off")).unwrap();
        assert_eq!(cfg.model, "");
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn invalid_dotenv_value_is_still_tolerated_like_env_vars() {
        // A non-numeric JONNYQ_CONTEXT_SIZE falls back to the default, exactly
        // as a bad environment variable does.
        let (_d, path) = temp_dotenv("bad-int", "JONNYQ_CONTEXT_SIZE=not-a-number\n");
        let cfg = load_dotenv_off(&[], &path);
        assert_eq!(cfg.context_size, DEFAULT_CONTEXT_SIZE);
    }

    #[test]
    fn invalid_tool_mode_in_dotenv_is_rejected() {
        let (_d, path) = temp_dotenv("bad-mode", "JONNYQ_TOOL_MODE=magic\n");
        let e = load_dotenv_off_err(&path);
        assert!(e.message.contains("invalid tool-mode"), "{}", e.message);
    }

    fn load_dotenv_off_err(path: &str) -> AgentError {
        let _g = clean_env();
        load(&["-env-file".to_string(), path.to_string()]).unwrap_err()
    }

    #[test]
    fn reload_replays_startup_flags_and_swaps_the_file() {
        let _g = clean_env();
        let (_d1, p1) = temp_dotenv("r1", "JONNYQ_MODEL=first\nJONNYQ_CONTEXT_SIZE=1000\n");
        let (_d2, p2) = temp_dotenv("r2", "JONNYQ_MODEL=second\nJONNYQ_CONTEXT_SIZE=2000\n");
        let startup = vec!["-context".to_string(), "4242".to_string()];

        let (c1, w) = reload_with(&p1, &startup).unwrap();
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c1.model, "first");
        // The startup flag keeps beating the file, before and after reload.
        assert_eq!(c1.context_size, 4242);

        let (c2, _) = reload_with(&p2, &startup).unwrap();
        assert_eq!(c2.model, "second");
        assert_eq!(c2.context_size, 4242);
        assert_eq!(c2.dotenv_path().as_deref(), Some(p2.as_str()));

        // 'off' drops back to defaults/flags instead of the previous file.
        let (c3, _) = reload_with("off", &startup).unwrap();
        assert_eq!(c3.model, "");
        assert_eq!(c3.dotenv_path(), None);
        // The recorded command line survives a reload.
        assert_eq!(c3.startup_args, startup);
    }

    #[test]
    fn dotenv_selector_off_is_reported_by_summary() {
        let cfg = load_dotenv_off(&[], "off");
        assert!(!is_disabled_selector(""));
        assert!(is_disabled_selector(" off "));
        assert!(is_disabled_selector("NONE"));
        assert_eq!(cfg.dotenv_path(), None);
    }

    // --- provenance (what /env list prints) -------------------------------

    #[test]
    fn sources_report_the_winning_layer() {
        let _g = clean_env();
        let (_d, path) = temp_dotenv(
            "src",
            "JONNYQ_MODEL=from-file\nJONNYQ_CONTEXT_SIZE=4096\nJONNYQ_NTFY_URL=https://x/y\n",
        );
        _g.set("JONNYQ_MODEL", "from-env");
        _g.set("JONNYQ_WATCH_MAGIC_WORD", "ENV!");
        let cfg = load(&[
            "-env-file".to_string(),
            path,
            "-ntfy-url".to_string(),
            "https://flag/z".to_string(),
        ])
        .unwrap();

        let src = |env: &str| cfg.src(env);
        assert_eq!(src("MODEL"), Source::File, "the file outranks an env var");
        assert_eq!(src("CONTEXT_SIZE"), Source::File);
        assert_eq!(src("NTFY_URL"), Source::Flag);
        assert_eq!(
            src("WATCH_MAGIC_WORD"),
            Source::Env,
            "a key the file omits comes from env"
        );
        assert_eq!(src("PDF_DPI"), Source::Default);
        assert_eq!(src("ENV_FILE"), Source::Flag);

        let find = |flag: &str| cfg.settings().into_iter().find(|s| s.flag == flag).unwrap();
        assert_eq!(find("model").source, Source::File);
        assert_eq!(find("model").display(), "from-file");
        assert_eq!(find("context").source, Source::File);
        // A secret is never printed in the clear.
        let (_d2, p2) = temp_dotenv("src-key", "JONNYQ_KEY=sk-super-secret\n");
        let cfg2 = build(&["-env-file".to_string(), p2], None).unwrap().0;
        assert_eq!(cfg2.key, "sk-super-secret");
        let key_row = cfg2
            .settings()
            .into_iter()
            .find(|s| s.flag == "key")
            .unwrap();
        assert_eq!(key_row.display(), "****");
        assert_eq!(key_row.source, Source::File);
        // Unset strings show as "(unset)" rather than a blank cell.
        let out_row = cfg
            .settings()
            .into_iter()
            .find(|s| s.flag == "key")
            .unwrap();
        assert_eq!(out_row.display(), "(unset)");
    }

    #[test]
    fn settings_table_covers_every_documented_setting() {
        let _g = clean_env();
        let (cfg, _) = build(&[], Some("off")).unwrap();
        let rows = cfg.settings();
        // Snapshot of the documented flag/env/meaning table (README section 5)
        // - catches a row being dropped or an env var name drifting.
        let got: Vec<String> = rows
            .iter()
            .map(|r| format!("{} {}", r.flag, r.env))
            .collect();
        let want = vec![
            "provider JONNYQ_PROVIDER",
            "key JONNYQ_KEY",
            "model JONNYQ_MODEL",
            "context JONNYQ_CONTEXT_SIZE",
            "output JONNYQ_OUTPUT_FILE",
            "thinking JONNYQ_THINKING",
            "searxng-url JONNYQ_SEARXNG_URL",
            "searxng-max-results JONNYQ_SEARXNG_MAX_RESULTS",
            "searxng-concurrency JONNYQ_SEARXNG_CONCURRENCY",
            "searxng-timeout-sec JONNYQ_SEARXNG_TIMEOUT_SEC",
            "fetch-concurrency JONNYQ_FETCH_CONCURRENCY",
            "fetch-timeout-sec JONNYQ_FETCH_TIMEOUT_SEC",
            "pdf-max-pages JONNYQ_PDF_MAX_PAGES",
            "pdf-dpi JONNYQ_PDF_DPI",
            "run-command-timeout-sec JONNYQ_RUN_COMMAND_TIMEOUT_SEC",
            "history-size JONNYQ_HISTORY_SIZE",
            "compact-headroom-percent JONNYQ_COMPACT_HEADROOM_PERCENT",
            "compact-target-percent JONNYQ_COMPACT_TARGET_PERCENT",
            "compact-keep-recent JONNYQ_COMPACT_KEEP_RECENT",
            "summary-transcript-cap JONNYQ_SUMMARY_TRANSCRIPT_CAP",
            "max-tool-result-chars JONNYQ_MAX_TOOL_RESULT_CHARS",
            "max-tool-calls-per-turn JONNYQ_MAX_TOOL_CALLS_PER_TURN",
            "watch-magic-word JONNYQ_WATCH_MAGIC_WORD",
            "watch-poll-interval-sec JONNYQ_WATCH_POLL_INTERVAL_SEC",
            "ntfy-url JONNYQ_NTFY_URL",
            "socks-proxy JONNYQ_SOCKS_PROXY",
            "no-proxy JONNYQ_NO_PROXY",
            "tool-mode JONNYQ_TOOL_MODE",
            "skill-path JONNYQ_SKILL_PATH",
            "web JONNYQ_WEB_PORT",
            "env-file JONNYQ_ENV_FILE",
        ];
        assert_eq!(got, want, "settings() drifted from the documented table");

        // Every flag in the table is accepted on the command line, and every
        // env name is the prefix plus a valid dotenv key.
        for r in &rows {
            assert!(
                KNOWN_FLAGS.contains(&r.flag),
                "-{} not accepted as a flag",
                r.flag
            );
            assert!(
                dotenv::valid_key(r.env.strip_prefix(ENV_PREFIX).unwrap()),
                "bad env name {}",
                r.env
            );
        }
        // Defaults are reported as defaults.
        let ctx = rows.into_iter().find(|r| r.flag == "context").unwrap();
        assert_eq!(ctx.source, Source::Default);
        assert_eq!(ctx.value, DEFAULT_CONTEXT_SIZE.to_string());
    }

    #[test]
    fn env_file_selector_is_reported_as_its_own_row_source() {
        let _g = clean_env();
        let (_d, path) = temp_dotenv("envrow", "JONNYQ_MODEL=x\n");
        // Via the flag -> "flag".
        let (cfg, _) = build(&["-env-file".to_string(), path.clone()], None).unwrap();
        let row = cfg
            .settings()
            .into_iter()
            .find(|r| r.flag == "env-file")
            .unwrap();
        assert_eq!(row.env, "JONNYQ_ENV_FILE");
        assert_eq!(row.source, Source::Flag);
        assert_eq!(row.display(), path);
        // Via the environment variable -> "env".
        _g.set("JONNYQ_ENV_FILE", &path);
        let (cfg, _) = build(&[], None).unwrap();
        let row = cfg
            .settings()
            .into_iter()
            .find(|r| r.flag == "env-file")
            .unwrap();
        assert_eq!(row.source, Source::Env);
        // A selector that disables dotenv is still a deliberate choice, so it
        // is reported like a flag rather than a default.
        let (cfg, _) = build(&[], Some("off")).unwrap();
        let row = cfg
            .settings()
            .into_iter()
            .find(|r| r.flag == "env-file")
            .unwrap();
        assert_eq!(row.source, Source::Flag);
        assert_eq!(row.display(), "off");
        // A plain default Config (nothing named a file) is "default".
        let row = Config::default()
            .settings()
            .into_iter()
            .find(|r| r.flag == "env-file")
            .unwrap();
        assert_eq!(row.source, Source::Default);
    }
}
