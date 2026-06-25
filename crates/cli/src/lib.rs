// SPDX-License-Identifier: Apache-2.0

//! Library face of the `bookrack` binary. Reserved for cli-side
//! modules that need cross-module access through `cargo test`; the
//! daemon-side primitives now live in `bookrack-runtime`.

pub mod distill_cmd;
