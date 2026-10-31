# AgentScope — richer session observability and harness coverage

**Date:** 2026-10-30  
**Status:** Approved by the user on 2026-10-30; implementation not started.
**Delivery size:** One coordinated feature release, delivered in independently verifiable stages.  
**Supersedes:** Earlier MVP exclusions only where this document explicitly adds passive local harness/desktop support. Read-only, privacy, and no-orchestration boundaries remain unchanged.

## 1. Intent and agreed scope

Make AgentScope useful for scanning recent local coding-agent work and understanding a selected session without immediately opening its transcript. Preserve facts, uncertainty, and provider-specific accounting while making the roster compact.

The user approved the two-line roster and right-hand preview, API-equivalent subscription pricing, timing and compaction views, metadata-first discovery, bundled official prices, and the delivery order below. The user delegated the desktop recommendation. This written design still requires approval before implementation planning.

### Goals

- Support Codex, Claude Code, OpenCode, OMP, Hermes, and native Pi session stores.
- Show subagents for Codex, Claude Code, OpenCode, OMP, and Hermes. Native Pi is included without extension-specific subagent discovery, an explicit user-approved scope decision.
- Include local Codex Desktop and Claude Desktop Code sessions; include accessible local Cowork Claude-format transcripts without accessing VM disks or cloud history.
- Show harness, recorded model provider, latest model and thinking level, lifetime session tokens, estimated API cost, active time when evidenced, and latest request speed in a readable roster.
- Show category-level token/cost breakdowns, start/end/elapsed/active/model timing, context usage/capacity, speed coverage, lineage, compaction, and recent activity in preview/details.
- Initially load 50 top-level session groups. Explicit Load more adds the next 50; children do not consume top-level slots.
- Open transcripts at the latest event, retaining access to older recorded history.

### Non-goals and anti-goals

- No launching, stopping, steering, reviving, approving, or modifying agents; none of OMP Hub's orchestration controls are copied.
- No network requests by AgentScope, telemetry, remote collectors, cloud conversation retrieval, VM mounting, provider hooks, or new persistent index database.
- No Pi extension adapters in this release. Forks must not masquerade as subagents.
- No claiming subscription-equivalent API value is subscription spend, missing costs are free, quiet agents are crashed, or an open session row proves liveness.
- No fabricated speed, active time, context occupancy, compaction cost, child attribution, or desktop origin.
- No global full-history search in this release: search/filter cover loaded groups and children, with that scope visible.

## 2. Approaches and decisions

### Pricing source

1. **Bundled, dated official price catalog — selected.** Offline and reproducible, with source URLs, checked dates, supported effective periods/tariffs, and explicit user overrides. It can age between AgentScope releases.
2. Read an installed OMP catalog. Rejected as the primary source: requires OMP and couples AgentScope to its internal catalog format.
3. Download prices at startup. Rejected: conflicts with the local-only/no-runtime-network boundary.

The user selected bundled prices. OMP is prior art for calculation semantics, not a runtime dependency.

### Discovery and loading

1. **Metadata-first discovery with on-demand detailed summaries — selected.** Enumerate cheap metadata and lineage, then hydrate the selected 50 groups and requested children. Large transcripts are not eagerly parsed merely to display a count.
2. Fully scan every session, then hide all but 50 rows. Rejected: simpler global search, but retains the current startup work problem and is not meaningful lazy loading.

The user selected metadata-first loading and accepted loaded-history-only search/filtering. Enumerating metadata for many candidates is allowed; fully reading all candidate transcripts is not a substitute.

### Pi and desktop scope

- **Native Pi — selected.** Standard sessions, transcripts, usage, effort changes, forks, and recorded compactions; no extension-specific child contract.
- Native Pi plus one extension was considered but rejected for this release because Pi has no canonical built-in subagent protocol.
- Skipping Pi was considered but rejected: native session inspection is useful without an extension.
- **Desktop recommendation, delegated by the user:** Codex Desktop plus Claude Desktop Code; readable local Cowork transcripts share the Claude parser. Chat/cloud-only and inaccessible VM-only history are outside the local coding-session boundary, not reconstructed from caches.

### UI and grouping

- **Two-line rows — selected.** Preserve titles while aligning provider below harness and thinking level below model.
- One-line dense rows were considered and rejected as the default because those facts move out of sight.
- **50 top-level groups with collapsible children — selected.** OMP Hub informed lineage and direct-cost separation, but has neither 50-group pagination nor a speed column. Those are AgentScope features.

## 3. Architecture and data contracts

Keep the existing Rust/Ratatui architecture: UI state and renderers are pure consumers; one background worker owns bounded provider reads and the process sampler. Extend the existing provider-reader boundary rather than introducing a new event bus, plugin system, async runtime, or daemon.

Separate three concepts currently conflated by `Provider`:

- **Harness:** Codex, Claude Code, OpenCode, Pi, OMP, Hermes; determines parsing/discovery semantics.
- **Model/billing provider:** recorded routing identity such as OpenAI, Anthropic, OpenCode Go, or OpenCode Zen; determines price lookup, not transcript parsing.
- **Origin surface:** CLI, Desktop Code, IDE, Cowork/local agent, other, or Unknown; provenance can be mixed across resumed sessions.

Preserve raw provider/model IDs alongside confirmed display names. Do not infer reasoning effort or origin from a model name. Separate raw identifiers from display-safe rendered strings.

The builder chooses concrete Rust type names and module splits. Required normalized facts are:

- Stable physical source identity; stable logical session identity; harness-native session/agent IDs; optional parent/fork/continuation edges with evidence.
- Origin observations, latest model/provider/effort, and provenance for each. Effective provider effort and configured effort may differ; details preserve both when recorded.
- Discovery metadata separate from hydrated usage/transcript data; loading, partial, unsupported, and unavailable states must not render as zero.
- Unique request usage, model/provider, recorded money, calculated API estimate and category contributions, optional request start/end/duration/TTFT, purpose, and deduplication identity.
- Session lifetime accounting separate from current-branch context; compaction observations separate from billed requests.
- Source-location references suitable for bounded source inspection, including SQLite row identity and compressed-source record identity.

No new exported contract is edited until references/callers have been inspected. Configuration changes use a clean cutover: update callers, examples, public docs, and tests; do not keep ambiguous aliases that continue pricing by harness rather than billing provider. Existing custom-price users need an actionable configuration error/migration explanation, not a silent reinterpretation.

### Configuration cutover (maintainer decision)

Keep TOML, existing discovery-root/CLI precedence, and existing non-pricing settings. New `[[pricing]]` overrides use `billing_provider` plus exact `model`; the old `provider = "codex" | "claude" | "opencode"` pricing field is rejected at startup with an actionable migration error. Explain that native Codex normally maps to an OpenAI reference and native Claude to an Anthropic reference, while an OpenCode entry requires the user's actual recorded route. Never auto-rewrite config or silently convert an ambiguous OpenCode price. Invalid overrides retain the existing fail-fast configuration behavior rather than disabling valid provider data invisibly.

No global configuration-version field is needed for this cutover; the renamed pricing key identifies the contract. `native_api_reference_estimates` defaults to true, may be disabled, and controls the explicitly labeled native-reference behavior in §6.1. Rename the old discovery-truncating `max_sessions` setting to `max_cached_sessions` (default 2,000 hydrated summaries); reject the obsolete key with an explanation. Lightweight metadata has separate explicit limits below, so a summary-cache eviction does not erase a session from history or silently end pagination.


## 4. Roster, preview, and details

### 4.1 Two-line roster

Default columns at sufficient width:

| Column | First line | Second line |
|---|---|---|
| Selection/state | Selection/expand marker and evidenced state | Tree continuation where needed |
| Session | Title or bounded first prompt | Project and concise activity/recency |
| Harness | Harness name | Recorded model provider, or Unknown |
| Model | Latest model | Latest recorded thinking level |
| Tokens | Lifetime direct-session token total | Partial marker when applicable |
| API cost | Estimated/retained API-equivalent USD | Partial/reference-price marker when applicable |
| Active | Evidenced active duration, or — | Only a short qualifier if necessary |
| Speed | Latest completed timed request, output tok/s | Latest/partial qualifier where needed |

Start/end dates and full timing are in preview/details; they must not crowd the roster. Preserve precise values in details while abbreviating roster counts. Missing effort is Unknown, not `none`/`off`. If the model changed after the latest timed request, details identify that request's model; do not label an old model's rate as the new model's speed.

Minimum supported terminal remains 60 columns by 20 rows; smaller terminals show the existing resize message. Initial layout contract: at 150 columns and above, reserve 45 columns for a scrollable preview and the remainder for the roster; below 150, Tab swaps roster/preview. Pin selection/tree markers and the session-title column; horizontal movement scrolls the remaining columns as whole columns, preserving harness/provider and model/effort pairs. Keep two physical lines per row and independently scroll details vertically. Reserve footer/Load more access even at minimum height. Verify 60×20, 90×25, and 160×40 plus resize transitions. Exact cosmetic widths may be tuned without making required facts unreachable.

Keep keyboard operation primary. Use roster Left/Right for column scrolling and a distinct child expansion action (Space); Enter opens a session or activates a focused Load more row. Load more must also be clickable. The current terminal loop does not capture mouse input, so this release adds opt-out mouse capture and hit-testing for read-only roster selection/expansion/Load more, restored on exit along with other terminal state. A user who disables mouse still has complete keyboard access. No mouse action enables agent control.

### 4.2 Fast preview

Selection changes update the preview without entering the transcript. Show cached metadata immediately, then queue bounded hydration for that selection. Stale responses must not overwrite the preview of a newer selection.

Sections: identity/state/task, harness/origin/provider/model/effort, usage with category costs, latest context/capacity, timing, speed, compaction summary, lineage, recent activity. Preview recent activity is bounded; full content is in transcript/details. Hydration is not allowed to block input or parse a full large file synchronously on each arrow key.

### 4.3 Details

Details provide full token counts and USD contributions, price sources/dates/qualifiers, timing evidence, eligible-request speed history and coverage, compaction history and expandable summaries, direct versus child usage, and source provenance. Preserve existing source/diagnostic views and redaction controls.

Synthetic browser studies under the private `.superpowers/brainstorm/` directory establish visual direction only. Do not commit their server state, session keys, private metadata, or synthetic values as real provider fixtures. Product behavior is not implemented merely because a mockup interaction works.

## 5. Discovery, grouping, and pagination

### 5.1 Metadata-first loading

1. Enumerate allowlisted session roots and database session metadata. File metadata plus bounded header/tail probes may be needed for parentage, identity, recorded activity, and titles. Do not broad-scan unrelated application data.
2. Build lightweight logical-session/group records. Child and continuation metadata can be inspected without parsing all of their message bodies.
3. Order top-level groups by latest known activity across the root and its descendants. Prefer cheap recorded event/activity timestamps; a filesystem-mtime fallback must remain visibly approximate in details. Resolve ties by stable source/session key. This is exact latest-activity ordering only where the metadata supports it, not an assertion that mtime equals provider activity.
4. Hydrate the first 50 groups. Child full summaries/transcripts are loaded when expanded or selected, not indiscriminately across all history. Preview hydration can finish independently of list metadata.
5. Load more adds the next 50 top-level groups to the loaded set; fewer at EOF. Keep loaded children attached and do not duplicate a child as an unrelated top-level row.

A discovery pass may inspect metadata for all candidate sources to find the global latest groups. It must not claim that directory enumeration is constant-time, nor that sparse metadata proves an exact global total. Display discovered/loading counts and `more available` when the count is not complete.

Metadata probes default to a 4 KiB prefix plus at most a 32 KiB tail for uncompressed files. SQLite may use allowlisted activity/lineage columns directly. Compressed files use bounded header probing and source mtime until explicitly indexed; do not decompress every archive to rank it. If a probe cannot establish a fact within its budget, retain Unknown/approximate metadata. The first usable page may be provisional while metadata discovery continues, visibly labeled as such; declaring the initial discovery generation complete freezes its ranked page universe. Additional or corrected ordering/parentage found later produces a refresh-available indication, not a silent row move.

Native grouping metadata comes from known paths/headers/sidecars or SQLite fields, not from loading every tool body. In OpenCode, tool metadata supplies a transcript jump, not the primary parent relationship (`parent_id`); Hermes `_delegate_from` is extracted from the session row's JSON; OMP/Claude artifacts and Codex session headers supply their native edges. If only full-history hydration reveals a new relationship, retain the current snapshot's orphan slot and show its newly discovered relation until explicit refresh; merge/repage on that refresh. No family rollup is called complete while an unresolved edge remains.

### 5.2 Stable navigation

A paging generation has a stable ordered group-key snapshot. Live content/state updates may update rows but must not silently reshuffle the selection or insert groups into already consumed pages. Advertise newly discovered work and use explicit refresh to rebuild order. Refresh preserves the selected identity and loaded top-level capacity where possible; removed selection moves to the nearest valid row. Load more against a refreshed snapshot cannot duplicate/skip groups through stale cursors.

Sources created after a generation freezes wait for explicit refresh; deleted sources remain marked Unavailable in that generation rather than silently changing its slot count. Refresh then removes or adds them and retains selection by identity where possible. A source with no recorded activity timestamp ranks by labeled approximate mtime; if even that is unavailable, rank it after timestamped sources with stable source-key ties.

Search/filter operate on loaded groups and available child metadata, explicitly labeled `loaded sessions`. A child match keeps its parent visible as context. Matching fewer than 50 does not trigger hidden automatic full-history scans; Load more remains available. Sort applies to loaded groups without detaching children.

### 5.3 Relationship safety

- Distinguish subagent, fork/branch, and compression continuation. Only continuations form one logical conversation; forks remain distinct top-level work unless they are explicitly subagents.
- A missing/ambiguous parent yields a visible unlinked/orphan session with the unresolved evidence, not a dropped record or an invented edge. Such an orphan may occupy a top-level slot.
- Reject cycles for display/link traversal with a visited set and bounded depth; retain inspectable source evidence and a warning. Do not hide valid unrelated sessions.
- The same physical source discovered through multiple configured/desktop roots must appear once. Canonical source/session identity is not guessed from title, cwd, or timestamps.
- Child activity may make its group recent, but displayed parent usage remains the parent's direct usage. A labeled `including N subagents` rollup is optional in details and must deduplicate physical requests and logical compression segments.

### 5.4 Initial resource defaults (maintainer decisions)

Use the existing per-worker-call ceiling of 256 records/rows, 8 MiB bytes, or 100 ms, whichever is reached first, for resumable discovery/hydration/indexing work. Decompression work counts expanded bytes and records, not just compressed bytes. Cancellation/selection changes are checked between work slices. All transcript opens and indexing are asynchronous worker operations regardless of file size; show progress immediately when the tail is not yet indexed, and permit Escape/selection change to cancel. EOF completes a pass, while a work-slice ceiling schedules continuation; neither a large valid archive nor a single slow source is silently truncated into a successful empty transcript.

Retain the existing 1 MiB record limit, 500-event/256 KiB page limits, 20,000 loaded-event window, 16 pending loads, and 100,000-entry/8 MiB exact dedup ledger unless a measured implementation need justifies an explicit documented change. Load child metadata in batches of 50; a separate child Load more continues the same family. Bound detailed summary caching to `max_cached_sessions`; evicted offscreen summaries can be rehydrated without removing their lightweight group rows.

Lightweight metadata may scale with discovered source count within explicit configurable limits: initially 100,000 source records and 64 MiB of metadata, whichever is reached first. This is the only intentionally history-proportional in-memory index; transcript bodies and expanded archives may not be retained proportionally. At a metadata ceiling show `Discovery limited` with the limit and how to raise it, never EOF or a complete total; all already discovered sessions remain usable. Acceptance of complete global ordering/pagination applies within the configured discovery envelope. Use synthetic low limits to prove interruption, partial counts, cache eviction/rehydration, and no accidental history truncation.


## 6. Official pricing and token accounting

### 6.1 Catalog and price identity

Initial built-in price families: Anthropic/Claude API, OpenAI API for Codex/ChatGPT-backed models, OpenCode Go, OpenCode Zen. These prices can apply across harnesses when the recorded routing identity matches. Other recorded providers are displayed but remain unpriced unless configured.

The first bundled catalog covers all text/coding model rows with published token prices in the four official pricing tables linked in §12 at implementation time, including documented free rows and explicitly dated promotions; it is not a handful of demo models. Preserve an inventory manifest so coverage can be checked against those reviewed tables. Unsupported non-token modalities and unlisted historical/custom IDs remain explicitly unpriced rather than guessed. The implementer retrieves the official tables during development, pins their checked date/source references, and stores reviewed real values as source data; no network call enters AgentScope runtime.

Use a bundled JSON artifact with an explicit catalog schema version, catalog revision/check date, per-card stable provider/model identity, official source URL, and checked/effective dates. This reuses the existing JSON parser dependency. Include explicit alias mappings only where evidenced, input/output/cache-read/cache-write rates, separate 5m/1h cache writes where applicable, context-length tiers, and evidenced service-tier/effective-time modifiers. Maintainers refresh it through reviewed source updates. Exact user provider+model overrides replace the relevant card with user-config provenance; missing optional override categories remain unknown, not silently inherited prices.

Unknown IDs, unconfirmed provider aliases, and unsupported accounting combinations remain Unknown/partial. For native Codex/Claude records missing route identity, `native_api_reference_estimates = true` enables a native API reference estimate using the confirmed model identity, labeled `reference API estimate`; it must not fabricate a recorded provider. For OpenCode and other routed harnesses, do not silently choose first-party prices merely from a model ID.

### 6.2 Money semantics

- Subscriptions use API-equivalent value, not a share of the monthly fee and not an inferred bill. Main labels use `Estimated API cost`/`~USD`.
- Preserve trustworthy recorded API/catalog monetary costs and their source; do not reprice them on model switch or reopening. Provider-reported billed USD, saved catalog estimates, and newly calculated estimates are distinct facts.
- A subscription's reported zero charge is not zero API-equivalent usage. Calculate the requested equivalent value when a rate card and usage exist; retain actual charge separately.
- Price requests individually at their recorded provider/model and known applicable tier. Do not apply the session's last model/rate to lifetime tokens. Handle cumulative snapshots and streamed duplicates according to the harness, never by blind summation.
- Use evidenced request time for effective-date or time-of-day tariffs. If the required timestamp is missing, that time-sensitive component remains unpriced.
- When historical official rates are unavailable, a current-card estimate is permitted only with an explicit `at catalog-date rates` qualifier. Never pretend the checked date proves a historical effective date.
- Include service-tier/region modifiers only when recorded and supported. Unsupported known modifiers yield partial/unknown estimates rather than silently using standard rates.
- Preserve server/tool fees when trustworthy reported money includes them; otherwise explicitly state excluded fees. Do not invent web-search or tool fees from token counts.
- Show a known subtotal and count/coverage of unpriced requests/categories when incomplete. Never label it a complete total or substitute zero for an unknown.

### 6.3 Category presentation

Each available category displays `tokens` and `~USD`: uncached input, cached input/cache reads, output, and cache writes with TTL split where recorded. Show the applicable rate card in details. Zero is displayed only when evidenced; distinguish tiny nonzero estimates from rounding to free (for example `<$0.0001`).

Provider inclusion/subset rules govern totals. Reasoning is often included in output and must not be added or billed twice. Cached input may already be a subset of input for one API but additive for another. Normalize those semantics before rendering or pricing.

Retain all unique executed requests for lifetime accounting, including recorded off-transcript auxiliary requests and executed branches. Do not confuse branch-local context with session lifetime spend. Exact request IDs/owned-record boundaries prevent inherited history from becoming new child usage.

## 7. Timing, speed, and context

### 7.1 Timing

- **Started:** recorded session start, with a labeled derived/approximate fallback if only source evidence exists.
- **Ended:** last valid explicit session-end event after any subsequent reopen/resume; turn completion or last modification alone is not session end.
- **Elapsed:** start to explicit end, or `since start` when no end is evidenced. Also expose last-recorded activity; do not present time since last activity as work time.
- **Active:** union of supported recorded turn-start/turn-end intervals, excluding user gaps. Include tool/provider waits within an active turn. Mark incomplete coverage; do not extend an unclosed historical interval indefinitely.
- **Model request time:** separately labeled sum of eligible request durations. It is not whole-agent active time and is not the parent/child wall duration.

OMP's live Hub `active` timer is not a disk-backed metric that AgentScope can read. Pi lacks a standard explicit session-end marker/request duration. Hermes `ended_at` proves close but `agent_close` does not by itself prove successful work. An open row never proves a live process.

### 7.2 Speed

Use OMP's whole-request convention: output tokens divided by recorded request duration, including TTFT. This avoids inflated decode-only rates when hidden reasoning precedes visible output.

- Roster: latest completed eligible conversation request, one decimal `tok/s`; no invented continuously live rate.
- Preview: same latest rate, TTFT and request duration where recorded, duration-weighted measured average, and coverage.
- Details: per-request provider/model/effort, output, duration, TTFT, and rate; aggregation by measured requests with raw numerator/denominator available.
- Average = sum of eligible output / sum of eligible request seconds, not an unweighted mean of rates. Mixed models remain identifiable; details may group by model without pretending one rate describes a single unchanged model.
- Eligible durations are finite, positive, >100 ms, and consistent with the request; output must be recorded and positive. Invalid/missing durations do not produce zero/infinite rates. Coverage explicitly counts measured versus untimed requests.
- Add reasoning only when the provider's output field excludes it and the normalized numerator explicitly says so; never add an output subset twice.
- Exclude compaction/auxiliary calls from the default conversation speed and expose purpose separately when timing exists.
- Pi/Hermes and older records without request timing show Unknown; do not divide tokens by session age or adjacent-message timestamp gaps.

### 7.3 Context

Show latest active-branch prompt occupancy versus the model's applicable capacity, not lifetime tokens or cumulative output-inclusive counters.

For OMP, prefer `contextSnapshot.promptTokens`, applying recorded `historyRewriteTokensRemoved` as its source semantics require; otherwise use explicit `usage.contextTokens` or provider-specific additive prompt categories. For Pi, latest assistant prompt-category usage is a labeled estimate/snapshot, not present live context. Invalidate stale pre-compaction occupancy when no post-compaction value exists; recorded after-compaction estimates may be shown as estimates.

Prefer recorded effective context capacity; otherwise use a confirmed model/provider catalog limit labeled catalog-derived. A custom, extended-context, or harness-specific budget cannot be replaced silently by the model's advertised maximum. Unknown capacity/occupancy stays Unknown and suppresses percentage bars. Hermes lifetime totals and messages.token_count are not sufficient evidence of current context.

## 8. Compaction

Preview shows recorded count, latest event/time/method, context before/after when available, and known attributable cost with incomplete coverage. Details list expandable events and bounded sanitized summaries, trigger, model/provider, duration, context reduction, token categories, estimate/provenance, and warnings only when recorded or safely derived.

Important distinctions:

- Before/after context reduction is not compaction request token usage, a refund, or a measured future-dollar saving.
- Costs already represented in request/session usage are a breakdown, not a second charge.
- Missing attributable usage is Unknown, not free. No nearby-call/time-window heuristic creates a compaction cost.
- OMP CompactionEntry has metadata but no verified persisted summarization usage producer was found; **OMP compaction cost is unavailable unless another concrete source is verified**. Generic `model_usage.purpose` schema support is not proof of a writer.
- Pi can persist compaction/branch-summary usage directly. Hermes `session_model_usage` can attribute auxiliary compression usage by task, but this may be an aggregate across events; show that scope honestly and do not invent a per-event split.
- Claude/Codex/OpenCode expose whatever their actual markers/usage support; do not assume field parity.
- Local-only methods may show zero additional model API usage only when the recorded method proves no model call. Later model consumption of retained/archive context is not silently free.
- `No compaction observed` applies only to the inspected history. Partial/unhydrated history is labeled, not presented as a verified lifetime count of zero.

## 9. Harness-specific ingestion

### 9.1 Claude Code and Claude Desktop

- Top-level Claude-format sessions are known project-root session JSONLs. Child discovery is restricted to `subagents/**/agent-*.jsonl`, including evidenced workflow nesting; do not ingest workflow `journal.jsonl` or tool-result files as sessions.
- Child records may carry `sessionId` equal to the parent's. Use explicit `agentId`/source identity for the child; never overwrite it with the parent ID.
- Parent relation comes from the session directory/confirmed metadata; nested `parentAgentId` and tool-use links are used only when present. `parentUuid` is a message UUID, not a session ID.
- Preserve independent child usage, deduplicate streamed assistant IDs within a source, and exclude inherited shared request IDs from a family rollup. Do not erase distinct child calls just because they share a prompt.
- Code-tab Desktop sessions reuse the ordinary Claude project store. Record per-record `entrypoint` evidence where supported; resumed origins can be mixed. Desktop manifests may enrich an existing transcript via a confirmed `cliSessionId`, never create a duplicate transcript.
- Explicitly allowlisted local Cowork/native Claude-format project roots under known Desktop data locations may be discovered. Report absent/inaccessible/unsupported roots without VM mounting or cloud retrieval. Directory layout/source provenance remains visible when exact origin is uncertain.
- Windows roots include confirmed Claude-Data installation directories, legacy Claude application-data locations where documented, and explicit user overrides. macOS documented equivalents are supported by fixture-backed path logic; do not claim real-platform validation unless exercised. No broad home/AppData crawl.
- Desktop release/delete markers are metadata, not source deletion. Mark a retained released session as such and put it behind an explicit include-released filter, following the desktop user's removal intent without deleting anything. If marker semantics cannot be confirmed, show raw known evidence rather than guess lifecycle completion.
- No cloud Chat transcript reconstruction from IndexedDB, audit logs, authentication, or Electron caches.

### 9.2 Codex CLI, Desktop, and IDE

- Discover normal and archived rollout roots under the configured CODEX_HOME. Preserve custom root overrides; compressed `.jsonl.zst` is supported using bounded streaming decompression. This is a justified new dependency for real recorded history, not a convenience parser.
- Each child thread is a distinct rollout; use confirmed `parent_thread_id`/subagent source metadata. Distinguish review/other auxiliary thread roles, forks, and explicit root/immediate-parent identifiers; never promote a root-thread ID to an immediate parent without evidence.
- Respect `subagent_history_start_ordinal` and version-supported child-owned usage boundaries. Older copied cumulative counters must not become child spend. If no safe inherited baseline can be established, mark child usage partial/unknown rather than include parent tokens again.
- Desktop shares Codex local stores. Originator/source combinations can identify Desktop/IDE on supported versions, but ambiguous bundled CLI usage remains Unknown/mixed. WSL stores are not automatically crawled or remotely accessed; users can configure an accessible explicit root.
- Rollout compression/archival can replace a physical source. Preserve logical identity and invalidate affected pages without duplicating compressed and uncompressed copies. Record compressed-source locators; do not pretend uncompressed byte offsets seek directly inside zstd.
- Stable compressed archives are not decompressed repeatedly on every refresh; cache bounded metadata/index state in memory. An explicit archive transcript open may require a bounded streaming indexing pass with progress/cancellation, never a blocked UI or unbounded expanded allocation.
- Use native rollouts as authoritative transcripts. Do not require a derived state SQLite database to support Desktop.

### 9.3 OpenCode

- Support existing detected SQLite shapes with `parent_id` child sessions and `fork_session_id` forks. Current v2 forks are roots with reset usage; legacy differences are schema-detected, not assumed.
- Show recorded model provider from model/message metadata; preserve providerID and variant. Exact supported variants become thinking level only where format semantics establish that meaning.
- Child usage is stored on child rows, not rolled into parent session totals. Database-level totals and per-message derivations remain alternatives, never additive copies.
- Read agent identity and confirmed child-session links from allowlisted columns/tool metadata (`subagent`/`sessionID`, legacy `task`/`sessionId`) where available. Missing links do not justify timestamp/prompt matching.

### 9.4 OMP

- Discover session JSONLs plus known artifact-tree child JSONLs; handle title slots, session headers, model/thinking changes, assistant messages, model_usage, compaction, session_init, and explicit session_exit when present.
- Read the session tree/active-leaf semantics correctly; keep request identities and raw branch history. Child parent links use documented artifacts/header/init evidence, not arbitrary directory coincidence.
- Parent task-result cost can already include descendants. Default direct parent cost excludes recorded child-rollup usage; do not add child files to that total a second time. OMP's `directCost = cost - subagentCost` is useful prior art, but the adapter must verify which raw records supply those values.
- Lifetime usage includes unique executed assistant/model_usage calls, with parent-side task-result usage treated as child accounting, not an extra parent API call.
- Persisted duration/TTFT/completedAt support historical request speed. The runtime Hub registry, live token meter, and executor timers are not required/read by AgentScope.
- Prefer persisted context snapshots over Hub's output-inclusive progress token field. Compaction cost stays Unknown without attributable persisted usage.

### 9.5 Hermes and its subagents

- Read native `state.db` session/message/usage metadata with schema detection and an allowlisted read-only connection. Respect documented HERMES_HOME/profile/platform roots and explicit overrides, not a hard-coded Unix-only path.
- Identify an explicit delegate edge when `_delegate_from` matches `parent_session_id`; a retained legacy orphan marker does not justify a fabricated parent. A `source='subagent'` fallback must be version/format-evidenced and labeled when incomplete.
- Classification must bind `_delegate_from`, `_branched_from`, and `_reset_from` to the actual edge. Compression continuations inherit model_config; testing merely for presence of `_delegate_from` misclassifies continued children.
- Merge explicit compression segments into one logical conversation; attach children whose delegate parent belongs to any segment of the parent's chain. Apply the same rule to compressed/nested subagents. Branches/resets remain distinct conversations.
- Child messages live under the child's session IDs. Parent tool-result JSON may report child cost but has no verified direct child-session ID; do not infer a precise tool-call-to-child link from goal text or timing.
- Persisted child/session costs are direct; the parent's in-memory child-cost rollup is not the stored parent row. Prefer a single deduplicated per-model/per-task ledger when complete; otherwise session totals plus only separately evidenced excluded auxiliary usage. Never sum the session aggregate and its duplicate per-model main usage.
- Auxiliary compression/title/etc. usage may exist only in session_model_usage. Include known auxiliary cost once; expose its purpose and incomplete category/timing coverage. Do not claim a session-row-only estimate is complete when known auxiliary calls are omitted.
- Effort can be obtained from allowlisted `model_config.reasoning_config` fields where present; it is persisted configuration, not necessarily a per-request historical effort timeline.
- `ended_at`/`agent_close` signals close, not automatically success; absence of close is not liveness. Use explicit terminal message evidence conservatively. Do not invent live task labels/batch IDs not persisted by Hermes.

### 9.6 Native Pi

- Support standard JSONL session/entry tree records, assistant provider/model/usage/cost, model_change/thinking_level_change, forks, compaction/branch_summary usage, and unknown records safely.
- No extension-specific child discovery or claiming forks are subagents. Display `Pi extension subagents: unsupported in this release` in capability/help context, not a misleading zero-child guarantee.
- Standard assistant records inspected lack request duration/TTFT; standard history lacks explicit session exit. Speed/end/active fields remain Unknown unless a supported stronger field exists in a detected version.
- Context derives from latest branch prompt usage with an explicit snapshot/estimate label and applicable known capacity; not lifetime total tokens.

## 10. Read-only, privacy, and resource bounds

- Never execute provider code/configuration to load data or prices. Never open auth files, credentials, message-provider APIs, or untrusted links embedded in sessions.
- Read only allowlisted transcript/metadata files and SQLite tables/columns. Extract only required metadata fields; model_config/desktop manifests may contain secrets and must not be dumped or broadly rendered.
- Extend the existing SQLite read-only authorizer/query-only/WAL handling to Hermes. The documented ADR-0002 SQLite runtime-sidecar exception applies; do not claim byte-identical live provider directories or silently create a separate copy/repair path.
- Sanitize terminal controls and mask credentials for new fields, titles, summaries, agent labels, paths, recent activity, warnings, errors, and source views. Do not log raw provider content or persist real transcripts as fixtures.
- Enforce the resource contracts in §5.4 for discovery, hydration, SQL rows/text, JSONL records, nesting, loaded summaries/events, decompression bytes/work, and pending requests. Budget exhaustion is resumable or explicitly limited, never silent success. Only the explicitly bounded lightweight metadata index may scale with discovered history; transcript bodies may not.
- Bound child expansion independently of the 50-root page budget; large families get a child-specific continuation action/progress indication. This must not silently omit descendants or turn one expansion into an unbounded read.
- Appending, partial trailing lines, truncation, compressed-source replacement, WAL updates, malformed metadata/JSON, and unsupported schemas remain isolated to the affected source. Retain valid unrelated sessions.

## 11. Delivery order and verification

No implementation starts until this design is approved and writing-plans has produced the implementation plan.

1. Pricing/domain/accounting contracts, metadata-first discovery/paging boundary, and timing/speed/context/compaction facts. Establish compressed-source identity/locators and streaming zstd support here because archived sources must use the same reader contract. A maintained streaming zstd decoder dependency is approved for this requirement; prefer an existing stable crate and safe caller API, with no provider-file writes or extraction. Do not invent a pure-Rust/static-only constraint absent from the repository.
2. Two-line roster, preview/details, grouping/Load more, and newest-event transcript entry; update existing Codex/Claude/OpenCode ingestion for those contracts and native children, including archived/compressed Codex records. Integrate Codex Desktop and Claude Desktop Code identification and allowlisted local Cowork roots with these adapters, not as a later incompatible parser.
3. OMP sessions and nested subagents.
4. Hermes logical compression chains and nested subagents.
5. Native Pi without extension-specific children.

Stages are not permission to silently ship a subset as the completed requested release. The full agreed scope is the deliverable. The exact dependency-aware implementation sequence and module/API details are left to writing-plans.

### Acceptance scenarios

1. Starting with more than 100 top-level groups across harnesses within the configured metadata envelope shows the newest 50 discovered groups by the declared metadata ordering; each Load more adds up to 50 distinct older groups until actual EOF. Children/continuations do not consume slots. Provisional discovery, approximate activity keys, and metadata ceilings are visible; hydration cannot silently alter a frozen generation.
2. Large/unrelated transcripts are not fully parsed at startup to implement fake UI-only pagination. Selecting/expanding hydration and cancellation obey budgets; stale responses cannot replace a newer preview.
3. Approved two-line alignment holds for harness/provider and model/effort. Tokens, API cost, active time, speed, and expandable children remain accessible at 60×20, 90×25, and 160×40, including narrow preview toggle, horizontal column scrolling, resize, keyboard activation and mouse-disabled operation.
4. Native children appear under the correct Claude, Codex, OpenCode, OMP, and Hermes parents, including nested and compressed child chains. Missing parents, cycles, reused IDs, copied prefixes, and corrupt children do not hide healthy roots or create guessed relationships.
5. OMP parent-side child totals, Claude shared assistant IDs, Codex inherited cumulative snapshots, OpenCode reset forks, and Hermes per-model/aux duplicates never produce double-counted rollups. Direct versus inclusive usage is labeled.
6. Exact official provider/model prices work without user config for documented initial catalog entries. Subscription usage shows API-equivalent estimates. Model switches, caches/TTL, reasoning subsets, tier boundaries, applicable timestamps, tiny costs, unknown IDs, overrides, and partial coverage obey the above rules.
7. Details show each token category and its dollar contribution, source/date and exclusions; displayed totals reconcile within formatting precision without charging subsets twice.
8. Latest recorded thinking level remains distinct from Unknown/none, reflects changes, and is associated with the correct model/branch. Provider IDs and ambiguous desktop origins are not invented.
9. Start/end/elapsed/active/model time are distinguished; turn completion is not session termination, resumed sessions reopen, and an unclosed row does not prove a live agent.
10. Request speed uses measured whole-request duration, excludes invalid/instant/missing timings, reports partial coverage and weighted mean correctly, and never uses session-age or message-gap fallbacks. New-model rows cannot mislabel an old-model measurement.
11. Context uses prompt occupancy, not lifetime total/output, respects compaction boundaries and known effective capacity, and labels stale/estimated/unknown values correctly.
12. Compaction details show observed metadata and attributable usage only, preserve unknown costs, distinguish context reduction from billed tokens, and include auxiliary charges once. OMP absent cost evidence cannot become a synthetic reported number.
13. Opening a transcript selects its latest available event without first loading the whole file. Older history remains accessible; manual historical reading is not forced to the end by live updates.
14. Native Pi sessions have complete supported transcripts/accounting and explicit capability limits, without fake native children, speed, or end markers.
15. Codex Desktop/CLI shared stores, archived and zstd rollouts, Claude Desktop Code shared stores and local Cowork roots are discovered once, labeled from evidence, and handled without reading app credential/cache stores. Inaccessible/remote-only history is clearly unavailable, not fabricated.
16. All new paths tolerate malformed records, partial writes, unsupported shapes, SQL errors, source replacement, and privacy sentinels without panics, leakage, provider writes, or loss of unrelated sessions.
17. Search/filter explicitly cover loaded groups; child matches preserve parent context. Refresh/order/page-generation changes preserve selection and cannot duplicate/skip records through stale responses.
18. Runtime remains offline/read-only apart from the documented SQLite sidecar behavior. Native source inventories, content hashes where appropriate, and sanitized diagnostics prove that boundary on exercised synthetic scenarios.

Use deterministic synthetic fixtures for permanent behavior/regression tests, including malformed/unknown data, privacy, boundaries, and transitions. Exercise the actual application with synthetic stores and inspect rendered TUI/keyboard/mouse behavior; tests alone are not proof of the changed UI. If interactive terminal access is unavailable, use the real app state/rendering/event pipeline in a throwaway smoke harness and explicitly report the visual/terminal limitation. Do not claim native desktop-app/OS validation from path fixtures or public source alone.

Run applicable repository gates once after coherent implementation/integration: `cargo fmt --all -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and an exercised changed-path smoke run. Update README, PRODUCT, ARCHITECTURE, CLI/help/config examples, and capability/limitation documentation to verified behavior; remove temporary smoke scaffolds. Do not commit/push/publish without an explicit delivery request.

## 12. Evidence and remaining builder work

These are format observations, not promises that every installed version records every optional field. Pin fixture/format references during implementation and validate the actual supported schema before pricing/grouping. No real private transcripts are part of this design.

Primary pricing sources:

- [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing)
- [OpenAI pricing](https://developers.openai.com/api/docs/pricing) and [GPT-6.1 Sol metadata/pricing](https://developers.openai.com/api/docs/models/gpt-6.1-sol)
- [OpenCode Go](https://opencode.ai/docs/go/) and [OpenCode Zen](https://opencode.ai/docs/zen/)

Harness/source references:

- [OMP Hub](https://github.com/can1357/oh-my-pi/blob/main/docs/agent-hub.md), [session format](https://github.com/can1357/oh-my-pi/blob/main/docs/session.md), [compaction](https://github.com/can1357/oh-my-pi/blob/main/docs/compaction.md), [pricing](https://github.com/can1357/oh-my-pi/blob/main/docs/models.md).
- [OMP whole-request speed](https://github.com/can1357/oh-my-pi/blob/6a86df8539fccf3ad7818081266e554022fc21bc/packages/tui/src/overlays/usage-row.ts#L108-L118); [direct-cost/tree projection](https://github.com/can1357/oh-my-pi/blob/6a86df8539fccf3ad7818081266e554022fc21bc/packages/tui/src/overlays/agent-hub-projection.ts).
- [Pi session format](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/session-format.md), [message types](https://github.com/earendil-works/pi/blob/main/packages/ai/src/types.ts).
- [Hermes delegate producer](https://github.com/NousResearch/hermes-agent/blob/main/tools/delegate_tool.py#L293-L346), [delegate result rollup](https://github.com/NousResearch/hermes-agent/blob/main/tools/delegate_tool_results.py#L399-L412), [usage persistence](https://github.com/NousResearch/hermes-agent/blob/main/agent/turn_usage.py#L230-L282), [session classification](https://github.com/NousResearch/hermes-agent/blob/main/hermes_state_sessions.py), [storage](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/developer-guide/session-storage.md).
- [Claude data directory](https://code.claude.com/docs/en/claude-directory), [subagents](https://code.claude.com/docs/en/sub-agents), [Desktop storage](https://claude.com/docs/third-party/claude-desktop/data-storage). Optional child metadata/desktop origin fields also have third-party format observations; validate fixtures and treat absent/unstable fields conservatively.
- [Codex rollout storage](https://github.com/openai/codex/blob/main/codex-rs/rollout/src/recorder.rs), [compression](https://github.com/openai/codex/blob/main/codex-rs/rollout/src/compression.rs), native protocol/session source definitions.
- [OpenCode v2 projector](https://github.com/anomalyco/opencode/blob/dev/packages/core/src/session/projector.ts), [subagent tool](https://github.com/anomalyco/opencode/blob/dev/packages/core/src/tool/plugin/subagent.ts), with legacy task-tool variants kept as independently detected source shapes.

Builder decisions explicitly delegated: Rust names/module organization, minimal catalog maintenance tooling, concrete bounded cache/index implementation, cosmetic responsive widths within the specified interaction contract, optional formatter precision beyond truthful small-number handling, and version-detection mechanics. These decisions cannot weaken the acceptance scenarios or replace unknowns with invented values. For Pi/OMP capacity, use the same official-provider/model metadata bundled with the catalog where an applicable effective window is known; otherwise capacity is Unknown, not an OMP/Pi catalog/config dependency. Claude released sessions default to hidden until include-released is enabled; unconfirmed release-marker shapes are diagnostic evidence rather than a guessed release filter.
