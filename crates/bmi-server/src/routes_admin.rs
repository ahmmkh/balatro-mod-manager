//! Bearer-token admin API and Prometheus metrics.
use crate::app::{AppState, ClientIp, json_error};
use crate::links::{check_all, dead_ids};
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use flate2::read::GzDecoder;
use serde_json::json;
use std::io::Read;
use std::sync::atomic::Ordering;
use subtle::ConstantTimeEq;

pub fn routes(state: AppState) -> Router<AppState> {
    let admin = Router::new()
        .route("/stats", get(stats))
        .route("/reload", post(reload))
        .route("/links/check", post(links_check))
        .route("/links", get(links))
        .route("/mods/{id}/hide", post(hide).delete(unhide))
        .route("/reports", get(reports))
        .route("/reports/{id}/log", get(report_log))
        .route("/ban", post(ban))
        .route("/whoami", get(whoami))
        .layer(middleware::from_fn_with_state(state.clone(), admin_auth));
    let metrics = Router::new()
        .route("/metrics", get(metrics))
        .layer(middleware::from_fn_with_state(state, metrics_auth));
    Router::new().nest("/admin", admin).merge(metrics)
}

fn bearer_ok(st: &AppState, req: &Request) -> bool {
    let Some(token) = &st.cfg.admin_token else {
        return false;
    };
    let given = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    // The token's length is not a secret worth hiding; the content comparison is constant-time.
    given.len() == token.len() && bool::from(given.as_bytes().ct_eq(token.as_bytes()))
}

async fn admin_auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if st.cfg.admin_token.is_none() {
        return json_error(StatusCode::NOT_FOUND, "not found");
    }
    let ip = req
        .extensions()
        .get::<ClientIp>()
        .map(|c| c.0.clone())
        .unwrap_or_default();
    if !st.limiter.allow("admin", &ip, 30.0, 30.0) {
        return json_error(StatusCode::TOO_MANY_REQUESTS, "rate limited");
    }
    if !bearer_ok(&st, &req) {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(req).await
}

async fn metrics_auth(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if st.cfg.admin_token.is_none() {
        return json_error(StatusCode::NOT_FOUND, "not found");
    }
    if !bearer_ok(&st, &req) {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    next.run(req).await
}

fn report_count(st: &AppState) -> i64 {
    st.reports
        .lock()
        .ok()
        .and_then(|c| {
            c.query_row("SELECT count(*) FROM reports", [], |r| r.get(0))
                .ok()
        })
        .unwrap_or(0)
}

async fn stats(State(st): State<AppState>) -> Response {
    let snap = st.downloads.snapshot();
    let mut top: Vec<_> = snap.iter().collect();
    top.sort_by(|a, b| b.1.total.cmp(&a.1.total).then_with(|| a.0.cmp(b.0)));
    let top: Vec<_> = top
        .into_iter()
        .take(10)
        .map(|(id, d)| json!([id, d]))
        .collect();
    let (mods, loaded) = {
        let s = st.store.read();
        (s.mods().len(), s.loaded_at())
    };
    Json(json!({
        "mods": mods,
        "catalogLoadedAt": loaded,
        "sync": st.sync.as_ref().map(|s| s.status()),
        "totalDownloads": snap.values().map(|d| d.total).sum::<u64>(),
        "downloadsToday": snap.values().map(|d| d.today).sum::<u64>(),
        "top": top,
        "deadLinks": dead_ids(&st.db).len(),
        "reports": report_count(&st),
    }))
    .into_response()
}

// Both jobs outlive any sane HTTP timeout, so they run in the background; poll /admin/stats.
async fn reload(State(st): State<AppState>) -> Response {
    let Some(sync) = st.sync.clone() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "started": false, "sync": null })),
        )
            .into_response();
    };
    if sync.status().running {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "started": false, "sync": sync.status() })),
        )
            .into_response();
    }
    tokio::spawn(async move { sync.sync_once(true).await });
    (StatusCode::ACCEPTED, Json(json!({ "started": true }))).into_response()
}

async fn links_check(State(st): State<AppState>) -> Response {
    if st.link_check_running.swap(true, Ordering::SeqCst) {
        return (StatusCode::CONFLICT, Json(json!({ "started": false }))).into_response();
    }
    tokio::spawn(async move {
        let s = check_all(&st.db, &st.store, &*st.fetch, 6).await;
        tracing::info!(event = "links_checked", checked = s.checked, dead = s.dead);
        st.link_check_running.store(false, Ordering::SeqCst);
    });
    (StatusCode::ACCEPTED, Json(json!({ "started": true }))).into_response()
}

async fn links(State(st): State<AppState>) -> Response {
    let rows: Vec<serde_json::Value> = {
        let conn = st.db.lock().unwrap_or_else(|e| e.into_inner());
        conn.prepare("SELECT mod_id, url, status, fails, checked_at FROM link_health WHERE fails > 0 ORDER BY fails DESC, mod_id")
            .and_then(|mut s| {
                s.query_map([], |r| {
                    Ok(json!({"mod_id": r.get::<_, String>(0)?, "url": r.get::<_, String>(1)?, "status": r.get::<_, i64>(2)?, "fails": r.get::<_, i64>(3)?, "checked_at": r.get::<_, i64>(4)?}))
                })?
                .collect()
            })
            .unwrap_or_default()
    };
    Json(rows).into_response()
}

fn set_hidden(st: &AppState, id: &str, hidden: bool) -> Response {
    if st.store.write().set_hidden(id, hidden, (st.now)()) {
        Json(json!({ "ok": true })).into_response()
    } else {
        json_error(StatusCode::NOT_FOUND, "unknown mod")
    }
}
async fn hide(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    set_hidden(&st, &id, true)
}
async fn unhide(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    set_hidden(&st, &id, false)
}

async fn reports(State(st): State<AppState>) -> Response {
    let rows: Vec<serde_json::Value> = {
        let conn = st.reports.lock().unwrap_or_else(|e| e.into_inner());
        conn.prepare("SELECT id, created_at, title, description, mm_version, os, arch, log_bytes FROM reports ORDER BY id DESC LIMIT 50")
            .and_then(|mut s| {
                s.query_map([], |r| {
                    Ok(json!({"id": r.get::<_, i64>(0)?, "created_at": r.get::<_, i64>(1)?, "title": r.get::<_, String>(2)?,
                        "description": r.get::<_, String>(3)?, "mm_version": r.get::<_, String>(4)?, "os": r.get::<_, String>(5)?,
                        "arch": r.get::<_, String>(6)?, "log_bytes": r.get::<_, i64>(7)?}))
                })?
                .collect()
            })
            .unwrap_or_default()
    };
    Json(rows).into_response()
}

async fn report_log(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let gz: Option<Vec<u8>> = st.reports.lock().ok().and_then(|c| {
        c.query_row("SELECT log_gz FROM reports WHERE id = ?1", [id], |r| {
            r.get(0)
        })
        .ok()
    });
    let Some(gz) = gz else {
        return json_error(StatusCode::NOT_FOUND, "not found");
    };
    let mut out = Vec::new();
    if GzDecoder::new(&gz[..]).read_to_end(&mut out).is_err() {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "corrupt log");
    }
    (
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        String::from_utf8_lossy(&out).into_owned(),
    )
        .into_response()
}

async fn ban(State(st): State<AppState>, body: axum::body::Bytes) -> Response {
    let ip = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("ip").and_then(|i| i.as_str()).map(str::to_string))
        .filter(|s| !s.is_empty());
    let Some(ip) = ip else {
        return json_error(StatusCode::BAD_REQUEST, "ip required");
    };
    let ip = crate::app::normalize_ip(&ip);
    let hash = st.downloads.hash(&["ban", &ip]);
    {
        let conn = st.db.lock().unwrap_or_else(|e| e.into_inner());
        let _ = conn.execute(
            "INSERT OR IGNORE INTO banned (hash, reason, created_at) VALUES (?1, 'admin', ?2)",
            rusqlite::params![hash, (st.now)()],
        );
    }
    st.banned.write().insert(hash);
    Json(json!({ "ok": true })).into_response()
}

async fn whoami(Extension(ip): Extension<ClientIp>) -> Response {
    Json(json!({ "ip": ip.0 })).into_response()
}

fn db_bytes(path: &std::path::Path) -> u64 {
    let size = |p: &std::path::Path| std::fs::metadata(p).map_or(0, |m| m.len());
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    size(path) + size(std::path::Path::new(&wal))
}

/// (free, total) bytes of the filesystem holding `dir`; (0, 0) where unsupported (the server is
/// deployed on Linux; other OSes only need to compile and run the tests).
#[cfg(unix)]
fn volume_bytes(dir: &std::path::Path) -> (u64, u64) {
    rustix::fs::statvfs(dir)
        .map(|v| (v.f_bavail * v.f_frsize, v.f_blocks * v.f_frsize))
        .unwrap_or((0, 0))
}
#[cfg(not(unix))]
fn volume_bytes(_dir: &std::path::Path) -> (u64, u64) {
    (0, 0)
}

async fn metrics(State(st): State<AppState>) -> Response {
    let snap = st.downloads.snapshot();
    let (mods, loaded) = {
        let s = st.store.read();
        (s.mods().len(), s.loaded_at())
    };
    let (free, total) = volume_bytes(&st.cfg.data_dir);
    let sync_ok = st
        .sync
        .as_ref()
        .is_none_or(|s| s.status().last_error.is_none());
    let mut l = vec![
        "# TYPE bmi_mods gauge".to_string(),
        format!("bmi_mods {mods}"),
        "# TYPE bmi_downloads_total counter".into(),
        format!(
            "bmi_downloads_total {}",
            snap.values().map(|d| d.total).sum::<u64>()
        ),
        "# TYPE bmi_dead_links gauge".into(),
        format!("bmi_dead_links {}", dead_ids(&st.db).len()),
        "# TYPE bmi_catalog_loaded_timestamp gauge".into(),
        format!("bmi_catalog_loaded_timestamp {loaded}"),
        "# TYPE bmi_sync_ok gauge".into(),
        format!("bmi_sync_ok {}", u8::from(sync_ok)),
        "# TYPE bmi_db_bytes gauge".into(),
        format!(
            "bmi_db_bytes{{db=\"main\"}} {}",
            db_bytes(&st.cfg.data_dir.join("bmi.sqlite"))
        ),
        format!(
            "bmi_db_bytes{{db=\"reports\"}} {}",
            db_bytes(&st.cfg.data_dir.join("reports.sqlite"))
        ),
        "# TYPE bmi_volume_free_bytes gauge".into(),
        format!("bmi_volume_free_bytes {free}"),
        "# TYPE bmi_volume_total_bytes gauge".into(),
        format!("bmi_volume_total_bytes {total}"),
        "# TYPE bmi_requests_total counter".into(),
    ];
    l.extend(
        st.counters
            .snapshot()
            .into_iter()
            .map(|(k, v)| format!("bmi_requests_total{{kind=\"{k}\"}} {v}")),
    );
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        l.join("\n") + "\n",
    )
        .into_response()
}
