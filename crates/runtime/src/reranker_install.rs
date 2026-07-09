// SPDX-License-Identifier: Apache-2.0

//! Operator-initiated install of the pinned reranker artifacts: the
//! llama-server binary and the GGUF reranker model.
//!
//! Downloads the archives named by the pins in the config crate,
//! verifies their SHA-256, and lands them in the per-user managed
//! directories — the last stops in the respective search chains, so
//! every later run finds them without any configuration. Network
//! access happens only here, behind an explicit operator action
//! (`bookrack doctor --install-reranker`); no query path ever
//! downloads anything.
//!
//! Two departures from the PDFium installer this module is patterned
//! on. The llama.cpp archive is unpacked whole into a per-build
//! directory instead of having one member extracted: `llama-server`
//! loads the `libggml*`/`libllama` dynamic libraries packed beside it.
//! And the model download is streamed to the staged file with the
//! digest fed incrementally, because a ~600 MB file has no business in
//! memory.

use std::io::Write;
use std::path::{Path, PathBuf};

use bookrack_config::llama_server_pin::{
    llama_server_download_url, llama_server_managed_dir, pinned_llama_server,
};
use bookrack_config::reranker_model_pin::{
    RERANKER_MODEL_PINS, RerankerModelPin, models_managed_dir, reranker_model_download_url,
};
use eyre::{Context, ContextCompat, Result, bail};
use sha2::{Digest, Sha256};

/// One artifact the combined verb accounted for: where it now lives
/// and whether this invocation downloaded it.
#[derive(Debug)]
pub struct InstallOutcome {
    pub path: PathBuf,
    pub downloaded: bool,
}

/// Install whichever pinned reranker artifacts are missing from the
/// managed directories: the llama-server binary, then every pinned
/// model. Artifacts already in place are reported, not re-downloaded,
/// so the verb is idempotent.
pub async fn install_reranker() -> Result<Vec<InstallOutcome>> {
    let mut outcomes = vec![install_pinned_llama_server().await?];
    for pin in RERANKER_MODEL_PINS {
        outcomes.push(install_pinned_reranker_model(pin).await?);
    }
    Ok(outcomes)
}

/// Download, verify, and unpack the pinned llama.cpp archive into the
/// per-build managed directory. Returns the path of the `llama-server`
/// executable; a no-op when it is already in place.
pub async fn install_pinned_llama_server() -> Result<InstallOutcome> {
    let Some(pin) = pinned_llama_server() else {
        bail!("no llama-server archive is pinned for this platform");
    };
    let dir = llama_server_managed_dir()
        .context("locate the per-user data directory for this platform")?;
    let target = dir.join(pin.exe_in_archive);
    if target.is_file() {
        return Ok(InstallOutcome {
            path: target,
            downloaded: false,
        });
    }
    let url = llama_server_download_url(pin);
    let response = reqwest::get(&url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("download {url}"))?;
    let archive = response
        .bytes()
        .await
        .with_context(|| format!("download {url}"))?;
    verify_sha256(&archive, pin.sha256)?;
    let parent = dir.parent().context("managed directory has a parent")?;
    std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    // Unpack into a staged sibling directory, then rename it to the
    // per-build name: a crash mid-unpack can never leave a half-
    // populated build directory for the search chain to pick up.
    let staged = tempfile::tempdir_in(parent)
        .with_context(|| format!("stage a temporary directory in {}", parent.display()))?;
    unpack_archive(&archive, staged.path())?;
    if !staged.path().join(pin.exe_in_archive).is_file() {
        bail!("the archive holds no member named {}", pin.exe_in_archive);
    }
    // A directory at the final path with the executable missing (the
    // pre-check above) can only be manual damage; replace it.
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("remove stale {}", dir.display()))?;
    }
    std::fs::rename(staged.keep(), &dir).with_context(|| format!("write {}", dir.display()))?;
    prune_stale_builds(parent, &dir);
    Ok(InstallOutcome {
        path: target,
        downloaded: true,
    })
}

/// Sweep sibling build directories after a pin bump lands the new
/// one, so exactly one managed build exists at a time. Best-effort:
/// a directory that will not delete is left for the next run.
fn prune_stale_builds(parent: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path != keep && path.is_dir() {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Download and verify one pinned model file into the managed models
/// directory. Returns the path of the `.gguf`; a no-op when it is
/// already in place.
pub async fn install_pinned_reranker_model(pin: &RerankerModelPin) -> Result<InstallOutcome> {
    let dir =
        models_managed_dir().context("locate the per-user data directory for this platform")?;
    let target = dir.join(pin.hf_file);
    if target.is_file() {
        return Ok(InstallOutcome {
            path: target,
            downloaded: false,
        });
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let url = reranker_model_download_url(pin);
    let mut response = reqwest::get(&url)
        .await
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("download {url}"))?;
    // Stage in the target directory, then persist (a rename): a crash
    // mid-download can never leave a partial model at the final path,
    // and a checksum mismatch drops the staged file with the guard.
    let mut staged = StagedDownload::new_in(&dir)?;
    while let Some(chunk) = response
        .chunk()
        .await
        .with_context(|| format!("download {url}"))?
    {
        staged.write_chunk(&chunk)?;
        staged.report_progress(pin.bytes);
    }
    let path = staged.finish(pin.sha256, &target)?;
    Ok(InstallOutcome {
        path,
        downloaded: true,
    })
}

/// A download being streamed to a staged temporary file, its SHA-256
/// computed as the bytes arrive.
struct StagedDownload {
    file: tempfile::NamedTempFile,
    hasher: Sha256,
    written: u64,
    reported: u64,
}

/// Emit a progress line at most once per this many downloaded bytes.
const PROGRESS_STEP: u64 = 64 * 1024 * 1024;

impl StagedDownload {
    fn new_in(dir: &Path) -> Result<Self> {
        let file = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("stage a temporary file in {}", dir.display()))?;
        Ok(Self {
            file,
            hasher: Sha256::new(),
            written: 0,
            reported: 0,
        })
    }

    fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        self.file
            .write_all(chunk)
            .context("write the staged download")?;
        self.hasher.update(chunk);
        self.written += chunk.len() as u64;
        Ok(())
    }

    /// Print a coarse progress line to stderr — diagnostics, so `--json`
    /// output on stdout stays clean.
    fn report_progress(&mut self, total: u64) {
        if self.written - self.reported >= PROGRESS_STEP {
            self.reported = self.written;
            eprintln!(
                "downloaded {} / {} MiB",
                self.written / (1024 * 1024),
                total.div_ceil(1024 * 1024),
            );
        }
    }

    /// Compare the accumulated digest with the pin and persist the
    /// staged file at the target path. On mismatch the staged file is
    /// dropped, which deletes it.
    fn finish(self, expected_sha256: &str, target: &Path) -> Result<PathBuf> {
        let actual: String = self
            .hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            bail!("download SHA-256 mismatch: expected {expected_sha256}, got {actual}");
        }
        self.file
            .persist(target)
            .with_context(|| format!("write {}", target.display()))?;
        Ok(target.to_path_buf())
    }
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

/// Unpack every member of a gzip-compressed tar archive under `dest`,
/// preserving the archive's own layout, file modes, and links.
fn unpack_archive(tgz: &[u8], dest: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    archive
        .unpack(dest)
        .with_context(|| format!("unpack the archive into {}", dest.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an in-memory `.tar.gz` holding the given members.
    fn tgz(members: &[(&str, &[u8])]) -> Vec<u8> {
        let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(encoder);
        for (path, data) in members {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
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
    fn unpack_archive_lands_every_member_in_layout() {
        let archive = tgz(&[
            ("llama-test/llama-server", b"exe".as_slice()),
            ("llama-test/libggml.so", b"lib".as_slice()),
        ]);
        let dest = tempfile::tempdir().expect("tempdir");
        unpack_archive(&archive, dest.path()).expect("unpack");
        assert_eq!(
            std::fs::read(dest.path().join("llama-test/llama-server")).expect("exe"),
            b"exe"
        );
        assert_eq!(
            std::fs::read(dest.path().join("llama-test/libggml.so")).expect("lib"),
            b"lib"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unpack_archive_preserves_the_executable_bit() {
        use std::os::unix::fs::PermissionsExt;
        let archive = tgz(&[("llama-test/llama-server", b"exe".as_slice())]);
        let dest = tempfile::tempdir().expect("tempdir");
        unpack_archive(&archive, dest.path()).expect("unpack");
        let mode = std::fs::metadata(dest.path().join("llama-test/llama-server"))
            .expect("metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o111, 0o111, "mode {mode:o}");
    }

    #[test]
    fn unpack_archive_rejects_garbage() {
        let dest = tempfile::tempdir().expect("tempdir");
        assert!(unpack_archive(b"not a tarball", dest.path()).is_err());
    }

    #[test]
    fn verify_sha256_accepts_the_digest_and_rejects_others() {
        // SHA-256 of the empty input, a fixed test vector.
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        verify_sha256(b"", empty).expect("matching digest");
        let err = verify_sha256(b"x", empty).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
    }

    #[test]
    fn staged_download_persists_on_a_matching_digest() {
        // SHA-256 of b"abc", a fixed test vector.
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("model.gguf");
        let mut staged = StagedDownload::new_in(dir.path()).expect("stage");
        staged.write_chunk(b"ab").expect("chunk");
        staged.write_chunk(b"c").expect("chunk");
        let path = staged.finish(abc, &target).expect("persist");
        assert_eq!(path, target);
        assert_eq!(std::fs::read(&target).expect("read"), b"abc");
    }

    #[test]
    fn staged_download_mismatch_leaves_no_file_behind() {
        let abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("model.gguf");
        let mut staged = StagedDownload::new_in(dir.path()).expect("stage");
        staged.write_chunk(b"abx").expect("chunk");
        let err = staged.finish(abc, &target).unwrap_err();
        assert!(err.to_string().contains("mismatch"), "{err}");
        assert!(!target.exists());
        assert_eq!(
            std::fs::read_dir(dir.path()).expect("read dir").count(),
            0,
            "the staged file is deleted with the guard"
        );
    }
}
