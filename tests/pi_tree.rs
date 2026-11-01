//! Branch-aware Pi accounting: lifetime spend versus active-path context, recorded
//! compaction/branch-summary usage, context invalidation and fork ownership, driven
//! through the concrete `PiTree`/`PiHydration`/`PiCatalog` components over synthetic
//! JSONL. No `ProviderReader` exists for Pi yet, so nothing here takes a test-only
//! provider path.

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    AggregationClass, Harness, Lineage, RefreshBudget, RequestPurpose, SessionKey, SessionLink,
    SessionMetadata, SessionSummary, SourceKey, WarningKind,
};
use agentscope::metrics::tokens_removed;
use agentscope::pricing::PriceSchedule;
use agentscope::providers::pi::{
    PiCatalog, PiEntry, PiHydration, PiInsert, PiOwnership, PiTree, PiTreeError, parse_entry,
};
use serde_json::Value;
use support::facts::price_schedule;
use support::pi as fx;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn prices_with_window(window: Option<u64>) -> PriceSchedule {
    let window = window.map_or_else(String::new, |w| format!(r#","context_capacity":{w}"#));
    price_schedule(&format!(
        r#"[{{"billing_provider":"anthropic","model":"claude-test",
             "rates":{{"input":0.0,"output":10.0,"cache_read":0.0,"cache_write":0.0}}{window}}}]"#
    ))
}

fn config_with(root: &Path, prices: PriceSchedule) -> Config {
    let cli = CliOverrides {
        pi_sessions: Some(root.to_path_buf()),
        ..CliOverrides::default()
    };
    let mut config = Config::load_with_env(&cli, &Env::new()).expect("valid test config");
    config.pricing = prices;
    config
}

#[derive(Default)]
struct Discovery {
    metadata: Vec<SessionMetadata>,
    warnings: Vec<agentscope::domain::Warning>,
}

fn discover(root: &Path) -> Discovery {
    let mut catalog = PiCatalog::new(vec![root.to_path_buf()], 1000, 64 << 20);
    let mut found = Discovery::default();
    let mut seen: HashMap<SessionKey, SessionMetadata> = HashMap::new();
    for _ in 0..10_000 {
        let delta = catalog.step(BIG).expect("discovery");
        for meta in delta.sessions {
            seen.insert(meta.key.clone(), meta);
        }
        found.warnings.extend(delta.warnings);
        if delta.scan.done {
            break;
        }
    }
    found.metadata = seen.into_values().collect();
    found
        .metadata
        .sort_by(|a, b| a.key.native_id.cmp(&b.key.native_id));
    found
}

fn find<'a>(found: &'a Discovery, id: &str) -> &'a SessionMetadata {
    found
        .metadata
        .iter()
        .find(|meta| meta.key.native_id == id)
        .unwrap_or_else(|| panic!("session {id}"))
}

fn hydrated(meta: &SessionMetadata, config: &Config) -> (PiHydration, SessionSummary) {
    let mut hydration = PiHydration::new(meta.clone(), config).expect("hydration");
    for _ in 1..=100_000 {
        let delta = hydration.step(BIG).expect("step");
        if delta.done {
            return (hydration, delta.summary);
        }
    }
    panic!("hydration did not finish");
}

fn usage_of(summary: &SessionSummary) -> &agentscope::domain::UsageSummary {
    summary.usage.as_ref().expect("usage")
}

fn one_session(
    entries: &[Value],
    prices: PriceSchedule,
) -> (tempfile::TempDir, SessionMetadata, Config) {
    let dir = tempfile::tempdir().expect("tempdir");
    fx::write_session(dir.path(), "proj", "session", entries);
    let config = config_with(dir.path(), prices);
    let found = discover(dir.path());
    assert_eq!(found.metadata.len(), 1, "exactly one session");
    (dir, found.metadata[0].clone(), config)
}

/// A `PiTree` built from the same fixture records hydration would read.
fn tree_of(entries: &[Value]) -> PiTree {
    let mut tree = PiTree::new(10_000, 64 << 20);
    for (ordinal, value) in entries.iter().enumerate().skip(1) {
        let entry = parse_entry(value, ordinal as u64, 0).expect("entry");
        assert_eq!(tree.insert(&entry), PiInsert::Inserted);
    }
    tree
}

fn entry_of(value: &Value, ordinal: u64) -> PiEntry {
    parse_entry(value, ordinal, 0).expect("entry")
}

/// user u1, assistant a1 (prompt 120, output 10), abandoned branch u2→a2
/// (prompt 900, output 20), then the recorded return to a1 and assistant a3
/// (prompt 150, output 30).
fn abandoned_branch() -> Vec<Value> {
    vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "start", None),
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "one",
            Some(fx::usage(120, 10, 0, 0)),
        ),
        fx::message("u2", Some("a1"), "user", "try another way", None),
        fx::message(
            "a2",
            Some("u2"),
            "assistant",
            "expensive",
            Some(fx::usage(900, 20, 0, 0)),
        ),
        // Navigation back to a1 is durable: a3's parentId is a1, and it is appended.
        fx::message(
            "a3",
            Some("a1"),
            "assistant",
            "three",
            Some(fx::usage(150, 30, 0, 0)),
        ),
    ]
}

#[test]
fn abandoned_branch_spend_is_lifetime_but_context_follows_the_active_path() {
    let entries = abandoned_branch();
    let (_dir, meta, config) = one_session(&entries, prices_with_window(Some(1000)));
    let (hydration, summary) = hydrated(&meta, &config);

    // Raw provenance of the abandoned branch stays inspectable.
    let requests = hydration.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[2].provenance.harness, Harness::Pi);
    assert_eq!(requests[2].provenance.record_type, "message");
    assert_eq!(requests[2].provenance.source, meta.key.source);
    match &requests[2].provenance.locator.kind {
        agentscope::domain::LocatorKind::FileOffset { path, .. } => {
            assert_eq!(
                path,
                &match &meta.key.source {
                    SourceKey::File(path) => path.clone(),
                    other => panic!("file source: {other:?}"),
                }
            );
        }
        other => panic!("record locator: {other:?}"),
    }

    let usage = usage_of(&summary);
    assert_eq!(
        usage.categories.output,
        Some(60),
        "every branch is lifetime spend"
    );
    assert_eq!(usage.categories.input, Some(1170));
    assert_eq!(usage.total, Some(1230));
    assert_eq!(usage.coverage.observed_requests, 3);

    let context = summary.metrics.context;
    assert_eq!(
        context.used,
        Some(150),
        "the active path's last prompt, not a lifetime total or the abandoned 900"
    );
    assert_eq!(
        context.capacity,
        Some(1000),
        "catalog window of the recorded route"
    );
    assert_eq!(context.class, AggregationClass::Estimated);
    assert_eq!(
        context.model.as_ref().and_then(|m| m.provider.as_deref()),
        Some("anthropic")
    );

    // Only the three conversation requests are timed; no summary exists here.
    assert_eq!(summary.metrics.speed.total_requests, 3);
    assert_eq!(summary.metrics.compactions.observed, 0);

    let tree = tree_of(&entries);
    assert_eq!(tree.active_path().expect("active path"), vec![1, 2, 5]);
    assert_eq!(tree.len(), 5);
}

#[test]
fn compaction_usage_is_a_separate_purpose_counted_once_and_invalidates_context() {
    let entries = abandoned_branch();
    let (dir, meta, config) = one_session(&entries, prices_with_window(Some(1000)));
    let (_hydration, before) = hydrated(&meta, &config);
    assert_eq!(before.metrics.context.used, Some(150));
    assert_eq!(usage_of(&before).total, Some(1230));

    let path = match &meta.key.source {
        SourceKey::File(path) => path.clone(),
        other => panic!("file source: {other:?}"),
    };
    fx::append(
        &path,
        &[fx::compaction(
            "k1",
            "a3",
            Some(fx::with_cost(fx::usage(4, 2, 0, 0), 0.25, false)),
        )],
    );
    let found = discover(dir.path());
    let grown = find(&found, "s1");
    let (_hydration, after) = hydrated(grown, &config);

    let usage = usage_of(&after);
    assert_eq!(
        usage.categories.output,
        Some(62),
        "the summary's output is billed lifetime output"
    );
    assert_eq!(usage.categories.input, Some(1174));
    assert_eq!(
        usage.total,
        Some(1236),
        "the 6 summary tokens are counted once, and the 500-token reduction is not billed"
    );
    assert_eq!(usage.coverage.observed_requests, 4);
    assert_eq!(
        usage.saved_estimate_usd, None,
        "a route-less summary keeps its recorded money at request level only"
    );

    let compactions = &after.metrics.compactions;
    assert_eq!(compactions.observed, 1);
    let latest = compactions.latest.as_ref().expect("latest compaction");
    assert_eq!(latest.before_tokens, Some(500));
    assert_eq!(
        latest.after_tokens, None,
        "Pi records no after-token figure"
    );
    assert_eq!(
        tokens_removed(latest),
        None,
        "a context reduction is not request usage"
    );
    let observation = latest.usage.as_ref().expect("attributable summary usage");
    let request = observation.request.as_ref().expect("request");
    assert_eq!(request.purpose, RequestPurpose::Compaction);
    assert_eq!(observation.categories.input, Some(4));
    assert_eq!(
        observation
            .money
            .saved_estimate
            .as_ref()
            .map(|fact| fact.usd),
        Some(0.25),
        "the recorded summary money is retained once"
    );
    assert_eq!(compactions.known_cost_usd, Some(0.25));

    assert_eq!(
        after.metrics.context.used, None,
        "a later compaction superseded the recorded snapshot"
    );
    assert_eq!(
        after.metrics.context.compaction_epoch.as_deref(),
        Some("c1")
    );

    // Conversation speed excludes the summarization call.
    assert_eq!(after.metrics.speed.total_requests, 3);
    assert_eq!(after.metrics.speed.measured_requests, 0);
}

#[test]
fn compaction_without_usage_has_unknown_cost() {
    let mut entries = abandoned_branch();
    entries.push(fx::compaction("k1", "a3", None));
    let (_dir, meta, config) = one_session(&entries, prices_with_window(None));
    let (_hydration, summary) = hydrated(&meta, &config);

    let compactions = &summary.metrics.compactions;
    assert_eq!(compactions.observed, 1);
    assert!(compactions.latest.as_ref().expect("latest").usage.is_none());
    assert_eq!(compactions.known_cost_usd, None);
    assert_eq!(compactions.unpriced, 1);
    assert_eq!(usage_of(&summary).total, Some(1230));
}

#[test]
fn branch_summary_usage_is_compaction_purpose_and_tool_usage_is_not_billed() {
    let mut tool = fx::tool_result("t1", "u1", 9_000);
    tool["message"]["usage"] = fx::usage(50, 50, 0, 0);
    let entries = vec![
        fx::header("s2", None),
        fx::message("u1", None, "user", "start", None),
        tool,
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "one",
            Some(fx::usage(100, 10, 0, 0)),
        ),
        fx::branch_summary("b1", Some("a1"), Some("a1"), Some(fx::usage(3, 1, 0, 0))),
        fx::message(
            "a2",
            Some("b1"),
            "assistant",
            "two",
            Some(fx::usage(100, 20, 0, 0)),
        ),
    ];
    let (_dir, meta, config) = one_session(&entries, prices_with_window(None));
    let (_hydration, summary) = hydrated(&meta, &config);

    let usage = usage_of(&summary);
    assert_eq!(
        usage.categories.output,
        Some(31),
        "a nested tool-result usage object is not a new billable request"
    );
    assert_eq!(usage.categories.input, Some(203));
    assert_eq!(usage.total, Some(234));
    assert_eq!(usage.coverage.observed_requests, 3);

    let compactions = &summary.metrics.compactions;
    assert_eq!(compactions.observed, 1);
    let observation = compactions
        .latest
        .as_ref()
        .expect("latest")
        .usage
        .as_ref()
        .expect("summary usage");
    assert_eq!(
        observation.request.as_ref().expect("request").purpose,
        RequestPurpose::Compaction
    );
    assert_eq!(observation.categories.output, Some(1));

    // The summary call is billed but is not the timed conversation stream.
    assert_eq!(summary.metrics.speed.total_requests, 2);
    assert_eq!(summary.metrics.context.used, Some(100));
}

#[test]
fn context_capacity_is_catalog_derived_only_for_the_recorded_route() {
    let entries = abandoned_branch();
    let (dir, meta, _config) = one_session(&entries, prices_with_window(Some(1000)));
    let without = config_with(dir.path(), prices_with_window(None));
    let (_hydration, summary) = hydrated(&meta, &without);
    assert_eq!(summary.metrics.context.used, Some(150));
    assert_eq!(
        summary.metrics.context.capacity, None,
        "no catalog window suppresses the percentage"
    );

    let unpriced = config_with(dir.path(), price_schedule("[]"));
    let (_hydration, summary) = hydrated(&meta, &unpriced);
    assert_eq!(summary.metrics.context.capacity, None);
}

#[test]
fn a_later_context_edit_invalidates_the_recorded_snapshot() {
    let mut entries = abandoned_branch();
    entries.push(fx::context_edit("e1", "a3", "u1", None));
    let (_dir, meta, config) = one_session(&entries, prices_with_window(Some(1000)));
    let (_hydration, summary) = hydrated(&meta, &config);
    assert_eq!(summary.metrics.context.used, None);
    assert_eq!(
        usage_of(&summary).total,
        Some(1230),
        "an edit changes future context, not recorded accounting"
    );
    let tree = tree_of(&entries);
    assert_eq!(tree.active_path().expect("active path").last(), Some(&6));

    // A request recorded after the edit is a valid post-edit snapshot.
    entries.push(fx::message(
        "a4",
        Some("e1"),
        "assistant",
        "after edit",
        Some(fx::usage(170, 5, 0, 0)),
    ));
    let (_dir, meta, config) = one_session(&entries, prices_with_window(Some(1000)));
    let (_hydration, summary) = hydrated(&meta, &config);
    assert_eq!(summary.metrics.context.used, Some(170));
}

/// One parent session and a fork that copies it verbatim (the producer preserves
/// entry ids and confirmed response ids) and then records its own request.
fn fork_store(root: &Path) -> (PathBuf, Vec<Value>, Vec<Value>) {
    let parent = vec![
        fx::header("p1", None),
        fx::message("u1", None, "user", "same prompt", None),
        fx::with_response(
            fx::message(
                "a1",
                Some("u1"),
                "assistant",
                "same answer",
                Some(fx::usage(100, 10, 0, 0)),
            ),
            "resp-1",
        ),
        fx::with_response(
            fx::message(
                "a2",
                Some("a1"),
                "assistant",
                "second answer",
                Some(fx::usage(50, 5, 0, 0)),
            ),
            "resp-2",
        ),
    ];
    let parent_path = fx::write_session(root, "proj", "parent", &parent);
    let mut fork = vec![fx::header("f1", Some(&parent_path.to_string_lossy()))];
    fork.extend(parent.iter().skip(1).cloned());
    fork.push(fx::message("u2", Some("a2"), "user", "continue", None));
    fork.push(fx::with_response(
        fx::message(
            "a3",
            Some("u2"),
            "assistant",
            "same answer",
            Some(fx::usage(100, 7, 0, 0)),
        ),
        "resp-3",
    ));
    fx::write_session(root, "proj", "fork", &fork);
    (parent_path, parent, fork)
}

#[test]
fn fork_copies_are_inherited_and_new_identical_text_requests_are_owned() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_parent_path, parent_entries, fork_entries) = fork_store(dir.path());
    let config = config_with(dir.path(), prices_with_window(Some(1000)));
    let found = discover(dir.path());
    assert_eq!(found.metadata.len(), 2, "a fork is a separate session");
    let parent = find(&found, "p1");
    let fork = find(&found, "f1");

    // Fork linkage is `lineage.fork`, never a native subagent parent.
    let Some(SessionLink::Resolved(target)) = fork.lineage.fork.as_ref() else {
        panic!("resolved fork lineage: {:?}", fork.lineage);
    };
    assert_eq!(target, &parent.key);
    assert!(fork.lineage.parent.is_none());
    assert!(!fork.lineage.sidechain);
    assert!(fork.lineage.agent_id.is_none());
    assert!(parent.lineage.fork.is_none());
    assert!(
        fork.members.is_empty(),
        "a fork is not a native child chain"
    );
    assert!(
        parent.members.is_empty(),
        "no synthetic subagent rows are created for a fork"
    );

    let (_hydration, parent_summary) = hydrated(parent, &config);
    assert_eq!(usage_of(&parent_summary).coverage.observed_requests, 2);
    assert_eq!(usage_of(&parent_summary).categories.output, Some(15));

    let (fork_hydration, fork_summary) = hydrated(fork, &config);
    let usage = usage_of(&fork_summary);
    assert_eq!(
        usage.coverage.observed_requests, 1,
        "the copied parent requests are not the fork's direct spend"
    );
    assert_eq!(usage.categories.output, Some(7));
    assert_eq!(usage.total, Some(107));
    assert_eq!(usage.models.len(), 1);
    assert!(
        usage.coverage.complete,
        "every call of the fork is either inherited or owned"
    );
    let requests: Vec<&str> = fork_hydration
        .requests()
        .iter()
        .filter_map(|request| request.request.as_ref()?.id.as_deref())
        .collect();
    assert_eq!(
        requests,
        vec!["resp-3"],
        "the new identical-text call is owned"
    );
    assert_eq!(
        fork_summary.metrics.context.used,
        Some(100),
        "the copied history is still the fork's context"
    );

    // Component-level ownership with producer-preserved identities.
    let parent_tree = tree_of(&parent_entries);
    let mut fork_tree = PiTree::new(10_000, 64 << 20);
    fork_tree.set_fork(fork.lineage.fork.as_ref());
    assert!(fork_tree.adopt(&parent_tree));
    for (ordinal, value) in fork_entries.iter().enumerate().skip(1) {
        assert_eq!(
            fork_tree.insert(&entry_of(value, ordinal as u64)),
            PiInsert::Inserted
        );
    }
    assert_eq!(fork_tree.ownership("a1"), PiOwnership::Inherited);
    assert_eq!(fork_tree.ownership("a2"), PiOwnership::Inherited);
    assert_eq!(fork_tree.ownership("a3"), PiOwnership::Owned);
    assert_eq!(fork_tree.ownership("u1"), PiOwnership::Owned);
    assert_eq!(fork_tree.ownership("missing"), PiOwnership::Unknown);
    assert_eq!(
        fork_tree.active_path().expect("active path"),
        vec![1, 2, 3, 4, 5]
    );
}

#[test]
fn a_fork_whose_parent_is_not_discovered_stays_partial_and_is_never_opened() {
    let root = tempfile::tempdir().expect("root");
    let elsewhere = tempfile::tempdir().expect("outside root");
    let parent = [
        fx::header("p9", None),
        fx::message(
            "a1",
            None,
            "assistant",
            "outside",
            Some(fx::usage(100, 10, 0, 0)),
        ),
    ];
    let parent_path = fx::write_session(elsewhere.path(), "proj", "parent", &parent);
    // A decoy beside the fork with the same file name but a different identity: a
    // shared basename is not predecessor evidence for a rooted reference.
    fx::write_session(
        root.path(),
        "proj",
        "parent",
        &[
            fx::header("decoy", None),
            fx::message(
                "d1",
                None,
                "assistant",
                "decoy",
                Some(fx::usage(7, 1, 0, 0)),
            ),
        ],
    );
    let fork = vec![
        fx::header("f9", Some(&parent_path.to_string_lossy())),
        parent[1].clone(),
        fx::message(
            "a2",
            Some("a1"),
            "assistant",
            "own",
            Some(fx::usage(40, 4, 0, 0)),
        ),
    ];
    fx::write_session(root.path(), "proj", "fork", &fork);

    let found = discover(root.path());
    assert_eq!(
        found.metadata.len(),
        2,
        "only the configured root is walked; the decoy is a separate session"
    );
    let meta = find(&found, "f9");
    assert!(
        matches!(meta.lineage.fork, Some(SessionLink::Unresolved(_))),
        "an out-of-root parent is unresolved, not resolved by its basename"
    );
    let config = config_with(root.path(), prices_with_window(None));
    let (_hydration, summary) = hydrated(meta, &config);
    assert_eq!(
        usage_of(&summary).coverage.observed_requests,
        0,
        "nothing in an unproven fork is charged"
    );
    assert!(!usage_of(&summary).coverage.complete);
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning.key.source.ends_with("#ownership")),
        "the unproven ownership is warned: {:?}",
        summary.warnings
    );
    assert_eq!(
        summary.metrics.context.used,
        Some(40),
        "the copied path is still context"
    );
}

#[test]
fn remapped_entry_ids_keep_a_producer_preserved_response_identity_inherited() {
    // A fixture that rewrites entry ids but keeps the confirmed response ids the
    // producer copies: the provider response identity still proves the copy.
    let parent_value = fx::with_response(
        fx::message(
            "a1",
            None,
            "assistant",
            "same answer",
            Some(fx::usage(100, 10, 0, 0)),
        ),
        "resp-1",
    );
    let mut parent = PiTree::new(1000, 1 << 20);
    assert_eq!(
        parent.insert(&entry_of(&parent_value, 0)),
        PiInsert::Inserted
    );

    let remapped = fx::with_response(
        fx::message(
            "x1",
            None,
            "assistant",
            "same answer",
            Some(fx::usage(100, 10, 0, 0)),
        ),
        "resp-1",
    );
    let mut fork = PiTree::new(1000, 1 << 20);
    fork.set_fork(Some(&SessionLink::Resolved(SessionKey {
        harness: Harness::Pi,
        native_id: "p1".into(),
        source: SourceKey::File(PathBuf::from("parent.jsonl")),
    })));
    assert!(fork.adopt(&parent));
    assert_eq!(fork.insert(&entry_of(&remapped, 1)), PiInsert::Inserted);
    assert_eq!(fork.ownership("x1"), PiOwnership::Inherited);

    // A later genuinely new request is still owned: the copied boundary is proven.
    let fresh = fx::with_response(
        fx::message(
            "x2",
            Some("x1"),
            "assistant",
            "same answer",
            Some(fx::usage(80, 8, 0, 0)),
        ),
        "resp-9",
    );
    assert_eq!(fork.insert(&entry_of(&fresh, 2)), PiInsert::Inserted);
    assert_eq!(fork.ownership("x2"), PiOwnership::Owned);
}

#[test]
fn remapped_identities_without_a_proven_boundary_stay_unknown() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (_parent_path, _parent_entries, fork_entries) = fork_store(dir.path());
    let mut parent = PiTree::new(1000, 1 << 20);
    for (ordinal, value) in fork_entries.iter().skip(1).take(2).enumerate() {
        parent.insert(&entry_of(value, ordinal as u64));
    }
    // New entry ids and no confirmed response ids: a copy cannot be told from new
    // work, so nothing is charged.
    let remapped = vec![
        fx::header("f3", Some("/nowhere/parent.jsonl")),
        fx::message(
            "r1",
            None,
            "assistant",
            "same answer",
            Some(fx::usage(100, 10, 0, 0)),
        ),
        fx::message(
            "r2",
            Some("r1"),
            "assistant",
            "second answer",
            Some(fx::usage(50, 5, 0, 0)),
        ),
    ];
    let mut fork = PiTree::new(1000, 1 << 20);
    let link = SessionLink::Resolved(SessionKey {
        harness: Harness::Pi,
        native_id: "p1".into(),
        source: SourceKey::File(PathBuf::from("parent.jsonl")),
    });
    fork.set_fork(Some(&link));
    assert!(fork.adopt(&parent));
    for (ordinal, value) in remapped.iter().enumerate().skip(1) {
        fork.insert(&entry_of(value, ordinal as u64));
    }
    assert_eq!(fork.ownership("r1"), PiOwnership::Unknown);
    assert_eq!(fork.ownership("r2"), PiOwnership::Unknown);

    // The same fixture through hydration: no spend charged, coverage partial.
    let root = tempfile::tempdir().expect("root");
    let parent_path = fx::write_session(root.path(), "proj", "parent", &fork_entries[..3]);
    let mut written = remapped;
    written[0] = fx::header("f3", Some(&parent_path.to_string_lossy()));
    fx::write_session(root.path(), "proj", "fork", &written);
    let config = config_with(root.path(), prices_with_window(None));
    let found = discover(root.path());
    let meta = find(&found, "f3");
    let (_hydration, summary) = hydrated(meta, &config);
    assert_eq!(usage_of(&summary).coverage.observed_requests, 0);
    assert!(!usage_of(&summary).coverage.complete);
}

#[test]
fn incomplete_predecessor_evidence_never_becomes_owned() {
    let parent_value = fx::with_response(
        fx::message(
            "a1",
            None,
            "assistant",
            "same answer",
            Some(fx::usage(100, 10, 0, 0)),
        ),
        "resp-1",
    );
    let mut parent = PiTree::new(1000, 1 << 20);
    parent.insert(&entry_of(&parent_value, 0));

    let mut fork = PiTree::new(1000, 1 << 20);
    fork.set_fork(Some(&SessionLink::Resolved(SessionKey {
        harness: Harness::Pi,
        native_id: "p1".into(),
        source: SourceKey::File(PathBuf::from("parent.jsonl")),
    })));
    assert!(fork.adopt(&parent));
    // A later unreadable parent record could hide a copied call.
    fork.mark_inherited_incomplete();
    fork.insert(&entry_of(&parent_value, 1));
    fork.insert(&entry_of(
        &fx::with_response(
            fx::message(
                "a2",
                Some("a1"),
                "assistant",
                "later",
                Some(fx::usage(90, 9, 0, 0)),
            ),
            "resp-2",
        ),
        2,
    ));
    assert_eq!(fork.ownership("a1"), PiOwnership::Inherited);
    assert_eq!(
        fork.ownership("a2"),
        PiOwnership::Unknown,
        "an unread predecessor record keeps a new call unattributable"
    );

    // End to end: the fork's own request is not charged while the parent read is
    // partial, and coverage says so.
    let root = tempfile::tempdir().expect("root");
    let mut parent_body = Vec::new();
    parent_body.extend_from_slice(fx::header("p2", None).to_string().as_bytes());
    parent_body.push(b'\n');
    parent_body.extend_from_slice(b"{\"type\":\"message\",\"id\":\"torn\"\n");
    parent_body.extend_from_slice(
        fx::with_response(
            fx::message(
                "a1",
                None,
                "assistant",
                "same answer",
                Some(fx::usage(100, 10, 0, 0)),
            ),
            "resp-1",
        )
        .to_string()
        .as_bytes(),
    );
    parent_body.push(b'\n');
    let parent_path = fx::write_raw(root.path(), "proj", "parent", &parent_body);
    let fork = vec![
        fx::header("f4", Some(&parent_path.to_string_lossy())),
        fx::message("u1", None, "user", "continue", None),
        fx::with_response(
            fx::message(
                "a2",
                Some("u1"),
                "assistant",
                "own answer",
                Some(fx::usage(90, 9, 0, 0)),
            ),
            "resp-2",
        ),
    ];
    fx::write_session(root.path(), "proj", "fork", &fork);
    let config = config_with(root.path(), prices_with_window(None));
    let found = discover(root.path());
    let meta = find(&found, "f4");
    let (_hydration, summary) = hydrated(meta, &config);
    assert_eq!(usage_of(&summary).coverage.observed_requests, 0);
    assert!(!usage_of(&summary).coverage.complete);
}

#[test]
fn missing_parent_and_cycle_are_reported_not_repaired() {
    let missing = vec![
        fx::header("s3", None),
        fx::message(
            "a1",
            Some("gone"),
            "assistant",
            "orphan",
            Some(fx::usage(10, 1, 0, 0)),
        ),
    ];
    let tree = tree_of(&missing);
    assert_eq!(tree.active_path(), Err(PiTreeError::MissingParent));

    let cycle = vec![
        fx::header("s4", None),
        fx::message("a1", Some("a2"), "assistant", "x", None),
        fx::message("a2", Some("a1"), "assistant", "y", None),
    ];
    let tree = tree_of(&cycle);
    assert_eq!(tree.active_path(), Err(PiTreeError::Cycle));
}

#[test]
fn bounded_tree_and_ledger_limits_are_reported() {
    let entries = abandoned_branch();
    let mut tree = PiTree::new(2, 64 << 20);
    for (ordinal, value) in entries.iter().enumerate().skip(1) {
        let insert = tree.insert(&entry_of(value, ordinal as u64));
        if ordinal < 3 {
            assert_eq!(insert, PiInsert::Inserted);
        } else {
            assert_eq!(insert, PiInsert::Limited);
        }
    }
    assert!(tree.is_limited());
    assert_eq!(tree.active_path(), Err(PiTreeError::Limited));

    // The hydration uses the same bounded index size.
    let dir = tempfile::tempdir().expect("tempdir");
    fx::write_session(dir.path(), "proj", "session", &entries);
    let mut config = config_with(dir.path(), prices_with_window(None));
    config.dedup_ledger_entries = 2;
    let found = discover(dir.path());
    let (_hydration, summary) = hydrated(&found.metadata[0], &config);
    assert_eq!(summary.metrics.context.used, None);
    assert!(!usage_of(&summary).coverage.complete);
    assert!(usage_of(&summary).coverage.observed_requests < 3);
}

#[test]
fn a_deep_recorded_chain_resolves_without_recursion() {
    let mut entries = vec![fx::header("s5", None)];
    let mut parent: Option<String> = None;
    for index in 0..500 {
        let id = format!("e{index}");
        entries.push(fx::message(&id, parent.as_deref(), "user", "step", None));
        parent = Some(id);
    }
    let tree = tree_of(&entries);
    assert_eq!(tree.active_path().expect("active path").len(), 500);
    assert_eq!(tree.fold_context(&prices_with_window(None)).used, None);
}

#[test]
fn a_session_without_a_declared_parent_owns_every_call() {
    let entries = abandoned_branch();
    let (_dir, meta, config) = one_session(&entries, prices_with_window(None));
    assert!(meta.lineage.fork.is_none());
    assert_eq!(meta.lineage, Lineage::default());
    let mut tree = tree_of(&entries);
    tree.set_fork(meta.lineage.fork.as_ref());
    assert_eq!(tree.ownership("a1"), PiOwnership::Owned);
    let (_hydration, summary) = hydrated(&meta, &config);
    assert_eq!(usage_of(&summary).coverage.observed_requests, 3);
}

#[test]
fn warnings_never_carry_record_text() {
    let entries = abandoned_branch();
    let (_dir, meta, config) = one_session(&entries, prices_with_window(Some(1000)));
    let (_hydration, summary) = hydrated(&meta, &config);
    for warning in &summary.warnings {
        assert_eq!(warning.key.harness, Harness::Pi);
        assert!(!warning.message.contains('\u{1b}'));
        assert!(!warning.message.contains("same answer"));
    }
    assert!(!format!("{:?}", summary.metrics.compactions).contains("same answer"));
    assert_eq!(
        summary
            .warnings
            .iter()
            .filter(|warning| warning.key.kind == WarningKind::Parse)
            .count(),
        0
    );
}

/// The parent records `a1` and the beginning of `a2`, then is truncated: the fork
/// holds the complete copy of `a2` the parent no longer persists.
#[test]
fn a_truncated_parent_tail_cannot_prove_ownership() {
    let root = tempfile::tempdir().expect("root");
    let copied = fx::with_response(
        fx::message(
            "a1",
            None,
            "assistant",
            "copied",
            Some(fx::usage(100, 10, 0, 0)),
        ),
        "resp-1",
    );
    let later = fx::with_response(
        fx::message(
            "a2",
            Some("a1"),
            "assistant",
            "copied too",
            Some(fx::usage(90, 9, 0, 0)),
        ),
        "resp-2",
    );
    let mut body = Vec::new();
    body.extend_from_slice(fx::header("p5", None).to_string().as_bytes());
    body.push(b'\n');
    body.extend_from_slice(copied.to_string().as_bytes());
    body.push(b'\n');
    // No trailing newline: the parent recorder ended inside `a2`.
    body.extend_from_slice(b"{\"type\":\"message\",\"id\":\"a2\",\"parentId\":\"a1\"");
    let parent_path = fx::write_raw(root.path(), "proj", "parent", &body);
    let fork = vec![
        fx::header("f5", Some(&parent_path.to_string_lossy())),
        copied,
        later,
    ];
    fx::write_session(root.path(), "proj", "fork", &fork);

    let config = config_with(root.path(), prices_with_window(None));
    let found = discover(root.path());
    let meta = find(&found, "f5");
    let (_hydration, summary) = hydrated(meta, &config);
    assert_eq!(
        usage_of(&summary).coverage.observed_requests,
        0,
        "an unfinished parent tail could hide the copied request"
    );
    assert!(!usage_of(&summary).coverage.complete);
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning.key.source.ends_with("#fork")),
        "the incomplete parent evidence is warned: {:?}",
        summary.warnings
    );
}

#[test]
fn an_empty_replaced_or_mismatched_parent_header_cannot_prove_ownership() {
    let call = |id: &str, parent: Option<&str>, response: &str, tokens: (u64, u64)| {
        fx::with_response(
            fx::message(
                id,
                parent,
                "assistant",
                "copied",
                Some(fx::usage(tokens.0, tokens.1, 0, 0)),
            ),
            response,
        )
    };

    // 1. An empty parent file proves nothing.
    let root = tempfile::tempdir().expect("root");
    let parent_path = fx::write_raw(root.path(), "proj", "parent", b"");
    let fork = vec![
        fx::header("f6", Some(&parent_path.to_string_lossy())),
        call("a1", None, "resp-1", (100, 10)),
    ];
    fx::write_session(root.path(), "proj", "fork", &fork);
    let config = config_with(root.path(), prices_with_window(None));
    let found = discover(root.path());
    let (_hydration, summary) = hydrated(find(&found, "f6"), &config);
    assert_eq!(usage_of(&summary).coverage.observed_requests, 0);
    assert!(!usage_of(&summary).coverage.complete);

    // 2. A parent replaced after discovery by another session, or by an unsupported
    // header version, is no longer the resolved predecessor.
    let mut body = Vec::new();
    body.extend_from_slice(fx::header("p8", None).to_string().as_bytes());
    body.push(b'\n');
    body.extend_from_slice(call("a1", None, "resp-1", (100, 10)).to_string().as_bytes());
    body.push(b'\n');
    for (id, replacement) in [
        (
            "f7",
            format!(
                "{}\n{}\n",
                fx::header("someone-else", None),
                call("a1", None, "resp-1", (100, 10))
            ),
        ),
        (
            "f7v",
            format!(
                "{}\n{}\n",
                fx::header_version("p8", 0),
                call("a1", None, "resp-1", (100, 10))
            ),
        ),
    ] {
        let root = tempfile::tempdir().expect("root");
        let parent_path = fx::write_raw(root.path(), "proj", "parent", &body);
        let fork = vec![
            fx::header(id, Some(&parent_path.to_string_lossy())),
            call("a1", None, "resp-1", (100, 10)),
            call("a2", Some("a1"), "resp-2", (50, 5)),
        ];
        fx::write_session(root.path(), "proj", "fork", &fork);
        let config = config_with(root.path(), prices_with_window(None));
        let found = discover(root.path());
        let meta = find(&found, id).clone();
        assert!(matches!(meta.lineage.fork, Some(SessionLink::Resolved(_))));
        std::fs::write(&parent_path, replacement).expect("replace parent");
        let (_hydration, summary) = hydrated(&meta, &config);
        assert_eq!(
            usage_of(&summary).coverage.observed_requests,
            0,
            "{id}: an unverified predecessor header is not evidence"
        );
        assert!(!usage_of(&summary).coverage.complete);
    }

    // 3. A parent replaced after discovery by an empty file keeps only unknown
    // ownership.
    let root = tempfile::tempdir().expect("root");
    let mut body = Vec::new();
    body.extend_from_slice(fx::header("p7", None).to_string().as_bytes());
    body.push(b'\n');
    body.extend_from_slice(call("a1", None, "resp-1", (100, 10)).to_string().as_bytes());
    body.push(b'\n');
    let parent_path = fx::write_raw(root.path(), "proj", "parent", &body);
    let fork = vec![
        fx::header("f8", Some(&parent_path.to_string_lossy())),
        call("a1", None, "resp-1", (100, 10)),
        call("a2", Some("a1"), "resp-2", (50, 5)),
    ];
    fx::write_session(root.path(), "proj", "fork", &fork);
    let config = config_with(root.path(), prices_with_window(None));
    let found = discover(root.path());
    let meta = find(&found, "f8").clone();
    assert!(matches!(meta.lineage.fork, Some(SessionLink::Resolved(_))));
    // The predecessor is replaced by an empty file between discovery and hydration.
    std::fs::write(&parent_path, b"").expect("replace parent");
    let (_hydration, summary) = hydrated(&meta, &config);
    assert_eq!(usage_of(&summary).coverage.observed_requests, 0);
    assert!(!usage_of(&summary).coverage.complete);
}

#[test]
fn a_flood_of_compaction_markers_is_bounded_and_reported_incomplete() {
    let mut entries = abandoned_branch();
    for index in 0..600 {
        let parent = if index == 0 {
            "a3".to_string()
        } else {
            format!("k{}", index - 1)
        };
        entries.push(fx::compaction(&format!("k{index}"), &parent, None));
    }
    let (_dir, meta, config) = one_session(&entries, prices_with_window(None));
    let (_hydration, summary) = hydrated(&meta, &config);
    let compactions = &summary.metrics.compactions;
    assert!(
        compactions.observed <= 512,
        "retained markers are bounded: {}",
        compactions.observed
    );
    assert!(
        compactions.observed < 600,
        "the flood is not fully retained"
    );
    assert!(
        !compactions.history_complete,
        "omitted markers make the history incomplete"
    );
    assert_eq!(compactions.known_cost_usd, None);
    assert_eq!(
        usage_of(&summary).total,
        Some(1230),
        "usage-less markers never change lifetime tokens"
    );
}

#[test]
fn short_id_and_response_heavy_entries_are_charged_their_structures() {
    let call = |id: &str, response: &str| {
        fx::with_response(
            fx::message(id, None, "assistant", "x", Some(fx::usage(1, 1, 0, 0))),
            response,
        )
    };

    // A one-byte id and response would cost 67 bytes under string-only accounting.
    let mut unbounded = PiTree::new(1000, 1 << 20);
    assert_eq!(
        unbounded.insert(&entry_of(&call("a", "r"), 0)),
        PiInsert::Inserted
    );
    let per_entry = unbounded.retained_bytes();
    assert!(
        per_entry > 160,
        "inline node, slot and key are charged: {per_entry}"
    );

    let mut tight = PiTree::new(1000, per_entry + per_entry / 2);
    assert_eq!(
        tight.insert(&entry_of(&call("a", "r"), 0)),
        PiInsert::Inserted
    );
    assert_eq!(
        tight.insert(&entry_of(&call("b", "s"), 1)),
        PiInsert::Limited,
        "the second short entry does not fit"
    );
    assert!(tight.is_limited());

    // Inherited evidence charges the same structures plus the duplicated response.
    let mut parent = PiTree::new(1000, 1 << 20);
    for index in 0..5u64 {
        assert_eq!(
            parent.insert(&entry_of(
                &call(&format!("s{index}"), &format!("r{index}")),
                index
            )),
            PiInsert::Inserted
        );
    }
    let link = SessionLink::Resolved(SessionKey {
        harness: Harness::Pi,
        native_id: "p1".into(),
        source: SourceKey::File(PathBuf::from("parent.jsonl")),
    });
    let mut fork = PiTree::new(1000, 200);
    fork.set_fork(Some(&link));
    assert!(
        !fork.adopt(&parent),
        "evidence beyond the byte budget is not adopted as complete"
    );
    for index in 0..5u64 {
        fork.insert(&entry_of(
            &call(&format!("s{index}"), &format!("r{index}")),
            index,
        ));
    }
    fork.insert(&entry_of(&call("n1", "r9"), 5));
    assert_eq!(
        fork.ownership("n1"),
        PiOwnership::Unknown,
        "partial evidence never becomes owned"
    );

    let mut generous = PiTree::new(1000, 1 << 20);
    generous.set_fork(Some(&link));
    assert!(generous.adopt(&parent));
    // A proven copied prefix (the producer copies in order) bounds the new work.
    generous.insert(&entry_of(&call("s0", "r0"), 0));
    assert_eq!(generous.ownership("s0"), PiOwnership::Inherited);
    generous.insert(&entry_of(&call("n1", "r9"), 1));
    assert_eq!(generous.ownership("n1"), PiOwnership::Owned);
}

#[test]
fn long_provenance_paths_are_charged_to_the_compaction_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A long (but legal) bucket/file spelling makes every marker own a long session
    // path three times: its source, its locator session and its locator kind.
    let bucket = "b".repeat(200);
    let name = "s".repeat(100);
    let mut entries = abandoned_branch();
    for index in 0..300 {
        let parent = if index == 0 {
            "a3".to_string()
        } else {
            format!("k{}", index - 1)
        };
        entries.push(fx::compaction(&format!("k{index}"), &parent, None));
    }
    fx::write_session(dir.path(), &bucket, &name, &entries);
    let config = config_with(dir.path(), prices_with_window(None));
    let found = discover(dir.path());
    let (hydration, summary) = hydrated(&found.metadata[0], &config);

    assert!(
        hydration.retained_compaction_bytes() <= 512 * 1024,
        "the charged bytes stay inside the byte ceiling: {}",
        hydration.retained_compaction_bytes()
    );
    assert!(
        hydration.retained_compaction_bytes() > 512 * 1024 / 4,
        "long provenance paths are actually charged: {}",
        hydration.retained_compaction_bytes()
    );
    assert!(
        hydration.omitted_compactions() > 0,
        "the byte ceiling evicted markers before the count cap"
    );
    assert_eq!(
        summary.metrics.compactions.observed,
        hydration.retained_compactions(),
        "only retained markers are reported as observed"
    );
    assert!(
        summary.metrics.compactions.observed < 300,
        "the 300-marker flood is not fully retained: {}",
        summary.metrics.compactions.observed
    );
    assert!(
        !summary.metrics.compactions.history_complete,
        "an evicted marker makes the history incomplete"
    );
}

#[test]
fn usage_bearing_markers_are_not_double_charged_the_nested_structure() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bucket = "b".repeat(100);
    let summary_usage = fx::usage(4, 2, 0, 0);
    let markers = |header_id: &str, count: usize, usage: Option<Value>| {
        let mut entries = abandoned_branch();
        entries[0] = fx::header(header_id, None);
        for index in 0..count {
            let parent = if index == 0 {
                "a3".to_string()
            } else {
                format!("k{:04}", index - 1)
            };
            entries.push(fx::compaction(
                &format!("k{index:04}"),
                &parent,
                usage.clone(),
            ));
        }
        entries
    };
    let config = config_with(dir.path(), prices_with_window(None));

    // One usage-less marker of the same shape charges every structure but no nested
    // usage strings.
    fx::write_session(dir.path(), &bucket, "ccc", &markers("s0", 1, None));
    let found = discover(dir.path());
    let (plain, _) = hydrated(find(&found, "s0"), &config);
    assert_eq!(plain.retained_compactions(), 1);
    let per_without = plain.retained_compaction_bytes();
    assert!(per_without > 0);

    // The only extra retention of a usage-bearing marker is the nested observation's
    // owned strings: its provenance (source, locator session, locator kind), the
    // session id, record type, harness version and the request id. Its inline
    // structure is already inside the marker's own charge.
    let path_len = match &find(&found, "s0").key.source {
        SourceKey::File(path) => path.as_os_str().len(),
        other => panic!("file source: {other:?}"),
    };
    let nested_strings =
        3 * path_len + "s0".len() + "compaction".len() + "v3".len() + "k0000".len();

    fx::write_session(
        dir.path(),
        &bucket,
        "ddd",
        &markers("s1", 10, Some(summary_usage.clone())),
    );
    let found = discover(dir.path());
    let (bearing, bearing_summary) = hydrated(find(&found, "s1"), &config);
    assert_eq!(bearing.retained_compactions(), 10);
    assert_eq!(bearing.omitted_compactions(), 0);
    assert!(
        bearing_summary
            .metrics
            .compactions
            .latest
            .as_ref()
            .expect("latest")
            .usage
            .is_some(),
        "the markers carry summarization usage"
    );
    assert_eq!(
        bearing.retained_compaction_bytes() - 10 * per_without,
        10 * nested_strings,
        "only the nested owned strings are charged, not its inline structure again"
    );

    // Fill the byte ceiling with the correct per-marker cost: nothing is evicted.
    let cap = 512 * 1024;
    let correct_per = per_without + nested_strings;
    let fit = cap / correct_per;
    assert!(fit > 50, "the cost admits a real flood: {fit}");
    assert!(
        fit < 512,
        "the byte ceiling governs, not the count cap: {fit}"
    );
    fx::write_session(
        dir.path(),
        &bucket,
        "eee",
        &markers("s2", fit, Some(summary_usage)),
    );
    let found = discover(dir.path());
    let (bulk, bulk_summary) = hydrated(find(&found, "s2"), &config);
    assert_eq!(
        bulk.omitted_compactions(),
        0,
        "no premature eviction by a double-charged nested structure"
    );
    assert_eq!(bulk.retained_compactions(), fit);
    assert_eq!(bulk_summary.metrics.compactions.observed, fit);
    assert!(bulk.retained_compaction_bytes() <= cap);
    assert!(
        bulk_summary.metrics.compactions.history_complete,
        "the whole recorded history is retained"
    );
}
