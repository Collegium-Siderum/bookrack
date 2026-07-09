# Pinned reranker artifacts

The reranker backend spawns a `llama-server` subprocess and points it
at a GGUF reranker model. Neither artifact is vendored; both are
pinned here — the binary because its rerank endpoint behaviour moved
fast upstream and an arbitrary build cannot be trusted to match the
request shape the client sends, the model because naive GGUF
conversions of the Qwen3 rerankers have shipped with missing tensors
that silently degrade every score. This file is the single source of
truth for the pinned pair; `src/llama_server_pin.rs` and
`src/reranker_model_pin.rs` carry the same values as constants for the
installer.

## The binary pin

| What | Value |
| --- | --- |
| Release tag | `b9934` (2026-07-09) |
| Source | <https://github.com/ggml-org/llama.cpp> (MIT) |
| Executable in archive | `llama-b9934/llama-server` |

Upstream publishes no checksums, so these SHA-256 values were computed
locally when the version was pinned; the installer verifies the
downloaded archive against them.

| Asset | SHA-256 |
| --- | --- |
| `llama-b9934-bin-macos-arm64.tar.gz` | `f9338784c562b91b48e3044aab29f7f2b7664da456f05e945bbc10f4b546b502` |
| `llama-b9934-bin-macos-x64.tar.gz` | `4babcdd101adcd8b312655ce86cfa8ae3f97daa3c74decafbf9136cd4aaf40c6` |
| `llama-b9934-bin-ubuntu-arm64.tar.gz` | `359515ef1290e64835b547475fcb84200b560bef5c400e6494520120146e4507` |
| `llama-b9934-bin-ubuntu-x64.tar.gz` | `a01b9ec4522047a5e2e8abc17cc92795e5710b125e00026f4916d66f41553b67` |

Download URL template:

```
https://github.com/ggml-org/llama.cpp/releases/download/b9934/llama-b9934-bin-<platform>.tar.gz
```

The archives place the executable and the `libggml*`/`libllama`
dynamic libraries it loads side by side in one top-level
`llama-<tag>/` directory, so the installer unpacks the whole archive
into a per-build managed directory rather than extracting a single
member. No Windows row is pinned: the upstream Windows assets are
`.zip`, a format the installer does not unpack; doctor reports the gap
on that platform.

## The model pin

| What | Value |
| --- | --- |
| Registry tag | `Qwen3-Reranker-0.6B` |
| Repository | `ggml-org/Qwen3-Reranker-0.6B-Q8_0-GGUF` (Apache-2.0) |
| File | `qwen3-reranker-0.6b-q8_0.gguf` |
| SHA-256 | `22c9979ce4fbcdc5acdc310c6641c32797eff1aa980b8f7a2db8a8ea23429a48` |
| Size | 639,153,184 bytes |

The `ggml-org` conversion is pinned because it is published by the
llama.cpp organization alongside its native Qwen3-Reranker support —
the provenance most credibly immune to the missing-tensor conversion
defects seen elsewhere. The 4B reranker is deliberately not pinned:
its conversion history and per-pair latency both fail the interactive
budget.

Download URL template:

```
https://huggingface.co/<repo>/resolve/main/<file>
```

## How the artifacts are found

Both chains stop at the first hit; an explicit override is
authoritative, with no fallback behind it, and is trusted without a
checksum.

The `llama-server` executable:

1. `BOOKRACK_LLAMA_SERVER_BIN` — names the executable itself.
2. `llama-server` in the running executable's own directory (the
   release-archive layout).
3. The per-user managed directory
   (`<platform data dir>/bookrack/llama-server/<tag>/`), which
   `bookrack doctor --install-reranker` populates.

The model file:

1. `BOOKRACK_RERANKER_MODEL` — names the `.gguf` file itself.
2. `<platform data dir>/bookrack/models/<file>`, which the same
   doctor verb populates. Model weights do not ship in release
   archives, so there is no executable-adjacent stop.

## When this changes

Bumping either pin is a behaviour-sensitive change: a different build
or conversion can score pairs differently, which reorders search
results. The reranker runs at query time only — nothing about it
enters `index_meta` or any stamp — so a bump invalidates no derived
layer and triggers no re-derivation, but it is an observable change:
pre-1.0 that is a minor bump, recorded under `Changed` in
`CHANGELOG.md`. Update, in lockstep:

- the tag, asset names, SHA-256 values, and executable path in
  `src/llama_server_pin.rs`, or the model row in
  `src/reranker_model_pin.rs`,
- the values in the tables above.

When picking a new binary build, take a recent one: Qwen3-Reranker
support landed upstream in 2025-09 and several rerank-specific defects
were fixed well after it, so an old build is a regression, not a
conservative choice.
