use bmi_server::app::{AppState, NowFn, router};
use bmi_server::config::Config;
use bmi_server::db::now_sec;
use bmi_server::links::check_all;
use bmi_server::maintenance::prune_reports;
use bmi_server::sync::IndexSync;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    if std::env::args().any(|a| a == "--version") {
        println!("bmi-server {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let env: HashMap<String, String> = std::env::vars().collect();
    let cfg = Config::from_env(&env)?;
    let now: NowFn = Arc::new(now_sec);
    let mut st = AppState::build(cfg.clone(), now)?;
    let sync = cfg
        .index_repo_url
        .is_some()
        .then(|| Arc::new(IndexSync::new(cfg.clone(), st.store.clone())));
    st.sync = sync.clone();

    // Serve whatever catalog is already on disk before the first network sync finishes.
    let disk = IndexSync::new(cfg.clone(), st.store.clone());
    match disk.load_from_disk() {
        Some(s) => tracing::info!(event = "catalog_loaded_from_disk", total = s.total),
        None => tracing::warn!(event = "no_catalog_on_disk"),
    }

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    tracing::info!(event = "listening", port = cfg.port, trust_proxy = cfg.trust_proxy, admin = cfg.admin_token.is_some(), repo = ?cfg.index_repo_url);

    spawn_background(&st, sync);

    let app = router(st.clone()).into_make_service_with_connect_info::<SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    tracing::info!(event = "shutdown", note = "flushing counters");
    let d = st.downloads.clone();
    match tokio::task::spawn_blocking(move || d.flush()).await? {
        Ok(n) => tracing::info!(event = "final_flush", rows = n),
        Err(e) => tracing::error!(event = "final_flush_failed", error = %e),
    }
    Ok(())
}

fn every<F, Fut>(period: Duration, mut f: F)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut t = tokio::time::interval(period);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        t.tick().await; // first tick is immediate; skip it
        loop {
            t.tick().await;
            f().await;
        }
    });
}

fn spawn_background(st: &AppState, sync: Option<Arc<IndexSync>>) {
    let d = st.downloads.clone();
    every(Duration::from_secs(5), move || {
        let d = d.clone();
        async move {
            match tokio::task::spawn_blocking(move || d.flush()).await {
                Ok(Err(e)) => tracing::error!(event = "flush_failed", error = %e),
                Err(e) => tracing::error!(event = "flush_panicked", error = %e),
                Ok(Ok(_)) => {}
            }
        }
    });
    let l = st.limiter.clone();
    every(Duration::from_secs(60), move || {
        l.sweep();
        async {}
    });
    let (reports, now) = (st.reports.clone(), st.now.clone());
    every(Duration::from_secs(24 * 3600), move || {
        let (r, n) = (reports.clone(), now.clone());
        async move {
            match tokio::task::spawn_blocking(move || prune_reports(&r, n())).await {
                Ok(Ok(n)) => tracing::info!(event = "reports_pruned", deleted = n),
                other => {
                    tracing::error!(event = "reports_prune_failed", result = ?other.map(|r| r.map_err(|e| e.to_string())))
                }
            }
        }
    });
    if let Some(sync) = sync {
        let s = sync.clone();
        tokio::spawn(async move { s.sync_once(false).await });
        every(Duration::from_secs(st.cfg.sync_interval_sec), move || {
            let s = sync.clone();
            async move {
                s.sync_once(false).await;
            }
        });
    }
    let st2 = st.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(120)).await;
        run_link_check(&st2).await;
        every(
            Duration::from_secs(st2.cfg.link_check_interval_sec),
            move || {
                let st = st2.clone();
                async move { run_link_check(&st).await }
            },
        );
    });
}

async fn run_link_check(st: &AppState) {
    if st
        .link_check_running
        .swap(true, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let s = check_all(&st.db, &st.store, &*st.fetch, 6).await;
    st.link_check_running
        .store(false, std::sync::atomic::Ordering::SeqCst);
    tracing::info!(event = "links_checked", checked = s.checked, dead = s.dead);
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! { () = ctrl_c => {}, () = term => {} }
}
