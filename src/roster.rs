//! Metadata index and frozen paging generations (spec §5).
//!
//! The index holds one lightweight [`SessionMetadata`] per discovered source (bounded
//! by [`MetadataLimits`]) and, separately, a bounded cache of hydrated
//! [`SessionSummary`] values. Top-level groups are ordered once per generation and
//! revealed 50 at a time; hydration, newly discovered sources and deletions never
//! move a frozen generation, they only raise `refresh_available` or mark a member
//! unavailable until the next explicit [`RosterIndex::freeze`].
//!
//! Relationships come from the native explicit edges the adapters report: resolved
//! or unresolved parent links and continuation `members`. Unknown, ambiguous,
//! missing and cyclic parents leave a session as its own top-level group; a fork is
//! never an edge.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use crate::domain::{
    Harness, ScanProgress, SessionKey, SessionLink, SessionMetadata, SessionSummary, SourceKey,
    Timestamp,
};
use crate::providers::DiscoveryDelta;

/// Top-level groups revealed per generation step, and children per family page.
pub const ROOT_PAGE: usize = 50;
pub const CHILD_PAGE: usize = 50;
/// Ancestor links followed when rolling descendant activity up into a group.
const MAX_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MetadataLimits {
    pub max_sources: usize,
    pub max_bytes: usize,
}

/// The ceiling that stopped discovery; shown instead of an end-of-list claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiscoveryLimit {
    pub sources: usize,
    pub bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChildPage {
    pub metadata: Vec<Arc<SessionMetadata>>,
    /// The last key of this page when more children remain: pass it as `after` to
    /// continue the same family.
    pub next: Option<SessionKey>,
}

/// What the UI needs of one generation. Only loaded roots are copied; the rest of
/// the candidate metadata stays in the worker's index.
#[derive(Clone, Debug)]
pub struct RosterSnapshot {
    pub generation: u64,
    /// Loaded top-level group keys in display order.
    pub roots: Arc<[SessionKey]>,
    /// Metadata of `roots`, same order.
    pub metadata: Vec<Arc<SessionMetadata>>,
    /// Hydrated summaries among `roots`, in `roots` order (rows without one are
    /// metadata-only).
    pub summaries: Vec<Arc<SessionSummary>>,
    pub scan: Vec<ScanProgress>,
    /// More known top-level groups than are loaded.
    pub has_more: bool,
    pub limited: Option<DiscoveryLimit>,
    /// Sources, deletions or edges found after the generation froze.
    pub refresh_available: bool,
    /// Discovery is still running and the ordering may still change.
    pub provisional: bool,
}

/// Total order over keys: harness, native ID, then source. Used for stable ties.
pub fn cmp_keys(a: &SessionKey, b: &SessionKey) -> Ordering {
    a.harness
        .cmp(&b.harness)
        .then_with(|| a.native_id.cmp(&b.native_id))
        .then_with(|| match (&a.source, &b.source) {
            (SourceKey::File(x), SourceKey::File(y)) => x.cmp(y),
            (SourceKey::File(_), SourceKey::DbRow { .. }) => Ordering::Less,
            (SourceKey::DbRow { .. }, SourceKey::File(_)) => Ordering::Greater,
            (
                SourceKey::DbRow {
                    database: da,
                    table: ta,
                    id: ia,
                },
                SourceKey::DbRow {
                    database: db,
                    table: tb,
                    id: ib,
                },
            ) => da.cmp(db).then_with(|| ta.cmp(tb)).then_with(|| ia.cmp(ib)),
        })
}

/// Newest first; no recorded activity sorts after every timestamped value.
pub fn activity_desc(a: Option<Timestamp>, b: Option<Timestamp>) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => y.cmp(&x),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Estimated resident size of one metadata record, for the byte ceiling.
pub fn metadata_bytes(meta: &SessionMetadata) -> usize {
    let source = match &meta.key.source {
        SourceKey::File(path) => path.as_os_str().len(),
        SourceKey::DbRow {
            database,
            table,
            id,
        } => database.as_os_str().len() + table.len() + id.len(),
    };
    std::mem::size_of::<SessionMetadata>()
        + meta.key.native_id.len()
        + source
        + meta.title.len()
        + meta.project.as_ref().map_or(0, String::len)
        + meta.cwd.as_ref().map_or(0, |p| p.as_os_str().len())
        + meta.model.as_ref().map_or(0, |m| m.id.len())
        + meta.origins.len() * 64
        + meta.members.len() * 96
        + meta.harness_version.as_ref().map_or(0, String::len)
}

struct Entry {
    meta: Arc<SessionMetadata>,
    bytes: usize,
    /// Part of the frozen generation's ordering.
    member: bool,
}

struct Built {
    order: Vec<SessionKey>,
    children: HashMap<SessionKey, Vec<SessionKey>>,
}

struct Frozen {
    generation: u64,
    built: Built,
    loaded: usize,
    roots: RefCell<Option<Arc<[SessionKey]>>>,
}

pub struct RosterIndex {
    limits: MetadataLimits,
    max_cached: usize,
    entries: HashMap<SessionKey, Entry>,
    bytes: usize,
    scan: BTreeMap<Harness, ScanProgress>,
    limited: Option<DiscoveryLimit>,
    summaries: HashMap<SessionKey, (Arc<SessionSummary>, u64)>,
    tick: u64,
    frozen: Option<Frozen>,
    refresh_available: bool,
    /// Members that disappeared after the freeze; kept unavailable until refresh.
    deleted: HashSet<SessionKey>,
    /// Newer sources a frozen, full index could not hold; a bounded candidate set that
    /// the next freeze merges (replacing the oldest members).
    overflow: HashMap<SessionKey, (Arc<SessionMetadata>, usize)>,
    /// Bumped on every change to `entries`.
    version: u64,
    provisional: RefCell<Option<(u64, Arc<Built>)>>,
}

impl RosterIndex {
    pub fn new(limits: MetadataLimits, max_cached_sessions: usize) -> Self {
        Self {
            limits,
            max_cached: max_cached_sessions.max(1),
            entries: HashMap::new(),
            bytes: 0,
            scan: BTreeMap::new(),
            limited: None,
            summaries: HashMap::new(),
            tick: 0,
            frozen: None,
            refresh_available: false,
            deleted: HashSet::new(),
            overflow: HashMap::new(),
            version: 0,
            provisional: RefCell::new(None),
        }
    }

    /// Sources currently indexed.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Hydrated summaries currently cached (bounded by `max_cached_sessions`).
    pub fn cached_summaries(&self) -> usize {
        self.summaries.len()
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen.is_some()
    }

    /// Generation of the frozen ordering; 0 before the first freeze.
    pub fn generation(&self) -> u64 {
        self.frozen.as_ref().map_or(0, |f| f.generation)
    }

    pub fn limited(&self) -> Option<DiscoveryLimit> {
        self.limited
    }

    pub fn contains(&self, key: &SessionKey) -> bool {
        self.entries.contains_key(key)
    }

    pub fn metadata(&self, key: &SessionKey) -> Option<Arc<SessionMetadata>> {
        self.entries.get(key).map(|e| e.meta.clone())
    }

    /// The cached summary, refreshing its recency.
    pub fn summary(&mut self, key: &SessionKey) -> Option<Arc<SessionSummary>> {
        self.tick += 1;
        let tick = self.tick;
        self.summaries.get_mut(key).map(|(summary, used)| {
            *used = tick;
            summary.clone()
        })
    }

    pub fn has_summary(&self, key: &SessionKey) -> bool {
        self.summaries.contains_key(key)
    }

    /// Keys of the loaded top-level groups. Empty before the first freeze and while
    /// provisional (the provisional page is only exposed through `snapshot`).
    pub fn loaded_roots(&self) -> Vec<SessionKey> {
        match &self.frozen {
            Some(f) => f.built.order[..f.loaded].to_vec(),
            None => {
                let built = self.provisional_built();
                built.order[..built.order.len().min(ROOT_PAGE)].to_vec()
            }
        }
    }

    /// Merges one bounded discovery slice.
    pub fn apply_discovery(&mut self, delta: DiscoveryDelta) {
        self.scan.insert(delta.scan.harness, delta.scan);
        for meta in delta.sessions {
            self.upsert(meta);
        }
        for key in delta.removed {
            self.remove(&key);
        }
        self.version += 1;
    }

    fn upsert(&mut self, meta: SessionMetadata) {
        let key = meta.key.clone();
        if let Some(entry) = self.entries.get_mut(&key) {
            let merged = match self.summaries.get(&key) {
                // Hydrated facts are richer than a probe: keep them, take the newest
                // activity and the current availability.
                Some(_) => {
                    let mut merged = (*entry.meta).clone();
                    merged.activity = if meta.activity.at > merged.activity.at {
                        meta.activity
                    } else {
                        merged.activity
                    };
                    merged.source_available = meta.source_available;
                    merged
                }
                None => meta,
            };
            if merged == *entry.meta {
                self.deleted.remove(&key);
                return;
            }
            if self.frozen.is_some()
                && (merged.lineage != entry.meta.lineage || merged.members != entry.meta.members)
            {
                self.refresh_available = true;
            }
            let bytes = metadata_bytes(&merged);
            self.bytes = self.bytes - entry.bytes + bytes;
            entry.bytes = bytes;
            entry.meta = Arc::new(merged);
            self.deleted.remove(&key);
            self.sync_summary(&key);
            return;
        }
        let bytes = metadata_bytes(&meta);
        if bytes > self.limits.max_bytes {
            // Larger than the whole ceiling: never admissible, and evicting to try would
            // only empty the index.
            self.limited = Some(DiscoveryLimit {
                sources: self.limits.max_sources,
                bytes: self.limits.max_bytes,
            });
            return;
        }
        // A frozen generation keeps every member; a bounded reserve of candidates for the
        // next generation may exceed the ceiling, and a refresh trims back to it.
        let (cap_sources, cap_bytes) = if self.frozen.is_some() {
            (
                self.limits.max_sources + (self.limits.max_sources / 10).max(16),
                self.limits.max_bytes + self.limits.max_bytes / 10,
            )
        } else {
            (self.limits.max_sources, self.limits.max_bytes)
        };
        while self.entries.len() >= cap_sources || self.bytes.saturating_add(bytes) > cap_bytes {
            // At the ceiling the newest sources are kept: a newer source displaces the
            // oldest unfrozen one, an older one is dropped. Either way the limit stays
            // visible and the list is never reported complete.
            self.limited = Some(DiscoveryLimit {
                sources: self.limits.max_sources,
                bytes: self.limits.max_bytes,
            });
            let Some((victim, at)) = self.oldest(false) else {
                self.offer_candidate(meta, bytes);
                return;
            };
            // `activity_desc(new, old) == Less` means the new source is strictly newer.
            if activity_desc(meta.activity.at, at) != Ordering::Less {
                self.offer_candidate(meta, bytes);
                return;
            }
            if let Some(e) = self.entries.remove(&victim) {
                self.bytes -= e.bytes;
            }
            self.summaries.remove(&victim);
        }
        if self.frozen.is_some() {
            self.refresh_available = true;
        }
        self.bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                meta: Arc::new(meta),
                bytes,
                member: false,
            },
        );
    }

    /// Remembers a source that is newer than the oldest indexed one but does not fit,
    /// within a bounded candidate set (count reserve, byte reserve of at least one
    /// record), and advertises that a refresh would show it.
    fn offer_candidate(&mut self, meta: SessionMetadata, bytes: usize) {
        // A record larger than the whole ceiling can never be admitted, so it is not a
        // candidate: merging it would trim fitting entries away.
        if self.frozen.is_none() || bytes > self.limits.max_bytes {
            return;
        }
        let newer_than_oldest = self
            .oldest(true)
            .is_none_or(|(_, at)| activity_desc(meta.activity.at, at) == Ordering::Less);
        if !newer_than_oldest {
            return;
        }
        self.refresh_available = true;
        let max_count = (self.limits.max_sources / 10).max(16);
        let max_bytes = (self.limits.max_bytes / 10).max(bytes);
        self.overflow
            .insert(meta.key.clone(), (Arc::new(meta), bytes));
        loop {
            let total: usize = self.overflow.values().map(|(_, b)| *b).sum();
            if self.overflow.len() <= max_count && total <= max_bytes {
                break;
            }
            let oldest = self
                .overflow
                .iter()
                .min_by(|(ka, (a, _)), (kb, (b, _))| {
                    activity_desc(a.activity.at, b.activity.at)
                        .reverse()
                        .then_with(|| cmp_keys(ka, kb).reverse())
                })
                .map(|(k, _)| k.clone());
            match oldest {
                Some(k) if self.overflow.len() > 1 => {
                    self.overflow.remove(&k);
                }
                _ => break,
            }
        }
    }

    /// The oldest entry (unknown activity oldest, ties by key), optionally including
    /// members of the frozen generation.
    fn oldest(&self, members: bool) -> Option<(SessionKey, Option<Timestamp>)> {
        self.entries
            .iter()
            .filter(|(_, e)| members || !e.member)
            .min_by(|(ka, a), (kb, b)| {
                activity_desc(a.meta.activity.at, b.meta.activity.at)
                    .reverse()
                    .then_with(|| cmp_keys(ka, kb).reverse())
            })
            .map(|(k, e)| (k.clone(), e.meta.activity.at))
    }

    /// Keeps a cached summary's metadata (availability, activity) in step with the index.
    fn sync_summary(&mut self, key: &SessionKey) {
        let Some(entry) = self.entries.get(key) else {
            return;
        };
        if let Some((summary, _)) = self.summaries.get_mut(key)
            && summary.meta != *entry.meta
        {
            let mut updated = (**summary).clone();
            updated.meta = (*entry.meta).clone();
            *summary = Arc::new(updated);
        }
    }

    fn remove(&mut self, key: &SessionKey) {
        self.overflow.remove(key);
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if self.frozen.is_some() {
            self.refresh_available = true;
            if entry.member {
                // A member of the frozen generation keeps its slot, marked unavailable.
                if entry.meta.source_available {
                    let mut meta = (*entry.meta).clone();
                    meta.source_available = false;
                    entry.meta = Arc::new(meta);
                }
                self.deleted.insert(key.clone());
                let key = key.clone();
                self.sync_summary(&key);
                return;
            }
        }
        self.bytes -= entry.bytes;
        self.entries.remove(key);
        self.summaries.remove(key);
    }

    /// Orders the current entries into a new frozen generation. Deleted members are
    /// dropped now; the loaded capacity of a previous generation is kept.
    pub fn freeze(&mut self, generation: u64) {
        for key in std::mem::take(&mut self.deleted) {
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes;
            }
            self.summaries.remove(&key);
        }
        // Candidates that did not fit join the index; the trim below keeps the newest.
        for (key, (meta, bytes)) in std::mem::take(&mut self.overflow) {
            if !self.entries.contains_key(&key) {
                self.bytes += bytes;
                self.entries.insert(
                    key,
                    Entry {
                        meta,
                        bytes,
                        member: false,
                    },
                );
            }
        }
        // Trim the candidate reserve (and any overshoot) back to the ceiling, oldest first.
        while self.entries.len() > self.limits.max_sources || self.bytes > self.limits.max_bytes {
            let Some((victim, _)) = self.oldest(true) else {
                break;
            };
            if let Some(e) = self.entries.remove(&victim) {
                self.bytes -= e.bytes;
            }
            self.summaries.remove(&victim);
            self.limited = Some(DiscoveryLimit {
                sources: self.limits.max_sources,
                bytes: self.limits.max_bytes,
            });
        }
        let built = build(&self.entries);
        for entry in self.entries.values_mut() {
            entry.member = true;
        }
        let previous = self.frozen.as_ref().map_or(0, |f| f.loaded);
        let loaded = previous.max(ROOT_PAGE).min(built.order.len());
        self.frozen = Some(Frozen {
            generation,
            built,
            loaded,
            roots: RefCell::new(None),
        });
        self.refresh_available = false;
        self.version += 1;
    }

    /// Reveals the next 50 top-level groups of `generation`. A stale generation
    /// changes nothing.
    pub fn load_more(&mut self, generation: u64) -> bool {
        let Some(frozen) = self.frozen.as_mut() else {
            return false;
        };
        if frozen.generation != generation || frozen.loaded >= frozen.built.order.len() {
            return false;
        }
        frozen.loaded = (frozen.loaded + ROOT_PAGE).min(frozen.built.order.len());
        *frozen.roots.borrow_mut() = None;
        true
    }

    /// Caches a hydrated summary and folds its metadata into the index. The cache is
    /// bounded; evicting a summary never removes its metadata or ends pagination.
    pub fn update_summary(&mut self, summary: SessionSummary) -> Arc<SessionSummary> {
        let key = summary.meta.key.clone();
        let Some(entry) = self.entries.get_mut(&key) else {
            // Not indexed (dropped by a ceiling or removed): answer, but cache nothing.
            return Arc::new(summary);
        };
        if *entry.meta != summary.meta {
            if self.frozen.is_some()
                && (entry.meta.lineage != summary.meta.lineage
                    || entry.meta.members != summary.meta.members)
            {
                self.refresh_available = true;
            }
            let bytes = metadata_bytes(&summary.meta);
            self.bytes = self.bytes - entry.bytes + bytes;
            entry.bytes = bytes;
            entry.meta = Arc::new(summary.meta.clone());
        }
        self.tick += 1;
        let cached = Arc::new(summary);
        self.summaries
            .insert(key.clone(), (cached.clone(), self.tick));
        while self.summaries.len() > self.max_cached {
            let victim = self
                .summaries
                .iter()
                .filter(|(k, _)| **k != key)
                .min_by_key(|(_, (_, used))| *used)
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    self.summaries.remove(&k);
                }
                None => break,
            }
        }
        self.version += 1;
        cached
    }

    /// Records the progress of a reader whose discovery failed.
    pub fn record_scan(&mut self, scan: ScanProgress) {
        self.scan.insert(scan.harness, scan);
    }

    /// One page of `parent`'s children, newest activity first. A cursor that is not
    /// a child of `parent` yields an empty page.
    pub fn children(
        &self,
        parent: &SessionKey,
        after: Option<&SessionKey>,
        limit: usize,
    ) -> ChildPage {
        let page = |list: &[SessionKey]| {
            let start = match after {
                None => 0,
                Some(cursor) => match list.iter().position(|k| k == cursor) {
                    Some(at) => at + 1,
                    None => {
                        return ChildPage {
                            metadata: Vec::new(),
                            next: None,
                        };
                    }
                },
            };
            let end = (start + limit).min(list.len());
            let slice = &list[start.min(end)..end];
            ChildPage {
                metadata: slice
                    .iter()
                    .filter_map(|k| self.entries.get(k).map(|e| e.meta.clone()))
                    .collect(),
                next: (end < list.len()).then(|| slice.last().cloned()).flatten(),
            }
        };
        let none = Vec::new();
        match &self.frozen {
            Some(f) => page(f.built.children.get(parent).unwrap_or(&none)),
            None => {
                let built = self.provisional_built();
                page(built.children.get(parent).unwrap_or(&none))
            }
        }
    }

    /// Number of children `parent` has in the current ordering.
    pub fn child_count(&self, parent: &SessionKey) -> usize {
        match &self.frozen {
            Some(f) => f.built.children.get(parent).map_or(0, Vec::len),
            None => self
                .provisional_built()
                .children
                .get(parent)
                .map_or(0, Vec::len),
        }
    }

    fn provisional_built(&self) -> Arc<Built> {
        let mut slot = self.provisional.borrow_mut();
        if let Some((version, built)) = slot.as_ref()
            && *version == self.version
        {
            return built.clone();
        }
        let built = Arc::new(build(&self.entries));
        *slot = Some((self.version, built.clone()));
        built
    }

    pub fn snapshot(&self) -> RosterSnapshot {
        let (generation, roots, has_more, provisional) = match &self.frozen {
            Some(f) => {
                let mut cached = f.roots.borrow_mut();
                let roots = cached
                    .get_or_insert_with(|| Arc::from(f.built.order[..f.loaded].to_vec()))
                    .clone();
                (f.generation, roots, f.loaded < f.built.order.len(), false)
            }
            None => {
                let built = self.provisional_built();
                let loaded = built.order.len().min(ROOT_PAGE);
                (
                    0,
                    Arc::from(built.order[..loaded].to_vec()),
                    loaded < built.order.len(),
                    true,
                )
            }
        };
        let metadata = roots
            .iter()
            .filter_map(|k| self.entries.get(k).map(|e| e.meta.clone()))
            .collect();
        let summaries = roots
            .iter()
            .filter_map(|k| self.summaries.get(k).map(|(s, _)| s.clone()))
            .collect();
        RosterSnapshot {
            generation,
            roots,
            metadata,
            summaries,
            scan: self.scan.values().copied().collect(),
            has_more,
            limited: self.limited,
            refresh_available: self.refresh_available,
            provisional,
        }
    }
}

fn build(entries: &HashMap<SessionKey, Entry>) -> Built {
    let mut by_native: HashMap<(Harness, &str), Vec<&SessionKey>> = HashMap::new();
    for key in entries.keys() {
        by_native
            .entry((key.harness, key.native_id.as_str()))
            .or_default()
            .push(key);
    }
    let activity = |key: &SessionKey| entries.get(key).and_then(|e| e.meta.activity.at);

    // Continuation members fold into the head that lists them.
    let heads: HashSet<&SessionKey> = entries
        .iter()
        .filter(|(_, e)| e.meta.members.len() > 1)
        .map(|(k, _)| k)
        .collect();
    let mut absorbed: HashMap<&SessionKey, &SessionKey> = HashMap::new();
    for (head, entry) in entries {
        if entry.meta.members.len() < 2 {
            continue;
        }
        for member in &entry.meta.members {
            if member == head || heads.contains(member) {
                continue;
            }
            if let Some((member, _)) = entries.get_key_value(member) {
                absorbed.entry(member).or_insert(head);
            }
        }
    }
    let lead = |key: &SessionKey| -> SessionKey {
        let mut at = key;
        if let Some(head) = absorbed.get(at) {
            at = head;
        }
        at.clone()
    };

    // Native parent edges, resolved among known sources.
    let mut parent: HashMap<&SessionKey, SessionKey> = HashMap::new();
    for (key, entry) in entries {
        if absorbed.contains_key(key) {
            continue;
        }
        let target = match entry.meta.lineage.parent.as_ref() {
            Some(SessionLink::Resolved(target)) => entries.get_key_value(target).map(|(k, _)| k),
            Some(SessionLink::Unresolved(id)) => {
                let database = match &key.source {
                    SourceKey::DbRow { database, .. } => Some(database),
                    SourceKey::File(_) => None,
                };
                let candidates: Vec<&SessionKey> = by_native
                    .get(&(key.harness, id.as_str()))
                    .into_iter()
                    .flatten()
                    .copied()
                    .filter(|c| *c != key)
                    .filter(|c| match (database, &c.source) {
                        (Some(db), SourceKey::DbRow { database, .. }) => database == db,
                        (Some(_), SourceKey::File(_)) => false,
                        (None, _) => true,
                    })
                    .collect();
                let newest = candidates.iter().map(|c| activity(c)).max();
                newest.and_then(|newest| {
                    let mut top = candidates.iter().filter(|c| activity(c) == newest);
                    match (top.next(), top.next()) {
                        (Some(only), None) => Some(*only),
                        _ => None,
                    }
                })
            }
            Some(SessionLink::Ambiguous) | None => None,
        };
        if let Some(target) = target {
            let target = lead(target);
            if target != *key {
                parent.insert(key, target);
            }
        }
    }

    // Cycles are broken: every member of one becomes a top-level group.
    let mut state: HashMap<&SessionKey, u8> = HashMap::new();
    let keys: Vec<&SessionKey> = entries.keys().collect();
    for start in keys {
        if state.contains_key(start) {
            continue;
        }
        let mut path: Vec<&SessionKey> = Vec::new();
        let mut current = start;
        loop {
            match state.get(current) {
                Some(1) => {
                    if let Some(at) = path.iter().position(|k| *k == current) {
                        for member in &path[at..] {
                            parent.remove(member);
                        }
                    }
                    break;
                }
                Some(_) => break,
                None => {}
            }
            state.insert(current, 1);
            path.push(current);
            match parent.get(current).and_then(|p| entries.get_key_value(p)) {
                Some((next, _)) => current = next,
                None => break,
            }
        }
        for key in path {
            state.insert(key, 2);
        }
    }

    // Group activity: the newest of a group's own, absorbed members', and every known
    // descendant's recorded activity.
    let mut own: HashMap<&SessionKey, Option<Timestamp>> =
        entries.keys().map(|k| (k, activity(k))).collect();
    for (member, head) in &absorbed {
        let value = own[member];
        if let Some(slot) = own.get_mut(head) {
            *slot = (*slot).max(value);
        }
    }
    let mut group = own.clone();
    for (key, value) in &own {
        let mut current: &SessionKey = key;
        for _ in 0..MAX_DEPTH {
            let Some((next, _)) = parent.get(current).and_then(|p| entries.get_key_value(p)) else {
                break;
            };
            if let Some(slot) = group.get_mut(next) {
                *slot = (*slot).max(*value);
            }
            current = next;
        }
    }

    let mut order: Vec<&SessionKey> = entries
        .keys()
        .filter(|k| !absorbed.contains_key(*k) && !parent.contains_key(*k))
        .collect();
    order.sort_unstable_by(|a, b| activity_desc(group[*a], group[*b]).then_with(|| cmp_keys(a, b)));
    let mut children: HashMap<SessionKey, Vec<&SessionKey>> = HashMap::new();
    for (child, target) in &parent {
        children.entry(target.clone()).or_default().push(child);
    }
    let children = children
        .into_iter()
        .map(|(target, mut list)| {
            list.sort_unstable_by(|a, b| {
                activity_desc(group[*a], group[*b]).then_with(|| cmp_keys(a, b))
            });
            (target, list.into_iter().cloned().collect())
        })
        .collect();
    Built {
        order: order.into_iter().cloned().collect(),
        children,
    }
}
