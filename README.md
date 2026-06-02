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
