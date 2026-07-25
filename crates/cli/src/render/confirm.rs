// SPDX-License-Identifier: Apache-2.0

//! Uniform destructive-action confirmation primitives.
//!
//! Two strengths: [`ConfirmMode::Soft`] accepts a case-insensitive
//! `yes` or `y`; [`ConfirmMode::Hard`] requires the operator to retype
//! a literal token (typically the object's name or an upper-case
//! sentinel such as `RESET`). The wrapper also honours an
//! `assume_yes` short-circuit that callers use to thread `--yes`
//! flags through without duplicating the read-stdin path.
//!
//! A confirmation has three outcomes, not two. Agreeing and declining
//! are both *answers* and both leave the operator in control; a stream
//! that carries no answer at all is neither. [`Confirmation`] keeps
//! that third case distinct so a caller cannot read a `/dev/null`
//! stdin as a considered "no" and report success for a command that
//! did nothing. Whether the answer arrives from a terminal or a pipe
//! is not part of the judgement: an answer is an answer.

use std::io::{self, BufRead, Write};

/// Confirmation strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmMode<'a> {
    /// Accept `yes` or `y` (case-insensitive).
    Soft,
    /// Require the operator to retype the given token verbatim.
    Hard { token: &'a str },
}

/// What arrived on the answer stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// One line, as read. Judged after trimming.
    Line(String),
    /// The stream ended before the first byte arrived.
    EndOfStream,
}

/// Why no answer could be obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoAnswer {
    /// The stream ended before the first byte arrived.
    EndOfStream,
}

impl NoAnswer {
    /// Operator-facing clause naming what went wrong, for the body of
    /// the error the caller returns.
    pub fn reason(self) -> &'static str {
        match self {
            Self::EndOfStream => "stdin reached end of file before an answer arrived",
        }
    }
}

/// What a confirmation prompt settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Confirmation {
    /// The operator agreed (or `--yes` vouched for them).
    Agreed,
    /// The operator answered, and the answer was not agreement.
    Declined,
    /// No answer could be obtained, so nothing was decided.
    Unanswerable(NoAnswer),
}

impl Confirmation {
    /// Collapse to "did the operator agree", turning an unanswerable
    /// prompt into the typed user error that carries `action` and
    /// `hint` to the operator. Every call site that needs a `bool`
    /// goes through here, so no site can quietly read "unanswerable"
    /// as "declined".
    pub fn agreed_or_refuse(
        self,
        action: &str,
        hint: &str,
    ) -> Result<bool, crate::error::BookrackCliError> {
        match self {
            Self::Agreed => Ok(true),
            Self::Declined => Ok(false),
            Self::Unanswerable(no_answer) => {
                Err(crate::error::BookrackCliError::ConfirmationUnanswerable {
                    action: action.to_string(),
                    reason: no_answer.reason().to_string(),
                    hint: hint.to_string(),
                })
            }
        }
    }

    /// Adapt to the `io::Result<bool>` seam
    /// [`bookrack_config::add_library`] takes for its manifest
    /// confirmation. `Unanswerable` crosses as an `io::Error` whose
    /// kind names the reason, so the caller can map it back to
    /// [`Confirmation::agreed_or_refuse`]'s error rather than losing
    /// the distinction at the crate boundary.
    pub fn into_io_result(self) -> io::Result<bool> {
        match self {
            Self::Agreed => Ok(true),
            Self::Declined => Ok(false),
            Self::Unanswerable(NoAnswer::EndOfStream) => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                NoAnswer::EndOfStream.reason(),
            )),
        }
    }

    /// Recover the [`NoAnswer`] an [`Confirmation::into_io_result`]
    /// error encoded, or `None` for any other I/O failure.
    pub fn no_answer_from_io(error: &io::Error) -> Option<NoAnswer> {
        match error.kind() {
            io::ErrorKind::UnexpectedEof => Some(NoAnswer::EndOfStream),
            _ => None,
        }
    }
}

/// Decide what an [`Answer`] means at the given strength. Pure: no
/// I/O, no clock, no terminal.
///
/// * [`ConfirmMode::Soft`] accepts `yes` or `y` in any ASCII case and
///   nothing else.
/// * [`ConfirmMode::Hard`] accepts only the token typed verbatim —
///   the comparison is case-sensitive.
///
/// Surrounding whitespace is trimmed first, so a line that carries
/// only a newline is an answer that declines. A stream that ended
/// before any byte arrived is not an answer at all.
pub fn judge(answer: &Answer, mode: ConfirmMode<'_>) -> Confirmation {
    let line = match answer {
        Answer::EndOfStream => return Confirmation::Unanswerable(NoAnswer::EndOfStream),
        Answer::Line(line) => line,
    };
    let entered = line.trim();
    let agreed = match mode {
        ConfirmMode::Soft => matches!(entered.to_ascii_lowercase().as_str(), "yes" | "y"),
        ConfirmMode::Hard { token } => entered == token,
    };
    if agreed {
        Confirmation::Agreed
    } else {
        Confirmation::Declined
    }
}

/// Reads one line from stdin and decides whether the operator agreed.
/// The prompt goes to stderr so a piped stdout stays machine-readable.
pub fn confirm_destructive(
    prompt: &str,
    mode: ConfirmMode<'_>,
    assume_yes: bool,
) -> io::Result<Confirmation> {
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
/// from `input`, and the result is handed to [`judge`].
pub fn confirm_destructive_from<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    prompt: &str,
    mode: ConfirmMode<'_>,
    assume_yes: bool,
) -> io::Result<Confirmation> {
    if assume_yes {
        return Ok(Confirmation::Agreed);
    }
    write!(out, "{prompt} ")?;
    out.flush()?;

    let mut line = String::new();
    let answer = match input.read_line(&mut line)? {
        0 => Answer::EndOfStream,
        _ => Answer::Line(line),
    };
    Ok(judge(&answer, mode))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run one answer through the real decision path and hand back the
    /// verdict plus whatever the prompt side wrote.
    fn answer(typed: &str, mode: ConfirmMode<'_>) -> (Confirmation, String) {
        let mut input = typed.as_bytes();
        let mut out: Vec<u8> = Vec::new();
        let verdict =
            confirm_destructive_from(&mut input, &mut out, "Type 'yes' to continue:", mode, false)
                .expect("in-memory streams do not fail");
        (verdict, String::from_utf8(out).expect("prompt is utf-8"))
    }

    fn soft(typed: &str) -> bool {
        answer(typed, ConfirmMode::Soft).0 == Confirmation::Agreed
    }

    fn hard(typed: &str, token: &str) -> bool {
        answer(typed, ConfirmMode::Hard { token }).0 == Confirmation::Agreed
    }

    #[test]
    fn assume_yes_short_circuits_both_modes() {
        assert_eq!(
            confirm_destructive("p", ConfirmMode::Soft, true).unwrap(),
            Confirmation::Agreed
        );
        assert_eq!(
            confirm_destructive("p", ConfirmMode::Hard { token: "RESET" }, true).unwrap(),
            Confirmation::Agreed
        );
    }

    #[test]
    fn assume_yes_neither_prompts_nor_reads_the_answer() {
        let mut input = b"no\n".as_slice();
        let mut out: Vec<u8> = Vec::new();
        assert_eq!(
            confirm_destructive_from(&mut input, &mut out, "prompt:", ConfirmMode::Soft, true)
                .unwrap(),
            Confirmation::Agreed
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

    /// A stream that ends before the first byte carries no answer, so
    /// it is neither consent nor a considered refusal. Callers turn
    /// this into a user error; reading it as a decline is what lets a
    /// `/dev/null` stdin report success for a command that did nothing.
    #[test]
    fn eof_before_any_byte_is_unanswerable() {
        for mode in [ConfirmMode::Soft, ConfirmMode::Hard { token: "RESET" }] {
            assert_eq!(
                answer("", mode).0,
                Confirmation::Unanswerable(NoAnswer::EndOfStream),
                "EOF is neither consent nor a decline: {mode:?}"
            );
        }
    }

    #[test]
    fn an_unterminated_answer_still_counts() {
        // A pipe that closes right after the token, with no newline,
        // reads as one line and must be judged on its contents. This
        // is the boundary that keeps "read zero bytes" distinct from
        // "read a line with no trailing newline".
        assert!(soft("yes"));
        assert!(hard("RESET", "RESET"));
    }

    /// A bare newline is an answer — an empty one — and answering
    /// declines. Only an empty *stream* is unanswerable.
    #[test]
    fn an_empty_line_declines_but_an_empty_stream_does_not() {
        assert_eq!(answer("\n", ConfirmMode::Soft).0, Confirmation::Declined);
        assert_eq!(
            answer("", ConfirmMode::Soft).0,
            Confirmation::Unanswerable(NoAnswer::EndOfStream)
        );
    }

    /// A Windows text pipe terminates lines with CRLF. The trim ahead
    /// of the comparison has to absorb the carriage return, or a Hard
    /// retype can never match on that platform.
    #[test]
    fn a_crlf_terminated_answer_is_judged_on_its_contents() {
        assert!(soft("yes\r\n"));
        assert!(hard("my-library\r\n", "my-library"));
        assert!(!soft("no\r\n"));
    }

    #[test]
    fn judge_maps_every_answer_shape() {
        use Confirmation::{Agreed, Declined, Unanswerable};

        let soft = ConfirmMode::Soft;
        let hard = ConfirmMode::Hard { token: "shelf" };
        let cases: &[(Answer, ConfirmMode<'_>, Confirmation)] = &[
            (Answer::Line("yes\n".into()), soft, Agreed),
            (Answer::Line("  Y  \n".into()), soft, Agreed),
            (Answer::Line("shelf\n".into()), soft, Declined),
            (Answer::Line("\n".into()), soft, Declined),
            (
                Answer::EndOfStream,
                soft,
                Unanswerable(NoAnswer::EndOfStream),
            ),
            (Answer::Line("shelf\n".into()), hard, Agreed),
            (Answer::Line("SHELF\n".into()), hard, Declined),
            (Answer::Line("yes\n".into()), hard, Declined),
            (
                Answer::EndOfStream,
                hard,
                Unanswerable(NoAnswer::EndOfStream),
            ),
        ];
        for (answer, mode, expected) in cases {
            assert_eq!(judge(answer, *mode), *expected, "{answer:?} at {mode:?}");
        }
    }

    /// The three outcomes must stay distinguishable across the
    /// `io::Result<bool>` seam `bookrack_config::add_library` takes,
    /// or the manifest-write confirmation loses the very distinction
    /// this module exists to keep.
    #[test]
    fn the_io_seam_round_trips_the_unanswerable_case() {
        assert!(Confirmation::Agreed.into_io_result().unwrap());
        assert!(!Confirmation::Declined.into_io_result().unwrap());
        let err = Confirmation::Unanswerable(NoAnswer::EndOfStream)
            .into_io_result()
            .expect_err("an unanswerable prompt must not cross as a decline");
        assert_eq!(
            Confirmation::no_answer_from_io(&err),
            Some(NoAnswer::EndOfStream)
        );
        let unrelated = io::Error::other("disk on fire");
        assert_eq!(Confirmation::no_answer_from_io(&unrelated), None);
    }
}
