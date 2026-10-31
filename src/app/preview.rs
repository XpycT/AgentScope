//! Fast session preview and paged detailed facts (spec §10.2–§10.3).
//!
//! Pure state, no I/O. A preview is requested for the row under the cursor and
//! carries the request identity that may update it; anything else is stale and
//! ignored. Detailed pages are accumulated under one session, so a later page
//! never replaces the facts already shown.

use std::sync::Arc;

use crate::domain::{PageAnchor, SessionDetailPage, SessionKey, SessionSummary};
use crate::providers::WorkProgress;

/// Detailed facts of the session under the cursor.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum PreviewState {
    /// Nothing is selected, or no preview was requested yet.
    #[default]
    Empty,
    /// The request is in flight and no cached facts were available.
    Loading {
        request_id: u64,
        key: SessionKey,
        progress: WorkProgress,
    },
    /// Facts are on screen. `complete` is false while more work is pending;
    /// cached facts are visible immediately and are not reported as final.
    Ready {
        request_id: u64,
        key: SessionKey,
        summary: Arc<SessionSummary>,
        complete: bool,
    },
    /// The request failed for good; only an explicit retry asks again.
    Failed { key: SessionKey },
}

impl PreviewState {
    pub fn key(&self) -> Option<&SessionKey> {
        match self {
            Self::Empty => None,
            Self::Loading { key, .. } | Self::Ready { key, .. } | Self::Failed { key } => Some(key),
        }
    }

    pub fn request_id(&self) -> Option<u64> {
        match self {
            Self::Loading { request_id, .. } | Self::Ready { request_id, .. } => Some(*request_id),
            Self::Empty | Self::Failed { .. } => None,
        }
    }

    pub fn summary(&self) -> Option<&Arc<SessionSummary>> {
        match self {
            Self::Ready { summary, .. } => Some(summary),
            _ => None,
        }
    }

    pub fn is_loading(&self) -> bool {
        matches!(self, Self::Loading { .. })
    }
}

/// Accumulated on-demand detail pages for one session.
///
/// `page.next` anchors the continuation into older events; pages are appended
/// only while the request id they answer is the one in flight.
#[derive(Clone, Debug, Default)]
pub struct DetailsState {
    key: Option<SessionKey>,
    page: SessionDetailPage,
    /// Request id of the page in flight.
    pending: Option<u64>,
    progress: WorkProgress,
    failed: bool,
}

impl DetailsState {
    pub fn key(&self) -> Option<&SessionKey> {
        self.key.as_ref()
    }

    pub fn page(&self) -> &SessionDetailPage {
        &self.page
    }

    pub fn loading(&self) -> Option<u64> {
        self.pending
    }

    pub fn progress(&self) -> WorkProgress {
        self.progress
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// The first page has arrived and every page is loaded.
    pub fn complete(&self) -> bool {
        self.key.is_some() && self.pending.is_none() && !self.failed && !self.page.has_more
    }

    /// Start a detail request. A fresh stream (the newest page) replaces any
    /// accumulated page, so reopening A or switching A→B never appends B's or a
    /// stale partial page under the old session.
    pub(super) fn begin(&mut self, request_id: u64, key: SessionKey, fresh: bool) {
        if fresh || self.key.as_ref() != Some(&key) {
            self.page = SessionDetailPage::default();
        }
        self.key = Some(key);
        self.pending = Some(request_id);
        self.progress = WorkProgress::default();
        self.failed = false;
    }

    pub(super) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(super) fn apply(
        &mut self,
        request_id: u64,
        key: &SessionKey,
        page: SessionDetailPage,
    ) -> bool {
        if self.pending != Some(request_id) || self.key.as_ref() != Some(key) {
            return false;
        }
        self.pending = None;
        self.page.requests.extend(page.requests);
        self.page.compactions.extend(page.compactions);
        self.page.recent.extend(page.recent);
        self.page.next = page.next;
        self.page.has_more = page.has_more;
        self.page.truncated |= page.truncated;
        true
    }

    pub(super) fn apply_progress(&mut self, request_id: u64, progress: WorkProgress) -> bool {
        if self.pending == Some(request_id) {
            self.progress = progress;
            true
        } else {
            false
        }
    }

    pub(super) fn fail(&mut self, request_id: u64) -> bool {
        if self.pending == Some(request_id) {
            self.pending = None;
            self.failed = true;
            true
        } else {
            false
        }
    }

    pub(super) fn has_pending(&self) -> bool {
        self.pending.is_some()
    }

    pub(super) fn next_anchor(&self) -> Option<PageAnchor> {
        self.page.next.clone()
    }
}
