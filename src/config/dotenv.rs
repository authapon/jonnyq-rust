//! Reads `KEY=value` settings from a dotenv file (`.env` by default).
//!
//! This is what lets a project keep its jonnyq settings in a file next to
//! the code instead of exporting `JONNYQ_*` in every shell. The file is read
//! once at startup (and again by `/env reload`) and outranks the *process*
//! environment, so the precedence stays:
//!
//! ```text
//! CLI flag  >  .env file  >  process env var  >  built-in default
//! ```
//!
//! A key the file sets to an empty value counts as set, so `JONNYQ_MODEL=`
//! overrides an exported `JONNYQ_MODEL` rather than being ignored.
//!
//! # Accepted syntax
//!
//! ```text
//! # a comment line
//!   # leading whitespace before a comment is fine
//! JONNYQ_MODEL=qwen3:14b          # an inline comment after an unquoted value
//! export JONNYQ_CONTEXT_SIZE=32768  # a leading `export ` is allowed
//! JONNYQ_KEY="sk-...with spaces"  # double quotes: escapes + $VARS are expanded
//! JONNYQ_NTFY_URL='https://x/$NO' # single quotes: taken literally
//! JONNYQ_SKILL_PATH=$HOME/skills  # $VAR/${VAR} expand: file keys first, env next
//! ```
//!
//! * blank lines and comment lines are skipped; a `#` also starts a comment
//!   inside an unquoted value when preceded by whitespace
//! * double-quoted values understand `\n`, `\r`, `\t`, `\\`, `\"`, `\$` and may
//!   span several lines; single-quoted values are literal
//! * `$VAR`, `${VAR}` and `${VAR:-fallback}` in unquoted and double-quoted
//!   values expand from earlier keys of the same file first, then from the
//!   process environment (the file is the layer that wins, so its own keys
//!   expand the way the file says); an unresolvable `$VAR` is left as written
//!   (so a key containing a literal `$` is never mangled) and `$$` is an
//!   escaped `$`
//! * a repeated key keeps its **last** value
//! * malformed lines (no `=`, bad key name) produce a warning and are skipped;
//!   only an unterminated quote is a hard error, because everything after it
//!   would be misread

use std::path::{Path, PathBuf};

use crate::error::{AgentError, R};

/// The file read when nothing selects another one, looked up in the working
/// directory.
pub const DEFAULT_ENV_FILE: &str = ".env";

/// Values of the `-env-file` flag / `JONNYQ_ENV_FILE` var that mean "do not
/// read any dotenv file at all".
pub const DISABLE_VALUES: &[&str] = &["off", "none", "no"];

/// Settings parsed out of one dotenv file, in file order.
#[derive(Clone, Default, Debug)]
pub struct Dotenv {
    path: PathBuf,
    vars: Vec<(String, String)>,
    /// Human-readable parse complaints (bad lines that were skipped). Kept so
    /// the caller can print them once, rather than the parser talking to
    /// stderr itself.
    pub warnings: Vec<String>,
}

impl Dotenv {
    /// Empty set with no file attached - the "dotenv disabled / not found"
    /// value, which makes every lookup fall through to the process env and
    /// then to the built-in defaults.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Resolves a user-supplied selector and reads the file it points at.
    ///
    /// * `""` (nothing selected) -> [`Dotenv::discover`]
    /// * `off`/`none`/`no` -> [`Dotenv::empty`], without touching the disk
    /// * anything else -> [`Dotenv::load`], which errors if the file is
    ///   missing (a mistyped path should fail loudly, not silently be ignored)
    pub fn resolve(selector: &str) -> R<Option<Dotenv>> {
        let sel = selector.trim();
        if sel.is_empty() {
            return Dotenv::discover();
        }
        if DISABLE_VALUES.iter().any(|v| v.eq_ignore_ascii_case(sel)) {
            return Ok(None);
        }
        Some(Dotenv::load(sel)).transpose()
    }

    /// Reads [`DEFAULT_ENV_FILE`] from the current working directory. `None`
    /// means there is no such file, which is the normal case and not an
    /// error.
    pub fn discover() -> R<Option<Dotenv>> {
        let path = Path::new(DEFAULT_ENV_FILE);
        if !path.is_file() {
            return Ok(None);
        }
        Dotenv::load(&path.to_string_lossy()).map(Some)
    }

    /// Reads and parses one file. Missing file, unreadable file and an
    /// unterminated quote are errors.
    pub fn load(path: &str) -> R<Dotenv> {
        let p = PathBuf::from(expand_tilde(path.trim()));
        if !p.exists() {
            return Err(AgentError::new(format!(
                "env file {:?} does not exist (use -env-file {} to disable dotenv loading)",
                p.display(),
                DISABLE_VALUES[0]
            )));
        }
        let text = std::fs::read_to_string(&p)
            .map_err(|e| AgentError::new(format!("cannot read env file {:?}: {}", p.display(), e)))?;
        let mut d = parse(&text, &p.display().to_string())?;
        d.path = p;
        Ok(d)
    }

    pub fn path(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    pub fn has_file(&self) -> bool {
        !self.path.as_os_str().is_empty()
    }

    pub fn len(&self) -> usize {
        self.vars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// All parsed pairs, in file order (including keys jonnyq ignores).
    pub fn vars(&self) -> &[(String, String)] {
        &self.vars
    }

    /// Looks one key up (exact name, e.g. `JONNYQ_MODEL`).
    pub fn get(&self, key: &str) -> Option<&str> {
        // Last occurrence wins, so search from the back.
        self.vars
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Expands a leading `~` to the user's home directory, so `-env-file
/// ~/.config/jonnyq.env` and `JONNYQ_ENV_FILE=~/jonnyq.env` work the way a
/// shell would do it.
fn expand_tilde(s: &str) -> String {
    if s == "~" || s.starts_with("~/") || s.starts_with("~\\") {
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return if s == "~" {
                    home
                } else {
                    format!("{}{}", home.trim_end_matches(['/', '\\']), &s[1..])
                };
            }
        }
    }
    s.to_string()
}

/// Character-position cursor that keeps track of the 1-based line number so
/// warnings can point at the offending line.
struct Scan<'a> {
    chars: Vec<char>,
    text: &'a str,
    i: usize,
    line: usize,
}

impl<'a> Scan<'a> {
    fn new(text: &'a str) -> Self {
        Self { chars: text.chars().collect(), text, i: 0, line: 1 }
    }

    fn eof(&self) -> bool {
        self.i >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.i).copied()
    }

    /// Consumes one character, counting newlines.
    fn next(&mut self) -> Option<char> {
        let c = self.chars.get(self.i).copied()?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ') | Some('\t') | Some('\r')) {
            self.next();
        }
    }

    /// Consumes the rest of the current line (and its newline, if any).
    fn skip_line(&mut self) {
        while let Some(c) = self.peek() {
            self.next();
            if c == '\n' {
                break;
            }
        }
    }

    /// Consumes the value part of the current line (used when the key was
    /// already rejected, so parsing resynchronises on the next line).
    fn skip_value(&mut self) {
        self.skip_line();
    }

    /// Skips whitespace, blank lines and whole-line comments, stopping at the
    /// first character that could start a `KEY=value` line.
    fn skip_to_key(&mut self) {
        loop {
            self.skip_blanks();
            match self.peek() {
                Some('\n') => {
                    self.next();
                }
                // A '#' only starts a comment at the beginning of a line
                // (after optional blanks); inside a value it is handled there.
                Some('#') => self.skip_line(),
                _ => return,
            }
        }
    }

    fn rest_of_line(&self) -> &'a str {
        let remainder = &self.text[self.offset_in_text()..];
        match remainder.find('\n') {
            Some(n) => &remainder[..n],
            None => remainder,
        }
    }

    /// Byte offset of the cursor, for slicing [`Scan::rest_of_line`].
    fn offset_in_text(&self) -> usize {
        self.chars[..self.i].iter().map(|c| c.len_utf8()).sum()
    }
}

/// Parses dotenv text. `path` is only used to label warnings and errors.
///
/// Every complaint that does not make the rest of the file unreadable is
/// collected into [`Dotenv::warnings`]; an unterminated quote returns `Err`
/// because there is no way to know where the value was meant to end.
pub fn parse(text: &str, path: &str) -> R<Dotenv> {
    let mut s = Scan::new(text);
    let mut d = Dotenv::default();

    loop {
        s.skip_to_key();
        if s.eof() {
            break;
        }
        let key_line = s.line;

        // Key: everything up to '=' or the end of the line.
        let mut key = String::new();
        let mut saw_eq = false;
        while let Some(c) = s.peek() {
            if c == '=' {
                s.next();
                saw_eq = true;
                break;
            }
            if c == '\n' {
                break;
            }
            key.push(c);
            s.next();
        }
        let key = key.trim().to_string();
        // `export KEY=value` is accepted (dotenv files are often shared with
        // shell startup files), and re-checked by valid_key below afterwards.
        let key = match key.split_once(char::is_whitespace) {
            Some(("export", rest)) => rest.trim().to_string(),
            _ => key,
        };

        if !saw_eq {
            if !key.trim().is_empty() {
                d.warnings.push(format!(
                    "{}:{}: ignoring line without '=': {:?}",
                    path,
                    key_line,
                    key.trim()
                ));
            }
            s.skip_line();
            continue;
        }
        if !valid_key(&key) {
            d.warnings.push(format!(
                "{}:{}: ignoring invalid variable name {:?}",
                path, key_line, key
            ));
            s.skip_value();
            continue;
        }

        let value = match read_value(&mut s, path, key_line)? {
            Some(v) => v,
            // `KEY=` (nothing after '=') is a legitimate empty value.
            None => Raw::Literal(String::new()),
        };

        let value = match value {
            Raw::Literal(v) => v,
            Raw::Expandable(v) => {
                // Earlier keys of this file first, then the process
                // environment: the file is the layer that wins, so a name the
                // file defines must expand to what the file says even when the
                // shell exports the same name.
                let expanded = expand(&v, |name| match d.get(name) {
                    Some(v) => Some(v.to_string()),
                    None => std::env::var(name).ok(),
                });
                expanded
            }
        };

        if key == "JONNYQ_ENV_FILE" {
            d.warnings.push(format!(
                "{}:{}: JONNYQ_ENV_FILE inside a dotenv file is ignored (it would select the file to read)",
                path, key_line
            ));
            continue;
        }

        d.vars.push((key, value));
    }

    Ok(d)
}

/// A value before `$VAR` expansion.
enum Raw {
    /// Quoted with single quotes, or already expanded: used verbatim.
    Literal(String),
    /// Unquoted or double-quoted: `$VAR`/`${VAR}` get expanded.
    Expandable(String),
}

fn read_value(s: &mut Scan<'_>, path: &str, key_line: usize) -> R<Option<Raw>> {
    s.skip_blanks();
    match s.peek() {
        Some('\n') | None => {
            if s.peek().is_some() {
                s.next();
            }
            return Ok(None);
        }
        Some('"') => {
            s.next();
            let v = read_double_quoted(s, path, key_line)?;
            // Anything after the closing quote is noise; drop a comment
            // quietly, warn on real text.
            s.skip_blanks();
            match s.peek() {
                None | Some('\n') => {}
                Some('#') => s.skip_line(),
                Some(_) => {
                    let mut trailing = String::new();
                    while let Some(c) = s.peek() {
                        if c == '\n' {
                            break;
                        }
                        trailing.push(c);
                        s.next();
                    }
                    s.skip_line();
                    return Err(AgentError::new(format!(
                        "{}:{}: unexpected text {:?} after quoted value",
                        path,
                        key_line,
                        trailing.trim()
                    )));
                }
            }
            Ok(Some(Raw::Expandable(v)))
        }
        Some('\'') => {
            s.next();
            let mut v = String::new();
            loop {
                match s.next() {
                    None => {
                        return Err(AgentError::new(format!(
                            "{}:{}: unterminated single-quoted value",
                            path, key_line
                        )))
                    }
                    Some('\'') => break,
                    Some(c) => v.push(c),
                }
            }
            s.skip_blanks();
            match s.peek() {
                None | Some('\n') | Some('#') => {}
                Some(_) => {
                    let rest = s.rest_of_line().trim().to_string();
                    s.skip_line();
                    return Err(AgentError::new(format!(
                        "{}:{}: unexpected text {:?} after quoted value",
                        path, key_line, rest
                    )));
                }
            }
            if s.peek() == Some('#') {
                s.skip_line();
            }
            Ok(Some(Raw::Literal(v)))
        }
        _ => {
            // Unquoted: read to end of line, with a '#' starting an inline
            // comment when it opens the value or follows whitespace (so
            // `pass#word` and `http://x/#frag` stay intact).
            let mut v = String::new();
            loop {
                let Some(c) = s.peek() else { break };
                if c == '\n' {
                    break;
                }
                if c == '#' && (v.is_empty() || matches!(peek_back(s), Some(' ') | Some('\t'))) {
                    s.skip_line();
                    break;
                }
                v.push(c);
                s.next();
            }
            while s.peek() == Some('\r') {
                s.next();
            }
            let v = v.trim_end().to_string();
            if s.peek() == Some('\n') {
                s.next();
            }
            Ok(Some(Raw::Expandable(v)))
        }
    }
}

/// The character just before the cursor (used to decide whether a '#' starts
/// an inline comment).
fn peek_back(s: &Scan<'_>) -> Option<char> {
    if s.i == 0 {
        return None;
    }
    s.chars.get(s.i - 1).copied()
}

fn read_double_quoted(s: &mut Scan<'_>, path: &str, key_line: usize) -> R<String> {
    let mut v = String::new();
    loop {
        match s.peek() {
            None => {
                return Err(AgentError::new(format!(
                    "{}:{}: unterminated double-quoted value",
                    path, key_line
                )))
            }
            Some('"') => {
                s.next();
                return Ok(v);
            }
            Some('\\') => {
                s.next();
                match s.next() {
                    None => {
                        return Err(AgentError::new(format!(
                            "{}:{}: unterminated double-quoted value",
                            path, key_line
                        )))
                    }
                    Some('n') => v.push('\n'),
                    Some('r') => v.push('\r'),
                    Some('t') => v.push('\t'),
                    Some('\\') => v.push('\\'),
                    Some('"') => v.push('"'),
                    Some('\'') => v.push('\''),
                    Some('$') => v.push('$'),
                    Some('\n') => {} // line continuation inside a quoted value
                    Some(c) => {
                        v.push('\\');
                        v.push(c);
                    }
                }
            }
            Some(c) => {
                s.next();
                v.push(c);
            }
        }
    }
}

/// `JONNYQ_FOO_BAR` and friends: a leading underscore or letter, then letters,
/// digits and underscores. (Dots and dashes are rejected rather than silently
/// accepted as an unused key.)
pub(crate) fn valid_key(key: &str) -> bool {
    let mut it = key.chars();
    match it.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    it.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Expands `$VAR`, `${VAR}`, `${VAR:-fallback}` and `$$` in a value.
///
/// `lookup` resolves a name; the caller decides the order (jonnyq consults
/// earlier keys of the same file first, then the real environment).
///
/// Unlike a shell, a name that resolves to nothing leaves the `$...` text
/// **as it was written** instead of blanking it out: a real API key can
/// contain `$` sequences (`sk-$2a$10$…`) and silently truncating a secret is
/// far worse than a path that stays visibly unresolved. `${VAR:-}` and
/// `${VAR:-fallback}` are the explicit ways to ask for empty/a default, and
/// `$$` is an escaped literal `$`.
fn expand(value: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let c: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < c.len() {
        if c[i] != '$' {
            out.push(c[i]);
            i += 1;
            continue;
        }
        // `$$` is an escaped literal '$'.
        if c.get(i + 1) == Some(&'$') {
            out.push('$');
            i += 2;
            continue;
        }
        if c.get(i + 1) == Some(&'{') {
            // ${NAME} or ${NAME:-fallback}
            if let Some(end) = find_brace_end(&c, i + 2) {
                let inner: String = c[i + 2..end].iter().collect();
                let (name, fallback) = match inner.split_once(":-") {
                    Some((n, f)) => (n, Some(f)),
                    None => (inner.as_str(), None),
                };
                if is_valid_name(name) {
                    match lookup(name) {
                        Some(v) => out.push_str(&v),
                        None => match &fallback {
                            Some(f) => out.push_str(f),
                            None => out.extend(c[i..=end].iter()),
                        },
                    }
                    i = end + 1;
                    continue;
                }
                // `${...}` that is not a name: leave it alone.
                out.extend(c[i..=end].iter());
                i = end + 1;
                continue;
            }
        }
        // $NAME
        let mut j = i + 1;
        if c.get(j).map(|c0| c0.is_ascii_alphabetic() || *c0 == '_') == Some(true) {
            while j < c.len() && (c[j].is_ascii_alphanumeric() || c[j] == '_') {
                j += 1;
            }
            let name: String = c[i + 1..j].iter().collect();
            match lookup(&name) {
                Some(v) => out.push_str(&v),
                None => out.extend(c[i..j].iter()),
            }
            i = j;
            continue;
        }
        // A lone '$' (e.g. "cost$ 5"): keep it verbatim.
        out.push('$');
        i += 1;
    }
    out
}

fn is_valid_name(name: &str) -> bool {
    valid_key(name)
}

/// Index of the `}` closing the `${` whose body starts at `from`.
fn find_brace_end(c: &[char], from: usize) -> Option<usize> {
    (from..c.len()).find(|&k| c[k] == '}')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(text: &str) -> Dotenv {
        parse(text, "test.env").expect("expected the fixture to parse")
    }

    #[test]
    fn parses_simple_pairs() {
        let d = vars("JONNYQ_MODEL=llama3\nJONNYQ_CONTEXT_SIZE=8192\n");
        assert_eq!(d.get("JONNYQ_MODEL"), Some("llama3"));
        assert_eq!(d.get("JONNYQ_CONTEXT_SIZE"), Some("8192"));
        assert_eq!(d.len(), 2);
        assert!(d.warnings.is_empty(), "unexpected warnings: {:?}", d.warnings);
    }

    #[test]
    fn skips_blank_lines_and_comments() {
        let d = vars("\n   # comment\n\t\nJONNYQ_MODEL=a\n  # indented\nJONNYQ_KEY=b\n");
        assert_eq!(d.len(), 2);
        assert_eq!(d.get("JONNYQ_KEY"), Some("b"));
    }

    #[test]
    fn accepts_export_prefix_and_spaces_around_equals() {
        let d = vars("export JONNYQ_MODEL = qwen3:14b \n");
        assert_eq!(d.get("JONNYQ_MODEL"), Some("qwen3:14b"));
    }

    #[test]
    fn inline_comment_after_unquoted_value() {
        let d = vars("JONNYQ_MODEL=llama3 # the model\nJONNYQ_URL=http://x/y#w\n");
        assert_eq!(d.get("JONNYQ_MODEL"), Some("llama3"));
        assert_eq!(d.get("JONNYQ_URL"), Some("http://x/y#w"));
    }

    #[test]
    fn quotes_are_stripped_and_protect_spaces_and_hashes() {
        let d = vars(
            "A=\"quoted value\" # trailing comment\nB='single # not a comment'\nC=\"hash # inside\"\n",
        );
        assert_eq!(d.get("A"), Some("quoted value"));
        assert_eq!(d.get("B"), Some("single # not a comment"));
        assert_eq!(d.get("C"), Some("hash # inside"));
    }

    #[test]
    fn double_quotes_unescape() {
        let d = vars(r#"A="line1\nline2\ttab\\slash\"quote""#.to_string().as_str());
        assert_eq!(d.get("A"), Some("line1\nline2\ttab\\slash\"quote"));
    }

    #[test]
    fn single_quotes_are_literal() {
        let d = vars(r#"A='no \n escapes here'"#);
        assert_eq!(d.get("A"), Some(r"no \n escapes here"));
    }

    #[test]
    fn quoted_value_can_span_lines() {
        let d = vars("A=\"first\nsecond\"\nB=next\n");
        assert_eq!(d.get("A"), Some("first\nsecond"));
        assert_eq!(d.get("B"), Some("next"));
    }

    #[test]
    fn empty_value_is_kept_as_empty() {
        let d = vars("A=\nB=\"\"\n");
        assert_eq!(d.get("A"), Some(""));
        assert_eq!(d.get("B"), Some(""));
        assert!(d.warnings.is_empty(), "unexpected warnings: {:?}", d.warnings);
    }

    #[test]
    fn expansion_uses_process_env_when_the_file_does_not_define_the_name() {
        let key = "JONNYQ_DOTENV_TEST_HOME_DIR";
        unsafe { std::env::set_var(key, "/tmp/home") };
        let d = vars(&format!(
            "A=${{{key}}} X\nB=$ {key}\nC=A/sub\nD=${{}}:E\nF=${{UNSET_XYZ:-fallback}}\nG=$$\nH=$UNSET_XYZ/tail\n",
            key = key
        ));
        unsafe { std::env::remove_var(key) };
        assert_eq!(d.get("A"), Some("/tmp/home X"));
        // A '$' not followed by a name is kept verbatim, not treated as a
        // (broken) expansion.
        assert_eq!(d.get("B"), Some("$ JONNYQ_DOTENV_TEST_HOME_DIR"));
        assert_eq!(d.get("C"), Some("A/sub"));
        assert_eq!(d.get("D"), Some("${}:E"));
        assert_eq!(d.get("F"), Some("fallback"));
        assert_eq!(d.get("G"), Some("$"));
        // An unset name is left exactly as written rather than blanked out.
        assert_eq!(d.get("H"), Some("$UNSET_XYZ/tail"));
    }

    #[test]
    fn expansion_falls_back_to_earlier_key_in_same_file() {
        let d = vars("BASE=/opt\nSKILLS=${BASE}/skills\nMORE=$BASE/more\n");
        assert_eq!(d.get("SKILLS"), Some("/opt/skills"));
        assert_eq!(d.get("MORE"), Some("/opt/more"));
    }

    #[test]
    fn expansion_prefers_a_file_key_over_the_process_environment() {
        // The file is the layer that beats the environment, so a name the file
        // defines must expand to the file's own value even when the shell
        // exports the same name.
        let key = "JONNYQ_DOTENV_SHADOWED_BASE";
        unsafe { std::env::set_var(key, "/from/env") };
        let d = vars(&format!("{key}=/from/file\nPATH2=${{{key}}}/sub\nPATH3=${key}/sub\n"));
        unsafe { std::env::remove_var(key) };
        assert_eq!(d.get("PATH2"), Some("/from/file/sub"));
        assert_eq!(d.get("PATH3"), Some("/from/file/sub"));
    }

    #[test]
    fn single_quoted_values_are_not_expanded() {
        let d = vars("A='$HOME/literal'\n");
        assert_eq!(d.get("A"), Some("$HOME/literal"));
    }

    #[test]
    fn last_duplicate_wins() {
        let d = vars("A=first\nA=second\n");
        assert_eq!(d.get("A"), Some("second"));
    }

    #[test]
    fn malformed_lines_warn_and_are_skipped() {
        let d = vars("just some text\n9BAD=x\nGOOD=y\n");
        assert_eq!(d.get("GOOD"), Some("y"));
        assert_eq!(d.warnings.len(), 2, "warnings: {:?}", d.warnings);
        assert!(d.warnings[0].contains("1:"));
        assert!(d.warnings[1].contains("invalid variable name"));
    }

    #[test]
    fn unterminated_quote_is_an_error() {
        let e = parse("A=\"never closed\nB=x\n", "t.env").unwrap_err();
        assert!(e.message.contains("unterminated double-quoted value"), "{}", e.message);
        let e = parse("A='never closed\n", "t.env").unwrap_err();
        assert!(e.message.contains("unterminated single-quoted value"), "{}", e.message);
    }

    #[test]
    fn comment_only_value_and_unmatched_line() {
        // `A=# nothing here` means an empty value, and a bare word with no
        // '=' is skipped with a warning while the rest of the file still loads.
        let d = vars("A=# nothing here\nB=1\njust words\nC=2\n");
        assert_eq!(d.get("A"), Some(""));
        assert_eq!(d.get("B"), Some("1"));
        assert_eq!(d.get("C"), Some("2"));
        assert_eq!(d.warnings.len(), 1, "{:?}", d.warnings);
    }

    #[test]
    fn crlf_line_endings() {
        let d = vars("A=1\r\nB=\"2\"\r\nC=3\r\n");
        assert_eq!(d.get("A"), Some("1"));
        assert_eq!(d.get("B"), Some("2"));
        assert_eq!(d.get("C"), Some("3"));
    }

    #[test]
    fn env_file_key_in_the_file_is_ignored() {
        let d = vars("JONNYQ_ENV_FILE=other.env\nA=1\n");
        assert!(d.get("JONNYQ_ENV_FILE").is_none());
        assert_eq!(d.get("A"), Some("1"));
    }

    #[test]
    fn resolves_selector_forms() {
        // off/none/no disable without touching the disk
        assert!(Dotenv::resolve("off").unwrap().is_none());
        assert!(Dotenv::resolve("NONE").unwrap().is_none());
        // an explicit missing path is an error, not a silent no-op
        let e = Dotenv::resolve("definitely-not-here.env").unwrap_err();
        assert!(e.message.contains("does not exist"), "{}", e.message);
    }

    #[test]
    fn loads_a_real_file_and_reports_its_path() {
        let dir = std::env::temp_dir().join(format!("jonnyq-dotenv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("custom.env");
        std::fs::write(&f, "JONNYQ_MODEL=from-file\n").unwrap();
        let d = Dotenv::load(&f.to_string_lossy()).unwrap();
        assert_eq!(d.get("JONNYQ_MODEL"), Some("from-file"));
        assert!(d.path().ends_with("custom.env"), "{}", d.path());
        assert!(d.has_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tilde_is_expanded_in_paths() {
        if std::env::var("HOME").is_err() {
            return;
        }
        assert_eq!(expand_tilde("~/x.env"), format!("{}/x.env", std::env::var("HOME").unwrap()));
        assert_eq!(expand_tilde("~"), std::env::var("HOME").unwrap());
        assert_eq!(expand_tilde("./x.env"), "./x.env");
    }

    #[test]
    fn empty_dotenv_looks_up_nothing() {
        let d = Dotenv::empty();
        assert!(d.is_empty());
        assert!(!d.has_file());
        assert_eq!(d.get("JONNYQ_MODEL"), None);
    }
}
