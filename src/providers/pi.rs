//! Native Pi sessions: the standard `<root>/<project-bucket>/<session>.jsonl`
//! layout, bounded metadata discovery and the observed-request fold.
//!
//! Pinned to earendil-works/pi `packages/coding-agent/docs/session-format.md`,
//! `core/session-manager.ts` and `packages/ai/src/types.ts` at the inspected main
//! revision. Only the standard JSONL tree format is read; Pi extension children are
//! not a native concept and no subagent relationship is invented. Pi stores no
//! explicit session-end marker and standard records have no request TTFT; an
//! assistant `durationMs` is the only request timing this adapter treats as
//! measured. Forks are separate files with their own `parentSession` evidence.
//!
//! No [`crate::providers::ProviderReader`] is registered for Pi yet: this stage
//! exposes the discovery, parsing and hydration components that the reader will
//! drive, and never a test-only provider path.

pub mod hydration;
pub mod metadata;
pub mod record;
pub mod tree;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::discovery::plain_file;
use crate::domain::{
    Harness, RefreshBudget, ScanProgress, SessionKey, SessionLink, SessionMetadata, SourceKey,
    Warning, WarningKey, WarningKind,
};
use crate::providers::{DiscoveryDelta, ProviderError};

pub use hydration::PiHydration;
pub use metadata::{Probed, probe_source};
pub use record::{
    PiEntry, PiEntryKind, PiHeader, PiPrompt, PiRecordError, PiRole, additive_total,
    assistant_model, categories_incomplete, invalid_money, parse_entry, parse_header,
    parse_pi_usage, prompt_tokens, record_may_carry_usage, reported_total, total_conflicts,
};
pub use tree::{PiInsert, PiOwnership, PiTree, PiTreeError};

/// Directory entries examined from one bucket.
const DIR_CAP: usize = 16 * 1024;

struct Source {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

/// One open directory of the walk.
struct Level {
    dir: PathBuf,
    depth: usize,
    entries: Vec<(std::ffi::OsString, bool)>,
    next: usize,
}

/// Resumable walk of the standard Pi layout: roots hold project buckets, buckets
/// hold session JSONL files. Nothing below a bucket is entered, so extension blobs
/// and tool output are never read.
struct Walk {
    roots: VecDeque<PathBuf>,
    stack: Vec<Level>,
    visited: HashSet<PathBuf>,
    examined: usize,
}

impl Walk {
    fn new(roots: &[PathBuf]) -> Self {
        Self {
            roots: roots.iter().cloned().collect(),
            stack: Vec::new(),
            visited: HashSet::new(),
            examined: 0,
        }
    }

    fn is_done(&self) -> bool {
        self.stack.is_empty() && self.roots.is_empty()
    }

    fn open(&mut self, dir: PathBuf, depth: usize, warnings: &mut Vec<Warning>) {
        if !self
            .visited
            .insert(fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone()))
        {
            return;
        }
        let listing = match fs::read_dir(&dir) {
            Ok(listing) => listing,
            Err(error) => {
                warnings.push(metadata::warning(
                    &dir,
                    "dir",
                    WarningKind::Io,
                    &format!("directory unreadable ({:?})", error.kind()),
                ));
                return;
            }
        };
        let mut entries = Vec::new();
        let mut truncated = false;
        for (seen, entry) in listing.enumerate() {
            if seen >= DIR_CAP {
                truncated = true;
                break;
            }
            let Ok(entry) = entry else { continue };
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                entries.push((entry.file_name(), true));
            } else if kind.is_file() {
                entries.push((entry.file_name(), false));
            }
        }
        if truncated {
            warnings.push(metadata::warning(
                &dir,
                "listing",
                WarningKind::Unsupported,
                "directory listing truncated: too many entries",
            ));
        }
        entries.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        self.stack.push(Level {
            dir,
            depth,
            entries,
            next: 0,
        });
    }

    fn next_source(&mut self, deadline: Instant, warnings: &mut Vec<Warning>) -> Option<Source> {
        loop {
            if self.examined > 0 && Instant::now() >= deadline && !self.stack.is_empty() {
                return None;
            }
            self.examined += 1;
            let Some(level) = self.stack.last_mut() else {
                let root = self.roots.pop_front()?;
                if fs::metadata(&root).is_ok_and(|meta| meta.is_dir()) {
                    self.open(root, 0, warnings);
                } else {
                    warnings.push(metadata::warning(
                        &root,
                        "root",
                        WarningKind::Unavailable,
                        "Pi sessions root is missing or not a directory",
                    ));
                }
                continue;
            };
            let Some((name, is_dir)) = level.entries.get(level.next).cloned() else {
                self.stack.pop();
                continue;
            };
            level.next += 1;
            let (dir, depth) = (level.dir.clone(), level.depth);
            let text = name.to_string_lossy();
            let path = dir.join(&name);
            if is_dir {
                // Roots hold project buckets; a bucket holds only session files.
                if depth == 0 {
                    self.open(path, 1, warnings);
                }
            } else if depth == 1
                && text.len() > ".jsonl".len()
                && text.ends_with(".jsonl")
                && !text.contains(".bak")
                && plain_file(&path)
                && let Ok(meta) = fs::metadata(&path)
            {
                return Some(Source {
                    path,
                    modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    len: meta.len(),
                });
            }
        }
    }
}

struct Known {
    len: u64,
    modified: SystemTime,
    pass: u64,
    retry: bool,
    base: SessionMetadata,
    /// The header's `parentSession` text, kept only to resolve the fork edge against
    /// already-discovered sources. The path itself is never opened.
    parent: Option<String>,
    bytes: usize,
    reported: Option<SessionMetadata>,
}

/// Whether a header reference is rooted on any platform: POSIX `/`, a Windows/UNC
/// separator, or a drive/URI prefix. A rooted reference names one exact location, so
/// an undiscovered file name is never a substitute for it.
fn rooted_reference(text: &str) -> bool {
    text.starts_with(['/', '\\']) || text.as_bytes().get(1) == Some(&b':') || text.contains("://")
}

fn warn_limit(max_sources: usize) -> Warning {
    Warning {
        key: WarningKey {
            harness: Harness::Pi,
            source: "metadata-limit".into(),
            kind: WarningKind::Unsupported,
        },
        message: format!(
            "Discovery limited: only {max_sources} Pi sources (or the byte ceiling) are tracked"
        ),
        count: 1,
    }
}

/// Bounded, resumable discovery of native Pi session metadata. Each
/// [`PiCatalog::step`] probes a budgeted number of sources and reports only
/// metadata that changed since it was last reported.
pub struct PiCatalog {
    roots: Vec<PathBuf>,
    max_sources: usize,
    max_bytes: usize,
    walk: Option<Walk>,
    pass: u64,
    pass_sources: usize,
    limit_reported: bool,
    known: HashMap<PathBuf, Known>,
    retained_bytes: usize,
    invalidated: Vec<SessionKey>,
    pending_removed: Vec<SessionKey>,
}

impl PiCatalog {
    pub fn new(roots: Vec<PathBuf>, max_sources: usize, max_bytes: usize) -> Self {
        // A root inside another configured root would be walked twice.
        let canonical: Vec<(PathBuf, PathBuf)> = roots
            .into_iter()
            .map(|root| {
                (
                    fs::canonicalize(&root).unwrap_or_else(|_| root.clone()),
                    root,
                )
            })
            .collect();
        let mut kept: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (canon, root) in &canonical {
            let nested = canonical
                .iter()
                .any(|(other, _)| other != canon && canon.starts_with(other));
            if !nested && !kept.iter().any(|(seen, _)| seen == canon) {
                kept.push((canon.clone(), root.clone()));
            }
        }
        Self {
            roots: kept.into_iter().map(|(_, root)| root).collect(),
            max_sources: max_sources.max(1),
            max_bytes: max_bytes.max(1),
            walk: None,
            pass: 0,
            pass_sources: 0,
            limit_reported: false,
            known: HashMap::new(),
            retained_bytes: 0,
            invalidated: Vec::new(),
            pending_removed: Vec::new(),
        }
    }

    fn weight(meta: &SessionMetadata, path: &Path) -> usize {
        crate::roster::metadata_bytes(meta) + path.as_os_str().len()
    }

    /// The metadata last reported for `key`, while its source is still tracked.
    pub fn reported(&self, key: &SessionKey) -> Option<&SessionMetadata> {
        let SourceKey::File(path) = &key.source else {
            return None;
        };
        self.known
            .get(path)?
            .reported
            .as_ref()
            .filter(|meta| meta.key == *key)
    }

    fn visit(&mut self, source: Source, bytes: &mut usize, warnings: &mut Vec<Warning>) -> bool {
        if let Some(known) = self.known.get_mut(&source.path)
            && !known.retry
            && known.len == source.len
            && known.modified == source.modified
        {
            known.pass = self.pass;
            return false;
        }
        let (base, retry, parent) =
            match metadata::probe_source(&source.path, source.len, source.modified) {
                Ok(probed) => {
                    *bytes = bytes.saturating_add(probed.bytes);
                    warnings.extend(probed.warnings);
                    let parent = probed
                        .header
                        .as_ref()
                        .and_then(|header| header.parent_session.clone())
                        .filter(|parent| !parent.is_empty());
                    (probed.meta, false, parent)
                }
                Err(_) => {
                    warnings.push(metadata::warning(
                        &source.path,
                        "unavailable",
                        WarningKind::Unavailable,
                        "source unavailable",
                    ));
                    (
                        metadata::unreadable(&source.path, source.modified),
                        true,
                        None,
                    )
                }
            };
        let weight = Self::weight(&base, &source.path);
        let old_weight = self.known.get(&source.path).map_or(0, |known| known.bytes);
        let is_new = !self.known.contains_key(&source.path);
        // Admission covers updates too: a tracked header that grows must still fit.
        let mut evicted = Vec::new();
        loop {
            let over_count = is_new && self.known.len() - evicted.len() >= self.max_sources;
            let freed: usize = evicted
                .iter()
                .map(|path: &PathBuf| self.known[path].bytes)
                .sum();
            let over_bytes = self
                .retained_bytes
                .saturating_sub(old_weight)
                .saturating_sub(freed)
                .saturating_add(weight)
                > self.max_bytes;
            if !over_count && !over_bytes {
                break;
            }
            if !self.limit_reported {
                self.limit_reported = true;
                warnings.push(warn_limit(self.max_sources));
            }
            let at = base.activity.at;
            let victim = self
                .known
                .iter()
                .filter(|(path, _)| **path != source.path && !evicted.contains(*path))
                .min_by(|(path_a, a), (path_b, b)| {
                    a.base
                        .activity
                        .at
                        .cmp(&b.base.activity.at)
                        .then_with(|| path_b.cmp(path_a))
                })
                .map(|(path, known)| (path.clone(), known.base.activity.at));
            match victim {
                Some((path, victim_at)) if at > victim_at => evicted.push(path),
                _ => {
                    if let Some(old) = self.known.get_mut(&source.path) {
                        old.pass = self.pass;
                    }
                    return false;
                }
            }
        }
        for path in evicted {
            if let Some(gone) = self.known.remove(&path) {
                self.retained_bytes -= gone.bytes;
                // A source already published to the roster must be withdrawn, not
                // silently forgotten, or later cleanup can never remove it.
                if let Some(reported) = gone.reported {
                    self.pending_removed.push(reported.key);
                }
            }
        }
        let mut reported = None;
        if let Some(old) = self.known.remove(&source.path) {
            if old.base.key != base.key {
                // The path now holds another session: the old one is gone.
                if let Some(reported) = &old.reported {
                    self.pending_removed.push(reported.key.clone());
                }
            } else {
                if source.len < old.len {
                    self.invalidated.push(base.key.clone());
                }
                reported = old.reported;
            }
        }
        self.retained_bytes = self.retained_bytes - old_weight + weight;
        self.known.insert(
            source.path,
            Known {
                len: source.len,
                modified: source.modified,
                pass: self.pass,
                retry,
                base,
                parent,
                bytes: weight,
                reported,
            },
        );
        true
    }

    /// One bounded slice. `scan.done` ends a pass; sources the pass did not see are
    /// then reported removed and the next call starts a new pass.
    pub fn step(&mut self, budget: RefreshBudget) -> Result<DiscoveryDelta, ProviderError> {
        let deadline = Instant::now() + Duration::from_millis(budget.max_millis);
        if self.walk.is_none() {
            self.pass += 1;
            self.pass_sources = 0;
            self.limit_reported = false;
            self.walk = Some(Walk::new(&self.roots));
        }
        if let Some(walk) = self.walk.as_mut() {
            walk.examined = 0;
        }
        let mut warnings = Vec::new();
        let (mut records, mut bytes) = (0usize, 0usize);
        let mut changed = false;
        let mut finished = false;
        while let Some(walk) = self.walk.as_mut() {
            let Some(source) = walk.next_source(deadline, &mut warnings) else {
                finished = walk.is_done();
                break;
            };
            self.pass_sources += 1;
            records += 1;
            changed |= self.visit(source, &mut bytes, &mut warnings);
            if records >= budget.max_records.max(1)
                || bytes >= budget.max_bytes.max(1)
                || Instant::now() >= deadline
            {
                break;
            }
        }
        let mut removed = std::mem::take(&mut self.pending_removed);
        if finished {
            self.walk = None;
            let pass = self.pass;
            let gone: Vec<PathBuf> = self
                .known
                .iter()
                .filter(|(_, known)| known.pass != pass)
                .map(|(path, _)| path.clone())
                .collect();
            for path in gone {
                if let Some(known) = self.known.remove(&path) {
                    self.retained_bytes -= known.bytes;
                    if let Some(reported) = known.reported {
                        removed.push(reported.key);
                    }
                    warnings.push(metadata::warning(
                        &path,
                        "removed",
                        WarningKind::Unavailable,
                        "session file is no longer present",
                    ));
                    changed = true;
                }
            }
        }
        let sessions = if changed || !removed.is_empty() {
            self.publish()
        } else {
            Vec::new()
        };
        Ok(DiscoveryDelta {
            sessions,
            removed,
            invalidated: std::mem::take(&mut self.invalidated),
            warnings,
            scan: ScanProgress {
                harness: Harness::Pi,
                scanned: self.pass_sources,
                total: finished.then_some(self.pass_sources),
                done: finished,
            },
        })
    }

    /// Reports metadata whose published form changed since the last time. The fork
    /// edge is resolved here, among already-discovered sources only.
    fn publish(&mut self) -> Vec<SessionMetadata> {
        let mut paths: Vec<PathBuf> = self.known.keys().cloned().collect();
        paths.sort();
        let links = self.fork_links(&paths);
        let mut changed = Vec::new();
        for (path, link) in paths.iter().zip(links) {
            let Some(known) = self.known.get_mut(path) else {
                continue;
            };
            known.base.lineage.fork = link;
            if known.reported.as_ref() != Some(&known.base) {
                known.reported = Some(known.base.clone());
                changed.push(known.base.clone());
            }
        }
        changed
    }

    /// The `lineage.fork` link of each source. The producer writes an absolute path
    /// to the source session file, so a path (or, when roots are spelled
    /// differently, a unique file name) identifies a discovered source. Nothing
    /// outside the discovered set is ever opened, and a fork is never a parent.
    fn fork_links(&self, paths: &[PathBuf]) -> Vec<Option<SessionLink>> {
        let mut by_path: HashMap<&Path, &SessionKey> = HashMap::new();
        let mut by_name: HashMap<&std::ffi::OsStr, Vec<&SessionKey>> = HashMap::new();
        for path in paths {
            let Some(known) = self.known.get(path) else {
                continue;
            };
            by_path.insert(path.as_path(), &known.base.key);
            if let Some(name) = path.file_name() {
                by_name.entry(name).or_default().push(&known.base.key);
            }
        }
        paths
            .iter()
            .map(|path| {
                let known = self.known.get(path)?;
                let text = known.parent.as_deref()?;
                let target = Path::new(text);
                let found: Vec<&SessionKey> = match by_path.get(target) {
                    Some(key) => vec![*key],
                    // A rooted path the discovery set does not contain is not evidence
                    // for a same-named file elsewhere; only a bare/relative reference
                    // may stand in for a relocated store.
                    None if rooted_reference(text) => Vec::new(),
                    None => target
                        .file_name()
                        .and_then(|name| by_name.get(name))
                        .cloned()
                        .unwrap_or_default(),
                };
                let found: Vec<&SessionKey> = found
                    .into_iter()
                    .filter(|key| key != &&known.base.key)
                    .collect();
                Some(match found.as_slice() {
                    [only] => SessionLink::Resolved((*only).clone()),
                    [] => SessionLink::Unresolved(metadata::name_id(target)),
                    _ => SessionLink::Ambiguous,
                })
            })
            .collect()
    }
}
