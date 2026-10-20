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

    /// Export every third-party driver package into `dest`
    /// (`pnputil /export-driver *`), write the manifest, and verify the
    /// result before returning it. `dest` must be new or empty: each backup
    /// is its own folder (plan §3.2). Requires elevation.
    pub fn export_drivers(dest: &Path) -> Result<BackupManifest> {
        if dest.exists() {
            let mut entries = std::fs::read_dir(dest).map_err(|e| WinError::io(dest, e))?;
            if entries.next().is_some() {
                return Err(WinError::Verification(format!(
                    "backup destination {} is not empty; each backup needs its own new folder",
                    dest.display()
                )));
            }
        } else {
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
            let dir =
                std::env::temp_dir().join(format!("driverguard-test-{}-{tag}", std::process::id()));
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
