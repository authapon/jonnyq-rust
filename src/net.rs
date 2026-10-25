//! Outbound HTTP: the shared reqwest clients and the SOCKS proxy setting.
//!
//! Every request jonnyq makes - the model itself (ollama / OpenAI-compatible),
//! `web_search`, `web_fetch`, and ntfy notifications - goes through [`client`]
//! here, which is what makes the proxy setting apply to all of them at once,
//! and to *change* under a running session: the proxy lives in one
//! process-wide slot ([`set_current`] / [`current`]) and every cached client is
//! dropped when it is replaced. That is how `/proxy` and `/env reload` switch
//! the transport for the very next request without rebuilding the provider,
//! the tools or the notifier.
//!
//! Settings reach that slot from the same layers as every other setting (CLI
//! flag `-socks-proxy` > `.env` > `JONNYQ_SOCKS_PROXY` > default, see
//! [`crate::config`]) plus the `/proxy` slash command, which writes the live
//! value back into the config so `/env list` stays truthful.
//!
//! Accepted `socks-proxy` values:
//!
//! | Value | Meaning |
//! |---|---|
//! | (empty, the default) | jonnyq says nothing: reqwest follows `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` if exported |
//! | `off` / `none` / `no` | never proxy - direct connections, ignoring `ALL_PROXY` etc. |
//! | `auto` / `system` / `env` | back to following the environment (undoing `off`) |
//! | `socks5h://host:port` | SOCKS5 with the proxy resolving the hostname (`socks5`, `socks4a`, `socks4` too) |
//! | `host:port` | shorthand for `socks5h://host:port` - remote DNS is the safe default behind a proxy |
//!
//! Credentials go in the URL (`socks5h://user:pass@host:1080`); they are
//! percent-decoded by reqwest and masked in everything jonnyq prints
//! ([`ProxyConfig::redacted`]).

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use crate::error::{AgentError, R};

/// Hosts that bypass the proxy unless `-no-proxy` / `JONNYQ_NO_PROXY` says
/// otherwise.
///
/// The default exists because the most common provider is a model server on
/// the same machine (`ollama:http://localhost:11434`): sending that to a SOCKS
/// proxy is at best a wasted round trip and at worst unreachable, so loopback
/// traffic stays direct unless asked for.
pub const DEFAULT_NO_PROXY: &str = "localhost,127.0.0.1,::1";

/// Spellings that turn the proxy off entirely (the same words `-env-file`
/// accepts for "read no file").
pub const DISABLE_VALUES: &[&str] = &["off", "none", "no"];

/// Spellings that hand proxy decisions back to the environment.
pub const SYSTEM_VALUES: &[&str] = &["auto", "system", "env", "default"];

/// Schemes [`normalize_proxy_url`] accepts, and the one a bare `host:port`
/// gets.
const SOCKS_SCHEMES: &[&str] = &["socks5h", "socks5", "socks4a", "socks4"];
const DEFAULT_SCHEME: &str = "socks5h";

/// How outbound connections reach their destination.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProxyMode {
    /// Nothing configured: reqwest's own behaviour, i.e. honour
    /// `ALL_PROXY`/`HTTPS_PROXY`/`HTTP_PROXY`/`NO_PROXY` if exported.
    System,
    /// Explicitly direct - `off` wins over an exported `ALL_PROXY`.
    Direct,
    /// Everything (http + https) is routed through this SOCKS proxy.
    Socks(String),
}

/// The proxy in effect: where to tunnel, and what must stay direct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyConfig {
    pub mode: ProxyMode,
    /// Comma-separated hosts/domains/CIDRs that bypass the proxy. Used by
    /// [`ProxyMode::Socks`] only - in [`ProxyMode::System`] reqwest reads the
    /// standard `NO_PROXY` variable itself.
    pub no_proxy: String,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self::system()
    }
}

impl ProxyConfig {
    /// "jonnyq has no opinion" - follow the environment.
    pub fn system() -> Self {
        Self { mode: ProxyMode::System, no_proxy: String::new() }
    }

    /// Direct connections, whatever the environment says.
    pub fn direct() -> Self {
        Self { mode: ProxyMode::Direct, no_proxy: String::new() }
    }

    /// Routes through `url`, bypassing `no_proxy`. `url` is normalised exactly
    /// as [`ProxyConfig::parse`] would; an unusable value is an error.
    pub fn socks(url: &str, no_proxy: &str) -> R<Self> {
        let url = normalize_proxy_url(url)?;
        Ok(Self { mode: ProxyMode::Socks(url), no_proxy: no_proxy.trim().to_string() })
    }

    /// Turns the raw `socks-proxy` / `no-proxy` setting values into a config,
    /// validating the URL so a typo is a config error at startup rather than a
    /// mysterious connection failure on the first model call.
    ///
    /// `no_proxy` is kept whatever the mode is - it is part of what the user
    /// asked for and `/proxy` reports it - even though it only *bites* for
    /// [`ProxyMode::Socks`] (in [`ProxyMode::System`] reqwest reads the
    /// standard `NO_PROXY` variable instead).
    pub fn parse(raw: &str, no_proxy: &str) -> R<Self> {
        let raw = raw.trim();
        let no_proxy = no_proxy.trim().to_string();
        if raw.is_empty() {
            return Ok(Self { mode: ProxyMode::System, no_proxy });
        }
        if is_disabled(raw) {
            return Ok(Self { mode: ProxyMode::Direct, no_proxy });
        }
        if is_system(raw) {
            return Ok(Self { mode: ProxyMode::System, no_proxy });
        }
        Self::socks(raw, &no_proxy)
    }

    /// True when jonnyq itself configured a proxy.
    pub fn enabled(&self) -> bool {
        matches!(self.mode, ProxyMode::Socks(_))
    }

    /// The proxy URL in effect, or empty when there is none.
    pub fn url(&self) -> &str {
        match &self.mode {
            ProxyMode::Socks(u) => u,
            _ => "",
        }
    }

    /// The URL with any password masked - the only form that may be printed or
    /// logged. (reqwest's own `Debug` for a proxy hides credentials too; this
    /// covers jonnyq's output.)
    pub fn redacted(&self) -> String {
        match &self.mode {
            ProxyMode::Socks(url) => redact_url(url),
            _ => self.describe(),
        }
    }

    /// One-line description for `/proxy`, `/env list` and startup chatter.
    pub fn describe(&self) -> String {
        match &self.mode {
            ProxyMode::System => "not set (follows ALL_PROXY/HTTPS_PROXY if exported)".to_string(),
            ProxyMode::Direct => "off (direct connections)".to_string(),
            ProxyMode::Socks(_) => {
                let mut s = self.redacted();
                if !self.no_proxy.is_empty() {
                    s.push_str(&format!(" (direct for: {})", self.no_proxy));
                }
                s
            }
        }
    }

    /// Puts the setting on a builder: nothing for [`ProxyMode::System`],
    /// "no proxy at all" for [`ProxyMode::Direct`], the SOCKS matcher (with its
    /// bypass list) otherwise.
    pub fn apply(&self, builder: reqwest::ClientBuilder) -> R<reqwest::ClientBuilder> {
        match &self.mode {
            ProxyMode::System => Ok(builder),
            ProxyMode::Direct => Ok(builder.no_proxy()),
            ProxyMode::Socks(url) => {
                let mut proxy = reqwest::Proxy::all(url)
                    .map_err(|e| AgentError::new(format!("invalid socks-proxy {:?}: {}", url, e)))?;
                if !self.no_proxy.is_empty() {
                    proxy = proxy.no_proxy(reqwest::NoProxy::from_string(&self.no_proxy));
                }
                Ok(builder.proxy(proxy))
            }
        }
    }
}

fn is_disabled(v: &str) -> bool {
    DISABLE_VALUES.iter().any(|d| d.eq_ignore_ascii_case(v))
}

fn is_system(v: &str) -> bool {
    SYSTEM_VALUES.iter().any(|d| d.eq_ignore_ascii_case(v))
}

/// Validates/normalises a proxy value into a full URL: `host:port` becomes
/// `socks5h://host:port`, and only SOCKS schemes are accepted (an `http://`
/// proxy is a different feature with different semantics - say so plainly).
fn normalize_proxy_url(raw: &str) -> R<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AgentError::new("socks-proxy is empty"));
    }
    let (scheme, rest) = match raw.split_once("://") {
        Some((s, r)) => (s.to_lowercase(), r.to_string()),
        // No scheme: the whole value is the proxy's authority.
        None => (DEFAULT_SCHEME.to_string(), raw.to_string()),
    };
    if !SOCKS_SCHEMES.contains(&scheme.as_str()) {
        if raw.starts_with("http://") || raw.starts_with("https://") {
            return Err(AgentError::new(format!(
                "socks-proxy {:?} is not a SOCKS proxy - expected socks5h://host:port \
                 (also socks5, socks4a, socks4, or a bare host:port)",
                raw
            )));
        }
        return Err(AgentError::new(format!(
            "unsupported socks-proxy scheme {:?} - expected socks5h, socks5, socks4a or socks4 \
             (or a bare host:port, which means socks5h)",
            scheme
        )));
    }
    // Everything after the scheme must be `[@user:pass@]host[:port]`: cheap
    // sanity checks whose real work is done by reqwest when the client is
    // built, but which turn a typo into a message naming the setting.
    let hostport = rest.rsplit('@').next().unwrap_or(&rest);
    if hostport.is_empty() || hostport.split_whitespace().next() != Some(hostport) {
        return Err(AgentError::new(format!(
            "socks-proxy {:?} has no usable host - expected socks5h://host:port",
            raw
        )));
    }
    if let Some((_, port)) = hostport.rsplit_once(':') {
        if port.parse::<u16>().is_err() {
            return Err(AgentError::new(format!(
                "socks-proxy {:?} has an invalid port - expected 0-65535",
                raw
            )));
        }
    }
    Ok(format!("{}://{}", scheme, rest))
}

/// Masks the password of a proxy URL (`socks5://user:pass@host:1080` ->
/// `socks5://user:***@host:1080`). A value with no userinfo, or not a URL at
/// all, is returned unchanged - so it is safe to call on `off`, `auto` or an
/// empty setting too.
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // Cut the authority at the first '/' so only userinfo is touched.
    let split = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = (&rest[..split], &rest[split..]);
    let Some((userinfo, host)) = authority.split_once('@') else {
        return url.to_string();
    };
    let user = userinfo.split_once(':').map(|(u, _)| u).unwrap_or(userinfo);
    format!("{scheme}://{user}:***@{host}{path}")
}

// --- the process-wide proxy slot -------------------------------------------

/// Which cached client a [`client`] call wants: the request timeout and the
/// connect timeout it was asked for (in milliseconds - whole seconds come from
/// the settings, but the pool should not care who asked).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Kind {
    timeout: Option<u128>,
    connect_timeout: Option<u128>,
}

/// One pool entry: the generation it was built for (see [`Net::generation`]),
/// an id naming *this* client (a `reqwest::Client` is a wrapper around a shared
/// `Arc`, so its address cannot be compared - see [`client_with_id`]), and the
/// client.
type Pooled = (u64, u64, reqwest::Client);

/// The proxy plus the clients built from it.
///
/// One instance is shared process-wide ([`NET`]); tests that need to exercise
/// the pooling/invalidation logic build their own so they never reroute the
/// traffic of a test running in parallel.
struct Net {
    proxy: ProxyConfig,
    /// Bumped by [`Net::set`], so a pooled client knows it is stale.
    generation: u64,
    /// Counter feeding [`Pooled`]'s identity - never reused.
    next_id: u64,
    pool: HashMap<Kind, Pooled>,
}

impl Net {
    fn new() -> Self {
        Self { proxy: ProxyConfig::system(), generation: 0, next_id: 0, pool: HashMap::new() }
    }

    /// Installs `proxy` as the transport for clients built from here on,
    /// dropping every cached client. Validated first (see [`set_current`]).
    fn set(&mut self, proxy: ProxyConfig) -> R<()> {
        // Build before installing: `apply` is where reqwest re-parses the URL.
        self.build(Kind { timeout: None, connect_timeout: None }, &proxy)?;
        self.proxy = proxy;
        self.generation += 1;
        // Clients built for a proxy that is no longer in effect would otherwise
        // keep being handed out (each holding its own connection pool).
        self.pool.clear();
        Ok(())
    }

    /// The client for `kind`, built (and remembered) when the pooled one is
    /// missing or belongs to an older generation.
    fn client_for(&mut self, kind: Kind) -> (reqwest::Client, u64) {
        if let Some((gen, id, c)) = self.pool.get(&kind) {
            if *gen == self.generation {
                return (c.clone(), *id);
            }
        }
        let c = match self.build(kind, &self.proxy) {
            Ok(c) => c,
            // Unreachable in practice: [`Net::set`] only installs a config
            // that builds, and the pool is cleared whenever it changes. Fall
            // back to a plain client rather than unwinding a turn over a
            // transport that cannot be broken this way.
            Err(e) => {
                eprintln!(
                    "warning: proxy settings could not be applied ({}); connecting directly",
                    e.message
                );
                reqwest::Client::new()
            }
        };
        self.next_id += 1;
        let id = self.next_id;
        let gen = self.generation;
        self.pool.insert(kind, (gen, id, c.clone()));
        (c, id)
    }

    fn build(&self, kind: Kind, proxy: &ProxyConfig) -> R<reqwest::Client> {
        let mut b = reqwest::Client::builder();
        if let Some(ms) = kind.timeout {
            b = b.timeout(Duration::from_millis(ms as u64));
        }
        if let Some(ms) = kind.connect_timeout {
            b = b.connect_timeout(Duration::from_millis(ms as u64));
        }
        b = proxy.apply(b)?;
        b.build().map_err(|e| AgentError::new(format!("cannot build HTTP client: {}", e)))
    }
}

static NET: OnceLock<RwLock<Net>> = OnceLock::new();

fn net() -> &'static RwLock<Net> {
    NET.get_or_init(|| RwLock::new(Net::new()))
}

/// The proxy settings new clients are built from (see [`set_current`]).
pub fn current() -> ProxyConfig {
    net().read().unwrap().proxy.clone()
}

/// Makes `proxy` the transport for every client built from now on - what
/// `/proxy` and `/env reload` call to switch a live session. The config is
/// probed by building one client first, so an unusable value is reported here
/// instead of leaving the session with a broken transport.
pub fn set_current(proxy: ProxyConfig) -> R<()> {
    net().write().unwrap().set(proxy)
}

/// A reqwest client honouring the proxy settings in effect, cached per timeout
/// pair and rebuilt the first time it is used after [`set_current`].
///
/// `timeout` bounds the whole request; `connect_timeout` only the connection
/// phase (what a long-lived stream, e.g. an ntfy subscription, wants).
pub fn client(timeout: Option<Duration>, connect_timeout: Option<Duration>) -> reqwest::Client {
    client_with_id(timeout, connect_timeout).0
}

/// [`client`] plus the pool entry's identity, so a test can tell "the same
/// client came back out of the pool" from "a client was rebuilt" - the ids are
/// ours, and a caller that does not care ignores the second half of the tuple.
fn client_with_id(timeout: Option<Duration>, connect_timeout: Option<Duration>) -> (reqwest::Client, u64) {
    let kind = Kind {
        timeout: timeout.map(|d| d.as_millis()),
        connect_timeout: connect_timeout.map(|d| d.as_millis()),
    };
    // The common path takes only a read lock (a shared lookup in a RwLock
    // still serialises a writer briefly, which is all the rebuild needs).
    {
        let net = net().read().unwrap();
        if let Some((gen, id, c)) = net.pool.get(&kind) {
            if *gen == net.generation {
                return (c.clone(), *id);
            }
        }
    }
    net().write().unwrap().client_for(kind)
}

/// The value to store in `Config.socks_proxy` - the URL, `off`, or empty ("let
/// the environment decide") - so `/env list` shows exactly what was asked for
/// and a reload reproduces the same config.
pub fn setting_value(proxy: &ProxyConfig) -> String {
    match &proxy.mode {
        ProxyMode::System => String::new(),
        ProxyMode::Direct => DISABLE_VALUES[0].to_string(),
        ProxyMode::Socks(u) => u.clone(),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises the tests that touch the process-wide slot and restores "no
    /// opinion" afterwards.
    ///
    /// Only ever `Direct`/`System` go in there: a live SOCKS proxy installed
    /// globally would reroute the requests of every other test running in
    /// parallel (they all talk to stub servers on loopback). Whether a proxy
    /// really carries traffic is proven on a client built from the config
    /// directly, and pooling/invalidation on a private [`Net`] - both below.
    struct SlotGuard(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl SlotGuard {
        fn new() -> Self {
            Self(SLOT_LOCK.lock().unwrap_or_else(|e| e.into_inner()))
        }
    }
    impl Drop for SlotGuard {
        fn drop(&mut self) {
            set_current(ProxyConfig::system()).unwrap();
        }
    }

    static SLOT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Ids of the live pool entries of a `Net` (stale ones excluded).
    fn pool_ids(net: &Net) -> Vec<u64> {
        net.pool
            .values()
            .filter(|(gen, _, _)| *gen == net.generation)
            .map(|(_, id, _)| *id)
            .collect()
    }

    #[test]
    fn parse_the_documented_spellings() {
        // Empty = the environment decides.
        let p = ProxyConfig::parse("", "").unwrap();
        assert_eq!(p.mode, ProxyMode::System);
        assert_eq!(p.describe(), "not set (follows ALL_PROXY/HTTPS_PROXY if exported)");

        // off/none/no = explicit direct (case- and space-insensitive).
        for v in ["off", "NONE", " no "] {
            assert_eq!(ProxyConfig::parse(v, "").unwrap().mode, ProxyMode::Direct, "{v}");
        }

        // auto/system/env/default = back to following the environment.
        for v in ["auto", "SYSTEM", "env"] {
            assert_eq!(ProxyConfig::parse(v, "").unwrap().mode, ProxyMode::System, "{v}");
        }

        // A bare host:port means socks5h (remote DNS).
        let p = ProxyConfig::parse("127.0.0.1:1080", "").unwrap();
        assert_eq!(p.url(), "socks5h://127.0.0.1:1080");
        assert_eq!(p.redacted(), "socks5h://127.0.0.1:1080");
        assert!(p.enabled());

        // Every socks scheme passes through, lowercased.
        for s in ["socks5h", "socks5", "socks4a", "socks4"] {
            let p = ProxyConfig::parse(&format!("{s}://PROXY.example:1080"), "").unwrap();
            assert_eq!(p.url(), &format!("{s}://PROXY.example:1080"));
        }

        // Credentials survive normalisation but never appear in what we print.
        let p = ProxyConfig::parse("socks5://user:s3cr3t@proxy.example:1080", "").unwrap();
        assert_eq!(p.url(), "socks5://user:s3cr3t@proxy.example:1080");
        assert_eq!(p.redacted(), "socks5://user:***@proxy.example:1080");
        assert!(!p.describe().contains("s3cr3t"), "{}", p.describe());
    }

    #[test]
    fn rejects_values_that_could_never_connect() {
        // An http proxy is not a socks proxy - say so instead of failing later.
        assert!(
            ProxyConfig::parse("http://proxy:3128", "").unwrap_err().message.contains("not a SOCKS proxy")
        );
        assert!(
            ProxyConfig::parse("https://proxy:8443", "").unwrap_err().message.contains("not a SOCKS proxy")
        );
        // Unknown scheme, missing host, impossible port, embedded space.
        assert!(ProxyConfig::parse("ftp://proxy:21", "").unwrap_err().message.contains("unsupported"));
        assert!(ProxyConfig::parse("socks5://", "").is_err());
        assert!(ProxyConfig::parse("socks5://proxy:99999", "").unwrap_err().message.contains("invalid port"));
        assert!(normalize_proxy_url("   ").is_err());
        assert!(normalize_proxy_url("socks5://local host:1080").is_err());
        // A whitespace-only *setting* is "unset", not "invalid" - the same
        // leniency every other string setting has.
        assert_eq!(ProxyConfig::parse("   ", "").unwrap().mode, ProxyMode::System);
    }

    #[test]
    fn redact_url_touches_only_urls_with_credentials() {
        assert_eq!(redact_url("off"), "off");
        assert_eq!(redact_url(""), "");
        assert_eq!(redact_url("socks5h://127.0.0.1:1080"), "socks5h://127.0.0.1:1080");
        assert_eq!(
            redact_url("socks5h://u:p%40ss@proxy.example:1080/deep"),
            "socks5h://u:***@proxy.example:1080/deep"
        );
    }

    #[test]
    fn describe_shows_the_bypass_list_only_when_proxying() {
        let p = ProxyConfig::socks("127.0.0.1:1080", "localhost,192.168.0.0/16").unwrap();
        assert_eq!(p.describe(), "socks5h://127.0.0.1:1080 (direct for: localhost,192.168.0.0/16)");
        assert_eq!(ProxyConfig::direct().describe(), "off (direct connections)");
        assert!(!ProxyConfig::system().describe().contains("direct for"));
    }

    #[test]
    fn apply_builds_a_client_for_every_mode() {
        // The mode must survive the round trip into reqwest's own types, so a
        // value that only *looks* right fails here - which is what Net::set
        // relies on to refuse a broken config.
        for p in [
            ProxyConfig::system(),
            ProxyConfig::direct(),
            ProxyConfig::socks("socks5h://127.0.0.1:1080", DEFAULT_NO_PROXY).unwrap(),
            ProxyConfig::socks("socks5://u:p@127.0.0.1:1080", "").unwrap(),
        ] {
            let built = p
                .apply(reqwest::Client::builder())
                .and_then(|b| b.build().map_err(|e| AgentError::new(e.to_string())));
            assert!(built.is_ok(), "{:?}: {:?}", p, built.err().map(|e| e.message));
        }
    }

    #[test]
    fn setting_value_round_trips_through_parse() {
        for v in ["", "off", "socks5h://127.0.0.1:1080", "127.0.0.1:1080"] {
            let p = ProxyConfig::parse(v, DEFAULT_NO_PROXY).unwrap();
            let stored = setting_value(&p);
            let again = ProxyConfig::parse(&stored, DEFAULT_NO_PROXY).unwrap();
            assert_eq!(again.mode, p.mode, "{v} -> {stored:?}");
            assert_eq!(again.url(), p.url(), "{v} -> {stored:?}");
        }
    }

    #[test]
    fn clients_are_pooled_per_timeout_and_dropped_on_a_change() {
        let mut net = Net::new();
        let no = Kind { timeout: None, connect_timeout: None };
        let ten = Kind { timeout: Some(10_000), connect_timeout: None };

        // Same request twice = the pooled client...
        let (_, id_a) = net.client_for(no);
        let (_, id_a2) = net.client_for(no);
        assert_eq!(id_a, id_a2, "the pool must hand back the same client");
        // ...a different timeout is a different client.
        let (_, id_t) = net.client_for(ten);
        assert_ne!(id_a, id_t);
        assert_eq!(pool_ids(&net).len(), 2);

        // A new proxy invalidates every pooled client, so the next request of
        // each kind gets a fresh one.
        net.set(ProxyConfig::direct()).unwrap();
        assert!(pool_ids(&net).is_empty(), "the pool is dropped with the proxy");
        let (_, id_b) = net.client_for(no);
        assert_ne!(id_a, id_b, "a cached client must not survive a proxy change");
        assert_eq!(pool_ids(&net), vec![id_b]);
        // And the proxy the clients are built from is the new one.
        assert_eq!(net.proxy.mode, ProxyMode::Direct);
    }

    #[test]
    fn a_proxy_that_cannot_be_applied_is_refused_and_changes_nothing() {
        let mut net = Net::new();
        let broken = ProxyConfig {
            mode: ProxyMode::Socks("socks5h://exa mple:1080".into()),
            no_proxy: String::new(),
        };
        assert!(net.set(broken).is_err());
        assert_eq!(net.proxy.mode, ProxyMode::System, "a rejected change changes nothing");
        assert_eq!(net.generation, 0, "and does not invalidate the pool");

        let _g = SlotGuard::new();
        assert!(set_current(broken_from("socks5h://exa mple:1080")).is_err());
        assert_eq!(current().mode, ProxyMode::System);
        set_current(ProxyConfig::direct()).unwrap();
        assert_eq!(current().mode, ProxyMode::Direct);
    }

    fn broken_from(url: &str) -> ProxyConfig {
        ProxyConfig { mode: ProxyMode::Socks(url.to_string()), no_proxy: String::new() }
    }

    /// End-to-end: a client built from a `socks-proxy` setting really does put
    /// its traffic through that proxy, and the proxy reports the destination it
    /// was asked for - the one claim testing the parsing alone cannot make.
    ///
    /// The client is built straight from [`ProxyConfig::apply`] (exactly what
    /// the session does with the settings in the slot) so no other test's
    /// traffic is rerouted through this stub.
    #[tokio::test]
    async fn a_proxied_client_really_reaches_the_server_through_the_proxy() {
        let srv = crate::testsupport::TestServer::with_handler(|r| {
            crate::testsupport::StubResponse::json(format!(r#"{{"path":"{}"}}"#, r.path))
        })
        .await;
        let proxy = crate::testsupport::SocksProxy::start().await;

        // Everything is tunnelled, loopback included (an empty bypass list),
        // which is what a deliberate `-socks-proxy ... -no-proxy ""` does.
        let cfg = ProxyConfig::socks(&proxy.url, "").unwrap();
        let client = cfg
            .apply(reqwest::Client::builder())
            .unwrap()
            .build()
            .expect("client for a valid socks proxy");

        let body: String = client.get(format!("{}/hello", srv.url)).send().await.unwrap().text().await.unwrap();
        assert!(body.contains("/hello"), "response through the proxy: {body}");
        let tunnels = proxy.tunnels();
        assert_eq!(tunnels.len(), 1, "exactly one tunnel: {tunnels:?}");
        let hostport = srv.url.strip_prefix("http://").unwrap();
        assert_eq!(tunnels[0], hostport, "the proxy was asked for the model server");

        // The bypass list is honoured: the same host is reached directly when
        // loopback is listed, so no tunnel is opened at all.
        let direct_cfg = ProxyConfig::socks(&proxy.url, "127.0.0.1").unwrap();
        let direct = direct_cfg.apply(reqwest::Client::builder()).unwrap().build().unwrap();
        let body: String = direct.get(format!("{}/hello", srv.url)).send().await.unwrap().text().await.unwrap();
        assert!(body.contains("/hello"), "{body}");
        assert_eq!(proxy.tunnels().len(), 1, "the bypassed request never reached the proxy");
    }

    /// `off` must beat a configured proxy: the same destination is reached
    /// directly, without the proxy ever seeing a request. (That empty means
    /// "ask the environment" is reqwest's own behaviour - [`ProxyMode::System`]
    /// adds nothing to the builder, so nothing here has to prove it.)
    #[tokio::test]
    async fn off_never_touches_the_proxy() {
        let srv = crate::testsupport::TestServer::with_handler(|_r| {
            crate::testsupport::StubResponse::json("{}")
        })
        .await;
        let proxy = crate::testsupport::SocksProxy::start().await;
        let hostport = srv.url.strip_prefix("http://").unwrap().to_string();

        // A proxy that is in effect: the request is tunnelled to this host.
        let on = ProxyConfig::socks(&proxy.url, "").unwrap();
        let client = on.apply(reqwest::Client::builder()).unwrap().build().unwrap();
        client.get(&srv.url).send().await.unwrap();
        let tunnels = proxy.tunnels_to(&hostport).len();
        assert_eq!(tunnels, 1);

        // The very same setting turned off: the request still succeeds and the
        // proxy is not consulted at all.
        let off = ProxyConfig::parse("off", "").unwrap();
        let client = off.apply(reqwest::Client::builder()).unwrap().build().unwrap();
        client.get(&srv.url).send().await.unwrap();
        assert_eq!(proxy.tunnels_to(&hostport).len(), tunnels, "off must connect directly");
    }
}
