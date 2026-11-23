// SPDX-License-Identifier: Apache-2.0

//! The tools an MCP client is shown, the two tables they are registered
//! from, and the documentation of both surfaces agree.
//!
//! * every tool `tools/list` hands a real client is a row of the ops
//!   method table that MCP carries, or a row of the runtime's session
//!   tool table, published under that row's name with that row's
//!   description, input schema, and output schema — and every such row
//!   is published;
//! * every answer of a tool that publishes an output schema is an
//!   instance of that schema and carries no field the schema leaves
//!   undeclared. The schema reader below implements the keywords the
//!   derived schemas use and reports any other keyword as a violation,
//!   so a schema it cannot read fails the check instead of passing it;
//! * `docs/control-plane.md` documents, between its `control-methods`
//!   anchors, exactly the methods `daemon.methods` lists, and between its
//!   `mcp-tools` anchors exactly the tools the server publishes, each
//!   once.
//!
//! The fixture is one library holding a book, a paper, and a reference
//! store, so the list answers carry rows for the schema reader to
//! descend into.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bookrack_catalog::{Catalog, NewIntake, NewPublicationAttrs};
use bookrack_config::{Config, DEFAULT_EMBED_MODEL, LibrarySelection};
use bookrack_core::ItemKind;
use bookrack_core::queue::{IngestJob, JobOp, JobState, Priority, QueueJob, QueueState};
use bookrack_embed::OllamaEmbedClient;
use bookrack_extract::{Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc};
use bookrack_extract::{envelope_filename, write_envelope};
use bookrack_mcp::ProcessEndpoints;
use bookrack_obs::LogStreamHandle;
use bookrack_ops::methods::{LIBRARY_METHODS, Output};
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::{Ops, PapersPaths};
use bookrack_refs::{NewBook, NewEntry, Refs};
use bookrack_runtime::control::methods::method_signatures;
use bookrack_runtime::control::methods::session::SESSION_TOOLS;
use bookrack_test_support::{EmbedStub, ProcessEnv, process_env};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, JsonObject};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::{Map, Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;

/// The one library the fixture serves.
const LIBRARY: &str = "parity";

/// The control-plane reference, read at compile time.
const CONTROL_PLANE_DOC: &str = include_str!("../../../docs/control-plane.md");

struct Fixture {
    _tmp: tempfile::TempDir,
    /// The embedder the paper side probed its width from.
    _stub: EmbedStub,
    /// The seeded book's intake id.
    book: i64,
    addr: SocketAddr,
    shutdown_tx: broadcast::Sender<()>,
    server: tokio::task::JoinHandle<eyre::Result<()>>,
}

impl Fixture {
    /// Seed a book with a cached extraction and a title, a paper with
    /// one, a reference store with one entry, and a finished queue job
    /// carrying a diagnostic, then boot `serve` over the library.
    async fn start() -> Fixture {
        let sandbox = process_env(ProcessEnv::isolated());
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join("parity-root");
        std::fs::create_dir_all(&root).expect("library root");
        let cfg = {
            // The registry is one file per process: write and resolve
            // under a lock so concurrent fixtures see their own entry.
            static REGISTRY: Mutex<()> = Mutex::new(());
            let _guard = REGISTRY.lock().expect("registry lock");
            sandbox.write_registry_entries(Some(LIBRARY), &[(LIBRARY, root.as_path())]);
            Arc::new(
                Config::resolve(&LibrarySelection {
                    data_dir: None,
                    library: Some(LIBRARY.to_string()),
                })
                .expect("resolve the fixture library"),
            )
        };

        let book = seed_item(
            &cfg.catalog_db(),
            &cfg.books_dir(),
            ItemKind::Book,
            "sha-parity-book",
            "A Synthetic Almanac",
            None,
        );
        seed_item(
            &cfg.papers_catalog_db(),
            &cfg.papers_dir(),
            ItemKind::Paper,
            "sha-parity-paper",
            "Synthetic Tides Revisited",
            Some("article-journal"),
        );
        seed_reference(&cfg.reference_db());

        let stub = EmbedStub::start();
        let embedder = OllamaEmbedClient::new(
            stub.url(),
            DEFAULT_EMBED_MODEL,
            Duration::from_secs(5),
            0,
            Duration::from_millis(10),
        )
        .expect("embed client");
        let papers = bookrack_query::Library::open(
            cfg.papers_corpus_db(),
            cfg.papers_catalog_db(),
            &cfg.papers_lancedb_dir(),
            embedder,
            DEFAULT_EMBED_MODEL.to_string(),
            5,
        )
        .await
        .expect("open the paper side")
        .with_kind(ItemKind::Paper);
        let ops = Ops::<OllamaEmbedClient>::catalog_only(
            cfg.corpus_db(),
            cfg.catalog_db(),
            &cfg.lancedb_dir(),
            cfg.books_dir(),
            cfg.backup_dir(),
        )
        .with_papers(
            papers,
            PapersPaths {
                corpus_db: cfg.papers_corpus_db(),
                catalog_db: cfg.papers_catalog_db(),
                lancedb_dir: cfg.papers_lancedb_dir(),
                papers_dir: cfg.papers_dir(),
            },
        );
        let registry = LibraryRegistry::from_handles(
            [LibraryHandle::new(LIBRARY, Arc::clone(&cfg), ops)],
            Some(LIBRARY.into()),
        )
        .expect("one-library registry");

        let queue_state = Arc::new(Mutex::new(QueueState::default()));
        queue_state
            .lock()
            .expect("queue state lock")
            .jobs
            .push(failed_job());

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(4);
        let serve_tx = shutdown_tx.clone();
        let endpoints = ProcessEndpoints {
            mcp_addr: addr.to_string(),
            ollama_url: "http://127.0.0.1:1".to_string(),
        };
        let server = tokio::spawn(async move {
            bookrack_mcp::serve(
                registry,
                endpoints,
                Instant::now(),
                test_identity(),
                LogStreamHandle::default(),
                queue_state,
                serve_tx,
                test_event_stream(),
                listener,
                shutdown_rx,
            )
            .await
        });

        timeout(Duration::from_secs(5), async {
            loop {
                match TcpStream::connect(addr).await {
                    Ok(stream) => break drop(stream),
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("serve accepts a connection within 5s");

        Fixture {
            _tmp: tmp,
            _stub: stub,
            book,
            addr,
            shutdown_tx,
            server,
        }
    }

    async fn connect(&self) -> RunningService<RoleClient, ()> {
        let transport =
            StreamableHttpClientTransport::from_uri(format!("http://{}/mcp", self.addr));
        ().serve(transport).await.expect("mcp client handshake")
    }

    async fn stop(self) {
        let _ = self.shutdown_tx.send(());
        let _ = timeout(Duration::from_secs(5), self.server).await;
    }
}

/// What one tool publishes: its description and its two schemas.
fn entry(
    description: &str,
    input: Map<String, Value>,
    output: Option<Map<String, Value>>,
) -> Value {
    json!({
        "description": description,
        "inputSchema": Value::Object(input),
        "outputSchema": output.map(Value::Object),
    })
}

/// Every published tool's name with its output schema, for the tools
/// that publish one.
async fn output_schemas(client: &RunningService<RoleClient, ()>) -> BTreeMap<String, Value> {
    client
        .list_all_tools()
        .await
        .expect("tools/list")
        .into_iter()
        .filter_map(|tool| {
            let schema = tool.output_schema.as_ref()?;
            Some((tool.name.to_string(), Value::Object((**schema).clone())))
        })
        .collect()
}

#[tokio::test]
async fn the_published_tools_are_the_two_tables_row_for_row() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;

    let published: BTreeMap<String, Value> = client
        .list_all_tools()
        .await
        .expect("tools/list")
        .into_iter()
        .map(|tool| {
            let description = tool.description.as_deref().unwrap_or_default();
            let input = (*tool.input_schema).clone();
            let output = tool.output_schema.as_ref().map(|schema| (**schema).clone());
            (tool.name.to_string(), entry(description, input, output))
        })
        .collect();

    let mut rows: Vec<(&str, Value)> = Vec::new();
    for spec in LIBRARY_METHODS {
        let Some(name) = spec.mcp_name else { continue };
        let output = match spec.output {
            Output::Structured(schema) => Some(schema()),
            Output::NullableObject | Output::TextOnly => None,
        };
        rows.push((name, entry(spec.summary, (spec.mcp_input_schema)(), output)));
    }
    for tool in SESSION_TOOLS {
        let row = entry(
            tool.summary,
            (tool.input_schema)(),
            Some((tool.output_schema)()),
        );
        rows.push((tool.name, row));
    }
    let row_count = rows.len();
    let tables: BTreeMap<String, Value> = rows
        .into_iter()
        .map(|(name, row)| (name.to_string(), row))
        .collect();
    assert_eq!(
        tables.len(),
        row_count,
        "two rows of the tables publish under one name"
    );

    let unregistered: Vec<&String> = tables
        .keys()
        .filter(|n| !published.contains_key(*n))
        .collect();
    let unlisted: Vec<&String> = published
        .keys()
        .filter(|n| !tables.contains_key(*n))
        .collect();
    assert!(
        unregistered.is_empty() && unlisted.is_empty(),
        "rows the server does not publish: {unregistered:?}; \
         published tools no row declares: {unlisted:?}"
    );

    let mut drifted = Vec::new();
    for (name, row) in &tables {
        let wire = &published[name];
        for field in ["description", "inputSchema", "outputSchema"] {
            if row[field] != wire[field] {
                drifted.push(format!(
                    "{name}.{field}: the table says {} and the wire says {}",
                    row[field], wire[field]
                ));
            }
        }
    }
    assert!(
        drifted.is_empty(),
        "published tools that differ from their table row:\n{}",
        drifted.join("\n")
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

/// The structured content of a successful call, which a tool that
/// publishes an output schema must carry.
async fn structured_answer(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    arguments: Value,
) -> Value {
    let result = client
        .call_tool(call(name, arguments))
        .await
        .unwrap_or_else(|e| panic!("{name}: {e:?}"));
    assert_ne!(result.is_error, Some(true), "{name} failed: {result:?}");
    result.structured_content.clone().unwrap_or_else(|| {
        panic!("{name} publishes an output schema, yet answered without structured content")
    })
}

#[tokio::test]
async fn every_answer_of_a_tool_with_an_output_schema_is_an_instance_of_it() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;
    let schemas = output_schemas(&client).await;
    let book = fx.book;

    let mut answers: Vec<(&str, Value)> = Vec::new();
    for (name, arguments) in [
        ("library.info", json!({})),
        ("library.stats", json!({})),
        ("library.categories", json!({})),
        ("library.vectors_status", json!({})),
        (
            "library.metadata.reaudit",
            json!({ "intake_id": book, "library": LIBRARY }),
        ),
        (
            "library.metadata.set",
            json!({
                "intake_id": book, "field": "publisher", "value": "Synthetic Press",
                "reason": "parity", "library": LIBRARY,
            }),
        ),
        (
            "library.metadata.contributor_add",
            json!({
                "intake_id": book, "role": "author", "name": "Ada Example",
                "reason": "parity", "library": LIBRARY,
            }),
        ),
        ("library.list_books", json!({})),
        (
            "library.find_books",
            json!({ "title_substring": "Almanac" }),
        ),
        ("library.list_metadata", json!({})),
        ("library.list_pending_reviews", json!({})),
        ("library.list_papers", json!({})),
        ("library.find_papers", json!({ "title_substring": "Tides" })),
        ("library.list_paper_metadata", json!({})),
        ("library.list_paper_pending_reviews", json!({})),
    ] {
        answers.push((name, structured_answer(&client, name, arguments).await));
    }

    let added = answers
        .iter()
        .find(|(name, _)| *name == "library.metadata.contributor_add")
        .and_then(|(_, answer)| answer["contributor_id"].as_i64())
        .expect("the contributor the fixture added");
    for (name, arguments) in [
        (
            "library.metadata.contributor_remove",
            json!({
                "intake_id": book, "contributor_id": added,
                "reason": "parity", "library": LIBRARY,
            }),
        ),
        (
            "library.metadata.clear",
            json!({
                "intake_id": book, "field": "publisher",
                "reason": "parity", "library": LIBRARY,
            }),
        ),
        (
            "library.metadata.void",
            json!({
                "intake_id": book, "field": "subtitle",
                "reason": "parity", "library": LIBRARY,
            }),
        ),
        (
            "library.metadata.ack",
            json!({ "intake_id": book, "reason": "parity", "library": LIBRARY }),
        ),
        (
            "library.metadata.approve",
            json!({ "intake_id": book, "library": LIBRARY }),
        ),
        (
            "library.metadata.reject",
            json!({ "intake_id": book, "reason": "parity", "library": LIBRARY }),
        ),
        (
            "reference.lookup",
            json!({ "book": "book_a", "entry_key": "smith" }),
        ),
        (
            "reference.overlay_set",
            json!({
                "library": LIBRARY, "book_slug": "book_a", "entry_key": "smith",
                "overlay": { "country": "United Kingdom" }, "reason": "parity",
            }),
        ),
        ("session.info", json!({})),
        ("session.logs_tail", json!({})),
        ("session.queue_status", json!({})),
        ("session.shutdown", json!({})),
    ] {
        answers.push((name, structured_answer(&client, name, arguments).await));
    }

    let called: BTreeSet<&str> = answers.iter().map(|(name, _)| *name).collect();
    let with_schema: BTreeSet<&str> = schemas.keys().map(String::as_str).collect();
    assert_eq!(
        called, with_schema,
        "every tool that publishes an output schema is called here, and only those"
    );

    let mut violations = Vec::new();
    for (name, answer) in &answers {
        let schema = &schemas[*name];
        for violation in schema_violations(schema, answer) {
            violations.push(format!("{name}: {violation}"));
        }
    }
    assert!(
        violations.is_empty(),
        "answers outside their published schema:\n{}",
        violations.join("\n")
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

#[tokio::test]
async fn the_schema_reader_refuses_what_a_schema_does_not_admit() {
    let fx = Fixture::start().await;
    let client = fx.connect().await;
    let schemas = output_schemas(&client).await;

    let stats = structured_answer(&client, "library.stats", json!({})).await;
    let stats_schema = &schemas["library.stats"];
    assert_eq!(
        schema_violations(stats_schema, &stats),
        Vec::<String>::new()
    );
    let required = stats_schema["required"][0]
        .as_str()
        .expect("library.stats requires a field");
    let mut missing = stats.clone();
    missing.as_object_mut().expect("an object").remove(required);
    assert!(
        !schema_violations(stats_schema, &missing).is_empty(),
        "an answer without the required `{required}` passed"
    );

    let books = structured_answer(&client, "library.list_books", json!({})).await;
    let books_schema = &schemas["library.list_books"];
    assert_eq!(
        schema_violations(books_schema, &books),
        Vec::<String>::new()
    );
    let row = &books["books"][0];
    let text_field = row
        .as_object()
        .expect("a book row")
        .iter()
        .find(|(_, value)| value.is_string())
        .map(|(key, _)| key.clone())
        .expect("a book row carries a string field");

    let mut retyped = books.clone();
    retyped["books"][0][&text_field] = json!(7);
    assert!(
        !schema_violations(books_schema, &retyped).is_empty(),
        "a number in the string field `{text_field}` of a book row passed"
    );
    let mut widened = books.clone();
    widened["books"][0]["undeclared_field"] = json!(true);
    assert!(
        !schema_violations(books_schema, &widened).is_empty(),
        "a book row with an undeclared field passed"
    );
    let mut negative = books.clone();
    negative["total"] = json!(-1);
    assert!(
        !schema_violations(books_schema, &negative).is_empty(),
        "a negative count passed an unsigned field"
    );

    let unread = json!({ "type": "object", "patternProperties": {} });
    assert!(
        schema_violations(&unread, &json!({}))
            .iter()
            .any(|v| v.contains("patternProperties")),
        "a keyword the reader does not implement passed silently"
    );

    let _ = client.cancel().await;
    fx.stop().await;
}

/// Every way `value` falls outside `schema`, one line each, empty when
/// it is an instance.
fn schema_violations(schema: &Value, value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    check(schema, schema, value, "$", &mut out);
    out
}

/// The keywords that only annotate: they constrain nothing.
const ANNOTATIONS: &[&str] = &["$schema", "$defs", "description", "default", "format"];

/// Check `value` at `path` against `schema`, resolving `$ref` in
/// `root`'s `$defs`. Beyond JSON Schema, an object schema that lists
/// `properties` and no `additionalProperties` admits no other key: an
/// answer may not carry a field its schema does not declare.
fn check(root: &Value, schema: &Value, value: &Value, path: &str, out: &mut Vec<String>) {
    let Some(keywords) = schema.as_object() else {
        match schema {
            Value::Bool(true) => {}
            Value::Bool(false) => out.push(format!("{path}: no value is admitted here")),
            _ => out.push(format!("{path}: the schema is not an object: {schema}")),
        }
        return;
    };
    for (keyword, argument) in keywords {
        match keyword.as_str() {
            k if ANNOTATIONS.contains(&k) => {}
            "$ref" => {
                let target = argument
                    .as_str()
                    .and_then(|r| r.strip_prefix("#/$defs/"))
                    .and_then(|name| root["$defs"].get(name));
                match target {
                    Some(target) => check(root, target, value, path, out),
                    None => out.push(format!("{path}: unresolvable $ref {argument}")),
                }
            }
            "type" => {
                let types: Vec<&str> = match argument {
                    Value::String(one) => vec![one.as_str()],
                    Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                };
                if !types.iter().any(|t| has_type(value, t)) {
                    out.push(format!("{path}: {value} is not of type {argument}"));
                }
            }
            // The two are read together, once: under `properties` when
            // the schema lists them, else under `additionalProperties`.
            "properties" | "additionalProperties" => {
                if keyword == "properties" || !keywords.contains_key("properties") {
                    check_object(root, keywords, value, path, out);
                }
            }
            "required" => {
                if let Some(object) = value.as_object() {
                    for name in argument.as_array().into_iter().flatten() {
                        let name = name.as_str().unwrap_or_default();
                        if !object.contains_key(name) {
                            out.push(format!("{path}: the required `{name}` is missing"));
                        }
                    }
                }
            }
            "items" => {
                if let Some(items) = value.as_array() {
                    for (i, item) in items.iter().enumerate() {
                        check(root, argument, item, &format!("{path}[{i}]"), out);
                    }
                }
            }
            "anyOf" => {
                let branches = argument.as_array().map(Vec::as_slice).unwrap_or_default();
                let fits = branches.iter().any(|branch| {
                    let mut probe = Vec::new();
                    check(root, branch, value, path, &mut probe);
                    probe.is_empty()
                });
                if !fits {
                    out.push(format!("{path}: {value} fits no branch of anyOf"));
                }
            }
            "minimum" => {
                if let (Some(number), Some(minimum)) = (value.as_f64(), argument.as_f64())
                    && number < minimum
                {
                    out.push(format!("{path}: {number} is below the minimum {minimum}"));
                }
            }
            other => out.push(format!(
                "{path}: the schema uses `{other}`, a keyword this reader does not implement"
            )),
        }
    }
}

/// Check an object value's keys against `keywords`' `properties` and
/// `additionalProperties`.
fn check_object(
    root: &Value,
    keywords: &Map<String, Value>,
    value: &Value,
    path: &str,
    out: &mut Vec<String>,
) {
    let Some(object) = value.as_object() else {
        return;
    };
    let properties = keywords.get("properties").and_then(Value::as_object);
    for (key, field) in object {
        let field_path = format!("{path}.{key}");
        match (
            properties.and_then(|p| p.get(key)),
            keywords.get("additionalProperties"),
        ) {
            (Some(declared), _) => check(root, declared, field, &field_path, out),
            (None, Some(extra)) => check(root, extra, field, &field_path, out),
            (None, None) if properties.is_some() => {
                out.push(format!("{field_path}: the schema declares no such field"));
            }
            (None, None) => {}
        }
    }
}

/// Whether `value` is of the JSON Schema type `name`.
fn has_type(value: &Value, name: &str) -> bool {
    match name {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "string" => value.is_string(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "number" => value.is_number(),
        "integer" => {
            value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0)
        }
        _ => false,
    }
}

#[test]
fn the_control_plane_reference_documents_every_method_it_serves() {
    let documented = documented_names(CONTROL_PLANE_DOC, "control-methods");
    let served: BTreeSet<String> = method_signatures()
        .iter()
        .map(|signature| signature.name.to_string())
        .collect();
    assert_same_names(&documented, &served, "control-methods", "served");
}

#[test]
fn the_mcp_section_documents_every_published_tool() {
    let documented = documented_names(CONTROL_PLANE_DOC, "mcp-tools");
    let published: BTreeSet<String> = bookrack_mcp::list_tools()
        .into_iter()
        .map(|tool| tool.name)
        .collect();
    assert_same_names(&documented, &published, "mcp-tools", "published");
}

fn assert_same_names(
    documented: &BTreeSet<String>,
    live: &BTreeSet<String>,
    anchor: &str,
    verb: &str,
) {
    let only_documented: Vec<&String> = documented.difference(live).collect();
    let only_live: Vec<&String> = live.difference(documented).collect();
    assert!(
        only_documented.is_empty() && only_live.is_empty(),
        "docs/control-plane.md, `{anchor}` anchors: documented but not {verb}: \
         {only_documented:?}; {verb} but not documented: {only_live:?}"
    );
}

/// The names the entries between `doc`'s `<anchor>:start` and
/// `<anchor>:end` comments document.
///
/// An entry is a line that opens a top-level list item with a code
/// span. Its head runs to the first em dash, across continuation lines,
/// and holds code-spanned names separated by `/`; a name that starts
/// with a dot completes the one before it (`.approve` after
/// `papers.metadata.ack` is `papers.metadata.approve`). A head holding
/// anything else, an entry with no dash, and a name documented twice
/// all fail: the rule is only as good as its refusal to guess.
fn documented_names(doc: &str, anchor: &str) -> BTreeSet<String> {
    let start = format!("<!-- {anchor}:start -->");
    let end = format!("<!-- {anchor}:end -->");
    assert_eq!(doc.matches(&start).count(), 1, "one `{start}` in the doc");
    assert_eq!(doc.matches(&end).count(), 1, "one `{end}` in the doc");
    let (_, after) = doc.split_once(&start).expect("the start anchor");
    let (region, _) = after
        .split_once(&end)
        .unwrap_or_else(|| panic!("`{end}` follows `{start}`"));

    let lines: Vec<&str> = region.lines().collect();
    let mut names = BTreeSet::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        i += 1;
        let Some(item) = line.strip_prefix("- ") else {
            continue;
        };
        if !item.starts_with('`') {
            continue;
        }
        let mut head = item.to_string();
        while !head.contains('\u{2014}') {
            let next = lines
                .get(i)
                .filter(|next| next.starts_with("  "))
                .unwrap_or_else(|| panic!("the entry `{line}` has no dash"));
            head.push(' ');
            head.push_str(next.trim());
            i += 1;
        }
        let head = head.split('\u{2014}').next().unwrap_or_default();
        let mut previous: Option<String> = None;
        for part in head.split('/') {
            let span = part
                .trim()
                .strip_prefix('`')
                .and_then(|p| p.strip_suffix('`'))
                .unwrap_or_else(|| panic!("the entry head `{head}` holds more than names"));
            let name = match (span.strip_prefix('.'), &previous) {
                (Some(last), Some(before)) => {
                    let (stem, _) = before.rsplit_once('.').expect("a dotted name before");
                    format!("{stem}.{last}")
                }
                (Some(_), None) => panic!("the entry head `{head}` opens with a shorthand"),
                (None, _) => span.to_string(),
            };
            assert!(
                !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c == '_' || c == '.'),
                "the entry head `{head}` names `{name}`, which is not a method name"
            );
            assert!(names.insert(name.clone()), "`{name}` is documented twice");
            previous = Some(name);
        }
    }
    names
}

/// Register one item of `kind` with a cached extraction under `dir`
/// and its title, and return its intake id.
fn seed_item(
    catalog_db: &Path,
    dir: &Path,
    kind: ItemKind,
    sha: &str,
    title: &str,
    csl_type: Option<&str>,
) -> i64 {
    std::fs::create_dir_all(dir).expect("intake store");
    let mut catalog = Catalog::open_for_write_unbacked(catalog_db).expect("open catalog");
    let intake_id = catalog
        .register_intake(kind, &NewIntake::new(sha).format("txt").byte_size(1))
        .expect("register intake")
        .into_intake()
        .intake_id;
    let path = dir.join(envelope_filename(kind, intake_id));
    write_envelope(&path, &sample_extraction(), intake_id, sha).expect("write envelope");
    catalog
        .set_stored_path(kind, intake_id, &path.to_string_lossy())
        .expect("stored path");
    let mut attrs = NewPublicationAttrs::new(intake_id, kind);
    attrs.title = Some(title.to_string());
    attrs.csl_type = csl_type.map(str::to_string);
    catalog.upsert_publication_attrs(&attrs).expect("attrs");
    intake_id
}

/// One reference book with one entry, `smith`.
fn seed_reference(path: &Path) {
    let refs =
        Refs::open_for_write(path, &path.with_file_name("backup")).expect("create reference.db");
    refs.upsert_book(&NewBook {
        book_slug: "book_a".to_string(),
        schema_name: "name_translation".to_string(),
        schema_version: 1,
        parser_version: "0.1.0".to_string(),
        title_zh: "book book_a".to_string(),
        title_en: None,
        edition: None,
        publisher: None,
        year: None,
        isbn: None,
        authority_rank: 10,
        built_at: "2026-06-25T00:00:00Z".to_string(),
        intake_id: None,
    })
    .expect("reference book");
    refs.upsert_entry(&NewEntry {
        book_slug: "book_a".to_string(),
        entry_key: "smith".to_string(),
        headword: "smith".to_string(),
        aliases: vec![],
        payload: json!({ "country": "UK" }),
        fts_text: "smith".to_string(),
        source: json!({ "book_slug": "book_a", "page": 1, "sheet": 1 }),
        quality_flags: vec!["pair_mismatch".to_string()],
    })
    .expect("reference entry");
}

/// A two-block extraction with no bibliographic fields.
fn sample_extraction() -> Extraction {
    Extraction {
        blocks: vec![
            Block {
                kind: BlockKind::Heading { level: 1 },
                text: "Chapter One".into(),
                source_unit: 0,
                style: None,
            },
            Block {
                kind: BlockKind::Body,
                text: "Some sample prose for the audit body sample.".into(),
                source_unit: 0,
                style: None,
            },
        ],
        toc: Toc::default(),
        biblio: Biblio::default(),
        provenance: Provenance {
            adapter: "txt".into(),
            extractor_version: 1,
            text_layer_quality: TextLayerQuality::BornDigital,
            skipped_units: vec![],
            derived_from_sha256: None,
            partial_pages: None,
            source_of_structure: None,
            fallbacks: Vec::new(),
            rule: None,
        },
    }
}

/// A failed book job carrying the diagnostic the daemon records beside
/// a failure it recognised.
fn failed_job() -> QueueJob {
    let at = chrono::DateTime::from_timestamp(1_700_000_000, 0).expect("timestamp");
    QueueJob {
        id: "job-parity".to_string(),
        library: LIBRARY.to_string(),
        priority: Priority::Normal,
        kind: ItemKind::Book,
        op: JobOp::Ingest(IngestJob::file("/srv/books/parity.epub")),
        state: JobState::Failed,
        queued_at: at,
        started_at: Some(at),
        finished_at: Some(at),
        error: Some("ingest: extract error: no body blocks".to_string()),
        problem: Some(
            bookrack_core::Problem::new("cannot ingest: the extraction produced no body")
                .hint("Check that the source is not image-only."),
        ),
        merged_into: None,
    }
}

/// A tool call: `name` with a JSON-object argument payload.
fn call(name: &str, arguments: Value) -> CallToolRequestParams {
    let arguments: JsonObject = arguments
        .as_object()
        .cloned()
        .expect("tool arguments are a JSON object");
    CallToolRequestParams::new(name.to_string()).with_arguments(arguments)
}

/// A daemon identity for a server no daemon stands behind.
fn test_identity() -> bookrack_core::DaemonIdentity {
    bookrack_core::DaemonIdentity {
        instance_id: "018f0000-0000-7000-8000-000000000000".to_string(),
        version: "0.0.0-test".to_string(),
        pid: 4242,
        runtime_dir: "/run/bookrack-test".to_string(),
        control_sock: None,
        started_at: "2026-01-01T00:00:00+00:00".to_string(),
    }
}

/// An event stream no control-plane client is attached to.
fn test_event_stream() -> bookrack_runtime::control::events::EventStreamHandle {
    use bookrack_runtime::control::events::{DaemonState, DaemonStateFlag, EventStreamHandle};

    EventStreamHandle::new(8, Arc::new(DaemonStateFlag::new(DaemonState::Idle)))
}
