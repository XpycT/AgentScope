//! Regressions for the B1 review: periodic discovery, frozen limits, detached
//! cancellation, bounded adapter bookkeeping, resumable details, frozen app order,
//! deletion availability, independent indexes, child caches and bounded enumeration.

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentscope::app::App;
use agentscope::domain::*;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::{DiscoveryDelta, ProviderReader, ReadProgress};
use agentscope::roster::{MetadataLimits, RosterIndex};
use agentscope::worker::{WorkerEvent, WorkerHandle, WorkerRequest};
use support::facts::{file_key, metadata, summary};
use support::{NOW, config_for, dated, write_lines};

const BASE: i64 = 1_793_000_000_000;
const BIG: RefreshBudget = RefreshBudget {
    max_records: 1_000_000,
    max_bytes: 1_000_000_000,
    max_millis: 600_000,
};

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn meta(id: &str, at: i64) -> SessionMetadata {
    let mut m = metadata(file_key(Harness::Codex, id, &PathBuf::from(id)), id);
    m.activity = ActivityStamp {
        at: Some(at),
        class: AggregationClass::Reported,
    };
    m
}

fn delta(sessions: Vec<SessionMetadata>) -> DiscoveryDelta {
    DiscoveryDelta {
        sessions,
        removed: vec![],
        invalidated: vec![],
        warnings: vec![],
        scan: ScanProgress {
            harness: Harness::Codex,
            scanned: 0,
            total: None,
            done: true,
        },
    }
}

// 1. Periodic discovery merges into the frozen generation; only Refresh freezes anew.
#[test]
fn periodic_discovery_never_refreezes_the_generation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    for i in 0..70i64 {
        let id = format!("c{i:03}");
        write_lines(
            &dated(&root, &format!("rollout-{id}.jsonl")),
            &[support::codex::session_meta(
                &iso(BASE + i * 1000),
                &id,
                "/w",
            )],
        );
    }
    let mut config = config_for(&root);
    config.refresh_ms = 30;
    config.process_enabled = false;
    let worker = WorkerHandle::spawn(vec![Box::new(CodexScanner::new(&config).unwrap())], config);
    let wait = |pred: &dyn Fn(&WorkerEvent) -> bool| {
        let end = Instant::now() + Duration::from_secs(30);
        while Instant::now() < end {
            if let Ok(e) = worker.rx.recv_timeout(Duration::from_millis(20))
                && pred(&e)
            {
                return;
            }
        }
        panic!("timeout");
    };
    wait(
        &|e| matches!(e, WorkerEvent::Roster { snapshot, .. } if snapshot.generation == 1 && !snapshot.provisional),
    );
    worker
        .tx
        .send(WorkerRequest::LoadRosterMore { generation: 1 })
        .unwrap();
    wait(&|e| matches!(e, WorkerEvent::Roster { snapshot, .. } if snapshot.roots.len() == 70));
    // Many periodic passes later the generation and the revealed roots are unchanged.
    let end = Instant::now() + Duration::from_millis(600);
    let mut finished = 0;
    while Instant::now() < end {
        match worker.rx.recv_timeout(Duration::from_millis(20)) {
            Ok(WorkerEvent::Roster { snapshot, .. }) => {
                assert_eq!(snapshot.generation, 1, "a timer pass must not freeze");
                assert_eq!(snapshot.roots.len(), 70);
            }
            Ok(WorkerEvent::RefreshFinished { generation }) => {
                assert_eq!(generation, 1);
                finished += 1;
            }
            _ => {}
        }
    }
    assert!(finished >= 2, "periodic passes did run ({finished})");
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    wait(&|e| matches!(e, WorkerEvent::RefreshFinished { generation } if *generation == 2));
    assert!(worker.shutdown());
}

// 2. A limited, frozen generation still admits newer sources for the next refresh.
#[test]
fn newer_sources_are_admitted_after_a_limited_generation_freezes() {
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 10,
            max_bytes: 64 << 20,
        },
        100,
    );
    index.apply_discovery(delta(
        (0..10)
            .map(|i| meta(&format!("old{i}"), BASE + i))
            .collect(),
    ));
    index.freeze(1);
    let before = index.snapshot();
    index.apply_discovery(delta(vec![meta("newest", BASE + 1_000)]));
    let during = index.snapshot();
    assert_eq!(
        during.roots.as_ref(),
        before.roots.as_ref(),
        "the frozen page is stable"
    );
    assert!(during.refresh_available, "the newer source is advertised");
    index.freeze(2);
    let after = index.snapshot();
    assert_eq!(after.roots[0].native_id, "newest");
    assert_eq!(index.len(), 10, "a refresh trims back to the ceiling");
    assert!(after.limited.is_some());
}

// 8. A deleted hydrated row no longer looks available.
#[test]
fn deleting_a_hydrated_member_updates_its_cached_summary() {
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 100,
            max_bytes: 64 << 20,
        },
        100,
    );
    let m = meta("a", BASE);
    index.apply_discovery(delta(vec![m.clone()]));
    index.update_summary(summary(m.clone()));
    index.freeze(1);
    let mut gone = delta(vec![]);
    gone.removed = vec![m.key.clone()];
    index.apply_discovery(gone);
    let snap = index.snapshot();
    assert!(!snap.metadata[0].source_available);
    assert!(
        !snap.summaries[0].meta.source_available,
        "the cached summary follows"
    );
}

// 7 + 11. App keeps the frozen group order and drops stale child caches.
fn app() -> App {
    App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    )
}

fn scan() -> ScanProgress {
    ScanProgress {
        harness: Harness::Codex,
        scanned: 2,
        total: Some(2),
        done: true,
    }
}

#[test]
fn the_app_keeps_the_frozen_group_order_and_generation_tags_child_pages() {
    let mut app = app();
    // `old-parent` is listed first because a newer child promoted its group.
    let parent = summary(meta("old-parent", BASE));
    let newer = summary(meta("newer-root", BASE + 5_000));
    assert!(app.apply_roster_snapshot(
        support::snapshot(1, vec![parent.clone(), newer.clone()], scan()),
        vec![]
    ));
    let order: Vec<String> = app
        .visible_rows()
        .iter()
        .map(|r| r.session.meta.key.native_id.clone())
        .collect();
    assert_eq!(order, ["old-parent", "newer-root"]);

    assert!(app.load_children(&parent.meta.key));
    let request_id = match app
        .take_requests()
        .into_iter()
        .find(|r| matches!(r, WorkerRequest::LoadChildren { .. }))
    {
        Some(WorkerRequest::LoadChildren { request_id, .. }) => request_id,
        other => panic!("{other:?}"),
    };
    // A refresh arrives before the reply: the reply belongs to the old generation.
    assert!(app.apply_roster_snapshot(
        support::snapshot(2, vec![parent.clone(), newer.clone()], scan()),
        vec![]
    ));
    let child = std::sync::Arc::new(meta("kid", BASE));
    assert!(!app.apply_children(request_id, &parent.meta.key, vec![child.clone()], None));
    assert!(app.children_of(&parent.meta.key).is_none());
    // A fresh request of the current generation is accepted.
    assert!(app.load_children(&parent.meta.key));
    let id = match app
        .take_requests()
        .into_iter()
        .find(|r| matches!(r, WorkerRequest::LoadChildren { .. }))
    {
        Some(WorkerRequest::LoadChildren { request_id, .. }) => request_id,
        other => panic!("{other:?}"),
    };
    assert!(app.apply_children(id, &parent.meta.key, vec![child], None));
    assert!(
        !app.apply_children(id, &parent.meta.key, vec![], None),
        "a reply counts once"
    );
}

// 3. A CancelLoad for a load already detached from the queue is honoured.
#[test]
fn cancelling_a_queued_load_while_another_runs_prevents_its_publication() {
    use std::sync::mpsc;
    let (entered_tx, entered) = mpsc::channel();
    let (release, gate_rx) = mpsc::channel();
    let (disc_entered_tx, disc_entered) = mpsc::channel();
    let (disc_release, disc_gate) = mpsc::channel();
    let reader = Gated {
        page: (entered_tx, gate_rx),
        discover: (disc_entered_tx, disc_gate),
    };
    let mut config = support::config_for(&PathBuf::from("none"));
    config.refresh_ms = 3_600_000;
    config.process_enabled = false;
    let worker = WorkerHandle::spawn(vec![Box::new(reader)], config);
    disc_entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let key = file_key(Harness::Codex, "a", &PathBuf::from("a"));
    // Both are queued before either is serviced.
    worker
        .tx
        .send(WorkerRequest::Select {
            request_id: 1,
            key: key.clone(),
        })
        .unwrap();
    worker
        .tx
        .send(WorkerRequest::LoadSource {
            request_id: 5,
            locator: SourceLocator {
                session: key,
                kind: LocatorKind::FileOffset {
                    path: "a".into(),
                    offset: 0,
                },
                generation: None,
            },
        })
        .unwrap();
    disc_release.send(()).unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap(); // the page is inside the adapter
    worker
        .tx
        .send(WorkerRequest::CancelLoad { request_id: 5 })
        .unwrap();
    release.send(()).unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    let mut saw_page = false;
    while Instant::now() < end {
        match worker.rx.recv_timeout(Duration::from_millis(50)) {
            Ok(WorkerEvent::Transcript { request_id: 1, .. }) => saw_page = true,
            Ok(WorkerEvent::Source { request_id: 5, .. }) => panic!("a cancelled load published"),
            _ => {}
        }
    }
    assert!(saw_page);
    assert!(worker.shutdown());
}

struct Gated {
    page: (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>),
    discover: (std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>),
}

impl ProviderReader for Gated {
    fn harness(&self) -> Harness {
        Harness::Codex
    }
    fn discover(
        &mut self,
        _: Timestamp,
        _: RefreshBudget,
    ) -> Result<DiscoveryDelta, agentscope::providers::ProviderError> {
        let _ = self.discover.0.send(());
        let _ = self.discover.1.recv_timeout(Duration::from_secs(5));
        Ok(delta(vec![]))
    }
    fn hydrate(
        &mut self,
        _: agentscope::providers::ReadJobId,
        _: &SessionKey,
        _: Timestamp,
        _: RefreshBudget,
    ) -> Result<agentscope::providers::HydrationDelta, agentscope::providers::ProviderError> {
        Err(agentscope::providers::ProviderError::Unavailable(
            "none".into(),
        ))
    }
    fn cancel_read(&mut self, _: &SessionKey, _: agentscope::providers::ReadJobId) {}
    fn details_page(
        &mut self,
        _: agentscope::providers::ReadJobId,
        _: &SessionKey,
        _: Option<PageAnchor>,
        _: PageRequest,
        _: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, agentscope::providers::ProviderError> {
        Ok(ReadProgress::Ready(SessionDetailPage::default()))
    }
    fn transcript_page(
        &mut self,
        _: agentscope::providers::ReadJobId,
        _: &SessionKey,
        _: Option<PageAnchor>,
        _: PageDirection,
        _: PageRequest,
        _: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, agentscope::providers::ProviderError> {
        let _ = self.page.0.send(());
        let _ = self.page.1.recv_timeout(Duration::from_secs(5));
        Ok(ReadProgress::Ready(TranscriptPage {
            events: vec![],
            diagnostics: vec![],
            next: None,
            has_more: false,
            truncated: false,
        }))
    }
    fn count_events_after(
        &mut self,
        _: agentscope::providers::ReadJobId,
        _: &SessionKey,
        _: &PageAnchor,
        _: RefreshBudget,
    ) -> Result<ReadProgress<usize>, agentscope::providers::ProviderError> {
        Ok(ReadProgress::Ready(0))
    }
    fn source_record(
        &mut self,
        _: agentscope::providers::ReadJobId,
        _: &SourceLocator,
        _: RefreshBudget,
    ) -> Result<
        ReadProgress<agentscope::providers::SourceRecord>,
        agentscope::providers::ProviderError,
    > {
        Ok(ReadProgress::Ready(agentscope::providers::SourceRecord {
            raw: String::new(),
            display: String::new(),
            unavailable: None,
        }))
    }
}

// 4 + P1. Adapter bookkeeping obeys the ceiling; eviction is not deletion.
#[test]
fn adapter_bookkeeping_is_bounded_and_an_evicted_frozen_row_stays_readable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    let write = |i: i64| {
        let id = format!("c{i:02}");
        write_lines(
            &dated(&root, &format!("rollout-{id}.jsonl")),
            &[
                support::codex::session_meta(&iso(BASE + i * 1000), &id, "/w"),
                support::codex::user(&iso(BASE + i * 1000), "task"),
            ],
        );
    };
    for i in 0..5 {
        write(i);
    }
    let mut config = config_for(&root);
    config.max_metadata_sources = 5;
    let mut scanner = CodexScanner::new(&config).unwrap();
    let first = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(first.sessions.len(), 5);
    // The frozen row is the oldest one.
    let frozen = first
        .sessions
        .iter()
        .find(|m| m.key.native_id == "c00")
        .unwrap()
        .key
        .clone();
    // Three newer sources evict its bookkeeping.
    for i in 5..8 {
        write(i);
    }
    let second = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert!(
        scanner.tracked_sources() <= 5,
        "{} tracked",
        scanner.tracked_sources()
    );
    assert!(
        second
            .warnings
            .iter()
            .any(|w| w.message.contains("Discovery limited"))
    );
    assert!(second.removed.is_empty(), "eviction is not a deletion");
    // The frozen row is still readable: hydrate and open it.
    let summary = support::world::hydrate_complete(&mut scanner, &frozen, NOW);
    assert_eq!(summary.meta.key, frozen);
    let page = support::world::transcript_ready(
        &mut scanner,
        &frozen,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 10,
            max_bytes: 1 << 20,
        },
        BIG,
        support::world::STEPS,
    );
    assert_eq!(page.events.len(), 1);
    // A genuinely deleted file still fails.
    std::fs::remove_file(dated(&root, "rollout-c00.jsonl")).unwrap();
    assert!(support::world::try_hydrate(&mut scanner, &frozen, NOW, BIG, 10).is_err());
}

// 3. Retention is bounded by bytes as well as count.
#[test]
fn retention_is_bounded_by_bytes_and_never_evicts_newer_for_older() {
    use agentscope::providers::{Admit, Retention};
    let mut r: Retention<u32> = Retention::new(100, 1000);
    for k in 0..10u32 {
        r.admit(&k, Some(i64::from(k)), 400);
        assert!(r.bytes() <= 1000, "{} bytes", r.bytes());
    }
    assert_eq!(r.len(), 2);
    assert!(r.contains(&9) && r.contains(&8));
    assert!(
        matches!(r.admit(&1, Some(1), 400), Admit::Dropped),
        "older than the kept set"
    );
    assert!(
        matches!(r.admit(&99, Some(99), 5000), Admit::Dropped),
        "larger than the byte cap"
    );
    assert_eq!(r.len(), 2, "a rejected candidate evicts nothing");
}

// P2. A byte ceiling of exactly one record still admits a newer source on refresh.
#[test]
fn a_one_record_byte_ceiling_replaces_the_oldest_member_on_refresh() {
    let old = meta("aaaa", BASE);
    let one = agentscope::roster::metadata_bytes(&old);
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 100,
            max_bytes: one,
        },
        10,
    );
    index.apply_discovery(delta(vec![old]));
    index.freeze(1);
    let newer = meta("bbbb", BASE + 1_000);
    assert_eq!(agentscope::roster::metadata_bytes(&newer), one);
    index.apply_discovery(delta(vec![newer]));
    let during = index.snapshot();
    assert!(during.refresh_available, "the newer source is advertised");
    assert_eq!(during.roots.len(), 1);
    assert_eq!(
        during.roots[0].native_id, "aaaa",
        "the frozen page is stable"
    );
    index.freeze(2);
    let after = index.snapshot();
    assert_eq!(after.roots.len(), 1);
    assert_eq!(after.roots[0].native_id, "bbbb");
    assert!(after.limited.is_some());
}

// 5. One returned record consumes the shared allowance exactly once.
#[test]
fn the_combined_allowance_holds_at_one_record() {
    use agentscope::providers::claude::ClaudeScanner;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let lines: Vec<String> = (0..4)
        .map(|i| {
            serde_json::json!({"type":"assistant","uuid":format!("u{i}"),"sessionId":"s",
                "timestamp":iso(BASE + i),"message":{"id":format!("m{i}"),"model":"x",
                "content":[{"type":"text","text":"hi"}],
                "usage":{"input_tokens":3,"output_tokens":2}}})
            .to_string()
        })
        .collect();
    write_lines(&root.join("p/s.jsonl"), &lines);
    let mut config = config_for(&root);
    config.harnesses.claude.root = Some(root);
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    for max in 1..=4usize {
        let page = support::world::details_ready(
            &mut scanner,
            &key,
            None,
            PageRequest {
                max_events: max,
                max_bytes: 1 << 20,
            },
            BIG,
            support::world::STEPS,
        );
        let used = page.requests.len() + page.compactions.len() + page.recent.len();
        assert!(used <= max, "max {max}: {used} records returned");
        assert!(!page.recent.is_empty() || max == 0);
    }
}

// 6. Nine concurrent index jobs: no live index is discarded, all complete.
#[test]
fn nine_concurrent_index_jobs_all_complete_without_restarting_live_indexes() {
    use agentscope::providers::claude::ClaudeScanner;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    for n in 0..9 {
        let name = format!("s{n}");
        let lines: Vec<String> = (0..1500)
            .map(|i| {
                serde_json::json!({"type":"user","uuid":format!("{name}-{i}"),"sessionId":name,
                    "timestamp":iso(BASE + i),"message":{"role":"user","content":"x"}})
                .to_string()
            })
            .collect();
        write_lines(&root.join("p").join(format!("{name}.jsonl")), &lines);
    }
    let mut config = config_for(&root);
    config.harnesses.claude.root = Some(root);
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let keys: Vec<SessionKey> = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .into_iter()
        .map(|m| m.key)
        .collect();
    assert_eq!(keys.len(), 9);
    let step = RefreshBudget {
        max_records: 400,
        ..BIG
    };
    let jobs: Vec<_> = (0..9).map(|_| support::world::new_job()).collect();
    let mut done = [false; 9];
    let mut calls = 0;
    while !done.iter().all(|d| *d) {
        for i in 0..9 {
            if done[i] {
                continue;
            }
            let r = scanner
                .transcript_page(
                    jobs[i],
                    &keys[i],
                    None,
                    PageDirection::Forward,
                    PageRequest {
                        max_events: 5,
                        max_bytes: 1 << 20,
                    },
                    step,
                )
                .unwrap();
            done[i] = matches!(r, ReadProgress::Ready(_));
            calls += 1;
        }
        assert!(
            calls < 9 * 60,
            "live indexes were restarted ({calls} calls)"
        );
    }
}

// 4. The OpenCode recent-event pager is sliced over rows that yield no events.
#[test]
fn opencode_recent_pager_counts_examined_rows_not_emitted_events() {
    use agentscope::providers::opencode::OpenCodeScanner;
    use support::opencode as oc;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    let conn = oc::create_v2(&db);
    oc::insert_session(&conn, &oc::Session::new("ses_1", 1, 1_000_000));
    // Two old messages produce events; 3,000 newer rows of an unknown type do not.
    oc::insert_message(&conn, "old1", "ses_1", 10, 10);
    oc::insert_message(&conn, "old2", "ses_1", 11, 11);
    for i in 0..3000i64 {
        conn.execute(
            "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, 'ses_1', 'noise', 0, ?2, ?2, '{}')",
            rusqlite::params![format!("n{i:05}"), 1_000 + i],
        )
        .unwrap();
    }
    let mut scanner = OpenCodeScanner::new(&oc::config_for_db(&db)).unwrap();
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    let budget = RefreshBudget {
        max_records: 256,
        max_bytes: 8 << 20,
        max_millis: 600_000,
    };
    let job = support::world::new_job();
    let mut pending = 0;
    let page = loop {
        match scanner
            .details_page(
                job,
                &key,
                None,
                PageRequest {
                    max_events: 20,
                    max_bytes: 1 << 20,
                },
                budget,
            )
            .unwrap()
        {
            ReadProgress::Pending(p) => {
                pending += 1;
                assert!(p.records <= 256, "{} rows examined in one call", p.records);
            }
            ReadProgress::Ready(page) => break page,
        }
        assert!(pending < 100);
    };
    assert!(pending >= 10, "{pending} pending calls over 3,002 rows");
    assert_eq!(
        page.recent.len(),
        2,
        "the older events are found across slices"
    );
    assert_eq!(scanner.job_state(), 0);
}

// 12. Directory enumeration is bounded per step.
#[test]
fn enumerating_a_large_flat_directory_is_resumable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    std::fs::create_dir_all(&root).unwrap();
    for i in 0..1000 {
        std::fs::write(root.join(format!("rollout-{i:05}.jsonl")), b"{}\n").unwrap();
    }
    let mut walk = agentscope::discovery::Walk::new(Harness::Codex, Some(&root));
    let expired = Instant::now();
    let mut steps = 0;
    let mut found = 0;
    while !walk.is_done() {
        let before = walk.scanned();
        found += walk.step(1, expired).len();
        assert!(
            walk.scanned() - before <= 65,
            "one step enumerated {} entries",
            walk.scanned() - before
        );
        steps += 1;
        assert!(steps < 10_000);
    }
    assert_eq!(found, 1000);
    assert!(steps >= 15, "enumeration spanned {steps} steps");
}

// 10. Two large live sessions keep independent in-progress indexes.
#[test]
fn two_large_sessions_index_without_restarting_each_other() {
    use agentscope::providers::claude::ClaudeScanner;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    for name in ["a", "b"] {
        let lines: Vec<String> = (0..3000)
            .map(|i| {
                serde_json::json!({"type":"user","uuid":format!("{name}{i}"),"sessionId":name,
                    "timestamp":iso(BASE + i),"message":{"role":"user","content":"x"}})
                .to_string()
            })
            .collect();
        write_lines(&root.join("p").join(format!("{name}.jsonl")), &lines);
    }
    let mut config = config_for(&root);
    config.harnesses.claude.root = Some(root);
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let keys: Vec<SessionKey> = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .into_iter()
        .map(|m| m.key)
        .collect();
    assert_eq!(keys.len(), 2);
    let step = RefreshBudget {
        max_records: 400,
        ..BIG
    };
    let jobs = [support::world::new_job(), support::world::new_job()];
    let mut done = [false; 2];
    let mut calls = 0;
    let mut per = [0usize; 2];
    while !done.iter().all(|d| *d) {
        for i in 0..2 {
            if done[i] {
                continue;
            }
            let r = scanner
                .transcript_page(
                    jobs[i],
                    &keys[i],
                    None,
                    PageDirection::Forward,
                    PageRequest {
                        max_events: 5,
                        max_bytes: 1 << 20,
                    },
                    step,
                )
                .unwrap();
            done[i] = matches!(r, ReadProgress::Ready(_));
            per[i] += 1;
            calls += 1;
        }
        assert!(
            calls < 160,
            "alternating sessions restarted each other ({calls} calls, {per:?})"
        );
    }
}

// 6. OpenCode details are resumable under the work budget over token-less rows.
#[test]
fn opencode_details_examine_a_bounded_number_of_rows_per_call() {
    use agentscope::providers::opencode::OpenCodeScanner;
    use support::opencode as oc;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    let conn = oc::create_v2(&db);
    oc::insert_session(&conn, &oc::Session::new("ses_1", 1, 1_000_000));
    let insert = |id: String, at: i64, data: String| {
        conn.execute(
            "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, 'ses_1', 'assistant', 0, ?2, ?2, ?3)",
            rusqlite::params![id, at, data],
        )
        .unwrap();
    };
    // Two old rows carry tokens; 3,000 newer assistant rows carry none.
    for i in 0..2 {
        insert(
            format!("req{i}"),
            10 + i,
            serde_json::json!({"role":"assistant","tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}).to_string(),
        );
    }
    for i in 0..3000 {
        insert(
            format!("t{i:05}"),
            1_000 + i,
            serde_json::json!({"role":"assistant"}).to_string(),
        );
    }
    let mut scanner = OpenCodeScanner::new(&oc::config_for_db(&db)).unwrap();
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    let budget = RefreshBudget {
        max_records: 256,
        max_bytes: 8 << 20,
        max_millis: 600_000,
    };
    let job = support::world::new_job();
    let mut pending = 0;
    let page = loop {
        match scanner
            .details_page(
                job,
                &key,
                None,
                PageRequest {
                    max_events: 20,
                    max_bytes: 1 << 20,
                },
                budget,
            )
            .unwrap()
        {
            ReadProgress::Pending(p) => {
                pending += 1;
                assert!(p.records <= 256, "{} rows examined in one call", p.records);
                assert_eq!(scanner.job_state(), 1);
            }
            ReadProgress::Ready(page) => break page,
        }
        assert!(pending < 100);
    };
    assert!(pending >= 10, "{pending} pending calls over 3,002 rows");
    assert_eq!(
        page.requests.len(),
        2,
        "the old requests are found without a whole-history call"
    );
    assert_eq!(scanner.job_state(), 0);
    // Cancelling mid-way releases the continuation.
    let job = support::world::new_job();
    assert!(matches!(
        scanner
            .details_page(
                job,
                &key,
                None,
                PageRequest {
                    max_events: 20,
                    max_bytes: 1 << 20
                },
                budget
            )
            .unwrap(),
        ReadProgress::Pending(_)
    ));
    scanner.cancel_read(&key, job);
    assert_eq!(scanner.job_state(), 0);
}

// ---- third review round -------------------------------------------------------

fn codex_files(root: &std::path::Path, ids: std::ops::Range<i64>) {
    for i in ids {
        let id = format!("c{i:02}");
        write_lines(
            &dated(root, &format!("rollout-{id}.jsonl")),
            &[
                support::codex::session_meta(&iso(BASE + i * 1000), &id, "/w"),
                support::codex::user(&iso(BASE + i * 1000), "task"),
            ],
        );
    }
}

fn limited_codex(root: &std::path::Path, cap: usize) -> CodexScanner {
    let mut config = config_for(root);
    config.max_metadata_sources = cap;
    CodexScanner::new(&config).unwrap()
}

// P1: the evicted-source fallback validates identity.
#[test]
fn an_evicted_sources_fallback_rejects_replaced_files_and_unknown_ids() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    codex_files(&root, 0..5);
    let mut scanner = limited_codex(&root, 5);
    let first = support::world::discover_pass(&mut scanner, NOW, BIG);
    let frozen = first
        .sessions
        .iter()
        .find(|m| m.key.native_id == "c00")
        .unwrap()
        .key
        .clone();
    codex_files(&root, 5..8); // evicts c00..c02 from the adapter's bookkeeping
    support::world::discover_pass(&mut scanner, NOW, BIG);
    support::world::hydrate_complete(&mut scanner, &frozen, NOW);

    // A never-discovered ID on an evicted path is refused.
    let ghost = SessionKey {
        native_id: "ghost".into(),
        ..frozen.clone()
    };
    assert!(support::world::try_hydrate(&mut scanner, &ghost, NOW, BIG, 10).is_err());
    // A path that was never tracked (older than the kept set) is refused too.
    let never = SessionKey {
        native_id: "c99".into(),
        source: SourceKey::File(dated(&root, "rollout-c99.jsonl")),
        ..frozen.clone()
    };
    write_lines(
        &dated(&root, "rollout-c99.jsonl"),
        &[support::codex::session_meta(&iso(BASE), "c99", "/w")],
    );
    assert!(support::world::try_hydrate(&mut scanner, &never, NOW, BIG, 10).is_err());
    // The file is replaced by a different session: the old key must not read it.
    write_lines(
        &dated(&root, "rollout-c00.jsonl"),
        &[support::codex::session_meta(
            &iso(BASE),
            "someone-else",
            "/w",
        )],
    );
    assert!(support::world::try_hydrate(&mut scanner, &frozen, NOW, BIG, 10).is_err());
}

// P2: a physical deletion of an evicted frozen source still reaches the roster.
#[test]
fn deleting_an_evicted_frozen_source_is_reported_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    codex_files(&root, 0..5);
    let mut scanner = limited_codex(&root, 5);
    let first = support::world::discover_pass(&mut scanner, NOW, BIG);
    let frozen = first
        .sessions
        .iter()
        .find(|m| m.key.native_id == "c00")
        .unwrap()
        .key
        .clone();
    codex_files(&root, 5..8);
    let evicting = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert!(
        !evicting.removed.contains(&frozen),
        "eviction is not deletion"
    );
    std::fs::remove_file(dated(&root, "rollout-c00.jsonl")).unwrap();
    let after = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert!(after.removed.contains(&frozen), "the deletion is reported");
    // And the index marks the frozen row unavailable.
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 100,
            max_bytes: 64 << 20,
        },
        10,
    );
    let mut m = meta("c00", BASE);
    m.key = frozen.clone();
    index.apply_discovery(delta(vec![m]));
    index.freeze(1);
    let mut gone = delta(vec![]);
    gone.removed = vec![frozen];
    index.apply_discovery(gone);
    assert!(!index.snapshot().metadata[0].source_available);
}

// P2/P2: growth updates follow the same age rules; an oversize update is not a deletion.
#[test]
fn retention_growth_updates_never_evict_newer_sources() {
    use agentscope::providers::{Admit, Retention};
    let mut r: Retention<&str> = Retention::new(100, 1000);
    r.admit(&"a", Some(1), 400);
    r.admit(&"b", Some(2), 400);
    // A (t1) grows to 800: it cannot fit without evicting newer B, so A is displaced.
    assert!(matches!(r.admit(&"a", Some(1), 800), Admit::Displaced));
    assert!(r.contains(&"b") && !r.contains(&"a"));
    assert_eq!(r.bytes(), 400);
    // An update larger than the byte cap displaces (never "removes") the source.
    assert!(matches!(r.admit(&"b", Some(2), 5000), Admit::Displaced));
    assert_eq!(r.len(), 0);
    // A growing newest source may evict older ones.
    let mut r: Retention<&str> = Retention::new(100, 1000);
    r.admit(&"old", Some(1), 400);
    r.admit(&"new", Some(5), 400);
    match r.admit(&"new", Some(5), 800) {
        Admit::Kept { evicted } => assert_eq!(evicted, ["old"]),
        _ => panic!("the newest source keeps its place"),
    }
}

#[test]
fn a_source_that_outgrows_the_ceiling_is_forgotten_stays_readable_and_reports_deletion() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    codex_files(&root, 0..1);
    // Measure what the small session weighs, then allow only a little more.
    let probe = {
        let mut scanner = limited_codex(&root, 10);
        let m = support::world::discover_pass(&mut scanner, NOW, BIG)
            .sessions
            .remove(0);
        agentscope::roster::metadata_bytes(&m) + dated(&root, "rollout-c00.jsonl").as_os_str().len()
    };
    let mut config = config_for(&root);
    config.max_metadata_bytes = probe + 40;
    let mut scanner = CodexScanner::new(&config).unwrap();
    let first = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(first.sessions.len(), 1);
    assert_eq!(scanner.tracked_sources(), 1);
    let key = first.sessions[0].key.clone();
    // The session's cwd (hence its metadata) grows well past the byte ceiling.
    let long = format!("/{}", "z".repeat(200));
    write_lines(
        &dated(&root, "rollout-c00.jsonl"),
        &[
            support::codex::session_meta(&iso(BASE), "c00", &long),
            support::codex::user(&iso(BASE + 1), "task"),
        ],
    );
    let again = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(
        scanner.tracked_sources(),
        0,
        "the oversized update was displaced"
    );
    assert!(
        again.removed.is_empty(),
        "an existing file is not reported removed"
    );
    assert!(
        again
            .warnings
            .iter()
            .any(|w| w.message.contains("Discovery limited"))
    );
    // Forgotten, but still readable under its published identity.
    let summary = support::world::hydrate_complete(&mut scanner, &key, NOW);
    assert_eq!(summary.meta.key, key);
    // A later genuine deletion is still reported.
    std::fs::remove_file(dated(&root, "rollout-c00.jsonl")).unwrap();
    let gone = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert!(gone.removed.contains(&key));
}

// P1: repeated discovery during a fallback hydration never restarts the read.
#[test]
fn discovery_passes_during_a_fallback_hydration_do_not_restart_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    // `rollout-b` (older) is visited first and admitted; `rollout-a` (newer) then evicts it.
    let mut old = vec![support::codex::session_meta(&iso(BASE), "older", "/w")];
    old.extend((0..60).map(|i| support::codex::user(&iso(BASE + 1 + i), "x")));
    write_lines(&dated(&root, "rollout-b.jsonl"), &old);
    write_lines(
        &dated(&root, "rollout-a.jsonl"),
        &[support::codex::session_meta(
            &iso(BASE + 90_000),
            "newer",
            "/w",
        )],
    );
    let mut scanner = limited_codex(&root, 1);
    let first = support::world::discover_pass(&mut scanner, NOW, BIG);
    let frozen = first
        .sessions
        .iter()
        .find(|m| m.key.native_id == "older")
        .unwrap()
        .key
        .clone();
    assert_eq!(scanner.tracked_sources(), 1);
    let step = RefreshBudget {
        max_records: 5,
        ..BIG
    };
    let job = support::world::new_job();
    let mut slices = 0;
    loop {
        let delta = scanner.hydrate(job, &frozen, NOW, step).unwrap();
        slices += 1;
        if delta.done {
            assert_eq!(delta.summary.displayed_event_count, 60);
            break;
        }
        // Discovery keeps offering the forgotten source to retention in between.
        support::world::discover_pass(&mut scanner, NOW, BIG);
        assert!(slices < 40, "the hydration restarted ({slices} slices)");
    }
    assert!(
        slices >= 12,
        "{slices} slices for 61 records at 5 per slice"
    );
}

// 5. Beyond the forgotten-set bound a frozen key fails closed.
#[test]
fn beyond_the_forgotten_bound_a_frozen_key_is_refused_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    const FILES: i64 = 4200;
    // File names descend as activity ascends, so each newer source evicts the previous one.
    for i in 0..FILES {
        let id = format!("s{i:04}");
        write_lines(
            &dated(&root, &format!("rollout-{:05}.jsonl", FILES - i)),
            &[support::codex::session_meta(
                &iso(BASE + i * 1000),
                &id,
                "/w",
            )],
        );
    }
    let mut scanner = limited_codex(&root, 1);
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(scanner.tracked_sources(), 1);
    let key_of = |id: &str| {
        found
            .sessions
            .iter()
            .find(|m| m.key.native_id == id)
            .unwrap()
            .key
            .clone()
    };
    // Recently forgotten: still readable through the fallback.
    let recent = key_of("s4000");
    assert_eq!(
        support::world::hydrate_complete(&mut scanner, &recent, NOW)
            .meta
            .key,
        recent
    );
    // The oldest evictions fell off the bounded set: readable no longer (fail closed).
    let stale = key_of("s0001");
    assert!(support::world::try_hydrate(&mut scanner, &stale, NOW, BIG, 10).is_err());
}

// P2: only a confirmed absence is a deletion (checked by construction with NotFound);
// a still-existing forgotten file is never reported removed.
#[test]
fn a_forgotten_source_that_still_exists_is_never_reported_removed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("codex");
    codex_files(&root, 0..3);
    let mut scanner = limited_codex(&root, 1);
    support::world::discover_pass(&mut scanner, NOW, BIG);
    for _ in 0..3 {
        let pass = support::world::discover_pass(&mut scanner, NOW, BIG);
        assert!(pass.removed.is_empty());
    }
}

// P2: the OpenCode deletion sweep is budgeted and resumable; no removal is lost.
#[test]
fn the_opencode_deletion_sweep_is_resumable_and_loses_no_removal() {
    use agentscope::providers::opencode::OpenCodeScanner;
    use support::opencode as oc;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    let conn = oc::create_v2(&db);
    for i in 0..4 {
        oc::insert_session(&conn, &oc::Session::new(&format!("s{i}"), 1, 100 + i));
    }
    let mut config = oc::config_for_db(&db);
    config.max_metadata_sources = 1;
    let mut scanner = OpenCodeScanner::new(&config).unwrap();
    support::world::discover_pass(&mut scanner, NOW, BIG);
    // Each update makes a different session the newest, displacing the tracked one.
    for (n, id) in ["s2", "s1", "s0"].iter().enumerate() {
        conn.execute(
            "UPDATE session_v2 SET time_updated = ?1 WHERE id = ?2",
            rusqlite::params![1_000 + n as i64, id],
        )
        .unwrap();
        support::world::discover_pass(&mut scanner, NOW, BIG);
    }
    assert_eq!(scanner.tracked_sources(), 1);
    // Delete the three displaced sessions; sweep one session per call.
    conn.execute("DELETE FROM session_v2 WHERE id IN ('s3','s2','s1')", [])
        .unwrap();
    let one = RefreshBudget {
        max_records: 1,
        ..BIG
    };
    let mut removed: Vec<String> = Vec::new();
    let mut calls = 0;
    loop {
        let delta = scanner.discover(NOW, one).unwrap();
        removed.extend(delta.removed.iter().map(|k| k.native_id.clone()));
        calls += 1;
        if delta.scan.done {
            break;
        }
        assert!(calls < 200, "the sweep never finished");
    }
    removed.sort();
    assert_eq!(
        removed,
        ["s1", "s2", "s3"],
        "every deletion was delivered once"
    );
    assert!(calls >= 3, "the sweep was spread over {calls} calls");
}

// P2: an oversized candidate is rejected before the merge.
#[test]
fn an_oversized_candidate_never_empties_the_roster() {
    let old = meta("aaaa", BASE);
    let one = agentscope::roster::metadata_bytes(&old);
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 100,
            max_bytes: one + one / 2,
        },
        10,
    );
    index.apply_discovery(delta(vec![old]));
    index.freeze(1);
    let mut huge = meta("huge", BASE + 9_000);
    huge.title = "t".repeat(one * 4);
    index.apply_discovery(delta(vec![huge]));
    index.freeze(2);
    let snap = index.snapshot();
    assert_eq!(snap.roots.len(), 1);
    assert_eq!(
        snap.roots[0].native_id, "aaaa",
        "the fitting entry survives"
    );
}

// ---- OpenCode details: cursor, resume row, accumulated allowance ---------------

mod opencode_details {
    use super::*;
    use agentscope::providers::opencode::OpenCodeScanner;
    use support::opencode as oc;

    fn store(
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

    fn typed(conn: &rusqlite::Connection, ty: &str, id: String, at: i64) {
        conn.execute(
            "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, 'ses_1', ?2, 0, ?3, ?3, '{}')",
            rusqlite::params![id, ty, at],
        )
        .unwrap();
    }

    fn drive(
        scanner: &mut OpenCodeScanner,
        key: &SessionKey,
        request: PageRequest,
        budget: RefreshBudget,
        max_calls: usize,
    ) -> (SessionDetailPage, usize) {
        let job = support::world::new_job();
        let mut pending = 0;
        loop {
            match scanner
                .details_page(job, key, None, request, budget)
                .unwrap()
            {
                ReadProgress::Pending(p) => {
                    pending += 1;
                    assert!(
                        p.records <= budget.max_records.max(1),
                        "{} rows in one call",
                        p.records
                    );
                }
                ReadProgress::Ready(page) => return (page, pending),
            }
            assert!(pending < max_calls, "no progress after {pending} calls");
        }
    }

    #[test]
    fn the_request_cursor_crosses_sql_filtered_rows_and_deadline_interruptions() {
        let (_d, mut scanner, key) = store(|c| {
            for i in 0..2 {
                c.execute(
                    "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
                     VALUES (?1, 'ses_1', 'assistant', 0, ?2, ?2, ?3)",
                    rusqlite::params![
                        format!("req{i}"),
                        10 + i,
                        serde_json::json!({"role":"assistant","tokens":{"input":1,"output":1,"reasoning":0,"cache":{"read":0,"write":0}}}).to_string()
                    ],
                )
                .unwrap();
            }
            // A long run of newer rows that are not assistant rows at all.
            for i in 0..4000i64 {
                typed(c, "user", format!("u{i:05}"), 1_000 + i);
            }
        });
        let request = PageRequest {
            max_events: 20,
            max_bytes: 1 << 20,
        };
        let slice = RefreshBudget {
            max_records: 256,
            max_bytes: 8 << 20,
            max_millis: 600_000,
        };
        let (page, pending) = drive(&mut scanner, &key, request, slice, 200);
        assert!(pending >= 15, "{pending} pending calls over 4,002 rows");
        assert_eq!(
            page.requests.len(),
            2,
            "the old requests sit behind the filtered rows"
        );
        // A 1 ms deadline interrupts queries mid-way; the parked cursor still advances.
        let tiny = RefreshBudget {
            max_records: 100_000,
            max_bytes: 8 << 20,
            max_millis: 1,
        };
        let (page, _) = drive(&mut scanner, &key, request, tiny, 100_000);
        assert_eq!(page.requests.len(), 2);
    }

    #[test]
    fn a_one_record_slice_always_progresses_through_the_recent_pager() {
        let (_d, mut scanner, key) = store(|c| {
            oc::insert_message(c, "ev1", "ses_1", 10, 10);
            oc::insert_message(c, "ev2", "ses_1", 11, 11);
            for i in 0..6i64 {
                typed(c, "noise", format!("n{i}"), 100 + i);
            }
        });
        let one = RefreshBudget {
            max_records: 1,
            max_bytes: 8 << 20,
            max_millis: 600_000,
        };
        let (page, pending) = drive(
            &mut scanner,
            &key,
            PageRequest {
                max_events: 10,
                max_bytes: 1 << 20,
            },
            one,
            200,
        );
        assert!(
            pending >= 7,
            "{pending} calls for 8 rows at one row per call"
        );
        assert_eq!(
            page.recent.len(),
            2,
            "no row was re-examined forever or skipped"
        );
    }

    #[test]
    fn the_recent_page_fills_exactly_and_never_exceeds_its_allowance() {
        let (_d, mut scanner, key) = store(|c| {
            for i in 0..5i64 {
                oc::insert_message(c, &format!("e{i}"), "ses_1", 10 + i, 10 + i);
                typed(c, "noise", format!("n{i}"), 10 + i);
            }
        });
        let one = RefreshBudget {
            max_records: 1,
            max_bytes: 8 << 20,
            max_millis: 600_000,
        };
        for (max, expect, truncated) in [
            (1usize, 1usize, true),
            (2, 2, true),
            (5, 5, false),
            (9, 5, false),
        ] {
            let (page, _) = drive(
                &mut scanner,
                &key,
                PageRequest {
                    max_events: max,
                    max_bytes: 1 << 20,
                },
                one,
                500,
            );
            assert_eq!(page.recent.len(), expect, "max_events {max}");
            assert!(page.requests.len() + page.recent.len() <= max.max(1));
            assert_eq!(page.truncated, truncated, "max_events {max}");
        }
        // A byte allowance smaller than two events stops the page at one, across slices.
        let (page, _) = drive(
            &mut scanner,
            &key,
            PageRequest {
                max_events: 10,
                max_bytes: 2,
            },
            one,
            500,
        );
        assert_eq!(page.recent.len(), 1);
    }
}
