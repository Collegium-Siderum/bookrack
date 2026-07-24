// SPDX-License-Identifier: Apache-2.0

//! Uniform destructive-action confirmation primitives.
//!
//! Two strengths: [`ConfirmMode::Soft`] accepts a case-insensitive
//! `yes` or `y`; [`ConfirmMode::Hard`] requires the operator to retype
//! a literal token (typically the object's name or an upper-case
//! sentinel such as `RESET`). The wrapper also honours an
//! `assume_yes` short-circuit that callers use to thread `--yes`
//! flags through without duplicating the read-stdin path.

use std::io::{self, BufRead, Write};

/// Confirmation strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmMode<'a> {
    /// Accept `yes` or `y` (case-insensitive).
    Soft,
    /// Require the operator to retype the given token verbatim.
    Hard { token: &'a str },
}

/// Reads one line from stdin and decides whether the operator agreed.
/// Returns `Ok(false)` on an empty line, EOF, or a mismatched token.
/// The prompt goes to stderr so a piped stdout stays machine-readable.
pub fn confirm_destructive(
    prompt: &str,
    mode: ConfirmMode<'_>,
    assume_yes: bool,
) -> io::Result<bool> {
    let stdin = io::stdin();
    let stderr = io::stderr();
    confirm_destructive_from(
        &mut stdin.lock(),
        &mut stderr.lock(),
        prompt,
        mode,
        assume_yes,
    )
}

/// [`confirm_destructive`] over caller-supplied streams.
///
/// `assume_yes` short-circuits before either stream is touched: no
/// prompt is written and `input` is not read. Otherwise `prompt` plus a
/// trailing space is written to `out` and flushed, one line is read
/// from `input`, and the answer is trimmed of surrounding whitespace
/// before it is judged:
///
/// * [`ConfirmMode::Soft`] accepts `yes` or `y` in any ASCII case and
///   nothing else.
/// * [`ConfirmMode::Hard`] accepts only the token typed verbatim —
///   the comparison is case-sensitive.
///
/// An empty line or an EOF before any byte arrives yields `false`.
pub fn confirm_destructive_from<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    prompt: &str,
    mode: ConfirmMode<'_>,
    assume_yes: bool,
) -> io::Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    write!(out, "{prompt} ")?;
    out.flush()?;

    let mut line = String::new();
    let read = input.read_line(&mut line)?;
    if read == 0 {
        return Ok(false);
    }
    let entered = line.trim();
    Ok(match mode {
        ConfirmMode::Soft => matches!(entered.to_ascii_lowercase().as_str(), "yes" | "y"),
        ConfirmMode::Hard { token } => entered == token,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run one answer through the real decision path and hand back the
    /// verdict plus whatever the prompt side wrote.
    fn answer(typed: &str, mode: ConfirmMode<'_>) -> (bool, String) {
        let mut input = typed.as_bytes();
        let mut out: Vec<u8> = Vec::new();
        let verdict =
            confirm_destructive_from(&mut input, &mut out, "Type 'yes' to continue:", mode, false)
                .expect("in-memory streams do not fail");
        (verdict, String::from_utf8(out).expect("prompt is utf-8"))
    }

    fn soft(typed: &str) -> bool {
        answer(typed, ConfirmMode::Soft).0
    }

    fn hard(typed: &str, token: &str) -> bool {
        answer(typed, ConfirmMode::Hard { token }).0
    }

    #[test]
    fn assume_yes_short_circuits_both_modes() {
        assert!(confirm_destructive("p", ConfirmMode::Soft, true).unwrap());
        assert!(confirm_destructive("p", ConfirmMode::Hard { token: "RESET" }, true).unwrap());
    }

    #[test]
    fn assume_yes_neither_prompts_nor_reads_the_answer() {
        let mut input = b"no\n".as_slice();
        let mut out: Vec<u8> = Vec::new();
        assert!(
            confirm_destructive_from(&mut input, &mut out, "prompt:", ConfirmMode::Soft, true)
                .unwrap()
        );
        assert!(out.is_empty(), "--yes must not write a prompt");
        assert_eq!(
            input, b"no\n",
            "--yes must not consume the answer stream: a later reader still sees it"
        );
    }

    #[test]
    fn the_prompt_is_written_and_flushed_before_the_read() {
        let (_, written) = answer("yes\n", ConfirmMode::Soft);
        assert_eq!(written, "Type 'yes' to continue: ");
    }

    #[test]
    fn soft_accepts_yes_and_y_in_any_case() {
        for typed in ["yes\n", "y\n", "YES\n", "Y\n", "Yes\n", "  yes  \n", "yes"] {
            assert!(soft(typed), "Soft should accept {typed:?}");
        }
    }

    #[test]
    fn soft_rejects_everything_else() {
        for typed in ["no\n", "n\n", "\n", "ye\n", "yes please\n", "yess\n", "1\n"] {
            assert!(!soft(typed), "Soft should reject {typed:?}");
        }
    }

    #[test]
    fn hard_accepts_only_the_token_retyped_verbatim() {
        assert!(hard("RESET\n", "RESET"));
        assert!(hard("  RESET  \n", "RESET"), "surrounding space is trimmed");
        assert!(hard("my-library\n", "my-library"));
    }

    #[test]
    fn hard_is_case_sensitive_and_ignores_the_soft_vocabulary() {
        for typed in ["reset\n", "Reset\n", "rESET\n"] {
            assert!(!hard(typed, "RESET"), "Hard must not fold case: {typed:?}");
        }
        for typed in ["yes\n", "y\n", "\n"] {
            assert!(
                !hard(typed, "RESET"),
                "Hard must not accept the Soft vocabulary: {typed:?}"
            );
        }
        assert!(
            !hard("RESET now\n", "RESET"),
            "a token with trailing words is not the token"
        );
        assert!(
            !hard("my-librar\n", "my-library"),
            "a truncated token is not the token"
        );
    }

    #[test]
    fn eof_before_any_byte_declines() {
        assert!(!soft(""), "Soft: EOF is not consent");
        assert!(!hard("", "RESET"), "Hard: EOF is not consent");
    }

    #[test]
    fn an_unterminated_answer_still_counts() {
        // A pipe that closes right after the token, with no newline,
        // reads as one line and must be judged on its contents.
        assert!(soft("yes"));
        assert!(hard("RESET", "RESET"));
    }
}
