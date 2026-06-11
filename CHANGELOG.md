# Changelog

All notable changes to bookrack are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project
follows [semver](https://semver.org/spec/v2.0.0.html). Each release
section is the source of truth for the GitHub Release notes — the
release workflow extracts the matching section verbatim from this file.

## [Unreleased]

### Added

- `bookrack doctor --install-pdfium`: downloads the pinned PDFium
  build, verifies its SHA-256 against the pin, and unpacks the
  library into a per-user managed directory that the loader searches
  automatically. The first-run wizard offers the same download when
  the library is missing.

- `metadata.set` / `metadata.clear` accept a `reason` that lands on the
  audit row, closing the gap where field edits — unlike approve / ack /
  reject — could never record their justification. The CLI and REPL
  expose it as an optional `--reason` flag; the MCP write tools require
  it, so an LLM edit always carries its rationale in the audit trail.

- `show_book` (and the `metadata.show` report that embeds it) lists the
  book's active overrides — which fields are curated rather than
  extracted, by whom and when, with notes — alongside the merged
  `effective_biblio`, so curation state is visible without replaying
  the audit trail.

- `metadata.void` (CLI/REPL `bookrack metadata void`, MCP
  `library.metadata.void`): suppress a field's extracted value with a
  NULL override when the value is known wrong and no correct value is
  at hand — the field reads as absent until one is set. `metadata.clear`
  removes the suppression. The MCP tool requires a `reason`.

### Changed

- The PDFium library search is now a chain: `BOOKRACK_PDFIUM_LIB`
  (authoritative when set), then the executable's own directory,
  then the per-user managed directory. A miss reports every searched
  directory and the remedies instead of a raw dynamic-loader error.

- `metadata.set` validates the field name against the curator-editable
  bibliographic set and rejects unknown names, instead of silently
  creating an override row no consumer ever reads; the error carries
  the full editable list. Pipeline-owned bookkeeping columns
  (`source`, `confidence`, `audit_verdict`, `source_format`,
  `enriched_by`) are no longer overridable. `metadata.clear` accepts a
  name outside the set only when a stale override row with that key
  exists, so pre-validation rows stay removable.

### Fixed

- `vectors reset` (including `--resume`) and `vectors reembed` append
  pipeline-audit rows for the chunk and embed stages they run, so a
  book's trail no longer ends on a stale failure after a maintenance
  pass re-embedded it successfully. Rows carry `actor_detail` `reset` /
  `reembed` and share one `reset-`/`reembed-`-prefixed run id per
  invocation.
- Metadata edits arriving over MCP are attributed to `actor_kind=llm`
  / `actor_detail=mcp` on their audit rows and write outcomes, as the
  tool descriptions promise. They previously inherited the caller the
  daemon was launched with (`human`/`cli` under `bookrack run`,
  `human`/`gui` under the tray app); headless `bookrack-mcp` had the
  reverse mislabel, recording control-socket edits as `llm`/`mcp`.

## [0.4.0] - 2026-06-11

### Added

- JSON-RPC control plane: the daemon serves a control socket, and
  every other process is a client of it. The method surface covers
  the write commands with progress events, queue lifecycle
  (`queue.pause` / `queue.resume` / `queue.clear`), `verify.run`,
  `library.fork`, `diagnose.run`, introspection (`daemon.methods`,
  `daemon.mcp_tools`), and a subscribable log/event channel.
- `bookrack repl`: standalone control-socket client process. Reedline
  runs in the client; every command is dispatched as a JSON-RPC call
  over the daemon's control plane. The prompt shows a live
  `[<state-indicator>bookrack:<library>/queue:<n>] >` status line
  driven by `events.subscribe`. When stdin is not a TTY the client
  runs a batch loop: each line is parsed via the shared
  `bookrack-repl-grammar` and dispatched in sequence, and the
  process exits non-zero on the first RPC failure. When the daemon
  is not running the client exits with code 2 and the message
  `bookrack daemon not running; start it with: bookrack run`.
- `library.read_context` and `library.read_span` MCP tools:
  structural passage reads around a hit, backed by a document-order
  `leaves_in_doc_span` corpus query, so an agent can widen a citation
  to its surroundings without a second search.
- Search citations carry the intake id and structural anchors of the
  cited span.
- `crates/app`: Tauri shell hosting the daemon in-process with a
  minimal Svelte 5 panel — groundwork for the tray GUI, not yet part
  of the release artifacts. Control-plane DTOs and events derive
  ts-rs schemas, generated as `.ts` files for the webview at test
  time.
- `WizardDriver` trait behind the first-run wizard, with the CLI
  driver as the first implementation and room for a GUI driver.
- The daemon raises its soft `RLIMIT_NOFILE` to the hard limit at
  startup, and `bookrack doctor` reports the effective limit. A
  GUI-launched daemon previously inherited a 256-fd default and
  starved mid-batch on LanceDB fragment files.

### Changed

- `bookrack run` defaults to the silent daemon: it no longer reads
  stdin, no longer spawns reedline, and emits no banner. Open an
  interactive REPL with `bookrack repl` in another process.
- One-shot subcommands are rewritten as control-plane clients and
  dispatch their work to the running daemon over the control socket.
- A second `bookrack run` against a live session brings the existing
  daemon to the front and exits cleanly; `bookrack-mcp` gains the
  same re-entry behaviour.
- The ingest queue pauses on process-level failures instead of
  rapidly failing every remaining job; the error is the process's,
  not the books'.
- Ingest error strings keep their flattened source chains, so root
  causes such as `Too many open files` reach the pipeline audit
  instead of an opaque `vector store error`.

### Deprecated

- `bookrack run --legacy-repl`: one-release transition flag (hidden
  from `--help`) that re-enables the in-process reedline REPL for CI
  scripts that fed it via stdin. The flag will be removed in the
  next release after this one; migrate to `bookrack repl` (which
  accepts stdin in batch mode for the same scripted use case).

### Fixed

- The RESET confirmation on destructive vector-store commands
  survives the control-plane client split.
- The daemon process exits after a control-plane shutdown request
  instead of lingering, and the `--legacy-repl` path prints its
  session banner again.
- The hint printed after `library.fork` names the correct
  model-switch command.

## [0.3.0] - 2026-06-08

### Added

- `bookrack run`: single-process daemon-REPL. Acquires the machine-wide
  session lock, serves MCP over streamable-HTTP, hosts a reedline REPL
  on the foreground tty, and drives a persistent ingest queue worker.
- `bookrack exec`: out-of-process discovery surface. Reads the session
  lock without opening any database; subcommands `info`, `tools`,
  `library.<tool>`, `logs {tail,follow}` reach the live daemon over
  MCP and SSE.
- `session.*` MCP tool family: `session.info`, `session.queue_status`,
  `session.logs_tail`, `session.shutdown`. Lets agent clients query
  daemon state, follow logs, and request a graceful stop without
  touching the host process directly.
- `/session/logs` SSE endpoint on the MCP axum router for live log
  streaming with a 15-second keep-alive.
- Service-architecture log pipeline: file + broadcast ring buffer +
  SSE. `LogConfig.console_level` controls the console layer
  independently of the file layer; the headless `bookrack-mcp` binary
  mirrors the file directive to the console for systemd / journalctl.
- `bookrack vectors reset` and `bookrack libraries fork`: in-place
  swap of the embedding model with a clean rebuild of the vector store
  or a copy-on-write side-by-side library.
- `bookrack run` on a fresh install offers the setup wizard inline
  when no library is configured. Non-interactive callers still get the
  resolver error and a pointer at `bookrack init`.
- `bookrack run` runs headless when stdin is not a tty. Shutdown is
  driven by signal handlers or the `session.shutdown` MCP tool.
- `bookrack run` prints a five-line summary (pid, MCP address,
  inspect-with, stop-with) when the session lock is already held, and
  waits for Enter on an interactive tty so a launcher window does not
  vanish.
- Double-click launchers for all three release platforms:
  `Bookrack.app` on macOS (ad-hoc signed, opens Terminal.app on the
  Resources/bookrack-run.command stub), `Start Bookrack.cmd` on
  Windows (runs `bookrack.exe run`, pauses on error), and
  `bookrack.desktop` on Linux (asks the desktop environment to host
  the daemon in its native terminal emulator).

### Changed

- The CLI surface is now eight external subcommands: `run`, `exec`,
  `init`, `doctor`, `verify`, `diagnose`, `libraries`,
  `audit-profile`. Every write command (`ingest`, `metadata.*`,
  `vectors.*`, `corpus rebuild`, `intake ocr`) moves into the
  `bookrack run` REPL; every read command (`books`, `find`,
  `metadata show`, `pipeline-trail`, `vectors status`, `info`) moves
  behind `bookrack exec library.<tool>`. Hard guarantee: every write
  goes through the in-process scheduler, so no two processes ever
  touch the catalog or vector store at the same time.
- macOS release tarball now contains `Bookrack.app` rather than a
  flat layout. Windows and Linux keep the flat layout.

### Fixed

- Queue worker pulls ingest jobs through the in-process
  `LibraryRegistry`, so a `--force` reingest no longer needs an
  external `bookrack` invocation.
- Signal-driven shutdown (SIGINT / SIGTERM / SIGHUP) exits the
  process within ~6s instead of parking on the reedline blocking
  thread.
- `Library.store` refreshes after every successful ingest, so a
  newly-ingested book is searchable in the same daemon session.

## [0.2.0] - 2026-06-07

### Added

- `bookrack run`: daemon-REPL skeleton. Acquires the session tty
  lock, listens for signals, and serves MCP over streamable-HTTP on
  the foreground process. The REPL hosts every write command moved
  off the external CLI.
- `bookrack exec`: out-of-process discovery surface that reads the
  session lock to find the live daemon and calls MCP tools over HTTP.
- `LibraryRegistry`: in-process scheduler that owns the only
  `Catalog`, `Corpus`, and vector-store handles. MCP and the REPL
  both route through it; `bookrack-mcp` opens its own scheduler when
  run standalone.
- Persistent ingest queue under `<data_root>/.bookrack-queue.json`.
  Jobs survive across sessions and resume on the next `bookrack run`.
- TXT chapter-heading detection across Sinographic, Latin, and
  Germanic families; rules live under `<data_root>/audit-rules/`.
- `library.list_metadata` and `library.vectors_status` read MCP
  tools.
- `bookrack audit-profile` top-level command for inspecting and
  comparing audit profiles.

### Changed

- The external CLI shrinks to nine commands: every write command
  (`ingest`, `metadata.*`, `corpus rebuild`, `vectors.*`, `intake
  ocr`, `dryrun`) moves into the REPL grammar of `bookrack run`;
  every library read command moves behind `bookrack exec
  library.<tool>`.
- HTML / quality / language thresholds previously hard-coded in
  `bookrack-extract` are now externalised to `headings.toml` and
  `audit_data.toml`. The extractor version stamp bumps accordingly.
- `bookrack-mcp` acquires the session tty lock too, so a standalone
  MCP daemon and a `bookrack run` daemon cannot start in the same
  data root.

### Fixed

- `Library.store` refreshes after each ingest so a newly-ingested
  book is searchable inside the same daemon session.
- Signal-driven shutdown no longer parks on the reedline blocking
  thread; the process exits within seconds of SIGINT / SIGTERM /
  SIGHUP.
- Broken-pipe panics from the obs layer silently exit instead of
  printing a stack trace.

## [0.1.0-rc4] - 2026-06-05

Re-cut of `v0.1.0-rc3` to drop the `x86_64-apple-darwin` build target.
No behaviour change in the shipped binaries.

### Removed

- Native `x86_64-apple-darwin` release tarball. `lance-linalg`'s build
  script hard-codes `-march=native` for its AVX-512 `dist_table`
  kernel, which is incompatible with cross-compiling on the Apple
  Silicon runner, and the previously-used `macos-13` Intel runner pool
  is being deprecated. Intel macOS users run the
  `aarch64-apple-darwin` binary under Rosetta 2 instead.

## [0.1.0-rc3] - 2026-06-05

Re-cut of `v0.1.0-rc2` to harden the release pipeline. No behaviour
change in the shipped binaries.

### Changed

- Release workflow: `x86_64-apple-darwin` is now cross-compiled on the
  `macos-14` Apple Silicon runner instead of running natively on
  `macos-13`. GitHub's Intel macOS runner pool is being deprecated and
  was queuing for hours.
- Release workflow: `windows-latest` pinned to `windows-2025-vs2026`,
  pre-empting GitHub's mid-June 2026 implicit redirect.
- Release workflow: protoc is installed inline from
  `protocolbuffers/protobuf` GitHub releases instead of via
  `arduino/setup-protoc`. The third-party action has been unmaintained
  since early 2024 and still runs on Node 20.

## [0.1.0-rc2] - 2026-06-05

Re-cut of `v0.1.0-rc1` to fix the release pipeline on Windows. No
behaviour change in the shipped binaries.

### Fixed

- Release workflow: SHA-256 verification of the PDFium archive and the
  packaged release tarball now picks `sha256sum` when present and falls
  back to `shasum -a 256`. Git Bash on Windows runners has no `shasum`,
  which broke the Windows job at the verification step.

## [0.1.0-rc1] - 2026-06-05

First release candidate. Pre-release while pre-production hardening
(schema migrations, approximate-nearest-neighbour indexing, metadata)
is finalised; small-batch testing precedes a stable v0.1.0 cut.

### Added

- End-to-end pipeline: EPUB / TXT / PDF source ingest with
  text normalization, prose chunking, embedding via a local Ollama
  daemon, dense storage in LanceDB, and cited-passage search.
- CLI surface: `ingest`, `query`, `books`, `metadata`, `intake ocr`,
  `corpus rebuild`, `vectors {status,rebuild,drop,reembed}`, `dryrun`,
  `info`, `verify`, `remove`, `pipeline-trail`, `diagnose`,
  `libraries list`, `stamps reconcile`.
- MCP server (`bookrack-mcp`): streamable-HTTP transport bound to
  `127.0.0.1:8765/mcp` by default for agent clients (e.g. Claude
  Code).
- `bookrack init`: five-step interactive install wizard. Picks a data
  root, probes the PDFium dynamic library, probes Ollama for the
  configured embed model, exercises the full
  ingest → embed → query pipeline against a tempdir, then writes
  `<data_root>/config.toml` and a pointer in the platform-default
  registry.
- `bookrack doctor`: one-screen install health check. Exits non-zero
  on any FAIL row; `--json` for machine consumption.
- `bookrack-embed::probe_ollama`: lightweight `/api/tags` probe with a
  2-second default timeout, reused by the wizard and doctor.
- Portable-mode data root: a `bookrack-data/` directory beside the
  running binary is detected automatically and wins over the registry
  default. A self-contained tarball is movable to any disk without
  environment configuration.
- Platform-default registry at `<config>/bookrack/registry.toml`,
  written by `bookrack init` so subsequent `bookrack` invocations find
  their data root from any working directory.
- Per-data-root configuration file `<data_root>/config.toml` for
  `ollama_url`, `embed_model`, `mcp_addr`, `log_directive`. Resolution
  precedence is env var > root config > hardcoded default.
- Audit profiles `default`, `trust-source`, and `strict`, selectable
  per command via `--audit-profile`. A local overlay TOML under
  `<data_root>/audit-rules/audit_profile.local.toml` adjusts
  thresholds without rebuilding the binary.
- Restartable ingest: long runs survive a host idle-sleep window
  idempotently. On macOS the README documents `caffeinate -i` for
  unattended overnight runs.
- Rebuildable derived layers: `bookrack corpus rebuild` regenerates
  `corpus.db` from the opaque store, and `bookrack vectors reembed`
  reruns the embedder over chunk text in place. Both accept
  `--stale-only` to scope the refresh to partitions whose stored
  stamps lag the running binary.
- `bookrack diagnose`: scrubbed `.tar.gz` bundle of crash reports,
  recent logs, and a small catalog snapshot for bug reports.

### Documentation

- README with installation, prerequisites, and operating notes.
- `docs/UPGRADE.md`: bump-to-refresh matrix mapping each
  behaviour-sensitive dependency and stamp constant to the cheapest
  CLI invocation that restores a consistent library.
- `crates/extract/PDFIUM_VERSION.md`: pinned PDFium version with
  per-platform SHA-256 checksums (Linux x86_64, Windows x86_64, macOS
  arm64, macOS x86_64).

[Unreleased]: https://github.com/Collegium-Siderum/bookrack/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/Collegium-Siderum/bookrack/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Collegium-Siderum/bookrack/compare/v0.1.0-rc4...v0.2.0
[0.1.0-rc1]: https://github.com/Collegium-Siderum/bookrack/releases/tag/v0.1.0-rc1
