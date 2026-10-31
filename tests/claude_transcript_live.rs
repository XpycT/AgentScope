mod support;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{PageDirection, PageRequest, RefreshBudget, SessionKey};
use agentscope::providers::claude::ClaudeScanner;
use serde_json::{Value, json};
use std::io::Write;
use support::{NOW, write_lines};

const BUDGET: RefreshBudget = RefreshBudget {
    max_records: 1000,
    max_bytes: 1_000_000,
    max_millis: 60_000,
};
const PAGE: PageRequest = PageRequest {
    max_events: 1,
    max_bytes: 4096,
};
fn setup(
    records: &[Value],
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    ClaudeScanner,
    SessionKey,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let path = root.join("p/s.jsonl");
    write_lines(
        &path,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let key = support::world::rescan(&mut scanner, NOW, BUDGET)
        .sessions
        .remove(0)
        .meta
        .key;
    (dir, path, scanner, key)
}
fn assistant(uuid: &str, content: Value) -> Value {
    json!({"type":"assistant","uuid":uuid,"sessionId":"s","message":{"id":uuid,"content":content,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2}}})
}
#[test]
fn forward_and_backward_pages_are_equivalent_and_never_split_tool_pairs() {
    let records = [
        assistant(
            "a",
            json!([{"type":"text","text":"hello"},{"type":"tool_use","id":"c","name":"Read","input":{}}]),
        ),
        json!({"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"c","content":"done"}]}}),
        assistant("c", json!("last")),
    ];
    let (_dir, _, mut s, key) = setup(&records);
    let mut forward = Vec::new();
    let mut anchor = None;
    loop {
        let p = support::world::try_transcript(
            &mut s,
            &key,
            anchor,
            PageDirection::Forward,
            PAGE,
            support::world::BIG,
            support::world::STEPS,
        )
        .unwrap();
        forward.extend(p.events);
        if !p.has_more {
            break;
        }
        anchor = p.next;
    }
    let mut backward = Vec::new();
    let mut anchor = None;
    loop {
        let p = support::world::try_transcript(
            &mut s,
            &key,
            anchor,
            PageDirection::Backward,
            PAGE,
            support::world::BIG,
            support::world::STEPS,
        )
        .unwrap();
        backward.splice(0..0, p.events);
        if !p.has_more {
            break;
        }
        anchor = p.next;
    }
    assert_eq!(forward, backward);
    assert_eq!(forward.len(), 3);
    assert_eq!(
        forward[1]
            .tool
            .as_ref()
            .unwrap()
            .output
            .as_ref()
            .unwrap()
            .preview,
        "done"
    );
    let first = agentscope::domain::PageAnchor {
        timestamp: forward[0].timestamp,
        source_order: forward[0].source_order,
        stable_id: forward[0].id.clone(),
    };
    assert_eq!(
        support::world::try_count(
            &mut s,
            &key,
            &first,
            support::world::BIG,
            support::world::STEPS
        )
        .unwrap(),
        2
    );
}
#[test]
fn source_locator_round_trip_and_foreign_locator_rejection() {
    let (_dir, _, mut s, key) = setup(&[assistant("a", json!("hello"))]);
    let p = support::world::try_transcript(
        &mut s,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap();
    let loc = p.events[0].provenance.locator.clone();
    assert!(
        support::world::try_source(&mut s, &loc, support::world::BIG, support::world::STEPS)
            .unwrap()
            .raw
            .contains("hello")
    );
    let mut foreign = loc;
    foreign.session.native_id = "other".into();
    assert!(
        support::world::try_source(&mut s, &foreign, support::world::BIG, support::world::STEPS)
            .is_err()
    );
}
#[test]
fn partial_append_then_completion_adds_one_event() {
    let (_dir, path, mut s, key) = setup(&[assistant("a", json!("first"))]);
    let line = assistant("b", json!("second")).to_string();
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    write!(f, "{}", &line[..line.len() / 2]).unwrap();
    f.flush().unwrap();
    support::world::rescan(&mut s, NOW, BUDGET);
    assert_eq!(
        support::world::try_transcript(
            &mut s,
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                ..PAGE
            },
            support::world::BIG,
            support::world::STEPS
        )
        .unwrap()
        .events
        .len(),
        1
    );
    writeln!(f, "{}", &line[line.len() / 2..]).unwrap();
    f.flush().unwrap();
    support::world::rescan(&mut s, NOW, BUDGET);
    assert_eq!(
        support::world::try_transcript(
            &mut s,
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                ..PAGE
            },
            support::world::BIG,
            support::world::STEPS
        )
        .unwrap()
        .events
        .len(),
        2
    );
}
#[test]
fn rotation_resets_cursor_content_and_usage_ledger() {
    let (_dir, path, mut s, key) = setup(&[
        assistant("a", json!("old content")),
        assistant("b", json!("old second")),
    ]);
    std::fs::write(&path, format!("{}\n", assistant("a", json!("new")))).unwrap();
    let delta = support::world::rescan(&mut s, NOW, BUDGET);
    assert_eq!(delta.sessions[0].usage.as_ref().unwrap().total, Some(3));
    let p = support::world::try_transcript(
        &mut s,
        &key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 10,
            ..PAGE
        },
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(p.events.len(), 1);
    assert_eq!(p.events[0].text.as_ref().unwrap().preview, "new");
}
#[test]
fn suppressed_and_unknown_records_appear_in_diagnostics() {
    let (_dir, _, mut s, key) = setup(&[
        json!({"type":"file-history-snapshot"}),
        json!({"type":"mystery"}),
        assistant("a", json!("visible")),
    ]);
    let p = support::world::try_transcript(
        &mut s,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(p.diagnostics.len(), 2);
    for d in p.diagnostics {
        assert!(
            support::world::try_source(
                &mut s,
                &d.locator,
                support::world::BIG,
                support::world::STEPS
            )
            .is_ok()
        );
    }
}

#[test]
fn index_builds_in_bounded_resumable_steps() {
    let records: Vec<_> = (0..2200)
        .map(|n| assistant(&format!("{n}"), json!("x")))
        .collect();
    let (_dir, _, mut scanner, key) = setup(&records);
    let (page, pending) = support::world::transcript_steps(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        BUDGET,
        support::world::STEPS,
    );
    assert!(pending >= 4, "{pending}: 2,200 records at 512 per step");
    assert_eq!(page.events.len(), 1);
    assert!(page.has_more);
}

#[test]
fn stale_locator_is_rejected_after_rotation_and_refresh() {
    let (_dir, path, mut scanner, key) =
        setup(&[assistant("old", json!("old content long enough"))]);
    let old = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap()
    .events[0]
        .provenance
        .locator
        .clone();
    std::fs::write(path, format!("{}\n", assistant("new", json!("new")))).unwrap();
    support::world::rescan(&mut scanner, NOW, BUDGET);
    assert!(
        support::world::try_source(
            &mut scanner,
            &old,
            support::world::BIG,
            support::world::STEPS
        )
        .is_err()
    );
    let new = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap()
    .events[0]
        .provenance
        .locator
        .clone();
    assert!(
        support::world::try_source(
            &mut scanner,
            &new,
            support::world::BIG,
            support::world::STEPS
        )
        .unwrap()
        .raw
        .contains("new")
    );
}

#[test]
fn nested_hidden_and_joined_result_have_openable_diagnostics() {
    let (_dir, _, mut scanner, key) = setup(&[
        assistant(
            "a",
            json!([{"type":"tool_use","id":"c","name":"Read","input":{}}]),
        ),
        json!({"type":"user","uuid":"b","message":{"content":[{"type":"tool_result","tool_use_id":"c","content":[{"type":"text","text":"done"},{"type":"unknown_nested","secret":"synthetic"}]}]}}),
    ]);
    let page = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert!(
        page.diagnostics
            .iter()
            .any(|d| d.label.contains("source-only"))
    );
    assert!(page.diagnostics.iter().any(|d| d.label.contains("hidden")));
    for diag in page.diagnostics {
        assert!(
            support::world::try_source(
                &mut scanner,
                &diag.locator,
                support::world::BIG,
                support::world::STEPS
            )
            .unwrap()
            .raw
            .contains("tool_result")
        );
    }
}

#[test]
fn revisions_and_shared_message_usage_are_consistent_across_pages() {
    let a = assistant("a", json!("old"));
    let mut b = assistant("b", json!("other"));
    b["message"]["id"] = json!("a");
    let replacement = assistant("a", json!("replacement"));
    let (_dir, _, mut scanner, key) = setup(&[a, b, replacement]);
    let page = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 10,
            ..PAGE
        },
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(page.events.len(), 2);
    assert_eq!(page.events.iter().filter(|e| e.usage.is_some()).count(), 1);
    assert_eq!(page.events[1].text.as_ref().unwrap().preview, "replacement");
}

#[test]
fn index_preserves_partial_record_completed_beyond_scanned_boundary() {
    let (_dir, path, mut scanner, key) = setup(&[assistant("first", json!("first"))]);
    let line = assistant("second", json!("second")).to_string();
    let split = line.len() / 2;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    write!(file, "{}", &line[..split]).unwrap();
    file.flush().unwrap();
    support::world::rescan(&mut scanner, NOW, BUDGET);
    writeln!(file, "{}", &line[split..]).unwrap();
    file.flush().unwrap();
    let page = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(page.events.len(), 1);
    support::world::rescan(&mut scanner, NOW, BUDGET);
    let page = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Backward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(page.events[0].text.as_ref().unwrap().preview, "second");
}

#[test]
fn large_sampled_source_rejects_replacement_with_identical_samples() {
    let (_dir, path, mut scanner, key) = setup(&[assistant("a", json!("visible"))]);
    let locator = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap()
    .events[0]
        .provenance
        .locator
        .clone();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(file, "{}", "x".repeat(5 * 1024 * 1024)).unwrap();
    drop(file);
    support::world::rescan(&mut scanner, NOW, BUDGET);
    assert!(
        support::world::try_source(
            &mut scanner,
            &locator,
            support::world::BIG,
            support::world::STEPS
        )
        .is_ok()
    );
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, std::fs::read(&path).unwrap()).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    assert!(
        support::world::try_source(
            &mut scanner,
            &locator,
            support::world::BIG,
            support::world::STEPS
        )
        .is_err()
    );
}

#[test]
fn enormous_oversized_record_is_indexed_across_bounded_steps() {
    let (_dir, path, mut scanner, key) = setup(&[assistant("a", json!("visible"))]);
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", "x".repeat(10 * 1024 * 1024)).unwrap();
    // Hydration drains the oversized line under the work budget, so it takes several
    // slices; the transcript index then builds in its own bounded steps.
    let found = support::world::discover_pass(&mut scanner, NOW, BUDGET);
    assert_eq!(found.sessions.len(), 1);
    let (_, slices) = support::world::hydrate_steps(&mut scanner, &key, NOW, BUDGET, 1000);
    assert!(slices >= 2, "oversized drain must span slices");
    let (page, pending) = support::world::transcript_steps(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        BUDGET,
        support::world::STEPS,
    );
    assert_eq!(page.events.len(), 1);
    assert!(pending >= 2);
}

#[test]
fn diagnostic_overflow_preserves_events_and_marks_truncation() {
    for visible in [false, true] {
        let mut records = vec![json!({"type":"hidden"}); 201];
        if visible {
            records.push(assistant("a", json!("visible")));
        }
        let (_dir, _, mut scanner, key) = setup(&records);
        let page = support::world::try_transcript(
            &mut scanner,
            &key,
            None,
            PageDirection::Forward,
            PAGE,
            support::world::BIG,
            support::world::STEPS,
        )
        .unwrap();
        assert_eq!(page.events.len(), usize::from(visible));
        if visible {
            assert_eq!(page.events[0].text.as_ref().unwrap().preview, "visible");
        }
        assert!(page.truncated);
        assert_eq!(page.diagnostics.len(), 200);
        let marker = page.diagnostics.last().unwrap();
        assert!(marker.label.contains("diagnostics truncated"));
        assert!(
            support::world::try_source(
                &mut scanner,
                &marker.locator,
                support::world::BIG,
                support::world::STEPS
            )
            .is_ok()
        );
    }
}

#[test]
fn finalization_is_resumable_after_small_append() {
    let records: Vec<_> = (0..1100)
        .map(|n| assistant(&format!("{n}"), json!("x")))
        .collect();
    let (_dir, path, mut scanner, key) = setup(&records);
    support::world::transcript_ready(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        BUDGET,
        support::world::STEPS,
    );
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", assistant("new", json!("last"))).unwrap();
    file.flush().unwrap();
    support::world::rescan(&mut scanner, NOW, BUDGET);
    let (page, pending) = support::world::transcript_steps(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PAGE,
        BUDGET,
        support::world::STEPS,
    );
    assert!(
        pending >= 1,
        "the extended index finalizes in resumable steps"
    );
    assert_eq!(page.events.len(), 1);
}

#[test]
fn large_uuid_many_blocks_hits_metadata_limit() {
    let record = assistant(
        &"u".repeat(500_000),
        json!(vec![json!({"type":"text","text":"x"}); 160]),
    );
    let (_dir, _, mut scanner, key) = setup(&[record]);
    assert!(matches!(
        support::world::try_transcript(
            &mut scanner,
            &key,
            None,
            PageDirection::Forward,
            PAGE,
            support::world::BIG,
            support::world::STEPS
        ),
        Err(agentscope::providers::ProviderError::Unsupported(_))
    ));
}

fn status_label(s: &agentscope::domain::SessionSummary) -> agentscope::domain::StatusLabel {
    let thresholds = agentscope::domain::Thresholds {
        quiet: std::time::Duration::from_secs(300),
        stall: std::time::Duration::from_secs(900),
    };
    agentscope::status::synthesize(s, NOW, &thresholds).label
}

fn stamped(kind: &str, uuid: &str, ts: &str, stop: Option<&str>, content: Value) -> Value {
    json!({"type":kind,"uuid":uuid,"sessionId":"s","timestamp":ts,
        "message":{"id":uuid,"role":kind,"stop_reason":stop,"content":content}})
}

fn append(path: &std::path::Path, records: &[Value]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for r in records {
        writeln!(f, "{r}").unwrap();
    }
    f.flush().unwrap();
}

#[test]
fn stop_reason_drives_completion_unfinished_work_and_reopening() {
    use agentscope::domain::{Lifecycle, StatusLabel};
    let (_dir, path, mut s, _key) = setup(&[
        stamped("user", "u1", "2026-10-29T10:00:00Z", None, json!("do it")),
        stamped(
            "assistant",
            "a1",
            "2026-10-29T10:00:01Z",
            Some("tool_use"),
            json!([{"type":"tool_use","id":"t1","name":"Read","input":{}}]),
        ),
    ]);
    append(
        &path,
        &[
            stamped(
                "user",
                "u2",
                "2026-10-29T10:00:02Z",
                None,
                json!([{"type":"tool_result","tool_use_id":"t1","content":"ok"}]),
            ),
            stamped(
                "assistant",
                "a2",
                "2026-10-29T10:00:03Z",
                Some("end_turn"),
                json!([{"type":"text","text":"all done"}]),
            ),
        ],
    );
    let done = support::world::rescan(&mut s, NOW, BUDGET)
        .sessions
        .remove(0);
    assert_eq!(done.lifecycle, Lifecycle::Completed);
    assert!(!done.unfinished_work);
    let completed_at = done.last_completion.expect("end_turn is a completion");
    assert_eq!(status_label(&done), StatusLabel::Complete);

    // Later user work reopens it and keeps the earlier completion time.
    append(
        &path,
        &[stamped(
            "user",
            "u3",
            "2026-10-29T11:00:00Z",
            None,
            json!("one more thing"),
        )],
    );
    let reopened = support::world::rescan(&mut s, NOW, BUDGET)
        .sessions
        .remove(0);
    assert!(reopened.unfinished_work);
    assert_eq!(reopened.lifecycle, Lifecycle::Ongoing);
    assert_eq!(reopened.last_completion, Some(completed_at));
    assert_ne!(status_label(&reopened), StatusLabel::Complete);

    // A tool_use stop keeps it open; injected-only user text does not reopen.
    append(
        &path,
        &[stamped(
            "assistant",
            "a3",
            "2026-10-29T11:00:01Z",
            Some("end_turn"),
            json!("fine"),
        )],
    );
    let closed = support::world::rescan(&mut s, NOW, BUDGET)
        .sessions
        .remove(0);
    assert!(!closed.unfinished_work);
    append(
        &path,
        &[stamped(
            "user",
            "u4",
            "2026-10-29T11:00:02Z",
            None,
            json!("<system-reminder>ignore</system-reminder>"),
        )],
    );
    let still = support::world::rescan(&mut s, NOW, BUDGET)
        .sessions
        .remove(0);
    assert!(!still.unfinished_work, "injected text is not user work");
    append(
        &path,
        &[stamped(
            "assistant",
            "a4",
            "2026-10-29T11:00:03Z",
            Some("tool_use"),
            json!([{"type":"tool_use","id":"t2","name":"Bash","input":{}}]),
        )],
    );
    let pending = support::world::rescan(&mut s, NOW, BUDGET)
        .sessions
        .remove(0);
    assert!(pending.unfinished_work);
    assert_eq!(pending.lifecycle, Lifecycle::Ongoing);
}

#[test]
fn result_appended_for_a_displayed_pending_call_invalidates_the_session() {
    let (_dir, path, mut s, key) = setup(&[assistant(
        "a",
        json!([{"type":"tool_use","id":"c","name":"Read","input":{}}]),
    )]);
    let page = |s: &mut ClaudeScanner| {
        support::world::try_transcript(
            s,
            &key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                ..PAGE
            },
            support::world::BIG,
            support::world::STEPS,
        )
        .unwrap()
    };
    assert_eq!(
        page(&mut s).events[0].tool.as_ref().unwrap().status,
        agentscope::domain::ToolStatus::Pending
    );
    // An unrelated append leaves the displayed row alone.
    append(&path, &[assistant("b", json!("note"))]);
    assert!(
        support::world::rescan(&mut s, NOW, BUDGET)
            .invalidated
            .is_empty()
    );

    append(
        &path,
        &[
            json!({"type":"user","uuid":"r","message":{"content":[{"type":"tool_result","tool_use_id":"c","content":"done"}]}}),
        ],
    );
    let delta = support::world::rescan(&mut s, NOW, BUDGET);
    assert_eq!(delta.invalidated, vec![key.clone()]);
    let after = page(&mut s);
    assert_eq!(
        after.events[0].tool.as_ref().unwrap().status,
        agentscope::domain::ToolStatus::Completed
    );
}

#[test]
fn growing_in_place_rewrite_resets_state_and_invalidates() {
    use std::io::Seek;
    let (_dir, path, mut s, key) = setup(&[
        assistant("a", json!("old content one")),
        assistant("b", json!("old content two")),
    ]);
    // Same file, same identity: overwrite the consumed bytes and grow past them.
    let new: String = ["new first", "new second", "new third", "new fourth"]
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{}\n", assistant(&format!("n{i}"), json!(t))))
        .collect();
    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(std::io::SeekFrom::Start(0)).unwrap();
    f.write_all(new.as_bytes()).unwrap();
    f.flush().unwrap();
    drop(f);

    let delta = support::world::rescan(&mut s, NOW, BUDGET);
    assert!(
        delta.invalidated.contains(&key),
        "rewrite must invalidate: {:?}",
        delta.invalidated
    );
    let summary = &delta.sessions[0];
    assert_eq!(summary.displayed_event_count, 4);
    let page = support::world::try_transcript(
        &mut s,
        &key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 10,
            ..PAGE
        },
        support::world::BIG,
        support::world::STEPS,
    )
    .unwrap();
    let texts: Vec<_> = page
        .events
        .iter()
        .map(|e| e.text.as_ref().unwrap().preview.clone())
        .collect();
    assert_eq!(
        texts,
        ["new first", "new second", "new third", "new fourth"]
    );
}

#[test]
fn middle_only_rewrite_followed_by_append_is_detected_exactly() {
    use std::io::Seek;
    let (_dir, path, mut s, key) = setup(&[
        assistant("a", json!("first record stays the same")),
        assistant("b", json!("MIDDLE-OLD-CONTENT")),
        assistant("c", json!("last record stays the same")),
    ]);
    // Rewrite only the middle record, same length, preserving head and tail bytes.
    let original = std::fs::read(&path).unwrap();
    let at = original
        .windows(18)
        .position(|w| w == b"MIDDLE-OLD-CONTENT")
        .unwrap() as u64;
    let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    f.seek(std::io::SeekFrom::Start(at)).unwrap();
    f.write_all(b"MIDDLE-NEW-CONTENT").unwrap();
    f.flush().unwrap();
    drop(f);
    append(&path, &[assistant("d", json!("appended"))]);

    let delta = support::world::rescan(&mut s, NOW, BUDGET);
    assert!(delta.invalidated.contains(&key), "{:?}", delta.invalidated);
    let page = support::world::try_transcript(
        &mut s,
        &key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 10,
            ..PAGE
        },
        support::world::BIG,
        support::world::STEPS,
    )
    .expect("pages are readable after the rebuild");
    let texts: Vec<_> = page
        .events
        .iter()
        .map(|e| e.text.as_ref().unwrap().preview.clone())
        .collect();
    assert_eq!(
        texts,
        [
            "first record stays the same",
            "MIDDLE-NEW-CONTENT",
            "last record stays the same",
            "appended"
        ]
    );
}

#[test]
fn prefix_verification_is_resumable_under_a_small_budget() {
    let records: Vec<Value> = (0..40)
        .map(|i| assistant(&format!("u{i}"), json!("x".repeat(2000))))
        .collect();
    let (_dir, path, mut s, key) = setup(&records);
    append(&path, &[assistant("extra", json!("more"))]);
    let small = RefreshBudget {
        max_records: 1000,
        max_bytes: 16 * 1024,
        max_millis: 60_000,
    };
    assert_eq!(
        support::world::discover_pass(&mut s, NOW, small)
            .sessions
            .len(),
        1
    );
    let (summary, steps) = support::world::hydrate_steps(&mut s, &key, NOW, small, 1000);
    assert!(steps >= 5, "80 KB verified at 16 KB per step: {steps}");
    assert_eq!(summary.displayed_event_count, 41);
    assert!(
        support::world::try_transcript(
            &mut s,
            &key,
            None,
            PageDirection::Forward,
            PAGE,
            support::world::BIG,
            support::world::STEPS
        )
        .is_ok()
    );
}

#[test]
fn tool_use_without_stop_reason_after_end_turn_reopens_the_turn() {
    use agentscope::domain::{Lifecycle, StatusLabel};
    let (_dir, path, mut s, _key) = setup(&[
        stamped("user", "u1", "2026-10-29T10:00:00Z", None, json!("go")),
        stamped(
            "assistant",
            "a1",
            "2026-10-29T10:00:01Z",
            Some("end_turn"),
            json!("done"),
        ),
    ]);
    // Establish the completed state through an append so it is emitted.
    append(
        &path,
        &[stamped(
            "assistant",
            "a2",
            "2026-10-29T10:00:02Z",
            Some("end_turn"),
            json!("really done"),
        )],
    );
    let done = support::world::rescan(&mut s, NOW, BUDGET)
        .sessions
        .remove(0);
    assert_eq!(status_label(&done), StatusLabel::Complete);
    let completed_at = done.last_completion.unwrap();
    // Streamed assistant record: tool_use block, null stop_reason, no user record.
    append(
        &path,
        &[stamped(
            "assistant",
            "a3",
            "2026-10-29T10:05:00Z",
            None,
            json!([{"type":"tool_use","id":"t9","name":"Bash","input":{}}]),
        )],
    );
    let open = support::world::rescan(&mut s, NOW, BUDGET)
        .sessions
        .remove(0);
    assert!(open.unfinished_work);
    assert_eq!(open.lifecycle, Lifecycle::Ongoing);
    assert_eq!(open.last_completion, Some(completed_at));
    assert_ne!(status_label(&open), StatusLabel::Complete);
}
