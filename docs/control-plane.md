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
- `events.subscribe` — `{ subscribed: true }` followed by an
  immediate snapshot bundle of `daemon.state`, `queue.list`,
  `queue.tick`, `library.list`, `library.changed`,
  `mcp.availability`, `daemon.version`.
- `events.snapshot` — explicit re-fetch of named channels;
  `params.channels` is the list to refresh.

## Methods (Phase 2)

- `ingest.submit` — `{ paths, library?, priority?, force? }` →
  `{ job_ids: [<uuid v7>] }`. Appends jobs to the persistent queue
  document; the worker picks them up on the next 200 ms tick.
- `ingest.cancel` — `{ job_id }` → `{ ok: true }`. Marks the matching
  pending or running job as cancelled.
- `metadata.set` / `metadata.clear` / `metadata.ack` /
  `metadata.approve` / `metadata.reject` — same params as the
  `bookrack metadata` REPL subcommands; return `{ ok: true }` on
  success.
- `vectors.rebuild` / `vectors.reembed` / `vectors.reset` /
  `vectors.drop` — mirror the matching `bookrack vectors` actions.
  `vectors.drop` takes no params.
- `corpus.rebuild` — `{ include_vectors?, book?, stale_only?, dry_run?, yes? }`.
- `stamps.reconcile` — no params; rewrites the corpus index stamps.
- `remove` — `{ intake_id?, sha?, dry_run?, yes? }`. Exactly one of
  `intake_id` or `sha` must be set.
- `dryrun` — `{ path, out?, stdout?, no_chunk? }`. Writes the JSONL
  plus a summary sidecar under `<data_root>/dryruns/`.

Every write command takes the runtime-wide write mutex on entry; a
second concurrent write returns `-32001 busy`.

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
  dropped from `bookrack-cli`'s dependency manifest. A new `log`
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
