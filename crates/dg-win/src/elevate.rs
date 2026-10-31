//! Elevated helper support (plan §2.1, v0.2 hardened channel).
//!
//! DriverGuard needs administrator approval for a small set of
//! operations: exporting drivers, applying lock policy, creating a
//! system restore point. Rather than running the whole GUI elevated,
//! the unelevated process relaunches *itself* through the Windows UAC
//! prompt (`ShellExecute` with the `runas` verb) as
//! `driverguard --elevated [--argv op args... | --request <path>]`.
//! The helper performs exactly one allowlisted operation, writes a
//! JSON response (for the --request path), and exits.
//!
//! v0.1 used an argv-only protocol for `export-drivers`. v0.2 adds
//! the JSON request/response channel (see [`crate::helper`]) for the
//! richer operations (`apply-lock`, `create-restore-point`); the
//! argv form is still accepted for backward compatibility.
//!
//! There is deliberately no generic "run this command" operation, and
//! every request is validated against an allowlist before anything
//! runs.

use super::{Result, WinError};
use std::path::{Path, PathBuf};

// ---- v0.1 argv-only operation (kept for backwards compat) -------------

pub const OP_EXPORT_DRIVERS: &str = "export-drivers";

pub const EXIT_OK: u8 = 0;
pub const EXIT_FAILED: u8 = 1;
pub const EXIT_USAGE: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyRequest {
    ExportDrivers(PathBuf),
}

pub fn parse_legacy_args(args: &[String]) -> std::result::Result<LegacyRequest, String> {
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
            Ok(LegacyRequest::ExportDrivers(dest))
        }
        other => Err(format!("unknown helper operation: {other}")),
    }
}

// ---- Helper mode entry point ------------------------------------------

/// Entry point for the `--elevated` helper. `args` are the process
/// arguments after `--elevated`. Returns the process exit code.
pub fn helper_main(args: &[String]) -> u8 {
    // Two forms:
    //   driverguard --elevated export-drivers <abs-folder>          (v0.1 argv)
    //   driverguard --elevated --request <path-to-json-request>     (v0.2 JSON)
    let (request, response_path): (crate::helper::HelperRequest, Option<&Path>) =
        if let Some(dash_dash) = args.iter().position(|a| a == "--request" || a == "-request") {
            let Some(path) = args.get(dash_dash + 1) else {
                eprintln!("driverguard --elevated --request: missing path");
                return EXIT_USAGE;
            };
            let path = Path::new(path);
            match crate::helper::read_request(path) {
                Ok(req) => (req, Some(path)),
                Err(e) => {
                    eprintln!("driverguard --elevated: invalid request at {}: {e}", path.display());
                    return EXIT_USAGE;
                }
            }
        } else {
            match parse_legacy_args(args) {
                Ok(LegacyRequest::ExportDrivers(dest)) => (
                    crate::helper::HelperRequest {
                        op: crate::helper::HelperOp::ExportDrivers { dest },
                    },
                    None,
                ),
                Err(e) => {
                    eprintln!("driverguard --elevated: {e}");
                    eprintln!("usage: driverguard --elevated [{OP_EXPORT_DRIVERS} <folder> | --request <file>]");
                    return EXIT_USAGE;
                }
            }
        };

    match is_elevated() {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("driverguard --elevated: this helper must run elevated; refusing.");
            try_write_response(
                response_path,
                &crate::helper::HelperResponse::err(
                    "helper launched without elevation",
                    None,
                ),
            );
            return EXIT_FAILED;
        }
        Err(e) => {
            eprintln!("driverguard --elevated: could not check elevation: {e}");
            return EXIT_FAILED;
        }
    }

    let resp = execute(&request.op);
    let ok = resp.is_ok();
    if let Some(path) = response_path {
        if let Err(e) = crate::helper::write_response(path, &resp) {
            eprintln!("driverguard --elevated: could not write response: {e}");
        }
    } else {
        // Legacy argv mode: print to stdout/stderr.
        match &resp {
            crate::helper::HelperResponse::Ok { message } => println!("{message}"),
            crate::helper::HelperResponse::Err { message, .. } => eprintln!("{message}"),
        }
    }
    if ok { EXIT_OK } else { EXIT_FAILED }
}

fn try_write_response(path: Option<&Path>, resp: &crate::helper::HelperResponse) {
    if let Some(p) = path {
        let _ = crate::helper::write_response(p, resp);
    }
}

fn execute(op: &crate::helper::HelperOp) -> crate::helper::HelperResponse {
    use crate::helper::{HelperOp, HelperResponse};
    match op {
        HelperOp::ExportDrivers { dest } => match crate::backup::export_drivers(dest) {
            Ok(manifest) => HelperResponse::ok(format!(
                "Backed up {} driver packages to {}.",
                manifest.packages.len(),
                dest.display()
            )),
            Err(e) => HelperResponse::err(e.to_string(), None),
        },
        HelperOp::ApplyLock { plan } => {
            // Attempt a restore point first if the plan asks for one.
            // A failed restore point does NOT abort the plan — the user
            // was given a chance to acknowledge in the Settings/UI flow;
            // but we still record the failure in the response so the
            // caller can note it in history.
            let mut rp_message: Option<String> = None;
            if plan.create_restore_point {
                match crate::system::create_restore_point(&plan.description) {
                    Ok(()) => { rp_message = Some("system restore point created".to_owned()); }
                    Err(e) => { rp_message = Some(format!("restore point not created: {e}")); }
                }
            }
            match crate::policy::apply_plan(plan) {
                Ok(()) => {
                    let mut msg = "policy applied".to_owned();
                    if let Some(m) = rp_message { msg.push_str("; "); msg.push_str(&m); }
                    HelperResponse::ok(msg)
                }
                Err(e) => HelperResponse::err(e.to_string(), None),
            }
        }
        HelperOp::CreateRestorePoint { description } => {
            match crate::system::create_restore_point(description) {
                Ok(()) => HelperResponse::ok("system restore point created".to_owned()),
                Err(e) => HelperResponse::err(e.to_string(), None),
            }
        }
    }
}

/// Whether the current process token is elevated.
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

/// Run an elevated helper for a generic HelperRequest, writing the
/// request to a temp file, launching `--elevated --request <path>`
/// through UAC, waiting for exit, and returning the response.
pub fn run_elevated(request: &crate::helper::HelperRequest) -> Result<crate::helper::HelperResponse> {
    #[cfg(target_os = "windows")]
    {
        platform::run_elevated(request)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = request;
        Err(WinError::NotImplemented)
    }
}

/// Back-compat wrapper: run the `export-drivers` helper (used by
/// `backup::export_drivers_elevated`). Delegates to run_elevated with
/// an ExportDrivers op.
pub fn run_export_helper(dest: &Path) -> Result<()> {
    let req = crate::helper::HelperRequest {
        op: crate::helper::HelperOp::ExportDrivers { dest: dest.to_path_buf() },
    };
    let resp = run_elevated(&req)?;
    match resp {
        crate::helper::HelperResponse::Ok { .. } => Ok(()),
        crate::helper::HelperResponse::Err { message, code } => {
            Err(WinError::Api { code: code.unwrap_or(1), message })
        }
    }
}

/// Apply a lock plan via the elevated helper. Returns the response
/// message on success; on failure the message is propagated as an
/// `Api` error so the caller can display it verbatim.
pub fn run_apply_lock(plan: &dg_core::LockPlan) -> Result<String> {
    let req = crate::helper::HelperRequest {
        op: crate::helper::HelperOp::ApplyLock { plan: plan.clone() },
    };
    let resp = run_elevated(&req)?;
    match resp {
        crate::helper::HelperResponse::Ok { message } => Ok(message),
        crate::helper::HelperResponse::Err { message, code } => {
            Err(WinError::Api { code: code.unwrap_or(1), message })
        }
    }
}

/// Create a system restore point via the elevated helper.
pub fn run_create_restore_point(description: &str) -> Result<String> {
    let req = crate::helper::HelperRequest {
        op: crate::helper::HelperOp::CreateRestorePoint { description: description.to_owned() },
    };
    let resp = run_elevated(&req)?;
    match resp {
        crate::helper::HelperResponse::Ok { message } => Ok(message),
        crate::helper::HelperResponse::Err { message, code } => {
            Err(WinError::Api { code: code.unwrap_or(1), message })
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{Result, WinError};
    use std::os::windows::ffi::OsStrExt;

    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn api(e: windows::core::Error) -> WinError {
        WinError::Api { code: e.code().0 as u32, message: e.to_string() }
    }

    pub fn is_elevated() -> Result<bool> {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
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

    pub fn run_elevated(request: &crate::helper::HelperRequest) -> Result<crate::helper::HelperResponse> {
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, WaitForSingleObject, INFINITE,
        };
        use windows::Win32::UI::Shell::{
            ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let path = crate::helper::new_request_path()?;
        crate::helper::write_request(&path, request)?;
        let _cleanup = CleanupPath(path.clone());

        let exe = std::env::current_exe().map_err(|e| WinError::ToolUnavailable {
            tool: "driverguard".to_owned(),
            detail: format!("could not locate the running program: {e}"),
        })?;
        let params = format!(
            "--elevated --request \"{}\"",
            path.display()
        );
        let verb = wide(std::ffi::OsStr::new("runas"));
        let file_w = wide(exe.as_os_str());
        let params_w = wide(std::ffi::OsStr::new(&params));

        let mut info = SHELLEXECUTEINFOW {
            cbSize: core::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file_w.as_ptr()),
            lpParameters: PCWSTR(params_w.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        unsafe {
            if let Err(e) = ShellExecuteExW(&mut info) {
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
            let resp = crate::helper::read_response(&path)?;
            if code != 0 && resp.is_ok() {
                return Err(WinError::ToolFailed {
                    tool: "DriverGuard elevated helper".to_owned(),
                    exit_code: code as i32,
                });
            }
            Ok(resp)
        }
    }

    struct CleanupPath(std::path::PathBuf);
    impl Drop for CleanupPath {
        fn drop(&mut self) { crate::helper::cleanup_request(&self.0); }
    }
}
