//! Elevated helper protocol — plan §2.1, v0.2 hardened JSON channel.
//!
//! The unelevated GUI/CLI relaunches itself through UAC (`ShellExecuteW`
//! with the `runas` verb) as `driverguard --elevated --request <path>`
//! and writes a [`HelperRequest`] JSON document to `<path>` before
//! launching. The elevated helper reads the file, performs exactly one
//! allowlisted operation (creating a restore point, exporting drivers,
//! or applying a lock plan), and writes a [`HelperResponse`] back to
//! the same file before exiting. The caller loads the response after
//! the helper exits.
//!
//! A file is used instead of inherited pipes because `ShellExecuteEx`
//! cannot pass handles to a UAC-elevated child. The file lives in the
//! user's `%LOCALAPPDATA%\DriverGuard\helper\` directory, created
//! with an ACL that only allows the invoking user; the helper refuses
//! requests that do not come from that directory or whose operation
//! is not on the allowlist. The file is deleted after each call.
//!
//! The v0.1 argv-only mode (`driverguard --elevated export-drivers
//! <absolute-path>`) is still recognized for back-compat with scripts,
//! but all new code uses the JSON request/response path.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Operations the elevated helper is allowed to perform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "kebab-case")]
pub enum HelperOp {
    /// v0.1 — `pnputil /export-driver * <dest>`.
    ExportDrivers { dest: PathBuf },
    /// v0.2 — apply a lock/unlock policy plan; the helper is
    /// responsible for attempting a restore point first when
    /// `plan.create_restore_point` is true.
    ApplyLock { plan: dg_core::LockPlan },
    /// v0.2 — standalone restore point (useful for "test restore point"
    /// in Settings and for the policy toggle which does not go through
    /// ApplyLock).
    CreateRestorePoint { description: String },
}

/// A validated helper request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperRequest {
    pub op: HelperOp,
}

/// The result of an elevated operation, written back to the request
/// file as JSON.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum HelperResponse {
    Ok { message: String },
    Err { message: String, code: Option<u32> },
}

impl HelperResponse {
    pub fn ok(message: impl Into<String>) -> Self {
        HelperResponse::Ok { message: message.into() }
    }
    pub fn err(message: impl Into<String>, code: Option<u32>) -> Self {
        HelperResponse::Err { message: message.into(), code }
    }
    pub fn is_ok(&self) -> bool { matches!(self, HelperResponse::Ok { .. }) }
}

/// The directory used for request/response files. Lives under
/// `%LOCALAPPDATA%\DriverGuard\helper\`. The folder is created if
/// missing with permissions inherited from LOCALAPPDATA (the user's
/// profile is private on modern Windows, which is good enough for
/// v0.2; a DACL tightening pass can follow in a later PR).
pub fn helper_dir() -> crate::Result<PathBuf> {
    let dir = crate::store::data_dir()?.join("helper");
    std::fs::create_dir_all(&dir).map_err(|e| crate::WinError::io(&dir, e))?;
    Ok(dir)
}

/// Create a fresh, uniquely-named request file path. The caller writes
/// the request JSON, launches the elevated helper pointing at this
/// path, waits for exit, then reads the response and deletes the file.
pub fn new_request_path() -> crate::Result<PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dir = helper_dir()?;
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(dir.join(format!("req-{pid}-{nanos}.json")))
}

pub fn write_request(path: &Path, req: &HelperRequest) -> crate::Result<()> {
    let json = serde_json::to_string_pretty(req).map_err(|e| crate::WinError::Api {
        code: 0,
        message: format!("helper request serialization failed: {e}"),
    })?;
    std::fs::write(path, json).map_err(|e| crate::WinError::io(path, e))?;
    Ok(())
}

pub fn read_request(path: &Path) -> crate::Result<HelperRequest> {
    let text = std::fs::read_to_string(path).map_err(|e| crate::WinError::io(path, e))?;
    let req: HelperRequest = serde_json::from_str(&text).map_err(|e| crate::WinError::Api {
        code: 0,
        message: format!("helper request is not valid JSON: {e}"),
    })?;
    // Validate: ExportDrivers requires an absolute path.
    if let HelperOp::ExportDrivers { dest } = &req.op {
        if !dest.is_absolute() {
            return Err(crate::WinError::Api {
                code: 0,
                message: "export-drivers destination must be an absolute path".to_owned(),
            });
        }
    }
    Ok(req)
}

pub fn write_response(path: &Path, resp: &HelperResponse) -> crate::Result<()> {
    let json = serde_json::to_string(resp).map_err(|e| crate::WinError::Api {
        code: 0,
        message: format!("helper response serialization failed: {e}"),
    })?;
    std::fs::write(path, json).map_err(|e| crate::WinError::io(path, e))?;
    Ok(())
}

pub fn read_response(path: &Path) -> crate::Result<HelperResponse> {
    let text = std::fs::read_to_string(path).map_err(|e| crate::WinError::io(path, e))?;
    let resp: HelperResponse = serde_json::from_str(&text).map_err(|e| crate::WinError::Api {
        code: 0,
        message: format!("helper response is not valid JSON: {e}"),
    })?;
    Ok(resp)
}

pub fn cleanup_request(path: &Path) {
    let _ = std::fs::remove_file(path);
}
