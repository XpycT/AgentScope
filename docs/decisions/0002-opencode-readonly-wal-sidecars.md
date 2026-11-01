# ADR-0002: Read-Only OpenCode Access and WAL Sidecars

**Status:** Accepted for v1; behavior verified on synthetic fixtures (2026-10-30), no correction required<br>
**Date:** 2026-10-29

## Context

OpenCode stores sessions in a live SQLite database (`opencode.db`) that uses WAL mode. SQLite documents that a connection opened with `SQLITE_OPEN_READONLY` may still create or update the `-shm`/`-wal` sidecars when they are needed for a WAL read ([SQLite WAL: Read-Only Databases](https://sqlite.org/wal.html#read_only_databases)). `PRAGMA query_only` prevents SQL writes but does not prevent SQLite's own sidecar management.

AgentScope's passive contract forbids modifying provider session content, databases, configuration, permissions, credentials, or runtime files. A zero-sidecar guarantee is not achievable with SQLite flags alone. The alternatives were:

- A custom read-only VFS/private shared-memory boundary: strict and live-capable, but a substantial unsafe/FFI component with new correctness risks.
- Refusing live-WAL access: strict and simple, but makes OpenCode unavailable while it is running, which defeats the live observability use case.
- Copying the database and WAL to an AgentScope-owned temporary location: provider untouched, but a 3.86 GiB copy with snapshot-consistency, privacy, and cleanup costs.
- Allowing SQLite's transient sidecars only: live-capable and pragmatic, but narrows the absolute “no writes” claim.

## Decision

AgentScope opens the OpenCode database read-only and **allows SQLite to create or update only its transient `-shm`/`-wal` sidecars** as a documented exception. It does not:

- write session content, the main database, provider configuration, permissions, or credentials;
- manually delete, repair, chmod, or copy provider files;
- use `immutable=1` on a live WAL database;
- infer or synthesize data when the database cannot be read.

A SQLite authorizer allows only read operations on the allowlisted session tables/columns and denies everything else. If the directory is not writable and a live WAL cannot be read without creating a sidecar, the provider is reported as `Unavailable` and retried later; permissions are never changed. The exception is disclosed in the README and tested by asserting that session content and the main database are unchanged while only transient sidecars may differ.

## Consequences

**Benefits:** live OpenCode sessions remain observable; no database copy; no low-level VFS in v1; no SQL writes; credential and account tables remain inaccessible.

**Limitations:** the product cannot claim that the provider directory is byte-for-byte untouched while OpenCode is live. `-shm`/`-wal` may be created or updated by SQLite as runtime state. Those files are not transcript content, but they are still writes in the provider directory.

**Testing:** read-only regression checks the main database and session files, the authorizer, the no-manual-repair rule, and the sidecar exception. A test fixture with a non-writable directory verifies the `Unavailable` path without chmod or copy.

## Revisit conditions

If a strict zero-sidecar guarantee becomes required, implement a reviewed read-only VFS/private-shm boundary or an explicit snapshot workflow, and record a new ADR. Do not silently tighten the promise without changing the implementation.

## Scope note (2026-11-01)

The same read-only SQLite behavior applies to the Hermes reader added later: `state.db` (including `profiles/<name>/state.db`) is opened through the shared read-only allowlist opener, and SQLite may likewise create or update its transient `-shm`/`-wal` sidecars. `tests/read_only.rs::hermes_profiles_and_decoy_credentials_leave_provider_content_unchanged` snapshots Hermes files before and after a full reader/worker/TUI session and permits only those sidecars, with every other database, record and decoy file byte-identical.

## Verification (2026-10-30)

What the implementation does matches the decision above. Evidence, all on synthetic fixtures unless stated:

- **Authorizer:** `tests/opencode_roster.rs::opens_read_only_with_authorizer_and_rejects_sensitive_tables` and `tests/read_only.rs::authorizer_denies_credential_tables_and_discovery_ignores_auth_files` — credential, account, share-URL, and write/schema/pragma statements are rejected at prepare time; `VACUUM` is refused at execution by the read-only connection.
- **Sidecar exception:** `tests/read_only.rs::provider_files_and_main_database_are_unchanged` and `full_worker_and_tui_session_leaves_provider_data_unchanged` snapshot every file (bytes, size, mtime, read-only flag) before and after a complete adapter/worker/TUI session against a live-WAL fixture; everything except `opencode.db-shm`/`opencode.db-wal` is identical and no other path appears or disappears. `tests/opencode_roster.rs::main_database_is_unchanged_and_only_sidecars_may_change` covers the same for the adapter alone.
- **WAL states:** a live writer with an existing `-shm`, a stale `-wal`, no active WAL, and a blocked `-shm` name are each covered in `tests/opencode_roster.rs`. The blocked case reports `Unavailable` and deletes/repairs nothing.
- **Unavailable on a non-writable directory:** the Unix permission variant (`read_only_directory_with_required_sidecar_shows_unavailable`) does not run on Windows; the Windows run uses the directory-occupies-`-shm` simulation instead. A true read-only-ACL directory on Windows and any macOS run are unverified.
- **Real data (Windows, opencode 2.0.29, read-only, counts only):** the adapter listed the developer's real OpenCode sessions and opened transcript pages, and a before/after content-hash inventory found no silent content change; length/mtime changes, sidecars, and added or removed paths are classified inconclusive rather than attributed to AgentScope (last run: 1 inconclusive file, not itemized). Whether OpenCode was running was not recorded.
- **No manual repair:** `tests/read_only.rs::unreadable_database_is_reported_unavailable_and_never_repaired` confirms a corrupt main file is reported and left byte-identical.
