//! Path-specific Claude metadata: bounded discovery probes, per-record origin
//! evidence and the allowlisted keys of Claude Desktop Code session manifests.
//!
//! A manifest is read only for `cliSessionId`, an optional `entrypoint`, an
//! archive flag and release/delete markers. Nothing else in it, and nothing next
//! to it (authentication, caches, audit logs, VM disks), is opened.

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::SystemTime;

use serde_json::Value;

use super::children::{
    ChildState, ClaudeChildId, child_lineage, child_parent, child_project, sidecar,
};
use super::{State, safe, string};
use crate::discovery::{ClaudePath, SourceFile, classify_claude};
use crate::domain::{
    OriginObservation, OriginSurface, RefreshBudget, ReleaseState, SessionKey, SessionMetadata,
    SourceKey, Timestamp,
};
use crate::pricing::{PriceSchedule, ReferencePolicy};
use crate::providers::ProviderError;
use crate::providers::probe::{HEAD_RECORDS, probe_file, probe_value};

/// A manifest larger than this is not read.
const MANIFEST_MAX: u64 = 64 * 1024;
/// Distinct origin observations kept per session.
pub(super) const MAX_ORIGINS: usize = 8;
/// Layout evidence recorded for sessions found through a Desktop manifest.
pub(super) const MANIFEST_ORIGIN: &str = "claude-code-sessions";
/// Layout evidence recorded for sessions found under a Cowork project root.
pub(super) const COWORK_ORIGIN: &str = "local-agent-mode-sessions";

/// The surface a per-record `entrypoint` value names. Values this reader has not
/// confirmed stay `Unknown`; the raw text is kept beside them.
pub(super) fn desktop_origin(entrypoint: Option<&str>) -> OriginSurface {
    match entrypoint {
        Some("cli") => OriginSurface::Cli,
        Some("claude-desktop") => OriginSurface::DesktopCode,
        Some(e) if e.starts_with("claude-vscode") || e.starts_with("claude-jetbrains") => {
            OriginSurface::Ide
        }
        Some(e) if e.starts_with("sdk-") => OriginSurface::Other,
        _ => OriginSurface::Unknown,
    }
}

/// The origin evidence of one record.
pub(super) fn record_origin(record: &Value, at: Option<Timestamp>) -> Option<OriginObservation> {
    let raw = string(record, "entrypoint")?;
    let raw = safe(&raw).preview;
    Some(OriginObservation::new(
        desktop_origin(Some(&raw)),
        Some(&raw),
        at,
    ))
}

/// Adds `origin` unless the same evidence is already recorded or the bound is hit.
pub(super) fn add_origin(origins: &mut Vec<OriginObservation>, origin: OriginObservation) {
    if origins.len() < MAX_ORIGINS
        && !origins
            .iter()
            .any(|o| o.surface == origin.surface && o.raw == origin.raw)
    {
        origins.push(origin);
    }
}

/// Allowlisted facts of one Desktop Code session manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct DesktopManifest {
    pub entrypoint: Option<String>,
    pub released: ReleaseState,
    pub archived: bool,
}

/// One plane of Desktop markers: release and deletion are separate evidence, so
/// a false deletion flag can never cancel a confirmed release.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Marker {
    Absent,
    Yes,
    No,
    Unknown,
}

fn marker(value: Option<&Value>) -> Marker {
    match value {
        None | Some(Value::Null) => Marker::Absent,
        Some(Value::Bool(true)) => Marker::Yes,
        Some(Value::Bool(false)) => Marker::No,
        Some(_) => Marker::Unknown,
    }
}

fn timestamp_marker(value: Option<&Value>) -> Marker {
    match value {
        None | Some(Value::Null) => Marker::Absent,
        Some(Value::String(s)) if !s.is_empty() => Marker::Yes,
        Some(Value::Number(n)) if n.as_i64().is_some_and(|n| n > 0) => Marker::Yes,
        Some(_) => Marker::Unknown,
    }
}

/// Combines the markers of one plane: confirmed evidence beats a false
/// companion; an unrecognized shape with no confirmation is not evidence.
fn plane(markers: impl IntoIterator<Item = Marker>) -> Marker {
    let (mut yes, mut no, mut unknown) = (false, false, false);
    for marker in markers {
        match marker {
            Marker::Yes => yes = true,
            Marker::No => no = true,
            Marker::Unknown => unknown = true,
            Marker::Absent => {}
        }
    }
    if yes {
        Marker::Yes
    } else if unknown {
        Marker::Unknown
    } else if no {
        Marker::No
    } else {
        Marker::Absent
    }
}

/// Release/delete markers, each plane read independently: a manifest that marks
/// the session released keeps it released even if a deletion flag is false (and
/// the reverse). Only unconfirmed shapes leave the state `Unknown`.
pub(super) fn release_state(manifest: &Value) -> ReleaseState {
    let released = plane([
        marker(manifest.get("released")),
        marker(manifest.get("isReleased")),
        timestamp_marker(manifest.get("releasedAt")),
    ]);
    let deleted = plane([
        marker(manifest.get("deleted")),
        marker(manifest.get("isDeleted")),
        timestamp_marker(manifest.get("deletedAt")),
    ]);
    match (released, deleted) {
        (Marker::Yes, _) | (_, Marker::Yes) => ReleaseState::Released,
        (Marker::Unknown, _) | (_, Marker::Unknown) => ReleaseState::Unknown,
        (Marker::No, _) | (_, Marker::No) => ReleaseState::Present,
        _ => ReleaseState::Unknown,
    }
}

fn session_id(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Reads one manifest: its `cliSessionId` and allowlisted facts, with the bytes
/// read. A manifest without a usable `cliSessionId` is not about a transcript, and
/// a manifest that is a link (or other reparse point) is never opened.
pub(super) fn read_manifest(path: &Path) -> (Option<(String, DesktopManifest)>, usize) {
    if !crate::discovery::plain_file(path) {
        return (None, 0);
    }
    let Ok(file) = File::open(path) else {
        return (None, 0);
    };
    let mut bytes = Vec::new();
    if file.take(MANIFEST_MAX + 1).read_to_end(&mut bytes).is_err() {
        return (None, bytes.len());
    }
    // Charged like any other source read; a linked store is never opened at all.
    crate::perf::bytes_read(bytes.len());
    let read = bytes.len();
    if read as u64 > MANIFEST_MAX {
        return (None, read);
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return (None, read);
    };
    let Some(id) = value
        .get("cliSessionId")
        .and_then(Value::as_str)
        .filter(|s| session_id(s))
    else {
        return (None, read);
    };
    let manifest = DesktopManifest {
        entrypoint: string(&value, "entrypoint").map(|e| safe(&e).preview),
        released: release_state(&value),
        archived: value.get("isArchived").and_then(Value::as_bool) == Some(true),
    };
    (Some((id.to_owned(), manifest)), read)
}

impl DesktopManifest {
    /// Evidence this manifest adds to a session's origins and release state.
    pub(super) fn apply(&self, overlay: &mut Overlay) {
        add_origin(
            &mut overlay.origins,
            OriginObservation::new(OriginSurface::DesktopCode, Some(MANIFEST_ORIGIN), None),
        );
        if let Some(entrypoint) = &self.entrypoint {
            add_origin(
                &mut overlay.origins,
                OriginObservation::new(desktop_origin(Some(entrypoint)), Some(entrypoint), None),
            );
        }
        overlay.released = self.released;
        overlay.archived = self.archived;
    }

    /// Two records for the same `cliSessionId`: facts they disagree on are not
    /// evidence for either.
    pub(super) fn merge(&mut self, other: &Self) {
        if self.entrypoint != other.entrypoint {
            self.entrypoint = None;
        }
        if self.released != other.released {
            self.released = ReleaseState::Unknown;
        }
        self.archived = self.archived && other.archived;
    }
}

/// Facts that come from where a session sits and from Desktop metadata rather
/// than from its records; they survive a rewrite of the source.
#[derive(Clone, Debug, Default)]
pub(super) struct Overlay {
    pub origins: Vec<OriginObservation>,
    pub released: ReleaseState,
    pub archived: bool,
}

impl State {
    /// Binds this state to its child identity: the owning session and agent come
    /// from the path (and confirmed sidecar), never from the records' `sessionId`.
    pub(super) fn bind_child(&mut self, root: &Path, main: Option<SessionKey>) {
        let SourceKey::File(path) = self.summary.meta.key.source.clone() else {
            return;
        };
        let Some(id) = ClaudeChildId::of(root, &path) else {
            return;
        };
        let evidence = child_parent(&path, sidecar(&path).as_ref());
        let lineage = child_lineage(&path, &evidence, main);
        self.restore_child(ChildState { id, lineage });
    }

    /// Installs a child identity on a fresh state.
    pub(super) fn restore_child(&mut self, child: ChildState) {
        self.summary.meta.key.native_id = child.id.agent_id.clone();
        self.pinned = Some(child.id.agent_id.clone());
        if let SourceKey::File(path) = &self.summary.meta.key.source
            && let Some(project) = child_project(path)
        {
            self.summary.meta.project = Some(safe(&project).preview);
        }
        self.summary.meta.lineage = child.lineage.clone();
        self.summary.meta.title = safe(&child.id.agent_id).preview;
        // Only the child's own records count as its usage; inherited history never
        // falls back to a sidechain tally.
        self.side_ledger = None;
        self.child = Some(child);
    }

    /// Applies path/Desktop facts to the metadata and keeps them across rewrites.
    /// Origins the previous overlay supplied are replaced, not accumulated.
    pub(super) fn set_overlay(&mut self, overlay: Overlay) {
        let previous = std::mem::take(&mut self.overlay);
        let meta = &mut self.summary.meta;
        meta.origins.retain(|origin| {
            !previous
                .origins
                .iter()
                .any(|old| old.surface == origin.surface && old.raw == origin.raw)
        });
        meta.released = overlay.released;
        meta.archived = overlay.archived;
        for origin in &overlay.origins {
            add_origin(&mut meta.origins, origin.clone());
        }
        self.overlay = overlay;
    }
}

impl Overlay {
    /// Copies this overlay's facts onto `meta`.
    pub(super) fn apply(&self, meta: &mut SessionMetadata) {
        meta.released = self.released;
        meta.archived = self.archived;
        for origin in &self.origins {
            add_origin(&mut meta.origins, origin.clone());
        }
    }
}

/// A state that knows its path layout but has read no record.
pub(super) fn bare_state(
    root: &Path,
    source: SourceFile,
    main: Option<SessionKey>,
    entries: usize,
    bytes: usize,
) -> State {
    let path = source.path.clone();
    let mut state = State::new(source, entries, bytes);
    if matches!(classify_claude(root, &path), Some(ClaudePath::Child { .. })) {
        state.bind_child(root, main);
    }
    state
}

/// Bounded discovery probe of one source: its path layout, the leading records
/// and the tail. Returns the metadata and the bytes read. The probe is atomic
/// (a constant number of bounded reads); discovery resumes between sources.
pub(super) fn probe_metadata(
    path: &Path,
    root: &Path,
    limits: RefreshBudget,
    max_record: usize,
    main: Option<SessionKey>,
) -> Result<(SessionMetadata, usize), ProviderError> {
    if limits.max_records == 0 || limits.max_bytes == 0 || limits.max_millis == 0 {
        return Err(ProviderError::Unavailable("no probe budget left".into()));
    }
    if classify_claude(root, path).is_none() {
        return Err(ProviderError::Unsupported(
            "not a Claude project transcript".into(),
        ));
    }
    let io = |e: std::io::Error| ProviderError::Io(format!("{:?}", e.kind()));
    let meta = std::fs::metadata(path).map_err(io)?;
    let source = SourceFile {
        path: path.to_path_buf(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        len: meta.len(),
    };
    let probe = probe_file(path, meta.len()).map_err(io)?;
    let mut state = bare_state(root, source, main, 16, 1 << 16);
    let prices = PriceSchedule::default();
    let mut parsed = 0usize;
    for (offset, line) in probe.head.iter().take(HEAD_RECORDS) {
        if parsed >= limits.max_records {
            break;
        }
        parsed += 1;
        if let Some(value) = probe_value(line, max_record) {
            state.ingest(*offset, &value, &prices, ReferencePolicy::Disabled);
        }
    }
    // Tail records carry no offset a consumer may use; they only refine the latest
    // activity, title, model and origin. With a small budget the newest records are
    // kept, then applied oldest-first so the last observation is the newest one.
    let room = limits.max_records.saturating_sub(parsed);
    let start = probe.tail.len().saturating_sub(room);
    for line in &probe.tail[start..] {
        if parsed >= limits.max_records {
            break;
        }
        parsed += 1;
        if let Some(value) = probe_value(line, max_record) {
            state.ingest(u64::MAX, &value, &prices, ReferencePolicy::Disabled);
        }
    }
    state.summary.meta.source_available = true;
    Ok((state.summary.meta, probe.bytes))
}

/// The metadata a discovery probe of `path` under `root` reports. `limits` caps
/// the records parsed; an exhausted budget is `Unavailable`, not an empty probe.
pub(super) fn probe_claude_metadata(
    path: &Path,
    root: &Path,
    limits: RefreshBudget,
) -> Result<SessionMetadata, ProviderError> {
    probe_metadata(path, root, limits, 1 << 20, None).map(|(meta, _)| meta)
}
