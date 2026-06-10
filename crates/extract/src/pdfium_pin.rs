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

/// The pinned archive for the compilation target, or `None` when no
/// binary is published for it.
pub fn pinned_pdfium_binary() -> Option<&'static PdfiumBinaryPin> {
    if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        Some(&PdfiumBinaryPin {
            asset: "pdfium-win-x64.tgz",
            sha256: "45c4cc5d052ef8ec6380b946b548a76100f4675e38362000a4c732e16d5e8eda",
            path_in_archive: "bin/pdfium.dll",
        })
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(&PdfiumBinaryPin {
            asset: "pdfium-linux-x64.tgz",
            sha256: "e3f0c66b2daad710cb6c8edd4a8c45c8902995e359dc0775917fc16e2e56349d",
            path_in_archive: "lib/libpdfium.so",
        })
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(&PdfiumBinaryPin {
            asset: "pdfium-mac-arm64.tgz",
            sha256: "9acf49e46c68992cd40810e88264b1ad171805d02fd41c4cca336aad6653b333",
            path_in_archive: "lib/libpdfium.dylib",
        })
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(&PdfiumBinaryPin {
            asset: "pdfium-mac-x64.tgz",
            sha256: "f455e0868ef7e5174a315de8789ee2b7a5544638d0ac7a3312ea7b68ebbc99cb",
            path_in_archive: "lib/libpdfium.dylib",
        })
    } else {
        None
    }
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
    fn pin_for_this_target_is_well_formed() {
        let Some(pin) = pinned_pdfium_binary() else {
            return;
        };
        assert_eq!(pin.sha256.len(), 64);
        assert!(pin.sha256.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(pin.asset.starts_with("pdfium-"));
        assert!(pin.asset.ends_with(".tgz"));
        assert!(
            pin.path_in_archive
                .rsplit('/')
                .next()
                .is_some_and(|f| f.contains("pdfium"))
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
        assert!(url.contains("chromium%2F7763/pdfium-test.tgz"), "{url}");
        assert!(!url.contains("chromium/7763"), "{url}");
    }
}
