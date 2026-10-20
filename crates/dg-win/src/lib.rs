//! Driver Guard Windows platform layer.
//!
//! This is the only crate permitted to contain `unsafe` code (FFI wrappers
//! over CfgMgr32/SetupAPI and friends), and every `unsafe` block must carry
//! a comment stating the invariants it relies on.
//!
//! Module status: `inventory` is implemented (plan §3.1); `policy`,
//! `backup`, and `system` are documented stubs returning
//! [`WinError::NotImplemented`] until their releases land (plan §8).

// NOTE: `#![forbid(unsafe_code)]` is intentionally not set here — this crate
// is the designated FFI layer. It *is* set in `dg-core` and `dg-ui`.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum WinError {
    #[error("not implemented yet (planned for a later release)")]
    NotImplemented,
    #[error("Windows API error {code:#010x}: {message}")]
    Api { code: u32, message: String },
    #[error("external tool {tool} failed with exit code {exit_code}")]
    ToolFailed { tool: String, exit_code: i32 },
}

pub type Result<T> = std::result::Result<T, WinError>;

/// Parse a Windows driver version string (e.g. `"31.0.15.3621"`) into
/// components. Accepts 1–4 dot-separated numeric components; missing
/// trailing components default to 0. Returns `None` for anything else.
pub fn parse_driver_version(text: &str) -> Option<dg_core::DriverVersion> {
    let mut parts = [0u32; 4];
    let mut any = false;
    for (i, part) in text.trim().split('.').enumerate() {
        if i >= 4 || part.is_empty() {
            return None;
        }
        parts[i] = part.parse().ok()?;
        any = true;
    }
    if !any {
        return None;
    }
    Some(dg_core::DriverVersion(
        parts[0], parts[1], parts[2], parts[3],
    ))
}

/// Parse a UTF-16 multi-string (a list of NUL-terminated strings ended by
/// an empty string), as used by `DEVPKEY_Device_HardwareIds` and the
/// device ID list APIs.
pub fn parse_multi_sz(units: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in units.split(|&u| u == 0) {
        if chunk.is_empty() {
            break;
        }
        out.push(String::from_utf16_lossy(chunk));
    }
    out
}

/// Device and driver inventory (plan §3.1). Read-only; needs no elevation.
///
/// Implementation: enumerate present devices with CfgMgr32
/// (`CM_Get_Device_ID_ListW`), then read each device's bound driver from
/// device properties (`DEVPKEY_Device_DriverProvider` / `DriverVersion` /
/// `DriverDate` / `DriverInfPath`).
pub mod inventory {
    use super::{Result, WinError};
    use dg_core::DeviceSnapshot;

    /// Enumerate present devices and the driver bound to each.
    #[cfg(target_os = "windows")]
    pub fn snapshot() -> Result<Vec<DeviceSnapshot>> {
        imp::snapshot()
    }

    /// Enumerate present devices and the driver bound to each.
    #[cfg(not(target_os = "windows"))]
    pub fn snapshot() -> Result<Vec<DeviceSnapshot>> {
        Err(WinError::NotImplemented)
    }

    #[cfg(target_os = "windows")]
    mod imp {
        use super::{Result, WinError};
        use crate::{parse_driver_version, parse_multi_sz};
        use dg_core::{Device, DeviceSnapshot, DriverDate, DriverInfo};
        use windows::core::PCWSTR;
        use windows::Win32::Devices::DeviceAndDriverInstallation::{
            CM_Get_DevNode_PropertyW, CM_Get_Device_ID_ListW, CM_Get_Device_ID_List_SizeW,
            CM_Locate_DevNodeW, CM_GETIDLIST_FILTER_PRESENT, CM_LOCATE_DEVNODE_NORMAL, CONFIGRET,
            CR_BUFFER_SMALL, CR_NO_SUCH_VALUE, CR_SUCCESS,
        };
        use windows::Win32::Devices::Properties::{
            DEVPKEY_Device_Class, DEVPKEY_Device_DeviceDesc, DEVPKEY_Device_DriverDate,
            DEVPKEY_Device_DriverInfPath, DEVPKEY_Device_DriverProvider,
            DEVPKEY_Device_DriverVersion, DEVPKEY_Device_FriendlyName, DEVPKEY_Device_HardwareIds,
            DEVPROPTYPE, DEVPROP_TYPE_FILETIME, DEVPROP_TYPE_STRING, DEVPROP_TYPE_STRING_LIST,
        };
        use windows::Win32::Foundation::{DEVPROPKEY, FILETIME, SYSTEMTIME};
        use windows::Win32::System::Time::FileTimeToSystemTime;

        fn cr_error(cr: CONFIGRET) -> WinError {
            WinError::Api {
                code: cr.0,
                message: "CfgMgr32 call failed".to_owned(),
            }
        }

        pub fn snapshot() -> Result<Vec<DeviceSnapshot>> {
            let ids = device_id_list()?;
            let mut out = Vec::with_capacity(ids.len());
            for id in &ids {
                // A device can vanish between listing and locating
                // (hot-unplug race). Skipping it is correct; failing the
                // whole inventory is not.
                if let Ok(snap) = read_device(id) {
                    out.push(snap);
                }
            }
            Ok(out)
        }

        fn device_id_list() -> Result<Vec<String>> {
            let mut size_chars: u32 = 0;
            // SAFETY: a null filter asks for all devices; the size call
            // writes only through `size_chars`, a valid out pointer.
            let cr = unsafe {
                CM_Get_Device_ID_List_SizeW(
                    &mut size_chars,
                    PCWSTR::null(),
                    CM_GETIDLIST_FILTER_PRESENT,
                )
            };
            if cr != CR_SUCCESS {
                return Err(cr_error(cr));
            }
            let mut buf = vec![0u16; size_chars as usize];
            // SAFETY: `buf` holds exactly `size_chars` WCHARs as queried
            // above; the API writes at most that many, as a multi-string.
            let cr = unsafe {
                CM_Get_Device_ID_ListW(PCWSTR::null(), &mut buf, CM_GETIDLIST_FILTER_PRESENT)
            };
            if cr != CR_SUCCESS {
                return Err(cr_error(cr));
            }
            Ok(parse_multi_sz(&buf))
        }

        fn read_device(instance_id: &str) -> Result<DeviceSnapshot> {
            let wide_id: Vec<u16> = instance_id.encode_utf16().chain([0]).collect();
            let mut devinst: u32 = 0;
            // SAFETY: `wide_id` is NUL-terminated and outlives the call;
            // `devinst` is a valid out pointer.
            let cr = unsafe {
                CM_Locate_DevNodeW(
                    &mut devinst,
                    PCWSTR(wide_id.as_ptr()),
                    CM_LOCATE_DEVNODE_NORMAL,
                )
            };
            if cr != CR_SUCCESS {
                return Err(cr_error(cr));
            }
            let name = string_prop(devinst, &DEVPKEY_Device_FriendlyName)
                .or_else(|| string_prop(devinst, &DEVPKEY_Device_DeviceDesc))
                .unwrap_or_else(|| instance_id.to_owned());
            let class = string_prop(devinst, &DEVPKEY_Device_Class).unwrap_or_default();
            let hardware_ids = string_list_prop(devinst, &DEVPKEY_Device_HardwareIds);
            let driver = read_driver(devinst);
            Ok(DeviceSnapshot {
                device: Device {
                    instance_id: instance_id.to_owned(),
                    name,
                    class,
                    hardware_ids,
                },
                driver,
            })
        }

        /// The bound driver, or `None` unless provider, version, and date
        /// are all readable. Property reads are best-effort per property:
        /// one unreadable property must not blank the whole inventory.
        fn read_driver(devinst: u32) -> Option<DriverInfo> {
            let provider = string_prop(devinst, &DEVPKEY_Device_DriverProvider)?;
            let version_text = string_prop(devinst, &DEVPKEY_Device_DriverVersion)?;
            let version = parse_driver_version(&version_text)?;
            let date = filetime_prop(devinst, &DEVPKEY_Device_DriverDate)?;
            let inf_name = string_prop(devinst, &DEVPKEY_Device_DriverInfPath).unwrap_or_default();
            Some(DriverInfo {
                provider,
                version,
                date,
                inf_name,
            })
        }

        /// Raw property bytes and type, or `None` if the property is absent
        /// or unreadable. (Callers deliberately treat "unreadable" like
        /// "absent" — see `read_driver`.)
        fn property_bytes(devinst: u32, key: &DEVPROPKEY) -> Option<(DEVPROPTYPE, Vec<u8>)> {
            let mut prop_type = DEVPROPTYPE(0);
            let mut size: u32 = 0;
            // SAFETY: a null buffer with a valid size out-pointer queries
            // the required size; nothing is written through the null.
            let cr = unsafe {
                CM_Get_DevNode_PropertyW(devinst, key, &mut prop_type, None, &mut size, 0)
            };
            if cr == CR_NO_SUCH_VALUE || cr != CR_BUFFER_SMALL {
                return None;
            }
            let mut buf = vec![0u8; size as usize];
            let mut actual = size;
            // SAFETY: `buf` is `size` bytes as queried; the API writes at
            // most `actual` bytes and reports the real size back.
            let cr = unsafe {
                CM_Get_DevNode_PropertyW(
                    devinst,
                    key,
                    &mut prop_type,
                    Some(buf.as_mut_ptr()),
                    &mut actual,
                    0,
                )
            };
            if cr != CR_SUCCESS {
                return None;
            }
            buf.truncate(actual as usize);
            Some((prop_type, buf))
        }

        fn bytes_to_units(bytes: &[u8]) -> Vec<u16> {
            bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect()
        }

        fn string_prop(devinst: u32, key: &DEVPROPKEY) -> Option<String> {
            let (ty, bytes) = property_bytes(devinst, key)?;
            if ty != DEVPROP_TYPE_STRING {
                return None;
            }
            let units = bytes_to_units(&bytes);
            let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
            Some(String::from_utf16_lossy(&units[..end]))
        }

        fn string_list_prop(devinst: u32, key: &DEVPROPKEY) -> Vec<String> {
            let Some((ty, bytes)) = property_bytes(devinst, key) else {
                return Vec::new();
            };
            if ty != DEVPROP_TYPE_STRING_LIST {
                return Vec::new();
            }
            parse_multi_sz(&bytes_to_units(&bytes))
        }

        fn filetime_prop(devinst: u32, key: &DEVPROPKEY) -> Option<DriverDate> {
            let (ty, bytes) = property_bytes(devinst, key)?;
            if ty != DEVPROP_TYPE_FILETIME || bytes.len() < 8 {
                return None;
            }
            let ft = FILETIME {
                dwLowDateTime: u32::from_le_bytes(bytes[0..4].try_into().ok()?),
                dwHighDateTime: u32::from_le_bytes(bytes[4..8].try_into().ok()?),
            };
            let mut st = SYSTEMTIME::default();
            // SAFETY: `ft` is a well-formed FILETIME built from the
            // property bytes; `st` is a valid out pointer.
            unsafe { FileTimeToSystemTime(&ft, &mut st) }.ok()?;
            Some(DriverDate {
                year: st.wYear,
                month: st.wMonth as u8,
                day: st.wDay as u8,
            })
        }
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

#[cfg(test)]
mod tests {
    use super::{parse_driver_version, parse_multi_sz};
    use dg_core::DriverVersion;

    #[test]
    fn parses_four_part_version() {
        assert_eq!(
            parse_driver_version("31.0.15.3621"),
            Some(DriverVersion(31, 0, 15, 3621))
        );
    }

    #[test]
    fn parses_partial_version() {
        assert_eq!(parse_driver_version("1.2"), Some(DriverVersion(1, 2, 0, 0)));
    }

    #[test]
    fn rejects_malformed_versions() {
        assert_eq!(parse_driver_version(""), None);
        assert_eq!(parse_driver_version("a.b"), None);
        assert_eq!(parse_driver_version("1..2"), None);
        assert_eq!(parse_driver_version("1.2.3.4.5"), None);
    }

    #[test]
    fn multi_sz_parsing_stops_at_empty_string() {
        let units: Vec<u16> = "AB\0CD\0\0EF".encode_utf16().collect();
        assert_eq!(
            parse_multi_sz(&units),
            vec!["AB".to_owned(), "CD".to_owned()]
        );
    }
}

#[cfg(all(test, target_os = "windows"))]
mod windows_tests {
    /// Smoke test (plan §5): inventory must find at least one device on
    /// any real Windows machine — including CI runners.
    #[test]
    fn inventory_returns_present_devices() {
        let snaps = crate::inventory::snapshot().expect("inventory snapshot failed");
        assert!(
            !snaps.is_empty(),
            "expected at least one present device on a Windows machine"
        );
    }
}
