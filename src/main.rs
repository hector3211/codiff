use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Seek, SeekFrom, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
mod controller;
mod db;
mod service;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive};
use axum::response::{Html, IntoResponse, Response, Sse};
use axum::routing::{get, patch};
use axum::Router;
use clap::Parser;
use fs2::FileExt;
use futures_util::StreamExt;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rand::distr::Alphanumeric;
use rand::{Rng, rng};
use serde::{Deserialize, Serialize};
use similar::TextDiff;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use walkdir::WalkDir;

use crate::db::{
    end_session, init_state_db, start_session,
};

#[derive(Debug, Parser)]
#[command(name = "codiff")]
#[command(about = "Realtime local diff review for AI-assisted coding")]
struct Cli {
    #[arg(value_name = "PATH", default_value = ".")]
    path: PathBuf,

    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    host: String,

    #[arg(short = 'p', long, default_value_t = 8787)]
    port: u16,

    #[arg(long, default_value_t = false)]
    allow_remote: bool,

    #[arg(long, default_value_t = 500_000)]
    max_file_bytes: usize,

    #[arg(long, default_value_t = 150)]
    debounce_ms: u64,

    #[arg(long)]
    state_dir: Option<PathBuf>,

    #[arg(long)]
    api_token: Option<String>,

    #[arg(long, default_value_t = 2)]
    max_concurrent_commands: usize,

    #[arg(long, default_value_t = 180)]
    command_timeout_secs: u64,

    #[arg(long, default_value_t = false)]
    no_open_browser: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum FileStatus {
    Added,
    Modified,
    Deleted,
    Unchanged,
}

#[derive(Debug, Clone, Serialize)]
struct FileChange {
    path: String,
    status: FileStatus,
    diff: String,
    timestamp_ms: u128,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) root: PathBuf,
    pub(crate) state_db_path: PathBuf,
    pub(crate) project_id: i64,
    pub(crate) session_id: i64,
    pub(crate) max_file_bytes: usize,
    pub(crate) baseline: Arc<HashMap<String, String>>,
    pub(crate) changes: Arc<RwLock<HashMap<String, FileChange>>>,
    pub(crate) tx: broadcast::Sender<FileChange>,
    pub(crate) api_token: Arc<String>,
    pub(crate) command_slots: Arc<Semaphore>,
    pub(crate) command_timeout_secs: u64,
}

struct ProjectLock {
    file: File,
    lock_path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct LockMetadata {
    pid: u32,
    project_path: String,
    host: String,
    port: u16,
    session_id: Option<i64>,
    started_at_ms: u128,
}

struct SessionGuard {
    db_path: PathBuf,
    session_id: i64,
}


impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl ProjectLock {
    fn refresh_metadata(
        &mut self,
        root: &Path,
        host: &str,
        port: u16,
        session_id: Option<i64>,
    ) -> Result<()> {
        let metadata = LockMetadata {
            pid: std::process::id(),
            project_path: root.display().to_string(),
            host: host.to_string(),
            port,
            session_id,
            started_at_ms: now_ms(),
        };
        write_lock_metadata(&mut self.file, &metadata)
    }
}

impl SessionGuard {
    fn new(db_path: PathBuf, session_id: i64) -> Self {
        Self {
            db_path,
            session_id,
        }
    }
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        let _ = end_session(&self.db_path, self.session_id, "ended", now_ms_i64());
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = canonicalize_or_current(&cli.path)?;
    enforce_bind_policy(&cli.host, cli.allow_remote)?;
    let state_dir = resolve_state_dir(cli.state_dir.as_deref())?;
    let mut lock = acquire_project_lock(&state_dir, &root)?;
    let state_db_path = state_dir.join("state.db");
    init_state_db(&state_db_path)?;
    let (project_id, session_id) = start_session(&state_db_path, &root, now_ms_i64())?;
    lock.refresh_metadata(&root, &cli.host, cli.port, Some(session_id))?;
    let _session_guard = SessionGuard::new(state_db_path.clone(), session_id);

    println!("Building baseline snapshot for {}", root.display());
    let baseline = build_baseline_snapshot(&root, cli.max_file_bytes)?;

    let (tx, _) = broadcast::channel(2048);
    let api_token = resolve_api_token(cli.api_token.as_deref());
    let state = AppState {
        root: root.clone(),
        state_db_path: state_db_path.clone(),
        project_id,
        session_id,
        max_file_bytes: cli.max_file_bytes,
        baseline: Arc::new(baseline),
        changes: Arc::new(RwLock::new(HashMap::new())),
        tx,
        api_token: Arc::new(api_token.clone()),
        command_slots: Arc::new(Semaphore::new(cli.max_concurrent_commands.max(1))),
        command_timeout_secs: cli.command_timeout_secs.max(1),
    };

    start_watcher(state.clone(), cli.debounce_ms).await?;

    let protected = Router::new()
        .route("/events", get(events))
        .route("/api/state", get(controller::api::api_state))
        .route("/api/sessions", get(controller::api::api_sessions))
        .route("/api/client", get(controller::api::api_client))
        .route(
            "/api/comments",
            get(controller::api::api_list_comments).post(controller::api::api_create_comment),
        )
        .route(
            "/api/comments/{id}",
            patch(controller::api::api_update_comment),
        )
        .route(
            "/api/commands",
            get(controller::api::api_list_commands).post(controller::api::api_create_command),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_token,
        ));

    let app = Router::new()
        .route("/", get(index))
        .merge(protected)
        .with_state(state.clone());

    let addr = format!("{}:{}", cli.host, cli.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind http server to {addr}"))?;

    println!("codiff is running at http://{addr}");
    println!("Watching {}", root.display());
    println!("State dir: {}", state_dir.display());
    println!("Lock file: {}", lock.lock_path.display());
    println!("Project ID: {project_id}, Session ID: {session_id}");
    println!("API token: {api_token}");
    println!(
        "Command limits: max-concurrent {}, timeout {}s",
        cli.max_concurrent_commands.max(1),
        cli.command_timeout_secs.max(1)
    );
    println!(
        "Diff limits: max file size {} bytes, debounce {}ms",
        cli.max_file_bytes, cli.debounce_ms
    );
    if cli.allow_remote {
        println!("Remote access is enabled by --allow-remote");
    }

    if !cli.no_open_browser {
        let url = format!("http://{addr}");
        if let Err(err) = webbrowser::open(&url) {
            eprintln!("failed to open browser automatically: {err}");
            eprintln!("open this URL manually: {url}");
        }
    }

    axum::serve(listener, app)
        .await
        .context("axum server failed")?;

    Ok(())
}

async fn start_watcher(state: AppState, debounce_ms: u64) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<notify::Result<notify::Event>>(1024);
    let root = state.root.clone();
    let mut watcher: RecommendedWatcher = RecommendedWatcher::new(
        move |event| {
            if !should_enqueue_event(&root, &event) {
                return;
            }
            if tx.try_send(event).is_err() {
                eprintln!("watch queue is full; dropping filesystem event");
            }
        },
        Config::default().with_poll_interval(Duration::from_millis(350)),
    )
    .context("failed to create file watcher")?;

    watcher
        .watch(&state.root, RecursiveMode::Recursive)
        .context("failed to watch root path")?;

    tokio::spawn(async move {
        let _watcher = watcher;
        while let Some(event_result) = rx.recv().await {
            let mut rel_paths = HashSet::new();
            collect_event_paths(&state, event_result, &mut rel_paths);

            let debounce_for = Duration::from_millis(debounce_ms.max(1));
            loop {
                match tokio::time::timeout(debounce_for, rx.recv()).await {
                    Ok(Some(next)) => collect_event_paths(&state, next, &mut rel_paths),
                    Ok(None) => break,
                    Err(_) => break,
                }
            }

            if rel_paths.is_empty() {
                continue;
            }

            if let Err(err) = process_rel_paths(state.clone(), rel_paths).await {
                eprintln!("watch processing error: {err:#}");
            }
        }
    });

    Ok(())
}

fn collect_event_paths(state: &AppState, event_result: notify::Result<notify::Event>, rel_paths: &mut HashSet<String>) {
    match event_result {
        Ok(event) => {
            if should_skip_kind(&event.kind) {
                return;
            }

            for path in event.paths {
                if let Some(rel) = to_relative(&state.root, &path) {
                    if should_ignore_rel(&rel) {
                        continue;
                    }
                    rel_paths.insert(rel);
                }
            }
        }
        Err(err) => eprintln!("watch error: {err}"),
    }
}

async fn process_rel_paths(state: AppState, rel_paths: HashSet<String>) -> Result<()> {
    for rel in rel_paths {
        if let Some(change) = compute_change(&state, &rel).await? {
            let mut changes = state.changes.write().await;
            match change.status {
                FileStatus::Unchanged => {
                    changes.remove(&change.path);
                }
                _ => {
                    changes.insert(change.path.clone(), change.clone());
                }
            }
            drop(changes);

            let _ = state.tx.send(change);
        }
    }

    Ok(())
}

fn should_skip_kind(kind: &EventKind) -> bool {
    matches!(kind, EventKind::Access(_))
}

async fn compute_change(state: &AppState, rel: &str) -> Result<Option<FileChange>> {
    let baseline = state.baseline.get(rel).cloned();
    let current = read_file_text_or_reason(&state.root.join(rel), state.max_file_bytes);

    let (status, current_text, note) = match (&baseline, current) {
        (None, FileRead::Missing) => return Ok(None),
        (Some(_), FileRead::Missing) => (FileStatus::Deleted, String::new(), None),
        (Some(base), FileRead::Text(curr)) if base == &curr => {
            (FileStatus::Unchanged, curr, None)
        }
        (Some(_), FileRead::Text(curr)) => (FileStatus::Modified, curr, None),
        (None, FileRead::Text(curr)) => (FileStatus::Added, curr, None),
        (Some(_), FileRead::Skipped(reason)) => {
            (FileStatus::Modified, String::new(), Some(reason))
        }
        (None, FileRead::Skipped(reason)) => (FileStatus::Added, String::new(), Some(reason)),
    };

    if matches!(status, FileStatus::Unchanged) {
        return Ok(Some(FileChange {
            path: rel.to_string(),
            status,
            diff: String::new(),
            timestamp_ms: now_ms(),
        }));
    }

    let diff = if let Some(reason) = note {
        format!("--- a/{rel}\n+++ b/{rel}\n@@\n# diff skipped: {reason}\n")
    } else {
        let base_text = baseline.as_deref().unwrap_or("");
        TextDiff::from_lines(base_text, &current_text)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{rel}"), &format!("b/{rel}"))
            .to_string()
    };

    Ok(Some(FileChange {
        path: rel.to_string(),
        status,
        diff,
        timestamp_ms: now_ms(),
    }))
}

fn build_baseline_snapshot(root: &Path, max_file_bytes: usize) -> Result<HashMap<String, String>> {
    let mut snapshot = HashMap::new();

    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !entry.file_type().is_file() {
            continue;
        }

        let Some(rel) = to_relative(root, path) else {
            continue;
        };

        if should_ignore_rel(&rel) {
            continue;
        }

        if let FileRead::Text(text) = read_file_text_or_reason(path, max_file_bytes) {
            snapshot.insert(rel, text);
        }
    }

    Ok(snapshot)
}

enum FileRead {
    Missing,
    Text(String),
    Skipped(&'static str),
}

fn read_file_text_or_reason(path: &Path, max_file_bytes: usize) -> FileRead {
    let Ok(metadata) = std::fs::metadata(path) else {
        return FileRead::Missing;
    };

    if !metadata.is_file() {
        return FileRead::Missing;
    }

    if metadata.len() as usize > max_file_bytes {
        return FileRead::Skipped("file exceeds max-file-bytes limit");
    }

    let Ok(bytes) = std::fs::read(path) else {
        return FileRead::Missing;
    };

    if bytes.iter().take(512).any(|b| *b == 0) {
        return FileRead::Skipped("binary file");
    }

    match String::from_utf8(bytes) {
        Ok(text) => FileRead::Text(text),
        Err(_) => FileRead::Skipped("non-UTF8 file"),
    }
}

fn to_relative(root: &Path, path: &Path) -> Option<String> {
    let rel = if path.is_absolute() {
        path.strip_prefix(root).ok()?.to_path_buf()
    } else {
        path.to_path_buf()
    };

    let rel_str = rel.to_string_lossy().replace('\\', "/");
    if rel_str.is_empty() {
        return None;
    }
    Some(rel_str)
}

fn should_ignore_rel(rel: &str) -> bool {
    const IGNORED_PATH_PREFIXES: &[&str] = &[
        ".git/",
        "target/",
        "node_modules/",
        "dist/",
        "build/",
        ".next/",
        ".nuxt/",
        ".svelte-kit/",
        "coverage/",
        ".turbo/",
        ".cache/",
        "tmp/",
        "temp/",
    ];

    IGNORED_PATH_PREFIXES
        .iter()
        .any(|prefix| rel.starts_with(prefix))
}

fn should_enqueue_event(root: &Path, event_result: &notify::Result<notify::Event>) -> bool {
    let event = match event_result {
        Ok(event) => event,
        Err(_) => return true,
    };

    if should_skip_kind(&event.kind) {
        return false;
    }

    event.paths.iter().any(|path| {
        to_relative(root, path)
            .map(|rel| !should_ignore_rel(&rel))
            .unwrap_or(false)
    })
}

fn enforce_bind_policy(host: &str, allow_remote: bool) -> Result<()> {
    if allow_remote {
        return Ok(());
    }

    if host.eq_ignore_ascii_case("localhost") {
        return Ok(());
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if ip.is_loopback() {
            return Ok(());
        }
    }

    Err(anyhow::anyhow!(
        "refusing to bind non-loopback host '{host}' without --allow-remote"
    ))
}

fn resolve_state_dir(state_dir: Option<&Path>) -> Result<PathBuf> {
    let resolved = if let Some(path) = state_dir {
        path.to_path_buf()
    } else if let Ok(path) = std::env::var("CODIFF_STATE_DIR") {
        PathBuf::from(path)
    } else {
        let home = std::env::var("HOME").context("HOME environment variable is not set")?;
        PathBuf::from(home).join(".local/share/codiff")
    };

    std::fs::create_dir_all(&resolved).with_context(|| {
        format!(
            "failed to create or access state directory {}",
            resolved.display()
        )
    })?;

    Ok(resolved)
}

fn acquire_project_lock(state_dir: &Path, root: &Path) -> Result<ProjectLock> {
    let lock_root = state_dir.join("locks");
    std::fs::create_dir_all(&lock_root)
        .with_context(|| format!("failed to create lock directory {}", lock_root.display()))?;

    let project_key = project_hash_key(&root);
    let lock_path = lock_root.join(format!("{project_key}.lock"));
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;

    match file.try_lock_exclusive() {
        Ok(()) => {
            let metadata = LockMetadata {
                pid: std::process::id(),
                project_path: root.display().to_string(),
                host: "127.0.0.1".to_string(),
                port: 8787,
                session_id: None,
                started_at_ms: now_ms(),
            };
            write_lock_metadata(&mut file, &metadata)?;
            Ok(ProjectLock { file, lock_path })
        }
        Err(err) if err.kind() == ErrorKind::WouldBlock => {
            let existing = read_lock_metadata(&lock_path).ok();
            let detail = existing
                .as_ref()
                .map(format_lock_detail)
                .unwrap_or_else(|| "existing instance metadata unavailable".to_string());
            Err(anyhow::anyhow!(
                "codiff is already running for this project ({detail})"
            ))
        }
        Err(err) => Err(anyhow::anyhow!(
            "failed to lock project {}: {err}",
            root.display()
        )),
    }
}

fn write_lock_metadata(file: &mut File, metadata: &LockMetadata) -> Result<()> {
    let text = serde_json::to_string(metadata).context("failed to serialize lock metadata")?;
    file.set_len(0).context("failed to truncate lock metadata")?;
    file.seek(SeekFrom::Start(0))
        .context("failed to seek lock metadata")?;
    file.write_all(text.as_bytes())
        .context("failed to write lock metadata")?;
    file.sync_data().context("failed to flush lock metadata")?;
    Ok(())
}

fn read_lock_metadata(lock_path: &Path) -> Result<LockMetadata> {
    let text = std::fs::read_to_string(lock_path)
        .with_context(|| format!("failed to read lock metadata from {}", lock_path.display()))?;
    let metadata = serde_json::from_str::<LockMetadata>(&text)
        .context("failed to parse lock metadata")?;
    Ok(metadata)
}

fn format_lock_detail(meta: &LockMetadata) -> String {
    let session_part = meta
        .session_id
        .map(|id| id.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    format!(
        "pid={}, url=http://{}:{}, session_id={}, started_ms={}",
        meta.pid, meta.host, meta.port, session_part, meta.started_at_ms
    )
}

fn project_hash_key(path: &Path) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn canonicalize_or_current(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return std::env::current_dir().context("failed to resolve current directory");
    }

    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize {}", path.display()));
    }

    Err(anyhow::anyhow!(
        "path does not exist: {}",
        path.to_string_lossy()
    ))
}

async fn index(State(state): State<AppState>) -> impl IntoResponse {
    let html = include_str!("../web/index.html").replace(
        "__CODIFF_TOKEN__",
        state.api_token.as_ref().as_str(),
    );
    Html(html)
}

async fn events(
    State(state): State<AppState>,
) -> Sse<impl futures_util::stream::Stream<Item = std::result::Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.tx.subscribe()).filter_map(|msg| async move {
        match msg {
            Ok(change) => {
                let payload = serde_json::to_string(&change).ok()?;
                Some(Ok(Event::default().event("change").data(payload)))
            }
            Err(_) => None,
        }
    });

    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(12)).text("ok"))
}

pub(crate) fn now_ms_i64() -> i64 {
    now_ms().try_into().unwrap_or(i64::MAX)
}

fn resolve_api_token(user_provided: Option<&str>) -> String {
    if let Some(token) = user_provided {
        let trimmed = token.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect()
}

async fn require_api_token(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let token_from_header = req
        .headers()
        .get("x-codiff-token")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let token_from_query = req
        .uri()
        .query()
        .and_then(extract_token_from_query)
        .and_then(percent_decode_query_value);

    let provided = token_from_header.or(token_from_query);
    if provided.as_deref() == Some(state.api_token.as_ref().as_str()) {
        return next.run(req).await;
    }

    StatusCode::UNAUTHORIZED.into_response()
}

fn extract_token_from_query(query: &str) -> Option<&str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find_map(|(k, v)| if k == "token" { Some(v) } else { None })
}

fn percent_decode_query_value(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut idx = 0;

    while idx < bytes.len() {
        match bytes[idx] {
            b'+' => {
                out.push(b' ');
                idx += 1;
            }
            b'%' => {
                if idx + 2 >= bytes.len() {
                    return None;
                }
                let hi = decode_hex_digit(bytes[idx + 1])?;
                let lo = decode_hex_digit(bytes[idx + 2])?;
                out.push((hi << 4) | lo);
                idx += 3;
            }
            b => {
                out.push(b);
                idx += 1;
            }
        }
    }

    String::from_utf8(out).ok()
}

fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_policy_allows_loopback_without_remote_flag() {
        assert!(enforce_bind_policy("127.0.0.1", false).is_ok());
        assert!(enforce_bind_policy("::1", false).is_ok());
        assert!(enforce_bind_policy("localhost", false).is_ok());
    }

    #[test]
    fn bind_policy_rejects_non_loopback_without_remote_flag() {
        let err = enforce_bind_policy("0.0.0.0", false).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing to bind non-loopback host '0.0.0.0' without --allow-remote")
        );
    }

    #[test]
    fn bind_policy_allows_non_loopback_with_remote_flag() {
        assert!(enforce_bind_policy("0.0.0.0", true).is_ok());
    }

    #[test]
    fn ignore_rules_skip_git_and_target() {
        assert!(should_ignore_rel(".git/HEAD"));
        assert!(should_ignore_rel("target/debug/app"));
        assert!(should_ignore_rel("node_modules/pkg/index.js"));
        assert!(should_ignore_rel("dist/assets/app.js"));
        assert!(should_ignore_rel("build/output.txt"));
        assert!(should_ignore_rel(".next/static/chunk.js"));
        assert!(!should_ignore_rel("src/main.rs"));
    }

    #[test]
    fn token_query_decodes_percent_encoded_value() {
        let encoded = extract_token_from_query("foo=bar&token=a%2Fb%3Dc%2B1").unwrap();
        let decoded = percent_decode_query_value(encoded).unwrap();
        assert_eq!(decoded, "a/b=c+1");
    }
}
