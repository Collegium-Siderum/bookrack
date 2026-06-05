# Changelog

All notable changes to bookrack are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project
follows [semver](https://semver.org/spec/v2.0.0.html). Each release
section is the source of truth for the GitHub Release notes — the
release workflow extracts the matching section verbatim from this file.

## [Unreleased]

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

[Unreleased]: https://github.com/Collegium-Siderum/bookrack/compare/v0.1.0-rc1...HEAD
[0.1.0-rc1]: https://github.com/Collegium-Siderum/bookrack/releases/tag/v0.1.0-rc1
