# bookrack

A local, offline RAG library. Point bookrack at a collection of
long-form books and academic papers — EPUB, PDF, TXT, or HTML — and it
turns them into a knowledge base an AI agent can search with precise,
cited passages. The pipeline runs entirely on your own machine; nothing ever
leaves the host. An MCP server speaks the standard agent protocol, so
clients like Claude Code can search the library as a tool.

## Status

Pre-release. The end-to-end pipeline — extract, ingest, embed, and
cited search — runs through the `bookrack run` daemon, driven by
one-shot subcommands, `bookrack list` / `find` / `search` across both
pipelines and `bookrack show <kind>:<id>` for a single item,
`bookrack rpc` for ad-hoc control-plane RPCs, and MCP. Books and
academic papers live in two parallel stores under one data root and
share the same MCP surface. Schema migrations and metadata workflows
are still being hardened for production use.

## Install

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

   Intel macOS is not supported by the release artefacts: only an
   arm64 macOS build is shipped, and an Intel machine cannot execute
   it. Build from source on that machine instead — see "Other ways
   to install".

3. **Extract and run the wizard.**

   macOS — the tarball holds a `Bookrack.app` bundle and the binaries
   live inside it:

   ```
   tar -xzf bookrack-*.tar.gz
   cd bookrack-*/Bookrack.app/Contents/Resources
   ./bookrack init
   ```

   Linux:

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
   pointer in your platform's config directory, so any later
   `bookrack` command finds the same data root without a `--data-dir`
   flag. The pointer records where the data lives; it does not put
   `bookrack` on your `PATH`. Keep invoking it by path, or add its
   directory to `PATH` yourself.

4. **If the first run is blocked** — only when you downloaded the
   archive with a browser. Browsers tag downloads and the release
   artefacts are unsigned, so macOS refuses to run them until you
   approve once and Windows shows a SmartScreen warning. Downloading
   with `curl` or `wget` sets no tag and skips this step.

   macOS:

   1. Run `./bookrack init`. macOS refuses and offers no override in
      the dialog.
   2. Open System Settings -> Privacy & Security, scroll to
      Security, and choose "Open Anyway" next to the blocked binary.
   3. Run the command again and confirm.

   The right-click -> Open shortcut older guides describe was removed
   in macOS 15; on Sequoia and later the System Settings route is the
   only one through the interface. From a terminal, the equivalent
   is:

   ```
   xattr -dr com.apple.quarantine bookrack-*/
   ```

   Windows — choose "More info" then "Run anyway" when SmartScreen
   appears, or clear the tag before extracting:

   ```
   Unblock-File bookrack-*.zip
   ```

5. **Start the daemon and ingest a book.** Run these from the same
   directory as step 3. `bookrack run` starts a foreground daemon: it
   serves MCP over streamable-HTTP at `127.0.0.1:8765/mcp` and a local
   control socket where the write commands arrive. From a second
   shell, submit work with the one-shot subcommands — `bookrack
   ingest` streams the queue worker's progress until the job lands.

   macOS / Linux:

   ```
   ./bookrack run                          # terminal 1: the daemon
   ./bookrack ingest /path/to/book.epub    # terminal 2
   ```

   Windows (PowerShell):

   ```
   .\bookrack.exe run
   .\bookrack.exe ingest path\to\book.epub
   ```

   Submit a paper through the parallel `papers` subcommand instead;
   ingest follows the same control-plane streaming, and `--recursive`
   walks a directory and forwards every supported file it finds:

   ```
   ./bookrack papers ingest /path/to/paper.pdf
   ./bookrack papers ingest --recursive /path/to/papers-dir/
   ```

   Papers live in a second cluster (catalog, corpus, vector store,
   source-PDF archive) under the same data root and share the same
   MCP server; `library.search` queries both stores at once unless a
   `kind` switch narrows it.

   For a headless deployment — systemd unit, Windows service — run
   `bookrack-mcp` instead; see the [operating guide](docs/operating.md#the-daemon).

## Connecting an MCP client

**Claude Code** — one command registers the running daemon. The
default scope is `local` — the server is visible only inside the
project the command is run from; pass `--scope user` to register it
once for every project on the machine:

```
# current project only (local scope, the default)
claude mcp add --transport http bookrack http://127.0.0.1:8765/mcp

# every project on this machine (user scope)
claude mcp add --transport http --scope user bookrack http://127.0.0.1:8765/mcp
```

**Cursor, Claude Desktop, Cline, Continue, others** — TBD. Streamable-
HTTP MCP support varies by client and version; community pointers
welcome via issues.

## Other ways to install

**Portable** — drop a `bookrack-data/` directory next to the extracted
binary. The wizard detects it and offers it as a default; the data
root is then movable to any disk along with the tarball, no
environment variable needed.

On macOS the binary lives inside `Bookrack.app`, so "next to the
binary" is inside the bundle. Upgrading replaces the whole bundle, and
every book, index, and log under it goes with it, so the wizard
refuses a data root anywhere inside `Bookrack.app`. Pick one outside
the bundle and let the pointer `init` writes find it.

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

## Uninstall

bookrack writes to more places than the directory it was extracted
into, and the registry is the only record of where the libraries are,
so the order matters.

1. **Find the libraries before touching anything.** `bookrack libraries
   list` prints every registered data root. `bookrack config effective
   --json` prints every other location below as this machine resolves
   it, including the directories the managed native dependencies were
   installed into. Then stop the daemon: `bookrack quit`.
2. **Delete the libraries you do not want to keep**: `bookrack
   libraries remove <name> --purge` deletes a root after a typed
   confirmation. A root you keep stays a library — a later install
   finds it with `bookrack libraries scan` and registers it again. Two
   things a root can leave outside itself: catalog snapshots, when
   `BOOKRACK_BACKUP_DIR` points elsewhere, and a `bookrack-data/`
   directory beside the binary in a portable layout.
3. **Delete the per-user state.** With no `BOOKRACK_*` directory
   variable set:

   | Platform | Registry and index profiles | Daemon state, logs, PDFium, llama-server, reranker models | Runtime directory |
   | --- | --- | --- | --- |
   | macOS | `~/Library/Application Support/bookrack/` | the same directory | `~/Library/Caches/bookrack/` |
   | Linux | `~/.config/bookrack/` | `~/.local/share/bookrack/` | `$XDG_RUNTIME_DIR/bookrack/`, else `~/.cache/bookrack/` |
   | Windows | `%APPDATA%\bookrack\` | the same directory | `%LOCALAPPDATA%\bookrack\` |

   `$XDG_CONFIG_HOME` and `$XDG_DATA_HOME` move the Linux paths;
   `BOOKRACK_REGISTRY`, `BOOKRACK_DAEMON_STATE_DIR` and
   `BOOKRACK_RUNTIME_DIR` move any platform's, and the `config
   effective --json` output from step 1 is what applies.
4. **Delete the extracted directory** — the `Bookrack.app` bundle on
   macOS, the directory the archive unpacked into elsewhere.
5. **Remove the MCP client's entry.** For Claude Code: `claude mcp
   remove bookrack`, with `--scope user` if it was registered that
   way. Other clients keep theirs in their own configuration.

Ollama and the models it pulled are a separate product; `ollama rm
qwen3-embedding:0.6b` removes the embedding model if nothing else on
the machine uses it.

## Features

- **Books and papers, side by side** — books and academic papers in
  two parallel stores under one data root; `library.search` queries
  one store or both.
- **Four source formats** — EPUB, PDF, TXT, and HTML (`.html` /
  `.htm` / `.xhtml`); image-only scans route to the OCR worklist.
  MOBI and AZW3 are not supported: convert them to EPUB first (e.g.
  with Calibre's `ebook-convert`).
- **Cited, fully offline search** — passages return with precise
  citations, and extraction, embedding, and search all run on the
  host. Nothing leaves the machine.
- **MCP-native, with a full CLI** — a streamable-HTTP MCP server
  exposes the library as a tool to agent clients, backed by a one-shot
  CLI and a control socket for operators.
- **Named retrieval profiles** — `index-profile` couples the embedding
  model, the ANN index shape, and the reranker stage into one named,
  statically-validated atom.
- **A managed, daemon-free registry** — `libraries` verbs register,
  detect, scan, and configure data roots with no daemon running; each
  root self-describes with an identity manifest.
- **Many libraries, one daemon** — started through the registry, the
  daemon mounts every registered library at bring-up: each answers
  reads, and queue jobs route to their target library by name. The set
  changes while it runs — `libraries mount` and `libraries unmount`
  add and release a library without a restart.
- **One-screen status** — `bookrack status` answers "is a daemon
  running, which library does it serve, is it busy" in a single
  no-argument call, and through the exit code alone under `--quiet`.
- **An OCR worklist** — image-only scans land on a durable worklist
  instead of failing; run any OCR engine and re-enter the product.
- **Observable pipelines** — every ingest and search records to
  queryable run and retrieval logs (`bookrack runs`, `bookrack
  retrieval`), with a `doctor` health check and a `diagnose` bundle
  for bug reports.

## Documentation

| Guide | Covers |
| --- | --- |
| [Operating](docs/operating.md) | the daemon, ingesting, the queue, the OCR worklist, the status card, health checks, observability |
| [Configuration](docs/configuration.md) | data-root resolution, the library registry, `config.toml`, index profiles, the audit profile |
| [Upgrading](docs/UPGRADE.md) | installing a new build, the bump-to-refresh matrix, downgrading, switching the embedding model |
| [Control plane](docs/control-plane.md) | the JSON-RPC surface behind the CLI and MCP |

## Troubleshooting

`bookrack doctor` runs a one-screen health check of every install
expectation and exits non-zero on any failure; `bookrack diagnose`
bundles logs and a scrubbed catalog snapshot for a bug report. Both
are covered in the
[operating guide](docs/operating.md#health-and-diagnostics).

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
