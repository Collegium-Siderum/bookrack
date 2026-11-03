// SPDX-License-Identifier: Apache-2.0

//! The `rich` preset: a sandbox whose libraries are in every state a
//! command meets, built by the binary under test itself.
//!
//! Five libraries: `alpha` is the default, with two books, a paper, a
//! failed job, an index and a reference book; `beta` is the one the
//! daemon serves first, with two books and an index; `gamma` has a
//! book, no index, and a job left pending behind a paused queue; `delta`
//! has lost its identity manifest and `epsilon` its root, so neither
//! can be mounted. Every input is generated here, ASCII and the same
//! byte for byte on every build, and `inputs/manifest.json` records
//! what each one holds.
//!
//! The build runs inside `new`'s own call, through the primitives that
//! take the call they run in, and ends with two checks. The shape check
//! reads the result through the binary and compares it with what the
//! build set out to make: that proves the preset reached its state. The
//! acceptance check reads it again and compares it with the manifest,
//! which the binary never touches: the books listed, the chunks
//! counted, the sources hashed, every chapter found first under its own
//! topic and nowhere under another library's, the reference entries
//! with their payloads, and the identities the registry and the roots
//! agree on. That proves the content is right. `check` runs either on
//! demand, on the `golden/` tree.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::io;
use std::path::Path;
use std::time::Duration;

use bookrack_config::{LibraryManifest, Registry, load_manifest, parse_registry};
use bookrack_core::Problem;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::exec::{Capture, DaemonStart, ExecOpts};
use super::snapshot::{self, Direction};
use super::stub::{StubStartRecord, vectors_word};
use super::{
    Call, Class, CreateHooks, Expect, InstanceKind, Outcome, STAGE_PATHS, STAGE_PRESET, STAGE_STUB,
    Session, ToolResult, write_atomic,
};
use crate::child::Output;
use crate::embed_stub::{StubSpec, Vectors, topic_marks};
use crate::sandbox::Sandbox;

/// The libraries the preset registers, in registration order.
pub const LIBRARIES: [&str; 5] = ["alpha", "beta", "gamma", "delta", "epsilon"];

/// Each book and the library it is ingested into.
pub const BOOKS: [(&str, &str); 7] = [
    ("b1", "alpha"),
    ("b2", "alpha"),
    ("b3", "beta"),
    ("b4", "beta"),
    ("b5", "gamma"),
    ("b6", "gamma"),
    ("b7", "delta"),
];

/// The schema of `inputs/manifest.json` this module writes and reads.
// setting: internal -- the manifest's schema number, not a value to tune
pub const MANIFEST_SCHEMA: u32 = 1;

/// The width of the stub the preset is built against.
// setting: internal -- the rich preset's embedding width, fixed by its index profile
const RICH_DIM: usize = 1024;

/// How long the build gives its daemon to leave.
// setting: internal -- the rich preset's daemon stop deadline, not a value to tune
const RICH_STOP: Duration = Duration::from_secs(30);

/// The most of a failed step's stderr a problem carries.
// setting: internal -- bounds a failed step's stderr excerpt, not a value to tune
const STDERR_TAIL: usize = 2048;

/// The topic number the paper's abstract carries; the chapters take 1
/// to 21 in [`BOOKS`] order.
// setting: internal -- the paper's topic axis, fixed by the chapter numbering before it
const PAPER_TOPIC: usize = 22;

/// The cosine distance under which a hit is the chapter it was looked
/// for: two inputs on one topic sit about 0.0025 apart under the
/// stub's topic fill, an order of magnitude inside this.
// setting: internal -- the acceptance check's near threshold, fixed by the stub's topic geometry
const HIT_NEAR: f64 = 0.05;

/// The cosine distance over which a hit is unrelated to the topic
/// looked for: inputs on different topics sit about 1.0 apart, an
/// order of magnitude outside this.
// setting: internal -- the acceptance check's far threshold, fixed by the stub's topic geometry
const HIT_FAR: f64 = 0.5;

/// How many hits each acceptance search asks for.
// setting: internal -- the acceptance check's search depth, fixed by its isolation assertion
const SEARCH_TOP_K: usize = 3;

/// The size a chapter's three paragraphs stay under, with the two
/// breaks between them, so the chapter is one chunk at the chunker's
/// target of 1000 characters.
// setting: internal -- a design bound on the generated inputs, not a value to tune
const CHAPTER_MAX_CHARS: usize = 900;

const WORDS: [&str; 16] = [
    "amber", "basalt", "cedar", "dune", "ember", "fjord", "granite", "harbor", "indigo", "juniper",
    "kestrel", "lantern", "meadow", "nectar", "orchid", "pewter",
];

fn word(i: usize) -> &'static str {
    WORDS[i % WORDS.len()]
}

/// The topic number of chapter `c` of the book at `index` in [`BOOKS`]:
/// 1 to 21, each chapter its own.
fn topic_of(index: usize, c: usize) -> usize {
    index * 3 + c
}

/// Chapter `c` of `book`: its marker, key sentence, topic and the three
/// paragraphs the source file carries for it, each one line.
fn chapter(book: &str, library: &str, index: usize, c: usize) -> ChapterInput {
    let marker = format!("sbx-{library}-{book}-c{c}");
    let key = format!(
        "The {} {} keeps the {} of chapter {c} in book {book}.",
        word(index * 3 + c),
        word(index * 5 + c + 7),
        word(index + c * 11)
    );
    let topic = topic_of(index, c);
    let mut paragraphs = vec![format!(
        "The marker {marker} opens this chapter under sbx-topic-{topic}. {key}"
    )];
    for p in 2..=3 {
        let sentences: Vec<String> = (0..4)
            .map(|s| {
                format!(
                    "Paragraph {p} of chapter {c} in book {book} walks past the {} and the {}.",
                    word(index + c + p + s),
                    word(index * 7 + c * 3 + p + s * 5)
                )
            })
            .collect();
        paragraphs.push(sentences.join(" "));
    }
    assert!(
        paragraphs.join("\n\n").len() < CHAPTER_MAX_CHARS,
        "chapter {c} of {book} outgrows one chunk"
    );
    ChapterInput {
        chapter: c,
        marker,
        topic,
        key,
        paragraphs,
    }
}

fn book_text(chapters: &[ChapterInput], book: &str) -> String {
    let mut text = format!("Sandbox Book {book}\n\n");
    for chapter in chapters {
        text.push_str(&format!("Chapter {}\n\n", chapter.chapter));
        for paragraph in &chapter.paragraphs {
            text.push_str(paragraph);
            text.push_str("\n\n");
        }
    }
    text
}

/// What the build leaves each book as: served by a mounted library,
/// pending behind the paused queue, or ingested into a library that
/// cannot be mounted.
fn book_state(book: &str) -> &'static str {
    match book {
        "b6" => "pending",
        "b7" => "unmounted",
        _ => "served",
    }
}

const PAPER_TITLE: &str = "Sandbox Paper One";
const PAPER_ABSTRACT: &str = "This paper sbx-alpha-p1 reports how a sandbox keeps its lanterns \
                              lit under sbx-topic-22.";

fn paper_text() -> String {
    format!(
        "{PAPER_TITLE}\n\nAbstract. {PAPER_ABSTRACT}\n\n\
         The first section describes the harbor and the meadow in plain terms.\n\n\
         The second section closes with the orchid and the pewter bowl.\n"
    )
}

/// The `names` reference book's recipe: a latin headword, a CJK name
/// with one variant, a bracketed country and a note, every part
/// landing in the payload. The full-width semicolon in `head_split_by`
/// is a TOML escape the decoder resolves; the file holds ASCII.
const NAMES_BOOK_TOML: &str = r#"book_slug      = "names"
schema_name    = "name_translation"
schema_version = 1
parser_version = "0.1.0"
authority_rank = 10

[parser]
writes_properties = ["country", "chinese_name", "variants", "bio_annotation"]
stages = [
  "split_pages",
  { stage = "one_block_per_page", lang = "latin" },
  { stage = "walk_anchors",
    anchor = "latin_headword",
    splice_orphans_to_prev_block = false },
  "split_at_first_cjk",
  { stage = "partition_body_around_match",
    pattern = { bracketed_tag = ["angle"] },
    head_payload_key = "country",
    head_split_by = "[;\uFF1B]",
    first_to = "chinese_name",
    rest_to = "variants",
    tail_to = "bio_annotation" },
  { stage = "to_entry_draft",
    key_normalizer = "normalize_latin_key",
    carry_payload_keys = ["country", "chinese_name", "variants", "bio_annotation"] },
]
"#;

/// One entry of the `names` book: the parts its source line is built
/// from, which are also the payload the recipe is expected to write.
struct NameEntry {
    headword: &'static str,
    chinese_name: &'static str,
    variant: &'static str,
    country: &'static str,
    bio: &'static str,
}

const NAMES: [NameEntry; 3] = [
    NameEntry {
        headword: "Ashgrove, Wren",
        chinese_name: "\u{7070}\u{6797}",
        variant: "\u{7070}\u{7433}",
        country: "\u{82F1}",
        bio: "keeper of the sbx-ref-1 lantern",
    },
    NameEntry {
        headword: "Brackenridge, Ivo",
        chinese_name: "\u{8568}\u{6865}",
        variant: "\u{8568}\u{6CB3}",
        country: "\u{6CD5}",
        bio: "mender of the sbx-ref-2 bell",
    },
    NameEntry {
        headword: "Coldwater, Sela",
        chinese_name: "\u{51B7}\u{6C34}",
        variant: "\u{51B7}\u{6CC9}",
        country: "\u{5FB7}",
        bio: "reader of the sbx-ref-3 map",
    },
];

fn names_source() -> String {
    let mut text = String::from("<!-- page 1 (sheet 1) -->\n");
    for entry in &NAMES {
        text.push_str(&format!(
            "{} {};{} \u{3008}{}\u{3009} {}\n",
            entry.headword, entry.chinese_name, entry.variant, entry.country, entry.bio
        ));
    }
    text
}

fn names_entries() -> Vec<EntryInput> {
    NAMES
        .iter()
        .map(|entry| EntryInput {
            headword: entry.headword.to_string(),
            payload: json!({
                "country": entry.country,
                "chinese_name": entry.chinese_name,
                "variants": [entry.variant],
                "bio_annotation": entry.bio,
            }),
        })
        .collect()
}

/// The preset's inputs as `inputs/manifest.json` records them: what
/// the tool wrote, in the shape the acceptance check reads its
/// expectations from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    pub books: Vec<BookInput>,
    pub paper: PaperInput,
    pub reference: ReferenceInput,
    /// Searches that must find nothing of the topic they name: a
    /// library asked for a topic another library's book carries.
    pub isolation: Vec<IsolationCase>,
    pub broken: BrokenInput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BookInput {
    pub file: String,
    pub library: String,
    /// SHA-256 of the file as written.
    pub sha256: String,
    /// `served`, `pending` or `unmounted`; see [`book_state`].
    pub state: String,
    /// Chunks the book ingests into: its first line and one per
    /// chapter. A property of the inputs' design, held by the
    /// [`CHAPTER_MAX_CHARS`] bound.
    pub chunks: u64,
    pub chapters: Vec<ChapterInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChapterInput {
    pub chapter: usize,
    pub marker: String,
    pub topic: usize,
    pub key: String,
    /// The chapter's paragraphs as written, one line each.
    pub paragraphs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaperInput {
    pub file: String,
    pub library: String,
    pub sha256: String,
    pub topic: usize,
    /// The abstract's body, after the `Abstract.` label.
    #[serde(rename = "abstract")]
    pub abstract_text: String,
    pub chunks: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReferenceInput {
    pub dir: String,
    pub library: String,
    pub slug: String,
    pub entries: Vec<EntryInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryInput {
    pub headword: String,
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IsolationCase {
    /// The library searched.
    pub library: String,
    /// The topic searched for.
    pub topic: usize,
    /// The book that carries the topic, in another library.
    pub owner: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokenInput {
    pub file: String,
    pub library: String,
}

/// The preset's inputs, computed: every file's text and the manifest
/// describing it.
fn inputs() -> (Vec<(String, String)>, Manifest) {
    let mut files = Vec::new();
    let mut books = Vec::new();
    for (index, (book, library)) in BOOKS.iter().enumerate() {
        let chapters: Vec<ChapterInput> =
            (1..=3).map(|c| chapter(book, library, index, c)).collect();
        let text = book_text(&chapters, book);
        let file = format!("{book}.txt");
        books.push(BookInput {
            file: file.clone(),
            library: library.to_string(),
            sha256: super::sig::sha256_bytes(text.as_bytes()),
            state: book_state(book).to_string(),
            chunks: 1 + chapters.len() as u64,
            chapters,
        });
        files.push((file, text));
    }
    let paper_text = paper_text();
    let paper = PaperInput {
        file: "p1.txt".to_string(),
        library: "alpha".to_string(),
        sha256: super::sig::sha256_bytes(paper_text.as_bytes()),
        topic: PAPER_TOPIC,
        abstract_text: PAPER_ABSTRACT.to_string(),
        chunks: 1,
    };
    files.push(("p1.txt".to_string(), paper_text));
    files.push((
        "refsrc/names/book.toml".to_string(),
        NAMES_BOOK_TOML.to_string(),
    ));
    files.push(("refsrc/names/source.md".to_string(), names_source()));
    files.push(("broken.epub".to_string(), "not a zip archive".to_string()));

    let first_topic = |owner: &str| {
        books
            .iter()
            .find(|b| b.file == format!("{owner}.txt"))
            .map(|b| b.chapters[0].topic)
            .expect("an isolation case names a book of the preset")
    };
    let isolation = [
        ("alpha", "b3"),
        ("beta", "b5"),
        ("gamma", "b1"),
        ("alpha", "b7"),
        ("beta", "b7"),
        ("gamma", "b7"),
    ]
    .into_iter()
    .map(|(library, owner)| IsolationCase {
        library: library.to_string(),
        topic: first_topic(owner),
        owner: owner.to_string(),
    })
    .collect();

    let manifest = Manifest {
        schema: MANIFEST_SCHEMA,
        books,
        paper,
        reference: ReferenceInput {
            dir: "refsrc/names".to_string(),
            library: "alpha".to_string(),
            slug: "names".to_string(),
            entries: names_entries(),
        },
        isolation,
        broken: BrokenInput {
            file: "broken.epub".to_string(),
            library: "alpha".to_string(),
        },
    };
    (files, manifest)
}

/// The manifest the inputs are written with.
pub fn manifest() -> Manifest {
    inputs().1
}

/// Write the preset's inputs under `host/inputs/`, the same bytes on
/// every call, and `manifest.json` naming each with what it holds.
pub fn write_inputs(host: &Sandbox) -> io::Result<()> {
    let dir = host.inputs();
    let (files, manifest) = inputs();
    for (name, text) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
    }
    let mut bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    bytes.push(b'\n');
    std::fs::write(dir.join("manifest.json"), bytes)
}

/// Read the manifest under `inputs`, refusing a schema this build does
/// not read.
pub fn read_manifest(inputs: &Path) -> io::Result<Manifest> {
    let bytes = std::fs::read(inputs.join("manifest.json"))?;
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(io::Error::other(format!(
            "manifest schema {} is not {MANIFEST_SCHEMA}",
            manifest.schema
        )));
    }
    Ok(manifest)
}

/// One finding of a check: the key it is about and what was read
/// instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShapeFinding {
    pub key: String,
    pub detail: String,
}

/// Each reading of the shape check, by source, parsed.
pub type Readings = BTreeMap<String, Value>;

/// The reads the shape check makes while the daemon serves `beta`, by
/// source name.
fn daemon_reads() -> Vec<(String, Vec<String>)> {
    let mut reads = vec![
        ("status".to_string(), vec!["status", "--json"]),
        ("libraries".to_string(), vec!["libraries", "list", "--json"]),
        ("queue".to_string(), vec!["queue", "list", "--json"]),
    ];
    for lib in ["alpha", "beta", "gamma"] {
        reads.push((
            format!("list.{lib}"),
            vec!["--library", lib, "list", "--json"],
        ));
    }
    let mut reads: Vec<(String, Vec<String>)> = reads
        .into_iter()
        .map(|(k, a)| (k, a.into_iter().map(str::to_string).collect()))
        .collect();
    for lib in ["alpha", "beta", "gamma"] {
        reads.push((
            format!("vectors.{lib}"),
            vec![
                "rpc".to_string(),
                "call".to_string(),
                "library.vectors_status".to_string(),
                format!("{{\"library\":\"{lib}\"}}"),
            ],
        ));
    }
    reads
}

/// The read the shape check makes once the daemon is stopped.
fn local_read() -> (String, Vec<String>) {
    (
        "distill".to_string(),
        ["--json", "--library", "alpha", "distill", "list"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    )
}

/// Judge `readings` against the shape the preset sets out to make. A
/// source that is absent was already reported by the reader.
pub fn check_shape(readings: &Readings) -> Vec<ShapeFinding> {
    let mut findings = Vec::new();
    let mut expect = |key: &str, ok: bool, saw: String| {
        if !ok {
            findings.push(ShapeFinding {
                key: key.to_string(),
                detail: saw,
            });
        }
    };
    let names = |rows: &Value, field: &str| -> Vec<String> {
        let mut out: Vec<String> = rows
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| r[field].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        out
    };
    let served: Vec<String> = vec!["alpha".into(), "beta".into(), "gamma".into()];

    if let Some(status) = readings.get("status") {
        let library = &status["library"];
        expect(
            "status.primary",
            library["name"] == "beta",
            format!("library.name is {}", library["name"]),
        );
        let got = names(&library["served"], "name");
        expect(
            "status.served",
            got == served,
            format!("library.served names {got:?}"),
        );
        let defaults: Vec<String> = library["served"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter(|r| r["default"] == true)
                    .filter_map(|r| r["name"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        expect(
            "status.default",
            defaults == ["alpha"],
            format!("the default served library is {defaults:?}"),
        );
        let unmounted = library["unmounted"].as_object();
        let mut keys: Vec<&String> = unmounted.map(|m| m.keys().collect()).unwrap_or_default();
        keys.sort();
        let reasons = unmounted.is_some_and(|m| {
            m.values().all(|v| {
                v["reason"]["summary"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            })
        });
        expect(
            "status.unmounted",
            keys == ["delta", "epsilon"] && reasons,
            format!("library.unmounted is {}", library["unmounted"]),
        );
    }

    if let Some(libraries) = readings.get("libraries") {
        let rows = libraries.as_array().cloned().unwrap_or_default();
        expect(
            "libraries.count",
            rows.len() == 5,
            format!("{} entries", rows.len()),
        );
        let mut got: Vec<String> = rows
            .iter()
            .filter(|r| r["served"] == true)
            .filter_map(|r| r["name"].as_str().map(str::to_string))
            .collect();
        got.sort();
        expect(
            "libraries.served",
            got == served,
            format!("served entries {got:?}"),
        );
    }

    if let Some(queue) = readings.get("queue") {
        expect(
            "queue.paused",
            queue["paused"] == true,
            format!("paused is {}", queue["paused"]),
        );
        let summary = &queue["summary"];
        let count = |state: &str| summary[state].as_u64().unwrap_or(0);
        let others_zero = summary.as_object().is_some_and(|m| {
            m.iter().all(|(state, n)| {
                ["done", "failed", "pending", "total"].contains(&state.as_str())
                    || n.as_u64() == Some(0)
            })
        });
        expect(
            "queue.summary",
            count("done") == 7 && count("failed") == 1 && count("pending") == 1 && others_zero,
            format!("summary is {summary}"),
        );
        let path_of = |state: &str| -> Vec<String> {
            queue["jobs"]
                .as_array()
                .map(|jobs| {
                    jobs.iter()
                        .filter(|j| j["state"] == state)
                        .filter_map(|j| j["path"].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let failed = path_of("failed");
        expect(
            "queue.failed_path",
            failed.len() == 1 && failed[0].ends_with("broken.epub"),
            format!("failed paths {failed:?}"),
        );
        let pending = path_of("pending");
        expect(
            "queue.pending_path",
            pending.len() == 1 && pending[0].ends_with("b6.txt"),
            format!("pending paths {pending:?}"),
        );
    }

    for (lib, books) in [("alpha", 2), ("beta", 2), ("gamma", 1)] {
        let Some(list) = readings.get(&format!("list.{lib}")) else {
            continue;
        };
        let total = &list["pages"]["book"]["total"];
        expect(
            &format!("list.{lib}.books"),
            total.as_u64() == Some(books),
            format!("pages.book.total is {total}"),
        );
        if lib == "alpha" {
            let papers = &list["pages"]["paper"]["total"];
            expect(
                "list.alpha.papers",
                papers.as_u64() == Some(1),
                format!("pages.paper.total is {papers}"),
            );
        }
        let truncated: Vec<&Value> = ["book", "paper"]
            .iter()
            .map(|kind| &list["pages"][kind]["truncated"])
            .filter(|t| !t.is_null())
            .collect();
        expect(
            &format!("list.{lib}.truncated"),
            !truncated.is_empty() && truncated.iter().all(|t| **t == false),
            format!("truncated flags {truncated:?}"),
        );
    }

    for (lib, indexed) in [("alpha", true), ("beta", true), ("gamma", false)] {
        let Some(vectors) = readings.get(&format!("vectors.{lib}")) else {
            continue;
        };
        let indices = vectors["indices"].as_array().map_or(0, Vec::len);
        let kind = &vectors["ann_config"]["kind"];
        let ok = if indexed {
            indices > 0 && kind == "ivf-flat"
        } else {
            indices == 0 && kind == "brute-force"
        };
        expect(
            &format!("vectors.{lib}"),
            ok,
            format!("{indices} indices, ann_config.kind {kind}"),
        );
    }

    if let Some(distill) = readings.get("distill") {
        let books = distill["books"].as_array().cloned().unwrap_or_default();
        let ok = books.len() == 1 && books[0]["slug"] == "names" && books[0]["entry_count"] == 3;
        expect(
            "distill.names",
            ok,
            format!("books are {}", distill["books"]),
        );
    }
    findings
}

/// One reading of the acceptance check, as it came back: the command's
/// stdout parsed, or why it could not be.
#[derive(Debug, Clone, PartialEq)]
pub enum Reading {
    Parsed(Value),
    /// The command did not exit 0; the class, code and stderr tail.
    Exec(String),
    /// The command exited 0 with stdout that is not JSON.
    Parse(String),
}

/// What a library root under `golden/` holds, read directly.
#[derive(Debug, Clone, PartialEq)]
pub enum RootReading {
    Manifest(LibraryManifest),
    /// The root exists without an identity manifest.
    NoManifest,
    /// The root directory does not exist.
    NoRoot,
    /// The manifest could not be read or parsed.
    Unreadable(String),
}

/// Everything the acceptance check reads, by slot. A slot the reader
/// never reached — a book it could not find in its library's listing —
/// is absent, and the judgement reports it as such.
#[derive(Debug, Clone, Default)]
pub struct GoldenReadings {
    /// `--json --library <lib> list`, by library.
    pub lists: BTreeMap<String, Reading>,
    /// `rpc call library.info`, by library.
    pub infos: BTreeMap<String, Reading>,
    /// `--json --library <lib> show book:<id>`, by book.
    pub book_details: BTreeMap<String, Reading>,
    /// `--json --library alpha papers show <id>`.
    pub paper_detail: Option<Reading>,
    /// `search sbx-topic-<k>` in the book's library, by book and chapter.
    pub hits: BTreeMap<(String, usize), Reading>,
    /// `search sbx-topic-<k>` for the paper's topic in its library.
    pub paper_hit: Option<Reading>,
    /// One search per isolation case, in manifest order.
    pub isolation: Vec<(String, usize, Reading)>,
    /// `rpc call reference.lookup`, by headword.
    pub reference: BTreeMap<String, Reading>,
    /// `--json --library alpha distill list`.
    pub distill_list: Option<Reading>,
    /// `golden/registry.toml`, parsed.
    pub registry: Option<Result<Registry, String>>,
    /// `golden/roots/<lib>`, by library.
    pub manifests: BTreeMap<String, RootReading>,
}

/// Judge `readings` against `manifest`: the pure half of the
/// acceptance check. Every finding is keyed by what it is about; a
/// reading that is not a parsed body is reported under `<key>.exec` or
/// `<key>.parse` and judged no further.
pub fn check_golden(manifest: &Manifest, readings: &GoldenReadings) -> Vec<ShapeFinding> {
    let mut findings: Vec<ShapeFinding> = Vec::new();
    let mut expect = |key: &str, ok: bool, saw: String| {
        if !ok {
            findings.push(ShapeFinding {
                key: key.to_string(),
                detail: saw,
            });
        }
    };
    fn parsed<'a>(
        reading: &'a Reading,
        key: &str,
        expect: &mut dyn FnMut(&str, bool, String),
    ) -> Option<&'a Value> {
        match reading {
            Reading::Parsed(value) => Some(value),
            Reading::Exec(detail) => {
                expect(&format!("{key}.exec"), false, detail.clone());
                None
            }
            Reading::Parse(detail) => {
                expect(&format!("{key}.parse"), false, detail.clone());
                None
            }
        }
    }
    let served_books = |lib: &str| -> Vec<&BookInput> {
        manifest
            .books
            .iter()
            .filter(|b| b.library == lib && b.state == "served")
            .collect()
    };
    let listed = |lib: &str| -> Option<&Value> {
        readings.lists.get(lib).and_then(|r| match r {
            Reading::Parsed(v) => Some(v),
            _ => None,
        })
    };
    let item_id = |lib: &str, kind: &str, file: &str| -> Option<i64> {
        listed(lib)?["items"].as_array()?.iter().find_map(|item| {
            (item["kind"] == kind && item["source_filename"] == file)
                .then(|| item["intake_id"].as_i64())
                .flatten()
        })
    };
    let item_set = |lib: &str| -> BTreeSet<(String, i64)> {
        listed(lib)
            .and_then(|list| list["items"].as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        Some((
                            item["kind"].as_str()?.to_string(),
                            item["intake_id"].as_i64()?,
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let last_crumb = |hit: &Value| -> String {
        hit["breadcrumb"]
            .as_str()
            .and_then(|b| b.rsplit(" \u{203a} ").next())
            .unwrap_or_default()
            .to_string()
    };

    for lib in ["alpha", "beta", "gamma"] {
        let key = format!("books.{lib}");
        if let Some(reading) = readings.lists.get(lib)
            && let Some(list) = parsed(reading, &key, &mut expect)
        {
            let of_kind = |kind: &str| -> BTreeSet<String> {
                list["items"]
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .filter(|i| i["kind"] == kind)
                            .filter_map(|i| i["source_filename"].as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let want: BTreeSet<String> = served_books(lib).iter().map(|b| b.file.clone()).collect();
            let got = of_kind("book");
            expect(
                &key,
                got == want,
                format!("the listed books are {got:?}, the served inputs {want:?}"),
            );
            if lib == manifest.paper.library {
                let papers = of_kind("paper");
                let want: BTreeSet<String> = [manifest.paper.file.clone()].into();
                expect(
                    &key,
                    papers == want,
                    format!("the listed papers are {papers:?}, the inputs {want:?}"),
                );
            }
        }

        let key = format!("chunks.{lib}");
        if let Some(reading) = readings.infos.get(lib)
            && let Some(info) = parsed(reading, &key, &mut expect)
        {
            let want: u64 = served_books(lib).iter().map(|b| b.chunks).sum();
            let name = &info["library_name"];
            let chunks = &info["current_chunks"];
            expect(
                &key,
                name == lib && chunks.as_u64() == Some(want),
                format!("library_name {name}, current_chunks {chunks}, the inputs make {want}"),
            );
            if lib == manifest.paper.library {
                let chunks = &info["papers"]["current_chunks"];
                expect(
                    &key,
                    chunks.as_u64() == Some(manifest.paper.chunks),
                    format!(
                        "papers.current_chunks {chunks}, the inputs make {}",
                        manifest.paper.chunks
                    ),
                );
            }
        }
    }

    for book in manifest.books.iter().filter(|b| b.state == "served") {
        let name = book.file.trim_end_matches(".txt");
        let key = format!("source.{name}");
        match readings.book_details.get(name) {
            None => expect(
                &key,
                false,
                format!("{} is not listed in {}", book.file, book.library),
            ),
            Some(reading) => {
                if let Some(detail) = parsed(reading, &key, &mut expect) {
                    let sha = &detail["source_sha256"];
                    expect(
                        &key,
                        sha == book.sha256.as_str(),
                        format!("source_sha256 {sha}, the input hashes {}", book.sha256),
                    );
                }
            }
        }
        let id = item_id(&book.library, "book", &book.file);
        for chapter in &book.chapters {
            let key = format!("hit.{name}.c{}", chapter.chapter);
            let Some(reading) = readings.hits.get(&(name.to_string(), chapter.chapter)) else {
                expect(&key, false, "the chapter was not searched for".to_string());
                continue;
            };
            let Some(body) = parsed(reading, &key, &mut expect) else {
                continue;
            };
            let Some(first) = body["items"].as_array().and_then(|i| i.first()) else {
                expect(&key, false, "the search found nothing".to_string());
                continue;
            };
            let text = first["text"].as_str().unwrap_or_default();
            let distance = first["distance"].as_f64().unwrap_or(f64::MAX);
            let crumb = last_crumb(first);
            let want_crumb = format!("Chapter {}", chapter.chapter);
            let marks = topic_marks(text);
            let want_marks: BTreeSet<usize> = [chapter.topic].into();
            let complete = text.contains(&chapter.marker)
                && text.contains(&chapter.key)
                && chapter.paragraphs.iter().all(|p| text.contains(p));
            expect(
                &key,
                first["kind"] == "book"
                    && id.is_some()
                    && first["intake_id"].as_i64() == id
                    && crumb == want_crumb
                    && distance < HIT_NEAR
                    && complete
                    && marks == want_marks,
                format!(
                    "first hit: kind {}, intake_id {} (the book is {id:?}), breadcrumb ends \
                     {crumb:?} (wanted {want_crumb:?}), distance {distance}, text complete \
                     {complete}, topic marks {marks:?}",
                    first["kind"], first["intake_id"]
                ),
            );
        }
    }

    {
        let paper = &manifest.paper;
        let key = "paper.p1";
        match &readings.paper_hit {
            None => expect(
                key,
                false,
                "the paper's topic was not searched for".to_string(),
            ),
            Some(reading) => {
                if let Some(body) = parsed(reading, key, &mut expect) {
                    match body["items"].as_array().and_then(|i| i.first()) {
                        None => expect(key, false, "the search found nothing".to_string()),
                        Some(first) => {
                            let text = first["text"].as_str().unwrap_or_default();
                            let distance = first["distance"].as_f64().unwrap_or(f64::MAX);
                            expect(
                                key,
                                first["kind"] == "paper"
                                    && distance < HIT_NEAR
                                    && text.contains(&paper.abstract_text),
                                format!(
                                    "first hit: kind {}, distance {distance}, text {text:?}",
                                    first["kind"]
                                ),
                            );
                        }
                    }
                }
            }
        }
        match &readings.paper_detail {
            None => expect(
                key,
                false,
                format!("{} is not listed in {}", paper.file, paper.library),
            ),
            Some(reading) => {
                if let Some(detail) = parsed(reading, key, &mut expect) {
                    let abstract_text = detail["abstract_text"].as_str().unwrap_or_default();
                    let sha = &detail["source_sha256"];
                    expect(
                        key,
                        abstract_text.contains(&paper.abstract_text)
                            && sha == paper.sha256.as_str(),
                        format!(
                            "abstract_text {abstract_text:?}, source_sha256 {sha}, the input \
                             hashes {}",
                            paper.sha256
                        ),
                    );
                }
            }
        }
    }

    for case in &manifest.isolation {
        let key = format!("isolation.{}", case.library);
        let Some((_, _, reading)) = readings
            .isolation
            .iter()
            .find(|(lib, topic, _)| *lib == case.library && *topic == case.topic)
        else {
            expect(
                &key,
                false,
                format!("topic {} was not searched for", case.topic),
            );
            continue;
        };
        let Some(body) = parsed(reading, &key, &mut expect) else {
            continue;
        };
        let items = body["items"].as_array().cloned().unwrap_or_default();
        let own = item_set(&case.library);
        let mut strangers = Vec::new();
        let mut near = Vec::new();
        let mut leaked = Vec::new();
        for item in &items {
            let id = (
                item["kind"].as_str().unwrap_or_default().to_string(),
                item["intake_id"].as_i64().unwrap_or(-1),
            );
            if !own.contains(&id) {
                strangers.push(id);
            }
            let distance = item["distance"].as_f64().unwrap_or(0.0);
            if distance <= HIT_FAR {
                near.push(distance);
            }
            let marks = topic_marks(item["text"].as_str().unwrap_or_default());
            if marks.contains(&case.topic) {
                leaked.push(marks);
            }
        }
        expect(
            &key,
            items.len() == SEARCH_TOP_K
                && strangers.is_empty()
                && near.is_empty()
                && leaked.is_empty(),
            format!(
                "topic {} of {}: {} hits, from other libraries {strangers:?}, within {HIT_FAR} \
                 {near:?}, carrying the topic {leaked:?}",
                case.topic,
                case.owner,
                items.len()
            ),
        );
    }

    {
        let reference = &manifest.reference;
        let key = format!("reference.{}", reference.slug);
        for entry in &reference.entries {
            let Some(reading) = readings.reference.get(&entry.headword) else {
                expect(
                    &key,
                    false,
                    format!("{:?} was not looked up", entry.headword),
                );
                continue;
            };
            let Some(body) = parsed(reading, &key, &mut expect) else {
                continue;
            };
            let hits = body["hits"].as_array().cloned().unwrap_or_default();
            let one = hits.len() == 1;
            let hit = hits.first().cloned().unwrap_or(Value::Null);
            expect(
                &key,
                one && hit["book_slug"] == reference.slug.as_str()
                    && hit["headword"] == entry.headword.as_str()
                    && hit["payload"] == entry.payload
                    && hit["quality_flags"] == json!([]),
                format!("{:?}: {} hits, first {}", entry.headword, hits.len(), hit),
            );
        }
        match &readings.distill_list {
            None => expect(
                &key,
                false,
                "the reference books were not listed".to_string(),
            ),
            Some(reading) => {
                if let Some(body) = parsed(reading, &key, &mut expect) {
                    let count = body["books"].as_array().and_then(|books| {
                        books
                            .iter()
                            .find(|b| b["slug"] == reference.slug.as_str())
                            .and_then(|b| b["entry_count"].as_u64())
                    });
                    expect(
                        &key,
                        count == Some(reference.entries.len() as u64),
                        format!(
                            "distill list counts {count:?} entries for {}, the inputs hold {}",
                            reference.slug,
                            reference.entries.len()
                        ),
                    );
                }
            }
        }
    }

    for lib in LIBRARIES {
        let key = format!("uuid.{lib}");
        let registry = match &readings.registry {
            Some(Ok(registry)) => registry,
            Some(Err(why)) => {
                expect(&key, false, format!("the registry is unreadable: {why}"));
                continue;
            }
            None => {
                expect(&key, false, "the registry was not read".to_string());
                continue;
            }
        };
        let registered = registry
            .libraries
            .get(lib)
            .and_then(|entry| entry.uuid())
            .map(str::to_string);
        let root = readings.manifests.get(lib);
        match lib {
            "delta" => expect(
                &key,
                matches!(root, Some(RootReading::NoManifest)),
                format!("the root reads as {root:?}, expected a root without a manifest"),
            ),
            "epsilon" => expect(
                &key,
                matches!(root, Some(RootReading::NoRoot)),
                format!("the root reads as {root:?}, expected no root"),
            ),
            _ => {
                let agreed = match root {
                    Some(RootReading::Manifest(m)) => {
                        registered.as_deref() == Some(m.uuid.as_str())
                    }
                    _ => false,
                };
                expect(
                    &key,
                    agreed,
                    format!("the registry records {registered:?}, the root reads as {root:?}"),
                );
            }
        }
    }

    findings
}

/// The tail of `bytes` as text, at most [`STDERR_TAIL`] bytes.
fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_TAIL);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn memory(output: Option<Output>) -> (Vec<u8>, Vec<u8>) {
    match output {
        Some(Output::Memory { stdout, stderr }) => (stdout, stderr),
        _ => (Vec::new(), Vec::new()),
    }
}

/// The command line `args` as a record shows it.
fn shown(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

/// The stub spec the preset is built against and the checks read
/// through.
fn rich_stub_spec() -> StubSpec {
    StubSpec::new(RICH_DIM, Vectors::Topic, None).expect("the preset's width is in range")
}

/// The problem a check raises over the inputs it cannot accept.
fn mismatch(summary: &str, findings: &[ShapeFinding]) -> Problem {
    Problem::new(format!("{summary}: {} findings", findings.len()))
        .detail(
            findings
                .iter()
                .map(|f| format!("{}: {}", f.key, f.detail))
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .hint("See run/preset/ for the build's steps; 'bookrack-sandbox check <dir>' reruns the check.")
}

impl Session {
    /// Run one build step: record it under `run/preset/NN-*`, note it on
    /// the call, and refuse the build when it did not exit `expect`.
    async fn preset_step(
        &self,
        call: &mut Call,
        n: &mut u32,
        args: &[OsString],
        expect: i32,
    ) -> ToolResult<()> {
        *n += 1;
        let step = format!("{n:02}");
        let dir = self.paths().preset_dir();
        let io_err = |e| Outcome::io(STAGE_PRESET, "record a preset step", e);
        write_atomic(
            &dir.join(format!("{step}-cmd")),
            format!("{}\n", shown(args)).as_bytes(),
        )
        .map_err(io_err)?;
        let opts = ExecOpts {
            capture: Capture::Memory,
            ..ExecOpts::default()
        };
        let mut output = None;
        let outcome = self.exec_inner(call, args, &opts, &mut output).await?;
        let (stdout, stderr) = memory(output);
        write_atomic(&dir.join(format!("{step}-stdout")), &stdout).map_err(io_err)?;
        write_atomic(&dir.join(format!("{step}-stderr")), &stderr).map_err(io_err)?;
        let ended = match (outcome.class, outcome.code) {
            (Class::Exited, Some(code)) => code.to_string(),
            (class, _) => serde_json::to_value(class)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
        };
        call.note(format!("step {step}: {}", shown(args)));
        call.note(format!("step {step}: ended {ended}"));
        if (outcome.class, outcome.code) != (Class::Exited, Some(expect)) {
            return Err(Outcome::tool_error(
                STAGE_PRESET,
                Problem::new(format!(
                    "cannot build the rich preset: step {step} exited {ended}, expected {expect}"
                ))
                .detail(tail(&stderr))
                .hint(format!("see run/preset/{step}-*")),
            ));
        }
        Ok(())
    }

    /// The build, inside `new`'s call: inputs, libraries, the daemon's
    /// share of the work, the local reference build, the two broken
    /// libraries, the snapshot, the shape check and the acceptance
    /// check.
    pub(crate) async fn build_rich(
        &mut self,
        call: &mut Call,
        hooks: &mut CreateHooks,
    ) -> ToolResult<()> {
        let host = self.host()?;
        let io_err = |what: &str, e| Outcome::io(STAGE_PRESET, what, e);
        let preset_dir = self.paths().preset_dir();
        self.paths()
            .check(&preset_dir, Expect::DirOrAbsent)
            .map_err(|why| Outcome::refused(STAGE_PATHS, super::problems::path_refused(&why)))?;
        if !preset_dir.exists() {
            std::fs::create_dir(&preset_dir).map_err(|e| io_err("create run/preset", e))?;
        }
        for lib in LIBRARIES {
            host.data_root(lib);
        }
        write_inputs(&host).map_err(|e| io_err("write the preset's inputs", e))?;
        if let Some(on_inputs) = hooks.on_inputs.take() {
            on_inputs(&host.inputs()).map_err(|e| io_err("rewrite the preset's inputs", e))?;
        }
        self.stub_start(rich_stub_spec());

        let mut n = 0;
        let root = |lib: &str| host.path().join("roots").join(lib).display().to_string();
        let input = |name: &str| host.inputs().join(name).display().to_string();
        for lib in LIBRARIES {
            let path = root(lib);
            let args = os(&["libraries", "add", lib, &path, "--yes", "--kind", "test"]);
            self.preset_step(call, &mut n, &args, 0).await?;
        }
        self.preset_step(call, &mut n, &os(&["libraries", "default", "alpha"]), 0)
            .await?;
        for lib in &LIBRARIES[..4] {
            let args = os(&["libraries", "config", lib, "index_profile=sandbox-flat"]);
            self.preset_step(call, &mut n, &args, 0).await?;
        }

        self.rich_daemon_start(call, &mut n).await?;
        let served = self.rich_with_daemon(call, &mut n, &input).await;
        n += 1;
        call.note(format!("step {n:02}: daemon stop"));
        let stopped = self.daemon_stop_inner(call, RICH_STOP).await;
        served?;
        let stopped = stopped?;
        if (stopped.class, stopped.code) != (Class::Exited, Some(0)) {
            return Err(Outcome::tool_error(
                STAGE_PRESET,
                Problem::new(format!(
                    "cannot build the rich preset: step {n:02} stopped the daemon as {:?}, \
                     expected a clean exit",
                    stopped.class
                ))
                .hint("see the last instance under run/daemon/"),
            ));
        }

        let args = os(&[
            "--library",
            "alpha",
            "distill",
            "build",
            &input("refsrc/names"),
        ]);
        self.preset_step(call, &mut n, &args, 0).await?;
        std::fs::remove_file(
            host.path()
                .join("roots")
                .join("delta")
                .join("bookrack-library.toml"),
        )
        .map_err(|e| io_err("remove delta's manifest", e))?;
        std::fs::remove_dir_all(host.path().join("roots").join("epsilon"))
            .map_err(|e| io_err("remove epsilon's root", e))?;
        snapshot::switch(self.paths(), Direction::Snapshot, &mut |_| Ok(()))?;

        let findings = self.shape_inner(call, &mut |_| Ok(())).await?;
        if !findings.is_empty() {
            return Err(Outcome::tool_error(
                STAGE_PRESET,
                Problem::new("cannot build the rich preset: its shape check failed").detail(
                    findings
                        .iter()
                        .map(|f| format!("{}: {}", f.key, f.detail))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            ));
        }
        let findings = self.check_inner(call, false, &mut |_| Ok(())).await?;
        if !findings.is_empty() {
            return Err(Outcome::tool_error(
                STAGE_PRESET,
                mismatch("the rich preset does not match its inputs", &findings),
            ));
        }
        call.note("acceptance: 0 findings");
        Ok(())
    }

    async fn rich_daemon_start(&mut self, call: &mut Call, n: &mut u32) -> ToolResult<()> {
        *n += 1;
        let start = DaemonStart::library("beta");
        let args = start.args();
        let shown_args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        call.note(format!(
            "step {n:02}: daemon start {}",
            shown_args.join(" ")
        ));
        let mut ready = None;
        self.daemon_start_inner(call, &start, &args, &shown_args, &mut ready)
            .await
            .map(|_| ())
    }

    /// The steps that need the daemon: ingests, a paper, a failed job,
    /// the indexes, and a job left pending behind a paused queue.
    async fn rich_with_daemon(
        &self,
        call: &mut Call,
        n: &mut u32,
        input: &dyn Fn(&str) -> String,
    ) -> ToolResult<()> {
        for book in ["b1", "b2"] {
            let args = os(&[
                "--library",
                "alpha",
                "ingest",
                &input(&format!("{book}.txt")),
            ]);
            self.preset_step(call, n, &args, 0).await?;
        }
        let args = os(&["--library", "alpha", "glean", &input("p1.txt")]);
        self.preset_step(call, n, &args, 0).await?;
        let args = os(&["--library", "alpha", "ingest", &input("broken.epub")]);
        self.preset_step(call, n, &args, 5).await?;
        for (book, lib) in [
            ("b3", "beta"),
            ("b4", "beta"),
            ("b5", "gamma"),
            ("b7", "delta"),
        ] {
            let args = os(&["--library", lib, "ingest", &input(&format!("{book}.txt"))]);
            self.preset_step(call, n, &args, 0).await?;
        }
        for lib in ["alpha", "beta"] {
            self.preset_step(call, n, &os(&["--library", lib, "vectors", "rebuild"]), 0)
                .await?;
        }
        let args = os(&["--library", "gamma", "vectors", "drop", "--yes"]);
        self.preset_step(call, n, &args, 0).await?;
        self.preset_step(call, n, &os(&["queue", "pause"]), 0)
            .await?;
        let args = os(&[
            "--library",
            "gamma",
            "ingest",
            &input("b6.txt"),
            "--no-wait",
        ]);
        self.preset_step(call, n, &args, 0).await
    }

    /// Check the shape of `golden/` as the binary reads it: restore,
    /// serve `beta`, read, stop, read the reference books, restore
    /// again. The daemon is stopped and the tree restored on every path.
    /// `on_read` runs before each read with its source's name.
    pub(crate) async fn shape_inner(
        &mut self,
        call: &mut Call,
        on_read: &mut dyn FnMut(&str) -> ToolResult<()>,
    ) -> ToolResult<Vec<ShapeFinding>> {
        snapshot::switch(self.paths(), Direction::Restore, &mut |_| Ok(()))?;
        if self.refresh_stub().await?.is_none() {
            self.stub_start(rich_stub_spec());
        }
        let mut readings = Readings::new();
        let mut findings = Vec::new();
        let start = DaemonStart::library("beta");
        let args = start.args();
        let shown_args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut ready = None;
        let started = self
            .daemon_start_inner(call, &start, &args, &shown_args, &mut ready)
            .await;
        let read = match started {
            Ok(_) => {
                let read = async {
                    for (source, args) in daemon_reads() {
                        on_read(&source)?;
                        self.shape_read(call, &source, &args, &mut readings, &mut findings)
                            .await?;
                    }
                    Ok::<(), Outcome>(())
                }
                .await;
                let stopped = self.daemon_stop_inner(call, RICH_STOP).await.map(|_| ());
                read.and(stopped)
            }
            Err(outcome) => Err(outcome),
        };
        let read = match read {
            Ok(()) => {
                let (source, args) = local_read();
                match on_read(&source) {
                    Ok(()) => {
                        self.shape_read(call, &source, &args, &mut readings, &mut findings)
                            .await
                    }
                    Err(outcome) => Err(outcome),
                }
            }
            Err(outcome) => Err(outcome),
        };
        let restored = snapshot::switch(self.paths(), Direction::Restore, &mut |_| Ok(()));
        read?;
        restored?;
        findings.extend(check_shape(&readings));
        Ok(findings)
    }

    /// One read: a command that did not exit 0 is an `.exec` finding,
    /// one whose stdout is not JSON a `.parse` finding.
    async fn shape_read(
        &self,
        call: &mut Call,
        source: &str,
        args: &[String],
        readings: &mut Readings,
        findings: &mut Vec<ShapeFinding>,
    ) -> ToolResult<()> {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        match self.golden_read(call, &args).await? {
            Reading::Parsed(value) => {
                readings.insert(source.to_string(), value);
            }
            Reading::Exec(detail) => findings.push(ShapeFinding {
                key: format!("{source}.exec"),
                detail,
            }),
            Reading::Parse(detail) => findings.push(ShapeFinding {
                key: format!("{source}.parse"),
                detail,
            }),
        }
        Ok(())
    }

    /// One read of a check, captured in memory and parsed.
    async fn golden_read(&self, call: &mut Call, args: &[OsString]) -> ToolResult<Reading> {
        let opts = ExecOpts {
            capture: Capture::Memory,
            ..ExecOpts::default()
        };
        let mut output = None;
        let outcome = self.exec_inner(call, args, &opts, &mut output).await?;
        let (stdout, stderr) = memory(output);
        Ok(
            if (outcome.class, outcome.code) != (Class::Exited, Some(0)) {
                Reading::Exec(format!(
                    "{:?} {:?}: {}",
                    outcome.class,
                    outcome.code,
                    tail(&stderr)
                ))
            } else {
                match serde_json::from_slice::<Value>(&stdout) {
                    Ok(value) => Reading::Parsed(value),
                    Err(e) => Reading::Parse(format!("{e}: {}", tail(&stdout))),
                }
            },
        )
    }

    /// The stub the acceptance check reads through: a stub this session
    /// started, or the running instance, must be the preset's width in
    /// the topic fill; with neither, one is started in this process.
    async fn ensure_rich_stub(&mut self) -> ToolResult<()> {
        let (dimension, vectors) = if let Some(stub) = self.stubs.last() {
            (
                stub.spec().dimension(),
                vectors_word(stub.spec().vectors()).to_string(),
            )
        } else {
            let unfinished = self
                .unfinished_instances(InstanceKind::Stub)
                .map_err(|e| Outcome::io(STAGE_STUB, "list the stub instances", e))?;
            match unfinished.last() {
                None => {
                    self.stub_start(rich_stub_spec());
                    return Ok(());
                }
                Some(&n) => {
                    let record: StubStartRecord =
                        super::read_json(&self.paths().stub_instance(n).join("start.json"))
                            .map_err(|e| {
                                Outcome::io(STAGE_STUB, "read the stub's start record", e)
                            })?;
                    (record.dimension, record.vectors)
                }
            }
        };
        let want = rich_stub_spec();
        if dimension != want.dimension() || vectors != vectors_word(want.vectors()) {
            return Err(Outcome::refused(
                STAGE_STUB,
                Problem::new(format!(
                    "the acceptance check needs a {}-wide {} stub",
                    want.dimension(),
                    vectors_word(want.vectors())
                ))
                .detail(format!(
                    "The stub in use is {dimension} wide in the {vectors} fill."
                ))
                .hint(
                    "Run 'bookrack-sandbox stub stop <dir>', then 'bookrack-sandbox stub start \
                     <dir>' without --vectors.",
                ),
            ));
        }
        Ok(())
    }

    /// The reads of the acceptance check while the daemon serves
    /// `beta`, in order, each announced to `on_read` first.
    async fn accept_daemon_reads(
        &self,
        call: &mut Call,
        manifest: &Manifest,
        readings: &mut GoldenReadings,
        on_read: &mut dyn FnMut(&str) -> ToolResult<()>,
    ) -> ToolResult<()> {
        let top_k = SEARCH_TOP_K.to_string();
        for lib in ["alpha", "beta", "gamma"] {
            on_read(&format!("list.{lib}"))?;
            let reading = self
                .golden_read(call, &os(&["--json", "--library", lib, "list"]))
                .await?;
            readings.lists.insert(lib.to_string(), reading);
            on_read(&format!("info.{lib}"))?;
            let params = format!("{{\"name\":\"{lib}\"}}");
            let reading = self
                .golden_read(call, &os(&["rpc", "call", "library.info", &params]))
                .await?;
            readings.infos.insert(lib.to_string(), reading);
        }
        let listed_id = |lib: &str, kind: &str, file: &str| -> Option<i64> {
            match readings.lists.get(lib)? {
                Reading::Parsed(list) => list["items"].as_array()?.iter().find_map(|item| {
                    (item["kind"] == kind && item["source_filename"] == file)
                        .then(|| item["intake_id"].as_i64())
                        .flatten()
                }),
                _ => None,
            }
        };
        let mut shows: Vec<(String, Vec<OsString>)> = Vec::new();
        let mut searches: Vec<((String, usize), Vec<OsString>)> = Vec::new();
        for book in manifest.books.iter().filter(|b| b.state == "served") {
            let name = book.file.trim_end_matches(".txt").to_string();
            if let Some(id) = listed_id(&book.library, "book", &book.file) {
                let item = format!("book:{id}");
                shows.push((
                    name.clone(),
                    os(&["--json", "--library", &book.library, "show", &item]),
                ));
            }
            for chapter in &book.chapters {
                let topic = format!("sbx-topic-{}", chapter.topic);
                searches.push((
                    (name.clone(), chapter.chapter),
                    os(&[
                        "--json",
                        "--library",
                        &book.library,
                        "search",
                        &topic,
                        "--top-k",
                        &top_k,
                    ]),
                ));
            }
        }
        let paper_id = listed_id(&manifest.paper.library, "paper", &manifest.paper.file);
        for (name, args) in shows {
            on_read(&format!("source.{name}"))?;
            let reading = self.golden_read(call, &args).await?;
            readings.book_details.insert(name, reading);
        }
        for ((name, chapter), args) in searches {
            on_read(&format!("hit.{name}.c{chapter}"))?;
            let reading = self.golden_read(call, &args).await?;
            readings.hits.insert((name, chapter), reading);
        }
        {
            let paper = &manifest.paper;
            let topic = format!("sbx-topic-{}", paper.topic);
            on_read("paper.p1.hit")?;
            let reading = self
                .golden_read(
                    call,
                    &os(&[
                        "--json",
                        "--library",
                        &paper.library,
                        "search",
                        &topic,
                        "--top-k",
                        &top_k,
                    ]),
                )
                .await?;
            readings.paper_hit = Some(reading);
            if let Some(id) = paper_id {
                on_read("paper.p1.detail")?;
                let id = id.to_string();
                let reading = self
                    .golden_read(
                        call,
                        &os(&["--json", "--library", &paper.library, "papers", "show", &id]),
                    )
                    .await?;
                readings.paper_detail = Some(reading);
            }
        }
        for case in &manifest.isolation {
            on_read(&format!("isolation.{}.{}", case.library, case.topic))?;
            let topic = format!("sbx-topic-{}", case.topic);
            let reading = self
                .golden_read(
                    call,
                    &os(&[
                        "--json",
                        "--library",
                        &case.library,
                        "search",
                        &topic,
                        "--top-k",
                        &top_k,
                    ]),
                )
                .await?;
            readings
                .isolation
                .push((case.library.clone(), case.topic, reading));
        }
        for entry in &manifest.reference.entries {
            on_read(&format!("reference.{}", entry.headword))?;
            let params = json!({
                "library": manifest.reference.library,
                "book": manifest.reference.slug,
                "entry_key": entry.headword,
            })
            .to_string();
            let reading = self
                .golden_read(call, &os(&["rpc", "call", "reference.lookup", &params]))
                .await?;
            readings.reference.insert(entry.headword.clone(), reading);
        }
        Ok(())
    }

    /// The reads of the acceptance check once the daemon is stopped:
    /// the reference-book listing through the binary, and the registry
    /// and the roots' identities straight from `golden/`.
    async fn accept_local_reads(
        &self,
        call: &mut Call,
        manifest: &Manifest,
        readings: &mut GoldenReadings,
        on_read: &mut dyn FnMut(&str) -> ToolResult<()>,
    ) -> ToolResult<()> {
        on_read("distill.list")?;
        let lib = manifest.reference.library.as_str();
        let reading = self
            .golden_read(call, &os(&["--json", "--library", lib, "distill", "list"]))
            .await?;
        readings.distill_list = Some(reading);

        let host = self.host()?;
        let relative = |path: &Path| {
            path.strip_prefix(host.path())
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| path.to_path_buf())
        };
        let golden = self.paths().golden();
        let registry = golden.join(relative(&host.registry_path()));
        readings.registry = Some(
            std::fs::read_to_string(&registry)
                .map_err(|e| e.to_string())
                .and_then(|text| parse_registry(&text).map_err(|e| e.to_string())),
        );
        for lib in LIBRARIES {
            let root = golden.join("roots").join(lib);
            let reading = if !root.is_dir() {
                RootReading::NoRoot
            } else {
                match load_manifest(&root) {
                    Ok(Some(manifest)) => RootReading::Manifest(manifest),
                    Ok(None) => RootReading::NoManifest,
                    Err(e) => RootReading::Unreadable(e.to_string()),
                }
            };
            readings.manifests.insert(lib.to_string(), reading);
        }
        Ok(())
    }

    /// Check `golden/` against its inputs, and against the preset's
    /// shape when `shape` is set, in one stub and one daemon cycle:
    /// restore, make sure of the stub, serve `beta`, read, stop, read
    /// what needs no daemon, restore again, judge. The daemon is stopped
    /// and the tree restored on every path. `on_read` runs before each
    /// read with its name.
    pub(crate) async fn check_inner(
        &mut self,
        call: &mut Call,
        shape: bool,
        on_read: &mut dyn FnMut(&str) -> ToolResult<()>,
    ) -> ToolResult<Vec<ShapeFinding>> {
        let manifest = read_manifest(&self.paths().golden().join("inputs"))
            .map_err(|e| Outcome::io(STAGE_PRESET, "read the inputs' manifest", e))?;
        snapshot::switch(self.paths(), Direction::Restore, &mut |_| Ok(()))?;
        self.ensure_rich_stub().await?;
        let mut shape_readings = Readings::new();
        let mut findings = Vec::new();
        let mut readings = GoldenReadings::default();
        let start = DaemonStart::library("beta");
        let args = start.args();
        let shown_args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut ready = None;
        let started = self
            .daemon_start_inner(call, &start, &args, &shown_args, &mut ready)
            .await;
        let read = match started {
            Ok(_) => {
                let read = async {
                    if shape {
                        for (source, args) in daemon_reads() {
                            on_read(&source)?;
                            self.shape_read(
                                call,
                                &source,
                                &args,
                                &mut shape_readings,
                                &mut findings,
                            )
                            .await?;
                        }
                    }
                    self.accept_daemon_reads(call, &manifest, &mut readings, on_read)
                        .await
                }
                .await;
                let stopped = self.daemon_stop_inner(call, RICH_STOP).await.map(|_| ());
                read.and(stopped)
            }
            Err(outcome) => Err(outcome),
        };
        let read = match read {
            Ok(()) => {
                let local = async {
                    if shape {
                        let (source, args) = local_read();
                        on_read(&source)?;
                        self.shape_read(call, &source, &args, &mut shape_readings, &mut findings)
                            .await?;
                    }
                    self.accept_local_reads(call, &manifest, &mut readings, on_read)
                        .await
                };
                local.await
            }
            Err(outcome) => Err(outcome),
        };
        let restored = snapshot::switch(self.paths(), Direction::Restore, &mut |_| Ok(()));
        read?;
        restored?;
        if shape {
            findings.extend(check_shape(&shape_readings));
        }
        findings.extend(check_golden(&manifest, &readings));
        Ok(findings)
    }

    /// Check the shape of the sandbox's golden tree, as the `rich`
    /// preset's build does. Refused while a daemon runs.
    pub async fn check_shape(&mut self) -> ToolResult<Vec<ShapeFinding>> {
        self.check_shape_with(&mut |_| Ok(())).await
    }

    /// [`check_shape`](Self::check_shape) with `on_read` run before each
    /// read, for a test to fail one part way.
    #[doc(hidden)]
    pub async fn check_shape_with(
        &mut self,
        on_read: &mut dyn FnMut(&str) -> ToolResult<()>,
    ) -> ToolResult<Vec<ShapeFinding>> {
        let mut call = self.begin("check", Vec::new(), true).await?;
        let result = match self.ensure_no_writer() {
            Ok(()) => self.shape_inner(&mut call, on_read).await,
            Err(outcome) => Err(outcome),
        };
        match result {
            Ok(findings) => {
                for finding in &findings {
                    call.note(format!("{}: {}", finding.key, finding.detail));
                }
                self.end(call, Ok(Outcome::ok()))?;
                Ok(findings)
            }
            Err(outcome) => Err(self.end(call, Err(outcome)).unwrap_err()),
        }
    }

    /// `check`: the acceptance check of `golden/` against its inputs,
    /// and the shape check too when `shape` is set, in one daemon
    /// cycle. Refused while a daemon runs, and on a preset the check
    /// does not cover: `rich` takes both, `release` takes the
    /// acceptance check alone, `bare` neither. The findings come back;
    /// the call's record is `refused` when there are any, `ok` when
    /// there are none.
    pub async fn check(&mut self, shape: bool) -> ToolResult<Vec<ShapeFinding>> {
        self.check_with(shape, &mut |_| Ok(())).await
    }

    /// [`check`](Self::check) with `on_read` run before each read, for
    /// a test to fail one part way.
    #[doc(hidden)]
    pub async fn check_with(
        &mut self,
        shape: bool,
        on_read: &mut dyn FnMut(&str) -> ToolResult<()>,
    ) -> ToolResult<Vec<ShapeFinding>> {
        let mut args = Vec::new();
        if shape {
            args.push("--shape".to_string());
        }
        let mut call = self.begin("check", args, true).await?;
        let result = match self.ensure_no_writer().and_then(|()| self.marker_ready()) {
            Ok(marker) => {
                let covered = match marker.preset.as_str() {
                    "rich" => true,
                    "release" => !shape,
                    _ => false,
                };
                if covered {
                    self.check_inner(&mut call, shape, on_read).await
                } else {
                    Err(Outcome::refused(
                        STAGE_PRESET,
                        Problem::new(format!(
                            "the check does not cover a {} sandbox{}",
                            marker.preset,
                            if shape { " with --shape" } else { "" }
                        ))
                        .detail(
                            "The acceptance check reads the rich preset's inputs; the shape \
                             check reads the state the rich build leaves.",
                        ),
                    ))
                }
            }
            Err(outcome) => Err(outcome),
        };
        match result {
            Ok(findings) => {
                for finding in &findings {
                    call.note(format!("{}: {}", finding.key, finding.detail));
                }
                let outcome = if findings.is_empty() {
                    Outcome::ok()
                } else {
                    Outcome::refused(
                        STAGE_PRESET,
                        mismatch("the golden does not match its inputs", &findings),
                    )
                };
                self.end(call, Ok(outcome))?;
                Ok(findings)
            }
            Err(outcome) => Err(self.end(call, Err(outcome)).unwrap_err()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Readings in the shapes the binary under test gives, trimmed to
    /// the fields the check reads, for a preset that reached its state.
    fn frozen() -> Readings {
        let served = |name: &str, default: bool, primary: bool| {
            json!({ "name": name, "data_dir": format!("/h/roots/{name}"),
                    "default": default, "primary": primary })
        };
        let reason = |summary: &str| {
            json!({ "data_dir": "/h/roots/x", "reason": { "summary": summary,
                    "detail": "d", "hint": "h", "retryable": false } })
        };
        let entry = |name: &str, served: bool| {
            json!({ "name": name, "data_dir": format!("/h/roots/{name}"), "kind": "test",
                    "is_default": name == "alpha", "served": served })
        };
        let job = |path: &str, state: &str| json!({ "path": path, "state": state, "kind": "book" });
        let list = |books: u64, papers: u64| {
            json!({ "items": [], "pages": {
                "book": { "total": books, "truncated": false },
                "paper": { "total": papers, "truncated": false } } })
        };
        let vectors = |kind: &str, indexed: bool| {
            let indices = if indexed {
                json!([{ "name": "vector_idx", "stats": { "index_type": "IvfFlat" } }])
            } else {
                json!([])
            };
            json!({ "ann_config": { "kind": kind }, "indices": indices })
        };
        let mut jobs: Vec<Value> = ["b1", "b2", "p1", "b3", "b4", "b5", "b7"]
            .iter()
            .map(|b| job(&format!("/h/inputs/{b}.txt"), "done"))
            .collect();
        jobs.push(job("/h/inputs/broken.epub", "failed"));
        jobs.push(job("/h/inputs/b6.txt", "pending"));
        let mut readings = Readings::new();
        readings.insert(
            "status".into(),
            json!({ "library": {
                "name": "beta",
                "served": [served("alpha", true, false), served("beta", false, true),
                           served("gamma", false, false)],
                "unmounted": { "delta": reason("no manifest"), "epsilon": reason("no root") },
            } }),
        );
        readings.insert(
            "libraries".into(),
            json!([
                entry("alpha", true),
                entry("beta", true),
                entry("delta", false),
                entry("epsilon", false),
                entry("gamma", true)
            ]),
        );
        readings.insert(
            "queue".into(),
            json!({ "paused": true, "jobs": jobs, "summary": {
                "cancelled": 0, "done": 7, "failed": 1, "needs_ocr": 0, "pending": 1,
                "running": 0, "skipped_duplicate": 0, "total": 9 } }),
        );
        readings.insert("list.alpha".into(), list(2, 1));
        readings.insert("list.beta".into(), list(2, 0));
        readings.insert("list.gamma".into(), list(1, 0));
        readings.insert("vectors.alpha".into(), vectors("ivf-flat", true));
        readings.insert("vectors.beta".into(), vectors("ivf-flat", true));
        readings.insert("vectors.gamma".into(), vectors("brute-force", false));
        readings.insert(
            "distill".into(),
            json!({ "books": [{ "slug": "names", "title": "names", "entry_count": 3,
                                "authority_rank": 10 }] }),
        );
        readings
    }

    fn keys(readings: &Readings) -> Vec<String> {
        check_shape(readings).into_iter().map(|f| f.key).collect()
    }

    #[test]
    fn a_preset_in_its_state_has_no_findings() {
        assert_eq!(check_shape(&frozen()), Vec::new());
    }

    /// Each key is held by its own field: changing that field alone is
    /// found under that key alone.
    #[test]
    fn each_key_finds_only_its_own_field() {
        type Change = fn(&mut Readings);
        let cases: Vec<(&str, Change)> = vec![
            ("status.primary", |r| {
                r.get_mut("status").unwrap()["library"]["name"] = json!("alpha")
            }),
            ("status.served", |r| {
                r.get_mut("status").unwrap()["library"]["served"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }),
            ("status.default", |r| {
                let served = &mut r.get_mut("status").unwrap()["library"]["served"];
                served[0]["default"] = json!(false);
                served[1]["default"] = json!(true);
            }),
            ("status.unmounted", |r| {
                r.get_mut("status").unwrap()["library"]["unmounted"]
                    .as_object_mut()
                    .unwrap()
                    .remove("epsilon");
            }),
            ("libraries.count", |r| {
                r.get_mut("libraries")
                    .unwrap()
                    .as_array_mut()
                    .unwrap()
                    .remove(2);
            }),
            ("libraries.served", |r| {
                r.get_mut("libraries").unwrap()[4]["served"] = json!(false)
            }),
            ("queue.paused", |r| {
                r.get_mut("queue").unwrap()["paused"] = json!(false)
            }),
            ("queue.summary", |r| {
                r.get_mut("queue").unwrap()["summary"]["done"] = json!(6)
            }),
            ("queue.failed_path", |r| {
                r.get_mut("queue").unwrap()["jobs"][7]["path"] = json!("/h/inputs/renamed.epub");
            }),
            ("queue.pending_path", |r| {
                r.get_mut("queue").unwrap()["jobs"][8]["path"] = json!("/h/inputs/b7.txt");
            }),
            ("list.alpha.books", |r| {
                r.get_mut("list.alpha").unwrap()["pages"]["book"]["total"] = json!(3);
            }),
            ("list.beta.books", |r| {
                r.get_mut("list.beta").unwrap()["pages"]["book"]["total"] = json!(1);
            }),
            ("list.gamma.books", |r| {
                r.get_mut("list.gamma").unwrap()["pages"]["book"]["total"] = json!(2);
            }),
            ("list.alpha.papers", |r| {
                r.get_mut("list.alpha").unwrap()["pages"]["paper"]["total"] = json!(0);
            }),
            ("list.alpha.truncated", |r| {
                r.get_mut("list.alpha").unwrap()["pages"]["book"]["truncated"] = json!(true);
            }),
            ("list.beta.truncated", |r| {
                r.get_mut("list.beta").unwrap()["pages"]["paper"]["truncated"] = json!(true);
            }),
            ("list.gamma.truncated", |r| {
                r.get_mut("list.gamma").unwrap()["pages"]["book"]["truncated"] = json!(true);
            }),
            ("vectors.alpha", |r| {
                r.get_mut("vectors.alpha").unwrap()["indices"] = json!([])
            }),
            ("vectors.beta", |r| {
                r.get_mut("vectors.beta").unwrap()["ann_config"]["kind"] = json!("IvfFlat");
            }),
            ("vectors.gamma", |r| {
                r.get_mut("vectors.gamma").unwrap()["ann_config"]["kind"] = json!("ivf-flat");
            }),
            ("distill.names", |r| {
                r.get_mut("distill").unwrap()["books"][0]["entry_count"] = json!(2);
            }),
        ];
        for (key, change) in cases {
            let mut readings = frozen();
            change(&mut readings);
            assert_eq!(keys(&readings), [key], "changing the field {key} reads");
        }
    }

    /// An absent source is left to the reader's own finding.
    #[test]
    fn an_absent_source_is_not_judged_again() {
        let mut readings = frozen();
        readings.remove("list.gamma");
        assert_eq!(check_shape(&readings), Vec::new());
    }

    /// The inputs are the same bytes on every write, and plain ASCII
    /// outside the reference book's source.
    #[test]
    fn the_inputs_are_deterministic_ascii() {
        let a = tempfile::tempdir().expect("tempdir");
        let b = tempfile::tempdir().expect("tempdir");
        let (ha, hb) = (
            Sandbox::create_at(&a.path().join("h")).expect("host"),
            Sandbox::create_at(&b.path().join("h")).expect("host"),
        );
        write_inputs(&ha).expect("write");
        write_inputs(&hb).expect("write");
        let sa = super::super::sig::signature(&ha.inputs()).expect("sign");
        let sb = super::super::sig::signature(&hb.inputs()).expect("sign");
        assert_eq!(sa, sb);
        assert!(sa.len() >= 12, "{sa:?}");
        for (book, _) in BOOKS {
            let text = std::fs::read(ha.inputs().join(format!("{book}.txt"))).expect("read");
            assert!(text.is_ascii(), "{book} is not ASCII");
        }
        let manifest = read_manifest(&ha.inputs()).expect("manifest");
        assert_eq!(manifest, super::manifest());
        let text = std::fs::read_to_string(ha.inputs().join("b3.txt")).expect("b3");
        let chapter = &manifest.books[2].chapters[1];
        assert!(text.contains(&chapter.marker), "{chapter:?}");
        assert!(text.contains(&chapter.key), "{chapter:?}");
        for paragraph in &chapter.paragraphs {
            assert!(text.contains(paragraph), "{paragraph}");
        }
        assert_eq!(
            super::super::sig::sha256_bytes(text.as_bytes()),
            manifest.books[2].sha256
        );
    }

    /// Every chapter's text stays small enough to be one chunk, which
    /// is what the manifest's `chunks` counts rest on; each chapter and
    /// the paper carry their own topic, 1 to 22.
    #[test]
    fn every_chapter_is_one_chunk_under_its_own_topic() {
        let manifest = manifest();
        let mut topics = BTreeSet::new();
        for book in &manifest.books {
            assert_eq!(book.chunks, 4, "{}", book.file);
            for chapter in &book.chapters {
                let text = chapter.paragraphs.join("\n\n");
                assert!(
                    text.len() < CHAPTER_MAX_CHARS,
                    "{} chapter {} runs to {} characters",
                    book.file,
                    chapter.chapter,
                    text.len()
                );
                assert_eq!(topic_marks(&text), [chapter.topic].into());
                assert!(
                    topics.insert(chapter.topic),
                    "topic {} repeats",
                    chapter.topic
                );
            }
        }
        assert!(topics.insert(manifest.paper.topic));
        assert_eq!(topics, (1..=22).collect());
        assert_eq!(topic_marks(&paper_text()), [manifest.paper.topic].into());
    }

    /// Readings in the shapes the binary under test gives, for a golden
    /// tree that matches the manifest.
    fn golden() -> (Manifest, GoldenReadings) {
        let manifest = manifest();
        let mut readings = GoldenReadings::default();
        let mut ids: BTreeMap<String, i64> = BTreeMap::new();
        for lib in ["alpha", "beta", "gamma"] {
            let mut items = Vec::new();
            let mut next = 1;
            for book in manifest.books.iter().filter(|b| b.library == lib) {
                if book.state != "served" {
                    continue;
                }
                ids.insert(book.file.clone(), next);
                items.push(json!({ "kind": "book", "intake_id": next,
                                   "source_filename": book.file, "id": format!("book:{next}") }));
                next += 1;
            }
            if manifest.paper.library == lib {
                items.push(json!({ "kind": "paper", "intake_id": 1,
                                   "source_filename": manifest.paper.file, "id": "paper:1" }));
            }
            readings.lists.insert(
                lib.to_string(),
                Reading::Parsed(json!({ "items": items, "library": lib, "pages": {} })),
            );
            let chunks: u64 = manifest
                .books
                .iter()
                .filter(|b| b.library == lib && b.state == "served")
                .map(|b| b.chunks)
                .sum();
            let mut info = json!({ "library_name": lib, "current_chunks": chunks });
            if manifest.paper.library == lib {
                info["papers"] = json!({ "current_chunks": manifest.paper.chunks });
            }
            readings
                .infos
                .insert(lib.to_string(), Reading::Parsed(info));
        }
        let hit = |kind: &str, id: i64, crumb: &str, distance: f64, text: &str| {
            json!({ "kind": kind, "intake_id": id, "breadcrumb": crumb,
                    "distance": distance, "text": text })
        };
        for book in manifest.books.iter().filter(|b| b.state == "served") {
            let name = book.file.trim_end_matches(".txt").to_string();
            let id = ids[&book.file];
            readings.book_details.insert(
                name.clone(),
                Reading::Parsed(json!({ "intake_id": id, "source_sha256": book.sha256 })),
            );
            for chapter in &book.chapters {
                let text = chapter.paragraphs.join("\n\n");
                let crumb = format!("{name} \u{203a} Chapter {}", chapter.chapter);
                let far = book
                    .chapters
                    .iter()
                    .filter(|c| c.chapter != chapter.chapter)
                    .map(|c| {
                        hit(
                            "book",
                            id,
                            &format!("{name} \u{203a} Chapter {}", c.chapter),
                            0.99,
                            &c.paragraphs.join("\n\n"),
                        )
                    });
                let mut items = vec![hit("book", id, &crumb, 0.0025, &text)];
                items.extend(far);
                readings.hits.insert(
                    (name.clone(), chapter.chapter),
                    Reading::Parsed(json!({ "items": items })),
                );
            }
        }
        let abstract_text = format!(". {}", manifest.paper.abstract_text);
        readings.paper_hit = Some(Reading::Parsed(json!({ "items": [
            hit("paper", 1, "p1", 0.002, &abstract_text),
            hit("book", 1, "b1 \u{203a} Chapter 1", 0.99, "elsewhere"),
        ] })));
        readings.paper_detail = Some(Reading::Parsed(json!({
            "intake_id": 1, "abstract_text": abstract_text,
            "source_sha256": manifest.paper.sha256 })));
        for case in &manifest.isolation {
            let own: Vec<&BookInput> = manifest
                .books
                .iter()
                .filter(|b| b.library == case.library && b.state == "served")
                .collect();
            let items: Vec<Value> = (0..SEARCH_TOP_K)
                .map(|i| {
                    let book = own[i % own.len()];
                    let chapter = &book.chapters[i % 3];
                    hit(
                        "book",
                        ids[&book.file],
                        &format!("x \u{203a} Chapter {}", chapter.chapter),
                        0.99 + 0.001 * i as f64,
                        &chapter.paragraphs.join("\n\n"),
                    )
                })
                .collect();
            readings.isolation.push((
                case.library.clone(),
                case.topic,
                Reading::Parsed(json!({ "items": items })),
            ));
        }
        for entry in &manifest.reference.entries {
            readings.reference.insert(
                entry.headword.clone(),
                Reading::Parsed(json!({
                    "entry_key": entry.headword,
                    "hits": [{ "book_slug": "names", "entry_key": "k",
                               "headword": entry.headword, "payload": entry.payload,
                               "quality_flags": [] }],
                    "primary_by_authority": 0, "redirect_followed": null })),
            );
        }
        readings.distill_list = Some(Reading::Parsed(json!({ "books": [
            { "slug": "names", "entry_count": 3, "authority_rank": 10 } ] })));
        let mut registry = String::from("default = \"alpha\"\n\n");
        for lib in LIBRARIES {
            registry.push_str(&format!(
                "[libraries.{lib}]\ndata_dir = \"/h/roots/{lib}\"\nuuid = \"uuid-{lib}\"\n\n"
            ));
        }
        readings.registry = Some(parse_registry(&registry).map_err(|e| e.to_string()));
        for lib in ["alpha", "beta", "gamma"] {
            let manifest: LibraryManifest = toml_manifest(lib);
            readings
                .manifests
                .insert(lib.to_string(), RootReading::Manifest(manifest));
        }
        readings
            .manifests
            .insert("delta".to_string(), RootReading::NoManifest);
        readings
            .manifests
            .insert("epsilon".to_string(), RootReading::NoRoot);
        (manifest, readings)
    }

    /// An identity manifest for `lib` with the uuid the frozen registry
    /// records.
    fn toml_manifest(lib: &str) -> LibraryManifest {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join(bookrack_config::MANIFEST_FILENAME),
            format!(
                "format = \"{}\"\nformat_version = {}\nuuid = \"uuid-{lib}\"\nname = \"{lib}\"\n\
                 kind = \"test\"\ncreated_at = \"2026-01-01T00:00:00Z\"\n",
                bookrack_config::MANIFEST_FORMAT,
                bookrack_config::MANIFEST_SCHEMA_VERSION
            ),
        )
        .expect("write manifest");
        load_manifest(dir.path())
            .expect("parse manifest")
            .expect("a manifest")
    }

    fn golden_keys(manifest: &Manifest, readings: &GoldenReadings) -> BTreeSet<String> {
        check_golden(manifest, readings)
            .into_iter()
            .map(|f| f.key)
            .collect()
    }

    fn set(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| k.to_string()).collect()
    }

    fn first_hit(reading: &mut Reading) -> &mut Value {
        match reading {
            Reading::Parsed(body) => &mut body["items"][0],
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_golden_that_matches_its_inputs_has_no_findings() {
        let (manifest, readings) = golden();
        assert_eq!(check_golden(&manifest, &readings), Vec::new());
    }

    /// Each key is held by the readings it names: changing one field
    /// alone is found under that key alone.
    #[test]
    fn each_golden_key_finds_only_its_own_field() {
        type Change = fn(&mut GoldenReadings);
        let cases: Vec<(&str, Change)> = vec![
            ("books.beta", |r| {
                if let Reading::Parsed(list) = r.lists.get_mut("beta").unwrap() {
                    list["items"].as_array_mut().unwrap().push(json!({
                        "kind": "book", "intake_id": 99, "source_filename": "b9.txt" }));
                }
            }),
            ("books.alpha", |r| {
                if let Reading::Parsed(list) = r.lists.get_mut("alpha").unwrap() {
                    list["items"][2]["source_filename"] = json!("p2.txt");
                }
            }),
            ("chunks.gamma", |r| {
                if let Reading::Parsed(info) = r.infos.get_mut("gamma").unwrap() {
                    info["current_chunks"] = json!(8);
                }
            }),
            ("chunks.alpha", |r| {
                if let Reading::Parsed(info) = r.infos.get_mut("alpha").unwrap() {
                    info["papers"]["current_chunks"] = json!(2);
                }
            }),
            ("source.b4", |r| {
                if let Reading::Parsed(d) = r.book_details.get_mut("b4").unwrap() {
                    d["source_sha256"] = json!("0000");
                }
            }),
            ("hit.b2.c3", |r| {
                first_hit(r.hits.get_mut(&("b2".to_string(), 3)).unwrap())["breadcrumb"] =
                    json!("b2 \u{203a} Chapter 2");
            }),
            ("paper.p1", |r| {
                first_hit(r.paper_hit.as_mut().unwrap())["kind"] = json!("book");
            }),
            ("isolation.gamma", |r| {
                let (_, _, reading) = r
                    .isolation
                    .iter_mut()
                    .find(|(lib, _, _)| lib == "gamma")
                    .unwrap();
                first_hit(reading)["distance"] = json!(0.3);
            }),
            ("reference.names", |r| {
                if let Reading::Parsed(body) = r.reference.get_mut("Coldwater, Sela").unwrap() {
                    body["hits"][0]["payload"]["country"] = json!("x");
                }
            }),
            ("uuid.beta", |r| {
                r.manifests
                    .insert("beta".to_string(), RootReading::NoManifest);
            }),
        ];
        for (key, change) in cases {
            let (manifest, mut readings) = golden();
            change(&mut readings);
            assert_eq!(
                golden_keys(&manifest, &readings),
                set(&[key]),
                "changing the field {key} reads"
            );
        }
    }

    /// What only the acceptance check catches: a chapter found first
    /// but not whole, a hit too far away, a hit carrying a second topic.
    #[test]
    fn a_hit_is_judged_on_its_text_and_distance() {
        type Change = fn(&mut Value);
        let cases: Vec<(&str, Change)> = vec![
            ("a paragraph missing", |hit| {
                let text = hit["text"].as_str().unwrap().to_string();
                let kept: Vec<&str> = text.split("\n\n").take(2).collect();
                hit["text"] = json!(kept.join("\n\n"));
            }),
            ("a distance of 0.3", |hit| hit["distance"] = json!(0.3)),
            ("a second topic in the text", |hit| {
                let text = hit["text"].as_str().unwrap().to_string();
                hit["text"] = json!(format!("{text} and sbx-topic-12"));
            }),
        ];
        for (case, change) in cases {
            let (manifest, mut readings) = golden();
            change(first_hit(
                readings.hits.get_mut(&("b1".to_string(), 1)).unwrap(),
            ));
            assert_eq!(
                golden_keys(&manifest, &readings),
                set(&["hit.b1.c1"]),
                "{case}"
            );
        }
    }

    /// Isolation is judged on every hit: none may be absent, from
    /// another library, near, or carrying the topic — and a topic
    /// number is read whole, so 10 is not 1.
    #[test]
    fn isolation_is_judged_on_each_hit() {
        type Change = fn(&mut Reading);
        let cases: Vec<(&str, Change, bool)> = vec![
            (
                "no hits",
                |r| {
                    if let Reading::Parsed(body) = r {
                        body["items"] = json!([]);
                    }
                },
                true,
            ),
            (
                "a hit from another library's book",
                |r| first_hit(r)["intake_id"] = json!(99),
                true,
            ),
            (
                "a hit carrying topic 10 when 1 is searched",
                |r| {
                    let text = first_hit(r)["text"].as_str().unwrap().to_string();
                    first_hit(r)["text"] = json!(format!("{text} sbx-topic-10"));
                },
                false,
            ),
            (
                "a hit carrying the searched topic",
                |r| {
                    let text = first_hit(r)["text"].as_str().unwrap().to_string();
                    first_hit(r)["text"] = json!(format!("{text} sbx-topic-1"));
                },
                true,
            ),
        ];
        for (case, change, found) in cases {
            let (manifest, mut readings) = golden();
            // gamma searches b1's first chapter, topic 1.
            let (_, topic, reading) = readings
                .isolation
                .iter_mut()
                .find(|(lib, _, _)| lib == "gamma")
                .unwrap();
            assert_eq!(*topic, 1);
            change(reading);
            let want = if found {
                set(&["isolation.gamma"])
            } else {
                set(&[])
            };
            assert_eq!(golden_keys(&manifest, &readings), want, "{case}");
        }
    }

    /// A count that happens to match is not enough: the info must be
    /// the library's own.
    #[test]
    fn a_library_info_from_another_library_is_found() {
        let (manifest, mut readings) = golden();
        if let Reading::Parsed(info) = readings.infos.get_mut("alpha").unwrap() {
            info["library_name"] = json!("beta");
        }
        assert_eq!(golden_keys(&manifest, &readings), set(&["chunks.alpha"]));
    }

    /// An entry the store does not answer is found under the reference
    /// key.
    #[test]
    fn a_reference_entry_without_a_hit_is_found() {
        let (manifest, mut readings) = golden();
        if let Reading::Parsed(body) = readings.reference.get_mut("Ashgrove, Wren").unwrap() {
            body["hits"] = json!([]);
        }
        assert_eq!(golden_keys(&manifest, &readings), set(&["reference.names"]));
    }

    /// A reading that is not a body is reported as how it failed, under
    /// the key it was read for.
    #[test]
    fn a_failed_or_unparsed_reading_is_reported_as_such() {
        let (manifest, mut readings) = golden();
        readings.lists.insert(
            "gamma".to_string(),
            Reading::Exec("exited 2: no such library".to_string()),
        );
        readings.infos.insert(
            "beta".to_string(),
            Reading::Parse("expected value at line 1".to_string()),
        );
        let found = golden_keys(&manifest, &readings);
        assert!(found.contains("books.gamma.exec"), "{found:?}");
        assert!(found.contains("chunks.beta.parse"), "{found:?}");
        assert!(!found.contains("books.gamma"), "{found:?}");
        assert!(!found.contains("chunks.beta"), "{found:?}");
    }
}
