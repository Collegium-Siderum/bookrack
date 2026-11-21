// SPDX-License-Identifier: Apache-2.0

//! The pinned llama-server binary, as machine-readable constants.
//!
//! `LLAMA_SERVER_VERSION.md` in this crate documents the pin for
//! humans; this module carries the same values for the installer that
//! downloads and verifies the archive. The reranker backend spawns
//! this binary, so the pin and the rerank request shape are exercised
//! together; both bump in the same commit.
//!
//! Unlike the PDFium pin (a single dynamic library pulled out of its
//! archive), the llama.cpp release archives place the executable and
//! the `libggml*`/`libllama` dynamic libraries it loads side by side
//! in one top-level directory, so the installer unpacks the whole
//! archive and the pin records where the executable sits inside it.

use std::path::PathBuf;

/// Upstream release tag the archives are published under.
pub const LLAMA_SERVER_RELEASE_TAG: &str = "b9934";

/// One platform's pinned archive: the asset name under the release
/// tag, the SHA-256 of the archive, and where the `llama-server`
/// executable sits inside it.
#[derive(Debug, Clone, Copy)]
pub struct LlamaServerBinaryPin {
    pub asset: &'static str,
    pub sha256: &'static str,
    pub exe_in_archive: &'static str,
}

/// The pinned archive for the compilation target, or `None` when no
/// archive is pinned for it. Windows archives exist upstream as `.zip`
/// (a format the installer does not unpack), so no Windows row is
/// pinned; doctor reports the gap instead.
pub fn pinned_llama_server() -> Option<&'static LlamaServerBinaryPin> {
    if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some(&LlamaServerBinaryPin {
            asset: "llama-b9934-bin-macos-arm64.tar.gz",
            sha256: "f9338784c562b91b48e3044aab29f7f2b7664da456f05e945bbc10f4b546b502",
            exe_in_archive: "llama-b9934/llama-server",
        })
    } else if cfg!(all(target_os = "macos", target_arch = "x86_64")) {
        Some(&LlamaServerBinaryPin {
            asset: "llama-b9934-bin-macos-x64.tar.gz",
            sha256: "4babcdd101adcd8b312655ce86cfa8ae3f97daa3c74decafbf9136cd4aaf40c6",
            exe_in_archive: "llama-b9934/llama-server",
        })
    } else if cfg!(all(target_os = "linux", target_arch = "aarch64")) {
        Some(&LlamaServerBinaryPin {
            asset: "llama-b9934-bin-ubuntu-arm64.tar.gz",
            sha256: "359515ef1290e64835b547475fcb84200b560bef5c400e6494520120146e4507",
            exe_in_archive: "llama-b9934/llama-server",
        })
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some(&LlamaServerBinaryPin {
            asset: "llama-b9934-bin-ubuntu-x64.tar.gz",
            sha256: "a01b9ec4522047a5e2e8abc17cc92795e5710b125e00026f4916d66f41553b67",
            exe_in_archive: "llama-b9934/llama-server",
        })
    } else {
        None
    }
}

/// Download URL for a pinned archive.
pub fn llama_server_download_url(pin: &LlamaServerBinaryPin) -> String {
    format!(
        "https://github.com/ggml-org/llama.cpp/releases/download/{LLAMA_SERVER_RELEASE_TAG}/{asset}",
        asset = pin.asset,
    )
}

/// Per-user directory where an operator-initiated install unpacks the
/// pinned llama.cpp archive; the last stop in the
/// [`locate_llama_server`] search chain. The build tag is a path
/// segment so a pin bump lands beside the old build instead of mixing
/// dynamic libraries from two builds in one directory. `None` when the
/// platform data directory cannot be located.
pub fn llama_server_managed_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| {
        d.join("bookrack")
            .join("llama-server")
            .join(LLAMA_SERVER_RELEASE_TAG)
    })
}

/// Outcome of the llama-server executable search.
#[derive(Debug)]
pub struct LlamaServerLocation {
    /// First candidate that exists as a file, if any.
    pub path: Option<PathBuf>,
    /// Every candidate that was checked, in search order.
    pub probed: Vec<PathBuf>,
}

/// Locate the `llama-server` executable to spawn.
///
/// The running executable's own directory (the release-archive layout)
/// is checked, then the per-user managed directory that `bookrack setup
/// reranker` populates. Nothing in the environment moves the search:
/// both stops hold the pinned build.
pub fn locate_llama_server() -> LlamaServerLocation {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.to_path_buf()));
    locate_llama_server_from(exe_dir, llama_server_managed_dir(), &|path| path.is_file())
}

/// Pure search logic for [`locate_llama_server`], factored out so the
/// chain can be tested without touching the filesystem.
// Parallel implementation: managed-lookup-chain.
fn locate_llama_server_from(
    exe_dir: Option<PathBuf>,
    managed_dir: Option<PathBuf>,
    is_file: &dyn Fn(&std::path::Path) -> bool,
) -> LlamaServerLocation {
    let candidates: Vec<PathBuf> = [
        exe_dir.map(|d| d.join("llama-server")),
        managed_dir
            .zip(pinned_llama_server())
            .map(|(d, pin)| d.join(pin.exe_in_archive)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let path = candidates.iter().find(|p| is_file(p)).cloned();
    LlamaServerLocation {
        path,
        probed: candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_for_this_target_is_well_formed() {
        let Some(pin) = pinned_llama_server() else {
            return;
        };
        assert_eq!(pin.sha256.len(), 64);
        assert!(pin.sha256.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(pin.asset.starts_with("llama-"));
        assert!(pin.asset.ends_with(".tar.gz"));
        assert!(pin.asset.contains(LLAMA_SERVER_RELEASE_TAG));
        assert!(pin.exe_in_archive.ends_with("llama-server"));
    }

    #[test]
    fn download_url_names_the_tag_and_asset() {
        let pin = LlamaServerBinaryPin {
            asset: "llama-test.tar.gz",
            sha256: "00",
            exe_in_archive: "llama-test/llama-server",
        };
        let url = llama_server_download_url(&pin);
        assert!(
            url.ends_with(&format!("/{LLAMA_SERVER_RELEASE_TAG}/llama-test.tar.gz")),
            "{url}"
        );
    }

    #[test]
    fn the_exe_directory_is_checked_before_the_managed_one() {
        let exe_side = PathBuf::from("exe/dir/llama-server");
        let found = locate_llama_server_from(
            Some(PathBuf::from("exe/dir")),
            Some(PathBuf::from("managed/dir")),
            &|_| true,
        );
        assert_eq!(found.path.as_ref(), Some(&exe_side));
        assert_eq!(found.probed.first(), Some(&exe_side));
    }

    #[test]
    fn managed_candidate_carries_the_archive_layout() {
        // Only meaningful on targets with a pin; elsewhere the chain
        // has no managed candidate at all.
        let found = locate_llama_server_from(None, Some(PathBuf::from("managed")), &|_| false);
        match pinned_llama_server() {
            Some(pin) => {
                assert_eq!(
                    found.probed,
                    vec![PathBuf::from("managed").join(pin.exe_in_archive)]
                );
            }
            None => assert!(found.probed.is_empty()),
        }
    }
}
