//! Bounded plain/zstd source consumption: identical records from either
//! representation, generation-safe locators, and decompression/oversize boundaries.

use agentscope::domain::{
    Harness, LocatorKind, PageDirection, PageRequest, RefreshBudget, SessionKey, SourceKey,
    SourceLocator,
};
use agentscope::ingest::source::{
    SourceDamage, SourceItem, SourceReader, SourceRecordItem, SourceRecordProgress,
    canonical_rollout_identity,
};
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::{ProviderReader, ReadProgress};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

mod support;

use support::NOW;
use support::codex as cx;
use support::world::{BIG, file_inventory};

/// The production per-call allowance: 256 records, 8 MiB expanded, 100 ms.
const BUDGET: RefreshBudget = RefreshBudget {
    max_records: 256,
    max_bytes: 8 * 1024 * 1024,
    max_millis: 100,
};

const MAX_RECORD_BYTES: usize = 1024 * 1024;

fn compress(bytes: &[u8]) -> Vec<u8> {
    zstd::stream::encode_all(bytes, 3).unwrap()
}

/// Writes `plain` to `name` and its compressed copy to `name.zst`.
fn rollout(dir: &Path, name: &str, plain: &[u8]) -> (PathBuf, PathBuf) {
    let path = dir.join(name);
    fs::write(&path, plain).unwrap();
    let archived = dir.join(format!("{name}.zst"));
    fs::write(&archived, compress(plain)).unwrap();
    (path, archived)
}

/// A synthetic Codex-shaped rollout: metadata, alternating messages, a token
/// event, an unusable line, and a trailing partial line.
fn synthetic_rollout() -> Vec<u8> {
    let mut out =
        String::from("{\"type\":\"session_meta\",\"payload\":{\"id\":\"synthetic-a4-session\"}}\n");
    for index in 0..40 {
        let (role, part) = if index % 2 == 0 {
            ("user", "input_text")
        } else {
            ("assistant", "output_text")
        };
        out.push_str(&format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"{role}\",\
             \"content\":[{{\"type\":\"{part}\",\"text\":\"synthetic line {index}\"}}]}}}}\n"
        ));
    }
    out.push_str(
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"info\":\
         {\"total_token_usage\":{\"input_tokens\":1200,\"output_tokens\":345}}}}\n",
    );
    out.push_str("not json\n");
    out.push_str("{\"type\":\"partial\"}");
    out.into_bytes()
}

/// What the diagnostics of a source are allowed to reveal: shape, never content.
fn normalize(items: &[(u64, SourceItem)]) -> Vec<String> {
    items
        .iter()
        .filter_map(|(ordinal, item)| match item {
            SourceItem::Json(value) => {
                let payload = &value["payload"];
                let text = payload["content"]
                    .as_array()
                    .and_then(|parts| parts.first())
                    .map(|part| part["text"].as_str().unwrap_or_default().to_string())
                    .unwrap_or_default();
                Some(format!(
                    "{ordinal}|{}|{}|{}|{}|{text}",
                    value["type"].as_str().unwrap_or_default(),
                    payload["type"].as_str().unwrap_or_default(),
                    payload["role"].as_str().unwrap_or_default(),
                    payload["info"]["total_token_usage"]["output_tokens"]
                        .as_u64()
                        .map_or_else(String::new, |tokens| tokens.to_string()),
                ))
            }
            SourceItem::Invalid => Some(format!("{ordinal}|invalid")),
            SourceItem::TooLarge => Some(format!("{ordinal}|too-large")),
            SourceItem::Incomplete => Some(format!("{ordinal}|incomplete")),
            SourceItem::Damaged(damage) => Some(format!("{ordinal}|damaged|{damage}")),
            // Chunking differs between the plain and compressed paths.
            SourceItem::Skipping => None,
        })
        .collect()
}

fn collect(reader: &mut SourceReader, budget: RefreshBudget) -> (Vec<(u64, SourceItem)>, usize) {
    let mut items = Vec::new();
    let mut expanded = 0;
    for _ in 0..1_000_000 {
        let batch = reader.step(budget).unwrap();
        expanded += batch.expanded_bytes;
        for record in batch.records {
            items.push((record.ordinal, record.item));
        }
        if batch.done {
            return (items, expanded);
        }
    }
    panic!("source did not finish");
}

fn session(physical: &Path) -> SessionKey {
    SessionKey {
        harness: Harness::Codex,
        native_id: "synthetic-a4-session".to_string(),
        source: SourceKey::File(canonical_rollout_identity(physical)),
    }
}

fn locator_of(reader: &mut SourceReader, locator: &SourceLocator) -> SourceRecordProgress {
    reader.record(locator, BUDGET).unwrap()
}

#[test]
fn plain_and_compressed_reads_produce_identical_records() {
    let dir = tempfile::tempdir().unwrap();
    let plain = synthetic_rollout();
    let (plain_path, archived_path) = rollout(dir.path(), "synthetic.jsonl", &plain);
    let before = file_inventory(dir.path());

    let (plain_items, plain_bytes) = collect(
        &mut SourceReader::open(&plain_path, MAX_RECORD_BYTES).unwrap(),
        BUDGET,
    );
    let (archived_items, expanded_bytes) = collect(
        &mut SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap(),
        BUDGET,
    );

    assert_eq!(normalize(&plain_items), normalize(&archived_items));
    assert_eq!(plain_items.len(), archived_items.len());
    // Every expanded byte of the plain source is one compressed byte stream's expansion.
    assert_eq!(plain_bytes, plain.len());
    assert_eq!(expanded_bytes, plain.len());
    assert!(
        plain_items
            .iter()
            .all(|(_, item)| item != &SourceItem::Damaged(SourceDamage::Truncated))
    );
    // Ordinals are representation-independent; locators are not.
    assert_eq!(
        plain_items.iter().map(|(o, _)| *o).collect::<Vec<_>>(),
        archived_items.iter().map(|(o, _)| *o).collect::<Vec<_>>()
    );
    for (_, item) in &archived_items {
        if matches!(item, SourceItem::Skipping) {
            continue;
        }
        // Reading never rewrites the source tree.
        assert!(!matches!(item, SourceItem::Damaged(_)));
    }
    assert_eq!(file_inventory(dir.path()), before);
}

#[test]
fn locators_name_the_physical_representation_never_a_bare_offset() {
    let dir = tempfile::tempdir().unwrap();
    let plain = synthetic_rollout();
    let (plain_path, archived_path) = rollout(dir.path(), "roll.jsonl", &plain);

    let mut reader = SourceReader::open(&plain_path, MAX_RECORD_BYTES).unwrap();
    let batch = reader.step(BUDGET).unwrap();
    for record in &batch.records {
        assert!(matches!(
            record.locator,
            LocatorKind::FileOffset { ref path, .. } if path == &plain_path
        ));
        assert_eq!(record.generation, reader.generation());
    }

    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let batch = reader.step(BUDGET).unwrap();
    assert!(!batch.records.is_empty());
    for record in &batch.records {
        match record.locator {
            LocatorKind::CompressedRecord { ref path, ordinal } => {
                assert_eq!(path, &archived_path);
                assert_eq!(ordinal, record.ordinal);
            }
            ref other => panic!("unexpected locator {other:?}"),
        }
    }
}

#[test]
fn canonical_identity_gathers_aliases_and_strips_only_jsonl_zst() {
    assert_eq!(
        canonical_rollout_identity(Path::new("a/rollout.jsonl.zst")),
        PathBuf::from("a/rollout.jsonl")
    );
    assert_eq!(
        canonical_rollout_identity(Path::new("a/rollout.JSONL.ZST")),
        PathBuf::from("a/rollout.JSONL")
    );
    // Everything that is not a confirmed archived rollout keeps its name.
    for unchanged in [
        "a/rollout.jsonl",
        "a/rollout.ndjson.zst",
        "a/rollout.zst",
        "a/rollout.jsonl.gz",
        "a/archive.tar.zst",
        ".zst",
    ] {
        assert_eq!(
            canonical_rollout_identity(Path::new(unchanged)),
            PathBuf::from(unchanged),
            "{unchanged}"
        );
    }

    let dir = tempfile::tempdir().unwrap();
    let plain = synthetic_rollout();
    let (plain_path, archived_path) = rollout(dir.path(), "alias.jsonl", &plain);
    let plain_reader = SourceReader::open(&plain_path, MAX_RECORD_BYTES).unwrap();
    let archived_reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    // One native id, one canonical key, whichever copy is discoverable.
    assert_eq!(
        canonical_rollout_identity(plain_reader.physical_path()),
        canonical_rollout_identity(archived_reader.physical_path())
    );
    assert_eq!(
        canonical_rollout_identity(archived_reader.physical_path()),
        plain_path
    );
}

#[test]
fn replacing_the_archive_invalidates_old_locators() {
    let dir = tempfile::tempdir().unwrap();
    let plain = synthetic_rollout();
    let (_plain_path, archived_path) = rollout(dir.path(), "gen.jsonl", &plain);
    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    assert_eq!(reader.generation(), 0);

    let batch = reader.step(BUDGET).unwrap();
    let session = session(&archived_path);
    let first = batch.records[0].source_locator(session.clone());
    let second = batch.records[1].source_locator(session.clone());
    assert_eq!(first.generation, Some(0));
    assert!(matches!(
        locator_of(&mut reader, &first),
        SourceRecordProgress::Pending | SourceRecordProgress::Ready(_)
    ));
    assert!(matches!(
        locator_of(&mut reader, &second),
        SourceRecordProgress::Ready(_)
    ));

    // A new frame with different content replaces the observed generation.
    let replacement = synthetic_rollout()
        .split(|byte| *byte == b'\n')
        .take(4)
        .collect::<Vec<_>>()
        .join(&b'\n');
    let replacement = [replacement, b"\n".to_vec()].concat();
    fs::write(&archived_path, compress(&replacement)).unwrap();

    let batch = reader.step(BUDGET).unwrap();
    assert!(batch.invalidated, "replacement must be reported");
    assert_eq!(reader.generation(), 1);
    assert_eq!(batch.records[0].ordinal, 0);
    assert_eq!(batch.records[0].generation, 1);

    // Locators from the previous generation no longer resolve.
    assert_eq!(locator_of(&mut reader, &first), SourceRecordProgress::Stale);
    assert_eq!(
        locator_of(&mut reader, &second),
        SourceRecordProgress::Stale
    );

    let fresh = batch.records[0].source_locator(session);
    match locator_of(&mut reader, &fresh) {
        SourceRecordProgress::Ready(record) => {
            assert!(record.unavailable.is_none());
            assert!(record.raw.starts_with("{\"type\":\"session_meta\""));
        }
        other => panic!("expected a ready record, got {other:?}"),
    }
}

#[test]
fn appended_frame_is_a_new_generation_with_the_combined_content() {
    let dir = tempfile::tempdir().unwrap();
    let archived_path = dir.path().join("append.jsonl.zst");
    let first = b"{\"n\":1}\n{\"n\":2}\n";
    fs::write(&archived_path, compress(first)).unwrap();

    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let (items, _) = collect(&mut reader, BUDGET);
    assert_eq!(normalize(&items).len(), 2);
    assert_eq!(reader.generation(), 0);

    // Concatenating a second frame keeps the file a valid zstd stream but changes the
    // observed source, so the reader restarts instead of serving a stale generation.
    let mut bytes = fs::read(&archived_path).unwrap();
    bytes.extend_from_slice(&compress(b"{\"n\":3}\n"));
    fs::write(&archived_path, &bytes).unwrap();

    let batch = reader.step(BUDGET).unwrap();
    assert!(batch.invalidated);
    assert_eq!(reader.generation(), 1);
    let mut items: Vec<(u64, SourceItem)> = batch
        .records
        .into_iter()
        .map(|r| (r.ordinal, r.item))
        .collect();
    items.extend(collect(&mut reader, BUDGET).0);
    let values: Vec<Value> = items
        .iter()
        .filter_map(|(_, item)| match item {
            SourceItem::Json(value) => Some(value.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(values.len(), 3);
    assert_eq!(values[2]["n"], 3);
}

#[test]
fn highly_compressible_history_expands_across_slices() {
    let dir = tempfile::tempdir().unwrap();
    let mut plain = String::new();
    for index in 0..200_000 {
        plain.push_str(&format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\
             \"content\":[{{\"type\":\"input_text\",\"text\":\"compressible {index}\"}}]}}}}\n"
        ));
    }
    let plain = plain.into_bytes();
    let archived_path = dir.path().join("big.jsonl.zst");
    let compressed = compress(&plain);
    fs::write(&archived_path, &compressed).unwrap();
    assert!(
        compressed.len() * 40 < plain.len(),
        "fixture must be far smaller than its expansion"
    );
    let before = file_inventory(dir.path());

    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let first = reader.step(BUDGET).unwrap();
    assert!(
        !first.done,
        "one call must not exhaust the expanded history"
    );
    assert!(!first.records.is_empty());
    assert!(first.expanded_bytes <= BUDGET.max_bytes);

    let mut steps = 1;
    let mut expanded = first.expanded_bytes;
    let mut records = first.records.len();
    let mut done = first.done;
    while !done {
        // Cancellation is possible between any two slices: the archive is untouched.
        let batch = reader.step(BUDGET).unwrap();
        steps += 1;
        assert!(batch.expanded_bytes <= BUDGET.max_bytes);
        expanded += batch.expanded_bytes;
        records += batch.records.len();
        done = batch.done;
        assert!(steps < 100_000);
    }
    assert!(steps > 2, "expanded in {steps} slices");
    assert_eq!(expanded, plain.len());
    assert_eq!(records, 200_000);
    assert_eq!(file_inventory(dir.path()), before);

    // Without a record limit the per-call byte ceiling alone has to split the history:
    // the fixture expands to more than twice the ceiling, so at least three slices are
    // required by arithmetic, not by timing.
    assert!(plain.len() > 2 * BUDGET.max_bytes, "{} bytes", plain.len());
    let by_bytes = RefreshBudget {
        max_records: 100_000,
        ..BUDGET
    };
    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let mut slices = 0;
    let mut expanded = 0;
    loop {
        let batch = reader.step(by_bytes).unwrap();
        assert!(batch.expanded_bytes <= BUDGET.max_bytes, "byte ceiling");
        expanded += batch.expanded_bytes;
        slices += 1;
        if batch.done {
            break;
        }
        assert!(slices < 1_000);
    }
    assert_eq!(expanded, plain.len());
    assert!(slices >= 3, "expanded in {slices} byte-bounded slices");
}

#[test]
fn record_limit_and_cancellation_between_slices() {
    let dir = tempfile::tempdir().unwrap();
    let mut plain = String::new();
    for index in 0..400 {
        plain.push_str(&format!("{{\"n\":{index}}}\n"));
    }
    let plain = plain.into_bytes();
    let archived_path = dir.path().join("cancel.jsonl.zst");
    fs::write(&archived_path, compress(&plain)).unwrap();
    let before = file_inventory(dir.path());

    let budget = RefreshBudget {
        max_records: 64,
        ..BUDGET
    };
    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let first = reader.step(budget).unwrap();
    assert_eq!(first.records.len(), 64);
    assert!(!first.done);
    drop(reader);

    let mut fresh = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let again = fresh.step(budget).unwrap();
    assert_eq!(
        normalize(
            &first
                .records
                .iter()
                .map(|r| (r.ordinal, r.item.clone()))
                .collect::<Vec<_>>()
        ),
        normalize(
            &again
                .records
                .iter()
                .map(|r| (r.ordinal, r.item.clone()))
                .collect::<Vec<_>>()
        )
    );
    assert_eq!(file_inventory(dir.path()), before);
}

#[test]
fn oversized_line_drains_in_bounded_slices_and_later_records_stay_readable() {
    let dir = tempfile::tempdir().unwrap();
    let mut plain = vec![b'x'; 5 * 1024 * 1024];
    plain.push(b'\n');
    plain.extend_from_slice(b"{\"after\":1}\n");
    plain.extend_from_slice(b"{\"after\":2}\n");
    let archived_path = dir.path().join("oversize.jsonl.zst");
    fs::write(&archived_path, compress(&plain)).unwrap();
    let before = file_inventory(dir.path());

    let budget = RefreshBudget {
        max_records: 1000,
        max_bytes: MAX_RECORD_BYTES,
        max_millis: 100,
    };
    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let mut skips = 0usize;
    let mut seen = Vec::new();
    let mut steps = 0usize;
    loop {
        let batch = reader.step(budget).unwrap();
        steps += 1;
        assert!(
            batch.expanded_bytes <= budget.max_bytes,
            "expanded {} over budget",
            batch.expanded_bytes
        );
        for record in batch.records {
            match record.item {
                SourceItem::Skipping => {
                    assert_eq!(record.ordinal, 0);
                    skips += 1;
                }
                item => seen.push((record.ordinal, item)),
            }
        }
        if batch.done {
            break;
        }
        assert!(steps < 10_000);
    }
    // Discarded in 64 KiB decoder chunks, never one source-sized buffer.
    let drained = 5 * 1024 * 1024 - MAX_RECORD_BYTES;
    assert!(skips >= drained / (64 * 1024) - 2, "{skips} skips");
    assert_eq!(
        seen.first().map(|(ordinal, item)| (*ordinal, item)),
        Some((0, &SourceItem::TooLarge))
    );
    let values: Vec<Value> = seen
        .iter()
        .filter_map(|(_, item)| match item {
            SourceItem::Json(value) => Some(value.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(values.len(), 2);
    assert_eq!(values[0]["after"], 1);
    assert_eq!(values[1]["after"], 2);
    assert_eq!(file_inventory(dir.path()), before);
}

#[test]
fn record_progress_is_pending_then_ready_and_never_a_decompressed_offset() {
    let dir = tempfile::tempdir().unwrap();
    let mut plain = String::new();
    for index in 0..30 {
        plain.push_str(&format!("{{\"n\":{index}}}\n"));
    }
    let archived_path = dir.path().join("progress.jsonl.zst");
    fs::write(&archived_path, compress(plain.as_bytes())).unwrap();
    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();

    let session = session(&archived_path);
    let locator = SourceLocator {
        session: session.clone(),
        kind: LocatorKind::CompressedRecord {
            path: archived_path.clone(),
            ordinal: 20,
        },
        generation: Some(0),
    };
    // One record per call: indexing is bounded and reports progress.
    let tiny = RefreshBudget {
        max_records: 1,
        max_bytes: 64,
        max_millis: 100,
    };
    let mut pendings = 0;
    let ready = loop {
        match reader.record(&locator, tiny).unwrap() {
            SourceRecordProgress::Pending => {
                pendings += 1;
                assert!(pendings < 10_000, "never reached the record");
            }
            other => break other,
        }
    };
    assert!(pendings > 10, "indexed in {pendings} slices");
    match ready {
        SourceRecordProgress::Ready(record) => {
            assert_eq!(record.raw, "{\"n\":20}");
            assert!(record.display.contains("20"));
            assert!(record.unavailable.is_none());
        }
        other => panic!("expected the record, got {other:?}"),
    }

    // Behind the cursor: a bounded restart serves it again.
    let earlier = SourceLocator {
        kind: LocatorKind::CompressedRecord {
            path: archived_path.clone(),
            ordinal: 3,
        },
        ..locator.clone()
    };
    match locator_of(&mut reader, &earlier) {
        SourceRecordProgress::Ready(record) => assert_eq!(record.raw, "{\"n\":3}"),
        other => panic!("expected a rewind, got {other:?}"),
    }

    // Wrong generation, path, and locator kind are all stale, never misread.
    let older = SourceLocator {
        generation: Some(9),
        ..locator.clone()
    };
    assert_eq!(locator_of(&mut reader, &older), SourceRecordProgress::Stale);
    let elsewhere = SourceLocator {
        kind: LocatorKind::CompressedRecord {
            path: dir.path().join("other.jsonl.zst"),
            ordinal: 3,
        },
        ..locator.clone()
    };
    assert_eq!(
        locator_of(&mut reader, &elsewhere),
        SourceRecordProgress::Stale
    );
    let wrong_kind = SourceLocator {
        kind: LocatorKind::FileOffset {
            path: archived_path.clone(),
            offset: 0,
        },
        ..locator.clone()
    };
    assert_eq!(
        locator_of(&mut reader, &wrong_kind),
        SourceRecordProgress::Stale
    );
    let beyond = SourceLocator {
        kind: LocatorKind::CompressedRecord {
            path: archived_path,
            ordinal: 999,
        },
        ..locator
    };
    assert_eq!(
        locator_of(&mut reader, &beyond),
        SourceRecordProgress::Stale
    );
}

#[test]
fn plain_locators_read_the_exact_record_and_reject_mid_line_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let plain_path = dir.path().join("plain.jsonl");
    fs::write(&plain_path, b"{\"a\":1}\n{\"b\":2}\nlast").unwrap();
    let mut reader = SourceReader::open(&plain_path, MAX_RECORD_BYTES).unwrap();
    let (items, _) = collect(&mut reader, BUDGET);
    assert_eq!(
        items
            .iter()
            .filter(|(_, item)| matches!(item, SourceItem::Json(_)))
            .count(),
        2
    );
    assert_eq!(items.last().unwrap().1, SourceItem::Incomplete);

    let session = session(&plain_path);
    let at = |offset: u64| SourceLocator {
        session: session.clone(),
        kind: LocatorKind::FileOffset {
            path: plain_path.clone(),
            offset,
        },
        generation: Some(0),
    };
    match locator_of(&mut reader, &at(8)) {
        SourceRecordProgress::Ready(record) => {
            assert_eq!(record.raw, "{\"b\":2}");
            assert!(record.unavailable.is_none());
        }
        other => panic!("expected the second record, got {other:?}"),
    }
    assert_eq!(locator_of(&mut reader, &at(9)), SourceRecordProgress::Stale);
    assert_eq!(
        locator_of(&mut reader, &at(999)),
        SourceRecordProgress::Stale
    );
}

#[test]
fn an_unchanged_archive_is_not_decoded_again() {
    let dir = tempfile::tempdir().unwrap();
    let archived_path = dir.path().join("stable.jsonl.zst");
    fs::write(&archived_path, compress(&synthetic_rollout())).unwrap();
    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();

    let (items, expanded) = collect(&mut reader, BUDGET);
    assert!(!items.is_empty());
    assert!(expanded > 0);

    // A refresh of an unchanged archive reads no source bytes and stays in its generation.
    for _ in 0..3 {
        let idle = reader.step(BUDGET).unwrap();
        assert!(idle.done);
        assert_eq!(idle.expanded_bytes, 0, "nothing is decompressed again");
        assert!(idle.records.is_empty());
        assert!(!idle.invalidated);
        assert_eq!(reader.generation(), 0);
    }
}

#[test]
fn truncated_frame_delivers_the_readable_prefix_then_one_damage_report() {
    let dir = tempfile::tempdir().unwrap();
    let mut plain = String::new();
    for index in 0..20_000 {
        plain.push_str(&format!(
            "{{\"type\":\"response_item\",\"payload\":{{\"type\":\"message\",\"role\":\"user\",\
             \"content\":[{{\"type\":\"input_text\",\"text\":\"secret-marker {index}\"}}]}}}}\n"
        ));
    }
    let full = compress(plain.as_bytes());
    let cut = &full[..full.len() * 6 / 10];
    let archived_path = dir.path().join("truncated.jsonl.zst");
    fs::write(&archived_path, cut).unwrap();
    let before = file_inventory(dir.path());

    let mut reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let (items, _) = collect(&mut reader, BUDGET);
    let damaged: Vec<&(u64, SourceItem)> = items
        .iter()
        .filter(|(_, item)| matches!(item, SourceItem::Damaged(_)))
        .collect();
    assert_eq!(damaged.len(), 1, "one damage report per source");
    assert_eq!(items.last().unwrap(), damaged[0], "damage ends the stream");
    assert_eq!(
        items.last().unwrap().1,
        SourceItem::Damaged(SourceDamage::Truncated)
    );
    let records = items
        .iter()
        .filter(|(_, item)| matches!(item, SourceItem::Json(_)))
        .count();
    assert!(records > 1000, "readable prefix was delivered: {records}");
    // Routine diagnostics stay content-free.
    let debug = format!("{items:?}");
    assert!(!debug.contains("secret-marker"));
    assert_eq!(file_inventory(dir.path()), before);
}

#[test]
fn corrupt_archive_warns_only_for_its_own_source() {
    let dir = tempfile::tempdir().unwrap();
    let healthy = dir.path().join("healthy.jsonl.zst");
    fs::write(&healthy, compress(&synthetic_rollout())).unwrap();
    let corrupt = dir.path().join("corrupt.jsonl.zst");
    fs::write(&corrupt, vec![0x51u8; 64]).unwrap();
    let before = file_inventory(dir.path());

    let mut broken = SourceReader::open(&corrupt, MAX_RECORD_BYTES).unwrap();
    let (items, _) = collect(&mut broken, BUDGET);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].1, SourceItem::Damaged(SourceDamage::Corrupt));

    let (healthy_items, _) = collect(
        &mut SourceReader::open(&healthy, MAX_RECORD_BYTES).unwrap(),
        BUDGET,
    );
    assert!(
        healthy_items
            .iter()
            .all(|(_, item)| !matches!(item, SourceItem::Damaged(_)))
    );
    assert_eq!(file_inventory(dir.path()), before);
}

/// P2: a plain locator must not be answered from a file that was replaced underneath.
#[test]
fn plain_locator_from_a_replaced_file_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swap.jsonl");
    fs::write(&path, b"{\"old\":1}\n{\"old\":2}\n").unwrap();
    let mut reader = SourceReader::open(&path, MAX_RECORD_BYTES).unwrap();
    let batch = reader.step(BUDGET).unwrap();
    assert_eq!(batch.records.len(), 2);
    assert_eq!(reader.generation(), 0);
    let session = session(&path);
    let first = batch.records[0].source_locator(session.clone());

    // Replaced between slices: the locator names content that is gone. The file is now
    // shorter than what this reader consumed, which the cursor's stamp rules detect.
    fs::write(&path, b"{\"new\":1}\n").unwrap();
    assert_eq!(
        locator_of(&mut reader, &first),
        SourceRecordProgress::Stale,
        "a replaced plain file must not answer with old bytes"
    );
    assert_eq!(reader.generation(), 1, "the check advanced the generation");

    let batch = reader.step(BUDGET).unwrap();
    assert!(!batch.invalidated, "the replacement was already reported");
    assert_eq!(batch.records[0].ordinal, 0);
    assert_eq!(batch.records[0].generation, 1);
}

/// P2: rotating into an empty file still invalidates locators and ordinals.
#[test]
fn truncating_to_an_empty_file_is_a_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.jsonl");
    fs::write(&path, b"{\"a\":1}\n{\"a\":2}\n").unwrap();
    let mut reader = SourceReader::open(&path, MAX_RECORD_BYTES).unwrap();
    let (items, _) = collect(&mut reader, BUDGET);
    assert_eq!(items.len(), 2);
    let locator = SourceLocator {
        session: session(&path),
        kind: LocatorKind::FileOffset {
            path: path.clone(),
            offset: 8,
        },
        generation: Some(0),
    };
    assert!(matches!(
        locator_of(&mut reader, &locator),
        SourceRecordProgress::Ready(_)
    ));

    fs::write(&path, b"").unwrap();
    let batch = reader.step(BUDGET).unwrap();
    assert!(
        batch.invalidated,
        "rotation into an empty file is a generation"
    );
    assert!(batch.done);
    assert!(batch.records.is_empty());
    assert_eq!(reader.generation(), 1);
    assert_eq!(
        locator_of(&mut reader, &locator),
        SourceRecordProgress::Stale
    );

    // The ordinal counter restarted with the new generation.
    fs::write(&path, b"{\"b\":1}\n").unwrap();
    let batch = reader.step(BUDGET).unwrap();
    assert!(!batch.invalidated);
    assert_eq!(batch.records[0].ordinal, 0);
    assert_eq!(batch.records[0].generation, 1);

    // A rotation that happens before the reader reached the old end is reported too.
    let mut text = String::new();
    for index in 0..10 {
        text.push_str(&format!("{{\"n\":{index}}}\n"));
    }
    fs::write(&path, &text).unwrap();
    let mut reader = SourceReader::open(&path, MAX_RECORD_BYTES).unwrap();
    let small = RefreshBudget {
        max_records: 2,
        ..BUDGET
    };
    let partial = reader.step(small).unwrap();
    assert_eq!(partial.records.len(), 2);
    assert!(!partial.done);
    fs::write(&path, b"").unwrap();
    // Bytes the cursor already buffered are still framed first; the truncation becomes a
    // new generation as soon as that buffer drains, without a caller restart.
    let mut reported = false;
    for _ in 0..20 {
        let batch = reader.step(small).unwrap();
        if batch.invalidated {
            reported = true;
            break;
        }
        assert!(!batch.done);
    }
    assert!(reported, "a drained truncation is a new generation");
    assert_eq!(reader.generation(), 1);
}

/// P2: archive records are terminator-free, exactly like plain ones.
#[test]
fn crlf_records_read_the_same_from_plain_and_archive() {
    let dir = tempfile::tempdir().unwrap();
    let plain = b"{\"a\":1}\r\n{\"b\":2}\r\n";
    let (plain_path, archived_path) = rollout(dir.path(), "crlf.jsonl", plain);

    for path in [&plain_path, &archived_path] {
        let mut reader = SourceReader::open(path, MAX_RECORD_BYTES).unwrap();
        let batch = reader.step(BUDGET).unwrap();
        assert_eq!(batch.records.len(), 2);
        let second = batch.records[1].source_locator(session(path));
        match locator_of(&mut reader, &second) {
            SourceRecordProgress::Ready(record) => {
                assert_eq!(record.raw, "{\"b\":2}", "no CR survived in {:?}", path);
                assert_eq!(record.display, "{\"b\":2}");
                assert!(record.unavailable.is_none());
            }
            other => panic!("expected the second record, got {other:?}"),
        }
    }
}

/// Residual contract: a trailing partial line is not a record in either representation.
#[test]
fn a_partial_trailing_line_is_stale_in_both_representations() {
    let dir = tempfile::tempdir().unwrap();
    let plain = b"{\"a\":1}\n{\"tail";
    let (plain_path, archived_path) = rollout(dir.path(), "partial.jsonl", plain);

    let mut plain_reader = SourceReader::open(&plain_path, MAX_RECORD_BYTES).unwrap();
    let plain_batch = plain_reader.step(BUDGET).unwrap();
    assert_eq!(plain_batch.records.len(), 2);
    assert_eq!(plain_batch.records[1].item, SourceItem::Incomplete);
    let plain_last = plain_batch.records[1].source_locator(session(&plain_path));
    assert_eq!(
        locator_of(&mut plain_reader, &plain_last),
        SourceRecordProgress::Stale,
        "a partial line is not a complete record"
    );

    let mut archived_reader = SourceReader::open(&archived_path, MAX_RECORD_BYTES).unwrap();
    let archived_batch = archived_reader.step(BUDGET).unwrap();
    assert_eq!(archived_batch.records.len(), 2);
    assert_eq!(archived_batch.records[1].item, SourceItem::Incomplete);
    let archived_last = archived_batch.records[1].source_locator(session(&archived_path));
    assert_eq!(
        locator_of(&mut archived_reader, &archived_last),
        SourceRecordProgress::Stale,
        "a partial line has no ordinal to serve"
    );

    // The complete record both representations do have reads identically.
    let plain_first = plain_batch.records[0].source_locator(session(&plain_path));
    let archived_first = archived_batch.records[0].source_locator(session(&archived_path));
    match (
        locator_of(&mut plain_reader, &plain_first),
        locator_of(&mut archived_reader, &archived_first),
    ) {
        (
            SourceRecordProgress::Ready(plain_record),
            SourceRecordProgress::Ready(archived_record),
        ) => {
            assert_eq!(plain_record.raw, archived_record.raw);
            assert_eq!(plain_record.display, archived_record.display);
        }
        other => panic!("expected both records, got {other:?}"),
    }
}

/// P3: an oversized line whose cap byte is the CR of its terminator is still oversized.
#[test]
fn a_cap_sized_prefix_ending_in_cr_is_not_a_complete_record() {
    let dir = tempfile::tempdir().unwrap();
    let plain_path = dir.path().join("cap.jsonl");
    // "abcd\rX\n": four bytes of content, a CR, then more content. A cap of 4 must not
    // accept "abcd" as the record.
    fs::write(&plain_path, b"abcd\rX\n").unwrap();
    let mut reader = SourceReader::open(&plain_path, 4).unwrap();
    let batch = reader.step(BUDGET).unwrap();
    assert_eq!(batch.records.len(), 1);
    assert_eq!(batch.records[0].item, SourceItem::TooLarge);
    let locator = batch.records[0].source_locator(session(&plain_path));
    match locator_of(&mut reader, &locator) {
        SourceRecordProgress::Ready(record) => {
            assert!(
                record
                    .unavailable
                    .as_deref()
                    .is_some_and(|reason| reason.contains("4-byte limit")),
                "unexpected record {record:?}"
            );
            assert!(record.raw.is_empty());
        }
        other => panic!("expected an unavailable oversized record, got {other:?}"),
    }

    // The legitimate CRLF slack still fits: content of exactly the cap is a record.
    let slack_path = dir.path().join("slack.jsonl");
    fs::write(&slack_path, b"abcd\r\n").unwrap();
    let (slack_plain, slack_archived) = rollout(dir.path(), "slack2.jsonl", b"abcd\r\n");
    for path in [&slack_path, &slack_plain, &slack_archived] {
        let mut reader = SourceReader::open(path, 4).unwrap();
        let batch = reader.step(BUDGET).unwrap();
        assert_eq!(batch.records.len(), 1);
        assert_eq!(batch.records[0].item, SourceItem::Invalid, "{path:?}");
        let locator = batch.records[0].source_locator(session(path));
        match locator_of(&mut reader, &locator) {
            SourceRecordProgress::Ready(record) => assert_eq!(record.raw, "abcd"),
            other => panic!("expected the capped record, got {other:?}"),
        }
    }
}

/// Residual contract: a plain slice stays inside its byte allowance.
#[test]
fn plain_slices_stay_inside_the_byte_allowance() {
    let dir = tempfile::tempdir().unwrap();
    let mut plain = String::new();
    for index in 0..400 {
        plain.push_str(&format!("{{\"n\":{index:04}}}\n"));
    }
    let path = dir.path().join("slice.jsonl");
    fs::write(&path, &plain).unwrap();
    let budget = RefreshBudget {
        max_records: 100_000,
        max_bytes: 1024,
        max_millis: 100,
    };
    let mut reader = SourceReader::open(&path, 64).unwrap();
    let mut slices = 0usize;
    let mut expanded = 0usize;
    let mut records = 0usize;
    loop {
        let batch = reader.step(budget).unwrap();
        assert!(
            batch.expanded_bytes <= budget.max_bytes,
            "slice expanded {} bytes",
            batch.expanded_bytes
        );
        slices += 1;
        expanded += batch.expanded_bytes;
        records += batch.records.len();
        if batch.done {
            break;
        }
        assert!(slices < 10_000);
    }
    assert_eq!(records, 400);
    assert_eq!(expanded, plain.len());
    assert!(slices >= plain.len() / budget.max_bytes, "{slices} slices");
}

fn collect_locators(reader: &mut SourceReader, budget: RefreshBudget) -> Vec<SourceRecordItem> {
    let mut out = Vec::new();
    for _ in 0..1_000_000 {
        let batch = reader.step(budget).unwrap();
        out.extend(batch.records);
        if batch.done {
            return out;
        }
    }
    panic!("source did not finish");
}

/// P2: oversized framing is budgeted too — one line can never eat a whole slice.
#[test]
fn an_oversized_line_never_consumes_more_than_the_slice_allowance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.jsonl");
    let mut bytes = vec![b'x'; 2048];
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"after\":1}\n");
    fs::write(&path, &bytes).unwrap();

    let budget = RefreshBudget {
        max_records: 1000,
        max_bytes: 1024,
        max_millis: 100,
    };
    let mut reader = SourceReader::open(&path, 64).unwrap();
    let mut slices = 0usize;
    let mut expanded = 0usize;
    let mut skips = 0usize;
    let mut records = Vec::new();
    loop {
        let batch = reader.step(budget).unwrap();
        assert!(
            batch.expanded_bytes <= budget.max_bytes,
            "one slice consumed {} of a {} byte line",
            batch.expanded_bytes,
            bytes.len()
        );
        slices += 1;
        expanded += batch.expanded_bytes;
        for record in batch.records {
            match record.item {
                SourceItem::Skipping => skips += 1,
                item => records.push((record.ordinal, item)),
            }
        }
        if batch.done {
            break;
        }
        assert!(slices < 100, "no progress");
    }
    // 2062 bytes over a 1024-byte allowance: the line is framed across slices.
    assert!(slices >= 3, "{slices} slices");
    assert!(skips >= 2, "{skips} discards");
    assert_eq!(expanded, bytes.len());
    assert_eq!(records.len(), 2);
    assert_eq!(records[0], (0, SourceItem::TooLarge));
    assert!(matches!(records[1].1, SourceItem::Json(_)));
}

/// P2: an unterminated oversized tail is not a record in either representation, while a
/// terminated oversized line still resolves to the same unavailable record.
#[test]
fn an_oversized_unterminated_tail_is_stale_in_both_representations() {
    const CAP: usize = 64;
    let dir = tempfile::tempdir().unwrap();

    // Trailing oversized partial line: no terminator anywhere after it.
    let mut partial = Vec::new();
    partial.extend_from_slice(b"{\"a\":1}\n");
    partial.extend(std::iter::repeat_n(b'y', 512));
    let (plain_tail, archived_tail) = rollout(dir.path(), "tail.jsonl", &partial);
    for path in [&plain_tail, &archived_tail] {
        let mut reader = SourceReader::open(path, CAP).unwrap();
        let items = collect_locators(&mut reader, BUDGET);
        let last = items.last().unwrap();
        assert_eq!(last.item, SourceItem::Incomplete, "{path:?}");
        assert_eq!(
            locator_of(&mut reader, &last.source_locator(session(path))),
            SourceRecordProgress::Stale,
            "an unterminated oversized tail is not a record: {path:?}"
        );
    }

    // The same line with its terminator is a complete, oversized record in both.
    let mut complete = Vec::new();
    complete.extend_from_slice(b"{\"a\":1}\n");
    complete.extend(std::iter::repeat_n(b'y', 512));
    complete.push(b'\n');
    let (plain_full, archived_full) = rollout(dir.path(), "full.jsonl", &complete);
    for path in [&plain_full, &archived_full] {
        let mut reader = SourceReader::open(path, CAP).unwrap();
        let items = collect_locators(&mut reader, BUDGET);
        let oversized = items
            .iter()
            .find(|record| record.item == SourceItem::TooLarge)
            .unwrap_or_else(|| panic!("no oversized record in {path:?}"));
        match locator_of(&mut reader, &oversized.source_locator(session(path))) {
            SourceRecordProgress::Ready(record) => {
                assert!(record.raw.is_empty());
                assert!(
                    record
                        .unavailable
                        .as_deref()
                        .is_some_and(|reason| reason.contains("64-byte limit")),
                    "unexpected {record:?}"
                );
            }
            other => panic!("expected the oversized record, got {other:?}"),
        }
    }
}

/// P2: proving an oversized line's completion is bounded by the caller's allowance.
#[test]
fn plain_locator_completion_scan_stays_within_the_budget() {
    const CAP: usize = 64;
    const TAIL: usize = 4 * 1024 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let budget = RefreshBudget {
        max_records: 1,
        max_bytes: 64 * 1024,
        max_millis: 100,
    };

    for terminated in [true, false] {
        let path = dir.path().join(if terminated {
            "terminated.jsonl"
        } else {
            "unterminated.jsonl"
        });
        let mut bytes = vec![b'z'; TAIL];
        if terminated {
            bytes.push(b'\n');
        }
        fs::write(&path, &bytes).unwrap();
        let mut reader = SourceReader::open(&path, CAP).unwrap();
        let locator = SourceLocator {
            session: session(&path),
            kind: LocatorKind::FileOffset {
                path: path.clone(),
                offset: 0,
            },
            generation: Some(0),
        };

        let mut calls = 0usize;
        let outcome = loop {
            calls += 1;
            assert!(calls < 200, "the scan must resume, not restart");
            match reader.record(&locator, budget).unwrap() {
                SourceRecordProgress::Pending => continue,
                other => break other,
            }
        };
        // 4 MiB at 64 KiB per call is far more work than one call may do.
        assert!(calls >= TAIL / budget.max_bytes - 1, "{calls} calls");
        if terminated {
            match outcome {
                SourceRecordProgress::Ready(record) => {
                    assert!(record.raw.is_empty());
                    assert!(
                        record
                            .unavailable
                            .as_deref()
                            .is_some_and(|reason| reason.contains("64-byte limit")),
                        "unexpected {record:?}"
                    );
                }
                other => panic!("expected the oversized record, got {other:?}"),
            }
        } else {
            assert_eq!(
                outcome,
                SourceRecordProgress::Stale,
                "a line that never ends is not a record"
            );
        }
    }
}

/// P2: a rotation that happens at the start of a slice must not corrupt the byte delta.
#[test]
fn rotation_before_a_slice_keeps_byte_accounting_in_the_new_generation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotate.jsonl");
    let mut first = vec![b'a'; 4096];
    first.push(b'\n');
    fs::write(&path, &first).unwrap();
    let mut reader = SourceReader::open(&path, 4096).unwrap();
    let (items, expanded) = collect(&mut reader, BUDGET);
    assert_eq!(items.len(), 1);
    assert_eq!(expanded, first.len());

    // Shorter than what was consumed: the next slice rotates before reading anything.
    let mut second = vec![b'b'; 2048];
    second.push(b'\n');
    fs::write(&path, &second).unwrap();

    let budget = RefreshBudget {
        max_records: 1000,
        max_bytes: 1024,
        max_millis: 100,
    };
    let rotated = reader.step(budget).unwrap();
    assert!(rotated.invalidated, "the truncation is a new generation");
    assert_eq!(reader.generation(), 1);
    assert!(rotated.records.is_empty());
    assert_eq!(
        rotated.expanded_bytes, budget.max_bytes,
        "the slice counts the new file's bytes from its first byte"
    );
    assert!(!rotated.done);

    let mut records = Vec::new();
    let mut expanded = 0;
    let mut done = false;
    while !done {
        let batch = reader.step(budget).unwrap();
        assert!(!batch.invalidated);
        assert!(batch.expanded_bytes <= budget.max_bytes);
        expanded += batch.expanded_bytes;
        records.extend(batch.records);
        done = batch.done;
    }
    assert_eq!(expanded, second.len() - budget.max_bytes);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].ordinal, 0);
    assert_eq!(records[0].generation, 1);
}

// ---- Codex archive discovery and canonical identity ----------------------

/// One synthetic Codex rollout with a recorded cumulative counter.
fn codex_rollout(id: &str, prompt: &str, total: u64) -> Vec<u8> {
    let mut lines = vec![
        cx::session_meta("2026-10-29T10:00:00Z", id, "/p"),
        cx::user("2026-10-29T10:00:01Z", prompt),
        cx::turn_context("2026-10-29T10:00:02Z", "gpt-5.5", "/p"),
        cx::usage_record_thread(
            "2026-10-29T10:00:03Z",
            id,
            cx::usage(total.saturating_sub(10), 0, 10, 0, total),
        ),
    ];
    lines.push(cx::assistant("2026-10-29T10:00:04Z", "done"));
    let mut body = lines.join("\n");
    body.push('\n');
    body.into_bytes()
}

fn write_rollout(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn write_archive(path: &Path, bytes: &[u8]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, compress(bytes)).unwrap();
}

fn latest(
    found: &[agentscope::domain::SessionMetadata],
) -> Vec<agentscope::domain::SessionMetadata> {
    let mut out: Vec<agentscope::domain::SessionMetadata> = Vec::new();
    for meta in found {
        match out.iter_mut().find(|m| m.key == meta.key) {
            Some(seen) => *seen = meta.clone(),
            None => out.push(meta.clone()),
        }
    }
    out
}

fn codex_keys(found: &[agentscope::domain::SessionMetadata]) -> Vec<SessionKey> {
    latest(found).into_iter().map(|m| m.key).collect()
}

#[test]
fn codex_live_and_archived_representations_are_one_logical_session() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let rollouts = sessions.join("2026").join("10").join("29");
    let bytes_a = codex_rollout("a", "live one", 150);
    let bytes_b = codex_rollout("b", "archived one", 90);
    let plain_a = rollouts.join("rollout-a.jsonl");
    let zst_a = rollouts.join("rollout-a.jsonl.zst");
    let plain_b = archived.join("rollout-b.jsonl");
    let zst_b = archived.join("rollout-b.jsonl.zst");
    write_rollout(&plain_a, &bytes_a);
    write_archive(&zst_a, &bytes_a);
    write_rollout(&plain_b, &bytes_b);
    write_archive(&zst_b, &bytes_b);

    let config = support::config_for_store(&sessions, &archived);
    let before = file_inventory(dir.path());
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let rows = latest(&found);
    assert_eq!(
        rows.len(),
        2,
        "plain/zstd and live/archived copies are one row each: {:?}",
        codex_keys(&found)
    );
    let a = rows.iter().find(|m| m.key.native_id == "a").unwrap();
    let b = rows.iter().find(|m| m.key.native_id == "b").unwrap();
    assert_eq!(
        a.key.source,
        SourceKey::File(plain_a.clone()),
        "the canonical key is the plain native path"
    );
    assert_eq!(b.key.source, SourceKey::File(plain_b.clone()));
    assert!(!a.archived, "a live rollout is not archived");
    assert!(b.archived, "a rollout from the archived store is archived");

    let a_summary = support::world::hydrate_complete(&mut scanner, &a.key, NOW);
    let b_summary = support::world::hydrate_complete(&mut scanner, &b.key, NOW);
    assert_eq!(a_summary.usage.as_ref().and_then(|u| u.total), Some(150));
    assert_eq!(b_summary.usage.as_ref().and_then(|u| u.total), Some(90));
    assert_eq!(
        file_inventory(dir.path()),
        before,
        "discovery and hydration never write a provider file"
    );
}

#[test]
fn codex_in_place_compression_preserves_the_confirmed_key() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let rollouts = sessions.join("2026").join("10").join("29");
    let bytes = codex_rollout("keep", "compress me", 150);
    let plain = rollouts.join("rollout-keep.jsonl");
    let zst = rollouts.join("rollout-keep.jsonl.zst");
    write_rollout(&plain, &bytes);
    let config = support::config_for_store(&sessions, &archived);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let first = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let key = latest(&first).remove(0).key;
    assert_eq!(key.source, SourceKey::File(plain.clone()));
    assert_eq!(
        support::world::hydrate_complete(&mut scanner, &key, NOW)
            .usage
            .and_then(|u| u.total),
        Some(150)
    );

    // Codex compresses an idle rollout in place and leaves no plain copy.
    fs::remove_file(&plain).unwrap();
    write_archive(&zst, &bytes);
    let second = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert!(
        second.removed.is_empty(),
        "compression is not a session removal: {:?}",
        second.removed
    );
    assert!(latest(&second.sessions).iter().any(|m| m.key == key));
    assert_eq!(
        support::world::hydrate_complete(&mut scanner, &key, NOW)
            .usage
            .and_then(|u| u.total),
        Some(150),
        "the compressed current source answers for the confirmed key"
    );
    assert!(zst.exists());
}

#[test]
fn codex_archival_move_preserves_the_confirmed_key_and_rejects_old_locators() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let rollouts = sessions.join("2026").join("10").join("29");
    let bytes = codex_rollout("move", "archive me", 150);
    let live = rollouts.join("rollout-move.jsonl");
    write_rollout(&live, &bytes);
    let config = support::config_for_store(&sessions, &archived);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let first = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let key = latest(&first).remove(0).key;
    assert_eq!(key.source, SourceKey::File(live.clone()));

    // The native archive moves the same rollout filename to the archived store.
    let archived_plain = archived.join("rollout-move.jsonl");
    fs::create_dir_all(&archived).unwrap();
    fs::rename(&live, &archived_plain).unwrap();
    let second = support::world::discover_pass(&mut scanner, NOW, BIG);
    assert!(
        second.removed.is_empty(),
        "an archive move is not a removal: {:?}",
        second.removed
    );
    let moved = latest(&second.sessions);
    let row = moved
        .iter()
        .find(|m| m.key == key)
        .expect("same logical key");
    assert!(row.archived, "the current source is the archived store");
    assert_eq!(
        support::world::hydrate_complete(&mut scanner, &key, NOW)
            .usage
            .and_then(|u| u.total),
        Some(150)
    );

    // Compress the archived copy and record a locator against its generation.
    let archived_zst = archived.join("rollout-move.jsonl.zst");
    write_archive(&archived_zst, &bytes);
    fs::remove_file(&archived_plain).unwrap();
    support::world::discover_pass(&mut scanner, NOW, BIG);
    support::world::hydrate_complete(&mut scanner, &key, NOW);
    let page = loop {
        match scanner
            .transcript_page(
                support::world::new_job(),
                &key,
                None,
                PageDirection::Backward,
                PageRequest {
                    max_events: 1,
                    max_bytes: 1_000_000,
                },
                BIG,
            )
            .unwrap()
        {
            ReadProgress::Pending(_) => continue,
            ReadProgress::Ready(page) => break page,
        }
    };
    let locator = page.events[0].provenance.locator.clone();
    assert!(
        matches!(
            &locator.kind,
            LocatorKind::CompressedRecord { path, .. } if path == &archived_zst
        ),
        "the current compressed physical source is named: {locator:?}"
    );
    let job = support::world::new_job();
    let first_record = scanner.source_record(job, &locator, BIG);
    assert!(
        matches!(first_record, Ok(ReadProgress::Ready(_))),
        "the current archived source serves its record: {first_record:?}"
    );

    // A real replacement is a new generation; the old locator is stale.
    let replaced = codex_rollout("move", "replaced content", 170);
    write_archive(&archived_zst, &replaced);
    support::world::hydrate_complete(&mut scanner, &key, NOW);
    let stale = scanner.source_record(job, &locator, BIG);
    assert!(
        stale.is_err(),
        "a locator from the replaced source is rejected: {stale:?}"
    );
}

#[test]
fn codex_conflicting_same_id_copies_stay_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let rollouts = sessions.join("2026").join("10").join("29");
    let bytes = codex_rollout("same", "one", 10);
    let other = codex_rollout("same", "two", 20);
    write_rollout(&rollouts.join("rollout-one.jsonl"), &bytes);
    write_rollout(&rollouts.join("rollout-two.jsonl"), &other);
    let config = support::config_for_store(&sessions, &archived);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let rows = latest(&found);
    assert_eq!(
        rows.len(),
        2,
        "same-ID copies without native representation proof stay distinct"
    );
    for meta in rows {
        let summary = support::world::hydrate_complete(&mut scanner, &meta.key, NOW);
        assert!(
            summary
                .warnings
                .iter()
                .any(|w| w.key.source.contains("#duplicate-id")),
            "the ambiguity is visible"
        );
    }
}

#[test]
fn codex_archived_transcript_pages_collect_lookahead_and_order_anchors() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let mut lines = vec![positions_session_meta("z", "/p")];
    // Recorded timestamps decrease, so ordinal order differs from anchor order.
    lines.push(cx::user("2026-10-29T10:00:05Z", "later prompt"));
    lines.push(cx::assistant("2026-10-29T10:00:01Z", "earlier answer"));
    lines.push(cx::user("2026-10-29T10:00:03Z", "middle prompt"));
    let mut body = lines.join("\n");
    body.push('\n');
    let zst = archived.join("rollout-z.jsonl.zst");
    write_archive(&zst, body.as_bytes());
    let config = support::config_for_store(&sessions, &archived);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let key = latest(&found).remove(0).key;
    support::world::hydrate_complete(&mut scanner, &key, NOW);

    let request = PageRequest {
        max_events: 1,
        max_bytes: 1_000_000,
    };
    let job = support::world::new_job();
    let mut seen = Vec::new();
    let mut anchor = None;
    let mut pages = 0;
    loop {
        let page = loop {
            match scanner
                .transcript_page(
                    job,
                    &key,
                    anchor.clone(),
                    PageDirection::Backward,
                    request,
                    BIG,
                )
                .unwrap()
            {
                ReadProgress::Pending(_) => continue,
                ReadProgress::Ready(page) => break page,
            }
        };
        pages += 1;
        assert!(pages < 10, "every page finishes");
        assert!(
            page.events.len() <= 1,
            "the page limit is honored: {}",
            page.events.len()
        );
        seen.extend(
            page.events
                .iter()
                .filter_map(|e| e.text.as_ref().map(|t| t.preview.clone())),
        );
        if !page.has_more {
            break;
        }
        anchor = page.next.clone();
        assert!(anchor.is_some(), "a continuing page has an anchor");
    }
    assert_eq!(
        seen,
        ["later prompt", "middle prompt", "earlier answer"],
        "backward pages follow the anchor total order and never fail on lookahead"
    );

    // Count after the earliest event's own anchor: the two later events remain.
    let page = loop {
        match scanner
            .transcript_page(
                job,
                &key,
                None,
                PageDirection::Backward,
                PageRequest {
                    max_events: 100,
                    max_bytes: 1_000_000,
                },
                BIG,
            )
            .unwrap()
        {
            ReadProgress::Pending(_) => continue,
            ReadProgress::Ready(page) => break page,
        }
    };
    let earliest = page.events.first().expect("an event");
    let anchor = agentscope::domain::PageAnchor {
        timestamp: earliest.timestamp,
        source_order: earliest.source_order,
        stable_id: earliest.id.clone(),
    };
    let count = loop {
        match scanner
            .count_events_after(support::world::new_job(), &key, &anchor, BIG)
            .unwrap()
        {
            ReadProgress::Pending(_) => continue,
            ReadProgress::Ready(count) => break count,
        }
    };
    assert_eq!(count, 2, "counts use the sorted anchor index");
}

fn positions_session_meta(id: &str, cwd: &str) -> String {
    cx::session_meta("2026-10-29T10:00:00Z", id, cwd)
}

#[test]
fn codex_live_compressed_rollout_uses_compressed_addressing_and_pages() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let rollouts = sessions.join("2026").join("10").join("29");
    let bytes = codex_rollout("live-zst", "compressed live", 150);
    let zst = rollouts.join("rollout-live-zst.jsonl.zst");
    write_archive(&zst, &bytes);
    let config = support::config_for_store(&sessions, &archived);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let meta = latest(&found).remove(0);
    assert!(
        !meta.archived,
        "a zstd copy in the live store is not an archived session"
    );
    assert_eq!(
        meta.key.source,
        SourceKey::File(rollouts.join("rollout-live-zst.jsonl")),
        "the key is the canonical plain path"
    );
    let summary = support::world::hydrate_complete(&mut scanner, &meta.key, NOW);
    assert_eq!(summary.usage.as_ref().and_then(|u| u.total), Some(150));

    // Byte addressing would fail here; compressed addressing must serve the page.
    let job = support::world::new_job();
    let page = loop {
        match scanner
            .transcript_page(
                job,
                &meta.key,
                None,
                PageDirection::Backward,
                PageRequest {
                    max_events: 50,
                    max_bytes: 1_000_000,
                },
                BIG,
            )
            .unwrap()
        {
            ReadProgress::Pending(_) => continue,
            ReadProgress::Ready(page) => break page,
        }
    };
    assert!(page.events.iter().any(|e| {
        e.text
            .as_ref()
            .is_some_and(|t| t.preview.contains("compressed live"))
    }));
    assert!(
        page.events.iter().all(|e| matches!(
            e.provenance.locator.kind,
            LocatorKind::CompressedRecord { .. }
        )),
        "events of a compressed source carry compressed locators"
    );
    // The emitted locator reads the record back successfully.
    let locator = page.events[0].provenance.locator.clone();
    let record = loop {
        match scanner
            .source_record(support::world::new_job(), &locator, BIG)
            .unwrap()
        {
            ReadProgress::Pending(_) => continue,
            ReadProgress::Ready(record) => break record,
        }
    };
    assert!(record.unavailable.is_none(), "{record:?}");
    assert!(!record.raw.is_empty(), "the raw record content is served");
}

#[test]
fn codex_old_archive_locator_is_stale_after_state_eviction() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    for id in ["one", "two"] {
        write_archive(
            &archived.join(format!("rollout-{id}.jsonl.zst")),
            &codex_rollout(id, "work", 20),
        );
    }
    let mut config = support::config_for_store(&sessions, &archived);
    config.max_cached_sessions = 1;
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let rows = latest(&found);
    let one = rows
        .iter()
        .find(|m| m.key.native_id == "one")
        .unwrap()
        .key
        .clone();
    let two = rows
        .iter()
        .find(|m| m.key.native_id == "two")
        .unwrap()
        .key
        .clone();
    support::world::hydrate_complete(&mut scanner, &one, NOW);
    let job = support::world::new_job();
    let page = loop {
        match scanner
            .transcript_page(
                job,
                &one,
                None,
                PageDirection::Backward,
                PageRequest {
                    max_events: 1,
                    max_bytes: 1_000_000,
                },
                BIG,
            )
            .unwrap()
        {
            ReadProgress::Pending(_) => continue,
            ReadProgress::Ready(page) => break page,
        }
    };
    let locator = page.events[0].provenance.locator.clone();
    // Another session hydrates and evicts the first session's state (cache 1).
    support::world::hydrate_complete(&mut scanner, &two, NOW);
    // Rehydration creates a fresh scanner-owned generation.
    support::world::hydrate_complete(&mut scanner, &one, NOW);
    let stale = scanner.source_record(support::world::new_job(), &locator, BIG);
    assert!(
        stale.is_err(),
        "an evicted/rehydrated archive never accepts an old generation: {stale:?}"
    );
}

#[test]
fn codex_archive_page_retention_stays_bounded_with_many_hidden_records() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let mut lines = vec![positions_session_meta("bounded", "/p")];
    // Every hidden record is larger than the eventual page itself.
    for i in 0..1_000 {
        lines.push(cx::record(
            "2026-10-29T10:00:05Z",
            "world_state",
            serde_json::json!({"index": i, "data": "x".repeat(6_000)}),
        ));
    }
    lines.push(cx::user("2026-10-29T10:00:10Z", "older visible"));
    lines.push(cx::assistant("2026-10-29T10:00:20Z", "newer visible"));
    let mut body = lines.join("\n");
    body.push('\n');
    let zst = archived.join("rollout-bounded.jsonl.zst");
    write_archive(&zst, body.as_bytes());
    let config = support::config_for_store(&sessions, &archived);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let key = latest(&found).remove(0).key;
    support::world::hydrate_complete(&mut scanner, &key, NOW);

    let job = support::world::new_job();
    let slice = RefreshBudget {
        max_records: 32,
        max_bytes: 128 * 1024,
        max_millis: 5_000,
    };
    let mut max_records = 0usize;
    let mut max_bytes = 0usize;
    let page = loop {
        match scanner
            .transcript_page(
                job,
                &key,
                None,
                PageDirection::Forward,
                PageRequest {
                    max_events: 1,
                    max_bytes: 1_000_000,
                },
                slice,
            )
            .unwrap()
        {
            ReadProgress::Pending(_) => {
                if let Some((records, bytes)) = scanner.page_retained(&key, job) {
                    max_records = max_records.max(records);
                    max_bytes = max_bytes.max(bytes);
                }
                continue;
            }
            ReadProgress::Ready(page) => break page,
        }
    };
    assert!(
        max_records <= 210,
        "only the lookahead rows and the diagnostic cap are retained, got {max_records}"
    );
    assert!(
        max_bytes <= 2_000_000,
        "retained expanded bytes stay near the diagnostic cap, got {max_bytes}"
    );
    assert!(
        page.has_more,
        "the second displayed event is still the lookahead"
    );
    assert_eq!(page.events.len(), 1);
    assert!(page.events.iter().any(|e| {
        e.text
            .as_ref()
            .is_some_and(|t| t.preview.contains("older visible"))
    }));
}

#[test]
fn archived_activity_prefers_recorded_time_and_marks_mtime_as_approximate() {
    let dir = tempfile::tempdir().unwrap();
    let sessions = dir.path().join("sessions");
    let archived = dir.path().join("archived_sessions");
    let rollouts = sessions.join("2026").join("10").join("29");
    fs::create_dir_all(&rollouts).unwrap();
    fs::create_dir_all(&archived).unwrap();

    // No recorded timestamp: discovery falls back to the archive's mtime and
    // labels it derived/approximate rather than inventing a reported time.
    let no_time = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"archive-no-time\"}}\n";
    let zst = rollouts.join("rollout-no-time.jsonl.zst");
    write_archive(&zst, no_time);
    support::set_modified(&zst, 1_700_000_000);
    let config = support::config_for_store(&sessions, &archived);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let meta = latest(&found)
        .into_iter()
        .find(|m| m.key.native_id == "archive-no-time")
        .expect("archive listed");
    assert_eq!(meta.activity.at, Some(1_700_000_000_000));
    assert_eq!(
        meta.activity.class,
        agentscope::domain::AggregationClass::Derived
    );

    // A recorded timestamp is reported and determines the exact order.
    let timed = b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"archive-timed\"},\
                  \"timestamp\":\"2026-10-29T10:00:00Z\"}\n";
    let zst = rollouts.join("rollout-timed.jsonl.zst");
    write_archive(&zst, timed);
    let found = support::world::discover_pass(&mut scanner, NOW, BIG).sessions;
    let meta = latest(&found)
        .into_iter()
        .find(|m| m.key.native_id == "archive-timed")
        .expect("timed archive listed");
    assert_eq!(meta.activity.at, Some(1_793_268_000_000));
    assert_eq!(
        meta.activity.class,
        agentscope::domain::AggregationClass::Reported
    );
}
