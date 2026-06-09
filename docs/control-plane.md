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
  `library.list`, `daemon.version`.
- `events.snapshot` — explicit re-fetch of named channels;
  `params.channels` is the list to refresh.

## Events (Phase 1)

- `daemon.state` — emitted on transitions between `idle`, `writing`,
  `degraded`, `stopping`. Phase 1 emits only `idle` and `stopping`;
  Phase 2 will flip `writing` around write commands.

## Phase log

- **Phase 1** — minimal read-only methods plus `daemon.shutdown`,
  `daemon.state` events, and the snapshot bundle. The MCP tool set,
  the CLI command surface, the REPL behaviour, the
  `.bookrack-queue.json` schema, and the session lock path are
  unchanged; the session lock gains a non-breaking
  `control_sock=<path>` line.
