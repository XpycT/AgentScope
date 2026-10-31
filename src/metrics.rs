//! Measured timing, whole-request speed, prompt-context occupancy, and
//! compaction facts (spec §8). Everything here is derived from recorded values;
//! a missing, invalid, zero, or non-finite input leaves the fact Unknown.

use std::collections::{HashMap, HashSet};

use crate::domain::{
    AggregationClass, Event, ModelDetail, PageAnchor, PageRequest, RequestPurpose, Timestamp,
    UsageObservation, UsageScope,
};
pub use crate::domain::{
    CompactionObservation, CompactionScope, CompactionSummary, ContextUsage, SessionDetailPage,
    SessionMetrics, SessionTiming, SpeedSummary, TimedRate, TurnInterval,
};
use crate::pricing::{PriceSchedule, ReferencePolicy, estimate_cost};
use crate::usage::{
    Identity, LedgerOutcome, UsageLedger, UsageUpdate, identity, normalized_categories,
};

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

/// Union of the complete, valid intervals in milliseconds. Overlap counts once.
/// Open intervals (no end) and inverted ones contribute nothing, so they can never
/// become unbounded active time; the caller decides whether that makes coverage
/// incomplete. `None` when no valid complete interval exists or arithmetic overflows.
pub fn union_active_ms(intervals: &[TurnInterval]) -> Option<u64> {
    let mut spans: Vec<(i64, i64)> = intervals
        .iter()
        .filter_map(|i| {
            i.ended_at
                .filter(|end| *end >= i.started_at)
                .map(|e| (i.started_at, e))
        })
        .collect();
    if spans.is_empty() {
        return None;
    }
    spans.sort_unstable();
    let mut total: u64 = 0;
    let (mut start, mut end) = spans[0];
    for &(s, e) in &spans[1..] {
        if s <= end {
            end = end.max(e);
        } else {
            total = total.checked_add(span_ms(start, end)?)?;
            (start, end) = (s, e);
        }
    }
    total.checked_add(span_ms(start, end)?)
}

fn span_ms(start: Timestamp, end: Timestamp) -> Option<u64> {
    u64::try_from(end.checked_sub(start)?).ok()
}

/// Builds measured session timing. `elapsed_ms` needs both a recorded start and a
/// recorded end; `active_complete` is set only when the caller vouches for full
/// interval coverage (`coverage_complete`), no interval is open or invalid, and
/// some active time was measured.
pub fn session_timing(
    started_at: Option<Timestamp>,
    ended_at: Option<Timestamp>,
    intervals: &[TurnInterval],
    coverage_complete: bool,
    model_ms: Option<f64>,
) -> SessionTiming {
    let active_ms = union_active_ms(intervals);
    let clean = intervals
        .iter()
        .all(|i| i.ended_at.is_some_and(|e| e >= i.started_at));
    SessionTiming {
        started_at,
        ended_at,
        elapsed_ms: started_at.zip(ended_at).and_then(|(s, e)| span_ms(s, e)),
        active_ms,
        model_ms: model_ms.filter(|m| m.is_finite() && *m >= 0.0),
        active_complete: coverage_complete && clean && active_ms.is_some(),
    }
}

/// Intervals kept per session by [`TurnTracker`].
const MAX_INTERVALS: usize = 4096;

/// Streaming turn-boundary recorder for adapters. A start with no stop is an open
/// turn; stops with no start, overlong histories, and restarts mark coverage
/// incomplete instead of inventing a span.
#[derive(Clone, Debug, Default)]
pub(crate) struct TurnTracker {
    done: Vec<TurnInterval>,
    open: Option<Timestamp>,
    incomplete: bool,
    first: Option<Timestamp>,
}

impl TurnTracker {
    pub(crate) fn start(&mut self, at: Option<Timestamp>) {
        let Some(at) = at else {
            self.incomplete = true;
            return;
        };
        if self.open.is_some() {
            // The earlier turn never recorded a stop.
            self.incomplete = true;
        }
        self.first = Some(self.first.map_or(at, |f| f.min(at)));
        self.open = Some(at);
    }

    pub(crate) fn end(&mut self, at: Option<Timestamp>) {
        let started = self.open.take();
        let (Some(started_at), Some(at)) = (started, at) else {
            self.incomplete = true;
            return;
        };
        if at < started_at {
            self.incomplete = true;
            return;
        }
        if self.done.len() >= MAX_INTERVALS {
            self.incomplete = true;
        } else {
            self.done.push(TurnInterval {
                started_at,
                ended_at: Some(at),
            });
        }
    }

    /// Coverage cannot be proven for this source (turn spans are only sometimes recorded).
    pub(crate) fn mark_incomplete(&mut self) {
        self.incomplete = true;
    }

    /// Measured timing. A turn stop is not a session end, so `ended_at` (and with
    /// it `elapsed_ms`) stays Unknown; an open turn leaves coverage incomplete.
    pub(crate) fn timing(&self, started_at: Option<Timestamp>) -> SessionTiming {
        let mut intervals = self.done.clone();
        if let Some(started_at) = self.open {
            intervals.push(TurnInterval {
                started_at,
                ended_at: None,
            });
        }
        session_timing(
            started_at.or(self.first),
            None,
            &intervals,
            !self.incomplete,
            None,
        )
    }
}

// ---------------------------------------------------------------------------
// Speed
// ---------------------------------------------------------------------------

/// Durations at or below this are treated as unmeasurable for throughput.
const MIN_DURATION_MS: f64 = 100.0;

fn counts_as_request(u: &UsageObservation) -> bool {
    u.scope != UsageScope::Session
        && u.request
            .as_ref()
            .is_none_or(|r| r.purpose == RequestPurpose::Conversation)
}

/// Recorded positive output and a finite duration above 100 ms, else `None`.
/// Reasoning is included or excluded per the observation's accounting, never twice.
fn timed_parts(u: &UsageObservation) -> Option<(u64, f64)> {
    let r = u.request.as_ref()?;
    let duration = r
        .duration_ms
        .filter(|d| d.is_finite() && *d > MIN_DURATION_MS)?;
    let output = normalized_categories(u).ok()?.output.filter(|o| *o > 0)?;
    Some((output, duration))
}

/// Whole-request output tokens per second; the first token's latency is part of the
/// duration and is not subtracted. `None` for anything unmeasurable.
pub fn request_rate(observation: &UsageObservation) -> Option<f64> {
    if !counts_as_request(observation) {
        return None;
    }
    let (output, duration) = timed_parts(observation)?;
    let rate = output as f64 / (duration / 1000.0);
    rate.is_finite().then_some(rate)
}

fn timed_rate(u: &UsageObservation) -> Option<TimedRate> {
    let tokens_per_second = request_rate(u)?;
    let (output_tokens, duration_ms) = timed_parts(u)?;
    let r = u.request.as_ref()?;
    Some(TimedRate {
        model: u.model.clone(),
        request_id: r.id.clone(),
        started_at: r.started_at,
        completed_at: r.completed_at,
        output_tokens,
        duration_ms,
        ttft_ms: r.ttft_ms.filter(|t| t.is_finite() && *t >= 0.0),
        tokens_per_second,
    })
}

/// Whether `a` was recorded later than `b`. Recorded completion decides; iteration
/// order never does unless nothing distinguishes the two.
fn newer(a: &TimedRate, b: &TimedRate) -> bool {
    (a.completed_at, a.started_at, &a.request_id) > (b.completed_at, b.started_at, &b.request_id)
}

#[derive(Default)]
struct Totals {
    output: u64,
    duration_ms: f64,
    measured: usize,
    total: usize,
    skipped: bool,
    latest: Option<TimedRate>,
}

impl Totals {
    fn add_timed(&mut self, rate: &TimedRate) -> bool {
        let Some(output) = self.output.checked_add(rate.output_tokens) else {
            self.skipped = true;
            return false;
        };
        self.output = output;
        self.duration_ms += rate.duration_ms;
        self.measured += 1;
        if self.latest.as_ref().is_none_or(|l| newer(rate, l)) {
            self.latest = Some(rate.clone());
        }
        true
    }

    fn remove_timed(&mut self, rate: &TimedRate) {
        self.output = self.output.saturating_sub(rate.output_tokens);
        self.measured = self.measured.saturating_sub(1);
        self.duration_ms = if self.measured == 0 {
            0.0
        } else {
            (self.duration_ms - rate.duration_ms).max(0.0)
        };
    }

    fn summary(&self) -> SpeedSummary {
        let weighted = (self.measured > 0 && self.duration_ms > 0.0)
            .then(|| self.output as f64 / (self.duration_ms / 1000.0))
            .filter(|w| w.is_finite());
        SpeedSummary {
            latest: self.latest.clone(),
            weighted_tokens_per_second: weighted,
            measured_output: self.output,
            measured_duration_ms: self.duration_ms,
            measured_requests: self.measured,
            total_requests: self.total,
            complete: !self.skipped && self.total > 0 && self.measured == self.total,
        }
    }
}

/// Finalized-iterator speed: weighted output over the summed durations of the
/// requests that carry a measurement, with coverage over all conversation
/// requests. Callers own deduplication; revisions go through [`SpeedAccumulator`].
pub fn measured_speed<'a>(
    requests: impl IntoIterator<Item = &'a UsageObservation>,
) -> SpeedSummary {
    let mut totals = Totals::default();
    for u in requests {
        if !counts_as_request(u) {
            continue;
        }
        totals.total += 1;
        if let Some(rate) = timed_rate(u) {
            totals.add_timed(&rate);
        }
    }
    totals.summary()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeedOutcome {
    Counted,
    Revised,
    Duplicate,
    Uncountable,
}

/// Streaming, revision-aware speed aggregation. One compact contribution is
/// retained per request identity (the same resolver as [`UsageLedger`]); request
/// bodies are not kept.
pub struct SpeedAccumulator {
    max_entries: usize,
    entries: HashMap<Identity, Option<TimedRate>>,
    totals: Totals,
    dropped: usize,
}

impl SpeedAccumulator {
    pub fn new(max_entries: usize) -> SpeedAccumulator {
        SpeedAccumulator {
            max_entries,
            entries: HashMap::new(),
            totals: Totals::default(),
            dropped: 0,
        }
    }

    /// First contribution for an identity. An identical replay is a duplicate; the
    /// same identity with different measurements is not a second request.
    pub fn observe(&mut self, request: &UsageObservation) -> SpeedOutcome {
        if !counts_as_request(request) {
            return SpeedOutcome::Uncountable;
        }
        let Some(id) = identity(request) else {
            return SpeedOutcome::Uncountable;
        };
        let rate = timed_rate(request);
        if let Some(old) = self.entries.get(&id) {
            return if *old == rate {
                SpeedOutcome::Duplicate
            } else {
                SpeedOutcome::Uncountable
            };
        }
        if self.entries.len() >= self.max_entries {
            self.dropped += 1;
            return SpeedOutcome::Uncountable;
        }
        if let Some(rate) = &rate
            && !self.totals.add_timed(rate)
        {
            return SpeedOutcome::Uncountable;
        }
        self.totals.total += 1;
        self.entries.insert(id, rate);
        SpeedOutcome::Counted
    }

    /// An authoritative replacement of an already counted request: the old
    /// contribution is subtracted and the new one added, exactly once.
    pub fn revise(&mut self, request: &UsageObservation) -> SpeedOutcome {
        if !counts_as_request(request) {
            return SpeedOutcome::Uncountable;
        }
        let Some(id) = identity(request) else {
            return SpeedOutcome::Uncountable;
        };
        let rate = timed_rate(request);
        let Some(old) = self.entries.get(&id) else {
            return SpeedOutcome::Uncountable;
        };
        if *old == rate {
            return SpeedOutcome::Duplicate;
        }
        let old = old.clone();
        if let Some(old) = &old {
            self.totals.remove_timed(old);
        }
        if let Some(new) = &rate
            && !self.totals.add_timed(new)
        {
            // Overflow: keep the old contribution rather than losing the request.
            if let Some(old) = &old {
                self.totals.add_timed(old);
            }
            return SpeedOutcome::Uncountable;
        }
        self.entries.insert(id, rate);
        self.refresh_latest();
        SpeedOutcome::Revised
    }

    /// The latest pointer may reference a replaced contribution; rescan the bounded map.
    fn refresh_latest(&mut self) {
        let mut best: Option<&TimedRate> = None;
        for rate in self.entries.values().flatten() {
            if best.is_none_or(|b| newer(rate, b)) {
                best = Some(rate);
            }
        }
        self.totals.latest = best.cloned();
    }

    pub fn snapshot(&self) -> SpeedSummary {
        let mut summary = self.totals.summary();
        if self.dropped > 0 {
            summary.complete = false;
        }
        summary
    }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

/// Builds a confirmed occupancy snapshot. A zero window is not a capacity, and a
/// snapshot with neither figure is Unknown. There is no total-tokens fallback.
pub fn context_usage(
    used: Option<u64>,
    capacity: Option<u64>,
    class: AggregationClass,
    recorded_at: Option<Timestamp>,
    model: Option<ModelDetail>,
    compaction_epoch: Option<String>,
) -> ContextUsage {
    let capacity = capacity.filter(|c| *c > 0);
    ContextUsage {
        used,
        capacity,
        class: if used.is_none() && capacity.is_none() {
            AggregationClass::Unknown
        } else {
            class
        },
        recorded_at,
        model,
        compaction_epoch,
    }
}

/// A snapshot taken in an earlier compaction epoch no longer describes the
/// prompt: the occupancy becomes Unknown until a newer snapshot arrives. The
/// window (a model property) is kept.
pub fn current_context(snapshot: &ContextUsage, current_epoch: Option<&str>) -> ContextUsage {
    if snapshot.compaction_epoch.as_deref() == current_epoch {
        return snapshot.clone();
    }
    ContextUsage {
        used: None,
        recorded_at: None,
        class: if snapshot.capacity.is_some() {
            snapshot.class
        } else {
            AggregationClass::Unknown
        },
        compaction_epoch: current_epoch.map(str::to_owned),
        ..snapshot.clone()
    }
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

/// Tokens removed from context by one compaction; `None` unless both are recorded
/// and the history actually shrank. Not billed request usage.
pub fn tokens_removed(c: &CompactionObservation) -> Option<u64> {
    c.before_tokens?.checked_sub(c.after_tokens?)
}

/// Summarizes compactions. Only `Event`-scope markers are counted as observed
/// compactions. Money comes from one non-overlapping plane: the per-event plane
/// when every event is priced, else a known `Aggregate` amount (never added to
/// event money, never divided among events), else the partial per-event subtotal.
/// Pricing reuses the request-aware ledger and estimator.
pub fn compaction_summary<'a>(
    events: impl IntoIterator<Item = &'a CompactionObservation>,
    history_complete: bool,
    prices: &PriceSchedule,
    policy: ReferencePolicy,
) -> CompactionSummary {
    let mut seen_events: HashSet<&str> = HashSet::new();
    let mut seen_aggregates: HashSet<&str> = HashSet::new();
    let mut ledger = UsageLedger::new(usize::MAX, isize::MAX as usize);
    let mut latest: Option<&CompactionObservation> = None;
    let mut observed = 0usize;
    let mut priced = 0usize;
    let mut event_subtotal = 0.0f64;
    let mut aggregate_sum = 0.0f64;
    let mut aggregate_seen = 0usize;
    let mut aggregate_unknown = false;

    for c in events {
        match c.scope {
            CompactionScope::Event => {
                if !seen_events.insert(c.id.as_str()) {
                    continue;
                }
                observed += 1;
                if latest.is_none_or(|l| (c.timestamp, &c.id) > (l.timestamp, &l.id)) {
                    latest = Some(c);
                }
                let Some(usage) = &c.usage else { continue };
                if ledger.observe(UsageUpdate::Initial(usage), prices, policy)
                    == LedgerOutcome::Counted
                {
                    priced += 1;
                }
            }
            CompactionScope::Aggregate => {
                if !seen_aggregates.insert(c.id.as_str()) {
                    continue;
                }
                aggregate_seen += 1;
                match c
                    .usage
                    .as_ref()
                    .and_then(|u| estimate_cost(u, prices, policy).usd)
                    .filter(|v| v.is_finite() && *v >= 0.0)
                {
                    Some(v) => aggregate_sum += v,
                    None => aggregate_unknown = true,
                }
            }
            CompactionScope::Unknown => {}
        }
    }

    let snapshot = ledger.snapshot();
    let priced_events = snapshot.coverage.priced_requests.min(priced);
    event_subtotal += snapshot.cost.known_subtotal;
    let unpriced = observed - priced_events;
    let aggregate_known = aggregate_seen > 0 && !aggregate_unknown;
    let (known_cost_usd, cost_scope) = if observed > 0 && unpriced == 0 {
        (Some(event_subtotal), CompactionScope::Event)
    } else if aggregate_known {
        (Some(aggregate_sum), CompactionScope::Aggregate)
    } else if priced_events > 0 {
        (Some(event_subtotal), CompactionScope::Event)
    } else {
        (None, CompactionScope::Unknown)
    };
    CompactionSummary {
        observed,
        history_complete,
        known_cost_usd,
        cost_scope,
        unpriced,
        latest: latest.cloned(),
    }
}

// ---------------------------------------------------------------------------
// Detail paging
// ---------------------------------------------------------------------------

fn usage_bytes(u: &UsageObservation) -> usize {
    std::mem::size_of::<UsageObservation>()
        + u.model.as_ref().map_or(0, |m| m.id.len())
        + u.request
            .as_ref()
            .and_then(|r| r.id.as_ref())
            .map_or(0, String::len)
}

fn compaction_bytes(c: &CompactionObservation) -> usize {
    std::mem::size_of::<CompactionObservation>()
        + c.id.len()
        + c.method.as_ref().map_or(0, String::len)
        + c.trigger.as_ref().map_or(0, String::len)
        + c.summary.as_ref().map_or(0, |s| s.preview.len())
        + c.usage.as_ref().map_or(0, usage_bytes)
}

fn event_bytes(e: &Event) -> usize {
    std::mem::size_of::<Event>()
        + e.id.len()
        + e.text.as_ref().map_or(0, |t| t.preview.len())
        + e.usage.as_ref().map_or(0, usage_bytes)
}

/// Pages request, compaction and event records together: the combined record count
/// and estimated bytes obey `request`. The first record is always returned so
/// paging progresses; when it alone exceeds the byte budget `truncated` is set.
pub fn page_details(
    requests: &[UsageObservation],
    compactions: &[CompactionObservation],
    recent: &[Event],
    request: PageRequest,
) -> SessionDetailPage {
    let max_events = request.max_events.max(1);
    let mut page = SessionDetailPage::default();
    let mut count = 0usize;
    let mut bytes = 0usize;
    let mut order = 0u64;
    let total = requests.len() + compactions.len() + recent.len();

    macro_rules! take {
        ($items:expr, $field:ident, $size:expr, $stamp:expr, $id:expr) => {
            for item in $items {
                let cost = $size(item);
                if count > 0
                    && (count >= max_events || bytes.saturating_add(cost) > request.max_bytes)
                {
                    page.has_more = true;
                    break;
                }
                if count == 0 && cost > request.max_bytes {
                    page.truncated = true;
                }
                count += 1;
                bytes = bytes.saturating_add(cost);
                page.next = Some(PageAnchor {
                    timestamp: $stamp(item),
                    source_order: order,
                    stable_id: $id(item),
                });
                order += 1;
                page.$field.push(item.clone());
            }
        };
    }

    take!(
        requests,
        requests,
        usage_bytes,
        |u: &UsageObservation| u.request.as_ref().and_then(|r| r.completed_at),
        |u: &UsageObservation| u
            .request
            .as_ref()
            .and_then(|r| r.id.clone())
            .unwrap_or_default()
    );
    if !page.has_more {
        take!(
            compactions,
            compactions,
            compaction_bytes,
            |c: &CompactionObservation| c.timestamp,
            |c: &CompactionObservation| c.id.clone()
        );
    }
    if !page.has_more {
        take!(
            recent,
            recent,
            event_bytes,
            |e: &Event| e.timestamp,
            |e: &Event| e.id.clone()
        );
    }
    if count < total {
        page.has_more = true;
    }
    if !page.has_more {
        page.next = None;
    }
    page
}
