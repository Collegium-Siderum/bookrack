# Changelog

All notable changes to bookrack are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project
follows [semver](https://semver.org/spec/v2.0.0.html). Each release
section is the source of truth for the GitHub Release notes — the
release workflow extracts the matching section verbatim from this file.

## [Unreleased]

### Added

- `library.find_books` honours the `categories` filter end to end:
  `BookFilter` and `IntakeFilter` carry the list, the catalog's
  `find_intakes` SQL adds an EXISTS sub-query against
  `node_categories` keyed on the book scope, and the MCP / RPC
  layers drop the warning that the field was ignored. The match is
  "at least one tag in the list".

- `metadata.advance` control-plane method resumes CHUNK→EMBED for
  a book parked at the metadata gate by `--hold-for-metadata`. The
  REPL grammar's existing `Advance` action and the CLI's
  `metadata advance` subcommand now reach the control plane
  instead of bailing through the placeholder error. `metadata.approve`
  also triggers the resume implicitly when the book is parked,
  matching the operator expectation that "approve" unblocks the
  pipeline without a separate command.

- `ingest.submit` accepts `hold_for_metadata: true`, persisted onto
  the per-job `QueueJob` record and forwarded to the ingest
  pipeline's existing `IngestParams::hold_for_metadata` knob so the
  worker parks the book at STRUCTURE when the audit verdict is
  `needs_work`. `bookrack ingest --hold-for-metadata` and the REPL
  `ingest --hold-for-metadata` flag now reach this code path
  instead of being silently ignored.

- `ingest.submit` accepts `recursive: true`, which expands every
  directory in `paths` to its supported-format files via the
  existing `bookrack_runtime::queue::collect_supported_files`
  walker before enqueueing. `bookrack ingest --recursive` and the
  REPL `ingest --recursive` flag now reach this code path instead
  of bailing through the placeholder error.

- `library.set_default` control-plane method re-points the
  registry's default-library pointer at one of its registered
  libraries; the change is in-memory only and fires the existing
  `library.changed` event. The REPL `use <library>` built-in
  drives it instead of bailing through the phase-unavailable
  shim, parallel to the in-process REPL's `use` command.

- `logs.tail` control-plane method snapshots the most recent `n`
  events from the daemon's in-memory log ring buffer (default 100,
  capped at 1024). Peer of the existing `session.logs_tail` MCP
  tool; shares the same `bookrack_obs` ring buffer. The REPL
  `logs [n]` built-in now drives it instead of bailing through the
  `print_phase_unavailable` shim.

- Paper-side metadata audit lands as a peer of `bookrack-metadata`
  inside the `bookrack-glean` crate. `PaperAuditProfile`,
  `PaperAuditData`, `PaperFlag`, `PaperReport`, and `audit_paper`
  carry their own signal set (DOI / arXiv format checks, ISSN
  MOD-11 and ORCID MOD 11-2 checksums, CSL-type-driven required
  field matrix, abstract length floor, sentinel contributor
  detection, body-script vs declared-language mismatch); the
  shipped default and a `paper_audit_*.local.toml` overlay drive it
  the same way `bookrack-audit-profile` drives the books audit. The
  pipeline writes the verdict and confidence through
  `update_audit_rollup` and posts a `pending` row to `node_reviews`
  with `reviewed_by = bookrack-glean:<profile>`.

- `bookrack_glean::reaudit::reaudit_paper` re-runs the audit
  offline from the cached extraction envelope and refreshes only
  the rollup; `build_report` exposes the same computation without
  write-back.

- Paper-side metadata curation surface: nine `papers.metadata.*`
  control-plane methods (`reaudit`, `set`, `clear`, `void`, `ack`,
  `approve`, `reject`, `reopen`, `contributor_add`,
  `contributor_remove`) and the matching `bookrack papers
  metadata <action>` CLI / REPL commands. Override, void, and
  review-state writes route through the existing ItemKind-aware
  catalog APIs; an audit trail row lands in a follow-up.

- Queue document schema bumped to v3: `QueueJob` carries a new
  `hold_for_metadata` field. The field is `#[serde(default)]` so
  any v2 document on disk continues to load with the flag unset.

### Security

- Destructive control-plane RPCs that expose a `yes` parameter —
  `corpus.rebuild`, `papers.corpus_rebuild`, `vectors.reembed`,
  `vectors.reset`, `papers.vectors_reembed`,
  `papers.vectors_reset` — now reject requests with `yes = false`
  before any work runs, returning the new
  `-32012 confirmation required` error. The control plane never
  prompts on the caller's behalf; previously the in-process `ask`
  callback was hard-coded to approve, so a client that issued the
  request with the flag unset would silently destroy the chunks
  table or rebuild the corpus tree. `dry_run` (rebuild / reembed)
  and `resume` (reset) paths remain exempt because they do not
  destroy data on this call. The cmd-layer `ask` closure handed in
  from the daemon also flipped to a denial so a future change to
  the short-circuit logic cannot reintroduce the silent path.

### Changed

- Write-class control-plane RPCs (`metadata.*`, `corpus.rebuild`,
  `vectors.{rebuild,reembed,reset,drop}`, `remove`, `dryrun`,
  `stamps.reconcile`, and their `papers.*` counterparts) classify
  downstream errors before responding. Unknown intake ids, unknown
  metadata fields and contributor roles, contributor / node lookup
  misses, and ingest / glean validation refusals (`NeedsOcr`,
  `EmptyExtraction`, `MissingEnvelope`, `EnvelopeMismatch`,
  `IntakeNotEmbedded`, `OcrPagesMissing`, ...) now surface as
  `-32602 invalid params`; unknown `library` parameters surface as
  `-32010 invalid library`. Genuine handler-side faults still surface
  as `-32603 internal error`. MCP / CLI clients can now distinguish
  "fix the request and retry" from "report or escalate" by the error
  code instead of parsing the human-readable message.

- `bookrack_glean::glean_paper`'s no-op fast path now requires
  `extractor_version` parity in addition to the embed model match,
  matching `bookrack_ingest::ingest_book`. A stale stamp falls
  through to the live pipeline instead of returning a cached report.
  `GleanReport` carries `audit_verdict` and `audit_confidence`, both
  populated on the live path from the paper audit and read back
  from `node_publication_attrs` on the no-op path.

- Paper-side maintenance commands brought to parity with the books
  pipeline: `bookrack papers corpus rebuild`,
  `bookrack papers vectors {rebuild,reembed,reset,drop}`, and
  `bookrack papers stamps reconcile`. They expose the same
  rebuild-from-envelopes, reembed, reset, ANN-rebuild, and
  index-stamp reconciliation primitives the book pipeline carries,
  routed through the control plane as `papers.corpus_rebuild`,
  `papers.vectors_{rebuild,reembed,reset,drop}`, and
  `papers.stamps_reconcile`. The `bookrack-glean` crate gains
  `rebuild`, `reembed`, `reset`, and `stamps` modules implementing the
  abstract-leaf-shaped paper variants; the runtime gains matching
  `cmd::papers_{corpus,vectors,stamps}` shims and JSON-RPC handlers,
  registered in the dispatch table and the `is_queue_bound_method`
  set so headless `bookrack-mcp` short-circuits them without
  `--with-queue-worker`. `docs/UPGRADE.md` and the README's Upgrading
  section now document the paper-side bump-to-refresh row alongside
  the book-side commands.

- `bookrack papers dryrun` replaces the placeholder `dryrun_paper`
  stub with a full per-file simulation: EXTRACT runs against the
  real adapter, IDENTIFY runs the same DOI / arXiv / ISSN / venue /
  year / abstract pick that `glean_paper` drives, STRUCTURE is
  predicted statically from the colored block stream, and CHUNK is
  replayed without touching the embedder. The runner walks a path,
  writes a paper-shaped JSONL plus a summary sidecar to
  `<data_root>/dryruns/dryrun-paper-...`, and the per-file report
  surfaces IDENTIFY hit rates so an operator can triage a paper
  cluster before committing the embed run. Routed through the
  control plane as `papers.dryrun`.

## [0.5.0] - 2026-06-14

### Added

- Two-store library: a `papers` cluster (`papers_catalog.db`,
  `papers_corpus.db`, `lancedb_papers`, `papers_dir/`) sits beside the
  existing book cluster under the same data root, with parallel
  intake / corpus / vector / metadata schemas. The `config` crate
  exposes paper-side getters; the in-process `LibraryRegistry` owns
  both stores and routes work by item kind, so books and papers share
  one scheduler, one MCP surface, and one search call.

- `bookrack-glean`: paper-side pipeline crate covering
  REGISTER → IDENTIFY → EXTRACT → STRUCTURE → CHUNK → EMBED. Driven
  through the control plane by `glean.submit`, which the queue worker
  consumes as paper-kinded jobs. REGISTER archives the source PDF
  bytes next to the envelope and stamps `intake.source_pdf_path`;
  IDENTIFY runs an offline pass that lifts DOI, arXiv id, venue, and
  abstract from the cached extraction; EXTRACT reuses the shared PDF
  adapter; STRUCTURE runs the paper-only coloring + Section tree
  pass; CHUNK and EMBED reuse the book-side primitives against the
  papers vector store.

- `bookrack papers` subcommand tree (one-shot CLI and REPL): `papers
  ingest [--recursive]` walks `.pdf` directories and forwards each
  path to `glean.submit`; `papers source` streams the archived source
  PDF back from `papers_dir/`; `papers remove` cascades a paper out
  of the papers catalog, corpus, vector store, and `papers_dir/`;
  the `papers <read>` commands mirror the book-side read family
  against the paper backend.

- MCP surface for papers: `library.search` accepts a `kind` switch
  (`book` / `paper` / unset for both); `library.read_context` /
  `library.read_span` and the other library read tools accept a
  `kind` field on their arguments; the `papers.*` read family,
  `papers.fetch_source`, and `glean.submit` expose papers to agent
  clients.

- CSL-JSON item model with two-way conversion in `bookrack-catalog`.
  `Biblio` and `Contributor` gain paper-shaped fields (DOI,
  container-title, volume / issue / pages, publication date parts,
  …) and round-trip through the M8 paper columns without lossy
  detours.

- `KindedNodeId`: every cross-store addressing surface
  (`read_context`, `read_span`, search citations, queue jobs) now
  carries the item kind alongside the intake id, so a hit, a context
  read, or a queued job is unambiguous between the two stores.
  `library.search` citations report the store they came from.

- Extraction envelopes are written with an explicit kind prefix in
  their filename; `bookrack-extract` tolerates the un-prefixed legacy
  layout on read. `bookrack doctor --rename-envelopes [--dry-run]` is
  the one-shot migrator that rewrites existing envelopes into the new
  naming.

- `glean_paper` STRUCTURE now assembles a Section tree from the
  heading-colored block stream: a `BlockKind::Heading{1}` opens a
  Section organizer under the Work root, a `Heading{2}` opens a
  Subsection (auto-opening a Section first when none is outstanding),
  depth-3+ Heading blocks stay as Heading leaves under the deepest
  open organizer, Body blocks attach as Paragraph leaves, and
  `BlockKind::Caption` blocks land as `FigureCaption` structural
  leaves. The abstract leaf is bit-for-bit unchanged (Tier 1 vector
  anchor): same NodeId allocation order, same `intake:{id}:abstract`
  stable anchor, same text / norm hashes, and the page bounds stay
  `NULL`. Body-leaf stable anchors continue to count Body blocks only,
  so a re-glean of a Phase-1 envelope still produces the same body
  hashes. When the heading pass identifies no candidates the tree
  falls back to the flat Phase-1 shape. The STRUCTURE audit row's
  `metric_summary` JSON gains `sections`, `subsections`, and
  `headings` counters alongside the existing `body_leaves`.

- `bookrack_extract::pdf_paper::extract_paper_structured`: a paper-only
  coloring pass that promotes [`BlockKind::Body`] blocks to
  `BlockKind::Heading` / `BlockKind::Caption` using the PDF outline that
  the adapter already attached to the extraction's `toc` first, then a
  port of the `pdffigures2` SectionTitleExtractor rule set over
  `BlockStyle` (font-size median, bold-majority, above-gap ratio,
  numbered prefixes including the CJK `\u{7b2c}…\u{7ae0}` /
  `\u{7b2c}…\u{8282}` families). Cross-page running headers whose
  case-folded text recurs on three or more pages are dropped from the
  candidate pool before promotion. The selected signal is recorded on
  `Provenance::source_of_structure` as `outline` / `heuristic` /
  `mixed` / `none`. `glean_paper` runs the pass against PDF
  extractions inside its EXTRACT stage; book-side adapters are not
  affected. A new `BlockKind::Abstract` variant is reserved for the
  paper structuring pass — book-side adapters never emit it.

- `bookrack doctor --install-pdfium`: downloads the pinned PDFium
  build, verifies its SHA-256 against the pin, and unpacks the
  library into a per-user managed directory that the loader searches
  automatically. The first-run wizard offers the same download when
  the library is missing.

- `metadata.set` / `metadata.clear` accept a `reason` that lands on the
  audit row, closing the gap where field edits — unlike approve / ack /
  reject — could never record their justification. The CLI and REPL
  expose it as an optional `--reason` flag; the MCP write tools require
  it, so an LLM edit always carries its rationale in the audit trail.

- `show_book` (and the `metadata.show` report that embeds it) lists the
  book's active overrides — which fields are curated rather than
  extracted, by whom and when, with notes — alongside the merged
  `effective_biblio`, so curation state is visible without replaying
  the audit trail.

- `metadata.set` accepts a `confirmed` mark (CLI/REPL `--confirmed`,
  MCP optional `confirmed: true`) recording that the curator checked
  the value against the source itself, not an external catalog. The
  audit grades a confirmed override strong unless a validation check
  (ISBN checksum, BCP-47 syntax, year range, language/body mismatch)
  fails. Rewriting the field without the mark drops it.

- `metadata.void` (CLI/REPL `bookrack metadata void`, MCP
  `library.metadata.void`): suppress a field's extracted value with a
  NULL override when the value is known wrong and no correct value is
  at hand — the field reads as absent until one is set. `metadata.clear`
  removes the suppression. The MCP tool requires a `reason`.

- `metadata.reaudit` (CLI/REPL `bookrack metadata reaudit`, MCP
  `library.metadata.reaudit`): re-run the metadata plausibility audit
  from the book's cached extraction envelope — no re-extraction — so
  the stored verdict / confidence reflect the current effective
  metadata after field corrections instead of reporting the
  ingest-time outcome forever. Only the rollup pair and one
  pipeline-audit row are written; the review status is untouched.
  Returns the previous and new verdict / confidence.

- `library.show_metadata_report` (MCP): recompute the metadata
  plausibility audit from the book's cached extraction and return the
  full per-field report — origin (`extracted` / `override` /
  `override_confirmed` / `voided`), grade, flags, and hint per field,
  plus TOC shape flags and copyright-page block candidates — next to
  the stored rollup and review status, so a curator can see why a
  record reads `needs_work` / `low` without re-deriving the signals.
  Pure read, runs the default audit profile; `metadata.reaudit`
  remains the write path that refreshes the stored rollup.

- Contributor curation: `metadata.contributor_add` writes an
  `origin = "user"` attribution (closed role set: `author` /
  `translator` / `editor` / `other`) that survives re-ingest and is
  immediately matched by `find_books` `contributor_name`;
  `metadata.contributor_remove` deletes one row by the
  `contributor_id` now listed in `show_book`, whatever its origin —
  the path for stripping a wrong extracted attribution. CLI/REPL:
  `bookrack metadata contributor-add` / `contributor-remove`; MCP:
  `library.metadata.contributor_add` / `_remove` with a required
  `reason`.

### Changed

- M8 catalog migration: state tables shed their `book_` prefix in
  favour of item-shaped names (`item_review`, `item_pipeline_audit`,
  …) and gain a `kind` column where the table is shared between
  books and papers; scope strings in the catalog API are replaced by
  `core::ItemKind`. New paper-shaped columns land on `biblio`,
  `contributor`, and `intake` (`source_pdf_path`). The migration
  runs automatically at daemon preflight against an older library;
  the per-database backup it writes is clustered by database stem so
  the books-side and papers-side migrations cannot overwrite each
  other.

- All MCP library read tools now reach the catalog and corpus through
  the control plane, closing the last direct-handle path. The
  standalone `bookrack-mcp` process and the in-tree daemon share one
  in-process scheduler for both reads and writes — completing the
  control-plane split started in 0.4.0.

- The extraction envelope type is hoisted out of `bookrack-ingest`
  into `bookrack-extract`, so the book-side ingest pipeline and the
  paper-side glean pipeline consume one shared definition.

- `extract_paper_structured` is now precision-first: it returns an
  empty TOC rather than guess. The pass runs in two cooperating
  stages — outline-guided heading promotion and a strict numbered
  heuristic with sequence validation — both gated so a noise line
  cannot survive: a candidate must carry a numbered prefix
  (`N`, `N.M`, `N.M.P`, Roman, or `Appendix`), be single-line, fit
  under 80 characters, have no math / arrow / geometric-shape
  Unicode, no `@`, no boilerplate prefix (`we`, `the`, `vol.`,
  `https://`, …). The sequence check accepts only candidates that
  advance the ascending series at their level, and the outline pass
  matches anchored entries against block text by label prefix (with
  numeric / Roman stripping) instead of trusting the page anchor
  blindly. Outline anchors that resolve to a sub-body-sized font or
  a multi-line block are rejected. The heuristic absorbs outline-
  promoted blocks into its sequence state, so an outline that skips
  past a section still lets the heuristic recognise the next number.

- The PDF adapter now attaches a `BlockStyle` geometry summary to every
  paragraph it reconstructs — font-size median and 90th-percentile, a
  bold-majority flag aggregated from per-character font weights, line
  count, first-line left coordinate, and a vertical gap above the block
  normalized by the page's median line height. Book-side TXT / EPUB /
  OCR adapters leave the field absent; older envelopes deserialize
  with `style = None` and remain readable. `EXTRACTOR_VERSION` bumps
  5 → 6; PDF sources need re-ingest before the heading heuristics that
  consume the geometry can take effect (see `docs/UPGRADE.md`).

- The PDF text-layer quality gate now counts U+FFFD as
  *replacement-character sites* — a `(FFFD | ' ')+` span counts as one
  signal rather than once per glyph — so a TOC dot-leader fill backed
  by an unmapped font glyph stops dragging an otherwise-clean PDF over
  the 5% OCR threshold. The per-glyph share is still reported as
  `replacement_char_ratio` for recalibration. `EXTRACTOR_VERSION` bumps
  4 → 5; affected sources need re-ingest (see `docs/UPGRADE.md`).

- The PDFium library search is now a chain: `BOOKRACK_PDFIUM_LIB`
  (authoritative when set), then the executable's own directory,
  then the per-user managed directory. A miss reports every searched
  directory and the remedies instead of a raw dynamic-loader error.

- `metadata.set` validates the field name against the curator-editable
  bibliographic set and rejects unknown names, instead of silently
  creating an override row no consumer ever reads; the error carries
  the full editable list. Pipeline-owned bookkeeping columns
  (`source`, `confidence`, `audit_verdict`, `source_format`,
  `enriched_by`) are no longer overridable. `metadata.clear` accepts a
  name outside the set only when a stale override row with that key
  exists, so pre-validation rows stay removable.

- The metadata audit now reads per-field origins and directs its
  suspicion accordingly. A field whose effective value is a curator's
  override is exempt from the source-format prior, the doubtful
  text-layer downgrade, and the PDF/timestamp year heuristics — those
  model the extractor, not the curator — so a verified PDF record can
  finally reach `high` confidence. A confirmed override is graded
  `strong` outright; heuristic flags stay on the report for
  observability while objective validation (ISBN checksum, BCP-47
  syntax, year range, language/body mismatch, emptiness) still
  downgrades. A voided field reads as a deliberate gap (`medium`, a
  `voided` flag) instead of a missing extraction, except `title` /
  `language`, which keep gating the verdict. Stored rollups reflect
  the new grading after the next ingest or `metadata.reaudit`.

### Fixed

- `bookrack dryrun` over the control plane returns the per-book JSONL
  path, the summary sidecar path, and the aggregate summary in the
  RPC body, instead of swallowing the per-book report and the summary
  into the daemon's own stdout / stderr where the client process
  never saw them. `--stdout` now streams the JSONL artifact from the
  client side, summary lands on the client's stderr, and the artifact
  always persists under `<data_root>/dryruns/` (or `--out`) so a long
  walk that the operator missed live can still be inspected after the
  fact.

- `vectors reset` (including `--resume`) and `vectors reembed` append
  pipeline-audit rows for the chunk and embed stages they run, so a
  book's trail no longer ends on a stale failure after a maintenance
  pass re-embedded it successfully. Rows carry `actor_detail` `reset` /
  `reembed` and share one `reset-`/`reembed-`-prefixed run id per
  invocation.
- Metadata edits arriving over MCP are attributed to `actor_kind=llm`
  / `actor_detail=mcp` on their audit rows and write outcomes, as the
  tool descriptions promise. They previously inherited the caller the
  daemon was launched with (`human`/`cli` under `bookrack run`,
  `human`/`gui` under the tray app); headless `bookrack-mcp` had the
  reverse mislabel, recording control-socket edits as `llm`/`mcp`.
- Review status flips no longer clobber `node_reviews.notes`: the
  ingest audit's note (verdict and flagged-field list) survives
  `metadata.approve` / `metadata.ack` / `metadata.reject`. Approve and
  reject previously overwrote it with the review reason — which already
  lives on the audit row — and ack nulled it outright.
- The daemon now migrates every catalog (books and papers) at
  preflight before opening connections, so a newer build started
  against an older library no longer fails mid-write when the first
  stage reaches for a column the running schema does not yet have.

## [0.4.0] - 2026-06-11

### Added

- JSON-RPC control plane: the daemon serves a control socket, and
  every other process is a client of it. The method surface covers
  the write commands with progress events, queue lifecycle
  (`queue.pause` / `queue.resume` / `queue.clear`), `verify.run`,
  `library.fork`, `diagnose.run`, introspection (`daemon.methods`,
  `daemon.mcp_tools`), and a subscribable log/event channel.
- `bookrack repl`: standalone control-socket client process. Reedline
  runs in the client; every command is dispatched as a JSON-RPC call
  over the daemon's control plane. The prompt shows a live
  `[<state-indicator>bookrack:<library>/queue:<n>] >` status line
  driven by `events.subscribe`. When stdin is not a TTY the client
  runs a batch loop: each line is parsed via the shared
  `bookrack-repl-grammar` and dispatched in sequence, and the
  process exits non-zero on the first RPC failure. When the daemon
  is not running the client exits with code 2 and the message
  `bookrack daemon not running; start it with: bookrack run`.
- `library.read_context` and `library.read_span` MCP tools:
  structural passage reads around a hit, backed by a document-order
  `leaves_in_doc_span` corpus query, so an agent can widen a citation
  to its surroundings without a second search.
- Search citations carry the intake id and structural anchors of the
  cited span.
- `crates/app`: Tauri shell hosting the daemon in-process with a
  minimal Svelte 5 panel — groundwork for the tray GUI, not yet part
  of the release artifacts. Control-plane DTOs and events derive
  ts-rs schemas, generated as `.ts` files for the webview at test
  time.
- `WizardDriver` trait behind the first-run wizard, with the CLI
  driver as the first implementation and room for a GUI driver.
- The daemon raises its soft `RLIMIT_NOFILE` to the hard limit at
  startup, and `bookrack doctor` reports the effective limit. A
  GUI-launched daemon previously inherited a 256-fd default and
  starved mid-batch on LanceDB fragment files.

### Changed

- `bookrack run` defaults to the silent daemon: it no longer reads
  stdin, no longer spawns reedline, and emits no banner. Open an
  interactive REPL with `bookrack repl` in another process.
- One-shot subcommands are rewritten as control-plane clients and
  dispatch their work to the running daemon over the control socket.
- A second `bookrack run` against a live session brings the existing
  daemon to the front and exits cleanly; `bookrack-mcp` gains the
  same re-entry behaviour.
- The ingest queue pauses on process-level failures instead of
  rapidly failing every remaining job; the error is the process's,
  not the books'.
- Ingest error strings keep their flattened source chains, so root
  causes such as `Too many open files` reach the pipeline audit
  instead of an opaque `vector store error`.

### Deprecated

- `bookrack run --legacy-repl`: one-release transition flag (hidden
  from `--help`) that re-enables the in-process reedline REPL for CI
  scripts that fed it via stdin. The flag will be removed in the
  next release after this one; migrate to `bookrack repl` (which
  accepts stdin in batch mode for the same scripted use case).

### Fixed

- The RESET confirmation on destructive vector-store commands
  survives the control-plane client split.
- The daemon process exits after a control-plane shutdown request
  instead of lingering, and the `--legacy-repl` path prints its
  session banner again.
- The hint printed after `library.fork` names the correct
  model-switch command.

## [0.3.0] - 2026-06-08

### Added

- `bookrack run`: single-process daemon-REPL. Acquires the machine-wide
  session lock, serves MCP over streamable-HTTP, hosts a reedline REPL
  on the foreground tty, and drives a persistent ingest queue worker.
- `bookrack exec`: out-of-process discovery surface. Reads the session
  lock without opening any database; subcommands `info`, `tools`,
  `library.<tool>`, `logs {tail,follow}` reach the live daemon over
  MCP and SSE.
- `session.*` MCP tool family: `session.info`, `session.queue_status`,
  `session.logs_tail`, `session.shutdown`. Lets agent clients query
  daemon state, follow logs, and request a graceful stop without
  touching the host process directly.
- `/session/logs` SSE endpoint on the MCP axum router for live log
  streaming with a 15-second keep-alive.
- Service-architecture log pipeline: file + broadcast ring buffer +
  SSE. `LogConfig.console_level` controls the console layer
  independently of the file layer; the headless `bookrack-mcp` binary
  mirrors the file directive to the console for systemd / journalctl.
- `bookrack vectors reset` and `bookrack libraries fork`: in-place
  swap of the embedding model with a clean rebuild of the vector store
  or a copy-on-write side-by-side library.
- `bookrack run` on a fresh install offers the setup wizard inline
  when no library is configured. Non-interactive callers still get the
  resolver error and a pointer at `bookrack init`.
- `bookrack run` runs headless when stdin is not a tty. Shutdown is
  driven by signal handlers or the `session.shutdown` MCP tool.
- `bookrack run` prints a five-line summary (pid, MCP address,
  inspect-with, stop-with) when the session lock is already held, and
  waits for Enter on an interactive tty so a launcher window does not
  vanish.
- Double-click launchers for all three release platforms:
  `Bookrack.app` on macOS (ad-hoc signed, opens Terminal.app on the
  Resources/bookrack-run.command stub), `Start Bookrack.cmd` on
  Windows (runs `bookrack.exe run`, pauses on error), and
  `bookrack.desktop` on Linux (asks the desktop environment to host
  the daemon in its native terminal emulator).

### Changed

- The CLI surface is now eight external subcommands: `run`, `exec`,
  `init`, `doctor`, `verify`, `diagnose`, `libraries`,
  `audit-profile`. Every write command (`ingest`, `metadata.*`,
  `vectors.*`, `corpus rebuild`, `intake ocr`) moves into the
  `bookrack run` REPL; every read command (`books`, `find`,
  `metadata show`, `pipeline-trail`, `vectors status`, `info`) moves
  behind `bookrack exec library.<tool>`. Hard guarantee: every write
  goes through the in-process scheduler, so no two processes ever
  touch the catalog or vector store at the same time.
- macOS release tarball now contains `Bookrack.app` rather than a
  flat layout. Windows and Linux keep the flat layout.

### Fixed

- Queue worker pulls ingest jobs through the in-process
  `LibraryRegistry`, so a `--force` reingest no longer needs an
  external `bookrack` invocation.
- Signal-driven shutdown (SIGINT / SIGTERM / SIGHUP) exits the
  process within ~6s instead of parking on the reedline blocking
  thread.
- `Library.store` refreshes after every successful ingest, so a
  newly-ingested book is searchable in the same daemon session.

## [0.2.0] - 2026-06-07

### Added

- `bookrack run`: daemon-REPL skeleton. Acquires the session tty
  lock, listens for signals, and serves MCP over streamable-HTTP on
  the foreground process. The REPL hosts every write command moved
  off the external CLI.
- `bookrack exec`: out-of-process discovery surface that reads the
  session lock to find the live daemon and calls MCP tools over HTTP.
- `LibraryRegistry`: in-process scheduler that owns the only
  `Catalog`, `Corpus`, and vector-store handles. MCP and the REPL
  both route through it; `bookrack-mcp` opens its own scheduler when
  run standalone.
- Persistent ingest queue under `<data_root>/.bookrack-queue.json`.
  Jobs survive across sessions and resume on the next `bookrack run`.
- TXT chapter-heading detection across Sinographic, Latin, and
  Germanic families; rules live under `<data_root>/audit-rules/`.
- `library.list_metadata` and `library.vectors_status` read MCP
  tools.
- `bookrack audit-profile` top-level command for inspecting and
  comparing audit profiles.

### Changed

- The external CLI shrinks to nine commands: every write command
  (`ingest`, `metadata.*`, `corpus rebuild`, `vectors.*`, `intake
  ocr`, `dryrun`) moves into the REPL grammar of `bookrack run`;
  every library read command moves behind `bookrack exec
  library.<tool>`.
- HTML / quality / language thresholds previously hard-coded in
  `bookrack-extract` are now externalised to `headings.toml` and
  `audit_data.toml`. The extractor version stamp bumps accordingly.
- `bookrack-mcp` acquires the session tty lock too, so a standalone
  MCP daemon and a `bookrack run` daemon cannot start in the same
  data root.

### Fixed

- `Library.store` refreshes after each ingest so a newly-ingested
  book is searchable inside the same daemon session.
- Signal-driven shutdown no longer parks on the reedline blocking
  thread; the process exits within seconds of SIGINT / SIGTERM /
  SIGHUP.
- Broken-pipe panics from the obs layer silently exit instead of
  printing a stack trace.

## [0.1.0-rc4] - 2026-06-05

Re-cut of `v0.1.0-rc3` to drop the `x86_64-apple-darwin` build target.
No behaviour change in the shipped binaries.

### Removed

- Native `x86_64-apple-darwin` release tarball. `lance-linalg`'s build
  script hard-codes `-march=native` for its AVX-512 `dist_table`
  kernel, which is incompatible with cross-compiling on the Apple
  Silicon runner, and the previously-used `macos-13` Intel runner pool
  is being deprecated. Intel macOS users run the
  `aarch64-apple-darwin` binary under Rosetta 2 instead.

## [0.1.0-rc3] - 2026-06-05

Re-cut of `v0.1.0-rc2` to harden the release pipeline. No behaviour
change in the shipped binaries.

### Changed

- Release workflow: `x86_64-apple-darwin` is now cross-compiled on the
  `macos-14` Apple Silicon runner instead of running natively on
  `macos-13`. GitHub's Intel macOS runner pool is being deprecated and
  was queuing for hours.
- Release workflow: `windows-latest` pinned to `windows-2025-vs2026`,
  pre-empting GitHub's mid-June 2026 implicit redirect.
- Release workflow: protoc is installed inline from
  `protocolbuffers/protobuf` GitHub releases instead of via
  `arduino/setup-protoc`. The third-party action has been unmaintained
  since early 2024 and still runs on Node 20.

## [0.1.0-rc2] - 2026-06-05

Re-cut of `v0.1.0-rc1` to fix the release pipeline on Windows. No
behaviour change in the shipped binaries.

### Fixed

- Release workflow: SHA-256 verification of the PDFium archive and the
  packaged release tarball now picks `sha256sum` when present and falls
  back to `shasum -a 256`. Git Bash on Windows runners has no `shasum`,
  which broke the Windows job at the verification step.

## [0.1.0-rc1] - 2026-06-05

First release candidate. Pre-release while pre-production hardening
(schema migrations, approximate-nearest-neighbour indexing, metadata)
is finalised; small-batch testing precedes a stable v0.1.0 cut.

### Added

- End-to-end pipeline: EPUB / TXT / PDF source ingest with
  text normalization, prose chunking, embedding via a local Ollama
  daemon, dense storage in LanceDB, and cited-passage search.
- CLI surface: `ingest`, `query`, `books`, `metadata`, `intake ocr`,
  `corpus rebuild`, `vectors {status,rebuild,drop,reembed}`, `dryrun`,
  `info`, `verify`, `remove`, `pipeline-trail`, `diagnose`,
  `libraries list`, `stamps reconcile`.
- MCP server (`bookrack-mcp`): streamable-HTTP transport bound to
  `127.0.0.1:8765/mcp` by default for agent clients (e.g. Claude
  Code).
- `bookrack init`: five-step interactive install wizard. Picks a data
  root, probes the PDFium dynamic library, probes Ollama for the
  configured embed model, exercises the full
  ingest → embed → query pipeline against a tempdir, then writes
  `<data_root>/config.toml` and a pointer in the platform-default
  registry.
- `bookrack doctor`: one-screen install health check. Exits non-zero
  on any FAIL row; `--json` for machine consumption.
- `bookrack-embed::probe_ollama`: lightweight `/api/tags` probe with a
  2-second default timeout, reused by the wizard and doctor.
- Portable-mode data root: a `bookrack-data/` directory beside the
  running binary is detected automatically and wins over the registry
  default. A self-contained tarball is movable to any disk without
  environment configuration.
- Platform-default registry at `<config>/bookrack/registry.toml`,
  written by `bookrack init` so subsequent `bookrack` invocations find
  their data root from any working directory.
- Per-data-root configuration file `<data_root>/config.toml` for
  `ollama_url`, `embed_model`, `mcp_addr`, `log_directive`. Resolution
  precedence is env var > root config > hardcoded default.
- Audit profiles `default`, `trust-source`, and `strict`, selectable
  per command via `--audit-profile`. A local overlay TOML under
  `<data_root>/audit-rules/audit_profile.local.toml` adjusts
  thresholds without rebuilding the binary.
- Restartable ingest: long runs survive a host idle-sleep window
  idempotently. On macOS the README documents `caffeinate -i` for
  unattended overnight runs.
- Rebuildable derived layers: `bookrack corpus rebuild` regenerates
  `corpus.db` from the opaque store, and `bookrack vectors reembed`
  reruns the embedder over chunk text in place. Both accept
  `--stale-only` to scope the refresh to partitions whose stored
  stamps lag the running binary.
- `bookrack diagnose`: scrubbed `.tar.gz` bundle of crash reports,
  recent logs, and a small catalog snapshot for bug reports.

### Documentation

- README with installation, prerequisites, and operating notes.
- `docs/UPGRADE.md`: bump-to-refresh matrix mapping each
  behaviour-sensitive dependency and stamp constant to the cheapest
  CLI invocation that restores a consistent library.
- `crates/extract/PDFIUM_VERSION.md`: pinned PDFium version with
  per-platform SHA-256 checksums (Linux x86_64, Windows x86_64, macOS
  arm64, macOS x86_64).

[Unreleased]: https://github.com/Collegium-Siderum/bookrack/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/Collegium-Siderum/bookrack/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/Collegium-Siderum/bookrack/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/Collegium-Siderum/bookrack/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/Collegium-Siderum/bookrack/compare/v0.1.0-rc4...v0.2.0
[0.1.0-rc1]: https://github.com/Collegium-Siderum/bookrack/releases/tag/v0.1.0-rc1
