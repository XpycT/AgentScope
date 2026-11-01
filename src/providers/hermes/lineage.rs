//! Pure Hermes logical-lineage resolution.
//!
//! A Hermes conversation can be split physically by compression while remaining
//! one logical conversation, and can spawn branch, reset and delegate children.
//! The producer binds the fork markers (`_delegate_from`, `_branched_from`,
//! `_reset_from`) to the *actual* parent id: a compression continuation inherits
//! its parent's `model_config` verbatim, so marker presence alone must never turn
//! a continuation into a new child. Edges are resolved here from bounded session
//! row metadata only; no timing, goal text or cost matching is used, and rows from
//! different physical databases never share a chain.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::domain::{Harness, SessionKey, SessionLink, SourceKey, Timestamp};

/// The three creation markers the producer persists inside `model_config`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HermesMarkers {
    pub delegate_from: Option<String>,
    pub branched_from: Option<String>,
    pub reset_from: Option<String>,
}

impl HermesMarkers {
    /// True when any marker is present. Presence alone is not an edge.
    pub fn any(&self) -> bool {
        self.delegate_from.is_some() || self.branched_from.is_some() || self.reset_from.is_some()
    }

    /// True when a marker names `parent`.
    fn binds_to(&self, parent: &str) -> bool {
        [
            self.delegate_from.as_deref(),
            self.branched_from.as_deref(),
            self.reset_from.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|id| id == parent)
    }

    /// True when a marker names some id other than `parent`.
    fn names_other(&self, parent: &str) -> bool {
        [
            self.delegate_from.as_deref(),
            self.branched_from.as_deref(),
            self.reset_from.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|id| id != parent)
    }
}

/// One bounded session row: the exact relation inputs plus its native DB key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HermesRowMeta {
    pub key: SessionKey,
    pub id: String,
    pub parent: Option<String>,
    pub source: Option<String>,
    pub markers: HermesMarkers,
    pub started: Option<Timestamp>,
    pub ended: Option<Timestamp>,
    pub end_reason: Option<String>,
}

impl HermesRowMeta {
    /// A row keyed by its own physical database and native id.
    pub fn new(database: &Path, id: &str) -> Self {
        Self {
            key: SessionKey {
                harness: Harness::Hermes,
                native_id: id.to_owned(),
                source: SourceKey::DbRow {
                    database: database.to_path_buf(),
                    table: "sessions".to_owned(),
                    id: id.to_owned(),
                },
            },
            id: id.to_owned(),
            parent: None,
            source: None,
            markers: HermesMarkers::default(),
            started: None,
            ended: None,
            end_reason: None,
        }
    }

    pub fn with_parent(mut self, parent: Option<&str>) -> Self {
        self.parent = parent.map(str::to_owned);
        self
    }

    pub fn with_source(mut self, source: &str) -> Self {
        self.source = Some(source.to_owned());
        self
    }

    pub fn with_markers(mut self, markers: HermesMarkers) -> Self {
        self.markers = markers;
        self
    }

    pub fn ending(mut self, ended: Option<Timestamp>, reason: Option<&str>) -> Self {
        self.ended = ended;
        self.end_reason = reason.map(str::to_owned);
        self
    }

    fn database(&self) -> &Path {
        match &self.key.source {
            SourceKey::DbRow { database, .. } => database.as_path(),
            SourceKey::File(path) => path.as_path(),
        }
    }
}

/// How a row continues its actual parent. Every fork marker is compared with the
/// recorded parent id before a compression continuation is considered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HermesEdge {
    Root,
    Delegate,
    Branch,
    Reset,
    Compression,
    Unresolved,
}

/// Why an edge could not be proven. Kept inspectable instead of guessing a chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HermesUnresolved {
    MissingParent(String),
    AmbiguousParent(String),
    AmbiguousContinuation(String),
    Cycle,
    MarkerMismatch(String),
    DuplicateId(String),
}

/// Classifies one row against its actual parent row (if resolved in this DB).
///
/// Marker edges are applied first. An inherited marker naming another id is not a
/// fork; the row can still be a compression continuation. Only an explicit
/// producer continuation (parent ended `compression`, child not a tool session)
/// merges segments. A `source='subagent'` child with an actual parent is the
/// documented format-evidenced legacy fallback and is labeled by the caller.
pub fn classify_edge(row: &HermesRowMeta, parent: Option<&HermesRowMeta>) -> HermesEdge {
    let Some(parent) = parent else {
        // A marker without an actual parent row proves no edge.
        return if row.markers.any() {
            HermesEdge::Unresolved
        } else {
            HermesEdge::Root
        };
    };
    if row.id == parent.id {
        return HermesEdge::Unresolved;
    }
    if row.markers.delegate_from.as_deref() == Some(parent.id.as_str()) {
        return HermesEdge::Delegate;
    }
    if row.markers.branched_from.as_deref() == Some(parent.id.as_str()) {
        return HermesEdge::Branch;
    }
    if row.markers.reset_from.as_deref() == Some(parent.id.as_str()) {
        return HermesEdge::Reset;
    }
    // Continuations inherit model_config verbatim; markers naming another row are
    // inherited state, not a fork. The producer excludes tool sessions.
    if parent.end_reason.as_deref() == Some("compression") && row.source.as_deref() != Some("tool")
    {
        return HermesEdge::Compression;
    }
    if row.source.as_deref() == Some("subagent") {
        // Format-evidenced legacy fallback; `HermesLogicalSession::legacy` labels it.
        return HermesEdge::Delegate;
    }
    if row.markers.names_other(parent.id.as_str()) {
        return HermesEdge::Unresolved;
    }
    HermesEdge::Unresolved
}

/// Explicit relation to the source conversation for a non-root logical session.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HermesRelation {
    pub parent: Option<SessionLink>,
    pub fork: Option<SessionLink>,
    pub unresolved: Option<HermesUnresolved>,
    /// The edge came from the documented legacy `source='subagent'` fallback.
    pub legacy: bool,
}

/// One logical conversation: a compression chain (`members` root→tip), keyed by
/// its stable earliest root segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HermesLogicalSession {
    pub key: SessionKey,
    pub members: Vec<SessionKey>,
    pub edge: HermesEdge,
    pub relation: HermesRelation,
}

impl HermesLogicalSession {
    pub fn contains(&self, key: &SessionKey) -> bool {
        self.members.iter().any(|member| member == key)
    }

    /// Applies the chain's members and explicit relation to discovery metadata.
    pub fn apply(&self, meta: &mut crate::domain::SessionMetadata) {
        meta.key = self.key.clone();
        meta.members = self.members.clone();
        meta.lineage.parent = self.relation.parent.clone();
        meta.lineage.fork = self.relation.fork.clone();
        meta.lineage.sidechain = self.edge == HermesEdge::Delegate;
    }
}

/// A resolution result that also says whether the input violated a uniqueness or
/// acyclic assumption. Callers keep these sessions visible instead of dropping
/// rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HermesLineage {
    pub sessions: Vec<HermesLogicalSession>,
}

impl HermesLineage {
    pub fn for_key(&self, key: &SessionKey) -> Option<&HermesLogicalSession> {
        self.sessions.iter().find(|session| session.contains(key))
    }
}

type RowId = (PathBuf, String);

/// Resolves every row into logical sessions. Rows are indexed by `(database, id)`
/// so equal native ids in two profile databases never share an edge. Ambiguous
/// parents, continuation fan-out, duplicate rows and cycles stay unresolved roots.
pub fn resolve_chains(rows: &[HermesRowMeta]) -> Vec<HermesLogicalSession> {
    let resolution = resolve(rows);
    resolution.sessions
}

/// Full resolution, including the per-row edges used by the inspector tests.
pub fn resolve_lineage(rows: &[HermesRowMeta]) -> HermesLineage {
    resolve(rows)
}

struct RowState<'a> {
    row: &'a HermesRowMeta,
    parent: Option<usize>,
    edge: HermesEdge,
    unresolved: Option<HermesUnresolved>,
    legacy: bool,
    continuation_children: usize,
}

fn resolve(rows: &[HermesRowMeta]) -> HermesLineage {
    let mut index: HashMap<RowId, Vec<usize>> = HashMap::new();
    for (at, row) in rows.iter().enumerate() {
        index
            .entry((row.database().to_path_buf(), row.id.clone()))
            .or_default()
            .push(at);
    }
    let mut states: Vec<RowState<'_>> = rows
        .iter()
        .map(|row| RowState {
            row,
            parent: None,
            edge: HermesEdge::Unresolved,
            unresolved: None,
            legacy: false,
            continuation_children: 0,
        })
        .collect();

    for at in 0..rows.len() {
        let row = &rows[at];
        if index
            .get(&(row.database().to_path_buf(), row.id.clone()))
            .is_some_and(|hits| hits.len() > 1)
        {
            states[at].unresolved = Some(HermesUnresolved::DuplicateId(row.id.clone()));
            continue;
        }
        let Some(parent_id) = row.parent.as_deref().filter(|id| !id.is_empty()) else {
            states[at].edge = classify_edge(row, None);
            if states[at].edge == HermesEdge::Unresolved {
                states[at].unresolved = Some(HermesUnresolved::MarkerMismatch(
                    row.markers
                        .delegate_from
                        .clone()
                        .or_else(|| row.markers.branched_from.clone())
                        .or_else(|| row.markers.reset_from.clone())
                        .unwrap_or_default(),
                ));
            }
            continue;
        };
        let key = (row.database().to_path_buf(), parent_id.to_owned());
        match index.get(&key).map(Vec::as_slice) {
            Some([parent_at]) => {
                let parent = &rows[*parent_at];
                states[at].parent = Some(*parent_at);
                states[at].edge = classify_edge(row, Some(parent));
                states[at].legacy = states[at].edge == HermesEdge::Delegate
                    && !row.markers.binds_to(parent.id.as_str());
            }
            Some(_) => {
                states[at].unresolved =
                    Some(HermesUnresolved::AmbiguousParent(parent_id.to_owned()));
            }
            None => {
                states[at].unresolved = Some(HermesUnresolved::MissingParent(parent_id.to_owned()));
            }
        }
    }

    mark_cycles(&mut states, rows.len());
    count_continuations(&mut states);
    let ambiguous_parents: HashSet<usize> = states
        .iter()
        .enumerate()
        .filter(|(_, state)| state.continuation_children > 1)
        .map(|(at, _)| at)
        .collect();
    for state in &mut states {
        if state.unresolved.is_none()
            && state.edge == HermesEdge::Compression
            && state
                .parent
                .is_some_and(|parent| ambiguous_parents.contains(&parent))
        {
            state.unresolved = Some(HermesUnresolved::AmbiguousContinuation(
                state
                    .row
                    .parent
                    .clone()
                    .unwrap_or_else(|| state.row.id.clone()),
            ));
            state.edge = HermesEdge::Unresolved;
        }
    }

    // Union compression edges after the unresolved pass: an ambiguous row starts
    // its own chain rather than being merged into a guessed one.
    let mut parent_of: Vec<usize> = (0..rows.len()).collect();
    let mut continuation_child: HashMap<usize, usize> = HashMap::new();
    for (at, state) in states.iter().enumerate() {
        if state.unresolved.is_none()
            && state.edge == HermesEdge::Compression
            && let Some(parent) = state.parent
            && states[parent].unresolved.is_none()
        {
            union(&mut parent_of, at, parent);
            continuation_child.insert(parent, at);
        }
    }
    let mut components: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for at in 0..rows.len() {
        let root = find(&mut parent_of, at);
        components.entry(root).or_default().push(at);
    }

    let mut chain_root_for_row: Vec<Option<usize>> = vec![None; rows.len()];
    let mut chains: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (component, members) in components {
        // The structural root is the member whose compression parent is outside
        // this component; the chain is then followed through continuation edges.
        // This keeps the key stable even when recorded start times are odd.
        let first = members
            .iter()
            .copied()
            .find(|at| {
                states[*at]
                    .parent
                    .is_none_or(|parent| find(&mut parent_of, parent) != component)
            })
            .unwrap_or_else(|| {
                members
                    .iter()
                    .copied()
                    .min_by_key(|at| sort_key(&states[*at]))
                    .unwrap_or(0)
            });
        let mut ordered = Vec::with_capacity(members.len());
        let mut seen_ordered = HashSet::with_capacity(members.len());
        let mut current = Some(first);
        while let Some(at) = current {
            if !seen_ordered.insert(at) {
                break;
            }
            ordered.push(at);
            current = continuation_child.get(&at).copied();
        }
        for member in &members {
            if seen_ordered.insert(*member) {
                ordered.push(*member);
            }
        }
        chains.insert(component, ordered.clone());
        for member in ordered {
            chain_root_for_row[member] = Some(first);
        }
    }

    let mut sessions = Vec::new();
    for (root, members) in chains {
        let first = members[0];
        let mut relation = HermesRelation::default();
        let row = states[first].row;
        let edge = states[first].edge;
        let unresolved = states[first].unresolved.clone();
        if let Some(reason) = unresolved {
            relation.unresolved = Some(reason.clone());
            relation.parent = match reason {
                HermesUnresolved::MissingParent(id)
                | HermesUnresolved::AmbiguousParent(id)
                | HermesUnresolved::AmbiguousContinuation(id)
                | HermesUnresolved::MarkerMismatch(id)
                | HermesUnresolved::DuplicateId(id) => Some(SessionLink::Unresolved(id)),
                HermesUnresolved::Cycle => None,
            };
        } else {
            match edge {
                HermesEdge::Delegate => {
                    relation.legacy = states[first].legacy;
                    relation.parent = states[first].parent.map(|parent_at| {
                        let parent_first = chain_root_for_row[parent_at].unwrap_or(parent_at);
                        SessionLink::Resolved(states[parent_first].row.key.clone())
                    });
                    // An explicit delegate edge whose parent vanished is unresolved.
                    if relation.parent.is_none() {
                        relation.unresolved = Some(HermesUnresolved::MissingParent(
                            row.parent.clone().unwrap_or_default(),
                        ));
                    }
                }
                HermesEdge::Branch | HermesEdge::Reset => {
                    relation.fork = states[first].parent.map(|parent_at| {
                        let parent_first = chain_root_for_row[parent_at].unwrap_or(parent_at);
                        SessionLink::Resolved(states[parent_first].row.key.clone())
                    });
                    if relation.fork.is_none() {
                        relation.unresolved = Some(HermesUnresolved::MissingParent(
                            row.parent.clone().unwrap_or_default(),
                        ));
                    }
                }
                HermesEdge::Root => {}
                HermesEdge::Compression => {
                    // Only an unresolved compression edge reaches this branch.
                    relation.unresolved = Some(HermesUnresolved::MarkerMismatch(
                        row.parent.clone().unwrap_or_default(),
                    ));
                }
                HermesEdge::Unresolved => {
                    relation.unresolved = Some(HermesUnresolved::MarkerMismatch(
                        row.parent.clone().unwrap_or_default(),
                    ));
                }
            }
        }
        sessions.push(HermesLogicalSession {
            key: row.key.clone(),
            members: members
                .iter()
                .map(|at| states[*at].row.key.clone())
                .collect(),
            edge,
            relation,
        });
        let _ = root;
    }
    sessions.sort_by(|a, b| a.key.native_id.cmp(&b.key.native_id));
    HermesLineage { sessions }
}

fn sort_key<'a>(state: &RowState<'a>) -> (Option<Timestamp>, Option<&'a str>, &'a str) {
    (
        state.row.started,
        state.row.parent.as_deref(),
        state.row.id.as_str(),
    )
}

fn mark_cycles(states: &mut [RowState<'_>], count: usize) {
    let mut settled = vec![false; count];
    for start in 0..count {
        if settled[start] {
            continue;
        }
        let mut path: Vec<usize> = Vec::new();
        let mut seen_at: HashMap<usize, usize> = HashMap::new();
        let mut current = Some(start);
        while let Some(at) = current {
            if let Some(offset) = seen_at.get(&at) {
                for member in &path[*offset..] {
                    states[*member].unresolved = Some(HermesUnresolved::Cycle);
                    states[*member].edge = HermesEdge::Unresolved;
                }
                break;
            }
            if settled[at] {
                break;
            }
            seen_at.insert(at, path.len());
            path.push(at);
            current = states[at].parent;
        }
        for at in path {
            settled[at] = true;
        }
    }
}

fn count_continuations(states: &mut [RowState<'_>]) {
    let continuation: Vec<Option<usize>> = states
        .iter()
        .map(|state| {
            (state.unresolved.is_none() && state.edge == HermesEdge::Compression)
                .then_some(state.parent)
                .flatten()
        })
        .collect();
    for parent in continuation.into_iter().flatten() {
        states[parent].continuation_children += 1;
    }
}

fn find(parent: &mut [usize], at: usize) -> usize {
    let mut root = at;
    while parent[root] != root {
        root = parent[root];
    }
    let mut node = at;
    while parent[node] != root {
        let next = parent[node];
        parent[node] = root;
        node = next;
    }
    root
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let ra = find(parent, a);
    let rb = find(parent, b);
    if ra != rb {
        parent[ra.max(rb)] = ra.min(rb);
    }
}
