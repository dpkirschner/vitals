//! JSON API routes. Every route is a GET over the archive; imports happen via
//! the CLI on the host. Shapes are the serde form of `vitals_core::query`
//! types — identical to the CLI's local `--json` output.

use axum::extract::{Path, Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use vitals_core::query::{
    self, ActivitySummary, DailyRow, DateRange, Night, Page, RecordQuery, RecordRow, RoutePoint,
    Series, SeriesQuery, Status, TypeInfo, WorkoutDetail, WorkoutQuery, WorkoutRow,
};

use crate::state::{with_store, ApiError, ApiResult, AppState};

#[derive(Deserialize)]
struct StatusParams {
    limit: Option<usize>,
}

async fn status(State(state): State<AppState>, Query(p): Query<StatusParams>) -> ApiResult<Status> {
    let limit = p.limit.unwrap_or(5).min(100);
    with_store(&state, move |s| query::status(s, limit))
        .await
        .map(Json)
}

async fn types(State(state): State<AppState>) -> ApiResult<Vec<TypeInfo>> {
    with_store(&state, query::types).await.map(Json)
}

/// The metric names `/daily` understands.
async fn metrics() -> Json<&'static [&'static str]> {
    Json(vitals_core::rollup::METRICS)
}

async fn records(
    State(state): State<AppState>,
    Query(q): Query<RecordQuery>,
) -> ApiResult<Page<RecordRow>> {
    with_store(&state, move |s| query::records(s, &q))
        .await
        .map(Json)
}

async fn series(State(state): State<AppState>, Query(q): Query<SeriesQuery>) -> ApiResult<Series> {
    with_store(&state, move |s| query::series(s, &q))
        .await
        .map(Json)
}

#[derive(Deserialize)]
struct DailyParams {
    since: Option<String>,
    until: Option<String>,
    /// Comma-separated metric names; default all.
    metrics: Option<String>,
}

async fn daily(
    State(state): State<AppState>,
    Query(p): Query<DailyParams>,
) -> ApiResult<Vec<DailyRow>> {
    let range = DateRange {
        since: p.since,
        until: p.until,
    };
    let metrics: Vec<String> = p
        .metrics
        .map(|m| {
            m.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    with_store(&state, move |s| query::daily(s, &range, &metrics))
        .await
        .map(Json)
}

async fn sleep(State(state): State<AppState>, Query(r): Query<DateRange>) -> ApiResult<Vec<Night>> {
    with_store(&state, move |s| query::sleep(s, &r))
        .await
        .map(Json)
}

async fn activity(
    State(state): State<AppState>,
    Query(r): Query<DateRange>,
) -> ApiResult<Vec<ActivitySummary>> {
    with_store(&state, move |s| query::activity(s, &r))
        .await
        .map(Json)
}

async fn workouts(
    State(state): State<AppState>,
    Query(q): Query<WorkoutQuery>,
) -> ApiResult<Vec<WorkoutRow>> {
    with_store(&state, move |s| query::workouts(s, &q))
        .await
        .map(Json)
}

async fn workout(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<WorkoutDetail> {
    with_store(&state, move |s| query::workout(s, id))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no workout {id}")))
}

/// Raw GPS points. Full token only — enforced in `require_bearer`.
async fn route(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<Vec<RoutePoint>> {
    with_store(&state, move |s| query::route(s, id))
        .await?
        .map(Json)
        .ok_or_else(|| ApiError::not_found(format!("no workout {id}")))
}

pub fn api_router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/types", get(types))
        .route("/metrics", get(metrics))
        .route("/records", get(records))
        .route("/series", get(series))
        .route("/daily", get(daily))
        .route("/sleep", get(sleep))
        .route("/activity", get(activity))
        .route("/workouts", get(workouts))
        .route("/workouts/{id}", get(workout))
        .route("/workouts/{id}/route", get(route))
}
