//! Driver Guard entry point.
//!
//! Modes (plan §2.1): GUI by default, `inventory` for a JSON inventory of
//! devices and their bound drivers, `--watch` for the headless scheduled
//! check, and `--elevated <op>` for the UAC helper. The GUI, watcher, and
//! elevated helper are not implemented yet; each reports that honestly.

use std::process::ExitCode;

const USAGE: &str = "\
Driver Guard — keep the drivers you chose.

USAGE:
    driverguard              Launch the GUI (not yet implemented)
    driverguard inventory    Print the device/driver inventory as JSON
    driverguard --watch      Run one headless driver-change check (planned, v0.3)
    driverguard --version    Print version
    driverguard --help       Show this help
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {
            println!("The Driver Guard GUI is not implemented yet (v0.1 in development).\n");
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("--version") => {
            println!("driverguard {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("inventory") => inventory(),
        Some("--watch") => {
            eprintln!("--watch is not implemented yet (planned for v0.3).");
            ExitCode::SUCCESS
        }
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
