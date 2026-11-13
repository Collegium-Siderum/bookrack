// SPDX-License-Identifier: Apache-2.0

//! The source half of rule intake, through its public entry points:
//! loading a rule file, finding its members, the canonical text, the
//! source hash, and the fingerprint.

use std::path::{Path, PathBuf};

use bookrack_core::Explain;
use bookrack_extract::fallback_kinds;
use bookrack_extract::rule::{MemberOrder, RuleError, RuleFile, RuleKind, RuleSource, UnitsBy};

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
