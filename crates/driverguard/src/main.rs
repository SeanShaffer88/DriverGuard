//! DriverGuard entry point.
//!
//! Modes (plan §2.1): GUI by default, `inventory` for a JSON inventory of
//! devices and their bound drivers, `--watch` for the headless scheduled
//! check, and `--elevated <op>` for the UAC helper. The GUI, watcher, and
//! elevated helper have separate implementation status; see --help.

use std::process::ExitCode;

const USAGE: &str = "\
DriverGuard — keep the drivers you chose.

USAGE:
    driverguard                       Launch the Devices window
    driverguard inventory             Print the device/driver inventory as JSON
    driverguard backup <folder>       Back up all third-party driver packages
                                      into a new folder, with a verified manifest
                                      (asks for administrator approval when needed)
    driverguard verify-backup <folder>
                                      Verify a backup folder against its manifest
    driverguard history               Show recorded snapshots, backups, and events
    driverguard lock <instance-id>    Lock the driver for a device (asks for approval)
    driverguard unlock <instance-id>  Unlock one device driver
    driverguard unlock-all            Remove all locks placed by DriverGuard
    driverguard exclude-wu-drivers <on|off>
                                      Toggle the global 'exclude drivers from Windows
                                      Quality Updates' policy
    driverguard status                Print current policy state and active locks
    driverguard --watch               Run one headless driver-change check (v0.3 watchdog)
    driverguard register-watch        Register the DriverGuard Watch scheduled task
                                      (runs --watch at logon)
    driverguard unregister-watch      Remove the DriverGuard Watch scheduled task
    driverguard --version             Print version
    driverguard --help                Show this help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => match dg_ui::run_gui() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("GUI failed to start: {e}\n");
                print!("{USAGE}");
                ExitCode::FAILURE
            }
        },
        Some("--help" | "-h") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--version") => {
            println!("driverguard {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--elevated") => ExitCode::from(dg_win::elevate::helper_main(&args[1..])),
        Some("inventory") => inventory(),
        Some("backup") => backup_cmd(&args),
        Some("verify-backup") => verify_backup_cmd(&args),
        Some("history") => history_cmd(),
        Some("lock") => lock_cmd(&args),
        Some("unlock") => unlock_cmd(&args),
        Some("unlock-all") => unlock_all_cmd(),
        Some("exclude-wu-drivers") => exclude_wu_cmd(&args),
        Some("status") => status_cmd(),
        Some("--watch") => watch_cmd(),
        Some("register-watch") => register_watch_cmd(),
        Some("unregister-watch") => unregister_watch_cmd(),
        Some(other) => {
            eprintln!("Unknown argument: {other}\n\n{USAGE}");
            ExitCode::FAILURE
        }
    }
}

fn inventory() -> ExitCode {
    match dg_win::inventory::snapshot() {
        Ok(devices) => {
            let snapshot = dg_core::Snapshot { devices };
            match serde_json::to_string_pretty(&snapshot) {
                Ok(json) => {
                    println!("{json}");
                    let device_count = snapshot.devices.len();
                    let snap_id = record_snapshot_best_effort(&snapshot, "cli");
                    record_event_best_effort(
                        dg_core::EventKind::Inventory,
                        &format!("Recorded {device_count} devices (snapshot #{snap_id})"),
                        Some(serde_json::json!({
                            "device_count": device_count,
                            "snapshot_id": snap_id,
                            "origin": "cli",
                        }).to_string()),
                    );
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("failed to serialize inventory: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(e) => {
            eprintln!("inventory failed: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Recording history is best-effort: the database is a cache (plan §2.3),
/// so a history failure warns on stderr but never fails the command the
/// user actually asked for. Returns the snapshot id when recorded, or 0.
fn record_snapshot_best_effort(snapshot: &dg_core::Snapshot, origin: &str) -> i64 {
    match dg_win::store::Store::open_default() {
        Ok(mut store) => match store.record_snapshot(&dg_win::now_iso8601_utc(), origin, snapshot) {
            Ok(id) => {
                eprintln!("(recorded as snapshot #{id} in history)");
                id
            }
            Err(e) => {
                eprintln!("warning: could not record snapshot in history: {e}");
                0
            }
        },
        Err(e) => {
            eprintln!("warning: history unavailable: {e}");
            0
        }
    }
}

fn record_backup_best_effort(folder: &std::path::Path, manifest: &dg_core::BackupManifest) {
    match dg_win::store::Store::open_default() {
        Ok(mut store) => {
            if let Err(e) = store.record_backup(folder, manifest) {
                eprintln!("warning: could not record backup in history: {e}");
            }
            let detail = serde_json::json!({
                "folder": folder.display().to_string(),
                "package_count": manifest.packages.len(),
                "created_utc": manifest.created_utc,
            });
            let _ = store.record_event(
                dg_core::EventKind::BackupCreated,
                &dg_win::now_iso8601_utc(),
                &format!(
                    "Backed up {} driver packages to {}",
                    manifest.packages.len(),
                    folder.display()
                ),
                Some(&detail.to_string()),
            );
        }
        Err(e) => eprintln!("warning: history unavailable: {e}"),
    }
}

fn record_event_best_effort(kind: dg_core::EventKind, summary: &str, detail: Option<String>) {
    match dg_win::store::Store::open_default() {
        Ok(mut store) => {
            let _ = store.record_event(kind, &dg_win::now_iso8601_utc(), summary, detail.as_deref());
        }
        Err(_) => {}
    }
}

fn backup_cmd(args: &[String]) -> ExitCode {
    let Some(dir) = args.get(1) else {
        eprintln!("usage: driverguard backup <folder>\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    let folder = std::path::Path::new(dir);
    match dg_win::backup::export_drivers_elevated(folder) {
        Ok(manifest) => {
            println!(
                "Backed up {} driver packages to {dir}:",
                manifest.packages.len()
            );
            for p in &manifest.packages {
                println!("  {} — {} {}", p.inf_name, p.provider, p.version);
            }
            println!("Manifest written and verified.");
            record_backup_best_effort(folder, &manifest);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("backup failed: {e}");
            let detail = serde_json::json!({
                "folder": dir,
                "error": e.to_string(),
            });
            record_event_best_effort(
                dg_core::EventKind::BackupFailed,
                &format!("Backup to {dir} failed"),
                Some(detail.to_string()),
            );
            ExitCode::FAILURE
        }
    }
}

fn verify_backup_cmd(args: &[String]) -> ExitCode {
    let Some(dir) = args.get(1) else {
        eprintln!("usage: driverguard verify-backup <folder>\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    match dg_win::backup::verify_backup(std::path::Path::new(dir)) {
        Ok(manifest) => {
            println!(
                "Backup at {dir} verified: {} packages, created {} (DriverGuard {}).",
                manifest.packages.len(),
                manifest.created_utc,
                manifest.app_version
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("verification failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn history_cmd() -> ExitCode {
    match history_report() {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("history failed: {e}");
            ExitCode::FAILURE
        }
    }
}

fn history_report() -> Result<String, dg_win::WinError> {
    let store = dg_win::store::Store::open_default()?;
    let count = store.snapshot_count()?;
    let latest = store.latest_snapshot()?;
    let backups = store.list_backups()?;
    let events = store.list_events(50)?;
    let locks = store.list_locks()?;
    let policy = dg_win::policy::read_state().unwrap_or_default();
    let mut out = String::from("DriverGuard history\n");
    out.push_str(&format!("  snapshots recorded: {count}\n"));
    match &latest {
        Some((taken_at, snap)) => {
            out.push_str(&format!(
                "  latest snapshot: {taken_at} ({} devices)\n",
                snap.devices.len()
            ));
        }
        None => out.push_str("  latest snapshot: none\n"),
    }
    out.push_str(&format!("  active locks: {}\n", locks.len()));
    for lr in &locks {
        out.push_str(&format!(
            "    {} ({}) since {}\n",
            lr.device_name, lr.hardware_id, lr.applied_at
        ));
    }
    out.push_str(&format!(
        "  per-device deny list: {} ({} entries)\n",
        if policy.deny_enabled { "enabled" } else { "disabled" },
        policy.denied_hardware_ids.len()
    ));
    out.push_str(&format!(
        "  exclude drivers from Windows Quality Updates: {}\n",
        if policy.wu_exclude_drivers { "on" } else { "off" }
    ));
    out.push_str(&format!("  backups: {}\n", backups.len()));
    for (folder, manifest) in &backups {
        out.push_str(&format!(
            "    {folder} — {} packages, created {}\n",
            manifest.packages.len(),
            manifest.created_utc
        ));
    }
    out.push_str(&format!("  recent events: {}\n", events.len()));
    if events.is_empty() {
        out.push_str("    (none yet — run `driverguard inventory` or `driverguard backup`)\n");
    } else {
        for ev in &events {
            out.push_str(&format!(
                "    [{}] {} — {}\n",
                ev.recorded_at,
                ev.kind.as_tag(),
                ev.summary
            ));
        }
    }
    Ok(out.trim_end().to_owned())
}

// ---- v0.2 lock commands --------------------------------------------------

fn find_device_by_instance<'a>(
    snap: &'a dg_core::Snapshot,
    instance_id: &str,
) -> Option<&'a dg_core::DeviceSnapshot> {
    snap.devices.iter().find(|d| {
        d.device.instance_id.eq_ignore_ascii_case(instance_id)
            || d.device.name.eq_ignore_ascii_case(instance_id)
    })
}

fn pick_hardware_id(ds: &dg_core::DeviceSnapshot) -> Option<String> {
    // Pick the most specific hardware ID (usually the first enumerated
    // one is the PCI/USB hardware match).
    ds.device.hardware_ids.first().cloned()
}

fn lock_cmd(args: &[String]) -> ExitCode {
    let Some(instance_id) = args.get(1) else {
        eprintln!("usage: driverguard lock <device-instance-id>\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    with_elevated_lock(|| -> Result<(String, String), String> {
        let devices = dg_win::inventory::snapshot().map_err(|e| e.to_string())?;
        let snap = dg_core::Snapshot { devices };
        let ds = find_device_by_instance(&snap, instance_id)
            .ok_or_else(|| format!("no device with instance id '{instance_id}'"))?;
        let hw = pick_hardware_id(ds).ok_or_else(|| "device has no hardware IDs to lock".to_string())?;
        let current = dg_win::policy::read_state().map_err(|e| e.to_string())?;
        let plan = dg_core::plan_lock(&current, &hw, &ds.device.name);
        if plan.is_empty() {
            return Ok((hw, format!("{} is already locked.", ds.device.name)));
        }
        let msg = dg_win::elevate::run_apply_lock(&plan).map_err(|e| e.to_string())?;
        let now = dg_win::now_iso8601_utc();
        let record = dg_core::LockRecord {
            hardware_id: hw.clone(),
            device_name: ds.device.name.clone(),
            prior: current,
            applied_at: now.clone(),
        };
        if let Ok(store) = dg_win::store::Store::open_default() {
            let mut store = store;
            let _ = store.record_lock(&record);
            let detail = serde_json::json!({
                "hardware_id": hw,
                "device": ds.device.name,
            });
            let _ = store.record_event(
                dg_core::EventKind::LockApplied,
                &now,
                &format!("Locked driver for {} ({})", ds.device.name, msg),
                Some(&detail.to_string()),
            );
        }
        Ok((hw, format!("Locked {} ({}). {}", ds.device.name, hw, msg)))
    })
}

fn unlock_cmd(args: &[String]) -> ExitCode {
    let Some(instance_id) = args.get(1) else {
        eprintln!("usage: driverguard unlock <device-instance-id>\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    with_elevated_lock(|| -> Result<(String, String), String> {
        let devices = dg_win::inventory::snapshot().map_err(|e| e.to_string())?;
        let snap = dg_core::Snapshot { devices };
        let ds = find_device_by_instance(&snap, instance_id)
            .ok_or_else(|| format!("no device with instance id '{instance_id}'"))?;
        let hw = pick_hardware_id(ds).ok_or_else(|| "device has no hardware IDs".to_string())?;
        let store = dg_win::store::Store::open_default().map_err(|e| e.to_string())?;
        let record = store
            .get_lock(&hw)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("{} is not locked by DriverGuard", ds.device.name))?;
        let current = dg_win::policy::read_state().map_err(|e| e.to_string())?;
        let originally_enabled = record.prior.deny_enabled;
        let plan = dg_core::plan_unlock(&current, &hw, originally_enabled, &ds.device.name);
        if plan.is_empty() {
            return Ok((hw, format!("{} is already unlocked.", ds.device.name)));
        }
        let msg = dg_win::elevate::run_apply_lock(&plan).map_err(|e| e.to_string())?;
        if let Ok(mut store) = dg_win::store::Store::open_default() {
            let _ = store.remove_lock(&hw);
            let now = dg_win::now_iso8601_utc();
            let detail = serde_json::json!({
                "hardware_id": hw,
                "device": ds.device.name,
            });
            let _ = store.record_event(
                dg_core::EventKind::LockRemoved,
                &now,
                &format!("Unlocked driver for {} ({})", ds.device.name, msg),
                Some(&detail.to_string()),
            );
        }
        Ok((hw, format!("Unlocked {} ({}). {}", ds.device.name, hw, msg)))
    })
}

fn unlock_all_cmd() -> ExitCode {
    with_elevated_lock(|| -> Result<(String, String), String> {
        let store = dg_win::store::Store::open_default().map_err(|e| e.to_string())?;
        let records = store.list_locks().map_err(|e| e.to_string())?;
        if records.is_empty() {
            return Ok((String::new(), "No DriverGuard locks to remove.".to_owned()));
        }
        let current = dg_win::policy::read_state().map_err(|e| e.to_string())?;
        let plan = dg_core::plan_unlock_all(&current, &records, true);
        if plan.is_empty() {
            return Ok((String::new(), "No changes needed.".to_owned()));
        }
        let msg = dg_win::elevate::run_apply_lock(&plan).map_err(|e| e.to_string())?;
        if let Ok(mut store) = dg_win::store::Store::open_default() {
            for r in &records {
                let _ = store.remove_lock(&r.hardware_id);
            }
            let now = dg_win::now_iso8601_utc();
            let detail = serde_json::json!({
                "unlocked_count": records.len(),
            });
            let _ = store.record_event(
                dg_core::EventKind::UnlockAll,
                &now,
                &format!("Unlocked {} drivers ({})", records.len(), msg),
                Some(&detail.to_string()),
            );
        }
        Ok((String::new(), format!("Unlocked {} drivers. {}", records.len(), msg)))
    })
}

fn exclude_wu_cmd(args: &[String]) -> ExitCode {
    let Some(flag) = args.get(1) else {
        eprintln!("usage: driverguard exclude-wu-drivers <on|off>\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    let on = match flag.as_str() {
        "on" | "true" | "1" => true,
        "off" | "false" | "0" => false,
        other => {
            eprintln!("expected 'on' or 'off' (got '{other}')\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    with_elevated_lock(|| -> Result<(String, String), String> {
        let current = dg_win::policy::read_state().map_err(|e| e.to_string())?;
        let Some(plan) = dg_core::plan_set_wu_exclude(&current, on) else {
            return Ok((String::new(), format!("Exclude WU drivers is already {}.", if on { "on" } else { "off" })));
        };
        let msg = dg_win::elevate::run_apply_lock(&plan).map_err(|e| e.to_string())?;
        if let Ok(mut store) = dg_win::store::Store::open_default() {
            let now = dg_win::now_iso8601_utc();
            let detail = serde_json::json!({ "enabled": on });
            let _ = store.record_event(
                dg_core::EventKind::PolicyChanged,
                &now,
                &format!(
                    "Exclude drivers from Windows Quality Updates → {} ({})",
                    if on { "on" } else { "off" },
                    msg
                ),
                Some(&detail.to_string()),
            );
        }
        Ok((String::new(), format!("Exclude WU drivers set to {}. {}", if on { "on" } else { "off" }, msg)))
    })
}

/// Watch mode: perform one inventory snapshot, diff it against the
/// last saved snapshot, and if anything changed record a
/// DriverChanged event and raise a toast. This is the entry point
/// invoked by the `DriverGuard Watch` scheduled task (plan §3.4).
///
/// Exit code is always SUCCESS — watch mode is best-effort and
/// must never pop an error dialog on the user's desktop; failures
/// are recorded in the history log as RestoreFailed-driver style
/// error entries so they are visible to the user on next open.
fn watch_cmd() -> ExitCode {
    let mut store = match dg_win::store::Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("watch: could not open store: {e}");
            return ExitCode::SUCCESS;
        }
    };
    let before: Option<dg_core::Snapshot> = match store.latest_snapshot() {
        Ok(Some((_, snap))) => Some(snap),
        _ => None,
    };
    let devices = match dg_win::inventory::snapshot() {
        Ok(d) => d,
        Err(e) => {
            let _ = store.record_event(
                dg_core::EventKind::RestoreFailedDriver,
                "watch: could not enumerate devices",
                &e.to_string(),
                None,
            );
            return ExitCode::SUCCESS;
        }
    };
    let after = dg_core::Snapshot { devices };
    let Some(prior) = before else {
        // First run ever (or no snapshot yet) — record the current
        // inventory silently. Plan §3.4: silence is the default; only
        // fire a notification on a change.
        let _ = store.record_snapshot(&dg_win::now_iso8601_utc(), "watch", &after);
        return ExitCode::SUCCESS;
    };
    let diff = dg_core::diff_snapshots(&prior, &after);
    if diff.is_empty() {
        return ExitCode::SUCCESS;
    }
    let n_added = diff.iter().filter(|c| c.before.is_none() && c.after.is_some()).count();
    let n_removed = diff.iter().filter(|c| c.before.is_some() && c.after.is_none()).count();
    let n_changed = diff.iter().filter(|c| c.before.is_some() && c.after.is_some()).count();
    let summary = format!(
        "DriverGuard found {n_changed} driver version changes, {n_added} new devices, {n_removed} removed."
    );
    let detail_changes: Vec<serde_json::Value> = diff
        .iter()
        .map(|c| {
            let kind = match (&c.before, &c.after) {
                (None, Some(_)) => "added",
                (Some(_), None) => "removed",
                (Some(_), Some(_)) => "driver-changed",
                _ => "unknown",
            };
            serde_json::json!({
                "instance_id": c.instance_id,
                "device_name": c.device_name,
                "change": kind,
            })
        })
        .collect();
    let snap_id = match store.record_snapshot(&dg_win::now_iso8601_utc(), "watch", &after) {
        Ok(id) => Some(id),
        Err(_) => None,
    };
    let detail_json =
        serde_json::to_string(&serde_json::json!({ "snapshot_id": snap_id, "changes": detail_changes }))
            .unwrap_or_default();
    let _ = store.record_event(
        dg_core::EventKind::DriverChanged,
        &summary,
        "",
        Some(&detail_json),
    );
    let _ = dg_win::system::notify_driver_changed(&summary);
    ExitCode::SUCCESS
}

fn register_watch_cmd() -> ExitCode {
    let exe = match dg_win::system::current_exe_path() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("could not determine own exe path: {e}");
            return ExitCode::FAILURE;
        }
    };
    match dg_win::system::register_watch_task(&exe) {
        Ok(()) => {
            println!("Registered DriverGuard Watch scheduled task (logon trigger).");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("could not register watch task: {e}");
            ExitCode::FAILURE
        }
    }
}

fn unregister_watch_cmd() -> ExitCode {
    match dg_win::system::unregister_watch_task() {
        Ok(()) => {
            println!("Removed DriverGuard Watch scheduled task.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("could not unregister watch task: {e}");
            ExitCode::FAILURE
        }
    }
}

fn status_cmd() -> ExitCode {
    match status_report() {
        Ok(text) => { println!("{text}"); ExitCode::SUCCESS }
        Err(e) => { eprintln!("status failed: {e}"); ExitCode::FAILURE }
    }
}

fn status_report() -> Result<String, dg_win::WinError> {
    let policy = dg_win::policy::read_state().unwrap_or_default();
    let store = dg_win::store::Store::open_default()?;
    let locks = store.list_locks()?;
    let mut out = String::from("DriverGuard status\n");
    out.push_str(&format!(
        "  per-device deny list: {}\n",
        if policy.deny_enabled { "enabled" } else { "disabled" }
    ));
    out.push_str(&format!("  denied hardware IDs: {}\n", policy.denied_hardware_ids.len()));
    for id in &policy.denied_hardware_ids {
        out.push_str(&format!("    - {id}\n"));
    }
    out.push_str(&format!(
        "  exclude drivers from Windows Quality Updates: {}\n",
        if policy.wu_exclude_drivers { "on" } else { "off" }
    ));
    out.push_str(&format!("  locks recorded: {}\n", locks.len()));
    for r in &locks {
        out.push_str(&format!(
            "    - {} ({}) locked at {}\n",
            r.device_name, r.hardware_id, r.applied_at
        ));
    }
    Ok(out.trim_end().to_owned())
}

fn with_elevated_lock<F>(f: F) -> ExitCode
where
    F: FnOnce() -> Result<(String, String), String>,
{
    // Pre-flight best-effort restore point check / creation is done
    // inside the elevated helper (apply_plan honors
    // plan.create_restore_point). Here we just surface the result.
    match f() {
        Ok((_hw, msg)) => {
            println!("{msg}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
