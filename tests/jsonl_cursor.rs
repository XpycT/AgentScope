use agentscope::domain::{LocatorKind, RefreshBudget};
use agentscope::ingest::jsonl::{JsonlCursor, Record};
use agentscope::ingest::source::{SourceItem, SourceReader};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn temp() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("s.jsonl");
    (dir, path)
}

fn append(path: &PathBuf, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}

#[test]
fn holds_partial_line_until_newline_arrives() {
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Incomplete { offset: 0 })
    ));
    assert!(cursor.next_record().unwrap().is_none());
    append(&path, b"\n{\"b\":2}\n");
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["a"] == 1)
    );
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 8, ref value }) if value["b"] == 2)
    );
    assert!(cursor.next_record().unwrap().is_none());
}

#[test]
fn reports_invalid_oversized_and_shrink_without_stopping() {
    let (_dir, path) = temp();
    append(&path, b"nope\n123456789\n{\"ok\":1}\n");
    let mut cursor = JsonlCursor::open(path.clone(), 8).unwrap();
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Invalid { offset: 0 })
    ));
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::TooLarge { offset: 5 })
    ));
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 15, .. })
    ));
    assert!(cursor.next_record().unwrap().is_none());
    assert_eq!(cursor.rotation_count(), 0);

    fs::write(&path, b"{\"n\":1}\n").unwrap();
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["n"] == 1)
    );
    assert_eq!(cursor.rotation_count(), 1);
}

// Windows "file tunneling" can carry the old creation time onto a same-named replacement,
// so identity-only replacement is a Unix-only guarantee; Windows relies on shrink/mtime.
#[cfg(unix)]
#[test]
fn replacement_with_new_identity_resets_cursor() {
    let (dir, path) = temp();
    append(&path, b"{\"old\":1}\n");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(cursor.next_record().unwrap().is_some());
    assert!(cursor.next_record().unwrap().is_none());
    let other = dir.path().join("new.jsonl");
    fs::write(&other, b"{\"old\":1}\n{\"new\":2}\n").unwrap();
    fs::rename(&other, &path).unwrap();
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["old"] == 1)
    );
    assert_eq!(cursor.rotation_count(), 1);
}

#[test]
fn same_length_same_mtime_replacement_is_a_documented_false_negative() {
    let (_dir, path) = temp();
    append(&path, b"{\"v\":1}\n");
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(cursor.next_record().unwrap().is_some());
    assert!(cursor.next_record().unwrap().is_none());
    let file = OpenOptions::new().write(true).open(&path).unwrap();
    (&file).write_all(b"{\"v\":2}\n").unwrap();
    file.set_modified(mtime).unwrap();
    drop(file);
    assert!(cursor.next_record().unwrap().is_none());
    assert_eq!(cursor.rotation_count(), 0);
}

#[test]
fn crlf_lines_are_parsed_and_offsets_count_every_byte() {
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}\r\n{\"b\":2}\r\n");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 0, .. })
    ));
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 9, .. })
    ));
    assert_eq!(cursor.read_at(9, 1024).unwrap().unwrap(), b"{\"b\":2}");
}

#[test]
fn read_at_returns_the_record_bytes_for_source_view() {
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}\n{\"b\":2}\nlast");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert_eq!(cursor.read_at(0, 1024).unwrap().unwrap(), b"{\"a\":1}");
    assert_eq!(cursor.read_at(8, 1024).unwrap().unwrap(), b"{\"b\":2}");
    assert_eq!(cursor.read_at(8, 3).unwrap().unwrap(), b"{\"b");
    assert_eq!(cursor.read_at(16, 1024).unwrap().unwrap(), b"last");
    assert!(cursor.read_at(20, 1024).unwrap().is_none());
    assert!(cursor.read_at(999, 1024).unwrap().is_none());
    // read_at must not disturb the sequential position.
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 0, .. })
    ));
}

#[test]
fn tail_records_returns_newest_complete_records_in_order() {
    let (_dir, path) = temp();
    append(&path, b"{\"n\":1}\n{\"n\":2}\nbad\n{\"n\":4}\n{\"n\":5");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    let tail = cursor.tail_records(3, 1024).unwrap();
    assert_eq!(tail.len(), 3);
    assert!(matches!(tail[0], Record::Json { offset: 8, ref value } if value["n"] == 2));
    assert!(matches!(tail[1], Record::Invalid { offset: 16 }));
    assert!(matches!(tail[2], Record::Json { offset: 20, ref value } if value["n"] == 4));
    // Byte budget cuts the oldest records and never yields a partial leading line.
    let small = cursor.tail_records(10, 14).unwrap();
    assert_eq!(small.len(), 1);
    assert!(matches!(small[0], Record::Json { offset: 20, .. }));
    // Tail does not move the sequential cursor.
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { offset: 0, .. })
    ));
}

#[test]
fn failed_reopen_is_retried_and_recovers() {
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}\n");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(cursor.next_record().unwrap().is_some());
    assert!(cursor.next_record().unwrap().is_none());
    fs::remove_file(&path).unwrap();
    assert!(cursor.next_record().is_err());
    assert!(cursor.next_record().is_err(), "error must not be swallowed");
    fs::write(&path, b"{\"b\":2}\n{\"c\":3}\n").unwrap();
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["b"] == 2)
    );
    assert_eq!(cursor.rotation_count(), 1);
}

#[test]
fn larger_replacement_is_detected_even_if_creation_time_is_preserved() {
    let (dir, path) = temp();
    append(&path, b"{\"o\":1}\n");
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    assert!(cursor.next_record().unwrap().is_some());
    assert!(cursor.next_record().unwrap().is_none());
    // Written immediately, so Windows tunneling may reuse the creation time.
    let other = dir.path().join("n.jsonl");
    fs::write(&other, b"{\"n\":1}\n{\"n\":2}\n{\"n\":3}\n").unwrap();
    fs::remove_file(&path).unwrap();
    fs::rename(&other, &path).unwrap();
    assert!(
        matches!(cursor.next_record().unwrap(), Some(Record::Json { offset: 0, ref value }) if value["n"] == 1)
    );
    assert_eq!(cursor.rotation_count(), 1);
}

#[test]
fn oversized_line_is_drained_in_resumable_chunks() {
    let (_dir, path) = temp();
    // Far larger than the reader's chunk size and the record cap.
    let mut bytes = vec![b'x'; 1024 * 1024];
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"after\":1}\n");
    append(&path, &bytes);
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    let mut skips = 0usize;
    let mut last = cursor.position();
    let outcome = loop {
        let record = cursor.next_record().unwrap().unwrap();
        match record {
            Record::Skipping { offset } => {
                assert_eq!(offset, 0);
                let now = cursor.position();
                assert!(now > last, "every call makes progress");
                // Each call is bounded: it never swallows the whole line.
                assert!(now - last <= 64 * 1024, "chunk of {} bytes", now - last);
                last = now;
                skips += 1;
            }
            other => break other,
        }
    };
    assert!(skips > 10, "drained in {skips} steps");
    assert!(matches!(outcome, Record::TooLarge { offset: 0 }));
    assert!(matches!(
        cursor.next_record().unwrap(),
        Some(Record::Json { ref value, .. }) if value["after"] == 1
    ));
}

#[test]
fn oversized_line_without_terminator_reports_incomplete_after_draining() {
    let (_dir, path) = temp();
    append(&path, &vec![b'y'; 200 * 1024]);
    let mut cursor = JsonlCursor::open(path.clone(), 1024).unwrap();
    loop {
        match cursor.next_record().unwrap().unwrap() {
            Record::Skipping { .. } => {}
            Record::Incomplete { offset: 0 } => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(cursor.next_record().unwrap().is_none());
}

/// The source reader frames exactly what the cursor frames, so plain rollouts keep
/// their byte offsets and per-record behaviour through the shared line framer.
#[test]
fn source_reader_frames_what_the_plain_cursor_frames() {
    const MAX: usize = 8;
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}\n");
    append(&path, b"nope\n");
    append(&path, b"0123456789012\n");
    append(&path, b"{\"b\":2}\r\n");
    append(&path, b"{\"tail");

    let mut cursor = JsonlCursor::open(path.clone(), MAX).unwrap();
    let mut expected = Vec::new();
    let mut cursor_skips = 0usize;
    loop {
        match cursor.next_record().unwrap() {
            Some(Record::Skipping { .. }) => cursor_skips += 1,
            Some(Record::Json { offset, value }) => expected.push((offset, Item::Json(value))),
            Some(Record::Invalid { offset }) => expected.push((offset, Item::Invalid)),
            Some(Record::TooLarge { offset }) => expected.push((offset, Item::TooLarge)),
            Some(Record::Incomplete { offset }) => expected.push((offset, Item::Incomplete)),
            None => break,
        }
    }

    let mut reader = SourceReader::open(&path, MAX).unwrap();
    let mut actual = Vec::new();
    let mut skips = 0usize;
    loop {
        let batch = reader.step(BIG).unwrap();
        for record in batch.records {
            let LocatorKind::FileOffset { path: file, offset } = record.locator else {
                panic!("plain sources are addressed by byte offset");
            };
            assert_eq!(file, path);
            match record.item {
                SourceItem::Json(value) => actual.push((offset, Item::Json(value))),
                SourceItem::Invalid => actual.push((offset, Item::Invalid)),
                SourceItem::TooLarge => actual.push((offset, Item::TooLarge)),
                SourceItem::Incomplete => actual.push((offset, Item::Incomplete)),
                SourceItem::Skipping => skips += 1,
                SourceItem::Damaged(damage) => panic!("plain source damaged: {damage}"),
            }
        }
        if batch.done {
            break;
        }
    }
    assert_eq!(
        skips, cursor_skips,
        "both readers chunk the drain identically"
    );
    assert_eq!(actual, expected);
    // Two records, one invalid line, one oversized line, and the partial tail.
    assert_eq!(actual.len(), 5);
}

#[test]
fn source_reader_appends_without_invalidation_and_reports_a_shrunk_source() {
    let (_dir, path) = temp();
    append(&path, b"{\"a\":1}\n");
    let mut reader = SourceReader::open(&path, 1024).unwrap();
    let first = reader.step(BIG).unwrap();
    assert_eq!(first.records.len(), 1);
    assert!(!first.invalidated);
    assert!(first.done);

    // Growth is not a replacement: the same generation continues at byte 8.
    append(&path, b"{\"b\":2}\n");
    let appended = reader.step(BIG).unwrap();
    assert!(!appended.invalidated);
    assert_eq!(appended.records[0].ordinal, 1);
    assert!(matches!(
        &appended.records[0].locator,
        LocatorKind::FileOffset { offset: 8, .. }
    ));

    // A shrink restarts the cursor and starts a new generation.
    fs::write(&path, b"{\"c\":3}\n").unwrap();
    let replaced = reader.step(BIG).unwrap();
    assert!(replaced.invalidated);
    assert_eq!(replaced.records[0].generation, 1);
    assert_eq!(replaced.records[0].ordinal, 0);
    assert_eq!(reader.generation(), 1);
}

enum Item {
    Json(serde_json::Value),
    Invalid,
    TooLarge,
    Incomplete,
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Json(a), Self::Json(b)) => a == b,
            (Self::Invalid, Self::Invalid)
            | (Self::TooLarge, Self::TooLarge)
            | (Self::Incomplete, Self::Incomplete) => true,
            _ => false,
        }
    }
}

impl std::fmt::Debug for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(_) => f.write_str("Json"),
            Self::Invalid => f.write_str("Invalid"),
            Self::TooLarge => f.write_str("TooLarge"),
            Self::Incomplete => f.write_str("Incomplete"),
        }
    }
}
