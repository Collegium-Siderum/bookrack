// SPDX-License-Identifier: Apache-2.0

//! Byte-quantity rendering shared across the workspace.

/// Formats a byte count with a binary-unit suffix, one decimal from
/// KiB up (`512 B`, `2.1 GiB`).
pub fn bytes_human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_pick_the_right_binary_unit() {
        assert_eq!(bytes_human(0), "0 B");
        assert_eq!(bytes_human(512), "512 B");
        assert_eq!(bytes_human(2048), "2.0 KiB");
        assert_eq!(bytes_human(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(bytes_human(2_254_857_830), "2.1 GiB");
    }
}
