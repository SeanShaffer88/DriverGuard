//! Driver Guard entry point.
//!
//! Modes (plan §2.1): GUI by default, `--watch` for the headless scheduled
//! check, and `--elevated <op>` for the UAC helper. This v0.1 skeleton
//! implements argument handling and mode dispatch only; each mode reports
//! honestly that it is not implemented yet.

use std::process::ExitCode;

const USAGE: &str = "\
Driver Guard — keep the drivers you chose.

USAGE:
    driverguard              Launch the GUI
    driverguard --watch      Run one headless driver-change check and exit
    driverguard --version    Print version
    driverguard --help       Show this help

STATUS: v0.1 skeleton — no mode is functional yet. See ROADMAP.md.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => {
            println!("The Driver Guard GUI is not implemented yet (v0.1 skeleton).\n");
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
