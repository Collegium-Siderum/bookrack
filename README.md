# bookrack

A local, offline RAG library: it turns a collection of long-form books
into a knowledge base an AI agent can search with precise, cited
passages. It exposes an MCP server for agent clients.

## Status

Pre-release. The end-to-end pipeline — extract, ingest, embed, and
cited search — runs from the CLI and over the MCP server. Pre-production
hardening (schema migrations, approximate-nearest-neighbour indexing,
metadata) is still in progress.

## Running long ingestions

Ingestion is restartable: when a host suspends mid-run the embedding
step pauses with it and resumes once the host wakes, idempotently. The
output is unchanged either way; only the wall-clock includes the time
spent asleep, which makes a run that crossed an idle-sleep window read
as far slower than it really was.

On macOS the default idle-sleep policy will suspend a backgrounded
shell. Wrap a long ingest to opt out of idle sleep for its duration:

```
caffeinate -i bookrack ingest <file>     # single book
caffeinate -i bash bulk-ingest.sh        # batch driver
```

`-i` keeps the system from sleeping on idle without blocking display
sleep, so an unattended overnight run finishes in its true wall-clock.

## Audit profile

The metadata audit, the filename parser, and the EPUB / TXT half-rules
read every toggle and threshold from an audit profile. Three built-in
presets ship with the binary:

- `default` — the previous hard-coded behaviour, expressed as
  toggles. Year range 1450–2100, every per-field signal active, every
  TOC shape signal active.
- `trust-source` — every toggle off. The audit substep is skipped
  entirely. The pipeline still seeds the base attrs and writes a
  `pending` review row stamped `bookrack-ingest:trust-source`, but no
  signal weakens or strengthens any field. Use this when you want
  bookrack to ingest "whatever the source says" and defer every
  quality call to a human or downstream LLM reviewer.
- `strict` — same toggle set as `default`; reserved for future
  upgrades that promote selected signals to higher severities.

The active preset is selected per command with the global flag:

```
bookrack --audit-profile trust-source ingest <file>
bookrack --audit-profile strict dryrun <dir>
```

Without the flag, the shipped `default` profile is merged with an
optional overlay at `<data_root>/audit-rules/audit_profile.local.toml`
so a deployment can adjust individual thresholds without recompiling.
Two existing list-typed inputs continue to load from the same
directory: `<data_root>/audit-rules/publishers.toml` carries the
reputable-imprint whitelist (see
[publishers.example.toml](crates/metadata/data/publishers.example.toml))
and `watermarks.toml` carries the four watermark token lists (see
[watermarks.example.toml](crates/metadata/data/watermarks.example.toml)).
Both files are user-supplied; bookrack ships only the examples.

## Upgrading

An upgraded binary may read older derived data exactly, or it may
need that data rebuilt before it can serve correct results. The
trigger is whether the upgrade bumps a behaviour-sensitive
dependency or a stamp constant. Three commands cover every refresh
case:

- `bookrack corpus rebuild` — regenerate `corpus.db` from the v1
  extraction envelopes. Use this after a corpus schema bump, or to
  recover from a deleted `corpus.db` when the chunks table is still
  on disk.
- `bookrack vectors reembed` — re-run the active embedder over the
  existing chunk text. Use this after bumping the embedding model,
  the vector width, `CHUNK_VERSION`, or `NORMALIZE_VERSION`.
- Re-ingest the source files — the only path that refreshes
  `extractor_version`. Use this after bumping a behaviour-sensitive
  parser dependency (`rbook`, `scraper`, `encoding_rs`,
  `unicode-normalization`, `pdfium-render`).

The `--stale-only` flag on `corpus rebuild` and `vectors reembed`
scopes the refresh to the partitions whose stored
`extractor_version` lags this binary's. A reembed that touches every
book is the most expensive refresh; schedule it for a low-activity
window and wrap long runs in `caffeinate -i` on macOS, as above.

See [docs/UPGRADE.md](docs/UPGRADE.md) for the full bump-to-refresh
matrix and per-command guidance.

## License

Apache-2.0 — see [LICENSE](LICENSE).

### Third-party native components

The PDF adapter extracts text with [PDFium](https://pdfium.googlesource.com/pdfium/)
(BSD-3-Clause), loaded at runtime as a native library. The library is
not vendored into this repository; a build obtains a pinned prebuilt
binary from [pdfium-binaries](https://github.com/bblanchon/pdfium-binaries)
— see [crates/extract/PDFIUM_VERSION.md](crates/extract/PDFIUM_VERSION.md)
for the pinned version. That binary statically bundles several
permissively licensed libraries (FreeType, LCMS2, libjpeg-turbo,
libpng, zlib, libtiff, OpenJPEG, and others); the upstream archive
ships their license texts, which a redistribution must carry alongside
the binary.
