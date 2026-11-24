//! Catalog types and pure operations: sorting, paging, change filtering.
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

pub const DEFAULT_LIMIT: usize = 200;
pub const MAX_LIMIT: usize = 500;

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CatalogMod {
    pub id: String,
    pub name: String,
    pub updated_at: String,
    #[serde(default)]
    pub description: String,
    /// Open map: unknown `meta.json` fields flow through untouched.
    #[serde(default)]
    pub meta: Map<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    /// Set by the server when the download link keeps failing health checks.
    #[serde(default, skip_serializing_if = "is_false")]
    pub unavailable: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Downloads {
    pub total: u64,
    pub today: u64,
    pub week: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    NameAsc,
    NameDesc,
    UpdatedAsc,
    UpdatedDesc,
    DownloadsAsc,
    DownloadsDesc,
}

pub fn parse_sort(value: &str) -> SortMode {
    match value {
        "name_desc" => SortMode::NameDesc,
        "updated_asc" => SortMode::UpdatedAsc,
        "updated_desc" => SortMode::UpdatedDesc,
        "downloads_asc" => SortMode::DownloadsAsc,
        "downloads_desc" => SortMode::DownloadsDesc,
        _ => SortMode::NameAsc,
    }
}

pub fn parse_limit(value: Option<&str>) -> usize {
    match value.and_then(|v| v.trim().parse::<usize>().ok()) {
        Some(n) if n >= 1 => n.min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    }
}

/// Cursor is the offset into the sorted list; `None` means malformed.
pub fn parse_cursor(value: Option<&str>) -> Option<usize> {
    match value {
        None | Some("") => Some(0),
        Some(v) if v.len() <= 9 && v.bytes().all(|b| b.is_ascii_digit()) => v.parse().ok(),
        Some(_) => None,
    }
}

fn updated(m: &CatalogMod) -> i64 {
    m.updated_at.parse().unwrap_or(0)
}

/// ASCII punctuation in CLDR root collation order (what `Intl.Collator("en")` gives the clients
/// and the TypeScript reference): whitespace < these symbols < digits < letters.
const PUNCT_ORDER: &str = " _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

/// Case-insensitive sort key for one character; no allocation so sorting stays cheap.
fn collate_key(c: char) -> (u8, u32) {
    let l = c.to_lowercase().next().unwrap_or(c);
    if let Some(i) = PUNCT_ORDER.find(l) {
        (0, i as u32)
    } else if l.is_ascii_digit() {
        (1, l as u32)
    } else {
        (2, l as u32)
    }
}

fn by_name(a: &CatalogMod, b: &CatalogMod) -> std::cmp::Ordering {
    a.name
        .chars()
        .map(collate_key)
        .cmp(b.name.chars().map(collate_key))
        .then_with(|| a.id.cmp(&b.id))
}

pub fn sort_mods(
    mods: &[CatalogMod],
    sort: SortMode,
    counts: &HashMap<String, Downloads>,
) -> Vec<CatalogMod> {
    let total = |m: &CatalogMod| counts.get(&m.id).map_or(0, |d| d.total);
    let mut out = mods.to_vec();
    match sort {
        SortMode::NameAsc => out.sort_by(by_name),
        SortMode::NameDesc => out.sort_by(|a, b| by_name(b, a)),
        SortMode::UpdatedAsc => {
            out.sort_by(|a, b| updated(a).cmp(&updated(b)).then_with(|| by_name(a, b)))
        }
        SortMode::UpdatedDesc => {
            out.sort_by(|a, b| updated(b).cmp(&updated(a)).then_with(|| by_name(a, b)))
        }
        SortMode::DownloadsAsc => {
            out.sort_by(|a, b| total(a).cmp(&total(b)).then_with(|| by_name(a, b)))
        }
        SortMode::DownloadsDesc => {
            out.sort_by(|a, b| total(b).cmp(&total(a)).then_with(|| by_name(a, b)))
        }
    }
    out
}

/// Mods strictly newer than `since`; a non-numeric `since` returns everything.
pub fn changed_since(mods: &[CatalogMod], since: &str) -> Vec<CatalogMod> {
    match since.trim().parse::<i64>() {
        Ok(cutoff) => mods
            .iter()
            .filter(|m| updated(m) > cutoff)
            .cloned()
            .collect(),
        Err(_) => mods.to_vec(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

pub fn paginate<T: Clone>(items: &[T], offset: usize, limit: usize) -> Page<T> {
    let start = offset.min(items.len());
    let end = offset.saturating_add(limit).min(items.len());
    let next = offset.saturating_add(limit);
    Page {
        items: items[start..end].to_vec(),
        next_cursor: (next < items.len()).then(|| next.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(id: &str, name: &str, updated: i64) -> CatalogMod {
        CatalogMod {
            id: id.into(),
            name: name.into(),
            updated_at: updated.to_string(),
            description: String::new(),
            meta: Map::new(),
            thumbnail_url: None,
            unavailable: false,
        }
    }
    fn mods() -> Vec<CatalogMod> {
        vec![
            m("b@B", "banana", 20),
            m("a@A", "Apple", 30),
            m("c@C", "cherry", 10),
        ]
    }
    fn counts() -> HashMap<String, Downloads> {
        HashMap::from([
            (
                "b@B".into(),
                Downloads {
                    total: 5,
                    today: 1,
                    week: 2,
                },
            ),
            (
                "c@C".into(),
                Downloads {
                    total: 9,
                    today: 0,
                    week: 3,
                },
            ),
        ])
    }
    fn ids(s: SortMode) -> Vec<String> {
        sort_mods(&mods(), s, &counts())
            .into_iter()
            .map(|m| m.id)
            .collect()
    }

    #[test]
    fn name_sort_is_case_insensitive() {
        assert_eq!(ids(SortMode::NameAsc), ["a@A", "b@B", "c@C"]);
        assert_eq!(ids(SortMode::NameDesc), ["c@C", "b@B", "a@A"]);
    }
    #[test]
    fn name_ties_break_by_id() {
        let v = vec![m("z@Z", "same", 1), m("a@A", "Same", 1)];
        let out = sort_mods(&v, SortMode::NameAsc, &HashMap::new());
        assert_eq!(out[0].id, "a@A");
    }
    #[test]
    fn punctuation_sorts_like_icu_root_collation() {
        let names = [
            "Balatro+",
            "Balatro: Blindside",
            "Balatro-Run",
            "2 For 1",
            "!Boo",
            "Zed",
            "alpha",
        ];
        let v: Vec<_> = names
            .iter()
            .enumerate()
            .map(|(i, n)| m(&format!("i{i}@x"), n, 1))
            .collect();
        let out: Vec<_> = sort_mods(&v, SortMode::NameAsc, &HashMap::new())
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(
            out,
            [
                "!Boo",
                "2 For 1",
                "alpha",
                "Balatro-Run",
                "Balatro: Blindside",
                "Balatro+",
                "Zed"
            ]
        );
    }
    #[test]
    fn updated_sorts() {
        assert_eq!(ids(SortMode::UpdatedDesc), ["a@A", "b@B", "c@C"]);
        assert_eq!(ids(SortMode::UpdatedAsc), ["c@C", "b@B", "a@A"]);
    }
    #[test]
    fn download_sorts_treat_missing_as_zero() {
        assert_eq!(ids(SortMode::DownloadsDesc), ["c@C", "b@B", "a@A"]);
        assert_eq!(ids(SortMode::DownloadsAsc), ["a@A", "b@B", "c@C"]);
    }
    #[test]
    fn sort_does_not_mutate_input() {
        let v = mods();
        let _ = sort_mods(&v, SortMode::NameDesc, &counts());
        assert_eq!(v[0].id, "b@B");
    }
    #[test]
    fn paginate_cursor_until_end() {
        let v = [1, 2, 3, 4, 5];
        assert_eq!(
            paginate(&v, 0, 2),
            Page {
                items: vec![1, 2],
                next_cursor: Some("2".into())
            }
        );
        assert_eq!(
            paginate(&v, 4, 2),
            Page {
                items: vec![5],
                next_cursor: None
            }
        );
        assert_eq!(paginate(&[1, 2], 0, 2).next_cursor, None);
        assert!(paginate(&v, 99, 2).items.is_empty());
    }
    #[test]
    fn param_parsing() {
        assert_eq!(parse_cursor(None), Some(0));
        assert_eq!(parse_cursor(Some("12")), Some(12));
        assert_eq!(parse_cursor(Some("-1")), None);
        assert_eq!(parse_cursor(Some("abc")), None);
        assert_eq!(parse_cursor(Some("1234567890")), None);
        assert_eq!(parse_limit(Some("9999")), 500);
        assert_eq!(parse_limit(Some("0")), 200);
        assert_eq!(parse_limit(None), 200);
        assert_eq!(parse_sort("nope"), SortMode::NameAsc);
        assert_eq!(parse_sort("downloads_desc"), SortMode::DownloadsDesc);
    }
    #[test]
    fn changed_since_is_strict() {
        let ids: Vec<_> = changed_since(&mods(), "20")
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, ["a@A"]);
        assert_eq!(changed_since(&mods(), "abc").len(), 3);
    }
    #[test]
    fn serde_skips_optional_fields_and_keeps_unknown_meta() {
        let mut x = m("a@A", "A", 1);
        x.meta
            .insert("newField".into(), serde_json::json!({"k": 1}));
        let v = serde_json::to_value(&x).unwrap();
        assert!(v.get("thumbnail_url").is_none() && v.get("unavailable").is_none());
        assert_eq!(v["meta"]["newField"]["k"], 1);
        x.unavailable = true;
        assert_eq!(serde_json::to_value(&x).unwrap()["unavailable"], true);
    }
}
