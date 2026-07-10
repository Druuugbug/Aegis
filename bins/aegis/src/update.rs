//! Once-a-day update-check notice: if the configured GitHub
//! repo has a newer release than the running binary, return a one-line notice.
//! Network is hit at most once per 24h (cached); failures are silent.

use std::path::PathBuf;

use aegis_core::config::Config;

fn cache_path() -> PathBuf {
    aegis_core::config::config_dir().join("update_check.json")
}

/// Compare dotted versions numerically: is `latest` newer than `current`?
fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| -> Vec<u64> {
        s.trim_start_matches('v')
            .split(|c: char| c == '.' || c == '-' || c == '+')
            .map(|p| p.chars().take_while(|c| c.is_ascii_digit()).collect::<String>())
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(latest), parse(current));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    false
}

async fn fetch_latest_tag(repo: &str) -> Option<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header("User-Agent", "aegis-update-check")
        .header("Accept", "application/vnd.github+json")
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("tag_name").and_then(|x| x.as_str()).map(|s| s.to_string())
}

/// Returns an update notice line if a newer release exists, else `None`.
/// Cheap on most runs (uses a 24h cache); only hits the network once a day.
pub async fn check_update_notice() -> Option<String> {
    let cfg = Config::load(&aegis_core::config::config_path()).unwrap_or_default();
    if !cfg.update.check {
        return None;
    }
    let repo = cfg.update.repo.trim().to_string();
    if repo.is_empty() {
        return None;
    }

    let now = chrono::Utc::now();
    let cached: Option<(chrono::DateTime<chrono::Utc>, String)> =
        std::fs::read_to_string(cache_path())
            .ok()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).ok())
            .and_then(|v| {
                let t = v.get("checked_at").and_then(|x| x.as_str())?;
                let latest = v.get("latest").and_then(|x| x.as_str())?.to_string();
                let t = chrono::DateTime::parse_from_rfc3339(t).ok()?.with_timezone(&chrono::Utc);
                Some((t, latest))
            });

    let latest = match &cached {
        // Fresh cache (<24h): no network.
        Some((t, latest)) if now - *t < chrono::Duration::hours(24) => latest.clone(),
        _ => {
            let latest = fetch_latest_tag(&repo).await?;
            let rec = serde_json::json!({ "checked_at": now.to_rfc3339(), "latest": latest });
            let _ = std::fs::write(cache_path(), rec.to_string());
            latest
        }
    };

    let current = env!("CARGO_PKG_VERSION");
    if is_newer(&latest, current) {
        use colored::Colorize;
        Some(format!(
            "  {} aegis {} → {} available — update: {}",
            "✦".bright_yellow(),
            format!("v{current}").dimmed(),
            latest.bright_white(),
            "`aegis update` (or git pull && cargo build --release)".dimmed()
        ))
    } else {
        None
    }
}
