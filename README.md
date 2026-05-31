# bookrack

A local, offline RAG library: it turns a collection of long-form books
into a knowledge base an AI agent can search with precise, cited
passages. It exposes an MCP server for agent clients.

## Status

Pre-release. The end-to-end pipeline — extract, ingest, embed, and
cited search — runs from the CLI and over the MCP server. Pre-production
hardening (schema migrations, approximate-nearest-neighbour indexing,
metadata) is still in progress.

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
