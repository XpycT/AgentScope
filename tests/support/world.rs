//! A three-provider synthetic "home directory" with credential decoys, used by the
//! read-only, privacy, and acceptance suites. Everything here is synthetic.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::providers::ProviderReader;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::omp::OmpScanner;
use agentscope::providers::opencode::OpenCodeScanner;
use rusqlite::Connection;
use serde_json::{Value, json};

use super::codex as cx;
use super::opencode as oc;
use super::{dated, write_lines};

/// Planted in every credential decoy. It must never reach any output.
pub const SENTINEL: &str = "SENTINEL-CRED-7f3a91";
/// Planted in transcript text in credential-looking shapes. It must be masked.
pub const INLINE_SECRET: &str = "SENTINEL-INLINE-c0ffee42";

pub const CODEX_ID: &str = "codex-ac-1";
pub const CLAUDE_ID: &str = "claude-ac-1";
pub const OPENCODE_ID: &str = "ses_ac1";

pub struct World {
    pub dir: tempfile::TempDir,
    pub codex_root: PathBuf,
    pub claude_root: PathBuf,
    pub db: PathBuf,
    /// Held open like a live OpenCode writer (autocheckpoint off), so rows stay in the WAL.
    pub writer: Connection,
    /// The B7 native population ([`World::native`]); absent in the base three-session world.
    pub grown: Option<Grown>,
    /// Directory `plant_omp` fills; configured only when `omp_on`.
    pub omp_root: PathBuf,
    pub omp_on: bool,
    /// The OMP family of [`World::with_omp`] (the native world keeps it in [`Grown`]).
    pub omp_plant: Option<OmpPlant>,
    /// Directory `plant_pi` fills; configured only when `pi_on`.
    pub pi_root: PathBuf,
    pub pi_on: bool,
    /// The Pi family of [`World::with_pi`] (the native world keeps it in [`Grown`]).
    pub pi_plant: Option<PiPlant>,
    /// Hermes home (its own `state.db` and a `profiles/work` database).
    pub hermes_home: PathBuf,
    /// Held open like a live Hermes writer so committed rows stay in the WAL.
    pub hermes_writers: Vec<Connection>,
    pub hermes_on: bool,
    pub hermes_plant: Option<HermesPlant>,
}

pub fn claude_line(
    kind: &str,
    uuid: &str,
    ts: &str,
    content: Value,
    usage: Option<Value>,
) -> String {
    let mut v = json!({"type": kind, "uuid": uuid, "sessionId": CLAUDE_ID, "cwd": "/work/claude",
        "version": "2.1.311", "timestamp": ts,
        "message": {"id": uuid, "role": kind, "model": "claude-test", "content": content}});
    if let Some(u) = usage {
        v["message"]["usage"] = u;
    }
    v.to_string()
}

impl World {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let codex_root = dir.path().join("codex").join("sessions");
        let claude_root = dir.path().join("claude").join("projects");
        let db = dir.path().join("opencode").join("opencode.db");
        fs::create_dir_all(db.parent().unwrap()).unwrap();

        // Codex: prompt, assistant (with an inline secret and a control sequence),
        // a function call/result pair, usage, completion.
        write_lines(
            &dated(&codex_root, "rollout-ac.jsonl"),
            &[
                cx::session_meta("2026-10-29T23:50:00Z", CODEX_ID, "/work/codex"),
                cx::turn_context("2026-10-29T23:50:01Z", "gpt-test", "/work/codex"),
                cx::user("2026-10-29T23:50:02Z", "Codex prompt: add inventory checks"),
                cx::assistant(
                    "2026-10-29T23:50:03Z",
                    &format!("done \u{1b}]0;pwned\u{7} api_key={INLINE_SECRET}"),
                ),
                cx::item_completed(
                    "2026-10-29T23:50:04Z",
                    cx::command_item("c1", "ls", "inventory.rs\nmain.rs", 0),
                ),
                cx::usage_record("2026-10-29T23:50:06Z", cx::usage(100, 40, 20, 5, 120)),
                cx::task_complete("2026-10-29T23:50:07Z"),
            ],
        );
        // Credential decoys: never read, never discovered.
        fs::write(
            codex_root.join("auth.json"),
            format!("{{\"token\":\"{SENTINEL}\"}}"),
        )
        .unwrap();
        fs::write(
            dated(&codex_root, "auth.json"),
            format!("{{\"token\":\"{SENTINEL}\"}}"),
        )
        .unwrap();

        // Claude: prompt, assistant tool_use with usage, tool_result, final answer.
        write_lines(
            &claude_root.join("proj").join("claude-ac-1.jsonl"),
            &[
                claude_line(
                    "user",
                    "u1",
                    "2026-10-29T23:51:00Z",
                    json!("Claude prompt: refactor"),
                    None,
                ),
                claude_line(
                    "assistant",
                    "a1",
                    "2026-10-29T23:51:01Z",
                    json!([{"type":"tool_use","id":"t1","name":"Read","input":{"path":"x"}}]),
                    Some(
                        json!({"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":2}),
                    ),
                ),
                claude_line(
                    "user",
                    "u2",
                    "2026-10-29T23:51:02Z",
                    json!([{"type":"tool_result","tool_use_id":"t1","content":"file body"}]),
                    None,
                ),
                claude_line(
                    "assistant",
                    "a2",
                    "2026-10-29T23:51:03Z",
                    json!("all done"),
                    None,
                ),
            ],
        );
        fs::write(
            claude_root.join(".credentials.json"),
            format!("{{\"k\":\"{SENTINEL}\"}}"),
        )
        .unwrap();
        fs::write(
            claude_root.join("proj").join(".credentials.json"),
            format!("{{\"k\":\"{SENTINEL}\"}}"),
        )
        .unwrap();

        // OpenCode: v2 database with credential tables seeded by the fixture schema.
        let writer = oc::create_v2(&db);
        let mut s = oc::Session::new(OPENCODE_ID, NOW_MS - 600_000, NOW_MS - 60_000);
        s.title = Some("OpenCode session");
        oc::insert_session(&writer, &s);
        writer
            .execute(
                "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
                 VALUES ('m1', ?1, 'user', 0, ?2, ?2, ?3)",
                rusqlite::params![
                    OPENCODE_ID,
                    NOW_MS - 590_000,
                    json!({"text": "OpenCode prompt"}).to_string()
                ],
            )
            .unwrap();
        writer
            .execute(
                "INSERT INTO credential VALUES ('k2', ?1)",
                rusqlite::params![SENTINEL],
            )
            .unwrap();

        let omp_root = dir.path().join("omp").join("agent").join("sessions");
        let pi_root = dir.path().join("pi").join("agent").join("sessions");
        let hermes_home = dir.path().join("hermes");
        World {
            dir,
            codex_root,
            claude_root,
            db,
            writer,
            grown: None,
            omp_root,
            omp_on: false,
            omp_plant: None,
            pi_root,
            pi_on: false,
            pi_plant: None,
            hermes_home,
            hermes_writers: Vec::new(),
            hermes_on: false,
            hermes_plant: None,
        }
    }

    pub fn config(&self) -> Config {
        let mut cli = CliOverrides {
            codex_sessions: Some(self.codex_root.clone()),
            claude_projects: Some(self.claude_root.clone()),
            opencode_db: Some(self.db.clone()),
            no_processes: true,
            omp_sessions: self.omp_on.then(|| self.omp_root.clone()),
            pi_sessions: self.pi_on.then(|| self.pi_root.clone()),
            ..CliOverrides::default()
        };
        cli.claude_desktop = self.grown.as_ref().map(|g| g.desktop.data.clone());
        let mut config = Config::load_with_env(&cli, &Env::new()).expect("valid world config");
        if self.hermes_on {
            config.harnesses.hermes.root = Some(self.hermes_home.clone());
            config.harnesses.hermes.db = Some(self.hermes_home.join("state.db"));
        }
        config
    }

    pub fn readers(&self) -> Vec<Box<dyn ProviderReader>> {
        let config = self.config();
        let mut readers: Vec<Box<dyn ProviderReader>> = vec![
            Box::new(CodexScanner::new(&config).expect("codex")),
            Box::new(ClaudeScanner::new(&config).expect("claude")),
        ];
        if self.omp_on {
            readers.push(Box::new(OmpScanner::new(&config).expect("omp")));
        }
        if self.pi_on {
            readers.push(Box::new(
                agentscope::providers::pi::PiScanner::new(&config).expect("pi"),
            ));
        }
        if self.hermes_on {
            readers.push(Box::new(
                agentscope::providers::hermes::HermesScanner::new(&config).expect("hermes"),
            ));
        }
        readers.push(Box::new(OpenCodeScanner::new(&config).expect("opencode")));
        readers
    }

    /// The three-provider World plus one OMP family, a fork, a secret-bearing session
    /// and OMP credential/history decoys.
    pub fn with_omp() -> Self {
        let mut world = World::new();
        world.omp_on = true;
        world.omp_plant = Some(plant_omp(
            &world.omp_root,
            OmpTimes {
                parent: NOW_MS - 400_000,
                child: NOW_MS - 390_000,
                nested: NOW_MS - 380_000,
                fork: NOW_MS - 370_000,
                secret: NOW_MS - 360_000,
            },
        ));
        world
    }

    /// The three-provider World plus a Pi family (parent, fork, secret-bearing
    /// session) and Pi credential/extension decoys.
    pub fn with_pi() -> Self {
        let mut world = World::new();
        world.pi_on = true;
        world.pi_plant = Some(plant_pi(
            &world.pi_root,
            PiTimes {
                parent: NOW_MS - 400_000,
                fork: NOW_MS - 390_000,
                secret: NOW_MS - 380_000,
            },
        ));
        world
    }

    /// The three-provider World plus a Hermes home (default and profile
    /// databases) with a compression chain, a delegate, in-place compaction and
    /// credential decoys.
    pub fn with_hermes() -> Self {
        let mut world = World::new();
        world.hermes_on = true;
        let (plant, writers) = plant_hermes(&world.hermes_home);
        world.hermes_plant = Some(plant);
        world.hermes_writers = writers;
        world
    }

    pub fn codex_file(&self) -> PathBuf {
        dated(&self.codex_root, "rollout-ac.jsonl")
    }

    pub fn claude_file(&self) -> PathBuf {
        self.claude_root.join("proj").join("claude-ac-1.jsonl")
    }
}

pub const NOW_MS: i64 = super::NOW;

/// Content snapshot of every regular file under `root` (path relative to `root`).
/// SQLite `-shm`/`-wal` sidecars are reported separately by [`Snapshot::sidecars`].
#[derive(Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    pub sidecars: BTreeMap<PathBuf, Vec<u8>>,
}

fn is_sidecar(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.ends_with("-shm") || name.ends_with("-wal") || name.ends_with("-journal")
}

pub fn snapshot(root: &Path) -> Snapshot {
    fn walk(dir: &Path, root: &Path, out: &mut Snapshot) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path.strip_prefix(root).unwrap().to_path_buf();
                let bytes = fs::read(&path).unwrap_or_default();
                if is_sidecar(&path) {
                    out.sidecars.insert(rel, bytes);
                } else {
                    out.files.insert(rel, bytes);
                }
            }
        }
    }
    let mut out = Snapshot {
        files: BTreeMap::new(),
        sidecars: BTreeMap::new(),
    };
    walk(root, root, &mut out);
    out
}

/// Names (relative) of every directory entry, including directories, for new-file detection.
pub fn all_paths(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
            if path.is_dir() {
                walk(&path, root, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

use std::sync::atomic::{AtomicU64, Ordering};

use agentscope::domain::{
    PageAnchor, PageDirection, PageRequest, RefreshBudget, ScanProgress, SessionDetailPage,
    SessionKey, SessionMetadata, SessionSummary, SourceLocator, Timestamp, TranscriptPage, Warning,
};
use agentscope::providers::{DiscoveryDelta, ProviderError, ReadJobId, ReadProgress, SourceRecord};

/// (relative path, length, mtime nanos, content hash) of every file below `root`.
/// Reading a source must leave this inventory exactly as it was.
pub fn file_inventory(root: &Path) -> Vec<(PathBuf, u64, u128, u64)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(PathBuf, u64, u128, u64)>) {
        for entry in fs::read_dir(dir).expect("read_dir") {
            let path = entry.expect("entry").path();
            let meta = fs::metadata(&path).expect("metadata");
            if meta.is_dir() {
                walk(&path, root, out);
                continue;
            }
            let bytes = fs::read(&path).expect("read file");
            // FNV-1a: cheap and enough to spot a rewritten source.
            let mut hash = 0xcbf2_9ce4_8422_2325u64;
            for byte in &bytes {
                hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
            }
            let mtime = meta
                .modified()
                .expect("mtime")
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("mtime after epoch")
                .as_nanos();
            out.push((
                path.strip_prefix(root).expect("relative").to_path_buf(),
                bytes.len() as u64,
                mtime,
                hash,
            ));
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

pub const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};
pub const PAGE: PageRequest = PageRequest {
    max_events: 1000,
    max_bytes: 10_000_000,
};

static JOBS: AtomicU64 = AtomicU64::new(1);

/// A fresh job ID, unique across the whole test process.
pub fn new_job() -> ReadJobId {
    ReadJobId(JOBS.fetch_add(1, Ordering::Relaxed))
}

/// Steps any fixture here may take before a helper reports that work never finished.
pub const STEPS: usize = 20_000;

/// Everything one discovery pass reported, accumulated across its slices.
#[derive(Default)]
pub struct Found {
    pub sessions: Vec<SessionMetadata>,
    pub removed: Vec<SessionKey>,
    pub invalidated: Vec<SessionKey>,
    pub warnings: Vec<Warning>,
    /// Slices the pass took.
    pub calls: usize,
    pub scan: Option<ScanProgress>,
}

impl Found {
    pub fn absorb(&mut self, delta: DiscoveryDelta) {
        self.sessions.extend(delta.sessions);
        self.removed.extend(delta.removed);
        self.invalidated.extend(delta.invalidated);
        self.warnings.extend(delta.warnings);
        self.scan = Some(delta.scan);
        self.calls += 1;
    }
}

/// Runs discovery of one reader to the end of one pass (a finite number of slices).
pub fn discover_pass<R: ProviderReader + ?Sized>(
    reader: &mut R,
    now: Timestamp,
    budget: RefreshBudget,
) -> Found {
    let mut found = Found::default();
    loop {
        let delta = reader.discover(now, budget).expect("discover");
        let done = delta.scan.done;
        found.absorb(delta);
        if done {
            return found;
        }
        assert!(found.calls < STEPS, "discovery never finished");
    }
}

/// Hydrates one session and also reports how many slices it took.
pub fn hydrate_steps<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    now: Timestamp,
    budget: RefreshBudget,
    max_steps: usize,
) -> (SessionSummary, usize) {
    let job = new_job();
    for step in 1..=max_steps {
        let delta = reader.hydrate(job, key, now, budget).expect("hydrate");
        if delta.done {
            return (delta.summary, step);
        }
    }
    reader.cancel_read(key, job);
    panic!("hydration did not finish in {max_steps} steps");
}

/// Reads one transcript page and also reports how many `Pending` answers preceded it.
#[allow(clippy::too_many_arguments)]
pub fn transcript_steps<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    anchor: Option<PageAnchor>,
    direction: PageDirection,
    request: PageRequest,
    budget: RefreshBudget,
    max_steps: usize,
) -> (TranscriptPage, usize) {
    let job = new_job();
    for pending in 0..max_steps {
        match reader.transcript_page(job, key, anchor.clone(), direction, request, budget) {
            Ok(ReadProgress::Ready(page)) => return (page, pending),
            Ok(ReadProgress::Pending(_)) => {}
            Err(error) => {
                reader.cancel_read(key, job);
                panic!("page: {error}");
            }
        }
    }
    reader.cancel_read(key, job);
    panic!("page did not finish in {max_steps} steps");
}

/// Hydrates one session to completion with `budget`, taking at most `max_steps` slices.
/// A timeout or error cancels the job before it is reported.
pub fn try_hydrate<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    now: Timestamp,
    budget: RefreshBudget,
    max_steps: usize,
) -> Result<SessionSummary, ProviderError> {
    let job = new_job();
    for _ in 0..max_steps {
        match reader.hydrate(job, key, now, budget) {
            Ok(delta) if delta.done => return Ok(delta.summary),
            Ok(_) => {}
            Err(error) => {
                reader.cancel_read(key, job);
                return Err(error);
            }
        }
    }
    reader.cancel_read(key, job);
    panic!("hydration did not finish in {max_steps} steps");
}

pub fn hydrate_budgeted<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    now: Timestamp,
    budget: RefreshBudget,
    max_steps: usize,
) -> SessionSummary {
    try_hydrate(reader, key, now, budget, max_steps).expect("hydrate")
}

pub fn hydrate_complete<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    now: Timestamp,
) -> SessionSummary {
    hydrate_budgeted(reader, key, now, BIG, STEPS)
}

/// Discovers one reader, then hydrates every session it found: the full facts a
/// whole roster pass used to deliver, one session at a time.
pub fn hydrated_all<R: ProviderReader + ?Sized>(
    reader: &mut R,
    now: Timestamp,
    budget: RefreshBudget,
) -> Vec<SessionSummary> {
    let found = discover_pass(reader, now, budget);
    let mut latest: Vec<SessionMetadata> = Vec::new();
    for meta in found.sessions {
        match latest.iter_mut().find(|m| m.key == meta.key) {
            Some(slot) => *slot = meta,
            None => latest.push(meta),
        }
    }
    latest
        .iter()
        .map(|m| hydrate_budgeted(reader, &m.key, now, budget, STEPS))
        .collect()
}

/// What re-reading a source tree reports: the sessions whose sources changed, fully
/// hydrated, plus the removals, invalidations and warnings the passes raised.
pub struct Rescan {
    pub sessions: Vec<SessionSummary>,
    pub removed: Vec<SessionKey>,
    pub invalidated: Vec<SessionKey>,
    pub warnings: Vec<Warning>,
    pub scan: ScanProgress,
}

/// One discovery pass, hydration of every session it reported as new or changed, then a
/// second pass that surfaces what hydration found out (in-place rewrites and rotations
/// are reported by the pass after the one that discovered the change).
pub fn rescan<R: ProviderReader + ?Sized>(
    reader: &mut R,
    now: Timestamp,
    budget: RefreshBudget,
) -> Rescan {
    let found = discover_pass(reader, now, budget);
    let mut keys: Vec<SessionKey> = Vec::new();
    for meta in &found.sessions {
        if !keys.contains(&meta.key) {
            keys.push(meta.key.clone());
        }
    }
    let mut sessions: Vec<SessionSummary> = keys
        .iter()
        .map(|key| hydrate_budgeted(reader, key, now, budget, STEPS))
        .collect();
    let again = discover_pass(reader, now, budget);
    // Sources a second pass reports changed (a duplicate ID appearing or clearing)
    // are hydrated too.
    for meta in &again.sessions {
        if !keys.contains(&meta.key) {
            keys.push(meta.key.clone());
            sessions.push(hydrate_budgeted(reader, &meta.key, now, budget, STEPS));
        }
    }
    let mut removed = found.removed;
    removed.extend(again.removed);
    let mut invalidated = found.invalidated;
    invalidated.extend(again.invalidated);
    let mut warnings = found.warnings;
    warnings.extend(again.warnings);
    Rescan {
        sessions,
        removed,
        invalidated,
        warnings,
        scan: again.scan.expect("a finished pass reports its scan"),
    }
}

/// Drives a resumable read to `Ready`. `Pending` consumes a step; at most `max_steps`
/// steps are taken. Timeouts and errors cancel the job first.
fn drive<R: ProviderReader + ?Sized, T>(
    reader: &mut R,
    key: &SessionKey,
    max_steps: usize,
    mut step: impl FnMut(&mut R, ReadJobId) -> Result<ReadProgress<T>, ProviderError>,
) -> Result<T, ProviderError> {
    let job = new_job();
    for _ in 0..max_steps {
        match step(reader, job) {
            Ok(ReadProgress::Ready(value)) => return Ok(value),
            Ok(ReadProgress::Pending(_)) => {}
            Err(error) => {
                reader.cancel_read(key, job);
                return Err(error);
            }
        }
    }
    reader.cancel_read(key, job);
    panic!("read did not finish in {max_steps} steps");
}

#[allow(clippy::too_many_arguments)]
pub fn try_transcript<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    anchor: Option<PageAnchor>,
    direction: PageDirection,
    request: PageRequest,
    budget: RefreshBudget,
    max_steps: usize,
) -> Result<TranscriptPage, ProviderError> {
    drive(reader, key, max_steps, |r, job| {
        r.transcript_page(job, key, anchor.clone(), direction, request, budget)
    })
}

#[allow(clippy::too_many_arguments)]
pub fn transcript_ready<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    anchor: Option<PageAnchor>,
    direction: PageDirection,
    request: PageRequest,
    budget: RefreshBudget,
    max_steps: usize,
) -> TranscriptPage {
    try_transcript(reader, key, anchor, direction, request, budget, max_steps).expect("page")
}

pub fn try_details<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    anchor: Option<PageAnchor>,
    request: PageRequest,
    budget: RefreshBudget,
    max_steps: usize,
) -> Result<SessionDetailPage, ProviderError> {
    drive(reader, key, max_steps, |r, job| {
        r.details_page(job, key, anchor.clone(), request, budget)
    })
}

pub fn details_ready<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    anchor: Option<PageAnchor>,
    request: PageRequest,
    budget: RefreshBudget,
    max_steps: usize,
) -> SessionDetailPage {
    try_details(reader, key, anchor, request, budget, max_steps).expect("details")
}

pub fn try_count<R: ProviderReader + ?Sized>(
    reader: &mut R,
    key: &SessionKey,
    anchor: &PageAnchor,
    budget: RefreshBudget,
    max_steps: usize,
) -> Result<usize, ProviderError> {
    drive(reader, key, max_steps, |r, job| {
        r.count_events_after(job, key, anchor, budget)
    })
}

pub fn try_source<R: ProviderReader + ?Sized>(
    reader: &mut R,
    locator: &SourceLocator,
    budget: RefreshBudget,
    max_steps: usize,
) -> Result<SourceRecord, ProviderError> {
    drive(reader, &locator.session, max_steps, |r, job| {
        r.source_record(job, locator, budget)
    })
}

/// Everything a provider surfaced for one pass.
#[derive(Default)]
pub struct Surface {
    pub sessions: Vec<SessionSummary>,
    pub warnings: Vec<Warning>,
    pub pages: Vec<TranscriptPage>,
}

/// Discover and hydrate every reader to completion once.
pub fn surface_all(readers: &mut [Box<dyn ProviderReader>], out: &mut Surface) {
    for r in readers.iter_mut() {
        let found = discover_pass(r.as_mut(), super::NOW, BIG);
        out.warnings.extend(found.warnings);
        let mut keys: Vec<SessionKey> = Vec::new();
        for meta in found.sessions {
            if !keys.contains(&meta.key) {
                keys.push(meta.key);
            }
        }
        for key in keys {
            let summary = hydrate_complete(r.as_mut(), &key, super::NOW);
            out.warnings.extend(summary.warnings.clone());
            out.sessions.push(summary);
        }
    }
}

/// Discover and hydrate, then open every session: all pages both directions, every
/// event's source record, every diagnostic's source record, and new-event counting.
pub fn exercise_everything(readers: &mut [Box<dyn ProviderReader>]) -> Surface {
    let mut out = Surface::default();
    surface_all(readers, &mut out);
    let sessions = out.sessions.clone();
    for s in &sessions {
        let r = readers
            .iter_mut()
            .find(|r| r.harness() == s.meta.key.harness)
            .expect("reader");
        for direction in [PageDirection::Forward, PageDirection::Backward] {
            let page = transcript_ready(r.as_mut(), &s.meta.key, None, direction, PAGE, BIG, STEPS);
            for e in &page.events {
                let _ = try_source(r.as_mut(), &e.provenance.locator, BIG, STEPS);
            }
            for d in &page.diagnostics {
                let _ = try_source(r.as_mut(), &d.locator, BIG, STEPS);
            }
            if let Some(first) = page.events.first() {
                let anchor = PageAnchor {
                    timestamp: first.timestamp,
                    source_order: first.source_order,
                    stable_id: first.id.clone(),
                };
                let _ = try_count(r.as_mut(), &s.meta.key, &anchor, BIG, STEPS);
            }
            out.pages.push(page);
        }
    }
    out
}

// ---- the integrated native population (B7) --------------------------------------

use agentscope::domain::{Harness, SourceKey};
use agentscope::providers::opencode::RetryingOpenCode;

use super::claude as fx;
use super::desktop;
use super::facts::file_key;

/// Planted in injected provider *fields* (never in transcript text). No surface may show it.
pub const FIELD_SECRET: &str = "SENTINEL-FIELD-91be5d";
/// Every root the integrated World lists, baseline included.
pub const ROOT_COUNT: usize = 125;
/// Oldest root activity; every other root is one minute newer than the previous.
const BASE_MS: i64 = NOW_MS - 2 * 24 * 3_600_000;
/// Child activity: newer than every root, so a child that leaked into root ordering
/// (or into its parent's activity) would be visible in the exact root pages.
const CHILD_MS: i64 = BASE_MS + 24 * 3_600_000;

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("a valid timestamp")
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn oc_key(db: &Path, id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::OpenCode,
        native_id: id.to_owned(),
        source: SourceKey::DbRow {
            database: db.to_path_buf(),
            table: "session_v2".into(),
            id: id.to_owned(),
        },
    }
}

fn claude_record(session: &str, kind: &str, uuid: &str, ms: i64, content: Value) -> Value {
    json!({"type": kind, "uuid": uuid, "sessionId": session, "cwd": "/work/native",
        "version": "2.1.311", "timestamp": iso(ms),
        "message": {"id": uuid, "role": kind, "content": content}})
}

fn at(mut record: Value, ms: i64) -> Value {
    record["timestamp"] = json!(iso(ms));
    record
}

fn priced(record: Value) -> Value {
    fx::model_of(record, "claude-sonnet-5-5")
}

/// A session and the children it owns.
#[derive(Clone, Debug)]
pub struct Family {
    pub parent: SessionKey,
    pub child: SessionKey,
    pub nested: SessionKey,
}

/// What [`World::native`] planted, with the identities and exact figures the
/// integrated suites assert.
pub struct Grown {
    pub desktop: desktop::Desktop,
    /// Every root with its recorded activity, in creation order.
    pub roots: Vec<(SessionKey, i64)>,
    /// Every child with its immediate parent. None of them is a root.
    pub children: Vec<(SessionKey, SessionKey)>,
    /// Forks: roots that only name their origin.
    pub forks: Vec<(SessionKey, SessionKey)>,
    /// Recorded activity of every child; it rolls up into its group's ordering.
    pub child_activity: Vec<(SessionKey, i64)>,
    /// Exact lifetime tokens each member owns (direct, no family rollup).
    pub direct_tokens: Vec<(SessionKey, u64)>,
    /// Costs a provider reported for a member.
    pub reported_cost: Vec<(SessionKey, f64)>,
    /// Text of the newest transcript entry of a session.
    pub tails: Vec<(SessionKey, String)>,
    pub codex: Family,
    pub claude: Family,
    pub opencode: Family,
    pub desktop_code: SessionKey,
    pub desktop_child: SessionKey,
    pub desktop_released: SessionKey,
    pub cowork: SessionKey,
    pub zstd: SessionKey,
    pub truncated_zstd: SessionKey,
    pub partial_codex: SessionKey,
    pub damaged_claude: SessionKey,
    pub unknown_billing: SessionKey,
    pub secret_claude: SessionKey,
    pub secret_codex: SessionKey,
    pub secret_opencode: SessionKey,
    pub omp: OmpPlant,
    pub pi: PiPlant,
    /// An OpenCode database whose WAL cannot be opened without a sidecar SQLite may not
    /// create (a directory occupies the `-shm` name).
    pub locked_db: PathBuf,
    _locked_writer: Connection,
}

impl Grown {
    /// Root identities in the order the roster lists them: newest group activity first,
    /// where a group's activity is the newest of its own and every descendant's recorded
    /// activity (a fork is its own group).
    pub fn expected_roots(&self) -> Vec<SessionKey> {
        let mut groups: Vec<(SessionKey, i64)> = self.roots.clone();
        for (child, at) in &self.child_activity {
            let mut top = child;
            while let Some((_, parent)) = self.children.iter().find(|(c, _)| c == top) {
                top = parent;
            }
            let group = groups.iter_mut().find(|(k, _)| k == top).expect("a root");
            group.1 = group.1.max(*at);
        }
        groups.sort_by_key(|(_, at)| std::cmp::Reverse(*at));
        groups.into_iter().map(|(k, _)| k).collect()
    }

    pub fn tokens(&self, key: &SessionKey) -> Option<u64> {
        self.direct_tokens
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, n)| *n)
    }

    pub fn is_root(&self, key: &SessionKey) -> bool {
        self.roots.iter().any(|(k, _)| k == key)
    }

    /// Every secret planted anywhere (decoy stores, injected fields): never visible.
    pub fn forbidden_texts(&self) -> Vec<&'static str> {
        vec![
            SENTINEL,
            INLINE_SECRET,
            FIELD_SECRET,
            "SENTINEL-DESKTOP-SECRET",
            "SECRET-SHARE",
            "SECRET-CREDENTIAL",
            "SECRET-ACCOUNT",
            "SECRET-CONTROL",
        ]
    }
}

impl World {
    /// The three-harness World grown to the integrated native population: exactly
    /// [`ROOT_COUNT`] roots plus nested and fork families, Claude Desktop stores and
    /// releases, zstd and damaged rollouts, an unknown billing route, injected secret
    /// fields and a locked WAL database. All synthetic.
    pub fn native() -> Self {
        let mut world = World::new();
        world.omp_on = true;
        world.pi_on = true;
        let grown = grow(&world);
        world.grown = Some(grown);
        world
    }

    pub fn grown(&self) -> &Grown {
        self.grown.as_ref().expect("World::native")
    }

    /// The same World whose OpenCode database is the locked one.
    pub fn locked_config(&self) -> Config {
        let mut config = self.config();
        config.harnesses.opencode.db = Some(self.grown().locked_db.clone());
        config
    }

    /// Readers over [`World::locked_config`]. The OpenCode reader stays registered and
    /// retries like the program's; the others are unaffected.
    pub fn locked_readers(&self) -> Vec<Box<dyn ProviderReader>> {
        let config = self.locked_config();
        let (opencode, error) = RetryingOpenCode::open(&config);
        assert!(error.is_some(), "the locked WAL cannot be opened");
        let mut readers: Vec<Box<dyn ProviderReader>> = vec![
            Box::new(CodexScanner::new(&config).expect("codex")),
            Box::new(ClaudeScanner::new(&config).expect("claude")),
        ];
        if self.omp_on {
            readers.push(Box::new(OmpScanner::new(&config).expect("omp")));
        }
        if self.pi_on {
            readers.push(Box::new(
                agentscope::providers::pi::PiScanner::new(&config).expect("pi"),
            ));
        }
        readers.push(Box::new(opencode));
        readers
    }
}

fn grow(world: &World) -> Grown {
    let codex = &world.codex_root;
    let claude = &world.claude_root;
    let db = &world.db;
    let conn = &world.writer;
    let d = desktop::layout_over(world.dir.path(), claude);

    let mut roots: Vec<(SessionKey, i64)> = vec![
        (
            file_key(Harness::Codex, CODEX_ID, &world.codex_file()),
            NOW_MS - 593_000,
        ),
        (
            file_key(Harness::Claude, CLAUDE_ID, &world.claude_file()),
            NOW_MS - 537_000,
        ),
        (oc_key(db, OPENCODE_ID), NOW_MS - 60_000),
    ];
    let mut children: Vec<(SessionKey, SessionKey)> = Vec::new();
    let mut forks: Vec<(SessionKey, SessionKey)> = Vec::new();
    let mut direct_tokens: Vec<(SessionKey, u64)> = Vec::new();
    let mut reported_cost: Vec<(SessionKey, f64)> = Vec::new();
    let mut tails: Vec<(SessionKey, String)> = Vec::new();
    let mut tick = 0i64;
    let mut next = || {
        tick += 1;
        BASE_MS + tick * 60_000
    };
    let codex_path = |name: &str| dated(codex, &format!("rollout-{name}.jsonl"));

    // ---- Codex: a parent, a spawned child that replays the parent's history, a nested
    // child, and a fork that is its own root.
    let t = next();
    let cdx_parent = file_key(Harness::Codex, "cdx-parent", &codex_path("cdx-parent"));
    write_lines(
        &codex_path("cdx-parent"),
        &[
            cx::session_meta(&iso(t - 3_000), "cdx-parent", "/work/native"),
            cx::turn_context(&iso(t - 2_500), "gpt-5.5", "/work/native"),
            cx::user(&iso(t - 2_000), "Codex family prompt"),
            cx::usage_record_thread(
                &iso(t - 1_000),
                "cdx-parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
            cx::assistant(&iso(t), "codex family tail entry"),
        ],
    );
    roots.push((cdx_parent.clone(), t));
    direct_tokens.push((cdx_parent.clone(), 1000));
    tails.push((cdx_parent.clone(), "codex family tail entry".into()));
    let cdx_kid = file_key(Harness::Codex, "cdx-kid", &codex_path("cdx-kid"));
    write_lines(
        &codex_path("cdx-kid"),
        &[
            cx::child_session_meta(
                &iso(CHILD_MS + 1),
                "cdx-kid",
                "cdx-parent",
                "cdx-parent",
                Some(3),
                "0.159.2",
            ),
            cx::session_meta(&iso(t - 3_000), "cdx-parent", "/work/native"),
            cx::usage_record_thread(
                &iso(t - 1_000),
                "cdx-parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
            cx::turn_context(&iso(CHILD_MS + 2), "gpt-5.5", "/work/native"),
            cx::usage_record_thread(
                &iso(CHILD_MS + 3),
                "cdx-kid",
                cx::usage(700, 0, 450, 0, 1150),
            ),
        ],
    );
    children.push((cdx_kid.clone(), cdx_parent.clone()));
    direct_tokens.push((cdx_kid.clone(), 150));
    let cdx_nested = file_key(Harness::Codex, "cdx-nested", &codex_path("cdx-nested"));
    write_lines(
        &codex_path("cdx-nested"),
        &[
            cx::child_session_meta(
                &iso(CHILD_MS + 4),
                "cdx-nested",
                "cdx-parent",
                "cdx-kid",
                Some(1),
                "0.159.2",
            ),
            cx::turn_context(&iso(CHILD_MS + 5), "gpt-5.5", "/work/native"),
            cx::usage_record_thread(
                &iso(CHILD_MS + 6),
                "cdx-nested",
                cx::usage(40, 0, 10, 0, 50),
            ),
        ],
    );
    children.push((cdx_nested.clone(), cdx_kid.clone()));
    direct_tokens.push((cdx_nested.clone(), 50));
    let t = next();
    let cdx_fork = file_key(Harness::Codex, "cdx-fork", &codex_path("cdx-fork"));
    write_lines(
        &codex_path("cdx-fork"),
        &[
            cx::session_meta_with(
                &iso(t - 1_000),
                json!({"id": "cdx-fork", "forked_from_id": "cdx-parent", "cwd": "/work/native",
                    "originator": "codex_cli_rs", "cli_version": "0.159.2", "source": "cli"}),
            ),
            cx::turn_context(&iso(t - 500), "gpt-5.5", "/work/native"),
            cx::user(&iso(t), "Codex fork prompt"),
            cx::usage_record_thread(&iso(t), "cdx-fork", cx::usage(30, 0, 5, 0, 35)),
        ],
    );
    roots.push((cdx_fork.clone(), t));
    forks.push((cdx_fork.clone(), cdx_parent.clone()));
    direct_tokens.push((cdx_fork, 35));

    // ---- Codex: zstd archive, a truncated archive, a partial trailing record, secrets.
    let rollout = |id: &str, t: i64, prompt: &str| -> Vec<u8> {
        let mut body = [
            cx::session_meta(&iso(t - 2_000), id, "/work/native"),
            cx::user(&iso(t - 1_000), prompt),
            cx::usage_record_thread(&iso(t), id, cx::usage(90, 0, 10, 0, 100)),
        ]
        .join("\n");
        body.push('\n');
        body.into_bytes()
    };
    let t = next();
    let zst_path = codex_path("cdx-zstd");
    fs::create_dir_all(zst_path.parent().unwrap()).unwrap();
    fs::write(
        zst_path.with_extension("jsonl.zst"),
        zstd::encode_all(rollout("cdx-zstd", t, "Archived prompt").as_slice(), 3).unwrap(),
    )
    .unwrap();
    let zstd_key = file_key(Harness::Codex, "cdx-zstd", &zst_path);
    roots.push((zstd_key.clone(), t));
    direct_tokens.push((zstd_key.clone(), 100));
    let t = next();
    let cut_path = codex_path("cdx-cut");
    let mut long = rollout("cdx-cut", t, "Truncated archive");
    for n in 0..700 {
        let filler = format!("history {n} {}", "lorem ipsum ".repeat(34));
        long.extend_from_slice(cx::assistant(&iso(t), &filler).as_bytes());
        long.push(b'\n');
    }
    let mut frame = zstd::encode_all(long.as_slice(), 3).unwrap();
    frame.truncate(frame.len() - 6);
    fs::write(cut_path.with_extension("jsonl.zst"), frame).unwrap();
    let cut_key = file_key(Harness::Codex, "cdx-cut", &cut_path);
    roots.push((cut_key.clone(), t));
    let t = next();
    let partial_path = codex_path("cdx-partial");
    let mut lines = vec![
        cx::session_meta(&iso(t - 2_000), "cdx-partial", "/work/native"),
        cx::user(&iso(t), "Partial trailing record"),
    ];
    lines.push(String::from(
        "{\"timestamp\":\"2026-10-29T23:59:59Z\",\"type\":\"response_item\",\"pay",
    ));
    // `write_lines` terminates every line; the partial record must have no newline.
    write_lines(&partial_path, &lines[..2]);
    {
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&partial_path)
            .unwrap()
            .write_all(lines[2].as_bytes())
            .unwrap();
    }
    let partial_key = file_key(Harness::Codex, "cdx-partial", &partial_path);
    roots.push((partial_key.clone(), t));
    let t = next();
    let secret_codex_path = codex_path("cdx-secret");
    write_lines(
        &secret_codex_path,
        &[
            cx::session_meta_with(
                &iso(t - 2_000),
                json!({"id": "cdx-secret", "cwd": "/work/native", "originator": "codex_cli_rs",
                    "cli_version": "0.159.2", "source": "cli",
                    "api_key": FIELD_SECRET, "authorization": format!("Bearer {FIELD_SECRET}"),
                    "instructions": FIELD_SECRET}),
            ),
            cx::user(&iso(t), &format!("password={INLINE_SECRET} \u{1b}[2J")),
        ],
    );
    let secret_codex = file_key(Harness::Codex, "cdx-secret", &secret_codex_path);
    roots.push((secret_codex.clone(), t));

    // ---- Claude: a main session, a subagent and a nested workflow agent; their
    // direct usage keeps the 4 : 2 : 1 cost ratio.
    let project = claude.join("fam");
    let main_path = project.join("fam-main.jsonl");
    let t = next();
    fx::write(
        &main_path,
        &[
            at(
                fx::user("fam-main", "u0", "Claude family prompt"),
                t - 3_000,
            ),
            at(
                priced(fx::assistant("fam-main", "u1", "m1", (200, 100))),
                t - 2_000,
            ),
            at(
                fx::agent(fx::assistant("fam-main", "u3", "side", (900, 900)), "a"),
                t - 1_500,
            ),
            claude_record(
                "fam-main",
                "user",
                "u2",
                t,
                json!("claude family tail entry"),
            ),
        ],
    );
    let claude_parent = file_key(Harness::Claude, "fam-main", &main_path);
    roots.push((claude_parent.clone(), t));
    direct_tokens.push((claude_parent.clone(), 300));
    tails.push((claude_parent.clone(), "claude family tail entry".into()));
    let child_path = project
        .join("fam-main")
        .join("subagents")
        .join("agent-a.jsonl");
    fx::write(
        &child_path,
        &[
            at(
                fx::user("fam-main", "u0", "Claude family prompt"),
                CHILD_MS + 1,
            ),
            at(
                priced(fx::assistant("fam-main", "u1", "m1", (200, 100))),
                CHILD_MS + 2,
            ),
            at(
                priced(fx::agent(
                    fx::assistant("fam-main", "a1", "c1", (100, 50)),
                    "a",
                )),
                CHILD_MS + 3,
            ),
        ],
    );
    fx::write_sidecar(&child_path, json!({"agentId": "a", "agentType": "Explore"}));
    let claude_child = file_key(Harness::Claude, "a", &child_path);
    children.push((claude_child.clone(), claude_parent.clone()));
    direct_tokens.push((claude_child.clone(), 150));
    let nested_path = project
        .join("fam-main")
        .join("subagents")
        .join("workflows")
        .join("run-1")
        .join("agent-b.jsonl");
    fx::write(
        &nested_path,
        &[at(
            fx::spawned_by(
                priced(fx::agent(
                    fx::assistant("fam-main", "b1", "c2", (50, 25)),
                    "b",
                )),
                "a",
                "toolu_01",
            ),
            CHILD_MS + 4,
        )],
    );
    let claude_nested = file_key(Harness::Claude, "b", &nested_path);
    children.push((claude_nested.clone(), claude_child.clone()));
    direct_tokens.push((claude_nested.clone(), 75));
    // Workflow junk beside the children: never sessions.
    fx::write(
        &project
            .join("fam-main")
            .join("subagents")
            .join("workflows")
            .join("run-1")
            .join("journal.jsonl"),
        &[fx::user("fam-main", "j1", "journal entry")],
    );

    // ---- Claude: damaged transcript (malformed middle line, truncated last record) and
    // injected secret fields.
    let t = next();
    let damaged_path = claude.join("proj").join("claude-damaged.jsonl");
    write_lines(
        &damaged_path,
        &[
            claude_record(
                "claude-damaged",
                "user",
                "d0",
                t - 2_000,
                json!("before the damage"),
            )
            .to_string(),
            String::from("{not json at all"),
            claude_record("claude-damaged", "user", "d1", t, json!("after the damage")).to_string(),
        ],
    );
    {
        use std::io::Write;
        fs::OpenOptions::new()
            .append(true)
            .open(&damaged_path)
            .unwrap()
            .write_all(b"{\"type\":\"assistant\",\"uuid\":\"d2\",\"sessi")
            .unwrap();
    }
    let damaged = file_key(Harness::Claude, "claude-damaged", &damaged_path);
    roots.push((damaged.clone(), t));
    let t = next();
    let secret_claude_path = claude.join("proj").join("claude-secret.jsonl");
    let mut secret_record = claude_record(
        "claude-secret",
        "user",
        "s0",
        t,
        json!(format!("token={INLINE_SECRET} \u{1b}]0;pwned\u{7}")),
    );
    secret_record["apiKey"] = json!(FIELD_SECRET);
    secret_record["authorization"] = json!(format!("Bearer {FIELD_SECRET}"));
    secret_record["env"] = json!({"ANTHROPIC_API_KEY": FIELD_SECRET});
    fx::write(&secret_claude_path, &[secret_record]);
    let secret_claude = file_key(Harness::Claude, "claude-secret", &secret_claude_path);
    roots.push((secret_claude.clone(), t));

    // ---- Claude Desktop: a Code session with a manifest and a subagent, a released
    // session, and a Cowork project root (all shared stores of the one projects root).
    let t = next();
    let code_path = claude.join("proj").join("dsk-code.jsonl");
    fx::write(
        &code_path,
        &[
            at(
                fx::entrypoint(fx::user("dsk-code", "u0", "Refactor"), "claude-desktop"),
                t - 1_000,
            ),
            at(
                fx::entrypoint(
                    fx::assistant("dsk-code", "u1", "m1", (100, 20)),
                    "claude-desktop",
                ),
                t,
            ),
        ],
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_code.json",
        "dsk-code",
        json!({"entrypoint": "claude-desktop"}),
    );
    let desktop_code = file_key(Harness::Claude, "dsk-code", &code_path);
    roots.push((desktop_code.clone(), t));
    direct_tokens.push((desktop_code.clone(), 120));
    let kid_path = claude
        .join("proj")
        .join("dsk-code")
        .join("subagents")
        .join("agent-kid.jsonl");
    fx::write(
        &kid_path,
        &[at(
            fx::agent(fx::assistant("dsk-code", "k1", "mk", (7, 3)), "kid"),
            CHILD_MS + 5,
        )],
    );
    let desktop_child = file_key(Harness::Claude, "kid", &kid_path);
    children.push((desktop_child.clone(), desktop_code.clone()));
    direct_tokens.push((desktop_child.clone(), 10));
    let t = next();
    let gone_path = claude.join("proj").join("dsk-gone.jsonl");
    fx::write(
        &gone_path,
        &[at(fx::user("dsk-gone", "u0", "Released work"), t)],
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_gone.json",
        "dsk-gone",
        json!({"released": true}),
    );
    let desktop_released = file_key(Harness::Claude, "dsk-gone", &gone_path);
    roots.push((desktop_released.clone(), t));
    let t = next();
    let cowork_path = d
        .cowork_projects
        .join("cowork-project")
        .join("cowork-1.jsonl");
    fx::write(
        &cowork_path,
        &[
            at(fx::user("cowork-1", "u0", "Summarize"), t - 1_000),
            at(fx::assistant("cowork-1", "u1", "m1", (3, 1)), t),
        ],
    );
    let cowork = file_key(Harness::Claude, "cowork-1", &cowork_path);
    roots.push((cowork.clone(), t));
    direct_tokens.push((cowork.clone(), 4));

    // ---- OpenCode: parent, child, nested child, a fork root, an unknown billing route,
    // and injected secret fields.
    let go = oc::model_json("gpt-5", "opencode-go", "high");
    let zen = oc::model_json("gpt-5", "opencode-zen", "high");
    let mystery = oc::model_json("gpt-5", "mystery-route", "high");
    let t = next();
    let mut parent = oc::Session::new("ses_fam", t - 600_000, t);
    parent.model = Some(&go);
    parent.tokens = Some([1_000, 400, 0, 0, 0]);
    parent.cost = Some(0.25);
    oc::insert_session(conn, &parent);
    for (n, text) in ["first", "second", "opencode family tail entry"]
        .iter()
        .enumerate()
    {
        conn.execute(
            "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
             VALUES (?1, 'ses_fam', 'user', ?2, ?3, ?3, ?4)",
            rusqlite::params![format!("fam-m{n}"), n as i64, t - 3_000 + n as i64 * 1_000, json!({"text": text}).to_string()],
        )
        .unwrap();
    }
    let oc_parent = oc_key(db, "ses_fam");
    roots.push((oc_parent.clone(), t));
    direct_tokens.push((oc_parent.clone(), 1_400));
    reported_cost.push((oc_parent.clone(), 0.25));
    tails.push((oc_parent.clone(), "opencode family tail entry".into()));
    let mut kid = oc::Session::new("ses_fam_kid", CHILD_MS, CHILD_MS + 10);
    kid.parent_id = Some("ses_fam");
    kid.model = Some(&zen);
    kid.tokens = Some([200, 80, 0, 0, 0]);
    kid.cost = Some(0.05);
    oc::insert_session(conn, &kid);
    let oc_child = oc_key(db, "ses_fam_kid");
    children.push((oc_child.clone(), oc_parent.clone()));
    direct_tokens.push((oc_child.clone(), 280));
    reported_cost.push((oc_child.clone(), 0.05));
    let mut nest = oc::Session::new("ses_fam_nest", CHILD_MS + 1, CHILD_MS + 20);
    nest.parent_id = Some("ses_fam_kid");
    nest.tokens = Some([40, 10, 0, 0, 0]);
    nest.cost = Some(0.01);
    oc::insert_session(conn, &nest);
    let oc_nested = oc_key(db, "ses_fam_nest");
    children.push((oc_nested.clone(), oc_child.clone()));
    direct_tokens.push((oc_nested.clone(), 50));
    reported_cost.push((oc_nested.clone(), 0.01));
    let t = next();
    let mut fork = oc::Session::new("ses_fam_fork", t - 1_000, t);
    fork.fork_id = Some("ses_fam");
    fork.tokens = Some([30, 5, 0, 0, 0]);
    fork.cost = Some(0.002);
    oc::insert_session(conn, &fork);
    let oc_fork = oc_key(db, "ses_fam_fork");
    roots.push((oc_fork.clone(), t));
    forks.push((oc_fork.clone(), oc_parent.clone()));
    direct_tokens.push((oc_fork.clone(), 35));
    reported_cost.push((oc_fork, 0.002));
    let t = next();
    let mut unknown = oc::Session::new("ses_unknown_route", t - 1_000, t);
    unknown.model = Some(&mystery);
    unknown.tokens = Some([500, 100, 0, 0, 0]);
    oc::insert_session(conn, &unknown);
    let unknown_billing = oc_key(db, "ses_unknown_route");
    roots.push((unknown_billing.clone(), t));
    direct_tokens.push((unknown_billing.clone(), 600));
    let t = next();
    let secret_title = format!("ops token={INLINE_SECRET} \u{1b}]0;pwned\u{7}");
    let mut secret = oc::Session::new("ses_secret", t - 1_000, t);
    secret.title = Some(&secret_title);
    oc::insert_session(conn, &secret);
    conn.execute(
        "UPDATE session_v2 SET metadata = ?1 WHERE id = 'ses_secret'",
        rusqlite::params![json!({"apiKey": FIELD_SECRET, "token": FIELD_SECRET}).to_string()],
    )
    .unwrap();
    let secret_opencode = oc_key(db, "ses_secret");
    roots.push((secret_opencode.clone(), t));

    // ---- OMP: a parent with a nested child family (its task result quotes a child cost
    // that must never be added), a fork root, a secret-bearing root and credential
    // decoys beside the sessions root.
    let omp_parent_t = next();
    let omp_fork_t = next();
    let omp_secret_t = next();
    let omp = plant_omp(
        &world.omp_root,
        OmpTimes {
            parent: omp_parent_t,
            child: CHILD_MS + 30,
            nested: CHILD_MS + 31,
            fork: omp_fork_t,
            secret: omp_secret_t,
        },
    );
    roots.push((omp.family.parent.clone(), omp_parent_t));
    roots.push((omp.fork.clone(), omp_fork_t));
    roots.push((omp.secret.clone(), omp_secret_t));
    children.push((omp.family.child.clone(), omp.family.parent.clone()));
    children.push((omp.family.nested.clone(), omp.family.child.clone()));
    forks.push((omp.fork.clone(), omp.family.parent.clone()));
    direct_tokens.extend(omp.tokens.clone());
    tails.push((omp.family.parent.clone(), OMP_TAIL.into()));

    // ---- Pi: a native parent with a recorded request, a fork that copies its history
    // verbatim (only its own call is charged), a secret-bearing root, and decoys that
    // are never read: no extension child exists in the native format.
    let pi_parent_t = next();
    let pi_fork_t = next();
    let pi_secret_t = next();
    let pi = plant_pi(
        &world.pi_root,
        PiTimes {
            parent: pi_parent_t,
            fork: pi_fork_t,
            secret: pi_secret_t,
        },
    );
    roots.push((pi.parent.clone(), pi_parent_t));
    roots.push((pi.fork.clone(), pi_fork_t));
    roots.push((pi.secret.clone(), pi_secret_t));
    forks.push((pi.fork.clone(), pi.parent.clone()));
    direct_tokens.extend(pi.tokens.clone());
    tails.extend(pi.tails.clone());

    // ---- Plain roots up to exactly ROOT_COUNT, spread across the three harnesses.
    let fillers = ROOT_COUNT - roots.len();
    for n in 0..fillers {
        let t = next();
        match n % 3 {
            0 => {
                let id = format!("cdx-f{n:03}");
                let path = codex_path(&id);
                write_lines(
                    &path,
                    &[
                        cx::session_meta(&iso(t - 1_000), &id, "/work/native"),
                        cx::user(&iso(t), &format!("task {id}")),
                    ],
                );
                roots.push((file_key(Harness::Codex, &id, &path), t));
            }
            1 => {
                let id = format!("claude-f{n:03}");
                let path = claude.join("proj").join(format!("{id}.jsonl"));
                fx::write(
                    &path,
                    &[claude_record(
                        &id,
                        "user",
                        &format!("f{n}"),
                        t,
                        json!(format!("task {id}")),
                    )],
                );
                roots.push((file_key(Harness::Claude, &id, &path), t));
            }
            _ => {
                let id = format!("ses_f{n:03}");
                oc::insert_session(conn, &oc::Session::new(&id, t - 1_000, t));
                roots.push((oc_key(db, &id), t));
            }
        }
    }
    assert_eq!(roots.len(), ROOT_COUNT);

    // ---- The locked WAL database: committed rows live in a WAL, and the shared-memory
    // name is occupied by a directory, so SQLite cannot open it without a sidecar it may
    // not create. Nothing may be repaired, deleted or copied by the reader.
    let source = world.dir.path().join("locked-src").join("opencode.db");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    let locked_writer = oc::create_v2(&source);
    oc::insert_session(&locked_writer, &oc::Session::new("ses_locked", 1, 2));
    let locked_db = world.dir.path().join("locked").join("opencode.db");
    fs::create_dir_all(locked_db.parent().unwrap()).unwrap();
    fs::copy(&source, &locked_db).unwrap();
    fs::copy(
        oc::sidecar(&source, "-wal"),
        oc::sidecar(&locked_db, "-wal"),
    )
    .unwrap();
    fs::create_dir(oc::sidecar(&locked_db, "-shm")).unwrap();

    let child_activity = vec![
        (cdx_kid.clone(), CHILD_MS + 3),
        (cdx_nested.clone(), CHILD_MS + 6),
        (claude_child.clone(), CHILD_MS + 3),
        (claude_nested.clone(), CHILD_MS + 4),
        (desktop_child.clone(), CHILD_MS + 5),
        (oc_child.clone(), CHILD_MS + 10),
        (oc_nested.clone(), CHILD_MS + 20),
        (omp.family.child.clone(), CHILD_MS + 30),
        (omp.family.nested.clone(), CHILD_MS + 31),
    ];
    Grown {
        desktop: d,
        roots,
        children,
        child_activity,
        forks,
        direct_tokens,
        reported_cost,
        tails,
        codex: Family {
            parent: cdx_parent,
            child: cdx_kid,
            nested: cdx_nested,
        },
        claude: Family {
            parent: claude_parent,
            child: claude_child,
            nested: claude_nested,
        },
        opencode: Family {
            parent: oc_parent,
            child: oc_child,
            nested: oc_nested,
        },
        desktop_code,
        desktop_child,
        desktop_released,
        cowork,
        zstd: zstd_key,
        truncated_zstd: cut_key,
        partial_codex: partial_key,
        damaged_claude: damaged,
        unknown_billing,
        secret_claude,
        secret_codex,
        secret_opencode,
        omp,
        pi,
        locked_db,
        _locked_writer: locked_writer,
    }
}

// ---- OMP ----------------------------------------------------------------------------

/// Text of the newest transcript entry of the planted OMP family parent.
pub const OMP_TAIL: &str = "omp family tail entry";

/// System-prompt and task text of the planted OMP init record: private content that
/// must never reach metadata, warnings or events (only the raw source view shows it).
pub const OMP_PROMPT_SECRET: &str = "SENTINEL-OMP-PROMPT-3d9c";

/// Recorded activity (epoch ms) of each planted OMP session.
pub struct OmpTimes {
    pub parent: i64,
    pub child: i64,
    pub nested: i64,
    pub fork: i64,
    pub secret: i64,
}

/// What [`plant_omp`] wrote.
pub struct OmpPlant {
    pub family: Family,
    pub fork: SessionKey,
    pub secret: SessionKey,
    /// Exact lifetime tokens each planted session owns (a fork's copies and a parent's
    /// quoted child cost are not theirs).
    pub tokens: Vec<(SessionKey, u64)>,
    /// Credential, history, model and statistics decoys: never read, never listed.
    pub decoys: Vec<PathBuf>,
}

/// Plants an OMP sessions tree under `root` (the `sessions` directory of an agent
/// directory) with credential decoys beside it.
pub fn plant_omp(root: &Path, t: OmpTimes) -> OmpPlant {
    use super::omp as om;

    let bucket = "work";
    let stamped = |entry: Value, ms: i64| om::at(entry, &iso(ms));
    let key = |id: &str, name: &str| {
        file_key(
            Harness::Omp,
            id,
            &root.join(bucket).join(format!("{name}.jsonl")),
        )
    };
    let owned = |tokens: (u64, u64, u64)| om::usage(tokens.0, tokens.1, tokens.2, 0, 99.0);

    // Parent: prompt, one owned call, a task result quoting an absurd child cost, a
    // compaction, and the tail entry.
    let parent = "t1_omp-fam";
    om::write_session(
        root,
        bucket,
        parent,
        &[
            stamped(om::header("omp-fam", None, None), t.parent - 4_000),
            stamped(
                om::user_message("u1", None, "OMP family prompt"),
                t.parent - 3_000,
            ),
            stamped(
                om::call("a1", Some("u1"), Some("r-a1"), owned((600, 300, 100))),
                t.parent - 2_000,
            ),
            stamped(
                om::task_result("t1", Some("a1"), "omp-kid", 9.99),
                t.parent - 1_500,
            ),
            stamped(om::compaction("k1", Some("t1"), "u1"), t.parent - 1_000),
            stamped(om::user_message("u2", Some("k1"), OMP_TAIL), t.parent),
        ],
    );
    om::write_session(
        root,
        bucket,
        &format!("{parent}/omp-kid"),
        &[
            stamped(om::header("omp-kid", None, None), t.child - 200),
            stamped(om::user_message("ku", None, "child task"), t.child - 100),
            stamped(
                om::call("ka", Some("ku"), Some("r-ka"), owned((200, 80, 0))),
                t.child,
            ),
        ],
    );
    om::write_session(
        root,
        bucket,
        &format!("{parent}/omp-kid/omp-nest"),
        &[
            stamped(om::header("omp-nest", None, None), t.nested - 200),
            stamped(om::user_message("nu", None, "nested task"), t.nested - 100),
            stamped(
                om::call("na", Some("nu"), Some("r-na"), owned((40, 10, 0))),
                t.nested,
            ),
        ],
    );
    // Fork: copies the parent's prompt and call (money reset, tokens kept), then adds
    // one call of its own.
    let fork = "t2_omp-fork";
    om::write_session(
        root,
        bucket,
        fork,
        &[
            stamped(
                om::header("omp-fork", Some("omp-fam"), None),
                t.parent - 4_000,
            ),
            stamped(
                om::user_message("u1", None, "OMP family prompt"),
                t.parent - 3_000,
            ),
            stamped(
                om::call(
                    "a1",
                    Some("u1"),
                    Some("r-a1"),
                    om::usage(600, 300, 100, 0, 0.0),
                ),
                t.parent - 2_000,
            ),
            stamped(
                om::call("f1", Some("a1"), Some("r-f1"), om::usage(30, 5, 0, 0, 0.0)),
                t.fork,
            ),
        ],
    );
    // Secret-bearing session: private fields, a system prompt, a credential-shaped
    // prompt and a hostile title.
    let secret = "t3_omp-secret";
    let mut header = om::header("omp-secret", None, None);
    header["apiKey"] = json!(FIELD_SECRET);
    let mut init = om::entry(
        "i1",
        None,
        "session_init",
        json!({"systemPrompt": OMP_PROMPT_SECRET, "task": OMP_PROMPT_SECRET, "authorization": format!("Bearer {FIELD_SECRET}")}),
    );
    init["env"] = json!({"OPENAI_API_KEY": FIELD_SECRET});
    om::write_slotted(
        root,
        bucket,
        secret,
        &om::title_slot(&format!("ops token={INLINE_SECRET} \u{1b}]0;pwned\u{7}")),
        &[
            stamped(header, t.secret - 1_000),
            stamped(init, t.secret - 500),
            stamped(
                om::user_message(
                    "u1",
                    Some("i1"),
                    &format!("password={INLINE_SECRET} \u{1b}[2J"),
                ),
                t.secret,
            ),
        ],
    );

    // Decoys: the agent directory's credential/history/model/statistics stores and
    // files that merely look like sessions in places no session lives.
    let agent = root.parent().expect("an agent directory").to_path_buf();
    let body = format!("{{\"token\":\"{SENTINEL}\"}}");
    let mut decoys = Vec::new();
    for path in [
        agent.join("agent.db"),
        agent.join("history.db"),
        agent.join("models.db"),
        agent.join("stats.db"),
        agent.join("config.yml"),
        agent.join("auth.json"),
        root.join("auth.json"),
        root.join(bucket).join("credentials.json"),
        root.join(bucket)
            .join("blobs")
            .join("sha256-deadbeef.jsonl"),
        root.join(bucket).join("loose-dir").join("stray.jsonl"),
        agent.join("blobs").join("sha256-cafe.jsonl"),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &body).unwrap();
        decoys.push(path);
    }

    let family = Family {
        parent: key("omp-fam", parent),
        child: file_key(
            Harness::Omp,
            "omp-kid",
            &root.join(bucket).join(parent).join("omp-kid.jsonl"),
        ),
        nested: file_key(
            Harness::Omp,
            "omp-nest",
            &root
                .join(bucket)
                .join(parent)
                .join("omp-kid")
                .join("omp-nest.jsonl"),
        ),
    };
    let fork_key = key("omp-fork", fork);
    OmpPlant {
        tokens: vec![
            (family.parent.clone(), 1_000),
            (family.child.clone(), 280),
            (family.nested.clone(), 50),
            (fork_key.clone(), 35),
        ],
        fork: fork_key,
        secret: key("omp-secret", secret),
        decoys,
        family,
    }
}

// ---- Pi -----------------------------------------------------------------------------

/// Text of the newest transcript entry of the planted Pi family parent.
pub const PI_TAIL: &str = "pi family tail entry";

/// Recorded activity (epoch ms) of each planted Pi session.
pub struct PiTimes {
    pub parent: i64,
    pub fork: i64,
    pub secret: i64,
}

/// What [`plant_pi`] wrote.
pub struct PiPlant {
    pub parent: SessionKey,
    pub fork: SessionKey,
    pub secret: SessionKey,
    /// Exact lifetime tokens each planted session owns (a fork's copied history is not
    /// its own spend).
    pub tokens: Vec<(SessionKey, u64)>,
    /// The newest entry text of each session the roster can open.
    pub tails: Vec<(SessionKey, String)>,
    /// Credential/extension decoys: never read, never listed.
    pub decoys: Vec<PathBuf>,
}

/// Plants a native Pi sessions tree under `root` (the `sessions` directory of an agent
/// directory) with credential decoys. Native Pi has no child-session record, so this
/// plants a fork (its own root) and no extension children.
pub fn plant_pi(root: &Path, t: PiTimes) -> PiPlant {
    use super::pi as px;

    let bucket = "work";
    let stamped = |entry: Value, ms: i64| px::at(entry, &iso(ms));
    let key = |id: &str, name: &str| {
        file_key(
            Harness::Pi,
            id,
            &root.join(bucket).join(format!("{name}.jsonl")),
        )
    };

    let prompt = "Pi family prompt";
    let copied = [
        stamped(
            px::message("pu0", None, "user", prompt, None),
            t.parent - 3_000,
        ),
        stamped(
            px::model_change("pm1", "pu0", "anthropic", "claude-test"),
            t.parent - 2_500,
        ),
        stamped(
            px::message(
                "pa1",
                Some("pm1"),
                "assistant",
                PI_TAIL,
                Some(px::usage(600, 0, 400, 0)),
            ),
            t.parent,
        ),
    ];
    let parent_file = px::write_session(root, bucket, "pi-parent", &{
        let mut entries = vec![px::header("pi-parent", None)];
        entries.extend(copied.iter().cloned());
        entries
    });

    // A producer fork: a new header naming the source file and every earlier entry
    // copied verbatim, then one new request of its own.
    let fork = "pi-fork";
    let mut fork_entries = vec![px::header("pi-fork", Some(&parent_file.to_string_lossy()))];
    fork_entries.extend(copied.iter().cloned());
    fork_entries.push(stamped(
        px::message(
            "f1",
            Some("pa1"),
            "assistant",
            "pi fork answer",
            Some(px::usage(10, 5, 0, 0)),
        ),
        t.fork,
    ));
    px::write_session(root, bucket, fork, &fork_entries);

    // A credential-shaped prompt in a discoverable session: it must never surface
    // outside the explicit raw source view.
    let secret = "pi-secret";
    px::write_session(
        root,
        bucket,
        secret,
        &[
            stamped(px::header("pi-secret", None), t.secret - 2_000),
            stamped(
                px::message(
                    "su0",
                    None,
                    "user",
                    &format!("Pi private password={INLINE_SECRET} \u{1b}[2J"),
                    None,
                ),
                t.secret - 1_000,
            ),
            stamped(
                px::session_info("si1", "su0", &format!("ops token={INLINE_SECRET}")),
                t.secret - 900,
            ),
            stamped(
                px::message(
                    "sa1",
                    Some("si1"),
                    "assistant",
                    "acknowledged",
                    Some(px::usage(1, 1, 0, 0)),
                ),
                t.secret,
            ),
        ],
    );

    // Decoys in places no native session lives: beside the root, inside a bucket as a
    // non-JSONL file, and one level below a bucket (never entered). A backup copy of a
    // real session name is not a session either.
    let body = format!("{{\"token\":\"{SENTINEL}\"}}");
    let mut decoys = Vec::new();
    for path in [
        root.join("auth.json"),
        root.join("notes.jsonl"),
        root.join(bucket).join("credentials.json"),
        root.join(bucket).join("pi-secret.jsonl.bak"),
        root.join(bucket).join("blobs").join("sha256-pi.jsonl"),
        root.join(bucket).join("ext").join("child.jsonl"),
    ] {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &body).unwrap();
        decoys.push(path);
    }

    let parent_key = key("pi-parent", "pi-parent");
    let fork_key = key("pi-fork", fork);
    let secret_key = key("pi-secret", secret);
    PiPlant {
        tokens: vec![
            (parent_key.clone(), 1_000),
            (fork_key.clone(), 15),
            (secret_key.clone(), 2),
        ],
        tails: vec![(parent_key.clone(), PI_TAIL.into())],
        parent: parent_key,
        fork: fork_key,
        secret: secret_key,
        decoys,
    }
}

// ---- Hermes ------------------------------------------------------------------------

use super::hermes as hx;

/// Identities and text of the planted Hermes home.
pub struct HermesPlant {
    /// Logical parent chain (`h-parent` → `h-cont`).
    pub parent: SessionKey,
    pub continuation: SessionKey,
    pub child: SessionKey,
    pub profile: SessionKey,
    pub profile_db: PathBuf,
    pub tail: String,
    pub summary: String,
}

/// Creates a Hermes home with a default `state.db` and one profile database,
/// plus credential decoys beside them. The writers stay open so rows are in WAL.
pub fn plant_hermes(home: &Path) -> (HermesPlant, Vec<Connection>) {
    let db = home.join("state.db");
    let profile_db = home.join("profiles").join("work").join("state.db");
    let root = hx::create_db(&db, true);
    let profile = hx::create_db(&profile_db, true);

    hx::insert_session(
        &root,
        "h-parent",
        None,
        json!({}),
        (NOW_MS - 500_000) as f64 / 1000.0,
    );
    hx::set_title(&root, "h-parent", "Hermes parent");
    hx::insert_session(
        &root,
        "h-cont",
        Some("h-parent"),
        json!({}),
        (NOW_MS - 490_000) as f64 / 1000.0,
    );
    hx::set_title(&root, "h-cont", "Hermes continued");
    hx::set_ended(
        &root,
        "h-parent",
        (NOW_MS - 480_000) as f64 / 1000.0,
        "compression",
    );
    // In-place compaction inside the continuation: an archived turn, a summary
    // carrier and fresh work.
    let archived = hx::insert_message(
        &root,
        "h-cont",
        "user",
        "hermes archived turn",
        (NOW_MS - 480_000) as f64 / 1000.0,
    );
    hx::set_message_display(&root, archived, 1, b"hermes-archived", "hermes-archived");
    hx::set_message_state(&root, archived, 0, 1);
    let carrier = hx::insert_message(
        &root,
        "h-cont",
        "user",
        "hermes compacted summary",
        (NOW_MS - 470_000) as f64 / 1000.0,
    );
    hx::set_message_summary(&root, carrier, 1);
    hx::set_message_display(&root, carrier, 0, b"hermes-summary", "hermes-summary");
    let tail = hx::insert_message(
        &root,
        "h-cont",
        "assistant",
        "hermes continuation tail",
        (NOW_MS - 460_000) as f64 / 1000.0,
    );
    hx::set_message_display(&root, tail, 2, b"hermes-tail", "hermes-tail");

    hx::insert_session(
        &root,
        "h-child",
        Some("h-cont"),
        json!({"_delegate_from": "h-cont"}),
        (NOW_MS - 450_000) as f64 / 1000.0,
    );
    hx::set_title(&root, "h-child", "Hermes delegate");
    hx::insert_message(
        &root,
        "h-child",
        "assistant",
        "hermes delegate answer",
        (NOW_MS - 440_000) as f64 / 1000.0,
    );
    for (session, model) in [
        ("h-parent", "test-model"),
        ("h-cont", "test-model"),
        ("h-child", "test-model"),
    ] {
        hx::insert_usage(
            &root,
            session,
            model,
            "test-provider",
            "",
            [100, 20, 0, 0, 0],
            0.01,
            "estimated",
        );
        hx::set_session_usage(
            &root,
            session,
            [100, 20, 0, 0, 0],
            1,
            0.01,
            0.0,
            Some("estimated"),
        );
    }

    hx::insert_session(
        &profile,
        "h-profile",
        None,
        json!({}),
        (NOW_MS - 430_000) as f64 / 1000.0,
    );
    hx::set_title(&profile, "h-profile", "Hermes profile session");
    hx::insert_message(
        &profile,
        "h-profile",
        "user",
        &format!("hermes profile prompt api_key={INLINE_SECRET}"),
        (NOW_MS - 430_000) as f64 / 1000.0,
    );
    hx::insert_usage(
        &profile,
        "h-profile",
        "test-model",
        "test-provider",
        "",
        [50, 10, 0, 0, 0],
        0.005,
        "estimated",
    );
    hx::set_session_usage(
        &profile,
        "h-profile",
        [50, 10, 0, 0, 0],
        1,
        0.005,
        0.0,
        Some("estimated"),
    );

    // Credential decoys: never opened, never shown.
    fs::write(
        home.join("auth.json"),
        format!("{{\"api_key\":\"{SENTINEL}\"}}"),
    )
    .unwrap();
    fs::write(
        home.join("config.toml"),
        format!("api_key = \"{SENTINEL}\"\n"),
    )
    .unwrap();
    fs::write(
        home.join("profiles").join("work").join(".env"),
        format!("HERMES_API_KEY={SENTINEL}\n"),
    )
    .unwrap();

    let key = |db: &Path, id: &str| SessionKey {
        harness: Harness::Hermes,
        native_id: id.to_owned(),
        source: SourceKey::DbRow {
            database: db.to_path_buf(),
            table: "sessions".into(),
            id: id.to_owned(),
        },
    };
    let plant = HermesPlant {
        parent: key(&db, "h-parent"),
        continuation: key(&db, "h-cont"),
        child: key(&db, "h-child"),
        profile: key(&profile_db, "h-profile"),
        profile_db,
        tail: "hermes continuation tail".into(),
        summary: "hermes compacted summary".into(),
    };
    (plant, vec![root, profile])
}
