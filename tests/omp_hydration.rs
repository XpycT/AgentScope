//! OMP lifetime request accounting and persisted measured facts, driven through the
//! real `OmpHydration` over synthetic sources. Cards price `acme/m` at output
//! 10 USD/M with explicit zero input/cache rates, and every fixture call records its
//! zero input/cache counters explicitly, so a call's cost is exactly its output.

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    AggregationClass, CompactionScope, Harness, LocatorKind, Provenance, RefreshBudget,
    RequestPurpose, SessionKey, SessionLink, SessionMetadata, SessionSummary, SourceKey,
    SourceLocator,
};
use agentscope::providers::omp::hydration::ChildClaim;
use agentscope::providers::omp::{
    OmpCatalog, OmpHydration, OmpOwnership, map_omp_usage, owned_request,
};
use serde_json::{Value, json};
use support::facts::price_schedule;
use support::omp as fx;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

const TINY: RefreshBudget = RefreshBudget {
    max_records: 2,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn prices() -> agentscope::pricing::PriceSchedule {
    price_schedule(
        r#"[{"billing_provider":"acme","model":"m",
             "rates":{"input":0.0,"output":10.0,"cache_read":0.0,"cache_write":0.0},
             "service_multipliers":{"priority":2.0}},
            {"billing_provider":"acme","model":"m2",
             "rates":{"input":0.0,"output":20.0,"cache_read":0.0,"cache_write":0.0}}]"#,
    )
}

fn config(root: &Path) -> Config {
    let cli = CliOverrides {
        omp_sessions: Some(root.to_path_buf()),
        ..CliOverrides::default()
    };
    let mut config = Config::load_with_env(&cli, &Env::new()).expect("valid test config");
    config.pricing = prices();
    config
}

/// Discovered metadata of every session under `root`, by native id.
fn discover(root: &Path) -> HashMap<String, SessionMetadata> {
    let mut catalog = OmpCatalog::new(vec![root.to_path_buf()], 1000, 64 << 20);
    let mut latest: HashMap<SessionKey, SessionMetadata> = HashMap::new();
    for _ in 0..1000 {
        let delta = catalog.step(BIG).expect("discovery");
        for meta in delta.sessions {
            latest.insert(meta.key.clone(), meta);
        }
        if delta.scan.done {
            break;
        }
    }
    latest
        .into_values()
        .map(|m| (m.key.native_id.clone(), m))
        .collect()
}

/// Steps one hydration to completion; returns the summary and the slice count.
fn finish(hydration: &mut OmpHydration, budget: RefreshBudget) -> (SessionSummary, usize) {
    for slices in 1..=100_000 {
        let delta = hydration.step(budget).expect("step");
        if delta.done {
            return (delta.summary, slices);
        }
    }
    panic!("hydration never finished");
}

fn hydrate(root: &Path, id: &str) -> SessionSummary {
    let meta = discover(root).remove(id).expect("discovered");
    let mut hydration = OmpHydration::new(meta, &config(root)).expect("hydration");
    finish(&mut hydration, BIG).0
}

fn usd(summary: &SessionSummary) -> Option<f64> {
    summary.usage.as_ref().expect("usage").cost.usd
}

fn close(a: Option<f64>, b: f64) {
    let a = a.expect("a complete amount");
    assert!((a - b).abs() < 1e-9, "{a} != {b}");
}

fn root(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("sessions")
}

fn out(n: u64) -> Value {
    // Saved money is deliberately absurd: it must never become the cost.
    fx::usage(0, n, 0, 0, 99.0)
}

#[test]
fn parent_child_branch_and_family_accounting() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "do it"),
            fx::with_snapshot(
                fx::with_message(
                    fx::with_message(
                        fx::call("a1", Some("u1"), Some("resp-a1"), out(10_000)),
                        "duration",
                        json!(19_000),
                    ),
                    "ttft",
                    json!(1_300),
                ),
                215_000,
                None,
            ),
            // The task result quotes the child's cost; it is private evidence only.
            fx::task_result("t1", Some("a1"), "c", 0.20),
            // Auxiliary work recorded off the active branch (it restores the leaf).
            fx::with_leaf(fx::aux("x1", Some("u1"), "title", out(3_000)), Some("t1")),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t1_p/c",
        &[
            fx::header("c", None, None),
            fx::user_message("cu", None, "child task"),
            fx::call("ca", Some("cu"), Some("resp-ca"), out(20_000)),
        ],
    );
    let meta = discover(&root);
    let mut parent = OmpHydration::new(meta["p"].clone(), &config(&root)).unwrap();
    let (parent_summary, _) = finish(&mut parent, BIG);
    let child_summary = hydrate(&root, "c");

    let parent_direct = usd(&parent_summary).unwrap();
    let child_direct = usd(&child_summary).unwrap();
    assert!((parent_direct - 0.13).abs() < 1e-9, "{parent_direct}");
    assert!((child_direct - 0.20).abs() < 1e-9, "{child_direct}");
    let unique_family = parent_direct + child_direct;
    assert!((unique_family - 0.33).abs() < 1e-9, "{unique_family}");

    // The quoted child cost stays private adapter evidence and is never added.
    let claims = parent.child_claims().to_vec();
    assert_eq!(
        claims,
        vec![ChildClaim {
            agent_id: "c".into(),
            quoted_usd: Some(0.20)
        }]
    );
    let naive = parent_direct + claims[0].quoted_usd.unwrap() + child_direct;
    assert!(
        (naive - 0.53).abs() < 1e-9,
        "the double count that is avoided"
    );
    assert!((unique_family - naive).abs() > 0.1);

    // Two owned calls (the off-branch auxiliary call is retained), no extra requests.
    let usage = parent_summary.usage.as_ref().unwrap();
    assert_eq!(usage.coverage.observed_requests, 2);
    assert!(usage.coverage.complete);
    assert_eq!(usage.class, AggregationClass::Derived);
    // The saved number is not the price; the recomputed amount is an estimate.
    assert_eq!(usage.cost.class, AggregationClass::Estimated);
    assert_eq!(usage.reported_cost_usd, None);

    // Speed counts only the conversation call (the auxiliary call is excluded).
    let speed = &parent_summary.metrics.speed;
    assert_eq!(speed.total_requests, 1);
    assert_eq!(speed.measured_requests, 1);
    assert!((speed.weighted_tokens_per_second.unwrap() - 10_000.0 / 19.0).abs() < 1e-6);
    assert!((parent_summary.metrics.timing.model_ms.unwrap() - 19_000.0).abs() < 1e-6);
    assert_eq!(parent_summary.metrics.context.used, Some(215_000));
    assert_eq!(
        parent_summary.metrics.context.class,
        AggregationClass::Reported
    );
}

#[test]
fn orchestration_is_billed_once_and_never_counts_as_occupancy() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    // 1,000 conversation output + 100,000 orchestration output at 10 USD/M = 1.01.
    let orchestrated =
        fx::with_orchestration(fx::usage(0, 1_000, 0, 0, 0.0), 40_000, 100_000, 5_000);
    fx::write_session(
        &root,
        "b",
        "t1_o",
        &[
            fx::header("o", None, None),
            fx::user_message("u1", None, "go"),
            fx::with_snapshot(
                fx::call("a1", Some("u1"), Some("r1"), orchestrated),
                50_000,
                None,
            ),
        ],
    );
    let summary = hydrate(&root, "o");
    let usage = summary.usage.as_ref().unwrap();
    close(usage.cost.usd, 1.01);
    assert_eq!(usage.coverage.observed_requests, 1);
    assert_eq!(usage.categories.orchestration_output, Some(100_000));
    assert_eq!(usage.categories.orchestration_input, Some(40_000));
    assert_eq!(usage.categories.orchestration_cache_read, Some(5_000));
    assert_eq!(usage.categories.output, Some(101_000));
    // Occupancy is the provider's prompt snapshot, not prompt + orchestration.
    assert_eq!(summary.metrics.context.used, Some(50_000));
}

#[test]
fn context_prefers_the_snapshot_minus_rewrites_and_never_total_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let mut call = fx::with_snapshot(
        fx::call("a1", Some("u1"), Some("r1"), fx::usage(0, 1_600, 0, 0, 0.0)),
        215_000,
        Some(5_000),
    );
    call["message"]["usage"]["totalTokens"] = json!(999_999);
    fx::write_session(
        &root,
        "b",
        "t1_s",
        &[
            fx::header("s", None, None),
            fx::user_message("u1", None, "go"),
            call,
        ],
    );
    let summary = hydrate(&root, "s");
    let context = &summary.metrics.context;
    assert_eq!(context.used, Some(210_000), "215k - 5k, never 999,999");
    assert_eq!(context.class, AggregationClass::Estimated);

    // No snapshot: explicit contextTokens, then the additive prompt categories.
    let dir = tempfile::tempdir().unwrap();
    let root = self::root(&dir);
    let mut explicit = fx::call("a1", Some("u1"), Some("r1"), fx::usage(7, 1, 3, 5, 0.0));
    explicit["message"]["usage"]["contextTokens"] = json!(40_000);
    let additive = fx::call("a2", Some("a1"), Some("r2"), fx::usage(7, 1, 3, 5, 0.0));
    fx::write_session(
        &root,
        "b",
        "t1_t",
        &[
            fx::header("t", None, None),
            fx::user_message("u1", None, "go"),
            explicit,
        ],
    );
    let explicit_summary = hydrate(&root, "t");
    assert_eq!(explicit_summary.metrics.context.used, Some(40_000));
    assert_eq!(
        explicit_summary.metrics.context.class,
        AggregationClass::Reported
    );
    fx::append(&root.join("b").join("t1_t.jsonl"), &[additive]);
    let additive_summary = hydrate(&root, "t");
    assert_eq!(
        additive_summary.metrics.context.used,
        Some(15),
        "input + cacheRead + cacheWrite, not output or total"
    );
    assert_eq!(
        additive_summary.metrics.context.class,
        AggregationClass::Derived
    );
}

#[test]
fn compaction_invalidates_stale_context_and_its_cost_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let path = fx::write_session(
        &root,
        "b",
        "t1_c",
        &[
            fx::header("c", None, None),
            fx::user_message("u1", None, "go"),
            fx::with_snapshot(
                fx::call("a1", Some("u1"), Some("r1"), out(1_000)),
                120_000,
                None,
            ),
            fx::compaction("k1", Some("a1"), "u1"),
            // A summarization-looking auxiliary call is real owned spend, but it is
            // not an attributed compaction cost.
            fx::aux("x1", Some("k1"), "compaction", out(1_000)),
        ],
    );
    let summary = hydrate(&root, "c");
    let metrics = &summary.metrics;
    assert_eq!(
        metrics.context.used, None,
        "pre-compaction occupancy is stale"
    );
    assert_eq!(metrics.compactions.observed, 1);
    assert_eq!(metrics.compactions.known_cost_usd, None);
    assert_eq!(metrics.compactions.cost_scope, CompactionScope::Unknown);
    assert_eq!(metrics.compactions.unpriced, 1);
    let latest = metrics.compactions.latest.as_ref().unwrap();
    assert_eq!(latest.before_tokens, Some(120_000));
    assert_eq!(latest.after_tokens, Some(30_000));
    assert_eq!(latest.method.as_deref(), Some("summary"));
    assert!(latest.usage.is_none(), "no persisted attribution exists");
    // Both calls are lifetime spend of the session.
    close(usd(&summary), 0.02);

    // A later snapshot makes the occupancy current again.
    fx::append(
        &path,
        &[fx::with_snapshot(
            fx::call("a2", Some("x1"), Some("r2"), out(500)),
            31_000,
            None,
        )],
    );
    let summary = hydrate(&root, "c");
    assert_eq!(summary.metrics.context.used, Some(31_000));
}

#[test]
fn speed_uses_whole_request_duration_and_unknown_without_one() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_v",
        &[
            fx::header("v", None, None),
            fx::user_message("u1", None, "go"),
            fx::with_message(
                fx::with_message(
                    fx::call("a1", Some("u1"), Some("r1"), fx::usage(0, 1_600, 0, 0, 0.0)),
                    "duration",
                    json!(19_000),
                ),
                "ttft",
                json!(1_300),
            ),
        ],
    );
    let speed = hydrate(&root, "v").metrics.speed;
    let rate = speed.weighted_tokens_per_second.unwrap();
    assert!((rate - 84.2).abs() < 0.05, "{rate}");
    assert_eq!(speed.latest.as_ref().unwrap().ttft_ms, Some(1_300.0));
    assert!(speed.complete);

    fx::write_session(
        &root,
        "b",
        "t1_w",
        &[
            fx::header("w", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), fx::usage(0, 1_600, 0, 0, 0.0)),
        ],
    );
    let speed = hydrate(&root, "w").metrics.speed;
    assert_eq!(
        speed.weighted_tokens_per_second, None,
        "no duration is Unknown"
    );
    assert_eq!(speed.measured_requests, 0);
    assert_eq!(speed.total_requests, 1);
    assert!(!speed.complete);
}

#[test]
fn recorded_service_tier_prices_the_request_not_the_current_setting() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_f",
        &[
            fx::header("f", None, None),
            fx::user_message("u1", None, "go"),
            fx::with_message(
                fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
                "serviceTier",
                json!("priority"),
            ),
            // The setting changes afterwards; the recorded request must not be repriced.
            fx::service_tier_change("s1", Some("a1"), "flex"),
            fx::call("a2", Some("s1"), Some("r2"), out(10_000)),
        ],
    );
    // priority 2x (0.20) + standard (0.10).
    close(usd(&hydrate(&root, "f")), 0.30);

    // A recorded tier the card does not price stays unpriced, never standard.
    fx::write_session(
        &root,
        "b",
        "t1_g",
        &[
            fx::header("g", None, None),
            fx::user_message("u1", None, "go"),
            fx::with_message(
                fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
                "serviceTier",
                json!("ultrafast"),
            ),
            fx::call("a2", Some("a1"), Some("r2"), out(10_000)),
        ],
    );
    let summary = hydrate(&root, "g");
    let usage = summary.usage.as_ref().unwrap();
    assert_eq!(usage.cost.usd, None);
    assert_eq!(usage.coverage.observed_requests, 2);
    assert_eq!(usage.coverage.priced_requests, 1);
    assert!((usage.cost.known_subtotal - 0.10).abs() < 1e-9);
}

#[test]
fn model_and_effort_follow_the_active_branch_but_cost_covers_every_branch() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_m",
        &[
            fx::header("m", None, None),
            fx::user_message("u1", None, "go"),
            fx::thinking_level("e1", Some("u1"), "high", "auto"),
            fx::call("a1", Some("e1"), Some("r1"), out(10_000)),
            // Another branch from u1: low effort on another model.
            fx::thinking_level("e2", Some("u1"), "low", "low"),
            fx::model_change("m2", Some("e2"), "acme/m2"),
            fx::with_message(
                fx::call("a2", Some("m2"), Some("r2"), out(10_000)),
                "model",
                json!("m2"),
            ),
            // The last write restores the first branch as the leaf.
            fx::with_leaf(fx::user_message("u2", Some("a1"), "again"), Some("a1")),
        ],
    );
    let summary = hydrate(&root, "m");
    let model = summary.meta.model.as_ref().expect("current model");
    assert_eq!(model.id, "m");
    assert_eq!(model.provider.as_deref(), Some("acme"));
    assert_eq!(model.effort.as_deref(), Some("high"));
    assert_eq!(model.configured_effort.as_deref(), Some("auto"));
    // Lifetime spend keeps the off-branch call on the other model: 0.10 + 0.20.
    close(usd(&summary), 0.30);
    let models = &summary.usage.as_ref().unwrap().models;
    assert_eq!(models.len(), 2);

    // Moving the leaf to the other branch switches the current facts, not the cost.
    fx::append(
        &root.join("b").join("t1_m.jsonl"),
        &[fx::with_leaf(
            fx::user_message("u3", Some("a2"), "other"),
            None,
        )],
    );
    let cleared = hydrate(&root, "m");
    assert_eq!(
        cleared.meta.model, None,
        "a cleared leaf has no current model"
    );
    close(usd(&cleared), 0.30);
}

#[test]
fn fork_copies_are_inherited_and_only_new_calls_are_charged() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let parent = [
        fx::header("p", None, None),
        fx::user_message("u1", None, "go"),
        fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
        fx::call("a2", Some("a1"), Some("r2"), out(20_000)),
    ];
    fx::write_session(&root, "b", "t1_p", &parent);
    // The fork copies the parent's entries with unchanged IDs, resets their money to
    // zero while keeping tokens, then adds new calls.
    let mut copied_a1 = fx::call(
        "a1",
        Some("u1"),
        Some("r1"),
        fx::usage(0, 10_000, 0, 0, 0.0),
    );
    copied_a1["message"]["usage"]["cost"]["total"] = json!(0.0);
    fx::write_session(
        &root,
        "b",
        "t2_f",
        &[
            fx::header("f", Some("p"), None),
            fx::user_message("u1", None, "go"),
            copied_a1,
            fx::call(
                "a2",
                Some("a1"),
                Some("r2"),
                fx::usage(0, 20_000, 0, 0, 0.0),
            ),
            // Identical counters and text but a distinct request: real new spend.
            fx::call("a3", Some("a2"), Some("r3"), out(10_000)),
            fx::call("a4", Some("a3"), Some("r4"), out(5_000)),
        ],
    );
    close(usd(&hydrate(&root, "p")), 0.30);
    let fork = hydrate(&root, "f");
    close(usd(&fork), 0.15);
    let usage = fork.usage.as_ref().unwrap();
    assert_eq!(usage.coverage.observed_requests, 2, "a3 and a4 only");
    assert!(usage.coverage.complete);
    // The raw ambiguous saved money is retained as such, never billed.
    assert_eq!(usage.reported_cost_usd, None);
}

#[test]
fn a_reused_entry_id_with_another_response_id_is_new_spend() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t2_f",
        &[
            fx::header("f", Some("p"), None),
            fx::user_message("u1", None, "go"),
            // Same entry id, different confirmed response: not a copy.
            fx::call("a1", Some("u1"), Some("r-other"), out(10_000)),
            // Another entry id carrying the parent's confirmed response id: a copy.
            fx::call("a9", Some("a1"), Some("r1"), out(10_000)),
        ],
    );
    let fork = hydrate(&root, "f");
    close(usd(&fork), 0.10);
    assert_eq!(fork.usage.as_ref().unwrap().coverage.observed_requests, 1);
}

#[test]
fn unprovable_ownership_is_partial_even_when_timestamps_precede_the_fork() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    // The named parent was never discovered: nothing can rule out a copy.
    fx::write_session(
        &root,
        "b",
        "t2_f",
        &[
            fx::header("f", Some("ghost"), None),
            fx::at(
                fx::user_message("u1", None, "go"),
                "2020-01-01T00:00:00.000Z",
            ),
            fx::at(
                fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
                "2020-01-01T00:00:01.000Z",
            ),
        ],
    );
    let fork = hydrate(&root, "f");
    let usage = fork.usage.as_ref().unwrap();
    assert_eq!(usage.cost.usd, None);
    assert!(!usage.coverage.complete);
    assert_eq!(usage.coverage.observed_requests, 0, "nothing is guessed");
    assert!(
        usage
            .coverage
            .reason
            .as_deref()
            .unwrap_or("")
            .contains("ownership")
    );
    assert_eq!(usage.class, AggregationClass::Partial);

    // Two sessions share the parent's id: the reference is ambiguous.
    for name in ["t1_p", "t3_p"] {
        fx::write_session(
            &root,
            "b",
            name,
            &[
                fx::header("dup", None, None),
                fx::user_message("u1", None, "go"),
            ],
        );
    }
    fx::write_session(
        &root,
        "b",
        "t4_g",
        &[
            fx::header("g", Some("dup"), None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
        ],
    );
    let metas = discover(&root);
    assert_eq!(metas["g"].lineage.fork, Some(SessionLink::Ambiguous));
    let ambiguous = hydrate(&root, "g");
    assert_eq!(ambiguous.usage.as_ref().unwrap().cost.usd, None);
    assert!(!ambiguous.usage.as_ref().unwrap().coverage.complete);
}

#[test]
fn continuation_segments_fold_without_double_counting_copies() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t2_p2",
        &[
            fx::header("p2", Some("p"), Some("p")),
            fx::user_message("u1", None, "go"),
            // Repeated from the earlier segment with unchanged IDs.
            fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
            fx::with_snapshot(
                fx::call("a2", Some("a1"), Some("r2"), out(20_000)),
                80_000,
                None,
            ),
        ],
    );
    let metas = discover(&root);
    assert_eq!(metas["p"].members.len(), 2);
    let summary = hydrate(&root, "p");
    close(usd(&summary), 0.30);
    assert_eq!(
        summary.usage.as_ref().unwrap().coverage.observed_requests,
        2
    );
    // Current facts come from the newest segment's active branch.
    assert_eq!(summary.metrics.context.used, Some(80_000));
}

#[test]
fn exit_is_cleared_by_a_later_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let path = fx::write_session(
        &root,
        "b",
        "t1_e",
        &[
            fx::header("e", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(1_000)),
            fx::at(fx::exit("x1", Some("a1")), "2026-10-30T01:00:00.000Z"),
        ],
    );
    let closed = hydrate(&root, "e");
    assert_eq!(
        closed.metrics.timing.ended_at,
        Some(fx::STAMP_MS + 3_600_000)
    );
    assert_eq!(closed.metrics.timing.elapsed_ms, Some(3_600_000));
    assert_eq!(
        closed.metrics.timing.active_ms, None,
        "no active timer exists"
    );

    fx::append(
        &path,
        &[fx::at(
            fx::call("a2", Some("x1"), Some("r2"), out(1_000)),
            "2026-10-30T02:00:00.000Z",
        )],
    );
    let reopened = hydrate(&root, "e");
    assert_eq!(reopened.metrics.timing.ended_at, None);
    assert_eq!(reopened.metrics.timing.elapsed_ms, None);
    assert_eq!(
        reopened.last_event_at,
        Some(fx::STAMP_MS + 7_200_000),
        "the latest recorded event"
    );
}

#[test]
fn zero_usage_aborted_assistant_is_kept_as_recorded_not_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let mut aborted = fx::call("a1", Some("u1"), None, fx::usage(0, 0, 0, 0, 0.0));
    aborted["message"]["stopReason"] = json!("aborted");
    fx::write_session(
        &root,
        "b",
        "t1_z",
        &[
            fx::header("z", None, None),
            fx::user_message("u1", None, "go"),
            aborted,
            // An assistant record with no usage object at all is missing spend.
            fx::entry(
                "a2",
                Some("a1"),
                "message",
                json!({"message": {"role": "assistant", "content": [], "provider": "acme",
                                   "model": "m", "stopReason": "aborted", "timestamp": fx::STAMP_MS}}),
            ),
        ],
    );
    let summary = hydrate(&root, "z");
    let usage = summary.usage.as_ref().unwrap();
    assert_eq!(
        usage.coverage.observed_requests, 1,
        "the recorded request is not dropped as a placeholder"
    );
    assert!(
        !usage.coverage.complete,
        "the usage-less record is not guessed"
    );
    assert_eq!(usage.cost.usd, None);
    assert_eq!(summary.metrics.speed.measured_requests, 0);
}

#[test]
fn repeated_lines_and_response_ids_are_counted_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let a1 = fx::call("a1", Some("u1"), Some("r1"), out(10_000));
    fx::write_session(
        &root,
        "b",
        "t1_d",
        &[
            fx::header("d", None, None),
            fx::user_message("u1", None, "go"),
            a1.clone(),
            // The same line written again.
            a1.clone(),
            // The same confirmed response under another entry (a retry of the write).
            fx::call("a5", Some("a1"), Some("r1"), out(10_000)),
        ],
    );
    let summary = hydrate(&root, "d");
    close(usd(&summary), 0.10);
    assert_eq!(
        summary.usage.as_ref().unwrap().coverage.observed_requests,
        1
    );

    // The same entry id carrying different counters cannot be attributed.
    fx::write_session(
        &root,
        "b",
        "t1_h",
        &[
            fx::header("h", None, None),
            fx::user_message("u1", None, "go"),
            a1.clone(),
            fx::call("a1", Some("u1"), Some("r1"), out(99_000)),
        ],
    );
    let conflicted = hydrate(&root, "h");
    let usage = conflicted.usage.as_ref().unwrap();
    assert!(!usage.coverage.complete);
    assert_eq!(usage.cost.usd, None);
}

#[test]
fn hydration_is_resumable_cancellable_and_follows_growth() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let mut entries = vec![
        fx::header("r", None, None),
        fx::user_message("u1", None, "go"),
    ];
    let mut parent = "u1".to_owned();
    for n in 0..20 {
        let id = format!("a{n}");
        entries.push(fx::call(
            &id,
            Some(&parent),
            Some(&format!("r{n}")),
            out(1_000),
        ));
        parent = id;
    }
    let path = fx::write_session(&root, "b", "t1_r", &entries);
    let meta = discover(&root).remove("r").unwrap();
    let mut hydration = OmpHydration::new(meta.clone(), &config(&root)).unwrap();

    // Partial progress carries no hydrated fact.
    let first = hydration.step(TINY).unwrap();
    assert!(!first.done);
    assert!(first.summary.usage.is_none());
    assert_eq!(first.summary.metrics, Default::default());
    assert!(hydration.retained_requests() <= 2);
    // Cancel drops the bounded work; the next step starts over.
    hydration.cancel();
    assert_eq!(hydration.retained_requests(), 0);
    let (summary, slices) = finish(&mut hydration, TINY);
    assert!(slices > 5, "budgeted slices, got {slices}");
    close(usd(&summary), 0.20);

    // A second hydration reaches the same facts in a single pass.
    let mut once = OmpHydration::new(meta, &config(&root)).unwrap();
    assert_eq!(finish(&mut once, BIG).0.usage, summary.usage);

    // Growth is folded on the next call without recounting.
    fx::append(
        &path,
        &[fx::call("a20", Some("a19"), Some("r20"), out(1_000))],
    );
    let (grown, _) = finish(&mut hydration, BIG);
    close(usd(&grown), 0.21);
    assert_eq!(grown.usage.as_ref().unwrap().coverage.observed_requests, 21);
    // A torn trailing line is not a record until it is completed.
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(b"{\"type\":\"message\",\"id\":\"a21\"")
            .unwrap();
    }
    let (torn, _) = finish(&mut hydration, BIG);
    close(usd(&torn), 0.21);
}

#[test]
fn unreadable_records_make_the_usage_partial() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let mut body = fx::title_slot("t1_q");
    for line in [
        fx::header("q", None, None).to_string(),
        fx::user_message("u1", None, "go").to_string(),
        "{ this is not json".to_owned(),
        fx::call("a1", Some("u1"), Some("r1"), out(10_000)).to_string(),
    ] {
        body.extend_from_slice(line.as_bytes());
        body.push(b'\n');
    }
    fx::write_raw(&root, "b", "t1_q", &body);
    let summary = hydrate(&root, "q");
    let usage = summary.usage.as_ref().unwrap();
    assert!(!usage.coverage.complete);
    assert_eq!(usage.coverage.observed_requests, 1);
    assert!(
        summary
            .warnings
            .iter()
            .all(|w| !w.message.contains("this is not json"))
    );
}

#[test]
fn map_omp_usage_binds_producer_fields_and_keeps_money_unverified() {
    let key = SessionKey {
        harness: Harness::Omp,
        native_id: "s".into(),
        source: SourceKey::File(PathBuf::from("s.jsonl")),
    };
    let provenance = |kind: &str| Provenance {
        harness: Harness::Omp,
        source: key.source.clone(),
        locator: SourceLocator {
            session: key.clone(),
            kind: LocatorKind::FileOffset {
                path: PathBuf::from("s.jsonl"),
                offset: 7,
            },
            generation: Some(1),
        },
        record_type: kind.into(),
        harness_version: None,
    };
    let mut usage = fx::with_orchestration(fx::usage(10, 200, 30, 40, 1.5), 1, 2, 3);
    usage["reasoningTokens"] = json!(50);
    usage["cttl"] = json!({"ephemeral5m": 25, "ephemeral1h": 15});
    let mut assistant = fx::call("a1", Some("u1"), Some("resp-1"), usage);
    assistant["message"]["serviceTier"] = json!("priority");
    assistant["message"]["duration"] = json!(19_000);
    assistant["message"]["ttft"] = json!(1_300);
    let observation = map_omp_usage(&assistant, provenance("message/assistant")).unwrap();
    let c = &observation.categories;
    assert_eq!(
        (c.input, c.output, c.cache_read, c.cache_write),
        (Some(10), Some(200), Some(30), Some(40))
    );
    assert_eq!((c.cache_write_5m, c.cache_write_1h), (Some(25), Some(15)));
    assert_eq!(c.reasoning, Some(50));
    assert_eq!(
        (
            c.orchestration_input,
            c.orchestration_output,
            c.orchestration_cache_read
        ),
        (Some(1), Some(2), Some(3))
    );
    let request = observation.request.as_ref().unwrap();
    assert_eq!(request.id.as_deref(), Some("resp-1"));
    assert_eq!(request.service_tier.as_deref(), Some("priority"));
    assert_eq!(
        request.inference_region, None,
        "the schema persists no region"
    );
    assert_eq!(request.purpose, RequestPurpose::Conversation);
    assert_eq!(request.duration_ms, Some(19_000.0));
    assert_eq!(request.ttft_ms, Some(1_300.0));
    assert_eq!(observation.money.unverified_usd, Some(1.5));
    assert!(observation.money.reported_api.is_none() && observation.money.saved_estimate.is_none());
    let model = observation.model.as_ref().unwrap();
    assert_eq!(
        (model.id.as_str(), model.provider.as_deref()),
        ("m", Some("acme"))
    );

    // A cache split that does not add up is not used.
    let mut odd = fx::usage(10, 200, 30, 40, 0.0);
    odd["cttl"] = json!({"ephemeral5m": 90});
    let odd = fx::call("a2", None, None, odd);
    let observation = map_omp_usage(&odd, provenance("message/assistant")).unwrap();
    assert_eq!(observation.categories.cache_write_5m, None);

    // `model_usage` is auxiliary work with its purpose.
    let aux = fx::aux("x1", None, "title", fx::usage(0, 9, 0, 0, 0.0));
    let observation = map_omp_usage(&aux, provenance("model_usage")).unwrap();
    assert_eq!(
        observation.request.as_ref().unwrap().purpose,
        RequestPurpose::Auxiliary("title".into())
    );
    // Not calls: user messages, tool results and usage-less assistants.
    assert!(map_omp_usage(&fx::user_message("u", None, "x"), provenance("m")).is_none());
    assert!(map_omp_usage(&fx::task_result("t", None, "c", 1.0), provenance("m")).is_none());
    assert!(map_omp_usage(&fx::compaction("k", None, "u"), provenance("m")).is_none());
    let mut bare = fx::call("a3", None, None, json!({}));
    bare["message"].as_object_mut().unwrap().remove("usage");
    assert!(map_omp_usage(&bare, provenance("m")).is_none());
}

#[test]
fn owned_request_distinguishes_owned_inherited_and_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t2_f",
        &[
            fx::header("f", Some("p"), None),
            fx::user_message("u1", None, "go"),
        ],
    );
    let metas = discover(&root);
    let (parent, fork) = (metas["p"].key.clone(), metas["f"].key.clone());
    let mut catalog = OmpCatalog::new(vec![root.clone()], 100, 1 << 20);
    catalog.seed_links(&metas["p"]);
    catalog.seed_links(&metas["f"]);

    // A source with no linked predecessor owns everything.
    assert_eq!(
        owned_request("a1", Some("r1"), &parent, &catalog),
        OmpOwnership::Owned
    );
    // The fork's parent evidence is not known yet: a copy cannot be ruled out.
    assert_eq!(
        owned_request("a1", Some("r1"), &fork, &catalog),
        OmpOwnership::Unknown
    );
    assert_eq!(
        catalog.fork_proof_sources(&fork),
        Some(vec![parent.clone()])
    );
    catalog.begin_evidence(&parent);
    assert!(catalog.record_evidence(&parent, "a1", Some("r1"), 1));
    // Still incomplete: a match is proof, a miss is not.
    assert_eq!(
        owned_request("a1", Some("r1"), &fork, &catalog),
        OmpOwnership::Inherited
    );
    assert_eq!(
        owned_request("zz", Some("rz"), &fork, &catalog),
        OmpOwnership::Unknown
    );
    catalog.complete_evidence(&parent);
    assert_eq!(
        owned_request("zz", Some("rz"), &fork, &catalog),
        OmpOwnership::Owned
    );
    // Same entry id, different confirmed response: a new request.
    assert_eq!(
        owned_request("a1", Some("other"), &fork, &catalog),
        OmpOwnership::Owned
    );
    // Same confirmed response under another entry id: a copy.
    assert_eq!(
        owned_request("renamed", Some("r1"), &fork, &catalog),
        OmpOwnership::Inherited
    );
    // No confirmed response on the copy: the unchanged entry id decides.
    assert_eq!(
        owned_request("a1", None, &fork, &catalog),
        OmpOwnership::Inherited
    );
}

#[test]
fn evidence_limit_and_content_free_debug() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
        ],
    );
    let metas = discover(&root);
    let key = metas["p"].key.clone();
    let mut catalog = OmpCatalog::new(vec![root.clone()], 100, 1 << 20).with_evidence_limit(1);
    catalog.begin_evidence(&key);
    assert!(catalog.record_evidence(&key, "a1", None, 1));
    assert!(!catalog.record_evidence(&key, "a2", None, 2), "bounded");
    let claim = ChildClaim {
        agent_id: "secret-agent".into(),
        quoted_usd: Some(1.0),
    };
    assert!(!format!("{claim:?}").contains("secret-agent"));
}

/// Prints what the actual hydration observed over a synthetic store (run with
/// `--nocapture`); asserts only that the numbers exist.
#[test]
fn hydration_smoke_over_a_synthetic_store() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            fx::thinking_level("e1", Some("u1"), "high", "auto"),
            fx::with_snapshot(
                fx::with_message(
                    fx::call("a1", Some("e1"), Some("r1"), out(10_000)),
                    "duration",
                    json!(19_000),
                ),
                215_000,
                Some(5_000),
            ),
            fx::task_result("t1", Some("a1"), "c", 0.20),
            fx::compaction("k1", Some("t1"), "u1"),
            fx::with_leaf(fx::aux("x1", Some("u1"), "title", out(3_000)), Some("k1")),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t1_p/c",
        &[
            fx::header("c", None, None),
            fx::user_message("cu", None, "child"),
            fx::call("ca", Some("cu"), Some("rc"), out(20_000)),
        ],
    );
    for id in ["p", "c"] {
        let meta = discover(&root).remove(id).unwrap();
        let mut hydration = OmpHydration::new(meta, &config(&root)).unwrap();
        let (summary, slices) = finish(&mut hydration, TINY);
        let usage = summary.usage.as_ref().unwrap();
        let (model, effort, configured) = summary
            .meta
            .model
            .as_ref()
            .map_or(("-".to_owned(), None, None), |m| {
                (m.id.clone(), m.effort.clone(), m.configured_effort.clone())
            });
        println!(
            "{id}: slices={slices} direct={:?} class={:?} requests={} model={model} effort={effort:?} \
             configured={configured:?} context.used={:?} context.class={:?} speed={:?} \
             compactions={} compaction_cost={:?} claims={:?}",
            usage.cost.usd,
            usage.class,
            usage.coverage.observed_requests,
            summary.metrics.context.used,
            summary.metrics.context.class,
            summary.metrics.speed.weighted_tokens_per_second,
            summary.metrics.compactions.observed,
            summary.metrics.compactions.known_cost_usd,
            hydration.child_claims(),
        );
        assert!(usage.cost.usd.is_some());
    }
}

#[test]
fn legacy_parent_without_entry_ids_cannot_prove_a_fork_copy() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    // A legacy (v1) parent: usage records carry no native entry ID.
    let mut legacy = fx::call("x", None, Some("r1"), out(10_000));
    let object = legacy.as_object_mut().unwrap();
    object.remove("id");
    object.remove("parentId");
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            legacy,
        ],
    );
    // The migrated fork carries fresh IDs for the same call.
    fx::write_session(
        &root,
        "b",
        "t2_f",
        &[
            fx::header("f", Some("p"), None),
            fx::user_message("u1", None, "go"),
            fx::call("fresh", Some("u1"), None, out(10_000)),
        ],
    );
    let fork = hydrate(&root, "f");
    let usage = fork.usage.as_ref().unwrap();
    assert!(
        !usage.coverage.complete,
        "an unindexable parent proves nothing"
    );
    assert_eq!(usage.cost.usd, None);
    assert_eq!(usage.coverage.observed_requests, 0, "not new spend");
}

#[test]
fn append_to_an_earlier_continuation_member_is_folded() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let first = fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t2_p2",
        &[
            fx::header("p2", Some("p"), Some("p")),
            fx::user_message("v1", None, "more"),
            fx::call("b1", Some("v1"), Some("rb1"), out(20_000)),
        ],
    );
    let meta = discover(&root).remove("p").unwrap();
    let mut hydration = OmpHydration::new(meta, &config(&root)).unwrap();
    close(usd(&finish(&mut hydration, BIG).0), 0.30);
    fx::append(
        &first,
        &[fx::with_leaf(
            fx::aux("x1", Some("u1"), "title", out(3_000)),
            Some("a1"),
        )],
    );
    close(usd(&finish(&mut hydration, BIG).0), 0.33);
}

#[test]
fn orchestration_output_is_not_conversation_throughput() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    let usage = fx::with_orchestration(fx::usage(0, 1_600, 0, 0, 0.0), 0, 100_000, 0);
    fx::write_session(
        &root,
        "b",
        "t1_o",
        &[
            fx::header("o", None, None),
            fx::user_message("u1", None, "go"),
            fx::with_message(
                fx::call("a1", Some("u1"), Some("r1"), usage),
                "duration",
                json!(19_000),
            ),
        ],
    );
    let summary = hydrate(&root, "o");
    let rate = summary.metrics.speed.weighted_tokens_per_second.unwrap();
    assert!((rate - 84.2).abs() < 0.05, "{rate}");
    close(usd(&summary), 1.016);
}

#[test]
fn compaction_history_is_incomplete_when_the_entry_index_is_exhausted() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_c",
        &[
            fx::header("c", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(1_000)),
            fx::compaction("k1", Some("a1"), "u1"),
        ],
    );
    let meta = discover(&root).remove("c").unwrap();
    let mut small = config(&root);
    small.dedup_ledger_entries = 2;
    let mut hydration = OmpHydration::new(meta, &small).unwrap();
    let (summary, _) = finish(&mut hydration, BIG);
    assert_eq!(summary.metrics.compactions.observed, 0);
    assert!(!summary.metrics.compactions.history_complete);
    // An intact index of the same file is complete.
    assert!(hydrate(&root, "c").metrics.compactions.history_complete);
}

#[test]
fn distinct_confirmed_responses_survive_a_repeated_entry_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_d",
        &[
            fx::header("d", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
            fx::call("a1", Some("u1"), Some("r2"), out(10_000)),
        ],
    );
    let summary = hydrate(&root, "d");
    close(usd(&summary), 0.20);
    assert_eq!(
        summary.usage.as_ref().unwrap().coverage.observed_requests,
        2
    );
}

#[test]
fn a_cleared_leaf_in_the_newest_member_does_not_inherit_earlier_facts() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            fx::thinking_level("e1", Some("u1"), "high", "auto"),
            fx::with_snapshot(
                fx::call("a1", Some("e1"), Some("r1"), out(1_000)),
                90_000,
                None,
            ),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t2_p2",
        &[
            fx::header("p2", Some("p"), Some("p")),
            fx::with_leaf(fx::user_message("v1", None, "reset"), None),
        ],
    );
    let summary = hydrate(&root, "p");
    assert_eq!(summary.meta.model, None);
    assert_eq!(summary.metrics.context.used, None);
    close(usd(&summary), 0.01);
}

#[test]
fn a_fork_copying_the_second_response_of_a_repeated_entry_id_is_inherited() {
    let dir = tempfile::tempdir().unwrap();
    let root = root(&dir);
    fx::write_session(
        &root,
        "b",
        "t1_p",
        &[
            fx::header("p", None, None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r1"), out(10_000)),
            fx::call("a1", Some("u1"), Some("r2"), out(10_000)),
        ],
    );
    fx::write_session(
        &root,
        "b",
        "t2_f",
        &[
            fx::header("f", Some("p"), None),
            fx::user_message("u1", None, "go"),
            fx::call("a1", Some("u1"), Some("r2"), out(10_000)),
        ],
    );
    let parent = usd(&hydrate(&root, "p")).unwrap();
    let fork_summary = hydrate(&root, "f");
    assert_eq!(
        fork_summary
            .usage
            .as_ref()
            .unwrap()
            .coverage
            .observed_requests,
        0
    );
    let fork = usd(&fork_summary).unwrap_or(0.0);
    assert!((parent - 0.20).abs() < 1e-9);
    assert!(fork.abs() < 1e-9, "{fork}");
    assert!((parent + fork - 0.20).abs() < 1e-9);
    assert_eq!(fork_summary.usage.as_ref().unwrap().coverage.reason, None);
}
