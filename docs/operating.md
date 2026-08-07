# Operating bookrack

This is the operator's guide to a running library: the daemon
lifecycle, submitting and watching ingest work, the OCR worklist,
health checks, and the observability surfaces. For configuration
(where the data root comes from, the registry, per-library settings)
see [configuration.md](configuration.md); for rebuilding derived data
after an upgrade see [UPGRADE.md](UPGRADE.md); for the raw JSON-RPC
control plane see [control-plane.md](control-plane.md).

## The daemon

`bookrack run` starts a foreground daemon. It serves MCP over
streamable-HTTP at `127.0.0.1:8765/mcp` and a local control socket
where the one-shot write subcommands arrive.

When its root is selected through the registry (`--library`, or the
registry's `default`), the daemon mounts **every** registered library
at bring-up: each one answers reads, and queue jobs route to their
target library by name. A root selected directly by path that the
registry does not know is served alone, as before. The set is not
fixed at bring-up: `bookrack libraries mount <name>` adds a registered
library to it and `bookrack libraries unmount <name>` takes one back
out, both without a restart. `libraries unmount` refuses the registry
default and the library the daemon came up under — the first is where
every unqualified command goes, the second is what `bookrack status`
reports the daemon as — and refuses a library with queue jobs still to
run. `bookrack libraries fork` mounts the clone it creates, so a forked
library is served on the same command.

`bookrack libraries list` reads the registry file, so it works with no
daemon and always shows every registered library. When a daemon does
answer, a `served` column marks the ones it holds — the two differ
whenever the daemon came up on a root the registry does not name, since
such a daemon serves that root alone. With no daemon the column is
absent rather than empty: nobody was asked, which is not the same
answer as nobody serving them. `bookrack status` reports the same set
from the other side, with each library's root and which one an
unqualified command reaches.

It takes the session lock plus one data-root lock per mounted library
for its lifetime, and they answer different questions. The session lock
under the runtime directory admits one daemon per runtime directory,
and carries the lines other tools read to find the session. The
data-root lock, `<data_root>/.bookrack.lock`, admits one writer per
library: a second daemon pointed at a served root — even from a
different `BOOKRACK_RUNTIME_DIR` — fails to start and names the holder.
Offline commands that would destroy data take it too, so `bookrack
libraries remove --purge` refuses a root a daemon is serving rather
than deleting it underneath. An eager daemon holds the lock on every
registered library's root, so a purge always meets a held one: give
that root back with `bookrack libraries unmount <name>` first, or —
for the registry default and the library the daemon came up under,
which unmounting refuses — `bookrack quit`. The root is released once
any call still using that library finishes, so a purge racing a long
read may need a second attempt. Read-only commands take neither.

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

## Naming an item

Books and papers are two pipelines with two catalogs, and each numbers
its intakes from one independently. `101` therefore names one book and
one paper, and nothing in the number says which is meant. A command
that already fixes the pipeline resolves that for you, which is why a
bare id works there and has always worked: everything under `bookrack
papers` reads the paper catalog, and the unprefixed write commands —
`metadata`, `remove` — read the book catalog.

An id can also be written with the pipeline in front of it:

```
bookrack papers show paper:101
bookrack papers metadata set paper:101 --field title --value "..."
bookrack metadata set book:12 --field title --value "..."
bookrack remove book:12
```

Both forms mean the same thing, and every command that takes an intake
id takes either. The prefixed form is what a listing prints, so an id
copied out of one command can be pasted into the next without editing.

The prefix must agree with the catalog the command reads. `bookrack
papers show book:12` names the other catalog, so it is refused while
arguments are being read — before anything is looked up or written —
and exits `2`:

```
$ bookrack papers show book:12
error: invalid value 'book:12' for '<INTAKE_ID>': "book:12" names the
book catalog, and this command reads the paper catalog. Pass it as
`paper:12`, drop the prefix, or read the item you named with `bookrack
show book:12`.
```

Refusing it is the point: the number is a valid paper id too, so
accepting it would act on a paper the operator never named. The book
side refuses `paper:101` the same way, and for the same reason.

### Reading one item by its id

A typed id needs no namespace to resolve it, so there is a verb that
takes one directly:

```
bookrack show book:12
bookrack show paper:101
```

`bookrack show paper:101` and `bookrack papers show 101` are the same
read; both stay. Under the card is the library the record came from —
the one `--library` named, or the one an unnamed call resolves to. If
no daemon can be asked, the line is absent rather than guessed.

`--json` prints the response as it came off the control plane, and
`--quiet` prints nothing; both skip the card and its library line.

`reference:` ids parse but have no read path on the command line yet:
the reference tools are published to MCP clients, and the control plane
carries no matching method. The refusal says so and names the surface
that does have it.


### Browsing and searching both catalogs

Three verbs read across the pipelines rather than within one:

```
bookrack list
bookrack find --title "Sample Title"
bookrack search "a sample phrase"
```

Each takes `--scope`, one of `book`, `paper`, or `all`; `all` is the
default, because reading across the pipelines is what these verbs are
for. The per-pipeline namespaces stay: `bookrack papers list` is
`bookrack list --scope paper`.

`list` and `find` page each side separately, so `--limit` and
`--offset` apply per side and `--scope all` can return up to twice the
limit. Each side reports its own total. `search` has no paging: one
method ranks both corpora against the same query and `--top-k` is the
whole result set.

Filters on `find` divide in two. The shared ones — `--title`,
`--contributor`, `--contributor-role`, `--language`, `--status` — name
a column both catalogs carry. The side-specific ones name a column only
one has: `--format` on the book side, `--year` / `--venue` / `--doi` on
the paper side. Each of those requires `--scope`, and a scope that
reaches the other side is refused before anything is sent:

```
$ bookrack find --year 2020 --scope all
bookrack: --year filters the paper side only
```

`--status` is shared but its vocabulary is not: each catalog enforces
its own, so a status only one side knows is refused by that side.

Under the rows is the library they came from, on the same rule the
single-item card follows. `--json` carries it too, but the payload is
assembled by the command rather than forwarded: `items` holds the rows
of every side that was read, each naming its own `kind` and carrying
the id that addresses it, and `list` / `find` add a `pages` block with
each side's own total. It is not the response of any one control-plane
method — read [control-plane.md](control-plane.md) for those.

One consequence worth knowing: a `--scope all` search is not recorded
in the retrieval sidecar, so it does not appear in `bookrack retrieval
list`. Its results span two corpora and no single corpus fingerprint
describes them. A single-sided search is recorded as usual.

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

The short card answers one thing the full card has no use for: the
`registry.default` row names the library a `bookrack run` here would
serve. It reads `(none)` when no registry is set or none of its entries
is the default, and `(unreadable: …)` when the registry file exists but
could not be read — reported in the row rather than as a failure, since
the card's own question was answered. `--json` carries the same three
states as a name, `null`, and `{"error": "…"}`.

The card distinguishes four verdicts:

| Verdict | How it is decided | Output | Exit |
| --- | --- | --- | --- |
| running | session lock held, control plane answers within 2s | full card | 0 |
| not running | no lock, or a leftover lock nobody holds | short card pointing at `bookrack run`, with the library a restart would serve | 0 |
| stale | lock held, control plane silent for 2s | error, with the steps to check the process before removing the lock | 3 |
| unprobeable | lock held but records no control socket | short card with the recorded pid | 0 |

"Not running" is an answer, not an error, so it exits 0; a daemon
killed outright releases its flock and lands here, no cleanup needed.
"Stale" means a process still holds the flock but its control plane
has stopped answering — the same exit-3 contract as `bookrack run`
against a stale lock. A suspended process looks exactly like this from
outside, so the message walks the pid it recorded: check the process is
there (`kill -0`), resume or end it if it is, and remove the lock only
once it is gone. Deleting the lock under a live daemon leaves two
processes believing they own the same session. "Unprobeable" means the lock names no control
address (a daemon started without a control listener, or a hand-edited
lock): the probe made no verdict that the daemon is dead, so status
does not either — but note that under `--quiet`, where the exit code
is the whole answer, this state is indistinguishable from a healthy
daemon; scripts that must tell them apart should parse `--json`
output, where an unreachable control plane surfaces as
`daemon.control: null`.

Other exits follow the standard buckets in
[control-plane.md](control-plane.md): 2 when an explicit selection
cannot be routed — `--library` naming a library the registry does not
hold, or `--data-dir` / `BOOKRACK_DATA_DIR` naming a root no registry
entry claims — or when the daemon exits in the race between the probe
and the connect; RPC failures map to 1 / 2 / 4 as everywhere else. A
selection the registry can name is routed to the daemon, which serves
every registered library, so it no longer disagrees with anything.

`--json` prints the same card as one JSON object
(`{ "daemon": …, "library": …, "queue": … }`); the short cards are
valid JSON objects too, with `daemon.running` saying which shape you
got. `--quiet` prints nothing and lets the exit code answer.

## Health and diagnostics

```
bookrack doctor
```

A one-screen health check: the data root resolves, every store under it
is accounted for, PDFium is on disk, the file-descriptor limit is
sufficient, the Ollama daemon is reachable, the embed model is pulled,
each registry entry agrees with its on-disk identity manifest, and each
library's referenced index profile is coherent with its built index
stamps.

The store rows cover the whole data root — the book catalog, corpus and
vector store, the three paper stores, the reference store, and the
directory a schema migration backs databases up into. A store a library
does not use is reported present-and-absent rather than warned about:
`OK` with a note saying it was looked for and is legitimately missing,
which is a different answer from a store nobody checked. A pipeline
whose content is ingested but whose vector index is missing is a `WARN`
— that content answers no search. A store that is there is opened
through its read-only door, so one written by a newer binary or
corrupted is a `FAIL` naming the reason rather than an `OK` naming its
path; those doors take no write lock and materialise nothing, so the
check is safe beside a running daemon. What a store *holds* — intake
counts, missing files, drift against a rebuild — is `bookrack verify`
and, on a running daemon, `bookrack status`. One more row covers what
those cannot:
free space on the volume holding the data root, warned on below the floor
`bookrack config fixed` reports, since a store that exists is not the same
as a store that can grow.

The `pipeline runs` row covers the registry's open rows — the ones
still reading `running`. A command that dies between registering a run
and closing it leaves a row that looks exactly like a run in flight
and that nothing else ever revisits. To tell the two apart, an open run
holds a lock file under `<data_root>/.run-locks/` for as long as its
process lives; the operating system releases it however the process
ends, so the row reports three cases: a run still owned by a live
process, one whose record is there and unheld — abandoned, and the only
case that warns — and one carrying no record at all, which is reported
but never acted on, since nothing proves its owner is gone. Runs opened
by an earlier version have no record and fall in that third case.

The registry sections need a readable registry: when one is
configured but cannot be read, they report that as a row apiece instead
of dropping out of the report, which would be indistinguishable from an
install that has no registry at all. When the effective profile enables
a reranker, three more rows cover its backend: the `llama-server`
binary, the reranker model, and whichever server is serving. A last row
covers the MCP endpoint itself: it sends a real `initialize` at the
address and checks that bookrack is what answers, so a port taken over
by another service is reported rather than assumed healthy. With a
daemon running the row is about the address that daemon bound; with
none running it is about the configured address, and a free one is
`OK`.

Which of those two the report is comes from the first row. A daemon
answering `doctor.gather` names the control socket it answered on; a
report gathered in-process says so, and warns when a daemon holds the
session lock but did not answer — that fallback is what silently
produces the thinner report. Two further rows cover what belongs to the
daemon rather than to a library: its state directory and the queue
snapshot inside it, whose parse failure is a `FAIL`, since the daemon
reads the same file at start-up and refuses to come up on it.

Each row is
`OK`, `WARN`, or `FAIL`; any `FAIL` exits non-zero so a script can branch
on it. Pass `--json` for a machine-readable report suitable for a bug
attachment.

Five maintenance sub-commands cover one-off repairs; `--dry-run`
computes the plan for the last three without touching disk:

- `bookrack doctor --install-pdfium` downloads the pinned PDFium
  build, verifies its SHA-256, and unpacks it into the per-user
  managed directory the loader searches.
- `bookrack doctor --install-reranker` does the same for the pinned
  reranker artifacts — the `llama-server` binary and the cross-encoder
  model — which only a profile with a reranker stage needs. The model
  alone is a ~610 MiB download; the row that asks for it names the size,
  and `bookrack config fixed` reports the free-space floor the disk row
  warns under.
- `bookrack doctor --rename-envelopes` migrates envelope files from
  older libraries into the kind-prefixed filename layout.
- `bookrack doctor --backfill-ocr-derivation` recovers the OCR
  provenance edge on a library upgraded across the catalog v14
  boundary (see [UPGRADE.md](UPGRADE.md)); run it once so the OCR
  worklist does not re-list already-processed sources.
- `bookrack doctor --close-abandoned-runs` closes registry rows left
  at `running` by a command whose process died, stamping each
  `abandoned`. See the `pipeline runs` row below for how a run's owner
  is judged; only rows proven ownerless are touched.

The last two write the catalog directly, so both are refused while a
daemon is serving the library — stop it with `bookrack quit` first.

When something is broken, `bookrack diagnose` bundles crash reports,
recent logs, and a scrubbed catalog snapshot into a `.tar.gz` for
issue attachments. The scrubber removes local paths and book titles;
`--no-scrub` keeps them verbatim for a bundle kept locally.

A redaction whose input the host does not expose is reported rather
than skipped quietly: the command warns on stderr, and the bundle's
`manifest.json` lists the shortfall under `scrub_gaps` next to
`scrubbed`. There are two cases, and both leave home paths outside the
generic user-root patterns unredacted. A host that exposes no home
directory at all — neither `HOME` nor the platform lookup — reports
`home_dir`. A `HOME` that names a directory this machine does not have
reports `home_dir_unverified`: the redaction ran, against a prefix
nothing lives under, and `HOME` is writable by anything that can start
the process, a `.env` above the working directory included. Fix either
by correcting `HOME` and running again, or read the bundle before
attaching it.

## Observability

`bookrack logs` reads the daemon's log stream: `--follow` (the default
with no other flag) subscribes to the live broadcast, `--tail N`
snapshots the last N events and exits, and `--level` drops everything
below a severity. `--json` emits newline-delimited `LogEvent` records.

`bookrack runs` reports the pipeline-run registry — one row per
top-level command that drives a pipeline over a set of items, grouped
with the audit rows it wrote:

```
bookrack runs list [--last N] [--command <name>]
bookrack runs show <run-id>       # verdict / flag / coverage histograms
```

The registered command names are `ingest`, `dryrun`, `papers_dryrun`,
`distill_build`, `glean`, and the whole-library maintenance passes
`reembed`, `reset`, `papers_reembed`, and `papers_reset`; any of them
is a valid `--command` filter. The passes write no audit rows, so
`runs show` renders them without the histograms.

A run still reading `running` whose owning process is gone prints its
status as `abandoned?` — the question mark marks the column as this
command's reading of the run's liveness record rather than what the
database stores, and the table's footer names the repair that resolves
it. Per-item verbs such
as `metadata reaudit` are deliberately not registered — they
recompute one intake's rollup, and their trail lives on the
`item_pipeline_audit` chain instead.

`bookrack retrieval` inspects the `retrieval_calls` sidecar — one row
per single-store search invocation, stamped with the 16-hex corpus
fingerprint that served it and its per-hit detail:

```
bookrack retrieval list [--last N] [--corpus-fingerprint <hex>]
bookrack retrieval show <call-id>
```
