//! DriverGuard core logic.
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
/// This models only what DriverGuard reads and writes. The retroactive
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
    /// The global "exclude drivers from Windows Update" toggle
    /// (plan §3.3, lever 2): `ExcludeWUDriversInQualityUpdate` under
    /// `HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate`.
    pub wu_exclude_drivers: bool,
    /// Whether the `ExcludeWUDriversInQualityUpdate` value existed
    /// before DriverGuard touched it — again used to restore the
    /// exact prior state on "unlock everything".
    pub wu_exclude_present: bool,
}

/// One primitive policy mutation. A plan is an ordered list of these.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PolicyOp {
    SetDenyEnabled(bool),
    /// Write a numbered value in the `DenyDeviceIDs` subkey (append).
    AddDeniedHardwareId(String),
    /// Delete the numbered value in the `DenyDeviceIDs` subkey (and
    /// renumber later values to keep a contiguous 0-based list).
    RemoveDeniedHardwareId(String),
    /// Toggle the global WU-driver-exclusion value (lever 2).
    SetWuExcludeDrivers(bool),
    /// Delete the `DenyDeviceIDs` subkey entirely (only used by
    /// "unlock everything" when we were the ones who created it and
    /// there are no foreign entries left).
    DeleteDenySubkey,
}

/// A lock record stored in the database for one hardware ID that
/// DriverGuard locked. It captures the policy state *as it was before
/// we touched this ID* so that unlock restores the prior state
/// faithfully (plan §1.1 principles 3 & 4, §3.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockRecord {
    /// Hardware ID this record covers.
    pub hardware_id: String,
    /// Human-friendly device name the lock was applied to, for UI.
    pub device_name: String,
    /// Policy state at the moment before the lock was applied.
    pub prior: PolicyState,
    /// RFC 3339 UTC timestamp the lock was applied.
    pub applied_at: String,
}

/// An ordered, minimal plan to mutate policy.
///
/// Plans are always produced against a read of the real registry state
/// (plan §1.1, principle 6), applied by the elevated helper (§2.1),
/// and verified by re-reading afterwards (§4). Every plan starts with
/// a request for a system restore point when `create_restore_point`
/// is true — the default for any operation that changes policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockPlan {
    pub create_restore_point: bool,
    /// Human-readable description for the restore point and the event log.
    pub description: String,
    pub ops: Vec<PolicyOp>,
}

impl LockPlan {
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// Build a plan that locks the WU-driver-exclusion toggle (lever 2)
/// to the requested state without touching the per-device deny list.
/// Returns `None` when the toggle is already at the requested state.
pub fn plan_set_wu_exclude(current: &PolicyState, enabled: bool) -> Option<LockPlan> {
    if current.wu_exclude_drivers == enabled {
        return None;
    }
    Some(LockPlan {
        create_restore_point: true,
        description: format!(
            "{} drivers from Windows Quality Updates",
            if enabled { "Exclude" } else { "Stop excluding" }
        ),
        ops: vec![PolicyOp::SetWuExcludeDrivers(enabled)],
    })
}

/// Build an "unlock everything" plan: remove every hardware ID
/// DriverGuard previously added, disable the deny list if we were the
/// ones who enabled it, and (optionally) reset the WU toggle back to
/// its state before us. The caller passes in the records it holds so
/// we can distinguish our entries from foreign ones.
pub fn plan_unlock_all(current: &PolicyState, our_records: &[LockRecord], reset_wu_toggle: bool) -> LockPlan {
    let our_ids: Vec<String> = our_records
        .iter()
        .map(|r| r.hardware_id.clone())
        .collect();
    let mut ops: Vec<PolicyOp> = Vec::new();
    let original_deny_enabled = our_records
        .iter()
        .map(|r| r.prior.deny_enabled)
        .min() // if any prior state had it enabled, we do not turn it off
        .unwrap_or(false);
    for id in &our_ids {
        if contains_id(&current.denied_hardware_ids, id) {
            ops.push(PolicyOp::RemoveDeniedHardwareId(id.clone()));
        }
    }
    let remaining_foreign: usize = current
        .denied_hardware_ids
        .iter()
        .filter(|id| !our_ids.iter().any(|ours| ours.eq_ignore_ascii_case(id)))
        .count();
    if remaining_foreign == 0 && !original_deny_enabled && current.deny_enabled {
        ops.push(PolicyOp::DeleteDenySubkey);
        ops.push(PolicyOp::SetDenyEnabled(false));
    }
    if reset_wu_toggle {
        // Find the oldest prior wu_exclude value across records —
        // that is the state before DriverGuard ever touched it.
        let original_wu = our_records
            .iter()
            .map(|r| r.prior.wu_exclude_drivers)
            .next()
            .unwrap_or(current.wu_exclude_drivers);
        if original_wu != current.wu_exclude_drivers {
            ops.push(PolicyOp::SetWuExcludeDrivers(original_wu));
        }
    }
    LockPlan {
        create_restore_point: true,
        description: "Unlock all drivers locked by DriverGuard".to_owned(),
        ops,
    }
}

fn contains_id(ids: &[String], hardware_id: &str) -> bool {
    ids.iter().any(|id| id.eq_ignore_ascii_case(hardware_id))
}

/// Plan locking one hardware ID, given the current policy state.
///
/// Minimal and append-only: entries already in the list (an admin's,
/// another tool's) are never removed or reordered by a lock plan.
/// The returned plan requests a system restore point (§1.1, principle 4).
pub fn plan_lock(current: &PolicyState, hardware_id: &str, device_name: &str) -> LockPlan {
    let mut ops = Vec::new();
    if !current.deny_enabled {
        ops.push(PolicyOp::SetDenyEnabled(true));
    }
    if !contains_id(&current.denied_hardware_ids, hardware_id) {
        ops.push(PolicyOp::AddDeniedHardwareId(hardware_id.to_owned()));
    }
    LockPlan {
        create_restore_point: true,
        description: format!("Lock driver for {device_name}"),
        ops,
    }
}

/// Plan unlocking one hardware ID that DriverGuard previously locked.
///
/// Removes only that ID. The deny list itself is switched off only when it
/// becomes empty **and** `originally_enabled` is false — i.e. DriverGuard
/// restores the state it found, and never tears down a deny list that
/// predates it.
pub fn plan_unlock(
    current: &PolicyState,
    hardware_id: &str,
    originally_enabled: bool,
    device_name: &str,
) -> LockPlan {
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
        ops.push(PolicyOp::DeleteDenySubkey);
        ops.push(PolicyOp::SetDenyEnabled(false));
    }
    LockPlan {
        create_restore_point: true,
        description: format!("Unlock driver for {device_name}"),
        ops,
    }
}

// ---------------------------------------------------------------------------
// History events
// ---------------------------------------------------------------------------

/// The kind of thing that happened. Driver-change events land in v0.3;
/// v0.1 records inventory runs and backup outcomes (plan §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    Inventory,
    BackupCreated,
    BackupFailed,
    LockApplied,
    LockRemoved,
    UnlockAll,
    PolicyChanged,
    RestorePointCreated,
    RestoreFailed,
}

impl EventKind {
    pub fn as_tag(&self) -> &'static str {
        match self {
            EventKind::Inventory => "inventory",
            EventKind::BackupCreated => "backup-created",
            EventKind::BackupFailed => "backup-failed",
            EventKind::LockApplied => "lock-applied",
            EventKind::LockRemoved => "lock-removed",
            EventKind::UnlockAll => "unlock-all",
            EventKind::PolicyChanged => "policy-changed",
            EventKind::RestorePointCreated => "restore-point-created",
            EventKind::RestoreFailed => "restore-failed",
        }
    }

    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "inventory" => Some(EventKind::Inventory),
            "backup-created" => Some(EventKind::BackupCreated),
            "backup-failed" => Some(EventKind::BackupFailed),
            "lock-applied" => Some(EventKind::LockApplied),
            "lock-removed" => Some(EventKind::LockRemoved),
            "unlock-all" => Some(EventKind::UnlockAll),
            "policy-changed" => Some(EventKind::PolicyChanged),
            "restore-point-created" => Some(EventKind::RestorePointCreated),
            "restore-failed" => Some(EventKind::RestoreFailed),
            _ => None,
        }
    }
}

/// One entry in the user-visible History log (plan §2.3 `events` table).
///
/// Everything DriverGuard does lands here so the user has an audit trail.
/// v0.1 records inventory snapshots and backup outcomes; v0.2 adds
/// lock/unlock and restore-point events; v0.3 adds driver-change detection
/// and restore events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: i64,
    pub kind: EventKind,
    /// RFC 3339 UTC timestamp.
    pub recorded_at: String,
    /// Short human-readable summary, e.g. "Backed up 12 driver packages".
    pub summary: String,
    /// Optional JSON detail blob (device/version before/after, folder, …).
    /// Free-form; the UI renders it in a Technical-details expander.
    pub detail_json: Option<String>,
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
/// Backups are plain folders, usable without DriverGuard (e.g. by hand with
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

/// Format a Unix timestamp (seconds since 1970-01-01T00:00:00Z) as an
/// RFC 3339 UTC string, e.g. `"2026-10-20T00:00:00Z"`. Pure arithmetic —
/// no clock access and no timezone database.
pub fn iso8601_utc_from_unix(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86_400) as i64;
    let secs_of_day = epoch_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hh, mm, ss) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

/// Inverse of days-from-civil (Howard Hinnant's algorithm): convert days
/// since 1970-01-01 into a (year, month, day) Gregorian date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era, [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year, [0, 365]
    let mp = (5 * doy + 2) / 153; // month index, [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
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
        let plan = plan_lock(&state, "PCI\\VEN_1002&DEV_73FF", "Test GPU");
        assert!(plan.create_restore_point);
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
            ..Default::default()
        };
        assert!(plan_lock(&state, "PCI\\VEN_1002&DEV_73FF", "Test GPU").is_empty());
    }

    #[test]
    fn lock_plan_never_removes_foreign_entries() {
        let state = PolicyState {
            deny_enabled: true,
            deny_value_present: true,
            denied_hardware_ids: vec!["USB\\VID_1234&PID_5678".to_owned()],
            ..Default::default()
        };
        let plan = plan_lock(&state, "PCI\\VEN_1002&DEV_73FF", "Test GPU");
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
            ..Default::default()
        };
        let plan = plan_unlock(&state, "PCI\\VEN_1002&DEV_73FF", false, "Test GPU");
        assert!(plan.create_restore_point);
        assert_eq!(
            plan.ops,
            vec![
                PolicyOp::RemoveDeniedHardwareId("PCI\\VEN_1002&DEV_73FF".to_owned()),
                PolicyOp::DeleteDenySubkey,
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
            ..Default::default()
        };
        let plan = plan_unlock(&state, "PCI\\VEN_1002&DEV_73FF", true, "Test GPU");
        assert_eq!(
            plan.ops,
            vec![PolicyOp::RemoveDeniedHardwareId(
                "PCI\\VEN_1002&DEV_73FF".to_owned()
            )]
        );
    }

    fn sample_record(hw: &str) -> LockRecord {
        LockRecord {
            hardware_id: hw.to_owned(),
            device_name: "Test GPU".to_owned(),
            prior: PolicyState::default(),
            applied_at: "2026-10-31T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn unlock_all_removes_only_our_entries_and_deletes_empty_key() {
        let current = PolicyState {
            deny_enabled: true,
            deny_value_present: true,
            denied_hardware_ids: vec![
                "PCI\\VEN_1002&DEV_73FF".to_owned(),
                "USB\\VID_1234&PID_5678".to_owned(),
            ],
            ..Default::default()
        };
        let records = vec![sample_record("PCI\\VEN_1002&DEV_73FF")];
        let plan = plan_unlock_all(&current, &records, false);
        // Only our ID is removed; foreign USB entry is left in place.
        assert!(plan.ops.contains(&PolicyOp::RemoveDeniedHardwareId(
            "PCI\\VEN_1002&DEV_73FF".to_owned()
        )));
        assert!(!plan.ops.contains(&PolicyOp::RemoveDeniedHardwareId(
            "USB\\VID_1234&PID_5678".to_owned()
        )));
        // Because a foreign entry remains, deny list stays enabled and
        // the subkey is NOT deleted.
        assert!(!plan.ops.contains(&PolicyOp::DeleteDenySubkey));
        assert!(!plan
            .ops
            .contains(&PolicyOp::SetDenyEnabled(false)));
    }

    #[test]
    fn unlock_all_clears_key_when_no_foreign_entries_remain() {
        let current = PolicyState {
            deny_enabled: true,
            deny_value_present: true,
            denied_hardware_ids: vec!["PCI\\VEN_1002&DEV_73FF".to_owned()],
            ..Default::default()
        };
        let records = vec![sample_record("PCI\\VEN_1002&DEV_73FF")];
        let plan = plan_unlock_all(&current, &records, true);
        assert!(plan.ops.contains(&PolicyOp::DeleteDenySubkey));
        assert!(plan.ops.contains(&PolicyOp::SetDenyEnabled(false)));
    }

    #[test]
    fn set_wu_exclude_is_noop_when_already_set() {
        let state = PolicyState {
            wu_exclude_drivers: true,
            ..Default::default()
        };
        assert!(plan_set_wu_exclude(&state, true).is_none());
    }

    #[test]
    fn set_wu_exclude_plans_toggle() {
        let state = PolicyState::default();
        let plan = plan_set_wu_exclude(&state, true).unwrap();
        assert!(plan.create_restore_point);
        assert_eq!(plan.ops, vec![PolicyOp::SetWuExcludeDrivers(true)]);
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

    #[test]
    fn iso8601_formatting() {
        assert_eq!(iso8601_utc_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso8601_utc_from_unix(1), "1970-01-01T00:00:01Z");
        // Leap day with a mid-day time.
        assert_eq!(iso8601_utc_from_unix(951_827_696), "2000-02-29T12:34:56Z");
        assert_eq!(iso8601_utc_from_unix(1_792_454_400), "2026-10-20T00:00:00Z");
    }

    #[test]
    fn event_kind_round_trips_via_tag() {
        for kind in [
            EventKind::Inventory,
            EventKind::BackupCreated,
            EventKind::BackupFailed,
        ] {
            assert_eq!(EventKind::from_tag(kind.as_tag()), Some(kind));
        }
        assert_eq!(EventKind::from_tag("not-a-kind"), None);
    }
}
