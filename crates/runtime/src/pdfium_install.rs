// SPDX-License-Identifier: Apache-2.0

//! Operator-initiated install of the pinned PDFium binary.
//!
//! Takes the archive named by [`bookrack_extract::pdfium_pin`] — from
//! the network, or from a file the operator already holds — verifies
//! its SHA-256 against the pin, and unpacks the dynamic library into
//! the pinned build's managed directory: the last stop in the library
//! search chain, so every later run of this build finds it without any
//! configuration, and a copy another build installed is left where it
//! is. Network access happens only here, behind an explicit operator
//! action (`bookrack setup pdfium` or a wizard confirmation); no ingest
//! path ever downloads anything.

use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};

use bookrack_config::{pdfium_library_filename, pdfium_managed_dir};
use bookrack_extract::pdfium_pin::{
    PDFIUM_RELEASE_TAG, PdfiumBinaryPin, pdfium_download_url, pinned_pdfium_binary,
};
use eyre::{Context, ContextCompat, Result, bail};
use sha2::{Digest, Sha256};

use crate::reranker_install::InstallOutcome;

/// Put the pinned PDFium library in the pinned build's managed
/// directory, downloading it unless it is already there. Returns where
/// the library is and whether this call downloaded it.
// Parallel implementation: installer-fetch.
pub async fn install_pinned_pdfium() -> Result<InstallOutcome> {
    let (pin, dir) = pinned_slot()?;
    let url = pdfium_download_url(pin);
    install_into(&dir, pin.sha256, pin.path_in_archive, || async {
        let response = reqwest::get(&url)
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("download {url}"))?;
        let archive = response
            .bytes()
            .await
            .with_context(|| format!("download {url}"))?;
        Ok(archive.to_vec())
    })
    .await
}

/// Install the pinned PDFium library from `archive`, a copy of the
/// pinned release archive on this machine. The archive passes the same
/// digest check a download does; one that fails it is refused and
/// nothing is written. A library already in the slot is replaced.
pub fn install_pdfium_from_archive(archive: &Path) -> Result<InstallOutcome> {
    let (pin, dir) = pinned_slot()?;
    let bytes = std::fs::read(archive).with_context(|| format!("read {}", archive.display()))?;
    let path = unpack_into(&dir, &bytes, pin.sha256, pin.path_in_archive)
        .with_context(|| format!("install from {}", archive.display()))?;
    Ok(InstallOutcome {
        path,
        downloaded: false,
    })
}

/// The library in the pinned build's managed directory, when one is
/// installed there.
pub fn installed_pdfium() -> Option<PathBuf> {
    let (_, dir) = pinned_slot().ok()?;
    let library = dir.join(pdfium_library_filename());
    library.is_file().then_some(library)
}

/// This platform's pin and the managed directory its library goes in.
fn pinned_slot() -> Result<(&'static PdfiumBinaryPin, PathBuf)> {
    let Some(pin) = pinned_pdfium_binary() else {
        bail!("no pinned PDFium binary is published for this platform");
    };
    let dir = pdfium_managed_dir(PDFIUM_RELEASE_TAG)
        .context("locate the per-user data directory for this platform")?;
    Ok((pin, dir))
}

/// Leave the library in `dir`, calling `fetch` for the archive only
/// when it is not already there.
async fn install_into<F, Fut>(
    dir: &Path,
    sha256: &str,
    member: &str,
    fetch: F,
) -> Result<InstallOutcome>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Vec<u8>>>,
{
    let target = dir.join(pdfium_library_filename());
    if target.is_file() {
        return Ok(InstallOutcome {
            path: target,
            downloaded: false,
        });
    }
    let archive = fetch().await?;
    let path = unpack_into(dir, &archive, sha256, member)?;
    Ok(InstallOutcome {
        path,
        downloaded: true,
    })
}

/// Verify `archive` against `sha256` and write its `member` into `dir`
/// as the platform's library file. Nothing is created — not `dir`
/// either — unless the digest matches.
fn unpack_into(dir: &Path, archive: &[u8], sha256: &str, member: &str) -> Result<PathBuf> {
    verify_sha256(archive, sha256)?;
    let library = extract_member(archive, member)?;
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let target = dir.join(pdfium_library_filename());
    // Stage in the target directory, then persist (a rename): a crash
    // mid-write can never leave a half-written library at the final
    // path for the loader to pick up.
    let mut staged = tempfile::NamedTempFile::new_in(dir)
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
// Parallel implementation: sha256-hex.
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

    fn digest(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    #[test]
    fn an_archive_with_the_wrong_digest_is_refused_and_nothing_is_written() {
        let root = tempfile::tempdir().expect("tempdir");
        let slot = root.path().join("chromium").join("0000");
        let archive = tgz(&[("lib/libpdfium", b"library bytes")]);

        let err = unpack_into(
            &slot,
            &archive,
            &digest(b"another archive"),
            "lib/libpdfium",
        )
        .expect_err("a digest that does not match is refused");
        assert!(err.to_string().contains("mismatch"), "{err}");
        assert!(!slot.exists(), "the slot was created for a refused archive");
    }

    #[test]
    fn an_archive_with_the_pinned_digest_lands_in_the_slot() {
        let root = tempfile::tempdir().expect("tempdir");
        let slot = root.path().join("chromium").join("0000");
        let archive = tgz(&[
            ("lib/other.txt", b"no"),
            ("lib/libpdfium", b"library bytes"),
        ]);

        let path = unpack_into(&slot, &archive, &digest(&archive), "lib/libpdfium")
            .expect("the pinned digest installs");
        assert_eq!(path, slot.join(pdfium_library_filename()));
        assert_eq!(std::fs::read(&path).expect("read"), b"library bytes");
        let entries: Vec<_> = std::fs::read_dir(&slot)
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "a staging file was left behind: {entries:?}"
        );
    }

    #[tokio::test]
    async fn an_installed_slot_is_not_downloaded_again() {
        let root = tempfile::tempdir().expect("tempdir");
        let slot = root.path().join("slot");
        let archive = tgz(&[("lib/libpdfium", b"library bytes")]);
        let sha = digest(&archive);
        let fetches = std::sync::atomic::AtomicUsize::new(0);
        let fetch = || async {
            fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(archive.clone())
        };

        let first = install_into(&slot, &sha, "lib/libpdfium", fetch)
            .await
            .expect("first install");
        assert!(first.downloaded);
        let second = install_into(&slot, &sha, "lib/libpdfium", fetch)
            .await
            .expect("second install");
        assert!(!second.downloaded, "an installed library was fetched again");
        assert_eq!(second.path, first.path);
        assert_eq!(fetches.load(std::sync::atomic::Ordering::SeqCst), 1);
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
