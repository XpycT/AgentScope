//! Shared read-only SQLite access for provider databases (ADR-0002).
//!
//! A database is opened `SQLITE_OPEN_READ_ONLY` with `query_only`, and a column
//! authorizer allows only `SELECT` of the caller's allowlisted tables/columns and
//! fixed functions. SQLite may create or update its transient `-shm`/`-wal`
//! sidecars; nothing here writes, deletes, repairs, copies, or uses `immutable=1`.
//! Each provider keeps its own [`Allowlist`]; this module owns only the lifecycle
//! they genuinely share.

use std::path::Path;
use std::time::Duration;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, ErrorCode, OpenFlags};

use super::ProviderError;

/// Marks the error a progress-handler interrupt produces.
pub(crate) const INTERRUPTED: &str = "interrupted at the work-slice deadline";

/// Tables with the only columns of each that SQL may read, plus the aggregate
/// functions it may call. Anything else is denied at prepare time.
pub(crate) struct Allowlist {
    pub tables: &'static [(&'static str, &'static [&'static str])],
    pub functions: &'static [&'static str],
}

fn authorize(allow: &'static Allowlist, ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select => Authorization::Allow,
        AuthAction::Read {
            table_name,
            column_name,
        } => {
            let allowed = allow.tables.iter().any(|(table, columns)| {
                table.eq_ignore_ascii_case(table_name)
                    && (column_name.is_empty()
                        || columns.iter().any(|c| c.eq_ignore_ascii_case(column_name)))
            });
            if allowed {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } => {
            if allow
                .functions
                .iter()
                .any(|f| f.eq_ignore_ascii_case(function_name))
            {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Deny,
    }
}

pub(crate) fn map_error(error: rusqlite::Error) -> ProviderError {
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
        | rusqlite::Error::SqlInputError { error: failure, .. } => match failure.code {
            ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked
            | ErrorCode::CannotOpen
            | ErrorCode::ReadOnly
            | ErrorCode::SystemIoFailure
            | ErrorCode::DiskFull
            | ErrorCode::OutOfMemory
            | ErrorCode::FileLockingProtocolFailed
            | ErrorCode::NoLargeFileSupport => ProviderError::Unavailable(error.to_string()),
            ErrorCode::OperationInterrupted => ProviderError::Unavailable(INTERRUPTED.into()),
            ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt => {
                ProviderError::Parse(error.to_string())
            }
            _ => ProviderError::Internal(error.to_string()),
        },
        _ => ProviderError::Internal(error.to_string()),
    }
}

/// True for the progress-handler interrupt, which ends a slice rather than failing
/// the source.
pub(crate) fn interrupted(error: &ProviderError) -> bool {
    matches!(error, ProviderError::Unavailable(m) if m.contains(INTERRUPTED))
}

/// Opens `path` read-only behind `allow`'s authorizer. The first schema read happens
/// here so a WAL that needs a sidecar SQLite cannot create surfaces as `Unavailable`
/// immediately rather than on a later query. `busy_ms` bounds lock waits so they fit
/// one work slice.
pub(crate) fn open_read_only_with(
    path: &Path,
    allow: &'static Allowlist,
    busy_ms: u64,
) -> Result<Connection, ProviderError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = Connection::open_with_flags(path, flags).map_err(map_error)?;
    conn.busy_timeout(Duration::from_millis(busy_ms.max(1)))
        .map_err(map_error)?;
    conn.pragma_update(None, "query_only", true)
        .map_err(map_error)?;
    conn.authorizer(Some(move |ctx: AuthContext<'_>| authorize(allow, ctx)))
        .map_err(map_error)?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(map_error)?;
    Ok(conn)
}

/// True when `sql` prepares. A SQLite "no such table/column" error means the shape
/// is absent; authorization and I/O failures are real errors.
pub(crate) fn shape_present(conn: &Connection, sql: &str) -> Result<bool, ProviderError> {
    match conn.prepare(sql) {
        Ok(_) => Ok(true),
        Err(
            rusqlite::Error::SqliteFailure(f, _) | rusqlite::Error::SqlInputError { error: f, .. },
        ) if f.code == ErrorCode::Unknown => Ok(false),
        Err(e) => Err(map_error(e)),
    }
}

pub(crate) fn has_rows(conn: &Connection, table: &str) -> Result<bool, ProviderError> {
    conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
        r.get::<_, bool>(0)
    })
    .map_err(map_error)
}
