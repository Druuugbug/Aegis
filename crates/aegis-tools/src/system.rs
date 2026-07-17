//! # SystemStatusTool
//!
//! Lets the agent see the machine it runs on: CPU, memory, swap, load average,
//! disks, uptime and OS info. Backed by `sysinfo` (pure-Rust, cross-platform).
//! Read-only and light — the cornerstone of Aegis's server-resident scenario.

use crate::registry::{Tool, ToolContext};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};

/// Reports host CPU/memory/disk/load/uptime.
pub struct SystemStatusTool;

impl SystemStatusTool {
    /// Create a new `SystemStatusTool`.
    pub fn new() -> Self {
        SystemStatusTool
    }
}

impl Default for SystemStatusTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for SystemStatusTool {
    fn name(&self) -> &str {
        "system_status"
    }

    fn description(&self) -> &str {
        "Report the host's current status: OS, uptime, load average, CPU usage, memory/swap usage, and disk usage. Read-only."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {}
        })
    }

    async fn execute(&self, _args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        // sysinfo is synchronous and CPU sampling needs a short sleep, so run
        // it on a blocking thread to avoid stalling the async runtime.
        let out = tokio::task::spawn_blocking(collect_status)
            .await
            .map_err(|e| anyhow::anyhow!("system_status task failed: {e}"))?;
        Ok(out)
    }
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

fn pct(used: u64, total: u64) -> String {
    if total == 0 {
        "n/a".to_string()
    } else {
        format!("{:.0}%", (used as f64 / total as f64) * 100.0)
    }
}

fn collect_status() -> String {
    use sysinfo::{Disks, System};

    let mut sys = System::new_all();
    // CPU usage needs two samples separated by a short interval.
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_cpu_usage();

    let mut out = String::new();

    // Host / OS
    out.push_str(&format!(
        "Host: {}\nOS: {} {} (kernel {})\nArch: {}\n",
        System::host_name().unwrap_or_else(|| "unknown".into()),
        System::name().unwrap_or_else(|| "unknown".into()),
        System::os_version().unwrap_or_else(|| "".into()),
        System::kernel_version().unwrap_or_else(|| "unknown".into()),
        System::cpu_arch(),
    ));

    // Uptime
    let up = System::uptime();
    let days = up / 86400;
    let hours = (up % 86400) / 3600;
    let mins = (up % 3600) / 60;
    out.push_str(&format!("Uptime: {days}d {hours}h {mins}m\n"));

    // Load average
    let la = System::load_average();
    out.push_str(&format!(
        "Load average: {:.2} {:.2} {:.2} (1m/5m/15m)\n",
        la.one, la.five, la.fifteen
    ));

    // CPU
    out.push_str(&format!(
        "CPU: {:.1}% used across {} logical core(s)\n",
        sys.global_cpu_usage(),
        sys.cpus().len(),
    ));

    // Memory / swap
    let (tm, um) = (sys.total_memory(), sys.used_memory());
    out.push_str(&format!(
        "Memory: {} / {} ({} used)\n",
        human_bytes(um),
        human_bytes(tm),
        pct(um, tm)
    ));
    let (ts, us) = (sys.total_swap(), sys.used_swap());
    if ts > 0 {
        out.push_str(&format!(
            "Swap: {} / {} ({} used)\n",
            human_bytes(us),
            human_bytes(ts),
            pct(us, ts)
        ));
    }

    // Disks
    let disks = Disks::new_with_refreshed_list();
    if disks.is_empty() {
        out.push_str("Disks: (none reported)\n");
    } else {
        out.push_str("Disks:\n");
        for d in &disks {
            let total = d.total_space();
            let avail = d.available_space();
            let used = total.saturating_sub(avail);
            out.push_str(&format!(
                "  {} at {} — {} / {} ({} used)\n",
                d.name().to_string_lossy(),
                d.mount_point().display(),
                human_bytes(used),
                human_bytes(total),
                pct(used, total),
            ));
        }
    }

    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(0), "0.0 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }

    #[test]
    fn pct_handles_zero() {
        assert_eq!(pct(0, 0), "n/a");
        assert_eq!(pct(1, 2), "50%");
    }
}

/// Read-only process listing (top consumers by CPU or memory).
pub struct ProcessListTool;

impl ProcessListTool {
    /// Create a new `ProcessListTool`.
    pub fn new() -> Self {
        ProcessListTool
    }
}

impl Default for ProcessListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ProcessListTool {
    fn name(&self) -> &str {
        "process"
    }

    fn description(&self) -> &str {
        "List the top running processes by CPU or memory usage (read-only). Optionally filter by name."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "sort_by": { "type": "string", "enum": ["cpu", "memory"], "description": "Sort key (default cpu)" },
                "limit": { "type": "integer", "description": "How many processes to show (default 15)" },
                "name": { "type": "string", "description": "Only show processes whose name contains this substring" }
            }
        })
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext<'_>) -> Result<String> {
        let sort_by = args["sort_by"].as_str().unwrap_or("cpu").to_string();
        let limit = args["limit"].as_u64().unwrap_or(15) as usize;
        let name_filter = args["name"].as_str().map(|s| s.to_lowercase());

        let out =
            tokio::task::spawn_blocking(move || collect_processes(&sort_by, limit, name_filter))
                .await
                .map_err(|e| anyhow::anyhow!("process task failed: {e}"))?;
        Ok(out)
    }
}

fn collect_processes(sort_by: &str, limit: usize, name_filter: Option<String>) -> String {
    use sysinfo::{ProcessesToUpdate, System};

    let mut sys = System::new_all();
    // Second sample for accurate CPU usage.
    std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
    sys.refresh_processes(ProcessesToUpdate::All, true);

    let mut rows: Vec<(String, u32, f32, u64)> = sys
        .processes()
        .iter()
        .map(|(pid, p)| {
            (
                p.name().to_string_lossy().to_string(),
                pid.as_u32(),
                p.cpu_usage(),
                p.memory(),
            )
        })
        .filter(|(name, _, _, _)| {
            name_filter
                .as_ref()
                .map(|f| name.to_lowercase().contains(f))
                .unwrap_or(true)
        })
        .collect();

    if sort_by == "memory" {
        rows.sort_by(|a, b| b.3.cmp(&a.3));
    } else {
        rows.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    }
    rows.truncate(limit);

    if rows.is_empty() {
        return "(no matching processes)".to_string();
    }

    let mut out = format!("Top {} processes by {}:\n", rows.len(), sort_by);
    out.push_str(&format!(
        "{:>8}  {:>6}  {:>10}  {}\n",
        "PID", "CPU%", "MEM", "NAME"
    ));
    for (name, pid, cpu, mem) in rows {
        out.push_str(&format!(
            "{pid:>8}  {cpu:>5.1}  {:>10}  {name}\n",
            human_bytes(mem)
        ));
    }
    out.trim_end().to_string()
}
