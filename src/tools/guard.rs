//! Path resolution confined to the working directory.
//!
//! Port of Go `internal/tools/guard.go`.

use std::path::{Path, PathBuf};

use crate::error::{AgentError, R};

/// Resolves `p` (absolute or relative to work_dir) and guarantees the result
/// stays inside work_dir. It walks the path component by component, resolving
/// symlinks as it goes, so a symlink placed inside work_dir cannot be used to
/// escape it (a plain lexical "canonicalize the parent + starts_with" check
/// alone would not catch that).
pub fn resolve_path(work_dir: &str, p: &str) -> R<PathBuf> {
    if p.trim().is_empty() {
        return Err(AgentError::new("path is required"));
    }

    let work_abs = absolutenow(work_dir);
    let work_real = std::fs::canonicalize(&work_abs).map_err(|e| {
        AgentError::new(format!(
            "resolve working directory: {} ({})",
            work_abs.display(),
            e
        ))
    })?;

    let target = if Path::new(p).is_absolute() {
        lexical_clean(p)
    } else {
        lexical_clean(&work_real.join(p).to_string_lossy())
    };

    let rel = match target.strip_prefix(&work_real) {
        Ok(r) => r.to_path_buf(),
        Err(_) => {
            return Err(AgentError::new(format!(
                "path {:?} is outside the working directory",
                p
            )))
        }
    };

    let mut cur = work_real.clone();
    for part in rel.components() {
        let part = match part {
            std::path::Component::Normal(s) => s,
            _ => continue,
        };
        cur = cur.join(part);
        let Some(real) = dynamic_canonicalize(&cur) else {
            // Component doesn't exist yet (e.g. a file being created) -
            // nothing more to resolve below it.
            break;
        };
        cur = real;
        if !cur.starts_with(&work_real) {
            return Err(AgentError::new(format!(
                "path {:?} escapes the working directory via a symlink",
                p
            )));
        }
    }

    Ok(target)
}

/// std::fs::canonicalize but tolerant of paths with a trailing component
/// that doesn't exist: canonicalizes the nearest existing ancestor and
/// re-appends the rest. Go's EvalSymlinks behaves the same way for
/// intermediate components (the loop above needs per-component resolution,
/// but the working directory itself must exist so plain canonicalize is fine
/// there; this helper is used defensively).
fn dynamic_canonicalize(p: &Path) -> Option<PathBuf> {
    if let Ok(c) = std::fs::canonicalize(p) {
        return Some(c);
    }
    // Fall back: walk up until an ancestor canonicalizes, then rejoin.
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p;
    loop {
        let parent = cur.parent()?;
        let name = cur.file_name()?;
        missing.push(name.to_os_string());
        cur = parent;
        if let Ok(mut c) = std::fs::canonicalize(cur) {
            for m in missing.iter().rev() {
                c = c.join(m);
            }
            return Some(c);
        }
    }
}

/// Lexical path cleanup resembling Go's filepath.Clean: collapses `.`, `..`,
/// duplicate and trailing separators, without touching the filesystem.
pub fn lexical_clean(p: &str) -> PathBuf {
    use std::path::Component;
    let in_path = Path::new(p);
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    let mut root = None;
    for comp in in_path.components() {
        match comp {
            Component::RootDir => {
                root = Some(std::path::PathBuf::from(std::path::MAIN_SEPARATOR_STR))
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(last) = parts.last() {
                    if last != ".." {
                        parts.pop();
                        continue;
                    }
                }
                if root.is_some() {
                    // .. at or above root just stays at root (filepath.Clean
                    // collapses "/../a" to "/a").
                    continue;
                }
                parts.push(std::ffi::OsString::from(".."));
            }
            Component::Normal(s) => parts.push(s.to_os_string()),
            Component::Prefix(_) => {}
        }
    }
    let mut out = root.unwrap_or_default();
    for part in &parts {
        // PathBuf::push inserts the separator itself - and pushing a bare
        // "/" would replace the whole path, so push the component only.
        out.push(part);
    }
    if parts.is_empty() {
        if out.as_os_str().is_empty() {
            return PathBuf::from(".");
        }
        return out;
    }
    out
}

/// std's env::current_dir-based absolutization of `base`
/// (Go: filepath.Abs applied to the work dir).
fn absolutenow(base: &str) -> PathBuf {
    let base_path = Path::new(base);
    if base_path.is_absolute() {
        lexical_clean(&base_path.to_string_lossy())
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        lexical_clean(&cwd.join(base_path).to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_paths() {
        assert_eq!(lexical_clean("/a/b/../c").to_string_lossy(), "/a/c");
        assert_eq!(lexical_clean("a//./b").to_string_lossy(), "a/b");
        assert_eq!(lexical_clean("/../a").to_string_lossy(), "/a");
        assert_eq!(lexical_clean("../a").to_string_lossy(), "../a");
        assert_eq!(lexical_clean("").to_string_lossy(), ".");
    }

    #[test]
    fn rejects_traversal() {
        let dir = std::env::temp_dir().join(format!("jonnyq-guard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let wd = dir.to_string_lossy().to_string();
        assert!(resolve_path(&wd, "../escape").is_err());
        assert!(resolve_path(&wd, "a/../..").is_err());
        assert!(resolve_path(&wd, "ok/inside").is_ok());
        assert!(resolve_path(&wd, "/etc/passwd").is_err());
        assert!(resolve_path(&wd, "").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_escape_rejected() {
        let dir = std::env::temp_dir().join(format!("jonnyq-guard-sym-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", dir.join("link")).unwrap();
            let wd = dir.to_string_lossy().to_string();
            assert!(resolve_path(&wd, "link/passwd").is_err());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
