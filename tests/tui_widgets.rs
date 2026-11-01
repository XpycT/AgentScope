//! Shared visual vocabulary: labels, glyphs, styles, and title cleanup.

mod support;

use agentscope::domain::{
    AggregationClass, CostEstimate, Harness, ModelDetail, SessionKey, SessionMetadata, SourceKey,
    StatusLabel, TimedRate, Timestamp, ToolStatus, UsageSummary,
};
use agentscope::ui::facts::{active_fact, cost_fact, speed_fact, tokens_fact};
use agentscope::ui::widgets::{
    ABSENT, LOADING, Palette, READ_FAILED, agent_tag, display_title, effort_span, status_chip,
    status_word, tool_glyph,
};
use ratatui::style::{Color, Modifier};
use support::facts;

fn meta(title: &str, title_derived: bool) -> SessionMetadata {
    let key = SessionKey {
        harness: Harness::Codex,
        native_id: "synthetic".into(),
        source: SourceKey::File("synthetic.jsonl".into()),
    };
    let mut meta = facts::metadata(key, title);
    meta.title_derived = title_derived;
    meta
}

#[test]
fn agent_tag_uses_short_labels_and_brackets_without_color() {
    assert_eq!(
        agent_tag(Harness::Codex, Palette::new(false)).content,
        " CDX "
    );
    assert_eq!(
        agent_tag(Harness::Hermes, Palette::new(true)).content,
        "[HRM]"
    );
    assert_eq!(agent_tag(Harness::Omp, Palette::new(true)).content, "[OMP]");
    assert_eq!(
        agent_tag(Harness::Claude, Palette::new(true)).content,
        "[CLD]"
    );
    assert_eq!(
        agent_tag(Harness::OpenCode, Palette::new(true)).content,
        "[OC]"
    );
    assert_eq!(agent_tag(Harness::Pi, Palette::new(true)).content, "[PI]");
}

#[test]
fn agent_tag_backgrounds_match_each_harness() {
    for (harness, background) in [
        (Harness::Omp, Color::Magenta),
        (Harness::Codex, Color::Blue),
        (Harness::Claude, Color::LightRed),
        (Harness::OpenCode, Color::Green),
        (Harness::Hermes, Color::Cyan),
        (Harness::Pi, Color::Yellow),
    ] {
        let tag = agent_tag(harness, Palette::new(false));
        assert_eq!(tag.style.fg, Some(Color::Black));
        assert_eq!(tag.style.bg, Some(background));
    }
    let plain = agent_tag(Harness::Omp, Palette::new(true));
    assert_eq!(plain.style, ratatui::style::Style::default());
}

#[test]
fn status_chip_has_glyph_word_and_age() {
    assert_eq!(
        status_chip(StatusLabel::Quiet, Some("22m"), Palette::new(true)).content,
        "! quiet 22m"
    );
    assert_eq!(
        status_chip(StatusLabel::PossiblyStalled, None, Palette::new(true)).content,
        "▲ stalled?"
    );
    assert_eq!(
        status_chip(StatusLabel::Active, Some(""), Palette::new(false)).content,
        "● active"
    );
    assert_eq!(
        status_chip(StatusLabel::Failed, None, Palette::new(false)).style,
        Palette::new(false).status(StatusLabel::Failed)
    );
}

#[test]
fn status_words_are_lowercase() {
    assert_eq!(status_word(StatusLabel::Active), "active");
    assert_eq!(status_word(StatusLabel::Recent), "recent");
    assert_eq!(status_word(StatusLabel::Quiet), "quiet");
    assert_eq!(status_word(StatusLabel::PossiblyStalled), "stalled?");
    assert_eq!(status_word(StatusLabel::Complete), "complete");
    assert_eq!(status_word(StatusLabel::Failed), "failed");
    assert_eq!(status_word(StatusLabel::Cancelled), "cancelled");
    assert_eq!(status_word(StatusLabel::Unavailable), "unavailable");
    assert_eq!(status_word(StatusLabel::Unknown), "unknown");
}

#[test]
fn effort_colors_follow_intensity() {
    let p = Palette::new(false);
    assert_eq!(effort_span("max", p).style.fg, Some(Color::Red));
    assert_eq!(effort_span("high", p).style.fg, Some(Color::Yellow));
    assert_eq!(effort_span("low", p).style.fg, Some(Color::DarkGray));
    assert_eq!(effort_span("medium", p).style.fg, None);
    assert_eq!(effort_span("max", Palette::new(true)).style.fg, None);
    // Case-insensitive, and the two extra names from the design vocabulary.
    assert_eq!(effort_span("XHigh", p).style.fg, Some(Color::Red));
    assert_eq!(effort_span("Minimal", p).style.fg, Some(Color::DarkGray));
    assert_eq!(effort_span("medium", p).content, "medium");
    assert_eq!(effort_span("turbo", p).style.fg, None);
    assert_eq!(effort_span("turbo", p).content, "turbo");
}

#[test]
fn tool_glyphs_cover_all_states() {
    assert_eq!(tool_glyph(ToolStatus::Completed), "✓");
    assert_eq!(tool_glyph(ToolStatus::Error), "✗");
    assert_eq!(tool_glyph(ToolStatus::Pending), "…");
    assert_eq!(tool_glyph(ToolStatus::Unknown), "?");
}

#[test]
fn absent_loading_and_read_failed_are_the_shared_words() {
    assert_eq!(ABSENT, "—");
    assert_eq!(LOADING, "…");
    assert_eq!(READ_FAILED, "couldn't read");
}

#[test]
fn palette_tiers_separate_quiet_values_and_sections() {
    let p = Palette::new(false);
    assert_eq!(p.quiet().fg, Some(Color::DarkGray));
    assert_eq!(p.section().fg, Some(Color::DarkGray));
    assert!(p.section().add_modifier.contains(Modifier::BOLD));
    assert_eq!(p.value().fg, None);
    assert!(p.value().add_modifier.contains(Modifier::BOLD));
    assert_eq!(p.selection_bg().bg, Some(Color::Black));
    assert_eq!(Palette::new(true).quiet().fg, None);
    assert_eq!(Palette::new(true).section().add_modifier, Modifier::BOLD);
    assert_eq!(
        Palette::new(true).selection_bg(),
        ratatui::style::Style::default()
    );
    assert_eq!(Palette::SELECTION_MARKER, "▌");
}

#[test]
fn display_title_strips_leading_fragments_of_derived_titles() {
    let derived = meta(
        r#"<external_codex_apps_open_page>{"a":1} Fix the login bug"#,
        true,
    );
    assert_eq!(display_title(&derived), "Fix the login bug");

    let bracketed = meta("[Feature] <tag>  Ship   it ", true);
    assert_eq!(display_title(&bracketed), "Ship it");
}

#[test]
fn display_title_keeps_recorded_titles() {
    let recorded = meta("{weird} nickname", false);
    assert_eq!(display_title(&recorded), "{weird} nickname");
    let recorded_tag = meta("<external_codex_apps_open_page>Recorded", false);
    assert_eq!(
        display_title(&recorded_tag),
        "<external_codex_apps_open_page>Recorded"
    );
}

#[test]
fn display_title_falls_back_when_cleaning_leaves_nothing() {
    let empty = meta("<tag>{}", true);
    let title = display_title(&empty);
    assert_eq!(title, "<tag>{}");
    assert!(!title.is_empty());
}

#[test]
fn display_title_falls_back_when_sanitizing_empties_the_cleaned_title() {
    // Cleaning removes `<tag>` and leaves a control character; sanitization
    // then empties the candidate, so the sanitized original must be used.
    let zero = meta("<tag>\0", true);
    assert_eq!(display_title(&zero), "<tag>");
}

#[test]
fn display_title_handles_mixed_delimiter_nesting() {
    let valid = meta(r#"{"a":[1,2]} Real title"#, true);
    assert_eq!(display_title(&valid), "Real title");

    let valid_strings = meta(r#"{"a":["]","]"]} String title"#, true);
    assert_eq!(display_title(&valid_strings), "String title");

    // A closer of the wrong kind, or an unfinished nested block, leaves the
    // whole leading block in place instead of discarding title content.
    for malformed in [
        r#"{"a":[} Keep this"#,
        r#"[{"a":1] Keep this"#,
        r#"{"a":[1,2} Keep this"#,
        r#"{"a":[1,2] Keep this"#,
    ] {
        assert_eq!(display_title(&meta(malformed, true)), malformed);
    }
}

#[test]
fn display_title_strips_deeply_nested_balanced_blocks() {
    // 33 is one past the inline stack slot count; 130 forces several spill
    // entries, so balanced blocks are stripped at any depth.
    for depth in [33, 64, 130] {
        let title = format!(
            "{}{}{} Real title",
            "[".repeat(depth),
            "0",
            "]".repeat(depth)
        );
        assert_eq!(display_title(&meta(&title, true)), "Real title");
    }

    let unfinished = format!("{}0", "[".repeat(33));
    assert_eq!(display_title(&meta(&unfinished, true)), unfinished);
}

#[test]
fn display_title_handles_deep_sibling_blocks() {
    // Sibling blocks after any opener must compare against their own closer,
    // not a stale entry left by an earlier sibling.
    let balanced = format!("{}{}{} Real title", "[".repeat(33), "[]{}", "]".repeat(33));
    assert_eq!(display_title(&meta(&balanced, true)), "Real title");

    // The mismatched `]` closing the `{` sibling rejects the whole prefix.
    let malformed = format!("{}{}{} Real title", "[".repeat(33), "[]{]", "]".repeat(33));
    assert_eq!(display_title(&meta(&malformed, true)), malformed);
}

#[test]
fn display_title_handles_braces_inside_json_strings() {
    let json = meta(r#"{"a":"}"} Real title"#, true);
    assert_eq!(display_title(&json), "Real title");

    let escaped = meta(r#"{"a":"\"}\""} Escaped title"#, true);
    assert_eq!(display_title(&escaped), "Escaped title");

    let unbalanced = meta(r#"{"a":1 Real title"#, true);
    assert_eq!(display_title(&unbalanced), r#"{"a":1 Real title"#);
}

#[test]
fn display_title_sanitizes_escape_sequences() {
    let escaped = meta("\x1b[31mred", true);
    let title = display_title(&escaped);
    assert!(!title.contains('\x1b'));
    assert_eq!(title, "red");

    let recorded = meta("\x1b[31mred", false);
    assert!(!display_title(&recorded).contains('\x1b'));
}

// ---- compact facts: `—`, `…`, `+`, `~` ------------------------------------

const NOW: Timestamp = 100_000_000;

fn fact_model(id: &str) -> ModelDetail {
    ModelDetail {
        id: id.into(),
        provider: None,
        variant: None,
        effort: None,
        configured_effort: None,
    }
}

#[test]
fn unknown_tokens_and_cost_are_dash() {
    assert_eq!(tokens_fact(None, true).head, ABSENT);
    assert_eq!(cost_fact(None, true).head, ABSENT);

    let recorded_nothing = UsageSummary::default();
    assert_eq!(tokens_fact(Some(&recorded_nothing), true).head, ABSENT);
    assert_eq!(cost_fact(Some(&recorded_nothing), true).head, ABSENT);
}

#[test]
fn unhydrated_facts_are_ellipsis() {
    let s = facts::summary(meta("Unread", false));
    assert_eq!(tokens_fact(None, false).head, LOADING);
    assert_eq!(cost_fact(None, false).head, LOADING);
    assert_eq!(active_fact(&s, false).head, LOADING);
    assert_eq!(speed_fact(&s, false, NOW).head, LOADING);
}

#[test]
fn partial_tokens_keep_plus_and_estimated_cost_keeps_tilde() {
    let usage = UsageSummary {
        total: Some(185_722_239),
        class: AggregationClass::Partial,
        cost: CostEstimate {
            usd: Some(1.77),
            class: AggregationClass::Estimated,
            ..CostEstimate::default()
        },
        ..UsageSummary::default()
    };
    let tokens = tokens_fact(Some(&usage), true);
    assert_eq!(tokens.head, "185.7M+");
    assert_eq!(tokens.sub, "partial");
    let cost = cost_fact(Some(&usage), true);
    assert_eq!(cost.head, "~$1.77");
    assert_eq!(cost.sub, "estimated");
    assert!(!cost.quiet);
}

#[test]
fn known_subtotal_without_total_keeps_plus() {
    let usage = UsageSummary {
        cost: CostEstimate {
            usd: None,
            class: AggregationClass::Partial,
            known_subtotal: 4.58,
            ..CostEstimate::default()
        },
        ..UsageSummary::default()
    };
    let cost = cost_fact(Some(&usage), true);
    assert_eq!(cost.head, "$4.58+");
    assert_eq!(cost.sub, "partial");
}

#[test]
fn provenance_word_moves_to_sub() {
    let usage = UsageSummary {
        total: Some(185_722_239),
        class: AggregationClass::Derived,
        ..UsageSummary::default()
    };
    let tokens = tokens_fact(Some(&usage), true);
    assert_eq!(tokens.head, "185.7M");
    assert!(!tokens.head.contains("derived"), "{}", tokens.head);
    assert_eq!(tokens.sub, "derived");
}

#[test]
fn speed_fact_marks_old_model_quiet() {
    let mut s = facts::summary(meta("Speed", false));
    s.meta.model = Some(fact_model("deepseek"));
    s.metrics.speed.latest = Some(TimedRate {
        model: Some(fact_model("gpt-6.1-sol")),
        request_id: None,
        started_at: Some(NOW - 190_000),
        completed_at: Some(NOW - 180_000),
        output_tokens: 385,
        duration_ms: 10_000.0,
        ttft_ms: None,
        tokens_per_second: 38.5,
    });
    let speed = speed_fact(&s, true, NOW);
    assert_eq!(speed.head, "38.5");
    assert!(speed.quiet);
    assert!(speed.sub.starts_with("old gpt-6.1-sol"), "{}", speed.sub);
}
