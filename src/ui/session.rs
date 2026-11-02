//! Session information dashboard: tokens, cost, time and speed, context,
//! family and process, under the shared three-line header. Pure: reads `App`,
//! draws into a `Frame`; every value is display-safe before it is drawn.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Widget, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, DetailsState};
use crate::domain::{
    AggregationClass, CompactionObservation, CompactionScope, CompactionSummary, ContextUsage,
    CostBreakdown, CostEstimate, ModelDetail, MoneySource, OriginSurface, ProcessAssociation,
    SessionLink, SessionMetadata, SessionSummary, TimedRate, Timestamp, TokenCategories,
    UsageSummary,
};
use crate::ingest::text::display_safe;
use crate::time::relative_label;

use super::RosterContext;
use super::inspector::{class_name, grouped, money_text, process_line};
use super::preview::token_bar;
use super::widgets::{
    ABSENT, LOADING, Palette, READ_FAILED, fit, harness_name, one_line, tokens_text,
};

/// Panels stack in one column below this width.
const TWO_COLUMN_MIN: usize = 90;
/// Blank columns between the two panel columns.
const COLUMN_GAP: usize = 2;
/// Most rows the dashboard body may occupy; the page budgets bound the real
/// content well below this.
const MAX_DASHBOARD_ROWS: usize = 4_000;
/// Number of dashboard panels, each with two border rows outside the content
/// allowance.
const PANEL_COUNT: usize = 6;
/// Rows kept available for the quiet closing lines.
const BOTTOM_RESERVE: usize = 16;
/// Header height, matching the inspector.
const HEADER_LINES: u16 = 3;
/// Quiet status sentence under the dashboard.
const STATUS: &str =
    "Usage is persisted evidence; cost is derived only from the dated price table.";
/// Footer hints for the dashboard.
const FOOTER: &str = "j/k scroll  m older compactions  Space summaries  Esc back";

pub(super) fn link_text(link: &Option<SessionLink>) -> String {
    match link {
        None => "none".into(),
        Some(SessionLink::Resolved(key)) => format!(
            "resolved — {} {}",
            harness_name(key.harness),
            one_line(&key.native_id)
        ),
        Some(SessionLink::Ambiguous) => "ambiguous — more than one candidate session".into(),
        Some(SessionLink::Unresolved(id)) => {
            format!("unresolved — id {} not found in the roster", one_line(id))
        }
    }
}

/// Per-leg dollars. A `None` leg is unknown or unused, never a free amount.
fn legs_text(b: &CostBreakdown) -> String {
    [
        ("input", b.input),
        ("cache read", b.cache_read),
        ("cache write 5m", b.cache_write_5m),
        ("cache write 1h", b.cache_write_1h),
        ("cache write other", b.cache_write_other),
        ("output", b.output),
        ("reasoning", b.reasoning),
    ]
    .iter()
    .map(|(label, usd)| {
        usd.map_or_else(
            || format!("{label} Unknown"),
            |v| format!("{label} {}", money_text(v)),
        )
    })
    .collect::<Vec<_>>()
    .join(" · ")
}

fn when(ts: Option<Timestamp>) -> String {
    ts.map_or_else(|| "Unknown".into(), crate::time::format_local)
}

/// `70_000` -> `1m 10s`; never a bare zero for a missing measurement.
fn span_text(ms: u64) -> String {
    if ms < 1_000 {
        return format!("{ms} ms");
    }
    let secs = ms / 1_000;
    let (days, secs) = (secs / 86_400, secs % 86_400);
    let (hours, secs) = (secs / 3_600, secs % 3_600);
    let (minutes, secs) = (secs / 60, secs % 60);
    let mut out = String::new();
    for (value, unit) in [(days, 'd'), (hours, 'h'), (minutes, 'm')] {
        if value > 0 {
            out.push_str(&format!("{value}{unit} "));
        }
    }
    out.push_str(&format!("{secs}s"));
    out
}

fn measured_span(ms: Option<f64>) -> String {
    ms.and_then(|v| (v.is_finite() && v >= 0.0).then_some(v))
        .map_or_else(|| "Unknown".into(), |v| span_text(v.round() as u64))
}

fn rate_text(rate: Option<f64>) -> String {
    match rate {
        Some(r) if r.is_finite() && r > 0.0 => format!("{r:.1} tok/s"),
        _ => "Unknown".into(),
    }
}

/// A headline value and its qualifier, shared by the roster row, the preview and
/// the details view so no surface prices, times or rates anything itself.
pub struct Fact {
    /// The compact value printed in the roster column or preview line.
    pub head: String,
    /// Provenance shown beside the value where the surface has room for it.
    pub sub: String,
    /// The value is real but no longer describes the current model.
    pub quiet: bool,
}

impl Fact {
    fn new(head: impl Into<String>, sub: impl Into<String>) -> Self {
        Self {
            head: head.into(),
            sub: sub.into(),
            quiet: false,
        }
    }

    /// Facts of a metadata-only row: not read yet, which is not zero.
    fn loading() -> Self {
        Self::new(LOADING, "loading")
    }

    /// Hydrated, but the source recorded no value: absent, which is not zero.
    fn absent() -> Self {
        Self::new(ABSENT, "")
    }

    /// The source could not be read. Quiet, because failure is not a value.
    pub fn failed() -> Self {
        let mut fact = Self::new(READ_FAILED, "read failed");
        fact.quiet = true;
        fact
    }
}

/// `1h 5m`, `3m 12s`, `45s`: two units at most, for narrow columns.
fn short_span(ms: u64) -> String {
    let secs = ms / 1_000;
    match secs {
        0 if ms < 1_000 => format!("{ms}ms"),
        0..=59 => format!("{secs}s"),
        60..=3_599 => format!("{}m {}s", secs / 60, secs % 60),
        3_600..=86_399 => format!("{}h {}m", secs / 3_600, secs % 3_600 / 60),
        _ => format!("{}d {}h", secs / 86_400, secs % 86_400 / 3_600),
    }
}

/// Lifetime tokens. A partial total is a lower bound, not the whole.
pub fn tokens_fact(usage: Option<&UsageSummary>, hydrated: bool) -> Fact {
    if !hydrated {
        return Fact::loading();
    }
    let Some(usage) = usage else {
        return Fact::absent();
    };
    match (usage.total, usage.class) {
        (None, class) => Fact::new(ABSENT, class_name(class)),
        (Some(total), AggregationClass::Partial) => {
            Fact::new(format!("{}+", tokens_text(Some(total))), "partial")
        }
        (Some(total), class) => Fact::new(tokens_text(Some(total)), class_name(class)),
    }
}

/// Legs whose tokens were recorded but could not be priced.
fn missing_legs(usage: &UsageSummary) -> Vec<&'static str> {
    let c = &usage.categories;
    let b = &usage.cost.breakdown;
    let used = |n: Option<u64>| n.is_some_and(|n| n > 0);
    [
        ("input", b.input, used(c.input)),
        ("cache read", b.cache_read, used(c.cache_read)),
        ("cache write 5m", b.cache_write_5m, used(c.cache_write_5m)),
        ("cache write 1h", b.cache_write_1h, used(c.cache_write_1h)),
        (
            "cache write",
            b.cache_write_other,
            used(c.cache_write) && c.cache_write_5m.is_none() && c.cache_write_1h.is_none(),
        ),
        ("output", b.output, used(c.output)),
        ("reasoning", b.reasoning, used(c.reasoning)),
    ]
    .into_iter()
    .filter(|(_, price, used)| *used && price.is_none())
    .map(|(label, _, _)| label)
    .collect()
}

/// API-equivalent estimate. Only a complete amount reads as a total; a priced
/// subset reads as a lower bound with its missing coverage.
pub fn cost_fact(usage: Option<&UsageSummary>, hydrated: bool) -> Fact {
    if !hydrated {
        return Fact::loading();
    }
    let Some(usage) = usage else {
        return Fact::absent();
    };
    let cost = &usage.cost;
    match (cost.usd, cost.class) {
        (Some(usd), AggregationClass::Estimated) => Fact::new(
            format!("~{}", money_text(usd)),
            if cost.reference {
                "reference estimate"
            } else {
                "estimated"
            },
        ),
        (Some(usd), class) if class != AggregationClass::Unknown => {
            Fact::new(money_text(usd), class_name(class))
        }
        _ if cost.known_subtotal > 0.0 => {
            let cov = &usage.coverage;
            let sub = if cov.observed_requests > 0 && cov.priced_requests < cov.observed_requests {
                format!("partial {}/{}", cov.priced_requests, cov.observed_requests)
            } else {
                "partial".to_string()
            };
            Fact::new(format!("{}+", money_text(cost.known_subtotal)), sub)
        }
        _ => Fact::absent(),
    }
}

/// Measured active time; an incomplete history is a lower bound.
pub fn active_fact(s: &SessionSummary, hydrated: bool) -> Fact {
    if !hydrated {
        return Fact::loading();
    }
    let t = &s.metrics.timing;
    match t.active_ms {
        Some(ms) if t.active_complete => Fact::new(short_span(ms), "active"),
        Some(ms) => Fact::new(format!("{}+", short_span(ms)), "partial"),
        None => Fact::absent(),
    }
}

/// The latest speed was measured on a model other than the session's current one.
fn speed_is_stale(s: &SessionSummary, latest: &TimedRate) -> bool {
    let current = s.meta.model.as_ref();
    latest
        .model
        .as_ref()
        .is_some_and(|lm| current.is_some_and(|cm| cm.id != lm.id || cm.provider != lm.provider))
}

/// Latest timed whole-request speed with its model and age. Unknown is never 0.
pub fn speed_fact(s: &SessionSummary, hydrated: bool, now: Timestamp) -> Fact {
    if !hydrated {
        return Fact::loading();
    }
    let Some(latest) = &s.metrics.speed.latest else {
        return Fact::absent();
    };
    let rate = latest.tokens_per_second;
    if !rate.is_finite() || rate <= 0.0 {
        return Fact::absent();
    }
    let age = latest
        .completed_at
        .or(latest.started_at)
        .map(|t| format!("{} ago", relative_label(t, now)));
    let model = latest
        .model
        .as_ref()
        .map(|m| super::widgets::one_line(&m.id));
    let stale = speed_is_stale(s, latest);
    let mut sub = match (stale, model) {
        (true, Some(model)) => format!("old {model}"),
        (true, None) => "old model".to_string(),
        (false, Some(model)) => model,
        (false, None) => String::new(),
    };
    if let Some(age) = age {
        if !sub.is_empty() {
            sub.push_str(" · ");
        }
        sub.push_str(&age);
    }
    Fact {
        quiet: stale,
        ..Fact::new(format!("{rate:.1}"), sub)
    }
}

/// Occupancy of the recorded window. Without both figures there is no percentage.
pub(super) fn context_use(c: &ContextUsage) -> String {
    match (c.used, c.capacity) {
        (Some(used), Some(window)) if window > 0 => {
            format!("{}% of window", used.saturating_mul(100) / window)
        }
        (Some(_), _) => "Unknown (window not recorded)".into(),
        _ => "Unknown".into(),
    }
}

fn model_label(model: &Option<ModelDetail>) -> String {
    model
        .as_ref()
        .map_or_else(|| "Unknown".into(), |m| one_line(&m.id))
}

/// Recorded billing/routing route, never the harness. Unknown when the provider
/// omitted it; it is not filled in from the harness or a price table.
fn billing_provider(model: &Option<ModelDetail>) -> String {
    model
        .as_ref()
        .and_then(|m| m.provider.as_deref())
        .map_or_else(|| "Unknown".into(), one_line)
}

fn coverage_note(complete: bool) -> &'static str {
    if complete {
        ""
    } else {
        " (incomplete coverage)"
    }
}

/// Latest recorded origin surface with its (sanitized) raw label.
pub(super) fn origin_label(meta: &SessionMetadata) -> String {
    let Some(origin) = meta.origins.iter().max_by_key(|o| o.recorded_at) else {
        return "Unknown".into();
    };
    let surface = match origin.surface {
        OriginSurface::Cli => "CLI",
        OriginSurface::DesktopCode => "Desktop Code",
        OriginSurface::Ide => "IDE",
        OriginSurface::Cowork => "Cowork",
        OriginSurface::Other => "Other",
        OriginSurface::Unknown => "Unknown",
    };
    // Only the surface's own name is shown; the recorded raw label stays in the data.
    surface.to_string()
}

/// Session-level compaction line. Event cost, aggregate cost and context
/// reduction are separate facts; an aggregate is never split across events.
pub(super) fn compactions_line(cm: &CompactionSummary) -> String {
    let observed = match (cm.observed, cm.history_complete) {
        (0, true) => "none observed".to_string(),
        (n, true) => format!("{n} observed"),
        (n, false) => format!("{n} observed (history incomplete)"),
    };
    let cost = match (cm.known_cost_usd, cm.cost_scope) {
        (Some(usd), CompactionScope::Event) if cm.unpriced > 0 || !cm.history_complete => {
            format!("{} known subtotal (not a total)", money_text(usd))
        }
        (Some(usd), CompactionScope::Event) => money_text(usd),
        (Some(usd), CompactionScope::Aggregate) => {
            format!("{} aggregate (not attributed per event)", money_text(usd))
        }
        _ => "Unknown".into(),
    };
    format!(
        "Compactions: {observed} · cost {cost} · {} unpriced",
        cm.unpriced
    )
}

/// Money the source recorded for one compaction event, never a computed price.
fn compaction_event_cost(c: &CompactionObservation) -> String {
    match c.scope {
        CompactionScope::Event => {
            let money = c.usage.as_ref().and_then(|u| {
                u.money
                    .reported_api
                    .as_ref()
                    .map(|m| (m.usd, "reported"))
                    .or_else(|| {
                        u.money
                            .saved_estimate
                            .as_ref()
                            .map(|m| (m.usd, "saved estimate"))
                    })
            });
            money.map_or_else(
                || "not recorded for this event".into(),
                |(usd, label)| format!("{} ({label})", money_text(usd)),
            )
        }
        CompactionScope::Aggregate => "session aggregate only, not attributed to this event".into(),
        CompactionScope::Unknown => "Unknown".into(),
    }
}

/// Context before and after one compaction: a reduction of the window, not a bill.
fn compaction_reduction(c: &CompactionObservation) -> String {
    let token = |v: Option<u64>| v.map_or_else(|| "Unknown".into(), grouped);
    format!(
        "context {} → {} (removed {})",
        token(c.before_tokens),
        token(c.after_tokens),
        token(crate::metrics::tokens_removed(c))
    )
}

/// How the on-demand detail pages are shown.
pub(super) struct PageView<'a> {
    pub state: &'a DetailsState,
    /// Compaction summaries are shown in full.
    pub expanded: bool,
}

/// Most characters of a collapsed compaction summary.
const COLLAPSED_SUMMARY_CHARS: usize = 100;

/// One bordered dashboard panel.
struct Panel {
    title: &'static str,
    rows: Vec<Line<'static>>,
}

impl Panel {
    fn new(title: &'static str, rows: Vec<Line<'static>>) -> Self {
        Self { title, rows }
    }

    /// The block border adds one row above and below the content.
    fn height(&self) -> u16 {
        u16::try_from(self.rows.len().saturating_add(2)).unwrap_or(u16::MAX)
    }
}

/// Shared row allowance for one dashboard frame. Panels and wrapped prose spend
/// from it as they are built, so page data can never allocate rows beyond the
/// scrollable budget.
struct RowBudget {
    remaining: usize,
}

impl RowBudget {
    fn new(remaining: usize) -> Self {
        Self { remaining }
    }

    fn remaining(&self) -> usize {
        self.remaining
    }

    /// Build a row only when the allowance still has room for it.
    fn push_lazy(&mut self, rows: &mut Vec<Line<'static>>, build: impl FnOnce() -> Line<'static>) {
        if self.remaining == 0 {
            return;
        }
        rows.push(build());
        self.remaining -= 1;
    }

    fn extend(&mut self, rows: &mut Vec<Line<'static>>, lines: Vec<Line<'static>>) {
        let take = lines.len().min(self.remaining);
        rows.extend(lines.into_iter().take(take));
        self.remaining -= take;
    }

    /// Charge a fully built panel and drop any rows beyond the allowance.
    fn consume(&mut self, rows: &mut Vec<Line<'static>>) {
        let take = rows.len().min(self.remaining);
        rows.truncate(take);
        self.remaining -= take;
    }
}

/// One styled row clipped to the panel's content width.
fn styled_row(parts: Vec<(String, Style)>, width: usize, _palette: Palette) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0usize;
    for (text, style) in parts {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let text = if UnicodeWidthStr::width(text.as_str()) > remaining {
            fit(&text, remaining)
        } else {
            text
        };
        used += UnicodeWidthStr::width(text.as_str());
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
    }
    Line::from(spans)
}

fn note(text: impl AsRef<str>, width: usize, palette: Palette) -> Line<'static> {
    Line::styled(fit(text.as_ref(), width), palette.quiet())
}

fn labeled(label: &str, value: impl Into<String>, width: usize, palette: Palette) -> Line<'static> {
    styled_row(
        vec![
            (format!("{label}: "), palette.quiet()),
            (value.into(), palette.value()),
        ],
        width,
        palette,
    )
}

fn loading_line(width: usize, palette: Palette) -> Line<'static> {
    note("loading…", width, palette)
}

/// Word-wrap prose to the panel width, hard-splitting only a word wider than the
/// panel. Summary and provenance sentences stay readable instead of breaking
/// mid-word. At most `limit` rows are produced.
fn wrap_words_limit(text: &str, width: usize, limit: usize) -> Vec<String> {
    let width = width.max(1);
    let mut rows = Vec::new();
    if limit == 0 {
        return rows;
    }
    for line in text.split('\n') {
        let mut current = String::new();
        for word in line.split_whitespace() {
            let word_width = UnicodeWidthStr::width(word);
            if !current.is_empty() {
                if UnicodeWidthStr::width(current.as_str()) + 1 + word_width <= width {
                    current.push(' ');
                } else {
                    rows.push(std::mem::take(&mut current));
                    if rows.len() == limit {
                        return rows;
                    }
                }
            }
            if current.is_empty() && word_width > width {
                let mut used = 0usize;
                for ch in word.chars() {
                    let ch_width = ch.width().unwrap_or(0);
                    if used + ch_width > width && !current.is_empty() {
                        rows.push(std::mem::take(&mut current));
                        used = 0;
                        if rows.len() == limit {
                            return rows;
                        }
                    }
                    current.push(ch);
                    used += ch_width;
                }
            } else {
                current.push_str(word);
            }
        }
        rows.push(current);
        if rows.len() == limit {
            return rows;
        }
    }
    rows
}

/// Wrap quiet prose to the panel width so no recorded sentence is truncated,
/// producing at most `limit` rows.
fn quiet_rows_limit(
    text: &str,
    width: usize,
    palette: Palette,
    limit: usize,
) -> Vec<Line<'static>> {
    wrap_words_limit(text, width, limit)
        .into_iter()
        .map(|row| Line::styled(row, palette.quiet()))
        .collect()
}

/// Wrap quiet prose to the panel width so no recorded sentence is truncated.
fn quiet_rows(text: &str, width: usize, palette: Palette) -> Vec<Line<'static>> {
    quiet_rows_limit(text, width, palette, usize::MAX)
}

/// Wrap a ` · `-separated fact list at fact boundaries, so each recorded fact
/// stays whole before ordinary word wrapping applies.
fn quiet_facts_limit(
    text: &str,
    width: usize,
    palette: Palette,
    limit: usize,
) -> Vec<Line<'static>> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    for part in text.split(" · ") {
        if current.is_empty() {
            current.push_str(part);
        } else if UnicodeWidthStr::width(current.as_str()) + 3 + UnicodeWidthStr::width(part)
            <= width
        {
            current.push_str(" · ");
            current.push_str(part);
        } else {
            chunks.push(std::mem::take(&mut current));
            current.push_str("  ");
            current.push_str(part);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    let mut rows = Vec::new();
    for chunk in chunks {
        let remaining = limit.saturating_sub(rows.len());
        if remaining == 0 {
            break;
        }
        rows.extend(wrap_words_limit(&chunk, width, remaining));
    }
    rows.into_iter()
        .map(|row| Line::styled(row, palette.quiet()))
        .collect()
}

fn quiet_facts(text: &str, width: usize, palette: Palette) -> Vec<Line<'static>> {
    quiet_facts_limit(text, width, palette, usize::MAX)
}

fn percent_text(value: u64, total: u64) -> String {
    if total == 0 {
        return ABSENT.into();
    }
    let pct = value.saturating_mul(100) / total;
    if value > 0 && pct == 0 {
        "<1%".into()
    } else {
        format!("{pct}%")
    }
}

fn has_categories(c: &TokenCategories) -> bool {
    [
        c.input,
        c.cached_input,
        c.cache_write,
        c.cache_write_5m,
        c.cache_write_1h,
        c.cache_read,
        c.output,
        c.reasoning,
    ]
    .into_iter()
    .flatten()
    .any(|value| value > 0)
}

fn tokens_panel(
    usage: Option<&UsageSummary>,
    hydrated: bool,
    width: usize,
    palette: Palette,
) -> Panel {
    let mut rows = Vec::new();
    if !hydrated {
        rows.push(loading_line(width, palette));
        return Panel::new("TOKENS", rows);
    }
    let Some(usage) = usage else {
        rows.push(note("Usage not recorded.", width, palette));
        return Panel::new("TOKENS", rows);
    };
    let total = match (usage.total, usage.class) {
        (Some(n), AggregationClass::Partial) => format!("{}+", grouped(n)),
        (Some(n), _) => grouped(n),
        (None, _) => ABSENT.into(),
    };
    rows.push(styled_row(
        vec![
            ("Total: ".into(), palette.quiet()),
            (
                total,
                if usage.total.is_some() {
                    palette.value()
                } else {
                    palette.quiet()
                },
            ),
            (format!(" · {}", class_name(usage.class)), palette.quiet()),
        ],
        width,
        palette,
    ));
    if has_categories(&usage.categories) {
        rows.push(token_bar(&usage.categories, width, palette));
    }
    let c = &usage.categories;
    for (label, value) in [
        ("Input", c.input),
        ("Cached input", c.cached_input),
        ("Cache write", c.cache_write),
        ("  Cache write 5m", c.cache_write_5m),
        ("  Cache write 1h", c.cache_write_1h),
        ("Cache read", c.cache_read),
        ("Output", c.output),
        ("Reasoning", c.reasoning),
    ] {
        let shown = value.map_or_else(|| ABSENT.to_string(), grouped);
        let mut parts = vec![
            (format!("{label}: "), palette.quiet()),
            (
                shown,
                if value.is_some() {
                    palette.value()
                } else {
                    palette.quiet()
                },
            ),
        ];
        if let (Some(value), Some(total)) = (value, usage.total)
            && total > 0
        {
            parts.push((format!("  {}", percent_text(value, total)), palette.quiet()));
        }
        rows.push(styled_row(parts, width, palette));
    }
    Panel::new("TOKENS", rows)
}

fn estimate_basis(cost: &CostEstimate) -> Option<&'static str> {
    match cost.source {
        Some(MoneySource::UserOverride) => Some("your price override"),
        Some(MoneySource::SavedCatalogEstimate) => Some("an estimate saved by the source"),
        Some(MoneySource::BundledReference) if cost.reference => Some(
            "the bundled official price catalog, applied as a reference estimate because no \
             billing route was recorded",
        ),
        Some(MoneySource::BundledReference) => Some("the bundled official price catalog"),
        _ => None,
    }
}

/// The one plain sentence that says why some observed requests have no price.
fn unpriced_sentence(usage: &UsageSummary) -> Option<String> {
    let cov = &usage.coverage;
    if cov.observed_requests == 0 || cov.priced_requests >= cov.observed_requests {
        return None;
    }
    if let Some(reason) = cov.reason.as_deref() {
        let reason = display_safe(reason);
        let reason = reason.trim().trim_end_matches('.');
        if !reason.is_empty() {
            return Some(format!("Unpriced because {reason}."));
        }
    }
    let missing = missing_legs(usage);
    if !missing.is_empty() {
        return Some(format!(
            "Unpriced because no price was recorded for: {}.",
            missing.join(", ")
        ));
    }
    Some("Unpriced because the source recorded no reason.".into())
}

/// A tiny proportional bar for priced-request coverage.
fn priced_bar(priced: usize, observed: usize, cells: usize) -> String {
    if observed == 0 {
        return String::new();
    }
    let cells = cells.max(4);
    let filled = priced.saturating_mul(cells).div_ceil(observed).min(cells);
    let mut bar = "█".repeat(filled);
    bar.push_str(&"░".repeat(cells - filled));
    bar
}

fn server_tool_rows(s: &SessionSummary, width: usize, palette: Palette) -> Vec<Line<'static>> {
    let Some(tools) = s.server_tools else {
        return Vec::new();
    };
    let mut rows = quiet_rows(
        &format!(
            "Server tools: web_search {} · web_fetch {}",
            tools.web_search, tools.web_fetch
        ),
        width,
        palette,
    );
    if tools.web_search > 0 || tools.web_fetch > 0 {
        rows.extend(quiet_rows(
            "Server tool calls are not included in the API estimate.",
            width,
            palette,
        ));
    }
    rows
}

fn billing_provider_row(s: &SessionSummary, width: usize, palette: Palette) -> Line<'static> {
    labeled(
        "Billing provider",
        billing_provider(&s.meta.model),
        width,
        palette,
    )
}

fn cost_panel(
    s: &SessionSummary,
    usage: Option<&UsageSummary>,
    hydrated: bool,
    width: usize,
    palette: Palette,
) -> Panel {
    let mut rows = Vec::new();
    if !hydrated {
        rows.push(loading_line(width, palette));
        return Panel::new("COST", rows);
    }
    let Some(usage) = usage else {
        rows.push(note(
            "Usage not recorded; no cost estimate.",
            width,
            palette,
        ));
        rows.extend(server_tool_rows(s, width, palette));
        rows.push(billing_provider_row(s, width, palette));
        return Panel::new("COST", rows);
    };
    let fact = cost_fact(Some(usage), true);
    if fact.head == ABSENT {
        rows.push(note("Cost: not recorded.", width, palette));
    } else {
        let mut parts = vec![
            ("Cost: ".into(), palette.quiet()),
            (fact.head.clone(), palette.value()),
        ];
        if !fact.sub.is_empty() {
            parts.push((" · ".into(), palette.quiet()));
            parts.push((fact.sub.clone(), palette.quiet()));
        }
        rows.push(styled_row(parts, width, palette));
    }
    if usage.cost.usd.is_none() && usage.cost.known_subtotal > 0.0 {
        rows.extend(quiet_rows(
            &format!(
                "Known subtotal: {} (incomplete, not a total)",
                money_text(usage.cost.known_subtotal)
            ),
            width,
            palette,
        ));
        let missing = missing_legs(usage);
        if !missing.is_empty() {
            rows.extend(quiet_rows(
                &format!("Unpriced coverage: {}", missing.join(", ")),
                width,
                palette,
            ));
        }
    }
    rows.extend(quiet_facts(
        &format!("Cost by category: {}", legs_text(&usage.cost.breakdown)),
        width,
        palette,
    ));
    let as_of = usage
        .cost
        .as_of
        .as_deref()
        .map_or_else(|| "not recorded".into(), one_line);
    rows.push(note(format!("Pricing as of: {as_of}"), width, palette));
    if usage.cost.class == AggregationClass::Estimated {
        rows.extend(quiet_rows(
            &format!(
                "Estimate basis: {}",
                estimate_basis(&usage.cost).unwrap_or("not recorded")
            ),
            width,
            palette,
        ));
    }
    if let Some(note_text) = &usage.cost.note {
        rows.extend(quiet_rows(
            &format!("Cost note: {}", display_safe(note_text)),
            width,
            palette,
        ));
    }
    if let Some(reported) = usage.reported_cost_usd
        && usage.cost.class != AggregationClass::Reported
    {
        rows.extend(quiet_rows(
            &format!(
                "Reported by source: {} (not an API-equivalent amount)",
                money_text(reported)
            ),
            width,
            palette,
        ));
    }
    let cov = &usage.coverage;
    if cov.observed_requests > 0 {
        let incomplete = cov.priced_requests < cov.observed_requests;
        let label = if incomplete {
            format!(
                "{} of {} priced",
                cov.priced_requests, cov.observed_requests
            )
        } else {
            format!("all {} priced", cov.observed_requests)
        };
        let extra = if incomplete { " · incomplete" } else { "" };
        let prefix = "Priced requests: ";
        let required = UnicodeWidthStr::width(prefix)
            + UnicodeWidthStr::width(label.as_str())
            + UnicodeWidthStr::width(extra)
            + 6;
        if required <= width {
            let bar = priced_bar(cov.priced_requests, cov.observed_requests, 10);
            rows.push(styled_row(
                vec![
                    (prefix.into(), palette.quiet()),
                    (bar, palette.value()),
                    (format!("  {label}{extra}"), palette.value()),
                ],
                width,
                palette,
            ));
        } else {
            let bar = priced_bar(cov.priced_requests, cov.observed_requests, 4);
            rows.push(styled_row(
                vec![(prefix.into(), palette.quiet()), (bar, palette.value())],
                width,
                palette,
            ));
            rows.extend(quiet_rows(&format!("  {label}{extra}"), width, palette));
        }
    } else {
        rows.push(note(
            "Priced requests: no requests observed",
            width,
            palette,
        ));
    }
    if let Some(sentence) = unpriced_sentence(usage) {
        rows.extend(quiet_rows(&sentence, width, palette));
    }
    rows.extend(server_tool_rows(s, width, palette));
    rows.push(billing_provider_row(s, width, palette));
    Panel::new("COST", rows)
}

fn time_speed_panel(
    s: &SessionSummary,
    hydrated: bool,
    now: Timestamp,
    width: usize,
    palette: Palette,
) -> Panel {
    let mut rows = Vec::new();
    if !hydrated {
        rows.push(loading_line(width, palette));
        return Panel::new("TIME & SPEED", rows);
    }
    rows.push(labeled("Model", model_label(&s.meta.model), width, palette));
    let effort = s
        .meta
        .model
        .as_ref()
        .and_then(|m| m.effort.as_deref())
        .map_or_else(|| "Unknown".into(), one_line);
    rows.push(labeled("Effort", effort, width, palette));
    if let Some(variant) = s.meta.model.as_ref().and_then(|m| m.variant.as_deref()) {
        rows.push(labeled("Variant", one_line(variant), width, palette));
    }
    let t = &s.metrics.timing;
    rows.push(labeled("Started", when(t.started_at), width, palette));
    if t.ended_at.is_some() {
        rows.push(labeled("Ended", when(t.ended_at), width, palette));
    }
    rows.push(labeled(
        "Elapsed",
        t.elapsed_ms.map_or_else(|| "Unknown".into(), span_text),
        width,
        palette,
    ));
    let active = t.active_ms.map_or_else(|| "Unknown".into(), span_text);
    let mut active_parts = vec![
        ("Active: ".into(), palette.quiet()),
        (
            active,
            if t.active_ms.is_some() {
                palette.value()
            } else {
                palette.quiet()
            },
        ),
    ];
    let active_note = coverage_note(t.active_complete);
    if !active_note.is_empty() {
        active_parts.push((active_note.into(), palette.quiet()));
    }
    rows.push(styled_row(active_parts, width, palette));
    rows.push(labeled(
        "Model time",
        measured_span(t.model_ms),
        width,
        palette,
    ));

    let speed = speed_fact(s, hydrated, now);
    if speed.head == ABSENT {
        rows.push(labeled("Latest speed", "Unknown", width, palette));
    } else {
        rows.push(styled_row(
            vec![
                ("Latest speed: ".into(), palette.quiet()),
                (format!("{} tok/s", speed.head), palette.value()),
            ],
            width,
            palette,
        ));
        if !speed.sub.is_empty() {
            rows.push(note(format!("  {}", speed.sub), width, palette));
        }
        if let Some(latest) = &s.metrics.speed.latest {
            rows.extend(quiet_rows(
                &format!(
                    "  {} output in {} · TTFT {}",
                    grouped(latest.output_tokens),
                    measured_span(Some(latest.duration_ms)),
                    measured_span(latest.ttft_ms)
                ),
                width,
                palette,
            ));
        }
    }
    let sp = &s.metrics.speed;
    if sp.measured_requests == 0 {
        rows.push(note(
            "Weighted speed: Unknown — no measured requests",
            width,
            palette,
        ));
    } else {
        rows.extend(quiet_rows(
            &format!(
                "Weighted speed: {} — {} output over {} across {} of {} requests{}",
                rate_text(sp.weighted_tokens_per_second),
                grouped(sp.measured_output),
                measured_span(Some(sp.measured_duration_ms)),
                sp.measured_requests,
                sp.total_requests,
                coverage_note(sp.complete),
            ),
            width,
            palette,
        ));
    }
    Panel::new("TIME & SPEED", rows)
}

fn context_panel(
    s: &SessionSummary,
    page: Option<&PageView<'_>>,
    width: usize,
    palette: Palette,
    limit: usize,
) -> Panel {
    let mut rows = Vec::new();
    let mut budget = RowBudget::new(limit);
    let c = &s.metrics.context;
    let used = c.used.map_or_else(|| "Unknown".into(), grouped);
    let window = c.capacity.map_or_else(
        || "Unknown window".into(),
        |w| format!("{} window", tokens_text(Some(w))),
    );
    let mut suffix = Vec::new();
    if c.class != AggregationClass::Unknown {
        suffix.push(class_name(c.class).to_string());
    }
    if c.recorded_at.is_some() {
        suffix.push(format!("recorded {}", when(c.recorded_at)));
    }
    if let Some(epoch) = c.compaction_epoch.as_deref() {
        suffix.push(format!("epoch {}", one_line(epoch)));
    }
    budget.push_lazy(&mut rows, || {
        styled_row(
            vec![
                ("Context: ".into(), palette.quiet()),
                (
                    format!("{used} of {window}"),
                    if c.used.is_some() || c.capacity.is_some() {
                        palette.value()
                    } else {
                        palette.quiet()
                    },
                ),
            ],
            width,
            palette,
        )
    });
    if !suffix.is_empty() {
        let lines = quiet_rows_limit(
            &format!("  {}", suffix.join(" · ")),
            width,
            palette,
            budget.remaining(),
        );
        budget.extend(&mut rows, lines);
    }
    budget.push_lazy(&mut rows, || {
        labeled("Context use", context_use(c), width, palette)
    });
    budget.push_lazy(&mut rows, || {
        labeled("Context model", model_label(&c.model), width, palette)
    });
    let lines = quiet_facts_limit(
        &compactions_line(&s.metrics.compactions),
        width,
        palette,
        budget.remaining(),
    );
    budget.extend(&mut rows, lines);
    if let Some(latest) = &s.metrics.compactions.latest {
        let lines = quiet_rows_limit(
            &format!("Latest compaction: {}", when(latest.timestamp)),
            width,
            palette,
            budget.remaining(),
        );
        budget.extend(&mut rows, lines);
        budget.push_lazy(&mut rows, || {
            let method = latest
                .method
                .as_deref()
                .map_or_else(|| "Unknown".into(), one_line);
            let trigger = latest
                .trigger
                .as_deref()
                .map_or_else(|| "Unknown".into(), one_line);
            note(
                format!("  method {method} · trigger {trigger}"),
                width,
                palette,
            )
        });
        let lines = quiet_rows_limit(
            &format!("  {}", compaction_reduction(latest)),
            width,
            palette,
            budget.remaining(),
        );
        budget.extend(&mut rows, lines);
    }
    if let Some(view) = page {
        let state = view.state;
        let page_data = state.page();
        if state.loading().is_some() {
            let lines = quiet_rows_limit(
                &format!(
                    "Loading older records… ({} records read)",
                    state.progress().records
                ),
                width,
                palette,
                budget.remaining(),
            );
            budget.extend(&mut rows, lines);
        } else if state.failed() {
            let lines = quiet_rows_limit(
                "Older records could not be read; the facts above stay as recorded.",
                width,
                palette,
                budget.remaining(),
            );
            budget.extend(&mut rows, lines);
        }
        if !page_data.compactions.is_empty() {
            budget.push_lazy(&mut rows, || {
                note(
                    format!("Loaded compaction records: {}", page_data.compactions.len()),
                    width,
                    palette,
                )
            });
        }
        for (index, c) in page_data.compactions.iter().enumerate() {
            if budget.remaining() == 0 {
                break;
            }
            budget.push_lazy(&mut rows, || {
                note(format!("Compaction {}", index + 1), width, palette)
            });
            if budget.remaining() == 0 {
                break;
            }
            let lines = quiet_rows_limit(
                &format!("  {}", compaction_reduction(c)),
                width,
                palette,
                budget.remaining(),
            );
            budget.extend(&mut rows, lines);
            if budget.remaining() == 0 {
                break;
            }
            let lines = quiet_rows_limit(
                &format!("  Event cost: {}", compaction_event_cost(c)),
                width,
                palette,
                budget.remaining(),
            );
            budget.extend(&mut rows, lines);
            if budget.remaining() == 0 {
                break;
            }
            budget.push_lazy(&mut rows, || {
                let method = c
                    .method
                    .as_deref()
                    .map_or_else(|| "Unknown".into(), one_line);
                let trigger = c
                    .trigger
                    .as_deref()
                    .map_or_else(|| "Unknown".into(), one_line);
                note(
                    format!("  method {method} · trigger {trigger}"),
                    width,
                    palette,
                )
            });
            if let Some(summary) = &c.summary {
                if view.expanded {
                    let lines = quiet_rows_limit(
                        &format!("  Summary: {}", display_safe(&summary.preview)),
                        width,
                        palette,
                        budget.remaining(),
                    );
                    budget.extend(&mut rows, lines);
                } else if budget.remaining() > 0 {
                    let safe = super::widgets::one_line(&summary.preview);
                    let mut short: String = safe.chars().take(COLLAPSED_SUMMARY_CHARS).collect();
                    if safe.chars().count() > COLLAPSED_SUMMARY_CHARS {
                        short.push('…');
                    }
                    budget.push_lazy(&mut rows, || {
                        Line::styled(fit(&format!("  Summary: {short}"), width), palette.quiet())
                    });
                    budget.push_lazy(&mut rows, || note("  (Space expands)", width, palette));
                }
            }
        }
        if page_data.has_more {
            let lines = quiet_rows_limit(
                "older records exist (m loads the next page)",
                width,
                palette,
                budget.remaining(),
            );
            budget.extend(&mut rows, lines);
        }
        if page_data.compactions.is_empty() && state.loading().is_none() && !state.failed() {
            budget.push_lazy(&mut rows, || {
                note("No compaction records loaded.", width, palette)
            });
        }
    }
    Panel::new("CONTEXT", rows)
}

fn family_panel(
    s: &SessionSummary,
    parent: &Option<SessionLink>,
    fork: &Option<SessionLink>,
    children: Option<&crate::app::ChildList>,
    width: usize,
    palette: Palette,
    limit: usize,
) -> Panel {
    let mut rows = Vec::new();
    let mut budget = RowBudget::new(limit);
    budget.push_lazy(&mut rows, || {
        labeled("Parent", link_text(parent), width, palette)
    });
    budget.push_lazy(&mut rows, || {
        labeled("Fork", link_text(fork), width, palette)
    });
    budget.push_lazy(&mut rows, || {
        labeled(
            "Agent",
            s.meta
                .lineage
                .agent_type
                .as_deref()
                .map_or_else(|| "Unknown".into(), one_line),
            width,
            palette,
        )
    });
    budget.push_lazy(&mut rows, || {
        labeled(
            "Sidechain",
            if s.meta.lineage.sidechain {
                "yes"
            } else {
                "no"
            },
            width,
            palette,
        )
    });
    if !s.meta.origins.is_empty() {
        budget.push_lazy(&mut rows, || {
            labeled("Origin", origin_label(&s.meta), width, palette)
        });
    }
    match children {
        None => budget.push_lazy(&mut rows, || note("Children: not loaded", width, palette)),
        Some(list) if list.metadata.is_empty() => {
            budget.push_lazy(&mut rows, || note("Children: none", width, palette));
        }
        Some(list) => {
            budget.push_lazy(&mut rows, || note("Children:", width, palette));
            let count = list.metadata.len();
            for (index, child) in list.metadata.iter().enumerate() {
                if budget.remaining() == 0 {
                    break;
                }
                let branch = if index + 1 == count { "└" } else { "├" };
                budget.push_lazy(&mut rows, || {
                    note(
                        format!(
                            "  {branch} {} {}",
                            harness_name(child.key.harness),
                            super::widgets::display_title(child)
                        ),
                        width,
                        palette,
                    )
                });
            }
            if list.next.is_some() {
                budget.push_lazy(&mut rows, || {
                    note("  └ more children not loaded", width, palette)
                });
            }
        }
    }
    Panel::new("FAMILY", rows)
}

fn process_panel(s: &SessionSummary, width: usize, palette: Palette) -> Panel {
    let mut rows = vec![labeled("Process", process_line(&s.process), width, palette)];
    if let ProcessAssociation::Matched {
        started_at: Some(at),
        ..
    } = s.process
    {
        rows.push(labeled("Process start", when(Some(at)), width, palette));
    }
    Panel::new("PROCESS", rows)
}

/// Fields the panels expose but the source did not record, in a fixed order.
fn not_recorded(s: &SessionSummary) -> Vec<&'static str> {
    let mut out = Vec::new();
    if s.metrics.timing.ended_at.is_none() {
        out.push("end time");
    }
    if s.meta.origins.is_empty() {
        out.push("origin");
    }
    if s.meta
        .model
        .as_ref()
        .and_then(|m| m.variant.as_deref())
        .is_none()
    {
        out.push("model variant");
    }
    if s.server_tools.is_none() {
        out.push("server tools");
    }
    if s.metrics.compactions.known_cost_usd.is_none() {
        out.push("compaction cost");
    }
    if s.usage
        .as_ref()
        .and_then(|u| u.categories.reasoning)
        .is_none()
    {
        out.push("reasoning tokens");
    }
    if s.metrics.context.capacity.is_none() {
        out.push("context window");
    }
    if !matches!(
        s.process,
        ProcessAssociation::Matched {
            started_at: Some(_),
            ..
        }
    ) {
        out.push("process start");
    }
    out
}

fn model_usage_label(model: &crate::domain::ModelUsageSummary) -> String {
    match model.model.provider.as_deref() {
        Some(provider) => format!("{}/{}", one_line(provider), one_line(&model.model.id)),
        None => one_line(&model.model.id),
    }
}

fn priced_models_line(usage: Option<&UsageSummary>) -> String {
    let Some(usage) = usage else {
        return "Priced models: none recorded".into();
    };
    if usage.models.is_empty() {
        return "Priced models: none recorded".into();
    }
    let labels: Vec<String> = usage.models.iter().take(3).map(model_usage_label).collect();
    let mut out = format!("Priced models: {}", labels.join(", "));
    if usage.models.len() > 3 {
        out.push_str(&format!(" +{}", usage.models.len() - 3));
    }
    out
}

/// The quiet closing lines, after a blank separator row.
fn bottom_lines(
    s: &SessionSummary,
    hydrated: bool,
    width: usize,
    palette: Palette,
    limit: usize,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut budget = RowBudget::new(limit);
    budget.push_lazy(&mut rows, || Line::raw(""));
    if !hydrated {
        let lines = quiet_rows_limit(
            "Facts are still loading; absent values are unknown until the read completes.",
            width,
            palette,
            budget.remaining(),
        );
        budget.extend(&mut rows, lines);
        return rows;
    }
    let missing = not_recorded(s);
    if !missing.is_empty() {
        let lines = quiet_rows_limit(
            &format!("Not recorded: {}", missing.join(", ")),
            width,
            palette,
            budget.remaining(),
        );
        budget.extend(&mut rows, lines);
    }
    let lines = quiet_rows_limit(
        &priced_models_line(s.usage.as_ref()),
        width,
        palette,
        budget.remaining(),
    );
    budget.extend(&mut rows, lines);
    rows
}

fn draw_panel(buf: &mut Buffer, rect: Rect, panel: &Panel, palette: Palette) {
    let block = Block::bordered()
        .border_style(palette.dim())
        .title(Line::styled(panel.title, palette.section()));
    let inner = block.inner(rect);
    block.render(rect, buf);
    Paragraph::new(panel.rows.clone()).render(inner, buf);
}

/// The dashboard content laid out in one or two columns, then copied into the
/// visible body so `j/k` scrolls the same bounded rows as every other overlay.
fn render_dashboard(
    frame: &mut Frame,
    app: &App,
    s: &SessionSummary,
    hydrated: bool,
    page: Option<&PageView<'_>>,
    body: Rect,
    palette: Palette,
) {
    let width = body.width as usize;
    let width_u16 = body.width;
    let two_columns = width >= TWO_COLUMN_MIN;
    let left_w = if two_columns {
        (width.saturating_sub(COLUMN_GAP) / 2) as u16
    } else {
        width_u16
    };
    let right_w = width_u16.saturating_sub(left_w + COLUMN_GAP as u16);
    let left_inner = left_w.saturating_sub(2) as usize;
    let right_inner = right_w.saturating_sub(2) as usize;

    let mut placements: Vec<(Rect, Panel)> = Vec::new();
    let mut y = 0u16;
    let mut budget = RowBudget::new(
        MAX_DASHBOARD_ROWS
            .saturating_sub(PANEL_COUNT * 2)
            .saturating_sub(1)
            .saturating_sub(BOTTOM_RESERVE),
    );
    if two_columns {
        let right_x = left_w + COLUMN_GAP as u16;
        let mut tokens = tokens_panel(s.usage.as_ref(), hydrated, left_inner, palette);
        budget.consume(&mut tokens.rows);
        let mut cost = cost_panel(s, s.usage.as_ref(), hydrated, right_inner, palette);
        budget.consume(&mut cost.rows);
        let mut time = time_speed_panel(s, hydrated, app.now(), left_inner, palette);
        budget.consume(&mut time.rows);
        let limit = budget.remaining();
        let mut family = family_panel(
            s,
            &app.resolved_parent(&s.meta.key),
            &app.resolved_fork(&s.meta.key),
            app.children_of(&s.meta.key),
            left_inner,
            palette,
            limit,
        );
        budget.consume(&mut family.rows);
        let mut process = process_panel(s, right_inner, palette);
        budget.consume(&mut process.rows);
        let limit = budget.remaining();
        let mut context = context_panel(s, page, right_inner, palette, limit);
        budget.consume(&mut context.rows);
        let pairs = [(tokens, cost), (time, context), (family, process)];
        for (left, right) in pairs {
            let left_h = left.height();
            let right_h = right.height();
            placements.push((Rect::new(0, y, left_w, left_h), left));
            placements.push((Rect::new(right_x, y, right_w, right_h), right));
            y = y.saturating_add(left_h.max(right_h));
        }
    } else {
        let mut tokens = tokens_panel(s.usage.as_ref(), hydrated, left_inner, palette);
        budget.consume(&mut tokens.rows);
        let mut cost = cost_panel(s, s.usage.as_ref(), hydrated, left_inner, palette);
        budget.consume(&mut cost.rows);
        let mut time = time_speed_panel(s, hydrated, app.now(), left_inner, palette);
        budget.consume(&mut time.rows);
        let limit = budget.remaining();
        let mut family = family_panel(
            s,
            &app.resolved_parent(&s.meta.key),
            &app.resolved_fork(&s.meta.key),
            app.children_of(&s.meta.key),
            left_inner,
            palette,
            limit,
        );
        budget.consume(&mut family.rows);
        let mut process = process_panel(s, left_inner, palette);
        budget.consume(&mut process.rows);
        let limit = budget.remaining();
        let mut context = context_panel(s, page, left_inner, palette, limit);
        budget.consume(&mut context.rows);
        let panels = [tokens, cost, time, context, family, process];
        for panel in panels {
            let height = panel.height();
            placements.push((Rect::new(0, y, left_w, height), panel));
            y = y.saturating_add(height);
        }
    }
    let bottom = bottom_lines(s, hydrated, width, palette, BOTTOM_RESERVE);
    let bottom_start = y.saturating_add(1);
    let total = (bottom_start as usize)
        .saturating_add(bottom.len())
        .min(MAX_DASHBOARD_ROWS);
    let total_u16 = u16::try_from(total).unwrap_or(u16::MAX);
    let mut buf = Buffer::empty(Rect::new(0, 0, width_u16, total_u16));
    for (rect, panel) in &placements {
        draw_panel(&mut buf, *rect, panel, palette);
    }
    if total_u16 > bottom_start {
        let bottom_height = total_u16 - bottom_start;
        let shown = bottom.len().min(bottom_height as usize);
        Paragraph::new(bottom.into_iter().take(shown).collect::<Vec<_>>()).render(
            Rect::new(0, bottom_start, width_u16, shown as u16),
            &mut buf,
        );
    }

    let visible = body.height as usize;
    let max_scroll = total.saturating_sub(visible);
    app.set_overlay_extent(max_scroll);
    let start = app.session_scroll().min(max_scroll);
    let frame_buf = frame.buffer_mut();
    for row in 0..visible {
        let source_row = start.saturating_add(row);
        if source_row >= total {
            break;
        }
        for x in 0..width {
            let Some(cell) = buf.cell((x as u16, source_row as u16)) else {
                continue;
            };
            if let Some(target) = frame_buf.cell_mut((
                body.x.saturating_add(x as u16),
                body.y.saturating_add(row as u16),
            )) {
                *target = cell.clone();
            }
        }
    }
}

pub fn render_session(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let area = frame.area();
    let palette = Palette::new(ctx.no_color);
    if area.width < super::roster::MIN_WIDTH || area.height < super::roster::MIN_HEIGHT {
        let msg = format!(
            "Terminal too small: {}x{} (need at least {}x{}). Resize to continue; Esc goes back.",
            area.width,
            area.height,
            super::roster::MIN_WIDTH,
            super::roster::MIN_HEIGHT
        );
        frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return;
    }
    let Some(transcript) = app.transcript() else {
        frame.render_widget(Paragraph::new("No session is open."), area);
        return;
    };
    let Some(row) = app.session(transcript.key()) else {
        frame.render_widget(Paragraph::new("Session is no longer in the roster."), area);
        return;
    };
    let key = &row.session.meta.key;
    let [head, rule1, body, rule2, status, footer] = Layout::vertical([
        Constraint::Length(HEADER_LINES),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let width = area.width as usize;

    frame.render_widget(
        Paragraph::new(super::inspector::header_lines(
            app, transcript, width, palette,
        )),
        head,
    );
    let rule = "─".repeat(width);
    frame.render_widget(Paragraph::new(rule.as_str()), rule1);
    frame.render_widget(Paragraph::new(rule.as_str()), rule2);

    let page = (app.details().key() == Some(key)).then(|| PageView {
        state: app.details(),
        expanded: app.summaries_expanded(),
    });
    render_dashboard(
        frame,
        app,
        row.session,
        row.hydrated,
        page.as_ref(),
        body,
        palette,
    );

    let mut spans = Vec::new();
    let mut used = 0usize;
    if let Some(notice) = app.notice() {
        let text = format!("{}  ·  ", one_line(notice));
        used = UnicodeWidthStr::width(text.as_str());
        spans.push(Span::styled(text, palette.quiet()));
    }
    spans.push(Span::styled(
        fit(STATUS, width.saturating_sub(used)),
        palette.quiet(),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), status);
    frame.render_widget(
        Paragraph::new(Line::styled(fit(FOOTER, width), palette.dim())),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Arc;
    use std::time::Duration;

    use ratatui::text::Line;

    use crate::app::App;
    use crate::domain::{
        ActivityStamp, Harness, Lineage, ReleaseState, SessionKey, SessionMetadata, SessionSummary,
        SourceKey, Thresholds,
    };
    use crate::worker::WorkerRequest;

    use super::{Palette, RowBudget, family_panel};

    fn metadata(key: SessionKey, title: &str) -> SessionMetadata {
        SessionMetadata {
            key,
            title: title.into(),
            title_derived: false,
            project: None,
            cwd: None,
            model: None,
            origins: Vec::new(),
            created_at: None,
            activity: ActivityStamp::default(),
            lineage: Lineage::default(),
            members: Vec::new(),
            archived: false,
            released: ReleaseState::Unknown,
            source_available: true,
            harness_version: None,
        }
    }

    fn child(i: usize) -> SessionKey {
        SessionKey {
            harness: Harness::Codex,
            native_id: format!("child-{i}"),
            source: SourceKey::File(format!("child-{i}.jsonl").into()),
        }
    }

    #[test]
    fn family_panel_stops_building_rows_at_its_allowance() {
        let parent = SessionKey {
            harness: Harness::Codex,
            native_id: "parent".into(),
            source: SourceKey::File("parent.jsonl".into()),
        };
        let summary = SessionSummary::unhydrated(metadata(parent.clone(), "Parent"));
        let mut app = App::new(
            0,
            Thresholds {
                quiet: Duration::from_secs(300),
                stall: Duration::from_secs(900),
            },
        );
        app.apply_roster(vec![summary.clone()], vec![]);
        app.take_requests();
        assert!(app.load_children(&parent));
        let request_id = app
            .take_requests()
            .iter()
            .find_map(|request| match request {
                WorkerRequest::LoadChildren { request_id, .. } => Some(*request_id),
                _ => None,
            })
            .expect("children request");
        let children: Vec<_> = (0..200)
            .map(|i| Arc::new(metadata(child(i), &format!("Child {i}"))))
            .collect();
        assert!(app.apply_children(request_id, &parent, children, Some(child(200))));
        let list = app.children_of(&parent).expect("children loaded");

        let panel = family_panel(
            &summary,
            &None,
            &None,
            Some(list),
            80,
            Palette::new(true),
            10,
        );
        assert!(
            panel.rows.len() <= 10,
            "built {} rows past the allowance",
            panel.rows.len()
        );
        let panel = family_panel(
            &summary,
            &None,
            &None,
            Some(list),
            80,
            Palette::new(true),
            0,
        );
        assert!(panel.rows.is_empty());
    }

    #[test]
    fn row_budget_builds_only_rows_with_an_allowance() {
        let built = Cell::new(0);
        let mut rows = Vec::new();
        let mut budget = RowBudget::new(0);
        budget.push_lazy(&mut rows, || {
            built.set(built.get() + 1);
            Line::raw("discarded")
        });
        assert_eq!(built.get(), 0, "no row is built without an allowance");
        assert!(rows.is_empty());

        let mut budget = RowBudget::new(1);
        budget.push_lazy(&mut rows, || {
            built.set(built.get() + 1);
            Line::raw("kept")
        });
        budget.push_lazy(&mut rows, || {
            built.set(built.get() + 1);
            Line::raw("discarded")
        });
        assert_eq!(built.get(), 1, "only the allowed row is constructed");
        assert_eq!(rows.len(), 1);
    }
}
