//! End-to-end install tests against a fake game directory (a temp `BMM_CONFIG_DIR`) and a local
//! HTTP fixture server. They run unchanged on Windows, macOS and Linux.
use bmm_lib::errors::AppError;
use bmm_lib::installer::{install_mod, uninstall_mod};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Build a zip in memory.
fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zw = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            if name.ends_with('/') {
                zw.add_directory(*name, opts).unwrap();
            } else {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(data).unwrap();
            }
        }
        zw.finish().unwrap();
    }
    buf.into_inner()
}

/// Serves fixed bodies over HTTP on 127.0.0.1 (never `localhost`, which may resolve to IPv6).
struct FixtureServer {
    server: mockito::ServerGuard,
    pub base_url: String,
    _mocks: Vec<mockito::Mock>,
}

impl FixtureServer {
    async fn start(files: Vec<(String, Vec<u8>)>) -> FixtureServer {
        let mut server = mockito::Server::new_async().await;
        let mut mocks = Vec::new();
        for (path, body) in files {
            mocks.push(
                server
                    .mock("GET", path.as_str())
                    .with_status(200)
                    .with_header("content-type", "application/zip")
                    .with_body(body)
                    .create_async()
                    .await,
            );
        }
        let base_url = server.url();
        FixtureServer {
            server,
            base_url,
            _mocks: mocks,
        }
    }
}

struct Sandbox {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Sandbox {
    fn new() -> Sandbox {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        // SAFETY: tests in this file are serialised by ENV_LOCK.
        unsafe { std::env::set_var("BMM_CONFIG_DIR", &root) };
        Sandbox {
            _tmp: tmp,
            root,
            _guard: guard,
        }
    }
    fn mods(&self) -> PathBuf {
        self.root.join("Balatro").join("Mods")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("BMM_CONFIG_DIR") };
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Install `zip` under `folder` via the real downloader and return the result.
fn install(sb: &Sandbox, zip: Vec<u8>, folder: Option<&str>) -> Result<PathBuf, AppError> {
    let _ = sb;
    rt().block_on(async {
        let srv = FixtureServer::start(vec![("/m.zip".into(), zip)]).await;
        let r = install_mod(
            format!("{}/m.zip", srv.base_url),
            folder.map(str::to_string),
        )
        .await;
        drop(srv.server);
        r
    })
}

fn list(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let p = e.path();
                out.push(
                    p.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
                if p.is_dir() {
                    walk(base, &p, out);
                }
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
}

#[test]
fn normal_archive_installs_into_the_mods_dir() {
    let sb = Sandbox::new();
    let z = make_zip(&[("ModA/main.lua", b"x"), ("ModA/manifest.json", b"{}")]);
    let path = install(&sb, z, Some("ModA")).expect("install");
    assert_eq!(path, sb.mods().join("ModA"));
    assert!(path.join("main.lua").is_file() && path.join("manifest.json").is_file());
}

#[test]
fn flat_archive_lands_in_the_requested_folder() {
    let sb = Sandbox::new();
    let z = make_zip(&[("main.lua", b"x"), ("lovely.toml", b"")]);
    let path = install(&sb, z, Some("Flat")).expect("install");
    assert_eq!(path, sb.mods().join("Flat"));
    assert!(path.join("main.lua").is_file());
}

#[test]
fn unicode_mod_name_installs_and_is_listed() {
    let sb = Sandbox::new();
    let z = make_zip(&[("main.lua", b"x")]);
    let path = install(&sb, z, Some("Ünïcödé Mod")).expect("install");
    assert!(path.join("main.lua").is_file());
    // get_installed_mods() returns full folder paths.
    let listed = bmm_lib::finder::get_installed_mods();
    assert!(
        listed
            .iter()
            .any(|m| Path::new(m).file_name().is_some_and(|n| n == "Ünïcödé Mod")),
        "{listed:?}"
    );
}

#[test]
fn escaping_entries_are_rejected_and_nothing_is_written_outside() {
    for evil in ["../evil.txt", "/abs.txt", "C:\\x.txt", "a/../../evil.txt"] {
        let sb = Sandbox::new();
        let z = make_zip(&[("ok.lua", b"x"), (evil, b"boom")]);
        let err = install(&sb, z, Some("Evil")).expect_err(evil);
        assert!(
            matches!(err, AppError::PathValidation { .. }),
            "{evil}: {err:?}"
        );
        assert!(
            !sb.mods().join("Evil").exists(),
            "{evil}: partial folder left behind"
        );
        // the sandbox holds only our own config tree; nothing leaked beside it
        let top = list(&sb.root);
        assert!(
            !top.iter()
                .any(|p| p.contains("evil") && !p.starts_with("Balatro")),
            "{evil}: {top:?}"
        );
        assert!(
            !sb.root.parent().unwrap().join("evil.txt").exists(),
            "{evil}"
        );
    }
}

#[test]
fn names_illegal_on_windows_are_rejected_on_every_os() {
    for bad in [
        "a:b.lua",
        "CON.lua",
        "bad.",
        "bad ",
        "x/aux/y.lua",
        "q?.lua",
        "p|q.lua",
    ] {
        let sb = Sandbox::new();
        let z = make_zip(&[("main.lua", b"x"), (bad, b"x")]);
        let err = install(&sb, z, Some("Bad")).expect_err(bad);
        assert!(
            matches!(err, AppError::PathValidation { .. }),
            "{bad}: {err:?}"
        );
        assert!(!sb.mods().join("Bad").exists(), "{bad}");
    }
}

#[test]
fn case_collisions_are_rejected_on_every_os() {
    let sb = Sandbox::new();
    let z = make_zip(&[("Foo.lua", b"1"), ("foo.lua", b"2")]);
    let err = install(&sb, z, Some("Case")).expect_err("collision");
    assert!(matches!(err, AppError::PathValidation { .. }), "{err:?}");
    assert!(!sb.mods().join("Case").exists());
    // also between directories
    let z = make_zip(&[("Lib/a.lua", b"1"), ("lib/b.lua", b"2")]);
    assert!(install(&sb, z, Some("Case2")).is_err());
}

#[test]
fn over_long_target_paths_are_rejected() {
    let sb = Sandbox::new();
    let long = format!("{}/x.lua", "d".repeat(230));
    let z = make_zip(&[("main.lua", b"x"), (long.as_str(), b"x")]);
    let err = install(&sb, z, Some("Long")).expect_err("too long");
    assert!(matches!(err, AppError::PathValidation { .. }), "{err:?}");
}

#[test]
fn a_bad_second_entry_leaves_no_folder_behind() {
    let sb = Sandbox::new();
    let z = make_zip(&[("good.lua", b"x"), ("nested/CON", b"x")]);
    assert!(install(&sb, z, Some("Half")).is_err());
    let mods = list(&sb.mods());
    assert!(
        !mods
            .iter()
            .any(|p| p.starts_with("Half") || p.contains(".staging")),
        "{mods:?}"
    );
}

#[test]
fn http_404_and_non_archive_bodies_do_not_touch_the_mods_dir() {
    let sb = Sandbox::new();
    let r = rt().block_on(async {
        let srv = FixtureServer::start(vec![(
            "/junk.zip".into(),
            b"this is not an archive".to_vec(),
        )])
        .await;
        let notfound =
            install_mod(format!("{}/missing.zip", srv.base_url), Some("Gone".into())).await;
        let junk = install_mod(format!("{}/junk.zip", srv.base_url), Some("Junk".into())).await;
        (notfound, junk)
    });
    assert!(
        matches!(r.0, Err(AppError::NetworkRequest { .. })),
        "{:?}",
        r.0
    );
    assert!(r.1.is_err(), "{:?}", r.1);
    let mods = list(&sb.mods());
    assert!(
        !mods
            .iter()
            .any(|p| p.starts_with("Gone") || p.starts_with("Junk")),
        "{mods:?}"
    );
}

#[test]
fn uninstall_outside_the_mods_dir_is_refused() {
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.mods()).unwrap();
    let outside = sb.root.join("not-a-mod");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("keep.txt"), "x").unwrap();
    let r = rt().block_on(uninstall_mod(outside.clone()));
    assert!(r.is_err());
    assert!(outside.join("keep.txt").is_file());
}
