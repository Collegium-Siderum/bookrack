# Pinned PDFium version

The PDF adapter binds the PDFium native library at runtime. The Rust
wrapper crate and the native binary are versioned independently and
**must be pinned together**: the `pdfium_NNNN` cargo feature selects an
API surface, and the binary must expose exactly that surface, or symbol
resolution fails at runtime. This file is the single source of truth
for that pinned pair.

## The pin

| What | Value |
| --- | --- |
| `pdfium-render` crate | `0.9` (resolves to 0.9.1) |
| Cargo feature | `pdfium_7763` |
| Native binary release | `chromium/7763` (PDFium 148.0.7763.0) |
| Binary source | <https://github.com/bblanchon/pdfium-binaries> |

The feature build number and the release build number are both `7763`
— they must stay equal. In `pdfium-render` 0.9.1, `pdfium_7763` is the
newest concrete API version (`pdfium_latest` aliases it); pinning the
concrete feature rather than `pdfium_latest` keeps the build
reproducible when a later crate release moves `pdfium_latest` forward.

## Binary archives

The non-V8 archives are used (no JavaScript engine is needed). Upstream
publishes no checksums, so these SHA-256 values were computed locally
when the version was pinned and the CI download step verifies against
them — a corrupted download or a silently re-cut upstream asset then
fails loudly.

| Asset | SHA-256 |
| --- | --- |
| `pdfium-win-x64.tgz` | `45c4cc5d052ef8ec6380b946b548a76100f4675e38362000a4c732e16d5e8eda` |
| `pdfium-linux-x64.tgz` | `e3f0c66b2daad710cb6c8edd4a8c45c8902995e359dc0775917fc16e2e56349d` |

Download URL template:

```
https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7763/pdfium-<platform>.tgz
```

The Windows archive holds the library at `bin/pdfium.dll`; the Linux
archive at `lib/libpdfium.so`.

## Local development

Set `BOOKRACK_PDFIUM_LIB` to a directory holding the platform library
(`pdfium.dll` / `libpdfium.so`) for this build. When unset, the
adapter loads the library from the running executable's own directory.

## When this changes

Bumping any value here is a behaviour-sensitive change: the PDFium
build number is part of the PDF adapter's `extractor_version`, so a
bump re-extracts downstream. Update, in lockstep:

- `pdfium-render` features in the workspace `Cargo.toml`,
- the tag, URL, and SHA-256 in the CI `Fetch pinned PDFium` step,
- the `PDFIUM_BUILD` constant in `crates/extract/src/pdf.rs`.
