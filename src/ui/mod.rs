//! Terminal UI: roster rendering, terminal lifecycle guard, and the event loop.
//!
//! The UI thread only polls terminal input and applies worker events; every
//! file/database read happens in the worker thread.

mod details;
mod inspector;
mod panels;
mod preview;
mod roster;
mod session;
pub mod widgets;

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::thread::ThreadId;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;

use crate::app::{App, Panel, Screen};
use crate::domain::Timestamp;
use crate::worker::{WorkerEvent, WorkerHandle};

pub use inspector::{REASONING_LABEL, render_inspector};
pub use preview::render_preview;
pub use roster::{
    Column, ColumnBox, MIN_HEIGHT, MIN_WIDTH, PREVIEW_WIDTH, RosterLayout, handle_mouse,
    render_roster,
};
/// Compact value facts (`head`, `sub`, `quiet`) shared by the roster, preview
/// and details views.
pub mod facts {
    pub use super::session::{Fact, active_fact, cost_fact, speed_fact, tokens_fact};
}

/// Draw whichever screen is active, then any panel on top of it.
pub fn render(frame: &mut ratatui::Frame, app: &App, ctx: &RosterContext) {
    match app.screen {
        Screen::Roster => render_roster(frame, app, ctx),
        Screen::Inspector => render_inspector(frame, app, ctx),
        Screen::Details => details::render_details(frame, app, ctx),
        Screen::Source => details::render_source(frame, app, ctx),
        Screen::Diagnostics => details::render_diagnostics(frame, app, ctx),
        Screen::Session => session::render_session(frame, app, ctx),
    }
    if app.panel() != Panel::None {
        panels::render_panel(frame, app, ctx);
    }
}

/// Presentation inputs that are not part of `App` state.
#[derive(Clone, Debug, Default)]
pub struct RosterContext {
    pub no_color: bool,
    /// Provider roots that were checked, shown in the empty state.
    pub roots: Vec<String>,
    /// Startup problems such as a provider that could not be opened.
    pub notes: Vec<String>,
    /// Local time of the last completed refresh.
    pub last_refresh: Option<String>,
    /// Set when the worker stopped; the UI shows `Live: paused`.
    pub error: Option<String>,
}

const TICK: Duration = Duration::from_millis(100);
/// Most queued key presses handled before the next redraw.
const MAX_KEYS_PER_FRAME: usize = 64;

type Term = Terminal<CrosstermBackend<Stdout>>;

/// Mouse capture was enabled by the live guard and still has to be disabled.
static MOUSE_CAPTURE: AtomicBool = AtomicBool::new(false);

/// The terminal mode changes a session makes, so the order of entering, rolling
/// back and leaving can be exercised without a real terminal.
trait ScreenModes {
    fn enter_alternate_screen(&mut self) -> io::Result<()>;
    fn leave_alternate_screen(&mut self) -> io::Result<()>;
    fn set_mouse_capture(&mut self, on: bool) -> io::Result<()>;
}

/// The process's real standard output.
struct StdoutModes;

impl ScreenModes for StdoutModes {
    fn enter_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), EnterAlternateScreen)
    }

    fn leave_alternate_screen(&mut self) -> io::Result<()> {
        execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show)
    }

    fn set_mouse_capture(&mut self, on: bool) -> io::Result<()> {
        if on {
            execute!(io::stdout(), EnableMouseCapture)
        } else {
            execute!(io::stdout(), DisableMouseCapture)
        }
    }
}

/// Best effort and idempotent: every step is attempted.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let mouse = MOUSE_CAPTURE.swap(false, Ordering::SeqCst);
    leave_screen(&mut StdoutModes, mouse);
}

/// Alternate screen, then (only when asked) mouse capture.
fn enter_screen(modes: &mut impl ScreenModes, mouse: bool) -> io::Result<()> {
    modes.enter_alternate_screen()?;
    if mouse {
        modes.set_mouse_capture(true)?;
    }
    Ok(())
}

/// Undo [`enter_screen`]; every step is attempted. Capture is released before
/// the screen so the shell never sees stray mouse reports.
fn leave_screen(modes: &mut impl ScreenModes, mouse: bool) {
    if mouse {
        let _ = modes.set_mouse_capture(false);
    }
    let _ = modes.leave_alternate_screen();
}

/// Enter the screen; a failure part-way leaves it again before returning.
fn setup_screen(modes: &mut impl ScreenModes, mouse: bool) -> io::Result<()> {
    enter_screen(modes, mouse).inspect_err(|_| leave_screen(modes, mouse))
}

/// Owns raw mode, the alternate screen and (optionally) mouse capture.
/// Dropping restores all of them, and a panic hook restores them before the
/// panic message prints.
pub struct TerminalGuard {
    terminal: Term,
}

impl TerminalGuard {
    /// Enter raw mode and the alternate screen, with mouse capture when
    /// `mouse_enabled`. The caller must have checked that stdin and stdout are
    /// terminals. Any failure restores what was already changed.
    pub fn enter(mouse_enabled: bool) -> io::Result<Self> {
        install_panic_hook(std::thread::current().id(), restore_terminal);
        enable_raw_mode()?;
        MOUSE_CAPTURE.store(mouse_enabled, Ordering::SeqCst);
        let setup = setup_screen(&mut StdoutModes, mouse_enabled).and_then(|()| {
            let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
            terminal.hide_cursor()?;
            Ok(terminal)
        });
        match setup {
            Ok(terminal) => Ok(Self { terminal }),
            Err(error) => {
                restore_terminal();
                Err(error)
            }
        }
    }

    pub fn terminal(&mut self) -> &mut Term {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn now_millis() -> Timestamp {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as Timestamp)
}

fn clock_label() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Outcome of handling one key.
#[derive(Debug, PartialEq, Eq)]
pub enum KeyAction {
    None,
    Quit,
    Refresh,
}

/// Apply one key press to the app. Pure state change: no I/O.
///
/// Text input comes first: while a search is being edited every printable key,
/// `q` included, is text, and only `Ctrl+c` still quits.
pub fn handle_key(app: &mut App, key: KeyEvent) -> KeyAction {
    if key.kind != KeyEventKind::Press {
        return KeyAction::None;
    }
    // Windows reports AltGr as Ctrl+Alt; that is a character, not a chord.
    let chord =
        key.modifiers.contains(KeyModifiers::CONTROL) && !key.modifiers.contains(KeyModifiers::ALT);
    let editing = app.is_editing();
    if chord {
        return match key.code {
            KeyCode::Char('c') => KeyAction::Quit,
            KeyCode::Char('u') if editing => {
                app.search_clear();
                KeyAction::None
            }
            _ if editing => KeyAction::None,
            KeyCode::Char('u') => {
                page_move(app, -10);
                KeyAction::None
            }
            KeyCode::Char('d') => {
                page_move(app, 10);
                KeyAction::None
            }
            _ => KeyAction::None,
        };
    }
    if editing {
        return handle_search_key(app, key.code);
    }
    if app.panel() != Panel::None {
        return handle_panel_key(app, key.code);
    }
    match key.code {
        KeyCode::Char('?') => {
            app.open_help();
            return KeyAction::None;
        }
        KeyCode::Char('p')
            if matches!(
                app.screen,
                Screen::Roster | Screen::Inspector | Screen::Details
            ) =>
        {
            app.open_processes();
            return KeyAction::None;
        }
        KeyCode::Tab => {
            app.focus_next();
            return KeyAction::None;
        }
        KeyCode::BackTab => {
            app.focus_prev();
            return KeyAction::None;
        }
        _ => {}
    }
    if app.screen == Screen::Roster {
        handle_roster_key(app, key.code)
    } else {
        handle_inspector_key(app, key.code)
    }
}

/// Keys while a narrow terminal shows the preview instead of the list. `None`
/// leaves the key to the ordinary roster handler.
fn handle_preview_key(app: &mut App, code: KeyCode) -> Option<KeyAction> {
    match code {
        KeyCode::Char('q') => return Some(KeyAction::Quit),
        KeyCode::Char('j') | KeyCode::Down => app.scroll_preview(1),
        KeyCode::Char('k') | KeyCode::Up => app.scroll_preview(-1),
        KeyCode::PageDown => app.scroll_preview(10),
        KeyCode::PageUp => app.scroll_preview(-10),
        KeyCode::Char('g') | KeyCode::Home => app.scroll_preview(isize::MIN),
        KeyCode::Char('G') | KeyCode::End => app.scroll_preview(isize::MAX),
        KeyCode::Enter => {
            app.open_inspector();
        }
        KeyCode::Esc => app.focus_prev(),
        _ => return None,
    }
    Some(KeyAction::None)
}

fn handle_roster_key(app: &mut App, code: KeyCode) -> KeyAction {
    if app.preview_replaces_list()
        && let Some(action) = handle_preview_key(app, code)
    {
        return action;
    }
    match code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.move_focus(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_focus(-1),
        KeyCode::Left | KeyCode::Char('h') => {
            app.fold_selected(false);
        }
        KeyCode::Right | KeyCode::Char('l') => {
            app.fold_selected(true);
        }
        KeyCode::Char('J') => app.scroll_preview(1),
        KeyCode::Char('K') => app.scroll_preview(-1),
        KeyCode::PageDown => app.move_selection(10),
        KeyCode::PageUp => app.move_selection(-10),
        KeyCode::Char('g') | KeyCode::Home => app.move_selection(isize::MIN),
        KeyCode::Char('G') | KeyCode::End => app.move_selection(isize::MAX),
        KeyCode::Enter => {
            if !app.activate_load_focus() {
                app.open_inspector();
            }
        }
        KeyCode::Char('/') => {
            app.begin_search();
        }
        KeyCode::Char('f') => {
            app.open_filter();
        }
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('S') => app.sort(app.sort_mode()),
        KeyCode::Char('r') => return KeyAction::Refresh,
        KeyCode::Char('m') => {
            app.load_more_roots();
        }
        KeyCode::Char(' ') if app.load_focus().is_none() => {
            if let Some(key) = app.selected_key() {
                app.select_session(&key);
            }
            app.toggle_children();
        }
        _ => {}
    }
    KeyAction::None
}

/// Keys while a search is being edited: everything printable is text.
fn handle_search_key(app: &mut App, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Esc => app.search_cancel(),
        KeyCode::Enter => app.search_commit(),
        KeyCode::Backspace => app.search_backspace(),
        KeyCode::Char(c) => app.search_push(c),
        _ => {}
    }
    KeyAction::None
}

/// Keys inside the help, process, and filter panels. `q` is not a text key in
/// the filter panel, so there it does nothing rather than quit.
fn handle_panel_key(app: &mut App, code: KeyCode) -> KeyAction {
    let panel = app.panel();
    match code {
        KeyCode::Char('q') if panel == Panel::Filter => {}
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Esc | KeyCode::Backspace => app.close_panel(),
        KeyCode::Char('?') if panel == Panel::Help => app.close_panel(),
        KeyCode::Char('p') if panel == Panel::Processes => app.close_panel(),
        KeyCode::Char('f') if panel == Panel::Filter => app.close_panel(),
        KeyCode::Enter | KeyCode::Char(' ') if panel == Panel::Filter => app.panel_activate(),
        KeyCode::Char('c') if panel == Panel::Filter => app.clear_filters(),
        KeyCode::Char('j') | KeyCode::Down => app.panel_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.panel_move(-1),
        KeyCode::PageDown => app.panel_move(10),
        KeyCode::PageUp => app.panel_move(-10),
        KeyCode::Char('g') | KeyCode::Home => app.panel_move(isize::MIN),
        KeyCode::Char('G') | KeyCode::End => app.panel_move(isize::MAX),
        _ => {}
    }
    KeyAction::None
}

fn page_move(app: &mut App, delta: isize) {
    if app.panel() != Panel::None {
        return app.panel_move(delta);
    }
    match app.screen {
        Screen::Details | Screen::Source | Screen::Session => app.overlay_scroll(delta),
        Screen::Diagnostics => app.diagnostics_move(delta),
        Screen::Inspector => app.inspector_move(delta),
        Screen::Roster => app.move_selection(delta),
    }
}

fn handle_inspector_key(app: &mut App, code: KeyCode) -> KeyAction {
    match app.screen {
        Screen::Details | Screen::Source | Screen::Session => {
            return handle_scroll_overlay_key(app, code);
        }
        Screen::Diagnostics => return handle_diagnostics_key(app, code),
        _ => {}
    }
    match code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.inspector_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.inspector_move(-1),
        KeyCode::PageDown => app.inspector_move(10),
        KeyCode::PageUp => app.inspector_move(-10),
        KeyCode::Char('g') | KeyCode::Home => app.inspector_first(),
        KeyCode::Char('G') | KeyCode::End => app.inspector_last(),
        KeyCode::Enter => app.inspector_toggle(),
        KeyCode::Char('/') => {
            app.begin_search();
        }
        KeyCode::Char('n') => app.search_step(true),
        KeyCode::Char('N') => app.search_step(false),
        KeyCode::Char('d') => {
            app.open_details();
        }
        KeyCode::Char('v') => {
            app.open_selected_source();
        }
        KeyCode::Char('D') => {
            app.open_diagnostics();
        }
        KeyCode::Char('i') => {
            app.open_session_info();
        }
        KeyCode::Char('x') => app.toggle_redaction(),
        KeyCode::Esc | KeyCode::Backspace => app.close_inspector(),
        KeyCode::Char('r') => {
            app.retry_transcript();
            return KeyAction::Refresh;
        }
        _ => {}
    }
    KeyAction::None
}

/// Details and source overlays: scroll, and the source/redaction/diagnostics keys.
fn handle_scroll_overlay_key(app: &mut App, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.overlay_scroll(1),
        KeyCode::Char('k') | KeyCode::Up => app.overlay_scroll(-1),
        KeyCode::PageDown => app.overlay_scroll(10),
        KeyCode::PageUp => app.overlay_scroll(-10),
        KeyCode::Home => app.overlay_scroll(isize::MIN),
        KeyCode::End => app.overlay_scroll(isize::MAX),
        KeyCode::Char('x') => app.toggle_redaction(),
        KeyCode::Char('m') if app.screen == Screen::Session => {
            app.load_more_details();
        }
        KeyCode::Char(' ') | KeyCode::Enter if app.screen == Screen::Session => {
            app.toggle_summaries();
        }
        KeyCode::Char('v') if app.screen == Screen::Details => {
            app.open_selected_source();
        }
        KeyCode::Char('o') if app.screen == Screen::Details => {
            app.open_tool_child_session();
        }
        KeyCode::Char('D') if app.screen == Screen::Details => {
            app.open_diagnostics();
        }
        KeyCode::Esc | KeyCode::Backspace => {
            app.close_overlay();
        }
        _ => {}
    }
    KeyAction::None
}

fn handle_diagnostics_key(app: &mut App, code: KeyCode) -> KeyAction {
    match code {
        KeyCode::Char('q') => return KeyAction::Quit,
        KeyCode::Char('j') | KeyCode::Down => app.diagnostics_move(1),
        KeyCode::Char('k') | KeyCode::Up => app.diagnostics_move(-1),
        KeyCode::PageDown => app.diagnostics_move(10),
        KeyCode::PageUp => app.diagnostics_move(-10),
        KeyCode::Home => app.diagnostics_move(isize::MIN),
        KeyCode::End => app.diagnostics_move(isize::MAX),
        KeyCode::Enter | KeyCode::Char('v') => {
            app.open_diagnostic_source();
        }
        KeyCode::Char('x') => app.toggle_redaction(),
        KeyCode::Esc | KeyCode::Backspace => {
            app.close_overlay();
        }
        _ => {}
    }
    KeyAction::None
}

/// Send queued inspector requests to the worker. Never blocks: the request
/// channel is unbounded, and a stopped worker simply drops them.
pub fn flush_requests(app: &mut App, worker: &WorkerHandle) {
    for request in app.take_requests() {
        let _ = worker.tx.send(request);
    }
}

/// Apply one worker event to the app and presentation context.
pub fn apply_worker_event(app: &mut App, ctx: &mut RosterContext, event: WorkerEvent) {
    match event {
        WorkerEvent::Roster { snapshot, warnings } => {
            app.apply_roster_snapshot(snapshot, warnings);
        }
        WorkerEvent::Children {
            request_id,
            parent,
            metadata,
            next,
        } => {
            app.apply_children(request_id, &parent, metadata, next);
        }
        // Cached and completed preview facts, paged details, and slice progress.
        WorkerEvent::Preview {
            request_id,
            key,
            summary,
            done,
        } => {
            app.apply_preview(request_id, key, summary, done);
        }
        WorkerEvent::Details {
            request_id,
            key,
            page,
        } => {
            app.apply_details(request_id, key, page);
        }
        WorkerEvent::LoadProgress {
            request_id,
            records,
            expanded_bytes,
        } => {
            app.apply_load_progress(
                request_id,
                crate::providers::WorkProgress {
                    records,
                    expanded_bytes,
                },
            );
        }
        WorkerEvent::Warning { warning } => app.apply_warning(warning),
        WorkerEvent::LoadFailed { request_id } => app.load_failed(request_id),
        WorkerEvent::EventsAfter { request_id, count } => {
            app.apply_events_after(request_id, count);
        }
        WorkerEvent::RefreshFinished { .. } => ctx.last_refresh = Some(clock_label()),
        WorkerEvent::Transcript { request_id, page } => {
            app.apply_transcript(request_id, page);
        }
        WorkerEvent::TranscriptInvalidated { key } => app.invalidate_transcript(&key),
        WorkerEvent::Source { request_id, record } => {
            app.apply_source(request_id, record);
        }
        WorkerEvent::Processes { snapshot } => app.apply_processes(snapshot),
    }
}

/// Apply every queued worker event. Returns `false` when the worker is gone.
pub fn drain_worker(app: &mut App, ctx: &mut RosterContext, worker: &WorkerHandle) -> bool {
    loop {
        match worker.rx.try_recv() {
            Ok(event) => apply_worker_event(app, ctx, event),
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

/// The worker thread is gone: show `Live: paused` until `r` replaces it.
pub fn worker_stopped(app: &mut App, ctx: &mut RosterContext) {
    ctx.error = Some("worker thread exited".into());
    app.live = false;
}

/// A replacement worker plus the channel on which its (worker-thread) reader
/// construction reports startup problems.
pub struct Respawned {
    pub worker: WorkerHandle,
    pub notes: Receiver<Vec<String>>,
}

/// Apply one terminal event: a key, a mouse event hit-tested against the
/// geometry of `area` as it is now, or a resize. Pure state change: no I/O.
pub fn handle_event(app: &mut App, event: Event, area: Rect) -> KeyAction {
    match event {
        Event::Key(key) => handle_key(app, key),
        Event::Mouse(mouse) => {
            // Pointer motion carries no action; skip building the geometry for it.
            if mouse.kind != MouseEventKind::Moved {
                let layout = RosterLayout::for_area(area);
                handle_mouse(app, mouse, &layout);
            }
            KeyAction::None
        }
        Event::Resize(width, _) => {
            app.set_viewport_width(width);
            KeyAction::None
        }
        _ => KeyAction::None,
    }
}

/// Run the roster until the user quits. `start` is the first worker, whose readers
/// are built on its own thread; `respawn` restarts a stopped worker when the user
/// presses `r`; it must not block on provider I/O. `mouse_enabled` turns mouse
/// capture on; keyboard operation never depends on it.
pub fn run(
    app: &mut App,
    ctx: &mut RosterContext,
    start: Respawned,
    respawn: &dyn Fn() -> Respawned,
    mouse_enabled: bool,
) -> io::Result<()> {
    let mut guard = TerminalGuard::enter(mouse_enabled)?;
    let Respawned { mut worker, notes } = start;
    let mut notes_rx: Option<Receiver<Vec<String>>> = Some(notes);
    let mut last_clock = now_millis();
    app.set_now(last_clock);
    loop {
        if let Some(rx) = &notes_rx
            && let Ok(notes) = rx.try_recv()
        {
            ctx.notes = notes;
        }
        if ctx.error.is_none() && !drain_worker(app, ctx, &worker) {
            worker_stopped(app, ctx);
        }
        flush_requests(app, &worker);
        let now = now_millis();
        if now - last_clock >= 1000 {
            last_clock = now;
            app.set_now(now);
        }
        let size = guard.terminal().size()?;
        app.set_viewport_width(size.width);
        guard.terminal().draw(|frame| render(frame, app, ctx))?;

        if !event::poll(TICK)? {
            continue;
        }
        // Handle this event and any already queued, then redraw once.
        let mut quit = false;
        let mut handled = 0;
        loop {
            let size = guard.terminal().size()?;
            let area = Rect::new(0, 0, size.width, size.height);
            match handle_event(app, event::read()?, area) {
                KeyAction::Quit => {
                    quit = true;
                    break;
                }
                KeyAction::Refresh if ctx.error.is_some() => {
                    let replacement = respawn();
                    worker = replacement.worker;
                    notes_rx = Some(replacement.notes);
                    reset_for_new_worker(app, ctx);
                }
                KeyAction::Refresh => {
                    app.refresh_roster();
                }
                KeyAction::None => {}
            }
            handled += 1;
            if handled >= MAX_KEYS_PER_FRAME || !event::poll(Duration::ZERO)? {
                break;
            }
        }
        if quit {
            break;
        }
        flush_requests(app, &worker);
    }
    drop(guard);
    worker.shutdown();
    Ok(())
}

/// State that belongs to the previous worker: its generation numbering, its
/// startup notes, its error, and its refresh time.
pub fn reset_for_new_worker(app: &mut App, ctx: &mut RosterContext) {
    app.reset_generation();
    app.restart_transcript();
    app.live = true;
    ctx.error = None;
    ctx.notes.clear();
    ctx.last_refresh = None;
}

/// Install a panic hook that restores the terminal only for panics on `owner`
/// (the UI thread), then forwards to the previous hook. Panics on other
/// threads, such as caught worker panics, leave the TUI intact.
fn install_panic_hook(owner: ThreadId, restore: fn()) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().id() == owner {
            restore();
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static RESTORES: AtomicUsize = AtomicUsize::new(0);

    fn count() {
        RESTORES.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn panic_hook_restores_only_on_the_owner_thread() {
        let original = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        install_panic_hook(std::thread::current().id(), count);

        let other = std::thread::spawn(|| catch_unwind(|| panic!("worker")).is_err());
        assert!(other.join().unwrap());
        assert_eq!(RESTORES.load(Ordering::SeqCst), 0);

        assert!(catch_unwind(AssertUnwindSafe(|| panic!("ui"))).is_err());
        assert_eq!(RESTORES.load(Ordering::SeqCst), 1);

        std::panic::set_hook(original);
    }

    /// Records the mode changes in order and can fail one step.
    #[derive(Default)]
    struct Recorder {
        log: Vec<&'static str>,
        fail: Option<&'static str>,
    }

    impl Recorder {
        fn step(&mut self, name: &'static str) -> io::Result<()> {
            self.log.push(name);
            if self.fail == Some(name) {
                Err(io::Error::other("injected"))
            } else {
                Ok(())
            }
        }
    }

    impl ScreenModes for Recorder {
        fn enter_alternate_screen(&mut self) -> io::Result<()> {
            self.step("screen on")
        }

        fn leave_alternate_screen(&mut self) -> io::Result<()> {
            self.step("screen off")
        }

        fn set_mouse_capture(&mut self, on: bool) -> io::Result<()> {
            self.step(if on { "capture on" } else { "capture off" })
        }
    }

    #[test]
    fn mouse_capture_is_entered_only_when_enabled() {
        let mut on = Recorder::default();
        setup_screen(&mut on, true).unwrap();
        assert_eq!(on.log, ["screen on", "capture on"]);

        let mut off = Recorder::default();
        setup_screen(&mut off, false).unwrap();
        assert_eq!(off.log, ["screen on"], "--no-mouse never captures");
        leave_screen(&mut off, false);
        assert_eq!(
            off.log,
            ["screen on", "screen off"],
            "and never releases what it did not take"
        );
    }

    #[test]
    fn leaving_releases_capture_first_and_attempts_every_step() {
        let mut ok = Recorder::default();
        leave_screen(&mut ok, true);
        assert_eq!(ok.log, ["capture off", "screen off"]);

        let mut broken = Recorder {
            fail: Some("capture off"),
            ..Recorder::default()
        };
        leave_screen(&mut broken, true);
        assert_eq!(
            broken.log,
            ["capture off", "screen off"],
            "a failed release still leaves the alternate screen"
        );
    }

    #[test]
    fn a_failed_setup_rolls_back_whatever_was_entered_and_reports_the_error() {
        let mut capture_fails = Recorder {
            fail: Some("capture on"),
            ..Recorder::default()
        };
        let error = setup_screen(&mut capture_fails, true).unwrap_err();
        assert_eq!(error.to_string(), "injected");
        assert_eq!(
            capture_fails.log,
            ["screen on", "capture on", "capture off", "screen off"]
        );

        let mut screen_fails = Recorder {
            fail: Some("screen on"),
            ..Recorder::default()
        };
        assert!(setup_screen(&mut screen_fails, true).is_err());
        assert_eq!(screen_fails.log, ["screen on", "capture off", "screen off"]);

        let mut no_mouse = Recorder {
            fail: Some("screen on"),
            ..Recorder::default()
        };
        assert!(setup_screen(&mut no_mouse, false).is_err());
        assert_eq!(no_mouse.log, ["screen on", "screen off"]);
    }
}
