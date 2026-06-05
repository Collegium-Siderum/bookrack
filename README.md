# bookrack

A local, offline RAG library. Point bookrack at a collection of
long-form books — EPUB, TXT, or PDF — and it turns them into a
knowledge base an AI agent can search with precise, cited passages.
The pipeline runs entirely on your own machine; nothing ever leaves
the host. An MCP server speaks the standard agent protocol, so
clients like Claude Code can search the library as a tool.

## Status

Pre-release. The end-to-end pipeline — extract, ingest, embed, and
cited search — runs from the CLI and over the MCP server.
Pre-production hardening (schema migrations, approximate-nearest-
neighbour indexing, metadata) is still in progress.

## 30-second install

1. **Make sure Ollama is up** and the embedding model is pulled. The
   default is `qwen3-embedding:0.6b`; any Ollama-served embedding
   model works once configured.

   ```
   # https://ollama.com/download
   ollama serve &
   ollama pull qwen3-embedding:0.6b
   ```

2. **Grab the tarball** for your platform from the
   [Releases page](https://github.com/Collegium-Siderum/bookrack/releases).
   Each tarball bundles the `bookrack` and `bookrack-mcp` binaries,
   the matching PDFium dynamic library, and licenses.

   | Platform | Tarball |
   | --- | --- |
   | macOS (Apple Silicon) | `bookrack-X.Y.Z-aarch64-apple-darwin.tar.gz` |
   | Linux x86_64 | `bookrack-X.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
   | Windows x86_64 | `bookrack-X.Y.Z-x86_64-pc-windows-msvc.zip` |

   Intel macOS users run the Apple Silicon binary under Rosetta 2; a
   native `x86_64-apple-darwin` build is not shipped.

3. **Extract and run the wizard.**

   ```
   tar -xzf bookrack-*.tar.gz
   cd bookrack-*
   ./bookrack init
   ```

   `init` is a five-step wizard: it picks a data root, checks the
   PDFium library, probes Ollama, runs an end-to-end smoke test
   against a tempdir, then writes `<data_root>/config.toml` and a
   pointer in your platform's config directory so subsequent
   `bookrack` invocations work from any shell.

4. **Ingest a book and start the MCP server.**

   ```
   ./bookrack ingest /path/to/book.epub
   ./bookrack-mcp                          # 127.0.0.1:8765/mcp
   ```

## Connecting an MCP client

**Claude Code** — one command registers the running daemon:

```
claude mcp add bookrack --transport http http://127.0.0.1:8765/mcp
```

**Cursor, Claude Desktop, Cline, Continue, others** — TBD. Streamable-
HTTP MCP support varies by client and version; community pointers
welcome via issues.

## Other ways to install

**Portable** — drop a `bookrack-data/` directory next to the extracted
binary. The wizard detects it and offers it as a default; the data
root is then movable to any disk along with the tarball, no
environment variable needed.

**From source** — Rust 1.95.0, edition 2024. Clone the repo and
build:

```
cargo build --release -p bookrack-cli -p bookrack-mcp
```

Set `BOOKRACK_PDFIUM_LIB` to a directory holding the platform's
PDFium library (see
[crates/extract/PDFIUM_VERSION.md](crates/extract/PDFIUM_VERSION.md)
for the pinned version and per-platform download). Without it, PDF
ingest is unavailable but EPUB and TXT still work.

## Troubleshooting

```
bookrack doctor
```

A one-screen health check covering every install expectation: data
root resolved, catalog and corpus schemas openable, PDFium library
on disk, Ollama daemon reachable, embed model pulled. Each row is
`OK`, `WARN`, or `FAIL`; any FAIL exits with a non-zero status so a
script can branch on the result. Pass `--json` for machine-readable
output suitable for a bug report.

If something is broken, `bookrack diagnose` bundles crash reports,
recent logs, and a scrubbed catalog snapshot into a `.tar.gz` for
issue attachments.

## Operating

### Long ingestions

Ingestion is restartable: when a host suspends mid-run the embedding
step pauses with it and resumes once the host wakes, idempotently.
The output is unchanged either way; only the wall-clock includes the
time spent asleep, which makes a run that crossed an idle-sleep
window read as far slower than it really was.

On macOS the default idle-sleep policy will suspend a backgrounded
shell. Wrap a long ingest to opt out of idle sleep for its duration:

```
caffeinate -i bookrack ingest <file>     # single book
caffeinate -i bash bulk-ingest.sh        # batch driver
```

`-i` keeps the system from sleeping on idle without blocking display
sleep, so an unattended overnight run finishes in its true
wall-clock.

### Audit profile

The metadata audit, the filename parser, and the EPUB / TXT
half-rules read every toggle and threshold from an audit profile.
Three built-in presets ship with the binary:

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
so a deployment can adjust individual thresholds without
recompiling. Two existing list-typed inputs continue to load from
the same directory: `<data_root>/audit-rules/publishers.toml`
carries the reputable-imprint whitelist (see
[publishers.example.toml](crates/metadata/data/publishers.example.toml))
and `watermarks.toml` carries the four watermark token lists (see
[watermarks.example.toml](crates/metadata/data/watermarks.example.toml)).
Both files are user-supplied; bookrack ships only the examples.

### Upgrading

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

### Configuration files and resolution order

bookrack reads its data root in this precedence, highest first:

1. `--data-dir <path>` flag
2. `--library <name>` flag (looked up in the registry named by
   `BOOKRACK_REGISTRY`)
3. `BOOKRACK_DATA_DIR` environment variable
4. A `bookrack-data/` directory next to the running binary
5. The `default` entry of the registry named by `BOOKRACK_REGISTRY`
6. The `default` entry of the platform-default registry at
   `<config_dir>/bookrack/registry.toml`

`bookrack init` writes step 6's registry file by default. Operational
knobs (Ollama endpoint, embed model, MCP listen address, log filter)
resolve env var > `<data_root>/config.toml` > hardcoded default.

The full keys accepted in `<data_root>/config.toml`:

```toml
ollama_url    = "http://localhost:11434"
embed_model   = "qwen3-embedding:0.6b"
mcp_addr      = "127.0.0.1:8765"
log_directive = "info,lance=warn"
```

Every field is optional. Edit by hand; bookrack does not rewrite
this file outside of `init`.

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
ships their license texts, which a redistribution must carry
alongside the binary. The release tarballs include the upstream
LICENSE file as `LICENSE-PDFIUM`.
