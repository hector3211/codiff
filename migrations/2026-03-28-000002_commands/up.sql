CREATE TABLE IF NOT EXISTS commands (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id INTEGER NOT NULL,
    comment_id INTEGER NOT NULL,
    tool TEXT NOT NULL,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL,
    output_text TEXT,
    error_text TEXT,
    created_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    ended_at_ms INTEGER,
    FOREIGN KEY(session_id) REFERENCES sessions(id) ON DELETE CASCADE,
    FOREIGN KEY(comment_id) REFERENCES comments(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_commands_comment_created
ON commands(comment_id, created_at_ms DESC);
