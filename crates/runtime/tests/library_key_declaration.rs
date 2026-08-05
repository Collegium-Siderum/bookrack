// SPDX-License-Identifier: Apache-2.0

//! The method registry declares, per method, whether it takes a
//! library selection and under which key. This asserts the declaration
//! against the running dispatcher rather than against the source it
//! was read from.
//!
//! The params each case sends are **built from the declaration**: the
//! key comes from `library_key_for`, never from a literal here. A row
//! that names the wrong key therefore sends the wrong key, the handler
//! resolves the registry default instead, and the call succeeds where
//! this test demands `-32010`. Asserting the key by eye would leave
//! exactly that mistake invisible.
//!
//! The other two axes are covered by their own negative: a
//! process-facing method and an unrouted one both answer normally with
//! a `library` in their params, because refusing an unhonourable
//! selection is a promise the *client* makes — the daemon has no way
//! to tell an injected key from a hand-written one.
//!
//! The registry env is pinned to a per-binary tempdir, so the test
//! never touches the user's real registry file. The embedder probe
//! daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use bookrack_runtime::control::methods::{REGISTRY, library_key_for, refuses_library};
use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::{Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer};
use crate::common::{connect, join_with_deadline, recv, send};

const INVALID_LIBRARY: i64 = -32010;

/// A name no entry in the seeded registry carries.
const GHOST: &str = "no-such-library";

/// One representative per shape the axis has to survive: a
/// parametrised read, a queue-free write, and the one method that
/// selects under a key of its own. The second element is whatever
/// else the method requires before it reaches the registry lookup —
/// the selection itself is never spelled here, it is read off the
/// declaration under test.
const ROUTED_REPRESENTATIVES: [(&str, &[(&str, Value)]); 3] = [
    ("library.stats", &[]),
    ("library.info", &[]),
    (
        "metadata.set",
        &[
            ("book", Value::Null),
            ("field", Value::Null),
            ("value", Value::Null),
        ],
    ),
];

/// The values behind the required-field names above. Kept out of the
/// const so they can be owned strings: a book id no fixture carries,
/// and a field the handler would accept if it ever got that far.
fn required_value(field: &str) -> Value {
    match field {
        "book" => json!(9_999_999_i64),
        "field" => json!("title"),
        _ => json!("anything"),
    }
}

async fn call(
    writer: &mut Writer,
    reader: &mut Reader,
    id: u64,
    method: &str,
    params: Value,
) -> Result<Value> {
    let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
    send(writer, &frame.to_string()).await?;
    recv(reader).await
}

/// Structural half: every row declares exactly one of the three
/// selections, and carries a key if and only if it says it is routed.
/// A row that claims to be routed without a key would make
/// `library_key_for` answer `None` and the client would silently drop
/// the selection.
#[test]
fn every_row_declares_a_selection_and_a_key_that_agree() {
    for sig in REGISTRY {
        assert!(
            matches!(sig.selection, "routed" | "process" | "unrouted"),
            "{} declares an unknown selection {:?}",
            sig.name,
            sig.selection
        );
        assert_eq!(
            sig.library_key.is_some(),
            sig.selection == "routed",
            "{} declares selection {:?} with library_key {:?}",
            sig.name,
            sig.selection,
            sig.library_key
        );
    }
    // The table is only worth asserting if it is populated: a registry
    // that lost its routed rows would satisfy every check above.
    let routed = REGISTRY.iter().filter(|s| s.selection == "routed").count();
    assert!(
        routed > 60,
        "only {routed} routed methods; the axis lost rows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_routed_method_honours_the_key_the_registry_declares() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    for root in [&alpha, &beta] {
        for db in ["catalog.db", "papers_catalog.db"] {
            bookrack_catalog::Catalog::open(&root.join(db))?;
        }
        for db in ["corpus.db", "papers_corpus.db"] {
            bookrack_corpus::Corpus::open(&root.join(db))?;
        }
    }
    sandbox.write_registry_entries(
        Some("alpha"),
        &[("alpha", alpha.as_path()), ("beta", beta.as_path())],
    );
    let runtime_root = tempfile::tempdir()?;

    let mut opts = RuntimeOpts::headless(None, Some("alpha".to_string()));
    opts.no_mcp = true;
    opts.runtime_dir = Some(runtime_root.path().to_path_buf());
    let runtime = DaemonRuntime::start(opts).await?;
    let sock = runtime.control_sock.path.clone();
    let repl_handle = tokio::task::spawn_blocking(|| -> Result<()> { Ok(()) });

    let driver = tokio::spawn(async move {
        let (mut reader, mut w) = connect(&sock).await?;

        for (offset, (method, required)) in ROUTED_REPRESENTATIVES.iter().enumerate() {
            let key = library_key_for(method)
                .ok_or_else(|| eyre!("{method} is not declared routed by the registry"))?;
            let id = 10 + offset as u64;
            let params = |library: &str| {
                let mut object = serde_json::Map::new();
                for (field, _) in required.iter() {
                    object.insert((*field).to_string(), required_value(field));
                }
                object.insert(key.to_string(), json!(library));
                Value::Object(object)
            };
            let resp = call(&mut w, &mut reader, id, method, params(GHOST)).await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_LIBRARY),
                "{method} must resolve its selection through the declared key {key:?}: {resp}"
            );

            // Positive control: the same key, a name the registry does
            // hold. Without it a handler that refused every call would
            // pass the assertion above. What it must not be is
            // `-32010`; whether the work behind it then succeeds on an
            // empty root is a different question and not this test's.
            let resp = call(&mut w, &mut reader, id + 100, method, params("beta")).await?;
            assert_ne!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_LIBRARY),
                "{method} must resolve a name the registry holds: {resp}"
            );
        }

        // A selection reaching a method that takes none is not the
        // daemon's to refuse: it has no way to tell an injected key
        // from one the caller typed. The client is what refuses, and
        // this pins that the split stays where it is.
        for method in ["queue.list", "status"] {
            assert!(
                library_key_for(method).is_none(),
                "{method} must not declare a library key"
            );
            let resp = call(
                &mut w,
                &mut reader,
                300,
                method,
                json!({ "library": GHOST }),
            )
            .await?;
            assert!(
                resp["error"].is_null(),
                "{method} must ignore a library it never asked for: {resp}"
            );
        }
        assert!(
            refuses_library("status") && !refuses_library("queue.list"),
            "the two negatives must not collapse into one class"
        );

        send(
            &mut w,
            r#"{"jsonrpc":"2.0","id":99,"method":"daemon.shutdown"}"#,
        )
        .await?;
        let _ = recv(&mut reader).await?;
        Ok::<(), eyre::Report>(())
    });

    join_with_deadline(runtime, repl_handle, driver).await
}
