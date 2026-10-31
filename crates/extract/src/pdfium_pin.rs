// SPDX-License-Identifier: Apache-2.0

//! The pinned PDFium binary, as machine-readable constants.
//!
//! `PDFIUM_VERSION.md` in this crate documents the pin for humans;
//! this module carries the same values for the installer that
//! downloads and verifies the binary. The `pdfium_NNNN` cargo feature
//! in the workspace manifest selects the ABI surface these archives
//! expose; all three places bump together.

/// Upstream release tag the archives are published under.
pub const PDFIUM_RELEASE_TAG: &str = "chromium/7881";

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
            sha256: "73cc0de638ac2095e7445bf56a38200a5b7c7ca0e9f4ba144598f2457377ac08",
            path_in_archive: "bin/pdfium.dll",
        },
    ),
    (
        "linux",
        "x86_64",
        PdfiumBinaryPin {
            asset: "pdfium-linux-x64.tgz",
            sha256: "1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d",
            path_in_archive: "lib/libpdfium.so",
        },
    ),
    (
        "macos",
        "aarch64",
        PdfiumBinaryPin {
            asset: "pdfium-mac-arm64.tgz",
            sha256: "52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40",
            path_in_archive: "lib/libpdfium.dylib",
        },
    ),
    (
        "macos",
        "x86_64",
        PdfiumBinaryPin {
            asset: "pdfium-mac-x64.tgz",
            sha256: "6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b",
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

    /// The build number the release tag pins, e.g. `7881`.
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

    /// The `key:` value of every PDFium cache step in `workflow`, in
    /// file order. A step is the run of lines from one YAML list item
    /// (`- ...`) to the next; a PDFium cache step is one whose `name:`
    /// mentions PDFium and which uses `actions/cache`.
    fn pdfium_cache_keys(workflow: &str) -> Vec<String> {
        let mut steps: Vec<Vec<&str>> = Vec::new();
        for line in workflow.lines().map(str::trim) {
            if line.starts_with("- ") || steps.is_empty() {
                steps.push(Vec::new());
            }
            steps.last_mut().expect("a step is open").push(line);
        }
        steps
            .iter()
            .filter(|step| {
                step.iter()
                    .any(|l| l.starts_with("- name:") && l.contains("PDFium"))
                    && step.iter().any(|l| l.starts_with("uses: actions/cache@"))
            })
            .filter_map(|step| {
                step.iter()
                    .find_map(|l| l.strip_prefix("key:"))
                    .map(|key| key.trim().to_string())
            })
            .collect()
    }

    #[test]
    fn every_pdfium_cache_step_keys_on_the_pinned_build() {
        let prefix = format!("pdfium-chromium-{}-", pinned_build());
        // Both CI jobs and the release build cache the archive; adding
        // or dropping a cache step is a deliberate change that restates
        // these counts.
        for (rel, steps) in [
            ("../../.github/workflows/ci.yml", 2),
            ("../../.github/workflows/release.yml", 1),
        ] {
            let keys = pdfium_cache_keys(&repo_file(rel));
            assert_eq!(keys.len(), steps, "PDFium cache steps in {rel}: {keys:?}");
            for key in &keys {
                assert!(
                    key.starts_with(&prefix),
                    "{rel}: cache key {key:?} does not start with {prefix:?}, \
                     so a hit could restore an archive of another build",
                );
            }
        }
    }

    #[test]
    fn extractor_version_is_pinned_with_the_pdfium_binary() {
        // A different binary extracts differently, so the release tag
        // and every archive digest move only together with
        // `EXTRACTOR_VERSION`. Restating all of them here makes a
        // re-cut asset under an unchanged tag as visible as a new tag.
        let mut assets: Vec<(&str, &str)> = PINNED_BINARIES
            .iter()
            .map(|(_, _, pin)| (pin.asset, pin.sha256))
            .collect();
        assets.sort_unstable();
        assert_eq!(
            (crate::EXTRACTOR_VERSION, PDFIUM_RELEASE_TAG, assets),
            (
                7,
                "chromium/7881",
                vec![
                    (
                        "pdfium-linux-x64.tgz",
                        "1470e21b8b4a3b4ad7f85684e2da11d94f3b69a86d81dee11b9b6709d927ac1d",
                    ),
                    (
                        "pdfium-mac-arm64.tgz",
                        "52e94ca5aa8847934330daf3f8150c190682c5ca93831468794f8b90d4392e40",
                    ),
                    (
                        "pdfium-mac-x64.tgz",
                        "6dedf83990e0e3d6b7c93c9e7589c5a126b0ae14b7464d76120cff7a26afb18b",
                    ),
                    (
                        "pdfium-win-x64.tgz",
                        "73cc0de638ac2095e7445bf56a38200a5b7c7ca0e9f4ba144598f2457377ac08",
                    ),
                ],
            ),
            "the PDFium pin and EXTRACTOR_VERSION move together; bump the version and restate \
             the tag and digests here",
        );
    }

    #[test]
    fn download_url_encodes_the_release_tag() {
        let pin = PdfiumBinaryPin {
            asset: "pdfium-test.tgz",
            sha256: "00",
            path_in_archive: "lib/libpdfium.so",
        };
        let url = pdfium_download_url(&pin);
        assert!(url.contains("chromium%2F7881/pdfium-test.tgz"), "{url}");
        assert!(!url.contains("chromium/7881"), "{url}");
    }
}
