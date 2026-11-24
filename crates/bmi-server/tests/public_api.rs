mod common;
use common::*;
use std::io::Read;
use std::sync::atomic::Ordering;

fn ids(v: &serde_json::Value) -> Vec<String> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn healthz_503_until_loaded() {
    let f = fx();
    assert_eq!(get(&f.app, "/healthz").await.status, 503);
    let _d = f.load(&std_mods(), &[]);
    let r = get(&f.app, "/healthz").await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("cache-control"), Some("no-store"));
}

#[tokio::test]
async fn paging_walks_all_and_validates_params() {
    let f = fx();
    let mods: Vec<_> = (0..5)
        .map(|i| mod_json(&format!("m{i}@M"), &format!("M{i}"), i + 1, None))
        .collect();
    let _d = f.load(&mods, &[]);
    let (mut seen, mut cursor) = (vec![], String::new());
    for _ in 0..10 {
        let r = get(&f.app, &format!("/mods?limit=2{cursor}")).await.json();
        seen.extend(ids(&r));
        match r["next_cursor"].as_str() {
            Some(c) => cursor = format!("&cursor={c}"),
            None => break,
        }
    }
    assert_eq!(seen.len(), 5);
    assert_eq!(get(&f.app, "/mods?cursor=abc").await.status, 400);
    assert_eq!(get(&f.app, "/mods/changed").await.status, 400);
}

#[tokio::test]
async fn mod_json_shape() {
    let f = fx();
    let _d = f.load(&std_mods(), &["a@Alpha"]);
    let r = get(&f.app, "/mods/a%40Alpha").await.json();
    assert_eq!(
        r["updated_at"],
        T0.to_string(),
        "first-seen mods are stamped now"
    );
    assert_eq!(
        r["downloads"],
        serde_json::json!({"total":0,"today":0,"week":0})
    );
    assert!(r.get("unavailable").is_none());
    assert_eq!(
        get(&f.app, "/mods/c%40Gamma%21").await.json()["id"],
        "c@Gamma!"
    );
}

#[tokio::test]
async fn download_counts_once_and_resolves_url() {
    let f = fx();
    let _d = f.load(&std_mods(), &[]);
    let r = post(&f.app, "/mods/a%40Alpha/download", "9.9.9.9", "").await;
    assert_eq!(
        r.json(),
        serde_json::json!({"download_url": "https://example.com/a%40Alpha.zip"})
    );
    post(&f.app, "/mods/a%40Alpha/download", "9.9.9.9", "").await;
    post(&f.app, "/mods/a%40Alpha/download", "8.8.8.8", "").await;
    assert_eq!(f.st.downloads.get("a@Alpha").total, 2);
    assert_eq!(
        post(&f.app, "/mods/nope/download", "1.1.1.1", "")
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn mod_without_url_is_404() {
    let f = fx();
    let _d = f.load(&[mod_json("x@NoUrl", "NoUrl", 1, None)], &[]);
    assert_eq!(
        post(&f.app, "/mods/x%40NoUrl/download", "1.1.1.1", "")
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn weird_ids_do_not_traverse() {
    let f = fx();
    let _d = f.load(&std_mods(), &[]);
    for p in [
        "/mods/..%2F..%2Fetc",
        "/mods/%2e%2e",
        "/mods/a%40Alpha%2F..%2Fb",
    ] {
        assert_eq!(get(&f.app, p).await.status, 404, "{p}");
    }
}

#[tokio::test]
async fn downloads_sort_uses_live_counts() {
    let f = fx();
    let _d = f.load(&std_mods(), &[]);
    for ip in ["1", "2", "3"] {
        f.st.downloads.record("c@Gamma!", ip);
    }
    f.st.downloads.record("b@Beta", "1");
    let r = get(&f.app, "/mods?sort=downloads_desc").await.json();
    assert_eq!(ids(&r), ["c@Gamma!", "b@Beta", "a@Alpha"]);
    assert_eq!(r["items"][0]["downloads"]["total"], 3);
}

#[tokio::test]
async fn thumbnails() {
    let f = fx();
    let _d = f.load(&std_mods(), &["a@Alpha"]);
    let ok = get(&f.app, "/thumbnails/a%40Alpha.webp").await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.header("content-type"), Some("image/webp"));
    assert_eq!(ok.header("cache-control"), Some("public, max-age=86400"));
    let etag = ok.header("etag").unwrap().to_string();
    assert_eq!(
        get_h(
            &f.app,
            "/thumbnails/a%40Alpha.webp",
            &[("if-none-match", &etag)]
        )
        .await
        .status,
        304
    );
    for p in [
        "/thumbnails/missing.webp",
        "/thumbnails/..%2Findex.json",
        "/thumbnails/a%40Alpha.png",
        "/thumbnails/..%2F..%2Fx.webp",
    ] {
        assert_eq!(get(&f.app, p).await.status, 404, "{p}");
    }
}

fn report_body(extra: serde_json::Value) -> String {
    let mut v = serde_json::json!({"title":"t","description":"d","mm_version":"1","os":"linux"});
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    v.to_string()
}

#[tokio::test]
async fn reports_validate_cap_and_rate_limit() {
    let f = fx();
    assert_eq!(
        post(
            &f.app,
            "/report",
            "1.1.1.1",
            &report_body(serde_json::json!({"log_b64":"QQ=="}))
        )
        .await
        .status,
        201
    );
    assert_eq!(post(&f.app, "/report", "2.2.2.2", "x").await.status, 400);
    assert_eq!(
        post(&f.app, "/report", "3.3.3.3", r#"{"description":"d"}"#)
            .await
            .status,
        400
    );
    let big = "x".repeat(1_100_000);
    assert_eq!(post(&f.app, "/report", "4.4.4.4", &big).await.status, 413);
    let codes: Vec<_> = {
        let mut v = vec![];
        for _ in 0..7 {
            v.push(
                post(
                    &f.app,
                    "/report",
                    "5.5.5.5",
                    &report_body(serde_json::json!({})),
                )
                .await
                .status
                .as_u16(),
            );
        }
        v
    };
    assert_eq!(codes, [201, 201, 201, 201, 201, 429, 429]);
}

#[tokio::test]
async fn trust_proxy_off_ignores_spoofed_header() {
    let f = fx_with(|c| c.trust_proxy = false);
    let _d = f.load(&std_mods(), &[]);
    let mut limited = 0;
    for i in 0..130 {
        if get_h(
            &f.app,
            "/mods",
            &[("cf-connecting-ip", &format!("10.0.0.{i}"))],
        )
        .await
        .status
            == 429
        {
            limited += 1;
        }
    }
    assert!(limited > 0);
}

#[tokio::test]
async fn tombstones_only_on_first_page() {
    let f = fx();
    let d1 = write_catalog(&std_mods(), &[]);
    f.st.store.write().load(d1.path(), 1000).unwrap();
    let d2 = write_catalog(&std_mods()[..2], &[]);
    f.st.store.write().load(d2.path(), 2000).unwrap();
    let r = get(&f.app, "/mods/changed?since=1500&limit=1").await.json();
    assert_eq!(
        r["items"],
        serde_json::json!([{"id":"c@Gamma!","deleted":true,"updated_at":"2000"}])
    );
    let r = get(&f.app, "/mods/changed?since=0&limit=1&cursor=1")
        .await
        .json();
    assert!(
        r["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i.get("deleted").is_none())
    );
}

#[tokio::test]
async fn downloads_map_is_lean() {
    let f = fx();
    let _d = f.load(&std_mods(), &[]);
    post(&f.app, "/mods/a%40Alpha/download", "1.1.1.1", "").await;
    post(&f.app, "/mods/a%40Alpha/download", "2.2.2.2", "").await;
    assert_eq!(
        get(&f.app, "/downloads").await.json(),
        serde_json::json!({"a@Alpha":{"total":2,"today":2,"week":2}})
    );
    // 448 mods of counts stay small
    for i in 0..448 {
        f.st.downloads
            .record(&format!("author{i}@SomeModName{i}"), "1.1.1.1");
    }
    assert!(get(&f.app, "/downloads").await.body.len() < 40_000);
}

#[tokio::test]
async fn cache_headers_per_route() {
    let f = fx();
    let _d = f.load(&std_mods(), &["a@Alpha"]);
    for p in [
        "/mods",
        "/mods/changed?since=0",
        "/mods/a%40Alpha",
        "/downloads",
    ] {
        let r = get(&f.app, p).await;
        assert_eq!(r.status, 200, "{p}");
        assert!(
            r.header("cache-control").unwrap().contains("max-age=60"),
            "{p}"
        );
        let etag = r
            .header("etag")
            .unwrap_or_else(|| panic!("no etag {p}"))
            .to_string();
        let again = get_h(&f.app, p, &[("if-none-match", &etag)]).await;
        assert_eq!(again.status, 304, "{p}");
        assert!(again.body.is_empty(), "{p}");
    }
    assert_eq!(
        post(&f.app, "/mods/a%40Alpha/download", "1.1.1.1", "")
            .await
            .header("cache-control"),
        Some("no-store")
    );
    assert_eq!(
        post(
            &f.app,
            "/report",
            "1.1.1.1",
            &report_body(serde_json::json!({}))
        )
        .await
        .header("cache-control"),
        Some("no-store")
    );
    assert_eq!(
        get(&f.app, "/mods/nope").await.header("cache-control"),
        Some("no-store")
    );
    assert_eq!(
        get(&f.app, "/nothing").await.header("cache-control"),
        Some("no-store")
    );
}

fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}
fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

#[tokio::test]
async fn reports_per_day_ceiling_resets_next_utc_day() {
    let f = fx_with(|c| c.reports_per_day = 3);
    for i in 0..3 {
        assert_eq!(
            post(
                &f.app,
                "/report",
                &format!("9.9.9.{i}"),
                &report_body(serde_json::json!({}))
            )
            .await
            .status,
            201
        );
    }
    assert_eq!(
        post(
            &f.app,
            "/report",
            "9.9.9.9",
            &report_body(serde_json::json!({}))
        )
        .await
        .status,
        503
    );
    f.clock.fetch_add(86_400, Ordering::SeqCst);
    assert_eq!(
        post(
            &f.app,
            "/report",
            "9.9.9.10",
            &report_body(serde_json::json!({}))
        )
        .await
        .status,
        201
    );
}

#[tokio::test]
async fn reports_byte_budget_stops_intake_and_stays_bounded() {
    let f = fx_with(|c| c.reports_max_bytes = 4096);
    let mut codes = vec![];
    for i in 0..12u64 {
        let body = report_body(serde_json::json!({"log_b64": b64(&noise(3000, i + 1))}));
        codes.push(
            post(&f.app, "/report", &format!("7.7.7.{i}"), &body)
                .await
                .status
                .as_u16(),
        );
    }
    assert!(codes.contains(&503), "{codes:?}");
    assert!(
        codes.iter().filter(|c| **c == 201).count() <= 6,
        "{codes:?}"
    );
    let main = std::fs::metadata(f._dir.path().join("bmi.sqlite"))
        .unwrap()
        .len();
    assert!(main < 200_000);
    let n: i64 =
        f.st.db
            .lock()
            .unwrap()
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name='reports'",
                [],
                |r| r.get(0),
            )
            .unwrap();
    assert_eq!(n, 0, "reports must not be in the counts db");
}

#[tokio::test]
async fn report_log_is_gzipped_and_roundtrips() {
    let f = fx();
    let log = "line of log\n".repeat(58_000); // ~700 KB: the largest log whose base64 fits the 1,000,000-byte body cap
    let body =
        report_body(serde_json::json!({"log_b64": b64(log.as_bytes()), "log_filename": "x.log"}));
    assert_eq!(post(&f.app, "/report", "1.1.1.1", &body).await.status, 201);
    let (gz, bytes): (Vec<u8>, i64) =
        f.st.reports
            .lock()
            .unwrap()
            .query_row("SELECT log_gz, log_bytes FROM reports", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
    let mut out = String::new();
    flate2::read::GzDecoder::new(&gz[..])
        .read_to_string(&mut out)
        .unwrap();
    assert_eq!(out, log);
    assert_eq!(bytes as usize, log.len());
    assert!(gz.len() < 20_000, "gz is {}", gz.len());
}

#[tokio::test]
async fn thumbnails_and_catalog_reads_have_separate_rate_limits() {
    let f = fx();
    let _d = f.load(&std_mods(), &["a@Alpha"]);
    assert_eq!(get(&f.app, "/mods").await.status, 200);
    let mut ok = 0;
    for _ in 0..200 {
        if get(&f.app, "/thumbnails/a%40Alpha.webp").await.status == 200 {
            ok += 1;
        }
    }
    assert_eq!(
        ok, 200,
        "thumbnails must not draw on the catalog read bucket"
    );
}

#[tokio::test]
async fn ipv6_addresses_in_one_slash_64_share_a_client_identity() {
    let f = fx();
    let _d = f.load(&std_mods(), &[]);
    for i in 0..3 {
        let ip = format!("2001:db8:1:2::{i:x}");
        post(&f.app, "/mods/a%40Alpha/download", &ip, "").await;
    }
    assert_eq!(f.st.downloads.get("a@Alpha").total, 1);
    post(&f.app, "/mods/a%40Alpha/download", "2001:db8:1:3::1", "").await;
    assert_eq!(f.st.downloads.get("a@Alpha").total, 2);
}
