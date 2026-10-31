use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[test]
fn inspects_synthetic_messages_safely_and_reports_bad_records() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "agentscope-inspect-{}-{unique}.jsonl",
        std::process::id()
    ));
    let data = concat!(
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"synthetic-id\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"hello\"}]}}\n",
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"hello\"}}\n",
        "{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hi \\u001b[31m sk-test-secret password = synthetic-password --password synthetic-private-value --api-key synthetic-private-key --password = synthetic-spaced-secret Authorization: Bearer synthetic-bearer\"}]}}\n",
        "not json\n",
        "{\"type\":\"partial\"}"
    );
    fs::write(&path, data).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("inspect")
        .arg(&path)
        .output()
        .unwrap();
    let original = fs::read(&path).unwrap();
    fs::remove_file(path).unwrap();
    assert!(output.status.success());
    assert_eq!(original, data.as_bytes());
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stdout.contains("synthetic-id"));
    assert_eq!(stdout.matches("hello").count(), 1);
    assert!(stdout.contains("hi"));
    assert!(!stdout.contains('\u{1b}'));
    assert!(!stdout.contains("sk-test-secret"));
    assert!(!stdout.contains("synthetic-password"));
    assert!(!stdout.contains("synthetic-bearer"));
    assert!(!stdout.contains("synthetic-private-value"));
    assert!(!stdout.contains("synthetic-private-key"));
    assert!(!stdout.contains("synthetic-spaced-secret"));
    assert!(stderr.contains("Invalid"));
    assert!(stderr.contains("Incomplete"));
}

#[test]
fn missing_input_fails_without_displaying_the_path() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .args(["inspect", "missing-synthetic-private-file.jsonl"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains("missing-synthetic-private-file")
    );
}

/// Content shared by the plain and archived fixtures: messages, an unusable line, an
/// oversized record, and a partial tail.
fn parity_rollout() -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"{\"type\":\"session_meta\",\"payload\":{\"id\":\"synthetic-id\"}}\n");
    data.extend_from_slice(
        b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"user\",\
          \"content\":[{\"type\":\"input_text\",\"text\":\"hello\"}]}}\n",
    );
    data.extend_from_slice(
        b"{\"type\":\"response_item\",\"payload\":{\"type\":\"message\",\"role\":\"assistant\",\
          \"content\":[{\"type\":\"output_text\",\"text\":\"hi \\u001b[31m sk-test-secret \
          --password synthetic-private-value\"}]}}\n",
    );
    data.extend_from_slice(b"not json\n");
    data.extend_from_slice(b"{\"type\":\"oversized\",\"payload\":\"");
    data.extend(std::iter::repeat_n(b'z', 1024 * 1024 + 4096));
    data.extend_from_slice(b"\"}\n");
    data.extend_from_slice(b"{\"type\":\"partial\"}");
    data
}

/// Anchors are the only difference between the two representations.
fn without_anchor(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(|line| match line.split_once(']') {
            Some((head, tail)) if head.starts_with('[') => {
                let label = head.rsplit_once(" @").map_or(head, |(label, _)| label);
                format!("{label}]{tail}")
            }
            _ => line.to_string(),
        })
        .collect()
}

#[test]
fn compressed_inspection_matches_the_plain_rollout() {
    let dir = tempfile::tempdir().unwrap();
    let plain_path = dir.path().join("parity.jsonl");
    let archived_path = dir.path().join("parity.jsonl.zst");
    let data = parity_rollout();
    fs::write(&plain_path, &data).unwrap();
    fs::write(
        &archived_path,
        zstd::stream::encode_all(&data[..], 3).unwrap(),
    )
    .unwrap();
    let before = (
        fs::read(&plain_path).unwrap(),
        fs::read(&archived_path).unwrap(),
    );

    let run = |path: &std::path::Path| {
        Command::new(env!("CARGO_BIN_EXE_agentscope"))
            .arg("inspect")
            .arg(path)
            .output()
            .unwrap()
    };
    let plain = run(&plain_path);
    let archived = run(&archived_path);
    assert!(plain.status.success());
    assert!(archived.status.success());

    let plain_out = String::from_utf8(plain.stdout).unwrap();
    let archived_out = String::from_utf8(archived.stdout).unwrap();
    assert_eq!(without_anchor(&plain_out), without_anchor(&archived_out));
    assert!(plain_out.contains("Session: synthetic-id"));
    assert!(plain_out.contains("hello"));
    assert!(plain_out.contains("hi"));
    for output in [&plain_out, &archived_out] {
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains("sk-test-secret"));
        assert!(!output.contains("synthetic-private-value"));
        // The oversized record's content is never echoed.
        assert!(!output.contains("zzzz"));
    }

    let plain_err = String::from_utf8(plain.stderr).unwrap();
    let archived_err = String::from_utf8(archived.stderr).unwrap();
    assert!(plain_err.contains("Invalid JSON record at byte "));
    assert!(plain_err.contains("Oversized record at byte "));
    assert!(plain_err.contains("Incomplete record at byte "));
    assert!(archived_err.contains("Invalid JSON record at record #"));
    assert!(archived_err.contains("Oversized record at record #"));
    assert!(archived_err.contains("Incomplete record at record #"));
    // Same diagnostics, in the same order, with the representation's own anchor.
    let kinds = |stderr: &str| -> Vec<String> {
        stderr
            .lines()
            .map(|line| line.split(" at ").next().unwrap_or_default().to_string())
            .collect()
    };
    assert_eq!(kinds(&plain_err), kinds(&archived_err));

    assert_eq!(
        (
            fs::read(&plain_path).unwrap(),
            fs::read(&archived_path).unwrap()
        ),
        before,
        "inspection never rewrites the source"
    );
}

#[test]
fn damaged_compressed_source_is_reported_without_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("damaged.jsonl.zst");
    let mut frame = zstd::stream::encode_all(&parity_rollout()[..], 3).unwrap();
    frame.truncate(frame.len() * 2 / 3);
    fs::write(&path, &frame).unwrap();
    let before = fs::read(&path).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("inspect")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Damaged source at record #"));
    assert!(stderr.contains("truncated compressed frame"));
    assert!(!stderr.contains("synthetic-private-value"));
    assert!(!String::from_utf8(output.stdout).unwrap().contains("zzzz"));
    assert_eq!(fs::read(&path).unwrap(), before);
}
