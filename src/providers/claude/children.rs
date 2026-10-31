//! Claude native subagent identity: the parent comes from the session directory
//! and exact metadata IDs, never from `parentUuid` (a message UUID) or from
//! timestamps, working directories or titles.
//!
//! A child transcript is `<project>/<session>/subagents/**/agent-<id>.jsonl`. Its
//! records usually carry `sessionId` equal to the parent's, so the child's own
//! identity is its `agentId` (the file name), and only records stamped with that
//! agent are owned by it.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::discovery::{ClaudePath, classify_claude};
use crate::domain::{Harness, Lineage, SessionKey, SessionLink, SourceKey};

/// Sidecar files are tiny; anything larger is not read.
const SIDECAR_MAX: u64 = 16 * 1024;

/// Provider-private identity of a child transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ClaudeChildId {
    /// Directory name of the owning session (its `<session>.jsonl` stem).
    pub parent_session_id: String,
    /// `agentId`, the file name without `agent-` and `.jsonl`.
    pub agent_id: String,
}

impl ClaudeChildId {
    /// The child identity of `path` under `root`, when it is a child transcript.
    pub(super) fn of(root: &Path, path: &Path) -> Option<Self> {
        match classify_claude(root, path)? {
            ClaudePath::Child { parent_session } => Some(Self {
                parent_session_id: parent_session,
                agent_id: agent_id_of(path)?,
            }),
            ClaudePath::Main => None,
        }
    }

    /// Whether a record of this child's transcript belongs to the child itself. A
    /// record stamped with another agent (or a copied parent record without an
    /// agent) is inherited history; one without any agent is the child's own only
    /// when it is marked as a sidechain record.
    pub(super) fn owns(&self, record: &Value) -> bool {
        match record.get("agentId").and_then(Value::as_str) {
            Some(id) => id == self.agent_id,
            None => record.get("isSidechain").and_then(Value::as_bool) == Some(true),
        }
    }
}

/// Parent evidence of one child. Every field is `None` unless confirmed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ClaudeParentEvidence {
    /// Owning session, from the directory the child sits under.
    pub parent_session: Option<String>,
    /// The child's own `agentId`.
    pub agent: Option<String>,
    /// Nested spawner, `parentAgentId`.
    pub parent_agent: Option<String>,
    /// Spawning tool-use ID, `parentToolUseId`.
    pub tool_use: Option<String>,
    pub depth: Option<u32>,
    pub agent_type: Option<String>,
}

/// An ID that is safe to use as a file-name part and a lookup key.
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn text(meta: &Value, key: &str) -> Option<String> {
    meta.get(key)
        .and_then(Value::as_str)
        .filter(|s| valid_id(s))
        .map(str::to_owned)
}

/// `agent-<id>.jsonl` → `<id>`.
pub(super) fn agent_id_of(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    stem.strip_prefix("agent-")
        .filter(|id| valid_id(id))
        .map(str::to_owned)
}

/// The project directory name of a child transcript.
pub(super) fn child_project(path: &Path) -> Option<String> {
    let dirs = subagents_parent(path)?;
    dirs.parent()?
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
}

/// The session directory a child transcript's `subagents` directory sits in.
fn subagents_parent(path: &Path) -> Option<&Path> {
    path.ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "subagents"))
        .and_then(Path::parent)
}

/// What the directory layout and confirmed metadata say about `path`'s parent.
/// `meta` holds only allowlisted keys: `agentId`, `parentAgentId`,
/// `parentToolUseId`, `depth`, `agentType`. Metadata that names another agent than
/// the file does is not about this child and contributes nothing.
pub(super) fn child_parent(path: &Path, meta: Option<&Value>) -> ClaudeParentEvidence {
    let agent = agent_id_of(path);
    let mut evidence = ClaudeParentEvidence {
        parent_session: subagents_parent(path)
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned()),
        agent: agent.clone(),
        ..ClaudeParentEvidence::default()
    };
    let Some(meta) = meta else {
        return evidence;
    };
    if text(meta, "agentId").is_some_and(|id| Some(&id) != agent.as_ref()) {
        return evidence;
    }
    evidence.parent_agent = text(meta, "parentAgentId").filter(|p| Some(p) != agent.as_ref());
    evidence.tool_use = text(meta, "parentToolUseId");
    evidence.depth = meta
        .get("depth")
        .and_then(Value::as_u64)
        .and_then(|d| u32::try_from(d).ok());
    evidence.agent_type = meta
        .get("agentType")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 64)
        .map(str::to_owned);
    evidence
}

/// The allowlisted keys of `agent-<id>.meta.json` next to a child transcript.
/// Only a regular, non-link file is read; a sidecar that is a link to a
/// credential, cache or audit file is refused before it is opened.
pub(super) fn sidecar(path: &Path) -> Option<Value> {
    let name = path.file_stem()?.to_str()?;
    let sidecar = path.with_file_name(format!("{name}.meta.json"));
    if !crate::discovery::plain_file(&sidecar) {
        return None;
    }
    let file = File::open(&sidecar).ok()?;
    let mut bytes = Vec::new();
    file.take(SIDECAR_MAX + 1).read_to_end(&mut bytes).ok()?;
    // Read work is charged like any other source read, so a caller's byte budget
    // covers sidecar metadata too.
    crate::perf::bytes_read(bytes.len());
    if bytes.len() as u64 > SIDECAR_MAX {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let object = value.as_object()?;
    let mut kept = serde_json::Map::new();
    for key in [
        "agentId",
        "parentAgentId",
        "parentToolUseId",
        "depth",
        "agentType",
    ] {
        if let Some(v) = object.get(key) {
            kept.insert(key.to_owned(), v.clone());
        }
    }
    Some(Value::Object(kept))
}

/// The transcript of agent `id` that a nested child names as its parent: the same
/// directory or an ancestor up to and including `subagents`, never a search.
pub(super) fn resolve_agent(path: &Path, id: &str) -> Option<PathBuf> {
    if !valid_id(id) {
        return None;
    }
    let file = format!("agent-{id}.jsonl");
    for dir in path.parent()?.ancestors() {
        let candidate = dir.join(&file);
        if candidate != path && std::fs::symlink_metadata(&candidate).is_ok_and(|m| m.is_file()) {
            return Some(candidate);
        }
        if dir.file_name().is_some_and(|n| n == "subagents") {
            break;
        }
    }
    None
}

/// The session transcript owning a child, by directory name. Only a main
/// transcript this reader already knows yields an exact key.
pub(super) fn main_path(path: &Path) -> Option<PathBuf> {
    let session = subagents_parent(path)?;
    let name = session.file_name()?.to_str()?;
    Some(session.parent()?.join(format!("{name}.jsonl")))
}

/// Separates an owning session from an agent ID in a scoped, unresolved parent
/// link, so a bare agent ID is never looked up across unrelated families.
pub(super) const FAMILY_SEPARATOR: char = '\u{1f}';

/// The parent-agent link of a child in `session`: exact when the spawner's
/// transcript sits in the child's own subagent tree, otherwise scoped to the
/// owning family (an unscoped agent ID would be guessed across families).
fn agent_link(path: &Path, parent: &str, session: Option<&str>) -> SessionLink {
    match resolve_agent(path, parent) {
        Some(file) => SessionLink::Resolved(SessionKey {
            harness: Harness::Claude,
            native_id: parent.to_owned(),
            source: SourceKey::File(file),
        }),
        None => SessionLink::Unresolved(match session {
            Some(session) => format!("{session}{FAMILY_SEPARATOR}{parent}"),
            None => parent.to_owned(),
        }),
    }
}

/// The owning session and agent of a scoped unresolved link, if it has a scope.
pub(super) fn scoped_parent(link: &SessionLink) -> Option<(&str, &str)> {
    let SessionLink::Unresolved(text) = link else {
        return None;
    };
    text.split_once(FAMILY_SEPARATOR)
}

/// A child's lineage from `evidence`. `main` is the owning session's key when it
/// is already known.
pub(super) fn child_lineage(
    path: &Path,
    evidence: &ClaudeParentEvidence,
    main: Option<SessionKey>,
) -> Lineage {
    let parent = match &evidence.parent_agent {
        Some(parent) => Some(agent_link(path, parent, evidence.parent_session.as_deref())),
        None => evidence.parent_session.as_ref().map(|session| {
            main.map_or_else(
                || SessionLink::Unresolved(session.clone()),
                SessionLink::Resolved,
            )
        }),
    };
    Lineage {
        parent,
        fork: None,
        sidechain: true,
        agent_id: evidence.agent.clone(),
        agent_type: evidence.agent_type.clone(),
        parent_agent_id: evidence.parent_agent.clone(),
        spawning_tool_id: evidence.tool_use.clone(),
        spawn_depth: evidence.depth,
    }
}

/// What a child state keeps of its identity across rewrites of its source.
pub(super) struct ChildState {
    pub id: ClaudeChildId,
    pub lineage: Lineage,
}

impl ChildState {
    /// Takes a nested spawner from an owned record when no sidecar named one.
    /// Returns whether the lineage changed.
    pub(super) fn absorb(&mut self, path: &Path, record: &Value) -> bool {
        if self.lineage.parent_agent_id.is_some() || !self.id.owns(record) {
            return false;
        }
        let mut kept = serde_json::Map::new();
        for key in ["agentId", "parentAgentId", "parentToolUseId", "depth"] {
            if let Some(v) = record.get(key) {
                kept.insert(key.to_owned(), v.clone());
            }
        }
        let evidence = child_parent(path, Some(&Value::Object(kept)));
        let Some(parent) = evidence.parent_agent else {
            return false;
        };
        self.lineage.parent = Some(agent_link(
            path,
            &parent,
            evidence.parent_session.as_deref(),
        ));
        self.lineage.parent_agent_id = Some(parent);
        self.lineage.spawning_tool_id = self.lineage.spawning_tool_id.take().or(evidence.tool_use);
        self.lineage.spawn_depth = self.lineage.spawn_depth.or(evidence.depth);
        true
    }
}
