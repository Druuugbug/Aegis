//! # ReadDocumentTool
//!
//! Local, pure-Rust extraction of text/Markdown from documents:
//! - **PDF** (`.pdf`)          — digital text via `pdf-extract` (no OCR)
//! - **Excel** (`.xlsx/.xls/.ods`) — cells → Markdown tables via `calamine`
//! - **Word** (`.docx`)        — paragraph text via `dotext`
//! - **PowerPoint** (`.pptx`)  — per-slide text via `dotext`
//!
//! No external runtime is required (no JVM, Python or browser), so this fits the
//! 1c1g / lite profile. Scanned PDFs, complex tables, OCR and formula recovery
//! are out of scope here — those belong to the opt-in `doc_extract_pro` tool
//! (external `opendataloader-pdf`).

use crate::registry::{Tool, ToolContext};
use aegis_security::check_path;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

/// Default output cap (characters). 0 disables truncation.
const DEFAULT_MAX_CHARS: usize = 16_000;
/// Refuse to load files bigger than this to protect memory on small hosts.
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024; // 50 MiB

/// Reads PDF / Word / Excel / PowerPoint into text or Markdown.
pub struct ReadDocumentTool;

impl ReadDocumentTool {
    /// Create a new `ReadDocumentTool`.
    pub fn new() -> Self {
        ReadDocumentTool
    }
}

impl Default for ReadDocumentTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ReadDocumentTool {
    fn name(&self) -> &str {
        "read_document"
    }

    fn description(&self) -> &str {
        "Extract text/Markdown from a local PDF, Word (.docx), Excel (.xlsx/.xls/.ods) or PowerPoint (.pptx) file. Pure-Rust, no OCR (use doc_extract_pro for scanned PDFs)."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the document (within the working directory)" },
                "format": { "type": "string", "enum": ["markdown", "text"], "description": "Output format (default: markdown)" },
                "pages": { "type": "string", "description": "PDF only: page range like '1-5,8' (1-indexed). Default: all pages." },
                "sheet": { "type": "string", "description": "Excel only: sheet name to extract. Default: all sheets." },
                "max_chars": { "type": "integer", "description": "Truncate output to this many characters (default 16000; 0 = no limit)" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let path_str = args["path"].as_str().unwrap_or("").trim();
        if path_str.is_empty() {
            return Ok("Error: path is required".to_string());
        }
        let as_markdown = args["format"].as_str().unwrap_or("markdown") == "markdown";
        let pages = args["pages"].as_str().map(|s| s.to_string());
        let sheet = args["sheet"].as_str().map(|s| s.to_string());
        let max_chars = args["max_chars"]
            .as_u64()
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);

        // Path safety: confine to the working directory.
        let safe_path = check_path(path_str, &ctx.cwd)?;
        if !safe_path.exists() {
            anyhow::bail!("File not found: {path_str}");
        }
        if let Ok(meta) = std::fs::metadata(&safe_path) {
            if meta.len() > MAX_FILE_BYTES {
                anyhow::bail!(
                    "File too large ({} bytes, limit {}). Try a smaller file or use doc_extract_pro.",
                    meta.len(),
                    MAX_FILE_BYTES
                );
            }
        }

        let ext = safe_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        // CPU-bound parsing on a blocking thread so we don't stall the runtime.
        let path_owned = safe_path.to_path_buf();
        let ext_c = ext.clone();
        let result = tokio::task::spawn_blocking(move || {
            extract_document(&path_owned, &ext_c, as_markdown, pages, sheet)
        })
        .await
        .map_err(|e| anyhow::anyhow!("document parsing task failed: {e}"))?;

        let content = match result {
            Ok(c) => c,
            Err(e) => return Ok(format!("Failed to read {path_str}: {e}")),
        };

        if content.trim().is_empty() {
            return Ok(format!(
                "No extractable text found in {path_str} (it may be a scanned/image-only document — try doc_extract_pro with OCR)."
            ));
        }

        if max_chars > 0 && content.chars().count() > max_chars {
            let truncated: String = content.chars().take(max_chars).collect();
            Ok(format!(
                "{truncated}\n\n... [content truncated at {max_chars} characters. Narrow with 'pages'/'sheet' or raise max_chars.]"
            ))
        } else {
            Ok(content)
        }
    }
}

/// Dispatch by extension. Runs synchronously (call from a blocking thread).
fn extract_document(
    path: &Path,
    ext: &str,
    markdown: bool,
    pages: Option<String>,
    sheet: Option<String>,
) -> Result<String> {
    match ext {
        "pdf" => extract_pdf(path, markdown, pages.as_deref()),
        "xlsx" | "xls" | "xlsm" | "xlsb" | "ods" => extract_spreadsheet(path, sheet.as_deref()),
        "docx" => extract_docx(path),
        "pptx" => extract_pptx(path, markdown),
        other => anyhow::bail!(
            "Unsupported document type '.{other}'. Supported: pdf, docx, xlsx/xls/ods, pptx."
        ),
    }
}

/// Parse a 1-indexed page-range spec like "1-5,8,10-12" into a set predicate.
fn page_selected(spec: &Option<&str>, page_1indexed: usize) -> bool {
    let Some(spec) = spec else {
        return true;
    };
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                if page_1indexed >= a && page_1indexed <= b {
                    return true;
                }
            }
        } else if let Ok(n) = part.parse::<usize>() {
            if page_1indexed == n {
                return true;
            }
        }
    }
    false
}

fn extract_pdf(path: &Path, markdown: bool, pages: Option<&str>) -> Result<String> {
    let by_pages = pdf_extract::extract_text_by_pages(path)
        .map_err(|e| anyhow::anyhow!("PDF text extraction failed: {e}"))?;
    let mut out = String::new();
    for (i, page_text) in by_pages.iter().enumerate() {
        let page_no = i + 1;
        if !page_selected(&pages, page_no) {
            continue;
        }
        let trimmed = page_text.trim();
        if trimmed.is_empty() {
            continue;
        }
        if markdown {
            out.push_str(&format!("## Page {page_no}\n\n{trimmed}\n\n"));
        } else {
            out.push_str(trimmed);
            out.push_str("\n\n");
        }
    }
    Ok(out.trim_end().to_string())
}

fn extract_spreadsheet(path: &Path, sheet: Option<&str>) -> Result<String> {
    use calamine::{open_workbook_auto, Reader};

    let mut workbook =
        open_workbook_auto(path).map_err(|e| anyhow::anyhow!("cannot open spreadsheet: {e}"))?;
    let names: Vec<String> = workbook.sheet_names().to_vec();
    let mut out = String::new();

    for name in names {
        if let Some(want) = sheet {
            if !name.eq_ignore_ascii_case(want) {
                continue;
            }
        }
        let range = match workbook.worksheet_range(&name) {
            Ok(r) => r,
            Err(e) => {
                out.push_str(&format!(
                    "# Sheet: {name}\n\n(error reading sheet: {e})\n\n"
                ));
                continue;
            }
        };
        out.push_str(&format!("# Sheet: {name}\n\n"));

        let mut rows = range.rows().peekable();
        // Header row (first row) → Markdown table header + separator.
        if let Some(first) = rows.next() {
            let header = row_to_cells(first);
            out.push_str(&format!("| {} |\n", header.join(" | ")));
            out.push_str(&format!(
                "| {} |\n",
                header.iter().map(|_| "---").collect::<Vec<_>>().join(" | ")
            ));
            for row in rows {
                let cells = row_to_cells(row);
                out.push_str(&format!("| {} |\n", cells.join(" | ")));
            }
        }
        out.push('\n');
    }

    if sheet.is_some() && out.is_empty() {
        anyhow::bail!("sheet '{}' not found", sheet.unwrap());
    }
    Ok(out.trim_end().to_string())
}

/// Render a calamine row of `Data` into Markdown-safe cell strings.
fn row_to_cells(row: &[calamine::Data]) -> Vec<String> {
    row.iter()
        .map(|c| {
            let s = match c {
                calamine::Data::Empty => String::new(),
                other => other.to_string(),
            };
            // Escape pipes so cells don't break the Markdown table.
            s.replace('|', "\\|").replace('\n', " ").trim().to_string()
        })
        .collect()
}

fn extract_docx(path: &Path) -> Result<String> {
    use dotext::MsDoc;
    use std::io::Read;

    let mut file =
        dotext::Docx::open(path).map_err(|e| anyhow::anyhow!("cannot open .docx: {e}"))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| anyhow::anyhow!("cannot read .docx: {e}"))?;
    Ok(text.trim().to_string())
}

fn extract_pptx(path: &Path, markdown: bool) -> Result<String> {
    use dotext::MsDoc;
    use std::io::Read;

    let mut file =
        dotext::Pptx::open(path).map_err(|e| anyhow::anyhow!("cannot open .pptx: {e}"))?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| anyhow::anyhow!("cannot read .pptx: {e}"))?;
    let text = text.trim();
    if markdown {
        // dotext separates slides with newlines; present as a single section.
        Ok(format!("## Slides\n\n{text}"))
    } else {
        Ok(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_selected_ranges() {
        assert!(page_selected(&Some("1-5,8"), 3));
        assert!(page_selected(&Some("1-5,8"), 8));
        assert!(!page_selected(&Some("1-5,8"), 7));
        assert!(page_selected(&None, 42)); // no spec = all pages
        assert!(!page_selected(&Some("2"), 1));
        assert!(page_selected(&Some("2"), 2));
    }

    #[test]
    fn test_row_to_cells_escapes_pipes() {
        let row = vec![
            calamine::Data::String("a|b".to_string()),
            calamine::Data::Empty,
            calamine::Data::Int(42),
        ];
        let cells = row_to_cells(&row);
        assert_eq!(cells[0], "a\\|b");
        assert_eq!(cells[1], "");
        assert_eq!(cells[2], "42");
    }

    #[test]
    fn test_unsupported_ext() {
        let err = extract_document(Path::new("/tmp/x.rtf"), "rtf", true, None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Unsupported"));
    }
}
