//! Periodic housekeeping.
use crate::db::Db;
use anyhow::Result;

pub const REPORT_RETENTION_DAYS: i64 = 30;

/// Delete reports older than the retention window, then give the space back to the filesystem.
pub fn prune_reports(reports: &Db, now: i64) -> Result<usize> {
    let conn = reports.lock().unwrap_or_else(|e| e.into_inner());
    let n = conn.execute(
        "DELETE FROM reports WHERE created_at < ?1",
        [now - REPORT_RETENTION_DAYS * 86_400],
    )?;
    conn.execute_batch("PRAGMA incremental_vacuum;")?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(n)
}
