mod support;

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use agentscope::domain::{
    Harness, RefreshBudget, SessionKey, SessionMetadata, SourceKey, Warning, WarningKind,
};
use agentscope::providers::FORGOTTEN_CAP;
use agentscope::providers::hermes::HermesCatalog;
use agentscope::providers::hermes::schema::open_read_only;
use support::NOW;
use support::hermes::*;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

struct Scan {
    sessions: Vec<SessionMetadata>,
    removed: Vec<SessionKey>,
    warnings: Vec<Warning>,
}

fn scan(catalog: &mut HermesCatalog, budget: RefreshBudget) -> Scan {
    let mut out = Scan {
        sessions: Vec::new(),
        removed: Vec::new(),
        warnings: Vec::new(),
    };
    for _ in 0..10_000 {
        let delta = catalog.step(NOW, budget).expect("discovery slice");
        out.sessions.extend(delta.sessions);
        out.removed.extend(delta.removed);
        out.warnings.extend(delta.warnings);
        if delta.scan.done {
            return out;
        }
    }
    panic!("discovery never completed");
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn native_ids(sessions: &[SessionMetadata]) -> Vec<&str> {
    sessions
        .iter()
        .map(|session| session.key.native_id.as_str())
        .collect()
}

fn database(key: &SessionKey) -> &Path {
    match &key.source {
        SourceKey::DbRow { database, .. } => database.as_path(),
        SourceKey::File(path) => path.as_path(),
    }
}

fn has_warning(warnings: &[Warning], kind: WarningKind, path: Option<&Path>) -> bool {
    warnings.iter().any(|warning| {
        warning.key.kind == kind
            && path.is_none_or(|path| warning.key.source.contains(&path.display().to_string()))
    })
}

/// Everything a reader could publish, flattened for a secret scan.
fn public_output(scan: &Scan) -> String {
    let mut text = String::new();
    for session in &scan.sessions {
        text.push_str(&session.title);
        text.push(' ');
        text.push_str(&format!("{:?}", session.model));
        text.push(' ');
        text.push_str(&format!("{session:?}"));
        text.push(' ');
        text.push_str(&format!("{:?}", session.key.source));
    }
    for warning in &scan.warnings {
        text.push_str(&warning.message);
        text.push(' ');
        text.push_str(&warning.key.source);
    }
    text
}

// ---- access boundary --------------------------------------------------------

#[test]
fn read_only_connection_denies_secret_tables_columns_writes_and_extensions() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    {
        let writer = create_db(&db, true);
        insert_session(
            &writer,
            "s1",
            None,
            serde_json::json!({"reasoning_config": {"enabled": true, "effort": "high"}}),
            1_793_318_400.0,
        );
        insert_message(&writer, "s1", "user", "hello", 1_793_318_401.0);
        insert_usage(
            &writer,
            "s1",
            "test-model",
            "test-provider",
            "main",
            [10, 20, 3, 4, 5],
            0.5,
            "actual",
        );
    }
    let conn = open_read_only(&db).expect("read-only open");

    // Allowlisted reads work.
    let sessions: i64 = conn
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .expect("allowed count");
    assert_eq!(sessions, 1);
    let title: Option<String> = conn
        .query_row("SELECT title FROM sessions", [], |row| row.get(0))
        .expect("allowed title");
    assert_eq!(title, None);
    let effort: Option<String> = conn
        .query_row(
            "SELECT json_extract(model_config, '$.reasoning_config.effort') FROM sessions",
            [],
            |row| row.get(0),
        )
        .expect("fixed config expression");
    assert_eq!(effort.as_deref(), Some("high"));
    let metadata: Option<String> = conn
        .query_row("SELECT display_metadata FROM messages", [], |row| {
            row.get(0)
        })
        .expect("allowed display metadata");
    assert_eq!(metadata, None);
    let status: Option<String> = conn
        .query_row("SELECT cost_status FROM session_model_usage", [], |row| {
            row.get(0)
        })
        .expect("allowed usage status");
    assert_eq!(status.as_deref(), Some("actual"));

    // Secret tables, denied columns, schema writes, pragmas, attachments and
    // extension/file functions stay denied at prepare/execute time.
    for sql in [
        "SELECT * FROM auth",
        "SELECT api_key FROM auth",
        "SELECT prompt FROM system_prompts",
        "SELECT * FROM schema_version",
        "SELECT version FROM schema_version",
        "SELECT system_prompt FROM sessions",
        "SELECT user_id FROM sessions",
        "SELECT api_key FROM sessions",
        "SELECT billing_base_url FROM sessions",
        "SELECT billing_base_url FROM session_model_usage",
        "SELECT billing_mode FROM session_model_usage",
        "SELECT reasoning FROM messages",
        "SELECT * FROM sessions",
        "SELECT * FROM messages",
        "INSERT INTO sessions(id, source, started_at) VALUES ('x', 'cli', 1)",
        "UPDATE sessions SET title = 'x'",
        "DELETE FROM messages",
        "CREATE TABLE t(x)",
        "DROP TABLE messages",
        "PRAGMA query_only = OFF",
        "PRAGMA journal_mode = DELETE",
        "ATTACH DATABASE ':memory:' AS other",
        "SELECT load_extension('x')",
        "SELECT readfile('x')",
        "SELECT writefile('x', 'y')",
    ] {
        assert!(conn.prepare(sql).is_err(), "must be denied: {sql}");
    }
    // A join cannot smuggle a denied column past the authorizer.
    assert!(
        conn.prepare("SELECT s.id FROM sessions s JOIN auth a ON a.api_key = s.id")
            .is_err()
    );
    // The allowlist is enforced by the connection itself, not by the adapter.
    let write = conn.execute(
        "INSERT INTO sessions(id, source, started_at) VALUES ('x', 'cli', 1)",
        [],
    );
    assert!(write.is_err(), "writes are refused");
}

// ---- database identity and profiles -----------------------------------------

#[test]
fn same_native_id_in_two_profiles_stays_distinct_and_aliases_appear_once() {
    let dir = tmp();
    let home = dir.path().join("hermes");
    let root_db = home.join("state.db");
    let profile_db = home.join("profiles").join("work").join("state.db");
    {
        let root = create_db(&root_db, true);
        insert_session(&root, "s1", None, serde_json::json!({}), 1_793_318_400.0);
        set_title(&root, "s1", "Root session");
        set_model(&root, "s1", "root-model");
    }
    {
        let profile = create_db(&profile_db, true);
        insert_session(&profile, "s1", None, serde_json::json!({}), 1_793_318_500.0);
        set_title(&profile, "s1", "Work session");
        set_model(&profile, "s1", "work-model");
    }

    let mut config = config_for_home(&home);
    // Repeated homes and a profile home named twice are aliases, not new stores.
    config.harnesses.hermes.extra_roots = vec![
        home.clone(),
        home.join("."),
        home.join("profiles").join("work"),
    ];
    let mut catalog = HermesCatalog::new(&config);
    assert_eq!(
        catalog.databases(),
        vec![root_db.clone(), profile_db.clone()]
    );

    let found = scan(&mut catalog, BIG);
    assert_eq!(found.sessions.len(), 2, "{:?}", native_ids(&found.sessions));
    assert!(found.sessions.iter().all(|session| {
        session.key.harness == Harness::Hermes && session.key.native_id == "s1"
    }));
    let root = found
        .sessions
        .iter()
        .find(|session| database(&session.key) == root_db)
        .expect("root profile session");
    let profile = found
        .sessions
        .iter()
        .find(|session| database(&session.key) == profile_db)
        .expect("work profile session");
    assert_ne!(root.key.source, profile.key.source);
    assert_eq!(root.title, "Root session");
    assert_eq!(profile.title, "Work session");
    assert_eq!(root.model.as_ref().unwrap().id, "root-model");
    assert_eq!(profile.model.as_ref().unwrap().id, "work-model");

    // An explicit database alias of the same file collapses too.
    let mut alias = config_for_db(&home.join(".").join("state.db"));
    alias.harnesses.hermes.extra_roots = vec![home.clone()];
    let mut catalog = HermesCatalog::new(&alias);
    assert_eq!(catalog.databases().len(), 2);
    let found = scan(&mut catalog, BIG);
    assert_eq!(found.sessions.len(), 2);
}

#[test]
fn an_explicit_profile_home_is_not_widened_to_siblings() {
    let dir = tmp();
    let home = dir.path().join("hermes");
    let profile = home.join("profiles").join("work");
    let sibling = home.join("profiles").join("other");
    {
        let work = create_db(&profile.join("state.db"), true);
        insert_session(&work, "work", None, serde_json::json!({}), 1_793_318_400.0);
    }
    {
        let other = create_db(&sibling.join("state.db"), true);
        insert_session(
            &other,
            "other",
            None,
            serde_json::json!({}),
            1_793_318_500.0,
        );
    }

    let mut config = config_for_home(&profile);
    config.harnesses.hermes.db = Some(profile.join("state.db"));
    let mut catalog = HermesCatalog::new(&config);
    assert_eq!(catalog.databases(), vec![profile.join("state.db")]);
    let found = scan(&mut catalog, BIG);
    assert_eq!(native_ids(&found.sessions), vec!["work"]);
}

// ---- cheap metadata and privacy ---------------------------------------------

#[test]
fn configured_effort_comes_only_from_the_verified_reasoning_shape() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    {
        let writer = create_db(&db, true);
        insert_session(
            &writer,
            "known",
            None,
            serde_json::json!({
                "max_tokens": 100,
                "reasoning_config": {"enabled": true, "effort": "high"},
                "api_key": SECRET,
            }),
            1_793_318_400.0,
        );
        insert_session(
            &writer,
            "disabled",
            None,
            serde_json::json!({"reasoning_config": {"enabled": false}, "api_key": SECRET}),
            1_793_318_401.0,
        );
        insert_session(
            &writer,
            "null-enabled",
            None,
            serde_json::json!({"reasoning_config": {"enabled": null, "effort": "high"}}),
            1_793_318_402.0,
        );
        insert_session(
            &writer,
            "missing",
            None,
            serde_json::json!({"max_tokens": 100}),
            1_793_318_403.0,
        );
        insert_session(
            &writer,
            "malformed",
            None,
            serde_json::json!({"max_tokens": 100}),
            1_793_318_404.0,
        );
        set_model_config_raw(&writer, "malformed", "{not json");
        insert_session(
            &writer,
            "effort-number",
            None,
            serde_json::json!({"reasoning_config": {"enabled": true, "effort": 7}}),
            1_793_318_405.0,
        );
        insert_session(
            &writer,
            "effort-object",
            None,
            serde_json::json!({"reasoning_config": {"enabled": true, "effort": {"level": "high"}}}),
            1_793_318_406.0,
        );
        insert_session(
            &writer,
            "string-config",
            None,
            serde_json::json!({"reasoning_config": "false"}),
            1_793_318_407.0,
        );
        insert_session(
            &writer,
            "integer-enabled",
            None,
            serde_json::json!({"reasoning_config": {"enabled": 0, "effort": "high"}}),
            1_793_318_408.0,
        );
        insert_message(&writer, "known", "user", "hello", 1_793_318_409.0);
    }

    let mut catalog = HermesCatalog::new(&config_for_db(&db));
    let found = scan(&mut catalog, BIG);
    assert_eq!(found.sessions.len(), 9, "{:?}", native_ids(&found.sessions));
    let by_id = |id: &str| {
        found
            .sessions
            .iter()
            .find(|session| session.key.native_id == id)
            .unwrap_or_else(|| panic!("missing session {id}"))
    };
    assert_eq!(
        by_id("known")
            .model
            .as_ref()
            .unwrap()
            .configured_effort
            .as_deref(),
        Some("high")
    );
    assert_eq!(
        by_id("disabled")
            .model
            .as_ref()
            .unwrap()
            .configured_effort
            .as_deref(),
        Some("off")
    );
    for id in [
        "null-enabled",
        "missing",
        "malformed",
        "effort-number",
        "effort-object",
        "string-config",
        "integer-enabled",
    ] {
        assert_eq!(
            by_id(id).model.as_ref().expect("model").configured_effort,
            None,
            "{id}"
        );
    }

    let published = public_output(&found);
    assert!(!published.contains(SECRET), "raw config secret leaked");
    assert!(!published.contains("SECRET-HERMES"), "decoy secret leaked");
    assert!(!published.contains("api_key"), "raw config key leaked");
    assert!(!published.contains("reasoning_config"), "raw config leaked");
    // Every visible session keeps a stable database identity in its key.
    assert!(
        found
            .sessions
            .iter()
            .all(|session| database(&session.key) == db)
    );
}

#[test]
fn metadata_uses_checked_epochs_and_bounded_latest_message_activity() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let started = 1_793_318_400.0;
    {
        let writer = create_db(&db, true);
        insert_session(&writer, "s1", None, serde_json::json!({}), started);
        insert_message(&writer, "s1", "user", "hello", started + 50.0);
        insert_session(
            &writer,
            "future",
            None,
            serde_json::json!({}),
            9_999_999_999.0,
        );
    }
    let mut catalog = HermesCatalog::new(&config_for_db(&db));
    let found = scan(&mut catalog, BIG);
    let by_id = |id: &str| {
        found
            .sessions
            .iter()
            .find(|session| session.key.native_id == id)
            .expect("session")
    };
    assert_eq!(by_id("s1").created_at, Some(1_793_318_400_000));
    assert_eq!(by_id("s1").activity.at, Some(1_793_318_450_000));
    // A cell outside the producer's epoch window stays missing, not zero.
    assert_eq!(by_id("future").created_at, None);
    assert_eq!(by_id("future").activity.at, None);
}

#[test]
fn every_profile_is_enumerated_without_a_hidden_cap() {
    let dir = tmp();
    let home = dir.path().join("hermes");
    {
        let root = create_db(&home.join("state.db"), true);
        insert_session(&root, "root", None, serde_json::json!({}), 1_793_318_400.0);
    }
    for index in 0..70 {
        let db = home
            .join("profiles")
            .join(format!("p{index:02}"))
            .join("state.db");
        let writer = create_db(&db, true);
        insert_session(
            &writer,
            &format!("s{index:02}"),
            None,
            serde_json::json!({}),
            1_793_318_400.0 + f64::from(index),
        );
    }
    let mut catalog = HermesCatalog::new(&config_for_home(&home));
    assert_eq!(catalog.databases().len(), 71);
    let found = scan(&mut catalog, BIG);
    assert_eq!(found.sessions.len(), 71);
    let ids: HashSet<&str> = native_ids(&found.sessions).into_iter().collect();
    assert!(ids.contains("root"));
    assert!(ids.contains("s69"));
}

#[test]
fn a_completed_pass_reports_sessions_that_disappear() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(
        &writer,
        "keep",
        None,
        serde_json::json!({}),
        1_793_318_400.0,
    );
    insert_session(
        &writer,
        "gone",
        None,
        serde_json::json!({}),
        1_793_318_401.0,
    );
    let mut catalog = HermesCatalog::new(&config_for_db(&db));
    let first = scan(&mut catalog, BIG);
    assert_eq!(first.sessions.len(), 2);

    writer
        .execute("DELETE FROM sessions WHERE id = 'gone'", [])
        .expect("delete session");
    let second = scan(&mut catalog, BIG);
    assert!(
        second.sessions.is_empty(),
        "unchanged metadata is not resent"
    );
    assert_eq!(second.removed.len(), 1);
    assert_eq!(second.removed[0].native_id, "gone");
    drop(writer);
}

// ---- WAL, locks, schema variants, budgets -----------------------------------

#[test]
fn wal_only_rows_are_visible_with_the_writer_open() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(
        &writer,
        "wal_only",
        None,
        serde_json::json!({}),
        1_793_318_400.0,
    );
    assert!(
        sidecar(&db, "-wal").exists(),
        "fixture keeps rows in the WAL"
    );
    let before = fs::read(&db).expect("read main db");

    let mut catalog = HermesCatalog::new(&config_for_db(&db));
    let found = scan(&mut catalog, BIG);
    assert_eq!(native_ids(&found.sessions), vec!["wal_only"]);
    assert!(found.sessions[0].source_available);
    assert_eq!(fs::read(&db).expect("read main db"), before);
    drop(writer);
}

#[test]
fn a_locked_profile_yields_while_siblings_discover_then_recovers() {
    let dir = tmp();
    let home_a = dir.path().join("a");
    let home_b = dir.path().join("b");
    let db_a = home_a.join("state.db");
    let db_b = home_b.join("state.db");
    let lock = create_rollback_db(&db_a, true);
    insert_session(&lock, "a1", None, serde_json::json!({}), 1_793_318_400.0);
    lock.execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");
    {
        let writer = create_db(&db_b, true);
        insert_session(&writer, "b1", None, serde_json::json!({}), 1_793_318_500.0);
    }
    let mut config = config_for_homes(&home_a, &[&home_b]);
    config.refresh_busy_timeout_ms = 25;
    config.page_busy_timeout_ms = 25;
    let mut catalog = HermesCatalog::new(&config);

    let started = std::time::Instant::now();
    let first = scan(&mut catalog, BIG);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "the busy wait is bounded by the configured timeout"
    );
    assert_eq!(native_ids(&first.sessions), vec!["b1"]);
    assert!(has_warning(
        &first.warnings,
        WarningKind::Unavailable,
        Some(&db_a)
    ));

    lock.execute_batch("COMMIT").expect("release lock");
    let mut recovered = Scan {
        sessions: Vec::new(),
        removed: Vec::new(),
        warnings: Vec::new(),
    };
    for _ in 0..20 {
        let delta = catalog.step(NOW, BIG).expect("recovery slice");
        recovered.sessions.extend(delta.sessions);
        recovered.warnings.extend(delta.warnings);
        if !recovered.sessions.is_empty() {
            break;
        }
        if delta.scan.done {
            // The next call starts a fresh pass and retries the failed profile.
            continue;
        }
    }
    assert_eq!(native_ids(&recovered.sessions), vec!["a1"]);
    assert_eq!(database(&recovered.sessions[0].key), db_a);
    drop(lock);
}

#[test]
fn reduced_extra_and_mistyped_schemas_warn_without_losing_sessions() {
    let dir = tmp();
    let home = dir.path().join("ok");
    let bad_home = dir.path().join("bad");
    let db = home.join("state.db");
    let bad_db = bad_home.join("state.db");
    {
        let writer = create_db(&db, false);
        insert_session(
            &writer,
            "legacy",
            None,
            serde_json::json!({}),
            1_793_318_400.0,
        );
        set_title(&writer, "legacy", "Legacy session");
        insert_message(&writer, "legacy", "user", "hello", 1_793_318_401.0);
        insert_session(
            &writer,
            "weird",
            None,
            serde_json::json!({"reasoning_config": {"enabled": true, "effort": "high"}}),
            1_793_318_410.0,
        );
        set_started_text(&writer, "weird", "not-a-timestamp");
        set_model_config_blob(&writer, "weird", &[0, 159, 146, 150]);
        // A future revision's extra columns are ignored, not selected.
        writer
            .execute(
                "UPDATE sessions SET future_note = 'future' WHERE id = 'legacy'",
                [],
            )
            .expect("future column");
        writer
            .execute(
                "INSERT INTO messages(session_id, role, content, timestamp, future_blob) \
                 VALUES ('legacy', 'assistant', 'x', 1.0, X'00')",
                [],
            )
            .expect("future message column");
    }
    fs::create_dir_all(&bad_home).expect("bad home");
    {
        // No messages table: the minimum supported shape is absent.
        let writer = rusqlite::Connection::open(&bad_db).expect("bad db");
        writer
            .execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY, started_at REAL)")
            .expect("minimal schema");
    }
    let before = fs::read(&db).expect("read main db");

    let mut config = config_for_homes(&home, &[&bad_home]);
    config.refresh_busy_timeout_ms = 50;
    let mut catalog = HermesCatalog::new(&config);
    let found = scan(&mut catalog, BIG);

    assert_eq!(found.sessions.len(), 2, "{:?}", native_ids(&found.sessions));
    let weird = found
        .sessions
        .iter()
        .find(|session| session.key.native_id == "weird")
        .expect("mistyped session stays visible");
    assert_eq!(weird.created_at, None);
    assert_eq!(
        weird.model.as_ref().unwrap().configured_effort,
        None,
        "a BLOB config is not the verified producer shape"
    );
    assert!(has_warning(
        &found.warnings,
        WarningKind::Unsupported,
        Some(&db)
    ));
    assert!(has_warning(
        &found.warnings,
        WarningKind::Unsupported,
        Some(&bad_db)
    ));
    let messages: Vec<&str> = found
        .warnings
        .iter()
        .filter(|warning| warning.key.source.contains(&db.display().to_string()))
        .map(|warning| warning.message.as_str())
        .collect();
    assert!(
        messages
            .iter()
            .any(|message| message.contains("optional column")),
        "a reduced schema must surface its missing optional columns"
    );
    assert!(
        messages
            .iter()
            .any(|message| message.contains("unexpected type")),
        "wrong storage types must warn"
    );
    let published = public_output(&found);
    assert!(!published.contains(SECRET));
    assert!(!published.contains("not-a-timestamp"));
    assert_eq!(fs::read(&db).expect("read main db"), before);

    // A producer migration in place is picked up on a later pass through the
    // same registered source.
    {
        let writer = rusqlite::Connection::open(&bad_db).expect("bad db");
        writer
            .execute_batch(
                "CREATE TABLE messages(id INTEGER PRIMARY KEY, session_id TEXT NOT NULL, \
                 role TEXT NOT NULL, content TEXT, timestamp REAL NOT NULL)",
            )
            .expect("migrated messages");
        insert_session(
            &writer,
            "migrated",
            None,
            serde_json::json!({}),
            1_793_318_700.0,
        );
        insert_message(&writer, "migrated", "user", "hello", 1_793_318_701.0);
    }
    let recovered = scan(&mut catalog, BIG);
    assert!(recovered.sessions.iter().any(|session| {
        database(&session.key) == bad_db && session.key.native_id == "migrated"
    }));
}

#[test]
fn discovery_of_a_thousand_rows_cannot_finish_in_one_work_slice() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    {
        let writer = create_db(&db, true);
        writer.execute_batch("BEGIN").expect("begin");
        for index in 0..1000 {
            insert_session(
                &writer,
                &format!("s{index:04}"),
                None,
                serde_json::json!({}),
                1_793_318_400.0 + index as f64,
            );
        }
        writer.execute_batch("COMMIT").expect("commit");
    }
    let budget = RefreshBudget {
        max_records: 100,
        max_bytes: 100_000_000,
        max_millis: 60_000,
    };
    let mut catalog = HermesCatalog::new(&config_for_db(&db));
    let first = catalog.step(NOW, budget).expect("first slice");
    assert!(!first.scan.done, "1000 rows must not finish under 100");
    assert_eq!(first.sessions.len(), 100);
    assert_eq!(first.scan.scanned, 100);

    let mut seen: HashSet<String> = first
        .sessions
        .iter()
        .map(|session| session.key.native_id.clone())
        .collect();
    let mut largest = first.sessions.len();
    let mut calls = 1;
    for _ in 0..40 {
        let delta = catalog.step(NOW, budget).expect("slice");
        calls += 1;
        largest = largest.max(delta.sessions.len());
        seen.extend(
            delta
                .sessions
                .iter()
                .map(|session| session.key.native_id.clone()),
        );
        if delta.scan.done {
            assert_eq!(seen.len(), 1000);
            assert_eq!(delta.scan.total, Some(1000));
            assert!(calls > 1);
            assert!(largest < 1000);
            return;
        }
    }
    panic!("discovery never completed within the slice budget");
}

// ---- review regressions -----------------------------------------------------

/// Runs one complete pass and returns its final (done) delta.
fn pass(
    catalog: &mut HermesCatalog,
    budget: RefreshBudget,
) -> agentscope::providers::DiscoveryDelta {
    for _ in 0..10_000 {
        let delta = catalog.step(NOW, budget).expect("discovery slice");
        if delta.scan.done {
            return delta;
        }
    }
    panic!("discovery never completed");
}

struct Driven {
    sessions: Vec<SessionMetadata>,
    removed: Vec<SessionKey>,
    total: Option<usize>,
}

/// Drives one complete pass through arbitrarily small slices, collecting every
/// slice's delta so per-slice emissions are observable.
fn drive_pass(catalog: &mut HermesCatalog, budget: RefreshBudget) -> Driven {
    let mut out = Driven {
        sessions: Vec::new(),
        removed: Vec::new(),
        total: None,
    };
    for _ in 0..100_000 {
        let delta = catalog.step(NOW, budget).expect("discovery slice");
        out.sessions.extend(delta.sessions);
        out.removed.extend(delta.removed);
        if delta.scan.done {
            out.total = delta.scan.total;
            return out;
        }
    }
    panic!("discovery never completed");
}

#[test]
fn bookkeeping_stays_bounded_past_the_metadata_ceiling() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    {
        let writer = create_db(&db, true);
        writer.execute_batch("BEGIN").expect("begin");
        for index in 0..5_000 {
            writer
                .execute(
                    "INSERT INTO sessions(id, source, started_at, model_config) \
                     VALUES (?1, 'cli', ?2, '{}')",
                    rusqlite::params![format!("s{index:05}"), 1_793_318_400.0 + f64::from(index)],
                )
                .expect("insert session");
        }
        writer.execute_batch("COMMIT").expect("commit");
    }
    let mut config = config_for_db(&db);
    config.max_metadata_sources = 10;
    let mut catalog = HermesCatalog::new(&config);
    let found = scan(&mut catalog, BIG);
    // Every session is still published, even past the ceiling.
    assert_eq!(found.sessions.len(), 5_000);
    assert!(found.removed.is_empty());
    // Only bounded bookkeeping survives: retained digests, a bounded forgotten
    // watch, and a per-pass seen set that never tracks an untracked ID.
    assert!(catalog.tracked_sources() <= 10);
    assert!(catalog.forgotten_sources() <= FORGOTTEN_CAP);
    assert!(
        catalog.peak_seen() <= 10 + FORGOTTEN_CAP,
        "seen set grew past the bounded bookkeeping: {}",
        catalog.peak_seen()
    );

    // A second pass over the same rows reports no deletions: displaced sessions
    // are still present, only their digest bookkeeping was evicted.
    let again = scan(&mut catalog, BIG);
    assert!(again.removed.is_empty());
}

#[test]
fn cross_source_displacement_never_leaks_seen_markers() {
    let dir = tmp();
    let home_a = dir.path().join("a");
    let home_b = dir.path().join("b");
    let db_a = home_a.join("state.db");
    let db_b = home_b.join("state.db");
    for (db, prefix) in [(&db_a, "a"), (&db_b, "b")] {
        let writer = create_db(db, true);
        writer.execute_batch("BEGIN").expect("begin");
        for index in 0..5_000 {
            writer
                .execute(
                    "INSERT INTO sessions(id, source, started_at, model_config) \
                     VALUES (?1, 'cli', 1793318400.0, '{}')",
                    rusqlite::params![format!("{prefix}{index:05}")],
                )
                .expect("insert session");
        }
        writer.execute_batch("COMMIT").expect("commit");
    }
    let mut config = config_for_homes(&home_a, &[&home_b]);
    config.max_metadata_sources = 1;
    let mut catalog = HermesCatalog::new(&config);
    // One record per slice, so evictions interleave between the two databases and
    // the shared forgotten watch displaces keys owned by the other source.
    let slice = RefreshBudget {
        max_records: 1,
        max_bytes: 100_000_000,
        max_millis: 60_000,
    };

    let first = drive_pass(&mut catalog, slice);
    assert_eq!(first.sessions.len(), 10_000);
    for db in [&db_a, &db_b] {
        assert_eq!(
            first
                .sessions
                .iter()
                .filter(|session| database(&session.key) == db)
                .count(),
            5_000,
            "both sources complete"
        );
    }
    assert!(first.removed.is_empty());
    assert_eq!(first.total, Some(10_000));
    assert_eq!(catalog.tracked_sources(), 1);
    assert!(catalog.forgotten_sources() <= FORGOTTEN_CAP);
    assert!(
        catalog.peak_seen() <= 1 + FORGOTTEN_CAP,
        "shared seen grew past bounded bookkeeping: {}",
        catalog.peak_seen()
    );

    // A completed second pass must not mistake a displaced key for a deletion.
    let second = drive_pass(&mut catalog, slice);
    assert!(
        second.removed.is_empty(),
        "displaced sessions were falsely removed: {}",
        second.removed.len()
    );
    assert!(
        catalog.peak_seen() <= 1 + FORGOTTEN_CAP,
        "shared seen grew on the second pass: {}",
        catalog.peak_seen()
    );
}

#[test]
fn oversized_model_config_is_never_parsed_and_stays_visible() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let padding = "x".repeat(200_000);
    {
        let writer = create_db(&db, true);
        insert_session(
            &writer,
            "huge",
            None,
            serde_json::json!({
                "reasoning_config": {"enabled": true, "effort": "high"},
                "unrelated": padding,
            }),
            1_793_318_400.0,
        );
    }
    let mut catalog = HermesCatalog::new(&config_for_db(&db));
    // The oversized cell is charged to the byte budget, not silently ignored.
    let first = catalog
        .step(
            NOW,
            RefreshBudget {
                max_records: 100,
                max_bytes: 4096,
                max_millis: 60_000,
            },
        )
        .expect("first slice");
    assert!(!first.scan.done);
    assert_eq!(first.sessions.len(), 1);
    assert_eq!(
        first.sessions[0].model.as_ref().unwrap().configured_effort,
        None,
        "an oversized config must not publish effort"
    );

    let rest = scan(&mut catalog, BIG);
    assert!(rest.sessions.is_empty(), "the session was already reported");
    let messages: Vec<&str> = first
        .warnings
        .iter()
        .chain(rest.warnings.iter())
        .map(|warning| warning.message.as_str())
        .collect();
    assert!(
        messages.iter().any(|message| message.contains("oversized")),
        "an oversized config warns safely: {messages:?}"
    );
    assert!(!public_output(&rest).contains("xxxxxxxx"));
}

#[test]
fn busy_timeout_is_recomputed_for_every_short_slice() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let writer = create_rollback_db(&db, true);
    insert_session(&writer, "s1", None, serde_json::json!({}), 1_793_318_400.0);
    let mut config = config_for_db(&db);
    config.refresh_busy_timeout_ms = 2_000;
    let mut catalog = HermesCatalog::new(&config);
    // The retained connection was opened while a long slice allowed a long wait.
    scan(&mut catalog, BIG);

    let lock = rusqlite::Connection::open(&db).expect("lock connection");
    lock.execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");
    let started = std::time::Instant::now();
    let delta = catalog
        .step(
            NOW,
            RefreshBudget {
                max_records: 1_000,
                max_bytes: 100_000_000,
                max_millis: 5,
            },
        )
        .expect("short slice");
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "a reused connection kept its 2 s busy timeout: {elapsed:?}"
    );
    assert!(delta.scan.done);
    assert!(has_warning(
        &delta.warnings,
        WarningKind::Unavailable,
        Some(&db)
    ));
    lock.execute_batch("COMMIT").expect("release");
    drop(lock);
    drop(writer);
}

#[test]
fn retained_connection_reprobes_a_migrated_supported_shape() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let started = 1_793_318_400.0;
    let writer = create_db(&db, false);
    insert_session(&writer, "s1", None, serde_json::json!({}), started);
    let mut catalog = HermesCatalog::new(&config_for_db(&db));
    let first = scan(&mut catalog, BIG);
    assert_eq!(first.sessions[0].created_at, Some(1_793_318_400_000));
    assert_eq!(first.sessions[0].activity.at, Some(1_793_318_400_000));
    assert!(!first.sessions[0].archived);

    // The producer migrates in place while the catalog keeps its connection.
    writer
        .execute_batch(
            "ALTER TABLE sessions ADD COLUMN last_activity_at REAL; \
             UPDATE sessions SET last_activity_at = 1793318500.0; \
             ALTER TABLE sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0; \
             UPDATE sessions SET archived = 1;",
        )
        .expect("in-place migration");
    let second = scan(&mut catalog, BIG);
    let session = second
        .sessions
        .iter()
        .find(|session| session.key.native_id == "s1")
        .expect("the migrated shape re-reports changed metadata");
    assert_eq!(session.activity.at, Some(1_793_318_500_000));
    assert!(session.archived);
    drop(writer);
}

#[test]
fn a_displaced_published_session_still_reports_its_confirmed_deletion() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(&writer, "a", None, serde_json::json!({}), 1_793_318_400.0);
    insert_session(&writer, "b", None, serde_json::json!({}), 1_793_318_401.0);
    let mut config = config_for_db(&db);
    config.max_metadata_sources = 1;
    let mut catalog = HermesCatalog::new(&config);
    let first = scan(&mut catalog, BIG);
    assert_eq!(native_ids(&first.sessions), vec!["a", "b"]);
    assert!(first.removed.is_empty());
    assert_eq!(catalog.tracked_sources(), 1);
    assert_eq!(catalog.forgotten_sources(), 1);

    writer
        .execute("DELETE FROM sessions WHERE id = 'a'", [])
        .expect("delete displaced session");
    let second = scan(&mut catalog, BIG);
    assert_eq!(second.removed.len(), 1);
    assert_eq!(second.removed[0].native_id, "a");
    drop(writer);
}

#[test]
fn scan_totals_are_refreshed_per_pass_and_invalidated_on_failure() {
    let dir = tmp();
    let home_a = dir.path().join("a");
    let home_b = dir.path().join("b");
    let db_a = home_a.join("state.db");
    let db_b = home_b.join("state.db");
    let writer_a = create_rollback_db(&db_a, true);
    insert_session(
        &writer_a,
        "a1",
        None,
        serde_json::json!({}),
        1_793_318_400.0,
    );
    insert_session(
        &writer_a,
        "a2",
        None,
        serde_json::json!({}),
        1_793_318_401.0,
    );
    let writer_b = create_rollback_db(&db_b, true);
    insert_session(
        &writer_b,
        "b1",
        None,
        serde_json::json!({}),
        1_793_318_500.0,
    );
    let mut config = config_for_homes(&home_a, &[&home_b]);
    config.refresh_busy_timeout_ms = 25;
    let mut catalog = HermesCatalog::new(&config);

    assert_eq!(pass(&mut catalog, BIG).scan.total, Some(3));

    writer_a
        .execute(
            "INSERT INTO sessions(id, source, started_at, model_config) \
             VALUES ('a3', 'cli', 1793318402.0, '{}')",
            [],
        )
        .expect("insert");
    assert_eq!(pass(&mut catalog, BIG).scan.total, Some(4));

    writer_a
        .execute_batch("DELETE FROM sessions WHERE id IN ('a2','a3')")
        .expect("delete");
    assert_eq!(pass(&mut catalog, BIG).scan.total, Some(2));

    // A locked database contributes no stale count at all.
    let lock = rusqlite::Connection::open(&db_b).expect("lock connection");
    lock.execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");
    let locked = pass(&mut catalog, BIG);
    assert_eq!(locked.scan.total, None);
    assert!(has_warning(
        &locked.warnings,
        WarningKind::Unavailable,
        Some(&db_b)
    ));
    lock.execute_batch("COMMIT").expect("release");
    assert_eq!(pass(&mut catalog, BIG).scan.total, Some(2));
    drop(lock);
    drop(writer_a);
    drop(writer_b);
}

// ---- read-only smoke --------------------------------------------------------

#[test]
fn read_only_smoke_over_wal_locked_and_malformed_profiles() {
    let dir = tmp();
    let home = dir.path().join("hermes");
    let root_db = home.join("state.db");
    let work_db = home.join("profiles").join("work").join("state.db");
    let locked_db = home.join("profiles").join("locked").join("state.db");
    let root_writer = create_db(&root_db, true);
    insert_session(
        &root_writer,
        "s1",
        None,
        serde_json::json!({
            "reasoning_config": {"enabled": true, "effort": "high"},
            "api_key": SECRET,
        }),
        1_793_318_400.0,
    );
    set_title(&root_writer, "s1", "Main session");
    insert_message(&root_writer, "s1", "user", "hello", 1_793_318_460.0);
    insert_session(
        &root_writer,
        "s2",
        None,
        serde_json::json!({"max_tokens": 1}),
        1_793_318_470.0,
    );
    set_model_config_raw(&root_writer, "s2", "{broken");
    {
        let work = create_db(&work_db, true);
        insert_session(&work, "s1", None, serde_json::json!({}), 1_793_318_500.0);
        set_title(&work, "s1", "Work session");
    }
    let lock = create_rollback_db(&locked_db, true);
    insert_session(&lock, "s1", None, serde_json::json!({}), 1_793_318_600.0);
    lock.execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");

    let root_before = fs::read(&root_db).expect("root bytes");
    let work_before = fs::read(&work_db).expect("work bytes");
    let locked_before = fs::read(&locked_db).expect("locked bytes");
    let names_before = paths(&home);

    let mut config = config_for_home(&home);
    config.refresh_busy_timeout_ms = 25;
    let mut catalog = HermesCatalog::new(&config);
    let found = scan(&mut catalog, BIG);

    assert_eq!(found.sessions.len(), 3, "{:?}", native_ids(&found.sessions));
    let keys: HashSet<&SourceKey> = found.sessions.iter().map(|s| &s.key.source).collect();
    assert_eq!(keys.len(), 3, "each profile DB keeps a distinct source");
    let root_s1 = found
        .sessions
        .iter()
        .find(|s| database(&s.key) == root_db && s.key.native_id == "s1")
        .expect("root s1");
    assert_eq!(
        root_s1.model.as_ref().unwrap().configured_effort.as_deref(),
        Some("high")
    );
    let root_s2 = found
        .sessions
        .iter()
        .find(|s| database(&s.key) == root_db && s.key.native_id == "s2")
        .expect("root s2");
    assert_eq!(root_s2.model.as_ref().unwrap().configured_effort, None);
    assert!(
        found
            .sessions
            .iter()
            .any(|s| database(&s.key) == work_db && s.title == "Work session")
    );
    assert!(has_warning(
        &found.warnings,
        WarningKind::Unavailable,
        Some(&locked_db)
    ));
    let published = public_output(&found);
    assert!(!published.contains(SECRET));
    assert!(!published.contains("api_key"));

    // Source content is unchanged; only documented transient sidecars may differ.
    assert_eq!(fs::read(&root_db).expect("root bytes"), root_before);
    assert_eq!(fs::read(&work_db).expect("work bytes"), work_before);
    assert_eq!(fs::read(&locked_db).expect("locked bytes"), locked_before);
    let names_after = paths(&home);
    for name in &names_after {
        assert!(
            names_before.contains(name)
                || name.ends_with("-wal")
                || name.ends_with("-shm")
                || name.ends_with("-journal"),
            "unexpected file appeared: {name}"
        );
    }
    println!(
        "hermes smoke: {} sessions, {} sources, {} warnings",
        found.sessions.len(),
        keys.len(),
        found.warnings.len()
    );
    drop(root_writer);
    drop(lock);
}

fn paths(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            out.push(
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
    out
}
