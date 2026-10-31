//! Cross-source OMP relationships from explicit native evidence only.
//!
//! * **Artifact child**: `<dir>/<stem>/<agent>.jsonl` is a child of the discovered
//!   `<dir>/<stem>.jsonl` (the producer writes a session's children into its
//!   artifacts directory, and nested children into theirs). A header that also
//!   names a parent must agree with the directory, else the link is ambiguous.
//! * **Fork**: a top-level file whose header names a `parentSession` without
//!   `continuationOf`. A fork is a separate session, never merged.
//! * **Continuation**: `continuationOf` naming exactly one discovered session in
//!   the same directory that no other file also continues. The earliest segment
//!   is the stable logical root; its `members` list the segments in chain order.
//!
//! Matching uses only already-discovered sources: header IDs, discovered file
//! paths, and the directory shape. A `parentSession` path is compared as text
//! with discovered paths and is never opened. Titles, entry text, timestamps and
//! working directories are never ownership evidence. Two files sharing a header
//! ID stay separate rows, and a reference to that ID is ambiguous.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::domain::{Lineage, SessionKey, SessionLink, SessionMetadata, SourceKey};
use crate::providers::omp::metadata::OmpHeader;

/// Prefix of an `Unresolved` parent text recording a continuation that was
/// rejected (predecessor not discovered beside it). The control character makes
/// it unmatchable against any native session ID, so a roster cannot turn the
/// rejected edge into a parent.
pub const REJECTED_CONTINUATION: &str = "\u{1f}rejected-continuation\u{1f}";

/// What a header reference resolved to, before it is attached to a role.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Reference {
    Found(usize),
    Ambiguous,
    Missing(String),
}

fn path_of(meta: &SessionMetadata) -> Option<&Path> {
    match &meta.key.source {
        SourceKey::File(path) => Some(path),
        SourceKey::DbRow { .. } => None,
    }
}

/// Whether a `parentSession` value names a file rather than a session ID.
fn path_like(text: &str) -> bool {
    text.contains(['/', '\\']) || text.ends_with(".jsonl")
}

/// The discovered file whose artifacts directory holds `path`:
/// `<dir>/<stem>/<name>.jsonl` → `<dir>/<stem>.jsonl`.
fn artifact_owner(path: &Path) -> Option<PathBuf> {
    let dir = path.parent()?;
    let stem = dir.file_name()?;
    let mut owner = stem.to_os_string();
    owner.push(".jsonl");
    Some(dir.parent()?.join(owner))
}

struct Index<'a> {
    by_path: HashMap<&'a Path, usize>,
    by_id: HashMap<&'a str, Vec<usize>>,
    by_name: HashMap<std::ffi::OsString, Vec<usize>>,
}

impl<'a> Index<'a> {
    fn new(metadata: &'a [SessionMetadata]) -> Self {
        let mut index = Self {
            by_path: HashMap::new(),
            by_id: HashMap::new(),
            by_name: HashMap::new(),
        };
        for (i, meta) in metadata.iter().enumerate() {
            index
                .by_id
                .entry(meta.key.native_id.as_str())
                .or_default()
                .push(i);
            if let Some(path) = path_of(meta) {
                index.by_path.insert(path, i);
                if let Some(name) = path.file_name() {
                    index
                        .by_name
                        .entry(name.to_os_string())
                        .or_default()
                        .push(i);
                }
            }
        }
        index
    }

    /// A session ID or file path from a header, resolved among discovered sources.
    /// `me` never resolves to itself.
    fn resolve(&self, text: &str, me: usize) -> Reference {
        let found: Vec<usize> = if path_like(text) {
            let path = Path::new(text);
            match self.by_path.get(path) {
                Some(&i) => vec![i],
                // A recorded path may be spelled differently from the discovered
                // one (root alias, separators); the session file name is unique, so
                // it identifies the file when exactly one discovered file has it.
                None => path
                    .file_name()
                    .and_then(|name| self.by_name.get(name))
                    .cloned()
                    .unwrap_or_default(),
            }
        } else {
            self.by_id.get(text).cloned().unwrap_or_default()
        };
        let found: Vec<usize> = found.into_iter().filter(|&i| i != me).collect();
        match found.as_slice() {
            [] => Reference::Missing(text.to_owned()),
            [only] => Reference::Found(*only),
            _ => Reference::Ambiguous,
        }
    }
}

fn dir_of(meta: &SessionMetadata) -> Option<&Path> {
    path_of(meta).and_then(Path::parent)
}

/// The continuation edge of every header, `None` when it does not name one.
/// `Found(j)` means item `i` continues item `j`.
fn continuation_edges(
    metadata: &[SessionMetadata],
    headers: &[Option<&OmpHeader>],
    index: &Index<'_>,
) -> Vec<Option<Reference>> {
    let mut edges: Vec<Option<Reference>> = metadata
        .iter()
        .zip(headers)
        .enumerate()
        .map(|(i, (meta, header))| {
            let header = (*header)?;
            let target = header.continuation_of.as_deref()?;
            // The producer writes `continuationOf` equal to `parentSession`; a
            // header naming two different sessions is not an unambiguous edge.
            if header
                .parent_session
                .as_deref()
                .is_some_and(|parent| parent != target)
            {
                return Some(Reference::Ambiguous);
            }
            let candidates: Vec<usize> = index
                .by_id
                .get(target)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&j| j != i)
                .collect();
            // The continuation is written next to the file it continues.
            let near: Vec<usize> = candidates
                .iter()
                .copied()
                .filter(|&j| dir_of(&metadata[j]) == dir_of(meta))
                .collect();
            Some(match near.as_slice() {
                [only] => Reference::Found(*only),
                [] => Reference::Missing(target.to_owned()),
                _ => Reference::Ambiguous,
            })
        })
        .collect();
    // A segment two files continue is continued by neither, and a cycle has no
    // earliest segment: both stay unmerged and ambiguous.
    let mut continuers: HashMap<usize, usize> = HashMap::new();
    for edge in edges.iter().flatten() {
        if let Reference::Found(j) = edge {
            *continuers.entry(*j).or_default() += 1;
        }
    }
    for edge in &mut edges {
        if let Some(Reference::Found(j)) = edge
            && continuers.get(j).copied().unwrap_or(0) > 1
        {
            *edge = Some(Reference::Ambiguous);
        }
    }
    for start in 0..edges.len() {
        let mut seen = HashSet::new();
        let mut at = start;
        while let Some(Reference::Found(next)) = edges[at] {
            if !seen.insert(at) {
                break;
            }
            at = next;
        }
        if seen.contains(&at) && matches!(edges[at], Some(Reference::Found(_))) {
            for member in seen {
                edges[member] = Some(Reference::Ambiguous);
            }
        }
    }
    edges
}

fn link(
    reference: &Reference,
    logical_root: &[usize],
    metadata: &[SessionMetadata],
) -> SessionLink {
    match reference {
        Reference::Found(j) => SessionLink::Resolved(metadata[logical_root[*j]].key.clone()),
        Reference::Ambiguous => SessionLink::Ambiguous,
        Reference::Missing(text) => SessionLink::Unresolved(text.clone()),
    }
}

/// Resolves forks, artifact parents and continuation chains across the
/// discovered `metadata`. `headers[i]` is the header of `metadata[i]` and counts
/// only when its ID is that session's native ID, so header evidence is never
/// attributed to another file; a session with no matching header has only
/// directory evidence. Existing relationship fields are recomputed, so the call
/// is idempotent.
pub fn resolve_omp_links(metadata: &mut [SessionMetadata], headers: &[OmpHeader]) {
    let aligned: Vec<Option<&OmpHeader>> = metadata
        .iter()
        .enumerate()
        .map(|(i, meta)| headers.get(i).filter(|h| h.id == meta.key.native_id))
        .collect();
    resolve_aligned(metadata, &aligned);
}

/// [`resolve_omp_links`] for callers that know which sources have no header.
pub(super) fn resolve_aligned(metadata: &mut [SessionMetadata], headers: &[Option<&OmpHeader>]) {
    let index = Index::new(metadata);
    let edges = continuation_edges(metadata, headers, &index);

    // Chains: `next[j] = i` when `i` validly continues `j`.
    let mut next: HashMap<usize, usize> = HashMap::new();
    for (i, edge) in edges.iter().enumerate() {
        if let Some(Reference::Found(j)) = edge {
            next.insert(*j, i);
        }
    }
    let mut logical_root: Vec<usize> = (0..metadata.len()).collect();
    let mut chains: Vec<(usize, Vec<usize>)> = Vec::new();
    for (root, edge) in edges.iter().enumerate() {
        if matches!(edge, Some(Reference::Found(_))) {
            continue;
        }
        let mut members = vec![root];
        while let Some(&following) = next.get(members.last().unwrap_or(&root)) {
            logical_root[following] = root;
            members.push(following);
        }
        if members.len() > 1 {
            chains.push((root, members));
        }
    }

    let mut lineage: Vec<Lineage> = Vec::with_capacity(metadata.len());
    for (i, meta) in metadata.iter().enumerate() {
        let path = path_of(meta);
        let owner = path
            .and_then(artifact_owner)
            .and_then(|owner| index.by_path.get(owner.as_path()).copied())
            .filter(|&j| j != i);
        let mut entry = Lineage {
            sidechain: owner.is_some(),
            agent_id: owner.and_then(|_| {
                path.and_then(Path::file_stem)
                    .map(|s| s.to_string_lossy().into_owned())
            }),
            ..Lineage::default()
        };
        let continuing = matches!(edges[i], Some(Reference::Found(_)));
        if !continuing {
            let named = headers[i]
                .and_then(|h| h.parent_session.as_deref())
                .map(|parent| index.resolve(parent, i))
                // A parent that is only another segment of this logical session is
                // this session, not a relationship.
                .filter(
                    |reference| !matches!(reference, Reference::Found(j) if logical_root[*j] == i),
                );
            if let Some(s) = owner {
                let resolved = link(&Reference::Found(s), &logical_root, metadata);
                entry.parent = Some(match named {
                    // A continuation claim that is itself ambiguous (conflicting
                    // header fields, two continuers, a cycle) is not erased by the
                    // directory shape.
                    _ if matches!(edges[i], Some(Reference::Ambiguous)) => SessionLink::Ambiguous,
                    // A header naming something undiscovered cannot contradict the
                    // directory, which is the stronger evidence.
                    None | Some(Reference::Missing(_)) => resolved,
                    Some(Reference::Found(n)) if logical_root[n] == logical_root[s] => resolved,
                    Some(_) => SessionLink::Ambiguous,
                });
            } else if let Some(reference @ (Reference::Ambiguous | Reference::Missing(_))) =
                &edges[i]
            {
                // The unmerged predecessor stays visible, but never as a plain
                // unresolved ID: the roster resolves those against every source and
                // would attach the very edge this module rejected.
                entry.parent = Some(match reference {
                    Reference::Missing(text) => {
                        SessionLink::Unresolved(format!("{REJECTED_CONTINUATION}{text}"))
                    }
                    other => link(other, &logical_root, metadata),
                });
            } else if let Some(reference) = named {
                entry.fork = Some(link(&reference, &logical_root, metadata));
            }
        }
        lineage.push(entry);
    }

    let mut members_of: HashMap<usize, Vec<SessionKey>> = HashMap::new();
    let mut newest: HashMap<usize, (Option<i64>, crate::domain::AggregationClass)> = HashMap::new();
    for (root, members) in &chains {
        members_of.insert(
            *root,
            members.iter().map(|&m| metadata[m].key.clone()).collect(),
        );
        let best = members
            .iter()
            .map(|&m| (metadata[m].activity.at, metadata[m].activity.class))
            .max_by_key(|(at, _)| *at);
        if let Some(best) = best {
            newest.insert(*root, best);
        }
    }
    for (i, meta) in metadata.iter_mut().enumerate() {
        meta.lineage = std::mem::take(&mut lineage[i]);
        meta.members = members_of.remove(&i).unwrap_or_default();
        if let Some((at, class)) = newest.get(&i) {
            meta.activity.at = *at;
            meta.activity.class = *class;
        }
    }
}
