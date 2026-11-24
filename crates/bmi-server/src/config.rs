//! Environment-driven configuration. Defaults match the TypeScript reference.
use anyhow::{Result, bail};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub data_dir: PathBuf,
    /// Directory holding `index.json` + `thumbnails/` (built by `scripts/build_index.py`).
    pub catalog_dir: PathBuf,
    /// Git remote of the index repo. If unset the server serves whatever is in `catalog_dir`.
    pub index_repo_url: Option<String>,
    pub index_branch: String,
    pub index_dir: PathBuf,
    pub sync_interval_sec: u64,
    pub link_check_interval_sec: u64,
    pub admin_token: Option<String>,
    /// Trust CF-Connecting-IP / X-Forwarded-For (only behind Cloudflare Tunnel / a trusted proxy).
    pub trust_proxy: bool,
    pub discord_webhook_url: Option<String>,
    pub reports_per_day: u64,
    pub reports_max_bytes: u64,
}

impl Config {
    pub fn from_env(env: &HashMap<String, String>) -> Result<Config> {
        let get = |k: &str| env.get(k).map(|v| v.trim()).filter(|v| !v.is_empty());
        let int = |k: &str, d: u64| {
            get(k)
                .and_then(|v| v.parse::<u64>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(d)
        };
        let data_dir = PathBuf::from(env.get("DATA_DIR").map_or("./data", |v| v.as_str()));
        let admin_token = get("ADMIN_TOKEN").map(str::to_string);
        if let Some(t) = &admin_token
            && t.chars().count() < 24
        {
            bail!("ADMIN_TOKEN must be at least 24 characters");
        }
        Ok(Config {
            port: int("PORT", 8080).min(u16::MAX as u64) as u16,
            catalog_dir: env
                .get("CATALOG_DIR")
                .map_or_else(|| data_dir.join("catalog"), PathBuf::from),
            index_repo_url: get("INDEX_REPO_URL").map(str::to_string),
            index_branch: env
                .get("INDEX_BRANCH")
                .cloned()
                .unwrap_or_else(|| "main".into()),
            index_dir: env
                .get("INDEX_DIR")
                .map_or_else(|| data_dir.join("index-repo"), PathBuf::from),
            sync_interval_sec: int("SYNC_INTERVAL_SEC", 900),
            link_check_interval_sec: int("LINK_CHECK_INTERVAL_SEC", 6 * 3600),
            admin_token,
            trust_proxy: env.get("TRUST_PROXY").is_some_and(|v| v == "1"),
            discord_webhook_url: get("DISCORD_WEBHOOK_URL").map(str::to_string),
            reports_per_day: int("REPORTS_PER_DAY", 300),
            reports_max_bytes: int("REPORTS_MAX_BYTES", 1 << 30),
            data_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults() {
        let c = Config::from_env(&env(&[])).unwrap();
        assert_eq!(c.port, 8080);
        assert_eq!(c.catalog_dir, PathBuf::from("./data/catalog"));
        assert_eq!(c.sync_interval_sec, 900);
        assert_eq!(c.reports_per_day, 300);
        assert_eq!(c.reports_max_bytes, 1 << 30);
        assert!(c.admin_token.is_none() && !c.trust_proxy);
    }
    #[test]
    fn admin_token_must_be_long_enough() {
        assert!(Config::from_env(&env(&[("ADMIN_TOKEN", "short")])).is_err());
        assert!(Config::from_env(&env(&[("ADMIN_TOKEN", &"a".repeat(24))])).is_ok());
        assert!(
            Config::from_env(&env(&[("ADMIN_TOKEN", "  ")]))
                .unwrap()
                .admin_token
                .is_none()
        );
    }
    #[test]
    fn overrides() {
        let c = Config::from_env(&env(&[
            ("PORT", "9"),
            ("TRUST_PROXY", "1"),
            ("DATA_DIR", "/d"),
            ("REPORTS_PER_DAY", "3"),
        ]))
        .unwrap();
        assert_eq!((c.port, c.trust_proxy, c.reports_per_day), (9, true, 3));
        assert_eq!(c.index_dir, PathBuf::from("/d/index-repo"));
    }
}
