mod support;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{AggregationClass, RefreshBudget};
use agentscope::pricing::PriceSchedule;
use agentscope::providers::claude::ClaudeScanner;
use serde_json::{Value, json};
use support::facts::price_schedule;
use support::{NOW, write_lines};

fn scan_with_prices(
    records: &[Value],
    prices: Option<PriceSchedule>,
) -> agentscope::domain::SessionSummary {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    write_lines(
        &root.join("project/session.jsonl"),
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    if let Some(prices) = prices {
        config.pricing = prices;
    }
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    support::world::rescan(
        &mut scanner,
        NOW,
        RefreshBudget {
            max_records: 1000,
            max_bytes: 1_000_000,
            max_millis: 60_000,
        },
    )
    .sessions
    .remove(0)
}
fn scan(records: &[Value]) -> agentscope::domain::SessionSummary {
    scan_with_prices(records, None)
}
fn record(uuid: &str, id: &str, content: Value) -> Value {
    json!({"type":"assistant","uuid":uuid,"sessionId":"s","message":{"id":id,"model":"claude-test","content":content,"usage":{"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":1,"ephemeral_1h_input_tokens":2},"output_tokens_details":{"thinking_tokens":3},"iterations":[{"input_tokens":1000}],"server_tool_use":{"web_search_requests":2,"web_fetch_requests":1}}}})
}
#[test]
fn top_level_usage_once_with_ttl_and_thinking_subset() {
    let s = scan(&[
        record("a", "m", json!([{"type":"text","text":"a"}])),
        record("b", "m", json!([{"type":"thinking","thinking":"b"}])),
    ]);
    let u = s.usage.unwrap();
    assert_eq!(u.class, AggregationClass::Derived);
    assert_eq!(u.total, Some(19));
    assert_eq!(
        (
            u.categories.cache_write_5m,
            u.categories.cache_write_1h,
            u.categories.reasoning
        ),
        (Some(1), Some(2), Some(3))
    );
    let counts = s.server_tools.unwrap();
    assert_eq!((counts.web_search, counts.web_fetch), (2, 1));
    assert!(s.metrics.context.capacity.is_none());
}
#[test]
fn missing_usage_marks_summary_partial() {
    let mut missing = record("b", "other", json!("b"));
    missing["message"].as_object_mut().unwrap().remove("usage");
    let s = scan(&[record("a", "m", json!("a")), missing]);
    assert_eq!(s.usage.unwrap().class, AggregationClass::Partial);
}

fn prices() -> PriceSchedule {
    // Native Claude usage records no billing route; the reference policy prices it at "anthropic".
    price_schedule(
        r#"[{"billing_provider":"anthropic","model":"claude-test","as_of":"2026-10-01",
            "rates":{"input":3.0,"output":5.0,"cache_read":1.0,"cache_write":4.0,
                "cache_write_5m":4.0,"cache_write_1h":4.0}}]"#,
    )
}

#[test]
fn cost_note_only_when_server_tools_used_and_single_cache_write_fallback() {
    let mut r = record("a", "m", json!("a"));
    r["message"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("cache_creation");
    r["message"]["usage"]
        .as_object_mut()
        .unwrap()
        .remove("server_tool_use");
    let usage = scan_with_prices(&[r.clone()], Some(prices()))
        .usage
        .unwrap();
    assert_eq!(usage.total, Some(19));
    assert_eq!(usage.cost.class, AggregationClass::Estimated);
    assert!((usage.cost.usd.unwrap() - 64.0 / 1_000_000.0).abs() < 1e-12);
    assert!(usage.cost.note.is_none());
    r["message"]["usage"]["server_tool_use"] = json!({"web_search_requests":1});
    assert!(
        scan_with_prices(&[r], Some(prices()))
            .usage
            .unwrap()
            .cost
            .note
            .unwrap()
            .contains("server-tool")
    );
}

#[test]
fn mixed_models_are_priced_per_request_and_unpriced_models_make_the_total_unknown() {
    let a = record("a", "one", json!("a"));
    let mut b = record("b", "two", json!("b"));
    b["message"]["model"] = json!("different");
    let usage = scan_with_prices(&[a, b], Some(prices())).usage.unwrap();
    assert_eq!(usage.total, Some(38));
    let ids: Vec<_> = usage.models.iter().map(|m| m.model.id.as_str()).collect();
    assert_eq!(ids, ["claude-test", "different"]);
    // The priced model keeps its own estimate; the unlisted one has none.
    assert!(usage.models[0].cost.usd.is_some());
    assert_eq!(usage.models[1].cost.usd, None);
    assert_eq!(usage.cost.usd, None);
    assert!(usage.cost.known_subtotal > 0.0);
    assert_eq!(usage.cost.class, AggregationClass::Unknown);
    assert_eq!(
        (
            usage.coverage.observed_requests,
            usage.coverage.priced_requests
        ),
        (2, 1)
    );

    // Two priced models sum their per-request estimates.
    let mut prices = prices();
    prices = prices
        .with_overrides(vec![agentscope::pricing::PriceOverride {
            card: agentscope::pricing::PriceCard::new(
                "anthropic",
                "different",
                agentscope::pricing::Rates {
                    input: Some(30.0),
                    output: Some(50.0),
                    cache_read: Some(10.0),
                    cache_write: Some(40.0),
                    cache_write_5m: Some(40.0),
                    cache_write_1h: Some(40.0),
                    ..Default::default()
                },
            ),
        }])
        .unwrap();
    let a = record("a", "one", json!("a"));
    let mut b = record("b", "two", json!("b"));
    b["message"]["model"] = json!("different");
    let usage = scan_with_prices(&[a, b], Some(prices)).usage.unwrap();
    // claude-test: 64e-6; different: ten times that.
    assert!((usage.cost.usd.unwrap() - 64.0 * 11.0 / 1e6).abs() < 1e-12);
    assert!(usage.coverage.complete);
}

fn stamped(uuid: &str, ts: Option<&str>, sidechain: bool, model: &str) -> Value {
    let mut v = json!({"type":"assistant","uuid":uuid,"sessionId":"s","isSidechain":sidechain,
        "message":{"id":uuid,"model":model,"content":[{"type":"text","text":"x"}],
        "usage":{"input_tokens":1,"output_tokens":1}}});
    if let Some(ts) = ts {
        v["timestamp"] = json!(ts);
    }
    v
}

#[test]
fn recorded_activity_timestamp_is_reported_and_absence_stays_unknown() {
    let with = scan(&[stamped("a", Some("2026-10-29T10:00:00Z"), false, "m")]);
    assert!(with.meta.activity.at.is_some());
    assert_eq!(with.meta.activity.class, AggregationClass::Reported);
    let without = scan(&[stamped("a", None, false, "m")]);
    assert_eq!(without.meta.activity.at, None);
    assert_eq!(without.meta.activity.class, AggregationClass::Unknown);
}

#[test]
fn sidechain_model_never_replaces_the_parent_model() {
    let mixed = scan(&[
        stamped("a", None, false, "parent-model"),
        stamped("b", None, true, "side-model"),
    ]);
    assert_eq!(mixed.meta.model.unwrap().id, "parent-model");
    let only_side = scan(&[stamped("b", None, true, "side-model")]);
    assert_eq!(only_side.meta.model.unwrap().id, "side-model");
    let side_first = scan(&[
        stamped("b", None, true, "side-model"),
        stamped("a", None, false, "parent-model"),
    ]);
    assert_eq!(side_first.meta.model.unwrap().id, "parent-model");
}

#[test]
fn metrics_capture_turn_spans_compaction_epoch_and_prompt_occupancy() {
    let mut before = record("p0", "m0", json!("before"));
    before["timestamp"] = json!("2026-10-30T09:59:00Z");
    let boundary = json!({
        "type": "system", "subtype": "compact_boundary", "uuid": "c1", "sessionId": "s",
        "timestamp": "2026-10-30T10:00:00Z",
        "compactMetadata": {"trigger": "auto", "preTokens": 180000}
    });
    let mut after = record("p1", "m1", json!("after"));
    after["timestamp"] = json!("2026-10-30T10:00:30Z");
    let span = json!({
        "type": "system", "subtype": "turn_duration", "uuid": "t1", "sessionId": "s",
        "timestamp": "2026-10-30T10:01:00Z", "durationMs": 60000
    });
    let s = scan(&[before, boundary, after, span]);
    let m = &s.metrics;
    assert_eq!(
        (m.context.used, m.context.capacity),
        (Some(15), None),
        "prompt occupancy is input + cache creation + cache read, not output or totals"
    );
    assert_eq!(m.context.compaction_epoch.as_deref(), Some("c1"));
    assert_eq!(m.compactions.observed, 1);
    assert_eq!(
        m.compactions.unpriced, 1,
        "a marker with no usage row is unpriced"
    );
    assert_eq!(m.compactions.known_cost_usd, None);
    assert!(m.compactions.history_complete);
    let latest = m.compactions.latest.as_ref().expect("latest marker");
    assert_eq!(
        (
            latest.trigger.as_deref(),
            latest.before_tokens,
            latest.after_tokens
        ),
        (Some("auto"), Some(180_000), None)
    );
    assert_eq!(m.timing.active_ms, Some(60_000));
    assert_eq!(m.timing.ended_at, None, "a turn stop is not a session end");
    assert!(
        !m.timing.active_complete,
        "turn spans are not recorded for every turn"
    );
    assert_eq!(m.speed, agentscope::domain::SpeedSummary::default());
}

#[test]
fn a_compaction_boundary_invalidates_the_earlier_prompt_snapshot() {
    let mut before = record("p0", "m0", json!("before"));
    before["timestamp"] = json!("2026-10-30T09:59:00Z");
    let boundary = json!({
        "type": "system", "subtype": "compact_boundary", "uuid": "c1", "sessionId": "s",
        "timestamp": "2026-10-30T10:00:00Z",
        "compactMetadata": {"trigger": "manual", "preTokens": 180000}
    });
    let s = scan(&[before, boundary]);
    assert_eq!(
        s.metrics.context.used, None,
        "no post-compaction snapshot exists"
    );
    assert_eq!(s.metrics.context.capacity, None);
    assert_eq!(s.metrics.compactions.observed, 1);
}

#[test]
fn the_newest_compaction_markers_are_retained_when_capped() {
    let records: Vec<Value> = (0..300)
        .map(|i| {
            json!({
                "type": "system", "subtype": "compact_boundary", "uuid": format!("c{i}"),
                "sessionId": "s", "timestamp": format!("2026-10-30T10:{:02}:{:02}Z", i / 60, i % 60),
                "compactMetadata": {"trigger": "auto", "preTokens": 1000 + i}
            })
        })
        .collect();
    let s = scan(&records);
    assert_eq!(s.metrics.compactions.observed, 256);
    assert!(!s.metrics.compactions.history_complete);
    let latest = s
        .metrics
        .compactions
        .latest
        .as_ref()
        .expect("latest marker");
    assert_eq!(
        latest.timestamp,
        Some(1_793_354_400_000 + 299_000),
        "the retained window slides to the newest marker"
    );
    assert_eq!(latest.before_tokens, Some(1299));
}
