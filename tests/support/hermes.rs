//! Synthetic Hermes `state.db` fixtures. The schema mirrors the producer's
//! current columns (revision 32) with secret-bearing decoy columns and tables;
//! the reduced shape drops optional columns to exercise legacy databases. Built
//! with a normal writable connection; the adapter under test only opens them
//! read-only.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};
use serde_json::Value as Json;

use agentscope::config::{CliOverrides, Config, Env};

/// A credential-shaped sentinel that must never appear in public output.
pub const SECRET: &str = "sk-hermes-SECRET-9f1c";
const SECRET_SYSTEM_PROMPT: &str = "SECRET-HERMES-SYSTEM-PROMPT";

/// Current producer shape (schema revision 32) limited to the reader's columns,
/// plus denied decoy columns/tables.
pub const CURRENT_SCHEMA: &str = r#"
CREATE TABLE sessions(
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    model TEXT,
    model_config TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    end_reason TEXT,
    last_activity_at REAL,
    parent_session_id TEXT,
    title TEXT,
    message_count INTEGER DEFAULT 0,
    tool_call_count INTEGER DEFAULT 0,
    input_tokens INTEGER DEFAULT 0,
    output_tokens INTEGER DEFAULT 0,
    cache_read_tokens INTEGER DEFAULT 0,
    cache_write_tokens INTEGER DEFAULT 0,
    reasoning_tokens INTEGER DEFAULT 0,
    api_call_count INTEGER DEFAULT 0,
    estimated_cost_usd REAL,
    actual_cost_usd REAL,
    cost_status TEXT,
    cost_source TEXT,
    billing_provider TEXT,
    billing_base_url TEXT,
    user_id TEXT,
    system_prompt TEXT,
    api_key TEXT,
    archived INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE messages(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_call_id TEXT,
    tool_calls TEXT,
    tool_name TEXT,
    timestamp REAL NOT NULL,
    finish_reason TEXT,
    token_count INTEGER,
    reasoning TEXT,
    reasoning_content TEXT,
    _compressed_summary INTEGER NOT NULL DEFAULT 0,
    active INTEGER NOT NULL DEFAULT 1,
    compacted INTEGER NOT NULL DEFAULT 0,
    display_kind TEXT,
    display_metadata TEXT,
    display_identity BLOB,
    display_order INTEGER,
    message_uid TEXT
);
CREATE INDEX idx_messages_session ON messages(session_id, timestamp);
CREATE TABLE session_model_usage(
    session_id TEXT NOT NULL,
    model TEXT NOT NULL,
    billing_provider TEXT NOT NULL DEFAULT '',
    billing_base_url TEXT NOT NULL DEFAULT '',
    billing_mode TEXT NOT NULL DEFAULT '',
    task TEXT NOT NULL DEFAULT '',
    api_call_count INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    cache_write_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens INTEGER NOT NULL DEFAULT 0,
    estimated_cost_usd REAL NOT NULL DEFAULT 0,
    actual_cost_usd REAL NOT NULL DEFAULT 0,
    cost_status TEXT,
    cost_source TEXT,
    first_seen REAL,
    last_seen REAL,
    PRIMARY KEY (session_id, model, billing_provider, billing_base_url, billing_mode, task)
);
CREATE TABLE auth(api_key TEXT);
CREATE TABLE system_prompts(hash TEXT PRIMARY KEY, prompt TEXT NOT NULL);
CREATE TABLE schema_version(version INTEGER NOT NULL);
INSERT INTO schema_version VALUES (32);
INSERT INTO auth VALUES ('SECRET-HERMES-AUTH');
INSERT INTO system_prompts VALUES ('h', 'SECRET-HERMES-SYSTEM-PROMPT');
"#;

/// An older, reduced shape: optional columns, display columns, accounting and
/// `session_model_usage` are absent, plus columns a future revision could add.
pub const REDUCED_SCHEMA: &str = r#"
CREATE TABLE sessions(
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    model TEXT,
    model_config TEXT,
    started_at REAL NOT NULL,
    ended_at REAL,
    end_reason TEXT,
    parent_session_id TEXT,
    title TEXT,
    billing_provider TEXT,
    api_key TEXT,
    future_note TEXT
);
CREATE TABLE messages(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role TEXT NOT NULL,
    content TEXT,
    tool_call_id TEXT,
    tool_calls TEXT,
    tool_name TEXT,
    timestamp REAL NOT NULL,
    finish_reason TEXT,
    future_blob BLOB
);
CREATE INDEX idx_messages_session ON messages(session_id, timestamp);
CREATE TABLE auth(api_key TEXT);
INSERT INTO auth VALUES ('SECRET-HERMES-AUTH');
"#;

fn schema(current_shape: bool) -> &'static str {
    if current_shape {
        CURRENT_SCHEMA
    } else {
        REDUCED_SCHEMA
    }
}

/// Creates a WAL-mode database (autocheckpoint off, so committed rows stay in
/// the WAL while the returned writer lives). `current_shape == false` builds the
/// reduced legacy shape.
pub fn create_db(path: &Path, current_shape: bool) -> Connection {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create db dirs");
    }
    let conn = Connection::open(path).expect("create db");
    conn.pragma_update(None, "journal_mode", "wal")
        .expect("wal");
    conn.pragma_update(None, "wal_autocheckpoint", 0)
        .expect("autocheckpoint");
    conn.execute_batch(schema(current_shape)).expect("schema");
    conn
}

/// Creates a rollback-journal database, so an exclusive writer lock blocks
/// concurrent readers (the lock/retry regression).
pub fn create_rollback_db(path: &Path, current_shape: bool) -> Connection {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create db dirs");
    }
    let conn = Connection::open(path).expect("create db");
    conn.pragma_update(None, "journal_mode", "delete")
        .expect("rollback journal");
    conn.execute_batch(schema(current_shape)).expect("schema");
    conn
}

fn columns(conn: &Connection, table: &str) -> Vec<String> {
    let mut statement = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .expect("table_info");
    statement
        .query_map([], |row| row.get::<_, String>(1))
        .expect("columns")
        .map(|name| name.expect("column name"))
        .collect()
}

fn insert_row(conn: &Connection, table: &str, values: Vec<(&str, Value)>) {
    let present = columns(conn, table);
    let kept: Vec<(&str, Value)> = values
        .into_iter()
        .filter(|(name, _)| present.iter().any(|column| column == name))
        .collect();
    let names = kept
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let marks = (1..=kept.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!("INSERT INTO {table}({names}) VALUES ({marks})"),
        params_from_iter(kept.into_iter().map(|(_, value)| value)),
    )
    .expect("insert fixture row");
}

fn text(value: Option<&str>) -> Value {
    value.map_or(Value::Null, |text| Value::Text(text.to_string()))
}

/// Inserts one session row with the producer's defaults. Columns absent from a
/// reduced fixture are skipped, never invented.
pub fn insert_session(
    conn: &Connection,
    id: &str,
    parent: Option<&str>,
    model_config: Json,
    started_seconds: f64,
) {
    insert_row(
        conn,
        "sessions",
        vec![
            ("id", Value::Text(id.to_string())),
            ("source", Value::Text("cli".to_string())),
            ("model", text(Some("test-model"))),
            ("model_config", Value::Text(model_config.to_string())),
            ("started_at", Value::Real(started_seconds)),
            ("last_activity_at", Value::Real(started_seconds)),
            ("parent_session_id", text(parent)),
            ("title", Value::Null),
            ("archived", Value::Integer(0)),
            ("billing_provider", text(Some("test-provider"))),
            ("message_count", Value::Integer(0)),
            ("tool_call_count", Value::Integer(0)),
            ("api_call_count", Value::Integer(0)),
            ("input_tokens", Value::Integer(0)),
            ("output_tokens", Value::Integer(0)),
            ("cache_read_tokens", Value::Integer(0)),
            ("cache_write_tokens", Value::Integer(0)),
            ("reasoning_tokens", Value::Integer(0)),
            ("api_key", Value::Text(SECRET.to_string())),
            (
                "system_prompt",
                Value::Text(SECRET_SYSTEM_PROMPT.to_string()),
            ),
            (
                "billing_base_url",
                Value::Text("https://secret.invalid".to_string()),
            ),
        ],
    );
}

/// Inserts one message and returns its native row id.
pub fn insert_message(
    conn: &Connection,
    session: &str,
    role: &str,
    content: &str,
    at_seconds: f64,
) -> i64 {
    insert_row(
        conn,
        "messages",
        vec![
            ("session_id", Value::Text(session.to_string())),
            ("role", Value::Text(role.to_string())),
            ("content", Value::Text(content.to_string())),
            ("timestamp", Value::Real(at_seconds)),
            ("active", Value::Integer(1)),
            ("compacted", Value::Integer(0)),
            ("_compressed_summary", Value::Integer(0)),
        ],
    );
    conn.last_insert_rowid()
}

/// Inserts one `session_model_usage` row; `token_legs` is
/// `(input, output, cache_read, cache_write, reasoning)`.
#[allow(clippy::too_many_arguments)]
pub fn insert_usage(
    conn: &Connection,
    session: &str,
    model: &str,
    provider: &str,
    task: &str,
    token_legs: [u64; 5],
    estimated: f64,
    cost_status: &str,
) {
    insert_row(
        conn,
        "session_model_usage",
        vec![
            ("session_id", Value::Text(session.to_string())),
            ("model", Value::Text(model.to_string())),
            ("billing_provider", Value::Text(provider.to_string())),
            ("task", Value::Text(task.to_string())),
            ("api_call_count", Value::Integer(1)),
            ("input_tokens", Value::Integer(token_legs[0] as i64)),
            ("output_tokens", Value::Integer(token_legs[1] as i64)),
            ("cache_read_tokens", Value::Integer(token_legs[2] as i64)),
            ("cache_write_tokens", Value::Integer(token_legs[3] as i64)),
            ("reasoning_tokens", Value::Integer(token_legs[4] as i64)),
            ("estimated_cost_usd", Value::Real(estimated)),
            ("actual_cost_usd", Value::Real(0.0)),
            ("cost_status", text(Some(cost_status))),
            ("cost_source", text(Some("fixture"))),
            ("first_seen", Value::Real(1_793_318_400.0)),
            ("last_seen", Value::Real(1_793_318_500.0)),
        ],
    );
}

// ---- explicit fixture updates (known columns only) --------------------------

fn update(conn: &Connection, id: &str, column: &str, value: Value) {
    conn.execute(
        &format!("UPDATE sessions SET {column} = ?1 WHERE id = ?2"),
        rusqlite::params![value, id],
    )
    .expect("update fixture session");
}

pub fn set_title(conn: &Connection, id: &str, title: &str) {
    update(conn, id, "title", Value::Text(title.to_string()));
}

pub fn set_model(conn: &Connection, id: &str, model: &str) {
    update(conn, id, "model", Value::Text(model.to_string()));
}

pub fn set_provider(conn: &Connection, id: &str, provider: &str) {
    update(
        conn,
        id,
        "billing_provider",
        Value::Text(provider.to_string()),
    );
}

pub fn set_model_config(conn: &Connection, id: &str, model_config: Json) {
    update(
        conn,
        id,
        "model_config",
        Value::Text(model_config.to_string()),
    );
}

pub fn set_model_config_raw(conn: &Connection, id: &str, raw: &str) {
    update(conn, id, "model_config", Value::Text(raw.to_string()));
}

pub fn set_model_config_blob(conn: &Connection, id: &str, blob: &[u8]) {
    update(conn, id, "model_config", Value::Blob(blob.to_vec()));
}

pub fn set_started_text(conn: &Connection, id: &str, raw: &str) {
    update(conn, id, "started_at", Value::Text(raw.to_string()));
}

pub fn set_last_activity(conn: &Connection, id: &str, seconds: f64) {
    update(conn, id, "last_activity_at", Value::Real(seconds));
}

pub fn set_archived(conn: &Connection, id: &str, archived: bool) {
    update(conn, id, "archived", Value::Integer(i64::from(archived)));
}

pub fn set_ended(conn: &Connection, id: &str, seconds: f64, reason: &str) {
    update(conn, id, "ended_at", Value::Real(seconds));
    update(conn, id, "end_reason", Value::Text(reason.to_string()));
}

pub fn set_message_state(conn: &Connection, message: i64, active: i64, compacted: i64) {
    conn.execute(
        "UPDATE messages SET active = ?1, compacted = ?2 WHERE id = ?3",
        rusqlite::params![active, compacted, message],
    )
    .expect("update message state");
}

pub fn set_message_display(
    conn: &Connection,
    message: i64,
    order: i64,
    identity: &[u8],
    uid: &str,
) {
    conn.execute(
        "UPDATE messages SET display_order = ?1, display_identity = ?2, message_uid = ?3 \
         WHERE id = ?4",
        rusqlite::params![order, identity, uid, message],
    )
    .expect("update message display");
}

pub fn set_message_summary(conn: &Connection, message: i64, flag: i64) {
    conn.execute(
        "UPDATE messages SET _compressed_summary = ?1 WHERE id = ?2",
        rusqlite::params![flag, message],
    )
    .expect("update message summary");
}

// ---- configuration ----------------------------------------------------------

fn base_config() -> Config {
    Config::load_with_env(&CliOverrides::default(), &Env::new()).expect("valid test config")
}

/// Config whose Hermes database is exactly `db`.
pub fn config_for_db(db: &Path) -> Config {
    let mut config = base_config();
    config.harnesses.hermes.root = None;
    config.harnesses.hermes.db = Some(db.to_path_buf());
    config
}

/// Config whose Hermes home is exactly `home` (its own `state.db` plus profiles).
pub fn config_for_home(home: &Path) -> Config {
    let mut config = base_config();
    config.harnesses.hermes.root = Some(home.to_path_buf());
    config.harnesses.hermes.db = Some(home.join("state.db"));
    config
}

/// Config whose primary home is `home` and whose extra homes are `extras`.
pub fn config_for_homes(home: &Path, extras: &[&Path]) -> Config {
    let mut config = config_for_home(home);
    config.harnesses.hermes.extra_roots = extras.iter().map(|path| path.to_path_buf()).collect();
    config
}

pub fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
