//! Read-only process observation and identity-argument matching (spec §9.4).
//!
//! Only OS-exposed process metadata is read: PID, start time, executable, cwd
//! when the OS provides it, and the argument vector used for identity matching.
//! Environments and process memory are never read and no process is signalled.
//! Executable names, working directories and timing never produce a match; they
//! only make a process a *candidate*.

use std::fmt;
use std::path::PathBuf;

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use thiserror::Error;

use crate::domain::{Harness, ProcessAssociation, SessionSummary, Timestamp};
use crate::ingest::text;

const HINT_MAX_BYTES: usize = 160;
const HINT_MAX_COLS: usize = 120;
/// Runtimes that may host a harness CLI script (`node …/opencode`).
const RUNTIMES: [&str; 3] = ["node", "bun", "deno"];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProcessError {
    /// The OS did not expose a process list (unsupported platform or denied).
    #[error("process list unavailable")]
    Unavailable,
}

#[derive(Clone, Debug)]
pub struct ProcessSnapshot {
    pub sampled_at: Timestamp,
    pub processes: Vec<ProcessInfo>,
}

#[derive(Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub started_at: Option<Timestamp>,
    /// Display-safe executable path (or process name when the path is hidden).
    pub executable: String,
    /// `None` when the OS does not expose it (generally on Windows).
    pub cwd: Option<PathBuf>,
    /// Sanitized, redacted, truncated hint; never the raw command line.
    pub command_line: Option<String>,
    /// Raw arguments, used only for matching. Never displayed or logged.
    argv: Option<Vec<String>>,
}

impl ProcessInfo {
    pub fn new(
        pid: u32,
        started_at: Option<Timestamp>,
        executable: String,
        cwd: Option<PathBuf>,
        argv: Option<Vec<String>>,
    ) -> Self {
        let command_line = argv.as_deref().filter(|a| !a.is_empty()).map(hint);
        Self {
            pid,
            started_at,
            executable: text::display_safe(&executable),
            cwd,
            command_line,
            argv: argv.filter(|a| !a.is_empty()),
        }
    }
}

impl fmt::Debug for ProcessInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProcessInfo")
            .field("pid", &self.pid)
            .field("started_at", &self.started_at)
            .field("executable_len", &self.executable.len())
            .field("cwd_known", &self.cwd.is_some())
            .field("argc", &self.argv.as_ref().map(Vec::len))
            .finish()
    }
}

fn hint(argv: &[String]) -> String {
    let mut parts = Vec::with_capacity(argv.len());
    for (i, a) in argv.iter().enumerate() {
        // Executable path may embed a user name; show the basename only.
        parts.push(if i == 0 {
            basename(a).to_string()
        } else {
            a.clone()
        });
    }
    text::preview(&parts.join(" "), HINT_MAX_BYTES, HINT_MAX_COLS).preview
}

fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// Lower-cased basename without a launcher/script extension.
fn stem(path: &str) -> String {
    let base = basename(path).to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".bat", ".js", ".mjs", ".cjs"] {
        if let Some(s) = base.strip_suffix(ext) {
            return s.to_string();
        }
    }
    base
}

fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Codex => "codex",
        Harness::Claude => "claude",
        Harness::OpenCode => "opencode",
        Harness::Omp => "omp",
        Harness::Hermes => "hermes",
        Harness::Pi => "pi",
    }
}

/// Runtime options that consume the next token.
const RUNTIME_VALUE_OPTS: &[&str] = &[
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
    "--conditions",
    "-C",
    "--env-file",
    "--inspect-port",
    "--config",
    "-c",
];

/// Index of the script a runtime invocation executes, or `None` (eval mode,
/// no script). Only options before the script are skipped.
fn runtime_script(argv: &[&str]) -> Option<usize> {
    let mut i = 1;
    let mut first = true;
    while i < argv.len() {
        let a = argv[i];
        if matches!(a, "-e" | "--eval" | "-p" | "--print")
            || a.starts_with("--eval=")
            || a.starts_with("--print=")
            || (a.starts_with('-')
                && !a.starts_with("--")
                && matches!(a.as_bytes().get(1), Some(b'e' | b'p')))
        {
            return None;
        }
        if RUNTIME_VALUE_OPTS.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        if first && a == "run" && stem(argv[0]) != "node" {
            first = false;
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

/// Index of the harness program in `argv`: argv[0], or the script actually
/// executed by a script runtime (`node /path/opencode …`). Script arguments
/// are never treated as the program.
fn locate(harness: Harness, argv: &[&str]) -> Option<usize> {
    let name = harness_name(harness);
    if stem(argv.first()?) == name {
        return Some(0);
    }
    if RUNTIMES.contains(&stem(argv[0]).as_str()) {
        return runtime_script(argv).filter(|&i| stem(argv[i]) == name);
    }
    None
}

/// True when `argv` (including argv[0]) names the harness program and carries
/// `id` in an identity-bearing position. Prompt text and other subcommands never match.
pub fn identity_matches(harness: Harness, argv: &[&str], id: &str) -> bool {
    if id.is_empty() {
        return false;
    }
    let Some(at) = locate(harness, argv) else {
        return false;
    };
    let args = &argv[at + 1..];
    match harness {
        Harness::Codex => match args {
            ["resume", got, ..] => *got == id,
            ["exec", "resume", got, ..] => *got == id,
            _ => false,
        },
        Harness::Claude => flag_value(args, &["-r", "--resume"], "--resume=", "--fork-session", id),
        Harness::OpenCode => flag_value(args, &["-s", "--session"], "--session=", "--fork", id),
        // No supported resume/session argument is verified for these yet, so a
        // process can only ever be a candidate.
        Harness::Omp | Harness::Hermes | Harness::Pi => false,
    }
}

/// Scan flag arguments for `flag <id>` / `prefix<id>`. A fork flag makes the
/// id a parent reference, not the running session. `--` ends option parsing.
fn flag_value(args: &[&str], flags: &[&str], prefix: &str, fork: &str, id: &str) -> bool {
    let value_opts: &[&str] = if prefix == "--resume=" {
        CLAUDE_VALUE_OPTS
    } else {
        OPENCODE_VALUE_OPTS
    };
    let mut found = false;
    let mut printing = false;
    let mut i = 0;
    while i < args.len() {
        let a = args[i];
        if a == "--" {
            break;
        }
        if a == "-p" || a == "--print" {
            // Later tokens are prompt-shaped and cannot supply identity, but a
            // later fork flag still makes the id a parent reference.
            printing = true;
        } else if a == fork {
            return false;
        } else if flags.contains(&a) {
            if !printing && args.get(i + 1) == Some(&id) {
                found = true;
            }
            i += 1;
        } else if a.strip_prefix(prefix) == Some(id) {
            found |= !printing;
        } else if value_opts.contains(&a) {
            // The next token is this option's value, never an identity flag.
            i += 1;
        }
        i += 1;
    }
    found
}

const CLAUDE_VALUE_OPTS: &[&str] = &[
    "--system-prompt",
    "--system-prompt-file",
    "--append-system-prompt-file",
    "--effort",
    "--max-budget-usd",
    "--json-schema",
    "--agent",
    "--file",
    "--max-thinking-tokens",
    "--append-system-prompt",
    "--model",
    "--fallback-model",
    "--add-dir",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--tools",
    "--mcp-config",
    "--permission-mode",
    "--permission-prompt-tool",
    "--settings",
    "--setting-sources",
    "--session-id",
    "--output-format",
    "--input-format",
    "--max-turns",
    "--agents",
    "--plugin-dir",
    "--betas",
    "--name",
    "-n",
    "--debug-file",
];

const OPENCODE_VALUE_OPTS: &[&str] = &[
    "-m",
    "--model",
    "--agent",
    "--prompt",
    "--port",
    "--hostname",
    "--log-level",
    "--mode",
    "--title",
    "--format",
    "--variant",
    "--file",
    "-f",
];

fn argv_refs(p: &ProcessInfo) -> Option<Vec<&str>> {
    p.argv
        .as_ref()
        .map(|a| a.iter().map(String::as_str).collect())
}

fn is_candidate(harness: Harness, p: &ProcessInfo) -> bool {
    if stem(&p.executable) == harness_name(harness) {
        return true;
    }
    argv_refs(p).is_some_and(|a| {
        locate(harness, &a).is_some()
            // A runtime running some other script that mentions the harness is
            // only a candidate, never a strong match.
            || (RUNTIMES.contains(&stem(a[0]).as_str())
                && a[1..].iter().any(|x| stem(x) == harness_name(harness)))
    })
}

/// Processes that look like the harness's CLI. Candidates are not matches.
pub fn candidates(harness: Harness, snapshot: &ProcessSnapshot) -> Vec<&ProcessInfo> {
    snapshot
        .processes
        .iter()
        .filter(|p| is_candidate(harness, p))
        .collect()
}

/// Sample the OS process list. Environments and memory are never requested.
pub fn observe_processes(now: Timestamp) -> Result<ProcessSnapshot, ProcessError> {
    let mut kind = ProcessRefreshKind::nothing()
        .with_cmd(UpdateKind::Always)
        .with_exe(UpdateKind::Always)
        .with_user(UpdateKind::Always);
    if CWD_OBSERVED {
        kind = kind.with_cwd(UpdateKind::Always);
    }
    let mut sys = System::new().map_err(|_| ProcessError::Unavailable)?;
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    let own_user = sysinfo::get_current_pid()
        .ok()
        .and_then(|pid| sys.process(pid))
        .and_then(|p| p.user_id().cloned());
    let mut processes: Vec<ProcessInfo> = sys
        .processes()
        .values()
        .map(|p| {
            // Command line and cwd are retained only when same-user ownership
            // is positively established; unknown ownership discloses nothing.
            let foreign = !ownership_confirmed(own_user.as_ref(), p.user_id());
            let argv: Vec<String> = p
                .cmd()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let executable = p
                .exe()
                .map(|e| e.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.name().to_string_lossy().into_owned());
            let started = p.start_time();
            ProcessInfo::new(
                pid_u32(p.pid()),
                (started > 0).then(|| (started as i64).saturating_mul(1000)),
                executable,
                if foreign || !CWD_OBSERVED {
                    None
                } else {
                    p.cwd().map(PathBuf::from)
                },
                if foreign || argv.is_empty() {
                    None
                } else {
                    Some(argv)
                },
            )
        })
        .collect();
    if processes.is_empty() {
        return Err(ProcessError::Unavailable);
    }
    processes.sort_by_key(|p| p.pid);
    Ok(ProcessSnapshot {
        sampled_at: now,
        processes,
    })
}

fn pid_u32(pid: Pid) -> u32 {
    pid.as_u32()
}

/// Map an observation attempt to an association; failure is `Unavailable`,
/// never `Unmatched`.
pub fn association_from_observation(
    session: &SessionSummary,
    observation: &Result<ProcessSnapshot, ProcessError>,
) -> ProcessAssociation {
    match observation {
        Ok(snapshot) => associate(session, snapshot),
        Err(_) => ProcessAssociation::Unavailable,
    }
}

/// Strong match only through an identity-bearing argument. Otherwise
/// same-harness processes are counted as candidates; if the OS withheld the
/// arguments of a runtime that could be hosting the harness, the answer is
/// `Unavailable` rather than a false `Unmatched`.
pub fn associate(session: &SessionSummary, snapshot: &ProcessSnapshot) -> ProcessAssociation {
    let harness = session.meta.key.harness;
    let id = session.meta.key.native_id.as_str();
    let matched = snapshot
        .processes
        .iter()
        .filter(|p| argv_refs(p).is_some_and(|a| identity_matches(harness, &a, id)))
        .min_by_key(|p| (p.started_at.unwrap_or(Timestamp::MAX), p.pid));
    if let Some(p) = matched {
        return ProcessAssociation::Matched {
            pid: p.pid,
            started_at: p.started_at,
        };
    }
    let count = snapshot
        .processes
        .iter()
        .filter(|p| is_candidate(harness, p))
        .count();
    if count > 0 {
        return ProcessAssociation::Candidates { count };
    }
    let hidden_runtime = snapshot
        .processes
        .iter()
        .any(|p| p.argv.is_none() && RUNTIMES.contains(&stem(&p.executable).as_str()));
    if hidden_runtime {
        ProcessAssociation::Unavailable
    } else {
        ProcessAssociation::Unmatched
    }
}

/// Whether this platform's cwd is read. Windows needs ReadProcessMemory on the
/// target to obtain a cwd, which is beyond the command-line-only exception in
/// spec §9.4, so it is never requested there (cwd shows `Unknown`).
pub const CWD_OBSERVED: bool = !cfg!(windows);

/// Same-user ownership must be positively established (both known and equal);
/// unknown ownership is treated as foreign.
pub fn ownership_confirmed<T: PartialEq>(me: Option<&T>, them: Option<&T>) -> bool {
    matches!((me, them), (Some(a), Some(b)) if a == b)
}
