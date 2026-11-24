mod common;
use async_trait::async_trait;
use bmi_server::links::{Fetch, check_all, is_global_ip, is_safe_url, probe};
use common::*;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn safe_url_accepts_public_https_only() {
    assert!(is_safe_url("https://github.com/x/y.zip"));
    for u in [
        "http://github.com/x",
        "https://localhost/x",
        "https://127.0.0.1/x",
        "https://10.0.0.5/x",
        "https://192.168.1.1/x",
        "https://169.254.169.254/x",
        "https://172.16.0.1/x",
        "https://svc.internal/x",
        "https://printer.local/x",
        "https://[::1]/x",
        "https://0.0.0.0/x",
        "file:///etc/passwd",
        "nope",
    ] {
        assert!(!is_safe_url(u), "{u}");
    }
    assert!(is_safe_url("https://172.32.0.1/x"));
}

struct Stub {
    head: u16,
    get: u16,
    calls: Mutex<Vec<&'static str>>,
}
#[async_trait]
impl Fetch for Stub {
    async fn head(&self, _: &str) -> Option<u16> {
        self.calls.lock().unwrap().push("HEAD");
        (self.head != 0).then_some(self.head)
    }
    async fn ranged_get(&self, _: &str) -> Option<u16> {
        self.calls.lock().unwrap().push("GET");
        Some(self.get)
    }
}

#[tokio::test]
async fn probe_falls_back_to_ranged_get() {
    let s = Stub {
        head: 405,
        get: 206,
        calls: Mutex::new(vec![]),
    };
    assert_eq!(probe(&s, "https://x.test/a.zip").await, 206);
    assert_eq!(*s.calls.lock().unwrap(), ["HEAD", "GET"]);
    let ok = Stub {
        head: 200,
        get: 0,
        calls: Mutex::new(vec![]),
    };
    assert_eq!(probe(&ok, "https://x.test/a.zip").await, 200);
    assert_eq!(ok.calls.lock().unwrap().len(), 1);
    let down = Stub {
        head: 0,
        get: 200,
        calls: Mutex::new(vec![]),
    };
    assert_eq!(probe(&down, "https://x.test/a.zip").await, 0);
}

struct ByUrl(AtomicBool);
#[async_trait]
impl Fetch for ByUrl {
    async fn head(&self, url: &str) -> Option<u16> {
        Some(if url.contains("Alpha") && !self.0.load(Ordering::SeqCst) {
            404
        } else {
            200
        })
    }
    async fn ranged_get(&self, _: &str) -> Option<u16> {
        None
    }
}

#[tokio::test]
async fn unavailable_only_after_three_failures_and_recovers() {
    let f = fx();
    let _d = f.load(&std_mods()[..2], &[]);
    let fetch = ByUrl(AtomicBool::new(false));
    for _ in 0..2 {
        check_all(&f.st.db, &f.st.store, &fetch, 6).await;
    }
    assert!(!f.st.store.read().get("a@Alpha").unwrap().unavailable);
    let third = check_all(&f.st.db, &f.st.store, &fetch, 6).await;
    assert_eq!((third.checked, third.dead), (2, 1));
    assert!(f.st.store.read().get("a@Alpha").unwrap().unavailable);
    assert!(!f.st.store.read().get("b@Beta").unwrap().unavailable);
    fetch.0.store(true, Ordering::SeqCst);
    check_all(&f.st.db, &f.st.store, &fetch, 6).await;
    assert!(!f.st.store.read().get("a@Alpha").unwrap().unavailable);
}

#[tokio::test]
async fn unavailable_flags_survive_a_catalog_reload() {
    let f = fx();
    let _d = f.load(&std_mods()[..2], &[]);
    let fetch = ByUrl(AtomicBool::new(false));
    for _ in 0..3 {
        check_all(&f.st.db, &f.st.store, &fetch, 6).await;
    }
    assert!(f.st.store.read().get("a@Alpha").unwrap().unavailable);
    assert!(f.st.store.write().set_hidden("b@Beta", true, 5000)); // triggers a reload
    assert!(
        f.st.store.read().get("a@Alpha").unwrap().unavailable,
        "flag lost by reload"
    );
}

#[test]
fn ssrf_guard_rejects_trailing_dot_cgnat_benchmark_and_odd_ports() {
    for u in [
        "https://localhost./",
        "https://foo.localhost./",
        "https://100.64.0.1/x",
        "https://100.100.100.100/x",
        "https://198.18.0.1/x",
        "https://[fd00::1]/x",
        "https://[::ffff:127.0.0.1]/x",
        "https://example.com:8443/x",
        "https://intranet/x",
    ] {
        assert!(!is_safe_url(u), "{u}");
    }
    assert!(is_safe_url("https://github.com:443/x"));
    assert!(is_safe_url("https://[2606:4700::1111]/x"));
}

#[test]
fn global_ip_classification() {
    for (ip, want) in [
        ("8.8.8.8", true),
        ("1.1.1.1", true),
        ("127.0.0.1", false),
        ("10.1.2.3", false),
        ("172.31.0.1", false),
        ("169.254.169.254", false),
        ("100.127.255.255", false),
        ("100.128.0.1", true),
        ("224.0.0.1", false),
        ("2606:4700::1111", true),
        ("::1", false),
        ("fe80::1", false),
        ("2001:db8::1", false),
    ] {
        assert_eq!(is_global_ip(ip.parse().unwrap()), want, "{ip}");
    }
}
