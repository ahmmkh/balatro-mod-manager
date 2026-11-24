//! Periodic housekeeping.
use crate::db::Db;
use crate::timeutil::utc_day_of;
use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub const REPORT_RETENTION_DAYS: i64 = 30;

/// Delete reports older than the retention window, then give the space back to the filesystem.
pub fn prune_reports(reports: &Db, now: i64) -> Result<usize> {
    let conn = reports.lock().unwrap_or_else(|e| e.into_inner());
    let n = conn.execute(
        "DELETE FROM reports WHERE created_at < ?1",
        [now - REPORT_RETENTION_DAYS * 86_400],
    )?;
    // incremental_vacuum frees one page per step, so it must be stepped to completion.
    let mut vacuum = conn.prepare("PRAGMA incremental_vacuum")?;
    let mut rows = vacuum.query([])?;
    while rows.next()?.is_some() {}
    drop(rows);
    drop(vacuum);
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(n)
}

/// Write a consistent snapshot of the counts database (never the reports database) to
/// `<dir>/bmi-YYYY-MM-DD.sqlite` and keep only the newest `keep` snapshots.
pub fn backup(db: &Connection, dir: &Path, keep: usize, now: i64) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(format!("bmi-{}.sqlite", utc_day_of(now)));
    // VACUUM INTO refuses to overwrite, so build beside it and rename into place.
    let tmp = dir.join(".bmi-backup.tmp");
    let _ = std::fs::remove_file(&tmp);
    db.execute(
        "VACUUM INTO ?1",
        [tmp.to_str().context("backup path is not utf-8")?],
    )?;
    std::fs::rename(&tmp, &dest)?;
    let mut snaps: Vec<String> = std::fs::read_dir(dir)?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("bmi-") && n.ends_with(".sqlite"))
        .collect();
    snaps.sort();
    for old in snaps.iter().take(snaps.len().saturating_sub(keep)) {
        let _ = std::fs::remove_file(dir.join(old));
    }
    Ok(dest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    #[test]
    fn backup_snapshots_counts_db_and_keeps_newest_n() {
        let d = tempfile::tempdir().unwrap();
        let conn = db::open(&d.path().join("bmi.sqlite")).unwrap();
        conn.execute(
            "INSERT INTO mod_state (id, updated_at) VALUES ('a@A', 1)",
            [],
        )
        .unwrap();
        let out = d.path().join("backups");
        for day in 0..4 {
            backup(&conn, &out, 2, 1_794_571_200 + day * 86_400).unwrap();
        }
        let mut names: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["bmi-2026-11-15.sqlite", "bmi-2026-11-16.sqlite"]);
        let snap = Connection::open(out.join(&names[1])).unwrap();
        let n: i64 = snap
            .query_row("SELECT count(*) FROM mod_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        let has_reports: i64 = snap
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='reports'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_reports, 0);
        // same-day rerun replaces rather than fails
        backup(&conn, &out, 2, 1_794_571_200 + 3 * 86_400).unwrap();
    }
}
