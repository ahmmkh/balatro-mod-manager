use bmi_server::catalog::Downloads;
use bmi_server::db::{self, Db};
use bmi_server::downloads::{DownloadCounter, Today};
use std::sync::{Arc, Mutex};

struct Fx {
    db: Db,
    secret: String,
    day: Arc<Mutex<String>>,
    _dir: tempfile::TempDir,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let conn = db::open(&dir.path().join("bmi.sqlite")).unwrap();
    let secret = db::secret(&conn).unwrap();
    Fx {
        db: Arc::new(Mutex::new(conn)),
        secret,
        day: Arc::new(Mutex::new("2026-11-15".into())),
        _dir: dir,
    }
}

fn counter(f: &Fx) -> DownloadCounter {
    let day = f.day.clone();
    let today: Today = Arc::new(move || day.lock().unwrap().clone());
    DownloadCounter::new(f.db.clone(), f.secret.clone(), today).unwrap()
}

#[test]
fn counts_once_per_visitor_mod_day_and_rolls_over() {
    let f = fx();
    let c = counter(&f);
    assert!(c.record("a@Alpha", "9.9.9.9"));
    assert!(!c.record("a@Alpha", "9.9.9.9"));
    assert!(!c.record("a@Alpha", "9.9.9.9"));
    assert!(c.record("a@Alpha", "8.8.8.8"));
    assert!(c.record("b@Beta", "9.9.9.9"));
    assert_eq!(
        c.get("a@Alpha"),
        Downloads {
            total: 2,
            today: 2,
            week: 2
        }
    );
    assert_eq!(c.get("b@Beta").total, 1);
    assert_eq!(c.get("nope"), Downloads::default());

    *f.day.lock().unwrap() = "2026-11-16".into();
    assert!(c.record("a@Alpha", "9.9.9.9"));
    assert_eq!(
        c.get("a@Alpha"),
        Downloads {
            total: 3,
            today: 1,
            week: 3
        }
    );
}

#[test]
fn persists_across_restart_without_double_counting() {
    let f = fx();
    let c = counter(&f);
    c.record("a@Alpha", "1.2.3.4");
    c.record("a@Alpha", "1.2.3.5");
    assert_eq!(c.flush().unwrap(), 2);
    assert_eq!(c.flush().unwrap(), 0);
    let reborn = counter(&f);
    assert_eq!(reborn.get("a@Alpha").total, 2);
    assert!(!reborn.record("a@Alpha", "1.2.3.4"), "seen-set restored");
}

#[test]
fn failed_flush_keeps_pending_counts_and_retries() {
    let f = fx();
    let c = counter(&f);
    c.record("a@Alpha", "1.1.1.1");
    c.record("a@Alpha", "2.2.2.2");
    f.db.lock()
        .unwrap()
        .execute_batch("PRAGMA query_only = ON")
        .unwrap();
    assert!(c.flush().is_err());
    f.db.lock()
        .unwrap()
        .execute_batch("PRAGMA query_only = OFF")
        .unwrap();
    assert_eq!(c.flush().unwrap(), 2);
    assert_eq!(counter(&f).get("a@Alpha").total, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_concurrent_records_are_exact() {
    let f = fx();
    let c = Arc::new(counter(&f));
    let hs: Vec<_> = (0..200)
        .map(|i| {
            let c = c.clone();
            tokio::spawn(async move { c.record("a@Alpha", &format!("10.0.0.{i}")) })
        })
        .collect();
    for h in hs {
        assert!(h.await.unwrap());
    }
    assert_eq!(c.get("a@Alpha").total, 200);
    assert_eq!(c.flush().unwrap(), 200);
}

#[test]
fn flush_does_not_block_reads() {
    let f = fx();
    let c = Arc::new(counter(&f));
    for i in 0..500 {
        c.record("a@Alpha", &format!("ip{i}"));
    }
    let c2 = c.clone();
    let h = std::thread::spawn(move || c2.flush().unwrap());
    for _ in 0..1000 {
        let _ = c.get("a@Alpha");
    }
    assert_eq!(h.join().unwrap(), 500);
}

#[test]
fn hash_depends_on_secret_and_parts() {
    let f = fx();
    let c = counter(&f);
    assert_eq!(c.hash(&["a", "b"]), c.hash(&["a", "b"]));
    assert_ne!(c.hash(&["a", "b"]), c.hash(&["ab"]));
    assert_eq!(c.hash(&["x"]).len(), 64);
}

#[test]
fn a_late_request_across_midnight_does_not_reset_dedupe() {
    let f = fx();
    let c = counter(&f);
    assert!(c.record("a@Alpha", "1.1.1.1"));
    *f.day.lock().unwrap() = "2026-11-16".into();
    assert!(c.record("a@Alpha", "2.2.2.2"));
    *f.day.lock().unwrap() = "2026-11-15".into(); // stale clock reading / clock stepped back
    c.record("a@Alpha", "3.3.3.3");
    *f.day.lock().unwrap() = "2026-11-16".into();
    assert!(
        !c.record("a@Alpha", "2.2.2.2"),
        "same visitor must not count twice"
    );
}
