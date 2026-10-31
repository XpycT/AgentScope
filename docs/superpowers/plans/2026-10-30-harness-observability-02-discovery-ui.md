# Metadata-First Discovery, Native Children, Desktop and TUI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver real lazy 50-group discovery, native Codex/Claude/OpenCode child/desktop coverage, and the approved compact roster/preview/details/navigation through the existing application.

**Architecture:** Replace the eager reader refresh contract atomically across the three existing adapters, worker and tests. A bounded metadata index freezes ordered root-group generations; the worker hydrates requested facts/pages in cancellable slices. App owns selection/preview/scroll state, and Ratatui renders immutable borrowed facts with no provider I/O.

**Tech Stack:** Rust stable/edition 2024, existing worker channels, Ratatui 0.30/Crossterm 0.29, rusqlite, serde_json, plan01's streaming zstd reader.

**Spec:** [Approved design](../specs/2026-10-30-harness-observability-design.md), especially §§4–5 and §§9.1–9.3. Depends on all of [plan01](2026-10-30-harness-observability-01-foundations.md); consumed by plans03–05 and the [master](2026-10-30-harness-observability.md).

## Global Constraints

- One background worker; no UI-thread provider reads, no runtime network/provider writes/orchestration/schema migration or persistent index. SQLite transient WAL/SHM behavior follows ADR-0002.
- Root and child metadata batches 50. Limits: 100,000 source records/64 MiB metadata, 2,000 hydrated summaries, 256 records/8 MiB/100 ms work slices, 1 MiB records, 500-event/256 KiB pages, 20,000 loaded events, 16 pending loads, 100,000-entry/8 MiB exact ledger.
- Metadata probe: 4 KiB prefix/32 KiB uncompressed tail. Expanded compressed bytes count work; compressed ranking stays approximate until requested indexing.
- Minimum 60×20. At ≥150 columns reserve 45 for preview; below it Tab swaps roster/preview. Verify 60×20, 90×25, 160×40 and resizing.
- Unknown/Loading/Partial must not turn into zero. Actual model/effort/origin/lineage is evidence-bound; child cost and inherited history may not be counted twice.
- No implementation while planning, no commits without authorization. Execution uses focused red/green and actual synthetic app smoke; full fmt/check/clippy/test gates only at integrated release.

## Review Focus

1. A new source, deletion, or late-discovered parent arrives between Load more clicks: frozen generation cannot duplicate/skip/reorder consumed groups (B1/B5).
2. Claude's child sessionId equals its parent, with nested workflow junk beside it: distinct child identity and allowlisted discovery, no phantom journal sessions (B2).
3. A Codex child inherits a parent's cumulative counter then archives to zstd: bill only owned child records while preserving logical selection/source generation (B3).
4. Two preview selections race while an old source is indexing or SQLite is busy: stale replies cannot replace the newest preview or freeze input (B1/B5).
5. Narrow resizing or mouse capture changes terminal behavior: required facts/Load more remain reachable and terminal modes restore on quit/error (B6).

## File structure and ownership

| File | Responsibility |
|---|---|
| Modify `src/providers/mod.rs` | Object-safe bounded metadata/hydration/read-progress contract |
| Modify `src/discovery.rs` | Resumable allowlisted source enumeration; no all-transcript scan |
| Create `src/roster.rs` | Metadata/source limits, group projection and frozen generations, no I/O |
| Modify native `src/providers/{claude,codex,opencode}.rs` | Native cheap discovery, request ownership, child/desktop parsing, paged facts |
| Create `src/providers/claude/{metadata,children}.rs`, `src/providers/codex/metadata.rs` only as needed | Focused new path/header/link logic; existing parsing code is split only at these responsibilities |
| Modify `src/worker.rs` | Reader scheduling, fair/cancellable progress, bounded cache, frozen roster publication |
| Modify `src/main.rs` | All reader construction on worker via lazy factory; help/root/mouse flags |
| Modify `src/app.rs`, create `src/app/{roster,preview}.rs` | Pure grouped selection, loaded-only query, preview/details and cancellation state |
| Modify `src/app/{input,overlay,transcript}.rs` | Key/focus/overlay/newest-page transitions; preserve source/search semantics |
| Modify `src/ui/{mod,roster,session,details,inspector,panels,widgets}.rs`, create `src/ui/preview.rs` | Two-line aligned rows, preview/details shared rendering, hit-testing, terminal lifecycle |
| Modify `tests/support/{world,codex,claude,opencode}.rs`; create `tests/support/desktop.rs` | Synthetic stores, native child/fork cases, desktop metadata decoys |
| Create `tests/{roster_paging,native_children,desktop_sessions}.rs`; modify provider/worker/UI/perf/read-only/privacy/acceptance tests | Behavioral integration rather than wiring assertions |

## Task B1: Atomic bounded-reader, metadata-index, and worker cutover

This is one integration boundary: migrate **all three existing readers, worker, App snapshot ingestion and real-reader test helpers together**. Do not merge half a trait migration or retain an eager-refresh shim. The old roster can render the new loaded summaries/metadata while B5/B6 add the richer visual surface.

**Files:** provider contract, discovery, native readers, new `src/roster.rs`, `src/worker.rs`, `src/main.rs`, `src/app.rs`/new roster module, `src/ui/mod.rs`, `tests/support/world.rs`, all reader tests using old refresh/page APIs, `tests/{roster_paging,worker,perf_budget,live_refresh,app_state}.rs`.

**Consumes:** plan01 `SessionMetadata`, `SessionSummary`, `SessionDetailPage`, source readers, configuration and canonical keys.

**Produces — shared contract for every harness:**

```rust
pub struct WorkProgress { pub records: usize, pub expanded_bytes: usize }
pub struct ReadJobId(pub u64); // worker-assigned, unique per in-flight request
pub enum ReadProgress<T> { Pending(WorkProgress), Ready(T) }
pub struct DiscoveryDelta {
    pub sessions: Vec<SessionMetadata>, pub removed: Vec<SessionKey>,
    pub invalidated: Vec<SessionKey>, pub warnings: Vec<Warning>, pub scan: ScanProgress,
}
pub struct HydrationDelta { pub summary: SessionSummary, pub done: bool }
```

`ScanProgress` retains existing fields renamed only for identity: `{ harness, scanned, total, done }`. `RefreshBudget` remains `{ max_records, max_bytes, max_millis }`. `WarningKind` retains `Io/Parse/Unsupported/Unavailable/Process/Pricing`.

`ProviderReader: Send` retains the same name and error/source-record types, replacing old `refresh`/`ProviderDelta` without aliases:

```rust
fn harness(&self) -> Harness;
fn discover(&mut self, now: Timestamp, budget: RefreshBudget) -> Result<DiscoveryDelta, ProviderError>;
fn hydrate(&mut self, job: ReadJobId, key: &SessionKey, now: Timestamp, budget: RefreshBudget) -> Result<HydrationDelta, ProviderError>;
fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId);
fn details_page(&mut self, job: ReadJobId, key: &SessionKey, anchor: Option<PageAnchor>, request: PageRequest, budget: RefreshBudget)
    -> Result<ReadProgress<SessionDetailPage>, ProviderError>;
fn transcript_page(&mut self, job: ReadJobId, key: &SessionKey, anchor: Option<PageAnchor>, direction: PageDirection,
    request: PageRequest, budget: RefreshBudget) -> Result<ReadProgress<TranscriptPage>, ProviderError>;
fn count_events_after(&mut self, job: ReadJobId, key: &SessionKey, anchor: &PageAnchor, budget: RefreshBudget)
    -> Result<ReadProgress<usize>, ProviderError>;
fn source_record(&mut self, job: ReadJobId, locator: &SourceLocator, budget: RefreshBudget)
    -> Result<ReadProgress<SourceRecord>, ProviderError>;
```

Pending is normal progress, not an Unavailable warning/retry error. Each adapter retains bounded continuation/decoder/index state keyed by `(SessionKey, ReadJobId)`, so cancelling one purpose on a key cannot release another purpose's work; `cancel_read` drops only that job's abandoned state after the last consumer disappears. Healthy completed summaries stay cached. Existing `SourceReader::record` progress maps into this enum without an alternate worker.

**Index:** `MetadataLimits { max_sources: usize, max_bytes: usize }`; `DiscoveryLimit { sources: usize, bytes: usize }`; `RosterIndex::new(limits: MetadataLimits, max_cached_sessions: usize) -> Self`; `apply_discovery(&mut self, delta: DiscoveryDelta)`; `freeze(&mut self, generation: u64)`; `load_more(&mut self, generation: u64) -> bool`; `update_summary(&mut self, summary: SessionSummary)`; `children(&self, parent: &SessionKey, after: Option<&SessionKey>, limit: usize) -> ChildPage`; `snapshot(&self) -> RosterSnapshot`. `ChildPage { metadata: Vec<Arc<SessionMetadata>>, next: Option<SessionKey> }`.

`RosterSnapshot { generation: u64, roots: Arc<[SessionKey]>, metadata: Vec<Arc<SessionMetadata>>, summaries: Vec<Arc<SessionSummary>>, scan: Vec<ScanProgress>, has_more: bool, limited: Option<DiscoveryLimit>, refresh_available: bool, provisional: bool }`. `roots` contains loaded root keys only; all lightweight candidate metadata stays in the worker index, not copied into UI every tick. Immutable Arc facts are intentionally shared across the worker/UI boundary; no Arc<Mutex<...>>. Rebuild only changed facts and selection/view indices; do not clone source-sized strings or format Debug keys in sort hot paths.

**Messages:**

- `WorkerRequest` retains Refresh/Select/LoadPage/CountAfter/LoadSource/Shutdown; adds `LoadRosterMore { generation }`, `LoadChildren { request_id, parent, cursor: Option<SessionKey> }`, `HydratePreview { request_id, key }`, `LoadDetails { request_id, key, anchor: Option<PageAnchor> }`, `CancelLoad { request_id }`.
- `WorkerEvent::Roster { snapshot: RosterSnapshot, warnings: Vec<Warning> }` replaces old full summary roster. Add `Preview { request_id, key, summary: Arc<SessionSummary>, done }`, `Details { request_id, key, page }`, `LoadProgress { request_id, records, expanded_bytes }`, `Children { request_id, parent, metadata: Vec<Arc<SessionMetadata>>, next }`; preserve transcript/source/process/failure/invalidation events.
- Worker privately maps each request to its key/generation/purpose. Cancelled/obsolete work cannot publish as current. Pending work is requeued after a slice with selection/shutdown checked first. Bounded automatic initial-50 hydration is a work iterator, **not** 50 entries pushed into a 16-entry pending queue that drops requests.
- `App::apply_roster_snapshot(&mut self, snapshot: RosterSnapshot, warnings: Vec<Warning>) -> bool` ignores stale generations and retains selection. Metadata-only rows render unknown/loading facts until a summary exists; they are not fake reported summaries.

**Test helper migration:** replace `refresh_all` in `tests/support/world.rs` with `discover_all(readers: &mut [Box<dyn ProviderReader>], now: Timestamp) -> Vec<SessionMetadata>` and `hydrate_complete(reader: &mut dyn ProviderReader, key: &SessionKey, now: Timestamp) -> SessionSummary`. Add `transcript_ready(...) -> TranscriptPage`, `details_ready(...) -> SessionDetailPage`, and `source_ready(...) -> SourceRecord` with the exact reader arguments plus a finite `max_steps: usize`. Each helper allocates a fresh `ReadJobId`, consumes Pending up to the fixture-specific step bound, and calls `cancel_read` on timeout/error before surfacing it; never spin indefinitely or absorb real errors. Update every old page/read/refresh caller; no old-name test shims.

- [ ] **Step 1: Add root-paging and metadata-work regressions.** In `tests/roster_paging.rs`, build 125 roots across the three native harnesses plus 80 children under one parent. Mark a parent's child as newest. After freeze/load, assert exact root IDs for pages 50/50/25, no duplicate IDs across accumulated pages, child expansion leaves the root count unchanged, a fork occupies a root slot, and unknown activity sorts after timestamped roots. Reusing a stale generation's Load more must change nothing.
  ```rust
  assert_eq!(first.roots.as_ref(), expected_newest_50.as_slice());
  assert_eq!(after_more.roots.len(), 100);
  assert_eq!(after_final.roots.len(), 125);
  assert!(!after_final.has_more);
  assert_eq!(expanded.roots.as_ref(), first.roots.as_ref());
  ```
- [ ] **Step 2: Add bounded discovery/read-progress regressions and run red.** Extend `tests/perf_budget.rs` with `metadata_discovery_does_not_parse_unloaded_bodies`: synthetic large sources have valid headers/tails and a middle sentinel record whose hydration would change an exact request total. Assert discovery does not observe that middle usage, consumes only the declared probes/work slices, and metadata ceilings produce Limited rather than EOF. Test zstd indexing returns Pending before completion and allows another selected small source to finish. Run `cargo test --test roster_paging --test perf_budget --test worker`.
- [ ] **Step 3: Implement object-safe reader and native discovery split.** Native constructors become cheap and all filesystem/SQLite opening occurs in the worker factory. Preserve existing parsers/indexes for lazy hydration; replace discovery's collect-and-truncate whole-file scan with resumable allowlisted traversal. For database metadata, query only known columns/JSON keys, cap rows/text and use a progress handler/deadline plus busy timeout no greater than the work slice; an aggregate query cannot block the worker for the old 2,000ms refresh timeout. Source proofs may be approximate; no metadata probe may quietly trigger a whole transcript scan.
- [ ] **Step 4: Implement index/frozen generation and worker integration.** Store native explicit edges; merge continuation members from adapters, isolate orphans/cycles. Max group activity includes known descendants. Freeze stable source-key ties and root ordering; hydration/new sources/deletions only set refresh-available or mark an existing row Unavailable until explicit refresh. Root capacity grows in 50s, child pages in 50s. Keep detailed summary LRU separate from metadata history; cache eviction never ends pagination. Root limits/incomplete scan counts are visible. Move initial `build_readers` work behind `WorkerHandle::spawn_lazy`, reusing the existing note channel/respawn pattern rather than only making respawn lazy.
- [ ] **Step 5: Run focused green and reader regressions.** `cargo test --test roster_paging --test worker --test perf_budget --test live_refresh --test app_state` plus native reader suites migrated to the bounded helpers. Confirm exact latest roots when recorded metadata is complete and explicitly approximate ordering for fallback cases. Confirm no queued load vanishes from the initial 50 and cancellation releases abandoned ledger state.
- [ ] **Step 6: Smoke a mixed native store through real startup/events/rendering.** Use 125 synthetic roots, one huge plain and one compressed source, and an open WAL fixture. Observe first-page metadata before hydration, progress without blocked navigation, one Load more, a child expansion, stale selection cancellation, cache eviction/rehydration and a discovery limit warning. Update the architecture's former full-roster-scan limitation to the exercised metadata contract, retaining actual benchmark/OS limits.

## Task B2: Claude native children and local Desktop Code/Cowork sources

**Files:** `src/providers/claude.rs`, focused `claude/metadata.rs`/`children.rs`, `src/discovery.rs`, configuration root resolution, `tests/support/{claude,desktop}.rs`, `tests/{native_children,desktop_sessions,claude_usage,claude_transcript_live,privacy,read_only}.rs`.

**Consumes:** B1 bounded reader/index and plan01 keyed request accounting.

**Produces:** provider-private `ClaudeChildId { parent_session_id: String, agent_id: String }`; `fn probe_claude_metadata(path: &Path, root: &Path, limits: RefreshBudget) -> Result<SessionMetadata, ProviderError>` resumable through discovery-owned state; `fn child_parent(path: &Path, meta: Option<&serde_json::Value>) -> ClaudeParentEvidence`; `fn desktop_origin(entrypoint: Option<&str>) -> OriginSurface`. `ClaudeParentEvidence` carries confirmed session/agent/tool-use links plus optional depth; unknown fields stay optional. These are private parsing helpers, not a new registry/plugin trait.

- [ ] **Step 1: Add identity/ownership and nested-directory regressions.** Build main `<id>.jsonl`, `subagents/agent-a.jsonl`, a child `agent-b` linked by `parentAgentId`, and a workflow `journal.jsonl` decoy. Child records carry `sessionId=main` plus distinct `agentId`. Assert distinct child native keys, correct nested parent, no journal session, parent direct usage excludes each child's own usage, and a shared fork `message.id` enters the family cost only once. `parentUuid` alone may not resolve a session edge.
  ```rust
  assert_ne!(main.meta.key, child.meta.key);
  assert_eq!(child.meta.key.native_id, "a");
  assert_eq!(nested.meta.lineage.parent, Some(SessionLink::Resolved(child.meta.key.clone())));
  assert_eq!(parent_direct_tokens, expected_parent_owned_tokens);
  assert_eq!(family_unique_cost, expected_unique_cost);
  ```
- [ ] **Step 2: Add Desktop root/manifest/release/privacy regressions and run red.** Synthetic Desktop Code manifests point to an existing `cliSessionId` transcript, two resumed records have CLI/Desktop entrypoints, a local Cowork Claude-format project root exists, and neighbouring auth/cache/audit/VM files contain sentinels. Assert only allowlisted native transcripts appear once, origins can be mixed, known release/delete markers default hidden but are inspectable with include-released, unknown marker shapes do not guess release/lifecycle, and no credential/VM/cloud cache content is read. Run `cargo test --test native_children claude`, `cargo test --test desktop_sessions claude`, and `cargo test --test privacy claude_desktop`.
- [ ] **Step 3: Implement path-specific metadata and native parsing.** Top-level source pattern is project-root session JSONL; children are `subagents/**/agent-*.jsonl` with confirmed optional sidecars/workflow nesting. Use parent session directory and exact metadata IDs, not arbitrary cwd/time. Preserve raw child request provenance while distinguishing inherited shared history. Sidechain lifecycle cannot move parent lifecycle. Extract only allowlisted Desktop manifest/entrypoint/release keys, never whole raw metadata or cache content.
- [ ] **Step 4: Run green and actual desktop-layout smoke.** Run the owning Claude reader/usage/live tests plus `native_children`, `desktop_sessions`, `privacy`, `read_only`. Launch synthetic Desktop directory structures through resolved roots and the real app, expand the family, select/open each child, inspect API costs/effort/origin and a released session after explicit filter change. No claim of running the proprietary desktop app follows from a fixture-path smoke. Update desktop/native-child docs with confirmed local-source boundaries after observation.

## Task B3: Codex owned child usage, Desktop identity and archive discovery

**Files:** `src/providers/codex.rs`, new `codex/metadata.rs`, `src/discovery.rs`, config root resolution, `tests/support/{codex,desktop}.rs`, `tests/{native_children,desktop_sessions,codex_roster,codex_transcript,compressed_source,read_only}.rs`.

**Consumes:** B1 metadata/progress, A4 canonical archive identities and source locators.

**Produces:** `CodexOwnership { first_owned_ordinal: Option<u64>, inherited_baseline: Option<TokenCategories>, complete: bool }`; `fn codex_parent(meta: &serde_json::Value) -> CodexParentEvidence`; `fn classify_codex_origin(source: &serde_json::Value, originator: Option<&str>) -> OriginSurface`; `fn apply_owned_usage(ownership: &mut CodexOwnership, ordinal: u64, usage: &serde_json::Value) -> OwnedUsage`. `OwnedUsage` is `Delta(ProviderUsageDelta)`, `Snapshot(TokenCategories)`, `Inherited`, or `Unknown`; define it in the provider module, not the shared price engine. `ProviderUsageDelta { categories: TokenCategories, request: Option<RequestMetadata> }` carries provider-private facts only; the source owner that knows the session/generation/source locator constructs the normalized `UsageObservation` with truthful `Provenance`. The helper never invents provenance from an ordinal alone.

- [ ] **Step 1: Add inherited-baseline regressions.** Create a parent with 1,000 cumulative tokens and a child replaying that prefix whose final counter is 1,150. With a valid child-owned boundary, direct child is 150, family unique total=1,150, not 2,150. A newer child-local-zero fixture must also produce 150 without subtracting the parent twice. With no trustworthy ownership boundary, child total/cost is partial/Unknown, not confidently 1,150. Include nested review/thread-spawn variants and distinguish immediate-parent from root-thread IDs.
  ```rust
  assert_eq!(owned_child.usage.as_ref().unwrap().total, Some(150));
  assert_eq!(family_total, Some(1150));
  assert!(!ambiguous_child.usage.as_ref().unwrap().coverage.complete);
  ```
- [ ] **Step 2: Add Desktop/archive/identity regressions and run red.** Cover native CLI, Desktop plus source `vscode`, IDE plus the same source, ambiguous bundled terminal originator, archived flat JSONL and zstd, and duplicate live/archive representations. Assert origin only where evidence distinguishes it, one logical session per native identity/source, preserved selection after compression/archival, and old locators rejected on actual source replacement. Run `cargo test --test native_children codex`, `cargo test --test desktop_sessions codex`, and `cargo test --test compressed_source`.
- [ ] **Step 3: Implement explicit ownership and metadata discovery.** Parse supported parent/source/agent-role fields and `subagent_history_start_ordinal` with version/source evidence; do not generalize session_id as an immediate parent. Read session and archived roots under native CODEX_HOME or explicit overrides; no automatic WSL/remote traversal. Keep a private canonical-identity→current-physical-source mapping: compression strips only the native zstd suffix; an archive move within the same native store preserves an already confirmed session key using native session ID and native rollout filename/header evidence. Never open the canonical key path as if it must still exist; source locators use the current physical source/generation. Conflicting same-ID copies without native representation proof stay distinct/ambiguous. Use native cumulative semantics and owned request deltas; do not require derived state SQLite metadata to support Desktop.
- [ ] **Step 4: Run green and smoke the actual family/archive flow.** Run Codex transcript/roster/cursor/live/source/read-only tests; open synthetic old/new child counters through worker/UI, inspect direct cost and family identity, then archive/compress and reopen newest/older events without duplicates. Demonstrate indexing progress/cancel rather than full expanded allocation. Update archive/Desktop/ownership docs and honest format-version limits after smoke.

## Task B4: OpenCode provider/effort metadata, native children, and exact tool links

**Files:** `src/providers/opencode.rs` schema allowlists/row/projector-independent reading, `tests/support/opencode.rs`, `tests/{native_children,opencode_roster,opencode_usage,opencode_transcript,opencode_legacy,privacy,read_only}.rs`; `src/domain.rs` only for the tool link below, plus UI source/details caller migration.

**Consumes:** B1 reader, exact billing-provider price lookup and normalized observations.

**Produces:** add `ToolInteraction.child_session: Option<SessionLink>` for a confirmed native tool relation. `fn opencode_model_detail(raw: &str) -> Option<ModelDetail>` replaces current private model helper with observed providerID/variant/effort; raw variant remains raw unless a detected model/format proves it is a thinking level. `fn opencode_child_link(tool_name: &str, metadata: &serde_json::Value, shape: Schema) -> Option<String>` reads v2 `subagent`/`sessionID`, legacy `task`/`sessionId`, never parses arbitrary output text as authority.

- [ ] **Step 1: Add DB ownership/fork/provider regressions.** Create parent/child/nested sessions with exact separate usage; fork has `parent_id=NULL`, `fork_session_id=parent`, reset counters and copied history. Assert direct parent/child cost remains separate, fork is a root, children don't consume root-page slots, and equal model IDs through Go/Zen/another provider keep their own route/pricing. Missing provider remains Unknown/unpriced rather than default Anthropic/OpenAI.
- [ ] **Step 2: Add legacy/v2 links and hostile metadata regressions, run red.** Confirm correct case-sensitive metadata keys link to the child under their supported schema; wrong tool/key/ambiguous ID leaves no guessed jump. Put secret/control fields beside the permitted `agent/model/providerID/variant` columns and in message metadata; assert they never appear in Debug, warnings or screens. Run relevant `native_children`, `opencode_roster`, `opencode_usage`, `opencode_transcript`, `opencode_legacy` tests.
- [ ] **Step 3: Implement narrow schema extensions and normalized facts.** Add only the required allowlisted agent/model columns/JSON keys, extract schema-specific identity/effort, and retain original raw model-provider IDs with confirmed friendly labels. Session aggregate versus derived-message usage remains alternatives, not copies to sum. Preserve the current read-only authorizer's credential-table denial and WAL handling; no provider migration or blanket table reads.
- [ ] **Step 4: Run green and real-app synthetic smoke.** From a live WAL fixture, expand parent/nested child, view provider below harness and effort below model, open the child through a confirmed tool link, and inspect fork/direct-category costs. Exercise missing/unsupported provider and locked/schema-changing sources with valid unrelated roots. Update OpenCode capabilities and source-provenance docs after smoke.

## Task B5: Cancellable fast preview, paged details, loaded-only navigation and newest transcript entry

**Files:** `src/app.rs`, new `src/app/{roster,preview}.rs`, `src/app/{input,overlay,transcript}.rs`, `src/worker.rs`, `src/ui/mod.rs`; tests `worker`, `app_state`, `roster_paging`, `tui_flow`, `tui_inspector`, `tui_details`, `live_refresh`.

**Consumes:** B1 snapshots/messages and B2–B4 native relationships, plan01 measured/detail facts.

**Produces:**

- `PreviewState::{Empty, Loading { request_id, key, progress: WorkProgress }, Ready { request_id, key, summary: Arc<SessionSummary>, complete: bool }, Failed { key }}`. `App::select_session(&mut self, key: &SessionKey)` schedules preview only when identity/generation changed or retry is explicit, not every render.
- `App::apply_preview(&mut self, request_id: u64, key: SessionKey, summary: Arc<SessionSummary>, done: bool) -> bool`, `apply_details(&mut self, request_id: u64, key: SessionKey, page: SessionDetailPage) -> bool`, `preview(&self) -> &PreviewState`, `load_more_roots(&mut self)`, `toggle_children(&mut self)`, `load_more_children(&mut self, parent: &SessionKey)`, and `refresh_roster(&mut self)` enqueue only the declared worker requests. Cached facts are immediately visible with correct request key.
- `App::roster_rows(&self) -> impl Iterator<Item = RosterItem<'_>>` borrows facts; `RosterItem` is a session row with tree depth/selection/status, a root LoadMore row, or a child LoadMore row. Keep stable selected identity separate from visual indices. No per-frame allocation/formatting of all unshown rows.
- `Transcript::select_request` starts `PageKind::Tail`; Worker Select loads Backward/None and selects the newest returned event. Restart/invalidations preserve a prior reading anchor when present; opening a different session starts newest. Manual history reading pauses tail-follow and live events only increment the indicator until explicit End/follow action.
- Loaded-only search considers loaded roots plus available children, preserving parent context for child matches. Harness filter counts use six identities; archived/released filters retain evidence semantics. Sort uses frozen loaded root groups and does not detach children. New discoveries/deletions/late edges wait for explicit generation refresh, not set_now/rebuild movement.

- [ ] **Step 1: Add competing-selection and cancellation regressions.** Using the actual worker with synthetic large/small stores, request preview A then B and deliver A's response last. Assert selected key/preview remains B, stale progress/details are ignored, and CancelLoad releases abandoned work without corrupting B. With max_cached_sessions=1, select A→B→A; A rehydrates from metadata rather than disappearing or requiring full-history reload. Open details and a transcript for the same key, cancel only the details job, and assert its adapter continuation is released while the transcript still completes; after cancelling the final consumer on that key, a fresh request starts cleanly and returns correct content. Run `cargo test --test worker preview` and `cargo test --test app_state preview` red.
- [ ] **Step 2: Add end-entry/history/generation regressions.** A transcript larger than the event window opens at its actual last event and can page backward to the previous content without eagerly retaining the full history. After manually selecting an older event, append/refresh/invalidate; selection remains anchored while N-new changes. During pages1→2, add a new root, delete another, and discover an old orphan's parent: existing generation membership remains stable, explicit refresh updates it, and stale page replies are rejected. Child-only search keeps its loaded parent visible and does not auto-load more historical roots.
- [ ] **Step 3: Implement state transitions and queue ownership.** Give preview/detail/transcript purposes independent `ReadJobId`s so a preview cancellation cannot invalidate the transcript/source currently being read. Read `ReadProgress::Pending` as normal in-flight progress, check shutdown/selection between slices, and call `cancel_read(key, job)` for only the abandoned job; the adapter releases that job's decoder/index continuation while other jobs on the same key continue. Never key continuation state by session alone. Reuse current overlay/search/source-state invariants rather than a parallel application state machine.
- [ ] **Step 4: Run green and actual navigation smoke.** `cargo test --test worker --test app_state --test roster_paging --test tui_flow --test tui_inspector --test tui_details --test live_refresh`. Through real reader/worker/key/render path, select roots and children, inspect cached/partial/failed preview, load more roots/children, open newest, page back, append and resume. Record the observed selection/request transitions; docs/help gain loaded-only search and refresh semantics after this proof.

## Task B6: Aligned two-line list, rich preview/details, responsive layout and read-only mouse

**Files:** `src/ui/{mod,roster,preview,session,details,inspector,panels,widgets}.rs`, app input/focus/scroll modules, `src/main.rs`/config help; tests `tui_roster`, `tui_details`, `tui_flow`, `text_safety`, `privacy` and terminal guard tests in their current owner.

**Consumes:** A2/A3 complete/partial category facts and B5 borrowed rows/preview/state transitions.

**Produces:**

- `pub fn render_preview(frame: &mut Frame, app: &App, ctx: &RosterContext, area: Rect)` in new `ui/preview.rs`. Shared render helpers in `ui/session.rs` consume typed facts for both preview and details: no separate price/timing calculation in either surface.
- `RosterLayout::for_area(area: Rect, horizontal_column: usize) -> RosterLayout` owns pinned marker/title columns, two-line aligned pairs, preview split and visible column hitboxes. `pub fn handle_mouse(app: &mut App, event: MouseEvent, layout: &RosterLayout)` dispatches selection/Space-equivalent expansion/Load more only for displayed hitboxes, not stale transcript scrollback.
- `TerminalGuard::enter(mouse_enabled: bool) -> io::Result<TerminalGuard>` enables/disables capture with restoration on Drop/error/panic. Existing no-color/redaction behavior stays intact; `--no-mouse`/config disables capture without disabling keyboard operations.
- Column order: session/project+recency, harness/provider, model/thinking, lifetime tokens, API estimate, active time, latest timed request tok/s. Required facts remain horizontally reachable. At width≥150 reserve45 preview; narrower Tab swaps, minimum60×20. Left/Right scroll entire trailing columns, Space expands, Enter opens or activates focused Load more. Preview/details have independent vertical scrolling.

- [ ] **Step 1: Add actual-surface layout/navigation regressions.** Render typed real-fixture snapshots at 60×20,90×25,160×40 and resize between them. Assert provider and harness share a start column on their respective physical lines; effort aligns under model; no escape sequences break rows; narrow Tab preserves selected identity and exposes preview; horizontal scrolling reaches tokens/cost/active/speed; root and child Load more remain keyboard focusable. Test selection-sensitive behavior/column boundaries, not byte-for-byte incidental screen wording.
- [ ] **Step 2: Add rich-fact rendering regressions and run red.** Known partial category costs render a known subtotal plus missing coverage, not a complete dollar total. Unknown speed is not 0; latest old-model speed keeps model/age qualifier; TTFT isn't subtracted; context capacity Unknown suppresses percentage. Compaction event cost and context reduction are distinct, expandable summaries are sanitized, aggregate compression cost is not split into invented per-event costs. Run `cargo test --test tui_roster --test tui_details --test tui_flow --test privacy`.
- [ ] **Step 3: Implement borrowed rendering and responsive interactions.** Format only displayed rows/visible detail pages; render source/date/override/service exclusions alongside costs. Use typed helpers to render small positive money rather than `$0.00`; distinguish Unknown, Loading and Partial. Add harness/provider/origin labels with safe display names while preserving raw source details. Keep readonly actions only; no OMP revive/kill/steer controls.
- [ ] **Step 4: Implement mouse capture and restoration.** Hit-test coordinates against current render geometry; ignore hidden/narrow/offscreen cells. Preserve Shift-modified terminal selection/scroll behavior per Crossterm's supported capture contract and document it; `--no-mouse` restores ordinary terminal gestures. Exercise normal quit, Ctrl+C, startup/setup error and injected render panic/error to prove raw-mode/capture cleanup. Do not catch errors by silently swallowing broken controls.
- [ ] **Step 5: Run green and actual terminal smoke.** Run focused UI/privacy suites, then launch real AgentScope on synthetic stores in an interactive terminal if available. Exercise mouse+keyboard Load more, expansion, narrow preview, resizing, fastest/Unknown/partial metrics, compaction details and newest transcript entry. If no interactive terminal exists, execute the real reader→worker→App→Crossterm-event→Ratatui TestBackend surface in a throwaway smoke driver, record the visual/mouse terminal limitation, then remove the driver. Browser mockups are not evidence the Rust TUI works. Update README/help/architecture and screenshots only from verified behavior.

## Task B7: Integrated native safety and capability closure

**Files:** extend `tests/support/world.rs`, `tests/{acceptance,read_only,privacy,perf_budget,desktop_sessions,native_children,tui_flow}.rs`; public README/PRODUCT/ARCHITECTURE/CONTRIBUTING/config and key help. No additional runtime abstraction.

**Consumes:** B1–B6 complete paths; new harnesses arrive through plans03–05 later without another interface migration.

- [ ] **Step 1: Add native end-to-end regression assertions.** Existing three-harness World gains 125 root metadata cases, nested/fork native families, desktop shared stores/releases, zstd, an unknown billing provider, injected secret fields, a locked WAL and partial/truncated records. Assert exact root identities for 50/100/125 pages, direct token/cost accounting, child/fork placement, tail-entry selection and stale response rejection. Use current fixed clock/source inventories and sanitize every exposed surface; do not assert wiring or raw source text.
- [ ] **Step 2: Run red against the integrated implementation, fix only real gaps.** `cargo test --test acceptance --test read_only --test privacy --test perf_budget --test native_children --test desktop_sessions --test tui_flow`. Keep failing consumer regressions for discovered bugs; do not suppress warnings, repin incidental text, or replace unsupported records with fake success. SQLite sidecars are reported separately, not declared a provider-content mutation or silently ignored outside ADR scope.
- [ ] **Step 3: Smoke native release surfaces and document actual limits.** Exercise the real program with explicit synthetic roots/no processes, default bundled prices, all readonly controls and a broken source alongside valid ones. Observe no raw secrets, no provider-content changes, no network activity, responsive indexing cancellation, and honest unavailable desktop/WSL/cloud history. Update public docs to say Codex/Claude/OpenCode children and local desktop support implemented only after proof; OMP/Hermes/Pi remain not yet implemented until their plans complete.
- [ ] **Step 4: Hand the stable reader/domain/UI contract to plans03–05.** Reconcile exported signatures against plan01 and this plan and remove all obsolete refresh/delta/old-key/test helper paths. Do not run full gates per stage or call the whole requested release finished: the master plan owns six-harness integration and final verification.
