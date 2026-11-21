// SPDX-License-Identifier: Apache-2.0

//! The host's managed PDFium directory, linked into a sandbox on
//! request.
//!
//! A sandbox starts with no native library: a missing-library doctor
//! row, the wizard's download offer, and `setup` listing a component as
//! absent all depend on that, and a sandbox that always carried the
//! host's library would make those tests report on the machine running
//! them. A test that extracts a PDF asks for the library by name
//! instead — [`crate::Sandbox::with_host_pdfium`], or the
//! [`crate::ProcessEnv::daemon`] preset — and gets the host's managed
//! directory mirrored under the sandbox's own: every directory created,
//! every file a link. The product then finds the library through the
//! search it runs on an operator's machine, with no variable involved.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static HOST_PDFIUM_ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();

/// The root of the managed PDFium directories on the host this process
/// started on: resolved on the first call and fixed from then on.
///
/// [`crate::process_env`] makes that first call before it moves the
/// home directory, so a sandbox built afterwards still links the host's
/// directory and not the one inside the sandbox the process now lives
/// in.
pub fn host_pdfium_root() -> Option<&'static Path> {
    HOST_PDFIUM_ROOT
        .get_or_init(bookrack_config::pdfium_managed_root)
        .as_deref()
}

/// Mirror `host_root` under `sandbox_root`: a directory for every
/// directory, a link for every file.
///
/// Entries are judged through any link they are, so a host root that is
/// itself a mirror — the home `scripts/test-clean.sh` builds — is
/// mirrored the same way. No directory is ever linked: a child that
/// writes into the sandbox's copy creates a file there and nothing on
/// the host. A host root that does not exist mirrors to nothing.
pub(crate) fn link_tree(host_root: &Path, sandbox_root: &Path) -> io::Result<()> {
    let entries = match std::fs::read_dir(host_root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    std::fs::create_dir_all(sandbox_root)?;
    for entry in entries {
        let entry = entry?;
        let source = entry.path();
        let target = sandbox_root.join(entry.file_name());
        let meta = match std::fs::metadata(&source) {
            Ok(meta) => meta,
            // A link whose target is gone holds nothing to mirror.
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err),
        };
        if meta.is_dir() {
            link_tree(&source, &target)?;
        } else if meta.is_file() {
            link_file(&source, &target)?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn link_file(source: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

/// Where a file link needs a privilege, the file is copied instead.
#[cfg(not(unix))]
fn link_file(source: &Path, target: &Path) -> io::Result<()> {
    std::fs::copy(source, target).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Sandbox;

    /// A stand-in host root: a two-level build directory holding a
    /// library file, a second build whose library is itself a link, and
    /// a file at the root.
    fn stand_in_host() -> tempfile::TempDir {
        let host = tempfile::tempdir().expect("tempdir");
        let build = host.path().join("chromium").join("1");
        std::fs::create_dir_all(&build).expect("build dir");
        std::fs::write(build.join("libpdfium.so"), b"one").expect("library");
        std::fs::write(host.path().join("flat.so"), b"flat").expect("root file");
        #[cfg(unix)]
        {
            let other = host.path().join("chromium").join("2");
            std::fs::create_dir_all(&other).expect("second build dir");
            std::os::unix::fs::symlink(build.join("libpdfium.so"), other.join("libpdfium.so"))
                .expect("linked library");
        }
        host
    }

    fn sandbox_pdfium_root(sandbox: &Sandbox) -> PathBuf {
        sandbox.managed_data_dir().join("bookrack").join("pdfium")
    }

    /// The default is clean, and the opt-in is what changes it. A
    /// sandbox that carried the host's library unasked would pass the
    /// second half and fail the first.
    #[test]
    fn a_sandbox_is_clean_unless_it_opts_in() {
        let host = stand_in_host();

        let plain = Sandbox::new();
        assert!(
            !sandbox_pdfium_root(&plain).exists(),
            "a sandbox that did not ask holds a managed PDFium directory"
        );

        let opted = Sandbox::new().with_pdfium_from(host.path());
        let library = sandbox_pdfium_root(&opted)
            .join("chromium")
            .join("1")
            .join("libpdfium.so");
        assert_eq!(std::fs::read(&library).expect("the linked library"), b"one");
    }

    /// Every file resolves at its own depth, a file that is a link on
    /// the host included, and no directory is shared with the host: a
    /// file created under the sandbox's build directory does not appear
    /// on the host's.
    #[test]
    fn the_host_slot_is_linked_file_by_file_at_any_depth_and_through_links() {
        let host = stand_in_host();
        let sandbox = Sandbox::new().with_pdfium_from(host.path());
        let root = sandbox_pdfium_root(&sandbox);

        let mut expected = vec![
            (root.join("flat.so"), &b"flat"[..]),
            (root.join("chromium").join("1").join("libpdfium.so"), b"one"),
        ];
        if cfg!(unix) {
            expected.push((root.join("chromium").join("2").join("libpdfium.so"), b"one"));
        }
        for (path, content) in expected {
            let meta = std::fs::metadata(&path)
                .unwrap_or_else(|err| panic!("{} does not resolve: {err}", path.display()));
            assert!(meta.is_file(), "{} is not a file", path.display());
            assert_eq!(std::fs::read(&path).expect("read"), content);
        }

        for dir in [
            root.clone(),
            root.join("chromium"),
            root.join("chromium").join("1"),
        ] {
            let meta = std::fs::symlink_metadata(&dir).expect("directory");
            assert!(
                meta.is_dir(),
                "{} is a link, so a write under it lands on the host",
                dir.display()
            );
        }
        std::fs::write(root.join("chromium").join("1").join("written"), b"x").expect("write");
        assert!(
            !host
                .path()
                .join("chromium")
                .join("1")
                .join("written")
                .exists(),
            "a file written in the sandbox appeared on the host"
        );
    }

    /// A host with no managed directory leaves the sandbox as it was.
    #[test]
    fn a_host_without_a_slot_links_nothing() {
        let host = tempfile::tempdir().expect("tempdir");
        let sandbox = Sandbox::new().with_pdfium_from(&host.path().join("absent"));
        assert!(!sandbox_pdfium_root(&sandbox).exists());
    }
}
