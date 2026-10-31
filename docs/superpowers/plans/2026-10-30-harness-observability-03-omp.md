# OMP Sessions and Native Subagents Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read OMP sessions and nested artifact-tree subagents with direct lifetime accounting, persisted request speed/context and complete transcript/detail inspection, without coupling AgentScope to OMP's live runtime.

**Architecture:** Private metadata/link/tree parsing feeds the same plan02 reader/worker/roster path. A cancellable hydrator counts uniquely owned executed requests across recorded branches; context follows the recorded active leaf. Parent-side task usage is child accounting, not an extra parent model request.

**Tech Stack:** Rust stable/edition2024; existing serde_json/chrono/JsonlCursor/source and text helpers. No new dependency or provider-code execution.

**Spec:** [Approved design](../specs/2026-10-30-harness-observability-design.md), especially §§6–10, and the [master](2026-10-30-harness-observability.md). Depends on completed [plan01](2026-10-30-harness-observability-01-foundations.md) and [plan02](2026-10-30-harness-observability-02-discovery-ui.md).

## Global Constraints

- Observe only; never open OMP auth/config secrets, history/models/stats databases, arbitrary blobs or embedded file paths to infer missing facts. No live registry, provider writes, network, agent controls or persistent index.
- Worker slices256 records/8MiB/100ms; metadata4KiB prefix/32KiB plain tail; metadata100,000 sources/64MiB; cache2,000 summaries; root/child batches50. Record1MiB, pages500events/256KiB, loadedwindow20,000, pendingloads16, ledger100,000entries/8MiB.
- ReadProgress::Pending is normal bounded continuation. `cancel_read(key, job)` drops only that job's abandoned mutable ledger/index work; another purpose on the same key continues. Source replacement invalidates stale locators; no per-frame full scan or raw-body retention.
- OMP before/after/method compaction metadata is supported; compaction cost is Unknown absent another verified producer. Hub live active time/token meters are not persisted facts.
- Content-free Debug, synthetic fixed-clock fixtures and sanitized/masked display everywhere. No automatic git commits; targeted red/green and real app smoke only during execution; master owns final full gates. No implementation/test execution while planning.

## Review Focus

1. Parent task results contain child rollups while child files have their own calls: direct parent excludes claims, descendants remain separate and no double billing occurs (O2).
2. Fork/continuation copies repeat entry IDs and retain old token counters: verify identity/ownership, never use timestamp comparisons as an inherited-history boundary (O1/O2).
3. A model_usage record is written off the active branch: count owned money in lifetime usage but do not move current context/effort to an inactive path (O2).
4. Explicit session_exit followed by a resume or compaction without a new prompt snapshot: remove stale end/context facts rather than claim completion/current occupancy (O2).
5. Spoofed title/header/session_init text, torn tail, cancellation, or replacement: structural metadata only, isolated diagnostics and stable source generations (O1/O3).

## Files, shared contracts and evidence

- Create `src/providers/omp.rs` (scanner/reader scheduling), `omp/metadata.rs` (known layout/title/header probes), `omp/links.rs` (explicit fork/continuation/child relationships), `omp/tree.rs` (bounded entry/active-leaf index), `omp/records.rs` (pure observation/event mapping).
- Create `tests/support/omp.rs`, `tests/omp_discovery.rs`, `tests/omp_hydration.rs`, `tests/omp_transcript.rs`; extend existing World/read_only/privacy/perf/acceptance owners.
- Modify `src/providers/mod.rs` to expose the real adapter module, then `src/main.rs::build_readers/checked_roots` when the complete reader exists. The shared plans own `CliOverrides.omp_sessions`, parsing `--omp-sessions`, default root resolution and global help syntax; do not add duplicate CLI arms. This plan adds implemented-capability help/docs only.

Use plan01 exact identities/types: `SessionSummary.meta`, `UsageCoverage { observed_requests, priced_requests, complete, reason }`, `TokenAccounting {input,output}`, `RecordedMoney {reported_api,saved_estimate,unverified_usd}`, `CompactionObservation {id,timestamp,method,trigger,summary,before_tokens,after_tokens,duration_ms,usage,scope,provenance}`, and context `recorded_at/compaction_epoch:Option<String>`. `UsageScope` remains Session/Turn/Message. `ProviderReader` and all budgeted ReadProgress methods are exactly plan02 B1; no refresh compatibility path. Use `UsageLedger`/`SpeedAccumulator` only after ownership/deduplication, submitting replacements only through `UsageUpdate::AuthoritativeReplacement`/`SpeedAccumulator::revise` with producer evidence; never on parent task-result claims.

Source pin: OMP `6a86df8539fccf3ad7818081266e554022fc21bc` (public main inspected on2026-10-30, installed18.14.0 may differ): [session entries](https://github.com/can1357/oh-my-pi/blob/6a86df8539fccf3ad7818081266e554022fc21bc/packages/coding-agent/src/session/session-entries.ts), [session manager](https://github.com/can1357/oh-my-pi/blob/6a86df8539fccf3ad7818081266e554022fc21bc/packages/coding-agent/src/session/session-manager.ts), [persisted agents](https://github.com/can1357/oh-my-pi/blob/6a86df8539fccf3ad7818081266e554022fc21bc/packages/coding-agent/src/registry/persisted-agents.ts), [pricing](https://github.com/can1357/oh-my-pi/blob/6a86df8539fccf3ad7818081266e554022fc21bc/packages/catalog/src/models.ts).

Important producer facts: createBranchedSession copies selected path entry IDs unchanged; forkFrom can reset inherited **money but retain tokens**; a moved session records continuationOf and copies history. This is why `entry.timestamp < header.timestamp`, zero cost or repeated text cannot establish request ownership. Plain usage.cost does not identify billed-vs-catalog provenance; retain it unverified unless the supported producer supplies stronger source evidence. Missing effort/null is Unknown unless the supported version explicitly encodes off. Unknown .gz archives are diagnosed as unsupported source variants, not silently treated as ordinary JSONL; gzip support is not part of the approved native JSONL/explicit Codex-zstd scope.

## Task O1: Metadata, tree identity and native relationship parsing

**Files:** OMP metadata/links/tree modules, fixture builders, `tests/omp_discovery.rs`; no incomplete ProviderReader implementation or early CLI registration.

**Consumes:** shared SessionMetadata/source limits and existing bounded file/text primitives.

**Produces:** private `OmpCatalog::new(roots: Vec<PathBuf>, max_sources: usize, max_bytes: usize) -> Self`, `step(&mut self, budget: RefreshBudget) -> Result<DiscoveryDelta, ProviderError>`; `OmpHeader { id: String, created_at: Option<Timestamp>, cwd: Option<String>, parent_session: Option<String>, continuation_of: Option<String> }`; `parse_header(value: &Value) -> Option<OmpHeader>`; `parse_title_slot(line: &[u8]) -> Option<String>`; `resolve_omp_links(metadata: &mut [SessionMetadata], headers: &[OmpHeader])` matches only discovered source/path/native IDs with explicit native structural evidence.

Private `OmpTree::new(max_entries: usize, max_bytes: usize)`, `insert(&mut self, header: OmpEntryHeader) -> OmpInsert`, `active_path(&self) -> Result<Vec<u64>, OmpTreeError>`, where `OmpEntryHeader { id:String, parent_id:Option<String>, active_leaf:ActiveLeaf, ordinal:u64 }`, `ActiveLeaf::{Absent,Cleared,Set(String)}`, `OmpInsert::{Inserted,Duplicate,Limited}`, `OmpTreeError::{MissingLeaf,Cycle,Limited}`. Parse the producer's exact three states: an absent `activeLeafId` property means `Absent` (select this entry as the leaf), an explicit null means `Cleared` (no active leaf), and a string means `Set(id)`. Missing/null/explicit distinctions are preserved into `OmpTree::insert`; last entry's activeLeafId/id policy is version-bound, not a generic newest timestamp rule. Store headers/ordinals/needed fingerprints, not raw bodies.

Fixture builder `tests/support/omp.rs`: `write_session(root:&Path,bucket:&str,name:&str,entries:&[Value])->PathBuf`, `header(id:&str,parent:Option<&str>,continuation:Option<&str>)->Value`, `title_slot(title:&str)->Vec<u8>` (valid256-byte physical slot), `entry(id:&str,parent:Option<&str>,kind:&str,payload:Value)->Value`; synthetic assistants/model_usage/task/compaction/exit payloads mirror the pinned producer and fixed clock. Reuse plan02's bounded discover/hydrate/read-ready helpers rather than duplicate unbounded loops.

- [ ] **Step1: Add metadata/relationship regressions.** Build parent p, artifact child c, nested child g, an explicit fork f and continuation p2. Assert strong child links/nesting, stable logical root and ordered member keys for continuation, fork separate, no title/message text spoofing of headers, and no embedded outside-root path read. A missing/conflicting parent is Unresolved/Ambiguous and remains inspectable. Two files sharing a header ID but different physical identities do not merge without an explicit unambiguous continuation edge.
  ```rust
  assert_eq!(child.lineage.parent, Some(SessionLink::Resolved(parent.key.clone())));
  assert_eq!(fork.lineage.fork, Some(SessionLink::Resolved(parent.key.clone())));
  assert_ne!(fork.key, parent.key);
  assert_eq!(continued.key, parent.key); // stable earliest logical root
  assert_eq!(continued.members, vec![parent.key.clone(), next_segment_key]);
  ```
- [ ] **Step2: Add probe/active-leaf safety regressions and run red.** Metadata discovery over multi-MiB files must stay within prefix/tail work, leave middle usage unhydrated and mark fallback mtime approximate; title slot length counts bytes and a malformed slot cannot swallow a valid header. Include paired fixtures with an omitted `activeLeafId` (selects the entry) versus explicit null (clears it), an off-branch `ActiveLeaf::Set`, a missing leaf target, cycles, missing parents, source replacement and depth/budget limits; unsupported portions warn without hiding unrelated sessions. Run `cargo test --test omp_discovery` (or provider-module unit tests until the complete public scanner is registered); expected failure is missing/incorrect native interpretation.
- [ ] **Step3: Implement catalog and explicit link/tree rules.** Walk only known bucket/session/artifact JSONL layouts with bounded directory stack/visited identities; no arbitrary blob/tool-output traversal. Link artifacts using the provider's verified structural/header evidence; source directory shape is not a generic cross-harness lineage heuristic. Suppress duplicate continuation segments from root slots but keep physical members for transcript/source reads. Keep late metadata changes as refresh-available according to the shared index. Tests may exercise these concrete parsing/index modules without inventing Unsupported adapter methods for later tasks.
- [ ] **Step4: Run green and inspect metadata.** Run the owning OMP metadata/tree regressions and global relevant perf/privacy filters. Use an actual metadata probe over the synthetic store and observe parent/fork/continuation IDs, approximate timestamps and warnings. No live-runtime/agent-control dependency or application support claim yet.

## Task O2: Direct lifetime request accounting and persisted measured facts

**Files:** OMP records/tree/scanner hydration core, `tests/omp_hydration.rs`, fixture helpers; no partial trait stubs.

**Consumes:** O1 indexed sources/ownership evidence, plan01 price/usage/metrics.

**Produces:** private `OmpHydration::new(meta: SessionMetadata, config: &Config) -> Result<Self, ProviderError>`, `step(&mut self, budget: RefreshBudget) -> Result<HydrationDelta, ProviderError>`; `map_omp_usage(entry: &Value, provenance: Provenance) -> Option<UsageObservation>`; `owned_request(entry_id: &str, response_id: Option<&str>, source: &SessionKey, lineage: &OmpCatalog) -> OmpOwnership`, `OmpOwnership::{Owned,Inherited,Unknown}`. `map_omp_usage` binds the recorded assistant `message.serviceTier` into `RequestMetadata.service_tier`; use a supported region field only when the detected schema persists it. Unsupported known tiers/regions remain partial/unpriced rather than being priced as standard. `cancel` drops its bounded mutable ledger/tree/source work; completed immutable summary caches do not retain all raw requests.

Request identity prefers confirmed provider response/request ID; a version-verified unchanged native entry ID is usable across an explicitly linked copied prefix. Do not synthesize identity from `<timestamp>` to split otherwise identical copied requests. Compare only already discovered linked source facts, never open arbitrary parentSession paths. Ambiguous inherited ownership leaves direct usage partial/unknown; identical new text with a distinct request ID is real new spend.

- [ ] **Step1: Add child-claim/branch/orchestration regressions.** Use synthetic cards with output=10 USD/M and explicitly known zero input/cache counts: parent owned output10,000 costs0.10; task result quotes child cost0.20; child owned output20,000 independently costs0.20; an off-active-branch auxiliary output3,000 costs0.03. Assert parent direct0.13, child0.20, optional deduplicated family0.33 (never0.53), owned lifetime off-branch request retained and conversation speed excludes auxiliary calls. Do not manufacture trusted prices from OMP's ambiguous saved `usage.cost`. Separately include additive orchestration input/output/cache-read usage: bill it exactly once but exclude it from prompt occupancy.
  ```rust
  assert!((parent_direct - 0.13).abs() < 1e-9);
  assert!((child_direct - 0.20).abs() < 1e-9);
  assert!((unique_family - 0.33).abs() < 1e-9);
  assert_eq!(summary.metrics.context.used, Some(215_000));
  assert_eq!(summary.metrics.speed.measured_requests, expected_conversation_timed_calls);
  ```
  Parent task claims remain private adapter accounting evidence; they are not inserted into details.requests or the shared UsageLedger. The inclusive-family rollup is optional; no new public claim registry is required. Price a recorded priority/flex request through the card's service modifier; an unsupported recorded tier remains partial/unpriced, and changing the session's current tier setting later must not reprice that earlier request.
- [ ] **Step2: Add fork/retained-money/context/exit regressions and run red.** Copy parent entry IDs into an explicitly linked fork, optionally reset their money to zero while retaining tokens. Assert inherited usage is not newly charged; new IDs count; missing parent proof stays partial even when timestamps precede the fork header. Retain raw ambiguous cost provenance rather than call it billed. Use request duration19,000ms/output1,600/TTFT1,300ms for84.2tok/s; no-duration source is Unknown. Context snapshot215,000 with rewrite_removed5,000 becomes210,000, not output-inclusive total. Compaction without attributable usage invalidates stale context and has Unknown cost. Exit then a later request clears obsolete ended_at. Do not classify a zero-usage aborted assistant as a nonexecuted placeholder solely from its shape. Run `cargo test --test omp_hydration` red.
- [ ] **Step3: Implement the resumable facts fold.** Count uniquely owned assistant/model_usage calls across executed branches, normalize the recorded additive cache and orchestration categories, bind model/effort/purpose to each call, and use the exact shared price policy. OMP usage.cost without billed/catalog provenance stays unverified; recomputed API equivalent and original number are both inspectable. Prefer contextSnapshot.promptTokens/rewrite removal, then an explicitly supported prompt-token fallback, never usage.totalTokens. Model changes/effort for current context follow the active path; lifetime cost does not. Record session_exit only while no later reopen evidence exists. No disk-backed active timer is assumed; model request time remains separately measured. Compaction metadata is supported but cost remains Unknown unless actual persisted attribution is verified.
- [ ] **Step4: Run green and smoke the hydration fold.** `cargo test --test omp_hydration --test omp_discovery`. Through actual OmpHydration on temp sources, observe partial progress/cancel/restart, direct versus child costs, off-branch expense, provider/effort/context and unknown compaction cost. Inspect raw sources only through the safe source-record path, not provider imports or live OMP control APIs.

## Task O3: Complete reader, paged history and application integration

**Files:** `src/providers/omp.rs`, records/events/index reading, `src/providers/mod.rs`, `src/main.rs::build_readers/checked_roots`, `tests/omp_transcript.rs`, existing World/acceptance/read_only/privacy/perf/live suites, public docs/help.

**Consumes:** O1/O2 complete metadata/hydration and every plan02 bounded reader/message method.

**Produces:** `OmpScanner::new(&Config) -> Result<Self, ProviderError>` and a complete ProviderReader implementation: discover/hydrate/job-scoped `cancel_read` plus budgeted details/transcript/count/source ReadProgress. No method returns Unsupported merely because its implementation was left to another task. Registration happens only here, after all native functionality exists.

- [ ] **Step1: Add paging/resilience/visibility regressions.** A large multi-branch/nested family opens at the true latest recorded event, pages back through exact older identities and exposes recorded summaries/model_usage with scope/purpose rather than dropping historical work. Detail pages jointly obey event/byte caps. Append a torn JSONL line, complete it, truncate/replace a member and evict/cancel hydration; assert no duplicate counted request, source-generation correctness, and a healthy sibling remains selectable. Private fields/system prompts/control/credential-shaped values in metadata/source warnings never escape sanitization.
- [ ] **Step2: Run red and implement complete reader plus registration.** `cargo test --test omp_transcript --test omp_hydration --test omp_discovery`. Reuse shared cursor/line-window/source-generation patterns, return Pending each indexing slice, keep source bodies out of global caches, and stop cancelled hydration. Add the real scanner to lazy `build_readers`, checked roots and implemented capability help; reuse already parsed `--omp-sessions` and resolved roots. No alternate UI/worker or provider-specific rendering path.
- [ ] **Step3: Run green and extend shared safety.** Run the three OMP suites plus OMP filters in acceptance/read_only/privacy/perf/live suites. Add OMP root and decoys to existing World, retaining previous native fixtures/accounting. Demonstrate no accidental reads of history/models/stats/credential stores and no OMP provider writes. Test repeated family copies and same native ID at distinct sources under explicit continuation rules.
- [ ] **Step4: Smoke the actual application, then docs.** Launch synthetic OMP alongside native roots; expand children/grandchildren, select preview, inspect request/category prices, speed/context/effort/timing, compaction Unknown cost, source, latest/older transcript and live append. Exercise indexing cancel and an unsupported sibling without freezing input. If interactive terminal unavailable, use actual worker/event/render smoke and report the limitation. Update README/PRODUCT/ARCHITECTURE/help after proof; remove throwaway drivers. Master six-harness release verification remains required.

## Stage self-review

- O1–O3 cover native OMP metadata, owned accounting, active-path context, native descendants and complete reader/UI integration; no temporary adapter stubs, timestamp ownership heuristic or synthetic-request suppression is planned.
- Shared method/type names are exactly plans01/02. Unknown cost/timing/provenance is explicit, not a reduced parser pretending to support unavailable runtime facts.
- Full gates and final release claims belong to the master plan. Commands here are future instructions; no implementation or tests were performed while writing this plan.
