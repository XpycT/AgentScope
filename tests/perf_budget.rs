//! Deterministic work-budget checks on synthetic corpora.
//!
//! Every assertion counts work (bytes read, records parsed, preview bytes allocated,
//! events retained) through the thread-local counters in `agentscope::perf`; none
//! depends on wall-clock time. `benchmark_report` (ignored by default) prints the
//! timings recorded in `docs/superpowers/notes/2026-10-30-benchmarks.md`.
mod support;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscope::app::{App, MAX_LOADED_EVENTS};
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::*;
use agentscope::ingest::source::SourceReader;
use agentscope::perf;
use agentscope::providers::ProviderReader;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::worker::WorkerRequest;
use serde_json::{Value, json};
use support::codex::{assistant, session_meta, turn_context, usage, usage_record, user};
use support::facts::{self, metadata};
use support::{NOW, config_for, dated, set_modified, write_lines};

const SESSIONS: usize = 2_000;
const RECORDS_PER_SESSION: usize = 20;
const BIG: RefreshBudget = RefreshBudget {
    max_records: 1_000_000,
    max_bytes: 1_000_000_000,
    max_millis: 600_000,
};
/// The scan budget the worker hands each adapter chunk (records and bytes only).
const CHUNK: RefreshBudget = RefreshBudget {
    max_records: 256,
    max_bytes: 64 * 1024,
    max_millis: 600_000,
};
const TS: &str = "2026-10-29T10:00:00Z";

fn codex_lines(index: usize, records: usize) -> Vec<String> {
    let mut lines = vec![
        session_meta(TS, &format!("sess-{index:05}"), "/work/demo"),
        turn_context(TS, "gpt-test", "/work/demo"),
        user(TS, &format!("task {index}")),
    ];
    while lines.len() < records {
        let n = lines.len();
        lines.push(assistant(TS, &format!("reply {n} for session {index}")));
        if lines.len() < records {
            lines.push(usage_record(
                TS,
                usage(10 * n as u64, 0, 5, 0, 11 * n as u64),
            ));
        }
    }
    lines
}

struct Corpus {
    _dir: tempfile::TempDir,
    root: PathBuf,
    /// Paths ordered oldest to newest by modified time.
    files: Vec<PathBuf>,
    total_bytes: u64,
    total_records: u64,
    longest_line: usize,
}

fn codex_corpus(sessions: usize, records: usize) -> Corpus {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let mut corpus = Corpus {
        _dir: dir,
        root: root.clone(),
        files: Vec::new(),
        total_bytes: 0,
        total_records: 0,
        longest_line: 0,
    };
    for i in 0..sessions {
        let path = dated(&root, &format!("rollout-{i:05}.jsonl"));
        let lines = codex_lines(i, records);
        write_lines(&path, &lines);
        set_modified(&path, 1_000_000 + i as u64);
        corpus.total_bytes += fs::metadata(&path).unwrap().len();
        corpus.total_records += lines.len() as u64;
        corpus.longest_line = corpus
            .longest_line
            .max(lines.iter().map(String::len).max().unwrap());
        corpus.files.push(path);
    }
    corpus
}

/// Discovers, then hydrates every session that discovery reported as new or changed.
fn scan_codex(scanner: &mut CodexScanner, budget: RefreshBudget) -> Vec<SessionSummary> {
    support::world::rescan(scanner, NOW, budget).sessions
}

fn append(path: &Path, line: &str) -> u64 {
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
    line.len() as u64 + 1
}

/// Largest read one probe may do: the prefix window, a header line read to its end,
/// and the tail window.
const PROBE_MAX: u64 = 4 * 1024 + 256 * 1024 + 32 * 1024;

#[test]
fn discovery_reads_bounded_probes_under_the_per_call_budget() {
    // Each source is far larger than its probe windows.
    let corpus = codex_corpus(200, 400);
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    let (mut bytes, mut calls) = (0u64, 0usize);
    let mut seen = std::collections::HashSet::new();
    loop {
        perf::reset();
        let delta = scanner.discover(NOW, CHUNK).expect("discover");
        let used = perf::snapshot();
        // A probe that crosses the byte budget finishes; the next call stops short of more.
        assert!(
            used.bytes_read <= CHUNK.max_bytes as u64 + PROBE_MAX,
            "call {calls} read {} bytes",
            used.bytes_read
        );
        bytes += used.bytes_read;
        calls += 1;
        seen.extend(delta.sessions.into_iter().map(|m| m.key));
        if delta.scan.done {
            break;
        }
        assert!(calls < 100_000, "discovery never finished");
    }
    assert_eq!(seen.len(), 200);
    // Probes touch the header window and the tail window, never the body between them.
    assert!(
        bytes <= 200 * (4 * 1024 + 32 * 1024 + 8 * 1024),
        "discovery read {bytes} of {} source bytes",
        corpus.total_bytes
    );
    assert!(bytes < corpus.total_bytes / 2);
}

#[test]
fn hydration_streams_each_file_once_under_per_call_budget() {
    let corpus = codex_corpus(SESSIONS, RECORDS_PER_SESSION);
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), SESSIONS);
    let (mut bytes, mut records, mut calls) = (0, 0, 0usize);
    for meta in &found.sessions {
        let job = support::world::new_job();
        loop {
            perf::reset();
            let delta = scanner
                .hydrate(job, &meta.key, NOW, CHUNK)
                .expect("hydrate");
            let used = perf::snapshot();
            // One record may finish after the budget is crossed; never more.
            assert!(
                used.bytes_read <= (CHUNK.max_bytes + corpus.longest_line + 1) as u64,
                "call {calls} read {} bytes",
                used.bytes_read
            );
            assert!(used.records_parsed <= CHUNK.max_records as u64);
            bytes += used.bytes_read;
            records += used.records_parsed;
            calls += 1;
            if delta.done {
                break;
            }
            assert!(calls < 1_000_000, "hydration never finished");
        }
    }
    // Across the whole corpus every source byte is consumed exactly once.
    assert_eq!(bytes, corpus.total_bytes);
    assert_eq!(records, corpus.total_records);
}

#[test]
fn detail_cache_is_bounded_while_discovery_lists_every_source() {
    let corpus = codex_corpus(SESSIONS + 25, 3);
    let mut config = config_for(&corpus.root);
    config.max_cached_sessions = SESSIONS;
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    // Metadata is not capped by the detail cache.
    assert_eq!(found.sessions.len(), SESSIONS + 25);
    for meta in &found.sessions {
        support::world::hydrate_complete(&mut scanner, &meta.key, NOW);
    }
    assert!(
        scanner.cached_sessions() <= SESSIONS,
        "{} cached",
        scanner.cached_sessions()
    );
}

#[test]
fn unchanged_file_refresh_does_no_full_reparse() {
    let corpus = codex_corpus(SESSIONS, RECORDS_PER_SESSION);
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    let sessions = scan_codex(&mut scanner, BIG);
    assert_eq!(sessions.len(), SESSIONS);

    // Nothing changed: the second pass probes, reads and parses nothing.
    perf::reset();
    let changed = scan_codex(&mut scanner, BIG);
    let idle = perf::snapshot();
    assert!(changed.is_empty(), "unchanged sources must not re-emit");
    assert_eq!((idle.bytes_read, idle.records_parsed), (0, 0));

    // One append to a source whose cursor is held: discovery probes it, and hydration
    // parses only the new line, re-reading the consumed prefix (below the 4 MiB verify
    // limit) once to prove it is unchanged. Both reads are counted.
    let newest = corpus.files.last().unwrap();
    // The newest source is the one being watched, so its cursor is held.
    let watched = sessions
        .iter()
        .find(|s| s.meta.key.source == SourceKey::File(newest.clone()))
        .unwrap()
        .meta
        .key
        .clone();
    support::world::hydrate_complete(&mut scanner, &watched, NOW);
    let before = fs::metadata(newest).unwrap().len();
    let added = append(newest, &assistant(TS, "one more reply"));
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), 1);
    perf::reset();
    let summary = support::world::hydrate_complete(&mut scanner, &found.sessions[0].key, NOW);
    let tail = perf::snapshot();
    assert_eq!(summary.meta.key, found.sessions[0].key);
    // Only the newest 32 sources keep a cursor, and hydration order does not make this
    // one of them, so it is verified once and re-parsed from the start.
    assert_eq!(tail.bytes_read, 2 * before + added);
    assert_eq!(tail.records_parsed, RECORDS_PER_SESSION as u64 + 1);

    // An append to an old source (cursor released to bound open files) re-reads that
    // one file and nothing else: one integrity pass over the old prefix, then a full
    // re-parse of the file from the start.
    let old = &corpus.files[0];
    let old_before = fs::metadata(old).unwrap().len();
    append(old, &assistant(TS, "late reply"));
    let old_len = fs::metadata(old).unwrap().len();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), 1);
    perf::reset();
    support::world::hydrate_complete(&mut scanner, &found.sessions[0].key, NOW);
    let reread = perf::snapshot();
    // Never more than one integrity pass plus one full re-parse of this one file.
    assert!(reread.bytes_read >= old_len && reread.bytes_read <= old_before + old_len);
    // Hydration visited the oldest source last, so its cursor is still held: only the
    // new line is parsed.
    assert_eq!(reread.records_parsed, 1);
}

fn claude_config(root: &Path, tweak: impl FnOnce(&mut Config)) -> Config {
    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root.to_path_buf()),
            ..CliOverrides::default()
        },
        &Env::new(),
    )
    .unwrap();
    tweak(&mut config);
    config
}

fn claude_record(session: &str, i: usize, text: &str) -> Value {
    json!({
        "type": "assistant", "uuid": format!("u{i}"), "sessionId": session,
        "timestamp": TS,
        "message": {
            "id": format!("msg-{i:06}"), "model": "claude-test",
            "content": [{"type": "text", "text": text}],
            "usage": {"input_tokens": 10, "output_tokens": 4,
                      "cache_creation_input_tokens": 3, "cache_read_input_tokens": 2}
        }
    })
}

fn write_claude(root: &Path, name: &str, records: &[Value]) -> PathBuf {
    let path = root.join("project").join(name);
    write_lines(
        &path,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    path
}

fn scan_claude(scanner: &mut ClaudeScanner, budget: RefreshBudget) -> Vec<SessionSummary> {
    support::world::rescan(scanner, NOW, budget).sessions
}

#[test]
fn claude_unchanged_refresh_reads_nothing_and_append_reads_only_the_tail() {
    // Held cursors are bounded by the process file-handle limit, so this corpus stays
    // below the lowest default (256 on macOS).
    const FILES: usize = 100;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let paths: Vec<PathBuf> = (0..FILES)
        .map(|f| {
            let records: Vec<Value> = (0..RECORDS_PER_SESSION)
                .map(|i| claude_record(&format!("s{f}"), f * 1000 + i, "hello"))
                .collect();
            write_claude(&root, &format!("s{f:03}.jsonl"), &records)
        })
        .collect();
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |_| {})).unwrap();
    // Discovery probes (small files are read whole by a probe) are not hydration work.
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), FILES);
    perf::reset();
    for meta in &found.sessions {
        support::world::hydrate_complete(&mut scanner, &meta.key, NOW);
    }
    let total: u64 = paths.iter().map(|p| fs::metadata(p).unwrap().len()).sum();
    assert_eq!(perf::snapshot().bytes_read, total);
    assert_eq!(
        perf::snapshot().records_parsed,
        (FILES * RECORDS_PER_SESSION) as u64
    );

    perf::reset();
    assert!(scan_claude(&mut scanner, BIG).is_empty());
    assert_eq!(perf::snapshot(), perf::Counters::default());

    let prefix = fs::metadata(&paths[7]).unwrap().len();
    let added = append(&paths[7], &claude_record("s7", 999_999, "tail").to_string());
    let changed = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(changed.sessions.len(), 1);
    perf::reset();
    support::world::hydrate_complete(&mut scanner, &changed.sessions[0].key, NOW);
    let tail = perf::snapshot();
    assert_eq!(tail.records_parsed, 1);
    // The new line plus an exact verification of the consumed prefix, which
    // detects in-place rewrites that also grow the file. Unchanged files still
    // read nothing (asserted above).
    assert_eq!(
        tail.bytes_read,
        added + prefix,
        "read {} bytes for {added} appended after a {prefix}-byte prefix",
        tail.bytes_read
    );
}

fn page_cost(page: &TranscriptPage) -> usize {
    let len = |t: &Option<BoundedText>| t.as_ref().map_or(0, |b| b.preview.len());
    page.events
        .iter()
        .map(|e| {
            len(&e.text)
                + e.tool
                    .as_ref()
                    .map_or(0, |t| t.name.len() + len(&t.input) + len(&t.output))
        })
        .sum()
}

/// Source size of each oversized row in the page test.
const HUGE_ROW: u64 = 100_000;

/// Largest preview any adapter allocates for one field.
const PREVIEW_CAP: u64 = 2048;

fn check_page_budget(
    reader: &mut dyn ProviderReader,
    key: &SessionKey,
    file_len: u64,
    max_bytes: usize,
) {
    let request = PageRequest {
        max_events: 500,
        max_bytes,
    };
    perf::reset();
    let page = support::world::try_transcript(
        reader,
        key,
        None,
        PageDirection::Forward,
        request,
        BIG,
        support::world::STEPS,
    )
    .expect("page");
    let used = perf::snapshot();
    assert!(!page.events.is_empty());
    assert!(
        page_cost(&page) <= max_bytes,
        "page exceeds {max_bytes} bytes"
    );
    assert!(page.truncated && page.has_more);
    // Retained previews: each body is cut at the 2 KiB cap before it is kept, so a small
    // page budget can still hold one cap-sized preview per row visited (the returned
    // rows plus the one that did not fit), plus short per-row metadata.
    const ROW_METADATA: u64 = 64;
    let visited = page.events.len() as u64 + 1;
    assert!(used.max_preview_bytes <= PREVIEW_CAP);
    assert!(
        used.preview_bytes <= visited * (PREVIEW_CAP + ROW_METADATA),
        "{:?} kept {} preview bytes for a {max_bytes}-byte page",
        reader.harness(),
        used.preview_bytes
    );
    // Transient buffers: sanitize/redact run over the whole source text before the cut,
    // so each visited row costs about two source-sized buffers (bounded by the 1 MiB
    // record cap). This, not the 2 KiB cap, is the real peak allocation per row; the
    // page byte cap bounds what is kept, not what is built.
    assert!(
        used.preview_temp_bytes <= visited * (2 * HUGE_ROW + 2 * ROW_METADATA),
        "{:?} built {} temporary preview bytes",
        reader.harness(),
        used.preview_temp_bytes
    );
    // Paging touches only this session file: at most one verification pass, one index
    // pass, and a re-read of each row it builds (1 MiB is the record cap).
    let bound = 2 * file_len + visited * 1_048_576;
    assert!(used.bytes_read <= bound, "read {} bytes", used.bytes_read);
}

#[test]
fn page_allocation_never_exceeds_max_page_bytes() {
    let huge = "x".repeat(HUGE_ROW as usize);
    let dir = tempfile::tempdir().unwrap();

    // Codex: 40 rows of 100 KB each.
    let root = dir.path().join("codex");
    let mut lines = vec![session_meta(TS, "oversized", "/work/demo")];
    lines.extend((0..40).map(|_| assistant(TS, &huge)));
    let path = dated(&root, "rollout-big.jsonl");
    write_lines(&path, &lines);
    let len = fs::metadata(&path).unwrap().len();
    let mut scanner = CodexScanner::new(&config_for(&root)).unwrap();
    let sessions = scan_codex(&mut scanner, BIG);
    let key = sessions[0].meta.key.clone();
    for max_bytes in [8192, 1000] {
        check_page_budget(&mut scanner, &key, len, max_bytes);
    }

    // Claude: same shape.
    let root = dir.path().join("projects");
    let records: Vec<Value> = (0..40).map(|i| claude_record("big", i, &huge)).collect();
    let path = write_claude(&root, "big.jsonl", &records);
    let len = fs::metadata(&path).unwrap().len();
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |_| {})).unwrap();
    let key = scan_claude(&mut scanner, BIG).remove(0).meta.key;
    for max_bytes in [8192, 1000] {
        check_page_budget(&mut scanner, &key, len, max_bytes);
    }
}

#[test]
fn first_page_reads_one_session_not_the_corpus() {
    let corpus = codex_corpus(SESSIONS, RECORDS_PER_SESSION);
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    let sessions = scan_codex(&mut scanner, BIG);
    let target = sessions
        .iter()
        .find(|s| s.meta.key.native_id == "sess-01234")
        .unwrap();
    let len = fs::metadata(&corpus.files[1234]).unwrap().len();
    perf::reset();
    let page = support::world::try_transcript(
        &mut scanner,
        &target.meta.key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 500,
            max_bytes: 262_144,
        },
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    let used = perf::snapshot();
    assert!(!page.events.is_empty());
    assert!(
        used.bytes_read <= 2 * len,
        "read {} of {len}",
        used.bytes_read
    );
    assert!(used.records_parsed <= 2 * RECORDS_PER_SESSION as u64);
}

#[test]
fn dedup_ledger_stays_within_entry_and_byte_caps() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let records: Vec<Value> = (0..5_000)
        .map(|i| claude_record("ledger", i, "hi"))
        .collect();
    write_claude(&root, "ledger.jsonl", &records);
    let per_message = 10 + 4 + 3 + 2;

    // Entry cap: later messages are not counted, and the aggregate says so.
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |c| {
        c.dedup_ledger_entries = 1_000;
    }))
    .unwrap();
    let usage = scan_claude(&mut scanner, BIG).remove(0).usage.unwrap();
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.total, Some(1_000 * per_message));

    // Byte cap: every retained byte of an entry is charged (state, key, strings), so
    // 100,000 bytes hold a bounded, nonzero prefix and the aggregate says it is partial.
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |c| {
        c.dedup_ledger_bytes = 100_000;
    }))
    .unwrap();
    let usage = scan_claude(&mut scanner, BIG).remove(0).usage.unwrap();
    assert_eq!(usage.class, AggregationClass::Partial);
    let counted = usage.total.unwrap() / per_message;
    assert!(
        (10..1_000).contains(&counted),
        "counted {counted} messages in a 100,000 byte budget"
    );
    assert_eq!(usage.total, Some(counted * per_message));

    // Within the caps, the same file stays exact.
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |_| {})).unwrap();
    let usage = scan_claude(&mut scanner, BIG).remove(0).usage.unwrap();
    assert_ne!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.total, Some(5_000 * per_message));
}

fn view_key() -> SessionKey {
    SessionKey {
        harness: Harness::Codex,
        native_id: "loaded".into(),
        source: SourceKey::File("loaded.jsonl".into()),
    }
}

fn view_summary() -> SessionSummary {
    let mut meta = metadata(view_key(), "Loaded");
    meta.created_at = Some(NOW - 86_400_000);
    meta.activity = ActivityStamp {
        at: Some(NOW - 540_000),
        class: AggregationClass::Reported,
    };
    SessionSummary {
        last_event_at: Some(NOW - 540_000),
        last_completion: None,
        unfinished_work: false,
        displayed_event_count: 100_000,
        hidden_records: 0,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::NotChecked,
        usage: None,
        warnings: vec![],
        ..facts::summary(meta)
    }
}

fn view_event(i: u64) -> Event {
    Event {
        id: format!("e{i}"),
        session: view_key(),
        timestamp: Some(NOW - 3_000_000 + i as i64 * 1_000),
        source_order: i,
        kind: EventKind::AssistantMessage,
        actor: Actor::Assistant,
        text: Some(BoundedText {
            preview: format!("message {i}"),
            byte_len: 12,
            truncated: false,
            locator: None,
        }),
        tool: None,
        usage: None,
        provenance: Provenance {
            harness: Harness::Codex,
            source: SourceKey::File("loaded.jsonl".into()),
            locator: SourceLocator {
                session: view_key(),
                kind: LocatorKind::FileOffset {
                    path: "loaded.jsonl".into(),
                    offset: i,
                },
                generation: None,
            },
            record_type: "test".into(),
            harness_version: None,
        },
    }
}

fn view_anchor(i: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(NOW - 3_000_000 + i as i64 * 1_000),
        source_order: i,
        stable_id: format!("e{i}"),
    }
}

fn view_page(range: std::ops::Range<u64>) -> TranscriptPage {
    TranscriptPage {
        events: range.clone().map(view_event).collect(),
        diagnostics: vec![],
        next: Some(view_anchor(range.start)),
        has_more: true,
        truncated: false,
    }
}

#[test]
fn loaded_transcript_stays_within_event_cap() {
    const PAGE: u64 = 500;
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![view_summary()], vec![]);
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("expected Select, got {requests:?}");
    };
    let total = 5 * MAX_LOADED_EVENTS as u64 / 2;
    let mut start = total - PAGE;
    assert!(app.apply_transcript(*request_id, view_page(start..total)));

    // Keep scrolling to the start and serving the older page until 2.5x the cap
    // has been delivered; the retained window never grows past the cap.
    let mut delivered = PAGE;
    let mut high_water = app.transcript().unwrap().events().len();
    while start > 0 && delivered < total {
        app.inspector_move(isize::MIN);
        let load = app
            .take_requests()
            .into_iter()
            .find_map(|r| match r {
                WorkerRequest::LoadPage { request_id, .. } => Some(request_id),
                _ => None,
            })
            .expect("scrolling to the start requests the older page");
        start = start.saturating_sub(PAGE);
        let end = (start + PAGE).min(total);
        assert!(app.apply_transcript(load, view_page(start..end)));
        delivered += end - start;
        let held = app.transcript().unwrap().events().len();
        assert!(held <= MAX_LOADED_EVENTS, "{held} events retained");
        high_water = high_water.max(held);
    }
    assert_eq!(
        high_water, MAX_LOADED_EVENTS,
        "the window did fill to the cap"
    );
}

/// Prints the timings recorded in the benchmark notes. Not a pass/fail check:
/// `cargo test --release --test perf_budget -- --ignored --nocapture benchmark_report`.
#[test]
#[ignore = "timing record; run manually"]
fn benchmark_report() {
    let build = Instant::now();
    let corpus = codex_corpus(SESSIONS, RECORDS_PER_SESSION);
    println!(
        "corpus: {SESSIONS} codex sessions, {} bytes, {} records, built in {:?}",
        corpus.total_bytes,
        corpus.total_records,
        build.elapsed()
    );

    // Initial roster: time to the first delta and to a complete pass, with the
    // worker's chunk shape (256 records per call).
    let worker_chunk = RefreshBudget {
        max_records: 256,
        max_bytes: 8 * 1024 * 1024,
        max_millis: 100,
    };
    let mut scanner = CodexScanner::new(&config_for(&corpus.root)).unwrap();
    perf::reset();
    let start = Instant::now();
    let first = scanner.discover(NOW, worker_chunk).unwrap();
    let first_delta = start.elapsed();
    let first_count = first.sessions.len();
    let mut keys: Vec<SessionKey> = first.sessions.into_iter().map(|m| m.key).collect();
    let mut calls = 1;
    let mut worst_call = first_delta;
    let mut done = first.scan.done;
    while !done {
        let call = Instant::now();
        let delta = scanner.discover(NOW, worker_chunk).unwrap();
        worst_call = worst_call.max(call.elapsed());
        keys.extend(delta.sessions.into_iter().map(|m| m.key));
        done = delta.scan.done;
        calls += 1;
    }
    println!(
        "codex metadata discovery: first delta {first_delta:?} ({first_count} sources), complete {:?} in {calls} calls, \
         longest call {worst_call:?}, {} bytes read, {} records parsed",
        start.elapsed(),
        perf::snapshot().bytes_read,
        perf::snapshot().records_parsed
    );
    perf::reset();
    let start = Instant::now();
    let sessions: Vec<SessionSummary> = keys
        .iter()
        .map(|k| {
            support::world::hydrate_budgeted(
                &mut scanner,
                k,
                NOW,
                worker_chunk,
                support::world::STEPS,
            )
        })
        .collect();
    println!(
        "codex hydrating all {} sessions: {:?}, {} bytes read, {} records parsed",
        sessions.len(),
        start.elapsed(),
        perf::snapshot().bytes_read,
        perf::snapshot().records_parsed
    );

    let key = sessions
        .iter()
        .find(|s| s.meta.key.native_id == "sess-01234")
        .unwrap()
        .meta
        .key
        .clone();
    perf::reset();
    let start = Instant::now();
    let page = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 500,
            max_bytes: 262_144,
        },
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    println!(
        "codex first page: {:?}, {} events, {} bytes read",
        start.elapsed(),
        page.events.len(),
        perf::snapshot().bytes_read
    );

    perf::reset();
    let start = Instant::now();
    let changed = scan_codex(&mut scanner, worker_chunk);
    println!(
        "codex unchanged refresh: {:?}, {} changed, {} bytes read, {} records parsed",
        start.elapsed(),
        changed.len(),
        perf::snapshot().bytes_read,
        perf::snapshot().records_parsed
    );

    append(
        corpus.files.last().unwrap(),
        &assistant(TS, "one more reply"),
    );
    perf::reset();
    let start = Instant::now();
    let changed = scan_codex(&mut scanner, worker_chunk);
    println!(
        "codex refresh after one append: {:?}, {} changed, {} bytes read, {} records parsed",
        start.elapsed(),
        changed.len(),
        perf::snapshot().bytes_read,
        perf::snapshot().records_parsed
    );

    // Claude over a corpus below the lowest default file-handle limit.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    for f in 0..200 {
        let records: Vec<Value> = (0..RECORDS_PER_SESSION)
            .map(|i| claude_record(&format!("s{f}"), f * 1000 + i, "hello"))
            .collect();
        write_claude(&root, &format!("s{f:03}.jsonl"), &records);
    }
    let mut scanner = ClaudeScanner::new(&claude_config(&root, |_| {})).unwrap();
    perf::reset();
    let start = Instant::now();
    let sessions = scan_claude(&mut scanner, worker_chunk);
    println!(
        "claude initial roster (200 sessions): {:?}, {} sessions, {} bytes read",
        start.elapsed(),
        sessions.len(),
        perf::snapshot().bytes_read
    );
    let key = sessions[0].meta.key.clone();
    perf::reset();
    let start = Instant::now();
    let page = support::world::try_transcript(
        &mut scanner,
        &key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 500,
            max_bytes: 262_144,
        },
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    println!(
        "claude first page: {:?}, {} events, {} bytes read",
        start.elapsed(),
        page.events.len(),
        perf::snapshot().bytes_read
    );
    perf::reset();
    let start = Instant::now();
    let changed = scan_claude(&mut scanner, worker_chunk);
    println!(
        "claude unchanged refresh: {:?}, {} changed, {} bytes read",
        start.elapsed(),
        changed.len(),
        perf::snapshot().bytes_read
    );
}

/// Tabs expand to up to four spaces in `sanitize`, so a tab-heavy row builds buffers
/// larger than its source: the expanded output, then redaction's copy of it.
#[test]
fn preview_temporaries_include_tab_expansion_and_redaction_output() {
    const TABS: u64 = 25_000;
    const EXPANDED: u64 = 4 * TABS;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    let mut lines = vec![session_meta(TS, "tabs", "/work/demo")];
    lines.extend((0..10).map(|_| assistant(TS, &"\t".repeat(TABS as usize))));
    write_lines(&dated(&root, "rollout-tabs.jsonl"), &lines);
    let mut scanner = CodexScanner::new(&config_for(&root)).unwrap();
    let sessions = scan_codex(&mut scanner, BIG);
    perf::reset();
    let page = support::world::try_transcript(
        &mut scanner,
        &sessions[0].meta.key,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 500,
            max_bytes: 1000,
        },
        BIG,
        support::world::STEPS,
    )
    .unwrap();
    let used = perf::snapshot();
    let visited = page.events.len() as u64 + 1;
    // Retained previews stay at the cap...
    assert!(used.max_preview_bytes <= PREVIEW_CAP);
    // ...but each visited row built at least the expanded buffer plus redaction's
    // owned copy (2 x 4N), and at most a grown sanitize buffer (< 2 x 4N) plus the copy.
    assert!(
        used.preview_temp_bytes >= visited * 2 * EXPANDED,
        "only {} temporary bytes counted for {visited} rows",
        used.preview_temp_bytes
    );
    assert!(
        used.preview_temp_bytes <= visited * 3 * EXPANDED + visited * 1024,
        "{} temporary bytes for {visited} rows",
        used.preview_temp_bytes
    );
}

/// A4: archived rollouts are consumed in bounded slices, never as one expansion, and
/// plain and compressed reads do the same record work.
#[test]
fn compressed_source_work_stays_within_each_call_budget() {
    let dir = tempfile::tempdir().unwrap();
    let mut plain = String::new();
    for index in 0..60_000 {
        plain.push_str(&format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\
             \"content\":[{{\"type\":\"input_text\",\"text\":\"budgeted {index}\"}}]}}}}\n"
        ));
    }
    let plain_path = dir.path().join("budgeted.jsonl");
    let archived_path = dir.path().join("budgeted.jsonl.zst");
    fs::write(&plain_path, &plain).unwrap();
    fs::write(
        &archived_path,
        zstd::stream::encode_all(plain.as_bytes(), 3).unwrap(),
    )
    .unwrap();

    let budget = RefreshBudget {
        max_records: 256,
        max_bytes: 256 * 1024,
        max_millis: 100,
    };
    // Below the byte allowance, so a slice holds many records in both representations.
    let max_record_bytes = 4096;
    let drain = |path: &Path| -> (usize, usize, usize) {
        let mut reader =
            SourceReader::open(path, max_record_bytes).expect("open the synthetic source");
        let mut slices = 0;
        let mut expanded = 0;
        let mut records = 0;
        loop {
            let batch = reader.step(budget).expect("bounded step");
            assert!(
                batch.expanded_bytes <= budget.max_bytes,
                "one call expanded {} bytes of a {} byte history",
                batch.expanded_bytes,
                plain.len()
            );
            slices += 1;
            expanded += batch.expanded_bytes;
            records += batch.records.len();
            if batch.done {
                return (slices, expanded, records);
            }
            assert!(slices < 100_000, "no progress");
        }
    };

    perf::reset();
    let (slices, expanded, records) = drain(&archived_path);
    let archived = perf::snapshot();
    // The expansion is many slices, each within one call's allowance.
    assert!(slices >= 20, "{slices} slices");
    assert_eq!(expanded, plain.len());
    assert_eq!(archived.records_parsed, 60_000);
    assert_eq!(records, 60_000);

    perf::reset();
    let (plain_slices, plain_expanded, plain_records) = drain(&plain_path);
    let plain_work = perf::snapshot();
    assert!(plain_slices >= 20, "{plain_slices} slices");
    assert_eq!(plain_expanded, plain.len());
    assert_eq!(plain_records, records);
    assert_eq!(plain_work.records_parsed, archived.records_parsed);
    // A plain source reads exactly its own bytes; nothing is re-read.
    assert_eq!(plain_work.bytes_read, plain.len() as u64);
}

/// B7: the integrated native world (125 roots, families, a zstd archive, a truncated
/// archive, damaged and partial records) is discovered and hydrated in bounded calls.
#[test]
fn native_world_discovery_and_hydration_stay_within_each_call_budget() {
    use support::world::{ROOT_COUNT, World};

    let world = World::native();
    let g = world.grown();
    let record_cap = world.config().max_record_bytes;
    let mut readers = world.readers();
    let mut sessions = 0;
    // Codex and Claude are file readers; the OpenCode database is covered by its own tests.
    for reader in readers.iter_mut().take(2) {
        let mut keys = Vec::new();
        let mut calls = 0usize;
        loop {
            perf::reset();
            let delta = reader.discover(NOW, CHUNK).expect("discover");
            let used = perf::snapshot();
            assert!(
                used.bytes_read <= CHUNK.max_bytes as u64 + PROBE_MAX,
                "call {calls} read {} bytes",
                used.bytes_read
            );
            keys.extend(delta.sessions.into_iter().map(|m| m.key));
            calls += 1;
            if delta.scan.done {
                break;
            }
            assert!(calls < 100_000, "discovery never finished");
        }
        keys.sort_by(agentscope::roster::cmp_keys);
        keys.dedup();
        for key in &keys {
            let job = support::world::new_job();
            let mut steps = 0usize;
            loop {
                perf::reset();
                let delta = reader.hydrate(job, key, NOW, CHUNK).expect("hydrate");
                let used = perf::snapshot();
                assert!(
                    used.bytes_read <= (CHUNK.max_bytes + record_cap + 1) as u64,
                    "{} read {} bytes in one call",
                    key.native_id,
                    used.bytes_read
                );
                assert!(used.records_parsed <= CHUNK.max_records as u64);
                steps += 1;
                if delta.done {
                    break;
                }
                assert!(
                    steps < 100_000,
                    "{} never finished hydrating",
                    key.native_id
                );
            }
        }
        sessions += keys.len();
    }
    // Every Codex and Claude root and child was reached; OpenCode owns the rest.
    let opencode = g
        .roots
        .iter()
        .map(|(k, _)| k)
        .chain(g.children.iter().map(|(k, _)| k))
        .filter(|k| k.harness == Harness::OpenCode)
        .count();
    assert_eq!(sessions, ROOT_COUNT + g.children.len() - opencode);
}
