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
    driverguard                     Launch the GUI (not yet implemented)
    driverguard inventory           Print the device/driver inventory as JSON
    driverguard backup <folder>     Back up all third-party driver packages
                                    into a new folder, with a verified manifest
                                    (requires an elevated terminal)
    driverguard verify-backup <folder>
                                    Verify a backup folder against its manifest
    driverguard --watch             Run one headless driver-change check (planned, v0.3)
    driverguard --version           Print version
    driverguard --help              Show this help
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
        Some("backup") => backup_cmd(&args),
        Some("verify-backup") => verify_backup_cmd(&args),
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

fn backup_cmd(args: &[String]) -> ExitCode {
    let Some(dir) = args.get(1) else {
        eprintln!("usage: driverguard backup <folder>\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    match dg_win::backup::export_drivers(std::path::Path::new(dir)) {
        Ok(manifest) => {
            println!(
                "Backed up {} driver packages to {dir}:",
                manifest.packages.len()
            );
            for p in &manifest.packages {
                println!("  {} — {} {}", p.inf_name, p.provider, p.version);
            }
            println!("Manifest written and verified.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("backup failed: {e}");
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
                "Backup at {dir} verified: {} packages, created {} (Driver Guard {}).",
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
