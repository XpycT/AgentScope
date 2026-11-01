//! Bounded mapping of one native Pi record to displayable transcript slots, pinned
//! to earendil-works/pi `docs/session-format.md` and `docs/message-types.md`.
//!
//! A record maps to zero or more *slots*: an assistant message yields one slot per
//! content block (text, reasoning, tool call), so a long turn is several events that
//! share one source line. Free-form bodies are bounded here and sanitized only when an
//! event is built. Nothing reads a file: extension state (`custom`) is a diagnostic,
//! an extension message is shown only when the producer marked it displayable, and a
//! record the record layer did not interpret (`label`, future types) stays a
//! diagnostic instead of invented content. Tool `details`, thinking signatures,
//! image data and nested tool calls are never copied.

use serde_json::{Map, Value};

use crate::domain::{Actor, EventKind, ToolStatus};
use crate::ingest::text::display_safe;

/// Raw bytes copied per free-form field (previews are cut again for display).
const FIELD_MAX: usize = 8192;
/// Content blocks of one record that become events.
const BLOCKS_MAX: usize = 64;
/// Longest diagnostic record type or tool label kept.
const LABEL_MAX: usize = 128;

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
    /// The record's events, in content-block order (never empty).
    Events(Vec<Slot>),
    /// A record known to exist but not shown; the label says why and is bounded.
    Hidden(String),
}

fn bounded(raw: &str) -> Text {
    // Mask (and sanitize) the whole field before cutting: a quoted credential whose
    // closing delimiter lies beyond the cut must still be recognized, so a truncated
    // prefix can never contain an unmasked secret. `len` stays the original length.
    let len = raw.len();
    let safe = display_safe(raw);
    let mut end = safe.len().min(FIELD_MAX);
    while !safe.is_char_boundary(end) {
        end -= 1;
    }
    let cut = end < safe.len();
    let mut raw = safe;
    raw.truncate(end);
    Text { raw, len, cut }
}

fn owned_text(raw: String) -> Text {
    let len = raw.len();
    let safe = display_safe(&raw);
    let mut end = safe.len().min(FIELD_MAX);
    while !safe.is_char_boundary(end) {
        end -= 1;
    }
    let cut = end < safe.len();
    let mut raw = safe;
    raw.truncate(end);
    Text { raw, len, cut }
}

fn string(value: Option<&Value>) -> Option<&str> {
    value?.as_str().filter(|s| !s.is_empty())
}

/// The text of a message `content`: a plain string or the text blocks of an array
/// (an image is a placeholder, never its data).
fn content_text(detail: bool, content: Option<&Value>) -> Option<Text> {
    if !detail {
        return None;
    }
    match content? {
        Value::String(text) if !text.is_empty() => Some(bounded(text)),
        Value::Array(blocks) => {
            let mut raw = String::new();
            let mut len = 0usize;
            let mut cut = blocks.len() > BLOCKS_MAX;
            for block in blocks.iter().take(BLOCKS_MAX) {
                let piece = match block.get("type").and_then(Value::as_str) {
                    Some("text") => block.get("text").and_then(Value::as_str).unwrap_or(""),
                    Some("image") => "[image]",
                    _ => "",
                };
                if piece.is_empty() {
                    continue;
                }
                let part = bounded(piece);
                cut |= part.cut;
                if len > 0 {
                    len += 1;
                    if raw.len() < FIELD_MAX {
                        raw.push('\n');
                    }
                }
                len += piece.len();
                if raw.len() < FIELD_MAX {
                    raw.push_str(&part.raw);
                }
            }
            (len > 0).then(|| {
                let mut text = owned_text(raw);
                text.len = len;
                text.cut |= cut;
                text
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

/// A diagnostic record type: sanitized and credential-masked before storage, because it
/// is untrusted producer text and the label is surfaced (and Debugged as a diagnostic).
fn safe_type(text: &str) -> String {
    display_safe(text).chars().take(64).collect()
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

/// A short single-field notice ("model changed to X"). The text is display content,
/// so it is bounded; `detail` keeps index-time mapping text-free.
fn notice(record_type: &'static str, prefix: &str, value: Option<String>, detail: bool) -> Mapped {
    let text = detail.then(|| match value {
        Some(value) => owned_text(format!("{prefix}: {value}")),
        None => owned_text(prefix.to_owned()),
    });
    Mapped::Events(vec![slot(
        EventKind::System,
        Actor::System,
        record_type,
        text,
    )])
}

fn tool_input(arguments: Option<&Value>) -> Option<Text> {
    let arguments = arguments?;
    match arguments {
        Value::Null => None,
        Value::String(text) => Some(bounded(text)),
        other => serde_json::to_string(other).ok().map(|text| bounded(&text)),
    }
}

fn assistant(message: &Map<String, Value>, detail: bool) -> Mapped {
    let mut slots = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => slots.push(slot(
            EventKind::AssistantMessage,
            Actor::Assistant,
            "message/assistant",
            detail.then(|| bounded(text)),
        )),
        Some(Value::Array(blocks)) => {
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
                    Some("thinking") => {
                        let text = detail.then(|| {
                            let visible = block
                                .get("thinking")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if !visible.is_empty() {
                                Some(bounded(visible))
                            } else if block.get("redacted").and_then(Value::as_bool) == Some(true) {
                                Some(owned_text("[redacted reasoning]".into()))
                            } else {
                                None
                            }
                        });
                        slots.push(slot(
                            EventKind::Reasoning,
                            Actor::Assistant,
                            "message/assistant#thinking",
                            text.flatten(),
                        ));
                    }
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
                            // The matching result is a separate record.
                            status: ToolStatus::Unknown,
                        }),
                    }),
                    // Unknown content blocks stay in the source view only.
                    _ => {}
                }
            }
            if blocks.len() > BLOCKS_MAX {
                // Later blocks are not events here: say so, and point at the source.
                slots.push(slot(
                    EventKind::System,
                    Actor::System,
                    "message/omitted",
                    detail.then(|| {
                        owned_text(format!(
                            "{} further content blocks are not shown; open the source record",
                            blocks.len() - BLOCKS_MAX
                        ))
                    }),
                ));
            }
        }
        _ => {}
    }
    if slots.is_empty() {
        // An assistant message with no displayable block is still one event so its
        // recorded usage stays visible.
        slots.push(slot(
            EventKind::AssistantMessage,
            Actor::Assistant,
            "message/assistant",
            None,
        ));
    }
    Mapped::Events(slots)
}

fn tool_result(message: &Map<String, Value>, detail: bool) -> Mapped {
    let status = if message.get("isError").and_then(Value::as_bool) == Some(true) {
        ToolStatus::Error
    } else {
        ToolStatus::Completed
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
            output: detail
                .then(|| content_text(true, message.get("content")))
                .flatten(),
            status,
        }),
    }])
}

fn message(object: &Map<String, Value>, detail: bool) -> Mapped {
    let Some(message) = object.get("message").and_then(Value::as_object) else {
        return Mapped::Hidden("unreadable Pi message".into());
    };
    match message.get("role").and_then(Value::as_str).unwrap_or("") {
        "assistant" => assistant(message, detail),
        "user" => Mapped::Events(vec![slot(
            EventKind::UserPrompt,
            Actor::User,
            "message/user",
            content_text(detail, message.get("content")),
        )]),
        "toolResult" => tool_result(message, detail),
        // A system message carries the prompt sections and tool loadout; the body is
        // only in the source view.
        "system" => Mapped::Events(vec![slot(
            EventKind::System,
            Actor::System,
            "message/system",
            detail.then(|| owned_text("system prompt/tools checkpoint".into())),
        )]),
        // `bashExecution` and any future/extension role: its content is a context
        // message, so it is shown as a system event rather than dropped.
        _ => Mapped::Events(vec![slot(
            EventKind::System,
            Actor::System,
            "message/other",
            content_text(detail, message.get("content")),
        )]),
    }
}

/// Maps one interpreted Pi record. Only shapes `record::parse_entry` accepts reach
/// this function; it re-reads the record itself so index-time and display-time
/// mapping agree on the slot count.
pub(super) fn map(value: &Value, detail: bool) -> Mapped {
    let Some(object) = value.as_object() else {
        return Mapped::Hidden("unreadable Pi record".into());
    };
    match object.get("type").and_then(Value::as_str).unwrap_or("") {
        "message" => message(object, detail),
        "model_change" => {
            let route = match (
                string(object.get("provider")),
                string(object.get("modelId")),
            ) {
                (Some(provider), Some(model)) => Some(format!("{provider}/{model}")),
                (Some(provider), None) => Some(provider.to_owned()),
                (None, Some(model)) => Some(model.to_owned()),
                (None, None) => None,
            };
            notice("model_change", "model", route, detail)
        }
        "thinking_level_change" => notice(
            "thinking_level_change",
            "thinking level",
            string(object.get("thinkingLevel")).map(str::to_owned),
            detail,
        ),
        "usage" => {
            let kind = string(object.get("kind")).unwrap_or("usage");
            Mapped::Events(vec![slot(
                EventKind::System,
                Actor::System,
                "usage",
                detail.then(|| owned_text(format!("usage entry: {kind}"))),
            )])
        }
        "compaction" => {
            let tokens = object.get("tokensBefore").and_then(Value::as_u64);
            notice(
                "compaction",
                "compaction",
                tokens.map(|before| format!("{before} tokens before")),
                detail,
            )
            .into_compaction()
        }
        "branch_summary" => {
            let text = detail.then(|| {
                let summary = object
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if summary.is_empty() {
                    owned_text("branch summary".into())
                } else {
                    owned_text(format!("branch summary: {summary}"))
                }
            });
            Mapped::Events(vec![slot(
                EventKind::Compaction,
                Actor::System,
                "branch_summary",
                text,
            )])
        }
        "context_edit" => {
            let detail_text = match string(object.get("targetId")) {
                Some(target) if object.get("replacement").is_some_and(|v| !v.is_null()) => {
                    Some(format!("replace {target}"))
                }
                Some(target) => Some(format!("omit {target}")),
                None => None,
            };
            notice("context_edit", "context edit", detail_text, detail)
        }
        "session_info" => notice(
            "session_info",
            "session name",
            string(object.get("name")).map(str::to_owned),
            detail,
        ),
        // Extension state persists but does not participate in context.
        "custom" => Mapped::Hidden("extension record".into()),
        "custom_message" => {
            if object.get("display").and_then(Value::as_bool) == Some(true) {
                Mapped::Events(vec![slot(
                    EventKind::System,
                    Actor::System,
                    "custom_message",
                    content_text(detail, object.get("content")),
                )])
            } else {
                Mapped::Hidden("extension message not displayed by the producer".into())
            }
        }
        other => Mapped::Hidden(format!("unsupported Pi record type: {}", safe_type(other))),
    }
}

impl Mapped {
    /// The same slots with system notices promoted to compaction markers.
    fn into_compaction(self) -> Self {
        match self {
            Self::Events(slots) => Self::Events(
                slots
                    .into_iter()
                    .map(|slot| Slot {
                        kind: EventKind::Compaction,
                        ..slot
                    })
                    .collect(),
            ),
            hidden => hidden,
        }
    }
}
