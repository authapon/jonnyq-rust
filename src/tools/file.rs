//! read_file / write_file / edit_file / create_folder, all restricted to the
//! working directory.
//!
//! Every write goes through [`crate::tools::context_guard`] first: another
//! agent's history file is readable by the whole team and writable by none.
//!
//! Port of Go `internal/tools/file.go`.

use serde_json::{json, Value};

use super::{
    bool_arg, context_guard::check_writable, guard::resolve_path, string_arg, CallCtx, Tool,
};
use crate::error::{AgentError, R};
use crate::llm::ToolSpec;

const MAX_READ_BYTES: usize = 2 * 1024 * 1024; // 2MB cap so one read_file can't blow the context

fn obj(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
    })
}

pub struct ReadFileTool {
    pub work_dir: String,
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read the contents of a text file inside the working directory.".into(),
            parameters: obj(
                json!({ "path": { "type": "string", "description": "File path, relative to the working directory." } }),
                &["path"],
            ),
        }
    }

    async fn call(&self, _ctx: CallCtx, args: &Value) -> R<String> {
        let Some(p) = string_arg(args, "path") else {
            return Err(AgentError::new("path is required"));
        };
        let full = resolve_path(&self.work_dir, p)?;
        let data = std::fs::read(&full)?;
        if data.len() > MAX_READ_BYTES {
            let head = String::from_utf8_lossy(&data[..MAX_READ_BYTES]);
            return Ok(format!(
                "{}\n\n[truncated: file is {} bytes, showing first {} bytes]",
                head,
                data.len(),
                MAX_READ_BYTES
            ));
        }
        Ok(String::from_utf8_lossy(&data).into_owned())
    }
}

pub struct WriteFileTool {
    pub work_dir: String,
}

#[async_trait::async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description:
                "Create or overwrite a file inside the working directory with the given content."
                    .into(),
            parameters: obj(
                json!({
                    "path": { "type": "string", "description": "File path, relative to the working directory." },
                    "content": { "type": "string", "description": "Full content to write to the file." },
                }),
                &["path", "content"],
            ),
        }
    }

    async fn call(&self, _ctx: CallCtx, args: &Value) -> R<String> {
        let Some(p) = string_arg(args, "path") else {
            return Err(AgentError::new("path is required"));
        };
        let content = string_arg(args, "content").unwrap_or("");
        let full = resolve_path(&self.work_dir, p)?;
        // An agent's history file is jonnyq's to write - read it freely, never
        // rewrite it (see [`crate::tools::context_guard`]).
        check_writable(&self.work_dir, p)?;
        if let Some(dir) = full.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&full, content.as_bytes())?;
        Ok(format!("wrote {} bytes to {}", content.len(), p))
    }
}

pub struct EditFileTool {
    pub work_dir: String,
}

#[async_trait::async_trait]
impl Tool for EditFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace an exact, unique occurrence of old_string with new_string in a file inside the working directory. Fails if old_string is not found, or is found more than once unless replace_all is true.".into(),
            parameters: obj(
                json!({
                    "path": { "type": "string", "description": "File path, relative to the working directory." },
                    "old_string": { "type": "string", "description": "Exact text to replace." },
                    "new_string": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence instead of requiring a unique match. Defaults to false." },
                }),
                &["path", "old_string", "new_string"],
            ),
        }
    }

    async fn call(&self, _ctx: CallCtx, args: &Value) -> R<String> {
        let Some(p) = string_arg(args, "path") else {
            return Err(AgentError::new("path is required"));
        };
        let old_str = string_arg(args, "old_string").unwrap_or("");
        if old_str.is_empty() {
            return Err(AgentError::new("old_string is required"));
        }
        let new_str = string_arg(args, "new_string").unwrap_or("");
        let replace_all = bool_arg(args, "replace_all", false);

        let full = resolve_path(&self.work_dir, p)?;
        check_writable(&self.work_dir, p)?;
        let data = std::fs::read(&full)?;
        let content = String::from_utf8_lossy(&data).into_owned();
        let count = content.matches(old_str).count();
        if count == 0 {
            return Err(AgentError::new(format!("old_string not found in {}", p)));
        }
        if count > 1 && !replace_all {
            return Err(AgentError::new(format!(
                "old_string found {} times in {}; provide more context or set replace_all",
                count, p
            )));
        }
        let updated = if replace_all {
            content.replace(old_str, new_str)
        } else {
            content.replacen(old_str, new_str, 1)
        };
        std::fs::write(&full, updated.as_bytes())?;
        Ok(format!("replaced {} occurrence(s) in {}", count, p))
    }
}

pub struct CreateFolderTool {
    pub work_dir: String,
}

#[async_trait::async_trait]
impl Tool for CreateFolderTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "create_folder".into(),
            description: "Create a directory, including any missing parent directories, inside the working directory.".into(),
            parameters: obj(
                json!({ "path": { "type": "string", "description": "Directory path, relative to the working directory." } }),
                &["path"],
            ),
        }
    }

    async fn call(&self, _ctx: CallCtx, args: &Value) -> R<String> {
        let Some(p) = string_arg(args, "path") else {
            return Err(AgentError::new("path is required"));
        };
        let full = resolve_path(&self.work_dir, p)?;
        // A folder cannot be created on top of an agent's history file.
        check_writable(&self.work_dir, p)?;
        std::fs::create_dir_all(&full)?;
        Ok(format!("created folder {}", p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wd(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("jonnyq-file-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn json(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[tokio::test]
    async fn write_read_edit() {
        let work = wd("wre");
        let ctx = CallCtx {
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let w = WriteFileTool {
            work_dir: work.clone(),
        };
        let out = w
            .call(
                ctx.clone(),
                &json(r#"{"path":"a/b.txt","content":"hello"}"#),
            )
            .await
            .unwrap();
        assert_eq!(out, "wrote 5 bytes to a/b.txt");

        let r = ReadFileTool {
            work_dir: work.clone(),
        };
        assert_eq!(
            r.call(ctx.clone(), &json(r#"{"path":"a/b.txt"}"#))
                .await
                .unwrap(),
            "hello"
        );

        let e = EditFileTool {
            work_dir: work.clone(),
        };
        let out = e
            .call(
                ctx.clone(),
                &json(r#"{"path":"a/b.txt","old_string":"hello","new_string":"hi"}"#),
            )
            .await
            .unwrap();
        assert_eq!(out, "replaced 1 occurrence(s) in a/b.txt");

        let err = e
            .call(
                ctx.clone(),
                &json(r#"{"path":"a/b.txt","old_string":"zzz","new_string":"x"}"#),
            )
            .await;
        assert!(err.unwrap_err().message.contains("old_string not found"));
        let _ = std::fs::remove_dir_all(&work);
    }

    #[tokio::test]
    async fn create_folder_and_traversal_guard() {
        let work = wd("cfd");
        let ctx = CallCtx {
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let c = CreateFolderTool {
            work_dir: work.clone(),
        };
        assert_eq!(
            c.call(ctx.clone(), &json(r#"{"path":"x/y/z"}"#))
                .await
                .unwrap(),
            "created folder x/y/z"
        );
        assert!(c
            .call(ctx.clone(), &json(r#"{"path":"../out"}"#))
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(&work);
    }
}
