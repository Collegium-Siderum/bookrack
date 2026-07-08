# Upgrading bookrack

A bookrack binary is a versioned reader and writer of four on-disk
stores. Most upgrades are transparent: install the new build, open the
library, keep working. Some upgrades require derived content to be
rebuilt before search returns correct results. This document is the
runbook for the cases that need action.

## Compatibility model in one paragraph

Each on-disk store records the build parameters of the data it holds:
`catalog.db` carries the schema revision and a per-row
`extractor_version`, `corpus.db` carries `embed_model` / `vector_dim` /
`chunk_version` / `normalize_version` in `index_meta`, and the LanceDB
chunks table is gated by those same stamps at serve time. Every store
also carries a `min_reader_version` value the writer last stamped;
opening a store whose stamp exceeds this binary's `READER_VERSION`
fails with `ReaderTooOld`. A binary that bumps a behaviour-sensitive
dependency or a stamp const reads the old stamps as stale and refuses
to serve until the affected derived layer is rebuilt.

## Bump-to-refresh matrix

The column "refresh command" lists the cheapest CLI invocation that
restores a consistent library after the dependency or constant in the
left column moves. Run them in the order listed; later steps assume
earlier ones already happened.

| Dependency or constant bumped | Stamp it advances | Layers that go stale | Refresh command |
|---|---|---|---|
| `rbook`, `scraper`, `encoding_rs`, `unicode-normalization`, `pdfium-render` | `extractor_version` | extract → structure → chunks → vectors | re-ingest the affected sources |
| `bookrack_extract::EXTRACTOR_VERSION` (manual bump) | `extractor_version` | same as above | re-ingest the affected sources (book-side); re-glean the affected papers (paper-side — the no-op fast path now invalidates on a stamp mismatch) |
| Externalised extract-side knobs (HTML block / skip tags, PDF quality thresholds, language codes / ratios) — bumped 3 → 4 | `extractor_version` | same as above | re-ingest the affected sources |
| `Block` carries per-paragraph `BlockStyle` geometry on the PDF path (font-size median / p90, bold-majority flag, line count, first-line left, normalized above-gap) — bumped 5 → 6 | `extractor_version` | same as above | re-ingest the affected PDF sources (book-side TXT / EPUB / OCR partitions read fine without a refresh, as their `style` is `None` on both old and new envelopes) |
| `bookrack_extract::OCR_INTAKE_VERSION` (manual bump) | `extractor_version` on OCR rows only | OCR extract → structure → chunks → vectors | re-run `bookrack intake ocr` against each affected OCR product |
| `text-splitter`, `bookrack_ingest::CHUNK_VERSION` | `chunk_version` | chunks → vectors | `bookrack vectors reembed` (books); `bookrack papers vectors reembed` (papers) |
| `bookrack_glean::CHUNK_VERSION` | `chunk_version` on the papers store | papers chunks → papers vectors | `bookrack papers vectors reset` |
| `bookrack_normalize::NORMALIZE_VERSION` | `normalize_version` | chunks → vectors | `bookrack vectors reembed` (books); `bookrack papers vectors reembed` (papers) |
| Embedding model name or vector width | `embed_model` / `vector_dim` | chunks → vectors | `bookrack libraries fork` (try it side by side) or `bookrack vectors reset` + `bookrack papers vectors reset` (in place) — see [Switching the embedding model](#switching-the-embedding-model) |
| `catalog.db` / `papers_catalog.db` schema bump | catalog `user_version` | none — migrated forward in place (one exception: the v14 OCR-derivation edge, see below) | open the library (migration is automatic); after upgrading a library that predates v14, run `bookrack doctor --backfill-ocr-derivation` once |
| `corpus.db` schema bump | corpus `schema_version` | corpus tree | `bookrack corpus rebuild` |
| `papers_corpus.db` schema bump | papers corpus `schema_version` | papers corpus tree | `bookrack papers corpus rebuild` |
| `rusqlite`, `lancedb` | engine-level | normally none — upstream guarantees backward compatibility for reads | open the library |
| Workspace `READER_VERSION` (manual bump) | per-store `min_reader_version` on next write | none on its own — a guard against older binaries | open the library |

### Stamps that advance without a refresh command

Some version dimensions move without invalidating anything on disk, so
they carry no runbook step:

- **Queue document schema** (`QUEUE_SCHEMA_VERSION`, now `6`) — the
  persistent ingest queue is versioned on its own track. An older
  document loads unchanged in a newer binary; the bump is one-way, so
  an older binary will not read a queue document a newer one wrote.
- **Corpus fingerprint** — a 16-hex digest of the five corpus stamps
  (`embed_model` / `vector_dim` / `chunk_version` / `normalize_version`
  / ANN kind) recorded per search in `retrieval_calls`. It is a
  composition of stamps already gated at serve time and gates nothing
  itself.
- **Audit-profile fingerprints** — pin the profile identity that judged
  each audit row (`node_paper_audit`, `book_distill_audit`). Provenance
  only; no derived layer depends on them.

## Commands and when to use each

`bookrack corpus rebuild` — refresh `corpus.db` from the v1 extraction
envelopes recorded in the opaque store, without re-extracting any
source file. Run this after a `corpus.db` schema bump, or to recover
from a deleted `corpus.db` when the chunks table is still on disk.
The L0-only invocation also re-stamps `index_meta` from the existing
chunks, so search keeps running afterwards. Pair with
`--include-vectors` to follow it with a reembed in one command.

`bookrack vectors reembed` — read each book's chunks back from
LanceDB, drop their vectors, and run the active embedder over the
unchanged chunk text. Run this after a `CHUNK_VERSION` or
`NORMALIZE_VERSION` bump — the chunks are re-derived in-place at the
existing vector dimension, so the embedding model must stay the same.
The delete-then-append flow is per-partition, so a long run can be
interrupted and resumed without losing progress.

`bookrack libraries fork <name> --data-dir <path>` — clone the
current data root into a sibling library. The envelope store
(`books/`) is hardlinked by default (falls back to copy when the
target lives on a different filesystem), and `catalog.db` /
`corpus.db` are copied as snapshots. The `lancedb/` directory is
**not** carried over: the new library starts with no vectors and
no `index_meta` stamps. Follow up with `vectors reset` against the
new library to rebuild it under whatever embedding model the
daemon's environment points at. The recommended path for trying a
new embedding model.

`bookrack vectors reset` — drop the chunks table and the
`vectors_meta.json` sidecar, clear the corpus `index_meta` stamps,
demote every `Embedded` intake back to `Extracted`, then re-chunk
from the corpus nodes and re-embed with whatever embedding model
the env points at. **The old vectors are unrecoverable** — there
is no rollback. Use this when switching the embedding model in
place; use `libraries fork` first if you would rather trial the
new model on a clone. Resumable with `--resume` after an
interruption.

`bookrack vectors rebuild` — rebuild the ANN index on top of the
existing chunks table. Run this after the indexing parameters
(kind / partition count / nprobes) change, or after enough churn has
accumulated to make the current index a bad fit. The chunks
themselves are not touched.

Re-ingesting a source file — the only path that refreshes
`extractor_version`. Drop the catalog row's `Embedded` state and
ingest the file again; the L0 envelope is replaced and a fresh chunk
plan + reembed follow. Use `bookrack corpus rebuild --stale-only` and
`bookrack vectors reembed --stale-only` to scope a sweep to the
partitions actually behind, identified by their stored
`extractor_version` not matching this binary's.

OCR intakes carry their own version dimension and sit on a sibling
track: `OCR_INTAKE_VERSION` advances them, `EXTRACTOR_VERSION` does
not. `corpus rebuild --stale-only` and `vectors reembed --stale-only`
target born-digital partitions and skip OCR rows, so a bump to
`OCR_INTAKE_VERSION` is refreshed by re-running `bookrack intake ocr`
against the OCR markdown the user produced — the original product is
the source of truth for the OCR side. `Catalog::stale_ocr_partitions`
is the catalog-level equivalent of `stale_partitions` for the OCR
side; it is exposed as a query but not yet wired to a CLI sweep.

`bookrack doctor --backfill-ocr-derivation` — a one-time offline repair
for libraries upgraded across the catalog v14 boundary. v14 added the
`intake.derived_from_sha256` edge that links an OCR product back to the
scan source it came from; OCR intakes written before v14 lack it, so
the OCR worklist (`bookrack intake list-ocr-pending`) would re-list
scan sources that were in fact already processed. The backfill recovers
the edge by reading each OCR intake's recorded provenance. It is
refused while a daemon is serving the library, and `--dry-run` opens
the catalog read-only so a plan never migrates or writes. Libraries
created at or after v14 never need it.

## Recommended window

A refresh that touches `extractor_version` or the embedding model is
the most expensive — it re-runs every behaviour-sensitive parser on
every book, and re-embeds every chunk through the embedding model.
Allocate a low-activity window for these runs; on macOS, wrap them in
`caffeinate -i` so the host does not idle-sleep mid-run (see the
README). A reembed that only changes `CHUNK_VERSION` or
`NORMALIZE_VERSION` re-uses the existing chunk text but still costs
one embedding pass per book.

## What never refreshes automatically

bookrack never decides, on its own, that derived content is stale and
re-derives it in the background. The decision is the operator's: run
the refresh command at a time of their choosing. `bookrack corpus
rebuild --stale-only` and the matching `vectors reembed --stale-only`
flag fold the on-disk `extractor_version` against this binary's,
which is the closest the tool gets to "tell me what needs work";
neither command runs without the operator typing it.

## Switching the embedding model

The chunks table is single-dim and tied to one model: stamps in
`corpus.db`'s `index_meta` encode the exact `embed_model` /
`vector_dim` pair, and both the build-side gate (next embed run) and
the serve-side gate (every query) refuse anything else. A library is
not a multi-model index.

Two supported workflows. Both execute inside the running daemon —
`vectors reset` is dispatched over its control plane — and the
embedding model resolves from the daemon process's environment
(`BOOKRACK_EMBED_MODEL`, then `config.toml`, then the default) at
the moment the reset runs. Setting the variable on the client
invocation has no effect: the daemon must be restarted with the new
model before the reset, and restarted once more afterwards so the
serve path picks up the new stamps.

### Side by side: `libraries fork` then `vectors reset` on the clone

The recommended path. The old library stays intact while a trial
clone runs against the new model.

```sh
# with the daemon running on the source library:
bookrack libraries fork trial --data-dir /abs/path/to/trial
bookrack quit
# restart against the clone, new model in the daemon's environment:
BOOKRACK_EMBED_MODEL=qwen3-embedding:4b bookrack --library trial run
# from a second shell; type RESET at the prompt (--yes skips it):
bookrack vectors reset
# evaluate the clone, compare with the original library
# decide:
#   keep the new model: rm -rf <old data root>, point the registry's
#     default at 'trial'
#   discard: rm -rf /abs/path/to/trial, remove the trial entry from
#     the registry
```

`fork` hardlinks the envelope store (`books/`) by default, so the
trial library carries no extra envelope bytes. It falls back to a
byte copy automatically when the target lives on a different
filesystem; `--copy-mode copy` forces the byte copy unconditionally.

### In place: `vectors reset`

For users on disk-tight hosts or fully confident in the new model.

```sh
# restart the daemon with the new model in its environment:
BOOKRACK_EMBED_MODEL=qwen3-embedding:4b bookrack run
# from a second shell; type RESET at the prompt (--yes skips it):
bookrack vectors reset
# restart the daemon once the reset completes
```

The old vectors are dropped before the new ones land — there is no
rollback. To "go back" to the previous model, repeat `reset` with the
old `BOOKRACK_EMBED_MODEL`; bookrack treats reverse as another
reset, not as undo.

If `reset` is interrupted mid-run (a kill or an embed-backend
outage), rerun with `bookrack vectors reset --resume` to pick up
whatever intakes are still in `Extracted` without redoing the
destructive A-D steps.

### What bookrack does not do

- **Zero-downtime swap, A/B querying, per-book model routing** —
  none are supported. The data model is one model per library; use
  `fork` and run two libraries side by side for any of these.
- **Detect that Ollama silently retagged a model** — bookrack
  compares model name strings, not content digests. If a model tag
  gets new weights upstream with the same dimension, bookrack will
  serve the old library against the new embedder without warning.
