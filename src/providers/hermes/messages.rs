//! Projected Hermes transcript and in-place compaction evidence.
//!
//! Hermes has two compression modes. A *rotation* ends one session with
//! `end_reason='compression'` and continues in a new session row; an *in-place*
//! compaction keeps one session id, soft-archives the summarized rows
//! (`active=0, compacted=1`), marks superseded copies `active=0, compacted=0`
//! (a rewind or a carried duplicate), and inserts fresh copies plus a
//! `_compressed_summary` carrier as `active=1`.
//!
//! The display projection therefore reads `active=1 OR compacted=1`, collapses
//! proven copies (stored `display_identity` and `message_uid`, never recomputed
//! content hashes), prefers the active then the latest physical copy, and first
//! removes every identity carried by a `active=0, compacted=0` row **of any
//! member**. That tombstone pass is what stops a rewind in a compression child
//! from resurrecting the archived parent copy, while unrelated archived history
//! stays visible. `display_order` is the native display position and falls back
//! to the physical row id without ever backfilling the producer's index.
//!
//! Every read is bounded by the caller's slice and by fixed index limits; no
//! statement writes, migrates, repairs or copies the database.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::lineage::HermesRowMeta;
use super::schema::{HermesShape, MAX_CONFIG_BYTES, epoch_ms, short};
use super::usage::HermesModelTotals;
use crate::domain::{
    BoundedText, CompactionObservation, CompactionScope, Event, EventKind, LocatorKind, PageAnchor,
    PageDirection, PageRequest, Provenance, RefreshBudget, SessionKey, SourceKey, SourceLocator,
    Timestamp, ToolInteraction, ToolStatus, TranscriptPage,
};
use crate::ingest::text::{display_safe, preview};
use crate::providers::claude::{event_anchor, event_size, fit_event};
use crate::providers::hermes::discovery::{busy_for_deadline, set_busy, with_deadline_bound};
use crate::providers::sqlite_guard::map_error;
use crate::providers::{ProviderError, ReadProgress, SourceRecord, WorkProgress};

/// Indexed display messages one projection may retain.
const ROW_CAP: usize = 1_000_000;
/// Approximate retained index bytes one projection may hold.
const INDEX_BYTES: usize = 64 * 1024 * 1024;
/// Rows fetched per member batch.
const BATCH: usize = 64;
/// Longest text/JSON cell carried out of SQLite per display row.
const TEXT_MAX: usize = 16 * 1024;
const TOOL_JSON_MAX: usize = 64 * 1024;
const UID_MAX: usize = 256;
const IDENTITY_MAX: usize = 64;
/// Content blocks (assistant text plus tool calls) one message may project.
pub(crate) const BLOCKS_MAX: usize = 16;
/// Display preview bounds of one text field.
const TEXT_PREVIEW: usize = 4096;
/// Carrier rows read per compaction scan (per member).
/// Carrier rows read per compaction scan (per member).
pub(crate) const CARRIER_SCAN: usize = 256;
const CARRIER_BATCH: usize = CARRIER_SCAN;

fn over() -> ProviderError {
    ProviderError::Unsupported(
        "Hermes transcript exceeds the bounded display index or metadata limit".into(),
    )
}

/// True when an integer flag cell is a stored non-zero integer.
fn flag(expr: &str) -> String {
    format!("CASE WHEN typeof({expr}) = 'integer' AND {expr} <> 0 THEN 1 ELSE 0 END")
}

fn text_cell(present: bool, expr: &str, max: usize) -> String {
    if present {
        format!("CASE WHEN typeof({expr}) = 'text' THEN SUBSTR({expr}, 1, {max}) END")
    } else {
        "NULL".to_string()
    }
}

fn cell_bytes(present: bool, expr: &str) -> String {
    if present {
        format!("CASE WHEN typeof({expr}) = 'text' THEN length(CAST({expr} AS BLOB)) ELSE 0 END")
    } else {
        "0".to_string()
    }
}

fn identity_cell(shape: &HermesShape) -> String {
    if shape.message_display_identity {
        format!(
            "CASE WHEN typeof(display_identity) = 'blob' \
             AND length(display_identity) BETWEEN 1 AND {IDENTITY_MAX} THEN display_identity END"
        )
    } else {
        "NULL".to_string()
    }
}

fn uid_cell(shape: &HermesShape) -> String {
    text_cell(shape.message_uid, "message_uid", UID_MAX)
}

fn active_cell(shape: &HermesShape) -> String {
    if shape.message_active {
        flag("active")
    } else {
        "1".to_string()
    }
}

fn compacted_cell(shape: &HermesShape) -> String {
    if shape.message_compacted {
        flag("compacted")
    } else {
        "0".to_string()
    }
}

fn summary_cell(shape: &HermesShape) -> String {
    if shape.message_summary {
        flag("_compressed_summary")
    } else {
        "0".to_string()
    }
}

/// A row that the display projection may paint: `active=1 OR compacted=1`, and
/// not a model-only carrier. Unknown/missing display metadata never hides a row.
fn display_where(shape: &HermesShape) -> String {
    let live = if shape.message_active || shape.message_compacted {
        format!(
            "({} = 1 OR {} = 1)",
            active_cell(shape),
            compacted_cell(shape)
        )
    } else {
        "1".to_string()
    };
    let visible = if shape.message_display_metadata {
        format!(
            "CASE WHEN typeof(display_metadata) = 'text' \
             AND length(CAST(display_metadata AS BLOB)) <= {MAX_CONFIG_BYTES} \
             AND json_valid(display_metadata) \
             THEN (COALESCE(json_extract(display_metadata, '$.model_only'), 0) = 0) \
             ELSE 1 END"
        )
    } else {
        "1".to_string()
    };
    format!("{live} AND ({visible} = 1)")
}

fn display_sql(shape: &HermesShape) -> String {
    format!(
        "SELECT id, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} \
         FROM messages WHERE session_id = ?1 AND id > ?2 AND ({}) ORDER BY id LIMIT ?3",
        text_cell(true, "role", 64),
        text_cell(true, "content", TEXT_MAX),
        cell_bytes(true, "content"),
        text_cell(shape.message_tool_call_id, "tool_call_id", 256),
        text_cell(shape.message_tool_calls, "tool_calls", TOOL_JSON_MAX),
        cell_bytes(shape.message_tool_calls, "tool_calls"),
        text_cell(shape.message_tool_name, "tool_name", 128),
        "CASE WHEN typeof(timestamp) IN ('integer','real') THEN timestamp END",
        active_cell(shape),
        compacted_cell(shape),
        summary_cell(shape),
        identity_cell(shape),
        if shape.message_display_order {
            "CASE WHEN typeof(display_order) = 'integer' THEN display_order END".to_string()
        } else {
            "NULL".to_string()
        },
        uid_cell(shape),
        display_where(shape),
    )
}

fn tombstone_sql(shape: &HermesShape) -> String {
    format!(
        "SELECT id, {}, {} FROM messages WHERE session_id = ?1 AND id > ?2 \
         AND {} = 0 AND {} = 0 AND (({}) IS NOT NULL OR ({}) IS NOT NULL) \
         ORDER BY id LIMIT ?3",
        identity_cell(shape),
        uid_cell(shape),
        active_cell(shape),
        compacted_cell(shape),
        identity_cell(shape),
        uid_cell(shape),
    )
}

/// One physical display row, before copies are folded.
#[derive(Clone)]
struct Row {
    id: i64,
    member: usize,
    /// `COALESCE(display_order, id)`, clamped to the 48 bits an order key carries.
    key: i64,
    active: bool,
    summary: bool,
    identity: Option<Vec<u8>>,
    uid: Option<String>,
    blocks: usize,
}

/// The proven logical identity of a row, used to fold copies.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum Identity {
    Hash(Vec<u8>),
    Uid(String),
    Row(usize, i64),
}

struct Group {
    identity: Identity,
    member: usize,
    key: i64,
    first_id: i64,
    representative: Row,
    blocks: usize,
    suppressed: bool,
    carrier: bool,
    identities: Vec<Identity>,
    last_display: (usize, i64),
    logical_id: String,
}

/// Resumable projection state: tombstones first, then display rows, then fold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Tombstones,
    Display,
    Fold,
    Ready,
}

pub(crate) struct Projection {
    members: Vec<SessionKey>,
    database: PathBuf,
    generation: u64,
    shape: HermesShape,
    phase: Phase,
    member: usize,
    cursor: i64,
    /// Last `(member, id)` position of each removed (`active=0, compacted=0`)
    /// identity. A removed copy suppresses an identity only when no display copy
    /// follows it, exactly like the producer's newest-copy rule.
    removed_hashes: HashMap<Vec<u8>, (usize, i64)>,
    removed_uids: HashMap<String, (usize, i64)>,
    rows: Vec<Row>,
    uid_group: HashMap<String, Identity>,
    groups: Vec<Group>,
    by_id: HashMap<String, usize>,
    suffix: Vec<usize>,
    bytes: usize,
    row_cap: usize,
    byte_cap: usize,
    over: bool,
    version: Option<String>,
    last_read: Option<(i64, ParsedMessage)>,
}

/// A row read back for one event.
struct ParsedMessage {
    role: String,
    content: Option<String>,
    content_len: usize,
    tool_call_id: Option<String>,
    tool_calls: Option<String>,
    tool_calls_len: usize,
    tool_name: Option<String>,
    timestamp: Option<Timestamp>,
}

impl Projection {
    pub(crate) fn new(
        members: &[SessionKey],
        database: &Path,
        generation: u64,
        shape: HermesShape,
        version: Option<String>,
    ) -> Self {
        Self {
            members: members.to_vec(),
            database: database.to_path_buf(),
            generation,
            shape,
            phase: if shape.message_active && shape.message_compacted {
                Phase::Tombstones
            } else {
                Phase::Display
            },
            member: 0,
            cursor: -1,
            removed_hashes: HashMap::new(),
            removed_uids: HashMap::new(),
            rows: Vec::new(),
            uid_group: HashMap::new(),
            groups: Vec::new(),
            by_id: HashMap::new(),
            suffix: Vec::new(),
            bytes: 0,
            row_cap: ROW_CAP,
            byte_cap: INDEX_BYTES,
            over: false,
            version,
            last_read: None,
        }
    }

    /// Test-only limits, so a small synthetic source can prove the caps.
    #[cfg(test)]
    fn with_caps(mut self, row_cap: usize, byte_cap: usize) -> Self {
        self.row_cap = row_cap;
        self.byte_cap = byte_cap;
        self
    }

    pub(crate) fn ready(&self) -> bool {
        self.phase == Phase::Ready
    }

    /// One bounded slice. A tiny slice leaves the phase and cursor untouched, so
    /// the next call continues instead of restarting.
    pub(crate) fn step(
        &mut self,
        conn: &Connection,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<WorkProgress>, ProviderError> {
        if self.over {
            return Err(over());
        }
        if self.phase == Phase::Ready {
            return Ok(ReadProgress::Ready(WorkProgress::default()));
        }
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(budget.max_millis.max(1));
        let mut records = budget.max_records.max(1);
        let mut bytes = budget.max_bytes.max(1);
        let mut progress = WorkProgress::default();
        loop {
            if records == 0 || bytes == 0 || std::time::Instant::now() >= deadline {
                return Ok(ReadProgress::Pending(progress));
            }
            if self.phase == Phase::Fold {
                self.fold()?;
                self.phase = Phase::Ready;
                return Ok(ReadProgress::Ready(progress));
            }
            if self.member >= self.members.len() {
                self.member = 0;
                self.cursor = -1;
                self.phase = match self.phase {
                    Phase::Tombstones => Phase::Display,
                    Phase::Display => Phase::Fold,
                    other => other,
                };
                continue;
            }
            let limit = records.min(BATCH) as i64;
            let sql = match self.phase {
                Phase::Tombstones => tombstone_sql(&self.shape),
                _ => display_sql(&self.shape),
            };
            let session = &self.members[self.member];
            let member = self.member;
            let mut fetched = 0usize;
            let mut stopped = false;
            let mut cursor = self.cursor;
            {
                let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
                let mut query = statement
                    .query(rusqlite::params![session.native_id, cursor, limit])
                    .map_err(map_error)?;
                while let Some(row) = query.next().map_err(map_error)? {
                    let id: i64 = row.get(0).map_err(map_error)?;
                    cursor = id;
                    fetched += 1;
                    // Per-row accounting: a previous row's bytes must not be
                    // subtracted again or let a later row skip the cap.
                    let mut spent = 96usize;
                    match self.phase {
                        Phase::Tombstones => {
                            let identity: Option<Vec<u8>> = row.get(1).map_err(map_error)?;
                            let uid: Option<String> = row.get(2).map_err(map_error)?;
                            spent += identity.as_ref().map_or(0, Vec::len);
                            spent += uid.as_ref().map_or(0, String::len);
                            let tombstones = self.removed_hashes.len() + self.removed_uids.len();
                            if tombstones >= self.row_cap
                                || self.bytes.saturating_add(spent) > self.byte_cap
                            {
                                self.over = true;
                                return Err(over());
                            }
                            self.bytes = self.bytes.saturating_add(spent);
                            if let Some(identity) = identity {
                                let position =
                                    self.removed_hashes.entry(identity).or_insert((member, id));
                                if (member, id) > *position {
                                    *position = (member, id);
                                }
                            }
                            if let Some(uid) = uid.filter(|uid| !uid.is_empty()) {
                                let position = self.removed_uids.entry(uid).or_insert((member, id));
                                if (member, id) > *position {
                                    *position = (member, id);
                                }
                            }
                        }
                        _ => {
                            let parsed = scan_row(row)?;
                            spent += parsed.message.byte_len();
                            self.last_read = None;
                            let blocks = blocks_of(&parsed.message);
                            if self.rows.len() >= self.row_cap
                                || self.bytes.saturating_add(spent) > self.byte_cap
                            {
                                self.over = true;
                                return Err(over());
                            }
                            self.bytes = self.bytes.saturating_add(spent);
                            self.rows.push(Row {
                                id,
                                member,
                                key: parsed.order.unwrap_or(id),
                                active: parsed.active,
                                summary: parsed.summary,
                                identity: parsed.identity,
                                uid: parsed.uid.filter(|uid| !uid.is_empty()),
                                blocks,
                            });
                        }
                    }
                    records = records.saturating_sub(1);
                    bytes = bytes.saturating_sub(spent);
                    progress.records += 1;
                    progress.expanded_bytes += spent;
                    if records == 0 || bytes == 0 {
                        // A budget stop is not query exhaustion: keep the member
                        // and cursor so later rows are never silently omitted.
                        stopped = true;
                        break;
                    }
                }
            }
            self.cursor = cursor;
            if fetched < limit as usize && !stopped {
                self.member += 1;
                self.cursor = -1;
            }
        }
    }

    /// Folds copies by proven identity, applies tombstones, orders by native
    /// display position and computes the suffix event counts.
    fn fold(&mut self) -> Result<(), ProviderError> {
        let mut groups: HashMap<Identity, Group> = HashMap::new();
        let mut order: Vec<(usize, i64, i64, Identity)> = Vec::new();
        let rows = std::mem::take(&mut self.rows);
        for row in rows {
            let hash = row.identity.clone().map(Identity::Hash);
            let uid = row.uid.clone().map(Identity::Uid);
            // A summary carrier folds onto the first copy with its message_uid;
            // every other row keys by its stored identity.
            let key = if row.summary {
                row.uid
                    .as_ref()
                    .and_then(|uid| self.uid_group.get(uid).cloned())
                    .or(hash.clone())
                    .or(uid.clone())
                    .unwrap_or(Identity::Row(row.member, row.id))
            } else {
                hash.clone()
                    .or(uid.clone())
                    .unwrap_or(Identity::Row(row.member, row.id))
            };
            if let Some(uid) = &row.uid {
                self.uid_group.entry(uid.clone()).or_insert(key.clone());
            }
            let mut identities = Vec::new();
            if let Some(identity) = &row.identity {
                identities.push(Identity::Hash(identity.clone()));
            }
            if let Some(uid) = &row.uid {
                identities.push(Identity::Uid(uid.clone()));
            }
            let position = (row.member, row.id);
            let (row_member, row_key, row_id) = (row.member, row.key, row.id);
            match groups.get_mut(&key) {
                Some(group) => {
                    group.carrier |= row.summary;
                    group.last_display = group.last_display.max(position);
                    for identity in identities {
                        if !group.identities.contains(&identity) {
                            group.identities.push(identity);
                        }
                    }
                    if (row.active, row.id) > (group.representative.active, group.representative.id)
                    {
                        group.representative = row;
                        group.blocks = group.blocks.max(group.representative.blocks);
                    }
                    if (row_member, row_key, row_id) < (group.member, group.key, group.first_id) {
                        group.member = row_member;
                        group.key = row_key;
                        group.first_id = row_id;
                    }
                }
                None => {
                    order.push((row_member, row_key, row_id, key.clone()));
                    groups.insert(
                        key.clone(),
                        Group {
                            identity: key.clone(),
                            member: row.member,
                            key: row.key,
                            first_id: row.id,
                            representative: row.clone(),
                            blocks: row.blocks,
                            suppressed: false,
                            carrier: row.summary,
                            identities,
                            last_display: position,
                            logical_id: String::new(),
                        },
                    );
                }
            }
        }
        for group in groups.values_mut() {
            group.suppressed = group.identities.iter().any(|identity| {
                let removed = match identity {
                    Identity::Hash(hash) => self.removed_hashes.get(hash),
                    Identity::Uid(uid) => self.removed_uids.get(uid),
                    Identity::Row(..) => None,
                };
                removed.is_some_and(|removed| *removed > group.last_display)
            });
        }
        order.sort_by_key(|(member, key, id, _)| (*member, *key, *id));
        let mut visible = Vec::with_capacity(order.len());
        for (_, _, _, identity) in order {
            if let Some(group) = groups.remove(&identity)
                && !group.suppressed
            {
                visible.push(group);
            }
        }
        self.by_id.clear();
        // The event id is the logical message's proven identity, not the physical
        // row that happens to be its representative: a compaction may remove that
        // copy while a new active copy keeps the same identity and anchor.
        for group in &mut visible {
            group.logical_id = match &group.identity {
                Identity::Hash(hash) => format!("h{}", hex(hash)),
                Identity::Uid(uid) => format!("u{}", hex(uid.as_bytes())),
                Identity::Row(_, id) => format!("r{id}"),
            };
        }
        for (at, group) in visible.iter().enumerate() {
            self.by_id.insert(group.logical_id.clone(), at);
        }
        let mut suffix = vec![0usize; visible.len() + 1];
        for at in (0..visible.len()).rev() {
            suffix[at] = suffix[at + 1].saturating_add(visible[at].blocks);
        }
        self.groups = visible;
        self.suffix = suffix;
        Ok(())
    }

    /// Events after `anchor` in native display order (unbounded count).
    pub(crate) fn count_after(&self, anchor: &PageAnchor) -> usize {
        let Some((at, block)) = self.position(anchor) else {
            return 0;
        };
        let after = self.suffix[at + 1];
        let blocks = self.groups[at].blocks.saturating_sub(block + 1);
        after + blocks
    }

    fn position(&self, anchor: &PageAnchor) -> Option<(usize, usize)> {
        let (logical, block) = parse_event_id(&anchor.stable_id)?;
        let at = *self.by_id.get(logical)?;
        (block < self.groups[at].blocks).then_some((at, block))
    }

    fn provenance(
        &self,
        key: &SessionKey,
        row: i64,
        part: Option<usize>,
        record_type: &str,
    ) -> Provenance {
        Provenance {
            harness: crate::domain::Harness::Hermes,
            source: SourceKey::DbRow {
                database: self.database.clone(),
                table: "messages".into(),
                id: row.to_string(),
            },
            locator: SourceLocator {
                session: key.clone(),
                kind: LocatorKind::DbRow {
                    table: "messages".into(),
                    row_id: row.to_string(),
                    part_index: part,
                },
                generation: Some(self.generation),
            },
            record_type: record_type.to_owned(),
            harness_version: self.version.clone(),
        }
    }

    /// Reads one physical message row back for its event fields. Consecutive
    /// blocks of one row share the read.
    fn read_row(&mut self, conn: &Connection, row: i64) -> Result<&ParsedMessage, ProviderError> {
        if self.last_read.as_ref().is_some_and(|(id, _)| *id == row) {
            return self
                .last_read
                .as_ref()
                .map(|(_, parsed)| parsed)
                .ok_or_else(over);
        }
        let sql = format!(
            "SELECT {}, {}, {}, {}, {}, {}, {}, {} FROM messages WHERE id = ?1 LIMIT 1",
            text_cell(true, "role", 64),
            text_cell(true, "content", TEXT_MAX),
            cell_bytes(true, "content"),
            text_cell(self.shape.message_tool_call_id, "tool_call_id", 256),
            text_cell(self.shape.message_tool_calls, "tool_calls", TOOL_JSON_MAX),
            cell_bytes(self.shape.message_tool_calls, "tool_calls"),
            text_cell(self.shape.message_tool_name, "tool_name", 128),
            "CASE WHEN typeof(timestamp) IN ('integer','real') THEN timestamp END",
        );
        let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
        let values = match statement.query_row(rusqlite::params![row], |values| {
            Ok((
                values.get::<_, Option<String>>(0)?,
                values.get::<_, Option<String>>(1)?,
                values.get::<_, Option<i64>>(2)?,
                values.get::<_, Option<String>>(3)?,
                values.get::<_, Option<String>>(4)?,
                values.get::<_, Option<i64>>(5)?,
                values.get::<_, Option<String>>(6)?,
                values.get::<_, Option<f64>>(7)?,
            ))
        }) {
            Ok(values) => values,
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                return Err(ProviderError::Unavailable(
                    "a displayed message no longer exists; reopen the session".into(),
                ));
            }
            Err(error) => return Err(map_error(error)),
        };
        let parsed = ParsedMessage {
            role: values.0.unwrap_or_default(),
            content: values.1.map(|text| decode_content(&text)),
            content_len: values.2.unwrap_or(0).max(0) as usize,
            tool_call_id: values.3.filter(|id| !id.is_empty() && id.len() <= 256),
            tool_calls: values.4,
            tool_calls_len: values.5.unwrap_or(0).max(0) as usize,
            tool_name: values.6.and_then(|name| short(&name)),
            timestamp: epoch_ms(values.7),
        };
        self.last_read = Some((row, parsed));
        self.last_read
            .as_ref()
            .map(|(_, parsed)| parsed)
            .ok_or_else(over)
    }

    /// One event (`at`, `block`) of the folded projection.
    fn event(
        &mut self,
        conn: &Connection,
        key: &SessionKey,
        at: usize,
        block: usize,
    ) -> Result<Event, ProviderError> {
        let group = &self.groups[at];
        let row = group.representative.id;
        // Ordering follows the logical message's first display position, but the
        // source row belongs to the member that actually holds the representative.
        let member = group.representative.member;
        let logical_id = group.logical_id.clone();
        let order_key = self.groups[at].key;
        let source_order = order_of(member, order_key, block);
        let parsed = self.read_row(conn, row)?;
        let timestamp = parsed.timestamp;
        let sequence = event_sequence(parsed);
        let slot = sequence.get(block).cloned().ok_or_else(over)?;
        let member_key = self
            .members
            .get(member)
            .cloned()
            .unwrap_or_else(|| key.clone());
        let provenance = self.provenance(&member_key, row, Some(block), slot.record_type);
        let bounded = |text: &str, truncated: bool, locator: &SourceLocator| {
            let p = preview(text, TEXT_PREVIEW, TEXT_PREVIEW);
            BoundedText {
                truncated: truncated || p.truncated,
                preview: p.preview,
                byte_len: text.len(),
                locator: Some(locator.clone()),
            }
        };
        let locator = provenance.locator.clone();
        let (kind, actor, text, tool) = match slot.kind {
            SlotKind::Text {
                kind,
                actor,
                text,
                truncated,
            } => (
                kind,
                actor,
                text.map(|text| bounded(&text, truncated, &locator)),
                None,
            ),
            SlotKind::Tool {
                name,
                call_id,
                input,
                output,
                status,
            } => (
                EventKind::Tool,
                crate::domain::Actor::Tool,
                None,
                Some(ToolInteraction {
                    name: if name.trim().is_empty() {
                        "tool".into()
                    } else {
                        name
                    },
                    call_id,
                    input: input.map(|(text, cut)| bounded(&text, cut, &locator)),
                    output: output.map(|(text, cut)| bounded(&text, cut, &locator)),
                    status,
                    exit_code: None,
                    started_at: None,
                    completed_at: None,
                    orphan: false,
                    // Hermes records no child session id on a tool call.
                    child_session: None,
                }),
            ),
        };
        Ok(Event {
            id: event_id(&logical_id, block),
            session: key.clone(),
            timestamp,
            source_order,
            kind,
            actor,
            text,
            tool,
            usage: None,
            provenance,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn page(
        &mut self,
        conn: &Connection,
        key: &SessionKey,
        anchor: Option<&PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        budget: RefreshBudget,
        started: std::time::Instant,
    ) -> Result<TranscriptPage, ProviderError> {
        let empty = TranscriptPage {
            events: Vec::new(),
            diagnostics: Vec::new(),
            next: None,
            has_more: false,
            truncated: false,
        };
        let Some((position, block)) = anchor.map(|a| self.position(a).unwrap_or((usize::MAX, 0)))
        else {
            let position = if direction == PageDirection::Forward {
                (0, 0)
            } else {
                match self.groups.last() {
                    Some(group) => (self.groups.len() - 1, group.blocks.saturating_sub(1)),
                    None => return Ok(empty),
                }
            };
            return self.collect(conn, key, position, direction, request, budget, started);
        };
        if position == usize::MAX {
            // The anchor belongs to an older generation or a removed event.
            return Ok(empty);
        }
        let next = match direction {
            PageDirection::Forward => (position, block + 1),
            PageDirection::Backward => {
                if block > 0 {
                    (position, block - 1)
                } else if position > 0 {
                    (
                        position - 1,
                        self.groups[position - 1].blocks.saturating_sub(1),
                    )
                } else {
                    return Ok(empty);
                }
            }
        };
        self.collect(conn, key, next, direction, request, budget, started)
    }

    #[allow(clippy::too_many_arguments)]
    fn collect(
        &mut self,
        conn: &Connection,
        key: &SessionKey,
        start: (usize, usize),
        direction: PageDirection,
        request: PageRequest,
        budget: RefreshBudget,
        started: std::time::Instant,
    ) -> Result<TranscriptPage, ProviderError> {
        let mut events = Vec::new();
        let mut size = 0usize;
        let mut truncated = false;
        let mut has_more = false;
        let mut cursor = normalize(self, start.0, start.1, direction);
        while let Some((at, block)) = cursor {
            if !events.is_empty()
                && (events.len() >= request.max_events.max(1)
                    || size >= request.max_bytes.max(1)
                    || events.len() >= budget.max_records.max(1)
                    || started.elapsed().as_millis() >= u128::from(budget.max_millis))
            {
                has_more = true;
                break;
            }
            let mut event = self.event(conn, key, at, block)?;
            let cut = fit_event(&mut event, request.max_bytes);
            let cost = event_size(&event);
            if !events.is_empty() && size.saturating_add(cost) > request.max_bytes {
                has_more = true;
                break;
            }
            size += cost;
            truncated |= cut
                || event.text.as_ref().is_some_and(|text| text.truncated)
                || event.tool.as_ref().is_some_and(|tool| {
                    tool.input.as_ref().is_some_and(|input| input.truncated)
                        || tool.output.as_ref().is_some_and(|output| output.truncated)
                });
            events.push(event);
            cursor = step(self, at, block, direction);
        }
        if direction == PageDirection::Backward {
            events.reverse();
        }
        let next = if direction == PageDirection::Forward {
            events.last()
        } else {
            events.first()
        }
        .map(event_anchor);
        Ok(TranscriptPage {
            events,
            diagnostics: Vec::new(),
            next,
            has_more,
            truncated,
        })
    }
}

/// Clamps a continuation cursor onto an existing event: an anchor at the last
/// block of a group advances to the next group instead of reading past it.
fn normalize(
    projection: &Projection,
    mut at: usize,
    mut block: usize,
    direction: PageDirection,
) -> Option<(usize, usize)> {
    match direction {
        PageDirection::Forward => {
            while at < projection.groups.len() && block >= projection.groups[at].blocks {
                at += 1;
                block = 0;
            }
            (at < projection.groups.len()).then_some((at, block))
        }
        PageDirection::Backward => {
            while block >= projection.groups[at].blocks {
                if at == 0 {
                    return None;
                }
                at -= 1;
                block = projection.groups[at].blocks.saturating_sub(1);
            }
            Some((at, block))
        }
    }
}

fn step(
    projection: &Projection,
    at: usize,
    block: usize,
    direction: PageDirection,
) -> Option<(usize, usize)> {
    match direction {
        PageDirection::Forward => {
            if block + 1 < projection.groups[at].blocks {
                Some((at, block + 1))
            } else if at + 1 < projection.groups.len() {
                Some((at + 1, 0))
            } else {
                None
            }
        }
        PageDirection::Backward => {
            if block > 0 {
                Some((at, block - 1))
            } else if at > 0 {
                Some((at - 1, projection.groups[at - 1].blocks.saturating_sub(1)))
            } else {
                None
            }
        }
    }
}

fn order_of(member: usize, key: i64, block: usize) -> u64 {
    let key = u64::try_from(key)
        .ok()
        .filter(|key| *key < (1 << 48))
        .unwrap_or(0);
    ((member as u64) << 56) | (key << 8) | (block as u64 & 0xff)
}

fn event_id(logical: &str, block: usize) -> String {
    if block == 0 {
        format!("hermes:{logical}")
    } else {
        format!("hermes:{logical}:{block}")
    }
}

fn parse_event_id(id: &str) -> Option<(&str, usize)> {
    let rest = id.strip_prefix("hermes:")?;
    match rest.rsplit_once(':') {
        Some((logical, block)) => Some((logical, block.parse().ok()?)),
        None => Some((rest, 0)),
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// One display scan row: id plus the message fields the projection folds.
struct ScanRow {
    message: ParsedMessage,
    active: bool,
    summary: bool,
    identity: Option<Vec<u8>>,
    order: Option<i64>,
    uid: Option<String>,
}

fn scan_row(row: &rusqlite::Row<'_>) -> Result<ScanRow, ProviderError> {
    let message = ParsedMessage {
        role: row
            .get::<_, Option<String>>(1)
            .map_err(map_error)?
            .unwrap_or_default(),
        content: row
            .get::<_, Option<String>>(2)
            .map_err(map_error)?
            .map(|text| decode_content(&text)),
        content_len: row
            .get::<_, Option<i64>>(3)
            .map_err(map_error)?
            .unwrap_or(0)
            .max(0) as usize,
        tool_call_id: row
            .get::<_, Option<String>>(4)
            .map_err(map_error)?
            .filter(|id| !id.is_empty()),
        tool_calls: row.get::<_, Option<String>>(5).map_err(map_error)?,
        tool_calls_len: row
            .get::<_, Option<i64>>(6)
            .map_err(map_error)?
            .unwrap_or(0)
            .max(0) as usize,
        tool_name: row
            .get::<_, Option<String>>(7)
            .map_err(map_error)?
            .and_then(|name| short(&name)),
        timestamp: epoch_ms(row.get::<_, Option<f64>>(8).map_err(map_error)?),
    };
    Ok(ScanRow {
        message,
        active: row.get::<_, i64>(9).map_err(map_error)? != 0,
        summary: row.get::<_, i64>(11).map_err(map_error)? != 0,
        identity: row.get::<_, Option<Vec<u8>>>(12).map_err(map_error)?,
        order: row.get::<_, Option<i64>>(13).map_err(map_error)?,
        uid: row.get::<_, Option<String>>(14).map_err(map_error)?,
    })
}

impl ParsedMessage {
    fn byte_len(&self) -> usize {
        96 + self.content_len
            + self.tool_calls_len
            + self.tool_call_id.as_ref().map_or(0, String::len)
            + self.tool_name.as_ref().map_or(0, String::len)
    }
}

/// Native content is a string, or the producer's reserved `\0json:` sentinel
/// wrapping structured multimodal parts. Only the fixed text shape is projected.
fn decode_content(raw: &str) -> String {
    if !raw.starts_with("\u{0}json:") {
        return raw.to_owned();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw[6..]) else {
        return raw.to_owned();
    };
    match value {
        serde_json::Value::String(text) => text,
        serde_json::Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part {
                serde_json::Value::String(text) => Some(text.as_str()),
                serde_json::Value::Object(object) => {
                    object.get("text").and_then(serde_json::Value::as_str)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

#[derive(Clone)]
struct TextSlot {
    text: String,
    truncated: bool,
}

#[derive(Clone)]
enum SlotKind {
    Text {
        kind: EventKind,
        actor: crate::domain::Actor,
        text: Option<String>,
        truncated: bool,
    },
    Tool {
        name: String,
        call_id: Option<String>,
        input: Option<(String, bool)>,
        output: Option<(String, bool)>,
        status: ToolStatus,
    },
}

#[derive(Clone)]
struct Slot {
    kind: SlotKind,
    record_type: &'static str,
}

fn text_slot(
    kind: EventKind,
    actor: crate::domain::Actor,
    record_type: &'static str,
    text: Option<TextSlot>,
) -> Slot {
    Slot {
        kind: SlotKind::Text {
            kind,
            actor,
            text: text.as_ref().map(|text| text.text.clone()),
            truncated: text.is_some_and(|text| text.truncated),
        },
        record_type,
    }
}

fn blocks_of(parsed: &ParsedMessage) -> usize {
    event_sequence(parsed).len().clamp(1, BLOCKS_MAX)
}

/// The native displayable slots of one message: text, then its tool calls.
fn event_sequence(parsed: &ParsedMessage) -> Vec<Slot> {
    let role = parsed.role.trim().to_ascii_lowercase();
    let has_text = parsed
        .content
        .as_ref()
        .is_some_and(|content| !content.trim().is_empty());
    let text = parsed.content.as_ref().map(|content| TextSlot {
        text: content.clone(),
        truncated: parsed.content_len > content.len(),
    });
    let mut slots = Vec::new();
    match role.as_str() {
        "user" => slots.push(text_slot(
            EventKind::UserPrompt,
            crate::domain::Actor::User,
            "message/user",
            text,
        )),
        "assistant" => {
            let calls = parse_tool_calls(parsed);
            if has_text || calls.is_empty() {
                slots.push(text_slot(
                    EventKind::AssistantMessage,
                    crate::domain::Actor::Assistant,
                    "message/assistant",
                    text,
                ));
            }
            for call in calls {
                slots.push(Slot {
                    kind: SlotKind::Tool {
                        name: call.name,
                        call_id: call.call_id,
                        input: call.arguments,
                        output: None,
                        status: ToolStatus::Unknown,
                    },
                    record_type: "message/assistant#toolCall",
                });
            }
        }
        "tool" => slots.push(Slot {
            kind: SlotKind::Tool {
                name: parsed.tool_name.clone().unwrap_or_default(),
                call_id: parsed.tool_call_id.clone(),
                input: None,
                output: text.map(|text| (text.text, text.truncated)),
                status: ToolStatus::Unknown,
            },
            record_type: "message/tool",
        }),
        "" => slots.push(text_slot(
            EventKind::AssistantMessage,
            crate::domain::Actor::Assistant,
            "message/unknown",
            Some(text.unwrap_or(TextSlot {
                text: String::new(),
                truncated: false,
            })),
        )),
        _ => slots.push(text_slot(
            EventKind::System,
            crate::domain::Actor::System,
            "message/system",
            text,
        )),
    }
    if slots.len() > BLOCKS_MAX {
        let omitted = slots.len() - BLOCKS_MAX;
        slots.truncate(BLOCKS_MAX - 1);
        slots.push(text_slot(
            EventKind::System,
            crate::domain::Actor::System,
            "message/omitted",
            Some(TextSlot {
                text: format!("{omitted} further tool calls are not shown; open the source record"),
                truncated: true,
            }),
        ));
    }
    slots
}

struct Call {
    name: String,
    call_id: Option<String>,
    arguments: Option<(String, bool)>,
}

/// Parses the producer's `tool_calls` list (`[{id, type, function:{name, arguments}}]`).
/// An oversized or malformed cell yields one unlabeled call instead of hiding the row.
fn parse_tool_calls(parsed: &ParsedMessage) -> Vec<Call> {
    let Some(raw) = parsed
        .tool_calls
        .as_deref()
        .filter(|raw| !raw.trim().is_empty())
    else {
        return Vec::new();
    };
    if parsed.tool_calls_len > TOOL_JSON_MAX {
        return vec![Call {
            name: String::new(),
            call_id: None,
            arguments: None,
        }];
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return vec![Call {
            name: String::new(),
            call_id: None,
            arguments: None,
        }];
    };
    let list = match value {
        serde_json::Value::Array(list) => list,
        other => vec![other],
    };
    list.into_iter()
        .filter_map(|call| {
            let object = call.as_object()?;
            let function = object
                .get("function")
                .and_then(serde_json::Value::as_object);
            let name = function
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(display_safe)
                .filter(|name| !name.trim().is_empty())?;
            let arguments = function
                .and_then(|function| function.get("arguments"))
                .map(|value| {
                    let text = match value {
                        serde_json::Value::String(text) => text.clone(),
                        other => other.to_string(),
                    };
                    let truncated = text.len() > TEXT_MAX;
                    (text, truncated)
                });
            Some(Call {
                name,
                call_id: object
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .map(display_safe)
                    .filter(|id| !id.is_empty() && id.len() <= 256),
                arguments,
            })
        })
        .collect()
}

/// One bounded projection step over an already opened connection. The reader
/// keeps the resumable `Projection`; this helper is the complete form used by
/// small sources and tests; the reader's cached projection is the resumable form.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
fn project_hermes_messages(
    conn: &Connection,
    members: &[SessionKey],
    anchor: Option<&PageAnchor>,
    direction: PageDirection,
    request: PageRequest,
    budget: RefreshBudget,
) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
    let Some(first) = members.first() else {
        return Ok(ReadProgress::Ready(TranscriptPage {
            events: Vec::new(),
            diagnostics: Vec::new(),
            next: None,
            has_more: false,
            truncated: false,
        }));
    };
    let database = match &first.source {
        SourceKey::DbRow { database, .. } => database.clone(),
        SourceKey::File(path) => path.clone(),
    };
    let shape = super::schema::detect_shape(conn)?;
    if !shape.supported {
        return Err(ProviderError::Unsupported(
            "database has no supported messages schema".into(),
        ));
    }
    let mut projection = Projection::new(members, &database, 0, shape, None);
    let started = std::time::Instant::now();
    match projection.step(conn, budget)? {
        ReadProgress::Pending(progress) => Ok(ReadProgress::Pending(progress)),
        ReadProgress::Ready(_) => Ok(ReadProgress::Ready(
            projection.page(conn, first, anchor, direction, request, budget, started)?,
        )),
    }
}

// ---- compaction evidence ----------------------------------------------------

/// Bounded carrier/rotation evidence for a logical session.
pub(crate) struct CompactionScan {
    /// Event-scope observations, oldest first.
    pub observations: Vec<CompactionObservation>,
    /// Only proven in-place carriers were observed and the read was not capped.
    pub history_complete: bool,
}

/// Reads the `_compressed_summary` carriers and rotation edges of one chain.
/// Carriers deduplicate by their proven stored identity; arbitrary archived text
/// is never treated as a compaction event.
#[allow(clippy::too_many_arguments)]
pub(crate) fn read_compactions(
    conn: &Connection,
    shape: HermesShape,
    database: &Path,
    members: &[SessionKey],
    rows: &[HermesRowMeta],
    version: Option<&str>,
    cap: usize,
    generation: Option<u64>,
) -> Result<CompactionScan, ProviderError> {
    struct CarrierRow {
        rowid: i64,
        member: SessionKey,
        timestamp: Option<Timestamp>,
        text: String,
        content_len: usize,
        aliases: Vec<Identity>,
    }
    /// Carrier rows retained for alias grouping. Beyond this the history is
    /// incomplete instead of materializing an unbounded set of summaries.
    const CARRIER_ROWS_CAP: usize = 4096;
    let mut observations = Vec::new();
    let mut complete = shape.message_summary
        && shape.message_active
        && shape.message_compacted
        && (shape.message_display_identity || shape.message_uid);
    let mut member_rows = HashMap::new();
    for row in rows {
        member_rows.insert(row.key.clone(), row);
    }
    let sql = format!(
        "SELECT id, {}, {}, {}, {}, {} FROM messages \
         WHERE session_id = ?1 AND {} = 1 AND ({}) \
         ORDER BY timestamp DESC, id DESC LIMIT ?2",
        uid_cell(&shape),
        text_cell(true, "content", TEXT_MAX),
        cell_bytes(true, "content"),
        "CASE WHEN typeof(timestamp) IN ('integer','real') THEN timestamp END",
        identity_cell(&shape),
        summary_cell(&shape),
        display_where(&shape),
    );
    let mut carriers: Vec<CarrierRow> = Vec::new();
    for member in members {
        if !shape.message_summary {
            break;
        }
        let read_limit = cap.clamp(1, CARRIER_BATCH) as i64;
        let total: i64 = conn
            .query_row(
                &format!(
                    "SELECT count(*) FROM messages WHERE session_id = ?1 AND {} = 1 AND ({})",
                    summary_cell(&shape),
                    display_where(&shape)
                ),
                rusqlite::params![member.native_id],
                |row| row.get(0),
            )
            .map_err(map_error)?;
        if total as usize > cap || total as usize > CARRIER_BATCH {
            complete = false;
        }
        let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
        let mut query = statement
            .query(rusqlite::params![member.native_id, read_limit])
            .map_err(map_error)?;
        while let Some(row) = query.next().map_err(map_error)? {
            let rowid: i64 = row.get(0).map_err(map_error)?;
            let uid: Option<String> = row.get(1).map_err(map_error)?;
            let content: Option<String> = row.get(2).map_err(map_error)?;
            let content_len: Option<i64> = row.get(3).map_err(map_error)?;
            let timestamp: Option<f64> = row.get(4).map_err(map_error)?;
            let identity: Option<Vec<u8>> = row.get(5).map_err(map_error)?;
            let mut aliases = Vec::new();
            if let Some(identity) = identity {
                aliases.push(Identity::Hash(identity));
            }
            if let Some(uid) = uid.filter(|uid| !uid.is_empty()) {
                aliases.push(Identity::Uid(uid));
            }
            if aliases.is_empty() {
                complete = false;
            }
            if carriers.len() >= CARRIER_ROWS_CAP {
                complete = false;
                break;
            }
            carriers.push(CarrierRow {
                rowid,
                member: member.clone(),
                timestamp: epoch_ms(timestamp),
                text: content
                    .map(|text| decode_content(&text))
                    .unwrap_or_default(),
                content_len: content_len.unwrap_or(0).max(0) as usize,
                aliases,
            });
        }
    }
    // Union carrier copies by every proven alias. A row may introduce only one
    // member of an identity set, so a later row can bridge two groups: merge
    // those groups instead of counting a second compaction.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut alias_group: HashMap<Identity, usize> = HashMap::new();
    for (index, row) in carriers.iter().enumerate() {
        let mut targets: Vec<usize> = row
            .aliases
            .iter()
            .filter_map(|alias| alias_group.get(alias).copied())
            .collect();
        targets.sort_unstable();
        targets.dedup();
        let keep = match targets.first().copied() {
            Some(keep) => {
                for other in &targets[1..] {
                    if *other != keep {
                        let moved = std::mem::take(&mut groups[*other]);
                        groups[keep].extend(moved);
                        for value in alias_group.values_mut() {
                            if *value == *other {
                                *value = keep;
                            }
                        }
                    }
                }
                keep
            }
            None => {
                groups.push(Vec::new());
                groups.len() - 1
            }
        };
        for alias in &row.aliases {
            alias_group.insert(alias.clone(), keep);
        }
        groups[keep].push(index);
    }
    for group in groups {
        let Some(best) = group
            .iter()
            .copied()
            .max_by_key(|index| (carriers[*index].timestamp, carriers[*index].rowid))
        else {
            continue;
        };
        let row = &carriers[best];
        let locator = SourceLocator {
            session: row.member.clone(),
            kind: LocatorKind::DbRow {
                table: "messages".into(),
                row_id: row.rowid.to_string(),
                part_index: None,
            },
            generation,
        };
        let summary = if row.text.trim().is_empty() {
            None
        } else {
            let p = preview(&row.text, TEXT_PREVIEW, TEXT_PREVIEW);
            Some(BoundedText {
                truncated: p.truncated || row.content_len > row.text.len(),
                preview: p.preview,
                byte_len: row.content_len,
                locator: Some(locator.clone()),
            })
        };
        observations.push(CompactionObservation {
            id: format!("hermes:summary:{}", row.rowid),
            timestamp: row.timestamp,
            method: None,
            trigger: None,
            summary,
            before_tokens: None,
            after_tokens: None,
            duration_ms: None,
            usage: None,
            scope: CompactionScope::Event,
            provenance: Provenance {
                harness: crate::domain::Harness::Hermes,
                source: SourceKey::DbRow {
                    database: database.to_path_buf(),
                    table: "messages".into(),
                    id: row.rowid.to_string(),
                },
                locator,
                record_type: "messages/_compressed_summary".into(),
                harness_version: version.map(str::to_owned),
            },
        });
    }
    for member in members {
        let Some(row) = member_rows.get(member) else {
            complete = false;
            continue;
        };
        if row.end_reason.as_deref() != Some("compression") {
            continue;
        }
        let locator = SourceLocator {
            session: member.clone(),
            kind: LocatorKind::DbRow {
                table: "sessions".into(),
                row_id: row.id.clone(),
                part_index: None,
            },
            generation,
        };
        observations.push(CompactionObservation {
            id: format!("hermes:rotation:{}", row.id),
            timestamp: row.ended,
            method: Some("rotation".into()),
            trigger: Some("compression".into()),
            summary: None,
            before_tokens: None,
            after_tokens: None,
            duration_ms: None,
            usage: None,
            scope: CompactionScope::Event,
            provenance: Provenance {
                harness: crate::domain::Harness::Hermes,
                source: SourceKey::DbRow {
                    database: database.to_path_buf(),
                    table: "sessions".into(),
                    id: row.id.clone(),
                },
                locator,
                record_type: "sessions/end_reason=compression".into(),
                harness_version: version.map(str::to_owned),
            },
        });
    }
    observations.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
    Ok(CompactionScan {
        observations,
        history_complete: complete,
    })
}

/// The aggregate compression ledger: one `Aggregate`-scope observation per
/// `task='compression'` row. It is never split into per-event costs.
pub(crate) struct LedgerRequest<'a> {
    pub members: &'a [SessionKey],
    /// Read rows strictly after this `session_model_usage.rowid`.
    pub after: i64,
    /// Rows one call may return.
    pub limit: usize,
    /// Bytes one call may charge; one row is always allowed so a tiny slice can
    /// still make progress.
    pub max_bytes: usize,
    /// Slice deadline; rows read after it are left for the next call.
    pub deadline: Option<std::time::Instant>,
    pub generation: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct LedgerPage {
    pub observations: Vec<CompactionObservation>,
    /// Continuation cursor, present while rows remain.
    pub next: Option<i64>,
    pub more: bool,
    /// Approximate bytes charged by this page.
    pub bytes: usize,
}

const LEDGER_ROW_BYTES: usize = 192;

/// One bounded page of `task='compression'` ledger rows, newest work last. It is
/// one `Aggregate`-scope observation per native row, never a per-event split.
pub(crate) fn compression_ledger(
    conn: &mut Connection,
    shape: HermesShape,
    database: &Path,
    prices: &crate::pricing::PriceSchedule,
    request: LedgerRequest<'_>,
) -> Result<ReadProgress<LedgerPage>, ProviderError> {
    if !shape.usage || request.members.is_empty() {
        return Ok(ReadProgress::Ready(LedgerPage {
            observations: Vec::new(),
            next: None,
            more: false,
            bytes: 0,
        }));
    }
    let limit = request.limit.max(1);
    let marks = (2..=request.members.len() + 1)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let status = if shape.usage_cost_status {
        "CASE WHEN typeof(cost_status) = 'text' THEN cost_status END"
    } else {
        "NULL"
    };
    let seen_col = if shape.usage_seen {
        "CASE WHEN typeof(last_seen) IN ('integer','real') THEN last_seen END"
    } else {
        "NULL"
    };
    let sql = format!(
        "SELECT rowid, session_id, model, billing_provider, api_call_count, \
         input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, \
         estimated_cost_usd, actual_cost_usd, {status}, {seen_col} FROM session_model_usage \
         WHERE task = 'compression' AND session_id IN ({marks}) AND rowid > ?1 \
         ORDER BY rowid LIMIT {}",
        limit + 1
    );
    let mut values = vec![rusqlite::types::Value::Integer(request.after)];
    values.extend(
        request
            .members
            .iter()
            .map(|member| rusqlite::types::Value::Text(member.native_id.clone())),
    );
    let mut observations = Vec::new();
    let mut cursor = request.after;
    let mut more = false;
    let mut bytes = 0usize;
    let max_bytes = request.max_bytes.max(1);
    let deadline = request.deadline;
    let run = |conn: &Connection| -> Result<(), ProviderError> {
        let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
        let mut query = statement
            .query(rusqlite::params_from_iter(values))
            .map_err(map_error)?;
        while let Some(row) = query.next().map_err(map_error)? {
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                if observations.is_empty() {
                    return Err(ProviderError::Unavailable(
                        crate::providers::sqlite_guard::INTERRUPTED.into(),
                    ));
                }
                more = true;
                break;
            }
            if observations.len() >= limit
                || (!observations.is_empty() && bytes.saturating_add(LEDGER_ROW_BYTES) > max_bytes)
            {
                more = true;
                break;
            }
            let rowid: i64 = row.get(0).map_err(map_error)?;
            cursor = rowid;
            bytes = bytes.saturating_add(LEDGER_ROW_BYTES);
            let session: Option<String> = row.get(1).map_err(map_error)?;
            let model: Option<String> = row.get(2).map_err(map_error)?;
            let provider: Option<String> = row.get(3).map_err(map_error)?;
            let calls: Option<i64> = row.get(4).map_err(map_error)?;
            let totals = HermesModelTotals {
                row: super::usage::HermesRowId(u64::try_from(rowid).unwrap_or(0)),
                key: SessionKey {
                    harness: crate::domain::Harness::Hermes,
                    native_id: session.clone().unwrap_or_default(),
                    source: SourceKey::DbRow {
                        database: database.to_path_buf(),
                        table: "sessions".into(),
                        id: session.unwrap_or_default(),
                    },
                },
                model: model.and_then(|id| {
                    short(&id).map(|id| crate::domain::ModelDetail {
                        id,
                        provider: provider.as_deref().and_then(short),
                        variant: None,
                        effort: None,
                        configured_effort: None,
                    })
                }),
                task: "compression".into(),
                api_call_count: calls.and_then(|calls| u64::try_from(calls).ok()),
                tokens: super::usage::HermesTokens::from_row(row, 5).map_err(map_error)?,
                money: super::usage::HermesMoney::from_row(row, 10, 11, 12).map_err(map_error)?,
            };
            let locator = SourceLocator {
                session: totals.key.clone(),
                kind: LocatorKind::DbRow {
                    table: "session_model_usage".into(),
                    row_id: rowid.to_string(),
                    part_index: None,
                },
                generation: request.generation,
            };
            let provenance = Provenance {
                harness: crate::domain::Harness::Hermes,
                source: SourceKey::DbRow {
                    database: database.to_path_buf(),
                    table: "session_model_usage".into(),
                    id: rowid.to_string(),
                },
                locator: locator.clone(),
                record_type: "session_model_usage/compression".into(),
                harness_version: None,
            };
            let last_seen: Option<f64> = row.get(13).map_err(map_error)?;
            observations.push(CompactionObservation {
                id: format!("hermes:compression:{rowid}"),
                timestamp: epoch_ms(last_seen),
                method: None,
                trigger: None,
                summary: None,
                before_tokens: None,
                after_tokens: None,
                duration_ms: None,
                usage: Some(totals.observation(prices, provenance.clone())),
                scope: CompactionScope::Aggregate,
                provenance,
            });
        }
        Ok(())
    };
    let outcome = match deadline {
        Some(deadline) => {
            set_busy(conn, busy_for_deadline(deadline, u64::MAX))?;
            with_deadline_bound(conn, deadline, run)
        }
        None => run(conn),
    };
    match outcome {
        Ok(()) => {}
        Err(error) if crate::providers::sqlite_guard::interrupted(&error) => {
            if observations.is_empty() {
                return Ok(ReadProgress::Pending(WorkProgress::default()));
            }
            more = true;
        }
        Err(error) => return Err(error),
    }
    Ok(ReadProgress::Ready(LedgerPage {
        next: more.then_some(cursor),
        observations,
        more,
        bytes,
    }))
}

// ---- bounded source-record reads ---------------------------------------------

fn redact_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => serde_json::Value::String(display_safe(text)),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact_json).collect())
        }
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), redact_json(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Reads one allowlisted physical row as bounded JSON. Text fields are redacted
/// **before** serialization for the default display form, so an embedded JSON
/// string such as `{"api_key":"..."}` cannot hide a secret behind JSON escapes;
/// `raw` keeps the bounded values for the explicit reveal toggle.
pub(crate) fn read_source_record(
    conn: &Connection,
    shape: HermesShape,
    table: &str,
    row_id: &str,
    cap: usize,
    generation: Option<u64>,
    session: &SessionKey,
) -> Result<SourceRecord, ProviderError> {
    let cap = cap.max(1024);
    let text = |present: bool, expr: &str| text_cell(present, expr, cap);
    let number = |present: bool, expr: &str| {
        if present {
            format!("CASE WHEN typeof({expr}) IN ('integer','real') THEN {expr} END")
        } else {
            "NULL".to_string()
        }
    };
    let missing = || {
        ProviderError::Unsupported(
            "source row changed since it was listed; reopen the session".into(),
        )
    };
    let mut object = serde_json::Map::new();
    match table {
        "messages" => {
            let id: i64 = row_id.parse().map_err(|_| missing())?;
            let sql = format!(
                "SELECT id, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} \
                 FROM messages WHERE id = ?1 LIMIT 1",
                text(true, "session_id"),
                text(true, "role"),
                text(true, "content"),
                cell_bytes(true, "content"),
                text(shape.message_tool_call_id, "tool_call_id"),
                text(shape.message_tool_calls, "tool_calls"),
                text(shape.message_tool_name, "tool_name"),
                number(true, "timestamp"),
                text(shape.message_finish_reason, "finish_reason"),
                active_cell(&shape),
                compacted_cell(&shape),
                summary_cell(&shape),
                text(shape.message_display_kind, "display_kind"),
                text(shape.message_uid, "message_uid"),
            );
            let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
            let mut query = statement.query(rusqlite::params![id]).map_err(map_error)?;
            let Some(row) = query.next().map_err(map_error)? else {
                return Err(missing());
            };
            object.insert("table".into(), "messages".into());
            object.insert("row_id".into(), id.into());
            let push = |object: &mut serde_json::Map<String, serde_json::Value>,
                        name: &str,
                        value: serde_json::Value| {
                object.insert(name.into(), value);
            };
            for (at, name) in [
                (1, "session_id"),
                (2, "role"),
                (3, "content"),
                (5, "tool_call_id"),
                (6, "tool_calls"),
                (7, "tool_name"),
            ] {
                push(
                    &mut object,
                    name,
                    row.get::<_, Option<String>>(at)
                        .map_err(map_error)?
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            push(
                &mut object,
                "timestamp",
                row.get::<_, Option<f64>>(8)
                    .map_err(map_error)?
                    .and_then(serde_json::Number::from_f64)
                    .map(serde_json::Value::Number)
                    .unwrap_or(serde_json::Value::Null),
            );
            push(
                &mut object,
                "finish_reason",
                row.get::<_, Option<String>>(9)
                    .map_err(map_error)?
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
            for (at, name) in ["active", "compacted", "_compressed_summary"]
                .into_iter()
                .enumerate()
            {
                push(
                    &mut object,
                    name,
                    row.get::<_, Option<i64>>(at + 10)
                        .map_err(map_error)?
                        .map(serde_json::Value::from)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            push(
                &mut object,
                "display_kind",
                row.get::<_, Option<String>>(13)
                    .map_err(map_error)?
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
            push(
                &mut object,
                "message_uid",
                row.get::<_, Option<String>>(14)
                    .map_err(map_error)?
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
            let content_bytes = row.get::<_, Option<i64>>(4).map_err(map_error)?;
            if content_bytes.unwrap_or(0).max(0) as usize > cap {
                object.insert("content_truncated".into(), true.into());
            }
        }
        "sessions" => {
            let sql = format!(
                "SELECT id, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} \
                 FROM sessions WHERE id = ?1 LIMIT 1",
                text(shape.parent, "parent_session_id"),
                text(shape.source, "source"),
                text(shape.model, "model"),
                text(shape.billing_provider, "billing_provider"),
                number(true, "started_at"),
                number(shape.ended, "ended_at"),
                text(shape.end_reason, "end_reason"),
                number(shape.last_activity, "last_activity_at"),
                text(shape.title, "title"),
                number(shape.archived, "archived"),
                number(shape.message_count, "message_count"),
                number(shape.tool_call_count, "tool_call_count"),
            );
            let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
            let mut query = statement
                .query(rusqlite::params![row_id])
                .map_err(map_error)?;
            let Some(row) = query.next().map_err(map_error)? else {
                return Err(missing());
            };
            object.insert("table".into(), "sessions".into());
            object.insert("id".into(), row_id.into());
            for (at, name) in [
                "parent_session_id",
                "source",
                "model",
                "billing_provider",
                "started_at",
                "ended_at",
                "end_reason",
                "last_activity_at",
                "title",
                "archived",
                "message_count",
                "tool_call_count",
            ]
            .into_iter()
            .enumerate()
            {
                let value = match name {
                    "parent_session_id" | "source" | "model" | "billing_provider"
                    | "end_reason" | "title" => row
                        .get::<_, Option<String>>(at + 1)
                        .map_err(map_error)?
                        .map(serde_json::Value::String),
                    _ => row
                        .get::<_, Option<f64>>(at + 1)
                        .map_err(map_error)?
                        .and_then(serde_json::Number::from_f64)
                        .map(serde_json::Value::Number),
                };
                object.insert(name.into(), value.unwrap_or(serde_json::Value::Null));
            }
        }
        "session_model_usage" => {
            let id: i64 = row_id.parse().map_err(|_| missing())?;
            let status = if shape.usage_cost_status {
                "CASE WHEN typeof(cost_status) = 'text' THEN cost_status END"
            } else {
                "NULL"
            };
            let sql = format!(
                "SELECT rowid, session_id, model, billing_provider, task, api_call_count, \
                 input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, \
                 estimated_cost_usd, actual_cost_usd, {status} \
                 FROM session_model_usage WHERE rowid = ?1 LIMIT 1"
            );
            let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
            let mut query = statement.query(rusqlite::params![id]).map_err(map_error)?;
            let Some(row) = query.next().map_err(map_error)? else {
                return Err(missing());
            };
            object.insert("table".into(), "session_model_usage".into());
            object.insert("row_id".into(), id.into());
            for (at, name) in ["session_id", "model", "billing_provider", "task"]
                .into_iter()
                .enumerate()
            {
                object.insert(
                    name.into(),
                    row.get::<_, Option<String>>(at + 1)
                        .map_err(map_error)?
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            for (at, name) in [
                "api_call_count",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "reasoning_tokens",
                "estimated_cost_usd",
                "actual_cost_usd",
            ]
            .into_iter()
            .enumerate()
            {
                object.insert(
                    name.into(),
                    row.get::<_, Option<f64>>(at + 5)
                        .map_err(map_error)?
                        .and_then(serde_json::Number::from_f64)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            object.insert(
                "cost_status".into(),
                row.get::<_, Option<String>>(13)
                    .map_err(map_error)?
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        _ => {
            return Err(ProviderError::Unsupported(
                "not a Hermes source locator".into(),
            ));
        }
    }
    let raw_value = serde_json::Value::Object(object.clone());
    let display_value = redact_json(&raw_value);
    let _ = (generation, session);
    Ok(SourceRecord {
        raw: raw_value.to_string(),
        display: display_value.to_string(),
        unavailable: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_keys_sort_by_member_then_display_position_then_block() {
        assert!(order_of(0, 10, 0) < order_of(0, 10, 1));
        assert!(order_of(0, 10, 5) < order_of(0, 11, 0));
        assert!(order_of(0, 11, 0) < order_of(1, 0, 0));
        assert_eq!(order_of(0, -3, 0), order_of(0, 0, 0));
    }

    fn projection_fixture() -> (Connection, SessionKey) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY, started_at REAL NOT NULL);
             CREATE TABLE messages(
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                role TEXT NOT NULL,
                content TEXT,
                tool_call_id TEXT,
                tool_calls TEXT,
                tool_name TEXT,
                timestamp REAL NOT NULL,
                active INTEGER NOT NULL DEFAULT 1,
                compacted INTEGER NOT NULL DEFAULT 0,
                _compressed_summary INTEGER NOT NULL DEFAULT 0,
                display_identity BLOB,
                display_order INTEGER,
                message_uid TEXT);",
        )
        .unwrap();
        let key = SessionKey {
            harness: crate::domain::Harness::Hermes,
            native_id: "s".into(),
            source: SourceKey::DbRow {
                database: std::path::PathBuf::from("mem"),
                table: "sessions".into(),
                id: "s".into(),
            },
        };
        (conn, key)
    }

    fn insert_fixture(
        conn: &Connection,
        role: &str,
        content: &str,
        active: i64,
        compacted: i64,
        uid: Option<&str>,
        order: i64,
    ) {
        conn.execute(
            "INSERT INTO messages(session_id, role, content, timestamp, active, compacted, \
             display_identity, display_order, message_uid) \
             VALUES ('s', ?1, ?2, 1.0, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                role,
                content,
                active,
                compacted,
                uid.map(|uid| uid.as_bytes().to_vec()),
                order,
                uid
            ],
        )
        .unwrap();
    }

    /// A byte budget that fits one row must not be mistaken for end-of-member:
    /// every later display row and tombstone is still read.
    #[test]
    fn a_byte_interrupted_batch_keeps_the_cursor_and_tombstones() {
        let (conn, key) = projection_fixture();
        // The first live row is older than the removed copy of its identity: a
        // skipped tombstone would resurrect it in the folded projection.
        insert_fixture(&conn, "user", "live one", 1, 0, Some("dead-1"), 10);
        for at in 0..3 {
            insert_fixture(
                &conn,
                "user",
                &format!("removed {at}"),
                0,
                0,
                Some(&format!("dead-{at}")),
                (at + 1) as i64,
            );
        }
        insert_fixture(&conn, "assistant", "live two", 1, 0, Some("live-2"), 11);
        insert_fixture(&conn, "assistant", "live three", 1, 0, Some("live-3"), 12);
        let shape = crate::providers::hermes::schema::detect_shape(&conn).unwrap();
        let mut projection = Projection::new(&[key], std::path::Path::new("mem"), 1, shape, None)
            .with_caps(100, 10_000);
        let budget = RefreshBudget {
            max_records: 100,
            max_bytes: 1,
            max_millis: 60_000,
        };
        let mut steps = 0usize;
        while let ReadProgress::Pending(_) = projection.step(&conn, budget).unwrap() {
            steps += 1;
            assert!(steps < 64, "the cursor never advanced");
        }
        assert!(steps > 1);
        assert_eq!(projection.suffix.first().copied(), Some(2));
    }

    /// UID-only tombstones are charged and capped by the same bounds as hashed
    /// identities; they cannot grow past the row ceiling.
    #[test]
    fn uid_only_tombstones_are_bounded() {
        let (conn, key) = projection_fixture();
        for at in 0..8 {
            insert_fixture(&conn, "user", "dead", 0, 0, Some(&format!("uid-{at}")), at);
        }
        insert_fixture(&conn, "assistant", "live", 1, 0, Some("live"), 20);
        conn.execute("UPDATE messages SET display_identity = NULL", [])
            .unwrap();
        let shape = crate::providers::hermes::schema::detect_shape(&conn).unwrap();
        let mut projection = Projection::new(&[key], std::path::Path::new("mem"), 1, shape, None)
            .with_caps(4, usize::MAX);
        let error = projection
            .step(
                &conn,
                RefreshBudget {
                    max_records: 100,
                    max_bytes: 1_000_000,
                    max_millis: 60_000,
                },
            )
            .expect_err("the tombstone ceiling is enforced");
        assert!(matches!(error, ProviderError::Unsupported(_)));
    }

    #[test]
    fn stable_event_ids_round_trip() {
        assert_eq!(event_id("r7", 0), "hermes:r7");
        assert_eq!(event_id("r7", 2), "hermes:r7:2");
        assert_eq!(parse_event_id("hermes:r7"), Some(("r7", 0)));
        assert_eq!(parse_event_id("hermes:r7:2"), Some(("r7", 2)));
        assert_eq!(hex(b"\x00\xff"), "00ff");
    }

    fn ledger_fixture(conn: &Connection, rows: usize) {
        conn.execute_batch(
            "CREATE TABLE session_model_usage(
                session_id TEXT, model TEXT, billing_provider TEXT, task TEXT,
                api_call_count INTEGER, input_tokens INTEGER, output_tokens INTEGER,
                cache_read_tokens INTEGER, cache_write_tokens INTEGER, reasoning_tokens INTEGER,
                estimated_cost_usd REAL, actual_cost_usd REAL, cost_status TEXT,
                first_seen REAL, last_seen REAL);",
        )
        .unwrap();
        for at in 0..rows {
            conn.execute(
                "INSERT INTO session_model_usage(
                    session_id, model, billing_provider, task, api_call_count,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, estimated_cost_usd, actual_cost_usd, cost_status)
                 VALUES ('s', ?1, 'p', 'compression', 1, 1, 1, 0, 0, 0, 0.001, 0.001, 'actual')",
                rusqlite::params![format!("model-{at}")],
            )
            .unwrap();
        }
    }

    fn ledger_shape() -> crate::providers::hermes::schema::HermesShape {
        crate::providers::hermes::schema::HermesShape {
            supported: true,
            usage: true,
            ..Default::default()
        }
    }

    /// A one-byte ledger slice yields one row per call: only the ledger is
    /// enumerated, so the step count is the enumeration count.
    #[test]
    fn a_one_byte_ledger_page_enumerates_one_row() {
        let conn = Connection::open_in_memory().unwrap();
        ledger_fixture(&conn, 300);
        let mut conn = conn;
        let key = SessionKey {
            harness: crate::domain::Harness::Hermes,
            native_id: "s".into(),
            source: SourceKey::DbRow {
                database: std::path::PathBuf::from("mem"),
                table: "sessions".into(),
                id: "s".into(),
            },
        };
        let prices = crate::pricing::PriceSchedule::bundled().expect("bundled prices");
        let mut after = -1i64;
        let mut calls = 0usize;
        loop {
            let page = compression_ledger(
                &mut conn,
                ledger_shape(),
                std::path::Path::new("mem"),
                &prices,
                LedgerRequest {
                    members: std::slice::from_ref(&key),
                    after,
                    limit: 64,
                    max_bytes: 1,
                    deadline: None,
                    generation: None,
                },
            )
            .unwrap();
            let ReadProgress::Ready(page) = page else {
                panic!("an unlimited deadline never yields Pending");
            };
            calls += 1;
            assert!(
                page.observations.len() <= 1,
                "a one-byte slice read 64 rows"
            );
            if !page.more {
                break;
            }
            after = page.next.expect("a continuation cursor");
            assert!(calls < 1000);
        }
        assert!(calls > 200, "300 rows at one per call ({calls})");
    }

    /// The ledger query itself is bounded by the slice deadline: the read-only
    /// connection and its schema handshake happen before the writer lock.
    #[test]
    fn a_locked_ledger_scan_respects_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let writer = Connection::open(&path).unwrap();
        writer
            .pragma_update(None, "journal_mode", "delete")
            .unwrap();
        ledger_fixture(&writer, 5);
        let mut reader = crate::providers::hermes::schema::open_read_only(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").expect("lock");
        let prices = crate::pricing::PriceSchedule::bundled().expect("bundled prices");
        let key = SessionKey {
            harness: crate::domain::Harness::Hermes,
            native_id: "s".into(),
            source: SourceKey::DbRow {
                database: path.clone(),
                table: "sessions".into(),
                id: "s".into(),
            },
        };
        let started = std::time::Instant::now();
        let result = compression_ledger(
            &mut reader,
            ledger_shape(),
            &path,
            &prices,
            LedgerRequest {
                members: std::slice::from_ref(&key),
                after: -1,
                limit: 64,
                max_bytes: usize::MAX,
                deadline: Some(std::time::Instant::now() + std::time::Duration::from_millis(20)),
                generation: None,
            },
        );
        assert!(
            started.elapsed() < std::time::Duration::from_millis(500),
            "the lock wait must fit the slice"
        );
        match result {
            Ok(ReadProgress::Pending(_)) => {}
            Err(ProviderError::Unavailable(_)) => {}
            other => panic!("unexpected locked ledger result: {other:?}"),
        }
        writer.execute_batch("ROLLBACK").expect("release");
    }
}
