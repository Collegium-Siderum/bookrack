// SPDX-License-Identifier: Apache-2.0

//! The method table's dispatcher, driven the way a transport drives it:
//! a spec from [`LIBRARY_METHODS`], a surface, a caller, and the call's
//! params as JSON.
//!
//! The fixture mounts one library, `alpha`, with a catalog-only book
//! side and a papers backend, so both report reads, the book-side
//! metadata writes, and the registry's routing can run without an
//! embedding service.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bookrack_catalog::{Catalog, NewIntake, NewOverride, NewPublicationAttrs};
use bookrack_config::Config;
use bookrack_core::ItemKind;
use bookrack_embed::{Embedder, Result as EmbedResult};
use bookrack_extract::{
    Biblio, Block, BlockKind, Extraction, Provenance, TextLayerQuality, Toc, envelope_filename,
    write_envelope,
};
use bookrack_ops::error_class::ErrorKind;
use bookrack_ops::methods::dispatch::{
    DispatchContext, DispatchError, HostContext, Reply, dispatch,
};
use bookrack_ops::methods::{LIBRARY_METHODS, Surface, library_method};
use bookrack_ops::reads::info::LibraryInfoContext;
use bookrack_ops::registry::{LibraryHandle, LibraryRegistry};
use bookrack_ops::write_session::{ChangeSink, Region, WriteIntent};
use bookrack_ops::{Caller, Ops, PapersPaths};
use serde_json::{Value, json};
use tempfile::TempDir;

/// A constant-vector embedder, so the fixture opens a warm papers
/// library without a live embedding service.
struct Fake;

impl Embedder for Fake {
    fn embed_batch(
        &self,
        texts: &[String],
    ) -> impl Future<Output = EmbedResult<Vec<Vec<f32>>>> + Send {
        let n = texts.len();
        async move { Ok(vec![vec![0.25f32; 8]; n]) }
    }
}

/// The libraries a sink heard announced, in order.
#[derive(Default)]
struct Announced(Mutex<Vec<String>>);

impl ChangeSink for Announced {
    fn exclusive_write_started(&self, _intent: &WriteIntent) {}
    fn library_changed(&self, library: &str) {
        self.0.lock().unwrap().push(library.to_owned());
    }
    fn exclusive_write_ended(&self, _intent: &WriteIntent) {}
}

/// A host whose card context names the library's own root, or fails.
struct Host {
    fail: bool,
}

impl HostContext for Host {
    fn library_info_context(&self, cfg: &Config) -> eyre::Result<LibraryInfoContext> {
        if self.fail {
            eyre::bail!("the library's index profile does not resolve");
        }
        Ok(LibraryInfoContext {
            data_dir: cfg.data_dir().display().to_string(),
            library_name: Some("alpha".to_owned()),
            resolution_source: "explicit".to_owned(),
            shadowed_default: None,
            library_identification: None,
            ollama_url: "http://127.0.0.1:9".to_owned(),
            embed_model_configured: "test-model".to_owned(),
            mcp_addr: "disabled".to_owned(),
        })
    }
}

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    handle: Arc<LibraryHandle<Fake>>,
    registry: Arc<LibraryRegistry<Fake>>,
    announced: Arc<Announced>,
}

impl Fixture {
    async fn build() -> Fixture {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("books")).expect("books dir");
        std::fs::create_dir_all(root.join("papers")).expect("papers dir");
        Catalog::open_for_write_unbacked(&root.join("catalog.db")).expect("seed catalog");
        Catalog::open_for_write_unbacked(&root.join("papers_catalog.db"))
            .expect("seed paper catalog");
        bookrack_corpus::Corpus::open_for_write(&root.join("papers_corpus.db"))
            .expect("seed paper corpus");
        let papers_library = bookrack_query::Library::open(
            root.join("papers_corpus.db"),
            root.join("papers_catalog.db"),
            &root.join("lancedb_papers"),
            Fake,
            "fake-model".to_owned(),
            5,
        )
        .await
        .expect("open papers library")
        .with_kind(ItemKind::Paper);
        let ops = Ops::catalog_only(
            root.join("corpus.db"),
            root.join("catalog.db"),
            &root.join("lancedb"),
            root.join("books"),
            root.join("backup"),
        )
        .with_papers(
            papers_library,
            PapersPaths {
                corpus_db: root.join("papers_corpus.db"),
                catalog_db: root.join("papers_catalog.db"),
                lancedb_dir: root.join("lancedb_papers"),
                papers_dir: root.join("papers"),
            },
        );
        let handle = LibraryHandle::new("alpha", Arc::new(Config::new(root.clone())), ops);
        let announced = Arc::new(Announced::default());
        let registry = LibraryRegistry::from_handles_with_sink(
            [Arc::clone(&handle)],
            Some("alpha".to_owned()),
            Arc::clone(&announced) as Arc<dyn ChangeSink>,
        )
        .expect("a one-library registry");
        Fixture {
            _tmp: tmp,
            root,
            handle,
            registry,
            announced,
        }
    }

    /// Run the method the control plane calls `name` on `surface`.
    async fn call(
        &self,
        name: &str,
        surface: Surface,
        caller: &Caller,
        params: Value,
    ) -> Result<Reply, DispatchError> {
        self.call_with_host(name, surface, caller, params, &Host { fail: false })
            .await
    }

    async fn call_with_host(
        &self,
        name: &str,
        surface: Surface,
        caller: &Caller,
        params: Value,
        host: &Host,
    ) -> Result<Reply, DispatchError> {
        let spec = library_method(name).unwrap_or_else(|| panic!("{name} is in the table"));
        let ctx = DispatchContext {
            registry: &self.registry,
            caller,
            host,
        };
        dispatch(&ctx, spec, surface, Some(params)).await
    }

    fn write_audit_rule(&self, file: &str, contents: &str) {
        let rules = self.root.join("audit-rules");
        std::fs::create_dir_all(&rules).expect("audit-rules dir");
        std::fs::write(rules.join(file), contents).expect("write overlay");
    }

    /// A book with a cached extraction and the placeholder title
    /// `Untitled` set as an override.
    fn seed_placeholder_book(&self, sha: &str) -> i64 {
        let catalog_db = self.root.join("catalog.db");
        let intake_id = seed_intake(&catalog_db, &self.root.join("books"), ItemKind::Book, sha);
        Catalog::open_for_write_unbacked(&catalog_db)
            .expect("open catalog")
            .set_override(&NewOverride::new(
                intake_id,
                ItemKind::Book,
                "title",
                Some("Untitled".to_owned()),
                "human",
            ))
            .expect("title override");
        intake_id
    }

    /// A paper with a cached extraction whose reported title is the
    /// placeholder `Untitled`.
    fn seed_placeholder_paper(&self, sha: &str) -> i64 {
        let catalog_db = self.root.join("papers_catalog.db");
        let intake_id = seed_intake(&catalog_db, &self.root.join("papers"), ItemKind::Paper, sha);
        let mut attrs = NewPublicationAttrs::new(intake_id, ItemKind::Paper);
        attrs.title = Some("Untitled".to_owned());
        attrs.csl_type = Some("article-journal".to_owned());
        Catalog::open_for_write_unbacked(&catalog_db)
            .expect("open paper catalog")
            .upsert_publication_attrs(&attrs)
            .expect("attrs");
        intake_id
    }
}

/// Register an intake of `kind` and write its cached extraction under
/// `dir`.
fn seed_intake(catalog_db: &Path, dir: &Path, kind: ItemKind, sha: &str) -> i64 {
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
    intake_id
}

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

/// The judgement a failed call carries.
type Judgement = bookrack_ops::error_class::ClassifiedError;

/// The judgement `result` carries; it must be a failure the dispatcher
/// judged.
fn judged(result: Result<Reply, DispatchError>) -> Judgement {
    match result {
        Ok(reply) => panic!("the call must fail: {}", reply.text),
        Err(DispatchError::Judged(classified)) => classified,
        Err(DispatchError::Host { method, report }) => {
            panic!("{method}: the host hook failed instead: {report:#}")
        }
    }
}

/// The flags on the report's `title` row.
fn title_flags(report: &Value) -> Vec<String> {
    report["fields"]
        .as_array()
        .unwrap_or_else(|| panic!("a report with fields: {report}"))
        .iter()
        .find(|row| row["field"] == "title")
        .unwrap_or_else(|| panic!("a title row: {report}"))["flags"]
        .as_array()
        .expect("a flags array")
        .iter()
        .map(|flag| flag.as_str().expect("a flag string").to_owned())
        .collect()
}

/// Every surface a row is carried on.
fn surfaces_of(spec: &bookrack_ops::methods::MethodSpec) -> Vec<Surface> {
    match spec.mcp_name {
        Some(_) => vec![Surface::ControlPlane, Surface::Mcp],
        None => vec![Surface::ControlPlane],
    }
}

/// Every row of the table reaches a body on every surface it is
/// carried on. A call naming a library the registry does not hold is
/// refused by routing or by the params, never by a missing body, which
/// would be a fault.
#[tokio::test]
async fn every_row_reaches_a_body_on_each_of_its_surfaces() {
    let fx = Fixture::build().await;
    for spec in LIBRARY_METHODS {
        for surface in surfaces_of(spec) {
            let ctx = DispatchContext {
                registry: &fx.registry,
                caller: &Caller::control_plane(),
                host: &Host { fail: false },
            };
            let params = json!({ "library": "ghost", "name": "ghost" });
            let classified = judged(dispatch(&ctx, spec, surface, Some(params)).await);
            assert!(
                matches!(
                    classified.kind,
                    ErrorKind::InvalidLibrary | ErrorKind::InvalidParams
                ),
                "{} on {surface:?}: {:?} {}",
                spec.name,
                classified.kind,
                classified.problem.summary
            );
        }
    }
}

/// The book report grades under the library's own audit profile
/// overlay on both surfaces, as a re-audit of the book would store it.
#[tokio::test]
async fn the_book_report_grades_under_the_librarys_profile_overlay() {
    let fx = Fixture::build().await;
    let book = fx.seed_placeholder_book("sha-book-report");
    for surface in [Surface::ControlPlane, Surface::Mcp] {
        let report = fx
            .call(
                "library.show_metadata_report",
                surface,
                &Caller::mcp(),
                json!({ "intake_id": book }),
            )
            .await
            .expect("the report");
        assert!(
            title_flags(&report.value).contains(&"placeholder_value".to_owned()),
            "the shipped profile flags a placeholder title: {}",
            report.text
        );
    }

    fx.write_audit_rule(
        "audit_profile.local.toml",
        "schema_version = 1\n\n[title]\nplaceholder_check = false\n",
    );
    for surface in [Surface::ControlPlane, Surface::Mcp] {
        let report = fx
            .call(
                "library.show_metadata_report",
                surface,
                &Caller::mcp(),
                json!({ "intake_id": book }),
            )
            .await
            .expect("the report");
        assert!(
            !title_flags(&report.value).contains(&"placeholder_value".to_owned()),
            "on {surface:?} the overlay turns the placeholder check off: {}",
            report.text
        );
    }
}

/// The paper report grades under the library's paper profile overlay,
/// and under a built-in profile when `audit_profile` names one.
#[tokio::test]
async fn the_paper_report_grades_under_the_overlay_or_the_named_profile() {
    let fx = Fixture::build().await;
    let paper = fx.seed_placeholder_paper("sha-paper-report");
    let report = |profile: Option<&str>| {
        let mut params = json!({ "intake_id": paper });
        if let Some(profile) = profile {
            params["audit_profile"] = json!(profile);
        }
        params
    };

    let shipped = fx
        .call(
            "library.show_paper_metadata_report",
            Surface::Mcp,
            &Caller::mcp(),
            report(None),
        )
        .await
        .expect("the report");
    assert!(
        title_flags(&shipped.value).contains(&"placeholder_value".to_owned()),
        "the shipped profile flags a placeholder title: {}",
        shipped.text
    );

    fx.write_audit_rule(
        "paper_audit_profile.local.toml",
        "schema_version = 1\n\n[title]\nplaceholder_check = false\n",
    );
    for surface in [Surface::ControlPlane, Surface::Mcp] {
        let overlaid = fx
            .call(
                "library.show_paper_metadata_report",
                surface,
                &Caller::mcp(),
                report(None),
            )
            .await
            .expect("the report");
        assert!(
            !title_flags(&overlaid.value).contains(&"placeholder_value".to_owned()),
            "on {surface:?} the overlay turns the placeholder check off: {}",
            overlaid.text
        );
    }

    let strict = fx
        .call(
            "library.show_paper_metadata_report",
            Surface::Mcp,
            &Caller::mcp(),
            report(Some("strict")),
        )
        .await
        .expect("the report");
    assert_eq!(strict.value["profile"], "strict", "{}", strict.text);

    let refused = judged(
        fx.call(
            "library.show_paper_metadata_report",
            Surface::Mcp,
            &Caller::mcp(),
            report(Some("bogus")),
        )
        .await,
    );
    assert_eq!(refused.kind, ErrorKind::InvalidParams);
    assert!(
        refused.problem.summary.contains("bogus"),
        "{}",
        refused.problem.summary
    );
}

/// One book write, asked for on each surface: the control plane
/// answers with the book, the field, `ok`, and the library beside the
/// audit identity; MCP answers with the audit identity alone. Each
/// write records the caller the transport passed, and announces the
/// library.
#[tokio::test]
async fn a_book_write_answers_in_each_surfaces_shape_and_records_its_caller() {
    let fx = Fixture::build().await;
    let book = fx.seed_placeholder_book("sha-book-write");

    let control = fx
        .call(
            "metadata.set",
            Surface::ControlPlane,
            &Caller::control_plane(),
            json!({ "book": book, "field": "publisher", "value": "A Press" }),
        )
        .await
        .expect("the control-plane write");
    assert_eq!(control.value["ok"], true, "{}", control.text);
    assert_eq!(control.value["library"], "alpha", "{}", control.text);
    assert_eq!(control.value["book"], book, "{}", control.text);
    assert_eq!(control.value["field"], "publisher", "{}", control.text);
    assert_eq!(
        control.value["actor_detail"], "control_plane",
        "{}",
        control.text
    );
    assert!(control.value["audit_id"].is_i64(), "{}", control.text);

    let mcp = fx
        .call(
            "metadata.set",
            Surface::Mcp,
            &Caller::mcp(),
            json!({
                "intake_id": book,
                "field": "publisher",
                "value": "Another Press",
                "reason": "the copyright page",
                "library": "alpha",
            }),
        )
        .await
        .expect("the MCP write");
    assert_eq!(mcp.value["actor_kind"], "llm", "{}", mcp.text);
    assert_eq!(mcp.value["actor_detail"], "mcp", "{}", mcp.text);
    for key in ["ok", "library", "book", "field"] {
        assert!(mcp.value.get(key).is_none(), "{key} in {}", mcp.text);
    }

    assert_eq!(
        *fx.announced.0.lock().unwrap(),
        vec!["alpha".to_owned(), "alpha".to_owned()],
        "each write announces its library once"
    );
}

/// A re-audit answers with the judgement it stored, in each surface's
/// shape, and the control plane's named profile is checked first.
#[tokio::test]
async fn a_reaudit_answers_with_the_stored_judgement_on_each_surface() {
    let fx = Fixture::build().await;
    let book = fx.seed_placeholder_book("sha-reaudit");

    let control = fx
        .call(
            "metadata.reaudit",
            Surface::ControlPlane,
            &Caller::control_plane(),
            json!({ "book": book }),
        )
        .await
        .expect("the control-plane re-audit");
    assert_eq!(control.value["book"], book, "{}", control.text);
    assert!(control.value["verdict"].is_string(), "{}", control.text);
    assert!(control.value.get("intake_id").is_none(), "{}", control.text);

    let mcp = fx
        .call(
            "metadata.reaudit",
            Surface::Mcp,
            &Caller::mcp(),
            json!({ "intake_id": book, "library": "alpha" }),
        )
        .await
        .expect("the MCP re-audit");
    assert_eq!(mcp.value["intake_id"], book, "{}", mcp.text);
    assert_eq!(
        mcp.value["verdict"], control.value["verdict"],
        "{}",
        mcp.text
    );

    let refused = judged(
        fx.call(
            "metadata.reaudit",
            Surface::ControlPlane,
            &Caller::control_plane(),
            json!({ "book": book, "audit_profile": "bogus" }),
        )
        .await,
    );
    assert_eq!(refused.kind, ErrorKind::InvalidParams);
}

/// Params that do not fit are refused under the name the call used.
#[tokio::test]
async fn malformed_params_are_refused_under_the_surfaces_own_name() {
    let fx = Fixture::build().await;
    for (surface, name) in [
        (Surface::ControlPlane, "metadata.set"),
        (Surface::Mcp, "library.metadata.set"),
    ] {
        let refused = judged(
            fx.call(
                "metadata.set",
                surface,
                &Caller::mcp(),
                json!({ "field": "title" }),
            )
            .await,
        );
        assert_eq!(refused.kind, ErrorKind::InvalidParams);
        assert!(
            refused
                .problem
                .summary
                .starts_with(&format!("invalid {name} params:")),
            "{}",
            refused.problem.summary
        );
    }
}

/// A write while another session holds the write mutex is busy and
/// may be retried; a read is not held up.
#[tokio::test]
async fn a_write_while_another_session_is_open_is_busy() {
    let fx = Fixture::build().await;
    let book = fx.seed_placeholder_book("sha-busy");
    let held = fx
        .registry
        .begin_write(&fx.handle, Region::Library, Caller::cli())
        .expect("the mutex is free");

    let busy = judged(
        fx.call(
            "metadata.ack",
            Surface::Mcp,
            &Caller::mcp(),
            json!({ "intake_id": book, "reason": "r", "library": "alpha" }),
        )
        .await,
    );
    assert_eq!(busy.kind, ErrorKind::Busy);
    assert!(busy.problem.data.retryable, "{:?}", busy.problem);

    fx.call("library.stats", Surface::Mcp, &Caller::mcp(), json!({}))
        .await
        .expect("a read runs beside the open session");
    drop(held);
}

/// `library.info` selects its library under `name` on the control
/// plane and under `library` over MCP, and takes the process half of
/// its card from the host.
#[tokio::test]
async fn the_library_card_routes_by_each_surfaces_key_and_asks_the_host() {
    let fx = Fixture::build().await;

    let control = fx
        .call(
            "library.info",
            Surface::ControlPlane,
            &Caller::control_plane(),
            json!({ "name": "alpha" }),
        )
        .await
        .expect("the card");
    assert_eq!(
        control.value["data_dir"],
        fx.root.display().to_string(),
        "{}",
        control.text
    );
    let refused = judged(
        fx.call(
            "library.info",
            Surface::ControlPlane,
            &Caller::control_plane(),
            json!({ "name": "ghost" }),
        )
        .await,
    );
    assert_eq!(refused.kind, ErrorKind::InvalidLibrary);
    // MCP reads `library`, so a `name` is an unknown field and the call
    // routes to the default.
    fx.call(
        "library.info",
        Surface::Mcp,
        &Caller::mcp(),
        json!({ "name": "ghost" }),
    )
    .await
    .expect("the default library's card");
    let refused = judged(
        fx.call(
            "library.info",
            Surface::Mcp,
            &Caller::mcp(),
            json!({ "library": "ghost" }),
        )
        .await,
    );
    assert_eq!(refused.kind, ErrorKind::InvalidLibrary);

    match fx
        .call_with_host(
            "library.info",
            Surface::Mcp,
            &Caller::mcp(),
            json!({}),
            &Host { fail: true },
        )
        .await
    {
        Err(DispatchError::Host { method, report }) => {
            assert_eq!(method, "library.info");
            assert!(format!("{report}").contains("index profile"), "{report:#}");
        }
        Err(DispatchError::Judged(classified)) => {
            panic!("the host's failure was judged here: {classified:?}")
        }
        Ok(reply) => panic!("the card must fail: {}", reply.text),
    }
}

/// An item read for an id no intake has answers `null`.
#[tokio::test]
async fn an_unknown_item_reads_as_null() {
    let fx = Fixture::build().await;
    for name in [
        "library.show_book",
        "library.show_metadata_report",
        "library.show_audit_trail",
        "library.show_paper",
        "library.show_paper_audit_trail",
        "papers.fetch_source",
    ] {
        let reply = fx
            .call(
                name,
                Surface::Mcp,
                &Caller::mcp(),
                json!({ "intake_id": 9999 }),
            )
            .await
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(reply.value, Value::Null, "{name}: {}", reply.text);
        assert_eq!(reply.text, "null", "{name}");
    }
}

/// The text of a reply keeps the result's declared field order; only
/// the JSON value is free to order keys its own way.
#[tokio::test]
async fn the_reply_text_keeps_the_declared_field_order() {
    let fx = Fixture::build().await;
    let book = fx.seed_placeholder_book("sha-order");
    let reply = fx
        .call(
            "metadata.ack",
            Surface::Mcp,
            &Caller::mcp(),
            json!({ "intake_id": book, "reason": "r", "library": "alpha" }),
        )
        .await
        .expect("the write");
    assert!(
        reply.text.starts_with(r#"{"audit_id":"#),
        "the outcome's first field leads the text: {}",
        reply.text
    );
    assert_eq!(
        serde_json::from_str::<Value>(&reply.text).expect("the text is JSON"),
        reply.value
    );
}

/// The intake ids on a listing reply's rows, in order.
fn listed_ids(reply: &Reply) -> Vec<i64> {
    reply.value["books"]
        .as_array()
        .unwrap_or_else(|| panic!("a listing with books: {}", reply.text))
        .iter()
        .map(|row| row["intake_id"].as_i64().expect("a row's intake id"))
        .collect()
}

/// A listing takes its page from the call: `limit` bounds the rows and
/// `offset` skips them, each as sent. With three books on file, the
/// first two rows and the row after them are distinct, exact pages.
#[tokio::test]
async fn a_listing_returns_the_page_the_params_name() {
    let fx = Fixture::build().await;
    let books = [
        fx.seed_placeholder_book("sha-page-1"),
        fx.seed_placeholder_book("sha-page-2"),
        fx.seed_placeholder_book("sha-page-3"),
    ];
    let page = |params: Value| async {
        let reply = fx
            .call(
                "library.list_books",
                Surface::ControlPlane,
                &Caller::control_plane(),
                params,
            )
            .await
            .expect("the listing");
        listed_ids(&reply)
    };

    let all = page(json!({})).await;
    assert_eq!(all.len(), 3, "every seeded book is listed: {all:?}");
    for book in books {
        assert!(all.contains(&book), "{book} is listed: {all:?}");
    }
    assert_eq!(page(json!({ "limit": 2 })).await, all[..2]);
    assert_eq!(page(json!({ "limit": 2, "offset": 2 })).await, all[2..]);
}

/// The metadata listings check the filter vocabularies the params
/// carry: a confidence the catalog does not grade with is the caller's
/// mistake on both sides, not an unfiltered page.
#[tokio::test]
async fn a_metadata_listing_refuses_a_filter_value_outside_the_vocabulary() {
    let fx = Fixture::build().await;
    fx.seed_placeholder_book("sha-filter");
    for method in ["library.list_metadata", "library.list_paper_metadata"] {
        let refused = judged(
            fx.call(
                method,
                Surface::Mcp,
                &Caller::mcp(),
                json!({ "confidence_in": ["certain"] }),
            )
            .await,
        );
        assert_eq!(refused.kind, ErrorKind::InvalidParams, "{method}");
        assert!(
            refused.problem.summary.contains("certain"),
            "{method}: {}",
            refused.problem.summary
        );
    }
}
