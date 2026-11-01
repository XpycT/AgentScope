//! Bounded entry-tree, active-branch context and fork-ownership index of one Pi
//! session file.
//!
//! Pinned to earendil-works/pi `core/session-manager.ts` (`_buildIndex`,
//! `createBranchedSession`, `forkFrom`) and `docs/session-format.md`:
//!
//! * The active leaf is the last entry in *file order*. Navigation that was not
//!   durably appended is not observable in the file, so it stays Unknown instead
//!   of being reconstructed from a live process.
//! * Both fork producers copy the parent's entries **unchanged**: the native entry
//!   id and the confirmed `responseId` are preserved and only the header id is new.
//!   Exact identity is therefore the only ownership evidence; prompt text, titles,
//!   timestamps and counters never are.
//! * A usage-bearing entry outside that copied prefix was recorded by this file and
//!   is owned. Without a proven copied prefix the ownership stays Unknown, so a
//!   remapped copy is never charged as new spend.
//!
//! Only entry headers, call identities, prompt totals and ordinals are kept; message
//! bodies are re-read from the source for display.

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::domain::{AggregationClass, ContextUsage, ModelDetail, SessionLink};
use crate::metrics::context_usage;
use crate::pricing::PriceSchedule;
use crate::providers::pi::record::{PiEntry, PiEntryKind, PiPrompt, PiRole};

/// Hash-table slot and allocator bookkeeping charged per retained node, beside the
/// inline size of [`Node`] and the heap strings it owns.
const SLOT_OVERHEAD: usize = 48;
/// Inline cost of one inherited-evidence entry: the map key/value pair plus the
/// response string duplicated in the response set.
const EVIDENCE_OVERHEAD: usize = 96;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiInsert {
    Inserted,
    /// The id was already indexed; the first structure is kept.
    Duplicate,
    /// The entry or byte ceiling was hit; the index is incomplete from now on.
    Limited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiOwnership {
    /// This file recorded the call.
    Owned,
    /// A linked, already-discovered predecessor holds the same call.
    Inherited,
    /// The call cannot be attributed: no proven copied prefix, or the link itself
    /// is unresolved/ambiguous/incomplete.
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiTreeError {
    /// The leaf, or a parent on its path, is not in the index.
    MissingParent,
    Cycle,
    /// The index dropped entries at its ceiling, so no path is certain.
    Limited,
}

impl fmt::Display for PiTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingParent => "active leaf or an ancestor is missing",
            Self::Cycle => "entry parents form a cycle",
            Self::Limited => "entry index limit reached",
        })
    }
}

impl std::error::Error for PiTreeError {}

/// What the header's `parentSession` established about the fork relationship.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Link {
    /// The header names no parent session.
    None,
    /// The parent is an already-discovered source whose entries are available.
    Proven,
    /// The parent was named but not resolved among discovered sources.
    Unproven,
}

struct Node {
    parent: Option<String>,
    ordinal: u64,
    /// Prompt categories recorded by an assistant call at this entry.
    prompt: Option<PiPrompt>,
    /// A compaction or context edit rewrites the context of this branch.
    resets_context: bool,
    /// `None` for a record without a model call. `Some(response)` keeps the
    /// confirmed provider response id, `Some(None)` an unconfirmed call.
    call: Option<Option<String>>,
}

/// Headers of one session file, bounded by entry count and retained bytes.
pub struct PiTree {
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
    nodes: HashMap<String, Node>,
    /// The last record in file order decides the active leaf.
    last: Option<(u64, String)>,
    limited: bool,
    /// Incremented by every compaction and context edit.
    epoch: u32,
    link: Link,
    /// Usage identities of the discovered fork parent: entry id -> response id.
    inherited: HashMap<String, Option<String>>,
    inherited_responses: HashSet<String>,
    inherited_bytes: usize,
    inherited_complete: bool,
    /// Highest ordinal of an entry that proves the copied prefix.
    copied_through: Option<u64>,
}

impl fmt::Debug for PiTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiTree")
            .field("entries", &self.nodes.len())
            .field("limited", &self.limited)
            .field("epoch", &self.epoch)
            .field("inherited", &self.inherited.len())
            .finish_non_exhaustive()
    }
}

fn call_of(kind: &PiEntryKind) -> Option<Option<String>> {
    match kind {
        PiEntryKind::Message {
            role: PiRole::Assistant,
            usage: true,
            response_id,
            ..
        } => Some(response_id.clone()),
        PiEntryKind::Usage { .. } => Some(None),
        PiEntryKind::Compaction { usage: true, .. }
        | PiEntryKind::BranchSummary { usage: true, .. } => Some(None),
        _ => None,
    }
}

fn prompt_of(kind: &PiEntryKind) -> Option<&PiPrompt> {
    match kind {
        PiEntryKind::Message {
            role: PiRole::Assistant,
            usage: true,
            prompt: Some(prompt),
            ..
        } => Some(prompt),
        _ => None,
    }
}

fn model_bytes(model: Option<&ModelDetail>) -> usize {
    model.map_or(0, |model| {
        model.id.len()
            + model.provider.as_deref().map_or(0, str::len)
            + model.variant.as_deref().map_or(0, str::len)
            + model.effort.as_deref().map_or(0, str::len)
            + model.configured_effort.as_deref().map_or(0, str::len)
    })
}

impl PiTree {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            max_entries,
            max_bytes,
            bytes: 0,
            nodes: HashMap::new(),
            last: None,
            limited: false,
            epoch: 0,
            link: Link::None,
            inherited: HashMap::new(),
            inherited_responses: HashSet::new(),
            inherited_bytes: 0,
            inherited_complete: false,
            copied_through: None,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn is_limited(&self) -> bool {
        self.limited
    }

    /// Bytes charged against the tree's entry budget.
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }

    /// Records the header's `parentSession` evidence. Only an already-resolved
    /// discovered source is usable; anything else leaves ownership Unknown.
    pub fn set_fork(&mut self, fork: Option<&SessionLink>) {
        self.link = match fork {
            None => Link::None,
            Some(SessionLink::Resolved(_)) => Link::Proven,
            Some(SessionLink::Unresolved(_) | SessionLink::Ambiguous) => Link::Unproven,
        };
        if !matches!(self.link, Link::Proven) {
            self.inherited.clear();
            self.inherited_responses.clear();
            self.inherited_bytes = 0;
            self.inherited_complete = false;
            self.copied_through = None;
        }
    }

    /// Installs the usage identities of the discovered source this file was forked
    /// from. Returns whether the evidence covers that predecessor completely.
    pub fn adopt(&mut self, predecessor: &PiTree) -> bool {
        self.inherited.clear();
        self.inherited_responses.clear();
        self.inherited_bytes = 0;
        let mut complete = !predecessor.limited;
        for (id, node) in &predecessor.nodes {
            let Some(response) = &node.call else {
                continue;
            };
            // The key, the value and the duplicated response string are all retained.
            let cost = EVIDENCE_OVERHEAD
                + id.len()
                + response
                    .as_ref()
                    .map_or(0, |response| response.len().saturating_mul(2));
            if self.inherited.len() >= self.max_entries
                || self.inherited_bytes.saturating_add(cost) > self.max_bytes
            {
                complete = false;
                break;
            }
            self.inherited_bytes += cost;
            if let Some(response) = response {
                self.inherited_responses.insert(response.clone());
            }
            self.inherited.insert(id.clone(), response.clone());
        }
        self.inherited_complete = complete;
        self.refresh_copied();
        complete
    }

    /// Marks the predecessor's evidence as not covering every usage-bearing entry,
    /// e.g. after an unreadable record. No later conclusion can be `Owned`.
    pub fn mark_inherited_incomplete(&mut self) {
        self.inherited_complete = false;
    }

    fn copied_here(&self, id: &str, response: Option<&str>) -> bool {
        match self.inherited.get(id) {
            Some(recorded) => match (recorded.as_deref(), response) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            },
            None => response.is_some_and(|r| self.inherited_responses.contains(r)),
        }
    }

    fn refresh_copied(&mut self) {
        self.copied_through = self
            .nodes
            .iter()
            .filter(|(id, node)| {
                node.call
                    .as_ref()
                    .is_some_and(|response| self.copied_here(id, response.as_deref()))
            })
            .map(|(_, node)| node.ordinal)
            .max();
    }

    fn note_last(&mut self, entry: &PiEntry) {
        if self
            .last
            .as_ref()
            .is_none_or(|(ordinal, _)| entry.ordinal >= *ordinal)
        {
            self.last = Some((entry.ordinal, entry.id.clone()));
        }
    }

    /// Indexes one record. Only identity, tree position, call identity and prompt
    /// totals are retained.
    pub fn insert(&mut self, entry: &PiEntry) -> PiInsert {
        self.note_last(entry);
        if self.nodes.contains_key(&entry.id) {
            // A repeated id keeps its first structure but is still the newest line
            // in the file, so it still decides the replayed leaf.
            return PiInsert::Duplicate;
        }
        let call = call_of(&entry.kind);
        let prompt = prompt_of(&entry.kind).cloned();
        let mut cost = std::mem::size_of::<Node>()
            + SLOT_OVERHEAD
            + std::mem::size_of::<String>()
            + entry.id.len();
        cost += entry.parent_id.as_ref().map_or(0, String::len);
        cost += call
            .as_ref()
            .and_then(|response| response.as_ref())
            .map_or(0, String::len);
        cost += prompt
            .as_ref()
            .map_or(0, |prompt| model_bytes(prompt.model.as_ref()));
        if self.nodes.len() >= self.max_entries || self.bytes.saturating_add(cost) > self.max_bytes
        {
            self.limited = true;
            return PiInsert::Limited;
        }
        let resets_context = matches!(
            &entry.kind,
            PiEntryKind::Compaction { .. } | PiEntryKind::ContextEdit { .. }
        );
        let copied = matches!(self.link, Link::Proven)
            && call
                .as_ref()
                .is_some_and(|response| self.copied_here(&entry.id, response.as_deref()));
        self.bytes += cost;
        self.nodes.insert(
            entry.id.clone(),
            Node {
                parent: entry.parent_id.clone(),
                ordinal: entry.ordinal,
                prompt,
                resets_context,
                call,
            },
        );
        if resets_context {
            self.epoch = self.epoch.saturating_add(1);
        }
        if copied {
            self.copied_through = Some(
                self.copied_through
                    .map_or(entry.ordinal, |at| at.max(entry.ordinal)),
            );
        }
        PiInsert::Inserted
    }

    fn active_nodes(&self) -> Result<Vec<&Node>, PiTreeError> {
        if self.limited {
            return Err(PiTreeError::Limited);
        }
        let Some((_, leaf)) = &self.last else {
            return Ok(Vec::new());
        };
        let mut seen: HashSet<&str> = HashSet::new();
        let mut nodes = Vec::new();
        let mut current = leaf.as_str();
        loop {
            let node = self.nodes.get(current).ok_or(PiTreeError::MissingParent)?;
            if !seen.insert(current) {
                return Err(PiTreeError::Cycle);
            }
            nodes.push(node);
            match node.parent.as_deref() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        nodes.reverse();
        Ok(nodes)
    }

    /// Ordinals of the active branch, root first. Empty for a file with no entry.
    pub fn active_path(&self) -> Result<Vec<u64>, PiTreeError> {
        Ok(self
            .active_nodes()?
            .into_iter()
            .map(|node| node.ordinal)
            .collect())
    }

    /// Whether the call recorded at `entry_id` is this file's own spend, a copy of
    /// a linked predecessor's call, or cannot be told.
    pub fn ownership(&self, entry_id: &str) -> PiOwnership {
        let Some(node) = self.nodes.get(entry_id) else {
            return PiOwnership::Unknown;
        };
        let Some(response) = &node.call else {
            // No model call: nothing to inherit.
            return PiOwnership::Owned;
        };
        match self.link {
            Link::None => PiOwnership::Owned,
            Link::Unproven => PiOwnership::Unknown,
            Link::Proven => {
                if let Some(recorded) = self.inherited.get(entry_id) {
                    return match (recorded.as_deref(), response.as_deref()) {
                        // The producer copies entries unchanged, so a different
                        // confirmed response under a copied id is not a copy this
                        // adapter can attribute.
                        (Some(a), Some(b)) if a != b => PiOwnership::Unknown,
                        _ => PiOwnership::Inherited,
                    };
                }
                if response
                    .as_deref()
                    .is_some_and(|r| self.inherited_responses.contains(r))
                {
                    return PiOwnership::Inherited;
                }
                // Being outside the copied prefix only proves ownership when the
                // predecessor's calls are all known; otherwise a call could hide in an
                // unread record.
                if self.inherited_complete
                    && self.copied_through.is_some_and(|at| node.ordinal > at)
                {
                    return PiOwnership::Owned;
                }
                if self.inherited_complete
                    && self.copied_through.is_none()
                    && self.inherited.is_empty()
                {
                    // A provably call-free predecessor cannot have been copied from.
                    return PiOwnership::Owned;
                }
                PiOwnership::Unknown
            }
        }
    }

    /// The latest assistant prompt snapshot on the active path, or Unknown when a
    /// later compaction/context edit superseded it. Capacity comes from the price
    /// catalog for the recorded provider/model only.
    pub fn fold_context(&self, prices: &PriceSchedule) -> ContextUsage {
        let Ok(nodes) = self.active_nodes() else {
            return ContextUsage::default();
        };
        let current = self.epoch;
        // Path-local: a compaction or context edit only supersedes a snapshot when it
        // sits later on the same active branch.
        let mut latest: Option<&Node> = None;
        let mut superseded = false;
        for node in &nodes {
            if node.prompt.is_some() {
                latest = Some(node);
                superseded = false;
            }
            if node.resets_context {
                superseded = true;
            }
        }
        let snapshot = match latest {
            Some(node) if !superseded => node.prompt.as_ref(),
            _ => None,
        };
        let Some(prompt) = snapshot else {
            // The path is known, but a later compaction/context edit left no current
            // snapshot: occupancy stays Unknown in the current epoch.
            return context_usage(
                None,
                None,
                AggregationClass::Unknown,
                None,
                None,
                Some(format!("c{current}")),
            );
        };
        let capacity = prompt.model.as_ref().and_then(|model| {
            let provider = model.provider.as_deref()?;
            prices.model_context_capacity(provider, &model.id)
        });
        context_usage(
            Some(prompt.used),
            capacity,
            AggregationClass::Estimated,
            prompt.recorded_at,
            prompt.model.clone(),
            Some(format!("c{current}")),
        )
    }
}
