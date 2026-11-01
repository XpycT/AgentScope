//! Bounded discovery of native Pi session files.
//!
//! The known layout is `<root>/<project-bucket>/<timestamp>_<session-id>.jsonl`
//! (pi `session-format.md`). Discovery reads only a short prefix (the header line
//! plus the first records) and a 32 KiB tail of each source: no middle message
//! bodies, no extension state, no credentials or configuration files.
//!
//! Cross-source relationships (a `parentSession` path or id) are parsed here but
//! resolved by the fork logic in the tree/hydration stages, not by guessing.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::domain::{
    ActivityStamp, AggregationClass, Harness, Lineage, ModelDetail, ReleaseState, SessionKey,
    SessionMetadata, SourceKey, Timestamp, Warning, WarningKey, WarningKind,
};
use crate::ingest::text::preview;
use crate::providers::pi::record::{PiHeader, parse_header, supported_version};
use crate::providers::probe::{HEAD_RECORDS, HEADER_MAX, probe_file_lines, probe_value};

/// Largest non-header record a probe parses.
const RECORD_MAX: usize = 1 << 20;
/// Newest tail records applied to the latest-activity estimate.
const TAIL_RECORDS: usize = 64;
/// Longest title kept for display.
const TITLE_BYTES: usize = 1024;
const TITLE_COLS: usize = 256;

fn clean(text: &str, max: usize) -> Option<String> {
    (!text.is_empty() && text.len() <= max && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

fn parse_time(text: &str) -> Option<Timestamp> {
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp_millis())
}

pub(super) fn modified_ms(modified: SystemTime) -> Option<Timestamp> {
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

pub(super) fn warning(path: &Path, label: &str, kind: WarningKind, message: &str) -> Warning {
    Warning {
        key: WarningKey {
            harness: Harness::Pi,
            source: format!("{}#{label}", path.display()),
            kind,
        },
        message: message.to_owned(),
        count: 1,
    }
}

/// The ID a file name names when its header could not be read:
/// `<timestamp>_<session-id>.jsonl` → `<session-id>`, else the whole stem.
pub fn name_id(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let id = stem.split_once('_').map_or(stem.as_str(), |(_, id)| id);
    id.chars().filter(|c| !c.is_control()).take(256).collect()
}

fn whitespace_collapsed(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn display_title(raw: &str) -> String {
    preview(&whitespace_collapsed(raw), TITLE_BYTES, TITLE_COLS).preview
}

/// The text of a Pi message `content`: a plain string or the text blocks of an
/// array. Images and tool payloads never become titles.
fn content_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => clean(text, 4096),
        Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                let Some(object) = block.as_object() else {
                    continue;
                };
                if object.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                if let Some(piece) = object.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push(' ');
                    }
                    text.push_str(piece);
                    if text.len() > 4096 {
                        break;
                    }
                }
            }
            clean(&text, 4096)
        }
        _ => None,
    }
}

fn message_title(value: &Value) -> Option<String> {
    let message = value.get("message")?.as_object()?;
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return None;
    }
    content_text(message.get("content"))
}

fn session_info_name(value: &Value) -> Option<String> {
    if value.get("type").and_then(Value::as_str) != Some("session_info") {
        return None;
    }
    value
        .get("name")
        .and_then(Value::as_str)
        .and_then(|name| clean(name, 4096))
}

fn model_change_model(value: &Value) -> Option<ModelDetail> {
    if value.get("type").and_then(Value::as_str) != Some("model_change") {
        return None;
    }
    Some(ModelDetail {
        id: clean(value.get("modelId")?.as_str()?, 256)?,
        provider: value
            .get("provider")
            .and_then(Value::as_str)
            .and_then(|provider| clean(provider, 256)),
        variant: None,
        effort: None,
        configured_effort: None,
    })
}

fn entry_time(value: &Value) -> Option<Timestamp> {
    if value.get("type").and_then(Value::as_str) == Some("session") {
        return None;
    }
    parse_time(value.get("timestamp")?.as_str()?)
}

/// What one bounded probe established.
pub struct Probed {
    pub meta: SessionMetadata,
    pub header: Option<PiHeader>,
    /// Bytes read from the file.
    pub bytes: usize,
    pub warnings: Vec<Warning>,
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
    header: Option<&PiHeader>,
    title: Option<(String, bool)>,
    model: Option<ModelDetail>,
    activity: ActivityStamp,
    available: bool,
) -> SessionMetadata {
    let (title, title_derived) = match title.filter(|(title, _)| !title.is_empty()) {
        Some((title, derived)) => (title, derived),
        None => (display_title(id), false),
    };
    let cwd = header
        .and_then(|header| header.cwd.as_deref())
        .map(PathBuf::from);
    SessionMetadata {
        key: SessionKey {
            harness: Harness::Pi,
            native_id: id.to_owned(),
            source: SourceKey::File(path.to_path_buf()),
        },
        title,
        title_derived,
        project: cwd
            .as_deref()
            .and_then(Path::file_name)
            .map(|name| display_title(&name.to_string_lossy())),
        cwd,
        model,
        origins: Vec::new(),
        created_at: header.and_then(|header| header.timestamp),
        activity,
        lineage: Lineage::default(),
        members: Vec::new(),
        archived: false,
        released: ReleaseState::Unknown,
        source_available: available,
        harness_version: header
            .and_then(|header| header.version)
            .map(|version| format!("v{version}")),
    }
}

/// Metadata of a source whose bytes could not be read: the file-name ID, the
/// modification time as an approximate activity stamp, no availability.
pub fn unreadable(path: &Path, modified: SystemTime) -> SessionMetadata {
    let id = name_id(path);
    build(
        path,
        &id,
        None,
        None,
        None,
        activity_fallback(modified),
        false,
    )
}

/// Bounded discovery probe of one source: the header line, the first records and
/// the tail. Relationship fields stay empty; they are decided across sources.
pub fn probe_source(path: &Path, len: u64, modified: SystemTime) -> Result<Probed, io::Error> {
    let probe = probe_file_lines(path, len, 1)?;
    let mut warnings = Vec::new();
    let header = probe
        .head
        .first()
        .and_then(|(_, line)| probe_value(line, HEADER_MAX))
        .and_then(|value| parse_header(&value));
    let skip = usize::from(header.is_some());
    let head_values: Vec<Value> = probe
        .head
        .iter()
        .skip(skip)
        .take(HEAD_RECORDS)
        .filter_map(|(_, line)| probe_value(line, RECORD_MAX))
        .collect();
    let tail_skip = probe.tail.len().saturating_sub(TAIL_RECORDS);
    let tail_values: Vec<Value> = probe
        .tail
        .iter()
        .skip(tail_skip)
        .filter_map(|line| probe_value(line, RECORD_MAX))
        .collect();
    let mut latest: Option<Timestamp> = None;
    for value in head_values.iter().chain(&tail_values) {
        latest = latest.max(entry_time(value));
    }
    let id = match &header {
        Some(header) => header.id.clone(),
        None => {
            warnings.push(warning(
                path,
                "header",
                WarningKind::Parse,
                "Approximate session ID: no Pi session header in the discovery probe, \
                 so the file name identifies this session",
            ));
            name_id(path)
        }
    };
    if let Some(header) = &header
        && !supported_version(header.version)
    {
        warnings.push(warning(
            path,
            "version",
            WarningKind::Unsupported,
            "unsupported Pi session version: only the v2/v3 tree format is interpreted",
        ));
    }
    // Head and tail do not overlap, so the last recorded name across both is the
    // newest one. A short file may have no tail at all.
    let title = head_values
        .iter()
        .chain(&tail_values)
        .filter_map(session_info_name)
        .next_back()
        .map(|name| (display_title(&name), false))
        .or_else(|| {
            head_values
                .iter()
                .find_map(message_title)
                .map(|text| (display_title(&text), true))
        });
    let model = tail_values
        .iter()
        .filter_map(model_change_model)
        .next_back()
        .or_else(|| {
            head_values
                .iter()
                .filter_map(model_change_model)
                .next_back()
        });
    let activity = match latest {
        Some(at) => ActivityStamp {
            at: Some(at),
            class: AggregationClass::Reported,
        },
        None => activity_fallback(modified),
    };
    let meta = build(path, &id, header.as_ref(), title, model, activity, true);
    Ok(Probed {
        meta,
        header,
        bytes: probe.bytes,
        warnings,
    })
}
