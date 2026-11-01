//! Known OMP session-file layout: the fixed-width title slot, the session header
//! and the bounded discovery probe.
//!
//! A session file starts with a physical title slot of exactly 256 bytes
//! (newline included), then the `session` header line, then entries. Only the
//! first two lines can be a header, so message text, tool output or a title that
//! merely looks like a header never becomes one. Nothing between the 4 KiB
//! prefix and the 32 KiB tail of a source is read.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use serde_json::Value;

use crate::domain::{
    ActivityStamp, AggregationClass, Harness, Lineage, ReleaseState, SessionKey, SessionMetadata,
    SourceKey, Timestamp, Warning, WarningKey, WarningKind,
};
use crate::providers::probe::{HEAD_RECORDS, HEADER_MAX, probe_file_lines, probe_value};

/// Physical size of the title slot, newline included.
pub const TITLE_SLOT_BYTES: usize = 256;
/// Longest id accepted from a header.
const ID_MAX: usize = 256;
/// Longest working directory or parent reference accepted from a header.
const REF_MAX: usize = 4096;
/// Largest non-header record a probe parses.
const RECORD_MAX: usize = 1 << 20;
/// Newest tail records applied to the latest-activity estimate.
const TAIL_RECORDS: usize = 64;

/// Identity and relationship evidence of a `session` header. `Debug` prints
/// presence only: every field is untrusted text.
#[derive(Clone, PartialEq, Eq)]
pub struct OmpHeader {
    pub id: String,
    pub created_at: Option<Timestamp>,
    pub cwd: Option<String>,
    /// A session id (forks, continuations) or a session file path (some
    /// branches) of the session this one was created from.
    pub parent_session: Option<String>,
    /// Set to the `parentSession` id when this file is the same conversation
    /// continued in a new file. Forks never set it.
    pub continuation_of: Option<String>,
}

impl std::fmt::Debug for OmpHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OmpHeader")
            .field("created_at", &self.created_at)
            .field("cwd", &self.cwd.is_some())
            .field("parent_session", &self.parent_session.is_some())
            .field("continuation_of", &self.continuation_of.is_some())
            .finish_non_exhaustive()
    }
}

fn clean(text: &str, max: usize) -> Option<String> {
    (!text.is_empty() && text.len() <= max && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

fn string_field(object: &serde_json::Map<String, Value>, key: &str, max: usize) -> Option<String> {
    clean(object.get(key)?.as_str()?, max)
}

fn parse_time(text: &str) -> Option<Timestamp> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// The header of a session file. Only a top-level `type: "session"` object with a
/// usable `id` is a header; every other field is optional evidence.
pub fn parse_header(value: &Value) -> Option<OmpHeader> {
    let object = value.as_object()?;
    if object.get("type")?.as_str()? != "session" {
        return None;
    }
    Some(OmpHeader {
        id: string_field(object, "id", ID_MAX)?,
        created_at: object
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_time),
        cwd: string_field(object, "cwd", REF_MAX),
        parent_session: string_field(object, "parentSession", REF_MAX),
        continuation_of: string_field(object, "continuationOf", ID_MAX),
    })
}

/// A recognised title slot.
#[derive(Clone, PartialEq, Eq)]
pub struct TitleSlot {
    pub title: String,
    /// `Some(true)` for an `auto` title, `Some(false)` for a `user` title.
    pub derived: Option<bool>,
}

/// The title slot written at the start of a session file. The slot is exactly
/// [`TITLE_SLOT_BYTES`] bytes including its newline, counted in bytes (a title
/// of multi-byte characters is not measured in characters); any other line, a
/// slot of another version, a malformed object or an unknown title source is not
/// a slot. `line` is the physical line with or without its trailing newline.
pub fn parse_title_slot_entry(line: &[u8]) -> Option<TitleSlot> {
    let body = line.strip_suffix(b"\n").unwrap_or(line);
    if body.len() + 1 != TITLE_SLOT_BYTES {
        return None;
    }
    let value: Value = serde_json::from_slice(body).ok()?;
    let object = value.as_object()?;
    if object.get("type")?.as_str()? != "title" || object.get("v")?.as_u64()? != 1 {
        return None;
    }
    object.get("updatedAt")?.as_str()?;
    object.get("pad")?.as_str()?;
    let derived = match object.get("source") {
        None => None,
        Some(Value::String(source)) if source == "auto" => Some(true),
        Some(Value::String(source)) if source == "user" => Some(false),
        Some(_) => return None,
    };
    let title: String = object
        .get("title")?
        .as_str()?
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    Some(TitleSlot { title, derived })
}

/// The title of a physical slot line, if it is one.
pub fn parse_title_slot(line: &[u8]) -> Option<String> {
    parse_title_slot_entry(line).map(|slot| slot.title)
}

pub(super) fn modified_ms(modified: SystemTime) -> Option<Timestamp> {
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

pub(super) fn warning(path: &Path, label: &str, kind: WarningKind, message: &str) -> Warning {
    Warning {
        key: WarningKey {
            harness: Harness::Omp,
            source: format!("{}#{label}", path.display()),
            kind,
        },
        message: message.to_owned(),
        count: 1,
    }
}

/// The ID a file name names when its header could not be read: the part after the
/// first `_` of `<timestamp>_<id>`, else the whole stem.
pub fn name_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let id = stem.split_once('_').map_or(stem.as_str(), |(_, id)| id);
    id.chars()
        .filter(|c| !c.is_control())
        .take(ID_MAX)
        .collect()
}

/// What one source probe established.
pub struct Probed {
    pub meta: SessionMetadata,
    pub header: Option<OmpHeader>,
    /// Bytes read from the file.
    pub bytes: usize,
    pub warnings: Vec<Warning>,
}

fn entry_time(line: &[u8]) -> Option<Timestamp> {
    let value = probe_value(line, RECORD_MAX)?;
    let object = value.as_object()?;
    if matches!(object.get("type")?.as_str()?, "session" | "title") {
        return None;
    }
    parse_time(object.get("timestamp")?.as_str()?)
}

/// Metadata of a source whose bytes could not be read: the file name's ID, the
/// modification time as an approximate activity stamp, no availability.
pub fn unreadable(path: &Path, modified: SystemTime) -> SessionMetadata {
    let id = name_id(path);
    build(path, &id, None, None, activity_fallback(modified), false)
}

/// Whitespace-collapsed, sanitized, credential-masked and bounded display text.
fn display_title(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::ingest::text::preview(&collapsed, 1024, 256).preview
}

fn activity_fallback(modified: SystemTime) -> ActivityStamp {
    ActivityStamp {
        at: modified_ms(modified),
        class: AggregationClass::Derived,
    }
}

fn build(
    path: &Path,
    id: &str,
    header: Option<&OmpHeader>,
    slot: Option<&TitleSlot>,
    activity: ActivityStamp,
    available: bool,
) -> SessionMetadata {
    // Titles and IDs are untrusted text: sanitized, credential-masked and bounded.
    let (title, title_derived) = match slot
        .map(|s| (display_title(&s.title), s.derived))
        .filter(|(title, _)| !title.is_empty())
    {
        Some((title, derived)) => (title, derived.unwrap_or(false)),
        None => (display_title(id), false),
    };
    let cwd = header.and_then(|h| h.cwd.as_deref()).map(PathBuf::from);
    SessionMetadata {
        key: SessionKey {
            harness: Harness::Omp,
            native_id: id.to_owned(),
            source: SourceKey::File(path.to_path_buf()),
        },
        title,
        title_derived,
        project: cwd
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| display_title(&n.to_string_lossy())),
        cwd,
        model: None,
        origins: Vec::new(),
        created_at: header.and_then(|h| h.created_at),
        activity,
        lineage: Lineage::default(),
        members: Vec::new(),
        archived: false,
        released: ReleaseState::Unknown,
        source_available: available,
        harness_version: None,
    }
}

/// Bounded discovery probe of one source: the leading lines (title slot, header,
/// first records) and the tail. The relationship fields of the returned
/// metadata are empty; they are decided across sources by `resolve_omp_links`.
pub fn probe_source(path: &Path, len: u64, modified: SystemTime) -> Result<Probed, io::Error> {
    // Title slot line, then the header line: read both to their newlines.
    let probe = probe_file_lines(path, len, 2)?;
    let mut warnings = Vec::new();
    let mut header = None;
    let mut slot = None;
    let mut first_entry = 0usize;
    if let Some((_, first)) = probe.head.first() {
        if let Some(found) = probe_value(first, HEADER_MAX).and_then(|v| parse_header(&v)) {
            // A legacy file without a title slot starts with its header.
            header = Some(found);
            first_entry = 1;
        } else {
            slot = parse_title_slot_entry(first);
            if let Some((_, second)) = probe.head.get(1) {
                header = probe_value(second, HEADER_MAX).and_then(|v| parse_header(&v));
            }
            first_entry = if header.is_some() { 2 } else { 0 };
        }
    }
    let mut latest: Option<Timestamp> = None;
    for (_, line) in probe.head.iter().skip(first_entry).take(HEAD_RECORDS) {
        latest = latest.max(entry_time(line));
    }
    let skip = probe.tail.len().saturating_sub(TAIL_RECORDS);
    for line in probe.tail.iter().skip(skip) {
        latest = latest.max(entry_time(line));
    }
    let id = match &header {
        Some(h) => h.id.clone(),
        None => {
            warnings.push(warning(
                path,
                "header",
                WarningKind::Parse,
                "Approximate session ID: no session header within the discovery probe, \
                 so the file name identifies this session",
            ));
            name_id(path)
        }
    };
    let activity = match latest {
        Some(at) => ActivityStamp {
            at: Some(at),
            class: AggregationClass::Reported,
        },
        None => activity_fallback(modified),
    };
    let meta = build(path, &id, header.as_ref(), slot.as_ref(), activity, true);
    Ok(Probed {
        meta,
        header,
        bytes: probe.bytes,
        warnings,
    })
}
