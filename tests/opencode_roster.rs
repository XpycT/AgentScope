mod support;

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use agentscope::domain::{
    Harness, RefreshBudget, SessionKey, SessionLink, SessionSummary, SourceKey, Warning,
    WarningKind,
};
use agentscope::providers::opencode::{OpenCodeScanner, Schema, detect_schema, open_read_only};
use agentscope::providers::{ProviderError, ProviderReader};
use support::NOW;
use support::opencode::*;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

struct Scan {
    sessions: HashMap<String, SessionSummary>,
    removed: Vec<SessionKey>,
    warnings: Vec<Warning>,
}

fn scan(scanner: &mut OpenCodeScanner, budget: RefreshBudget) -> Scan {
    let mut out = Scan {
        sessions: HashMap::new(),
        removed: vec![],
        warnings: vec![],
    };
    loop {
        let delta = support::world::rescan(scanner, NOW, budget);
        for s in delta.sessions {
            out.sessions.insert(s.meta.key.native_id.clone(), s);
        }
        out.removed.extend(delta.removed);
        out.warnings.extend(delta.warnings);
        if delta.scan.done {
            return out;
        }
    }
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn scanner_for(db: &Path) -> OpenCodeScanner {
    OpenCodeScanner::new(&config_for_db(db)).expect("scanner")
}

fn file_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).expect("read file")
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn opens_read_only_with_authorizer_and_rejects_sensitive_tables() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        insert_session(&w, &Session::new("ses_1", 1, 2));
        insert_message(&w, "m1", "ses_1", 1, 2);
    }
    let conn = open_read_only(&db).expect("open");

    // Allowlisted reads work.
    let n: i64 = conn
        .query_row("SELECT count(*) FROM session_v2", [], |r| r.get(0))
        .expect("allowed count");
    assert_eq!(n, 1);
    let id: String = conn
        .query_row("SELECT id FROM session_v2", [], |r| r.get(0))
        .expect("allowed select");
    assert_eq!(id, "ses_1");
    let agent: Option<String> = conn
        .query_row("SELECT agent FROM session_v2", [], |r| r.get(0))
        .expect("allowed agent select");
    assert_eq!(agent, None);

    // Sensitive tables, share URLs, non-allowlisted columns, message content, schema
    // writes and pragmas are denied.
    for sql in [
        "SELECT * FROM account",
        "SELECT secret FROM control_account",
        "SELECT secret FROM credential",
        "SELECT share_url FROM session_share",
        "SELECT share_url FROM session_v2",
        "SELECT metadata FROM session_v2",
        "SELECT * FROM session_v2",
        "SELECT seq FROM session_message",
        "INSERT INTO project(id, name) VALUES ('x', 'y')",
        "UPDATE session_v2 SET title = 'x'",
        "DELETE FROM session_v2",
        "CREATE TABLE t(x)",
        "DROP TABLE project",
        "PRAGMA query_only = OFF",
        "PRAGMA journal_mode = DELETE",
        "ATTACH DATABASE ':memory:' AS other",
        "SELECT load_extension('x')",
        "SELECT readfile('x')",
    ] {
        assert!(conn.prepare(sql).is_err(), "must be denied: {sql}");
    }
    // A join cannot smuggle a denied column past the authorizer.
    assert!(
        conn.prepare("SELECT s.id FROM session_v2 s JOIN account a ON a.id = s.id")
            .is_err()
    );
    assert!(
        conn.prepare(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM c) SELECT x FROM c"
        )
        .is_err()
    );
}

#[test]
fn selects_v2_then_legacy_then_unsupported() {
    let dir = tmp();

    // v2 with message rows.
    let v2 = dir.path().join("v2.db");
    {
        let w = create_v2(&v2);
        insert_session(&w, &Session::new("s", 1, 2));
        insert_message(&w, "m", "s", 1, 2);
        // Legacy tables are ignored when v2 is populated.
        w.execute_batch("CREATE TABLE session(id TEXT); CREATE TABLE message(id TEXT); CREATE TABLE part(id TEXT);")
            .unwrap();
    }
    assert_eq!(
        detect_schema(&open_read_only(&v2).unwrap()).unwrap(),
        Schema::V2
    );

    // Legacy only.
    let legacy = dir.path().join("legacy.db");
    {
        let w = create_legacy(&legacy);
        insert_legacy_session(&w, &Session::new("s", 1, 2));
        insert_legacy_message(&w, "m", "s", 1, 2);
    }
    assert_eq!(
        detect_schema(&open_read_only(&legacy).unwrap()).unwrap(),
        Schema::Legacy
    );

    // v2 tables exist but are empty and legacy has rows: legacy wins.
    let both = dir.path().join("both.db");
    {
        let w = create(&both, &format!("{V2_SCHEMA_NO_PROJECT}{LEGACY_NO_PROJECT}"));
        insert_legacy_session(&w, &Session::new("s", 1, 2));
        insert_legacy_message(&w, "m", "s", 1, 2);
    }
    assert_eq!(
        detect_schema(&open_read_only(&both).unwrap()).unwrap(),
        Schema::Legacy
    );

    // v2 shape and nothing anywhere: still v2 (an empty roster, not unsupported).
    let empty = dir.path().join("empty.db");
    drop(create_v2(&empty));
    assert_eq!(
        detect_schema(&open_read_only(&empty).unwrap()).unwrap(),
        Schema::V2
    );

    // Unrecognised shapes.
    let other = dir.path().join("other.db");
    drop(create(&other, "CREATE TABLE unrelated(x INTEGER);"));
    assert_eq!(
        detect_schema(&open_read_only(&other).unwrap()).unwrap(),
        Schema::Unsupported
    );
    let missing_column = dir.path().join("missing.db");
    drop(create(
        &missing_column,
        "CREATE TABLE session_v2(id TEXT, title TEXT);
         CREATE TABLE session_message(id TEXT, session_id TEXT);",
    ));
    assert_eq!(
        detect_schema(&open_read_only(&missing_column).unwrap()).unwrap(),
        Schema::Unsupported
    );

    // An unsupported database becomes a warning, not an error and not a guess.
    let mut scanner = scanner_for(&other);
    let out = scan(&mut scanner, BIG);
    assert!(out.sessions.is_empty());
    assert!(
        out.warnings
            .iter()
            .any(|w| w.key.kind == WarningKind::Unsupported && w.key.harness == Harness::OpenCode)
    );
}

const V2_SCHEMA_NO_PROJECT: &str = "
CREATE TABLE session_v2(
    id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, fork_session_id TEXT,
    title TEXT, directory TEXT, model TEXT, version TEXT,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, time_archived INTEGER);
CREATE TABLE session_message(
    id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
";

const LEGACY_NO_PROJECT: &str = "
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
";

#[test]
fn legacy_fallback_when_v2_is_empty_warns() {
    let dir = tmp();
    let db = dir.path().join("both.db");
    {
        let w = create(&db, &format!("{V2_SCHEMA_NO_PROJECT}{LEGACY_NO_PROJECT}"));
        let mut s = Session::new("old", 10, 20);
        s.parent_id = Some("older");
        insert_legacy_session(&w, &s);
        insert_legacy_session(&w, &Session::new("older", 5, 6));
        insert_legacy_message(&w, "m", "old", 10, 30);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    let s = &out.sessions["old"];
    assert_eq!(
        s.meta.key.source,
        SourceKey::DbRow {
            database: db.clone(),
            table: "session".into(),
            id: "old".into()
        }
    );
    assert_eq!(s.meta.activity.at, Some(30));
    assert!(
        matches!(&s.meta.lineage.parent, Some(SessionLink::Resolved(k)) if k.native_id == "older")
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.key.kind == WarningKind::Unsupported)
    );
}

#[test]
fn maps_title_directory_model_lineage_archived_and_time_updated() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        insert_project(&w, "p1", "Project One");
        let model = model_json("gpt-5", "openai", "high");
        let mut parent = Session::new("ses_parent", 100, 200);
        parent.title = Some("Parent  title");
        parent.directory = Some("/work/alpha");
        parent.project_id = Some("p1");
        parent.model = Some(&model);
        insert_session(&w, &parent);
        let mut child = Session::new("ses_child", 150, 300);
        child.parent_id = Some("ses_parent");
        child.fork_id = Some("ses_ghost");
        child.title = None;
        child.directory = Some("/work/beta-dir");
        child.archived = Some(250);
        insert_session(&w, &child);
        // Session row is stale: a message was updated later.
        insert_message(&w, "m1", "ses_parent", 110, 900);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    assert_eq!(out.sessions.len(), 2);

    let p = &out.sessions["ses_parent"];
    assert_eq!(p.meta.key.harness, Harness::OpenCode);
    assert_eq!(
        p.meta.key.source,
        SourceKey::DbRow {
            database: db.clone(),
            table: "session_v2".into(),
            id: "ses_parent".into()
        }
    );
    assert_eq!(p.meta.title, "Parent title");
    assert!(!p.meta.title_derived);
    assert_eq!(p.meta.cwd.as_deref(), Some(Path::new("/work/alpha")));
    assert_eq!(p.meta.project.as_deref(), Some("Project One"));
    assert_eq!(p.meta.model.as_ref().map(|m| m.id.as_str()), Some("gpt-5"));
    assert_eq!(p.meta.harness_version.as_deref(), Some("2.0.29"));
    assert_eq!(p.meta.created_at, Some(100));
    assert_eq!(
        p.meta.activity.at,
        Some(900),
        "stale session row uses newest message time_updated"
    );
    assert!(!p.meta.archived);
    assert!(p.meta.source_available);
    assert!(p.meta.lineage.parent.is_none() && p.meta.lineage.fork.is_none());
    assert!(p.usage.is_none());

    let c = &out.sessions["ses_child"];
    assert_eq!(
        c.meta.title, "ses_child",
        "no provider title falls back to the session id"
    );
    assert_eq!(
        c.meta.project.as_deref(),
        Some("beta-dir"),
        "project falls back to the directory basename"
    );
    assert!(c.meta.archived);
    assert_eq!(c.meta.activity.at, Some(300));
    assert!(c.meta.model.is_none());
    match &c.meta.lineage.parent {
        Some(SessionLink::Resolved(k)) => {
            assert_eq!(k, &p.meta.key);
        }
        other => panic!("parent: {other:?}"),
    }
    assert_eq!(
        c.meta.lineage.fork,
        Some(SessionLink::Unresolved("ses_ghost".into()))
    );
    assert!(!c.meta.lineage.sidechain);

    // Sensitive text never reaches the roster.
    let dump = format!(
        "{:?}",
        out.sessions
            .values()
            .map(|s| (&s.meta.title, &s.meta.project, &s.warnings))
            .collect::<Vec<_>>()
    );
    assert!(!dump.contains("SECRET"));
}

#[test]
fn in_place_message_update_advances_last_persisted_at() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_session(&writer, &Session::new("s", 100, 200));
    insert_message(&writer, "m", "s", 110, 120);
    let mut scanner = scanner_for(&db);
    let first = scan(&mut scanner, BIG);
    assert_eq!(first.sessions["s"].meta.activity.at, Some(200));

    // Unchanged database: nothing is re-emitted.
    assert!(scan(&mut scanner, BIG).sessions.is_empty());

    // The message row is rewritten in place; the session row is not touched.
    writer
        .execute(
            "UPDATE session_message SET time_updated = 5000 WHERE id = 'm'",
            [],
        )
        .unwrap();
    let second = scan(&mut scanner, BIG);
    assert_eq!(second.sessions["s"].meta.activity.at, Some(5000));
    assert!(second.removed.is_empty());

    // Rows disappearing are reported as removed once a pass completes.
    writer.execute("DELETE FROM session_v2", []).unwrap();
    let third = scan(&mut scanner, BIG);
    assert_eq!(third.removed.len(), 1);
    assert_eq!(third.removed[0].native_id, "s");
}

#[test]
fn displayed_event_count_is_a_monotonic_message_activity_count() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_session(&writer, &Session::new("s", 100, 200));
    insert_message(&writer, "m0", "s", 110, 120);
    let mut scanner = scanner_for(&db);
    let first = scan(&mut scanner, BIG);
    assert_eq!(first.sessions["s"].displayed_event_count, 1);

    // Appends advance the count even though OpenCode rows expand to a different
    // number of transcript events: it is an activity count, not displayed rows.
    let mut last = 1;
    for (i, id) in ["m1", "m2", "m3"].into_iter().enumerate() {
        insert_message(&writer, id, "s", 130 + i as i64, 140 + i as i64);
        let next = scan(&mut scanner, BIG);
        let count = next.sessions["s"].displayed_event_count;
        assert!(count > last, "{count} should exceed {last}");
        last = count;
    }
    assert_eq!(last, 4);
}

#[test]
fn roster_pages_within_budget_and_honours_session_cap() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        for i in 0..5 {
            insert_session(&w, &Session::new(&format!("s{i}"), 1, 100 + i));
        }
    }
    let mut scanner = scanner_for(&db);
    let small = RefreshBudget {
        max_records: 2,
        ..BIG
    };
    let mut calls = 0;
    let mut ids = vec![];
    loop {
        let d = scanner.discover(NOW, small).unwrap();
        calls += 1;
        assert!(d.sessions.len() <= 2);
        ids.extend(d.sessions.iter().map(|s| s.key.native_id.clone()));
        if d.scan.done {
            break;
        }
    }
    assert_eq!(calls, 3);
    assert_eq!(
        ids,
        ["s4", "s3", "s2", "s1", "s0"],
        "newest time_updated first"
    );
}

#[test]
fn live_writer_with_existing_shm_sees_wal_only_rows() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_session(&writer, &Session::new("wal_only", 1, 2));
    assert!(sidecar(&db, "-wal").exists());
    assert!(sidecar(&db, "-shm").exists());
    let main_before = file_bytes(&db);

    let out = scan(&mut scanner_for(&db), BIG);
    assert!(out.sessions.contains_key("wal_only"));
    assert!(out.sessions["wal_only"].meta.source_available);
    assert_eq!(file_bytes(&db), main_before, "main database untouched");

    // The live writer keeps working while the reader is open.
    insert_session(&writer, &Session::new("later", 3, 4));
    drop(writer);
}

#[test]
fn stale_wal_with_writable_directory_reads_and_lets_sqlite_manage_sidecars() {
    let live = tmp();
    let live_db = live.path().join("opencode.db");
    let writer = create_v2(&live_db);
    insert_session(&writer, &Session::new("stale_wal", 1, 2));

    // Copy the database and WAL without the shm (fixture only; never provider files).
    let stale = tmp();
    let db = stale.path().join("opencode.db");
    fs::copy(&live_db, &db).unwrap();
    fs::copy(sidecar(&live_db, "-wal"), sidecar(&db, "-wal")).unwrap();
    assert!(!sidecar(&db, "-shm").exists());
    let main_before = file_bytes(&db);

    let out = scan(&mut scanner_for(&db), BIG);
    assert!(
        out.sessions.contains_key("stale_wal"),
        "WAL rows are visible"
    );
    assert_eq!(file_bytes(&db), main_before, "main database untouched");
    // SQLite may have created -shm; that is the ADR-0002 exception.
    let allowed = ["opencode.db", "opencode.db-shm", "opencode.db-wal"];
    assert!(
        files_in(stale.path())
            .iter()
            .all(|f| allowed.contains(&f.as_str()))
    );
    drop(writer);
}

#[test]
fn no_active_wal_opens_read_only() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        insert_session(&w, &Session::new("plain", 1, 2));
    }
    assert!(
        !sidecar(&db, "-wal").exists(),
        "closing the last connection removes sidecars"
    );
    let main_before = file_bytes(&db);

    let out = scan(&mut scanner_for(&db), BIG);
    assert!(out.sessions.contains_key("plain"));
    assert_eq!(file_bytes(&db), main_before);
}

#[test]
fn unusable_sidecars_show_unavailable_and_are_left_alone() {
    let live = tmp();
    let live_db = live.path().join("opencode.db");
    let writer = create_v2(&live_db);
    insert_session(&writer, &Session::new("hidden", 1, 2));

    // The database is WAL-mode, but both sidecar names are occupied by directories. SQLite
    // cannot open the WAL read-only on Unix (where an absent `-shm` alone is tolerated) or
    // on Windows. No chmod, no repair, no copy of provider files.
    let blocked = tmp();
    let db = blocked.path().join("opencode.db");
    fs::copy(&live_db, &db).unwrap();
    fs::create_dir(sidecar(&db, "-shm")).unwrap();
    fs::create_dir(sidecar(&db, "-wal")).unwrap();
    let main_before = file_bytes(&db);

    let result = OpenCodeScanner::new(&config_for_db(&db));
    assert!(
        matches!(result, Err(ProviderError::Unavailable(_))),
        "got {:?}",
        result.err()
    );
    assert_eq!(file_bytes(&db), main_before);
    assert!(
        sidecar(&db, "-shm").is_dir() && sidecar(&db, "-wal").is_dir(),
        "nothing is deleted or repaired"
    );
    drop(writer);
}

#[test]
fn main_database_is_unchanged_and_only_sidecars_may_change() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_project(&writer, "p", "P");
    insert_session(&writer, &Session::new("s", 1, 2));
    insert_message(&writer, "m", "s", 1, 3);
    let main_before = file_bytes(&db);
    let names_before = files_in(dir.path());

    let mut scanner = scanner_for(&db);
    for _ in 0..3 {
        scan(&mut scanner, BIG);
    }
    drop(scanner);

    assert_eq!(
        file_bytes(&db),
        main_before,
        "main database bytes unchanged"
    );
    let names_after = files_in(dir.path());
    assert!(
        names_after
            .iter()
            .all(|f| names_before.contains(f) || f == "opencode.db-shm" || f == "opencode.db-wal"),
        "only sidecars may appear: {names_after:?}"
    );
    // Session content is intact for the writer.
    let content: String = writer
        .query_row("SELECT data FROM session_message WHERE id='m'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(content.contains("SECRET-CONTENT"));
}

#[test]
fn model_keeps_id_for_pricing_and_provider_variant_for_display() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        let model = model_json("gpt-5", "openai", "high");
        let mut s = Session::new("a", 1, 3);
        s.model = Some(&model);
        s.agent = Some("build");
        insert_session(&w, &s);
        let mut bare = Session::new("b", 1, 2);
        bare.model = Some("plain-model");
        insert_session(&w, &bare);
        // A variant that is not an observed thinking level stays raw, never effort.
        let default = model_json("gpt-5", "openai", "default");
        let mut d = Session::new("d", 1, 4);
        d.model = Some(&default);
        insert_session(&w, &d);
        // A custom/unknown model may reuse an effort-looking variant for something
        // else: the raw value stays, effort is Unknown.
        let custom = model_json("my-custom-model", "openai", "high");
        let mut c = Session::new("c", 1, 6);
        c.model = Some(&custom);
        insert_session(&w, &c);
        // No provider: Unknown route, never a default vendor or effort.
        let unrecorded = model_json("gpt-5", "", "high");
        let mut u = Session::new("u", 1, 5);
        u.model = Some(&unrecorded);
        insert_session(&w, &u);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    let a = &out.sessions["a"];
    assert_eq!(a.meta.model.as_ref().map(|m| m.id.as_str()), Some("gpt-5"));
    assert_eq!(a.meta.model.as_ref().unwrap().id, "gpt-5");
    assert_eq!(
        a.meta.model.as_ref().unwrap().provider.as_deref(),
        Some("openai")
    );
    assert_eq!(
        a.meta.model.as_ref().unwrap().variant.as_deref(),
        Some("high"),
        "the raw variant is retained"
    );
    assert_eq!(
        a.meta.model.as_ref().unwrap().effort.as_deref(),
        Some("high"),
        "an observed thinking level is also an effort"
    );
    assert_eq!(a.meta.lineage.agent_type.as_deref(), Some("build"));
    let b = &out.sessions["b"];
    assert_eq!(
        b.meta.model.as_ref().map(|m| m.id.as_str()),
        Some("plain-model")
    );
    assert_eq!(b.meta.model.as_ref().unwrap().provider, None);
    assert_eq!(b.meta.model.as_ref().unwrap().effort, None);
    let d = &out.sessions["d"];
    assert_eq!(
        d.meta.model.as_ref().unwrap().variant.as_deref(),
        Some("default"),
        "an unrecognized variant is never promoted to effort"
    );
    assert_eq!(d.meta.model.as_ref().unwrap().effort, None);
    let c = &out.sessions["c"];
    assert_eq!(
        c.meta.model.as_ref().unwrap().variant.as_deref(),
        Some("high"),
        "a custom model's raw variant is retained"
    );
    assert_eq!(
        c.meta.model.as_ref().unwrap().effort,
        None,
        "an unknown model route never promotes a variant to effort"
    );
    let u = &out.sessions["u"];
    assert_eq!(u.meta.model.as_ref().unwrap().provider, None);
    assert_eq!(
        u.meta.model.as_ref().unwrap().variant.as_deref(),
        Some("high")
    );
    assert_eq!(u.meta.model.as_ref().unwrap().effort, None);
}

#[test]
fn cap_displaced_sessions_are_evicted_across_refreshes() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db);
    insert_session(&writer, &Session::new("a", 1, 10));
    insert_session(&writer, &Session::new("b", 1, 20));
    let mut cfg = config_for_db(&db);
    cfg.max_cached_sessions = 2;
    let mut scanner = OpenCodeScanner::new(&cfg).unwrap();
    let first = scan(&mut scanner, BIG);
    assert_eq!(first.sessions.len(), 2);
    assert!(first.removed.is_empty());

    // Metadata is not capped by the detail cache: the new session is listed and
    // nothing is displaced.
    insert_session(&writer, &Session::new("c", 1, 30));
    let second = scan(&mut scanner, BIG);
    assert!(second.sessions.contains_key("c"));
    assert!(second.removed.is_empty());
    assert!(scanner.cached_sessions() <= 2);

    // Retained set stays at the cap and is stable.
    let third = scan(&mut scanner, BIG);
    assert!(third.sessions.is_empty() && third.removed.is_empty());

    // The displaced session returns once it is among the newest again.
    writer
        .execute("UPDATE session_v2 SET time_updated = 99 WHERE id = 'a'", [])
        .unwrap();
    let fourth = scan(&mut scanner, BIG);
    assert!(fourth.sessions.contains_key("a"));
    assert!(fourth.removed.is_empty(), "no session is ever displaced");
    assert!(scanner.cached_sessions() <= 2);
}

fn seeded(n: usize) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        for i in 0..n {
            insert_session(&w, &Session::new(&format!("s{i}"), 1, 100 + i as i64));
        }
    }
    (dir, db)
}

#[test]
fn exhausted_record_byte_and_time_budgets_read_nothing_and_never_finish() {
    let (_dir, db) = seeded(3);
    for budget in [
        RefreshBudget {
            max_records: 0,
            ..BIG
        },
        RefreshBudget {
            max_bytes: 0,
            ..BIG
        },
        RefreshBudget {
            max_millis: 0,
            ..BIG
        },
    ] {
        let mut scanner = scanner_for(&db);
        let d = scanner.discover(NOW, budget).unwrap();
        assert!(d.sessions.is_empty(), "{budget:?}");
        assert!(!d.scan.done, "{budget:?}");
        assert_eq!(d.scan.scanned, 0);
        // A real budget afterwards completes the same pass.
        let all = scan(&mut scanner, BIG);
        assert_eq!(all.sessions.len(), 3);
    }
}

#[test]
fn byte_budget_bounds_a_page_but_always_progresses() {
    let (_dir, db) = seeded(5);
    let mut scanner = scanner_for(&db);
    let tiny = RefreshBudget {
        max_bytes: 1,
        ..BIG
    };
    let mut calls = 0;
    let mut seen = 0;
    loop {
        let d = scanner.discover(NOW, tiny).unwrap();
        calls += 1;
        assert!(
            d.sessions.len() <= 1,
            "one row per call under a 1-byte budget"
        );
        seen += d.sessions.len();
        if d.scan.done {
            break;
        }
        assert!(calls < 20);
    }
    assert_eq!(seen, 5);
}

#[test]
fn oversized_text_columns_are_bounded_at_the_query() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        let huge = "x".repeat(2_000_000);
        let mut s = Session::new("big", 1, 2);
        s.title = Some(&huge);
        s.directory = Some(&huge);
        insert_session(&w, &s);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    let s = &out.sessions["big"];
    assert!(s.meta.title.len() <= 1024, "title {}", s.meta.title.len());
    assert!(s.meta.cwd.as_ref().unwrap().as_os_str().len() <= 4096);
}

/// Column names verified read-only (schema only, no row content) against a local
/// OpenCode 2.0.29 database. The adapter must accept the real column set.
#[test]
fn real_opencode_2_0_29_column_set_is_recognised() {
    const REAL_V2: &str = "
CREATE TABLE project(id TEXT PRIMARY KEY, worktree TEXT, vcs TEXT, name TEXT, icon_url TEXT,
    icon_url_override TEXT, icon_color TEXT, time_created INTEGER, time_updated INTEGER,
    time_initialized INTEGER, sandboxes TEXT, commands TEXT, time_active INTEGER, time_archived INTEGER);
CREATE TABLE session_v2(id TEXT PRIMARY KEY, project_id TEXT, workspace_id TEXT, parent_id TEXT,
    fork_session_id TEXT, fork_boundary TEXT, slug TEXT, directory TEXT, path TEXT, title TEXT,
    version TEXT, share_url TEXT, summary_additions INTEGER, summary_deletions INTEGER,
    summary_files INTEGER, summary_diffs TEXT, metadata TEXT, cost REAL, tokens_input INTEGER,
    tokens_output INTEGER, tokens_reasoning INTEGER, tokens_cache_read INTEGER,
    tokens_cache_write INTEGER, revert TEXT, permission TEXT, agent TEXT, model TEXT,
    time_created INTEGER, time_updated INTEGER, time_compacting INTEGER, time_archived INTEGER,
    time_suspended INTEGER, resume_attempts INTEGER, time_idle INTEGER, time_viewed INTEGER,
    idle_outcome TEXT);
CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER,
    time_created INTEGER, time_updated INTEGER, data TEXT);
";
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create(&db, REAL_V2);
        w.execute(
            "INSERT INTO session_v2(id, title, model, time_created, time_updated) VALUES ('s', 't', ?1, 1, 2)",
            [model_json("m", "p", "v")],
        )
        .unwrap();
    }
    assert_eq!(
        detect_schema(&open_read_only(&db).unwrap()).unwrap(),
        Schema::V2
    );
    let out = scan(&mut scanner_for(&db), BIG);
    assert_eq!(
        out.sessions["s"]
            .meta
            .model
            .as_ref()
            .unwrap()
            .provider
            .as_deref(),
        Some("p")
    );
}

#[cfg(unix)]
#[test]
fn read_only_directory_with_required_sidecar_shows_unavailable() {
    use std::os::unix::fs::PermissionsExt;
    let live = tmp();
    let live_db = live.path().join("opencode.db");
    let writer = create_v2(&live_db);
    insert_session(&writer, &Session::new("hidden", 1, 2));

    let ro = tmp();
    let db = ro.path().join("opencode.db");
    fs::copy(&live_db, &db).unwrap();
    fs::copy(sidecar(&live_db, "-wal"), sidecar(&db, "-wal")).unwrap();
    fs::set_permissions(ro.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let probe = fs::File::create(ro.path().join("probe"));
    let result = if probe.is_ok() {
        // Running as root: permissions are not enforced; nothing to assert.
        None
    } else {
        Some(OpenCodeScanner::new(&config_for_db(&db)))
    };
    fs::set_permissions(ro.path(), fs::Permissions::from_mode(0o755)).unwrap();
    if let Some(result) = result {
        assert!(
            matches!(result, Err(ProviderError::Unavailable(_))),
            "got {:?}",
            result.err()
        );
        assert!(
            !sidecar(&db, "-shm").exists(),
            "no sidecar was created or repaired"
        );
    }
    drop(writer);
}

#[test]
fn long_ids_sharing_a_prefix_stay_distinct_and_page_correctly() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let prefix = "p".repeat(1000);
    let ids: Vec<String> = (0..4).map(|i| format!("{prefix}{i}")).collect();
    {
        let w = create_v2(&db);
        // Same time_updated so the keyset cursor must order by the exact id.
        for id in &ids {
            insert_session(&w, &Session::new(id, 1, 50));
        }
        let mut child = Session::new("child", 1, 40);
        child.parent_id = Some(&ids[2]);
        insert_session(&w, &child);
    }
    let mut scanner = scanner_for(&db);
    let one = RefreshBudget {
        max_records: 1,
        ..BIG
    };
    let mut seen = vec![];
    loop {
        let d = support::world::rescan(&mut scanner, NOW, one);
        seen.extend(d.sessions.into_iter().map(|s| s.meta.key));
        if d.scan.done {
            break;
        }
    }
    assert_eq!(seen.len(), 5, "no collision, no skipped row");
    let got: std::collections::HashSet<_> = seen.iter().map(|k| k.native_id.clone()).collect();
    assert_eq!(got.len(), 5);
    for id in &ids {
        assert!(got.contains(id), "exact id preserved");
    }
    let out = scan(&mut scanner_for(&db), BIG);
    match &out.sessions["child"].meta.lineage.parent {
        Some(SessionLink::Resolved(k)) => assert_eq!(k.native_id, ids[2]),
        other => panic!("{other:?}"),
    }
}

#[test]
fn identifiers_too_long_to_key_exactly_are_skipped_with_a_warning() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        insert_session(&w, &Session::new(&"z".repeat(5000), 1, 9));
        insert_session(&w, &Session::new("ok", 1, 8));
    }
    let out = scan(&mut scanner_for(&db), BIG);
    assert_eq!(out.sessions.len(), 1);
    assert!(out.sessions.contains_key("ok"));
    assert!(
        out.warnings
            .iter()
            .any(|w| w.count == 1 && w.message.contains("identifier"))
    );
}

#[test]
fn long_model_json_is_whole_or_unknown_never_a_fragment() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        // Valid JSON whose variant exceeds the old 4096-char cut.
        let long = model_json("gpt-5", "openai", &"v".repeat(10_000));
        let mut a = Session::new("a", 1, 3);
        a.model = Some(&long);
        insert_session(&w, &a);
        // Beyond any sane size: Unknown, not a fabricated id.
        let huge = model_json("gpt-5", "openai", &"v".repeat(200_000));
        let mut b = Session::new("b", 1, 2);
        b.model = Some(&huge);
        insert_session(&w, &b);
        // Malformed JSON is never read as a bare id.
        let mut c = Session::new("c", 1, 1);
        c.model = Some("{\"id\":\"cut");
        insert_session(&w, &c);
    }
    let out = scan(&mut scanner_for(&db), BIG);
    let a = &out.sessions["a"];
    assert_eq!(a.meta.model.as_ref().map(|m| m.id.as_str()), Some("gpt-5"));
    assert_eq!(
        a.meta.model.as_ref().unwrap().provider.as_deref(),
        Some("openai")
    );
    assert!(
        a.meta
            .model
            .as_ref()
            .unwrap()
            .variant
            .as_ref()
            .unwrap()
            .len()
            <= 1024
    );
    assert!(out.sessions["b"].meta.model.is_none());
    assert!(out.sessions["c"].meta.model.is_none());
}

/// Live-WAL smoke through the real adapter, app state and renderer: parent/nested
/// expansion facts, provider below harness, effort below model, and a confirmed
/// tool link opening the nested child.
#[test]
fn live_wal_family_provider_effort_and_confirmed_link_smoke() {
    use agentscope::app::App;
    use agentscope::domain::Thresholds;
    use agentscope::ui::{RosterContext, render};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::time::Duration;

    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let writer = create_v2(&db); // WAL, autocheckpoint off, writer kept open.
    let model = model_json("gpt-5", "openai", "high");
    {
        let mut parent = Session::new("ses_parent", 100, 900);
        parent.model = Some(&model);
        parent.agent = Some("build");
        parent.tokens = Some([1_000, 400, 0, 0, 0]);
        parent.cost = Some(0.25);
        insert_session(&writer, &parent);
        let mut child = Session::new("ses_nested", 150, 500);
        child.parent_id = Some("ses_parent");
        child.tokens = Some([200, 80, 0, 0, 0]);
        child.cost = Some(0.05);
        insert_session(&writer, &child);
        let mut fork = Session::new("ses_fork", 300, 700);
        fork.fork_id = Some("ses_parent");
        fork.tokens = Some([30, 5, 0, 0, 0]);
        fork.cost = Some(0.002);
        insert_session(&writer, &fork);
        writer
            .execute(
                "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
                 VALUES ('m1', 'ses_parent', 'assistant', 0, 110, 110, ?1)",
                [serde_json::json!({
                    "content": [{
                        "type": "tool",
                        "id": "c1",
                        "name": "subagent",
                        "state": {
                            "status": "completed",
                            "input": {"agent": "explore"},
                            "content": [{"type": "text", "text": "done"}],
                            "metadata": {"sessionID": "ses_nested"}
                        }
                    }],
                    "model": {"id": "gpt-5", "providerID": "openai", "variant": "high"}
                })
                .to_string()],
            )
            .expect("tool message");
    }

    let mut config = config_for_db(&db);
    config.process_enabled = false;
    let mut scanner = OpenCodeScanner::new(&config).unwrap();
    let sessions = scan(&mut scanner, BIG).sessions;
    assert_eq!(sessions.len(), 3);

    // Direct facts remain separate; the fork is a root with its reset counters.
    assert_eq!(
        sessions["ses_parent"].usage.as_ref().unwrap().total,
        Some(1_400)
    );
    assert_eq!(
        sessions["ses_nested"].usage.as_ref().unwrap().total,
        Some(280)
    );
    assert_eq!(sessions["ses_fork"].usage.as_ref().unwrap().total, Some(35));
    assert!(sessions["ses_fork"].meta.lineage.parent.is_none());

    // The v2 tool shape confirms the nested child; the app resolves it.
    let parent = &sessions["ses_parent"];
    let nested = &sessions["ses_nested"];
    let page = support::world::try_transcript(
        &mut scanner,
        &parent.meta.key,
        None,
        agentscope::domain::PageDirection::Forward,
        support::world::PAGE,
        BIG,
        support::world::STEPS,
    )
    .expect("parent transcript");
    let tool = page
        .events
        .iter()
        .find_map(|e| e.tool.as_ref())
        .expect("tool event");
    assert_eq!(
        tool.child_session,
        Some(SessionLink::Unresolved("ses_nested".into()))
    );

    let now = support::NOW;
    let mut app = App::new(
        now,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(sessions.values().cloned().collect(), vec![]);
    assert_eq!(app.total_sessions(), 3);
    assert_eq!(
        app.resolved_tool_child(&parent.meta.key, tool),
        Some(SessionLink::Resolved(nested.meta.key.clone()))
    );
    assert!(app.session(&nested.meta.key).is_some());

    // Open the parent, load its transcript through the app's request, and inspect
    // the details screen that carries the confirmed link.
    while app.selected_key().as_ref() != Some(&parent.meta.key) {
        assert!(app.selected_index().is_some());
        app.move_selection(1);
    }
    assert!(app.open_inspector());
    let request_id = app
        .take_requests()
        .iter()
        .find_map(|r| match r {
            agentscope::worker::WorkerRequest::Select { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("select request");
    assert!(app.apply_transcript(request_id, page));
    let ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    let draw = |app: &App| {
        let mut t = Terminal::new(TestBackend::new(160, 40)).unwrap();
        t.draw(|f| render(f, app, &ctx)).unwrap();
        t.backend()
            .buffer()
            .content()
            .chunks(160)
            .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    // Session info: the header carries the harness tag, model and effort; the
    // cost panel carries the recorded billing route.
    assert!(app.open_session_info());
    let info = draw(&app);
    assert_eq!(
        app.resolved_parent(&nested.meta.key),
        Some(SessionLink::Resolved(parent.meta.key.clone()))
    );
    for needle in [
        "[OC]",
        "Billing provider: openai",
        "model gpt-5",
        "thinking high",
        "Agent: build",
    ] {
        assert!(info.contains(needle), "missing {needle:?}:\n{info}");
    }
    let header_at = info.find("model gpt-5").unwrap();
    let billing_at = info.find("Billing provider: openai").unwrap();
    assert!(header_at < billing_at, "{info}");

    // Details shows the confirmed link; `o` opens the child session rather than
    // leaving the parent transcript on screen.
    app.close_overlay();
    assert!(app.open_details());
    let details = draw(&app);
    assert!(
        details.contains("Child session: resolved — OpenCode ses_nested"),
        "confirmed link is visible:\n{details}"
    );
    agentscope::ui::handle_key(
        &mut app,
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('o'),
            crossterm::event::KeyModifiers::NONE,
        ),
    );
    assert_eq!(app.screen, agentscope::app::Screen::Inspector);
    assert_eq!(app.selected_key().as_ref(), Some(&nested.meta.key));
    assert_eq!(app.transcript().unwrap().key(), &nested.meta.key);
    assert!(
        app.take_requests().iter().any(|r| matches!(
            r,
            agentscope::worker::WorkerRequest::Select { key, .. } if key == &nested.meta.key
        )),
        "the child transcript is requested"
    );

    // A schema-changing live source keeps valid sessions readable: an unrelated
    // future column and a new row appear after the scanner already ran.
    writer
        .execute("ALTER TABLE session_v2 ADD COLUMN future_note TEXT", [])
        .unwrap();
    writer
        .execute(
            "UPDATE session_v2 SET future_note = 'ignored' WHERE id = 'ses_parent'",
            [],
        )
        .unwrap();
    insert_session(&writer, &Session::new("ses_later", 400, 950));
    let refreshed = scan(&mut scanner, BIG).sessions;
    assert!(refreshed.contains_key("ses_later"));
    let parent_after = support::world::hydrate_complete(&mut scanner, &parent.meta.key, now);
    assert_eq!(
        parent_after.usage.as_ref().unwrap().total,
        Some(1_400),
        "an extra column does not disturb an existing row"
    );
}

#[test]
fn user_price_overrides_never_authorize_effort_inference() {
    use agentscope::pricing::{PriceCard, PriceOverride, PriceSchedule, Rates};

    let dir = tmp();
    let db = dir.path().join("opencode.db");
    {
        let w = create_v2(&db);
        let custom = model_json("my-custom-model", "openai", "high");
        let mut c = Session::new("custom", 1, 3);
        c.model = Some(&custom);
        insert_session(&w, &c);
        let bundled = model_json("gpt-5", "openai", "high");
        let mut b = Session::new("bundled", 1, 2);
        b.model = Some(&bundled);
        insert_session(&w, &b);
    }
    // A user-configured price for the unknown model must not become model-format
    // evidence: raw variant stays, effort remains Unknown.
    let schedule = PriceSchedule::bundled()
        .expect("bundled catalog")
        .with_overrides(vec![PriceOverride {
            card: PriceCard::new(
                "openai",
                "my-custom-model",
                Rates {
                    input: Some(1.0),
                    output: Some(2.0),
                    ..Rates::default()
                },
            ),
        }])
        .expect("user price override");
    let mut config = config_for_db(&db);
    config.pricing = schedule;
    let mut scanner = OpenCodeScanner::new(&config).expect("scanner");
    let out = scan(&mut scanner, BIG);

    let custom = out.sessions["custom"].meta.model.as_ref().unwrap();
    assert_eq!(custom.variant.as_deref(), Some("high"), "raw variant kept");
    assert_eq!(
        custom.effort, None,
        "a user price entry is not capability evidence"
    );
    assert_eq!(custom.provider.as_deref(), Some("openai"));

    let bundled = out.sessions["bundled"].meta.model.as_ref().unwrap();
    assert_eq!(bundled.variant.as_deref(), Some("high"));
    assert_eq!(
        bundled.effort.as_deref(),
        Some("high"),
        "a bundled evidenced route still maps"
    );
}
