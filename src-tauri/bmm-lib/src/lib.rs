//! Balatro Mod Manager Library
//!
//! This crate provides the core business logic for the Balatro Mod Manager,
//! including mod installation, detection, caching, and platform-specific handling.
//!
//! # Modules
//!
//! - [`backup`]: Backup and restore functionality for mod snapshots
//! - [`balamod`]: Balamod (alternative mod loader) support and Balatro game detection
//! - [`cache`]: Binary cache for remote mod index data
//! - [`database`]: SQLite database for storing app settings and installed mod metadata
//! - [`discord_rpc`]: Discord Rich Presence integration
//! - [`errors`]: Application-wide error types
//! - [`finder`]: Automatic Balatro installation path detection
//! - [`installer`]: Mod installation and uninstallation logic
//! - [`local_mod_detection`]: Scanning Mods folder and detecting untracked mods
//! - [`logging`]: Centralized logging configuration
//! - [`lovely`]: Lovely (mod loader/injector) installer and version management
//! - [`mod_collections`]: Curated mod collection support
//! - [`rate_limiter`]: Rate limiter for GitHub API requests
//! - [`smods_installer`]: Steamodded/Talisman installer

/// Backup and restore functionality for mod snapshots.
pub mod backup;
/// Balamod support and Balatro game detection.
pub mod balamod;
/// Binary cache for remote mod index data.
pub mod cache;
/// SQLite database for app settings and installed mod metadata.
pub mod database;
/// Discord Rich Presence integration.
pub mod discord_rpc;
/// Application-wide error types.
pub mod errors;
/// Automatic Balatro installation path detection.
pub mod finder;
/// Shared HTTP client singletons.
pub mod http;
/// Mod installation and uninstallation logic.
pub mod installer;
/// Scanning Mods folder and detecting untracked mods.
pub mod local_mod_detection;
/// Centralized logging configuration.
pub mod logging;
/// Lovely (mod loader/injector) installer and version management.
pub mod lovely;
/// Curated mod collection support.
pub mod mod_collections;
/// Rate limiter for GitHub API requests.
pub mod rate_limiter;
/// Steamodded/Talisman installer.
pub mod smods_installer;

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Mutex;

#[cfg(test)]
pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

/// The per-user configuration directory the app keeps everything under (`<root>/Balatro/...`).
///
/// `$BMM_CONFIG_DIR` wins when set and non-empty; otherwise the platform default from `dirs`.
/// The override exists because `dirs` ignores `HOME`/`XDG_CONFIG_HOME` on Windows, so tests and
/// portable setups could not otherwise redirect where the app reads and writes. This is the only
/// place in the workspace allowed to call `dirs::config_dir()`.
pub fn config_root() -> Option<PathBuf> {
    match std::env::var_os("BMM_CONFIG_DIR") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => dirs::config_dir(),
    }
}

/// Resolve the mods directory on the host (config dir).
pub fn mods_dir() -> PathBuf {
    crate::local_mod_detection::resolve_mods_dir_path().unwrap_or_else(|_| {
        // Fallback to the platform config dir if detection failed
        config_root()
            .unwrap_or_else(|| Path::new(".").to_path_buf())
            .join("Balatro")
            .join("Mods")
    })
}
