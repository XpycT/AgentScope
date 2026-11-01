//! Aggregate usage reconciliation for native Hermes accounting rows.
//!
//! Hermes writes cumulative main-loop counters on the `sessions` row and a
//! per-(session, model, provider, task) ledger on `session_model_usage`. The two
//! main sources are alternatives, never addends: a complete main per-model ledger
//! duplicates the session aggregate. Auxiliary tasks (`compression`,
//! `title_generation`, ...) are written only to the per-model ledger and are added
//! once. The documented per-turn route `voice_chat` *is* included in the session
//! totals, so it is treated as session-scope rather than an auxiliary addend.
//!
//! Costs are read through the producer's `cost_status`:
//! `actual` authorizes `actual_cost_usd`, `estimated` authorizes the saved
//! estimate, `included` is a subscription charge (not a free API call), and an
//! unknown/missing status never promotes the column default `0` into "free".
//! Request-only machinery (`UsageLedger`, speed samples) is deliberately not used:
//! these rows cover an aggregate scope with an explicit native api-call count.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::domain::{
    AggregationClass, CostBreakdown, CostEstimate, InputAccounting, ModelDetail, ModelUsageSummary,
    MoneyFact, MoneySource, OutputAccounting, Provenance, RecordedMoney, SessionKey,
    SessionMetadata, TokenAccounting, TokenCategories, UsageCoverage, UsageObservation, UsageScope,
    UsageSummary,
};
use crate::pricing::{PriceSchedule, PricedFacts, ReferencePolicy, estimate_facts};

/// Producer `cost_status` values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HermesCostStatus {
    Actual,
    Estimated,
    Included,
    #[default]
    Unknown,
}

impl HermesCostStatus {
    pub fn parse(text: Option<&str>) -> Self {
        match text.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("actual") => Self::Actual,
            Some("estimated") => Self::Estimated,
            Some("included") => Self::Included,
            _ => Self::Unknown,
        }
    }
}

/// Typed native token legs. `input` is the non-cached input; cache reads/writes
/// sit beside it (the producer's `CanonicalUsage.prompt_tokens` sums them), and
/// `reasoning` is a subset label of `output`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HermesTokens {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning: Option<u64>,
}

impl HermesTokens {
    pub fn categories(self) -> TokenCategories {
        TokenCategories {
            input: self.input,
            cached_input: None,
            cache_write: self.cache_write,
            cache_write_5m: None,
            cache_write_1h: None,
            cache_read: self.cache_read,
            output: self.output,
            reasoning: self.reasoning,
            total: None,
            orchestration_input: None,
            orchestration_output: None,
            orchestration_cache_read: None,
        }
    }

    pub fn accounting() -> TokenAccounting {
        TokenAccounting {
            input: InputAccounting::AdditiveCaches,
            output: OutputAccounting::IncludesReasoning,
        }
    }

    /// Lifetime tokens when every core leg is known.
    pub fn total(self) -> Option<u64> {
        [self.input, self.cache_read, self.cache_write, self.output]
            .into_iter()
            .try_fold(0u64, |acc, leg| acc.checked_add(leg?))
    }

    pub(crate) fn from_row(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Self> {
        let read = |index: usize| -> rusqlite::Result<Option<u64>> {
            let value: Option<i64> = row.get(index)?;
            Ok(value.and_then(|v| u64::try_from(v).ok()))
        };
        Ok(Self {
            input: read(at)?,
            output: read(at + 1)?,
            cache_read: read(at + 2)?,
            cache_write: read(at + 3)?,
            reasoning: read(at + 4)?,
        })
    }
}

/// Typed native money and its producer status.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HermesMoney {
    pub status: HermesCostStatus,
    pub estimated_usd: Option<f64>,
    pub actual_usd: Option<f64>,
}

impl HermesMoney {
    pub(crate) fn from_row(
        row: &rusqlite::Row<'_>,
        estimated: usize,
        actual: usize,
        status: usize,
    ) -> rusqlite::Result<Self> {
        let value = |index: usize| -> rusqlite::Result<Option<f64>> {
            let value: Option<f64> = row.get(index)?;
            Ok(value.filter(|v| v.is_finite() && *v >= 0.0))
        };
        let status: Option<String> = row.get(status)?;
        Ok(Self {
            status: HermesCostStatus::parse(status.as_deref()),
            estimated_usd: value(estimated)?,
            actual_usd: value(actual)?,
        })
    }

    pub(crate) fn recorded(self) -> RecordedMoney {
        match self.status {
            HermesCostStatus::Actual => RecordedMoney {
                reported_api: self.actual_usd.map(|usd| MoneyFact {
                    usd,
                    source: MoneySource::ReportedApi,
                    as_of: None,
                }),
                saved_estimate: None,
                unverified_usd: None,
            },
            HermesCostStatus::Estimated => RecordedMoney {
                reported_api: None,
                saved_estimate: self.estimated_usd.map(|usd| MoneyFact {
                    usd,
                    source: MoneySource::SavedCatalogEstimate,
                    as_of: None,
                }),
                unverified_usd: None,
            },
            HermesCostStatus::Included => RecordedMoney {
                // Subscription-included: the invoice is zero, but the API-equivalent
                // value is not.
                reported_api: Some(MoneyFact {
                    usd: 0.0,
                    source: MoneySource::ReportedApi,
                    as_of: None,
                }),
                saved_estimate: None,
                unverified_usd: None,
            },
            HermesCostStatus::Unknown => RecordedMoney {
                // The default 0 column is not evidence of a free call.
                reported_api: None,
                saved_estimate: None,
                unverified_usd: self.estimated_usd.filter(|usd| *usd > 0.0),
            },
        }
    }
}

/// The `sessions` row's cumulative accounting for one physical segment.
#[derive(Clone, Debug, PartialEq)]
pub struct HermesSessionTotals {
    pub key: SessionKey,
    pub model: Option<ModelDetail>,
    pub api_call_count: Option<u64>,
    pub tokens: HermesTokens,
    pub money: HermesMoney,
}

/// Opaque native row identity for deduplicating one physical usage row without
/// exposing denied routing columns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct HermesRowId(pub u64);

/// One `session_model_usage` row: a main-loop route (`task == ""`), the documented
/// `voice_chat` per-turn route, or a separately evidenced auxiliary task.
#[derive(Clone, Debug, PartialEq)]
pub struct HermesModelTotals {
    pub row: HermesRowId,
    pub key: SessionKey,
    pub model: Option<ModelDetail>,
    pub task: String,
    pub api_call_count: Option<u64>,
    pub tokens: HermesTokens,
    pub money: HermesMoney,
}

impl HermesModelTotals {
    /// One aggregate-scope observation for a ledger row (used by the compaction
    /// details page). It is never treated as one request.
    pub(crate) fn observation(
        &self,
        _prices: &PriceSchedule,
        provenance: Provenance,
    ) -> UsageObservation {
        UsageObservation {
            model: self.model.clone(),
            scope: UsageScope::Session,
            categories: self.tokens.categories(),
            accounting: HermesTokens::accounting(),
            request: None,
            money: self.money.recorded(),
            provenance,
        }
    }
}

/// Where a contribution came from, kept for honest detail display.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HermesUsageScope {
    /// Complete main per-model ledger rows.
    MainRows,
    /// The session aggregate stood in for incomplete main rows.
    MainAggregate,
    /// Separately evidenced auxiliary ledger rows.
    Auxiliary,
}

/// One reconciled contribution, exposed with its actual native api-call coverage.
#[derive(Clone, Debug, PartialEq)]
pub struct HermesUsageContribution {
    pub scope: HermesUsageScope,
    pub key: SessionKey,
    pub model: Option<ModelDetail>,
    pub task: String,
    pub api_call_count: Option<u64>,
    pub categories: TokenCategories,
    pub status: HermesCostStatus,
    /// Actual provider-reported charge authorized by `cost_status=actual`.
    pub reported_usd: Option<f64>,
    /// Saved estimate authorized by `cost_status=estimated`.
    pub saved_estimate_usd: Option<f64>,
    pub cost: CostEstimate,
}

/// A summary plus the aggregate-scope contributions behind it.
#[derive(Clone, Debug, PartialEq)]
pub struct HermesUsageReport {
    pub summary: UsageSummary,
    pub contributions: Vec<HermesUsageContribution>,
}

/// Reconciles one logical conversation's segments. Call `observe_segment` once per
/// compression member (root→tip) and then `finish`.
pub struct HermesUsageReconciler<'a> {
    meta: SessionMetadata,
    prices: &'a PriceSchedule,
    contributions: Vec<HermesUsageContribution>,
    seen_aux: HashSet<HermesRowId>,
}

impl<'a> HermesUsageReconciler<'a> {
    pub fn new(meta: SessionMetadata, prices: &'a PriceSchedule) -> Self {
        Self {
            meta,
            prices,
            contributions: Vec::new(),
            seen_aux: HashSet::new(),
        }
    }

    /// Selects exactly one main source for the segment, then adds each separately
    /// evidenced auxiliary row once. Session-scope counters never enter a request
    /// ledger or speed accumulator.
    pub fn observe_segment(&mut self, session: &HermesSessionTotals, rows: &[HermesModelTotals]) {
        let main_rows: Vec<&HermesModelTotals> = rows
            .iter()
            .filter(|row| included_in_session_totals(&row.task))
            .collect();
        let counters = rows_cover_counters(session, &main_rows);
        let complete = counters && money_covers(session, &main_rows);
        if complete {
            for row in &main_rows {
                self.push(row_contribution(
                    HermesUsageScope::MainRows,
                    &row.key,
                    &row.model,
                    &row.task,
                    row.api_call_count,
                    row.tokens,
                    row.money,
                    self.prices,
                ));
            }
        } else if session_has_usage(session) {
            let models: BTreeSet<(Option<String>, String)> = main_rows
                .iter()
                .filter_map(|row| {
                    row.model
                        .as_ref()
                        .map(|model| (model.provider.clone(), model.id.clone()))
                })
                .collect();
            let all_modelled = main_rows.iter().all(|row| row.model.is_some());
            let (model, proven) = if counters && all_modelled && models.len() == 1 {
                (main_rows.iter().find_map(|row| row.model.clone()), true)
            } else {
                // Zero/unknown main models leave the aggregate model unproven;
                // several main routes prove the lifetime tokens are mixed.
                (None, false)
            };
            self.push(aggregate_contribution(session, self.prices, model, proven));
        } else if !main_rows.is_empty() {
            // No session aggregate exists (legacy/partial row): the per-model
            // ledger is the only available main evidence.
            for row in &main_rows {
                self.push(row_contribution(
                    HermesUsageScope::MainRows,
                    &row.key,
                    &row.model,
                    &row.task,
                    row.api_call_count,
                    row.tokens,
                    row.money,
                    self.prices,
                ));
            }
        }
        for row in rows
            .iter()
            .filter(|row| !included_in_session_totals(&row.task))
        {
            if !self.seen_aux.insert(row.row) {
                continue;
            }
            self.push(row_contribution(
                HermesUsageScope::Auxiliary,
                &row.key,
                &row.model,
                &row.task,
                row.api_call_count,
                row.tokens,
                row.money,
                self.prices,
            ));
        }
    }

    fn push(&mut self, contribution: HermesUsageContribution) {
        if contribution_is_empty(&contribution) {
            return;
        }
        self.contributions.push(contribution);
    }

    pub fn contributions(&self) -> &[HermesUsageContribution] {
        &self.contributions
    }

    pub fn has_contributions(&self) -> bool {
        !self.contributions.is_empty()
    }

    pub fn finish(self) -> UsageSummary {
        self.finish_report().summary
    }

    pub fn finish_report(self) -> HermesUsageReport {
        let summary = summarize(&self.meta, &self.contributions);
        HermesUsageReport {
            summary,
            contributions: self.contributions,
        }
    }
}

fn included_in_session_totals(task: &str) -> bool {
    // `voice_chat` is the producer's documented per-turn route that increments the
    // sessions row; every other non-empty task is written only by
    // `record_auxiliary_usage` and is therefore a separate addend.
    task.is_empty() || task == "voice_chat"
}

fn session_has_usage(session: &HermesSessionTotals) -> bool {
    session.api_call_count.is_some_and(|calls| calls > 0)
        || [
            session.tokens.input,
            session.tokens.output,
            session.tokens.cache_read,
            session.tokens.cache_write,
            session.tokens.reasoning,
        ]
        .into_iter()
        .flatten()
        .any(|tokens| tokens > 0)
        || session.money.actual_usd.is_some_and(|usd| usd > 0.0)
        || session.money.estimated_usd.is_some_and(|usd| usd > 0.0)
}

/// A complete main ledger must cover the session aggregate's api calls and every
/// recorded core token leg. A missing session counter cannot make an incomplete
/// ledger "complete".
fn rows_cover_counters(session: &HermesSessionTotals, rows: &[&HermesModelTotals]) -> bool {
    if rows.is_empty() {
        return false;
    }
    let Some(session_calls) = session.api_call_count.filter(|calls| *calls > 0) else {
        return false;
    };
    let mut calls = Some(0u64);
    for row in rows {
        calls = sum_option(calls, row.api_call_count);
    }
    if calls != Some(session_calls) {
        return false;
    }
    leg_covers(session.tokens.input, rows, |row| row.tokens.input)
        && leg_covers(session.tokens.output, rows, |row| row.tokens.output)
        && leg_covers(session.tokens.cache_read, rows, |row| row.tokens.cache_read)
        && leg_covers(session.tokens.cache_write, rows, |row| {
            row.tokens.cache_write
        })
        && leg_covers(session.tokens.reasoning, rows, |row| row.tokens.reasoning)
}

/// A per-model ledger that matches calls/tokens but records different preserved
/// money is not a complete duplicate of the aggregate.
fn money_covers(session: &HermesSessionTotals, rows: &[&HermesModelTotals]) -> bool {
    let status = session.money.status;
    if status == HermesCostStatus::Unknown {
        return true;
    }
    if rows.iter().any(|row| row.money.status != status) {
        return false;
    }
    let sum = |value: fn(&HermesMoney) -> Option<f64>| -> Option<f64> {
        rows.iter()
            .try_fold(0.0, |acc, row| value(&row.money).map(|usd| acc + usd))
    };
    match status {
        HermesCostStatus::Actual => {
            let expected = session.money.actual_usd;
            match (expected, sum(|money| money.actual_usd)) {
                (Some(a), Some(b)) => (a - b).abs() < 1e-9,
                (None, None) => true,
                _ => false,
            }
        }
        HermesCostStatus::Estimated => {
            let expected = session.money.estimated_usd;
            match (expected, sum(|money| money.estimated_usd)) {
                (Some(a), Some(b)) => (a - b).abs() < 1e-9,
                (None, None) => true,
                _ => false,
            }
        }
        HermesCostStatus::Included => true,
        HermesCostStatus::Unknown => true,
    }
}

fn leg_covers(
    session_leg: Option<u64>,
    rows: &[&HermesModelTotals],
    get: impl Fn(&&HermesModelTotals) -> Option<u64>,
) -> bool {
    let Some(expected) = session_leg else {
        return true;
    };
    let mut sum = Some(0u64);
    for row in rows {
        sum = sum_option(sum, get(row));
    }
    sum == Some(expected)
}

fn sum_option(acc: Option<u64>, value: Option<u64>) -> Option<u64> {
    acc?.checked_add(value?)
}

fn contribution_is_empty(contribution: &HermesUsageContribution) -> bool {
    contribution.api_call_count.is_none_or(|calls| calls == 0)
        && [
            contribution.categories.input,
            contribution.categories.output,
            contribution.categories.cache_read,
            contribution.categories.cache_write,
            contribution.categories.reasoning,
        ]
        .into_iter()
        .flatten()
        .all(|tokens| tokens == 0)
        && contribution.cost.known_subtotal == 0.0
        && contribution.cost.usd.is_none()
}

#[allow(clippy::too_many_arguments)]
fn row_contribution(
    scope: HermesUsageScope,
    key: &SessionKey,
    model: &Option<ModelDetail>,
    task: &str,
    calls: Option<u64>,
    tokens: HermesTokens,
    money: HermesMoney,
    prices: &PriceSchedule,
) -> HermesUsageContribution {
    priced_contribution(scope, key, model, task, calls, tokens, money, prices, true)
}

fn aggregate_contribution(
    session: &HermesSessionTotals,
    prices: &PriceSchedule,
    model: Option<ModelDetail>,
    proven: bool,
) -> HermesUsageContribution {
    priced_contribution(
        HermesUsageScope::MainAggregate,
        &session.key,
        &model,
        "",
        session.api_call_count,
        session.tokens,
        session.money,
        prices,
        proven,
    )
}

#[allow(clippy::too_many_arguments)]
fn priced_contribution(
    scope: HermesUsageScope,
    key: &SessionKey,
    model: &Option<ModelDetail>,
    task: &str,
    calls: Option<u64>,
    tokens: HermesTokens,
    money: HermesMoney,
    prices: &PriceSchedule,
    attribution_proven: bool,
) -> HermesUsageContribution {
    let categories = tokens.categories();
    let recorded = money.recorded();
    let reported_usd = match money.status {
        HermesCostStatus::Actual => money.actual_usd,
        HermesCostStatus::Included => Some(0.0),
        HermesCostStatus::Estimated | HermesCostStatus::Unknown => None,
    };
    let saved_estimate_usd = match money.status {
        HermesCostStatus::Estimated => money.estimated_usd,
        HermesCostStatus::Actual | HermesCostStatus::Included | HermesCostStatus::Unknown => None,
    };
    let mut cost = if money.status == HermesCostStatus::Actual && money.actual_usd.is_some() {
        // `actual` authorizes the provider-reported amount even when it is zero.
        let usd = money.actual_usd.unwrap_or(0.0);
        CostEstimate {
            usd: Some(usd),
            class: AggregationClass::Reported,
            known_subtotal: usd,
            source: Some(MoneySource::ReportedApi),
            ..CostEstimate::default()
        }
    } else {
        estimate_facts(
            PricedFacts {
                model: model.as_ref(),
                scope: crate::domain::UsageScope::Session,
                categories: &categories,
                accounting: HermesTokens::accounting(),
                request: None,
                money: &recorded,
                harness: crate::domain::Harness::Hermes,
            },
            prices,
            ReferencePolicy::Disabled,
        )
    };
    if !attribution_proven
        && !matches!(
            cost.source,
            Some(MoneySource::ReportedApi | MoneySource::SavedCatalogEstimate)
        )
    {
        // The aggregate spans routes we cannot attribute. Never price its lifetime
        // tokens with a single latest/session model.
        cost = CostEstimate {
            class: AggregationClass::Unknown,
            note: Some("aggregate model attribution is unproven; no model rate was applied".into()),
            ..CostEstimate::default()
        };
    }
    if matches!(
        money.status,
        HermesCostStatus::Actual | HermesCostStatus::Estimated
    ) && reported_usd.is_none()
        && saved_estimate_usd.is_none()
        && cost.note.is_none()
    {
        cost.note =
            Some("cost_status has no usable recorded amount; API-equivalent estimate".into());
    }
    qualify_request_dependent(
        &mut cost,
        model,
        &recorded,
        money.status,
        money.actual_usd.is_some(),
        prices,
    );
    HermesUsageContribution {
        scope,
        key: key.clone(),
        model: model.clone(),
        task: task.to_owned(),
        api_call_count: calls,
        categories,
        status: money.status,
        reported_usd,
        saved_estimate_usd,
        cost,
    }
}

/// Aggregate counters carry no per-request tier, tariff window or service/region
/// choice. When a card needs one, the base-rate subtotal may still be useful but
/// the complete amount is not provable. Preserved actual/saved money is untouched.
fn qualify_request_dependent(
    cost: &mut CostEstimate,
    model: &Option<ModelDetail>,
    recorded: &RecordedMoney,
    status: HermesCostStatus,
    actual_authorized: bool,
    prices: &PriceSchedule,
) {
    if status == HermesCostStatus::Actual && actual_authorized {
        return;
    }
    if cost.source == Some(MoneySource::SavedCatalogEstimate) && recorded.saved_estimate.is_some() {
        return;
    }
    let Some(model) = model else {
        return;
    };
    let Some(provider) = model.provider.as_deref() else {
        return;
    };
    let Some(cards) = prices.cards(provider, &model.id) else {
        return;
    };
    let dependent = cards.iter().find_map(|card| {
        if card.threshold.is_some() {
            Some("request-length")
        } else if !card.tariff.is_empty() {
            Some("time-of-day")
        } else if !card.service_multipliers.is_empty() || !card.service_unpublished.is_empty() {
            Some("service-tier")
        } else if !card.region_multipliers.is_empty() {
            Some("inference-region")
        } else {
            None
        }
    });
    let Some(what) = dependent else {
        return;
    };
    // Preserved monetary evidence wins even when request details are absent.
    if recorded.reported_api.as_ref().is_some_and(|m| m.usd > 0.0)
        || recorded.saved_estimate.is_some()
    {
        return;
    }
    cost.usd = None;
    cost.class = AggregationClass::Partial;
    cost.note = Some(format!(
        "{what} pricing needs per-request evidence; the aggregate is priced only partially"
    ));
}

struct Group {
    model: ModelDetail,
    tokens: LegSums,
    cost: CostSum,
    calls: CallSum,
}

struct LegSums {
    input: Option<u64>,
    output: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    reasoning: Option<u64>,
    unknown: bool,
    seen: bool,
}

impl LegSums {
    fn new() -> Self {
        Self {
            input: None,
            output: None,
            cache_read: None,
            cache_write: None,
            reasoning: None,
            unknown: false,
            seen: false,
        }
    }

    fn add(&mut self, categories: &TokenCategories) {
        if !self.seen {
            self.seen = true;
            self.input = categories.input;
            self.output = categories.output;
            self.cache_read = categories.cache_read;
            self.cache_write = categories.cache_write;
            self.reasoning = categories.reasoning;
            self.unknown = [
                categories.input,
                categories.output,
                categories.cache_read,
                categories.cache_write,
            ]
            .into_iter()
            .any(|leg| leg.is_none());
            return;
        }
        self.input = merge_leg(self.input, categories.input, &mut self.unknown);
        self.output = merge_leg(self.output, categories.output, &mut self.unknown);
        self.cache_read = merge_leg(self.cache_read, categories.cache_read, &mut self.unknown);
        self.cache_write = merge_leg(self.cache_write, categories.cache_write, &mut self.unknown);
        self.reasoning = merge_leg(self.reasoning, categories.reasoning, &mut self.unknown);
    }

    fn categories(&self) -> TokenCategories {
        TokenCategories {
            input: self.input,
            cached_input: None,
            cache_write: self.cache_write,
            cache_write_5m: None,
            cache_write_1h: None,
            cache_read: self.cache_read,
            output: self.output,
            reasoning: self.reasoning,
            total: None,
            orchestration_input: None,
            orchestration_output: None,
            orchestration_cache_read: None,
        }
    }

    fn total(&self) -> Option<u64> {
        if self.unknown {
            return None;
        }
        [
            self.input?,
            self.cache_read?,
            self.cache_write?,
            self.output?,
        ]
        .into_iter()
        .try_fold(0u64, |acc, value| acc.checked_add(value))
    }
}

fn merge_leg(acc: Option<u64>, value: Option<u64>, unknown: &mut bool) -> Option<u64> {
    match (acc, value) {
        (Some(a), Some(b)) => a.checked_add(b),
        _ => {
            *unknown = true;
            None
        }
    }
}

struct CallSum {
    known: u64,
    missing: bool,
}

impl CallSum {
    fn new() -> Self {
        Self {
            known: 0,
            missing: false,
        }
    }

    fn add(&mut self, calls: Option<u64>) {
        match calls {
            Some(calls) => self.known = self.known.saturating_add(calls),
            None => self.missing = true,
        }
    }

    fn observed(&self) -> usize {
        usize::try_from(self.known).unwrap_or(usize::MAX)
    }
}

struct CostSum {
    usd: Option<f64>,
    known_subtotal: f64,
    breakdown: CostBreakdown,
    breakdown_known: [bool; 7],
    as_of: Option<String>,
    source: Option<MoneySource>,
    source_seen: bool,
    same_source: bool,
    reported: Option<f64>,
    reported_all: bool,
    all_actual: bool,
    saved: Option<f64>,
    saved_all: bool,
    partial_note: Option<String>,
    any: bool,
    any_unknown: bool,
}

impl CostSum {
    fn new() -> Self {
        Self {
            usd: None,
            known_subtotal: 0.0,
            breakdown: CostBreakdown::default(),
            breakdown_known: [true; 7],
            as_of: None,
            source: None,
            source_seen: false,
            same_source: true,
            reported: None,
            reported_all: true,
            all_actual: true,
            saved: None,
            saved_all: true,
            partial_note: None,
            any: false,
            any_unknown: false,
        }
    }

    fn add(&mut self, cost: &CostEstimate, contribution: &HermesUsageContribution) {
        self.any = true;
        self.known_subtotal += cost.known_subtotal;
        match (self.usd, cost.usd) {
            (None, Some(usd)) if !self.any_unknown => self.usd = Some(usd),
            (Some(a), Some(b)) => self.usd = Some(a + b),
            _ => {
                self.usd = None;
                self.any_unknown = true;
            }
        }
        if let Some(note) = &cost.note
            && self.partial_note.is_none()
        {
            self.partial_note = Some(note.clone());
        }
        if cost.usd.is_none() && cost.class == AggregationClass::Unknown {
            self.any_unknown = true;
        }
        add_breakdown(
            &mut self.breakdown.input,
            &mut self.breakdown_known[0],
            cost.breakdown.input,
        );
        add_breakdown(
            &mut self.breakdown.cache_read,
            &mut self.breakdown_known[1],
            cost.breakdown.cache_read,
        );
        add_breakdown(
            &mut self.breakdown.cache_write_5m,
            &mut self.breakdown_known[2],
            cost.breakdown.cache_write_5m,
        );
        add_breakdown(
            &mut self.breakdown.cache_write_1h,
            &mut self.breakdown_known[3],
            cost.breakdown.cache_write_1h,
        );
        add_breakdown(
            &mut self.breakdown.cache_write_other,
            &mut self.breakdown_known[4],
            cost.breakdown.cache_write_other,
        );
        add_breakdown(
            &mut self.breakdown.output,
            &mut self.breakdown_known[5],
            cost.breakdown.output,
        );
        add_breakdown(
            &mut self.breakdown.reasoning,
            &mut self.breakdown_known[6],
            cost.breakdown.reasoning,
        );
        if let Some(as_of) = &cost.as_of
            && self.as_of.as_ref().is_none_or(|old| old < as_of)
        {
            self.as_of = Some(as_of.clone());
        }
        if !self.source_seen {
            self.source_seen = true;
            self.source = cost.source;
        } else if self.source != cost.source {
            self.same_source = false;
        }
        if contribution.status != HermesCostStatus::Actual || contribution.reported_usd.is_none() {
            self.all_actual = false;
        }
        match contribution.reported_usd {
            Some(reported) => self.reported = Some(self.reported.unwrap_or(0.0) + reported),
            None => self.reported_all = false,
        }
        match contribution.saved_estimate_usd {
            Some(saved) => self.saved = Some(self.saved.unwrap_or(0.0) + saved),
            None => self.saved_all = false,
        }
    }

    fn estimate(&self) -> CostEstimate {
        let class = if !self.any {
            AggregationClass::Unknown
        } else if self.usd.is_some() {
            if self.all_actual {
                AggregationClass::Reported
            } else {
                AggregationClass::Estimated
            }
        } else {
            AggregationClass::Partial
        };
        CostEstimate {
            usd: self.usd,
            class,
            as_of: self.as_of.clone(),
            note: self.partial_note.clone(),
            known_subtotal: self.known_subtotal,
            breakdown: CostBreakdown {
                input: self.breakdown.input,
                cache_read: self.breakdown.cache_read,
                cache_write_5m: self.breakdown.cache_write_5m,
                cache_write_1h: self.breakdown.cache_write_1h,
                cache_write_other: self.breakdown.cache_write_other,
                output: self.breakdown.output,
                reasoning: self.breakdown.reasoning,
            },
            source: if self.same_source { self.source } else { None },
        }
    }
}

fn add_breakdown(acc: &mut Option<f64>, known: &mut bool, value: Option<f64>) {
    if !*known {
        return;
    }
    match (*acc, value) {
        (Some(a), Some(b)) => *acc = Some(a + b),
        (None, Some(b)) => *acc = Some(b),
        _ => {
            *acc = None;
            *known = false;
        }
    }
}

fn summarize(_meta: &SessionMetadata, contributions: &[HermesUsageContribution]) -> UsageSummary {
    if contributions.is_empty() {
        return UsageSummary {
            class: AggregationClass::Unknown,
            cost: CostEstimate {
                class: AggregationClass::Unknown,
                note: Some("no usage rows are recorded for this conversation".into()),
                ..CostEstimate::default()
            },
            coverage: UsageCoverage {
                complete: false,
                reason: Some("no usage rows are recorded for this conversation".into()),
                ..UsageCoverage::default()
            },
            ..UsageSummary::default()
        };
    }

    let mut total = LegSums::new();
    let mut calls = CallSum::new();
    let mut cost = CostSum::new();
    let mut groups: BTreeMap<(Option<String>, String), Group> = BTreeMap::new();
    for contribution in contributions {
        total.add(&contribution.categories);
        calls.add(contribution.api_call_count);
        cost.add(&contribution.cost, contribution);
        if let Some(model) = &contribution.model {
            let key = (model.provider.clone(), model.id.clone());
            let group = groups.entry(key).or_insert_with(|| Group {
                model: ModelDetail {
                    id: model.id.clone(),
                    provider: model.provider.clone(),
                    variant: None,
                    effort: None,
                    configured_effort: None,
                },
                tokens: LegSums::new(),
                cost: CostSum::new(),
                calls: CallSum::new(),
            });
            group.tokens.add(&contribution.categories);
            group.cost.add(&contribution.cost, contribution);
            group.calls.add(contribution.api_call_count);
        }
    }

    let models: Vec<ModelUsageSummary> = groups
        .into_values()
        .map(|group| ModelUsageSummary {
            model: group.model,
            categories: group.tokens.categories(),
            total: group.tokens.total(),
            cost: group.cost.estimate(),
            coverage: coverage(&group.calls, group.tokens.unknown, group.cost.usd.is_some()),
        })
        .collect();

    let mut coverage = coverage(&calls, total.unknown, cost.usd.is_some());
    if contributions.iter().any(|contribution| {
        contribution.scope == HermesUsageScope::MainAggregate && contribution.model.is_none()
    }) {
        coverage.complete = false;
        let reason = "aggregate model attribution is unproven";
        coverage.reason = Some(match coverage.reason.take() {
            Some(existing) => format!("{existing}; {reason}"),
            None => reason.to_owned(),
        });
    }
    let mut estimate = cost.estimate();
    if contributions
        .iter()
        .any(|contribution| contribution.scope == HermesUsageScope::MainAggregate)
        && estimate.note.is_none()
    {
        estimate.note = Some(
            "main usage is a session aggregate because the per-model main ledger is incomplete"
                .into(),
        );
    }
    let complete = coverage.complete;
    UsageSummary {
        categories: total.categories(),
        total: total.total(),
        class: if complete {
            if estimate.class == AggregationClass::Reported {
                AggregationClass::Reported
            } else {
                AggregationClass::Estimated
            }
        } else {
            AggregationClass::Partial
        },
        cost: estimate,
        reported_cost_usd: (cost.reported_all && cost.reported.is_some())
            .then_some(cost.reported.unwrap_or(0.0)),
        saved_estimate_usd: (cost.saved_all && cost.saved.is_some())
            .then_some(cost.saved.unwrap_or(0.0)),
        coverage,
        models,
    }
}

fn coverage(calls: &CallSum, tokens_unknown: bool, priced: bool) -> UsageCoverage {
    let mut reason = Vec::new();
    if calls.missing {
        reason.push("api_call_count is unknown for some rows".to_owned());
    }
    if tokens_unknown {
        reason.push("a core token counter is not reported".to_owned());
    }
    if !priced {
        reason.push("some usage has no complete price".to_owned());
    }
    UsageCoverage {
        observed_requests: calls.observed(),
        priced_requests: if priced && !calls.missing {
            calls.observed()
        } else {
            0
        },
        complete: reason.is_empty(),
        reason: (!reason.is_empty()).then(|| reason.join("; ")),
    }
}
