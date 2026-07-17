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
   `<config_dir>/bookrack/registry.toml`, where `<config_dir>` is:

   | Platform | `<config_dir>` |
   | --- | --- |
   | macOS | `~/Library/Application Support` |
   | Linux | `$XDG_CONFIG_HOME`, or `~/.config` if unset |
   | Windows | `%APPDATA%` (the Roaming AppData directory) |

`bookrack init` writes step 6's registry file by default. When a
path-class source (1, 3, or 4) wins while a registry `default` is also
set, `bookrack info` and `bookrack doctor` report the eclipse so the
shadowed default is visible rather than silently ignored.

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

Operational knobs resolve `environment variable > <data_root>/config.toml
> hardcoded default`. That chain covers this machine's operational
preferences only; a library's embed model is not one of them — it is the
index profile's fact, and nothing overrides it. The file accepts these
keys:

```toml
ollama_url    = "http://localhost:11434"
mcp_addr      = "127.0.0.1:8765"
log_directive = "info,lance=warn"

[search]
top_k          = 5      # passages a query returns
weak_threshold = 0.5    # cosine distance at or above which a hit is weak

[reranker]
url     = "http://localhost:8080"  # probe an operator-run server instead
                                   # of supervising one
ctx     = 8192          # -c for the supervised server
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

Two keys are **retired**: `embed_model` and `index_profile` as *file*
fields. The embed model is declared by the library's index profile, and
the profile reference lives in the manifest. A file still carrying
either is refused by name — every command fails until the line goes,
rather than the field being silently ignored — and `libraries config
<name> --unset <key>` deletes it. See [Declaring the embed model through
an index
profile](UPGRADE.md#declaring-the-embed-model-through-an-index-profile).

## Environment knobs

Every environment variable bookrack reads is documented, with its
default, in [`.env.example`](../.env.example): the data-root and
registry selectors, the Ollama endpoint, the embed-batch and search
knobs, the PDFium library directory, the log filters, and the
per-query ANN overrides. Copy that file to `.env` and fill in what you
need.

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

Five read-only verbs resolve locally with no daemon:

```
bookrack index-profile list                 # built-ins + user profiles
bookrack index-profile show <name>          # source and validation result
bookrack index-profile validate <name>      # static checks; non-zero on error
bookrack index-profile current              # what a library runs under, vs its stamps
bookrack index-profile diff <a> <b>         # two profiles, field by field
```

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
running it; `--dry-run` prints and exits, offline. It is the preferred
front door for switching the embedding model or the ANN shape; use
`libraries config` above when you only mean to declare a profile the
library already matches.

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
