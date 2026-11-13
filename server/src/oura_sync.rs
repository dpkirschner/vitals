//! Background Oura sync (`--features oura`). With Oura configured, the server
//! syncs shortly after start and then every `VITALS_OURA_SYNC_INTERVAL_SECS`
//! (default 6h), on its own DB connection so API reads never wait on the
//! network. Each run's outcome lands in `oura_sync_log` (`status` shows it).
//! The API itself stays GET-only.

use std::time::Duration;

use crate::state::Config;

const DEFAULT_INTERVAL_SECS: u64 = 6 * 3600;
const FIRST_DELAY_SECS: u64 = 30;

/// Resolve the Oura config at startup. A broken config is fatal, like a bad
/// token file: better to fail at boot than to quietly never sync.
pub fn start(cfg: &Config) {
    let oura = match vitals_oura::Config::from_env(&cfg.db_path) {
        Ok(Some(o)) => o,
        Ok(None) => {
            tracing::info!("oura: not configured; no outbound calls");
            return;
        }
        Err(e) => {
            eprintln!("vitals-server: oura: {e}");
            std::process::exit(1);
        }
    };
    let every = std::env::var("VITALS_OURA_SYNC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s >= 300)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    tracing::info!(every_secs = every, "oura: background sync on");
    let cfg = cfg.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(FIRST_DELAY_SECS)).await;
        loop {
            run_once(&cfg, &oura).await;
            tokio::time::sleep(Duration::from_secs(every)).await;
        }
    });
}

async fn run_once(cfg: &Config, oura: &vitals_oura::Config) {
    if !vitals_oura::is_connected(oura) {
        tracing::warn!("oura: not connected yet; run `vitals oura auth` on this host");
        return;
    }
    let (cfg, oura) = (cfg.clone(), oura.clone());
    let res = tokio::task::spawn_blocking(move || {
        let store = crate::open_db(&cfg)?;
        vitals_oura::sync(&oura, &store, None)
    })
    .await;
    match res {
        Ok(Ok(r)) => tracing::info!(
            added = r.added,
            updated = r.updated,
            removed = r.removed,
            ms = r.elapsed_ms,
            "oura: synced"
        ),
        Ok(Err(e)) => tracing::error!(error = %e, "oura: sync failed"),
        Err(e) => tracing::error!(error = %e, "oura: sync task panicked"),
    }
}

/// The page Oura redirects the browser to after approval. Static: it never
/// reads, echoes or logs the query string (it holds the authorization code),
/// and it is mounted outside the request-logging layer.
pub async fn callback() -> impl axum::response::IntoResponse {
    (
        [
            (axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (axum::http::header::CACHE_CONTROL, "no-store"),
            (axum::http::header::REFERRER_POLICY, "no-referrer"),
        ],
        CALLBACK_HTML,
    )
}

const CALLBACK_HTML: &str = "<!doctype html><meta charset=utf-8>\
<meta name=viewport content='width=device-width,initial-scale=1'>\
<title>vitals: Oura</title>\
<body style='font:16px/1.5 system-ui,sans-serif;max-width:36em;margin:3em auto;padding:0 1em'>\
<h1>Almost done</h1>\
<p>Copy the full address of this page from the address bar and paste it into \
the waiting <code>vitals oura auth</code> prompt.</p>\
<p>Then close this tab. The address works only once.</p>";
