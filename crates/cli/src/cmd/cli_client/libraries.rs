//! `bookrack libraries {info,fork}` — control-plane wrapper.

use std::path::{Path, PathBuf};

use bookrack_cli::render::confirm::{ConfirmMode, Confirmation, confirm_destructive};
use bookrack_cli::render::ctx;
use bookrack_cli::render::table::{KvTable, flatten_into_kv};
use bookrack_runtime::cmd::libraries::CopyMode;
use eyre::Result;
use serde_json::{Value, json};

use crate::LibrariesAction;

use super::helpers;

/// Ask before forking, with the answer supplied by the caller so the
/// three outcomes are reachable without a terminal. `yes` short-circuits
/// before the prompt is built. Returns whether the operator agreed; a
/// stdin that carries no answer is a user error rather than a decline.
fn confirm_fork<F>(new_name: &str, data_dir: &Path, yes: bool, confirm: F) -> Result<bool>
where
    F: FnOnce(&str, ConfirmMode<'_>) -> std::io::Result<Confirmation>,
{
    if yes {
        return Ok(true);
    }
    let prompt = format!(
        "Fork library to '{new_name}' at {}? Type 'yes' to continue:",
        data_dir.display(),
    );
    let outcome = confirm(&prompt, ConfirmMode::Soft)
        .map_err(|e| eyre::eyre!("read fork confirmation: {e}"))?;
    Ok(outcome.agreed_or_refuse(
        "libraries fork",
        "re-run with --yes to fork without a prompt",
    )?)
}

pub async fn run(action: LibrariesAction, runtime_dir: Option<PathBuf>) -> Result<()> {
    let client = helpers::connect(runtime_dir.as_deref()).await?;
    match action {
        LibrariesAction::List { .. } => {
            // `list` renders the on-disk registry offline; `main`
            // dispatches it before reaching this daemon-routed path.
            unreachable!("libraries list is handled offline in main")
        }
        LibrariesAction::Info { name } => {
            let params = match name {
                Some(name) => json!({ "name": name }),
                None => Value::Null,
            };
            let response = helpers::dispatch(&client, "library.info", params).await?;
            if ctx().is_json() {
                helpers::print_value(&response);
                return Ok(());
            }
            if ctx().is_quiet() {
                return Ok(());
            }
            render_library_info(&response);
            Ok(())
        }
        LibrariesAction::Default { .. } => {
            // `libraries default` writes the registry offline; `main`
            // dispatches it before reaching this daemon-routed path.
            unreachable!("libraries default is handled offline in main")
        }
        LibrariesAction::Detect { .. } | LibrariesAction::Scan { .. } => {
            // `detect` / `scan` are read-only and resolve locally; `main`
            // dispatches them before reaching this daemon-routed path.
            unreachable!("libraries detect/scan are handled offline in main")
        }
        LibrariesAction::Add { .. }
        | LibrariesAction::Register { .. }
        | LibrariesAction::Remove { .. }
        | LibrariesAction::Config { .. } => {
            // `add` / `register` / `remove` / `config` edit the registry
            // or a root's `config.toml` offline; `main` dispatches them
            // before reaching this daemon path.
            unreachable!("libraries add/register/remove/config are handled offline in main")
        }
        LibrariesAction::Fork {
            new_name,
            data_dir,
            copy_mode,
            yes,
        } => {
            let confirmed = confirm_fork(&new_name, &data_dir, yes, |prompt, mode| {
                confirm_destructive(prompt, mode, false)
            })?;
            if !confirmed {
                eprintln!("aborted; no changes written");
                return Ok(());
            }
            let mode = match copy_mode {
                CopyMode::Hardlink => "hardlink",
                CopyMode::Copy => "copy",
            };
            let params = json!({
                "new_name": new_name.clone(),
                "data_dir": data_dir.clone(),
                "copy_mode": mode,
                "yes": true,
            });
            let response = helpers::dispatch(&client, "library.fork", params).await?;
            if ctx().is_json() {
                helpers::print_value(&response);
                return Ok(());
            }
            if ctx().is_quiet() {
                return Ok(());
            }
            println!(
                "Forked library to '{new_name}' at {} ({mode}).",
                data_dir.display()
            );
            Ok(())
        }
    }
}

fn render_library_info(response: &Value) {
    let mut table = KvTable::new();
    flatten_into_kv(&mut table, "", response);
    println!("{}", table.render());
}

#[cfg(test)]
mod tests {
    use super::*;
    use bookrack_cli::error::BookrackCliError;
    use bookrack_cli::render::confirm::NoAnswer;

    fn target() -> PathBuf {
        PathBuf::from("/data/copy")
    }

    #[test]
    fn yes_forks_without_asking() {
        let agreed = confirm_fork("copy", &target(), true, |_, _| {
            panic!("--yes must not prompt")
        })
        .expect("--yes confirms");
        assert!(agreed);
    }

    /// The prompt has to name what it is about to create, because the
    /// operator is agreeing to a second copy of a library, not to a
    /// generic action.
    #[test]
    fn the_prompt_names_the_fork_target() {
        let seen = std::cell::RefCell::new(None);
        let agreed = confirm_fork("copy", &target(), false, |text, mode| {
            *seen.borrow_mut() = Some(text.to_string());
            assert!(matches!(mode, ConfirmMode::Soft), "a fork takes a soft yes");
            Ok(Confirmation::Agreed)
        })
        .expect("an answered fork is not an error");
        assert!(agreed);
        let text = seen.into_inner().expect("the prompt ran");
        assert!(text.contains("copy"), "names the new library: {text}");
        assert!(text.contains("/data/copy"), "names the target: {text}");
    }

    #[test]
    fn a_declined_fork_is_a_clean_abort() {
        let agreed = confirm_fork("copy", &target(), false, |_, _| Ok(Confirmation::Declined))
            .expect("a declined fork is not an error");
        assert!(!agreed);
    }

    #[test]
    fn an_unanswered_fork_is_a_user_error() {
        let err = confirm_fork("copy", &target(), false, |_, _| {
            Ok(Confirmation::Unanswerable(NoAnswer::EndOfStream))
        })
        .expect_err("an unanswered fork is refused");
        let cli = err
            .downcast_ref::<BookrackCliError>()
            .expect("the refusal is a typed CLI error");
        assert_eq!(cli.exit_code(), 2);
        assert!(cli.to_string().contains("--yes"), "{cli}");
    }
}
