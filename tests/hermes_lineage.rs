//! Hermes logical chains: compression continuations, delegates, branches, resets,
//! inherited markers, unresolved/cyclic edges and per-database identity.

mod support;

use std::path::Path;

use agentscope::domain::{RefreshBudget, SessionKey, SessionLink, SourceKey};
use agentscope::providers::hermes::{
    HermesEdge, HermesHydration, HermesMarkers, HermesRowMeta, HermesUnresolved, classify_edge,
    resolve_lineage,
};
use agentscope::providers::{HydrationDelta, ReadJobId};
use support::NOW;
use support::hermes::*;
use tempfile::TempDir;

const TINY: RefreshBudget = RefreshBudget {
    max_records: 1,
    max_bytes: 64 * 1024,
    max_millis: 60_000,
};

fn key(db: &Path, id: &str) -> SessionKey {
    SessionKey {
        harness: agentscope::domain::Harness::Hermes,
        native_id: id.to_owned(),
        source: SourceKey::DbRow {
            database: db.to_path_buf(),
            table: "sessions".to_owned(),
            id: id.to_owned(),
        },
    }
}

fn row(db: &Path, id: &str) -> HermesRowMeta {
    HermesRowMeta::new(db, id)
}

fn markers(delegate: Option<&str>, branch: Option<&str>, reset: Option<&str>) -> HermesMarkers {
    HermesMarkers {
        delegate_from: delegate.map(str::to_owned),
        branched_from: branch.map(str::to_owned),
        reset_from: reset.map(str::to_owned),
    }
}

fn hydrate(db: &Path, id: &str, budget: RefreshBudget) -> HydrationDelta {
    let prices = agentscope::pricing::PriceSchedule::bundled().expect("bundled prices");
    let mut hydration = HermesHydration::new(db, prices);
    for attempt in 0..10_000 {
        let delta = hydration
            .step(ReadJobId(1), &key(db, id), NOW, budget)
            .expect("hydration slice");
        if delta.done {
            return delta;
        }
        assert!(attempt < 9_999, "hydration never completed");
    }
    unreachable!()
}

fn tmp() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn compression_delegates_and_nested_children_form_logical_sessions() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let rows = vec![
        row(&db, "p").ending(Some(20_000), Some("compression")),
        row(&db, "p2").with_parent(Some("p")),
        row(&db, "c")
            .with_parent(Some("p"))
            .with_markers(markers(Some("p"), None, None))
            .ending(Some(25_000), Some("compression")),
        // Inherits the parent's `_delegate_from=p`; the actual parent is `c`.
        row(&db, "c2")
            .with_parent(Some("c"))
            .with_markers(markers(Some("p"), None, None))
            .ending(Some(30_000), Some("compression")),
        row(&db, "g")
            .with_parent(Some("c2"))
            .with_markers(markers(Some("c2"), None, None)),
        // A delegate can attach to any member of the parent's chain.
        row(&db, "tipchild")
            .with_parent(Some("p2"))
            .with_markers(markers(Some("p2"), None, None)),
        row(&db, "branch")
            .with_parent(Some("p"))
            .with_markers(markers(None, Some("p"), None)),
        row(&db, "reset")
            .with_parent(Some("p"))
            .with_markers(markers(None, None, Some("p"))),
        row(&db, "other"),
    ];
    assert_eq!(
        classify_edge(&rows[3], Some(&rows[2])),
        HermesEdge::Compression
    );
    assert_eq!(
        classify_edge(&rows[2], Some(&rows[0])),
        HermesEdge::Delegate
    );

    let lineage = resolve_lineage(&rows);
    let parent = lineage.for_key(&key(&db, "p2")).expect("parent chain");
    assert_eq!(parent.key, key(&db, "p"));
    assert_eq!(
        parent
            .members
            .iter()
            .map(|member| member.native_id.as_str())
            .collect::<Vec<_>>(),
        ["p", "p2"]
    );

    let child = lineage.for_key(&key(&db, "c2")).expect("child chain");
    assert_eq!(child.key, key(&db, "c"));
    assert_eq!(
        child
            .members
            .iter()
            .map(|member| member.native_id.as_str())
            .collect::<Vec<_>>(),
        ["c", "c2"]
    );
    assert_eq!(
        child.relation.parent,
        Some(SessionLink::Resolved(parent.key.clone()))
    );
    assert!(!child.relation.legacy);

    let grandchild = lineage.for_key(&key(&db, "g")).expect("grandchild");
    assert_eq!(grandchild.key, key(&db, "g"));
    assert_eq!(
        grandchild.relation.parent,
        Some(SessionLink::Resolved(child.key.clone()))
    );

    let tipchild = lineage
        .for_key(&key(&db, "tipchild"))
        .expect("delegate from a compression tip");
    assert_eq!(
        tipchild.relation.parent,
        Some(SessionLink::Resolved(parent.key.clone()))
    );

    let branch = lineage.for_key(&key(&db, "branch")).expect("branch");
    assert_eq!(branch.key, key(&db, "branch"));
    assert_eq!(branch.relation.parent, None);
    assert_eq!(
        branch.relation.fork,
        Some(SessionLink::Resolved(parent.key.clone()))
    );

    let reset = lineage.for_key(&key(&db, "reset")).expect("reset");
    assert_eq!(reset.key, key(&db, "reset"));
    assert_eq!(
        reset.relation.fork,
        Some(SessionLink::Resolved(parent.key.clone()))
    );

    // A fork marker equal to the parent is a fork; a marker naming another row is
    // inherited and must not split a compression continuation.
    assert_eq!(classify_edge(&rows[6], Some(&rows[0])), HermesEdge::Branch);
    assert_eq!(classify_edge(&rows[7], Some(&rows[0])), HermesEdge::Reset);
    let inherited = row(&db, "c2")
        .with_parent(Some("c"))
        .with_markers(markers(Some("p"), None, None))
        .ending(Some(30_000), Some("compression"));
    assert_eq!(
        classify_edge(&inherited, Some(&rows[2])),
        HermesEdge::Compression
    );
    assert_eq!(lineage.sessions.len(), 7);
}

#[test]
fn missing_ambiguous_and_cyclic_edges_stay_unresolved_roots() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let rows = vec![
        row(&db, "ghost-child").with_parent(Some("ghost")),
        row(&db, "mismatch")
            .with_parent(Some("p"))
            .with_markers(markers(Some("somewhere-else"), None, None)),
        row(&db, "p"),
        row(&db, "cycle-a").with_parent(Some("cycle-b")),
        row(&db, "cycle-b").with_parent(Some("cycle-a")),
        row(&db, "fan-parent").ending(Some(10_000), Some("compression")),
        row(&db, "fan-1").with_parent(Some("fan-parent")),
        row(&db, "fan-2").with_parent(Some("fan-parent")),
    ];
    let lineage = resolve_lineage(&rows);

    let ghost = lineage
        .for_key(&key(&db, "ghost-child"))
        .expect("missing parent root");
    assert_eq!(ghost.members, vec![key(&db, "ghost-child")]);
    assert_eq!(
        ghost.relation.unresolved,
        Some(HermesUnresolved::MissingParent("ghost".into()))
    );

    let mismatch = lineage
        .for_key(&key(&db, "mismatch"))
        .expect("mismatched marker");
    assert_eq!(mismatch.edge, HermesEdge::Unresolved);
    assert!(matches!(
        mismatch.relation.unresolved,
        Some(HermesUnresolved::MarkerMismatch(_))
    ));

    for id in ["cycle-a", "cycle-b"] {
        let cycle = lineage.for_key(&key(&db, id)).expect("cycle root");
        assert_eq!(cycle.members, vec![key(&db, id)]);
        assert_eq!(cycle.relation.unresolved, Some(HermesUnresolved::Cycle));
    }

    for id in ["fan-1", "fan-2"] {
        let fan = lineage.for_key(&key(&db, id)).expect("fan root");
        assert_eq!(fan.members, vec![key(&db, id)]);
        assert_eq!(
            fan.relation.unresolved,
            Some(HermesUnresolved::AmbiguousContinuation("fan-parent".into()))
        );
    }
    // The ambiguous children never become one guessed chain.
    assert_ne!(
        lineage.for_key(&key(&db, "fan-1")).unwrap().key,
        lineage.for_key(&key(&db, "fan-2")).unwrap().key
    );
}

#[test]
fn equal_native_ids_in_two_profile_databases_never_merge() {
    let dir = tmp();
    let first = dir.path().join("profiles").join("alpha").join("state.db");
    let second = dir.path().join("profiles").join("beta").join("state.db");
    let rows = vec![
        row(&first, "p"),
        row(&first, "c")
            .with_parent(Some("p"))
            .with_markers(markers(Some("p"), None, None)),
        row(&second, "p"),
        row(&second, "c")
            .with_parent(Some("p"))
            .with_markers(markers(Some("p"), None, None)),
    ];
    let lineage = resolve_lineage(&rows);
    assert_eq!(lineage.sessions.len(), 4);
    for db in [&first, &second] {
        let child = lineage.for_key(&key(db, "c")).expect("child per profile");
        assert_eq!(
            child.relation.parent,
            Some(SessionLink::Resolved(key(db, "p")))
        );
        assert_eq!(child.members, vec![key(db, "c")]);
    }
    assert_ne!(key(&first, "c").source, key(&second, "c").source);
}

#[test]
fn legacy_subagent_source_is_a_labeled_delegate_fallback() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let rows = vec![
        row(&db, "p"),
        row(&db, "legacy")
            .with_parent(Some("p"))
            .with_source("subagent"),
    ];
    let lineage = resolve_lineage(&rows);
    let legacy = lineage.for_key(&key(&db, "legacy")).expect("legacy child");
    assert_eq!(legacy.edge, HermesEdge::Delegate);
    assert!(legacy.relation.legacy);
    assert_eq!(
        legacy.relation.parent,
        Some(SessionLink::Resolved(key(&db, "p")))
    );
}

#[test]
fn hydrated_metadata_carries_stable_chain_keys_and_explicit_links() {
    let dir = tmp();
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(
        &writer,
        "p",
        None,
        serde_json::json!({"reasoning_config": {"enabled": true, "effort": "high"}}),
        1_000.0,
    );
    insert_session(
        &writer,
        "p2",
        Some("p"),
        serde_json::json!({"reasoning_config": {"enabled": true, "effort": "high"}}),
        1_100.0,
    );
    insert_session(
        &writer,
        "c",
        Some("p"),
        serde_json::json!({"_delegate_from": "p"}),
        1_200.0,
    );
    insert_session(
        &writer,
        "c2",
        Some("c"),
        serde_json::json!({"_delegate_from": "p", "reasoning_config": {"enabled": true, "effort": "high"}}),
        1_300.0,
    );
    insert_session(
        &writer,
        "g",
        Some("c2"),
        serde_json::json!({"_delegate_from": "c2"}),
        1_400.0,
    );
    insert_session(
        &writer,
        "branch",
        Some("p"),
        serde_json::json!({"_branched_from": "p"}),
        1_500.0,
    );
    insert_session(
        &writer,
        "reset",
        Some("p"),
        serde_json::json!({"_reset_from": "p"}),
        1_600.0,
    );
    set_ended(&writer, "p", 1_050.0, "compression");
    set_ended(&writer, "c", 1_250.0, "compression");

    let parent = hydrate(&db, "p2", TINY).summary;
    assert_eq!(parent.meta.key.native_id, "p");
    assert_eq!(
        parent
            .meta
            .members
            .iter()
            .map(|member| member.native_id.as_str())
            .collect::<Vec<_>>(),
        ["p", "p2"]
    );

    let child = hydrate(&db, "c2", TINY).summary;
    assert_eq!(child.meta.key.native_id, "c");
    assert_eq!(
        child.meta.lineage.parent,
        Some(SessionLink::Resolved(parent.meta.key.clone()))
    );
    assert_eq!(
        child
            .meta
            .members
            .iter()
            .map(|member| member.native_id.as_str())
            .collect::<Vec<_>>(),
        ["c", "c2"]
    );
    assert_eq!(
        child
            .meta
            .model
            .as_ref()
            .and_then(|model| model.configured_effort.as_deref()),
        Some("high")
    );

    let grandchild = hydrate(&db, "g", TINY).summary;
    assert_eq!(grandchild.meta.key.native_id, "g");
    assert_eq!(
        grandchild.meta.lineage.parent,
        Some(SessionLink::Resolved(child.meta.key.clone()))
    );

    let branch = hydrate(&db, "branch", TINY).summary;
    assert_eq!(branch.meta.key.native_id, "branch");
    assert_eq!(
        branch.meta.lineage.fork,
        Some(SessionLink::Resolved(parent.meta.key.clone()))
    );
    let reset = hydrate(&db, "reset", TINY).summary;
    assert_eq!(
        reset.meta.lineage.fork,
        Some(SessionLink::Resolved(parent.meta.key.clone()))
    );
    assert_ne!(branch.meta.key, parent.meta.key);
    assert_ne!(reset.meta.key, parent.meta.key);
}
