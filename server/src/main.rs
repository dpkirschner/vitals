//! vitals server: read-only JSON API over the Apple Health archive, for a
//! headless always-on box (e.g. a Mac mini), fronted by `tailscale serve` for
//! HTTPS on the tailnet. Binds loopback by default — Tailscale is the network
//! boundary; optional bearer tokens add an app-level check.
//!
//! Headless posture: all config and secrets resolve from env / 0600 files —
//! never the keychain (locked without a GUI session) and never the DB.

mod routes;
mod state;

use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::Router;
use std::sync::{Arc, Mutex};
use vitals_core::Store;

use state::{AppState, Config};

fn open_db(cfg: &Config) -> Result<Store, String> {
    if let Some(dir) = std::path::Path::new(&cfg.db_path).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    #[cfg(feature = "encryption")]
    {
        let key = resolve_db_key()?;
        Store::open_encrypted(&cfg.db_path, &key)
            .map_err(|e| format!("open encrypted db {}: {e}", cfg.db_path))
    }
    #[cfg(not(feature = "encryption"))]
    {
        Store::open(&cfg.db_path).map_err(|e| format!("open db {}: {e}", cfg.db_path))
    }
}

/// SQLCipher key for the encrypted build: `VITALS_DB_KEY` env, else
/// `VITALS_DB_KEY_FILE` (0600). No keychain on a headless box.
#[cfg(feature = "encryption")]
fn resolve_db_key() -> Result<String, String> {
    if let Ok(k) = std::env::var("VITALS_DB_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    let path = std::env::var("VITALS_DB_KEY_FILE")
        .ok()
        .filter(|p| !p.is_empty())
        .ok_or("encryption needs a key: set VITALS_DB_KEY or VITALS_DB_KEY_FILE (0600 file)")?;
    vitals_core::secrets::read_secret_file(&path)
}

/// Compare without an early exit on the first differing byte, so a token can't
/// be recovered byte-by-byte from response timing. Length is not secret (both
/// tokens are fixed-width random hex), so returning early on a length mismatch
/// is fine.
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Paths the read-only token may not read even with GET: raw GPS route
/// points are precise location history (INVARIANTS #4). Matches with or
/// without the `/api` prefix, since nested routers see the stripped path.
fn full_token_only(path: &str) -> bool {
    let p = path.trim_end_matches('/');
    p.contains("/workouts/") && p.ends_with("/route")
}

/// Optional bearer check on /api/*, in two tiers:
///
/// - `VITALS_API_TOKEN`    — full access.
/// - `VITALS_API_TOKEN_RO` — GET only, and not raw route points; 403 otherwise.
///
/// With **neither** set this layer passes everything through — the tailnet is
/// the boundary, which is the zero-config single-user default. Setting either
/// one flips the whole /api surface to deny-by-default.
///
/// Like outflow, the read-only tier keys off the HTTP method rather than a
/// route allowlist, so a future mutating route is denied to it the day it
/// lands. The one addition is `full_token_only` for location data.
async fn require_bearer(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let (full, ro) = (&state.cfg.api_token, &state.cfg.api_token_ro);
    if full.is_none() && ro.is_none() {
        return Ok(next.run(req).await);
    }

    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");

    if full.as_deref().is_some_and(|t| ct_eq(presented, t)) {
        return Ok(next.run(req).await);
    }
    if ro.as_deref().is_some_and(|t| ct_eq(presented, t)) {
        if req.method() == Method::GET && !full_token_only(req.uri().path()) {
            return Ok(next.run(req).await);
        }
        return Err(StatusCode::FORBIDDEN);
    }
    Err(StatusCode::UNAUTHORIZED)
}

fn app(state: AppState) -> Router {
    let api = routes::api_router()
        // Unknown /api paths 404 as JSON, never an empty body.
        .fallback(|| async {
            (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({ "error": "no such API route" })),
            )
        })
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_bearer,
        ));
    Router::new()
        .nest("/api", api)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "vitals_server=info,tower_http=info".into()),
        )
        .init();

    let cfg = Config::from_env();
    let store = match open_db(&cfg) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("vitals-server: {e}");
            std::process::exit(1);
        }
    };
    let count = store.count_records().unwrap_or(-1);
    tracing::info!(db = %cfg.db_path, records = count, "opened database");
    if cfg.api_token.is_none() && cfg.api_token_ro.is_none() {
        tracing::warn!("no API tokens configured: /api is open to anyone who can reach the port");
    }

    let state = AppState {
        store: Arc::new(Mutex::new(store)),
        cfg: Arc::new(cfg.clone()),
    };

    let listener = match tokio::net::TcpListener::bind(&cfg.listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("vitals-server: bind {}: {e}", cfg.listen);
            std::process::exit(1);
        }
    };
    tracing::info!(listen = %cfg.listen, "vitals server up");
    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server error");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::{delete, get, post};
    use tower::ServiceExt; // for `oneshot`

    const FULL: &str = "fulltoken0000000000000000000000f";
    const RO: &str = "readonlytoken00000000000000000ro";

    fn state(store: Store, full: Option<&str>, ro: Option<&str>) -> AppState {
        AppState {
            store: Arc::new(Mutex::new(store)),
            cfg: Arc::new(Config {
                db_path: ":memory:".into(),
                listen: "127.0.0.1:0".into(),
                api_token: full.map(Into::into),
                api_token_ro: ro.map(Into::into),
            }),
        }
    }

    /// A stand-in for the real /api surface: one GET, one POST, one DELETE,
    /// and a route-points path, all behind the same layer main() applies.
    fn stub(full: Option<&str>, ro: Option<&str>) -> Router {
        let st = state(Store::open_in_memory().unwrap(), full, ro);
        Router::new()
            .route("/read", get(|| async { "ok" }))
            .route("/write", post(|| async { "ok" }))
            .route("/remove", delete(|| async { "ok" }))
            .route("/workouts/{id}", get(|| async { "ok" }))
            .route("/workouts/{id}/route", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(st, require_bearer))
    }

    async fn call(
        app: Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
    ) -> (StatusCode, String) {
        let mut b = HttpRequest::builder().method(method).uri(uri);
        if let Some(t) = token {
            b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let resp = app.oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn status(app: Router, method: Method, uri: &str, token: Option<&str>) -> StatusCode {
        call(app, method, uri, token).await.0
    }

    /// Both tiers configured — the production shape.
    async fn tiered(method: Method, uri: &str, token: &str) -> StatusCode {
        status(stub(Some(FULL), Some(RO)), method, uri, Some(token)).await
    }

    #[tokio::test]
    async fn no_tokens_configured_passes_everything() {
        assert_eq!(
            status(stub(None, None), Method::GET, "/read", None).await,
            StatusCode::OK
        );
        assert_eq!(
            status(stub(None, None), Method::POST, "/write", None).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn full_token_required_once_configured() {
        let s = |m, u, t| status(stub(Some(FULL), None), m, u, t);
        assert_eq!(
            s(Method::GET, "/read", None).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            s(Method::GET, "/read", Some("wrong")).await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(s(Method::GET, "/read", Some(FULL)).await, StatusCode::OK);
        assert_eq!(s(Method::POST, "/write", Some(FULL)).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn ro_token_reads_but_cannot_mutate() {
        assert_eq!(tiered(Method::GET, "/read", RO).await, StatusCode::OK);
        assert_eq!(
            tiered(Method::POST, "/write", RO).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            tiered(Method::DELETE, "/remove", RO).await,
            StatusCode::FORBIDDEN
        );
    }

    // INVARIANTS #4: the RO tier sees workout summaries but never raw GPS.
    #[tokio::test]
    async fn ro_token_cannot_read_route_points() {
        assert_eq!(tiered(Method::GET, "/workouts/7", RO).await, StatusCode::OK);
        assert_eq!(
            tiered(Method::GET, "/workouts/7/route", RO).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            tiered(Method::GET, "/workouts/7/route/", RO).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            tiered(Method::GET, "/workouts/7/route", FULL).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn full_token_still_mutates_alongside_ro() {
        assert_eq!(tiered(Method::POST, "/write", FULL).await, StatusCode::OK);
        assert_eq!(
            tiered(Method::DELETE, "/remove", FULL).await,
            StatusCode::OK
        );
        assert_eq!(
            tiered(Method::GET, "/read", "nope").await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn ro_token_alone_denies_writes_to_all() {
        let s = |m, u, t| status(stub(None, Some(RO)), m, u, t);
        assert_eq!(s(Method::GET, "/read", Some(RO)).await, StatusCode::OK);
        assert_eq!(
            s(Method::POST, "/write", Some(RO)).await,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            s(Method::GET, "/read", None).await,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn ct_eq_matches_only_identical_strings() {
        assert!(ct_eq(FULL, FULL));
        assert!(!ct_eq(FULL, RO));
        assert!(!ct_eq("abc", "abcd"));
        assert!(!ct_eq("abc", "abd"));
        assert!(ct_eq("", ""));
    }

    #[test]
    fn full_token_only_paths() {
        assert!(full_token_only("/workouts/1/route"));
        assert!(full_token_only("/api/workouts/1/route"));
        assert!(!full_token_only("/workouts/1"));
        assert!(!full_token_only("/workouts"));
    }

    /// The real router over the synthetic fixture.
    fn real(full: Option<&str>, ro: Option<&str>) -> Router {
        let dir = std::env::temp_dir().join(format!(
            "vitals-server-{}-{}",
            std::process::id(),
            std::thread::current()
                .name()
                .unwrap_or("t")
                .replace("::", "_")
        ));
        std::fs::create_dir_all(dir.join("workout-routes")).unwrap();
        let xml = dir.join("export.xml");
        std::fs::write(&xml, include_str!("../../examples/export-sample.xml")).unwrap();
        std::fs::write(
            dir.join("workout-routes/route_sample.gpx"),
            include_str!("../../examples/route-sample.gpx"),
        )
        .unwrap();
        let store = Store::open_in_memory().unwrap();
        vitals_core::import_path(&store, xml.to_str().unwrap(), |_| {}).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        app(state(store, full, ro))
    }

    #[tokio::test]
    async fn real_routes_answer_json() {
        let (code, body) = call(
            real(None, None),
            Method::GET,
            "/api/daily?since=2026-03-01&until=2026-03-01&metrics=steps",
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(
            body,
            r#"[{"date":"2026-03-01","metrics":{"steps":3500.0}}]"#
        );

        let (code, body) = call(
            real(None, None),
            Method::GET,
            "/api/series?type=StepCount&bucket=month",
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{body}");
        assert!(
            body.contains(r#""bucket":"2026-03-01","value":4500.0"#),
            "{body}"
        );

        let (code, body) = call(
            real(None, None),
            Method::GET,
            "/api/workouts?activity=running",
            None,
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains(r#""has_route":true"#), "{body}");
    }

    #[tokio::test]
    async fn real_routes_errors() {
        let (code, body) = call(real(None, None), Method::GET, "/api/nope", None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(body.contains("no such API route"));

        let (code, body) = call(
            real(None, None),
            Method::GET,
            "/api/series?type=Bogus",
            None,
        )
        .await;
        assert_eq!(code, StatusCode::BAD_REQUEST);
        assert!(body.contains("unknown type"), "{body}");

        let (code, _) = call(real(None, None), Method::GET, "/api/workouts/999", None).await;
        assert_eq!(code, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn real_route_points_need_full_token() {
        let (code, _) = call(
            real(Some(FULL), Some(RO)),
            Method::GET,
            "/api/workouts/1/route",
            Some(RO),
        )
        .await;
        assert_eq!(code, StatusCode::FORBIDDEN);
        let (code, body) = call(
            real(Some(FULL), Some(RO)),
            Method::GET,
            "/api/workouts/1/route",
            Some(FULL),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
        assert!(body.contains(r#""lat":47.6"#), "{body}");
        let (code, _) = call(
            real(Some(FULL), Some(RO)),
            Method::GET,
            "/api/workouts/1",
            Some(RO),
        )
        .await;
        assert_eq!(code, StatusCode::OK);
    }
}
