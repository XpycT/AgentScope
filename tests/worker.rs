use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::*;
use agentscope::processes::{ProcessInfo, ProcessSnapshot};
use agentscope::providers::{
    DiscoveryDelta, HydrationDelta, ProviderError, ProviderReader, ReadJobId, ReadProgress,
    SourceRecord, WorkProgress,
};
use agentscope::worker::{WorkerEvent, WorkerHandle, WorkerRequest};
use support::facts::{self, metadata};

const WAIT: Duration = Duration::from_secs(5);

/// What a scripted discovery slice reports: the full sessions it announces (their
/// metadata goes to the index; the summaries answer later hydration).
struct Delta {
    sessions: Vec<SessionSummary>,
    removed: Vec<SessionKey>,
    invalidated: Vec<SessionKey>,
    warnings: Vec<Warning>,
    scan: ScanProgress,
}

#[derive(Default)]
struct Script {
    refreshes: VecDeque<Result<Delta, ProviderError>>,
    /// Summaries hydration answers with, by key.
    known: HashMap<SessionKey, SessionSummary>,
    pages: VecDeque<Result<ReadProgress<TranscriptPage>, ProviderError>>,
    refresh_calls: usize,
    hydrate_calls: Vec<SessionKey>,
    cancelled: Vec<(SessionKey, ReadJobId)>,
    page_calls: usize,
    page_jobs: Vec<ReadJobId>,
    page_keys: Vec<SessionKey>,
    page_requests: Vec<(Option<PageAnchor>, PageDirection)>,
    page_max_events: Vec<usize>,
    count: usize,
    endless: bool,
}

struct Fake {
    harness: Harness,
    script: Arc<Mutex<Script>>,
    /// When set, every `refresh` call announces itself then waits for a permit.
    gate: Option<(Sender<()>, Receiver<()>)>,
    /// When set, every `transcript_page` call announces itself then waits.
    page_gate: Option<(Sender<()>, Receiver<()>)>,
    /// When set, every `hydrate` call announces itself then waits.
    hydrate_gate: Option<(Sender<()>, Receiver<()>)>,
    /// When set, every `details_page` call announces itself then waits.
    details_gate: Option<(Sender<()>, Receiver<()>)>,
}

fn key(provider: Harness, id: &str) -> SessionKey {
    SessionKey {
        harness: provider,
        native_id: id.into(),
        source: SourceKey::File("a".into()),
    }
}

fn summary(provider: Harness, id: &str, persisted: i64, count: usize) -> SessionSummary {
    let mut meta = metadata(key(provider, id), "t");
    meta.activity = ActivityStamp {
        at: Some(persisted),
        class: AggregationClass::Reported,
    };
    SessionSummary {
        last_event_at: None,
        last_completion: None,
        unfinished_work: false,
        displayed_event_count: count,
        hidden_records: 0,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::NotChecked,
        usage: None,
        warnings: vec![],
        ..facts::summary(meta)
    }
}

fn warning(provider: Harness, source: &str, kind: WarningKind, msg: &str) -> Warning {
    Warning {
        key: WarningKey {
            harness: provider,
            source: source.into(),
            kind,
        },
        message: msg.into(),
        count: 1,
    }
}

fn delta(sessions: Vec<SessionSummary>, warnings: Vec<Warning>, done: bool) -> Delta {
    Delta {
        scan: ScanProgress {
            harness: sessions
                .first()
                .map_or(Harness::Codex, |s| s.meta.key.harness),
            scanned: sessions.len(),
            total: None,
            done,
        },
        sessions,
        removed: vec![],
        invalidated: vec![],
        warnings,
    }
}

fn with_invalidated(mut d: Delta, keys: Vec<SessionKey>) -> Delta {
    d.invalidated = keys;
    d
}

fn empty_page() -> TranscriptPage {
    TranscriptPage {
        events: vec![],
        diagnostics: vec![],
        next: None,
        has_more: false,
        truncated: false,
    }
}

fn event_page(key: &SessionKey, id: &str) -> TranscriptPage {
    TranscriptPage {
        events: vec![Event {
            id: id.into(),
            session: key.clone(),
            timestamp: Some(1),
            source_order: 1,
            kind: EventKind::AssistantMessage,
            actor: Actor::Assistant,
            text: Some(BoundedText {
                preview: format!("body {id}"),
                byte_len: 4,
                truncated: false,
                locator: None,
            }),
            tool: None,
            usage: None,
            provenance: Provenance {
                harness: key.harness,
                source: key.source.clone(),
                locator: SourceLocator {
                    session: key.clone(),
                    kind: LocatorKind::FileOffset {
                        path: "a.jsonl".into(),
                        offset: 1,
                    },
                    generation: None,
                },
                record_type: "test".into(),
                harness_version: None,
            },
        }],
        diagnostics: vec![],
        next: None,
        has_more: false,
        truncated: false,
    }
}

impl ProviderReader for Fake {
    fn harness(&self) -> Harness {
        self.harness
    }
    fn discover(
        &mut self,
        _now: Timestamp,
        _budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        if let Some((entered, gate)) = &self.gate {
            let _ = entered.send(());
            let _ = gate.recv_timeout(WAIT);
        }
        let mut s = self.script.lock().unwrap();
        s.refresh_calls += 1;
        let d = match s.refreshes.pop_front() {
            Some(r) => r?,
            None => delta(vec![], vec![], !s.endless),
        };
        for session in &d.sessions {
            s.known.insert(session.meta.key.clone(), session.clone());
        }
        Ok(DiscoveryDelta {
            sessions: d.sessions.into_iter().map(|s| s.meta).collect(),
            removed: d.removed,
            invalidated: d.invalidated,
            warnings: d.warnings,
            scan: d.scan,
        })
    }
    fn hydrate(
        &mut self,
        _job: ReadJobId,
        key: &SessionKey,
        _now: Timestamp,
        _budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        if let Some((entered, gate)) = &self.hydrate_gate {
            let _ = entered.send(());
            let _ = gate.recv_timeout(WAIT);
        }
        let mut s = self.script.lock().unwrap();
        s.hydrate_calls.push(key.clone());
        s.known
            .get(key)
            .map(|summary| HydrationDelta {
                summary: summary.clone(),
                done: true,
            })
            .ok_or_else(|| ProviderError::Unavailable("unknown session".into()))
    }
    fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId) {
        self.script
            .lock()
            .unwrap()
            .cancelled
            .push((key.clone(), job));
    }
    fn details_page(
        &mut self,
        _job: ReadJobId,
        _key: &SessionKey,
        _anchor: Option<PageAnchor>,
        _request: PageRequest,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError> {
        if let Some((entered, gate)) = &self.details_gate {
            let _ = entered.send(());
            let _ = gate.recv_timeout(WAIT);
        }
        Ok(ReadProgress::Ready(SessionDetailPage::default()))
    }
    fn transcript_page(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        if let Some((entered, gate)) = &self.page_gate {
            let _ = entered.send(());
            let _ = gate.recv_timeout(WAIT);
        }
        let mut s = self.script.lock().unwrap();
        s.page_calls += 1;
        s.page_jobs.push(job);
        s.page_keys.push(key.clone());
        s.page_requests.push((anchor, direction));
        s.page_max_events.push(request.max_events);
        s.pages
            .pop_front()
            .unwrap_or_else(|| Ok(ReadProgress::Ready(empty_page())))
    }
    fn count_events_after(
        &mut self,
        _job: ReadJobId,
        _key: &SessionKey,
        _anchor: &PageAnchor,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        Ok(ReadProgress::Ready(self.script.lock().unwrap().count))
    }
    fn source_record(
        &mut self,
        _job: ReadJobId,
        _locator: &SourceLocator,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        Ok(ReadProgress::Ready(SourceRecord {
            raw: "raw".into(),
            display: "display".into(),
            unavailable: None,
        }))
    }
}

fn config() -> Config {
    let mut c = Config::load_with_env(&CliOverrides::default(), &Env::new()).unwrap();
    c.refresh_ms = 3_600_000;
    c.process_ms = 3_600_000;
    c.process_enabled = false;
    c
}

fn fake(provider: Harness, script: &Arc<Mutex<Script>>) -> Fake {
    Fake {
        harness: provider,
        script: script.clone(),
        gate: None,
        page_gate: None,
        hydrate_gate: None,
        details_gate: None,
    }
}

fn gated(provider: Harness, script: &Arc<Mutex<Script>>) -> (Fake, Receiver<()>, Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (gate_tx, gate_rx) = mpsc::channel();
    (
        Fake {
            harness: provider,
            script: script.clone(),
            gate: Some((entered_tx, gate_rx)),
            page_gate: None,
            hydrate_gate: None,
            details_gate: None,
        },
        entered_rx,
        gate_tx,
    )
}

fn gated_hydrate(
    provider: Harness,
    script: &Arc<Mutex<Script>>,
) -> (Fake, Receiver<()>, Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (gate_tx, gate_rx) = mpsc::channel();
    (
        Fake {
            harness: provider,
            script: script.clone(),
            gate: None,
            page_gate: None,
            hydrate_gate: Some((entered_tx, gate_rx)),
            details_gate: None,
        },
        entered_rx,
        gate_tx,
    )
}

fn gated_details(
    provider: Harness,
    script: &Arc<Mutex<Script>>,
) -> (Fake, Receiver<()>, Sender<()>) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (gate_tx, gate_rx) = mpsc::channel();
    (
        Fake {
            harness: provider,
            script: script.clone(),
            gate: None,
            page_gate: None,
            hydrate_gate: None,
            details_gate: Some((entered_tx, gate_rx)),
        },
        entered_rx,
        gate_tx,
    )
}

fn spawn(fake: Fake) -> WorkerHandle {
    WorkerHandle::spawn(vec![Box::new(fake)], config())
}

fn next(h: &WorkerHandle) -> WorkerEvent {
    h.rx.recv_timeout(WAIT).expect("worker event")
}

/// Collect events until `pred` matches one (inclusive).
fn until(h: &WorkerHandle, pred: impl Fn(&WorkerEvent) -> bool) -> Vec<WorkerEvent> {
    let mut out = Vec::new();
    loop {
        let ev = next(h);
        let hit = pred(&ev);
        out.push(ev);
        if hit {
            return out;
        }
    }
}

fn finished(gen_: u64) -> impl Fn(&WorkerEvent) -> bool {
    move |e| matches!(e, WorkerEvent::RefreshFinished { generation } if *generation == gen_)
}

/// The loaded roots of the newest roster event and its warnings.
fn last_roster(events: &[WorkerEvent]) -> (&[Arc<SessionMetadata>], &[Warning]) {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            WorkerEvent::Roster { snapshot, warnings } => {
                Some((snapshot.metadata.as_slice(), warnings.as_slice()))
            }
            _ => None,
        })
        .expect("a roster event")
}

#[test]
fn coalesces_refresh_and_drops_stale_transcript_responses() {
    let script = Arc::new(Mutex::new(Script::default()));
    let (f, entered, gate) = gated(Harness::Codex, &script);
    let h = spawn(f);
    // The initial refresh is now running and blocked inside the adapter.
    entered.recv_timeout(WAIT).unwrap();
    let a = key(Harness::Codex, "a");
    let b = key(Harness::Codex, "b");
    for _ in 0..3 {
        h.tx.send(WorkerRequest::Refresh).unwrap();
    }
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: a,
    })
    .unwrap();
    h.tx.send(WorkerRequest::Select {
        request_id: 2,
        key: b.clone(),
    })
    .unwrap();
    h.tx.send(WorkerRequest::LoadPage {
        request_id: 1,
        key: b,
        anchor: None,
        direction: PageDirection::Backward,
    })
    .unwrap();
    gate.send(()).unwrap();
    gate.send(()).unwrap();

    let events = until(&h, finished(2));
    let transcripts: Vec<u64> = events
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::Transcript { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .collect();
    assert_eq!(
        transcripts,
        vec![2],
        "only the latest selection is answered"
    );
    // Three Refresh requests collapsed into exactly one extra pass.
    assert!(matches!(
        h.rx.recv_timeout(Duration::from_millis(400)),
        Err(RecvTimeoutError::Timeout)
    ));
    let s = script.lock().unwrap();
    assert_eq!(s.refresh_calls, 2);
    assert_eq!(s.page_calls, 1);
    drop(s);
    assert!(h.shutdown());
}

#[test]
fn services_page_requests_between_refresh_chunks() {
    let script = Arc::new(Mutex::new(Script::default()));
    {
        let mut s = script.lock().unwrap();
        s.refreshes.push_back(Ok(delta(
            vec![summary(Harness::Codex, "a", 1, 1)],
            vec![],
            false,
        )));
        s.refreshes.push_back(Ok(delta(vec![], vec![], true)));
    }
    let (f, entered, gate) = gated(Harness::Codex, &script);
    let h = spawn(f);
    entered.recv_timeout(WAIT).unwrap(); // chunk 1 running
    h.tx.send(WorkerRequest::Select {
        request_id: 7,
        key: key(Harness::Codex, "a"),
    })
    .unwrap();
    gate.send(()).unwrap(); // chunk 1 returns (not done)
    entered.recv_timeout(WAIT).unwrap(); // chunk 2 is now blocked

    // The page must be answered before chunk 2 is allowed to finish.
    let events = until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::RefreshFinished { .. }))
    );
    assert!(events.iter().any(|e| matches!(
        e,
        WorkerEvent::Roster { snapshot, .. } if snapshot.scan.iter().any(|s| !s.done)
    )));
    gate.send(()).unwrap();
    until(&h, finished(1));
    assert!(h.shutdown());
}

#[test]
fn provider_failure_keeps_last_good_sessions_and_replaces_warning() {
    let script = Arc::new(Mutex::new(Script::default()));
    let w1 = warning(Harness::Codex, "root", WarningKind::Parse, "bad line");
    let w2 = warning(
        Harness::Codex,
        "root",
        WarningKind::Unsupported,
        "newer schema",
    );
    {
        let mut s = script.lock().unwrap();
        s.refreshes.push_back(Ok(delta(
            vec![summary(Harness::Codex, "a", 1, 1)],
            vec![w1.clone()],
            true,
        )));
        s.refreshes
            .push_back(Err(ProviderError::Io("disk gone".into())));
        s.refreshes
            .push_back(Ok(delta(vec![], vec![w2.clone()], true)));
        s.refreshes.push_back(Ok(delta(vec![], vec![], true)));
    }
    let h = spawn(fake(Harness::Codex, &script));

    let ev = until(&h, finished(1));
    let (sessions, warnings) = last_roster(&ev);
    assert_eq!(sessions.len(), 1);
    assert_eq!(warnings, std::slice::from_ref(&w1));

    h.tx.send(WorkerRequest::Refresh).unwrap();
    let ev = until(&h, finished(2));
    let (sessions, warnings) = last_roster(&ev);
    assert_eq!(sessions.len(), 1, "last good sessions survive a failure");
    assert!(warnings.contains(&w1));
    assert!(warnings.iter().any(|w| w.key.source == "refresh"
        && w.key.kind == WarningKind::Io
        && w.message.contains("disk gone")));

    h.tx.send(WorkerRequest::Refresh).unwrap();
    let ev = until(&h, finished(3));
    let (sessions, warnings) = last_roster(&ev);
    assert_eq!(sessions.len(), 1);
    assert_eq!(warnings, [w2], "warnings are replaced, not appended");

    h.tx.send(WorkerRequest::Refresh).unwrap();
    let ev = until(&h, finished(4));
    let (_, warnings) = last_roster(&ev);
    assert!(warnings.is_empty(), "recovery clears the key");
    assert!(h.shutdown());
}

#[test]
fn opencode_in_place_update_emits_transcript_invalidated_for_selected_session() {
    let script = Arc::new(Mutex::new(Script::default()));
    {
        let mut s = script.lock().unwrap();
        s.refreshes.push_back(Ok(delta(
            vec![
                summary(Harness::OpenCode, "a", 10, 3),
                summary(Harness::OpenCode, "b", 10, 3),
            ],
            vec![],
            true,
        )));
        // Rows were edited in place; the adapter names the sessions.
        s.refreshes.push_back(Ok(with_invalidated(
            delta(
                vec![
                    summary(Harness::OpenCode, "a", 20, 3),
                    summary(Harness::OpenCode, "b", 20, 3),
                ],
                vec![],
                true,
            ),
            vec![key(Harness::OpenCode, "a"), key(Harness::OpenCode, "b")],
        )));
    }
    let h = spawn(fake(Harness::OpenCode, &script));
    until(&h, finished(1));
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Harness::OpenCode, "a"),
    })
    .unwrap();
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));

    h.tx.send(WorkerRequest::Refresh).unwrap();
    let ev = until(&h, finished(2));
    let invalidated: Vec<&SessionKey> = ev
        .iter()
        .filter_map(|e| match e {
            WorkerEvent::TranscriptInvalidated { key } => Some(key),
            _ => None,
        })
        .collect();
    assert_eq!(invalidated, vec![&key(Harness::OpenCode, "a")]);
    assert!(h.shutdown());
}

#[test]
fn unavailable_page_is_retried_later_without_busy_looping() {
    let script = Arc::new(Mutex::new(Script::default()));
    {
        let mut s = script.lock().unwrap();
        s.pages.push_back(Err(ProviderError::Unavailable(
            "transcript index is building (0 of 9 bytes); retry".into(),
        )));
        s.pages.push_back(Err(ProviderError::Unavailable(
            "transcript index is building (5 of 9 bytes); retry".into(),
        )));
    }
    let h = spawn(fake(Harness::Codex, &script));
    until(&h, finished(1));
    let start = Instant::now();
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Harness::Codex, "a"),
    })
    .unwrap();
    until(&h, |e| {
        matches!(e, WorkerEvent::Transcript { request_id: 1, .. })
    });
    assert!(
        start.elapsed() >= Duration::from_millis(400),
        "two retries must be spaced out, took {:?}",
        start.elapsed()
    );
    assert_eq!(script.lock().unwrap().page_calls, 3);
    assert!(h.shutdown());
}

#[test]
fn processes_are_merged_into_published_roster() {
    let script = Arc::new(Mutex::new(Script::default()));
    script.lock().unwrap().refreshes.push_back(Ok(delta(
        vec![summary(Harness::Codex, "01a1", 1, 1)],
        vec![],
        true,
    )));
    let mut c = config();
    c.process_enabled = true;
    let sampler = Box::new(|now| {
        Ok(ProcessSnapshot {
            sampled_at: now,
            processes: vec![ProcessInfo::new(
                42,
                Some(1_000),
                "codex".into(),
                None,
                Some(vec!["codex".into(), "resume".into(), "01a1".into()]),
            )],
        })
    });
    let h =
        WorkerHandle::spawn_with_sampler(vec![Box::new(fake(Harness::Codex, &script))], c, sampler);
    // The hydrated summary arrives after the frozen page; its process is associated.
    let ev = until(&h, |e| {
        matches!(e, WorkerEvent::Roster { snapshot, .. }
            if snapshot.summaries.first().is_some_and(|s| s.process != ProcessAssociation::NotChecked))
    });
    assert!(
        ev.iter()
            .any(|e| matches!(e, WorkerEvent::Processes { .. }))
    );
    let Some(WorkerEvent::Roster { snapshot, .. }) = ev.last() else {
        panic!("roster expected");
    };
    assert_eq!(
        snapshot.summaries[0].process,
        ProcessAssociation::Matched {
            pid: 42,
            started_at: Some(1_000)
        }
    );
    assert!(h.shutdown());
}

#[test]
fn shutdown_joins_without_panicking() {
    let script = Arc::new(Mutex::new(Script::default()));
    let h = spawn(fake(Harness::Codex, &script));
    until(&h, finished(1));
    assert!(h.shutdown());

    // A consumer that never reads must not wedge shutdown: the worker keeps
    // producing chunks until its bounded event queue fills.
    let script = Arc::new(Mutex::new(Script {
        endless: true,
        ..Script::default()
    }));
    let h = spawn(fake(Harness::Codex, &script));
    std::thread::sleep(Duration::from_millis(200));
    let start = Instant::now();
    assert!(h.shutdown());
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
fn invalidation_follows_the_adapter_signal_not_summary_changes() {
    let script = Arc::new(Mutex::new(Script::default()));
    {
        let mut s = script.lock().unwrap();
        s.refreshes.push_back(Ok(delta(
            vec![summary(Harness::Claude, "a", 10, 3)],
            vec![],
            true,
        )));
        // Persisted time and count both moved, but nothing was revised.
        s.refreshes.push_back(Ok(delta(
            vec![summary(Harness::Claude, "a", 20, 2)],
            vec![],
            true,
        )));
        // Same persisted time and count, content revised.
        s.refreshes.push_back(Ok(with_invalidated(
            delta(vec![summary(Harness::Claude, "a", 20, 2)], vec![], true),
            vec![key(Harness::Claude, "a")],
        )));
    }
    let h = spawn(fake(Harness::Claude, &script));
    until(&h, finished(1));
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Harness::Claude, "a"),
    })
    .unwrap();
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    let count = |ev: &[WorkerEvent]| {
        ev.iter()
            .filter(|e| matches!(e, WorkerEvent::TranscriptInvalidated { .. }))
            .count()
    };
    h.tx.send(WorkerRequest::Refresh).unwrap();
    assert_eq!(count(&until(&h, finished(2))), 0);
    h.tx.send(WorkerRequest::Refresh).unwrap();
    assert_eq!(count(&until(&h, finished(3))), 1);
    assert!(h.shutdown());
}

#[test]
fn process_sampling_is_not_starved_by_faster_refreshes() {
    let script = Arc::new(Mutex::new(Script::default()));
    let mut c = config();
    c.refresh_ms = 40;
    c.process_ms = 200;
    c.process_enabled = true;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let sampler = Box::new(move |now| {
        counter.fetch_add(1, Ordering::SeqCst);
        Ok(ProcessSnapshot {
            sampled_at: now,
            processes: vec![],
        })
    });
    let h =
        WorkerHandle::spawn_with_sampler(vec![Box::new(fake(Harness::Codex, &script))], c, sampler);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1_100) {
        let _ = h.rx.recv_timeout(Duration::from_millis(20));
    }
    let n = calls.load(Ordering::SeqCst);
    assert!(n >= 4, "expected periodic samples, got {n}");
    assert!(h.shutdown());
}

#[test]
fn selection_arriving_during_a_page_load_is_served_without_more_messages() {
    let script = Arc::new(Mutex::new(Script::default()));
    let (entered_tx, entered) = mpsc::channel();
    let (release, gate_rx) = mpsc::channel();
    let mut f = fake(Harness::Codex, &script);
    f.page_gate = Some((entered_tx, gate_rx));
    let h = spawn(f);
    until(&h, finished(1));
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Harness::Codex, "a"),
    })
    .unwrap();
    entered.recv_timeout(WAIT).unwrap(); // request 1 is inside the adapter
    h.tx.send(WorkerRequest::Select {
        request_id: 2,
        key: key(Harness::Codex, "b"),
    })
    .unwrap();
    release.send(()).unwrap();
    release.send(()).unwrap();
    // No further messages are sent; request 2 must still be answered.
    let events = until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::Transcript { request_id: 2, .. })
    ));
    assert!(h.shutdown());
}

#[test]
fn initial_selection_requests_the_newest_page_backward() {
    let script = Arc::new(Mutex::new(Script::default()));
    let h = spawn(fake(Harness::Codex, &script));
    until(&h, finished(1));
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Harness::Codex, "a"),
    })
    .unwrap();
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert_eq!(
        script.lock().unwrap().page_requests,
        vec![(None, PageDirection::Backward)]
    );
    assert!(h.shutdown());
}

#[test]
fn requests_are_serviced_at_provider_boundaries() {
    let a = Arc::new(Mutex::new(Script::default()));
    let b = Arc::new(Mutex::new(Script::default()));
    let (fa, entered_a, gate_a) = gated(Harness::Codex, &a);
    let (fb, entered_b, gate_b) = gated(Harness::Claude, &b);
    let h = WorkerHandle::spawn(vec![Box::new(fa), Box::new(fb)], config());
    entered_a.recv_timeout(WAIT).unwrap();
    h.tx.send(WorkerRequest::Select {
        request_id: 1,
        key: key(Harness::Codex, "a"),
    })
    .unwrap();
    // Provider A finishes (done) in this single chunk; the page must be
    // answered before provider B's chunk is released.
    gate_a.send(()).unwrap();
    entered_b.recv_timeout(WAIT).unwrap();
    let events = until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::RefreshFinished { .. }))
    );
    gate_b.send(()).unwrap();
    until(&h, finished(1));
    assert!(h.shutdown());
}

fn select(h: &WorkerHandle, id: u64, k: &SessionKey) {
    h.tx.send(WorkerRequest::Select {
        request_id: id,
        key: k.clone(),
    })
    .unwrap();
}

fn anchor(n: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(n as i64),
        source_order: n,
        stable_id: format!("e{n}"),
    }
}

#[test]
fn count_after_is_answered_with_the_adapter_count() {
    let script = Arc::new(Mutex::new(Script::default()));
    script.lock().unwrap().count = 7;
    let h = spawn(fake(Harness::Codex, &script));
    let k = key(Harness::Codex, "a");
    select(&h, 1, &k);
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    h.tx.send(WorkerRequest::CountAfter {
        request_id: 2,
        key: k,
        anchor: anchor(3),
    })
    .unwrap();
    let events = until(&h, |e| matches!(e, WorkerEvent::EventsAfter { .. }));
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::EventsAfter {
            request_id: 2,
            count: 7
        })
    ));
    assert!(h.shutdown());
}

#[test]
fn page_failures_carry_the_request_id() {
    let script = Arc::new(Mutex::new(Script::default()));
    script
        .lock()
        .unwrap()
        .pages
        .push_back(Err(ProviderError::Parse("bad".into())));
    let h = spawn(fake(Harness::Codex, &script));
    select(&h, 5, &key(Harness::Codex, "a"));
    let events = until(&h, |e| matches!(e, WorkerEvent::LoadFailed { .. }));
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::LoadFailed { request_id: 5 })
    ));
    assert!(h.shutdown());
}

#[test]
fn requested_page_size_never_exceeds_the_ui_window() {
    let script = Arc::new(Mutex::new(Script::default()));
    let mut c = config();
    c.max_page_events = 1_000_000;
    let h = WorkerHandle::spawn(vec![Box::new(fake(Harness::Codex, &script))], c);
    select(&h, 1, &key(Harness::Codex, "a"));
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    assert_eq!(
        script.lock().unwrap().page_max_events,
        vec![agentscope::app::MAX_LOADED_EVENTS]
    );
    assert!(h.shutdown());
}

/// An OpenCode-like reader that owns one database and rejects any other
/// database's keys, as the real scanner does.
struct DbReader {
    database: &'static str,
    session: SessionSummary,
    served: Arc<Mutex<Vec<(&'static str, &'static str)>>>,
}

impl DbReader {
    fn owns(&self, key: &SessionKey) -> bool {
        key == &self.session.meta.key
    }
    fn note(&self, what: &'static str) {
        self.served.lock().unwrap().push((self.database, what));
    }
}

impl ProviderReader for DbReader {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }
    fn discover(
        &mut self,
        _: Timestamp,
        _: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        Ok(DiscoveryDelta {
            sessions: vec![self.session.meta.clone()],
            removed: vec![],
            invalidated: vec![],
            warnings: vec![],
            scan: ScanProgress {
                harness: Harness::OpenCode,
                scanned: 1,
                total: Some(1),
                done: true,
            },
        })
    }
    fn hydrate(
        &mut self,
        _: ReadJobId,
        key: &SessionKey,
        _: Timestamp,
        _: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        if !self.owns(key) {
            return Err(ProviderError::Unsupported("not an OpenCode session".into()));
        }
        Ok(HydrationDelta {
            summary: self.session.clone(),
            done: true,
        })
    }
    fn cancel_read(&mut self, _: &SessionKey, _: ReadJobId) {}
    fn details_page(
        &mut self,
        _: ReadJobId,
        _: &SessionKey,
        _: Option<PageAnchor>,
        _: PageRequest,
        _: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError> {
        Ok(ReadProgress::Ready(SessionDetailPage::default()))
    }
    fn transcript_page(
        &mut self,
        _: ReadJobId,
        key: &SessionKey,
        _: Option<PageAnchor>,
        _: PageDirection,
        _: PageRequest,
        _: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        if !self.owns(key) {
            return Err(ProviderError::Unsupported("not an OpenCode session".into()));
        }
        self.note("page");
        Ok(ReadProgress::Ready(empty_page()))
    }
    fn count_events_after(
        &mut self,
        _: ReadJobId,
        key: &SessionKey,
        _: &PageAnchor,
        _: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        if !self.owns(key) {
            return Err(ProviderError::Unsupported("not an OpenCode session".into()));
        }
        self.note("count");
        Ok(ReadProgress::Ready(3))
    }
    fn source_record(
        &mut self,
        _: ReadJobId,
        locator: &SourceLocator,
        _: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        if !self.owns(&locator.session) {
            return Err(ProviderError::Unsupported("not an OpenCode session".into()));
        }
        self.note("source");
        Ok(ReadProgress::Ready(SourceRecord {
            raw: "raw".into(),
            display: "display".into(),
            unavailable: None,
        }))
    }
}

#[test]
fn loads_reach_the_reader_that_owns_the_database() {
    let served = Arc::new(Mutex::new(Vec::new()));
    let mk = |database: &'static str| {
        let key = SessionKey {
            harness: Harness::OpenCode,
            native_id: "same".into(),
            source: SourceKey::DbRow {
                database: database.into(),
                table: "session_v2".into(),
                id: "same".into(),
            },
        };
        DbReader {
            database,
            session: facts::summary(metadata(key, "t")),
            served: served.clone(),
        }
    };
    let (a, b) = (mk("a/state.db"), mk("b/state.db"));
    let kb = b.session.meta.key.clone();
    let h = WorkerHandle::spawn(vec![Box::new(a), Box::new(b)], config());
    until(&h, |e| matches!(e, WorkerEvent::RefreshFinished { .. }));

    select(&h, 1, &kb);
    until(&h, |e| matches!(e, WorkerEvent::Transcript { .. }));
    h.tx.send(WorkerRequest::CountAfter {
        request_id: 2,
        key: kb.clone(),
        anchor: anchor(1),
    })
    .unwrap();
    let events = until(&h, |e| matches!(e, WorkerEvent::EventsAfter { .. }));
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::EventsAfter {
            request_id: 2,
            count: 3
        })
    ));
    h.tx.send(WorkerRequest::LoadSource {
        request_id: 3,
        locator: SourceLocator {
            session: kb,
            kind: LocatorKind::DbRow {
                table: "session_v2".into(),
                row_id: "same".into(),
                part_index: None,
            },
            generation: None,
        },
    })
    .unwrap();
    until(&h, |e| {
        matches!(e, WorkerEvent::Source { request_id: 3, .. })
    });
    assert!(h.shutdown());
    assert_eq!(
        *served.lock().unwrap(),
        [
            ("b/state.db", "page"),
            ("b/state.db", "count"),
            ("b/state.db", "source")
        ]
    );
}

#[test]
fn an_in_flight_preview_superseded_by_b_is_released_without_answering() {
    let script = Arc::new(Mutex::new(Script::default()));
    let a = summary(Harness::Codex, "a", 10, 3);
    let b = summary(Harness::Codex, "b", 20, 4);
    {
        let mut s = script.lock().unwrap();
        s.known.insert(a.meta.key.clone(), a.clone());
        s.known.insert(b.meta.key.clone(), b.clone());
    }
    let (f, entered, release) = gated_hydrate(Harness::Codex, &script);
    let h = spawn(f);
    until(&h, finished(1));
    h.tx.send(WorkerRequest::HydratePreview {
        request_id: 1,
        key: a.meta.key.clone(),
    })
    .unwrap();
    // A is inside its adapter slice when B supersedes it.
    entered.recv_timeout(WAIT).unwrap();
    h.tx.send(WorkerRequest::HydratePreview {
        request_id: 2,
        key: b.meta.key.clone(),
    })
    .unwrap();
    release.send(()).unwrap(); // A's slice returns and is discarded
    release.send(()).unwrap(); // B's slice runs to completion

    let events = until(
        &h,
        |e| matches!(e, WorkerEvent::Preview { request_id: 2, key, done: true, .. } if key == &b.meta.key),
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Preview { request_id: 1, .. })),
        "the superseded in-flight preview never answers"
    );
    let s = script.lock().unwrap();
    assert!(
        s.cancelled.iter().any(|(key, _)| key == &a.meta.key),
        "the running preview job is released when superseded"
    );
    drop(s);
    assert!(h.shutdown());
}

#[test]
fn an_in_flight_details_slice_can_be_cancelled_without_answering() {
    let script = Arc::new(Mutex::new(Script::default()));
    let a = summary(Harness::Codex, "a", 10, 3);
    let k = a.meta.key.clone();
    script.lock().unwrap().known.insert(k.clone(), a);
    let (f, entered, release) = gated_details(Harness::Codex, &script);
    let h = spawn(f);
    until(&h, finished(1));
    h.tx.send(WorkerRequest::LoadDetails {
        request_id: 1,
        key: k.clone(),
        anchor: None,
    })
    .unwrap();
    entered.recv_timeout(WAIT).unwrap(); // details slice is running
    h.tx.send(WorkerRequest::CancelLoad { request_id: 1 })
        .unwrap();
    release.send(()).unwrap();
    // A refresh is a deterministic completion boundary for the cancelled slice.
    h.tx.send(WorkerRequest::Refresh).unwrap();
    let events = until(&h, finished(2));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Details { request_id: 1, .. }))
    );
    {
        let s = script.lock().unwrap();
        assert_eq!(s.cancelled.iter().filter(|(key, _)| key == &k).count(), 1);
    }
    // A fresh request on the same key starts cleanly.
    h.tx.send(WorkerRequest::LoadDetails {
        request_id: 2,
        key: k,
        anchor: None,
    })
    .unwrap();
    release.send(()).unwrap();
    let events = until(&h, |e| {
        matches!(e, WorkerEvent::Details { request_id: 2, .. })
    });
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::Details { request_id: 2, .. })
    ));
    assert!(h.shutdown());
}

#[test]
fn cancelling_details_releases_only_that_job_and_the_transcript_completes() {
    let script = Arc::new(Mutex::new(Script::default()));
    let a = summary(Harness::Codex, "a", 10, 3);
    let k = a.meta.key.clone();
    {
        let mut s = script.lock().unwrap();
        s.known.insert(k.clone(), a);
        s.pages
            .push_back(Ok(ReadProgress::Ready(event_page(&k, "e0"))));
        s.pages
            .push_back(Ok(ReadProgress::Ready(event_page(&k, "fresh"))));
    }
    let (f, entered, gate) = gated(Harness::Codex, &script);
    let h = spawn(f);
    entered.recv_timeout(WAIT).unwrap();
    h.tx.send(WorkerRequest::LoadDetails {
        request_id: 1,
        key: k.clone(),
        anchor: None,
    })
    .unwrap();
    h.tx.send(WorkerRequest::Select {
        request_id: 2,
        key: k.clone(),
    })
    .unwrap();
    h.tx.send(WorkerRequest::CancelLoad { request_id: 1 })
        .unwrap();
    gate.send(()).unwrap();

    let events = until(&h, |e| {
        matches!(e, WorkerEvent::Transcript { request_id: 2, .. })
    });
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::Details { request_id: 1, .. })),
        "the cancelled details job never answers"
    );
    {
        let s = script.lock().unwrap();
        assert_eq!(
            s.cancelled.iter().filter(|(key, _)| key == &k).count(),
            1,
            "only the details job was released"
        );
    }

    // The transcript selection still owns its worker state: a fresh page loads.
    h.tx.send(WorkerRequest::LoadPage {
        request_id: 3,
        key: k.clone(),
        anchor: None,
        direction: PageDirection::Backward,
    })
    .unwrap();
    let events = until(&h, |e| {
        matches!(e, WorkerEvent::Transcript { request_id: 3, .. })
    });
    match events.last() {
        Some(WorkerEvent::Transcript { page, .. }) => {
            assert_eq!(page.events.first().map(|e| e.id.as_str()), Some("fresh"));
        }
        other => panic!("expected a fresh transcript page, got {other:?}"),
    }
    assert!(h.shutdown());
}

#[test]
fn a_preview_rehydrates_after_its_cached_summary_was_evicted() {
    let script = Arc::new(Mutex::new(Script::default()));
    let a = summary(Harness::Codex, "a", 10, 3);
    let b = summary(Harness::Codex, "b", 20, 4);
    {
        let mut s = script.lock().unwrap();
        s.known.insert(a.meta.key.clone(), a.clone());
        s.known.insert(b.meta.key.clone(), b.clone());
        // Discovery keeps both metadata records; only one summary fits the cache.
        s.refreshes
            .push_back(Ok(delta(vec![a.clone(), b.clone()], vec![], true)));
    }
    let mut c = config();
    c.max_cached_sessions = 1;
    let h = WorkerHandle::spawn(vec![Box::new(fake(Harness::Codex, &script))], c);
    until(&h, finished(1));

    for (id, key) in [&a.meta.key, &b.meta.key, &a.meta.key]
        .into_iter()
        .enumerate()
    {
        let id = id as u64 + 1;
        h.tx.send(WorkerRequest::HydratePreview {
            request_id: id,
            key: key.clone(),
        })
        .unwrap();
        let events = until(&h, |e| {
            matches!(e, WorkerEvent::Preview { request_id, key: got, done: true, .. }
                if *request_id == id && got == key)
        });
        let Some(WorkerEvent::Preview { summary, .. }) = events.last() else {
            panic!("expected a completed preview");
        };
        assert_eq!(&summary.meta.key, key);
    }
    let calls = script.lock().unwrap().hydrate_calls.clone();
    assert!(
        calls.iter().filter(|key| *key == &a.meta.key).count() >= 2,
        "A is hydrated again from its metadata, not lost with the summary"
    );
    assert!(h.shutdown());
}

#[test]
fn preview_tail_returns_newest_page_with_own_job() {
    let script = Arc::new(Mutex::new(Script::default()));
    let (entered_tx, entered) = mpsc::channel();
    let (release, gate_rx) = mpsc::channel();
    let mut f = fake(Harness::Codex, &script);
    f.page_gate = Some((entered_tx, gate_rx));
    let h = spawn(f);
    let k = key(Harness::Codex, "a");
    until(&h, finished(1));

    // The transcript read for the same key is inside its slice.
    select(&h, 1, &k);
    entered.recv_timeout(WAIT).unwrap();
    // The preview tail is queued behind it, then gets its own slice.
    h.tx.send(WorkerRequest::PreviewTail {
        request_id: 2,
        key: k.clone(),
        anchor: None,
    })
    .unwrap();
    release.send(()).unwrap();
    until(&h, |e| {
        matches!(e, WorkerEvent::Transcript { request_id: 1, .. })
    });
    entered.recv_timeout(WAIT).unwrap();
    // A further tail is queued and cancelled while the second one runs, so the
    // cancellation reaches the queued load rather than a detached slice.
    h.tx.send(WorkerRequest::PreviewTail {
        request_id: 3,
        key: k.clone(),
        anchor: None,
    })
    .unwrap();
    h.tx.send(WorkerRequest::CancelLoad { request_id: 3 })
        .unwrap();
    release.send(()).unwrap();
    let events = until(&h, |e| {
        matches!(e, WorkerEvent::PreviewTail { request_id: 2, .. })
    });
    assert!(matches!(
        events.last(),
        Some(WorkerEvent::PreviewTail { key: got, .. }) if got == &k
    ));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, WorkerEvent::PreviewTail { request_id: 3, .. })),
        "a cancelled tail never answers"
    );

    // The transcript continuation still works after the tail job was released.
    h.tx.send(WorkerRequest::LoadPage {
        request_id: 4,
        key: k.clone(),
        anchor: Some(anchor(1)),
        direction: PageDirection::Backward,
    })
    .unwrap();
    entered.recv_timeout(WAIT).unwrap();
    release.send(()).unwrap();
    until(&h, |e| {
        matches!(e, WorkerEvent::Transcript { request_id: 4, .. })
    });

    let s = script.lock().unwrap();
    assert_eq!(s.page_jobs.len(), 3);
    assert_ne!(
        s.page_jobs[0], s.page_jobs[1],
        "the tail must not share the transcript's job"
    );
    assert_eq!(s.page_requests[1], (None, PageDirection::Backward));
    assert_eq!(
        s.page_requests[2],
        (Some(anchor(1)), PageDirection::Backward)
    );
    assert_eq!(s.cancelled.len(), 1);
    assert_eq!(s.cancelled[0].0, k);
    assert_ne!(s.cancelled[0].1, s.page_jobs[0]);
    assert_ne!(s.cancelled[0].1, s.page_jobs[1]);
    assert!(s.page_keys.iter().all(|seen| seen == &k));
    drop(s);
    assert!(h.shutdown());
}

#[test]
fn preview_tail_pending_is_requeued_until_done() {
    let script = Arc::new(Mutex::new(Script::default()));
    let k = key(Harness::Codex, "a");
    {
        let mut s = script.lock().unwrap();
        s.pages.push_back(Ok(ReadProgress::Pending(WorkProgress {
            records: 2,
            expanded_bytes: 8,
        })));
        s.pages
            .push_back(Ok(ReadProgress::Ready(event_page(&k, "reply"))));
    }
    let h = spawn(fake(Harness::Codex, &script));
    until(&h, finished(1));
    h.tx.send(WorkerRequest::PreviewTail {
        request_id: 9,
        key: k.clone(),
        anchor: None,
    })
    .unwrap();
    let events = until(&h, |e| {
        matches!(e, WorkerEvent::PreviewTail { request_id: 9, .. })
    });
    assert!(
        events.iter().any(|e| matches!(
            e,
            WorkerEvent::LoadProgress {
                request_id: 9,
                records: 2,
                expanded_bytes: 8
            }
        )),
        "pending progress is reported while the tail is retried"
    );
    match events.last() {
        Some(WorkerEvent::PreviewTail { key: got, page, .. }) => {
            assert_eq!(got, &k);
            assert_eq!(page.events.first().map(|e| e.id.as_str()), Some("reply"));
        }
        other => panic!("expected a completed preview tail, got {other:?}"),
    }
    let s = script.lock().unwrap();
    assert_eq!(s.page_calls, 2, "pending is requeued once, then done");
    assert_eq!(
        s.page_requests,
        vec![
            (None, PageDirection::Backward),
            (None, PageDirection::Backward)
        ]
    );
    drop(s);
    assert!(h.shutdown());
}
