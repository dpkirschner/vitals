//! The embed views: chrome-less pages another app frames (agent-avatar
//! shows them as screens). Static files compiled into the binary; the data
//! comes from `GET /api/chart` behind the usual bearer check, so the pages
//! themselves need no token and read nothing from the archive.

use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::state::{frame_ancestors_csp, AppState};

const HTML: &str = include_str!("../embed/chart.html");
const JS: &str = include_str!("../embed/chart.js");
const CSS: &str = include_str!("../embed/embed.css");

/// The page's own policy: scripts, styles and fetches from vitals only,
/// nothing inline, plus who may frame it.
pub fn page_csp(origins: &[String]) -> String {
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
         img-src 'self'; base-uri 'none'; form-action 'none'; {}",
        frame_ancestors_csp(origins)
    )
}

fn file(content_type: &'static str, body: &'static str, csp: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            // A new release must replace the page at the next load.
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, csp),
        ],
        body,
    )
        .into_response()
}

/// `/embed/chart`, and every other `/embed/` path: the page shows "No such
/// embed view" for those itself.
async fn page(State(state): State<AppState>) -> Response {
    file(
        "text/html; charset=utf-8",
        HTML,
        page_csp(&state.cfg.frame_ancestors),
    )
}

async fn script(State(state): State<AppState>) -> Response {
    file(
        "text/javascript; charset=utf-8",
        JS,
        page_csp(&state.cfg.frame_ancestors),
    )
}

async fn style(State(state): State<AppState>) -> Response {
    file(
        "text/css; charset=utf-8",
        CSS,
        page_csp(&state.cfg.frame_ancestors),
    )
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/embed/chart.js", get(script))
        .route("/embed/embed.css", get(style))
        .route("/embed/{*rest}", get(page))
}
