//! Small pure helpers shared by the TUI screens: text fitting, labels, styles.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::domain::{Harness, ProcessAssociation, SessionMetadata, StatusLabel, ToolStatus};

pub const ELLIPSIS: char = '…';

/// Compact placeholder for a value that is known to be absent or unrecorded.
pub const ABSENT: &str = "—";
/// Compact placeholder for a value that has not been read yet.
pub const LOADING: &str = "…";
/// Compact stand-in for a value whose source could not be read.
pub const READ_FAILED: &str = "couldn't read";

/// Longest tail of the search text shown in the prompt.
const PROMPT_TAIL: usize = 40;

/// Tool-name groups used only to color a name by what the call does.
pub(super) const SHELL_TOOLS: &[&str] =
    &["bash", "shell", "exec", "command", "terminal", "run", "cmd"];
const FILE_TOOLS: &[&str] = &[
    "read",
    "write",
    "edit",
    "apply_patch",
    "patch",
    "str_replace",
    "delete",
    "move",
    "copy",
    "list",
    "ls",
    "glob",
    "grep",
    "find",
    "todo",
    "todo_write",
    "update_plan",
];
const SEARCH_TOOLS: &[&str] = &["search", "web_search", "web_fetch", "fetch"];
const TASK_TOOLS: &[&str] = &["task", "agent", "spawn", "subagent", "delegate"];

/// The footer line while a search is edited. Long text keeps its end (the
/// part being typed) visible. Input must already be display-safe.
pub fn search_prompt(text: &str) -> String {
    let count = text.chars().count();
    let shown: String = if count > PROMPT_TAIL {
        let tail: String = text.chars().skip(count - PROMPT_TAIL).collect();
        format!("{ELLIPSIS}{tail}")
    } else {
        text.to_string()
    };
    format!("Search: {shown}_   Enter keep  Esc cancel  Ctrl+u clear")
}

/// Display-safe text on one physical line: sanitized, redacted, and with line
/// breaks flattened so untrusted text can never start a new screen row.
pub fn one_line(text: &str) -> String {
    crate::ingest::text::display_safe(text).replace(['\n', '\r'], " ")
}

/// Fit `text` into exactly `width` columns: truncate with a visible marker or
/// pad with spaces. Input must already be display-safe.
pub fn fit(text: &str, width: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    if text.width() <= width {
        out.push_str(text);
        used = text.width();
    } else if width > 0 {
        for ch in text.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w + 1 > width {
                break;
            }
            out.push(ch);
            used += w;
        }
        out.push(ELLIPSIS);
        used += 1;
    }
    out.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    out
}

pub fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Codex => "Codex",
        Harness::Claude => "Claude",
        Harness::OpenCode => "OpenCode",
        Harness::Omp => "OMP",
        Harness::Hermes => "Hermes",
        Harness::Pi => "Pi",
    }
}

/// Glyph and text for a synthesized state. Both are always shown together.
pub fn status_text(label: StatusLabel) -> (&'static str, &'static str) {
    match label {
        StatusLabel::Active => ("●", "Active"),
        StatusLabel::Recent => ("○", "Recent"),
        StatusLabel::Quiet => ("!", "Quiet"),
        StatusLabel::PossiblyStalled => ("▲", "Stalled?"),
        StatusLabel::Complete => ("✓", "Complete"),
        StatusLabel::Failed => ("✗", "Failed"),
        StatusLabel::Cancelled => ("×", "Cancelled"),
        StatusLabel::Unavailable => ("-", "Unavailable"),
        StatusLabel::Unknown => ("?", "Unknown"),
    }
}

/// Lowercase state word for chips and legends. Same vocabulary as
/// [`status_text`], without the glyph.
pub fn status_word(label: StatusLabel) -> &'static str {
    match label {
        StatusLabel::Active => "active",
        StatusLabel::Recent => "recent",
        StatusLabel::Quiet => "quiet",
        StatusLabel::PossiblyStalled => "stalled?",
        StatusLabel::Complete => "complete",
        StatusLabel::Failed => "failed",
        StatusLabel::Cancelled => "cancelled",
        StatusLabel::Unavailable => "unavailable",
        StatusLabel::Unknown => "unknown",
    }
}

/// One status chip: glyph, lowercase word, and the age of the last activity
/// when it is known. The age is flattened to one display-safe line.
pub fn status_chip(label: StatusLabel, age: Option<&str>, palette: Palette) -> Span<'static> {
    let (glyph, _) = status_text(label);
    let mut text = format!("{glyph} {}", status_word(label));
    if let Some(age) = age.filter(|age| !age.is_empty()) {
        text.push(' ');
        text.push_str(&one_line(age));
    }
    Span::styled(text, palette.status(label))
}

/// Short, fixed-width agent tag. Without color the label carries its own
/// brackets because the background cue is unavailable.
pub fn agent_tag(harness: Harness, palette: Palette) -> Span<'static> {
    let label = match harness {
        Harness::Codex => "CDX",
        Harness::Claude => "CLD",
        Harness::OpenCode => "OC",
        Harness::Omp => "OMP",
        Harness::Hermes => "HRM",
        Harness::Pi => "PI",
    };
    if palette.no_color {
        Span::raw(format!("[{label}]"))
    } else {
        let background = match harness {
            Harness::Codex => Color::Blue,
            Harness::Claude => Color::LightRed,
            Harness::OpenCode => Color::Green,
            Harness::Omp => Color::Magenta,
            Harness::Hermes => Color::Cyan,
            Harness::Pi => Color::Yellow,
        };
        Span::styled(
            format!(" {label} "),
            Style::default().fg(Color::Black).bg(background),
        )
    }
}

/// Thinking level colored by intensity. Matching ignores case; values outside
/// the known vocabulary stay plain but are shown as recorded.
pub fn effort_span(effort: &str, palette: Palette) -> Span<'static> {
    let style = match effort.trim().to_ascii_lowercase().as_str() {
        "low" | "minimal" => palette.colored(Color::DarkGray),
        "high" => palette.colored(Color::Yellow),
        "max" | "xhigh" => palette.colored(Color::Red),
        _ => Style::default(),
    };
    Span::styled(one_line(effort), style)
}

/// One glyph per tool outcome; the word is shown beside it where space allows.
pub fn tool_glyph(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Completed => "✓",
        ToolStatus::Error => "✗",
        ToolStatus::Pending => "…",
        ToolStatus::Unknown => "?",
    }
}

/// Readable title for list rows and headers. Derived titles lose only their
/// leading machine-made fragments; recorded titles keep their wording. The
/// result is sanitized for display and never empty when the source has
/// readable content.
pub fn display_title(meta: &SessionMetadata) -> String {
    if !meta.title_derived {
        return crate::ingest::text::display_safe(&meta.title);
    }
    let candidate = collapse_whitespace(&crate::ingest::text::display_safe(
        strip_leading_fragments(&meta.title),
    ));
    if candidate.is_empty() {
        return collapse_whitespace(&crate::ingest::text::display_safe(&meta.title));
    }
    candidate
}

/// Strip leading `<…>` tags and balanced `{…}`/`[…]` blocks. Blocks are
/// scanned string-aware so punctuation inside quoted text cannot end them
/// early; an unbalanced or mismatched block is left untouched.
fn strip_leading_fragments(title: &str) -> &str {
    let mut rest = title.trim_start();
    while let Some(stripped) = strip_leading_fragment(rest) {
        rest = stripped.trim_start();
    }
    rest
}

/// Collapse all whitespace runs to single spaces, which also flattens line
/// breaks. Input must already be display-safe.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn strip_leading_fragment(text: &str) -> Option<&str> {
    if let Some(after_tag) = strip_angle_tag(text) {
        return Some(after_tag);
    }
    strip_balanced_block(text)
}

fn strip_angle_tag(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('<')?;
    let end = rest.find('>')?;
    Some(&rest[end + 1..])
}

fn strip_balanced_block(text: &str) -> Option<&str> {
    let mut chars = text.chars();
    match chars.next()? {
        '{' | '[' => {}
        _ => return None,
    }
    // Expected closers, innermost last, so mixed `{…[…]…}` nesting is checked
    // and a closer of the wrong kind rejects the whole block. A growable stack
    // keeps any depth correct, including sibling blocks below the top level.
    let mut expected: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => expected.push('}'),
            '[' => expected.push(']'),
            '}' | ']' => {
                if expected.pop()? != ch {
                    return None;
                }
                if expected.is_empty() {
                    return Some(&text[index + ch.len_utf8()..]);
                }
            }
            _ => {}
        }
    }
    None
}

pub fn process_text(process: &ProcessAssociation) -> String {
    match process {
        ProcessAssociation::Matched { .. } => "matched".into(),
        ProcessAssociation::Candidates { count } => format!("{count} cand."),
        ProcessAssociation::Unmatched => "unmatched".into(),
        ProcessAssociation::NotChecked => "not checked".into(),
        ProcessAssociation::Unavailable => "unavailable".into(),
    }
}

/// Compact token count: `842`, `1.2k`, `91k`, `1.2M`.
pub fn tokens_text(total: Option<u64>) -> String {
    match total {
        None => ABSENT.into(),
        Some(n) if n < 1_000 => n.to_string(),
        Some(n) if n < 10_000 => format!("{:.1}k", n as f64 / 1_000.0),
        Some(n) if n < 1_000_000 => format!("{}k", n / 1_000),
        Some(n) => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

/// Styles; with `no_color` nothing but text modifiers is ever used.
#[derive(Clone, Copy)]
pub struct Palette {
    no_color: bool,
}

impl Palette {
    pub fn new(no_color: bool) -> Self {
        Self { no_color }
    }

    /// True when every style must rely on text alone, with no color cue.
    pub fn is_no_color(&self) -> bool {
        self.no_color
    }

    fn colored(&self, color: Color) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(color)
        }
    }

    /// The `▌` bar that starts a user prompt.
    pub fn prompt_bar(&self) -> Style {
        self.colored(Color::Cyan)
    }

    /// The `▌` bar that starts an agent reply; bold so it differs from a prompt
    /// without relying on color.
    pub fn reply_bar(&self) -> Style {
        self.colored(Color::Cyan).add_modifier(Modifier::BOLD)
    }

    /// Tool names grouped by kind, so the eye can separate shell work, file
    /// work, searches and delegated tasks. Color is decorative: the glyph and
    /// the name carry the same information without it.
    pub fn tool_name(&self, name: &str) -> Style {
        let is = |names: &[&str]| names.iter().any(|n| name.eq_ignore_ascii_case(n));
        if is(SHELL_TOOLS) {
            self.colored(Color::Yellow)
        } else if is(FILE_TOOLS) {
            self.colored(Color::Green)
        } else if is(SEARCH_TOOLS) {
            self.colored(Color::Cyan)
        } else if is(TASK_TOOLS) {
            self.colored(Color::Magenta)
        } else {
            Style::default()
        }
    }

    /// Glyph color for a tool outcome.
    pub fn tool_status(&self, status: ToolStatus) -> Style {
        match status {
            ToolStatus::Completed => self.colored(Color::Green),
            ToolStatus::Error => self.colored(Color::Red),
            ToolStatus::Pending => self.colored(Color::Yellow),
            ToolStatus::Unknown => self.colored(Color::DarkGray),
        }
    }

    pub fn status(&self, label: StatusLabel) -> Style {
        match label {
            StatusLabel::Active => self.colored(Color::Green),
            StatusLabel::Recent => self.colored(Color::Cyan),
            StatusLabel::Quiet | StatusLabel::PossiblyStalled => self.colored(Color::Yellow),
            StatusLabel::Complete => self.colored(Color::Blue),
            StatusLabel::Failed => self.colored(Color::Red),
            StatusLabel::Cancelled | StatusLabel::Unavailable | StatusLabel::Unknown => {
                self.colored(Color::DarkGray)
            }
        }
    }

    pub fn warning(&self) -> Style {
        self.colored(Color::Yellow).add_modifier(Modifier::BOLD)
    }

    pub fn title(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    pub fn dim(&self) -> Style {
        self.colored(Color::DarkGray)
    }

    /// Secondary metadata: ages, provenance, and other quiet notes.
    pub fn quiet(&self) -> Style {
        self.colored(Color::DarkGray)
    }

    /// Uppercase labels that group a panel or section.
    pub fn section(&self) -> Style {
        self.colored(Color::DarkGray).add_modifier(Modifier::BOLD)
    }

    /// Primary values, which must read before their labels.
    pub fn value(&self) -> Style {
        Style::default().add_modifier(Modifier::BOLD)
    }

    /// Subtle background for the selected row. The [`Palette::SELECTION_MARKER`]
    /// and [`Palette::selected`] carry the same cue without color.
    pub fn selection_bg(&self) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().bg(Color::Black)
        }
    }

    /// Marker on every line of the selected item.
    pub const SELECTION_MARKER: &str = "▌";

    /// Accent for the selection marker, so the selected item stands out even
    /// where the subtle background is invisible.
    pub fn selection_marker(&self) -> Style {
        self.colored(Color::Cyan).add_modifier(Modifier::BOLD)
    }

    pub fn selected(&self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }
}
