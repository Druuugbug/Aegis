//! Proactive monitoring (watcher) — the resident daemon runs each `[[watch]]`
//! on a schedule, evaluates a trigger over the check's output, and pushes an
//! alert (Feishu) + logs it when the condition fires. Checks are plain shell
//! commands (no LLM cost), suited to a 1c1g resident.

use std::collections::HashMap;
use std::io::Write as _;
use std::time::{Duration, Instant};

use aegis_core::channel::{Channel, OutboundMessage};
use aegis_core::config::{Config, GatewayFeishuConfig, SelfWatchConfig, WatchConfig};
use aegis_core::feishu_channel::FeishuChannel;
use tracing::{info, warn};

/// Parse a `every <N>[s|m|h]` schedule into a polling interval. Falls back to
/// 5 minutes if it can't be parsed.
fn parse_schedule(s: &str) -> Duration {
    let spec = s.trim().strip_prefix("every").unwrap_or(s).trim();
    // spec like "5m", "30s", "1h"
    let (num, unit): (String, char) = {
        let mut digits = String::new();
        let mut unit = 'm';
        for c in spec.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
            } else if c.is_ascii_alphabetic() {
                unit = c.to_ascii_lowercase();
                break;
            }
        }
        (digits, unit)
    };
    let n: u64 = num.parse().unwrap_or(5);
    let secs = match unit {
        's' => n,
        'h' => n.saturating_mul(3600),
        _ => n.saturating_mul(60), // 'm' default
    };
    Duration::from_secs(secs.max(1))
}

/// First numeric token in `s`, parsed as f64 (for `output_gt`/`output_lt`).
fn first_number(s: &str) -> Option<f64> {
    let mut cur = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() || c == '.' || (c == '-' && cur.is_empty()) {
            cur.push(c);
        } else if !cur.is_empty() {
            if let Ok(n) = cur.parse::<f64>() {
                return Some(n);
            }
            cur.clear();
        }
    }
    cur.parse::<f64>().ok()
}

/// Decide whether a watch fires given the check's exit status and stdout.
/// Precedence: contains > output_gt > output_lt > (default) non-zero exit.
fn fired(w: &WatchConfig, exit_ok: bool, stdout: &str) -> bool {
    if let Some(sub) = &w.contains {
        return stdout.contains(sub.as_str());
    }
    if let Some(gt) = w.output_gt {
        return first_number(stdout).is_some_and(|n| n > gt);
    }
    if let Some(lt) = w.output_lt {
        return first_number(stdout).is_some_and(|n| n < lt);
    }
    !exit_ok
}

/// Run a watch's check command, returning (exit_ok, stdout). A spawn error or
/// timeout counts as a non-ok exit (so default-trigger watches fire).
async fn run_check(w: &WatchConfig) -> (bool, String) {
    let fut = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(&w.check)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();
    match tokio::time::timeout(Duration::from_secs(w.timeout_secs), fut).await {
        Ok(Ok(o)) => {
            let mut out = String::from_utf8_lossy(&o.stdout).to_string();
            if out.trim().is_empty() {
                out = String::from_utf8_lossy(&o.stderr).to_string();
            }
            (o.status.success(), out)
        }
        Ok(Err(e)) => (false, format!("spawn error: {e}")),
        Err(_) => (false, "check timed out".to_string()),
    }
}

/// Build the alert text from the watch's template (or a default).
fn alert_text(w: &WatchConfig, stdout: &str) -> String {
    let out_short: String = stdout.trim().chars().take(300).collect();
    let tmpl = w
        .message
        .clone()
        .unwrap_or_else(|| format!("[{}] check triggered: {{output}}", w.name));
    tmpl.replace("{name}", &w.name).replace("{output}", &out_short)
}

/// Push + log an alert. Pushes to Feishu only when configured and `notify_to`
/// is set; always logs (tracing + `~/.aegis/logs/alerts.log`).
async fn fire_alert(w: &WatchConfig, stdout: &str, feishu: &GatewayFeishuConfig, alerts_max_bytes: u64) {
    let text = alert_text(w, stdout);
    warn!(watch = %w.name, "ALERT: {text}");

    // Append to the alerts log (best-effort).
    let log_dir = aegis_core::config::config_dir().join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let alerts_path = log_dir.join("alerts.log");
    // Size-bound: rotate at the configured cap, keep one backup (alerts.log.1).
    if alerts_max_bytes > 0 {
        if let Ok(meta) = std::fs::metadata(&alerts_path) {
            if meta.len() > alerts_max_bytes {
                let _ = std::fs::rename(&alerts_path, log_dir.join("alerts.log.1"));
            }
        }
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&alerts_path)
    {
        let _ = writeln!(f, "{} [{}] {text}", chrono::Utc::now().to_rfc3339(), w.name);
    }

    // Push to Feishu if configured.
    if let Some(to) = w.notify_to.as_deref().filter(|s| !s.is_empty()) {
        if feishu.enabled && !feishu.app_id.is_empty() {
            let ch = FeishuChannel::new(&feishu.app_id, &feishu.app_secret, to);
            if let Err(e) = ch.send(OutboundMessage::new(to, format!("⚠️ {text}"))).await {
                warn!(watch = %w.name, "alert push failed: {e}");
            }
        }
    }
}

/// Built-in host self-guardian seeds: memory/disk/load monitors so the resident
/// daemon watches its own box out of the box (P3). Pure shell (no LLM). Empty if
/// disabled. On non-Linux (no `/proc`) the numeric checks simply never fire.
pub fn default_host_watches(cfg: &SelfWatchConfig) -> Vec<WatchConfig> {
    if !cfg.enabled {
        return Vec::new();
    }
    let cpus = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    let load_threshold = (cpus * 2.0).max(2.0);
    vec![
        WatchConfig {
            name: "self:disk".into(),
            schedule: "every 5m".into(),
            check: "df -P / | awk 'NR==2{gsub(\"%\",\"\",$5); print $5}'".into(),
            contains: None,
            output_gt: Some(cfg.disk_pct),
            output_lt: None,
            notify_to: cfg.notify_to.clone(),
            message: Some("host disk usage high: {output}% used on /".into()),
            cooldown_secs: 3600,
            timeout_secs: 15,
            enabled: true,
        },
        WatchConfig {
            name: "self:memory".into(),
            schedule: "every 2m".into(),
            check: "awk '/MemAvailable/{a=$2}/MemTotal/{t=$2}END{if(t>0)printf \"%.0f\", a/t*100}' /proc/meminfo".into(),
            contains: None,
            output_gt: None,
            output_lt: Some(cfg.mem_pct),
            notify_to: cfg.notify_to.clone(),
            message: Some("host memory low: only {output}% available".into()),
            cooldown_secs: 3600,
            timeout_secs: 15,
            enabled: true,
        },
        WatchConfig {
            name: "self:load".into(),
            schedule: "every 2m".into(),
            check: "awk '{print $1}' /proc/loadavg".into(),
            contains: None,
            output_gt: Some(load_threshold),
            output_lt: None,
            notify_to: cfg.notify_to.clone(),
            message: Some(format!(
                "host load high: 1-min load {{output}} (> {load_threshold} on {} cpu)",
                cpus as u64
            )),
            cooldown_secs: 3600,
            timeout_secs: 15,
            enabled: true,
        },
    ]
}

/// Daemon entry: loop over enabled watches forever, running each on its
/// interval and alerting on its trigger (throttled by cooldown).
pub async fn run_watchers(config: Config) {
    let mut watches: Vec<WatchConfig> = default_host_watches(&config.self_watch);
    watches.extend(config.watch.iter().filter(|w| w.enabled).cloned());
    if watches.is_empty() {
        return;
    }
    let intervals: HashMap<String, Duration> = watches
        .iter()
        .map(|w| (w.name.clone(), parse_schedule(&w.schedule)))
        .collect();
    let mut last_run: HashMap<String, Instant> = HashMap::new();
    let mut last_alert: HashMap<String, Instant> = HashMap::new();
    let alerts_max_bytes = config.logs.alerts_max_mb.saturating_mul(1024 * 1024);
    info!("watcher: monitoring {} check(s)", watches.len());

    loop {
        let now = Instant::now();
        for w in &watches {
            let interval = intervals.get(&w.name).copied().unwrap_or(Duration::from_secs(300));
            let due = last_run
                .get(&w.name)
                .map_or(true, |t| now.duration_since(*t) >= interval);
            if !due {
                continue;
            }
            last_run.insert(w.name.clone(), now);
            let (exit_ok, stdout) = run_check(w).await;
            if fired(w, exit_ok, &stdout) {
                let cooled = last_alert
                    .get(&w.name)
                    .map_or(true, |t| now.duration_since(*t) >= Duration::from_secs(w.cooldown_secs));
                if cooled {
                    fire_alert(w, &stdout, &config.gateway.feishu, alerts_max_bytes).await;
                    last_alert.insert(w.name.clone(), now);
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// `aegis watch list` — print configured monitors.
pub fn run_watch_list(config: &Config) {
    let mut all = default_host_watches(&config.self_watch);
    all.extend(config.watch.iter().cloned());
    if all.is_empty() {
        println!("No monitors configured. Add [[watch]] blocks to config.toml (see config.example.toml), or enable [self_watch].");
        return;
    }
    println!("Monitors ({}):", all.len());
    for w in &all {
        let cond = if let Some(c) = &w.contains {
            format!("contains '{c}'")
        } else if let Some(gt) = w.output_gt {
            format!("output > {gt}")
        } else if let Some(lt) = w.output_lt {
            format!("output < {lt}")
        } else {
            "non-zero exit".to_string()
        };
        let notify = w.notify_to.as_deref().filter(|s| !s.is_empty()).unwrap_or("(log only)");
        let state = if w.enabled { "" } else { " [disabled]" };
        println!(
            "  {}{}  schedule={}  when {}  notify={}",
            w.name, state, w.schedule, cond, notify
        );
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_host_watches_enabled() {
        let cfg = SelfWatchConfig::default();
        let seeds = default_host_watches(&cfg);
        assert_eq!(seeds.len(), 3);
        let names: Vec<&str> = seeds.iter().map(|w| w.name.as_str()).collect();
        assert!(names.contains(&"self:disk"));
        assert!(names.contains(&"self:memory"));
        assert!(names.contains(&"self:load"));
        // disk fires on high usage (output_gt), memory on low (output_lt).
        let disk = seeds.iter().find(|w| w.name == "self:disk").unwrap();
        assert!(disk.output_gt.is_some() && disk.output_lt.is_none());
        let mem = seeds.iter().find(|w| w.name == "self:memory").unwrap();
        assert!(mem.output_lt.is_some() && mem.output_gt.is_none());
    }

    #[test]
    fn test_default_host_watches_disabled() {
        let cfg = SelfWatchConfig { enabled: false, ..Default::default() };
        assert!(default_host_watches(&cfg).is_empty());
    }
}
