// SPDX-License-Identifier: Apache-2.0

//! Library face of the `bookrack` binary. Reserved for cli-side
//! modules that need cross-module access through `cargo test`; the
//! daemon-side primitives now live in `bookrack-runtime`.

pub mod config_effective;
pub mod config_fixed;
pub mod config_knobs;
pub mod daemon_call;
pub mod distill_cmd;
pub mod error;
pub mod libraries_local;
pub mod render;
pub mod retrieval_cmd;
pub mod runs_cmd;

bookrack_core::fixed_settings! {
    owner = "cli";
    "cli.await_stall_timeout" = daemon_call::DEFAULT_AWAIT_STALL_TIMEOUT,
        "how long a wait tolerates silence from the daemon before reporting no progress",
        acts on "any command that waits on a queued job";
    "cli.call_timeout" = daemon_call::DEFAULT_CALL_TIMEOUT,
        "how long one call waits for the daemon before it is given up on",
        acts on "every command that reaches the daemon";
    "cli.scan_parent_depth" = libraries_local::PARENT_SCAN_DEPTH,
        "levels below a named parent that a scan descends looking for libraries",
        acts on "libraries scan <parent>";
    "cli.scan_volumes_depth" = libraries_local::VOLUMES_SCAN_DEPTH,
        "levels below each mounted volume that a scan descends",
        acts on "libraries scan --volumes";
}
