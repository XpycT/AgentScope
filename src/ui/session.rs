//! Session information overlay: token categories, pricing provenance, server
//! tools, and lineage. Pure: reads `App`, draws into a `Frame`; every value is
//! display-safe before it is drawn.

use ratatui::Frame;
use ratatui::widgets::Paragraph;

use crate::app::{App, DetailsState};
use crate::domain::{
    AggregationClass, CompactionObservation, CompactionScope, CompactionSummary, ContextUsage,
    CostBreakdown, Lifecycle, ModelDetail, MoneySource, OriginSurface, SessionLink,
    SessionMetadata, SessionSummary, TimedRate, Timestamp, TokenCategories, UsageSummary,
};
use crate::ingest::text::display_safe;
use crate::time::relative_label;

use super::RosterContext;
use super::details::{chrome, draw_rows, put, wrap_into};
use super::inspector::{class_name, cost_text, grouped, money_text, process_line};
use super::widgets::{ABSENT, LOADING, harness_name, tokens_text};

fn count(value: Option<u64>) -> String {
    value.map_or_else(|| "—".into(), grouped)
}

/// One-line category breakdown shared by the session view and event details.
pub(super) fn categories_text(c: &TokenCategories) -> String {
    format!(
        "input {} · cached input {} · cache write {} (5m {} · 1h {}) · cache read {} · output {} · reasoning {} · total {}",
        count(c.input),
        count(c.cached_input),
        count(c.cache_write),
        count(c.cache_write_5m),
        count(c.cache_write_1h),
        count(c.cache_read),
        count(c.output),
        count(c.reasoning),
        count(c.total),
    )
}

pub(super) fn link_text(link: &Option<SessionLink>) -> String {
    match link {
        None => "none".into(),
        Some(SessionLink::Resolved(key)) => format!(
            "resolved — {} {}",
            harness_name(key.harness),
            display_safe(&key.native_id)
        ),
        Some(SessionLink::Ambiguous) => "ambiguous — more than one candidate session".into(),
        Some(SessionLink::Unresolved(id)) => {
            format!(
                "unresolved — id {} not found in the roster",
                display_safe(id)
            )
        }
    }
}

fn lifecycle_text(l: Lifecycle) -> &'static str {
    match l {
        Lifecycle::Unknown => "unknown",
        Lifecycle::Ongoing => "ongoing",
        Lifecycle::Completed => "completed",
        Lifecycle::Failed => "failed",
        Lifecycle::Cancelled => "cancelled",
    }
}

/// Per-leg dollars. A `None` leg is unknown or unused, never a free amount.
fn legs_text(b: &CostBreakdown) -> String {
    [
        ("input", b.input),
        ("cache read", b.cache_read),
        ("cache write 5m", b.cache_write_5m),
        ("cache write 1h", b.cache_write_1h),
        ("cache write other", b.cache_write_other),
        ("output", b.output),
        ("reasoning", b.reasoning),
    ]
    .iter()
    .map(|(label, usd)| {
        usd.map_or_else(
            || format!("{label} Unknown"),
            |v| format!("{label} {}", money_text(v)),
        )
    })
    .collect::<Vec<_>>()
    .join(" · ")
}

fn when(ts: Option<Timestamp>) -> String {
    ts.map_or_else(|| "Unknown".into(), crate::time::format_local)
}

/// `70_000` -> `1m 10s`; never a bare zero for a missing measurement.
fn span_text(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms} ms");
    }
    let secs = ms / 1_000;
    let (days, secs) = (secs / 86_400, secs % 86_400);
    let (hours, secs) = (secs / 3_600, secs % 3_600);
    let (minutes, secs) = (secs / 60, secs % 60);
    let mut out = String::new();
    for (value, unit) in [(days, 'd'), (hours, 'h'), (minutes, 'm')] {
        if value > 0 {
            out.push_str(&format!("{value}{unit} "));
        }
    }
    out.push_str(&format!("{secs}s"));
    out
}

fn measured_span(ms: Option<f64>) -> String {
    ms.and_then(|v| (v.is_finite() && v >= 0.0).then_some(v))
        .map_or_else(|| "Unknown".into(), |v| span_text(v.round() as u64))
}

fn rate_text(rate: Option<f64>) -> String {
    match rate {
        Some(r) if r.is_finite() && r > 0.0 => format!("{r:.1} tok/s"),
        _ => "Unknown".into(),
    }
}

/// A headline value and its qualifier, shared by the roster row, the preview and
/// the details view so no surface prices, times or rates anything itself.
pub struct Fact {
    /// The compact value printed in the roster column or preview line.
    pub head: String,
    /// Provenance shown beside the value where the surface has room for it.
    pub sub: String,
    /// The value is real but no longer describes the current model.
    pub quiet: bool,
}

impl Fact {
    fn new(head: impl Into<String>, sub: impl Into<String>) -> Self {
        Self {
            head: head.into(),
            sub: sub.into(),
            quiet: false,
        }
    }

    /// Facts of a metadata-only row: not read yet, which is not zero.
    fn loading() -> Self {
        Self::new(LOADING, "loading")
    }

    /// Hydrated, but the source recorded no value: absent, which is not zero.
    fn absent() -> Self {
        Self::new(ABSENT, "")
    }
}

/// `1h 5m`, `3m 12s`, `45s`: two units at most, for narrow columns.
fn short_span(ms: u64) -> String {
    let secs = ms / 1_000;
    match secs {
        0 if ms < 1_000 => format!("{ms}ms"),
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m {}s", secs / 60, secs % 60),
        3_600..=86_399 => format!("{}h {}m", secs / 3_600, secs % 3_600 / 60),
        _ => format!("{}d {}h", secs / 86_400, secs % 86_400 / 3_600),
    }
}

/// Lifetime tokens. A partial total is a lower bound, not the whole.
pub fn tokens_fact(usage: Option<&UsageSummary>, hydrated: bool) -> Fact {
    if !hydrated {
        return Fact::loading();
    }
    let Some(usage) = usage else {
        return Fact::absent();
    };
    match (usage.total, usage.class) {
        (None, class) => Fact::new(ABSENT, class_name(class)),
        (Some(total), AggregationClass::Partial) => {
            Fact::new(format!("{}+", tokens_text(Some(total))), "partial")
        }
        (Some(total), class) => Fact::new(tokens_text(Some(total)), class_name(class)),
    }
}

/// Legs whose tokens were recorded but could not be priced.
fn missing_legs(usage: &UsageSummary) -> Vec<&'static str> {
    let c = &usage.categories;
    let b = &usage.cost.breakdown;
    let used = |n: Option<u64>| n.is_some_and(|n| n > 0);
    [
        ("input", b.input, used(c.input)),
        ("cache read", b.cache_read, used(c.cache_read)),
        ("cache write 5m", b.cache_write_5m, used(c.cache_write_5m)),
        ("cache write 1h", b.cache_write_1h, used(c.cache_write_1h)),
        (
            "cache write",
            b.cache_write_other,
            used(c.cache_write) && c.cache_write_5m.is_none() && c.cache_write_1h.is_none(),
        ),
        ("output", b.output, used(c.output)),
        ("reasoning", b.reasoning, used(c.reasoning)),
    ]
    .into_iter()
    .filter(|(_, price, used)| *used && price.is_none())
    .map(|(label, _, _)| label)
    .collect()
}

/// API-equivalent estimate. Only a complete amount reads as a total; a priced
/// subset reads as a lower bound with its missing coverage.
pub fn cost_fact(usage: Option<&UsageSummary>, hydrated: bool) -> Fact {
    if !hydrated {
        return Fact::loading();
    }
    let Some(usage) = usage else {
        return Fact::absent();
    };
    let cost = &usage.cost;
    match (cost.usd, cost.class) {
        (Some(usd), AggregationClass::Estimated) => Fact::new(
            format!("~{}", money_text(usd)),
            if cost.reference {
                "reference estimate"
            } else {
                "estimated"
            },
        ),
        (Some(usd), class) if class != AggregationClass::Unknown => {
            Fact::new(money_text(usd), class_name(class))
        }
        _ if cost.known_subtotal > 0.0 => {
            let cov = &usage.coverage;
            let sub = if cov.observed_requests > 0 && cov.priced_requests < cov.observed_requests {
                format!("partial {}/{}", cov.priced_requests, cov.observed_requests)
            } else {
                "partial".to_string()
            };
            Fact::new(format!("{}+", money_text(cost.known_subtotal)), sub)
        }
        _ => Fact::absent(),
    }
}

/// Measured active time; an incomplete history is a lower bound.
pub fn active_fact(s: &SessionSummary, hydrated: bool) -> Fact {
    if !hydrated {
        return Fact::loading();
    }
    let t = &s.metrics.timing;
    match t.active_ms {
        Some(ms) if t.active_complete => Fact::new(short_span(ms), "active"),
        Some(ms) => Fact::new(format!("{}+", short_span(ms)), "partial"),
        None => Fact::absent(),
    }
}

/// The latest speed was measured on a model other than the session's current one.
fn speed_is_stale(s: &SessionSummary, latest: &TimedRate) -> bool {
    let current = s.meta.model.as_ref();
    latest
        .model
        .as_ref()
        .is_some_and(|lm| current.is_some_and(|cm| cm.id != lm.id || cm.provider != lm.provider))
}

/// Latest timed whole-request speed with its model and age. Unknown is never 0.
pub fn speed_fact(s: &SessionSummary, hydrated: bool, now: Timestamp) -> Fact {
    if !hydrated {
        return Fact::loading();
    }
    let Some(latest) = &s.metrics.speed.latest else {
        return Fact::absent();
    };
    let rate = latest.tokens_per_second;
    if !rate.is_finite() || rate <= 0.0 {
        return Fact::absent();
    }
    let age = latest
        .completed_at
        .or(latest.started_at)
        .map(|t| format!("{} ago", relative_label(t, now)));
    let model = latest
        .model
        .as_ref()
        .map(|m| super::widgets::one_line(&m.id));
    let stale = speed_is_stale(s, latest);
    let mut sub = match (stale, model) {
        (true, Some(model)) => format!("old {model}"),
        (true, None) => "old model".to_string(),
        (false, Some(model)) => model,
        (false, None) => String::new(),
    };
    if let Some(age) = age {
        if !sub.is_empty() {
            sub.push_str(" · ");
        }
        sub.push_str(&age);
    }
    Fact {
        quiet: stale,
        ..Fact::new(format!("{rate:.1}"), sub)
    }
}

/// Occupancy of the recorded window. Without both figures there is no percentage.
pub(super) fn context_use(c: &ContextUsage) -> String {
    match (c.used, c.capacity) {
        (Some(used), Some(window)) if window > 0 => {
            format!("{}% of window", used.saturating_mul(100) / window)
        }
        (Some(_), _) => "Unknown (window not recorded)".into(),
        _ => "Unknown".into(),
    }
}

fn model_label(model: &Option<ModelDetail>) -> String {
    model
        .as_ref()
        .map_or_else(|| "Unknown".into(), |m| display_safe(&m.id))
}

/// Recorded billing/routing route, never the harness. Unknown when the provider
/// omitted it; it is not filled in from the harness or a price table.
fn billing_provider(model: &Option<ModelDetail>) -> String {
    model
        .as_ref()
        .and_then(|m| m.provider.as_deref())
        .map_or_else(|| "Unknown".into(), display_safe)
}

/// Model identity, with effort directly below the model and the raw variant kept
/// separate. A missing effort is Unknown, never a default.
fn model_lines(model: &Option<ModelDetail>) -> Vec<String> {
    let Some(m) = model else {
        return vec![
            "Model: Unknown".into(),
            "Effort: Unknown".into(),
            "Variant: Unknown".into(),
        ];
    };
    let mut out = vec![
        format!("Model: {}", display_safe(&m.id)),
        format!(
            "Effort: {}",
            m.effort
                .as_deref()
                .map_or_else(|| "Unknown".into(), display_safe)
        ),
        format!(
            "Variant: {}",
            m.variant
                .as_deref()
                .map_or_else(|| "Unknown".into(), display_safe)
        ),
    ];
    if let Some(configured) = m.configured_effort.as_deref() {
        out.push(format!("Configured effort: {}", display_safe(configured)));
    }
    out
}

fn coverage_note(complete: bool) -> &'static str {
    if complete {
        ""
    } else {
        " (incomplete coverage)"
    }
}

/// Measured timing, speed, occupancy and compaction evidence. Every figure is
/// either recorded or labeled Unknown; a partial history is never shown as zero.
fn measured_lines(s: &SessionSummary) -> Vec<String> {
    let m = &s.metrics;
    let mut out = vec![String::new(), "# Measured".into()];

    let t = &m.timing;
    out.push(format!(
        "Started: {}   Ended: {}   Elapsed: {}",
        when(t.started_at),
        when(t.ended_at),
        t.elapsed_ms.map_or_else(|| "Unknown".into(), span_text),
    ));
    out.push(format!(
        "Active: {}{}   Model time: {}",
        t.active_ms.map_or_else(|| "Unknown".into(), span_text),
        coverage_note(t.active_complete),
        measured_span(t.model_ms),
    ));

    let sp = &m.speed;
    match &sp.latest {
        Some(latest) => {
            let stale = speed_is_stale(s, latest);
            out.push(format!(
                "Latest speed: {} — {}{} at {}",
                rate_text(Some(latest.tokens_per_second)),
                model_label(&latest.model),
                if stale {
                    " (measured on an earlier model)"
                } else {
                    ""
                },
                when(latest.completed_at.or(latest.started_at)),
            ));
            out.push(format!(
                "  Latest request: {} output in {} · TTFT {}",
                grouped(latest.output_tokens),
                measured_span(Some(latest.duration_ms)),
                measured_span(latest.ttft_ms),
            ));
        }
        None => out.push("Latest speed: Unknown".into()),
    }
    if sp.measured_requests == 0 {
        out.push("Weighted speed: Unknown — no measured requests".into());
    } else {
        out.push(format!(
            "Weighted speed: {} — {} output over {} across {} of {} requests{}",
            rate_text(sp.weighted_tokens_per_second),
            grouped(sp.measured_output),
            measured_span(Some(sp.measured_duration_ms)),
            sp.measured_requests,
            sp.total_requests,
            coverage_note(sp.complete),
        ));
    }

    let c = &m.context;
    out.push(format!(
        "Context: {} of {} ({}, recorded {}{})",
        c.used.map_or_else(|| "Unknown".into(), grouped),
        c.capacity.map_or_else(
            || "Unknown window".into(),
            |w| format!("{} window", tokens_text(Some(w)))
        ),
        class_name(c.class),
        when(c.recorded_at),
        c.compaction_epoch
            .as_deref()
            .map_or_else(String::new, |e| format!(", epoch {}", display_safe(e))),
    ));
    out.push(format!("Context use: {}", context_use(c)));
    out.push(format!("Context model: {}", model_label(&c.model)));

    let cm = &m.compactions;
    out.push(compactions_line(cm));
    if let Some(latest) = &cm.latest {
        let method = latest
            .method
            .as_deref()
            .map_or_else(|| "Unknown".into(), display_safe);
        let trigger = latest
            .trigger
            .as_deref()
            .map_or_else(|| "Unknown".into(), display_safe);
        out.push(format!(
            "Latest compaction: {} · method {method} · trigger {trigger} · tokens removed {}",
            when(latest.timestamp),
            crate::metrics::tokens_removed(latest).map_or_else(|| "Unknown".into(), grouped),
        ));
        out.push(format!(
            "  Latest compaction event cost: {}",
            compaction_event_cost(latest)
        ));
    }
    out
}

fn usage_lines(usage: &UsageSummary) -> Vec<String> {
    let mut out = vec![format!(
        "Tokens: {} ({})",
        usage.total.map_or_else(|| "Unknown".into(), grouped),
        class_name(usage.class)
    )];
    for m in &usage.models {
        let id = display_safe(&m.model.id);
        match &m.model.provider {
            Some(p) => out.push(format!("Priced model: {}/{id}", display_safe(p))),
            None => out.push(format!("Priced model: {id}")),
        }
    }
    let c = &usage.categories;
    out.push("Categories:".into());
    for (label, value) in [
        ("Input", c.input),
        ("Cached input", c.cached_input),
        ("Cache write", c.cache_write),
        ("  Cache write 5m", c.cache_write_5m),
        ("  Cache write 1h", c.cache_write_1h),
        ("Cache read", c.cache_read),
        ("Output", c.output),
        ("Reasoning", c.reasoning),
        ("Total", c.total),
    ] {
        out.push(format!("  {label}: {}", count(value)));
    }
    out.push(format!("Cost: {}", cost_text(Some(usage))));
    out.push(format!(
        "Cost by category: {}",
        legs_text(&usage.cost.breakdown)
    ));
    out.push(format!(
        "Pricing as of: {}",
        usage
            .cost
            .as_of
            .as_deref()
            .map_or_else(|| "—".into(), display_safe)
    ));
    if let Some(note) = &usage.cost.note {
        out.push(format!("Cost note: {}", display_safe(note)));
    }
    if usage.cost.reference {
        out.push(
            "Reference API estimate: no billing route was recorded; the provider was not invented."
                .into(),
        );
    }
    if usage.cost.class == AggregationClass::Estimated {
        out.push("Cost is an estimate from the local price table, not a billed amount.".into());
        let basis = match usage.cost.source {
            Some(MoneySource::UserOverride) => Some("your price override"),
            Some(MoneySource::SavedCatalogEstimate) => Some("an estimate saved by the source"),
            Some(MoneySource::BundledReference) if usage.cost.reference => Some(
                "the bundled official price catalog, applied as a reference API estimate \
                 because no billing route was recorded",
            ),
            Some(MoneySource::BundledReference) if usage.cost.reference => Some(
                "the bundled official price catalog, applied as a reference API estimate \
                 because no billing route was recorded",
            ),
            Some(MoneySource::BundledReference) => Some("the bundled official price catalog"),
            _ => None,
        };
        if let Some(basis) = basis {
            out.push(format!("Estimate basis: {basis}"));
        }
    }
    if let Some(reported) = usage.reported_cost_usd
        && usage.cost.class != AggregationClass::Reported
    {
        out.push(format!(
            "Reported by source: {} (not an API-equivalent amount)",
            money_text(reported)
        ));
    }
    if usage.cost.usd.is_none() && usage.cost.known_subtotal > 0.0 {
        out.push(format!(
            "Known subtotal: {} (incomplete, not a total)",
            money_text(usage.cost.known_subtotal)
        ));
        let missing = missing_legs(usage);
        if !missing.is_empty() {
            out.push(format!("Unpriced coverage: {}", missing.join(", ")));
        }
    }
    let coverage = &usage.coverage;
    if coverage.observed_requests > 0 {
        if coverage.priced_requests < coverage.observed_requests {
            out.push(format!(
                "Priced requests: {} of {} (incomplete coverage)",
                coverage.priced_requests, coverage.observed_requests
            ));
        } else {
            out.push(format!(
                "Priced requests: all {} priced",
                coverage.observed_requests
            ));
        }
    }
    if let Some(reason) = &coverage.reason {
        out.push(format!("Coverage note: {}", display_safe(reason)));
    }
    out
}

/// Latest recorded origin surface with its (sanitized) raw label.
pub(super) fn origin_label(meta: &SessionMetadata) -> String {
    let Some(origin) = meta.origins.iter().max_by_key(|o| o.recorded_at) else {
        return "Unknown".into();
    };
    let surface = match origin.surface {
        OriginSurface::Cli => "CLI",
        OriginSurface::DesktopCode => "Desktop Code",
        OriginSurface::Ide => "IDE",
        OriginSurface::Cowork => "Cowork",
        OriginSurface::Other => "Other",
        OriginSurface::Unknown => "Unknown",
    };
    // Only the surface's own name is shown; the recorded raw label stays in the data.
    surface.to_string()
}

/// Session-level compaction line. Event cost, aggregate cost and context
/// reduction are separate facts; an aggregate is never split across events.
pub(super) fn compactions_line(cm: &CompactionSummary) -> String {
    let observed = match (cm.observed, cm.history_complete) {
        (0, true) => "none observed".to_string(),
        (n, true) => format!("{n} observed"),
        (n, false) => format!("{n} observed (history incomplete)"),
    };
    let cost = match (cm.known_cost_usd, cm.cost_scope) {
        (Some(usd), CompactionScope::Event) if cm.unpriced > 0 || !cm.history_complete => {
            format!("{} known subtotal (not a total)", money_text(usd))
        }
        (Some(usd), CompactionScope::Event) => money_text(usd),
        (Some(usd), CompactionScope::Aggregate) => {
            format!("{} aggregate (not attributed per event)", money_text(usd))
        }
        _ => "Unknown".into(),
    };
    format!(
        "Compactions: {observed} · cost {cost} · {} unpriced",
        cm.unpriced
    )
}

/// Money the source recorded for one compaction event, never a computed price.
fn compaction_event_cost(c: &CompactionObservation) -> String {
    match c.scope {
        CompactionScope::Event => {
            let money = c.usage.as_ref().and_then(|u| {
                u.money
                    .reported_api
                    .as_ref()
                    .map(|m| (m.usd, "reported"))
                    .or_else(|| {
                        u.money
                            .saved_estimate
                            .as_ref()
                            .map(|m| (m.usd, "saved estimate"))
                    })
            });
            money.map_or_else(
                || "not recorded for this event".into(),
                |(usd, label)| format!("{} ({label})", money_text(usd)),
            )
        }
        CompactionScope::Aggregate => "session aggregate only, not attributed to this event".into(),
        CompactionScope::Unknown => "Unknown".into(),
    }
}

/// Context before and after one compaction: a reduction of the window, not a bill.
fn compaction_reduction(c: &CompactionObservation) -> String {
    let token = |v: Option<u64>| v.map_or_else(|| "Unknown".into(), grouped);
    format!(
        "context {} → {} (removed {})",
        token(c.before_tokens),
        token(c.after_tokens),
        token(crate::metrics::tokens_removed(c))
    )
}

/// How the on-demand detail pages are shown.
pub(super) struct PageView<'a> {
    pub state: &'a DetailsState,
    /// Compaction summaries are shown in full.
    pub expanded: bool,
}

/// Most characters of one recent event shown in the details view.
const RECENT_EVENT_CHARS: usize = 120;

/// Most characters of a collapsed compaction summary.
const COLLAPSED_SUMMARY_CHARS: usize = 100;

/// Rows for the paged record-level facts: request usage and compaction events.
fn page_lines(view: &PageView<'_>) -> Vec<String> {
    let state = view.state;
    let page = state.page();
    let mut out = vec![String::new(), "# Detailed facts".into()];
    if state.loading().is_some() {
        out.push(format!(
            "Loading detailed facts… ({} records read)",
            state.progress().records
        ));
    } else if state.failed() {
        out.push("Detailed facts could not be read; the facts above stay as recorded.".into());
    }
    out.push(format!(
        "{} requests · {} compactions · {} events loaded{}{}",
        page.requests.len(),
        page.compactions.len(),
        page.recent.len(),
        if page.has_more {
            " — older records exist (m loads the next page)"
        } else {
            ""
        },
        if page.truncated {
            " — page truncated by its budget"
        } else {
            ""
        },
    ));
    if !page.recent.is_empty() {
        out.push("Recent events (in loaded order, one line each):".into());
    }
    for event in &page.recent {
        let body = event
            .text
            .as_ref()
            .map(|t| super::widgets::one_line(&t.preview))
            .unwrap_or_default();
        let mut short: String = body.chars().take(RECENT_EVENT_CHARS).collect();
        if body.chars().count() > RECENT_EVENT_CHARS {
            short.push('…');
        }
        out.push(format!(
            "Event {} · {} · {short}",
            when(event.timestamp),
            super::inspector::actor_label(event)
        ));
    }
    for req in &page.requests {
        let meta = req.request.as_ref();
        let at = meta.and_then(|m| m.completed_at.or(m.started_at));
        let effort = req
            .model
            .as_ref()
            .and_then(|m| m.effort.as_deref())
            .map_or_else(|| "Unknown".into(), display_safe);
        out.push(format!(
            "Request {} · {} · effort {effort} · input {} · output {} · TTFT {}",
            when(at),
            model_label(&req.model),
            count(req.categories.input),
            count(req.categories.output),
            measured_span(meta.and_then(|m| m.ttft_ms)),
        ));
    }
    for c in &page.compactions {
        let method = c
            .method
            .as_deref()
            .map_or_else(|| "Unknown".into(), display_safe);
        out.push(format!(
            "Compaction {} · method {method} · {}",
            when(c.timestamp),
            compaction_reduction(c)
        ));
        out.push(format!("  Event cost: {}", compaction_event_cost(c)));
        if let Some(summary) = &c.summary {
            let safe = display_safe(&summary.preview);
            if view.expanded {
                out.push(format!("  Summary: {safe}"));
            } else {
                let mut short: String = safe
                    .chars()
                    .take(COLLAPSED_SUMMARY_CHARS)
                    .map(|c| if c == '\n' { ' ' } else { c })
                    .collect();
                if safe.chars().count() > COLLAPSED_SUMMARY_CHARS {
                    short.push('…');
                }
                out.push(format!("  Summary: {short} (Space expands)"));
            }
        }
    }
    out
}

/// Session facts for the roster preview. Same typed helpers as the details view.
pub(super) fn preview_lines(s: &SessionSummary, hydrated: bool, now: Timestamp) -> Vec<String> {
    let m = &s.meta;
    let model = m.model.as_ref();
    let mut out = vec![
        display_safe(&m.title),
        format!("Session: {}", display_safe(&m.key.native_id)),
        format!(
            "Harness: {} · Billing provider: {} · Origin: {}",
            harness_name(m.key.harness),
            billing_provider(&m.model),
            origin_label(m)
        ),
        format!(
            "Model: {} · Effort: {}",
            model_label(&m.model),
            model
                .and_then(|x| x.effort.as_deref())
                .map_or_else(|| "Unknown".into(), display_safe)
        ),
    ];
    if let Some(project) = m.project.as_deref().map(display_safe) {
        out.push(format!("Project: {project}"));
    }
    out.push(String::new());
    let tokens = tokens_fact(s.usage.as_ref(), hydrated);
    let cost = cost_fact(s.usage.as_ref(), hydrated);
    let active = active_fact(s, hydrated);
    let speed = speed_fact(s, hydrated, now);
    for (label, fact) in [
        ("Tokens", &tokens),
        ("API estimate", &cost),
        ("Active", &active),
        ("Latest speed", &speed),
    ] {
        if fact.sub.is_empty() {
            out.push(format!("{label}: {}", fact.head));
        } else {
            out.push(format!("{label}: {} ({})", fact.head, fact.sub));
        }
    }
    if let Some(usage) = &s.usage
        && hydrated
    {
        out.push(format!(
            "Categories: {}",
            categories_text(&usage.categories)
        ));
        if usage.cost.usd.is_none() && usage.cost.known_subtotal > 0.0 {
            let missing = missing_legs(usage);
            if !missing.is_empty() {
                out.push(format!("Unpriced coverage: {}", missing.join(", ")));
            }
        }
        if let Some(as_of) = usage.cost.as_of.as_deref() {
            out.push(format!("Pricing as of: {}", display_safe(as_of)));
        }
        let basis = match usage.cost.source {
            Some(MoneySource::UserOverride) => Some("your price override"),
            Some(MoneySource::SavedCatalogEstimate) => Some("an estimate saved by the source"),
            Some(MoneySource::BundledReference) => Some("the bundled official price catalog"),
            _ => None,
        };
        if let Some(basis) = basis {
            out.push(format!("Estimate basis: {basis}"));
        }
    }
    if let Some(tools) = s.server_tools
        && (tools.web_search > 0 || tools.web_fetch > 0)
    {
        out.push(format!(
            "Excluded from estimate: server tools (web_search {} · web_fetch {})",
            tools.web_search, tools.web_fetch
        ));
    }
    out.push(String::new());
    if hydrated {
        out.push(format!(
            "Context: {} · window {}",
            context_use(&s.metrics.context),
            s.metrics
                .context
                .capacity
                .map_or_else(|| "Unknown".into(), |w| tokens_text(Some(w)))
        ));
        out.push(compactions_line(&s.metrics.compactions));
        if let Some(latest) = &s.metrics.compactions.latest {
            out.push(format!(
                "Latest compaction: {}",
                compaction_reduction(latest)
            ));
            out.push(format!("  Event cost: {}", compaction_event_cost(latest)));
        }
    } else {
        out.push("Context and compactions: loading".into());
    }
    out.push(format!(
        "Lifecycle: {} · Process: {}",
        lifecycle_text(s.lifecycle),
        process_line(&s.process)
    ));
    out
}

pub(super) fn session_rows(
    s: &SessionSummary,
    parent: &Option<SessionLink>,
    fork: &Option<SessionLink>,
    width: usize,
    page: Option<&PageView<'_>>,
) -> Vec<String> {
    let model = model_lines(&s.meta.model);
    let mut text: Vec<String> = vec![
        format!(
            "Session: {}   Provider: {}",
            display_safe(&s.meta.key.native_id),
            harness_name(s.meta.key.harness)
        ),
        format!(
            "Billing provider: {}   Origin: {}",
            billing_provider(&s.meta.model),
            origin_label(&s.meta)
        ),
        format!(
            "Lifecycle: {}   Unfinished work: {}",
            lifecycle_text(s.lifecycle),
            if s.unfinished_work { "yes" } else { "no" }
        ),
        format!("Process: {}", process_line(&s.process)),
        String::new(),
        "# Model".into(),
    ];
    text.extend(model);
    text.push(String::new());
    text.push("# Usage".into());
    match &s.usage {
        Some(usage) => text.extend(usage_lines(usage)),
        None => text.push("No usage recorded for this session.".into()),
    }
    match s.server_tools {
        Some(t) => {
            text.push(format!(
                "Server tools: web_search {} · web_fetch {}",
                t.web_search, t.web_fetch
            ));
            if t.web_search > 0 || t.web_fetch > 0 {
                text.push("Server tool calls are not included in the API estimate.".into());
            }
        }
        None => text.push("Server tools: Unknown".into()),
    }
    text.push(String::new());
    text.push("# Lineage".into());
    text.push(format!("Parent: {}", link_text(parent)));
    text.push(format!("Fork: {}", link_text(fork)));
    text.push(format!(
        "Agent: {}",
        s.meta
            .lineage
            .agent_type
            .as_deref()
            .map_or_else(|| "Unknown".into(), display_safe)
    ));
    text.push(format!(
        "Sidechain: {}",
        if s.meta.lineage.sidechain {
            "yes"
        } else {
            "no"
        }
    ));
    text.extend(measured_lines(s));
    if let Some(page) = page {
        text.extend(page_lines(page));
    }
    let mut rows = Vec::new();
    for line in &text {
        if wrap_into(&mut rows, line, width) {
            break;
        }
    }
    rows
}

pub fn render_session(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let Some(f) = chrome(frame, ctx) else { return };
    let Some(row) = app.transcript().and_then(|t| app.session(t.key())) else {
        frame.render_widget(
            Paragraph::new("Session is no longer in the roster."),
            f.body,
        );
        return;
    };
    let key = &row.session.meta.key;
    let page = (app.details().key() == Some(key)).then(|| PageView {
        state: app.details(),
        expanded: app.summaries_expanded(),
    });
    let rows = session_rows(
        row.session,
        &app.resolved_parent(key),
        &app.resolved_fork(key),
        f.width,
        page.as_ref(),
    );
    draw_rows(frame, app, &rows, app.session_scroll(), f.body);
    put(
        frame,
        &f,
        "< Session: usage and lineage",
        "Usage is persisted evidence; cost is derived only from the dated price table.",
        "j/k scroll  m older details  Space expand summaries  Esc back  q quit",
    );
}
