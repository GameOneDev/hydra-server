use crate::auth::CurrentUser;
use crate::error::ApiResult;
use crate::state::AppState;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};

/// GET /profile/members/{userId} — whether that user has ever signed in to
/// this server, so launchers can badge profiles of people on the same
/// custom server. Any authenticated user may ask; membership isn't a
/// secret between people already sharing the server.
pub async fn lookup(
    State(state): State<AppState>,
    _viewer: CurrentUser,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM users WHERE id = ?")
        .bind(&user_id)
        .fetch_optional(&state.pool)
        .await?;

    Ok(Json(json!({ "isMember": exists.is_some() })))
}
