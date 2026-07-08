# bookrack

A local, offline RAG library. Point bookrack at a collection of
long-form books and academic papers — EPUB, TXT, or PDF — and it turns
them into a knowledge base an AI agent can search with precise, cited
passages. The pipeline runs entirely on your own machine; nothing ever
leaves the host. An MCP server speaks the standard agent protocol, so
clients like Claude Code can search the library as a tool.

## Status

Pre-release. The end-to-end pipeline — extract, ingest, embed, and
cited search — runs through the `bookrack run` daemon, driven by
one-shot subcommands, `bookrack exec` for ad-hoc control-plane RPCs,
and MCP. Books and academic papers live in two parallel stores under
one data root and share the same MCP surface. Schema migrations and
metadata workflows are still being hardened for production use.

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

4. **Start the daemon and ingest a book.** `bookrack run` starts a
   foreground daemon: it serves MCP over streamable-HTTP at
   `127.0.0.1:8765/mcp` and a local control socket where the write
   commands arrive. From a second shell, submit work with the
   one-shot subcommands — `bookrack ingest` streams the queue
   worker's progress until the job lands.

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

## Features

- **Books and papers, side by side** — EPUB / TXT / PDF books and
  academic papers in two parallel stores under one data root;
  `library.search` queries one store or both.
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
- **An OCR worklist** — image-only scans land on a durable worklist
  instead of failing; run any OCR engine and re-enter the product.
- **Observable pipelines** — every ingest and search records to
  queryable run and retrieval logs (`bookrack runs`, `bookrack
  retrieval`), with a `doctor` health check and a `diagnose` bundle
  for bug reports.

## Documentation

| Guide | Covers |
| --- | --- |
| [Operating](docs/operating.md) | the daemon, ingesting, the queue, the OCR worklist, health checks, observability |
| [Configuration](docs/configuration.md) | data-root resolution, the library registry, `config.toml`, index profiles, the audit profile |
| [Upgrading](docs/UPGRADE.md) | the bump-to-refresh matrix and switching the embedding model |
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
