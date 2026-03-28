use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use axum::extract::State;
use axum::response::sse::{Event, KeepAlive};
use axum::response::{Html, IntoResponse, Sse};
use axum::routing::get;
use axum::{Json, Router};
use clap::Parser;
use futures_util::StreamExt;
use notify::{Config, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use similar::TextDiff;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use walkdir::WalkDir;

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
struct AppState {
    root: PathBuf,
    max_file_bytes: usize,
    baseline: Arc<HashMap<String, String>>,
    changes: Arc<RwLock<HashMap<String, FileChange>>>,
    tx: broadcast::Sender<FileChange>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = canonicalize_or_current(&cli.path)?;
    enforce_bind_policy(&cli.host, cli.allow_remote)?;

    println!("Building baseline snapshot for {}", root.display());
    let baseline = build_baseline_snapshot(&root, cli.max_file_bytes)?;

    let (tx, _) = broadcast::channel(2048);
    let state = AppState {
        root: root.clone(),
        max_file_bytes: cli.max_file_bytes,
        baseline: Arc::new(baseline),
        changes: Arc::new(RwLock::new(HashMap::new())),
        tx,
    };

    start_watcher(state.clone(), cli.debounce_ms).await?;

    let app = Router::new()
        .route("/", get(index))
        .route("/events", get(events))
        .route("/api/state", get(api_state))
        .with_state(state.clone());

    let addr = format!("{}:{}", cli.host, cli.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind http server to {addr}"))?;

    println!("codiff is running at http://{addr}");
    println!("Watching {}", root.display());
    println!(
        "Diff limits: max file size {} bytes, debounce {}ms",
        cli.max_file_bytes, cli.debounce_ms
    );
    if cli.allow_remote {
        println!("Remote access is enabled by --allow-remote");
    }

    axum::serve(listener, app)
        .await
        .context("axum server failed")?;

    Ok(())
}

async fn start_watcher(state: AppState, debounce_ms: u64) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<notify::Result<notify::Event>>(1024);
    let mut watcher: RecommendedWatcher = RecommendedWatcher::new(
        move |event| {
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
    rel.starts_with(".git/") || rel.starts_with("target/")
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

async fn index() -> impl IntoResponse {
    Html(include_str!("../web/index.html"))
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

async fn api_state(State(state): State<AppState>) -> impl IntoResponse {
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
