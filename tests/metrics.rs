mod support;

use agentscope::domain::{
    AggregationClass, CompactionObservation, CompactionScope, InputAccounting, ModelDetail,
    OutputAccounting, PageRequest, RequestPurpose, TokenAccounting, TokenCategories,
    UsageObservation, UsageScope,
};
use agentscope::metrics::{
    SpeedAccumulator, SpeedOutcome, TurnInterval, compaction_summary, context_usage,
    current_context, measured_speed, page_details, request_rate, session_timing, tokens_removed,
    union_active_ms,
};
use agentscope::pricing::ReferencePolicy;
use support::facts::{price_schedule, usage};

const ADDITIVE: TokenAccounting = TokenAccounting {
    input: InputAccounting::AdditiveCaches,
    output: OutputAccounting::IncludesReasoning,
};

fn out(n: Option<u64>) -> TokenCategories {
    TokenCategories {
        output: n,
        ..TokenCategories::default()
    }
}

/// A request with recorded output, duration, TTFT, and completion time.
fn timed(
    id: &str,
    output: u64,
    duration_ms: Option<f64>,
    completed: Option<i64>,
) -> UsageObservation {
    let mut u = usage(id, Some("acme"), "m", out(Some(output)), ADDITIVE);
    let r = u.request.as_mut().unwrap();
    r.duration_ms = duration_ms;
    r.ttft_ms = duration_ms.map(|_| 2_000.0);
    r.completed_at = completed;
    u
}

fn five_timed() -> Vec<UsageObservation> {
    [
        (1_600, 19_000.0),
        (1_200, 15_000.0),
        (900, 12_000.0),
        (1_000, 13_000.0),
        (800, 11_000.0),
    ]
    .iter()
    .enumerate()
    .map(|(i, (o, d))| {
        timed(
            &format!("t{i}"),
            *o,
            Some(*d),
            Some(10_000 - 1_000 * i as i64),
        )
    })
    .collect()
}

#[test]
fn request_speed_includes_ttft_and_weighted_coverage() {
    let mut all = five_timed();
    // Three untimed requests, earlier than every timed one, with 7,300 output in total.
    for (i, o) in [2_500u64, 2_800, 2_000].into_iter().enumerate() {
        all.push(timed(&format!("u{i}"), o, None, Some(100 + i as i64)));
    }
    // Input order must not decide "latest".
    all.reverse();
    let speed = measured_speed(&all);
    let latest = speed.latest.as_ref().unwrap();
    assert!((latest.tokens_per_second - 1600.0 / 19.0).abs() < 1e-9);
    assert_eq!(latest.request_id.as_deref(), Some("t0"));
    assert_eq!(
        latest.ttft_ms,
        Some(2_000.0),
        "TTFT is reported, not subtracted"
    );
    assert!((speed.weighted_tokens_per_second.unwrap() - 5500.0 / 70.0).abs() < 1e-9);
    assert_eq!(speed.measured_output, 5_500);
    assert_eq!(speed.measured_duration_ms, 70_000.0);
    assert_eq!((speed.measured_requests, speed.total_requests), (5, 8));
    assert!(!speed.complete);
}

#[test]
fn latest_keeps_its_actual_model() {
    let mut a = timed("old", 100, Some(1_000.0), Some(1));
    let mut b = timed("new", 100, Some(2_000.0), Some(2));
    a.model = Some(ModelDetail {
        id: "first".into(),
        ..a.model.clone().unwrap()
    });
    b.model = None;
    let speed = measured_speed([&a, &b]);
    assert_eq!(
        speed.latest.as_ref().unwrap().request_id.as_deref(),
        Some("new")
    );
    assert!(speed.latest.unwrap().model.is_none());
}

#[test]
fn invalid_or_missing_measurements_never_yield_a_rate() {
    for duration in [0.0, 100.0, -1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let u = timed("r", 500, Some(duration), Some(1));
        assert_eq!(request_rate(&u), None, "duration {duration}");
        let speed = measured_speed([&u]);
        assert_eq!(speed.measured_requests, 0);
        assert_eq!(speed.total_requests, 1);
        assert_eq!(speed.weighted_tokens_per_second, None);
        assert!(speed.latest.is_none());
    }
    assert_eq!(request_rate(&timed("r", 500, None, None)), None);
    let mut no_output = timed("r", 0, Some(5_000.0), Some(1));
    assert_eq!(request_rate(&no_output), None, "zero output");
    no_output.categories = out(None);
    assert_eq!(request_rate(&no_output), None, "missing output");
    let mut compaction = timed("r", 500, Some(5_000.0), Some(1));
    compaction.request.as_mut().unwrap().purpose = RequestPurpose::Compaction;
    assert_eq!(request_rate(&compaction), None);
    let mut session = timed("r", 500, Some(5_000.0), Some(1));
    session.scope = UsageScope::Session;
    assert_eq!(request_rate(&session), None);
    let instant = timed("r", 500, Some(100.0), Some(1));
    assert_eq!(request_rate(&instant), None);
    assert!(request_rate(&timed("r", 500, Some(100.5), Some(1))).is_some());
}

#[test]
fn separately_reported_reasoning_is_included_once() {
    let mut u = timed("r", 100, Some(10_000.0), Some(1));
    u.categories.reasoning = Some(50);
    u.accounting.output = OutputAccounting::SeparateReasoning;
    assert!((request_rate(&u).unwrap() - 15.0).abs() < 1e-9);
    u.accounting.output = OutputAccounting::IncludesReasoning;
    assert!((request_rate(&u).unwrap() - 10.0).abs() < 1e-9);
}

#[test]
fn active_time_unions_overlap_and_never_runs_through_now() {
    let span = |s, e| TurnInterval {
        started_at: s,
        ended_at: Some(e),
    };
    let overlapping = [span(0, 1_000), span(500, 1_500)];
    assert_eq!(union_active_ms(&overlapping), Some(1500));
    assert_eq!(
        union_active_ms(&[span(0, 100), span(200, 300)]),
        Some(200),
        "gaps are not active"
    );
    let open = TurnInterval {
        started_at: 2_000,
        ended_at: None,
    };
    assert_eq!(union_active_ms(&[open]), None);
    let mixed = [span(0, 1_000), open, span(900, 100)];
    assert_eq!(union_active_ms(&mixed), Some(1_000));
    assert_eq!(
        union_active_ms(&[span(i64::MIN, i64::MAX)]),
        None,
        "overflow"
    );

    let t = session_timing(Some(0), Some(1_500), &overlapping, true, Some(f64::NAN));
    assert_eq!((t.elapsed_ms, t.active_ms), (Some(1_500), Some(1_500)));
    assert!(t.active_complete);
    assert_eq!(t.model_ms, None, "NaN model time is not a measurement");
    let t = session_timing(Some(0), None, &mixed, true, None);
    assert!(!t.active_complete, "open or invalid interval is incomplete");
    assert_eq!((t.elapsed_ms, t.active_ms), (None, Some(1_000)));
    let t = session_timing(Some(10), Some(5), &[], true, None);
    assert_eq!(
        (t.elapsed_ms, t.active_ms, t.active_complete),
        (None, None, false)
    );
}

#[test]
fn timed_revision_replaces_not_duplicates() {
    let mut acc = SpeedAccumulator::new(8);
    let first = timed("r", 3, Some(19_000.0), Some(5));
    let revised = timed("r", 9, Some(19_000.0), Some(5));
    assert_eq!(acc.observe(&first), SpeedOutcome::Counted);
    assert_eq!(acc.observe(&revised), SpeedOutcome::Uncountable);
    assert_eq!(acc.revise(&revised), SpeedOutcome::Revised);
    assert_eq!(acc.revise(&revised), SpeedOutcome::Duplicate);
    assert_eq!(acc.observe(&revised), SpeedOutcome::Duplicate);
    let s = acc.snapshot();
    assert_eq!((s.measured_requests, s.total_requests), (1, 1));
    assert_eq!(s.measured_output, 9);
    assert_eq!(s.measured_duration_ms, 19_000.0);
    assert!((s.latest.unwrap().tokens_per_second - 9.0 / 19.0).abs() < 1e-12);
    assert!(s.complete);
}

#[test]
fn a_revision_can_add_missing_timing_and_cannot_invent_requests() {
    let mut acc = SpeedAccumulator::new(8);
    let untimed = timed("r", 40, None, Some(5));
    assert_eq!(acc.revise(&untimed), SpeedOutcome::Uncountable, "no target");
    assert_eq!(acc.observe(&untimed), SpeedOutcome::Counted);
    let s = acc.snapshot();
    assert_eq!((s.measured_requests, s.total_requests), (0, 1));
    assert!(!s.complete);
    let now_timed = timed("r", 40, Some(4_000.0), Some(5));
    assert_eq!(acc.revise(&now_timed), SpeedOutcome::Revised);
    let s = acc.snapshot();
    assert_eq!((s.measured_requests, s.total_requests), (1, 1));
    assert!(s.complete);
    assert!((s.weighted_tokens_per_second.unwrap() - 10.0).abs() < 1e-9);
    // Losing the measurement moves it back out and clears the latest.
    assert_eq!(acc.revise(&untimed), SpeedOutcome::Revised);
    let s = acc.snapshot();
    assert_eq!((s.measured_requests, s.measured_output), (0, 0));
    assert!(s.latest.is_none() && s.weighted_tokens_per_second.is_none());
}

#[test]
fn accumulator_is_bounded_and_marks_coverage_incomplete() {
    let mut acc = SpeedAccumulator::new(2);
    for i in 0..3 {
        let u = timed(&format!("r{i}"), 10, Some(1_000.0), Some(i));
        let expect = if i < 2 {
            SpeedOutcome::Counted
        } else {
            SpeedOutcome::Uncountable
        };
        assert_eq!(acc.observe(&u), expect);
    }
    let s = acc.snapshot();
    assert_eq!(s.total_requests, 2);
    assert!(!s.complete);
    let mut compaction = timed("c", 10, Some(1_000.0), Some(9));
    compaction.request.as_mut().unwrap().purpose = RequestPurpose::Compaction;
    assert_eq!(acc.observe(&compaction), SpeedOutcome::Uncountable);
}

#[test]
fn accumulator_agrees_with_iterator_and_revising_the_latest_rescans() {
    let all = five_timed();
    let mut acc = SpeedAccumulator::new(16);
    for u in &all {
        acc.observe(u);
    }
    let from_acc = acc.snapshot();
    let from_iter = measured_speed(&all);
    assert_eq!(from_acc, from_iter);
    // The latest request loses its measurement: the next latest takes over.
    assert_eq!(
        acc.revise(&timed("t0", 1_600, None, Some(10_000))),
        SpeedOutcome::Revised
    );
    let s = acc.snapshot();
    assert_eq!(s.latest.unwrap().request_id.as_deref(), Some("t1"));
    assert_eq!((s.measured_requests, s.total_requests), (4, 5));
}

#[test]
fn context_occupancy_is_distinct_from_lifetime_tokens() {
    let mut lifetime = TokenCategories {
        input: Some(5_000_000),
        output: Some(900_000),
        total: Some(5_900_000),
        ..TokenCategories::default()
    };
    lifetime.cache_read = Some(10_000_000);
    let ctx = context_usage(
        Some(215_000),
        Some(1_000_000),
        AggregationClass::Reported,
        Some(7),
        None,
        Some("e0".into()),
    );
    assert_eq!((ctx.used, ctx.capacity), (Some(215_000), Some(1_000_000)));
    assert_ne!(ctx.used, lifetime.total);
    assert_eq!(ctx.class, AggregationClass::Reported);

    // No generic fallback: unknown occupancy stays unknown.
    let unknown = context_usage(None, None, AggregationClass::Reported, None, None, None);
    assert_eq!((unknown.used, unknown.capacity), (None, None));
    assert_eq!(unknown.class, AggregationClass::Unknown);
    assert_eq!(
        context_usage(
            Some(1),
            Some(0),
            AggregationClass::Reported,
            None,
            None,
            None
        )
        .capacity,
        None,
        "a zero window is not a capacity"
    );
}

#[test]
fn compaction_epoch_change_invalidates_occupancy_until_a_new_snapshot() {
    let before = context_usage(
        Some(180_000),
        Some(1_000_000),
        AggregationClass::Reported,
        Some(7),
        None,
        Some("e0".into()),
    );
    assert_eq!(current_context(&before, Some("e0")), before);
    let stale = current_context(&before, Some("e1"));
    assert_eq!(stale.used, None);
    assert_eq!(stale.recorded_at, None);
    assert_eq!(stale.capacity, Some(1_000_000), "the window survives");
    assert_eq!(stale.compaction_epoch.as_deref(), Some("e1"));
}

fn card() -> agentscope::pricing::PriceSchedule {
    price_schedule(
        r#"[{"billing_provider":"acme","model":"m","rates":{"input":2.0,"output":10.0,"cache_read":0.1,"cache_write_5m":2.5,"cache_write_1h":4.0,"reasoning":12.0}}]"#,
    )
}

fn billed(id: &str) -> UsageObservation {
    // 4,000 input at $2/M plus 2,000 output at $10/M = $0.028.
    usage(
        id,
        Some("acme"),
        "m",
        TokenCategories {
            input: Some(4_000),
            output: Some(2_000),
            cache_read: Some(0),
            cache_write: Some(0),
            ..TokenCategories::default()
        },
        ADDITIVE,
    )
}

fn marker(
    id: &str,
    scope: CompactionScope,
    usage: Option<UsageObservation>,
) -> CompactionObservation {
    let provenance = billed("p").provenance;
    CompactionObservation {
        id: id.into(),
        timestamp: Some(1_000),
        method: None,
        trigger: None,
        summary: None,
        before_tokens: Some(180_000),
        after_tokens: Some(42_000),
        duration_ms: None,
        usage,
        scope,
        provenance,
    }
}

#[test]
fn partially_priced_compactions_report_a_subtotal_not_a_total() {
    let events = [
        marker("a", CompactionScope::Event, Some(billed("req-a"))),
        marker("b", CompactionScope::Event, None),
    ];
    let s = compaction_summary(&events, true, &card(), ReferencePolicy::Disabled);
    assert_eq!((s.observed, s.unpriced), (2, 1));
    assert!((s.known_cost_usd.unwrap() - 0.028).abs() < 1e-12);
    assert_eq!(s.cost_scope, CompactionScope::Event);
    assert!(s.history_complete);
    assert_eq!(tokens_removed(&events[0]), Some(138_000));
    // Not billed request usage: the reduction is a context fact only.
    let mut inverted = events[0].clone();
    inverted.after_tokens = Some(200_000);
    assert_eq!(tokens_removed(&inverted), None);

    let none = compaction_summary(&events[1..], false, &card(), ReferencePolicy::Disabled);
    assert_eq!(
        (none.observed, none.unpriced, none.known_cost_usd),
        (1, 1, None)
    );
    assert_eq!(none.cost_scope, CompactionScope::Unknown);
    assert!(!none.history_complete);
    let empty = compaction_summary([], false, &card(), ReferencePolicy::Disabled);
    assert_eq!(
        (empty.observed, empty.known_cost_usd, empty.latest.is_none()),
        (0, None, true)
    );
}

#[test]
fn aggregate_compaction_cost_is_not_an_extra_event() {
    let aggregate = marker("agg", CompactionScope::Aggregate, Some(billed("req-agg")));
    // Two unattributed markers plus a known aggregate: still two events, with the
    // amount labeled aggregate and not divided between them.
    let events = [
        marker("a", CompactionScope::Event, None),
        marker("b", CompactionScope::Event, None),
        aggregate.clone(),
    ];
    let s = compaction_summary(&events, true, &card(), ReferencePolicy::Disabled);
    assert_eq!(s.observed, 2);
    assert_eq!(s.unpriced, 2);
    assert_eq!(s.cost_scope, CompactionScope::Aggregate);
    assert!((s.known_cost_usd.unwrap() - 0.028).abs() < 1e-12);
    assert_eq!(s.latest.as_ref().unwrap().scope, CompactionScope::Event);

    // Events already priced individually: the aggregate is not added again.
    let events = [
        marker("a", CompactionScope::Event, Some(billed("req-a"))),
        marker("b", CompactionScope::Event, Some(billed("req-b"))),
        aggregate,
    ];
    let s = compaction_summary(&events, true, &card(), ReferencePolicy::Disabled);
    assert_eq!((s.observed, s.unpriced), (2, 0));
    assert_eq!(s.cost_scope, CompactionScope::Event);
    assert!((s.known_cost_usd.unwrap() - 0.056).abs() < 1e-12);
}

#[test]
fn detail_pages_obey_the_combined_record_and_byte_budget() {
    let requests: Vec<_> = (0..3).map(|i| billed(&format!("r{i}"))).collect();
    let compactions = [marker("c0", CompactionScope::Event, None)];
    let page = |max_events, max_bytes| {
        page_details(
            &requests,
            &compactions,
            &[],
            PageRequest {
                max_events,
                max_bytes,
            },
        )
    };
    let p = page(2, usize::MAX);
    assert_eq!((p.requests.len(), p.compactions.len()), (2, 0));
    assert!(p.has_more && p.next.is_some() && !p.truncated);
    let p = page(10, usize::MAX);
    assert_eq!((p.requests.len(), p.compactions.len()), (3, 1));
    assert!(!p.has_more && p.next.is_none());
    let p = page(10, 1);
    assert_eq!(
        p.requests.len(),
        1,
        "progress even when one record exceeds the budget"
    );
    assert!(p.truncated && p.has_more);
}
