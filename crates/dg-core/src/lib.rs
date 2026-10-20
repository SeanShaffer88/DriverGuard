//! Driver Guard core logic.
//!
//! This crate is pure logic: no Windows FFI and no clock/filesystem I/O.
//! Everything here is unit-testable on any platform. Platform access lives
//! in `dg-win`; presentation lives in `dg-ui`.
//!
//! The design contract is `docs/ARCHITECTURE_AND_UX_PLAN.md` in the
//! repository root.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

// ---------------------------------------------------------------------------
// Driver identity
// ---------------------------------------------------------------------------

/// A four-part Windows driver version, e.g. `31.0.15.3621`.
///
/// Ordering is lexicographic over the four components, most significant
/// first — the order Windows itself uses when comparing driver versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DriverVersion(pub u32, pub u32, pub u32, pub u32);

impl fmt::Display for DriverVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0, self.1, self.2, self.3)
    }
}

/// Where a driver came from, as reported by its provider name.
///
/// Classification is deliberately transparent: a driver counts as
/// Microsoft-supplied if and only if its provider string is exactly
/// `Microsoft`. There is no hidden scoring or reputation judgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriverSource {
    Microsoft,
    Vendor(String),
}

impl DriverSource {
    pub fn from_provider(provider: &str) -> Self {
        if provider == "Microsoft" {
            DriverSource::Microsoft
        } else {
            DriverSource::Vendor(provider.to_owned())
        }
    }
}

/// A calendar date, as reported for a driver by device properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DriverDate {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

impl fmt::Display for DriverDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

/// The driver currently bound to a device, as observed at snapshot time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverInfo {
    pub provider: String,
    pub version: DriverVersion,
    pub date: DriverDate,
    /// INF file name in the Driver Store, e.g. `oem42.inf`.
    pub inf_name: String,
}

impl DriverInfo {
    pub fn source(&self) -> DriverSource {
        DriverSource::from_provider(&self.provider)
    }
}

// ---------------------------------------------------------------------------
// Devices and snapshots
// ---------------------------------------------------------------------------

/// A device as enumerated by the OS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Device instance ID — the stable key across snapshots.
    pub instance_id: String,
    pub name: String,
    /// Device class name, e.g. `Display`, `Net`, `MEDIA`.
    pub class: String,
    pub hardware_ids: Vec<String>,
}

/// One device and the driver bound to it at snapshot time.
///
/// `driver` is `None` when no bound driver could be determined (devices
/// without a driver are real and must still appear in the inventory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceSnapshot {
    pub device: Device,
    pub driver: Option<DriverInfo>,
}

/// A point-in-time inventory of every device that could be read.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub devices: Vec<DeviceSnapshot>,
}

/// A detected change to one device's bound driver between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriverChange {
    pub instance_id: String,
    pub device_name: String,
    pub before: Option<DriverInfo>,
    pub after: Option<DriverInfo>,
}

/// Diff two snapshots by device instance ID.
///
/// A change is any difference in the bound driver (provider, version, date,
/// or INF) — including a provider change at an identical version, which is
/// exactly the Windows Update replacement case this project exists to catch.
pub fn diff_snapshots(before: &Snapshot, after: &Snapshot) -> Vec<DriverChange> {
    let before_by_id: BTreeMap<&str, &DeviceSnapshot> = before
        .devices
        .iter()
        .map(|d| (d.device.instance_id.as_str(), d))
        .collect();
    let after_by_id: BTreeMap<&str, &DeviceSnapshot> = after
        .devices
        .iter()
        .map(|d| (d.device.instance_id.as_str(), d))
        .collect();

    let mut changes = Vec::new();
    for (id, after_dev) in &after_by_id {
        match before_by_id.get(id) {
            None => changes.push(DriverChange {
                instance_id: (*id).to_owned(),
                device_name: after_dev.device.name.clone(),
                before: None,
                after: after_dev.driver.clone(),
            }),
            Some(before_dev) if before_dev.driver != after_dev.driver => {
                changes.push(DriverChange {
                    instance_id: (*id).to_owned(),
                    device_name: after_dev.device.name.clone(),
                    before: before_dev.driver.clone(),
                    after: after_dev.driver.clone(),
                });
            }
            _ => {}
        }
    }
    for (id, before_dev) in &before_by_id {
        if !after_by_id.contains_key(id) {
            changes.push(DriverChange {
                instance_id: (*id).to_owned(),
                device_name: before_dev.device.name.clone(),
                before: before_dev.driver.clone(),
                after: None,
            });
        }
    }
    changes
}

// ---------------------------------------------------------------------------
// Lock planning (Device Installation Restrictions policy)
// ---------------------------------------------------------------------------

/// A read of the Device Installation Restrictions policy, as found.
///
/// This models only what Driver Guard reads and writes. The retroactive
/// flag (`DenyDeviceIDsRetroactive`) is intentionally **not** modelled:
/// this product never sets it, because applying restrictions retroactively
/// can break devices that are already installed and working. Leaving it out
/// of the types makes the dangerous operation unrepresentable in a plan
/// (plan §3.3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyState {
    /// `DenyDeviceIDs` DWORD: the deny list is active.
    pub deny_enabled: bool,
    /// Whether the `DenyDeviceIDs` value existed when read. Absent vs
    /// present-but-zero matters when restoring the exact prior state.
    pub deny_value_present: bool,
    /// Hardware IDs currently in the deny list, in policy order.
    pub denied_hardware_ids: Vec<String>,
}

/// One primitive policy mutation. A plan is an ordered list of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyOp {
    SetDenyEnabled(bool),
    AddDeniedHardwareId(String),
    RemoveDeniedHardwareId(String),
}

/// An ordered, minimal plan to mutate policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockPlan {
    pub ops: Vec<PolicyOp>,
}

impl LockPlan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

fn contains_id(ids: &[String], hardware_id: &str) -> bool {
    ids.iter().any(|id| id.eq_ignore_ascii_case(hardware_id))
}

/// Plan locking one hardware ID, given the current policy state.
///
/// Minimal and append-only: entries already in the list (an admin's,
/// another tool's) are never removed or reordered by a lock plan.
pub fn plan_lock(current: &PolicyState, hardware_id: &str) -> LockPlan {
    let mut ops = Vec::new();
    if !current.deny_enabled {
        ops.push(PolicyOp::SetDenyEnabled(true));
    }
    if !contains_id(&current.denied_hardware_ids, hardware_id) {
        ops.push(PolicyOp::AddDeniedHardwareId(hardware_id.to_owned()));
    }
    LockPlan { ops }
}

/// Plan unlocking one hardware ID that Driver Guard previously locked.
///
/// Removes only that ID. The deny list itself is switched off only when it
/// becomes empty **and** `originally_enabled` is false — i.e. Driver Guard
/// restores the state it found, and never tears down a deny list that
/// predates it.
pub fn plan_unlock(current: &PolicyState, hardware_id: &str, originally_enabled: bool) -> LockPlan {
    let mut ops = Vec::new();
    if contains_id(&current.denied_hardware_ids, hardware_id) {
        ops.push(PolicyOp::RemoveDeniedHardwareId(hardware_id.to_owned()));
    }
    let remaining = current
        .denied_hardware_ids
        .iter()
        .filter(|id| !id.eq_ignore_ascii_case(hardware_id))
        .count();
    if remaining == 0 && !originally_enabled && current.deny_enabled {
        ops.push(PolicyOp::SetDenyEnabled(false));
    }
    LockPlan { ops }
}

// ---------------------------------------------------------------------------
// Backup manifest
// ---------------------------------------------------------------------------

/// One exported driver package in a backup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupPackage {
    pub inf_name: String,
    pub provider: String,
    pub version: DriverVersion,
    /// Names of devices this package serves.
    pub devices: Vec<String>,
}

/// The `manifest.json` written into every backup folder.
///
/// Backups are plain folders, usable without Driver Guard (e.g. by hand with
/// `pnputil /add-driver`); this file is metadata, not a lock-in format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    pub app_version: String,
    /// RFC 3339 timestamp, supplied by the caller (this crate does no I/O).
    pub created_utc: String,
    pub packages: Vec<BackupPackage>,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl BackupManifest {
    pub fn to_json(&self) -> Result<String, ManifestError> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        Ok(serde_json::from_str(text)?)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn driver(provider: &str, version: DriverVersion) -> DriverInfo {
        DriverInfo {
            provider: provider.to_owned(),
            version,
            date: DriverDate {
                year: 2026,
                month: 6,
                day: 15,
            },
            inf_name: "oem42.inf".to_owned(),
        }
    }

    fn device_snapshot(id: &str, drv: DriverInfo) -> DeviceSnapshot {
        DeviceSnapshot {
            device: Device {
                instance_id: id.to_owned(),
                name: format!("Device {id}"),
                class: "Display".to_owned(),
                hardware_ids: vec!["PCI\\VEN_1002&DEV_73FF".to_owned()],
            },
            driver: Some(drv),
        }
    }

    #[test]
    fn version_ordering_is_lexicographic() {
        assert!(DriverVersion(31, 0, 15, 3621) > DriverVersion(30, 0, 14, 9));
        assert!(DriverVersion(1, 0, 0, 1) > DriverVersion(1, 0, 0, 0));
        assert_eq!(DriverVersion(2, 0, 0, 0), DriverVersion(2, 0, 0, 0));
    }

    #[test]
    fn version_display_matches_windows_format() {
        assert_eq!(DriverVersion(31, 0, 15, 3621).to_string(), "31.0.15.3621");
    }

    #[test]
    fn source_classification_uses_provider_name() {
        assert_eq!(
            DriverSource::from_provider("Microsoft"),
            DriverSource::Microsoft
        );
        assert_eq!(
            DriverSource::from_provider("Advanced Micro Devices, Inc."),
            DriverSource::Vendor("Advanced Micro Devices, Inc.".to_owned())
        );
    }

    #[test]
    fn diff_is_empty_for_identical_snapshots() {
        let snap = Snapshot {
            devices: vec![device_snapshot(
                "A",
                driver("AMD", DriverVersion(1, 0, 0, 0)),
            )],
        };
        assert!(diff_snapshots(&snap, &snap).is_empty());
    }

    #[test]
    fn diff_detects_provider_change_at_same_version() {
        // The Windows Update replacement case: same version number,
        // different provider. This must count as a change.
        let v = DriverVersion(31, 0, 15, 3621);
        let before = Snapshot {
            devices: vec![device_snapshot("A", driver("AMD", v))],
        };
        let after = Snapshot {
            devices: vec![device_snapshot("A", driver("Microsoft", v))],
        };
        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before.as_ref().unwrap().provider, "AMD");
        assert_eq!(changes[0].after.as_ref().unwrap().provider, "Microsoft");
    }

    #[test]
    fn diff_detects_added_and_removed_devices() {
        let before = Snapshot {
            devices: vec![device_snapshot(
                "A",
                driver("AMD", DriverVersion(1, 0, 0, 0)),
            )],
        };
        let after = Snapshot {
            devices: vec![device_snapshot(
                "B",
                driver("Intel", DriverVersion(2, 0, 0, 0)),
            )],
        };
        let changes = diff_snapshots(&before, &after);
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn diff_detects_driver_appearing_and_disappearing() {
        let no_driver = Snapshot {
            devices: vec![DeviceSnapshot {
                device: Device {
                    instance_id: "A".to_owned(),
                    name: "Device A".to_owned(),
                    class: "Display".to_owned(),
                    hardware_ids: Vec::new(),
                },
                driver: None,
            }],
        };
        let with_driver = Snapshot {
            devices: vec![device_snapshot(
                "A",
                driver("AMD", DriverVersion(1, 0, 0, 0)),
            )],
        };
        let appeared = diff_snapshots(&no_driver, &with_driver);
        assert_eq!(appeared.len(), 1);
        assert!(appeared[0].before.is_none());
        assert!(appeared[0].after.is_some());
        let disappeared = diff_snapshots(&with_driver, &no_driver);
        assert_eq!(disappeared.len(), 1);
        assert!(disappeared[0].before.is_some());
        assert!(disappeared[0].after.is_none());
    }

    #[test]
    fn lock_plan_on_clean_policy_enables_and_adds() {
        let state = PolicyState::default();
        let plan = plan_lock(&state, "PCI\\VEN_1002&DEV_73FF");
        assert_eq!(
            plan.ops,
            vec![
                PolicyOp::SetDenyEnabled(true),
                PolicyOp::AddDeniedHardwareId("PCI\\VEN_1002&DEV_73FF".to_owned()),
            ]
        );
    }

    #[test]
    fn lock_plan_is_noop_when_already_locked() {
        let state = PolicyState {
            deny_enabled: true,
            deny_value_present: true,
            denied_hardware_ids: vec!["pci\\ven_1002&dev_73ff".to_owned()],
        };
        assert!(plan_lock(&state, "PCI\\VEN_1002&DEV_73FF").is_empty());
    }

    #[test]
    fn lock_plan_never_removes_foreign_entries() {
        let state = PolicyState {
            deny_enabled: true,
            deny_value_present: true,
            denied_hardware_ids: vec!["USB\\VID_1234&PID_5678".to_owned()],
        };
        let plan = plan_lock(&state, "PCI\\VEN_1002&DEV_73FF");
        assert!(plan
            .ops
            .iter()
            .all(|op| !matches!(op, PolicyOp::RemoveDeniedHardwareId(_))));
    }

    #[test]
    fn unlock_restores_originally_disabled_policy() {
        let state = PolicyState {
            deny_enabled: true,
            deny_value_present: true,
            denied_hardware_ids: vec!["PCI\\VEN_1002&DEV_73FF".to_owned()],
        };
        let plan = plan_unlock(&state, "PCI\\VEN_1002&DEV_73FF", false);
        assert_eq!(
            plan.ops,
            vec![
                PolicyOp::RemoveDeniedHardwareId("PCI\\VEN_1002&DEV_73FF".to_owned()),
                PolicyOp::SetDenyEnabled(false),
            ]
        );
    }

    #[test]
    fn unlock_keeps_preexisting_enabled_policy() {
        let state = PolicyState {
            deny_enabled: true,
            deny_value_present: true,
            denied_hardware_ids: vec!["PCI\\VEN_1002&DEV_73FF".to_owned()],
        };
        let plan = plan_unlock(&state, "PCI\\VEN_1002&DEV_73FF", true);
        assert_eq!(
            plan.ops,
            vec![PolicyOp::RemoveDeniedHardwareId(
                "PCI\\VEN_1002&DEV_73FF".to_owned()
            )]
        );
    }

    #[test]
    fn manifest_json_round_trips() {
        let manifest = BackupManifest {
            app_version: "0.1.0".to_owned(),
            created_utc: "2026-10-19T23:00:00Z".to_owned(),
            packages: vec![BackupPackage {
                inf_name: "oem42.inf".to_owned(),
                provider: "AMD".to_owned(),
                version: DriverVersion(31, 0, 15, 3621),
                devices: vec!["Device A".to_owned()],
            }],
        };
        let json = manifest.to_json().unwrap();
        let back = BackupManifest::from_json(&json).unwrap();
        assert_eq!(manifest, back);
    }
}
