// SPDX-License-Identifier: Apache-2.0

//! The plan printed as shell lines, for someone debugging by hand.

use std::ffi::OsStr;

use bookrack_config::NO_DOTENV_ENV;

use crate::spawn::EnvPlan;

/// `unset`, `export` and `cd` lines reproducing `plan` in a POSIX shell.
/// Values are single-quoted. With `dotenv`, the dotenv switch is left
/// out and a note says where to put the file.
pub fn shell_lines(plan: &EnvPlan, dotenv: bool) -> String {
    let mut out = String::new();
    for key in &plan.remove {
        out.push_str(&format!("unset {}\n", key.to_string_lossy()));
    }
    for (key, value) in &plan.set {
        if dotenv && key == OsStr::new(NO_DOTENV_ENV) {
            continue;
        }
        out.push_str(&format!(
            "export {}={}\n",
            key.to_string_lossy(),
            quote(&value.to_string_lossy())
        ));
    }
    out.push_str(&format!("cd {}\n", quote(&plan.cwd.to_string_lossy())));
    if dotenv {
        out.push_str(&format!(
            "# dotenv run: copy the .env into {} and remove it afterwards\n",
            quote(&plan.cwd.join(".env").to_string_lossy())
        ));
    }
    out.push_str(
        "# a scripted run goes through 'bookrack-sandbox exec', which also runs the tripwires\n",
    );
    out
}

/// `value` in single quotes, with embedded single quotes closed,
/// escaped and reopened.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_single_quoted_and_quotes_escaped() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }
}
