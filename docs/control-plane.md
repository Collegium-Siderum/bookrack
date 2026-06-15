# bookrack control plane

The bookrack daemon exposes a local-only JSON-RPC 2.0 control plane
alongside its MCP HTTP listener. Operator tooling, the in-process
REPL, and (in later phases) one-shot CLI subcommands reach the
running daemon through this surface; the MCP listener stays read-only
and tool-scoped.

## Transport

- Unix-likes: Unix domain socket bound at `<runtime_dir>/control.sock`.
- Windows: named pipe bound at `\\.\pipe\bookrack-control`.
- Discovery: clients read `<runtime_dir>/bookrack.tty.lock` and pick
  up the `control_sock=<path>` line. The lock file's `pid=` and
  `mcp=` lines are unchanged.

## Protocol

- JSON-RPC 2.0 over newline-delimited JSON. Each TCP-style frame is
  one valid JSON value terminated by a single `\n`.
- Requests carry `{"jsonrpc":"2.0","id":<value>,"method":<name>,"params":<value>}`.
- Responses carry `{"jsonrpc":"2.0","id":<value>,"result":<value>}`
  on success and `{"jsonrpc":"2.0","id":<value>,"error":{...}}` on
  failure.
- Server-initiated notifications carry
  `{"jsonrpc":"2.0","method":"event","params":{"channel":<name>,"value":<payload>}}`
  with an optional `lag: true` marker the client uses to resync via
  `events.snapshot`.

### Error codes

- `-32700` parse error
- `-32600` invalid request
- `-32601` method not found
- `-32602` invalid params
- `-32603` internal error
- `-32001` busy (bookrack-specific; a write command is already in flight)
- `-32002` not ready (bookrack-specific; the runtime has not finished
  initialising the resource the method needs)
- `-32010` invalid library (bookrack-specific; a `library` param does
  not exist in the registry)
- `-32011` job not found (bookrack-specific; `ingest.cancel` named a
  job id no longer in the queue document)

## Methods (Phase 1)

- `daemon.version` — `{ version, started_at }`.
- `daemon.shutdown` — fires the shared shutdown broadcast; the
  response is `null` and is written before the listener stops.
- `status` — `{ state, queue_pending, queue_running }`. `state` is
  one of `idle`, `writing`, `degraded`, `stopping`.
- `doctor.gather` — JSON serialisation of the same report the
  `bookrack doctor` subcommand prints.
- `queue.list` — `{ schema_version, paused, jobs }`. `params.limit`
  optionally caps the jobs slice.
- `library.list` — array of `{ name, default, dimension }` entries.
- `library.info` — full status card for one library;
  `params.name` selects which.
- `library.set_default` — `{ name }` → `{ ok: true, name }`. Move
  the registry's default-library pointer to `name`. The change
  lives in the daemon's in-memory registry only; the on-disk
  library registry stays as written. Returns `-32602` with the
  list of known libraries when `name` is unregistered. Fires a
  `library.changed` event so subscribers can refresh their view.
- `events.subscribe` — `{ subscribed: true }` followed by an
  immediate snapshot bundle of `daemon.state`, `queue.list`,
  `queue.tick`, `library.list`, `library.changed`,
  `mcp.availability`, `daemon.version`.
- `events.snapshot` — explicit re-fetch of named channels;
  `params.channels` is the list to refresh.
- `logs.tail` — `{ n? }` → `{ events, returned }`. Snapshot the most
  recent `n` events from the daemon's in-memory log ring buffer,
  oldest first. `n` defaults to 100 and is capped server-side at
  1024. Peer of the `session.logs_tail` MCP tool; same backing
  buffer.

## Methods (Phase 2)

- `ingest.submit` — `{ paths, library?, priority?, force?,
  recursive?, hold_for_metadata? }` → `{ job_ids: [<uuid v7>] }`.
  Appends jobs to the persistent queue document; the worker picks
  them up on the next 200 ms tick. When `recursive` is `true`,
  every directory in `paths` is walked depth-first and expanded to
  its supported-format files before enqueueing; files passed
  directly are enqueued verbatim. With `recursive` omitted or
  `false`, directory paths reach the worker as-is and fail there.
  When `hold_for_metadata` is `true`, the worker parks every book
  whose audit verdict is `needs_work` at STRUCTURE, skipping CHUNK
  and EMBED until a curator drives it past the metadata gate.
- `ingest.cancel` — `{ job_id }` → `{ ok: true }`. Marks the matching
  pending or running job as cancelled.
- `metadata.set` / `metadata.clear` / `metadata.void` /
  `metadata.reaudit` / `metadata.ack` / `metadata.approve` /
  `metadata.reject` / `metadata.advance` — same params as the
  `bookrack metadata` REPL subcommands; return `{ ok: true }` on
  success. `metadata.advance` resumes CHUNK→EMBED for a book held
  at the metadata gate by `--hold-for-metadata`. `metadata.approve`
  triggers the same resume implicitly when the book is parked. `metadata.void`
  (added post-Phase 5) writes a NULL override that suppresses a
  field's extracted value until a correct one is set;
  `metadata.clear` removes it. `metadata.reaudit` (same vintage)
  re-runs the plausibility audit from the book's cached extraction
  envelope, refreshing the stored verdict / confidence; the review
  status is untouched.
- `metadata.contributor_add` / `metadata.contributor_remove` (same
  vintage) — curate the contributor rows: `contributor_add` writes an
  `origin = "user"` attribution that re-ingest preserves and
  `find_books` matches; `contributor_remove` deletes one row by the
  `contributor_id` that `show_book` lists, whatever its origin.
- `vectors.rebuild` / `vectors.reembed` / `vectors.reset` /
  `vectors.drop` — mirror the matching `bookrack vectors` actions.
  `vectors.drop` takes no params.
- `corpus.rebuild` — `{ include_vectors?, book?, stale_only?, dry_run?, yes? }`.
- `stamps.reconcile` — no params; rewrites the corpus index stamps.
- `papers.corpus_rebuild` —
  `{ include_vectors?, paper?, stale_only?, dry_run?, yes? }`. Peer of
  `corpus.rebuild` for the paper pipeline; reconstructs
  `papers_corpus.db` from envelopes in `papers_dir` and reseats each
  abstract leaf from `node_publication_attrs`.
- `papers.vectors_rebuild` / `papers.vectors_reembed` /
  `papers.vectors_reset` / `papers.vectors_drop` — peers of
  `vectors.*` against `lancedb_papers`. Same param shapes; the
  reembed variant takes `paper?` instead of `book?`.
- `papers.stamps_reconcile` — no params; rewrites the
  `papers_corpus.db` index stamps from the active embedder.
- `papers.dryrun` — `{ path, out?, no_chunk? }`. Peer of `dryrun` for
  the paper pipeline; writes a paper-shaped JSONL plus a summary
  sidecar under `<data_root>/dryruns/dryrun-paper-...`. Reports
  IDENTIFY hit rates (DOI / arXiv / ISSN / venue / title / year /
  abstract) and the predicted STRUCTURE node shape per file.
- `papers.metadata.reaudit` — `{ intake_id, audit_profile?,
  library? }`. Re-runs the paper-side metadata audit against the
  intake's cached extraction envelope and writes only the
  `confidence` / `audit_verdict` rollup on
  `node_publication_attrs`. The named profile (`default` /
  `trust-source` / `strict`) takes precedence; absent it, the
  `<data_root>/audit-rules/paper_audit_profile.local.toml` overlay
  applies on top of the shipped default.
- `papers.metadata.set` — `{ intake_id, field, value, confirmed?,
  library? }`. Writes an override on one paper field. `field` must
  belong to the editable set
  (`title`, `subtitle`, `publisher`, `year`, `language`, `series`,
  `doi`, `arxiv_id`, `issn`, `container_title`, `abstract`,
  `csl_type`). `confirmed` marks the override as having been checked
  against the source.
- `papers.metadata.clear` — `{ intake_id, field, library? }`.
  Removes the override row on one field, reverting to the extracted
  value. Returns `{ removed: bool }`.
- `papers.metadata.void` — `{ intake_id, field, library? }`.
  Writes a value-less override row so the field reads as
  deliberately empty rather than extracted.
- `papers.metadata.ack` / `.approve` / `.reject` / `.reopen` —
  `{ intake_id, reviewer?, notes?, library? }`. Move the review
  row through the four states; `reviewer` defaults to `human`.
  `reopen` returns the row to `pending` after an approve / reject.
- `papers.metadata.contributor_add` — `{ intake_id, role, name,
  family?, given?, orcid?, library? }`. Appends a curator-authored
  contributor row after every extracted one.
- `papers.metadata.contributor_remove` — `{ contributor_id,
  library? }`. Removes a contributor row by id; returns
  `{ removed: bool }`.
- `remove` — `{ intake_id?, sha?, dry_run?, yes? }`. Exactly one of
  `intake_id` or `sha` must be set.
- `dryrun` — `{ path, out?, stdout?, no_chunk? }`. Writes the JSONL
  plus a summary sidecar under `<data_root>/dryruns/`.

Every write command takes the runtime-wide write mutex on entry; a
second concurrent write returns `-32001 busy`.

## Methods (library read proxies)

Operator-facing read pathway: each method below mirrors the MCP
`library.*` read tool of the same name. The JSON params shape, the
returned body, and the underlying `bookrack_ops::reads::*` call are
identical, so `bookrack exec <method> '<json>'` over the control
socket reaches the same code path agents exercise over MCP HTTP. None
of these methods take the write mutex; they read straight from the
catalog and corpus handles the daemon already holds.

- `library.stats` — aggregate counts over the library.
- `library.list_books` / `library.find_books` — paginated registry
  browse and filter.
- `library.show_book` / `library.show_toc` — per-book bibliographic
  record and TOC; `null` when the intake id is unknown.
- `library.read_context` / `library.read_span` — passage windows by
  anchor leaf or organizing node.
- `library.show_metadata_audit` / `library.show_metadata_report` —
  stored audit verdict and the recomputed per-field plausibility
  report.
- `library.list_metadata` / `library.list_pending_reviews` —
  paginated review-queue browse.
- `library.show_audit_trail` / `library.show_pipeline_trail` —
  per-book metadata-edit and pipeline-step audit trails.
- `library.search` / `library.search_in_book` — cited passage search
  across the library or a single book.
- `library.vectors_status` — vector-store snapshot for the library.

## Events (Phase 2)

- `daemon.state` — `idle` / `writing` / `degraded` / `stopping`. The
  flag flips to `writing` around every write command.
- `queue.tick` — `{ current, pending, running, last_finished? }`
  published immediately after every persisted change to
  `.bookrack-queue.json`, so a subscriber's view always coincides
  with what a crash recovery would replay.
- `worker.progress` — `{ job_id, stage, stage_progress?, message? }`
  with `stage` in `extract` / `ingest` / `embed`. Phase 2 emits at
  the runner's two visible boundaries (`extract` on pull,
  `embed` on success); finer-grained progress is deferred.
- `library.changed` — `{ library }` published after every successful
  write command finishes.
- `mcp.availability` — `{ paused }` published `true` at the start of
  every write command and `false` after it returns, so subscribers
  can advertise the MCP write surface as temporarily paused even
  though the runtime currently does not expose any MCP write tools.

## Phase log

- **Phase 1** — minimal read-only methods plus `daemon.shutdown`,
  `daemon.state` events, and the snapshot bundle. The MCP tool set,
  the CLI command surface, the REPL behaviour, the
  `.bookrack-queue.json` schema, and the session lock path are
  unchanged; the session lock gains a non-breaking
  `control_sock=<path>` line.
- **Phase 2** — write methods + queue / worker event flow +
  `bookrack exec call` over the control channel. New methods:
  `ingest.submit` / `ingest.cancel`, `metadata.{set,clear,ack,approve,reject}`,
  `vectors.{rebuild,reembed,reset,drop}`, `corpus.rebuild`,
  `stamps.reconcile`, `remove`, `dryrun`. New events: `queue.tick`,
  `worker.progress`, `library.changed`, `mcp.availability`. New error
  codes: `-32010 invalid_library`, `-32011 job_not_found`. The MCP
  tool set is still read-only and unchanged; the REPL still runs
  in-process; the on-disk queue document keeps its v1 schema.
- **Phase 3** — REPL extracted into the standalone `bookrack repl`
  client process. New crate `bookrack-repl-grammar` hosts the shared
  `clap` derive types (`ReplCli`, `ReplCommand`, `IntakeAction`,
  `WriteMetadataAction`, `WriteVectorsAction`, `CorpusAction`,
  `StampsAction`), so the daemon-side runner and the client-side
  parser stay in lockstep without a runtime dependency. New crate
  `bookrack-control-client` hosts `discover` / `connect` and the
  multiplexed JSON-RPC client (`ControlClient::call` /
  `ControlClient::subscribe` / `ControlClient::shutdown`) reused by
  the repl client and (Phase 4) the one-shot subcommand clients.
  `bookrack run` defaults to the silent daemon: it no longer reads
  stdin, no longer spawns reedline, and emits no banner. The
  hidden one-release transition flag `--legacy-repl` re-enables the
  in-process reedline REPL for CI scripts that fed it via stdin;
  it will be removed in the next release after this one. The
  `bookrack repl` client reads `<runtime_dir>/bookrack.tty.lock`'s
  `control_sock` line, opens the socket, subscribes to the event
  stream for a live `[<state-indicator>bookrack:<library>/queue:<n>] >`
  status line, and dispatches every command as the matching RPC.
  When stdin is not a TTY the client runs a batch loop: each line is
  parsed via the same grammar and dispatched in sequence, and the
  process exits non-zero on the first RPC failure. The MCP tool set,
  the session-lock schema, the on-disk queue schema, and the
  one-shot CLI dispatch path are unchanged. The reedline history
  file path (`<runtime_dir>/.bookrack-history`) is preserved across
  the client move.
- **Phase 4** — one-shot CLI subcommands rewired as control-plane
  clients. New top-level subcommands `bookrack ingest`,
  `bookrack metadata {set,clear,ack,approve,reject}`,
  `bookrack vectors {rebuild,reembed,reset,drop}`,
  `bookrack corpus rebuild`, `bookrack stamps reconcile`,
  `bookrack remove`, `bookrack dryrun`, and `bookrack quit`, each
  dispatched as the matching RPC over the control plane. The
  existing `bookrack verify`, `bookrack libraries`, and
  `bookrack diagnose` subcommands move to the same path; the
  daemon now answers them via the new `verify.run`,
  `library.fork`, and `diagnose.run` methods. Two reflection
  endpoints land alongside: `daemon.methods` returns the
  registry of every control-plane method (used by
  `bookrack exec tools` to enumerate the live surface), and
  `daemon.mcp_tools` returns the MCP tool list as published by
  the live `BookrackServer`. `bookrack exec` no longer holds an
  rmcp client; its `info` / `tools` / `logs` subcommands route
  through the control plane only, the `BOOKRACK_EXEC_CHANNEL`
  selector is gone, and the `rmcp` / `reqwest` crates are
  dropped from `bookrack-cli`'s dependency manifest. A follow-on
  to this phase reopens an operator read pathway: every MCP
  `library.*` read tool gains a control-plane proxy of the same
  name (see *Methods (library read proxies)* above), and
  `bookrack exec` accepts any control-plane method name
  containing a `.` as a sub-command, forwarding the JSON params
  through the control socket — so `bookrack exec
  library.show_book '{"intake_id":N}'` reaches the same code
  path as the MCP tool of the same name. A new `log`
  event channel forwards every tracing event captured by
  `bookrack_obs::stream::LogStreamHandle` through the control-plane
  broadcast so `events.subscribe` consumers can multiplex log
  output instead of opening a separate MCP SSE channel. The
  `bookrack doctor` subcommand keeps its local fallback: when no
  daemon is running it gathers checks directly via
  `bookrack_runtime::doctor::run`; when a daemon is running it
  calls `doctor.gather` and renders the same report.
  Daemon-not-running exits with code 2 from every one-shot
  client, matching the REPL client's contract; `bookrack doctor`
  and `bookrack quit` are the documented exceptions. The MCP
  tool set, the session-lock schema, the on-disk queue schema,
  and the REPL client are unchanged.
- **Phase 5** — second-launch semantics and `bookrack-mcp`
  control-plane parity. `LockInfo` and `peek_lock` move from the
  `bookrack-cli` `exec` module into `bookrack-session`; the new
  `bookrack_runtime::control::probe` resolves a recorded session
  into one of `Healthy(pid, control_sock)` / `Stale` /
  `Unprobeable` inside a hard 2 s budget by attempting
  `daemon.version` against the recorded socket. A second
  `bookrack run` (or `bookrack-mcp`) invocation against a healthy
  lock prints `bookrack daemon already running: pid=… control_sock=…`
  and exits zero; a lock pointing at a dead daemon exits with
  status 3 so the operator removes the lock by hand; an
  unprobeable lock (no `control_sock=` recorded) falls back to
  surfacing the original acquire error. New `RuntimeOpts.launch_mode`
  (`LaunchMode::Cli` / `LaunchMode::Gui`) routes a future GUI
  entry through the new `tray.focus` control-plane method instead
  of competing for the lock; with no GUI attached the method is a
  no-op and still returns `{ ok: true }` so the contract stays
  stable between CLI-only and GUI builds. `bookrack-mcp` gains a
  `--with-queue-worker` flag: without it, queue-bound write
  methods (`ingest.submit`, `ingest.cancel`, `vectors.*`,
  `corpus.rebuild`, `stamps.reconcile`, `remove`, `dryrun`,
  `papers.corpus_rebuild`, `papers.vectors_*`, `papers.stamps_reconcile`,
  `papers.dryrun`, `papers.remove`)
  short-circuit at dispatch with JSON-RPC error
  `-32002 queue worker disabled in headless mode` rather than
  enqueueing work no one will run; with it, the headless entry
  exposes the same full method set as `bookrack run`. The MCP
  tool set, the session-lock schema (still
  `pid=… / mcp=… / control_sock=…`, with unknown keys ignored
  per Phase 1's append-only rule), and the on-disk queue schema
  are unchanged.
- **PR-1** — queue lifecycle. `queue.pause`, `queue.resume`, and
  `queue.clear` are added to the control plane; the standalone
  REPL gains `queue pause`, `queue resume`, and `queue clear`
  routed through the same dispatch, and the shared grammar
  exposes a matching `ReplCommand::Queue { action }` variant. The
  worker loop honours an `Arc<AtomicBool>` pause flag that is
  mirrored onto `QueueState::paused`, so the on-disk snapshot and
  the in-memory behaviour agree across a process restart; a
  `pause` blocks new pickups but lets the running job finish on
  its own. Every mutation emits one `Event::QueueTick`; the
  payload schema matches the worker-side tick, so subscribers
  cannot tell handler-emitted ticks apart from worker-emitted
  ones. The MCP tool set, the on-disk queue schema, and the
  session-lock schema are unchanged.
- **PR-2** — TypeScript binding generation. The `Event` enum and
  every control-plane Params / Response struct derive `ts_rs::TS`
  under `#[cfg(test)]`; `cargo test --workspace` writes one `.ts`
  file per type into `crates/app/web/src/generated/`, where the
  future webview imports them. Routing is set by
  `.cargo/config.toml`'s `TS_RS_EXPORT_DIR` entry; the runtime
  crate's release build does not link `ts-rs`, and the wire
  schema, serde derives, and method registry are unchanged.
- **PR-3** — `WizardDriver` trait.
  `bookrack_runtime::wizard::{Wizard, WizardDriver, WizardOpts}`
  carries the first-run flow; `CliWizardDriver` is the only
  implementation today. Five steps in a fixed order: data root,
  PDFium file check, Ollama probe, smoke ingest+search, finalize.
  The trait's `Result` is the only abort path — the runner does
  not auto-retry. Finalize is three writes: skeleton directories,
  `<data_root>/config.toml`, and a merge into the platform-default
  registry. A GUI driver drives the same probes and the same
  writes through the same trait. `bookrack init` keeps its exact
  flag set and terminal transcript; `crates/cli/src/init.rs` is a
  thin shim that pairs `CliWizardDriver` with the runner.
- **GUI shell (first slice)** — `crates/app` ships a Tauri 2 shell
  (`bookrack-app`) that hosts the daemon in-process: it builds a
  `DaemonRuntime` with `LaunchMode::Gui`, so the control socket,
  the MCP listener, and the queue worker run inside the GUI
  process and terminal `bookrack` subcommands attach as usual.
  The window is a logo panel; closing it hides it, the tray menu
  (open / quit) stays resident, and tray quit routes
  `daemon.shutdown` through `control::methods::dispatch` so
  shutdown semantics match socket clients. A second GUI launch is
  caught by the single-instance plugin in-process, or — when the
  lock is held by a CLI daemon — by probe + `tray.focus` RPC
  followed by exit 0. No webview RPC surface exists yet; no
  control-plane methods were added or changed.
