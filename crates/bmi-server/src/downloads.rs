//! Download counters. Increments apply to memory immediately (reads are always fresh) and are
//! written to SQLite in batches. De-duplicated to one count per (visitor, mod, UTC day); the
//! visitor key is a salted SHA-256 and no IP is ever stored.
use crate::catalog::Downloads;
use crate::db::Db;
use crate::timeutil::days_ago;
use anyhow::Result;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

const KEEP_DAILY_DAYS: i64 = 35;
const SNAPSHOT_TTL: Duration = Duration::from_secs(2);

pub type Today = Arc<dyn Fn() -> String + Send + Sync>;

type Snapshot = Arc<HashMap<String, Downloads>>;

#[derive(Default)]
struct Inner {
    totals: HashMap<String, u64>,
    daily: HashMap<String, HashMap<String, u64>>,
    seen: HashSet<String>,
    seen_day: String,
    pending_keys: Vec<(String, String)>,
    pending_deltas: HashMap<(String, String), u64>,
    snapshot: Option<(Instant, String, Snapshot)>,
    last_prune_day: String,
}

pub struct DownloadCounter {
    db: Db,
    secret: String,
    today: Today,
    inner: Mutex<Inner>,
}

impl DownloadCounter {
    pub fn new(db: Db, secret: String, today: Today) -> Result<Self> {
        let mut inner = Inner::default();
        {
            let conn = db.lock().unwrap_or_else(|e| e.into_inner());
            let mut st = conn.prepare("SELECT mod_id, total FROM dl_total")?;
            for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
                let (id, n) = r?;
                inner.totals.insert(id, n.max(0) as u64);
            }
            let day = today();
            let mut st = conn.prepare("SELECT mod_id, day, n FROM dl_daily WHERE day >= ?1")?;
            let rows = st.query_map([days_ago(&day, 7)], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?;
            for r in rows {
                let (id, d, n) = r?;
                inner
                    .daily
                    .entry(id)
                    .or_default()
                    .insert(d, n.max(0) as u64);
            }
            let mut st = conn.prepare("SELECT key FROM dl_seen WHERE day = ?1")?;
            for r in st.query_map([&day], |r| r.get::<_, String>(0))? {
                inner.seen.insert(r?);
            }
            inner.seen_day = day;
        }
        Ok(Self {
            db,
            secret,
            today,
            inner: Mutex::new(inner),
        })
    }

    pub fn hash(&self, parts: &[&str]) -> String {
        let mut h = Sha256::new();
        h.update(self.secret.as_bytes());
        for p in parts {
            h.update(b"|");
            h.update(p.as_bytes());
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Returns true if this call was counted (false = same visitor already counted today).
    pub fn record(&self, mod_id: &str, ip: &str) -> bool {
        let day = (self.today)();
        let key = self.hash(&[&day, ip, mod_id]);
        let mut g = self.inner.lock();
        if g.seen_day != day {
            g.seen.clear(); // keys embed the day, so yesterday's can never match again
            g.seen_day = day.clone();
        }
        if !g.seen.insert(key.clone()) {
            return false;
        }
        g.pending_keys.push((key, day.clone()));
        *g.totals.entry(mod_id.to_string()).or_insert(0) += 1;
        *g.daily
            .entry(mod_id.to_string())
            .or_default()
            .entry(day.clone())
            .or_insert(0) += 1;
        *g.pending_deltas
            .entry((mod_id.to_string(), day))
            .or_insert(0) += 1;
        g.snapshot = None;
        true
    }

    pub fn snapshot(&self) -> Snapshot {
        let today = (self.today)();
        let mut g = self.inner.lock();
        if let Some((at, day, snap)) = &g.snapshot
            && *day == today
            && at.elapsed() < SNAPSHOT_TTL
        {
            return Arc::clone(snap);
        }
        let week_start = days_ago(&today, 6);
        let mut out = HashMap::with_capacity(g.totals.len());
        for (id, total) in &g.totals {
            let (mut week, mut day_n) = (0, 0);
            if let Some(b) = g.daily.get(id) {
                week = b
                    .iter()
                    .filter(|(d, _)| d.as_str() >= week_start.as_str())
                    .map(|(_, n)| n)
                    .sum();
                day_n = b.get(&today).copied().unwrap_or(0);
            }
            out.insert(
                id.clone(),
                Downloads {
                    total: *total,
                    today: day_n,
                    week,
                },
            );
        }
        let snap = Arc::new(out);
        g.snapshot = Some((Instant::now(), today, Arc::clone(&snap)));
        snap
    }

    pub fn get(&self, mod_id: &str) -> Downloads {
        self.snapshot().get(mod_id).copied().unwrap_or_default()
    }

    /// Persist pending increments in one transaction. Safe to call repeatedly; on error the
    /// batch is merged back so no counts are lost.
    pub fn flush(&self) -> Result<usize> {
        let (keys, deltas) = {
            let mut g = self.inner.lock();
            if g.pending_keys.is_empty() {
                return Ok(0);
            }
            (
                std::mem::take(&mut g.pending_keys),
                std::mem::take(&mut g.pending_deltas),
            )
        };
        if let Err(e) = self.write(&keys, &deltas) {
            let mut g = self.inner.lock();
            let mut merged = keys;
            merged.append(&mut g.pending_keys);
            g.pending_keys = merged;
            for (k, n) in deltas {
                *g.pending_deltas.entry(k).or_insert(0) += n;
            }
            return Err(e);
        }
        self.prune_if_new_day();
        Ok(keys.len())
    }

    fn write(
        &self,
        keys: &[(String, String)],
        deltas: &HashMap<(String, String), u64>,
    ) -> Result<()> {
        let mut conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let tx = conn.transaction()?;
        {
            let mut seen =
                tx.prepare("INSERT OR IGNORE INTO dl_seen (key, day) VALUES (?1, ?2)")?;
            for (k, d) in keys {
                seen.execute((k, d))?;
            }
            let mut total = tx.prepare(
                "INSERT INTO dl_total (mod_id, total) VALUES (?1, ?2) ON CONFLICT(mod_id) DO UPDATE SET total = total + ?2",
            )?;
            let mut daily = tx.prepare(
                "INSERT INTO dl_daily (mod_id, day, n) VALUES (?1, ?2, ?3) ON CONFLICT(mod_id, day) DO UPDATE SET n = n + ?3",
            )?;
            for ((id, day), n) in deltas {
                total.execute((id, *n as i64))?;
                daily.execute((id, day, *n as i64))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn prune_if_new_day(&self) {
        let today = (self.today)();
        {
            let mut g = self.inner.lock();
            if g.last_prune_day == today {
                return;
            }
            g.last_prune_day = today.clone();
        }
        let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute("DELETE FROM dl_seen WHERE day < ?1", [&today]);
        let _ = conn.execute(
            "DELETE FROM dl_daily WHERE day < ?1",
            [days_ago(&today, KEEP_DAILY_DAYS)],
        );
    }
}
