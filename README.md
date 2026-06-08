# bookrack

A local, offline RAG library. Point bookrack at a collection of
long-form books — EPUB, TXT, or PDF — and it turns them into a
knowledge base an AI agent can search with precise, cited passages.
The pipeline runs entirely on your own machine; nothing ever leaves
the host. An MCP server speaks the standard agent protocol, so
clients like Claude Code can search the library as a tool.

## Status

Pre-release. The end-to-end pipeline — extract, ingest, embed, and
cited search — runs through the `bookrack run` daemon-REPL and over
MCP. The vector store ships with an IVF index family (flat / SQ / PQ
and the IVF-HNSW variants) selectable through `vectors rebuild`.
Schema migrations and metadata workflows are still being hardened
for production use.

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

   macOS / Linux:

   ```
   tar -xzf bookrack-*.tar.gz
   cd bookrack-*
   ./bookrack init
   ```

   Windows (PowerShell):

   ```
   Expand-Archive bookrack-*.zip -DestinationPath .
   cd bookrack-*
   .\bookrack.exe init
   ```

   `init` is a five-step wizard: it picks a data root, checks the
   PDFium library, probes Ollama, runs an end-to-end smoke test
   against a tempdir, then writes `<data_root>/config.toml` and a
   pointer in your platform's config directory so subsequent
   `bookrack` invocations work from any shell.

4. **Start the session and ingest a book.** `bookrack run` opens a
   daemon-REPL: it serves MCP over streamable-HTTP at
   `127.0.0.1:8765/mcp` and hosts a foreground prompt where the write
   commands live. Ingest from inside it; the daemon's queue worker
   handles long runs without blocking the prompt.

   macOS / Linux:

   ```
   ./bookrack run
   bookrack> ingest /path/to/book.epub
   ```

   Windows (PowerShell):

   ```
   .\bookrack.exe run
   bookrack> ingest path\to\book.epub
   ```

   For a headless deployment — systemd unit, Windows service — run
   `bookrack-mcp` instead; it serves the same MCP endpoint without a
   REPL. The two are mutually exclusive against one library because
   each holds the machine-wide session lock; stop one before starting
   the other.

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

On every desktop platform the default idle-sleep policy will suspend
a backgrounded shell. The natural unit to wrap is the `bookrack run`
session itself: queue work from the REPL with `queue add <path>`,
then leave the session open while the worker grinds away.

macOS — `caffeinate` blocks idle sleep without blocking display sleep:

```
caffeinate -i ./bookrack run
```

Linux (systemd) — `systemd-inhibit` takes the same kind of lock the
desktop environment uses for media playback:

```
systemd-inhibit --what=idle --why="bookrack" ./bookrack run
```

Windows (PowerShell) — flip the active power scheme's idle-sleep
timeout to zero for the duration of the session, then restore it:

```
powercfg /change standby-timeout-ac 0
.\bookrack.exe run
powercfg /change standby-timeout-ac 30   # restore the previous value
```

For an unattended overnight run, prefer a wrapper that runs the
restore step even when the session exits with an error.

### Audit profile

The metadata audit, the filename parser, the EPUB / TXT half-rules,
and the extract-side HTML / quality / language gates read every
toggle and threshold from an audit profile. Three built-in presets
ship with the binary:

- `default` — every per-field signal active, every TOC shape signal
  active, year range 1450–2100. This is the active profile at
  ingest time.
- `trust-source` — every toggle off. The audit substep is skipped
  entirely; the pipeline still seeds the base attrs and writes a
  `pending` review row stamped `bookrack-ingest:trust-source`, but
  no signal weakens or strengthens any field. Useful when ingesting
  "whatever the source says" and deferring every quality call to a
  human or downstream LLM reviewer.
- `strict` — same toggle set as `default`; reserved for future
  upgrades that promote selected signals to higher severities.

Inspect them with the `audit-profile` subcommand:

```
bookrack audit-profile list
bookrack audit-profile show trust-source
bookrack audit-profile diff default strict
```

The shipped `default` profile is merged with an optional overlay at
`<data_root>/audit-rules/audit_profile.local.toml` so a deployment
can adjust individual thresholds, the HTML block / skip tag lists,
the PDF text-quality cutoffs, or the BCP-47 script buckets without
recompiling.

Two further on-disk schemas live under the same directory and follow
the same shipped-default-plus-overlay merge:

- `<data_root>/audit-rules/audit_data.toml` — the reputable-imprint
  whitelist, the four watermark token lists, the URL / e-mail
  watermark substrings, the whitelist normalisation abbreviations,
  the placeholder-title words, and the book-extension lists the
  ingest dryrun walker and the diagnose scrubber consult. See the
  shipped default at
  [crates/audit-profile/data/audit_data.toml](crates/audit-profile/data/audit_data.toml).
- `<data_root>/audit-rules/headings.toml` — the multi-language
  chapter / volume marker grammars the TXT adapter dispatches across
  (Sino, Latin, German families today). Add a unit char, ordinal
  stem, or first-ordinal spelling for a new language without a
  recompile. See the shipped default at
  [crates/audit-profile/data/headings.toml](crates/audit-profile/data/headings.toml).

All three overlays are user-supplied; bookrack falls through to the
shipped defaults when an overlay is absent or omits a field.

### Upgrading

An upgraded binary may read older derived data exactly, or it may
need that data rebuilt before it can serve correct results. The
trigger is whether the upgrade bumps a behaviour-sensitive
dependency or a stamp constant. Three commands cover every refresh
case; all run inside the `bookrack run` REPL because they take the
write scheduler:

- `corpus rebuild` — regenerate `corpus.db` from the v1 extraction
  envelopes. Use this after a corpus schema bump, or to recover
  from a deleted `corpus.db` when the chunks table is still on disk.
- `vectors reembed` — re-run the active embedder over the existing
  chunk text. Use this after bumping the vector width,
  `CHUNK_VERSION`, or `NORMALIZE_VERSION`. For an embedding-model
  swap, see [Switching the embedding model](#switching-the-embedding-model).
- `ingest <file> --force` — re-ingest a source file. The only path
  that refreshes `extractor_version`. Use this after bumping a
  behaviour-sensitive parser dependency (`rbook`, `scraper`,
  `encoding_rs`, `unicode-normalization`, `pdfium-render`).

The `--stale-only` flag on `corpus rebuild` and `vectors reembed`
scopes the refresh to the partitions whose stored
`extractor_version` lags this binary's. A reembed that touches every
book is the most expensive refresh; schedule it for a low-activity
window and wrap the session with the platform's idle-sleep override
covered under [Long ingestions](#long-ingestions).

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
   `<config_dir>/bookrack/registry.toml`, where `<config_dir>` is:

   | Platform | `<config_dir>` |
   | --- | --- |
   | macOS | `~/Library/Application Support` |
   | Linux | `$XDG_CONFIG_HOME`, or `~/.config` if unset |
   | Windows | `%APPDATA%` (the Roaming AppData directory) |

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

### Switching the embedding model

`embed_model` is part of the library's identity: stamps in
`corpus.db` pin the model name and its vector dimension, and both
embed and query refuse a mismatch. Editing the field on an existing
library leaves the on-disk vectors orphaned. The supported swaps are:

- **`bookrack libraries fork <name> --data-dir <path>`** — clone
  the current library into a sibling under `<path>` (envelope store
  hardlinked, catalog + corpus snapshotted, vector store dropped),
  then start a `bookrack run` session against the clone
  (`bookrack --library <name> run`) and execute `vectors reset` in
  its REPL to rebuild under the new model. The original library
  stays intact; throw the clone away if the new model is worse.
  `libraries fork` is a top-level command; stop any `bookrack run`
  session on the source library before invoking it.
- **`vectors reset`** — REPL command, in place: drops the chunks
  table and re-embeds every book under the env-configured model. The
  old vectors are unrecoverable. Use when disk space is tight or the
  swap is settled.

See [docs/UPGRADE.md](docs/UPGRADE.md#switching-the-embedding-model)
for the full procedure.

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
