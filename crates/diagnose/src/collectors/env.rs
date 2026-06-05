// SPDX-License-Identifier: Apache-2.0

//! Write `env.txt`: bookrack version, host triple, generation
//! timestamp, the time window, and the redacted data-dir hint.

use std::fmt::Write;
use std::path::Path;
use std::time::SystemTime;

use bookrack_config::Config;

use crate::manifest::iso8601_z;
use crate::scrub::Scrubber;
use crate::{Options, Result};

/// Write `<bundle>/env.txt`.
pub fn collect(
    cfg: &Config,
    opts: &Options,
    now: SystemTime,
    bundle_dir: &Path,
    scrubber: &Scrubber,
) -> Result<()> {
    let mut buf = String::new();
    writeln!(buf, "bookrack version : {}", env!("CARGO_PKG_VERSION")).ok();
    writeln!(
        buf,
        "os/arch          : {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
    .ok();
    writeln!(buf, "generated at     : {}", iso8601_z(now)).ok();
    writeln!(buf, "days window      : {}", opts.days).ok();
    writeln!(buf, "scrubbed         : {}", opts.scrub).ok();
    writeln!(
        buf,
        "data_dir         : {}",
        scrubber.scrub_string(&cfg.data_dir().to_string_lossy()),
    )
    .ok();
    let path = bundle_dir.join("env.txt");
    std::fs::write(&path, buf)?;
    Ok(())
}
