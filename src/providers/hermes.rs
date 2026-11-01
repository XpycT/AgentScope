//! Hermes native `state.db` support.
//!
//! Discovery and the narrow read-only access boundary live here; transcript,
//! lineage and usage components are added with the complete reader. Nothing in
//! this module writes, migrates, repairs, copies, or loads an extension in a
//! provider database, and no provider code or configuration is executed.

pub mod discovery;
pub mod schema;

pub use discovery::HermesCatalog;
