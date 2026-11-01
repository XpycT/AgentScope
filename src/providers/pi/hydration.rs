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
//! Tool-result durations are never model time.
//!
//! Lifetime accounting and current context are separate: every uniquely owned call of
//! every executed branch stays in the ledger, while [`PiTree`] selects the prompt
//! snapshot of the recorded active path only. A copied fork prefix is excluded from
//! direct spend only with a resolved lineage and producer-preserved identities; when
//! that cannot be proven the call is not charged and coverage stays partial.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::Config;
use crate::domain::{
    CompactionObservation, CompactionScope, LocatorKind, ModelDetail, Provenance, RecordedMoney,
    RefreshBudget, RequestMetadata, RequestPurpose, SessionSummary, Timestamp, Warning,
    WarningKind,
};
use crate::ingest::source::{SourceItem, SourceReader};
use crate::metrics::{SpeedAccumulator, compaction_summary, session_timing};
use crate::pricing::{PriceSchedule, ReferencePolicy};
use crate::providers::pi::metadata::warning;
use crate::providers::pi::record::{
    PiEntry, PiEntryKind, PiRecordError, PiRole, additive_total, assistant_model,
    categories_incomplete, invalid_money, parse_entry, parse_header, parse_pi_usage,
    record_may_carry_usage, supported_version, total_conflicts,
};
use crate::providers::pi::tree::{PiInsert, PiOwnership, PiTree};
use crate::usage::{LedgerOutcome, UsageLedger, UsageUpdate};
use crate::{
    domain::{Harness, SourceKey, UsageObservation, UsageScope},
    providers::{HydrationDelta, ProviderError},
};

/// Requests whose full observation is retained for details/consumers. The ledger is
/// the accounting source; this is a bounded convenience view, never a second ledger.
const MAX_RETAINED_REQUESTS: usize = 4096;
/// Compaction markers retained for the summary/details view. A hostile marker flood
/// is bounded; an omitted marker makes the recorded history incomplete.
const MAX_RETAINED_COMPACTIONS: usize = 512;
/// Byte budget across retained markers.
const MAX_COMPACTION_BYTES: usize = 512 * 1024;
/// Inline structures and allocator bookkeeping reserved per retained marker.
const COMPACTION_RESERVE: usize = 512;

fn path_bytes(path: &Path) -> usize {
    path.as_os_str().len()
}

fn source_bytes(source: &SourceKey) -> usize {
    match source {
        SourceKey::File(path) => path_bytes(path),
        SourceKey::DbRow {
            database,
            table,
            id,
        } => path_bytes(database) + table.len() + id.len(),
    }
}

fn locator_kind_bytes(kind: &LocatorKind) -> usize {
    match kind {
        LocatorKind::FileOffset { path, .. } | LocatorKind::CompressedRecord { path, .. } => {
            path_bytes(path)
        }
        LocatorKind::DbRow { table, row_id, .. } => table.len() + row_id.len(),
    }
}

/// Heap bytes of a provenance: every source, locator and session string is cloned
/// per record, so a retained marker owns all of them.
fn provenance_bytes(provenance: &Provenance) -> usize {
    source_bytes(&provenance.source)
        + source_bytes(&provenance.locator.session.source)
        + provenance.locator.session.native_id.len()
        + locator_kind_bytes(&provenance.locator.kind)
        + provenance.record_type.len()
        + provenance.harness_version.as_deref().map_or(0, str::len)
}

fn model_bytes(model: &ModelDetail) -> usize {
    model.id.len()
        + model.provider.as_deref().map_or(0, str::len)
        + model.variant.as_deref().map_or(0, str::len)
        + model.effort.as_deref().map_or(0, str::len)
        + model.configured_effort.as_deref().map_or(0, str::len)
}

fn request_bytes(request: &RequestMetadata) -> usize {
    request.id.as_deref().map_or(0, str::len)
        + request.service_tier.as_deref().map_or(0, str::len)
        + request.inference_region.as_deref().map_or(0, str::len)
}

fn money_bytes(money: &RecordedMoney) -> usize {
    [&money.reported_api, &money.saved_estimate]
        .into_iter()
        .flatten()
        .map(|fact| fact.as_of.as_deref().map_or(0, str::len))
        .sum()
}

/// Owned strings of the nested usage observation. Its inline structure is already
/// inside `size_of::<CompactionObservation>()`, which `compaction_cost` charges, so
/// only the heap strings are added here.
fn observation_bytes(observation: &UsageObservation) -> usize {
    observation.model.as_ref().map_or(0, model_bytes)
        + observation.request.as_ref().map_or(0, request_bytes)
        + money_bytes(&observation.money)
        + provenance_bytes(&observation.provenance)
}

/// Charged cost of one retained marker: its own structures, every owned provenance
/// string, and the nested observation with its own cloned provenance.
fn compaction_cost(observation: &CompactionObservation) -> usize {
    COMPACTION_RESERVE
        + std::mem::size_of::<CompactionObservation>()
        + observation.id.len()
        + observation.method.as_deref().map_or(0, str::len)
        + observation.trigger.as_deref().map_or(0, str::len)
        + observation
            .summary
            .as_ref()
            .map_or(0, |summary| summary.preview.len())
        + provenance_bytes(&observation.provenance)
        + observation.usage.as_ref().map_or(0, observation_bytes)
}

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
    tree: PiTree,
    compactions: VecDeque<CompactionObservation>,
    compaction_bytes: usize,
    /// Markers the retention cap refused to keep.
    compactions_omitted: usize,
    /// A marker could have been unreadable, or the retention cap omitted one, so the
    /// observed compaction history is not complete.
    compactions_incomplete: bool,
    warnings: Vec<Warning>,
}

impl Fold {
    /// Retains one compaction marker within the count/byte cap, newest-last, noting
    /// that the recorded history is incomplete once a marker is dropped.
    fn retain_compaction(&mut self, observation: CompactionObservation) {
        if self
            .compactions
            .iter()
            .any(|known| known.id == observation.id)
        {
            // The same marker again is not another event.
            return;
        }
        let cost = compaction_cost(&observation);
        if cost > MAX_COMPACTION_BYTES {
            self.compactions_omitted = self.compactions_omitted.saturating_add(1);
            self.compactions_incomplete = true;
            return;
        }
        while !self.compactions.is_empty()
            && (self.compactions.len() >= MAX_RETAINED_COMPACTIONS
                || self.compaction_bytes.saturating_add(cost) > MAX_COMPACTION_BYTES)
        {
            if let Some(old) = self.compactions.pop_front() {
                self.compaction_bytes = self.compaction_bytes.saturating_sub(compaction_cost(&old));
                self.compactions_omitted = self.compactions_omitted.saturating_add(1);
                self.compactions_incomplete = true;
            }
        }
        self.compaction_bytes = self.compaction_bytes.saturating_add(cost);
        self.compactions.push_back(observation);
    }

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
    /// Collecting the resolved fork parent's call identities as ownership evidence.
    Proof,
    Entries,
    Done,
}

/// Bounded read of the discovered fork parent's usage identities.
struct Proof {
    reader: SourceReader,
    tree: PiTree,
    /// An unreadable record, an unfinished tail or an unverifiable header could have
    /// carried a model call this reader never saw.
    incomplete: bool,
    /// The header id the discovered predecessor must still carry.
    expected_id: String,
    /// The first record was examined as the header.
    header_seen: bool,
    /// A supported header with exactly that id was read.
    verified: bool,
}

struct Work {
    reader: SourceReader,
    fold: Fold,
    phase: Phase,
    unsupported: bool,
    proof: Option<Proof>,
}

impl Proof {
    fn open(
        path: &Path,
        expected_id: &str,
        max_record_bytes: usize,
        max_entries: usize,
        max_bytes: usize,
    ) -> Option<Self> {
        let reader = SourceReader::open(path, max_record_bytes).ok()?;
        Some(Self {
            reader,
            tree: PiTree::new(max_entries, max_bytes),
            incomplete: false,
            expected_id: expected_id.to_owned(),
            header_seen: false,
            verified: false,
        })
    }
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
    /// The discovered fork parent's file and native id, when the lineage resolved to
    /// one inside the allowed roots. An unresolved reference is never opened.
    fork_parent: Option<(PathBuf, String)>,
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
        let fork_parent = match meta.lineage.fork.as_ref() {
            Some(crate::domain::SessionLink::Resolved(key)) => match &key.source {
                SourceKey::File(parent) if key.harness == Harness::Pi => {
                    Some((parent.clone(), key.native_id.clone()))
                }
                _ => None,
            },
            _ => None,
        };
        Ok(Self {
            prices: config.pricing.clone(),
            policy: config.reference_policy(Harness::Pi),
            max_entries: config.dedup_ledger_entries,
            max_bytes: config.dedup_ledger_bytes,
            max_record_bytes: config.max_record_bytes,
            path,
            fork_parent,
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

    /// Ownership of a recorded entry's call, as the bounded tree proved it. Before
    /// any slice (or after cancellation) nothing is owned, so it is `Unknown`.
    pub fn ownership(&self, entry_id: &str) -> PiOwnership {
        self.work.as_ref().map_or(PiOwnership::Unknown, |work| {
            work.fold.tree.ownership(entry_id)
        })
    }

    /// Continues a completed fold after append-only growth: the source reader keeps
    /// its consumed position, so the next [`PiHydration::step`] reads the appended
    /// records once. A finished fold of an unsupported header stays finished.
    pub fn resume(&mut self) {
        if let Some(work) = &mut self.work
            && matches!(work.phase, Phase::Done)
            && !work.unsupported
        {
            work.phase = Phase::Entries;
        }
    }

    /// Refreshes the discovery metadata a fold reports (title, project, activity, model
    /// fallback) without touching accounting state or the source position. Identity and
    /// relationships must be unchanged; the scanner only rebases within one identity.
    pub fn rebase(&mut self, meta: crate::domain::SessionMetadata) {
        self.meta = meta;
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

    /// Compaction markers the bounded view currently retains (zero once cancelled).
    pub fn retained_compactions(&self) -> usize {
        self.work
            .as_ref()
            .map_or(0, |work| work.fold.compactions.len())
    }

    /// Bytes charged for those retained markers, including owned provenance paths.
    pub fn retained_compaction_bytes(&self) -> usize {
        self.work
            .as_ref()
            .map_or(0, |work| work.fold.compaction_bytes)
    }

    /// Markers the retention cap omitted from the bounded view.
    pub fn omitted_compactions(&self) -> usize {
        self.work
            .as_ref()
            .map_or(0, |work| work.fold.compactions_omitted)
    }

    fn fresh(&self) -> Result<Work, ProviderError> {
        let mut tree = PiTree::new(self.max_entries, self.max_bytes);
        tree.set_fork(self.meta.lineage.fork.as_ref());
        let mut fold = Fold {
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
            tree,
            compactions: VecDeque::new(),
            compaction_bytes: 0,
            compactions_omitted: 0,
            compactions_incomplete: false,
            warnings: Vec::new(),
        };
        let reader = SourceReader::open(&self.path, self.max_record_bytes).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ProviderError::Unavailable("session source is unavailable".into())
            } else {
                ProviderError::Io(error.to_string())
            }
        })?;
        // The fork parent's identities must be known before any of this file's
        // requests are attributed, so that phase runs first and never shares a slice
        // with a partially-folded entry batch.
        let (phase, proof) = match &self.fork_parent {
            Some((parent, id)) => match Proof::open(
                parent,
                id,
                self.max_record_bytes,
                self.max_entries,
                self.max_bytes,
            ) {
                Some(proof) => (Phase::Proof, Some(proof)),
                None => {
                    fold.warnings.push(warning(
                        &self.path,
                        "fork",
                        WarningKind::Unavailable,
                        "the linked fork parent could not be read; copied requests stay \
                         unattributed and coverage is partial",
                    ));
                    (Phase::Header, None)
                }
            },
            None => (Phase::Header, None),
        };
        Ok(Work {
            reader,
            fold,
            phase,
            unsupported: false,
            proof,
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
        if matches!(work.phase, Phase::Proof) {
            return self.proof(work, budget);
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
                        work.fold.compactions_incomplete = true;
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
                    work.fold.compactions_incomplete = true;
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
                    work.fold.compactions_incomplete = true;
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
                    work.fold.compactions_incomplete = true;
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

    /// Collects the resolved fork parent's call identities. Bounded and resumable:
    /// the parent is only ever a source discovery already reported inside the allowed
    /// roots, never an arbitrary path from the header.
    fn proof(&mut self, work: &mut Work, budget: RefreshBudget) -> Result<Advance, ProviderError> {
        let Some(mut proof) = work.proof.take() else {
            work.phase = Phase::Header;
            return Ok(Advance::Pending);
        };
        let batch = match proof.reader.step(budget) {
            Ok(batch) => batch,
            Err(_) => {
                work.fold.tree.mark_inherited_incomplete();
                work.fold.warn(
                    "fork",
                    WarningKind::Io,
                    "the linked fork parent became unreadable; copied requests stay \
                     unattributed and coverage is partial",
                );
                work.phase = Phase::Header;
                return Ok(Advance::Pending);
            }
        };
        if batch.invalidated {
            work.proof = self.fork_parent.as_ref().and_then(|(parent, id)| {
                Proof::open(
                    parent,
                    id,
                    self.max_record_bytes,
                    self.max_entries,
                    self.max_bytes,
                )
            });
            work.fold.tree.mark_inherited_incomplete();
            return Ok(Advance::Pending);
        }
        for item in &batch.records {
            match &item.item {
                SourceItem::Json(value) => {
                    if !proof.header_seen {
                        proof.header_seen = true;
                        match parse_header(value) {
                            Some(header) => {
                                // The predecessor must still be the source discovery
                                // resolved: a truncated/replaced file is not evidence.
                                if header.id == proof.expected_id
                                    && supported_version(header.version)
                                {
                                    proof.verified = true;
                                } else {
                                    proof.incomplete = true;
                                }
                                continue;
                            }
                            None => proof.incomplete = true,
                        }
                    }
                    match parse_entry(value, item.ordinal, 0) {
                        Ok(entry) => {
                            if matches!(proof.tree.insert(&entry), PiInsert::Limited) {
                                proof.incomplete = true;
                            }
                        }
                        Err(PiRecordError::Unsupported) => {}
                        Err(PiRecordError::Malformed) => proof.incomplete = true,
                    }
                }
                SourceItem::Invalid
                | SourceItem::TooLarge
                | SourceItem::Damaged(_)
                | SourceItem::Skipping => proof.incomplete = true,
                // A torn trailing line could hide a request the fork copied.
                SourceItem::Incomplete => proof.incomplete = true,
            }
        }
        if batch.done {
            if !proof.verified {
                work.fold.tree.mark_inherited_incomplete();
                work.fold.warn(
                    "fork",
                    WarningKind::Unsupported,
                    "the linked fork parent no longer carries its recorded session \
                     header, so copied requests stay unattributed and coverage is partial",
                );
            } else if !work.fold.tree.adopt(&proof.tree) || proof.incomplete {
                work.fold.tree.mark_inherited_incomplete();
                work.fold.warn(
                    "fork",
                    WarningKind::Parse,
                    "the linked fork parent's usage could not be read completely; \
                     copied requests stay partial",
                );
            }
            work.phase = Phase::Header;
        } else {
            work.proof = Some(proof);
        }
        Ok(Advance::Pending)
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
        // The tree keeps only structure, call identities and prompt totals. An entry
        // the bounded index could not hold stays unattributable rather than owned.
        work.fold.tree.insert(entry);
        let ownership = work.fold.tree.ownership(&entry.id);
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
                        let observation = parse_pi_usage(&value, model, provenance);
                        self.count(
                            work,
                            observation,
                            &value,
                            &entry.id,
                            RequestPurpose::Conversation,
                            ownership,
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
                let observation = parse_pi_usage(value, Some(detail), provenance);
                self.count(
                    work,
                    observation,
                    value,
                    &entry.id,
                    RequestPurpose::Auxiliary(kind.clone()),
                    ownership,
                );
            }
            PiEntryKind::Compaction { usage, .. } | PiEntryKind::BranchSummary { usage, .. } => {
                // A summary entry persists no provider/model route, and Pi resolves
                // summarization auth independently of the conversation model. Its
                // categories/money are kept, but route-dependent pricing stays Unknown
                // instead of charging the preceding conversation model.
                let observed = if *usage {
                    let observation = parse_pi_usage(value, None, provenance.clone());
                    self.count(
                        work,
                        observation,
                        value,
                        &entry.id,
                        RequestPurpose::Compaction,
                        ownership,
                    )
                } else {
                    None
                };
                // A copied marker is still shown as an observed event, but its cost
                // belongs to the linked predecessor and is never charged here.
                work.fold.retain_compaction(CompactionObservation {
                    id: entry.id.clone(),
                    timestamp: entry.timestamp,
                    method: None,
                    trigger: None,
                    summary: None,
                    before_tokens: match &entry.kind {
                        PiEntryKind::Compaction { tokens_before, .. } => *tokens_before,
                        _ => None,
                    },
                    after_tokens: None,
                    duration_ms: None,
                    usage: observed,
                    scope: CompactionScope::Event,
                    provenance,
                });
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

    /// Normalizes one recorded call and counts it only when this file owns it.
    /// `None` means the call is a linked predecessor's spend or cannot be told.
    fn count(
        &self,
        work: &mut Work,
        mut observation: UsageObservation,
        value: &Value,
        entry_id: &str,
        purpose: RequestPurpose,
        ownership: PiOwnership,
    ) -> Option<UsageObservation> {
        observation.scope = UsageScope::Message;
        if let Some(request) = &mut observation.request {
            request.purpose = purpose.clone();
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
        match ownership {
            // A copy of a linked source's call is that source's spend.
            PiOwnership::Inherited => None,
            PiOwnership::Unknown => {
                work.fold
                    .ledger
                    .mark_partial("ownership of a copied Pi request could not be proven");
                work.fold.warn(
                    "ownership",
                    WarningKind::Parse,
                    "a Pi request could not be attributed to this or a linked session, \
                     so it is not charged and coverage is partial",
                );
                None
            }
            PiOwnership::Owned => {
                let duration = observation
                    .request
                    .as_ref()
                    .and_then(|request| request.duration_ms);
                match work.fold.ledger.observe(
                    UsageUpdate::Initial(&observation),
                    &self.prices,
                    self.policy,
                ) {
                    LedgerOutcome::Counted => {
                        // Throughput is conversation output only: a summary or an
                        // auxiliary model call is billed but is not the timed stream.
                        if purpose == RequestPurpose::Conversation {
                            work.fold.speed.observe(&observation);
                        }
                        if let Some(duration) = duration {
                            work.fold.model_ms += duration;
                        }
                        if work.fold.requests.len() < MAX_RETAINED_REQUESTS {
                            work.fold.requests.push(observation.clone());
                        }
                        Some(observation)
                    }
                    LedgerOutcome::Duplicate => None,
                    LedgerOutcome::Revised | LedgerOutcome::Uncountable => {
                        work.fold.warn(
                            "usage",
                            WarningKind::Parse,
                            "a Pi request could not be attributed and is not counted",
                        );
                        None
                    }
                }
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
                // Occupancy follows the recorded active path, never lifetime tokens.
                context: fold.tree.fold_context(&self.prices),
                compactions: compaction_summary(
                    &fold.compactions,
                    !fold.compactions_incomplete,
                    &self.prices,
                    self.policy,
                ),
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
