# Operating bookrack

This is the operator's guide to a running library: the daemon
lifecycle, submitting and watching ingest work, the OCR worklist,
health checks, and the observability surfaces. For configuration
(where the data root comes from, the registry, per-library settings)
see [configuration.md](configuration.md); for rebuilding derived data
after an upgrade see [UPGRADE.md](UPGRADE.md); for the raw JSON-RPC
control plane see [control-plane.md](control-plane.md).

## The daemon

`bookrack run` starts a foreground daemon that owns one library. It
serves MCP over streamable-HTTP at `127.0.0.1:8765/mcp` and a local
control socket where the one-shot write subcommands arrive. It holds a
machine-wide session lock for its lifetime, so only one daemon serves a
given library at a time.

For a headless deployment — a systemd unit, a Windows service — run
`bookrack-mcp` instead. It serves the same MCP endpoint, and takes
`--with-queue-worker` when it should also process ingest jobs; without
that flag the queue-bound write methods short-circuit rather than
enqueue work no one will run. `bookrack run` and `bookrack-mcp` are
mutually exclusive against one library — each takes the session lock —
so stop one before starting the other.

`bookrack quit` stops a running daemon. `bookrack logs` streams or
snapshots its log ring (see [Observability](#observability)).

## Ingesting

`bookrack ingest <path>` submits one or more files (or a directory
with `--recursive`) to the daemon and streams the queue worker's
progress until the batch reaches a terminal state. `bookrack papers
ingest <path>` is the parallel entry point for academic papers, which
live in a second cluster under the same data root.

```
bookrack ingest /path/to/book.epub
bookrack ingest --recursive /path/to/books-dir/
bookrack papers ingest --recursive /path/to/papers-dir/
```

The command exits `5` when an awaited batch had any `Failed` or
`Cancelled` job, so a script can branch on ingest success; a batch
whose sources all end in `needs_ocr` (see below) is a success and
exits `0`. `--no-wait` returns at queue-ack without awaiting, and so
always exits `0`.

`--hold-for-metadata` parks each book whose audit verdict is
`needs_work` at the metadata gate instead of embedding it; a curator
drives it past the gate with `bookrack metadata advance` (or
`approve`) once the record is corrected.

### Long ingestions

Ingestion is restartable: when a host suspends mid-run the embedding
step pauses with it and resumes idempotently once the host wakes. The
output is unchanged either way; only the wall-clock includes the time
spent asleep, which makes a run that crossed an idle-sleep window read
as far slower than it really was.

On every desktop platform the default idle-sleep policy will suspend a
backgrounded shell, so the natural unit to wrap is the `bookrack run`
daemon itself.

macOS — `caffeinate` blocks idle sleep without blocking display sleep:

```
caffeinate -i ./bookrack run
```

Linux (systemd) — `systemd-inhibit` takes the same lock the desktop
uses for media playback:

```
systemd-inhibit --what=idle --why="bookrack" ./bookrack run
```

Windows (PowerShell) — flip the active power scheme's idle-sleep
timeout to zero for the session, then restore it:

```
powercfg /change standby-timeout-ac 0
.\bookrack.exe run
powercfg /change standby-timeout-ac 30   # restore the previous value
```

For an unattended overnight run, prefer a wrapper that runs the restore
step even when the session exits with an error.

### The queue

Ingest work runs through a persistent queue the worker drains on a
fixed tick. `bookrack queue` inspects and steers it:

```
bookrack queue list          # jobs plus a per-state count footer
bookrack queue pause         # stop draining without losing the queue
bookrack queue resume
bookrack queue cancel <id>   # cancel by job-id prefix
bookrack queue clear         # drop every not-yet-running job
```

### The OCR worklist

A scan whose PDF carries no usable text layer is not a failure: the
extract stage records it as a `needs_ocr` intake anchor — its bytes,
a best-effort page count, and the rejection reason — rather than
dropping the job. `bookrack intake list-ocr-pending` lists every scan
source still awaiting OCR (`--json` emits a tool-agnostic manifest of
`intake_id` / `source_path` / `sha256` / `pages` / `reason`).

bookrack stays engine-agnostic: run any OCR tool over the manifest,
then bring the Markdown product back in through `bookrack intake ocr`.

```
bookrack intake list-ocr-pending
# ...run an OCR engine over each listed source...
bookrack intake ocr <ocr_md> --from-pdf <scan.pdf>
```

The OCR product is registered as an intake whose provenance references
the scan PDF's hash and flows through the normal STRUCTURE / CHUNK /
EMBED path. The expected page count comes from the source PDF's
`/Pages`; pass `--expected-pages` when PDFium cannot read the source,
and `--allow-partial` to accept a product that does not cover every
page.

## Health and diagnostics

```
bookrack doctor
```

A one-screen health check covering every install expectation: the data
root resolves, the catalog and corpus schemas open, PDFium is on disk,
the Ollama daemon is reachable, the embed model is pulled, each
registry entry agrees with its on-disk identity manifest, and each
library's referenced index profile is coherent with its built index
stamps. Each row is `OK`, `WARN`, or `FAIL`; any `FAIL` exits non-zero
so a script can branch on it. Pass `--json` for a machine-readable
report suitable for a bug attachment.

Three maintenance sub-commands cover one-off repairs; `--dry-run`
computes the plan for the last two without touching disk:

- `bookrack doctor --install-pdfium` downloads the pinned PDFium
  build, verifies its SHA-256, and unpacks it into the per-user
  managed directory the loader searches.
- `bookrack doctor --rename-envelopes` migrates envelope files from
  older libraries into the kind-prefixed filename layout.
- `bookrack doctor --backfill-ocr-derivation` recovers the OCR
  provenance edge on a library upgraded across the catalog v14
  boundary (see [UPGRADE.md](UPGRADE.md)); run it once so the OCR
  worklist does not re-list already-processed sources.

When something is broken, `bookrack diagnose` bundles crash reports,
recent logs, and a scrubbed catalog snapshot into a `.tar.gz` for
issue attachments. The scrubber removes local paths and book titles;
`--no-scrub` keeps them verbatim for a bundle kept locally.

## Observability

`bookrack logs` reads the daemon's log stream: `--follow` (the default
with no other flag) subscribes to the live broadcast, `--tail N`
snapshots the last N events and exits, and `--level` drops everything
below a severity. `--json` emits newline-delimited `LogEvent` records.

`bookrack runs` reports the pipeline-run registry — one row per
top-level pipeline command (ingest, dryrun, distill build, glean),
grouped with the audit rows it wrote:

```
bookrack runs list [--last N] [--command <name>]
bookrack runs show <run-id>       # verdict / flag / coverage histograms
```

`bookrack retrieval` inspects the `retrieval_calls` sidecar — one row
per single-store search invocation, stamped with the 16-hex corpus
fingerprint that served it and its per-hit detail:

```
bookrack retrieval list [--last N] [--corpus-fingerprint <hex>]
bookrack retrieval show <call-id>
```
