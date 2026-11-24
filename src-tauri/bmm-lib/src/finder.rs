//! Automatic Balatro installation path detection.
//!
//! This module provides functions to locate Balatro game installations across
//! different platforms and store types:
//! - Steam installations (via library folders)
//! - Registry entries (Windows)
//! - Common installation directories
//! - Flatpak and Proton prefixes (Linux)
//!
//! # Detection Strategy
//!
//! 1. Check user-configured path in database
//! 2. Query Steam library folders via `libraryfolders.vdf`
//! 3. Check common installation directories
//! 4. (Windows) Query Windows Registry for Steam path

use crate::database::Database;
use log::{debug, error};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};
use std::time::{Duration, Instant};
#[cfg(target_os = "windows")]
use sysinfo::System;
#[cfg(target_os = "windows")]
use winreg::RegKey;
#[cfg(target_os = "windows")]
use winreg::enums::*;

/// TTL for the Balatro paths cache (5 minutes).
const BALATRO_PATHS_CACHE_TTL: Duration = Duration::from_secs(300);

/// Cached Balatro installation paths with timestamp.
struct CachedPaths {
    paths: Vec<PathBuf>,
    cached_at: Instant,
}

/// Global cache for Balatro paths to avoid repeated filesystem scans.
static BALATRO_PATHS_CACHE: Lazy<RwLock<Option<CachedPaths>>> = Lazy::new(|| RwLock::new(None));

/// Returns cached Balatro paths if available and not expired, otherwise performs a fresh scan.
///
/// This function wraps `get_balatro_paths()` with a time-based cache to avoid
/// repeated filesystem scanning on every call. The cache is valid for 60 seconds.
pub fn get_balatro_paths_cached() -> Vec<PathBuf> {
    // Try to read from cache first
    if let Ok(guard) = BALATRO_PATHS_CACHE.read()
        && let Some(cached) = guard.as_ref()
        && cached.cached_at.elapsed() < BALATRO_PATHS_CACHE_TTL
    {
        debug!("Returning {} cached Balatro paths", cached.paths.len());
        return cached.paths.clone();
    }

    // Cache miss or expired, acquire write lock and refresh
    if let Ok(mut guard) = BALATRO_PATHS_CACHE.write() {
        // Double-check after acquiring write lock
        if let Some(cached) = guard.as_ref()
            && cached.cached_at.elapsed() < BALATRO_PATHS_CACHE_TTL
        {
            return cached.paths.clone();
        }

        // Perform fresh scan
        debug!("Refreshing Balatro paths cache");
        let paths = get_balatro_paths();
        *guard = Some(CachedPaths {
            paths: paths.clone(),
            cached_at: Instant::now(),
        });
        return paths;
    }

    // Fallback if we can't get the lock
    get_balatro_paths()
}

/// Invalidates the Balatro paths cache, forcing a fresh scan on the next call.
///
/// Call this when the user changes their installation path or when you need
/// to ensure fresh results.
pub fn invalidate_balatro_paths_cache() {
    if let Ok(mut guard) = BALATRO_PATHS_CACHE.write() {
        *guard = None;
        debug!("Balatro paths cache invalidated");
    }
}

#[cfg(target_os = "windows")]
fn read_path_from_registry() -> Result<String, std::io::Error> {
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);
    let steam_path = hklm.open_subkey("SOFTWARE\\WOW6432Node\\Valve\\Steam")?;

    Ok(steam_path.get_value("InstallPath")?)
}

/// Steam's install path from the Windows registry; `None` elsewhere or when unreadable.
fn registry_steam_install() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        read_path_from_registry().ok()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// The operating systems with their own Steam/Balatro layouts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::MacOs
        } else {
            Os::Linux
        }
    }
}

/// Everything about the machine that path detection reads, gathered in one place so the layout
/// logic in [`balatro_paths_in`] is a pure function that runs the same on every host.
#[derive(Clone, Debug)]
pub struct PathEnv {
    pub os: Os,
    /// The user's home directory.
    pub home: Option<PathBuf>,
    /// A Balatro folder the user configured in the app's database.
    pub custom_path: Option<PathBuf>,
    /// Windows: Steam's `InstallPath` from the registry (`None` => the default location).
    pub steam_install: Option<String>,
    /// Linux: whether `FLATPAK_ID` is set (we run inside a Flatpak sandbox).
    pub flatpak: bool,
    /// Linux: `$USER`.
    pub user: Option<String>,
    /// Linux: `$XDG_DATA_HOME`.
    pub xdg_data_home: Option<PathBuf>,
}

impl PathEnv {
    /// The real machine this process runs on.
    pub fn current() -> PathEnv {
        let custom_path = Database::new()
            .ok()
            .and_then(|db| db.get_installation_path().ok().flatten())
            .map(PathBuf::from);
        PathEnv {
            os: Os::current(),
            home: home::home_dir(),
            custom_path,
            steam_install: registry_steam_install(),
            flatpak: std::env::var("FLATPAK_ID").is_ok(),
            user: std::env::var("USER").ok(),
            xdg_data_home: std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        }
    }
}

/// Read-only filesystem access used by path detection; tests substitute an in-memory fake.
pub trait FsProbe {
    fn exists(&self, p: &Path) -> bool;
    fn read_to_string(&self, p: &Path) -> Option<String>;
    /// Child *directories* of `p`, in a stable (sorted) order; empty when unreadable.
    fn list_dir(&self, p: &Path) -> Vec<PathBuf>;
}

/// The real filesystem.
pub struct RealFs;

impl FsProbe for RealFs {
    fn exists(&self, p: &Path) -> bool {
        p.exists()
    }
    fn read_to_string(&self, p: &Path) -> Option<String> {
        std::fs::read_to_string(p).ok()
    }
    fn list_dir(&self, p: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(p)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|path| path.is_dir())
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    }
}

/// Normalised, case-insensitive identity of a path for de-duplication.
fn path_key(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/").to_lowercase();
    s.trim_end_matches('/').to_string()
}

/// Dedupe by [`path_key`], keeping the first occurrence.
fn dedup_by_key(paths: &mut Vec<PathBuf>) {
    let mut seen: HashSet<String> = HashSet::new();
    paths.retain(|p| seen.insert(path_key(p)));
}

/// Pure layout logic: every place Balatro can live on `env.os`, filtered to what exists in `fs`.
/// The user's configured path comes first, then discovered installs in a stable order.
pub fn balatro_paths_in(env: &PathEnv, fs: &dyn FsProbe) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Some(custom) = &env.custom_path
        && fs.exists(custom)
    {
        paths.push(custom.clone());
    }
    match env.os {
        Os::Windows => windows_candidates(env, fs, &mut paths),
        Os::MacOs => macos_candidates(env, &mut paths),
        Os::Linux => linux_candidates(env, fs, &mut paths),
    }
    paths.retain(|p| fs.exists(p));
    dedup_by_key(&mut paths);
    debug!("Found {} Balatro installations: {:?}", paths.len(), paths);
    paths
}

/// Steam library folder paths from a `libraryfolders.vdf` body (`"path"  "<dir>"` lines); VDF
/// escapes backslashes, so `\\` becomes `\`.
fn vdf_library_paths(contents: &str) -> Vec<String> {
    static PATH_LINE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""path"\s*"([^"]+)""#).expect("static regex"));
    PATH_LINE
        .captures_iter(contents)
        .filter_map(|c| c.get(1))
        .map(|m| m.as_str().replace("\\\\", "\\"))
        .collect()
}

fn windows_candidates(env: &PathEnv, fs: &dyn FsProbe, paths: &mut Vec<PathBuf>) {
    let steam = env.steam_install.clone().unwrap_or_else(|| {
        error!(
            "Could not read steam install path from Registry! Trying standard installation path in C:\\"
        );
        String::from("C:\\Program Files (x86)\\Steam")
    });
    // Built as strings with `\` so the result does not depend on the host's path separator.
    let vdf = PathBuf::from(format!(
        "{}\\steamapps\\libraryfolders.vdf",
        steam.trim_end_matches('\\')
    ));
    let Some(contents) = fs.read_to_string(&vdf) else {
        error!("'{}' not found or unreadable.", vdf.to_string_lossy());
        return;
    };
    for lib in vdf_library_paths(&contents) {
        paths.push(PathBuf::from(format!(
            "{}\\steamapps\\common\\Balatro",
            lib.trim_end_matches('\\')
        )));
    }
}

fn macos_candidates(env: &PathEnv, paths: &mut Vec<PathBuf>) {
    match &env.home {
        Some(home) => {
            paths.push(home.join("Library/Application Support/Steam/steamapps/common/Balatro"));
        }
        None => error!("Impossible to get your home dir!"),
    }
}

fn scan_for_steamapps_dirs(
    fs: &dyn FsProbe,
    base: &Path,
    max_depth: usize,
    max_dirs: usize,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut queue: VecDeque<(PathBuf, usize)> = VecDeque::new();
    let mut visited: usize = 0;

    if !fs.exists(base) {
        return found;
    }
    queue.push_back((base.to_path_buf(), 0));

    while let Some((dir, depth)) = queue.pop_front() {
        if visited >= max_dirs {
            break;
        }
        visited += 1;

        let dir_name = dir
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_lowercase();

        if dir_name == "steamapps" {
            found.push(dir);
            continue;
        }
        if depth >= max_depth {
            continue;
        }
        if matches!(
            dir_name.as_str(),
            "node_modules" | ".git" | "cache" | ".cache" | "trash" | ".trash"
        ) {
            continue;
        }
        for child in fs.list_dir(&dir) {
            queue.push_back((child, depth + 1));
        }
    }
    found
}

fn linux_candidates(env: &PathEnv, fs: &dyn FsProbe, paths: &mut Vec<PathBuf>) {
    let mut steam_roots: Vec<PathBuf> = Vec::new();
    let mut seen_roots: HashSet<String> = HashSet::new();
    let mut add_root = |root: PathBuf| {
        if seen_roots.insert(path_key(&root)) {
            steam_roots.push(root);
        }
    };

    match &env.home {
        Some(home) => {
            // Native Steam installations
            add_root(home.join(".local/share/Steam"));
            add_root(home.join(".steam/steam"));
            add_root(home.join(".steam/root"));
            add_root(home.join(".steam/debian-installation"));

            // Flatpak Steam installations - check all known variants
            add_root(home.join(".var/app/com.valvesoftware.Steam/data/Steam"));
            add_root(home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"));
            add_root(home.join(".var/app/com.valvesoftware.Steam/.steam/steam"));
            add_root(home.join(".var/app/com.valvesoftware.Steam/.steam/root"));
            add_root(home.join(".var/app/com.valvesoftware.Steam/.steam/debian-installation"));

            // Snap installs keep the real Steam data under this prefix.
            add_root(home.join("snap/steam/common/.local/share/Steam"));

            debug!("Home directory: {:?}", home);
        }
        None => error!("Impossible to get your home dir!"),
    }

    // When running inside a Flatpak, also try the host home via /var/home or direct path
    // in case the sandbox path resolution differs
    if env.flatpak {
        debug!("Running inside Flatpak sandbox, checking additional host paths");
        if let Some(user) = &env.user {
            let host_home = PathBuf::from("/var/home").join(user);
            if fs.exists(&host_home) {
                add_root(host_home.join(".local/share/Steam"));
                add_root(host_home.join(".steam/steam"));
                add_root(host_home.join(".var/app/com.valvesoftware.Steam/data/Steam"));
            }
            // Also try /home directly (common Flatpak host mount)
            let direct_home = PathBuf::from("/home").join(user);
            add_root(direct_home.join(".local/share/Steam"));
            add_root(direct_home.join(".steam/steam"));
            add_root(direct_home.join(".var/app/com.valvesoftware.Steam/data/Steam"));
        }
    }

    if let Some(xdg) = &env.xdg_data_home {
        add_root(xdg.join("Steam"));
        add_root(xdg.join("steam"));
    }

    add_root(PathBuf::from("/opt/steam"));
    add_root(PathBuf::from("/opt/Steam"));
    add_root(PathBuf::from("/usr/lib/steam"));
    add_root(PathBuf::from("/usr/local/steam"));
    add_root(PathBuf::from("/var/lib/steam"));

    let mut steamapps_dirs: Vec<PathBuf> = Vec::new();
    let mut seen_steamapps: HashSet<String> = HashSet::new();
    let mut add_steamapps = |path: PathBuf| {
        if seen_steamapps.insert(path_key(&path)) {
            steamapps_dirs.push(path);
        }
    };

    for root in steam_roots {
        add_steamapps(root.join("steamapps"));
    }

    if let Some(home) = &env.home {
        add_steamapps(home.join(".steam/steamapps"));
        let bases = [
            home.join(".local/share"),
            home.join(".steam"),
            home.join(".var/app"),
            home.join("snap"),
            home.clone(),
        ];
        for base in bases {
            for steamapps in scan_for_steamapps_dirs(fs, &base, 5, 2000) {
                add_steamapps(steamapps);
            }
        }
    }

    if let Some(user) = &env.user {
        // Steam Deck SD cards and other removable libraries mount here.
        let media_base = PathBuf::from("/run/media").join(user);
        for steamapps in scan_for_steamapps_dirs(fs, &media_base, 4, 1500) {
            add_steamapps(steamapps);
        }
    }

    for steamapps in steamapps_dirs {
        paths.push(steamapps.join("common/Balatro"));
        let libraryfolders = steamapps.join("libraryfolders.vdf");
        if let Some(contents) = fs.read_to_string(&libraryfolders) {
            for lib in vdf_library_paths(&contents) {
                paths.push(PathBuf::from(lib).join("steamapps/common/Balatro"));
            }
        }
    }
}

/// Dedupe on the canonical path so a symlinked alias of the same install collapses too.
fn dedup_paths_case_insensitive(paths: &mut Vec<PathBuf>) {
    let mut seen: HashSet<String> = HashSet::new();
    paths.retain(|p| {
        let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let key = canon.to_string_lossy().to_string().to_lowercase();
        seen.insert(key)
    });
}

/// Balatro installs on this machine.
pub fn get_balatro_paths() -> Vec<PathBuf> {
    let mut paths = balatro_paths_in(&PathEnv::current(), &RealFs);
    dedup_paths_case_insensitive(&mut paths);
    paths
}

pub fn is_steam_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        let system = System::new_all();
        let x = system
            .processes_by_exact_name(std::ffi::OsStr::new("steam.exe"))
            .next()
            .is_some();
        x
    }

    #[cfg(target_family = "unix")]
    {
        use libproc::proc_pid::name;
        use libproc::processes;

        if let Ok(pids) = processes::pids_by_type(processes::ProcFilter::All) {
            for pid in pids {
                if let Ok(name) = name(pid as i32)
                    && name.to_lowercase().contains("steam")
                {
                    return true;
                }
            }
        }
        false
    }
}

pub fn get_installed_mods() -> Vec<String> {
    let mut installed_mods_paths: Vec<PathBuf> = vec![];

    let mod_dir = crate::mods_dir();

    if !mod_dir.exists() {
        return vec![];
    }

    match mod_dir.read_dir() {
        Ok(read_dir) => {
            for entry in read_dir.flatten() {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    installed_mods_paths.push(entry.path());
                }
            }
        }
        Err(e) => {
            error!("Failed to read mods directory {}: {}", mod_dir.display(), e);
            return vec![];
        }
    }

    installed_mods_paths
        .iter()
        .filter_map(|p| {
            // Filter out internal directories by exact folder name match (not substring)
            let folder_name = p.file_name()?.to_str()?.to_lowercase();
            if folder_name == ".lovely" || folder_name == "lovely" || folder_name == "bmm-compat" {
                return None;
            }
            p.to_str().map(|s| s.to_string())
        })
        .collect()
}

pub fn is_balatro_running() -> bool {
    #[cfg(target_os = "windows")]
    {
        let system = System::new_all();
        let exact = system
            .processes_by_exact_name(std::ffi::OsStr::new("Balatro.exe"))
            .next()
            .is_some();
        if exact {
            return true;
        }
        for process in system.processes().values() {
            let name = process.name().to_string_lossy().to_lowercase();
            if name.contains("balatro")
                && !name.contains("balatro-mod")
                && !name.contains("balatro mod")
                && !name.contains("balatro_mod")
            {
                return true;
            }
        }
        false
    }

    #[cfg(target_family = "unix")]
    {
        use libproc::proc_pid::name;
        use libproc::processes;

        let balatro_roots: Vec<_> = crate::finder::get_balatro_paths_cached()
            .into_iter()
            .map(|p| p.canonicalize().unwrap_or(p))
            .collect();
        #[cfg(target_os = "macos")]
        let balatro_exec_roots: Vec<_> = balatro_roots
            .iter()
            .map(|root| root.join("Balatro.app").join("Contents").join("MacOS"))
            .collect();

        let current_pid = std::process::id() as i32;
        let self_exe_path = std::env::current_exe().ok();
        let self_exe_name = self_exe_path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));

        if let Ok(pids) = processes::pids_by_type(processes::ProcFilter::All) {
            for pid in pids {
                let pid = pid as i32;
                if pid == current_pid {
                    continue;
                }

                if let Ok(name) = name(pid) {
                    let name_lower = name.to_lowercase();

                    #[cfg(target_os = "macos")]
                    {
                        if let Ok(proc_path) = libproc::proc_pid::pidpath(pid) {
                            let proc_path = PathBuf::from(proc_path);
                            if proc_path
                                .to_string_lossy()
                                .to_lowercase()
                                .contains("balatro.app")
                            {
                                return true;
                            }
                            for root in &balatro_exec_roots {
                                if proc_path.starts_with(root) {
                                    return true;
                                }
                            }
                        }
                    }

                    #[cfg(target_os = "linux")]
                    {
                        // Skip processes whose executable path matches our own (covers AppImage/wrapped launches).
                        if let (Some(self_path), Ok(proc_path)) = (
                            &self_exe_path,
                            std::fs::read_link(format!("/proc/{pid}/exe")),
                        ) && proc_path == *self_path
                        {
                            continue;
                        }
                    }

                    // Linux can truncate binary names; skip anything that matches our own executable name or obvious variants.
                    if let Some(self_name) = self_exe_name.as_deref()
                        && (name_lower == self_name
                            || self_name.starts_with(&name_lower)
                            || name_lower.starts_with(self_name))
                    {
                        continue;
                    }
                    if name_lower.contains("balatro-mod")
                        || name_lower.contains("balatro mod")
                        || name_lower.contains("balatro_mod")
                        || name_lower == "bmm"
                    {
                        continue;
                    }

                    if name_lower.contains("balatro") {
                        return true;
                    }
                    if name_lower == "love"
                        && let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd"))
                    {
                        let cwd = cwd.canonicalize().unwrap_or(cwd);
                        if balatro_roots.contains(&cwd) {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use tempfile::tempdir;

    fn set_var(key: &str, val: impl AsRef<OsStr>) {
        unsafe { std::env::set_var(key, val) };
    }

    fn remove_var(key: &str) {
        unsafe { std::env::remove_var(key) };
    }

    #[test]
    fn test_get_installed_mods_filters_lovely_dirs() {
        let _lock = crate::ENV_LOCK.lock().unwrap();
        let tmp = tempdir().unwrap();
        // Redirect config dir to temp using XDG_CONFIG_HOME and HOME (for macOS)
        let original_cfg = std::env::var_os("XDG_CONFIG_HOME");
        let original_home = std::env::var_os("HOME");
        set_var("XDG_CONFIG_HOME", tmp.path());
        // Ensure HOME points at temp so path resolution doesn't touch real user dirs.
        set_var("HOME", tmp.path());

        let mods_root = dirs::config_dir().unwrap().join("Balatro").join("Mods");
        std::fs::create_dir_all(&mods_root).unwrap();

        // Create sample mod directories
        let keep_a = mods_root.join("CoolMod");
        let keep_b = mods_root.join("Another");
        let ignore_a = mods_root.join(".lovely");
        let ignore_b = mods_root.join("lovely");

        std::fs::create_dir_all(&keep_a).unwrap();
        std::fs::create_dir_all(&keep_b).unwrap();
        std::fs::create_dir_all(&ignore_a).unwrap();
        std::fs::create_dir_all(&ignore_b).unwrap();

        let mut mods = super::get_installed_mods();
        mods.sort();

        // Should only include the two non-lovely directories
        assert_eq!(mods.len(), 2);
        assert!(mods.iter().any(|p| p.ends_with("CoolMod")));
        assert!(mods.iter().any(|p| p.ends_with("Another")));

        // restore environment
        match original_cfg {
            Some(val) => set_var("XDG_CONFIG_HOME", val),
            None => remove_var("XDG_CONFIG_HOME"),
        }
        match original_home {
            Some(val) => set_var("HOME", val),
            None => remove_var("HOME"),
        }
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    /// In-memory filesystem. Paths are normalised (`\` -> `/`, no trailing slash) so Windows
    /// layouts can be exercised on any host.
    #[derive(Default)]
    struct FakeFs {
        files: BTreeMap<String, String>,
        dirs: BTreeSet<String>,
    }

    fn norm(p: &Path) -> String {
        let s = p.to_string_lossy().replace('\\', "/");
        let mut out = String::new();
        for (i, part) in s.split('/').filter(|c| !c.is_empty()).enumerate() {
            if i > 0 || s.starts_with('/') {
                out.push('/');
            }
            out.push_str(part);
        }
        out
    }

    impl FakeFs {
        fn dir(&mut self, p: &str) -> &mut Self {
            let mut cur = String::new();
            let n = norm(Path::new(p));
            for part in n.split('/').filter(|c| !c.is_empty()) {
                if cur.is_empty() && !n.starts_with('/') {
                    cur = part.to_string();
                } else {
                    cur = format!("{cur}/{part}");
                }
                self.dirs.insert(cur.clone());
            }
            self
        }
        fn file(&mut self, p: &str, contents: &str) -> &mut Self {
            let n = norm(Path::new(p));
            if let Some((parent, _)) = n.rsplit_once('/') {
                self.dir(parent);
            }
            self.files.insert(n, contents.to_string());
            self
        }
    }

    impl FsProbe for FakeFs {
        fn exists(&self, p: &Path) -> bool {
            let n = norm(p);
            self.files.contains_key(&n) || self.dirs.contains(&n)
        }
        fn read_to_string(&self, p: &Path) -> Option<String> {
            self.files.get(&norm(p)).cloned()
        }
        fn list_dir(&self, p: &Path) -> Vec<PathBuf> {
            let parent = norm(p);
            self.dirs
                .iter()
                .filter(|d| d.rsplit_once('/').map(|(pp, _)| pp) == Some(parent.as_str()))
                .map(PathBuf::from)
                .collect()
        }
    }

    fn env(os: Os) -> PathEnv {
        PathEnv {
            os,
            home: None,
            custom_path: None,
            steam_install: None,
            flatpak: false,
            user: None,
            xdg_data_home: None,
        }
    }

    fn found(env: &PathEnv, fs: &FakeFs) -> Vec<String> {
        balatro_paths_in(env, fs).iter().map(|p| norm(p)).collect()
    }

    fn vdf(libs: &[&str]) -> String {
        let mut s = String::from("\"libraryfolders\"\n{\n");
        for (i, l) in libs.iter().enumerate() {
            s.push_str(&format!("\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{l}\"\n\t}}\n"));
        }
        s.push('}');
        s
    }

    #[test]
    fn windows_default_steam_location() {
        let mut fs = FakeFs::default();
        fs.file(
            r"C:\Program Files (x86)\Steam\steamapps\libraryfolders.vdf",
            &vdf(&[r"C:\\Program Files (x86)\\Steam"]),
        )
        .dir(r"C:\Program Files (x86)\Steam\steamapps\common\Balatro");
        assert_eq!(
            found(&env(Os::Windows), &fs),
            ["C:/Program Files (x86)/Steam/steamapps/common/Balatro"]
        );
    }

    #[test]
    fn windows_second_library_from_registry_root() {
        let mut e = env(Os::Windows);
        e.steam_install = Some(r"E:\Steam".into());
        let mut fs = FakeFs::default();
        fs.file(
            r"E:\Steam\steamapps\libraryfolders.vdf",
            &vdf(&[r"E:\\Steam", r"D:\\Games\\SteamLibrary"]),
        )
        .dir(r"D:\Games\SteamLibrary\steamapps\common\Balatro");
        assert_eq!(
            found(&e, &fs),
            ["D:/Games/SteamLibrary/steamapps/common/Balatro"]
        );
    }

    #[test]
    fn macos_application_support() {
        let mut e = env(Os::MacOs);
        e.home = Some(PathBuf::from("/Users/kh"));
        let mut fs = FakeFs::default();
        fs.dir("/Users/kh/Library/Application Support/Steam/steamapps/common/Balatro");
        assert_eq!(
            found(&e, &fs),
            ["/Users/kh/Library/Application Support/Steam/steamapps/common/Balatro"]
        );
    }

    #[test]
    fn linux_native_steam() {
        let mut e = env(Os::Linux);
        e.home = Some(PathBuf::from("/home/kh"));
        let mut fs = FakeFs::default();
        fs.dir("/home/kh/.local/share/Steam/steamapps/common/Balatro");
        assert_eq!(
            found(&e, &fs),
            ["/home/kh/.local/share/Steam/steamapps/common/Balatro"]
        );
    }

    #[test]
    fn linux_flatpak_steam() {
        let mut e = env(Os::Linux);
        e.home = Some(PathBuf::from("/home/kh"));
        let mut fs = FakeFs::default();
        fs.dir(
            "/home/kh/.var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common/Balatro",
        );
        assert_eq!(
            found(&e, &fs),
            [
                "/home/kh/.var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common/Balatro"
            ]
        );
    }

    #[test]
    fn steam_deck_internal_and_sd_card_with_proton_prefix() {
        let mut e = env(Os::Linux);
        e.home = Some(PathBuf::from("/home/deck"));
        e.user = Some("deck".into());
        let mut fs = FakeFs::default();
        fs.dir("/home/deck/.local/share/Steam/steamapps/common/Balatro")
            .dir("/home/deck/.local/share/Steam/steamapps/compatdata/2379780/pfx")
            .dir("/run/media/deck/mmcblk0p1/steamapps/common/Balatro")
            .dir("/run/media/deck/mmcblk0p1/steamapps/compatdata/2379780/pfx");
        assert_eq!(
            found(&e, &fs),
            [
                "/home/deck/.local/share/Steam/steamapps/common/Balatro",
                "/run/media/deck/mmcblk0p1/steamapps/common/Balatro",
            ]
        );
    }

    #[test]
    fn linux_library_listed_in_libraryfolders() {
        let mut e = env(Os::Linux);
        e.home = Some(PathBuf::from("/home/kh"));
        let mut fs = FakeFs::default();
        fs.file(
            "/home/kh/.local/share/Steam/steamapps/libraryfolders.vdf",
            &vdf(&["/mnt/games/SteamLibrary"]),
        )
        .dir("/mnt/games/SteamLibrary/steamapps/common/Balatro");
        assert_eq!(
            found(&e, &fs),
            ["/mnt/games/SteamLibrary/steamapps/common/Balatro"]
        );
    }

    #[test]
    fn nothing_installed_yields_empty() {
        for os in [Os::Windows, Os::MacOs, Os::Linux] {
            let mut e = env(os);
            e.home = Some(PathBuf::from("/home/kh"));
            assert!(found(&e, &FakeFs::default()).is_empty(), "{os:?}");
        }
    }

    #[test]
    fn same_install_via_two_library_entries_is_returned_once() {
        let mut e = env(Os::Windows);
        e.steam_install = Some(r"C:\Steam".into());
        let mut fs = FakeFs::default();
        fs.file(
            r"C:\Steam\steamapps\libraryfolders.vdf",
            &vdf(&[r"C:\\Steam", r"c:\\steam", r"D:\\Lib", r"D:\\Lib"]),
        )
        .dir(r"C:\Steam\steamapps\common\Balatro")
        .dir(r"D:\Lib\steamapps\common\Balatro");
        let got = found(&e, &fs);
        assert_eq!(got.len(), 2, "{got:?}");
    }

    #[test]
    fn result_order_is_stable_and_custom_path_comes_first() {
        let mut e = env(Os::Linux);
        e.home = Some(PathBuf::from("/home/kh"));
        e.custom_path = Some(PathBuf::from("/games/MyBalatro"));
        let mut fs = FakeFs::default();
        fs.dir("/games/MyBalatro")
            .dir("/home/kh/.local/share/Steam/steamapps/common/Balatro")
            .dir("/home/kh/.steam/steam/steamapps/common/Balatro");
        let first = found(&e, &fs);
        assert_eq!(first[0], "/games/MyBalatro");
        assert!(first.len() >= 2);
        for _ in 0..5 {
            assert_eq!(found(&e, &fs), first);
        }
    }

    #[test]
    fn missing_custom_path_is_ignored() {
        let mut e = env(Os::MacOs);
        e.home = Some(PathBuf::from("/Users/kh"));
        e.custom_path = Some(PathBuf::from("/gone"));
        assert!(found(&e, &FakeFs::default()).is_empty());
    }

    #[test]
    fn vdf_escapes_are_decoded() {
        assert_eq!(
            vdf_library_paths(&vdf(&[r"D:\\Games\\Steam Library"])),
            [r"D:\Games\Steam Library"]
        );
    }
}
