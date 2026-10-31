mod support;

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use agentscope::config::Config;
use agentscope::domain::{
    DiagnosticEntry, Event, EventKind, Lifecycle, LocatorKind, PageAnchor, PageDirection,
    PageRequest, RefreshBudget, SessionKey, SessionSummary, SourceKey, SourceLocator, ToolStatus,
    TranscriptPage,
};
use agentscope::providers::ProviderError;
use agentscope::providers::codex::CodexScanner;
use serde_json::json;
use support::codex::*;
use support::{NOW, config_for, dated, set_modified, write_lines};

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};
const ALL: PageRequest = PageRequest {
    max_events: 1000,
    max_bytes: 10_000_000,
};

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    path: PathBuf,
    scanner: CodexScanner,
}

fn scan(scanner: &mut CodexScanner) -> Vec<SessionSummary> {
    support::world::hydrated_all(scanner, NOW, BIG)
}

fn open_with(lines: &[String], tweak: impl FnOnce(&mut Config)) -> (Fixture, SessionSummary) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let path = dated(&root, "rollout-t.jsonl");
    write_lines(&path, lines);
    let mut config = config_for(&root);
    tweak(&mut config);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let summary = scan(&mut scanner).pop().expect("one session");
    (
        Fixture {
            _dir: dir,
            root,
            path,
            scanner,
        },
        summary,
    )
}

fn open(lines: &[String]) -> (Fixture, SessionSummary) {
    open_with(lines, |_| {})
}

fn first(f: &mut Fixture, key: &SessionKey) -> TranscriptPage {
    support::world::try_transcript(
        &mut f.scanner,
        key,
        None,
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .expect("page")
}

fn anchor_of(e: &Event) -> PageAnchor {
    PageAnchor {
        timestamp: e.timestamp,
        source_order: e.source_order,
        stable_id: e.id.clone(),
    }
}

fn kinds(page: &TranscriptPage) -> Vec<EventKind> {
    page.events.iter().map(|e| e.kind).collect()
}

fn ids(events: &[Event]) -> Vec<String> {
    events.iter().map(|e| e.id.clone()).collect()
}

fn append(path: &Path, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(bytes)
        .unwrap();
}

fn diag<'a>(page: &'a TranscriptPage, label: &str) -> Vec<&'a DiagnosticEntry> {
    page.diagnostics
        .iter()
        .filter(|d| d.label.contains(label))
        .collect()
}

fn offset_of(locator: &SourceLocator) -> u64 {
    match &locator.kind {
        LocatorKind::FileOffset { offset, .. } => *offset,
        other => panic!("unexpected locator {other:?}"),
    }
}

#[test]
fn renders_user_assistant_reasoning_and_one_row_per_tool() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "s1", "/p"),
        turn_context("2026-10-29T10:00:00Z", "gpt-x", "/p"),
        user("2026-10-29T10:00:01Z", "run the tests"),
        reasoning(
            "2026-10-29T10:00:02Z",
            "Thinking about tests",
            "SECRET-RAW-CHAIN",
        ),
        assistant("2026-10-29T10:00:03Z", "Running them now"),
        custom_call("2026-10-29T10:00:04Z", "call-1", "shell", "cargo test"),
        custom_output("2026-10-29T10:00:05Z", "call-1", "ok"),
        item_completed_timed(
            "2026-10-29T10:00:05Z",
            command_item("item-9", "cargo test", "3 passed", 0),
            1_793_000_000_000,
            1_793_000_002_500,
        ),
        item_completed(
            "2026-10-29T10:00:06Z",
            json!({"type": "AgentMessage", "id": "m", "content": [{"text": "Running them now"}]}),
        ),
        token_count("2026-10-29T10:00:06Z", usage(10, 0, 5, 0, 15), Some(1000)),
    ]);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(
        kinds(&page),
        vec![
            EventKind::UserPrompt,
            EventKind::Reasoning,
            EventKind::AssistantMessage,
            EventKind::Tool
        ]
    );
    assert_eq!(
        page.events[0].text.as_ref().unwrap().preview,
        "run the tests"
    );
    let reasoning_text = page.events[1].text.as_ref().unwrap();
    assert_eq!(reasoning_text.preview, "Thinking about tests");
    assert!(!format!("{:?}", page.events).contains("SECRET-RAW-CHAIN"));
    for e in &page.events {
        assert!(
            !e.text
                .iter()
                .any(|t| t.preview.contains("SECRET-RAW-CHAIN"))
        );
    }
    let tool = page.events[3].tool.as_ref().expect("tool interaction");
    assert_eq!(tool.status, ToolStatus::Completed);
    assert_eq!(tool.exit_code, Some(0));
    assert_eq!(tool.input.as_ref().unwrap().preview, "bash -lc cargo test");
    assert_eq!(tool.output.as_ref().unwrap().preview, "3 passed");
    assert_eq!(tool.started_at, Some(1_793_000_000_000));
    assert_eq!(tool.completed_at, Some(1_793_000_002_500));
    assert!(!tool.orphan);
    // Raw custom call/output are source-only diagnostics, never a second row.
    assert_eq!(
        page.events
            .iter()
            .filter(|e| e.kind == EventKind::Tool)
            .count(),
        1
    );
    assert_eq!(diag(&page, "custom_tool_call").len(), 2);
    // The duplicate AgentMessage item and token_count are hidden, not rows.
    assert_eq!(s.displayed_event_count, page.events.len());
    assert_eq!(s.hidden_records, 6);
}

#[test]
fn tool_exit_code_is_tool_status_not_session_failure() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "s2", "/p"),
        task_started("2026-10-29T10:00:01Z", None),
        item_completed(
            "2026-10-29T10:00:02Z",
            command_item("i1", "false", "boom", 1),
        ),
        task_complete("2026-10-29T10:00:03Z"),
    ]);
    let page = first(&mut f, &s.meta.key);
    let tool = page
        .events
        .iter()
        .find_map(|e| e.tool.as_ref())
        .expect("tool");
    assert_eq!(tool.status, ToolStatus::Error);
    assert_eq!(tool.exit_code, Some(1));
    assert_eq!(s.lifecycle, Lifecycle::Completed);
    assert_eq!(
        kinds(&page),
        vec![EventKind::Lifecycle, EventKind::Tool, EventKind::Lifecycle]
    );
}

#[test]
fn pending_and_orphan_tools_are_labeled() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "s3", "/p"),
        function_call("2026-10-29T10:00:01Z", "c-paired", "shell", "{}"),
        function_output("2026-10-29T10:00:02Z", "c-paired", "done"),
        function_call(
            "2026-10-29T10:00:03Z",
            "c-open",
            "shell",
            "{\"cmd\":\"sleep\"}",
        ),
        function_output("2026-10-29T10:00:04Z", "c-ghost", "late result"),
        item_completed(
            "2026-10-29T10:00:05Z",
            json!({"type": "McpToolCall", "id": "m", "server": "srv", "tool": "t",
                   "arguments": {"a": 1}, "status": "in_progress"}),
        ),
    ]);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 3);
    let pending = page.events[0].tool.as_ref().unwrap();
    assert_eq!(pending.status, ToolStatus::Pending);
    assert_eq!(pending.call_id.as_deref(), Some("c-open"));
    assert_eq!(pending.name, "shell");
    assert!(pending.output.is_none() && !pending.orphan);
    let orphan = page.events[1].tool.as_ref().unwrap();
    assert!(orphan.orphan);
    assert!(orphan.input.is_none());
    assert_eq!(orphan.output.as_ref().unwrap().preview, "late result");
    assert_eq!(orphan.status, ToolStatus::Unknown);
    let running = page.events[2].tool.as_ref().unwrap();
    assert_eq!(running.status, ToolStatus::Pending);
    assert_eq!(running.name, "srv/t");
    // The paired call/output stay source-only.
    assert_eq!(diag(&page, "function_call").len(), 2);
    assert!(
        s.warnings
            .iter()
            .any(|w| w.key.source.ends_with("#orphan-tool") && w.count == 1)
    );
    assert_eq!(s.displayed_event_count, 3);

    // Completing the open call later removes its row (the item is source-only again).
    append(
        &f.path,
        format!(
            "{}\n",
            function_output("2026-10-29T10:00:06Z", "c-open", "woke")
        )
        .as_bytes(),
    );
    scan(&mut f.scanner);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 2);
    assert!(page.events.iter().all(|e| {
        e.tool
            .as_ref()
            .is_none_or(|t| t.call_id.as_deref() != Some("c-open"))
    }));
}

#[test]
fn suppressed_custom_calls_are_source_only_and_addressable() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "s4", "/p"),
        user("2026-10-29T10:00:01Z", "go"),
        custom_call(
            "2026-10-29T10:00:02Z",
            "k",
            "apply_patch",
            "*** Begin Patch",
        ),
        custom_output("2026-10-29T10:00:03Z", "k", "Done!"),
        record("2026-10-29T10:00:04Z", "future_thing", json!({"x": 1})),
        record("2026-10-29T10:00:05Z", "world_state", json!({})),
    ]);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 1);
    let source_only = diag(&page, "custom_tool_call");
    assert_eq!(source_only.len(), 2);
    assert!(source_only.iter().all(|d| !d.hidden));
    let unknown = diag(&page, "future_thing");
    assert_eq!(unknown.len(), 1);
    assert!(unknown[0].hidden);
    assert_eq!(diag(&page, "world_state").len(), 1);

    let call = support::world::try_source(
        &mut f.scanner,
        &source_only[0].locator,
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert!(call.raw.contains("apply_patch"));
    assert!(call.unavailable.is_none());
    let output = support::world::try_source(
        &mut f.scanner,
        &source_only[1].locator,
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert!(output.raw.contains("Done!"));
    let future = support::world::try_source(
        &mut f.scanner,
        &unknown[0].locator,
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert!(future.raw.contains("future_thing"));
    assert!(future.display.contains("future_thing"));

    // Locators are validated against this session's own source.
    let good = &source_only[0].locator;
    let LocatorKind::FileOffset { path, offset } = &good.kind else {
        panic!("file locator")
    };
    let mut other_path = path.clone();
    other_path.set_file_name("rollout-other.jsonl");
    let bad = [
        SourceLocator {
            session: good.session.clone(),
            generation: good.generation,
            kind: LocatorKind::FileOffset {
                path: other_path,
                offset: *offset,
            },
        },
        SourceLocator {
            session: SessionKey {
                native_id: "nope".into(),
                ..good.session.clone()
            },
            kind: good.kind.clone(),
            generation: good.generation,
        },
        SourceLocator {
            session: SessionKey {
                source: SourceKey::File(f.root.join("elsewhere.jsonl")),
                ..good.session.clone()
            },
            kind: good.kind.clone(),
            generation: good.generation,
        },
        SourceLocator {
            session: good.session.clone(),
            generation: good.generation,
            kind: LocatorKind::FileOffset {
                path: path.clone(),
                offset: offset + 3, // mid-record
            },
        },
        SourceLocator {
            session: good.session.clone(),
            generation: good.generation,
            kind: LocatorKind::FileOffset {
                path: path.clone(),
                offset: 10_000_000,
            },
        },
        SourceLocator {
            session: good.session.clone(),
            generation: good.generation,
            kind: LocatorKind::DbRow {
                table: "t".into(),
                row_id: "1".into(),
                part_index: None,
            },
        },
    ];
    for locator in &bad {
        assert!(
            support::world::try_source(&mut f.scanner, locator, BIG, support::world::STEPS)
                .is_err(),
            "accepted {locator:?}"
        );
    }
    let missing = support::world::try_transcript(
        &mut f.scanner,
        &SessionKey {
            native_id: "nope".into(),
            ..s.meta.key.clone()
        },
        None,
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    );
    assert!(missing.is_err());
}

fn mixed_session(n: usize) -> Vec<String> {
    let mut lines = vec![session_meta("2026-10-29T10:00:00Z", "mix", "/p")];
    for i in 0..n {
        let ts = format!("2026-10-29T10:{:02}:{:02}Z", i / 60, i % 60);
        match i % 6 {
            0 => lines.push(user(&ts, &format!("prompt {i}"))),
            1 => lines.push(reasoning(&ts, &format!("think {i}"), "raw")),
            2 => {
                lines.push(custom_call(&ts, &format!("c{i}"), "shell", "x"));
                lines.push(custom_output(&ts, &format!("c{i}"), "y"));
                lines.push(item_completed(
                    &ts,
                    command_item(&format!("it{i}"), "ls", &format!("out {i}"), 0),
                ));
            }
            3 => {
                lines.push(token_count(&ts, usage(1, 0, 1, 0, 2), None));
                lines.push(assistant(&ts, &format!("answer {i}")));
            }
            4 => lines.push(record(&ts, "mystery", json!({}))),
            _ => lines.push(compacted(&ts)),
        }
    }
    lines
}

fn chain(
    f: &mut Fixture,
    key: &SessionKey,
    direction: PageDirection,
    request: PageRequest,
) -> Vec<TranscriptPage> {
    let mut pages = Vec::new();
    let mut anchor = None;
    loop {
        let page = support::world::try_transcript(
            &mut f.scanner,
            key,
            anchor.clone(),
            direction,
            request,
            BIG,
            support::world::STEPS,
        )
        .unwrap();
        assert!(page.events.len() <= request.max_events);
        let more = page.has_more;
        anchor = page.next.clone();
        pages.push(page);
        if !more {
            return pages;
        }
        assert!(anchor.is_some(), "has_more without a continuation anchor");
        assert!(pages.len() < 1000, "paging never finished");
    }
}

#[test]
fn forward_and_backward_pages_do_not_duplicate_or_split_tool_pairs() {
    let (mut f, s) = open(&mixed_session(60));
    let whole = first(&mut f, &s.meta.key);
    assert!(!whole.has_more);
    let all_ids = ids(&whole.events);
    assert_eq!(all_ids.iter().collect::<HashSet<_>>().len(), all_ids.len());
    assert_eq!(
        whole.events.iter().filter(|e| e.tool.is_some()).count(),
        10,
        "one row per completed tool"
    );
    assert_eq!(s.displayed_event_count, whole.events.len());

    for size in [1usize, 2, 3, 7, 1000] {
        let request = PageRequest {
            max_events: size,
            max_bytes: 10_000_000,
        };
        let forward = chain(&mut f, &s.meta.key, PageDirection::Forward, request);
        let backward = chain(&mut f, &s.meta.key, PageDirection::Backward, request);
        let fwd: Vec<_> = forward.iter().flat_map(|p| ids(&p.events)).collect();
        let mut bwd_pages: Vec<_> = backward.iter().map(|p| ids(&p.events)).collect();
        bwd_pages.reverse();
        let bwd: Vec<_> = bwd_pages.into_iter().flatten().collect();
        assert_eq!(fwd, all_ids, "forward size {size}");
        assert_eq!(bwd, all_ids, "backward size {size}");
        // Diagnostics partition the same way in both directions.
        let diag_ids = |pages: &[TranscriptPage]| {
            let mut v: Vec<u64> = pages
                .iter()
                .flat_map(|p| p.diagnostics.iter().map(|d| offset_of(&d.locator)))
                .collect();
            let n = v.len();
            v.sort_unstable();
            v.dedup();
            assert_eq!(v.len(), n, "duplicate diagnostics, size {size}");
            v
        };
        assert_eq!(diag_ids(&forward), diag_ids(&backward), "size {size}");
        assert_eq!(diag_ids(&forward).len(), whole.diagnostics.len());
    }

    // Backward from an interior anchor returns exactly the events before it.
    let mid = &whole.events[10];
    let before = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        Some(anchor_of(mid)),
        PageDirection::Backward,
        PageRequest {
            max_events: 4,
            max_bytes: 10_000_000,
        },
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(ids(&before.events), all_ids[6..10].to_vec());
    assert!(before.has_more);
    assert_eq!(before.next.as_ref().unwrap().stable_id, all_ids[6]);

    // The byte budget limits a page but always makes progress.
    let tiny = PageRequest {
        max_events: 1000,
        max_bytes: 40,
    };
    let pages = chain(&mut f, &s.meta.key, PageDirection::Forward, tiny);
    assert!(pages.len() > 5);
    assert!(pages.iter().all(|p| !p.events.is_empty()));
    let combined: Vec<_> = pages.iter().flat_map(|p| ids(&p.events)).collect();
    assert_eq!(combined, all_ids);
}

#[test]
fn count_events_after_counts_displayed_work_only() {
    let (mut f, s) = open(&mixed_session(30));
    let whole = first(&mut f, &s.meta.key);
    let anchor = anchor_of(&whole.events[whole.events.len() - 4]);
    assert_eq!(
        support::world::try_count(
            &mut f.scanner,
            &s.meta.key,
            &anchor,
            BIG,
            support::world::STEPS
        )
        .unwrap(),
        3
    );
    append(
        &f.path,
        format!(
            "{}\n{}\n{}\n",
            token_count("2026-10-29T11:00:00Z", usage(1, 0, 1, 0, 2), None),
            assistant("2026-10-29T11:00:01Z", "new one"),
            user("2026-10-29T11:00:02Z", "new two"),
        )
        .as_bytes(),
    );
    // Not visible until the roster scan has consumed it.
    assert_eq!(
        support::world::try_count(
            &mut f.scanner,
            &s.meta.key,
            &anchor,
            BIG,
            support::world::STEPS
        )
        .unwrap(),
        3
    );
    scan(&mut f.scanner);
    assert_eq!(
        support::world::try_count(
            &mut f.scanner,
            &s.meta.key,
            &anchor,
            BIG,
            support::world::STEPS
        )
        .unwrap(),
        5
    );
}

#[test]
fn partial_append_rotation_and_oversized_records_are_safe() {
    let (mut f, s) = open_with(
        &[
            session_meta("2026-10-29T10:00:00Z", "rot", "/p"),
            user("2026-10-29T10:00:01Z", "first"),
            user("2026-10-29T10:00:02Z", &"y".repeat(5000)),
            assistant("2026-10-29T10:00:03Z", "after the big one"),
        ],
        |c| c.max_record_bytes = 2048,
    );
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 2);
    let big = diag(&page, "oversized");
    assert_eq!(big.len(), 1);
    assert!(big[0].hidden);
    let raw =
        support::world::try_source(&mut f.scanner, &big[0].locator, BIG, support::world::STEPS)
            .unwrap();
    assert!(raw.raw.is_empty() && raw.unavailable.is_some());
    // Backward reads classify the same oversized record.
    let back = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        None,
        PageDirection::Backward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(ids(&back.events), ids(&page.events));
    assert_eq!(diag(&back, "oversized").len(), 1);

    // A partial trailing line is held until its newline arrives.
    let tail = assistant("2026-10-29T10:00:04Z", "late reply");
    let (head, rest) = tail.split_at(20);
    append(&f.path, head.as_bytes());
    scan(&mut f.scanner);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 2);
    assert_eq!(
        support::world::try_transcript(
            &mut f.scanner,
            &s.meta.key,
            None,
            PageDirection::Backward,
            ALL,
            BIG,
            support::world::STEPS
        )
        .unwrap()
        .events
        .len(),
        2
    );
    append(&f.path, format!("{rest}\n").as_bytes());
    scan(&mut f.scanner);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 3);
    let old_locator = page.events[0].provenance.locator.clone();
    assert!(
        support::world::try_source(&mut f.scanner, &old_locator, BIG, support::world::STEPS)
            .is_ok()
    );
    let old_anchor = anchor_of(&page.events[2]);

    // Rotation: a shorter replacement restarts the transcript; stale anchors yield nothing.
    write_lines(
        &f.path,
        &[
            session_meta("2026-10-29T12:00:00Z", "rot", "/p"),
            user("2026-10-29T12:00:01Z", "fresh"),
        ],
    );
    let summaries = scan(&mut f.scanner);
    assert!(summaries.iter().any(|x| x.meta.key == s.meta.key));
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 1);
    assert!(
        support::world::try_source(&mut f.scanner, &old_locator, BIG, support::world::STEPS)
            .is_err()
    );
    assert!(
        support::world::try_source(
            &mut f.scanner,
            &page.events[0].provenance.locator,
            BIG,
            support::world::STEPS
        )
        .is_ok()
    );
    assert_eq!(page.events[0].text.as_ref().unwrap().preview, "fresh");
    let stale = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        Some(old_anchor),
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert!(stale.events.is_empty() && !stale.has_more);
}

#[test]
fn previews_are_bounded_redacted_and_marked_truncated() {
    let secret = "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789";
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "red", "/p"),
        item_completed(
            "2026-10-29T10:00:01Z",
            command_item("i", "curl", &format!("{secret}\n{}", "z".repeat(50_000)), 0),
        ),
    ]);
    let page = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 10,
            max_bytes: 4096,
        },
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    let out = page.events[0]
        .tool
        .as_ref()
        .unwrap()
        .output
        .as_ref()
        .unwrap();
    assert!(!out.preview.contains("abcdefghijklmnop"));
    assert!(out.truncated);
    assert!(out.preview.len() <= 4096);
    assert!(out.byte_len > 50_000);
    assert!(page.truncated);
    assert!(out.locator.is_some());
}

#[test]
fn backward_reads_cross_block_boundaries_and_oversized_records() {
    // Records of 100 B to 200 KB straddle the 64 KiB read blocks in every alignment.
    let mut lines = vec![session_meta("2026-10-29T10:00:00Z", "big", "/p")];
    for (i, size) in [
        100usize, 70_000, 30_000, 200_000, 65_536, 65_000, 1, 140_000, 5,
    ]
    .iter()
    .cycle()
    .take(27)
    .enumerate()
    {
        let ts = format!("2026-10-29T10:{:02}:{:02}Z", i / 60, i % 60);
        lines.push(user(&ts, &format!("{i}:{}", "q".repeat(*size))));
        if i % 4 == 0 {
            lines.push(record(&ts, "mystery", json!({"i": i})));
        }
    }
    let (mut f, s) = open_with(&lines, |c| c.max_record_bytes = 150_000);
    let whole = first(&mut f, &s.meta.key);
    let oversized = diag(&whole, "oversized").len();
    assert_eq!(oversized, 3);
    assert_eq!(whole.events.len(), 24);
    let request = PageRequest {
        max_events: 5,
        max_bytes: 10_000_000,
    };
    let forward = chain(&mut f, &s.meta.key, PageDirection::Forward, request);
    let backward = chain(&mut f, &s.meta.key, PageDirection::Backward, request);
    let mut bwd: Vec<_> = backward.iter().map(|p| ids(&p.events)).collect();
    bwd.reverse();
    assert_eq!(
        forward
            .iter()
            .flat_map(|p| ids(&p.events))
            .collect::<Vec<_>>(),
        bwd.into_iter().flatten().collect::<Vec<_>>()
    );
    let offsets = |pages: &[TranscriptPage]| {
        let mut v: Vec<u64> = pages
            .iter()
            .flat_map(|p| p.diagnostics.iter().map(|d| offset_of(&d.locator)))
            .collect();
        v.sort_unstable();
        v
    };
    assert_eq!(offsets(&forward), offsets(&backward));
    assert_eq!(offsets(&forward).len(), whole.diagnostics.len());
    // Every diagnostic locator resolves to the record it names.
    for d in &whole.diagnostics {
        let rec =
            support::world::try_source(&mut f.scanner, &d.locator, BIG, support::world::STEPS)
                .unwrap();
        if d.label.contains("oversized") {
            assert!(rec.unavailable.is_some());
        } else {
            assert!(
                rec.raw.starts_with('{') && rec.raw.ends_with('}'),
                "{}",
                d.label
            );
        }
    }
}

fn last_summary(scanner: &mut CodexScanner) -> SessionSummary {
    scan(scanner).pop().expect("changed session")
}

#[test]
fn long_credential_values_are_redacted_before_the_preview_is_cut() {
    let value = format!("correct horse battery staple {}", "Z".repeat(20_000));
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "cred", "/p"),
        item_completed(
            "2026-10-29T10:00:01Z",
            command_item("i", "env", &format!("password: \"{value}\" trailing"), 0),
        ),
        user("2026-10-29T10:00:02Z", &format!("api_key = \"{value}\"")),
    ]);
    let page = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 10,
            max_bytes: 4096,
        },
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    let out = page.events[0]
        .tool
        .as_ref()
        .unwrap()
        .output
        .as_ref()
        .unwrap();
    let prompt = page.events[1].text.as_ref().unwrap();
    for text in [out, prompt] {
        assert!(!text.preview.contains("correct horse"), "{}", text.preview);
        assert!(!text.preview.contains("ZZZZ"), "{}", text.preview);
        assert!(text.byte_len > 20_000);
    }
}

#[test]
fn page_byte_budget_applies_to_the_whole_first_tool_row() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "fit", "/p"),
        item_completed(
            "2026-10-29T10:00:01Z",
            command_item("i", &"a".repeat(3000), &"b".repeat(3000), 0),
        ),
        user("2026-10-29T10:00:02Z", "next"),
    ]);
    for budget in [1000usize, 300, 20] {
        let page = support::world::try_transcript(
            &mut f.scanner,
            &s.meta.key,
            None,
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                max_bytes: budget,
            },
            BIG,
            support::world::STEPS,
        )
        .unwrap();
        assert_eq!(page.events.len(), 1, "budget {budget}: oversized row alone");
        assert!(page.has_more && page.truncated);
        let tool = page.events[0].tool.as_ref().unwrap();
        let used = tool.name.len()
            + tool.input.as_ref().map_or(0, |t| t.preview.len())
            + tool.output.as_ref().map_or(0, |t| t.preview.len());
        assert!(used <= budget, "budget {budget}: {used} bytes");
        assert!(tool.input.as_ref().unwrap().truncated);
        assert!(tool.output.as_ref().unwrap().truncated);
        // Progress is still made: the next page starts at the following row.
        let next = support::world::try_transcript(
            &mut f.scanner,
            &s.meta.key,
            page.next.clone(),
            PageDirection::Forward,
            PageRequest {
                max_events: 10,
                max_bytes: budget,
            },
            BIG,
            support::world::STEPS,
        )
        .unwrap();
        assert_eq!(next.events.len(), 1);
        assert_eq!(next.events[0].kind, EventKind::UserPrompt);
    }
}

#[test]
fn completion_without_raw_output_replaces_the_pending_call() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "pend", "/p"),
        custom_call("2026-10-29T10:00:01Z", "k1", "shell", "cargo test"),
        item_completed(
            "2026-10-29T10:00:02Z",
            command_item("item-1", "cargo test", "ok", 0),
        ),
    ]);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 1, "no pending row beside the completion");
    let tool = page.events[0].tool.as_ref().unwrap();
    assert_eq!(tool.status, ToolStatus::Completed);
    assert_eq!(s.displayed_event_count, 1);
    assert_eq!(diag(&page, "custom_tool_call").len(), 1);

    // The raw output arriving later is neither an orphan nor a second row.
    append(
        &f.path,
        format!("{}\n", custom_output("2026-10-29T10:00:03Z", "k1", "ok")).as_bytes(),
    );
    let s = last_summary(&mut f.scanner);
    assert!(
        !s.warnings
            .iter()
            .any(|w| w.key.source.ends_with("#orphan-tool"))
    );
    assert_eq!(s.displayed_event_count, 1);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 1);
    assert_eq!(diag(&page, "custom_tool_call").len(), 2);

    // An in-progress item does not retire a genuinely pending call.
    append(
        &f.path,
        format!(
            "{}\n{}\n",
            custom_call("2026-10-29T10:00:04Z", "k2", "shell", "sleep"),
            item_completed(
                "2026-10-29T10:00:05Z",
                json!({"type": "McpToolCall", "id": "m", "server": "s", "tool": "t",
                       "status": "in_progress"}),
            ),
        )
        .as_bytes(),
    );
    let s = last_summary(&mut f.scanner);
    assert_eq!(first(&mut f, &s.meta.key).events.len(), 3);
}

#[test]
fn replaced_source_is_not_served_through_old_offsets() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "swap", "/p"),
        user("2026-10-29T10:00:01Z", "aaaa original"),
        custom_call("2026-10-29T10:00:02Z", "c", "shell", "x"),
        custom_output("2026-10-29T10:00:03Z", "c", "y"),
    ]);
    let page = first(&mut f, &s.meta.key);
    let locator = diag(&page, "custom_tool_call")[0].locator.clone();
    let anchor = anchor_of(&page.events[0]);
    let original = std::fs::read_to_string(&f.path).unwrap();

    // Same length, different content, no refresh in between.
    std::fs::write(&f.path, original.replace("aaaa", "bbbb")).unwrap();
    // Longer replacement.
    let check = |f: &mut Fixture| {
        assert!(
            support::world::try_transcript(
                &mut f.scanner,
                &s.meta.key,
                None,
                PageDirection::Forward,
                ALL,
                BIG,
                support::world::STEPS
            )
            .is_err()
        );
        assert!(
            support::world::try_transcript(
                &mut f.scanner,
                &s.meta.key,
                None,
                PageDirection::Backward,
                ALL,
                BIG,
                support::world::STEPS
            )
            .is_err()
        );
        assert!(
            support::world::try_count(
                &mut f.scanner,
                &s.meta.key,
                &anchor,
                BIG,
                support::world::STEPS
            )
            .is_err()
        );
        assert!(
            support::world::try_source(&mut f.scanner, &locator, BIG, support::world::STEPS)
                .is_err()
        );
    };
    check(&mut f);
    std::fs::write(
        &f.path,
        format!(
            "{}\n{}",
            original.replace("aaaa", "cccc"),
            user("2026-10-29T10:00:09Z", "appended elsewhere")
        ),
    )
    .unwrap();
    check(&mut f);

    // After a refresh the new content is what is served. (A shrink is what the
    // cursor can detect; an in-place longer rewrite is its documented blind spot.)
    write_lines(
        &f.path,
        &[
            session_meta("2026-10-29T11:00:00Z", "swap", "/p"),
            user("2026-10-29T11:00:01Z", "new"),
        ],
    );
    scan(&mut f.scanner);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].text.as_ref().unwrap().preview, "new");
}

#[test]
fn record_cap_is_applied_after_the_line_terminator_for_pages_and_source() {
    let cap = 2048usize;
    let pad = |total: usize| {
        let base = user("2026-10-29T10:00:01Z", "").len();
        user("2026-10-29T10:00:01Z", &"x".repeat(total - base))
    };
    let just_over = pad(cap + 1);
    let exact = pad(cap);
    assert_eq!((just_over.len(), exact.len()), (cap + 1, cap));
    let (mut f, s) = open_with(
        &[
            session_meta("2026-10-29T10:00:00Z", "cap", "/p"),
            just_over,
            exact,
        ],
        |c| c.max_record_bytes = cap,
    );
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 1, "only the in-cap record is a row");
    let over = diag(&page, "oversized");
    assert_eq!(over.len(), 1);
    let rec =
        support::world::try_source(&mut f.scanner, &over[0].locator, BIG, support::world::STEPS)
            .unwrap();
    assert!(rec.raw.is_empty() && rec.unavailable.is_some());
    let ok = support::world::try_source(
        &mut f.scanner,
        &page.events[0].provenance.locator,
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert!(ok.unavailable.is_none() && ok.raw.len() == cap);
}

#[test]
fn pages_follow_the_anchor_total_order_with_missing_and_decreasing_timestamps() {
    let no_clock = json!({"type": "response_item", "payload": {
        "type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "no clock"}]}})
    .to_string();
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "ord", "/p"),
        user("2026-10-29T10:00:05Z", "t5"),
        assistant("2026-10-29T10:00:01Z", "t1"),
        user("2026-10-29T10:00:03Z", "t3"),
        no_clock,
        assistant("2026-10-29T10:00:03Z", "t3b"),
        record("2026-10-29T10:00:02Z", "mystery", json!({})),
        item_completed("2026-10-29T10:00:04Z", command_item("i", "ls", "out", 0)),
        record("2026-10-29T10:00:06Z", "mystery", json!({})),
    ]);
    let whole = first(&mut f, &s.meta.key);
    let label = |e: &Event| match (&e.text, &e.tool) {
        (Some(t), _) => t.preview.clone(),
        (_, Some(_)) => "tool".to_owned(),
        _ => unreachable!(),
    };
    let order: Vec<String> = whole.events.iter().map(label).collect();
    assert_eq!(order, ["no clock", "t1", "t3", "t3b", "tool", "t5"]);
    let keys: Vec<_> = whole
        .events
        .iter()
        .map(|e| (e.timestamp, e.source_order))
        .collect();
    assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "anchor order: {keys:?}"
    );
    assert_eq!(whole.events[0].timestamp, None);
    let all_ids = ids(&whole.events);

    for size in [1usize, 2, 3, 5] {
        let request = PageRequest {
            max_events: size,
            max_bytes: 10_000_000,
        };
        let forward = chain(&mut f, &s.meta.key, PageDirection::Forward, request);
        let backward = chain(&mut f, &s.meta.key, PageDirection::Backward, request);
        let fwd: Vec<_> = forward.iter().flat_map(|p| ids(&p.events)).collect();
        let mut bwd_pages: Vec<_> = backward.iter().map(|p| ids(&p.events)).collect();
        bwd_pages.reverse();
        let bwd: Vec<_> = bwd_pages.into_iter().flatten().collect();
        assert_eq!(fwd, all_ids, "forward size {size}");
        assert_eq!(bwd, all_ids, "backward size {size}");
        let offsets = |pages: &[TranscriptPage]| {
            let mut v: Vec<u64> = pages
                .iter()
                .flat_map(|p| p.diagnostics.iter().map(|d| offset_of(&d.locator)))
                .collect();
            let n = v.len();
            v.sort_unstable();
            v.dedup();
            assert_eq!(v.len(), n, "duplicate diagnostics, size {size}");
            v
        };
        assert_eq!(offsets(&forward), offsets(&backward), "size {size}");
        assert_eq!(offsets(&forward).len(), whole.diagnostics.len());
    }

    // Interior anchors split the order exactly, including across a decreasing step.
    let t3 = &whole.events[2];
    let after = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        Some(anchor_of(t3)),
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(ids(&after.events), all_ids[3..].to_vec());
    let before = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        Some(anchor_of(t3)),
        PageDirection::Backward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(ids(&before.events), all_ids[..2].to_vec());
    assert_eq!(
        support::world::try_count(
            &mut f.scanner,
            &s.meta.key,
            &anchor_of(t3),
            BIG,
            support::world::STEPS
        )
        .unwrap(),
        3
    );
    assert_eq!(
        support::world::try_count(
            &mut f.scanner,
            &s.meta.key,
            &anchor_of(&whole.events[0]),
            BIG,
            support::world::STEPS
        )
        .unwrap(),
        5
    );
}

#[test]
fn tool_names_are_cut_to_the_page_budget_and_mark_the_page_truncated() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "name", "/p"),
        item_completed(
            "2026-10-29T10:00:01Z",
            json!({"type": "McpToolCall", "id": "m", "status": "completed",
                   "server": "s".repeat(30), "tool": "t".repeat(30)}),
        ),
        user("2026-10-29T10:00:02Z", "next"),
    ]);
    let request = |max_bytes| PageRequest {
        max_events: 10,
        max_bytes,
    };
    let page = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        None,
        PageDirection::Forward,
        request(10),
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(page.events.len(), 1);
    assert!(page.events[0].tool.as_ref().unwrap().name.len() <= 10);
    assert!(page.truncated && page.has_more);
    // A budget that fits the whole name leaves it intact and unmarked.
    let page = support::world::try_transcript(
        &mut f.scanner,
        &s.meta.key,
        None,
        PageDirection::Forward,
        request(1000),
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    assert_eq!(page.events[0].tool.as_ref().unwrap().name.len(), 61);
    assert!(!page.truncated);
}

#[test]
fn a_completion_claims_its_own_call_not_a_different_open_one() {
    let (mut f, s) = open(&[
        session_meta("2026-10-29T10:00:00Z", "seq", "/p"),
        custom_call("2026-10-29T10:00:01Z", "A", "shell", "first"),
        custom_call("2026-10-29T10:00:02Z", "B", "shell", "second"),
        custom_output("2026-10-29T10:00:03Z", "A", "done A"),
        item_completed(
            "2026-10-29T10:00:04Z",
            command_item("item-a", "first", "done A", 0),
        ),
    ]);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(
        page.events.len(),
        2,
        "B stays pending beside A's completion"
    );
    let pending = page.events[0].tool.as_ref().unwrap();
    assert_eq!(pending.call_id.as_deref(), Some("B"));
    assert_eq!(pending.status, ToolStatus::Pending);
    assert_eq!(
        page.events[1].tool.as_ref().unwrap().status,
        ToolStatus::Completed
    );
    assert_eq!(s.displayed_event_count, 2);

    // B's own output then its completion: one row each, never a stray orphan.
    append(
        &f.path,
        format!(
            "{}\n{}\n",
            custom_output("2026-10-29T10:00:05Z", "B", "done B"),
            item_completed(
                "2026-10-29T10:00:06Z",
                command_item("item-b", "second", "done B", 0)
            ),
        )
        .as_bytes(),
    );
    let s = last_summary(&mut f.scanner);
    assert_eq!(first(&mut f, &s.meta.key).events.len(), 2);
    assert!(
        !s.warnings
            .iter()
            .any(|w| w.key.source.ends_with("#orphan-tool"))
    );
}

fn wide_session() -> Vec<String> {
    let mut lines = vec![session_meta("2026-10-29T10:00:00Z", "wide", "/p")];
    for i in 1..=7 {
        let head = if i == 2 {
            "marker-two".to_owned()
        } else {
            format!("rec{i}")
        };
        lines.push(user(
            &format!("2026-10-29T10:00:{i:02}Z"),
            &format!("{head} {}", "p".repeat(650)),
        ));
    }
    lines
}

fn preview_of(e: &Event) -> &str {
    &e.text.as_ref().unwrap().preview
}

#[test]
fn same_length_replacement_outside_any_sample_is_rejected() {
    let (mut f, s) = open(&wide_session());
    let page = first(&mut f, &s.meta.key); // builds and caches the index
    assert_eq!(page.events.len(), 7);
    let original = std::fs::read_to_string(&f.path).unwrap();
    let at = original.find("marker-two").unwrap();
    // Not within the first, middle, or last 512 bytes.
    assert!(
        at > 600 && (at as f64) < original.len() as f64 * 0.4,
        "at {at}"
    );
    let locator = page.events[3].provenance.locator.clone();

    std::fs::write(&f.path, original.replacen("marker-two", "MARKER-TWO", 1)).unwrap();
    set_modified(&f.path, 1_000_000_000);
    for direction in [PageDirection::Forward, PageDirection::Backward] {
        assert!(
            support::world::try_transcript(
                &mut f.scanner,
                &s.meta.key,
                None,
                direction,
                ALL,
                BIG,
                support::world::STEPS
            )
            .is_err()
        );
    }
    assert!(
        support::world::try_source(&mut f.scanner, &locator, BIG, support::world::STEPS).is_err()
    );
    assert!(
        support::world::try_count(
            &mut f.scanner,
            &s.meta.key,
            &anchor_of(&page.events[0]),
            BIG,
            support::world::STEPS
        )
        .is_err()
    );

    // A refresh restarts the parse and the new bytes are served.
    let changed = scan(&mut f.scanner);
    let s2 = changed.last().expect("session re-emitted");
    assert!(
        s2.warnings
            .iter()
            .any(|w| w.key.source.ends_with("#rotation"))
    );
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 7);
    assert!(preview_of(&page.events[1]).starts_with("MARKER-TWO"));
}

#[test]
fn rewritten_prefix_followed_by_growth_restarts_the_parse() {
    let (mut f, s) = open(&wide_session());
    assert_eq!(first(&mut f, &s.meta.key).events.len(), 7);
    let original = std::fs::read_to_string(&f.path).unwrap();
    let grown = format!(
        "{}{}\n",
        original.replacen("marker-two", "MARKER-TWO", 1),
        user("2026-10-29T10:00:30Z", "appended after the rewrite"),
    );
    assert!(grown.len() > original.len());
    std::fs::write(&f.path, grown).unwrap();
    set_modified(&f.path, 1_000_000_000);

    let changed = scan(&mut f.scanner);
    let s2 = changed.last().expect("session re-emitted");
    assert!(
        s2.warnings
            .iter()
            .any(|w| w.key.source.ends_with("#rotation"))
    );
    assert_eq!(s2.displayed_event_count, 8);
    let page = first(&mut f, &s.meta.key);
    assert_eq!(page.events.len(), 8);
    assert!(preview_of(&page.events[1]).starts_with("MARKER-TWO"));
    assert_eq!(preview_of(&page.events[7]), "appended after the rewrite");

    // Plain growth with an untouched prefix does not restart.
    append(
        &f.path,
        format!("{}\n", user("2026-10-29T10:00:40Z", "more")).as_bytes(),
    );
    let s3 = last_summary(&mut f.scanner);
    assert_eq!(
        s3.warnings
            .iter()
            .find(|w| w.key.source.ends_with("#rotation"))
            .map(|w| w.count),
        Some(1)
    );
    assert_eq!(first(&mut f, &s.meta.key).events.len(), 9);
}

fn numbered(n: usize) -> Vec<String> {
    let mut lines = vec![session_meta("2026-10-29T10:00:00Z", "many", "/p")];
    for i in 0..n {
        lines.push(user(
            &format!("2026-10-29T10:{:02}:{:02}Z", i / 60, i % 60),
            &format!("row {i}"),
        ));
    }
    lines
}

#[test]
fn index_construction_is_bounded_resumable_and_capped() {
    // Resumable: each request indexes at most `step` records, then succeeds.
    let (mut f, s) = open(&numbered(300));
    f.scanner.set_index_limits(100, 10_000);
    let (page, building) = support::world::transcript_steps(
        &mut f.scanner,
        &s.meta.key,
        None,
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    );
    assert_eq!(building, 3, "301 records at 100 per request");
    assert_eq!(page.events.len(), 300);
    assert_eq!(preview_of(&page.events[299]), "row 299");
    let anchor = anchor_of(&page.events[289]);
    assert_eq!(
        support::world::try_count(
            &mut f.scanner,
            &s.meta.key,
            &anchor,
            BIG,
            support::world::STEPS
        )
        .unwrap(),
        10
    );

    // Hard cap: a clear error, repeated cheaply, never a silent unbounded index.
    let (mut f, s) = open(&numbered(300));
    f.scanner.set_index_limits(100, 200);
    for _ in 0..6 {
        match support::world::try_transcript(
            &mut f.scanner,
            &s.meta.key,
            None,
            PageDirection::Backward,
            ALL,
            BIG,
            support::world::STEPS,
        ) {
            Err(ProviderError::Unavailable(_)) => {}
            Err(ProviderError::Unsupported(message)) => {
                assert!(message.contains("200-record index limit"), "{message}");
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    for _ in 0..3 {
        assert!(matches!(
            support::world::try_transcript(
                &mut f.scanner,
                &s.meta.key,
                None,
                PageDirection::Forward,
                ALL,
                BIG,
                support::world::STEPS
            ),
            Err(ProviderError::Unsupported(_))
        ));
        assert!(matches!(
            support::world::try_count(
                &mut f.scanner,
                &s.meta.key,
                &anchor_of(&page.events[0]),
                BIG,
                support::world::STEPS
            ),
            Err(ProviderError::Unsupported(_))
        ));
    }
}
