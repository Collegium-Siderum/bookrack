// SPDX-License-Identifier: Apache-2.0

//! `bookrack-sandbox`: run a `bookrack` binary under test inside a
//! persistent, isolated sandbox directory, with every command refused
//! by the tripwires before it can reach outside and every call leaving
//! a record.

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::Duration;

    use bookrack_test_support::tool::exec::{Capture, ExecOpts};
    use bookrack_test_support::tool::{
        CreateOpts, Outcome, Preset, Session, SessionOpts, Source, sig,
    };
    use clap::{Parser, Subcommand, ValueEnum};

    #[derive(Parser)]
    #[command(name = "bookrack-sandbox", version, about)]
    struct Cli {
        /// How long to wait for the sandbox's session lock, in seconds.
        #[arg(long, global = true, value_name = "SECS")]
        lock_wait: Option<u64>,
        /// Hold at a named point until the pause file is removed.
        #[arg(long, global = true, hide = true, value_name = "POINT")]
        pause_at: Option<String>,
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Clone, Copy, ValueEnum, Default)]
    enum PresetArg {
        #[default]
        Bare,
    }

    #[derive(Subcommand)]
    enum Command {
        /// Create a sandbox: build the binary, lay out the preset, take
        /// the first snapshot.
        New {
            dir: PathBuf,
            #[arg(long, value_enum, default_value_t)]
            preset: PresetArg,
            /// The workspace to build from; the git toplevel of the current
            /// directory when omitted.
            #[arg(long)]
            workspace: Option<PathBuf>,
            /// Rebuild an existing sandbox in place, keeping its call records.
            #[arg(long)]
            replace: bool,
            /// Copy this script in as the binary under test.
            #[arg(long, hide = true, value_name = "PATH")]
            stand_in: Option<PathBuf>,
        },
        /// Rebuild or re-copy the binary under test.
        Build { dir: PathBuf },
        /// Print the child's environment as shell lines.
        Env {
            dir: PathBuf,
            #[arg(long)]
            dotenv: bool,
        },
        /// Run one bookrack command inside the sandbox.
        Exec {
            dir: PathBuf,
            /// Keep the command's artifacts under cells/<id>/.
            #[arg(long, value_name = "ID")]
            cell: Option<String>,
            /// Deadline for the command, in seconds.
            #[arg(long, value_name = "SECS")]
            timeout: Option<u64>,
            /// A file under the sandbox to feed the command as stdin.
            #[arg(long, value_name = "FILE")]
            stdin: Option<PathBuf>,
            /// A file under the sandbox to place as the command's .env.
            #[arg(long, value_name = "FILE")]
            dotenv: Option<PathBuf>,
            /// The bookrack arguments.
            #[arg(last = true, required = true)]
            args: Vec<OsString>,
        },
        /// Run the tripwires for a command without running it.
        Tripwire {
            dir: PathBuf,
            #[arg(last = true)]
            args: Vec<OsString>,
        },
        /// Print a tree's signature.
        Sig { path: PathBuf },
        /// Print the difference between two trees' signatures.
        Diff { before: PathBuf, after: PathBuf },
        /// Copy host/ to golden/.
        Snapshot { dir: PathBuf },
        /// Copy golden/ back over host/.
        Restore { dir: PathBuf },
    }

    fn session_opts(cli: &Cli) -> SessionOpts {
        let mut opts = SessionOpts {
            pause_at: cli.pause_at.clone(),
            ..SessionOpts::default()
        };
        if let Some(secs) = cli.lock_wait {
            opts.lock_wait = Duration::from_secs(secs);
        }
        opts
    }

    /// The exit code, after the outcome has been printed.
    fn finish(outcome: Result<Outcome, Outcome>) -> i32 {
        let outcome = match outcome {
            Ok(outcome) | Err(outcome) => outcome,
        };
        outcome.eprint();
        outcome.exit_code()
    }

    async fn run(cli: Cli) -> i32 {
        let opts = session_opts(&cli);
        match cli.command {
            Command::New {
                dir,
                preset,
                workspace,
                replace,
                stand_in,
            } => {
                let source = match stand_in {
                    Some(path) => Source::StandIn(path),
                    None => Source::Cargo,
                };
                let preset = match preset {
                    PresetArg::Bare => Preset::Bare,
                };
                let created = Session::create(
                    &dir,
                    source,
                    preset,
                    CreateOpts { replace, workspace },
                    opts,
                )
                .await;
                finish(created.map(|_| Outcome::ok()))
            }
            Command::Build { dir } => match Session::open(&dir, opts) {
                Ok(mut session) => finish(session.build().await.map(|_| Outcome::ok())),
                Err(outcome) => finish(Err(outcome)),
            },
            Command::Env { dir, dotenv } => match Session::open(&dir, opts) {
                Ok(session) => match session.env_lines(dotenv).await {
                    Ok(lines) => {
                        print!("{lines}");
                        finish(Ok(Outcome::ok()))
                    }
                    Err(outcome) => finish(Err(outcome)),
                },
                Err(outcome) => finish(Err(outcome)),
            },
            Command::Exec {
                dir,
                cell,
                timeout,
                stdin,
                dotenv,
                args,
            } => match Session::open(&dir, opts) {
                Ok(mut session) => {
                    let mut exec_opts = ExecOpts {
                        cell,
                        stdin,
                        dotenv,
                        capture: Capture::Inherit,
                        ..ExecOpts::default()
                    };
                    if let Some(secs) = timeout {
                        exec_opts.timeout = Duration::from_secs(secs);
                    }
                    finish(session.exec(&args, exec_opts).await.map(|e| e.outcome))
                }
                Err(outcome) => finish(Err(outcome)),
            },
            Command::Tripwire { dir, args } => match Session::open(&dir, opts) {
                Ok(session) => match session.tripwire(&args).await {
                    Ok(findings) if findings.is_empty() => finish(Ok(Outcome::ok())),
                    Ok(findings) => {
                        for finding in &findings {
                            eprintln!("{finding}");
                        }
                        finish(Ok(bookrack_test_support::tool::Outcome::refused(
                            bookrack_test_support::tool::STAGE_TRIPWIRE,
                            bookrack_test_support::tool::problems::tripped(&findings),
                        )))
                    }
                    Err(outcome) => finish(Err(outcome)),
                },
                Err(outcome) => finish(Err(outcome)),
            },
            Command::Sig { path } => match sig::signature(&path) {
                Ok(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                    0
                }
                Err(err) => {
                    eprintln!("error: cannot sign {}", path.display());
                    eprintln!("detail: {err}");
                    bookrack_test_support::tool::EXIT_TOOL
                }
            },
            Command::Diff { before, after } => {
                match (sig::signature(&before), sig::signature(&after)) {
                    (Ok(a), Ok(b)) => {
                        for line in sig::diff(&a, &b) {
                            println!("{line}");
                        }
                        0
                    }
                    (Err(err), _) | (_, Err(err)) => {
                        eprintln!("error: cannot sign a tree to compare");
                        eprintln!("detail: {err}");
                        bookrack_test_support::tool::EXIT_TOOL
                    }
                }
            }
            Command::Snapshot { dir } => match Session::open(&dir, opts) {
                Ok(mut session) => finish(session.snapshot().await),
                Err(outcome) => finish(Err(outcome)),
            },
            Command::Restore { dir } => match Session::open(&dir, opts) {
                Ok(mut session) => finish(session.restore().await),
                Err(outcome) => finish(Err(outcome)),
            },
        }
    }

    pub fn main() {
        let cli = Cli::parse();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        let code = runtime.block_on(run(cli));
        // The record is written; a sink writer still stuck on a pipe goes
        // down with the process rather than holding it open.
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
        std::process::exit(code);
    }
}

#[cfg(unix)]
fn main() {
    unix::main();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("bookrack-sandbox runs on unix only");
    std::process::exit(125);
}
