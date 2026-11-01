//! The Pi reader end to end over synthetic JSONL: the newest event of a session
//! larger than one page, exact older identities without duplicate usage, source
//! inspection, unsupported-record isolation, torn appends, truncation/replacement and
//! job-scoped cancellation, driven through the real `PiScanner` and its bounded
//! `ProviderReader` methods.

mod support;

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    EventKind, Harness, PageAnchor, PageDirection, PageRequest, RefreshBudget, RequestPurpose,
    SessionKey,
};
use agentscope::providers::ProviderReader;
use agentscope::providers::pi::PiScanner;
use serde_json::{Value, json};
use support::facts::price_schedule;
use support::pi as fx;
use support::world::{
    BIG, INLINE_SECRET, PAGE, STEPS, details_ready, discover_pass, hydrate_complete, new_job,
    transcript_ready, transcript_steps, try_count, try_source, try_transcript,
};

const NOW: i64 = support::NOW;

/// Few records per call, so a large session must yield `Pending` slices.
const TINY: RefreshBudget = RefreshBudget {
    max_records: 7,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

/// The hydration slice that stops before a 500-record session is folded.
const HALF_HYDRATION: RefreshBudget = RefreshBudget {
    max_records: 256,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn config(root: &Path) -> Config {
    let cli = CliOverrides {
        pi_sessions: Some(root.to_path_buf()),
        ..CliOverrides::default()
    };
    let mut config = Config::load_with_env(&cli, &Env::new()).expect("valid test config");
    config.pricing = price_schedule(
        r#"[{"billing_provider":"anthropic","model":"claude-test",
             "rates":{"input":0.0,"output":10.0,"cache_read":0.0,"cache_write":0.0}}]"#,
    );
    config
}

fn scanner(root: &Path) -> PiScanner {
    PiScanner::new(&config(root)).expect("scanner")
}

/// Discovers every session and returns its key by native ID.
fn session_keys(scanner: &mut PiScanner) -> HashMap<String, SessionKey> {
    let found = discover_pass(scanner, NOW, BIG);
    let mut by_id = HashMap::new();
    for meta in found.sessions {
        by_id.insert(meta.key.native_id.clone(), meta.key);
    }
    by_id
}

/// The entry ID an event ID was built from.
fn entry_of(id: &str) -> &str {
    id.rsplit_once('@').map_or(id, |(entry, _)| entry)
}

fn ids(events: &[agentscope::domain::Event]) -> Vec<String> {
    events.iter().map(|e| entry_of(&e.id).to_owned()).collect()
}

/// Every event of a session, oldest first, read through backward pages of `size`.
fn all_events(
    scanner: &mut PiScanner,
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

fn session_root(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("sessions")
}

/// Appends exactly `body` (used for torn and hostile fixtures).
fn append_raw(path: &Path, body: &[u8]) {
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open");
    file.write_all(body).expect("append");
}

/// A long linear conversation: `u0`, then `a1..=a{count}` with increasing stamps and
/// a recorded request on each assistant message.
fn long_session(id: &str, count: usize) -> Vec<Value> {
    let mut entries = vec![fx::header(id, None)];
    entries.push(fx::at(
        fx::message("u0", None, "user", "start", None),
        &fx::stamp(0),
    ));
    for n in 1..=count {
        let previous = if n == 1 {
            "u0".to_owned()
        } else {
            format!("a{}", n - 1)
        };
        entries.push(fx::at(
            fx::message(
                &format!("a{n}"),
                Some(&previous),
                "assistant",
                &format!("answer {n}"),
                Some(fx::usage(1, 1, 0, 0)),
            ),
            &fx::stamp(n as i64),
        ));
    }
    entries
}

#[test]
fn a_large_session_opens_at_the_true_last_event_and_pages_back_exactly() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let entries = long_session("big", 260);
    fx::write_session(&root, "proj", "big", &entries);
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["big"];

    // The first page is the newest one: reading it takes several bounded indexing
    // slices (`Pending` is progress, never an error) and ends at the true last event.
    let (latest, pending) = transcript_steps(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PageRequest {
            max_events: 10,
            max_bytes: 10_000_000,
        },
        TINY,
        STEPS,
    );
    assert!(pending > 0, "a 261-row session cannot fit one tiny slice");
    assert_eq!(ids(&latest.events).last().map(String::as_str), Some("a260"));
    assert!(
        !latest.events.is_empty() && latest.events.len() <= 10,
        "a bounded page of newest events"
    );
    assert!(latest.has_more);
    assert!(latest.diagnostics.is_empty());

    // Older pages recover every exact older identity once, in chronological order.
    let events = all_events(&mut scanner, key, 10, TINY);
    let mut expected: Vec<String> = vec!["u0".into()];
    expected.extend((1..=260).map(|n| format!("a{n}")));
    assert_eq!(ids(&events), expected);
    assert_eq!(events.last().unwrap().kind, EventKind::AssistantMessage);

    // Each recorded request is attached to exactly one event: no duplicate usage
    // through overlapping or backward pages.
    let with_usage: Vec<_> = events.iter().filter(|e| e.usage.is_some()).collect();
    assert_eq!(with_usage.len(), 260, "one request per assistant message");
    assert_eq!(
        with_usage
            .iter()
            .filter(|e| e.usage.as_ref().unwrap().categories.output == Some(1))
            .count(),
        260
    );

    // Counting after the first page's oldest anchor matches the remaining rows.
    let anchor = latest.next.clone().expect("a continuation anchor");
    let count = try_count(&mut scanner, key, &anchor, BIG, STEPS).expect("count");
    assert_eq!(
        count,
        latest.events.len() - 1,
        "the rows newer than the page's oldest anchor"
    );

    // Details and the summary agree with the transcript.
    let details = details_ready(
        &mut scanner,
        key,
        None,
        PageRequest {
            max_events: 50,
            max_bytes: 10_000_000,
        },
        BIG,
        STEPS,
    );
    assert_eq!(details.requests.len(), 25);
    assert_eq!(details.compactions.len(), 0);
    assert!(details.has_more, "older events remain");
    let summary = hydrate_complete(&mut scanner, key, NOW);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.coverage.observed_requests, 260);
    assert_eq!(usage.categories.output, Some(260));
    assert_eq!(summary.displayed_event_count, 261);
}

#[test]
fn source_inspection_maps_the_original_record() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let entries = vec![
        fx::header("mapped", None),
        fx::message("u1", None, "user", "inspect me", None),
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "the recorded answer",
            Some(fx::usage(2, 3, 0, 0)),
        ),
        json!({"type": "unseen_shape", "id": "x1", "parentId": "a1", "timestamp": fx::STAMP}),
    ];
    fx::write_session(&root, "proj", "mapped", &entries);
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["mapped"];

    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let assistant = page
        .events
        .iter()
        .find(|e| entry_of(&e.id) == "a1")
        .expect("assistant event");
    let record =
        try_source(&mut scanner, &assistant.provenance.locator, BIG, STEPS).expect("source record");
    assert!(record.unavailable.is_none());
    assert!(record.raw.contains("\"id\":\"a1\""), "{}", record.raw);
    assert!(record.display.contains("the recorded answer"));
    assert_eq!(
        assistant.provenance.source,
        agentscope::domain::SourceKey::File(root.join("proj").join("mapped.jsonl"))
    );
    assert_eq!(assistant.provenance.record_type, "message/assistant");

    // The unsupported record's diagnostic points at exactly that source line, and the
    // valid neighbours are untouched.
    assert_eq!(page.diagnostics.len(), 1);
    let diagnostic = &page.diagnostics[0];
    assert!(diagnostic.hidden);
    assert!(diagnostic.label.contains("unseen_shape"));
    let raw = try_source(&mut scanner, &diagnostic.locator, BIG, STEPS).expect("diagnostic source");
    assert!(raw.raw.contains("\"type\":\"unseen_shape\""));
}

#[test]
fn unsupported_and_malformed_records_do_not_erase_valid_neighbours() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let mut body = Vec::new();
    for entry in [
        fx::header("isolation", None),
        fx::message("u1", None, "user", "first", None),
        json!({"type": "extension_thing", "id": "x1", "parentId": "u1", "timestamp": fx::STAMP}),
    ] {
        body.extend_from_slice(entry.to_string().as_bytes());
        body.push(b'\n');
    }
    body.extend_from_slice(b"{not json\n");
    for entry in [
        fx::message(
            "a1",
            Some("x1"),
            "assistant",
            "still here",
            Some(fx::usage(1, 1, 0, 0)),
        ),
        json!({"type": "label", "id": "l1", "parentId": "a1", "timestamp": fx::STAMP,
               "targetId": "u1", "label": "marker"}),
        fx::message("u2", Some("a1"), "user", "after", None),
    ] {
        body.extend_from_slice(entry.to_string().as_bytes());
        body.push(b'\n');
    }
    fx::write_raw(&root, "proj", "isolation", &body);
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["isolation"];

    let events = all_events(&mut scanner, key, 2, BIG);
    assert_eq!(ids(&events), vec!["u1", "a1", "u2"]);
    let assistant = events.iter().find(|e| entry_of(&e.id) == "a1").unwrap();
    assert_eq!(assistant.text.as_ref().unwrap().preview, "still here");
    assert!(assistant.usage.is_some());

    // Diagnostics for the unsupported record, the unreadable line and the uninterpreted
    // label; they never hide the valid rows between them.
    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(page.events.len(), 3);
    assert_eq!(page.diagnostics.len(), 3);
    assert!(page.diagnostics.iter().all(|d| d.hidden));
    let labels: Vec<&str> = page.diagnostics.iter().map(|d| d.label.as_str()).collect();
    assert!(labels.iter().any(|l| l.contains("extension_thing")));
    assert!(labels.iter().any(|l| l.contains("label")));
    assert!(labels.contains(&"unreadable record"));

    let summary = hydrate_complete(&mut scanner, key, NOW);
    assert_eq!(summary.hidden_records, 3);
    assert_eq!(
        summary.usage.as_ref().unwrap().coverage.observed_requests,
        1
    );
}

#[test]
fn a_torn_trailing_record_is_never_a_phantom_and_completes_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let path = fx::write_session(
        &root,
        "proj",
        "torn",
        &[
            fx::header("torn", None),
            fx::message("u1", None, "user", "begin", None),
            fx::message(
                "a1",
                Some("u1"),
                "assistant",
                "first answer",
                Some(fx::usage(2, 2, 0, 0)),
            ),
        ],
    );
    // Half of the second assistant record: no terminator, no closing braces.
    let full = fx::message(
        "a2",
        Some("a1"),
        "assistant",
        "second answer",
        Some(fx::usage(3, 3, 0, 0)),
    )
    .to_string();
    let cut = full.len() / 2;
    append_raw(&path, &full.as_bytes()[..cut]);

    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["torn"];

    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(ids(&page.events), vec!["u1", "a1"]);
    let summary = hydrate_complete(&mut scanner, key, NOW);
    let usage = summary.usage.as_ref().unwrap();
    assert_eq!(usage.coverage.observed_requests, 1, "no phantom request");
    assert_eq!(usage.categories.output, Some(2));

    // The terminator arrives: the record is read exactly once, as one request.
    append_raw(&path, format!("{}\n", &full[cut..]).as_bytes());
    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(ids(&page.events), vec!["u1", "a1", "a2"]);
    let matching: Vec<_> = page
        .events
        .iter()
        .filter(|e| e.usage.is_some())
        .map(|e| entry_of(&e.id).to_owned())
        .collect();
    assert_eq!(matching, vec!["a1", "a2"]);

    let summary = hydrate_complete(&mut scanner, key, NOW);
    let usage = summary.usage.as_ref().unwrap();
    assert_eq!(usage.coverage.observed_requests, 2, "one request, once");
    assert_eq!(usage.categories.output, Some(5));
    assert!(usage.coverage.complete);
}

#[test]
fn truncation_and_replacement_invalidate_rows_and_locators() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let entries = long_session("big", 6);
    let path = fx::write_session(&root, "proj", "shrink", &entries);
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["big"];

    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(page.events.len(), 7);
    let old_locator = page.events[0].provenance.locator.clone();
    let old_anchor = page
        .events
        .last()
        .map(|e| PageAnchor {
            timestamp: e.timestamp,
            source_order: e.source_order,
            stable_id: e.id.clone(),
        })
        .unwrap();

    // Truncate in place: the same session now holds only its first exchange.
    fx::write_session(&root, "proj", "shrink", &entries[..2]);
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert!(found.invalidated.iter().any(|k| k == key));

    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(ids(&page.events), vec!["u0"]);
    // The old anchor names an event this generation no longer has.
    let stale = transcript_ready(
        &mut scanner,
        key,
        Some(old_anchor),
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert!(stale.events.is_empty());
    assert!(!stale.has_more);
    // A locator from the old generation is terminal, not silently re-read.
    let error = try_source(&mut scanner, &old_locator, BIG, STEPS).expect_err("stale locator");
    assert!(matches!(
        error,
        agentscope::providers::ProviderError::Unsupported(_)
    ));

    // Replacing the path with another session identity retires the old key and lists
    // the new one; nothing is guessed from the old rows.
    fx::write_session(
        &root,
        "proj",
        "replaced",
        &[
            fx::header("replaced", None),
            fx::message("r1", None, "user", "other session", None),
        ],
    );
    let _ = fs::remove_file(&path);
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert!(found.removed.iter().any(|k| k == key));
    let error = try_transcript(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    )
    .expect_err("the retired key is unavailable");
    assert!(matches!(
        error,
        agentscope::providers::ProviderError::Unavailable(_)
    ));
    let mut keys = session_keys(&mut scanner);
    let replacement = found
        .sessions
        .iter()
        .find(|meta| meta.key.native_id == "replaced")
        .map(|meta| meta.key.clone())
        .or_else(|| keys.remove("replaced"))
        .expect("the replacement session");
    let page = transcript_ready(
        &mut scanner,
        &replacement,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(ids(&page.events), vec!["r1"]);
}

#[test]
fn cancelling_a_hydration_releases_its_ledger_and_the_next_preview_is_correct() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    fx::write_session(&root, "proj", "large", &long_session("large", 500));
    fx::write_session(
        &root,
        "proj",
        "small",
        &[
            fx::header("small", None),
            fx::message("s1", None, "user", "small prompt", None),
            fx::message(
                "s2",
                Some("s1"),
                "assistant",
                "small answer",
                Some(fx::usage(0, 77, 0, 0)),
            ),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let large = keys["large"].clone();
    let small = keys["small"].clone();

    // One 256-record slice of a 500-record hydration leaves partial work behind.
    let first = new_job();
    let delta = scanner
        .hydrate(first, &large, NOW, HALF_HYDRATION)
        .expect("hydrate slice");
    assert!(!delta.done);
    assert_eq!(scanner.job_state(), 1);
    assert_eq!(scanner.active_work(), 1);

    // Cancelling releases the abandoned ledger and drop the partial fold.
    scanner.cancel_read(&large, first);
    assert_eq!(scanner.job_state(), 0);
    assert_eq!(scanner.active_work(), 0);

    // A different selection hydrates from scratch and reports exactly its own facts.
    let second = new_job();
    let delta = scanner
        .hydrate(second, &small, NOW, BIG)
        .expect("second preview");
    assert!(delta.done);
    let summary = delta.summary;
    assert_eq!(summary.meta.key, small);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.coverage.observed_requests, 1);
    assert_eq!(usage.categories.output, Some(77));
    assert_eq!(scanner.job_state(), 0, "a finished job holds nothing");
    assert_eq!(scanner.cached_sessions(), 1);
    // The abandoned session is still readable afterwards.
    let page = transcript_ready(
        &mut scanner,
        &large,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert!(!page.events.is_empty());
}

#[test]
fn details_share_one_record_and_byte_budget_across_lists() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let long = "s".repeat(20_000);
    let mut entries = vec![fx::header("budget", None)];
    entries.push(fx::message("u1", None, "user", "start", None));
    for n in 0..4 {
        let id = format!("c{n}");
        let mut marker = fx::compaction(&id, "u1", Some(fx::usage(0, 1, 0, 0)));
        marker["summary"] = json!(long);
        entries.push(marker);
        entries.push(fx::message(
            &format!("a{n}"),
            Some(&id),
            "assistant",
            &format!("answer {n}"),
            Some(fx::usage(0, 1, 0, 0)),
        ));
    }
    fx::write_session(&root, "proj", "budget", &entries);
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["budget"];

    let request = PageRequest {
        max_events: 6,
        max_bytes: 4096,
    };
    let details = details_ready(&mut scanner, key, None, request, BIG, STEPS);
    let records = details.compactions.len() + details.requests.len() + details.recent.len();
    assert!(records <= request.max_events, "{records} records");
    assert_eq!(
        details.compactions.len(),
        2,
        "a third of the record allowance"
    );
    assert!(details.truncated, "older markers are cut");

    let bytes = details
        .compactions
        .iter()
        .map(|c| c.summary.as_ref().map_or(0, |s| s.preview.len()))
        .sum::<usize>()
        + details
            .recent
            .iter()
            .map(|e| {
                e.text.as_ref().map_or(0, |t| t.preview.len())
                    + e.tool.as_ref().map_or(0, |t| {
                        t.name.len()
                            + t.input.as_ref().map_or(0, |i| i.preview.len())
                            + t.output.as_ref().map_or(0, |o| o.preview.len())
                    })
            })
            .sum::<usize>();
    assert!(
        bytes <= request.max_bytes,
        "{bytes} > {}",
        request.max_bytes
    );
    for marker in &details.compactions {
        let summary = marker.summary.as_ref().expect("a bounded summary");
        assert!(summary.truncated);
        let usage = marker.usage.as_ref().expect("recorded summary usage");
        assert_eq!(
            usage.request.as_ref().map(|r| r.purpose.clone()),
            Some(RequestPurpose::Compaction)
        );
        assert!(usage.money.saved_estimate.is_none());
    }

    // A later page continues older events without repeating the markers.
    let older = details_ready(&mut scanner, key, details.next.clone(), request, BIG, STEPS);
    assert!(older.compactions.is_empty());
    assert!(
        older
            .recent
            .iter()
            .all(|e| details.recent.iter().all(|newer| newer.id != e.id))
    );
}

#[test]
fn request_speed_and_session_end_stay_unknown_without_recorded_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    fx::write_session(
        &root,
        "proj",
        "no-timing",
        &[
            fx::header("no-timing", None),
            fx::message("u1", None, "user", "x", None),
            fx::message(
                "a1",
                Some("u1"),
                "assistant",
                "ok",
                Some(fx::usage(0, 40, 0, 0)),
            ),
            fx::tool_result("t1", "a1", 9000),
        ],
    );
    fx::write_session(
        &root,
        "proj",
        "timed",
        &[
            fx::header("timed", None),
            fx::message("u2", None, "user", "y", None),
            fx::with_duration(
                fx::with_response(
                    fx::message(
                        "a2",
                        Some("u2"),
                        "assistant",
                        "ok",
                        Some(fx::usage(0, 40, 0, 0)),
                    ),
                    "resp-timed",
                ),
                1200,
            ),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);

    let unknown = hydrate_complete(&mut scanner, &keys["no-timing"], NOW);
    assert_eq!(unknown.lifecycle, agentscope::domain::Lifecycle::Unknown);
    assert_eq!(unknown.metrics.timing.ended_at, None);
    assert_eq!(unknown.metrics.timing.active_ms, None);
    assert_eq!(unknown.metrics.timing.model_ms, None);
    assert_eq!(unknown.metrics.speed.measured_requests, 0);
    assert!(unknown.metrics.speed.latest.is_none());

    let timed = hydrate_complete(&mut scanner, &keys["timed"], NOW);
    assert_eq!(timed.metrics.speed.measured_requests, 1);
    assert_eq!(
        timed.metrics.timing.model_ms.map(|ms| ms.round()),
        Some(1200.0)
    );
    assert_eq!(
        timed.metrics.timing.ended_at, None,
        "no session end recorded"
    );
    assert_eq!(timed.lifecycle, agentscope::domain::Lifecycle::Unknown);
}

#[test]
fn discovery_lists_only_native_session_files_and_no_invented_children() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    fx::write_session(
        &root,
        "proj",
        "one",
        &[
            fx::header("one", None),
            fx::message("u1", None, "user", "hello", None),
        ],
    );
    // Not a session: a credential file inside the bucket, a non-JSONL file at the
    // root, an extension blob one level deeper, and a backup copy.
    fs::write(
        root.join("proj").join("auth.json"),
        format!("{{\"token\":\"{}\"}}", "SENTINEL-CRED-7f3a91"),
    )
    .unwrap();
    fs::write(root.join("notes.jsonl"), "{}").unwrap();
    fx::write_session(
        &root,
        "proj/ext/child",
        "extension",
        &[fx::header("ext", None)],
    );
    fs::write(root.join("proj").join("one.jsonl.bak"), "{}\n").unwrap();

    let mut scanner = scanner(&root);
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), 1);
    assert_eq!(found.sessions[0].key.native_id, "one");
    assert_eq!(found.sessions[0].harness_version.as_deref(), Some("v3"));
    assert!(found.sessions[0].lineage.parent.is_none());
    assert!(found.sessions[0].lineage.fork.is_none());
    assert!(found.sessions[0].members.is_empty());
    assert!(
        found
            .sessions
            .iter()
            .all(|meta| meta.key.harness == Harness::Pi)
    );
    assert_eq!(scanner.active_work(), 0);
    assert_eq!(scanner.cached_sessions(), 0);
}

#[test]
fn the_shared_world_pi_family_is_discovered_and_owned() {
    let world = support::world::World::with_pi();
    let plant = world.pi_plant.as_ref().expect("planted Pi family");
    let mut reader = PiScanner::new(&world.config()).expect("scanner");
    let found = discover_pass(&mut reader, NOW, BIG);
    let ids: Vec<String> = found
        .sessions
        .iter()
        .map(|m| m.key.native_id.clone())
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    for key in [&plant.parent, &plant.fork, &plant.secret] {
        let summary = hydrate_complete(&mut reader, key, NOW);
        assert_eq!(
            summary.usage.as_ref().and_then(|u| u.total),
            plant.tokens.iter().find(|(k, _)| k == key).map(|(_, t)| *t),
            "{}",
            key.native_id
        );
    }
}

#[test]
fn the_native_world_lists_the_pi_family_without_invented_children() {
    let world = support::world::World::native();
    let grown = world.grown();
    let mut readers = world.readers();
    let pi = readers
        .iter_mut()
        .find(|r| r.harness() == Harness::Pi)
        .expect("the Pi reader is registered");
    let found = discover_pass(pi.as_mut(), NOW, BIG);
    let ids: Vec<String> = found
        .sessions
        .iter()
        .map(|m| m.key.native_id.clone())
        .collect();
    assert_eq!(ids.len(), 3, "{ids:?}");
    let family = [&grown.pi.parent, &grown.pi.fork, &grown.pi.secret];
    for meta in &found.sessions {
        assert!(family.contains(&&meta.key));
        assert!(
            meta.lineage.parent.is_none(),
            "no native child record exists"
        );
        assert!(meta.members.is_empty());
    }
    let fork = found
        .sessions
        .iter()
        .find(|m| m.key == grown.pi.fork)
        .expect("the fork root");
    assert!(matches!(
        &fork.lineage.fork,
        Some(agentscope::domain::SessionLink::Resolved(key)) if key == &grown.pi.parent
    ));
}

// ---- review regressions ------------------------------------------------------------

/// P1: masking happens before raw-field truncation. The quoted value starts inside the
/// 4,096-byte preview window but its closing delimiter lies beyond the 8,192-byte raw cut,
/// so a truncate-first implementation would expose the unterminated value in the preview,
/// while a mask-first one shows the redaction.
#[test]
fn a_quoted_secret_across_both_boundaries_is_masked_before_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    // The delimiter sits just past the preview window; the value (and its closing quote)
    // runs past the raw-field cut.
    let padding = "x".repeat(4_000);
    // Trailing text keeps the *masked* field over the raw cut, so truncation happens on
    // the masked form rather than shrinking it away.
    let quoted = |prefix: &str| {
        let value = format!("{prefix}-{}-TAIL", "V".repeat(5_000));
        format!("{padding}password=\"{value}\" {}", "y".repeat(5_000))
    };
    let content = quoted("SENTINEL-BLOCK");
    assert!(content.len() > 8_192);
    fx::write_session(
        &root,
        "proj",
        "boundary",
        &[
            fx::header("boundary", None),
            fx::message(
                "a1",
                None,
                "assistant",
                &content,
                Some(fx::usage(0, 1, 0, 0)),
            ),
            // The same shape through the `owned_text` path (a session-name notice).
            fx::session_info("i1", "a1", &quoted("SENTINEL-NAME")),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["boundary"];
    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let text = page
        .events
        .iter()
        .find_map(|event| event.text.as_ref())
        .expect("the message text");
    assert!(text.truncated);
    assert_eq!(text.byte_len, content.len(), "the original length is kept");
    assert!(
        text.preview.contains("password=[REDACTED]"),
        "the delimiter inside the preview must be masked: {}",
        text.preview
            .chars()
            .skip(3_990)
            .take(80)
            .collect::<String>()
    );
    assert!(
        !text.preview.contains("SENTINEL"),
        "a truncated prefix leaked a secret"
    );
    let notice = page
        .events
        .iter()
        .filter_map(|event| event.text.as_ref())
        .find(|text| text.preview.starts_with("session name:"))
        .expect("the session-name notice");
    assert!(notice.preview.contains("password=[REDACTED]"), "{notice:?}");
    assert!(!notice.preview.contains("SENTINEL"));

    // The details page reads the same materialized text.
    let details = details_ready(
        &mut scanner,
        key,
        None,
        PageRequest {
            max_events: 6,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    for event in &details.recent {
        if let Some(text) = &event.text {
            assert!(!text.preview.contains("SENTINEL"));
        }
    }
}

/// P2: a resumed hydration reports the newer discovery metadata (rename/activity), so a
/// later preview cannot move the roster's activity backwards.
#[test]
fn a_resumed_hydration_reports_the_newer_title_and_activity() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let path = fx::write_session(
        &root,
        "proj",
        "renamed",
        &[
            fx::header("renamed", None),
            fx::message("u1", None, "user", "original prompt", None),
            fx::message(
                "a1",
                Some("u1"),
                "assistant",
                "answer",
                Some(fx::usage(0, 10, 0, 0)),
            ),
        ],
    );
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["renamed"];
    let first = hydrate_complete(&mut scanner, key, NOW);
    let before = first.meta.activity.at.expect("recorded activity");
    assert_eq!(first.meta.title, "original prompt");

    // A rename and a newer turn: discovery reports newer title/activity.
    fx::append(
        &path,
        &[
            fx::at(
                fx::session_info("i1", "a1", "Renamed Pi session"),
                &fx::stamp(50),
            ),
            fx::at(
                fx::message("u2", Some("i1"), "user", "later prompt", None),
                &fx::stamp(60),
            ),
        ],
    );
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert!(found.sessions.iter().any(|m| m.key == *key));
    let second = hydrate_complete(&mut scanner, key, NOW);
    assert_eq!(second.meta.title, "Renamed Pi session");
    let after = second.meta.activity.at.expect("newer activity");
    assert!(after > before, "{after} <= {before}");
    let usage = second.usage.as_ref().expect("usage");
    assert_eq!(
        usage.coverage.observed_requests, 1,
        "the resumed fold does not re-count the old request"
    );
    assert_eq!(usage.categories.output, Some(10));
}

/// P2: opening a fork before its parent is discovered, then after, then after it
/// disappears, recomputes ownership (index and hydration) instead of keeping stale
/// counted flags.
#[test]
fn fork_evidence_changes_rebuild_ownership_dependent_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let parent_name = "fork-parent";
    let parent_path = root.join("proj").join(format!("{parent_name}.jsonl"));
    let copied = [
        fx::message("u0", None, "user", "shared prompt", None),
        fx::message(
            "a1",
            Some("u0"),
            "assistant",
            "shared answer",
            Some(fx::usage(600, 0, 400, 0)),
        ),
    ];
    // The fork exists first, naming a parent that is not (yet) discovered. The fork's
    // copied history is verbatim and it adds one request of its own.
    let mut fork_entries = vec![fx::header("fork", Some(&parent_path.to_string_lossy()))];
    fork_entries.extend(copied.iter().cloned());
    fork_entries.push(fx::message(
        "f1",
        Some("a1"),
        "assistant",
        "own answer",
        Some(fx::usage(10, 5, 0, 0)),
    ));
    let fork_path = fx::write_session(&root, "proj", "fork", &fork_entries);
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let fork = keys["fork"].clone();

    // Unresolved predecessor: nothing can be proven owned, so no request is charged.
    let unknown = hydrate_complete(&mut scanner, &fork, NOW);
    assert_eq!(
        unknown.usage.as_ref().unwrap().coverage.observed_requests,
        0
    );
    let page = transcript_ready(
        &mut scanner,
        &fork,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert!(page.events.iter().all(|e| e.usage.is_none()));

    // The parent appears: discovery republishes a resolved fork link and the index and
    // hydration are rebuilt, so only the fork's own call is charged.
    let mut parent_entries = vec![fx::header("parent", None)];
    parent_entries.extend(copied.iter().cloned());
    fx::write_session(&root, "proj", parent_name, &parent_entries);
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert!(
        found
            .sessions
            .iter()
            .any(|m| m.key.native_id == "fork" && m.lineage.fork.is_some())
    );
    let resolved = hydrate_complete(&mut scanner, &fork, NOW);
    let usage = resolved.usage.as_ref().unwrap();
    assert_eq!(
        usage.coverage.observed_requests, 1,
        "only the fork's own call"
    );
    assert_eq!(usage.categories.output, Some(5));
    let page = transcript_ready(
        &mut scanner,
        &fork,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let counted: Vec<&str> = page
        .events
        .iter()
        .filter(|e| e.usage.is_some())
        .map(|e| entry_of(&e.id))
        .collect();
    assert_eq!(counted, vec!["f1"], "the copied call is inherited");

    // The parent disappears: the fork's ownership evidence is gone again and both the
    // preview and the transcript stop charging anything.
    let _ = fs::remove_file(fork_path.with_file_name("fork-parent.jsonl"));
    let _ = discover_pass(&mut scanner, NOW, BIG);
    let stale = hydrate_complete(&mut scanner, &fork, NOW);
    assert_eq!(
        stale.usage.as_ref().unwrap().coverage.observed_requests,
        0,
        "unproven ownership is never charged"
    );
    let page = transcript_ready(
        &mut scanner,
        &fork,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert!(page.events.iter().all(|e| e.usage.is_none()));
}

/// P2: an unsupported record type is untrusted text and is masked before it is stored
/// in a diagnostic label (or printed by Debug).
#[test]
fn a_hostile_record_type_is_masked_in_the_diagnostic_label() {
    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let mut body = Vec::new();
    for entry in [
        fx::header("labels", None),
        json!({"type": format!("password={INLINE_SECRET}"), "id": "x1",
               "parentId": null, "timestamp": fx::STAMP}),
        json!({"type": "odd\u{7}type", "id": "x2", "parentId": "x1", "timestamp": fx::STAMP}),
        fx::message("u1", Some("x2"), "user", "still here", None),
    ] {
        body.extend_from_slice(entry.to_string().as_bytes());
        body.push(b'\n');
    }
    fx::write_raw(&root, "proj", "labels", &body);
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["labels"];
    let page = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(page.events.len(), 1, "the valid neighbour is still listed");
    assert_eq!(page.diagnostics.len(), 2);
    for diagnostic in &page.diagnostics {
        assert!(
            !diagnostic.label.contains(INLINE_SECRET),
            "label leaked: {}",
            diagnostic.label
        );
        assert!(!diagnostic.label.contains('\u{7}'));
    }
    assert!(
        page.diagnostics
            .iter()
            .any(|d| d.label.contains("[REDACTED]"))
    );
    let debug = format!("{page:?}");
    assert!(!debug.contains(INLINE_SECRET), "diagnostic Debug leaked");
}

/// P2: marker bodies are materialized under one shared call budget; a large set of
/// retained markers yields resumable `Pending` answers instead of reading every body.
#[test]
fn marker_materialization_shares_the_call_budget_and_resumes() {
    use agentscope::perf;
    use agentscope::providers::ReadProgress;

    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let long = "s".repeat(600 * 1024);
    let mut entries = vec![fx::header("budget", None)];
    let mut parent = "u0".to_owned();
    entries.push(fx::message("u0", None, "user", "start", None));
    for n in 0..20 {
        let id = format!("c{n}");
        let mut marker = fx::compaction(&id, &parent, Some(fx::usage(0, 1, 0, 0)));
        marker["summary"] = json!(long);
        entries.push(marker);
        parent = id;
        let answer = format!("a{n}");
        entries.push(fx::message(
            &answer,
            Some(&parent),
            "assistant",
            "answer",
            Some(fx::usage(0, 1, 0, 0)),
        ));
        parent = answer;
    }
    fx::write_session(&root, "proj", "budget", &entries);
    let config = config(&root);
    let record_cap = config.max_record_bytes;
    let mut scanner = PiScanner::new(&config).expect("scanner");
    let keys = session_keys(&mut scanner);
    let key = &keys["budget"];

    let budget = RefreshBudget {
        max_records: 500,
        max_bytes: 2 << 20,
        max_millis: 60_000,
    };
    let ceiling = budget.max_bytes as u64 + record_cap as u64;
    let request = PageRequest {
        max_events: 60,
        max_bytes: 4 << 20,
    };
    let job = new_job();
    let mut pending = 0usize;
    let page = loop {
        perf::reset();
        match scanner
            .details_page(job, key, None, request, budget)
            .expect("details")
        {
            ReadProgress::Pending(_) => {
                pending += 1;
                let used = perf::snapshot();
                assert!(
                    used.bytes_read <= ceiling,
                    "call {pending} read {} bytes",
                    used.bytes_read
                );
            }
            ReadProgress::Ready(page) => {
                let used = perf::snapshot();
                assert!(used.bytes_read <= ceiling);
                break page;
            }
        }
        assert!(pending < 1_000, "marker materialization never finished");
    };
    assert!(pending >= 3, "{pending} bounded calls for 20 large markers");
    assert_eq!(page.compactions.len(), 20);
    let cap = request.max_bytes / 3 / 20;
    let stored: usize = page
        .compactions
        .iter()
        .map(|marker| marker.summary.as_ref().expect("summary").preview.len())
        .sum();
    for marker in &page.compactions {
        let summary = marker.summary.as_ref().expect("summary");
        assert!(summary.preview.len() <= cap);
        assert!(summary.truncated);
    }
    assert!(stored <= request.max_bytes, "{stored} bytes of summaries");
}

/// A linear session whose only records are `count` compaction markers.
fn marker_session(id: &str, count: usize) -> Vec<Value> {
    let mut entries = vec![fx::header(id, None)];
    entries.push(fx::message("u0", None, "user", "start", None));
    let mut parent = "u0".to_owned();
    for n in 0..count {
        let marker = format!("c{n}");
        entries.push(fx::compaction(&marker, &parent, None));
        parent = marker;
    }
    entries
}

/// P2 (fix round 2): an append at the 512-marker cap evicts the oldest marker while the
/// retained length (and `skip`) stay the same, so a pending prefix must be invalidated by
/// the selection revision, not by its length.
#[test]
fn an_append_at_the_marker_cap_invalidates_a_pending_prefix() {
    use agentscope::providers::ReadProgress;

    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    let path = fx::write_session(&root, "proj", "cap", &marker_session("cap", 512));
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["cap"];

    // Build the index first so the details call goes straight to marker bodies.
    let _ = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let request = PageRequest {
        max_events: 60,
        max_bytes: 4 << 20,
    };
    let small = RefreshBudget {
        max_records: 4,
        max_bytes: 100_000_000,
        max_millis: 60_000,
    };
    let job = new_job();
    let first = scanner
        .details_page(job, key, None, request, small)
        .expect("details");
    assert!(matches!(first, ReadProgress::Pending(_)));
    assert_eq!(
        scanner.pending_marker_pages(),
        1,
        "the first call materialized part of its markers"
    );

    // The 513th marker shifts the retained selection: c0 is evicted, c512 is newest.
    fx::append(&path, &[fx::compaction("c512", "c511", None)]);
    let page = details_ready(&mut scanner, key, None, request, BIG, STEPS);
    let ids: Vec<&str> = page
        .compactions
        .iter()
        .map(|marker| marker.id.as_str())
        .collect();
    let expected: Vec<String> = (493..=512).map(|n| format!("pi:c{n}")).collect();
    assert_eq!(ids, expected, "the shifted selection is rebuilt exactly");
    assert!(page.compactions.iter().all(|m| !m.id.contains("pi:c0")));
    assert!(page.truncated, "older markers are cut");
    assert_eq!(scanner.pending_marker_pages(), 0);
}

/// P2 (fix round 2): cancelling the only details job drops its partially materialized
/// marker prefix, and a fresh request rebuilds the page correctly.
#[test]
fn cancelling_a_details_job_drops_its_marker_prefix() {
    use agentscope::providers::ReadProgress;

    let dir = tempfile::tempdir().unwrap();
    let root = session_root(&dir);
    fx::write_session(&root, "proj", "cancel", &marker_session("cancel", 20));
    let mut scanner = scanner(&root);
    let keys = session_keys(&mut scanner);
    let key = &keys["cancel"];
    let _ = transcript_ready(
        &mut scanner,
        key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let request = PageRequest {
        max_events: 60,
        max_bytes: 4 << 20,
    };
    let small = RefreshBudget {
        max_records: 4,
        max_bytes: 100_000_000,
        max_millis: 60_000,
    };
    let job = new_job();
    let first = scanner
        .details_page(job, key, None, request, small)
        .expect("details");
    assert!(matches!(first, ReadProgress::Pending(_)));
    assert_eq!(scanner.pending_marker_pages(), 1);
    assert_eq!(scanner.job_state(), 1);

    scanner.cancel_read(key, job);
    assert_eq!(scanner.job_state(), 0, "the job's ledger is released");
    assert_eq!(
        scanner.pending_marker_pages(),
        0,
        "the abandoned prefix is dropped"
    );

    let page = details_ready(&mut scanner, key, None, request, BIG, STEPS);
    let expected: Vec<String> = (0..20).map(|n| format!("pi:c{n}")).collect();
    let ids: Vec<&str> = page
        .compactions
        .iter()
        .map(|marker| marker.id.as_str())
        .collect();
    assert_eq!(ids, expected);
    assert_eq!(page.compactions.len(), 20);
}
