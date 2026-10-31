use std::path::Path;
use std::time::Duration;

mod support;

use agentscope::domain::*;
use agentscope::status::synthesize;
use support::facts::{self, file_key, metadata};

fn matched() -> ProcessAssociation {
    ProcessAssociation::Matched {
        pid: 1,
        started_at: None,
    }
}
fn unmatched() -> ProcessAssociation {
    ProcessAssociation::Unmatched
}
fn unavailable() -> ProcessAssociation {
    ProcessAssociation::Unavailable
}

fn thresholds() -> Thresholds {
    Thresholds {
        quiet: Duration::from_secs(300),
        stall: Duration::from_secs(900),
    }
}

fn key() -> SessionKey {
    file_key(Harness::Codex, "s", Path::new("a"))
}

fn summary(
    process: ProcessAssociation,
    last: Timestamp,
    completion: Option<Timestamp>,
    unfinished: bool,
    source_available: bool,
) -> SessionSummary {
    let mut meta = metadata(key(), "t");
    meta.activity = ActivityStamp {
        at: Some(last),
        class: AggregationClass::Reported,
    };
    meta.source_available = source_available;
    SessionSummary {
        last_event_at: Some(last),
        last_completion: completion,
        unfinished_work: unfinished,
        process,
        ..facts::summary(meta)
    }
}

#[test]
fn status_truth_table() {
    let now = 1_000_000;
    let cases = [
        (
            "matched recent",
            matched(),
            999_000,
            None,
            true,
            true,
            StatusLabel::Active,
        ),
        (
            "matched quiet",
            matched(),
            600_000,
            None,
            true,
            true,
            StatusLabel::Quiet,
        ),
        (
            "matched stalled",
            matched(),
            0,
            None,
            true,
            true,
            StatusLabel::PossiblyStalled,
        ),
        (
            "matched completed",
            matched(),
            0,
            Some(0),
            false,
            true,
            StatusLabel::Complete,
        ),
        (
            "matched completed then work",
            matched(),
            999_000,
            Some(0),
            true,
            true,
            StatusLabel::Active,
        ),
        (
            "unmatched recent",
            unmatched(),
            999_000,
            None,
            true,
            true,
            StatusLabel::Recent,
        ),
        (
            "unmatched completed quiet",
            unmatched(),
            0,
            Some(0),
            false,
            true,
            StatusLabel::Complete,
        ),
        (
            "recent completion is recent",
            unmatched(),
            999_000,
            Some(999_000),
            false,
            true,
            StatusLabel::Recent,
        ),
        (
            "source unavailable",
            unmatched(),
            999_000,
            None,
            false,
            false,
            StatusLabel::Unavailable,
        ),
        (
            "process unavailable, source available",
            unavailable(),
            999_000,
            None,
            true,
            true,
            StatusLabel::Recent,
        ),
    ];
    for (name, process, last, completion, unfinished, source_available, expected) in cases {
        let status = synthesize(
            &summary(process, last, completion, unfinished, source_available),
            now,
            &thresholds(),
        );
        assert_eq!(status.label, expected, "{name}: {}", status.evidence);
    }
}

#[test]
fn explicit_failure_and_cancel_win_over_process_state() {
    let now = 1_000_000;
    let mut s = summary(matched(), 999_000, None, true, true);
    s.lifecycle = Lifecycle::Failed;
    assert_eq!(
        synthesize(&s, now, &thresholds()).label,
        StatusLabel::Failed
    );
    s.lifecycle = Lifecycle::Cancelled;
    assert_eq!(
        synthesize(&s, now, &thresholds()).label,
        StatusLabel::Cancelled
    );
    s.meta.source_available = false;
    assert_eq!(
        synthesize(&s, now, &thresholds()).label,
        StatusLabel::Unavailable
    );
}

#[test]
fn unmatched_quiet_and_unknown() {
    let now = 1_000_000;
    let quiet = summary(unmatched(), 0, None, true, true);
    assert_eq!(
        synthesize(&quiet, now, &thresholds()).label,
        StatusLabel::Quiet
    );
    let mut none = summary(unmatched(), 0, None, false, true);
    none.last_event_at = None;
    none.meta.activity.at = None;
    assert_eq!(
        synthesize(&none, now, &thresholds()).label,
        StatusLabel::Unknown
    );
}

#[test]
fn stall_unreachable_without_process_match_or_with_completion() {
    let now = 1_000_000;
    assert_eq!(
        synthesize(
            &summary(unavailable(), 0, None, true, true),
            now,
            &thresholds()
        )
        .label,
        StatusLabel::Quiet
    );
    assert_ne!(
        synthesize(
            &summary(matched(), 0, Some(0), false, true),
            now,
            &thresholds()
        )
        .label,
        StatusLabel::PossiblyStalled
    );
}

#[test]
fn event_total_order_handles_missing_timestamps() {
    let mut anchors = [
        PageAnchor {
            timestamp: None,
            source_order: 2,
            stable_id: "b".into(),
        },
        PageAnchor {
            timestamp: None,
            source_order: 1,
            stable_id: "a".into(),
        },
        PageAnchor {
            timestamp: Some(1),
            source_order: 0,
            stable_id: "c".into(),
        },
    ];
    anchors.sort();
    let keys: Vec<_> = anchors.iter().map(|a| a.stable_id.as_str()).collect();
    assert_eq!(keys, vec!["a", "b", "c"]);
}

#[test]
fn debug_does_not_leak_previews() {
    let text = BoundedText {
        preview: "SENTINEL_SECRET".into(),
        byte_len: 15,
        truncated: false,
        locator: None,
    };
    assert!(!format!("{text:?}").contains("SENTINEL_SECRET"));
    assert!(!format!("{text:#?}").contains("SENTINEL_SECRET"));
    let mut s = summary(unmatched(), 0, None, false, true);
    s.meta.title = "SENTINEL_TITLE".into();
    s.meta.cwd = Some("/SENTINEL_CWD".into());
    s.meta.project = Some("SENTINEL_PROJECT".into());
    let dbg = format!("{s:?}");
    assert!(!dbg.contains("SENTINEL"), "{dbg}");
}
