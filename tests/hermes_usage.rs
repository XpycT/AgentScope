//! Hermes aggregate accounting: main/aux alternatives, cost status, request-
//! dependent partial pricing, honest api-call coverage, closure and resumable
//! hydration cancellation.

mod support;

use std::path::Path;

use agentscope::domain::{
    ActivityStamp, AggregationClass, Harness, Lifecycle, Lineage, ModelDetail, RefreshBudget,
    ReleaseState, SessionKey, SessionMetadata, SourceKey,
};
use agentscope::pricing::PriceSchedule;
use agentscope::providers::hermes::{
    HermesCostStatus, HermesHydration, HermesModelTotals, HermesMoney, HermesRowId,
    HermesSessionTotals, HermesTokens, HermesUsageReconciler, HermesUsageScope,
};
use agentscope::providers::{HydrationDelta, ProviderError, ReadJobId};
use support::NOW;
use support::hermes::*;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};
const TINY: RefreshBudget = RefreshBudget {
    max_records: 1,
    max_bytes: 1_000_000,
    max_millis: 60_000,
};

fn key(db: &Path, id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::Hermes,
        native_id: id.to_owned(),
        source: SourceKey::DbRow {
            database: db.to_path_buf(),
            table: "sessions".to_owned(),
            id: id.to_owned(),
        },
    }
}

fn metadata(db: &Path, id: &str, model: &str, provider: &str) -> SessionMetadata {
    SessionMetadata {
        key: key(db, id),
        title: id.to_owned(),
        title_derived: true,
        project: None,
        cwd: None,
        model: Some(ModelDetail {
            id: model.to_owned(),
            provider: Some(provider.to_owned()),
            variant: None,
            effort: None,
            configured_effort: None,
        }),
        origins: Vec::new(),
        created_at: None,
        activity: ActivityStamp::default(),
        lineage: Lineage::default(),
        members: Vec::new(),
        archived: false,
        released: ReleaseState::Unknown,
        source_available: true,
        harness_version: None,
    }
}

fn tokens(input: u64, output: u64, read: u64, write: u64, reasoning: u64) -> HermesTokens {
    HermesTokens {
        input: Some(input),
        output: Some(output),
        cache_read: Some(read),
        cache_write: Some(write),
        reasoning: Some(reasoning),
    }
}

fn money(status: HermesCostStatus, estimated: Option<f64>, actual: Option<f64>) -> HermesMoney {
    HermesMoney {
        status,
        estimated_usd: estimated,
        actual_usd: actual,
    }
}

#[allow(clippy::too_many_arguments)]
fn session(
    db: &Path,
    id: &str,
    model: &str,
    provider: &str,
    calls: Option<u64>,
    tokens: HermesTokens,
    money: HermesMoney,
) -> HermesSessionTotals {
    HermesSessionTotals {
        key: key(db, id),
        model: Some(ModelDetail {
            id: model.to_owned(),
            provider: Some(provider.to_owned()),
            variant: None,
            effort: None,
            configured_effort: None,
        }),
        api_call_count: calls,
        tokens,
        money,
    }
}

#[allow(clippy::too_many_arguments)]
fn row(
    db: &Path,
    id: &str,
    model: &str,
    provider: &str,
    task: &str,
    calls: Option<u64>,
    tokens: HermesTokens,
    money: HermesMoney,
) -> HermesModelTotals {
    HermesModelTotals {
        row: HermesRowId(0),
        key: key(db, id),
        model: Some(ModelDetail {
            id: model.to_owned(),
            provider: Some(provider.to_owned()),
            variant: None,
            effort: None,
            configured_effort: None,
        }),
        task: task.to_owned(),
        api_call_count: calls,
        tokens,
        money,
    }
}

/// A single incomplete main row proves the aggregate's model attribution without
/// duplicating the session aggregate (one of two calls is attributed).
fn main_row(
    db: &Path,
    id: &str,
    model: &str,
    provider: &str,
    status: HermesCostStatus,
    estimated: Option<f64>,
    actual: Option<f64>,
) -> HermesModelTotals {
    row(
        db,
        id,
        model,
        provider,
        "",
        Some(1),
        tokens(1_000, 500, 0, 0, 0),
        money(status, estimated, actual),
    )
}

fn linear_prices() -> PriceSchedule {
    PriceSchedule::from_json(
        r#"{
          "schema_version": 1,
          "revision": "hermes-test",
          "checked_at": "2026-10-30",
          "cards": [{
            "billing_provider": "test-provider",
            "model": "test-model",
            "rates": {"input": 1.0, "output": 2.0, "cache_read": 0.5, "cache_write": 1.5}
          }]
        }"#,
    )
    .expect("linear test prices")
}

fn threshold_prices() -> PriceSchedule {
    PriceSchedule::from_json(
        r#"{
          "schema_version": 1,
          "revision": "hermes-test",
          "checked_at": "2026-10-30",
          "cards": [{
            "billing_provider": "test-provider",
            "model": "test-model",
            "rates": {"input": 1.0, "output": 2.0},
            "threshold": {"input_tokens": 1000, "above": {"input": 2.0, "output": 4.0}}
          }]
        }"#,
    )
    .expect("threshold test prices")
}

fn tariff_prices() -> PriceSchedule {
    PriceSchedule::from_json(
        r#"{
          "schema_version": 1,
          "revision": "hermes-test",
          "checked_at": "2026-10-30",
          "cards": [{
            "billing_provider": "test-provider",
            "model": "test-model",
            "rates": {"input": 1.0, "output": 2.0},
            "tariff": [{
              "name": "all-day",
              "days": [0, 1, 2, 3, 4, 5, 6],
              "start_minute": 0,
              "end_minute": 1440,
              "rates": {"input": 0.5, "output": 1.0}
            }]
          }]
        }"#,
    )
    .expect("tariff test prices")
}

fn tiered_prices() -> PriceSchedule {
    PriceSchedule::from_json(
        r#"{
          "schema_version": 1,
          "revision": "hermes-test",
          "checked_at": "2026-10-30",
          "cards": [{
            "billing_provider": "test-provider",
            "model": "test-model",
            "rates": {"input": 1.0, "output": 2.0},
            "service_multipliers": {"fast": 2.0}
          }]
        }"#,
    )
    .expect("tiered test prices")
}

fn approx(left: Option<f64>, right: f64) {
    let value = left.expect("cost is present");
    assert!(
        (value - right).abs() < 1e-9,
        "expected {right}, got {value}"
    );
}

fn hydrate(db: &Path, id: &str, budget: RefreshBudget) -> HydrationDelta {
    let mut hydration = HermesHydration::new(db, linear_prices());
    for _ in 0..100_000 {
        let delta = hydration
            .step(ReadJobId(1), &key(db, id), NOW, budget)
            .expect("hydration slice");
        if delta.done {
            return delta;
        }
    }
    panic!("hydration never completed");
}

#[test]
fn main_rows_and_excluded_auxiliary_are_added_once_and_quoted_child_money_is_ignored() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(&writer, "p", None, serde_json::json!({}), 1_000.0);
    insert_session(&writer, "p2", Some("p"), serde_json::json!({}), 1_100.0);
    insert_session(
        &writer,
        "c",
        Some("p"),
        serde_json::json!({"_delegate_from": "p"}),
        1_200.0,
    );
    set_ended(&writer, "p", 1_050.0, "compression");

    // Parent main estimate 0.10 duplicates its complete main model ledger.
    set_session_usage(
        &writer,
        "p",
        [1_000, 500, 0, 0, 100],
        2,
        0.10,
        0.0,
        Some("estimated"),
    );
    insert_usage(
        &writer,
        "p",
        "test-model",
        "test-provider",
        "",
        [1_000, 500, 0, 0, 100],
        0.10,
        "estimated",
    );
    set_usage_usage(
        &writer,
        "p",
        "test-model",
        "",
        2,
        0.10,
        0.0,
        Some("estimated"),
    );
    // Auxiliary compression usage lives only in the per-model ledger.
    insert_usage(
        &writer,
        "p",
        "test-model",
        "test-provider",
        "compression",
        [100, 20, 0, 0, 0],
        0.02,
        "estimated",
    );
    // The child's own direct spend, quoted inside a parent tool result.
    set_session_usage(
        &writer,
        "c",
        [4_000, 1_000, 0, 0, 0],
        1,
        0.20,
        0.0,
        Some("estimated"),
    );
    insert_usage(
        &writer,
        "c",
        "test-model",
        "test-provider",
        "",
        [4_000, 1_000, 0, 0, 0],
        0.20,
        "estimated",
    );
    set_usage_usage(
        &writer,
        "c",
        "test-model",
        "",
        1,
        0.20,
        0.0,
        Some("estimated"),
    );
    insert_message(
        &writer,
        "p",
        "tool",
        "delegate finished: child cost $0.20",
        1_060.0,
    );

    let parent = hydrate(&db, "p2", TINY).summary;
    approx(parent.usage.as_ref().unwrap().cost.usd, 0.12);
    assert_eq!(parent.usage.as_ref().unwrap().coverage.observed_requests, 3);
    let child = hydrate(&db, "c", TINY).summary;
    approx(child.usage.as_ref().unwrap().cost.usd, 0.20);
    let family = parent.usage.as_ref().unwrap().cost.usd.unwrap()
        + child.usage.as_ref().unwrap().cost.usd.unwrap();
    assert!((family - 0.32).abs() < 1e-9);
    assert!((family - 0.42).abs() > 1e-9);
    assert!((family - 0.52).abs() > 1e-9);
}

#[test]
fn incomplete_main_rows_fall_back_to_the_session_aggregate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let meta = metadata(&db, "p", "test-model", "test-provider");
    let session = session(
        &db,
        "p",
        "test-model",
        "test-provider",
        Some(3),
        tokens(1_000, 500, 0, 0, 100),
        money(HermesCostStatus::Estimated, Some(0.10), Some(0.0)),
    );
    let rows = vec![
        // Only one of three main calls is attributed: the ledger is incomplete.
        row(
            &db,
            "p",
            "test-model",
            "test-provider",
            "",
            Some(1),
            tokens(1_000, 500, 0, 0, 100),
            money(HermesCostStatus::Estimated, Some(0.10), Some(0.0)),
        ),
        row(
            &db,
            "p",
            "test-model",
            "test-provider",
            "compression",
            Some(1),
            tokens(100, 20, 0, 0, 0),
            money(HermesCostStatus::Estimated, Some(0.02), Some(0.0)),
        ),
    ];
    let prices = linear_prices();
    let mut reconciler = HermesUsageReconciler::new(meta, &prices);
    reconciler.observe_segment(&session, &rows);
    let report = reconciler.finish_report();
    let summary = report.summary;
    approx(summary.cost.usd, 0.12);
    assert_eq!(summary.coverage.observed_requests, 4);
    assert!(
        summary
            .cost
            .note
            .as_deref()
            .is_some_and(|note| note.contains("session aggregate"))
    );
    assert!(
        report
            .contributions
            .iter()
            .any(|contribution| contribution.scope == HermesUsageScope::MainAggregate)
    );
    assert!(
        !report
            .contributions
            .iter()
            .any(|contribution| contribution.scope == HermesUsageScope::MainRows)
    );
    let aux = report
        .contributions
        .iter()
        .find(|contribution| contribution.scope == HermesUsageScope::Auxiliary)
        .expect("auxiliary contribution");
    assert_eq!(aux.task, "compression");
    assert_eq!(aux.api_call_count, Some(1));
}

#[test]
fn cost_status_actual_estimated_included_and_unknown_stay_distinct() {
    let db = Path::new("state.db");
    let prices = linear_prices();
    let meta = metadata(db, "s", "test-model", "test-provider");

    let mut actual = HermesUsageReconciler::new(meta.clone(), &prices);
    actual.observe_segment(
        &session(
            db,
            "s",
            "test-model",
            "test-provider",
            Some(1),
            tokens(1_000, 500, 0, 0, 0),
            money(HermesCostStatus::Actual, Some(0.0), Some(0.30)),
        ),
        &[main_row(
            db,
            "s",
            "test-model",
            "test-provider",
            HermesCostStatus::Actual,
            Some(0.0),
            Some(0.0),
        )],
    );
    let actual = actual.finish();
    approx(actual.cost.usd, 0.30);
    assert_eq!(actual.cost.class, AggregationClass::Reported);
    assert_eq!(actual.reported_cost_usd, Some(0.30));

    let mut estimated = HermesUsageReconciler::new(meta.clone(), &prices);
    estimated.observe_segment(
        &session(
            db,
            "s",
            "test-model",
            "test-provider",
            Some(1),
            tokens(1_000, 500, 0, 0, 0),
            money(HermesCostStatus::Estimated, Some(0.25), Some(0.0)),
        ),
        &[main_row(
            db,
            "s",
            "test-model",
            "test-provider",
            HermesCostStatus::Estimated,
            Some(0.0),
            Some(0.0),
        )],
    );
    let estimated = estimated.finish();
    approx(estimated.cost.usd, 0.25);
    assert_eq!(estimated.cost.class, AggregationClass::Estimated);
    assert_eq!(estimated.saved_estimate_usd, Some(0.25));

    // Included zero is a subscription charge. The API-equivalent value is priced
    // from the linear card, not turned into a free call.
    let mut included = HermesUsageReconciler::new(meta.clone(), &prices);
    included.observe_segment(
        &session(
            db,
            "s",
            "test-model",
            "test-provider",
            Some(1),
            tokens(1_000, 500, 0, 0, 0),
            money(HermesCostStatus::Included, Some(0.0), Some(0.0)),
        ),
        &[main_row(
            db,
            "s",
            "test-model",
            "test-provider",
            HermesCostStatus::Included,
            Some(0.0),
            Some(0.0),
        )],
    );
    let included = included.finish();
    let included_cost = included.cost.usd.expect("API-equivalent is known");
    assert!(included_cost > 0.0);
    assert_eq!(included.cost.class, AggregationClass::Estimated);
    assert_eq!(included.reported_cost_usd, Some(0.0));
    assert!(
        included
            .cost
            .note
            .as_deref()
            .is_some_and(|note| note.contains("reported no charge"))
    );

    // Unknown/missing status with the default 0 must be re-estimated, not free.
    let mut unknown = HermesUsageReconciler::new(meta.clone(), &prices);
    unknown.observe_segment(
        &session(
            db,
            "s",
            "test-model",
            "test-provider",
            Some(1),
            tokens(1_000, 500, 0, 0, 0),
            money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
        ),
        &[main_row(
            db,
            "s",
            "test-model",
            "test-provider",
            HermesCostStatus::Unknown,
            Some(0.0),
            Some(0.0),
        )],
    );
    let unknown = unknown.finish();
    let unknown_cost = unknown.cost.usd.expect("linear price applies");
    assert!(unknown_cost > 0.0);
    assert_eq!(unknown.reported_cost_usd, None);
    assert_eq!(unknown.saved_estimate_usd, None);

    // No price card: the amount stays Unknown, never the default zero.
    let mut unpriced =
        HermesUsageReconciler::new(metadata(db, "s", "unlisted", "test-provider"), &prices);
    unpriced.observe_segment(
        &session(
            db,
            "s",
            "unlisted",
            "test-provider",
            Some(1),
            tokens(1_000, 500, 0, 0, 0),
            money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
        ),
        &[main_row(
            db,
            "s",
            "unlisted",
            "test-provider",
            HermesCostStatus::Unknown,
            Some(0.0),
            Some(0.0),
        )],
    );
    let unpriced = unpriced.finish();
    assert_eq!(unpriced.cost.usd, None);
    assert_ne!(unpriced.cost.class, AggregationClass::Reported);
    assert_ne!(unpriced.cost.class, AggregationClass::Estimated);
}

#[test]
fn request_dependent_pricing_is_partial_and_never_selects_a_lifetime_tier() {
    let db = Path::new("state.db");
    let meta = metadata(db, "s", "test-model", "test-provider");
    let session = session(
        db,
        "s",
        "test-model",
        "test-provider",
        Some(1),
        tokens(500_000, 1_000, 0, 0, 0),
        money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
    );
    let matching = row(
        db,
        "s",
        "test-model",
        "test-provider",
        "",
        Some(1),
        tokens(500_000, 1_000, 0, 0, 0),
        money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
    );

    let threshold_prices = threshold_prices();
    let mut threshold = HermesUsageReconciler::new(meta.clone(), &threshold_prices);
    threshold.observe_segment(&session, std::slice::from_ref(&matching));
    let threshold = threshold.finish();
    assert_eq!(threshold.cost.usd, None);
    assert_eq!(threshold.cost.class, AggregationClass::Partial);
    assert!(
        threshold
            .cost
            .note
            .as_deref()
            .is_some_and(|note| note.contains("request-length"))
    );

    let tiered_prices = tiered_prices();
    let mut tiered = HermesUsageReconciler::new(meta.clone(), &tiered_prices);
    tiered.observe_segment(&session, std::slice::from_ref(&matching));
    let tiered = tiered.finish();
    assert_eq!(tiered.cost.usd, None);
    assert_eq!(tiered.cost.class, AggregationClass::Partial);
    assert!(tiered.cost.known_subtotal > 0.0);
    assert!(
        tiered
            .cost
            .note
            .as_deref()
            .is_some_and(|note| note.contains("service-tier"))
    );

    let tariff_prices = tariff_prices();
    let mut tariff = HermesUsageReconciler::new(meta, &tariff_prices);
    tariff.observe_segment(&session, std::slice::from_ref(&matching));
    let tariff = tariff.finish();
    assert_eq!(tariff.cost.usd, None);
    assert_eq!(tariff.cost.class, AggregationClass::Partial);
    assert!(
        tariff
            .cost
            .note
            .as_deref()
            .is_some_and(|note| note.contains("time-of-day"))
    );
}

#[test]
fn mixed_model_incomplete_rows_never_reprice_or_attribute_the_aggregate() {
    let db = Path::new("state.db");
    let prices = linear_prices();
    let meta = metadata(db, "s", "model-a", "test-provider");
    let known_session = session(
        db,
        "s",
        "model-a",
        "test-provider",
        Some(3),
        tokens(3_000, 1_000, 0, 0, 0),
        money(HermesCostStatus::Actual, Some(0.0), Some(0.30)),
    );
    let mut rows = vec![
        row(
            db,
            "s",
            "model-a",
            "test-provider",
            "",
            Some(1),
            tokens(1_000, 400, 0, 0, 0),
            money(HermesCostStatus::Actual, Some(0.0), Some(0.30)),
        ),
        row(
            db,
            "s",
            "model-b",
            "test-provider",
            "",
            Some(1),
            tokens(2_000, 600, 0, 0, 0),
            money(HermesCostStatus::Actual, Some(0.0), Some(0.0)),
        ),
    ];
    rows[0].row = HermesRowId(1);
    rows[1].row = HermesRowId(2);
    let mut reconciler = HermesUsageReconciler::new(meta.clone(), &prices);
    reconciler.observe_segment(&known_session, &rows);
    let summary = reconciler.finish();
    // Preserved actual aggregate money survives, but no single model may claim it.
    approx(summary.cost.usd, 0.30);
    assert_eq!(summary.cost.class, AggregationClass::Reported);
    assert!(!summary.coverage.complete);
    assert!(
        summary
            .coverage
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("unproven"))
    );
    assert!(summary.models.is_empty());

    // Unknown money on mixed routes cannot be repriced with a latest model.
    let unknown_session = session(
        db,
        "s",
        "model-a",
        "test-provider",
        Some(3),
        tokens(3_000, 1_000, 0, 0, 0),
        money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
    );
    for row in &mut rows {
        row.money = money(HermesCostStatus::Unknown, Some(0.0), Some(0.0));
    }
    let mut reconciler = HermesUsageReconciler::new(meta, &prices);
    reconciler.observe_segment(&unknown_session, &rows);
    let summary = reconciler.finish();
    assert_eq!(summary.cost.usd, None);
    assert_ne!(summary.cost.class, AggregationClass::Reported);
    assert_ne!(summary.cost.class, AggregationClass::Estimated);
    assert!(
        summary
            .cost
            .note
            .as_deref()
            .is_some_and(|note| note.contains("model attribution is unproven"))
    );
}

#[test]
fn a_single_attributed_model_does_not_own_an_incomplete_aggregate() {
    let db = Path::new("state.db");
    let prices = linear_prices();
    let meta = metadata(db, "s", "model-a", "test-provider");
    let known_session = session(
        db,
        "s",
        "model-a",
        "test-provider",
        Some(3),
        tokens(3_000, 1_000, 0, 0, 0),
        money(HermesCostStatus::Actual, Some(0.0), Some(0.30)),
    );
    // Only one of three calls is attributed, even though it names the same model
    // as the session row. That is not proof the residual calls used it.
    let mut residual = row(
        db,
        "s",
        "model-a",
        "test-provider",
        "",
        Some(1),
        tokens(1_000, 400, 0, 0, 0),
        money(HermesCostStatus::Actual, Some(0.0), Some(0.0)),
    );
    residual.row = HermesRowId(11);
    let mut reconciler = HermesUsageReconciler::new(meta.clone(), &prices);
    reconciler.observe_segment(&known_session, std::slice::from_ref(&residual));
    let summary = reconciler.finish();
    // Authorized aggregate money survives; model allocation/repricing does not.
    approx(summary.cost.usd, 0.30);
    assert_eq!(summary.cost.class, AggregationClass::Reported);
    assert!(!summary.coverage.complete);
    assert!(
        summary
            .coverage
            .reason
            .as_deref()
            .is_some_and(|reason| reason.contains("unproven"))
    );
    assert!(summary.models.is_empty());

    let unknown_session = session(
        db,
        "s",
        "model-a",
        "test-provider",
        Some(3),
        tokens(3_000, 1_000, 0, 0, 0),
        money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
    );
    residual.money = money(HermesCostStatus::Unknown, Some(0.0), Some(0.0));
    let mut reconciler = HermesUsageReconciler::new(meta, &prices);
    reconciler.observe_segment(&unknown_session, std::slice::from_ref(&residual));
    let summary = reconciler.finish();
    assert_eq!(summary.cost.usd, None);
    assert!(
        summary
            .cost
            .note
            .as_deref()
            .is_some_and(|note| note.contains("model attribution is unproven"))
    );
}

#[test]
fn auxiliary_rows_are_deduplicated_by_native_row_identity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(&writer, "p", None, serde_json::json!({}), 1_000.0);
    set_session_usage(
        &writer,
        "p",
        [1_000, 500, 0, 0, 0],
        2,
        0.10,
        0.0,
        Some("estimated"),
    );
    insert_usage(
        &writer,
        "p",
        "test-model",
        "test-provider",
        "",
        [1_000, 500, 0, 0, 0],
        0.10,
        "estimated",
    );
    set_usage_usage(
        &writer,
        "p",
        "test-model",
        "",
        2,
        0.10,
        0.0,
        Some("estimated"),
    );
    insert_usage(
        &writer,
        "p",
        "test-model",
        "test-provider",
        "compression",
        [100, 20, 0, 0, 0],
        0.01,
        "estimated",
    );
    // Same visible session/model/provider/task, different native routing columns:
    // both rows are legitimate and must be added.
    insert_usage_routed(
        &writer,
        "p",
        "test-model",
        "test-provider",
        "https://secret.invalid",
        "subscription",
        "compression",
        [200, 40, 0, 0, 0],
        0.01,
        "estimated",
    );

    let summary = hydrate(&db, "p", BIG).summary;
    approx(summary.usage.as_ref().unwrap().cost.usd, 0.12);
    assert!(!format!("{summary:?}").contains("secret.invalid"));

    // Re-observing the exact same native row does not add it twice.
    let prices = linear_prices();
    let aux = {
        let mut row = row(
            Path::new("state.db"),
            "p",
            "test-model",
            "test-provider",
            "compression",
            Some(1),
            tokens(100, 20, 0, 0, 0),
            money(HermesCostStatus::Estimated, Some(0.01), Some(0.0)),
        );
        row.row = HermesRowId(42);
        row
    };
    let mut reconciler = HermesUsageReconciler::new(
        metadata(Path::new("state.db"), "p", "test-model", "test-provider"),
        &prices,
    );
    reconciler.observe_segment(
        &session(
            Path::new("state.db"),
            "p",
            "test-model",
            "test-provider",
            Some(0),
            tokens(0, 0, 0, 0, 0),
            money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
        ),
        std::slice::from_ref(&aux),
    );
    reconciler.observe_segment(
        &session(
            Path::new("state.db"),
            "p",
            "test-model",
            "test-provider",
            Some(0),
            tokens(0, 0, 0, 0, 0),
            money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
        ),
        std::slice::from_ref(&aux),
    );
    let report = reconciler.finish_report();
    assert_eq!(
        report
            .contributions
            .iter()
            .filter(|contribution| contribution.scope == HermesUsageScope::Auxiliary)
            .count(),
        1
    );
}

#[test]
fn missing_recorded_amounts_never_fabricate_reported_or_saved_money() {
    let db = Path::new("state.db");
    let prices = linear_prices();
    let meta = metadata(db, "s", "test-model", "test-provider");

    let mut actual = HermesUsageReconciler::new(meta.clone(), &prices);
    actual.observe_segment(
        &session(
            db,
            "s",
            "test-model",
            "test-provider",
            Some(1),
            tokens(1_000, 500, 0, 0, 0),
            money(HermesCostStatus::Actual, Some(0.0), None),
        ),
        &[main_row(
            db,
            "s",
            "test-model",
            "test-provider",
            HermesCostStatus::Actual,
            Some(0.0),
            None,
        )],
    );
    let actual = actual.finish();
    assert!(actual.cost.usd.is_some());
    assert_eq!(actual.cost.class, AggregationClass::Estimated);
    assert_eq!(actual.reported_cost_usd, None);

    let mut estimated = HermesUsageReconciler::new(meta, &prices);
    estimated.observe_segment(
        &session(
            db,
            "s",
            "test-model",
            "test-provider",
            Some(1),
            tokens(1_000, 500, 0, 0, 0),
            money(HermesCostStatus::Estimated, None, Some(0.0)),
        ),
        &[main_row(
            db,
            "s",
            "test-model",
            "test-provider",
            HermesCostStatus::Estimated,
            None,
            Some(0.0),
        )],
    );
    let estimated = estimated.finish();
    assert!(estimated.cost.usd.is_some());
    assert_eq!(estimated.cost.class, AggregationClass::Estimated);
    assert_eq!(estimated.saved_estimate_usd, None);
}

#[test]
fn api_call_coverage_is_native_not_one_request_per_row() {
    let db = Path::new("state.db");
    let meta = metadata(db, "s", "test-model", "test-provider");
    let session = session(
        db,
        "s",
        "test-model",
        "test-provider",
        Some(7),
        tokens(1_000, 500, 0, 0, 0),
        money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
    );
    let rows = vec![row(
        db,
        "s",
        "test-model",
        "test-provider",
        "compression",
        Some(2),
        tokens(100, 20, 0, 0, 0),
        money(HermesCostStatus::Unknown, Some(0.0), Some(0.0)),
    )];
    let prices = linear_prices();
    let mut reconciler = HermesUsageReconciler::new(meta, &prices);
    reconciler.observe_segment(&session, &rows);
    let report = reconciler.finish_report();
    // Two rows exist, but the native coverage is 7 main + 2 auxiliary calls.
    assert_eq!(report.summary.coverage.observed_requests, 9);
    assert_ne!(report.summary.coverage.observed_requests, 2);
    assert_eq!(report.contributions.len(), 2);
    assert!(
        report
            .contributions
            .iter()
            .all(|contribution| contribution.api_call_count.is_some())
    );
}

#[test]
fn closure_is_not_success_and_absent_end_is_not_active() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(&writer, "failed", None, serde_json::json!({}), 1_000.0);
    insert_session(&writer, "quiet", None, serde_json::json!({}), 1_100.0);
    insert_session(&writer, "complete", None, serde_json::json!({}), 1_200.0);
    insert_session(&writer, "interrupted", None, serde_json::json!({}), 1_300.0);
    insert_session(
        &writer,
        "pending_tool",
        None,
        serde_json::json!({}),
        1_400.0,
    );
    set_ended(&writer, "failed", 2_000.0, "agent_close");
    set_ended(&writer, "complete", 3_000.0, "agent_close");
    set_ended(&writer, "interrupted", 3_100.0, "agent_close");
    set_ended(&writer, "pending_tool", 3_200.0, "agent_close");
    let failed = insert_message(&writer, "failed", "assistant", "boom", 1_900.0);
    set_message_finish_reason(&writer, failed, Some("error"));
    let quiet = insert_message(&writer, "quiet", "assistant", "waiting", 1_150.0);
    set_message_finish_reason(&writer, quiet, Some("stop"));
    let complete = insert_message(&writer, "complete", "assistant", "done", 2_900.0);
    set_message_finish_reason(&writer, complete, Some("stop"));
    // An older terminal finish must not outrank a newer unfinished user turn.
    let old_stop = insert_message(&writer, "interrupted", "assistant", "old", 2_950.0);
    set_message_finish_reason(&writer, old_stop, Some("stop"));
    insert_message(&writer, "interrupted", "user", "new request", 2_990.0);
    let pending = insert_message(&writer, "pending_tool", "assistant", "working", 3_150.0);
    set_message_finish_reason(&writer, pending, Some("stop"));
    set_message_tool_calls(&writer, pending, "[{\"id\":\"call\"}]");

    let failed = hydrate(&db, "failed", BIG).summary;
    assert_eq!(failed.lifecycle, Lifecycle::Failed);
    assert!(failed.metrics.timing.ended_at.is_some());
    assert_eq!(failed.last_completion, None);

    let quiet = hydrate(&db, "quiet", BIG).summary;
    assert_eq!(quiet.lifecycle, Lifecycle::Unknown);
    assert_ne!(quiet.lifecycle, Lifecycle::Ongoing);
    assert_eq!(quiet.metrics.timing.ended_at, None);

    let complete = hydrate(&db, "complete", BIG).summary;
    assert_eq!(complete.lifecycle, Lifecycle::Completed);
    assert_eq!(complete.last_completion, complete.metrics.timing.ended_at);

    let interrupted = hydrate(&db, "interrupted", BIG).summary;
    assert_eq!(interrupted.lifecycle, Lifecycle::Unknown);
    assert_eq!(interrupted.last_completion, None);

    let pending_tool = hydrate(&db, "pending_tool", BIG).summary;
    assert_eq!(pending_tool.lifecycle, Lifecycle::Unknown);
    assert_eq!(pending_tool.last_completion, None);

    // No request timing, active timer or current context is invented.
    assert_eq!(complete.metrics.speed, Default::default());
    assert_eq!(complete.metrics.timing.active_ms, None);
    assert!(!complete.metrics.timing.active_complete);
    assert_eq!(complete.metrics.context.used, None);
}

#[test]
fn byte_budget_suspends_sessions_and_usage_without_losing_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(&writer, "a", None, serde_json::json!({}), 1_000.0);
    insert_session(&writer, "b", None, serde_json::json!({}), 1_010.0);
    insert_session(&writer, "target", None, serde_json::json!({}), 1_020.0);
    set_session_usage(
        &writer,
        "target",
        [1_000, 500, 0, 0, 0],
        2,
        0.10,
        0.0,
        Some("estimated"),
    );
    insert_usage(
        &writer,
        "target",
        "test-model",
        "test-provider",
        "",
        [1_000, 500, 0, 0, 0],
        0.10,
        "estimated",
    );
    set_usage_usage(
        &writer,
        "target",
        "test-model",
        "",
        2,
        0.10,
        0.0,
        Some("estimated"),
    );
    insert_usage(
        &writer,
        "target",
        "test-model",
        "test-provider",
        "compression",
        [100, 20, 0, 0, 0],
        0.02,
        "estimated",
    );

    let budget = RefreshBudget {
        max_records: 1_000,
        max_bytes: 1,
        max_millis: 1_000,
    };
    let summary = hydrate(&db, "target", budget).summary;
    assert_eq!(summary.meta.key.native_id, "target");
    assert_eq!(
        summary
            .meta
            .members
            .iter()
            .map(|member| member.native_id.as_str())
            .collect::<Vec<_>>(),
        ["target"]
    );
    // Both the main row and the auxiliary row survive the byte-budget suspension.
    approx(summary.usage.as_ref().unwrap().cost.usd, 0.12);
}

#[test]
fn opening_work_respects_the_deadline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let writer = create_rollback_db(&db, true);
    insert_session(&writer, "p", None, serde_json::json!({}), 1_000.0);
    writer
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");

    // The deadline handler must already be active for the opener's schema read;
    // otherwise the opener's own 2,000 ms busy setting can hold this 1 ms slice.
    let mut hydration = HermesHydration::with_busy(&db, linear_prices(), 2_000);
    let started = std::time::Instant::now();
    let result = hydration.step(
        ReadJobId(1),
        &key(&db, "p"),
        NOW,
        RefreshBudget {
            max_records: 1_000,
            max_bytes: 100_000_000,
            max_millis: 1,
        },
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "opening work overran the slice: {elapsed:?}"
    );
    assert!(matches!(result, Err(ProviderError::Unavailable(_))));
}

#[test]
fn a_locked_database_respects_the_slice_deadline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let writer = create_rollback_db(&db, true);
    insert_session(&writer, "p", None, serde_json::json!({}), 1_000.0);
    writer
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");

    let mut hydration = HermesHydration::with_busy(&db, linear_prices(), 2_000);
    let started = std::time::Instant::now();
    let result = hydration.step(
        ReadJobId(1),
        &key(&db, "p"),
        NOW,
        RefreshBudget {
            max_records: 1_000,
            max_bytes: 100_000_000,
            max_millis: 20,
        },
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "a 20 ms slice waited for the 2 s busy timeout: {elapsed:?}"
    );
    assert!(matches!(result, Err(ProviderError::Unavailable(_))));
}

#[test]
fn usage_scan_deadline_is_bounded_and_resumable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(&writer, "p", None, serde_json::json!({}), 1_000.0);
    writer.execute_batch("BEGIN").expect("usage batch");
    for index in 0..5_000 {
        insert_usage(
            &writer,
            "p",
            "test-model",
            "test-provider",
            &format!("aux{index}"),
            [1, 1, 0, 0, 0],
            0.0,
            "unknown",
        );
    }
    writer.execute_batch("COMMIT").expect("usage batch commit");

    let mut hydration = HermesHydration::new(&db, linear_prices());
    let budget = RefreshBudget {
        max_records: 1_000,
        max_bytes: 100_000_000,
        max_millis: 5,
    };
    let started = std::time::Instant::now();
    let mut delta = hydration
        .step(ReadJobId(1), &key(&db, "p"), NOW, budget)
        .expect("usage deadline slice");
    let first = started.elapsed();
    assert!(
        first < std::time::Duration::from_millis(500),
        "a 5 ms usage slice overran: {first:?}"
    );
    let mut steps = 0;
    while !delta.done {
        steps += 1;
        assert!(steps < 10_000, "usage scan never resumed");
        delta = hydration
            .step(ReadJobId(1), &key(&db, "p"), NOW, budget)
            .expect("usage deadline continuation");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the bounded usage scan took too long"
        );
    }
    assert_eq!(
        delta
            .summary
            .usage
            .as_ref()
            .expect("usage summary")
            .coverage
            .observed_requests,
        5_000
    );
}

#[test]
fn cancellation_drops_partial_state_and_rehydration_observes_new_values() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("state.db");
    let writer = create_db(&db, true);
    insert_session(&writer, "p", None, serde_json::json!({}), 1_000.0);
    set_session_usage(
        &writer,
        "p",
        [1_000, 500, 0, 0, 0],
        2,
        0.10,
        0.0,
        Some("estimated"),
    );
    insert_usage(
        &writer,
        "p",
        "test-model",
        "test-provider",
        "",
        [1_000, 500, 0, 0, 0],
        0.10,
        "estimated",
    );
    set_usage_usage(
        &writer,
        "p",
        "test-model",
        "",
        2,
        0.10,
        0.0,
        Some("estimated"),
    );

    let mut hydration = HermesHydration::new(&db, linear_prices());
    let first = hydration
        .step(ReadJobId(1), &key(&db, "p"), NOW, TINY)
        .expect("first slice");
    assert!(!first.done);
    assert_eq!(hydration.tracked_jobs(), 1);
    assert!(hydration.has_state(&key(&db, "p"), ReadJobId(1)));
    hydration.cancel_read(&key(&db, "p"), ReadJobId(1));
    assert_eq!(hydration.tracked_jobs(), 0);
    assert!(!hydration.has_state(&key(&db, "p"), ReadJobId(1)));

    // A writer changes the value between the abandoned job and rehydration.
    set_session_usage(
        &writer,
        "p",
        [1_000, 500, 0, 0, 0],
        2,
        0.50,
        0.0,
        Some("estimated"),
    );
    set_usage_usage(
        &writer,
        "p",
        "test-model",
        "",
        2,
        0.50,
        0.0,
        Some("estimated"),
    );

    let mut done = None;
    for _ in 0..100_000 {
        let delta = hydration
            .step(ReadJobId(2), &key(&db, "p"), NOW, TINY)
            .expect("rehydration slice");
        if delta.done {
            done = Some(delta);
            break;
        }
    }
    let summary = done.expect("rehydration completed").summary;
    assert_eq!(summary.meta.key.native_id, "p");
    assert_eq!(
        summary
            .meta
            .members
            .iter()
            .map(|member| member.native_id.as_str())
            .collect::<Vec<_>>(),
        ["p"]
    );
    approx(summary.usage.as_ref().unwrap().cost.usd, 0.50);
    assert_eq!(hydration.tracked_jobs(), 0);
}
