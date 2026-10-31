# DriverGuard

**Your drivers, your choice — enforced.**

DriverGuard is a small, open-source, offline Windows utility that gives you durable
control over your device drivers. Windows Update sometimes replaces a working,
user-chosen driver (GPU drivers are the classic case) with an older
Microsoft-supplied one — and the built-in recovery paths don't always help.
DriverGuard exists to make that situation boring.

> **Status: v0.3 in progress.** Inventory, backup, history (with the
> events audit log), policy-based driver locking, system-restore-point
> integration, the hardened elevated JSON helper, a CLI for
> lock/unlock/unlock-all/WU toggle/status, and a headless `--watch`
> mode with scheduled-task registration and driver-change notifications
> are all implemented and CI-tested on Windows. Remaining v0.3 work:
> GUI wiring for double-click lock/unlock confirmation, restore from
> History, WU/lock/watch toggles in Settings, and real toast
> notifications (currently a PowerShell balloon-tip baseline). Treat
> this as a pre-release — try it, and please report what you find.

## What it will do

| Verb | Release | Meaning |
| ---- | ------- | ------- |
| **See** | v0.1 | Inventory of every device and the driver it is actually running — provider, version, date — badged as vendor-supplied or Microsoft-supplied. |
| **Save** | v0.1 | One-click backup of all third-party driver packages to a plain local folder (usable without DriverGuard). |
| **Lock** | v0.2 | Per-device protection against driver replacement, using documented policy mechanisms — on Home and Pro. Fully reversible, with restore points. |
| **Watch** | v0.3 | A periodic check that detects driver changes and offers one-click restore from your backup. |

## Using it

**GUI** — run `driverguard.exe` with no arguments. The window lists every
device and the driver it is actually running, and the status line keeps a
count of devices using a Microsoft-supplied driver. **Back up all
drivers** saves every third-party driver package to
`%LOCALAPPDATA%\DriverGuard\backups\<timestamp>\` and verifies the result
before reporting success. Exporting drivers is one of the few things
Windows reserves for administrators, so Windows asks for approval at
that moment: the DriverGuard window itself stays unprivileged, and a
small elevated helper performs just the export and exits.

**CLI**

```
driverguard inventory              List devices and drivers as JSON (also records a history snapshot)
driverguard backup <folder>        Export all third-party drivers to a folder, with manifest, verified
driverguard verify-backup <folder> Check a backup folder against its manifest
driverguard history                Show recorded snapshots, backups, locks, and events
driverguard lock <instance-id>     Lock a device against driver replacement (UAC; restore point taken)
driverguard unlock <instance-id>   Unlock a device, restoring prior policy state
driverguard unlock-all             Remove every DriverGuard-owned entry and (optionally) reset the WU toggle
driverguard exclude-wu-drivers on  Toggle the global 'exclude drivers from Windows Quality Updates' lever
driverguard exclude-wu-drivers off
driverguard status                 Show current policy state and active locks
driverguard --watch                One headless driver-change check (used by the scheduled task)
driverguard register-watch         Install the DriverGuard Watch scheduled task (logon trigger)
driverguard unregister-watch       Remove the DriverGuard Watch scheduled task
```

The `backup` command asks for administrator approval the same way the
window does, unless the terminal is already elevated.

**Your data** — everything is local, under `%LOCALAPPDATA%\DriverGuard\`:
the history database (`db.sqlite`) and the `backups\` folders. A backup is
a plain folder of driver packages with a JSON manifest — usable without
DriverGuard. Nothing is ever sent anywhere.

## Design principles

- **Documented mechanisms only.** SetupAPI/CfgMgr32, `pnputil`, published policy keys. No undocumented hacks. **No kernel driver — ever.**
- **Offline-first, zero telemetry, zero accounts.** The app makes no network calls.
- **Reversible everything.** Every change captures prior state and can be undone from inside the app.
- **Honest guarantees.** Locks prevent replacement through the standard install pipeline; the watcher catches anything that gets through another way. The app never claims absolutes.
- **Safe Rust, narrowly scoped unsafe.** `unsafe` is confined to the FFI layer (`dg-win`), each block carrying its invariants. Long work runs off the UI thread.
- **Native, accessible UI.** Standard Win32 controls, system theme, High Contrast, text scaling, full keyboard operation, screen-reader labels.

## Building from source

Requirements: Windows 10 22H2 or Windows 11, and Rust 1.80 or newer.

```powershell
cargo build --workspace --release
```

Checks every PR must pass (also run by CI):

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Project docs

- [Architecture & UX Plan](docs/ARCHITECTURE_AND_UX_PLAN.md) — the design document. Read this before writing code.
- [Roadmap](ROADMAP.md)
- [Contributing](CONTRIBUTING.md)

## License

MIT License. See [LICENSE](LICENSE).
