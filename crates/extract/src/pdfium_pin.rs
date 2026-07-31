// SPDX-License-Identifier: Apache-2.0

//! The pinned PDFium binary, as machine-readable constants.
//!
//! `PDFIUM_VERSION.md` in this crate documents the pin for humans;
//! this module carries the same values for the installer that
//! downloads and verifies the binary. The `pdfium_NNNN` cargo feature
//! in the workspace manifest selects the ABI surface these archives
//! expose; all three places bump together.

/// Upstream release tag the archives are published under.
pub const PDFIUM_RELEASE_TAG: &str = "chromium/7763";

/// One platform's pinned archive: the asset name under the release
/// tag, the SHA-256 of the archive, and where the dynamic library
/// sits inside it.
#[derive(Debug, Clone, Copy)]
pub struct PdfiumBinaryPin {
    pub asset: &'static str,
    pub sha256: &'static str,
    pub path_in_archive: &'static str,
}

/// Every published pin, keyed by `(target_os, target_arch)` — the same
/// pair [`pinned_pdfium_binary`] selects on. One table rather than a
/// `cfg!` chain, so the parity tests below can compare every copy of
/// the pin, not only the compilation target's row.
const PINNED_BINARIES: &[(&str, &str, PdfiumBinaryPin)] = &[
    (
        "windows",
        "x86_64",
        PdfiumBinaryPin {
            asset: "pdfium-win-x64.tgz",
            sha256: "45c4cc5d052ef8ec6380b946b548a76100f4675e38362000a4c732e16d5e8eda",
            path_in_archive: "bin/pdfium.dll",
        },
    ),
    (
        "linux",
        "x86_64",
        PdfiumBinaryPin {
            asset: "pdfium-linux-x64.tgz",
            sha256: "e3f0c66b2daad710cb6c8edd4a8c45c8902995e359dc0775917fc16e2e56349d",
            path_in_archive: "lib/libpdfium.so",
        },
    ),
    (
        "macos",
        "aarch64",
        PdfiumBinaryPin {
            asset: "pdfium-mac-arm64.tgz",
            sha256: "9acf49e46c68992cd40810e88264b1ad171805d02fd41c4cca336aad6653b333",
            path_in_archive: "lib/libpdfium.dylib",
        },
    ),
    (
        "macos",
        "x86_64",
        PdfiumBinaryPin {
            asset: "pdfium-mac-x64.tgz",
            sha256: "f455e0868ef7e5174a315de8789ee2b7a5544638d0ac7a3312ea7b68ebbc99cb",
            path_in_archive: "lib/libpdfium.dylib",
        },
    ),
];

/// The pinned archive for the compilation target, or `None` when no
/// binary is published for it.
pub fn pinned_pdfium_binary() -> Option<&'static PdfiumBinaryPin> {
    PINNED_BINARIES
        .iter()
        .find(|(os, arch, _)| *os == std::env::consts::OS && *arch == std::env::consts::ARCH)
        .map(|(_, _, pin)| pin)
}

/// Download URL for a pinned archive. The `/` in the release tag is
/// percent-encoded, as GitHub release asset URLs require.
pub fn pdfium_download_url(pin: &PdfiumBinaryPin) -> String {
    format!(
        "https://github.com/bblanchon/pdfium-binaries/releases/download/{tag}/{asset}",
        tag = PDFIUM_RELEASE_TAG.replace('/', "%2F"),
        asset = pin.asset,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pin_is_well_formed() {
        for (os, arch, pin) in PINNED_BINARIES {
            let label = format!("{os}/{arch}");
            assert_eq!(pin.sha256.len(), 64, "{label}");
            assert!(pin.sha256.chars().all(|c| c.is_ascii_hexdigit()), "{label}");
            assert!(pin.asset.starts_with("pdfium-"), "{label}");
            assert!(pin.asset.ends_with(".tgz"), "{label}");
            assert!(
                pin.path_in_archive
                    .rsplit('/')
                    .next()
                    .is_some_and(|f| f.contains("pdfium")),
                "{label}",
            );
        }
    }

    // --- pin parity across the copies of the pinned values ------------
    //
    // The pin lives in five places: this module, PDFIUM_VERSION.md, the
    // workspace manifest's `pdfium_NNNN` feature, the CI fetch step,
    // and the release build matrix. These tests hold every copy to the
    // table above, so bumping one copy without the others goes red.

    fn repo_file(rel: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
    }

    /// The build number the release tag pins, e.g. `7763`.
    fn pinned_build() -> &'static str {
        PDFIUM_RELEASE_TAG
            .rsplit('/')
            .next()
            .expect("the release tag carries a build number")
    }

    #[test]
    fn the_version_doc_matches_the_pin_table() {
        let doc = repo_file("PDFIUM_VERSION.md");
        assert!(
            doc.contains(PDFIUM_RELEASE_TAG),
            "PDFIUM_VERSION.md names the release tag {PDFIUM_RELEASE_TAG}",
        );
        assert!(
            doc.contains(&format!("pdfium_{}", pinned_build())),
            "PDFIUM_VERSION.md names the matching cargo feature",
        );
        for (_, _, pin) in PINNED_BINARIES {
            let row = doc
                .lines()
                .find(|line| line.contains(pin.asset))
                .unwrap_or_else(|| panic!("PDFIUM_VERSION.md lists {}", pin.asset));
            assert!(
                row.contains(pin.sha256),
                "the {} row carries its pinned sha256",
                pin.asset,
            );
        }
    }

    #[test]
    fn the_workspace_manifest_selects_the_matching_abi_feature() {
        let manifest = repo_file("../../Cargo.toml");
        assert!(
            manifest.contains(&format!("\"pdfium_{}\"", pinned_build())),
            "the pdfium-render feature selects the ABI of build {}",
            pinned_build(),
        );
    }

    #[test]
    fn the_ci_workflow_fetches_and_verifies_the_linux_pin() {
        let ci = repo_file("../../.github/workflows/ci.yml");
        let (_, _, linux) = PINNED_BINARIES
            .iter()
            .find(|(os, _, _)| *os == "linux")
            .expect("a linux pin exists");
        let encoded_tag = PDFIUM_RELEASE_TAG.replace('/', "%2F");
        assert!(
            ci.contains(&format!("{encoded_tag}/{}", linux.asset)),
            "the CI download URL names the pinned tag and asset",
        );
        assert!(
            ci.contains(linux.sha256),
            "the CI verify step carries the pinned linux sha256",
        );
    }

    #[test]
    fn the_release_workflow_matrix_matches_the_pin_table() {
        let workflow = repo_file("../../.github/workflows/release.yml");
        assert!(
            workflow.contains(&PDFIUM_RELEASE_TAG.replace('/', "%2F")),
            "the release download URL names the pinned tag",
        );
        let mut bundled = 0;
        for (_, _, pin) in PINNED_BINARIES {
            if workflow.contains(pin.asset) {
                assert!(
                    workflow.contains(pin.sha256),
                    "the release matrix row for {} carries its pinned sha256",
                    pin.asset,
                );
                bundled += 1;
            }
        }
        // Three platform tarballs ship today; adding or dropping a
        // release target is a deliberate change that restates this
        // count.
        assert_eq!(bundled, 3, "release targets bundling a pinned PDFium");
    }

    #[test]
    fn download_url_encodes_the_release_tag() {
        let pin = PdfiumBinaryPin {
            asset: "pdfium-test.tgz",
            sha256: "00",
            path_in_archive: "lib/libpdfium.so",
        };
        let url = pdfium_download_url(&pin);
        assert!(url.contains("chromium%2F7763/pdfium-test.tgz"), "{url}");
        assert!(!url.contains("chromium/7763"), "{url}");
    }
}
