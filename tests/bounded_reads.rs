//! Bounded-read regressions: zstd indexing is Pending while other work completes,
//! discovery never reads unloaded bodies, and job-scoped cancellation.

mod support;

use agentscope::domain::*;
use agentscope::ingest::source::SourceReader;
use agentscope::perf;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::{ProviderReader, ReadProgress};
use support::codex::{assistant, session_meta, usage, usage_record};
use support::{NOW, config_for, dated, write_lines};

const BIG: RefreshBudget = RefreshBudget {
    max_records: 1_000_000,
    max_bytes: 1_000_000_000,
    max_millis: 600_000,
};
const TS: &str = "2026-10-29T10:00:00Z";

#[test]
fn discovery_does_not_observe_a_middle_sentinel_but_hydration_does() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let mut lines = vec![session_meta(TS, "big", "/work/demo")];
    for i in 0..3000 {
        lines.push(assistant(TS, &format!("filler {i} {}", "x".repeat(60))));
        if i == 1500 {
            // Exactly one usage record, far from both the head and the tail window.
            lines.push(usage_record(TS, usage(500, 0, 277, 0, 777)));
        }
    }
    let path = dated(&root, "rollout-big.jsonl");
    write_lines(&path, &lines);
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(len > 400_000);
    let mut scanner = CodexScanner::new(&config_for(&root)).unwrap();
    perf::reset();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    let used = perf::snapshot();
    assert_eq!(found.sessions.len(), 1);
    assert!(
        used.bytes_read < 40_000 + 8_192,
        "probe read {} of {len}",
        used.bytes_read
    );
    assert!(used.records_parsed < 400, "{} parsed", used.records_parsed);
    // Metadata carries no hydrated facts, so the sentinel total is not observed.
    let summary = support::world::hydrate_complete(&mut scanner, &found.sessions[0].key, NOW);
    assert_eq!(summary.usage.as_ref().and_then(|u| u.total), Some(777));
}

#[test]
fn zstd_indexing_is_pending_while_another_small_source_completes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let mut plain = String::new();
    for i in 0..40_000 {
        plain.push_str(&format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",\"text\":\"r{i}\"}}]}}}}\n"
        ));
    }
    let archive = root.join("archived").join("rollout-old.jsonl.zst");
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    std::fs::write(
        &archive,
        zstd::stream::encode_all(plain.as_bytes(), 3).unwrap(),
    )
    .unwrap();
    let small = dated(&root, "rollout-small.jsonl");
    write_lines(
        &small,
        &[session_meta(TS, "small", "/p"), assistant(TS, "hi")],
    );

    let generation = SourceReader::open(&archive, 1 << 20).unwrap().generation();
    let session = SessionKey {
        harness: Harness::Codex,
        native_id: "old".into(),
        source: SourceKey::File(root.join("archived").join("rollout-old.jsonl")),
    };
    let locator = SourceLocator {
        session: session.clone(),
        kind: LocatorKind::CompressedRecord {
            path: archive,
            ordinal: 39_000,
        },
        generation: Some(generation),
    };
    let mut scanner = CodexScanner::new(&config_for(&root)).unwrap();
    let step = RefreshBudget {
        max_records: 256,
        max_bytes: 64 * 1024,
        max_millis: 600_000,
    };
    let job = support::world::new_job();
    let first = scanner.source_record(job, &locator, step).unwrap();
    assert!(
        matches!(first, ReadProgress::Pending(_)),
        "indexing is not complete"
    );
    // Another selected source finishes while the archive is still indexing.
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    let key = found
        .sessions
        .iter()
        .find(|m| m.key.native_id == "small")
        .expect("small source")
        .key
        .clone();
    let summary = support::world::hydrate_complete(&mut scanner, &key, NOW);
    assert_eq!(summary.meta.key, key);
    assert!(matches!(
        scanner.source_record(job, &locator, step).unwrap(),
        ReadProgress::Pending(_) | ReadProgress::Ready(_)
    ));
    assert!(
        scanner.job_state() >= 1,
        "the archive decoder is retained for its job"
    );
    scanner.cancel_read(&session, job);
    assert_eq!(scanner.job_state(), 0, "its last consumer released it");
}

#[test]
fn cancelling_one_job_keeps_the_other_and_the_last_release_drops_partial_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let mut lines = vec![session_meta(TS, "k", "/p")];
    lines.extend((0..60).map(|i| assistant(TS, &format!("m{i}"))));
    write_lines(&dated(&root, "rollout-k.jsonl"), &lines);
    let mut scanner = CodexScanner::new(&config_for(&root)).unwrap();
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    let step = RefreshBudget {
        max_records: 5,
        ..BIG
    };
    // Two purposes on the same key.
    let (a, b) = (support::world::new_job(), support::world::new_job());
    assert!(!scanner.hydrate(a, &key, NOW, step).unwrap().done);
    assert!(!scanner.hydrate(b, &key, NOW, step).unwrap().done);
    assert_eq!(scanner.job_state(), 2);
    scanner.cancel_read(&key, a);
    assert_eq!(scanner.job_state(), 1, "only job a was released");
    assert_eq!(
        scanner.cached_sessions(),
        1,
        "job b still owns the partial scan"
    );
    // Job b completes with correct content.
    let mut done = None;
    for _ in 0..100 {
        let delta = scanner.hydrate(b, &key, NOW, step).unwrap();
        if delta.done {
            done = Some(delta.summary);
            break;
        }
    }
    assert_eq!(done.expect("job b finishes").displayed_event_count, 60);
    assert_eq!(scanner.job_state(), 0);

    // A fresh scanner: the last consumer of an abandoned partial scan releases it.
    let mut fresh = CodexScanner::new(&config_for(&root)).unwrap();
    support::world::discover_pass(&mut fresh, NOW, BIG);
    let c = support::world::new_job();
    assert!(!fresh.hydrate(c, &key, NOW, step).unwrap().done);
    assert_eq!(fresh.cached_sessions(), 1);
    fresh.cancel_read(&key, c);
    assert_eq!(
        fresh.cached_sessions(),
        0,
        "abandoned partial state is dropped"
    );
    assert_eq!(fresh.job_state(), 0);
    // A fresh request starts cleanly and returns correct content.
    let again = support::world::hydrate_complete(&mut fresh, &key, NOW);
    assert_eq!(again.displayed_event_count, 60);
}

// ---- OpenCode: resumable counting and paged details ---------------------------

mod opencode_reads {
    use super::*;
    use agentscope::providers::opencode::OpenCodeScanner;
    use support::opencode as oc;

    fn db_with(
        f: impl FnOnce(&rusqlite::Connection),
    ) -> (tempfile::TempDir, OpenCodeScanner, SessionKey) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("opencode.db");
        let conn = oc::create_v2(&db);
        oc::insert_session(&conn, &oc::Session::new("ses_1", 1, 1_000_000));
        f(&conn);
        let mut scanner = OpenCodeScanner::new(&oc::config_for_db(&db)).unwrap();
        let key = support::world::discover_pass(&mut scanner, NOW, BIG)
            .sessions
            .remove(0)
            .key;
        std::mem::forget(conn);
        (dir, scanner, key)
    }

    #[test]
    fn counting_a_large_post_anchor_history_is_bounded_and_resumable() {
        let (_d, mut scanner, key) = db_with(|c| {
            for i in 0..3000 {
                oc::insert_message(c, &format!("m{i:05}"), "ses_1", 1_000 + i, 1_000 + i);
            }
        });
        let page = support::world::transcript_ready(
            &mut scanner,
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 1,
                max_bytes: 100_000,
            },
            BIG,
            support::world::STEPS,
        );
        let anchor = page.next.expect("an anchor");
        let budget = RefreshBudget {
            max_records: 256,
            max_bytes: 8 * 1024 * 1024,
            max_millis: 600_000,
        };
        let job = support::world::new_job();
        let mut pending = 0;
        let count = loop {
            match scanner
                .count_events_after(job, &key, &anchor, budget)
                .unwrap()
            {
                ReadProgress::Pending(p) => {
                    pending += 1;
                    assert!(
                        p.records >= 1 && p.records <= 256,
                        "{} rows in one call",
                        p.records
                    );
                    assert!(p.expanded_bytes <= budget.max_bytes);
                    assert_eq!(scanner.job_state(), 1, "continuation is held for the job");
                }
                ReadProgress::Ready(n) => break n,
            }
            assert!(pending < 100);
        };
        assert!(
            pending >= 10,
            "{pending} pending calls for 2,999 rows at 256 per call"
        );
        assert_eq!(count, 2999);
        assert_eq!(scanner.job_state(), 0);

        // Cancelling mid-way releases only that job's continuation.
        let (a, b) = (support::world::new_job(), support::world::new_job());
        assert!(matches!(
            scanner
                .count_events_after(a, &key, &anchor, budget)
                .unwrap(),
            ReadProgress::Pending(_)
        ));
        assert!(matches!(
            scanner
                .count_events_after(b, &key, &anchor, budget)
                .unwrap(),
            ReadProgress::Pending(_)
        ));
        scanner.cancel_read(&key, a);
        assert_eq!(scanner.job_state(), 1);
        scanner.cancel_read(&key, b);
        assert_eq!(scanner.job_state(), 0);
    }

    #[test]
    fn details_pages_request_observations_within_a_combined_budget() {
        let (_d, mut scanner, key) = db_with(|c| {
            for i in 0..25i64 {
                c.execute(
                    "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
                     VALUES (?1, 'ses_1', 'assistant', 0, ?2, ?2, ?3)",
                    rusqlite::params![
                        format!("a{i:03}"),
                        1_000 + i,
                        serde_json::json!({"role":"assistant","finish":"stop","cost":0.01,
                            "tokens":{"input":10+i,"output":5,"reasoning":0,"cache":{"read":0,"write":0}},
                            "modelID":"m","providerID":"p"}).to_string()
                    ],
                )
                .unwrap();
            }
        });
        let request = PageRequest {
            max_events: 10,
            max_bytes: 1_000_000,
        };
        let first = support::world::details_ready(
            &mut scanner,
            &key,
            None,
            request,
            BIG,
            support::world::STEPS,
        );
        assert_eq!(
            first.requests.len(),
            5,
            "requests share the record budget with recent events"
        );
        assert!(first.requests.len() + first.recent.len() <= 10);
        assert!(first.has_more && first.next.is_some());
        assert!(
            first.compactions.is_empty(),
            "OpenCode records no compaction markers"
        );
        let mut ids: Vec<String> = first
            .requests
            .iter()
            .map(|r| r.request.as_ref().unwrap().id.clone().unwrap())
            .collect();
        assert_eq!(ids[0], "a024", "newest request first");
        let mut next = first.next;
        while let Some(anchor) = next {
            let page = support::world::details_ready(
                &mut scanner,
                &key,
                Some(anchor),
                request,
                BIG,
                support::world::STEPS,
            );
            assert!(page.recent.is_empty() && page.requests.len() <= 10);
            ids.extend(
                page.requests
                    .iter()
                    .map(|r| r.request.as_ref().unwrap().id.clone().unwrap()),
            );
            next = page.next;
        }
        let distinct: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(
            (ids.len(), distinct.len()),
            (25, 25),
            "no request repeated or skipped"
        );
        // A byte budget alone also bounds a page, but always makes progress.
        let tiny = PageRequest {
            max_events: 100,
            max_bytes: 10,
        };
        let p = support::world::details_ready(
            &mut scanner,
            &key,
            None,
            tiny,
            BIG,
            support::world::STEPS,
        );
        assert_eq!(p.requests.len(), 1);
        assert!(p.has_more);
    }
}

#[test]
fn codex_details_page_compactions_and_report_no_fabricated_requests() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let mut lines = vec![session_meta(TS, "cmp", "/p")];
    for i in 0..6 {
        lines.push(support::codex::compacted(&format!(
            "2026-10-29T10:00:0{i}Z"
        )));
        lines.push(assistant(TS, "x"));
    }
    write_lines(&dated(&root, "rollout-cmp.jsonl"), &lines);
    let mut scanner = CodexScanner::new(&config_for(&root)).unwrap();
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    let page = support::world::details_ready(
        &mut scanner,
        &key,
        None,
        PageRequest {
            max_events: 9,
            max_bytes: 1_000_000,
        },
        BIG,
        support::world::STEPS,
    );
    assert!(
        page.requests.is_empty(),
        "Codex records cumulative snapshots, not requests"
    );
    assert_eq!(
        page.compactions.len(),
        3,
        "the newest compactions fit the record budget"
    );
    assert!(page.truncated);
}

#[test]
fn claude_details_report_the_usage_each_paged_message_recorded() {
    use agentscope::providers::claude::ClaudeScanner;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let lines: Vec<String> = (0..6)
        .map(|i| {
            serde_json::json!({"type":"assistant","uuid":format!("u{i}"),"sessionId":"s",
                "timestamp":TS,"message":{"id":format!("msg{i}"),"model":"claude-x",
                "content":[{"type":"text","text":"hi"}],
                "usage":{"input_tokens":10,"output_tokens":4}}})
            .to_string()
        })
        .collect();
    write_lines(&root.join("p/s.jsonl"), &lines);
    let config = support::config_for(&root);
    let mut config = config;
    config.harnesses.claude.root = Some(root);
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    let page = support::world::details_ready(
        &mut scanner,
        &key,
        None,
        PageRequest {
            max_events: 4,
            max_bytes: 1_000_000,
        },
        BIG,
        support::world::STEPS,
    );
    assert_eq!(page.recent.len(), 2);
    assert_eq!(
        page.requests.len(),
        2,
        "one request observation per paged message"
    );
    assert!(page.has_more);
}
