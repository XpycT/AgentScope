//! Pure mapping of one OMP record to displayable event slots, pinned to
//! can1357/oh-my-pi `6a86df8539fccf3ad7818081266e554022fc21bc`
//! (`session-entries.ts`, pi-ai message types).
//!
//! A record maps to zero or more *slots*: an assistant message yields one slot per
//! content block (text, reasoning, tool call), so a long turn is several events that
//! share one source line. Free-form fields are bounded when copied and sanitized
//! only when an event is built. Nothing here reads a file; the session-init record
//! (which carries the system prompt and task text) is mapped to a fixed label, and
//! tool `details`, extension (`custom`) records and labels are never displayed.

use serde_json::Value;

use crate::domain::{Actor, EventKind, ToolStatus};

/// Raw bytes copied per free-form field (previews are cut again for display).
const FIELD_MAX: usize = 8192;
/// Content blocks of one record that become events.
const BLOCKS_MAX: usize = 64;
/// Longest tool name or call ID shown.
pub(super) const LABEL_MAX: usize = 128;

/// A bounded copy of untrusted text and its full length.
#[derive(Clone)]
pub(super) struct Text {
    pub raw: String,
    pub len: usize,
    /// Part of the text (or of its content blocks) is not carried.
    pub cut: bool,
}

#[derive(Clone)]
pub(super) struct ToolSlot {
    pub name: String,
    pub call_id: Option<String>,
    pub input: Option<Text>,
    pub output: Option<Text>,
    pub status: ToolStatus,
    pub exit_code: Option<i32>,
}

/// One displayable event of a record.
#[derive(Clone)]
pub(super) struct Slot {
    pub kind: EventKind,
    pub actor: Actor,
    pub record_type: &'static str,
    pub text: Option<Text>,
    pub tool: Option<ToolSlot>,
}

pub(super) enum Mapped {
    /// The record's events, in block order (never empty).
    Events(Vec<Slot>),
    /// A record known to exist but not shown; the label says why.
    Hidden(&'static str),
    /// Structural noise with no diagnostic (the physical title slot).
    Skip,
}

fn bounded(raw: &str) -> Text {
    let mut end = raw.len().min(FIELD_MAX);
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    Text {
        raw: raw[..end].to_owned(),
        len: raw.len(),
        cut: end < raw.len(),
    }
}

fn string(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|s| !s.is_empty())
}

/// The text of a message `content`: a plain string, or the text blocks of an array
/// (an image is a placeholder, never its data).
fn content_text(content: Option<&Value>) -> Option<Text> {
    match content? {
        Value::String(text) if !text.is_empty() => Some(bounded(text)),
        Value::Array(blocks) => {
            let mut raw = String::new();
            let mut len = 0usize;
            for block in blocks.iter().take(BLOCKS_MAX) {
                let piece = match block.get("type").and_then(Value::as_str) {
                    Some("text") => block.get("text").and_then(Value::as_str).unwrap_or(""),
                    Some("image") => "[image]",
                    _ => "",
                };
                if piece.is_empty() {
                    continue;
                }
                if len > 0 {
                    len += 1;
                    if raw.len() < FIELD_MAX {
                        raw.push('\n');
                    }
                }
                len += piece.len();
                if raw.len() < FIELD_MAX {
                    raw.push_str(&bounded(piece).raw);
                }
            }
            (len > 0).then(|| {
                let mut end = raw.len().min(FIELD_MAX);
                while !raw.is_char_boundary(end) {
                    end -= 1;
                }
                raw.truncate(end);
                let cut = blocks.len() > BLOCKS_MAX || raw.len() < len;
                Text { raw, len, cut }
            })
        }
        _ => None,
    }
}

fn label(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(LABEL_MAX)
        .collect()
}

fn slot(kind: EventKind, actor: Actor, record_type: &'static str, text: Option<Text>) -> Slot {
    Slot {
        kind,
        actor,
        record_type,
        text,
        tool: None,
    }
}

fn fixed(kind: EventKind, actor: Actor, record_type: &'static str, text: &str) -> Mapped {
    Mapped::Events(vec![slot(kind, actor, record_type, Some(bounded(text)))])
}

/// Short single-field notices ("model changed to X") share this shape.
fn notice(record_type: &'static str, prefix: &str, detail: Option<String>) -> Mapped {
    let text = match detail {
        Some(detail) => format!("{prefix}: {detail}"),
        None => prefix.to_owned(),
    };
    fixed(EventKind::System, Actor::System, record_type, &text)
}

fn tool_input(arguments: Option<&Value>) -> Option<Text> {
    let arguments = arguments?;
    match arguments {
        Value::Null => None,
        Value::String(text) => Some(bounded(text)),
        other => serde_json::to_string(other).ok().map(|text| bounded(&text)),
    }
}

fn assistant(message: &Value, detail: bool) -> Mapped {
    let mut slots = Vec::new();
    if let Some(blocks) = message.get("content").and_then(Value::as_array) {
        for block in blocks.iter().take(BLOCKS_MAX) {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => slots.push(slot(
                    EventKind::AssistantMessage,
                    Actor::Assistant,
                    "message/assistant",
                    detail
                        .then(|| block.get("text").and_then(Value::as_str).map(bounded))
                        .flatten(),
                )),
                Some("thinking") => slots.push(slot(
                    EventKind::Reasoning,
                    Actor::Assistant,
                    "message/assistant#thinking",
                    detail
                        .then(|| block.get("thinking").and_then(Value::as_str).map(bounded))
                        .flatten(),
                )),
                Some("redactedThinking") => slots.push(slot(
                    EventKind::Reasoning,
                    Actor::Assistant,
                    "message/assistant#thinking",
                    detail.then(|| bounded("[redacted reasoning]")),
                )),
                Some("toolCall") => slots.push(Slot {
                    kind: EventKind::Tool,
                    actor: Actor::Tool,
                    record_type: "message/assistant#toolCall",
                    text: None,
                    tool: Some(ToolSlot {
                        name: if detail {
                            string(block.get("name")).map(label).unwrap_or_default()
                        } else {
                            String::new()
                        },
                        call_id: detail.then(|| string(block.get("id")).map(label)).flatten(),
                        input: detail.then(|| tool_input(block.get("arguments"))).flatten(),
                        output: None,
                        status: ToolStatus::Unknown,
                        exit_code: None,
                    }),
                }),
                _ => {}
            }
        }
    }
    if let Some(blocks) = message.get("content").and_then(Value::as_array)
        && blocks.len() > BLOCKS_MAX
    {
        // Later blocks are not events here: say so, and point at the source record.
        slots.push(slot(
            EventKind::System,
            Actor::System,
            "message/omitted",
            detail.then(|| Text {
                cut: true,
                ..bounded(&format!(
                    "{} further content blocks are not shown; open the source record",
                    blocks.len() - BLOCKS_MAX
                ))
            }),
        ));
    }
    if slots.is_empty() {
        // An aborted or empty turn still carries its usage record.
        slots.push(slot(
            EventKind::AssistantMessage,
            Actor::Assistant,
            "message/assistant",
            None,
        ));
    }
    Mapped::Events(slots)
}

fn message(entry: &Value, detail: bool) -> Mapped {
    let Some(message) = entry.get("message") else {
        return Mapped::Hidden("message without a body");
    };
    let text = |content: Option<&Value>| detail.then(|| content_text(content)).flatten();
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    match role {
        "user" => Mapped::Events(vec![slot(
            EventKind::UserPrompt,
            Actor::User,
            "message/user",
            text(message.get("content")),
        )]),
        "developer" => Mapped::Events(vec![slot(
            EventKind::System,
            Actor::System,
            "message/developer",
            text(message.get("content")),
        )]),
        "assistant" => assistant(message, detail),
        "toolResult" => {
            let status = match message.get("isError").and_then(Value::as_bool) {
                Some(true) => ToolStatus::Error,
                Some(false) => ToolStatus::Completed,
                None => ToolStatus::Unknown,
            };
            Mapped::Events(vec![Slot {
                kind: EventKind::Tool,
                actor: Actor::Tool,
                record_type: "message/toolResult",
                text: None,
                tool: Some(ToolSlot {
                    name: if detail {
                        string(message.get("toolName"))
                            .map(label)
                            .unwrap_or_default()
                    } else {
                        String::new()
                    },
                    call_id: detail
                        .then(|| string(message.get("toolCallId")).map(label))
                        .flatten(),
                    input: None,
                    output: text(message.get("content")),
                    status,
                    exit_code: None,
                }),
            }])
        }
        "bashExecution" | "pythonExecution" => {
            let (name, input) = if role == "bashExecution" {
                ("bash", "command")
            } else {
                ("python", "code")
            };
            let exit_code = message
                .get("exitCode")
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok());
            let status = match (exit_code, message.get("cancelled").and_then(Value::as_bool)) {
                (_, Some(true)) | (Some(1..) | Some(..=-1), _) => ToolStatus::Error,
                (Some(0), _) => ToolStatus::Completed,
                (None, _) => ToolStatus::Unknown,
            };
            Mapped::Events(vec![Slot {
                kind: EventKind::Tool,
                actor: Actor::Tool,
                record_type: "message/execution",
                text: None,
                tool: Some(ToolSlot {
                    name: name.to_owned(),
                    call_id: None,
                    input: detail
                        .then(|| string(message.get(input)).map(bounded))
                        .flatten(),
                    output: detail
                        .then(|| string(message.get("output")).map(bounded))
                        .flatten(),
                    status,
                    exit_code,
                }),
            }])
        }
        // Custom, hook and summary roles: shown as system context when they carry text.
        _ => Mapped::Events(vec![slot(
            EventKind::System,
            Actor::System,
            "message/other",
            detail
                .then(|| {
                    content_text(message.get("content"))
                        .or_else(|| string(message.get("summary")).map(bounded))
                })
                .flatten(),
        )]),
    }
}

/// Maps one parsed record. `detail` copies the bounded text; without it only the
/// number and kinds of events are computed (indexing, counting).
pub(super) fn map(entry: &Value, detail: bool) -> Mapped {
    let Some(kind) = entry.get("type").and_then(Value::as_str) else {
        return Mapped::Hidden("record without a type");
    };
    match kind {
        "title" => Mapped::Skip,
        "session" => fixed(
            EventKind::Lifecycle,
            Actor::System,
            "session",
            "session started",
        ),
        "message" => message(entry, detail),
        "custom_message" => Mapped::Events(vec![slot(
            EventKind::System,
            Actor::System,
            "custom_message",
            detail.then(|| content_text(entry.get("content"))).flatten(),
        )]),
        "compaction" => Mapped::Events(vec![slot(
            EventKind::Compaction,
            Actor::System,
            "compaction",
            detail
                .then(|| string(entry.get("summary")).map(bounded))
                .flatten(),
        )]),
        "branch_summary" => Mapped::Events(vec![slot(
            EventKind::System,
            Actor::System,
            "branch_summary",
            detail
                .then(|| string(entry.get("summary")).map(bounded))
                .flatten(),
        )]),
        "model_change" => notice(
            "model_change",
            "model changed",
            string(entry.get("model")).map(label),
        ),
        "thinking_level_change" => {
            let level = string(entry.get("thinkingLevel")).map(label);
            let configured = string(entry.get("configured")).map(label);
            notice(
                "thinking_level_change",
                "thinking level",
                match (level, configured) {
                    (Some(level), Some(configured)) => {
                        Some(format!("{level} (configured {configured})"))
                    }
                    (level, configured) => level.or(configured),
                },
            )
        }
        "service_tier_change" => {
            let tiers = entry
                .get("serviceTier")
                .and_then(Value::as_object)
                .map(|tiers| {
                    tiers
                        .iter()
                        .take(4)
                        .filter_map(|(provider, tier)| {
                            Some(format!("{}={}", label(provider), label(tier.as_str()?)))
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .filter(|s| !s.is_empty());
            notice("service_tier_change", "service tier", tiers)
        }
        "mode_change" => notice("mode_change", "mode", string(entry.get("mode")).map(label)),
        "model_usage" => {
            let purpose = string(entry.get("purpose")).map(label);
            let model = string(entry.get("model")).map(label);
            let detail_text = match (purpose, model) {
                (Some(purpose), Some(model)) => Some(format!("{purpose} ({model})")),
                (purpose, model) => purpose.or(model),
            };
            notice("model_usage", "auxiliary model call", detail_text)
        }
        "session_exit" => notice(
            "session_exit",
            "session exit",
            string(entry.get("reason")).map(label),
        ),
        // The initialization record holds the system prompt and task text.
        "session_init" => fixed(
            EventKind::Lifecycle,
            Actor::System,
            "session_init",
            "session initialized (content not shown)",
        ),
        "label" => Mapped::Hidden("label record"),
        "ttsr_injection" => Mapped::Hidden("injection record"),
        "custom" => Mapped::Hidden("extension record"),
        _ => Mapped::Hidden("unsupported record type"),
    }
}

/// Whether a record's first event carries a model call's usage.
pub(super) fn carries_call(entry: &Value) -> bool {
    super::usage::call_identity(entry).is_some()
}
