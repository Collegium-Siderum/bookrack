//! cli-side sub-commands. The daemon-side runtime crate hosts the
//! `cmd::*` runners that talk to catalog / corpus / vectors; this
//! module is for client-side dispatch that goes through the control
//! socket instead.

pub mod cli_client;
pub mod repl_client;
