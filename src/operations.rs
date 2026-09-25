use std::sync::Arc;

use axum::{
    extract::{Path as UrlPath, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

use crate::app::App;
use crate::auth::authorized;
use crate::errors::{internal, ApiError, ApiResult};
use crate::store::now_ms;

const TERMINAL_STATES: [&str; 3] = ["completed", "failed", "cancelled"];

pub fn update_operation(
    app: &App,
    id: &str,
    state: &str,
    error: Option<&str>,
    bytes: u64,
) -> ApiResult<()> {
    let db = app.db.lock().map_err(internal)?;
    let now = now_ms();
    if TERMINAL_STATES.contains(&state) {
        db.execute(
            "UPDATE operations SET state=?2, error=?3, progress_bytes=?4, updated_at=?5, finished_at=?5 WHERE id=?1 AND state <> 'cancelled'",
            params![id, state, error, bytes, now],
        )
        .map_err(internal)?;
    } else {
        db.execute(
            "UPDATE operations SET state=?2, error=?3, progress_bytes=?4, updated_at=?5 WHERE id=?1 AND state <> 'cancelled'",
            params![id, state, error, bytes, now],
        )
        .map_err(internal)?;
    }
    Ok(())
}

pub fn operation_state(app: &App, id: &str) -> ApiResult<Option<String>> {
    let db = app.db.lock().map_err(internal)?;
    db.query_row(
        "SELECT state FROM operations WHERE id=?1",
        params![id],
        |row| row.get(0),
    )
    .optional()
    .map_err(internal)
}

pub async fn operation(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    let db = app.db.lock().map_err(internal)?;
    let record = db
        .query_row(
            "SELECT model_id, kind, state, error, progress_bytes, total_bytes, created_at, updated_at, finished_at FROM operations WHERE id=?1",
            params![id],
            |row| {
                Ok(json!({
                    "id":id,
                    "model_id":row.get::<_, String>(0)?,
                    "kind":row.get::<_, String>(1)?,
                    "state":row.get::<_, String>(2)?,
                    "error":row.get::<_, Option<String>>(3)?,
                    "progress_bytes":row.get::<_, u64>(4)?,
                    "total_bytes":row.get::<_, u64>(5)?,
                    "created_at":row.get::<_, Option<i64>>(6)?,
                    "updated_at":row.get::<_, Option<i64>>(7)?,
                    "finished_at":row.get::<_, Option<i64>>(8)?
                }))
            },
        )
        .optional()
        .map_err(internal)?
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "operation_not_found", "Unknown operation"))?;
    Ok(Json(record))
}

pub async fn cancel_operation(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    authorized(&headers, &app)?;
    match operation_state(&app, &id)?.as_deref() {
        Some("queued" | "running") => {
            let db = app.db.lock().map_err(internal)?;
            let changed = db.execute(
                "UPDATE operations SET state='cancelled' WHERE id=?1 AND state IN ('queued','running')",
                params![id],
            )
            .map_err(internal)?;
            if changed == 0 {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "operation_finished",
                    "Operation already finished",
                ));
            }
            Ok(Json(json!({"id":id,"state":"cancelled"})))
        }
        Some(_) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "operation_finished",
            "Operation already finished",
        )),
        None => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "operation_not_found",
            "Unknown operation",
        )),
    }
}
