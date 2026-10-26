//! Reads PDF files by shelling out to poppler-utils (pdftotext, pdftoppm)
//! rather than linking a PDF library into the binary, per the original Go
//! design. Both binaries must be installed and on PATH.
//!
//! Port of Go `internal/tools/read_pdf.go`.

use std::sync::atomic::{AtomicI64, Ordering};

use serde_json::{json, Value};

use super::{guard::resolve_path, string_arg, CallCtx, Tool};
use crate::config::Config;
use crate::error::{AgentError, R};
use crate::llm::ToolSpec;

pub struct PdfTool {
    pub work_dir: String,
    max_pages: AtomicI64,
    dpi: AtomicI64,
}

impl PdfTool {
    pub fn new(work_dir: &str, max_pages: i64, dpi: i64) -> Self {
        Self {
            work_dir: work_dir.to_string(),
            max_pages: AtomicI64::new(max_pages),
            dpi: AtomicI64::new(dpi),
        }
    }

    fn max_pages(&self) -> i64 {
        self.max_pages.load(Ordering::SeqCst)
    }

    fn dpi(&self) -> i64 {
        self.dpi.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Tool for PdfTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_pdf".into(),
            description: "Extract text from a PDF file inside the working directory (up to the configured max pages). If the PDF has no extractable text (e.g. a scanned document), its pages are rendered to PNG images instead and their paths are returned for use with read_pic.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "PDF file path, relative to the working directory." } },
                "required": ["path"],
            }),
        }
    }

    fn configure(&self, cfg: &Config) {
        self.max_pages.store(cfg.pdf_max_pages, Ordering::SeqCst);
        self.dpi.store(cfg.pdf_dpi, Ordering::SeqCst);
    }

    async fn call(&self, ctx: CallCtx, args: &Value) -> R<String> {
        let Some(p) = string_arg(args, "path") else {
            return Err(AgentError::new("path is required"));
        };
        let full = resolve_path(&self.work_dir, p)?;

        let max_pages = if self.max_pages() <= 0 {
            50
        } else {
            self.max_pages()
        };

        if which("pdftotext").is_none() {
            return Err(AgentError::new(
                "read_pdf requires poppler-utils (pdftotext not found on PATH)",
            ));
        }

        let text_out = run_capture(
            &ctx,
            "pdftotext",
            &[
                "-layout".to_string(),
                "-f".to_string(),
                "1".to_string(),
                "-l".to_string(),
                max_pages.to_string(),
                full.to_string_lossy().to_string(),
                "-".to_string(),
            ],
        )
        .await
        .map_err(|e| AgentError::new(format!("pdftotext failed: {}", e.message)))?;

        if !String::from_utf8_lossy(&text_out.0).trim().is_empty() {
            return Ok(String::from_utf8_lossy(&text_out.0).into_owned());
        }

        // No extractable text - likely a scanned PDF. Render pages to images.
        if which("pdftoppm").is_none() {
            return Err(AgentError::new(
                "pdf has no extractable text and pdftoppm is not on PATH to render page images",
            ));
        }
        let dpi = if self.dpi() <= 0 { 150 } else { self.dpi() };
        let file_name = full
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "document".to_string());
        let stem = match file_name.rsplit_once('.') {
            Some((s, _)) if !s.is_empty() => s.to_string(),
            _ => file_name.clone(),
        };
        let out_dir = std::path::Path::new(&self.work_dir)
            .join(".jonnyq")
            .join("pdf_pages")
            .join(&stem);
        std::fs::create_dir_all(&out_dir)?;
        let prefix = out_dir.join("page");
        run_capture(
            &ctx,
            "pdftoppm",
            &[
                "-r".to_string(),
                dpi.to_string(),
                "-png".to_string(),
                "-f".to_string(),
                "1".to_string(),
                "-l".to_string(),
                max_pages.to_string(),
                full.to_string_lossy().to_string(),
                prefix.to_string_lossy().to_string(),
            ],
        )
        .await
        .map_err(|e| AgentError::new(format!("pdftoppm failed: {}", e.message)))?;

        let rel = full
            .strip_prefix(&self.work_dir)
            .map(|r| r.to_string_lossy().trim_start_matches('/').to_string())
            .unwrap_or_else(|_| String::new());
        // Go reports the dir relative to workDir; rebuild it from the pieces.
        let rel_dir = format!(".jonnyq/pdf_pages/{}", stem);
        let _ = rel;
        Ok(format!(
            "no extractable text; rendered pages at {} DPI to {}/page-*.png - use read_pic on those files",
            dpi, rel_dir
        ))
    }
}

/// Runs a command, returning (stdout, stderr). Errors (non-zero exit or spawn
/// failure) come back with stderr appended, like Go's runCapture.
async fn run_capture(ctx: &CallCtx, name: &str, args: &[String]) -> R<(Vec<u8>, String)> {
    let mut cmd = tokio::process::Command::new(name);
    cmd.args(args);
    let out = tokio::select! {
        o = cmd.output() => o.map_err(|e| AgentError::new(e.to_string()))?,
        _ = ctx.cancel.cancelled() => return Err(AgentError::new("context canceled")),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !stderr.trim().is_empty() {
            return Err(AgentError::new(format!(
                "exit status {}: {}",
                out.status.code().unwrap_or(-1),
                stderr
            )));
        }
        return Err(AgentError::new(format!(
            "exit status {}",
            out.status.code().unwrap_or(-1)
        )));
    }
    Ok((
        out.stdout,
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        if dir.is_empty() {
            continue;
        }
        let candidate = std::path::Path::new(dir).join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
