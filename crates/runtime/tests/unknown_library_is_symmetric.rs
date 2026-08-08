// SPDX-License-Identifier: Apache-2.0

//! A `library` param naming a library the registry does not hold is
//! caller input, and every handler that resolves that param reports it
//! the same way: `-32010 invalid library`, naming the name it could not
//! resolve and the names it could.
//!
//! `docs/control-plane.md` states the contract as a property of the
//! parameter rather than of the handler class, so the read proxies and
//! the write handlers are asserted against one another in a single
//! test: either side alone is satisfied by an implementation that picks
//! a code per class, which is the thing the document says it does not
//! do.
//!
//! The registry env is pinned to a per-binary tempdir, so the test
//! never touches the user's real registry file. The embedder probe
//! daemon bring-up performs is answered by
//! `bookrack_test_support::EmbedStub`, so no Ollama daemon is required.

#![cfg(unix)]

mod common;

use bookrack_runtime::{DaemonRuntime, RuntimeOpts};
use bookrack_test_support::{ProcessEnv, process_env};
use eyre::{Result, eyre};
use serde_json::{Value, json};

use crate::common::{Reader, Writer};
use crate::common::{connect, join_with_deadline, recv, send};

const INVALID_LIBRARY: i64 = -32010;

/// A name no entry in the seeded registry carries.
const GHOST: &str = "no-such-library";

/// One request, whole response frame.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_handler_resolving_a_library_param_refuses_an_unknown_name_alike() -> Result<()> {
    let sandbox = process_env(ProcessEnv::daemon().without_data_dir());
    let alpha = sandbox.data_root("alpha-root");
    let beta = sandbox.data_root("beta-root");
    // Both roots carry the layers a read projects over, so the positive
    // control below reports a library rather than a missing store.
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

        // Three handlers, three ways of reaching the registry: the
        // shared read resolver behind the `library.*` proxies, the
        // separate one `library.info` carries for its own `name` key,
        // and the write path that has always reported `-32010`. The
        // write case is the reference the other two are held to; it is
        // a queue-free write, so the refusal under test is reached
        // rather than pre-empted by the headless queue gate. Its other
        // params name a book that cannot exist, which proves the
        // registry is resolved before the selector is looked at.
        let cases = [
            (1_u64, "library.stats", json!({ "library": GHOST })),
            (2, "library.info", json!({ "name": GHOST })),
            (
                3,
                "metadata.set",
                json!({
                    "library": GHOST,
                    "book": 9_999_999_i64,
                    "field": "title",
                    "value": "anything",
                }),
            ),
        ];
        for (id, method, params) in cases {
            let resp = call(&mut w, &mut reader, id, method, params).await?;
            assert_eq!(
                resp["error"]["code"].as_i64(),
                Some(INVALID_LIBRARY),
                "{method} with an unknown library name: {resp}"
            );
            let message = resp["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains(GHOST),
                "{method} must name the library it could not resolve: {resp}"
            );
            // The available set is what lets an operator correct the
            // call without a separate listing round trip; it is carried
            // by the error type and must survive the mapping.
            for available in ["alpha", "beta"] {
                assert!(
                    message.contains(available),
                    "{method} must offer {available} as a name that does resolve: {resp}"
                );
            }
            // A wrapper noun in place of the fact is what rule 2 of the
            // error-message discipline forbids on the wire.
            assert!(
                !message.starts_with("registry:"),
                "{method} must state the failure, not the layer that raised it: {resp}"
            );
        }

        // Positive control: a name the registry does hold still
        // resolves. Without it, a resolver that refused every name
        // would satisfy every assertion above.
        let resp = call(
            &mut w,
            &mut reader,
            4,
            "library.stats",
            json!({"library": "beta"}),
        )
        .await?;
        assert!(
            resp["error"].is_null() && !resp["result"].is_null(),
            "library.stats must still answer for a registered library: {resp}"
        );
        let resp = call(
            &mut w,
            &mut reader,
            5,
            "library.info",
            json!({"name": "beta"}),
        )
        .await?;
        let named = resp["result"]["library_name"]
            .as_str()
            .ok_or_else(|| eyre!("library.info answered without naming a library: {resp}"))?;
        assert_eq!(
            named, "beta",
            "library.info must answer for the named library: {resp}"
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
