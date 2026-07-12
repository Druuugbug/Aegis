//! Canonical filesystem path resolution for the Aegis ecosystem.
//!
//! This is the **single source of truth** for where Aegis stores its data.
//! It lives in `aegis-types` (the lowest shared crate) so every crate can call
//! it, instead of hardcoding `~/.aegis` independently. Previously ~21 sites
//! across 8 crates hardcoded `dirs_next::home_dir().join(".aegis")` because
//! they could not depend on the `config_dir()` that lived in the higher-level
//! `aegis-core::config`; that scattering caused the config root to drift/split
//! (see docs/aegis-config-root-unify-design.md).
//!
//! `aegis-core::config::config_dir()` now delegates here, keeping its public
//! API stable.

use std::path::PathBuf;

/// Resolve the Aegis config/data root directory.
///
/// Order (unchanged, for backward compatibility):
/// 1. `$AEGIS_HOME` if set.
/// 2. `~/.aegis` if it already exists (legacy installs keep working).
/// 3. Platform-native config dir + `aegis` (Linux `~/.config/aegis`, macOS
///    `~/Library/Application Support/aegis`, Windows `%APPDATA%/aegis`).
/// 4. Fallback `~/.aegis`.
///
/// Because every module now routes through this one function, a fresh install
/// never spuriously creates `~/.aegis`, so the root no longer flips mid-run.
pub fn config_dir() -> PathBuf {
    if let Ok(home) = std::env::var("AEGIS_HOME") {
        return PathBuf::from(home);
    }

    let legacy = dirs_next::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".aegis");
    if legacy.is_dir() {
        return legacy;
    }

    if let Some(base) = dirs_next::config_dir() {
        base.join("aegis")
    } else {
        legacy
    }
}

/// The default config file path (`<config_dir>/config.toml`).
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// `<config_dir>/<sub>` convenience join.
pub fn config_subdir(sub: &str) -> PathBuf {
    config_dir().join(sub)
}

/// The legacy `~/.aegis` root (may or may not exist / be in use).
pub fn legacy_root() -> Option<PathBuf> {
    dirs_next::home_dir().map(|h| h.join(".aegis"))
}

/// The platform-native `~/.config/aegis` (Linux) root.
pub fn platform_root() -> Option<PathBuf> {
    dirs_next::config_dir().map(|b| b.join("aegis"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn respects_aegis_home_env() {
        // Use a unique value to avoid clobbering a real env in parallel tests.
        let tmp = std::env::temp_dir().join("aegis-paths-test-home");
        std::env::set_var("AEGIS_HOME", &tmp);
        assert_eq!(config_dir(), tmp);
        assert_eq!(config_path(), tmp.join("config.toml"));
        std::env::remove_var("AEGIS_HOME");
    }

    #[test]
    fn config_path_is_under_config_dir() {
        // Without AEGIS_HOME the exact root is environment-dependent, but
        // config_path must always be config_dir()/config.toml.
        std::env::remove_var("AEGIS_HOME");
        assert_eq!(config_path(), config_dir().join("config.toml"));
    }
}
