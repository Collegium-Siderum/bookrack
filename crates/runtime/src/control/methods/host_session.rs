// SPDX-License-Identifier: Apache-2.0

//! The daemon's side of write sessions.
//!
//! [`DaemonChangeSink`] is what the daemon's library registry reports
//! sessions to: an exclusive session raises the RPC write source and
//! pauses MCP for its duration, and a finished write announces
//! `library.changed`. [`run_host_write`] runs a change to the set of
//! served libraries — a mount, an unmount, a fork — as a host session
//! over the same write mutex the library sessions take.

use bookrack_ops::write_session::{ChangeSet, ChangeSink, HostWrite, WriteIntent};
use bookrack_ops::{Caller, OpsError};
use serde_json::Value;

use super::{MethodContext, name_the_library, session_failure};
use crate::control::error_map::ops_err;
use crate::control::events::{Event, EventStreamHandle};
use crate::control::jsonrpc::RpcError;

/// Reports the registry's sessions on the daemon's event stream.
///
/// An exclusive session starts by raising the RPC write source — the
/// daemon reads [`crate::control::events::DaemonState::Writing`] while it
/// is up — and broadcasting `mcp.availability { paused: true }`; it ends
/// by broadcasting `mcp.availability { paused: false }` and lowering the
/// source, before the session's lock is released. A finished write
/// publishes `library.changed { library }` in between.
#[derive(Clone)]
pub struct DaemonChangeSink {
    events: EventStreamHandle,
}

impl DaemonChangeSink {
    /// A sink publishing on `events`.
    pub fn new(events: EventStreamHandle) -> DaemonChangeSink {
        DaemonChangeSink { events }
    }
}

impl ChangeSink for DaemonChangeSink {
    fn exclusive_write_started(&self, _intent: &WriteIntent) {
        self.events.set_rpc_write(true);
        self.events.publish(Event::McpAvailability { paused: true });
    }

    fn library_changed(&self, library: &str) {
        self.events.publish(Event::LibraryChanged {
            library: library.to_string(),
        });
    }

    fn exclusive_write_ended(&self, _intent: &WriteIntent) {
        self.events
            .publish(Event::McpAvailability { paused: false });
        self.events.set_rpc_write(false);
    }
}

/// Run a change to the set of served libraries in a host session on
/// `library`, the library whose entry changes.
///
/// `op` returns the reply and the libraries it changed; the session
/// announces them once `op` is done. A second exclusive writer is
/// refused busy rather than queued. A success reply that is a JSON
/// object names `library` unless `op` already put a `library` key there.
pub(crate) async fn run_host_write<F>(
    ctx: &MethodContext,
    library: &str,
    op: F,
) -> Result<Value, RpcError>
where
    F: AsyncFnOnce(&mut HostWrite) -> Result<(Value, ChangeSet), RpcError> + Send + 'static,
{
    let session = ctx
        .registry
        .begin_host_write(Caller::control_plane(), library, None)
        .map_err(|busy| ops_err(OpsError::WriteBusy(busy)))?;
    let library = library.to_string();
    session
        .run_blocking(async move |s| {
            let (value, changes) = op(s).await?;
            Ok((name_the_library(value, &library), changes))
        })
        .await
        .map_err(session_failure)
}
