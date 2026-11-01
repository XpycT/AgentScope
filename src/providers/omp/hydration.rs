//! Resumable facts fold of one OMP session (or one continuation chain).
//!
//! Lifetime accounting counts every uniquely *owned* model call across all executed
//! branches (assistant messages and `model_usage` entries), no matter which branch is
//! active; the active branch only decides the *current* facts (model, effort, prompt
//! occupancy). Copies of a fork parent's or earlier segment's entries are inherited,
//! not new spend, and a call whose ownership cannot be proven leaves the direct usage
//! partial (see [`crate::providers::omp::ownership`]). Child sessions are separate
//! files with their own direct cost; what a parent's `task` result quotes about them
//! is kept only as private evidence and never enters the parent's ledger.
//!
//! The fold is bounded (ledger, entry index, evidence) and resumable: [`OmpHydration::step`]
//! does one budgeted slice. A fork first needs a discovery pass (to find its parent's
//! chain) and a proof scan of the parent's usage entries; plain sessions go straight
//! to the fold. [`OmpHydration::cancel`] drops all of that mutable work.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use serde_json::Value;

use crate::config::Config;
use crate::domain::{
    AggregationClass, CompactionObservation, CompactionScope, ContextUsage, Harness, Lifecycle,
    ModelDetail, Provenance, RefreshBudget, ServerToolCounts, SessionKey, SessionMetadata,
    SessionMetrics, SessionSummary, SourceKey, SourceLocator, Timestamp, Warning, WarningKind,
};
use crate::ingest::source::{SourceItem, SourceReader, SourceRecordItem};
use crate::metrics::{
    SpeedAccumulator, compaction_summary, context_usage, current_context, session_timing,
};
use crate::pricing::{PriceSchedule, ReferencePolicy};
use crate::providers::omp::OmpCatalog;
use crate::providers::omp::metadata::warning;
use crate::providers::omp::ownership::{OmpOwnership, owned_request};
use crate::providers::omp::tree::{OmpInsert, OmpTree, parse_entry_header};
use crate::providers::omp::usage::{
    call_identity, counter, entry_time, epoch_ms, is_assistant, map_omp_usage, server_tools, text,
};
use crate::providers::{HydrationDelta, ProviderError};
use crate::usage::{LedgerOutcome, UsageLedger, UsageUpdate};

/// Compaction markers kept per session; more mark the history incomplete.
const MAX_COMPACTIONS: usize = 256;
/// Quoted child results kept as private evidence.
const MAX_CLAIMS: usize = 4096;

/// What a parent's `task` result quoted about a child's spend. Private accounting
/// evidence only: a quoted amount is never added to the parent's usage, and the child
/// session's own file is the only source of the child's direct cost.
#[derive(Clone, PartialEq)]
pub struct ChildClaim {
    /// The child's agent ID as recorded in the task result.
    pub agent_id: String,
    pub quoted_usd: Option<f64>,
}

// The agent ID is untrusted text from a record: only its presence is printed.
impl std::fmt::Debug for ChildClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChildClaim")
            .field("quoted_usd", &self.quoted_usd)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct ModelRef {
    provider: Option<String>,
    id: String,
}

#[derive(Clone, PartialEq)]
struct ContextSnap {
    used: u64,
    class: AggregationClass,
    at: Option<Timestamp>,
    epoch: u32,
}

/// Facts that follow one branch: the state after an entry is its parent's state
/// updated by the entry.
#[derive(Clone, Default, PartialEq)]
struct PathState {
    model: Option<Arc<ModelRef>>,
    effort: Option<Arc<str>>,
    configured: Option<Arc<str>>,
    /// Compaction/reset boundaries on this branch.
    epoch: u32,
    context: Option<ContextSnap>,
}

fn model_ref(provider: Option<String>, id: String) -> Arc<ModelRef> {
    Arc::new(ModelRef { provider, id })
}

/// `provider/model` as recorded by `model_change` and `session_init`.
fn split_selector(selector: &str) -> Option<Arc<ModelRef>> {
    let clean = text(Some(&Value::String(selector.to_owned())))?;
    Some(match clean.split_once('/') {
        Some((provider, id)) if !provider.is_empty() && !id.is_empty() => {
            model_ref(Some(provider.to_owned()), id.to_owned())
        }
        _ => model_ref(None, clean),
    })
}

/// Prompt occupancy of an assistant message: the authoritative snapshot minus the
/// estimated rewrite removal, else an explicit `usage.contextTokens`, else the
/// additive prompt categories. Never `totalTokens`, never output, never orchestration.
fn context_of(message: &serde_json::Map<String, Value>) -> Option<(u64, AggregationClass)> {
    if let Some(snapshot) = message.get("contextSnapshot").and_then(Value::as_object)
        && let Some(prompt) = counter(snapshot, "promptTokens").filter(|p| *p > 0)
    {
        let removed = counter(snapshot, "historyRewriteTokensRemoved").unwrap_or(0);
        if let Some(used) = prompt.checked_sub(removed).filter(|u| *u > 0) {
            // The removal is OMP's own estimate, so the remainder is not purely reported.
            let class = if removed > 0 {
                AggregationClass::Estimated
            } else {
                AggregationClass::Reported
            };
            return Some((used, class));
        }
    }
    let usage = message.get("usage")?.as_object()?;
    if let Some(used) = counter(usage, "contextTokens").filter(|u| *u > 0) {
        return Some((used, AggregationClass::Reported));
    }
    let prompt = counter(usage, "input")?
        .checked_add(counter(usage, "cacheRead")?)?
        .checked_add(counter(usage, "cacheWrite")?)?;
    (prompt > 0).then_some((prompt, AggregationClass::Derived))
}

fn next_state(parent: &Arc<PathState>, kind: &str, entry: &Value) -> Option<Arc<PathState>> {
    let mut state = (**parent).clone();
    match kind {
        "message" => {
            let message = entry.get("message")?.as_object()?;
            if message.get("role")?.as_str()? != "assistant" {
                return None;
            }
            if let Some(id) = text(message.get("model")) {
                state.model = Some(model_ref(text(message.get("provider")), id));
            }
            if let Some((used, class)) = context_of(message) {
                state.context = Some(ContextSnap {
                    used,
                    class,
                    at: epoch_ms(message.get("completedAt"))
                        .or_else(|| epoch_ms(message.get("timestamp")))
                        .or_else(|| entry_time(entry)),
                    epoch: state.epoch,
                });
            }
        }
        "model_change" => {
            // Other roles configure side models, not the conversation's.
            if !matches!(
                entry.get("role").and_then(Value::as_str),
                None | Some("default" | "fallback" | "prewalk")
            ) {
                return None;
            }
            state.model = split_selector(entry.get("model")?.as_str()?);
        }
        "thinking_level_change" => {
            let level = text(entry.get("thinkingLevel")).map(Arc::<str>::from);
            // Entries from before auto mode recorded no selector: the level stands in.
            state.configured = text(entry.get("configured"))
                .map(Arc::<str>::from)
                .or_else(|| level.clone());
            state.effort = level;
        }
        // A rewrite of the history makes the earlier snapshot describe another prompt.
        "compaction" | "reset_boundary" => state.epoch = state.epoch.saturating_add(1),
        "session_init" => {
            if state.model.is_none() {
                state.model = split_selector(entry.get("resolvedModel")?.as_str()?);
            }
        }
        _ => return None,
    }
    Some(Arc::new(state))
}

/// The current facts of one branch.
#[derive(Clone)]
struct Current {
    model: Option<ModelDetail>,
    context: ContextUsage,
}

fn current_of(state: &PathState) -> Current {
    let model = state.model.as_ref().map(|m| ModelDetail {
        id: m.id.clone(),
        provider: m.provider.clone(),
        variant: None,
        effort: state.effort.as_deref().map(str::to_owned),
        configured_effort: state.configured.as_deref().map(str::to_owned),
    });
    let context = match &state.context {
        Some(snap) => {
            let measured = context_usage(
                Some(snap.used),
                None,
                snap.class,
                snap.at,
                model.clone(),
                Some(format!("c{}", snap.epoch)),
            );
            current_context(&measured, Some(&format!("c{}", state.epoch)))
        }
        None => ContextUsage::default(),
    };
    Current { model, context }
}

/// One source being folded: its entry index, per-entry branch state and reader.
struct FoldScan {
    key: SessionKey,
    reader: SourceReader,
    tree: OmpTree,
    states: HashMap<String, Arc<PathState>>,
    root: Arc<PathState>,
    /// Evidence for this source overflowed, so it cannot prove anything.
    evidence_full: bool,
    /// Length and mtime taken before the first read of this source.
    stamp: (PathBuf, u64, Option<SystemTime>),
}

impl FoldScan {
    /// The current facts of the active branch, or why they are unknown.
    fn current(&self) -> Result<Option<Current>, crate::providers::omp::OmpTreeError> {
        Ok(match self.tree.active_leaf_id()? {
            Some(id) => self.states.get(id).map(|s| current_of(s)),
            None => None,
        })
    }
}

struct ProofScan {
    key: SessionKey,
    reader: SourceReader,
    /// An overflow or unreadable record: the evidence stays incomplete.
    incomplete: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Find the fork parent's chain among discovered sources.
    Discover,
    /// Collect the parent chain's usage entries as ownership evidence.
    Proof,
    Fold,
    Done,
}

/// Lifetime accumulators of the fold.
struct Fold {
    ledger: UsageLedger,
    speed: SpeedAccumulator,
    compactions: Vec<CompactionObservation>,
    compaction_total: u64,
    claims: Vec<ChildClaim>,
    server: Option<ServerToolCounts>,
    model_ms: f64,
    ended_at: Option<Timestamp>,
    last_event_at: Option<Timestamp>,
    warnings: Vec<Warning>,
    /// Facts of the newest finished segment (continuation chains).
    finished: Option<Current>,
    unproven: usize,
    /// A record that could hold a compaction was unreadable or unindexed.
    history_gap: bool,
}

struct Work {
    catalog: OmpCatalog,
    phase: Phase,
    proofs: Vec<SessionKey>,
    proof_at: usize,
    proof: Option<ProofScan>,
    source_at: usize,
    /// `(path, length, modified)` of earlier members as folded: a change restarts.
    completed: Vec<(PathBuf, u64, Option<SystemTime>)>,
    scan: Option<FoldScan>,
    fold: Fold,
}

fn file_path(key: &SessionKey) -> Result<PathBuf, ProviderError> {
    match &key.source {
        SourceKey::File(path) => Ok(path.clone()),
        SourceKey::DbRow { .. } => Err(ProviderError::Unsupported(
            "an OMP session is a file source".into(),
        )),
    }
}

fn open_error(error: &crate::ingest::source::SourceError) -> ProviderError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ProviderError::Unavailable("session source is unavailable".into())
    } else {
        ProviderError::Io(error.to_string())
    }
}

/// Resumable lifetime facts of one OMP session.
pub struct OmpHydration {
    meta: SessionMetadata,
    prices: PriceSchedule,
    policy: ReferencePolicy,
    roots: Vec<PathBuf>,
    max_entries: usize,
    max_bytes: usize,
    max_record_bytes: usize,
    max_sources: usize,
    max_meta_bytes: usize,
    /// The physical sources in chain order; one for a plain session.
    sources: Vec<SessionKey>,
    work: Option<Work>,
}

impl OmpHydration {
    /// Cheap: no file is opened until the first [`OmpHydration::step`].
    pub fn new(meta: SessionMetadata, config: &Config) -> Result<Self, ProviderError> {
        if meta.key.harness != Harness::Omp {
            return Err(ProviderError::Unsupported("not an OMP session".into()));
        }
        file_path(&meta.key)?;
        let sources = if meta.members.is_empty() {
            vec![meta.key.clone()]
        } else {
            meta.members.clone()
        };
        let omp = &config.harnesses.omp;
        Ok(Self {
            prices: config.pricing.clone(),
            policy: config.reference_policy(Harness::Omp),
            roots: omp.root.iter().chain(&omp.extra_roots).cloned().collect(),
            max_entries: config.dedup_ledger_entries,
            max_bytes: config.dedup_ledger_bytes,
            max_record_bytes: config.max_record_bytes,
            max_sources: config.max_metadata_sources,
            max_meta_bytes: config.max_metadata_bytes,
            sources,
            work: None,
            meta,
        })
    }

    /// Drops the bounded mutable ledger, entry index, evidence and source readers.
    /// A later [`OmpHydration::step`] starts over.
    pub fn cancel(&mut self) {
        self.work = None;
    }

    /// Requests the ledger currently holds (zero once cancelled).
    pub fn retained_requests(&self) -> usize {
        self.work
            .as_ref()
            .map_or(0, |w| w.fold.ledger.observed_requests())
    }

    /// What this session's `task` results quoted about children (private evidence).
    pub fn child_claims(&self) -> &[ChildClaim] {
        self.work.as_ref().map_or(&[], |w| w.fold.claims.as_slice())
    }

    fn fresh(&self) -> Work {
        let mut catalog =
            OmpCatalog::new(self.roots.clone(), self.max_sources, self.max_meta_bytes)
                .with_evidence_limit(self.max_entries.saturating_mul(2));
        catalog.seed_links(&self.meta);
        let needs_parent = matches!(
            self.meta.lineage.fork,
            Some(crate::domain::SessionLink::Resolved(_))
        );
        let mut work = Work {
            catalog,
            phase: Phase::Fold,
            proofs: Vec::new(),
            proof_at: 0,
            proof: None,
            source_at: 0,
            completed: Vec::new(),
            scan: None,
            fold: Fold {
                ledger: UsageLedger::new(self.max_entries, self.max_bytes),
                speed: SpeedAccumulator::new(self.max_entries),
                compactions: Vec::new(),
                compaction_total: 0,
                claims: Vec::new(),
                server: None,
                model_ms: 0.0,
                ended_at: None,
                last_event_at: None,
                warnings: Vec::new(),
                finished: None,
                unproven: 0,
                history_gap: false,
            },
        };
        if needs_parent {
            work.phase = Phase::Discover;
        }
        work
    }

    /// One budgeted slice. While `done` is false the summary holds the discovery
    /// metadata only; once done it carries every measured fact, and a grown source is
    /// folded on the next call.
    pub fn step(&mut self, budget: RefreshBudget) -> Result<HydrationDelta, ProviderError> {
        let mut work = match self.work.take() {
            Some(work) => work,
            None => self.fresh(),
        };
        let outcome = self.advance(&mut work, budget);
        match outcome {
            Ok(Advance::Restart) => {
                // The source was replaced: nothing read so far describes it.
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
            // The next call starts over rather than trusting half-folded state.
            Err(error) => Err(error),
        }
    }

    fn advance(&self, work: &mut Work, budget: RefreshBudget) -> Result<Advance, ProviderError> {
        match work.phase {
            Phase::Discover => {
                let delta = work.catalog.step(budget)?;
                if delta.scan.done {
                    // The session's own facts are the caller's, not a re-probe.
                    work.catalog.seed_links(&self.meta);
                    work.proofs = work
                        .catalog
                        .fork_proof_sources(&self.meta.key)
                        .unwrap_or_default();
                    work.phase = if work.proofs.is_empty() {
                        Phase::Fold
                    } else {
                        Phase::Proof
                    };
                }
                Ok(Advance::Pending)
            }
            Phase::Proof => self.advance_proof(work, budget),
            Phase::Fold | Phase::Done => self.advance_fold(work, budget),
        }
    }

    fn advance_proof(
        &self,
        work: &mut Work,
        budget: RefreshBudget,
    ) -> Result<Advance, ProviderError> {
        if work.proof.is_none() {
            let Some(key) = work.proofs.get(work.proof_at).cloned() else {
                work.phase = Phase::Fold;
                return Ok(Advance::Pending);
            };
            work.catalog.begin_evidence(&key);
            let reader = file_path(&key)
                .ok()
                .and_then(|path| SourceReader::open(&path, self.max_record_bytes).ok());
            match reader {
                Some(reader) => {
                    work.proof = Some(ProofScan {
                        key,
                        reader,
                        incomplete: false,
                    });
                }
                None => {
                    // The parent cannot be read: its evidence stays incomplete, so a copy
                    // cannot be ruled out and ownership stays unproven.
                    work.fold.warn(
                        &key,
                        "proof",
                        WarningKind::Unavailable,
                        "a linked source could not be read, so copied requests cannot be ruled out",
                    );
                    work.proof_at += 1;
                }
            }
            return Ok(Advance::Pending);
        }
        let Some(mut proof) = work.proof.take() else {
            return Ok(Advance::Pending);
        };
        let batch = proof.reader.step(budget).map_err(|e| open_error(&e))?;
        if batch.invalidated {
            return Ok(Advance::Restart);
        }
        for item in &batch.records {
            match &item.item {
                SourceItem::Json(value) => {
                    let Some(header) = parse_entry_header(value, item.ordinal) else {
                        // A usage record with no native entry ID (a legacy file) cannot
                        // be matched later, so this proof can never be complete.
                        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
                        if !matches!(kind, "session" | "title")
                            && (call_identity(value).is_some() || is_assistant(value))
                        {
                            proof.incomplete = true;
                        }
                        continue;
                    };
                    if let Some((response, print)) = call_identity(value)
                        && !work.catalog.record_evidence(
                            &proof.key,
                            &header.id,
                            response.as_deref(),
                            print,
                        )
                    {
                        proof.incomplete = true;
                    }
                }
                SourceItem::Invalid | SourceItem::TooLarge | SourceItem::Damaged(_) => {
                    proof.incomplete = true;
                }
                SourceItem::Incomplete | SourceItem::Skipping => {}
            }
        }
        if batch.done {
            if proof.incomplete {
                work.fold.warn(
                    &proof.key,
                    "proof",
                    WarningKind::Unsupported,
                    "a linked source could not be fully read, so copied requests cannot be ruled out",
                );
            } else {
                work.catalog.complete_evidence(&proof.key);
            }
            work.proof_at += 1;
        } else {
            work.proof = Some(proof);
        }
        Ok(Advance::Pending)
    }

    fn advance_fold(
        &self,
        work: &mut Work,
        budget: RefreshBudget,
    ) -> Result<Advance, ProviderError> {
        let Some(key) = self.sources.get(work.source_at).cloned() else {
            work.phase = Phase::Done;
            return Ok(Advance::Done);
        };
        for (path, len, modified) in &work.completed {
            let now = std::fs::metadata(path)
                .ok()
                .map(|m| (m.len(), m.modified().ok()));
            if now != Some((*len, *modified)) {
                // An earlier member changed after it was folded.
                return Ok(Advance::Restart);
            }
        }
        let mut scan = match work.scan.take() {
            Some(scan) => scan,
            None => {
                let path = file_path(&key)?;
                let stamp = {
                    let meta = std::fs::metadata(&path)
                        .map_err(|e| ProviderError::Io(e.kind().to_string()))?;
                    (path.clone(), meta.len(), meta.modified().ok())
                };
                let reader =
                    SourceReader::open(&path, self.max_record_bytes).map_err(|e| open_error(&e))?;
                work.catalog.begin_evidence(&key);
                FoldScan {
                    key: key.clone(),
                    reader,
                    tree: OmpTree::new(self.max_entries, self.max_bytes),
                    states: HashMap::new(),
                    root: Arc::new(PathState::default()),
                    evidence_full: false,
                    stamp,
                }
            }
        };
        let batch = scan.reader.step(budget).map_err(|e| open_error(&e))?;
        if batch.invalidated {
            return Ok(Advance::Restart);
        }
        let ctx = Ctx {
            version: &self.meta.harness_version,
            prices: &self.prices,
            policy: self.policy,
        };
        for item in &batch.records {
            work.record(&mut scan, item, &ctx);
        }
        if !batch.done {
            work.phase = Phase::Fold;
            work.scan = Some(scan);
            return Ok(Advance::Pending);
        }
        if !scan.evidence_full {
            work.catalog.complete_evidence(&scan.key);
        }
        let last = work.source_at + 1 >= self.sources.len();
        if last {
            work.phase = Phase::Done;
            work.scan = Some(scan);
            return Ok(Advance::Done);
        }
        // An earlier segment: keep its facts, drop its index.
        match scan.current() {
            Ok(Some(current)) => work.fold.finished = Some(current),
            Ok(None) if !scan.tree.is_empty() => work.fold.finished = None,
            Ok(None) => {}
            Err(_) => work.fold.finished = None,
        }
        work.completed.push(scan.stamp.clone());
        work.source_at += 1;
        Ok(Advance::Pending)
    }

    fn summary(&self, work: &Work) -> SessionSummary {
        let fold = &work.fold;
        let mut meta = self.meta.clone();
        let mut warnings = fold.warnings.clone();
        let live = work.scan.as_ref().map(FoldScan::current);
        let cleared = work.scan.as_ref().is_some_and(|s| !s.tree.is_empty());
        let current = match live {
            Some(Ok(Some(current))) => Some(current),
            // An explicitly cleared leaf has no current facts to inherit.
            Some(Ok(None)) if cleared => None,
            Some(Ok(None)) | None => fold.finished.clone(),
            Some(Err(error)) => {
                if let Some(scan) = &work.scan {
                    warnings.push(source_warning(
                        &scan.key,
                        "active-branch",
                        WarningKind::Parse,
                        &format!("active branch unavailable: {error}"),
                    ));
                }
                None
            }
        };
        let (model, context) = match current {
            Some(current) => (current.model, current.context),
            None => (None, ContextUsage::default()),
        };
        meta.model = model.or(meta.model);
        let model_ms = (fold.model_ms > 0.0).then_some(fold.model_ms);
        let compactions = compaction_summary(
            &fold.compactions,
            fold.compaction_total as usize == fold.compactions.len() && !fold.history_gap,
            &self.prices,
            self.policy,
        );
        SessionSummary {
            meta,
            usage: Some(fold.ledger.snapshot()),
            metrics: SessionMetrics {
                // No disk-backed active timer exists: active time stays Unknown, and
                // only recorded request time is measured.
                timing: session_timing(self.meta.created_at, fold.ended_at, &[], false, model_ms),
                speed: fold.speed.snapshot(),
                context,
                compactions,
            },
            lifecycle: Lifecycle::Unknown,
            unfinished_work: false,
            last_event_at: fold.last_event_at,
            last_completion: None,
            displayed_event_count: 0,
            hidden_records: 0,
            server_tools: fold.server,
            process: crate::domain::ProcessAssociation::NotChecked,
            warnings,
        }
    }
}

enum Advance {
    Pending,
    Done,
    /// The source was replaced or rotated; start over.
    Restart,
}

fn source_warning(key: &SessionKey, label: &str, kind: WarningKind, message: &str) -> Warning {
    match &key.source {
        SourceKey::File(path) => warning(path, label, kind, message),
        SourceKey::DbRow { .. } => warning(std::path::Path::new(""), label, kind, message),
    }
}

impl Fold {
    fn warn(&mut self, key: &SessionKey, label: &str, kind: WarningKind, message: &str) {
        let warning = source_warning(key, label, kind, message);
        match self.warnings.iter_mut().find(|w| w.key == warning.key) {
            Some(known) => known.count = known.count.saturating_add(1),
            None => self.warnings.push(warning),
        }
    }
}

fn claims_of(entry: &Value) -> Vec<ChildClaim> {
    let Some(message) = entry.get("message") else {
        return Vec::new();
    };
    if message.get("role").and_then(Value::as_str) != Some("toolResult")
        || message.get("toolName").and_then(Value::as_str) != Some("task")
    {
        return Vec::new();
    }
    let Some(results) = message
        .get("details")
        .and_then(|d| d.get("results"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|result| {
            let agent_id = text(result.get("id"))?;
            let quoted = result
                .get("usage")
                .and_then(|u| u.get("cost"))
                .and_then(|c| c.get("total"))
                .or_else(|| result.get("cost"))
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0);
            Some(ChildClaim {
                agent_id,
                quoted_usd: quoted,
            })
        })
        .collect()
}

/// What the record fold needs from the hydration besides its own state.
struct Ctx<'a> {
    version: &'a Option<String>,
    prices: &'a PriceSchedule,
    policy: ReferencePolicy,
}

impl Work {
    fn record(&mut self, scan: &mut FoldScan, item: &SourceRecordItem, ctx: &Ctx<'_>) {
        match &item.item {
            SourceItem::Json(value) => self.entry(scan, item, value, ctx),
            SourceItem::Invalid | SourceItem::TooLarge | SourceItem::Damaged(_) => {
                self.fold.history_gap = true;
                self.fold
                    .ledger
                    .mark_partial("a record could not be read, so its usage may be missing");
                self.fold.warn(
                    &scan.key,
                    "record",
                    WarningKind::Parse,
                    "a record could not be read",
                );
            }
            SourceItem::Incomplete | SourceItem::Skipping => {}
        }
    }

    fn entry(
        &mut self,
        scan: &mut FoldScan,
        item: &SourceRecordItem,
        value: &Value,
        ctx: &Ctx<'_>,
    ) {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(header) = parse_entry_header(value, item.ordinal) else {
            // The session header and title slot are not entries; any other record
            // that carries usage but cannot be indexed may be missing spend.
            if !matches!(kind, "session" | "title")
                && (call_identity(value).is_some() || is_assistant(value))
            {
                self.fold
                    .ledger
                    .mark_partial("a usage record could not be indexed");
            }
            if !matches!(kind, "session" | "title") {
                self.fold.history_gap = true;
            }
            return;
        };
        let id = header.id.clone();
        let parent = header.parent_id.clone();
        let inserted = scan.tree.insert(header);
        if inserted == OmpInsert::Limited {
            self.fold.history_gap = true;
            self.fold.ledger.mark_partial("the entry index is full");
            self.fold.warn(
                &scan.key,
                "entries",
                WarningKind::Unsupported,
                "entry index limit reached; the active branch is unknown",
            );
        }
        if let Some(at) = entry_time(value) {
            self.fold.last_event_at = self.fold.last_event_at.max(Some(at));
        }
        let is_call = matches!(kind, "message" | "model_usage");
        if is_call {
            // A later record is evidence the session was reopened after an exit.
            self.fold.ended_at = None;
        }
        let parent_state = parent
            .as_ref()
            .and_then(|p| scan.states.get(p))
            .cloned()
            .unwrap_or_else(|| Arc::clone(&scan.root));

        if inserted == OmpInsert::Inserted {
            let state =
                next_state(&parent_state, kind, value).unwrap_or_else(|| parent_state.clone());
            scan.states.insert(id.clone(), state);
        }
        if let Some((response, print)) = call_identity(value) {
            self.call(
                scan,
                item,
                value,
                kind,
                &id,
                &parent_state,
                response,
                print,
                ctx,
            );
        } else if is_assistant(value) {
            self.fold
                .ledger
                .mark_partial("an assistant record carries no usage");
        }
        match kind {
            "session_exit" if inserted != OmpInsert::Duplicate => {
                self.fold.ended_at = entry_time(value);
            }
            "compaction" if inserted == OmpInsert::Inserted => {
                self.compaction(scan, item, value, &id, ctx);
            }
            "message"
                if inserted != OmpInsert::Duplicate && self.fold.claims.len() < MAX_CLAIMS =>
            {
                let room = MAX_CLAIMS - self.fold.claims.len();
                self.fold
                    .claims
                    .extend(claims_of(value).into_iter().take(room));
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        &mut self,
        scan: &mut FoldScan,
        item: &SourceRecordItem,
        value: &Value,
        kind: &str,
        id: &str,
        parent_state: &PathState,
        response: Option<String>,
        print: u64,
        ctx: &Ctx<'_>,
    ) {
        // The same entry again (a repeated line) is not a second call; the same ID
        // with other counters cannot be attributed.
        if let Some((recorded, old)) = self.catalog.evidence_entry(&scan.key, id) {
            let distinct_response = matches!(
                (recorded.as_deref(), response.as_deref()),
                (Some(a), Some(b)) if a != b
            );
            if !distinct_response {
                if *old != print || *recorded != response {
                    self.fold
                        .ledger
                        .mark_partial("a repeated entry ID carried different usage");
                }
                return;
            }
        }
        let ownership = owned_request(id, response.as_deref(), &scan.key, &self.catalog);
        if !self
            .catalog
            .record_evidence(&scan.key, id, response.as_deref(), print)
        {
            scan.evidence_full = true;
            self.fold
                .ledger
                .mark_partial("the ownership evidence limit was reached");
        }
        match ownership {
            // Copies of a linked source's calls are that source's spend.
            OmpOwnership::Inherited => {}
            OmpOwnership::Unknown => {
                self.fold.unproven += 1;
                self.fold
                    .ledger
                    .mark_partial("ownership of copied requests could not be proven");
            }
            OmpOwnership::Owned => {
                let provenance = Provenance {
                    harness: Harness::Omp,
                    source: scan.key.source.clone(),
                    locator: SourceLocator {
                        session: scan.key.clone(),
                        kind: item.locator.clone(),
                        generation: Some(item.generation),
                    },
                    record_type: if kind == "model_usage" {
                        "model_usage".into()
                    } else {
                        "message/assistant".into()
                    },
                    harness_version: ctx.version.clone(),
                };
                let Some(mut observation) = map_omp_usage(value, provenance) else {
                    return;
                };
                if let Some(model) = observation.model.as_mut() {
                    model.effort = parent_state.effort.as_deref().map(str::to_owned);
                    model.configured_effort = parent_state.configured.as_deref().map(str::to_owned);
                }
                let counted = self.fold.ledger.observe(
                    UsageUpdate::Initial(&observation),
                    ctx.prices,
                    ctx.policy,
                ) == LedgerOutcome::Counted;
                // Speed and request time follow the ledger's identity, so a repeat of
                // the same confirmed request is measured once.
                if counted {
                    // Throughput is conversation output only: additive orchestration is
                    // billed above but is not the generated stream being timed.
                    let mut timed = observation.clone();
                    timed.categories.orchestration_input = None;
                    timed.categories.orchestration_output = None;
                    timed.categories.orchestration_cache_read = None;
                    self.fold.speed.observe(&timed);
                    if let Some(duration) = observation
                        .request
                        .as_ref()
                        .and_then(|r| r.duration_ms)
                        .filter(|d| d.is_finite() && *d > 0.0)
                    {
                        self.fold.model_ms += duration;
                    }
                    if let Some((search, fetch)) = server_tools(value) {
                        let totals = self
                            .fold
                            .server
                            .get_or_insert_with(ServerToolCounts::default);
                        totals.web_search = totals.web_search.saturating_add(search);
                        totals.web_fetch = totals.web_fetch.saturating_add(fetch);
                    }
                }
            }
        }
    }

    fn compaction(
        &mut self,
        scan: &FoldScan,
        item: &SourceRecordItem,
        value: &Value,
        id: &str,
        ctx: &Ctx<'_>,
    ) {
        self.fold.compaction_total += 1;
        if self.fold.compactions.len() >= MAX_COMPACTIONS {
            self.fold.compactions.remove(0);
        }
        self.fold.compactions.push(CompactionObservation {
            id: format!("omp:{id}"),
            timestamp: entry_time(value),
            method: text(value.get("method")),
            trigger: None,
            summary: None,
            before_tokens: value.get("tokensBefore").and_then(Value::as_u64),
            after_tokens: value.get("tokensAfter").and_then(Value::as_u64),
            duration_ms: None,
            // OMP persists no usage for the summarization call, so its cost is Unknown.
            usage: None,
            scope: CompactionScope::Event,
            provenance: Provenance {
                harness: Harness::Omp,
                source: scan.key.source.clone(),
                locator: SourceLocator {
                    session: scan.key.clone(),
                    kind: item.locator.clone(),
                    generation: Some(item.generation),
                },
                record_type: "compaction".into(),
                harness_version: ctx.version.clone(),
            },
        });
    }
}
