//! Bounded consumption of one physical session source, plain or archived.
//!
//! A [`SourceReader`] frames records with the same line rules as [`JsonlCursor`],
//! whatever the storage representation. Archived (`.jsonl.zst`) rollouts are decoded
//! incrementally into the framing buffer, so the expanded history is never
//! materialized and one call's work is bounded by the [`RefreshBudget`].

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::domain::{LocatorKind, RefreshBudget, SessionKey, SourceKey, SourceLocator};
use crate::ingest::jsonl::{Chunk, JsonlCursor, LineFramer, LineLookup, Record, ScanBudget, Stamp};
use crate::ingest::text::display_safe;
use crate::providers::SourceRecord;

/// Decompressed bytes requested per decoder read: bounds the working set of one step
/// and the fraction of the record limit a single read can overshoot.
const DECODE_CHUNK: usize = 64 * 1024;

/// Why a compressed source could not be decoded further. Content-free by construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceDamage {
    /// The compressed stream ended before its frame completed; decoded content before
    /// the cut was already delivered.
    Truncated,
    /// The compressed stream is not a decodable zstd frame.
    Corrupt,
}

impl fmt::Display for SourceDamage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated compressed frame"),
            Self::Corrupt => f.write_str("corrupt compressed frame"),
        }
    }
}

/// Failure to read the physical source itself. Per-record problems are reported as
/// [`SourceItem`]s instead, so one bad record never stops the rest of a source.
#[derive(Debug)]
pub enum SourceError {
    Io(io::Error),
}

impl SourceError {
    pub fn kind(&self) -> io::ErrorKind {
        let Self::Io(error) = self;
        error.kind()
    }
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self::Io(error) = self;
        write!(f, "source {:?} error", error.kind())
    }
}

impl std::error::Error for SourceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        let Self::Io(error) = self;
        Some(error)
    }
}

impl From<io::Error> for SourceError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// One framed line's outcome, without byte offsets (those live in the locator).
#[derive(Clone, PartialEq)]
pub enum SourceItem {
    Json(serde_json::Value),
    Invalid,
    TooLarge,
    /// The stream ended inside a partial line, or inside a line still being discarded.
    Incomplete,
    /// An oversized line is being discarded; resuming continues the drain.
    Skipping,
    /// The physical source could not be decoded further; nothing follows it.
    Damaged(SourceDamage),
}

// Records hold prompts, tool output, and credentials; only shape is reported.
impl fmt::Debug for SourceItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(_) => f.write_str("Json"),
            Self::Invalid => f.write_str("Invalid"),
            Self::TooLarge => f.write_str("TooLarge"),
            Self::Incomplete => f.write_str("Incomplete"),
            Self::Skipping => f.write_str("Skipping"),
            Self::Damaged(damage) => f.debug_tuple("Damaged").field(damage).finish(),
        }
    }
}

/// One line of a source with its exact physical address and generation.
#[derive(Debug)]
pub struct SourceRecordItem {
    /// Zero-based position of the line in the expanded record stream.
    pub ordinal: u64,
    /// Exact start of this line in the physical source: a byte offset for a plain file,
    /// a compressed ordinal for an archive. Decompressed offsets are never recorded.
    pub locator: LocatorKind,
    /// Generation of the source this line was read from.
    pub generation: u64,
    pub item: SourceItem,
}

impl SourceRecordItem {
    /// The full locator, once the caller supplies the session its source belongs to.
    pub fn source_locator(&self, session: SessionKey) -> SourceLocator {
        SourceLocator {
            session,
            kind: self.locator.clone(),
            generation: Some(self.generation),
        }
    }
}

/// One bounded slice of source work.
#[derive(Debug)]
pub struct SourceBatch {
    pub records: Vec<SourceRecordItem>,
    /// Plain bytes or decompressed bytes framed by this call.
    pub expanded_bytes: usize,
    /// This generation is exhausted; a later call resumes only after the physical
    /// source changes.
    pub done: bool,
    /// The physical source was replaced or rotated and the reader restarted. Locators
    /// whose generation is older than [`SourceReader::generation`] are stale.
    pub invalidated: bool,
}

/// Progress towards one locator. `Pending` is normal work, not an error.
#[derive(Clone, Debug, PartialEq)]
pub enum SourceRecordProgress {
    /// Indexing this generation has not reached the record yet; call again.
    Pending,
    Ready(SourceRecord),
    /// The locator belongs to another source, an older generation, or a record this
    /// generation no longer contains.
    Stale,
}

/// Reads one physical session file as a stream of framed lines.
///
/// One reader serves one purpose at a time. `step` scans forward and keeps a sequential
/// position; `record` answers a locator:
///
/// - a plain source is read at the locator's byte offset through a separate handle and
///   leaves the scan position untouched (an oversized line's completion check is itself
///   resumable, reporting `Pending` when the call's allowance ends);
/// - an archive cannot seek a decompressed ordinal, so locating indexes the live stream
///   forward, and rewinds + re-decodes it when the record is behind the current position.
///   After a successful archive `record`, `step` continues from the record after it.
///
/// Interleaving scan and locate on one reader is therefore unsupported (they would
/// re-read or skip rows); plan02's job-keyed readers give each purpose its own reader.
pub struct SourceReader {
    physical: PathBuf,
    identity: PathBuf,
    max_record_bytes: usize,
    kind: Kind,
}

enum Kind {
    /// Plain, possibly growing file: append/truncate/rotation stay the cursor's job.
    Plain { cursor: JsonlCursor, ordinal: u64 },
    /// Archived zstd stream: stable only while its observed stamp is unchanged.
    Zstd(ZstdSource),
}

impl SourceReader {
    /// Opens `path` for bounded reading. A `.zst` path is decoded as an archived
    /// rollout; anything else is read as a plain, possibly growing file.
    pub fn open(path: &Path, max_record_bytes: usize) -> Result<Self, SourceError> {
        let kind = if is_archived(path) {
            Kind::Zstd(ZstdSource::open(path, max_record_bytes)?)
        } else {
            Kind::Plain {
                cursor: JsonlCursor::open(path.to_path_buf(), max_record_bytes)?,
                ordinal: 0,
            }
        };
        Ok(Self {
            physical: path.to_path_buf(),
            identity: canonical_rollout_identity(path),
            max_record_bytes,
            kind,
        })
    }

    /// The file actually read: the plain rollout or the compressed archive.
    pub fn physical_path(&self) -> &Path {
        &self.physical
    }

    /// Opaque generation of the currently readable source. Locators carry it, so a
    /// replacement makes every earlier locator stale.
    pub fn generation(&self) -> u64 {
        match &self.kind {
            Kind::Plain { cursor, .. } => cursor.rotation_count(),
            Kind::Zstd(zstd) => zstd.generation,
        }
    }

    /// Frames records until the record, byte, or time allowance of `budget` is spent,
    /// the source is exhausted, or the physical source is replaced.
    pub fn step(&mut self, budget: RefreshBudget) -> Result<SourceBatch, SourceError> {
        let mut batch = SourceBatch {
            records: Vec::new(),
            expanded_bytes: 0,
            done: false,
            invalidated: false,
        };
        if budget.max_records == 0 || budget.max_bytes == 0 {
            return Ok(batch);
        }
        let deadline = deadline_of(budget);
        match &mut self.kind {
            Kind::Plain { cursor, ordinal } => {
                step_plain(
                    &self.physical,
                    cursor,
                    ordinal,
                    &mut batch,
                    budget,
                    deadline,
                )?;
            }
            Kind::Zstd(zstd) => {
                if zstd.refresh_generation()? {
                    batch.invalidated = true;
                }
                zstd.step(&self.physical, &mut batch, budget, deadline)?;
            }
        }
        Ok(batch)
    }

    /// Reads the record a locator names, in the representation that locator addresses.
    ///
    /// Returns `Stale` when the locator's generation is not the observed one (including a
    /// replacement discovered by this call), when its path or locator kind is not this
    /// source, or when nothing complete is recorded there — past the end, mid-line, or a
    /// trailing partial line without a terminator. `Pending` means the allowance (or the
    /// deadline) ran out before the answer was known — the archive stream had not reached
    /// the record, or a plain source's oversized-line completion scan was interrupted —
    /// so calling again resumes the same work.
    pub fn record(
        &mut self,
        locator: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<SourceRecordProgress, SourceError> {
        if locator.generation != Some(self.generation()) || !self.addresses(&locator.session.source)
        {
            return Ok(SourceRecordProgress::Stale);
        }
        let deadline = deadline_of(budget);
        let max = self.max_record_bytes;
        match (&mut self.kind, &locator.kind) {
            (Kind::Plain { cursor, ordinal }, LocatorKind::FileOffset { path, offset }) => {
                if path != &self.physical {
                    return Ok(SourceRecordProgress::Stale);
                }
                // The stamp check `next_record` performs at end of file, done before
                // serving: a replaced or truncated file must not answer with old bytes.
                let generation = cursor.rotation_count();
                cursor.refresh()?;
                if cursor.rotation_count() != generation {
                    *ordinal = 0;
                    return Ok(SourceRecordProgress::Stale);
                }
                let lookup = cursor.read_line(
                    *offset,
                    max,
                    ScanBudget {
                        bytes: budget.max_bytes,
                        deadline,
                    },
                )?;
                Ok(match lookup {
                    LineLookup::Complete(Some(bytes)) => {
                        SourceRecordProgress::Ready(source_record(&bytes))
                    }
                    LineLookup::Complete(None) => {
                        SourceRecordProgress::Ready(oversized_record(max))
                    }
                    // A long unterminated tail is proven within the caller's allowance.
                    LineLookup::Pending => SourceRecordProgress::Pending,
                    // Past the end, a trailing partial line, or a mid-line offset: there
                    // is no complete record there, in either representation.
                    LineLookup::Absent | LineLookup::NotBoundary => SourceRecordProgress::Stale,
                })
            }
            (Kind::Zstd(zstd), LocatorKind::CompressedRecord { path, ordinal }) => {
                if path != &self.physical {
                    return Ok(SourceRecordProgress::Stale);
                }
                if zstd.refresh_generation()? {
                    // The archive was replaced; the locator predates the new generation.
                    return Ok(SourceRecordProgress::Stale);
                }
                zstd.record(*ordinal, max, budget, deadline)
            }
            // A byte offset cannot name an archived record and an ordinal cannot name a
            // plain one; either way this reader cannot serve the locator.
            _ => Ok(SourceRecordProgress::Stale),
        }
    }

    /// Whether `source` names this physical file or its canonical identity.
    fn addresses(&self, source: &SourceKey) -> bool {
        match source {
            SourceKey::File(path) => path == &self.physical || path == &self.identity,
            SourceKey::DbRow { .. } => false,
        }
    }
}

/// One framing step of the streaming decoder.
struct Advanced {
    /// Decompressed bytes framed by this step.
    consumed: usize,
    outcome: Outcome,
}

enum Outcome {
    Line {
        ordinal: u64,
        record: Record,
        /// The line's bytes, when the caller asked for them and it is not oversized.
        bytes: Option<Vec<u8>>,
    },
    Skipping {
        ordinal: u64,
    },
    Incomplete,
    End,
    Damaged(SourceDamage),
    Exhausted,
}

/// Incremental zstd decoding into bounded line framing.
struct ZstdSource {
    path: PathBuf,
    max_record_bytes: usize,
    stamp: Stamp,
    decoder: zstd::stream::read::Decoder<'static, io::BufReader<File>>,
    framer: LineFramer,
    /// Decompressed bytes read from the decoder, of which `[pos, filled)` is unframed.
    chunk: Vec<u8>,
    pos: usize,
    filled: usize,
    /// Ordinal of the next line the framer will complete.
    ordinal: u64,
    generation: u64,
    finished: bool,
    damage: Option<SourceDamage>,
}

impl ZstdSource {
    fn open(path: &Path, max_record_bytes: usize) -> Result<Self, SourceError> {
        let file = File::open(path)?;
        let stamp = Stamp::of(&file.metadata()?);
        Ok(Self {
            path: path.to_path_buf(),
            max_record_bytes,
            stamp,
            decoder: zstd::stream::read::Decoder::new(file)?,
            framer: LineFramer::new(max_record_bytes),
            chunk: vec![0; DECODE_CHUNK],
            pos: 0,
            filled: 0,
            ordinal: 0,
            generation: 0,
            finished: false,
            damage: None,
        })
    }

    /// Restarts decoding at the first byte of the archive, keeping the generation for
    /// locators that index backwards.
    fn rewind(&mut self) -> Result<(), SourceError> {
        let generation = self.generation;
        *self = Self::open(&self.path, self.max_record_bytes)?;
        self.generation = generation;
        Ok(())
    }

    /// Restarts once the archive is no longer the one observed, and reports whether it
    /// did. Archives never grow in place, so any stamp change is a new generation.
    fn refresh_generation(&mut self) -> Result<bool, SourceError> {
        let current = Stamp::of(&fs::metadata(&self.path)?);
        if current == self.stamp {
            return Ok(false);
        }
        self.rewind()?;
        self.generation += 1;
        Ok(true)
    }

    fn step(
        &mut self,
        physical: &Path,
        batch: &mut SourceBatch,
        budget: RefreshBudget,
        deadline: Instant,
    ) -> Result<(), SourceError> {
        if self.finished || self.damage.is_some() {
            batch.done = true;
            return Ok(());
        }
        loop {
            if batch.records.len() >= budget.max_records || Instant::now() >= deadline {
                return Ok(());
            }
            let remaining = budget.max_bytes.saturating_sub(batch.expanded_bytes);
            let advanced = self.advance(remaining, deadline, false)?;
            batch.expanded_bytes = batch.expanded_bytes.saturating_add(advanced.consumed);
            let (ordinal, item) = match advanced.outcome {
                Outcome::Line {
                    ordinal, record, ..
                } => {
                    let item = match record {
                        Record::Json { value, .. } => SourceItem::Json(value),
                        Record::Invalid { .. } => SourceItem::Invalid,
                        Record::TooLarge { .. } => SourceItem::TooLarge,
                        Record::Incomplete { .. } => SourceItem::Incomplete,
                        Record::Skipping { .. } => SourceItem::Skipping,
                    };
                    (ordinal, item)
                }
                Outcome::Skipping { ordinal } => (ordinal, SourceItem::Skipping),
                Outcome::Incomplete => {
                    batch
                        .records
                        .push(self.item(physical, self.ordinal, SourceItem::Incomplete));
                    batch.done = true;
                    return Ok(());
                }
                Outcome::End => {
                    batch.done = true;
                    return Ok(());
                }
                Outcome::Damaged(damage) => {
                    batch.records.push(self.item(
                        physical,
                        self.ordinal,
                        SourceItem::Damaged(damage),
                    ));
                    batch.done = true;
                    return Ok(());
                }
                Outcome::Exhausted => return Ok(()),
            };
            batch.records.push(self.item(physical, ordinal, item));
        }
    }

    fn record(
        &mut self,
        target: u64,
        max_record_bytes: usize,
        budget: RefreshBudget,
        deadline: Instant,
    ) -> Result<SourceRecordProgress, SourceError> {
        if self.damage.is_some() || (self.finished && target >= self.ordinal) {
            return Ok(SourceRecordProgress::Stale);
        }
        // A compressed ordinal cannot be seeked: serving an earlier record restarts the
        // decode and indexes forward within the same generation.
        if target < self.ordinal {
            self.rewind()?;
        }
        let mut remaining = budget.max_bytes;
        let mut framed = 0usize;
        loop {
            if framed >= budget.max_records || Instant::now() >= deadline {
                return Ok(SourceRecordProgress::Pending);
            }
            let advanced = self.advance(remaining, deadline, true)?;
            remaining = remaining.saturating_sub(advanced.consumed);
            match advanced.outcome {
                Outcome::Line { ordinal, bytes, .. } => {
                    if ordinal != target {
                        framed += 1;
                        continue;
                    }
                    return Ok(SourceRecordProgress::Ready(match bytes {
                        Some(bytes) => source_record(&bytes),
                        None => oversized_record(max_record_bytes),
                    }));
                }
                Outcome::Skipping { ordinal } => {
                    if ordinal != target {
                        framed += 1;
                        continue;
                    }
                    // The requested line is being discarded: keep indexing until it ends.
                }
                Outcome::Incomplete | Outcome::End => return Ok(SourceRecordProgress::Stale),
                Outcome::Damaged(damage) => {
                    return Ok(SourceRecordProgress::Ready(SourceRecord {
                        raw: String::new(),
                        display: String::new(),
                        unavailable: Some(damage.to_string()),
                    }));
                }
                Outcome::Exhausted => return Ok(SourceRecordProgress::Pending),
            }
        }
    }

    fn item(&self, physical: &Path, ordinal: u64, item: SourceItem) -> SourceRecordItem {
        SourceRecordItem {
            ordinal,
            locator: LocatorKind::CompressedRecord {
                path: physical.to_path_buf(),
                ordinal,
            },
            generation: self.generation,
            item,
        }
    }

    /// Frames until one line outcome, end of stream, or the allowance runs out.
    /// `remaining` strictly bounds the decompressed bytes this call may frame, so no
    /// single call expands more than its share of the source.
    fn advance(
        &mut self,
        remaining: usize,
        deadline: Instant,
        capture: bool,
    ) -> Result<Advanced, SourceError> {
        let mut remaining = remaining;
        let mut consumed = 0usize;
        loop {
            if remaining == 0 {
                return Ok(Advanced {
                    consumed,
                    outcome: Outcome::Exhausted,
                });
            }
            if self.pos == self.filled {
                if Instant::now() >= deadline {
                    return Ok(Advanced {
                        consumed,
                        outcome: Outcome::Exhausted,
                    });
                }
                match self
                    .decoder
                    .read(&mut self.chunk[..remaining.min(DECODE_CHUNK)])
                {
                    Ok(0) => {
                        self.filled = 0;
                        self.pos = 0;
                        self.finished = true;
                        let outcome = if self.framer.incomplete().is_some() {
                            Outcome::Incomplete
                        } else {
                            Outcome::End
                        };
                        return Ok(Advanced { consumed, outcome });
                    }
                    Ok(read) => {
                        self.filled = read;
                        self.pos = 0;
                    }
                    Err(error) => {
                        let damage = damage_of(&error);
                        self.damage = Some(damage);
                        return Ok(Advanced {
                            consumed,
                            outcome: Outcome::Damaged(damage),
                        });
                    }
                }
            }
            // Never frame more than the allowance, even when a chunk was read ahead:
            // a line split at the slice end simply resumes on the next call.
            let take = (self.filled - self.pos).min(remaining);
            let fed = self.framer.feed(&self.chunk[self.pos..self.pos + take]);
            debug_assert!(fed.consumed > 0, "a non-empty chunk always consumes bytes");
            self.pos += fed.consumed;
            consumed += fed.consumed;
            remaining -= fed.consumed;
            if fed.ready {
                let ordinal = self.ordinal;
                self.ordinal += 1;
                // Captured bytes are the record without its terminator, exactly like the
                // plain reader's `read_line_at`: one trailing CR of a CRLF line is gone.
                let bytes = if capture {
                    self.framer
                        .ready_line()
                        .map(|line| line.strip_suffix(b"\r").unwrap_or(line).to_vec())
                } else {
                    None
                };
                let record = self
                    .framer
                    .take_ready()
                    .expect("a fed line is ready for classification");
                return Ok(Advanced {
                    consumed,
                    outcome: Outcome::Line {
                        ordinal,
                        record,
                        bytes,
                    },
                });
            }
            if fed.discarded {
                return Ok(Advanced {
                    consumed,
                    outcome: Outcome::Skipping {
                        ordinal: self.ordinal,
                    },
                });
            }
        }
    }
}

/// Frames plain-file records for one slice.
///
/// Byte ceiling: framing is budgeted ([`JsonlCursor::next_record_within`]), so a slice
/// consumes at most `budget.max_bytes` file bytes. A line, oversized or not, is consumed
/// in slices and resumed by the next call, so an oversized line can never swallow a
/// whole slice's allowance in one record.
fn step_plain(
    physical: &Path,
    cursor: &mut JsonlCursor,
    ordinal: &mut u64,
    batch: &mut SourceBatch,
    budget: RefreshBudget,
    deadline: Instant,
) -> Result<(), SourceError> {
    loop {
        if batch.records.len() >= budget.max_records || Instant::now() >= deadline {
            return Ok(());
        }
        let remaining = budget.max_bytes.saturating_sub(batch.expanded_bytes);
        if remaining == 0 {
            return Ok(());
        }
        let rotations = cursor.rotation_count();
        let before = cursor.position();
        let chunk = cursor.next_record_within(remaining)?;
        // A rotation restarts the cursor at byte 0 during this call, so the old position
        // is not a usable baseline: everything consumed then belongs to the new source.
        let rotated = cursor.rotation_count() != rotations;
        let after = cursor.position();
        let consumed = if rotated {
            after
        } else {
            after.saturating_sub(before)
        };
        batch.expanded_bytes = batch
            .expanded_bytes
            .saturating_add(usize::try_from(consumed).unwrap_or(usize::MAX));
        if rotated {
            // The cursor restarted before this record. Keep the record, then hand the
            // caller a clean generation boundary instead of mixing two in one batch.
            batch.invalidated = true;
            *ordinal = 0;
        }
        let (record, done) = match chunk {
            // The allowance ended mid-line: the record continues on the next call.
            Chunk::Spent => {
                debug_assert!(batch.expanded_bytes >= budget.max_bytes);
                return Ok(());
            }
            Chunk::Eof(incomplete) => {
                // `next_record_within` samples the path only when it was already at end
                // of file. Sample once more so a rotation that happened during this
                // slice is still a new generation, even when the slice had not reached
                // the old end.
                cursor.refresh()?;
                if cursor.rotation_count() != rotations {
                    batch.invalidated = true;
                    *ordinal = 0;
                }
                (incomplete, true)
            }
            Chunk::Record(record) => (Some(record), false),
        };
        let Some(record) = record else {
            batch.done = true;
            return Ok(());
        };
        let (offset, item, framed) = match record {
            Record::Json { offset, value } => (offset, SourceItem::Json(value), true),
            Record::Invalid { offset } => (offset, SourceItem::Invalid, true),
            Record::TooLarge { offset } => (offset, SourceItem::TooLarge, true),
            Record::Incomplete { offset } => (offset, SourceItem::Incomplete, false),
            Record::Skipping { offset } => (offset, SourceItem::Skipping, false),
        };
        let generation = cursor.rotation_count();
        batch.records.push(SourceRecordItem {
            ordinal: *ordinal,
            locator: LocatorKind::FileOffset {
                path: physical.to_path_buf(),
                offset,
            },
            generation,
            item,
        });
        if framed {
            *ordinal += 1;
        }
        if done {
            batch.done = true;
            return Ok(());
        }
        if batch.invalidated {
            return Ok(());
        }
    }
}

/// The canonical identity of a rollout: the plain `.jsonl` path an archived
/// `.jsonl.zst` copy stands for. Only that exact suffix pair is stripped, and paths
/// are never resolved through the filesystem, so an alias cannot escape its directory.
pub fn canonical_rollout_identity(path: &Path) -> PathBuf {
    if !is_archived(path) {
        return path.to_path_buf();
    }
    let Some(stem) = path.file_stem() else {
        return path.to_path_buf();
    };
    let archived_rollout = Path::new(stem)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"));
    if archived_rollout {
        path.with_file_name(stem)
    } else {
        path.to_path_buf()
    }
}

/// Whether the path names a zstd archive.
fn is_archived(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("zst"))
}

fn source_record(bytes: &[u8]) -> SourceRecord {
    let raw = String::from_utf8_lossy(bytes).into_owned();
    let display = display_safe(&raw);
    SourceRecord {
        raw,
        display,
        unavailable: None,
    }
}

fn oversized_record(max_bytes: usize) -> SourceRecord {
    SourceRecord {
        raw: String::new(),
        display: String::new(),
        unavailable: Some(format!("record exceeds the {max_bytes}-byte limit")),
    }
}

fn damage_of(error: &io::Error) -> SourceDamage {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        SourceDamage::Truncated
    } else {
        SourceDamage::Corrupt
    }
}

fn deadline_of(budget: RefreshBudget) -> Instant {
    let now = Instant::now();
    now.checked_add(Duration::from_millis(budget.max_millis))
        .unwrap_or(now)
}
