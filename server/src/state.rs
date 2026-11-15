//! Shared server state: the Store behind a mutex (rusqlite `Connection` is
//! `Send` but not `Sync`), bridged onto the async runtime with
//! `spawn_blocking`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::{Arc, Mutex};
use vitals_core::Store;

#[derive(Clone)]
pub struct Config {
    pub db_path: String,
    pub listen: String,
    /// Optional bearer token required on /api/* (tailnet is the main boundary).
    /// Full access, including raw GPS routes.
    pub api_token: Option<String>,
    /// Optional second bearer token with read-only access: GET /api/* except
    /// raw route points (INVARIANTS #4). For agents and scripts.
    pub api_token_ro: Option<String>,
    /// Origins besides vitals' own that may frame its pages
    /// (`VITALS_FRAME_ANCESTORS`, space-separated), e.g. the agent-avatar
    /// screen. Empty = only `'self'`. Validated at startup.
    pub frame_ancestors: Vec<String>,
}

impl Config {
    pub fn from_env() -> Config {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());

        // A secret from `K`, else from a 0600 file named by `K_FILE`. Prefer
        // the file form in production: /Library/LaunchDaemons plists are
        // world-readable, so anything in the env block is legible to every
        // account on the box.
        //
        // A `K_FILE` that is set but unreadable is fatal, never None: falling
        // through would leave the token unset, which silently opens all of
        // /api/* to anyone who can reach the port. Fail closed, loudly.
        let secret = |k: &str| -> Option<String> {
            if let Some(v) = env(k) {
                return Some(v);
            }
            let path = env(&format!("{k}_FILE"))?;
            match vitals_core::secrets::read_secret_file(&path) {
                Ok(v) => Some(v),
                Err(e) => {
                    eprintln!("vitals-server: {k}_FILE: {e}");
                    std::process::exit(1);
                }
            }
        };

        let data_dir = env("VITALS_DATA_DIR").unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            format!("{home}/Library/Application Support/vitals")
        });
        Config {
            db_path: env("VITALS_DB").unwrap_or_else(|| format!("{data_dir}/vitals.db")),
            listen: env("VITALS_LISTEN").unwrap_or_else(|| "127.0.0.1:8090".into()),
            api_token: secret("VITALS_API_TOKEN"),
            api_token_ro: secret("VITALS_API_TOKEN_RO"),
            // A bad entry is fatal: it would either widen who may frame the
            // pages or corrupt the header. Fail at boot, not in a browser.
            frame_ancestors: match parse_frame_ancestors(
                &env("VITALS_FRAME_ANCESTORS").unwrap_or_default(),
            ) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("vitals-server: VITALS_FRAME_ANCESTORS: {e}");
                    std::process::exit(1);
                }
            },
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub cfg: Arc<Config>,
}

/// API errors surface as `{ "error": "..." }`.
pub struct ApiError(pub StatusCode, pub String);

impl ApiError {
    pub fn internal(msg: impl std::fmt::Display) -> ApiError {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg.to_string())
    }

    pub fn not_found(msg: impl std::fmt::Display) -> ApiError {
        ApiError(StatusCode::NOT_FOUND, msg.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

/// Split `VITALS_FRAME_ANCESTORS` into origins, each exactly
/// `http(s)://host[:port]` (one trailing `/` tolerated and dropped). No
/// wildcards, paths, quotes or `;`: the value goes straight into a CSP
/// header. Same rules as outflow's `OUTFLOW_FRAME_ANCESTORS`.
pub fn parse_frame_ancestors(raw: &str) -> Result<Vec<String>, String> {
    raw.split_whitespace()
        .map(|entry| {
            let origin = entry.strip_suffix('/').unwrap_or(entry);
            let bad = || format!("{entry:?} is not an origin like https://host[:port]");
            let rest = origin
                .strip_prefix("https://")
                .or_else(|| origin.strip_prefix("http://"))
                .ok_or_else(bad)?;
            let (host, port) = match rest.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (rest, None),
            };
            let host_ok = !host.is_empty()
                && host
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
            let port_ok = port.is_none_or(|p| {
                !p.is_empty() && p.len() <= 5 && p.chars().all(|c| c.is_ascii_digit())
            });
            if host_ok && port_ok {
                Ok(origin.to_string())
            } else {
                Err(bad())
            }
        })
        .collect()
}

/// The `frame-ancestors` directive: vitals itself plus the configured
/// origins.
pub fn frame_ancestors_csp(origins: &[String]) -> String {
    std::iter::once("frame-ancestors 'self'".to_string())
        .chain(origins.iter().cloned())
        .collect::<Vec<_>>()
        .join(" ")
}

pub type ApiResult<T> = Result<Json<T>, ApiError>;

/// Run a closure against the Store on the blocking pool. Core query errors
/// are prefixed `db:` when the database failed (→ 500); every other error is
/// a validation message about the request (unknown type, bad bucket, …) and
/// maps to 400.
pub async fn with_store<T, F>(state: &AppState, f: F) -> Result<T, ApiError>
where
    F: FnOnce(&Store) -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || {
        let guard = store
            .lock()
            .map_err(|e| format!("db: store lock poisoned: {e}"))?;
        f(&guard)
    })
    .await
    .map_err(|e| ApiError::internal(format!("blocking task: {e}")))?
    .map_err(|e| {
        if e.starts_with("db:") {
            ApiError::internal(e)
        } else {
            ApiError(StatusCode::BAD_REQUEST, e)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ancestors_accepts_origins() {
        assert_eq!(parse_frame_ancestors("").unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_frame_ancestors(" https://a.example.net/  http://localhost:8077 ").unwrap(),
            vec!["https://a.example.net", "http://localhost:8077"]
        );
    }

    #[test]
    fn frame_ancestors_rejects_non_origins() {
        for bad in [
            "*",
            "a.example",
            "https://",
            "https://*.example",
            "https://a.example/path",
            "https://a.example;script-src",
            "'none'",
            "https://a.example:",
            "https://a.example:80x",
            "ftp://a.example",
        ] {
            assert!(parse_frame_ancestors(bad).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn csp_always_starts_with_self() {
        assert_eq!(frame_ancestors_csp(&[]), "frame-ancestors 'self'");
        assert_eq!(
            frame_ancestors_csp(&["https://a.example".into()]),
            "frame-ancestors 'self' https://a.example"
        );
    }
}
