//! Self-update engine: download the matching prebuilt release and atomically
//! replace the running executable, so the next launch is the new version.
//!
//! Two entry points share this module:
//! - `aegis update` (CLI): replace the binary; next launch is new.
//! - `/update` (in-session REPL): replace + hot-swap (re-exec) or prompt restart.
//!
//! Prebuilt binaries are Linux-only (musl static, `aegis-linux-<arch>.tar.gz`);
//! on other OSes we point the user at `cargo install aegis-agent`. Extraction
//! shells out to the system `tar` (no extra crate); download uses `reqwest`.

use std::path::{Path, PathBuf};

use aegis_core::config::Config;
use anyhow::{anyhow, Context, Result};

/// Outcome of an update attempt.
#[derive(Debug)]
pub enum UpdateOutcome {
    /// Already on the newest release.
    UpToDate { current: String },
    /// Binary replaced; the new version takes effect next launch (or via
    /// hot-swap). `exe` is the replaced executable path.
    Updated { from: String, to: String, exe: PathBuf },
    /// No prebuilt asset for this OS/arch — user must use cargo.
    NeedsCargo { hint: String },
}

/// Options controlling an update run.
#[derive(Default)]
pub struct UpdateOptions {
    /// Update even when not strictly newer (reinstall the latest).
    pub force: bool,
    /// Override `owner/repo` (otherwise resolved from config/env).
    pub repo: Option<String>,
}

/// Resolve the GitHub `owner/repo` to update from: explicit override →
/// `[update].repo` config → parsed from the compiled-in `CARGO_PKG_REPOSITORY`.
///
/// The env fallback means a private build targets its internal repo while a
/// public release (whose Cargo `repository` was rewritten on export) targets
/// the public repo — zero config either way.
pub(crate) fn resolve_repo(cfg: &Config, override_repo: Option<&str>) -> Option<String> {
    if let Some(r) = override_repo {
        let r = r.trim();
        if !r.is_empty() {
            return Some(r.to_string());
        }
    }
    let from_cfg = cfg.update.repo.trim();
    if !from_cfg.is_empty() {
        return Some(from_cfg.to_string());
    }
    repo_from_url(env!("CARGO_PKG_REPOSITORY"))
}

/// Extract `owner/repo` from a GitHub URL like
/// `https://github.com/Druuugbug/Aegis` (trailing `.git` / `/` tolerated).
pub(crate) fn repo_from_url(url: &str) -> Option<String> {
    let s = url.trim().trim_end_matches('/');
    let rest = s.strip_prefix("https://github.com/")
        .or_else(|| s.strip_prefix("http://github.com/"))
        .or_else(|| s.strip_prefix("git@github.com:"))?;
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next().filter(|p| !p.is_empty())?;
    let repo = parts.next().filter(|p| !p.is_empty())?;
    Some(format!("{owner}/{repo}"))
}

/// Prebuilt asset name for the current OS/arch, or `None` when no prebuilt is
/// published for this target (caller then points the user at cargo).
pub(crate) fn target_asset() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let arch = std::env::consts::ARCH; // "x86_64" | "aarch64" | ...
    match arch {
        "x86_64" | "aarch64" => Some(format!("aegis-linux-{arch}.tar.gz")),
        _ => None,
    }
}

const CARGO_HINT: &str =
    "No prebuilt binary for this OS/arch. Update with `cargo install aegis-agent` \
     (or `cargo binstall aegis-agent`).";

/// Run an update: check latest, download the matching asset, and atomically
/// replace the current executable. Pure decisions are factored into helpers so
/// they can be unit-tested; this function performs the network + filesystem IO.
pub async fn perform_update(opts: &UpdateOptions) -> Result<UpdateOutcome> {
    let cfg = Config::load(&aegis_core::config::config_path()).unwrap_or_default();
    let current = env!("CARGO_PKG_VERSION").to_string();

    let repo = resolve_repo(&cfg, opts.repo.as_deref())
        .ok_or_else(|| anyhow!("cannot determine update repo (set [update].repo)"))?;

    let latest = crate::update::fetch_latest_tag(&repo)
        .await
        .ok_or_else(|| anyhow!("could not fetch latest release from {repo}"))?;

    if !opts.force && !crate::update::is_newer(&latest, &current) {
        return Ok(UpdateOutcome::UpToDate { current });
    }

    let asset = match target_asset() {
        Some(a) => a,
        None => return Ok(UpdateOutcome::NeedsCargo { hint: CARGO_HINT.to_string() }),
    };

    let url = format!("https://github.com/{repo}/releases/download/{latest}/{asset}");
    let staged = download_and_extract(&url, &asset).await?;

    // Verify-before-commit: never replace the working binary with one that
    // can't even start (wrong arch / corrupt download / missing libc).
    verify_binary(&staged)?;

    let exe = std::env::current_exe().context("locating current executable")?;
    replace_executable(&staged, &exe)?;

    Ok(UpdateOutcome::Updated { from: current, to: latest, exe })
}

/// Sanity-check a freshly downloaded binary before trusting it: it must run and
/// exit 0 on `--version`. Guards against replacing a working binary with a
/// broken one (wrong arch, corrupt download, missing shared libs).
fn verify_binary(bin: &Path) -> Result<()> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("new binary {} failed to execute", bin.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "new binary failed its `--version` self-check (exit {:?}); keeping the current binary",
            out.status.code()
        ));
    }
    Ok(())
}

/// Download `url` to the cache dir and untar it (system `tar`), returning the
/// path to the extracted `aegis` binary.
async fn download_and_extract(url: &str, asset: &str) -> Result<PathBuf> {
    let cache = aegis_core::config::config_dir().join("cache");
    std::fs::create_dir_all(&cache).ok();
    let tarball = cache.join(asset);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .user_agent("aegis-self-update")
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("download failed ({}) from {url}", resp.status()));
    }
    let bytes = resp.bytes().await?;
    // Guard against absurd sizes on tiny hosts (256 MiB cap).
    if bytes.len() as u64 > 256 * 1024 * 1024 {
        return Err(anyhow!("release asset unexpectedly large ({} bytes)", bytes.len()));
    }
    std::fs::write(&tarball, &bytes).with_context(|| format!("writing {}", tarball.display()))?;

    let extract_dir = cache.join("extract");
    let _ = std::fs::remove_dir_all(&extract_dir);
    std::fs::create_dir_all(&extract_dir)?;
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&extract_dir)
        .status()
        .context("running system `tar` to extract the release")?;
    if !status.success() {
        return Err(anyhow!("`tar` failed to extract {}", tarball.display()));
    }

    let bin = find_binary(&extract_dir, "aegis")
        .ok_or_else(|| anyhow!("extracted archive did not contain an `aegis` binary"))?;
    Ok(bin)
}

/// Find a file named `name` anywhere under `dir` (shallow: root then one level).
fn find_binary(dir: &Path, name: &str) -> Option<PathBuf> {
    let direct = dir.join(name);
    if direct.is_file() {
        return Some(direct);
    }
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let candidate = p.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        } else if p.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(p);
        }
    }
    None
}

/// Atomically replace `exe` with `new_bin`. Writes to a sibling temp file (same
/// filesystem → atomic `rename`), sets 0755, then renames over the target.
/// Replacing a running executable is legal on Linux (the old inode stays open).
fn replace_executable(new_bin: &Path, exe: &Path) -> Result<()> {
    let dir = exe.parent().ok_or_else(|| anyhow!("executable has no parent dir"))?;
    // Back up the current binary first so a bad update can be rolled back with
    // `aegis update --rollback` (best-effort; failure here is non-fatal).
    let _ = std::fs::copy(exe, dir.join(".aegis-prev"));
    let tmp = dir.join(".aegis-update.tmp");
    // Copy into place (cross-device safe) then fix perms.
    std::fs::copy(new_bin, &tmp)
        .with_context(|| format!("staging new binary at {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("chmod 0755 on staged binary")?;
    }
    std::fs::rename(&tmp, exe).with_context(|| {
        format!("replacing {} (is it writable? may need sudo)", exe.display())
    })?;
    Ok(())
}

/// Roll back to the binary saved just before the last update (`.aegis-prev`
/// beside the executable). Returns the restored executable path.
pub fn restore_previous() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("locating current executable")?;
    let dir = exe.parent().ok_or_else(|| anyhow!("executable has no parent dir"))?;
    let backup = dir.join(".aegis-prev");
    if !backup.is_file() {
        return Err(anyhow!(
            "no previous binary to roll back to ({})",
            backup.display()
        ));
    }
    let tmp = dir.join(".aegis-rollback.tmp");
    std::fs::copy(&backup, &tmp).with_context(|| "staging the previous binary".to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
            .context("chmod 0755 on staged binary")?;
    }
    std::fs::rename(&tmp, &exe)
        .with_context(|| format!("restoring {} (writable?)", exe.display()))?;
    Ok(exe)
}

/// CLI handler for `aegis update`. Replaces the on-disk binary; the new version
/// takes effect on the next launch (this one-shot command does not re-exec).
pub async fn run_update(force: bool, repo: Option<String>, rollback: bool) -> Result<()> {
    use colored::Colorize;
    if rollback {
        let exe = restore_previous()?;
        println!(
            "{} rolled back to the previous binary.\n  {} restart aegis to run it.\n  binary: {}",
            "✓".green(),
            "↻".cyan(),
            exe.display().to_string().dimmed(),
        );
        return Ok(());
    }
    let opts = UpdateOptions { force, repo };
    println!("{} checking for updates…", "⟳".cyan());
    match perform_update(&opts).await? {
        UpdateOutcome::UpToDate { current } => {
            println!("{} already up to date (v{current}).", "✓".green());
        }
        UpdateOutcome::Updated { from, to, exe } => {
            println!(
                "{} updated aegis {} → {}.\n  {} restart aegis (or your `aegis gateway`) to run the new version.\n  binary: {}",
                "✓".green(),
                format!("v{from}").dimmed(),
                to.bright_white(),
                "↻".cyan(),
                exe.display().to_string().dimmed(),
            );
        }
        UpdateOutcome::NeedsCargo { hint } => {
            println!("{} {hint}", "ℹ".yellow());
        }
    }
    Ok(())
}

/// In-session hot-swap: persist swap-state, then re-exec into the freshly
/// replaced binary so the session resumes seamlessly (chat.rs startup recovery
/// picks up the swap-state). Returns `false` when hot-swap is not possible
/// (non-interactive / non-unix / exec failed) — the caller then prompts the
/// user to restart. On success this never returns (the process image is
/// replaced).
#[cfg(unix)]
pub fn hot_swap(session_id: &str, prev_version: &str) -> bool {
    use std::io::IsTerminal;
    use std::os::unix::process::CommandExt;
    if !std::io::stdin().is_terminal() {
        return false;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return false,
    };
    let state = aegis_core::swap_state::SwapState::new(
        session_id,
        aegis_core::swap_state::SwapReason::HotUpgrade,
    )
    .with_new_binary(exe.display().to_string())
    .with_previous_version(prev_version);
    if aegis_core::swap_state::save(&state).is_err() {
        return false;
    }
    // exec() only returns on failure.
    let err = std::process::Command::new(&exe)
        .args(std::env::args_os().skip(1))
        .exec();
    tracing::warn!("hot-swap exec failed: {err}");
    aegis_core::swap_state::clear();
    false
}

#[cfg(not(unix))]
pub fn hot_swap(_session_id: &str, _prev_version: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_repo_from_url_variants() {
        assert_eq!(repo_from_url("https://github.com/Druuugbug/Aegis").as_deref(), Some("Druuugbug/Aegis"));
        assert_eq!(repo_from_url("https://github.com/Druuugbug/Aegis/").as_deref(), Some("Druuugbug/Aegis"));
        assert_eq!(repo_from_url("https://github.com/foo/bar.git").as_deref(), Some("foo/bar"));
        assert_eq!(repo_from_url("git@github.com:foo/bar.git").as_deref(), Some("foo/bar"));
        assert_eq!(repo_from_url("https://example.com/foo/bar"), None);
        assert_eq!(repo_from_url(""), None);
    }

    #[test]
    fn test_resolve_repo_precedence() {
        let mut cfg = Config::default();
        // env fallback (config empty) → parsed CARGO_PKG_REPOSITORY.
        assert_eq!(
            resolve_repo(&cfg, None),
            repo_from_url(env!("CARGO_PKG_REPOSITORY"))
        );
        // config beats env.
        cfg.update.repo = "acme/aegis".into();
        assert_eq!(resolve_repo(&cfg, None).as_deref(), Some("acme/aegis"));
        // explicit override beats config.
        assert_eq!(resolve_repo(&cfg, Some("x/y")).as_deref(), Some("x/y"));
        // blank override is ignored.
        assert_eq!(resolve_repo(&cfg, Some("  ")).as_deref(), Some("acme/aegis"));
    }

    #[test]
    fn test_target_asset_shape() {
        match target_asset() {
            Some(a) => {
                assert!(a.starts_with("aegis-linux-"));
                assert!(a.ends_with(".tar.gz"));
            }
            None => {} // non-linux / unknown arch — acceptable
        }
    }
}
