//! Hermes native `state.db` support.
//!
//! Discovery, the narrow read-only access boundary, pure logical lineage and the
//! aggregate usage reconciler live here. The bounded hydration core is a private
//! scanner component: it is not a registered `ProviderReader` and advertises no
//! product capability. Nothing in this module writes, migrates, repairs, copies,
//! or loads an extension in a provider database, and no provider code or
//! configuration is executed.

pub mod discovery;
pub mod hydration;
pub mod lineage;
pub mod schema;
pub mod usage;

pub use discovery::HermesCatalog;
pub use hydration::{
    HermesClosure, HermesHydration, HermesLastTurn, classify_closure, classify_turn_closure,
};
pub use lineage::{
    HermesEdge, HermesLineage, HermesLogicalSession, HermesMarkers, HermesRelation, HermesRowMeta,
    HermesUnresolved, classify_edge, resolve_chains, resolve_lineage,
};
pub use usage::{
    HermesCostStatus, HermesModelTotals, HermesMoney, HermesRowId, HermesSessionTotals,
    HermesTokens, HermesUsageContribution, HermesUsageReconciler, HermesUsageReport,
    HermesUsageScope,
};
