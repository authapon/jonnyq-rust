//! Discovers skill files under the configured skill_path directories. A
//! skill is either a subdirectory containing SKILL.md, or a standalone .md
//! file; its description is the first non-empty, non-heading line of that
//! markdown so it can be summarized to the model without requiring any
//! frontmatter parser.
//!
//! Port of Go `internal/skill`.

use std::path::Path;

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: String,
}

pub fn discover(paths: &[String]) -> Vec<Skill> {
    let mut out = Vec::new();
    for root in paths {
        let entries = match std::fs::read_dir(root) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let file_type = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            let name = entry.file_name().to_string_lossy().to_string();
            if file_type.is_dir() {
                let md_path = Path::new(root).join(&name).join("SKILL.md");
                if let Ok(data) = std::fs::read_to_string(&md_path) {
                    out.push(Skill {
                        name: name.clone(),
                        description: first_line(&data),
                        path: md_path.to_string_lossy().to_string(),
                    });
                }
                continue;
            }
            let path = Path::new(root).join(&name);
            let ext = path.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            if !ext.eq_ignore_ascii_case("md") {
                continue;
            }
            let Ok(data) = std::fs::read_to_string(&path) else {
                continue;
            };
            // Go trims the ".md" suffix (filepath.Ext includes the dot), so
            // drop the dot as well.
            let skill_name = name[..name.len() - ext.len() - 1].to_string();
            out.push(Skill {
                name: skill_name,
                description: first_line(&data),
                path: path.to_string_lossy().to_string(),
            });
        }
    }
    out
}

fn first_line(content: &str) -> String {
    for line in content.split('\n') {
        let line = line.trim().trim_start_matches('#').trim();
        if !line.is_empty() {
            // Go truncates to 200 *bytes*; do the same but on a char
            // boundary so multi-byte text is never split.
            if line.len() > 200 {
                let mut end = 200;
                while end > 0 && !line.is_char_boundary(end) {
                    end -= 1;
                }
                return line[..end].to_string();
            }
            return line.to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_dirs_and_files() {
        let dir = std::env::temp_dir().join(format!("jonnyq-skill-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("myskill")).unwrap();
        std::fs::write(dir.join("myskill/SKILL.md"), "# My Skill\n\nA helpful skill.\n").unwrap();
        std::fs::write(dir.join("other.md"), "## heading two\nreal description\n").unwrap();
        std::fs::write(dir.join("ignore.txt"), "nope").unwrap();

        let root = dir.to_string_lossy().to_string();
        let skills = discover(&[root]);
        assert_eq!(skills.len(), 2);
        let my = skills.iter().find(|s| s.name == "myskill").unwrap();
        assert_eq!(my.description, "My Skill");
        let other = skills.iter().find(|s| s.name == "other").unwrap();
        assert_eq!(other.description, "heading two");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
