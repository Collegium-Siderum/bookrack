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
| `pdfium-render` crate | `0.9` (resolves to 0.9.5) |
| Cargo feature | `pdfium_8125` |
| Native binary release | `chromium/8125` (PDFium 158.0.8125.0) |
| Binary source | <https://github.com/bblanchon/pdfium-binaries> |

The feature build number and the release build number are both `8125`
— they must stay equal. In `pdfium-render` 0.9.5, `pdfium_8125` is the
newest concrete API version (`pdfium_latest` aliases it); pinning the
concrete feature rather than `pdfium_latest` keeps the build
reproducible when a later crate release moves `pdfium_latest` forward.

## Known issues

Advisories open against the pinned build are listed here, one row
each, with the advisory, its severity, the component, and the build
carrying the fix. Treat each row as reachable from any PDF source; the
operator-facing consequence is the note on trusted sources under
*Ingesting* in `docs/operating.md`.

No advisory is open against the pinned build at present.

Advisories against PDFium's JavaScript and XFA layers are not listed:
the non-V8 archives compile neither layer.

## Binary archives

The non-V8 archives are used (no JavaScript engine is needed). These
SHA-256 values are the digests GitHub records for each release asset,
and the CI download step verifies against them — a corrupted download
or a silently re-cut upstream asset then fails loudly.

| Asset | SHA-256 |
| --- | --- |
| `pdfium-win-x64.tgz` | `9a964261760cff52570aabf038ab346d3a5034dd8b30d0d5fe8861235d356b8b` |
| `pdfium-linux-x64.tgz` | `e1c3ce6e270cb5e0c20816b379ad9f2c1309972697c505f508de92bf5e7d6659` |
| `pdfium-mac-arm64.tgz` | `4939bd935ac83ddc09a3a4f038bc43a19cea1660316a3667f26cb07a070e139d` |
| `pdfium-mac-x64.tgz` | `8a5193535b80c7215d8932f6647ae5097465b6dba2811602c21e120c2af112b0` |

Download URL template:

```
https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F8125/pdfium-<platform>.tgz
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
3. The per-user managed directory of the pinned build
   (`<platform data dir>/bookrack/pdfium/<release tag>/`, one directory
   per pinned build), which `bookrack doctor --install-pdfium` and the
   first-run wizard's download offer populate with that build.

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
