//! The Hermes reader end to end over synthetic `state.db` fixtures: in-place
//! compaction and rotation projection, tombstone rewinds, chain-local hydration,
//! bounded/paged source reads and per-source recovery.

mod support;

use std::path::Path;

use agentscope::domain::{
    CompactionScope, Harness, PageDirection, PageRequest, RefreshBudget, SessionKey, SessionLink,
};
use agentscope::providers::ProviderReader;
use agentscope::providers::hermes::HermesScanner;
use serde_json::json;
use support::hermes as fx;
use support::world::{
    BIG, PAGE, STEPS, discover_pass, hydrate_complete, new_job, transcript_ready,
};

const NOW: i64 = support::NOW;

fn scanner(db: &Path) -> HermesScanner {
    HermesScanner::new(&fx::config_for_db(db)).expect("scanner")
}

fn keys(found: &support::world::Found) -> Vec<SessionKey> {
    found.sessions.iter().map(|meta| meta.key.clone()).collect()
}

#[test]
fn a_compression_chain_is_one_logical_root() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "p", None, json!({}), 1_000.0);
    fx::insert_session(&writer, "p2", Some("p"), json!({}), 1_100.0);
    fx::set_ended(&writer, "p", 1_050.0, "compression");
    fx::insert_message(&writer, "p", "user", "first prompt", 1_000.0);
    fx::insert_message(&writer, "p2", "assistant", "continued answer", 1_100.0);

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.removed.len(), 0);
    assert_eq!(found.sessions.len(), 1, "{:?}", keys(&found));
    let meta = &found.sessions[0];
    assert_eq!(meta.key.native_id, "p");
    assert_eq!(
        meta.members
            .iter()
            .map(|key| key.native_id.as_str())
            .collect::<Vec<_>>(),
        ["p", "p2"]
    );
    assert_eq!(meta.key.harness, Harness::Hermes);

    let summary = hydrate_complete(&mut scanner, &meta.key, NOW);
    assert_eq!(summary.meta.key.native_id, "p");
    assert_eq!(summary.meta.members.len(), 2);

    let page = transcript_ready(
        &mut scanner,
        &meta.key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let ids: Vec<&str> = page.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["hermes:r1", "hermes:r2"]);
    assert_eq!(
        page.events[0].text.as_ref().unwrap().preview,
        "first prompt"
    );
    assert_eq!(
        page.events[1].text.as_ref().unwrap().preview,
        "continued answer"
    );
}

#[test]
fn in_place_compaction_keeps_summarized_rows_and_collapses_copies() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);

    // Old summarized history: archived as compacted=1, still real history.
    let old_user = fx::insert_message(&writer, "s", "user", "old question", 1_001.0);
    let old_answer = fx::insert_message(&writer, "s", "assistant", "old answer", 1_002.0);
    fx::set_message_display(&writer, old_user, 1, b"identity-u", "uid-u");
    fx::set_message_display(&writer, old_answer, 2, b"identity-a", "uid-a");
    // The protected tail was copied into the new generation and the original was
    // rewound out of display (active=0, compacted=0).
    let tail_original = fx::insert_message(&writer, "s", "user", "tail prompt", 1_003.0);
    fx::set_message_display(&writer, tail_original, 3, b"identity-t", "uid-t");
    let tail_copy = fx::insert_message(&writer, "s", "user", "tail prompt", 1_003.0);
    fx::set_message_display(&writer, tail_copy, 3, b"identity-t", "uid-t");
    // A compaction summary carrier for the summarized conversation.
    let carrier = fx::insert_message(&writer, "s", "user", "summary of the prior turns", 1_004.0);
    fx::set_message_summary(&writer, carrier, 1);
    fx::set_message_display(&writer, carrier, 0, b"identity-s", "uid-s");
    // New work after the compaction.
    let new_answer = fx::insert_message(&writer, "s", "assistant", "new answer", 1_005.0);
    fx::set_message_display(&writer, new_answer, 4, b"identity-n", "uid-n");

    fx::set_message_state(&writer, old_user, 0, 1);
    fx::set_message_state(&writer, old_answer, 0, 1);
    fx::set_message_state(&writer, tail_original, 0, 0);

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let page = transcript_ready(
        &mut scanner,
        &key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let ids: Vec<&str> = page.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids.len(), 5);
    assert!(ids.iter().all(|id| id.starts_with("hermes:h")), "{ids:?}");
    let _ = (old_user, old_answer, tail_copy, new_answer);
    assert_eq!(
        page.events
            .iter()
            .filter(|e| e.text.as_ref().is_some_and(|t| t.preview == "tail prompt"))
            .count(),
        1
    );
    assert!(page.events.iter().any(|e| {
        e.text
            .as_ref()
            .is_some_and(|t| t.preview.contains("summary of"))
    }));

    let job = new_job();
    let details = match scanner
        .details_page(
            job,
            &key,
            None,
            PageRequest {
                max_events: 100,
                max_bytes: 1_000_000,
            },
            BIG,
        )
        .unwrap()
    {
        agentscope::providers::ReadProgress::Ready(page) => page,
        agentscope::providers::ReadProgress::Pending(_) => panic!("details pending"),
    };
    let summary = details
        .compactions
        .iter()
        .find(|c| c.id == format!("hermes:summary:{carrier}"))
        .expect("the summary carrier is an observed compaction");
    assert_eq!(summary.scope, CompactionScope::Event);
    assert!(summary.usage.is_none());
    assert!(summary.before_tokens.is_none());
    assert!(summary.after_tokens.is_none());
    assert!(summary.duration_ms.is_none());
}

fn all_forward(scanner: &mut HermesScanner, key: &SessionKey) -> Vec<agentscope::domain::Event> {
    let mut events = Vec::new();
    let mut anchor = None;
    loop {
        let page = transcript_ready(
            scanner,
            key,
            anchor.clone(),
            PageDirection::Forward,
            PageRequest {
                max_events: 2,
                max_bytes: 1_000_000,
            },
            BIG,
            STEPS,
        );
        let more = page.has_more;
        anchor = page.next.clone();
        events.extend(page.events);
        if !more {
            return events;
        }
    }
}

#[test]
fn a_rewind_tombstone_never_resurrects_the_archived_parent_copy() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "p", None, json!({}), 1_000.0);
    fx::insert_session(&writer, "p2", Some("p"), json!({}), 1_100.0);
    fx::set_ended(&writer, "p", 1_050.0, "compression");

    // The parent keeps an archived copy of a carried message (identity X) and an
    // unrelated archived identity Y.
    let archived_x = fx::insert_message(&writer, "p", "user", "carried X", 1_001.0);
    fx::set_message_display(&writer, archived_x, 1, b"identity-x", "uid-x");
    fx::set_message_state(&writer, archived_x, 0, 1);
    let archived_y = fx::insert_message(&writer, "p", "assistant", "unrelated Y", 1_002.0);
    fx::set_message_display(&writer, archived_y, 2, b"identity-y", "uid-y");
    fx::set_message_state(&writer, archived_y, 0, 1);
    // The child carries X, then rewinds it: the later child copy is active=0,
    // compacted=0, so the archived parent copy must not come back.
    let child_x = fx::insert_message(&writer, "p2", "user", "carried X", 1_100.0);
    fx::set_message_display(&writer, child_x, 1, b"identity-x", "uid-x");
    let rewound = fx::insert_message(&writer, "p2", "user", "carried X", 1_100.0);
    fx::set_message_display(&writer, rewound, 1, b"identity-x", "uid-x");
    fx::set_message_state(&writer, rewound, 0, 0);

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let events = all_forward(&mut scanner, &key);
    let texts: Vec<&str> = events
        .iter()
        .map(|event| event.text.as_ref().map_or("", |text| text.preview.as_str()))
        .collect();
    assert_eq!(texts, ["unrelated Y"]);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].provenance.locator.session.native_id, "p",
        "the archived parent row is the visible source"
    );

    let anchor = agentscope::domain::PageAnchor {
        timestamp: events[0].timestamp,
        source_order: events[0].source_order,
        stable_id: events[0].id.clone(),
    };
    let after = support::world::try_count(&mut scanner, &key, &anchor, BIG, STEPS).unwrap();
    assert_eq!(after, 0, "no event is invented after the only visible row");
}

#[test]
fn a_regressing_timestamp_cannot_reorder_native_display_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    let first = fx::insert_message(&writer, "s", "user", "first by display order", 1_500.0);
    fx::set_message_display(&writer, first, 1, b"id-1", "uid-1");
    let second = fx::insert_message(
        &writer,
        "s",
        "assistant",
        "second by display order",
        1_100.0,
    );
    fx::set_message_display(&writer, second, 2, b"id-2", "uid-2");

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let page = transcript_ready(
        &mut scanner,
        &key,
        None,
        PageDirection::Backward,
        PageRequest {
            max_events: 1,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    assert_eq!(
        page.events[0].text.as_ref().unwrap().preview,
        "second by display order"
    );
    assert!(page.has_more);
    let next = page.next.expect("an anchor");
    let older = transcript_ready(
        &mut scanner,
        &key,
        Some(next),
        PageDirection::Backward,
        PageRequest {
            max_events: 1,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    assert_eq!(
        older.events[0].text.as_ref().unwrap().preview,
        "first by display order"
    );
}

#[test]
fn a_compressed_delegate_keeps_member_sources_and_rotation_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "p", None, json!({}), 1_000.0);
    fx::insert_session(&writer, "p2", Some("p"), json!({}), 1_100.0);
    fx::set_ended(&writer, "p", 1_050.0, "compression");
    // The child delegates from the parent's continuation segment and inherits the
    // marker when it is itself compressed into c2.
    fx::insert_session(
        &writer,
        "c",
        Some("p2"),
        json!({"_delegate_from": "p2"}),
        1_200.0,
    );
    fx::insert_session(
        &writer,
        "c2",
        Some("c"),
        json!({"_delegate_from": "p2"}),
        1_300.0,
    );
    fx::set_ended(&writer, "c", 1_250.0, "compression");
    let parent_event = fx::insert_message(&writer, "p", "user", "parent prompt", 1_000.0);
    let child_event = fx::insert_message(&writer, "c2", "assistant", "child answer", 1_300.0);
    fx::set_message_display(&writer, parent_event, 1, b"pl", "pl");
    fx::set_message_display(&writer, child_event, 1, b"cl", "cl");

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let parent = found
        .sessions
        .iter()
        .find(|meta| meta.key.native_id == "p")
        .expect("parent root");
    let child = found
        .sessions
        .iter()
        .find(|meta| meta.key.native_id == "c")
        .expect("child root");
    assert_eq!(
        parent
            .members
            .iter()
            .map(|key| key.native_id.as_str())
            .collect::<Vec<_>>(),
        ["p", "p2"]
    );
    assert_eq!(
        child
            .members
            .iter()
            .map(|key| key.native_id.as_str())
            .collect::<Vec<_>>(),
        ["c", "c2"]
    );
    assert_eq!(
        child.lineage.parent,
        Some(SessionLink::Resolved(parent.key.clone()))
    );

    let events = all_forward(&mut scanner, &child.key);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].text.as_ref().unwrap().preview, "child answer");
    assert_eq!(events[0].provenance.locator.session.native_id, "c2");
    let record =
        support::world::try_source(&mut scanner, &events[0].provenance.locator, BIG, STEPS)
            .unwrap();
    assert!(record.raw.contains("\"session_id\""), "{}", record.raw);
    assert!(record.raw.contains("\"c2\""), "{}", record.raw);

    let job = new_job();
    let details = match scanner
        .details_page(
            job,
            &child.key,
            None,
            PageRequest {
                max_events: 100,
                max_bytes: 1_000_000,
            },
            BIG,
        )
        .unwrap()
    {
        agentscope::providers::ReadProgress::Ready(page) => page,
        agentscope::providers::ReadProgress::Pending(_) => panic!("details pending"),
    };
    assert!(
        details
            .compactions
            .iter()
            .any(|c| c.id == "hermes:rotation:c" && c.scope == CompactionScope::Event)
    );
    let parent_details = {
        let job = new_job();
        match scanner
            .details_page(
                job,
                &parent.key,
                None,
                PageRequest {
                    max_events: 100,
                    max_bytes: 1_000_000,
                },
                BIG,
            )
            .unwrap()
        {
            agentscope::providers::ReadProgress::Ready(page) => page,
            agentscope::providers::ReadProgress::Pending(_) => panic!("details pending"),
        }
    };
    assert!(
        parent_details
            .compactions
            .iter()
            .any(|c| c.id == "hermes:rotation:p")
    );
}

#[test]
fn compaction_carriers_count_once_and_aggregate_cost_is_not_split() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    let first = fx::insert_message(&writer, "s", "user", "summary one", 1_001.0);
    fx::set_message_summary(&writer, first, 1);
    fx::set_message_display(&writer, first, 0, b"summary-one", "summary-one");
    fx::set_message_state(&writer, first, 0, 1);
    let duplicate = fx::insert_message(&writer, "s", "user", "summary one", 1_001.0);
    fx::set_message_summary(&writer, duplicate, 1);
    fx::set_message_display(&writer, duplicate, 0, b"summary-one", "summary-one");
    // The same carrier identity with mixed aliases: (H,no UID), (H,UID U),
    // (no hash,UID U) must all be one compaction.
    let hash_only = fx::insert_message(&writer, "s", "user", "summary one", 1_001.0);
    fx::set_message_summary(&writer, hash_only, 1);
    fx::set_message_display(&writer, hash_only, 0, b"summary-one", "");
    let uid_only = fx::insert_message(&writer, "s", "user", "summary one", 1_001.0);
    fx::set_message_summary(&writer, uid_only, 1);
    fx::set_message_display(&writer, uid_only, 0, b"", "summary-one");
    // Arbitrary archived text is not a compaction event.
    let ordinary = fx::insert_message(&writer, "s", "assistant", "old archived answer", 1_002.0);
    fx::set_message_display(&writer, ordinary, 1, b"ordinary", "ordinary");
    fx::set_message_state(&writer, ordinary, 0, 1);
    fx::insert_usage(
        &writer,
        "s",
        "test-model",
        "test-provider",
        "compression",
        [10, 5, 0, 0, 0],
        0.02,
        "actual",
    );
    fx::set_usage_usage(
        &writer,
        "s",
        "test-model",
        "compression",
        1,
        0.02,
        0.02,
        Some("actual"),
    );
    fx::set_session_usage(&writer, "s", [0, 0, 0, 0, 0], 0, 0.0, 0.0, None);

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let summary = hydrate_complete(&mut scanner, &key, NOW);
    let compactions = summary.metrics.compactions;
    assert_eq!(compactions.observed, 1, "the carrier is counted once");
    assert_eq!(compactions.cost_scope, CompactionScope::Aggregate);
    assert_eq!(compactions.known_cost_usd, Some(0.02));
    assert!(compactions.history_complete);
    let latest = compactions.latest.expect("the carrier is inspectable");
    assert!(latest.usage.is_none());
    assert!(latest.before_tokens.is_none());
    assert!(latest.after_tokens.is_none());
    assert!(latest.duration_ms.is_none());

    // The known compression spend stays in the session totals once.
    let usage = summary.usage.expect("usage");
    assert_eq!(usage.reported_cost_usd, Some(0.02));

    let job = new_job();
    let details = match scanner
        .details_page(
            job,
            &key,
            None,
            PageRequest {
                max_events: 100,
                max_bytes: 1_000_000,
            },
            BIG,
        )
        .unwrap()
    {
        agentscope::providers::ReadProgress::Ready(page) => page,
        agentscope::providers::ReadProgress::Pending(_) => panic!("details pending"),
    };
    let aggregate: Vec<_> = details
        .compactions
        .iter()
        .filter(|c| c.scope == CompactionScope::Aggregate)
        .collect();
    assert_eq!(aggregate.len(), 1, "the ledger row is one aggregate record");
    assert!(aggregate[0].usage.is_some());
    assert!(
        details
            .compactions
            .iter()
            .filter(|c| c.scope == CompactionScope::Event)
            .count()
            == 1,
        "the duplicate carrier and archived text add no events"
    );
}

#[test]
fn hostile_and_oversized_cells_stay_bounded_and_do_not_hide_rows() {
    use support::world::INLINE_SECRET;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    let huge = format!("{} {} \u{1b}[2J", fx::SECRET, "lorem ipsum ".repeat(20_000));
    let huge_event = fx::insert_message(&writer, "s", "assistant", &huge, 1_001.0);
    fx::set_message_display(&writer, huge_event, 1, b"huge", "huge");
    let tool_json = json!([{
        "id": "call-1",
        "type": "function",
        "function": {"name": "read_file", "arguments": "x".repeat(100_000)}
    }])
    .to_string();
    let tool_event = fx::insert_message(&writer, "s", "assistant", "", 1_002.0);
    fx::set_message_tool_calls(&writer, tool_event, &tool_json);
    fx::set_message_display(&writer, tool_event, 2, b"tool", "tool");
    let hostile = fx::insert_message(&writer, "s", "user", "hostile metadata", 1_003.0);
    fx::set_message_display(&writer, hostile, 3, b"hostile", "hostile");
    fx::set_message_display_metadata(
        &writer,
        hostile,
        Some(&format!(
            "{{\"model_only\": false, \"note\": \"{}\"}}",
            "z".repeat(200_000)
        )),
    );
    let invalid = fx::insert_message(
        &writer,
        "s",
        "user",
        "invalid metadata stays visible",
        1_004.0,
    );
    fx::set_message_display(&writer, invalid, 4, b"invalid", "invalid");
    fx::set_message_display_metadata(&writer, invalid, Some("{not-json"));
    let model_only =
        fx::insert_message(&writer, "s", "assistant", "model-only scaffolding", 1_005.0);
    fx::set_message_display(&writer, model_only, 5, b"model-only", "model-only");
    fx::set_message_display_metadata(&writer, model_only, Some("{\"model_only\": true}"));

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let mut events = Vec::new();
    let mut anchor = None;
    loop {
        let page = transcript_ready(
            &mut scanner,
            &key,
            anchor.clone(),
            PageDirection::Forward,
            PageRequest {
                max_events: 2,
                max_bytes: 4096,
            },
            BIG,
            STEPS,
        );
        let more = page.has_more;
        anchor = page.next.clone();
        events.extend(page.events);
        if !more {
            break;
        }
    }
    let page = agentscope::domain::TranscriptPage {
        events,
        diagnostics: Vec::new(),
        next: None,
        has_more: false,
        truncated: true,
    };
    let texts: Vec<&str> = page
        .events
        .iter()
        .map(|event| event.text.as_ref().map_or("", |text| text.preview.as_str()))
        .collect();
    assert!(!texts.iter().any(|text| text.contains("model-only")));
    assert!(texts.iter().any(|text| text.contains("invalid metadata")));
    assert!(texts.iter().any(|text| text.contains("hostile metadata")));
    assert!(!texts.iter().any(|text| text.contains(fx::SECRET)));
    assert!(!page.events.iter().any(|event| {
        event
            .text
            .as_ref()
            .is_some_and(|text| text.preview.contains('\u{1b}'))
    }));
    for event in &page.events {
        if let Some(text) = &event.text {
            assert!(text.preview.len() <= 4096);
        }
        if let Some(tool) = &event.tool {
            assert!(
                tool.input
                    .as_ref()
                    .is_none_or(|input| input.preview.len() <= 4096)
            );
        }
    }
    assert!(page.truncated);
    assert!(
        page.events
            .iter()
            .filter_map(|event| event.text.as_ref())
            .any(|text| text.truncated && text.byte_len > text.preview.len())
    );
    assert!(
        page.events.iter().all(|event| event
            .text
            .as_ref()
            .is_none_or(|text| !text.preview.contains(INLINE_SECRET))),
        "the credential sentinel never reaches a preview"
    );
}

#[test]
fn a_reduced_schema_still_projects_every_message() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, false);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    fx::insert_message(&writer, "s", "user", "legacy prompt", 1_001.0);
    fx::insert_message(&writer, "s", "assistant", "legacy answer", 1_002.0);

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), 1);
    let key = found.sessions[0].key.clone();
    let events = all_forward(&mut scanner, &key);
    assert_eq!(events.len(), 2);
    let summary = hydrate_complete(&mut scanner, &key, NOW);
    assert!(!summary.metrics.compactions.history_complete);
}

#[test]
fn a_locked_profile_yields_an_error_not_an_empty_transcript_then_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_rollback_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    fx::insert_message(&writer, "s", "user", "locked prompt", 1_001.0);

    let mut config = fx::config_for_db(&db);
    config.refresh_busy_timeout_ms = 25;
    config.page_busy_timeout_ms = 25;
    let mut scanner = HermesScanner::new(&config).unwrap();
    // Learn the real published key before the database becomes unavailable.
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), 1);
    let key = found.sessions[0].key.clone();

    writer.execute_batch("BEGIN EXCLUSIVE").expect("lock");
    let locked = discover_pass(&mut scanner, NOW, BIG);
    assert!(locked.sessions.is_empty());
    assert!(locked.removed.is_empty(), "last-good metadata is retained");
    assert!(!locked.warnings.is_empty());
    assert_eq!(scanner.published_sessions(), 1);

    let job = new_job();
    let error = scanner
        .transcript_page(job, &key, None, PageDirection::Backward, PAGE, BIG)
        .expect_err("a locked database is not an empty transcript");
    assert!(
        matches!(error, agentscope::providers::ProviderError::Unavailable(_)),
        "{error:?}"
    );
    scanner.cancel_read(&key, job);

    writer.execute_batch("ROLLBACK").expect("release lock");
    let recovered = discover_pass(&mut scanner, NOW, BIG);
    assert!(recovered.removed.is_empty());
    let page = transcript_ready(
        &mut scanner,
        &key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(page.events.len(), 1);
    assert_eq!(
        page.events[0].text.as_ref().unwrap().preview,
        "locked prompt"
    );
}

#[test]
fn appends_and_a_replaced_source_are_visible_but_a_stale_page_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    fx::insert_message(&writer, "s", "user", "first", 1_001.0);

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let first = transcript_ready(
        &mut scanner,
        &key,
        None,
        PageDirection::Backward,
        PageRequest {
            max_events: 10,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    assert_eq!(first.events.len(), 1);

    // A concurrent append is picked up on the next read.
    fx::insert_message(&writer, "s", "assistant", "second", 1_002.0);
    let grown = transcript_ready(
        &mut scanner,
        &key,
        None,
        PageDirection::Backward,
        PageRequest {
            max_events: 10,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    assert_eq!(grown.events.len(), 2);
    assert_eq!(grown.events[1].text.as_ref().unwrap().preview, "second");

    // A page continued after the producer migrated the schema in place is
    // rejected, not silently served from the old generation.
    let job = new_job();
    let first_page = match scanner
        .transcript_page(
            job,
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 1,
                max_bytes: 1_000_000,
            },
            BIG,
        )
        .unwrap()
    {
        agentscope::providers::ReadProgress::Ready(page) => page,
        agentscope::providers::ReadProgress::Pending(_) => panic!("pending"),
    };
    assert!(first_page.has_more);
    let anchor = first_page.next.expect("a continuation anchor");
    writer
        .execute_batch(
            "ALTER TABLE messages RENAME COLUMN display_identity TO display_identity_gone",
        )
        .expect("rename a probed display column");
    let continuation_job = new_job();
    let stale = scanner.transcript_page(
        continuation_job,
        &key,
        Some(anchor),
        PageDirection::Forward,
        PageRequest {
            max_events: 1,
            max_bytes: 1_000,
        },
        BIG,
    );
    match stale {
        Err(error) => assert!(
            error.to_string().contains("changed") || error.to_string().contains("stale"),
            "{error}"
        ),
        Ok(agentscope::providers::ReadProgress::Ready(page)) => {
            assert!(
                page.events.is_empty(),
                "the old generation is not repainted"
            )
        }
        Ok(agentscope::providers::ReadProgress::Pending(_)) => {}
    }
}

#[test]
fn cancellation_clears_partial_projection_and_a_small_source_is_responsive() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    for at in 0..40 {
        fx::insert_message(
            &writer,
            "s",
            "user",
            &format!("turn {at}"),
            1_000.0 + at as f64,
        );
    }

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();

    let tiny = RefreshBudget {
        max_records: 1,
        max_bytes: 1024,
        max_millis: 60_000,
    };
    let job = new_job();
    let first = scanner
        .transcript_page(job, &key, None, PageDirection::Backward, PAGE, tiny)
        .unwrap();
    assert!(matches!(
        first,
        agentscope::providers::ReadProgress::Pending(_)
    ));
    assert!(scanner.job_state() > 0);
    scanner.cancel_read(&key, job);
    assert_eq!(scanner.job_state(), 0);

    let (page, pending) = support::world::transcript_steps(
        &mut scanner,
        &key,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(pending, 0, "a small source opens in one call");
    assert_eq!(page.events.len(), 40);
    assert_eq!(page.events[39].text.as_ref().unwrap().preview, "turn 39");
}

#[test]
fn a_carried_row_replacement_keeps_the_anchor_identity() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    let carried = fx::insert_message(&writer, "s", "user", "carried", 1_001.0);
    fx::set_message_display(&writer, carried, 1, b"carried-identity", "carried-uid");
    let after = fx::insert_message(&writer, "s", "assistant", "after", 1_002.0);
    fx::set_message_display(&writer, after, 2, b"after-identity", "after-uid");

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let events = all_forward(&mut scanner, &key);
    assert_eq!(events.len(), 2);
    let anchor = agentscope::domain::PageAnchor {
        timestamp: events[0].timestamp,
        source_order: events[0].source_order,
        stable_id: events[0].id.clone(),
    };
    assert_eq!(
        support::world::try_count(&mut scanner, &key, &anchor, BIG, STEPS).unwrap(),
        1
    );

    // In-place compaction removes the earliest physical copy and inserts a new
    // active copy with the same proven identity. The logical anchor survives.
    fx::set_message_state(&writer, carried, 0, 0);
    let replacement = fx::insert_message(&writer, "s", "user", "carried", 1_001.0);
    fx::set_message_display(&writer, replacement, 1, b"carried-identity", "carried-uid");

    let count = support::world::try_count(&mut scanner, &key, &anchor, BIG, STEPS).unwrap();
    assert_eq!(
        count, 1,
        "the same logical event still precedes one real event"
    );
    let page = transcript_ready(
        &mut scanner,
        &key,
        Some(anchor),
        PageDirection::Forward,
        PAGE,
        BIG,
        STEPS,
    );
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].text.as_ref().unwrap().preview, "after");
}

#[test]
fn a_carried_child_copy_names_the_member_that_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "p", None, json!({}), 1_000.0);
    fx::insert_session(&writer, "p2", Some("p"), json!({}), 1_100.0);
    fx::set_ended(&writer, "p", 1_050.0, "compression");
    let parent_copy = fx::insert_message(&writer, "p", "user", "carried", 1_001.0);
    fx::set_message_display(&writer, parent_copy, 1, b"carried-id", "carried-uid");
    fx::set_message_state(&writer, parent_copy, 0, 1);
    let child_copy = fx::insert_message(&writer, "p2", "user", "carried", 1_100.0);
    fx::set_message_display(&writer, child_copy, 1, b"carried-id", "carried-uid");

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let events = all_forward(&mut scanner, &key);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].provenance.locator.session.native_id, "p2",
        "the representative, not the earliest display member, is the source"
    );
    let record =
        support::world::try_source(&mut scanner, &events[0].provenance.locator, BIG, STEPS)
            .unwrap();
    assert!(record.raw.contains("\"p2\""), "{}", record.raw);
}

#[test]
fn a_carrier_without_a_proven_identity_marks_history_incomplete() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    let carrier = fx::insert_message(&writer, "s", "user", "summary without identity", 1_001.0);
    fx::set_message_summary(&writer, carrier, 1);

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let summary = hydrate_complete(&mut scanner, &key, NOW);
    assert_eq!(summary.metrics.compactions.observed, 1);
    assert!(
        !summary.metrics.compactions.history_complete,
        "an identity-free carrier cannot be deduplicated"
    );
}

#[test]
fn an_embedded_json_credential_is_redacted_before_the_source_view_serializes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    fx::insert_message(
        &writer,
        "s",
        "assistant",
        &format!("{{\"api_key\":\"{}\",\"note\":\"hi\"}}", fx::SECRET),
        1_001.0,
    );

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let events = all_forward(&mut scanner, &key);
    let record =
        support::world::try_source(&mut scanner, &events[0].provenance.locator, BIG, STEPS)
            .unwrap();
    assert!(
        !record.display.contains(fx::SECRET),
        "the default source view must redact embedded JSON values: {}",
        record.display
    );
    assert!(record.display.contains("api_key"), "{}", record.display);
}

#[test]
fn a_large_compression_ledger_is_enumerated_in_bounded_slices() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    for at in 0..300 {
        fx::insert_usage(
            &writer,
            "s",
            &format!("model-{at:03}"),
            "test-provider",
            "compression",
            [1, 1, 0, 0, 0],
            0.001,
            "actual",
        );
        fx::set_usage_usage(
            &writer,
            "s",
            &format!("model-{at:03}"),
            "compression",
            1,
            0.001,
            0.001,
            Some("actual"),
        );
    }
    fx::set_session_usage(&writer, "s", [0, 0, 0, 0, 0], 0, 0.0, 0.0, None);

    let mut scanner = scanner(&db);
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();
    let budget = RefreshBudget {
        max_records: 5,
        max_bytes: 1_000_000,
        max_millis: 60_000,
    };
    let (summary, steps) = support::world::hydrate_steps(&mut scanner, &key, NOW, budget, STEPS);
    assert!(steps > 1, "300 ledger rows cannot fit a five-row slice");
    assert_eq!(
        summary.metrics.compactions.cost_scope,
        CompactionScope::Aggregate
    );
    let cost = summary
        .metrics
        .compactions
        .known_cost_usd
        .unwrap_or_default();
    assert!((cost - 0.3).abs() < 0.001, "{cost}");

    let job = new_job();
    let details = match scanner
        .details_page(
            job,
            &key,
            None,
            PageRequest {
                max_events: 10,
                max_bytes: 100_000,
            },
            BIG,
        )
        .unwrap()
    {
        agentscope::providers::ReadProgress::Ready(page) => page,
        agentscope::providers::ReadProgress::Pending(_) => panic!("details pending"),
    };
    assert!(
        details.truncated,
        "the 300-row ledger is capped in one details page"
    );
}

#[test]
fn a_locked_database_scan_respects_the_slice_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let writer = fx::create_rollback_db(&db, true);
    fx::insert_session(&writer, "s", None, json!({}), 1_000.0);
    fx::insert_message(&writer, "s", "user", "prompt", 1_001.0);
    for at in 0..5 {
        fx::insert_usage(
            &writer,
            "s",
            &format!("model-{at}"),
            "test-provider",
            "compression",
            [1, 1, 0, 0, 0],
            0.001,
            "actual",
        );
    }
    let mut config = fx::config_for_db(&db);
    config.refresh_busy_timeout_ms = 25;
    config.page_busy_timeout_ms = 25;
    let mut scanner = HermesScanner::new(&config).unwrap();
    let found = discover_pass(&mut scanner, NOW, BIG);
    let key = found.sessions[0].key.clone();

    writer.execute_batch("BEGIN EXCLUSIVE").expect("lock");
    let started = std::time::Instant::now();
    let job = new_job();
    let result = scanner.hydrate(
        job,
        &key,
        NOW,
        RefreshBudget {
            max_records: 64,
            max_bytes: 1_000_000,
            max_millis: 20,
        },
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "the lock wait is bounded by the slice ({elapsed:?})"
    );
    match result {
        Ok(delta) => assert!(!delta.done, "a locked scan is not a completed hydration"),
        Err(error) => assert!(
            matches!(error, agentscope::providers::ProviderError::Unavailable(_)),
            "{error:?}"
        ),
    }
    scanner.cancel_read(&key, job);
    writer.execute_batch("ROLLBACK").expect("release");
}

#[test]
fn a_continuation_survives_database_cache_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("hermes");
    let db = home.join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "main", None, json!({}), 1_000.0);
    for at in 0..3 {
        fx::insert_message(
            &writer,
            "main",
            "user",
            &format!("main {at}"),
            1_000.0 + at as f64,
        );
    }
    for profile in 0..9 {
        let path = home
            .join("profiles")
            .join(format!("p{profile}"))
            .join("state.db");
        let profile_writer = fx::create_db(&path, true);
        fx::insert_session(
            &profile_writer,
            "p",
            None,
            json!({}),
            2_000.0 + profile as f64,
        );
        fx::insert_message(
            &profile_writer,
            "p",
            "user",
            &format!("profile {profile}"),
            2_001.0,
        );
    }

    let config = fx::config_for_home(&home);
    let mut scanner = HermesScanner::new(&config).unwrap();
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), 10);
    let main = found
        .sessions
        .iter()
        .find(|meta| meta.key.native_id == "main")
        .expect("main session")
        .key
        .clone();
    let first = transcript_ready(
        &mut scanner,
        &main,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 1,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    assert!(first.has_more);
    let anchor = first.next.expect("a continuation anchor");

    // Touch every other database so the eight-handle cache evicts `main`.
    for meta in found.sessions.iter().filter(|meta| meta.key != main) {
        let _ = transcript_ready(
            &mut scanner,
            &meta.key,
            None,
            PageDirection::Backward,
            PAGE,
            BIG,
            STEPS,
        );
    }

    let next = transcript_ready(
        &mut scanner,
        &main,
        Some(anchor),
        PageDirection::Forward,
        PageRequest {
            max_events: 1,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    assert_eq!(next.events.len(), 1);
    assert_eq!(next.events[0].text.as_ref().unwrap().preview, "main 1");
}

#[test]
fn a_schema_migration_and_eviction_preserve_the_post_migration_generation() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("hermes");
    let db = home.join("state.db");
    let writer = fx::create_db(&db, true);
    fx::insert_session(&writer, "main", None, json!({}), 1_000.0);
    for at in 0..3 {
        let message = fx::insert_message(
            &writer,
            "main",
            "user",
            &format!("main {at}"),
            1_000.0 + at as f64,
        );
        fx::set_message_display(
            &writer,
            message,
            at as i64 + 1,
            format!("identity-{at}").as_bytes(),
            &format!("uid-{at}"),
        );
    }
    for profile in 0..9 {
        let path = home
            .join("profiles")
            .join(format!("p{profile}"))
            .join("state.db");
        let profile_writer = fx::create_db(&path, true);
        fx::insert_session(
            &profile_writer,
            "p",
            None,
            json!({}),
            2_000.0 + profile as f64,
        );
        fx::insert_message(
            &profile_writer,
            "p",
            "user",
            &format!("profile {profile}"),
            2_001.0,
        );
    }

    let config = fx::config_for_home(&home);
    let mut scanner = HermesScanner::new(&config).unwrap();
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), 10);
    let main = found
        .sessions
        .iter()
        .find(|meta| meta.key.native_id == "main")
        .expect("main session")
        .key
        .clone();

    // An initial page caches the pre-migration generation.
    let _ = transcript_ready(
        &mut scanner,
        &main,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 1,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );

    // In-place schema migration: the first page after it detects the new shape
    // and records a new authoritative generation.
    writer
        .execute_batch(
            "ALTER TABLE messages RENAME COLUMN display_identity TO display_identity_old",
        )
        .expect("migrate schema");
    let migrated = transcript_ready(
        &mut scanner,
        &main,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 1,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    assert!(migrated.has_more);
    let locator = migrated.events[0].provenance.locator.clone();

    // Evict the main database from the eight-handle cache.
    for meta in found.sessions.iter().filter(|meta| meta.key != main) {
        let _ = transcript_ready(
            &mut scanner,
            &meta.key,
            None,
            PageDirection::Backward,
            PAGE,
            BIG,
            STEPS,
        );
    }

    let continues = transcript_ready(
        &mut scanner,
        &main,
        migrated.next,
        PageDirection::Forward,
        PageRequest {
            max_events: 10,
            max_bytes: 1_000_000,
        },
        BIG,
        STEPS,
    );
    assert_eq!(
        continues.events.len(),
        2,
        "the post-migration anchor survives eviction"
    );
    assert_eq!(continues.events[0].text.as_ref().unwrap().preview, "main 1");
    assert_eq!(continues.events[1].text.as_ref().unwrap().preview, "main 2");

    // A source locator issued after the migration is readable across eviction.
    let record = support::world::try_source(&mut scanner, &locator, BIG, STEPS)
        .expect("the locator generation survives eviction");
    assert!(record.raw.contains("\"main\""), "{}", record.raw);
}
