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
//!
//! TTY-ness decides only how long to wait, never whether to ask. A
//! terminal has a human reading the prompt, so the read is unbounded;
//! any other stdin is bounded by [`AnswerWindow`], because a pipe that
//! never closes may have nobody behind it. That keeps
//! `echo shelf | bookrack …`, `ssh host …` without `-t`, and Git Bash
//! on Windows — where `is_terminal` is false with a human at the
//! keyboard — able to answer, while an idle pipe stops the command
//! instead of parking it forever.

use bookrack_core::knob::{
    Candidate, DotenvSupply, KnobOrigin, KnobReach, Layer, ReadAt, env_over, resolve_knob,
};
use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// Environment knob overriding [`DEFAULT_CONFIRM_TIMEOUT_SECS`]. `0`
/// removes the bound and waits indefinitely on any stdin.
pub const CONFIRM_TIMEOUT_ENV: &str = "BOOKRACK_CONFIRM_TIMEOUT_SECS";

/// How long a non-terminal stdin may stay silent before a confirmation
/// gives up.
///
/// Sized for a human, not a machine. Over `ssh host …` without `-t` no
/// remote pty is allocated, so the command sees nothing until the
/// operator presses Enter — the window has to cover reading a
/// multi-line irreversible-deletion warning, deciding, and retyping a
/// library name in one stretch. Erring long is cheap: a machine that
/// was going to fail waits one more minute, while a window that is too
/// short throws away an answer a human was halfway through typing.
/// The ceiling is the daemon's 15-minute pinned-plan TTL, past which
/// an answer buys a plan-expired error instead of the work.
pub const DEFAULT_CONFIRM_TIMEOUT_SECS: u64 = 120;

/// Resolve the answer-window bound from the environment. `None` means
/// no bound. A malformed or empty value falls back to the default
/// rather than failing the command: a confirmation prompt is the wrong
/// place to litigate a typo in a knob.
pub fn confirm_bound_from(get: impl Fn(&str) -> Option<String>) -> Option<Duration> {
    let secs = get(CONFIRM_TIMEOUT_ENV)
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_CONFIRM_TIMEOUT_SECS);
    (secs > 0).then(|| Duration::from_secs(secs))
}

/// Every knob this module reads, with where the value came from.
pub fn knob_origins(dotenv: Option<DotenvSupply<'_>>) -> Vec<KnobOrigin> {
    knob_origins_from(|name| std::env::var(name).ok(), dotenv)
}

/// The same row on a machine where nothing is configured: the inventory
/// form, reporting [`DEFAULT_CONFIRM_TIMEOUT_SECS`] as the value and the
/// variable as the place it can be moved from.
pub fn knob_catalog() -> Vec<KnobOrigin> {
    knob_origins_from(|_| None, None)
}

/// Pure form of [`knob_origins`], sharing the parse rule with
/// [`confirm_bound_from`] rather than restating it.
fn knob_origins_from(
    get: impl Fn(&str) -> Option<String>,
    dotenv: Option<DotenvSupply<'_>>,
) -> Vec<KnobOrigin> {
    vec![resolve_knob(
        "confirm.timeout_secs",
        KnobReach::Process,
        ReadAt::PerCall,
        env_over(
            dotenv,
            CONFIRM_TIMEOUT_ENV,
            get(CONFIRM_TIMEOUT_ENV)
                .and_then(|raw| raw.trim().parse::<u64>().ok())
                .map(|v| v.to_string()),
            vec![Candidate::of(
                Layer::Default,
                "built-in",
                Some(DEFAULT_CONFIRM_TIMEOUT_SECS.to_string()),
            )],
        ),
    )]
}

/// How long an answer may take to arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnswerWindow {
    /// Wait as long as it takes.
    Unbounded,
    /// Give up after this long with nothing on the stream.
    Within(Duration),
}

/// Choose the window. A terminal has a human reading the prompt and
/// waits indefinitely; anything else is bounded, unless the operator
/// removed the bound. Pure, so both cases are reachable without a
/// terminal.
pub fn answer_window(stdin_is_tty: bool, bound: Option<Duration>) -> AnswerWindow {
    match bound {
        Some(bound) if !stdin_is_tty => AnswerWindow::Within(bound),
        _ => AnswerWindow::Unbounded,
    }
}

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
    /// The window expired with nothing on the stream.
    Silent(Duration),
    /// An earlier expired window abandoned the reader, so this
    /// process can no longer read stdin.
    Abandoned,
}

/// Why no answer could be obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoAnswer {
    /// The stream ended before the first byte arrived.
    EndOfStream,
    /// The window expired with nothing on the stream.
    Silent(Duration),
    /// An earlier expired window abandoned the reader.
    Abandoned,
}

impl NoAnswer {
    /// Operator-facing clause naming what went wrong, for the body of
    /// the error the caller returns.
    pub fn reason(self) -> String {
        match self {
            Self::EndOfStream => "stdin reached end of file before an answer arrived".to_string(),
            Self::Silent(window) => format!(
                "stdin carried no answer within {}s (set {CONFIRM_TIMEOUT_ENV} to change the \
                 window, or 0 to wait indefinitely)",
                window.as_secs()
            ),
            Self::Abandoned => {
                "stdin was abandoned by an earlier confirmation that went unanswered".to_string()
            }
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
                    reason: no_answer.reason(),
                    hint: hint.to_string(),
                })
            }
        }
    }

    /// Adapt to the `io::Result<bool>` seam
    /// [`bookrack_config::add_library`] takes for its manifest
    /// confirmation. `Unanswerable` crosses as an `io::Error` carrying
    /// the reason as its message, so the caller can recover it with
    /// [`Confirmation::no_answer_reason_from_io`] instead of losing
    /// the distinction at the crate boundary.
    pub fn into_io_result(self) -> io::Result<bool> {
        match self {
            Self::Agreed => Ok(true),
            Self::Declined => Ok(false),
            Self::Unanswerable(no_answer) => {
                let kind = match no_answer {
                    NoAnswer::EndOfStream => io::ErrorKind::UnexpectedEof,
                    NoAnswer::Silent(_) | NoAnswer::Abandoned => io::ErrorKind::TimedOut,
                };
                Err(io::Error::new(kind, no_answer.reason()))
            }
        }
    }

    /// Recover the reason an [`Confirmation::into_io_result`] error
    /// encoded, or `None` for any other I/O failure.
    pub fn no_answer_reason_from_io(error: &io::Error) -> Option<String> {
        matches!(
            error.kind(),
            io::ErrorKind::UnexpectedEof | io::ErrorKind::TimedOut
        )
        .then(|| error.to_string())
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
        Answer::Silent(window) => return Confirmation::Unanswerable(NoAnswer::Silent(*window)),
        Answer::Abandoned => return Confirmation::Unanswerable(NoAnswer::Abandoned),
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

/// Somewhere one answer can come from.
pub trait AnswerSource {
    /// Produce the next answer, giving up once `window` expires.
    fn next_answer(&mut self, window: AnswerWindow) -> io::Result<Answer>;
}

/// Any [`BufRead`] as an answer source. The window does not apply: a
/// caller-supplied stream is read to completion by whoever owns it.
pub struct StreamAnswers<R>(pub R);

impl<R: BufRead> AnswerSource for StreamAnswers<R> {
    fn next_answer(&mut self, _window: AnswerWindow) -> io::Result<Answer> {
        read_one_line(&mut self.0)
    }
}

fn read_one_line<R: BufRead>(input: &mut R) -> io::Result<Answer> {
    let mut line = String::new();
    match input.read_line(&mut line)? {
        0 => Ok(Answer::EndOfStream),
        _ => Ok(Answer::Line(line)),
    }
}

/// Set once a bounded read gave up on the process's real stdin.
static STDIN_ABANDONED: AtomicBool = AtomicBool::new(false);

/// Whether a bounded read has already given up on this process's
/// stdin. Once that happens the abandoned reader owns the stdin lock
/// for the life of the process, so nothing may read it again.
pub fn stdin_is_abandoned() -> bool {
    STDIN_ABANDONED.load(Ordering::SeqCst)
}

/// The process's real stdin.
///
/// Under [`AnswerWindow::Within`] the read runs on a detached thread,
/// because `Stdin::read_line` cannot be cancelled: an expired window
/// abandons that thread, and with it the process-global stdin lock,
/// which `std` implements as a plain non-reentrant mutex. A second
/// read would therefore block forever rather than fail, so the
/// abandonment is recorded and every later read answers
/// [`Answer::Abandoned`] instead of touching stdin. Callers turn that
/// into an error, so the process is on its way out either way.
///
/// The thread is a bare `std::thread`, never a `tokio` blocking task:
/// dropping a multi-thread runtime waits for its blocking pool to
/// drain, and a task parked in `read_line` never drains — which would
/// deadlock at exit, in exactly the case this bound exists to escape.
pub struct ProcessStdin;

impl AnswerSource for ProcessStdin {
    fn next_answer(&mut self, window: AnswerWindow) -> io::Result<Answer> {
        read_process_stdin(window, &STDIN_ABANDONED)
    }
}

/// [`ProcessStdin::next_answer`] over a caller-supplied abandonment
/// flag, so the already-abandoned branch is reachable from a test
/// without writing to the process-wide one.
fn read_process_stdin(window: AnswerWindow, abandoned: &AtomicBool) -> io::Result<Answer> {
    if abandoned.load(Ordering::SeqCst) {
        return Ok(Answer::Abandoned);
    }
    match window {
        AnswerWindow::Unbounded => read_one_line(&mut io::stdin().lock()),
        AnswerWindow::Within(bound) => read_stdin_within(bound, abandoned),
    }
}

fn read_stdin_within(bound: Duration, abandoned: &AtomicBool) -> io::Result<Answer> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("bookrack-confirm-stdin".to_string())
        .stack_size(64 * 1024)
        .spawn(move || {
            let _ = tx.send(read_one_line(&mut io::stdin().lock()));
        })?;

    // Drive against a deadline rather than a single duration, so an
    // early return re-arms on the time actually left.
    let deadline = Instant::now() + bound;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(answer) => return answer,
            Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => continue,
            Err(RecvTimeoutError::Timeout) => {
                abandoned.store(true, Ordering::SeqCst);
                return Ok(Answer::Silent(bound));
            }
            // The reader dropped its sender without answering, so the
            // read itself failed. Reporting that as "no answer" would
            // print the wrong remedy.
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other(
                    "the confirmation reader stopped before answering",
                ));
            }
        }
    }
}

/// Reads one line from stdin and decides whether the operator agreed.
/// The prompt goes to stderr so a piped stdout stays machine-readable.
/// A terminal is waited on indefinitely; any other stdin is bounded by
/// [`answer_window`].
pub fn confirm_destructive(
    prompt: &str,
    mode: ConfirmMode<'_>,
    assume_yes: bool,
) -> io::Result<Confirmation> {
    use std::io::IsTerminal;

    let window = answer_window(
        io::stdin().is_terminal(),
        confirm_bound_from(|key| std::env::var(key).ok()),
    );
    let stderr = io::stderr();
    confirm_destructive_via(
        &mut ProcessStdin,
        &mut stderr.lock(),
        prompt,
        mode,
        assume_yes,
        window,
    )
}

/// [`confirm_destructive`] over caller-supplied streams, read to
/// completion with no window.
pub fn confirm_destructive_from<R: BufRead, W: Write>(
    input: &mut R,
    out: &mut W,
    prompt: &str,
    mode: ConfirmMode<'_>,
    assume_yes: bool,
) -> io::Result<Confirmation> {
    confirm_destructive_via(
        &mut StreamAnswers(input),
        out,
        prompt,
        mode,
        assume_yes,
        AnswerWindow::Unbounded,
    )
}

/// [`confirm_destructive`] over a caller-supplied answer source and
/// window, so every outcome is reachable without a terminal.
///
/// `assume_yes` short-circuits before anything is touched: no prompt
/// is written and the source is not consulted. Otherwise `prompt` plus
/// a trailing space is written to `out` and flushed *before* the clock
/// starts, so a slow stderr does not eat the operator's window.
pub fn confirm_destructive_via<S: AnswerSource + ?Sized, W: Write>(
    source: &mut S,
    out: &mut W,
    prompt: &str,
    mode: ConfirmMode<'_>,
    assume_yes: bool,
    window: AnswerWindow,
) -> io::Result<Confirmation> {
    if assume_yes {
        return Ok(Confirmation::Agreed);
    }
    write!(out, "{prompt} ")?;
    out.flush()?;
    let answer = source.next_answer(window)?;
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
        for no_answer in [
            NoAnswer::EndOfStream,
            NoAnswer::Silent(Duration::from_secs(7)),
            NoAnswer::Abandoned,
        ] {
            let err = Confirmation::Unanswerable(no_answer)
                .into_io_result()
                .expect_err("an unanswerable prompt must not cross as a decline");
            assert_eq!(
                Confirmation::no_answer_reason_from_io(&err).as_deref(),
                Some(no_answer.reason().as_str()),
                "the reason must survive the crossing: {no_answer:?}"
            );
        }
        let unrelated = io::Error::other("disk on fire");
        assert_eq!(Confirmation::no_answer_reason_from_io(&unrelated), None);
    }

    /// An answer source that hands back one canned answer and records
    /// how it was asked. Standing in for real stdin keeps every window
    /// case reachable without a terminal and without a clock.
    struct ScriptedSource {
        canned: Answer,
        seen_window: Option<AnswerWindow>,
        calls: usize,
    }

    impl ScriptedSource {
        fn new(canned: Answer) -> Self {
            Self {
                canned,
                seen_window: None,
                calls: 0,
            }
        }
    }

    impl AnswerSource for ScriptedSource {
        fn next_answer(&mut self, window: AnswerWindow) -> io::Result<Answer> {
            self.calls += 1;
            self.seen_window = Some(window);
            Ok(self.canned.clone())
        }
    }

    fn via(
        canned: Answer,
        window: AnswerWindow,
        assume_yes: bool,
    ) -> (Confirmation, ScriptedSource) {
        let mut source = ScriptedSource::new(canned);
        let mut out: Vec<u8> = Vec::new();
        let verdict = confirm_destructive_via(
            &mut source,
            &mut out,
            "Type 'yes' to continue:",
            ConfirmMode::Soft,
            assume_yes,
            window,
        )
        .expect("the scripted source does not fail");
        (verdict, source)
    }

    /// A terminal has a human reading the prompt, so it waits. Any
    /// other stdin is bounded — but only because it might have nobody
    /// behind it, never because it is not a terminal: removing the
    /// bound puts a non-terminal back on an indefinite wait.
    #[test]
    fn only_a_non_terminal_with_a_bound_gets_a_window() {
        let bound = Duration::from_secs(90);
        assert_eq!(answer_window(true, Some(bound)), AnswerWindow::Unbounded);
        assert_eq!(answer_window(true, None), AnswerWindow::Unbounded);
        assert_eq!(answer_window(false, None), AnswerWindow::Unbounded);
        assert_eq!(
            answer_window(false, Some(bound)),
            AnswerWindow::Within(bound)
        );
    }

    #[test]
    fn the_bound_comes_from_the_environment_and_zero_removes_it() {
        let default = Some(Duration::from_secs(DEFAULT_CONFIRM_TIMEOUT_SECS));
        assert_eq!(confirm_bound_from(|_| None), default, "unset falls back");
        assert_eq!(
            confirm_bound_from(|_| Some("  ".to_string())),
            default,
            "blank falls back"
        );
        assert_eq!(
            confirm_bound_from(|_| Some("banana".to_string())),
            default,
            "a typo in a knob must not fail the confirmation"
        );
        assert_eq!(
            confirm_bound_from(|_| Some("45".to_string())),
            Some(Duration::from_secs(45))
        );
        assert_eq!(
            confirm_bound_from(|_| Some("0".to_string())),
            None,
            "zero is the documented way to wait indefinitely"
        );
        assert_eq!(
            confirm_bound_from(|key| (key == CONFIRM_TIMEOUT_ENV).then(|| "5".to_string())),
            Some(Duration::from_secs(5)),
            "the knob is read under its documented name"
        );
    }

    #[test]
    fn the_window_reaches_the_source_unchanged() {
        let bound = AnswerWindow::Within(Duration::from_secs(30));
        let (_, source) = via(Answer::Line("yes\n".into()), bound, false);
        assert_eq!(source.seen_window, Some(bound));
    }

    #[test]
    fn assume_yes_never_consults_the_source() {
        let (verdict, source) = via(
            Answer::Line("no\n".into()),
            AnswerWindow::Within(Duration::from_secs(1)),
            true,
        );
        assert_eq!(verdict, Confirmation::Agreed);
        assert_eq!(source.calls, 0, "--yes must not read the answer stream");
    }

    #[test]
    fn a_silent_or_abandoned_source_is_unanswerable() {
        let window = Duration::from_secs(3);
        let (verdict, _) = via(Answer::Silent(window), AnswerWindow::Within(window), false);
        assert_eq!(
            verdict,
            Confirmation::Unanswerable(NoAnswer::Silent(window))
        );
        let (verdict, _) = via(Answer::Abandoned, AnswerWindow::Within(window), false);
        assert_eq!(verdict, Confirmation::Unanswerable(NoAnswer::Abandoned));
    }

    /// The catalog row reports the compiled-in window and names the
    /// variable that moves it, which is what an inventory of this
    /// build's knobs has to say about a knob nobody has set.
    #[test]
    fn the_catalog_reports_the_compiled_in_window() {
        let rows = knob_catalog();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];

        assert_eq!(row.key, "confirm.timeout_secs");
        assert_eq!(row.layer, Layer::Default, "site: {}", row.site);
        assert_eq!(
            row.value.as_deref(),
            Some(DEFAULT_CONFIRM_TIMEOUT_SECS.to_string().as_str())
        );
        let sites: Vec<&str> = row.chain.iter().map(|s| s.site.as_str()).collect();
        assert!(sites.contains(&CONFIRM_TIMEOUT_ENV), "{sites:?}");
    }

    /// The reason has to tell the operator which knob widens the
    /// window, or a legitimately slow answer has no documented remedy.
    #[test]
    fn a_silent_reason_names_the_window_and_its_knob() {
        let reason = NoAnswer::Silent(Duration::from_secs(120)).reason();
        assert!(reason.contains("120s"), "{reason}");
        assert!(reason.contains(CONFIRM_TIMEOUT_ENV), "{reason}");
    }

    /// Once a window has expired, the abandoned reader owns the stdin
    /// lock for the life of the process. A later prompt must answer
    /// from the flag instead of queueing behind it, or the second
    /// destructive command in a process would block forever on a lock
    /// nobody will release. Driven against a local flag so the check
    /// holds even when the whole suite shares one process.
    #[test]
    fn an_abandoned_reader_answers_without_touching_stdin() {
        let abandoned = AtomicBool::new(true);
        for window in [
            AnswerWindow::Unbounded,
            AnswerWindow::Within(Duration::from_secs(60)),
        ] {
            // Reaching stdin here would block on the suite's own stdin;
            // returning at all is the assertion.
            let answer = read_process_stdin(window, &abandoned)
                .expect("an abandoned reader reports, it does not fail");
            assert_eq!(answer, Answer::Abandoned, "window={window:?}");
        }
        assert!(
            !stdin_is_abandoned(),
            "the local flag must not have leaked into the process-wide one"
        );
    }
}
