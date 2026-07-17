//! # Host self-inspection tools (zero new deps)
//!
//! - **disk_usage**: `du`-style directory size analysis via `std::fs`.
//! - **listening_ports**: parse `/proc/net/tcp{,6}` to list listening sockets
//!   (Linux only; degrades gracefully elsewhere).
//!
//! Both are read-only, light, and default-on (Core tier).

use crate::registry::{Tool, ToolContext};
use aegis_security::check_path;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::Path;

// ═══════════════════════════════════════════
// disk_usage
// ═══════════════════════════════════════════

/// Analyzes directory size (du-style).
pub struct DiskUsageTool;

impl DiskUsageTool {
    /// Create a new `DiskUsageTool`.
    pub fn new() -> Self {
        DiskUsageTool
    }
}

impl Default for DiskUsageTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for DiskUsageTool {
    fn name(&self) -> &str {
        "disk_usage"
    }

    fn description(&self) -> &str {
        "Analyze disk usage of a directory (du-style): total size plus the largest top-level children. Read-only, within the working directory."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to analyze (default: working directory)" },
                "top": { "type": "integer", "description": "How many largest children to list (default 20)" }
            }
        })
    }

    async fn execute(&self, args: Value, ctx: &ToolContext<'_>) -> Result<String> {
        let path_arg = args["path"].as_str().unwrap_or(".");
        let top = args["top"].as_u64().unwrap_or(20) as usize;
        let dir = check_path(path_arg, &ctx.cwd)?;
        if !dir.exists() {
            anyhow::bail!("Path not found: {path_arg}");
        }
        if !dir.is_dir() {
            // A single file: just report its size.
            let size = std::fs::metadata(&dir).map(|m| m.len()).unwrap_or(0);
            return Ok(format!("{}  {}", human_bytes(size), path_arg));
        }

        let dir_owned = dir.clone();
        let (total, mut children) = tokio::task::spawn_blocking(move || {
            let mut children: Vec<(String, u64)> = Vec::new();
            let mut total = 0u64;
            if let Ok(rd) = std::fs::read_dir(&dir_owned) {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let sz = dir_size(&entry.path());
                    total += sz;
                    children.push((name, sz));
                }
            }
            (total, children)
        })
        .await
        .map_err(|e| anyhow::anyhow!("disk_usage task failed: {e}"))?;

        children.sort_by(|a, b| b.1.cmp(&a.1));
        children.truncate(top);

        let mut out = format!("Total: {}  ({})\n", human_bytes(total), path_arg);
        for (name, sz) in children {
            out.push_str(&format!("{:>10}  {}\n", human_bytes(sz), name));
        }
        Ok(out.trim_end().to_string())
    }
}

/// Recursively sum the size of a path (follows no symlinks; best-effort).
fn dir_size(path: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if meta.is_file() {
        return meta.len();
    }
    if meta.is_dir() {
        let mut total = 0u64;
        if let Ok(rd) = std::fs::read_dir(path) {
            for entry in rd.flatten() {
                total += dir_size(&entry.path());
            }
        }
        return total;
    }
    0
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", UNITS[i])
}

// ═══════════════════════════════════════════
// listening_ports
// ═══════════════════════════════════════════

/// Lists listening TCP sockets by parsing `/proc/net/tcp{,6}` (Linux).
pub struct ListeningPortsTool;

impl ListeningPortsTool {
    /// Create a new `ListeningPortsTool`.
    pub fn new() -> Self {
        ListeningPortsTool
    }
}

impl Default for ListeningPortsTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ListeningPortsTool {
    fn name(&self) -> &str {
        "listening_ports"
    }

    fn description(&self) -> &str {
        "List TCP sockets in the LISTEN state (what's listening on this host) by reading /proc/net/tcp and /proc/net/tcp6. Linux only, read-only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let mut listeners: Vec<String> = Vec::new();
        let mut any_file = false;

        for (proc_path, is_v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
            if let Ok(content) = std::fs::read_to_string(proc_path) {
                any_file = true;
                for line in content.lines().skip(1) {
                    if let Some(addr) = parse_listen_line(line, is_v6) {
                        listeners.push(addr);
                    }
                }
            }
        }

        if !any_file {
            return Ok(
                "listening_ports is only supported on Linux (could not read /proc/net/tcp)."
                    .to_string(),
            );
        }
        if listeners.is_empty() {
            return Ok("No listening TCP sockets found.".to_string());
        }
        listeners.sort();
        listeners.dedup();
        Ok(format!("Listening TCP sockets:\n{}", listeners.join("\n")))
    }
}

/// Parse one `/proc/net/tcp{,6}` row; return `Some("addr:port")` if it is in the
/// LISTEN state (`st == 0A`), else `None`.
fn parse_listen_line(line: &str, is_v6: bool) -> Option<String> {
    let fields: Vec<&str> = line.split_whitespace().collect();
    // Fields: sl local_address rem_address st ...
    if fields.len() < 4 {
        return None;
    }
    let st = fields[3];
    if !st.eq_ignore_ascii_case("0A") {
        return None; // not LISTEN
    }
    let (addr_hex, port_hex) = fields[1].split_once(':')?;
    let port = u16::from_str_radix(port_hex, 16).ok()?;
    let ip = if is_v6 {
        parse_ipv6_hex(addr_hex)?
    } else {
        parse_ipv4_hex(addr_hex)?
    };
    if is_v6 {
        Some(format!("[{ip}]:{port}"))
    } else {
        Some(format!("{ip}:{port}"))
    }
}

/// `/proc` stores the IPv4 address as little-endian hex, e.g. `0100007F` = 127.0.0.1.
fn parse_ipv4_hex(hex: &str) -> Option<String> {
    if hex.len() != 8 {
        return None;
    }
    let n = u32::from_str_radix(hex, 16).ok()?;
    let b = n.to_le_bytes(); // little-endian → network order bytes
    Some(std::net::Ipv4Addr::new(b[0], b[1], b[2], b[3]).to_string())
}

/// `/proc` stores IPv6 as 4 little-endian 32-bit words (32 hex chars).
fn parse_ipv6_hex(hex: &str) -> Option<String> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for word in 0..4 {
        let chunk = &hex[word * 8..word * 8 + 8];
        let n = u32::from_str_radix(chunk, 16).ok()?;
        let le = n.to_le_bytes();
        bytes[word * 4..word * 4 + 4].copy_from_slice(&le);
    }
    Some(std::net::Ipv6Addr::from(bytes).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_hex_is_little_endian() {
        assert_eq!(parse_ipv4_hex("0100007F").as_deref(), Some("127.0.0.1"));
        assert_eq!(parse_ipv4_hex("00000000").as_deref(), Some("0.0.0.0"));
        assert_eq!(parse_ipv4_hex("bad"), None);
    }

    #[test]
    fn listen_line_filters_state() {
        // A LISTEN row (st = 0A) on 0.0.0.0:22 (port 0x0016 = 22).
        let listen =
            "  0: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000  0";
        assert_eq!(
            parse_listen_line(listen, false).as_deref(),
            Some("0.0.0.0:22")
        );
        // An ESTABLISHED row (st = 01) is ignored.
        let est = "  1: 0100007F:1F90 0100007F:C000 01 00000000:00000000 00:00000000 00000000  0";
        assert_eq!(parse_listen_line(est, false), None);
    }

    #[test]
    fn human_bytes_ok() {
        assert_eq!(human_bytes(1536), "1.5 KiB");
    }
}
