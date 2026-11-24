//! Public, unauthenticated routes: health, catalog listings, downloads, thumbnails, reports.
use crate::app::{AppState, ClientIp, cacheable, if_none_match_hits, json_error};
use crate::catalog::{
    CatalogMod, Downloads, changed_since, paginate, parse_cursor, parse_limit, parse_sort,
    sort_mods,
};
use crate::store::Tombstone;
use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde::Serialize;
use std::collections::HashMap;
use std::io::Write;
use std::time::UNIX_EPOCH;
use tokio_util::io::ReaderStream;

const REPORT_MAX_BYTES: usize = 1_000_000;
const FIELD_MAX: usize = 20_000;
const MAX_AGE: u32 = 60;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/mods", get(list_mods))
        .route("/mods/changed", get(list_changed))
        .route("/mods/{id}", get(get_mod))
        .route("/mods/{id}/download", post(download))
        .route("/downloads", get(downloads_map))
        .route("/thumbnails/{file}", get(thumbnail))
        .route("/report", post(report))
}

fn limited(st: &AppState, ip: &ClientIp, cls: &str, per_minute: f64, burst: f64) -> bool {
    !st.limiter.allow(cls, &ip.0, per_minute, burst)
}
fn rate_limited() -> Response {
    json_error(StatusCode::TOO_MANY_REQUESTS, "rate limited")
}

async fn healthz(State(st): State<AppState>) -> Response {
    let db_ok = st
        .db
        .lock()
        .map(|c| c.query_row("SELECT 1", [], |r| r.get::<_, i64>(0)).is_ok())
        .unwrap_or(false);
    if !db_ok {
        return (StatusCode::SERVICE_UNAVAILABLE, "db unavailable").into_response();
    }
    if st.store.read().ready() {
        "ok".into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "catalog not loaded").into_response()
    }
}

#[derive(Serialize)]
struct ModOut<'a> {
    #[serde(flatten)]
    m: &'a CatalogMod,
    downloads: Downloads,
}

#[derive(Serialize)]
#[serde(untagged)]
enum Item<'a> {
    Mod(ModOut<'a>),
    Gone(Tombstone),
}

#[derive(Serialize)]
struct ListOut<'a> {
    items: Vec<Item<'a>>,
    next_cursor: Option<String>,
}

fn list(
    st: &AppState,
    ip: &ClientIp,
    q: &HashMap<String, String>,
    headers: &HeaderMap,
    changed_only: bool,
) -> Response {
    if limited(st, ip, "read", 120.0, 120.0) {
        return rate_limited();
    }
    let Some(offset) = parse_cursor(q.get("cursor").map(String::as_str)) else {
        return json_error(StatusCode::BAD_REQUEST, "bad cursor");
    };
    let since = q.get("since").filter(|s| !s.is_empty());
    if changed_only && since.is_none() {
        return json_error(StatusCode::BAD_REQUEST, "since is required");
    }
    let counts = st.downloads.snapshot();
    let (mods, tombstones) = {
        let s = st.store.read();
        let tombs = match (changed_only, since) {
            (true, Some(since)) if offset == 0 => s.tombstones_since(since),
            _ => Vec::new(),
        };
        (s.mods(), tombs)
    };
    let source: Vec<CatalogMod> = match (changed_only, since) {
        (true, Some(since)) => changed_since(&mods, since),
        _ => mods.to_vec(),
    };
    let sorted = sort_mods(
        &source,
        parse_sort(q.get("sort").map_or("", String::as_str)),
        &counts,
    );
    let page = paginate(
        &sorted,
        offset,
        parse_limit(q.get("limit").map(String::as_str)),
    );
    let mut items: Vec<Item> = page
        .items
        .iter()
        .map(|m| {
            Item::Mod(ModOut {
                m,
                downloads: counts.get(&m.id).copied().unwrap_or_default(),
            })
        })
        .collect();
    // Removed/hidden mods are reported once, on the first page of a "changed" listing.
    items.extend(tombstones.into_iter().map(Item::Gone));
    match serde_json::to_vec(&ListOut {
        items,
        next_cursor: page.next_cursor,
    }) {
        Ok(body) => cacheable(headers, body, MAX_AGE),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

async fn list_mods(
    State(st): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    list(&st, &ip, &q, &headers, false)
}

async fn list_changed(
    State(st): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    list(&st, &ip, &q, &headers, true)
}

async fn get_mod(
    State(st): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if limited(&st, &ip, "read", 120.0, 120.0) {
        return rate_limited();
    }
    let Some(m) = st.store.read().get(&id) else {
        return json_error(StatusCode::NOT_FOUND, "mod not found");
    };
    let out = ModOut {
        downloads: st.downloads.get(&m.id),
        m: &m,
    };
    match serde_json::to_vec(&out) {
        Ok(body) => cacheable(&headers, body, MAX_AGE),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

async fn download(
    State(st): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    Path(id): Path<String>,
) -> Response {
    if limited(&st, &ip, "download", 60.0, 60.0) {
        return rate_limited();
    }
    let m = st.store.read().get(&id);
    let url = m
        .as_ref()
        .and_then(|m| m.meta.get("downloadURL"))
        .and_then(|v| v.as_str())
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let (Some(m), Some(url)) = (m, url) else {
        return json_error(StatusCode::NOT_FOUND, "mod not found");
    };
    if st.downloads.record(&m.id, &ip.0) {
        st.counters.inc("downloads_counted");
    }
    Json(serde_json::json!({ "download_url": url })).into_response()
}

async fn downloads_map(
    State(st): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    headers: HeaderMap,
) -> Response {
    if limited(&st, &ip, "read", 120.0, 120.0) {
        return rate_limited();
    }
    match serde_json::to_vec(&*st.downloads.snapshot()) {
        Ok(body) => cacheable(&headers, body, MAX_AGE),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

fn valid_thumb_name(file: &str) -> bool {
    file.ends_with(".webp") && file.len() > 5 && !file.contains(['/', '\\']) && !file.contains("..")
}

async fn thumbnail(
    State(st): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    Path(file): Path<String>,
    headers: HeaderMap,
) -> Response {
    if limited(&st, &ip, "read", 600.0, 600.0) {
        return rate_limited();
    }
    let dir = st.store.read().dir();
    let (true, Some(dir)) = (valid_thumb_name(&file), dir) else {
        return json_error(StatusCode::NOT_FOUND, "not found");
    };
    let path = dir.join("thumbnails").join(&file);
    let Ok(meta) = tokio::fs::metadata(&path).await else {
        return json_error(StatusCode::NOT_FOUND, "not found");
    };
    if !meta.is_file() {
        return json_error(StatusCode::NOT_FOUND, "not found");
    }
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis());
    let etag = format!("W/\"{}-{}\"", meta.len(), mtime_ms);
    let base = [
        (header::ETAG, etag.clone()),
        (header::CACHE_CONTROL, "public, max-age=86400".to_string()),
        (header::CONTENT_TYPE, "image/webp".to_string()),
    ];
    if if_none_match_hits(&headers, &etag) {
        return (StatusCode::NOT_MODIFIED, base).into_response();
    }
    match tokio::fs::File::open(&path).await {
        Ok(f) => (base, Body::from_stream(ReaderStream::new(f))).into_response(),
        Err(_) => json_error(StatusCode::NOT_FOUND, "not found"),
    }
}

fn field(v: &serde_json::Value, k: &str, max: usize) -> String {
    v.get(k)
        .and_then(|x| x.as_str())
        .map(|s| s.chars().take(max).collect())
        .unwrap_or_default()
}

struct NewReport {
    title: String,
    description: String,
    mm_version: String,
    os: String,
    arch: String,
    cpu: String,
    gpu: String,
    ram: String,
    log_filename: String,
    log: Vec<u8>,
}

enum ReportError {
    Full,
    Db,
}

fn store_report(st: &AppState, r: &NewReport) -> Result<(), ReportError> {
    let now = (st.now)();
    let day_start = now.div_euclid(86_400) * 86_400;
    let path = st.cfg.data_dir.join("reports.sqlite");
    let size = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len());
    let mut wal = path.clone().into_os_string();
    wal.push("-wal");
    let used = size(&path) + size(std::path::Path::new(&wal));

    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    gz.write_all(&r.log).map_err(|_| ReportError::Db)?;
    let log_gz = gz.finish().map_err(|_| ReportError::Db)?;

    let conn = st.reports.lock().unwrap_or_else(|e| e.into_inner());
    let today: i64 = conn
        .query_row(
            "SELECT count(*) FROM reports WHERE created_at >= ?1",
            [day_start],
            |row| row.get(0),
        )
        .map_err(|_| ReportError::Db)?;
    if today as u64 >= st.cfg.reports_per_day || used > st.cfg.reports_max_bytes {
        return Err(ReportError::Full);
    }
    conn.execute(
        "INSERT INTO reports (created_at, title, description, mm_version, os, arch, cpu, gpu, ram, log_filename, log_gz, log_bytes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        rusqlite::params![
            now, r.title, r.description, r.mm_version, r.os, r.arch, r.cpu, r.gpu, r.ram,
            r.log_filename, log_gz, r.log.len() as i64
        ],
    )
    .map_err(|_| ReportError::Db)?;
    Ok(())
}

async fn report(
    State(st): State<AppState>,
    Extension(ip): Extension<ClientIp>,
    req: Request,
) -> Response {
    if limited(&st, &ip, "report", 5.0 / 60.0, 5.0) {
        return json_error(StatusCode::TOO_MANY_REQUESTS, "too many reports");
    }
    let Ok(bytes) = axum::body::to_bytes(req.into_body(), REPORT_MAX_BYTES).await else {
        return json_error(StatusCode::PAYLOAD_TOO_LARGE, "report too large");
    };
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return json_error(StatusCode::BAD_REQUEST, "invalid json");
    };
    let title = field(&body, "title", 300);
    if title.trim().is_empty() || !body.get("description").is_some_and(|d| d.is_string()) {
        return json_error(
            StatusCode::BAD_REQUEST,
            "title and description are required",
        );
    }
    use base64::Engine;
    let log = match body.get("log_b64").and_then(|v| v.as_str()) {
        None | Some("") => Vec::new(),
        Some(b64) => match base64::engine::general_purpose::STANDARD.decode(b64.trim()) {
            Ok(b) => b,
            Err(_) => return json_error(StatusCode::BAD_REQUEST, "invalid log_b64"),
        },
    };
    let r = NewReport {
        description: field(&body, "description", FIELD_MAX),
        mm_version: field(&body, "mm_version", 64),
        os: field(&body, "os", 200),
        arch: field(&body, "arch", 64),
        cpu: field(&body, "cpu", 200),
        gpu: field(&body, "gpu", 200),
        ram: field(&body, "ram", 64),
        log_filename: field(&body, "log_filename", 200),
        title,
        log,
    };
    let st2 = st.clone();
    let stored = tokio::task::spawn_blocking(move || store_report(&st2, &r).map(|()| r)).await;
    let r = match stored {
        Ok(Ok(r)) => r,
        Ok(Err(ReportError::Full)) => {
            tracing::error!(event = "reports_full");
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "reports are temporarily disabled",
            );
        }
        _ => return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    };
    st.counters.inc("reports");
    if let Some(url) = st.cfg.discord_webhook_url.clone() {
        let desc: String = r.description.chars().take(1200).collect();
        let content: String = format!(
            "**New BMM report:** {}\n{}\n_v{} · {}_",
            r.title,
            desc,
            r.mm_version.chars().take(32).collect::<String>(),
            r.os.chars().take(80).collect::<String>()
        )
        .chars()
        .take(1900)
        .collect();
        let http = st.http.clone();
        tokio::spawn(async move {
            let res = http
                .post(url)
                .timeout(std::time::Duration::from_secs(5))
                .json(
                    &serde_json::json!({ "content": content, "allowed_mentions": { "parse": [] } }),
                )
                .send()
                .await;
            if let Err(e) = res {
                tracing::error!(event = "discord_forward_failed", error = %e);
            }
        });
    }
    (StatusCode::CREATED, Json(serde_json::json!({ "ok": true }))).into_response()
}
