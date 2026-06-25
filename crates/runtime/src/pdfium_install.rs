// SPDX-License-Identifier: Apache-2.0

//! Operator-initiated install of the pinned PDFium binary.
//!
//! Downloads the archive named by [`bookrack_extract::pdfium_pin`],
//! verifies its SHA-256 against the pin, and unpacks the dynamic
//! library into the per-user managed directory — the last stop in the
//! library search chain, so every later run finds it without any
//! configuration. Network access happens only here, behind an explicit
//! operator action (`bookrack doctor --install-pdfium` or a wizard
//! confirmation); no ingest path ever downloads anything.

use std::io::Read;
use std::path::{Path, PathBuf};

use bookrack_config::{pdfium_library_filename, pdfium_managed_dir};
use bookrack_extract::pdfium_pin::{pdfium_download_url, pinned_pdfium_binary};
use eyre::{Context, ContextCompat, Result, bail};
use sha2::{Digest, Sha256};

/// Download, verify, and unpack the pinned PDFium library into the
/// managed directory. Returns the path of the installed library.
pub async fn install_pinned_pdfium() -> Result<PathBuf> {
    let Some(pin) = pinned_pdfium_binary() else {
        bail!("no pinned PDFium binary is published for this platform");
    };
    let dir =
        pdfium_managed_dir().context("locate the per-user data directory for this platform")?;
    let url = pdfium_download_url(pin);
    let response = reqwest::get(&url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("download {url}"))?;
    let archive = response
        .bytes()
        .await
        .with_context(|| format!("download {url}"))?;
    verify_sha256(&archive, pin.sha256)?;
    let library = extract_member(&archive, pin.path_in_archive)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let target = dir.join(pdfium_library_filename());
    // Stage in the target directory, then persist (a rename): a crash
    // mid-write can never leave a half-written library at the final
    // path for the loader to pick up.
    let mut staged = tempfile::NamedTempFile::new_in(&dir)
        .with_context(|| format!("stage a temporary file in {}", dir.display()))?;
    std::io::Write::write_all(&mut staged, &library).context("write the staged library")?;
    staged
        .persist(&target)
        .with_context(|| format!("write {}", target.display()))?;
    Ok(target)
}

/// Compare a downloaded archive's SHA-256 with the pinned digest.
/// Upstream publishes no checksums, so this pin is the only defence
/// against a corrupted download or a silently re-cut asset.
fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual: String = Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if !actual.eq_ignore_ascii_case(expected) {
        bail!("archive SHA-256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

/// Pull one member out of a gzip-compressed tar archive.
fn extract_member(tgz: &[u8], member: &str) -> Result<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    for entry in archive.entries().context("read the archive index")? {
        let mut entry = entry.context("read an archive entry")?;
        if entry.path().context("read an entry path")?.as_ref() == Path::new(member) {
            let mut bytes = Vec::new();
            entry
                .read_to_end(&mut bytes)
                .with_context(|| format!("unpack {member}"))?;
            return Ok(bytes);
        }
    }
    bail!("the archive holds no member named {member}");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an in-memory `.tgz` holding the given members.
    fn tgz(members: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, data) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, *data)
                .expect("append");
        }
        builder
            .into_inner()
            .expect("finish tar")
            .finish()
            .expect("finish gzip")
    }

    #[test]
    fn extract_member_finds_the_named_entry() {
        let archive = tgz(&[("lib/other.txt", b"no"), ("lib/libpdfium.so", b"yes")]);
        let bytes = extract_member(&archive, "lib/libpdfium.so").expect("member");
        assert_eq!(bytes, b"yes");
    }

    #[test]
    fn extract_member_reports_a_missing_entry() {
        let archive = tgz(&[("lib/other.txt", b"no")]);
        let err = extract_member(&archive, "lib/libpdfium.so").unwrap_err();
        assert!(err.to_string().contains("lib/libpdfium.so"), "{err}");
    }

    #[test]
    fn verify_sha256_accepts_the_digest_and_rejects_others() {
        // SHA-256 of the empty input, a fixed test vector.
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        verify_sha256(b"", empty).expect("matching digest");
        let err = verify_sha256(b"x", empty).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
    }
}
