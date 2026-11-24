//! Catalog sync from the index git repo, with an atomic "current build" pointer.
//!
//! Builds land in `<catalog_dir>/build-<ms>/`. The active one is published as a `current`
//! symlink on unix, or a `current.txt` pointer file elsewhere (symlinks need privileges on
//! Windows). Both are replaced with write-temp-then-rename so readers never see a half state.
use crate::config::Config;
use crate::db::now_sec;
use crate::store::{LoadStats, Store};
use anyhow::{Context, Result, bail};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncStatus {
    pub last_run_at: Option<i64>,
    pub last_ok_at: Option<i64>,
    pub last_error: Option<String>,
    pub head: Option<String>,
    pub running: bool,
    /// HEAD whose build failed; not retried until HEAD changes (or a forced reload).
    #[serde(skip)]
    pub failed_head: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub changed: bool,
}

const POINTER_FILE: &str = "current.txt";
const COMMAND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

/// Resolve the directory holding the active catalog (`index.json` + `thumbnails/`).
pub fn current_dir(catalog_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        Some(catalog_dir.join("current")),
        pointer_target(catalog_dir),
        Some(catalog_dir.to_path_buf()),
    ];
    candidates
        .into_iter()
        .flatten()
        .find(|d| d.join("index.json").is_file())
        .and_then(|d| std::fs::canonicalize(d).ok())
}

/// The directory named by `current.txt`, if the pointer file exists.
fn pointer_target(catalog_dir: &Path) -> Option<PathBuf> {
    let name = std::fs::read_to_string(catalog_dir.join(POINTER_FILE)).ok()?;
    let name = name.trim();
    // A pointer is a bare directory name, never a path.
    (!name.is_empty() && !name.contains(['/', '\\']) && name != "..")
        .then(|| catalog_dir.join(name))
}

/// Atomically make `<catalog_dir>/<name>` the active build.
pub fn publish_current(catalog_dir: &Path, name: &str) -> Result<()> {
    #[cfg(unix)]
    {
        let tmp = catalog_dir.join("current.tmp");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(name, &tmp)?;
        std::fs::rename(&tmp, catalog_dir.join("current"))?;
    }
    write_pointer(catalog_dir, name)
}

/// The pointer file is written on every OS so `current_dir` has one mechanism that works
/// everywhere; the symlink is an extra on unix.
fn write_pointer(catalog_dir: &Path, name: &str) -> Result<()> {
    let tmp = catalog_dir.join("current.txt.tmp");
    std::fs::write(&tmp, name)?;
    std::fs::rename(&tmp, catalog_dir.join(POINTER_FILE))?;
    Ok(())
}

async fn run(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new(program);
    // The index repo is untrusted input: its build script must not see the server's secrets
    // (ADMIN_TOKEN, ...), so children get a minimal environment.
    cmd.args(args).kill_on_drop(true).env_clear();
    for key in [
        "PATH",
        "HOME",
        "LANG",
        "SYSTEMROOT",
        "TEMP",
        "TMP",
        "TMPDIR",
    ] {
        if let Some(v) = std::env::var_os(key) {
            cmd.env(key, v);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(c) = cwd {
        cmd.current_dir(c);
    }
    // `kill_on_drop` reaps the child when the timeout drops the future, so a stalled fetch
    // cannot wedge the sync forever.
    let out = tokio::time::timeout(COMMAND_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow::anyhow!("{program} timed out after {}s", COMMAND_TIMEOUT.as_secs()))?
        .with_context(|| format!("spawning {program}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let tail: String = err
            .trim()
            .chars()
            .rev()
            .take(500)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        bail!(
            "{program} {} failed ({}): {tail}",
            args.join(" "),
            out.status
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn python() -> &'static str {
    if cfg!(windows) { "python" } else { "python3" }
}

/// Loads the catalog at boot and keeps it current from the index git repo.
pub struct IndexSync {
    cfg: Config,
    store: Arc<RwLock<Store>>,
    status: Mutex<SyncStatus>,
}

impl IndexSync {
    pub fn new(cfg: Config, store: Arc<RwLock<Store>>) -> IndexSync {
        IndexSync {
            cfg,
            store,
            status: Mutex::new(SyncStatus::default()),
        }
    }

    pub fn status(&self) -> SyncStatus {
        self.status.lock().clone()
    }

    /// Load whatever catalog is already on disk (fast; used before the first network sync): the
    /// published build, else the newest earlier build that loads.
    pub fn load_from_disk(&self) -> Option<LoadStats> {
        let mut candidates: Vec<PathBuf> = current_dir(&self.cfg.catalog_dir).into_iter().collect();
        if let Ok(rd) = std::fs::read_dir(&self.cfg.catalog_dir) {
            let mut builds: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("build-"))
                .map(|e| e.path())
                .collect();
            builds.sort();
            builds.reverse();
            candidates.extend(builds);
        }
        for dir in candidates {
            let Ok(dir) = std::fs::canonicalize(&dir) else {
                continue;
            };
            match self.store.write().load(&dir, now_sec()) {
                Ok(stats) => return Some(stats),
                Err(e) => {
                    tracing::warn!(event = "catalog_disk_load_failed", dir = %dir.display(), error = %format!("{e:#}"))
                }
            }
        }
        None
    }

    pub async fn sync_once(&self, force: bool) -> SyncOutcome {
        if self.cfg.index_repo_url.is_none() {
            return SyncOutcome::default();
        }
        {
            let mut s = self.status.lock();
            if s.running {
                return SyncOutcome::default();
            }
            s.running = true;
            s.last_run_at = Some(now_sec());
        }
        let result = self.sync_inner(force).await;
        let mut s = self.status.lock();
        s.running = false;
        match result {
            Ok(changed) => {
                s.last_ok_at = s.last_run_at;
                s.last_error = None;
                SyncOutcome { changed }
            }
            Err(e) => {
                s.last_error = Some(format!("{e:#}"));
                tracing::error!(event = "catalog_sync_failed", error = %format!("{e:#}"));
                SyncOutcome::default()
            }
        }
    }

    async fn sync_inner(&self, force: bool) -> Result<bool> {
        let cfg = &self.cfg;
        let repo = cfg
            .index_repo_url
            .as_deref()
            .context("no index repo configured")?;
        let index_dir = cfg.index_dir.as_path();
        if cfg.index_branch.starts_with('-') || cfg.index_branch.contains(char::is_whitespace) {
            bail!("INDEX_BRANCH is not a valid branch name");
        }
        if !index_dir.join(".git").exists() {
            std::fs::create_dir_all(index_dir)?;
            let dir = index_dir.to_string_lossy();
            run(
                "git",
                &[
                    "-c",
                    "core.symlinks=false",
                    "clone",
                    "--filter=blob:none",
                    "--branch",
                    &cfg.index_branch,
                    repo,
                    &dir,
                ],
                None,
            )
            .await?;
        }
        // Symlinks in the index repo become plain text files: a mod must not be able to point its
        // description or thumbnail at a file on this host.
        run(
            "git",
            &["config", "core.symlinks", "false"],
            Some(index_dir),
        )
        .await?;
        run(
            "git",
            &[
                "-c",
                "http.lowSpeedLimit=1000",
                "-c",
                "http.lowSpeedTime=60",
                "fetch",
                "--prune",
                "origin",
                &cfg.index_branch,
            ],
            Some(index_dir),
        )
        .await?;
        let remote = format!("origin/{}", cfg.index_branch);
        let head = run("git", &["rev-parse", &remote], Some(index_dir)).await?;
        let head_file = cfg.catalog_dir.join("HEAD");
        let built = std::fs::read_to_string(&head_file)
            .ok()
            .map(|s| s.trim().to_string());
        self.status.lock().head = Some(head.clone());
        if !force && built.as_deref() == Some(head.as_str()) && self.store.read().ready() {
            return Ok(false);
        }
        if !force && self.status.lock().failed_head.as_deref() == Some(head.as_str()) {
            return Ok(false); // this commit already failed to build; wait for a new one
        }
        run("git", &["reset", "--hard", &remote], Some(index_dir)).await?;

        std::fs::create_dir_all(&cfg.catalog_dir)?;
        let name = format!(
            "build-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis()
        );
        let out_dir = cfg.catalog_dir.join(&name);
        let script = index_dir.join("scripts").join("build_index.py");
        let built_ok: Result<LoadStats> = async {
            run(
                python(),
                &[
                    &script.to_string_lossy(),
                    "--out",
                    &out_dir.to_string_lossy(),
                ],
                Some(index_dir),
            )
            .await?;
            // Validate by loading it *before* publishing, so a bad build can never become the
            // build the server picks up at its next restart.
            let store = self.store.clone();
            let dir = std::fs::canonicalize(&out_dir)?;
            let stats =
                tokio::task::spawn_blocking(move || store.write().load(&dir, now_sec())).await??;
            publish_current(&cfg.catalog_dir, &name)?;
            Ok(stats)
        }
        .await;
        let stats = match built_ok {
            Ok(s) => s,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&out_dir);
                self.status.lock().failed_head = Some(head.clone());
                self.prune(None);
                return Err(e);
            }
        };
        self.status.lock().failed_head = None;
        std::fs::write(&head_file, &head)?;
        self.prune(Some(&name));
        tracing::info!(event = "catalog_synced", head = %head, total = stats.total, added = stats.added, removed = stats.removed);
        Ok(true)
    }

    /// Keep the active build and the previous one.
    fn prune(&self, keep: Option<&str>) {
        let Ok(rd) = std::fs::read_dir(&self.cfg.catalog_dir) else {
            return;
        };
        let mut builds: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with("build-"))
            .collect();
        builds.sort();
        let drop = builds.len().saturating_sub(2);
        for old in builds
            .into_iter()
            .take(drop)
            .filter(|o| Some(o.as_str()) != keep)
        {
            let _ = std::fs::remove_dir_all(self.cfg.catalog_dir.join(old));
        }
    }
}
