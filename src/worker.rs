//! Background worker: owns the provider adapters, the metadata index and the process
//! sampler, and speaks a small request/event protocol so the UI thread never touches
//! disk.
//!
//! Scheduling rules (spec §5, §13):
//! - `Refresh` requests are coalesced into one flag; a request that arrives during a
//!   pass means "run one more pass afterwards". Only the initial pass and explicit
//!   refreshes freeze a new roster generation; periodic passes merge metadata and
//!   changed facts into the frozen one.
//! - Every adapter call is one bounded slice (a [`RefreshBudget`]). The main loop does
//!   at most one discovery slice and one hydration slice per iteration and services
//!   queued loads and requests in between, so navigation is never blocked.
//! - `ReadProgress::Pending` is normal progress: the load is requeued after the slice
//!   with selection, cancellation and shutdown checked first. Loads are not retried
//!   by delay unless an adapter reports `Unavailable` (for example a source that
//!   changed under the read), which waits [`RETRY_DELAY`], never a tight loop.
//! - Only the latest selection / preview matters. Responses carry the request ID of
//!   the request that produced them and are dropped (their job cancelled) if a newer
//!   one superseded them while they were being computed.
//! - Automatic hydration of the first 50 groups is a work iterator over the loaded
//!   roots, not entries in the 16-slot load queue, so none of them is ever dropped.
//! - A provider failure keeps its last good facts and replaces its warnings by
//!   [`WarningKey`] per pass; recovery clears the key.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::domain::{
    Harness, PageAnchor, PageDirection, PageRequest, RefreshBudget, ScanProgress,
    SessionDetailPage, SessionKey, SessionMetadata, SessionSummary, SourceLocator, Timestamp,
    TranscriptPage, Warning, WarningKey, WarningKind,
};
use crate::processes::{self, ProcessError, ProcessSnapshot};
use crate::providers::{
    DiscoveryDelta, ProviderError, ProviderReader, ReadJobId, ReadProgress, SourceRecord,
    WorkProgress,
};
use crate::roster::{CHILD_PAGE, MetadataLimits, RosterIndex, RosterSnapshot};

/// Capacity of the worker → UI event queue. The worker blocks (politely, see
/// `emit`) when the UI falls behind.
const EVENT_CAPACITY: usize = 256;
/// Maximum queued user-driven loads; the oldest are dropped first.
const MAX_PENDING_LOADS: usize = 16;
/// Delay before a retryable (`Unavailable`) request is attempted again.
const RETRY_DELAY: Duration = Duration::from_millis(250);
/// Poll interval while the event queue is full.
const BACKPRESSURE_POLL: Duration = Duration::from_millis(2);
/// Roster publications caused by background progress are at most this frequent.
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
/// Per-slice work budget handed to each adapter.
const CHUNK_BUDGET: RefreshBudget = RefreshBudget {
    max_records: 256,
    max_bytes: 8 * 1024 * 1024,
    max_millis: 100,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerRequest {
    /// Rebuild the roster ordering from fresh discovery (explicit refresh).
    Refresh,
    Select {
        request_id: u64,
        key: SessionKey,
    },
    LoadPage {
        request_id: u64,
        key: SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
    },
    /// Count events after `anchor` (new-event counting, spec §10.3).
    CountAfter {
        request_id: u64,
        key: SessionKey,
        anchor: PageAnchor,
    },
    LoadSource {
        request_id: u64,
        locator: SourceLocator,
    },
    /// Reveal the next 50 top-level groups of `generation`; stale generations are ignored.
    LoadRosterMore {
        generation: u64,
    },
    /// One batch of a family's child metadata after `cursor` (the previous `next`).
    LoadChildren {
        request_id: u64,
        parent: SessionKey,
        cursor: Option<SessionKey>,
    },
    /// Hydrate one session's detailed facts; the newest request wins.
    HydratePreview {
        request_id: u64,
        key: SessionKey,
    },
    LoadDetails {
        request_id: u64,
        key: SessionKey,
        anchor: Option<PageAnchor>,
    },
    /// Read the newest (or one older) page for the preview's last-activity
    /// block. Never shares the open transcript's job.
    PreviewTail {
        request_id: u64,
        key: SessionKey,
        anchor: Option<PageAnchor>,
    },
    /// Abandon a queued or running load; its adapter state is released.
    CancelLoad {
        request_id: u64,
    },
    Shutdown,
}

#[derive(Clone, Debug)]
pub enum WorkerEvent {
    Roster {
        snapshot: RosterSnapshot,
        warnings: Vec<Warning>,
    },
    /// Detailed facts for a `HydratePreview`. `done == false` is a cached or partial
    /// answer; the completed one follows.
    Preview {
        request_id: u64,
        key: SessionKey,
        summary: Arc<SessionSummary>,
        done: bool,
    },
    Details {
        request_id: u64,
        key: SessionKey,
        page: SessionDetailPage,
    },
    /// Work a pending load has done so far.
    LoadProgress {
        request_id: u64,
        records: usize,
        expanded_bytes: usize,
    },
    Children {
        request_id: u64,
        parent: SessionKey,
        metadata: Vec<Arc<SessionMetadata>>,
        next: Option<SessionKey>,
    },
    Transcript {
        request_id: u64,
        page: TranscriptPage,
    },
    /// One page of the preview's last-activity read.
    PreviewTail {
        request_id: u64,
        key: SessionKey,
        page: TranscriptPage,
    },
    /// The selected session changed in place; the visible page must be reloaded.
    /// This is not a new event for new-event counting.
    TranscriptInvalidated {
        key: SessionKey,
    },
    Source {
        request_id: u64,
        record: SourceRecord,
    },
    /// Number of events after the anchor of a `CountAfter` request.
    EventsAfter {
        request_id: u64,
        count: usize,
    },
    /// A request failed for good. Carries the request identity so only the matching
    /// in-flight request is aborted.
    LoadFailed {
        request_id: u64,
    },
    Processes {
        snapshot: ProcessSnapshot,
    },
    Warning {
        warning: Warning,
    },
    RefreshFinished {
        generation: u64,
    },
}

type Sampler = Box<dyn FnMut(Timestamp) -> Result<ProcessSnapshot, ProcessError> + Send>;

/// Builds provider readers; runs on the worker thread.
pub type ReaderFactory = Box<dyn FnOnce() -> Vec<Box<dyn ProviderReader>> + Send>;

pub struct WorkerHandle {
    pub tx: Sender<WorkerRequest>,
    pub rx: Receiver<WorkerEvent>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    /// Start a worker that samples real processes (when enabled in `config`).
    pub fn spawn(readers: Vec<Box<dyn ProviderReader>>, config: Config) -> Self {
        Self::spawn_with_sampler(readers, config, Box::new(processes::observe_processes))
    }

    /// Start a worker with an injected process sampler.
    pub fn spawn_with_sampler(
        readers: Vec<Box<dyn ProviderReader>>,
        config: Config,
        sampler: Sampler,
    ) -> Self {
        Self::spawn_inner(Box::new(move || readers), config, sampler)
    }

    /// Start a worker that builds its readers on the worker thread, so slow
    /// provider setup (e.g. opening SQLite) never blocks the caller.
    pub fn spawn_lazy(factory: ReaderFactory, config: Config) -> Self {
        Self::spawn_inner(factory, config, Box::new(processes::observe_processes))
    }

    /// [`Self::spawn_lazy`] with an injected process sampler.
    pub fn spawn_lazy_with_sampler(
        factory: ReaderFactory,
        config: Config,
        sampler: Sampler,
    ) -> Self {
        Self::spawn_inner(factory, config, sampler)
    }

    fn spawn_inner(factory: ReaderFactory, config: Config, sampler: Sampler) -> Self {
        let (tx, req_rx) = mpsc::channel();
        let (ev_tx, rx) = mpsc::sync_channel(EVENT_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let join = thread::Builder::new()
            .name("agentscope-worker".into())
            .spawn(move || {
                let worker = Worker::new(factory(), config, sampler, req_rx, ev_tx, worker_stop);
                worker.run()
            })
            .expect("spawn worker thread");
        Self {
            tx,
            rx,
            stop,
            join: Some(join),
        }
    }

    /// Ask the worker to stop and wait for it. Returns `false` if the worker
    /// thread had panicked.
    pub fn shutdown(mut self) -> bool {
        self.close()
    }

    fn close(&mut self) -> bool {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.send(WorkerRequest::Shutdown);
        match self.join.take() {
            Some(join) => join.join().is_ok(),
            None => true,
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.close();
    }
}

/// One user-driven load. Its job ID is private to the worker and unique per load, so
/// cancelling one load can never release another's adapter state.
struct Load {
    request_id: u64,
    job: ReadJobId,
    purpose: Purpose,
    retry_at: Option<Instant>,
    progress: WorkProgress,
}

enum Purpose {
    Page {
        key: SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
    },
    Source {
        locator: SourceLocator,
    },
    Count {
        key: SessionKey,
        anchor: PageAnchor,
    },
    Preview {
        key: SessionKey,
    },
    Details {
        key: SessionKey,
        anchor: Option<PageAnchor>,
    },
    PreviewTail {
        key: SessionKey,
        anchor: Option<PageAnchor>,
    },
}

impl Purpose {
    fn key(&self) -> &SessionKey {
        match self {
            Self::Page { key, .. }
            | Self::Count { key, .. }
            | Self::Preview { key }
            | Self::Details { key, .. }
            | Self::PreviewTail { key, .. } => key,
            Self::Source { locator } => &locator.session,
        }
    }
}

#[derive(Default)]
struct ProviderState {
    /// Warnings published for the last completed pass.
    warnings: HashMap<WarningKey, Warning>,
    /// Warnings accumulated during the pass in progress.
    pending: HashMap<WarningKey, Warning>,
    failure: Option<Warning>,
    limit: Option<Warning>,
}

/// A discovery pass in progress over every reader.
struct Pass {
    /// Freeze a new generation when it completes.
    explicit: bool,
    /// Readers that finished (or failed) this pass.
    done: Vec<bool>,
    /// Round-robin cursor, so one slow reader cannot starve the others.
    next: usize,
}

/// A hydration slice in flight.
struct Active {
    key: SessionKey,
    job: ReadJobId,
    reader: usize,
}

/// Automatic hydration: loaded roots without a summary, then changed cached sessions.
#[derive(Default)]
struct Hydration {
    roots: Arc<[SessionKey]>,
    cursor: usize,
    /// Roots already hydrated or tried; an evicted one is rehydrated on demand, not
    /// by this iterator.
    attempted: HashSet<SessionKey>,
    stale: VecDeque<SessionKey>,
    queued: HashSet<SessionKey>,
    current: Option<Active>,
    /// Sessions whose hydration was `Unavailable`, retried later.
    retry: Vec<(Instant, SessionKey)>,
}

struct Worker {
    readers: Vec<Box<dyn ProviderReader>>,
    states: Vec<ProviderState>,
    index: RosterIndex,
    config: Config,
    sampler: Sampler,
    requests: Receiver<WorkerRequest>,
    events: SyncSender<WorkerEvent>,
    stop: Arc<AtomicBool>,
    shutting_down: bool,
    generation: u64,
    next_job: u64,
    refresh_requested: bool,
    /// Run one non-freezing pass soon: a rehydrated selected session may have reported
    /// an in-place change, which adapters deliver with the next discovery slice.
    followup: bool,
    pass: Option<Pass>,
    pending: VecDeque<Load>,
    /// Newest transcript selection (request ID, key).
    selected: Option<(u64, SessionKey)>,
    /// Newest preview request (request ID, key).
    preview: Option<(u64, SessionKey)>,
    /// Request whose adapter slice is executing, and cancellations that arrived for it.
    running: Option<u64>,
    cancelled: HashSet<u64>,
    hydration: Hydration,
    /// Which reader published a key, for harnesses with several readers.
    owners: HashMap<SessionKey, usize>,
    observation: Option<Result<ProcessSnapshot, ProcessError>>,
    process_every: Duration,
    /// Advanced only when a sample is actually taken, never by refresh passes.
    next_process: Instant,
    dirty: bool,
    last_publish: Instant,
}

fn now_ms() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as Timestamp)
}

/// Run an adapter call, converting a panic into an internal error so a faulty
/// adapter cannot take the worker down.
fn guard<T>(f: impl FnOnce() -> Result<T, ProviderError>) -> Result<T, ProviderError> {
    catch_unwind(AssertUnwindSafe(f))
        .unwrap_or_else(|_| Err(ProviderError::Internal("provider adapter panicked".into())))
}

fn warning_kind(err: &ProviderError) -> WarningKind {
    match err {
        ProviderError::Io(_) => WarningKind::Io,
        ProviderError::Parse(_) => WarningKind::Parse,
        ProviderError::Unsupported(_) => WarningKind::Unsupported,
        ProviderError::Unavailable(_) | ProviderError::Internal(_) => WarningKind::Unavailable,
    }
}

fn failure_warning(harness: Harness, source: &str, err: &ProviderError) -> Warning {
    Warning {
        key: WarningKey {
            harness,
            source: source.into(),
            kind: warning_kind(err),
        },
        message: err.to_string(),
        count: 1,
    }
}

impl Worker {
    fn new(
        readers: Vec<Box<dyn ProviderReader>>,
        config: Config,
        sampler: Sampler,
        requests: Receiver<WorkerRequest>,
        events: SyncSender<WorkerEvent>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        let states = readers.iter().map(|_| ProviderState::default()).collect();
        let process_every = Duration::from_millis(config.process_ms.max(1));
        let index = RosterIndex::new(
            MetadataLimits {
                max_sources: config.max_metadata_sources,
                max_bytes: config.max_metadata_bytes,
            },
            config.max_cached_sessions,
        );
        Self {
            readers,
            states,
            index,
            config,
            sampler,
            requests,
            events,
            stop,
            shutting_down: false,
            generation: 0,
            next_job: 0,
            refresh_requested: true, // initial pass
            followup: false,
            pass: None,
            pending: VecDeque::new(),
            selected: None,
            preview: None,
            running: None,
            cancelled: HashSet::new(),
            hydration: Hydration::default(),
            owners: HashMap::new(),
            observation: None,
            process_every,
            next_process: Instant::now() + process_every,
            dirty: false,
            // The first publication is never throttled.
            last_publish: Instant::now()
                .checked_sub(PUBLISH_EVERY)
                .unwrap_or_else(Instant::now),
        }
    }

    fn run(mut self) {
        let refresh_every = Duration::from_millis(self.config.refresh_ms.max(1));
        let mut next_refresh = Instant::now() + refresh_every;
        while !self.shutting_down {
            self.drain_requests();
            if self.shutting_down {
                break;
            }
            self.service_pending();
            if self.shutting_down {
                break;
            }
            let now = Instant::now();
            if self.config.process_enabled && now >= self.next_process {
                self.sample_processes();
                continue;
            }
            if self.pass.is_none() {
                // Only an explicit request (or the very first pass) may freeze a new
                // generation; the timer and follow-ups merge into the frozen one.
                if self.refresh_requested || self.generation == 0 {
                    self.start_pass(true);
                    next_refresh = Instant::now() + refresh_every;
                } else if now >= next_refresh || self.followup {
                    self.start_pass(false);
                    next_refresh = Instant::now() + refresh_every;
                }
            }
            let mut worked = false;
            if self.pass.is_some() {
                self.discover_slice();
                worked = true;
            }
            if !self.shutting_down && self.hydrate_slice() {
                worked = true;
            }
            self.publish_if_due();
            if self.shutting_down {
                break;
            }
            if worked || self.has_due_load() {
                // Work remains: look at requests, then take the next slice.
                continue;
            }
            let mut deadline = next_refresh;
            if self.config.process_enabled {
                deadline = deadline.min(self.next_process);
            }
            if let Some(retry) = self.pending.iter().filter_map(|p| p.retry_at).min() {
                deadline = deadline.min(retry);
            }
            if let Some(retry) = self.hydration.retry.iter().map(|(at, _)| *at).min() {
                deadline = deadline.min(retry);
            }
            if self.dirty {
                deadline = deadline.min(self.last_publish + PUBLISH_EVERY);
            }
            let wait = deadline.saturating_duration_since(Instant::now());
            match self.requests.recv_timeout(wait) {
                Ok(req) => self.handle(req),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => self.shutting_down = true,
            }
        }
    }

    /// A queued load that can run now. Servicing always removes it, requeues it
    /// behind newer work, or pushes its `retry_at` into the future, so this cannot spin.
    fn has_due_load(&self) -> bool {
        let now = Instant::now();
        self.pending
            .iter()
            .any(|p| p.retry_at.is_none_or(|t| t <= now))
    }

    // ---- requests -------------------------------------------------------

    fn drain_requests(&mut self) {
        while !self.shutting_down {
            match self.requests.try_recv() {
                Ok(req) => self.handle(req),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => self.shutting_down = true,
            }
        }
    }

    fn handle(&mut self, req: WorkerRequest) {
        match req {
            WorkerRequest::Refresh => self.refresh_requested = true,
            WorkerRequest::Shutdown => self.shutting_down = true,
            WorkerRequest::Select { request_id, key } => {
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|(id, _)| *id > request_id)
                {
                    return; // older than the current selection
                }
                self.selected = Some((request_id, key.clone()));
                self.cancel_where(|load| matches!(load.purpose, Purpose::Page { .. }));
                self.push_load(
                    request_id,
                    Purpose::Page {
                        key,
                        anchor: None,
                        // Opening a transcript starts at its newest entry.
                        direction: PageDirection::Backward,
                    },
                );
            }
            WorkerRequest::LoadPage {
                request_id,
                key,
                anchor,
                direction,
            } => {
                if self.page_is_stale(request_id, &key) {
                    return;
                }
                self.push_load(
                    request_id,
                    Purpose::Page {
                        key,
                        anchor,
                        direction,
                    },
                );
            }
            WorkerRequest::CountAfter {
                request_id,
                key,
                anchor,
            } => {
                if self.page_is_stale(request_id, &key) {
                    return;
                }
                self.push_load(request_id, Purpose::Count { key, anchor });
            }
            WorkerRequest::LoadSource {
                request_id,
                locator,
            } => self.push_load(request_id, Purpose::Source { locator }),
            WorkerRequest::LoadRosterMore { generation } => {
                if self.index.load_more(generation) {
                    self.dirty = true;
                    self.publish();
                }
            }
            WorkerRequest::LoadChildren {
                request_id,
                parent,
                cursor,
            } => {
                let page = self.index.children(&parent, cursor.as_ref(), CHILD_PAGE);
                self.emit(WorkerEvent::Children {
                    request_id,
                    parent,
                    metadata: page.metadata,
                    next: page.next,
                });
            }
            WorkerRequest::HydratePreview { request_id, key } => {
                if self
                    .preview
                    .as_ref()
                    .is_some_and(|(id, _)| *id > request_id)
                {
                    return;
                }
                self.preview = Some((request_id, key.clone()));
                self.cancel_where(|load| matches!(load.purpose, Purpose::Preview { .. }));
                if let Some(summary) = self.index.summary(&key) {
                    // Cached facts are visible immediately; a fresh hydration follows.
                    self.emit(WorkerEvent::Preview {
                        request_id,
                        key: key.clone(),
                        summary,
                        done: false,
                    });
                }
                self.push_load(request_id, Purpose::Preview { key });
            }
            WorkerRequest::LoadDetails {
                request_id,
                key,
                anchor,
            } => self.push_load(request_id, Purpose::Details { key, anchor }),
            WorkerRequest::PreviewTail {
                request_id,
                key,
                anchor,
            } => self.push_load(request_id, Purpose::PreviewTail { key, anchor }),
            WorkerRequest::CancelLoad { request_id } => {
                // A load detached into the servicing loop is not in the queue; remember
                // the cancellation so it is honoured before it runs, requeues or publishes.
                if !self.cancel_where(|load| load.request_id == request_id) {
                    if self.cancelled.len() >= 1024 {
                        self.cancelled.clear();
                    }
                    self.cancelled.insert(request_id);
                }
            }
        }
    }

    fn new_job(&mut self) -> ReadJobId {
        self.next_job += 1;
        ReadJobId(self.next_job)
    }

    fn push_load(&mut self, request_id: u64, purpose: Purpose) {
        if self.pending.len() >= MAX_PENDING_LOADS
            && let Some(dropped) = self.pending.pop_front()
        {
            self.release(&dropped);
        }
        let job = self.new_job();
        self.pending.push_back(Load {
            request_id,
            job,
            purpose,
            retry_at: None,
            progress: WorkProgress::default(),
        });
    }

    /// Removes every queued load matching `matches` and releases only its own job.
    fn cancel_where(&mut self, matches: impl Fn(&Load) -> bool) -> bool {
        let queue = std::mem::take(&mut self.pending);
        let mut found = false;
        for load in queue {
            if matches(&load) {
                found = true;
                self.release(&load);
            } else {
                self.pending.push_back(load);
            }
        }
        found
    }

    /// Tells the adapter that `load`'s job is abandoned.
    fn release(&mut self, load: &Load) {
        let key = load.purpose.key().clone();
        if let Some(i) = self.reader_index(&key) {
            let job = load.job;
            let reader = &mut self.readers[i];
            let _ = guard(|| {
                reader.cancel_read(&key, job);
                Ok(())
            });
        }
    }

    /// A page load is stale unless its key is the selected session and it is
    /// not older than the latest selection.
    fn page_is_stale(&self, request_id: u64, key: &SessionKey) -> bool {
        match &self.selected {
            Some((sel_id, sel_key)) => request_id < *sel_id || sel_key != key,
            None => true,
        }
    }

    fn preview_is_stale(&self, request_id: u64) -> bool {
        self.preview
            .as_ref()
            .is_none_or(|(id, _)| *id != request_id)
    }

    /// Service every due queued load once. Loads still `Pending` and retryable
    /// failures are requeued.
    fn service_pending(&mut self) {
        let now = Instant::now();
        let queue = std::mem::take(&mut self.pending);
        let mut keep = VecDeque::new();
        let mut iter = queue.into_iter();
        for load in iter.by_ref() {
            if self.shutting_down {
                keep.push_back(load);
                break;
            }
            if self.cancelled.remove(&load.request_id) {
                self.release(&load);
                continue;
            }
            if load.retry_at.is_some_and(|t| t > now) {
                keep.push_back(load);
                continue;
            }
            if let Some(next) = self.service_one(load) {
                keep.push_back(next);
            }
        }
        keep.extend(iter);
        // Requests handled while servicing were pushed to `self.pending`.
        keep.extend(std::mem::take(&mut self.pending));
        // Requeued work was already admitted; only new arrivals can overflow.
        while keep.len() > MAX_PENDING_LOADS {
            if let Some(dropped) = keep.pop_front() {
                self.release(&dropped);
            }
        }
        self.pending = keep;
    }

    /// Runs one slice of `load`. `Some` is the load requeued (pending progress or a
    /// retryable failure).
    fn service_one(&mut self, mut load: Load) -> Option<Load> {
        let request_id = load.request_id;
        let job = load.job;
        let key = load.purpose.key().clone();
        let stale = |w: &Self, load: &Load| match &load.purpose {
            Purpose::Page { key, .. } | Purpose::Count { key, .. } => {
                w.page_is_stale(load.request_id, key)
            }
            Purpose::Preview { .. } => w.preview_is_stale(load.request_id),
            // The preview tail belongs to the row under the cursor, not to the
            // open transcript; only an explicit cancel abandons it.
            Purpose::PreviewTail { .. } | Purpose::Source { .. } | Purpose::Details { .. } => false,
        };
        if stale(self, &load) {
            self.release(&load);
            return None;
        }
        let Some(i) = self.reader_index(&key) else {
            self.emit(WorkerEvent::LoadFailed { request_id });
            return None;
        };
        let page_request = PageRequest {
            // A replacement page must fit the UI window (spec §10.3).
            max_events: self
                .config
                .max_page_events
                .min(crate::app::MAX_LOADED_EVENTS),
            max_bytes: self.config.max_page_bytes,
        };
        let now = now_ms();

        #[allow(clippy::large_enum_variant)]
        enum Answer {
            Page(Result<ReadProgress<TranscriptPage>, ProviderError>),
            Count(Result<ReadProgress<usize>, ProviderError>),
            Source(Result<ReadProgress<SourceRecord>, ProviderError>),
            Details(Result<ReadProgress<SessionDetailPage>, ProviderError>),
            Preview(Result<crate::providers::HydrationDelta, ProviderError>),
            Tail(Result<ReadProgress<TranscriptPage>, ProviderError>),
        }
        let reader = &mut self.readers[i];
        self.running = Some(request_id);
        let answer = match &load.purpose {
            Purpose::Page {
                key,
                anchor,
                direction,
            } => Answer::Page(guard(|| {
                reader.transcript_page(
                    job,
                    key,
                    anchor.clone(),
                    *direction,
                    page_request,
                    CHUNK_BUDGET,
                )
            })),
            Purpose::Count { key, anchor } => Answer::Count(guard(|| {
                reader.count_events_after(job, key, anchor, CHUNK_BUDGET)
            })),
            Purpose::Source { locator } => {
                Answer::Source(guard(|| reader.source_record(job, locator, CHUNK_BUDGET)))
            }
            Purpose::Details { key, anchor } => Answer::Details(guard(|| {
                reader.details_page(job, key, anchor.clone(), page_request, CHUNK_BUDGET)
            })),
            Purpose::Preview { key } => {
                Answer::Preview(guard(|| reader.hydrate(job, key, now, CHUNK_BUDGET)))
            }
            Purpose::PreviewTail { key, anchor } => Answer::Tail(guard(|| {
                reader.transcript_page(
                    job,
                    key,
                    anchor.clone(),
                    PageDirection::Backward,
                    page_request,
                    CHUNK_BUDGET,
                )
            })),
        };

        // Newer requests may have arrived while the adapter ran.
        self.drain_requests();
        self.running = None;
        if self.shutting_down {
            return None;
        }
        if stale(self, &load) || self.cancelled.remove(&request_id) {
            self.release(&load);
            return None;
        }

        // Progress of a still-pending slice, or the terminal outcome.
        enum Next {
            Done,
            Pending(WorkProgress),
            Retry,
            Failed(Option<ProviderError>),
        }
        let next = match answer {
            Answer::Page(result) => match result {
                Ok(ReadProgress::Ready(page)) => {
                    self.emit(WorkerEvent::Transcript { request_id, page });
                    Next::Done
                }
                Ok(ReadProgress::Pending(p)) => Next::Pending(p),
                Err(ProviderError::Unavailable(_)) => Next::Retry,
                Err(err) => Next::Failed(Some(err)),
            },
            Answer::Count(result) => match result {
                Ok(ReadProgress::Ready(count)) => {
                    self.emit(WorkerEvent::EventsAfter { request_id, count });
                    Next::Done
                }
                Ok(ReadProgress::Pending(p)) => Next::Pending(p),
                Err(ProviderError::Unavailable(_)) => Next::Retry,
                Err(_) => Next::Failed(None),
            },
            Answer::Source(result) => match result {
                Ok(ReadProgress::Ready(record)) => {
                    self.emit(WorkerEvent::Source { request_id, record });
                    Next::Done
                }
                Ok(ReadProgress::Pending(p)) => Next::Pending(p),
                Err(ProviderError::Unavailable(_)) => Next::Retry,
                Err(err) => Next::Failed(Some(err)),
            },
            Answer::Details(result) => match result {
                Ok(ReadProgress::Ready(page)) => {
                    self.emit(WorkerEvent::Details {
                        request_id,
                        key: key.clone(),
                        page,
                    });
                    Next::Done
                }
                Ok(ReadProgress::Pending(p)) => Next::Pending(p),
                Err(ProviderError::Unavailable(_)) => Next::Retry,
                Err(err) => Next::Failed(Some(err)),
            },
            Answer::Preview(result) => match result {
                Ok(delta) if delta.done => {
                    let summary = self.finish_hydration(delta.summary);
                    self.note_hydrated(&key);
                    self.emit(WorkerEvent::Preview {
                        request_id,
                        key: key.clone(),
                        summary,
                        done: true,
                    });
                    Next::Done
                }
                Ok(_) => Next::Pending(WorkProgress::default()),
                Err(ProviderError::Unavailable(_)) => Next::Retry,
                Err(err) => Next::Failed(Some(err)),
            },
            Answer::Tail(result) => match result {
                Ok(ReadProgress::Ready(page)) => {
                    self.emit(WorkerEvent::PreviewTail {
                        request_id,
                        key: key.clone(),
                        page,
                    });
                    Next::Done
                }
                Ok(ReadProgress::Pending(p)) => Next::Pending(p),
                Err(ProviderError::Unavailable(_)) => Next::Retry,
                Err(err) => Next::Failed(Some(err)),
            },
        };
        match next {
            Next::Done => None,
            Next::Pending(p) => {
                load.progress.records = load.progress.records.saturating_add(p.records);
                load.progress.expanded_bytes = load
                    .progress
                    .expanded_bytes
                    .saturating_add(p.expanded_bytes);
                self.emit(WorkerEvent::LoadProgress {
                    request_id,
                    records: load.progress.records,
                    expanded_bytes: load.progress.expanded_bytes,
                });
                load.retry_at = None;
                Some(load)
            }
            Next::Retry => {
                load.retry_at = Some(Instant::now() + RETRY_DELAY);
                Some(load)
            }
            Next::Failed(err) => {
                if let Some(err) = err {
                    let label = match &load.purpose {
                        Purpose::Source { .. } => "source",
                        Purpose::Preview { .. } => "hydrate",
                        Purpose::Details { .. } => "details",
                        Purpose::PreviewTail { .. } => "activity",
                        _ => "transcript",
                    };
                    let warning = failure_warning(key.harness, label, &err);
                    self.emit(WorkerEvent::Warning { warning });
                }
                self.emit(WorkerEvent::LoadFailed { request_id });
                None
            }
        }
    }

    /// The reader that published `key`; two readers of one harness (separate
    /// databases) are told apart by ownership. A key no reader has published
    /// falls back to the first reader of its harness.
    fn reader_index(&self, key: &SessionKey) -> Option<usize> {
        if let Some(i) = self.owners.get(key) {
            return Some(*i);
        }
        self.readers.iter().position(|r| r.harness() == key.harness)
    }

    // ---- discovery ------------------------------------------------------

    fn start_pass(&mut self, explicit: bool) {
        self.followup = false;
        self.refresh_requested = false;
        if self.config.process_enabled && self.observation.is_none() {
            self.sample_processes();
        }
        for state in &mut self.states {
            state.pending.clear();
        }
        self.pass = Some(Pass {
            explicit,
            done: vec![false; self.readers.len()],
            next: 0,
        });
    }

    /// One bounded discovery slice for the next unfinished reader.
    fn discover_slice(&mut self) {
        let Some(pass) = self.pass.as_mut() else {
            return;
        };
        let total = self.readers.len();
        let Some(i) = (0..total)
            .map(|n| (pass.next + n) % total)
            .find(|i| !pass.done[*i])
        else {
            self.finish_pass();
            return;
        };
        pass.next = (i + 1) % total.max(1);
        let harness = self.readers[i].harness();
        let now = now_ms();
        let result = guard(|| self.readers[i].discover(now, CHUNK_BUDGET));
        let scan = match result {
            Ok(delta) => {
                let scan = delta.scan;
                self.apply_delta(i, delta);
                scan
            }
            Err(err) => {
                let state = &mut self.states[i];
                // Keep last-good facts and the previously published warnings; only
                // the failure warning is (re)set.
                state.failure = Some(failure_warning(harness, "refresh", &err));
                state.pending.clear();
                let scan = ScanProgress {
                    harness,
                    scanned: 0,
                    total: None,
                    done: true,
                };
                self.index.record_scan(scan);
                scan
            }
        };
        if scan.done {
            if let Some(pass) = self.pass.as_mut() {
                pass.done[i] = true;
            }
            let state = &mut self.states[i];
            if state.failure.is_none() {
                state.warnings = std::mem::take(&mut state.pending);
            }
        }
        self.dirty = true;
        if self
            .pass
            .as_ref()
            .is_some_and(|p| p.done.iter().all(|d| *d))
        {
            self.finish_pass();
        }
    }

    fn finish_pass(&mut self) {
        let Some(pass) = self.pass.take() else {
            return;
        };
        if pass.explicit {
            self.generation += 1;
            self.index.freeze(self.generation);
        }
        self.publish();
        self.emit(WorkerEvent::RefreshFinished {
            generation: self.generation,
        });
    }

    fn apply_delta(&mut self, i: usize, delta: DiscoveryDelta) {
        let selected = self.selected.as_ref().map(|(_, k)| k.clone());
        let previewed = self.preview.as_ref().map(|(_, k)| k.clone());
        let harness = self.readers[i].harness();
        let shared = self
            .readers
            .iter()
            .filter(|r| r.harness() == harness)
            .count()
            > 1;
        let wants = |w: &Self, key: &SessionKey| {
            w.index.has_summary(key)
                || selected.as_ref() == Some(key)
                || previewed.as_ref() == Some(key)
        };
        let mut stale: Vec<SessionKey> = Vec::new();
        for meta in &delta.sessions {
            if shared {
                self.owners.insert(meta.key.clone(), i);
            }
            if wants(self, &meta.key) {
                stale.push(meta.key.clone());
            }
        }
        for key in &delta.removed {
            self.owners.remove(key);
            self.hydration.attempted.remove(key);
        }
        for key in &delta.invalidated {
            if selected.as_ref() == Some(key) {
                self.emit(WorkerEvent::TranscriptInvalidated { key: key.clone() });
            }
            if wants(self, key) {
                stale.push(key.clone());
            }
        }
        for key in stale {
            if self.hydration.queued.insert(key.clone()) {
                self.hydration.stale.push_back(key);
            }
        }
        let state = &mut self.states[i];
        state.failure = None;
        for warning in delta.warnings {
            state.pending.insert(warning.key.clone(), warning);
        }
        let delta = DiscoveryDelta {
            warnings: Vec::new(),
            ..delta
        };
        self.index.apply_discovery(delta);
        if let Some(limit) = self.index.limited() {
            let state = &mut self.states[i];
            state.limit = Some(Warning {
                key: WarningKey {
                    harness,
                    source: "metadata-limit".into(),
                    kind: WarningKind::Unsupported,
                },
                message: format!(
                    "Discovery limited: stopped at {} sources / {} bytes of metadata; \
                     raise max_metadata_sources or max_metadata_bytes to see more",
                    limit.sources, limit.bytes
                ),
                count: 1,
            });
        }
    }

    // ---- hydration ------------------------------------------------------

    /// The next automatic hydration target.
    fn next_target(&mut self) -> Option<SessionKey> {
        if !self.index.is_frozen() {
            return None;
        }
        let now = Instant::now();
        if let Some(at) = self.hydration.retry.iter().position(|(at, _)| *at <= now) {
            return Some(self.hydration.retry.remove(at).1);
        }
        while let Some(key) = self.hydration.stale.pop_front() {
            self.hydration.queued.remove(&key);
            if self.index.contains(&key) {
                return Some(key);
            }
        }
        while self.hydration.cursor < self.hydration.roots.len() {
            let key = self.hydration.roots[self.hydration.cursor].clone();
            self.hydration.cursor += 1;
            if self.hydration.attempted.contains(&key) || self.index.has_summary(&key) {
                continue;
            }
            return Some(key);
        }
        None
    }

    /// One bounded hydration slice. Returns whether work remains.
    fn hydrate_slice(&mut self) -> bool {
        let active = match self.hydration.current.take() {
            Some(active) => active,
            None => {
                let Some(key) = self.next_target() else {
                    return false;
                };
                let Some(reader) = self.reader_index(&key) else {
                    self.hydration.attempted.insert(key);
                    return true;
                };
                let job = self.new_job();
                Active { key, job, reader }
            }
        };
        let now = now_ms();
        let (key, job, i) = (active.key.clone(), active.job, active.reader);
        let result = guard(|| self.readers[i].hydrate(job, &key, now, CHUNK_BUDGET));
        if self.shutting_down {
            return false;
        }
        match result {
            Ok(delta) if delta.done => {
                self.hydration.attempted.insert(key.clone());
                self.finish_hydration(delta.summary);
                self.note_hydrated(&key);
            }
            Ok(_) => self.hydration.current = Some(active),
            Err(ProviderError::Unavailable(_)) => {
                self.hydration
                    .retry
                    .push((Instant::now() + RETRY_DELAY, key.clone()));
                self.hydration.attempted.insert(key);
            }
            Err(err) => {
                self.hydration.attempted.insert(key.clone());
                let warning = failure_warning(key.harness, "hydrate", &err);
                self.emit(WorkerEvent::Warning { warning });
            }
        }
        true
    }

    /// A session was hydrated. If a transcript of it is open, the adapter may have seen
    /// its already-readable content change; it reports that with the next discovery.
    fn note_hydrated(&mut self, key: &SessionKey) {
        if self.selected.as_ref().is_some_and(|(_, k)| k == key) {
            self.followup = true;
        }
    }

    /// Applies the latest process observation, caches the summary in the index, and
    /// returns it.
    fn finish_hydration(&mut self, mut summary: SessionSummary) -> Arc<SessionSummary> {
        self.associate(&mut summary);
        self.dirty = true;
        self.index.update_summary(summary)
    }

    fn associate(&self, summary: &mut SessionSummary) {
        if self.config.process_enabled
            && let Some(observation) = &self.observation
        {
            summary.process = processes::association_from_observation(summary, observation);
        }
    }

    // ---- processes ------------------------------------------------------

    fn sample_processes(&mut self) {
        let observation = (self.sampler)(now_ms());
        if let Ok(snapshot) = &observation {
            let snapshot = snapshot.clone();
            self.emit(WorkerEvent::Processes { snapshot });
        }
        self.observation = Some(observation);
        self.next_process = Instant::now() + self.process_every;
        // Re-associate the loaded, cached summaries against the new observation.
        let roots = self.index.snapshot();
        for summary in roots.summaries {
            let mut updated = (*summary).clone();
            self.associate(&mut updated);
            if updated.process != summary.process {
                self.index.update_summary(updated);
            }
        }
        self.dirty = true;
        self.publish();
    }

    // ---- publishing -----------------------------------------------------

    fn publish_if_due(&mut self) {
        if self.dirty && self.last_publish.elapsed() >= PUBLISH_EVERY {
            self.publish();
        }
    }

    fn warnings(&self) -> Vec<Warning> {
        let mut warnings: Vec<Warning> = Vec::new();
        for state in &self.states {
            let mut ws: Vec<Warning> = state.warnings.values().cloned().collect();
            ws.extend(state.failure.clone());
            ws.extend(state.limit.clone());
            ws.sort_by(|a, b| {
                a.key
                    .source
                    .cmp(&b.key.source)
                    .then(a.message.cmp(&b.message))
            });
            warnings.extend(ws);
        }
        warnings
    }

    fn publish(&mut self) {
        self.dirty = false;
        self.last_publish = Instant::now();
        let snapshot = self.index.snapshot();
        if self.index.is_frozen() && !Arc::ptr_eq(&snapshot.roots, &self.hydration.roots) {
            self.hydration.roots = snapshot.roots.clone();
            self.hydration.cursor = 0;
        }
        let warnings = self.warnings();
        self.emit(WorkerEvent::Roster { snapshot, warnings });
    }

    /// Send with backpressure: wait for queue space but give up on shutdown or
    /// when the UI side is gone, so a stalled consumer can never wedge `join`.
    fn emit(&mut self, mut event: WorkerEvent) {
        loop {
            if self.stop.load(Ordering::SeqCst) {
                self.shutting_down = true;
                return;
            }
            match self.events.try_send(event) {
                Ok(()) => return,
                Err(TrySendError::Disconnected(_)) => {
                    self.shutting_down = true;
                    return;
                }
                Err(TrySendError::Full(e)) => {
                    event = e;
                    thread::sleep(BACKPRESSURE_POLL);
                }
            }
        }
    }
}
