//! Native child discovery for Claude Code: `subagents/**/agent-*.jsonl` files
//! whose records keep the parent's `sessionId`, identified by their own
//! `agentId`, with journal decoys, exact parent edges and owned-only usage.

mod support;

use std::path::Path;

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{Harness, RefreshBudget, SessionKey, SessionLink, SourceKey};
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::{ProviderReader, ReadJobId};
use serde_json::json;
use support::NOW;
use support::claude as fx;
use support::codex as cx;
use support::desktop;
use support::world::{BIG, discover_pass};

const MAIN: &str = "demo";

fn config(root: &Path) -> Config {
    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root.to_path_buf()),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    config.process_enabled = false;
    config
}

fn key(id: &str, path: &Path) -> SessionKey {
    SessionKey {
        harness: Harness::Claude,
        native_id: id.to_owned(),
        source: SourceKey::File(path.to_path_buf()),
    }
}

/// The synthetic family: a main session, one child, a nested child and decoys.
struct Family {
    root: std::path::PathBuf,
    project: std::path::PathBuf,
    main: std::path::PathBuf,
    child: std::path::PathBuf,
    nested: std::path::PathBuf,
    journal: std::path::PathBuf,
    tool_results: std::path::PathBuf,
}

fn family(base: &Path) -> Family {
    let root = base.join("projects");
    let project = root.join("demo-project");
    let main = project.join("demo.jsonl");
    let child = project.join("demo").join("subagents").join("agent-a.jsonl");
    let nested = project
        .join("demo")
        .join("subagents")
        .join("workflows")
        .join("run-1")
        .join("agent-b.jsonl");
    let journal = project
        .join("demo")
        .join("subagents")
        .join("workflows")
        .join("run-1")
        .join("journal.jsonl");
    let tool_results = project
        .join("demo")
        .join("subagents")
        .join("agent-a.tool-results")
        .join("result.jsonl");
    fx::write(
        &main,
        &[
            // A message UUID that is not a session; it never becomes an edge.
            fx::parent_uuid(fx::user(MAIN, "u0", "Refactor the parser"), "no-session"),
            fx::assistant(MAIN, "u1", "m1", (100, 50)),
            fx::assistant(MAIN, "u2", "m2", (10, 5)),
            // A sidechain record inside the main transcript: never parent usage.
            fx::agent(fx::assistant(MAIN, "u3", "side", (900, 900)), "a"),
        ],
    );
    fx::write(
        &child,
        &[
            fx::user(MAIN, "u0", "Refactor the parser"),
            // Inherited history: the same message ID the parent recorded, with no
            // agent of its own.
            fx::assistant(MAIN, "u1", "m1", (100, 50)),
            fx::agent(fx::assistant(MAIN, "a1", "c1", (20, 4)), "a"),
        ],
    );
    fx::write_sidecar(&child, json!({"agentId": "a", "agentType": "Explore"}));
    fx::write(
        &nested,
        &[
            fx::spawned_by(
                fx::agent(fx::assistant(MAIN, "b1", "c2", (5, 1)), "b"),
                "a",
                "toolu_01",
            ),
            fx::agent(fx::user(MAIN, "b2", "review the diff"), "b"),
        ],
    );
    // Workflow junk: a journal and tool results are not sessions.
    fx::write(&journal, &[fx::user(MAIN, "j1", "journal entry")]);
    fx::write(&tool_results, &[fx::user(MAIN, "t1", "tool output")]);
    Family {
        root,
        project,
        main,
        child,
        nested,
        journal,
        tool_results,
    }
}

fn discover(scanner: &mut ClaudeScanner) -> Vec<agentscope::domain::SessionMetadata> {
    discover_pass(scanner, NOW, BIG).sessions
}

#[test]
fn claude_children_are_distinct_sessions_with_exact_parents() {
    let dir = tempfile::tempdir().unwrap();
    let f = family(dir.path());
    let mut scanner = ClaudeScanner::new(&config(&f.root)).unwrap();
    let found = discover(&mut scanner);

    let mut ids: Vec<&str> = found.iter().map(|m| m.key.native_id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        ["a", "b", MAIN],
        "journal and tool results are not sessions"
    );

    let main = found.iter().find(|m| m.key.native_id == MAIN).unwrap();
    let child = found.iter().find(|m| m.key.native_id == "a").unwrap();
    let nested = found.iter().find(|m| m.key.native_id == "b").unwrap();
    assert_ne!(main.key, child.key);
    assert_eq!(
        child.key,
        key("a", &f.child),
        "the child answers to its agent ID, not the shared session ID"
    );
    // `parentUuid` alone is a message link, not a session edge.
    assert_eq!(main.lineage.parent, None);
    // Inline sidechain records mark the transcript as carrying subagent work; they
    // never give the main session a parent edge or move its lifecycle.
    assert_eq!(main.lineage.agent_id, None);
    assert_eq!(
        child.lineage.parent,
        Some(SessionLink::Resolved(key(MAIN, &f.main)))
    );
    assert_eq!(
        nested.lineage.parent,
        Some(SessionLink::Resolved(key("a", &f.child)))
    );
    assert_eq!(nested.lineage.parent_agent_id.as_deref(), Some("a"));
    assert_eq!(nested.lineage.spawning_tool_id.as_deref(), Some("toolu_01"));
    assert_eq!(child.lineage.agent_id.as_deref(), Some("a"));
    assert_eq!(child.lineage.agent_type.as_deref(), Some("Explore"));
    // The shared `sessionId` is not a duplicate-ID conflict between a parent and
    // its own child.
    for meta in &found {
        let summary = support::world::hydrate_complete(&mut scanner, &meta.key, NOW);
        assert!(
            summary
                .warnings
                .iter()
                .all(|w| !w.key.source.contains("#duplicate-id")),
            "no duplicate-id warning within one family"
        );
    }
}

#[test]
fn claude_children_report_nested_layout_without_phantom_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let f = family(dir.path());
    let mut scanner = ClaudeScanner::new(&config(&f.root)).unwrap();
    let found = discover(&mut scanner);
    assert!(!found.iter().any(
        |m| matches!(&m.key.source, SourceKey::File(p) if p == &f.journal || p == &f.tool_results)
    ));
    for meta in &found {
        let expected = if meta.key.native_id == MAIN {
            // The main transcript records the working directory it ran in.
            "demo"
        } else {
            // A child repeats that cwd, so its project stays the session directory.
            "demo-project"
        };
        assert_eq!(meta.project.as_deref(), Some(expected));
    }
    // The child's transcript sits under the project's session directory; the nested
    // one sits in an evidenced workflow directory below `subagents`.
    assert!(f.child.starts_with(&f.project));
    assert!(
        f.nested
            .starts_with(f.project.join("demo").join("subagents").join("workflows")),
        "workflow nesting is walked"
    );
    assert!(found.iter().any(|m| matches!(
        &m.key.source,
        SourceKey::File(p) if p == &f.nested
    )));
}

#[test]
fn claude_parent_direct_usage_excludes_child_owned_usage() {
    let dir = tempfile::tempdir().unwrap();
    let f = family(dir.path());
    let mut scanner = ClaudeScanner::new(&config(&f.root)).unwrap();
    let found = discover(&mut scanner);

    let mut totals = std::collections::BTreeMap::new();
    for meta in &found {
        let summary = support::world::hydrate_complete(&mut scanner, &meta.key, NOW);
        let usage = summary.usage.expect("usage");
        let total = usage.total.expect("a token total");
        // Record the message IDs this session owns, for the family check.
        totals.insert(meta.key.native_id.clone(), total);
    }
    assert_eq!(
        totals["demo"], 165,
        "the parent counts only its own records"
    );
    assert_eq!(
        totals["a"], 24,
        "the child's inherited copy is not its usage"
    );
    assert_eq!(totals["b"], 6);

    let family_unique: u64 = totals.values().sum();
    assert_eq!(family_unique, 195);
    // The shared fork message ID `m1` (165 = m1 + m2 of the parent) is counted in
    // the parent only: the child's copy would otherwise add another 150.
    assert_ne!(family_unique, 345);
}

#[test]
fn claude_same_agent_id_under_different_parents_is_not_a_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    for (session, extra) in [("one", 1u64), ("two", 2u64)] {
        let main = root.join("proj").join(format!("{session}.jsonl"));
        fx::write(&main, &[fx::user(session, "u0", "go")]);
        let child = root
            .join("proj")
            .join(session)
            .join("subagents")
            .join("agent-a.jsonl");
        fx::write(
            &child,
            &[fx::agent(
                fx::assistant(session, "a1", "m", (10 * extra, 1)),
                "a",
            )],
        );
    }
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let found = discover(&mut scanner);
    assert_eq!(found.len(), 4);
    for meta in &found {
        let summary = support::world::hydrate_complete(&mut scanner, &meta.key, NOW);
        assert!(
            summary
                .warnings
                .iter()
                .all(|w| !w.key.source.contains("#duplicate-id")),
            "agent IDs are only unique inside their own session"
        );
    }
    let keys: Vec<SessionKey> = found.iter().map(|m| m.key.clone()).collect();
    for (i, a) in keys.iter().enumerate() {
        for b in &keys[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn claude_child_without_sidecar_takes_its_parent_span_from_records() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let main = root.join("proj").join("demo.jsonl");
    let child = root
        .join("proj")
        .join("demo")
        .join("subagents")
        .join("agent-x.jsonl");
    let nested = root
        .join("proj")
        .join("demo")
        .join("subagents")
        .join("agent-y.jsonl");
    fx::write(&main, &[fx::user("demo", "u0", "go")]);
    fx::write(
        &child,
        &[fx::agent(fx::assistant("demo", "x1", "mx", (7, 2)), "x")],
    );
    // No sidecar: the nested spawner comes from the child's own record.
    fx::write(
        &nested,
        &[fx::spawned_by(
            fx::agent(fx::assistant("demo", "y1", "my", (3, 1)), "y"),
            "x",
            "toolu_9",
        )],
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let found = discover(&mut scanner);
    let child_meta = found.iter().find(|m| m.key.native_id == "x").unwrap();
    let nested_meta = found.iter().find(|m| m.key.native_id == "y").unwrap();
    assert_eq!(
        child_meta.lineage.parent,
        Some(SessionLink::Resolved(key("demo", &main)))
    );
    assert_eq!(
        nested_meta.lineage.parent,
        Some(SessionLink::Resolved(key("x", &child)))
    );
}

#[test]
fn claude_child_transcript_hydrates_its_own_records() {
    let dir = tempfile::tempdir().unwrap();
    let f = family(dir.path());
    let mut scanner = ClaudeScanner::new(&config(&f.root)).unwrap();
    let found = discover(&mut scanner);
    let child_key = found
        .iter()
        .find(|m| m.key.native_id == "a")
        .unwrap()
        .key
        .clone();

    let job = ReadJobId(99_001);
    let mut done = false;
    for _ in 0..64 {
        let delta = scanner.hydrate(job, &child_key, NOW, BIG).unwrap();
        if delta.done {
            assert_eq!(delta.summary.meta.key, child_key);
            assert_eq!(delta.summary.usage.as_ref().and_then(|u| u.total), Some(24));
            assert_eq!(delta.summary.meta.lineage.agent_id.as_deref(), Some("a"));
            done = true;
            break;
        }
    }
    assert!(done, "child hydration finished within the bound");
    complete_open_jobs(&mut scanner, &child_key, job);
}

fn complete_open_jobs(scanner: &mut ClaudeScanner, key: &SessionKey, job: ReadJobId) {
    scanner.cancel_read(key, job);
}

/// A scan that reports a different budget than it used is still bounded per call.
#[test]
fn claude_child_discovery_is_bounded_per_call() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let main = root.join("proj").join("demo.jsonl");
    fx::write(&main, &[fx::user("demo", "u0", "go")]);
    for i in 0..20 {
        let child = root
            .join("proj")
            .join("demo")
            .join("subagents")
            .join(format!("agent-c{i}.jsonl"));
        fx::write(
            &child,
            &[fx::agent(
                fx::assistant("demo", &format!("u{i}"), &format!("m{i}"), (1, 1)),
                &format!("c{i}"),
            )],
        );
    }
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let slice = RefreshBudget {
        max_records: 4,
        max_bytes: 1 << 20,
        max_millis: 60_000,
    };
    let mut calls = 0;
    let mut sessions = 0;
    loop {
        let delta = scanner.discover(NOW, slice).unwrap();
        assert!(
            delta.sessions.len() + delta.warnings.len() <= 8,
            "a slice reports only the work it did"
        );
        sessions += delta.sessions.len();
        calls += 1;
        if delta.scan.done {
            break;
        }
        assert!(calls < 100, "discovery finished");
    }
    assert!(calls > 1, "a small budget still takes several slices");
    assert!(sessions >= 21);
}

#[test]
fn claude_scoped_parent_agent_never_crosses_families() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let proj = root.join("proj");
    // Family one: child `x` spawned by agent `y` that exists in this family.
    let a_main = proj.join("one.jsonl");
    let a_x = proj.join("one").join("subagents").join("agent-x.jsonl");
    let a_y = proj
        .join("one")
        .join("subagents")
        .join("workflows")
        .join("r1")
        .join("agent-y.jsonl");
    // Family two: the same agent IDs, but its `y` is not part of this family.
    let b_main = proj.join("two.jsonl");
    let b_x = proj.join("two").join("subagents").join("agent-x.jsonl");
    fx::write(&a_main, &[fx::user("one", "u0", "go")]);
    fx::write(
        &a_y,
        &[fx::agent(fx::assistant("one", "y1", "my", (5, 1)), "y")],
    );
    fx::write(
        &a_x,
        &[fx::spawned_by(
            fx::agent(fx::assistant("one", "x1", "mx", (4, 1)), "x"),
            "y",
            "toolu_a",
        )],
    );
    fx::write(&b_main, &[fx::user("two", "u0", "go")]);
    fx::write(
        &b_x,
        &[fx::spawned_by(
            fx::agent(fx::assistant("two", "x2", "mx", (3, 1)), "x"),
            "y",
            "toolu_b",
        )],
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let found = discover(&mut scanner);
    let a_child = found
        .iter()
        .find(|m| m.key.source == SourceKey::File(a_x.clone()))
        .unwrap();
    let b_child = found
        .iter()
        .find(|m| m.key.source == SourceKey::File(b_x.clone()))
        .unwrap();
    assert_eq!(
        a_child.lineage.parent,
        Some(SessionLink::Resolved(key("y", &a_y))),
        "the spawner is exact inside its own family"
    );
    // Family two has no such spawner: the link stays scoped and unresolvable rather
    // than attaching to family one's `y`.
    match &b_child.lineage.parent {
        Some(SessionLink::Resolved(other)) => {
            panic!("cross-family attach: {other:?}");
        }
        Some(SessionLink::Unresolved(scoped)) => {
            assert!(
                scoped.starts_with("two\u{1f}"),
                "the owning family is preserved: {scoped:?}"
            );
        }
        Some(SessionLink::Ambiguous) | None => {}
    }
}

#[test]
fn claude_origins_come_only_from_owned_records() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let main = root.join("proj").join("demo.jsonl");
    let child = root
        .join("proj")
        .join("demo")
        .join("subagents")
        .join("agent-a.jsonl");
    // The parent's own record is CLI; an inline sidechain record claims Desktop.
    fx::write(
        &main,
        &[
            fx::entrypoint(fx::user("demo", "u0", "go"), "cli"),
            fx::entrypoint(
                fx::agent(fx::assistant("demo", "u1", "side", (5, 1)), "a"),
                "claude-desktop",
            ),
        ],
    );
    // The child inherits a Desktop-stamped record and owns a CLI one.
    fx::write(
        &child,
        &[
            fx::entrypoint(
                fx::assistant("demo", "h1", "inherited", (5, 1)),
                "claude-desktop",
            ),
            fx::entrypoint(
                fx::agent(fx::assistant("demo", "a1", "own", (2, 1)), "a"),
                "cli",
            ),
        ],
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let found = discover(&mut scanner);
    for meta in &found {
        let origins = &meta.origins;
        assert!(
            origins
                .iter()
                .any(|o| o.surface == agentscope::domain::OriginSurface::Cli),
            "the owned record labels the session"
        );
        assert!(
            origins
                .iter()
                .all(|o| o.surface != agentscope::domain::OriginSurface::DesktopCode),
            "an inherited or inline sidechain record does not label a session: {origins:?}"
        );
    }
}

#[test]
fn claude_eviction_keeps_duplicate_scope_bookkeeping_in_step() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let main = root.join("proj").join("demo.jsonl");
    fx::write(&main, &[fx::user("demo", "u0", "go")]);
    for id in ["a", "b", "c"] {
        let child = root
            .join("proj")
            .join("demo")
            .join("subagents")
            .join(format!("agent-{id}.jsonl"));
        fx::write(
            &child,
            &[fx::agent(
                fx::assistant("demo", "u1", &format!("m-{id}"), (1, 1)),
                id,
            )],
        );
    }
    let mut config = config(&root);
    config.max_metadata_sources = 2;
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    discover(&mut scanner);
    let tracked = scanner.tracked_sources();
    assert!(tracked <= 2, "the metadata ceiling is enforced: {tracked}");
    assert_eq!(
        scanner.duplicate_scopes(),
        tracked,
        "every evicted child leaves no duplicate-scope path behind"
    );
}

#[test]
fn claude_probe_applies_tail_records_in_source_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let file = root.join("proj").join("demo.jsonl");
    // Filler larger than the 4 KiB head window, so the model and title records are
    // only reachable through the tail; the newest of them must win.
    let mut records = Vec::new();
    for i in 0..12 {
        let text = format!("{i}: {}", "padding ".repeat(80));
        records.push(fx::user("demo", &format!("pad{i}"), &text));
    }
    records.push(fx::model_of(
        fx::assistant("demo", "m-old", "m1", (1, 1)),
        "claude-old",
    ));
    records.push(fx::model_of(
        fx::assistant("demo", "m-mid", "m2", (1, 1)),
        "claude-mid",
    ));
    records.push(fx::model_of(
        fx::assistant("demo", "m-new", "m3", (1, 1)),
        "claude-new",
    ));
    records.push(fx::ai_title("demo", "t-old", "old title"));
    records.push(fx::ai_title("demo", "t-new", "new title"));
    fx::write(&file, &records);
    let len = std::fs::metadata(&file).unwrap().len();
    assert!(
        len > 8 * 1024,
        "the transcript is larger than the head window"
    );

    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let found = discover(&mut scanner);
    let meta = &found[0];
    assert_eq!(
        meta.model.as_ref().map(|m| m.id.as_str()),
        Some("claude-new"),
        "the newest tail record is the last observation"
    );
    assert_eq!(meta.title, "new title");
}

#[test]
fn claude_scope_is_the_owning_source_not_the_session_directory_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    // Two projects use the same session-directory name `demo`. Only project A has
    // an agent `y`; project B's child names `y` too and must not attach to A's.
    let a_main = root.join("projA").join("demo.jsonl");
    let a_y = root
        .join("projA")
        .join("demo")
        .join("subagents")
        .join("agent-y.jsonl");
    let a_x = root
        .join("projA")
        .join("demo")
        .join("subagents")
        .join("agent-x.jsonl");
    let b_main = root.join("projB").join("demo.jsonl");
    let b_x = root
        .join("projB")
        .join("demo")
        .join("subagents")
        .join("agent-x.jsonl");
    fx::write(&a_main, &[fx::user("demo", "u0", "go")]);
    fx::write(
        &a_y,
        &[fx::agent(fx::assistant("demo", "y1", "my", (5, 1)), "y")],
    );
    fx::write(
        &a_x,
        &[fx::spawned_by(
            fx::agent(fx::assistant("demo", "x1", "mx", (4, 1)), "x"),
            "y",
            "toolu_a",
        )],
    );
    fx::write(&b_main, &[fx::user("demo", "u0", "go")]);
    fx::write(
        &b_x,
        &[fx::spawned_by(
            fx::agent(fx::assistant("demo", "x2", "mx", (3, 1)), "x"),
            "y",
            "toolu_b",
        )],
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let found = discover(&mut scanner);
    let a_child = found
        .iter()
        .find(|m| m.key.source == SourceKey::File(a_x.clone()))
        .unwrap();
    let b_child = found
        .iter()
        .find(|m| m.key.source == SourceKey::File(b_x.clone()))
        .unwrap();
    assert_eq!(
        a_child.lineage.parent,
        Some(SessionLink::Resolved(key("y", &a_y)))
    );
    assert!(
        !matches!(b_child.lineage.parent, Some(SessionLink::Resolved(_))),
        "a same-named session directory in another project is not this family"
    );
}

#[test]
fn claude_child_discovered_before_its_sibling_parent_is_republished_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let main = root.join("proj").join("demo.jsonl");
    // `agent-a.jsonl` sorts before `aaa/` descending, so the child is visited first
    // and the sibling-directory spawner only afterwards, in the same pass.
    let child = root
        .join("proj")
        .join("demo")
        .join("subagents")
        .join("agent-a.jsonl");
    let parent = root
        .join("proj")
        .join("demo")
        .join("subagents")
        .join("aaa")
        .join("agent-p.jsonl");
    fx::write(&main, &[fx::user("demo", "u0", "go")]);
    fx::write(
        &child,
        &[fx::spawned_by(
            fx::agent(fx::assistant("demo", "a1", "ma", (4, 1)), "a"),
            "p",
            "toolu_1",
        )],
    );
    fx::write(
        &parent,
        &[fx::agent(fx::assistant("demo", "p1", "mp", (5, 1)), "p")],
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let first = discover(&mut scanner);
    let first_child = first
        .iter()
        .find(|m| m.key.source == SourceKey::File(child.clone()))
        .unwrap();
    assert!(
        !matches!(first_child.lineage.parent, Some(SessionLink::Resolved(_))),
        "the spawner is not known yet in the first pass"
    );
    let second = discover(&mut scanner);
    let second_child = second
        .iter()
        .find(|m| m.key.source == SourceKey::File(child.clone()))
        .expect("the child is republished when its spawner appears");
    assert_eq!(
        second_child.lineage.parent,
        Some(SessionLink::Resolved(key("p", &parent)))
    );
}

#[test]
fn claude_duplicate_scope_survives_a_cowork_root_disappearing() {
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    let cowork = d.cowork_projects.join("cowork-project");
    let main = cowork.join("cowork-1.jsonl");
    let child = cowork
        .join("cowork-1")
        .join("subagents")
        .join("agent-a.jsonl");
    fx::write(&main, &[fx::user("cowork-1", "u0", "go")]);
    fx::write(
        &child,
        &[fx::agent(fx::assistant("cowork-1", "a1", "m", (2, 1)), "a")],
    );
    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(d.projects.clone()),
            claude_desktop: Some(d.data.clone()),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    config.process_enabled = false;
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let found = discover(&mut scanner);
    assert_eq!(found.len(), 2);
    assert_eq!(scanner.duplicate_scopes(), scanner.tracked_sources());

    // The Desktop app removes the whole Cowork session; the next pass forgets it.
    std::fs::remove_dir_all(&d.cowork_projects).unwrap();
    discover(&mut scanner);
    discover(&mut scanner);
    assert_eq!(
        scanner.tracked_sources(),
        0,
        "the removed Cowork sources are forgotten"
    );
    assert_eq!(
        scanner.duplicate_scopes(),
        0,
        "the scoped duplicate key was uncounted, not leaked"
    );
}

// ---- Codex native children -------------------------------------------------

fn codex_config(root: &Path) -> Config {
    let mut config = Config::load_with_env(
        &CliOverrides {
            codex_sessions: Some(root.to_path_buf()),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    config.process_enabled = false;
    config
}

fn codex_key(id: &str, path: &Path) -> SessionKey {
    SessionKey {
        harness: Harness::Codex,
        native_id: id.to_owned(),
        source: SourceKey::File(path.to_path_buf()),
    }
}

fn codex_hydrate(
    scanner: &mut CodexScanner,
    key: &SessionKey,
) -> agentscope::domain::SessionSummary {
    support::world::hydrate_complete(scanner, key, NOW)
}

fn usage_total(summary: &agentscope::domain::SessionSummary) -> Option<u64> {
    summary.usage.as_ref().and_then(|u| u.total)
}

/// A parent with 1,000 cumulative tokens and a child that replays that prefix.
struct CodexFamily {
    root: std::path::PathBuf,
    parent: std::path::PathBuf,
    child: std::path::PathBuf,
}

fn codex_family(base: &Path, child_resets: bool) -> CodexFamily {
    let root = base.join("sessions");
    let parent = support::dated(&root, "rollout-parent.jsonl");
    let child = support::dated(&root, "rollout-child.jsonl");
    support::write_lines(
        &parent,
        &[
            cx::session_meta("2026-10-29T10:00:00Z", "parent", "/p"),
            cx::usage_record_thread(
                "2026-10-29T10:00:01Z",
                "parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
        ],
    );
    let mut child_records = vec![
        // Ordinal 0: the child's own identity.
        cx::child_session_meta(
            "2026-10-29T10:00:02Z",
            "kid",
            "parent",
            "parent",
            Some(3),
            "0.159.2",
        ),
        // Ordinals 1..3: the copied parent prefix, including its cumulative counter.
        cx::session_meta("2026-10-29T10:00:00Z", "parent", "/p"),
        cx::usage_record_thread(
            "2026-10-29T10:00:01Z",
            "parent",
            cx::usage(600, 0, 400, 0, 1000),
        ),
        // Owned records.
        cx::turn_context("2026-10-29T10:00:03Z", "gpt-5.5", "/p"),
    ];
    if child_resets {
        child_records.push(cx::usage_record_thread(
            "2026-10-29T10:00:04Z",
            "kid",
            cx::usage(0, 0, 0, 0, 0),
        ));
        child_records.push(cx::usage_record_thread(
            "2026-10-29T10:00:05Z",
            "kid",
            cx::usage(100, 0, 50, 0, 150),
        ));
    } else {
        child_records.push(cx::usage_record_thread(
            "2026-10-29T10:00:04Z",
            "kid",
            cx::usage(700, 0, 450, 0, 1150),
        ));
    }
    support::write_lines(&child, &child_records);
    CodexFamily {
        root,
        parent,
        child,
    }
}

#[test]
fn codex_child_replay_bills_only_the_owned_delta() {
    let dir = tempfile::tempdir().unwrap();
    let f = codex_family(dir.path(), false);
    let mut scanner = CodexScanner::new(&codex_config(&f.root)).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    assert_eq!(found.len(), 2, "two rollouts, two sessions");
    let parent_meta = found.iter().find(|m| m.key.native_id == "parent").unwrap();
    let child_meta = found.iter().find(|m| m.key.native_id == "kid").unwrap();
    assert_eq!(parent_meta.key, codex_key("parent", &f.parent));
    assert_eq!(child_meta.key, codex_key("kid", &f.child));
    assert_eq!(
        child_meta.lineage.parent,
        Some(SessionLink::Resolved(codex_key("parent", &f.parent)))
    );
    assert!(child_meta.lineage.sidechain);
    assert_eq!(child_meta.lineage.agent_type.as_deref(), Some("explorer"));

    let parent = codex_hydrate(&mut scanner, &parent_meta.key);
    let child = codex_hydrate(&mut scanner, &child_meta.key);
    assert_eq!(usage_total(&parent), Some(1000));
    assert_eq!(
        usage_total(&child),
        Some(150),
        "the copied prefix is not the child's spend"
    );
    assert_eq!(
        usage_total(&parent).unwrap() + usage_total(&child).unwrap(),
        1150,
        "the family counts the inherited tokens once"
    );
    assert_ne!(
        usage_total(&parent).unwrap() + usage_total(&child).unwrap(),
        2150
    );
}

#[test]
fn codex_child_local_zero_counter_is_not_subtracted_twice() {
    let dir = tempfile::tempdir().unwrap();
    let f = codex_family(dir.path(), true);
    let mut scanner = CodexScanner::new(&codex_config(&f.root)).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let child_key = found
        .iter()
        .find(|m| m.key.native_id == "kid")
        .unwrap()
        .key
        .clone();
    let child = codex_hydrate(&mut scanner, &child_key);
    assert_eq!(usage_total(&child), Some(150));
}

#[test]
fn codex_child_without_a_trustworthy_boundary_stays_partial() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let parent = support::dated(&root, "rollout-parent.jsonl");
    let child = support::dated(&root, "rollout-child.jsonl");
    support::write_lines(
        &parent,
        &[
            cx::session_meta("2026-10-29T10:00:00Z", "parent", "/p"),
            cx::usage_record_thread(
                "2026-10-29T10:00:01Z",
                "parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
        ],
    );
    // The version predates paginated history, so the recorded boundary is not
    // supported evidence for this shape.
    support::write_lines(
        &child,
        &[
            // The shape is a spawned child with a boundary, but the recorded
            // version predates paginated history and no history mode confirms it.
            cx::session_meta_with(
                "2026-10-29T10:00:02Z",
                json!({
                    "id": "kid",
                    "session_id": "parent",
                    "parent_thread_id": "parent",
                    "cli_version": "0.140.0",
                    "thread_source": "subagent",
                    "subagent_history_start_ordinal": 3,
                    "source": {"subagent": {"thread_spawn": {
                        "parent_thread_id": "parent", "depth": 1
                    }}}
                }),
            ),
            cx::usage_record_thread(
                "2026-10-29T10:00:01Z",
                "parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
            cx::turn_context("2026-10-29T10:00:03Z", "gpt-5.5", "/p"),
            cx::usage_record_thread(
                "2026-10-29T10:00:04Z",
                "kid",
                cx::usage(700, 0, 450, 0, 1150),
            ),
        ],
    );
    let mut scanner = CodexScanner::new(&codex_config(&root)).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let child_key = found
        .iter()
        .find(|m| m.key.native_id == "kid")
        .unwrap()
        .key
        .clone();
    let child = codex_hydrate(&mut scanner, &child_key);
    let usage = child.usage.as_ref().expect("a partial usage fact");
    assert!(
        !usage.coverage.complete,
        "an unsupported boundary never reports confident usage"
    );
    assert_eq!(usage.total, None);
    assert!(
        child
            .warnings
            .iter()
            .any(|w| w.key.source.contains("#usage-boundary")),
        "the missing boundary is visible: {:?}",
        child.warnings
    );
}

#[test]
fn codex_nested_spawn_and_guardian_review_use_recorded_roles() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let parent = support::dated(&root, "rollout-parent.jsonl");
    let outer = support::dated(&root, "rollout-outer.jsonl");
    let inner = support::dated(&root, "rollout-inner.jsonl");
    let guardian = support::dated(&root, "rollout-guardian.jsonl");
    support::write_lines(
        &parent,
        &[cx::session_meta("2026-10-29T10:00:00Z", "parent", "/p")],
    );
    support::write_lines(
        &outer,
        &[cx::child_session_meta(
            "2026-10-29T10:00:01Z",
            "outer",
            "parent",
            "parent",
            None,
            "0.159.2",
        )],
    );
    // A depth-2 child names the root thread in `session_id` and the immediate
    // parent in `parent_thread_id`.
    support::write_lines(
        &inner,
        &[cx::child_session_meta(
            "2026-10-29T10:00:02Z",
            "inner",
            "parent",
            "outer",
            None,
            "0.159.2",
        )],
    );
    support::write_lines(
        &guardian,
        &[cx::guardian_session_meta(
            "2026-10-29T10:00:03Z",
            "review",
            "parent",
            "parent",
        )],
    );
    let mut scanner = CodexScanner::new(&codex_config(&root)).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let by_id = |id: &str| found.iter().find(|m| m.key.native_id == id).unwrap();
    assert_eq!(
        by_id("outer").lineage.parent,
        Some(SessionLink::Resolved(codex_key("parent", &parent)))
    );
    assert_eq!(
        by_id("inner").lineage.parent,
        Some(SessionLink::Resolved(codex_key("outer", &outer))),
        "the immediate parent wins over the root thread ID"
    );
    assert_eq!(by_id("inner").lineage.spawn_depth, Some(1));
    // A guardian review is its own thread, not a spawned child.
    assert_eq!(by_id("review").lineage.parent, None);
    assert_eq!(
        by_id("review").lineage.agent_type.as_deref(),
        Some("guardian")
    );
    assert!(!by_id("review").lineage.sidechain);
}

#[test]
fn codex_root_thread_id_alone_is_not_an_immediate_parent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let parent = support::dated(&root, "rollout-parent.jsonl");
    let child = support::dated(&root, "rollout-child.jsonl");
    support::write_lines(
        &parent,
        &[cx::session_meta("2026-10-29T10:00:00Z", "parent", "/p")],
    );
    // `thread_source` marks a child but nothing names an immediate parent; the
    // recorded `session_id` is only the root thread.
    support::write_lines(
        &child,
        &[cx::session_meta_with(
            "2026-10-29T10:00:01Z",
            json!({
                "id": "kid",
                "session_id": "parent",
                "thread_source": "subagent",
                "cli_version": "0.159.2",
                "source": {"subagent": {"other": "unknown"}}
            }),
        )],
    );
    let mut scanner = CodexScanner::new(&codex_config(&root)).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let child_meta = found.iter().find(|m| m.key.native_id == "kid").unwrap();
    assert_eq!(
        child_meta.lineage.parent, None,
        "a root-thread ID is never promoted to an immediate parent"
    );
}

#[test]
fn codex_source_only_spawn_header_owns_its_usage_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    let parent = support::dated(&root, "rollout-parent.jsonl");
    let child = support::dated(&root, "rollout-child.jsonl");
    support::write_lines(
        &parent,
        &[
            cx::session_meta("2026-10-29T10:00:00Z", "parent", "/p"),
            cx::usage_record_thread(
                "2026-10-29T10:00:01Z",
                "parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
        ],
    );
    // No `thread_source`: only `source.subagent.thread_spawn` proves the child,
    // and the recorded boundary/version still govern its owned usage.
    support::write_lines(
        &child,
        &[
            cx::session_meta_with(
                "2026-10-29T10:00:02Z",
                json!({
                    "id": "kid",
                    "session_id": "parent",
                    "cwd": "/p",
                    "cli_version": "0.159.2",
                    "history_mode": "paginated",
                    "subagent_history_start_ordinal": 2,
                    "source": {"subagent": {"thread_spawn": {
                        "parent_thread_id": "parent",
                        "depth": 1,
                        "agent_role": "explorer"
                    }}}
                }),
            ),
            cx::usage_record_thread(
                "2026-10-29T10:00:01Z",
                "parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
            cx::turn_context("2026-10-29T10:00:03Z", "gpt-5.5", "/p"),
            cx::usage_record_thread(
                "2026-10-29T10:00:04Z",
                "kid",
                cx::usage(700, 0, 450, 0, 1150),
            ),
        ],
    );
    let mut scanner = CodexScanner::new(&codex_config(&root)).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let child_meta = found.iter().find(|m| m.key.native_id == "kid").unwrap();
    assert_eq!(
        child_meta.lineage.parent,
        Some(SessionLink::Resolved(codex_key("parent", &parent)))
    );
    let child = codex_hydrate(&mut scanner, &child_meta.key);
    assert_eq!(
        usage_total(&child),
        Some(150),
        "a source-only spawn header obeys the owned boundary instead of billing 1,150"
    );
}

#[test]
fn opencode_parent_child_nested_and_fork_are_roots_or_children_with_direct_usage() {
    use agentscope::providers::opencode::OpenCodeScanner;
    use agentscope::roster::{MetadataLimits, RosterIndex};
    use support::opencode as ox;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    {
        let conn = ox::create_v2(&db);
        let go = ox::model_json("gpt-5", "opencode-go", "high");
        let zen = ox::model_json("gpt-5", "opencode-zen", "high");
        let mut parent = ox::Session::new("ses_parent", 100, 400);
        parent.model = Some(&go);
        parent.tokens = Some([1_000, 400, 0, 0, 0]);
        parent.cost = Some(0.25);
        ox::insert_session(&conn, &parent);
        let mut child = ox::Session::new("ses_child", 150, 500);
        child.parent_id = Some("ses_parent");
        child.model = Some(&zen);
        child.tokens = Some([200, 80, 0, 0, 0]);
        child.cost = Some(0.05);
        ox::insert_session(&conn, &child);
        let mut nested = ox::Session::new("ses_nested", 200, 600);
        nested.parent_id = Some("ses_child");
        nested.tokens = Some([40, 10, 0, 0, 0]);
        nested.cost = Some(0.01);
        ox::insert_session(&conn, &nested);
        // Fork: its own root, naming the parent as a fork only, reset counters.
        let mut fork = ox::Session::new("ses_fork", 300, 700);
        fork.fork_id = Some("ses_parent");
        fork.tokens = Some([30, 5, 0, 0, 0]);
        fork.cost = Some(0.002);
        ox::insert_session(&conn, &fork);
    }
    let mut config = ox::config_for_db(&db);
    config.process_enabled = false;
    let mut scanner = OpenCodeScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, support::world::BIG).sessions;
    assert_eq!(found.len(), 4);

    // The family tree: children never become roots, and a fork stays a root.
    let mut index = RosterIndex::new(
        MetadataLimits {
            max_sources: 1_000,
            max_bytes: 64 * 1024 * 1024,
        },
        32,
    );
    index.apply_discovery(agentscope::providers::DiscoveryDelta {
        sessions: found.clone(),
        removed: vec![],
        invalidated: vec![],
        warnings: vec![],
        scan: agentscope::domain::ScanProgress {
            harness: Harness::OpenCode,
            scanned: 4,
            total: Some(4),
            done: true,
        },
    });
    index.freeze(1);
    let snapshot = index.snapshot();
    let mut roots: Vec<&str> = snapshot
        .roots
        .iter()
        .map(|k| k.native_id.as_str())
        .collect();
    roots.sort_unstable();
    assert_eq!(roots, ["ses_fork", "ses_parent"]);
    let by_id = |id: &str| found.iter().find(|m| m.key.native_id == id).unwrap();
    let parent_key = &by_id("ses_parent").key;
    let child_key = &by_id("ses_child").key;
    assert_eq!(index.child_count(parent_key), 1);
    assert_eq!(index.child_count(child_key), 1, "a nested child is a child");
    assert_eq!(index.child_count(&by_id("ses_nested").key), 0);
    assert_eq!(index.child_count(&by_id("ses_fork").key), 0);

    // Direct usage is separate for every member; no family rollup.
    let mut usage = |key: &SessionKey| {
        support::world::hydrate_complete(&mut scanner, key, NOW)
            .usage
            .expect("usage")
    };
    let parent_usage = usage(parent_key);
    let child_usage = usage(child_key);
    let nested_usage = usage(&by_id("ses_nested").key);
    let fork_usage = usage(&by_id("ses_fork").key);
    assert_eq!(parent_usage.total, Some(1_400));
    assert_eq!(child_usage.total, Some(280));
    assert_eq!(nested_usage.total, Some(50));
    assert_eq!(fork_usage.total, Some(35));
    assert_eq!(parent_usage.cost.usd, Some(0.25));
    assert_eq!(child_usage.cost.usd, Some(0.05));
    assert_eq!(nested_usage.cost.usd, Some(0.01));
    assert_eq!(fork_usage.cost.usd, Some(0.002));

    // The parent and fork are distinct roots; each child links its exact parent.
    let parent_meta = by_id("ses_parent");
    assert!(parent_meta.lineage.parent.is_none());
    assert_eq!(
        by_id("ses_child").lineage.parent,
        Some(SessionLink::Resolved(parent_key.clone()))
    );
    assert_eq!(
        by_id("ses_nested").lineage.parent,
        Some(SessionLink::Resolved(child_key.clone())),
        "the nested child names its immediate parent"
    );
    assert!(by_id("ses_fork").lineage.parent.is_none());
    assert_eq!(
        by_id("ses_fork").lineage.fork,
        Some(SessionLink::Resolved(parent_key.clone()))
    );
}

// ---- B7: the integrated native world ---------------------------------------------

#[test]
fn native_world_places_children_and_forks_and_bills_only_direct_usage() {
    use support::world::{ROOT_COUNT, Surface, World, surface_all};

    let world = World::native();
    let g = world.grown();
    let mut readers = world.readers();
    let mut surface = Surface::default();
    surface_all(&mut readers, &mut surface);
    let by_key = |key: &SessionKey| {
        surface
            .sessions
            .iter()
            .find(|s| &s.meta.key == key)
            .unwrap_or_else(|| panic!("{:?} {} not discovered", key.harness, key.native_id))
    };

    // Roots, children and nothing else: workflow journals are never sessions.
    assert_eq!(surface.sessions.len(), ROOT_COUNT + g.children.len());
    assert!(surface.sessions.iter().all(|s| !matches!(
        &s.meta.key.source,
        SourceKey::File(path) if path.file_name().is_some_and(|n| n == "journal.jsonl")
    )));

    // Placement: every child names its immediate parent exactly; none is a root.
    for (child, parent) in &g.children {
        assert!(!g.is_root(child));
        assert_eq!(
            by_key(child).meta.lineage.parent,
            Some(SessionLink::Resolved(parent.clone())),
            "{:?} {}",
            child.harness,
            child.native_id
        );
    }
    for (fork, origin) in &g.forks {
        assert!(g.is_root(fork), "a fork is a root");
        let meta = &by_key(fork).meta;
        assert!(meta.lineage.parent.is_none(), "a fork is not a child");
        match meta
            .lineage
            .fork
            .as_ref()
            .expect("the fork names its origin")
        {
            SessionLink::Resolved(key) => assert_eq!(key, origin),
            SessionLink::Unresolved(id) => assert_eq!(id, &origin.native_id),
            SessionLink::Ambiguous => panic!("the fork origin is unambiguous"),
        }
    }
    for (key, _) in &g.roots {
        assert!(
            by_key(key).meta.lineage.parent.is_none(),
            "{:?} {} is a root",
            key.harness,
            key.native_id
        );
    }

    // Direct usage: each member reports only the tokens it owns.
    for (key, tokens) in &g.direct_tokens {
        assert_eq!(
            by_key(key).usage.as_ref().and_then(|u| u.total),
            Some(*tokens),
            "{:?} {}",
            key.harness,
            key.native_id
        );
    }
    // Reported cost is each member's own; a claimed parent total would be the sum.
    for (key, usd) in &g.reported_cost {
        assert_eq!(by_key(key).usage.as_ref().unwrap().cost.usd, Some(*usd));
    }
    // Estimated Claude cost follows direct tokens exactly: 300 : 150 : 75.
    let cost = |key: &SessionKey| {
        by_key(key)
            .usage
            .as_ref()
            .unwrap()
            .cost
            .usd
            .expect("priced")
    };
    let (parent, child, nested) = (
        cost(&g.claude.parent),
        cost(&g.claude.child),
        cost(&g.claude.nested),
    );
    assert!((parent - 2.0 * child).abs() < 1e-12, "{parent} vs {child}");
    assert!((child - 2.0 * nested).abs() < 1e-12, "{child} vs {nested}");

    // An unknown billing route keeps its recorded tokens and an Unknown (never $0) cost;
    // a Codex session whose aggregate cannot select a price tier is also Unknown.
    let unknown = by_key(&g.unknown_billing);
    let facts = unknown.usage.as_ref().unwrap();
    assert_eq!(facts.total, Some(600));
    assert_eq!(facts.cost.usd, None);
    assert_eq!(facts.cost.known_subtotal, 0.0, "no leg was priced");
    assert_eq!(
        unknown
            .meta
            .model
            .as_ref()
            .and_then(|m| m.provider.as_deref()),
        Some("mystery-route")
    );
    assert_eq!(
        by_key(&g.codex.parent).usage.as_ref().unwrap().cost.usd,
        None
    );
}
