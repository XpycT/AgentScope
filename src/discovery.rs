//! Read-only, resumable discovery of provider session files (spec §7.1, §5.1).
//!
//! Only the configured session root is walked. Symlinks are never followed, and
//! only files matching the provider's session-file pattern are returned, so
//! credential files and unrelated provider data are unreachable by construction.
//! The walk is a cursor over open directory listings: each [`Walk::step`] yields a
//! bounded number of sources and the next call continues where it stopped, so no
//! call collects or sorts the whole tree. Entries are visited in descending name
//! order, which is newest-first for date-structured roots.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

use crate::domain::{Harness, Warning, WarningKey, WarningKind};

/// Where a Claude-format transcript sits under a project root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaudePath {
    /// `<root>/<project>/<session>.jsonl`.
    Main,
    /// `<root>/<project>/<session>/subagents/**/agent-<id>.jsonl`.
    Child { parent_session: String },
}

fn is_agent_file(name: &str) -> bool {
    name.len() > "agent-.jsonl".len() && name.starts_with("agent-") && name.ends_with(".jsonl")
}

/// Classifies `path` against the Claude project layout. Anything else under a
/// root (workflow `journal.jsonl`, tool results, sidecars) is not a session.
pub fn classify_claude(root: &Path, path: &Path) -> Option<ClaudePath> {
    let rel = path.strip_prefix(root).ok()?;
    let parts: Vec<&str> = rel
        .components()
        .map(|c| c.as_os_str().to_str())
        .collect::<Option<_>>()?;
    let name = *parts.last()?;
    match parts.as_slice() {
        [_, file] if file.len() > ".jsonl".len() && file.ends_with(".jsonl") => {
            Some(ClaudePath::Main)
        }
        [_, session, subagents, .., _] if *subagents == "subagents" && is_agent_file(name) => {
            Some(ClaudePath::Child {
                parent_session: (*session).to_owned(),
            })
        }
        _ => None,
    }
}

/// Rollout files live at `<root>/YYYY/MM/DD/`; leave slack for layout changes.
const MAX_DEPTH: usize = 6;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceFile {
    pub path: PathBuf,
    pub modified: SystemTime,
    pub len: u64,
}

fn warning(
    harness: Harness,
    source: String,
    kind: WarningKind,
    message: String,
    count: u32,
) -> Warning {
    Warning {
        key: WarningKey {
            harness,
            source,
            kind,
        },
        message,
        count,
    }
}

/// Whether `name` is a Codex rollout file name, plain or compressed.
pub fn is_rollout_name(name: &str) -> bool {
    is_rollout(name)
}

fn is_rollout(name: &str) -> bool {
    let base = name.strip_suffix(".zst").unwrap_or(name);
    base.len() > "rollout-.jsonl".len() && base.starts_with("rollout-") && base.ends_with(".jsonl")
}

struct Entry {
    name: OsString,
    dir: bool,
}

struct Level {
    dir: PathBuf,
    depth: usize,
    /// The directory listing while it is still being enumerated (resumable: entries
    /// are read a bounded number per step, then sorted once enumeration ends).
    listing: Option<fs::ReadDir>,
    entries: Vec<Entry>,
    next: usize,
}

/// Directory entries read between deadline checks.
const ENUM_BATCH: usize = 64;

/// A resumable allowlisted traversal of one provider root.
pub struct Walk {
    harness: Harness,
    root: PathBuf,
    stack: Vec<Level>,
    warnings: Vec<Warning>,
    /// Directory entries examined so far (matching or not).
    scanned: usize,
    done: bool,
}

impl Walk {
    /// Starts a traversal of `root`. A missing root is a warning, not an error, and
    /// finishes the walk immediately.
    pub fn new(harness: Harness, root: Option<&Path>) -> Self {
        Self::new_in(harness, root, true)
    }

    /// Like [`Walk::new`]; with `report_missing` false an absent root finishes the
    /// walk silently (a platform default for an app that is not installed).
    pub fn new_in(harness: Harness, root: Option<&Path>, report_missing: bool) -> Self {
        let mut walk = Self {
            harness,
            root: root.map(Path::to_path_buf).unwrap_or_default(),
            stack: Vec::new(),
            warnings: Vec::new(),
            scanned: 0,
            done: false,
        };
        let (label, noun) = match harness {
            Harness::Codex => ("codex", "Codex sessions"),
            Harness::Claude => ("claude", "Claude projects"),
            _ => ("", "sessions"),
        };
        let Some(root) = root else {
            walk.warnings.push(warning(
                harness,
                label.into(),
                WarningKind::Unavailable,
                format!("no {noun} root could be determined"),
                1,
            ));
            walk.done = true;
            return walk;
        };
        // An explicitly configured root may itself be a link; links beneath it are never followed.
        if !fs::metadata(root).is_ok_and(|m| m.is_dir()) {
            if report_missing {
                walk.warnings.push(warning(
                    harness,
                    root.display().to_string(),
                    WarningKind::Unavailable,
                    format!("{noun} root is missing or not a directory"),
                    1,
                ));
            }
            walk.done = true;
            return walk;
        }
        walk.open(root.to_path_buf(), 0);
        walk.done = walk.stack.is_empty();
        walk
    }

    /// Begins enumerating `dir`; no entry is read until a step asks for it.
    fn open(&mut self, dir: PathBuf, depth: usize) {
        match fs::read_dir(&dir) {
            Ok(listing) => self.stack.push(Level {
                dir,
                depth,
                listing: Some(listing),
                entries: Vec::new(),
                next: 0,
            }),
            Err(error) => self.warnings.push(warning(
                self.harness,
                dir.display().to_string(),
                WarningKind::Io,
                format!("directory unreadable ({:?})", error.kind()),
                1,
            )),
        }
    }

    /// Whether an entry of a directory at `depth` belongs to the allowlist.
    fn accepts(
        harness: Harness,
        root: &Path,
        dir: &Path,
        depth: usize,
        entry: &fs::DirEntry,
    ) -> Option<Entry> {
        // `DirEntry::file_type` does not follow symlinks, so links report as neither.
        let kind = entry.file_type().ok()?;
        let name = entry.file_name();
        let text = name.to_string_lossy();
        let keep = if kind.is_dir() {
            // A Claude session directory holds only `subagents` worth walking.
            depth < MAX_DEPTH && (harness != Harness::Claude || depth != 2 || text == "subagents")
        } else if kind.is_file() {
            match harness {
                Harness::Codex => is_rollout(&text),
                Harness::Claude => classify_claude(root, &dir.join(&name)).is_some(),
                _ => false,
            }
        } else {
            false
        };
        keep.then_some(Entry {
            name,
            dir: kind.is_dir(),
        })
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Directory entries examined so far.
    pub fn scanned(&self) -> usize {
        self.scanned
    }

    pub fn take_warnings(&mut self) -> Vec<Warning> {
        std::mem::take(&mut self.warnings)
    }

    /// Yields up to `max_sources` matching files, stopping early at `deadline`. Directory
    /// enumeration itself is bounded by the deadline too. At least one directory entry
    /// is always examined so every call makes progress.
    pub fn step(&mut self, max_sources: usize, deadline: Instant) -> Vec<SourceFile> {
        let mut out = Vec::new();
        let mut examined = 0usize;
        while let Some(level) = self.stack.last_mut() {
            if examined > 0 && (out.len() >= max_sources || Instant::now() >= deadline) {
                break;
            }
            if let Some(listing) = level.listing.as_mut() {
                // Enumerate a bounded batch, then return to the deadline check above.
                let mut ended = false;
                for _ in 0..ENUM_BATCH {
                    match listing.next() {
                        Some(Ok(entry)) => {
                            if let Some(e) = Self::accepts(
                                self.harness,
                                &self.root,
                                &level.dir,
                                level.depth,
                                &entry,
                            ) {
                                level.entries.push(e);
                            }
                        }
                        Some(Err(_)) => {}
                        None => {
                            ended = true;
                            break;
                        }
                    }
                    examined += 1;
                    self.scanned += 1;
                }
                if ended {
                    level.listing = None;
                    level.entries.sort_unstable_by(|a, b| b.name.cmp(&a.name));
                }
                examined += usize::from(examined == 0);
                continue;
            }
            let Some(entry) = level.entries.get(level.next) else {
                self.stack.pop();
                continue;
            };
            level.next += 1;
            examined += 1;
            self.scanned += 1;
            let path = level.dir.join(&entry.name);
            if entry.dir {
                let depth = level.depth + 1;
                self.open(path, depth);
            } else if let Ok(meta) = fs::metadata(&path) {
                out.push(SourceFile {
                    path,
                    modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    len: meta.len(),
                });
            }
        }
        self.done = self.stack.is_empty();
        out
    }
}

/// The session directory a child transcript sits under (`.../<session>/subagents/...`),
/// derived from the path alone. Discovery bookkeeping uses it so a scope stays
/// computable after the root registry no longer lists that tree.
pub(crate) fn claude_family(path: &Path) -> Option<String> {
    path.ancestors()
        .find(|a| a.file_name().is_some_and(|n| n == "subagents"))
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
}

/// A regular file that is not a link (or, on Windows, any other reparse point).
/// Automatically discovered files are opened only through this check.
pub(crate) fn plain_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && !is_link(&m))
}

/// A real directory reached without crossing a link.
pub(crate) fn plain_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir() && !is_link(&m))
}

fn is_link(meta: &fs::Metadata) -> bool {
    if meta.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    false
}

/// Entries examined while locating Desktop stores; a data directory with more
/// session directories than this is reported as truncated, not crawled.
const DESKTOP_ENTRY_CAP: usize = 4096;
/// Directory depth of Code session manifests below `claude-code-sessions`
/// (`<account>/<organization>/local_<id>.json`, with one level of slack).
const MANIFEST_DEPTH: usize = 4;

/// The allowlisted local stores of one Claude Desktop application-data directory.
/// Only `claude-code-sessions/` manifests and Cowork project roots are located;
/// authentication, cache, audit, VM and renderer storage are never opened.
#[derive(Debug, Default)]
pub struct DesktopStores {
    /// `claude-code-sessions/**/local_*.json` Code session manifests.
    pub manifests: Vec<PathBuf>,
    /// `local-agent-mode-sessions/<a>/<b>/local_<id>/.claude/projects` roots.
    pub cowork_roots: Vec<PathBuf>,
    pub warnings: Vec<Warning>,
}

fn child_dirs(
    dir: &Path,
    budget: &mut usize,
    warnings: &mut Vec<Warning>,
) -> Vec<(String, PathBuf)> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                warnings.push(warning(
                    Harness::Claude,
                    dir.display().to_string(),
                    WarningKind::Io,
                    format!("directory unreadable ({:?})", error.kind()),
                    1,
                ));
            }
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in listing.flatten() {
        if *budget == 0 {
            warnings.push(warning(
                Harness::Claude,
                dir.display().to_string(),
                WarningKind::Unsupported,
                "Claude Desktop store listing truncated at the discovery limit".into(),
                1,
            ));
            break;
        }
        *budget -= 1;
        // Links (and Windows junctions) are never followed.
        if entry.file_type().is_ok_and(|t| t.is_dir())
            && !is_link_entry(&entry)
            && let Some(name) = entry.file_name().to_str()
        {
            out.push((name.to_owned(), entry.path()));
        }
    }
    out.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    out
}

fn is_link_entry(entry: &fs::DirEntry) -> bool {
    fs::symlink_metadata(entry.path()).is_ok_and(|m| is_link(&m))
}

/// Locates the allowlisted stores of the Desktop data directory `data_dir`. A
/// missing directory is reported only when `report_missing` (the user named it).
pub fn desktop_stores(data_dir: &Path, report_missing: bool) -> DesktopStores {
    let mut stores = DesktopStores::default();
    if !fs::metadata(data_dir).is_ok_and(|m| m.is_dir()) {
        if report_missing {
            stores.warnings.push(warning(
                Harness::Claude,
                data_dir.display().to_string(),
                WarningKind::Unavailable,
                "Claude Desktop data directory is missing or not a directory".into(),
                1,
            ));
        }
        return stores;
    }
    let mut budget = DESKTOP_ENTRY_CAP;

    // Code sessions: manifests only; the transcripts live in the ordinary project store.
    // Every automatically discovered component is opened only when it is not a link.
    let sessions_dir = data_dir.join("claude-code-sessions");
    let mut pending = Vec::new();
    if plain_dir(&sessions_dir) {
        pending.push((sessions_dir, 0usize));
    }
    while let Some((dir, depth)) = pending.pop() {
        let Ok(listing) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in listing.flatten() {
            if budget == 0 {
                stores.warnings.push(warning(
                    Harness::Claude,
                    dir.display().to_string(),
                    WarningKind::Unsupported,
                    "Claude Desktop store listing truncated at the discovery limit".into(),
                    1,
                ));
                pending.clear();
                break;
            }
            budget -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let path = entry.path();
            if kind.is_dir() && !is_link_entry(&entry) && depth < MANIFEST_DEPTH {
                pending.push((path, depth + 1));
            } else if kind.is_file()
                && !is_link_entry(&entry)
                && name.starts_with("local_")
                && name.ends_with(".json")
            {
                stores.manifests.push(path);
            }
        }
    }
    stores.manifests.sort_unstable();

    // Cowork: a session directory that carries a Claude-format project store. The
    // `.claude` component and the project store itself must be real directories.
    let base = data_dir.join("local-agent-mode-sessions");
    if plain_dir(&base) {
        for (_, device) in child_dirs(&base, &mut budget, &mut stores.warnings) {
            for (_, organization) in child_dirs(&device, &mut budget, &mut stores.warnings) {
                for (name, session) in child_dirs(&organization, &mut budget, &mut stores.warnings)
                {
                    if !name.starts_with("local_") {
                        continue;
                    }
                    let dot_claude = session.join(".claude");
                    let projects = dot_claude.join("projects");
                    if plain_dir(&dot_claude) && plain_dir(&projects) {
                        stores.cowork_roots.push(projects);
                    }
                }
            }
        }
    }
    stores
}
