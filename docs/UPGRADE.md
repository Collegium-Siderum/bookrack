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
| `bookrack_extract::EXTRACTOR_VERSION` (manual bump) | `extractor_version` | same as above | re-ingest the affected sources |
| `text-splitter`, `bookrack_ingest::CHUNK_VERSION` | `chunk_version` | chunks → vectors | `bookrack vectors reembed` |
| `bookrack_normalize::NORMALIZE_VERSION` | `normalize_version` | chunks → vectors | `bookrack vectors reembed` |
| Embedding model name or vector width | `embed_model` / `vector_dim` | vectors only | `bookrack vectors reembed` |
| `catalog.db` schema bump | catalog `user_version` | none — migrated forward in place | open the library (migration is automatic) |
| `corpus.db` schema bump | corpus `schema_version` | corpus tree | `bookrack corpus rebuild` |
| `rusqlite`, `lancedb` | engine-level | normally none — upstream guarantees backward compatibility for reads | open the library |
| Workspace `READER_VERSION` (manual bump) | per-store `min_reader_version` on next write | none on its own — a guard against older binaries | open the library |

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
unchanged chunk text. Run this after bumping the embedding model,
the vector width, `CHUNK_VERSION`, or `NORMALIZE_VERSION`. The
delete-then-append flow is per-partition, so a long run can be
interrupted and resumed without losing progress.

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
