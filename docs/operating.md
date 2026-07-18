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
control socket where the one-shot write subcommands arrive.

It takes two locks for its lifetime, and they answer different
questions. The session lock under the runtime directory admits one daemon
per runtime directory, and carries the lines other tools read to find the
session. The data-root lock, `<data_root>/.bookrack.lock`, admits one
writer per library: a second daemon pointed at the same root — even from
a different `BOOKRACK_RUNTIME_DIR` — fails to start and names the holder.
Offline commands that would destroy data take it too, so `bookrack
libraries remove --purge` refuses a root a daemon is serving rather than
deleting it underneath. Read-only commands take neither.

For a headless deployment — a systemd unit, a Windows service — run
`bookrack-mcp` instead. It serves the same MCP endpoint, and takes
`--with-queue-worker` when it should also process ingest jobs; without
that flag the queue-bound write methods short-circuit rather than
enqueue work no one will run. `bookrack run` and `bookrack-mcp` are
mutually exclusive against one library — each takes both locks — so stop
one before starting the other.

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

## The status card

```
bookrack status
```

One no-argument call answers "is a daemon running, which library does
it serve, is it busy". With a live daemon it renders a single card in
three sections — `daemon.*` (version, pid, uptime, state, MCP and
control endpoints), `library.*` (name, data root, chunk and ready-book
counts, disk usage), `queue.*` (pending, running, worker) — and a hint
pointing at `bookrack doctor` for the health probes the card
deliberately skips (embedder and reranker reachability involve network
round-trips; status stays fast). The identity rows come from the
daemon over RPC, so they name what is actually served, not what a lock
file once recorded. `library.name` is empty when the served data root
was selected directly by path — a normal state, not a fault.

The card distinguishes four verdicts:

| Verdict | How it is decided | Output | Exit |
| --- | --- | --- | --- |
| running | session lock held, control plane answers within 2s | full card | 0 |
| not running | no lock, or a leftover lock nobody holds | short card pointing at `bookrack run` | 0 |
| stale | lock held, control plane silent for 2s | error naming the lock to remove | 3 |
| unprobeable | lock held but records no control socket | short card with the recorded pid | 0 |

"Not running" is an answer, not an error, so it exits 0; a daemon
killed outright releases its flock and lands here, no cleanup needed.
"Stale" means a process still holds the flock but its control plane
has stopped answering — the same exit-3 contract as `bookrack run`
against a stale lock. "Unprobeable" means the lock names no control
address (a daemon started without a control listener, or a hand-edited
lock): the probe made no verdict that the daemon is dead, so status
does not either — but note that under `--quiet`, where the exit code
is the whole answer, this state is indistinguishable from a healthy
daemon; scripts that must tell them apart should parse `--json`
output, where an unreachable control plane surfaces as
`daemon.control: null`.

Other exits follow the standard buckets in
[control-plane.md](control-plane.md): 2 when an explicit `--library` /
`--data-dir` disagrees with what the running daemon serves (the
preflight names both sides), or when the daemon exits in the race
between the probe and the connect; RPC failures map to 1 / 2 / 4 as
everywhere else.

`--json` prints the same card as one JSON object
(`{ "daemon": …, "library": …, "queue": … }`); the short cards are
valid JSON objects too, with `daemon.running` saying which shape you
got. `--quiet` prints nothing and lets the exit code answer.

## Health and diagnostics

```
bookrack doctor
```

A one-screen health check: the data root resolves, the catalog and
corpus schemas open, PDFium is on disk, the file-descriptor limit is
sufficient, the Ollama daemon is reachable, the embed model is pulled,
each registry entry agrees with its on-disk identity manifest, and each
library's referenced index profile is coherent with its built index
stamps. When the effective profile enables a reranker, three more rows
cover its backend: the `llama-server` binary, the reranker model, and
whichever server is serving. A deprecated embed-model override, if one is
in effect, gets a row of its own. Each row is `OK`, `WARN`, or `FAIL`;
any `FAIL` exits non-zero so a script can branch on it. Pass `--json` for
a machine-readable report suitable for a bug attachment.

Four maintenance sub-commands cover one-off repairs; `--dry-run`
computes the plan for the last two without touching disk:

- `bookrack doctor --install-pdfium` downloads the pinned PDFium
  build, verifies its SHA-256, and unpacks it into the per-user
  managed directory the loader searches.
- `bookrack doctor --install-reranker` does the same for the pinned
  reranker artifacts — the `llama-server` binary and the cross-encoder
  model — which only a profile with a reranker stage needs.
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
