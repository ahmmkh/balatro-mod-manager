mod common;
use async_trait::async_trait;
use bmi_server::links::Fetch;
use common::*;
use std::sync::Arc;
use tokio::sync::Notify;

fn auth() -> Vec<(&'static str, String)> {
    bearer().to_vec()
}
async fn aget(app: &axum::Router, path: &str) -> Res {
    let a = auth();
    let h: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
    send(app, "GET", path, "1.1.1.1", &h, vec![]).await
}
async fn apost(app: &axum::Router, method: &str, path: &str, body: &str) -> Res {
    let a = auth();
    let h: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
    send(app, method, path, "1.1.1.1", &h, body.as_bytes().to_vec()).await
}

#[tokio::test]
async fn hidden_without_token_401_when_wrong_200_when_right() {
    let off = fx_with(|c| c.admin_token = None);
    assert_eq!(get(&off.app, "/admin/stats").await.status, 404);
    assert_eq!(get(&off.app, "/metrics").await.status, 404);

    let f = fx();
    let _d = f.load(&std_mods()[..1], &[]);
    assert_eq!(get(&f.app, "/admin/stats").await.status, 401);
    assert_eq!(
        get_h(&f.app, "/admin/stats", &[("authorization", "Bearer wrong")])
            .await
            .status,
        401
    );
    assert_eq!(
        get_h(
            &f.app,
            "/admin/stats",
            &[("authorization", "Bearer aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaX")]
        )
        .await
        .status,
        401
    );
    let ok = aget(&f.app, "/admin/stats").await;
    assert_eq!(ok.status, 200);
    assert_eq!(ok.json()["mods"], 1);
    assert_eq!(ok.header("cache-control"), Some("no-store"));
    assert_eq!(aget(&f.app, "/metrics").await.status, 200);
    assert_eq!(get(&f.app, "/metrics").await.status, 401);
}

#[tokio::test]
async fn ban_blocks_that_ip_only_and_keeps_healthz() {
    let f = fx();
    let _d = f.load(&std_mods(), &[]);
    assert_eq!(
        apost(&f.app, "POST", "/admin/ban", r#"{"ip":"6.6.6.6"}"#)
            .await
            .status,
        200
    );
    assert_eq!(apost(&f.app, "POST", "/admin/ban", "{}").await.status, 400);
    assert_eq!(
        send(&f.app, "GET", "/mods", "6.6.6.6", &[], vec![])
            .await
            .status,
        403
    );
    assert_eq!(
        send(&f.app, "GET", "/healthz", "6.6.6.6", &[], vec![])
            .await
            .status,
        200
    );
    assert_eq!(
        send(&f.app, "GET", "/mods", "7.7.7.7", &[], vec![])
            .await
            .status,
        200
    );
    // survives a restart: the ban set is reloaded from the database
    let again = bmi_server::app::AppState::build((*f.st.cfg).clone(), f.st.now.clone()).unwrap();
    assert_eq!(again.banned.read().len(), 1);
}

#[tokio::test]
async fn whoami_reports_resolved_ip() {
    let f = fx();
    let a = auth();
    let h: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert_eq!(
        send(&f.app, "GET", "/admin/whoami", "5.4.3.2", &h, vec![])
            .await
            .json()["ip"],
        "5.4.3.2"
    );
    let g = fx_with(|c| c.trust_proxy = false);
    let a = auth();
    let h: Vec<(&str, &str)> = a.iter().map(|(k, v)| (*k, v.as_str())).collect();
    assert_eq!(
        send(&g.app, "GET", "/admin/whoami", "5.4.3.2", &h, vec![])
            .await
            .json()["ip"],
        "unknown"
    );
}

struct Gate(Arc<Notify>);
#[async_trait]
impl Fetch for Gate {
    async fn head(&self, _: &str) -> Option<u16> {
        self.0.notified().await;
        Some(200)
    }
    async fn ranged_get(&self, _: &str) -> Option<u16> {
        Some(200)
    }
}

#[tokio::test]
async fn long_jobs_return_202_then_409_and_reload_needs_a_repo() {
    let mut f = fx();
    let gate = Arc::new(Notify::new());
    f.st.fetch = Arc::new(Gate(gate.clone()));
    f.rebuild();
    let _d = f.load(&std_mods()[..1], &[]);
    assert_eq!(apost(&f.app, "POST", "/admin/reload", "").await.status, 400);
    assert_eq!(
        apost(&f.app, "POST", "/admin/links/check", "").await.status,
        202
    );
    assert_eq!(
        apost(&f.app, "POST", "/admin/links/check", "").await.status,
        409
    );
    gate.notify_waiters();
    gate.notify_one();
    for _ in 0..50 {
        if !f
            .st
            .link_check_running
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            break;
        }
        gate.notify_one();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        apost(&f.app, "POST", "/admin/links/check", "").await.status,
        202,
        "flag cleared after job"
    );
    gate.notify_one();
}

#[tokio::test]
async fn hide_and_unhide() {
    let f = fx();
    let _d = f.load(&std_mods()[..2], &[]);
    assert_eq!(
        apost(&f.app, "POST", "/admin/mods/a%40Alpha/hide", "")
            .await
            .status,
        200
    );
    assert_eq!(
        get(&f.app, "/mods").await.json()["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let ch = get(&f.app, "/mods/changed?since=0").await.json();
    assert!(
        ch["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["id"] == "a@Alpha" && i["deleted"] == true)
    );
    assert_eq!(
        apost(&f.app, "DELETE", "/admin/mods/a%40Alpha/hide", "")
            .await
            .status,
        200
    );
    assert_eq!(
        get(&f.app, "/mods").await.json()["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        apost(&f.app, "POST", "/admin/mods/nope/hide", "")
            .await
            .status,
        404
    );
}

#[tokio::test]
async fn metrics_include_sizes_and_volume() {
    let f = fx();
    let _d = f.load(&std_mods(), &[]);
    let body = String::from_utf8(aget(&f.app, "/metrics").await.body).unwrap();
    let val = |name: &str| -> u64 {
        body.lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("missing {name}\n{body}"))
            .rsplit(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(val("bmi_db_bytes{db=\"main\"}") > 0);
    assert!(val("bmi_db_bytes{db=\"reports\"}") > 0);
    assert!(val("bmi_volume_total_bytes") > 0);
    assert!(val("bmi_volume_free_bytes") <= val("bmi_volume_total_bytes"));
    assert_eq!(val("bmi_mods"), 3);
}

#[tokio::test]
async fn admin_reports_roundtrip_log() {
    let f = fx();
    use base64::Engine;
    let log = "hello log\nsecond line";
    let b = serde_json::json!({"title":"t","description":"d","log_b64": base64::engine::general_purpose::STANDARD.encode(log)});
    assert_eq!(
        post(&f.app, "/report", "2.2.2.2", &b.to_string())
            .await
            .status,
        201
    );
    let list = aget(&f.app, "/admin/reports").await.json();
    let id = list[0]["id"].as_i64().unwrap();
    let r = aget(&f.app, &format!("/admin/reports/{id}/log")).await;
    assert_eq!(String::from_utf8(r.body).unwrap(), log);
    assert_eq!(aget(&f.app, "/admin/reports/9999/log").await.status, 404);
    assert_eq!(aget(&f.app, "/admin/stats").await.json()["reports"], 1);
}
