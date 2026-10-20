//! Driver Guard Windows platform layer.
//!
//! This is the only crate permitted to contain `unsafe` code (FFI wrappers
//! over CfgMgr32/SetupAPI and friends), and every `unsafe` block must carry
//! a comment stating the invariants it relies on.
//!
//! In the v0.1 skeleton the modules below are documented stubs returning
//! [`WinError::NotImplemented`]; implementations land PR by PR, following
//! `docs/ARCHITECTURE_AND_UX_PLAN.md`.

// NOTE: `#![forbid(unsafe_code)]` is intentionally not set here — this crate
// is the designated FFI layer. It *is* set in `dg-core` and `dg-ui`.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum WinError {
    #[error("not implemented yet (v0.1 skeleton)")]
    NotImplemented,
    #[error("Windows API error {code:#010x}: {message}")]
    Api { code: u32, message: String },
    #[error("external tool {tool} failed with exit code {exit_code}")]
    ToolFailed { tool: String, exit_code: i32 },
}

pub type Result<T> = std::result::Result<T, WinError>;

/// Device and driver inventory (plan §3.1).
///
/// Implementation: enumerate present devices with CfgMgr32
/// (`CM_Get_Device_ID_ListW`), then read the bound driver from device
/// properties (`DEVPKEY_Device_DriverProvider` / `DriverVersion` /
/// `DriverDate` / `DriverInfPath`). Read-only; needs no elevation.
pub mod inventory {
    use super::{Result, WinError};
    use dg_core::DeviceSnapshot;

    /// Enumerate present devices and the driver bound to each.
    pub fn snapshot() -> Result<Vec<DeviceSnapshot>> {
        Err(WinError::NotImplemented)
    }
}

/// Device Installation Restrictions policy (plan §3.3).
///
/// Reads and writes
/// `HKLM\SOFTWARE\Policies\Microsoft\Windows\DeviceInstall\Restrictions`.
/// Only the operations in [`dg_core::PolicyOp`] are supported — there is no
/// generic registry-write path, and the retroactive-restriction value is
/// never written by this crate.
pub mod policy {
    use super::{Result, WinError};
    use dg_core::{LockPlan, PolicyState};

    /// Read the current policy state. The registry is the source of truth;
    /// the app's database is only a cache (plan §1.1, principle 6).
    pub fn read_state() -> Result<PolicyState> {
        Err(WinError::NotImplemented)
    }

    /// Apply a lock/unlock plan produced by `dg-core`, verifying by
    /// re-reading after the write. Requires elevation (elevated helper).
    pub fn apply_plan(_plan: &LockPlan) -> Result<()> {
        Err(WinError::NotImplemented)
    }
}

/// Driver package backup and restore via `pnputil` (plan §3.2, §3.5).
///
/// Results are verified by folder contents and our own inventory — never by
/// parsing `pnputil`'s human-readable output, which is localized.
pub mod backup {
    use super::{Path, Result, WinError};

    /// Export every third-party driver package into `dest`
    /// (`pnputil /export-driver`). Requires elevation (elevated helper).
    pub fn export_drivers(_dest: &Path) -> Result<()> {
        Err(WinError::NotImplemented)
    }

    /// Install driver packages from a backup folder
    /// (`pnputil /add-driver ... /subdirs /install`). Requires elevation.
    pub fn install_drivers(_backup_dir: &Path) -> Result<()> {
        Err(WinError::NotImplemented)
    }
}

/// System integration: restore points, the watcher task, and toasts.
pub mod system {
    use super::{Result, WinError};

    /// Create a system restore point before a policy-changing operation
    /// (plan §1.1, principle 4). Requires elevation (elevated helper).
    pub fn create_restore_point(_description: &str) -> Result<()> {
        Err(WinError::NotImplemented)
    }

    /// Register the `DriverGuard Watch` scheduled task (logon + daily,
    /// plan §3.4).
    pub fn register_watch_task() -> Result<()> {
        Err(WinError::NotImplemented)
    }

    /// Remove the `DriverGuard Watch` scheduled task.
    pub fn unregister_watch_task() -> Result<()> {
        Err(WinError::NotImplemented)
    }

    /// Raise the "A driver changed" toast (plan §6.3). Watch mode only;
    /// called solely when a diff is non-empty — silence is the default.
    pub fn notify_driver_changed(_summary: &str) -> Result<()> {
        Err(WinError::NotImplemented)
    }
}
