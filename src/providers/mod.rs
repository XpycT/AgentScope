//! Provider adapter contract. Adapters implement [`ProviderReader`]; the worker
//! drives them. No provider-specific parsing lives here.
//!
//! Every call is bounded by a [`RefreshBudget`]. Work that does not fit returns
//! progress (`ReadProgress::Pending`, `HydrationDelta { done: false }`, or
//! `ScanProgress { done: false }`) and is resumed by calling again; that is
//! normal progress, never an error or a retry warning. Continuation state is
//! owned per `(SessionKey, ReadJobId)` through [`JobLedger`], so abandoning one
//! purpose on a session cannot release another purpose's work.

pub mod claude;
pub mod codex;
pub mod opencode;
pub(crate) mod probe;

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use crate::domain::{
    Harness, PageAnchor, PageDirection, PageRequest, RefreshBudget, ScanProgress,
    SessionDetailPage, SessionKey, SessionMetadata, SessionSummary, SourceLocator, Timestamp,
    TranscriptPage, Warning,
};

/// Work a bounded call actually performed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkProgress {
    pub records: usize,
    pub expanded_bytes: usize,
}

/// Worker-assigned and unique per in-flight request. One session may have several
/// live jobs (preview, details, transcript, source); they never share a job ID.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReadJobId(pub u64);

/// Result of one bounded slice of resumable work.
#[derive(Clone, Debug, PartialEq)]
pub enum ReadProgress<T> {
    /// More work remains; call again with the same job.
    Pending(WorkProgress),
    Ready(T),
}

/// One bounded slice of metadata discovery. `scan.done == false` means the budget
/// was exhausted and the caller should call `discover` again to continue.
#[derive(Clone, Debug, PartialEq)]
pub struct DiscoveryDelta {
    /// New or changed lightweight metadata. No transcript body was parsed for it.
    pub sessions: Vec<SessionMetadata>,
    pub removed: Vec<SessionKey>,
    /// Sessions whose already-readable content changed (revised in place,
    /// rewritten, or rotated), so any loaded page or cached summary is stale.
    pub invalidated: Vec<SessionKey>,
    pub warnings: Vec<Warning>,
    pub scan: ScanProgress,
}

/// One slice of a session's detailed facts. While `done` is false `summary`
/// holds the known metadata only; every hydrated fact stays Unknown.
#[derive(Clone, Debug, PartialEq)]
pub struct HydrationDelta {
    pub summary: SessionSummary,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderError {
    Io(String),
    Parse(String),
    Unsupported(String),
    Unavailable(String),
    Internal(String),
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, msg) = match self {
            Self::Io(m) => ("io error", m),
            Self::Parse(m) => ("parse error", m),
            Self::Unsupported(m) => ("unsupported", m),
            Self::Unavailable(m) => ("unavailable", m),
            Self::Internal(m) => ("internal error", m),
        };
        write!(f, "{kind}: {msg}")
    }
}

impl std::error::Error for ProviderError {}

/// Raw and display forms of a persisted source record, or why it is unavailable.
/// `Debug` is deliberately content-free: records may hold prompts, tool output
/// or credentials, so only lengths and availability are printed.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceRecord {
    pub raw: String,
    pub display: String,
    pub unavailable: Option<String>,
}

impl fmt::Debug for SourceRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceRecord")
            .field("raw_len", &self.raw.len())
            .field("display_len", &self.display.len())
            .field("unavailable", &self.unavailable.is_some())
            .finish()
    }
}

pub trait ProviderReader: Send {
    fn harness(&self) -> Harness;

    /// Resumable, allowlisted metadata discovery. Never parses transcript bodies or
    /// decompresses archives; a fact the bounded probes cannot establish stays
    /// Unknown. A pass that finishes (`scan.done`) also reports removals; the next
    /// call starts a new pass.
    fn discover(
        &mut self,
        now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError>;

    /// Detailed facts for one session. Resumable: call again with the same job
    /// until `done`. A completed summary stays cached by the reader; an unchanged
    /// source answers in one call and a grown one continues from its consumed prefix.
    fn hydrate(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError>;

    /// Drops the work only `job` still needed. Other jobs on `key` keep theirs, and
    /// completed (healthy) state stays cached.
    fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId);

    /// Record-level details: requests, compaction markers and recent events share
    /// `request`. `anchor` is the `next` of the previous page and continues into
    /// older events.
    fn details_page(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError>;

    fn transcript_page(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError>;

    fn count_events_after(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: &PageAnchor,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError>;

    fn source_record(
        &mut self,
        job: ReadJobId,
        locator: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError>;
}

/// Which jobs currently consume a session's partial work. An adapter registers a job
/// when it first answers `Pending` for it and releases the job when the job finishes
/// or is cancelled. Partial state may be dropped only when [`JobLedger::release`]
/// reports that no consumer remains.
#[derive(Debug, Default)]
pub struct JobLedger {
    consumers: HashMap<SessionKey, BTreeSet<ReadJobId>>,
}

impl JobLedger {
    pub fn join(&mut self, key: &SessionKey, job: ReadJobId) {
        match self.consumers.get_mut(key) {
            Some(jobs) => {
                jobs.insert(job);
            }
            None => {
                self.consumers.insert(key.clone(), BTreeSet::from([job]));
            }
        }
    }

    /// Removes `job`; `true` when `key` has no consumer left.
    pub fn release(&mut self, key: &SessionKey, job: ReadJobId) -> bool {
        let Some(jobs) = self.consumers.get_mut(key) else {
            return true;
        };
        jobs.remove(&job);
        if jobs.is_empty() {
            self.consumers.remove(key);
            true
        } else {
            false
        }
    }

    pub fn has_consumer(&self, key: &SessionKey) -> bool {
        self.consumers.contains_key(key)
    }

    /// Number of `(session, job)` pairs still holding partial work.
    pub fn len(&self) -> usize {
        self.consumers.values().map(BTreeSet::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.consumers.is_empty()
    }
}

/// Outcome of offering a source to a [`Retention`] set.
pub enum Admit<K> {
    /// Kept; `evicted` are the older sources displaced to stay within the caps.
    Kept { evicted: Vec<K> },
    /// A source that was never tracked is older than everything kept at the cap, or
    /// larger than the byte cap.
    Dropped,
    /// A tracked source could not stay (it grew past the caps or is no longer newer than
    /// what would have to go): its bookkeeping is forgotten. The source still exists, so
    /// this is not a deletion.
    Displaced,
}

/// Bounded newest-first retention for an adapter's discovery bookkeeping, so resident
/// per-source state never exceeds the configured metadata ceiling (source count and
/// estimated bytes). Ordering matches the roster: unknown activity is oldest, ties by
/// key. Eviction only forgets bookkeeping; it says nothing about the source on disk.
pub struct Retention<K: Ord + Clone + std::hash::Hash> {
    cap: usize,
    cap_bytes: usize,
    bytes: usize,
    order: std::collections::BTreeSet<(Option<Timestamp>, K)>,
    at: HashMap<K, (Option<Timestamp>, usize)>,
}

impl<K: Ord + Clone + std::hash::Hash> Retention<K> {
    pub fn new(cap: usize, cap_bytes: usize) -> Self {
        Self {
            cap: cap.max(1),
            cap_bytes: cap_bytes.max(1),
            bytes: 0,
            order: std::collections::BTreeSet::new(),
            at: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.at.len()
    }

    pub fn is_empty(&self) -> bool {
        self.at.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn contains(&self, key: &K) -> bool {
        self.at.contains_key(key)
    }

    pub fn admit(&mut self, key: &K, at: Option<Timestamp>, size: usize) -> Admit<K> {
        // An update re-enters as a fresh admission under the same age rules; the old
        // entry is taken out first so it cannot shield itself or evict newer keys.
        let existing = self.at.remove(key);
        if let Some((old, old_size)) = existing {
            self.order.remove(&(old, key.clone()));
            self.bytes -= old_size;
        }
        let rejected = if existing.is_some() {
            Admit::Displaced
        } else {
            Admit::Dropped
        };
        if size > self.cap_bytes {
            return rejected;
        }
        // Plan the evictions first: only strictly older sources may be displaced, and
        // nothing is evicted when the candidate cannot be admitted.
        let (mut count, mut bytes) = (self.at.len(), self.bytes);
        let mut victims: Vec<(Option<Timestamp>, K)> = Vec::new();
        for (oldest_at, oldest) in &self.order {
            if count < self.cap && bytes + size <= self.cap_bytes {
                break;
            }
            if at <= *oldest_at {
                return rejected;
            }
            count -= 1;
            bytes -= self.at[oldest].1;
            victims.push((*oldest_at, oldest.clone()));
        }
        if count >= self.cap || bytes + size > self.cap_bytes {
            return rejected;
        }
        let mut evicted = Vec::new();
        for (a, k) in victims {
            self.order.remove(&(a, k.clone()));
            self.at.remove(&k);
            evicted.push(k);
        }
        self.bytes = bytes;
        self.order.insert((at, key.clone()));
        self.at.insert(key.clone(), (at, size));
        self.bytes += size;
        Admit::Kept { evicted }
    }

    pub fn remove(&mut self, key: &K) {
        if let Some((old, size)) = self.at.remove(key) {
            self.order.remove(&(old, key.clone()));
            self.bytes -= size;
        }
    }
}

/// Estimated size of one compaction observation in a details page.
pub const COMPACTION_BYTES: usize = 512;

/// Splits one record/byte allowance across a first details page: at most a third of
/// the records and bytes go to compactions, the rest to the paged events, halved when
/// `requests_from_events` because each event's request observation counts against the
/// same allowance. Returns how many compactions to keep and the event page request.
pub fn share_details_budget(
    request: PageRequest,
    compactions: usize,
    requests_from_events: bool,
) -> (usize, PageRequest) {
    let max = request.max_events.max(1);
    let keep = compactions
        .min(max / 3)
        .min(request.max_bytes / 3 / COMPACTION_BYTES);
    let mut events = max - keep;
    let mut bytes = request.max_bytes.saturating_sub(keep * COMPACTION_BYTES);
    if requests_from_events {
        events = (events / 2).max(1);
        bytes = (bytes / 2).max(1);
    }
    (
        keep,
        PageRequest {
            max_events: events.max(1),
            max_bytes: bytes.max(1),
        },
    )
}

/// Sources whose bookkeeping was evicted but may still be frozen into a roster or read by
/// an in-flight job: a bounded authorization/deletion-watch set (oldest entries drop).
pub struct Forgotten<K: Eq + std::hash::Hash + Clone, V> {
    cap: usize,
    order: std::collections::VecDeque<K>,
    map: HashMap<K, V>,
}

impl<K: Eq + std::hash::Hash + Clone, V> Forgotten<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            order: std::collections::VecDeque::new(),
            map: HashMap::new(),
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        if self.map.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.map.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            } else {
                break;
            }
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let value = self.map.remove(key);
        if value.is_some() {
            self.order.retain(|k| k != key);
        }
        value
    }

    pub fn keys(&self) -> Vec<K> {
        self.order.iter().cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// Frozen or in-flight sources watched after eviction.
pub const FORGOTTEN_CAP: usize = 4096;
