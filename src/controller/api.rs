use axum::extract::{Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, timeout};

use crate::db::{
    CommandRecord, CommentRecord, CreateCommandRequest, CreateCommentRequest, SessionSummary,
    UpdateCommentRequest, create_command, create_comment, get_comment, list_commands_for_comment,
    list_comments, list_sessions, mark_command_done, update_comment,
};
use crate::service::ai::{build_command_prompt, detect_ai_client, execute_ai_command};
use crate::{AppState, now_ms_i64};

#[derive(Debug, Clone, Serialize)]
pub struct SessionsResponse {
    pub project_id: i64,
    pub current_session_id: i64,
    pub items: Vec<SessionSummary>,
}

#[derive(Debug, Deserialize)]
pub struct CommentQuery {
    pub file: Option<String>,
    pub include_resolved: Option<bool>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CommandQuery {
    pub comment_id: Option<i64>,
    pub limit: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ClientInfo {
    pub client: String,
    pub source: String,
}

pub async fn api_state(State(state): State<AppState>) -> impl IntoResponse {
    let mut items = state
        .changes
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    items.sort_by(|a, b| a.path.cmp(&b.path));
    Json(items)
}

pub async fn api_sessions(State(state): State<AppState>) -> impl IntoResponse {
    match list_sessions(&state.state_db_path, state.project_id, 25) {
        Ok(items) => Json(SessionsResponse {
            project_id: state.project_id,
            current_session_id: state.session_id,
            items,
        })
        .into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

pub async fn api_list_comments(
    State(state): State<AppState>,
    Query(query): Query<CommentQuery>,
) -> std::result::Result<Json<Vec<CommentRecord>>, (StatusCode, String)> {
    let include_resolved = query.include_resolved.unwrap_or(true);
    let limit = query.limit.unwrap_or(200).clamp(1, 1000);
    list_comments(
        &state.state_db_path,
        state.session_id,
        query.file.as_deref(),
        include_resolved,
        limit,
    )
    .map(Json)
    .map_err(internal_error)
}

pub async fn api_create_comment(
    State(state): State<AppState>,
    Json(payload): Json<CreateCommentRequest>,
) -> std::result::Result<Json<CommentRecord>, (StatusCode, String)> {
    create_comment(&state.state_db_path, state.session_id, &payload, now_ms_i64())
        .map(Json)
        .map_err(bad_or_internal_error)
}

pub async fn api_update_comment(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<i64>,
    Json(payload): Json<UpdateCommentRequest>,
) -> std::result::Result<impl IntoResponse, (StatusCode, String)> {
    match update_comment(
        &state.state_db_path,
        state.session_id,
        id,
        &payload,
        now_ms_i64(),
    )
    .map_err(bad_or_internal_error)?
    {
        Some(item) => Ok((StatusCode::OK, Json(item)).into_response()),
        None => Ok((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "comment not found" })),
        )
            .into_response()),
    }
}

pub async fn api_list_commands(
    State(state): State<AppState>,
    Query(query): Query<CommandQuery>,
) -> std::result::Result<Json<Vec<CommandRecord>>, (StatusCode, String)> {
    let Some(comment_id) = query.comment_id else {
        return Err((
            StatusCode::BAD_REQUEST,
            "comment_id query parameter is required".to_string(),
        ));
    };

    let limit = query.limit.unwrap_or(20).clamp(1, 100);
    list_commands_for_comment(&state.state_db_path, state.session_id, comment_id, limit)
        .map(Json)
        .map_err(internal_error)
}

pub async fn api_create_command(
    State(state): State<AppState>,
    Json(payload): Json<CreateCommandRequest>,
) -> std::result::Result<Json<CommandRecord>, (StatusCode, String)> {
    let comment = get_comment(&state.state_db_path, state.session_id, payload.comment_id)
        .map_err(internal_error)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "comment not found".to_string()))?;

    let (tool, _) = detect_ai_client();
    let prompt = build_command_prompt(&comment);
    let command = create_command(
        &state.state_db_path,
        state.session_id,
        comment.id,
        &tool,
        &prompt,
        now_ms_i64(),
    )
    .map_err(internal_error)?;

    let permit = state
        .command_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "command queue is full; try again shortly".to_string(),
            )
        })?;

    let timeout_secs = state.command_timeout_secs;
    let db_path = state.state_db_path.clone();
    let root = state.root.clone();
    let command_id = command.id;
    let tool = command.tool.clone();
    let prompt = command.prompt.clone();

    tokio::spawn(async move {
        let _permit = permit;
        let result = timeout(
            Duration::from_secs(timeout_secs.max(1)),
            execute_ai_command(db_path.clone(), root, command_id, tool, prompt),
        )
        .await;

        if result.is_err() {
            let _ = mark_command_done(
                &db_path,
                command_id,
                "failed",
                None,
                Some("command timed out"),
                now_ms_i64(),
            );
        }
    });

    Ok(Json(command))
}

pub async fn api_client() -> Json<ClientInfo> {
    let (client, source) = detect_ai_client();
    Json(ClientInfo { client, source })
}

fn internal_error(err: anyhow::Error) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
}

fn bad_or_internal_error(err: anyhow::Error) -> (StatusCode, String) {
    let msg = err.to_string();
    if msg.contains("cannot be empty")
        || msg.contains("must be 'open' or 'resolved'")
        || msg.contains("invalid line selection range")
    {
        (StatusCode::BAD_REQUEST, msg)
    } else {
        (StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}
