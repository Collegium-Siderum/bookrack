// SPDX-License-Identifier: Apache-2.0

//! Member patterns over relative paths, the directory walk they are
//! matched against, and the natural order of the result.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use regex::Regex;

/// Compile a member pattern into a regex anchored at both ends of a
/// `/`-separated relative path.
///
/// `**/` matches zero or more directories and a bare `**` any run of
/// characters; `*` matches a run without `/` and `?` one character
/// other than `/`; `[...]` is a character class, negated by a leading
/// `!`. Every other character matches itself.
pub(super) fn compile(pattern: &str) -> Result<Regex, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut re = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' if chars.get(i + 1) == Some(&'*') => {
                if chars.get(i + 2) == Some(&'/') {
                    re.push_str("(?:.*/)?");
                    i += 3;
                } else {
                    re.push_str(".*");
                    i += 2;
                }
                continue;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '[' => {
                let Some(len) = chars[i..].iter().position(|&c| c == ']') else {
                    return Err(String::from("a `[` character class is never closed"));
                };
                let class: String = chars[i + 1..i + len].iter().collect();
                re.push('[');
                match class.strip_prefix('!') {
                    Some(negated) => {
                        re.push('^');
                        re.push_str(negated);
                    }
                    None => re.push_str(&class),
                }
                re.push(']');
                i += len + 1;
                continue;
            }
            other => re.push_str(&regex::escape(other.encode_utf8(&mut [0; 4]))),
        }
        i += 1;
    }
    re.push('$');
    Regex::new(&re).map_err(|e| e.to_string())
}

/// Every regular file under `root`, as its `/`-separated path relative
/// to `root` and the path to read it by, sorted by the relative path.
///
/// Symbolic links are followed. A directory reached a second time,
/// through a link back into the tree, is not entered again. The error
/// carries the path whose read failed.
pub(super) fn files_under(
    root: &Path,
) -> Result<Vec<(String, PathBuf)>, (PathBuf, std::io::Error)> {
    let mut files = Vec::new();
    let mut entered = BTreeSet::new();
    let mut pending = vec![(String::new(), root.to_path_buf())];
    while let Some((prefix, dir)) = pending.pop() {
        let fail = |e| (dir.clone(), e);
        if !entered.insert(std::fs::canonicalize(&dir).map_err(fail)?) {
            continue;
        }
        for entry in std::fs::read_dir(&dir).map_err(fail)? {
            let entry = entry.map_err(fail)?;
            let path = entry.path();
            let name = entry.file_name();
            let relative = format!("{prefix}{}", name.to_string_lossy());
            let meta = std::fs::metadata(&path).map_err(|e| (path.clone(), e))?;
            if meta.is_dir() {
                pending.push((format!("{relative}/"), path));
            } else if meta.is_file() {
                files.push((relative, path));
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Natural order: a run of ASCII digits compares by its numeric value,
/// every other character by code point. Two runs of equal value order
/// the one with fewer leading zeros first, so the order is total.
pub(super) fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let mut run_a = String::new();
                while let Some(c) = x.next_if(char::is_ascii_digit) {
                    run_a.push(c);
                }
                let mut run_b = String::new();
                while let Some(c) = y.next_if(char::is_ascii_digit) {
                    run_b.push(c);
                }
                let (value_a, value_b) =
                    (run_a.trim_start_matches('0'), run_b.trim_start_matches('0'));
                let order = value_a
                    .len()
                    .cmp(&value_b.len())
                    .then_with(|| value_a.cmp(value_b))
                    .then_with(|| run_a.len().cmp(&run_b.len()));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(p), Some(q)) => {
                if p != q {
                    return p.cmp(&q);
                }
                x.next();
                y.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(pattern: &str, path: &str) -> bool {
        compile(pattern).expect("pattern compiles").is_match(path)
    }

    #[test]
    fn a_star_stays_inside_one_directory() {
        assert!(matches("chapters/*.txt", "chapters/01.txt"));
        assert!(!matches("chapters/*.txt", "chapters/part/01.txt"));
        assert!(!matches("*.txt", "chapters/01.txt"));
    }

    #[test]
    fn a_double_star_descends_through_any_number_of_directories() {
        assert!(matches("**/*.txt", "01.txt"));
        assert!(matches("**/*.txt", "a/b/01.txt"));
        assert!(matches("vol/**", "vol/a/b.txt"));
        assert!(!matches("**/*.txt", "a/b/01.md"));
    }

    #[test]
    fn a_question_mark_is_one_character_and_never_a_separator() {
        assert!(matches("0?.txt", "01.txt"));
        assert!(!matches("0?.txt", "012.txt"));
        assert!(!matches("a?b", "a/b"));
    }

    #[test]
    fn a_class_matches_its_members_and_a_bang_negates_it() {
        assert!(matches("0[1-9].txt", "05.txt"));
        assert!(!matches("0[1-9].txt", "00.txt"));
        assert!(matches("[!0]1.txt", "11.txt"));
        assert!(!matches("[!0]1.txt", "01.txt"));
    }

    #[test]
    fn regex_metacharacters_in_a_pattern_are_literal() {
        assert!(matches("a.b(1)+.txt", "a.b(1)+.txt"));
        assert!(!matches("a.b", "axb"));
    }

    #[test]
    fn a_pattern_is_anchored_at_both_ends() {
        assert!(!matches("01.txt", "x01.txt"));
        assert!(!matches("01.txt", "01.txt.bak"));
    }

    #[test]
    fn an_unclosed_class_and_an_inverted_range_do_not_compile() {
        assert!(compile("0[1-9.txt").is_err());
        assert!(compile("[z-a].txt").is_err());
    }

    #[test]
    fn natural_order_compares_digit_runs_by_value() {
        let mut names = vec!["10_a", "2_a", "1_a", "page_010", "page_9", "page_10"];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            ["1_a", "2_a", "10_a", "page_9", "page_10", "page_010"]
        );
    }

    #[test]
    fn natural_order_is_total_over_names_that_differ_only_in_padding() {
        assert_eq!(natural_cmp("1", "01"), Ordering::Less);
        assert_eq!(natural_cmp("01", "1"), Ordering::Greater);
        assert_eq!(natural_cmp("01", "01"), Ordering::Equal);
        assert_eq!(natural_cmp("a1b2", "a01b1"), Ordering::Less);
    }
}
