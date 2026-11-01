//! Loaded-transcript window for the inspector (spec §10.3).
//!
//! Pure state: no rendering, no I/O. The window is a contiguous run of events
//! assembled from pages. Each page response carries one continuation anchor
//! (the last event of a forward page, the first event of a backward page), so
//! the window remembers one *chunk* per page and only ever cuts at a chunk
//! edge whose anchor it knows. That keeps `older`/`newer` request anchors exact:
//! scrolling never duplicates or skips events, and a tool call and its result
//! (one `Event`) are never separated.

use std::collections::HashSet;

use crate::domain::{
    BoundedText, DiagnosticEntry, Event, PageAnchor, PageDirection, SessionKey, SessionSummary,
    SourceLocator, TranscriptPage,
};
use crate::worker::WorkerRequest;

use super::search::TranscriptSearch;
use super::turns::turn_rows;

/// Most events kept loaded; farther pages are dropped (spec §10.3). The worker
/// also never requests a page larger than this.
pub const MAX_LOADED_EVENTS: usize = 20_000;
/// Distance from a window edge at which the next page is requested.
const NEAR_EDGE: usize = 5;

/// Why a page was requested; decides how the response is merged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageKind {
    /// Oldest page, from `Select`. Replaces the window.
    Initial,
    /// Backward from the window start. Prepends.
    Older,
    /// Forward from the window end. Appends.
    Newer,
    /// Backward from the tail (`G`/`End`). Replaces the window.
    Tail,
    /// Re-read the loaded range after an in-place change, keeping the selected
    /// event. Pages are staged and swapped in once the selection is reached.
    Reload,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pending {
    request_id: u64,
    kind: PageKind,
}

/// The anchor a chunk's page reported, and which end of the chunk it names.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Edge {
    /// Anchor of the chunk's last event (forward page).
    Last(PageAnchor),
    /// Anchor of the chunk's first event (backward page).
    First(PageAnchor),
    Unknown,
}

#[derive(Clone, Debug)]
struct Chunk {
    len: usize,
    edge: Edge,
    diagnostics: Vec<DiagnosticEntry>,
}

/// A reload in progress, staged and not shown until the current selection (and
/// the old window start) has been re-read.
///
/// With newer pages beyond the window it first reads one page forward from the
/// window end: anchors are exclusive, so that page plus a backward read from its
/// last event re-reads the old last event too. Otherwise it reads backward from
/// the tail.
struct Reload {
    /// Staged window in display order.
    staged: Vec<Event>,
    chunks: Vec<Chunk>,
    /// Anchor for the next request: forward from the window end while
    /// `probing_end`, else backward (`None` = the tail).
    anchor: Option<PageAnchor>,
    probing_end: bool,
    /// Events staged from the forward end probe; they only anchor the backward
    /// read and neither count toward the window cap nor survive the swap.
    probe_len: usize,
    /// The old window's last event and its anchor.
    end_id: Option<String>,
    end_anchor: Option<PageAnchor>,
    /// Continuation for events newer than the staged window.
    newer: Option<PageAnchor>,
    /// Id of the old window's first event; re-reading it refreshes the boundary.
    boundary: Option<String>,
}

pub struct Transcript {
    key: SessionKey,
    events: Vec<Event>,
    chunks: Vec<Chunk>,
    selected: usize,
    /// `Some` while older events exist: the anchor to page backward from.
    older: Option<PageAnchor>,
    /// `Some` while newer events exist: the anchor to page forward from.
    newer: Option<PageAnchor>,
    pending: Option<Pending>,
    reload: Option<Reload>,
    /// The last load failed for good; `r` retries it.
    failed: Option<PageKind>,
    expanded: HashSet<String>,
    /// The latest row's event, expanded automatically while following. Dismissed
    /// by the reader's own toggle; never mixed into `expanded`. Only an opening
    /// at the tail, explicit tail navigation or an append moves it.
    auto_expanded: Option<String>,
    /// The reading position follows the newest row: set by a tail page, an
    /// append that reaches the newest end, or a selection landing on the last
    /// row of a window that reaches the tail; cleared by leaving that row.
    follow: bool,
    /// A positive count arrived while following but another load was in flight:
    /// the newest page still has to be re-read once that load settles.
    tail_owed: bool,
    /// Events counted after the newest observed event (`N new`).
    new_events: usize,
    /// Newest-observed boundary: an anchor and how many events were already
    /// known after it when the newest end was last loaded. Window-independent.
    mark: Option<(PageAnchor, usize)>,
    /// In-flight count request and the offset it is relative to.
    count_pending: Option<(u64, usize)>,
    /// The first count after opening away from the tail fixes the baseline.
    baseline_pending: bool,
    /// Roster event totals at opening and now; their difference is the arrivals
    /// the baseline count cannot see (providers that report a total).
    open_total: usize,
    latest_total: usize,
    /// A reload could not reach the reading position within the window cap.
    reload_capped: bool,
    /// Summary at the last probe; a different summary triggers a new count.
    probed: Option<SessionSummary>,
    /// Event id a restarted transcript should return to when it is still present
    /// in the re-read history. Opening a different session does not set it.
    resume: Option<String>,
    /// Events scanned while chasing `resume`; recovery is bounded by the window cap.
    resume_scanned: usize,
    /// True once any page has been applied.
    loaded: bool,
    search: TranscriptSearch,
}

impl Transcript {
    pub(super) fn new(key: SessionKey, probed: Option<SessionSummary>) -> Self {
        Self {
            key,
            events: Vec::new(),
            chunks: Vec::new(),
            selected: 0,
            older: None,
            newer: None,
            pending: None,
            reload: None,
            failed: None,
            expanded: HashSet::new(),
            auto_expanded: None,
            follow: false,
            tail_owed: false,
            new_events: 0,
            mark: None,
            count_pending: None,
            baseline_pending: false,
            open_total: probed.as_ref().map_or(0, |s| s.displayed_event_count),
            latest_total: probed.as_ref().map_or(0, |s| s.displayed_event_count),
            reload_capped: false,
            probed,
            resume: None,
            resume_scanned: 0,
            loaded: false,
            search: TranscriptSearch::default(),
        }
    }

    pub fn key(&self) -> &SessionKey {
        &self.key
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn selected_event(&self) -> Option<&Event> {
        self.events.get(self.selected)
    }

    /// Whether the row for `event` is expanded: by the reader, or because it is
    /// the latest row while following.
    pub fn is_expanded(&self, event: &Event) -> bool {
        self.auto_expanded.as_deref() == Some(event.id.as_str())
            || self.expanded.contains(&event.id)
    }

    /// The reading position follows the newest row: set by a tail page or by
    /// landing on the last row of a window that reaches the tail, cleared by
    /// moving off that row. Only while following does an arrival pull the newest
    /// page in on its own.
    pub fn following(&self) -> bool {
        self.follow
    }

    /// Diagnostics for the loaded chunks, in window order.
    pub fn diagnostics(&self) -> Vec<&DiagnosticEntry> {
        self.chunks
            .iter()
            .flat_map(|c| c.diagnostics.iter())
            .collect()
    }

    /// Whether `locator` names a record of this session that the loaded window
    /// (or its diagnostics) currently references. A locator of another session,
    /// or one from a superseded source generation, is never known.
    pub fn knows_locator(&self, locator: &SourceLocator) -> bool {
        if locator.session != self.key {
            return false;
        }
        let text_is =
            |t: &Option<BoundedText>| t.as_ref().and_then(|t| t.locator.as_ref()) == Some(locator);
        self.events.iter().any(|e| {
            &e.provenance.locator == locator
                || text_is(&e.text)
                || e.tool
                    .as_ref()
                    .is_some_and(|t| text_is(&t.input) || text_is(&t.output))
        }) || self
            .chunks
            .iter()
            .flat_map(|c| c.diagnostics.iter())
            .any(|d| &d.locator == locator)
    }

    pub fn has_older(&self) -> bool {
        self.older.is_some()
    }

    pub fn has_newer(&self) -> bool {
        self.newer.is_some()
    }

    pub fn loaded(&self) -> bool {
        self.loaded
    }

    /// The load in flight, for the "Loading…" indicator.
    pub fn loading(&self) -> Option<PageKind> {
        self.pending.map(|p| p.kind)
    }

    /// The kind of load that failed for good, if any.
    pub fn failed(&self) -> Option<PageKind> {
        self.failed
    }

    /// A reload could not reach the reading position within the window cap.
    pub fn reload_capped(&self) -> bool {
        self.reload_capped
    }

    /// Events persisted after the newest loaded event.
    pub fn new_events(&self) -> usize {
        self.new_events
    }

    /// Carry user-visible state (expansion, follow) and the reading position
    /// over to a replacement transcript. The event identity is resolved after
    /// the newest page arrives; when it moved beyond the window it is chased
    /// backward.
    pub(super) fn carry_over(&mut self, previous: &Transcript) {
        self.expanded = previous.expanded.clone();
        self.auto_expanded = previous.auto_expanded.clone();
        self.follow = previous.follow;
        self.search.set_query(previous.search.query());
        // A follower wants the new newest row, not the old anchor: only a reader
        // who was reading history resumes a position.
        self.resume = if previous.follow {
            None
        } else {
            previous.selected_event().map(|e| e.id.clone())
        };
        self.resume_scanned = 0;
    }

    /// The active transcript search query (empty when none).
    pub fn search_query(&self) -> &str {
        self.search.query()
    }

    pub(super) fn set_search_query(&mut self, query: &str) {
        self.search.set_query(query);
    }

    /// Loaded events matching the search (spec §10.4: loaded events only).
    pub fn match_count(&self) -> usize {
        self.search.matches(&self.events).len()
    }

    pub fn is_match(&self, index: usize) -> bool {
        self.search
            .matches(&self.events)
            .binary_search(&index)
            .is_ok()
    }

    /// Zero-based rank of the selected event among the matches, if it is one.
    pub fn match_position(&self) -> Option<usize> {
        self.search
            .matches(&self.events)
            .binary_search(&self.selected)
            .ok()
    }

    /// The match nearest `from` in the given direction, wrapping around.
    /// `inclusive` lets `from` itself match.
    pub(super) fn match_from(&self, from: usize, forward: bool, inclusive: bool) -> Option<usize> {
        let matches = self.search.matches(&self.events);
        if forward {
            let at = matches.partition_point(|&m| if inclusive { m < from } else { m <= from });
            matches.get(at).or_else(|| matches.first()).copied()
        } else {
            let at = matches.partition_point(|&m| if inclusive { m <= from } else { m < from });
            at.checked_sub(1)
                .and_then(|i| matches.get(i))
                .or_else(|| matches.last())
                .copied()
        }
    }

    /// Current window index of the event with `id`, if still loaded.
    pub(super) fn index_of(&self, id: &str) -> Option<usize> {
        self.events.iter().position(|e| e.id == id)
    }

    pub(super) fn select_index(&mut self, index: usize) {
        if !self.events.is_empty() {
            self.selected = index.min(self.events.len() - 1);
            self.settle_selection();
        }
    }

    fn begin(&mut self, request_id: u64, kind: PageKind) {
        self.pending = Some(Pending { request_id, kind });
        self.failed = None;
        self.reload_capped = false;
        if kind == PageKind::Tail {
            // The newest page is being loaded: a deferred catch-up is met.
            self.tail_owed = false;
        }
        if kind != PageKind::Reload {
            self.reload = None;
        }
    }

    pub(super) fn select_request(&mut self, request_id: u64) -> WorkerRequest {
        // Opening a transcript starts at its newest entry; `Select` loads the
        // newest page backward from the tail.
        self.begin(request_id, PageKind::Tail);
        WorkerRequest::Select {
            request_id,
            key: self.key.clone(),
        }
    }

    fn backward_request(&self, request_id: u64, anchor: Option<PageAnchor>) -> WorkerRequest {
        WorkerRequest::LoadPage {
            request_id,
            key: self.key.clone(),
            anchor,
            direction: PageDirection::Backward,
        }
    }

    /// Request the newest page. A new request supersedes any load in flight.
    pub(super) fn tail_request(&mut self, request_id: u64) -> WorkerRequest {
        self.begin(request_id, PageKind::Tail);
        self.backward_request(request_id, None)
    }

    /// Re-read the loaded range after an in-place change, keeping the selected
    /// event. Pages are read backward from the newest known end (the tail, or
    /// the window's last event) until both the selection and the old window
    /// start are re-read, so every event including the boundary is refreshed.
    /// Returns `None` before the first page has arrived.
    pub(super) fn reload_request(&mut self, request_id: u64) -> Option<WorkerRequest> {
        if !self.loaded {
            return None;
        }
        let probing_end = self.newer.is_some() && !self.events.is_empty();
        let anchor = if probing_end {
            self.newer.clone()
        } else {
            None
        };
        self.reload = Some(Reload {
            staged: Vec::new(),
            chunks: Vec::new(),
            anchor: anchor.clone(),
            probing_end,
            newer: None,
            probe_len: 0,
            end_id: if probing_end {
                self.events.last().map(|e| e.id.clone())
            } else {
                None
            },
            end_anchor: self.newer.clone(),
            boundary: self.events.first().map(|e| e.id.clone()),
        });
        self.begin(request_id, PageKind::Reload);
        if probing_end {
            Some(WorkerRequest::LoadPage {
                request_id,
                key: self.key.clone(),
                anchor,
                direction: PageDirection::Forward,
            })
        } else {
            Some(self.backward_request(request_id, None))
        }
    }

    /// Retry the load that failed for good. `None` when nothing failed or a
    /// load is already in flight.
    pub(super) fn retry_request(&mut self, request_id: u64) -> Option<WorkerRequest> {
        if self.pending.is_some() {
            return None;
        }
        let kind = self.failed?;
        match kind {
            PageKind::Initial | PageKind::Tail => {
                // Selecting again re-establishes both the worker selection and
                // the newest page load.
                self.begin(request_id, PageKind::Tail);
                Some(WorkerRequest::Select {
                    request_id,
                    key: self.key.clone(),
                })
            }
            // The reading position was out of reach: the user's retry is an
            // explicit jump to the newest page.
            PageKind::Reload if self.reload_capped => Some(self.tail_request(request_id)),
            PageKind::Reload => self.reload_request(request_id),
            PageKind::Older => {
                self.failed = None;
                let anchor = self.older.clone()?;
                self.begin(request_id, kind);
                Some(self.backward_request(request_id, Some(anchor)))
            }
            PageKind::Newer => {
                self.failed = None;
                let anchor = self.newer.clone()?;
                self.begin(request_id, kind);
                Some(WorkerRequest::LoadPage {
                    request_id,
                    key: self.key.clone(),
                    anchor: Some(anchor),
                    direction: PageDirection::Forward,
                })
            }
        }
    }

    /// A load or count request failed. Only the matching request is aborted;
    /// a superseded request's failure is ignored.
    pub(super) fn fail_request(&mut self, request_id: u64) {
        if let Some(pending) = self.pending.filter(|p| p.request_id == request_id) {
            self.pending = None;
            self.failed = Some(pending.kind);
            if pending.kind == PageKind::Reload {
                self.reload = None;
            }
        }
        if self.count_pending.is_some_and(|(id, _)| id == request_id) {
            self.count_pending = None;
        }
    }

    /// After a fresh tail load, return to the event a restarted transcript was
    /// reading. If it is older than the window, `next_load` walks backward.
    fn resume_selection(&mut self) {
        let Some(id) = self.resume.clone() else {
            return;
        };
        if let Some(i) = self.index_of(&id) {
            self.selected = i;
            self.resume = None;
        } else if self.older.is_none() {
            // The event no longer exists in the re-read history.
            self.resume = None;
        }
    }

    /// While chasing a restart anchor, replace the window with each older page
    /// instead of prepending it. The newest events are not needed on the way to
    /// an older reading position, and discarding them keeps recovery within the
    /// window cap instead of retaining the whole history.
    fn recover_older(&mut self, page: TranscriptPage, continuation: Option<PageAnchor>) {
        self.resume_scanned = self.resume_scanned.saturating_add(page.events.len());
        // Forward pages are exclusive: continue from the retained page's last
        // event so the next forward page starts with the discarded window's first
        // event, not after it.
        let newer = page.events.last().map(anchor_of_event);
        self.replace(page, PageDirection::Backward);
        self.older = continuation;
        self.newer = newer;
        self.selected = self.events.len().saturating_sub(1);
        if self.resume.is_some() {
            if let Some(i) = self.resume.as_ref().and_then(|id| self.index_of(id)) {
                self.selected = i;
                self.resume = None;
            } else if self.older.is_none() || self.resume_scanned >= MAX_LOADED_EVENTS {
                // The anchor is farther back than one window can hold, or it no
                // longer exists. Stay on the nearest reachable page and stop
                // walking; the user can page manually from here.
                self.resume = None;
                if self.events.len() > NEAR_EDGE * 2 {
                    self.selected = self.events.len() / 2;
                }
            }
        }
    }

    /// The page to fetch next, if a reload is mid-way, or the selection is near
    /// a window edge, and no load is in flight.
    pub(super) fn next_load(&mut self, request_id: u64) -> Option<WorkerRequest> {
        if self.pending.is_some() || !self.loaded || self.failed.is_some() {
            return None;
        }
        if let Some(anchor) = self.reload.as_ref().map(|r| r.anchor.clone()) {
            self.begin(request_id, PageKind::Reload);
            return Some(self.backward_request(request_id, anchor));
        }
        // A restarted transcript returns to its reading anchor, one page at a time.
        if self.resume.is_some() {
            if self.resume_scanned >= MAX_LOADED_EVENTS {
                self.resume = None;
                return None;
            }
            if let Some(anchor) = self.older.clone() {
                self.begin(request_id, PageKind::Older);
                return Some(self.backward_request(request_id, Some(anchor)));
            }
            self.resume = None;
        }
        let (kind, anchor, direction) = if self.selected < NEAR_EDGE && self.older.is_some() {
            (PageKind::Older, self.older.clone(), PageDirection::Backward)
        } else if self.selected + NEAR_EDGE >= self.events.len() && self.newer.is_some() {
            (PageKind::Newer, self.newer.clone(), PageDirection::Forward)
        } else {
            return None;
        };
        self.begin(request_id, kind);
        Some(WorkerRequest::LoadPage {
            request_id,
            key: self.key.clone(),
            anchor,
            direction,
        })
    }

    /// Ask for the number of events after the newest-observed boundary when
    /// `summary` differs from the last probe. Works while older pages are shown
    /// and while page loads are in flight: the boundary is independent of the
    /// window.
    pub(super) fn count_request(
        &mut self,
        request_id: u64,
        summary: &SessionSummary,
    ) -> Option<WorkerRequest> {
        // Track the roster's total even while a count is outstanding.
        self.latest_total = summary.displayed_event_count;
        let baseline = self.baseline_pending;
        if !self.loaded
            || self.reload.is_some()
            || self.count_pending.is_some()
            || (!baseline && self.probed.as_ref() == Some(summary))
        {
            return None;
        }
        self.probed = Some(summary.clone());
        let Some((anchor, offset)) = self.mark.clone() else {
            if self.events.is_empty() && self.pending.is_none() {
                // Nothing to anchor a count on: re-read from the tail.
                return self.reload_request(request_id);
            }
            return None;
        };
        self.count_pending = Some((request_id, offset));
        Some(WorkerRequest::CountAfter {
            request_id,
            key: self.key.clone(),
            anchor,
        })
    }

    /// Apply a count response. Stale responses are ignored.
    pub(super) fn apply_count(&mut self, request_id: u64, count: usize) -> bool {
        match self.count_pending {
            Some((id, offset)) if id == request_id => {
                self.count_pending = None;
                if self.baseline_pending {
                    // The baseline is the opening snapshot: arrivals reported by
                    // the roster since opening stay visible, and the observed
                    // offset excludes them.
                    self.baseline_pending = false;
                    let arrived = self.latest_total.saturating_sub(self.open_total);
                    if let Some(mark) = &mut self.mark {
                        mark.1 = count.saturating_sub(arrived);
                    }
                    self.new_events = arrived;
                } else {
                    self.new_events = count.saturating_sub(offset);
                }
                true
            }
            _ => false,
        }
    }

    /// Move by visible rows, so folded runs are skipped: the selection becomes
    /// the target row's first event.
    pub fn move_selection(&mut self, delta: isize) {
        let rows = turn_rows(&self.events);
        let Some(current) = rows
            .iter()
            .position(|row| row.first <= self.selected && self.selected <= row.last)
        else {
            return;
        };
        let last = rows.len() as isize - 1;
        let target = (current as isize).saturating_add(delta).clamp(0, last) as usize;
        self.selected = rows[target].first;
        self.settle_selection();
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
        self.settle_selection();
    }

    pub fn select_last(&mut self) {
        if let Some(first) = self.last_row_first() {
            self.selected = first;
        }
        self.settle_selection();
    }

    /// Expand or collapse the selected row. Collapsing the row that follow mode
    /// expanded dismisses only that automatic expansion: the row is not recorded
    /// as manually expanded, so the next append expands the new latest row.
    pub fn toggle_selected(&mut self) {
        let Some(event) = self.events.get(self.selected) else {
            return;
        };
        if self.auto_expanded.as_deref() == Some(event.id.as_str()) {
            self.auto_expanded = None;
            return;
        }
        if !self.expanded.remove(&event.id) {
            self.expanded.insert(event.id.clone());
        }
    }

    /// First event index of the last visible row, folded runs included.
    fn last_row_first(&self) -> Option<usize> {
        turn_rows(&self.events).last().map(|row| row.first)
    }

    /// Whether the selection sits inside the last visible row.
    fn on_last_row(&self) -> bool {
        turn_rows(&self.events)
            .last()
            .is_some_and(|row| row.first <= self.selected && self.selected <= row.last)
    }

    /// Record a user selection change (movement or a search jump): following
    /// means the selection sits in the last row of a window that reaches the
    /// tail. Leaving that row also cancels a catch-up the reader no longer
    /// wants.
    fn settle_selection(&mut self) {
        self.follow = self.newer.is_none() && self.on_last_row();
        if !self.follow {
            self.tail_owed = false;
        }
    }

    /// Put a following reader on the newest row after the newest end was loaded.
    fn land_on_newest(&mut self) {
        let Some(first) = self.last_row_first() else {
            return;
        };
        self.selected = first;
        self.follow = true;
    }

    /// Expand the latest row unless the reader expanded it by hand.
    fn expand_latest(&mut self, first: usize) {
        let Some(id) = self.events.get(first).map(|e| e.id.clone()) else {
            return;
        };
        if !self.expanded.contains(&id) {
            self.auto_expanded = Some(id);
        }
    }

    /// Whether the newest page still has to be read because an arrival was
    /// counted while another load was in flight.
    pub(super) fn tail_refresh_owed(&self) -> bool {
        self.tail_owed
    }

    /// Record that an arrival counted while following could not be fetched yet.
    pub(super) fn owe_tail_refresh(&mut self) {
        self.tail_owed = true;
    }

    /// The newest end was just (re)loaded: nothing is unread, any count in
    /// flight predates it, and the observed boundary moves to the new tail.
    fn newest_refreshed(&mut self) {
        self.new_events = 0;
        self.count_pending = None;
        self.baseline_pending = false;
        let total = self.events.len();
        let mut start = 0;
        let mut mark = None;
        for chunk in &self.chunks {
            if chunk.len > 0 {
                match &chunk.edge {
                    Edge::Last(a) => mark = Some((a.clone(), total - (start + chunk.len))),
                    Edge::First(a) => mark = Some((a.clone(), total - start - 1)),
                    Edge::Unknown => {}
                }
            }
            start += chunk.len;
        }
        self.mark = mark;
    }

    /// Merge a page response. Returns `false` (and changes nothing) when the
    /// response does not answer the load in flight.
    pub(super) fn apply_page(&mut self, request_id: u64, page: TranscriptPage) -> bool {
        let Some(pending) = self.pending.filter(|p| p.request_id == request_id) else {
            return false;
        };
        self.pending = None;
        self.search.invalidate();
        if page.events.len() > MAX_LOADED_EVENTS {
            // Cannot be kept without losing the page's anchor; treat as failed.
            self.fail_oversized(pending.kind);
            return true;
        }
        self.failed = None;
        self.loaded = true;
        let continuation = if page.has_more {
            page.next.clone()
        } else {
            None
        };
        match pending.kind {
            PageKind::Initial => self.apply_initial(page, continuation),
            PageKind::Tail => {
                // `Select` asks the worker for `Backward/None`, so this page is
                // always the newest window, even when it holds a single event.
                self.replace(page, PageDirection::Backward);
                self.older = continuation;
                self.newer = None;
                self.tail_owed = false;
                self.selected = self.last_row_first().unwrap_or(0);
                self.follow = true;
                self.newest_refreshed();
                self.resume_selection();
                if self.resume.is_some() || !self.on_last_row() {
                    // A restarted transcript is still walking back to the
                    // reading position it carried over.
                    self.follow = false;
                } else {
                    self.expand_latest(self.selected);
                }
            }
            PageKind::Newer => {
                self.append(page);
                self.newer = continuation;
                self.drop_front();
                if self.newer.is_none() {
                    // The append reached the newest end: the arrivals are on
                    // screen, and a following reader lands on the newest row.
                    self.tail_owed = false;
                    self.newest_refreshed();
                    if self.follow {
                        self.land_on_newest();
                        self.expand_latest(self.selected);
                    }
                }
            }
            PageKind::Older => {
                if self.resume.is_some() {
                    self.recover_older(page, continuation);
                } else {
                    self.prepend(page);
                    self.older = continuation;
                    self.drop_back();
                    self.resume_selection();
                }
            }
            PageKind::Reload => {
                self.apply_reload(page, continuation);
                if self.newer.is_none() {
                    // The reload re-read the newest end: a pending catch-up is
                    // met, and a follower lands on the newest row. The automatic
                    // expansion is preserved as it was.
                    self.tail_owed = false;
                    if self.follow {
                        self.newest_refreshed();
                        self.land_on_newest();
                    }
                }
            }
        }
        true
    }

    fn apply_initial(&mut self, page: TranscriptPage, continuation: Option<PageAnchor>) {
        self.replace(page, PageDirection::Forward);
        self.older = None;
        self.newer = continuation;
        self.selected = 0;
        if self.newer.is_none() {
            // The page already ends at the tail: open on its newest row.
            self.tail_owed = false;
            self.follow = true;
            self.selected = self.last_row_first().unwrap_or(0);
            self.newest_refreshed();
            self.expand_latest(self.selected);
        } else {
            // Opened away from the tail: the newest observed event is the
            // window end; a baseline count (below) discounts later history.
            self.follow = false;
            self.new_events = 0;
            self.count_pending = None;
            self.mark = self.newer.clone().map(|a| (a, 0));
            self.baseline_pending = self.mark.is_some();
        }
    }

    fn fail_oversized(&mut self, kind: PageKind) {
        self.failed = Some(kind);
        if kind == PageKind::Reload {
            self.reload = None;
        }
    }

    fn apply_reload(&mut self, mut page: TranscriptPage, continuation: Option<PageAnchor>) {
        let Some(mut reload) = self.reload.take() else {
            return;
        };
        if reload.probing_end {
            reload.probing_end = false;
            match (page.events.is_empty(), page.next.clone()) {
                (false, Some(next)) => {
                    reload.chunks = vec![Chunk {
                        len: page.events.len(),
                        edge: Edge::Last(next.clone()),
                        diagnostics: page.diagnostics,
                    }];
                    reload.probe_len = page.events.len();
                    reload.staged = page.events;
                    reload.newer = continuation;
                    reload.anchor = Some(next);
                }
                // The old last event is now the newest: read from the tail.
                _ => reload.anchor = None,
            }
            self.reload = Some(reload);
            return;
        }
        let known: HashSet<&str> = reload.staged.iter().map(|e| e.id.as_str()).collect();
        page.events.retain(|e| !known.contains(e.id.as_str()));
        reload.chunks.insert(
            0,
            Chunk {
                len: page.events.len(),
                edge: edge_of(&page, PageDirection::Backward),
                diagnostics: page.diagnostics,
            },
        );
        reload.staged.splice(0..0, page.events);
        // The selection may have moved while pages were loading: always chase
        // the current one.
        let current = self.selected_event().map(|e| e.id.clone());
        let has = |id: &Option<String>| {
            id.as_ref()
                .is_some_and(|id| reload.staged.iter().any(|e| &e.id == id))
        };
        let found_selection = has(&current);
        let found_boundary = reload.boundary.is_none() || has(&reload.boundary);
        // Probe-only events are not part of the re-read window.
        let capped = reload.staged.len().saturating_sub(reload.probe_len) >= MAX_LOADED_EVENTS;
        if continuation.is_some() && !(found_selection && found_boundary) && !capped {
            reload.anchor = continuation;
            self.reload = Some(reload);
            return;
        }
        if continuation.is_some() && !found_selection {
            // The reading position is beyond the window cap. Keep the old window
            // on screen, and make that visible and actionable: `r` jumps to the
            // newest page.
            self.failed = Some(PageKind::Reload);
            self.reload_capped = true;
            return;
        }
        let mut newer = reload.newer;
        if reload.probe_len > 0
            && let Some(end) = reload
                .end_id
                .as_ref()
                .and_then(|id| reload.staged.iter().position(|e| &e.id == id))
        {
            // The probe only served to re-read the old last event: drop it, and
            // continue from that event's own anchor.
            reload.staged.truncate(end + 1);
            trim_chunks(&mut reload.chunks, end + 1);
            newer = reload.end_anchor;
        }
        self.selected = current
            .and_then(|id| reload.staged.iter().position(|e| e.id == id))
            .unwrap_or(reload.staged.len().saturating_sub(1));
        self.events = reload.staged;
        self.chunks = reload.chunks;
        self.older = continuation;
        self.newer = newer;
        if self.newer.is_none() {
            if self.mark.is_some() && self.selected + 1 < self.events.len() {
                // A background reload must not consume arrivals the reader has not
                // reached: keep the unread boundary and recount against it.
                self.count_pending = None;
                self.probed = None;
            } else {
                self.newest_refreshed();
            }
        }
        self.drop_front();
        self.drop_back();
    }

    fn replace(&mut self, page: TranscriptPage, direction: PageDirection) {
        let edge = edge_of(&page, direction);
        self.chunks = vec![Chunk {
            len: page.events.len(),
            edge,
            diagnostics: page.diagnostics,
        }];
        self.events = page.events;
    }

    fn fresh(&self, page: &mut TranscriptPage) {
        let known: HashSet<&str> = self.events.iter().map(|e| e.id.as_str()).collect();
        page.events.retain(|e| !known.contains(e.id.as_str()));
    }

    fn append(&mut self, mut page: TranscriptPage) {
        self.fresh(&mut page);
        self.chunks.push(Chunk {
            len: page.events.len(),
            edge: edge_of(&page, PageDirection::Forward),
            diagnostics: page.diagnostics,
        });
        self.events.extend(page.events);
    }

    fn prepend(&mut self, mut page: TranscriptPage) {
        self.fresh(&mut page);
        let added = page.events.len();
        self.chunks.insert(
            0,
            Chunk {
                len: added,
                edge: edge_of(&page, PageDirection::Backward),
                diagnostics: page.diagnostics,
            },
        );
        self.events.splice(0..0, page.events);
        self.selected += added;
    }

    /// Drop the oldest events until the window fits, cutting only at a chunk
    /// edge with a known anchor and never past the selection.
    fn drop_front(&mut self) {
        let excess = self.events.len().saturating_sub(MAX_LOADED_EVENTS);
        if excess == 0 {
            return;
        }
        let mut before = 0; // events in chunks preceding `k`
        let mut best: Option<(usize, usize, PageAnchor)> = None; // (chunk, drop, anchor)
        for (k, chunk) in self.chunks.iter().enumerate() {
            let candidate = match &chunk.edge {
                _ if chunk.len == 0 => None,
                Edge::First(a) => Some((before, a.clone())),
                Edge::Last(a) => Some((before + chunk.len.saturating_sub(1), a.clone())),
                Edge::Unknown => None,
            };
            if let Some((drop, anchor)) = candidate
                && drop > 0
                && drop <= self.selected
            {
                best = Some((k, drop, anchor));
                if drop >= excess {
                    break;
                }
            }
            before += chunk.len;
        }
        let Some((k, drop, anchor)) = best else {
            return;
        };
        self.events.drain(..drop);
        self.selected -= drop;
        self.chunks.drain(..k);
        if let Some(first) = self.chunks.first_mut() {
            // A forward chunk is trimmed to its anchor event; a backward chunk is whole.
            if matches!(first.edge, Edge::Last(_)) {
                first.len = 1;
                first.diagnostics.clear();
            }
        }
        self.older = Some(anchor);
    }

    /// Mirror of [`Self::drop_front`] for the newest end.
    fn drop_back(&mut self) {
        let excess = self.events.len().saturating_sub(MAX_LOADED_EVENTS);
        if excess == 0 {
            return;
        }
        let total = self.events.len();
        let mut upto = Vec::with_capacity(self.chunks.len());
        let mut running = 0;
        for chunk in &self.chunks {
            running += chunk.len;
            upto.push(running);
        }
        let mut best: Option<(usize, usize, PageAnchor)> = None;
        for (k, chunk) in self.chunks.iter().enumerate().rev() {
            let candidate = match &chunk.edge {
                _ if chunk.len == 0 => None,
                Edge::Last(a) => Some((total - upto[k], a.clone())),
                Edge::First(a) => Some((total - upto[k] + chunk.len.saturating_sub(1), a.clone())),
                Edge::Unknown => None,
            };
            if let Some((drop, anchor)) = candidate
                && drop > 0
                && total - drop > self.selected
            {
                best = Some((k, drop, anchor));
                if drop >= excess {
                    break;
                }
            }
        }
        let Some((k, drop, anchor)) = best else {
            return;
        };
        self.events.truncate(total - drop);
        self.chunks.truncate(k + 1);
        if let Some(last) = self.chunks.last_mut()
            && matches!(last.edge, Edge::First(_))
        {
            last.len = 1;
            last.diagnostics.clear();
        }
        self.newer = Some(anchor);
    }
}

/// Keep only the first `keep` events' worth of chunks. A chunk cut short loses
/// an end anchor that named a removed event.
fn trim_chunks(chunks: &mut Vec<Chunk>, keep: usize) {
    let mut total: usize = chunks.iter().map(|c| c.len).sum();
    while total > keep {
        let Some(last) = chunks.last_mut() else {
            break;
        };
        let excess = total - keep;
        if last.len <= excess {
            total -= last.len;
            chunks.pop();
        } else {
            last.len -= excess;
            total = keep;
            if matches!(last.edge, Edge::Last(_)) {
                last.edge = Edge::Unknown;
            }
            last.diagnostics.clear();
        }
    }
}
fn anchor_of_event(event: &Event) -> PageAnchor {
    PageAnchor {
        timestamp: event.timestamp,
        source_order: event.source_order,
        stable_id: event.id.clone(),
    }
}

fn edge_of(page: &TranscriptPage, direction: PageDirection) -> Edge {
    match (&page.next, direction) {
        (Some(a), PageDirection::Forward) => Edge::Last(a.clone()),
        (Some(a), PageDirection::Backward) => Edge::First(a.clone()),
        (None, _) => Edge::Unknown,
    }
}
