//! Cached last-activity read for the preview (readability redesign).
//!
//! The preview pane shows the newest tool interaction and assistant reply of
//! the selected session without opening its transcript. One bounded
//! `PreviewTail` read serves that block at a time; the results are cached by
//! session and invalidated when the session's fingerprint changes.
//!
//! Pure state, no I/O. Text is sanitized and credential-masked at ingest, so
//! neither rendering nor a diagnostic dump can expose a sentinel that the
//! transcript would hide. Reply text keeps [`BoundedText`]'s content-free
//! `Debug`.

use std::collections::{HashMap, VecDeque};
use std::fmt;

use crate::domain::{
    BoundedText, Event, EventKind, PageAnchor, SessionKey, Timestamp, ToolInteraction, ToolStatus,
    TranscriptPage,
};
use crate::ingest::text::display_safe;

use super::turns::tool_summary;

/// Cached sessions kept before the least recently touched one is evicted.
pub const ACTIVITY_CACHE_CAP: usize = 64;

/// How long the selection must stay unchanged before its activity is read.
pub const ACTIVITY_DEBOUNCE_MS: u64 = 150;

/// The newest tool interaction and assistant reply of one session.
#[derive(Clone, PartialEq, Eq)]
pub struct LastActivity {
    pub tool: Option<ToolLine>,
    pub reply: Option<BoundedText>,
    /// Timestamp of the newest event the read saw.
    pub at: Option<Timestamp>,
}

impl fmt::Debug for LastActivity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LastActivity")
            .field("tool", &self.tool)
            .field("reply", &self.reply)
            .field("at", &self.at)
            .finish()
    }
}

/// One tool interaction reduced to a preview line.
#[derive(Clone, PartialEq, Eq)]
pub struct ToolLine {
    pub name: String,
    pub status: ToolStatus,
    pub summary: String,
}

impl fmt::Debug for ToolLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolLine")
            .field("name_len", &self.name.len())
            .field("status", &self.status)
            .field("summary_len", &self.summary.len())
            .finish()
    }
}

/// What the preview's activity block can show. `Debug` is content-free through
/// [`LastActivity`] and [`ToolLine`].
// The brief fixes `Ready(LastActivity)`; every state is short-lived and owned by
// one preview entry, so the size difference is not worth an indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityState {
    /// The read is in flight and no result is known yet.
    Loading,
    /// The newest activity was read.
    Ready(LastActivity),
    /// The session has no events.
    Nothing,
    /// The read failed for good; only invalidation asks again.
    Failed,
}

/// The single activity read in flight.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingActivity {
    pub request_id: u64,
    pub key: SessionKey,
    /// The one permitted older page was already requested.
    pub scanned_back: bool,
    /// Newest tool line and event time of the newest page, kept across the
    /// bounded scan-back so an older page cannot hide newer activity.
    tool: Option<ToolLine>,
    at: Option<Timestamp>,
}

/// How a page applied to the cache.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ActivityStep {
    /// The entry was updated; nothing more to read.
    Done,
    /// No assistant reply yet and one older page is still allowed.
    NeedOlder(PageAnchor),
    /// The reply does not answer the in-flight request; nothing changed.
    Stale,
}

/// LRU cache of preview activity, at most one read in flight.
#[derive(Default)]
pub struct ActivityCache {
    entries: HashMap<SessionKey, ActivityState>,
    /// Recency of the entries, front = least recently touched.
    order: VecDeque<SessionKey>,
    /// Last observed `(last_event_at, displayed_event_count)` per session. A
    /// change means the session changed and its cached activity is stale.
    fingerprints: HashMap<SessionKey, (Option<Timestamp>, usize)>,
    pending: Option<PendingActivity>,
}

impl ActivityCache {
    /// Cached state for rendering. Read-only; it never touches recency.
    pub fn get(&self, key: &SessionKey) -> Option<&ActivityState> {
        self.entries.get(key)
    }

    /// Mark `key` as the most recently used entry, on selection.
    pub(crate) fn touch(&mut self, key: &SessionKey) {
        if self.entries.contains_key(key) {
            self.bump(key);
        }
    }

    /// Start a read for `key`, showing `Loading` immediately. At most one read
    /// is in flight; a different pending request is superseded and its id is
    /// returned so the caller can cancel the worker job.
    pub(crate) fn begin(&mut self, request_id: u64, key: SessionKey) -> Option<u64> {
        let superseded = self.cancel_pending();
        self.insert(key.clone(), ActivityState::Loading);
        self.pending = Some(PendingActivity {
            request_id,
            key,
            scanned_back: false,
            tool: None,
            at: None,
        });
        superseded.filter(|old| *old != request_id)
    }

    /// Abandon the in-flight read, if any: drop its `Loading` entry and return
    /// the request id so the caller can cancel the worker job. A page answering
    /// the abandoned id is stale.
    pub(crate) fn cancel_pending(&mut self) -> Option<u64> {
        let pending = self.pending.take()?;
        if self
            .entries
            .get(&pending.key)
            .is_some_and(|state| matches!(state, ActivityState::Loading))
        {
            self.remove_entry(&pending.key);
        }
        Some(pending.request_id)
    }

    /// Request id of `key`'s in-flight read, if one is pending.
    pub(crate) fn pending_id(&self, key: &SessionKey) -> Option<u64> {
        self.pending
            .as_ref()
            .filter(|pending| pending.key == *key)
            .map(|pending| pending.request_id)
    }

    /// Apply one page to the in-flight read. A page that does not answer the
    /// pending request changes nothing.
    pub(crate) fn apply_page(
        &mut self,
        request_id: u64,
        key: &SessionKey,
        page: &TranscriptPage,
    ) -> ActivityStep {
        let Some(pending) = self.pending.as_ref() else {
            return ActivityStep::Stale;
        };
        if pending.request_id != request_id || pending.key != *key {
            return ActivityStep::Stale;
        }
        let scanned_back = pending.scanned_back;
        let first_tool = pending.tool.clone();
        let first_at = pending.at;

        if page.events.is_empty() {
            self.pending = None;
            // An empty newest page means there is no activity; an empty
            // scan-back page still leaves the newest page's tool line, which
            // the caller renders with "no reply in recent events".
            let state = if scanned_back {
                ActivityState::Ready(LastActivity {
                    tool: first_tool,
                    reply: None,
                    at: first_at,
                })
            } else {
                ActivityState::Nothing
            };
            self.insert(key.clone(), state);
            return ActivityStep::Done;
        }

        let page_tool = newest_tool(page);
        // The newest page is newer than a scan-back page, so its tool wins.
        let tool = first_tool.or(page_tool);
        let at = first_at.max(newest_at(page));
        let reply_event = newest_reply(page);

        if reply_event.is_none()
            && page.has_more
            && !scanned_back
            && let Some(anchor) = page.next.clone()
        {
            if let Some(pending) = self.pending.as_mut() {
                pending.scanned_back = true;
                pending.tool = tool;
                pending.at = at;
            }
            return ActivityStep::NeedOlder(anchor);
        }

        let reply = reply_event.and_then(reply_text);
        self.pending = None;
        self.insert(
            key.clone(),
            ActivityState::Ready(LastActivity { tool, reply, at }),
        );
        ActivityStep::Done
    }

    /// A read failed for good. Only the pending request changes.
    pub(crate) fn fail(&mut self, request_id: u64) -> bool {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.request_id != request_id)
        {
            return false;
        }
        if let Some(pending) = self.pending.take() {
            self.insert(pending.key, ActivityState::Failed);
        }
        true
    }

    /// Record the session's fingerprint. A changed fingerprint drops its cached
    /// activity and, if its read is in flight, clears it and returns the request
    /// id so the caller can cancel the worker job.
    pub(crate) fn observe(
        &mut self,
        key: &SessionKey,
        last_event_at: Option<Timestamp>,
        count: usize,
    ) -> Option<u64> {
        let fingerprint = (last_event_at, count);
        let changed = match self.fingerprints.insert(key.clone(), fingerprint) {
            None => false,
            Some(previous) => previous != fingerprint,
        };
        if !changed {
            return None;
        }
        self.remove_entry(key);
        match self.pending.take() {
            Some(pending) if pending.key == *key => Some(pending.request_id),
            other => {
                self.pending = other;
                None
            }
        }
    }

    /// Drop `key`'s cached activity. Used when the session changed in place;
    /// a late page for an in-flight read will no longer match.
    pub(crate) fn invalidate(&mut self, key: &SessionKey) {
        self.remove_entry(key);
        if self.pending.as_ref().is_some_and(|p| p.key == *key) {
            self.pending = None;
        }
    }

    fn insert(&mut self, key: SessionKey, state: ActivityState) {
        self.entries.insert(key.clone(), state);
        self.bump(&key);
        self.trim();
    }

    fn bump(&mut self, key: &SessionKey) {
        self.order.retain(|known| known != key);
        self.order.push_back(key.clone());
    }

    fn remove_entry(&mut self, key: &SessionKey) {
        self.entries.remove(key);
        self.order.retain(|known| known != key);
    }

    fn trim(&mut self) {
        while self.entries.len() > ACTIVITY_CACHE_CAP {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if self.entries.remove(&oldest).is_some() {
                // Without a cached entry the fingerprint cannot invalidate
                // anything; the next observation seeds a fresh baseline.
                self.fingerprints.remove(&oldest);
            }
        }
    }
}

/// Chronological order of a recorded event, without allocating.
fn order_key(event: &Event) -> (Option<Timestamp>, u64, &str) {
    (event.timestamp, event.source_order, event.id.as_str())
}

fn newest_at(page: &TranscriptPage) -> Option<Timestamp> {
    page.events.iter().filter_map(|event| event.timestamp).max()
}

fn newest_tool(page: &TranscriptPage) -> Option<ToolLine> {
    let mut newest: Option<(&Event, &ToolInteraction)> = None;
    for event in &page.events {
        if event.kind != EventKind::Tool {
            continue;
        }
        let Some(tool) = event.tool.as_ref() else {
            continue;
        };
        if newest.is_none_or(|(known, _)| order_key(event) > order_key(known)) {
            newest = Some((event, tool));
        }
    }
    let (_, tool) = newest?;
    Some(ToolLine {
        name: display_safe(&tool.name),
        status: tool.status,
        // `tool_summary` already folds through `one_line`; masking the folded
        // result keeps this cache subject to the same policy as the transcript.
        summary: display_safe(&tool_summary(tool)),
    })
}

fn newest_reply(page: &TranscriptPage) -> Option<&Event> {
    let mut newest: Option<&Event> = None;
    for event in &page.events {
        if event.kind != EventKind::AssistantMessage || event.text.is_none() {
            continue;
        }
        if newest.is_none_or(|known| order_key(event) > order_key(known)) {
            newest = Some(event);
        }
    }
    newest
}

/// The recorded bounded reply, sanitized and masked again so the cache never
/// holds text the transcript would hide.
fn reply_text(event: &Event) -> Option<BoundedText> {
    let text = event.text.as_ref()?;
    Some(BoundedText {
        preview: display_safe(&text.preview),
        byte_len: text.byte_len,
        truncated: text.truncated,
        locator: None,
    })
}
