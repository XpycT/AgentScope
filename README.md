# AgentScope

**One terminal dashboard for all your AI coding agents.** See what they're doing, what they're using, and whether they're actually stuck.

You run Codex in one terminal, Claude Code in another, and OpenCode somewhere else. Each keeps its own logs in its own format. Which session burned all those tokens? Is that agent still working, or did it quietly die twenty minutes ago? What did it actually do?

AgentScope puts every session on one machine into a single keyboard-driven TUI. It reads the logs your agents already write. It never launches, stops or messages them, and it never changes their data.

**Works with:** Codex CLI · Claude Code (CLI and Desktop) · OpenCode · OMP (oh-my-pi) · Hermes · Pi

## Why AgentScope

- **See everything in one place.** One roster across all harnesses, with search, filter and sort. Subagents, forks and continuations are nested under their parent session.
- **Read what really happened.** Browse the full transcript: messages, reasoning summaries, tool calls and results, errors. Huge records load lazily, and the raw source record is always one key away.
- **Trust the numbers.** Token counts follow each provider's own usage rules, so duplicated streaming events and cumulative counters aren't double-counted. Costs are labeled as estimates. Unknown stays `Unknown`, never `$0.00`.
- **Know if an agent is stuck, with proof.** Status comes from evidence (`Active`, `Quiet`, `Possibly stalled`, `Complete`, `Failed`, and more) and always shows its reason. Silence is never presented as a crash.
- **Safe to point at your real sessions.** It's read-only and local, with no telemetry and no network. Terminal escape sequences in untrusted transcripts are neutralized, and common secrets are masked by default.

## Quick start

```bash
git clone <this repo>
cd AgentScope
cargo run --release
```

AgentScope finds the default session folders of each supported agent on its own. Press `?` for the key map, `Enter` to open a session, `/` to search and `q` to quit. Custom locations and every option are covered in [Run](#run).

**Project status: v1 implemented (pre-release).** All six readers (Codex, Claude Code, OpenCode, OMP, Hermes and native Pi), the transcript explorer, usage accounting, process candidates, and the TUI are implemented and covered by synthetic-fixture tests. Native child sessions and forks are implemented where the provider records them: children for Codex, Claude Code, OpenCode, OMP and Hermes, forks for Codex, OpenCode, OMP and Pi. Local Claude Desktop Code/Cowork stores are implemented and were exercised end to end on synthetic stores (see Verification status). Native Pi is read from its standard JSONL tree format; Pi extension subagents are explicitly unsupported. Validation against real installations is partial; see [Verification status](#verification-status) and [Known limitations](#known-limitations) before relying on it.

## What it does

- **Unified session roster** for Codex, Claude Code, OpenCode, OMP, Hermes, and native Pi, with search, filter, and sort over the loaded roots.
- **Transcript explorer**: user/assistant messages, reasoning summaries, tool calls and results (expandable), errors, and timestamps when the provider recorded them. Pages load lazily; huge records are bounded previews with an explicit way to view the source record.
- **Usage** per provider rules (cumulative snapshots, duplicate streamed records, cache categories), labeled `Reported`, `Derived`, `Partial`, `Estimated`, or `Unknown`. Streamed requests are priced one by one with their own model; cost is an estimate from the bundled official price catalog (or your exact override) and is `Unknown`, never `$0.00`, when it cannot be priced completely. A reported `$0` (e.g. a subscription) is shown as reported next to an API-equivalent estimate, never as free.
- **Evidence-based status**: `Active`, `Recent`, `Quiet`, `Possibly stalled`, `Complete`, `Failed`, `Cancelled`, `Unavailable`, or `Unknown`, each with its reason. Silence is never presented as proof of a crash.
- **Measured facts** in the session view: recorded turn timing and active-time coverage, whole-request output speed with `measured/total` requests, latest prompt occupancy versus its applicable window, and compaction events with token reduction and per-event versus aggregate cost. Each figure is `Unknown` unless the provider recorded the evidence; unsupported or unread histories are labeled rather than shown as zero.
- **Process candidates**: same-provider processes are listed as *candidates*; a session is shown as *matched* only when an identity-bearing argument (its session ID) appears on a process command line.
- **Live refresh**: appended records, partial trailing lines, in-place revisions, and rotation are handled without restarting.

## Run

```bash
cargo run --release
```

Options: `--config <path>`, `--codex-sessions <dir>`, `--claude-projects <dir>`, `--claude-desktop <dir>`, `--opencode-db <path>`, `--omp-sessions <dir>`, `--pi-sessions <dir>`, `--hermes-db <path>`, `--no-processes`, `--no-color`, `--no-mouse`, `--help`, `--version`. The TUI needs an interactive terminal on both stdin and stdout.

Roster rows are two lines: session (title; status, age, project), harness over billing provider, model over effort, lifetime tokens, API-equivalent estimate, active time and the latest timed request tok/s. The marker and session columns stay pinned; `Left`/`Right` (`h`/`l`) scroll whole trailing columns. From 150 columns the preview takes 45 columns beside the list; narrower, `Tab` swaps list and preview (`J`/`K` scroll the preview). Minimum size 60×20.

The roster loads the newest **50 top-level groups** at a time; children, forks, and compression continuations never consume a root slot. `m` (or `Enter` on a focused Load more row) reveals the next 50. A load is a frozen generation: new discoveries, deletions, and late parent edges appear only after an explicit refresh, which keeps the selected session where it can. **Search and filters cover only the loaded roots** (and any loaded children, whose matching parent stays visible); an unloaded session is never reported as absent.

Roster keys: `j`/`k` move (and rest on explicit Load more rows), `g`/`G` first/last, `Enter` open a session or load more on a focused Load more row, `m` load the next 50 roots, `Space` show or hide a session family's children, `/` search loaded roots, `n`/`N` next/previous match, `f` filter, `s` sort (`S` reverse), `r` refresh, `p` process candidates, `?` help, `q` or `Ctrl+c` quit. Inspector keys: `j`/`k`/`g`/`G` move, `Enter` expand or collapse, `/` search loaded events, `n`/`N` next/previous match, `d` details, `v` source record, `D` diagnostics, `x` toggle masking in source views, `r` retry/refresh, `Esc` back.

Default data locations are the providers' own session stores, and each root resolves in this order: **CLI flag, then the provider's environment variable (where one exists), then `[providers.<harness>].root`/`db`, then the platform default**. Configured `extra_roots` are appended after the primary root. The config file is `agentscope/config.toml` in the platform config directory (`%APPDATA%\agentscope\config.toml` on Windows, `~/Library/Application Support/agentscope/config.toml` on macOS, `$XDG_CONFIG_HOME/agentscope/config.toml` or `~/.config/agentscope/config.toml` elsewhere) or the path given with `--config`; an unreadable explicit path is an error, while a missing default file is fine.

| Harness | CLI | Environment | Config | Platform default |
|---|---|---|---|---|
| Codex | `--codex-sessions` | `CODEX_HOME/sessions` | `[providers.codex].root` | `~/.codex/sessions` |
| Codex archive | — | `CODEX_HOME/archived_sessions` (derived) | `[providers.codex].archived_root` | `~/.codex/archived_sessions` (derived) |
| Claude Code | `--claude-projects` | `CLAUDE_CONFIG_DIR/projects` | `[providers.claude].root` | `~/.claude/projects` |
| Claude Desktop | `--claude-desktop` | — | `[providers.claude_desktop].root` / `extra_roots` | platform app-data `Claude` and `Claude-3p` directories |
| OpenCode | `--opencode-db` | `XDG_DATA_HOME/opencode/opencode.db` | `[providers.opencode].db` | `~/.local/share/opencode/opencode.db` |
| OMP | `--omp-sessions` | — | `[providers.omp].root` / `extra_roots` | `~/.omp/agent/sessions` |
| Pi | `--pi-sessions` | `PI_CODING_AGENT_DIR/sessions` | `[providers.pi].root` / `extra_roots` | `~/.pi/agent/sessions` |
| Hermes | `--hermes-db` | `HERMES_HOME/state.db` | `[providers.hermes].db`, else `.root` / `extra_roots` | `%LOCALAPPDATA%\hermes<suffix>\state.db` or `~/.hermes<suffix>/state.db` |

A configured Hermes home root additionally discovers `profiles/<name>/state.db`; `--hermes-db` names a single database. `HERMES_DATA_DIR_SUFFIX` is appended literally to the platform home name. A path named for both OMP and Pi (a CLI flag, config root, or extra root) is kept only when exactly one harness was given it through its CLI flag; if both or neither name it, neither harness reads it, and a startup note explains the conflict. Deriving the Codex archive from `CODEX_HOME` happens only when no custom sessions root is in use; an explicit `archived_root` always names the archive store.

Other configuration keys and their defaults: `refresh_ms` 2000, `process_ms` 5000, `process_enabled` true, `quiet_after_secs` 300, `stall_after_secs` 900 (must exceed quiet), `max_cached_sessions` 2000, `max_metadata_sources` 100000, `max_metadata_bytes` 67108864, `max_page_events` 500, `max_page_bytes` 262144, `max_record_bytes` 1048576, `dedup_ledger_entries` 100000, `dedup_ledger_bytes` 8388608, `refresh_busy_timeout_ms` 2000, `page_busy_timeout_ms` 250, `redaction` true, `mouse_enabled` true, `native_api_reference_estimates` true. Zero is rejected for every numeric limit. `--no-processes` and `--no-mouse` override config; `NO_COLOR` and `--no-color` disable color.

Discovery, hydration, transcript indexing, and count scans are sliced at **256 records, 8 MiB of expanded bytes, and a 100 ms scheduling target** (the time limit is checked between units of work, so one huge record can exceed it); discovery probes a 4 KiB head (header line up to 256 KiB) and a 32 KiB tail per source. A transcript/details page is at most 500 events and 256 KiB, a record 1 MiB, the loaded transcript 20,000 events with at most 16 pending loads, and the per-source ownership/dedup ledger is capped by the configured entries/bytes. Cancellation is job-scoped: cancelling a transcript read never releases another purpose's continuation on the same session. A session's canonical identity is `(harness, native ID, source)`; a compressed or archived physical copy is a locator on that identity, so one logical rollout is one row and a moved copy is not a second session. Metadata ceilings print `Discovery limited`/per-harness warnings instead of pretending the roster ended.

Mouse: capture is on by default and read-only. A click selects a row, a click on the marker expands it, a click on a Load more row loads that page, the wheel scrolls the list or the preview. Events that carry Shift are ignored, so Shift+drag (or your terminal's equivalent) still selects text. `--no-mouse` (or `mouse_enabled = false`) never enables capture and keeps every keyboard action. The terminal is restored on quit, `Ctrl+c`, setup failure and panic. Mouse clicks never act on monitored agents.

Claude Code subagents (`<project>/<session>/subagents/**/agent-<id>.jsonl`) appear as children of their session with their own direct usage; workflow `journal.jsonl` and tool-result files are never listed as sessions. Claude Desktop Code sessions reuse the same project store: `--claude-desktop <dir>` (or `[providers.claude_desktop]` `root`/`extra_roots`) names the Desktop application-data directory whose allowlisted `claude-code-sessions/**/local_*.json` manifests and Cowork project roots are read; without it the platform defaults are used when present. Only those stores are opened: credentials, renderer caches, session audit logs and VM bundles are not read, and no local chat history is reconstructed. Sessions a Desktop manifest marks released are hidden by default behind the filter panel's "Include released sessions" row and are never deleted.

OMP (oh-my-pi) sessions (`<root>/<working-directory bucket>/<timestamp>_<id>.jsonl`) and their artifact-tree subagents (`<session>/<agent>.jsonl`, nested to any supported depth) are read from `--omp-sessions` (or `[providers.omp]` `root`/`extra_roots`). A child is linked from the artifact directory shape plus the header evidence, a fork (`parentSession` without `continuationOf`) stays its own root, and a continuation chain is one logical session whose segments are read together. Lifetime usage counts every uniquely *owned* model call across all recorded branches (assistant messages and `model_usage` entries, with their purpose); copies a fork or continuation repeats are never charged again, and a parent's `task` result that quotes a child's cost is not added to the parent. Current model, effort and prompt occupancy follow the recorded active branch; whole-request speed uses the recorded request duration. OMP persists no usage for a compaction's summarization call, so **compaction cost is Unknown**; before/after tokens, method and the recorded summary are shown. Only the sessions tree is opened: the agent directory's credential, configuration, history, model and statistics stores, blob directories and arbitrary embedded paths are never read, and `.jsonl.gz` archives are reported as unsupported.

Prices come from an offline catalog compiled into the binary; nothing is fetched at runtime. The catalog is keyed by billing route and exact model, and every amount it produces is an **API-equivalent estimate**, never billed spend. Only two recorded amounts are retained: a provider-reported API amount (`ReportedApi`; a reported zero stays reported and only a positive amount overrides the API-equivalent estimate) and a provider-saved estimate whose source is verified (`SavedCatalogEstimate`). An unverified saved amount, a subscription value, or a parent's quoted child cost is not counted as money. Optional `[[pricing]]` entries replace the bundled card with the same exact `billing_provider` (the route that bills the model, e.g. `anthropic`, `openai`, `opencode`, `opencode-go`) and `model`; legs you omit stay unknown instead of inheriting bundled rates:

```toml
# Global keys must come before the first table: TOML assigns later keys to the
# table they follow.
native_api_reference_estimates = true   # label native Claude/Codex usage as a reference API estimate when no route is recorded
max_cached_sessions = 2000              # replaces the old `max_sessions`

[providers.codex]
root = "D:/stores/codex/sessions"       # CLI --codex-sessions wins over this
archived_root = "D:/stores/codex/archived_sessions"

[providers.hermes]
root = "D:/stores/hermes"               # a home: profiles/<name>/state.db is also read

[[pricing]]
billing_provider = "anthropic"          # replaces the old `provider = "claude"`
model = "claude-sonnet-5-5"
as_of = "2026-10-01"
input_per_mtok = 2.0
output_per_mtok = 10.0
cache_read_per_mtok = 0.10
cache_write_5m_per_mtok = 2.5
cache_write_1h_per_mtok = 4.0
```

`[[pricing]]` requires `billing_provider`, `model`, `input_per_mtok`, and `output_per_mtok`; optional `effective_from`/`effective_until` bound a card's window, `cached_input_per_mtok` is an accepted alias for `cache_read_per_mtok` (the two may not disagree), and `cache_write_per_mtok` prices a write whose retention was not recorded. Separately reported reasoning tokens need `reasoning_per_mtok`; reasoning already counted inside output is never charged twice. Unknown top-level or `[[pricing]]` keys are rejected.

The removed keys fail with the replacement name instead of being guessed: `max_sessions` → `max_cached_sessions` (cached roster rows) and `[[pricing]].provider` → `billing_provider` (the route that bills the model). The config file is never rewritten for you.

The experimental `inspect` command prints a Codex transcript from an explicitly supplied JSONL file without the TUI:

```bash
cargo run -- inspect path/to/session.jsonl
```

It escapes terminal controls and applies best-effort masking, reports malformed, incomplete, or over-1-MiB records with byte offsets, and may still print sensitive session content.

## Design principles

**No interference.** AgentScope does not start, stop, signal, or approve coding agents and does not modify their session stores, configuration, permissions, or credentials. **Evidence over guesses.** Missing logs, ambiguous process matches, and unknown costs remain explicitly unknown. **Local-first.** No network listeners, telemetry, or uploads.

## Read-only guarantee and its one exception

AgentScope opens Codex, Claude Code, OMP, and native Pi transcripts for reading only, and opens the OpenCode and Hermes SQLite databases read-only behind fixed allowlists that permit only named session tables and columns and deny credential, account, share-URL, system-prompt, write, and schema operations. Every query is a checked, bounded projection; no SQLite extension, file function, or arbitrary JSON path is reachable. It never reads provider credential stores or unrelated provider state, and never repairs, deletes, chmods, copies, or migrates provider files.

**Exception ([ADR-0002](docs/decisions/0002-opencode-readonly-wal-sidecars.md)):** OpenCode and Hermes use SQLite WAL mode. SQLite itself may create or update their transient `-shm`/`-wal` sidecar files next to the database when a read-only connection reads a live WAL database. Those are SQLite runtime files, not session content, but they are writes in the provider directory, so a provider directory is **not** guaranteed byte-for-byte untouched while that provider is running. If the directory is not writable and a live WAL cannot be read without a sidecar, that harness is shown as `Unavailable` and retried; permissions are never changed.

## Verification status

Run on Windows 11 (rustc/cargo 1.99.1) with provider versions codex-cli 0.159.2, Claude Code 2.1.311, and opencode 2.0.29 installed:

- `cargo fmt --all -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo build --release` pass. Last full run (2026-11-01, Windows 11, rustc/cargo 1.99.1): fmt and check clean, clippy clean with `-D warnings`, `cargo test` 857 passed / 0 failed / 3 ignored across 53 test targets (the ignored tests are the pre-existing real-data, timing-record, and process-sampling cases).
- **Fixture-backed (synthetic) acceptance** for AC-01..AC-10 in `tests/acceptance.rs`, read-only regression in `tests/read_only.rs` (session files, the main database, mtimes, and permissions unchanged; only `-shm`/`-wal` sidecars may appear), and privacy in `tests/privacy.rs` (sentinel secrets absent from Debug output, warnings, errors, and rendered screens; control sequences neutralized; credential-shaped text masked).
- **Real-data smoke (read-only, counts only):** `cargo test --test acceptance -- --ignored --nocapture real_data_smoke` read this developer's real Codex, Claude Code, and OpenCode stores through the adapters (discovery and transcript page reading only) and compared a content-hash inventory before and after. Changes that coincide with a length or mtime change, new files, and removals are reported as inconclusive (a live provider or a SQLite sidecar); only a same-length, same-mtime content change fails. The last run: 2058 files, 2057 identical, 1 inconclusive (concurrent change), 0 silent changes. It prints counts only, never content.
- **Not verified:** an interactive TUI session on real data (the harness has no interactive terminal; the keyboard path is covered by `tests/tui_flow.rs` and the acceptance flow on fixtures), anything on macOS or Linux, and live-append against a real running agent (covered on synthetic fixtures only).
- Performance: deterministic work-budget tests plus informational timings in [the benchmark note](docs/superpowers/notes/2026-10-30-benchmarks.md). Measured on Windows only.
- **Integrated native closure (synthetic fixtures):** one 125-root store (nested Codex/Claude/OpenCode families and forks, Claude Desktop Code/Cowork/released sessions, a `.jsonl.zst` archive and a truncated one, a partial Codex tail, a damaged Claude transcript, an unrecorded billing route, injected secret fields and a locked WAL database) runs through the readers, worker, keyboard path and renderer in `tests/acceptance.rs` (`ac11_…`, `ac12_…`) and the native-world tests of `native_children`, `desktop_sessions`, `read_only`, `privacy`, `perf_budget` and `tui_flow`. They assert exact root pages of 50/100/125, direct per-member tokens and cost, child/fork placement, opening at the newest entry, stale-generation rejection, no planted secret or control sequence on any surface, and unchanged provider files with SQLite sidecars reported separately (ADR-0002).
- **Program smoke (Windows, debug build, ConPTY, explicit synthetic roots, `--no-processes`, bundled prices):** the real binary paged the 125 roots with `m`, expanded families, opened sessions at their newest entry, and used `/`, `f`, `s`/`S`, `p`, `?`, `Tab`, `←`/`→`, `J`/`K`, `d`, `v`, `D`, `r`, mouse click/wheel, resize and `q`; `q` and `Ctrl+c` sent 60 ms after launch (while indexing) exited as quickly as a no-op program under the same harness; one `netstat` sample showed no connection for the process; the raw terminal output held no planted secret or title-escape injection; a locked OpenCode database showed `OpenCode unavailable … retrying` beside listed Codex/Claude sessions and a missing Desktop directory produced an explicit warning; provider files were byte-identical afterwards (only the live database's `-shm` sidecar changed). Not exercised in that run: `x`, `n`/`N`, `g`/`G`, `h`/`l`, `i`, a real (non-ConPTY) terminal, and macOS/Linux.
- **OMP (synthetic fixtures):** `tests/omp_discovery.rs`, `omp_hydration.rs` and `omp_transcript.rs` cover the metadata, lifetime accounting and the paged reader (a 150-record multi-branch family opened at its true latest event and paged back through exact identities, details sharing one record/byte allowance, auxiliary purposes and compaction summaries, fork/continuation copies never counted twice, torn lines, appends to the newest and an earlier segment, truncation and replacement with stale-locator refusal, cancellation that keeps the other purposes' work, a damaged sibling, hostile text and private records); the OMP family, fork and a secret-bearing session also run through `World::native` and `World::with_omp` in `acceptance`, `read_only` (provider files and credential/history/model/statistics decoys byte-identical and never opened, no sidecar), `privacy`, `perf_budget` (a 20,000-record file probed by prefix/tail, hydrated and indexed in per-call-bounded slices) and `live_refresh` (append, replacement and indexing cancel through the real worker). **Program smoke (Windows, debug build, PTY, explicit synthetic roots, `--no-processes`):** the real binary listed the OMP family beside Codex/Claude/OpenCode, opened the parent at its newest entry, showed the session info, the source record and diagnostics, followed live appends made from outside (`1 new (G for newest)` while the inspector was open), and kept the healthy sessions selectable beside a garbage `.jsonl` and an unsupported `.jsonl.gz`. The harness could not send `Esc`, and the child/grandchild expansion, indexing cancel and replacement flows were exercised through the worker/event/render path (`TestBackend`) rather than in the live terminal.
- **Hermes and Pi (synthetic fixtures):** `tests/hermes_discovery.rs`, `hermes_lineage.rs`, `hermes_transcript.rs`, `hermes_usage.rs` cover DB shape detection, the read-only allowlist, compression chains and delegates, in-place and rotated compaction, display order, aggregate money and events, damaged/locked databases; `tests/pi_sessions.rs`, `pi_tree.rs`, `pi_transcript.rs` cover discovery, the tree/fork edge, active-branch display, owned lifetime usage, paging identities, cancellation and hostile records. Both also run through `World::with_all_harnesses` (the integrated six-harness world) in `acceptance`, `read_only`, `privacy`, `perf_budget` and `live_refresh`; their focused suites use `World::with_hermes`/`World::with_pi`. The six-harness smoke below exercised both on the real binary.
- **Six-harness program smoke (Windows, debug build, console pty, explicit synthetic roots, `--no-processes`, `--no-mouse`, bundled prices):** the real binary opened the integrated 125-root store (Codex, Claude Code, OpenCode, OMP with a family, Hermes chain/profile/delegate, Pi family/fork, Desktop Code/Cowork plus a released root). First page: `Codex 15  Claude 17  OpenCode 15  OMP 1  Hermes 2  Pi 0`, `Showing 50 of 50`, `Quiet 49 · Complete 1`. Root Load more through the keyboard focus row produced 100 and then the full generation; final counts `Codex 40  Claude 40  OpenCode 37  OMP 3  Hermes 2  Pi 3` (125 tracked), `Showing 124 of 125` because the released Desktop root is hidden by the default filter. Observed: a Pi row with `Unknown` cost/active/speed and no compaction section; a Hermes chain root with 240 owned tokens, `~$0.02 estimated`, `Compactions: 2 observed · cost Unknown · 2 unpriced`, and no request speed from its aggregate rows; a locked OpenCode database isolated as `OpenCode unavailable … retrying` beside healthy harnesses; the metadata ceiling (10 sources / 64 MiB) shown as `Showing 7 of 7` with per-harness "newest 10 sources" warnings; and a redacted Codex source view. A before/after inventory of 133 files left 130 byte-identical and changed only three SQLite `-shm` sidecars (OpenCode and two Hermes databases). Partial append, archive truncation and in-place replacement were also observed. The pty writer could not inject `Esc`/`Tab`/arrows, so child expansion, nested child, child Load more, timed-child speed, transcript navigation and three terminal sizes (60×20, 90×25, 160×40) ran through the real readers/worker/App/key/render path under a throwaway Ratatui `TestBackend` harness (deleted). Mouse behavior, terminal restoration beyond clean exits, and proprietary app behavior were not directly observed in that run; network observation was three 1-second `netstat` TCP samples plus one UDP sample for the running PID (connection-table sampling, not packet capture or OS tracing) and a static scan with no network API or crate.

## Known limitations

- **Codex archives and children:** the native `archived_sessions` store and `.jsonl.zst` rollouts are read in place as one logical session with their live copies; a child's copied parent history is not counted as its own usage, and a child without a trustworthy owned boundary is shown as partial/Unknown. Desktop/IDE origins are labeled only where `source`/`originator` distinguish them.
- **Not available, by design or not yet:** Claude Desktop chat history is not reconstructed (only Code session manifests and Cowork project stores that point at ordinary project transcripts are read); Codex, Claude Code, OpenCode, OMP, Hermes, or Pi data inside WSL distributions, on other machines, or in a cloud account is not discovered (no network access, no WSL path translation); **Pi extension subagents are unsupported** (the native format records no child sessions, so none are listed instead of a verified zero). A locked SQLite database that cannot be opened without a sidecar it may not create is shown as `Unavailable` and retried.
- **Roster scan is not metadata-only.** Codex and Claude Code files are streamed once in the initial pass (bounded per call, each byte read once); header/tail-only scanning is deferred. Appending to a large file re-reads the consumed prefix for an integrity check.
- **Benchmarks:** Windows only; OpenCode not benchmarked; Claude Code keeps one open handle per scanned file (a roster of more than a few hundred Claude files may hit the default macOS descriptor limit). Preview building can allocate source-sized temporaries for one record (bounded by the 1 MiB record cap).
- **Process matching is conservative.** Most live agents do not put their session ID on the command line, so most sessions show `Unmatched` or only same-provider *candidates*. On Windows the working directory of other processes is not read.
- **Cost is an estimate.** The bundled catalog is revision `2026-10-30.2` (schema version 1), checked 2026-10-30 against eight source pages: Anthropic pricing, OpenAI pricing, an OpenAI GPT-6.1 Sol model page, OpenCode Go, OpenCode Zen, plus Anthropic's model/deprecation and models-overview pages and OpenAI's model pages for context windows. See `src/pricing/catalog.json` for sources, per-row inventory and unmapped rows. A model, billing route, tier, region, TTL, or token counter the catalog cannot price stays `Unknown`; a partial subtotal is never shown as a total. Session-wide counters (Codex, OpenCode) are not individual requests, so models with a request-length tier or a time-of-day tariff stay `Unknown` at session level. Cache-write rates for OpenAI's 30m/12h retention are not mapped (a write with no recorded retention can be priced by a `cache_write` rate), and separately reported reasoning tokens need a card with an explicit `reasoning` rate.
- **Speed is mostly `Unknown`.** Throughput needs a recorded whole-request model duration above 100 ms and recorded output: OMP uses its recorded request duration, and Pi uses `durationMs` when the supported assistant record carries it. Codex and OpenCode expose session totals, Hermes records no request timing, and Claude Code records turn spans for some but not all turns, so its active-time coverage stays labeled incomplete. A rate keeps the model it was measured on, and weighted throughput always reports `measured/total`. Claude and Codex record compaction markers without a usage row, so those events stay `unpriced` rather than attributed to a nearby request.
- Codex `function_call` records are source-only (viewable with `v`); tool interactions come from completed thread items. Some uncommon provider part shapes are unverified.
- OpenCode: arrivals between opening a transcript and the first count can be missed by the "N new" indicator (content itself is not lost).
- **OpenCode model identity and child links are evidence-bound.** The billing route (`providerID`) and raw `variant` are shown as recorded; effort is also shown only for observed thinking-level spellings (`minimal`/`low`/`medium`/`high`/`xhigh`/`max`), and `default`/`none` stay raw variants with `Unknown` effort. A missing provider stays `Unknown`/unpriced, never a vendor default. A tool call links to a native child only from the documented metadata shapes (v2 `subagent`/`sessionID`, legacy `task`/`sessionId`), matched case-sensitively; an observed v2 database that records `task`/`sessionId` therefore shows no link rather than a guess.
- **OMP is read from recorded evidence only.** Active time and the live Hub meters are not persisted, so active time stays `Unknown` (only the summed recorded request time is shown), and speed/TTFT appear only for requests that recorded a duration. Effort and prompt occupancy are bound to the recorded active branch; a session whose active leaf cannot be resolved shows them as `Unknown` with a warning. A fork or continuation whose ancestors are not among the discovered sources cannot prove which requests it owns: those requests are not counted and the usage is labeled partial. Event timestamps order the transcript, so a record whose clock is out of order is placed by its timestamp. A continuation chain lists each segment's records (history a later segment copies appears again, uncounted). The metadata ceiling can forget an evicted OMP source until a later pass sees it again. Child links come from the artifact tree, not from a tool call: an OMP tool event never offers an open-child action.
- **Native subagents cover five harnesses.** Codex (`parent_thread_id`/`thread_spawn`), Claude Code (`subagents/agent-<id>.jsonl`), OpenCode (`parent_id` plus documented link metadata), OMP (artifact-tree directories plus header evidence), and Hermes (`parent_session_id`/delegate rows) expose nested children; Pi records no native child session, so a Pi family has none. Edges are shown only from persisted evidence; an orphan, cycle, reused ID, or child of an undiscovered parent stays unresolved rather than guessed.
- **Hermes is read through a fixed allowlist.** Compression chains, delegates, in-place and rotated compaction, and display order come from the allowlisted `sessions`, `messages`, and `session_model_usage` projections. A `task='compression'` ledger row is one aggregate money amount and is never split per event; `_compressed_summary` markers are event-scope evidence without invented before/after/duration. Request speed, live context, and per-event compaction cost stay `Unknown`. No Hermes tool record names a child session, so a tool call has no open-child action.
- **Pi is read from its standard JSONL tree.** Lifetime usage counts owned requests on the recorded active path; inherited forks and unprovable ownership are not charged and the coverage is labeled partial. Compaction and branch-summary markers are observed, and their cost is priced only when a retained record carries a complete amount. Pi records no session end or active interval, so `Ended`/active time stay `Unknown`; request speed appears only from a supported assistant `durationMs`, and tool durations are never model time. A fork edge is resolved only among discovered sources.
- **Search and paging are scoped.** Roster search, filters, and counts cover loaded roots and loaded children only; load the next 50 with `m` to widen the search. Transcript search covers only the loaded window. Loaded-only is a deliberate bound, not a claim that an unloaded session does not exist.
- No macOS or Linux validation yet; both are targets, not claims.

## Development

See [Product requirements](docs/PRODUCT.md), [Architecture](docs/ARCHITECTURE.md), and [Contributing](CONTRIBUTING.md). Use a stable Rust toolchain with `rustfmt` and `clippy`:

```bash
cargo fmt --all -- --check
cargo check --all-targets
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
```

## Security and privacy

Coding-agent logs may include private prompts, repository contents, paths, credentials, and untrusted terminal output. AgentScope neutralizes control sequences, masks common credential patterns by default (best effort — it cannot identify every secret), bounds record and page sizes, and uses synthetic fixtures only. Do not paste real transcripts into issues. In the inspector, `x` toggles masking for the selected record's source view (local only); nothing is ever transmitted. Redaction and read-only behavior are tested, not certified.

## Out of scope

A web dashboard, multi-device hub, cloud sync, MCP server, agent control, Hermes-specific integration, and LLM-based reviewer.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).
