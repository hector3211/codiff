use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::{BigInt, Integer, Nullable, Text};
use diesel::QueryableByName;
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use serde::{Deserialize, Serialize};

pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

#[derive(Debug, Clone, Serialize, QueryableByName)]
pub struct SessionSummary {
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    #[diesel(sql_type = Text)]
    pub status: String,
    #[diesel(sql_type = BigInt)]
    pub started_at_ms: i64,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub ended_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, QueryableByName)]
pub struct CommentRecord {
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    #[diesel(sql_type = BigInt)]
    pub session_id: i64,
    #[diesel(sql_type = Text)]
    pub file_path: String,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub line_start: Option<i64>,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub line_end: Option<i64>,
    #[diesel(sql_type = Nullable<Text>)]
    pub selected_text: Option<String>,
    #[diesel(sql_type = Text)]
    pub body: String,
    #[diesel(sql_type = Text)]
    pub status: String,
    #[diesel(sql_type = BigInt)]
    pub created_at_ms: i64,
    #[diesel(sql_type = BigInt)]
    pub updated_at_ms: i64,
}

#[derive(Debug, Deserialize)]
pub struct CreateCommentRequest {
    pub file_path: String,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub selected_text: Option<String>,
    pub body: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCommentRequest {
    pub body: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, QueryableByName)]
pub struct CommandRecord {
    #[diesel(sql_type = BigInt)]
    pub id: i64,
    #[diesel(sql_type = BigInt)]
    pub session_id: i64,
    #[diesel(sql_type = BigInt)]
    pub comment_id: i64,
    #[diesel(sql_type = Text)]
    pub tool: String,
    #[diesel(sql_type = Text)]
    pub prompt: String,
    #[diesel(sql_type = Text)]
    pub status: String,
    #[diesel(sql_type = Nullable<Text>)]
    pub output_text: Option<String>,
    #[diesel(sql_type = Nullable<Text>)]
    pub error_text: Option<String>,
    #[diesel(sql_type = BigInt)]
    pub created_at_ms: i64,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub started_at_ms: Option<i64>,
    #[diesel(sql_type = Nullable<BigInt>)]
    pub ended_at_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCommandRequest {
    pub comment_id: i64,
}

#[derive(QueryableByName)]
struct LastInsertId {
    #[diesel(sql_type = BigInt)]
    id: i64,
}

#[derive(QueryableByName)]
struct ColumnInfo {
    #[diesel(sql_type = Text)]
    name: String,
}

pub fn init_state_db(db_path: &Path) -> Result<()> {
    let mut conn = establish_connection(db_path)?;
    conn.batch_execute(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        ",
    )
    .context("failed to apply sqlite pragmas")?;

    conn.run_pending_migrations(MIGRATIONS)
        .map_err(|e| anyhow::anyhow!("failed to run database migrations: {e}"))?;
    ensure_comments_columns(&mut conn)?;
    Ok(())
}

pub fn start_session(db_path: &Path, root: &Path, now_ms: i64) -> Result<(i64, i64)> {
    let mut conn = establish_connection(db_path)?;
    let path = root.display().to_string();
    let display = root
        .file_name()
        .map(|v| v.to_string_lossy().to_string())
        .unwrap_or_else(|| path.clone());

    conn.immediate_transaction::<_, anyhow::Error, _>(|conn| {
        sql_query(
            "
            INSERT INTO projects (canonical_path, display_name, created_at_ms, last_opened_at_ms)
            VALUES (?1, ?2, ?3, ?3)
            ON CONFLICT(canonical_path)
            DO UPDATE SET
                display_name = excluded.display_name,
                last_opened_at_ms = excluded.last_opened_at_ms
            ",
        )
        .bind::<Text, _>(&path)
        .bind::<Text, _>(&display)
        .bind::<BigInt, _>(now_ms)
        .execute(conn)
        .context("failed to upsert project")?;

        let project_id = sql_query("SELECT id FROM projects WHERE canonical_path = ?1")
            .bind::<Text, _>(&path)
            .load::<LastInsertId>(conn)
            .context("failed to query project id")?
            .into_iter()
            .next()
            .map(|row| row.id)
            .context("project row not found after upsert")?;

        sql_query(
            "UPDATE sessions SET status = 'aborted', ended_at_ms = ?1 WHERE project_id = ?2 AND status = 'active'",
        )
        .bind::<BigInt, _>(now_ms)
        .bind::<BigInt, _>(project_id)
        .execute(conn)
        .context("failed to close previous active session")?;

        sql_query("INSERT INTO sessions (project_id, started_at_ms, status) VALUES (?1, ?2, 'active')")
            .bind::<BigInt, _>(project_id)
            .bind::<BigInt, _>(now_ms)
            .execute(conn)
            .context("failed to create session")?;

        let session_id = sql_query("SELECT last_insert_rowid() AS id")
            .get_result::<LastInsertId>(conn)
            .context("failed to fetch new session id")?
            .id;

        Ok((project_id, session_id))
    })
}

pub fn end_session(db_path: &Path, session_id: i64, status: &str, now_ms: i64) -> Result<()> {
    let mut conn = establish_connection(db_path)?;
    sql_query(
        "UPDATE sessions SET status = ?1, ended_at_ms = ?2 WHERE id = ?3 AND status = 'active'",
    )
    .bind::<Text, _>(status)
    .bind::<BigInt, _>(now_ms)
    .bind::<BigInt, _>(session_id)
    .execute(&mut conn)
    .context("failed to end session")?;
    Ok(())
}

pub fn list_sessions(db_path: &Path, project_id: i64, limit: i64) -> Result<Vec<SessionSummary>> {
    let mut conn = establish_connection(db_path)?;
    let rows = sql_query(
        "
        SELECT id, status, started_at_ms, ended_at_ms
        FROM sessions
        WHERE project_id = ?1
        ORDER BY started_at_ms DESC
        LIMIT ?2
        ",
    )
    .bind::<BigInt, _>(project_id)
    .bind::<BigInt, _>(limit)
    .load::<SessionSummary>(&mut conn)
    .context("failed to list sessions")?;
    Ok(rows)
}

pub fn list_comments(
    db_path: &Path,
    session_id: i64,
    file_path: Option<&str>,
    include_resolved: bool,
    limit: i64,
) -> Result<Vec<CommentRecord>> {
    let mut conn = establish_connection(db_path)?;
    let rows = match file_path {
        Some(file) => {
            sql_query(
                "
                SELECT id, session_id, file_path, line_start, line_end, selected_text, body, status, created_at_ms, updated_at_ms
                FROM comments
                WHERE session_id = ?1
                  AND file_path = ?2
                  AND (?3 = 1 OR status != 'resolved')
                ORDER BY updated_at_ms DESC
                LIMIT ?4
                ",
            )
            .bind::<BigInt, _>(session_id)
            .bind::<Text, _>(file)
            .bind::<Integer, _>(if include_resolved { 1 } else { 0 })
            .bind::<BigInt, _>(limit)
            .load::<CommentRecord>(&mut conn)
        }
        None => {
            sql_query(
                "
                SELECT id, session_id, file_path, line_start, line_end, selected_text, body, status, created_at_ms, updated_at_ms
                FROM comments
                WHERE session_id = ?1
                  AND (?2 = 1 OR status != 'resolved')
                ORDER BY updated_at_ms DESC
                LIMIT ?3
                ",
            )
            .bind::<BigInt, _>(session_id)
            .bind::<Integer, _>(if include_resolved { 1 } else { 0 })
            .bind::<BigInt, _>(limit)
            .load::<CommentRecord>(&mut conn)
        }
    }
    .context("failed to list comments")?;

    Ok(rows)
}

pub fn create_comment(
    db_path: &Path,
    session_id: i64,
    req: &CreateCommentRequest,
    now_ms: i64,
) -> Result<CommentRecord> {
    let body = req.body.trim();
    if body.is_empty() {
        return Err(anyhow::anyhow!("comment body cannot be empty"));
    }

    let file_path = req.file_path.trim();
    if file_path.is_empty() {
        return Err(anyhow::anyhow!("file_path cannot be empty"));
    }

    let line_start = req.line_start;
    let line_end = req.line_end.or(req.line_start);
    if let (Some(s), Some(e)) = (line_start, line_end)
        && (s < 1 || e < 1 || s > e)
    {
        return Err(anyhow::anyhow!("invalid line selection range"));
    }
    let selected_text = req.selected_text.as_ref().map(|v| v.trim().to_string());

    let mut conn = establish_connection(db_path)?;
    sql_query(
        "
        INSERT INTO comments (session_id, file_path, line_start, line_end, selected_text, body, status, created_at_ms, updated_at_ms)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?7)
        ",
    )
    .bind::<BigInt, _>(session_id)
    .bind::<Text, _>(file_path)
    .bind::<Nullable<BigInt>, _>(line_start)
    .bind::<Nullable<BigInt>, _>(line_end)
    .bind::<Nullable<Text>, _>(selected_text.as_deref())
    .bind::<Text, _>(body)
    .bind::<BigInt, _>(now_ms)
    .execute(&mut conn)
    .context("failed to insert comment")?;

    let id = sql_query("SELECT last_insert_rowid() AS id")
        .get_result::<LastInsertId>(&mut conn)
        .context("failed to fetch comment id")?
        .id;

    Ok(CommentRecord {
        id,
        session_id,
        file_path: file_path.to_string(),
        line_start,
        line_end,
        selected_text,
        body: body.to_string(),
        status: "open".to_string(),
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })
}

pub fn update_comment(
    db_path: &Path,
    session_id: i64,
    comment_id: i64,
    req: &UpdateCommentRequest,
    now_ms: i64,
) -> Result<Option<CommentRecord>> {
    let mut conn = establish_connection(db_path)?;
    let existing = sql_query(
        "
        SELECT id, session_id, file_path, line_start, line_end, selected_text, body, status, created_at_ms, updated_at_ms
        FROM comments
        WHERE id = ?1 AND session_id = ?2
        LIMIT 1
        ",
    )
    .bind::<BigInt, _>(comment_id)
    .bind::<BigInt, _>(session_id)
    .load::<CommentRecord>(&mut conn)
    .context("failed to read comment")?
    .into_iter()
    .next();

    let Some(mut item) = existing else {
        return Ok(None);
    };

    if let Some(body) = req.body.as_ref() {
        let trimmed = body.trim();
        if !trimmed.is_empty() {
            item.body = trimmed.to_string();
        }
    }

    if let Some(status) = req.status.as_ref() {
        let normalized = status.trim().to_lowercase();
        if normalized == "open" || normalized == "resolved" {
            item.status = normalized;
        } else {
            return Err(anyhow::anyhow!("status must be 'open' or 'resolved'"));
        }
    }

    item.updated_at_ms = now_ms;

    sql_query("UPDATE comments SET body = ?1, status = ?2, updated_at_ms = ?3 WHERE id = ?4")
        .bind::<Text, _>(&item.body)
        .bind::<Text, _>(&item.status)
        .bind::<BigInt, _>(item.updated_at_ms)
        .bind::<BigInt, _>(item.id)
        .execute(&mut conn)
        .context("failed to update comment")?;

    Ok(Some(item))
}

pub fn get_comment(db_path: &Path, session_id: i64, comment_id: i64) -> Result<Option<CommentRecord>> {
    let mut conn = establish_connection(db_path)?;
    let row = sql_query(
        "
        SELECT id, session_id, file_path, line_start, line_end, selected_text, body, status, created_at_ms, updated_at_ms
        FROM comments
        WHERE id = ?1 AND session_id = ?2
        LIMIT 1
        ",
    )
    .bind::<BigInt, _>(comment_id)
    .bind::<BigInt, _>(session_id)
    .load::<CommentRecord>(&mut conn)
    .context("failed to read comment")?
    .into_iter()
    .next();

    Ok(row)
}

pub fn create_command(
    db_path: &Path,
    session_id: i64,
    comment_id: i64,
    tool: &str,
    prompt: &str,
    now_ms: i64,
) -> Result<CommandRecord> {
    let mut conn = establish_connection(db_path)?;
    sql_query(
        "
        INSERT INTO commands (
            session_id, comment_id, tool, prompt, status, created_at_ms
        ) VALUES (?1, ?2, ?3, ?4, 'queued', ?5)
        ",
    )
    .bind::<BigInt, _>(session_id)
    .bind::<BigInt, _>(comment_id)
    .bind::<Text, _>(tool)
    .bind::<Text, _>(prompt)
    .bind::<BigInt, _>(now_ms)
    .execute(&mut conn)
    .context("failed to create command")?;

    let id = sql_query("SELECT last_insert_rowid() AS id")
        .get_result::<LastInsertId>(&mut conn)
        .context("failed to fetch command id")?
        .id;

    get_command(db_path, id)?.context("command not found after insert")
}

pub fn get_command(db_path: &Path, command_id: i64) -> Result<Option<CommandRecord>> {
    let mut conn = establish_connection(db_path)?;
    let row = sql_query(
        "
        SELECT id, session_id, comment_id, tool, prompt, status, output_text, error_text, created_at_ms, started_at_ms, ended_at_ms
        FROM commands
        WHERE id = ?1
        LIMIT 1
        ",
    )
    .bind::<BigInt, _>(command_id)
    .load::<CommandRecord>(&mut conn)
    .context("failed to fetch command")?
    .into_iter()
    .next();
    Ok(row)
}

pub fn list_commands_for_comment(
    db_path: &Path,
    session_id: i64,
    comment_id: i64,
    limit: i64,
) -> Result<Vec<CommandRecord>> {
    let mut conn = establish_connection(db_path)?;
    let rows = sql_query(
        "
        SELECT id, session_id, comment_id, tool, prompt, status, output_text, error_text, created_at_ms, started_at_ms, ended_at_ms
        FROM commands
        WHERE comment_id = ?1
          AND session_id = ?2
        ORDER BY created_at_ms DESC
        LIMIT ?3
        ",
    )
    .bind::<BigInt, _>(comment_id)
    .bind::<BigInt, _>(session_id)
    .bind::<BigInt, _>(limit)
    .load::<CommandRecord>(&mut conn)
    .context("failed to list commands for comment")?;
    Ok(rows)
}

pub fn mark_command_running(db_path: &Path, command_id: i64, started_at_ms: i64) -> Result<()> {
    let mut conn = establish_connection(db_path)?;
    sql_query("UPDATE commands SET status = 'running', started_at_ms = ?1 WHERE id = ?2")
        .bind::<BigInt, _>(started_at_ms)
        .bind::<BigInt, _>(command_id)
        .execute(&mut conn)
        .context("failed to mark command running")?;
    Ok(())
}

pub fn mark_command_done(
    db_path: &Path,
    command_id: i64,
    status: &str,
    output_text: Option<&str>,
    error_text: Option<&str>,
    ended_at_ms: i64,
) -> Result<()> {
    let mut conn = establish_connection(db_path)?;
    sql_query(
        "
        UPDATE commands
        SET status = ?1,
            output_text = ?2,
            error_text = ?3,
            ended_at_ms = ?4
        WHERE id = ?5
        ",
    )
    .bind::<Text, _>(status)
    .bind::<Nullable<Text>, _>(output_text)
    .bind::<Nullable<Text>, _>(error_text)
    .bind::<BigInt, _>(ended_at_ms)
    .bind::<BigInt, _>(command_id)
    .execute(&mut conn)
    .context("failed to finalize command")?;
    Ok(())
}

fn establish_connection(db_path: &Path) -> Result<SqliteConnection> {
    let url = db_path.to_string_lossy().to_string();
    let mut conn = SqliteConnection::establish(&url)
        .with_context(|| format!("failed to open sqlite database {}", db_path.display()))?;
    conn.batch_execute(
        "
        PRAGMA foreign_keys = ON;
        PRAGMA busy_timeout = 5000;
        ",
    )
    .context("failed to apply sqlite connection pragmas")?;
    Ok(conn)
}

fn ensure_comments_columns(conn: &mut SqliteConnection) -> Result<()> {
    let columns = sql_query("PRAGMA table_info(comments)")
        .load::<ColumnInfo>(conn)
        .context("failed to inspect comments table")?;

    let existing = columns
        .into_iter()
        .map(|c| c.name)
        .collect::<HashSet<_>>();

    if !existing.contains("line_start") {
        sql_query("ALTER TABLE comments ADD COLUMN line_start INTEGER")
            .execute(conn)
            .context("failed adding comments.line_start")?;
    }
    if !existing.contains("line_end") {
        sql_query("ALTER TABLE comments ADD COLUMN line_end INTEGER")
            .execute(conn)
            .context("failed adding comments.line_end")?;
    }
    if !existing.contains("selected_text") {
        sql_query("ALTER TABLE comments ADD COLUMN selected_text TEXT")
            .execute(conn)
            .context("failed adding comments.selected_text")?;
    }

    if existing.contains("line_number") {
        sql_query("UPDATE comments SET line_start = line_number WHERE line_start IS NULL")
            .execute(conn)
            .context("failed backfilling comments.line_start")?;
    }

    Ok(())
}
