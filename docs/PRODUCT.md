# AgentScope — Product Requirements & Design

**Version:** 0.3<br>
**Date:** 1 November 2026<br>
**Status:** Requirements document; v1 implemented and verified on synthetic fixtures across six harnesses, partially validated on real data (see [Implementation status](#implementation-status))<br>
**Product category:** Local-first developer observability tool<br>
**Initial interface:** Terminal User Interface (TUI)

## Implementation status

*Updated 2026-11-01. The sections below remain the product requirements; this section records what was actually verified.*

| Area | Status |
|---|---|
| Six harness readers, roster, transcript explorer, usage, status, process candidates, TUI | Implemented for Codex, Claude Code, OpenCode, OMP (oh-my-pi), Hermes, and native Pi. AC-01..AC-10 pass on synthetic fixtures through the real adapters, worker thread, and keyboard path (`tests/acceptance.rs`). |
| Native subagents | Implemented for five harnesses: Codex, Claude Code, OpenCode, OMP, and Hermes. Pi records no native child session, so none is listed. Child/grandchild, fork, continuation, orphan and cycle cases are covered by `native_children`, the adapter suites, and the integrated world. |
| Local desktop and archives | Claude Desktop Code/Cowork manifests and Codex `archived_sessions`/`.jsonl.zst` are read locally; released sessions are hidden by default. Chat history, credentials, renderer caches, VM bundles, and cloud-only or WSL-contained stores are not read. |
| Money | Offline catalog revision `2026-10-30.2` (schema version 1, checked 2026-10-30) plus exact user overrides. Every amount is an API-equivalent estimate; a reported amount must be `ReportedApi`, a saved estimate must have a verified source, and unknown routes/tiers/tariffs/TTL/reasoning rates stay `Unknown`. |
| Measured facts | Per-harness coverage is explicit: turn timing and context for Codex/Claude Code; recorded model time, request speed, active-branch context for OMP/Pi; compaction markers for Codex/Claude/OMP/Pi plus Hermes event/aggregate compaction. Hermes active time, speed, and context stay Unknown. See [Architecture §5](ARCHITECTURE.md#5-state-and-measurement-semantics). |
| Bounded reads and paging | One worker slice stops at 256 records, 8 MiB, or 100 ms; metadata probes are 4 KiB head plus 32 KiB tail; root/child pages are 50; record/page/loaded-window/ledger caps are configurable and visible. Cancellation is job-scoped. |
| Read-only (§10, AC-08) | Verified on fixtures: session files, the main databases, mtimes, and permissions (Unix mode bits; on Windows only the read-only attribute, not ACLs) unchanged; OpenCode and Hermes SQLite run behind fixed table/column allowlists; credential tables denied; `auth.json`/credential files never discovered (`tests/read_only.rs`). **Exception:** SQLite may create/update `-shm`/`-wal` sidecars ([ADR-0002](decisions/0002-opencode-readonly-wal-sidecars.md)); the claim is "no writes to session content, databases, configuration, permissions, or credentials", not "byte-identical directory". |
| Privacy (§10, AC-09) | Verified on fixtures: sentinel secrets absent from Debug output, warnings, errors, and rendered screens; control sequences neutralized; credential-shaped text masked by default (`tests/privacy.rs`). Masking is best effort. |
| Real data | Read-only adapter smoke on one Windows developer machine (codex-cli 0.159.2, Claude Code 2.1.311, opencode 2.0.29): discovery of all three providers and transcript page reading only; a before/after content-hash inventory found no silent content change (concurrent provider changes are reported as inconclusive, not attributed). The six synthetic harnesses were exercised through the real binary for roster, paging, session summaries, limits and locked-source isolation, with child expansion and transcript navigation exercised through the real readers/worker/App/renderer under a test backend rather than a live terminal. Outstanding on real data: OMP/Hermes/Pi installations, live append (AC-04), an interactive TUI run, macOS/Linux, and Windows ACL-level permission checks. |
| Performance (§11) | Deterministic work budgets enforced; timings measured on Windows only ([benchmarks](superpowers/notes/2026-10-30-benchmarks.md)). |

**Known deviations from the requirements above:**

- The roster scan is not the header/tail metadata-only pass the design called for; Codex and Claude Code stream each file once (bounded per call). OMP and Pi use bounded prefix/tail probes. Deferred.
- Process-to-session matching needs the session ID on a command line, so most sessions show `Unmatched` or same-provider *candidates*; Windows does not expose other processes' working directories.
- Cost is priced only from the bundled catalog or an exact user override; anything else is `Unknown`. A session-wide token counter cannot be split into individual requests, so per-request tiers and time-of-day tariffs stay `Unknown` at session level.
- Search, filters, and counts cover loaded roots and loaded children only; 50 roots load at a time. This is a deliberate bound, not an absence claim.
- Native Pi has no subagents, no recorded session end/active interval, and speed only when an assistant record carries `durationMs`. Hermes keeps per-event compaction before/after/duration and request speed `Unknown`.
- macOS and Linux are unvalidated; OpenCode and macOS are not benchmarked.

## 1. Executive summary

AgentScope is a **read-only, local-first observability application for AI coding agents**. It gives developers one place to discover agent sessions, inspect full conversations and tool activity, understand token consumption and estimated costs, and identify sessions that may need attention. It does not launch, control, or modify the underlying agents.

The initial product focuses on sessions produced by **OpenAI Codex CLI, Claude Code, OpenCode, OMP (oh-my-pi), Hermes, and native Pi**, including agents launched directly or through orchestrators such as Hermes. AgentScope should feel like an independent, cross-provider counterpart to the OMP Agent Hub: a session roster with a drill-down inspector and transcript timeline. A future architecture will support collectors on multiple machines and a unified hub, but **the MVP is strictly local and TUI-only**.

**Product promise:** *Understand what every local coding agent is doing without changing how it runs.*

## 2. Problem and opportunity

### 2.1 The problem

Developers running several coding agents often lose visibility after delegation. An orchestrator may launch a worker and receive a session or process identifier, but it might fail to poll for completion, lose context, or not surface failures. Standard OS process views show CPU and PID but not task intent, tool calls, transcripts, token usage, or whether the agent has stopped making progress.

This creates five user problems:

1. **Discovery:** “Which agent sessions exist, and which are still active?”
2. **Transparency:** “What did this agent read, edit, execute, or say?”
3. **Diagnosis:** “Is this worker running, waiting, potentially stalled, or actually finished?”
4. **Efficiency:** “How many tokens and requests were used, and what did they cost?”
5. **Traceability:** “Which project, working directory, and possibly parent workflow does this session belong to?”

### 2.2 Primary user

A developer or small technical team operating several local coding agents concurrently, possibly launched by Hermes, shell scripts, IDEs, or other orchestrators. They need a fast, trustworthy read-only view rather than another orchestration framework.

### 2.3 Jobs to be done

- **When I delegate implementation or review**, I want to open a roster and see the newest activity for each session, so I know whether work is progressing.
- **When an agent becomes quiet**, I want an evidence-based explanation of what is known and unknown, so I can decide whether to investigate manually.
- **When a task is expensive**, I want token totals and cost provenance, so I can distinguish measured use from estimates.
- **When a worker fails**, I want to read its transcript and tool outputs in chronological order without switching among six harnesses' tools.
- **When I move between machines** (future), I want the same inspection experience without exposing local secrets or sending raw transcripts to a third party.

## 3. Goals, non-goals, and principles

### 3.1 Product goals

**G1 — Unified session roster.** Discover supported local provider sessions and display provider, title or first prompt, project, recency, and observable state.

**G2 — Full-fidelity drill-down where available.** Show normalized conversation turns, tool invocations/results, timestamps, error details, and expandable original content.

**G3 — Evidence-based activity and health.** Correlate persisted events with OS process information where possible. Never mistake “quiet” for “crashed.”

**G4 — Trustworthy usage information.** Show input, output, cache, model, context, and estimated cost only when derivable; label every metric's reliability.

**G5 — No-disruption adoption.** AgentScope must run beside existing tools with no hooks, wrappers, modified commands, elevated privileges, or write access to agent data.

**G6 — Extension path.** Keep provider ingestion separate from presentation so that new providers, future Hermes metadata, and remote collectors can be added without redesigning the TUI.

### 3.2 Non-goals for the MVP

- Launch, kill, resume, approve, message, or retry agents.
- Replace Hermes, OMP, Codex, Claude Code, or OpenCode.
- Act as an MCP server, agent orchestrator, LLM reviewer, or autonomous fixer.
- Guarantee reconstruction of hidden reasoning, unsaved conversations, or missing stdout.
- Establish parent-child relationships from guesses or shared project folders alone.
- Offer a web UI, smartphone access, cloud sync, authentication service, or multi-machine aggregation.
- Present estimated monetary cost as actual billed spend.

### 3.3 Operating principles

**Read-only by design; provenance over polish; unknown is a valid state; local-first privacy; progressive enhancement; no unnecessary daemon; small, testable components.**

## 4. Product scope and release definition

### 4.1 Initial delivery slice vs MVP

The **initial delivery slice** covers Codex session discovery and a readable transcript only. This is an incremental engineering milestone, *not* the full product MVP.

The **MVP release** includes:

| Area | MVP capability |
|---|---|
| Platforms | One local machine per running application; Windows and macOS validation, Linux best effort |
| Providers | Codex CLI, Claude Code, OpenCode, OMP, Hermes, and native Pi (current detected local storage variants; cloud-only and WSL-contained stores excluded) |
| UI | Keyboard-first Ratatui TUI: session roster, session inspector, transcript reader |
| Discovery | Provider-native local session stores; recent sessions, project and model metadata when present |
| Transcript | Chronological user/assistant messages and expandable tool inputs/outputs |
| Live view | Incremental refresh of stored sessions and local process sampling |
| Usage | Token categories, requests where provable, optional estimates with provenance |
| Health | Last activity, observed process presence, “possibly stalled” heuristics, explicit completed/failed events where available |
| Controls | Search/filter/sort, open details, refresh, help, quit |
| Privacy | Local-only, read-only, redact obvious secrets in display, no network telemetry |

**MVP does not require** a persistent cache database, distributed architecture, notifications, or an integration that changes how an agent starts. Add a small cache only if necessary to meet performance targets.

### 4.2 Later releases

- **Post-MVP 1 — Diagnostics:** repeated-tool-error detection, useful filtering, longer history, richer usage reports, export, optional local alerts.
- **Post-MVP 2 — Integrations:** Hermes registry correlation where a safe read interface exists; explicit lineage links; project/task labels.
- **Post-MVP 3 — Multi-device:** lightweight host collectors, secure hub transport, remote device health, TUI attached to an aggregate hub, optional web dashboard.
- **Exploratory:** opt-in LLM health advisor *only for ambiguous cases*; not part of the monitoring hot path.

## 5. Core user experience

### 5.1 Information architecture

**Roster → Session inspector → Transcript / Tool details**

Secondary views: filters, usage details, source/provenance information, and application help.

### 5.2 Session roster

The roster must support: provider, display name, project, last activity, current/last model, state plus confidence, age, token totals (if available), and estimated cost (if configured). A compact summary shows the total number of discovered sessions and counts by state, with time range and filter visibility clearly indicated.

```text
AgentScope | LOCAL | Codex 7  Claude 4  OpenCode 3 | Live: on
─────────────────────────────────────────────────────────────────
STATUS       PROVIDER   SESSION / PROJECT             LAST    TOKENS
● Active     Codex      Inventory API · RipperGame     12s     91k
! Quiet      Claude     Scanner refactor · RipperGame   9m     73k
? Unknown    OpenCode   PR review · CortexSandbox       2h     41k
✓ Complete   Codex      Localization · RipperGame       1d     28k
─────────────────────────────────────────────────────────────────
Enter inspect   / filter   s sort   r refresh   ? help   q quit
```

The source of “Active” must be clear. **A recently modified transcript does not by itself prove that an associated process remains alive.**

### 5.3 Session inspector

The inspector displays a header (session/provider/model/project, first/last timestamps), process correlation (if supported), state and supporting evidence, measured/estimated usage, and an event timeline. Selecting a timeline row opens readable tool arguments and results, with truncation indicators and an explicit “show full content” action. Long paths and huge outputs should be scrollable and searchable.

```text
< Back   CODEX / Inventory API
Project: RipperGame  Model: ...  Session: ...
Status: ? Possibly stalled   Evidence: no recorded events in 9m
Tokens: 73,214 reported | Cost: $0.23 estimated | Process: unmatched

12:14  user       Implement inventory checks and run tests
12:15  assistant  I will inspect the current implementation
12:15  tool       Read InventoryComponent.cpp                [expand]
12:17  tool       Edit InventoryService.cpp                [expand]
12:18  tool       cargo test / build                        [expand]
12:18  tool       Exit 1; 2 failed assertions               [expand]
12:20  assistant  Investigating the validation path...
```

### 5.4 Transcript reading rules

- Preserve order and timestamps; clearly mark missing timestamps.
- Distinguish user prompts, assistant text, tool requests, tool results, system events, and usage snapshots.
- Collapse verbose outputs initially; show both preview length and total available length.
- Display raw-record view only on explicit request, with sensitive fields masked.
- Permit session search and navigation; do not infer or synthesize assistant text.
- Never promise access to private chain-of-thought; display only provider-exposed and persisted text or reasoning summaries.

### 5.5 Keyboard behavior

| Key | Action |
|---|---|
| `j`/`k`, arrows | Navigate rows |
| `Enter` | Inspect session or expand selected event |
| `Esc`/`Backspace` | Back / close panel |
| `/` | Search or filter |
| `Tab` | Switch focus between panes |
| `s` | Cycle sort order |
| `r` | Refresh immediately |
| `?` | Contextual help |
| `q` | Quit when not in text input |

Accessibility considerations: good contrast on dark/light terminals, icons never as the only state indicator, keyboard-only operation, graceful narrow terminals, and safe truncation of long strings.

## 6. Functional requirements

| ID | Requirement | Priority | Acceptance evidence |
|---|---|---|---|
| FR-01 | Auto-discover known provider-native session paths; allow configured overrides | Must | Fixtures and manual runs find valid sessions on supported OSes |
| FR-02 | Parse Codex JSONL with version-tolerant handling of unrecognized records | Must | Partial, unknown, and malformed records do not crash app |
| FR-03 | Parse Claude Code JSONL including tool-use/result association where possible | Must | Correctly associates multi-part tool events in fixtures |
| FR-04 | Read OpenCode session/message data through a safe read-only approach | Must | Reads an active SQLite DB without modifying source data |
| FR-05 | Unify provider data as sessions, messages, tool events, usage, and provenance | Must | Identical TUI contract across adapters |
| FR-06 | Show searchable, sortable roster and session inspector | Must | Keyboard-only navigation passes acceptance script |
| FR-07 | Expand tool inputs, tool outputs, errors, and truncated content | Must | Long content remains readable, no output lost silently |
| FR-08 | Incrementally update observed sessions while app runs | Must | Newly appended complete records appear without restart |
| FR-09 | Display measured token categories and flag derived or unavailable values | Must | Usage fixtures avoid double-counting |
| FR-10 | Display model-aware *estimated* costs only when price configuration is available | Should | Missing pricing produces Unknown, not $0 |
| FR-11 | Discover relevant agent processes and show evidence/correlation confidence | Should | No false hard failure on ambiguous process match |
| FR-12 | Show time since last observed activity and possible-stall alerts | Should | Silent active processes remain labeled uncertain |
| FR-13 | Display discovered child-session relationships only when explicit source evidence exists | Should | Ambiguous ancestry remains Unlinked |
| FR-14 | Keep read-only behavior even on malformed files, race conditions, or database locks | Must | No write paths to provider stores; automated checks |
| FR-15 | Support settings for source roots, refresh cadence, redaction, and pricing | Should | Runs without editing source code |

## 7. Session, activity, and health semantics

### 7.1 Session state is not process state

Use separate fields to avoid conflation:

- **Session lifecycle:** `unknown`, `ongoing`, `completed`, `failed`, `cancelled` — only when supported by persisted evidence.
- **Process observation:** `observed_running`, `observed_exited`, `not_found`, `not_checked`.
- **Activity:** `recent`, `quiet`, `possibly_stalled` — heuristic classifications, not terminal outcomes.

The UI synthesizes a human-readable status plus a short evidence label. Example: “Possibly stalled — process observed, no persisted events for 9m.” Do not show “Crashed” without a reliable failure event or captured exit code.

### 7.2 Process matching

Match a process to a session only with strong evidence (explicit session ID in arguments or provider metadata, recorded PID, or future integration). Timestamp, executable name, and working directory are *hints*, not proof. When ambiguous, show process candidates separately rather than fabricating a relationship. PID reuse must be considered (include start time when available).

### 7.3 Stall heuristics

The default can flag a candidate after a configurable quiet interval (e.g., 5 minutes) **only if the process is observed running and the data source is normally expected to write activity**. There should be a distinct “quiet/unknown” status if event write cadence is unknown. Repeated identical failed tools can be diagnosed in a later release. The MVP does not auto-retry or kill anything.

## 8. Token and cost accounting

A normalized `UsageObservation` records provider, model, timestamp, token breakdown, aggregation scope, raw source location, and provenance. Required distinctions:

- **Reported usage:** emitted by a provider or client in a persisted record.
- **Derived usage:** computed from trusted reported records after provider-specific de-duplication/delta logic.
- **Estimated cost:** usage multiplied by the bundled, versioned price schedule or an exact user override.
- **Unknown:** no reliable observation; never silently coerce missing values to zero.

Implemented money provenance distinguishes four sources: `ReportedApi` (a provider-reported API amount; a reported zero stays reported and does not override the estimate), `SavedCatalogEstimate` (a provider-saved estimate whose source is verified), `UserOverride`, and `BundledReference`. The bundled catalog is `src/pricing/catalog.json` (schema version 1, revision `2026-10-30.2`, checked 2026-10-30 from eight official source pages). Every amount is an API-equivalent estimate, never billed spend; an unverified saved amount, a subscription allocation, a parent's quoted child cost, and an aggregate compaction row are never added to per-request money. A model, billing route, request-length tier, time-of-day tariff, unmapped cache-retention rate, or reasoning counter without an explicit rate stays `Unknown`; a partial subtotal is labeled and never promoted to a total.

Provider adapters must handle cumulative snapshots versus per-request counts differently. For Codex, do not sum every `token_count` snapshot; compare the appropriate cumulative/delta semantics per observed record. For Claude Code, deduplicate repeated or streaming messages by stable identifiers and avoid counting parent rollups and child sessions twice. For OpenCode, validate whether usage is per-message/part and whether the stored amount is already a computed charge before aggregation. OMP and Pi count only uniquely owned requests across recorded branches: copied fork/continuation history and unprovable ownership are not charged, and an authoritative revision replaces a stored contribution instead of adding one. Hermes counts recorded per-row/session aggregates and never converts them into request deltas or measured throughput; a compression ledger row is one aggregate amount, never split per event.

**Context utilization** is not equivalent to total lifetime session tokens; display a context percentage only when current-window consumption and window size are defensible. **Provider subscription spend cannot be inferred from token totals.** Estimated API-equivalent costs must be labeled accordingly.

## 9. Technical design and boundaries

### 9.1 Local architecture

```text
Codex / Claude / OMP / Pi JSONL ───> native readers ──┐
OpenCode / Hermes SQLite        ───> DB readers ──────┼──> Normalized snapshots/events
Claude Desktop Code/Cowork      ───> Desktop reader ──┤
OS processes                    ───> Process observer┘              │
                                                                    ↓
                                                      In-memory session index
                                                                    ↓
                                                               Ratatui TUI
```

The MVP can rebuild a bounded in-memory index for recent data, then maintain incremental cursors. Persistent storage is optional; if introduced, it must be a separate AgentScope-owned cache, never an agent's database. Roots resolve from CLI, provider environment, config, then the platform default; OMP and Pi refuse to share an ambiguously configured path rather than mislabel it.

### 9.2 Possible Rust modules (illustrative, not prescribed)

```text
src/
  main.rs
  app.rs                 # state + update loop
  domain.rs              # session/event/usage types
  providers/
    mod.rs               # read-only provider interface
    codex.rs
    claude.rs
    opencode.rs
  ingest/
    jsonl.rs             # incremental line reader
    discovery.rs         # paths, overrides
  processes.rs           # optional process snapshots
  usage.rs               # provider-aware accounting
  ui/
    mod.rs
    roster.rs
    transcript.rs
    details.rs
  privacy.rs
  config.rs
```

Start with a small provider interface using `trait` only when two implementations demonstrate genuine commonality. Avoid building a plugin framework, event bus, or microservices in the MVP.

### 9.3 Normalized data contracts (conceptual)

| Entity | Minimum fields |
|---|---|
| `SessionKey` | `host_id` (local placeholder), `provider`, `provider_session_id`, optional source path |
| `Session` | key, display title, project/cwd, timestamps, model(s), lifecycle evidence |
| `Event` | session key, source event ID/offset, timestamp, kind, text/structured payload, parent event ID |
| `ToolInteraction` | tool name, invocation ID, arguments, output, status, related event IDs |
| `UsageObservation` | model, token categories, scope, timestamp, provenance, pricing reference |
| `ProcessObservation` | PID, process start time, executable, CPU/RAM, observed timestamp, matching confidence |

The source record's **provenance** must survive normalization, enabling trustworthy diagnostics and adapter regression tests. Do not persist secrets unintentionally as debugging payloads.

### 9.4 Ingestion safety and performance

- JSONL: stream instead of loading all history into memory; handle trailing partial lines, appended records, truncation, rotation, invalid UTF-8, very large records, and disappearing files.
- SQLite: open OpenCode DB strictly read-only, account for WAL and concurrent writers, use bounded queries and `busy_timeout`, and never bypass live WAL state with an inappropriate `immutable=1` shortcut.
- UI: never perform large disk scans on every keypress; use a bounded refresh loop and lazy transcript details.
- Discovery: respect `CODEX_HOME`, relevant configuration roots, OpenCode's `opencode db path`, and per-provider platform-specific paths where available.
- Failure isolation: one corrupt session must not hide the rest; surface a readable adapter warning.

### 9.5 Suggested technologies

`ratatui` and `crossterm` for UI, `serde`/`serde_json` for formats, `rusqlite` for read-only OpenCode access, `sysinfo` for processes, `thiserror` or `anyhow` for errors, and Rust standard library I/O first. Introduce `tokio` or filesystem watchers only when a tested use case makes the synchronous loop insufficient.

## 10. Security and privacy

Read-only ingestion is essential but not sufficient. Local transcripts may contain API keys, filesystem paths, code, command output, personal information, and embedded images.

**MVP controls:**

1. No network listeners, analytics, crash-report uploads, auto-update telemetry, or cloud requests.
2. No access to `auth.json` or other credential stores; source discovery must explicitly exclude credentials and unrelated provider data.
3. Source files and databases opened read-only; never modify permissions, lock files, or session content.
4. Mask common secret patterns in UI by default; allow revealing original content only through an explicit, local action if supported. Redaction is best-effort and not a guarantee.
5. Avoid logging raw prompts/tool results in AgentScope diagnostics by default.
6. Limit file/record sizes and display output to prevent terminal escape/control-sequence injection; sanitize untrusted text before rendering.
7. Show source paths and unsupported-format warnings clearly; guard against accidental directory traversal or symlink escape during discovery.

**Future multi-device control:** authenticate each collector, encrypt transport, keep raw transcript sync opt-in, support metadata-only mode, and define retention/access controls before implementation.

## 11. Non-functional requirements

| Category | MVP target |
|---|---|
| Safety | No writes to agent data or agent processes; no agent launches or terminations |
| Responsiveness | Typing/navigation should remain responsive while scanning recent sessions; aim for visible input response under 100 ms on ordinary machines |
| Initial discovery | Aim for a usable roster within 3 seconds on a moderate dataset; report scan progress when larger |
| Resource usage | Bounded memory through streaming and lazy loading; no repeated full-file reparse on every refresh |
| Robustness | Corrupt/truncated JSONL or locked SQLite does not crash TUI |
| Portability | Develop on one OS; verify path/process handling on Windows and macOS before claiming both supported |
| Transparency | Every displayed status or number can be traced to a source observation or explicitly labeled estimate |
| Maintainability | Adapter fixtures and regression tests protect against provider format changes |

These are engineering targets, not claims of measured performance. Define a benchmark fixture corpus before treating them as release gates.

## 12. Acceptance scenarios

**AC-01 — No-disruption start:** With an independently running Codex session, launch AgentScope in another terminal; the Codex execution and its file contents remain unchanged.

**AC-02 — Drill-down:** Select a detected session and read an ordered transcript. Expand a tool event and inspect its stored arguments and result.

**AC-03 — Multi-provider:** Discover at least one real saved session each from Codex, Claude Code, and OpenCode in the same local roster.

**AC-04 — Live append:** With a provider writing new complete records, observe new activity after refresh without restarting the application.

**AC-05 — Partial and invalid records:** A truncated final JSONL line is retried after completion; malformed non-final records are reported without halting unrelated sessions.

**AC-06 — Accurate usage:** Golden fixtures for duplicate and cumulative token observations produce the expected totals, with no double-counting.

**AC-07 — Missing data:** Unavailable cost, missing completion event, or ambiguous process matching display Unknown/Unmatched instead of misleading zeros or hard failures.

**AC-08 — Read-only regression:** Running all acceptance tests never changes provider session files, the main provider databases, configuration, permissions, credentials, or running processes. The only permitted filesystem change is SQLite's transient `-shm`/`-wal` sidecars for read-only SQLite readers (OpenCode and Hermes, ADR-0002).

**AC-09 — Privacy:** Raw terminal control sequences are neutralized and known test secrets are masked in the default transcript display.

**AC-10 — Keyboard UX:** A user can discover a session, open its transcript, inspect a tool result, search, and return to the roster without a mouse.

**Verification (2026-10-30):** each scenario has a fixture-backed test in `tests/acceptance.rs` (`ac01_…` through `ac09_…`, and `acceptance_scenarios_ac01_to_ac10_have_exercised_paths` for the keyboard-only AC-10 flow), with the full read-only and privacy suites in `tests/read_only.rs` and `tests/privacy.rs`. **Real-data scope:** only discovery (AC-03: all three providers listed) and transcript page reading were exercised against real local stores, plus a hash-based filesystem inventory before and after that classifies concurrent provider changes as inconclusive. AC-04 (live append) and AC-01 against an independently running real agent remain outstanding on real data, as does AC-10 interactively. On Windows the permission checks cover only the read-only attribute, not NTFS ACLs; Unix mode bits are compared on Unix. For AC-08, SQLite's transient OpenCode `-shm`/`-wal` sidecars are the single permitted change ([ADR-0002](decisions/0002-opencode-readonly-wal-sidecars.md)); session files, the main database, configuration, permissions, and credentials must remain unchanged, and AgentScope never signals or alters processes.

**Native closure (synthetic, 2026-10-31):** `ac11_…` (exact 50/100/125 root pages, children, forks, direct tokens/cost, newest-entry open, stale generation) and `ac12_…` (partial, truncated, damaged and locked sources hide nothing) in `tests/acceptance.rs` run on the integrated native world, with matching native-world cases in `native_children`, `desktop_sessions`, `read_only`, `privacy`, `perf_budget` and `tui_flow`. Native children, local Claude Desktop sessions, the Hermes reader and the native Pi reader are implemented; Claude Desktop chat history, WSL/remote/cloud histories and Pi extension subagents are **not**.

**OMP (synthetic, 2026-11-01):** OMP sessions and artifact-tree subagents are implemented. The OMP family, fork and a secret-bearing session are part of the native world (`World::native`; `World::with_omp` adds them to the three-provider world). `omp_family_opens_through_the_real_worker_app_and_renderer` (acceptance), the `omp_*` cases in `read_only`, `privacy`, `perf_budget` and `live_refresh`, and `tests/omp_discovery.rs`, `omp_hydration.rs`, `omp_transcript.rs` cover children/grandchildren, direct tokens (a parent's quoted child cost is not added), category counters, compaction summaries with Unknown cost, newest and older transcript pages, live append, replacement, cancellation and a damaged or unsupported sibling. Compaction cost and active time are `Unknown` (OMP persists neither).

**Native Pi (synthetic, 2026-11-01):** the standard Pi JSONL tree format is read as a complete `ProviderReader` (discovery, hydration, transcript/details/count/source, job-scoped cancellation). The Pi family, a verbatim fork and a secret-bearing session are part of the native world (`World::native`; `World::with_pi` adds them to the three-provider world). `pi_family_opens_through_the_real_worker_app_and_renderer` (acceptance), the `pi_*` cases in `read_only`, `privacy` and `perf_budget`, the native-world cases in `native_children`, `desktop_sessions` and `tui_flow` (whose population now includes Pi), plus `tests/pi_sessions.rs`, `pi_tree.rs` and `pi_transcript.rs`, cover: newest-entry open and exact backward identities, source inspection, unsupported/malformed records that keep valid neighbours, torn appends (no phantom until the terminator, then one request), truncation/replacement invalidation, cancellation, combined details budgets, extension state excluded, and no invented extension children. Request speed is Unknown unless an assistant record carries `durationMs`; session end and active intervals are Unknown because standard Pi records persist neither.

**Six-harness release (synthetic, 2026-11-01):** `World::with_all_harnesses()` builds one 125-root store across all six harnesses with five native child families (Codex, Claude Code, OpenCode, OMP, Hermes delegate), a nested grandchild, a Pi root with no children, a Hermes continuation member, a native zstd archive, Claude Desktop Code/Cowork/released sessions, partial/damaged/locked sources and injected secrets. `six_harness_roster_preserves_direct_usage_and_generation` and `six_harness_refresh_keeps_selection_and_truthful_unknowns` in `tests/acceptance.rs` assert exact 50/100/125 root identities with no duplicate or child root slot, every member's owned tokens/cost, each family's final transcript event, a selected identity surviving a roster-generation change with a late obsolete preview answer discarded, loaded-only search (an unloaded match is not found; a child match keeps its parent), manual historical reading not being jumped, and Hermes aggregate rows/Pi missing timing remaining `Unknown` rather than becoming request speed. The full gate run after integration passed 857 tests (0 failed, 3 ignored) with clean `fmt`/`check`/`clippy -D warnings`; the real binary reproduced the six-harness roster, paging, per-harness summaries, locked/limit isolation and a 133-file source inventory (130 byte-identical; three SQLite `-shm` sidecars only). The approved release's 18 design scenarios are walked row-by-row, with the owning tests and smoke observations, in the R2 completion report; no row is satisfied by a staged subset.

## 13. Risks, dependencies, and mitigations

| Risk | Why it matters | Mitigation |
|---|---|---|
| Internal format drift | Provider file layouts change across versions | Version-tolerant adapters; fixtures; warnings |
| Incomplete persisted history | Agent may suppress persistence or write only some events | Explicit missing-data markers |
| Misleading process correlation | A generic Codex process may not belong to the session shown | Evidence/confidence field; never force a match |
| Token overcounting | Cumulative records and duplicate streaming entries | Provider-specific rules and golden test vectors |
| Large transcripts | Hundreds of MB and huge tool outputs | Streaming, bounded previews, lazy loads |
| Concurrent SQLite writes | Live OpenCode writes while AgentScope reads | Read-only connections, WAL-aware queries, retries |
| Sensitive output | Agent logs can contain credentials or private source | Default masking, no upload, sanitized display |
| Scope creep | Orchestration, web UI, and advisor could derail MVP | Explicit non-goals and staged roadmap |

## 14. Open decisions and validation spikes

These items remain to be verified against **real sessions on the target devices** before the release is called stable (implementation on synthetic fixtures is complete):

1. Exact Codex, Claude Code, OpenCode, OMP, Hermes, and Pi schema versions and configurable data roots on real installations; only the first three were partly exercised.
2. Real per-request versus cumulative token behavior for OMP, Hermes, and Pi producers, including whether copied branch history and compaction usage are persisted the same way the fixtures model.
3. Availability of usable session IDs in OS processes, and realistic limits of process-to-session correlation.
4. Whether source records include explicit terminal outcomes; where they do not, the display stays Unknown.
5. How OpenCode and Hermes schema/WAL behavior responds to a concurrent read-only query on real live databases.
6. Whether the Hermes process registry can eventually be inspected safely without importing, mutating, or invoking Hermes internals.
7. How much parent/child lineage is explicitly persisted across provider versions for each of the five subagent-capable harnesses.
8. Claude Desktop Code/Cowork manifest layouts and released-session semantics across application versions.
9. Whether OMP/Pi keep recording whole-request durations in the same fields, and whether Pi ever gains a native child-session format.

Capture **redacted, consented, version-labeled test fixtures**; never commit real agent transcripts or credentials.

## 15. Success criteria

MVP is successful when the user can run Hermes and other coding tools unchanged, launch a single terminal application, see sessions from all six supported harnesses, select one, read its recorded work and tool outputs, inspect usable token totals, and distinguish *observed facts* from uncertain lifecycle and cost estimates.

Longer-term product success: the same experience across multiple local devices without compromising source privacy or depending on any single orchestrator.

## 16. Reference material and format validation

These are implementation research leads, **not stable provider data format contracts**. Verify formats against installed versions before coding adapters.

- Hermes Codex skill: https://github.com/NousResearch/hermes-agent/blob/main/skills/autonomous-ai-agents/codex/SKILL.md
- Hermes process management: https://hermes-agent.nousresearch.com/docs/user-guide/features/tools
- Hermes process registry: https://github.com/NousResearch/hermes-agent/blob/main/tools/process_registry.py
- Codex session/usage exploratory format: https://tokenuse.app/docs/development/tools/codex/
- Claude Code transcript structure: https://github.com/eidos-agi/claude-session-commons/blob/master/docs/jsonl-schema.md
- OpenCode official storage troubleshooting: https://docs.opencode.ai/docs/troubleshooting/
- AgentScope's UX inspiration: OMP Agent Hub (user-provided reference screenshot; not a dependency)

---

**Current scope:** Local, read-only, six-harness TUI MVP. Process control, orchestrator integration, web UI, and remote collection are outside the first release.
