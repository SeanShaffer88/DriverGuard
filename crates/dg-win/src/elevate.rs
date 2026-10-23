//! Elevated helper support (plan §2.1).
//!
//! Driver export (`pnputil /export-driver`) needs an elevated process,
//! but the GUI and CLI normally run unelevated. The bridge is the plan's
//! helper mode: the unelevated process relaunches itself through the
//! Windows UAC prompt (`ShellExecute` with the `runas` verb) as
//! `driverguard --elevated <op> [args]`, and the helper performs exactly
//! one allowlisted operation and exits.
//!
//! v0.1 implements one operation, `export-drivers`. The remaining plan
//! operations (`apply-lock`, `remove-lock`, `install-driver`,
//! `create-restore-point`) join the allowlist as their features land,
//! and the helper gains the plan's JSON request channel when it is
//! hardened in v0.2 (plan §8). There is deliberately no generic
//! "run this command" operation, and the helper validates its arguments
//! against the allowlist before doing anything else.

use super::{Result, WinError};
use std::path::{Path, PathBuf};

/// The one allowlisted helper operation in v0.1.
pub const OP_EXPORT_DRIVERS: &str = "export-drivers";

/// Helper process exit codes. The parent maps any non-zero code to an
/// error; the codes exist so the helper's own console output and logs
/// can distinguish "you called me wrong" from "the operation failed".
pub const EXIT_OK: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;

/// A validated helper request. Parsing is the allowlist: anything that
/// does not parse into one of these variants does not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperRequest {
    ExportDrivers(PathBuf),
}

/// Parse the arguments after `--elevated` into a [`HelperRequest`].
/// Pure logic; unit-tested on any platform.
pub fn parse_helper_args(args: &[String]) -> std::result::Result<HelperRequest, String> {
    let Some(op) = args.first() else {
        return Err("missing operation".to_owned());
    };
    match op.as_str() {
        OP_EXPORT_DRIVERS => {
            let Some(dest) = args.get(1) else {
                return Err("export-drivers needs a destination folder".to_owned());
            };
            if args.len() > 2 {
                return Err("export-drivers takes exactly one folder".to_owned());
            }
            let dest = PathBuf::from(dest);
            if !dest.is_absolute() {
                return Err("export-drivers folder must be an absolute path".to_owned());
            }
            Ok(HelperRequest::ExportDrivers(dest))
        }
        other => Err(format!("unknown helper operation: {other}")),
    }
}

/// Entry point of the `--elevated` helper mode. `args` are the process
/// arguments after `--elevated`. Returns the process exit code.
pub fn helper_main(args: &[String]) -> u8 {
    let request = match parse_helper_args(args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("driverguard --elevated: {e}");
            eprintln!("usage: driverguard --elevated {OP_EXPORT_DRIVERS} <folder>");
            return EXIT_USAGE;
        }
    };
    match is_elevated() {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("driverguard --elevated: this helper must run elevated; refusing to run.");
            return EXIT_FAILED;
        }
        Err(e) => {
            eprintln!("driverguard --elevated: could not check elevation: {e}");
            return EXIT_FAILED;
        }
    }
    match request {
        HelperRequest::ExportDrivers(dest) => match crate::backup::export_drivers(&dest) {
            Ok(manifest) => {
                println!(
                    "Backed up {} driver packages to {}.",
                    manifest.packages.len(),
                    dest.display()
                );
                EXIT_OK
            }
            Err(e) => {
                eprintln!("backup failed: {e}");
                EXIT_FAILED
            }
        },
    }
}

/// Whether the current process token is elevated (an administrator
/// token, after any UAC split). Always `false` off Windows.
pub fn is_elevated() -> Result<bool> {
    #[cfg(target_os = "windows")]
    {
        platform::is_elevated()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(false)
    }
}

/// Run the `export-drivers` helper through the UAC prompt and wait for
/// it to finish. The helper writes the backup itself; the caller loads
/// the result back with `backup::verify_backup`, so the elevated
/// process never touches the history database (which lives in the
/// invoking user's profile).
pub fn run_export_helper(dest: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        platform::run_export_helper(dest)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = dest;
        Err(WinError::NotImplemented)
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{Path, Result, WinError, OP_EXPORT_DRIVERS};
    use std::os::windows::ffi::OsStrExt;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn api(e: windows::core::Error) -> WinError {
        WinError::Api {
            code: e.code().0 as u32,
            message: e.to_string(),
        }
    }

    pub fn is_elevated() -> Result<bool> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // Invariants: the pseudo-handle from GetCurrentProcess needs no
        // close; the token handle from OpenProcessToken is ours and is
        // closed exactly once on every path below.
        unsafe {
            let mut token = windows::Win32::Foundation::HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).map_err(api)?;
            let mut elevation = TOKEN_ELEVATION::default();
            let mut returned = 0u32;
            let result = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut TOKEN_ELEVATION as *mut core::ffi::c_void),
                core::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned,
            );
            let _ = CloseHandle(token);
            result.map_err(api)?;
            Ok(elevation.TokenIsElevated != 0)
        }
    }

    pub fn run_export_helper(dest: &Path) -> Result<()> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, WaitForSingleObject, INFINITE,
        };
        use windows::Win32::UI::Shell::{
            ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let exe = std::env::current_exe().map_err(|e| WinError::ToolUnavailable {
            tool: "driverguard".to_owned(),
            detail: format!("could not locate the running program: {e}"),
        })?;
        // Windows file names cannot contain a double-quote, so quoting
        // the destination is safe.
        let params = format!("--elevated {OP_EXPORT_DRIVERS} \"{}\"", dest.display());
        let verb = wide(std::ffi::OsStr::new("runas"));
        let file = wide(exe.as_os_str());
        let params_w = wide(std::ffi::OsStr::new(&params));

        let mut info = SHELLEXECUTEINFOW {
            cbSize: core::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(params_w.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // Invariants: the wide strings outlive the call; with
        // SEE_MASK_NOCLOSEPROCESS, info.hProcess is a live process
        // handle owned by us and closed exactly once below.
        unsafe {
            if let Err(e) = ShellExecuteExW(&mut info) {
                // HRESULT_FROM_WIN32(ERROR_CANCELLED): the user declined
                // the UAC prompt.
                if e.code().0 as u32 == 0x8007_04C7 {
                    return Err(WinError::ElevationDeclined);
                }
                return Err(api(e));
            }
            let process = info.hProcess;
            let _ = WaitForSingleObject(process, INFINITE);
            let mut code = 0u32;
            let result = GetExitCodeProcess(process, &mut code);
            let _ = CloseHandle(process);
            result.map_err(api)?;
            if code == 0 {
                Ok(())
            } else {
                Err(WinError::ToolFailed {
                    tool: "DriverGuard elevated helper".to_owned(),
                    exit_code: code as i32,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    /// A path that is absolute on the host running the tests.
    fn abs_path() -> &'static str {
        if cfg!(windows) {
            "C:\\Backups\\one"
        } else {
            "/backups/one"
        }
    }

    #[test]
    fn parses_export_drivers_with_absolute_path() {
        let parsed = parse_helper_args(&args(&["export-drivers", abs_path()]));
        assert_eq!(
            parsed,
            Ok(HelperRequest::ExportDrivers(PathBuf::from(abs_path())))
        );
    }

    #[test]
    fn rejects_missing_operation() {
        assert!(parse_helper_args(&args(&[])).is_err());
    }

    #[test]
    fn rejects_unknown_operation() {
        let err = parse_helper_args(&args(&["format-disk", "C:\\"])).unwrap_err();
        assert!(err.contains("unknown helper operation"), "{err}");
    }

    #[test]
    fn rejects_relative_destination() {
        assert!(parse_helper_args(&args(&["export-drivers", "backups\\one"])).is_err());
    }

    #[test]
    fn rejects_extra_arguments() {
        assert!(parse_helper_args(&args(&["export-drivers", abs_path(), "extra"])).is_err());
    }

    #[test]
    fn rejects_missing_destination() {
        assert!(parse_helper_args(&args(&["export-drivers"])).is_err());
    }

    #[test]
    fn not_elevated_on_non_windows() {
        #[cfg(not(target_os = "windows"))]
        assert!(!is_elevated().unwrap());
    }
}
