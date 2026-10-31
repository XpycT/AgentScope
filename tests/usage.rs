mod support;

use agentscope::domain::{
    AggregationClass, Harness, InputAccounting, MoneyFact, MoneySource, OutputAccounting,
    RecordedMoney, RequestMetadata, TokenAccounting, TokenCategories, UsageObservation, UsageScope,
};
use agentscope::pricing::{
    PriceCard, PriceOverride, PriceSchedule, PricingError, Rates, ReferencePolicy, estimate_cost,
};
use agentscope::usage::{
    LedgerOutcome, UsageError, UsageLedger, UsageUpdate, claude_total, codex_total,
    normalized_categories, opencode_total,
};
use support::facts::{price_schedule, usage};

const ADDITIVE: TokenAccounting = TokenAccounting {
    input: InputAccounting::AdditiveCaches,
    output: OutputAccounting::IncludesReasoning,
};
const INCLUSIVE: TokenAccounting = TokenAccounting {
    input: InputAccounting::InclusiveCaches,
    output: OutputAccounting::IncludesReasoning,
};
const SEPARATE: TokenAccounting = TokenAccounting {
    input: InputAccounting::AdditiveCaches,
    output: OutputAccounting::SeparateReasoning,
};

/// input=2, output=10, read=0.1, write5m=2.5, write1h=4 per million; reasoning=12.
const RATES: &str = r#""rates":{"input":2.0,"output":10.0,"cache_read":0.1,"cache_write_5m":2.5,"cache_write_1h":4.0,"reasoning":12.0}"#;

fn card(provider: &str, model: &str, extra: &str) -> String {
    format!(r#"{{"billing_provider":"{provider}","model":"{model}",{RATES}{extra}}}"#)
}

fn schedule(cards: &[String]) -> PriceSchedule {
    price_schedule(&format!("[{}]", cards.join(",")))
}

fn base() -> PriceSchedule {
    schedule(&[card("acme", "m", "")])
}

fn estimate(u: &UsageObservation, prices: &PriceSchedule) -> agentscope::domain::CostEstimate {
    estimate_cost(u, prices, ReferencePolicy::Disabled)
}

fn additive(input: u64, output: u64, read: u64, w5: u64, w1: u64) -> TokenCategories {
    TokenCategories {
        input: Some(input),
        output: Some(output),
        cache_read: Some(read),
        cache_write: Some(w5 + w1),
        cache_write_5m: Some(w5),
        cache_write_1h: Some(w1),
        ..TokenCategories::default()
    }
}

fn at(u: &mut UsageObservation, ms: i64) {
    u.request
        .get_or_insert_with(RequestMetadata::default)
        .started_at = Some(ms);
}

// 2026-10-30T00:00:00Z is a Friday.
const FRIDAY: i64 = 1_793_318_400_000;
const MINUTE: i64 = 60_000;

#[test]
fn codex_uses_latest_cumulative_total_not_a_sum() {
    let latest = TokenCategories {
        input: Some(900),
        cached_input: Some(400),
        output: Some(100),
        reasoning: Some(60),
        total: Some(1000),
        ..Default::default()
    };
    assert_eq!(codex_total(&latest), Some(1000));
    let no_total = TokenCategories {
        total: None,
        ..latest
    };
    assert_eq!(codex_total(&no_total), Some(1000));
    assert_eq!(codex_total(&TokenCategories::default()), None);
}

#[test]
fn claude_does_not_add_iterations_or_thinking() {
    let u = TokenCategories {
        input: Some(10),
        cache_write: Some(100),
        cache_read: Some(1000),
        output: Some(50),
        reasoning: Some(30),
        ..Default::default()
    };
    assert_eq!(claude_total(&u), Some(1160));
    let split = TokenCategories {
        cache_write: None,
        cache_write_5m: Some(60),
        cache_write_1h: Some(40),
        ..u.clone()
    };
    assert_eq!(claude_total(&split), Some(1160));
    let missing = TokenCategories {
        cache_read: None,
        ..u
    };
    assert_eq!(claude_total(&missing), None);
}

#[test]
fn opencode_uses_session_totals() {
    let u = TokenCategories {
        input: Some(1),
        output: Some(2),
        reasoning: Some(3),
        cache_read: Some(4),
        cache_write: Some(5),
        total: Some(999),
        ..Default::default()
    };
    assert_eq!(opencode_total(&u), Some(999));
    let derived = TokenCategories {
        total: None,
        ..u.clone()
    };
    assert_eq!(opencode_total(&derived), Some(15));
    let missing = TokenCategories {
        total: None,
        reasoning: None,
        ..u
    };
    assert_eq!(opencode_total(&missing), None);
}

#[test]
fn cache_ttl_costs_reconcile() {
    let u = usage(
        "r1",
        Some("acme"),
        "m",
        TokenCategories {
            reasoning: Some(9_000),
            ..additive(48_200, 12_800, 154_000, 6_000, 2_000)
        },
        ADDITIVE,
    );
    let e = estimate(&u, &base());
    assert!((e.usd.unwrap() - 0.2628).abs() < 1e-9, "{e:?}");
    assert_eq!(e.class, AggregationClass::Estimated);
    assert_eq!(
        e.breakdown.output,
        Some(0.128),
        "reasoning is already an output subset"
    );
    assert_eq!(e.breakdown.reasoning, None);
    assert!((e.breakdown.cache_write_5m.unwrap() - 0.015).abs() < 1e-12);
    assert!((e.breakdown.cache_write_1h.unwrap() - 0.008).abs() < 1e-12);
    assert!((e.known_subtotal - 0.2628).abs() < 1e-9);
    assert_eq!(e.source, Some(MoneySource::BundledReference));
    let n = normalized_categories(&u).unwrap();
    assert_eq!(
        (n.input, n.cache_read, n.cache_write, n.output, n.reasoning),
        (
            Some(48_200),
            Some(154_000),
            Some(8_000),
            Some(12_800),
            Some(9_000)
        )
    );
}

#[test]
fn inclusive_cached_input_and_reasoning_are_not_rebilled() {
    let cats = TokenCategories {
        input: Some(1_000),
        cached_input: Some(400),
        cache_write: Some(0),
        output: Some(100),
        reasoning: Some(60),
        ..Default::default()
    };
    let u = usage("r1", Some("acme"), "m", cats.clone(), INCLUSIVE);
    // 600 uncached * 2 + 400 read * 0.1 + 100 output * 10, per million.
    let e = estimate(&u, &base());
    assert!((e.usd.unwrap() - 0.00224).abs() < 1e-12, "{e:?}");
    assert_eq!(e.breakdown.reasoning, None);
    let n = normalized_categories(&u).unwrap();
    assert_eq!(
        (n.input, n.cache_read, n.output),
        (Some(600), Some(400), Some(100))
    );

    // A cached count above input is invalid, never clamped to zero.
    let over = usage(
        "r2",
        Some("acme"),
        "m",
        TokenCategories {
            cached_input: Some(2_000),
            ..cats.clone()
        },
        INCLUSIVE,
    );
    assert_eq!(normalized_categories(&over), Err(UsageError::InvalidSubset));
    assert_eq!(estimate(&over, &base()).usd, None);

    // Reasoning above output is invalid when output includes it.
    let bad = usage(
        "r3",
        Some("acme"),
        "m",
        TokenCategories {
            reasoning: Some(500),
            ..cats
        },
        INCLUSIVE,
    );
    assert_eq!(normalized_categories(&bad), Err(UsageError::InvalidSubset));
}

#[test]
fn unknown_accounting_never_guesses_inclusion() {
    let cats = TokenCategories {
        input: Some(10),
        cache_read: Some(4),
        cache_write: Some(0),
        output: Some(5),
        ..Default::default()
    };
    let u = usage("r", Some("acme"), "m", cats, TokenAccounting::default());
    assert_eq!(
        normalized_categories(&u),
        Err(UsageError::UnknownAccounting)
    );
    assert_eq!(estimate(&u, &base()).usd, None);
    let huge = usage(
        "r",
        Some("acme"),
        "m",
        TokenCategories {
            input: Some(u64::MAX),
            output: Some(1),
            cache_read: Some(0),
            cache_write: Some(0),
            orchestration_input: Some(1),
            ..Default::default()
        },
        ADDITIVE,
    );
    assert_eq!(normalized_categories(&huge), Err(UsageError::Overflow));
}

#[test]
fn separate_reasoning_needs_its_own_price_and_orchestration_is_additive() {
    let cats = TokenCategories {
        input: Some(1_000_000),
        output: Some(1_000_000),
        reasoning: Some(1_000_000),
        cache_read: Some(0),
        cache_write: Some(0),
        ..Default::default()
    };
    let u = usage("r", Some("acme"), "m", cats.clone(), SEPARATE);
    assert_eq!(estimate(&u, &base()).usd, Some(2.0 + 10.0 + 12.0));
    assert_eq!(normalized_categories(&u).unwrap().output, Some(2_000_000));
    let no_reasoning = schedule(&[
        r#"{"billing_provider":"acme","model":"m","rates":{"input":2.0,"output":10.0,"cache_read":0.0,"cache_write":0.0}}"#
            .to_owned(),
    ]);
    let e = estimate(&u, &no_reasoning);
    assert_eq!((e.usd, e.class), (None, AggregationClass::Unknown));
    assert!((e.known_subtotal - 12.0).abs() < 1e-9);

    let orch = usage(
        "r2",
        Some("acme"),
        "m",
        TokenCategories {
            orchestration_input: Some(1_000_000),
            orchestration_output: Some(1_000_000),
            orchestration_cache_read: Some(1_000_000),
            ..additive(1_000_000, 1_000_000, 1_000_000, 0, 0)
        },
        ADDITIVE,
    );
    // (1M+1M) in + (1M+1M) out + (1M+1M) read.
    let got = estimate(&orch, &base()).usd.unwrap();
    assert!((got - (4.0 + 20.0 + 0.2)).abs() < 1e-9, "{got}");
}

#[test]
fn mixed_models_are_priced_per_request() {
    let prices = schedule(&[
        card("acme", "cheap", ""),
        r#"{"billing_provider":"acme","model":"dear","rates":{"input":20.0,"output":100.0,"cache_read":1.0,"cache_write_5m":25.0,"cache_write_1h":40.0}}"#
            .to_owned(),
    ]);
    let cats = additive(1_000_000, 1_000_000, 0, 0, 0);
    let mut ledger = UsageLedger::new(100, 1 << 20);
    let a = usage("a", Some("acme"), "cheap", cats.clone(), ADDITIVE);
    let b = usage("b", Some("acme"), "dear", cats.clone(), ADDITIVE);
    let c = usage("c", Some("acme"), "nobody", cats, ADDITIVE);
    for u in [&a, &b] {
        assert_eq!(
            ledger.observe(UsageUpdate::Initial(u), &prices, ReferencePolicy::Disabled),
            LedgerOutcome::Counted
        );
    }
    let s = ledger.snapshot();
    assert_eq!(s.cost.usd, Some(12.0 + 120.0));
    assert_eq!(s.cost.class, AggregationClass::Estimated);
    assert_eq!(s.coverage.observed_requests, 2);
    assert!(s.coverage.complete);
    assert_eq!(s.models.len(), 2);
    let dear = s.models.iter().find(|m| m.model.id == "dear").unwrap();
    assert_eq!(dear.cost.usd, Some(120.0));
    assert_eq!(s.categories.input, Some(2_000_000));

    ledger.observe(UsageUpdate::Initial(&c), &prices, ReferencePolicy::Disabled);
    let s = ledger.snapshot();
    assert_eq!(
        s.cost.usd, None,
        "an unpriced request makes the total unknown"
    );
    assert!((s.cost.known_subtotal - 132.0).abs() < 1e-9);
    assert_eq!(
        (s.coverage.observed_requests, s.coverage.priced_requests),
        (3, 2)
    );
    assert!(!s.coverage.complete);
    // Token totals stay complete even though the price is not.
    assert_eq!(s.categories.input, Some(3_000_000));
}

#[test]
fn subscription_zero_is_not_api_equivalent_zero() {
    let mut u = usage(
        "r",
        Some("acme"),
        "m",
        additive(1_000_000, 1_000_000, 0, 0, 0),
        ADDITIVE,
    );
    u.money.reported_api = Some(MoneyFact {
        usd: 0.0,
        source: MoneySource::ReportedApi,
        as_of: None,
    });
    let mut ledger = UsageLedger::new(10, 1 << 20);
    ledger.observe(UsageUpdate::Initial(&u), &base(), ReferencePolicy::Disabled);
    let subscription = ledger.snapshot();
    assert_eq!(subscription.reported_cost_usd, Some(0.0));
    assert!(subscription.cost.usd.unwrap() > 0.0);
    assert_eq!(subscription.cost.class, AggregationClass::Estimated);

    // A reported zero never becomes a complete $0 when the engine cannot price it:
    // the amount stays in the reported field, the estimate stays unknown with a reason.
    let other = schedule(&[card("acme", "other", "")]);
    let unlisted = estimate(&u, &other);
    assert_eq!(
        (unlisted.usd, unlisted.class),
        (None, AggregationClass::Unknown)
    );
    assert!(
        unlisted
            .note
            .as_deref()
            .unwrap()
            .contains("reported no charge")
    );
    let mut unpriced = UsageLedger::new(10, 1 << 20);
    unpriced.observe(UsageUpdate::Initial(&u), &other, ReferencePolicy::Disabled);
    let s = unpriced.snapshot();
    assert_eq!(s.reported_cost_usd, Some(0.0));
    assert_eq!(s.cost.usd, None);
    // A proven-free card keeps the reported zero as a complete $0.
    let proven_free = schedule(&[r#"{"billing_provider":"acme","model":"m","rates":{"input":0.0,"output":0.0,"cache_read":0.0,"cache_write_5m":0.0,"cache_write_1h":0.0}}"#.to_owned()]);
    let f = estimate(&u, &proven_free);
    assert_eq!((f.usd, f.class), (Some(0.0), AggregationClass::Reported));
    // A missing counter leaves the known subtotal and reason instead of $0.
    let mut missing = u.clone();
    missing.categories.cache_read = None;
    let m = estimate(&missing, &base());
    assert_eq!(m.usd, None);
    assert!(m.known_subtotal > 0.0);
    assert!(m.note.is_some());

    // A positive reported amount is final.
    u.money.reported_api = Some(MoneyFact {
        usd: 1.5,
        source: MoneySource::ReportedApi,
        as_of: None,
    });
    let paid = estimate(&u, &base());
    assert_eq!(
        (paid.usd, paid.class),
        (Some(1.5), AggregationClass::Reported)
    );

    // Unusable or unverified money never replaces the engine.
    u.money = RecordedMoney {
        reported_api: Some(MoneyFact {
            usd: f64::NAN,
            source: MoneySource::ReportedApi,
            as_of: None,
        }),
        saved_estimate: Some(MoneyFact {
            usd: -1.0,
            source: MoneySource::SavedCatalogEstimate,
            as_of: None,
        }),
        unverified_usd: Some(999.0),
    };
    let e = estimate(&u, &base());
    assert_eq!(e.usd, Some(12.0));
    assert_eq!(
        e.note.as_deref(),
        Some("recorded money was unusable and ignored")
    );
}

#[test]
fn saved_estimate_survives_a_newer_card() {
    let mut u = usage(
        "r",
        Some("acme"),
        "m",
        additive(1_000_000, 0, 0, 0, 0),
        ADDITIVE,
    );
    u.money.saved_estimate = Some(MoneyFact {
        usd: 1.23,
        source: MoneySource::SavedCatalogEstimate,
        as_of: Some("2026-01-01".into()),
    });
    let newer = schedule(&[r#"{"billing_provider":"acme","model":"m","rates":{"input":99.0,"output":99.0,"cache_read":0.0,"cache_write_5m":0.0,"cache_write_1h":0.0}}"#.to_owned()]);
    let e = estimate(&u, &newer);
    assert_eq!(e.usd, Some(1.23));
    assert_eq!(e.source, Some(MoneySource::SavedCatalogEstimate));
    assert_eq!(e.as_of.as_deref(), Some("2026-01-01"));
}

#[test]
fn unknown_leg_keeps_complete_total_unknown() {
    let prices = schedule(&[
        r#"{"billing_provider":"acme","model":"m","rates":{"input":2.0,"output":10.0,"cache_write_5m":2.5,"cache_write_1h":4.0}}"#
            .to_owned(),
    ]);
    let u = usage(
        "r",
        Some("acme"),
        "m",
        additive(48_200, 12_800, 154_000, 6_000, 2_000),
        ADDITIVE,
    );
    let partial = estimate(&u, &prices);
    assert_eq!(partial.usd, None);
    assert!(partial.known_subtotal > 0.0);
    assert_eq!(partial.breakdown.cache_read, None);
    assert!((partial.known_subtotal - (0.2628 - 0.0154)).abs() < 1e-9);
    // An unused unpriced leg is fine.
    let unused = usage("r2", Some("acme"), "m", additive(10, 10, 0, 0, 0), ADDITIVE);
    assert!(estimate(&unused, &prices).usd.is_some());
    // A missing counter is unknown, not zero.
    let mut missing = additive(10, 10, 0, 0, 0);
    missing.cache_read = None;
    let m = estimate(&usage("r3", Some("acme"), "m", missing, ADDITIVE), &base());
    assert_eq!(m.usd, None);
}

#[test]
fn threshold_inclusivity_is_per_card() {
    let above = r#","threshold":{"input_tokens":272000,"inclusive":INC,"above":{"input":4.0,"output":20.0}}"#;
    let request = |tokens: u64| {
        usage(
            "r",
            Some("acme"),
            "m",
            TokenCategories {
                input: Some(tokens),
                output: Some(0),
                cache_read: Some(0),
                cache_write: Some(0),
                ..Default::default()
            },
            ADDITIVE,
        )
    };
    for (inclusive, at_edge_rate) in [("true", 2.0), ("false", 4.0)] {
        let prices = schedule(&[card("acme", "m", &above.replace("INC", inclusive))]);
        let at_edge = estimate(&request(272_000), &prices).usd.unwrap();
        assert!(
            (at_edge - 272_000.0 * at_edge_rate / 1e6).abs() < 1e-9,
            "{inclusive}"
        );
        let over = estimate(&request(272_001), &prices).usd.unwrap();
        assert!((over - 272_001.0 * 4.0 / 1e6).abs() < 1e-9, "{inclusive}");
    }
    // Cache reads and writes count toward the prompt length.
    let prices = schedule(&[card("acme", "m", &above.replace("INC", "true"))]);
    let cached = usage(
        "c",
        Some("acme"),
        "m",
        additive(1_000, 0, 271_000, 0, 1_000),
        ADDITIVE,
    );
    let e = estimate(&cached, &prices);
    // Prompt = 273,000 tokens: the upper tier has no read/write price, so those legs are unknown.
    assert_eq!(e.usd, None);
}

#[test]
fn session_aggregate_is_not_a_single_threshold_request() {
    let cards = [card(
        "acme",
        "m",
        r#","threshold":{"input_tokens":272000,"inclusive":true,"above":{"input":4.0,"output":20.0,"cache_read":0.2,"cache_write_5m":5.0,"cache_write_1h":8.0}}"#,
    )];
    let prices = schedule(&cards);
    let lower = additive(150_000, 0, 0, 0, 0);
    let mut ledger = UsageLedger::new(10, 1 << 20);
    for id in ["a", "b"] {
        let u = usage(id, Some("acme"), "m", lower.clone(), ADDITIVE);
        ledger.observe(UsageUpdate::Initial(&u), &prices, ReferencePolicy::Disabled);
    }
    let s = ledger.snapshot();
    // Two proven requests each stay in the lower tier: 300k tokens at $2/M.
    assert!((s.cost.usd.unwrap() - 0.6).abs() < 1e-9);
    assert_eq!(s.coverage.observed_requests, 2);

    let mut aggregate = usage(
        "s",
        Some("acme"),
        "m",
        additive(300_000, 0, 0, 0, 0),
        ADDITIVE,
    );
    aggregate.scope = UsageScope::Session;
    aggregate.request = None;
    let e = estimate(&aggregate, &prices);
    assert_eq!(
        e.usd, None,
        "a session aggregate cannot select a request tier"
    );
    assert!(e.note.is_some());
    // Without a threshold card the same aggregate is a plain linear price.
    assert!(estimate(&aggregate, &base()).usd.is_some());
}

#[test]
fn request_timestamp_selects_tier_and_tariff() {
    let tariff = r#","tariff":[
        {"name":"peak","days":[0,1,2,3,4],"start_minute":60,"end_minute":240,"rates":{"input":4.0,"output":20.0,"cache_read":0.2,"cache_write_5m":5.0,"cache_write_1h":8.0}}]"#;
    let prices = schedule(&[card("acme", "m", tariff)]);
    let request = |ms: Option<i64>| {
        let mut u = usage(
            "r",
            Some("acme"),
            "m",
            additive(1_000_000, 0, 0, 0, 0),
            ADDITIVE,
        );
        if let Some(ms) = ms {
            at(&mut u, ms);
        }
        u
    };
    let usd = |ms: Option<i64>| estimate(&request(ms), &prices).usd;
    // Friday 00:59:59.999 is off-peak; 01:00:00.000 is the first peak instant;
    // 03:59:59.999 is peak; 04:00:00.000 is off-peak again.
    assert_eq!(usd(Some(FRIDAY + 60 * MINUTE - 1)), Some(2.0));
    assert_eq!(usd(Some(FRIDAY + 60 * MINUTE)), Some(4.0));
    assert_eq!(usd(Some(FRIDAY + 240 * MINUTE - 1)), Some(4.0));
    assert_eq!(usd(Some(FRIDAY + 240 * MINUTE)), Some(2.0));
    // Saturday and Sunday are never peak.
    assert_eq!(
        usd(Some(FRIDAY + 24 * 60 * MINUTE + 90 * MINUTE)),
        Some(2.0)
    );
    // A scheduled tariff cannot be chosen without a request time.
    assert_eq!(usd(None), None);

    // Dated cards: the promotion ends at an exact UTC instant.
    let dated = schedule(&[
        r#"{"billing_provider":"acme","model":"m","effective_until":"2026-12-01","rates":{"input":1.0,"output":1.0,"cache_read":0.0,"cache_write_5m":0.0,"cache_write_1h":0.0}}"#.to_owned(),
        r#"{"billing_provider":"acme","model":"m","effective_from":"2026-12-01","rates":{"input":2.0,"output":2.0,"cache_read":0.0,"cache_write_5m":0.0,"cache_write_1h":0.0}}"#.to_owned(),
    ]);
    let dec1 = FRIDAY + 32 * 24 * 60 * MINUTE; // 2026-12-01T00:00:00Z
    let usd = |ms: Option<i64>| estimate(&request(ms), &dated).usd;
    assert_eq!(usd(Some(dec1 - 1)), Some(1.0));
    assert_eq!(usd(Some(dec1)), Some(2.0));
    assert_eq!(usd(None), None, "two dated cards need a request time");
}

#[test]
fn service_tier_and_region_are_unpriced_unless_the_card_knows_them() {
    let prices = schedule(&[card(
        "acme",
        "m",
        r#","service_multipliers":{"fast":2.0},"region_multipliers":{"us":1.1}"#,
    )]);
    let mut u = usage(
        "r",
        Some("acme"),
        "m",
        additive(1_000_000, 0, 0, 0, 0),
        ADDITIVE,
    );
    let usd = |u: &UsageObservation| estimate(u, &prices).usd;
    assert_eq!(usd(&u), Some(2.0));
    let meta = |u: &mut UsageObservation, tier: Option<&str>, region: Option<&str>| {
        let r = u.request.as_mut().unwrap();
        r.service_tier = tier.map(str::to_owned);
        r.inference_region = region.map(str::to_owned);
    };
    meta(&mut u, Some("standard"), Some("global"));
    assert_eq!(usd(&u), Some(2.0));
    meta(&mut u, Some("fast"), None);
    assert_eq!(usd(&u), Some(4.0));
    meta(&mut u, Some("fast"), Some("us"));
    assert!((usd(&u).unwrap() - 4.4).abs() < 1e-9);
    meta(&mut u, Some("priority-plus"), None);
    assert_eq!(usd(&u), None, "an unknown tier is not silently standard");
    meta(&mut u, None, Some("eu"));
    assert_eq!(usd(&u), None);
}

#[test]
fn billing_routes_are_exact_and_native_reference_is_policy_gated() {
    let prices = schedule(&[
        card("anthropic", "claude-x", ""),
        card("acme", "claude-x", ""),
    ]);
    let cats = additive(1_000_000, 0, 0, 0, 0);
    let routed = usage("r", Some("acme"), "claude-x", cats.clone(), ADDITIVE);
    assert!(estimate(&routed, &prices).usd.is_some());
    // Provider names are exact: no case folding or harness aliases.
    for alias in ["Anthropic", "ACME", "claude", "opencode"] {
        let u = usage("r", Some(alias), "claude-x", cats.clone(), ADDITIVE);
        assert_eq!(estimate(&u, &prices).usd, None, "{alias}");
    }
    // A recorded route that has no card never falls back to the native API.
    let unlisted = usage("r", Some("elsewhere"), "claude-x", cats.clone(), ADDITIVE);
    let native = ReferencePolicy::NativeApi(Harness::Claude);
    assert_eq!(estimate_cost(&unlisted, &prices, native).usd, None);
    // No recorded route: only an enabled native policy prices it, as a reference.
    let bare = usage("r", None, "claude-x", cats, ADDITIVE);
    assert_eq!(estimate(&bare, &prices).usd, None);
    assert_eq!(estimate_cost(&bare, &prices, native).usd, Some(2.0));
    let codex = ReferencePolicy::NativeApi(Harness::Codex);
    assert_eq!(estimate_cost(&bare, &prices, codex).usd, None);
}

#[test]
fn overrides_replace_the_whole_card_and_missing_legs_stay_unknown() {
    let bundled = schedule(&[card("acme", "m", "")]);
    let user = PriceOverride {
        card: PriceCard::new(
            "acme",
            "m",
            Rates {
                input: Some(1.0),
                output: Some(1.0),
                ..Rates::default()
            },
        ),
    };
    let prices = bundled.with_overrides(vec![user]).unwrap();
    let plain = usage(
        "r",
        Some("acme"),
        "m",
        additive(1_000_000, 1_000_000, 0, 0, 0),
        ADDITIVE,
    );
    let e = estimate(&plain, &prices);
    // Cache legs are zero tokens, so the override still prices; rates came from the user card.
    assert_eq!(e.usd, Some(2.0));
    assert_eq!(e.source, Some(MoneySource::UserOverride));
    let cached = usage("r2", Some("acme"), "m", additive(0, 0, 10, 0, 0), ADDITIVE);
    assert_eq!(
        estimate(&cached, &prices).usd,
        None,
        "no inherited bundled cache price"
    );
}

#[test]
fn invalid_cards_and_overlaps_are_rejected() {
    let parse = |cards: &str| {
        PriceSchedule::from_json(&format!(
            r#"{{"schema_version":1,"revision":"t","checked_at":"2026-10-30","cards":{cards}}}"#
        ))
    };
    assert!(matches!(
        parse(&format!("[{},{}]", card("a", "m", ""), card("a", "m", ""))),
        Err(PricingError::Duplicate { .. })
    ));
    let dated = |from: &str, until: &str| {
        card(
            "a",
            "m",
            &format!(r#","effective_from":"{from}","effective_until":"{until}""#),
        )
    };
    assert!(matches!(
        parse(&format!(
            "[{},{}]",
            dated("2026-01-01", "2026-06-01"),
            dated("2026-05-01", "2026-09-01")
        )),
        Err(PricingError::Overlap { .. })
    ));
    assert!(
        parse(&format!(
            "[{},{}]",
            dated("2026-01-01", "2026-06-01"),
            dated("2026-06-01", "2026-09-01")
        ))
        .is_ok()
    );
    for bad in [
        r#"{"billing_provider":"a","model":"m","rates":{"input":-1.0}}"#,
        r#"{"billing_provider":"","model":"m","rates":{}}"#,
        r#"{"billing_provider":"a","model":" ","rates":{}}"#,
        r#"{"billing_provider":"a","model":"m","rates":{},"threshold":{"input_tokens":0,"above":{}}}"#,
        r#"{"billing_provider":"a","model":"m","rates":{},"service_multipliers":{"fast":0.0}}"#,
        r#"{"billing_provider":"a","model":"m","rates":{},"tariff":[{"name":"x","days":[7],"start_minute":0,"end_minute":10,"rates":{}}]}"#,
        r#"{"billing_provider":"a","model":"m","rates":{},"effective_from":"2026-02-01","effective_until":"2026-01-01"}"#,
        r#"{"billing_provider":"a","model":"m","rates":{},"unknown_field":1}"#,
    ] {
        assert!(parse(&format!("[{bad}]")).is_err(), "{bad}");
    }
    assert!(matches!(
        PriceSchedule::from_json(
            r#"{"schema_version":2,"revision":"t","checked_at":"x","cards":[]}"#
        ),
        Err(PricingError::Schema(2))
    ));
}

#[test]
fn stream_revision_replaces_instead_of_adds() {
    let prices = base();
    let policy = ReferencePolicy::Disabled;
    let make = |output: u64| {
        usage(
            "req-1",
            Some("acme"),
            "m",
            TokenCategories {
                input: Some(10),
                output: Some(output),
                cache_read: Some(0),
                cache_write: Some(0),
                ..Default::default()
            },
            ADDITIVE,
        )
    };
    let (first, final_) = (make(3), make(9));
    let mut ledger = UsageLedger::new(10, 1 << 20);
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&first), &prices, policy),
        LedgerOutcome::Counted
    );
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&final_),
            &prices,
            policy
        ),
        LedgerOutcome::Revised
    );
    // Replays of the revised value never add again.
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&final_), &prices, policy),
        LedgerOutcome::Duplicate
    );
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&final_),
            &prices,
            policy
        ),
        LedgerOutcome::Duplicate
    );
    let s = ledger.snapshot();
    assert_eq!(s.categories.output, Some(9));
    assert_eq!(s.coverage.observed_requests, 1);
    assert!((s.cost.usd.unwrap() - (10.0 * 2.0 + 9.0 * 10.0) / 1e6).abs() < 1e-12);

    // A changed value is never an implicit revision.
    let mut ledger = UsageLedger::new(10, 1 << 20);
    ledger.observe(UsageUpdate::Initial(&first), &prices, policy);
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&final_), &prices, policy),
        LedgerOutcome::Uncountable
    );
    let s = ledger.snapshot();
    assert_eq!(s.categories.output, Some(3));
    assert!(!s.coverage.complete);
    assert_eq!(s.class, AggregationClass::Partial);
    assert_eq!(s.cost.usd, None);

    let mut ledger = UsageLedger::new(10, 1 << 20);
    ledger.observe(UsageUpdate::Initial(&first), &prices, policy);
    assert_eq!(
        ledger.observe(UsageUpdate::Conflict(&final_), &prices, policy),
        LedgerOutcome::Uncountable
    );
    assert!(!ledger.snapshot().coverage.complete);

    // A revision needs an existing target.
    let mut ledger = UsageLedger::new(10, 1 << 20);
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&final_),
            &prices,
            policy
        ),
        LedgerOutcome::Uncountable
    );
}

#[test]
fn revisions_keep_the_original_request_context() {
    let tariff = r#","tariff":[{"name":"peak","days":[4],"start_minute":60,"end_minute":240,"rates":{"input":4.0,"output":20.0,"cache_read":0.2,"cache_write_5m":5.0,"cache_write_1h":8.0}}]"#;
    let prices = schedule(&[card("acme", "m", tariff)]);
    let policy = ReferencePolicy::Disabled;
    let mut first = usage(
        "r",
        Some("acme"),
        "m",
        additive(1_000_000, 0, 0, 0, 0),
        ADDITIVE,
    );
    at(&mut first, FRIDAY + 90 * MINUTE);
    let mut revised = usage(
        "r",
        Some("acme"),
        "m",
        additive(2_000_000, 0, 0, 0, 0),
        ADDITIVE,
    );
    // The later revision carries no time; it must still price in the peak window.
    revised.request = Some(RequestMetadata {
        id: Some("r".into()),
        ..RequestMetadata::default()
    });
    let mut ledger = UsageLedger::new(10, 1 << 20);
    ledger.observe(UsageUpdate::Initial(&first), &prices, policy);
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&revised),
            &prices,
            policy
        ),
        LedgerOutcome::Revised
    );
    assert_eq!(ledger.snapshot().cost.usd, Some(8.0));
}

#[test]
fn ledger_is_bounded_and_marks_overflow_partial() {
    let prices = base();
    let mut ledger = UsageLedger::new(2, 1 << 20);
    for id in ["a", "b", "c"] {
        let u = usage(id, Some("acme"), "m", additive(1, 1, 0, 0, 0), ADDITIVE);
        ledger.observe(UsageUpdate::Initial(&u), &prices, ReferencePolicy::Disabled);
    }
    let s = ledger.snapshot();
    assert_eq!(s.coverage.observed_requests, 2);
    assert!(!s.coverage.complete);
    assert_eq!(s.class, AggregationClass::Partial);
    assert_eq!(
        s.cost.usd, None,
        "a bounded-out request is not a complete total"
    );
}

#[test]
fn tiny_costs_are_not_free() {
    let u = usage("r", Some("acme"), "m", additive(1, 0, 0, 0, 0), ADDITIVE);
    let usd = estimate(&u, &base()).usd.unwrap();
    assert!(usd > 0.0 && usd < 0.0001, "{usd}");
}

#[test]
fn bundled_catalog_loads_with_inventory() {
    let prices = PriceSchedule::bundled().expect("bundled catalog");
    assert!(!prices.is_empty());
    assert!(!prices.catalog().sources.is_empty());
    assert!(!prices.catalog().inventory.is_empty());
}

#[test]
fn ledger_byte_budget_charges_all_retained_state_not_just_the_id() {
    let prices = base();
    let policy = ReferencePolicy::Disabled;
    // A one-byte id would fit any id-only budget, but its entry, price context and
    // key are retained too.
    let tiny = usage("x", Some("acme"), "m", additive(1, 1, 0, 0, 0), ADDITIVE);
    let mut ledger = UsageLedger::new(1_000, 100);
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&tiny), &prices, policy),
        LedgerOutcome::Uncountable
    );
    assert_eq!(ledger.snapshot().coverage.observed_requests, 0);

    // With a modest budget only a bounded number of entries are admitted, long before
    // the entry cap, and the aggregate says it is partial.
    let mut ledger = UsageLedger::new(1_000_000, 8_192);
    let mut counted = 0;
    for i in 0..200 {
        let u = usage(
            &format!("r{i}"),
            Some("acme"),
            "m",
            additive(1, 1, 0, 0, 0),
            ADDITIVE,
        );
        if ledger.observe(UsageUpdate::Initial(&u), &prices, policy) == LedgerOutcome::Counted {
            counted += 1;
        }
    }
    assert!((1..40).contains(&counted), "{counted}");
    let s = ledger.snapshot();
    assert_eq!(s.coverage.observed_requests, counted);
    assert!(!s.coverage.complete);
    assert_eq!(s.class, AggregationClass::Partial);
    assert_eq!(s.cost.usd, None);

    // Strings the entry keeps (model identity, non-standard tier) cost budget too.
    let long_model = "m".repeat(4_000);
    let big_prices = schedule(&[card("acme", &long_model, "")]);
    let big = usage(
        "a",
        Some("acme"),
        &long_model,
        additive(1, 1, 0, 0, 0),
        ADDITIVE,
    );
    let mut ledger = UsageLedger::new(10, 3_000);
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&big), &big_prices, policy),
        LedgerOutcome::Uncountable,
        "an identity of a few bytes, a model name of thousands"
    );
}

#[test]
fn money_only_revisions_are_processed_and_changed_initials_are_not_duplicates() {
    let prices = base();
    let policy = ReferencePolicy::Disabled;
    let money = |usd: f64| RecordedMoney {
        reported_api: Some(MoneyFact {
            usd,
            source: MoneySource::ReportedApi,
            as_of: None,
        }),
        ..RecordedMoney::default()
    };
    let mut first = usage("r", Some("acme"), "m", additive(10, 10, 0, 0, 0), ADDITIVE);
    first.money = money(1.0);
    let mut second = first.clone();
    second.money = money(2.5);
    let mut ledger = UsageLedger::new(10, 1 << 20);
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&first), &prices, policy),
        LedgerOutcome::Counted
    );
    assert_eq!(ledger.snapshot().cost.usd, Some(1.0));
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&first), &prices, policy),
        LedgerOutcome::Duplicate
    );
    // Same counters, different money, ordinary Initial: never a silent duplicate.
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&second), &prices, policy),
        LedgerOutcome::Uncountable
    );
    assert!(!ledger.snapshot().coverage.complete);

    let mut ledger = UsageLedger::new(10, 1 << 20);
    ledger.observe(UsageUpdate::Initial(&first), &prices, policy);
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&second),
            &prices,
            policy
        ),
        LedgerOutcome::Revised
    );
    let s = ledger.snapshot();
    assert_eq!(s.cost.usd, Some(2.5));
    assert_eq!(s.reported_cost_usd, Some(2.5));
    assert_eq!(s.coverage.observed_requests, 1);
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&second),
            &prices,
            policy
        ),
        LedgerOutcome::Duplicate
    );
}

#[test]
fn revision_keeps_the_original_card_and_tier_after_a_schedule_change_and_threshold_cross() {
    let tiered = |input: f64, above: f64| {
        schedule(&[card(
            "acme",
            "m",
            &format!(
                r#","threshold":{{"input_tokens":272000,"inclusive":true,"above":{{"input":{above},"output":20.0,"cache_read":0.2,"cache_write_5m":5.0,"cache_write_1h":8.0}}}}"#
            ),
        )
        .replace(r#""input":2.0,"output":10.0"#, &format!(r#""input":{input},"output":10.0"#))])
    };
    let policy = ReferencePolicy::Disabled;
    let first = usage(
        "r",
        Some("acme"),
        "m",
        additive(150_000, 0, 0, 0, 0),
        ADDITIVE,
    );
    let revised = usage(
        "r",
        Some("acme"),
        "m",
        additive(300_000, 0, 0, 0, 0),
        ADDITIVE,
    );
    let mut ledger = UsageLedger::new(10, 1 << 20);
    ledger.observe(UsageUpdate::Initial(&first), &tiered(2.0, 4.0), policy);
    // The schedule changed (new base and upper rates) and the revision now exceeds the
    // threshold; the request keeps its original card and lower tier.
    let newer = tiered(9.0, 40.0);
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&revised),
            &newer,
            policy
        ),
        LedgerOutcome::Revised
    );
    let got = ledger.snapshot().cost.usd.unwrap();
    assert!((got - 300_000.0 * 2.0 / 1e6).abs() < 1e-12, "{got}");
    assert_eq!(ledger.snapshot().categories.input, Some(300_000));
}

#[test]
fn service_tiers_do_not_invent_unpublished_legs() {
    let batch = r#","service_multipliers":{"batch":0.5},"service_unpublished":{"batch":{"base":["cache_read"]}}"#;
    let prices = schedule(&[card("acme", "m", batch)]);
    let mut u = usage(
        "r",
        Some("acme"),
        "m",
        additive(1_000_000, 1_000_000, 1_000_000, 0, 0),
        ADDITIVE,
    );
    u.request.as_mut().unwrap().service_tier = Some("batch".into());
    let e = estimate(&u, &prices);
    assert_eq!(e.usd, None, "the batch cache-read price is not published");
    assert_eq!(e.breakdown.cache_read, None);
    assert_eq!(e.breakdown.input, Some(1.0));
    assert!((e.known_subtotal - 6.0).abs() < 1e-9);
    // A zero-token unpublished leg needs no price.
    let mut none = usage(
        "r2",
        Some("acme"),
        "m",
        additive(1_000_000, 0, 0, 0, 0),
        ADDITIVE,
    );
    none.request.as_mut().unwrap().service_tier = Some("batch".into());
    assert_eq!(estimate(&none, &prices).usd, Some(1.0));
    // Standard service still prices the leg.
    u.request.as_mut().unwrap().service_tier = None;
    assert!(estimate(&u, &prices).usd.is_some());
    // The bundled catalog carries the same distinction for a real row.
    let bundled = PriceSchedule::bundled().unwrap();
    let mut gpt = usage(
        "r3",
        Some("openai"),
        "gpt-4.1",
        additive(1_000_000, 0, 1_000_000, 0, 0),
        INCLUSIVE,
    );
    gpt.categories.cached_input = None;
    gpt.request.as_mut().unwrap().service_tier = Some("batch".into());
    let e = estimate(&gpt, &bundled);
    assert_eq!(
        e.usd, None,
        "gpt-4.1 batch cache read is unpublished: {e:?}"
    );
    assert_eq!(e.breakdown.cache_read, None);
    // Unpublished legs must name a defined tier and leg.
    assert!(
        PriceSchedule::from_json(&format!(
            r#"{{"schema_version":1,"revision":"t","checked_at":"x","cards":[{}]}}"#,
            card(
                "acme",
                "m",
                r#","service_unpublished":{"batch":{"base":["cache_read"]}}"#
            )
        ))
        .is_err()
    );
}

#[test]
fn overlapping_tariff_windows_are_rejected_but_adjacent_ones_load() {
    let window = |days: &str, start: u32, end: u32| {
        format!(
            r#"{{"name":"w","days":{days},"start_minute":{start},"end_minute":{end},"rates":{{"input":3.0}}}}"#
        )
    };
    let load = |windows: &[String]| {
        PriceSchedule::from_json(&format!(
            r#"{{"schema_version":1,"revision":"t","checked_at":"x","cards":[{}]}}"#,
            card(
                "acme",
                "m",
                &format!(r#","tariff":[{}]"#, windows.join(","))
            )
        ))
    };
    assert!(load(&[window("[0,1]", 0, 120), window("[1,2]", 60, 180)]).is_err());
    assert!(load(&[window("[0]", 0, 120), window("[0]", 119, 180)]).is_err());
    assert!(load(&[window("[0,1]", 0, 120), window("[1,2]", 120, 180)]).is_ok());
    assert!(load(&[window("[0]", 0, 120), window("[1]", 60, 180)]).is_ok());
}

#[test]
fn ledger_rejects_session_aggregates_and_keeps_stable_locators_as_identity() {
    let prices = base();
    let policy = ReferencePolicy::Disabled;
    let mut aggregate = usage("s", Some("acme"), "m", additive(100, 10, 0, 0, 0), ADDITIVE);
    aggregate.scope = UsageScope::Session;
    let mut ledger = UsageLedger::new(10, 1 << 20);
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&aggregate), &prices, policy),
        LedgerOutcome::Uncountable
    );
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&aggregate),
            &prices,
            policy
        ),
        LedgerOutcome::Uncountable
    );
    let s = ledger.snapshot();
    assert_eq!(s.coverage.observed_requests, 0);
    assert!(!s.coverage.complete);

    // Without a confirmed request id the stable record locator is the identity;
    // timestamps and text never are.
    let record = |offset: u64, started: i64, output: u64| {
        let mut u = usage(
            "ignored",
            Some("acme"),
            "m",
            additive(1, output, 0, 0, 0),
            ADDITIVE,
        );
        u.request = Some(RequestMetadata {
            started_at: Some(started),
            ..RequestMetadata::default()
        });
        u.provenance.locator.kind = agentscope::domain::LocatorKind::FileOffset {
            path: "synthetic.jsonl".into(),
            offset,
        };
        u
    };
    let mut ledger = UsageLedger::new(10, 1 << 20);
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&record(0, 1_000, 5)), &prices, policy),
        LedgerOutcome::Counted
    );
    // Same locator, different timestamp, same counters: duplicate.
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&record(0, 9_999, 5)), &prices, policy),
        LedgerOutcome::Duplicate
    );
    // Same locator, different counters: unattributable, not a second request.
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&record(0, 1_000, 6)), &prices, policy),
        LedgerOutcome::Uncountable
    );
    // Another offset is another request even with identical counters and time.
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&record(40, 1_000, 5)), &prices, policy),
        LedgerOutcome::Counted
    );
    assert_eq!(ledger.snapshot().coverage.observed_requests, 2);
}

#[test]
fn bundled_catalog_has_official_aliases_batch_and_context_capacities() {
    let prices = PriceSchedule::bundled().unwrap();
    for (alias, base) in [
        ("claude-sonnet-4-5-20250929", "claude-sonnet-4-5"),
        ("claude-opus-4-5-20251101", "claude-opus-4-5"),
        ("claude-haiku-4-5-20251001", "claude-haiku-4-5"),
    ] {
        let a = &prices
            .cards("anthropic", alias)
            .unwrap_or_else(|| panic!("{alias}"))[0];
        let b = &prices.cards("anthropic", base).unwrap()[0];
        assert_eq!(a.rates, b.rates, "{alias}");
    }
    for retired in [
        "claude-opus-4-1-20250805",
        "claude-opus-4-20250514",
        "claude-sonnet-4-20250514",
        "claude-3-5-haiku-20241022",
        "claude-mythos-5-1",
        "claude-mythos-5",
    ] {
        assert!(prices.cards("anthropic", retired).is_some(), "{retired}");
    }
    let mut u = usage(
        "r",
        Some("anthropic"),
        "claude-sonnet-5-5",
        additive(1_000_000, 1_000_000, 1_000_000, 0, 0),
        ADDITIVE,
    );
    let standard = estimate(&u, &prices).usd.unwrap();
    u.request.as_mut().unwrap().service_tier = Some("batch".into());
    let batch = estimate(&u, &prices).usd.unwrap();
    assert!(
        (batch - standard / 2.0).abs() < 1e-9,
        "{batch} vs {standard}"
    );
    for (provider, model) in [
        ("anthropic", "claude-opus-5-5"),
        ("anthropic", "claude-haiku-5-5"),
        ("openai", "gpt-5.4-mini"),
        ("openai", "gpt-4.1"),
        ("openai", "gpt-6.1-sol"),
    ] {
        assert!(
            prices.model_context_capacity(provider, model).is_some(),
            "{model}"
        );
    }
}

#[test]
fn a_revision_that_grows_retained_pricing_state_is_checked_against_the_budget() {
    let policy = ReferencePolicy::Disabled;
    let unpriced = schedule(&[card("acme", "other", "")]);
    let huge = "d".repeat(6_000);
    let priced = schedule(&[card("acme", "m", &format!(r#","as_of":"{huge}""#))]);
    let first = usage("r", Some("acme"), "m", additive(1, 1, 0, 0, 0), ADDITIVE);
    let revised = usage("r", Some("acme"), "m", additive(1, 9, 0, 0, 0), ADDITIVE);

    // The first observation has no price context (unpriced); the replacement would
    // select one whose metadata no longer fits the budget.
    let mut ledger = UsageLedger::new(10, 3_000);
    assert_eq!(
        ledger.observe(UsageUpdate::Initial(&first), &unpriced, policy),
        LedgerOutcome::Counted
    );
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&revised),
            &priced,
            policy
        ),
        LedgerOutcome::Uncountable
    );
    let s = ledger.snapshot();
    assert_eq!(
        s.categories.output,
        Some(1),
        "the rejected revision changed nothing"
    );
    assert!(!s.coverage.complete);

    // The same revision is accepted when the budget has room for that state.
    let mut ledger = UsageLedger::new(10, 100_000);
    ledger.observe(UsageUpdate::Initial(&first), &unpriced, policy);
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&revised),
            &priced,
            policy
        ),
        LedgerOutcome::Revised
    );
    assert_eq!(ledger.snapshot().categories.output, Some(9));
}

#[test]
fn aggregate_metadata_stays_charged_when_a_revision_drops_the_entry_money_string() {
    let prices = base();
    let policy = ReferencePolicy::Disabled;
    let long = "d".repeat(6_000);
    let money = |as_of: Option<&str>| RecordedMoney {
        reported_api: Some(MoneyFact {
            usd: 1.0,
            source: MoneySource::ReportedApi,
            as_of: as_of.map(str::to_owned),
        }),
        ..RecordedMoney::default()
    };
    let request = |id: &str, m: RecordedMoney| {
        let mut u = usage(id, Some("acme"), "m", additive(1, 1, 0, 0, 0), ADDITIVE);
        u.money = m;
        u
    };
    // Smallest budget that admits `n` plain requests, i.e. the marginal entry cost.
    let min_budget = |ok: &dyn Fn(usize) -> bool| {
        let (mut lo, mut hi) = (1usize, 1 << 20);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if ok(mid) { hi = mid } else { lo = mid + 1 }
        }
        lo
    };
    let plain = |n: usize| {
        move |max: usize| {
            let mut l = UsageLedger::new(100, max);
            (0..n).all(|i| {
                let u = request(&format!("p{i}"), RecordedMoney::default());
                l.observe(UsageUpdate::Initial(&u), &base(), ReferencePolicy::Disabled)
                    == LedgerOutcome::Counted
            })
        }
    };
    let entry = min_budget(&plain(2)) - min_budget(&plain(1));
    assert!((200..4_000).contains(&entry), "{entry}");

    // The first request retains a long as-of in its money and in both aggregates.
    let first = request("r", money(Some(&long)));
    let initial = min_budget(&|max| {
        UsageLedger::new(100, max).observe(UsageUpdate::Initial(&first), &prices, policy)
            == LedgerOutcome::Counted
    });
    let mut ledger = UsageLedger::new(100, initial);
    ledger.observe(UsageUpdate::Initial(&first), &prices, policy);
    // Same amount, no as-of: the entry's own string is released, but the total and
    // per-model aggregates still hold the 6,000-byte as-of, which stays charged.
    let revised = request("r", money(None));
    assert_eq!(
        ledger.observe(
            UsageUpdate::AuthoritativeReplacement(&revised),
            &prices,
            policy
        ),
        LedgerOutcome::Revised
    );
    let mut admitted = 0;
    for i in 0..60 {
        let u = request(&format!("n{i}"), RecordedMoney::default());
        if ledger.observe(UsageUpdate::Initial(&u), &prices, policy) == LedgerOutcome::Counted {
            admitted += 1;
        }
    }
    // Freed room is only the released money string (plus slack): about 6,000 bytes.
    assert!(
        admitted * entry <= 6_000 + entry,
        "admitted {admitted} entries of ~{entry} bytes after releasing only ~6,000"
    );
    assert!(admitted < 60);
}
