//! Bounded entry/active-leaf index of one OMP session file.
//!
//! Only entry headers are kept (id, parent id, the three-state `activeLeafId`
//! marker and the file ordinal), never bodies. The active branch follows the
//! pinned producer's replay rule: the last entry in *file order* selects the
//! leaf, `activeLeafId` absent selects that entry itself, an explicit `null`
//! clears the leaf and a string selects that entry (an off-branch write such as
//! late usage restores the leaf captured at append time). Timestamps are never
//! consulted. A dangling or cyclic reference is reported, not repaired: the
//! producer's own recovery (falling back to the newest self-selecting entry) is
//! version-bound and stays out of this generic index.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde_json::Value;

/// Fixed per-node cost added to the id lengths when bounding retained bytes.
const NODE_OVERHEAD: usize = 64;
/// Longest entry or parent id accepted from a record.
const ID_MAX: usize = 256;

/// The producer's exact `activeLeafId` states.
#[derive(Clone, PartialEq, Eq)]
pub enum ActiveLeaf {
    /// Property absent: this entry selects itself as the leaf.
    Absent,
    /// Explicit `null`: no active leaf.
    Cleared,
    /// A string: that entry is the leaf.
    Set(String),
}

impl fmt::Debug for ActiveLeaf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Absent => f.write_str("Absent"),
            Self::Cleared => f.write_str("Cleared"),
            Self::Set(id) => write!(f, "Set(len {})", id.len()),
        }
    }
}

/// What one entry line says about tree structure.
#[derive(Clone, PartialEq, Eq)]
pub struct OmpEntryHeader {
    pub id: String,
    pub parent_id: Option<String>,
    pub active_leaf: ActiveLeaf,
    /// Position of the entry in the file (record order), not a timestamp.
    pub ordinal: u64,
}

impl fmt::Debug for OmpEntryHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OmpEntryHeader")
            .field("parent", &self.parent_id.is_some())
            .field("active_leaf", &self.active_leaf)
            .field("ordinal", &self.ordinal)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmpInsert {
    Inserted,
    /// The id was already indexed; the first structure is kept.
    Duplicate,
    /// The entry or byte ceiling was hit; the index is incomplete from now on.
    Limited,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmpTreeError {
    /// The selected leaf, or a parent on its path, is not in the index.
    MissingLeaf,
    Cycle,
    /// The index dropped entries at its ceiling, so no path is certain.
    Limited,
}

impl fmt::Display for OmpTreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::MissingLeaf => "active leaf or an ancestor is missing",
            Self::Cycle => "entry parents form a cycle",
            Self::Limited => "entry index limit reached",
        })
    }
}

impl std::error::Error for OmpTreeError {}

fn entry_id(text: &str) -> Option<String> {
    (!text.is_empty() && text.len() <= ID_MAX && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

/// The structural header of an entry record at `ordinal`. The session header and
/// the physical title slot are not entries; an entry with an unrecognized
/// `parentId`/`activeLeafId` shape is not indexed rather than guessed.
pub fn parse_entry_header(value: &Value, ordinal: u64) -> Option<OmpEntryHeader> {
    let object = value.as_object()?;
    if matches!(object.get("type")?.as_str()?, "session" | "title") {
        return None;
    }
    let id = entry_id(object.get("id")?.as_str()?)?;
    let parent_id = match object.get("parentId") {
        None | Some(Value::Null) => None,
        Some(Value::String(parent)) => Some(entry_id(parent)?),
        Some(_) => return None,
    };
    let active_leaf = match object.get("activeLeafId") {
        None => ActiveLeaf::Absent,
        Some(Value::Null) => ActiveLeaf::Cleared,
        Some(Value::String(leaf)) => ActiveLeaf::Set(entry_id(leaf)?),
        Some(_) => return None,
    };
    Some(OmpEntryHeader {
        id,
        parent_id,
        active_leaf,
        ordinal,
    })
}

struct Node {
    parent: Option<String>,
    ordinal: u64,
}

/// Headers of one session file, bounded by entry count and retained bytes.
pub struct OmpTree {
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
    nodes: HashMap<String, Node>,
    /// The last entry in file order decides the leaf.
    last: Option<(u64, String, ActiveLeaf)>,
    limited: bool,
}

impl OmpTree {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            max_entries,
            max_bytes,
            bytes: 0,
            nodes: HashMap::new(),
            last: None,
            limited: false,
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

    fn note_last(&mut self, header: &OmpEntryHeader) {
        if self
            .last
            .as_ref()
            .is_none_or(|(ordinal, ..)| header.ordinal >= *ordinal)
        {
            self.last = Some((
                header.ordinal,
                header.id.clone(),
                header.active_leaf.clone(),
            ));
        }
    }

    pub fn insert(&mut self, header: OmpEntryHeader) -> OmpInsert {
        if self.nodes.contains_key(&header.id) {
            // A repeated id keeps its first structure but is still the newest line
            // in the file, so it still decides the replayed leaf.
            self.note_last(&header);
            return OmpInsert::Duplicate;
        }
        let cost =
            NODE_OVERHEAD + header.id.len() + header.parent_id.as_ref().map_or(0, String::len);
        if self.nodes.len() >= self.max_entries || self.bytes.saturating_add(cost) > self.max_bytes
        {
            self.limited = true;
            return OmpInsert::Limited;
        }
        self.note_last(&header);
        self.bytes += cost;
        self.nodes.insert(
            header.id,
            Node {
                parent: header.parent_id,
                ordinal: header.ordinal,
            },
        );
        OmpInsert::Inserted
    }

    /// Ordinals of the active branch, root first. Empty for an empty file or a
    /// cleared leaf.
    pub fn active_path(&self) -> Result<Vec<u64>, OmpTreeError> {
        if self.limited {
            return Err(OmpTreeError::Limited);
        }
        let Some((_, id, leaf)) = &self.last else {
            return Ok(Vec::new());
        };
        let start = match leaf {
            ActiveLeaf::Cleared => return Ok(Vec::new()),
            ActiveLeaf::Absent => id,
            ActiveLeaf::Set(target) => target,
        };
        let mut seen: HashSet<&str> = HashSet::new();
        let mut path = Vec::new();
        let mut current = start.as_str();
        loop {
            let node = self.nodes.get(current).ok_or(OmpTreeError::MissingLeaf)?;
            if !seen.insert(current) {
                return Err(OmpTreeError::Cycle);
            }
            path.push(node.ordinal);
            match node.parent.as_deref() {
                Some(parent) => current = parent,
                None => break,
            }
        }
        path.reverse();
        Ok(path)
    }
}
