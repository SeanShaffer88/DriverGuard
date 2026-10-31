//! DriverGuard Windows platform layer.
//!
//! This is the only crate permitted to contain `unsafe` code (FFI wrappers
//! over CfgMgr32/SetupAPI and friends), and every `unsafe` block must carry
//! a comment stating the invariants it relies on.
//!
//! Module status: `inventory`, `backup`, `store` (with the events audit
//! log), `gui` (with four-view navigation), and `elevate` are implemented
//! for v0.1 See & Save; `policy` and `system` are documented stubs
//! returning [`WinError::NotImplemented`] until their releases land
//! (plan §8).

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
    #[error("could not run external tool {tool}: {detail}")]
    ToolUnavailable { tool: String, detail: String },
    #[error("backup verification failed: {0}")]
    Verification(String),
    #[error("filesystem error at {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
    #[error(transparent)]
    Manifest(#[from] dg_core::ManifestError),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("could not determine the local data directory: {0}")]
    DataDir(String),
    #[error("administrator approval was declined, so the operation did not run")]
    ElevationDeclined,
}

impl WinError {
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        WinError::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

pub type Result<T> = std::result::Result<T, WinError>;

pub mod elevate;
pub mod gui;
pub mod helper;

/// Current UTC time as an RFC 3339 string, for manifest timestamps.
pub fn now_iso8601_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    dg_core::iso8601_utc_from_unix(secs)
}

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
/// `HKLM\SOFTWARE\Policies\Microsoft\Windows\DeviceInstall\Restrictions`
/// (per-device deny list) and
/// `HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate`
/// (`ExcludeWUDriversInQualityUpdate`, the global lever 2 toggle).
///
/// Only the operations in [`dg_core::PolicyOp`] are supported — there is no
/// generic registry-write path, and the retroactive-restriction value
/// (`DenyDeviceIDsRetroactive`) is never written by this crate.
/// Device Installation Restrictions policy (plan §3.3).
///
/// Reads and writes
/// `HKLM\SOFTWARE\Policies\Microsoft\Windows\DeviceInstall\Restrictions`
/// (per-device deny list) and
/// `HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate`
/// (`ExcludeWUDriversInQualityUpdate`, the global lever 2 toggle).
///
/// Only the operations in [`dg_core::PolicyOp`] are supported — there is no
/// generic registry-write path, and the retroactive-restriction value
/// (`DenyDeviceIDsRetroactive`) is never written by this crate.
pub mod policy {
    use super::{Result, WinError};
    use dg_core::{LockPlan, PolicyOp, PolicyState};

    /// Read the current policy state. The registry is the source of truth;
    /// the app's database is only a cache (plan §1.1, principle 6).
    pub fn read_state() -> Result<PolicyState> {
        #[cfg(target_os = "windows")]
        {
            imp::read_state()
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(WinError::NotImplemented)
        }
    }

    /// Apply a lock/unlock plan produced by `dg-core`, verifying by
    /// re-reading after the write. Requires elevation (elevated helper).
    pub fn apply_plan(plan: &LockPlan) -> Result<()> {
        #[cfg(target_os = "windows")]
        {
            imp::apply_plan(plan)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = plan;
            Err(WinError::NotImplemented)
        }
    }

    #[cfg(target_os = "windows")]
    mod imp {
        use super::super::{Result, WinError};
        use super::{LockPlan, PolicyOp, PolicyState};
        use std::ffi::c_void;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, WIN32_ERROR};
        use windows::Win32::System::Registry::{
            RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegEnumValueW,
            RegGetValueW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
            KEY_WRITE, REG_DWORD, REG_SZ, RRF_RT_DWORD, RRF_RT_REG_SZ,
        };

        const RESTRICTIONS_KEY: &str =
            "SOFTWARE\\Policies\\Microsoft\\Windows\\DeviceInstall\\Restrictions";
        const DENY_SUBKEY: &str = "DenyDeviceIDs";
        const WU_KEY: &str = "SOFTWARE\\Policies\\Microsoft\\Windows\\WindowsUpdate";

        fn wide(s: &str) -> Vec<u16> {
            s.encode_utf16().chain(std::iter::once(0)).collect()
        }

        /// Closes a key; safe to call on a null/default HKEY.
        fn close(h: HKEY) {
            unsafe { let _ = RegCloseKey(h); }
        }

        /// RAII key closer.
        struct Key(Option<HKEY>);
        impl Key {
            fn none() -> Self { Key(None) }
            fn set(&mut self, h: HKEY) { self.0 = Some(h); }
            fn get(&self) -> Option<HKEY> { self.0 }
        }
        impl Drop for Key { fn drop(&mut self) { if let Some(h) = self.0 { close(h); } } }

        fn api_err(code: u32, msg: &'static str) -> WinError {
            WinError::Api { code, message: msg.to_owned() }
        }

        fn open_key(path_pcw: PCWSTR, access: u32) -> Result<Option<HKEY>> {
            let mut h = HKEY::default();
            let st = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path_pcw, 0, access, &mut h) };
            match WIN32_ERROR(st) {
                ERROR_FILE_NOT_FOUND => Ok(None),
                WIN32_ERROR(0) => Ok(Some(h)),
                other => Err(api_err(other.0, "RegOpenKeyExW failed")),
            }
        }

        fn create_key(path_pcw: PCWSTR) -> Result<HKEY> {
            let mut h = HKEY::default();
            let mut disp = 0u32;
            let st = unsafe {
                RegCreateKeyExW(
                    HKEY_LOCAL_MACHINE,
                    path_pcw,
                    0,
                    PCWSTR::null(),
                    0,
                    KEY_WRITE,
                    std::ptr::null(),
                    &mut h,
                    Some(&mut disp),
                )
            };
            if WIN32_ERROR(st) != WIN32_ERROR(0) {
                return Err(api_err(st, "RegCreateKeyExW failed"));
            }
            Ok(h)
        }

        fn create_subkey(parent: HKEY, sub_pcw: PCWSTR) -> Result<HKEY> {
            let mut h = HKEY::default();
            let mut disp = 0u32;
            let st = unsafe {
                RegCreateKeyExW(
                    parent,
                    sub_pcw,
                    0,
                    PCWSTR::null(),
                    0,
                    KEY_WRITE,
                    std::ptr::null(),
                    &mut h,
                    Some(&mut disp),
                )
            };
            if WIN32_ERROR(st) != WIN32_ERROR(0) {
                return Err(api_err(st, "RegCreateKeyExW (subkey) failed"));
            }
            Ok(h)
        }

        fn read_dword(h: HKEY, name_pcw: PCWSTR) -> Result<Option<u32>> {
            let mut v: u32 = 0;
            let mut sz = std::mem::size_of::<u32>() as u32;
            let mut ty = 0u32;
            let st = unsafe {
                RegGetValueW(
                    h,
                    PCWSTR::null(),
                    name_pcw,
                    RRF_RT_DWORD,
                    Some(&mut ty),
                    Some(&mut v as *mut u32 as *mut c_void),
                    Some(&mut sz),
                )
            };
            match WIN32_ERROR(st) {
                ERROR_FILE_NOT_FOUND => Ok(None),
                WIN32_ERROR(0) => Ok(Some(v)),
                other => Err(api_err(other.0, "RegGetValueW (DWORD) failed")),
            }
        }

        /// Read numbered REG_SZ values under the `DenyDeviceIDs` subkey,
        /// returned in numeric order.
        fn read_denied_ids(restrictions_h: HKEY) -> Result<Vec<String>> {
            let sub_w = wide(DENY_SUBKEY);
            let mut sub = HKEY::default();
            let st = unsafe {
                RegOpenKeyExW(restrictions_h, PCWSTR(sub_w.as_ptr()), 0, KEY_READ, &mut sub)
            };
            match WIN32_ERROR(st) {
                ERROR_FILE_NOT_FOUND => return Ok(Vec::new()),
                WIN32_ERROR(0) => {}
                other => return Err(api_err(other.0, "RegOpenKeyExW (DenyDeviceIDs) failed")),
            }
            let _guard = Key(Some(sub));
            let mut pairs: Vec<(u32, String)> = Vec::new();
            for idx in 0u32.. {
                let mut name_buf = [0u16; 512];
                let mut name_len = name_buf.len() as u32;
                let st = unsafe {
                    RegEnumValueW(
                        sub, idx,
                        windows::core::PWSTR(name_buf.as_mut_ptr()),
                        &mut name_len,
                        std::ptr::null(), std::ptr::null_mut(),
                        std::ptr::null_mut(), std::ptr::null_mut(),
                    )
                };
                if WIN32_ERROR(st) == ERROR_NO_MORE_ITEMS { break; }
                if WIN32_ERROR(st) != WIN32_ERROR(0) {
                    return Err(api_err(st, "RegEnumValueW failed"));
                }
                let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
                let Ok(num) = name.parse::<u32>() else { continue };
                let mut data = [0u16; 1024];
                let mut bytes = (data.len() * 2) as u32;
                let mut ty = 0u32;
                let name_pcw = PCWSTR(name_buf.as_ptr());
                let st2 = unsafe {
                    RegGetValueW(
                        sub, name_pcw, PCWSTR::null(),
                        RRF_RT_REG_SZ, Some(&mut ty),
                        Some(data.as_mut_ptr() as *mut c_void),
                        Some(&mut bytes),
                    )
                };
                if WIN32_ERROR(st2) != WIN32_ERROR(0) { continue; }
                let n_chars = (bytes / 2) as usize;
                let end = data[..n_chars].iter().position(|&u| u == 0).unwrap_or(n_chars);
                pairs.push((num, String::from_utf16_lossy(&data[..end])));
            }
            pairs.sort_by_key(|(n, _)| *n);
            Ok(pairs.into_iter().map(|(_, s)| s).collect())
        }

        fn write_dword(h: HKEY, name_pcw: PCWSTR, v: u32) -> Result<()> {
            let bytes = v.to_le_bytes();
            let st = unsafe { RegSetValueExW(h, name_pcw, 0, REG_DWORD, Some(&bytes)) };
            if WIN32_ERROR(st) != WIN32_ERROR(0) {
                return Err(api_err(st, "RegSetValueExW (DWORD) failed"));
            }
            Ok(())
        }

        fn delete_val(h: HKEY, name_pcw: PCWSTR) -> Result<()> {
            let st = unsafe { RegDeleteValueW(h, name_pcw) };
            if WIN32_ERROR(st) != WIN32_ERROR(0) && WIN32_ERROR(st) != ERROR_FILE_NOT_FOUND {
                return Err(api_err(st, "RegDeleteValueW failed"));
            }
            Ok(())
        }

        fn write_sz(h: HKEY, name_pcw: PCWSTR, text: &str) -> Result<()> {
            let data: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
            let bytes = unsafe {
                std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2)
            };
            let st = unsafe { RegSetValueExW(h, name_pcw, 0, REG_SZ, Some(bytes)) };
            if WIN32_ERROR(st) != WIN32_ERROR(0) {
                return Err(api_err(st, "RegSetValueExW (SZ) failed"));
            }
            Ok(())
        }

        /// Rewrite the numbered DenyDeviceIDs subkey from `entries`. If
        /// `entries` is empty the subkey is deleted entirely.
        fn rewrite_deny_list(parent: HKEY, entries: &[String]) -> Result<()> {
            let sub_w = wide(DENY_SUBKEY);
            // Open-or-create the subkey.
            let mut sub = HKEY::default();
            let mut disp = 0u32;
            let create_st = unsafe {
                RegCreateKeyExW(
                    parent, PCWSTR(sub_w.as_ptr()), 0, PCWSTR::null(), 0,
                    KEY_WRITE, std::ptr::null(), &mut sub, Some(&mut disp),
                )
            };
            if WIN32_ERROR(create_st) != WIN32_ERROR(0) {
                return Err(api_err(create_st, "RegCreateKeyExW (DenyDeviceIDs subkey) failed"));
            }
            let _g = Key(Some(sub));
            // Wipe existing numbered values.
            for n in 0u32..4096 {
                let nw = wide(&n.to_string());
                let st = unsafe { RegDeleteValueW(sub, PCWSTR(nw.as_ptr())) };
                if WIN32_ERROR(st) == ERROR_FILE_NOT_FOUND { break; }
            }
            if entries.is_empty() {
                // Close the subkey first, then delete it.
                drop(_g);
                let st = unsafe { RegDeleteKeyW(parent, PCWSTR(sub_w.as_ptr())) };
                if WIN32_ERROR(st) != WIN32_ERROR(0) && WIN32_ERROR(st) != ERROR_FILE_NOT_FOUND {
                    return Err(api_err(st, "RegDeleteKeyW (DenyDeviceIDs subkey) failed"));
                }
                return Ok(());
            }
            for (i, e) in entries.iter().enumerate() {
                let nw = wide(&i.to_string());
                write_sz(sub, PCWSTR(nw.as_ptr()), e)?;
            }
            Ok(())
        }

        pub(super) fn read_state() -> Result<PolicyState> {
            let mut state = PolicyState::default();
            let r_w = wide(RESTRICTIONS_KEY);
            let wu_w = wide(WU_KEY);
            if let Some(h) = open_key(PCWSTR(r_w.as_ptr()), KEY_READ)? {
                let _g = Key(Some(h));
                if let Some(v) = read_dword(h, PCWSTR(wide("DenyDeviceIDs").as_ptr()))? {
                    state.deny_enabled = v != 0;
                    state.deny_value_present = true;
                }
                state.denied_hardware_ids = read_denied_ids(h)?;
            }
            if let Some(h) = open_key(PCWSTR(wu_w.as_ptr()), KEY_READ)? {
                let _g = Key(Some(h));
                if let Some(v) = read_dword(h, PCWSTR(wide("ExcludeWUDriversInQualityUpdate").as_ptr()))? {
                    state.wu_exclude_drivers = v != 0;
                    state.wu_exclude_present = true;
                }
            }
            Ok(state)
        }

        pub(super) fn apply_plan(plan: &LockPlan) -> Result<()> {
            // Restore point is the caller's (helper_main) responsibility
            // before apply_plan is invoked; we just mutate policy.

            // Compute the target deny list / WU toggle by folding the
            // plan ops into the current read.
            let current = read_state()?;
            let mut denied = current.denied_hardware_ids.clone();
            let mut wu = current.wu_exclude_drivers;
            let mut deny_enabled = current.deny_enabled;
            for op in &plan.ops {
                match op {
                    PolicyOp::SetDenyEnabled(on) => deny_enabled = *on,
                    PolicyOp::AddDeniedHardwareId(id) => {
                        if !denied.iter().any(|e| e.eq_ignore_ascii_case(id)) {
                            denied.push(id.clone());
                        }
                    }
                    PolicyOp::RemoveDeniedHardwareId(id) => {
                        denied.retain(|e| !e.eq_ignore_ascii_case(id));
                    }
                    PolicyOp::SetWuExcludeDrivers(on) => wu = *on,
                    PolicyOp::DeleteDenySubkey => {
                        denied.clear();
                        deny_enabled = false;
                    }
                }
            }

            let touches_restrictions = plan.ops.iter().any(|o| {
                matches!(
                    o,
                    PolicyOp::SetDenyEnabled(_)
                        | PolicyOp::AddDeniedHardwareId(_)
                        | PolicyOp::RemoveDeniedHardwareId(_)
                        | PolicyOp::DeleteDenySubkey
                )
            });
            let touches_wu = plan
                .ops
                .iter()
                .any(|o| matches!(o, PolicyOp::SetWuExcludeDrivers(_)));

            let mut r_key = Key::none();
            let mut wu_key = Key::none();

            if touches_restrictions {
                let r_w = wide(RESTRICTIONS_KEY);
                let h = create_key(PCWSTR(r_w.as_ptr()))?;
                r_key.set(h);
                write_dword(h, PCWSTR(wide("DenyDeviceIDs").as_ptr()), if deny_enabled { 1 } else { 0 })?;
                rewrite_deny_list(h, &denied)?;
                if denied.is_empty() && !deny_enabled && !current.deny_value_present {
                    // Restore the "value was never there" state by
                    // deleting the DWORD we wrote.
                    delete_val(h, PCWSTR(wide("DenyDeviceIDs").as_ptr()))?;
                }
            }
            if touches_wu {
                let w_w = wide(WU_KEY);
                let h = create_key(PCWSTR(w_w.as_ptr()))?;
                wu_key.set(h);
                write_dword(
                    h,
                    PCWSTR(wide("ExcludeWUDriversInQualityUpdate").as_ptr()),
                    if wu { 1 } else { 0 },
                )?;
            }
            // Verification pass: re-read and sanity-check that the
            // values we asked for are present.
            let after = read_state()?;
            if touches_restrictions {
                let mut a = after.denied_hardware_ids.clone();
                a.sort_by(|x, y| x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()));
                let mut d = denied.clone();
                d.sort_by(|x, y| x.to_ascii_lowercase().cmp(&y.to_ascii_lowercase()));
                if a != d {
                    return Err(WinError::Verification(
                        "policy apply did not produce the expected deny list".to_owned(),
                    ));
                }
                if after.deny_enabled != deny_enabled {
                    return Err(WinError::Verification(
                        "policy apply did not set DenyDeviceIDs DWORD as expected".to_owned(),
                    ));
                }
            }
            if touches_wu && after.wu_exclude_drivers != wu {
                return Err(WinError::Verification(
                    "policy apply did not set ExcludeWUDriversInQualityUpdate as expected"
                        .to_owned(),
                ));
            }
            Ok(())
        }
    }
}

/// Driver package backup and restore via `pnputil` (plan §3.2, §3.5).
///
/// Results are verified by folder contents and our own inventory — never by
/// parsing `pnputil`'s human-readable output, which is localized.
pub mod backup {
    use super::{Path, Result, WinError};
    use dg_core::{BackupManifest, BackupPackage};
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    /// File name of the manifest inside every backup folder.
    pub const MANIFEST_FILE: &str = "manifest.json";

    /// One driver package folder found on disk by [`scan_packages`].
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct ScannedPackage {
        /// Folder name (relative to the backup root).
        pub folder: String,
        /// The package's INF file name as found in the folder, e.g. `oem42.inf`.
        pub inf_name: String,
        pub file_count: u32,
        pub total_bytes: u64,
    }

    /// Scan a backup/export folder: one package per immediate subfolder,
    /// identified by the single `.inf` file at the subfolder's top level.
    /// Subfolders without a top-level INF are not packages and are ignored.
    /// A subfolder with several top-level INFs is an error — it is not a
    /// shape `pnputil /export-driver` produces, and guessing would make
    /// verification meaningless.
    pub fn scan_packages(root: &Path) -> Result<Vec<ScannedPackage>> {
        let mut dirs: Vec<PathBuf> = Vec::new();
        let entries = std::fs::read_dir(root).map_err(|e| WinError::io(root, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| WinError::io(root, e))?;
            if entry.path().is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs.sort();

        let mut out = Vec::new();
        for dir in dirs {
            let files = walk_files(&dir)?;
            let infs: Vec<String> = files
                .iter()
                .filter(|p| p.parent() == Some(dir.as_path()))
                .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("inf")))
                .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .collect();
            match infs.len() {
                0 => continue,
                1 => {}
                n => {
                    return Err(WinError::Verification(format!(
                        "folder {} contains {n} INF files; expected exactly one per driver package",
                        dir.display()
                    )));
                }
            }
            let mut total_bytes: u64 = 0;
            for f in &files {
                total_bytes += std::fs::metadata(f).map_err(|e| WinError::io(f, e))?.len();
            }
            out.push(ScannedPackage {
                folder: dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                inf_name: infs.into_iter().next().unwrap_or_default(),
                file_count: files.len() as u32,
                total_bytes,
            });
        }
        Ok(out)
    }

    fn walk_files(dir: &Path) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let entries = std::fs::read_dir(dir).map_err(|e| WinError::io(dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| WinError::io(dir, e))?;
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk_files(&path)?);
            } else {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }

    /// The part of the destination contract that needs no writes and no
    /// elevation: an existing destination must be empty (plan §3.2).
    /// Checked up front by [`export_drivers_elevated`] so a doomed
    /// backup fails before any UAC prompt appears.
    fn validate_dest(dest: &Path) -> Result<()> {
        if dest.exists() {
            let mut entries = std::fs::read_dir(dest).map_err(|e| WinError::io(dest, e))?;
            if entries.next().is_some() {
                return Err(WinError::Verification(format!(
                    "backup destination {} is not empty; each backup needs its own new folder",
                    dest.display()
                )));
            }
        }
        Ok(())
    }

    /// Export drivers into `dest`, going through the elevated helper
    /// (plan §2.1) when this process is not already elevated: the user
    /// gets one UAC prompt, the helper performs the export, and the
    /// result is loaded back through the same [`verify_backup`] every
    /// backup must pass.
    pub fn export_drivers_elevated(dest: &Path) -> Result<BackupManifest> {
        validate_dest(dest)?;
        if crate::elevate::is_elevated()? {
            return export_drivers(dest);
        }
        crate::elevate::run_export_helper(dest)?;
        verify_backup(dest)
    }

    /// Export every third-party driver package into `dest`
    /// (`pnputil /export-driver *`), write the manifest, and verify the
    /// result before returning it. `dest` must be new or empty: each backup
    /// is its own folder (plan §3.2). Requires elevation.
    pub fn export_drivers(dest: &Path) -> Result<BackupManifest> {
        validate_dest(dest)?;
        if !dest.exists() {
            std::fs::create_dir_all(dest).map_err(|e| WinError::io(dest, e))?;
        }

        run_pnputil_export(dest)?;

        let scanned = scan_packages(dest)?;
        if scanned.is_empty() {
            return Err(WinError::Verification(
                "pnputil reported success but no driver packages were found in the export folder"
                    .to_owned(),
            ));
        }
        let manifest = build_manifest(&scanned);
        let json = manifest.to_json()?;
        std::fs::write(dest.join(MANIFEST_FILE), json)
            .map_err(|e| WinError::io(&dest.join(MANIFEST_FILE), e))?;
        // Self-check: the backup we just made must verify cleanly.
        verify_backup(dest)?;
        Ok(manifest)
    }

    fn run_pnputil_export(dest: &Path) -> Result<()> {
        let output = std::process::Command::new("pnputil")
            .arg("/export-driver")
            .arg("*")
            .arg(dest)
            .output()
            .map_err(|e| WinError::ToolUnavailable {
                tool: "pnputil".to_owned(),
                detail: e.to_string(),
            })?;
        if !output.status.success() {
            return Err(WinError::ToolFailed {
                tool: "pnputil".to_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            });
        }
        Ok(())
    }

    /// Build the manifest for an export: package identity comes from the
    /// folders on disk; provider, version, and served devices are joined
    /// from the live inventory by INF name (best effort — a store package
    /// no present device uses gets an empty provider and version 0.0.0.0,
    /// visibly, rather than invented data).
    fn build_manifest(scanned: &[ScannedPackage]) -> BackupManifest {
        let devices = crate::inventory::snapshot().unwrap_or_default();
        let mut by_inf: BTreeMap<String, (String, dg_core::DriverVersion, Vec<String>)> =
            BTreeMap::new();
        for ds in &devices {
            if let Some(drv) = &ds.driver {
                let entry = by_inf
                    .entry(drv.inf_name.to_lowercase())
                    .or_insert_with(|| (drv.provider.clone(), drv.version, Vec::new()));
                entry.2.push(ds.device.name.clone());
            }
        }
        let mut packages: Vec<BackupPackage> = scanned
            .iter()
            .map(|sp| {
                let (provider, version, devs) = by_inf
                    .get(&sp.inf_name.to_lowercase())
                    .cloned()
                    .unwrap_or_else(|| {
                        (
                            String::new(),
                            dg_core::DriverVersion(0, 0, 0, 0),
                            Vec::new(),
                        )
                    });
                BackupPackage {
                    inf_name: sp.inf_name.clone(),
                    provider,
                    version,
                    devices: devs,
                }
            })
            .collect();
        packages.sort_by_key(|a| a.inf_name.to_lowercase());
        BackupManifest {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            created_utc: crate::now_iso8601_utc(),
            packages,
        }
    }

    /// Verify a backup folder against its `manifest.json`: every manifest
    /// package must have a folder on disk (identified by its INF file),
    /// and every package folder on disk must be in the manifest.
    pub fn verify_backup(dir: &Path) -> Result<BackupManifest> {
        let manifest_path = dir.join(MANIFEST_FILE);
        let text =
            std::fs::read_to_string(&manifest_path).map_err(|e| WinError::io(&manifest_path, e))?;
        let manifest = BackupManifest::from_json(&text)?;
        let scanned = scan_packages(dir)?;

        let scanned_infs: BTreeSet<String> =
            scanned.iter().map(|s| s.inf_name.to_lowercase()).collect();
        for pkg in &manifest.packages {
            if !scanned_infs.contains(&pkg.inf_name.to_lowercase()) {
                return Err(WinError::Verification(format!(
                    "backup is missing the package folder for {} (provider {})",
                    pkg.inf_name, pkg.provider
                )));
            }
        }
        let manifest_infs: BTreeSet<String> = manifest
            .packages
            .iter()
            .map(|p| p.inf_name.to_lowercase())
            .collect();
        for sp in &scanned {
            if !manifest_infs.contains(&sp.inf_name.to_lowercase()) {
                return Err(WinError::Verification(format!(
                    "package folder {} ({}) is not listed in the manifest",
                    sp.folder, sp.inf_name
                )));
            }
        }
        Ok(manifest)
    }

    /// Install driver packages from a backup folder
    /// (`pnputil /add-driver ... /subdirs /install`). Requires elevation.
    /// Restore lands with the watcher release (plan §3.5).
    pub fn install_drivers(_backup_dir: &Path) -> Result<()> {
        Err(WinError::NotImplemented)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use dg_core::DriverVersion;
        use std::fs;

        fn temp_dir(tag: &str) -> PathBuf {
            // Unique per call: tests run in parallel threads in one
            // process, so sharing a directory per tag makes them race
            // (delete/recreate each other's fixtures) — this failed
            // intermittently when run repeatedly, and on Windows CI.
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("driverguard-test-{}-{tag}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn write(path: &Path, bytes: &[u8]) {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        /// A synthetic export tree: two packages (one with a nested file),
        /// one stray folder with no INF.
        fn fake_export() -> PathBuf {
            let root = temp_dir("export");
            write(&root.join("pkg-one").join("oem42.inf"), b"[Version]");
            write(&root.join("pkg-one").join("driver.sys"), b"sys!");
            write(&root.join("pkg-two").join("oem7.inf"), b"[Version]");
            write(&root.join("pkg-two").join("a.dll"), b"dll");
            write(&root.join("pkg-two").join("sub").join("b.dll"), b"dll2");
            write(&root.join("notes").join("readme.txt"), b"hello");
            root
        }

        fn manifest_for(root: &Path) -> BackupManifest {
            let scanned = scan_packages(root).unwrap();
            BackupManifest {
                app_version: "0.1.0".to_owned(),
                created_utc: "2026-10-20T00:00:00Z".to_owned(),
                packages: scanned
                    .iter()
                    .map(|sp| BackupPackage {
                        inf_name: sp.inf_name.clone(),
                        provider: "Test".to_owned(),
                        version: DriverVersion(1, 0, 0, 0),
                        devices: Vec::new(),
                    })
                    .collect(),
            }
        }

        #[test]
        fn scan_finds_packages_by_inf() {
            let root = fake_export();
            let scanned = scan_packages(&root).unwrap();
            assert_eq!(scanned.len(), 2);
            assert_eq!(scanned[0].inf_name, "oem42.inf");
            assert_eq!(scanned[0].file_count, 2);
            assert_eq!(scanned[0].total_bytes, 9 + 4);
            assert_eq!(scanned[1].inf_name, "oem7.inf");
            assert_eq!(scanned[1].file_count, 3);
            assert_eq!(scanned[1].total_bytes, 9 + 3 + 4);
            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn verify_accepts_matching_manifest() {
            let root = fake_export();
            let m = manifest_for(&root);
            fs::write(root.join(MANIFEST_FILE), m.to_json().unwrap()).unwrap();
            let back = verify_backup(&root).unwrap();
            assert_eq!(back.packages.len(), 2);
            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn verify_rejects_missing_package_folder() {
            let root = fake_export();
            let m = manifest_for(&root);
            fs::write(root.join(MANIFEST_FILE), m.to_json().unwrap()).unwrap();
            fs::remove_dir_all(root.join("pkg-two")).unwrap();
            let err = verify_backup(&root).unwrap_err();
            assert!(err.to_string().contains("oem7.inf"), "got: {err}");
            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn verify_rejects_unlisted_package_folder() {
            let root = fake_export();
            let m = manifest_for(&root);
            fs::write(root.join(MANIFEST_FILE), m.to_json().unwrap()).unwrap();
            write(&root.join("pkg-three").join("oem99.inf"), b"[Version]");
            assert!(verify_backup(&root).is_err());
            let _ = fs::remove_dir_all(&root);
        }

        #[test]
        fn export_refuses_non_empty_destination() {
            let root = temp_dir("dest");
            write(&root.join("existing.txt"), b"x");
            let err = export_drivers(&root).unwrap_err();
            assert!(matches!(err, WinError::Verification(_)), "got: {err}");
            let _ = fs::remove_dir_all(&root);
        }
    }
}

/// Local history store: snapshots and backup records in SQLite
/// (`db.sqlite` in the data directory, plan §2.2/§2.3).
///
/// The database is a cache and history — never the source of truth for
/// anything safety-relevant (policy state lives in the registry and is
/// re-read, plan §1.1). Losing or deleting the database loses history,
/// nothing else.
pub mod store {
    use super::{Path, Result, WinError};
    use dg_core::{
        BackupManifest, Device, DeviceSnapshot, DriverDate, DriverInfo, DriverVersion, Event,
        EventKind, LockRecord, PolicyState, Snapshot,
    };
    use rusqlite::{params, Connection, OptionalExtension};
    use std::path::PathBuf;

    const SCHEMA_V1: &str = "\
CREATE TABLE IF NOT EXISTS snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    taken_at TEXT NOT NULL,
    origin TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS driver_observations (
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
    instance_id TEXT NOT NULL,
    name TEXT NOT NULL,
    class TEXT NOT NULL,
    hardware_ids TEXT NOT NULL,
    provider TEXT,
    ver0 INTEGER, ver1 INTEGER, ver2 INTEGER, ver3 INTEGER,
    date_y INTEGER, date_m INTEGER, date_d INTEGER,
    inf_name TEXT
);
CREATE TABLE IF NOT EXISTS backups (
    folder TEXT PRIMARY KEY,
    created_utc TEXT NOT NULL,
    manifest_json TEXT NOT NULL
);
PRAGMA user_version = 1;
";

    // v2 adds the events audit log referenced by the plan §2.3 and by
    // every command that produces a user-visible outcome.
    const SCHEMA_V2: &str = "\
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    summary TEXT NOT NULL,
    detail_json TEXT
);
CREATE INDEX IF NOT EXISTS events_recorded_at_idx ON events(recorded_at DESC);
PRAGMA user_version = 2;
";

    // v3 adds the locks table so we can faithfully restore policy state
    // on a per-device unlock or unlock-everything (plan §2.3).
    const SCHEMA_V3: &str = "\
CREATE TABLE IF NOT EXISTS locks (
    hardware_id TEXT PRIMARY KEY,
    device_name TEXT NOT NULL,
    prior_state_json TEXT NOT NULL,
    applied_at TEXT NOT NULL
);
PRAGMA user_version = 3;
";

    /// The application data directory: `%LOCALAPPDATA%\DriverGuard`.
    pub fn data_dir() -> Result<PathBuf> {
        let base = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
            WinError::DataDir("the LOCALAPPDATA environment variable is not set".to_owned())
        })?;
        Ok(PathBuf::from(base).join("DriverGuard"))
    }

    /// An open history store.
    pub struct Store {
        conn: Connection,
    }

    impl Store {
        /// Open (creating if needed) the store in the default data
        /// directory.
        pub fn open_default() -> Result<Store> {
            let dir = data_dir()?;
            std::fs::create_dir_all(&dir).map_err(|e| WinError::io(&dir, e))?;
            Store::open_at(&dir.join("db.sqlite"))
        }

        /// Open (creating if needed) a store at an explicit path. Used by
        /// tests; production code uses [`Store::open_default`].
        pub fn open_at(path: &Path) -> Result<Store> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| WinError::io(parent, e))?;
            }
            let conn = Connection::open(path)?;
            conn.execute_batch("PRAGMA foreign_keys = ON;")?;
            let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
            match version {
                0 => {
                    conn.execute_batch(SCHEMA_V1)?;
                    conn.execute_batch(SCHEMA_V2)?;
                    conn.execute_batch(SCHEMA_V3)?;
                }
                1 => {
                    conn.execute_batch(SCHEMA_V2)?;
                    conn.execute_batch(SCHEMA_V3)?;
                }
                2 => {
                    conn.execute_batch(SCHEMA_V3)?;
                }
                3 => {}
                other => {
                    return Err(WinError::DataDir(format!(
                        "unsupported database schema version {other} (this build understands up to 3)"
                    )));
                }
            }
            Ok(Store { conn })
        }

        /// Record one inventory snapshot with all its device
        /// observations. Returns the new snapshot's ID.
        pub fn record_snapshot(
            &mut self,
            taken_at: &str,
            origin: &str,
            snapshot: &Snapshot,
        ) -> Result<i64> {
            let tx = self.conn.transaction()?;
            tx.execute(
                "INSERT INTO snapshots (taken_at, origin) VALUES (?1, ?2)",
                params![taken_at, origin],
            )?;
            let id = tx.last_insert_rowid();
            for ds in &snapshot.devices {
                let hw_json =
                    serde_json::to_string(&ds.device.hardware_ids).map_err(|e| WinError::Api {
                        code: 0,
                        message: format!("hardware ID serialization failed: {e}"),
                    })?;
                let (provider, version, date, inf_name) = match &ds.driver {
                    Some(d) => (
                        Some(d.provider.clone()),
                        Some(d.version),
                        Some(d.date),
                        Some(d.inf_name.clone()),
                    ),
                    None => (None, None, None, None),
                };
                tx.execute(
                    "INSERT INTO driver_observations (
                        snapshot_id, instance_id, name, class, hardware_ids,
                        provider, ver0, ver1, ver2, ver3, date_y, date_m, date_d, inf_name
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        id,
                        ds.device.instance_id,
                        ds.device.name,
                        ds.device.class,
                        hw_json,
                        provider,
                        version.map(|v| v.0),
                        version.map(|v| v.1),
                        version.map(|v| v.2),
                        version.map(|v| v.3),
                        date.map(|d| d.year),
                        date.map(|d| d.month),
                        date.map(|d| d.day),
                        inf_name,
                    ],
                )?;
            }
            tx.commit()?;
            Ok(id)
        }

        /// How many snapshots have been recorded.
        pub fn snapshot_count(&self) -> Result<u32> {
            let n: i64 = self
                .conn
                .query_row("SELECT COUNT(*) FROM snapshots", [], |r| r.get(0))?;
            Ok(n as u32)
        }

        /// The most recent snapshot, with the time it was taken.
        pub fn latest_snapshot(&self) -> Result<Option<(String, Snapshot)>> {
            let head: Option<(i64, String)> = self
                .conn
                .query_row(
                    "SELECT id, taken_at FROM snapshots ORDER BY id DESC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((id, taken_at)) = head else {
                return Ok(None);
            };
            let mut stmt = self.conn.prepare(
                "SELECT instance_id, name, class, hardware_ids,
                        provider, ver0, ver1, ver2, ver3, date_y, date_m, date_d, inf_name
                 FROM driver_observations WHERE snapshot_id = ?1 ORDER BY rowid",
            )?;
            let rows = stmt.query_map(params![id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<u32>>(5)?,
                    r.get::<_, Option<u32>>(6)?,
                    r.get::<_, Option<u32>>(7)?,
                    r.get::<_, Option<u32>>(8)?,
                    r.get::<_, Option<u32>>(9)?,
                    r.get::<_, Option<u32>>(10)?,
                    r.get::<_, Option<u32>>(11)?,
                    r.get::<_, Option<String>>(12)?,
                ))
            })?;
            let mut devices = Vec::new();
            for row in rows {
                let (
                    instance_id,
                    name,
                    class,
                    hw_json,
                    provider,
                    ver0,
                    ver1,
                    ver2,
                    ver3,
                    date_y,
                    date_m,
                    date_d,
                    inf_name,
                ) = row?;
                // Corrupt JSON in a cache row degrades to "no hardware
                // IDs recorded" rather than failing the whole read.
                let hardware_ids: Vec<String> = serde_json::from_str(&hw_json).unwrap_or_default();
                let driver = match (provider, ver0, date_y, inf_name) {
                    (Some(p), Some(v0), Some(y), Some(inf)) => Some(DriverInfo {
                        provider: p,
                        version: DriverVersion(
                            v0,
                            ver1.unwrap_or(0),
                            ver2.unwrap_or(0),
                            ver3.unwrap_or(0),
                        ),
                        date: DriverDate {
                            year: y as u16,
                            month: date_m.unwrap_or(0) as u8,
                            day: date_d.unwrap_or(0) as u8,
                        },
                        inf_name: inf,
                    }),
                    _ => None,
                };
                devices.push(DeviceSnapshot {
                    device: Device {
                        instance_id,
                        name,
                        class,
                        hardware_ids,
                    },
                    driver,
                });
            }
            Ok(Some((taken_at, Snapshot { devices })))
        }

        /// Record a completed backup (re-recording the same folder
        /// replaces its record).
        pub fn record_backup(&self, folder: &Path, manifest: &BackupManifest) -> Result<()> {
            self.conn.execute(
                "INSERT OR REPLACE INTO backups (folder, created_utc, manifest_json)
                 VALUES (?1, ?2, ?3)",
                params![
                    folder.display().to_string(),
                    manifest.created_utc,
                    manifest.to_json()?
                ],
            )?;
            Ok(())
        }

        /// All recorded backups (folder + manifest), newest first.
        pub fn list_backups(&self) -> Result<Vec<(String, BackupManifest)>> {
            let mut stmt = self
                .conn
                .prepare("SELECT folder, manifest_json FROM backups ORDER BY created_utc DESC")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (folder, json) = row?;
                out.push((folder, BackupManifest::from_json(&json)?));
            }
            Ok(out)
        }

        /// Append one event to the audit log. Returns the new event ID.
        pub fn record_event(
            &mut self,
            kind: EventKind,
            recorded_at: &str,
            summary: &str,
            detail_json: Option<&str>,
        ) -> Result<i64> {
            self.conn.execute(
                "INSERT INTO events (kind, recorded_at, summary, detail_json)
                 VALUES (?1, ?2, ?3, ?4)",
                params![kind.as_tag(), recorded_at, summary, detail_json],
            )?;
            Ok(self.conn.last_insert_rowid())
        }

        /// Record a per-device lock. Replaces any prior record for the
        /// same hardware ID (e.g. re-locking after an unlock).
        pub fn record_lock(&self, record: &LockRecord) -> Result<()> {
            let prior_json = serde_json::to_string(&record.prior).map_err(|e| WinError::Api {
                code: 0,
                message: format!("prior-state serialization failed: {e}"),
            })?;
            self.conn.execute(
                "INSERT OR REPLACE INTO locks (hardware_id, device_name, prior_state_json, applied_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![
                    record.hardware_id,
                    record.device_name,
                    prior_json,
                    record.applied_at,
                ],
            )?;
            Ok(())
        }

        /// Remove a lock record for the given hardware ID (after unlock).
        pub fn remove_lock(&self, hardware_id: &str) -> Result<()> {
            self.conn.execute(
                "DELETE FROM locks WHERE hardware_id = ?1",
                rusqlite::params![hardware_id],
            )?;
            Ok(())
        }

        /// All lock records, in hardware-id order.
        pub fn list_locks(&self) -> Result<Vec<LockRecord>> {
            let mut stmt = self.conn.prepare(
                "SELECT hardware_id, device_name, prior_state_json, applied_at
                 FROM locks ORDER BY hardware_id",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (hw, name, prior_json, at) = row?;
                let prior: PolicyState = serde_json::from_str(&prior_json).map_err(|e| {
                    WinError::Api { code: 0, message: format!("corrupt lock prior_state_json: {e}") }
                })?;
                out.push(LockRecord {
                    hardware_id: hw,
                    device_name: name,
                    prior,
                    applied_at: at,
                });
            }
            Ok(out)
        }

        /// Look up one lock record by hardware ID.
        pub fn get_lock(&self, hardware_id: &str) -> Result<Option<LockRecord>> {
            let mut stmt = self.conn.prepare(
                "SELECT hardware_id, device_name, prior_state_json, applied_at
                 FROM locks WHERE hardware_id = ?1",
            )?;
            let row = stmt.query_row(rusqlite::params![hardware_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            });
            match row {
                Ok((hw, name, prior_json, at)) => {
                    let prior: PolicyState = serde_json::from_str(&prior_json).map_err(|e| {
                        WinError::Api { code: 0, message: format!("corrupt lock prior_state_json: {e}") }
                    })?;
                    Ok(Some(LockRecord { hardware_id: hw, device_name: name, prior, applied_at: at }))
                }
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        }

        /// Recent events, newest first, up to `limit`.
        pub fn list_events(&self, limit: u32) -> Result<Vec<Event>> {
            let mut stmt = self.conn.prepare(
                "SELECT id, kind, recorded_at, summary, detail_json
                 FROM events ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = stmt.query_map(params![limit as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, tag, recorded_at, summary, detail_json) = row?;
                let kind = EventKind::from_tag(&tag).unwrap_or(EventKind::Inventory);
                out.push(Event {
                    id,
                    kind,
                    recorded_at,
                    summary,
                    detail_json,
                });
            }
            Ok(out)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use dg_core::{BackupPackage, Device, DeviceSnapshot, DriverInfo};
        use std::fs;

        fn temp_dir(tag: &str) -> PathBuf {
            // Unique per call — parallel tests must not share fixtures
            // (the backup-test race fixed in PR #2).
            use std::sync::atomic::{AtomicU64, Ordering};
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "driverguard-store-test-{}-{tag}-{n}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn sample_snapshot() -> Snapshot {
            Snapshot {
                devices: vec![
                    DeviceSnapshot {
                        device: Device {
                            instance_id: "PCI\\VEN_1002&DEV_73FF".to_owned(),
                            name: "Test GPU".to_owned(),
                            class: "Display".to_owned(),
                            hardware_ids: vec![
                                "PCI\\VEN_1002&DEV_73FF".to_owned(),
                                "PCI\\VEN_1002".to_owned(),
                            ],
                        },
                        driver: Some(DriverInfo {
                            provider: "AMD".to_owned(),
                            version: DriverVersion(31, 0, 15, 3621),
                            date: DriverDate {
                                year: 2026,
                                month: 6,
                                day: 15,
                            },
                            inf_name: "oem42.inf".to_owned(),
                        }),
                    },
                    DeviceSnapshot {
                        device: Device {
                            instance_id: "USB\\VID_0000".to_owned(),
                            name: "Driverless Thing".to_owned(),
                            class: "USB".to_owned(),
                            hardware_ids: Vec::new(),
                        },
                        driver: None,
                    },
                ],
            }
        }

        fn sample_manifest() -> BackupManifest {
            BackupManifest {
                app_version: "0.1.0".to_owned(),
                created_utc: "2026-10-20T00:00:00Z".to_owned(),
                packages: vec![BackupPackage {
                    inf_name: "oem42.inf".to_owned(),
                    provider: "AMD".to_owned(),
                    version: DriverVersion(31, 0, 15, 3621),
                    devices: vec!["Test GPU".to_owned()],
                }],
            }
        }

        #[test]
        fn snapshot_round_trips() {
            let dir = temp_dir("roundtrip");
            let mut store = Store::open_at(&dir.join("db.sqlite")).unwrap();
            let snap = sample_snapshot();
            let id = store
                .record_snapshot("2026-10-20T00:00:00Z", "test", &snap)
                .unwrap();
            assert!(id >= 1);
            assert_eq!(store.snapshot_count().unwrap(), 1);
            let (taken_at, back) = store.latest_snapshot().unwrap().unwrap();
            assert_eq!(taken_at, "2026-10-20T00:00:00Z");
            assert_eq!(back, snap);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn latest_is_the_most_recent() {
            let dir = temp_dir("latest");
            let mut store = Store::open_at(&dir.join("db.sqlite")).unwrap();
            store
                .record_snapshot("2026-10-20T00:00:00Z", "test", &sample_snapshot())
                .unwrap();
            let mut second = sample_snapshot();
            second.devices.truncate(1);
            store
                .record_snapshot("2026-10-20T01:00:00Z", "test", &second)
                .unwrap();
            assert_eq!(store.snapshot_count().unwrap(), 2);
            let (taken_at, back) = store.latest_snapshot().unwrap().unwrap();
            assert_eq!(taken_at, "2026-10-20T01:00:00Z");
            assert_eq!(back.devices.len(), 1);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn backups_round_trip_and_replace() {
            let dir = temp_dir("backups");
            let store = Store::open_at(&dir.join("db.sqlite")).unwrap();
            let folder = Path::new("C:\\backups\\2026-10-20");
            store.record_backup(folder, &sample_manifest()).unwrap();
            let listed = store.list_backups().unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].0, "C:\\backups\\2026-10-20");
            assert_eq!(listed[0].1, sample_manifest());
            // Re-recording the same folder replaces, not duplicates.
            let mut bigger = sample_manifest();
            bigger.packages.push(BackupPackage {
                inf_name: "oem7.inf".to_owned(),
                provider: "Intel".to_owned(),
                version: DriverVersion(1, 0, 0, 0),
                devices: Vec::new(),
            });
            store.record_backup(folder, &bigger).unwrap();
            let listed = store.list_backups().unwrap();
            assert_eq!(listed.len(), 1);
            assert_eq!(listed[0].1.packages.len(), 2);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn empty_store_reads_as_empty() {
            let dir = temp_dir("empty");
            let store = Store::open_at(&dir.join("db.sqlite")).unwrap();
            assert_eq!(store.snapshot_count().unwrap(), 0);
            assert!(store.latest_snapshot().unwrap().is_none());
            assert!(store.list_backups().unwrap().is_empty());
            assert!(store.list_events(10).unwrap().is_empty());
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn events_round_trip_newest_first() {
            let dir = temp_dir("events");
            let mut store = Store::open_at(&dir.join("db.sqlite")).unwrap();
            let id1 = store
                .record_event(
                    dg_core::EventKind::Inventory,
                    "2026-10-20T00:00:00Z",
                    "Recorded 1 device",
                    None,
                )
                .unwrap();
            let id2 = store
                .record_event(
                    dg_core::EventKind::BackupCreated,
                    "2026-10-20T00:01:00Z",
                    "Backed up 12 packages",
                    Some("{\"packages\":12}"),
                )
                .unwrap();
            assert!(id2 > id1);
            let listed = store.list_events(10).unwrap();
            assert_eq!(listed.len(), 2);
            // Newest first.
            assert_eq!(listed[0].kind, dg_core::EventKind::BackupCreated);
            assert_eq!(listed[0].summary, "Backed up 12 packages");
            assert_eq!(listed[0].detail_json.as_deref(), Some("{\"packages\":12}"));
            assert_eq!(listed[1].kind, dg_core::EventKind::Inventory);
            // Limit is respected.
            let one = store.list_events(1).unwrap();
            assert_eq!(one.len(), 1);
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        fn schema_v1_to_v2_migration_adds_events() {
            use rusqlite::Connection;
            let dir = temp_dir("migrate");
            let path = dir.join("db.sqlite");
            // Manually build a v1 database without the events table.
            {
                let conn = Connection::open(&path).unwrap();
                conn.execute_batch(SCHEMA_V1).unwrap();
                conn.execute_batch("PRAGMA user_version = 1;").unwrap();
            }
            // Opening must migrate to v2 without error and accept events.
            let mut store = Store::open_at(&path).unwrap();
            let id = store
                .record_event(
                    dg_core::EventKind::Inventory,
                    "2026-10-20T00:00:00Z",
                    "migrated",
                    None,
                )
                .unwrap();
            assert!(id >= 1);
            assert_eq!(store.list_events(10).unwrap().len(), 1);
            let _ = fs::remove_dir_all(&dir);
        }
    }
}

/// System integration: restore points, the watcher task, and toasts.
pub mod system {
    use super::{Result, WinError};

    /// Create a system restore point before a policy-changing operation
    /// (plan §1.1, principle 4). Requires elevation (elevated helper).
    ///
    /// On Windows this calls `SRSetRestorePointW` with
    /// `APPLICATION_INSTALL | BEGIN_SYSTEM_CHANGE`, which is the
    /// documented sequence for "an application is about to make changes
    /// the user might want to undo". If System Restore is disabled the
    /// call returns an error — callers surface this to the user for an
    /// explicit acknowledgement before proceeding, per plan §1.1.
    pub fn create_restore_point(description: &str) -> Result<()> {
        #[cfg(target_os = "windows")]
        {
            imp::create_restore_point(description)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = description;
            Err(WinError::NotImplemented)
        }
    }

    #[cfg(target_os = "windows")]
    mod imp {
        use super::super::{Result, WinError};
        use std::ffi::c_long;

        // SRSetRestorePoint lives in srclient.dll and is exposed with
        // the stdcall convention. These constants match the values in
        // srrestoreptapi.h from the Windows SDK.
        const BEGIN_SYSTEM_CHANGE: u32 = 0x0001;
        const APPLICATION_INSTALL: u32 = 0x0000;
        // MODIFY_SETTINGS would be a more accurate event type for a
        // policy change, but APPLICATION_INSTALL is what every published
        // PowerShell/Win32 sample uses for a generic "take a checkpoint"
        // operation, and it produces a normal restore point in System
        // Restore UI.

        #[repr(C)]
        struct RestorePointInfoW {
            dw_event_type: u32,
            dw_restore_pt_type: u32,
            sz_description: [u16; 256],
        }

        #[link(name = "srclient")]
        extern "system" {
            fn SRSetRestorePointW(
                p_smgr_status: *mut RestorePointInfoW,
                p_status: *mut c_long,
            ) -> i32; // BOOL as i32
        }

        pub(super) fn create_restore_point(description: &str) -> Result<()> {
            let mut info = RestorePointInfoW {
                dw_event_type: BEGIN_SYSTEM_CHANGE,
                dw_restore_pt_type: APPLICATION_INSTALL,
                sz_description: [0u16; 256],
            };
            let mut written = 0usize;
            for (i, u) in description.encode_utf16().take(255).enumerate() {
                info.sz_description[i] = u;
                written = i + 1;
            }
            info.sz_description[written] = 0;
            let mut seq: c_long = 0;
            // SAFETY: info and seq are live, stack-allocated out args;
            // SRSetRestorePointW does not retain them past the call.
            let ok = unsafe { SRSetRestorePointW(&mut info, &mut seq) };
            if ok == 0 {
                // Returns FALSE (0) on error; the STAT_* codes in
                // seq distinguish "service disabled" from "cancelled".
                // We surface the numeric code; the UI uses it to give
                // the user a clear message.
                Err(WinError::Api {
                    code: seq as u32,
                    message: format!(
                        "could not create a system restore point (status {seq:#x}). \
                         System Restore may be disabled."
                    ),
                })
            } else {
                Ok(())
            }
        }
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
