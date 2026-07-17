use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

const LARGE_OUTPUT_THRESHOLD: usize = 64 * 1024; // 64KB
const RECEIPT_HEAD_LINES: usize = 8;
const RECEIPT_TAIL_LINES: usize = 8;
const MAX_RECEIPT_LINE_CHARS: usize = 240;
const MAX_RECEIPT_CMD_CHARS: usize = 500;
const FULL_OUTPUT_PREFIX: &str = "[aegis:full-output=";
const FULL_OUTPUT_SUFFIX: &str = "]";

/// If output is larger than 64KB, write it to the persistent tool-runs area
/// and return a summary with a path reference.
pub fn guard_output_size(output: String) -> String {
    if output.len() <= LARGE_OUTPUT_THRESHOLD {
        return output;
    }

    let mut hasher = DefaultHasher::new();
    output.hash(&mut hasher);
    let hash = hasher.finish();

    let dir = tool_runs_dir().join("buffer");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!("Failed to create output buffer dir: {e}");
        return format!(
            "{}...\n[output truncated: {} bytes total, buffer dir unavailable]",
            &output[..output.floor_char_boundary(LARGE_OUTPUT_THRESHOLD)],
            output.len()
        );
    }

    let path = dir.join(format!("{hash:016x}.txt"));
    match std::fs::write(&path, output.as_bytes()) {
        Ok(_) => format!(
            "[Large output ({} bytes) saved to: {}]\nFirst 2KB preview:\n{}",
            output.len(),
            path.display(),
            &output[..output.floor_char_boundary(2048)]
        ),
        Err(e) => {
            tracing::warn!("Failed to write output buffer: {e}");
            format!(
                "{}...\n[output truncated: {} bytes total]",
                &output[..output.floor_char_boundary(LARGE_OUTPUT_THRESHOLD)],
                output.len()
            )
        }
    }
}

/// Build a bounded, persistent terminal execution receipt. The provided output
/// should already be credential-sanitized by the caller.
pub fn terminal_receipt(
    session_id: &str,
    command: &str,
    cwd: &Path,
    exit_code: i32,
    elapsed_ms: u128,
    output: &str,
) -> String {
    let run_id = tool_run_id(session_id, command, output);
    let session_dir = tool_runs_dir().join(safe_component(session_id, "session"));
    let path = session_dir.join(format!("{run_id}.log"));
    let saved = std::fs::create_dir_all(&session_dir)
        .and_then(|_| std::fs::write(&path, output.as_bytes()))
        .map(|_| path);

    let line_count = output.lines().count();
    let byte_count = output.len();
    let cmd = truncate_chars(command, MAX_RECEIPT_CMD_CHARS);

    let mut receipt = String::new();
    receipt.push_str("terminal receipt\n");
    receipt.push_str(&format!("cmd: {cmd}\n"));
    receipt.push_str(&format!("cwd: {}\n", cwd.display()));
    receipt.push_str(&format!("exit: {exit_code} · {elapsed_ms}ms\n"));

    match saved {
        Ok(path) => {
            receipt.push_str(&format!(
                "output: {line_count} lines · {byte_count} bytes · full saved to {}\n",
                path.display()
            ));
            receipt.push_str(&format!(
                "{FULL_OUTPUT_PREFIX}{}{FULL_OUTPUT_SUFFIX}\n",
                path.display()
            ));
        }
        Err(e) => {
            receipt.push_str(&format!(
                "output: {line_count} lines · {byte_count} bytes · full output save failed: {e}\n"
            ));
        }
    }

    if output.trim().is_empty() {
        receipt.push_str("\n--- output preview ---\n(empty)\n");
    } else {
        receipt.push_str("\n--- output preview ---\n");
        receipt.push_str(&bounded_preview(output));
        if !receipt.ends_with('\n') {
            receipt.push('\n');
        }
    }

    receipt
}

/// Extract a full-output path from a terminal receipt. Only paths under
/// `<config_dir>/logs/tool-runs` are accepted.
pub fn receipt_full_output_path(receipt: &str) -> Option<PathBuf> {
    let base = tool_runs_dir();
    for line in receipt.lines() {
        let Some(raw) = line
            .strip_prefix(FULL_OUTPUT_PREFIX)
            .and_then(|s| s.strip_suffix(FULL_OUTPUT_SUFFIX))
        else {
            continue;
        };
        let path = PathBuf::from(raw);
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }
        if path.starts_with(&base) {
            return Some(path);
        }
    }
    None
}

/// Read the full output referenced by a receipt. Returns `None` when the text is
/// not a receipt; returns an explanatory string when the referenced file cannot
/// be read.
pub fn read_receipt_full_output(receipt: &str) -> Option<String> {
    let path = receipt_full_output_path(receipt)?;
    Some(
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| format!("Failed to read full tool output {}: {e}", path.display())),
    )
}

fn tool_runs_dir() -> PathBuf {
    aegis_types::paths::config_dir()
        .join("logs")
        .join("tool-runs")
}

fn tool_run_id(session_id: &str, command: &str, output: &str) -> String {
    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    command.hash(&mut hasher);
    output.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{}-{hash:016x}", chrono::Utc::now().timestamp_millis())
}

fn safe_component(raw: &str, fallback: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .take(96)
        .collect();
    while out.contains("..") {
        out = out.replace("..", ".");
    }
    let out = out.trim_matches(['.', '-']).to_string();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

fn bounded_preview(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    if lines.len() <= RECEIPT_HEAD_LINES + RECEIPT_TAIL_LINES {
        return lines
            .iter()
            .map(|line| truncate_chars(line, MAX_RECEIPT_LINE_CHARS))
            .collect::<Vec<_>>()
            .join("\n");
    }

    let mut out = String::new();
    for line in &lines[..RECEIPT_HEAD_LINES] {
        out.push_str(&truncate_chars(line, MAX_RECEIPT_LINE_CHARS));
        out.push('\n');
    }
    out.push_str(&format!(
        "... [{} lines omitted] ...\n",
        lines.len() - RECEIPT_HEAD_LINES - RECEIPT_TAIL_LINES
    ));
    for line in &lines[lines.len() - RECEIPT_TAIL_LINES..] {
        out.push_str(&truncate_chars(line, MAX_RECEIPT_LINE_CHARS));
        out.push('\n');
    }
    out
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_small_output_passes_through() {
        let output = "hello world".to_string();
        assert_eq!(guard_output_size(output.clone()), output);
    }

    #[test]
    fn test_empty_output() {
        assert_eq!(guard_output_size(String::new()), "");
    }

    #[test]
    fn test_exactly_threshold_passes_through() {
        let output = "a".repeat(LARGE_OUTPUT_THRESHOLD);
        let result = guard_output_size(output.clone());
        assert_eq!(result, output);
    }

    #[test]
    fn test_large_output_writes_to_file() {
        let output = "x".repeat(LARGE_OUTPUT_THRESHOLD + 1000);
        let result = guard_output_size(output);
        assert!(result.contains("Large output"));
        assert!(result.contains("saved to"));
        assert!(result.contains("preview"));
    }

    #[test]
    fn terminal_receipt_is_bounded_and_expandable() {
        let output = (0..40)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let receipt = terminal_receipt(
            "session/test",
            "printf lines",
            Path::new("/tmp"),
            0,
            12,
            &output,
        );
        assert!(receipt.contains("terminal receipt"));
        assert!(receipt.contains("lines omitted"));
        assert!(receipt_full_output_path(&receipt).is_some());
        assert_eq!(read_receipt_full_output(&receipt).unwrap(), output);
    }

    #[test]
    fn receipt_path_rejects_non_tool_run_paths() {
        assert!(receipt_full_output_path("[aegis:full-output=/etc/passwd]").is_none());
    }
}
