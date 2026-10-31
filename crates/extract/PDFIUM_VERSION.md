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
| `pdfium-render` crate | `0.9` (resolves to 0.9.4) |
| Cargo feature | `pdfium_7881` |
| Native binary release | `chromium/7881` (PDFium 151.0.7881.0) |
| Binary source | <https://github.com/bblanchon/pdfium-binaries> |

The feature build number and the release build number are both `7881`
— they must stay equal. In `pdfium-render` 0.9.4, `pdfium_7881` is the
newest concrete API version (`pdfium_latest` aliases it); pinning the
concrete feature rather than `pdfium_latest` keeps the build
reproducible when a later crate release moves `pdfium_latest` forward.

## Known issues

Advisories open against the pinned build. Treat each row as reachable
from any PDF source; the operator-facing consequence is the note on
trusted sources under *Ingesting* in `docs/operating.md`.

| Advisory | Severity | Component | Build carrying the fix |
| --- | --- | --- | --- |
| CVE-2026-17875 | Medium | name tree, use-after-free | `chromium/8100`, newer than the `pdfium_7881` API that `pdfium-render` 0.9.4 binds at most |

CVE-2026-10002 (`cjs_field`, High) is not in the table: the field code
belongs to the JavaScript layer, which the non-V8 archives do not
compile.

## Binary archives

The non-V8 archives are used (no JavaScript engine is needed). These
SHA-256 values are the digests GitHub records for each release asset,
and the CI download step verifies against them — a corrupted download
or a silently re-cut upstream asset then fails loudly.

| Asset | SHA-256 |
| --- | --- |
| `pdfium-win-x64.tgz` | `73cc0de638ac2095e7445bf56a38200a5b7c7ca0e9f4ba144598f2457377ac08` |
| `pdfium-linux-x64.tgz` | `1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d` |
| `pdfium-mac-arm64.tgz` | `52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40` |
| `pdfium-mac-x64.tgz` | `6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b` |

Download URL template:

```
https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7881/pdfium-<platform>.tgz
```

The Windows archive holds the library at `bin/pdfium.dll`; the Linux
archive at `lib/libpdfium.so`; the macOS archives at
`lib/libpdfium.dylib`.

## How the library is found

The adapter searches a chain of directories, first hit wins:

1. `BOOKRACK_PDFIUM_LIB` — authoritative when set; no fallback, so a
   typo surfaces as a miss instead of being papered over.
2. The running executable's own directory (the release-archive
   layout, where the library ships next to the binaries).
3. The per-user managed directory (`<platform data dir>/bookrack/
   pdfium`), which `bookrack doctor --install-pdfium` and the
   first-run wizard's download offer populate with the pinned build.

`src/pdfium_pin.rs` carries the values in the tables above as
constants for that installer.

## When this changes

Bumping any value here is a behaviour-sensitive change: a different
PDFium binary can extract text differently, which downstream must
re-extract on. Update, in lockstep:

- the `pdfium-render` cargo feature in the workspace `Cargo.toml`,
- the tag, URL, SHA-256, and cache key in the CI workflow's two
  PDFium fetch steps and the release workflow's build matrix,
- the release tag, asset names, and SHA-256 values in
  `src/pdfium_pin.rs` (the installer's copy of the pin),
- the values in the tables above.

Bumping any of these values means re-extracting, so it bumps
`bookrack_extract::EXTRACTOR_VERSION`: the test
`extractor_version_is_pinned_with_the_pdfium_binary` in
`src/pdfium_pin.rs` restates the release tag and every archive digest
beside that version, and fails until both sides move. Bumping the
`pdfium-render` crate version itself also flips the behaviour-sensitive
deps hash that `crates/extract/tests/dep_hash.rs` anchors.

Parity across the copies is held mechanically: the inline tests in
`src/pdfium_pin.rs` compare this file, the workspace manifest's
`pdfium_NNNN` feature, and both workflow files against the pin table,
so a one-sided bump fails the extract test suite.
