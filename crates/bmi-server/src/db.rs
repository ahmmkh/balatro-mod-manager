//! SQLite schemas. Counts/lifecycle live in `bmi.sqlite`; reports live in their own file so a
//! report flood can never bloat the database that backups and counters depend on.
use rusqlite::Connection;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub type Db = Arc<Mutex<Connection>>;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Lifecycle of every mod id we've ever served. Lets /mods/changed emit tombstones for removed mods.
CREATE TABLE IF NOT EXISTS mod_state (
  id         TEXT PRIMARY KEY,
  updated_at INTEGER NOT NULL,
  deleted    INTEGER NOT NULL DEFAULT 0,
  hidden     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS dl_total (mod_id TEXT PRIMARY KEY, total INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS dl_daily (
  mod_id TEXT NOT NULL, day TEXT NOT NULL, n INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (mod_id, day)
);
-- One counted download per (visitor, mod, UTC day). key = salted SHA-256; no IPs stored.
CREATE TABLE IF NOT EXISTS dl_seen (key TEXT PRIMARY KEY, day TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS dl_seen_day ON dl_seen(day);

CREATE TABLE IF NOT EXISTS link_health (
  mod_id     TEXT PRIMARY KEY,
  url        TEXT NOT NULL,
  status     INTEGER NOT NULL,
  fails      INTEGER NOT NULL DEFAULT 0,
  checked_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS banned (hash TEXT PRIMARY KEY, reason TEXT, created_at INTEGER NOT NULL);
";

const REPORTS_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS reports (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  created_at INTEGER NOT NULL,
  title TEXT NOT NULL, description TEXT NOT NULL,
  mm_version TEXT NOT NULL, os TEXT NOT NULL, arch TEXT NOT NULL,
  cpu TEXT NOT NULL, gpu TEXT NOT NULL, ram TEXT NOT NULL,
  log_filename TEXT NOT NULL,
  log_gz BLOB NOT NULL,
  log_bytes INTEGER NOT NULL
);
";

/// Open (creating if needed) the counts/lifecycle database.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
    )?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

/// Open (creating if needed) the reports database. `auto_vacuum` must be set before the first
/// table exists, so it comes first.
pub fn open_reports(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL; PRAGMA busy_timeout = 5000;",
    )?;
    conn.execute_batch(REPORTS_SCHEMA)?;
    Ok(conn)
}

/// Persistent random secret used to salt visitor hashes; created on first run.
pub fn secret(conn: &Connection) -> rusqlite::Result<String> {
    let existing: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'salt'", [], |r| {
            r.get(0)
        })
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    if let Some(v) = existing {
        return Ok(v);
    }
    let bytes: [u8; 32] = rand::random();
    let value: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('salt', ?1)",
        [&value],
    )?;
    Ok(value)
}

pub fn now_sec() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}
