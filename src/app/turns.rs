//! Conversation turns over loaded events (readability redesign).
//!
//! Pure: [`turn_rows`] indexes the event slice and never clones an event. One
//! [`TurnRow`] points at a contiguous `first..=last` run of events, so the
//! inspector can move selection by row while still expanding the real event.
//! Auxiliary model calls and allowlisted system notices each fold into one
//! quiet row per consecutive run, but a row that carries failure evidence is
//! always shown on its own.

use std::fmt;

use serde_json::Value;

use crate::domain::{Event, EventKind, RequestPurpose, ToolInteraction, ToolStatus};
use crate::ui::widgets::one_line;

/// System record types that are housekeeping noise, not conversation.
///
/// These are the OMP `notice` records in `src/providers/omp/records.rs`
/// (`model_usage`, `service_tier_change`, `mode_change`). `model_change`,
/// `thinking_level_change` and `session_exit` are deliberately absent: they
/// change what the session is doing and stay visible.
pub const NOISE_RECORD_TYPES: &[&str] = &["model_usage", "service_tier_change", "mode_change"];

/// Tool input fields that make a useful one-line summary, best first.
const TOOL_SUMMARY_FIELDS: [&str; 9] = [
    "command",
    "cmd",
    "description",
    "prompt",
    "path",
    "file_path",
    "pattern",
    "text",
    "task",
];

/// Words that mark an event as failure evidence. Case-insensitive substrings.
const FAILURE_WORDS: [&str; 4] = ["error", "fail", "warn", "denied"];

/// What a transcript row represents. `Debug` prints counts only: `Noise.label`
/// is provider purpose text and never belongs in a diagnostic dump.
#[derive(Clone, PartialEq, Eq)]
pub enum RowKind {
    Prompt,
    Reply,
    Tool,
    Reasoning,
    ReasoningEmpty { count: usize },
    Noise { count: usize, label: String },
    Other,
}

/// One visible transcript row over event indices `first..=last`.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnRow {
    pub kind: RowKind,
    pub first: usize,
    pub last: usize,
    /// 0 for prompts, replies and `Other` before the first prompt; 1 for agent
    /// work and `Other` inside a turn.
    pub depth: u8,
}

impl fmt::Debug for RowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prompt => f.write_str("Prompt"),
            Self::Reply => f.write_str("Reply"),
            Self::Tool => f.write_str("Tool"),
            Self::Reasoning => f.write_str("Reasoning"),
            Self::ReasoningEmpty { count } => f
                .debug_struct("ReasoningEmpty")
                .field("count", count)
                .finish(),
            // The label is provider text; only its count is printable.
            Self::Noise { count, .. } => f.debug_struct("Noise").field("count", count).finish(),
            Self::Other => f.write_str("Other"),
        }
    }
}

impl fmt::Debug for TurnRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TurnRow")
            .field("kind", &self.kind)
            .field("first", &self.first)
            .field("last", &self.last)
            .field("depth", &self.depth)
            .finish()
    }
}

/// Fold the loaded events into conversation rows. `first..=last` are indices
/// into `events`; no event is cloned.
pub fn turn_rows(events: &[Event]) -> Vec<TurnRow> {
    let mut rows = Vec::new();
    let mut noise: Option<NoiseRun> = None;
    let mut in_turn = false;
    let mut last_failed = false;

    for (index, event) in events.iter().enumerate() {
        let failed = failure_evidence(event);
        if let Some(class) = noise_class(event)
            && !failed
        {
            // Auxiliary calls and notices are agent work: always indented.
            extend_noise(&mut rows, &mut noise, class, event, index, 1);
            continue;
        }
        flush_noise(&mut rows, &mut noise);

        let kind = visible_kind(event);
        if event.kind == EventKind::UserPrompt {
            in_turn = true;
        }
        let depth = match kind {
            RowKind::Prompt | RowKind::Reply => 0,
            RowKind::Other => u8::from(in_turn),
            _ => 1,
        };
        // A failure row never merges into, or absorbs, a quiet reasoning row.
        push_row(&mut rows, kind, index, depth, !failed && !last_failed);
        last_failed = failed;
    }
    flush_noise(&mut rows, &mut noise);
    rows
}

/// One line of tool input for a tool row: the first useful field of a JSON
/// input, or the preview with JSON punctuation stripped.
pub fn tool_summary(tool: &ToolInteraction) -> String {
    let Some(input) = tool.input.as_ref() else {
        return String::new();
    };
    if let Ok(value) = serde_json::from_str::<Value>(&input.preview) {
        for key in TOOL_SUMMARY_FIELDS {
            if let Some(found) = value.get(key).and_then(Value::as_str) {
                return one_line(found).trim().to_owned();
            }
        }
    }
    let stripped = input.preview.replace(['{', '}', '"', '\''], "");
    one_line(&stripped).trim().to_owned()
}

/// What a folded run describes. A run is homogeneous: the two labels count
/// different things, so a change of kind ends the run.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NoiseClass {
    Auxiliary,
    SystemNotice,
}

struct NoiseRun {
    class: NoiseClass,
    first: usize,
    last: usize,
    count: usize,
    depth: u8,
    /// Purpose of the first auxiliary event; the label names it.
    name: Option<String>,
}

impl NoiseRun {
    fn start(class: NoiseClass, event: &Event, index: usize, depth: u8) -> Self {
        Self {
            class,
            first: index,
            last: index,
            count: 1,
            depth,
            name: auxiliary_name(event).map(str::to_owned),
        }
    }
}

fn noise_class(event: &Event) -> Option<NoiseClass> {
    // Compaction and lifecycle are never conversation noise, even if a
    // provider tagged their usage as auxiliary.
    if matches!(event.kind, EventKind::Compaction | EventKind::Lifecycle) {
        return None;
    }
    if auxiliary_name(event).is_some() {
        return Some(NoiseClass::Auxiliary);
    }
    if event.kind == EventKind::System
        && NOISE_RECORD_TYPES.contains(&event.provenance.record_type.as_str())
    {
        return Some(NoiseClass::SystemNotice);
    }
    None
}

fn auxiliary_name(event: &Event) -> Option<&str> {
    let request = event.usage.as_ref()?.request.as_ref()?;
    match &request.purpose {
        RequestPurpose::Auxiliary(name) => Some(name.as_str()),
        _ => None,
    }
}

fn extend_noise(
    rows: &mut Vec<TurnRow>,
    run: &mut Option<NoiseRun>,
    class: NoiseClass,
    event: &Event,
    index: usize,
    depth: u8,
) {
    match run {
        Some(current) if current.class == class => {
            current.last = index;
            current.count += 1;
        }
        Some(_) => {
            flush_noise(rows, run);
            *run = Some(NoiseRun::start(class, event, index, depth));
        }
        None => *run = Some(NoiseRun::start(class, event, index, depth)),
    }
}

fn flush_noise(rows: &mut Vec<TurnRow>, run: &mut Option<NoiseRun>) {
    let Some(run) = run.take() else {
        return;
    };
    let label = match run.class {
        NoiseClass::Auxiliary => {
            let name = run
                .name
                .as_deref()
                .map(one_line)
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "unspecified".to_owned());
            format!("{} auxiliary model calls ({name})", run.count)
        }
        NoiseClass::SystemNotice => format!("{} system notices", run.count),
    };
    rows.push(TurnRow {
        kind: RowKind::Noise {
            count: run.count,
            label,
        },
        first: run.first,
        last: run.last,
        depth: run.depth,
    });
}

/// The row an event would render as on its own, without noise folding.
fn visible_kind(event: &Event) -> RowKind {
    match event.kind {
        EventKind::UserPrompt => RowKind::Prompt,
        EventKind::AssistantMessage => RowKind::Reply,
        EventKind::Tool => RowKind::Tool,
        EventKind::Reasoning if has_text(event) => RowKind::Reasoning,
        EventKind::Reasoning => RowKind::ReasoningEmpty { count: 1 },
        EventKind::System | EventKind::Lifecycle | EventKind::Compaction => RowKind::Other,
    }
}

/// Push a row, collapsing consecutive text-less reasoning into one count.
/// `merge` is false around failure evidence, so a failure row stays standalone.
fn push_row(rows: &mut Vec<TurnRow>, kind: RowKind, index: usize, depth: u8, merge: bool) {
    if merge
        && matches!(&kind, RowKind::ReasoningEmpty { .. })
        && let Some(last) = rows.last_mut()
        && last.last + 1 == index
        && matches!(&last.kind, RowKind::ReasoningEmpty { .. })
    {
        last.last = index;
        if let RowKind::ReasoningEmpty { count } = &mut last.kind {
            *count += 1;
        }
        return;
    }
    rows.push(TurnRow {
        kind,
        first: index,
        last: index,
        depth,
    });
}

fn has_text(event: &Event) -> bool {
    event
        .text
        .as_ref()
        .is_some_and(|text| !text.preview.trim().is_empty())
}

/// Failure evidence makes a noise candidate visible on its own row.
fn failure_evidence(event: &Event) -> bool {
    if event.kind == EventKind::Lifecycle {
        return true;
    }
    if let Some(tool) = &event.tool
        && (tool.status == ToolStatus::Error || tool.exit_code.is_some_and(|code| code != 0))
    {
        return true;
    }
    has_failure_word(&event.provenance.record_type)
        || event
            .text
            .as_ref()
            .is_some_and(|text| has_failure_word(&text.preview))
}

fn has_failure_word(text: &str) -> bool {
    let bytes = text.as_bytes();
    FAILURE_WORDS
        .iter()
        .any(|word| contains_ignore_ascii_case(bytes, word.as_bytes()))
}

/// ASCII case-insensitive substring search without allocating a lowercase copy.
fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle))
}
