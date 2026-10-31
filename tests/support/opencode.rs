//! Synthetic OpenCode SQLite fixtures. Built with a normal writable connection;
//! the adapter under test only ever opens the file read-only.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use agentscope::config::{CliOverrides, Config, Env};
use rusqlite::{Connection, params};

pub const V2_SCHEMA: &str = "
CREATE TABLE project(id TEXT PRIMARY KEY, name TEXT, worktree TEXT);
CREATE TABLE session_v2(
    id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, fork_session_id TEXT,
    title TEXT, directory TEXT, model TEXT, version TEXT, share_url TEXT,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER,
    idle_outcome TEXT, agent TEXT, metadata TEXT, tokens_input INTEGER,
    tokens_output INTEGER, tokens_reasoning INTEGER,
    tokens_cache_read INTEGER, tokens_cache_write INTEGER, cost REAL);
CREATE TABLE session_message(
    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL, seq INTEGER,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
CREATE INDEX session_message_session_time_created_id_idx
    ON session_message(session_id, time_created, id);
CREATE TABLE account(id TEXT PRIMARY KEY, secret TEXT);
CREATE TABLE control_account(id TEXT PRIMARY KEY, secret TEXT);
CREATE TABLE credential(id TEXT PRIMARY KEY, secret TEXT);
CREATE TABLE session_share(session_id TEXT PRIMARY KEY, share_url TEXT);
INSERT INTO account VALUES ('a', 'SECRET-ACCOUNT');
INSERT INTO control_account VALUES ('c', 'SECRET-CONTROL');
INSERT INTO credential VALUES ('k', 'SECRET-CREDENTIAL');
INSERT INTO session_share VALUES ('s', 'https://share.invalid/SECRET-SHARE');
";

pub const LEGACY_SCHEMA: &str = "
CREATE TABLE project(id TEXT PRIMARY KEY, name TEXT, worktree TEXT);
CREATE TABLE session(
    id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, title TEXT, directory TEXT,
    version TEXT, share_url TEXT, time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL, time_archived INTEGER);
CREATE TABLE message(
    id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
CREATE TABLE part(
    id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
CREATE TABLE account(id TEXT PRIMARY KEY, secret TEXT);
";

/// Creates a WAL-mode database with `schema`. Autocheckpoint is off so committed
/// rows stay in the WAL while the returned writer lives.
pub fn create(path: &Path, schema: &str) -> Connection {
    let conn = Connection::open(path).expect("create db");
    conn.pragma_update(None, "journal_mode", "wal")
        .expect("wal");
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("autocheckpoint");
    conn.execute_batch(schema).expect("schema");
    conn
}

pub fn create_v2(path: &Path) -> Connection {
    create(path, V2_SCHEMA)
}

pub fn create_legacy(path: &Path) -> Connection {
    create(path, LEGACY_SCHEMA)
}

pub struct Session<'a> {
    pub id: &'a str,
    pub title: Option<&'a str>,
    pub directory: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub parent_id: Option<&'a str>,
    pub fork_id: Option<&'a str>,
    pub model: Option<&'a str>,
    pub agent: Option<&'a str>,
    pub created: i64,
    pub updated: i64,
    pub archived: Option<i64>,
    /// `(input, output, reasoning, cache_read, cache_write)` session totals.
    pub tokens: Option<[i64; 5]>,
    pub cost: Option<f64>,
    pub idle_outcome: Option<&'a str>,
}

impl<'a> Session<'a> {
    pub fn new(id: &'a str, created: i64, updated: i64) -> Self {
        Self {
            id,
            title: Some("A title"),
            directory: Some("/work/demo"),
            project_id: None,
            parent_id: None,
            fork_id: None,
            model: None,
            agent: None,
            created,
            updated,
            archived: None,
            tokens: None,
            cost: None,
            idle_outcome: None,
        }
    }
}

pub fn insert_session(conn: &Connection, s: &Session<'_>) {
    conn.execute(
        "INSERT INTO session_v2(id, project_id, parent_id, fork_session_id, title, directory,
            model, agent, version, share_url, time_created, time_updated, time_archived,
            tokens_input, tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write,
            cost, idle_outcome)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '2.0.29', 'https://share.invalid/SECRET-SHARE', ?9, ?10, ?11,
            ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        params![
            s.id, s.project_id, s.parent_id, s.fork_id, s.title, s.directory, s.model, s.agent,
            s.created, s.updated, s.archived,
            s.tokens.map(|t| t[0]), s.tokens.map(|t| t[1]), s.tokens.map(|t| t[2]),
            s.tokens.map(|t| t[3]), s.tokens.map(|t| t[4]), s.cost, s.idle_outcome
        ],
    )
    .expect("insert session");
}

pub fn insert_legacy_session(conn: &Connection, s: &Session<'_>) {
    conn.execute(
        "INSERT INTO session(id, project_id, parent_id, title, directory, version, share_url,
            time_created, time_updated, time_archived)
         VALUES (?1, ?2, ?3, ?4, ?5, '1.0', 'https://share.invalid/SECRET-SHARE', ?6, ?7, ?8)",
        params![
            s.id,
            s.project_id,
            s.parent_id,
            s.title,
            s.directory,
            s.created,
            s.updated,
            s.archived
        ],
    )
    .expect("insert legacy session");
}

pub fn insert_project(conn: &Connection, id: &str, name: &str) {
    conn.execute(
        "INSERT INTO project(id, name, worktree) VALUES (?1, ?2, '/w')",
        params![id, name],
    )
    .expect("insert project");
}

pub fn insert_message(conn: &Connection, id: &str, session: &str, created: i64, updated: i64) {
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1, ?2, 'user', 0, ?3, ?4, '{\"text\":\"SECRET-CONTENT\"}')",
        params![id, session, created, updated],
    )
    .expect("insert message");
}

pub fn insert_legacy_message(
    conn: &Connection,
    id: &str,
    session: &str,
    created: i64,
    updated: i64,
) {
    conn.execute(
        "INSERT INTO message(id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, '{}')",
        params![id, session, created, updated],
    )
    .expect("insert legacy message");
}

pub fn model_json(id: &str, provider: &str, variant: &str) -> String {
    serde_json::json!({"id": id, "providerID": provider, "variant": variant}).to_string()
}

/// Config whose OpenCode database is exactly `db`.
pub fn config_for_db(db: &Path) -> Config {
    let cli = CliOverrides {
        opencode_db: Some(db.to_path_buf()),
        ..CliOverrides::default()
    };
    Config::load_with_env(&cli, &Env::new()).expect("valid test config")
}

pub fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
