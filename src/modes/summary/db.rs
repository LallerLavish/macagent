//! SQLite persistence for session summaries.
//!
//! Schema: per-(repo_id, branch) summaries with a join table for files
//! touched. Retrieval is filtered by file-overlap so summaries surface
//! only when relevant to today's changes.
//!
//! All rusqlite calls are sync; wrapped in tokio::task::spawn_blocking
//! at the public async boundaries.

use rusqlite::{params, Connection};
use std::path::PathBuf;
use thiserror::Error;
use tokio::fs;
use tokio::task;
use tracing::debug;

const DB_FILENAME: &str = "summaries.db";
const STATE_SUBDIR: &str = "macagent/state";

#[derive(Debug, Error)]
pub enum DbError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("no config dir available")]
    NoConfigDir,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("join error: {0}")]
    Join(#[from] task::JoinError),
}

/// One row in the summaries table.
#[derive(Debug, Clone)]
pub struct SummaryRow {
    pub id: i64,
    pub repo_id: String,
    pub repo_path: String,
    pub branch: String,
    pub commit_sha: String,
    pub mode_name: String,
    pub timestamp: i64,  // unix seconds
    pub headline: String,
    pub body: String,
    pub file_path: String,  // path to backup .txt on disk
}

/// Input for inserting a new summary.
#[derive(Debug, Clone)]
pub struct NewSummary {
    pub repo_id: String,
    pub repo_path: String,
    pub branch: String,
    pub commit_sha: String,
    pub mode_name: String,
    pub timestamp: i64,
    pub headline: String,
    pub body: String,
    pub file_path: String,
    pub files_touched: Vec<String>,
}

async fn db_path() -> Result<PathBuf, DbError> {
    let base = dirs::config_dir().ok_or(DbError::NoConfigDir)?;
    let dir = base.join(STATE_SUBDIR);
    fs::create_dir_all(&dir).await?;
    Ok(dir.join(DB_FILENAME))
}

/// Open a connection. Creates the DB file and tables on first call.
fn open_with_init(path: &std::path::Path) -> Result<Connection, rusqlite::Error> {
    let conn = Connection::open(path)?;
    conn.execute_batch(SCHEMA_SQL)?;
    Ok(conn)
}

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS summaries (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id      TEXT NOT NULL,
    repo_path    TEXT NOT NULL,
    branch       TEXT NOT NULL,
    commit_sha   TEXT NOT NULL,
    mode_name    TEXT NOT NULL,
    timestamp    INTEGER NOT NULL,
    headline     TEXT NOT NULL,
    body         TEXT NOT NULL,
    file_path    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS summary_files (
    summary_id   INTEGER NOT NULL REFERENCES summaries(id) ON DELETE CASCADE,
    file_path    TEXT NOT NULL,
    PRIMARY KEY (summary_id, file_path)
);

CREATE INDEX IF NOT EXISTS idx_summaries_lookup
    ON summaries(repo_id, branch, timestamp DESC);

CREATE INDEX IF NOT EXISTS idx_files_lookup
    ON summary_files(file_path, summary_id);
"#;

/// Insert a summary and its file-touch rows in one transaction.
pub async fn insert_summary(new: NewSummary) -> Result<i64, DbError> {
    let path = db_path().await?;
    let id = task::spawn_blocking(move || -> Result<i64, rusqlite::Error> {
        let mut conn = open_with_init(&path)?;
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT INTO summaries
                (repo_id, repo_path, branch, commit_sha, mode_name,
                 timestamp, headline, body, file_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                new.repo_id, new.repo_path, new.branch, new.commit_sha,
                new.mode_name, new.timestamp, new.headline, new.body,
                new.file_path,
            ],
        )?;
        let summary_id = tx.last_insert_rowid();

        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO summary_files (summary_id, file_path)
                 VALUES (?1, ?2)",
            )?;
            for f in &new.files_touched {
                stmt.execute(params![summary_id, f])?;
            }
        }

        tx.commit()?;
        Ok(summary_id)
    })
    .await??;

    debug!(summary_id = id, "summary inserted");
    Ok(id)
}

/// Find summaries on (repo_id, branch) whose touched files overlap with
/// the given file paths. Ordered most recent first.
///
/// This is the core retrieval used to build the [CONTEXT] block at
/// summary-generation time: "show me prior work on these same files."
pub async fn recent_summaries_by_files(
    repo_id: String,
    branch: String,
    files: Vec<String>,
    limit: usize,
) -> Result<Vec<SummaryRow>, DbError> {
    let path = db_path().await?;
    let rows = task::spawn_blocking(move || -> Result<Vec<SummaryRow>, rusqlite::Error> {
        let conn = open_with_init(&path)?;

        if files.is_empty() {
            // Fall back to recent-on-branch when no file filter
            return query_recent_on_branch(&conn, &repo_id, &branch, limit);
        }

        let placeholders = vec!["?"; files.len()].join(",");
        let sql = format!(
            "SELECT DISTINCT s.id, s.repo_id, s.repo_path, s.branch,
                    s.commit_sha, s.mode_name, s.timestamp,
                    s.headline, s.body, s.file_path
             FROM summaries s
             JOIN summary_files sf ON s.id = sf.summary_id
             WHERE s.repo_id = ?
               AND s.branch = ?
               AND sf.file_path IN ({placeholders})
             ORDER BY s.timestamp DESC
             LIMIT ?"
        );

        let mut params_vec: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(3 + files.len());
        params_vec.push(&repo_id);
        params_vec.push(&branch);
        for f in &files {
            params_vec.push(f);
        }
        let limit_i64 = limit as i64;
        params_vec.push(&limit_i64);

        let mut stmt = conn.prepare(&sql)?;
        let mapped = stmt.query_map(params_vec.as_slice(), row_to_summary)?;
        mapped.collect::<Result<Vec<_>, _>>()
    })
    .await??;

    Ok(rows)
}

fn query_recent_on_branch(
    conn: &Connection,
    repo_id: &str,
    branch: &str,
    limit: usize,
) -> Result<Vec<SummaryRow>, rusqlite::Error> {
    let mut stmt = conn.prepare(
        "SELECT id, repo_id, repo_path, branch, commit_sha, mode_name,
                timestamp, headline, body, file_path
         FROM summaries
         WHERE repo_id = ?1 AND branch = ?2
         ORDER BY timestamp DESC
         LIMIT ?3",
    )?;
    let mapped = stmt.query_map(params![repo_id, branch, limit as i64], row_to_summary)?;
    mapped.collect()
}

/// The newest summary across all repos. Used for "show me the last summary."
pub async fn most_recent_summary() -> Result<Option<SummaryRow>, DbError> {
    let path = db_path().await?;
    let row = task::spawn_blocking(move || -> Result<Option<SummaryRow>, rusqlite::Error> {
        let conn = open_with_init(&path)?;
        let mut stmt = conn.prepare(
            "SELECT id, repo_id, repo_path, branch, commit_sha, mode_name,
                    timestamp, headline, body, file_path
             FROM summaries
             ORDER BY timestamp DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map([], row_to_summary)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    })
    .await??;
    Ok(row)
}

fn row_to_summary(row: &rusqlite::Row) -> Result<SummaryRow, rusqlite::Error> {
    Ok(SummaryRow {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        repo_path: row.get(2)?,
        branch: row.get(3)?,
        commit_sha: row.get(4)?,
        mode_name: row.get(5)?,
        timestamp: row.get(6)?,
        headline: row.get(7)?,
        body: row.get(8)?,
        file_path: row.get(9)?,
    })
}