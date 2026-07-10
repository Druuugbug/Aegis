//! Baked-in default catalog of common server components, organised by category
//! and preference tier (`minimal` | `standard` | `advanced`).
//!
//! This is a *default starting point* for the server-admin scenario. It is
//! intentionally compact (small system-prompt footprint) and is meant to be
//! overridden by:
//!   1. explicit user requests, and
//!   2. persistent memory updated from live `web_search` (servers have network),
//! which both take precedence over these defaults.

/// Recommendation rows: (category, minimal, standard, advanced).
const ROWS: &[(&str, &str, &str, &str)] = &[
    ("Reverse proxy / web", "Caddy", "Nginx", "Caddy or Traefik (auto-TLS)"),
    ("TLS certificates", "Caddy auto-HTTPS", "certbot (Let's Encrypt)", "Caddy/Traefik ACME"),
    ("Container runtime", "none / static binaries", "Docker", "Podman or containerd + nerdctl"),
    ("Database", "SQLite", "PostgreSQL", "PostgreSQL + extensions"),
    ("Cache / KV", "none", "Redis", "Redis / Dragonfly"),
    ("Process manager", "systemd unit", "systemd", "systemd + watchdog"),
    ("Firewall", "ufw", "nftables", "nftables (scripted)"),
    ("Monitoring", "htop / vmstat", "netdata", "Prometheus + Grafana"),
    ("Logs", "journalctl", "journald + logrotate", "Loki / Vector"),
    ("Proxy / VPN", "single xray", "x-ui (3X-UI)", "sing-box or xray + REALITY"),
    ("Backup", "tar + cron", "restic", "restic + offsite (S3)"),
    ("Shell / ops", "bash + coreutils", "tmux + fzf", "zsh + modern CLI suite"),
];

/// Normalise a tier string to one of `minimal|standard|advanced`.
fn norm_tier(tier: &str) -> &'static str {
    match tier.trim().to_lowercase().as_str() {
        "minimal" | "min" | "lite" => "minimal",
        "advanced" | "max" | "cutting-edge" => "advanced",
        _ => "standard",
    }
}

/// Render a compact, system-prompt-friendly catalog for the given tier, with a
/// note that live web search + memory should override these defaults.
pub fn catalog(tier: &str) -> String {
    let t = norm_tier(tier);
    let mut out = String::with_capacity(1024);
    out.push_str(&format!(
        "# Server components (preference tier: {t})\n\
         Default picks for common server stacks at this tier. These are only a \
         starting point — prefer the user's explicit choice, then anything you \
         have remembered from a recent `web_search` (servers have network; when \
         a choice matters or may be outdated, search for the current best option \
         and `remember` it), then these defaults.\n\n"
    ));
    out.push_str("| Category | Recommended |\n|---|---|\n");
    for (cat, min, std, adv) in ROWS {
        let pick = match t {
            "minimal" => min,
            "advanced" => adv,
            _ => std,
        };
        out.push_str(&format!("| {cat} | {pick} |\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_varies_by_tier() {
        let min = catalog("minimal");
        let adv = catalog("advanced");
        assert!(min.contains("preference tier: minimal"));
        assert!(adv.contains("preference tier: advanced"));
        assert!(min.contains("Caddy")); // minimal reverse proxy
        assert!(adv.contains("Podman") || adv.contains("containerd"));
    }

    #[test]
    fn unknown_tier_is_standard() {
        let s = catalog("weird");
        assert!(s.contains("preference tier: standard"));
        assert!(s.contains("Nginx"));
    }
}
