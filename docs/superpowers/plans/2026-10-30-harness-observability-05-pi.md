# Native Pi Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add useful native Pi sessions, usage/model/effort, branch-aware context, recorded summarization usage and complete transcript inspection, without inventing extension children or request timing.

**Architecture:** A `PiScanner` implements plan02's same bounded metadata/hydration/paged-reader contract. Pure Pi record/tree interpretation stays under the native adapter; price/math/rendering reuse plans01/02. Spend covers uniquely owned executed requests across branches, while context follows the latest recorded active path.

**Tech Stack:** Rust stable/edition 2024, existing serde_json/chrono/JsonlCursor, existing fixture/tempdir utilities. No new dependency.

**Spec:** [Approved design](../specs/2026-10-30-harness-observability-design.md), §§5–10 and acceptance 14/16. Depends on [plan01](2026-10-30-harness-observability-01-foundations.md) and [plan02](2026-10-30-harness-observability-02-discovery-ui.md); release closure belongs to the [master](2026-10-30-harness-observability.md).

## Global Constraints

- Native Pi only; extension-specific children are explicitly unsupported. Fork is not a subagent. No provider writes, config/credential reads, network, provider code execution, automatic migrations, or embedded-path traversal.
- Worker calls yield at 256 records/8 MiB/100 ms; metadata 4 KiB prefix/32 KiB tail; metadata cap 100,000 sources/64 MiB; summary cache 2,000; root/child batches 50.
- Records 1 MiB, pages 500 events/256 KiB, loaded events 20,000, pending reads16, exact request ledger100,000 entries/8 MiB. ReadProgress::Pending is normal continuation; cancellation drops abandoned hydration state.
- Standard inspected Pi records do not persist duration/TTFT/explicit session exit. Speed/end/active stay Unknown for those shapes; tool duration or message gaps never substitute. Context is a recorded/estimated snapshot, not live occupancy or lifetime tokens.
- Synthetic deterministic fixtures only; source-safe Debug and sanitized/masked rendering. Task red/green is targeted; master owns the four final full gates. No automatic commits or private-store validation. Plan commands are expectations, not claims of execution.

## Review Focus

1. An abandoned branch was expensive but is not the active context: lifetime cost retained, occupancy follows recorded active path (P2).
2. Fork copies/inherits parent history with remapped IDs: only producer-verified request ownership can exclude/include spend; never compare prompt text (P2).
3. Missing/invalid usage or an unknown routed model: preserve facts and partial coverage, never price unknown as free or pick a first-party route (P1).
4. Appended incomplete record/truncation or duplicate entry IDs: wait/invalidate deterministically, no phantom request or duplicated usage (P3).
5. Compaction or context_edit supersedes the last usage snapshot: stale pre-edit context cannot remain a precise percentage (P2).

## File responsibilities and exact shared interfaces

- Create `src/providers/pi.rs`: `PiScanner` discovery/hydration, ProviderReader methods, source/index continuation and cancellation.
- Create `src/providers/pi/record.rs`: pure typed field/usage interpretation; `src/providers/pi/tree.rs`: entry-ID/parent path and fork ownership index; `src/providers/pi/events.rs`: normalized transcript/detail mapping. Do not introduce another provider trait or worker.
- Modify `src/providers/mod.rs`, `src/main.rs`, config/root resolution/help only to register the real adapter. `--pi-sessions` and `Config.harnesses.pi` are supplied by the shared plans; do not add duplicate flags/settings paths.
- Create `tests/support/pi.rs`, `tests/pi_sessions.rs`, `tests/pi_tree.rs`, `tests/pi_transcript.rs`; extend existing `tests/support/world.rs`, acceptance/read_only/privacy/perf suites instead of one-use wiring tests.

Consume plan01's exact types, not guessed alternatives: `TokenAccounting { input: InputAccounting::AdditiveCaches, output: OutputAccounting::IncludesReasoning }` for a verified native Pi format; `RecordedMoney.saved_estimate: Option<MoneyFact>` with `MoneySource::SavedCatalogEstimate` for a verified locally computed cost; raw/unproven money remains `unverified_usd`. Never call Pi's estimate reported API billing. Model provider comes from the recorded route; confirmed physical responseModel may refine the model identity without inventing aliases.

Use `UsageLedger::{new,observe,snapshot}` with `UsageUpdate::{Initial,AuthoritativeReplacement,Conflict}`, `PriceSchedule::model_context_capacity(&str,&str)`, and plan01 measured/context/compaction types. A repeated identical usage record is Duplicate; only producer-proven later values use `AuthoritativeReplacement`. `ActivityStamp.class` is `AggregationClass`, not an invented kind field. Context fields are `recorded_at` and `compaction_epoch: Option<String>`. SessionSummary has `meta`, not duplicated top-level metadata fields.

`PiScanner::new(config: &Config) -> Result<Self, ProviderError>` must be cheap; filesystem opening occurs in the worker. Implement every plan02 ProviderReader method, including job-scoped `cancel_read`. All indexing reads receive their trailing `RefreshBudget` and return `ReadProgress<T>`; test helpers use the canonical discover/hydrate/transcript/details/source helpers from plan02 with a finite step bound. No old refresh/page shim.

Public source references to verify/pin before parsing: [Pi session format](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/session-format.md), [AI message/usage types](https://github.com/earendil-works/pi/blob/main/packages/ai/src/types.ts), [session manager and fork producer](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/session-manager.ts), [usage totals](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/usage-totals.ts). Format v3 is inspected prior art, not permission to assume fields in unknown future versions. Confirm a producer's ownership/schema semantics rather than treating a permissive type as evidence of a recorded field.

## Task P1: Native discovery and truthful request/model/effort hydration

**Files:** Pi scanner/record module, fixture builder, `tests/pi_sessions.rs`, existing config/perf/privacy suites.

**Consumes:** shared metadata, bounded reader contract and price ledger.

**Produces:** private `PiHeader { version: Option<u32>, id: String, timestamp: Option<Timestamp>, cwd: Option<String>, parent_session: Option<String> }`; `parse_header(value: &Value) -> Option<PiHeader>`; private `PiEntry { id: String, parent_id: Option<String>, timestamp: Option<Timestamp>, ordinal: u64, offset: u64, kind: PiEntryKind }`; `parse_entry(value: &Value, ordinal: u64, offset: u64) -> Result<PiEntry, PiRecordError>`. PiEntryKind enumerates the actually supported message/model_change/thinking_level_change/usage/compaction/branch_summary/context_edit/session_info/custom shapes and retains unknown records as diagnostics. Define `PiRecordError::{Malformed, Unsupported}` with no raw content. `parse_pi_usage(value: &Value, model: Option<ModelDetail>, provenance: Provenance) -> UsageObservation` validates categories, ownership identity and retained money; it does not infer semantics solely from total equality.

Fixture helpers in `tests/support/pi.rs`: `write_session(root: &Path, bucket: &str, name: &str, entries: &[Value]) -> PathBuf`, `header(id: &str, parent: Option<&str>) -> Value`, `message(id: &str, parent: Option<&str>, role: &str, content: &str, usage: Option<Value>) -> Value`, `usage(input:u64,output:u64,read:u64,write:u64)->Value`, `model_change(id:&str,parent:&str,provider:&str,model:&str)->Value`, `thinking_change(id:&str,parent:&str,level:&str)->Value`. All timestamps use the fixed fixture clock and provider/model fields are explicit synthetic values.

- [ ] **Step 1: Add owned-request/provider/effort regressions.** Create a session with input100/output40/cacheRead20/cacheWrite10, reasoning15 already in output, a repeated identical assistant/request ID, then a recorded model/provider/effort change. Assert direct lifetime total170, one request counted once, reasoning15 not added to total/output, earlier request retains its model, latest metadata shows the new provider/model/configured effort and no invented effective effort. Two copied files sharing header id remain distinct sources; an equal model name through a different provider stays separately priced/unpriced.
  ```rust
  assert_eq!(summary.usage.as_ref().unwrap().total, Some(170));
  assert_eq!(summary.usage.as_ref().unwrap().categories.reasoning, Some(15));
  assert_eq!(summary.meta.model.as_ref().unwrap().provider.as_deref(), Some("openai"));
  assert_eq!(details.requests[0].model.as_ref().unwrap().provider.as_deref(), Some("anthropic"));
  assert_eq!(summary.usage.as_ref().unwrap().coverage.observed_requests, 1);
  ```
- [ ] **Step 2: Add malformed/no-timing/discovery boundary regressions and run red.** Include a record with negative input, fractional output, missing usage, and a toolResult with durationMs9000 between messages five seconds apart. Assert missing/invalid categories remain None, known additive categories can still be interpreted from a proven schema even when totalTokens is absent, a conflicting reported total produces warning/partial coverage rather than silently redefining inclusion, and for fixture shapes without recorded request duration there is no request TTFT/end/active/model time. When a supported producer version records assistant `durationMs`, parse it as measured model time; never infer TTFT, session end or active time from gaps, and never derive timing from the toolResult duration. Include credential-looking non-session files and unsupported headers; valid sessions remain discoverable. Run `cargo test --test pi_sessions` and the Pi filters of existing `perf_budget`/privacy suites.
- [ ] **Step 3: Implement bounded metadata and hydration.** Enumerate only configured standard project buckets/session JSONLs, using bounded header/tail metadata rather than all message bodies. Parse provider/model/thinking changes and assistant usage through checked integer/money fields, including a recorded assistant duration when the detected shape proves it; otherwise leave timing Unknown. Treat saved cost as a retained estimate only when the supported producer semantics establish that origin; otherwise expose unverified money and calculate an estimate only from proven route/accounting. Missing format evidence is partial/Unknown, not an invented generic legacy schema. Every hydration slice can yield/cancel with bounded ledger state.
- [ ] **Step 4: Run green and inspect concrete hydration.** Run `cargo test --test pi_sessions`, `cargo test --test perf_budget pi`, `cargo test --test privacy pi`. Drive the Pi parser/hydration components directly over a synthetic session, inspect distinct provider/effort/category costs and Unknown-or-recorded timing, then cancel/reselect during a multi-slice hydrate. Application reader/worker/App selection and paged views are verified in P3 after the complete reader exists; do not register a partial adapter or use a test-only reader path. Document native Pi metadata/accounting capability and no extension-child promise only after this component-level proof, then complete the app documentation in P3.

## Task P2: Tree/owned fork history, context edits and summary usage

**Files:** Pi tree/events modules, record/scanner, `tests/pi_tree.rs`, fixture helpers, shared native acceptance/privacy cases.

**Consumes:** P1 requests/metadata, plan01 context/compaction/price helpers and exact request IDs.

**Produces:** private `PiTree::new(max_entries: usize, max_bytes: usize) -> Self`, `insert(&mut self, entry: &PiEntry) -> PiInsert`, `active_path(&self) -> Result<Vec<u64>, PiTreeError>`, `ownership(&self, entry_id: &str) -> PiOwnership`. `PiInsert::{Inserted, Duplicate, Limited}`, `PiOwnership::{Owned, Inherited, Unknown}`, `PiTreeError::{Cycle, MissingParent, Limited}`. Keep only bounded header/path facts/ordinals, not message bodies; active-path allocation is itself bounded and can be cached until append. `fold_context(&self, prices: &PriceSchedule) -> ContextUsage` selects the latest supported assistant prompt snapshot or invalidates it after later compaction/context_edit. `parentSession` is matched only to sources already discovered inside allowed roots; it never grants permission to open an arbitrary embedded path.

- [ ] **Step 1: Add branch/compaction/context regressions.** Build entries user u→assistant a (prompt120,output10), branch old→assistant b (prompt900,output20), then return to a and append assistant c (prompt150,output30). Lifetime owned output is60 while active-path latest prompt is150, not lifetime total or the abandoned900. Append compaction with before500 and attributable summary usage input4/output2; assert separate purpose=Compaction, known summarization cost is included once in lifetime cost but not conversation speed. Its context reduction is not billed tokens. A compaction without usage has Unknown cost.
  ```rust
  assert_eq!(summary.usage.as_ref().unwrap().categories.output, Some(62));
  assert_eq!(before_compaction.metrics.context.used, Some(150));
  assert_eq!(after_compaction.metrics.context.used, None);
  assert_eq!(compaction.usage.as_ref().unwrap().request.as_ref().unwrap().purpose, RequestPurpose::Compaction);
  assert_eq!(unpriced_compaction.usage, None);
  ```
  For branch_summary and tool-result nested model usage, count only actually persisted unique usage; do not assume a JSON `usage` object of arbitrary shape is a new billable request.
- [ ] **Step 2: Add fork-ownership/cycle regressions and run red.** Inspect/pin the native fork producer's actual copied/remapped entry and response IDs. Fixtures must mirror those semantics: copied parent-owned requests are excluded from the fork's direct spend only with explicit lineage plus producer-verified identities/boundary; genuinely new identical-text requests remain owned. An out-of-root/missing parent or remapped IDs without a safe ownership boundary leave inherited usage partial, never follow the path or charge the complete copied prefix. Add missing-parent/cycle/depth/ledger-limit fixtures and a later context_edit invalidating the old usage snapshot. Run `cargo test --test pi_tree`.
- [ ] **Step 3: Implement active-path and lifetime separation.** For the supported Pi format, follow id/parentId from the last recorded leaf; navigation not durably appended remains Unknown rather than reading live Pi state. Keep all uniquely owned executed branches in lifetime ledger and only active-path snapshots in context. Mark capacity catalog-derived only when applicable for the recorded provider/model; missing capacity suppresses percentages. Fold recorded compaction/branch-summary usage as its actual purpose, preserving provenance and not inventing method/duration/after-token fields absent in the source. Record fork linkage as `lineage.fork`, never `lineage.parent` subagent.
- [ ] **Step 4: Run green and inspect a branch/summary session.** `cargo test --test pi_tree --test pi_sessions`. Exercise the concrete PiTree/ownership/fold components over a synthetic abandoned branch and current context; assert the lifetime ledger retains abandoned-branch spend, context shows only the active path, raw provenance remains inspectable, and no fake native subagent rows exist. Paged details/source views and application selection are verified in P3 once the complete reader exists.

## Task P3: Paged transcripts/source, live-file resilience and full app registration

**Files:** Pi scanner/events, `src/providers/mod.rs`, `src/main.rs`, config/help, `tests/pi_transcript.rs`, fixture World and existing acceptance/read_only/privacy/perf suites, public docs.

**Consumes:** P1/P2 complete facts, plan02 progress/cancellation/source-generation handling, existing UI.

**Produces:** complete ProviderReader implementation with budgeted transcript/details/count/source reads, independent native anchors/source locators and job-scoped `cancel_read(key, job)`. Native session pages are chronological with stable ID/ordinal ties; newest entry opens through the existing worker Select semantics. Details page count/bytes include request/summary/recent lists together, not separate unbounded lists.

- [ ] **Step 1: Add transcript/detail/source and interruption regressions.** With a synthetic session larger than one page, assert newest open selects the true last event, backward pages recover exact older identities without duplicate usage, source inspection maps to the original record, and unsupported record diagnostics do not erase valid neighbours. Append half a JSONL line: no phantom assistant until newline arrives, then one owned request. Truncate/replace: stale generation/anchors invalidate safely. Cancellation during a 256-record-budget hydration followed by a different selection must leave the second preview correct and release the first ledger.
- [ ] **Step 2: Run red; implement real paging/resilience/registration.** `cargo test --test pi_transcript --test pi_tree --test pi_sessions`. Use existing JSONL cursor/index conventions and plan02 ReadProgress to yield on expensive indexing; never represent normal pending work as Unavailable. Register `PiScanner::new(&Config)` in lazy `build_readers`, add checked roots and native capability/help text, and reuse `--pi-sessions` precedence. No fake entries for unsupported extension subagents; capability text must explicitly say unsupported, not report a verified zero children.
- [ ] **Step 3: Run green and extend safety fixtures.** `cargo test --test pi_transcript --test pi_sessions --test pi_tree`, then relevant acceptance/read_only/privacy/perf filters. Add a Pi store and credential decoys to the existing World constructor/readers without silently changing expected native accounting of existing harnesses. Verify reader and source APIs expose no raw private fields through Debug/errors, fixtures remain deterministic, and malformed Pi files do not hide other harnesses.
- [ ] **Step 4: Smoke the actual application and update docs.** Launch AgentScope against synthetic Pi plus existing native roots with `--no-processes`, select/preview/details/open-tail/backward/search/source/refresh, exercise partial append and a broken sibling. Observe Unknown-or-recorded request speed (only measured when the fixture records a supported duration), Unknown end/active, honest catalog context and summary costs, retained fork roles and unsupported extension children. If only TestBackend/event smoke is available, report the visual terminal limit. Update README/PRODUCT/ARCHITECTURE/help after observed behavior; retain no private captured data or throwaway smoke driver. Master final gates and six-harness closure remain outstanding.

## Stage self-review

- All Pi features/explicit exclusions in §9.6 map to P1–P3 and observable regressions; no assumed struct fields or undefined test helper bodies are needed.
- Shared contracts use exact plan01/02 names; the only private types here are owned/defined above. Parser bodies and visual formatting are left to the implementer, not embedded as source transcripts.
- Unknowns are source/version limitations, not permission for incomplete adapters: supported native records/transcripts/accounting are end-to-end, extension children are explicitly out of release scope, and no request speed/end signal is invented beyond a duration the supported producer actually records.
