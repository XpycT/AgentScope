//! Bounded, incremental reading of JSONL files without interpreting provider semantics.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

/// Streaming FNV-1a 64: split-independent, so hashing in different chunk sizes
/// agrees. Integrity check for "is this still the file I read", not cryptographic.
#[derive(Clone, Copy)]
pub struct ContentHash(u64);

impl ContentHash {
    pub fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 = (self.0 ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3);
        }
    }

    pub fn finish(&self) -> u64 {
        self.0
    }
}

impl Default for ContentHash {
    fn default() -> Self {
        Self::new()
    }
}

/// A complete JSON value or a content-free indication of an unusable line.
pub enum Record {
    Json {
        offset: u64,
        value: serde_json::Value,
    },
    Invalid {
        offset: u64,
    },
    TooLarge {
        offset: u64,
    },
    Incomplete {
        offset: u64,
    },
    /// An oversized line is being discarded and has not reached its terminator.
    /// Returned after each consumed chunk so the caller can apply its byte and
    /// time allowance; call `next_record` again to continue draining.
    Skipping {
        offset: u64,
    },
}

// Avoid leaking transcript content through routine debug output.
impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json { offset, .. } => f
                .debug_struct("Json")
                .field("offset", offset)
                .finish_non_exhaustive(),
            Self::Invalid { offset } => f.debug_struct("Invalid").field("offset", offset).finish(),
            Self::TooLarge { offset } => {
                f.debug_struct("TooLarge").field("offset", offset).finish()
            }
            Self::Incomplete { offset } => f
                .debug_struct("Incomplete")
                .field("offset", offset)
                .finish(),
            Self::Skipping { offset } => {
                f.debug_struct("Skipping").field("offset", offset).finish()
            }
        }
    }
}

/// Strongest portable file identity: Unix `(dev, ino)`; Windows `creation_time`.
/// Windows length and last-write time are tracked separately in [`Stamp`].
#[cfg(unix)]
type FileKey = (u64, u64);
#[cfg(windows)]
type FileKey = u64;
#[cfg(not(any(unix, windows)))]
type FileKey = ();

#[cfg(unix)]
fn file_key(meta: &fs::Metadata) -> FileKey {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(windows)]
fn file_key(meta: &fs::Metadata) -> FileKey {
    use std::os::windows::fs::MetadataExt;
    meta.creation_time()
}

#[cfg(not(any(unix, windows)))]
fn file_key(_meta: &fs::Metadata) -> FileKey {}

/// Identity plus the length and mtime observed at the last check. Shared with the
/// archived-source reader, which restarts once the observed stamp no longer matches.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stamp {
    key: FileKey,
    len: u64,
    modified: Option<SystemTime>,
}

impl Stamp {
    pub(crate) fn of(meta: &fs::Metadata) -> Self {
        Self {
            key: file_key(meta),
            len: meta.len(),
            modified: meta.modified().ok(),
        }
    }
}

/// One framing step: how many bytes of the chunk were consumed and what happened.
pub(crate) struct Fed {
    /// Bytes consumed from the chunk, including a terminator when one was framed.
    pub(crate) consumed: usize,
    /// A complete line, or the end of an oversized one, is ready in the framer.
    pub(crate) ready: bool,
    /// Bytes of an oversized line were discarded and it has not ended yet.
    pub(crate) discarded: bool,
}

/// Bounded line framing shared by the plain file cursor and the streaming compressed
/// reader. Bytes accumulate until a terminator; a line over `max_bytes` is discarded in
/// chunks so draining stays resumable and memory never grows with the source.
pub(crate) struct LineFramer {
    max_bytes: usize,
    buffer: Vec<u8>,
    /// Stream offset of the current line's first byte.
    line_start: u64,
    /// Stream offset of the next byte to be consumed.
    offset: u64,
    ready: bool,
    too_large: bool,
    incomplete_reported: bool,
}

impl LineFramer {
    pub(crate) fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            buffer: Vec::new(),
            line_start: 0,
            offset: 0,
            ready: false,
            too_large: false,
            incomplete_reported: false,
        }
    }

    /// Returns to the start of a new stream after a rotation or replacement.
    pub(crate) fn reset(&mut self) {
        self.buffer.clear();
        self.line_start = 0;
        self.offset = 0;
        self.ready = false;
        self.too_large = false;
        self.incomplete_reported = false;
    }

    /// Stream offset of the next unconsumed byte, including a held partial line.
    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }

    /// Stream offset of the line currently being framed.
    pub(crate) fn line_start(&self) -> u64 {
        self.line_start
    }

    /// Consumes bytes up to the next terminator, but never more than `limit`. A ready
    /// line must be taken with [`Self::take_ready`] before more bytes are fed.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Fed {
        self.feed_limited(chunk, usize::MAX)
    }

    /// [`Self::feed`] bounded by `limit` bytes: a window that ends mid-line consumes
    /// exactly `limit` bytes and leaves the rest of the line for the next call.
    pub(crate) fn feed_limited(&mut self, chunk: &[u8], limit: usize) -> Fed {
        debug_assert!(!self.ready, "a ready line must be taken before feeding");
        let chunk = &chunk[..chunk.len().min(limit)];
        if chunk.is_empty() {
            return Fed {
                consumed: 0,
                ready: false,
                discarded: false,
            };
        }
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        let data_len = newline.unwrap_or(chunk.len());
        let consumed = data_len + usize::from(newline.is_some());
        if !self.too_large {
            // One byte of slack so a CRLF terminator is not counted against the record.
            if data_len
                > self
                    .max_bytes
                    .saturating_add(1)
                    .saturating_sub(self.buffer.len())
            {
                self.buffer.clear();
                self.too_large = true;
            } else {
                self.buffer.extend_from_slice(&chunk[..data_len]);
            }
        }
        // A 2^64-byte stream is not a real source; saturate instead of failing a read.
        self.offset = self.offset.saturating_add(consumed as u64);
        self.incomplete_reported = false;
        self.ready = newline.is_some();
        Fed {
            consumed,
            ready: self.ready,
            discarded: newline.is_none() && self.too_large,
        }
    }

    /// Classifies the ready line and advances to the next one.
    pub(crate) fn take_ready(&mut self) -> Option<Record> {
        if !self.ready {
            return None;
        }
        let offset = self.line_start;
        let record = if self.too_large {
            Record::TooLarge { offset }
        } else {
            classify(&self.buffer, offset, self.max_bytes)
        };
        self.buffer.clear();
        self.too_large = false;
        self.ready = false;
        self.line_start = self.offset;
        Some(record)
    }

    /// The ready line's bytes without their terminator; `None` while an oversized
    /// line is being discarded.
    pub(crate) fn ready_line(&self) -> Option<&[u8]> {
        (self.ready && !self.too_large).then_some(self.buffer.as_slice())
    }

    /// Reports a held partial or still-draining line once, at end of stream.
    pub(crate) fn incomplete(&mut self) -> Option<Record> {
        if !(self.too_large || !self.buffer.is_empty()) || self.incomplete_reported {
            return None;
        }
        self.incomplete_reported = true;
        Some(Record::Incomplete {
            offset: self.line_start,
        })
    }
}

/// Outcome of one budgeted framing call. See [`JsonlCursor::next_record_within`].
pub(crate) enum Chunk {
    /// A complete line, the end of an oversized line, or one discarded chunk.
    Record(Record),
    /// The byte allowance was spent mid-line; call again to continue it.
    Spent,
    /// End of file; the optional record is the one-time `Incomplete` report.
    Eof(Option<Record>),
}

/// Incremental line reader over a growing file. A trailing partial line is held and
/// retried once more bytes arrive. Rotation (identity change, shrink, or an mtime change
/// without growth) resets the cursor to the start of the file and is counted.
///
/// Known false negative: replacing a file in place with same-length content and the same
/// mtime (and, on Windows, the same creation time) is indistinguishable from no change.
pub struct JsonlCursor {
    path: PathBuf,
    reader: BufReader<File>,
    max_bytes: usize,
    framer: LineFramer,
    at_eof: bool,
    stamp: Stamp,
    rotations: u64,
    hash: ContentHash,
    /// Resumable completion scan for an oversized line a locator read started.
    scan: Option<ScanState>,
}

/// Where an interrupted oversized-line completion scan stopped.
struct ScanState {
    /// Byte offset of the line the scan belongs to.
    line: u64,
    /// Next physical byte to examine.
    position: u64,
}

fn classify(line: &[u8], offset: u64, max_bytes: usize) -> Record {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    if line.len() > max_bytes {
        return Record::TooLarge { offset };
    }
    crate::perf::record_parsed();
    match serde_json::from_slice(line) {
        Ok(value) => Record::Json { offset, value },
        Err(_) => Record::Invalid { offset },
    }
}

impl JsonlCursor {
    pub fn open(path: PathBuf, max_record_bytes: usize) -> io::Result<Self> {
        let file = File::open(&path)?;
        let stamp = Stamp::of(&file.metadata()?);
        Ok(Self {
            path,
            reader: BufReader::new(file),
            max_bytes: max_record_bytes,
            framer: LineFramer::new(max_record_bytes),
            at_eof: false,
            stamp,
            rotations: 0,
            hash: ContentHash::new(),
            scan: None,
        })
    }

    /// Bytes consumed from the file so far, including a held partial line.
    pub fn position(&self) -> u64 {
        self.framer.offset()
    }

    /// Hash of every byte consumed so far (including a held partial line), reset on
    /// rotation. Right after a record is returned it covers exactly `[0, position)`.
    pub fn content_hash(&self) -> u64 {
        self.hash.finish()
    }

    /// Number of times the source was detected as rotated or truncated and re-read.
    pub fn rotation_count(&self) -> u64 {
        self.rotations
    }

    /// Whether the current path still names the file held by this cursor.
    pub(crate) fn same_file(&self) -> io::Result<bool> {
        let held = same_file::Handle::from_file(self.reader.get_ref().try_clone()?)?;
        let current = same_file::Handle::from_path(&self.path)?;
        Ok(held == current)
    }

    /// Re-checks the path for replacement or truncation without consuming a byte.
    /// [`Self::next_record`] does this at end of file; a locator read has to do it
    /// before serving bytes it may no longer own.
    pub(crate) fn refresh(&mut self) -> io::Result<()> {
        self.check_rotation()
    }

    fn check_rotation(&mut self) -> io::Result<()> {
        // Sample the path first, then the held handle: a writer appending between the two
        // samples can only make the handle look larger, never the path.
        let current = Stamp::of(&fs::metadata(&self.path)?);
        let held = Stamp::of(&self.reader.get_ref().metadata()?);
        let rotated = path_replaced(current, held, self.framer.offset())
            || (current.len == self.stamp.len && current.modified != self.stamp.modified);
        if rotated {
            // Open first; commit stamp and handle together so a failed reopen is retried.
            let file = File::open(&self.path)?;
            let stamp = Stamp::of(&file.metadata()?);
            self.stamp = stamp;
            self.reader = BufReader::new(file);
            self.framer.reset();
            self.rotations += 1;
            self.hash = ContentHash::new();
            self.scan = None;
        } else {
            self.stamp = current;
        }
        Ok(())
    }

    /// One framing call that consumes at most `max_bytes` of file bytes.
    ///
    /// Unlike [`Self::next_record`], which feeds whole buffered chunks, this lets a
    /// caller bound its own work: a line — oversized or not — is consumed in slices and
    /// resumed by the next call.
    pub(crate) fn next_record_within(&mut self, max_bytes: usize) -> io::Result<Chunk> {
        if self.at_eof {
            self.check_rotation()?;
            self.at_eof = false;
        }
        let mut remaining = max_bytes;
        loop {
            if remaining == 0 {
                return Ok(Chunk::Spent);
            }
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                self.at_eof = true;
                return Ok(Chunk::Eof(self.framer.incomplete()));
            }
            let fed = self.framer.feed_limited(available, remaining);
            debug_assert!(fed.consumed > 0, "a non-empty window consumes bytes");
            crate::perf::bytes_read(fed.consumed);
            self.hash.update(&available[..fed.consumed]);
            self.reader.consume(fed.consumed);
            remaining -= fed.consumed;
            if fed.ready {
                let record = self
                    .framer
                    .take_ready()
                    .expect("a fed line is ready for classification");
                return Ok(Chunk::Record(record));
            }
            if fed.discarded {
                // Yield after every discarded chunk so draining stays resumable.
                return Ok(Chunk::Record(Record::Skipping {
                    offset: self.framer.line_start(),
                }));
            }
        }
    }

    /// Returns one outcome. A partial line is retained and retried when more bytes arrive.
    pub fn next_record(&mut self) -> io::Result<Option<Record>> {
        loop {
            match self.next_record_within(usize::MAX)? {
                Chunk::Record(record) => return Ok(Some(record)),
                Chunk::Eof(record) => return Ok(record),
                // Only a finite allowance can be spent before a record completes.
                Chunk::Spent => continue,
            }
        }
    }

    /// Reads the line starting at `offset` (without its terminator), truncated to
    /// `max_bytes`. Returns `None` at or past end of file. Does not move the cursor.
    pub fn read_at(&mut self, offset: u64, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
        let mut file = BufReader::new(File::open(&self.path)?);
        if offset >= file.get_ref().metadata()?.len() {
            return Ok(None);
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut line = Vec::new();
        (&mut file)
            .take(max_bytes as u64)
            .read_until(b'\n', &mut line)?;
        crate::perf::bytes_read(line.len());
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Ok(Some(line))
    }

    /// Reads the complete record that starts exactly at `offset`, without its terminator.
    ///
    /// A capped window answers the common case in one pass. When the line continues past
    /// it, completion is proven with a forward scan bounded by `budget`; an interrupted
    /// scan resumes on the next call for the same offset, so no single call reads an
    /// arbitrarily long unterminated line. Does not move the sequential cursor.
    pub(crate) fn read_line(
        &mut self,
        offset: u64,
        max_bytes: usize,
        budget: ScanBudget,
    ) -> io::Result<LineLookup> {
        if budget.bytes == 0 {
            return Ok(LineLookup::Pending);
        }
        let mut position = match self.scan.take() {
            // Same locator: continue where the previous call's allowance ran out.
            Some(state) if state.line == offset => state.position,
            _ => {
                let mut file = File::open(&self.path)?;
                let len = file.metadata()?.len();
                if offset >= len {
                    return Ok(LineLookup::Absent);
                }
                if offset > 0 {
                    file.seek(SeekFrom::Start(offset - 1))?;
                    let mut previous = [0u8; 1];
                    file.read_exact(&mut previous)?;
                    if previous[0] != b'\n' {
                        return Ok(LineLookup::NotBoundary);
                    }
                }
                file.seek(SeekFrom::Start(offset))?;
                // One byte past the cap for the CR of a CRLF line, one more to prove
                // whether the line continues rather than ending at the cap with a `\r`.
                let limit = (max_bytes as u64).saturating_add(2);
                let mut reader = BufReader::new(file);
                let mut line = Vec::new();
                (&mut reader).take(limit).read_until(b'\n', &mut line)?;
                crate::perf::bytes_read(line.len());
                if line.last() == Some(&b'\n') {
                    line.pop();
                    if line.last() == Some(&b'\r') {
                        line.pop();
                    }
                    let bytes = (line.len() <= max_bytes).then_some(line);
                    return Ok(LineLookup::Complete(bytes));
                }
                if offset + line.len() as u64 >= len {
                    // The window reached the end of the file: no record here.
                    return Ok(LineLookup::Absent);
                }
                offset + line.len() as u64
            }
        };

        // The window held no terminator, so the line is longer than the cap: prove that
        // it ends before reporting an oversized record, within this call's allowance.
        let mut file = File::open(&self.path)?;
        file.seek(SeekFrom::Start(position))?;
        let mut reader = BufReader::new(file);
        let mut scratch = vec![0u8; 64 * 1024];
        let mut allowance = budget.bytes;
        loop {
            if allowance == 0 || Instant::now() >= budget.deadline {
                self.scan = Some(ScanState {
                    line: offset,
                    position,
                });
                return Ok(LineLookup::Pending);
            }
            let want = allowance.min(scratch.len());
            let read = reader.read(&mut scratch[..want])?;
            if read == 0 {
                // The line never ends: a trailing partial is not a record, however long.
                return Ok(LineLookup::Absent);
            }
            crate::perf::bytes_read(read);
            position += read as u64;
            allowance -= read;
            if scratch[..read].contains(&b'\n') {
                return Ok(LineLookup::Complete(None));
            }
        }
    }

    /// Returns up to `max_records` of the newest complete lines, oldest first, examining at
    /// most about `max_bytes` from the end of the file. A partial trailing line and a
    /// partial leading line are excluded. Does not move the cursor.
    pub fn tail_records(
        &mut self,
        max_records: usize,
        max_bytes: usize,
    ) -> io::Result<Vec<Record>> {
        let mut file = File::open(&self.path)?;
        let len = file.metadata()?.len();
        let start = len.saturating_sub(max_bytes as u64);
        let read_from = start.saturating_sub(1);
        file.seek(SeekFrom::Start(read_from))?;
        let mut data = Vec::new();
        file.take(len - read_from).read_to_end(&mut data)?;
        crate::perf::bytes_read(data.len());

        Ok(tail_from_window(
            data,
            read_from,
            start > 0,
            max_records,
            self.max_bytes,
        ))
    }
}

/// Remaining work allowance for one locator read.
pub(crate) struct ScanBudget {
    /// Bytes the completion scan may read in this call.
    pub(crate) bytes: usize,
    /// Wall-clock limit for this call.
    pub(crate) deadline: Instant,
}

/// Outcome of reading the record at an exact byte offset.
pub(crate) enum LineLookup {
    /// A complete line; `bytes` is `None` when it is longer than the record cap.
    Complete(Option<Vec<u8>>),
    /// The allowance ran out before completion was known; call again to continue.
    Pending,
    /// No complete record there: past the end, or a line that never ends.
    Absent,
    /// `offset` is not a line start.
    NotBoundary,
}

/// True if the path (sampled before `held`) no longer refers to the held file. Growth is
/// not replacement; this also catches Windows "tunneled" replacements that keep the
/// creation time but are larger than the file we hold.
pub(crate) fn path_replaced(path: Stamp, held: Stamp, consumed: u64) -> bool {
    path.key != held.key
        || path.len < consumed
        || path.len > held.len
        || (path.len == held.len && path.modified != held.modified)
}

/// Parses complete lines from a window read at `read_from`. When `has_lead`, `data[0]` is
/// the byte before the window proper. A short or empty buffer (the file was truncated
/// while being read) yields no records rather than panicking.
fn tail_from_window(
    mut data: Vec<u8>,
    read_from: u64,
    has_lead: bool,
    max_records: usize,
    max_record_bytes: usize,
) -> Vec<Record> {
    let mut base = read_from;
    if has_lead {
        match data.first() {
            None => return Vec::new(),
            Some(b'\n') => {
                data.remove(0);
                base += 1;
            }
            Some(_) => {
                let Some(pos) = data.iter().position(|&b| b == b'\n') else {
                    return Vec::new();
                };
                data.drain(..=pos);
                base += pos as u64 + 1;
            }
        }
    }
    let Some(end) = data.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    let mut records = Vec::new();
    let mut line_start = 0usize;
    for (i, _) in data[..=end]
        .iter()
        .enumerate()
        .filter(|(_, b)| **b == b'\n')
    {
        records.push(classify(
            &data[line_start..i],
            base + line_start as u64,
            max_record_bytes,
        ));
        line_start = i + 1;
    }
    let skip = records.len().saturating_sub(max_records);
    records.drain(..skip);
    records
}

#[cfg(test)]
mod tests {
    use super::{FileKey, LineFramer, Record, Stamp, path_replaced, tail_from_window};
    use std::time::{Duration, SystemTime};

    #[test]
    fn framer_holds_a_partial_line_and_reports_it_once() {
        let mut framer = LineFramer::new(1024);
        let fed = framer.feed(b"{\"a\":1}");
        assert_eq!((fed.consumed, fed.ready, fed.discarded), (7, false, false));
        assert_eq!(framer.offset(), 7);
        assert_eq!(framer.line_start(), 0);
        assert!(matches!(
            framer.incomplete(),
            Some(Record::Incomplete { offset: 0 })
        ));
        assert!(
            framer.incomplete().is_none(),
            "the partial line is reported once"
        );
        assert!(framer.take_ready().is_none(), "nothing framed yet");

        let fed = framer.feed(b"\n{\"b\":2}\n");
        assert!(fed.ready);
        assert!(matches!(
            framer.take_ready(),
            Some(Record::Json { offset: 0, .. })
        ));
        // Two lines arrived in one chunk; the framer only consumed the first.
        assert_eq!(framer.offset(), 8);
        assert_eq!(framer.line_start(), 8);
        let fed = framer.feed(b"{\"b\":2}\n");
        assert!(fed.ready);
        assert!(matches!(
            framer.take_ready(),
            Some(Record::Json { offset: 8, .. })
        ));
        assert_eq!(framer.offset(), 16);
    }

    #[test]
    fn framer_allows_one_terminator_byte_over_the_cap_and_drains_beyond_it() {
        let mut framer = LineFramer::new(8);
        // CRLF content of exactly the cap is not oversized.
        assert!(!framer.feed(b"xxxxxxxx\r").discarded);
        assert!(framer.feed(b"\n").ready);
        assert!(matches!(
            framer.take_ready(),
            Some(Record::Invalid { offset: 0 })
        ));

        // One byte more than the cap plus CRLF is discarded, chunk by chunk.
        let mut skipped = 0usize;
        for chunk in [&b"yyyyyyyyy0123"[..], &b"abc"[..], &b"\n"[..]] {
            let fed = framer.feed(chunk);
            assert!(fed.consumed > 0);
            if fed.ready {
                assert!(matches!(
                    framer.take_ready(),
                    Some(Record::TooLarge { offset: 10 })
                ));
            } else {
                assert!(fed.discarded);
                skipped += 1;
            }
        }
        assert_eq!(skipped, 2, "draining yields once per discarded chunk");
        assert!(framer.ready_line().is_none());
    }

    #[test]
    fn framer_reset_restarts_offsets_and_drops_held_bytes() {
        let mut framer = LineFramer::new(16);
        framer.feed(b"{\"a\":1}");
        assert_eq!(framer.offset(), 7);
        framer.reset();
        assert_eq!((framer.offset(), framer.line_start()), (0, 0));
        assert!(framer.incomplete().is_none(), "the held line was dropped");
        assert!(framer.feed(b"{\"b\":2}\n").ready);
        assert!(matches!(
            framer.take_ready(),
            Some(Record::Json { offset: 0, .. })
        ));
    }

    #[test]
    fn append_between_samples_is_not_a_replacement() {
        let key: FileKey = Default::default();
        let t = SystemTime::UNIX_EPOCH;
        let path = Stamp {
            key,
            len: 10,
            modified: Some(t),
        };
        let held = Stamp {
            key,
            len: 15,
            modified: Some(t + Duration::from_secs(1)),
        };
        assert!(!path_replaced(path, held, 10));
        // A larger path than the held file is a replacement.
        assert!(path_replaced(held, path, 10));
        // Shrink below the consumed offset is a replacement.
        assert!(path_replaced(path, held, 11));
    }

    #[test]
    fn truncated_tail_window_returns_nothing_instead_of_panicking() {
        assert!(tail_from_window(Vec::new(), 99, true, 5, 1024).is_empty());
        assert!(tail_from_window(b"x".to_vec(), 99, true, 5, 1024).is_empty());
    }
}
