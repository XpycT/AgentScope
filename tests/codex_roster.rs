mod support;

use std::collections::HashMap;

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    AggregationClass, Harness, Lifecycle, RefreshBudget, SessionKey, SessionLink, SessionSummary,
    SourceKey, Warning, WarningKind,
};
use agentscope::providers::ProviderReader;
use agentscope::providers::codex::CodexScanner;
use support::codex::*;
use support::facts::price_schedule;
use support::{NOW, Unreadable, config_for, dated, link_dir, set_modified, write_lines};

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

struct Scan {
    sessions: HashMap<SessionKey, SessionSummary>,
    removed: Vec<SessionKey>,
    warnings: Vec<Warning>,
    calls: usize,
}

/// Discovery to the end of one pass, then hydration of every source it reported, all
/// under `budget`. `calls` counts every discovery and hydration slice.
fn scan_until_done(scanner: &mut CodexScanner, budget: RefreshBudget) -> Scan {
    let mut scan = Scan {
        sessions: HashMap::new(),
        removed: vec![],
        warnings: vec![],
        calls: 0,
    };
    let mut keys: Vec<SessionKey> = Vec::new();
    loop {
        let delta = scanner.discover(NOW, budget).expect("discover");
        scan.calls += 1;
        for meta in delta.sessions {
            if !keys.contains(&meta.key) {
                keys.push(meta.key);
            }
        }
        scan.removed.extend(delta.removed);
        scan.warnings.extend(delta.warnings);
        if delta.scan.done {
            break;
        }
        assert!(scan.calls < 10_000, "discovery never finished");
    }
    for key in keys {
        let job = support::world::new_job();
        loop {
            let delta = scanner.hydrate(job, &key, NOW, budget).expect("hydrate");
            scan.calls += 1;
            if delta.done {
                scan.sessions.insert(key, delta.summary);
                break;
            }
            assert!(scan.calls < 1_000_000, "hydration never finished");
        }
    }
    scan
}

fn only(scan: &Scan) -> &SessionSummary {
    assert_eq!(scan.sessions.len(), 1, "expected one session");
    scan.sessions.values().next().unwrap()
}

fn warning<'a>(s: &'a SessionSummary, label: &str) -> Option<&'a Warning> {
    s.warnings.iter().find(|w| w.key.source.ends_with(label))
}

fn scanner(root: &std::path::Path) -> CodexScanner {
    CodexScanner::new(&config_for(root)).expect("scanner")
}

#[test]
fn discovers_rollout_files_and_skips_symlinks_and_missing_roots() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let a = dated(&root, "rollout-a.jsonl");
    let b = root.join("2026/10/30/rollout-b.jsonl");
    write_lines(&a, &[session_meta("2026-10-29T10:00:00Z", "a", "/p")]);
    write_lines(&b, &[session_meta("2026-10-30T10:00:00Z", "b", "/p")]);
    write_lines(&dated(&root, "notes.jsonl"), &["{}".into()]);
    write_lines(&dated(&root, "auth.json"), &["{}".into()]);
    write_lines(&dated(&root, "rollout-c.txt"), &["{}".into()]);

    // Symlinks (file and directory) are skipped when the platform lets us create them.
    let outside = dir.path().join("outside");
    write_lines(&outside.join("rollout-out.jsonl"), &["{}".into()]);
    let mut linked = 0;
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        linked += symlink(&a, dated(&root, "rollout-link.jsonl")).is_ok() as usize;
        linked += symlink(&outside, root.join("linkdir")).is_ok() as usize;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        linked += symlink_file(&a, dated(&root, "rollout-link.jsonl")).is_ok() as usize;
        linked += symlink_dir(&outside, root.join("linkdir")).is_ok() as usize;
        // Directory junctions need no privilege; discovery must skip them too.
        let junction = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(root.join("junction"))
            .arg(&outside)
            .output()
            .is_ok_and(|o| o.status.success());
        linked += junction as usize;
    }
    eprintln!("symlinks created: {linked}");

    let found = support::codex_sources(&config_for(&root));
    let mut names: Vec<_> = found
        .sources
        .iter()
        .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["rollout-a.jsonl", "rollout-b.jsonl"]);
    assert!(found.sources.iter().all(|s| s.len > 0));
    assert!(found.warnings.is_empty());

    let missing = support::codex_sources(&config_for(&dir.path().join("nope")));
    assert!(missing.sources.is_empty());
    assert_eq!(missing.warnings.len(), 1);
    assert_eq!(missing.warnings[0].key.kind, WarningKind::Unavailable);
    assert_eq!(missing.warnings[0].key.harness, Harness::Codex);
}

#[test]
fn root_overrides_follow_cli_then_env_then_home() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let cli_root = dir.path().join("exact");
    write_lines(
        &home.join("sessions/2026/10/29/rollout-h.jsonl"),
        &["{}".into()],
    );
    write_lines(&cli_root.join("rollout-x.jsonl"), &["{}".into()]);
    let mut env = Env::new();
    env.insert("CODEX_HOME".into(), home.clone().into());

    let from_env = Config::load_with_env(&CliOverrides::default(), &env).unwrap();
    let found = support::codex_sources(&from_env);
    assert_eq!(found.sources.len(), 1);
    assert!(found.sources[0].path.ends_with("rollout-h.jsonl"));

    let cli = CliOverrides {
        codex_sessions: Some(cli_root),
        ..CliOverrides::default()
    };
    let from_cli = Config::load_with_env(&cli, &env).unwrap();
    let found = support::codex_sources(&from_cli);
    assert_eq!(found.sources.len(), 1);
    assert!(found.sources[0].path.ends_with("rollout-x.jsonl"));
}

#[test]
fn discovery_is_newest_name_first_and_not_capped_by_the_detail_cache() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    for (name, secs) in [
        ("rollout-1.jsonl", 1000),
        ("rollout-2.jsonl", 3000),
        ("rollout-3.jsonl", 2000),
    ] {
        let path = dated(&root, name);
        write_lines(&path, &["{}".into()]);
        set_modified(&path, secs);
    }
    let mut config = config_for(&root);
    config.max_cached_sessions = 2;
    let found = support::codex_sources(&config);
    let names: Vec<_> = found
        .sources
        .iter()
        .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    // Descending file names (newest first for dated rollouts); nothing is dropped.
    assert_eq!(
        names,
        ["rollout-3.jsonl", "rollout-2.jsonl", "rollout-1.jsonl"]
    );
    assert!(found.warnings.is_empty());

    let mut scanner = CodexScanner::new(&config).unwrap();
    let scan = scan_until_done(&mut scanner, BIG);
    assert_eq!(scan.sessions.len(), 3, "every discovered source hydrates");
    assert!(
        scanner.cached_sessions() <= 2,
        "the detail cache stays bounded: {}",
        scanner.cached_sessions()
    );
}

#[test]
fn maps_meta_cwd_source_lineage_and_model() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let path = dated(&root, "rollout-child.jsonl");
    write_lines(
        &path,
        &[
            session_meta_with(
                "2026-10-29T10:00:00Z",
                serde_json::json!({
                    "session_id": "child-1",
                    "cwd": "/work/RipperGame",
                    "cli_version": "0.159.2",
                    "source": {"subagent": {"thread_spawn": {
                        "parent_thread_id": "parent-1", "agent_nickname": "Scout"}}},
                }),
            ),
            turn_context("2026-10-29T10:00:01Z", "gpt-a", "/work/RipperGame"),
            user("2026-10-29T10:00:02Z", "do the thing"),
            turn_context("2026-10-29T10:00:03Z", "gpt-b", "/work/RipperGame"),
            turn_context("2026-10-29T10:00:04Z", "gpt-a", "/work/RipperGame"),
        ],
    );
    let scan = scan_until_done(&mut scanner(&root), BIG);
    let s = only(&scan);
    assert_eq!(s.meta.key.harness, Harness::Codex);
    assert_eq!(s.meta.key.native_id, "child-1");
    assert_eq!(s.meta.key.source, SourceKey::File(path));
    assert_eq!(
        s.meta.cwd.as_deref(),
        Some(std::path::Path::new("/work/RipperGame"))
    );
    assert_eq!(s.meta.project.as_deref(), Some("RipperGame"));
    assert_eq!(s.meta.harness_version.as_deref(), Some("0.159.2"));
    // The latest turn context wins; an earlier gpt-b is not retained.
    assert_eq!(s.meta.model.as_ref().map(|m| m.id.as_str()), Some("gpt-a"));
    assert_eq!(s.meta.title, "Scout");
    assert!(!s.meta.title_derived);
    assert_eq!(
        s.meta.lineage.parent,
        Some(SessionLink::Unresolved("parent-1".into()))
    );
    assert!(!s.meta.lineage.sidechain);
    assert!(s.meta.source_available);
    assert_eq!(s.meta.created_at, Some(1_793_268_000_000));
    assert_eq!(s.meta.activity.at, Some(1_793_268_004_000));
}

#[test]
fn parent_links_resolve_to_the_most_recent_source_or_stay_ambiguous() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let child = |name: &str, id: &str, parent: &str| {
        write_lines(
            &dated(&root, name),
            &[session_meta_with(
                "2026-10-29T10:00:00Z",
                serde_json::json!({"id": id, "cwd": "/p", "parent_thread_id": parent}),
            )],
        );
    };
    let plain = |name: &str, id: &str, ts: &str| {
        write_lines(&dated(&root, name), &[session_meta(ts, id, "/p")]);
    };
    plain("rollout-p1.jsonl", "dup", "2026-10-29T09:00:00Z");
    plain("rollout-p2.jsonl", "dup", "2026-10-29T09:30:00Z");
    plain("rollout-q1.jsonl", "tie", "2026-10-29T09:00:00Z");
    plain("rollout-q2.jsonl", "tie", "2026-10-29T09:00:00Z");
    plain("rollout-solo.jsonl", "solo", "2026-10-29T09:00:00Z");
    child("rollout-c1.jsonl", "c1", "dup");
    child("rollout-c2.jsonl", "c2", "tie");
    child("rollout-c3.jsonl", "c3", "solo");
    child("rollout-c4.jsonl", "c4", "ghost");
    let scan = scan_until_done(&mut scanner(&root), BIG);
    let by_id = |id: &str| {
        scan.sessions
            .values()
            .filter(|s| s.meta.key.native_id == id)
            .collect::<Vec<_>>()
    };
    let link = |id: &str| by_id(id)[0].meta.lineage.parent.clone().unwrap();
    match link("c1") {
        SessionLink::Resolved(k) => {
            assert_eq!(k.source, SourceKey::File(dated(&root, "rollout-p2.jsonl")))
        }
        other => panic!("expected resolved, got {other:?}"),
    }
    assert_eq!(link("c2"), SessionLink::Ambiguous);
    assert!(matches!(link("c3"), SessionLink::Resolved(_)));
    assert_eq!(link("c4"), SessionLink::Unresolved("ghost".into()));
}

#[test]
fn uses_first_user_prompt_as_derived_title_and_never_developer_text() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    write_lines(
        &dated(&root, "rollout-t.jsonl"),
        &[
            session_meta("2026-10-29T10:00:00Z", "t-1", "/p"),
            developer("2026-10-29T10:00:01Z", "DEVELOPER SECRET RULES"),
            user(
                "2026-10-29T10:00:02Z",
                "<environment_context>cwd</environment_context>",
            ),
            user("2026-10-29T10:00:03Z", "# AGENTS.md instructions for /p"),
            user("2026-10-29T10:00:04Z", "  Fix the\n inventory   API  "),
            user("2026-10-29T10:00:05Z", "second prompt"),
        ],
    );
    write_lines(
        &dated(&root, "rollout-d.jsonl"),
        &[
            session_meta("2026-10-29T11:00:00Z", "dev-only", "/p"),
            developer("2026-10-29T11:00:01Z", "DEVELOPER SECRET RULES"),
        ],
    );
    let long = "x".repeat(200);
    write_lines(
        &dated(&root, "rollout-l.jsonl"),
        &[
            session_meta("2026-10-29T12:00:00Z", "long", "/p"),
            user("2026-10-29T12:00:01Z", &long),
        ],
    );
    write_lines(
        &dated(&root, "rollout-nometa.jsonl"),
        &[user("2026-10-29T13:00:00Z", "hello")],
    );
    let scan = scan_until_done(&mut scanner(&root), BIG);
    let get = |id: &str| {
        scan.sessions
            .values()
            .find(|s| s.meta.key.native_id == id)
            .unwrap_or_else(|| panic!("no session {id}"))
    };
    let t = get("t-1");
    assert_eq!(t.meta.title, "Fix the inventory API");
    assert!(t.meta.title_derived);
    let d = get("dev-only");
    assert_eq!(d.meta.title, "dev-only");
    assert!(!d.meta.title.contains("SECRET"));
    assert!(!d.meta.title_derived);
    assert_eq!(get("long").meta.title.chars().count(), 80);
    // Missing session_meta: id and title fall back to the file stem with a warning.
    let n = get("rollout-nometa");
    assert!(warning(n, "#missing-meta").is_some());
}

#[test]
fn usage_uses_latest_thread_snapshot_with_token_count_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    write_lines(
        &dated(&root, "rollout-rec.jsonl"),
        &[
            session_meta("2026-10-29T10:00:00Z", "rec", "/p"),
            turn_context("2026-10-29T10:00:01Z", "gpt-a", "/p"),
            usage_record("2026-10-29T10:00:02Z", usage(100, 40, 20, 5, 120)),
            token_count(
                "2026-10-29T10:00:02Z",
                usage(100, 40, 20, 5, 120),
                Some(272_000),
            ),
            usage_record("2026-10-29T10:00:03Z", usage(300, 100, 50, 10, 350)),
            token_count(
                "2026-10-29T10:00:03Z",
                usage(999, 1, 1, 1, 1000),
                Some(272_000),
            ),
        ],
    );
    write_lines(
        &dated(&root, "rollout-cnt.jsonl"),
        &[
            session_meta("2026-10-29T11:00:00Z", "cnt", "/p"),
            token_count("2026-10-29T11:00:01Z", usage(10, 2, 5, 1, 15), None),
            token_count("2026-10-29T11:00:02Z", usage(30, 6, 9, 2, 39), None),
        ],
    );
    write_lines(
        &dated(&root, "rollout-none.jsonl"),
        &[
            session_meta("2026-10-29T12:00:00Z", "none", "/p"),
            user("2026-10-29T12:00:01Z", "hi"),
        ],
    );
    let scan = scan_until_done(&mut scanner(&root), BIG);
    let get = |id: &str| {
        scan.sessions
            .values()
            .find(|s| s.meta.key.native_id == id)
            .unwrap()
    };
    let rec = get("rec").usage.as_ref().unwrap();
    assert_eq!(rec.total, Some(350));
    // Canonical legs: uncached input and cache reads (300 input includes 100 cached).
    assert_eq!(rec.categories.input, Some(200));
    assert_eq!(rec.categories.cache_read, Some(100));
    assert_eq!(rec.categories.cached_input, None);
    assert_eq!(rec.categories.output, Some(50));
    assert_eq!(rec.categories.reasoning, Some(10));
    assert_eq!(rec.categories.cache_write, Some(0));
    assert_eq!(rec.models.len(), 1);
    assert_eq!(rec.models[0].model.id, "gpt-a");
    assert_eq!(rec.class, AggregationClass::Reported);
    // No price configured: cost stays Unknown, never zero.
    assert_eq!(rec.cost.class, AggregationClass::Unknown);
    assert_eq!(rec.cost.usd, None);

    let cnt = get("cnt").usage.as_ref().unwrap();
    assert_eq!(cnt.total, Some(39));
    assert_eq!(cnt.categories.input, Some(24), "30 input minus 6 cached");

    assert!(get("none").usage.is_none());
}

#[test]
fn latest_cumulative_snapshot_wins_even_when_the_value_decreases() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    write_lines(
        &dated(&root, "rollout-dec.jsonl"),
        &[
            session_meta("2026-10-29T10:00:00Z", "dec", "/p"),
            usage_record("2026-10-29T10:00:01Z", usage(1000, 0, 200, 0, 1200)),
            usage_record("2026-10-29T10:00:02Z", usage(400, 0, 100, 0, 500)),
        ],
    );
    write_lines(
        &dated(&root, "rollout-dec2.jsonl"),
        &[
            session_meta("2026-10-29T11:00:00Z", "dec2", "/p"),
            token_count("2026-10-29T11:00:01Z", usage(1000, 0, 200, 0, 1200), None),
            token_count("2026-10-29T11:00:02Z", usage(400, 0, 100, 0, 500), None),
        ],
    );
    let scan = scan_until_done(&mut scanner(&root), BIG);
    for s in scan.sessions.values() {
        assert_eq!(
            s.usage.as_ref().unwrap().total,
            Some(500),
            "{}",
            s.meta.key.native_id
        );
    }
}

#[test]
fn cost_is_estimated_from_prices_and_unknown_for_nonzero_cache_writes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let mut with_write = usage(1_000_000, 400_000, 100_000, 0, 1_100_000);
    with_write["cache_write_input_tokens"] = 5.into();
    for (name, id, u) in [
        (
            "rollout-a.jsonl",
            "plain",
            usage(1_000_000, 400_000, 100_000, 0, 1_100_000),
        ),
        ("rollout-b.jsonl", "writes", with_write),
    ] {
        write_lines(
            &dated(&root, name),
            &[
                session_meta("2026-10-29T10:00:00Z", id, "/p"),
                turn_context("2026-10-29T10:00:01Z", "gpt-a", "/p"),
                usage_record("2026-10-29T10:00:02Z", u),
            ],
        );
    }
    let mut config = config_for(&root);
    // Native Codex records no billing route, so the reference policy prices it at "openai".
    config.pricing = price_schedule(
        r#"[{"billing_provider":"openai","model":"gpt-a","as_of":"2026-10-01",
            "rates":{"input":1.0,"output":2.0,"cache_read":0.1}}]"#,
    );
    let scan = scan_until_done(&mut CodexScanner::new(&config).unwrap(), BIG);
    let cost = |id: &str| {
        scan.sessions
            .values()
            .find(|s| s.meta.key.native_id == id)
            .unwrap()
            .usage
            .as_ref()
            .unwrap()
            .cost
            .clone()
    };
    let plain = cost("plain");
    assert_eq!(plain.class, AggregationClass::Estimated);
    assert!((plain.usd.unwrap() - 0.84).abs() < 1e-9);
    let writes = cost("writes");
    assert_eq!(writes.class, AggregationClass::Unknown);
    assert_eq!(writes.usd, None);
}

#[test]
fn duplicate_session_ids_remain_separate_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    for (name, total) in [("rollout-1.jsonl", 100), ("rollout-2.jsonl", 250)] {
        write_lines(
            &dated(&root, name),
            &[
                session_meta("2026-10-29T10:00:00Z", "same-id", "/p"),
                usage_record("2026-10-29T10:00:01Z", usage(total, 0, 0, 0, total)),
            ],
        );
    }
    let scan = scan_until_done(&mut scanner(&root), BIG);
    assert_eq!(scan.sessions.len(), 2);
    let mut totals: Vec<_> = scan
        .sessions
        .values()
        .map(|s| s.usage.as_ref().unwrap().total.unwrap())
        .collect();
    totals.sort();
    assert_eq!(totals, [100, 250]);
    for s in scan.sessions.values() {
        assert_eq!(s.meta.key.native_id, "same-id");
        let w = warning(s, "#duplicate-id").expect("duplicate warning");
        assert_eq!(w.count, 2);
    }
}

#[test]
fn malformed_unknown_and_oversized_records_are_counted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let path = dated(&root, "rollout-bad.jsonl");
    let lines = vec![
        session_meta("2026-10-29T10:00:00Z", "bad", "/p"),
        "not json".to_string(),
        "also {bad".to_string(),
        record(
            "2026-10-29T10:00:01Z",
            "future_record",
            serde_json::json!({}),
        ),
        record("2026-10-29T10:00:02Z", "world_state", serde_json::json!({})),
        user("2026-10-29T10:00:03Z", &"y".repeat(5000)),
        user("2026-10-29T10:00:04Z", "after the damage"),
    ];
    write_lines(&path, &lines);
    // A trailing partial line is held, not counted as malformed.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"type\":\"resp")
        .unwrap();
    let mut config = config_for(&root);
    config.max_record_bytes = 2048;
    let scan = scan_until_done(&mut CodexScanner::new(&config).unwrap(), BIG);
    let s = only(&scan);
    assert_eq!(
        warning(s, "#malformed").map(|w| (w.count, w.key.kind)),
        Some((2, WarningKind::Parse))
    );
    assert_eq!(warning(s, "#oversized").map(|w| w.count), Some(1));
    assert_eq!(warning(s, "#unknown").map(|w| w.count), Some(1));
    assert_eq!(s.hidden_records, 3);
    assert_eq!(s.meta.title, "after the damage");
    assert_eq!(s.displayed_event_count, 1);
}

#[test]
fn context_window_and_lifecycle_are_reported_evidence_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    write_lines(
        &dated(&root, "rollout-w.jsonl"),
        &[
            session_meta("2026-10-29T10:00:00Z", "w", "/p"),
            task_started("2026-10-29T10:00:01Z", Some(258_400)),
            assistant("2026-10-29T10:00:02Z", "ok"),
            task_complete("2026-10-29T10:00:03Z"),
            task_started("2026-10-29T10:00:04Z", None),
        ],
    );
    write_lines(
        &dated(&root, "rollout-x.jsonl"),
        &[
            session_meta("2026-10-29T11:00:00Z", "x", "/p"),
            task_started("2026-10-29T11:00:01Z", None),
            task_complete("2026-10-29T11:00:05Z"),
            usage_record("2026-10-29T11:00:06Z", usage(1, 0, 1, 0, 2)),
        ],
    );
    write_lines(
        &dated(&root, "rollout-y.jsonl"),
        &[
            session_meta("2026-10-29T12:00:00Z", "y", "/p"),
            token_count("2026-10-29T12:00:01Z", usage(1, 0, 1, 0, 2), Some(128_000)),
        ],
    );
    let scan = scan_until_done(&mut scanner(&root), BIG);
    let get = |id: &str| {
        scan.sessions
            .values()
            .find(|s| s.meta.key.native_id == id)
            .unwrap()
    };
    assert_eq!(get("w").metrics.context.capacity, Some(258_400));
    assert_eq!(
        get("w").metrics.context.used,
        None,
        "no prompt snapshot yet"
    );
    assert_eq!(
        get("w").metrics.timing.active_ms,
        Some(2_000),
        "the closed turn contributes its own span"
    );
    assert!(
        !get("w").metrics.timing.active_complete,
        "the last turn is open"
    );
    assert_eq!(get("x").metrics.timing.active_ms, Some(4_000));
    assert!(get("x").metrics.timing.active_complete);
    assert_eq!(
        get("y").metrics.context.used,
        Some(1),
        "occupancy is the last prompt's input tokens, not lifetime totals"
    );
    assert_eq!(get("w").lifecycle, Lifecycle::Ongoing);
    assert!(get("w").unfinished_work);
    assert_eq!(get("w").last_completion, Some(1_793_268_003_000));
    assert_eq!(get("x").lifecycle, Lifecycle::Completed);
    assert!(!get("x").unfinished_work);
    assert_eq!(get("x").last_completion, Some(1_793_271_605_000));
    assert_eq!(get("x").metrics.context.capacity, None);
    assert_eq!(get("y").metrics.context.capacity, Some(128_000));
    assert_eq!(get("y").lifecycle, Lifecycle::Unknown);
}

#[test]
fn chunked_budget_resumes_and_appends_are_read_incrementally() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let a = dated(&root, "rollout-a.jsonl");
    let b = dated(&root, "rollout-b.jsonl");
    write_lines(
        &a,
        &[
            session_meta("2026-10-29T10:00:00Z", "a", "/p"),
            usage_record("2026-10-29T10:00:01Z", usage(10, 0, 5, 0, 15)),
            usage_record("2026-10-29T10:00:02Z", usage(20, 0, 10, 0, 30)),
        ],
    );
    write_lines(&b, &[session_meta("2026-10-29T11:00:00Z", "b", "/p")]);
    let tiny = RefreshBudget {
        max_records: 1,
        max_bytes: 1 << 20,
        max_millis: 60_000,
    };
    let mut s = scanner(&root);
    let first = scan_until_done(&mut s, tiny);
    assert!(first.calls > 1, "tiny budget must need several calls");
    assert_eq!(first.sessions.len(), 2);
    let key_a = first
        .sessions
        .keys()
        .find(|k| k.native_id == "a")
        .unwrap()
        .clone();
    assert_eq!(
        first.sessions[&key_a].usage.as_ref().unwrap().total,
        Some(30)
    );

    // Nothing changed: nothing re-emitted.
    let idle = scan_until_done(&mut s, BIG);
    assert!(idle.sessions.is_empty() && idle.removed.is_empty());

    // Append one cumulative snapshot: only that session is re-emitted.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(&a).unwrap();
    writeln!(
        f,
        "{}",
        usage_record("2026-10-29T10:00:03Z", usage(25, 0, 12, 0, 37))
    )
    .unwrap();
    drop(f);
    let grown = scan_until_done(&mut s, BIG);
    assert_eq!(grown.sessions.len(), 1);
    assert_eq!(
        grown.sessions[&key_a].usage.as_ref().unwrap().total,
        Some(37)
    );

    // Removing a file removes its row.
    std::fs::remove_file(&b).unwrap();
    let gone = scan_until_done(&mut s, BIG);
    assert_eq!(gone.removed.len(), 1);
    assert_eq!(gone.removed[0].native_id, "b");
    assert!(!gone.warnings.is_empty());
}

#[test]
fn changed_session_id_retires_the_previous_key() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let path = dated(&root, "rollout-late.jsonl");
    write_lines(&path, &[]);
    std::fs::write(&path, b"").unwrap();
    let mut s = scanner(&root);
    let first = scan_until_done(&mut s, BIG);
    let old = first.sessions.keys().next().unwrap().clone();
    assert_eq!(old.native_id, "rollout-late");

    // The file gains metadata: the stem-keyed row must be retired.
    write_lines(
        &path,
        &[session_meta("2026-10-29T10:00:00Z", "real-id", "/p")],
    );
    let second = scan_until_done(&mut s, BIG);
    assert_eq!(second.removed, [old]);
    assert_eq!(second.sessions.keys().next().unwrap().native_id, "real-id");

    // A replacement under another ID retires the previous one as well.
    std::fs::remove_file(&path).unwrap();
    write_lines(
        &path,
        &[
            session_meta("2026-10-29T10:00:00Z", "other-id", "/p"),
            user("2026-10-29T10:00:01Z", "padding so the length changes"),
        ],
    );
    let third = scan_until_done(&mut s, BIG);
    assert_eq!(third.removed.len(), 1);
    assert_eq!(third.removed[0].native_id, "real-id");
    assert_eq!(third.sessions.len(), 1);
    assert_eq!(third.sessions.keys().next().unwrap().native_id, "other-id");
}

#[test]
fn open_cursors_stay_bounded_across_repeated_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let make = |i: u64| {
        let path = dated(&root, &format!("rollout-{i:03}.jsonl"));
        write_lines(
            &path,
            &[session_meta(
                "2026-10-29T10:00:00Z",
                &format!("id{i}"),
                "/p",
            )],
        );
        set_modified(&path, 1_000 + i);
    };
    for i in 0..40 {
        make(i);
    }
    let mut s = scanner(&root);
    scan_until_done(&mut s, BIG);
    assert_eq!(s.open_cursors(), 32);
    // New, newer files push unchanged ones below rank 32: their handles must go.
    for i in 40..50 {
        make(i);
    }
    let grown = scan_until_done(&mut s, BIG);
    assert_eq!(grown.sessions.len(), 10);
    assert_eq!(s.open_cursors(), 32);
    scan_until_done(&mut s, BIG);
    assert_eq!(s.open_cursors(), 32);
}

#[test]
fn budget_charges_each_records_own_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    for i in 0..5 {
        write_lines(
            &dated(&root, &format!("rollout-{i}.jsonl")),
            &[session_meta(
                "2026-10-29T10:00:00Z",
                &format!("id{i}"),
                "/p",
            )],
        );
    }
    let one_byte = RefreshBudget {
        max_records: 1_000,
        max_bytes: 1,
        max_millis: 60_000,
    };
    let scan = scan_until_done(&mut scanner(&root), one_byte);
    assert_eq!(scan.sessions.len(), 5);
    assert!(
        scan.calls >= 5,
        "each single-record file must consume budget, got {}",
        scan.calls
    );
}

#[test]
fn long_cwd_is_preserved_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let cwd = format!("/work/{}/RipperGame", "d".repeat(400));
    write_lines(
        &dated(&root, "rollout-long.jsonl"),
        &[
            session_meta("2026-10-29T10:00:00Z", "long-cwd", &cwd),
            turn_context("2026-10-29T10:00:01Z", "gpt-a", &cwd),
        ],
    );
    let scan = scan_until_done(&mut scanner(&root), BIG);
    let s = only(&scan);
    assert_eq!(s.meta.cwd.as_deref(), Some(std::path::Path::new(&cwd)));
    assert_eq!(s.meta.project.as_deref(), Some("RipperGame"));
}

#[test]
fn symlinked_descendants_are_skipped_but_a_linked_root_works() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    write_lines(
        &outside.join("rollout-out.jsonl"),
        &[session_meta("2026-10-29T10:00:00Z", "out", "/p")],
    );
    let root = dir.path().join("s");
    write_lines(
        &dated(&root, "rollout-in.jsonl"),
        &[session_meta("2026-10-29T10:00:00Z", "in", "/p")],
    );
    // Descendant link: never followed. Windows uses a junction; file symlinks
    // need privilege there and are covered only where the platform allows them.
    assert!(
        link_dir(&outside, &root.join("2026").join("linked")),
        "could not create a link"
    );
    let found = support::codex_sources(&config_for(&root));
    assert_eq!(found.sources.len(), 1);
    assert!(found.sources[0].path.ends_with("rollout-in.jsonl"));

    // An explicitly configured root that is itself a link still works.
    let linked_root = dir.path().join("rootlink");
    assert!(
        link_dir(&root, &linked_root),
        "could not create a root link"
    );
    let found = support::codex_sources(&config_for(&linked_root));
    assert_eq!(found.sources.len(), 1);
}

#[test]
fn unreadable_source_is_listed_unavailable_without_hiding_others_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let path = dated(&root, "rollout-lock.jsonl");
    let other = dated(&root, "rollout-ok.jsonl");
    write_lines(
        &path,
        &[session_meta("2026-10-29T10:00:00Z", "locked", "/p")],
    );
    write_lines(&other, &[session_meta("2026-10-29T10:00:00Z", "ok", "/p")]);
    let lock = Unreadable::new(&path);
    if !lock.active {
        eprintln!("cannot make a file unreadable here (elevated?); skipping");
        return;
    }
    let mut s = scanner(&root);
    let found = support::world::discover_pass(&mut s, NOW, BIG);
    assert_eq!(found.sessions.len(), 2, "a healthy neighbour is not hidden");
    let locked = found
        .sessions
        .iter()
        .find(|m| m.key.source == SourceKey::File(path.clone()))
        .expect("listed");
    assert!(!locked.source_available);
    assert!(
        found
            .warnings
            .iter()
            .any(|w| w.key.source.ends_with("#unavailable")),
        "{:?}",
        found.warnings
    );
    assert!(found.removed.is_empty());
    let row = support::world::hydrate_complete(&mut s, &locked.key, NOW);
    assert!(!row.meta.source_available);
    assert!(warning(&row, "#unavailable").is_some());

    // Still unreadable on later passes: it is probed again (never silently trusted) and
    // the row neither flaps nor disappears.
    let again = support::world::discover_pass(&mut s, NOW, BIG);
    assert!(again.removed.is_empty());
    assert!(
        again
            .sessions
            .iter()
            .all(|m| m.key.source == SourceKey::File(path.clone()) && !m.source_available)
    );

    // Once readable again it comes back.
    lock.release();
    let back = support::world::rescan(&mut s, NOW, BIG);
    let row = back.sessions.first().expect("recovered row");
    assert_eq!(row.meta.key.native_id, "locked");
    assert!(row.meta.source_available);
}

#[test]
fn token_usage_record_forms_are_all_read_and_latest_wins() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let cases = [
        (
            "payload",
            usage_record("2026-10-29T10:00:02Z", usage(5, 0, 5, 0, 10)),
        ),
        (
            "top",
            usage_record_top_level("2026-10-29T10:00:02Z", usage(5, 0, 5, 0, 10)),
        ),
        (
            "event",
            usage_record_event("2026-10-29T10:00:02Z", usage(5, 0, 5, 0, 10)),
        ),
    ];
    for (id, line) in cases {
        write_lines(
            &dated(&root, &format!("rollout-{id}.jsonl")),
            &[
                session_meta("2026-10-29T10:00:00Z", id, "/p"),
                usage_record("2026-10-29T10:00:01Z", usage(900, 0, 90, 0, 990)),
                line,
            ],
        );
    }
    let scan = scan_until_done(&mut scanner(&root), BIG);
    assert_eq!(scan.sessions.len(), 3);
    for s in scan.sessions.values() {
        assert_eq!(
            s.usage.as_ref().unwrap().total,
            Some(10),
            "{}",
            s.meta.key.native_id
        );
    }
}

#[test]
fn rotation_charges_the_replacements_full_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let path = dated(&root, "rollout-rot.jsonl");
    let small = session_meta("2026-10-29T10:00:00Z", "a", "/p");
    write_lines(&path, std::slice::from_ref(&small));
    let mut s = scanner(&root);
    scan_until_done(&mut s, BIG);

    // Replace with a single record much longer than the old end position.
    let big = session_meta(
        "2026-10-29T10:00:00Z",
        "a",
        &format!("/{}", "z".repeat(400)),
    );
    let old_len = small.len() as u64 + 1;
    let new_len = big.len() as u64 + 1;
    let max_bytes = (new_len - 20) as usize;
    assert!(new_len > max_bytes as u64 && new_len - old_len < max_bytes as u64);
    std::fs::remove_file(&path).unwrap();
    write_lines(&path, &[big]);
    // Offset difference alone (new_len - old_len) fits the budget; the full record does not.
    let budget = RefreshBudget {
        max_records: 1_000,
        max_bytes,
        max_millis: 60_000,
    };
    let found = support::world::discover_pass(&mut s, NOW, BIG);
    assert_eq!(found.sessions.len(), 1, "the replacement is rediscovered");
    let first = s
        .hydrate(
            support::world::new_job(),
            &found.sessions[0].key,
            NOW,
            budget,
        )
        .unwrap();
    assert!(
        !first.done,
        "replacement's full bytes must exhaust the budget"
    );
}

#[test]
fn cursorless_rewrite_reports_invalidated_but_append_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let target = dated(&root, "rollout-000.jsonl");
    let body = |text: &str| {
        [
            session_meta("2026-10-29T10:00:00Z", "id0", "/p"),
            user("2026-10-29T10:00:01Z", "question"),
            assistant("2026-10-29T10:00:02Z", text),
        ]
    };
    write_lines(&target, &body("first answer"));
    set_modified(&target, 1_000);
    for i in 1..40u64 {
        let path = dated(&root, &format!("rollout-{i:03}.jsonl"));
        write_lines(
            &path,
            &[session_meta(
                "2026-10-29T10:00:00Z",
                &format!("id{i}"),
                "/p",
            )],
        );
        set_modified(&path, 1_000 + i);
    }
    let mut s = scanner(&root);
    scan_until_done(&mut s, BIG);
    scan_until_done(&mut s, BIG);
    assert_eq!(s.open_cursors(), 32, "the oldest rollout lost its cursor");

    // Pure append: prefix intact, so nothing already readable changed.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&target)
        .unwrap();
    use std::io::Write;
    writeln!(f, "{}", assistant("2026-10-29T10:00:03Z", "more")).unwrap();
    set_modified(&target, 1_000); // stays the oldest rollout, so no cursor
    let appended = support::world::rescan(&mut s, NOW, BIG);
    assert!(appended.invalidated.is_empty());

    // Rewrite: same event count, different content, no cursor to notice.
    write_lines(&target, &body("third answer!!"));
    set_modified(&target, 1_000);
    let mut invalidated = vec![];
    loop {
        let d = support::world::rescan(&mut s, NOW, BIG);
        invalidated.extend(d.invalidated);
        if d.scan.done {
            break;
        }
    }
    assert!(
        invalidated.iter().any(|k| k.native_id == "id0"),
        "rewrite without a cursor must invalidate: {invalidated:?}"
    );
}

fn append_lines(path: &std::path::Path, lines: &[String]) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for line in lines {
        writeln!(f, "{line}").unwrap();
    }
}

fn status_label(s: &SessionSummary) -> agentscope::domain::StatusLabel {
    let thresholds = agentscope::domain::Thresholds {
        quiet: std::time::Duration::from_secs(300),
        stall: std::time::Duration::from_secs(900),
    };
    agentscope::status::synthesize(s, NOW, &thresholds).label
}

#[test]
fn user_prompt_or_pending_call_after_task_complete_reopens_the_turn() {
    use agentscope::domain::StatusLabel;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let target = dated(&root, "rollout-reopen.jsonl");
    write_lines(
        &target,
        &[
            session_meta("2026-10-29T10:00:00Z", "reopen", "/p"),
            task_started("2026-10-29T10:00:01Z", Some(1000)),
            user("2026-10-29T10:00:02Z", "first"),
            assistant("2026-10-29T10:00:03Z", "done"),
            task_complete("2026-10-29T10:00:04Z"),
        ],
    );
    let mut s = scanner(&root);
    let done = scan_until_done(&mut s, BIG);
    let first = only(&done).clone();
    assert_eq!(first.lifecycle, Lifecycle::Completed);
    assert!(!first.unfinished_work);
    assert_eq!(status_label(&first), StatusLabel::Complete);
    assert!(
        first.metrics.timing.active_complete,
        "a fully recorded, closed turn is complete coverage"
    );
    let completed_at = first.last_completion.expect("completion recorded");

    // A later genuine user prompt reopens the turn but keeps the old completion time.
    append_lines(&target, &[user("2026-10-29T10:05:00Z", "follow up")]);
    let reopened = scan_until_done(&mut s, BIG);
    let reopened = only(&reopened);
    assert!(reopened.unfinished_work);
    assert_eq!(reopened.lifecycle, Lifecycle::Ongoing);
    assert_eq!(reopened.last_completion, Some(completed_at));
    assert_ne!(status_label(reopened), StatusLabel::Complete);
    assert!(
        !reopened.metrics.timing.active_complete,
        "a turn reopened without task_started cannot claim complete active coverage"
    );

    // Injected context is not a genuine prompt.
    append_lines(&target, &[task_complete("2026-10-29T10:05:30Z")]);
    scan_until_done(&mut s, BIG);
    append_lines(
        &target,
        &[user(
            "2026-10-29T10:06:00Z",
            "<environment_context>cwd</environment_context>",
        )],
    );
    let injected = scan_until_done(&mut s, BIG);
    assert!(injected.sessions.values().all(|x| !x.unfinished_work));

    // A new pending call reopens it as well.
    append_lines(
        &target,
        &[function_call("2026-10-29T10:07:00Z", "c1", "shell", "{}")],
    );
    let pending = scan_until_done(&mut s, BIG);
    let pending = only(&pending);
    assert!(pending.unfinished_work);
    assert_eq!(pending.lifecycle, Lifecycle::Ongoing);
    assert_ne!(status_label(pending), StatusLabel::Complete);
}

#[test]
fn output_appended_for_a_displayed_pending_call_invalidates_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let target = dated(&root, "rollout-pending.jsonl");
    write_lines(
        &target,
        &[
            session_meta("2026-10-29T10:00:00Z", "pend", "/p"),
            task_started("2026-10-29T10:00:01Z", None),
            function_call("2026-10-29T10:00:02Z", "c1", "shell", "{}"),
        ],
    );
    let mut s = scanner(&root);
    let first = scan_until_done(&mut s, BIG);
    let before = only(&first).displayed_event_count;
    // Unrelated append: nothing displayed changes.
    append_lines(&target, &[assistant("2026-10-29T10:00:03Z", "working")]);
    let quiet = support::world::rescan(&mut s, NOW, BIG);
    assert!(quiet.invalidated.is_empty(), "{:?}", quiet.invalidated);

    append_lines(
        &target,
        &[function_output("2026-10-29T10:00:04Z", "c1", "ok")],
    );
    let delta = support::world::rescan(&mut s, NOW, BIG);
    assert!(
        delta.invalidated.iter().any(|k| k.native_id == "pend"),
        "retiring a displayed pending row must invalidate: {:?}",
        delta.invalidated
    );
    assert!(delta.sessions[0].displayed_event_count < before + 1);
}

#[test]
fn oversized_line_is_consumed_under_the_refresh_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let target = dated(&root, "rollout-huge.jsonl");
    write_lines(
        &target,
        &[session_meta("2026-10-29T10:00:00Z", "huge", "/p")],
    );
    append_lines(&target, &["z".repeat(4 * 1024 * 1024)]);
    append_lines(&target, &[user("2026-10-29T10:00:05Z", "after")]);
    let small = RefreshBudget {
        max_records: 100_000,
        max_bytes: 256 * 1024,
        max_millis: 60_000,
    };
    let mut s = scanner(&root);
    let found = support::world::discover_pass(&mut s, NOW, small);
    assert_eq!(found.sessions.len(), 1);
    let (sum, steps) =
        support::world::hydrate_steps(&mut s, &found.sessions[0].key, NOW, small, 10_000);
    assert!(steps >= 8, "4 MiB at 256 KiB per step took {steps} steps");
    assert!(warning(&sum, "#oversized").is_some());
    assert_eq!(
        sum.displayed_event_count, 1,
        "the record after the huge line is read"
    );
}

#[test]
fn a_model_switch_leaves_the_session_aggregate_unattributed_and_unpriced() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    write_lines(
        &dated(&root, "rollout-switch.jsonl"),
        &[
            session_meta("2026-10-29T10:00:00Z", "switch", "/p"),
            turn_context("2026-10-29T10:00:01Z", "gpt-a", "/p"),
            turn_context("2026-10-29T10:00:02Z", "gpt-b", "/p"),
            usage_record(
                "2026-10-29T10:00:03Z",
                usage(1_000_000, 0, 1_000_000, 0, 2_000_000),
            ),
        ],
    );
    write_lines(
        &dated(&root, "rollout-single.jsonl"),
        &[
            session_meta("2026-10-29T11:00:00Z", "single", "/p"),
            turn_context("2026-10-29T11:00:01Z", "gpt-a", "/p"),
            usage_record(
                "2026-10-29T11:00:03Z",
                usage(1_000_000, 0, 1_000_000, 0, 2_000_000),
            ),
        ],
    );
    let mut config = config_for(&root);
    config.pricing = price_schedule(
        r#"[{"billing_provider":"openai","model":"gpt-a","rates":{"input":1.0,"output":2.0,"cache_read":0.1}},
            {"billing_provider":"openai","model":"gpt-b","rates":{"input":10.0,"output":20.0,"cache_read":1.0}}]"#,
    );
    let scan = scan_until_done(&mut CodexScanner::new(&config).unwrap(), BIG);
    let usage_of = |id: &str| {
        scan.sessions
            .values()
            .find(|s| s.meta.key.native_id == id)
            .unwrap()
            .usage
            .clone()
            .unwrap()
    };
    let mixed = usage_of("switch");
    assert_eq!(mixed.total, Some(2_000_000), "provider total is preserved");
    assert!(mixed.models.is_empty(), "no per-model attribution");
    assert_eq!(mixed.cost.usd, None, "not priced at the latest model");
    assert!(!mixed.coverage.complete);
    assert!(
        mixed
            .coverage
            .reason
            .as_deref()
            .unwrap()
            .contains("several models")
    );
    let single = usage_of("single");
    assert_eq!(single.models.len(), 1);
    assert!(single.cost.usd.is_some());
}

#[test]
fn the_newest_compaction_markers_are_retained_when_capped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("s");
    let stamps: Vec<String> = (0..300)
        .map(|i| format!("2026-10-29T10:{:02}:{:02}Z", i / 60, i % 60))
        .collect();
    let mut lines = vec![session_meta("2026-10-29T10:00:00Z", "cap", "/p")];
    for ts in &stamps {
        lines.push(compacted(ts));
    }
    write_lines(&dated(&root, "rollout-cap.jsonl"), &lines);
    let scan = scan_until_done(&mut scanner(&root), BIG);
    let s = only(&scan);
    assert_eq!(s.metrics.compactions.observed, 256);
    assert!(!s.metrics.compactions.history_complete);
    let latest = s
        .metrics
        .compactions
        .latest
        .as_ref()
        .expect("latest marker");
    assert_eq!(
        latest.timestamp,
        Some(1_793_268_000_000 + 299_000),
        "the retained window slides to the newest marker"
    );
}
