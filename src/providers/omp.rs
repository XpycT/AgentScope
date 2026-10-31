//! OMP (oh-my-pi) session metadata discovery, tree identity and native
//! relationship parsing, pinned to can1357/oh-my-pi
//! `6a86df8539fccf3ad7818081266e554022fc21bc` (an installed build may differ).
//!
//! This module is the metadata foundation only: it is not a [`ProviderReader`]
//! and nothing registers it with the application. A sessions root holds one
//! bucket directory per working directory, each holding `<timestamp>_<id>.jsonl`
//! sessions and, per session, an artifacts directory named like the file without
//! `.jsonl` that holds child sessions (recursively). Only that layout is walked:
//! symlinks are not followed, directories that are not the artifacts directory of
//! a listed session (blobs, tool output) are not entered, and sources are never
//! opened beyond the bounded prefix/tail probe.
//!
//! [`ProviderReader`]: crate::providers::ProviderReader

pub mod links;
pub mod metadata;
pub mod tree;

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use crate::discovery::plain_file;
use crate::domain::{
    Harness, RefreshBudget, ScanProgress, SessionKey, SessionMetadata, Warning, WarningKey,
    WarningKind,
};
use crate::providers::{DiscoveryDelta, ProviderError};

pub use links::resolve_omp_links;
pub use metadata::{OmpHeader, parse_header, parse_title_slot};
pub use tree::{ActiveLeaf, OmpEntryHeader, OmpInsert, OmpTree, OmpTreeError};

/// Artifact directory levels below a bucket that are walked.
const MAX_DEPTH: usize = 9;
/// Entries of one directory that are examined.
const DIR_CAP: usize = 16 * 1024;

struct Level {
    dir: PathBuf,
    depth: usize,
    entries: Vec<(std::ffi::OsString, bool)>,
    next: usize,
}

/// Resumable walk of the known OMP layout.
struct Walk {
    roots: VecDeque<PathBuf>,
    stack: Vec<Level>,
    visited: HashSet<PathBuf>,
    /// Entries examined in the current step; the deadline applies after the first.
    examined: usize,
}

struct Source {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
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
        // Links are never followed, so a repeated directory means overlapping roots.
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
            // `file_type` does not follow links, so links are neither.
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
        // Newest first for timestamp-named sessions; a session file is listed
        // before its artifacts directory.
        entries.sort_unstable_by(|a, b| b.0.cmp(&a.0));
        self.stack.push(Level {
            dir,
            depth,
            entries,
            next: 0,
        });
    }

    /// The next source file, or `None` when the walk is exhausted or `deadline` passed.
    fn next_source(&mut self, deadline: Instant, warnings: &mut Vec<Warning>) -> Option<Source> {
        loop {
            if self.examined > 0 && Instant::now() >= deadline && !self.stack.is_empty() {
                return None;
            }
            self.examined += 1;
            let Some(level) = self.stack.last_mut() else {
                let root = self.roots.pop_front()?;
                if fs::metadata(&root).is_ok_and(|m| m.is_dir()) {
                    self.open(root, 0, warnings);
                } else {
                    warnings.push(metadata::warning(
                        &root,
                        "root",
                        WarningKind::Unavailable,
                        "OMP sessions root is missing or not a directory",
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
                // The root holds buckets; below that only a listed session's
                // artifacts directory is entered.
                let artifacts = depth == 0
                    || level.entries.iter().any(|(other, other_dir)| {
                        !other_dir && other.to_string_lossy().strip_suffix(".jsonl") == Some(&*text)
                    });
                if artifacts && depth < MAX_DEPTH {
                    self.open(path, depth + 1, warnings);
                } else if artifacts {
                    warnings.push(metadata::warning(
                        &path,
                        "depth",
                        WarningKind::Unsupported,
                        "artifact directories nested deeper than the supported depth are not scanned",
                    ));
                }
            } else if depth > 0 {
                if text.ends_with(".jsonl.gz") {
                    warnings.push(metadata::warning(
                        &path,
                        "gz",
                        WarningKind::Unsupported,
                        "compressed OMP session archives are not supported",
                    ));
                } else if text.len() > ".jsonl".len()
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
}

struct Known {
    len: u64,
    modified: SystemTime,
    pass: u64,
    /// The source could not be read; it is probed again next pass.
    retry: bool,
    base: SessionMetadata,
    header: Option<OmpHeader>,
    /// Weight charged against the byte ceiling.
    bytes: usize,
    /// Metadata last reported for this source.
    reported: Option<SessionMetadata>,
}

/// Bounded, resumable discovery of OMP session metadata with cross-source
/// relationship resolution. Each [`OmpCatalog::step`] does a budgeted amount of
/// work and reports only metadata that changed since it was last reported.
pub struct OmpCatalog {
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
    /// Sources replaced by another session at the same path since the last slice.
    pending_removed: Vec<SessionKey>,
    /// Published sources displaced from `known`: only their key is kept, so a
    /// confirmed deletion still reaches the roster. Bounded; checked with a
    /// metadata call only, never read.
    forgotten: HashMap<PathBuf, SessionKey>,
}

fn warn_limit(max_sources: usize) -> Warning {
    Warning {
        key: WarningKey {
            harness: Harness::Omp,
            source: "metadata-limit".into(),
            kind: WarningKind::Unsupported,
        },
        message: format!(
            "Discovery limited: only {max_sources} OMP sources (or the byte ceiling) are tracked"
        ),
        count: 1,
    }
}

impl OmpCatalog {
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
            if !nested && !kept.iter().any(|(k, _)| k == canon) {
                kept.push((canon.clone(), root.clone()));
            }
        }
        Self {
            roots: kept.into_iter().map(|(_, root)| root).collect(),
            max_sources,
            max_bytes,
            walk: None,
            pass: 0,
            pass_sources: 0,
            limit_reported: false,
            known: HashMap::new(),
            retained_bytes: 0,
            invalidated: Vec::new(),
            pending_removed: Vec::new(),
            forgotten: HashMap::new(),
        }
    }

    fn weight(meta: &SessionMetadata, path: &Path) -> usize {
        crate::roster::metadata_bytes(meta) + path.as_os_str().len()
    }

    /// Probes one source and records it.
    fn visit(&mut self, source: Source, bytes: &mut usize, warnings: &mut Vec<Warning>) -> bool {
        if let Some(known) = self.known.get_mut(&source.path)
            && !known.retry
            && known.len == source.len
            && known.modified == source.modified
        {
            known.pass = self.pass;
            return false;
        }
        let (base, header, retry) =
            match metadata::probe_source(&source.path, source.len, source.modified) {
                Ok(probed) => {
                    *bytes = bytes.saturating_add(probed.bytes);
                    warnings.extend(probed.warnings);
                    (probed.meta, probed.header, false)
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
                        None,
                        true,
                    )
                }
            };
        let weight = Self::weight(&base, &source.path);
        let old_weight = self.known.get(&source.path).map_or(0, |k| k.bytes);
        let is_new = !self.known.contains_key(&source.path);
        // Admission covers updates too: a tracked header that grows must still fit.
        let mut evicted = Vec::new();
        loop {
            let over_count = is_new && self.known.len() - evicted.len() >= self.max_sources;
            let freed: usize = evicted.iter().map(|p: &PathBuf| self.known[p].bytes).sum();
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
            // Only a strictly newer source displaces the oldest other one.
            let at = base.activity.at;
            let victim = self
                .known
                .iter()
                .filter(|(p, _)| **p != source.path && !evicted.contains(*p))
                .min_by(|(pa, a), (pb, b)| {
                    a.base
                        .activity
                        .at
                        .cmp(&b.base.activity.at)
                        .then_with(|| pb.cmp(pa))
                })
                .map(|(p, k)| (p.clone(), k.base.activity.at));
            match victim {
                Some((path, victim_at)) if at > victim_at => evicted.push(path),
                _ => {
                    // A refused update keeps the admitted entry (still present).
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
                if let Some(reported) = gone.reported {
                    if self.forgotten.len() >= self.max_sources.saturating_mul(4).max(64)
                        && let Some(drop) = self.forgotten.keys().next().cloned()
                    {
                        self.forgotten.remove(&drop);
                    }
                    self.forgotten.insert(path, reported.key);
                }
            }
        }
        self.forgotten.remove(&source.path);
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
                header,
                bytes: weight,
                reported,
            },
        );
        true
    }

    /// Resolves relationships over every known source and returns the metadata
    /// whose reported form changed.
    fn publish(&mut self) -> Vec<SessionMetadata> {
        let mut paths: Vec<&PathBuf> = self.known.keys().collect();
        paths.sort();
        let paths: Vec<PathBuf> = paths.into_iter().cloned().collect();
        let mut metas: Vec<SessionMetadata> =
            paths.iter().map(|p| self.known[p].base.clone()).collect();
        let headers: Vec<Option<&OmpHeader>> = paths
            .iter()
            .map(|p| self.known[p].header.as_ref())
            .collect();
        links::resolve_aligned(&mut metas, &headers);
        let mut changed = Vec::new();
        for (path, meta) in paths.into_iter().zip(metas) {
            if let Some(known) = self.known.get_mut(&path)
                && known.reported.as_ref() != Some(&meta)
            {
                known.reported = Some(meta.clone());
                changed.push(meta);
            }
        }
        changed
    }

    /// One bounded slice of discovery. `scan.done` ends a pass; sources the pass
    /// did not see are then reported removed and the next call starts a new pass.
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
        if finished {
            // Displaced sources are not walked into `known`; only a confirmed
            // absence is a deletion (any other error keeps the watch).
            let missing: Vec<PathBuf> = self
                .forgotten
                .keys()
                .filter(|path| {
                    !self.known.contains_key(*path)
                        && fs::symlink_metadata(path)
                            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
                })
                .cloned()
                .collect();
            for path in missing {
                if let Some(key) = self.forgotten.remove(&path) {
                    removed.push(key);
                    warnings.push(metadata::warning(
                        &path,
                        "removed",
                        WarningKind::Unavailable,
                        "session file is no longer present",
                    ));
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
                harness: Harness::Omp,
                scanned: self.pass_sources,
                total: finished.then_some(self.pass_sources),
                done: finished,
            },
        })
    }
}
