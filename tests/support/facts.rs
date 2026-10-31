//! Fixture constructors for canonical session identity and metadata. The values
//! are only convenient construction defaults, not statements about product defaults.

use std::path::Path;

use agentscope::domain::{
    ActivityStamp, Harness, Lifecycle, Lineage, LocatorKind, ModelDetail, ProcessAssociation,
    Provenance, RecordedMoney, ReleaseState, RequestMetadata, SessionKey, SessionMetadata,
    SessionMetrics, SessionSummary, SourceKey, SourceLocator, TokenAccounting, TokenCategories,
    UsageObservation, UsageScope,
};
use agentscope::pricing::PriceSchedule;

/// One synthetic request observation: `id` is the confirmed request id, the
/// route is `provider` (never a harness), and the scope is a single message.
pub fn usage(
    id: &str,
    provider: Option<&str>,
    model: &str,
    categories: TokenCategories,
    accounting: TokenAccounting,
) -> UsageObservation {
    let path = std::path::PathBuf::from("synthetic.jsonl");
    let key = file_key(Harness::Claude, "synthetic-session", &path);
    UsageObservation {
        model: Some(ModelDetail {
            id: model.to_owned(),
            provider: provider.map(str::to_owned),
            variant: None,
            effort: None,
            configured_effort: None,
        }),
        scope: UsageScope::Message,
        categories,
        accounting,
        request: Some(RequestMetadata {
            id: Some(id.to_owned()),
            ..RequestMetadata::default()
        }),
        money: RecordedMoney::default(),
        provenance: Provenance {
            harness: Harness::Claude,
            source: SourceKey::File(path.clone()),
            locator: SourceLocator {
                session: key,
                kind: LocatorKind::FileOffset { path, offset: 0 },
                generation: None,
            },
            record_type: "assistant".into(),
            harness_version: None,
        },
    }
}

/// Parses a deterministic synthetic catalog from a JSON array of cards.
pub fn price_schedule(cards_json: &str) -> PriceSchedule {
    PriceSchedule::from_json(&format!(
        r#"{{"schema_version":1,"revision":"test","checked_at":"2026-10-30","sources":[],"inventory":[],"cards":{cards_json}}}"#
    ))
    .expect("synthetic price cards")
}

pub fn file_key(harness: Harness, id: &str, path: &Path) -> SessionKey {
    SessionKey {
        harness,
        native_id: id.to_owned(),
        source: SourceKey::File(path.to_path_buf()),
    }
}

pub fn metadata(key: SessionKey, title: &str) -> SessionMetadata {
    SessionMetadata {
        key,
        title: title.to_owned(),
        title_derived: false,
        project: None,
        cwd: None,
        model: None,
        origins: Vec::new(),
        created_at: None,
        activity: ActivityStamp::default(),
        lineage: Lineage::default(),
        members: Vec::new(),
        archived: false,
        released: ReleaseState::Unknown,
        source_available: true,
        harness_version: None,
    }
}

pub fn summary(meta: SessionMetadata) -> SessionSummary {
    SessionSummary {
        meta,
        usage: None,
        metrics: SessionMetrics::default(),
        lifecycle: Lifecycle::Unknown,
        unfinished_work: false,
        last_event_at: None,
        last_completion: None,
        displayed_event_count: 0,
        hidden_records: 0,
        server_tools: None,
        process: ProcessAssociation::NotChecked,
        warnings: Vec::new(),
    }
}
