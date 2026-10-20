//! DriverGuard user interface (native Win32).
//!
//! The Drivers window (plan §6): a device/driver list fed by
//! `dg-win`'s inventory, and a one-click "Back up all drivers" action.
//! Long work (inventory, backup) runs on worker threads and reports
//! back through the safe `dg-win::gui` host — this crate forbids
//! `unsafe` and holds no platform code.

#![forbid(unsafe_code)]

use dg_core::{DeviceSnapshot, DriverSource, Snapshot};
use dg_win::gui::{Gui, GuiEvent, GuiHandle, WindowSpec};

/// The four top-level views, in navigation order (plan §6.2).
pub const VIEW_NAMES: [&str; 4] = ["Devices", "Backups", "History", "Settings"];

/// Messages worker threads send back to the window.
enum Msg {
    InventoryLoaded(Result<Vec<DeviceSnapshot>, String>),
    BackupFinished(Result<BackupDone, String>),
}

/// A completed backup, as reported to the status line.
struct BackupDone {
    folder: String,
    packages: usize,
}

/// Window state (UI thread only).
#[derive(Default)]
struct App {
    backup_running: bool,
}

/// Launch the GUI. Blocks until the window closes.
pub fn run_gui() -> Result<(), dg_win::WinError> {
    let spec = WindowSpec {
        title: "DriverGuard".to_owned(),
        columns: vec![
            ("Device".to_owned(), 240),
            ("Class".to_owned(), 90),
            ("Provider".to_owned(), 150),
            ("Version".to_owned(), 110),
            ("Date".to_owned(), 95),
            ("Status".to_owned(), 140),
        ],
        backup_label: "Back up all drivers".to_owned(),
        width: 920,
        height: 600,
    };
    let mut app = App::default();
    dg_win::gui::run_window(spec, move |event, gui| app.on_event(event, gui))
}

impl App {
    fn on_event(&mut self, event: GuiEvent<Msg>, gui: &Gui<Msg>) {
        match event {
            GuiEvent::Started => {
                gui.set_status("Loading devices…");
                spawn_inventory(gui.handle());
            }
            GuiEvent::BackupClicked => {
                if self.backup_running {
                    return;
                }
                self.backup_running = true;
                gui.set_backup_enabled(false);
                gui.set_status("Backing up all drivers… this can take a minute.");
                spawn_backup(gui.handle());
            }
            GuiEvent::Worker(Msg::InventoryLoaded(Ok(devices))) => {
                let rows: Vec<Vec<String>> = devices.iter().map(row_for).collect();
                gui.set_rows(&rows);
                let microsoft = devices
                    .iter()
                    .filter(|d| {
                        d.driver.as_ref().is_some_and(|dr| {
                            matches!(
                                DriverSource::from_provider(&dr.provider),
                                DriverSource::Microsoft
                            )
                        })
                    })
                    .count();
                gui.set_status(&format!(
                    "{} devices · {} using a Microsoft driver",
                    devices.len(),
                    microsoft
                ));
            }
            GuiEvent::Worker(Msg::InventoryLoaded(Err(e))) => {
                gui.set_status(&format!("Could not list devices: {e}"));
            }
            GuiEvent::Worker(Msg::BackupFinished(Ok(done))) => {
                self.backup_running = false;
                gui.set_backup_enabled(true);
                gui.set_status(&format!(
                    "Backup saved and verified: {} driver packages in {}",
                    done.packages, done.folder
                ));
            }
            GuiEvent::Worker(Msg::BackupFinished(Err(e))) => {
                self.backup_running = false;
                gui.set_backup_enabled(true);
                gui.set_status(&format!("Backup failed: {e}"));
            }
            GuiEvent::Closed => {}
        }
    }
}

/// One list row for a device (pure; unit-tested).
fn row_for(ds: &DeviceSnapshot) -> Vec<String> {
    let device = &ds.device;
    match &ds.driver {
        Some(d) => vec![
            device.name.clone(),
            device.class.clone(),
            d.provider.clone(),
            d.version.to_string(),
            d.date.to_string(),
            match DriverSource::from_provider(&d.provider) {
                DriverSource::Microsoft => "From Microsoft".to_owned(),
                DriverSource::Vendor(_) => "From vendor".to_owned(),
            },
        ],
        None => vec![
            device.name.clone(),
            device.class.clone(),
            "—".to_owned(),
            "—".to_owned(),
            "—".to_owned(),
            "No driver".to_owned(),
        ],
    }
}

/// Load the inventory on a worker thread, record it in history (like
/// the CLI does), and post the result to the window.
fn spawn_inventory(handle: GuiHandle<Msg>) {
    std::thread::spawn(move || {
        let msg = match dg_win::inventory::snapshot() {
            Ok(devices) => {
                if let Ok(mut store) = dg_win::store::Store::open_default() {
                    let snapshot = Snapshot {
                        devices: devices.clone(),
                    };
                    let _ = store.record_snapshot(&dg_win::now_iso8601_utc(), "gui", &snapshot);
                }
                Msg::InventoryLoaded(Ok(devices))
            }
            Err(e) => Msg::InventoryLoaded(Err(e.to_string())),
        };
        handle.post(msg);
    });
}

/// Export all drivers on a worker thread into
/// `%LOCALAPPDATA%\DriverGuard\backups\<timestamp>`, record the backup
/// in history, and post the result to the window.
fn spawn_backup(handle: GuiHandle<Msg>) {
    std::thread::spawn(move || {
        let msg = Msg::BackupFinished(run_backup());
        handle.post(msg);
    });
}

fn run_backup() -> Result<BackupDone, String> {
    let base = dg_win::store::data_dir()
        .map_err(|e| e.to_string())?
        .join("backups");
    let stamp = dg_win::now_iso8601_utc().replace(':', "-");
    let dest = base.join(stamp.trim_end_matches('Z'));
    let manifest = dg_win::backup::export_drivers(&dest).map_err(|e| e.to_string())?;
    if let Ok(store) = dg_win::store::Store::open_default() {
        let _ = store.record_backup(&dest, &manifest);
    }
    Ok(BackupDone {
        folder: dest.display().to_string(),
        packages: manifest.packages.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dg_core::{Device, DriverDate, DriverInfo, DriverVersion};

    fn device(driver: Option<DriverInfo>) -> DeviceSnapshot {
        DeviceSnapshot {
            device: Device {
                instance_id: "PCI\\TEST".to_owned(),
                name: "Test GPU".to_owned(),
                class: "Display".to_owned(),
                hardware_ids: vec!["PCI\\TEST".to_owned()],
            },
            driver,
        }
    }

    fn driver(provider: &str) -> DriverInfo {
        DriverInfo {
            provider: provider.to_owned(),
            version: DriverVersion(31, 0, 15, 3621),
            date: DriverDate {
                year: 2026,
                month: 6,
                day: 15,
            },
            inf_name: "oem42.inf".to_owned(),
        }
    }

    #[test]
    fn row_for_vendor_driver() {
        let row = row_for(&device(Some(driver("AMD"))));
        assert_eq!(
            row,
            vec![
                "Test GPU",
                "Display",
                "AMD",
                "31.0.15.3621",
                "2026-06-15",
                "From vendor"
            ]
        );
    }

    #[test]
    fn row_for_microsoft_driver() {
        let row = row_for(&device(Some(driver("Microsoft"))));
        assert_eq!(row[5], "From Microsoft");
    }

    #[test]
    fn row_for_driverless_device() {
        let row = row_for(&device(None));
        assert_eq!(row, vec!["Test GPU", "Display", "—", "—", "—", "No driver"]);
    }
}
