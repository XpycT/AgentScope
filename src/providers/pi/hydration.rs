//! Bounded, resumable lifetime facts of one native Pi session file.
//!
//! The fold counts every recorded model call once: an assistant message's `usage`
//! is one conversation request (deduplicated by its confirmed `responseId` when
//! recorded, else by its native entry id), a `usage` entry is an auxiliary request,
//! and a compaction/branch-summary usage is a compaction-purpose request.
//! Pi is additive: cache counters sit beside `input`, `output` already includes
//! `reasoning`, and `totalTokens` is only a cross-check, so a disagreement marks the
//! coverage partial instead of redefining what the categories include.
//!
//! No timing is invented. A recorded assistant `durationMs` is measured whole-request
//! model time and contributes to state speed; request completion, session end and
//! active intervals remain Unknown because standard Pi records do not persist them.
//! Tool-result durations are never model time. Context/tree ownership belongs to the
//! next stage; this fold keeps per-request facts and the ledger only.

use std::path::PathBuf;

use serde_json::Value;

use crate::config::Config;
use crate::domain::{
    ModelDetail, Provenance, RefreshBudget, RequestPurpose, SessionSummary, Timestamp, Warning,
    WarningKind,
};
use crate::ingest::source::{SourceItem, SourceReader};
use crate::metrics::{SpeedAccumulator, session_timing};
use crate::pricing::{PriceSchedule, ReferencePolicy};
use crate::providers::pi::metadata::warning;
use crate::providers::pi::record::{
    PiEntry, PiEntryKind, PiRole, additive_total, categories_incomplete, invalid_money,
    parse_entry, parse_header, parse_pi_usage, record_may_carry_usage, supported_version,
    total_conflicts,
};
use crate::usage::{LedgerOutcome, UsageLedger, UsageUpdate};
use crate::{
    domain::{Harness, SourceKey, UsageObservation, UsageScope},
    providers::{HydrationDelta, ProviderError},
};

/// Requests whose full observation is retained for details/consumers. The ledger is
/// the accounting source; this is a bounded convenience view, never a second ledger.
const MAX_RETAINED_REQUESTS: usize = 4096;

struct Fold {
    path: PathBuf,
    ledger: UsageLedger,
    speed: SpeedAccumulator,
    requests: Vec<UsageObservation>,
    model: Option<ModelDetail>,
    /// Current Pi thinking level (`thinking_level_change`).
    level: Option<String>,
    model_ms: f64,
    last_event_at: Option<Timestamp>,
    displayed: usize,
    hidden: usize,
    warnings: Vec<Warning>,
}

impl Fold {
    fn warn(&mut self, label: &str, kind: WarningKind, message: &str) {
        let warning = warning(&self.path, label, kind, message);
        match self
            .warnings
            .iter_mut()
            .find(|known| known.key == warning.key)
        {
            Some(known) => known.count = known.count.saturating_add(1),
            None => self.warnings.push(warning),
        }
    }
}

enum Phase {
    Header,
    Entries,
    Done,
}

struct Work {
    reader: SourceReader,
    fold: Fold,
    phase: Phase,
    unsupported: bool,
}

/// Resumable lifetime facts of one Pi session.
pub struct PiHydration {
    meta: crate::domain::SessionMetadata,
    path: PathBuf,
    prices: PriceSchedule,
    policy: ReferencePolicy,
    max_entries: usize,
    max_bytes: usize,
    max_record_bytes: usize,
    work: Option<Work>,
}

impl PiHydration {
    /// Cheap: no file is opened until the first [`PiHydration::step`].
    pub fn new(
        meta: crate::domain::SessionMetadata,
        config: &Config,
    ) -> Result<Self, ProviderError> {
        if meta.key.harness != Harness::Pi {
            return Err(ProviderError::Unsupported("not a Pi session".into()));
        }
        let path = match &meta.key.source {
            SourceKey::File(path) => path.clone(),
            SourceKey::DbRow { .. } => {
                return Err(ProviderError::Unsupported(
                    "a Pi session is a file source".into(),
                ));
            }
        };
        Ok(Self {
            prices: config.pricing.clone(),
            policy: config.reference_policy(Harness::Pi),
            max_entries: config.dedup_ledger_entries,
            max_bytes: config.dedup_ledger_bytes,
            max_record_bytes: config.max_record_bytes,
            path,
            meta,
            work: None,
        })
    }

    /// Drops the bounded ledger, request view and reader. A later step starts over.
    pub fn cancel(&mut self) {
        self.work = None;
    }

    /// The metadata this hydration was created for.
    pub fn metadata(&self) -> &crate::domain::SessionMetadata {
        &self.meta
    }

    /// Requests the ledger currently holds (zero once cancelled).
    pub fn retained_requests(&self) -> usize {
        self.work
            .as_ref()
            .map_or(0, |work| work.fold.ledger.observed_requests())
    }

    /// Bounded retained observations of counted requests (empty once cancelled).
    pub fn requests(&self) -> &[UsageObservation] {
        self.work
            .as_ref()
            .map_or(&[], |work| work.fold.requests.as_slice())
    }

    fn fresh(&self) -> Result<Work, ProviderError> {
        let fold = Fold {
            path: self.path.clone(),
            ledger: UsageLedger::new(self.max_entries, self.max_bytes),
            speed: SpeedAccumulator::new(self.max_entries),
            requests: Vec::new(),
            model: None,
            level: None,
            model_ms: 0.0,
            last_event_at: None,
            displayed: 0,
            hidden: 0,
            warnings: Vec::new(),
        };
        let reader = SourceReader::open(&self.path, self.max_record_bytes).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Unavailable("session source is unavailable".into())
            } else {
                ProviderError::Io(error.to_string())
            }
        })?;
        Ok(Work {
            reader,
            fold,
            phase: Phase::Header,
            unsupported: false,
        })
    }

    fn unavailable(&self, error: &ProviderError) -> HydrationDelta {
        let mut summary = SessionSummary::unhydrated(self.meta.clone());
        summary.warnings.push(warning(
            &self.path,
            "source",
            WarningKind::Unavailable,
            &format!("source unavailable: {error}"),
        ));
        HydrationDelta {
            summary,
            done: true,
        }
    }

    /// One budgeted slice. While `done` is false the summary holds known metadata
    /// only; once done it carries the folded facts.
    pub fn step(&mut self, budget: RefreshBudget) -> Result<HydrationDelta, ProviderError> {
        let mut work = match self.work.take() {
            Some(work) => work,
            None => match self.fresh() {
                Ok(work) => work,
                Err(error) => return Ok(self.unavailable(&error)),
            },
        };
        let outcome = self.advance(&mut work, budget);
        match outcome {
            Ok(Advance::Restart) => {
                self.work = None;
                Ok(HydrationDelta {
                    summary: SessionSummary::unhydrated(self.meta.clone()),
                    done: false,
                })
            }
            Ok(Advance::Pending) => {
                self.work = Some(work);
                Ok(HydrationDelta {
                    summary: SessionSummary::unhydrated(self.meta.clone()),
                    done: false,
                })
            }
            Ok(Advance::Done) => {
                let summary = self.summary(&work);
                self.work = Some(work);
                Ok(HydrationDelta {
                    summary,
                    done: true,
                })
            }
            Err(error) => Err(error),
        }
    }

    fn advance(
        &mut self,
        work: &mut Work,
        budget: RefreshBudget,
    ) -> Result<Advance, ProviderError> {
        if matches!(work.phase, Phase::Done) {
            return Ok(Advance::Done);
        }
        let batch = work.reader.step(budget).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Unavailable("session source is unavailable".into())
            } else {
                ProviderError::Io(error.to_string())
            }
        })?;
        if batch.invalidated {
            return Ok(Advance::Restart);
        }
        for item in &batch.records {
            let offset = match &item.locator {
                crate::domain::LocatorKind::FileOffset { offset, .. } => *offset,
                _ => item.ordinal,
            };
            match &item.item {
                SourceItem::Json(value) => {
                    if matches!(work.phase, Phase::Header) {
                        if !self.header(work, value)? {
                            return Ok(Advance::Restart);
                        }
                        if work.unsupported {
                            work.phase = Phase::Done;
                            return Ok(Advance::Done);
                        }
                        continue;
                    }
                    let Ok(entry) = parse_entry(value, item.ordinal, offset) else {
                        work.fold.hidden += 1;
                        if record_may_carry_usage(value) {
                            // Dropping an unreadable model call must never leave a
                            // complete-looking total or cost.
                            work.fold
                                .ledger
                                .mark_partial("a usage-bearing Pi record could not be interpreted");
                            work.fold.warn(
                                "usage",
                                WarningKind::Parse,
                                "a usage-bearing Pi record could not be interpreted; \
                                 accounting coverage is partial",
                            );
                        }
                        work.fold.warn(
                            "record",
                            WarningKind::Parse,
                            "a Pi record could not be interpreted and was skipped",
                        );
                        continue;
                    };
                    let provenance = Provenance {
                        harness: Harness::Pi,
                        source: self.meta.key.source.clone(),
                        locator: item.source_locator(self.meta.key.clone()),
                        record_type: value
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .chars()
                            .take(64)
                            .collect(),
                        harness_version: self.meta.harness_version.clone(),
                    };
                    self.entry(work, &entry, value, provenance);
                }
                SourceItem::Invalid | SourceItem::TooLarge => {
                    work.fold.hidden += 1;
                    work.fold
                        .ledger
                        .mark_partial("an unreadable Pi record could contain usage");
                    work.fold.warn(
                        "record",
                        WarningKind::Parse,
                        "a Pi record was unreadable and was skipped; \
                         accounting coverage is partial",
                    );
                }
                SourceItem::Damaged(_) => {
                    work.fold
                        .ledger
                        .mark_partial("a damaged Pi source may hide usage");
                    work.fold.warn(
                        "source",
                        WarningKind::Io,
                        "the Pi source could not be decoded further; \
                         accounting coverage is partial",
                    );
                }
                SourceItem::Skipping => {
                    work.fold
                        .ledger
                        .mark_partial("an oversized Pi record could contain usage");
                }
                SourceItem::Incomplete => {}
            }
        }
        if batch.done {
            work.phase = Phase::Done;
            Ok(Advance::Done)
        } else {
            Ok(Advance::Pending)
        }
    }

    /// Interprets the header. `false` means the path now holds another session.
    fn header(&self, work: &mut Work, value: &Value) -> Result<bool, ProviderError> {
        let Some(header) = parse_header(value) else {
            work.unsupported = true;
            work.fold.warn(
                "header",
                WarningKind::Parse,
                "no interpretable Pi session header; the file is left partial",
            );
            return Ok(true);
        };
        if header.id != self.meta.key.native_id {
            return Ok(false);
        }
        work.phase = Phase::Entries;
        if !supported_version(header.version) {
            work.unsupported = true;
            work.fold.warn(
                "version",
                WarningKind::Unsupported,
                "unsupported Pi session version: only the v2/v3 tree format is interpreted",
            );
        }
        Ok(true)
    }

    fn entry(&mut self, work: &mut Work, entry: &PiEntry, value: &Value, provenance: Provenance) {
        if let Some(at) = entry.timestamp {
            work.fold.last_event_at = work.fold.last_event_at.max(Some(at));
        }
        match &entry.kind {
            PiEntryKind::Message { role, usage, .. } => {
                work.fold.displayed += 1;
                match role {
                    PiRole::Assistant => {
                        let message = value
                            .get("message")
                            .and_then(Value::as_object)
                            .cloned()
                            .unwrap_or_default();
                        let model = assistant_model(&message, work.fold.level.as_deref());
                        if model.is_some() {
                            work.fold.model = model.clone();
                        }
                        if !*usage {
                            work.fold
                                .ledger
                                .mark_partial("an assistant message has no recorded usage");
                            work.fold.warn(
                                "usage",
                                WarningKind::Parse,
                                "an assistant message has no recorded usage",
                            );
                            return;
                        }
                        let value = Value::Object(message);
                        let mut observation = parse_pi_usage(&value, model, provenance);
                        self.observe(
                            work,
                            &mut observation,
                            &value,
                            &entry.id,
                            RequestPurpose::Conversation,
                        );
                    }
                    // Tool-result usage is tool execution accounting, and its
                    // duration is tool time, never model or session time.
                    PiRole::ToolResult => {}
                    PiRole::System | PiRole::User | PiRole::Custom | PiRole::Unknown => {}
                }
            }
            PiEntryKind::ModelChange { provider, model_id } => {
                work.fold.model = Some(ModelDetail {
                    id: model_id.clone(),
                    provider: Some(provider.clone()),
                    variant: None,
                    effort: None,
                    configured_effort: work.fold.level.clone(),
                });
            }
            PiEntryKind::ThinkingLevelChange { level } => {
                work.fold.level = Some(level.clone());
                if let Some(model) = &mut work.fold.model {
                    model.configured_effort = Some(level.clone());
                }
            }
            PiEntryKind::Usage {
                kind,
                provider,
                model,
            } => {
                let detail = ModelDetail {
                    id: model.clone(),
                    provider: Some(provider.clone()),
                    variant: None,
                    effort: None,
                    configured_effort: work.fold.level.clone(),
                };
                work.fold.model = Some(detail.clone());
                let mut observation = parse_pi_usage(value, Some(detail), provenance);
                self.observe(
                    work,
                    &mut observation,
                    value,
                    &entry.id,
                    RequestPurpose::Auxiliary(kind.clone()),
                );
            }
            PiEntryKind::Compaction { usage, .. } | PiEntryKind::BranchSummary { usage, .. } => {
                if *usage {
                    // A summary entry persists no provider/model route, and Pi resolves
                    // summarization auth independently of the conversation model. Keep
                    // its recorded categories/money, but leave route-dependent pricing
                    // Unknown instead of charging the preceding conversation model.
                    let mut observation = parse_pi_usage(value, None, provenance);
                    self.observe(
                        work,
                        &mut observation,
                        value,
                        &entry.id,
                        RequestPurpose::Compaction,
                    );
                }
            }
            PiEntryKind::SessionInfo { .. }
            | PiEntryKind::ContextEdit { .. }
            | PiEntryKind::Custom { .. } => {}
            PiEntryKind::Unknown { .. } => {
                work.fold.hidden += 1;
                if record_may_carry_usage(value) {
                    work.fold
                        .ledger
                        .mark_partial("an unsupported Pi record could contain usage");
                    work.fold.warn(
                        "usage",
                        WarningKind::Unsupported,
                        "an unsupported Pi record could contain usage; \
                         accounting coverage is partial",
                    );
                }
            }
        }
    }

    fn observe(
        &self,
        work: &mut Work,
        observation: &mut UsageObservation,
        value: &Value,
        entry_id: &str,
        purpose: RequestPurpose,
    ) {
        observation.scope = UsageScope::Message;
        if let Some(request) = &mut observation.request {
            request.purpose = purpose;
            // A confirmed provider response id wins; otherwise the native entry id
            // is the record's ownership identity, so a literal duplicate counts once.
            request.id = request.id.take().or_else(|| Some(entry_id.to_owned()));
        }
        if categories_incomplete(&observation.categories) {
            work.fold
                .ledger
                .mark_partial("a recorded usage counter was missing or invalid");
            work.fold.warn(
                "usage",
                WarningKind::Parse,
                "a recorded usage record is missing a required token counter",
            );
        }
        if total_conflicts(value, &observation.categories) {
            work.fold
                .ledger
                .mark_partial("a recorded total disagrees with the additive token categories");
            work.fold.warn(
                "total",
                WarningKind::Parse,
                "a recorded total disagrees with the additive token categories; \
                 the additive counters are kept and coverage is partial",
            );
        }
        if invalid_money(value) {
            work.fold.warn(
                "cost",
                WarningKind::Parse,
                "a recorded cost was unusable and is not retained",
            );
        }
        let duration = observation
            .request
            .as_ref()
            .and_then(|request| request.duration_ms);
        match work
            .fold
            .ledger
            .observe(UsageUpdate::Initial(observation), &self.prices, self.policy)
        {
            LedgerOutcome::Counted => {
                work.fold.speed.observe(observation);
                if let Some(duration) = duration {
                    work.fold.model_ms += duration;
                }
                if work.fold.requests.len() < MAX_RETAINED_REQUESTS {
                    work.fold.requests.push(observation.clone());
                }
            }
            LedgerOutcome::Duplicate => {}
            LedgerOutcome::Revised | LedgerOutcome::Uncountable => {
                work.fold.warn(
                    "usage",
                    WarningKind::Parse,
                    "a Pi request could not be attributed and is not counted",
                );
            }
        }
    }

    fn summary(&self, work: &Work) -> SessionSummary {
        let fold = &work.fold;
        if work.unsupported {
            let mut summary = SessionSummary::unhydrated(self.meta.clone());
            summary.warnings = fold.warnings.clone();
            summary.hidden_records = fold.hidden;
            return summary;
        }
        let mut meta = self.meta.clone();
        if fold.model.is_some() {
            meta.model = fold.model.clone();
        }
        let mut usage = fold.ledger.snapshot();
        usage.total = additive_total(&usage.categories);
        for model in &mut usage.models {
            model.total = additive_total(&model.categories);
        }
        let model_ms = (fold.model_ms > 0.0).then_some(fold.model_ms);
        SessionSummary {
            meta,
            usage: Some(usage),
            metrics: crate::domain::SessionMetrics {
                // Pi standard records persist no turn or session end: only recorded
                // request durations are measured.
                timing: session_timing(self.meta.created_at, None, &[], false, model_ms),
                speed: fold.speed.snapshot(),
                ..crate::domain::SessionMetrics::default()
            },
            lifecycle: crate::domain::Lifecycle::Unknown,
            unfinished_work: false,
            last_event_at: fold.last_event_at,
            last_completion: None,
            displayed_event_count: fold.displayed,
            hidden_records: fold.hidden,
            server_tools: None,
            process: crate::domain::ProcessAssociation::NotChecked,
            warnings: fold.warnings.clone(),
        }
    }
}

enum Advance {
    Pending,
    Done,
    Restart,
}

/// The model identity an assistant message proves. `responseModel` refines the
/// requested `model`; `providerThinkingLevel` is the only effective effort Pi
/// records, and `thinkingLevel`/the branch setting is the configured level.
fn assistant_model(
    message: &serde_json::Map<String, Value>,
    level: Option<&str>,
) -> Option<ModelDetail> {
    let id = message
        .get("responseModel")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .or_else(|| message.get("model").and_then(Value::as_str))
        .filter(|model| !model.is_empty() && model.len() <= 256)?;
    Some(ModelDetail {
        id: id.to_owned(),
        provider: message
            .get("provider")
            .and_then(Value::as_str)
            .filter(|provider| !provider.is_empty() && provider.len() <= 256)
            .map(str::to_owned),
        variant: None,
        effort: message
            .get("providerThinkingLevel")
            .and_then(Value::as_str)
            .filter(|effort| !effort.is_empty() && effort.len() <= 256)
            .map(str::to_owned),
        configured_effort: message
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .filter(|level| !level.is_empty() && level.len() <= 256)
            .map(str::to_owned)
            .or_else(|| level.map(str::to_owned)),
    })
}
