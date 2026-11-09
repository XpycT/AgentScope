//! Borrowed roster rows: project group headings, loaded roots, their loaded
//! children, and the explicit Load more rows between them (spec §10.2).
//!
//! Rows borrow the facts already in `App`; nothing is cloned or formatted here,
//! so a frame only pays for the rows it actually asks for. Group order and
//! totals are computed once per rebuild and only borrowed here.

use std::cell::Cell;
use std::fmt;

use crate::domain::{SessionKey, SessionSummary, Status};

use super::App;

/// Terminal width at and above which the preview sits beside the list.
pub const PREVIEW_SPLIT_WIDTH: u16 = 150;

/// Keyboard focus resting on an explicit Load more row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadFocus {
    /// The root Load more row.
    Roots,
    /// The Load more row of one family's children.
    Children(SessionKey),
}

impl LoadFocus {
    fn matches(&self, item: &RosterItem<'_>) -> bool {
        match (self, item) {
            (Self::Roots, RosterItem::RootLoadMore) => true,
            (Self::Children(key), RosterItem::ChildLoadMore { parent, .. }) => key == *parent,
            _ => false,
        }
    }
}

/// Responsive roster state that is not session data: the preview pane and its
/// independent scroll, and focus on a Load more row.
#[derive(Default)]
pub(super) struct RosterView {
    preview_scroll: usize,
    /// Largest preview scroll of the last rendered preview.
    preview_extent: Cell<usize>,
    /// Narrow terminals: the preview replaces the list until Tab swaps back.
    preview_shown: bool,
    /// Last known terminal width; 0 until the first render or resize.
    width: u16,
    load_focus: Option<LoadFocus>,
}

impl RosterView {
    pub(super) fn reset_preview_scroll(&mut self) {
        self.preview_scroll = 0;
    }

    pub(super) fn clear_load_focus(&mut self) {
        self.load_focus = None;
    }
}

enum Slot {
    Session(SessionKey),
    Load(LoadFocus),
}

/// Totals of the loaded members of one project group. A value stays `None`
/// until a member reports one, so a group with no recorded usage reads as
/// unknown, never as zero.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct GroupTotals {
    /// Loaded roots plus loaded children in the group.
    pub sessions: usize,
    pub tokens: Option<u64>,
    /// Some member reported no token total, or the group is incomplete.
    pub tokens_partial: bool,
    pub cost_usd: Option<f64>,
    /// Some member reported no cost or only a priced subtotal, or the group
    /// is incomplete.
    pub cost_partial: bool,
    /// Some summed cost amount is an estimate.
    pub cost_estimated: bool,
}

/// The one-line heading emitted before a project group's members.
#[derive(Clone, Copy)]
pub struct GroupHeader<'a> {
    pub project: Option<&'a str>,
    pub totals: GroupTotals,
}

/// The project name is provider-supplied text: report its presence and length
/// only, like [`crate::domain::SessionMetadata`].
impl fmt::Debug for GroupHeader<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupHeader")
            .field("project_len", &self.project.map(str::len))
            .field("totals", &self.totals)
            .finish()
    }
}

/// One project group over the visible roots. Rebuilt with the view and totals,
/// so the row iterator only borrows it.
#[derive(Clone, Default)]
pub(crate) struct GroupInfo {
    pub project: Option<String>,
    /// Indices into `App::view`, in view order.
    pub roots: Vec<usize>,
    pub totals: GroupTotals,
}

impl fmt::Debug for GroupInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GroupInfo")
            .field("project_len", &self.project.as_deref().map(str::len))
            .field("roots", &self.roots.len())
            .field("totals", &self.totals)
            .finish()
    }
}

/// One session row in the loaded tree.
#[derive(Clone, Debug)]
pub struct RosterSession<'a> {
    pub session: &'a SessionSummary,
    pub status: &'a Status,
    /// Detailed facts have been read. A metadata-only row reports Unknown, not
    /// zero, for facts that were not read.
    pub hydrated: bool,
    /// 0 for a loaded root, 1 for a child.
    pub depth: u8,
    pub selected: bool,
    pub expanded: bool,
    /// Number of children known so far (0 when none were loaded).
    pub child_count: usize,
}

/// A visual roster row.
#[derive(Clone, Debug)]
pub enum RosterItem<'a> {
    /// Project group heading; never selectable.
    GroupHeader(GroupHeader<'a>),
    Session(RosterSession<'a>),
    /// Reveal the next frozen root page.
    RootLoadMore,
    /// Load the next child page of `parent`.
    ChildLoadMore {
        parent: &'a SessionKey,
        depth: u8,
    },
}

impl RosterItem<'_> {
    /// Physical lines this row occupies.
    pub fn height(&self) -> u16 {
        match self {
            Self::GroupHeader(_) => 1,
            Self::Session(_) => 2,
            Self::RootLoadMore | Self::ChildLoadMore { .. } => 2,
        }
    }
}

/// Iterator over the loaded roster rows of [`App::roster_rows`].
pub struct RosterRows<'a> {
    app: &'a App,
    /// Next group in `App::groups`.
    group: usize,
    /// Next root within the current group.
    root_in_group: usize,
    /// The current group's heading has not been emitted yet.
    header_pending: bool,
    /// Root whose children are being emitted, and the next child index.
    children: Option<(usize, &'a SessionKey, usize)>,
    /// Filter/search active when this traversal began.
    filtering: bool,
    /// The (single) trailing root Load more row.
    root_more: bool,
    finished: bool,
}

impl<'a> RosterRows<'a> {
    pub(super) fn new(app: &'a App) -> Self {
        Self {
            app,
            group: 0,
            root_in_group: 0,
            header_pending: !app.groups.is_empty(),
            children: None,
            filtering: app.filter_active(),
            root_more: false,
            finished: false,
        }
    }

    fn child_row(&self, root: usize, i: usize) -> Option<RosterSession<'a>> {
        let session = &self.app.sessions[root];
        let list = self.app.children.get(&session.meta.key)?;
        let metadata = list.metadata.get(i)?;
        let status = list.statuses.get(i)?;
        let summary = self.app.child_summaries.get(&metadata.key)?;
        Some(RosterSession {
            session: summary,
            status,
            hydrated: self.app.hydrated_child(&metadata.key),
            depth: 1,
            selected: self.app.selected.as_ref() == Some(&metadata.key),
            expanded: false,
            child_count: 0,
        })
    }
}

impl<'a> Iterator for RosterRows<'a> {
    type Item = RosterItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        // Continue an expanded/filtering parent's children before moving on.
        if let Some((root, parent, i)) = self.children {
            let list = self.app.children.get(parent);
            let mut at = i;
            while let Some(list) = list {
                if list.metadata.get(at).is_none() {
                    break;
                }
                if !self.app.child_matches(list, at) {
                    at += 1;
                    continue;
                }
                if let Some(row) = self.child_row(root, at) {
                    self.children = Some((root, parent, at + 1));
                    return Some(RosterItem::Session(row));
                }
                at += 1;
            }
            // The family's next page is an explicit row, never auto-loaded.
            let more = list.is_some_and(|l| l.next.is_some() && !self.filtering);
            self.children = None;
            if more {
                return Some(RosterItem::ChildLoadMore { parent, depth: 1 });
            }
        }
        // A group starts at its first root in view order and collects the other
        // roots with the same project, even non-adjacent ones.
        if self.header_pending {
            self.header_pending = false;
            if let Some(group) = self.app.groups.get(self.group) {
                return Some(RosterItem::GroupHeader(GroupHeader {
                    project: group.project.as_deref(),
                    totals: group.totals,
                }));
            }
        }
        let root = self
            .app
            .groups
            .get(self.group)
            .and_then(|group| group.roots.get(self.root_in_group))
            .and_then(|&position| self.app.view.get(position))
            .copied();
        if let Some(index) = root {
            self.root_in_group += 1;
            if self
                .app
                .groups
                .get(self.group)
                .is_none_or(|group| self.root_in_group >= group.roots.len())
            {
                self.group += 1;
                self.root_in_group = 0;
                self.header_pending = self.group < self.app.groups.len();
            }
            let session = &self.app.sessions[index];
            let key = &session.meta.key;
            let selected = self.app.selected.as_ref() == Some(key);
            let expanded = self.app.expanded_children.contains(key);
            let child_count = self.app.children.get(key).map_or(0, |l| l.metadata.len());
            if expanded || self.filtering {
                self.children = Some((index, key, 0));
            }
            return Some(RosterItem::Session(RosterSession {
                session,
                status: &self.app.statuses[index],
                hydrated: self.app.hydrated[index],
                depth: 0,
                selected,
                expanded,
                child_count,
            }));
        }
        if !self.root_more
            && self.app.roster.has_more
            && !self.app.roster.provisional
            && !self.filtering
        {
            self.root_more = true;
            return Some(RosterItem::RootLoadMore);
        }
        self.finished = true;
        None
    }
}

/// Keys of every row a user can move through, in visual order.
pub(super) fn session_keys(app: &App) -> Vec<SessionKey> {
    app.roster_rows()
        .filter_map(|item| match item {
            RosterItem::Session(row) => Some(row.session.meta.key.clone()),
            _ => None,
        })
        .collect()
}

impl App {
    /// Record the terminal width so Tab knows whether the preview shares the screen.
    pub fn set_viewport_width(&mut self, width: u16) {
        self.roster_view.width = width;
        if width >= PREVIEW_SPLIT_WIDTH {
            self.roster_view.preview_shown = false;
        }
    }

    /// The preview has its own column beside the list.
    pub fn preview_beside_list(&self) -> bool {
        self.roster_view.width >= PREVIEW_SPLIT_WIDTH
    }

    /// A narrow terminal currently shows the preview instead of the list.
    pub fn preview_replaces_list(&self) -> bool {
        let width = self.roster_view.width;
        width > 0 && width < PREVIEW_SPLIT_WIDTH && self.roster_view.preview_shown
    }

    /// `Tab` on a narrow roster swaps list and preview. Returns `false` when the
    /// width is unknown or wide enough to show both.
    pub(super) fn swap_roster_pane(&mut self) -> bool {
        let view = &mut self.roster_view;
        if view.width == 0 || view.width >= PREVIEW_SPLIT_WIDTH {
            return false;
        }
        view.preview_shown = !view.preview_shown;
        true
    }

    pub fn preview_scroll(&self) -> usize {
        self.roster_view.preview_scroll
    }

    /// Render reports how far the preview can scroll.
    pub fn set_preview_extent(&self, max_scroll: usize) {
        self.roster_view.preview_extent.set(max_scroll);
    }

    /// Scroll the preview independently of the list.
    pub fn scroll_preview(&mut self, delta: isize) {
        let view = &mut self.roster_view;
        let max = view.preview_extent.get();
        view.preview_scroll = view.preview_scroll.saturating_add_signed(delta).min(max);
    }

    /// The Load more row holding keyboard focus, when it is still shown.
    pub fn load_focus(&self) -> Option<&LoadFocus> {
        self.focused_load_row()?;
        self.roster_view.load_focus.as_ref()
    }

    /// Visual index of the focused Load more row.
    fn focused_load_row(&self) -> Option<usize> {
        let focus = self.roster_view.load_focus.as_ref()?;
        self.roster_rows().position(|item| focus.matches(&item))
    }

    /// Visual row that scrolling follows: the focused Load more row, else the selection.
    pub fn focus_visual_index(&self) -> Option<usize> {
        self.focused_load_row().or(self.visual_selected)
    }

    /// Move keyboard focus by `delta` rows. Unlike [`Self::move_selection`] this
    /// also rests on explicit Load more rows, so they stay reachable; it never
    /// loads anything by itself.
    pub fn move_focus(&mut self, delta: isize) {
        let slots: Vec<Slot> = self
            .roster_rows()
            .filter_map(|item| match item {
                RosterItem::Session(row) => Some(Slot::Session(row.session.meta.key.clone())),
                RosterItem::RootLoadMore => Some(Slot::Load(LoadFocus::Roots)),
                RosterItem::ChildLoadMore { parent, .. } => {
                    Some(Slot::Load(LoadFocus::Children(parent.clone())))
                }
                // A heading is never a keyboard stop.
                RosterItem::GroupHeader(_) => None,
            })
            .collect();
        if slots.is_empty() {
            self.selected = None;
            self.roster_view.load_focus = None;
            return;
        }
        let load = self.roster_view.load_focus.clone();
        let current = load
            .and_then(|focus| {
                slots
                    .iter()
                    .position(|slot| matches!(slot, Slot::Load(l) if *l == focus))
            })
            .or_else(|| {
                let key = self.selected.as_ref()?;
                slots
                    .iter()
                    .position(|slot| matches!(slot, Slot::Session(k) if k == key))
            })
            .unwrap_or(0) as isize;
        let next = current
            .saturating_add(delta)
            .clamp(0, slots.len() as isize - 1) as usize;
        match &slots[next] {
            Slot::Session(key) => {
                self.roster_view.load_focus = None;
                self.selected = Some(key.clone());
                self.selection_chosen = true;
                self.recompute_visual_selection();
                self.select_session(&key.clone());
            }
            Slot::Load(focus) => self.roster_view.load_focus = Some(focus.clone()),
        }
    }

    /// Put keyboard focus on a Load more row (mouse click).
    pub fn focus_load_more(&mut self, focus: LoadFocus) {
        self.roster_view.load_focus = Some(focus);
    }

    /// `Enter` on a focused Load more row asks for that page. Returns `false`
    /// when no shown Load more row holds focus.
    pub fn activate_load_focus(&mut self) -> bool {
        let Some(focus) = self.roster_view.load_focus.clone() else {
            return false;
        };
        if self.focused_load_row().is_none() {
            self.roster_view.load_focus = None;
            return false;
        }
        match focus {
            LoadFocus::Roots => {
                self.load_more_roots();
            }
            LoadFocus::Children(parent) => {
                self.load_more_children(&parent);
            }
        }
        true
    }

    /// Select a shown session row (mouse click). Load more rows are never sessions.
    pub fn select_row(&mut self, key: &SessionKey) -> bool {
        if !self
            .roster_rows()
            .any(|item| matches!(&item, RosterItem::Session(row) if &row.session.meta.key == key))
        {
            return false;
        }
        self.roster_view.load_focus = None;
        self.selected = Some(key.clone());
        self.selection_chosen = true;
        self.recompute_visual_selection();
        self.select_session(key);
        true
    }

    /// `→`/`l` expands the selected root, `←`/`h` collapses it. On a child,
    /// `←` selects its parent root and collapses nothing; on a header, a
    /// Load more row or no selection nothing happens. Returns whether the
    /// visible rows changed (or the selection moved).
    pub fn fold_selected(&mut self, expand: bool) -> bool {
        if self.roster_view.load_focus.is_some() {
            return false;
        }
        let Some(key) = self.selected.clone() else {
            return false;
        };
        if let Some(parent) = self.child_parent.get(&key).cloned() {
            if expand {
                return false; // No deeper levels are rendered today.
            }
            return self.select_row(&parent);
        }
        if !self.roster_rows().any(|item| {
            matches!(&item, RosterItem::Session(row)
                if row.depth == 0 && row.session.meta.key == key)
        }) {
            return false;
        }
        let before = self.roster_rows().count();
        if expand {
            if self.expanded_children.contains(&key) {
                return false;
            }
            self.expanded_children.insert(key.clone());
            if !self.children.contains_key(&key) {
                self.load_children(&key);
            }
        } else if !self.expanded_children.remove(&key) {
            return false;
        }
        let previous = self.selected_root_position();
        self.rebuild(previous);
        self.roster_rows().count() != before
    }
}
