// SPDX-License-Identifier: Apache-2.0

//! Carries the invocation's `--library` onto the wire.
//!
//! A control-plane method either takes a library selection or it does
//! not, and which one it is belongs to the handler that reads the
//! parameter. The client therefore asks the runtime's method registry
//! — [`library_key_for`] and [`refuses_library`] — rather than keeping
//! a list of its own that would have to be corrected twice.
//!
//! Three outcomes, one per row of that registry:
//!
//! * a method that takes a selection gets it injected under the key it
//!   declares (`library` everywhere but `library.info`, whose own
//!   `name` parameter predates the shared spelling);
//! * a method that describes the process rather than a library —
//!   `daemon.*`, the queue verbs, `logs.tail`, `diagnose.run` — is left
//!   alone: the selection is meaningless there, and meaningless is not
//!   the same as wrong;
//! * a method that answers about the daemon or about every library at
//!   once is **refused**. Sending the call and letting the selection
//!   evaporate would answer a question the operator did not ask.
//!
//! A selection the caller wrote out by hand always wins: the injection
//! only fills a key that is absent, so `bookrack rpc call` stays an
//! escape hatch rather than a surface with opinions.

use std::sync::OnceLock;

use bookrack_config::LibrarySelection;
use bookrack_core::{Problem, ProblemData};
use bookrack_runtime::control::methods::{library_key_for, refuses_library};
use serde_json::Value;

use crate::error::BookrackCliError;

static PENDING: OnceLock<LibrarySelection> = OnceLock::new();
static SELECTED: OnceLock<Option<String>> = OnceLock::new();

/// Record what a daemon-routed invocation selected, before anything is
/// known about which library that is. Installed at startup; read once a
/// connection exists, by the client code that settles it into a name.
///
/// A locally resolving command installs nothing: its selection never
/// travels.
pub fn init_pending(selection: LibrarySelection) {
    let _ = PENDING.set(selection);
}

/// The selection waiting to be settled, or `None` when this invocation
/// installed none.
pub fn pending() -> Option<&'static LibrarySelection> {
    PENDING.get()
}

/// Install the library this invocation names, once, before any call
/// goes out. A second call is ignored, mirroring [`crate::render::init`]:
/// the selection is a property of the process, not of a call site.
///
/// A selection expressed as a path arrives here already translated, or
/// as `None` when the root it names belongs to no registry entry and
/// the daemon serving it needs no name to find it. The translation is
/// [`crate::path_sugar`]; it happens once a connection exists, because
/// its last question — is this the root you serve? — is the daemon's to
/// answer.
pub fn init(selected: Option<String>) {
    let _ = SELECTED.set(selected);
}

/// The library this invocation acts on, or `None` when it names none.
pub fn selected() -> Option<&'static str> {
    SELECTED.get().and_then(|s| s.as_deref())
}

/// Apply the process-wide selection to one outgoing call.
pub fn apply(method: &str, params: Value) -> Result<Value, BookrackCliError> {
    apply_selection(selected(), method, params)
}

/// [`apply`] against an explicit selection rather than the global one.
pub fn apply_selection(
    selected: Option<&str>,
    method: &str,
    mut params: Value,
) -> Result<Value, BookrackCliError> {
    let Some(name) = selected else {
        return Ok(params);
    };
    if refuses_library(method) {
        return Err(BookrackCliError::LibraryNotRoutable {
            problem: not_routable(name, method),
        });
    }
    let Some(key) = library_key_for(method) else {
        return Ok(params);
    };
    if params.is_null() {
        params = Value::Object(serde_json::Map::new());
    }
    let Some(object) = params.as_object_mut() else {
        // Neither an object nor absent: the daemon rejects the shape
        // itself, and rewriting it here would replace that report with
        // a different one.
        return Ok(params);
    };
    object
        .entry(key)
        .or_insert_with(|| Value::String(name.to_string()));
    Ok(params)
}

/// The refusal a method that cannot take a selection earns.
///
/// The hint names commands rather than RPC methods: an operator asked
/// to run something can run a command, and the method name is a detail
/// of how the command gets there.
fn not_routable(name: &str, method: &str) -> Problem {
    let alternative = match method {
        "status" | "daemon.status" => {
            "Run `bookrack libraries info --name <library>` for one library's counts, \
             or drop the flag to report on the daemon."
        }
        "library.list" => {
            "`bookrack libraries list` reports every library the daemon serves; \
             drop the flag."
        }
        "doctor.gather" => {
            "Stop the daemon and re-run `bookrack doctor` to diagnose another \
             library's data root, or drop the flag to diagnose the running one."
        }
        _ => "Drop the flag: this command does not act on a single library.",
    };
    Problem {
        summary: format!("cannot run this command against library '{name}'"),
        data: ProblemData {
            detail: Some(format!(
                "The `{method}` call behind it reports on the daemon itself, or on \
                 every library it serves, so it takes no library selection."
            )),
            hint: Some(alternative.to_string()),
            retryable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The key comes from the registry, not from this module:
    /// `library.info` selects under `name`, every other routed method
    /// under `library`. Asserting both directions here is what makes a
    /// hand-written key in the injector fail.
    #[test]
    fn injection_uses_the_key_the_registry_declares() {
        let params = apply_selection(Some("beta"), "library.stats", json!({})).unwrap();
        assert_eq!(params, json!({"library": "beta"}));

        let params = apply_selection(Some("beta"), "library.info", json!({})).unwrap();
        assert_eq!(params, json!({"name": "beta"}));
    }

    #[test]
    fn null_params_become_an_object_so_the_selection_survives() {
        let params = apply_selection(Some("beta"), "library.stats", Value::Null).unwrap();
        assert_eq!(params, json!({"library": "beta"}));
    }

    /// `rpc call` hands through whatever the operator typed. A key
    /// already in the params is their decision and outranks the flag.
    #[test]
    fn a_hand_written_key_outranks_the_flag() {
        let params =
            apply_selection(Some("beta"), "library.stats", json!({"library": "alpha"})).unwrap();
        assert_eq!(params, json!({"library": "alpha"}));
    }

    /// Process-facing methods are left exactly as they were: no
    /// injection, no refusal.
    #[test]
    fn process_methods_pass_through_untouched() {
        for method in ["diagnose.run", "queue.list", "daemon.version", "logs.tail"] {
            let params = apply_selection(Some("beta"), method, json!({"n": 5})).unwrap();
            assert_eq!(params, json!({"n": 5}), "{method} must not be rewritten");
        }
    }

    /// The refusal is the half that keeps a selection from evaporating,
    /// so it is asserted on the two shapes it has to cover: a card
    /// about the daemon and a listing of every library.
    #[test]
    fn methods_that_cannot_route_are_refused_with_three_parts() {
        for method in ["status", "daemon.status", "library.list", "doctor.gather"] {
            let err = apply_selection(Some("beta"), method, json!({}))
                .expect_err("{method} must refuse an explicit selection");
            let BookrackCliError::LibraryNotRoutable { problem } = err else {
                panic!("{method} refused with the wrong variant");
            };
            assert!(
                problem.summary.contains("beta"),
                "{method} must name the library it cannot use: {}",
                problem.summary
            );
            assert!(
                problem.data.hint.is_some(),
                "{method} must say what to run instead"
            );
            assert!(
                !problem.summary.contains(method),
                "the summary states the failure, not the method behind it: {}",
                problem.summary
            );
        }
    }

    /// Without a `--library` nothing changes — including on the
    /// methods that would otherwise refuse.
    #[test]
    fn no_selection_leaves_every_method_alone() {
        for method in ["library.stats", "status", "diagnose.run"] {
            let params = apply_selection(None, method, json!({"a": 1})).unwrap();
            assert_eq!(params, json!({"a": 1}), "{method}");
        }
    }

    /// A name this build does not know is forwarded untouched: `rpc
    /// call` may address a method a newer daemon has, and inventing a
    /// parameter for it would put words in the caller's mouth.
    #[test]
    fn an_unknown_method_is_neither_injected_nor_refused() {
        let params = apply_selection(Some("beta"), "library.no_such_method", json!({})).unwrap();
        assert_eq!(params, json!({}));
    }
}
