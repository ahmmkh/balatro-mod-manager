#![allow(dead_code)]
use axum::Router;
use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use bmi_server::app::{AppState, NowFn, router};
use bmi_server::config::Config;
use http_body_util::BodyExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use tower::ServiceExt;

pub const ADMIN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
/// 2026-11-15 12:00:00 UTC
pub const T0: i64 = 1_794_571_200;

pub struct Fx {
    pub st: AppState,
    pub app: Router,
    pub clock: Arc<AtomicI64>,
    pub _dir: tempfile::TempDir,
}

pub fn mod_json(id: &str, name: &str, updated: i64, url: Option<&str>) -> serde_json::Value {
    let mut meta = serde_json::json!({ "title": name });
    if let Some(u) = url {
        meta["downloadURL"] = u.into();
    }
    serde_json::json!({ "id": id, "name": name, "updated_at": updated.to_string(), "description": format!("{name} desc"), "meta": meta })
}

pub fn std_mods() -> Vec<serde_json::Value> {
    vec![
        mod_json(
            "a@Alpha",
            "Alpha",
            100,
            Some("https://example.com/a%40Alpha.zip"),
        ),
        mod_json(
            "b@Beta",
            "Beta",
            200,
            Some("https://example.com/b%40Beta.zip"),
        ),
        mod_json("c@Gamma!", "Gamma", 300, Some("https://example.com/c.zip")),
    ]
}

pub fn write_catalog(mods: &[serde_json::Value], thumbs: &[&str]) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join("thumbnails")).unwrap();
    std::fs::write(
        d.path().join("index.json"),
        serde_json::to_vec(&serde_json::json!({ "mods": mods })).unwrap(),
    )
    .unwrap();
    for t in thumbs {
        std::fs::write(
            d.path().join("thumbnails").join(format!("{t}.webp")),
            b"RIFFxxxxWEBP",
        )
        .unwrap();
    }
    d
}

pub fn fx_with(edit: impl FnOnce(&mut Config)) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let env: HashMap<String, String> = [
        ("DATA_DIR", dir.path().to_str().unwrap()),
        ("ADMIN_TOKEN", ADMIN),
        ("TRUST_PROXY", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let mut cfg = Config::from_env(&env).unwrap();
    edit(&mut cfg);
    let clock = Arc::new(AtomicI64::new(T0));
    let c2 = clock.clone();
    let now: NowFn = Arc::new(move || c2.load(Ordering::SeqCst));
    let st = AppState::build(cfg, now).unwrap();
    Fx {
        app: router(st.clone()),
        st,
        clock,
        _dir: dir,
    }
}

pub fn fx() -> Fx {
    fx_with(|_| {})
}

impl Fx {
    /// Rebuild the router after mutating `self.st` (e.g. swapping the link fetcher).
    pub fn rebuild(&mut self) {
        self.app = router(self.st.clone());
    }

    pub fn load(&self, mods: &[serde_json::Value], thumbs: &[&str]) -> tempfile::TempDir {
        let d = write_catalog(mods, thumbs);
        self.st.store.write().load(d.path(), T0).unwrap();
        d
    }
}

pub struct Res {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Res {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|_| panic!("not json: {}", String::from_utf8_lossy(&self.body)))
    }
    pub fn header(&self, k: &str) -> Option<&str> {
        self.headers.get(k).and_then(|v| v.to_str().ok())
    }
}

pub async fn send(
    app: &Router,
    method: &str,
    path: &str,
    ip: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Res {
    let mut b = Request::builder()
        .method(method)
        .uri(path)
        .header("cf-connecting-ip", ip);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res: Response<Body> = app
        .clone()
        .oneshot(b.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let (parts, body) = res.into_parts();
    Res {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes().to_vec(),
    }
}

pub fn bearer() -> [(&'static str, String); 1] {
    [("authorization", format!("Bearer {ADMIN}"))]
}

pub async fn get(app: &Router, path: &str) -> Res {
    send(app, "GET", path, "1.1.1.1", &[], vec![]).await
}
pub async fn get_h(app: &Router, path: &str, headers: &[(&str, &str)]) -> Res {
    send(app, "GET", path, "1.1.1.1", headers, vec![]).await
}
pub async fn post(app: &Router, path: &str, ip: &str, body: &str) -> Res {
    send(app, "POST", path, ip, &[], body.as_bytes().to_vec()).await
}
