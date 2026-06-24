// SPDX-License-Identifier: Apache-2.0

//! Confirm the bundled SQLite was built with the FTS5 module and its
//! trigram tokenizer is reachable.
//!
//! The `refs` crate uses FTS5 virtual tables with `tokenize='trigram'`
//! to give the reference-book lookup tools substring search over
//! Chinese strings (the default `unicode61` tokenizer treats a CJK run
//! as one token, so substring queries return nothing). This test
//! exercises the smallest end-to-end path that proves both the FTS5
//! module and the trigram tokenizer are linked.

use rusqlite::Connection;

#[test]
fn fts5_trigram_substring_search_works() {
    let con = Connection::open_in_memory().expect("open in-memory db");

    // Compiled SQLite must report FTS5 in its option list.
    let has_fts5: bool = con
        .query_row(
            "SELECT count(*) > 0 FROM pragma_compile_options WHERE compile_options = 'ENABLE_FTS5'",
            [],
            |row| row.get(0),
        )
        .expect("query compile options");
    assert!(has_fts5, "rusqlite was not built with the FTS5 feature");

    // CJK literals are encoded as \u{...} per the leak-check rule
    // (English-only source outside test fixture directories). The
    // payloads are synthetic CJK strings; the match probe is a 3-char
    // substring of the second row's CJK run, which the default
    // unicode61 tokenizer would miss and trigram catches.
    let en1 = "Aaron, Hank";
    let zh1 = "\u{827E}\u{4F26}, \u{68D2}\u{7403}\u{8FD0}\u{52A8}\u{5458}";
    let en2 = "M\u{00FC}ller";
    let zh2 = "\u{5F25}\u{52D2}, \u{81EA}\u{7136}\u{54F2}\u{5B66}\u{5BB6}";
    let probe_cjk = "\u{8FD0}\u{52A8}\u{5458}";

    con.execute(
        "CREATE VIRTUAL TABLE t USING fts5(en, zh, tokenize='trigram')",
        [],
    )
    .expect("create fts5 trigram table");
    con.execute("INSERT INTO t(en, zh) VALUES (?1, ?2)", (en1, zh1))
        .expect("insert row 1");
    con.execute("INSERT INTO t(en, zh) VALUES (?1, ?2)", (en2, zh2))
        .expect("insert row 2");

    let count_three_char_cjk: i64 = con
        .query_row(
            "SELECT count(*) FROM t WHERE t MATCH ?1",
            [probe_cjk],
            |row| row.get(0),
        )
        .expect("query trigram on a 3-char CJK substring");
    assert_eq!(
        count_three_char_cjk, 1,
        "trigram tokenizer should match a 3-char CJK substring inside a longer CJK run"
    );

    let count_latin: i64 = con
        .query_row("SELECT count(*) FROM t WHERE t MATCH 'Aaron'", [], |row| {
            row.get(0)
        })
        .expect("query trigram on a latin word");
    assert_eq!(
        count_latin, 1,
        "trigram tokenizer should match a latin word"
    );
}
