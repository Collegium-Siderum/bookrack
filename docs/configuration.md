# Configuring bookrack

How bookrack decides which library to serve, where its data root is,
and what knobs govern embedding, search, and the metadata audit. For
running a library day to day see [operating.md](operating.md); the
authoritative, commented list of every environment variable is
[`.env.example`](../.env.example).

## Data-root resolution order

bookrack chooses its data root by precedence, highest first:

1. `--data-dir <path>` flag
2. `--library <name>` flag (looked up in the registry named by
   `BOOKRACK_REGISTRY`)
3. `BOOKRACK_DATA_DIR` environment variable
4. A `bookrack-data/` directory next to the running binary (the
   portable layout)
5. The `default` entry of the registry named by `BOOKRACK_REGISTRY`
6. The `default` entry of the platform-default registry at
   `<config_dir>/bookrack/registry.toml` — **only when
   `BOOKRACK_REGISTRY` is unset or blank**; a variable that names a
   registry makes that registry the only one consulted, and this rung
   is not reached. `<config_dir>` is:

   | Platform | `<config_dir>` |
   | --- | --- |
   | macOS | `~/Library/Application Support` |
   | Linux | `$XDG_CONFIG_HOME`, or `~/.config` if unset |
   | Windows | `%APPDATA%` (the Roaming AppData directory) |

`bookrack init` writes step 6's registry file by default. When a
path-class source (1, 3, or 4) wins while a registry `default` is also
set, `bookrack info` and `bookrack doctor` report the eclipse so the
shadowed default is visible rather than silently ignored.

A registry that cannot be read is fatal only to a resolution that
needed it. A root fixed by `--data-dir`, `BOOKRACK_DATA_DIR`, or the
portable layout never consults the registry, so an unreadable or
malformed one does not veto it: the resolution succeeds and the
annotations that would have come from the registry — the shadowed
default, the library name claimed for a path-class root — are simply
absent. A selection that does need the registry (`--library`, or
falling through to a `default`) still fails, and it fails naming the
registry rather than reporting that no library is configured.

## The library registry

The registry maps short names to data roots and records the machine's
`default`. Its entries are metadata-bearing tables — `data_dir`,
`kind`, `description`, `index_profile`, `uuid`, `created_at` — and the
legacy bare-path form (`name = "/path"`) stays permanently readable; a
write rewrites the file into the table form atomically. Every data root
also carries a self-describing `bookrack-library.toml` manifest naming
its stable identity and the index profile it runs under, so the registry
is a regenerable cache over the manifests rather than the sole record of
either. Editing an entry's `index_profile` by hand therefore accomplishes
nothing durable: the manifest outranks it, and `doctor` reports the
difference as drift.

Every registry verb resolves locally with no running daemon, so it
works during a fresh install or a recovery:

```
bookrack libraries list                       # entries, marking the default
bookrack libraries info [--name <name>]        # per-library status card
bookrack libraries default <name>              # persist the default pointer
bookrack libraries add <name> <path>           # register a root under a name
bookrack libraries register <path>             # name taken from the manifest
bookrack libraries remove <name> [--purge]     # forget an entry (data kept)
bookrack libraries detect <path>               # is this path a data root?
bookrack libraries scan <parent> | --volumes   # find data roots to register
bookrack libraries config <name> [KEY=VALUE]   # read or edit config.toml
bookrack libraries fork <name> --data-dir <p>  # clone into a sibling library
```

`add` and `register` write an identity manifest to a root that lacks
one (previewed and confirmed first, unless `--yes`); `--new-uuid`
re-mints the identity so a copied root registers as a distinct library.
`remove` never deletes data unless `--purge` is given, which is gated
on a detect verdict and a typed confirmation. `scan --register` brings
every confirmed root it finds into the registry — turning
`scan --volumes --register` into a one-command rebuild after a
reinstall.

## Per-library settings: `config.toml`

`libraries config` edits this file for one library. To see what all
the layers together produce — this file, the environment, `.env`, the
manifest, the registry, and the built-in defaults — use
`bookrack config effective`, which reports the value each knob
resolves to and the layer it came from.

Operational knobs resolve `environment variable > <data_root>/config.toml
> hardcoded default`. That chain covers this machine's operational
preferences only; a library's embed model is not one of them — it is the
index profile's fact, and nothing overrides it. The file accepts these
keys:

```toml
ollama_url = "http://localhost:11434"

[search]
top_k          = 5      # passages a query returns
weak_threshold = 0.5    # cosine distance at or above which a hit is
                        # weak; `retrieval show` flags a call whose every
                        # recorded hit sits there

[reranker]
url     = "http://localhost:8080"  # probe an operator-run server instead
                                   # of supervising one
ctx     = 8192          # -c for the supervised server; 8192 is also the
                        # default, sized to the rerank working set
threads = 4             # --threads; unset leaves the server's own choice
```

Every field is optional. Edit the file by hand, or through the offline
`bookrack libraries config <name> KEY=VALUE ...` verb (with `--unset
KEY` to clear one); nested keys are spelled `search.top_k`,
`reranker.ctx`, and so on. An edit does not reach a running daemon until
it restarts.

`index_profile` is accepted by the same verb, but written to the
library's manifest rather than to this file, because it is a property of
the library rather than of this machine. See [Retrieval
profiles](#retrieval-profiles-index-profile).

Four keys are **retired**. Two of them belong to the library rather than
to this machine: `embed_model` is declared by the library's index
profile, and `index_profile` as a *file* field is superseded by the
manifest. The other two belong to the process rather than to a library:
`mcp_addr` and `log_directive` are resolved before any data root is
known — the log subscriber is installed and the listen address is chosen
by the binary that then goes looking for a library, so a value sitting
inside a library could never have been read. Their homes are
`BOOKRACK_MCP_ADDR` (or `bookrack run --mcp-addr`) and `BOOKRACK_LOG` /
`BOOKRACK_LOG_CONSOLE`; both variables are alive and unchanged.

A file still carrying any of the four is refused by name — every command
that resolves a data root fails until the line goes, rather than the
field being silently ignored — and `libraries config <name> --unset
<key>` deletes it. Printing the file with `libraries config <name>`
still works and annotates each retired line. See [Declaring the embed
model through an index
profile](UPGRADE.md#declaring-the-embed-model-through-an-index-profile)
and [Retiring the process-level keys from
`config.toml`](UPGRADE.md#retiring-the-process-level-keys-from-configtoml).

## Environment knobs

`bookrack config effective` reports what every knob below actually
resolves to on this machine, which layer supplied it, and — for the
knobs nothing is set for — every layer each one *can* be set at. It
needs no daemon and works when the data root does not resolve, so it
is also the surface for diagnosing a root that will not open.

`bookrack config knobs` lists every knob this build has, without
consulting the machine at all: each one's compiled-in default, the
variable that moves it, every other layer it can be set at, its reach,
and when it is read. Use it to find out what can be configured;
`config effective` to find out what is.

Every environment variable bookrack reads is documented, with its
default, in [`.env.example`](../.env.example): the data-root and
registry selectors, the Ollama endpoint, the embed-batch and search
knobs, the PDFium library directory, the log filters, and the
per-query ANN overrides. Copy that file to `.env` and fill in what you
need. That file and `config knobs` cannot drift apart: a test compares
the two in both directions, so a knob with no stanza and a stanza with
no knob are both build failures.

### Process-level knobs with no `config.toml` key

These are the rows `config effective` marks `per_call`: read afresh on
every operation rather than snapshotted, so two calls in one process
can legitimately differ.

`BOOKRACK_VECTORS_BYPASS_ANN`, `BOOKRACK_VECTORS_NPROBES`, and
`BOOKRACK_VECTORS_REFINE_FACTOR` are read from the environment and have
no `config.toml` counterpart, on purpose. They are per-query overrides
for tuning retrieval breadth on a daemon that cannot take a per-call
flag on its request surface — a debugging escape hatch, not a durable
property of a library. Giving them a key would give a temporary knob a
permanent home, and `libraries config set` would then warn that a
library-level value is "overridden by the environment" for a value the
library never had.

Leaving the latter two unset is not the same as switching them off.
`nprobes` and `refine_factor` fall through to the ANN settings the
build stamped beside the vector store (`vectors_meta.json`), which is
why `config knobs` reports no default for them and names that file as
the rung under the variable. The values in it come from the index
profile the library ran under when the index was built, so a profile
edited afterwards does not move them until `index-profile apply`
rebuilds; `index-profile current` shows both sides. With no ANN index
present nothing decides them, and the search is exhaustive anyway.

The same reasoning retired `mcp_addr` and `log_directive` from
`config.toml`: a knob read before any data root is known does not
belong to a data root.

### The MCP address is taken before the daemon reports success

`BOOKRACK_MCP_ADDR` (or `bookrack run --mcp-addr`) is bound during
bring-up, alongside the session lock and the control socket. An address
another process holds refuses the start in one sentence at exit 2,
before any success line is printed — the daemon never comes up serving
nothing, and no health surface reports an address it does not own.
`--no-mcp` starts a session with no MCP surface at all; the control
plane still works.

Port `0` asks the operating system for any free port. The daemon then
reports what it was given: `bookrack run` prints it, `bookrack status`
shows it under `daemon.mcp`, and the session lock records it. The port
differs on every start, so an agent client configured with a fixed
`http://…/mcp` URL wants a fixed port; `:0` is for hosts where a
collision matters more than a stable URL.

### When `.env` is read

`.env` is loaded by the binaries — `bookrack`, `bookrack-mcp`, and the
desktop shell — as the first thing each of them does, before anything
reads a variable. The search runs upward from the working directory,
so which file is found depends on where the command was started.

Two consequences follow. A process that reads a variable does so after
the file has been applied, whichever variable it is: there is no
ordering in which one part of a command sees the file and another part
does not. And embedding a bookrack crate as a *library* gets no `.env`
at all — loading a file out of the caller's working directory is a
binary's decision, not a library's, so an embedder configures itself
through the real environment.

Which of the two set a variable is not something a reader of the
environment can tell, so the loader records it and `config effective`
reports it: a value the file supplied is shown at the `dotenv` layer,
sited at the file, and a line the real environment got to first is
shown as a layer that offered a value and lost. It lost at load time,
so it stays lost — a variable that is set but blank, or set to text the
knob cannot parse, offers nothing itself and still does not hand the
file's line its chance back. The value in that case comes from
`config.toml` or the built-in default, and the table shows the file's
line beneath it. This holds for the data root as well, on both halves:
a `BOOKRACK_DATA_DIR` written into `.env` wins rung 3 of the ladder
above and is credited to the file, and one the real environment already
carried leaves the file's line reported as a layer that lost. The
second holds whichever rung the root came from — including a
`--data-dir` flag that stopped the walk above rung 3, which decides
where the root comes from and not what the file says. What the file
*supplied* is the one thing a losing rung cannot report: the loader
records which keys it filled in, not the values, so a root the file
supplied and a higher rung outranked is absent from the row.

### How far `.env` reaches

The file is applied to the real process environment, not to a private
table of bookrack's own knobs, so its reach is every variable name.
`.env.example` lists only `BOOKRACK_*` variables, which makes "this
file configures bookrack" a natural reading and an incomplete one: a
`HTTP_PROXY` line routes every embedding request and every installer
download, an `XDG_*` line moves the managed directories the registry
and the downloaded reranker live in, and a `HOME` line changes the
prefix `bookrack diagnose` redacts against.

Nothing above is a knob, so no row in the table can report it. The
report says so separately: `config effective` ends with the variables
the file named outside the `BOOKRACK_` prefix, each marked either as
set in this process or as read and discarded because the environment
already carried one. Names only — a foreign variable is as likely to
carry a credential as it is to be `NO_COLOR`, and a report that prints
one is a report an operator cannot paste into an issue. `--json`
carries the same list as `dotenv_foreign`, alongside `dotenv_path`.

`BOOKRACK_NO_DOTENV` turns the load off. It has to come from the real
environment, since a value written inside `.env` is only read if the
file is loaded. `scripts/test-clean.sh` is why it exists: that script
starts the test suite from an empty environment, and without the
suppression cargo's package-root working directory would let dotenv
refill it from the repository's own file.

### Running the test suite against nothing

```sh
./scripts/test-clean.sh                 # the whole workspace
./scripts/test-clean.sh -p bookrack-cli # or a narrowed run
```

`cargo nextest run --workspace` inherits the machine it runs on: its
home directory, its registry, whatever `BOOKRACK_*` variables the shell
exports, and the `.env` above the package root. That is the right loop
for development. `scripts/test-clean.sh` is the contract underneath it:
it starts from an empty environment and lets through only what cargo
itself needs, plus `CI` and the two PDFium variables — those, because
dropping them would turn the PDF tests from a loud failure into a
silent skip, which is the outcome the script exists to prevent.

Both are worth running. A difference between them is a test reading the
machine rather than its fixtures. CI runs the scrubbed form as its own
job.

## Values compiled in: `config fixed`

Not every number that shapes bookrack's behaviour is a knob. A page
cap, a retry count, a timeout on an internal call: an operator cannot
change one without a rebuild, and until now could not read one without
opening the source either. `bookrack config fixed` prints them.

```sh
bookrack config fixed                  # the whole table
bookrack config fixed | grep timeout   # or the part that explains a failure
bookrack config fixed --json
```

Each row names the value, what it bounds, and the surface whose
behaviour changes with it — so a response that stopped short or a call
that died on a deadline can be checked against the number that decided
it. Like `config knobs`, the command reads no data root, no daemon and
no `.env`: the table describes the binary, and the same build prints
the same table everywhere.

The three configuration surfaces divide as:

| Question | Command |
|---|---|
| What resolves on this machine, and from where? | `config effective` |
| What can be set, and at which layer? | `config knobs` |
| What is decided at build time? | `config fixed` |

Being listed is not being settable, and the split is deliberate. Many
of these values are ones an operator should not move even given the
choice: raising the read-character cap overruns the context of the
agent the passage is for. Discoverable and adjustable are separate
properties, and only the first is claimed here.

The inventory is checked by a gate rather than kept in step by hand,
and the gate covers the whole workspace: every numeric constant in
every crate carries a marker naming either the key it is registered
under or the reason it is not a setting — a version stamp, a data-format
invariant, a heuristic that would need recalibrating — and the markers
and the registrations are compared in both directions. A registered
value is rendered from the constant itself, so a row cannot report a
number the code no longer holds, and one key can be claimed by only one
crate, which turns the same value given two homes into a build failure
instead of two rows that happen to agree.

A value that is already a knob's compiled-in default is not listed
twice: it stays in `config knobs`, where the rest of its chain is, and
its constant carries a marker saying so.

## Retrieval profiles: `index-profile`

An index profile couples the three retrieval knobs — the embedding
model, the ANN index shape, and the reranker stage — into one named,
statically-checkable atom. Two presets ship compiled into the binary:
`qwen3-0.6b-default` (a product-quantized IVF index, no reranker) and
`qwen3-4b-quality` (an HNSW index with a cross-encoder reranker stage).
A user profile at `<config_dir>/bookrack/index-profiles/<name>.toml`
shadows a built-in of the same name.

A library declares the profile it runs under in its manifest
(`bookrack-library.toml`), so the declaration travels with the data; its
registry entry caches the same name, and `libraries scan` refreshes the
cache from the manifests. Declare one offline with:

```
bookrack libraries config <name> index_profile=<profile>
```

Five read-only verbs resolve locally with no daemon. `current` needs a
library and takes it from the ordinary selection — `--data-dir`,
`--library`, `BOOKRACK_DATA_DIR`, then the registry default — so it
reports on the same root every other command would use:

```
bookrack index-profile list                 # built-ins + user profiles
bookrack index-profile show <name>          # source and validation result
bookrack index-profile validate <name>      # static checks; non-zero on error
bookrack index-profile current              # what a library runs under, vs its stamps
bookrack index-profile diff <a> <b>         # two profiles, field by field
```

A profile's `[ann]` and `[reranker]` fields are not knobs and have no
priority chain of their own: they are settings a build bakes into an
index, and `current` is where they are read back. The only ones a
running system can still move are `nprobes` and `refine_factor`, through
the two per-query variables above.

`validate` enforces the product-quantization constraints, checks the
cross-encoder reranker contract, and consults an offline model registry
that `--allow-unknown-model` bypasses. `bookrack doctor` additionally
compares each library's referenced profile against its built index
stamps and warns on a mismatch that would keep the daemon from starting.

The sixth verb changes things rather than reporting them:

```
bookrack index-profile apply <profile> [--library <name>] [--dry-run]
```

`apply` reconciles a library *to* a profile — re-embedding, rebuilding
the ANN index, reconciling stamps — so it derives an action plan and needs
a daemon already serving that library. It prints the plan and asks before
running it; `--dry-run` prints and exits, offline. It selects its library
the same way `current` does, `--data-dir` included. It is the preferred
front door for switching the embedding model or the ANN shape; use
`libraries config` above when you only mean to declare a profile the
library already matches.

A data root the registry does not carry is a valid target for both
verbs: the manifest owns the profile reference, so `current` reports it
and `apply` declares it. There is simply no registry entry to refresh,
and `apply` says so instead of minting one — `libraries add` registers
a root.

## The metadata audit profile

The metadata audit, the filename parser, the EPUB / TXT half-rules, and
the extract-side HTML / quality / language gates all read their toggles
and thresholds from an audit profile. Three built-in presets ship with
the binary:

- `default` — every per-field and TOC-shape signal active. This is the
  active profile at ingest time.
- `trust-source` — every toggle off: the audit substep is skipped, the
  pipeline still seeds base attrs and a `pending` review row, but no
  signal weakens or strengthens a field. Useful for ingesting
  "whatever the source says" and deferring quality calls to a reviewer.
- `strict` — the `default` toggle set, reserved for future upgrades
  that promote selected signals to higher severities.

```
bookrack audit-profile list
bookrack audit-profile show trust-source
bookrack audit-profile diff default strict
```

The global `--audit-profile <name>` flag overrides the profile for a
single audit-aware command — `ingest`, `intake ocr`, `dryrun`,
`metadata reaudit` / `advance`, and `papers metadata reaudit`. Passing
it on any other subcommand aborts before an RPC is sent, so the value
cannot silently drop.

### Overlays under `audit-rules/`

The shipped `default` profile merges with an optional overlay at
`<data_root>/audit-rules/audit_profile.local.toml`, so a deployment can
adjust individual thresholds, the HTML block / skip tag lists, the PDF
text-quality cutoffs, or the BCP-47 script buckets without recompiling.
Two further on-disk schemas under the same directory follow the same
shipped-default-plus-overlay merge:

- `audit_data.toml` — the reputable-imprint whitelist, the watermark
  token and substring lists, the whitelist normalisation abbreviations,
  the placeholder-title words, and the book-extension lists the ingest
  dryrun walker and the diagnose scrubber consult. Shipped default:
  [crates/audit-profile/data/audit_data.toml](../crates/audit-profile/data/audit_data.toml).
- `headings.toml` — the multi-language chapter / volume marker grammars
  the TXT adapter dispatches across (Sino, Latin, German families
  today). Shipped default:
  [crates/audit-profile/data/headings.toml](../crates/audit-profile/data/headings.toml).

All overlays are user-supplied; bookrack falls through to the shipped
defaults when an overlay is absent or omits a field.
