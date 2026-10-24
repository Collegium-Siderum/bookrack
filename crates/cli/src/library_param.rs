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
//!   `daemon.*`, the queue verbs, `logs.tail` — is left alone: the
//!   selection is meaningless there, and meaningless is not the same
//!   as wrong;
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
static SELECTED: OnceLock<Option<Selected>> = OnceLock::new();

/// The library a daemon-routed invocation acts on, once settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selected {
    /// The name the daemon serves the library under: a registry name,
    /// or the in-memory key of a root selected by path.
    pub name: String,
    /// Whether the selection is the root the daemon came up under,
    /// reached by path. A process-level method reports on that very
    /// root, so the selection is honoured by leaving such a call alone
    /// rather than refusing it; a routed method still gets the name.
    pub bring_up_root: bool,
}

impl Selected {
    /// A selection by registry name, or by a path the registry claims.
    pub fn named(name: impl Into<String>) -> Selected {
        Selected {
            name: name.into(),
            bring_up_root: false,
        }
    }

    /// A path selection the daemon answered is the root it came up
    /// under, served as `name`.
    pub fn bring_up_root(name: impl Into<String>) -> Selected {
        Selected {
            name: name.into(),
            bring_up_root: true,
        }
    }
}

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
/// A selection expressed as a path arrives here already translated:
/// into the registry name that claims the root, or into the key the
/// daemon serves the root under when no entry claims it. The
/// translation is [`crate::path_sugar`] and the daemon's `status`; it
/// happens once a connection exists, because its last question — is
/// this a root you serve? — is the daemon's to answer.
pub fn init(selected: Option<Selected>) {
    let _ = SELECTED.set(selected);
}

/// The library this invocation acts on, or `None` when it names none.
pub fn selected() -> Option<&'static str> {
    selection().map(|s| s.name.as_str())
}

/// The settled selection, with how it was reached.
fn selection() -> Option<&'static Selected> {
    SELECTED.get().and_then(|s| s.as_ref())
}

/// Apply the process-wide selection to one outgoing call.
pub fn apply(method: &str, params: Value) -> Result<Value, BookrackCliError> {
    apply_selection(selection(), method, params)
}

/// [`apply`] against an explicit selection rather than the global one.
pub fn apply_selection(
    selected: Option<&Selected>,
    method: &str,
    mut params: Value,
) -> Result<Value, BookrackCliError> {
    let Some(selected) = selected else {
        return Ok(params);
    };
    let name = selected.name.as_str();
    if refuses_library(method) {
        // A process-level method reports on the daemon, and the daemon
        // is the one that came up on this root: the selection did not
        // fall away, so the call goes out as written.
        if selected.bring_up_root {
            return Ok(params);
        }
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
            "`bookrack libraries list` reports every registered library and marks \
             the ones the daemon serves; drop the flag."
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

    fn named(name: &str) -> Option<Selected> {
        Some(Selected::named(name))
    }

    /// The root the daemon came up under, selected by path, is the
    /// subject of a process-level method already: the call goes out as
    /// written. A routed method still gets the name, and a selection
    /// reached any other way is refused as before.
    #[test]
    fn the_bring_up_root_passes_process_methods_through_and_names_routed_ones() {
        let root = Selected::bring_up_root("default");
        let params = apply_selection(Some(&root), "status", json!({}))
            .expect("the bring-up root is what status reports on");
        assert_eq!(params, json!({}));

        let params = apply_selection(Some(&root), "library.info", json!({})).unwrap();
        assert_eq!(params, json!({"name": "default"}));

        apply_selection(Some(&Selected::named("default")), "status", json!({}))
            .expect_err("a selection reached by name is still refused on status");
    }

    /// The key comes from the registry, not from this module:
    /// `library.info` selects under `name`, every other routed method
    /// under `library`. Asserting both directions here is what makes a
    /// hand-written key in the injector fail.
    #[test]
    fn injection_uses_the_key_the_registry_declares() {
        let params = apply_selection(named("beta").as_ref(), "library.stats", json!({})).unwrap();
        assert_eq!(params, json!({"library": "beta"}));

        let params = apply_selection(named("beta").as_ref(), "library.info", json!({})).unwrap();
        assert_eq!(params, json!({"name": "beta"}));
    }

    #[test]
    fn null_params_become_an_object_so_the_selection_survives() {
        let params = apply_selection(named("beta").as_ref(), "library.stats", Value::Null).unwrap();
        assert_eq!(params, json!({"library": "beta"}));
    }

    /// `rpc call` hands through whatever the operator typed. A key
    /// already in the params is their decision and outranks the flag.
    #[test]
    fn a_hand_written_key_outranks_the_flag() {
        let params = apply_selection(
            named("beta").as_ref(),
            "library.stats",
            json!({"library": "alpha"}),
        )
        .unwrap();
        assert_eq!(params, json!({"library": "alpha"}));
    }

    /// Process-facing methods are left exactly as they were: no
    /// injection, no refusal.
    #[test]
    fn process_methods_pass_through_untouched() {
        for method in ["queue.list", "daemon.version", "logs.tail"] {
            let params = apply_selection(named("beta").as_ref(), method, json!({"n": 5})).unwrap();
            assert_eq!(params, json!({"n": 5}), "{method} must not be rewritten");
        }
    }

    /// The refusal is the half that keeps a selection from evaporating,
    /// so it is asserted on the two shapes it has to cover: a card
    /// about the daemon and a listing of every library.
    #[test]
    fn methods_that_cannot_route_are_refused_with_three_parts() {
        for method in ["status", "daemon.status", "library.list"] {
            let err = apply_selection(named("beta").as_ref(), method, json!({}))
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

    /// The two diagnostics gather one library's data among the
    /// process-wide rest, so a selection reaches them under `library`
    /// rather than being dropped or refused.
    #[test]
    fn the_two_diagnostics_take_the_selection_under_library() {
        for method in ["doctor.gather", "diagnose.run"] {
            let params = apply_selection(named("beta").as_ref(), method, Value::Null).unwrap();
            assert_eq!(params, json!({"library": "beta"}), "{method}");
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
        let params =
            apply_selection(named("beta").as_ref(), "library.no_such_method", json!({})).unwrap();
        assert_eq!(params, json!({}));
    }
}
