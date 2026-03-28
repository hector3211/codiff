use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::db::{CommentRecord, mark_command_done, mark_command_running};
use crate::now_ms_i64;

pub fn detect_ai_client() -> (String, String) {
    let checks = [
        ("opencode", ["OPENCODE", "OPEN_CODE"]),
        ("claude", ["CLAUDE_CODE", "ANTHROPIC_API_KEY"]),
        ("codex", ["CODEX", "OPENAI_API_KEY"]),
    ];

    for (name, keys) in checks {
        for key in keys {
            if std::env::vars().any(|(k, _)| k.contains(key)) {
                return (name.to_string(), format!("env:{key}"));
            }
        }
    }

    ("unknown".to_string(), "no-known-env-marker".to_string())
}

pub fn build_command_prompt(comment: &CommentRecord) -> String {
    let mut prompt = format!(
        "Apply a code change for file '{}' based on this review comment:\n{}",
        comment.file_path, comment.body
    );

    if let (Some(start), Some(end)) = (comment.line_start, comment.line_end) {
        if start == end {
            prompt.push_str(&format!("\nFocus on line {}.", start));
        } else {
            prompt.push_str(&format!("\nFocus on lines {}-{}.", start, end));
        }
    }

    if let Some(selected) = comment.selected_text.as_ref()
        && !selected.is_empty()
    {
        prompt.push_str(&format!("\nSelected context:\n{}", selected));
    }

    prompt.push_str("\nReturn a concise summary of what changed.");
    prompt
}

pub async fn execute_ai_command(
    db_path: PathBuf,
    root: PathBuf,
    command_id: i64,
    tool: String,
    prompt: String,
    custom_command: Option<String>,
) {
    if let Err(err) = mark_command_running(&db_path, command_id, now_ms_i64()) {
        eprintln!("failed to mark command running: {err:#}");
        return;
    }

    let result = run_ai_tool(&root, &tool, &prompt, custom_command.as_deref()).await;
    match result {
        Ok(output) => {
            let _ = mark_command_done(
                &db_path,
                command_id,
                "succeeded",
                Some(&output),
                None,
                now_ms_i64(),
            );
        }
        Err(err) => {
            let _ = mark_command_done(
                &db_path,
                command_id,
                "failed",
                None,
                Some(&err.to_string()),
                now_ms_i64(),
            );
        }
    }
}

async fn run_ai_tool(root: &Path, tool: &str, prompt: &str, custom_command: Option<&str>) -> Result<String> {
    let mut cmd = match tool {
        "opencode" => {
            let mut c = Command::new("opencode");
            c.arg("run").arg(prompt);
            c
        }
        "claude" | "claude_code" => {
            let mut c = Command::new("claude");
            c.arg("-p").arg(prompt);
            c
        }
        "codex" => {
            let mut c = Command::new("codex");
            c.arg(prompt);
            c
        }
        "custom" => build_custom_command(custom_command, prompt)?,
        _ => {
            let mut c = Command::new("opencode");
            c.arg("run").arg(prompt);
            c
        }
    };

    let output = cmd
        .kill_on_drop(true)
        .current_dir(root)
        .output()
        .await
        .with_context(|| format!("failed to execute AI command with tool {tool}"))?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).trim().to_string());
    }

    Err(anyhow::anyhow!(
        "AI command failed ({tool}): {}",
        String::from_utf8_lossy(&output.stderr).trim()
    ))
}

fn build_custom_command(custom_command: Option<&str>, prompt: &str) -> Result<Command> {
    let template = custom_command
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .context("custom command template is empty")?;

    let escaped_prompt = shell_escape(prompt);
    let rendered = if template.contains("{prompt}") {
        template.replace("{prompt}", &escaped_prompt)
    } else {
        format!("{template} {escaped_prompt}")
    };

    #[cfg(target_os = "windows")]
    {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(rendered);
        Ok(c)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut c = Command::new("sh");
        c.arg("-lc").arg(rendered);
        Ok(c)
    }
}

fn shell_escape(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\"'\"'"))
}
