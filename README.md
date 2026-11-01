# AgentScope

**Read-only observability for AI coding agents.**

AgentScope is a local, keyboard-first terminal application that discovers **Codex CLI**, **Claude Code**, **OpenCode**, **OMP (oh-my-pi)**, **Hermes**, and **native Pi** sessions on one machine, lets you read what each session did, and reports token usage, estimated cost, and health only with visible evidence — without changing how those agents run.

**Project status: v1 implemented (pre-release).** All six readers (Codex, Claude Code, OpenCode, OMP, Hermes and native Pi), the transcript explorer, usage accounting, process candidates, and the TUI are implemented and covered by synthetic-fixture tests. Native child sessions and forks (Codex, Claude Code, OpenCode, OMP, Hermes) and local Claude Desktop Code/Cowork stores are implemented and were exercised end to end on synthetic stores (see Verification status). Native Pi is read from its standard JSONL tree format; Pi extension subagents are explicitly unsupported. Validation against real installations is partial; see [Verification status](#verification-status) and [Known limitations](#known-limitations) before relying on it.

## What it does

- **Unified session roster** for Codex, Claude Code, OpenCode, and OMP, with search, filter, and sort.
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

Roster keys: `j`/`k` move (and rest on explicit Load more rows), `g`/`G` first/last, `Enter` open a session or load more on a focused Load more row, `Space` show or hide a session family's children, `/` search, `n`/`N` next/previous match, `f` filter, `s` sort (`S` reverse), `r` refresh, `p` process candidates, `?` help, `q` or `Ctrl+c` quit. Inspector keys: `j`/`k`/`g`/`G` move, `Enter` expand or collapse, `/` search loaded events, `n`/`N` next/previous match, `d` details, `v` source record, `D` diagnostics, `x` toggle masking in source views, `r` retry/refresh, `Esc` back.

Default data locations are the providers' own session stores (Codex sessions directory, Claude Code projects directory, OpenCode database, OMP sessions directory `~/.omp/agent/sessions`); override them with the flags above or the config file (`agentscope/config.toml` in the platform config directory, or `--config`). `--hermes-db` selects one Hermes `state.db`; a Hermes home additionally discovers `profiles/<name>/state.db`. `--pi-sessions` (and the matching `[providers.pi]` roots) are accepted and resolved by the configuration but have no reader yet.

Mouse: capture is on by default and read-only. A click selects a row, a click on the marker expands it, a click on a Load more row loads that page, the wheel scrolls the list or the preview. Events that carry Shift are ignored, so Shift+drag (or your terminal's equivalent) still selects text. `--no-mouse` (or `mouse_enabled = false`) never enables capture and keeps every keyboard action. The terminal is restored on quit, `Ctrl+c`, setup failure and panic. Mouse clicks never act on monitored agents.

Claude Code subagents (`<project>/<session>/subagents/**/agent-<id>.jsonl`) appear as children of their session with their own direct usage; workflow `journal.jsonl` and tool-result files are never listed as sessions. Claude Desktop Code sessions reuse the same project store: `--claude-desktop <dir>` (or `[providers.claude_desktop]` `root`/`extra_roots`) names the Desktop application-data directory whose allowlisted `claude-code-sessions/**/local_*.json` manifests and Cowork project roots are read; without it the platform defaults are used when present. Only those stores are opened: credentials, renderer caches, session audit logs and VM bundles are not read, and no local chat history is reconstructed. Sessions a Desktop manifest marks released are hidden by default behind the filter panel's "Include released sessions" row and are never deleted.

OMP (oh-my-pi) sessions (`<root>/<working-directory bucket>/<timestamp>_<id>.jsonl`) and their artifact-tree subagents (`<session>/<agent>.jsonl`, nested to any supported depth) are read from `--omp-sessions` (or `[providers.omp]` `root`/`extra_roots`). A child is linked from the artifact directory shape plus the header evidence, a fork (`parentSession` without `continuationOf`) stays its own root, and a continuation chain is one logical session whose segments are read together. Lifetime usage counts every uniquely *owned* model call across all recorded branches (assistant messages and `model_usage` entries, with their purpose); copies a fork or continuation repeats are never charged again, and a parent's `task` result that quotes a child's cost is not added to the parent. Current model, effort and prompt occupancy follow the recorded active branch; whole-request speed uses the recorded request duration. OMP persists no usage for a compaction's summarization call, so **compaction cost is Unknown**; before/after tokens, method and the recorded summary are shown. Only the sessions tree is opened: the agent directory's credential, configuration, history, model and statistics stores, blob directories and arbitrary embedded paths are never read, and `.jsonl.gz` archives are reported as unsupported.

Prices come from an offline catalog compiled into the binary; nothing is fetched at runtime. Optional `[[pricing]]` entries replace the bundled card with the same exact `billing_provider` (the route that bills the model, e.g. `anthropic`, `openai`, `opencode`, `opencode-go`) and `model`; legs you omit stay unknown instead of inheriting bundled rates:

```toml
native_api_reference_estimates = true   # price native Claude/Codex usage at the vendor API rate when no route is recorded
max_cached_sessions = 2000              # replaces the old `max_sessions`

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

An old `[[pricing]].provider` or `max_sessions` key is rejected with the replacement name; the file is never rewritten for you.

The experimental `inspect` command prints a Codex transcript from an explicitly supplied JSONL file without the TUI:

```bash
cargo run -- inspect path/to/session.jsonl
```

It escapes terminal controls and applies best-effort masking, reports malformed, incomplete, or over-1-MiB records with byte offsets, and may still print sensitive session content.

## Design principles

**No interference.** AgentScope does not start, stop, signal, or approve coding agents and does not modify their session stores, configuration, permissions, or credentials. **Evidence over guesses.** Missing logs, ambiguous process matches, and unknown costs remain explicitly unknown. **Local-first.** No network listeners, telemetry, or uploads.

## Read-only guarantee and its one exception

AgentScope opens Codex and Claude Code transcripts for reading only, and opens the OpenCode SQLite database read-only behind an authorizer that permits only allowlisted session tables and columns and denies credential, account, share-URL, write, and schema operations. It never reads `auth.json`, credential stores, or other provider state, and never repairs, deletes, chmods, or copies provider files.

**Exception ([ADR-0002](docs/decisions/0002-opencode-readonly-wal-sidecars.md)):** OpenCode uses SQLite WAL mode. SQLite itself may create or update its transient `-shm`/`-wal` sidecar files next to the database when a read-only connection reads a live WAL database. Those are SQLite runtime files, not session content, but they are writes in the provider directory, so the provider directory is **not** guaranteed byte-for-byte untouched while OpenCode is running. If the directory is not writable and a live WAL cannot be read without a sidecar, OpenCode is shown as `Unavailable` and retried; permissions are never changed.

## Verification status

Run on Windows 11 (rustc/cargo 1.99.1) with provider versions codex-cli 0.159.2, Claude Code 2.1.311, and opencode 2.0.29 installed:

- `cargo fmt --all -- --check`, `cargo check --all-targets`, `cargo clippy --all-targets -- -D warnings`, `cargo test`, and `cargo build --release` pass.
- **Fixture-backed (synthetic) acceptance** for AC-01..AC-10 in `tests/acceptance.rs`, read-only regression in `tests/read_only.rs` (session files, the main database, mtimes, and permissions unchanged; only `-shm`/`-wal` sidecars may appear), and privacy in `tests/privacy.rs` (sentinel secrets absent from Debug output, warnings, errors, and rendered screens; control sequences neutralized; credential-shaped text masked).
- **Real-data smoke (read-only, counts only):** `cargo test --test acceptance -- --ignored --nocapture real_data_smoke` read this developer's real Codex, Claude Code, and OpenCode stores through the adapters (discovery and transcript page reading only) and compared a content-hash inventory before and after. Changes that coincide with a length or mtime change, new files, and removals are reported as inconclusive (a live provider or a SQLite sidecar); only a same-length, same-mtime content change fails. The last run: 2058 files, 2057 identical, 1 inconclusive (concurrent change), 0 silent changes. It prints counts only, never content.
- **Not verified:** an interactive TUI session on real data (the harness has no interactive terminal; the keyboard path is covered by `tests/tui_flow.rs` and the acceptance flow on fixtures), anything on macOS or Linux, and live-append against a real running agent (covered on synthetic fixtures only).
- Performance: deterministic work-budget tests plus informational timings in [the benchmark note](docs/superpowers/notes/2026-10-30-benchmarks.md). Measured on Windows only.
- **Integrated native closure (synthetic fixtures):** one 125-root store (nested Codex/Claude/OpenCode families and forks, Claude Desktop Code/Cowork/released sessions, a `.jsonl.zst` archive and a truncated one, a partial Codex tail, a damaged Claude transcript, an unrecorded billing route, injected secret fields and a locked WAL database) runs through the readers, worker, keyboard path and renderer in `tests/acceptance.rs` (`ac11_…`, `ac12_…`) and the native-world tests of `native_children`, `desktop_sessions`, `read_only`, `privacy`, `perf_budget` and `tui_flow`. They assert exact root pages of 50/100/125, direct per-member tokens and cost, child/fork placement, opening at the newest entry, stale-generation rejection, no planted secret or control sequence on any surface, and unchanged provider files with SQLite sidecars reported separately (ADR-0002).
- **Program smoke (Windows, debug build, ConPTY, explicit synthetic roots, `--no-processes`, bundled prices):** the real binary paged the 125 roots with `m`, expanded families, opened sessions at their newest entry, and used `/`, `f`, `s`/`S`, `p`, `?`, `Tab`, `←`/`→`, `J`/`K`, `d`, `v`, `D`, `r`, mouse click/wheel, resize and `q`; `q` and `Ctrl+c` sent 60 ms after launch (while indexing) exited as quickly as a no-op program under the same harness; one `netstat` sample showed no connection for the process; the raw terminal output held no planted secret or title-escape injection; a locked OpenCode database showed `OpenCode unavailable … retrying` beside listed Codex/Claude sessions and a missing Desktop directory produced an explicit warning; provider files were byte-identical afterwards (only the live database's `-shm` sidecar changed). Not exercised in that run: `x`, `n`/`N`, `g`/`G`, `h`/`l`, `i`, a real (non-ConPTY) terminal, and macOS/Linux.
- **OMP (synthetic fixtures):** `tests/omp_discovery.rs`, `omp_hydration.rs` and `omp_transcript.rs` cover the metadata, lifetime accounting and the paged reader (a 150-record multi-branch family opened at its true latest event and paged back through exact identities, details sharing one record/byte allowance, auxiliary purposes and compaction summaries, fork/continuation copies never counted twice, torn lines, appends to the newest and an earlier segment, truncation and replacement with stale-locator refusal, cancellation that keeps the other purposes' work, a damaged sibling, hostile text and private records); the OMP family, fork and a secret-bearing session also run through `World::native` and `World::with_omp` in `acceptance`, `read_only` (provider files and credential/history/model/statistics decoys byte-identical and never opened, no sidecar), `privacy`, `perf_budget` (a 20,000-record file probed by prefix/tail, hydrated and indexed in per-call-bounded slices) and `live_refresh` (append, replacement and indexing cancel through the real worker). **Program smoke (Windows, debug build, PTY, explicit synthetic roots, `--no-processes`):** the real binary listed the OMP family beside Codex/Claude/OpenCode, opened the parent at its newest entry, showed the session info, the source record and diagnostics, followed live appends made from outside (`1 new (G for newest)` while the inspector was open), and kept the healthy sessions selectable beside a garbage `.jsonl` and an unsupported `.jsonl.gz`. The harness could not send `Esc`, and the child/grandchild expansion, indexing cancel and replacement flows were exercised through the worker/event/render path (`TestBackend`) rather than in the live terminal.

## Known limitations

- **Codex archives and children:** the native `archived_sessions` store and `.jsonl.zst` rollouts are read in place as one logical session with their live copies; a child's copied parent history is not counted as its own usage, and a child without a trustworthy owned boundary is shown as partial/Unknown. Desktop/IDE origins are labeled only where `source`/`originator` distinguish them.
- **Not available, by design or not yet:** Claude Desktop chat history is not reconstructed (only Code session manifests and Cowork project stores that point at ordinary project transcripts are read); Codex, Claude Code, OpenCode, OMP or Pi data inside WSL distributions, on other machines, or in a cloud account is not discovered (no network access, no WSL path translation); **Pi extension subagents are unsupported** (the native format records no child sessions, so none are listed instead of a verified zero) and Pi request speed is Unknown unless an assistant record carries `durationMs` (the producer records no session end or active interval). A locked OpenCode WAL database that SQLite cannot open without a sidecar it may not create is shown as `Unavailable` and retried.
- **Roster scan is not metadata-only.** Codex and Claude Code files are streamed once in the initial pass (bounded per call, each byte read once); header/tail-only scanning is deferred. Appending to a large file re-reads the consumed prefix for an integrity check.
- **Benchmarks:** Windows only; OpenCode not benchmarked; Claude Code keeps one open handle per scanned file (a roster of more than a few hundred Claude files may hit the default macOS descriptor limit). Preview building can allocate source-sized temporaries for one record (bounded by the 1 MiB record cap).
- **Process matching is conservative.** Most live agents do not put their session ID on the command line, so most sessions show `Unmatched` or only same-provider *candidates*. On Windows the working directory of other processes is not read.
- **Cost is an estimate.** The bundled catalog covers only rows evidenced by four official pricing pages as checked on 2026-10-30 (see `src/pricing/catalog.json`: sources, per-row inventory, unmapped rows). A model, billing route, tier, region, or token counter the catalog cannot price stays `Unknown`; a partial subtotal is never shown as a total. Session-wide counters (Codex, OpenCode) are not individual requests, so models with a request-length tier or a time-of-day tariff stay `Unknown` at session level. Cache-write rates for OpenAI (30m/12h) are not mapped, and separately reported reasoning tokens need a card with an explicit `reasoning` rate.
- **Speed is mostly `Unknown`.** No native reader currently extracts a whole-request model duration (Codex item durations are tool time, and OpenCode/Codex expose session totals), so throughput is shown only where a request measurement exists. Claude Code records turn spans but not for every turn, so its active-time coverage stays labeled incomplete. Claude and Codex record compaction markers without a usage row, so those events stay `unpriced` rather than attributed to a nearby request.
- Codex `function_call` records are source-only (viewable with `v`); tool interactions come from completed thread items. Some uncommon provider part shapes are unverified.
- OpenCode: arrivals between opening a transcript and the first count can be missed by the "N new" indicator (content itself is not lost).
- **OpenCode model identity and child links are evidence-bound.** The billing route (`providerID`) and raw `variant` are shown as recorded; effort is also shown only for observed thinking-level spellings (`minimal`/`low`/`medium`/`high`/`xhigh`/`max`), and `default`/`none` stay raw variants with `Unknown` effort. A missing provider stays `Unknown`/unpriced, never a vendor default. A tool call links to a native child only from the documented metadata shapes (v2 `subagent`/`sessionID`, legacy `task`/`sessionId`), matched case-sensitively; an observed v2 database that records `task`/`sessionId` therefore shows no link rather than a guess.
- **OMP is read from recorded evidence only.** Active time and the live Hub meters are not persisted, so active time stays `Unknown` (only the summed recorded request time is shown), and speed/TTFT appear only for requests that recorded a duration. Effort and prompt occupancy are bound to the recorded active branch; a session whose active leaf cannot be resolved shows them as `Unknown` with a warning. A fork or continuation whose ancestors are not among the discovered sources cannot prove which requests it owns: those requests are not counted and the usage is labeled partial. Event timestamps order the transcript, so a record whose clock is out of order is placed by its timestamp. A continuation chain lists each segment's records (history a later segment copies appears again, uncounted). The metadata ceiling can forget an evicted OMP source until a later pass sees it again. Child links come from the artifact tree, not from a tool call: an OMP tool event never offers an open-child action.
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
