//! The OMP reader end to end over synthetic sources: the latest event, exact older
//! identities, details budgets, request ownership, torn/appended/replaced sources,
//! cancellation and isolation of a damaged sibling, driven through the real
//! `OmpScanner` and its bounded `ProviderReader` methods.

mod support;

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    EventKind, LocatorKind, PageAnchor, PageDirection, PageRequest, RefreshBudget, RequestPurpose,
    SessionKey,
};
use agentscope::providers::omp::OmpScanner;
use agentscope::providers::{ProviderReader, ReadProgress};
use serde_json::{Value, json};
use support::facts::price_schedule;
use support::omp as fx;
use support::world::{
    BIG, PAGE, STEPS, details_ready, discover_pass, hydrate_complete, new_job, transcript_ready,
    transcript_steps, try_count, try_source, try_transcript,
};

const NOW: i64 = support::NOW;

const TINY: RefreshBudget = RefreshBudget {
    max_records: 7,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn config(root: &Path) -> Config {
    let cli = CliOverrides {
        omp_sessions: Some(root.to_path_buf()),
        ..CliOverrides::default()
    };
    let mut config = Config::load_with_env(&cli, &Env::new()).expect("valid test config");
    config.pricing = price_schedule(
        r#"[{"billing_provider":"acme","model":"m",
             "rates":{"input":0.0,"output":10.0,"cache_read":0.0,"cache_write":0.0}}]"#,
    );
    config
}

fn root(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("sessions")
}

fn out(n: u64) -> Value {
    fx::usage(0, n, 0, 0, 99.0)
}

fn scanner(root: &Path) -> OmpScanner {
    OmpScanner::new(&config(root)).expect("scanner")
}

/// Discovers every session and returns its key by native ID.
fn keys(scanner: &mut OmpScanner) -> HashMap<String, SessionKey> {
    let found = discover_pass(scanner, NOW, BIG);
    let mut by_id = HashMap::new();
    for meta in found.sessions {
        by_id.insert(meta.key.native_id.clone(), meta.key);
    }
    by_id
}

/// The entry ID an event id was built from.
fn entry_of(id: &str) -> &str {
    id.rsplit_once('@').map_or(id, |(entry, _)| entry)
}

/// Every event of a session, oldest first, read through backward pages of `size`.
fn all_events(
    scanner: &mut OmpScanner,
    key: &SessionKey,
    size: usize,
    budget: RefreshBudget,
) -> Vec<agentscope::domain::Event> {
    let mut pages = Vec::new();
    let mut anchor: Option<PageAnchor> = None;
    loop {
        let page = transcript_ready(
            scanner,
            key,
            anchor.clone(),
            PageDirection::Backward,
            PageRequest {
                max_events: size,
                max_bytes: 10_000_000,
            },
            budget,
            STEPS,
        );
        assert!(page.events.len() <= size);
        let more = page.has_more;
        anchor = page.next.clone();
        pages.push(page.events);
        if !more {
            break;
        }
    }
    pages.into_iter().rev().flatten().collect()
}

fn ids(events: &[agentscope::domain::Event]) -> Vec<String> {
    events.iter().map(|e| entry_of(&e.id).to_owned()).collect()
}

#[test]
fn a_large_branching_family_opens_at_the_latest_event_and_pages_back_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    // A long conversation with side branches: the last record in the file is on a
    // branch that is not the active one; the file order is still the history.
    let mut entries = vec![fx::header("p", None, None)];
    let mut expected = vec!["session".to_owned()];
    entries.push(fx::user_message("u0", None, "start"));
    expected.push("u0".into());
    let mut last = "u0".to_owned();
    for n in 0..150 {
        let id = format!("a{n}");
        entries.push(fx::call(&id, Some(&last), Some(&format!("r{n}")), out(10)));
        expected.push(id.clone());
        if n % 10 == 0 {
            // A sibling branch off the same parent.
            let side = format!("s{n}");
            entries.push(fx::user_message(&side, Some(&last), "side"));
            expected.push(side);
        }
        last = id;
    }
    // The final record is an off-branch late write.
    entries.push(fx::with_leaf(
        fx::aux("late", Some("a0"), "title", out(1)),
        Some(&last),
    ));
    expected.push("late".into());
    fx::write_session(&root, "b", "t1_p", &entries);
    // Nested child and grandchild, each with its own history.
    fx::write_session(
        &root,
        "b",
        "t1_p/c",
        &[
            fx::header("c", None, None),
            fx::user_message("cu", None, "child"),
            fx::call("ca", Some("cu"), Some("rc"), out(20)),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t1_p/c/g",
        &[
            fx::header("g", None, None),
            fx::user_message("gu", None, "grandchild"),
            fx::call("ga", Some("gu"), Some("rg"), out(30)),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let key = &keys["p"];

    // The newest page ends at the true latest recorded event, after several
    // bounded indexing slices (Pending is progress, not an error).
    let (latest, pending) = transcript_steps(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PageRequest {
            max_events: 5,
            max_bytes: 10_000_000,
        },
        TINY,
        STEPS,
    );
    assert!(pending > 1, "indexing must yield between slices: {pending}");
    assert_eq!(latest.events.len(), 5);
    assert_eq!(entry_of(&latest.events.last().unwrap().id), "late");
    assert!(latest.has_more);

    // Backward pages cover every record exactly once, in recorded order.
    let events = all_events(&mut scanner, key, 13, TINY);
    assert_eq!(ids(&events), expected);
    let unique: std::collections::HashSet<_> = events.iter().map(|e| e.id.clone()).collect();
    assert_eq!(unique.len(), events.len(), "event ids are exact identities");

    // Forward from an old anchor is the same suffix, and the count agrees.
    let from = events[100].clone();
    let anchor = PageAnchor {
        timestamp: from.timestamp,
        source_order: from.source_order,
        stable_id: from.id.clone(),
    };
    let forward = transcript_ready(
        &mut scanner,
        key,
        Some(anchor.clone()),
        PageDirection::Forward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(ids(&forward.events), expected[101..].to_vec());
    assert_eq!(
        try_count(&mut scanner, key, &anchor, BIG, STEPS).unwrap(),
        expected.len() - 101
    );

    // Children open as sessions of their own at their own latest event.
    for (id, last) in [("c", "ca"), ("g", "ga")] {
        let page = transcript_ready(
            &mut scanner,
            &keys[id],
            None,
            PageDirection::Backward,
            PAGE,
            BIG,
            STEPS,
        );
        assert_eq!(entry_of(&page.events.last().unwrap().id), last);
    }
    // The summary's event count is the transcript's.
    let summary = hydrate_complete(&mut scanner, key, NOW);
    assert_eq!(summary.displayed_event_count, expected.len());
    assert_eq!(scanner.job_state(), 0, "finished jobs hold nothing");
}

#[test]
fn details_expose_requests_purposes_and_summaries_under_one_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_d",
        &[
            fx::header("d", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10)),
            fx::aux("x1", Some("a1"), "title", out(5)),
            fx::compaction("k1", Some("a1"), "u1"),
            fx::call("a2", Some("a1"), Some("r2"), out(20)),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let key = &keys["d"];
    let page = details_ready(&mut scanner, key, None, PAGE, BIG, STEPS);

    let purposes: Vec<RequestPurpose> = page
        .requests
        .iter()
        .map(|r| r.request.as_ref().unwrap().purpose.clone())
        .collect();
    assert_eq!(
        purposes,
        vec![
            RequestPurpose::Conversation,
            RequestPurpose::Auxiliary("title".into()),
            RequestPurpose::Conversation
        ],
        "auxiliary work keeps its purpose and the historical calls are not dropped"
    );
    assert_eq!(page.compactions.len(), 1);
    let compaction = &page.compactions[0];
    assert_eq!(compaction.before_tokens, Some(120_000));
    assert_eq!(compaction.after_tokens, Some(30_000));
    assert_eq!(
        compaction.summary.as_ref().unwrap().preview,
        "synthetic summary"
    );
    assert!(compaction.usage.is_none(), "compaction cost stays Unknown");
    assert!(
        page.recent.iter().any(|e| e.kind == EventKind::Compaction),
        "the marker is also a transcript event"
    );

    // Requests, compactions and events share one record allowance.
    let tight = details_ready(
        &mut scanner,
        key,
        None,
        PageRequest {
            max_events: 4,
            max_bytes: 10_000_000,
        },
        BIG,
        STEPS,
    );
    assert!(
        tight.requests.len() + tight.compactions.len() + tight.recent.len() <= 4,
        "{} + {} + {}",
        tight.requests.len(),
        tight.compactions.len(),
        tight.recent.len()
    );
    assert!(tight.truncated || tight.has_more);
    // ... and one byte allowance.
    let bytes = details_ready(
        &mut scanner,
        key,
        None,
        PageRequest {
            max_events: 100,
            max_bytes: 40,
        },
        BIG,
        STEPS,
    );
    let shown: usize = bytes
        .recent
        .iter()
        .map(|e| e.text.as_ref().map_or(0, |t| t.preview.len()))
        .sum();
    assert!(shown <= 40 || bytes.recent.len() == 1, "{shown}");
    // An older detail page continues from the previous `next` with the same budget.
    let older = details_ready(
        &mut scanner,
        key,
        tight.next.clone(),
        PageRequest {
            max_events: 4,
            max_bytes: 10_000_000,
        },
        BIG,
        STEPS,
    );
    assert!(older.compactions.is_empty());
    let first: Vec<_> = tight.recent.iter().map(|e| e.id.clone()).collect();
    assert!(older.recent.iter().all(|e| !first.contains(&e.id)));
}

#[test]
fn copied_history_is_never_a_second_counted_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
            fx::call("a2", Some("a1"), Some("r2"), out(20_000)),
        ],
    );
    // A fork repeats the parent's entries with unchanged IDs, then adds its own.
    fx::write_session(
        &root,
        "b",
        "t2_f",
        &[
            fx::header("f", Some("p"), None),
            fx::user_message("u1", None, "go"),
            fx::call(
                "a1",
                Some("u1"),
                Some("r1"),
                fx::usage(0, 10_000, 0, 0, 0.0),
            ),
            fx::call(
                "a2",
                Some("a1"),
                Some("r2"),
                fx::usage(0, 20_000, 0, 0, 0.0),
            ),
            fx::call("a3", Some("a2"), Some("r3"), out(5_000)),
        ],
    );
    // A continuation segment repeats its predecessor too.
    fx::write_session(
        &root,
        "b",
        "t3_q",
        &[
            fx::header("q", None, None),
            fx::user_message("qu", None, "go"),
            fx::call("q1", Some("qu"), Some("rq1"), out(1_000)),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t4_q2",
        &[
            fx::header("q2", Some("q"), Some("q")),
            fx::user_message("qu", None, "go"),
            fx::call("q1", Some("qu"), Some("rq1"), out(1_000)),
            fx::call("q2", Some("q1"), Some("rq2"), out(2_000)),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);

    let counted = |scanner: &mut OmpScanner, id: &str| -> Vec<String> {
        all_events(scanner, &keys[id], 50, BIG)
            .into_iter()
            .filter(|e| e.usage.is_some())
            .map(|e| entry_of(&e.id).to_owned())
            .collect()
    };
    assert_eq!(counted(&mut scanner, "p"), ["a1", "a2"]);
    assert_eq!(
        counted(&mut scanner, "f"),
        ["a3"],
        "a1 and a2 are the parent's"
    );
    // The logical continuation lists both members once each.
    assert_eq!(counted(&mut scanner, "q"), ["q1", "q2"]);
    let q = all_events(&mut scanner, &keys["q"], 50, BIG);
    assert_eq!(
        q.iter().filter(|e| e.kind == EventKind::UserPrompt).count(),
        2
    );

    // The requests a details page lists agree with the ledger.
    for (id, count) in [("p", 2usize), ("f", 1), ("q", 2)] {
        let summary = hydrate_complete(&mut scanner, &keys[id], NOW);
        let details = details_ready(&mut scanner, &keys[id], None, PAGE, BIG, STEPS);
        assert_eq!(details.requests.len(), count, "{id}");
        assert_eq!(
            summary.usage.unwrap().coverage.observed_requests,
            count,
            "{id}"
        );
    }
}

fn append_raw(path: &Path, bytes: &[u8]) {
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn torn_lines_appends_truncation_and_replacement_keep_generations_exact() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let path = fx::write_session(
        &root,
        "b",
        "t1_w",
        &[
            fx::header("w", None, None),
            fx::user_message("u1", None, "first"),
            fx::call("a1", Some("u1"), Some("r1"), out(10)),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let key = &keys["w"];
    let latest = |scanner: &mut OmpScanner| {
        transcript_ready(
            scanner,
            key,
            None,
            PageDirection::Backward,
            PAGE,
            BIG,
            STEPS,
        )
    };
    let page = latest(&mut scanner);
    let last = page.events.last().unwrap().clone();
    assert_eq!(entry_of(&last.id), "a1");
    let anchor = PageAnchor {
        timestamp: last.timestamp,
        source_order: last.source_order,
        stable_id: last.id.clone(),
    };

    // A torn trailing line is not an event; completing it appends exactly one.
    let next = fx::user_message("u2", Some("a1"), "second").to_string();
    let (head, tail) = next.split_at(next.len() / 2);
    append_raw(&path, head.as_bytes());
    let torn = latest(&mut scanner);
    assert_eq!(entry_of(&torn.events.last().unwrap().id), "a1");
    assert_eq!(
        try_count(&mut scanner, key, &anchor, BIG, STEPS).unwrap(),
        0
    );
    append_raw(&path, tail.as_bytes());
    append_raw(&path, b"\n");
    let grown = latest(&mut scanner);
    assert_eq!(entry_of(&grown.events.last().unwrap().id), "u2");
    assert_eq!(
        try_count(&mut scanner, key, &anchor, BIG, STEPS).unwrap(),
        1
    );
    // The earlier anchor is still exact after the append.
    let forward = transcript_ready(
        &mut scanner,
        key,
        Some(anchor.clone()),
        PageDirection::Forward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(ids(&forward.events), ["u2"]);
    // The hydration folds the append once, never twice.
    let summary = hydrate_complete(&mut scanner, key, NOW);
    assert_eq!(summary.usage.unwrap().coverage.observed_requests, 1);

    // A locator from this generation reads its record ...
    let locator = grown.events.last().unwrap().provenance.locator.clone();
    let record = try_source(&mut scanner, &locator, BIG, STEPS).expect("source record");
    assert!(record.raw.contains("second"));

    // ... until the member is truncated and replaced by different content.
    fx::write_session(
        &root,
        "b",
        "t1_w",
        &[
            fx::header("w", None, None),
            fx::user_message("u9", None, "replaced"),
        ],
    );
    support::set_modified(&path, 1_900_000_000);
    let replaced = latest(&mut scanner);
    assert_eq!(ids(&replaced.events), ["session", "u9"]);
    assert!(
        try_source(&mut scanner, &locator, BIG, STEPS).is_err(),
        "a stale locator never reads the replacement"
    );
    let fresh = replaced.events.last().unwrap().provenance.locator.clone();
    assert!(
        try_source(&mut scanner, &fresh, BIG, STEPS)
            .unwrap()
            .raw
            .contains("replaced")
    );
    let summary = hydrate_complete(&mut scanner, key, NOW);
    assert_eq!(
        summary.usage.unwrap().coverage.observed_requests,
        0,
        "the replaced history no longer counts the old request"
    );
}

#[test]
fn a_damaged_sibling_does_not_hide_a_healthy_session() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_ok",
        &[
            fx::header("ok", None, None),
            fx::user_message("u1", None, "fine"),
            fx::call("a1", Some("u1"), Some("r1"), out(10)),
        ],
    );
    fx::write_raw(
        &root,
        "b",
        "t2_bad",
        b"\x00\x01 not json\n{\"type\":\n\xff\xfe\n",
    );
    fx::write_session(
        &root,
        "b",
        "t3_mixed",
        &[
            fx::header("mixed", None, None),
            fx::user_message("m1", None, "before"),
            json!({"type": "future_record_type", "id": "z1"}),
            fx::call("m2", Some("m1"), Some("rm"), out(10)),
        ],
    );
    append_raw(&root.join("b").join("t3_mixed.jsonl"), b"{ broken json\n");
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    assert!(keys.contains_key("ok"));
    let events = all_events(&mut scanner, &keys["ok"], 10, BIG);
    assert_eq!(ids(&events), ["session", "u1", "a1"]);

    let mixed = transcript_ready(
        &mut scanner,
        &keys["mixed"],
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(ids(&mixed.events), ["session", "m1", "m2"]);
    let labels: Vec<&str> = mixed.diagnostics.iter().map(|d| d.label.as_str()).collect();
    assert!(labels.contains(&"unsupported record type"), "{labels:?}");
    assert!(labels.contains(&"unreadable record"), "{labels:?}");
    assert!(mixed.diagnostics.iter().all(|d| d.hidden));

    // The garbage file is itself a session with no events; nothing panics or hangs.
    let bad = &keys["bad"];
    let page = transcript_ready(
        &mut scanner,
        bad,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert!(page.events.is_empty());
    assert!(!page.diagnostics.is_empty());
    let summary = hydrate_complete(&mut scanner, &keys["ok"], NOW);
    assert_eq!(summary.usage.unwrap().coverage.observed_requests, 1);
}

#[test]
fn cancelling_one_job_keeps_the_other_purposes_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let mut entries = vec![
        fx::header("w", None, None),
        fx::user_message("u0", None, "go"),
    ];
    let mut last = "u0".to_owned();
    for n in 0..60 {
        let id = format!("a{n}");
        entries.push(fx::call(&id, Some(&last), Some(&format!("r{n}")), out(10)));
        last = id;
    }
    fx::write_session(&root, "b", "t1_w", &entries);
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let key = &keys["w"];

    // Two purposes on the same session: a hydration and a transcript open.
    let hydrate_job = new_job();
    let page_job = new_job();
    let tiny = RefreshBudget {
        max_records: 4,
        ..BIG
    };
    assert!(!scanner.hydrate(hydrate_job, key, NOW, tiny).unwrap().done);
    let opened = scanner
        .transcript_page(page_job, key, None, PageDirection::Backward, PAGE, tiny)
        .unwrap();
    assert!(matches!(opened, ReadProgress::Pending(_)));
    assert_eq!(scanner.job_state(), 2);
    scanner.cancel_read(key, hydrate_job);
    assert_eq!(scanner.job_state(), 1);
    assert!(
        scanner.active_work() >= 1,
        "the page job still needs the work"
    );
    // The surviving job finishes with a complete, single-counted result.
    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PageRequest {
            max_events: 3,
            max_bytes: 1_000_000,
        },
        tiny,
        STEPS,
    );
    assert_eq!(entry_of(&page.events.last().unwrap().id), "a59");
    scanner.cancel_read(key, page_job);
    assert_eq!(scanner.job_state(), 0);
    assert_eq!(scanner.active_work(), 0, "nothing abandoned stays resident");

    // A cancelled, restarted hydration counts every request exactly once.
    let job = new_job();
    scanner.hydrate(job, key, NOW, tiny).unwrap();
    scanner.cancel_read(key, job);
    let summary = hydrate_complete(&mut scanner, key, NOW);
    assert_eq!(summary.usage.unwrap().coverage.observed_requests, 60);
}

#[test]
fn evicted_summaries_rehydrate_and_unknown_sessions_are_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    for name in ["a", "b", "c"] {
        fx::write_session(
            &root,
            "b",
            &format!("t1_{name}"),
            &[
                fx::header(name, None, None),
                fx::user_message("u1", None, "go"),
                fx::call("a1", Some("u1"), Some("r1"), out(10)),
            ],
        );
    }
    let mut config = config(&root);
    config.max_cached_sessions = 1;
    let mut scanner = OmpScanner::new(&config).unwrap();
    let keys = keys(&mut scanner);
    for id in ["a", "b", "c", "a"] {
        let summary = hydrate_complete(&mut scanner, &keys[id], NOW);
        assert_eq!(summary.usage.unwrap().coverage.observed_requests, 1);
        assert!(scanner.cached_sessions() <= 1);
    }
    let mut stranger = keys["a"].clone();
    stranger.native_id = "elsewhere".into();
    stranger.source = agentscope::domain::SourceKey::File(root.join("nowhere.jsonl"));
    assert!(scanner.hydrate(new_job(), &stranger, NOW, BIG).is_err());
    let page = try_transcript(
        &mut scanner,
        &stranger,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert!(page.is_err());
    // A locator naming a file outside the session is refused, not opened.
    let outside = agentscope::domain::SourceLocator {
        session: keys["a"].clone(),
        kind: LocatorKind::FileOffset {
            path: root.join("b").join("t1_b.jsonl"),
            offset: 0,
        },
        generation: Some(1),
    };
    assert!(try_source(&mut scanner, &outside, BIG, STEPS).is_err());
}

#[test]
fn hostile_text_and_private_records_never_reach_events() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let secret = "SYSTEM-PROMPT-SECRET-5c1d";
    fx::write_session(
        &root,
        "b",
        "t1_h",
        &[
            fx::header("h", None, None),
            fx::entry(
                "i1",
                None,
                "session_init",
                json!({"systemPrompt": secret, "task": secret, "tools": [secret]}),
            ),
            fx::user_message("u1", Some("i1"), "hello \u{1b}[31mred\u{1b}[0m \u{7}bell"),
            fx::entry(
                "t9",
                Some("u1"),
                "message",
                json!({"message": {"role": "toolResult", "toolName": "bash\u{1b}]0;x\u{7}",
                       "toolCallId": "c1", "isError": false,
                       "content": [{"type": "text", "text": "ok"}],
                       "details": {"private": secret}}}),
            ),
            fx::entry("l1", Some("t9"), "label", json!({"label": secret})),
            fx::entry(
                "e1",
                Some("l1"),
                "custom",
                json!({"customType": "x", "data": secret}),
            ),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let page = transcript_ready(
        &mut scanner,
        &keys["h"],
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let rendered = format!("{:?}", page.events);
    assert!(!rendered.contains(secret));
    for event in &page.events {
        let mut texts: Vec<&str> = Vec::new();
        if let Some(text) = &event.text {
            texts.push(&text.preview);
        }
        if let Some(tool) = &event.tool {
            texts.push(&tool.name);
            texts.extend(tool.output.iter().map(|o| o.preview.as_str()));
        }
        for text in texts {
            assert!(!text.contains(secret), "{text}");
            assert!(
                !text
                    .chars()
                    .any(|c| c.is_control() && c != '\n' && c != '\t'),
                "{text:?}"
            );
        }
    }
    assert!(
        page.events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview.contains("hello")))
    );
    assert!(page.diagnostics.iter().all(|d| d.hidden));
    let summary = hydrate_complete(&mut scanner, &keys["h"], NOW);
    let dump = format!("{summary:?}");
    assert!(!dump.contains(secret));
}

#[test]
fn oversized_and_dense_records_stay_inside_the_page_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let big = "x".repeat(500_000);
    let mut entries = vec![fx::header("o", None, None)];
    let mut last: Option<String> = None;
    for n in 0..10 {
        let id = format!("u{n}");
        entries.push(fx::user_message(&id, last.as_deref(), &big));
        last = Some(id);
    }
    fx::write_session(&root, "b", "t1_o", &entries);
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let page = transcript_ready(
        &mut scanner,
        &keys["o"],
        None,
        PageDirection::Backward,
        PageRequest {
            max_events: 500,
            max_bytes: 10_000,
        },
        BIG,
        STEPS,
    );
    let bytes: usize = page
        .events
        .iter()
        .map(|e| e.text.as_ref().map_or(0, |t| t.preview.len()))
        .sum();
    assert!(bytes <= 10_000 + 4096, "{bytes}");
    assert!(page.truncated);
    assert!(page.has_more);
    assert_eq!(page.events.last().map(|e| entry_of(&e.id)), Some("u9"));
}

#[test]
fn the_same_native_id_at_distinct_sources_never_merges_without_a_continuation_edge() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    // Two unrelated files (and a repeated family copy) that happen to share IDs.
    for (bucket, name, text) in [
        ("x", "t1_dup", "first copy"),
        ("y", "t2_dup", "second copy"),
    ] {
        fx::write_session(
            &root,
            bucket,
            name,
            &[
                fx::header("dup", None, None),
                fx::user_message("u1", None, text),
                fx::call("a1", Some("u1"), Some("r1"), out(10)),
            ],
        );
    }
    // A family copied wholesale into another bucket keeps its own children.
    for bucket in ["x", "y"] {
        fx::write_session(
            &root,
            bucket,
            "t3_fam",
            &[
                fx::header("fam", None, None),
                fx::user_message("fu", None, &format!("family in {bucket}")),
            ],
        );
        fx::write_session(
            &root,
            bucket,
            "t3_fam/kid",
            &[
                fx::header("kid", None, None),
                fx::user_message("ku", None, &format!("child in {bucket}")),
                fx::call("ka", Some("ku"), Some(&format!("rk-{bucket}")), out(7)),
            ],
        );
    }
    let mut scanner = scanner(&root);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let mut metas: Vec<_> = found.sessions.into_iter().collect();
    metas.dedup_by(|a, b| a.key == b.key);
    let dups: Vec<_> = metas.iter().filter(|m| m.key.native_id == "dup").collect();
    assert_eq!(dups.len(), 2, "same native ID, two physical sources");
    assert_ne!(dups[0].key, dups[1].key);
    assert!(dups.iter().all(|m| m.members.is_empty()));
    let mut seen = Vec::new();
    for meta in &dups {
        let events = all_events(&mut scanner, &meta.key, 20, BIG);
        assert_eq!(events.len(), 3);
        seen.push(
            events
                .iter()
                .filter_map(|e| e.text.as_ref())
                .map(|t| t.preview.clone())
                .find(|t| t.contains("copy"))
                .unwrap(),
        );
        let summary = hydrate_complete(&mut scanner, &meta.key, NOW);
        assert_eq!(summary.usage.unwrap().coverage.observed_requests, 1);
    }
    seen.sort();
    assert_eq!(seen, ["first copy", "second copy"]);
    // Each copied family's child reads only its own file.
    let kids: Vec<_> = metas.iter().filter(|m| m.key.native_id == "kid").collect();
    assert_eq!(kids.len(), 2);
    for kid in kids {
        let events = all_events(&mut scanner, &kid.key, 20, BIG);
        let bucket = match &kid.key.source {
            agentscope::domain::SourceKey::File(p) => p
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.file_name())
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            other => panic!("{other:?}"),
        };
        assert!(
            events
                .iter()
                .filter_map(|e| e.text.as_ref())
                .any(|t| t.preview == format!("child in {bucket}")),
            "the child of the {bucket} family reads its own transcript"
        );
    }
}

#[test]
fn an_append_to_an_earlier_continuation_member_joins_the_logical_transcript_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let first = fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10)),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t2_p2",
        &[
            fx::header("p2", Some("p"), Some("p")),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10)),
            fx::call("a2", Some("a1"), Some("r2"), out(20)),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let key = &keys["p"];
    let before = all_events(&mut scanner, key, 50, BIG);
    assert_eq!(before.len(), 7, "both members, header included");
    let latest = before.last().unwrap();
    assert_eq!(entry_of(&latest.id), "a2");
    let anchor = PageAnchor {
        timestamp: latest.timestamp,
        source_order: latest.source_order,
        stable_id: latest.id.clone(),
    };

    // The earlier segment grows (a late write): it lands in member order, and the
    // newest event of the chain is unchanged.
    fx::append(&first, &[fx::call("a9", Some("a1"), Some("r9"), out(5))]);
    let after = all_events(&mut scanner, key, 50, BIG);
    assert_eq!(after.len(), 8);
    assert_eq!(entry_of(&after.last().unwrap().id), "a2");
    assert!(after.iter().any(|e| entry_of(&e.id) == "a9"));
    assert_eq!(
        try_count(&mut scanner, key, &anchor, BIG, STEPS).unwrap(),
        0
    );
    let counted: Vec<_> = after
        .iter()
        .filter(|e| e.usage.is_some())
        .map(|e| entry_of(&e.id).to_owned())
        .collect();
    assert_eq!(counted, ["a1", "a9", "a2"], "each request is counted once");
    let summary = hydrate_complete(&mut scanner, key, NOW);
    assert_eq!(summary.usage.unwrap().coverage.observed_requests, 3);
}

fn blocks_call(id: &str, parent: Option<&str>, n: usize, size: usize) -> Value {
    let mut call = fx::call(id, parent, Some(&format!("r-{id}")), out(10));
    let text = "word\n".repeat(size / 5);
    call["message"]["content"] = Value::Array(
        (0..n)
            .map(|i| json!({"type": "text", "text": format!("{i}:{text}")}))
            .collect(),
    );
    call
}

#[test]
fn orchestration_is_not_added_twice_to_the_lifetime_total() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_o",
        &[
            fx::header("o", None, None),
            fx::user_message("u1", None, "go"),
            fx::call(
                "a1",
                Some("u1"),
                Some("r1"),
                fx::with_orchestration(fx::usage(1000, 500, 100, 0, 0.0), 10, 20, 30),
            ),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let usage = hydrate_complete(&mut scanner, &keys["o"], NOW)
        .usage
        .unwrap();
    let c = &usage.categories;
    let legs =
        c.input.unwrap() + c.output.unwrap() + c.cache_read.unwrap() + c.cache_write.unwrap_or(0);
    assert_eq!(usage.total, Some(legs), "normalized legs only: {c:?}");
    assert_eq!(usage.total, Some(1_660));
}

#[test]
fn a_stale_source_locator_stays_stale_on_every_repeat() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let path = fx::write_session(
        &root,
        "b",
        "t1_w",
        &[
            fx::header("w", None, None),
            fx::user_message("u1", None, "original"),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let page = transcript_ready(
        &mut scanner,
        &keys["w"],
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let locator = page.events.last().unwrap().provenance.locator.clone();
    assert!(
        try_source(&mut scanner, &locator, BIG, STEPS)
            .unwrap()
            .raw
            .contains("original")
    );
    // Same line boundaries, different content.
    fx::write_session(
        &root,
        "b",
        "t1_w",
        &[
            fx::header("w", None, None),
            fx::user_message("u1", None, "REPLACED"),
        ],
    );
    support::set_modified(&path, 1_900_000_000);
    for attempt in 0..3 {
        // No transcript refresh in between.
        match try_source(&mut scanner, &locator, BIG, STEPS) {
            Err(_) => {}
            Ok(record) => panic!("attempt {attempt} served {:?}", record.raw.len()),
        }
    }
}

#[test]
fn a_long_compaction_summary_is_charged_to_the_details_byte_budget() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let mut entries = vec![
        fx::header("c", None, None),
        fx::user_message("u1", None, "go"),
    ];
    let mut last = "u1".to_owned();
    for n in 0..3 {
        let id = format!("k{n}");
        let mut k = fx::compaction(&id, Some(&last), "u1");
        k["summary"] = json!("s".repeat(20_000));
        entries.push(k);
        last = id;
    }
    fx::write_session(&root, "b", "t1_c", &entries);
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let page = details_ready(
        &mut scanner,
        &keys["c"],
        None,
        PageRequest {
            max_events: 100,
            max_bytes: 1_536,
        },
        BIG,
        STEPS,
    );
    let summaries: usize = page
        .compactions
        .iter()
        .filter_map(|c| c.summary.as_ref())
        .map(|s| s.preview.len())
        .sum();
    let events: usize = page
        .recent
        .iter()
        .map(|e| e.text.as_ref().map_or(0, |t| t.preview.len()))
        .sum();
    assert!(summaries <= 1_536 / 3, "summaries {summaries}");
    assert!(summaries + events <= 1_536, "{summaries} + {events}");
    assert!(
        page.compactions
            .iter()
            .all(|c| c.summary.as_ref().unwrap().truncated)
    );
}

#[test]
fn more_than_the_block_ceiling_is_reported_not_silently_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_m",
        &[
            fx::header("m", None, None),
            fx::user_message("u1", None, "go"),
            blocks_call("a1", Some("u1"), 100, 5),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let events = all_events(&mut scanner, &keys["m"], 500, BIG);
    let omitted = events.last().unwrap();
    assert!(omitted.text.as_ref().unwrap().truncated);
    assert!(
        omitted
            .text
            .as_ref()
            .unwrap()
            .preview
            .contains("36 further content blocks")
    );
    assert!(omitted.provenance.locator.generation.is_some());
    let page = transcript_ready(
        &mut scanner,
        &keys["m"],
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert!(
        page.truncated,
        "the capped page is not presented as complete"
    );
    let summary = hydrate_complete(&mut scanner, &keys["m"], NOW);
    assert_eq!(summary.displayed_event_count, events.len());
}

#[test]
fn a_huge_multi_block_record_is_read_once_and_pages_stay_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let mut entries = vec![
        fx::header("h", None, None),
        fx::user_message("u1", None, "go"),
    ];
    let mut last = "u1".to_owned();
    // 3 records of 64 blocks x 15 KiB (~1 MiB each).
    for n in 0..3 {
        let id = format!("a{n}");
        entries.push(blocks_call(&id, Some(&last), 64, 12_000));
        last = id;
    }
    fx::write_session(&root, "b", "t1_h", &entries);
    let mut scanner = scanner(&root);
    let keys = keys(&mut scanner);
    let small = RefreshBudget {
        max_records: 256,
        max_bytes: 1 << 20,
        max_millis: 60_000,
    };
    // Warm up: hydration and indexing are separate bounded work.
    let _ = transcript_ready(
        &mut scanner,
        &keys["h"],
        None,
        PageDirection::Backward,
        PageRequest {
            max_events: 1,
            max_bytes: 1 << 20,
        },
        small,
        STEPS,
    );
    let mut total = 0usize;
    let mut anchor: Option<PageAnchor> = None;
    let mut calls = 0;
    loop {
        agentscope::perf::reset();
        let page = transcript_ready(
            &mut scanner,
            &keys["h"],
            anchor.clone(),
            PageDirection::Backward,
            PageRequest {
                max_events: 500,
                max_bytes: 10_000_000,
            },
            small,
            STEPS,
        );
        let used = agentscope::perf::snapshot().bytes_read;
        // One budget plus the one record that must always be taken.
        assert!(used <= 2 * (1 << 20) + (1 << 20) + 8192, "{used}");
        total += page.events.len();
        calls += 1;
        anchor = page.next.clone();
        if !page.has_more {
            break;
        }
        assert!(calls < 100);
    }
    assert!(calls > 1, "the budget ended pages early");
    assert_eq!(total, 2 + 3 * 64);
}
