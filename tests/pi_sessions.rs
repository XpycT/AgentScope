//! Native Pi discovery and truthful request/model/effort hydration, driven through
//! the concrete `PiCatalog`/`PiHydration` components over synthetic JSONL. No
//! ProviderReader is registered at this stage, so these tests never take a
//! test-only provider path.

mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    AggregationClass, Harness, RefreshBudget, SessionKey, SessionMetadata, SessionSummary,
    SourceKey, WarningKind,
};
use agentscope::pricing::PriceSchedule;
use agentscope::providers::pi::{
    PiCatalog, PiHydration, PiRecordError, PiRole, additive_total, categories_incomplete,
    parse_entry, parse_header, parse_pi_usage, reported_total, total_conflicts,
};
use serde_json::{Value, json};
use support::facts::price_schedule;
use support::pi as fx;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

const TINY: RefreshBudget = RefreshBudget {
    max_records: 1,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn prices() -> PriceSchedule {
    price_schedule(
        r#"[{"billing_provider":"anthropic","model":"claude-test",
             "rates":{"input":0.0,"output":10.0,"cache_read":0.0,"cache_write":0.0}},
            {"billing_provider":"acme","model":"m",
             "rates":{"input":0.0,"output":10.0,"cache_read":0.0,"cache_write":0.0}}]"#,
    )
}

fn config(root: &Path) -> Config {
    let cli = CliOverrides {
        pi_sessions: Some(root.to_path_buf()),
        ..CliOverrides::default()
    };
    let mut config = Config::load_with_env(&cli, &Env::new()).expect("valid test config");
    config.pricing = prices();
    config
}

#[derive(Default)]
struct Discovery {
    metadata: Vec<SessionMetadata>,
    warnings: Vec<agentscope::domain::Warning>,
    invalidated: Vec<SessionKey>,
    removed: Vec<SessionKey>,
    slices: usize,
}

/// Runs discovery to the end of one pass.
fn discover(root: &Path) -> Discovery {
    let mut catalog = PiCatalog::new(vec![root.to_path_buf()], 1000, 64 << 20);
    let mut found = Discovery::default();
    let mut seen: HashMap<SessionKey, SessionMetadata> = HashMap::new();
    for _ in 0..10_000 {
        let delta = catalog.step(BIG).expect("discovery");
        found.slices += 1;
        for meta in delta.sessions {
            seen.insert(meta.key.clone(), meta);
        }
        found.warnings.extend(delta.warnings);
        found.removed.extend(delta.removed);
        found.invalidated.extend(delta.invalidated);
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

fn hydrated(
    meta: &SessionMetadata,
    config: &Config,
    budget: RefreshBudget,
) -> (PiHydration, SessionSummary, usize) {
    let mut hydration = PiHydration::new(meta.clone(), config).expect("hydration");
    for slices in 1..=100_000 {
        let delta = hydration.step(budget).expect("step");
        if delta.done {
            return (hydration, delta.summary, slices);
        }
    }
    panic!("hydration did not finish");
}

fn one_session(entries: &[Value]) -> (tempfile::TempDir, SessionMetadata, Config) {
    let dir = tempfile::tempdir().expect("tempdir");
    fx::write_session(dir.path(), "proj", "session", entries);
    let config = config(dir.path());
    let found = discover(dir.path());
    assert_eq!(found.metadata.len(), 1, "exactly one session");
    let meta = found.metadata[0].clone();
    (dir, meta, config)
}

#[test]
fn owned_request_provider_effort_and_lifetime_total() {
    let request = fx::with_response(
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "ok",
            Some(fx::with_reasoning(fx::usage(100, 40, 20, 10), 15)),
        ),
        "resp-1",
    );
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "build it", None),
        request.clone(),
        // The producer repeated the identical assistant/request record.
        request,
        fx::model_change("mc1", "a1", "openai", "gpt-test"),
        fx::thinking_change("tc1", "mc1", "high"),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);

    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.total, Some(170));
    assert_eq!(usage.categories.input, Some(100));
    assert_eq!(usage.categories.output, Some(40));
    assert_eq!(usage.categories.cache_read, Some(20));
    assert_eq!(usage.categories.cache_write, Some(10));
    assert_eq!(usage.categories.reasoning, Some(15));
    assert_eq!(usage.coverage.observed_requests, 1);
    assert!(usage.coverage.complete);

    let model = summary.meta.model.as_ref().expect("latest model");
    assert_eq!(model.provider.as_deref(), Some("openai"));
    assert_eq!(model.id, "gpt-test");
    assert_eq!(model.configured_effort.as_deref(), Some("high"));
    assert_eq!(model.effort, None, "no effective effort was recorded");

    let requests = _hydration.requests();
    assert_eq!(requests.len(), 1);
    let earlier = requests[0].model.as_ref().expect("request model");
    assert_eq!(earlier.provider.as_deref(), Some("anthropic"));
    assert_eq!(earlier.id, "claude-test");
    assert_eq!(
        requests[0]
            .request
            .as_ref()
            .and_then(|request| request.id.as_deref()),
        Some("resp-1")
    );
}

#[test]
fn reasoning_is_inside_output_and_not_added_twice() {
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "ok",
            Some(fx::with_reasoning(fx::usage(100, 40, 20, 10), 15)),
        ),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.total, Some(170));
    assert_eq!(usage.categories.output, Some(40));
    assert_eq!(usage.categories.reasoning, Some(15));
    assert_eq!(
        summary
            .metrics
            .speed
            .latest
            .as_ref()
            .map(|rate| rate.output_tokens),
        None
    );
}

#[test]
fn missing_total_tokens_still_uses_the_additive_categories() {
    let mut recorded = fx::usage(100, 40, 20, 10);
    recorded
        .as_object_mut()
        .expect("usage")
        .remove("totalTokens");
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::message("a1", Some("u1"), "assistant", "ok", Some(recorded)),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.total, Some(170));
    assert_eq!(
        _hydration.requests()[0].categories.total,
        Some(170),
        "the additive categories prove the total without totalTokens"
    );
    assert!(usage.coverage.complete);
}

#[test]
fn malformed_categories_remain_unknown_and_coverage_is_partial() {
    let malformed = json!({
        "input": -5, "output": 40.5, "cacheRead": 20, "cacheWrite": 10,
        "totalTokens": 70, "reasoning": 15
    });
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::message("a1", Some("u1"), "assistant", "ok", Some(malformed)),
        // An assistant record without a usage object cannot be one request.
        fx::message("a2", Some("a1"), "assistant", "ok", None),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.coverage.observed_requests, 1);
    assert_eq!(usage.categories.input, None);
    assert_eq!(usage.categories.output, None);
    assert_eq!(usage.total, None);
    assert_eq!(usage.categories.cache_read, Some(20));
    assert_eq!(usage.categories.cache_write, Some(10));
    assert!(!usage.coverage.complete);
    assert_eq!(usage.class, AggregationClass::Partial);
}

#[test]
fn conflicting_reported_total_warns_and_marks_partial() {
    let mut recorded = fx::usage(100, 40, 20, 10);
    recorded["totalTokens"] = json!(999);
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::message("a1", Some("u1"), "assistant", "ok", Some(recorded)),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let usage = summary.usage.as_ref().expect("usage");
    // The additive counters are kept; the recorded total never redefines them.
    assert_eq!(usage.total, Some(170));
    assert!(!usage.coverage.complete);
    assert!(
        usage
            .coverage
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("total")),
        "coverage reason: {:?}",
        usage.coverage.reason
    );
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning.message.contains("total")),
        "warnings: {:?}",
        summary.warnings
    );
}

#[test]
fn no_timing_is_invented_from_gaps_or_tool_duration() {
    let entries = vec![
        fx::header("s1", None),
        fx::at(fx::message("u1", None, "user", "x", None), &fx::stamp(0)),
        fx::at(fx::tool_result("tr1", "u1", 9000), &fx::stamp(5)),
        fx::at(
            fx::message(
                "a1",
                Some("tr1"),
                "assistant",
                "ok",
                Some(fx::usage(100, 40, 20, 10)),
            ),
            &fx::stamp(10),
        ),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let timing = summary.metrics.timing;
    assert_eq!(timing.ended_at, None);
    assert_eq!(timing.elapsed_ms, None);
    assert_eq!(timing.active_ms, None);
    assert_eq!(timing.model_ms, None);
    assert!(!timing.active_complete);
    assert_eq!(summary.metrics.speed.latest, None);
    assert_eq!(summary.metrics.speed.measured_requests, 0);
    assert_eq!(summary.metrics.speed.total_requests, 1);
    let request = _hydration.requests()[0]
        .request
        .as_ref()
        .expect("request metadata");
    assert_eq!(request.duration_ms, None);
    assert_eq!(request.ttft_ms, None);
    assert_eq!(request.completed_at, None);
}

#[test]
fn recorded_assistant_duration_is_measured_model_time() {
    let assistant = fx::with_duration(
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "ok",
            Some(fx::usage(100, 40, 20, 10)),
        ),
        1_200,
    );
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        assistant,
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    assert_eq!(summary.metrics.timing.model_ms, Some(1_200.0));
    // The duration is measured model time, not a request completion or session end.
    assert_eq!(summary.metrics.timing.ended_at, None);
    assert_eq!(summary.metrics.timing.active_ms, None);
    let rate = summary
        .metrics
        .speed
        .latest
        .as_ref()
        .expect("timed request");
    assert_eq!(rate.duration_ms, 1_200.0);
    assert_eq!(rate.output_tokens, 40);
    assert_eq!(rate.completed_at, None);
    assert_eq!(rate.ttft_ms, None);
    assert!((rate.tokens_per_second - 40.0 / 1.2).abs() < 1e-9);
    assert_eq!(
        _hydration.requests()[0]
            .request
            .as_ref()
            .and_then(|request| request.duration_ms),
        Some(1_200.0)
    );
}

#[test]
fn same_model_through_another_provider_is_priced_separately() {
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "ok",
            Some(fx::usage(100, 40, 20, 10)),
        ),
        fx::with_model(
            fx::message(
                "a2",
                Some("a1"),
                "assistant",
                "ok",
                Some(fx::usage(100, 40, 20, 10)),
            ),
            "openai",
            "claude-test",
        ),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.models.len(), 2, "route is part of model identity");
    assert_eq!(usage.models[0].model.provider.as_deref(), Some("anthropic"));
    assert!(
        usage.models[0].cost.usd.is_some(),
        "the anthropic route is priced"
    );
    assert_eq!(usage.models[1].model.provider.as_deref(), Some("openai"));
    assert_eq!(
        usage.models[1].cost.usd, None,
        "the openai route is unpriced"
    );
    assert_eq!(usage.models[1].total, Some(170));
    assert_eq!(usage.coverage.priced_requests, 1);
}

#[test]
fn copied_files_with_one_header_id_are_distinct_sources() {
    let dir = tempfile::tempdir().expect("tempdir");
    // A byte-copied file is still its own source and names no fork parent. (A
    // declared but undiscovered parent makes ownership unproven instead; see
    // `pi_tree`.)
    let entries = vec![
        fx::header("same-id", None),
        fx::message("u1", None, "user", "x", None),
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "ok",
            Some(fx::usage(100, 40, 20, 10)),
        ),
    ];
    let a = fx::write_session(dir.path(), "proj", "copy-a", &entries);
    let b = fx::write_session(dir.path(), "proj", "copy-b", &entries);
    assert_ne!(a, b);
    let config = config(dir.path());
    let found = discover(dir.path());
    assert_eq!(found.metadata.len(), 2);
    assert!(
        found
            .metadata
            .iter()
            .all(|meta| meta.key.native_id == "same-id")
    );
    let mut paths: Vec<PathBuf> = found
        .metadata
        .iter()
        .filter_map(|meta| match &meta.key.source {
            SourceKey::File(path) => Some(path.clone()),
            SourceKey::DbRow { .. } => None,
        })
        .collect();
    paths.sort();
    assert_eq!(paths, vec![a, b]);

    // Each source is its own ledger; no copied request is inherited across files.
    for meta in &found.metadata {
        let (_hydration, summary, _slices) = hydrated(meta, &config, BIG);
        assert_eq!(
            summary
                .usage
                .as_ref()
                .expect("usage")
                .coverage
                .observed_requests,
            1
        );
    }
}

#[test]
fn discovery_ignores_credentials_and_keeps_unsupported_headers_visible() {
    let dir = tempfile::tempdir().expect("tempdir");
    fx::write_session(
        dir.path(),
        "proj",
        "good",
        &[
            fx::header("good-id", None),
            fx::message("u1", None, "user", "hello", None),
        ],
    );
    fx::write_session(
        dir.path(),
        "proj",
        "future",
        &[
            fx::header_version("future-id", 99),
            fx::message("u1", None, "user", "x", None),
        ],
    );
    std::fs::write(
        dir.path().join("auth.json"),
        br#"{"api_key":"sk-credential-looking"}"#,
    )
    .expect("credential decoy");
    std::fs::create_dir_all(dir.path().join("proj").join("nested")).expect("nested dir");
    std::fs::write(dir.path().join("proj").join("notes.txt"), b"not a session").expect("notes");

    let found = discover(dir.path());
    let ids: Vec<&str> = found
        .metadata
        .iter()
        .map(|meta| meta.key.native_id.as_str())
        .collect();
    assert_eq!(ids, vec!["future-id", "good-id"]);
    assert!(
        found
            .warnings
            .iter()
            .any(|warning| warning.key.kind == WarningKind::Unsupported),
        "warnings: {:?}",
        found.warnings
    );
    let config = config(dir.path());
    let future = found
        .metadata
        .iter()
        .find(|meta| meta.key.native_id == "future-id")
        .expect("future session");
    let (_hydration, summary, _slices) = hydrated(future, &config, BIG);
    assert_eq!(summary.usage, None, "an unknown version is not interpreted");
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning.key.kind == WarningKind::Unsupported)
    );
}

#[test]
fn hydration_is_bounded_and_cancel_releases_partial_work() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
    ];
    for index in 0..40 {
        entries.push(fx::with_response(
            fx::message(
                &format!("a{index}"),
                Some("u1"),
                "assistant",
                "ok",
                Some(fx::usage(100, 40, 20, 10)),
            ),
            &format!("resp-{index}"),
        ));
    }
    fx::write_session(dir.path(), "proj", "long", &entries);
    let config = config(dir.path());
    let meta = discover(dir.path()).metadata.remove(0);

    let mut hydration = PiHydration::new(meta.clone(), &config).expect("hydration");
    let first = hydration.step(TINY).expect("step");
    assert!(!first.done, "a one-record budget cannot finish the session");
    assert_eq!(hydration.retained_requests(), 0);
    hydration.cancel();
    assert_eq!(hydration.retained_requests(), 0);
    assert!(hydration.requests().is_empty());

    let mut slices = 0;
    loop {
        slices += 1;
        let delta = hydration.step(TINY).expect("step");
        if delta.done {
            assert_eq!(
                delta
                    .summary
                    .usage
                    .as_ref()
                    .expect("usage")
                    .coverage
                    .observed_requests,
                40
            );
            break;
        }
        assert!(slices < 10_000);
    }
    assert!(slices > 40, "bounded slices, not one unbounded read");
    assert_eq!(hydration.requests().len(), 40);
}

#[test]
fn entry_parser_rejects_malformed_identity_and_keeps_unknown_records() {
    assert_eq!(
        parse_entry(&json!({"type": "message"}), 0, 0),
        Err(PiRecordError::Malformed)
    );
    assert_eq!(
        parse_entry(&json!({"type": "session", "id": "s"}), 0, 0),
        Err(PiRecordError::Unsupported)
    );
    let unknown = parse_entry(
        &json!({"type": "extension_thing", "id": "x1", "parentId": null}),
        3,
        64,
    )
    .expect("unknown record keeps its identity");
    assert_eq!(unknown.ordinal, 3);
    assert_eq!(unknown.offset, 64);
    assert!(matches!(
        unknown.kind,
        agentscope::providers::pi::PiEntryKind::Unknown { .. }
    ));
    let assistant = parse_entry(
        &fx::message("a1", None, "assistant", "ok", Some(fx::usage(1, 2, 3, 4))),
        1,
        0,
    )
    .expect("message");
    assert!(matches!(
        assistant.kind,
        agentscope::providers::pi::PiEntryKind::Message {
            role: PiRole::Assistant,
            usage: true,
            ..
        }
    ));
}

#[test]
fn usage_helpers_agree_on_additive_totals() {
    let recorded = fx::with_reasoning(fx::usage(100, 40, 20, 10), 15);
    assert_eq!(reported_total(&recorded), Some(170));
    let usage = fx::entry(
        "u1",
        None,
        "message",
        json!({
            "message": {"role": "assistant", "content": [], "provider": "acme", "model": "m",
                        "timestamp": fx::STAMP_MS, "usage": recorded}
        }),
    );
    let message = usage["message"].clone();
    let observation = parse_pi_usage(
        &message,
        Some(agentscope::domain::ModelDetail {
            id: "m".into(),
            provider: Some("acme".into()),
            variant: None,
            effort: None,
            configured_effort: None,
        }),
        agentscope::domain::Provenance {
            harness: Harness::Pi,
            source: SourceKey::File(PathBuf::from("synthetic.jsonl")),
            locator: agentscope::domain::SourceLocator {
                session: SessionKey {
                    harness: Harness::Pi,
                    native_id: "s1".into(),
                    source: SourceKey::File(PathBuf::from("synthetic.jsonl")),
                },
                kind: agentscope::domain::LocatorKind::FileOffset {
                    path: PathBuf::from("synthetic.jsonl"),
                    offset: 0,
                },
                generation: Some(1),
            },
            record_type: "message".into(),
            harness_version: Some("v3".into()),
        },
    );
    assert_eq!(additive_total(&observation.categories), Some(170));
    assert!(!categories_incomplete(&observation.categories));
    assert!(!total_conflicts(&message, &observation.categories));

    let mut conflicting = recorded.clone();
    conflicting["totalTokens"] = json!(171);
    assert!(total_conflicts(&conflicting, &observation.categories));
    assert_eq!(
        parse_header(&fx::header("s1", None))
            .expect("header")
            .version,
        Some(3)
    );
}

#[test]
fn repeated_entry_id_without_response_id_counts_once() {
    let assistant = fx::message(
        "a1",
        Some("u1"),
        "assistant",
        "ok",
        Some(fx::usage(100, 40, 20, 10)),
    );
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        assistant.clone(),
        assistant,
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.coverage.observed_requests, 1);
    assert_eq!(usage.total, Some(170));
    assert_eq!(
        _hydration.requests()[0]
            .request
            .as_ref()
            .and_then(|request| request.id.as_deref()),
        Some("a1")
    );
}

#[test]
fn recorded_cost_origin_is_retained_truthfully() {
    let reported = fx::with_cost(fx::usage(100, 40, 20, 10), 0.5, true);
    let saved = fx::with_cost(fx::usage(100, 40, 20, 10), 0.25, false);
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::message("a1", Some("u1"), "assistant", "ok", Some(reported)),
        fx::with_response(
            fx::message("a2", Some("a1"), "assistant", "ok", Some(saved)),
            "resp-2",
        ),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let requests = _hydration.requests();
    let first = &requests[0].money;
    assert_eq!(
        first.reported_api.as_ref().map(|fact| fact.usd),
        Some(0.5),
        "a provider-reported total is reported money"
    );
    assert!(first.saved_estimate.is_none());
    let second = &requests[1].money;
    assert!(second.reported_api.is_none());
    assert_eq!(
        second.saved_estimate.as_ref().map(|fact| fact.usd),
        Some(0.25),
        "an unflagged Pi cost is only the producer's catalog estimate"
    );
    // Mixed origins cannot become one session-wide reported/saved amount.
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.reported_cost_usd, None);
    assert_eq!(usage.saved_estimate_usd, None);
}

#[test]
fn unusable_recorded_cost_is_not_retained_and_is_warned() {
    let mut recorded = fx::usage(100, 40, 20, 10);
    recorded["cost"] = json!({"total": -3.0, "input": 0.0, "output": 0.0,
                              "cacheRead": 0.0, "cacheWrite": 0.0});
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::message("a1", Some("u1"), "assistant", "ok", Some(recorded)),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let money = &_hydration.requests()[0].money;
    assert_eq!(money.reported_api, None);
    assert_eq!(money.saved_estimate, None);
    assert_eq!(money.unverified_usd, None);
    assert!(
        summary
            .warnings
            .iter()
            .any(|warning| warning.message.contains("cost")),
        "warnings: {:?}",
        summary.warnings
    );
}

#[test]
fn assistant_with_null_usage_marks_coverage_partial() {
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        // A malformed assistant record whose usage cannot be read.
        fx::message("a1", Some("u1"), "assistant", "lost", Some(Value::Null)),
        fx::with_response(
            fx::message(
                "a2",
                Some("u1"),
                "assistant",
                "ok",
                Some(fx::usage(100, 40, 20, 10)),
            ),
            "resp-2",
        ),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.coverage.observed_requests, 1);
    assert_eq!(usage.total, Some(170));
    assert!(
        !usage.coverage.complete,
        "dropping a usage-bearing record cannot leave complete coverage"
    );
    assert!(
        usage
            .coverage
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("usage")),
        "coverage reason: {:?}",
        usage.coverage.reason
    );
}

#[test]
fn invalid_and_oversized_records_make_coverage_partial() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut body = Vec::new();
    for entry in [
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::with_response(
            fx::message(
                "a1",
                Some("u1"),
                "assistant",
                "ok",
                Some(fx::usage(100, 40, 20, 10)),
            ),
            "resp-1",
        ),
    ] {
        body.extend_from_slice(entry.to_string().as_bytes());
        body.push(b'\n');
    }
    // A torn JSON line and a record well past the 1 MiB source record cap.
    body.extend_from_slice(b"{not-json\n");
    let oversized = format!(
        r#"{{"type":"message","id":"big","parentId":"u1","message":{{"role":"assistant","usage":{{"input":1}}}},"pad":"{}"}}"#,
        "x".repeat(1_200_000)
    );
    body.extend_from_slice(oversized.as_bytes());
    body.push(b'\n');
    body.extend_from_slice(
        fx::with_response(
            fx::message(
                "a2",
                Some("u1"),
                "assistant",
                "ok",
                Some(fx::usage(100, 40, 20, 10)),
            ),
            "resp-2",
        )
        .to_string()
        .as_bytes(),
    );
    body.push(b'\n');
    fx::write_raw(dir.path(), "proj", "session", &body);
    let config = config(dir.path());
    let meta = discover(dir.path()).metadata.remove(0);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.coverage.observed_requests, 2);
    assert!(!usage.coverage.complete);
    assert!(
        usage
            .coverage
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("unreadable") || reason.contains("record")),
        "coverage reason: {:?}",
        usage.coverage.reason
    );
}

#[test]
fn short_file_keeps_its_leading_session_info_name_as_title() {
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "short prompt", None),
        fx::session_info("si1", "u1", "old name"),
        fx::session_info("si2", "si1", "Refactor auth module"),
    ];
    // The whole fixture fits the 4 KiB prefix, so discovery has no tail to read.
    let (_dir, meta, _config) = one_session(&entries);
    assert_eq!(meta.title, "Refactor auth module");
    assert!(
        !meta.title_derived,
        "a recorded name is not a derived title"
    );
}

#[test]
fn evicted_published_source_is_reported_removed() {
    let dir = tempfile::tempdir().expect("tempdir");
    fx::write_session(
        dir.path(),
        "proj",
        "a-old",
        &[
            fx::at(fx::header("old-id", None), &fx::stamp(0)),
            fx::at(fx::message("u1", None, "user", "old", None), &fx::stamp(0)),
        ],
    );
    let mut catalog = PiCatalog::new(vec![dir.path().to_path_buf()], 1, 64 << 20);

    fn pass(catalog: &mut PiCatalog) -> (Vec<SessionMetadata>, Vec<SessionKey>) {
        let mut sessions = Vec::new();
        let mut removed = Vec::new();
        loop {
            let delta = catalog.step(BIG).expect("discovery");
            sessions.extend(delta.sessions);
            removed.extend(delta.removed);
            if delta.scan.done {
                return (sessions, removed);
            }
        }
    }

    let (first, first_removed) = pass(&mut catalog);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].key.native_id, "old-id");
    assert!(first_removed.is_empty());

    fx::write_session(
        dir.path(),
        "proj",
        "b-new",
        &[
            fx::at(fx::header("new-id", None), &fx::stamp(10)),
            fx::at(fx::message("u1", None, "user", "new", None), &fx::stamp(10)),
        ],
    );
    let (second, removed) = pass(&mut catalog);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].key.native_id, "new-id");
    assert_eq!(
        removed.len(),
        1,
        "the evicted published source is withdrawn"
    );
    assert_eq!(removed[0].native_id, "old-id");
}

#[test]
fn summarization_usage_keeps_categories_without_a_model_route() {
    let entries = vec![
        fx::header("s1", None),
        fx::message("u1", None, "user", "x", None),
        fx::message(
            "a1",
            Some("u1"),
            "assistant",
            "ok",
            Some(fx::usage(100, 40, 20, 10)),
        ),
        // A summary with a recorded catalog cost, and one with no money at all.
        fx::compaction(
            "c1",
            "a1",
            Some(fx::with_cost(fx::usage(4, 2, 0, 0), 0.01, false)),
        ),
        fx::compaction("c2", "c1", Some(fx::usage(4, 2, 0, 0))),
    ];
    let (_dir, meta, config) = one_session(&entries);
    let (_hydration, summary, _slices) = hydrated(&meta, &config, BIG);
    let requests = _hydration.requests();
    assert_eq!(requests.len(), 3);
    for compaction in &requests[1..] {
        assert_eq!(
            compaction.request.as_ref().map(|request| &request.purpose),
            Some(&agentscope::domain::RequestPurpose::Compaction)
        );
        assert_eq!(
            compaction.model, None,
            "a summary entry persists no model route, so it is not priced as the conversation model"
        );
        assert_eq!(compaction.categories.input, Some(4));
        assert_eq!(compaction.categories.output, Some(2));
        assert_eq!(compaction.categories.total, Some(6));
    }
    assert_eq!(
        requests[1]
            .money
            .saved_estimate
            .as_ref()
            .map(|fact| fact.usd),
        Some(0.01),
        "the recorded money is retained even though the route is unknown"
    );
    assert_eq!(
        requests[2].money,
        agentscope::domain::RecordedMoney::default()
    );

    // Route pricing never falls back to the preceding conversation model. A
    // recorded saved estimate is still a money fact; without one the cost is Unknown.
    let policy = agentscope::pricing::ReferencePolicy::Disabled;
    let saved = agentscope::pricing::estimate_cost(&requests[1], &config.pricing, policy);
    assert_eq!(saved.usd, Some(0.01));
    assert_eq!(
        saved.source,
        Some(agentscope::domain::MoneySource::SavedCatalogEstimate)
    );
    let unpriced = agentscope::pricing::estimate_cost(&requests[2], &config.pricing, policy);
    assert_eq!(unpriced.usd, None);

    let usage = summary.usage.as_ref().expect("usage");
    assert_eq!(usage.total, Some(182));
    assert_eq!(
        usage.models.len(),
        1,
        "only the conversation route is modeled"
    );
    assert_eq!(usage.models[0].total, Some(170));
    assert_eq!(usage.coverage.observed_requests, 3);
    assert_eq!(usage.coverage.priced_requests, 2);
    assert!(
        !usage.coverage.complete,
        "a summary without a route or recorded cost stays unpriced"
    );
    assert_eq!(usage.cost.usd, None);
}
