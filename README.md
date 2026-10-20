# Driver Guard

**Your drivers, your choice — enforced.**

Driver Guard is a small, open-source, offline Windows utility that gives you durable
control over your device drivers. Windows Update sometimes replaces a working,
user-chosen driver (GPU drivers are the classic case) with an older
Microsoft-supplied one — and the built-in recovery paths don't always help.
Driver Guard exists to make that situation boring.

> **Status: v0.1 skeleton.** This repository currently holds the workspace
> scaffolding, core data model, and documentation. It is not yet usable.
> Follow the [roadmap](ROADMAP.md) as it becomes real, release by release.

## What it will do

| Verb | Release | Meaning |
| ---- | ------- | ------- |
| **See** | v0.1 | Inventory of every device and the driver it is actually running — provider, version, date — badged as vendor-supplied or Microsoft-supplied. |
| **Save** | v0.1 | One-click backup of all third-party driver packages to a plain local folder (usable without Driver Guard). |
| **Lock** | v0.2 | Per-device protection against driver replacement, using documented policy mechanisms — on Home and Pro. Fully reversible, with restore points. |
| **Watch** | v0.3 | A periodic check that detects driver changes and offers one-click restore from your backup. |

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

Apache License, Version 2.0. See [LICENSE](LICENSE).
