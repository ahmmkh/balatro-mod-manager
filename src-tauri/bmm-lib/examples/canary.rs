//! "Does the index actually install?" canary.
//!
//! Installs a sample of the catalog's mods with the real installer into a throwaway config
//! directory, verifies each looks like a mod, uninstalls it and reports failures by class. It
//! reads each mod's `meta.downloadURL` from the catalog and **never** calls `POST …/download`,
//! so it cannot inflate the public download counters.
//!
//!   cargo run -p bmm-lib --example canary -- \
//!       --catalog <https://bmi.khatab.me | path/to/index.json> --sample <N|all> --seed <u64> --out summary.json
//!
//! Exit code 0 unless the harness itself fails or more than half of the sample fails (that
//! pattern means an outage or a systematic bug, not a few bad mods).
use bmm_lib::errors::AppError;
use bmm_lib::installer::{install_mod, uninstall_mod};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const USER_AGENT: &str = "bmm-canary/1.0 (+https://github.com/ahmmkh/balatro-mod-manager)";
const MAX_ARCHIVE_BYTES: u64 = 100 * 1024 * 1024;
const CONCURRENCY: usize = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum FailureClass {
    Network,
    NotAnArchive,
    UnsafePath,
    WindowsIllegalName,
    CaseCollision,
    TooLong,
    Empty,
    TooLarge,
    Other,
}

pub fn classify(err: &AppError) -> FailureClass {
    match err {
        AppError::NetworkRequest { .. } | AppError::Network(_) => FailureClass::Network,
        AppError::ArchiveTooLarge { .. } => FailureClass::TooLarge,
        AppError::PathValidation { reason, .. } => {
            if reason.contains("differs only by letter case") {
                FailureClass::CaseCollision
            } else if reason.contains("path too long") {
                FailureClass::TooLong
            } else if reason.contains("Windows") || reason.contains("ends with a dot or space") {
                FailureClass::WindowsIllegalName
            } else {
                FailureClass::UnsafePath
            }
        }
        AppError::InvalidState(m) if m.contains("Unsupported or unknown archive type") => {
            FailureClass::NotAnArchive
        }
        AppError::InvalidState(m) if m.to_lowercase().contains("empty") => FailureClass::Empty,
        AppError::FileWrite { source, .. } if source.contains("Invalid zip archive") => {
            FailureClass::NotAnArchive
        }
        _ => FailureClass::Other,
    }
}

/// Deterministic sample of `n` ids: same seed, same ids; `n >= len` returns everything in
/// sorted order; never repeats an id.
pub fn sample_ids(ids: &[String], n: usize, seed: u64) -> Vec<String> {
    let mut all: Vec<String> = ids.to_vec();
    all.sort();
    all.dedup();
    if n >= all.len() {
        return all;
    }
    // splitmix64-driven Fisher–Yates; no external RNG so the sample is stable across versions.
    let mut state = seed;
    let mut next = || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for i in (1..all.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        all.swap(i, j);
    }
    all.truncate(n);
    all
}

#[derive(Clone, Debug, Serialize)]
pub struct ModResult {
    pub id: String,
    pub url: String,
    pub failure: Option<FailureClass>,
    pub detail: String,
}

/// Markdown report: totals, then failures grouped by class.
pub fn render_summary(results: &[ModResult]) -> String {
    let failed: Vec<&ModResult> = results.iter().filter(|r| r.failure.is_some()).collect();
    let mut out = format!(
        "## Install canary\n\n{} sampled, {} installed, {} failed\n",
        results.len(),
        results.len() - failed.len(),
        failed.len()
    );
    if failed.is_empty() {
        return out;
    }
    let mut by_class: BTreeMap<FailureClass, Vec<&ModResult>> = BTreeMap::new();
    for r in failed {
        by_class
            .entry(r.failure.expect("filtered"))
            .or_default()
            .push(r);
    }
    out.push_str("\n| Class | Mod | Detail |\n|---|---|---|\n");
    for (class, rs) in by_class {
        for r in rs {
            out.push_str(&format!(
                "| {:?} | `{}` | {} |\n",
                class,
                r.id,
                r.detail.replace('|', "\\|").replace('\n', " ")
            ));
        }
    }
    out
}

/// A folder looks like a mod when it holds a `.json` manifest or a `.lua` file within 3 levels.
fn looks_like_a_mod(dir: &Path) -> bool {
    fn walk(dir: &Path, depth: usize) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return false;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                if depth < 3 && walk(&p, depth + 1) {
                    return true;
                }
            } else if matches!(p.extension().and_then(|x| x.to_str()), Some("json" | "lua")) {
                return true;
            }
        }
        false
    }
    walk(dir, 0)
}

#[derive(Debug)]
struct CatalogEntry {
    id: String,
    url: String,
}

fn entries_from_items(items: &[serde_json::Value]) -> Vec<CatalogEntry> {
    items
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let url = m.get("meta")?.get("downloadURL")?.as_str()?.to_string();
            (!url.is_empty()).then_some(CatalogEntry { id, url })
        })
        .collect()
}

async fn load_catalog(src: &str, client: &reqwest::Client) -> Result<Vec<CatalogEntry>, String> {
    if src.starts_with("http://") || src.starts_with("https://") {
        let base = src.trim_end_matches('/');
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut url = format!("{base}/mods?limit=500");
            if let Some(c) = &cursor {
                url.push_str(&format!("&cursor={c}"));
            }
            let page: serde_json::Value = client
                .get(&url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("{url}: {e}"))?
                .json()
                .await
                .map_err(|e| format!("{url}: {e}"))?;
            let items = page["items"].as_array().cloned().unwrap_or_default();
            out.extend(entries_from_items(&items));
            match page["next_cursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        Ok(out)
    } else {
        let mut p = PathBuf::from(src);
        if p.is_dir() {
            p = p.join("index.json");
        }
        let raw = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        Ok(entries_from_items(
            v["mods"].as_array().map(Vec::as_slice).unwrap_or_default(),
        ))
    }
}

/// `Content-Length` of the archive, when the server reports one. A GitHub token (if present) is
/// only used for this probe: `install_mod` itself downloads anonymously.
async fn content_length(client: &reqwest::Client, url: &str) -> Option<u64> {
    let mut req = client.head(url);
    if url.contains("github.com")
        && let Ok(t) = std::env::var("GITHUB_TOKEN")
    {
        req = req.bearer_auth(t);
    }
    req.send().await.ok()?.content_length()
}

fn folder_name(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn check_one(client: reqwest::Client, e: CatalogEntry, mods_root: PathBuf) -> ModResult {
    let fail = |class, detail: String| ModResult {
        id: e.id.clone(),
        url: e.url.clone(),
        failure: Some(class),
        detail,
    };
    if content_length(&client, &e.url)
        .await
        .is_some_and(|n| n > MAX_ARCHIVE_BYTES)
    {
        return fail(
            FailureClass::TooLarge,
            "archive larger than 100 MB, skipped".into(),
        );
    }
    let folder = folder_name(&e.id);
    let result = tokio::time::timeout(
        Duration::from_secs(180),
        install_mod(e.url.clone(), Some(folder.clone())),
    )
    .await;
    let path = match result {
        Err(_) => return fail(FailureClass::Network, "timed out after 180 s".into()),
        Ok(Err(err)) => return fail(classify(&err), err.to_string()),
        Ok(Ok(p)) => p,
    };
    let verdict = if looks_like_a_mod(&path) {
        None
    } else {
        Some(fail(
            FailureClass::Empty,
            "installed folder has no .json manifest or .lua file".into(),
        ))
    };
    let _ = uninstall_mod(mods_root.join(folder)).await;
    verdict.unwrap_or(ModResult {
        id: e.id,
        url: e.url,
        failure: None,
        detail: String::new(),
    })
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    match run(&args).await {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("canary harness error: {e}");
            std::process::exit(2);
        }
    }
}

async fn run(args: &[String]) -> Result<i32, String> {
    let catalog = arg(args, "--catalog").ok_or("--catalog is required")?;
    let sample = arg(args, "--sample").unwrap_or_else(|| "40".into());
    let seed: u64 = arg(args, "--seed")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let out = arg(args, "--out");

    let sandbox = tempfile::tempdir().map_err(|e| e.to_string())?;
    // SAFETY: set once at startup before any other thread reads the environment.
    unsafe { std::env::set_var("BMM_CONFIG_DIR", sandbox.path()) };
    let mods_root = sandbox.path().join("Balatro").join("Mods");

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let entries = load_catalog(&catalog, &client).await?;
    if entries.is_empty() {
        return Err("catalog has no mods with a downloadURL".into());
    }
    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    let n = if sample == "all" {
        usize::MAX
    } else {
        sample
            .parse()
            .map_err(|_| "--sample must be a number or 'all'")?
    };
    let chosen: std::collections::HashSet<String> = sample_ids(&ids, n, seed).into_iter().collect();
    let todo: Vec<CatalogEntry> = entries
        .into_iter()
        .filter(|e| chosen.contains(&e.id))
        .collect();
    eprintln!("canary: installing {} mods (seed {seed})", todo.len());

    let sem = Arc::new(tokio::sync::Semaphore::new(CONCURRENCY));
    let mut tasks = Vec::new();
    for e in todo {
        let (permit, client, root) = (sem.clone(), client.clone(), mods_root.clone());
        tasks.push(tokio::spawn(async move {
            let _p = permit.acquire_owned().await.expect("semaphore open");
            check_one(client, e, root).await
        }));
    }
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.map_err(|e| e.to_string())?);
    }
    results.sort_by(|a, b| a.id.cmp(&b.id));

    let md = render_summary(&results);
    println!("{md}");
    if let Some(out) = out {
        let json = serde_json::to_string_pretty(&results).map_err(|e| e.to_string())?;
        std::fs::write(&out, json).map_err(|e| format!("{out}: {e}"))?;
    }
    let failed = results.iter().filter(|r| r.failure.is_some()).count();
    Ok(if failed * 2 > results.len() { 1 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pv(reason: &str) -> AppError {
        AppError::PathValidation {
            path: PathBuf::from("x"),
            reason: format!("unsafe archive entry: {reason}"),
        }
    }

    #[test]
    fn classify_covers_every_class() {
        let net = AppError::NetworkRequest {
            url: "u".into(),
            source: "s".into(),
        };
        let table: Vec<(AppError, FailureClass)> = vec![
            (net, FailureClass::Network),
            (
                AppError::InvalidState(
                    "Unsupported or unknown archive type (supported: .zip)".into(),
                ),
                FailureClass::NotAnArchive,
            ),
            (pv("parent directory reference"), FailureClass::UnsafePath),
            (pv("absolute path"), FailureClass::UnsafePath),
            (
                pv("reserved Windows device name"),
                FailureClass::WindowsIllegalName,
            ),
            (
                pv("character not allowed in Windows file names"),
                FailureClass::WindowsIllegalName,
            ),
            (
                pv("name ends with a dot or space"),
                FailureClass::WindowsIllegalName,
            ),
            (
                pv("differs only by letter case from another entry"),
                FailureClass::CaseCollision,
            ),
            (pv("path too long"), FailureClass::TooLong),
            (
                AppError::InvalidState("Empty zip archive".into()),
                FailureClass::Empty,
            ),
            (
                AppError::ArchiveTooLarge { reason: "r".into() },
                FailureClass::TooLarge,
            ),
            (AppError::Unknown("?".into()), FailureClass::Other),
        ];
        for (err, want) in table {
            assert_eq!(classify(&err), want, "{err:?}");
        }
    }

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("a{i}@M{i}")).collect()
    }

    #[test]
    fn sampling_is_deterministic_and_seed_dependent() {
        let all = ids(40);
        let a = sample_ids(&all, 3, 7);
        assert_eq!(a, sample_ids(&all, 3, 7));
        assert_eq!(a.len(), 3);
        assert_ne!(a, sample_ids(&all, 3, 8));
        let set: std::collections::HashSet<_> = a.iter().collect();
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn oversized_sample_returns_everything_in_stable_order() {
        let mut all = ids(5);
        all.reverse();
        let got = sample_ids(&all, 99, 1);
        assert_eq!(got.len(), 5);
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted);
        assert_eq!(sample_ids(&all, 5, 1), got);
    }

    #[test]
    fn sampling_never_repeats_even_with_duplicate_input() {
        let mut all = ids(10);
        all.extend(ids(10));
        let got = sample_ids(&all, 10, 3);
        let set: std::collections::HashSet<_> = got.iter().collect();
        assert_eq!(set.len(), got.len());
    }

    #[test]
    fn summary_groups_failures_by_class() {
        let ok = ModResult {
            id: "a@Ok".into(),
            url: "u".into(),
            failure: None,
            detail: String::new(),
        };
        let f1 = ModResult {
            id: "b@Net".into(),
            url: "u".into(),
            failure: Some(FailureClass::Network),
            detail: "HTTP 404".into(),
        };
        let f2 = ModResult {
            id: "c@Bad".into(),
            url: "u".into(),
            failure: Some(FailureClass::WindowsIllegalName),
            detail: "a|b".into(),
        };
        let md = render_summary(&[ok, f1, f2]);
        let expected = "## Install canary\n\n3 sampled, 1 installed, 2 failed\n\n| Class | Mod | Detail |\n|---|---|---|\n| Network | `b@Net` | HTTP 404 |\n| WindowsIllegalName | `c@Bad` | a\\|b |\n";
        assert_eq!(md, expected);
        assert!(render_summary(&[]).contains("0 sampled"));
    }

    #[test]
    fn folder_names_are_filesystem_safe() {
        assert_eq!(folder_name("a@Mod!Name"), "a_Mod_Name");
    }
}
