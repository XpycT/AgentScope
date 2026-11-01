//! Offline price cards and the request-aware cost engine.
//!
//! The bundled catalog is reviewed source data checked in with the binary
//! (`pricing/catalog.json`); nothing is fetched at runtime. A card is keyed by
//! the *billing provider* recorded with a model (never by the harness), then by
//! the exact model id. Missing prices stay unknown, never zero, and every
//! amount is an estimate unless a source reported it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, NaiveDate};
use serde::Deserialize;
use thiserror::Error;

use crate::domain::{
    AggregationClass, CostBreakdown, CostEstimate, Harness, ModelDetail, MoneyFact, MoneySource,
    RecordedMoney, RequestMetadata, TokenAccounting, TokenCategories, UsageObservation, UsageScope,
};
use crate::usage::{Leg, Legs, legs};

const BUNDLED: &str = include_str!("pricing/catalog.json");
const SCHEMA_VERSION: u32 = 1;
const MS_PER_DAY: i64 = 86_400_000;

#[derive(Debug, Error)]
pub enum PricingError {
    #[error("unreadable price catalog: {0}")]
    Parse(String),
    #[error("unsupported price catalog schema_version {0}")]
    Schema(u32),
    #[error("invalid price card {provider:?}/{model:?}: {reason}")]
    InvalidCard {
        provider: String,
        model: String,
        reason: String,
    },
    #[error("duplicate price card for billing_provider {provider:?}, model {model:?}")]
    Duplicate { provider: String, model: String },
    #[error(
        "price cards for billing_provider {provider:?}, model {model:?} have overlapping effective windows"
    )]
    Overlap { provider: String, model: String },
}

/// USD per million tokens. `None` is "not priced", which stays unknown when the
/// leg is used; an explicit `0.0` is a proven free leg.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Rates {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    /// Cache writes without a recorded retention.
    pub cache_write: Option<f64>,
    /// Separately reported reasoning tokens; never applied to reasoning that
    /// the source already counts inside output.
    pub reasoning: Option<f64>,
}

impl Rates {
    fn all(&self) -> [Option<f64>; 7] {
        [
            self.input,
            self.output,
            self.cache_read,
            self.cache_write_5m,
            self.cache_write_1h,
            self.cache_write,
            self.reasoning,
        ]
    }
}

/// A request whose prompt exceeds `input_tokens` pays `above` instead.
/// `inclusive` means the threshold value itself still belongs to the lower tier.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Threshold {
    pub input_tokens: u64,
    #[serde(default)]
    pub inclusive: bool,
    pub above: Rates,
}

/// UTC wall-clock window with its own rates. `days` are 0 = Monday ..= 6 =
/// Sunday; `start_minute` is inclusive and `end_minute` exclusive.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TariffWindow {
    pub name: String,
    pub days: Vec<u8>,
    pub start_minute: u16,
    pub end_minute: u16,
    pub rates: Rates,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PriceCard {
    pub billing_provider: String,
    pub model: String,
    /// Further exact model ids priced identically.
    pub aliases: Vec<String>,
    pub source: Option<String>,
    pub checked_at: Option<String>,
    /// Date shown with the estimate; the catalog check date when absent.
    pub as_of: Option<String>,
    /// First instant (inclusive, UTC) at which the card applies.
    pub effective_from: Option<String>,
    /// First instant (exclusive, UTC) at which the card no longer applies.
    pub effective_until: Option<String>,
    pub note: Option<String>,
    pub rates: Rates,
    pub threshold: Option<Threshold>,
    pub tariff: Vec<TariffWindow>,
    /// Known non-standard service tiers (e.g. `fast`) as whole-request multipliers.
    pub service_multipliers: BTreeMap<String, f64>,
    /// Known non-global inference regions as whole-request multipliers.
    pub region_multipliers: BTreeMap<String, f64>,
    /// Legs a service tier does not publish a price for, even though the base
    /// card prices them; they stay unknown under that tier.
    pub service_unpublished: BTreeMap<String, UnpublishedLegs>,
    pub context_capacity: Option<u64>,
    #[serde(skip)]
    pub origin: CardOrigin,
    #[serde(skip)]
    from_ms: Option<i64>,
    #[serde(skip)]
    until_ms: Option<i64>,
}

impl Default for PriceCard {
    fn default() -> Self {
        Self {
            billing_provider: String::new(),
            model: String::new(),
            aliases: Vec::new(),
            source: None,
            checked_at: None,
            as_of: None,
            effective_from: None,
            effective_until: None,
            note: None,
            rates: Rates::default(),
            threshold: None,
            tariff: Vec::new(),
            service_multipliers: BTreeMap::new(),
            region_multipliers: BTreeMap::new(),
            service_unpublished: BTreeMap::new(),
            context_capacity: None,
            origin: CardOrigin::Bundled,
            from_ms: None,
            until_ms: None,
        }
    }
}

/// Leg names (`input`, `output`, `cache_read`, `cache_write_5m`,
/// `cache_write_1h`, `cache_write`, `reasoning`) a service tier leaves unpriced.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct UnpublishedLegs {
    pub base: Vec<String>,
    /// Applies when the request is in the card's upper length tier.
    pub above: Vec<String>,
}

/// Where a card came from, which labels the estimate that uses it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CardOrigin {
    #[default]
    Bundled,
    User,
}

/// A user card that replaces every bundled card with the same provider and model.
#[derive(Clone, Debug, PartialEq)]
pub struct PriceOverride {
    pub card: PriceCard,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogSource {
    pub id: String,
    pub name: String,
    pub url: String,
    pub checked_at: String,
}

/// One reviewed row of an official table and what became of it.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryRow {
    pub source: String,
    pub row: String,
    /// `priced`, `free`, or `unmapped` (no officially evidenced model id).
    pub status: String,
    pub model: Option<String>,
    pub note: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogMeta {
    pub revision: String,
    pub checked_at: String,
    pub sources: Vec<CatalogSource>,
    pub inventory: Vec<InventoryRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCatalog {
    schema_version: u32,
    revision: String,
    checked_at: String,
    #[serde(default)]
    sources: Vec<CatalogSource>,
    #[serde(default)]
    inventory: Vec<InventoryRow>,
    cards: Vec<PriceCard>,
}

/// Cards keyed by billing provider, then exact model id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PriceSchedule {
    cards: HashMap<String, HashMap<String, Vec<Arc<PriceCard>>>>,
    /// Exact `(provider, model)` routes evidenced by the bundled catalog, kept
    /// independently of user overrides. Capability inference must never treat a
    /// user-configured price as model/format evidence.
    bundled_routes: HashMap<String, HashSet<String>>,
    meta: CatalogMeta,
}

/// Whether a missing billing route may fall back to the harness's own API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferencePolicy {
    Disabled,
    /// Native Claude/Codex usage with no recorded provider is priced at the
    /// vendor API rate, labeled as a reference estimate.
    NativeApi(Harness),
}

fn invalid(card: &PriceCard, reason: impl Into<String>) -> PricingError {
    PricingError::InvalidCard {
        provider: card.billing_provider.clone(),
        model: card.model.clone(),
        reason: reason.into(),
    }
}

/// Parses `YYYY-MM-DD` (UTC midnight) or an RFC 3339 instant into epoch ms.
fn parse_instant(text: &str) -> Option<i64> {
    if let Ok(date) = NaiveDate::parse_from_str(text, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().timestamp_millis());
    }
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn validate_rates(card: &PriceCard, rates: &Rates, what: &str) -> Result<(), PricingError> {
    for v in rates.all().into_iter().flatten() {
        if !v.is_finite() || v < 0.0 {
            return Err(invalid(
                card,
                format!("{what} rates must be finite non-negative numbers"),
            ));
        }
    }
    Ok(())
}

fn validate_multipliers(card: &PriceCard, map: &BTreeMap<String, f64>) -> Result<(), PricingError> {
    for (name, v) in map {
        if name.trim().is_empty() || !v.is_finite() || *v <= 0.0 {
            return Err(invalid(
                card,
                "multipliers need a name and a finite positive value",
            ));
        }
    }
    Ok(())
}

/// Validates a card and resolves its effective window.
fn prepare(mut card: PriceCard, origin: CardOrigin) -> Result<PriceCard, PricingError> {
    if card.billing_provider.trim().is_empty() {
        return Err(invalid(&card, "billing_provider is empty"));
    }
    if card.model.trim().is_empty() || card.aliases.iter().any(|a| a.trim().is_empty()) {
        return Err(invalid(&card, "model id is empty"));
    }
    validate_rates(&card, &card.rates, "base")?;
    validate_multipliers(&card, &card.service_multipliers)?;
    validate_multipliers(&card, &card.region_multipliers)?;
    for (tier, legs) in &card.service_unpublished {
        if !card.service_multipliers.contains_key(tier)
            || legs
                .base
                .iter()
                .chain(&legs.above)
                .any(|n| !LEG_NAMES.contains(&n.as_str()))
        {
            return Err(invalid(
                &card,
                "service_unpublished needs a known service tier and leg names",
            ));
        }
    }
    if let Some(t) = &card.threshold {
        if t.input_tokens == 0 {
            return Err(invalid(&card, "threshold input_tokens must be positive"));
        }
        validate_rates(&card, &t.above, "threshold")?;
        if !card.tariff.is_empty() {
            return Err(invalid(
                &card,
                "a card cannot combine a threshold and a tariff schedule",
            ));
        }
    }
    for w in &card.tariff {
        if w.days.is_empty()
            || w.days.iter().any(|d| *d > 6)
            || w.start_minute >= w.end_minute
            || w.end_minute > 1440
        {
            return Err(invalid(&card, "tariff window days or minutes are invalid"));
        }
        validate_rates(&card, &w.rates, "tariff")?;
    }
    for (i, a) in card.tariff.iter().enumerate() {
        for b in &card.tariff[i + 1..] {
            let shared_day = a.days.iter().any(|d| b.days.contains(d));
            if shared_day && a.start_minute < b.end_minute && b.start_minute < a.end_minute {
                return Err(invalid(&card, "tariff windows overlap on a shared weekday"));
            }
        }
    }
    let parse = |value: &Option<String>, name: &str| -> Result<Option<i64>, PricingError> {
        match value {
            None => Ok(None),
            Some(text) => parse_instant(text)
                .map(Some)
                .ok_or_else(|| invalid(&card, format!("{name} is not a date or RFC 3339 instant"))),
        }
    };
    let from_ms = parse(&card.effective_from, "effective_from")?;
    let until_ms = parse(&card.effective_until, "effective_until")?;
    card.from_ms = from_ms;
    card.until_ms = until_ms;
    if let (Some(a), Some(b)) = (card.from_ms, card.until_ms)
        && a >= b
    {
        return Err(invalid(
            &card,
            "effective_from must precede effective_until",
        ));
    }
    card.origin = origin;
    Ok(card)
}

impl PriceCard {
    /// A card with only rates; metadata, tiers and windows start empty.
    pub fn new(billing_provider: &str, model: &str, rates: Rates) -> Self {
        Self {
            billing_provider: billing_provider.to_owned(),
            model: model.to_owned(),
            rates,
            ..Self::default()
        }
    }

    fn covers(&self, at: i64) -> bool {
        self.from_ms.is_none_or(|f| at >= f) && self.until_ms.is_none_or(|u| at < u)
    }

    fn bounded(&self) -> bool {
        self.from_ms.is_some() || self.until_ms.is_some()
    }
}

impl PriceSchedule {
    /// The reviewed official catalog compiled into the binary.
    pub fn bundled() -> Result<Self, PricingError> {
        Self::from_json(BUNDLED)
    }

    pub fn from_json(data: &str) -> Result<Self, PricingError> {
        let raw: RawCatalog =
            serde_json::from_str(data).map_err(|e| PricingError::Parse(e.to_string()))?;
        if raw.schema_version != SCHEMA_VERSION {
            return Err(PricingError::Schema(raw.schema_version));
        }
        let mut schedule = Self {
            cards: HashMap::new(),
            bundled_routes: HashMap::new(),
            meta: CatalogMeta {
                revision: raw.revision,
                checked_at: raw.checked_at,
                sources: raw.sources,
                inventory: raw.inventory,
            },
        };
        let mut grouped: BTreeMap<(String, String), Vec<PriceCard>> = BTreeMap::new();
        for card in raw.cards {
            Self::expand(card, CardOrigin::Bundled, &mut grouped)?;
        }
        for (provider, model) in grouped.keys() {
            schedule
                .bundled_routes
                .entry(provider.clone())
                .or_default()
                .insert(model.clone());
        }
        schedule.install(grouped)?;
        Ok(schedule)
    }

    /// Replaces every card whose provider/model matches an override; legs a
    /// user card leaves out stay unknown rather than inheriting bundled rates.
    pub fn with_overrides(mut self, overrides: Vec<PriceOverride>) -> Result<Self, PricingError> {
        let mut grouped: BTreeMap<(String, String), Vec<PriceCard>> = BTreeMap::new();
        for o in overrides {
            Self::expand(o.card, CardOrigin::User, &mut grouped)?;
        }
        for ((provider, model), cards) in &grouped {
            if let Some(models) = self.cards.get_mut(provider) {
                models.remove(model);
            }
            Self::check_windows(provider, model, cards)?;
        }
        for ((provider, model), mut cards) in grouped {
            cards.sort_by_key(|c| c.from_ms.unwrap_or(i64::MIN));
            self.cards
                .entry(provider)
                .or_default()
                .insert(model, cards.into_iter().map(Arc::new).collect());
        }
        Ok(self)
    }

    fn expand(
        card: PriceCard,
        origin: CardOrigin,
        grouped: &mut BTreeMap<(String, String), Vec<PriceCard>>,
    ) -> Result<(), PricingError> {
        let mut card = prepare(card, origin)?;
        let aliases = std::mem::take(&mut card.aliases);
        for alias in aliases {
            let mut copy = card.clone();
            copy.model = alias;
            grouped
                .entry((copy.billing_provider.clone(), copy.model.clone()))
                .or_default()
                .push(copy);
        }
        grouped
            .entry((card.billing_provider.clone(), card.model.clone()))
            .or_default()
            .push(card);
        Ok(())
    }

    fn check_windows(provider: &str, model: &str, cards: &[PriceCard]) -> Result<(), PricingError> {
        let mut order: Vec<&PriceCard> = cards.iter().collect();
        order.sort_by_key(|c| c.from_ms.unwrap_or(i64::MIN));
        for pair in order.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            if a.from_ms == b.from_ms && a.until_ms == b.until_ms {
                return Err(PricingError::Duplicate {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                });
            }
            if a.until_ms.is_none_or(|u| b.from_ms.is_none_or(|f| f < u)) {
                return Err(PricingError::Overlap {
                    provider: provider.to_owned(),
                    model: model.to_owned(),
                });
            }
        }
        Ok(())
    }

    fn install(
        &mut self,
        grouped: BTreeMap<(String, String), Vec<PriceCard>>,
    ) -> Result<(), PricingError> {
        for ((provider, model), mut cards) in grouped {
            cards.sort_by_key(|c| c.from_ms.unwrap_or(i64::MIN));
            Self::check_windows(&provider, &model, &cards)?;
            self.cards
                .entry(provider)
                .or_default()
                .insert(model, cards.into_iter().map(Arc::new).collect());
        }
        Ok(())
    }

    pub fn revision(&self) -> &str {
        &self.meta.revision
    }

    pub fn catalog(&self) -> &CatalogMeta {
        &self.meta
    }

    /// Number of distinct provider/model keys.
    pub fn len(&self) -> usize {
        self.cards.values().map(HashMap::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.cards.is_empty()
    }

    /// Every card for an exact billing provider and model id.
    pub fn cards(&self, provider: &str, model: &str) -> Option<&[Arc<PriceCard>]> {
        self.cards
            .get(provider)?
            .get(model)
            .map(Vec::as_slice)
            .filter(|c| !c.is_empty())
    }

    /// True only when the bundled catalog (not a user override) evidences this
    /// exact provider/model route. This is model-format evidence, not a price:
    /// user-configured entries must never authorize capability inference.
    pub fn bundled_route(&self, provider: &str, model: &str) -> bool {
        self.bundled_routes
            .get(provider)
            .is_some_and(|models| models.contains(model))
    }

    /// Catalog-derived context capacity. It never overrides a harness-recorded
    /// effective limit; callers label it as catalog metadata.
    pub fn model_context_capacity(&self, provider: &str, model: &str) -> Option<u64> {
        self.cards(provider, model)?
            .iter()
            .find_map(|c| c.context_capacity)
    }
}

/// The facts a price depends on, borrowed so the ledger can price a stored
/// request without rebuilding a full observation.
#[derive(Clone, Copy)]
pub(crate) struct PricedFacts<'a> {
    pub model: Option<&'a ModelDetail>,
    pub scope: UsageScope,
    pub categories: &'a TokenCategories,
    pub accounting: TokenAccounting,
    pub request: Option<&'a RequestMetadata>,
    pub money: &'a RecordedMoney,
    pub harness: Harness,
}

impl<'a> PricedFacts<'a> {
    pub(crate) fn of(u: &'a UsageObservation) -> Self {
        Self {
            model: u.model.as_ref(),
            scope: u.scope,
            categories: &u.categories,
            accounting: u.accounting,
            request: u.request.as_ref(),
            money: &u.money,
            harness: u.provenance.harness,
        }
    }
}

/// Which rate set of a card a request was priced from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Base,
    Above,
    Window(usize),
}

/// The price context chosen when a request was first priced: the exact card
/// revision, request-length tier or tariff window, service/region multiplier
/// and the legs the service tier does not publish. A later revision of the
/// request is repriced with this, not with a newer schedule.
#[derive(Clone, Debug)]
pub(crate) struct Selection {
    card: Arc<PriceCard>,
    tier: Tier,
    multiplier: f64,
    /// Bit per leg in `LEG_NAMES` that the service tier leaves unpriced.
    unpublished: u8,
    as_of: Option<String>,
    /// The route was supplied by the native-vendor reference policy, not by a
    /// recorded provider.
    reference: bool,
}

impl Selection {
    /// Heap bytes this selection keeps alive beyond the shared card.
    pub(crate) fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + self.as_of.as_ref().map_or(0, String::len)
    }

    fn rates(&self) -> &Rates {
        match (self.tier, &self.card.threshold) {
            (Tier::Above, Some(t)) => &t.above,
            (Tier::Window(i), _) => self
                .card
                .tariff
                .get(i)
                .map_or(&self.card.rates, |w| &w.rates),
            _ => &self.card.rates,
        }
    }

    fn rate(&self, bit: usize, rate: Option<f64>) -> Option<f64> {
        if self.unpublished & (1 << bit) != 0 {
            None
        } else {
            rate
        }
    }
}

const LEG_NAMES: [&str; 7] = [
    "input",
    "output",
    "cache_read",
    "cache_write_5m",
    "cache_write_1h",
    "cache_write",
    "reasoning",
];

fn leg_mask(names: &[String]) -> u8 {
    names
        .iter()
        .filter_map(|n| LEG_NAMES.iter().position(|l| l == n))
        .fold(0, |m, i| m | (1 << i))
}

/// Estimates one observation. A reported API amount above zero and a saved
/// catalog estimate win; everything else, including a reported zero and
/// unverified money, goes through the engine.
pub fn estimate_cost(
    usage: &UsageObservation,
    prices: &PriceSchedule,
    policy: ReferencePolicy,
) -> CostEstimate {
    estimate_facts(PricedFacts::of(usage), prices, policy)
}

fn usable(fact: &Option<MoneyFact>) -> Option<&MoneyFact> {
    fact.as_ref().filter(|f| f.usd.is_finite() && f.usd >= 0.0)
}

pub(crate) fn estimate_facts(
    facts: PricedFacts<'_>,
    prices: &PriceSchedule,
    policy: ReferencePolicy,
) -> CostEstimate {
    estimate_selected(facts, prices, policy, None).0
}

/// Like [`estimate_facts`], but returns the price context that was used and
/// reuses `retained` instead of selecting again when one is given.
pub(crate) fn estimate_selected(
    facts: PricedFacts<'_>,
    prices: &PriceSchedule,
    policy: ReferencePolicy,
    retained: Option<&Selection>,
) -> (CostEstimate, Option<Selection>) {
    let (mut estimate, selection) = price(facts, prices, policy, retained);
    let money = facts.money;
    let unusable = (money.reported_api.is_some() && usable(&money.reported_api).is_none())
        || (money.saved_estimate.is_some() && usable(&money.saved_estimate).is_none());
    if let Some(saved) = usable(&money.saved_estimate) {
        let cost = CostEstimate {
            usd: Some(saved.usd),
            class: AggregationClass::Estimated,
            as_of: saved.as_of.clone(),
            known_subtotal: saved.usd,
            source: Some(MoneySource::SavedCatalogEstimate),
            ..CostEstimate::default()
        };
        return (cost, selection);
    }
    let reported = usable(&money.reported_api);
    if let Some(r) = reported
        && r.usd > 0.0
    {
        let cost = CostEstimate {
            usd: Some(r.usd),
            class: AggregationClass::Reported,
            as_of: r.as_of.clone(),
            known_subtotal: r.usd,
            source: Some(MoneySource::ReportedApi),
            ..CostEstimate::default()
        };
        return (cost, selection);
    }
    if let Some(r) = reported {
        // A reported zero may be a subscription or a free route. It stays in the
        // reported amount; only the engine decides the API-equivalent value.
        match estimate.usd {
            Some(u) if u <= 0.0 => {
                let cost = CostEstimate {
                    usd: Some(0.0),
                    class: AggregationClass::Reported,
                    as_of: r.as_of.clone(),
                    source: Some(MoneySource::ReportedApi),
                    ..CostEstimate::default()
                };
                return (cost, selection);
            }
            Some(_) => {
                estimate.note =
                    Some("API-equivalent estimate; the source reported no charge".into());
            }
            None => {
                let why = estimate.note.take().unwrap_or_default();
                estimate.note = Some(format!(
                    "the source reported no charge; API-equivalent unknown: {why}"
                ));
            }
        }
    }
    if unusable && estimate.note.is_none() {
        estimate.note = Some("recorded money was unusable and ignored".into());
    }
    (estimate, selection)
}

fn unknown(note: &str) -> CostEstimate {
    CostEstimate {
        usd: None,
        class: AggregationClass::Unknown,
        note: Some(note.to_owned()),
        ..CostEstimate::default()
    }
}

pub(crate) fn is_standard_tier(tier: &str) -> bool {
    ["standard", "default", "auto"]
        .iter()
        .any(|s| tier.eq_ignore_ascii_case(s))
}

pub(crate) fn is_standard_region(region: &str) -> bool {
    ["global", "default"]
        .iter()
        .any(|s| region.eq_ignore_ascii_case(s))
}

fn weekday_and_minute(at: i64) -> (u8, u16) {
    let days = at.div_euclid(MS_PER_DAY);
    // 1970-01-01 was a Thursday; Monday is 0.
    let weekday = (days + 3).rem_euclid(7) as u8;
    let minute = (at.rem_euclid(MS_PER_DAY) / 60_000) as u16;
    (weekday, minute)
}

fn select_card(cards: &[Arc<PriceCard>], at: Option<i64>) -> Result<&Arc<PriceCard>, &'static str> {
    match (cards, at) {
        ([only], None) if !only.bounded() => Ok(only),
        ([only], Some(t)) if only.covers(t) => Ok(only),
        ([_], Some(_)) => Err("no price card is effective at the request time"),
        (_, None) => Err("request time is needed to select the effective price card"),
        (many, Some(t)) => many
            .iter()
            .find(|c| c.covers(t))
            .ok_or("no price card is effective at the request time"),
    }
}

struct Sum {
    known: f64,
    complete: bool,
    unreported: bool,
}

impl Sum {
    fn leg(&mut self, tokens: Leg, rate: Option<f64>, multiplier: f64) -> Option<f64> {
        let cost = match tokens {
            Leg::NotApplicable => return None,
            Leg::Unknown => {
                self.unreported = true;
                None
            }
            Leg::Tokens(0) => Some(0.0),
            Leg::Tokens(n) => rate.map(|r| n as f64 * r * multiplier / 1_000_000.0),
        };
        match cost {
            Some(c) => self.known += c,
            None => self.complete = false,
        }
        cost
    }
}

fn prompt_tokens(l: &Legs) -> Option<u64> {
    [l.input, l.cache_read, l.write_5m, l.write_1h, l.write_other]
        .into_iter()
        .try_fold(0u64, |acc, leg| match leg {
            Leg::NotApplicable => Some(acc),
            Leg::Tokens(n) => acc.checked_add(n),
            Leg::Unknown => None,
        })
}

/// Chooses the card, tier/window and multipliers for a request.
fn select(
    facts: PricedFacts<'_>,
    prices: &PriceSchedule,
    policy: ReferencePolicy,
    l: &Legs,
) -> Result<Selection, &'static str> {
    let Some(model) = facts.model else {
        return Err("no model recorded");
    };
    let (provider, reference) = match (model.provider.as_deref(), policy) {
        (Some(p), _) => (p, false),
        (None, ReferencePolicy::NativeApi(h)) if h == facts.harness => match h {
            Harness::Claude => ("anthropic", true),
            Harness::Codex => ("openai", true),
            _ => return Err("no billing route recorded"),
        },
        (None, _) => return Err("no billing route recorded"),
    };
    let Some(cards) = prices.cards(provider, &model.id) else {
        return Err("no price card for this billing route and model");
    };
    let request = facts.request;
    let at = request.and_then(|r| r.started_at.or(r.completed_at));
    let card = select_card(cards, at)?;

    // Tier, tariff, service and region are all per-request choices.
    let mut multiplier = 1.0;
    let mut service = None;
    if let Some(tier) = request.and_then(|r| r.service_tier.as_deref())
        && !is_standard_tier(tier)
    {
        match card.service_multipliers.get(tier) {
            Some(m) => {
                multiplier *= m;
                service = Some(tier);
            }
            None => return Err("service tier is not priced by the card"),
        }
    }
    if let Some(region) = request.and_then(|r| r.inference_region.as_deref())
        && !is_standard_region(region)
    {
        match card.region_multipliers.get(region) {
            Some(m) => multiplier *= m,
            None => return Err("inference region is not priced by the card"),
        }
    }
    let single_request = facts.scope == UsageScope::Message;
    let tier = if let Some(t) = &card.threshold {
        if !single_request {
            return Err("an aggregate cannot select a request-length price tier");
        }
        let Some(prompt) = prompt_tokens(l) else {
            return Err("prompt length is unknown, so the price tier is unknown");
        };
        if prompt < t.input_tokens || (t.inclusive && prompt == t.input_tokens) {
            Tier::Base
        } else {
            Tier::Above
        }
    } else if !card.tariff.is_empty() {
        if !single_request {
            return Err("an aggregate cannot select a time-of-day tariff");
        }
        let Some(at) = at else {
            return Err("request time is needed to select the tariff");
        };
        let (weekday, minute) = weekday_and_minute(at);
        card.tariff
            .iter()
            .position(|w| {
                w.days.contains(&weekday) && minute >= w.start_minute && minute < w.end_minute
            })
            .map_or(Tier::Base, Tier::Window)
    } else {
        Tier::Base
    };
    let unpublished = service
        .and_then(|s| card.service_unpublished.get(s))
        .map_or(0, |u| {
            leg_mask(if tier == Tier::Above {
                &u.above
            } else {
                &u.base
            })
        });
    let as_of = card.as_of.clone().or_else(|| {
        card.checked_at
            .clone()
            .or_else(|| Some(prices.meta.checked_at.clone()).filter(|s| !s.is_empty()))
    });
    Ok(Selection {
        card: Arc::clone(card),
        tier,
        multiplier,
        unpublished,
        as_of,
        reference,
    })
}

fn price(
    facts: PricedFacts<'_>,
    prices: &PriceSchedule,
    policy: ReferencePolicy,
    retained: Option<&Selection>,
) -> (CostEstimate, Option<Selection>) {
    let l = match legs(facts.categories, facts.accounting) {
        Ok(l) => l,
        Err(e) => {
            // A retained context stays attached even when the counters are unusable.
            return (unknown(&e.to_string()), retained.cloned());
        }
    };
    let selection = match retained {
        Some(s) => s.clone(),
        None => match select(facts, prices, policy, &l) {
            Ok(s) => s,
            Err(note) => return (unknown(note), None),
        },
    };
    let rates = selection.rates();
    let multiplier = selection.multiplier;
    let mut sum = Sum {
        known: 0.0,
        complete: true,
        unreported: false,
    };
    let breakdown = CostBreakdown {
        input: sum.leg(l.input, selection.rate(0, rates.input), multiplier),
        cache_read: sum.leg(
            l.cache_read,
            selection.rate(2, rates.cache_read),
            multiplier,
        ),
        cache_write_5m: sum.leg(
            l.write_5m,
            selection.rate(3, rates.cache_write_5m),
            multiplier,
        ),
        cache_write_1h: sum.leg(
            l.write_1h,
            selection.rate(4, rates.cache_write_1h),
            multiplier,
        ),
        cache_write_other: sum.leg(
            l.write_other,
            selection.rate(5, rates.cache_write),
            multiplier,
        ),
        output: sum.leg(l.output, selection.rate(1, rates.output), multiplier),
        reasoning: sum.leg(l.reasoning, selection.rate(6, rates.reasoning), multiplier),
    };
    let source = match selection.card.origin {
        CardOrigin::Bundled => MoneySource::BundledReference,
        CardOrigin::User => MoneySource::UserOverride,
    };
    let cost = CostEstimate {
        usd: sum.complete.then_some(sum.known),
        class: if sum.complete {
            AggregationClass::Estimated
        } else {
            AggregationClass::Unknown
        },
        as_of: selection.as_of.clone(),
        note: (!sum.complete).then(|| {
            if sum.unreported {
                "a token counter was not reported".to_owned()
            } else {
                "a used token category has no price".to_owned()
            }
        }),
        known_subtotal: sum.known,
        breakdown,
        source: Some(source),
        reference: selection.reference,
    };
    (cost, Some(selection))
}
