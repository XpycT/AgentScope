# AgentScope usage reference

The full reference for running and configuring AgentScope. For installation and a quick tour, see the [README](../README.md).

## Command line

```text
agentscope [OPTIONS]
agentscope inspect <rollout.jsonl|rollout.jsonl.zst>
```

| Option | Meaning |
|---|---|
| `--config <path>` | Config file (default: platform config directory) |
| `--codex-sessions <dir>` | Codex sessions root |
| `--claude-projects <dir>` | Claude Code projects root |
| `--claude-desktop <dir>` | Claude Desktop application-data directory |
| `--opencode-db <path>` | OpenCode database |
| `--omp-sessions <dir>` | OMP sessions root |
| `--pi-sessions <dir>` | Pi sessions root (native JSONL; extension subagents unsupported) |
| `--hermes-db <path>` | Hermes `state.db` |
| `--no-processes` | Disable process observation |
| `--no-color` | Disable color (`NO_COLOR` is also honored) |
| `--no-mouse` | Disable mouse capture |
| `--version`, `--help` | Print version or help |

The TUI needs an interactive terminal on both stdin and stdout. Minimum size is 60×20.

### `inspect`

The experimental `inspect` command prints a Codex transcript from an explicitly supplied JSONL (or archived `.jsonl.zst`) file without the TUI:

```bash
agentscope inspect path/to/session.jsonl
```

It escapes terminal controls and applies best-effort masking, reports malformed, incomplete, or over-1-MiB records with byte offsets, and may still print sensitive session content.

## Screens

### Roster

The roster groups sessions by project; a group heading shows its loaded member totals (`N sessions · tokens · cost`), and a child always renders under its parent in the parent's group. Each session is two lines: a status glyph, fold marker and bold display title with right-aligned `STATUS`, `SPEED`, `TOKENS` and `COST`, then a quiet line with the agent tag (`OMP`, `CDX`, `CLD`, `OC`, `HRM`, `PI`; `[OMP]` in no-color mode), model, thinking level and billing provider. Narrow terminals drop `COST`, then `SPEED`, then `TOKENS`; `STATUS` always stays, and there is no horizontal column scrolling. Children are folded by default (`▸ Title +N`).

From 150 columns the preview takes 45 columns beside the list. Its pinned header shows the tag, title, project/model/effort/provider, status and evidence, tokens with provenance, cost, speed, and a stacked token bar with a legend; `J`/`K` or the wheel scroll `CONTEXT`, `STARTED` and `LAST ACTIVITY`. The last-activity block is read in the background and shows the latest tool line and reply, or an explicit `…`, `no activity recorded`, `no reply in recent events` or `couldn't read latest activity`. Narrower than 150 columns, `Tab` swaps list and preview and `Esc` returns.

The roster loads the newest **50 top-level groups** at a time; children, forks, and compression continuations never consume a root slot. `m` (or `Enter` on a focused Load more row) reveals the next 50. A load is a frozen generation: new discoveries, deletions, and late parent edges appear only after an explicit refresh, which keeps the selected session where it can. **Search and filters cover only the loaded roots** (and any loaded children, whose matching parent stays visible); an unloaded session is never reported as absent.

### Inspector

The inspector opens at the newest event with follow mode on: a three-line header, then conversation rows with `▌ You`/`▌ Agent`, tool rows (`✓` completed, `✗` error, `…` pending, `?` unknown), persisted reasoning summaries and folded auxiliary calls. `Enter` expands a row in place with recorded argument, output and reasoning previews ending `recorded preview · v opens the source`; while following, appended events move to and expand the latest row. `o` opens a resolved child's transcript and `Esc` pops back.

`d` opens event details: a badge header with time, relative age and `event N of M loaded`, `[`/`]` for the previous/next loaded event, and content beside `USAGE`/`COST`/`SOURCE` at 110 columns (stacked below). `i` opens session info: the same header and `TOKENS`, `COST`, `TIME & SPEED`, `CONTEXT`, `FAMILY` and `PROCESS` panels in two columns at 90 columns (stacked below), with `m` for older compactions and `Space` for summaries.

## Keys

| Screen | Keys |
|---|---|
| Roster | `j`/`k` move (and rest on explicit Load more rows), `g`/`G` first/last, `←`/`→` (`h`/`l`) collapse/expand (Left on a child selects its parent), `Space` show or hide a family's children, `Enter` open a session or load more on a focused Load more row, `m` load the next 50 roots, `/` search loaded roots, `f` filter, `s` sort (`S` reverse), `r` refresh, `p` process candidates, `?` help, `q` or `Ctrl+c` quit. On wide terminals `Tab` opens the inspector; on narrow ones it swaps list and preview. |
| Inspector | `j`/`k`/`g`/`G` move (`G`/`End` jumps to latest), `Enter` expand or collapse, `/` search loaded events, `n`/`N` next/previous match, `d` details, `i` session info, `o` open a resolved child, `v` source record, `D` diagnostics, `x` reveal or re-mask the selected record's source, `r` retry/refresh, `Esc` back (popping child navigation before the roster), `Shift+Tab` roster. |
| Details | `[`/`]` previous/next loaded event, `v` source, `D` diagnostics, `o` child, `Esc` back. |
| Session info | `m` older compactions, `Space` summaries. |

The help overlay (`?`) lists the main keys and the value, status and tool legend.

### Mouse

Capture is on by default and read-only. A click selects a row, a click on the marker expands it, a click on a Load more row loads that page, the wheel scrolls the list or the preview. Events that carry Shift are ignored, so Shift+drag (or your terminal's equivalent) still selects text. `--no-mouse` (or `mouse_enabled = false`) never enables capture and keeps every keyboard action. The terminal is restored on quit, `Ctrl+c`, setup failure and panic. Mouse clicks never act on monitored agents.

## Reading values

Compact views show `—` for unavailable values and `…` for values not read yet; `+` marks a partial total (a lower bound) and `~` an estimate or mtime-derived age; a failed read says `couldn't read`. Detail views spell Unknown/not recorded in words.

Status is a glyph, word and age, with its evidence in the preview and detail headers:

| Glyph | Status |
|---|---|
| `●` | active |
| `○` | recent |
| `!` | quiet |
| `▲` | stalled? (possibly stalled) |
| `✓` | complete |
| `✗` | failed |
| `×` | cancelled |
| `-` | unavailable |
| `?` | unknown |

Usage is labeled `Reported`, `Derived`, `Partial`, `Estimated`, or `Unknown`. Streamed requests are priced one by one with their own model; cost is an estimate and is `Unknown`, never `$0.00`, when it cannot be priced completely. A reported `$0` (e.g. a subscription) is shown as reported next to an API-equivalent estimate, never as free.

## Data locations

Each root resolves in this order: **CLI flag, then the provider's environment variable (where one exists), then `[providers.<harness>].root`/`db`, then the platform default**. Configured `extra_roots` are appended after the primary root.

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

## Configuration

The config file is `agentscope/config.toml` in the platform config directory, or the path given with `--config`:

| Platform | Default path |
|---|---|
| Windows | `%APPDATA%\agentscope\config.toml` |
| macOS | `~/Library/Application Support/agentscope/config.toml` |
| Linux | `$XDG_CONFIG_HOME/agentscope/config.toml` or `~/.config/agentscope/config.toml` |

An unreadable explicit path is an error, while a missing default file is fine. The config file is never rewritten for you.

```toml
# Global keys must come before the first table: TOML assigns later keys to the
# table they follow.
native_api_reference_estimates = true   # label native Claude/Codex usage as a reference API estimate when no route is recorded
max_cached_sessions = 2000

[providers.codex]
root = "D:/stores/codex/sessions"       # CLI --codex-sessions wins over this
archived_root = "D:/stores/codex/archived_sessions"

[providers.hermes]
root = "D:/stores/hermes"               # a home: profiles/<name>/state.db is also read
```

Other keys and their defaults:

| Key | Default |
|---|---|
| `refresh_ms` | 2000 |
| `process_ms` | 5000 |
| `process_enabled` | true |
| `quiet_after_secs` | 300 |
| `stall_after_secs` | 900 (must exceed quiet) |
| `max_cached_sessions` | 2000 |
| `max_metadata_sources` | 100000 |
| `max_metadata_bytes` | 67108864 |
| `max_page_events` | 500 |
| `max_page_bytes` | 262144 |
| `max_record_bytes` | 1048576 |
| `dedup_ledger_entries` | 100000 |
| `dedup_ledger_bytes` | 8388608 |
| `refresh_busy_timeout_ms` | 2000 |
| `page_busy_timeout_ms` | 250 |
| `redaction` | true |
| `mouse_enabled` | true |
| `native_api_reference_estimates` | true |

Zero is rejected for every numeric limit. `--no-processes` and `--no-mouse` override config; `NO_COLOR` and `--no-color` disable color. Unknown keys are rejected. Removed keys fail with the replacement name instead of being guessed: `max_sessions` → `max_cached_sessions` and `[[pricing]].provider` → `billing_provider`.

### Pricing

Prices come from an offline catalog compiled into the binary; nothing is fetched at runtime. The catalog is keyed by billing route and exact model, and every amount it produces is an **API-equivalent estimate**, never billed spend. Only two recorded amounts are retained: a provider-reported API amount (`ReportedApi`; a reported zero stays reported and only a positive amount overrides the API-equivalent estimate) and a provider-saved estimate whose source is verified (`SavedCatalogEstimate`). An unverified saved amount, a subscription value, or a parent's quoted child cost is not counted as money.

Optional `[[pricing]]` entries replace the bundled card with the same exact `billing_provider` (the route that bills the model, e.g. `anthropic`, `openai`, `opencode`, `opencode-go`) and `model`; legs you omit stay unknown instead of inheriting bundled rates:

```toml
[[pricing]]
billing_provider = "anthropic"
model = "claude-sonnet-5-5"
as_of = "2026-10-01"
input_per_mtok = 2.0
output_per_mtok = 10.0
cache_read_per_mtok = 0.10
cache_write_5m_per_mtok = 2.5
cache_write_1h_per_mtok = 4.0
```

`[[pricing]]` requires `billing_provider`, `model`, `input_per_mtok`, and `output_per_mtok`; optional `effective_from`/`effective_until` bound a card's window, `cached_input_per_mtok` is an accepted alias for `cache_read_per_mtok` (the two may not disagree), and `cache_write_per_mtok` prices a write whose retention was not recorded. Separately reported reasoning tokens need `reasoning_per_mtok`; reasoning already counted inside output is never charged twice.

## Provider notes

**Claude Code and Claude Desktop.** Subagents (`<project>/<session>/subagents/**/agent-<id>.jsonl`) appear as children of their session with their own direct usage; workflow `journal.jsonl` and tool-result files are never listed as sessions. Claude Desktop Code sessions reuse the same project store: `--claude-desktop <dir>` (or `[providers.claude_desktop]` `root`/`extra_roots`) names the Desktop application-data directory whose allowlisted `claude-code-sessions/**/local_*.json` manifests and Cowork project roots are read; without it the platform defaults are used when present. Only those stores are opened: credentials, renderer caches, session audit logs and VM bundles are not read, and no local chat history is reconstructed. Sessions a Desktop manifest marks released are hidden by default behind the filter panel's "Include released sessions" row and are never deleted.

**OMP (oh-my-pi).** Sessions (`<root>/<working-directory bucket>/<timestamp>_<id>.jsonl`) and their artifact-tree subagents (`<session>/<agent>.jsonl`, nested to any supported depth) are read from `--omp-sessions` (or `[providers.omp]` `root`/`extra_roots`). A child is linked from the artifact directory shape plus the header evidence, a fork (`parentSession` without `continuationOf`) stays its own root, and a continuation chain is one logical session whose segments are read together. Lifetime usage counts every uniquely *owned* model call across all recorded branches; copies a fork or continuation repeats are never charged again, and a parent's `task` result that quotes a child's cost is not added to the parent. Current model, effort and prompt occupancy follow the recorded active branch; whole-request speed uses the recorded request duration. OMP persists no usage for a compaction's summarization call, so **compaction cost is Unknown**. Only the sessions tree is opened: credential, configuration, history, model and statistics stores, blob directories and arbitrary embedded paths are never read, and `.jsonl.gz` archives are reported as unsupported.

## Performance bounds

Discovery, hydration, transcript indexing, and count scans are sliced at **256 records, 8 MiB of expanded bytes, and a 100 ms scheduling target** (the time limit is checked between units of work, so one huge record can exceed it); discovery probes a 4 KiB head (header line up to 256 KiB) and a 32 KiB tail per source. A transcript/details page is at most 500 events and 256 KiB, a record 1 MiB, the loaded transcript 20,000 events with at most 16 pending loads, and the per-source ownership/dedup ledger is capped by the configured entries/bytes. A session's canonical identity is `(harness, native ID, source)`; a compressed or archived physical copy is a locator on that identity, so one logical rollout is one row and a moved copy is not a second session. Metadata ceilings print `Discovery limited`/per-harness warnings instead of pretending the roster ended.
