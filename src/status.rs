//! Pure status synthesis (spec §9.3). No clock, no filesystem.

use crate::domain::{
    Lifecycle, ProcessAssociation, SessionSummary, Status, StatusLabel, Thresholds, Timestamp,
};

fn status(label: StatusLabel, evidence: impl Into<String>) -> Status {
    Status {
        label,
        evidence: evidence.into(),
    }
}

fn process_note(p: &ProcessAssociation) -> &'static str {
    match p {
        ProcessAssociation::Matched { .. } => "process observed",
        ProcessAssociation::Candidates { .. } => "process ambiguous",
        ProcessAssociation::Unmatched => "process unmatched",
        ProcessAssociation::NotChecked => "process not checked",
        ProcessAssociation::Unavailable => "process observation unavailable",
    }
}

pub fn synthesize(session: &SessionSummary, now: Timestamp, thresholds: &Thresholds) -> Status {
    // 1. Source unavailable wins over everything.
    if !session.meta.source_available {
        return status(
            StatusLabel::Unavailable,
            "source unavailable; state not inferred",
        );
    }
    // 2. Explicit terminal failure/cancel.
    match session.lifecycle {
        Lifecycle::Failed => return status(StatusLabel::Failed, "failure persisted by provider"),
        Lifecycle::Cancelled => {
            return status(StatusLabel::Cancelled, "cancellation persisted by provider");
        }
        _ => {}
    }

    let matched = matches!(session.process, ProcessAssociation::Matched { .. });
    let pnote = process_note(&session.process);
    let completed = session.last_completion.is_some() && !session.unfinished_work;
    let activity = [
        session.meta.activity.at,
        session.last_event_at,
        session.last_completion,
    ]
    .into_iter()
    .flatten()
    .max();
    let age_ms = activity.map(|t| now.saturating_sub(t).max(0) as u128);
    let quiet_ms = thresholds.quiet.as_millis();
    let stall_ms = thresholds.stall.as_millis();
    let is_quiet = age_ms.is_some_and(|a| a > quiet_ms);

    // 4. Matched process, latest turn completed: idle, never stalled.
    if matched && completed {
        return status(
            StatusLabel::Complete,
            "last turn completed; process observed",
        );
    }

    if matched {
        let Some(age) = age_ms else {
            return status(
                StatusLabel::Unknown,
                "process observed; no activity evidence",
            );
        };
        // 3. Matched process, unfinished work.
        if session.unfinished_work && age > stall_ms {
            return status(
                StatusLabel::PossiblyStalled,
                "process observed, unfinished work, no writes past stall threshold; a long-running tool may explain silence",
            );
        }
        if age > quiet_ms {
            return status(StatusLabel::Quiet, "process observed; no recent writes");
        }
        return status(StatusLabel::Active, "process observed; recent writes");
    }

    // 5. Explicit completion without a matched process: Complete only once quiet.
    if completed {
        return if is_quiet {
            status(
                StatusLabel::Complete,
                format!("last turn completed; {pnote}"),
            )
        } else {
            status(
                StatusLabel::Recent,
                format!("last turn completed recently; {pnote}"),
            )
        };
    }

    // 6/7/8. No process match.
    match age_ms {
        None => status(
            StatusLabel::Unknown,
            format!("no activity evidence; {pnote}"),
        ),
        Some(_) if is_quiet => status(StatusLabel::Quiet, format!("no recent writes; {pnote}")),
        Some(_) => status(StatusLabel::Recent, format!("recent writes; {pnote}")),
    }
}
