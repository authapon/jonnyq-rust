//! Loads the full content of one discovered skill by name.
//!
//! Port of Go `internal/tools/read_skill.go`.

use serde_json::{json, Value};

use super::{string_arg, CallCtx, Tool};
use crate::config::Config;
use crate::error::{AgentError, R};
use crate::llm::ToolSpec;
use crate::skill;

pub struct SkillTool {
    /// Behind a lock so `/env reload` can retarget the skill directories from
    /// a changed `JONNYQ_SKILL_PATH` without rebuilding the registry.
    paths: std::sync::RwLock<Vec<String>>,
}

impl SkillTool {
    pub fn new(paths: Vec<String>) -> Self {
        Self {
            paths: std::sync::RwLock::new(paths),
        }
    }

    pub fn set_paths(&self, paths: Vec<String>) {
        *self.paths.write().unwrap() = paths;
    }

    pub fn paths(&self) -> Vec<String> {
        self.paths.read().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn configure(&self, cfg: &Config) {
        self.set_paths(cfg.skill_paths.clone());
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_skill".into(),
            description:
                "Read the full content of a skill by name (see the skill list provided each turn)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Skill name, as listed." } },
                "required": ["name"],
            }),
        }
    }

    async fn call(&self, _ctx: CallCtx, args: &Value) -> R<String> {
        let Some(name) = string_arg(args, "name") else {
            return Err(AgentError::new("name is required"));
        };
        if name.is_empty() {
            return Err(AgentError::new("name is required"));
        }
        for s in skill::discover(&self.paths()) {
            if s.name == name {
                let data = std::fs::read_to_string(&s.path)?;
                return Ok(data);
            }
        }
        Err(AgentError::new(format!("skill {:?} not found", name)))
    }
}
