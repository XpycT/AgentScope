use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

mod support;

use agentscope::app::{
    ACTIVITY_DEBOUNCE_MS, ActivityState, App, FilterState, FilterTarget, GroupTotals, PreviewState,
    RosterItem, SortMode, UsageAvailability,
};
use agentscope::domain::*;
use agentscope::providers::WorkProgress;
use agentscope::roster::RosterSnapshot;
use agentscope::ui::{RosterContext, apply_worker_event};
use agentscope::worker::{WorkerEvent, WorkerRequest};
use support::facts::{self, file_key, metadata};

const NOW: Timestamp = 10_000_000;

fn thresholds() -> Thresholds {
    Thresholds {
        quiet: Duration::from_secs(300),
        stall: Duration::from_secs(900),
    }
}

fn app() -> App {
    App::new(NOW, thresholds())
}

fn key(harness: Harness, id: &str) -> SessionKey {
    file_key(harness, id, Path::new(&format!("{id}.jsonl")))
}

fn dup_key(harness: Harness, id: &str, file: &str) -> SessionKey {
    file_key(harness, id, Path::new(file))
}

fn usage(total: Option<u64>, usd: Option<f64>) -> UsageSummary {
    UsageSummary {
        categories: TokenCategories::default(),
        total,
        class: AggregationClass::Reported,
        cost: CostEstimate {
            usd,
            class: AggregationClass::Estimated,
            as_of: None,
            note: None,
            ..CostEstimate::default()
        },
        ..UsageSummary::default()
    }
}

/// Active-looking session (recent persisted activity, matched process).
fn session(harness: Harness, id: &str, last: Timestamp) -> SessionSummary {
    let mut meta = metadata(key(harness, id), &format!("title {id}"));
    meta.activity = ActivityStamp {
        at: Some(last),
        class: AggregationClass::Reported,
    };
    SessionSummary {
        last_event_at: Some(last),
        lifecycle: Lifecycle::Ongoing,
        process: ProcessAssociation::Unmatched,
        ..facts::summary(meta)
    }
}

fn ids(app: &App) -> Vec<String> {
    app.visible_rows()
        .iter()
        .map(|r| r.session.meta.key.native_id.clone())
        .collect()
}

fn selected_id(app: &App) -> Option<String> {
    app.selected_key().map(|k| k.native_id)
}

fn with_project(mut summary: SessionSummary, project: Option<&str>) -> SessionSummary {
    summary.meta.project = project.map(str::to_owned);
    summary
}

/// Project groups as `(project, totals)` in the order the roster draws them.
fn group_headers(app: &App) -> Vec<(Option<String>, GroupTotals)> {
    app.roster_rows()
        .filter_map(|item| match item {
            RosterItem::GroupHeader(header) => {
                Some((header.project.map(str::to_owned), header.totals))
            }
            _ => None,
        })
        .collect()
}

/// Roster session rows in visual order, skipping headings and Load more rows.
fn roster_ids(app: &App) -> Vec<String> {
    app.roster_rows()
        .filter_map(|item| match item {
            RosterItem::Session(row) => Some(row.session.meta.key.native_id.clone()),
            _ => None,
        })
        .collect()
}

fn warning(source: &str, message: &str, count: u32) -> Warning {
    Warning {
        key: WarningKey {
            harness: Harness::Codex,
            source: source.into(),
            kind: WarningKind::Parse,
        },
        message: message.into(),
        count,
    }
}

#[test]
fn merges_roster_deltas_and_preserves_selection() {
    let mut app = app();
    app.apply_roster(
        vec![
            session(Harness::Codex, "a", NOW - 1000),
            session(Harness::Claude, "b", NOW - 2000),
            session(Harness::OpenCode, "c", NOW - 3000),
        ],
        vec![],
    );
    // First row selected by default.
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
    app.move_selection(1);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));

    // A new, newer session arrives and `a` is updated; selection stays on `b`.
    let mut a2 = session(Harness::Codex, "a", NOW - 10);
    a2.meta.title = "renamed".into();
    app.apply_roster(
        vec![
            session(Harness::Codex, "z", NOW - 5),
            a2,
            session(Harness::Claude, "b", NOW - 2000),
            session(Harness::OpenCode, "c", NOW - 3000),
        ],
        vec![],
    );
    assert_eq!(ids(&app), ["z", "a", "b", "c"]);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));
    assert_eq!(app.visible_rows()[1].session.meta.title, "renamed");

    // Sessions absent from the new snapshot are removed.
    app.apply_roster(vec![session(Harness::Claude, "b", NOW - 2000)], vec![]);
    assert_eq!(ids(&app), ["b"]);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));
    assert_eq!(app.total_sessions(), 1);

    // Empty roster clears selection.
    app.apply_roster(vec![], vec![]);
    assert!(app.selected_key().is_none());
    assert!(app.visible_rows().is_empty());
}

#[test]
fn counts_providers_and_synthesized_states() {
    let mut app = app();
    let mut done = session(Harness::Claude, "done", NOW - 1000);
    done.lifecycle = Lifecycle::Failed;
    let mut gone = session(Harness::Claude, "gone", NOW - 1000);
    gone.meta.source_available = false;
    app.apply_roster(
        vec![
            session(Harness::Codex, "a", NOW - 1000),
            session(Harness::Codex, "b", NOW - 1000),
            done,
            gone,
        ],
        vec![],
    );
    let providers = app.provider_counts();
    assert_eq!(
        providers,
        vec![
            (Harness::Codex, 2),
            (Harness::Claude, 2),
            (Harness::OpenCode, 0),
            (Harness::Omp, 0),
            (Harness::Hermes, 0),
            (Harness::Pi, 0),
        ]
    );
    let states = app.status_counts();
    let count = |l: StatusLabel| states.iter().find(|(x, _)| *x == l).map(|(_, n)| *n);
    assert_eq!(count(StatusLabel::Failed), Some(1));
    assert_eq!(count(StatusLabel::Unavailable), Some(1));
    assert_eq!(states.iter().map(|(_, n)| n).sum::<usize>(), 4);
    // Counts cover discovered sessions, not the filtered view.
    app.set_filter(FilterState {
        providers: Some(vec![Harness::Codex]),
        ..FilterState::default()
    });
    assert_eq!(app.visible_rows().len(), 2);
    assert_eq!(app.provider_counts()[1], (Harness::Claude, 2));
}

#[test]
fn filters_and_sorts_with_unknown_tokens_last() {
    let mut app = app();
    let mut a = session(Harness::Codex, "a", NOW - 3000);
    a.usage = Some(usage(Some(100), None));
    a.meta.project = Some("Zeta".into());
    let mut b = session(Harness::Claude, "b", NOW - 2000);
    b.usage = Some(usage(None, None));
    b.meta.project = Some("alpha".into());
    let mut c = session(Harness::OpenCode, "c", NOW - 1000);
    c.usage = Some(usage(Some(900), None));
    let d = session(Harness::Codex, "d", NOW - 4000); // usage None
    app.apply_roster(vec![a, b, c, d], vec![]);

    // Default: last activity, newest first.
    assert_eq!(ids(&app), ["c", "b", "a", "d"]);

    app.sort(SortMode::Tokens);
    assert_eq!(ids(&app), ["c", "a", "b", "d"], "unknown tokens last");
    app.sort(SortMode::Tokens); // reverse: ascending
    assert_eq!(ids(&app), ["a", "c", "b", "d"], "unknown still last");

    app.sort(SortMode::Provider);
    assert_eq!(ids(&app), ["a", "d", "b", "c"]);

    app.sort(SortMode::LastActivity);
    assert_eq!(ids(&app), ["c", "b", "a", "d"]);
    app.sort(SortMode::LastActivity);
    assert_eq!(ids(&app), ["d", "a", "b", "c"]);

    // Provider + status + project filters.
    app.set_filter(FilterState {
        providers: Some(vec![Harness::Codex, Harness::Claude]),
        ..FilterState::default()
    });
    assert_eq!(app.visible_rows().len(), 3);
    app.set_filter(FilterState {
        project: Some("ALPHA".into()),
        ..FilterState::default()
    });
    assert_eq!(ids(&app), ["b"]);
    app.set_filter(FilterState {
        statuses: Some(vec![StatusLabel::Complete]),
        ..FilterState::default()
    });
    assert!(app.visible_rows().is_empty());
    app.set_filter(FilterState {
        statuses: Some(vec![app_status_of(&app, "a")]),
        ..FilterState::default()
    });
    assert_eq!(app.visible_rows().len(), 4, "all share one status here");
}

fn app_status_of(app: &App, id: &str) -> StatusLabel {
    app.all_rows()
        .into_iter()
        .find(|r| r.session.meta.key.native_id == id)
        .unwrap()
        .status
        .label
}

#[test]
fn search_matches_title_project_cwd_and_session_id() {
    let mut app = app();
    let mut a = session(Harness::Codex, "abc-123", NOW - 1);
    a.meta.title = "Inventory API".into();
    let mut b = session(Harness::Claude, "b", NOW - 2);
    b.meta.project = Some("RipperGame".into());
    let mut c = session(Harness::Claude, "c", NOW - 3);
    c.meta.cwd = Some("/work/Sandbox".into());
    app.apply_roster(vec![a, b, c], vec![]);

    let q = |app: &mut App, q: &str| {
        app.set_query(q);
        ids(app)
    };
    assert_eq!(q(&mut app, "inventory"), ["abc-123"]);
    assert_eq!(q(&mut app, "RIPPERgame"), ["b"]);
    assert_eq!(q(&mut app, "sandbox"), ["c"]);
    assert_eq!(q(&mut app, "ABC-1"), ["abc-123"]);
    assert_eq!(q(&mut app, "nomatch"), Vec::<String>::new());
    assert_eq!(q(&mut app, "  "), ["abc-123", "b", "c"]);
}

#[test]
fn removing_the_selected_session_selects_the_nearest_row() {
    let mut app = app();
    let mk = |id: &str, age: i64| session(Harness::Codex, id, NOW - age);
    app.apply_roster(vec![mk("a", 1), mk("b", 2), mk("c", 3), mk("d", 4)], vec![]);
    app.move_selection(2);
    assert_eq!(selected_id(&app).as_deref(), Some("c"));
    app.apply_roster(vec![mk("a", 1), mk("b", 2), mk("d", 4)], vec![]);
    assert_eq!(
        selected_id(&app).as_deref(),
        Some("d"),
        "takes the same slot"
    );

    // Removing the last row moves to the new last row.
    app.apply_roster(vec![mk("a", 1), mk("b", 2)], vec![]);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));

    // Filtering out the selection selects the nearest matching row;
    // a filter that still matches preserves it.
    app.set_query("title a");
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
    app.set_query("");
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
    app.move_selection(5);
    assert_eq!(selected_id(&app).as_deref(), Some("b"), "clamped");
    app.move_selection(-9);
    assert_eq!(selected_id(&app).as_deref(), Some("a"), "clamped");
}

#[test]
fn warning_keys_replace_and_count_instead_of_accumulating() {
    let mut app = app();
    app.apply_roster(vec![], vec![warning("x", "first", 1)]);
    app.apply_roster(vec![], vec![warning("x", "second", 3)]);
    assert_eq!(app.warnings().len(), 1);
    assert_eq!(app.warnings()[0].message, "second");
    assert_eq!(app.warnings()[0].count, 3);
    assert_eq!(app.warning_total(), 3);

    // A single-warning event replaces by key too; a new key adds a banner.
    app.apply_warning(warning("x", "third", 4));
    app.apply_warning(warning("y", "other", 2));
    assert_eq!(app.warnings().len(), 2);
    assert_eq!(app.warning_total(), 6);
    assert_eq!(
        app.warnings()
            .iter()
            .find(|w| w.key.source == "x")
            .unwrap()
            .message,
        "third"
    );

    // Keys absent from the next roster are retired (recovery clears them).
    app.apply_roster(vec![], vec![warning("y", "other", 2)]);
    assert_eq!(app.warnings().len(), 1);
    assert_eq!(app.warnings()[0].key.source, "y");
    app.apply_roster(vec![], vec![]);
    assert!(app.warnings().is_empty());
    assert_eq!(app.warning_total(), 0);
}

#[test]
fn filters_usage_availability_and_composes_with_search() {
    let mut app = app();
    let mut known = session(Harness::Codex, "known", NOW - 1);
    known.usage = Some(usage(Some(10), None));
    let mut cost = session(Harness::Codex, "cost", NOW - 2);
    cost.usage = Some(usage(Some(20), Some(0.5)));
    let mut unk = session(Harness::Claude, "unk", NOW - 3);
    unk.usage = Some(usage(None, None));
    let none = session(Harness::Claude, "none", NOW - 4);
    app.apply_roster(vec![known, cost, unk, none], vec![]);

    let with = |app: &mut App, usage| {
        app.set_filter(FilterState {
            usage,
            ..FilterState::default()
        });
        ids(app)
    };
    assert_eq!(with(&mut app, UsageAvailability::Any).len(), 4);
    assert_eq!(with(&mut app, UsageAvailability::Known), ["known", "cost"]);
    assert_eq!(with(&mut app, UsageAvailability::Unknown), ["unk", "none"]);
    assert_eq!(with(&mut app, UsageAvailability::HasCost), ["cost"]);

    // Composes with search and provider filters.
    app.set_filter(FilterState {
        usage: UsageAvailability::Known,
        query: "KNOWN".into(),
        ..FilterState::default()
    });
    assert_eq!(ids(&app), ["known"]);
    app.set_filter(FilterState {
        usage: UsageAvailability::Unknown,
        providers: Some(vec![Harness::Claude]),
        query: "none".into(),
        ..FilterState::default()
    });
    assert_eq!(ids(&app), ["none"]);
    app.set_filter(FilterState {
        usage: UsageAvailability::HasCost,
        query: "known".into(),
        ..FilterState::default()
    });
    assert!(ids(&app).is_empty());
}

#[test]
fn duplicate_parent_resolution_is_ambiguous_on_equal_recency_and_resolves_newest_otherwise() {
    let parent_target = key(Harness::Codex, "parent");
    let mut child = session(Harness::Codex, "child", NOW - 1);
    child.meta.lineage.parent = Some(SessionLink::Resolved(parent_target.clone()));

    let mut old = session(Harness::Codex, "parent", NOW - 5000);
    old.meta.key = dup_key(Harness::Codex, "parent", "old.jsonl");
    let mut new = session(Harness::Codex, "parent", NOW - 100);
    new.meta.key = dup_key(Harness::Codex, "parent", "new.jsonl");

    let mut app = app();
    app.apply_roster(vec![child.clone(), old.clone(), new.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.meta.key),
        Some(SessionLink::Resolved(new.meta.key.clone()))
    );

    // Equal recency across duplicates: never guess.
    let mut tie = old.clone();
    tie.last_event_at = new.last_event_at;
    tie.meta.activity.at = new.meta.activity.at;
    app.apply_roster(vec![child.clone(), tie, new.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.meta.key),
        Some(SessionLink::Ambiguous)
    );

    // A single match resolves to itself; a missing target keeps the link.
    app.apply_roster(vec![child.clone(), new.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.meta.key),
        Some(SessionLink::Resolved(new.meta.key.clone()))
    );
    app.apply_roster(vec![child.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.meta.key),
        Some(SessionLink::Resolved(parent_target))
    );

    // Unresolved ids resolve against same-provider sessions; Ambiguous is preserved.
    let mut by_id = session(Harness::Codex, "c2", NOW - 2);
    by_id.meta.lineage.parent = Some(SessionLink::Unresolved("parent".into()));
    by_id.meta.lineage.fork = Some(SessionLink::Ambiguous);
    app.apply_roster(vec![by_id.clone(), new.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&by_id.meta.key),
        Some(SessionLink::Resolved(new.meta.key.clone()))
    );
    assert_eq!(
        app.resolved_fork(&by_id.meta.key),
        Some(SessionLink::Ambiguous)
    );
    assert_eq!(app.resolved_parent(&new.meta.key), None);
}

#[test]
fn stale_generations_are_ignored_and_clock_updates_statuses() {
    let mut app = app();
    let scan = ScanProgress {
        harness: Harness::Codex,
        scanned: 1,
        total: Some(1),
        done: true,
    };
    assert!(app.apply_roster_snapshot(
        support::snapshot(2, vec![session(Harness::Codex, "a", NOW - 1000)], scan),
        vec![]
    ));
    assert!(!app.apply_roster_snapshot(support::snapshot(1, vec![], scan), vec![]));
    assert_eq!(app.total_sessions(), 1);
    assert_eq!(app.generation(), 2);
    let before = app_status_of(&app, "a");
    app.set_now(NOW + 3_600_000);
    assert_ne!(app_status_of(&app, "a"), before);
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
}

#[test]
fn reset_generation_accepts_a_restarted_worker() {
    let scan = ScanProgress {
        harness: Harness::Codex,
        scanned: 0,
        total: None,
        done: true,
    };
    let mut app = app();
    for g in 1..=4 {
        assert!(app.apply_roster_snapshot(support::snapshot(g, vec![], scan), vec![]));
    }
    assert!(!app.apply_roster_snapshot(support::snapshot(1, vec![], scan), vec![]));
    app.reset_generation();
    assert_eq!(app.generation(), 0);
    assert!(app.scan().is_none());
    assert!(app.apply_roster_snapshot(support::snapshot(1, vec![], scan), vec![]));
}

fn db_key(database: &str, id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::OpenCode,
        native_id: id.into(),
        source: SourceKey::DbRow {
            database: database.into(),
            table: "session_v2".into(),
            id: id.into(),
        },
    }
}

fn db_session(database: &str, id: &str, title: &str, last: Timestamp) -> SessionSummary {
    let mut meta = metadata(db_key(database, id), title);
    meta.activity = ActivityStamp {
        at: Some(last),
        class: AggregationClass::Reported,
    };
    facts::summary(meta)
}

#[test]
fn profile_database_identity() {
    let a = db_session("a/state.db", "ses_same", "profile a", NOW - 100);
    let b = db_session("b/state.db", "ses_same", "profile b", NOW - 200);
    assert_ne!(a.meta.key, b.meta.key);

    let mut app = app();
    app.apply_roster(vec![a.clone(), b.clone()], vec![]);
    assert_eq!(app.total_sessions(), 2, "equal native IDs must not merge");

    app.move_selection(1);
    assert_eq!(app.selected_key().as_ref(), Some(&b.meta.key));

    // A roster update that touches the other profile keeps the selection on `b`.
    let mut a2 = a.clone();
    a2.meta.title = "profile a renamed".into();
    app.apply_roster(vec![a2, b.clone()], vec![]);
    let selected_after_refresh = app.selected_key().expect("selection survives");
    assert_eq!(selected_after_refresh, b.meta.key);

    // Opening the inspector asks the worker for exactly the selected database's session.
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { key, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("inspector open must request the selected session: {requests:?}");
    };
    assert_eq!(*key, b.meta.key);
    assert_eq!(app.transcript().map(|t| t.key()), Some(&b.meta.key));
    assert_eq!(
        app.session(&a.meta.key)
            .map(|r| r.session.meta.title.clone()),
        Some("profile a renamed".to_owned())
    );
}

#[test]
fn database_sessions_only_resolve_lineage_within_their_database() {
    let mut child = db_session("b/state.db", "child", "child", NOW - 10);
    child.meta.lineage.parent = Some(SessionLink::Unresolved("shared".into()));
    let other_db_parent = db_session("a/state.db", "shared", "elsewhere", NOW - 20);
    let mut app = app();
    app.apply_roster(vec![child.clone(), other_db_parent.clone()], vec![]);
    assert_eq!(
        app.resolved_parent(&child.meta.key),
        Some(SessionLink::Unresolved("shared".into())),
        "a same-ID session in another database is not the parent"
    );

    let own_parent = db_session("b/state.db", "shared", "mine", NOW - 30);
    app.apply_roster(
        vec![child.clone(), other_db_parent, own_parent.clone()],
        vec![],
    );
    assert_eq!(
        app.resolved_parent(&child.meta.key),
        Some(SessionLink::Resolved(own_parent.meta.key))
    );
}

#[test]
fn tool_child_links_resolve_like_lineage_and_never_guess() {
    let db = Path::new("opencode.db");
    let db_key = |id: &str| SessionKey {
        harness: Harness::OpenCode,
        native_id: id.to_owned(),
        source: SourceKey::DbRow {
            database: db.to_path_buf(),
            table: "session_v2".into(),
            id: id.to_owned(),
        },
    };
    let mut child_meta = metadata(db_key("child"), "child");
    child_meta.activity = ActivityStamp {
        at: Some(NOW - 1),
        class: AggregationClass::Reported,
    };
    let child = facts::summary(child_meta);
    let mut old_meta = metadata(db_key("target"), "old");
    old_meta.activity = ActivityStamp {
        at: Some(NOW - 5_000),
        class: AggregationClass::Reported,
    };
    let old = facts::summary(old_meta);
    let mut new_meta = metadata(db_key("target"), "new");
    new_meta.activity = ActivityStamp {
        at: Some(NOW - 100),
        class: AggregationClass::Reported,
    };
    let new = facts::summary(new_meta);

    let tool = |link: Option<SessionLink>| ToolInteraction {
        name: "subagent".into(),
        call_id: None,
        input: None,
        output: None,
        status: ToolStatus::Completed,
        exit_code: None,
        started_at: None,
        completed_at: None,
        orphan: false,
        child_session: link,
    };

    let mut app = app();
    app.apply_roster(vec![child.clone(), old.clone(), new.clone()], vec![]);
    let linked = tool(Some(SessionLink::Unresolved("target".into())));
    assert_eq!(
        app.resolved_tool_child(&child.meta.key, &linked),
        Some(SessionLink::Resolved(new.meta.key.clone())),
        "the newest confirmed same-database candidate wins"
    );

    // Equal recency across duplicate candidates is ambiguous, never a jump.
    let mut tie = old.clone();
    tie.meta.activity.at = new.meta.activity.at;
    app.apply_roster(vec![child.clone(), tie, new.clone()], vec![]);
    assert_eq!(
        app.resolved_tool_child(&child.meta.key, &linked),
        Some(SessionLink::Ambiguous)
    );

    // A missing target keeps the recorded id; a tool with no link has no jump.
    app.apply_roster(vec![child.clone()], vec![]);
    assert_eq!(
        app.resolved_tool_child(&child.meta.key, &linked),
        Some(SessionLink::Unresolved("target".into()))
    );
    assert_eq!(app.resolved_tool_child(&child.meta.key, &tool(None)), None);
}

// ---- B5: preview, details, loaded-only navigation and newest entry ----------

fn done_scan() -> ScanProgress {
    ScanProgress {
        harness: Harness::Codex,
        scanned: 1,
        total: Some(1),
        done: true,
    }
}

fn event_for(key: &SessionKey, i: u64) -> Event {
    Event {
        id: format!("e{i}"),
        session: key.clone(),
        timestamp: Some(NOW + i as i64),
        source_order: i,
        kind: EventKind::AssistantMessage,
        actor: Actor::Assistant,
        text: Some(BoundedText {
            preview: format!("body {i}"),
            byte_len: 6,
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
                    offset: i,
                },
                generation: None,
            },
            record_type: "test".into(),
            harness_version: None,
        },
    }
}

fn anchor_of(event: &Event) -> PageAnchor {
    PageAnchor {
        timestamp: event.timestamp,
        source_order: event.source_order,
        stable_id: event.id.clone(),
    }
}

fn forward_page(key: &SessionKey, range: std::ops::Range<u64>, has_more: bool) -> TranscriptPage {
    let events: Vec<Event> = range.map(|i| event_for(key, i)).collect();
    let next = events.last().map(anchor_of);
    TranscriptPage {
        events,
        diagnostics: vec![],
        next,
        has_more,
        truncated: false,
    }
}

fn backward_page(key: &SessionKey, range: std::ops::Range<u64>, has_more: bool) -> TranscriptPage {
    let events: Vec<Event> = range.map(|i| event_for(key, i)).collect();
    let next = events.first().map(anchor_of);
    TranscriptPage {
        events,
        diagnostics: vec![],
        next,
        has_more,
        truncated: false,
    }
}

#[test]
fn preview_only_answers_the_current_selection_and_retries_explicitly() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let b = session(Harness::Claude, "b", NOW - 2_000);
    let (ka, kb) = (a.meta.key.clone(), b.meta.key.clone());
    app.apply_roster(vec![a.clone(), b.clone()], vec![]);
    app.move_selection(1);
    assert_eq!(app.selected_key(), Some(kb.clone()));

    // Keyboard movement drove the first preview request for B, and only once.
    // A's preview came with the first selection and is released for B's.
    let requests = app.take_requests();
    let [
        WorkerRequest::HydratePreview { .. },
        WorkerRequest::CancelLoad { .. },
        WorkerRequest::HydratePreview {
            request_id: b_id, ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected A's then B's preview, got {requests:?}");
    };
    assert!(!app.select_session(&kb), "a render must not re-request");

    // Moving to A cancels only the abandoned preview and asks for A.
    app.move_selection(-1);
    assert_eq!(app.selected_key(), Some(ka.clone()));
    let requests = app.take_requests();
    let [
        WorkerRequest::CancelLoad {
            request_id: cancelled,
        },
        WorkerRequest::HydratePreview {
            request_id: a_id, ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected cancel then preview, got {requests:?}");
    };
    assert_eq!(cancelled, b_id);

    // A late response from B cannot replace the current preview or facts.
    assert!(!app.apply_preview(*b_id, kb.clone(), Arc::new(b.clone()), true));
    assert_eq!(app.preview().key(), Some(&ka));
    assert!(!app.apply_load_progress(
        *b_id,
        WorkProgress {
            records: 3,
            expanded_bytes: 12
        }
    ));

    // Cached facts are visible immediately and marked incomplete.
    assert!(app.apply_preview(*a_id, ka.clone(), Arc::new(a.clone()), false));
    assert!(matches!(
        app.preview(),
        PreviewState::Ready {
            complete: false,
            ..
        }
    ));
    assert!(app.apply_preview(*a_id, ka.clone(), Arc::new(a.clone()), true));
    assert!(matches!(
        app.preview(),
        PreviewState::Ready { complete: true, .. }
    ));

    // A failure is terminal; selecting the same row again does not retry it.
    app.load_failed(*a_id);
    assert!(matches!(app.preview(), PreviewState::Failed { key } if *key == ka));
    assert!(!app.select_session(&ka));
    assert!(app.retry_preview());
    let requests = app.take_requests();
    assert!(matches!(
        requests.as_slice(),
        [WorkerRequest::HydratePreview { .. }]
    ));
}

#[test]
fn a_child_only_search_keeps_its_loaded_parent_and_does_not_load_more_roots() {
    let mut app = app();
    let parent = session(Harness::Codex, "parent", NOW - 1_000);
    let parent_key = parent.meta.key.clone();
    let mut snap = support::snapshot(1, vec![parent], done_scan());
    snap.has_more = true;
    assert!(app.apply_roster_snapshot(snap, vec![]));
    app.take_requests();

    assert!(app.load_children(&parent_key));
    let requests = app.take_requests();
    let [WorkerRequest::LoadChildren { request_id, .. }] = requests.as_slice() else {
        panic!("expected a child page request, got {requests:?}");
    };
    let child = metadata(key(Harness::Codex, "child"), "nested reviewer child");
    assert!(app.apply_children(*request_id, &parent_key, vec![Arc::new(child)], None));
    app.take_requests();

    // A child-only match keeps the parent row as context; expanding is explicit.
    app.set_query("nested reviewer");
    let rows: Vec<RosterItem> = app.roster_rows().collect();
    let keys: Vec<&SessionKey> = rows
        .iter()
        .filter_map(|item| match item {
            RosterItem::Session(row) => Some(&row.session.meta.key),
            _ => None,
        })
        .collect();
    assert!(keys.contains(&&parent_key), "parent context must stay");
    assert!(
        keys.iter()
            .any(|k| k.native_id == "child" && k.harness == Harness::Codex)
    );
    assert!(
        !rows
            .iter()
            .any(|item| matches!(item, RosterItem::RootLoadMore)),
        "search is loaded-only"
    );
    assert!(
        app.take_requests().is_empty(),
        "no historical roots are loaded by searching"
    );
}

#[test]
fn harness_filters_offer_six_identities() {
    let app = app();
    let providers: Vec<Harness> = app
        .filter_items()
        .into_iter()
        .filter_map(|item| match item.target {
            FilterTarget::Provider(h) => Some(h),
            _ => None,
        })
        .collect();
    assert_eq!(providers, Harness::ALL);
}

#[test]
fn transcript_opens_at_the_newest_entry_and_backward_paging_keeps_the_anchor() {
    let mut app = app();
    let root = session(Harness::Codex, "s", NOW - 1_000);
    let k = root.meta.key.clone();
    app.apply_roster(vec![root], vec![]);
    app.take_requests(); // the initial selection requested its preview
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected Select, got {requests:?}");
    };
    // e3..e5 are the newest page even though the history is larger.
    assert!(app.apply_transcript(*request_id, backward_page(&k, 3..6, true)));
    let t = app.transcript().unwrap();
    assert_eq!(t.selected(), 2, "lands on the actual newest event");
    assert_eq!(t.selected_event().unwrap().id, "e5");
    assert!(t.has_older());

    // Walking toward the start pages backward from the window's first event.
    app.inspector_move(-100);
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadPage {
            request_id,
            anchor: Some(anchor),
            direction: PageDirection::Backward,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected an older page request, got {requests:?}");
    };
    assert_eq!(anchor.stable_id, "e3");
    assert!(app.apply_transcript(*request_id, backward_page(&k, 0..3, false)));
    let t = app.transcript().unwrap();
    assert_eq!(t.events().len(), 6);
    // The explicit move put the reading anchor on e3; prepending older events
    // keeps the same event selected rather than jumping back to the newest.
    assert_eq!(t.selected_event().unwrap().id, "e3");
    assert_eq!(t.selected(), 3);
    assert!(!t.has_older());
}

#[test]
fn details_pages_accumulate_and_stale_replies_are_rejected() {
    let mut app = app();
    let root = session(Harness::Codex, "s", NOW - 1_000);
    let k = root.meta.key.clone();
    app.apply_roster(vec![root], vec![]);
    app.take_requests(); // the initial selection requested its preview
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected Select, got {requests:?}");
    };
    assert!(app.apply_transcript(*request_id, backward_page(&k, 0..1, false)));
    app.take_requests();
    assert!(app.open_session_info());
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadDetails {
            request_id: first_id,
            key: first_key,
            anchor: None,
        },
    ] = requests.as_slice()
    else {
        panic!("expected the first details page, got {requests:?}");
    };
    assert_eq!(first_key, &k);

    let mut page = SessionDetailPage {
        recent: vec![event_for(&k, 0)],
        has_more: true,
        next: Some(anchor_of(&event_for(&k, 0))),
        ..SessionDetailPage::default()
    };
    assert!(app.apply_details(*first_id, k.clone(), page.clone()));
    assert_eq!(app.details().page().recent.len(), 1);
    assert!(app.details().page().has_more);

    assert!(app.load_more_details());
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadDetails {
            request_id: second_id,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected an older details page, got {requests:?}");
    };
    // The superseded first page no longer changes anything.
    assert!(!app.apply_details(*first_id, k.clone(), page.clone()));
    page.recent = vec![event_for(&k, 1)];
    page.has_more = false;
    page.next = None;
    assert!(app.apply_details(*second_id, k.clone(), page));
    assert_eq!(app.details().page().recent.len(), 2);
    assert!(!app.details().page().has_more);
    assert!(app.details().complete());
}

#[test]
fn restart_resumes_the_reading_anchor_by_walking_back_from_the_tail() {
    let mut app = app();
    let root = session(Harness::Codex, "s", NOW - 1_000);
    let k = root.meta.key.clone();
    app.apply_roster(vec![root], vec![]);
    app.take_requests(); // the initial selection requested its preview
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected Select, got {requests:?}");
    };
    assert!(app.apply_transcript(*request_id, backward_page(&k, 4..10, true)));
    app.inspector_move(-100);
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadPage {
            request_id: older, ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected the older page, got {requests:?}");
    };
    assert!(app.apply_transcript(*older, backward_page(&k, 0..4, false)));
    app.inspector_move(-1);
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e3");

    // The worker was replaced: the same session is re-read from its tail.
    app.restart_transcript();
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, key }] = requests.as_slice() else {
        panic!("expected Select after restart, got {requests:?}");
    };
    assert_eq!(key, &k);
    assert!(app.apply_transcript(*request_id, backward_page(&k, 7..10, true)));
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadPage {
            request_id: older,
            anchor: Some(anchor),
            direction: PageDirection::Backward,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected the resume page, got {requests:?}");
    };
    assert_eq!(anchor.stable_id, "e7");
    assert!(app.apply_transcript(*older, backward_page(&k, 4..7, true)));
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadPage {
            request_id: oldest,
            anchor: Some(anchor),
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected one more resume page, got {requests:?}");
    };
    assert_eq!(anchor.stable_id, "e4");
    assert!(app.apply_transcript(*oldest, backward_page(&k, 0..4, false)));
    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, "e3");
    // Recovery replaces pages instead of prepending the whole history.
    assert_eq!(t.events().len(), 4);
    assert!(!t.has_older());

    // Forward pagination resumes after the retained page's last event, so the
    // discarded window's first event (e4) is not skipped.
    app.inspector_move(isize::MAX);
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadPage {
            request_id: forward,
            anchor: Some(anchor),
            direction: PageDirection::Forward,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected forward resume, got {requests:?}");
    };
    assert_eq!(anchor.stable_id, "e3");
    assert!(app.apply_transcript(*forward, forward_page(&k, 4..10, false)));
    let ids: Vec<String> = app
        .transcript()
        .unwrap()
        .events()
        .iter()
        .map(|e| e.id.clone())
        .collect();
    let expected: Vec<String> = (0..10).map(|i| format!("e{i}")).collect();
    assert_eq!(ids, expected, "forward round trip has no gap or duplicate");
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e3");
}

#[test]
fn a_single_event_backward_tail_page_still_reaches_older_history() {
    let mut app = app();
    let root = session(Harness::Codex, "s", NOW - 1_000);
    let k = root.meta.key.clone();
    app.apply_roster(vec![root], vec![]);
    app.take_requests(); // the initial selection requested its preview
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected Select, got {requests:?}");
    };
    let mut tail = backward_page(&k, 5..6, true);
    tail.next = Some(anchor_of(&tail.events[0]));
    assert!(app.apply_transcript(*request_id, tail));
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e5");
    app.inspector_move(-100);
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadPage {
            request_id: older,
            anchor: Some(anchor),
            direction: PageDirection::Backward,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("single-event tail must keep an older anchor: {requests:?}");
    };
    assert_eq!(anchor.stable_id, "e5");
    assert!(app.apply_transcript(*older, backward_page(&k, 3..5, false)));
    assert_eq!(app.transcript().unwrap().events().len(), 3);
}

#[test]
fn details_reset_between_sessions_and_on_reopen() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let b = session(Harness::Claude, "b", NOW - 2_000);
    let (ka, kb) = (a.meta.key.clone(), b.meta.key.clone());
    app.apply_roster(vec![a, b], vec![]);
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let select = requests
        .iter()
        .find_map(|r| match r {
            WorkerRequest::Select { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .unwrap();
    assert!(app.apply_transcript(select, backward_page(&ka, 0..1, false)));
    assert!(app.open_session_info());
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadDetails {
            request_id: first,
            anchor: None,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected first details page, got {requests:?}");
    };
    let mut page = SessionDetailPage {
        recent: vec![event_for(&ka, 0)],
        has_more: true,
        next: Some(anchor_of(&event_for(&ka, 0))),
        ..SessionDetailPage::default()
    };
    assert!(app.apply_details(*first, ka.clone(), page.clone()));

    // Closing and reopening the same session starts a fresh stream, not a
    // continuation that duplicates page 1.
    app.close_overlay();
    app.close_inspector();
    assert!(app.open_inspector());
    app.take_requests();
    assert!(app.open_session_info());
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadDetails {
            request_id: second,
            anchor: None,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("reopen must request the newest page, got {requests:?}");
    };
    page.recent = vec![event_for(&ka, 9)];
    page.has_more = true;
    page.next = Some(anchor_of(&event_for(&ka, 9)));
    assert!(app.apply_details(*second, ka.clone(), page));
    assert_eq!(app.details().page().recent.len(), 1);

    // A pending stream for A is retired when B opens.
    app.close_overlay();
    app.close_inspector();
    assert!(app.open_inspector());
    app.take_requests();
    assert!(app.open_session_info());
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadDetails {
            request_id: pending,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected pending details, got {requests:?}");
    };
    app.close_overlay();
    app.close_inspector();
    app.move_selection(1);
    assert_eq!(app.selected_key(), Some(kb.clone()));
    assert!(app.open_inspector());
    let requests = app.take_requests();
    assert!(
        requests.iter().any(
            |r| matches!(r, WorkerRequest::CancelLoad { request_id } if request_id == pending)
        ),
        "abandoned details must be cancelled: {requests:?}"
    );
    assert_eq!(app.details().key(), None);
}

#[test]
fn obsolete_generation_previews_are_rejected_and_released() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let ka = a.meta.key.clone();
    assert!(app.apply_roster_snapshot(support::snapshot(1, vec![a.clone()], done_scan()), vec![]));
    // The snapshot's own selection asked for the preview.
    let requests = app.take_requests();
    let [WorkerRequest::HydratePreview { request_id, .. }] = requests.as_slice() else {
        panic!("expected preview, got {requests:?}");
    };
    assert!(app.apply_roster_snapshot(support::snapshot(2, vec![a.clone()], done_scan()), vec![]));
    // The old job is released and the new generation asks again for the same row.
    let requests = app.take_requests();
    let [
        WorkerRequest::CancelLoad { request_id: old },
        WorkerRequest::HydratePreview {
            request_id: fresh, ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected cancel then a fresh preview, got {requests:?}");
    };
    assert_eq!(old, request_id);
    assert_ne!(fresh, request_id);
    assert!(matches!(app.preview(), PreviewState::Loading { .. }));
    assert!(!app.apply_preview(*request_id, ka.clone(), Arc::new(a.clone()), true));
    assert!(app.apply_preview(*fresh, ka, Arc::new(a), true));
}

#[test]
fn visual_positions_include_child_load_more_and_released_children_stay_hidden() {
    let mut app = app();
    let parent = session(Harness::Codex, "parent", NOW - 1_000);
    let other = session(Harness::Claude, "other", NOW - 5_000);
    let parent_key = parent.meta.key.clone();
    let mut snap = support::snapshot(1, vec![parent, other], done_scan());
    snap.has_more = false;
    assert!(app.apply_roster_snapshot(snap, vec![]));
    app.take_requests();
    assert!(app.load_children(&parent_key));
    let requests = app.take_requests();
    let [WorkerRequest::LoadChildren { request_id, .. }] = requests.as_slice() else {
        panic!("expected child page, got {requests:?}");
    };
    let child_key = key(Harness::Codex, "child");
    let mut child = metadata(child_key.clone(), "child");
    child.released = ReleaseState::Released;
    assert!(app.apply_children(
        *request_id,
        &parent_key,
        vec![Arc::new(child.clone())],
        Some(child_key.clone())
    ));
    app.toggle_children(); // parent selected
    let rows: Vec<RosterItem> = app.roster_rows().collect();
    // The released child is hidden even without a search, but its Load more
    // row remains an explicit visual row.
    assert!(
        rows.iter()
            .any(|item| matches!(item, RosterItem::ChildLoadMore { .. }))
    );
    assert!(!rows.iter().any(|item| matches!(
        item,
        RosterItem::Session(row) if row.session.meta.key == child_key
    )));
    assert_eq!(
        app.provider_counts()
            .into_iter()
            .find(|(h, _)| *h == Harness::Codex),
        Some((Harness::Codex, 2)),
        "loaded children count toward their harness"
    );

    // Selecting the second root accounts for the group heading and the
    // ChildLoadMore row before it.
    assert_eq!(app.visual_selected_index(), Some(1), "parent selected");
    app.move_selection(isize::MAX);
    assert_eq!(app.selected_index(), Some(1));
    assert_eq!(app.visual_selected_index(), Some(3));
    // Collapsing the family removes the ChildLoadMore row and the cached visual
    // position follows immediately.
    app.move_selection(isize::MIN);
    app.toggle_children();
    assert_eq!(app.visual_selected_index(), Some(1));
    app.move_selection(isize::MAX);
    assert_eq!(app.visual_selected_index(), Some(2));
}

#[test]
fn roster_rows_emit_group_header_before_each_project() {
    let mut app = app();
    let a = with_project(session(Harness::Codex, "a", NOW - 1000), Some("X"));
    let b = with_project(session(Harness::Claude, "b", NOW - 2000), Some("Y"));
    let c = with_project(session(Harness::OpenCode, "c", NOW - 3000), Some("X"));
    app.apply_roster(vec![a, b, c], vec![]);

    // Group order follows the first member in view order; C joins X even though
    // B sits between them.
    let rows: Vec<RosterItem> = app.roster_rows().collect();
    let shape: Vec<String> = rows
        .iter()
        .map(|item| match item {
            RosterItem::GroupHeader(header) => {
                format!("header:{}", header.project.unwrap_or("none"))
            }
            RosterItem::Session(row) => row.session.meta.key.native_id.clone(),
            RosterItem::RootLoadMore => "roots-more".into(),
            RosterItem::ChildLoadMore { .. } => "child-more".into(),
        })
        .collect();
    assert_eq!(shape, ["header:X", "a", "c", "header:Y", "b"]);
    assert_eq!(rows[0].height(), 1);
    assert_eq!(rows[1].height(), 2);
    assert_eq!(rows[3].height(), 1);
    assert!(matches!(
        &rows[0],
        RosterItem::GroupHeader(header) if header.totals.sessions == 2
    ));
    assert_eq!(group_headers(&app).len(), 2);

    // Selection moves between sessions only; a heading is never a stop.
    assert_eq!(selected_id(&app).as_deref(), Some("a"));
    assert_eq!(roster_ids(&app), ["a", "c", "b"]);
    app.move_selection(1);
    assert_eq!(selected_id(&app).as_deref(), Some("c"));
    app.move_selection(1);
    assert_eq!(selected_id(&app).as_deref(), Some("b"));
}

#[test]
fn sessions_without_project_group_under_none() {
    let mut app = app();
    app.apply_roster(
        vec![
            session(Harness::Codex, "a", NOW - 1000),
            session(Harness::Claude, "b", NOW - 2000),
            with_project(session(Harness::OpenCode, "c", NOW - 3000), Some("X")),
        ],
        vec![],
    );
    let headers = group_headers(&app);
    assert_eq!(headers.len(), 2);
    assert_eq!(headers[0].0, None, "roots without a project lead here");
    assert_eq!(headers[0].1.sessions, 2);
    assert_eq!(headers[1].0.as_deref(), Some("X"));
    assert_eq!(headers[1].1.sessions, 1);
}

#[test]
fn child_stays_in_parent_group_even_with_other_project() {
    let mut app = app();
    let parent = with_project(session(Harness::Codex, "parent", NOW - 1000), Some("X"));
    let parent_key = parent.meta.key.clone();
    app.apply_roster(vec![parent], vec![]);
    app.take_requests();
    assert!(app.load_children(&parent_key));
    let requests = app.take_requests();
    let [WorkerRequest::LoadChildren { request_id, .. }] = requests.as_slice() else {
        panic!("expected a child page request, got {requests:?}");
    };
    let child_key = key(Harness::Codex, "child");
    let mut child = metadata(child_key, "child");
    child.project = Some("Y".into());
    assert!(app.apply_children(*request_id, &parent_key, vec![Arc::new(child)], None));
    app.toggle_children();

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
    // Lineage wins: the child's own project never opens a second group.
    assert_eq!(shape, ["header:X", "parent", "child"]);
    assert_eq!(group_headers(&app).len(), 1);
}

#[test]
fn group_totals_sum_roots_and_loaded_children() {
    let mut app = app();
    let mut parent = with_project(session(Harness::Codex, "parent", NOW - 1000), Some("X"));
    parent.usage = Some(usage(Some(1_000), Some(1.5)));
    let parent_key = parent.meta.key.clone();
    app.apply_roster(vec![parent], vec![]);
    app.take_requests();
    assert!(app.load_children(&parent_key));
    let requests = app.take_requests();
    let [WorkerRequest::LoadChildren { request_id, .. }] = requests.as_slice() else {
        panic!("expected a child page request, got {requests:?}");
    };
    let child_key = key(Harness::Codex, "child");
    let child = metadata(child_key.clone(), "child");
    assert!(app.apply_children(
        *request_id,
        &parent_key,
        vec![Arc::new(child.clone())],
        None
    ));
    app.toggle_children();

    let mut child_summary = facts::summary(child);
    child_summary.usage = Some(usage(Some(500), Some(0.5)));
    assert!(app.select_row(&child_key));
    let requests = app.take_requests();
    let request_id = requests
        .iter()
        .find_map(|request| match request {
            WorkerRequest::HydratePreview { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("a preview request for the child");
    assert!(app.apply_preview(request_id, child_key, Arc::new(child_summary), true));

    let totals = group_headers(&app).remove(0).1;
    assert_eq!(totals.sessions, 2, "root plus loaded child");
    assert_eq!(totals.tokens, Some(1_500));
    assert_eq!(totals.cost_usd, Some(2.0));
    assert!(!totals.tokens_partial);
    assert!(!totals.cost_partial);
}

#[test]
fn group_totals_mark_partial_and_estimated() {
    let mut app = app();
    let mut partial = session(Harness::Codex, "partial", NOW - 1000);
    let mut partial_usage = usage(Some(10), Some(1.0));
    partial_usage.cost.class = AggregationClass::Partial;
    partial.usage = Some(partial_usage);
    let mut estimated = session(Harness::Claude, "estimated", NOW - 2000);
    estimated.usage = Some(usage(Some(20), Some(2.0)));
    let unknown = session(Harness::OpenCode, "unknown", NOW - 3000);
    app.apply_roster(vec![partial, estimated, unknown], vec![]);

    let totals = group_headers(&app).remove(0).1;
    assert_eq!(totals.sessions, 3);
    assert_eq!(totals.tokens, Some(30));
    assert_eq!(totals.cost_usd, Some(3.0));
    assert!(totals.tokens_partial, "one member recorded no tokens");
    assert!(totals.cost_partial, "a partial class and a missing amount");
    assert!(totals.cost_estimated, "one summed amount is an estimate");
}

#[test]
fn group_totals_partial_when_more_roots_exist() {
    let mut app = app();
    let mut a = with_project(session(Harness::Codex, "a", NOW - 1000), Some("X"));
    a.usage = Some(usage(Some(10), Some(1.0)));
    let mut b = with_project(session(Harness::Claude, "b", NOW - 2000), Some("Y"));
    b.usage = Some(usage(Some(20), Some(2.0)));
    let mut snap = support::snapshot(1, vec![a, b], done_scan());
    snap.has_more = true;
    assert!(app.apply_roster_snapshot(snap, vec![]));

    let headers = group_headers(&app);
    assert_eq!(headers.len(), 2);
    for (_, totals) in headers {
        assert!(totals.tokens_partial, "roots beyond the page are unknown");
        assert!(totals.cost_partial, "roots beyond the page are unknown");
    }
}

#[test]
fn group_totals_use_known_subtotal_with_plus() {
    let mut app = app();
    let mut short = session(Harness::Codex, "short", NOW - 1000);
    let mut short_usage = usage(Some(10), None);
    short_usage.cost.known_subtotal = 0.75;
    short_usage.cost.class = AggregationClass::Reported;
    short.usage = Some(short_usage);
    app.apply_roster(vec![short], vec![]);

    let totals = group_headers(&app).remove(0).1;
    assert_eq!(totals.cost_usd, Some(0.75));
    assert!(totals.cost_partial, "a priced subtotal is a lower bound");
    assert!(!totals.cost_estimated);
    assert!(!totals.tokens_partial);
}

#[test]
fn group_totals_none_when_nothing_recorded() {
    let mut app = app();
    app.apply_roster(
        vec![
            session(Harness::Codex, "a", NOW - 1000),
            session(Harness::Claude, "b", NOW - 2000),
        ],
        vec![],
    );
    let totals = group_headers(&app).remove(0).1;
    assert_eq!(totals.sessions, 2);
    assert_eq!(totals.tokens, None, "never a reported zero");
    assert_eq!(totals.cost_usd, None, "never a reported $0.00");
    assert!(totals.tokens_partial && totals.cost_partial);
}

#[test]
fn sort_cycle_has_no_project_mode() {
    let mut app = app();
    let mut seen = Vec::new();
    for _ in 0..4 {
        app.cycle_sort();
        seen.push(app.sort_mode());
    }
    assert_eq!(
        seen,
        [
            SortMode::Provider,
            SortMode::Tokens,
            SortMode::Status,
            SortMode::LastActivity,
        ],
        "four steps return to the default; project is not a mode"
    );
}

#[test]
fn restart_anchor_recovery_is_bounded_by_the_window_cap() {
    let cap = agentscope::app::MAX_LOADED_EVENTS;
    let mut app = app();
    let root = session(Harness::Codex, "s", NOW - 1_000);
    let k = root.meta.key.clone();
    app.apply_roster(vec![root], vec![]);
    app.take_requests(); // the initial selection requested its preview
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected Select, got {requests:?}");
    };
    // The reading anchor is an event the re-read history no longer contains.
    let mut old = backward_page(&k, 1_000_000..1_000_001, false);
    old.next = Some(anchor_of(&old.events[0]));
    assert!(app.apply_transcript(*request_id, old));
    app.restart_transcript();
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected Select, got {requests:?}");
    };
    assert!(app.apply_transcript(*request_id, backward_page(&k, 0..1, true)));
    let mut scanned = 0usize;
    loop {
        let requests = app.take_requests();
        let [
            WorkerRequest::LoadPage {
                request_id,
                anchor: Some(_),
                direction: PageDirection::Backward,
                ..
            },
        ] = requests.as_slice()
        else {
            break;
        };
        let start = (scanned + 10_000) as u64;
        scanned += cap / 2;
        assert!(app.apply_transcript(
            *request_id,
            backward_page(&k, start..start + (cap as u64 / 2), true)
        ));
        assert!(app.transcript().unwrap().events().len() <= cap);
        if scanned > cap {
            break;
        }
    }
    assert!(scanned >= cap, "recovery scanned beyond the cap: {scanned}");
    assert!(
        app.take_requests().is_empty(),
        "bounded recovery must stop asking for more history"
    );
}

fn preview_keys(requests: &[WorkerRequest]) -> Vec<SessionKey> {
    requests
        .iter()
        .filter_map(|r| match r {
            WorkerRequest::HydratePreview { key, .. } => Some(key.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_snapshot_selection_requests_its_preview_and_a_refresh_asks_again() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let b = session(Harness::Claude, "b", NOW - 2_000);
    let ka = a.meta.key.clone();
    assert!(app.apply_roster_snapshot(
        support::snapshot(1, vec![a.clone(), b.clone()], done_scan()),
        vec![]
    ));
    assert_eq!(app.selected_key(), Some(ka.clone()));
    assert_eq!(
        preview_keys(&app.take_requests()),
        std::slice::from_ref(&ka)
    );
    assert!(app.preview().is_loading());

    // The same generation (live summaries, clock ticks) does not ask again.
    assert!(app.apply_roster_snapshot(
        support::snapshot(1, vec![a.clone(), b.clone()], done_scan()),
        vec![]
    ));
    app.set_now(NOW + 1_000);
    assert!(app.take_requests().is_empty());

    // A refresh freezes a new generation: the old job is released and the
    // selected row is requested again, once.
    assert!(app.apply_roster_snapshot(
        support::snapshot(2, vec![b.clone(), a.clone()], done_scan()),
        vec![]
    ));
    let requests = app.take_requests();
    assert!(
        matches!(requests[0], WorkerRequest::CancelLoad { .. }),
        "{requests:?}"
    );
    assert_eq!(preview_keys(&requests).len(), 1, "{requests:?}");
    assert_eq!(preview_keys(&requests), [app.selected_key().unwrap()]);
}

#[test]
fn a_search_that_moves_the_selection_requests_the_new_preview_and_cancels_the_old() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let b = session(Harness::Claude, "b", NOW - 2_000);
    let kb = b.meta.key.clone();
    assert!(app.apply_roster_snapshot(support::snapshot(1, vec![a, b], done_scan()), vec![]));
    let first = app.take_requests();
    let [
        WorkerRequest::HydratePreview {
            request_id: a_id, ..
        },
    ] = first.as_slice()
    else {
        panic!("{first:?}");
    };
    app.set_query("title b");
    assert_eq!(app.selected_key(), Some(kb.clone()));
    let requests = app.take_requests();
    assert!(
        matches!(requests.first(), Some(WorkerRequest::CancelLoad { request_id }) if request_id == a_id),
        "{requests:?}"
    );
    assert_eq!(preview_keys(&requests), std::slice::from_ref(&kb));

    // Clearing the search returns to rows that keep the selection: no new request.
    app.set_query("");
    assert_eq!(app.selected_key(), Some(kb));
    assert!(app.take_requests().is_empty());
}

#[test]
fn a_child_selection_is_hydrated_once_and_a_filter_that_drops_it_selects_a_visible_row() {
    let mut app = app();
    let parent = session(Harness::Codex, "parent", NOW - 1_000);
    let parent_key = parent.meta.key.clone();
    assert!(app.apply_roster_snapshot(support::snapshot(1, vec![parent], done_scan()), vec![]));
    app.take_requests();
    assert!(app.load_children(&parent_key));
    let requests = app.take_requests();
    let [WorkerRequest::LoadChildren { request_id, .. }] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    let child_key = key(Harness::Codex, "child");
    let child = metadata(child_key.clone(), "nested reviewer child");
    assert!(app.apply_children(*request_id, &parent_key, vec![Arc::new(child)], None));
    app.toggle_children();
    app.take_requests();

    app.move_selection(1);
    assert_eq!(app.selected_key(), Some(child_key.clone()));
    assert_eq!(
        preview_keys(&app.take_requests()),
        std::slice::from_ref(&child_key)
    );

    // Rebuilds that keep the child selected never duplicate its request.
    app.set_now(NOW + 5_000);
    app.set_query("nested reviewer");
    assert_eq!(app.selected_key(), Some(child_key.clone()));
    assert!(preview_keys(&app.take_requests()).is_empty());

    // A query the child no longer matches moves the selection and its preview.
    app.set_query("no such session");
    assert_ne!(app.selected_key(), Some(child_key));
    let requests = app.take_requests();
    assert!(preview_keys(&requests).len() <= 1, "{requests:?}");
    if let Some(selected) = app.selected_key() {
        assert_eq!(preview_keys(&requests), [selected]);
    }
}

// ---- Task 5: bounded last-activity read for the preview --------------------

fn empty_page() -> TranscriptPage {
    TranscriptPage {
        events: vec![],
        diagnostics: vec![],
        next: None,
        has_more: false,
        truncated: false,
    }
}

fn reply_event(key: &SessionKey, i: u64, text: &str) -> Event {
    let mut event = event_for(key, i);
    event.text = Some(BoundedText {
        preview: text.to_owned(),
        byte_len: text.len(),
        truncated: false,
        locator: None,
    });
    event
}

fn tool_event(key: &SessionKey, i: u64, name: &str, input: &str) -> Event {
    let mut event = event_for(key, i);
    event.kind = EventKind::Tool;
    event.actor = Actor::Assistant;
    event.text = None;
    event.tool = Some(ToolInteraction {
        name: name.to_owned(),
        call_id: None,
        input: Some(BoundedText {
            preview: input.to_owned(),
            byte_len: input.len(),
            truncated: false,
            locator: None,
        }),
        output: None,
        status: ToolStatus::Completed,
        exit_code: None,
        started_at: None,
        completed_at: None,
        orphan: false,
        child_session: None,
    });
    event
}

fn activity_page(events: Vec<Event>, has_more: bool) -> TranscriptPage {
    let next = events.first().map(anchor_of);
    TranscriptPage {
        events,
        diagnostics: vec![],
        next,
        has_more,
        truncated: false,
    }
}

/// Drain the queued activity read request, if one was issued.
fn take_activity_request(app: &mut App) -> Option<u64> {
    app.take_requests()
        .into_iter()
        .find_map(|request| match request {
            WorkerRequest::PreviewTail { request_id, .. } => Some(request_id),
            _ => None,
        })
}

/// Register the current selection with the debounce and return the request id
/// the settled selection issues.
fn request_activity(app: &mut App, at: u64) -> u64 {
    app.settle_selection(at);
    app.settle_selection(at + ACTIVITY_DEBOUNCE_MS);
    take_activity_request(app).expect("the settled selection asks for its activity")
}

#[test]
fn activity_requested_once_after_debounce() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let key = a.meta.key.clone();
    app.apply_roster(vec![a], vec![]);
    app.take_requests(); // the initial preview hydration

    app.settle_selection(0);
    assert!(
        app.take_requests().is_empty(),
        "a changed selection only starts the timer"
    );
    app.settle_selection(149);
    assert!(app.take_requests().is_empty(), "not yet settled");
    app.settle_selection(150);
    let requests = app.take_requests();
    let [
        WorkerRequest::PreviewTail {
            request_id,
            key: asked,
            anchor: None,
        },
    ] = requests.as_slice()
    else {
        panic!("expected one newest-page activity read, got {requests:?}");
    };
    assert_eq!(asked, &key);
    let _ = request_id;
    assert_eq!(app.activity(&key), Some(&ActivityState::Loading));

    app.settle_selection(10_000);
    assert!(
        app.take_requests().is_empty(),
        "an unsettled read is not requested twice"
    );
}

#[test]
fn selection_change_cancels_inflight_activity() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let b = session(Harness::Claude, "b", NOW - 2_000);
    let (ka, kb) = (a.meta.key.clone(), b.meta.key.clone());
    app.apply_roster(vec![a, b], vec![]);
    app.take_requests();
    let first = request_activity(&mut app, 0);
    assert_eq!(app.activity(&ka), Some(&ActivityState::Loading));

    // Moving the selection abandons A's read immediately, before B is settled.
    app.move_selection(1);
    assert_eq!(app.selected_key(), Some(kb.clone()));
    app.take_requests();
    app.settle_selection(1_000);
    let requests = app.take_requests();
    let [
        WorkerRequest::CancelLoad {
            request_id: cancelled,
        },
    ] = requests.as_slice()
    else {
        panic!("expected A's read to be released, got {requests:?}");
    };
    assert_eq!(cancelled, &first);
    assert!(
        app.activity(&ka).is_none(),
        "the abandoned read has no entry"
    );

    // B's own timer starts on the same tick and asks only after the debounce.
    app.settle_selection(1_149);
    assert!(app.take_requests().is_empty());
    app.settle_selection(1_150);
    let requests = app.take_requests();
    let [
        WorkerRequest::PreviewTail {
            request_id: second,
            key: asked,
            anchor: None,
        },
    ] = requests.as_slice()
    else {
        panic!("expected B's activity read, got {requests:?}");
    };
    assert_eq!(asked, &kb);
    assert_eq!(app.activity(&kb), Some(&ActivityState::Loading));

    // A late page for A cannot land, and cannot request a scan-back page.
    let late = activity_page(
        vec![tool_event(&ka, 10, "exec", r#"{"command":"cargo test"}"#)],
        true,
    );
    assert!(!app.apply_activity(first, ka.clone(), late));
    assert!(app.activity(&ka).is_none());
    assert!(
        take_activity_request(&mut app).is_none(),
        "an abandoned read never scans back"
    );
    let _ = second;
}

#[test]
fn activity_scans_back_at_most_one_page() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let key = a.meta.key.clone();
    app.apply_roster(vec![a], vec![]);
    app.take_requests();
    let id = request_activity(&mut app, 0);

    let tool = tool_event(&key, 10, "exec", r#"{"command":"cargo test"}"#);
    let first = TranscriptPage {
        events: vec![tool.clone()],
        diagnostics: vec![],
        next: Some(anchor_of(&tool)),
        has_more: true,
        truncated: false,
    };
    assert!(app.apply_activity(id, key.clone(), first));
    assert_eq!(app.activity(&key), Some(&ActivityState::Loading));
    let requests = app.take_requests();
    let [
        WorkerRequest::PreviewTail {
            request_id,
            key: asked,
            anchor: Some(older),
        },
    ] = requests.as_slice()
    else {
        panic!("expected one older-page read, got {requests:?}");
    };
    assert_eq!(request_id, &id);
    assert_eq!(asked, &key);
    assert_eq!(older, &anchor_of(&tool));

    // The older page supplies the reply; the newest page keeps the tool line.
    let reply = reply_event(&key, 1, "older reply");
    let second = TranscriptPage {
        events: vec![reply.clone()],
        diagnostics: vec![],
        next: Some(anchor_of(&reply)),
        has_more: true,
        truncated: false,
    };
    assert!(app.apply_activity(*request_id, key.clone(), second));
    assert!(
        app.take_requests().is_empty(),
        "scan-back is bounded to one older page"
    );
    let Some(ActivityState::Ready(activity)) = app.activity(&key) else {
        panic!(
            "expected a ready activity state, got {:?}",
            app.activity(&key)
        );
    };
    assert_eq!(
        activity.reply.as_ref().map(|text| text.preview.as_str()),
        Some("older reply")
    );
    assert_eq!(
        activity.tool.as_ref().map(|line| line.name.as_str()),
        Some("exec")
    );
    assert_eq!(activity.at, Some(NOW + 10), "the newest event wins the age");
}

#[test]
fn activity_nothing_and_failed_states() {
    let mut app = app();
    let mut a = session(Harness::Codex, "a", NOW - 1_000);
    a.displayed_event_count = 4;
    let key = a.meta.key.clone();
    app.apply_roster(vec![a.clone()], vec![]);
    app.take_requests();

    let id = request_activity(&mut app, 0);
    assert!(app.apply_activity(id, key.clone(), empty_page()));
    assert_eq!(app.activity(&key), Some(&ActivityState::Nothing));

    // An append drops the entry, so the next settled selection reads again.
    a.displayed_event_count = 5;
    app.apply_roster(vec![a], vec![]);
    app.take_requests();
    assert!(app.activity(&key).is_none());
    app.settle_selection(1_000);
    let failed_id = take_activity_request(&mut app).expect("a fresh read after the append");
    app.load_failed(failed_id);
    assert_eq!(app.activity(&key), Some(&ActivityState::Failed));

    app.settle_selection(10_000);
    assert!(
        app.take_requests().is_empty(),
        "a failed read waits for invalidation, not the next tick"
    );
}

#[test]
fn activity_cache_invalidated_on_roster_change() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let key = a.meta.key.clone();
    app.apply_roster(vec![a], vec![]);
    app.take_requests();
    let id = request_activity(&mut app, 0);
    assert!(app.apply_activity(
        id,
        key.clone(),
        activity_page(vec![reply_event(&key, 1, "reply")], false)
    ));
    assert!(matches!(app.activity(&key), Some(ActivityState::Ready(_))));

    // The same protected session changed in the refreshed roster.
    app.apply_roster(vec![session(Harness::Codex, "a", NOW - 500)], vec![]);
    assert!(
        app.activity(&key).is_none(),
        "a changed session drops its cached activity"
    );
    app.take_requests();
    app.settle_selection(1_000);
    assert!(
        take_activity_request(&mut app).is_some(),
        "the settled selection reads the changed session again"
    );
}

#[test]
fn activity_cache_invalidated_on_append() {
    let mut app = app();
    let mut a = session(Harness::Codex, "a", NOW - 1_000);
    a.displayed_event_count = 5;
    let key = a.meta.key.clone();
    assert!(app.apply_roster_snapshot(support::snapshot(1, vec![a.clone()], done_scan()), vec![]));
    app.take_requests();
    let id = request_activity(&mut app, 0);
    assert!(app.apply_activity(
        id,
        key.clone(),
        activity_page(vec![reply_event(&key, 1, "reply")], false)
    ));
    assert!(matches!(app.activity(&key), Some(ActivityState::Ready(_))));

    a.displayed_event_count = 6;
    assert!(app.apply_roster_snapshot(support::snapshot(2, vec![a], done_scan()), vec![]));
    assert!(
        app.activity(&key).is_none(),
        "a larger displayed event count clears the cached entry"
    );
}

#[test]
fn change_during_inflight_read_discards_late_page() {
    let mut app = app();
    let mut a = session(Harness::Codex, "a", NOW - 1_000);
    a.displayed_event_count = 2;
    let key = a.meta.key.clone();
    app.apply_roster(vec![a.clone()], vec![]);
    app.take_requests();
    let id = request_activity(&mut app, 0);
    assert_eq!(app.activity(&key), Some(&ActivityState::Loading));

    a.displayed_event_count = 3;
    app.apply_roster(vec![a], vec![]);
    let requests = app.take_requests();
    assert!(
        requests
            .iter()
            .any(|request| matches!(request, WorkerRequest::CancelLoad { request_id } if *request_id == id)),
        "the in-flight read is released when the fingerprint changes"
    );
    assert!(app.activity(&key).is_none());

    let late = activity_page(vec![reply_event(&key, 1, "late reply")], false);
    assert!(
        !app.apply_activity(id, key.clone(), late),
        "a page answering the cancelled read is stale"
    );
    assert!(app.activity(&key).is_none());
}

#[test]
fn preview_hydration_change_invalidates_activity() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let key = a.meta.key.clone();
    app.apply_roster(vec![a.clone()], vec![]);
    let requests = app.take_requests();
    let preview_id = requests
        .iter()
        .find_map(|request| match request {
            WorkerRequest::HydratePreview { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("the initial preview hydration");

    let id = request_activity(&mut app, 0);
    assert!(app.apply_activity(
        id,
        key.clone(),
        activity_page(vec![reply_event(&key, 1, "reply")], false)
    ));
    assert!(matches!(app.activity(&key), Some(ActivityState::Ready(_))));

    // Hydration can change the fingerprint without a roster refresh.
    let mut hydrated = a;
    hydrated.last_event_at = Some(NOW + 5);
    hydrated.displayed_event_count = 9;
    assert!(app.apply_preview(preview_id, key.clone(), Arc::new(hydrated), true));
    assert!(
        app.activity(&key).is_none(),
        "the hydrated summary invalidates the cached tail"
    );
}

#[test]
fn activity_cache_evicts_least_recent_beyond_cap() {
    let mut app = app();
    let sessions: Vec<SessionSummary> = (0..65)
        .map(|i| {
            session(
                Harness::Codex,
                &format!("s{i:02}"),
                NOW - i as Timestamp * 1_000,
            )
        })
        .collect();
    let keys: Vec<SessionKey> = sessions.iter().map(|s| s.meta.key.clone()).collect();
    app.apply_roster(sessions, vec![]);
    app.take_requests();

    // Fill the cache with the first 64 sessions in selection order.
    for (i, key) in keys.iter().take(64).enumerate() {
        if i > 0 {
            app.move_selection(1);
        }
        assert_eq!(app.selected_key().as_ref(), Some(key));
        let at = i as u64 * 1_000;
        let id = request_activity(&mut app, at);
        assert!(app.apply_activity(id, key.clone(), empty_page()));
        assert!(matches!(app.activity(key), Some(ActivityState::Nothing)));
    }

    // Touch #1 without reading it again.
    app.move_selection(-63);
    assert_eq!(app.selected_key().as_ref(), Some(&keys[0]));
    app.settle_selection(100_000);
    app.settle_selection(100_150);
    assert!(
        take_activity_request(&mut app).is_none(),
        "a cached entry is not read again"
    );

    // Inserting the 65th evicts the least recently touched (#2), not #1.
    app.move_selection(64);
    assert_eq!(app.selected_key().as_ref(), Some(&keys[64]));
    let id = request_activity(&mut app, 200_000);
    assert!(app.apply_activity(id, keys[64].clone(), empty_page()));

    assert!(
        app.activity(&keys[0]).is_some(),
        "#1 was touched and survives"
    );
    assert!(app.activity(&keys[1]).is_none(), "#2 is evicted");
    assert!(app.activity(&keys[2]).is_some());
    assert!(app.activity(&keys[64]).is_some());
}

#[test]
fn activity_cancels_when_next_selection_is_cached_or_none() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let b = session(Harness::Claude, "b", NOW - 2_000);
    let (ka, kb) = (a.meta.key.clone(), b.meta.key.clone());
    app.apply_roster(vec![a, b], vec![]);
    app.take_requests();

    // Cache B's activity while B is selected.
    app.move_selection(1);
    app.take_requests();
    let bid = request_activity(&mut app, 0);
    assert!(app.apply_activity(bid, kb.clone(), empty_page()));
    assert!(matches!(app.activity(&kb), Some(ActivityState::Nothing)));

    // Start A's read, then return to the already-cached B: A is released even
    // though B will not start a read of its own.
    app.move_selection(-1);
    app.take_requests();
    let aid = request_activity(&mut app, 1_000);
    assert_eq!(app.activity(&ka), Some(&ActivityState::Loading));
    app.move_selection(1);
    app.take_requests();
    app.settle_selection(2_000);
    let requests = app.take_requests();
    let [WorkerRequest::CancelLoad { request_id }] = requests.as_slice() else {
        panic!("expected A's read to be released for a cached selection, got {requests:?}");
    };
    assert_eq!(request_id, &aid);
    app.settle_selection(2_150);
    assert!(
        app.take_requests().is_empty(),
        "a cached selection asks for nothing"
    );
    assert!(matches!(app.activity(&kb), Some(ActivityState::Nothing)));
    assert!(app.activity(&ka).is_none());

    // No selection also releases the in-flight read.
    app.move_selection(-1);
    app.take_requests();
    let aid = request_activity(&mut app, 3_000);
    assert_eq!(app.activity(&ka), Some(&ActivityState::Loading));
    app.apply_roster(vec![], vec![]);
    app.take_requests();
    assert_eq!(app.selected_key(), None);
    app.settle_selection(4_000);
    let requests = app.take_requests();
    let [WorkerRequest::CancelLoad { request_id }] = requests.as_slice() else {
        panic!("expected the read to be released with no selection, got {requests:?}");
    };
    assert_eq!(request_id, &aid);
    assert!(app.activity(&ka).is_none());
}

#[test]
fn activity_invalidation_cancels_inflight_read() {
    let mut app = app();
    let a = session(Harness::Codex, "a", NOW - 1_000);
    let key = a.meta.key.clone();
    app.apply_roster(vec![a], vec![]);
    app.take_requests();
    let id = request_activity(&mut app, 0);
    assert_eq!(app.activity(&key), Some(&ActivityState::Loading));

    let mut ctx = RosterContext::default();
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::TranscriptInvalidated { key: key.clone() },
    );
    let requests = app.take_requests();
    assert!(
        requests
            .iter()
            .any(|request| matches!(request, WorkerRequest::CancelLoad { request_id } if *request_id == id)),
        "invalidation releases the worker job, not just the cache entry: {requests:?}"
    );
    assert!(app.activity(&key).is_none());

    // The invalidated page is stale and cannot request the scan-back page.
    let first = activity_page(
        vec![tool_event(&key, 10, "exec", r#"{"command":"cargo test"}"#)],
        true,
    );
    assert!(!app.apply_activity(id, key.clone(), first));
    assert!(
        take_activity_request(&mut app).is_none(),
        "an invalidated read never scans back"
    );

    // A replacement read starts once, for the settled selection.
    app.settle_selection(1_000);
    let fresh = take_activity_request(&mut app).expect("a replacement read after invalidation");
    assert_ne!(fresh, id);
    assert_eq!(app.activity(&key), Some(&ActivityState::Loading));
}

#[test]
fn metadata_only_row_hydration_invalidates_activity() {
    let mut app = app();
    let mut meta = metadata(key(Harness::Codex, "m"), "metadata only");
    meta.activity = ActivityStamp {
        at: Some(NOW - 1_000),
        class: AggregationClass::Reported,
    };
    let mk = meta.key.clone();
    let snapshot = RosterSnapshot {
        generation: 1,
        roots: vec![mk.clone()].into(),
        metadata: vec![Arc::new(meta.clone())],
        summaries: vec![],
        scan: vec![done_scan()],
        has_more: false,
        limited: None,
        refresh_available: false,
        provisional: false,
    };
    assert!(app.apply_roster_snapshot(snapshot, vec![]));
    assert_eq!(app.selected_key(), Some(mk.clone()));
    let requests = app.take_requests();
    let preview_id = requests
        .iter()
        .find_map(|request| match request {
            WorkerRequest::HydratePreview { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("the metadata row asks for hydration");

    let id = request_activity(&mut app, 0);
    assert!(app.apply_activity(
        id,
        mk.clone(),
        activity_page(vec![reply_event(&mk, 1, "reply")], false)
    ));
    assert!(matches!(app.activity(&mk), Some(ActivityState::Ready(_))));

    // The placeholder cut (None, 0), so the first hydration with real facts is
    // a change even when no earlier summary was ever observed.
    let hydrated = SessionSummary {
        last_event_at: Some(NOW),
        displayed_event_count: 3,
        ..SessionSummary::unhydrated(meta)
    };
    assert!(app.apply_preview(preview_id, mk.clone(), Arc::new(hydrated), true));
    assert!(
        app.activity(&mk).is_none(),
        "the first hydration invalidates the metadata row's cached activity"
    );
}
