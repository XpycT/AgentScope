//! AC-01 / AC-08: AgentScope never writes provider data. Session files, the main
//! OpenCode database, and permissions stay unchanged; under ADR-0002 only SQLite's
//! transient `-shm`/`-wal` sidecars may appear or change.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use agentscope::app::{App, Screen, SourceState};
use agentscope::domain::{Thresholds, WarningKind};
use agentscope::providers::opencode::open_read_only;
use agentscope::ui::{RosterContext, apply_worker_event, flush_requests, handle_key};
use agentscope::worker::{WorkerHandle, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use support::world::*;
use support::{NOW, Unreadable};

type Meta = BTreeMap<PathBuf, (u64, std::time::SystemTime, u32)>;

/// Unix: full mode bits (0644 -> 0600 is detected). Windows: only the read-only attribute;
/// NTFS ACLs are not inspected, so permission verification there is partial.
fn permission_bits(m: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode()
    }
    #[cfg(not(unix))]
    {
        u32::from(m.permissions().readonly())
    }
}

/// (len, mtime, permission bits) of every non-sidecar file; permission bits of directories.
fn metadata(world: &World) -> Meta {
    all_paths(world.dir.path())
        .into_iter()
        .filter(|p| {
            let n = p.to_string_lossy();
            !(n.ends_with("-shm") || n.ends_with("-wal") || n.ends_with("-journal"))
        })
        .filter_map(|rel| {
            let m = fs::metadata(world.dir.path().join(&rel)).ok()?;
            if m.is_dir() {
                // Directory mtimes legitimately move when a sidecar appears; permissions must not.
                return Some((rel, (0, std::time::UNIX_EPOCH, permission_bits(&m))));
            }
            Some((rel, (m.len(), m.modified().unwrap(), permission_bits(&m))))
        })
        .collect()
}

fn assert_only_adr0002_sidecars_added(world: &World, before: &[PathBuf]) {
    let after = all_paths(world.dir.path());
    for p in &after {
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            before.contains(p) || name == "opencode.db-shm" || name == "opencode.db-wal",
            "unexpected new path: {p:?}"
        );
    }
    for p in before {
        assert!(after.contains(p), "path removed: {p:?}");
    }
}

fn wait_for(
    app: &mut App,
    ctx: &mut RosterContext,
    worker: &WorkerHandle,
    done: impl Fn(&App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done(app) {
        assert!(Instant::now() < deadline, "worker did not deliver in time");
        if let Ok(ev) = worker.rx.recv_timeout(Duration::from_millis(50)) {
            apply_worker_event(app, ctx, ev);
        }
    }
}

#[test]
fn provider_files_and_main_database_are_unchanged() {
    let world = World::new();
    let before_files = snapshot(world.dir.path());
    let before_paths = all_paths(world.dir.path());
    let before_meta = metadata(&world);

    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    // A second pass hits the unchanged-source and cursor-reuse paths.
    let _ = exercise_everything(&mut readers);
    assert_eq!(surface.sessions.len(), 3, "all three providers were read");
    drop(readers);

    let after_files = snapshot(world.dir.path());
    assert_eq!(
        after_files.files, before_files.files,
        "session files, the main database, and credential decoys are byte-identical"
    );
    assert_eq!(
        metadata(&world),
        before_meta,
        "sizes, mtimes, permissions unchanged"
    );
    assert_only_adr0002_sidecars_added(&world, &before_paths);
    // The live writer still sees its own data: nothing was truncated or repaired.
    let n: i64 = world
        .writer
        .query_row("SELECT count(*) FROM session_message", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn full_worker_and_tui_session_leaves_provider_data_unchanged() {
    let world = World::new();
    let before = snapshot(world.dir.path());
    let before_paths = all_paths(world.dir.path());
    let before_meta = metadata(&world);

    let worker = WorkerHandle::spawn(world.readers(), world.config());
    let mut app = App::new(
        support::NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    wait_for(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 3);
    // Visit every session: open, expand, view its source record, then close the source
    // overlay and the inspector, and confirm we are back on the roster before moving on.
    // Start at the first row so `j` visits each session exactly once.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
    let mut visited = Vec::new();
    for _ in 0..3 {
        let selected = app.selected_key().expect("a selected session");
        assert!(
            !visited.contains(&selected),
            "each iteration is a new session"
        );
        visited.push(selected.clone());
        handle_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Inspector);
        flush_requests(&mut app, &worker);
        wait_for(&mut app, &mut ctx, &worker, |a| {
            a.transcript().is_some_and(|t| t.loaded())
        });
        assert_eq!(app.transcript().unwrap().key(), &selected);
        handle_key(&mut app, key(KeyCode::Enter));
        handle_key(&mut app, key(KeyCode::Char('v')));
        assert_eq!(app.screen, Screen::Source);
        flush_requests(&mut app, &worker);
        wait_for(&mut app, &mut ctx, &worker, |a| {
            a.source_view()
                .is_some_and(|v| !matches!(v.state, SourceState::Loading))
        });
        handle_key(&mut app, key(KeyCode::Esc));
        assert_eq!(
            app.screen,
            Screen::Inspector,
            "Esc closes the source overlay"
        );
        handle_key(&mut app, key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Roster, "second Esc returns to roster");
        assert!(app.transcript().is_none());
        assert_eq!(app.selected_key(), Some(selected));
        handle_key(&mut app, key(KeyCode::Char('j')));
    }
    let mut providers: Vec<_> = visited.iter().map(|k| format!("{:?}", k.harness)).collect();
    providers.sort();
    assert_eq!(providers, ["Claude", "Codex", "OpenCode"]);
    let _ = worker.tx.send(WorkerRequest::Refresh);
    assert!(worker.shutdown(), "worker exits cleanly");

    assert_eq!(snapshot(world.dir.path()).files, before.files);
    assert_eq!(metadata(&world), before_meta);
    assert_only_adr0002_sidecars_added(&world, &before_paths);
}

#[test]
fn authorizer_denies_credential_tables_and_discovery_ignores_auth_files() {
    let world = World::new();
    let conn = open_read_only(&world.db).expect("open");
    for sql in [
        "SELECT secret FROM credential",
        "SELECT secret FROM account",
        "SELECT secret FROM control_account",
        "SELECT share_url FROM session_share",
        "SELECT share_url FROM session_v2",
        "SELECT seq FROM session_message",
        "INSERT INTO session_v2(id, time_created, time_updated) VALUES ('x', 1, 1)",
        "UPDATE credential SET secret = ''",
        "DELETE FROM credential",
        "DROP TABLE credential",
        "PRAGMA wal_checkpoint(TRUNCATE)",
        "PRAGMA journal_mode = DELETE",
        "ATTACH DATABASE 'other.db' AS other",
    ] {
        // Message content is only reachable through the adapter's bounded queries.
        assert!(conn.prepare(sql).is_err(), "must be denied: {sql}");
    }
    // VACUUM has no authorizer action; the read-only connection refuses it at execution.
    assert!(conn.execute_batch("VACUUM").is_err());

    // Discovery lists only transcript sources; decoys named like credentials are skipped.
    let codex = support::codex_sources(&world.config());
    let claude = support::claude_sources(&world.config());
    assert_eq!(codex.sources.len(), 1);
    assert_eq!(claude.sources.len(), 1);
    for s in codex.sources.iter().chain(&claude.sources) {
        let name = s.path.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(
            !name.contains("auth") && !name.contains("credential"),
            "{name}"
        );
    }
    assert!(
        codex
            .warnings
            .iter()
            .chain(&claude.warnings)
            .all(|w| !w.message.to_lowercase().contains("auth.json")),
        "decoys are neither read nor reported"
    );
}

#[test]
fn unreadable_database_is_reported_unavailable_and_never_repaired() {
    // A directory holding a WAL-mode database with no sidecars and a read-only
    // *file system view* cannot be simulated portably; the non-writable-directory
    // behavior is covered in tests/opencode_roster.rs. Here: a corrupt main file is
    // reported, not repaired or replaced.
    let world = World::new();
    let bad = world.dir.path().join("opencode").join("garbage.db");
    fs::write(&bad, b"this is not a database but it contains secrets").unwrap();
    let before = fs::read(&bad).unwrap();
    let cli = agentscope::config::CliOverrides {
        opencode_db: Some(bad.clone()),
        ..Default::default()
    };
    let config =
        agentscope::config::Config::load_with_env(&cli, &agentscope::config::Env::new()).unwrap();
    let result = agentscope::providers::opencode::OpenCodeScanner::new(&config);
    if let Ok(mut scanner) = result {
        use agentscope::providers::ProviderReader;
        let outcome = scanner.discover(support::NOW, BIG);
        let reported = match outcome {
            Err(_) => true,
            Ok(d) => d.warnings.iter().any(|w| {
                matches!(
                    w.key.kind,
                    WarningKind::Unavailable | WarningKind::Unsupported | WarningKind::Io
                )
            }),
        };
        assert!(reported, "corrupt database must be surfaced");
    }
    assert_eq!(
        fs::read(&bad).unwrap(),
        before,
        "never repaired or truncated"
    );
}

/// AC-01: consuming an archived rollout through the real CLI, and through the source
/// reader directly, leaves the whole tree byte-identical.
#[test]
fn compressed_rollout_consumption_never_rewrites_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let mut plain = Vec::new();
    for index in 0..500 {
        plain.extend_from_slice(
            format!("{{\"type\":\"event_msg\",\"payload\":{{\"n\":{index}}}}}\n").as_bytes(),
        );
    }
    let plain_path = dir.path().join("rollout.jsonl");
    let archived_path = dir.path().join("rollout.jsonl.zst");
    fs::write(&plain_path, &plain).unwrap();
    fs::write(
        &archived_path,
        zstd::stream::encode_all(&plain[..], 3).unwrap(),
    )
    .unwrap();
    let before = file_inventory(dir.path());

    for path in [&plain_path, &archived_path] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_agentscope"))
            .arg("inspect")
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        let mut reader = agentscope::ingest::source::SourceReader::open(path, 1024 * 1024).unwrap();
        loop {
            let batch = reader
                .step(agentscope::domain::RefreshBudget {
                    max_records: 64,
                    max_bytes: 64 * 1024,
                    max_millis: 100,
                })
                .unwrap();
            if batch.done {
                break;
            }
        }
    }

    assert_eq!(
        file_inventory(dir.path()),
        before,
        "sizes, mtimes and content are unchanged by reading"
    );
}

/// AC-01: discovering, hydrating, paging, and serving a record from a live and an
/// archived Codex rollout leaves every provider file byte-identical.
#[test]
fn codex_archived_discovery_and_reads_never_rewrite_the_store() {
    use agentscope::domain::{PageDirection, PageRequest, SourceLocator};
    use agentscope::providers::codex::CodexScanner;
    use agentscope::providers::{ProviderReader, ReadProgress};
    use support::codex as cx;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let live = sessions
        .join("2026")
        .join("10")
        .join("29")
        .join("rollout-live.jsonl");
    let archived_path = archived.join("rollout-old.jsonl.zst");
    let live_bytes = [
        cx::session_meta("2026-10-29T10:00:00Z", "live", "/p"),
        cx::user("2026-10-29T10:00:01Z", "live prompt"),
        cx::usage_record_thread("2026-10-29T10:00:02Z", "live", cx::usage(10, 0, 5, 0, 15)),
    ];
    let mut live_body = live_bytes.join("\n");
    live_body.push('\n');
    fs::create_dir_all(live.parent().unwrap()).unwrap();
    fs::write(&live, &live_body).unwrap();
    let archived_bytes = [
        cx::session_meta("2026-10-28T10:00:00Z", "old", "/p"),
        cx::assistant("2026-10-28T10:00:01Z", "old answer"),
        cx::usage_record_thread("2026-10-28T10:00:02Z", "old", cx::usage(20, 0, 5, 0, 25)),
    ];
    let mut archived_body = archived_bytes.join("\n");
    archived_body.push('\n');
    fs::create_dir_all(&archived).unwrap();
    fs::write(
        &archived_path,
        zstd::stream::encode_all(archived_body.as_bytes(), 3).unwrap(),
    )
    .unwrap();
    let before = file_inventory(dir.path());

    let config = support::config_for_store(&sessions, &archived);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = discover_pass(&mut scanner, NOW, BIG);
    assert_eq!(found.sessions.len(), 2);
    for meta in &found.sessions {
        let summary = hydrate_complete(&mut scanner, &meta.key, NOW);
        assert!(summary.usage.is_some(), "usage is read from both stores");
        let job = new_job();
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
        assert!(!page.events.is_empty(), "the transcript is readable");
        // Serving a source record (plain offset or compressed ordinal) is the other
        // path that opens the physical file.
        let first = page.events[0].provenance.locator.clone();
        let locator: SourceLocator = first;
        let job = new_job();
        let record = loop {
            match scanner.source_record(job, &locator, BIG).unwrap() {
                ReadProgress::Pending(_) => continue,
                ReadProgress::Ready(record) => break record,
            }
        };
        assert!(record.unavailable.is_none(), "{record:?}");
        assert!(
            !record.raw.trim().is_empty(),
            "the record content is served"
        );
    }

    assert_eq!(
        file_inventory(dir.path()),
        before,
        "no provider file changed size, mtime or content"
    );
}

// ---- B7: the integrated native world ---------------------------------------------

fn load_every_root(app: &mut App, ctx: &mut RosterContext, worker: &WorkerHandle) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.total_sessions() != ROOT_COUNT {
        app.load_more_roots();
        flush_requests(app, worker);
        if let Ok(ev) = worker.rx.recv_timeout(Duration::from_millis(20)) {
            apply_worker_event(app, ctx, ev);
        }
        assert!(Instant::now() < deadline, "every root never loaded");
    }
}

#[test]
fn native_world_full_session_leaves_provider_content_unchanged_and_sidecars_classified() {
    use agentscope::domain::SessionKey;

    let world = World::native();
    let g = world.grown();
    let before = snapshot(world.dir.path());
    let before_paths = all_paths(world.dir.path());
    let before_meta = metadata(&world);

    // Every reader reads every session, page and source record, twice.
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    let _ = exercise_everything(&mut readers);
    assert_eq!(surface.sessions.len(), ROOT_COUNT + g.children.len());
    drop(readers);

    // Then the program: worker, App and keyboard over the damaged and compressed
    // sources as well as the healthy ones.
    let worker = WorkerHandle::spawn(world.readers(), world.config());
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    load_every_root(&mut app, &mut ctx, &worker);
    app.set_filter(agentscope::app::FilterState {
        include_released: true,
        ..Default::default()
    });
    let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
    let visit: Vec<&SessionKey> = vec![
        &g.zstd,
        &g.truncated_zstd,
        &g.partial_codex,
        &g.damaged_claude,
        &g.desktop_code,
        &g.desktop_released,
        &g.cowork,
        &g.secret_claude,
        &g.secret_codex,
        &g.secret_opencode,
        &g.unknown_billing,
        &g.opencode.parent,
    ];
    for target in visit {
        assert!(
            app.select_row(target),
            "{} is a visible root",
            target.native_id
        );
        handle_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Inspector);
        flush_requests(&mut app, &worker);
        wait_for(&mut app, &mut ctx, &worker, |a| {
            a.transcript().is_some_and(|t| t.loaded())
        });
        handle_key(&mut app, key(KeyCode::Char('v')));
        if app.screen == Screen::Source {
            flush_requests(&mut app, &worker);
            wait_for(&mut app, &mut ctx, &worker, |a| {
                a.source_view()
                    .is_some_and(|v| !matches!(v.state, SourceState::Loading))
            });
            handle_key(&mut app, key(KeyCode::Esc));
        }
        handle_key(&mut app, key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Roster);
    }
    let _ = worker.tx.send(WorkerRequest::Refresh);
    assert!(worker.shutdown(), "worker exits cleanly");

    // Provider content: byte-identical, sizes, mtimes and permissions unchanged.
    let after = snapshot(world.dir.path());
    assert_eq!(after.files, before.files);
    assert_eq!(metadata(&world), before_meta);
    assert_only_adr0002_sidecars_added(&world, &before_paths);
    // Sidecars are reported separately (ADR-0002): only the live OpenCode database's
    // own may differ. The locked database's copy is exactly as it was built.
    for (path, bytes) in &before.sidecars {
        let live = path.starts_with("opencode");
        if !live {
            assert_eq!(after.sidecars.get(path), Some(bytes), "{path:?}");
        }
    }
    for path in after.sidecars.keys() {
        assert!(
            before.sidecars.contains_key(path) || path.starts_with("opencode"),
            "a new sidecar outside the live database: {path:?}"
        );
    }
}

#[test]
fn native_world_locked_wal_is_unavailable_never_repaired_and_hides_nothing_else() {
    let world = World::native();
    let g = world.grown();
    let before = snapshot(world.dir.path());
    let before_paths = all_paths(world.dir.path());
    let shm = support::opencode::sidecar(&g.locked_db, "-shm");
    assert!(shm.is_dir());

    let mut config = world.locked_config();
    config.refresh_ms = 3_600_000;
    let worker = WorkerHandle::spawn(world.locked_readers(), config);
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    // Codex and Claude (including Desktop) roots all load; OpenCode contributes none.
    let healthy = g
        .roots
        .iter()
        .filter(|(k, _)| k.harness != agentscope::domain::Harness::OpenCode)
        .count();
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.total_sessions() != healthy {
        app.load_more_roots();
        flush_requests(&mut app, &worker);
        if let Ok(ev) = worker.rx.recv_timeout(Duration::from_millis(20)) {
            apply_worker_event(&mut app, &mut ctx, ev);
        }
        assert!(Instant::now() < deadline, "the healthy roots never loaded");
    }
    assert!(
        app.all_rows()
            .iter()
            .all(|r| r.session.meta.key.harness != agentscope::domain::Harness::OpenCode)
    );
    assert!(worker.shutdown());

    // Nothing is repaired, deleted, copied or created beside the locked database.
    assert!(shm.is_dir(), "the occupied -shm name is left alone");
    let after = snapshot(world.dir.path());
    assert_eq!(after.files, before.files);
    assert_eq!(after.sidecars, before.sidecars);
    assert_eq!(all_paths(world.dir.path()), before_paths);
}

// ---- OMP: provider stores and decoys stay untouched and unread ---------------------

#[test]
fn omp_sessions_and_decoy_stores_are_never_written_and_decoys_never_opened() {
    let world = World::with_omp();
    let plant = world.omp_plant.as_ref().expect("planted OMP family");
    let before_files = snapshot(world.dir.path());
    let before_paths = all_paths(world.dir.path());
    let before_meta = metadata(&world);

    // The credential/history/model/statistics decoys cannot be opened while this guard
    // holds them: a reader that tried would fail or warn, and the sessions are still
    // all found, so none of them was opened.
    let locks: Vec<Unreadable> = plant.decoys.iter().map(|p| Unreadable::new(p)).collect();
    let locked = locks.iter().filter(|l| l.active).count();

    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    let _ = exercise_everything(&mut readers);
    // The three base sessions plus parent, child, nested child, fork and secret session.
    assert_eq!(surface.sessions.len(), 8);
    let omp: Vec<_> = surface
        .sessions
        .iter()
        .filter(|s| s.meta.key.harness == agentscope::domain::Harness::Omp)
        .collect();
    assert_eq!(omp.len(), 5);
    for session in &omp {
        assert!(session.meta.source_available, "{:?}", session.meta.key);
    }
    for warning in &surface.warnings {
        let text = format!("{} {}", warning.key.source, warning.message);
        for decoy in &plant.decoys {
            let name = decoy.file_name().unwrap().to_string_lossy();
            assert!(
                !text.contains(name.as_ref()),
                "a decoy was reported: {text}"
            );
        }
    }
    // Every event's source record is served for every OMP session.
    for session in &omp {
        let reader = readers
            .iter_mut()
            .find(|r| r.harness() == session.meta.key.harness)
            .unwrap();
        let page = support::world::transcript_ready(
            reader.as_mut(),
            &session.meta.key,
            None,
            agentscope::domain::PageDirection::Backward,
            PAGE,
            BIG,
            support::world::STEPS,
        );
        assert!(!page.events.is_empty());
        for event in &page.events {
            let record = support::world::try_source(
                reader.as_mut(),
                &event.provenance.locator,
                BIG,
                support::world::STEPS,
            )
            .expect("source record");
            assert!(record.unavailable.is_none(), "{record:?}");
        }
    }
    drop(readers);
    for lock in locks {
        lock.release();
    }
    // Windows holds every decoy exclusively, so a stray open would have failed.
    #[cfg(windows)]
    assert_eq!(locked, plant.decoys.len(), "every decoy was locked");
    #[cfg(not(windows))]
    let _ = locked;

    assert_eq!(
        snapshot(world.dir.path()).files,
        before_files.files,
        "session files and decoys are byte-identical"
    );
    assert_eq!(metadata(&world), before_meta);
    assert_only_adr0002_sidecars_added(&world, &before_paths);
    let omp_after: Vec<_> = all_paths(&world.omp_root).into_iter().collect();
    assert!(
        !omp_after
            .iter()
            .any(|p| p.to_string_lossy().contains("-wal") || p.to_string_lossy().contains("-shm")),
        "no sidecar or runtime file appears beside OMP data"
    );
}

#[test]
fn omp_full_worker_and_tui_session_leaves_provider_data_unchanged() {
    let world = World::with_omp();
    let before = snapshot(world.dir.path());
    let before_paths = all_paths(world.dir.path());
    let before_meta = metadata(&world);

    let worker = WorkerHandle::spawn(world.readers(), world.config());
    let mut app = App::new(
        support::NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    wait_for(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 6);
    let plant = world.omp_plant.as_ref().unwrap();
    let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
    for target in [&plant.family.parent, &plant.fork, &plant.secret] {
        assert!(app.select_row(target), "{} is a root", target.native_id);
        handle_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Inspector);
        flush_requests(&mut app, &worker);
        wait_for(&mut app, &mut ctx, &worker, |a| {
            a.transcript().is_some_and(|t| t.loaded())
        });
        handle_key(&mut app, key(KeyCode::Char('v')));
        if app.screen == Screen::Source {
            flush_requests(&mut app, &worker);
            wait_for(&mut app, &mut ctx, &worker, |a| {
                a.source_view()
                    .is_some_and(|v| !matches!(v.state, SourceState::Loading))
            });
            handle_key(&mut app, key(KeyCode::Esc));
        }
        handle_key(&mut app, key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Roster);
    }
    let _ = worker.tx.send(WorkerRequest::Refresh);
    assert!(worker.shutdown(), "worker exits cleanly");

    assert_eq!(snapshot(world.dir.path()).files, before.files);
    assert_eq!(metadata(&world), before_meta);
    assert_only_adr0002_sidecars_added(&world, &before_paths);
}
