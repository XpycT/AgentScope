//! Token normalization, per-request accounting, and session totals (spec §8).
//! Missing data is `Unknown`, never zero.

use std::collections::{HashMap, HashSet};

use thiserror::Error;

use crate::domain::{
    AggregationClass, CostBreakdown, CostEstimate, Harness, InputAccounting, LocatorKind,
    ModelDetail, ModelUsageSummary, MoneyFact, MoneySource, OutputAccounting, RecordedMoney,
    RequestMetadata, ServerToolCounts, TokenAccounting, TokenCategories, UsageCoverage,
    UsageObservation, UsageScope, UsageSummary,
};
use crate::pricing::{
    PriceSchedule, PricedFacts, ReferencePolicy, Selection, estimate_facts, estimate_selected,
};

// ---------------------------------------------------------------------------
// Normalization
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum UsageError {
    #[error("token accounting semantics are unknown")]
    UnknownAccounting,
    #[error("a token subset exceeds the counter that must contain it")]
    InvalidSubset,
    #[error("token counters overflow")]
    Overflow,
}

/// A canonical token leg: absent by construction, present but unreported, or counted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Leg {
    NotApplicable,
    Unknown,
    Tokens(u64),
}

impl Leg {
    fn value(self) -> Option<u64> {
        match self {
            Leg::Tokens(n) => Some(n),
            _ => None,
        }
    }

    fn of(v: Option<u64>) -> Leg {
        v.map_or(Leg::Unknown, Leg::Tokens)
    }

    fn label(v: Option<u64>) -> Leg {
        v.map_or(Leg::NotApplicable, Leg::Tokens)
    }

    fn plus(self, extra: Option<u64>) -> Result<Leg, UsageError> {
        match (self, extra) {
            (Leg::Tokens(a), Some(b)) => a
                .checked_add(b)
                .map(Leg::Tokens)
                .ok_or(UsageError::Overflow),
            (leg, _) => Ok(leg),
        }
    }
}

/// Canonical billable legs of one observation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Legs {
    /// Uncached input.
    pub input: Leg,
    pub cache_read: Leg,
    pub write_5m: Leg,
    pub write_1h: Leg,
    pub write_other: Leg,
    /// Output billed at the output rate; excludes separately reported reasoning.
    pub output: Leg,
    /// Separately reported reasoning; `NotApplicable` when output already contains it.
    pub reasoning: Leg,
    /// Reasoning subset label, whichever way the source counts it.
    pub reasoning_label: Leg,
}

impl Legs {
    fn write_total(&self) -> Leg {
        let parts = [self.write_5m, self.write_1h, self.write_other];
        let mut total = 0u64;
        let mut any = false;
        for part in parts {
            match part {
                Leg::NotApplicable => {}
                Leg::Unknown => return Leg::Unknown,
                Leg::Tokens(n) => match total.checked_add(n) {
                    Some(t) => {
                        total = t;
                        any = true;
                    }
                    None => return Leg::Unknown,
                },
            }
        }
        if any {
            Leg::Tokens(total)
        } else {
            Leg::NotApplicable
        }
    }

    /// Output with separately reported reasoning folded in.
    fn folded_output(&self) -> Leg {
        match (self.output, self.reasoning) {
            (Leg::Tokens(o), Leg::Tokens(r)) => o.checked_add(r).map_or(Leg::Unknown, Leg::Tokens),
            (_, Leg::Unknown) | (Leg::Unknown, _) => Leg::Unknown,
            (o, _) => o,
        }
    }
}

/// Interprets raw source counters under the adapter-proven accounting.
pub(crate) fn legs(c: &TokenCategories, a: TokenAccounting) -> Result<Legs, UsageError> {
    let nonzero = |v: Option<u64>| v.is_some_and(|x| x > 0);
    let cache_used = [
        c.cached_input,
        c.cache_read,
        c.cache_write,
        c.cache_write_5m,
        c.cache_write_1h,
    ]
    .into_iter()
    .any(nonzero);
    if a.input == InputAccounting::Unknown && cache_used {
        return Err(UsageError::UnknownAccounting);
    }
    if a.output == OutputAccounting::Unknown && nonzero(c.reasoning) {
        return Err(UsageError::UnknownAccounting);
    }

    let read = match (c.cache_read, c.cached_input) {
        (Some(a), Some(b)) if a != b => return Err(UsageError::InvalidSubset),
        (a, b) => a.or(b),
    };
    let (write_5m, write_1h, write_other) = match (c.cache_write_5m, c.cache_write_1h) {
        (None, None) => (
            Leg::NotApplicable,
            Leg::NotApplicable,
            Leg::of(c.cache_write),
        ),
        (five, one) => {
            // A producer may record only one retention split (Pi records the
            // one-hour subset; Claude may omit an absent split). The known leg is
            // priced at its own rate and the residual at the generic write rate;
            // a missing total leaves the residual unknown rather than zero.
            let known = five
                .unwrap_or(0)
                .checked_add(one.unwrap_or(0))
                .ok_or(UsageError::Overflow)?;
            let other = match c.cache_write {
                Some(total) => {
                    Leg::Tokens(total.checked_sub(known).ok_or(UsageError::InvalidSubset)?)
                }
                None => Leg::Unknown,
            };
            (
                Leg::Tokens(five.unwrap_or(0)),
                Leg::Tokens(one.unwrap_or(0)),
                other,
            )
        }
    };
    let mut out = Legs {
        input: Leg::Unknown,
        cache_read: Leg::of(read),
        write_5m,
        write_1h,
        write_other,
        output: Leg::of(c.output),
        reasoning: Leg::NotApplicable,
        reasoning_label: Leg::label(c.reasoning),
    };
    out.input = match a.input {
        InputAccounting::InclusiveCaches => match (c.input, read, out.write_total()) {
            (Some(input), Some(read), Leg::Tokens(write)) => {
                let cached = read.checked_add(write).ok_or(UsageError::Overflow)?;
                Leg::Tokens(input.checked_sub(cached).ok_or(UsageError::InvalidSubset)?)
            }
            (Some(input), Some(read), Leg::NotApplicable) => {
                Leg::Tokens(input.checked_sub(read).ok_or(UsageError::InvalidSubset)?)
            }
            _ => Leg::Unknown,
        },
        InputAccounting::AdditiveCaches | InputAccounting::Unknown => Leg::of(c.input),
    };
    if let (Some(output), Some(reasoning)) = (c.output, c.reasoning)
        && a.output != OutputAccounting::SeparateReasoning
        && reasoning > output
    {
        return Err(UsageError::InvalidSubset);
    }
    if a.output == OutputAccounting::SeparateReasoning {
        out.reasoning = Leg::of(c.reasoning);
    }
    out.input = out.input.plus(c.orchestration_input)?;
    out.cache_read = out.cache_read.plus(c.orchestration_cache_read)?;
    out.output = out.output.plus(c.orchestration_output)?;
    Ok(out)
}

/// Canonical uncached-input/read/write/output legs of a raw observation.
/// Reasoning and orchestration counters remain as subset labels; the output
/// leg already contains separately reported reasoning. Unknown legs stay `None`.
pub fn normalized_categories(usage: &UsageObservation) -> Result<TokenCategories, UsageError> {
    let l = legs(&usage.categories, usage.accounting)?;
    Ok(canonical(&l, &usage.categories))
}

fn canonical(l: &Legs, raw: &TokenCategories) -> TokenCategories {
    TokenCategories {
        input: l.input.value(),
        cached_input: None,
        cache_write: l.write_total().value(),
        cache_write_5m: l.write_5m.value(),
        cache_write_1h: l.write_1h.value(),
        cache_read: l.cache_read.value(),
        output: l.folded_output().value(),
        reasoning: l.reasoning_label.value(),
        total: raw.total,
        orchestration_input: raw.orchestration_input,
        orchestration_output: raw.orchestration_output,
        orchestration_cache_read: raw.orchestration_cache_read,
    }
}

// ---------------------------------------------------------------------------
// Request ledger
// ---------------------------------------------------------------------------

/// One update to a request already or newly observed.
#[derive(Clone, Copy, Debug)]
pub enum UsageUpdate<'a> {
    /// A first contribution for an identity; an identical replay is a duplicate.
    Initial(&'a UsageObservation),
    /// A later revision of the same request that the producer proves supersedes it.
    AuthoritativeReplacement(&'a UsageObservation),
    /// Evidence of a reused identity that could not be resolved; never counted.
    Conflict(&'a UsageObservation),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedgerOutcome {
    Counted,
    Revised,
    Duplicate,
    Uncountable,
}

const TOKEN_LEGS: usize = 10;
const INPUT: usize = 0;
const READ: usize = 1;
const W5: usize = 2;
const W1H: usize = 3;
const WRITE: usize = 4;
const OUTPUT: usize = 5;
const REASONING: usize = 6;
const ORCH_IN: usize = 7;
const ORCH_OUT: usize = 8;
const ORCH_READ: usize = 9;

#[derive(Clone, Default)]
struct Tally {
    sum: u128,
    unknown: usize,
    applicable: usize,
}

impl Tally {
    fn apply(&mut self, leg: Leg, add: bool) {
        let step = |v: &mut usize| {
            if add {
                *v += 1;
            } else {
                *v = v.saturating_sub(1);
            }
        };
        match leg {
            Leg::NotApplicable => {}
            Leg::Unknown => {
                step(&mut self.unknown);
                step(&mut self.applicable);
            }
            Leg::Tokens(n) => {
                self.sum = if add {
                    self.sum + u128::from(n)
                } else {
                    self.sum.saturating_sub(u128::from(n))
                };
                step(&mut self.applicable);
            }
        }
    }

    /// Known only when every counted request reported the leg.
    fn value(&self, requests: usize) -> Option<u64> {
        if requests == 0 || self.unknown != 0 || self.applicable != requests {
            return None;
        }
        u64::try_from(self.sum).ok()
    }
}

#[derive(Clone, Default)]
struct MoneyTally {
    sum: f64,
    unknown: usize,
    applicable: usize,
}

impl MoneyTally {
    fn apply(&mut self, v: Option<f64>, applicable: bool, add: bool) {
        if !applicable {
            return;
        }
        let step = |n: &mut usize| {
            if add {
                *n += 1;
            } else {
                *n = n.saturating_sub(1);
            }
        };
        step(&mut self.applicable);
        match v {
            Some(v) => self.sum += if add { v } else { -v },
            None => step(&mut self.unknown),
        }
    }

    fn value(&self, requests: usize) -> Option<f64> {
        (requests > 0 && self.unknown == 0 && self.applicable == requests)
            .then_some(self.sum.max(0.0))
    }
}

/// What one request adds to every aggregate.
#[derive(Clone)]
struct Contribution {
    tokens: [Leg; TOKEN_LEGS],
    usd: Option<f64>,
    subtotal: f64,
    breakdown: [Option<f64>; 7],
    reported_class: bool,
    source: Option<MoneySource>,
    reported: Option<f64>,
    saved: Option<f64>,
    reference: bool,
}

#[derive(Clone, Default)]
struct Agg {
    requests: usize,
    priced: usize,
    reported_class: usize,
    reference: usize,
    sources: [usize; 4],
    tokens: [Tally; TOKEN_LEGS],
    usd: f64,
    subtotal: f64,
    breakdown: [MoneyTally; 7],
    reported: MoneyTally,
    saved: MoneyTally,
    as_of: Option<String>,
    /// Why the most recently added unpriced request had no complete price.
    unpriced_note: Option<String>,
}

fn source_slot(s: MoneySource) -> usize {
    match s {
        MoneySource::ReportedApi => 0,
        MoneySource::SavedCatalogEstimate => 1,
        MoneySource::UserOverride => 2,
        MoneySource::BundledReference => 3,
    }
}

const SOURCES: [MoneySource; 4] = [
    MoneySource::ReportedApi,
    MoneySource::SavedCatalogEstimate,
    MoneySource::UserOverride,
    MoneySource::BundledReference,
];

impl Agg {
    fn apply(&mut self, c: &Contribution, add: bool) {
        let bump = |n: &mut usize| {
            if add {
                *n += 1;
            } else {
                *n = n.saturating_sub(1);
            }
        };
        bump(&mut self.requests);
        for (tally, leg) in self.tokens.iter_mut().zip(c.tokens) {
            tally.apply(leg, add);
        }
        if let Some(usd) = c.usd {
            bump(&mut self.priced);
            self.usd += if add { usd } else { -usd };
        }
        self.subtotal += if add { c.subtotal } else { -c.subtotal };
        if c.reported_class {
            bump(&mut self.reported_class);
        }
        if c.reference {
            bump(&mut self.reference);
        }
        if let Some(s) = c.source {
            bump(&mut self.sources[source_slot(s)]);
        }
        for (tally, v) in self.breakdown.iter_mut().zip(c.breakdown) {
            tally.apply(v, true, add);
        }
        self.reported.apply(c.reported, c.reported.is_some(), add);
        self.saved.apply(c.saved, c.saved.is_some(), add);
    }

    fn categories(&self) -> TokenCategories {
        let n = self.requests;
        let t = |i: usize| self.tokens[i].value(n);
        TokenCategories {
            input: t(INPUT),
            cache_read: t(READ),
            cache_write_5m: t(W5),
            cache_write_1h: t(W1H),
            cache_write: t(WRITE),
            output: t(OUTPUT),
            reasoning: t(REASONING),
            orchestration_input: t(ORCH_IN),
            orchestration_output: t(ORCH_OUT),
            orchestration_cache_read: t(ORCH_READ),
            ..TokenCategories::default()
        }
    }

    fn cost(&self) -> CostEstimate {
        let n = self.requests;
        if n == 0 {
            return CostEstimate {
                note: Some("no requests observed".into()),
                ..CostEstimate::default()
            };
        }
        let complete = self.priced == n;
        let source = SOURCES
            .into_iter()
            .find(|s| self.sources[source_slot(*s)] == n);
        let leg = |i: usize| {
            let t = &self.breakdown[i];
            (t.applicable > 0 && t.unknown == 0).then_some(t.sum.max(0.0))
        };
        CostEstimate {
            usd: complete.then_some(self.usd.max(0.0)),
            class: if !complete {
                AggregationClass::Unknown
            } else if self.reported_class == n {
                AggregationClass::Reported
            } else {
                AggregationClass::Estimated
            },
            as_of: self.as_of.clone(),
            note: (!complete).then(|| {
                let count = format!("{} of {n} requests have no complete price", n - self.priced);
                match &self.unpriced_note {
                    Some(why) => format!("{count}: {why}"),
                    None => count,
                }
            }),
            known_subtotal: self.subtotal.max(0.0),
            breakdown: CostBreakdown {
                input: leg(0),
                cache_read: leg(1),
                cache_write_5m: leg(2),
                cache_write_1h: leg(3),
                cache_write_other: leg(4),
                output: leg(5),
                reasoning: leg(6),
            },
            source,
            reference: self.reference > 0,
        }
    }

    fn coverage(&self, extra_reason: Option<&str>) -> UsageCoverage {
        let complete = extra_reason.is_none() && self.requests > 0 && self.priced == self.requests;
        let reason = extra_reason.map(str::to_owned).or_else(|| {
            (self.priced != self.requests).then(|| {
                format!(
                    "{} of {} requests have no complete price",
                    self.requests - self.priced,
                    self.requests
                )
            })
        });
        UsageCoverage {
            observed_requests: self.requests,
            priced_requests: self.priced,
            complete,
            reason,
        }
    }
}

struct PriceMeta {
    as_of: Option<String>,
    note: Option<String>,
}

/// Request facts retained so a revision re-prices in its original context.
struct Stored {
    scope: UsageScope,
    categories: TokenCategories,
    accounting: TokenAccounting,
    harness: Harness,
    /// Interned model identity.
    slot: Option<usize>,
    started_at: Option<i64>,
    completed_at: Option<i64>,
    /// Only non-standard values are kept; a standard tier prices as absent.
    service_tier: Option<Box<str>>,
    inference_region: Option<Box<str>>,
    /// The money the contribution was priced with.
    money: RecordedMoney,
    /// The card, tier/window and multipliers chosen when first priced.
    selection: Option<Selection>,
    /// Budget charge that does not depend on pricing: entry, key, kept strings.
    fixed_bytes: usize,
    /// Total bytes charged against the ledger budget for this entry.
    bytes: usize,
    contribution: Contribution,
}

/// Fixed allowance for the map slot and allocator bookkeeping of one entry.
const ENTRY_OVERHEAD: usize = 64;

fn fact_eq(a: &Option<MoneyFact>, b: &Option<MoneyFact>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a.usd.to_bits() == b.usd.to_bits() && a.source == b.source && a.as_of == b.as_of
        }
        _ => false,
    }
}

fn money_eq(a: &RecordedMoney, b: &RecordedMoney) -> bool {
    fact_eq(&a.reported_api, &b.reported_api)
        && fact_eq(&a.saved_estimate, &b.saved_estimate)
        && a.unverified_usd.map(f64::to_bits) == b.unverified_usd.map(f64::to_bits)
}

/// Pricing-dependent retained state of one entry: recorded money text and the
/// price context. The aggregates' as-of/reason strings are charged separately,
/// by what they actually retain (`meta_delta`).
fn dynamic_bytes(money: &RecordedMoney, selection: Option<&Selection>) -> usize {
    money_bytes(money) + selection.map_or(0, Selection::retained_bytes)
}

/// Where the per-model aggregate for a request lives.
#[derive(Clone, Copy)]
enum SlotKind {
    Unmodeled,
    Existing(usize),
    New,
}

/// Bytes of the as-of and reason strings an aggregate keeps after `meta` is
/// applied with the same rules as `note_meta`: a greater as-of replaces, a present
/// reason replaces, anything else is retained as is.
fn meta_bytes_after(agg: Option<&Agg>, meta: &PriceMeta) -> (usize, usize) {
    let len = |s: &Option<String>| s.as_ref().map_or(0, String::len);
    let (old_as, old_note) = agg.map_or((0, 0), |a| (len(&a.as_of), len(&a.unpriced_note)));
    let replaces =
        |old: Option<&Agg>, new: &str| old.and_then(|a| a.as_of.as_deref()).is_none_or(|o| o < new);
    let as_of = match &meta.as_of {
        Some(n) if replaces(agg, n) => n.len(),
        _ => old_as,
    };
    let note = meta.note.as_ref().map_or(old_note, String::len);
    (old_as + old_note, as_of + note)
}

fn money_bytes(m: &RecordedMoney) -> usize {
    [&m.reported_api, &m.saved_estimate]
        .into_iter()
        .flatten()
        .map(|f| f.as_of.as_ref().map_or(0, String::len))
        .sum()
}

struct ModelSlot {
    detail: ModelDetail,
    agg: Agg,
}

type ModelKey = (Option<String>, String);

/// Bounded, owned ledger of unique requests. Not a cumulative-counter adder:
/// session-scope aggregates and their replacement semantics stay with adapters.
pub struct UsageLedger {
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
    frozen: bool,
    conflicts: usize,
    marked: Option<String>,
    entries: HashMap<Identity, Stored>,
    slots: Vec<ModelSlot>,
    slot_index: HashMap<ModelKey, usize>,
    total: Agg,
    unmodeled: Agg,
}

/// `true` for a source-confirmed request id, `false` for a record locator.
pub(crate) type Identity = (bool, String);

/// The one request-identity resolver: a source-confirmed id, else the record locator.
/// Shared by the usage ledger and the speed accumulator.
pub(crate) fn identity(u: &UsageObservation) -> Option<Identity> {
    if let Some(id) = u
        .request
        .as_ref()
        .and_then(|r| r.id.as_deref())
        .filter(|id| !id.is_empty())
    {
        return Some((true, id.to_owned()));
    }
    match &u.provenance.locator.kind {
        LocatorKind::FileOffset { path, offset } => {
            Some((false, format!("f:{}:{offset}", path.to_string_lossy())))
        }
        LocatorKind::DbRow {
            table,
            row_id,
            part_index,
        } => Some((false, format!("d:{table}:{row_id}:{part_index:?}"))),
        LocatorKind::CompressedRecord { path, ordinal } => {
            Some((false, format!("z:{}:{ordinal}", path.to_string_lossy())))
        }
    }
}

fn usable_money(fact: &Option<MoneyFact>) -> Option<f64> {
    fact.as_ref()
        .map(|f| f.usd)
        .filter(|v| v.is_finite() && *v >= 0.0)
}

impl UsageLedger {
    pub fn new(max_entries: usize, max_bytes: usize) -> UsageLedger {
        UsageLedger {
            max_entries,
            max_bytes,
            bytes: 0,
            frozen: false,
            conflicts: 0,
            marked: None,
            entries: HashMap::new(),
            slots: Vec::new(),
            slot_index: HashMap::new(),
            total: Agg::default(),
            unmodeled: Agg::default(),
        }
    }

    /// True when a request with this confirmed provider/request id is counted.
    pub(crate) fn knows_request(&self, id: &str) -> bool {
        self.entries.contains_key(&(true, id.to_owned()))
    }

    pub fn observed_requests(&self) -> usize {
        self.entries.len()
    }

    /// Records that some usage could not be attributed to a request.
    pub fn mark_partial(&mut self, reason: &str) {
        self.marked.get_or_insert_with(|| reason.to_owned());
    }

    fn contribution(
        &self,
        facts: PricedFacts<'_>,
        prices: &PriceSchedule,
        policy: ReferencePolicy,
        retained: Option<&Selection>,
    ) -> (Contribution, PriceMeta, Option<Selection>) {
        let (cost, selection) = estimate_selected(facts, prices, policy, retained);
        let tokens = match legs(facts.categories, facts.accounting) {
            Ok(l) => {
                let raw = facts.categories;
                [
                    l.input,
                    l.cache_read,
                    l.write_5m,
                    l.write_1h,
                    l.write_total(),
                    l.folded_output(),
                    l.reasoning_label,
                    Leg::label(raw.orchestration_input),
                    Leg::label(raw.orchestration_output),
                    Leg::label(raw.orchestration_cache_read),
                ]
            }
            Err(_) => [Leg::Unknown; TOKEN_LEGS],
        };
        let b = &cost.breakdown;
        let contribution = Contribution {
            tokens,
            usd: cost.usd,
            subtotal: cost.known_subtotal,
            breakdown: [
                b.input,
                b.cache_read,
                b.cache_write_5m,
                b.cache_write_1h,
                b.cache_write_other,
                b.output,
                b.reasoning,
            ],
            reported_class: cost.class == AggregationClass::Reported,
            source: cost.source,
            reported: usable_money(&facts.money.reported_api),
            saved: usable_money(&facts.money.saved_estimate),
            reference: cost.reference,
        };
        let meta = PriceMeta {
            as_of: cost.as_of,
            note: cost.usd.is_none().then_some(cost.note).flatten(),
        };
        (contribution, meta, selection)
    }

    /// Whether `model` is the interned model `slot` (route and id decide identity).
    fn same_model(&self, slot: Option<usize>, model: &Option<ModelDetail>) -> bool {
        match (slot, model) {
            (None, None) => true,
            (Some(i), Some(m)) => {
                let d = &self.slots[i].detail;
                d.id == m.id && d.provider == m.provider
            }
            _ => false,
        }
    }

    fn slot_for(&mut self, model: &Option<ModelDetail>) -> Option<usize> {
        let m = model.as_ref()?;
        let key = (m.provider.clone(), m.id.clone());
        if let Some(i) = self.slot_index.get(&key) {
            return Some(*i);
        }
        let i = self.slots.len();
        self.slots.push(ModelSlot {
            detail: ModelDetail {
                id: m.id.clone(),
                provider: m.provider.clone(),
                variant: None,
                effort: None,
                configured_effort: None,
            },
            agg: Agg::default(),
        });
        self.slot_index.insert(key, i);
        Some(i)
    }

    fn apply(&mut self, slot: Option<usize>, c: &Contribution, add: bool) {
        self.total.apply(c, add);
        match slot {
            Some(i) => self.slots[i].agg.apply(c, add),
            None => self.unmodeled.apply(c, add),
        }
    }

    fn note_meta(&mut self, slot: Option<usize>, meta: PriceMeta) {
        let mut aggs = vec![&mut self.total];
        if let Some(i) = slot {
            aggs.push(&mut self.slots[i].agg);
        }
        for agg in aggs {
            if let Some(as_of) = &meta.as_of
                && agg.as_of.as_ref().is_none_or(|old| old < as_of)
            {
                agg.as_of = Some(as_of.clone());
            }
            if meta.note.is_some() {
                agg.unpriced_note = meta.note.clone();
            }
        }
    }

    pub fn observe(
        &mut self,
        update: UsageUpdate<'_>,
        prices: &PriceSchedule,
        policy: ReferencePolicy,
    ) -> LedgerOutcome {
        match update {
            UsageUpdate::Conflict(_) => {
                self.conflicts += 1;
                LedgerOutcome::Uncountable
            }
            UsageUpdate::Initial(u) | UsageUpdate::AuthoritativeReplacement(u)
                if u.scope == UsageScope::Session =>
            {
                // A session aggregate is not a request; it has no boundaries to count once.
                let _ = u;
                self.conflicts += 1;
                LedgerOutcome::Uncountable
            }
            UsageUpdate::Initial(u) => {
                let Some(id) = identity(u) else {
                    self.conflicts += 1;
                    return LedgerOutcome::Uncountable;
                };
                match self.entries.get(&id) {
                    Some(old) => {
                        if old.categories == u.categories
                            && old.accounting == u.accounting
                            && money_eq(&old.money, &u.money)
                            && self.same_model(old.slot, &u.model)
                        {
                            LedgerOutcome::Duplicate
                        } else {
                            self.conflicts += 1;
                            LedgerOutcome::Uncountable
                        }
                    }
                    None => self.insert(id, u, prices, policy),
                }
            }
            UsageUpdate::AuthoritativeReplacement(u) => {
                let Some(id) = identity(u) else {
                    self.conflicts += 1;
                    return LedgerOutcome::Uncountable;
                };
                self.revise(&id, u, prices, policy)
            }
        }
    }

    fn insert(
        &mut self,
        id: Identity,
        u: &UsageObservation,
        prices: &PriceSchedule,
        policy: ReferencePolicy,
    ) -> LedgerOutcome {
        if self.frozen || self.entries.len() >= self.max_entries {
            self.frozen = true;
            return LedgerOutcome::Uncountable;
        }
        let (contribution, meta, selection) =
            self.contribution(PricedFacts::of(u), prices, policy, None);
        let request = u.request.as_ref();
        let keep = |v: Option<&str>, standard: fn(&str) -> bool| {
            v.filter(|t| !standard(t)).map(Box::<str>::from)
        };
        let service_tier = keep(
            request.and_then(|r| r.service_tier.as_deref()),
            crate::pricing::is_standard_tier,
        );
        let inference_region = keep(
            request.and_then(|r| r.inference_region.as_deref()),
            crate::pricing::is_standard_region,
        );
        // Everything retained is charged: the entry, its key, recorded strings,
        // the price context and metadata copies, and a model identity new to the ledger.
        let fixed_bytes = std::mem::size_of::<Stored>()
            + std::mem::size_of::<Identity>()
            + ENTRY_OVERHEAD
            + id.1.len()
            + service_tier.as_ref().map_or(0, |t| t.len())
            + inference_region.as_ref().map_or(0, |t| t.len());
        let bytes = fixed_bytes + dynamic_bytes(&u.money, selection.as_ref());
        let slot_bytes = self.new_slot_bytes(&u.model);
        let meta_delta = self.meta_delta(self.slot_kind(&u.model), &meta);
        let net = (bytes + slot_bytes) as isize + meta_delta;
        if net > self.max_bytes.saturating_sub(self.bytes) as isize {
            self.frozen = true;
            return LedgerOutcome::Uncountable;
        }
        let slot = self.slot_for(&u.model);
        self.apply(slot, &contribution, true);
        self.note_meta(slot, meta);
        self.bytes = (self.bytes as isize + net).max(0) as usize;
        self.entries.insert(
            id,
            Stored {
                scope: u.scope,
                categories: u.categories.clone(),
                accounting: u.accounting,
                harness: u.provenance.harness,
                slot,
                started_at: request.and_then(|r| r.started_at),
                completed_at: request.and_then(|r| r.completed_at),
                service_tier,
                inference_region,
                money: u.money.clone(),
                selection,
                fixed_bytes,
                bytes,
                contribution,
            },
        );
        LedgerOutcome::Counted
    }

    fn slot_kind(&self, model: &Option<ModelDetail>) -> SlotKind {
        match model {
            None => SlotKind::Unmodeled,
            Some(m) => self
                .slot_index
                .get(&(m.provider.clone(), m.id.clone()))
                .map_or(SlotKind::New, |i| SlotKind::Existing(*i)),
        }
    }

    /// Net change in bytes the total and per-model aggregates retain once `meta`
    /// is recorded.
    fn meta_delta(&self, kind: SlotKind, meta: &PriceMeta) -> isize {
        let mut delta = 0isize;
        let mut add = |agg: Option<&Agg>| {
            let (old, new) = meta_bytes_after(agg, meta);
            delta += new as isize - old as isize;
        };
        add(Some(&self.total));
        match kind {
            SlotKind::Unmodeled => {}
            SlotKind::Existing(i) => add(Some(&self.slots[i].agg)),
            SlotKind::New => add(None),
        }
        delta
    }

    /// Budget charge for a model identity not yet interned.
    fn new_slot_bytes(&self, model: &Option<ModelDetail>) -> usize {
        let Some(m) = model else { return 0 };
        let key = (m.provider.clone(), m.id.clone());
        if self.slot_index.contains_key(&key) {
            return 0;
        }
        // The identity is held by both the slot and its index key.
        std::mem::size_of::<ModelSlot>()
            + std::mem::size_of::<ModelKey>()
            + ENTRY_OVERHEAD
            + 2 * (m.id.len() + m.provider.as_ref().map_or(0, String::len))
    }

    fn revise(
        &mut self,
        id: &Identity,
        u: &UsageObservation,
        prices: &PriceSchedule,
        policy: ReferencePolicy,
    ) -> LedgerOutcome {
        let Some(old) = self.entries.get(id) else {
            self.conflicts += 1;
            return LedgerOutcome::Uncountable;
        };
        if !self.same_model(old.slot, &u.model) || old.accounting != u.accounting {
            self.conflicts += 1;
            return LedgerOutcome::Uncountable;
        }
        if old.categories == u.categories && money_eq(&old.money, &u.money) {
            return LedgerOutcome::Duplicate;
        }
        // Original model, scope, request metadata, accounting and price context;
        // revised counters and money.
        let request = RequestMetadata {
            started_at: old.started_at,
            completed_at: old.completed_at,
            service_tier: old.service_tier.as_deref().map(str::to_owned),
            inference_region: old.inference_region.as_deref().map(str::to_owned),
            ..RequestMetadata::default()
        };
        let facts = PricedFacts {
            model: old.slot.map(|i| &self.slots[i].detail),
            scope: old.scope,
            categories: &u.categories,
            accounting: old.accounting,
            request: Some(&request),
            money: &u.money,
            harness: old.harness,
        };
        let (revised, meta, selection) =
            self.contribution(facts, prices, policy, old.selection.as_ref());
        // Charge the complete retained state after pricing and before applying.
        let new_bytes = old.fixed_bytes + dynamic_bytes(&u.money, selection.as_ref());
        let net = new_bytes as isize - old.bytes as isize
            + self.meta_delta(
                old.slot.map_or(SlotKind::Unmodeled, SlotKind::Existing),
                &meta,
            );
        if net > self.max_bytes.saturating_sub(self.bytes) as isize {
            self.conflicts += 1;
            return LedgerOutcome::Uncountable;
        }
        let slot = old.slot;
        let previous = old.contribution.clone();
        self.apply(slot, &previous, false);
        self.apply(slot, &revised, true);
        self.note_meta(slot, meta);
        self.bytes = (self.bytes as isize + net).max(0) as usize;
        if let Some(entry) = self.entries.get_mut(id) {
            entry.categories = u.categories.clone();
            entry.money = u.money.clone();
            entry.selection = selection;
            entry.bytes = new_bytes;
            entry.contribution = revised;
        }
        LedgerOutcome::Revised
    }

    fn reason(&self) -> Option<String> {
        if let Some(reason) = &self.marked {
            return Some(reason.clone());
        }
        if self.frozen {
            return Some("request ledger is full; later requests are not counted".into());
        }
        (self.conflicts > 0).then(|| {
            format!(
                "{} request update(s) could not be attributed",
                self.conflicts
            )
        })
    }

    pub fn snapshot(&self) -> UsageSummary {
        let reason = self.reason();
        let coverage = self.total.coverage(reason.as_deref());
        let n = self.total.requests;
        let mut models: Vec<ModelUsageSummary> = self
            .slots
            .iter()
            .filter(|s| s.agg.requests > 0)
            .map(|s| {
                let categories = s.agg.categories();
                ModelUsageSummary {
                    model: s.detail.clone(),
                    total: None,
                    categories,
                    cost: s.agg.cost(),
                    coverage: s.agg.coverage(None),
                }
            })
            .collect();
        models.sort_by(|a, b| {
            (&a.model.provider, &a.model.id).cmp(&(&b.model.provider, &b.model.id))
        });
        let mut cost = self.total.cost();
        if !coverage.complete && cost.usd.is_some() {
            // Uncounted requests mean the sum is not the session's cost.
            cost.usd = None;
            cost.class = AggregationClass::Unknown;
            cost.note = coverage.reason.clone();
        }
        UsageSummary {
            categories: self.total.categories(),
            total: None,
            class: if reason.is_none() {
                AggregationClass::Derived
            } else {
                AggregationClass::Partial
            },
            cost,
            reported_cost_usd: reason
                .is_none()
                .then(|| self.total.reported.value(n))
                .flatten(),
            saved_estimate_usd: reason
                .is_none()
                .then(|| self.total.saved.value(n))
                .flatten(),
            coverage,
            models,
        }
    }
}

// ---------------------------------------------------------------------------
// Claude per-message ledger
// ---------------------------------------------------------------------------

/// Exact, bounded message identity ledger, independent of transcript retention.
/// The first record of a message id wins; later records of it are ignored.
pub(crate) struct ClaudeLedger {
    inner: UsageLedger,
    frozen: bool,
    partial: bool,
    server_tools: Option<ServerToolCounts>,
}

impl ClaudeLedger {
    pub(crate) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            inner: UsageLedger::new(max_entries, max_bytes),
            frozen: false,
            partial: false,
            server_tools: None,
        }
    }

    fn mark(&mut self, reason: &str) {
        self.partial = true;
        self.inner.mark_partial(reason);
    }

    pub(crate) fn observe(
        &mut self,
        id: Option<&str>,
        usage: Option<&UsageObservation>,
        tools: Option<ServerToolCounts>,
        prices: &PriceSchedule,
        policy: ReferencePolicy,
    ) {
        if self.frozen {
            return;
        }
        let Some(id) = id.filter(|id| !id.is_empty()) else {
            self.mark("an assistant message has no id");
            return;
        };
        if self.inner.knows_request(id) {
            return;
        }
        // Do not reserve an ID until its usage exists: a later block can supply it.
        let Some(usage) = usage else {
            self.mark("an assistant message has no usage");
            return;
        };
        if self
            .inner
            .observe(UsageUpdate::Initial(usage), prices, policy)
            == LedgerOutcome::Uncountable
        {
            self.frozen = true;
            self.mark("the message ledger is full");
            return;
        }
        if let Some(tools) = tools {
            let counts = self.server_tools.get_or_insert_default();
            counts.web_search = counts.web_search.saturating_add(tools.web_search);
            counts.web_fetch = counts.web_fetch.saturating_add(tools.web_fetch);
        }
    }

    pub(crate) fn summary(&self) -> Option<UsageSummary> {
        if self.inner.observed_requests() == 0 && !self.partial {
            return None;
        }
        let mut summary = self.inner.snapshot();
        summary.total = claude_total(&summary.categories);
        for m in &mut summary.models {
            m.total = claude_total(&m.categories);
        }
        if summary.cost.class == AggregationClass::Estimated
            && self
                .server_tools
                .is_some_and(|counts| counts.web_search != 0 || counts.web_fetch != 0)
        {
            summary.cost.note = Some("server-tool fees excluded".into());
        }
        Some(summary)
    }

    pub(crate) fn server_tools(&self) -> Option<ServerToolCounts> {
        self.server_tools
    }
}

/// Codex: the latest cumulative `total_tokens` snapshot. Never a sum of
/// snapshots. Without a reported total, `input + output` (cached input and
/// reasoning are subsets of those).
pub fn codex_total(input: &TokenCategories) -> Option<u64> {
    input
        .total
        .or_else(|| input.input?.checked_add(input.output?))
}

/// Claude: `input + cache_creation + cache_read + output`. Thinking is a
/// subset of output and iterations are not added. Any missing category is `None`.
pub fn claude_total(input: &TokenCategories) -> Option<u64> {
    input
        .input?
        .checked_add(cache_write_total(input)?)?
        .checked_add(input.cache_read?)?
        .checked_add(input.output?)
}

/// OpenCode: the reported session total; otherwise the additive categories.
pub fn opencode_total(input: &TokenCategories) -> Option<u64> {
    if input.total.is_some() {
        return input.total;
    }
    input
        .input?
        .checked_add(input.output?)?
        .checked_add(input.reasoning?)?
        .checked_add(input.cache_read?)?
        .checked_add(cache_write_total(input)?)
}

/// Cache-write tokens: the single total, else the sum of the TTL split.
fn cache_write_total(u: &TokenCategories) -> Option<u64> {
    match (u.cache_write, u.cache_write_5m, u.cache_write_1h) {
        (Some(w), _, _) => Some(w),
        (None, Some(a), Some(b)) => a.checked_add(b),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Session-scope aggregates
// ---------------------------------------------------------------------------

/// Which model a session-wide counter set may be attributed to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Attribution<'a> {
    /// Evidence shows the counters came from this one model.
    Model(&'a ModelDetail),
    /// Evidence shows more than one model contributed.
    Mixed,
    /// No model was recorded.
    Unrecorded,
    /// A model is recorded but nothing evidences which model produced the counters.
    Unproven,
}

/// Prices session-wide counters. Valid only where no request-dependent choice
/// (threshold tier, tariff, effective card) is needed; the engine declines
/// those because an aggregate is not one request. Counters that span several
/// models are never priced at one model's rates.
pub(crate) fn aggregate_cost(
    harness: Harness,
    attribution: Attribution<'_>,
    categories: &TokenCategories,
    accounting: TokenAccounting,
    money: &RecordedMoney,
    prices: &PriceSchedule,
    policy: ReferencePolicy,
) -> CostEstimate {
    let model = match attribution {
        Attribution::Model(m) => Some(m),
        _ => None,
    };
    let mut cost = estimate_facts(
        PricedFacts {
            model,
            scope: UsageScope::Session,
            categories,
            accounting,
            request: None,
            money,
            harness,
        },
        prices,
        policy,
    );
    if cost.usd.is_none() {
        let why = match attribution {
            Attribution::Mixed => "session usage spans several models, so no single price applies",
            Attribution::Unproven => "nothing evidences which model produced the session totals",
            _ => "",
        };
        if !why.is_empty() {
            // Keep any "source reported no charge" prefix; replace only the model reason.
            cost.note = Some(match cost.note.take() {
                Some(n) if n.contains("no model recorded") => n.replace("no model recorded", why),
                _ => why.to_owned(),
            });
        }
    }
    cost
}

/// Canonical billable categories of raw session counters. Counters whose
/// accounting cannot be proven stay unknown instead of being guessed.
fn aggregate_categories(raw: &TokenCategories, accounting: TokenAccounting) -> TokenCategories {
    match legs(raw, accounting) {
        Ok(l) => canonical(&l, raw),
        Err(_) => TokenCategories {
            total: raw.total,
            ..TokenCategories::default()
        },
    }
}

/// Summary of provider-reported session counters. They are not requests, so
/// coverage records no observed requests and says why. `raw` is normalized to
/// canonical legs here; the provider `total` and the raw counters in the
/// source observation are untouched. A per-model entry exists only when the
/// attribution is evidenced.
pub(crate) fn aggregate_summary(
    attribution: Attribution<'_>,
    raw: &TokenCategories,
    accounting: TokenAccounting,
    total: Option<u64>,
    class: AggregationClass,
    cost: CostEstimate,
    money: &RecordedMoney,
) -> UsageSummary {
    let categories = aggregate_categories(raw, accounting);
    let attributed = matches!(attribution, Attribution::Model(_));
    let reason = match attribution {
        Attribution::Model(_) => "session counters; request boundaries are unknown",
        Attribution::Mixed => "session counters span several models; no per-model attribution",
        Attribution::Unrecorded => "session counters have no recorded model",
        Attribution::Unproven => "session counters have no per-message model evidence",
    };
    let coverage = UsageCoverage {
        observed_requests: 0,
        priced_requests: 0,
        complete: attributed && cost.usd.is_some(),
        reason: Some(reason.into()),
    };
    let models = match attribution {
        Attribution::Model(m) => vec![ModelUsageSummary {
            model: ModelDetail {
                id: m.id.clone(),
                provider: m.provider.clone(),
                variant: m.variant.clone(),
                effort: m.effort.clone(),
                configured_effort: m.configured_effort.clone(),
            },
            categories: categories.clone(),
            total,
            cost: cost.clone(),
            coverage: coverage.clone(),
        }],
        _ => Vec::new(),
    };
    UsageSummary {
        categories,
        total,
        class,
        cost,
        reported_cost_usd: usable_money(&money.reported_api),
        saved_estimate_usd: usable_money(&money.saved_estimate),
        coverage,
        models,
    }
}

/// Usage derived from unique OpenCode assistant messages. Only used when the
/// session row reports no totals; never added to reported totals.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct DerivedUsage {
    /// Sums over the messages that reported each category (`None` when none did).
    pub categories: TokenCategories,
    /// Number of unique assistant messages counted.
    pub messages: usize,
    /// Sum of message-reported costs, only when every message reported one.
    pub cost: Option<f64>,
    /// False when any message lacked data or the scan hit its row cap.
    pub complete: bool,
    /// Distinct `(billing provider, model id)` pairs named by the messages (bounded).
    pub models: Vec<(Option<String>, String)>,
    /// Some counted message named no model.
    pub unnamed: bool,
}

/// Accumulates assistant messages by exact id so a repeated id counts once.
pub(crate) struct OpenCodeDerivation {
    seen: HashSet<String>,
    sums: [Option<u64>; 5],
    missing: [bool; 5],
    cost: f64,
    cost_missing: bool,
    complete: bool,
    models: Vec<(Option<String>, String)>,
    unnamed: bool,
}

impl OpenCodeDerivation {
    pub(crate) fn new() -> Self {
        Self {
            seen: HashSet::new(),
            sums: [None; 5],
            missing: [false; 5],
            cost: 0.0,
            cost_missing: false,
            complete: true,
            models: Vec::new(),
            unnamed: false,
        }
    }

    /// Counts one assistant message. `tokens` is `None` when the record was
    /// unreadable (oversized or malformed). Repeated ids are ignored.
    pub(crate) fn add(
        &mut self,
        id: &str,
        tokens: Option<&TokenCategories>,
        cost: Option<f64>,
        model: Option<(Option<String>, String)>,
    ) {
        if !self.seen.insert(id.to_owned()) {
            return;
        }
        match model {
            Some(m) => {
                if self.models.len() < 8 && !self.models.contains(&m) {
                    self.models.push(m);
                }
            }
            None => self.unnamed = true,
        }
        let Some(t) = tokens else {
            self.missing = [true; 5];
            self.cost_missing = true;
            self.complete = false;
            return;
        };
        let values = [t.input, t.output, t.reasoning, t.cache_read, t.cache_write];
        for (i, value) in values.into_iter().enumerate() {
            match value {
                Some(v) => self.sums[i] = Some(self.sums[i].unwrap_or(0).saturating_add(v)),
                None => self.missing[i] = true,
            }
        }
        match cost {
            Some(c) => self.cost += c,
            None => self.cost_missing = true,
        }
    }

    /// The scan stopped before the last message.
    pub(crate) fn mark_truncated(&mut self) {
        self.complete = false;
    }

    pub(crate) fn finish(self) -> DerivedUsage {
        let pick = |i: usize| self.sums[i];
        let complete = self.complete && !self.missing.iter().any(|m| *m);
        let messages = self.seen.len();
        DerivedUsage {
            categories: TokenCategories {
                input: pick(0),
                output: pick(1),
                reasoning: pick(2),
                cache_read: pick(3),
                cache_write: pick(4),
                ..TokenCategories::default()
            },
            messages,
            cost: (messages > 0 && !self.cost_missing && complete).then_some(self.cost),
            complete,
            models: self.models,
            unnamed: self.unnamed,
        }
    }
}

fn has_all_categories(c: &TokenCategories) -> bool {
    [c.input, c.output, c.reasoning, c.cache_read, c.cache_write]
        .iter()
        .all(Option::is_some)
}

fn any_category(c: &TokenCategories) -> bool {
    [c.input, c.output, c.reasoning, c.cache_read, c.cache_write]
        .iter()
        .any(Option::is_some)
}

/// OpenCode session usage. Session columns are authoritative (`Reported`);
/// message-derived totals are used only when every token column is absent and are
/// labeled `Derived` (`Partial` when incomplete). A reported cost above zero wins
/// over everything; a reported zero is kept but never replaces a priced
/// API-equivalent estimate; otherwise the price engine decides.
pub(crate) fn opencode_summary(
    model: Option<&ModelDetail>,
    session: &TokenCategories,
    session_cost: Option<f64>,
    derived: Option<&DerivedUsage>,
    prices: &PriceSchedule,
    policy: ReferencePolicy,
) -> Option<UsageSummary> {
    let derived = derived.filter(|d| d.messages > 0);
    let (categories, class) = if any_category(session) {
        let class = if has_all_categories(session) {
            AggregationClass::Reported
        } else {
            AggregationClass::Partial
        };
        (session.clone(), class)
    } else if let Some(d) = derived.filter(|d| any_category(&d.categories)) {
        let class = if d.complete && has_all_categories(&d.categories) {
            AggregationClass::Derived
        } else {
            AggregationClass::Partial
        };
        (d.categories.clone(), class)
    } else if session_cost.is_some() || derived.is_some_and(|d| d.cost.is_some()) {
        (TokenCategories::default(), AggregationClass::Unknown)
    } else {
        return None;
    };
    // A partial sum is not a total.
    let total = match class {
        AggregationClass::Reported | AggregationClass::Derived => opencode_total(&categories),
        _ => None,
    };
    let from_derived = session_cost.is_none() && derived.is_some_and(|d| d.cost.is_some());
    let reported = session_cost.or_else(|| derived.and_then(|d| d.cost));
    let money = RecordedMoney {
        reported_api: reported.map(|usd| MoneyFact {
            usd,
            source: MoneySource::ReportedApi,
            as_of: None,
        }),
        ..RecordedMoney::default()
    };
    let accounting = TokenAccounting {
        input: InputAccounting::AdditiveCaches,
        output: OutputAccounting::SeparateReasoning,
    };
    let complete_tokens = matches!(
        class,
        AggregationClass::Reported | AggregationClass::Derived
    );
    // Session columns name no model per message, so attribution needs positive
    // evidence: a scanned message set in which every message names a model and all
    // agree with the session's. Another named model proves the totals are mixed;
    // no scan, or an unnamed message, leaves the attribution unproven.
    let attribution = match model {
        None => Attribution::Unrecorded,
        Some(m) => match derived {
            Some(d)
                if d.models
                    .iter()
                    .any(|(p, id)| *id != m.id || (p.is_some() && *p != m.provider)) =>
            {
                Attribution::Mixed
            }
            Some(d) if !d.unnamed && !d.models.is_empty() && d.complete => Attribution::Model(m),
            _ => Attribution::Unproven,
        },
    };
    let mut cost = if reported.is_none() && !complete_tokens {
        CostEstimate {
            note: Some("token data incomplete".into()),
            ..CostEstimate::default()
        }
    } else {
        aggregate_cost(
            Harness::OpenCode,
            attribution,
            &categories,
            accounting,
            &money,
            prices,
            policy,
        )
    };
    if from_derived && cost.class == AggregationClass::Reported {
        cost.class = AggregationClass::Derived;
        cost.note = Some("sum of unique message-reported costs".into());
    }
    Some(aggregate_summary(
        attribution,
        &categories,
        accounting,
        total,
        class,
        cost,
        &money,
    ))
}
