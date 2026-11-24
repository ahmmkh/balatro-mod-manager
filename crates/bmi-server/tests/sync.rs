mod common;
use bmi_server::app::AppState;
use bmi_server::maintenance::prune_reports;
use bmi_server::sync::{IndexSync, current_dir, publish_current};
use common::*;
use std::path::Path;
use std::process::Command;

fn sh(dir: &Path, prog: &str, args: &[&str]) {
    let o = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "{prog} {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}

/// A tiny stand-in for the real index repo: mods/<id>/meta.json plus a stdlib-only build script
/// with the same `--out` contract as scripts/build_index.py.
const BUILD: &str = r#"
import json, sys, pathlib
out = pathlib.Path(sys.argv[sys.argv.index("--out") + 1])
(out / "thumbnails").mkdir(parents=True)
mods = []
for d in sorted(pathlib.Path("mods").iterdir()):
    meta = json.loads((d / "meta.json").read_text())
    desc = (d / "description.md")
    description = desc.read_text() if desc.exists() else ""
    import os
    description += os.environ.get("BMI_TEST_SECRET", "")
    mods.append({"id": d.name, "name": meta["title"], "updated_at": "1", "description": description, "meta": meta})
(out / "index.json").write_text(json.dumps({"mods": mods}))
"#;

fn make_repo(ids: &[&str]) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    let p = d.path();
    sh(p, "git", &["init", "-q", "-b", "main"]);
    sh(p, "git", &["config", "user.email", "t@t"]);
    sh(p, "git", &["config", "user.name", "t"]);
    std::fs::create_dir_all(p.join("scripts")).unwrap();
    std::fs::write(p.join("scripts/build_index.py"), BUILD).unwrap();
    for id in ids {
        add_mod(p, id);
    }
    sh(p, "git", &["add", "-A"]);
    sh(p, "git", &["commit", "-q", "-m", "init"]);
    d
}
fn add_mod(p: &Path, id: &str) {
    std::fs::create_dir_all(p.join("mods").join(id)).unwrap();
    std::fs::write(
        p.join("mods").join(id).join("meta.json"),
        format!(r#"{{"title":"{id}"}}"#),
    )
    .unwrap();
}

fn synced_fx(repo: &Path) -> (Fx, IndexSync) {
    let f = fx_with(|c| {
        c.index_repo_url = Some(repo.to_string_lossy().into_owned());
    });
    let sync = IndexSync::new((*f.st.cfg).clone(), f.st.store.clone());
    (f, sync)
}

#[tokio::test]
async fn sync_clones_builds_swaps_and_skips_when_unchanged() {
    let repo = make_repo(&["a@A", "b@B"]);
    let (f, sync) = synced_fx(repo.path());
    assert!(sync.sync_once(false).await.changed, "{:?}", sync.status());
    assert_eq!(f.st.store.read().mods().len(), 2);
    assert!(current_dir(&f.st.cfg.catalog_dir).is_some());
    assert!(sync.status().last_error.is_none());
    assert!(
        !sync.sync_once(false).await.changed,
        "same HEAD, nothing to do"
    );
    assert!(sync.sync_once(true).await.changed, "force rebuilds");

    add_mod(repo.path(), "c@C");
    sh(repo.path(), "git", &["add", "-A"]);
    sh(repo.path(), "git", &["commit", "-q", "-m", "more"]);
    assert!(sync.sync_once(false).await.changed);
    assert_eq!(f.st.store.read().mods().len(), 3);
    let builds = std::fs::read_dir(&f.st.cfg.catalog_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("build-"))
        .count();
    assert!(builds <= 2, "old builds pruned, got {builds}");
}

#[tokio::test]
async fn a_failed_sync_keeps_the_previous_catalog() {
    let repo = make_repo(&["a@A"]);
    let (f, sync) = synced_fx(repo.path());
    assert!(sync.sync_once(false).await.changed);
    std::fs::write(
        repo.path().join("scripts/build_index.py"),
        "raise SystemExit(3)",
    )
    .unwrap();
    sh(repo.path(), "git", &["commit", "-qam", "break build"]);
    assert!(!sync.sync_once(false).await.changed);
    assert!(sync.status().last_error.is_some());
    assert_eq!(f.st.store.read().mods().len(), 1);
    assert!(!sync.status().running);
}

#[tokio::test]
async fn load_from_disk_serves_the_last_published_build() {
    let repo = make_repo(&["a@A"]);
    let (f, sync) = synced_fx(repo.path());
    sync.sync_once(false).await;
    let fresh = AppState::build((*f.st.cfg).clone(), f.st.now.clone()).unwrap();
    let again = IndexSync::new((*fresh.cfg).clone(), fresh.store.clone());
    assert_eq!(again.load_from_disk().unwrap().total, 1);
    assert!(fresh.store.read().ready());
}

#[test]
fn pointer_file_resolves_the_active_build_on_any_os() {
    let d = tempfile::tempdir().unwrap();
    let a = d.path().join("build-1");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(a.join("index.json"), "{}").unwrap();
    std::fs::write(d.path().join("current.txt"), "build-1").unwrap();
    assert_eq!(
        current_dir(d.path()),
        Some(std::fs::canonicalize(&a).unwrap())
    );
    // a pointer may never escape the catalog dir
    std::fs::write(d.path().join("current.txt"), "../elsewhere").unwrap();
    assert_eq!(current_dir(d.path()), None);
    assert_eq!(current_dir(&d.path().join("missing")), None);
}

#[test]
fn publish_current_replaces_atomically() {
    let d = tempfile::tempdir().unwrap();
    for n in ["build-1", "build-2"] {
        std::fs::create_dir_all(d.path().join(n)).unwrap();
        std::fs::write(d.path().join(n).join("index.json"), n).unwrap();
    }
    publish_current(d.path(), "build-1").unwrap();
    publish_current(d.path(), "build-2").unwrap();
    assert_eq!(
        current_dir(d.path()),
        Some(std::fs::canonicalize(d.path().join("build-2")).unwrap())
    );
}

#[test]
fn old_reports_are_pruned() {
    let f = fx();
    let now = T0;
    {
        let c = f.st.reports.lock().unwrap();
        for age_days in [1i64, 29, 31, 90] {
            c.execute(
                "INSERT INTO reports (created_at,title,description,mm_version,os,arch,cpu,gpu,ram,log_filename,log_gz,log_bytes) VALUES (?1,'t','d','','','','','','','',x'00',0)",
                [now - age_days * 86_400],
            )
            .unwrap();
        }
    }
    assert_eq!(prune_reports(&f.st.reports, now).unwrap(), 2);
    let left: i64 =
        f.st.reports
            .lock()
            .unwrap()
            .query_row("SELECT count(*) FROM reports", [], |r| r.get(0))
            .unwrap();
    assert_eq!(left, 2);
}

#[test]
fn prune_shrinks_the_reports_file() {
    let f = fx();
    let blob = vec![7u8; 200_000];
    {
        let c = f.st.reports.lock().unwrap();
        for i in 0..40 {
            c.execute(
                "INSERT INTO reports (created_at,title,description,mm_version,os,arch,cpu,gpu,ram,log_filename,log_gz,log_bytes) VALUES (?1,'t','d','','','','','','','',?2,0)",
                rusqlite::params![T0 - 40 * 86_400 - i, blob],
            )
            .unwrap();
        }
        c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .unwrap();
    }
    let size = || {
        std::fs::metadata(f._dir.path().join("reports.sqlite"))
            .unwrap()
            .len()
    };
    let before = size();
    assert_eq!(prune_reports(&f.st.reports, T0).unwrap(), 40);
    assert!(size() < before / 2, "before {before}, after {}", size());
}

fn build_dirs(catalog: &Path) -> usize {
    std::fs::read_dir(catalog)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_name().to_string_lossy().starts_with("build-"))
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn a_build_with_no_usable_mods_is_never_published_and_is_cleaned_up() {
    let repo = make_repo(&["a@A"]);
    let (f, sync) = synced_fx(repo.path());
    assert!(sync.sync_once(false).await.changed);
    let before = build_dirs(&f.st.cfg.catalog_dir);
    // a build whose index.json contains only an invalid entry
    std::fs::write(
        repo.path().join("scripts/build_index.py"),
        r#"
import sys, pathlib, json
out = pathlib.Path(sys.argv[sys.argv.index("--out") + 1]); (out / "thumbnails").mkdir(parents=True)
(out / "index.json").write_text(json.dumps({"mods": [{"bad": 1}]}))
"#,
    )
    .unwrap();
    sh(repo.path(), "git", &["commit", "-qam", "bad build"]);
    assert!(!sync.sync_once(false).await.changed);
    assert!(sync.status().last_error.is_some());
    assert_eq!(
        build_dirs(&f.st.cfg.catalog_dir),
        before,
        "failed build directory removed"
    );
    // retrying the same HEAD does not rebuild
    let _ = sync.sync_once(false).await;
    assert_eq!(build_dirs(&f.st.cfg.catalog_dir), before);
    // a restart still finds the last good catalog
    let fresh = AppState::build((*f.st.cfg).clone(), f.st.now.clone()).unwrap();
    let again = IndexSync::new((*fresh.cfg).clone(), fresh.store.clone());
    assert!(again.load_from_disk().is_some());
    assert!(fresh.store.read().ready());
}

#[tokio::test]
async fn one_malformed_entry_is_skipped_not_fatal() {
    let repo = make_repo(&["a@A", "b@B"]);
    std::fs::write(repo.path().join("mods/b@B/meta.json"), r#"{"title": 1}"#).unwrap();
    sh(repo.path(), "git", &["commit", "-qam", "title is a number"]);
    let (f, sync) = synced_fx(repo.path());
    assert!(sync.sync_once(false).await.changed, "{:?}", sync.status());
    assert_eq!(f.st.store.read().mods().len(), 1);
}

#[tokio::test]
async fn the_build_script_does_not_see_server_secrets() {
    // SAFETY: unique variable name, only read by the child script below.
    unsafe { std::env::set_var("BMI_TEST_SECRET", "s3cr3t-token") };
    let repo = make_repo(&["a@A"]);
    let (f, sync) = synced_fx(repo.path());
    assert!(sync.sync_once(false).await.changed, "{:?}", sync.status());
    let m = f.st.store.read().get("a@A").unwrap();
    assert!(
        !m.description.contains("s3cr3t"),
        "child inherited the environment: {:?}",
        m.description
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlinks_in_the_index_repo_do_not_expose_host_files() {
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "TOP-SECRET-CONTENT").unwrap();
    let repo = make_repo(&["a@A"]);
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        repo.path().join("mods/a@A/description.md"),
    )
    .unwrap();
    sh(repo.path(), "git", &["add", "-A"]);
    sh(
        repo.path(),
        "git",
        &["commit", "-qm", "symlinked description"],
    );
    let (f, sync) = synced_fx(repo.path());
    assert!(sync.sync_once(false).await.changed, "{:?}", sync.status());
    let m = f.st.store.read().get("a@A").unwrap();
    assert!(!m.description.contains("TOP-SECRET"), "{:?}", m.description);
}
