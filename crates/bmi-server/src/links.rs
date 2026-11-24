//! Download-link health checks. SSRF-guarded: only public https hosts are ever contacted.
use crate::db::{Db, now_sec};
use crate::store::Store;
use async_trait::async_trait;
use futures_util::StreamExt;
use parking_lot::RwLock;
use reqwest::Url;
use reqwest::redirect::Policy;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

pub const DEAD_AFTER_FAILS: i64 = 3;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkSummary {
    pub checked: usize,
    pub dead: usize,
}

/// Network access for probes; tests substitute a stub.
#[async_trait]
pub trait Fetch: Send + Sync {
    /// HTTP status of a HEAD request, `None` on a network error or timeout.
    async fn head(&self, url: &str) -> Option<u16>;
    /// HTTP status of `GET` with `Range: bytes=0-0`.
    async fn ranged_get(&self, url: &str) -> Option<u16>;
}

pub struct ReqwestFetch(reqwest::Client);

impl ReqwestFetch {
    pub fn new() -> reqwest::Result<Self> {
        // Redirect targets are re-checked so a public URL cannot bounce us into the cluster.
        let policy = Policy::custom(|a| {
            if a.previous().len() >= 5 {
                a.error("too many redirects")
            } else if is_safe_url(a.url().as_str()) {
                a.follow()
            } else {
                a.error("unsafe redirect")
            }
        });
        Ok(Self(
            reqwest::Client::builder()
                // Checks the address a name actually resolves to (covers redirects and DNS
                // rebinding), not just the text of the URL.
                .dns_resolver(Arc::new(PublicOnlyResolver))
                .timeout(TIMEOUT)
                .redirect(policy)
                .user_agent("bmi-link-check")
                .build()?,
        ))
    }
}

#[async_trait]
impl Fetch for ReqwestFetch {
    async fn head(&self, url: &str) -> Option<u16> {
        self.0
            .head(url)
            .send()
            .await
            .ok()
            .map(|r| r.status().as_u16())
    }
    async fn ranged_get(&self, url: &str) -> Option<u16> {
        self.0
            .get(url)
            .header("range", "bytes=0-0")
            .send()
            .await
            .ok()
            .map(|r| r.status().as_u16())
    }
}

/// Whether an address is globally routable (not loopback, private, link-local, CGNAT,
/// benchmarking, documentation, multicast, reserved, unique-local ...).
pub fn is_global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => is_global_v4(v),
        IpAddr::V6(v) => match v.to_ipv4_mapped() {
            Some(m) => is_global_v4(m),
            None => is_global_v6(v),
        },
    }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || a == 0
        || (a == 100 && (64..128).contains(&b)) // 100.64/10 CGNAT (also Tailscale)
        || (a == 192 && b == 0 && c == 0) // 192.0.0/24
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19)) // 198.18/15 benchmarking
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 240)
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // Global unicast is 2000::/3; exclude the documentation range 2001:db8::/32.
    (s[0] & 0xe000) == 0x2000 && !(s[0] == 0x2001 && s[1] == 0x0db8)
}

/// Resolves names but drops every address that is not globally routable; fails when none is left.
struct PublicOnlyResolver;

impl reqwest::dns::Resolve for PublicOnlyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0))
                .await?
                .filter(|a| is_global_ip(a.ip()))
                .collect();
            if addrs.is_empty() {
                return Err("name resolves only to non-public addresses".into());
            }
            Ok(Box::new(addrs.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Only public https URLs on the default port.
pub fn is_safe_url(raw: &str) -> bool {
    let Ok(u) = Url::parse(raw) else { return false };
    if u.scheme() != "https" || u.port().is_some_and(|p| p != 443) {
        return false;
    }
    match u.host() {
        Some(url::Host::Domain(d)) => {
            let d = d.trim_end_matches('.').to_ascii_lowercase();
            !(d.is_empty()
                || d == "localhost"
                || d.ends_with(".local")
                || d.ends_with(".internal")
                || d.ends_with(".localhost")
                || !d.contains('.'))
        }
        Some(url::Host::Ipv4(ip)) => is_global_v4(ip),
        Some(url::Host::Ipv6(ip)) => is_global_ip(IpAddr::V6(ip)),
        None => false,
    }
}

/// HEAD, then a ranged GET when HEAD is refused (403/405/501). 0 on any error.
pub async fn probe(f: &dyn Fetch, url: &str) -> u16 {
    match f.head(url).await {
        None => 0,
        Some(s) if ![403, 405, 501].contains(&s) => s,
        Some(_) => f.ranged_get(url).await.unwrap_or(0),
    }
}

pub fn dead_ids(db: &Db) -> HashSet<String> {
    let conn = db.lock().unwrap_or_else(|e| e.into_inner());
    let Ok(mut st) = conn.prepare("SELECT mod_id FROM link_health WHERE fails >= ?1") else {
        return HashSet::new();
    };
    st.query_map([DEAD_AFTER_FAILS], |r| r.get::<_, String>(0))
        .map(|rows| rows.flatten().collect())
        .unwrap_or_default()
}

pub async fn check_all(
    db: &Db,
    store: &RwLock<Store>,
    fetch: &dyn Fetch,
    concurrency: usize,
) -> LinkSummary {
    let jobs: Vec<(String, String)> = store
        .read()
        .mods()
        .iter()
        .filter_map(|m| {
            let url = m
                .meta
                .get("downloadURL")?
                .as_str()
                .filter(|u| !u.is_empty())?;
            Some((m.id.clone(), url.to_string()))
        })
        .collect();
    let prev: HashMap<String, i64> = {
        let conn = db.lock().unwrap_or_else(|e| e.into_inner());
        conn.prepare("SELECT mod_id, fails FROM link_health")
            .and_then(|mut st| st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect())
            .unwrap_or_default()
    };
    let checked = jobs.len();
    futures_util::stream::iter(jobs)
        .for_each_concurrent(concurrency.max(1), |(id, url)| {
            let prev_fails = prev.get(&id).copied().unwrap_or(0);
            async move {
                let status = if is_safe_url(&url) { probe(fetch, &url).await } else { 0 };
                let ok = (200..400).contains(&status);
                let fails = if ok { 0 } else { prev_fails + 1 };
                let conn = db.lock().unwrap_or_else(|e| e.into_inner());
                let _ = conn.execute(
                    "INSERT INTO link_health (mod_id, url, status, fails, checked_at) VALUES (?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT(mod_id) DO UPDATE SET url = ?2, status = ?3, fails = ?4, checked_at = ?5",
                    rusqlite::params![id, url, status as i64, fails, now_sec()],
                );
            }
        })
        .await;
    let dead = dead_ids(db);
    store.write().mark_unavailable(&dead);
    LinkSummary {
        checked,
        dead: dead.len(),
    }
}
