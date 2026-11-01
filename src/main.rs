use agentscope::app::App;
use agentscope::config::{CliOverrides, Config};
use agentscope::domain::{LocatorKind, RefreshBudget, Thresholds};
use agentscope::ingest::source::{SourceError, SourceItem, SourceReader};
use agentscope::ingest::text::display_safe;
use agentscope::providers::ProviderReader;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::hermes::RetryingHermes;
use agentscope::providers::omp::OmpScanner;
use agentscope::providers::opencode::RetryingOpenCode;
use agentscope::providers::pi::PiScanner;
use agentscope::ui::{self, RosterContext};
use agentscope::worker::WorkerHandle;
use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_RECORD_BYTES: usize = 1024 * 1024;

/// One diagnostic inspection slice: a diagnostic must stay bounded on a huge archive
/// and simply take more steps.
const INSPECT_BUDGET: RefreshBudget = RefreshBudget {
    max_records: 256,
    max_bytes: 8 * 1024 * 1024,
    max_millis: 100,
};

const USAGE: &str =
    "Usage: agentscope [OPTIONS]\n       agentscope inspect <rollout.jsonl|rollout.jsonl.zst>";

const HELP: &str =
    "AgentScope: read-only monitor for local Codex, Claude Code, OpenCode, OMP, Hermes, and Pi sessions.

Usage:
  agentscope [OPTIONS]
  agentscope inspect <rollout.jsonl|rollout.jsonl.zst>
                            Print a Codex transcript or its archived copy (no TUI)

Options:
  --config <path>           Config file (default: platform config directory)
  --codex-sessions <dir>    Codex sessions root
  --claude-projects <dir>   Claude projects root
  --claude-desktop <dir>    Claude Desktop application-data directory
  --opencode-db <path>      OpenCode database
  --omp-sessions <dir>      OMP sessions root
  --pi-sessions <dir>       Pi sessions root (native JSONL; extension subagents unsupported)
  --hermes-db <path>        Hermes state.db
  --no-processes            Disable process observation
  --no-color                Disable color (NO_COLOR is also honored)
  --no-mouse                Disable mouse capture (keyboard still works; the terminal keeps native selection)
  --version                 Print version
  --help                    Print this help

Keys: j/k move, g/G first/last, Enter open (or load more), m next 50 roots, Space expand,
      Tab swap list/preview, Left/Right scroll columns, J/K scroll preview, / search loaded,
      n/N next/previous match, f filter, s sort (S reverse), r refresh, p process candidates,
      D diagnostics, v source, x redaction, ? help, q or Ctrl+c quit.
Mouse: click selects, the marker expands, Load more rows load, the wheel scrolls; hold Shift to
       select text with the terminal. Mouse input never changes agent sessions.";

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Version,
    Inspect(PathBuf),
    /// Boxed: the resolved overrides outgrew the other variants.
    Run(Box<CliOverrides>),
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command, String> {
    let mut args = args.into_iter();
    let mut cli = CliOverrides::default();
    let mut first = true;
    while let Some(arg) = args.next() {
        if first && arg == "inspect" {
            return match (args.next(), args.next()) {
                (Some(path), None) => Ok(Command::Inspect(PathBuf::from(path))),
                _ => Err("inspect takes exactly one file".into()),
            };
        }
        first = false;
        let text = arg.to_string_lossy().into_owned();
        let (name, inline) = match text.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(OsString::from(v))),
            _ => (text.clone(), None),
        };
        let value = |args: &mut dyn Iterator<Item = OsString>| {
            inline
                .clone()
                .or_else(|| args.next())
                .map(PathBuf::from)
                .ok_or_else(|| format!("{name} requires a value"))
        };
        match name.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--version" | "-V" => return Ok(Command::Version),
            "--config" => cli.config = Some(value(&mut args)?),
            "--codex-sessions" => cli.codex_sessions = Some(value(&mut args)?),
            "--claude-projects" => cli.claude_projects = Some(value(&mut args)?),
            "--claude-desktop" => cli.claude_desktop = Some(value(&mut args)?),
            "--opencode-db" => cli.opencode_db = Some(value(&mut args)?),
            "--omp-sessions" => cli.omp_sessions = Some(value(&mut args)?),
            "--pi-sessions" => cli.pi_sessions = Some(value(&mut args)?),
            "--hermes-db" => cli.hermes_db = Some(value(&mut args)?),
            "--no-processes" if inline.is_none() => cli.no_processes = true,
            "--no-color" if inline.is_none() => cli.no_color = true,
            "--no-mouse" if inline.is_none() => cli.no_mouse = true,
            _ => return Err(format!("unrecognized argument: {}", display_safe(&text))),
        }
    }
    Ok(Command::Run(Box::new(cli)))
}

fn main() -> ExitCode {
    match parse_args(std::env::args_os().skip(1)) {
        Ok(Command::Help) => {
            println!("{HELP}");
            ExitCode::SUCCESS
        }
        Ok(Command::Version) => {
            println!("agentscope {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Ok(Command::Inspect(path)) => match inspect(path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("Could not inspect source: {:?}", error.kind());
                ExitCode::FAILURE
            }
        },
        Ok(Command::Run(cli)) => run_tui(&cli),
        Err(message) => {
            eprintln!("agentscope: {message}\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn run_tui(cli: &CliOverrides) -> ExitCode {
    // Checked before any terminal mode change.
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!(
            "agentscope: stdin and stdout must be a terminal for the interactive view.\n\
             To read a transcript without the TUI, use: \
             agentscope inspect <rollout.jsonl|rollout.jsonl.zst>"
        );
        return ExitCode::from(2);
    }
    let config = match Config::load(cli) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("agentscope: {error}");
            return ExitCode::from(2);
        }
    };
    let no_color = config.no_color || std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let mut app = App::new(
        now,
        Thresholds {
            quiet: Duration::from_secs(config.quiet_after_secs),
            stall: Duration::from_secs(config.stall_after_secs),
        },
    );
    let mut ctx = RosterContext {
        no_color,
        roots: checked_roots(&config),
        notes: Vec::new(),
        last_refresh: None,
        error: None,
    };
    // Readers (including the SQLite open) are always built on the worker thread, for
    // the first worker and for every replacement.
    let spawn = || {
        let (notes_tx, notes) = std::sync::mpsc::channel();
        let factory_config = config.clone();
        let worker = WorkerHandle::spawn_lazy(
            Box::new(move || {
                let (readers, notes) = build_readers(&factory_config);
                let _ = notes_tx.send(notes);
                readers
            }),
            config.clone(),
        );
        ui::Respawned { worker, notes }
    };
    match ui::run(&mut app, &mut ctx, spawn(), &spawn, config.mouse_enabled) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("agentscope: terminal error: {:?}", error.kind());
            ExitCode::FAILURE
        }
    }
}

fn build_readers(config: &Config) -> (Vec<Box<dyn ProviderReader>>, Vec<String>) {
    let mut readers: Vec<Box<dyn ProviderReader>> = Vec::new();
    let mut notes = config.startup_notes();
    let mut add = |name: &str, result: Result<Box<dyn ProviderReader>, _>| match result {
        Ok(reader) => readers.push(reader),
        Err(error) => notes.push(format!("{name} unavailable: {error}")),
    };
    add(
        "Codex",
        CodexScanner::new(config).map(|r| Box::new(r) as Box<dyn ProviderReader>),
    );
    add(
        "Claude",
        ClaudeScanner::new(config).map(|r| Box::new(r) as Box<dyn ProviderReader>),
    );
    add(
        "OMP",
        OmpScanner::new(config).map(|r| Box::new(r) as Box<dyn ProviderReader>),
    );
    add(
        "Pi",
        PiScanner::new(config).map(|r| Box::new(r) as Box<dyn ProviderReader>),
    );
    // Stays registered when the first open fails; refreshes retry.
    let (opencode, opened) = RetryingOpenCode::open(config);
    match opened {
        None => readers.push(Box::new(opencode)),
        Some(error) if config.harnesses.opencode.db.is_some() => {
            notes.push(format!("OpenCode unavailable: {error}; retrying"));
            readers.push(Box::new(opencode));
        }
        Some(error) => notes.push(format!("OpenCode unavailable: {error}")),
    }
    // A configured Hermes database that cannot be opened is retried on every
    // refresh; an absent platform default stays silent.
    let (hermes, opened) = RetryingHermes::open(config);
    let configured = config
        .harnesses
        .hermes
        .db
        .as_ref()
        .is_some_and(|path| path.is_file());
    if let Some(error) = opened.filter(|_| configured) {
        notes.push(format!("Hermes unavailable: {error}; retrying"));
    }
    readers.push(Box::new(hermes));
    (readers, notes)
}

fn checked_roots(config: &Config) -> Vec<String> {
    let show = |name: &str, path: &Option<PathBuf>| match path {
        Some(path) => format!("{name}: {}", path.display()),
        None => format!("{name}: no location configured"),
    };
    vec![
        show("Codex", &config.harnesses.codex.root),
        show("Claude", &config.harnesses.claude.root),
        show("OpenCode", &config.harnesses.opencode.db),
        show("OMP", &config.harnesses.omp.root),
        show("Pi", &config.harnesses.pi.root),
        show(
            "Hermes",
            &config
                .harnesses
                .hermes
                .root
                .clone()
                .or_else(|| config.harnesses.hermes.db.clone()),
        ),
    ]
}

fn inspect(path: PathBuf) -> Result<(), SourceError> {
    let mut reader = SourceReader::open(&path, MAX_RECORD_BYTES)?;
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    loop {
        let batch = reader.step(INSPECT_BUDGET)?;
        for record in &batch.records {
            // A compressed ordinal names the record; a plain file's own byte offset is
            // meaningful, so each source keeps its own anchor.
            let (anchor, at) = match &record.locator {
                LocatorKind::FileOffset { offset, .. } => {
                    (offset.to_string(), format!("byte {offset}"))
                }
                LocatorKind::CompressedRecord { ordinal, .. } => {
                    (format!("#{ordinal}"), format!("record #{ordinal}"))
                }
                LocatorKind::DbRow { .. } => continue,
            };
            match &record.item {
                SourceItem::Json(value) => {
                    let payload = &value["payload"];
                    if value["type"] == "session_meta" {
                        if let Some(id) = payload["id"].as_str() {
                            writeln!(stdout, "Session: {}", display_safe(id))?;
                        }
                    } else if value["type"] == "response_item" && payload["type"] == "message" {
                        let role = match payload["role"].as_str() {
                            Some("user") => "User",
                            Some("assistant") => "Assistant",
                            _ => continue,
                        };
                        if let Some(parts) = payload["content"].as_array() {
                            for part in parts {
                                if (part["type"] == "input_text" || part["type"] == "output_text")
                                    && let Some(text) = part["text"].as_str()
                                {
                                    writeln!(stdout, "[{role} @ {anchor}] {}", display_safe(text))?;
                                }
                            }
                        }
                    }
                }
                SourceItem::Invalid => writeln!(stderr, "Invalid JSON record at {at}")?,
                SourceItem::TooLarge => writeln!(stderr, "Oversized record at {at}")?,
                SourceItem::Skipping => {}
                SourceItem::Incomplete => writeln!(stderr, "Incomplete record at {at}")?,
                SourceItem::Damaged(damage) => {
                    writeln!(stderr, "Damaged source at {at}: {damage}")?
                }
            }
        }
        if batch.done {
            return Ok(());
        }
    }
}
