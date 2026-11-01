//! Pure application state for the roster (spec §10.2–§10.4).
//!
//! No rendering, no I/O, no clock: the caller supplies `now`. Selection is
//! tracked by [`SessionKey`], so roster updates never move it.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::domain::{
    AggregationClass, Harness, PageAnchor, ReleaseState, ScanProgress, SessionDetailPage,
    SessionKey, SessionLink, SessionMetadata, SessionSummary, SourceKey, Status, StatusLabel,
    Thresholds, Timestamp, ToolInteraction, TranscriptPage, Warning,
};
use crate::providers::WorkProgress;
use crate::roster::{DiscoveryLimit, RosterSnapshot, cmp_keys};
use crate::status::synthesize;
use crate::worker::WorkerRequest;

mod input;
mod overlay;
mod preview;
mod roster;
mod search;
mod transcript;
pub use input::{FilterItem, FilterTarget, Panel, ProcessView, SearchScope};
pub use overlay::{MAX_SOURCE_BYTES, SourceState, SourceView};
pub use preview::{DetailsState, PreviewState};
pub use roster::{
    GroupHeader, GroupTotals, LoadFocus, PREVIEW_SPLIT_WIDTH, RosterItem, RosterRows, RosterSession,
};
pub use transcript::{MAX_LOADED_EVENTS, PageKind, Transcript};

/// Every harness identity, in stable display order.
const PROVIDERS: [Harness; 6] = Harness::ALL;

const STATUS_ORDER: [StatusLabel; 9] = [
    StatusLabel::Active,
    StatusLabel::Recent,
    StatusLabel::Quiet,
    StatusLabel::PossiblyStalled,
    StatusLabel::Complete,
    StatusLabel::Failed,
    StatusLabel::Cancelled,
    StatusLabel::Unavailable,
    StatusLabel::Unknown,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    LastActivity,
    Provider,
    Tokens,
    Status,
}

impl SortMode {
    /// Order used by `s` to cycle (spec §10.4).
    const CYCLE: [SortMode; 4] = [
        SortMode::LastActivity,
        SortMode::Provider,
        SortMode::Tokens,
        SortMode::Status,
    ];

    /// Newest activity and largest token counts first; names and states ascending.
    fn default_ascending(self) -> bool {
        !matches!(self, SortMode::LastActivity | SortMode::Tokens)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum UsageAvailability {
    #[default]
    Any,
    /// A reported token total exists.
    Known,
    /// No token total is available.
    Unknown,
    /// A cost figure exists.
    HasCost,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FilterState {
    pub providers: Option<Vec<Harness>>,
    pub statuses: Option<Vec<StatusLabel>>,
    pub usage: UsageAvailability,
    pub project: Option<String>,
    pub query: String,
    /// Sessions a desktop app marked released stay hidden until explicitly shown;
    /// nothing is deleted or rewritten either way.
    pub include_released: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Screen {
    #[default]
    Roster,
    Inspector,
    Details,
    Source,
    Diagnostics,
    Session,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Roster,
    Search,
    Filter,
}

pub struct RosterRow<'a> {
    pub session: &'a SessionSummary,
    pub status: Status,
    /// Detailed facts have been read. A row without them is metadata only: its usage
    /// and measured facts are Unknown (still loading), not reported zeros.
    pub hydrated: bool,
}

/// Where discovery and paging stand, as the last snapshot reported them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RosterStatus {
    pub generation: u64,
    pub scan: Vec<ScanProgress>,
    pub has_more: bool,
    pub limited: Option<DiscoveryLimit>,
    pub refresh_available: bool,
    pub provisional: bool,
}

/// Child metadata of one family loaded so far.
#[derive(Clone, Debug, Default)]
pub struct ChildList {
    pub metadata: Vec<Arc<SessionMetadata>>,
    /// Parallel to `metadata`, recomputed with the clock.
    statuses: Vec<Status>,
    /// Parallel visibility under the active filter/search, cached on state change.
    visible: Vec<bool>,
    /// Cursor for the next batch; `None` once the family is complete.
    pub next: Option<SessionKey>,
}

pub struct App {
    now: Timestamp,
    thresholds: Thresholds,
    sessions: Vec<Arc<SessionSummary>>,
    /// Parallel to `sessions`: detailed facts have been read.
    hydrated: Vec<bool>,
    /// Parallel to `sessions`.
    statuses: Vec<Status>,
    /// Indices into `sessions` that pass the filter, in display order.
    view: Vec<usize>,
    /// Project groups over `view`, with their loaded-member totals. Rebuilt with
    /// the view so the row iterator only borrows it.
    groups: Vec<roster::GroupInfo>,
    selected: Option<SessionKey>,
    /// Cached visual position of `selected`, including Load more rows.
    visual_selected: Option<usize>,
    sort: SortMode,
    ascending: bool,
    filter: FilterState,
    warnings: Vec<Warning>,
    roster: RosterStatus,
    /// Frozen root positions of the applied snapshot; the default activity view keeps
    /// this group order (a newest child promotes its parent) instead of re-sorting rows.
    frozen_order: HashMap<SessionKey, usize>,
    /// Metadata-only placeholders, reused while the metadata `Arc` is unchanged.
    placeholders: HashMap<SessionKey, (Arc<SessionMetadata>, Arc<SessionSummary>)>,
    children: HashMap<SessionKey, ChildList>,
    /// Placeholder summaries for loaded children (metadata only until hydrated).
    child_summaries: HashMap<SessionKey, Arc<SessionSummary>>,
    /// Loaded children whose detailed facts arrived.
    child_hydrated: HashSet<SessionKey>,
    /// Parent of each loaded child key, for selection and toggling.
    child_parent: HashMap<SessionKey, SessionKey>,
    /// Families whose children are shown in the roster.
    expanded_children: HashSet<SessionKey>,
    /// Fast facts for the row under the cursor.
    preview: PreviewState,
    /// Generation the preview was requested in.
    preview_generation: u64,
    /// Progress of cached preview facts while hydration continues.
    preview_progress: WorkProgress,
    /// Paged detailed facts of the open session.
    details: DetailsState,
    /// Outstanding child requests by ID, with the generation they were asked in.
    child_requests: HashMap<u64, (SessionKey, u64)>,
    /// Responsive roster state: column scroll, preview pane, Load more focus.
    roster_view: roster::RosterView,
    pub screen: Screen,
    pub focus: Focus,
    pub live: bool,
    transcript: Option<Transcript>,
    request_seq: u64,
    outbox: Vec<WorkerRequest>,
    overlays: overlay::Overlays,
    input: input::InputState,
}

impl App {
    pub fn new(now: Timestamp, thresholds: Thresholds) -> Self {
        Self {
            now,
            thresholds,
            sessions: Vec::new(),
            hydrated: Vec::new(),
            statuses: Vec::new(),
            view: Vec::new(),
            groups: Vec::new(),
            selected: None,
            visual_selected: None,
            sort: SortMode::LastActivity,
            ascending: SortMode::LastActivity.default_ascending(),
            filter: FilterState::default(),
            warnings: Vec::new(),
            roster: RosterStatus::default(),
            frozen_order: HashMap::new(),
            placeholders: HashMap::new(),
            children: HashMap::new(),
            child_summaries: HashMap::new(),
            child_hydrated: HashSet::new(),
            child_parent: HashMap::new(),
            expanded_children: HashSet::new(),
            preview: PreviewState::Empty,
            preview_generation: 0,
            preview_progress: WorkProgress::default(),
            details: DetailsState::default(),
            child_requests: HashMap::new(),
            roster_view: roster::RosterView::default(),
            screen: Screen::default(),
            focus: Focus::default(),
            live: true,
            transcript: None,
            request_seq: 0,
            outbox: Vec::new(),
            overlays: overlay::Overlays::default(),
            input: input::InputState::default(),
        }
    }

    /// Replace the rows with already-hydrated summaries. Sessions are replaced as a
    /// set, the selection follows its key, and the warning set is replaced wholesale,
    /// which retires cleared keys.
    pub fn apply_roster(&mut self, sessions: Vec<SessionSummary>, warnings: Vec<Warning>) {
        let hydrated = vec![true; sessions.len()];
        self.frozen_order.clear();
        self.install(
            sessions.into_iter().map(Arc::new).collect(),
            hydrated,
            warnings,
        );
    }

    /// Apply a worker roster publication. Returns `false` (and changes nothing) for a
    /// generation older than one already applied. The loaded roots keep their frozen
    /// order; a root without a summary yet is a metadata-only row whose measured facts
    /// are Unknown, never a reported summary.
    pub fn apply_roster_snapshot(
        &mut self,
        snapshot: RosterSnapshot,
        warnings: Vec<Warning>,
    ) -> bool {
        if snapshot.generation < self.roster.generation {
            return false;
        }
        let generation_changed = snapshot.generation != self.roster.generation;
        if generation_changed {
            // Child pages and cursors belong to the generation they were read in.
            self.children.clear();
            self.child_summaries.clear();
            self.child_hydrated.clear();
            self.child_parent.clear();
            self.expanded_children.clear();
            self.child_requests.clear();
        }
        let summaries: HashMap<&SessionKey, &Arc<SessionSummary>> = snapshot
            .summaries
            .iter()
            .map(|s| (&s.meta.key, s))
            .collect();
        let mut sessions = Vec::with_capacity(snapshot.metadata.len());
        let mut hydrated = Vec::with_capacity(snapshot.metadata.len());
        let mut placeholders = HashMap::new();
        for meta in &snapshot.metadata {
            match summaries.get(&meta.key) {
                Some(summary) => {
                    sessions.push((*summary).clone());
                    hydrated.push(true);
                }
                None => {
                    // Preview facts already read for this row stay visible while
                    // the worker's summary cache is being rehydrated.
                    if let PreviewState::Ready { summary, .. } = &self.preview
                        && summary.meta.key == meta.key
                    {
                        sessions.push(summary.clone());
                        hydrated.push(true);
                        continue;
                    }
                    let placeholder = match self.placeholders.remove(&meta.key) {
                        Some((known, placeholder)) if Arc::ptr_eq(&known, meta) => placeholder,
                        _ => Arc::new(SessionSummary::unhydrated((**meta).clone())),
                    };
                    placeholders.insert(meta.key.clone(), (meta.clone(), placeholder.clone()));
                    sessions.push(placeholder);
                    hydrated.push(false);
                }
            }
        }
        self.placeholders = placeholders;
        self.frozen_order = snapshot
            .roots
            .iter()
            .enumerate()
            .map(|(i, k)| (k.clone(), i))
            .collect();
        self.roster = RosterStatus {
            generation: snapshot.generation,
            scan: snapshot.scan,
            has_more: snapshot.has_more,
            limited: snapshot.limited,
            refresh_available: snapshot.refresh_available,
            provisional: snapshot.provisional,
        };
        if generation_changed {
            // An answer computed against the previous generation cannot own the
            // new one: release it and let the next selection request fresh facts.
            if let Some(old) = self.preview.request_id() {
                self.outbox
                    .push(WorkerRequest::CancelLoad { request_id: old });
            }
            self.preview = PreviewState::Empty;
            self.preview_progress = WorkProgress::default();
            self.preview_generation = 0;
        }
        self.install(sessions, hydrated, warnings);
        true
    }

    fn install(
        &mut self,
        sessions: Vec<Arc<SessionSummary>>,
        hydrated: Vec<bool>,
        warnings: Vec<Warning>,
    ) {
        let previous = self.selected_root_position();
        self.sessions = sessions;
        self.hydrated = hydrated;
        self.warnings = warnings;
        self.rebuild(previous);
        self.probe_transcript();
    }

    /// Insert or replace one warning by key.
    pub fn apply_warning(&mut self, warning: Warning) {
        match self.warnings.iter_mut().find(|w| w.key == warning.key) {
            Some(slot) => *slot = warning,
            None => self.warnings.push(warning),
        }
    }

    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Sum of occurrence counts across all warning keys.
    pub fn warning_total(&self) -> u32 {
        self.warnings.iter().map(|w| w.count).sum()
    }

    /// Discovery and paging state of the applied generation.
    pub fn roster_status(&self) -> &RosterStatus {
        &self.roster
    }

    /// Progress of the first harness whose discovery is still running.
    pub fn scan(&self) -> Option<ScanProgress> {
        self.roster.scan.iter().copied().find(|s| !s.done)
    }

    pub fn generation(&self) -> u64 {
        self.roster.generation
    }

    /// Forget the applied worker generation. Call when the worker is replaced:
    /// a new worker numbers its publications from 1 again.
    pub fn reset_generation(&mut self) {
        self.roster = RosterStatus::default();
        self.children.clear();
        self.child_summaries.clear();
        self.child_hydrated.clear();
        self.child_parent.clear();
        self.expanded_children.clear();
        self.child_requests.clear();
        self.preview = PreviewState::Empty;
        self.preview_generation = 0;
        self.details.reset();
        self.visual_selected = None;
        self.rebuild_groups();
    }

    /// Ask the worker for the next 50 top-level groups of the applied generation.
    /// Returns `false` when nothing is left to load.
    pub fn load_more_roots(&mut self) -> bool {
        if !self.roster.has_more || self.roster.provisional {
            return false;
        }
        self.outbox.push(WorkerRequest::LoadRosterMore {
            generation: self.roster.generation,
        });
        true
    }

    /// Ask the worker for the next batch of `parent`'s children.
    pub fn load_children(&mut self, parent: &SessionKey) -> bool {
        let cursor = match self.children.get(parent) {
            Some(list) if list.next.is_none() => return false,
            Some(list) => list.next.clone(),
            None => None,
        };
        let request_id = self.next_request_id();
        self.child_requests
            .insert(request_id, (parent.clone(), self.roster.generation));
        self.outbox.push(WorkerRequest::LoadChildren {
            request_id,
            parent: parent.clone(),
            cursor,
        });
        true
    }

    /// Merge one batch of child metadata. A batch continues the family only when it
    /// follows the cursor already held, so a stale reply changes nothing.
    pub fn apply_children(
        &mut self,
        request_id: u64,
        parent: &SessionKey,
        metadata: Vec<Arc<SessionMetadata>>,
        next: Option<SessionKey>,
    ) -> bool {
        // Only the reply to an outstanding request of the current generation counts.
        match self.child_requests.remove(&request_id) {
            Some((asked, generation))
                if asked == *parent && generation == self.roster.generation => {}
            _ => return false,
        }
        let previous = self.selected_root_position();
        let now = self.now;
        let thresholds = self.thresholds;
        let known: HashSet<SessionKey> = self
            .children
            .get(parent)
            .map(|l| l.metadata.iter().map(|m| m.key.clone()).collect())
            .unwrap_or_default();
        for meta in metadata {
            if known.contains(&meta.key) {
                continue;
            }
            self.child_parent.insert(meta.key.clone(), parent.clone());
            let summary = self
                .child_summaries
                .entry(meta.key.clone())
                .or_insert_with(|| Arc::new(SessionSummary::unhydrated((*meta).clone())))
                .clone();
            let status = synthesize(summary.as_ref(), now, &thresholds);
            let list = self.children.entry(parent.clone()).or_default();
            list.metadata.push(meta);
            list.statuses.push(status);
            list.visible.push(false);
        }
        if let Some(list) = self.children.get_mut(parent) {
            list.next = next;
        }
        self.rebuild(previous);
        true
    }

    /// Child metadata of `parent` loaded so far.
    pub fn children_of(&self, parent: &SessionKey) -> Option<&ChildList> {
        self.children.get(parent)
    }

    /// Ask for the next child page of `parent`. Also the explicit action of a
    /// child Load more row.
    pub fn load_more_children(&mut self, parent: &SessionKey) -> bool {
        self.load_children(parent)
    }

    /// Show or hide the loaded children of the selected session. Expanding a family
    /// whose first page was never read asks for it; it never walks further pages.
    pub fn toggle_children(&mut self) -> bool {
        let Some(key) = self.selected.clone() else {
            return false;
        };
        let parent = self.child_parent.get(&key).cloned().unwrap_or(key);
        if !self.expanded_children.remove(&parent) {
            self.expanded_children.insert(parent.clone());
            if !self.children.contains_key(&parent) {
                self.load_children(&parent);
            }
        }
        let previous = self.selected_root_position();
        self.rebuild(previous);
        true
    }

    /// The selected family's children are shown.
    pub fn children_expanded(&self, parent: &SessionKey) -> bool {
        self.expanded_children.contains(parent)
    }

    /// Ask the worker to freeze a new discovery generation. This is the only way
    /// new sources, deletions, or late edges enter the loaded tree.
    pub fn refresh_roster(&mut self) -> bool {
        self.outbox.push(WorkerRequest::Refresh);
        true
    }

    /// Fast facts for the row under the cursor.
    pub fn preview(&self) -> &PreviewState {
        &self.preview
    }

    /// Progress of cached preview facts while hydration continues.
    pub fn preview_progress(&self) -> WorkProgress {
        match &self.preview {
            PreviewState::Loading { progress, .. } => *progress,
            _ => self.preview_progress,
        }
    }

    /// Detailed pages accumulated for the open session.
    pub fn details(&self) -> &DetailsState {
        &self.details
    }

    /// Schedule a preview only when the identity or generation changed, or when
    /// `retry` explicitly asks after a failure. Safe to call every frame.
    pub fn select_session(&mut self, key: &SessionKey) -> bool {
        self.select_session_with(key, false)
    }

    /// `r`/explicit retry: request the preview again even for the same row.
    pub fn retry_preview(&mut self) -> bool {
        let Some(key) = self.selected.clone() else {
            return false;
        };
        self.select_session_with(&key, true)
    }

    fn select_session_with(&mut self, key: &SessionKey, retry: bool) -> bool {
        let same = self.preview.key() == Some(key);
        let generation_changed = self.preview_generation != self.roster.generation;
        if same && !retry && !generation_changed {
            return false;
        }
        if !same {
            self.roster_view.reset_preview_scroll();
        }
        if let Some(id) = self.preview.request_id() {
            // Only the abandoned preview job is released; other purposes reading
            // the same session keep their own job and continuation.
            self.outbox
                .push(WorkerRequest::CancelLoad { request_id: id });
        }
        let request_id = self.next_request_id();
        self.preview = PreviewState::Loading {
            request_id,
            key: key.clone(),
            progress: WorkProgress::default(),
        };
        self.preview_progress = WorkProgress::default();
        self.preview_generation = self.roster.generation;
        self.outbox.push(WorkerRequest::HydratePreview {
            request_id,
            key: key.clone(),
        });
        true
    }

    /// Merge a preview response. Only the request that the current preview expects
    /// may change it; a late answer from a competing selection is ignored.
    pub fn apply_preview(
        &mut self,
        request_id: u64,
        key: SessionKey,
        summary: Arc<SessionSummary>,
        done: bool,
    ) -> bool {
        if summary.meta.key != key {
            return false;
        }
        if self.preview.key() != Some(&key) || self.preview.request_id() != Some(request_id) {
            return false;
        }
        self.install_summary(summary.clone());
        self.preview_progress = WorkProgress::default();
        self.preview = PreviewState::Ready {
            request_id,
            key,
            summary,
            complete: done,
        };
        true
    }

    /// Track preview/detail work reported for a request still in flight.
    pub fn apply_load_progress(&mut self, request_id: u64, progress: WorkProgress) -> bool {
        if let PreviewState::Loading {
            request_id: id,
            progress: slot,
            ..
        } = &mut self.preview
            && *id == request_id
        {
            *slot = progress;
            return true;
        }
        if self.preview.request_id() == Some(request_id) {
            self.preview_progress = progress;
            return true;
        }
        self.details.apply_progress(request_id, progress)
    }

    /// A request failed for good. Only the matching purpose and in-flight request
    /// changes; superseded work stays ignored.
    pub fn load_failed(&mut self, request_id: u64) {
        self.transcript_load_failed(request_id);
        if self.preview.request_id() == Some(request_id)
            && let Some(key) = self.preview.key().cloned()
        {
            self.preview = PreviewState::Failed { key };
        }
        self.details.fail(request_id);
    }

    /// Request the newest page of the open session's detailed facts.
    pub fn request_details(&mut self) -> bool {
        let Some(key) = self.transcript.as_ref().map(|t| t.key().clone()) else {
            return false;
        };
        if self.details.key() == Some(&key)
            && (self.details.loading().is_some() || self.details.complete())
        {
            return false;
        }
        self.begin_details(key, None)
    }

    /// Request the next (older) detailed page while one is loaded.
    pub fn load_more_details(&mut self) -> bool {
        let Some(key) = self.details.key().cloned() else {
            return false;
        };
        let Some(anchor) = self.details.next_anchor() else {
            return false;
        };
        if self.details.has_pending() {
            return false;
        }
        self.begin_details(key, Some(anchor))
    }

    fn begin_details(&mut self, key: SessionKey, anchor: Option<PageAnchor>) -> bool {
        if let Some(old) = self.details.loading() {
            self.outbox
                .push(WorkerRequest::CancelLoad { request_id: old });
        }
        let request_id = self.next_request_id();
        self.details
            .begin(request_id, key.clone(), anchor.is_none());
        self.outbox.push(WorkerRequest::LoadDetails {
            request_id,
            key,
            anchor,
        });
        true
    }

    /// Merge one detailed page. Only the page expected for the open session and
    /// request id counts; stale replies change nothing.
    pub fn apply_details(
        &mut self,
        request_id: u64,
        key: SessionKey,
        page: SessionDetailPage,
    ) -> bool {
        self.details.apply(request_id, &key, page)
    }

    /// Borrowed loaded roster rows: roots in frozen order, an expanded family's
    /// loaded children, and explicit Load more rows.
    pub fn roster_rows(&self) -> RosterRows<'_> {
        RosterRows::new(self)
    }

    pub(super) fn hydrated_child(&self, key: &SessionKey) -> bool {
        self.child_hydrated.contains(key)
    }

    /// The active filter/search narrows loaded rows (and their loaded children).
    pub(super) fn filter_active(&self) -> bool {
        let f = &self.filter;
        !f.query.trim().is_empty()
            || f.providers.is_some()
            || f.statuses.is_some()
            || f.project.is_some()
            || f.usage != UsageAvailability::Any
            || f.include_released
    }

    pub(super) fn child_matches(&self, list: &ChildList, index: usize) -> bool {
        list.visible.get(index).copied().unwrap_or(false)
    }

    fn install_summary(&mut self, summary: Arc<SessionSummary>) {
        let key = summary.meta.key.clone();
        let previous = self.selected_root_position();
        if let Some(i) = self.sessions.iter().position(|s| s.meta.key == key) {
            self.sessions[i] = summary.clone();
            self.hydrated[i] = true;
        } else if self.child_summaries.contains_key(&key) {
            self.child_summaries.insert(key.clone(), summary.clone());
            self.child_hydrated.insert(key.clone());
        }
        if let Some((_, placeholder)) = self.placeholders.get_mut(&key) {
            *placeholder = summary;
        }
        self.rebuild(previous);
    }

    /// The injected clock value used for status and age display.
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// Advance the injected clock; statuses and status filters are recomputed.
    pub fn set_now(&mut self, now: Timestamp) {
        let previous = self.selected_root_position();
        self.now = now;
        self.rebuild(previous);
    }

    pub fn total_sessions(&self) -> usize {
        self.sessions.len()
    }

    /// Discovered sessions per harness identity (unfiltered), including loaded
    /// children, in fixed six-harness order.
    pub fn provider_counts(&self) -> Vec<(Harness, usize)> {
        PROVIDERS
            .iter()
            .map(|&p| {
                let roots = self
                    .sessions
                    .iter()
                    .filter(|s| s.meta.key.harness == p)
                    .count();
                let children = self
                    .child_summaries
                    .keys()
                    .filter(|k| k.harness == p)
                    .count();
                (p, roots + children)
            })
            .collect()
    }

    /// Discovered sessions per synthesized state (unfiltered), in fixed order.
    pub fn status_counts(&self) -> Vec<(StatusLabel, usize)> {
        STATUS_ORDER
            .iter()
            .map(|&l| (l, self.statuses.iter().filter(|s| s.label == l).count()))
            .collect()
    }

    pub fn filter(&self) -> &FilterState {
        &self.filter
    }

    pub fn set_filter(&mut self, filter: FilterState) {
        let previous = self.selected_root_position();
        self.filter = filter;
        self.rebuild(previous);
    }

    pub fn set_query(&mut self, query: &str) {
        let previous = self.selected_root_position();
        self.filter.query = query.to_owned();
        self.rebuild(previous);
    }

    pub fn sort_mode(&self) -> SortMode {
        self.sort
    }

    pub fn sort_ascending(&self) -> bool {
        self.ascending
    }

    /// Select `mode`; selecting the active mode again reverses direction.
    pub fn sort(&mut self, mode: SortMode) {
        let previous = self.selected_root_position();
        if mode == self.sort {
            self.ascending = !self.ascending;
        } else {
            self.sort = mode;
            self.ascending = mode.default_ascending();
        }
        self.rebuild(previous);
    }

    /// Advance to the next sort mode in cycle order (spec §10.4).
    pub fn cycle_sort(&mut self) {
        let at = SortMode::CYCLE
            .iter()
            .position(|&m| m == self.sort)
            .unwrap_or(0);
        self.sort(SortMode::CYCLE[(at + 1) % SortMode::CYCLE.len()]);
    }

    pub fn visible_rows(&self) -> Vec<RosterRow<'_>> {
        self.view.iter().map(|&i| self.row(i)).collect()
    }

    /// Every discovered session, ignoring filter and sort.
    pub fn all_rows(&self) -> Vec<RosterRow<'_>> {
        (0..self.sessions.len()).map(|i| self.row(i)).collect()
    }

    /// One session by key, with its synthesized status. Loaded children resolve
    /// too, with their metadata-only or hydrated facts.
    pub fn session(&self, key: &SessionKey) -> Option<RosterRow<'_>> {
        if let Some(i) = self.sessions.iter().position(|s| &s.meta.key == key) {
            return Some(self.row(i));
        }
        let parent = self.child_parent.get(key)?;
        let list = self.children.get(parent)?;
        let i = list.metadata.iter().position(|m| &m.key == key)?;
        Some(RosterRow {
            session: self.child_summaries.get(key)?,
            status: list.statuses.get(i)?.clone(),
            hydrated: self.child_hydrated.contains(key),
        })
    }

    // ---- inspector and transcript (spec §10.3) ---------------------------

    pub fn transcript(&self) -> Option<&Transcript> {
        self.transcript.as_ref()
    }

    /// Requests for the worker queued by inspector actions. The UI loop sends
    /// them; `App` itself never touches I/O.
    pub fn take_requests(&mut self) -> Vec<WorkerRequest> {
        std::mem::take(&mut self.outbox)
    }

    /// Open the inspector for the selected roster row and request its oldest
    /// page. Returns `false` when nothing is selected.
    pub fn open_inspector(&mut self) -> bool {
        let Some(key) = self.selected.clone() else {
            return false;
        };
        self.start_transcript(key, None);
        self.screen = Screen::Inspector;
        true
    }

    pub fn close_inspector(&mut self) {
        self.abandon_details();
        self.transcript = None;
        self.overlays = overlay::Overlays::default();
        self.screen = Screen::Roster;
    }

    /// Restart the open transcript from its oldest page, keeping expansion.
    /// Used when the worker is replaced (its selection state is gone).
    pub fn restart_transcript(&mut self) {
        if let Some(old) = self.transcript.take() {
            let key = old.key().clone();
            self.start_transcript(key, Some(old));
        }
    }

    /// The worker reported that `key` changed in place. Re-read the loaded
    /// range and keep the selected event, so live activity never moves the
    /// reading position. This is not a new event.
    pub fn invalidate_transcript(&mut self, key: &SessionKey) {
        if self.transcript.as_ref().is_none_or(|t| t.key() != key) {
            return;
        }
        let id = self.next_request_id();
        let request = self.transcript.as_mut().and_then(|t| t.reload_request(id));
        match request {
            Some(request) => self.outbox.push(request),
            // The first page has not arrived yet; ask for it again.
            None => self.restart_transcript(),
        }
    }

    /// Merge a worker page response. Stale or unrequested responses are ignored.
    pub fn apply_transcript(&mut self, request_id: u64, page: TranscriptPage) -> bool {
        let applied = self
            .transcript
            .as_mut()
            .is_some_and(|t| t.apply_page(request_id, page));
        if applied {
            self.revalidate_source();
            self.pump_transcript();
            self.probe_transcript();
        }
        applied
    }

    /// Apply a worker count of events after the newest loaded one.
    pub fn apply_events_after(&mut self, request_id: u64, count: usize) -> bool {
        self.transcript
            .as_mut()
            .is_some_and(|t| t.apply_count(request_id, count))
    }

    /// A page or count request failed for good. Only the matching in-flight
    /// request is aborted; failures of superseded requests are ignored.
    pub fn transcript_load_failed(&mut self, request_id: u64) {
        if let Some(t) = &mut self.transcript {
            t.fail_request(request_id);
        }
        self.fail_source(request_id);
    }

    /// `r` in the inspector: retry a failed load.
    pub fn retry_transcript(&mut self) {
        let id = self.request_seq + 1;
        if let Some(t) = &mut self.transcript
            && let Some(request) = t.retry_request(id)
        {
            self.request_seq = id;
            self.outbox.push(request);
        }
    }

    pub fn inspector_move(&mut self, delta: isize) {
        if let Some(t) = &mut self.transcript {
            t.move_selection(delta);
        }
        self.pump_transcript();
    }

    /// `g`/`Home`: the oldest loaded event.
    pub fn inspector_first(&mut self) {
        if let Some(t) = &mut self.transcript {
            t.select_first();
        }
        self.pump_transcript();
    }

    /// `G`/`End`: always reload the newest page from the tail. Content can
    /// change without the row count growing (a paired tool result), so the
    /// window is never assumed current.
    pub fn inspector_last(&mut self) {
        let id = self.next_request_id();
        if let Some(t) = &mut self.transcript {
            let request = t.tail_request(id);
            self.outbox.push(request);
        }
    }

    pub fn inspector_toggle(&mut self) {
        if let Some(t) = &mut self.transcript {
            t.toggle_selected();
        }
    }

    /// Events persisted after the newest loaded one (`N new`).
    pub fn new_event_count(&self) -> usize {
        self.transcript.as_ref().map_or(0, Transcript::new_events)
    }

    fn next_request_id(&mut self) -> u64 {
        self.request_seq += 1;
        self.request_seq
    }

    /// Abandon detail work for a session that is no longer open. Only the
    /// abandoned job is cancelled; other purposes on that key keep running.
    fn abandon_details(&mut self) {
        if let Some(old) = self.details.loading() {
            self.outbox
                .push(WorkerRequest::CancelLoad { request_id: old });
        }
        self.details.reset();
    }

    fn start_transcript(&mut self, key: SessionKey, previous: Option<Transcript>) {
        self.abandon_details();
        let id = self.next_request_id();
        let summary = self
            .sessions
            .iter()
            .find(|s| s.meta.key == key)
            .map(|s| (**s).clone());
        let mut transcript = Transcript::new(key, summary);
        if let Some(previous) = &previous {
            transcript.carry_over(previous);
        }
        let request = transcript.select_request(id);
        self.transcript = Some(transcript);
        self.revalidate_source();
        self.outbox.push(request);
    }

    /// Queue the next edge page when the selection is near the window edge.
    fn pump_transcript(&mut self) {
        let id = self.request_seq + 1;
        if let Some(t) = &mut self.transcript
            && let Some(request) = t.next_load(id)
        {
            self.request_seq = id;
            self.outbox.push(request);
        }
    }

    /// When the open session's summary changed, ask the worker how many events
    /// follow the newest loaded one. Counting goes through the adapter's
    /// `count_events_after`, so it works for every provider.
    fn probe_transcript(&mut self) {
        let Some(key) = self.transcript.as_ref().map(|t| t.key().clone()) else {
            return;
        };
        let Some(summary) = self.sessions.iter().find(|s| s.meta.key == key).cloned() else {
            return;
        };
        let id = self.request_seq + 1;
        if let Some(t) = &mut self.transcript
            && let Some(request) = t.count_request(id, &summary)
        {
            self.request_seq = id;
            self.outbox.push(request);
        }
    }

    pub fn selected_key(&self) -> Option<SessionKey> {
        self.selected.clone()
    }

    /// Position of the selection among the loaded session rows shown by
    /// [`Self::roster_rows`]. Load more rows are not session selections.
    pub fn selected_index(&self) -> Option<usize> {
        let key = self.selected.as_ref()?;
        let mut index = 0;
        for item in self.roster_rows() {
            if let RosterItem::Session(row) = item {
                if &row.session.meta.key == key {
                    return Some(index);
                }
                index += 1;
            }
        }
        None
    }

    /// Visual position including Load more rows, for scrolling and highlighting.
    /// Cached by roster rebuilds and selection changes; rendering never scans twice.
    pub fn visual_selected_index(&self) -> Option<usize> {
        self.visual_selected
    }

    /// Recompute the cached visual selection after a roster or selection change.
    pub(super) fn recompute_visual_selection(&mut self) {
        let key = self.selected.clone();
        self.visual_selected = key.and_then(|key| {
            self.roster_rows()
                .enumerate()
                .find_map(|(i, item)| match item {
                    RosterItem::Session(row) if row.session.meta.key == key => Some(i),
                    _ => None,
                })
        });
    }

    /// Whether any roster row (session or explicit Load more) is shown.
    pub fn has_visible_rows(&self) -> bool {
        !self.view.is_empty()
            || (self.roster.has_more && !self.roster.provisional && !self.filter_active())
    }

    /// Move by `delta` loaded session rows, clamped to the loaded range. Moving
    /// never loads more roots or children on its own.
    pub fn move_selection(&mut self, delta: isize) {
        self.roster_view.clear_load_focus();
        let keys = roster::session_keys(self);
        if keys.is_empty() {
            self.selected = None;
            return;
        }
        let current = self
            .selected
            .as_ref()
            .and_then(|key| keys.iter().position(|k| k == key))
            .unwrap_or(0) as isize;
        let last = keys.len() as isize - 1;
        let next = current.saturating_add(delta).clamp(0, last) as usize;
        let key = keys[next].clone();
        self.selected = Some(key.clone());
        self.recompute_visual_selection();
        self.select_session(&key);
    }

    /// Resolved parent link for a session (spec §6.3): the most recent source
    /// among duplicate IDs, or `Ambiguous` when the newest tie.
    pub fn resolved_parent(&self, key: &SessionKey) -> Option<SessionLink> {
        let session = self.sessions.iter().find(|s| &s.meta.key == key)?;
        let link = session.meta.lineage.parent.as_ref()?;
        Some(self.resolve_link(&session.meta.key, link))
    }

    pub fn resolved_fork(&self, key: &SessionKey) -> Option<SessionLink> {
        let session = self.sessions.iter().find(|s| &s.meta.key == key)?;
        let link = session.meta.lineage.fork.as_ref()?;
        Some(self.resolve_link(&session.meta.key, link))
    }

    /// Resolved native child a tool call recorded, if it recorded one. The link is
    /// only ever the provider's allowlisted metadata value, never parsed output.
    pub fn resolved_tool_child(
        &self,
        session: &SessionKey,
        tool: &ToolInteraction,
    ) -> Option<SessionLink> {
        let link = tool.child_session.as_ref()?;
        Some(self.resolve_link(session, link))
    }

    /// `o` in details: open the selected tool's confirmed child session. Only a
    /// resolved link to a session in this roster navigates; unresolved and
    /// ambiguous links are ignored rather than guessed.
    pub fn open_tool_child_session(&mut self) -> bool {
        if self.screen != Screen::Details {
            return false;
        }
        let link = self.transcript.as_ref().and_then(|transcript| {
            let event = transcript.selected_event()?;
            let tool = event.tool.as_ref()?;
            self.resolved_tool_child(&event.session, tool)
        });
        let Some(SessionLink::Resolved(key)) = link else {
            return false;
        };
        if self.session(&key).is_none() {
            return false;
        }
        self.overlays = overlay::Overlays::default();
        self.selected = Some(key);
        self.recompute_visual_selection();
        self.open_inspector()
    }

    fn resolve_link(&self, child: &SessionKey, link: &SessionLink) -> SessionLink {
        let (harness, id, source) = match link {
            SessionLink::Ambiguous => return SessionLink::Ambiguous,
            SessionLink::Resolved(target) => (target.harness, &target.native_id, &target.source),
            SessionLink::Unresolved(id) => (child.harness, id, &child.source),
        };
        // Database sessions only link within their own database; equal native
        // IDs in different databases are different sessions.
        let database = match source {
            SourceKey::DbRow { database, .. } => Some(database),
            SourceKey::File(_) => None,
        };
        let mut newest: Option<&SessionSummary> = None;
        let mut tied = false;
        for s in self.sessions.iter().filter(|s| {
            s.meta.key.harness == harness
                && &s.meta.key.native_id == id
                && database.is_none_or(|db| {
                    matches!(&s.meta.key.source, SourceKey::DbRow { database, .. } if database == db)
                })
        }) {
            match newest {
                None => newest = Some(s),
                Some(best) => match activity(s).cmp(&activity(best)) {
                    Ordering::Greater => {
                        newest = Some(s);
                        tied = false;
                    }
                    Ordering::Equal => tied = true,
                    Ordering::Less => {}
                },
            }
        }
        match newest {
            None => link.clone(),
            Some(_) if tied => SessionLink::Ambiguous,
            Some(s) => SessionLink::Resolved(s.meta.key.clone()),
        }
    }

    fn row(&self, i: usize) -> RosterRow<'_> {
        RosterRow {
            session: &self.sessions[i],
            status: self.statuses[i].clone(),
            hydrated: self.hydrated[i],
        }
    }

    /// Position of the selected session's root in the filtered view. A selected
    /// child keeps its parent's position, so live updates do not jump.
    fn selected_root_position(&self) -> Option<usize> {
        let key = self.selected.as_ref()?;
        let root = self.child_parent.get(key).unwrap_or(key);
        self.view
            .iter()
            .position(|&i| &self.sessions[i].meta.key == root)
    }

    /// Recompute statuses, filter, and sort; keep the selection if it is still
    /// visible, else take the row now occupying its previous position.
    fn rebuild(&mut self, previous_position: Option<usize>) {
        self.statuses = self
            .sessions
            .iter()
            .map(|s| synthesize(s, self.now, &self.thresholds))
            .collect();
        let query = self.filter.query.trim().to_lowercase();
        let project = self.filter.project.as_deref().map(str::to_lowercase);
        // Loaded children keep their parent context usable as time passes, and
        // their visibility is cached here instead of being recomputed per frame.
        let now = self.now;
        let thresholds = self.thresholds;
        let filter = &self.filter;
        let summaries = &self.child_summaries;
        for list in self.children.values_mut() {
            list.visible.clear();
            for (meta, status) in list.metadata.iter().zip(list.statuses.iter_mut()) {
                let visible = match summaries.get(&meta.key) {
                    Some(summary) => {
                        *status = synthesize(summary.as_ref(), now, &thresholds);
                        summary_matches(filter, summary, status, &query, project.as_deref())
                    }
                    None => false,
                };
                list.visible.push(visible);
            }
        }

        let mut view: Vec<usize> = (0..self.sessions.len())
            .filter(|&i| {
                self.matches(i, &query, project.as_deref())
                    || self.any_child_matches(&self.sessions[i].meta.key)
            })
            .collect();
        let (sort, asc) = (self.sort, self.ascending);
        view.sort_by(|&a, &b| self.compare(a, b, sort, asc));
        self.view = view;
        self.rebuild_groups();

        // A selected child stays only while its actual row is included: its
        // parent must be visible and its own cached visibility true.
        let filtering = self.filter_active();
        let keep = self
            .selected
            .as_ref()
            .is_some_and(|k| self.view.iter().any(|&i| &self.sessions[i].meta.key == k))
            || self.selected.as_ref().is_some_and(|k| {
                self.child_parent.get(k).is_some_and(|p| {
                    self.children
                        .get(p)
                        .and_then(|list| {
                            list.metadata
                                .iter()
                                .position(|meta| &meta.key == k)
                                .map(|i| self.child_matches(list, i))
                        })
                        .unwrap_or(false)
                        && (filtering || self.expanded_children.contains(p))
                        && self.view.iter().any(|&i| &self.sessions[i].meta.key == p)
                })
            });
        if !keep {
            self.selected = if self.view.is_empty() {
                None
            } else {
                let at = previous_position.unwrap_or(0).min(self.view.len() - 1);
                Some(self.sessions[self.view[at]].meta.key.clone())
            };
        }
        self.recompute_visual_selection();
        // Whatever selection this rebuild settled on gets its preview, through the
        // same identity-deduplicated path as keyboard moves.
        if let Some(key) = self.selected.clone() {
            self.select_session(&key);
        }
    }

    /// Rebuild the project groups of the current view. A group starts at its
    /// first root in view order; later roots with the same project join it even
    /// when other projects sit between them. Totals count every loaded member,
    /// so the frame only borrows the result.
    fn rebuild_groups(&mut self) {
        let mut groups: Vec<roster::GroupInfo> = Vec::new();
        let mut indices: HashMap<Option<&str>, usize> = HashMap::new();
        for (position, root) in self.view.iter().enumerate() {
            let project = self.sessions[*root].meta.project.as_deref();
            let at = match indices.get(&project) {
                Some(&at) => at,
                None => {
                    indices.insert(project, groups.len());
                    groups.push(roster::GroupInfo {
                        project: project.map(str::to_owned),
                        roots: Vec::new(),
                        totals: roster::GroupTotals::default(),
                    });
                    groups.len() - 1
                }
            };
            groups[at].roots.push(position);
        }
        let more_roots = self.roster.has_more;
        for group in &mut groups {
            let totals = &mut group.totals;
            for &position in &group.roots {
                let root = self.view[position];
                let key = &self.sessions[root].meta.key;
                add_member(totals, &self.sessions[root]);
                if let Some(list) = self.children.get(key) {
                    for meta in &list.metadata {
                        match self.child_summaries.get(&meta.key) {
                            Some(summary) => add_member(totals, summary),
                            None => {
                                totals.sessions += 1;
                                totals.tokens_partial = true;
                                totals.cost_partial = true;
                            }
                        }
                    }
                    if list.next.is_some() {
                        totals.tokens_partial = true;
                        totals.cost_partial = true;
                    }
                }
            }
            if more_roots {
                // Roots beyond the loaded page may belong to any group.
                totals.tokens_partial = true;
                totals.cost_partial = true;
            }
        }
        self.groups = groups;
    }

    /// A loaded child of `parent` is visible under the active filter/search.
    fn any_child_matches(&self, parent: &SessionKey) -> bool {
        self.children
            .get(parent)
            .is_some_and(|list| list.visible.iter().any(|v| *v))
    }

    fn matches(&self, i: usize, query: &str, project: Option<&str>) -> bool {
        summary_matches(
            &self.filter,
            &self.sessions[i],
            &self.statuses[i],
            query,
            project,
        )
    }

    /// Missing values sort last in either direction; ties fall back to newest
    /// activity, then identity, independent of direction.
    fn compare(&self, a: usize, b: usize, mode: SortMode, asc: bool) -> Ordering {
        let (sa, sb) = (&self.sessions[a], &self.sessions[b]);
        let primary = match mode {
            SortMode::LastActivity => {
                match (
                    self.frozen_order.get(&sa.meta.key),
                    self.frozen_order.get(&sb.meta.key),
                ) {
                    (Some(pa), Some(pb)) => directed(
                        pa.cmp(pb),
                        asc == SortMode::LastActivity.default_ascending(),
                    ),
                    _ => opt_cmp(activity(sa), activity(sb), asc),
                }
            }
            SortMode::Provider => directed(sa.meta.key.harness.cmp(&sb.meta.key.harness), asc),
            SortMode::Tokens => opt_cmp(
                sa.usage.as_ref().and_then(|u| u.total),
                sb.usage.as_ref().and_then(|u| u.total),
                asc,
            ),
            SortMode::Status => directed(
                status_rank(self.statuses[a].label).cmp(&status_rank(self.statuses[b].label)),
                asc,
            ),
        };
        primary
            .then_with(|| opt_cmp(activity(sa), activity(sb), false))
            .then_with(|| cmp_keys(&sa.meta.key, &sb.meta.key))
    }
}

fn activity(s: &SessionSummary) -> Option<Timestamp> {
    [s.meta.activity.at, s.last_event_at, s.last_completion]
        .into_iter()
        .flatten()
        .max()
}

/// Add one loaded member's recorded facts to a group total. A missing value is
/// unknown, never zero: it only marks the totals partial.
fn add_member(totals: &mut roster::GroupTotals, summary: &SessionSummary) {
    totals.sessions += 1;
    let Some(usage) = summary.usage.as_ref() else {
        totals.tokens_partial = true;
        totals.cost_partial = true;
        return;
    };
    match usage.total {
        Some(total) => totals.tokens = Some(totals.tokens.unwrap_or(0).saturating_add(total)),
        None => totals.tokens_partial = true,
    }
    if usage.class == AggregationClass::Partial {
        totals.tokens_partial = true;
    }
    let cost = &usage.cost;
    match cost.usd {
        Some(usd) => {
            totals.cost_usd = Some(totals.cost_usd.unwrap_or(0.0) + usd);
            if cost.class == AggregationClass::Partial {
                totals.cost_partial = true;
            }
            if cost.class == AggregationClass::Estimated {
                totals.cost_estimated = true;
            }
        }
        // Only priced legs are known: the sum is a lower bound, not a total.
        None if cost.known_subtotal > 0.0 => {
            totals.cost_usd = Some(totals.cost_usd.unwrap_or(0.0) + cost.known_subtotal);
            totals.cost_partial = true;
            if cost.class == AggregationClass::Estimated {
                totals.cost_estimated = true;
            }
        }
        None => totals.cost_partial = true,
    }
}

/// Case-insensitive query plus the active filters for one summary. Shared by
/// roots and loaded children so a child-only match keeps its parent context.
fn summary_matches(
    f: &FilterState,
    s: &SessionSummary,
    status: &Status,
    query: &str,
    project: Option<&str>,
) -> bool {
    if s.meta.released == ReleaseState::Released && !f.include_released {
        return false;
    }
    if f.providers
        .as_ref()
        .is_some_and(|p| !p.contains(&s.meta.key.harness))
    {
        return false;
    }
    if f.statuses
        .as_ref()
        .is_some_and(|l| !l.contains(&status.label))
    {
        return false;
    }
    if let Some(p) = project
        && s.meta.project.as_deref().map(str::to_lowercase).as_deref() != Some(p)
    {
        return false;
    }
    let total = s.usage.as_ref().and_then(|u| u.total);
    let usage_ok = match f.usage {
        UsageAvailability::Any => true,
        UsageAvailability::Known => total.is_some(),
        UsageAvailability::Unknown => total.is_none(),
        UsageAvailability::HasCost => s.usage.as_ref().is_some_and(|u| u.cost.usd.is_some()),
    };
    if !usage_ok {
        return false;
    }
    if query.is_empty() {
        return true;
    }
    let contains = |text: &str| text.to_lowercase().contains(query);
    contains(&s.meta.title)
        || s.meta.project.as_deref().is_some_and(contains)
        || s.meta
            .cwd
            .as_ref()
            .is_some_and(|c| contains(&c.to_string_lossy()))
        || contains(&s.meta.key.native_id)
}

fn status_rank(label: StatusLabel) -> usize {
    STATUS_ORDER
        .iter()
        .position(|&l| l == label)
        .unwrap_or(STATUS_ORDER.len())
}

fn directed(ord: Ordering, asc: bool) -> Ordering {
    if asc { ord } else { ord.reverse() }
}

fn opt_cmp<T: Ord>(a: Option<T>, b: Option<T>, asc: bool) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => directed(a.cmp(&b), asc),
    }
}
