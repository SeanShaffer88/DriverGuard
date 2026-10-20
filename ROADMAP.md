# Roadmap

DriverGuard ships in small, CI-gated steps. Exit criteria come from the
[Architecture & UX Plan](docs/ARCHITECTURE_AND_UX_PLAN.md) §8; a release is
done when its criteria pass, not when its code merely compiles.

## v0.1 — See & Save

- Device/driver inventory (provider, version, date; vendor vs Microsoft badges)
- One-click backup of all third-party driver packages + JSON manifest
- History: local log of everything the app does
- Exit: backup scenario passes on Windows 11 Home and Pro

*Status: implementation complete (inventory, backup, history, and the
Drivers window; all CI-gated). The exit scenario still has to run on real
Windows 11 Home and Pro machines before the v0.1 tag.*

## v0.2 — Lock

- Per-device locks via Device Installation Restrictions (documented policy)
- Unlock, and unlock-everything that restores the exact prior policy state
- System restore point before policy changes
- Optional global "exclude drivers from Windows Update" toggle (off by default)
- Exit: lock and unlock scenarios pass the full VM test matrix

*Lock work starts only after v0.1's community validation clears the bar set
in the plan (§1.2).*

## v0.3 — Watchdog

- Scheduled watcher (logon + daily): detects driver changes, notifies once,
  stays silent otherwise
- One-click restore of a backed-up driver — including temporarily lifting
  the device's own lock so the restore is not blocked by it
- Exit: change-detection and restore scenarios pass; two weeks of dogfooding
  with zero false-positive notifications

## v1.0 — Polish

- Accessibility audit (keyboard, screen reader, High Contrast, text scaling)
- User guide and a plain-language explainer of how locking works
- Signed releases

## Explicit non-goals

Downloading or updating drivers; blocking Windows Update as a whole;
deleting Driver Store packages; Windows older than 10 22H2; cloud features,
accounts, or telemetry of any kind.
