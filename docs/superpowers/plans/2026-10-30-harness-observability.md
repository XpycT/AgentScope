# Richer Harness Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver the full approved six-harness observability release: compact roster and preview, truthful API-equivalent prices and metrics, native subagents, local desktop stores, and bounded 50-group pagination.

**Architecture:** Keep the existing local reader → worker → App → Ratatui pipeline. Separate metadata discovery from cancellable hydration, normalize evidence into shared facts, and price owned usage using an offline official catalog. Add concrete OMP, Hermes and Pi adapters after the existing three readers and UI have migrated to the same bounded contract; no second application state machine or plugin framework.

**Tech Stack:** Stable Rust, edition2024; existing Ratatui/Crossterm, serde and read-only SQLite stack; one maintained streaming zstd decoder for native Codex archives. No async runtime, network service or persistent index.

**Spec:** [Approved design](../specs/2026-10-30-harness-observability-design.md). User approved the design on2026-10-30. This implementation plan is awaiting review and execution-method selection; no implementation or Rust verification is claimed.

## Global Constraints

- Observe only. Never launch, message, stop, approve, resume or modify a monitored agent. No provider writes, configuration rewrites, credential/cache scans, remote retrieval, VM mounting, telemetry or runtime networking.
- Preserve user changes. No commit, push, PR, release, reset, clean or stash without explicit authorization; generic skill commit steps do not apply.
- Facts remain reported/derived/estimated/partial/unknown. Subscription values use API-equivalent estimates, not subscription allocation or actual spend. Unknown is not zero; quiet or process presence is not progress.
- One worker; each work slice stops at256 records/rows,8MiB expanded bytes or100ms. Metadata probes are4KiB prefix/32KiB plain tail. Compressed discovery does not decompress history.
- Source record limit1MiB; transcript/detail pages500 events/256KiB; loaded event window20,000; pending loads16. Exact ownership ledger100,000 entries/8MiB; bounded active hydration state, not a full ledger per cached summary.
- Metadata defaults100,000 source records/64MiB; hydrated summaries2,000. Root and child batches50. Limits and incomplete coverage are visible, never hidden EOF. Initial root hydration uses an iterator, not50 simultaneous requests into a16-entry queue.
- Two-line roster aligns provider under harness and reasoning under model; total tokens/API cost/active/speed accessible down to60×20. Preview is45cols at≥150cols; narrow Tab swaps roster/preview and horizontal scrolling reveals whole columns.
- Initial50 top-level logical groups; child expansion and compression continuations do not consume root slots. Frozen generations cannot silently repage on hydration/new sources; explicit refresh preserves selected identity where possible. Search/filter explicitly cover loaded groups.
- Real direct requests are billed once. Source semantics, not provider names, determine cache/reasoning inclusion. Copied history, child claims, cumulative snapshots, main/aux duplicates and aggregate compaction rows cannot be blindly added.
- Native Pi only; no extension-specific child protocol. Claude/Codex desktop support covers accessible native stores only; cloud-only or VM-contained history stays unavailable. Every other requested harness includes evidence-backed nested children.
- Use deterministic synthetic stores only in tests and smoke. Sanitize every untrusted rendered field; diagnostics/Debug never dump raw content/config/metadata. SQLite WAL/SHM sidecars are the narrow documented ADR-0002 exception, not permission to mutate database content.
- Update existing public docs after behavior is exercised; no speculative implementation claims. No permanent scaffolds, stub trait methods, fake timings, compatibility shims or duplicate old/new public interfaces.

## Review Focus

1. Late discovery/reparenting or source deletion during paging must not skip/duplicate consumed roots or replace the selected preview. B1/B5 and R1 pin generation/identity transitions.
2. Replayed streaming/fork history and parent-side child totals must not inflate money; later authoritative revisions must not retain stale first-fragment output. A2, B2–B4, O2 and R1 pin owned direct usage and revision conservation.
3. Session aggregate usage must not become one fictional long-context request or measured throughput; aggregate compaction cost must not create another event or overlap event costs. A2/A3 and H2/H3 pin these distinctions.
4. Compressed/in-place-compacted histories must retain accessible ordering and honest context without full startup parsing, unbounded expansion or reopening a nonexistent canonical path. A4, B3, H3 and R1 pin source/generation/order boundaries.
5. Unsupported schema/private fields/hostile controls/locked stores must remain isolated and sanitized; one broken source must not hide healthy sessions or read credentials. B2/B4, all new adapters, and R1/R2 pin privacy/read-only resilience.

---

## Read and execute these parts

| Order | Plan | Tasks | Deliverable |
|---|---|---|---|
|1|[Shared foundations](2026-10-30-harness-observability-01-foundations.md)|A1–A4|Identity/domain cutover, official pricing/config, measured facts, streaming source and diagnostic inspection|
|2|[Discovery, native readers and TUI](2026-10-30-harness-observability-02-discovery-ui.md)|B1–B7|Atomic bounded-reader/worker cutover, native children/Desktop/archive, preview/details/paging/responsive controls|
|3|[OMP](2026-10-30-harness-observability-03-omp.md)|O1–O3|Complete native tree/transcript reader, direct usage, nested persisted subagents and evidence-backed metrics|
|4|[Hermes](2026-10-30-harness-observability-04-hermes.md)|H1–H3|Read-only profile DB reader, logical compression chains, native children, in-place and rotated compaction|
|5|[Native Pi](2026-10-30-harness-observability-05-pi.md)|P1–P3|Standard session tree/transcript/accounting with explicit limits, no invented native children/timings|
|6|This master|R1–R2|Six-harness acceptance, actual-program smoke, final gates, docs and whole-change review|

The parts are one release, not optional scope. Each task ends in coherent working behavior and targeted checks; no new reader is registered until its full supported contract works. O1/H1/P1 can introduce private tested parsing/storage components without half-working ProviderReader implementations.

### Dependency and edit ownership

Execute A1→A2→A3→A4→B1. B2/B3/B4 follow B1 and precede B5→B6→B7. Then O1→O2→O3→H1→H2→H3→P1→P2→P3→R1→R2. This conservative order matches the approved delivery sequence and avoids concurrent edits to reader registration, domain types, worker or shared fixtures.

A1 owns canonical domain/identity names and migrates all existing callers. A2 owns config/CLI parse arms, pricing types and normalized request ledger; A3 owns measured-fact helpers; A4 owns source framing/streaming. B1 owns the complete ProviderReader/discovery/worker/App protocol cutover; B2–B4 own native parsing and B5/B6 own state/presentation. Each new adapter owns only its concrete parser/storage/tests and complete registration step, with shared-file changes integrated by one owner. R1 owns cross-harness gaps, not a parallel replacement architecture.

Later tasks read the earlier **Interfaces/Produces** blocks, not stale exploratory artifacts. In particular:

- `Harness`, `SessionSummary.meta`, `UsageCoverage`, `RecordedMoney`, `TokenAccounting`, `SessionMetrics` and `SessionDetailPage` are defined by plan01; old Provider aliases and singular usage.model are removed, not shimmed.
- `UsageLedger::observe` takes an explicit `UsageUpdate::{Initial, AuthoritativeReplacement, Conflict}`; only adapters with producer evidence construct a replacement. Revisions replace the stored contribution/cost with the originally selected pricing context, and `SpeedAccumulator::revise` replaces that request's output/timing rather than adding a second request. Never infer a revision from increasing counters, timestamps, or source order.
- `UsageLedger` accepts proven owned request deltas/revisions, never Hermes session aggregates or copied request claims. Request-aware pricing is separate from provider-specific aggregate reconciliation. Compaction summary receives the same price schedule/reference policy as other estimates.
- Plan02 defines `ReadProgress<T>`, `ReadJobId`, discovery/hydration deltas, detail/transcript/source/count signatures, RosterSnapshot and worker messages atomically. All readers and tests migrate together; Pending is in-flight work, not an error/empty result. Cancellation is job-scoped: `cancel_read(key, job)` must not release another purpose's continuation on the same key.
- Source keys are canonical identities; source locators identify current physical sources/generations. Codex's native live/archive representation mapping must not turn a moved canonical path into a file-open target.
- Optional native evidence may remain Unknown; a missing field is not permission to manufacture it. OMP parses `ActiveLeaf::{Absent,Cleared,Set}` as distinct states; Hermes applies rewind tombstones across the whole compression chain; Pi uses a recorded assistant `durationMs` when the supported shape proves one. Optional inclusive-family UI may be omitted; mandatory direct accounting and no-double-counting may not.

### Current repository and reference map

The source areas and existing callers/tests were inspected during planning: `src/{domain,config,pricing,usage,worker,app,main}.rs`, `src/app/*`, `src/providers/{mod,codex,claude,opencode}.rs`, `src/ingest/*`, `src/ui/*` and `tests/support/*`. Each part names exact task-owned files and focused tests. Re-read relevant ranges at execution time; line positions may change. LSP was attempted but the installed stable toolchain lacks rust-analyzer; retry references before exported changes if it becomes available, otherwise use the already mapped callers plus scoped literal searches.

Official pricing sources are the design's Anthropic/OpenAI/OpenCode Go/Zen links. Fetch/review real text/coding price rows during A2, store source/check/effective metadata and explicit inventory, and verify arithmetic through the same parser. No live runtime fetch or demo-only catalog. Adapter parts link producer-format evidence; pin fixture schema assumptions during implementation. Hermes's current in-place archive/compact projection is required, not merely compression rotation. OMP's generic usage schema does not prove a persisted compaction-cost producer.

## Acceptance coverage

Numbers match the approved design's18 scenarios; the final column names the evidence to record during execution, not checks already run.

|AC|Requirement|Owners|Required execution evidence|
|---|---|---|---|
|1|Newest50 groups,50-more, EOF/limits and stable metadata ordering|B1/B5/R1|Exact125-root identity pages50/100/125; children/continuations excluded; provisional/ceiling/refresh behavior|
|2|Metadata-first work and cancellation; no stale preview|A4/B1/B5/R1|Bounded read counters plus A→B stale-response transition; same-key details/transcript job cancellation isolates continuations|
|3|Two-line aligned compact/responsive roster and controls|B6/R1|Actual surfaces60×20,90×25,160×40; Tab/scroll/resize/tree/Load more; mouse on/off|
|4|Five harnesses' nested native children; bad relations isolated|B2–B4/O1/O3/H1/H3/R1|Correct edges and selectable orphan/cycle/reused-ID/corrupt-child cases, no guessed links; OMP absent/null leaf distinction|
|5|No inherited/claim/main-aux/fork double billing|A2/B2–B4/O2/H2/R1|Exact direct contribution conservation; authoritative revisions replace, copies/conflicts counted once; labels truthful|
|6|Offline official prices, API equivalents and unknown/partial semantics|A2/R1|Reviewed catalog inventory plus deterministic request/tier/time/cache/model/override regressions|
|7|Per-category dollars/source/date reconcile|A2/B6/R1|48,200/12,800/cache synthetic arithmetic and actual detail rendering; unknown leg stays partial|
|8|Thinking level/model/branch/provider/origin evidence|A1/B2–B4/O2/H2/P2/B6|Changed/none/unknown/configured effort separated; raw variant and ambiguous origin not promoted|
|9|Start/end/elapsed/active/model time and resume semantics|A3/all adapters/B6|Overlapping interval union, open/resumed sessions, no turn-stop→session-end inference|
|10|Measured whole-request speed/TTFT/coverage/model attribution|A3/O2/B6/R1|5/8 timed requests,5500/70 weighted rate, invalid durations excluded; old-model qualifier|
|11|Prompt occupancy/effective window/compaction staleness|A3/O2/H3/P2/B6|215k/1M distinct from lifetime totals; rewrite removal and stale epoch tests|
|12|Compaction metadata/attributable costs, auxiliary once|A3/O2/H2/H3/P2/B6|Observed versus aggregate scope, unknown OMP cost, in-place/rotation/rewind-tombstone history and reduction≠billing|
|13|Open newest; older accessible; manual reading anchored|A4/B3/B5/all adapters|Actual last event beyond window, backward paging, append without forced jump, explicit resume|
|14|Native Pi complete supported behavior and honest limits|P1–P3/R1|Active-tree transcript/lifetime-owned usage, unknown children/end evidence, speed only when a supported recorded duration exists|
|15|Native local desktop/shared/archive stores once|B2/B3/R1|Synthetic DesktopCode/Cowork/Codex archive paths, released filter, no app-cache reads|
|16|Malformed/partial/unsupported/SQL/replacement/privacy safety|All adapter tasks/R1|Healthy unrelated roots survive each injected fault; no panics/raw leakage/provider mutation|
|17|Loaded-only filter/parent context/refresh selection|B1/B5/B6/R1|Unloaded match excluded; child match keeps parent; generation/reparent transitions stable|
|18|Offline/read-only native sources and safe diagnostics|All source tasks/R1/R2|Source inventories/hashes, denied SQL/private projections, bounded/observed runtime and ADR sidecar qualification|

## Task R1: Close six-harness consumer behavior and actual-program smoke

**Files:** Modify `tests/support/world.rs` and the concrete provider fixture modules from the five parts; extend `tests/{acceptance,read_only,privacy,perf_budget,roster_paging,tui_flow,tui_inspector,tui_details,live_refresh}.rs`. Fix real uncovered implementation gaps only in their owning existing source module. No new shared service/plugin/test-only provider.

**Consumes:** Completed A/B/O/H/P contracts, six complete real readers, existing synthetic World and fixed clock, actual worker/App/input/render pipeline.

**Produces:** `tests/support/world.rs::World::with_all_harnesses() -> Self`, an explicit synthetic-world constructor with all six harness roots, native families and fixture config. Preserve existing focused constructors rather than silently adding new fixtures to every old test. This fixture API is test support, not a public application abstraction. No new production API.

- [ ] **Step1: Add the cross-harness regression.** `tests/acceptance.rs::six_harness_roster_preserves_direct_usage_and_generation` builds125 top-level groups spread across all six harnesses, five native child families with one nested child, one Pi root without children, one Hermes continuation and a native archived Codex representation. Give roots explicit distinct activity stamps and deterministic expected ordered IDs. Assert pages50→100→125 with exact IDs/no duplicates; expanding children/continuations does not add root slots; each direct cost matches its owned synthetic requests; selecting a child's newest transcript reaches its actual final event. A provider-broken source and privacy sentinels coexist with healthy roots.
- [ ] **Step2: Add refresh and aggregate-boundary assertions, then run red.** `tests/acceptance.rs::six_harness_refresh_keeps_selection_and_truthful_unknowns` selects a timed OMP child, changes the roster generation during pending preview, adds a newer root and late parent, and resumes an older transcript anchor. Assert obsolete responses do not replace selection, existing pages remain frozen until refresh, loaded-only search cannot find an unloaded root, child matches retain the parent, and manual historical reading is not jumped. Hermes aggregate rows/Pi missing timing remain Unknown rather than generating request speed. Run `cargo test --test acceptance six_harness` and the affected focused regression; a failing consumer assertion is the expected red signal, not a made-up output transcript. If the implementation already meets a case, retain only distinct useful regression coverage.
- [ ] **Step3: Integrate only the observed gaps and run focused green.** Reuse the shared protocol and concrete adapters. Correct affected callsites and meaningful tests; delete obsolete incidental wording/default/wiring assertions rather than repinning them. Run `cargo test --test acceptance --test read_only --test privacy --test perf_budget --test roster_paging --test tui_flow --test tui_inspector --test tui_details --test live_refresh`; expected all pass with no leakage/mutation and exact consumer-visible outcomes above.
- [ ] **Step4: Launch the actual program on synthetic stores.** Materialize the World under a temporary directory and launch `cargo run -- --codex-sessions <tmp>/codex/sessions --claude-projects <tmp>/claude/projects --opencode-db <tmp>/opencode/opencode.db --omp-sessions <tmp>/omp/sessions --hermes-db <tmp>/hermes/state.db --pi-sessions <tmp>/pi/sessions --no-processes --config <tmp>/config.toml`. Use actual generated paths if fixture names differ. Explicit roots must scope discovery to the synthetic sources; do not fall back to private user homes/default desktop extras. Observe aligned roster, preview, total tokens/cost/active/speed, reasoning/provider, category dollars, context, compaction, five child families, root/child Load more and newest/older navigation. Resize through all three specified dimensions. Exercise mouse capture/opt-out and restoration on quit/error where terminal access permits.
- [ ] **Step5: Observe failure/limits/source safety.** On the real runtime or real pipeline smoke, inject one unsupported/locked source, partial append, truncation/replacement and over-budget compressed history beside valid roots. Observe isolated warnings, indexing progress/cancellation, metadata-limit/provisional indications and no raw sentinel exposure. Compare native source content inventories/hashes before/after; report SQLite sidecars separately. Check actual runtime networking behavior through available OS tracing; if unavailable, explicitly report that observation limit rather than claiming a trace. Never import live private agent stores into fixtures.
- [ ] **Step6: Record evidence and remove temporary smoke material.** If interactive TUI is unavailable, use a throwaway harness driving the real readers/worker/App/key/render pipeline with Ratatui TestBackend and report that terminal/mouse/proprietary-app behavior was not directly observed. Do not substitute screenshots/mockups or tests alone for actual changed-path execution. Remove throwaway fixture launcher/example/scripts after observed smoke; permanent meaningful synthetic regressions remain. Update only the public behavior proved so far.

## Task R2: Final verification, public contracts and whole-change review

**Files:** `README.md`, `docs/PRODUCT.md`, `docs/ARCHITECTURE.md`, `CONTRIBUTING.md` if its setup/checks changed, current CLI/help/config examples in their existing owning files, and the capability/limitation documentation updated by the parts. No new exploratory design transcript or release publication.

**Consumes:** R1 evidence, approved18-scenario matrix, current implemented source/tests, exact catalog source/effective manifest and config migration behavior.

**Produces:** A verified complete user-facing contract and an evidence-grounded completion report, not a commit/PR. No new public API.

- [ ] **Step1: Reconcile docs and every acceptance row.** Walk the18 rows against the actually exercised scenarios. Document six harnesses, five native subagent implementations and native Pi limits; local desktop/archive restrictions; API-equivalent versus reported/retained money; cache/TTL/reasoning/tier/tariff exclusions; timing/speed/context/compaction coverage; loaded-only search/50-group semantics and source/cancellation budgets; keyboard/mouse opt-out. Publish exact old-key migration (`max_sessions`→`max_cached_sessions`, pricing `provider`→`billing_provider`) and actual root precedence. Mark architecture implemented only where proved. Any missing mandatory row returns to its owning task; do not call a staged subset complete.
- [ ] **Step2: Run repository gates once after integration.** Run and read the complete output/exit status of `cargo fmt --all -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, and `cargo test`. If formatting is needed, format only the intended Rust changes and recheck; fix real failures, do not suppress warnings or narrow the suite. Record actual results, not hypothetical expected counts. Repeat affected verification after substantive fixes; do not repeatedly run full gates between tasks.
- [ ] **Step3: Obtain fresh whole-change review.** Invoke requesting-code-review and the strongest available reviewer under the selected execution workflow. Review owned accounting/price provenance, read-only SQL/JSON projection, source budgets/cancellation, lineage/compaction, physical versus canonical source identity and terminal restoration against the approved design. Resolve material findings, run the affected regression/smoke again if behavior changed, and update docs to match. No merge/commit is implied.
- [ ] **Step4: Verify completion evidence before reporting.** Invoke verification-before-completion; confirm every named scope item and18 scenarios is implemented/exercised or an explicitly documented native unavailable field (not omitted implementation). State commands/results, actual smoke surface, catalog date/source, supported schemas and remaining observation/platform limits. No completion claim for proprietary desktop/OS/mouse/network tracing that was not actually run. No actionable failed acceptance may be hidden as a follow-up.

## Plan self-review and execution handoff

The parent reviewed the plan against the approved design, shared signatures and all18 acceptance scenarios. Corrections include owned-request revisions, per-model aggregate pricing boundaries, OMP orchestration and copied-history evidence, Hermes in-place compaction/display order, aggregate-cost/event separation, and archive physical-source mapping. Plan parts specify contracts and consumer assertions, not implementation bodies or stub stages. Structural document checks are separate from future Rust/runtime verification.

Recommended method: **Subagent-driven**, with a fresh implementer/reviewer for each coherent task and one final whole-change review. The accounting/privacy/source-format boundaries make independent review valuable. Keep the conservative sequence above; no sibling agents edit shared domain/worker/registration state concurrently. If Native is chosen, the parent executes the same tasks and retains the final fresh reviewer. Neither method can reduce approved scope or skip actual-program smoke/full gates.

Before implementation, ask the user to review this master and its linked parts and select an execution method. Then invoke only the corresponding execution skill and perform its isolation/verification prerequisites. Do not begin product code, dependency changes or registration from this planning handoff.
