//! Small pure helpers shared by the TUI screens: text fitting, labels, styles.

use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::domain::{Harness, ProcessAssociation, StatusLabel};

pub const ELLIPSIS: char = '…';

/// Longest tail of the search text shown in the prompt.
const PROMPT_TAIL: usize = 40;

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
        None => "—".into(),
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

    fn colored(&self, color: Color) -> Style {
        if self.no_color {
            Style::default()
        } else {
            Style::default().fg(color)
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

    pub fn selected(&self) -> Style {
        Style::default().add_modifier(Modifier::REVERSED)
    }
}
