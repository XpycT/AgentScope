//! Frozen-generation root paging, child batches, cache eviction and metadata ceilings
//! (spec §5), first on the index with synthetic metadata, then end to end through the
//! real native readers and the worker.

mod support;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentscope::app::{App, RosterItem};
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::*;
use agentscope::providers::DiscoveryDelta;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::opencode::OpenCodeScanner;
use agentscope::roster::{CHILD_PAGE, MetadataLimits, ROOT_PAGE, RosterIndex, RosterSnapshot};
use agentscope::worker::{WorkerEvent, WorkerHandle, WorkerRequest};
use serde_json::json;
use support::facts::{file_key, metadata};
use support::opencode as oc;

const BASE: i64 = 1_793_000_000_000;

fn limits() -> MetadataLimits {
    MetadataLimits {
        max_sources: 100_000,
        max_bytes: 64 * 1024 * 1024,
    }
}

fn key(harness: Harness, id: &str) -> SessionKey {
    file_key(harness, id, &PathBuf::from(format!("{id}.jsonl")))
}

fn meta(harness: Harness, id: &str, at: Option<i64>) -> SessionMetadata {
    let mut m = metadata(key(harness, id), id);
    if let Some(at) = at {
        m.activity = ActivityStamp {
            at: Some(at),
            class: AggregationClass::Reported,
        };
    }
    m
}

fn with_parent(mut m: SessionMetadata, parent: &SessionKey) -> SessionMetadata {
    m.lineage.parent = Some(SessionLink::Resolved(parent.clone()));
    m
}

fn delta(harness: Harness, sessions: Vec<SessionMetadata>) -> DiscoveryDelta {
    DiscoveryDelta {
        sessions,
        removed: vec![],
        invalidated: vec![],
        warnings: vec![],
        scan: ScanProgress {
            harness,
            scanned: 0,
            total: None,
            done: true,
        },
    }
}

fn ids(snapshot: &RosterSnapshot) -> Vec<String> {
    snapshot.roots.iter().map(|k| k.native_id.clone()).collect()
}

/// 125 top-level groups: 42 Codex, 42 Claude, 41 OpenCode, among them one fork and two
/// sessions with no recorded activity; plus 80 children under one OpenCode parent, one
/// of which is the newest activity anywhere.
struct Store {
    roots: Vec<SessionMetadata>,
    children: Vec<SessionMetadata>,
    parent: SessionKey,
    /// Expected root IDs, newest group first.
    order: Vec<String>,
}

fn store() -> Store {
    let mut roots = Vec::new();
    let harness_of = |i: usize| match i % 3 {
        0 => Harness::Codex,
        1 => Harness::Claude,
        _ => Harness::OpenCode,
    };
    let unknown = [7usize, 99];
    for i in 0..125usize {
        let id = format!("root-{i:03}");
        // Activity grows with the index but is deliberately not harness-ordered.
        let at = (!unknown.contains(&i)).then(|| BASE + (i as i64) * 60_000);
        roots.push(meta(harness_of(i), &id, at));
    }
    // One fork: it is its own top-level group, never a child of what it forked.
    let forked_from = roots[5].key.clone();
    roots[120].lineage.fork = Some(SessionLink::Resolved(forked_from));
    // The family: root-010 is the parent; its 80 children are older than most roots
    // except `child-079`, which is newer than everything.
    let parent = roots[10].key.clone();
    let children: Vec<SessionMetadata> = (0..80usize)
        .map(|n| {
            let at = if n == 79 {
                BASE + 10_000_000
            } else {
                BASE + (n as i64) * 1_000
            };
            with_parent(
                meta(Harness::OpenCode, &format!("child-{n:03}"), Some(at)),
                &parent,
            )
        })
        .collect();
    // Expected: group activity desc (root-010 inherits child-079's), unknown last,
    // stable key order among equals.
    let group = |i: usize| -> Option<i64> {
        if i == 10 {
            Some(BASE + 10_000_000)
        } else {
            roots[i].activity.at
        }
    };
    let mut order: Vec<usize> = (0..125).collect();
    order.sort_by(|a, b| match (group(*a), group(*b)) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => agentscope::roster::cmp_keys(&roots[*a].key, &roots[*b].key),
    });
    let order = order.into_iter().map(|i| format!("root-{i:03}")).collect();
    Store {
        roots,
        children,
        parent,
        order,
    }
}

fn fill(index: &mut RosterIndex, store: &Store) {
    // Discovery arrives in slices, per harness, children before parents included.
    index.apply_discovery(delta(Harness::OpenCode, store.children.clone()));
    for harness in [Harness::Codex, Harness::Claude, Harness::OpenCode] {
        index.apply_discovery(delta(
            harness,
            store
                .roots
                .iter()
                .filter(|m| m.key.harness == harness)
                .cloned()
                .collect(),
        ));
    }
}

#[test]
fn root_pages_are_exact_distinct_and_unaffected_by_children_forks_and_unknown_activity() {
    let store = store();
    let mut index = RosterIndex::new(limits(), 2_000);
    fill(&mut index, &store);

    let provisional = index.snapshot();
    assert!(provisional.provisional, "unfrozen discovery is labeled");
    index.freeze(1);

    let first = index.snapshot();
    assert!(!first.provisional);
    assert_eq!(first.generation, 1);
    let expected: Vec<String> = store.order[..ROOT_PAGE].to_vec();
    assert_eq!(ids(&first), expected);
    assert_eq!(first.roots.len(), 50);
    assert_eq!(
        first.roots[0].native_id, "root-010",
        "a child's newest activity makes its parent's group newest"
    );
    assert!(first.has_more);
    assert_eq!(
        first.metadata.len(),
        50,
        "only loaded roots are copied into the snapshot"
    );

    assert!(index.load_more(1));
    let after_more = index.snapshot();
    assert_eq!(after_more.roots.len(), 100);
    assert_eq!(ids(&after_more), store.order[..100].to_vec());
    assert!(after_more.has_more);

    assert!(index.load_more(1));
    let after_final = index.snapshot();
    assert_eq!(after_final.roots.len(), 125);
    assert_eq!(ids(&after_final), store.order);
    assert!(!after_final.has_more);
    assert!(!index.load_more(1), "nothing is left to reveal");

    // No duplicate or skipped group across the accumulated pages.
    let distinct: HashSet<&String> = store.order.iter().collect();
    assert_eq!(distinct.len(), 125);
    let revealed: HashSet<String> = ids(&after_final).into_iter().collect();
    for i in 0..125 {
        assert!(
            revealed.contains(&format!("root-{i:03}")),
            "root-{i:03} skipped"
        );
    }
    // The fork occupies a top-level slot; unknown activity sorts after every timestamp.
    assert!(revealed.contains("root-120"));
    let last_two: Vec<&str> = store.order[123..].iter().map(String::as_str).collect();
    // Stable key order (harness first): root-099 is Codex, root-007 is Claude.
    assert_eq!(last_two, ["root-099", "root-007"]);
    for child in &store.children {
        assert!(
            !revealed.contains(&child.key.native_id),
            "a child never takes a root slot"
        );
    }

    // Expanding the family leaves the root count and order unchanged.
    let page = index.children(&store.parent, None, CHILD_PAGE);
    assert_eq!(page.metadata.len(), 50);
    assert_eq!(
        page.metadata[0].key.native_id, "child-079",
        "children are newest first"
    );
    let next = page.next.clone().expect("30 children remain");
    let rest = index.children(&store.parent, Some(&next), CHILD_PAGE);
    assert_eq!(rest.metadata.len(), 30);
    assert!(rest.next.is_none());
    let child_ids: HashSet<String> = page
        .metadata
        .iter()
        .chain(&rest.metadata)
        .map(|m| m.key.native_id.clone())
        .collect();
    assert_eq!(child_ids.len(), 80, "no child repeated or skipped");
    assert_eq!(index.snapshot().roots.as_ref(), after_final.roots.as_ref());
    let expanded_first = {
        let mut fresh = RosterIndex::new(limits(), 2_000);
        fill(&mut fresh, &store);
        fresh.freeze(1);
        fresh.snapshot()
    };
    assert_eq!(expanded_first.roots.as_ref(), first.roots.as_ref());
    // A cursor that is not this family's yields nothing rather than a restart.
    let stray = key(Harness::Codex, "root-001");
    assert!(
        index
            .children(&store.parent, Some(&stray), 50)
            .metadata
            .is_empty()
    );
}

#[test]
fn a_stale_generation_cannot_load_more() {
    let store = store();
    let mut index = RosterIndex::new(limits(), 2_000);
    fill(&mut index, &store);
    index.freeze(1);
    index.freeze(2);
    let before = index.snapshot();
    assert!(!index.load_more(1), "generation 1 is gone");
    assert_eq!(index.snapshot().roots.as_ref(), before.roots.as_ref());
    assert!(index.load_more(2));
    assert_eq!(index.snapshot().roots.len(), 100);
    // An explicit refresh keeps the capacity the reader already revealed.
    index.freeze(3);
    assert_eq!(index.snapshot().roots.len(), 100);
}

#[test]
fn a_frozen_generation_ignores_new_sources_and_keeps_deleted_members_until_refresh() {
    let store = store();
    let mut index = RosterIndex::new(limits(), 2_000);
    fill(&mut index, &store);
    index.freeze(1);
    let before = index.snapshot();
    assert!(!before.refresh_available);

    // A brand-new newest source, and a loaded member that disappears.
    let gone = before.roots[3].clone();
    let mut changes = delta(
        Harness::Codex,
        vec![meta(Harness::Codex, "brand-new", Some(BASE + 99_000_000))],
    );
    changes.removed = vec![gone.clone()];
    index.apply_discovery(changes);

    let during = index.snapshot();
    assert_eq!(
        during.roots.as_ref(),
        before.roots.as_ref(),
        "membership is stable"
    );
    assert!(during.refresh_available);
    let slot = during
        .metadata
        .iter()
        .find(|m| m.key == gone)
        .expect("the deleted member keeps its slot");
    assert!(!slot.source_available, "marked Unavailable, not dropped");

    index.freeze(2);
    let after = index.snapshot();
    assert!(!after.refresh_available);
    assert_eq!(after.roots[0].native_id, "brand-new");
    assert!(!after.roots.contains(&gone));
}

#[test]
fn a_late_edge_waits_for_refresh_and_never_reshuffles_the_page() {
    let store = store();
    let mut index = RosterIndex::new(limits(), 2_000);
    fill(&mut index, &store);
    index.freeze(1);
    let before = index.snapshot();
    // Hydration discovers that root-050 is a child of root-051.
    let orphan = store.roots[50].clone();
    let mut revised = orphan.clone();
    revised.lineage.parent = Some(SessionLink::Resolved(store.roots[51].key.clone()));
    index.apply_discovery(delta(Harness::Codex, vec![revised]));
    let during = index.snapshot();
    assert_eq!(during.roots.as_ref(), before.roots.as_ref());
    assert!(during.refresh_available);
    index.freeze(2);
    assert!(
        !index
            .snapshot()
            .roots
            .iter()
            .any(|k| k.native_id == "root-050")
    );
}

#[test]
fn orphans_cycles_ambiguous_parents_and_continuations_never_hide_healthy_roots() {
    let mut index = RosterIndex::new(limits(), 2_000);
    let a = meta(Harness::Claude, "a", Some(BASE));
    let b = with_parent(meta(Harness::Claude, "b", Some(BASE + 1)), &a.key);
    let a = with_parent(a, &b.key); // a <-> b cycle
    let missing = with_parent(
        meta(Harness::Claude, "missing-parent", Some(BASE + 2)),
        &key(Harness::Claude, "not-discovered"),
    );
    let mut unresolved = meta(Harness::Codex, "by-id", Some(BASE + 3));
    unresolved.lineage.parent = Some(SessionLink::Unresolved("twin".into()));
    // Two sources share the id "twin" with equal activity: the edge stays ambiguous.
    let twin1 = metadata(file_key(Harness::Codex, "twin", &PathBuf::from("t1")), "t1");
    let twin2 = metadata(file_key(Harness::Codex, "twin", &PathBuf::from("t2")), "t2");
    // A continuation head absorbs its earlier member.
    let early = meta(Harness::Codex, "early", Some(BASE + 5));
    let mut head = meta(Harness::Codex, "head", Some(BASE + 6));
    head.members = vec![early.key.clone(), head.key.clone()];
    let healthy = meta(Harness::OpenCode, "healthy", Some(BASE + 7));

    index.apply_discovery(delta(Harness::Claude, vec![a, b, missing]));
    index.apply_discovery(delta(
        Harness::Codex,
        vec![unresolved, twin1, twin2, early, head],
    ));
    index.apply_discovery(delta(Harness::OpenCode, vec![healthy]));
    index.freeze(1);
    let roots: HashSet<String> = ids(&index.snapshot()).into_iter().collect();
    for id in ["a", "b", "missing-parent", "by-id", "head", "healthy"] {
        assert!(
            roots.contains(id),
            "{id} must stay a top-level group: {roots:?}"
        );
    }
    assert!(
        !roots.contains("early"),
        "a continuation member folds into its head"
    );
}

#[test]
fn eviction_of_detailed_summaries_never_removes_rows_or_ends_pagination() {
    let store = store();
    let mut index = RosterIndex::new(limits(), 3);
    fill(&mut index, &store);
    index.freeze(1);
    let roots = index.snapshot().roots.clone();
    for key in roots.iter().take(10) {
        let meta = index.metadata(key).expect("indexed");
        index.update_summary(support::facts::summary((*meta).clone()));
    }
    assert_eq!(index.cached_summaries(), 3, "the cache is bounded");
    let snap = index.snapshot();
    assert_eq!(snap.roots.len(), 50, "rows survive eviction");
    assert_eq!(snap.metadata.len(), 50);
    assert!(snap.summaries.len() <= 3);
    assert!(snap.has_more);
    assert!(index.load_more(1), "pagination continues after eviction");
    // An evicted session rehydrates from its metadata.
    let evicted = roots
        .iter()
        .take(10)
        .find(|k| !index.has_summary(k))
        .expect("something was evicted")
        .clone();
    let meta = index.metadata(&evicted).unwrap();
    index.update_summary(support::facts::summary((*meta).clone()));
    assert!(index.has_summary(&evicted));
    assert_eq!(index.cached_summaries(), 3);
}

#[test]
fn a_metadata_ceiling_reports_limited_never_the_end_of_the_list() {
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 10,
            max_bytes: 64 * 1024 * 1024,
        },
        2_000,
    );
    let sessions: Vec<SessionMetadata> = (0..25)
        .map(|i| meta(Harness::Codex, &format!("s{i:02}"), Some(BASE + i)))
        .collect();
    index.apply_discovery(delta(Harness::Codex, sessions));
    index.freeze(1);
    let snap = index.snapshot();
    assert_eq!(
        snap.roots.len(),
        10,
        "everything already discovered stays usable"
    );
    let limit = snap.limited.expect("a ceiling was hit");
    assert_eq!(limit.sources, 10);
    assert!(
        !snap.has_more,
        "no further known roots, but the list is not complete"
    );

    // The byte ceiling stops discovery the same way.
    let mut bytes = RosterIndex::new(
        MetadataLimits {
            max_sources: 100_000,
            max_bytes: 4_000,
        },
        2_000,
    );
    let sessions: Vec<SessionMetadata> = (0..25)
        .map(|i| meta(Harness::Codex, &format!("s{i:02}"), Some(BASE + i)))
        .collect();
    bytes.apply_discovery(delta(Harness::Codex, sessions));
    let snap = bytes.snapshot();
    assert!(bytes.len() < 25);
    assert_eq!(snap.limited.map(|l| l.bytes), Some(4_000));
}

// ---- real native stores through the worker -----------------------------------

struct Native {
    _dir: tempfile::TempDir,
    config: Config,
    /// (key, activity) of every root.
    roots: Vec<(SessionKey, i64)>,
    parent: SessionKey,
    children: usize,
}

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

/// 125 roots across the three native harnesses and 80 children under one OpenCode parent.
fn native_store() -> Native {
    let dir = tempfile::tempdir().unwrap();
    let codex = dir.path().join("codex");
    let claude = dir.path().join("claude");
    let db = dir.path().join("opencode.db");
    let conn = oc::create_v2(&db);
    let mut roots = Vec::new();
    let mut parent = None;
    for i in 0..125usize {
        let at = BASE + (i as i64) * 60_000;
        match i % 3 {
            0 => {
                let id = format!("codex-{i:03}");
                let path = support::dated(&codex, &format!("rollout-{id}.jsonl"));
                support::write_lines(
                    &path,
                    &[
                        support::codex::session_meta(&iso(at - 1_000), &id, "/work/demo"),
                        support::codex::user(&iso(at), &format!("task {id}")),
                    ],
                );
                roots.push((file_key(Harness::Codex, &id, &path), at));
            }
            1 => {
                let id = format!("claude-{i:03}");
                let path = claude.join("proj").join(format!("{id}.jsonl"));
                let record = json!({
                    "type": "user", "uuid": format!("u{i}"), "sessionId": id,
                    "timestamp": iso(at), "cwd": "/work/demo",
                    "message": {"role": "user", "content": format!("task {id}")}
                });
                support::write_lines(&path, &[record.to_string()]);
                roots.push((file_key(Harness::Claude, &id, &path), at));
            }
            _ => {
                let id = format!("ses_{i:03}");
                oc::insert_session(&conn, &oc::Session::new(&id, at - 1_000, at));
                let key = SessionKey {
                    harness: Harness::OpenCode,
                    native_id: id.clone(),
                    source: SourceKey::DbRow {
                        database: db.clone(),
                        table: "session_v2".into(),
                        id: id.clone(),
                    },
                };
                if i == 2 {
                    parent = Some(key.clone());
                }
                roots.push((key, at));
            }
        }
    }
    let parent = parent.expect("a parent");
    for n in 0..80i64 {
        let id = format!("ses_child_{n:03}");
        let mut child = oc::Session::new(&id, BASE, BASE + n);
        child.parent_id = Some(&parent.native_id);
        oc::insert_session(&conn, &child);
    }
    drop(conn);
    let config = Config::load_with_env(
        &CliOverrides {
            codex_sessions: Some(codex),
            claude_projects: Some(claude),
            opencode_db: Some(db),
            ..CliOverrides::default()
        },
        &Env::new(),
    )
    .unwrap();
    Native {
        _dir: dir,
        config,
        roots,
        parent,
        children: 80,
    }
}

fn wait(
    worker: &WorkerHandle,
    what: &str,
    pred: impl Fn(&WorkerEvent) -> bool,
) -> Vec<WorkerEvent> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(50)) {
            let hit = pred(&event);
            seen.push(event);
            if hit {
                return seen;
            }
        }
    }
    panic!("never saw {what}");
}

fn last_snapshot(events: &[WorkerEvent]) -> &RosterSnapshot {
    events
        .iter()
        .rev()
        .find_map(|e| match e {
            WorkerEvent::Roster { snapshot, .. } => Some(snapshot),
            _ => None,
        })
        .expect("a roster event")
}

#[test]
fn real_native_stores_page_fifty_at_a_time_through_the_worker() {
    let store = native_store();
    let mut config = store.config.clone();
    config.refresh_ms = 3_600_000;
    config.process_enabled = false;
    // 125 roots + 80 children are all discovered and none is dropped by the 2,000 cache.
    let readers: Vec<Box<dyn agentscope::providers::ProviderReader>> = vec![
        Box::new(CodexScanner::new(&config).unwrap()),
        Box::new(ClaudeScanner::new(&config).unwrap()),
        Box::new(OpenCodeScanner::new(&config).unwrap()),
    ];
    let worker = WorkerHandle::spawn(readers, config);

    // The parent's family contains the newest group? No: children are old. The expected
    // page is the 50 newest roots by recorded activity.
    let mut expected: Vec<(SessionKey, i64)> = store.roots.clone();
    expected.sort_by_key(|e| std::cmp::Reverse(e.1));
    let expected_keys: Vec<SessionKey> = expected.iter().map(|(k, _)| k.clone()).collect();

    let events = wait(
        &worker,
        "the frozen first generation",
        |e| matches!(e, WorkerEvent::Roster { snapshot, .. } if !snapshot.provisional),
    );
    let first = last_snapshot(&events).clone();
    assert_eq!(first.generation, 1);
    assert_eq!(first.roots.as_ref(), &expected_keys[..50]);
    assert!(first.has_more);
    assert!(
        first.summaries.len() < 50,
        "the first page appears from metadata before hydration has finished"
    );

    // The initial 50 are all hydrated, none dropped by the 16-entry load queue.
    let events = wait(
        &worker,
        "all 50 hydrated",
        |e| matches!(e, WorkerEvent::Roster { snapshot, .. } if snapshot.summaries.len() == 50),
    );
    let hydrated = last_snapshot(&events);
    assert_eq!(hydrated.roots.as_ref(), first.roots.as_ref());

    // A stale generation changes nothing; the current one reveals exactly 50 more.
    worker
        .tx
        .send(WorkerRequest::LoadRosterMore { generation: 9 })
        .unwrap();
    worker
        .tx
        .send(WorkerRequest::LoadRosterMore { generation: 1 })
        .unwrap();
    let events = wait(
        &worker,
        "100 roots",
        |e| matches!(e, WorkerEvent::Roster { snapshot, .. } if snapshot.roots.len() >= 100),
    );
    let more = last_snapshot(&events);
    assert_eq!(more.roots.len(), 100, "the stale request revealed nothing");
    assert_eq!(more.roots.as_ref(), &expected_keys[..100]);
    worker
        .tx
        .send(WorkerRequest::LoadRosterMore { generation: 1 })
        .unwrap();
    let events = wait(
        &worker,
        "125 roots",
        |e| matches!(e, WorkerEvent::Roster { snapshot, .. } if snapshot.roots.len() == 125),
    );
    let all = last_snapshot(&events);
    assert_eq!(all.roots.as_ref(), expected_keys.as_slice());
    assert!(!all.has_more);

    // Children load in batches of 50 and never become roots.
    worker
        .tx
        .send(WorkerRequest::LoadChildren {
            request_id: 1,
            parent: store.parent.clone(),
            cursor: None,
        })
        .unwrap();
    let events = wait(&worker, "first children", |e| {
        matches!(e, WorkerEvent::Children { request_id: 1, .. })
    });
    let Some(WorkerEvent::Children { metadata, next, .. }) = events.last() else {
        panic!("children expected");
    };
    assert_eq!(metadata.len(), 50);
    let cursor = next.clone().expect("30 more children");
    worker
        .tx
        .send(WorkerRequest::LoadChildren {
            request_id: 2,
            parent: store.parent.clone(),
            cursor: Some(cursor),
        })
        .unwrap();
    let events = wait(&worker, "second children", |e| {
        matches!(e, WorkerEvent::Children { request_id: 2, .. })
    });
    let Some(WorkerEvent::Children {
        metadata: rest,
        next,
        ..
    }) = events.last()
    else {
        panic!("children expected");
    };
    assert_eq!(rest.len(), store.children - 50);
    assert!(next.is_none());
    assert!(worker.shutdown());
    let _: Option<Arc<()>> = None;
}

#[test]
fn root_load_more_comes_after_all_groups() {
    let mut app = App::new(
        BASE,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut a = meta(Harness::Codex, "a", Some(BASE + 3_000));
    a.project = Some("X".into());
    let mut b = meta(Harness::Claude, "b", Some(BASE + 2_000));
    b.project = Some("Y".into());
    let mut c = meta(Harness::OpenCode, "c", Some(BASE + 1_000));
    c.project = Some("X".into());
    let sessions = [a, b, c]
        .into_iter()
        .map(SessionSummary::unhydrated)
        .collect();
    let scan = ScanProgress {
        harness: Harness::Codex,
        scanned: 3,
        total: Some(3),
        done: true,
    };
    let mut snap = support::snapshot(1, sessions, scan);
    snap.has_more = true;
    assert!(app.apply_roster_snapshot(snap, vec![]));

    let shape: Vec<String> = app
        .roster_rows()
        .map(|item| match item {
            RosterItem::GroupHeader(header) => {
                format!("header:{}", header.project.unwrap_or("none"))
            }
            RosterItem::Session(row) => row.session.meta.key.native_id.clone(),
            RosterItem::RootLoadMore => "roots-more".into(),
            RosterItem::ChildLoadMore { .. } => "child-more".into(),
        })
        .collect();
    assert_eq!(
        shape,
        ["header:X", "a", "c", "header:Y", "b", "roots-more"],
        "the one root Load more row stays below every group"
    );
}

#[test]
fn the_metadata_ceiling_keeps_the_newest_sources() {
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 10,
            max_bytes: 64 * 1024 * 1024,
        },
        2_000,
    );
    // Discovery order is the reverse of recency: the oldest arrive first.
    let sessions: Vec<SessionMetadata> = (0..25)
        .map(|i| meta(Harness::Claude, &format!("s{i:02}"), Some(BASE + i)))
        .collect();
    index.apply_discovery(delta(Harness::Claude, sessions));
    index.freeze(1);
    let snap = index.snapshot();
    assert!(snap.limited.is_some());
    let expected: Vec<String> = (15..25).rev().map(|i| format!("s{i:02}")).collect();
    assert_eq!(ids(&snap), expected, "the 10 newest sources survive");
}
