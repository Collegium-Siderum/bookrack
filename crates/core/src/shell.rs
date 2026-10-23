// SPDX-License-Identifier: Apache-2.0

//! Quoting a value as one argument for a POSIX shell.
//!
//! Operator-facing hints print commands meant to be pasted into a
//! shell, and the paths in them come from the environment or from an
//! explicit argument, so they can carry spaces, quotes, or shell
//! metacharacters. [`quote_arg`] makes one such value a single word
//! the shell hands over unchanged. It targets POSIX shells only: the
//! quoting is not valid for `cmd.exe` or PowerShell.

use std::borrow::Cow;

/// Quote `arg` so that a POSIX shell reads it as exactly one word
/// carrying the original characters.
///
/// A non-empty value made of ASCII letters, digits, and
/// `_ . / : @ % + = , -` is returned as is. Anything else is wrapped in
/// single quotes, with each single quote inside written as `'\''`; the
/// empty string becomes `''`.
pub fn quote_arg(arg: &str) -> Cow<'_, str> {
    fn is_plain(c: char) -> bool {
        c.is_ascii_alphanumeric()
            || matches!(c, '_' | '.' | '/' | ':' | '@' | '%' | '+' | '=' | ',' | '-')
    }
    if !arg.is_empty() && arg.chars().all(is_plain) {
        return Cow::Borrowed(arg);
    }
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('\'');
    for c in arg.chars() {
        if c == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(c);
        }
    }
    quoted.push('\'');
    Cow::Owned(quoted)
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use super::quote_arg;

    #[test]
    fn a_plain_path_is_returned_unquoted() {
        let path = "/run/user-1000/bookrack.tty.lock";
        assert!(matches!(quote_arg(path), Cow::Borrowed(p) if p == path));
    }

    #[test]
    fn a_path_with_a_space_is_single_quoted() {
        assert_eq!(quote_arg("/run/book rack/lock"), "'/run/book rack/lock'");
    }

    #[test]
    fn a_single_quote_inside_closes_reopens_and_escapes() {
        assert_eq!(
            quote_arg("/run/book rack's/lock"),
            "'/run/book rack'\\''s/lock'"
        );
    }

    #[test]
    fn the_empty_string_is_an_empty_pair_of_quotes() {
        assert_eq!(quote_arg(""), "''");
    }

    #[test]
    fn dollar_and_backtick_are_quoted_literally() {
        assert_eq!(quote_arg("/run/$HOME/`x`"), "'/run/$HOME/`x`'");
    }
}
