//! DriverGuard user interface (native Win32).
//!
//! Skeleton: the UI lands after `dg-core` and `dg-win` inventory works end
//! to end. The layout, views, and copy rules are specified in
//! `docs/ARCHITECTURE_AND_UX_PLAN.md` §6 — read it before writing UI code.
//!
//! This crate forbids `unsafe`: all FFI goes through `dg-win`, and the UI
//! layer contains no business logic (view-models are fed by `dg-core`).

#![forbid(unsafe_code)]

/// The four top-level views, in navigation order (plan §6.2).
pub const VIEW_NAMES: [&str; 4] = ["Devices", "Backups", "History", "Settings"];
