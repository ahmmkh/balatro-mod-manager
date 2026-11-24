use bmi_server::catalog::CatalogMod;
use bmi_server::db::{self, Db};
use bmi_server::store::{Store, Tombstone};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

fn m(id: &str, name: &str, updated: i64) -> CatalogMod {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": name, "updated_at": updated.to_string(), "description": "",
        "meta": {"downloadURL": format!("https://example.com/{id}.zip")}
    }))
    .unwrap()
}

fn catalog(mods: &[CatalogMod]) -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join("thumbnails")).unwrap();
    std::fs::write(
        d.path().join("index.json"),
        serde_json::to_vec(&serde_json::json!({ "mods": mods })).unwrap(),
    )
    .unwrap();
    let p = d.path().to_path_buf();
    (d, p)
}

fn store() -> (Store, Db, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let db: Db = Arc::new(Mutex::new(
        db::open(&dir.path().join("bmi.sqlite")).unwrap(),
    ));
    (Store::new(db.clone()), db, dir)
}

fn abc() -> [CatalogMod; 3] {
    [
        m("a@Alpha", "Alpha", 10),
        m("b@Beta", "Beta", 10),
        m("c@Gamma", "Gamma", 10),
    ]
}

#[test]
fn not_ready_before_load() {
    let (mut s, _db, _d) = store();
    assert!(!s.ready());
    let (_k, p) = catalog(&abc());
    s.load(&p, 1000).unwrap();
    assert!(s.ready());
}

#[test]
fn removed_mod_gets_tombstone_and_can_return() {
    let (mut s, _db, _d) = store();
    let [a, b, c] = abc();
    let (_k1, p1) = catalog(&[a.clone(), b.clone(), c.clone()]);
    s.load(&p1, 1000).unwrap();
    let (_k2, p2) = catalog(&[a.clone(), c.clone()]);
    let stats = s.load(&p2, 2000).unwrap();
    assert_eq!(stats.removed, 1);
    assert_eq!(
        s.tombstones_since("1500"),
        vec![Tombstone {
            id: "b@Beta".into(),
            deleted: true,
            updated_at: "2000".into()
        }]
    );
    assert_eq!(s.mods().len(), 2);
    assert!(s.get("b@Beta").is_none());

    let (_k3, p3) = catalog(&[a, b, c]);
    let back = s.load(&p3, 3000).unwrap();
    assert_eq!(back.reappeared, 1);
    assert_eq!(s.get("b@Beta").unwrap().updated_at, "3000");
    assert!(s.tombstones_since("2500").is_empty());
}

#[test]
fn first_seen_mods_are_changed_now() {
    let (mut s, _db, _d) = store();
    let (_k1, p1) = catalog(&[abc()[0].clone()]);
    s.load(&p1, 1000).unwrap();
    let (_k2, p2) = catalog(&[abc()[0].clone(), m("n@New", "New", 5)]);
    s.load(&p2, 5000).unwrap();
    let changed = bmi_server::catalog::changed_since(&s.mods(), "4000");
    assert_eq!(
        changed.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["n@New"]
    );
}

#[test]
fn hide_and_unhide_toggle_tombstone() {
    let (mut s, _db, _d) = store();
    let (_k, p) = catalog(&abc());
    s.load(&p, 1000).unwrap();
    assert!(s.set_hidden("a@Alpha", true, 2000));
    assert_eq!(s.mods().len(), 2);
    assert!(s.tombstones_since("0").iter().any(|t| t.id == "a@Alpha"));
    assert!(s.set_hidden("a@Alpha", false, 3000));
    assert_eq!(s.mods().len(), 3);
    assert!(s.tombstones_since("0").is_empty());
    assert!(!s.set_hidden("nope", true, 4000));
}

#[test]
fn empty_index_is_an_error_and_keeps_previous_catalog() {
    let (mut s, _db, _d) = store();
    let (_k, p) = catalog(&abc());
    s.load(&p, 1000).unwrap();
    let (_k2, empty) = catalog(&[]);
    assert!(s.load(&empty, 2000).is_err());
    assert_eq!(s.mods().len(), 3);
}

#[test]
fn schemas_are_separate() {
    let d = tempfile::tempdir().unwrap();
    let main = db::open(&d.path().join("bmi.sqlite")).unwrap();
    let tables = |c: &rusqlite::Connection| -> Vec<String> {
        let mut st = c
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();
        st.query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert!(!tables(&main).contains(&"reports".to_string()));
    assert!(tables(&main).contains(&"mod_state".to_string()));

    let rep = db::open_reports(&d.path().join("reports.sqlite")).unwrap();
    assert!(tables(&rep).contains(&"reports".to_string()));
    let av: i64 = rep
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    assert_eq!(av, 2);
}

#[test]
fn secret_is_stable() {
    let (_s, db, _d) = store();
    let c = db.lock().unwrap();
    let a = db::secret(&c).unwrap();
    assert_eq!(a.len(), 64);
    assert_eq!(a, db::secret(&c).unwrap());
}
