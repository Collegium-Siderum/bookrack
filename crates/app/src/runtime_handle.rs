// SPDX-License-Identifier: Apache-2.0

use bookrack_runtime::control::methods::MethodContext;

/// Thin newtype so `tauri::State` can hand out the daemon's
/// [`MethodContext`] (its bundle of cheap shared handles) to tray and
/// command code without holding the whole runtime.
pub struct RuntimeHandle(pub MethodContext);
