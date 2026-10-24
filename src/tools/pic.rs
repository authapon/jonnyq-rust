//! Reads an image file and returns it base64-encoded as a data URI,
//! restricted to the working directory.
//!
//! Port of Go `internal/tools/read_pic.go`. Note: like the Go version, this
//! returns the encoded image as a plain text tool result; wiring it into a
//! provider's native multimodal message format (Ollama's message.images /
//! OpenAI's image_url content parts) would require extending Message beyond
//! a plain string body.

use base64::Engine;
use serde_json::{json, Value};

use super::{guard::resolve_path, string_arg, CallCtx, Tool};
use crate::error::{AgentError, R};
use crate::llm::ToolSpec;

const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;

pub struct PicTool {
    pub work_dir: String,
}

impl PicTool {
    pub fn new(work_dir: &str) -> Self {
        Self { work_dir: work_dir.to_string() }
    }
}

#[async_trait::async_trait]
impl Tool for PicTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_pic".into(),
            description: "Read an image file inside the working directory and return it as a base64 data URI.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Image file path, relative to the working directory." } },
                "required": ["path"],
            }),
        }
    }

    async fn call(&self, _ctx: CallCtx, args: &Value) -> R<String> {
        let Some(p) = string_arg(args, "path") else {
            return Err(AgentError::new("path is required"));
        };
        let full = resolve_path(&self.work_dir, p)?;
        let info = std::fs::metadata(&full)?;
        if info.len() > MAX_IMAGE_BYTES {
            return Err(AgentError::new(format!(
                "image is {} bytes, exceeds max of {} bytes",
                info.len(),
                MAX_IMAGE_BYTES
            )));
        }
        let data = std::fs::read(&full)?;
        let mime = detect_content_type(&data);
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        Ok(format!("data:{};base64,{}", mime, encoded))
    }
}

/// Mimics net/http.DetectContentType for the cases that matter here (it
/// sniffs the first 512 bytes and defaults to application/octet-stream).
fn detect_content_type(data: &[u8]) -> &'static str {
    let n = data.len().min(512);
    let b = &data[..n];
    if b.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        "image/gif"
    } else if b.starts_with(b"%PDF") {
        "application/pdf"
    } else if b.starts_with(b"BM") {
        "image/bmp"
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        "image/webp"
    } else if b.len() >= 12 && &b[4..8] == b"ftyp" && (&b[8..11] == b"avif" || &b[8..11] == b"avis") {
        "image/avif"
    } else if b.starts_with(b"<svg") || b.starts_with(b"<?xml") && b.windows(20).any(|w| w.starts_with(b"<svg")) {
        "image/svg+xml"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffing() {
        assert_eq!(detect_content_type(b"\x89PNG\r\n\x1a\n...."), "image/png");
        assert_eq!(detect_content_type(b"\xff\xd8\xff\xe0...."), "image/jpeg");
        assert_eq!(detect_content_type(b"whatever"), "application/octet-stream");
    }

    #[tokio::test]
    async fn read_pic_roundtrip() {
        let dir = std::env::temp_dir().join(format!("jonnyq-pic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), b"\x89PNG\r\n\x1a\nabc").unwrap();
        let t = PicTool::new(&dir.to_string_lossy());
        let ctx = CallCtx { cancel: tokio_util::sync::CancellationToken::new() };
        let out = t.call(ctx, &json!({"path": "a.png"})).await.unwrap();
        assert!(out.starts_with("data:image/png;base64,"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
