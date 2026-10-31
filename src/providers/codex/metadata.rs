//! Codex rollout metadata that is private to this adapter: the header's thread
//! relationship (immediate parent vs root thread), the supported history boundary
//! that separates inherited copies from child-owned usage, launch-surface evidence,
//! and a bounded header probe for compressed rollouts.
//!
//! Nothing here invents provenance: the scanner owns the session/generation/source
//! locator and attaches it to a normalized observation. The helpers only report
//! what the recorded header proves.

use std::io;
use std::path::Path;

use serde_json::Value;

use crate::domain::{OriginObservation, OriginSurface, RequestMetadata, TokenCategories};
use crate::ingest::source::{SourceItem, SourceReader};
use crate::providers::probe::HEAD_RECORDS;

/// Expanded bytes one compressed discovery probe may decompress before it stops
/// with the header facts it already has.
pub(crate) const ARCHIVE_PROBE_BYTES: usize = 256 * 1024;
/// Records applied from that probe.
pub(crate) const ARCHIVE_PROBE_RECORDS: usize = HEAD_RECORDS;
/// Distinct origin observations kept per session.
pub(crate) const MAX_ORIGINS: usize = 8;

/// Token counters of one Codex usage record. Only the counters Codex records are
/// read here; provenance belongs to the scanner that knows the source.
pub(crate) fn parse_usage(v: &Value) -> Option<TokenCategories> {
    let o = v.as_object()?;
    let get = |k: &str| o.get(k).and_then(Value::as_u64);
    let c = TokenCategories {
        input: get("input_tokens"),
        cached_input: get("cached_input_tokens"),
        cache_write: get("cache_write_input_tokens"),
        output: get("output_tokens"),
        reasoning: get("reasoning_output_tokens"),
        total: get("total_tokens"),
        ..TokenCategories::default()
    };
    (c.input.is_some() || c.output.is_some() || c.total.is_some()).then_some(c)
}

/// Provider-private facts of one owned usage step. The scanner constructs the
/// normalized `UsageObservation`; this carries no provenance.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProviderUsageDelta {
    /// The child-owned cumulative tokens this step proves.
    pub categories: TokenCategories,
    pub request: Option<RequestMetadata>,
}

/// One usage record's ownership outcome.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OwnedUsage {
    /// A cumulative counter relative to the inherited baseline.
    Delta(ProviderUsageDelta),
    /// A counter that is already the child's own total (a reset or no inherited copy).
    Snapshot(TokenCategories),
    /// A copied record before the supported child-owned boundary.
    Inherited,
    /// No trustworthy ownership evidence, or not a usage record.
    Unknown,
}

/// How a rollout header declares the child-owned usage boundary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CodexOwnership {
    /// First record ordinal the child owns; everything before it is inherited.
    pub first_owned_ordinal: Option<u64>,
    /// Last inherited cumulative counter, used to remove the copied baseline.
    pub inherited_baseline: Option<TokenCategories>,
    /// The boundary is supported by the recorded source and format version.
    pub complete: bool,
}

impl CodexOwnership {
    pub(crate) fn new(first_owned_ordinal: Option<u64>, complete: bool) -> Self {
        Self {
            first_owned_ordinal,
            inherited_baseline: None,
            complete: complete && first_owned_ordinal.is_some(),
        }
    }
}

/// Applies one usage record to the ownership state. The caller has already decided
/// that the record belongs to this rollout (a record naming another thread is not a
/// candidate). `usage` is the raw `token_usage_record`/`token_count` body.
pub(crate) fn apply_owned_usage(
    ownership: &mut CodexOwnership,
    ordinal: u64,
    usage: &Value,
) -> OwnedUsage {
    let Some(current) = usage_categories(usage) else {
        return OwnedUsage::Unknown;
    };
    if !ownership.complete {
        return OwnedUsage::Unknown;
    }
    let Some(first) = ownership.first_owned_ordinal else {
        return OwnedUsage::Unknown;
    };
    if ordinal < first {
        // The last inherited snapshot is the baseline a cumulative child counter
        // starts from.
        ownership.inherited_baseline = Some(current);
        return OwnedUsage::Inherited;
    }
    match ownership.inherited_baseline.as_ref() {
        Some(baseline) => match subtract(&current, baseline) {
            Some(delta) => OwnedUsage::Delta(ProviderUsageDelta {
                categories: delta,
                request: None,
            }),
            None => {
                // The child counter went backwards: it is child-local from here on,
                // so nothing more may be subtracted.
                ownership.inherited_baseline = Some(TokenCategories::default());
                OwnedUsage::Snapshot(current)
            }
        },
        // No inherited counter was recorded; a thread snapshot is already its own.
        None => OwnedUsage::Snapshot(current),
    }
}

/// Counters of a usage body: the thread cumulative first, then the flat forms the
/// adapter accepts, then a single request's usage.
fn usage_categories(usage: &Value) -> Option<TokenCategories> {
    parse_usage(usage)
        .or_else(|| parse_usage(value(usage, "thread_token_usage")))
        .or_else(|| parse_usage(value(usage, "turn_token_usage")))
        .or_else(|| parse_usage(value(usage, "usage")))
        .or_else(|| parse_usage(value(value(usage, "info"), "total_token_usage")))
}

fn value<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&Value::Null)
}

/// `current - baseline` per leg, or `None` when any counter decreased (a reset).
fn subtract(current: &TokenCategories, baseline: &TokenCategories) -> Option<TokenCategories> {
    let leg = |a: Option<u64>, b: Option<u64>| -> Option<Option<u64>> {
        match (a, b) {
            (Some(a), Some(b)) => a.checked_sub(b).map(Some),
            (Some(a), None) => Some(Some(a)),
            (None, Some(_)) => Some(None),
            (None, None) => Some(None),
        }
    };
    Some(TokenCategories {
        input: leg(current.input, baseline.input)?,
        cached_input: leg(current.cached_input, baseline.cached_input)?,
        cache_write: leg(current.cache_write, baseline.cache_write)?,
        cache_write_5m: leg(current.cache_write_5m, baseline.cache_write_5m)?,
        cache_write_1h: leg(current.cache_write_1h, baseline.cache_write_1h)?,
        cache_read: leg(current.cache_read, baseline.cache_read)?,
        output: leg(current.output, baseline.output)?,
        reasoning: leg(current.reasoning, baseline.reasoning)?,
        total: leg(current.total, baseline.total)?,
        orchestration_input: leg(current.orchestration_input, baseline.orchestration_input)?,
        orchestration_output: leg(current.orchestration_output, baseline.orchestration_output)?,
        orchestration_cache_read: leg(
            current.orchestration_cache_read,
            baseline.orchestration_cache_read,
        )?,
    })
}

/// Header thread evidence. `parent_thread` is the immediate parent only when the
/// header's spawn source declares it; `root_thread` is the root session the header
/// names at every depth and is never promoted to a parent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CodexParentEvidence {
    /// Immediate parent thread (`thread_spawn.parent_thread_id`).
    pub parent_thread: Option<String>,
    /// Root thread (`session_id`) when it differs from this rollout's own ID.
    pub root_thread: Option<String>,
    /// Raw `thread_source` (`user`, `subagent`, `guardian_review`, ...).
    pub thread_source: Option<String>,
    /// Recorded agent role (`agent_role` or `source.subagent.other`).
    pub agent_role: Option<String>,
    pub agent_nickname: Option<String>,
    pub spawn_depth: Option<u32>,
    /// `forked_from_id`, a fork relation — not an immediate-parent declaration.
    pub forked_from: Option<String>,
    /// `subagent_history_start_ordinal`.
    pub history_boundary: Option<u64>,
    /// The boundary is supported by this rollout's recorded history mode/version.
    pub boundary_supported: bool,
    /// `history_mode` when recorded.
    pub history_mode: Option<String>,
    /// A spawned child or guardian review: its usage can include copied history.
    pub is_child: bool,
    /// A spawned child thread (not a guardian review).
    pub is_spawned: bool,
}

/// Reads the supported parent/source/agent-role fields of one `session_meta` payload.
pub(crate) fn codex_parent(meta: &Value) -> CodexParentEvidence {
    let source = value(meta, "source");
    let subagent = value(source, "subagent");
    let spawn = value(subagent, "thread_spawn");
    let thread_source = text(meta, "thread_source");
    let parent_thread = text(spawn, "parent_thread_id").or_else(|| text(meta, "parent_thread_id"));
    let own_id = text(meta, "id").or_else(|| text(meta, "session_id"));
    let root_thread = text(meta, "session_id").filter(|id| Some(id.as_str()) != own_id.as_deref());
    let agent_role = text(spawn, "agent_role").or_else(|| {
        subagent
            .get("other")
            .and_then(Value::as_str)
            .map(canonical_label)
    });
    let history_mode = text(meta, "history_mode");
    let version = text(meta, "cli_version");
    let boundary = meta
        .get("subagent_history_start_ordinal")
        .and_then(Value::as_u64);
    let declares_spawn = !spawn.is_null();
    let guardian = thread_source.as_deref() == Some("guardian_review")
        || subagent.get("other").and_then(Value::as_str) == Some("guardian");
    let is_spawned = thread_source.as_deref() == Some("subagent");
    let is_child = is_spawned || declares_spawn || guardian;
    let boundary_supported = boundary.is_some()
        && is_child
        && (history_mode.as_deref() == Some("paginated")
            || version.as_deref().is_some_and(at_least_paginated));
    CodexParentEvidence {
        // A root-thread ID (`session_id`) is never treated as the immediate parent;
        // a guardian review records a parent but is not a spawned child.
        parent_thread: parent_thread.filter(|_| !guardian),
        root_thread,
        thread_source,
        agent_role,
        agent_nickname: text(meta, "agent_nickname").or_else(|| text(spawn, "agent_nickname")),
        spawn_depth: spawn
            .get("depth")
            .and_then(Value::as_u64)
            .and_then(|d| u32::try_from(d).ok()),
        forked_from: text(meta, "forked_from_id"),
        history_boundary: boundary,
        boundary_supported,
        history_mode,
        is_child,
        is_spawned,
    }
}

/// The earliest Codex version whose paginated recorder writes
/// `subagent_history_start_ordinal`.
fn at_least_paginated(version: &str) -> bool {
    let mut parts = version
        .trim()
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .split('.');
    let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor: u32 = parts
        .next()
        .and_then(|p| p.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    (major, minor) >= (0, 152)
}

fn canonical_label(raw: &str) -> String {
    raw.trim()
        .chars()
        .take(64)
        .collect::<String>()
        .to_ascii_lowercase()
}

fn text(v: &Value, key: &str) -> Option<String> {
    let raw = v.get(key)?.as_str()?;
    let clean = crate::ingest::text::sanitize(raw);
    let clean: String = clean.chars().take(256).collect();
    (!clean.trim().is_empty()).then_some(clean)
}

/// The launch surface recorded by `source`/`originator`. `vscode` alone is not
/// enough: the bundled CLI also records it, so only an originator that distinguishes
/// the Desktop app from the IDE extension decides between them. Everything else is
/// `Unknown` with its raw text kept beside it.
pub(crate) fn classify_codex_origin(source: &Value, originator: Option<&str>) -> OriginSurface {
    let source = source
        .as_str()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty());
    let originator = originator.map(|s| s.trim().to_ascii_lowercase());
    let ide = originator.as_deref().is_some_and(is_ide_originator);
    let desktop = originator.as_deref().is_some_and(is_desktop_originator);
    match source.as_deref() {
        // The interactive CLI and `codex exec` are terminal surfaces, even when the
        // bundled build records the Desktop originator.
        Some("cli" | "exec") => OriginSurface::Cli,
        Some("vscode") => {
            if ide {
                OriginSurface::Ide
            } else if desktop {
                OriginSurface::DesktopCode
            } else {
                OriginSurface::Unknown
            }
        }
        // An unconfirmed or auxiliary source string is evidence without a surface.
        Some(_) => OriginSurface::Unknown,
        None if ide => OriginSurface::Ide,
        // A Desktop originator alone is ambiguous: the bundled CLI writes it too.
        None => OriginSurface::Unknown,
    }
}

fn is_ide_originator(originator: &str) -> bool {
    matches!(
        originator,
        "codex_vscode" | "codex-vscode" | "codex_vscode_extension"
    ) || originator.starts_with("codex_vscode")
}

fn is_desktop_originator(originator: &str) -> bool {
    matches!(
        originator,
        "codex desktop" | "codex_desktop" | "codex-desktop" | "chatgpt desktop"
    )
}

/// Origin evidence of one `session_meta` payload, or `None` when it records neither
/// field. The raw text is kept with the surface for inspection.
pub(crate) fn codex_origin(meta: &Value, at: Option<i64>) -> Option<OriginObservation> {
    let originator = text(meta, "originator");
    let source = value(meta, "source");
    if originator.is_none() && source.is_null() {
        return None;
    }
    let surface = classify_codex_origin(source, originator.as_deref());
    let raw = originator.or_else(|| {
        source
            .as_str()
            .map(str::to_owned)
            .or_else(|| (!source.is_null()).then(|| "subagent".to_owned()))
    });
    Some(OriginObservation::new(surface, raw.as_deref(), at))
}

/// Adds `origin` unless the same evidence is already recorded or the bound is hit.
pub(crate) fn add_origin(origins: &mut Vec<OriginObservation>, origin: OriginObservation) {
    if origins.len() < MAX_ORIGINS
        && !origins
            .iter()
            .any(|o| o.surface == origin.surface && o.raw == origin.raw)
    {
        origins.push(origin);
    }
}

/// A bounded header probe of one compressed rollout. `records` carry their expanded
/// ordinals; a damaged stream stops after the readable prefix.
pub(crate) struct ArchiveProbe {
    pub records: Vec<(u64, Value)>,
    pub expanded_bytes: usize,
    pub damaged: bool,
}

pub(crate) fn probe_archive_head(path: &Path, max_record_bytes: usize) -> io::Result<ArchiveProbe> {
    let mut reader = SourceReader::open(path, max_record_bytes).map_err(source_error)?;
    let mut records = Vec::new();
    let mut expanded_bytes = 0usize;
    let mut damaged = false;
    let budget = crate::domain::RefreshBudget {
        max_records: ARCHIVE_PROBE_RECORDS,
        max_bytes: ARCHIVE_PROBE_BYTES,
        max_millis: 50,
    };
    for _ in 0..8 {
        let batch = reader.step(budget).map_err(source_error)?;
        expanded_bytes += batch.expanded_bytes;
        for record in batch.records {
            match record.item {
                SourceItem::Json(value) => records.push((record.ordinal, value)),
                SourceItem::Damaged(_) => damaged = true,
                _ => {}
            }
        }
        if batch.done || batch.invalidated || records.len() >= ARCHIVE_PROBE_RECORDS {
            break;
        }
    }
    Ok(ArchiveProbe {
        records,
        expanded_bytes,
        damaged,
    })
}

fn source_error(error: crate::ingest::source::SourceError) -> io::Error {
    match error {
        crate::ingest::source::SourceError::Io(error) => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(input: u64, output: u64, total: u64) -> Value {
        json!({
            "input_tokens": input,
            "output_tokens": output,
            "total_tokens": total,
        })
    }

    #[test]
    fn a_replayed_child_counter_is_its_owned_delta() {
        let mut ownership = CodexOwnership::new(Some(2), true);
        assert_eq!(
            apply_owned_usage(&mut ownership, 0, &usage(600, 400, 1000)),
            OwnedUsage::Inherited
        );
        assert_eq!(
            apply_owned_usage(&mut ownership, 1, &usage(600, 400, 1000)),
            OwnedUsage::Inherited
        );
        let OwnedUsage::Delta(delta) = apply_owned_usage(&mut ownership, 2, &usage(700, 450, 1150))
        else {
            panic!("owned delta");
        };
        assert_eq!(delta.categories.total, Some(150));
        assert_eq!(delta.categories.input, Some(100));
        assert_eq!(delta.categories.output, Some(50));
    }

    #[test]
    fn a_child_local_zero_counter_is_not_subtracted_twice() {
        let mut ownership = CodexOwnership::new(Some(2), true);
        assert_eq!(
            apply_owned_usage(&mut ownership, 1, &usage(600, 400, 1000)),
            OwnedUsage::Inherited
        );
        assert_eq!(
            apply_owned_usage(&mut ownership, 2, &usage(0, 0, 0)),
            OwnedUsage::Snapshot(TokenCategories {
                input: Some(0),
                output: Some(0),
                total: Some(0),
                ..TokenCategories::default()
            })
        );
        let OwnedUsage::Delta(delta) = apply_owned_usage(&mut ownership, 3, &usage(100, 50, 150))
        else {
            panic!("owned delta");
        };
        assert_eq!(delta.categories.total, Some(150));
    }

    #[test]
    fn no_supported_boundary_is_never_owned() {
        let mut ownership = CodexOwnership::default();
        assert_eq!(
            apply_owned_usage(&mut ownership, 3, &usage(700, 450, 1150)),
            OwnedUsage::Unknown
        );
        let mut unsupported = CodexOwnership::new(Some(4), false);
        assert_eq!(
            apply_owned_usage(&mut unsupported, 4, &usage(700, 450, 1150)),
            OwnedUsage::Unknown
        );
    }

    #[test]
    fn parent_evidence_reads_spawn_not_the_root_thread() {
        let meta = json!({
            "id": "child",
            "session_id": "root",
            "parent_thread_id": "middle",
            "thread_source": "subagent",
            "cli_version": "0.159.2",
            "history_mode": "paginated",
            "subagent_history_start_ordinal": 7,
            "source": {"subagent": {"thread_spawn": {
                "parent_thread_id": "middle",
                "depth": 2,
                "agent_role": "explorer",
                "agent_nickname": "kid"
            }}}
        });
        let evidence = codex_parent(&meta);
        assert_eq!(evidence.parent_thread.as_deref(), Some("middle"));
        assert_eq!(evidence.root_thread.as_deref(), Some("root"));
        assert_eq!(evidence.agent_role.as_deref(), Some("explorer"));
        assert_eq!(evidence.spawn_depth, Some(2));
        assert_eq!(evidence.history_boundary, Some(7));
        assert!(evidence.boundary_supported);

        let root_only = json!({"id": "child", "session_id": "root"});
        let evidence = codex_parent(&root_only);
        assert_eq!(evidence.parent_thread, None);
        assert_eq!(evidence.root_thread.as_deref(), Some("root"));
    }

    #[test]
    fn versions_before_paginated_history_do_not_support_a_boundary() {
        let meta = json!({
            "id": "child",
            "thread_source": "subagent",
            "cli_version": "0.140.0",
            "subagent_history_start_ordinal": 7
        });
        assert!(!codex_parent(&meta).boundary_supported);
    }

    #[test]
    fn origin_distinguishes_ide_desktop_and_the_ambiguous_bundled_terminal() {
        assert_eq!(
            classify_codex_origin(&json!("cli"), Some("Codex Desktop")),
            OriginSurface::Cli,
            "a bundled terminal session is not the Desktop app"
        );
        assert_eq!(
            classify_codex_origin(&json!("vscode"), Some("codex_vscode")),
            OriginSurface::Ide
        );
        assert_eq!(
            classify_codex_origin(&json!("vscode"), Some("Codex Desktop")),
            OriginSurface::DesktopCode
        );
        assert_eq!(
            classify_codex_origin(&json!("vscode"), None),
            OriginSurface::Unknown,
            "vscode alone is ambiguous"
        );
        assert_eq!(
            classify_codex_origin(&json!(null), Some("Codex Desktop")),
            OriginSurface::Unknown
        );
        assert_eq!(
            classify_codex_origin(&json!("mystery"), None),
            OriginSurface::Unknown
        );
    }
}
