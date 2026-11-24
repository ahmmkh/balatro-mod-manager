//! Shared application state, middleware and the router.
use crate::config::Config;
use crate::db::{self, Db};
use crate::downloads::{DownloadCounter, Today};
use crate::links::{Fetch, ReqwestFetch};
use crate::ratelimit::RateLimiter;
use crate::store::Store;
use crate::sync::IndexSync;
use crate::timeutil::utc_day_of;
use anyhow::Result;
use axum::Router;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex as StdMutex};
use tower_http::compression::CompressionLayer;

/// Unix seconds; injected so tests control the clock.
pub type NowFn = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Default)]
pub struct Counters(Mutex<HashMap<String, u64>>);

impl Counters {
    pub fn inc(&self, kind: &str) {
        *self.0.lock().entry(kind.to_string()).or_insert(0) += 1;
    }
    pub fn snapshot(&self) -> Vec<(String, u64)> {
        let mut v: Vec<_> = self.0.lock().iter().map(|(k, n)| (k.clone(), *n)).collect();
        v.sort();
        v
    }
}

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<Config>,
    pub db: Db,
    pub reports: Db,
    pub store: Arc<RwLock<Store>>,
    pub downloads: Arc<DownloadCounter>,
    pub limiter: Arc<RateLimiter>,
    pub sync: Option<Arc<IndexSync>>,
    pub counters: Arc<Counters>,
    pub http: reqwest::Client,
    pub now: NowFn,
    /// Network access for link probes (stubbed in tests).
    pub fetch: Arc<dyn Fetch>,
    pub link_check_running: Arc<AtomicBool>,
    /// Salted hashes of banned client IPs.
    pub banned: Arc<RwLock<HashSet<String>>>,
}

impl AppState {
    /// Open both databases under `cfg.data_dir` and wire every service.
    pub fn build(cfg: Config, now: NowFn) -> Result<AppState> {
        std::fs::create_dir_all(&cfg.data_dir)?;
        let conn = db::open(&cfg.data_dir.join("bmi.sqlite"))?;
        let secret = db::secret(&conn)?;
        let db: Db = Arc::new(StdMutex::new(conn));
        let reports: Db = Arc::new(StdMutex::new(db::open_reports(
            &cfg.data_dir.join("reports.sqlite"),
        )?));
        let n = now.clone();
        let today: Today = Arc::new(move || utc_day_of(n()));
        let downloads = Arc::new(DownloadCounter::new(db.clone(), secret, today)?);
        let multiplier = cfg.rate_limit_multiplier;
        let banned: HashSet<String> = {
            let c = db.lock().unwrap_or_else(|e| e.into_inner());
            let mut st = c.prepare("SELECT hash FROM banned")?;
            st.query_map([], |r| r.get::<_, String>(0))?
                .collect::<Result<_, _>>()?
        };
        Ok(AppState {
            store: Arc::new(RwLock::new(Store::new(db.clone()))),
            cfg: Arc::new(cfg),
            db,
            reports,
            downloads,
            limiter: Arc::new(RateLimiter::default().with_multiplier(multiplier)),
            sync: None,
            counters: Arc::new(Counters::default()),
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            now,
            fetch: Arc::new(ReqwestFetch::new()?),
            link_check_running: Arc::new(AtomicBool::new(false)),
            banned: Arc::new(RwLock::new(banned)),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ClientIp(pub String);

/// `cf-connecting-ip`, then the first `x-forwarded-for`, only when `trust_proxy`; otherwise the
/// socket address, else `"unknown"`.
pub fn client_ip(cfg: &Config, headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    if cfg.trust_proxy {
        let cf = headers
            .get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .map(str::trim);
        if let Some(v) = cf.filter(|v| !v.is_empty()) {
            return v.to_string();
        }
        let xff = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim);
        if let Some(v) = xff.filter(|v| !v.is_empty()) {
            return v.to_string();
        }
    }
    peer.map_or_else(|| "unknown".into(), |p| p.ip().to_string())
}

/// Strong ETag: hex of SHA-256 truncated to 16 bytes, quoted.
pub fn etag_for(body: &[u8]) -> String {
    let d = Sha256::digest(body);
    let hex: String = d[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("\"{hex}\"")
}

pub fn if_none_match_hits(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim().trim_start_matches("W/") == etag.trim_start_matches("W/"))
        })
}

/// A JSON body with `Cache-Control` + `ETag`, or `304` on a matching `If-None-Match`.
pub fn cacheable(req: &HeaderMap, body: Vec<u8>, max_age: u32) -> Response {
    let etag = etag_for(&body);
    let cc = format!("public, max-age={max_age}, stale-while-revalidate=300");
    let mut res = if if_none_match_hits(req, &etag) {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        ([(header::CONTENT_TYPE, "application/json")], body).into_response()
    };
    let h = res.headers_mut();
    h.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_str(&cc).expect("static"),
    );
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    res
}

pub fn json_error(status: StatusCode, msg: &str) -> Response {
    (status, axum::Json(serde_json::json!({ "error": msg }))).into_response()
}

/// Resolve the client IP, count the response class, and make sure nothing is cached unless a
/// handler said so explicitly. Errors are never cacheable.
async fn edge(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let peer = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0);
    let ip = client_ip(&st.cfg, req.headers(), peer);
    if req.uri().path() != "/healthz"
        && st.banned.read().contains(&st.downloads.hash(&["ban", &ip]))
    {
        st.counters.inc("blocked");
        return json_error(StatusCode::FORBIDDEN, "forbidden");
    }
    req.extensions_mut().insert(ClientIp(ip));
    let mut res = next.run(req).await;
    st.counters
        .inc(&format!("http_{}xx", res.status().as_u16() / 100));
    let ok = res.status().is_success() || res.status() == StatusCode::NOT_MODIFIED;
    if !ok || !res.headers().contains_key(header::CACHE_CONTROL) {
        res.headers_mut().insert(
            HeaderName::from_static("cache-control"),
            HeaderValue::from_static("no-store"),
        );
    }
    res
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(crate::routes_public::routes())
        .merge(crate::routes_admin::routes(state.clone()))
        .fallback(|| async { json_error(StatusCode::NOT_FOUND, "not found") })
        .layer(middleware::from_fn_with_state(state.clone(), edge))
        .layer(CompressionLayer::new())
        .with_state(state)
}
