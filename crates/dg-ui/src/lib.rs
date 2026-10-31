//! DriverGuard user interface (native Win32).
//!
//! The Drivers window (plan §6) uses a left navigation with four views
//! (Devices · Backups · History · Settings) and a status line. Long
//! work (inventory, backup) runs on worker threads and reports back
//! through the safe `dg-win::gui` host — this crate forbids `unsafe`
//! and holds no platform code.

#![forbid(unsafe_code)]

use dg_core::{DeviceSnapshot, DriverSource, Snapshot};
use dg_win::backup::MANIFEST_FILE;
use dg_win::gui::{Gui, GuiEvent, GuiHandle, WindowSpec};
use std::path::PathBuf;

/// The four top-level views, in navigation order (plan §6.2).
pub const VIEW_NAMES: [&str; 4] = ["Devices", "Backups", "History", "Settings"];

/// Index of the view currently selected in the left nav.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum View {
    Devices = 0,
    Backups = 1,
    History = 2,
    Settings = 3,
}

impl View {
    fn from_index(i: usize) -> View {
        match i {
            0 => View::Devices,
            1 => View::Backups,
            2 => View::History,
            _ => View::Settings,
        }
    }
}

/// Column layout per view. The list-view columns are rebuilt when the
/// user switches views, so each view can use its own headers.
fn columns_for(view: View) -> Vec<(String, i32)> {
    match view {
        View::Devices => vec![
            ("Device".to_owned(), 240),
            ("Class".to_owned(), 90),
            ("Provider".to_owned(), 150),
            ("Version".to_owned(), 110),
            ("Date".to_owned(), 95),
            ("Status".to_owned(), 140),
        ],
        View::Backups => vec![
            ("Created (UTC)".to_owned(), 160),
            ("Packages".to_owned(), 80),
            ("Location".to_owned(), 440),
        ],
        View::History => vec![
            ("When (UTC)".to_owned(), 160),
            ("Event".to_owned(), 140),
            ("Summary".to_owned(), 440),
        ],
        View::Settings => vec![
            ("Setting".to_owned(), 200),
            ("Value".to_owned(), 520),
        ],
    }
}

/// Action-button label per view. Only Devices exposes the backup action
/// (plan §6.2: one primary action per view).
fn action_label(view: View) -> &'static str {
    match view {
        View::Devices => "Back up all drivers",
        View::Backups => "Back up now",
        View::History => "Refresh",
        View::Settings => "Open data folder",
    }
}

/// Messages worker threads send back to the window.
enum Msg {
    InventoryLoaded(Result<Vec<DeviceSnapshot>, String>),
    BackupFinished(Result<BackupDone, String>),
    BackupsLoaded(Result<Vec<(String, dg_core::BackupManifest)>, String>),
    HistoryLoaded(Result<Vec<dg_core::Event>, String>),
}

/// A completed backup, as reported to the status line.
struct BackupDone {
    folder: String,
    packages: usize,
}

/// Window state (UI thread only).
#[derive(Default)]
struct App {
    view: View,
    backup_running: bool,
    /// Last loaded inventory, used to refresh the Devices view header
    /// and to build Settings/About summaries.
    last_device_count: Option<usize>,
    last_microsoft_count: Option<usize>,
}

/// Launch the GUI. Blocks until the window closes.
pub fn run_gui() -> Result<(), dg_win::WinError> {
    let initial_view = View::Devices;
    let spec = WindowSpec {
        title: "DriverGuard".to_owned(),
        columns: columns_for(initial_view),
        backup_label: action_label(initial_view).to_owned(),
        view_names: VIEW_NAMES.iter().map(|s| s.to_string()).collect(),
        width: 920,
        height: 600,
    };
    let mut app = App::default();
    dg_win::gui::run_window(spec, move |event, gui| app.on_event(event, gui))
}

impl App {
    fn switch_view(&mut self, view: View, gui: &Gui<Msg>) {
        self.view = view;
        gui.set_columns(&columns_for(view));
        gui.set_backup_label(action_label(view));
        gui.set_backup_enabled(!self.backup_running || view != View::Devices);
        match view {
            View::Devices => {
                if self.last_device_count.is_none() {
                    gui.set_status("Loading devices…");
                    spawn_inventory(gui.handle());
                } else {
                    gui.set_status(&self.device_summary());
                }
            }
            View::Backups => {
                gui.set_status("Loading backups…");
                spawn_backups_list(gui.handle());
            }
            View::History => {
                gui.set_status("Loading history…");
                spawn_history(gui.handle());
            }
            View::Settings => {
                self.render_settings(gui);
            }
        }
    }

    fn device_summary(&self) -> String {
        match (self.last_device_count, self.last_microsoft_count) {
            (Some(total), Some(ms)) => {
                format!("{total} devices · {ms} using a Microsoft driver")
            }
            _ => String::new(),
        }
    }

    fn render_settings(&self, gui: &Gui<Msg>) {
        let mut rows: Vec<Vec<String>> = Vec::new();
        rows.push(vec![
            "Version".to_owned(),
            format!("DriverGuard {}", env!("CARGO_PKG_VERSION")),
        ]);
        rows.push(vec![
            "Data folder".to_owned(),
            dg_win::store::data_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "(unavailable)".to_owned()),
        ]);
        rows.push(vec![
            "Lock drivers (v0.2)".to_owned(),
            "Planned — see docs/ROADMAP.md".to_owned(),
        ]);
        rows.push(vec![
            "Watcher (v0.3)".to_owned(),
            "Planned — see docs/ROADMAP.md".to_owned(),
        ]);
        rows.push(vec![
            "Network / telemetry".to_owned(),
            "DriverGuard makes no network calls.".to_owned(),
        ]);
        gui.set_rows(&rows);
        gui.set_status(&format!(
            "Settings. {}",
            self.device_summary()
        ));
    }

    fn on_event(&mut self, event: GuiEvent<Msg>, gui: &Gui<Msg>) {
        match event {
            GuiEvent::Started => {
                // Start on the Devices view (§6.2); switch_view kicks off
                // the initial inventory load.
                self.switch_view(View::Devices, gui);
            }
            GuiEvent::ViewSelected(idx) => {
                let new_view = View::from_index(idx);
                if new_view != self.view {
                    self.switch_view(new_view, gui);
                }
            }
            GuiEvent::BackupClicked => match self.view {
                View::Devices => {
                    if self.backup_running {
                        return;
                    }
                    self.backup_running = true;
                    gui.set_backup_enabled(false);
                    gui.set_status(
                        "Backing up all drivers… Windows may ask for administrator approval.",
                    );
                    spawn_backup(gui.handle());
                }
                View::Backups => {
                    // "Back up now" from the Backups view behaves the
                    // same as the Devices button but returns to the
                    // Backups list when done.
                    if self.backup_running {
                        return;
                    }
                    self.backup_running = true;
                    gui.set_backup_enabled(false);
                    gui.set_status(
                        "Backing up all drivers… Windows may ask for administrator approval.",
                    );
                    spawn_backup(gui.handle());
                }
                View::History => {
                    gui.set_status("Refreshing history…");
                    spawn_history(gui.handle());
                }
                View::Settings => {
                    // Open the local data folder in Explorer on Windows;
                    // a no-op on other platforms (the GUI itself is
                    // Windows-only in practice but we keep the stub honest).
                    let _ = dg_win::gui::open_folder_in_shell(
                        &dg_win::store::data_dir().unwrap_or_default(),
                    );
                }
            },
            GuiEvent::Worker(Msg::InventoryLoaded(Ok(devices))) => {
                let rows: Vec<Vec<String>> = devices.iter().map(row_for).collect();
                if self.view == View::Devices {
                    gui.set_rows(&rows);
                }
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
                self.last_device_count = Some(devices.len());
                self.last_microsoft_count = Some(microsoft);
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
                // Stay on the current view, but refresh Backups/History
                // if that is what the user is looking at.
                if self.view == View::Backups {
                    spawn_backups_list(gui.handle());
                } else if self.view == View::History {
                    spawn_history(gui.handle());
                }
            }
            GuiEvent::Worker(Msg::BackupFinished(Err(e))) => {
                self.backup_running = false;
                gui.set_backup_enabled(true);
                gui.set_status(&format!("Backup failed: {e}"));
                if self.view == View::History {
                    spawn_history(gui.handle());
                }
            }
            GuiEvent::Worker(Msg::BackupsLoaded(Ok(list))) => {
                let rows: Vec<Vec<String>> = list
                    .iter()
                    .map(|(folder, m)| {
                        vec![
                            m.created_utc.clone(),
                            m.packages.len().to_string(),
                            folder.clone(),
                        ]
                    })
                    .collect();
                gui.set_rows(&rows);
                gui.set_status(&format!("{} backup(s).", list.len()));
            }
            GuiEvent::Worker(Msg::BackupsLoaded(Err(e))) => {
                gui.set_status(&format!("Could not list backups: {e}"));
            }
            GuiEvent::Worker(Msg::HistoryLoaded(Ok(events))) => {
                let rows: Vec<Vec<String>> = events
                    .iter()
                    .map(|ev| {
                        vec![
                            ev.recorded_at.clone(),
                            ev.kind.as_tag().to_owned(),
                            ev.summary.clone(),
                        ]
                    })
                    .collect();
                gui.set_rows(&rows);
                gui.set_status(&format!("{} event(s) shown (newest first).", events.len()));
            }
            GuiEvent::Worker(Msg::HistoryLoaded(Err(e))) => {
                gui.set_status(&format!("Could not load history: {e}"));
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
                    let snap_id = store
                        .record_snapshot(&dg_win::now_iso8601_utc(), "gui", &snapshot)
                        .unwrap_or(0);
                    let _ = store.record_event(
                        dg_core::EventKind::Inventory,
                        &dg_win::now_iso8601_utc(),
                        &format!(
                            "Recorded {} devices (snapshot #{snap_id})",
                            devices.len()
                        ),
                        None,
                    );
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
    let dest: PathBuf = base.join(stamp.trim_end_matches('Z'));
    match dg_win::backup::export_drivers_elevated(&dest) {
        Ok(manifest) => {
            if let Ok(mut store) = dg_win::store::Store::open_default() {
                let _ = store.record_backup(&dest, &manifest);
                let detail = serde_json::json!({
                    "folder": dest.display().to_string(),
                    "package_count": manifest.packages.len(),
                    "manifest": MANIFEST_FILE,
                });
                let _ = store.record_event(
                    dg_core::EventKind::BackupCreated,
                    &dg_win::now_iso8601_utc(),
                    &format!(
                        "Backed up {} driver packages to {}",
                        manifest.packages.len(),
                        dest.display()
                    ),
                    Some(&detail.to_string()),
                );
            }
            Ok(BackupDone {
                folder: dest.display().to_string(),
                packages: manifest.packages.len(),
            })
        }
        Err(e) => {
            if let Ok(mut store) = dg_win::store::Store::open_default() {
                let detail = serde_json::json!({
                    "folder": dest.display().to_string(),
                    "error": e.to_string(),
                });
                let _ = store.record_event(
                    dg_core::EventKind::BackupFailed,
                    &dg_win::now_iso8601_utc(),
                    &format!("Backup to {} failed", dest.display()),
                    Some(&detail.to_string()),
                );
            }
            Err(e.to_string())
        }
    }
}

fn spawn_backups_list(handle: GuiHandle<Msg>) {
    std::thread::spawn(move || {
        let msg = match dg_win::store::Store::open_default() {
            Ok(store) => Msg::BackupsLoaded(store.list_backups().map_err(|e| e.to_string())),
            Err(e) => Msg::BackupsLoaded(Err(e.to_string())),
        };
        handle.post(msg);
    });
}

fn spawn_history(handle: GuiHandle<Msg>) {
    std::thread::spawn(move || {
        let msg = match dg_win::store::Store::open_default() {
            Ok(store) => Msg::HistoryLoaded(store.list_events(100).map_err(|e| e.to_string())),
            Err(e) => Msg::HistoryLoaded(Err(e.to_string())),
        };
        handle.post(msg);
    });
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

    #[test]
    fn four_views_are_declared() {
        assert_eq!(VIEW_NAMES.len(), 4);
        assert_eq!(VIEW_NAMES[0], "Devices");
        assert_eq!(VIEW_NAMES[3], "Settings");
    }
}
