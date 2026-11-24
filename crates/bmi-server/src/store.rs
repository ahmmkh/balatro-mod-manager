//! The served catalog: parsed `index.json` overlaid with per-mod lifecycle state. Lifecycle
//! (added / removed / hidden) is what lets `/mods/changed` tell clients to drop mods.
use crate::catalog::CatalogMod;
use crate::db::Db;
use anyhow::{Context, Result, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadStats {
    pub total: usize,
    pub added: usize,
    pub removed: usize,
    pub reappeared: usize,
}

/// Marker sent in `changed` responses so clients drop a mod they previously cached.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub id: String,
    pub deleted: bool,
    pub updated_at: String,
}

struct StateRow {
    updated_at: i64,
    deleted: bool,
    hidden: bool,
}

#[derive(Deserialize)]
struct IndexFile {
    mods: Vec<CatalogMod>,
}

pub struct Store {
    db: Db,
    mods: Arc<Vec<CatalogMod>>,
    by_id: HashMap<String, usize>,
    tombstones: Vec<Tombstone>,
    dir: Option<PathBuf>,
    loaded_at: i64,
}

impl Store {
    pub fn new(db: Db) -> Store {
        Store {
            db,
            mods: Arc::new(Vec::new()),
            by_id: HashMap::new(),
            tombstones: Vec::new(),
            dir: None,
            loaded_at: 0,
        }
    }

    pub fn mods(&self) -> Arc<Vec<CatalogMod>> {
        Arc::clone(&self.mods)
    }
    pub fn get(&self, id: &str) -> Option<CatalogMod> {
        self.by_id.get(id).map(|&i| self.mods[i].clone())
    }
    pub fn ready(&self) -> bool {
        !self.mods.is_empty()
    }
    pub fn dir(&self) -> Option<PathBuf> {
        self.dir.clone()
    }
    pub fn loaded_at(&self) -> i64 {
        self.loaded_at
    }

    /// Tombstones (removed or hidden mods) newer than `since`.
    pub fn tombstones_since(&self, since: &str) -> Vec<Tombstone> {
        match since.trim().parse::<i64>() {
            Ok(cutoff) => self
                .tombstones
                .iter()
                .filter(|t| t.updated_at.parse::<i64>().unwrap_or(0) > cutoff)
                .cloned()
                .collect(),
            Err(_) => self.tombstones.clone(),
        }
    }

    pub fn load(&mut self, dir: &Path, now: i64) -> Result<LoadStats> {
        let raw = std::fs::read_to_string(dir.join("index.json"))
            .with_context(|| format!("reading {}/index.json", dir.display()))?;
        let parsed: IndexFile = serde_json::from_str(&raw).context("parsing index.json")?;
        if parsed.mods.is_empty() {
            bail!("index.json has no mods");
        }

        let mut conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
        let mut state: HashMap<String, StateRow> = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT id, updated_at, deleted, hidden FROM mod_state")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    StateRow {
                        updated_at: r.get(1)?,
                        deleted: r.get::<_, i64>(2)? != 0,
                        hidden: r.get::<_, i64>(3)? != 0,
                    },
                ))
            })?;
            for row in rows {
                let (id, s) = row?;
                state.insert(id, s);
            }
        }

        let mut stats = LoadStats {
            total: parsed.mods.len(),
            ..Default::default()
        };
        let mut seen: HashSet<&str> = HashSet::new();
        let mut visible: Vec<CatalogMod> = Vec::with_capacity(parsed.mods.len());
        let tx = conn.transaction()?;
        {
            let mut upsert = tx.prepare(
                "INSERT INTO mod_state (id, updated_at, deleted, hidden) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET updated_at = ?2, deleted = ?3, hidden = ?4",
            )?;
            for m in &parsed.mods {
                seen.insert(m.id.as_str());
                let from_index: i64 = m.updated_at.parse().unwrap_or(0);
                let (updated, hidden) = match state.get(&m.id) {
                    None => {
                        // First sight: guarantee incremental clients pick it up.
                        stats.added += 1;
                        upsert.execute(params![m.id, now, 0, 0])?;
                        (now, false)
                    }
                    Some(prev) if prev.deleted => {
                        stats.reappeared += 1;
                        upsert.execute(params![m.id, now, 0, prev.hidden as i64])?;
                        (now, prev.hidden)
                    }
                    Some(prev) => {
                        let updated = prev.updated_at.max(from_index);
                        if updated != prev.updated_at {
                            upsert.execute(params![m.id, updated, 0, prev.hidden as i64])?;
                        }
                        (updated, prev.hidden)
                    }
                };
                if !hidden {
                    let mut v = m.clone();
                    v.updated_at = updated.to_string();
                    visible.push(v);
                }
            }
            for (id, row) in &state {
                if !seen.contains(id.as_str()) && !row.deleted {
                    upsert.execute(params![id, now, 1, row.hidden as i64])?;
                    stats.removed += 1;
                }
            }
        }
        tx.commit()?;
        let tombstones = Self::read_tombstones(&conn)?;
        drop(conn);

        self.by_id = visible
            .iter()
            .enumerate()
            .map(|(i, m)| (m.id.clone(), i))
            .collect();
        self.mods = Arc::new(visible);
        self.dir = Some(dir.to_path_buf());
        self.loaded_at = now;
        self.tombstones = tombstones;
        Ok(stats)
    }

    /// Hide or unhide a mod; returns false when the id was never seen.
    pub fn set_hidden(&mut self, id: &str, hidden: bool, now: i64) -> bool {
        let changed = {
            let conn = self.db.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "UPDATE mod_state SET hidden = ?1, updated_at = ?2 WHERE id = ?3",
                params![hidden as i64, now, id],
            )
            .unwrap_or(0)
        };
        if changed == 0 {
            return false;
        }
        if let Some(dir) = self.dir.clone() {
            let _ = self.load(&dir, now);
        }
        true
    }

    /// Flag mods whose download links are failing so clients and admins can see them.
    pub fn mark_unavailable(&mut self, ids: &HashSet<String>) {
        let mods = Arc::make_mut(&mut self.mods);
        for m in mods.iter_mut() {
            m.unavailable = ids.contains(&m.id);
        }
    }

    fn read_tombstones(conn: &rusqlite::Connection) -> rusqlite::Result<Vec<Tombstone>> {
        let mut stmt =
            conn.prepare("SELECT id, updated_at FROM mod_state WHERE deleted = 1 OR hidden = 1")?;
        stmt.query_map([], |r| {
            Ok(Tombstone {
                id: r.get(0)?,
                deleted: true,
                updated_at: r.get::<_, i64>(1)?.to_string(),
            })
        })?
        .collect()
    }
}
