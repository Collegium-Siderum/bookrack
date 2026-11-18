// SPDX-License-Identifier: Apache-2.0

//! The source half of rule intake, through its public entry points:
//! loading a rule file, finding its members, the canonical text, the
//! source hash, and the fingerprint.

use std::path::{Path, PathBuf};

use bookrack_audit_profile::HeadingPatterns;
use bookrack_core::Explain;
use bookrack_extract::rule::{
    CompletenessReport, MemberOrder, RuleError, RuleFile, RuleKind, RuleSource, UnitsBy,
};
use bookrack_extract::{
    BlockKind, ContributorRole, EXTRACTOR_VERSION, Extraction, TextLayerQuality, fallback_kinds,
};

/// A rule that reads every `.txt` beside it as one unit per file.
const BY_FILE: &str = "schema = 1\n\n[members]\ninclude = [\"*.txt\"]\n\n[units]\nby = \"file\"\n";

/// Three members with line feeds only. The second carries a
/// non-ASCII word so that a GB18030 copy of it is not valid UTF-8.
const BASE: &[(&str, &str)] = &[
    ("01.txt", "Chapter one\n\nFirst paragraph.\nIt runs on.\n"),
    (
        "02.txt",
        "Chapter two \u{7b2c}\u{4e8c}\u{7ae0}\n\n  An indented line.\n",
    ),
    ("03.txt", "Chapter three\n\nLast paragraph.\n"),
];

struct Dir(tempfile::TempDir);

impl Dir {
    fn new() -> Dir {
        Dir(tempfile::tempdir().expect("tempdir"))
    }

    fn path(&self) -> &Path {
        self.0.path()
    }

    fn write(&self, relative: &str, bytes: impl AsRef<[u8]>) -> &Dir {
        let path = self.path().join(relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
        std::fs::write(path, bytes).expect("write");
        self
    }

    fn rule(&self, text: &str) -> PathBuf {
        self.write("rule.toml", text);
        self.path().join("rule.toml")
    }

    fn base(&self) -> &Dir {
        for (name, text) in BASE {
            self.write(name, text);
        }
        self
    }
}

fn load(path: &Path) -> RuleFile {
    RuleFile::load(path).expect("the rule loads")
}

fn read(dir: &Dir, rule: &str) -> (RuleFile, RuleSource) {
    let file = load(&dir.rule(rule));
    let source = file.read().expect("the members read");
    (file, source)
}

fn base_sha() -> String {
    let dir = Dir::new();
    dir.base();
    read(&dir, BY_FILE).1.source_sha256()
}

fn relatives(file: &RuleFile) -> Vec<String> {
    file.members()
        .expect("members")
        .into_iter()
        .map(|m| m.relative)
        .collect()
}

/// The detail of the `Parse` refusal loading `rule` ends in.
fn refusal(rule: &str) -> String {
    let dir = Dir::new();
    match RuleFile::load(&dir.rule(rule)) {
        Err(RuleError::Parse { detail, .. }) => detail,
        other => panic!("expected a Parse refusal, got {other:?}"),
    }
}

#[track_caller]
fn assert_names(detail: &str, parts: &[&str]) {
    for part in parts {
        assert!(detail.contains(part), "{detail:?} does not name {part:?}");
    }
}

// ---- canonical text -------------------------------------------------------

#[test]
fn members_join_into_one_text_with_a_blank_line_between() {
    let dir = Dir::new();
    dir.write("1.txt", "a\r\n\r\nb  \r\n\r\n")
        .write("2.txt", "")
        .write("3.txt", "  c\rd\n");
    let (_, source) = read(&dir, BY_FILE);

    assert_eq!(source.text, "a\n\nb\n\n\n\n  c\nd");
    assert_eq!(source.byte_size(), 13);
    let spans: Vec<(&str, &str)> = source
        .members
        .iter()
        .map(|m| (m.relative.as_str(), &source.text[m.span.clone()]))
        .collect();
    assert_eq!(
        spans,
        [("1.txt", "a\n\nb"), ("2.txt", ""), ("3.txt", "  c\nd")]
    );
    assert_eq!(source.members[1].span, 6..6);
}

// ---- source hash ----------------------------------------------------------

#[test]
fn a_byte_order_mark_on_every_member_keeps_the_source_hash() {
    let dir = Dir::new();
    for (name, text) in BASE {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(text.as_bytes());
        assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]));
        dir.write(name, bytes);
    }
    assert_eq!(read(&dir, BY_FILE).1.source_sha256(), base_sha());
}

#[test]
fn a_gb18030_member_keeps_the_source_hash_and_is_recorded() {
    let dir = Dir::new();
    dir.base();
    let (bytes, _, lossy) = encoding_rs::GB18030.encode(BASE[1].1);
    assert!(!lossy, "the member encodes to GB18030 without loss");
    assert!(
        std::str::from_utf8(&bytes).is_err(),
        "the GB18030 copy must not be valid UTF-8, or it decodes as the original",
    );
    dir.write("02.txt", &bytes);

    let (_, source) = read(&dir, BY_FILE);
    assert_eq!(source.source_sha256(), base_sha());
    let recorded: Vec<(&str, bool)> = source
        .fallbacks
        .iter()
        .map(|e| {
            let names_member = e
                .detail
                .as_deref()
                .is_some_and(|d| d.starts_with("02.txt: "));
            (e.kind.as_str(), names_member)
        })
        .collect();
    assert_eq!(recorded, [(fallback_kinds::TXT_GB18030, true)]);
}

#[test]
fn a_member_saved_with_other_line_endings_keeps_the_source_hash() {
    for (ending, probe) in [("\r\n", &b"\r\n"[..]), ("\r", &b"\rFirst"[..])] {
        let dir = Dir::new();
        dir.base();
        let resaved = BASE[0].1.replace('\n', ending);
        assert!(
            resaved.as_bytes().windows(probe.len()).any(|w| w == probe),
            "the resaved member carries {ending:?}",
        );
        assert!(ending == "\r\n" || !resaved.contains('\n'));
        dir.write("01.txt", resaved);
        assert_eq!(
            read(&dir, BY_FILE).1.source_sha256(),
            base_sha(),
            "{ending:?}"
        );
    }
}

#[test]
fn extra_line_breaks_at_the_end_of_a_member_keep_the_source_hash() {
    let dir = Dir::new();
    dir.base();
    let padded = format!("{}\n\n\n", BASE[0].1);
    assert!(padded.ends_with("\n\n\n\n"));
    dir.write("01.txt", padded);
    assert_eq!(read(&dir, BY_FILE).1.source_sha256(), base_sha());
}

#[test]
fn whitespace_at_the_end_of_a_line_keeps_the_source_hash() {
    let dir = Dir::new();
    dir.base();
    let padded = BASE[0].1.replace("paragraph.\n", "paragraph. \t\u{3000}\n");
    assert_ne!(padded, BASE[0].1);
    dir.write("01.txt", padded);
    assert_eq!(read(&dir, BY_FILE).1.source_sha256(), base_sha());
}

#[test]
fn whitespace_at_the_start_of_a_line_changes_the_source_hash() {
    let dir = Dir::new();
    dir.base();
    let indented = BASE[0].1.replace("First", " First");
    assert_ne!(indented, BASE[0].1);
    dir.write("01.txt", indented);
    assert_ne!(read(&dir, BY_FILE).1.source_sha256(), base_sha());
}

#[test]
fn a_byte_order_mark_past_the_start_of_a_member_changes_the_source_hash() {
    let dir = Dir::new();
    dir.base();
    dir.write("01.txt", BASE[0].1.replace("First", "\u{feff}First"));
    assert_ne!(read(&dir, BY_FILE).1.source_sha256(), base_sha());
}

#[test]
fn the_rule_does_not_reach_the_source_hash() {
    let dir = Dir::new();
    dir.base();
    let whole = BY_FILE.replace("by = \"file\"", "by = \"whole\"");
    assert_ne!(whole, BY_FILE);
    assert_eq!(read(&dir, &whole).1.source_sha256(), base_sha());
}

// ---- fingerprint ----------------------------------------------------------

/// A rule that sets a heading pattern and a title, to have keys to edit.
const TITLED: &str = "schema = 1\n\n[members]\ninclude = [\"*.txt\"]\n\n[units]\nby = \"file\"\n\n\
                      [structure]\nheadings = \"patterns\"\n\
                      patterns = [{ regex = '^Chapter \\w+', depth = 0 }]\n\n\
                      [biblio]\ntitle = \"A Work\"\n";

fn fingerprint_of(rule: &str) -> String {
    let dir = Dir::new();
    dir.base();
    let (file, source) = read(&dir, rule);
    file.fingerprint(&source)
}

#[test]
fn whitespace_and_comments_in_the_rule_file_keep_the_fingerprint() {
    let respaced = TITLED
        .replace(
            "schema = 1",
            "# what this rule reads\nschema=1   # the revision",
        )
        .replace("\n\n", "\n\n\n")
        .replace("depth = 0", "depth=0");
    assert_ne!(respaced, TITLED);
    assert_eq!(fingerprint_of(&respaced), fingerprint_of(TITLED));
}

#[test]
fn a_changed_key_changes_the_fingerprint() {
    let base = fingerprint_of(TITLED);
    for (from, to) in [
        ("^Chapter \\w+", "^Chapter \\w+$"),
        ("by = \"file\"", "by = \"whole\""),
        ("title = \"A Work\"", "title = \"Another Work\""),
    ] {
        let edited = TITLED.replace(from, to);
        assert_ne!(edited, TITLED, "{from:?} is in the rule");
        assert_ne!(fingerprint_of(&edited), base, "{from:?} -> {to:?}");
    }
}

#[test]
fn two_members_merged_into_one_change_the_fingerprint_but_not_the_text() {
    let apart = Dir::new();
    apart.base();
    let (apart_file, apart_source) = read(&apart, BY_FILE);

    let merged = Dir::new();
    merged
        .write("01.txt", format!("{}\n{}", BASE[0].1, BASE[1].1))
        .write("03.txt", BASE[2].1);
    let (merged_file, merged_source) = read(&merged, BY_FILE);

    assert_eq!(merged_source.text, apart_source.text);
    assert_eq!(merged_source.source_sha256(), apart_source.source_sha256());
    assert_eq!(merged_source.members.len() + 1, apart_source.members.len());
    assert_ne!(
        merged_file.fingerprint(&merged_source),
        apart_file.fingerprint(&apart_source)
    );
}

#[test]
fn a_renamed_member_changes_the_fingerprint_but_not_the_text() {
    let before = Dir::new();
    before.base();
    let (before_file, before_source) = read(&before, BY_FILE);

    let after = Dir::new();
    after
        .write("01.txt", BASE[0].1)
        .write("02-renamed.txt", BASE[1].1)
        .write("03.txt", BASE[2].1);
    let (after_file, after_source) = read(&after, BY_FILE);

    assert_eq!(after_source.text, before_source.text);
    assert_ne!(
        after_file.fingerprint(&after_source),
        before_file.fingerprint(&before_source)
    );
}

#[test]
fn moving_the_rule_and_its_members_together_keeps_both_values() {
    let here = Dir::new();
    here.write("book/rule.toml", TITLED);
    for (name, text) in BASE {
        here.write(&format!("book/{name}"), text);
    }
    let before = load(&here.path().join("book/rule.toml"));
    let before_source = before.read().expect("read");

    std::fs::rename(here.path().join("book"), here.path().join("moved")).expect("move");
    let after = load(&here.path().join("moved/rule.toml"));
    let after_source = after.read().expect("read");

    assert_eq!(after_source.source_sha256(), before_source.source_sha256());
    assert_eq!(
        after.fingerprint(&after_source),
        before.fingerprint(&before_source)
    );
}

// ---- members --------------------------------------------------------------

fn numbered(dir: &Dir) {
    for name in ["1.txt", "2.txt", "10.txt", "notes.md", "sub/3.txt"] {
        dir.write(name, name);
    }
}

#[test]
fn members_come_in_natural_order_unless_the_rule_asks_for_name_order() {
    let dir = Dir::new();
    numbered(&dir);
    let natural = load(&dir.rule(BY_FILE));
    assert_eq!(natural.rule().members.order, MemberOrder::Natural);
    assert_eq!(relatives(&natural), ["1.txt", "2.txt", "10.txt"]);

    let by_name = BY_FILE.replace(
        "include = [\"*.txt\"]",
        "include = [\"*.txt\"]\norder = \"name\"",
    );
    assert_eq!(
        relatives(&load(&dir.rule(&by_name))),
        ["1.txt", "10.txt", "2.txt"]
    );
}

#[test]
fn only_a_double_star_descends_and_an_exclude_keeps_a_file_out() {
    let dir = Dir::new();
    numbered(&dir);
    let deep = BY_FILE.replace("[\"*.txt\"]", "[\"**/*.txt\"]\nexclude = [\"2.*\"]");
    assert_eq!(
        relatives(&load(&dir.rule(&deep))),
        ["1.txt", "10.txt", "sub/3.txt"]
    );
}

#[test]
fn an_explicit_list_sets_the_order_and_must_cover_every_match() {
    let dir = Dir::new();
    numbered(&dir);
    let listed = |list: &str| {
        BY_FILE.replace(
            "[\"*.txt\"]",
            &format!("[\"*.txt\"]\norder = \"explicit\"\nexplicit = {list}"),
        )
    };

    let all = load(&dir.rule(&listed("[\"10.txt\", \"sub/3.txt\", \"1.txt\", \"2.txt\"]")));
    assert_eq!(relatives(&all), ["10.txt", "sub/3.txt", "1.txt", "2.txt"]);

    let short = load(&dir.rule(&listed("[\"10.txt\", \"1.txt\"]")));
    match short.members() {
        Err(RuleError::MemberUnlisted { member, .. }) => assert_eq!(member, "2.txt"),
        other => panic!("expected MemberUnlisted, got {other:?}"),
    }

    let absent = load(&dir.rule(&listed("[\"1.txt\", \"2.txt\", \"10.txt\", \"9.txt\"]")));
    match absent.members() {
        Err(RuleError::MemberMissing { member, .. }) => assert_eq!(member, "9.txt"),
        other => panic!("expected MemberMissing, got {other:?}"),
    }
}

#[test]
fn a_rule_that_matches_nothing_has_no_members() {
    let dir = Dir::new();
    dir.write("notes.md", "x");
    let file = load(&dir.rule(BY_FILE));
    assert!(matches!(file.members(), Err(RuleError::NoMembers { .. })));
    assert!(matches!(file.read(), Err(RuleError::NoMembers { .. })));
}

#[cfg(unix)]
#[test]
fn a_linked_directory_is_searched_and_a_link_back_into_the_tree_ends() {
    let outside = Dir::new();
    outside.write("4.txt", "four");
    let dir = Dir::new();
    dir.write("1.txt", "one");
    std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).expect("link out");
    std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).expect("link back");

    let deep = BY_FILE.replace("[\"*.txt\"]", "[\"**/*.txt\"]");
    assert_eq!(
        relatives(&load(&dir.rule(&deep))),
        ["1.txt", "linked/4.txt"]
    );
}

#[cfg(unix)]
#[test]
fn a_member_that_cannot_be_read_is_a_retryable_failure() {
    let dir = Dir::new();
    dir.write("1.txt", "one");
    std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("2.txt")).expect("link");
    let file = load(&dir.rule(BY_FILE));
    let err = file.read().expect_err("a dangling member fails the read");
    assert!(matches!(err, RuleError::MemberRead { .. }), "{err:?}");
    assert!(err.explain().data.retryable);
}

// ---- refusals at load -----------------------------------------------------

#[test]
fn a_rule_with_defaults_loads_as_a_book() {
    let dir = Dir::new();
    let file = load(&dir.rule(BY_FILE));
    assert_eq!(file.rule().kind, RuleKind::Book);
    assert_eq!(file.rule().units.by, UnitsBy::File);
    assert_eq!(file.rule().completeness.number, r"^(?P<n>\d+)");
}

#[test]
fn a_reference_rule_loads() {
    let dir = Dir::new();
    let file = load(&dir.rule(&format!("kind = \"reference\"\n{BY_FILE}")));
    assert_eq!(file.rule().kind, RuleKind::Reference);
}

#[test]
fn an_unknown_key_is_refused_by_name_and_line() {
    assert_names(
        &refusal(&BY_FILE.replace("by = \"file\"", "by = \"file\"\nseparator = \"x\"")),
        &["line 8", "`separator`"],
    );
    assert_names(
        &refusal(&format!("{BY_FILE}\n[extra]\nx = 1\n")),
        &["line 9", "`extra`"],
    );
}

#[test]
fn a_pattern_that_does_not_compile_is_refused_at_each_of_its_keys() {
    let cases = [
        (
            "[units]\nby = \"marker\"\nmarker = '^(page'\n",
            "units.marker (line 8)",
        ),
        (
            "[units]\nby = \"file\"\n\n[structure]\nunit_levels = [\n  { regex = '^a', depth = 0 },\n  \
             { regex = '^(b', depth = 1 },\n]\n",
            "structure.unit_levels[1].regex (line 12)",
        ),
        (
            "[units]\nby = \"file\"\n\n[structure]\npatterns = [{ regex = '[z-a]', depth = 0 }]\n",
            "structure.patterns[0].regex (line 10)",
        ),
        (
            "[units]\nby = \"file\"\n\n[completeness]\nnumber = '(?P<n>\\d+'\n",
            "completeness.number (line 10)",
        ),
    ];
    for (tail, key) in cases {
        let rule = format!("schema = 1\n\n[members]\ninclude = [\"*.txt\"]\n\n{tail}");
        let detail = refusal(&rule);
        assert_names(&detail, &[key]);
        assert!(!detail.contains('\n'), "{detail:?} spans lines");
    }
}

#[test]
fn a_member_pattern_that_does_not_compile_is_refused() {
    assert_names(
        &refusal(&BY_FILE.replace("[\"*.txt\"]", "[\"*.txt\", \"0[1-9.txt\"]")),
        &["members.include[1] (line 4)", "never closed"],
    );
    assert_names(
        &refusal(&BY_FILE.replace("[\"*.txt\"]", "[\"*.txt\"]\nexclude = [\"[z-a]\"]")),
        &["members.exclude[0] (line 5)"],
    );
}

#[test]
fn keys_that_contradict_one_another_are_refused() {
    let cases = [
        (
            BY_FILE.replace(
                "by = \"file\"",
                "by = \"whole\"\n\n[structure]\nunit_titles = \"filename\"",
            ),
            "structure.unit_titles (line 10)",
        ),
        (
            BY_FILE.replace("[\"*.txt\"]", "[\"*.txt\"]\norder = \"explicit\""),
            "members.explicit:",
        ),
        (
            BY_FILE.replace("by = \"file\"", "by = \"marker\""),
            "units.marker:",
        ),
        (
            BY_FILE.replace("[\"*.txt\"]", "[]"),
            "members.include (line 4)",
        ),
    ];
    for (rule, key) in cases {
        assert_names(&refusal(&rule), &[key]);
    }
}

#[test]
fn the_reserved_unit_source_is_refused_as_unsupported() {
    assert_names(
        &refusal(&BY_FILE.replace("by = \"file\"", "by = \"adapter\"")),
        &["units.by (line 7)", "not supported in this version"],
    );
}

#[test]
fn a_schema_revision_other_than_this_builds_is_refused() {
    assert_names(
        &refusal(&BY_FILE.replace("schema = 1", "schema = 2")),
        &["schema (line 1)", "revision 2"],
    );
    assert_names(
        &refusal(&BY_FILE.replace("schema = 1\n", "")),
        &["`schema`"],
    );
}

#[test]
fn a_number_pattern_without_the_n_group_is_refused() {
    assert_names(
        &refusal(&format!("{BY_FILE}\n[completeness]\nnumber = '^\\d+'\n")),
        &["completeness.number (line 10)", "`n`"],
    );
}

#[test]
fn a_listed_member_outside_the_rule_directory_or_listed_twice_is_refused() {
    let listed = |list: &str| {
        BY_FILE.replace(
            "[\"*.txt\"]",
            &format!("[\"*.txt\"]\norder = \"explicit\"\nexplicit = {list}"),
        )
    };
    for list in [
        "[\"../1.txt\"]",
        "[\"/tmp/1.txt\"]",
        "[\"a//1.txt\"]",
        "[\"./1.txt\"]",
        "[\"\"]",
    ] {
        assert_names(&refusal(&listed(list)), &["members.explicit[0] (line 6)"]);
    }
    assert_names(
        &refusal(&listed("[\"1.txt\", \"2.txt\", \"1.txt\"]")),
        &["members.explicit[2] (line 6)", "twice"],
    );
}

#[test]
fn a_rule_file_that_is_missing_or_not_text_is_refused() {
    let dir = Dir::new();
    match RuleFile::load(&dir.path().join("absent.toml")) {
        Err(RuleError::Parse { detail, .. }) => assert_names(&detail, &["could not be read"]),
        other => panic!("expected a Parse refusal, got {other:?}"),
    }
    dir.write("rule.toml", [0xFF, 0xFE, 0x00]);
    match RuleFile::load(&dir.path().join("rule.toml")) {
        Err(RuleError::Parse { detail, .. }) => assert_names(&detail, &["not UTF-8"]),
        other => panic!("expected a Parse refusal, got {other:?}"),
    }
}

// ---- wording --------------------------------------------------------------

#[test]
fn every_refusal_explains_itself_in_three_parts() {
    let dir = Dir::new();
    numbered(&dir);
    let listed = |list: &str| {
        BY_FILE.replace(
            "[\"*.txt\"]",
            &format!("[\"*.txt\"]\norder = \"explicit\"\nexplicit = {list}"),
        )
    };
    let parse = RuleFile::load(&dir.rule("schema = 7\n")).expect_err("refused");
    let none = load(&dir.rule(&BY_FILE.replace("*.txt", "*.rst")))
        .members()
        .expect_err("no members");
    let missing = load(&dir.rule(&listed("[\"1.txt\", \"2.txt\", \"10.txt\", \"9.txt\"]")))
        .members()
        .expect_err("missing");
    let unlisted = load(&dir.rule(&listed("[\"1.txt\"]")))
        .members()
        .expect_err("unlisted");

    let rule_path = dir.path().join("rule.toml").display().to_string();
    for (err, opening, retryable) in [
        (&parse, "cannot read rule file \"", false),
        (&none, "rule \"", false),
        (&missing, "rule \"", false),
        (&unlisted, "rule \"", false),
    ] {
        let problem = err.explain();
        assert!(problem.summary.starts_with(opening), "{problem:?}");
        assert!(problem.summary.contains(&rule_path), "{problem:?}");
        assert!(!problem.summary.ends_with('.'), "{problem:?}");
        assert!(!problem.summary.contains('\n'), "{problem:?}");
        assert!(problem.data.hint.is_some(), "{problem:?}");
        assert_eq!(problem.data.retryable, retryable, "{problem:?}");
        assert!(!err.to_string().contains('\n'), "{err}");
    }
    assert_names(
        parse.explain().data.detail.as_deref().expect("detail"),
        &["`members`"],
    );
    assert!(missing.explain().summary.ends_with("\"9.txt\""));
    assert!(unlisted.explain().summary.ends_with("\"10.txt\""));
}

fn rule_fixture(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rule")
        .join(relative)
}

fn extract_dir(dir: &Dir, rule: &str) -> (Extraction, CompletenessReport) {
    let (file, source) = read(dir, rule);
    file.extract_with_report(&source, &HeadingPatterns::default())
        .expect("extract_with_report")
}

// ---- golden: a page-marked file ------------------------------------------

/// The blocks of the page-marked fixture, as `(text, sheet)`, every one
/// a body block. The values are frozen: a change to how the rule engine
/// cuts units, strips frontmatter, or joins the lines of a paragraph
/// shows up here.
const POLYOCR_V1_BLOCKS: &[(&str, u32)] = &[
    ("Synthetic OCR Fixture", 1),
    ("A round-trip anchor for the OCR adapter", 1),
    ("Chapter One", 2),
    (
        "The first paragraph of the first chapter. Plain English prose with no special \
         characters; the sample exists to anchor the round-trip assertions in tests, not to \
         look like a real book.",
        2,
    ),
    ("A second paragraph follows on the same physical page.", 2),
    (
        "The third page continues the chapter. There is no outline entry for this page; the \
         parser must place the body block here under the existing chapter, with the outline \
         anchored at the chapter's first block on page two.",
        3,
    ),
];

#[test]
fn golden_polyocr_v1_yields_the_frozen_blocks() {
    let file = load(&rule_fixture("polyocr-v1/rule.toml"));
    let source = file.read().expect("read rule source");
    let ex = file
        .extract(&source, &HeadingPatterns::default())
        .expect("rule extract");

    assert!(ex.toc.entries.is_empty());
    let origin = ex
        .provenance
        .rule
        .as_ref()
        .expect("Provenance.rule must be populated");
    assert_eq!(origin.units.len(), 3);

    let blocks: Vec<(BlockKind, &str, u32)> = ex
        .blocks
        .iter()
        .map(|b| {
            let sheet = origin.units[b.source_unit as usize]
                .sheet
                .expect("sheet number from marker");
            (b.kind, b.text.as_str(), sheet)
        })
        .collect();
    let frozen: Vec<(BlockKind, &str, u32)> = POLYOCR_V1_BLOCKS
        .iter()
        .map(|&(text, sheet)| (BlockKind::Body, text, sheet))
        .collect();
    assert_eq!(blocks, frozen);
}

// ---- units & source_unit --------------------------------------------------

#[test]
fn unit_cutting_preserves_empty_unit_index_and_omits_its_toc_entry() {
    // M13a: 3-member by=file set with the middle member empty.
    let dir = Dir::new();
    dir.write("01_First.txt", "First body paragraph.\n")
        .write("02_Middle.txt", "   \n\t\n")
        .write("03_Third.txt", "Third body paragraph.\n");
    let rule = format!("{BY_FILE}\n[structure]\nunit_titles = \"filename\"\nheadings = \"none\"\n");
    let (ex, report) = extract_dir(&dir, &rule);
    let origin = ex.provenance.rule.as_ref().expect("rule origin");
    assert_eq!(origin.units.len(), 3);
    assert_eq!(origin.units[0].sheet, Some(1));
    assert_eq!(origin.units[1].sheet, Some(2));
    assert_eq!(origin.units[2].sheet, Some(3));
    assert_eq!(report.numbered, 3);
    assert!(report.gaps.is_empty());

    let block_units: Vec<u32> = ex.blocks.iter().map(|b| b.source_unit).collect();
    assert_eq!(block_units, vec![0, 2]);
    let toc_labels: Vec<(&str, Option<usize>)> = ex
        .toc
        .entries
        .iter()
        .map(|e| (e.label.as_str(), e.start_block))
        .collect();
    assert_eq!(toc_labels, vec![("First", Some(0)), ("Third", Some(1))]);

    // M21: marker file with an empty middle page.
    let mdir = Dir::new();
    mdir.write(
        "pages.md",
        "<!-- page 1 (sheet 1) -->\nPage one.\n<!-- page 2 (sheet 2) -->\n\n<!-- page 3 (sheet 3) -->\nPage three.\n",
    );
    let mrule = "schema = 1\n[members]\ninclude = [\"pages.md\"]\n[units]\nby = \"marker\"\nmarker = '^<!-- page (?P<label>\\S*) \\(sheet (?P<sheet>\\d+)\\) -->$'\n[structure]\nunit_titles = \"none\"\nheadings = \"none\"\n";
    let (mfile, msource) = read(&mdir, mrule);
    let units = mfile.units(&msource).expect("units");
    assert_eq!(units.len(), 3);
    assert!(units[1].text.is_empty());
    let mex = mfile
        .extract(&msource, &HeadingPatterns::default())
        .expect("extract");
    let munits: Vec<u32> = mex.blocks.iter().map(|b| b.source_unit).collect();
    assert_eq!(munits, vec![0, 2]);
}

#[test]
fn unit_origins_record_member_label_and_sheet_across_all_modes() {
    // Marker starting at sheet 5, with non-numeric label (M20) and empty label (M23).
    let dir = Dir::new();
    dir.write(
        "pages.md",
        "<!-- page iii (sheet 5) -->\nPreface text.\n<!-- page  (sheet 6) -->\nUnlabeled page text.\n",
    );
    let mrule = "schema = 1\n[members]\ninclude = [\"pages.md\"]\n[units]\nby = \"marker\"\nmarker = '^<!-- page (?P<label>\\S*) \\(sheet (?P<sheet>\\d+)\\) -->$'\n[structure]\nunit_titles = \"none\"\nheadings = \"none\"\n";
    let (ex, _) = extract_dir(&dir, mrule);
    let origin = ex.provenance.rule.as_ref().expect("rule origin");
    assert_eq!(origin.units.len(), 2);
    assert_eq!(origin.units[0].member, "pages.md");
    assert_eq!(origin.units[0].label.as_deref(), Some("iii"));
    assert_eq!(origin.units[0].sheet, Some(5));
    assert_eq!(origin.units[1].label, None);
    assert_eq!(origin.units[1].sheet, Some(6));
    assert_eq!(ex.blocks[0].source_unit, 0);
    assert_eq!(ex.blocks[1].source_unit, 1);

    // by = whole over multiple members records the first member's relative path.
    let wdir = Dir::new();
    wdir.write("01.txt", "First file paragraph.\n")
        .write("02.txt", "Second file paragraph.\n");
    let wrule = "schema = 1\n[members]\ninclude = [\"*.txt\"]\n[units]\nby = \"whole\"\n[structure]\nunit_titles = \"none\"\nheadings = \"none\"\n";
    let (wex, _) = extract_dir(&wdir, wrule);
    let worigin = wex.provenance.rule.as_ref().expect("rule origin");
    assert_eq!(worigin.units.len(), 1);
    assert_eq!(worigin.units[0].member, "01.txt");
    assert_eq!(worigin.units[0].label, None);
    assert_eq!(worigin.units[0].sheet, None);
    assert_eq!(wex.blocks.len(), 2);
    assert_eq!(wex.blocks[0].text, "First file paragraph.");
    assert_eq!(wex.blocks[1].text, "Second file paragraph.");
}

#[test]
fn frontmatter_stripping_obeys_none_first_and_each() {
    let dir = Dir::new();
    dir.write("01.txt", "---\ntitle: One\n---\nBody one.\n")
        .write("02.txt", "---\ntitle: Two\n---\nBody two.\n");
    let make_rule = |fm: &str| {
        format!(
            "schema = 1\n[members]\ninclude = [\"*.txt\"]\nfrontmatter = \"{fm}\"\n[units]\nby = \"file\"\n[structure]\nunit_titles = \"none\"\nheadings = \"none\"\n"
        )
    };
    let (ex_first, _) = extract_dir(&dir, &make_rule("first"));
    assert_eq!(ex_first.blocks[0].text, "Body one.");
    assert!(ex_first.blocks[1].text.starts_with("--- title: Two ---"));

    let (ex_each, _) = extract_dir(&dir, &make_rule("each"));
    assert_eq!(ex_each.blocks[0].text, "Body one.");
    assert_eq!(ex_each.blocks[1].text, "Body two.");

    let (ex_none, _) = extract_dir(&dir, &make_rule("none"));
    assert!(ex_none.blocks[0].text.starts_with("--- title: One ---"));
}

// ---- structure: unit_levels, headings, blocks, TOC ------------------------

#[test]
fn unit_levels_set_heading_depth_level_and_strip_label_prefix() {
    let dir = Dir::new();
    let md_bytes = std::fs::read(rule_fixture("cjk-cases/markdown.md")).expect("read fixture");
    dir.write("01.md", md_bytes);

    let rule_d0 = "schema = 1\n[members]\ninclude = [\"*.md\"]\n[units]\nby = \"file\"\n[structure]\nunit_titles = \"first-line\"\nunit_levels = [{ regex = '^#\\s+(?P<label>.+)$', depth = 0 }]\nheadings = \"none\"\n";
    let (ex0, _) = extract_dir(&dir, rule_d0);
    assert_eq!(ex0.blocks[0].kind, BlockKind::Heading { level: 1 });
    assert_eq!(ex0.blocks[0].text, "\u{5bfc}\u{8bba}");
    assert_eq!(ex0.toc.entries.len(), 1);
    assert_eq!(ex0.toc.entries[0].label, "\u{5bfc}\u{8bba}");
    assert_eq!(ex0.toc.entries[0].depth, 0);

    let rule_d1 = "schema = 1\n[members]\ninclude = [\"*.md\"]\n[units]\nby = \"file\"\n[structure]\nunit_titles = \"first-line\"\nunit_levels = [{ regex = '^#\\s+(?P<label>.+)$', depth = 1 }]\nheadings = \"none\"\n";
    let (ex1, _) = extract_dir(&dir, rule_d1);
    assert_eq!(ex1.blocks[0].kind, BlockKind::Heading { level: 2 });
    assert_eq!(ex1.toc.entries[0].depth, 1);
}

#[test]
fn toc_is_sorted_by_start_block_and_unit_title_wins_on_tie() {
    let dir = Dir::new();
    dir.write(
        "01_Overview.txt",
        "## Subheading First\n\nParagraph one.\n\n## Second Section\n\nParagraph two.\n",
    );
    let rule = "schema = 1\n[members]\ninclude = [\"*.txt\"]\n[units]\nby = \"file\"\n[structure]\nunit_titles = \"filename\"\nheadings = \"patterns\"\npatterns = [{ regex = '^##\\s+(?P<label>.+)$', depth = 1 }]\n";
    let (ex, _) = extract_dir(&dir, rule);

    // Block 0 is the `## Subheading First` heading block, which is also
    // `first_block` of unit 0. The unit title ("Overview", depth 0) and
    // the body heading ("Subheading First", depth 1) both target block 0;
    // the unit title must win and start_block must be strictly ascending.
    let entries: Vec<(&str, u8, Option<usize>)> = ex
        .toc
        .entries
        .iter()
        .map(|e| (e.label.as_str(), e.depth, e.start_block))
        .collect();
    assert_eq!(
        entries,
        vec![("Overview", 0, Some(0)), ("Second Section", 1, Some(2))],
    );
}

#[test]
fn match_raw_versus_trimmed_and_builtin_heading_family() {
    let dir = Dir::new();
    dir.write(
        "01.txt",
        std::fs::read(rule_fixture("cjk-cases/indented.txt")).expect("indented fixture"),
    );
    let make_rule = |m: &str| {
        format!(
            "schema = 1\n[members]\ninclude = [\"*.txt\"]\n[units]\nby = \"file\"\n[structure]\nunit_titles = \"none\"\nheadings = \"builtin\"\nmatch = \"{m}\"\n"
        )
    };
    let (ex_trimmed, _) = extract_dir(&dir, &make_rule("trimmed"));
    assert_eq!(ex_trimmed.toc.entries.len(), 2);

    let (ex_raw, _) = extract_dir(&dir, &make_rule("raw"));
    assert_eq!(ex_raw.toc.entries.len(), 1);
    assert_eq!(
        ex_raw.toc.entries[0].label,
        "\u{7b2c}\u{4e00}\u{7ae0} \u{8d77}\u{59cb}"
    );

    // Builtin sino family false-positive pin on a body sentence starting with an ordinal.
    let fp_dir = Dir::new();
    fp_dir.write(
        "01.txt",
        std::fs::read(rule_fixture("cjk-cases/false_positive.txt")).expect("fp fixture"),
    );
    let fp_builtin = "schema = 1\n[members]\ninclude = [\"*.txt\"]\n[units]\nby = \"file\"\n[structure]\nunit_titles = \"first-line\"\nheadings = \"builtin\"\n";
    let (ex_fp, _) = extract_dir(&fp_dir, fp_builtin);
    assert_eq!(ex_fp.toc.entries.len(), 2);
    assert!(
        ex_fp.toc.entries[1]
            .label
            .starts_with("\u{7b2c}\u{4e09}\u{90e8}")
    );

    let fp_none = "schema = 1\n[members]\ninclude = [\"*.txt\"]\n[units]\nby = \"file\"\n[structure]\nunit_titles = \"first-line\"\nheadings = \"none\"\n";
    let (ex_clean, _) = extract_dir(&fp_dir, fp_none);
    assert_eq!(ex_clean.toc.entries.len(), 1);
}

#[test]
fn block_splitting_blank_line_versus_line_and_biblio_provenance() {
    let dir = Dir::new();
    dir.write("01.txt", "Title\n\nLine one.\nLine two.\n\nLine three.\n");
    let rule_blank = "schema = 1\n[members]\ninclude = [\"*.txt\"]\n[units]\nby = \"file\"\n[blocks]\nsplit = \"blank-line\"\n[structure]\nunit_titles = \"first-line\"\nheadings = \"none\"\n[biblio]\ntitle = \"Sample Book\"\nsubtitle = \"A Subtitle\"\nauthors = [\"First Author\", \"Second Author\"]\npublisher = \"Test Press\"\nyear = 2025\nisbn = \"978-0-00-000000-0\"\nseries = \"Test Series\"\nlanguage = \"en\"\n[provenance]\ntext_layer = \"doubtful\"\n";
    let (ex_blank, _) = extract_dir(&dir, rule_blank);
    let body_blank: Vec<&str> = ex_blank
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Body))
        .map(|b| b.text.as_str())
        .collect();
    assert_eq!(body_blank, vec!["Line one. Line two.", "Line three."]);
    assert_eq!(ex_blank.biblio.title.as_deref(), Some("Sample Book"));
    assert_eq!(ex_blank.biblio.subtitle.as_deref(), Some("A Subtitle"));
    assert_eq!(ex_blank.biblio.publisher.as_deref(), Some("Test Press"));
    assert_eq!(ex_blank.biblio.year, Some(2025));
    assert_eq!(ex_blank.biblio.year_raw.as_deref(), Some("2025"));
    assert_eq!(ex_blank.biblio.isbn.as_deref(), Some("978-0-00-000000-0"));
    assert_eq!(ex_blank.biblio.series.as_deref(), Some("Test Series"));
    assert_eq!(ex_blank.biblio.language.as_deref(), Some("en"));
    assert_eq!(ex_blank.biblio.contributors.len(), 2);
    assert_eq!(ex_blank.biblio.contributors[0].name, "First Author");
    assert_eq!(
        ex_blank.biblio.contributors[0].role,
        ContributorRole::Author
    );
    assert_eq!(ex_blank.provenance.adapter, "rule-text");
    assert_eq!(ex_blank.provenance.extractor_version, EXTRACTOR_VERSION);
    assert_eq!(
        ex_blank.provenance.text_layer_quality,
        TextLayerQuality::Doubtful
    );

    let rule_line = "schema = 1\n[members]\ninclude = [\"*.txt\"]\n[units]\nby = \"file\"\n[blocks]\nsplit = \"line\"\n[structure]\nunit_titles = \"first-line\"\nheadings = \"none\"\n";
    let (ex_line, _) = extract_dir(&dir, rule_line);
    let body_line: Vec<&str> = ex_line
        .blocks
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Body))
        .map(|b| b.text.as_str())
        .collect();
    assert_eq!(body_line, vec!["Line one.", "Line two.", "Line three."]);
}

// ---- completeness & M-row cases -------------------------------------------

#[test]
fn completeness_reports_gaps_duplicates_and_order_or_fails_when_configured() {
    // M01: gap at 3.
    let gap_dir = Dir::new();
    gap_dir
        .write("01.txt", "Ch 1\n\nBody 1.\n")
        .write("02.txt", "Ch 2\n\nBody 2.\n")
        .write("04.txt", "Ch 4\n\nBody 4.\n");
    let (ex_gap, rep_gap) = extract_dir(&gap_dir, BY_FILE);
    assert_eq!(rep_gap.gaps, vec![3]);
    assert!(rep_gap.duplicates.is_empty());
    assert!(rep_gap.ordered);
    assert!(
        ex_gap
            .provenance
            .fallbacks
            .iter()
            .any(|f| f.kind == fallback_kinds::RULE_GAP && f.detail.as_deref() == Some("3"))
    );

    let strict = format!("{BY_FILE}\n[completeness]\nfail_on_gap = true\n");
    let (strict_file, strict_src) = read(&gap_dir, &strict);
    let err = strict_file
        .extract(&strict_src, &HeadingPatterns::default())
        .expect_err("fail_on_gap must reject gap");
    assert!(
        matches!(err, RuleError::Completeness { ref gaps, .. } if gaps == &[3]),
        "{err:?}"
    );

    // M02: unpadded numbers under `name` order step backwards (1, 10, 2).
    let ord_dir = Dir::new();
    for n in [1, 2, 10] {
        ord_dir.write(&format!("page_{n}.txt"), format!("Page {n}\n\nBody {n}.\n"));
    }
    let name_rule = "schema = 1\n[members]\ninclude = [\"*.txt\"]\norder = \"name\"\n[units]\nby = \"file\"\n[completeness]\nnumber = '^page_(?P<n>\\d+)'\n";
    let (ex_ord, rep_ord) = extract_dir(&ord_dir, name_rule);
    assert!(!rep_ord.ordered);
    assert!(
        ex_ord.provenance.fallbacks.iter().any(
            |f| f.kind == fallback_kinds::RULE_ORDER && f.detail.as_deref() == Some("1, 10, 2")
        )
    );

    // M03 / M04 / M22: duplicate unit numbers.
    let dup_dir = Dir::new();
    dup_dir
        .write("01-1.txt", "Part 1\n\nBody 1a.\n")
        .write("01-2.txt", "Part 2\n\nBody 1b.\n")
        .write("02.txt", "Ch 2\n\nBody 2.\n");
    let (ex_dup, rep_dup) = extract_dir(&dup_dir, BY_FILE);
    assert_eq!(rep_dup.duplicates, vec![1]);
    assert!(
        ex_dup
            .provenance
            .fallbacks
            .iter()
            .any(|f| f.kind == fallback_kinds::RULE_DUPLICATE && f.detail.as_deref() == Some("1"))
    );

    // M05: numbering starting at 0 is contiguous.
    let zero_dir = Dir::new();
    zero_dir
        .write("00.txt", "Prologue\n\nBody 0.\n")
        .write("01.txt", "Ch 1\n\nBody 1.\n");
    let (ex_zero, rep_zero) = extract_dir(&zero_dir, BY_FILE);
    assert_eq!(rep_zero.min, Some(0));
    assert_eq!(rep_zero.max, Some(1));
    assert!(rep_zero.gaps.is_empty());
    assert!(ex_zero.provenance.fallbacks.is_empty());

    // M06: unnumbered front/back matter ordered via explicit list.
    let m06_dir = Dir::new();
    for name in [
        "\u{5e8f}.txt",
        "\u{6954}\u{5b50}.txt",
        "01_\u{7b2c}\u{4e00}\u{7ae0}.txt",
        "\u{540e}\u{8bb0}.txt",
    ] {
        m06_dir.write(
            name,
            std::fs::read(rule_fixture(&format!("m06-unnumbered/{name}"))).expect("m06"),
        );
    }
    let m06_rule = "schema = 1\n[members]\ninclude = [\"*.txt\"]\norder = \"explicit\"\nexplicit = [\"\u{5e8f}.txt\", \"\u{6954}\u{5b50}.txt\", \"01_\u{7b2c}\u{4e00}\u{7ae0}.txt\", \"\u{540e}\u{8bb0}.txt\"]\n[units]\nby = \"file\"\n[completeness]\nexpect = \"none\"\n";
    let (ex_m06, _) = extract_dir(&m06_dir, m06_rule);
    let m06_titles: Vec<&str> = ex_m06
        .toc
        .entries
        .iter()
        .map(|e| e.label.as_str())
        .collect();
    assert_eq!(
        m06_titles,
        vec![
            "\u{5e8f}",
            "\u{6954}\u{5b50}",
            "\u{7b2c}\u{4e00}\u{7ae0} \u{542f}\u{7a0b}",
            "\u{540e}\u{8bb0}"
        ]
    );

    // M17 & M18: volume subdirectories each starting at 01.
    let m18_dir = Dir::new();
    for vol in ["\u{5377}\u{4e00}", "\u{5377}\u{4e8c}", "\u{5377}\u{4e09}"] {
        let rel = format!("{vol}/01.txt");
        m18_dir.write(
            &rel,
            std::fs::read(rule_fixture(&format!("m18-volumes/{rel}"))).expect("m18"),
        );
    }
    let m18_natural = "schema = 1\n[members]\ninclude = [\"**/*.txt\"]\n[units]\nby = \"file\"\n";
    let (ex_m18_nat, rep_m18) = extract_dir(&m18_dir, m18_natural);
    assert_eq!(rep_m18.duplicates, vec![1]);
    assert!(
        ex_m18_nat
            .provenance
            .fallbacks
            .iter()
            .any(|f| f.kind == fallback_kinds::RULE_DUPLICATE)
    );

    let m18_explicit = "schema = 1\n[members]\ninclude = [\"**/*.txt\"]\norder = \"explicit\"\nexplicit = [\"\u{5377}\u{4e00}/01.txt\", \"\u{5377}\u{4e8c}/01.txt\", \"\u{5377}\u{4e09}/01.txt\"]\n[units]\nby = \"file\"\n[completeness]\nexpect = \"none\"\n";
    let (ex_m18_exp, _) = extract_dir(&m18_dir, m18_explicit);
    let m18_titles: Vec<&str> = ex_m18_exp
        .toc
        .entries
        .iter()
        .map(|e| e.label.as_str())
        .collect();
    assert_eq!(
        m18_titles,
        vec![
            "\u{5377}\u{4e00} \u{7b2c}\u{4e00}\u{7ae0}",
            "\u{5377}\u{4e8c} \u{7b2c}\u{4e00}\u{7ae0}",
            "\u{5377}\u{4e09} \u{7b2c}\u{4e00}\u{7ae0}",
        ]
    );

    // M24: marker-native `{0}----` pagination ingested with `by = "file"`.
    let m24_dir = Dir::new();
    m24_dir
        .write(
            "page_01.md",
            "{0}------------------------------------------------\n\nFirst page prose.\n",
        )
        .write(
            "page_02.md",
            "{1}------------------------------------------------\n\nSecond page prose.\n",
        );
    let m24_rule = "schema = 1\n[members]\ninclude = [\"page_*.md\"]\n[units]\nby = \"file\"\n[structure]\nunit_titles = \"none\"\nheadings = \"none\"\n[completeness]\nnumber = '^page_(?P<n>\\d+)'\n";
    let (ex_m24, rep_m24) = extract_dir(&m24_dir, m24_rule);
    assert_eq!(rep_m24.numbered, 2);
    assert!(rep_m24.gaps.is_empty());
    assert_eq!(ex_m24.blocks.len(), 4);
}

#[test]
fn static_bom_and_gb18030_fixtures_and_generated_crlf_variants() {
    // M07 / M11: static GB18030 member with non-ASCII byte precondition.
    let gb_bytes = std::fs::read(rule_fixture("m07-gb18030/02_gb18030.txt")).expect("read m07");
    assert!(
        std::str::from_utf8(&gb_bytes).is_err(),
        "GB18030 fixture must not be valid UTF-8"
    );
    let m07_dir = Dir::new();
    m07_dir
        .write(
            "01_utf8.txt",
            std::fs::read(rule_fixture("m07-gb18030/01_utf8.txt")).expect("read m07 01"),
        )
        .write("02_gb18030.txt", &gb_bytes);
    let (ex_m07, _) = extract_dir(&m07_dir, BY_FILE);
    assert_eq!(ex_m07.toc.entries.len(), 2);
    assert_eq!(
        ex_m07.toc.entries[1].label,
        "\u{7b2c}\u{4e8c}\u{7ae0} \u{591c}\u{96e8}"
    );
    assert!(
        ex_m07
            .provenance
            .fallbacks
            .iter()
            .any(|f| f.kind == fallback_kinds::TXT_GB18030)
    );

    // M08a: per-page BOM at start of non-first page file.
    let m08a_p2 = std::fs::read(rule_fixture("m08a-page-bom/page_002.md")).expect("m08a p2");
    assert!(m08a_p2.starts_with(b"\xEF\xBB\xBF"));
    let m08a_dir = Dir::new();
    for name in ["page_001.md", "page_002.md", "page_003.md"] {
        m08a_dir.write(
            name,
            std::fs::read(rule_fixture(&format!("m08a-page-bom/{name}"))).expect("m08a"),
        );
    }
    let marker_rule = "schema = 1\n[members]\ninclude = [\"page_*.md\"]\n[units]\nby = \"marker\"\nmarker = '^<!-- page (?P<label>\\S*) \\(sheet (?P<sheet>\\d+)\\) -->$'\n[structure]\nunit_titles = \"none\"\nheadings = \"none\"\n";
    let (ex_m08a, rep_m08a) = extract_dir(&m08a_dir, marker_rule);
    assert_eq!(ex_m08a.provenance.rule.as_ref().unwrap().units.len(), 3);
    assert!(rep_m08a.gaps.is_empty());

    // M08b: single file with BOM in the middle right before marker 2.
    let m08b_bytes = std::fs::read(rule_fixture("m08b-mid-bom/book.md")).expect("m08b");
    assert!(!m08b_bytes.starts_with(b"\xEF\xBB\xBF"));
    assert!(
        m08b_bytes
            .windows(b"\xEF\xBB\xBF<!-- page 2".len())
            .any(|w| w == b"\xEF\xBB\xBF<!-- page 2")
    );
    let m08b_dir = Dir::new();
    m08b_dir.write("book.md", &m08b_bytes);
    let m08b_rule = marker_rule.replace("page_*.md", "book.md");
    let (ex_m08b, rep_m08b) = extract_dir(&m08b_dir, &m08b_rule);
    assert_eq!(ex_m08b.provenance.rule.as_ref().unwrap().units.len(), 2);
    assert_eq!(rep_m08b.gaps, vec![2]);

    // M09 & M10: BOM on non-first member and BOM pair identity.
    let m09_p2 = std::fs::read(rule_fixture("m09-member-bom/02_two.txt")).expect("m09 p2");
    assert!(m09_p2.starts_with(b"\xEF\xBB\xBF"));
    let m09_dir = Dir::new();
    for name in ["01_one.txt", "02_two.txt"] {
        m09_dir.write(
            name,
            std::fs::read(rule_fixture(&format!("m09-member-bom/{name}"))).expect("m09"),
        );
    }
    let (ex_m09, _) = extract_dir(&m09_dir, BY_FILE);
    assert_eq!(ex_m09.toc.entries.len(), 2);
    assert_eq!(ex_m09.toc.entries[1].label, "Chapter 2");

    let clean_dir = Dir::new();
    let bom_dir = Dir::new();
    for name in ["01.txt", "02.txt"] {
        let c = std::fs::read(rule_fixture(&format!("m10-bom-pair/clean/{name}"))).expect("c");
        let b = std::fs::read(rule_fixture(&format!("m10-bom-pair/bom/{name}"))).expect("b");
        assert!(!c.starts_with(b"\xEF\xBB\xBF"));
        assert!(b.starts_with(b"\xEF\xBB\xBF"));
        clean_dir.write(name, c);
        bom_dir.write(name, b);
    }
    assert_eq!(
        read(&clean_dir, BY_FILE).1.source_sha256(),
        read(&bom_dir, BY_FILE).1.source_sha256()
    );

    // M12 & M19: generated in-test CRLF / LF mix and single-page CRLF.
    let crlf_page =
        b"<!-- page 2 (sheet 2) -->\r\n\r\nPara 2a line 1.\r\nPara 2a line 2.\r\n\r\nPara 2b.\r\n";
    assert!(crlf_page.windows(2).any(|w| w == b"\r\n"));
    let m19_dir = Dir::new();
    m19_dir
        .write("page_01.md", b"<!-- page 1 (sheet 1) -->\n\nPara 1.\n")
        .write("page_02.md", crlf_page);
    let (ex_m19, _) = extract_dir(&m19_dir, marker_rule);
    let m19_texts: Vec<&str> = ex_m19.blocks.iter().map(|b| b.text.as_str()).collect();
    assert_eq!(
        m19_texts,
        vec!["Para 1.", "Para 2a line 1. Para 2a line 2.", "Para 2b."]
    );
}

// ---- worker & unit error wording ------------------------------------------

#[test]
fn worker_and_unit_refusals_explain_themselves_in_three_parts() {
    // 1. Units: content before the first marker (names member and 1-based line).
    let pre_dir = Dir::new();
    pre_dir.write(
        "pages.md",
        "---\ntitle: Test\n---\n\nStray line before marker.\n<!-- page 1 (sheet 1) -->\nBody.\n",
    );
    let mrule = "schema = 1\n[members]\ninclude = [\"pages.md\"]\nfrontmatter = \"first\"\n[units]\nby = \"marker\"\nmarker = '^<!-- page (?P<label>\\S*) \\(sheet (?P<sheet>\\d+)\\) -->$'\n";
    let (pre_file, pre_src) = read(&pre_dir, mrule);
    let pre_err = pre_file
        .extract(&pre_src, &HeadingPatterns::default())
        .expect_err("content before first marker");
    match &pre_err {
        RuleError::Units { detail, .. } => {
            assert_names(detail, &["pages.md", "line 5"]);
        }
        other => panic!("expected Units error, got {other:?}"),
    }

    // 2. Units: no marker matched any line.
    let nom_dir = Dir::new();
    nom_dir.write("pages.md", "\n\n");
    let (nom_file, nom_src) = read(&nom_dir, mrule);
    let nom_err = nom_file
        .extract(&nom_src, &HeadingPatterns::default())
        .expect_err("no marker matched");
    assert!(matches!(nom_err, RuleError::Units { .. }), "{nom_err:?}");

    // 3. Completeness: fail_on_gap = true with gap and duplicate.
    let comp_dir = Dir::new();
    comp_dir
        .write("01-a.txt", "Ch 1a\n\nBody.\n")
        .write("01-b.txt", "Ch 1b\n\nBody.\n")
        .write("03.txt", "Ch 3\n\nBody.\n");
    let comp_rule = format!("{BY_FILE}\n[completeness]\nfail_on_gap = true\n");
    let (comp_file, comp_src) = read(&comp_dir, &comp_rule);
    let comp_err = comp_file
        .extract(&comp_src, &HeadingPatterns::default())
        .expect_err("completeness failure");
    assert!(
        matches!(comp_err, RuleError::Completeness { .. }),
        "{comp_err:?}"
    );

    // 4. EmptyExtraction (M13b): all members empty, or heading-only with no body blocks.
    let empty_dir = Dir::new();
    empty_dir.write("01.txt", "   \n").write("02.txt", "");
    let (empty_file, empty_src) = read(&empty_dir, BY_FILE);
    let empty_err = empty_file
        .extract(&empty_src, &HeadingPatterns::default())
        .expect_err("empty extraction");
    assert!(
        matches!(empty_err, RuleError::EmptyExtraction { .. }),
        "{empty_err:?}"
    );

    let head_only_dir = Dir::new();
    head_only_dir.write("01.txt", "Only A Heading\n");
    let (ho_file, ho_src) = read(&head_only_dir, BY_FILE);
    let ho_err = ho_file
        .extract(&ho_src, &HeadingPatterns::default())
        .expect_err("heading-only extraction has no body blocks");
    assert!(
        matches!(ho_err, RuleError::EmptyExtraction { .. }),
        "{ho_err:?}"
    );

    for (err, opening) in [
        (&pre_err, "cannot split \""),
        (&nom_err, "cannot split \""),
        (&comp_err, "units of \""),
        (&empty_err, "extraction of \""),
    ] {
        let problem = err.explain();
        assert!(problem.summary.starts_with(opening), "{problem:?}");
        assert!(!problem.summary.ends_with('.'), "{problem:?}");
        assert!(!problem.summary.contains('\n'), "{problem:?}");
        assert!(problem.data.hint.is_some(), "{problem:?}");
        assert!(!problem.data.retryable, "{problem:?}");
        assert!(!err.to_string().contains('\n'), "{err}");
    }
}
