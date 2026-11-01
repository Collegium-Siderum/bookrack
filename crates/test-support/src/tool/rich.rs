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
//! byte for byte on every build.
//!
//! The build runs inside `new`'s own call, through the primitives that
//! take the call they run in, and ends with a shape check: the readings
//! the binary gives of the result must match what the build set out to
//! make. That proves the preset reached its state, not that the content
//! is right.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::time::Duration;

use bookrack_core::Problem;
use serde_json::{Value, json};

use super::exec::{Capture, DaemonStart, ExecOpts};
use super::snapshot::{self, Direction};
use super::{
    Call, Class, Expect, Outcome, STAGE_PATHS, STAGE_PRESET, Session, ToolResult, write_atomic,
};
use crate::child::Output;
use crate::embed_stub::{StubSpec, Vectors};
use crate::fixtures::{DISTILL_TINY_BOOK_TOML, DISTILL_TINY_SOURCE};
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

/// The width of the stub the preset is built against.
// setting: internal -- the rich preset's embedding width, fixed by its index profile
const RICH_DIM: usize = 1024;

/// How long the build gives its daemon to leave.
// setting: internal -- the rich preset's daemon stop deadline, not a value to tune
const RICH_STOP: Duration = Duration::from_secs(30);

/// The most of a failed step's stderr a problem carries.
// setting: internal -- bounds a failed step's stderr excerpt, not a value to tune
const STDERR_TAIL: usize = 2048;

const WORDS: [&str; 16] = [
    "amber", "basalt", "cedar", "dune", "ember", "fjord", "granite", "harbor", "indigo", "juniper",
    "kestrel", "lantern", "meadow", "nectar", "orchid", "pewter",
];

fn word(i: usize) -> &'static str {
    WORDS[i % WORDS.len()]
}

/// The marker and key sentence of chapter `c` of `book`.
fn chapter_marks(book: &str, library: &str, index: usize, c: usize) -> (String, String) {
    let marker = format!("sbx-{library}-{book}-c{c}");
    let key = format!(
        "The {} {} keeps the {} of chapter {c} in book {book}.",
        word(index * 3 + c),
        word(index * 5 + c + 7),
        word(index + c * 11)
    );
    (marker, key)
}

fn book_text(book: &str, library: &str, index: usize) -> String {
    let mut text = format!("Sandbox Book {book}\n\n");
    for c in 1..=3 {
        let (marker, key) = chapter_marks(book, library, index, c);
        text.push_str(&format!("Chapter {c}\n\n"));
        text.push_str(&format!(
            "The marker {marker} opens this chapter. {key}\n\n"
        ));
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
            text.push_str(&sentences.join(" "));
            text.push_str("\n\n");
        }
    }
    text
}

const PAPER_TITLE: &str = "Sandbox Paper One";
const PAPER_ABSTRACT: &str =
    "Abstract. This paper sbx-alpha-p1 reports how a sandbox keeps its lanterns lit.";

fn paper_text() -> String {
    format!(
        "{PAPER_TITLE}\n\n{PAPER_ABSTRACT}\n\n\
         The first section describes the harbor and the meadow in plain terms.\n\n\
         The second section closes with the orchid and the pewter bowl.\n"
    )
}

/// Write the preset's inputs under `host/inputs/`, the same bytes on
/// every call, and `manifest.json` naming each with what it holds.
pub fn write_inputs(host: &Sandbox) -> io::Result<()> {
    let inputs = host.inputs();
    std::fs::create_dir_all(&inputs)?;
    let mut books = Vec::new();
    for (index, (book, library)) in BOOKS.iter().enumerate() {
        std::fs::write(
            inputs.join(format!("{book}.txt")),
            book_text(book, library, index),
        )?;
        let chapters: Vec<Value> = (1..=3)
            .map(|c| {
                let (marker, key) = chapter_marks(book, library, index, c);
                json!({ "chapter": c, "marker": marker, "key": key })
            })
            .collect();
        books.push(
            json!({ "file": format!("{book}.txt"), "library": library, "chapters": chapters }),
        );
    }
    std::fs::write(inputs.join("p1.txt"), paper_text())?;
    let tiny = inputs.join("refsrc").join("tiny");
    std::fs::create_dir_all(&tiny)?;
    std::fs::write(tiny.join("book.toml"), DISTILL_TINY_BOOK_TOML)?;
    std::fs::write(tiny.join("source.md"), DISTILL_TINY_SOURCE)?;
    std::fs::write(inputs.join("broken.epub"), "not a zip archive")?;
    let manifest = json!({
        "books": books,
        "paper": {
            "file": "p1.txt",
            "library": "alpha",
            "title": PAPER_TITLE,
            "abstract": PAPER_ABSTRACT,
        },
        "reference": {
            "dir": "refsrc/tiny",
            "library": "alpha",
            "slug": "tiny",
            "headwords": ["Smith", "Jones"],
        },
        "broken": { "file": "broken.epub", "library": "alpha" },
    });
    let mut bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    bytes.push(b'\n');
    std::fs::write(inputs.join("manifest.json"), bytes)
}

/// One finding of the shape check: the key it is about and what was
/// read instead.
#[derive(Debug, Clone, PartialEq, Eq)]
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
        let ok = books.len() == 1 && books[0]["slug"] == "tiny" && books[0]["entry_count"] == 2;
        expect(
            "distill.tiny",
            ok,
            format!("books are {}", distill["books"]),
        );
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

/// One read of the shape check, as it came back.
enum Read {
    Json(Value),
    Exec(String),
    Parse(String),
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
    /// libraries, the snapshot, and the shape check.
    pub(crate) async fn build_rich(&mut self, call: &mut Call) -> ToolResult<()> {
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
        let spec =
            StubSpec::new(RICH_DIM, Vectors::Spread, None).expect("the preset's width is in range");
        self.stub_start(spec);

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
            &input("refsrc/tiny"),
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
            let spec = StubSpec::new(RICH_DIM, Vectors::Spread, None)
                .expect("the preset's width is in range");
            self.stub_start(spec);
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
        let opts = ExecOpts {
            capture: Capture::Memory,
            ..ExecOpts::default()
        };
        let mut output = None;
        let outcome = self.exec_inner(call, &args, &opts, &mut output).await?;
        let (stdout, stderr) = memory(output);
        let read = if (outcome.class, outcome.code) != (Class::Exited, Some(0)) {
            Read::Exec(format!(
                "{:?} {:?}: {}",
                outcome.class,
                outcome.code,
                tail(&stderr)
            ))
        } else {
            match serde_json::from_slice::<Value>(&stdout) {
                Ok(value) => Read::Json(value),
                Err(e) => Read::Parse(format!("{e}: {}", tail(&stdout))),
            }
        };
        match read {
            Read::Json(value) => {
                readings.insert(source.to_string(), value);
            }
            Read::Exec(detail) => findings.push(ShapeFinding {
                key: format!("{source}.exec"),
                detail,
            }),
            Read::Parse(detail) => findings.push(ShapeFinding {
                key: format!("{source}.parse"),
                detail,
            }),
        }
        Ok(())
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
            json!({ "books": [{ "slug": "tiny", "title": "tiny", "entry_count": 2,
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
            ("distill.tiny", |r| {
                r.get_mut("distill").unwrap()["books"][0]["entry_count"] = json!(3);
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

    /// The inputs are the same bytes on every write, and plain ASCII.
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
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(ha.inputs().join("manifest.json")).unwrap())
                .expect("manifest");
        let text = std::fs::read_to_string(ha.inputs().join("b3.txt")).expect("b3");
        let chapter = &manifest["books"][2]["chapters"][1];
        assert!(
            text.contains(chapter["marker"].as_str().unwrap()),
            "{chapter}"
        );
        assert!(text.contains(chapter["key"].as_str().unwrap()), "{chapter}");
    }
}
