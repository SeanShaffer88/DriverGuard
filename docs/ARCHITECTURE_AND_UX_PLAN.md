# DriverGuard — Architecture & UX Plan

- **Status:** v1, 2026-10-19 — approved; in implementation. v0.1 (See & Save) code-complete 2026-10-20; on-machine verification (§8) pending.
- **Name:** DriverGuard — confirmed final by the maintainer, 2026-10-20 (see §10)
- **Platform:** Windows 10 22H2 and Windows 11 · **Language:** Rust · **License:** MIT (switched from Apache-2.0 by maintainer decision, 2026-10-20)
- **Problem in one line:** Windows Update silently replaces working, user-chosen drivers with older Microsoft-supplied versions, and the user has no supported, reversible way to prevent it or undo it.

## 1. Product definition

DriverGuard is a single-purpose, open-source, offline Windows utility that gives the user durable control over their device drivers:

1. **See** — an inventory of every device and the driver it is actually running (provider, version, date), with Microsoft-supplied vs vendor-supplied clearly badged.
2. **Save** — one-click backup of all third-party driver packages to a plain local folder.
3. **Lock** — per-device protection that prevents Windows from installing a different driver for that device, using documented policy mechanisms, on Home and Pro alike.
4. **Watch** — a periodic check that detects any driver change and offers one-click restore from backup.

**Target users:** gamers and creators running vendor GPU drivers (the most common victims), anyone whose Wi-Fi/audio/printer driver was "upgraded" into brokenness, and the family-IT person who gets the call after Patch Tuesday.

**Core value proposition:** *Your drivers, your choice — enforced.* No registry editing, no retired Microsoft tools, no rituals (unplugging the network cable before rebooting is a real user workaround today; this product exists to delete it).

### 1.1 Design principles (binding)

1. **Documented mechanisms only.** SetupAPI/CfgMgr32, `pnputil`, published Group Policy registry keys. No undocumented hacks. **No kernel driver — ever.**
2. **Offline-first, zero telemetry, zero accounts.** The application makes no network calls.
3. **Reversible everything.** Every mutation captures prior state, is logged with before/after values, and can be undone from inside the app.
4. **Restore point before risk.** Any operation that changes driver-install policy first attempts to create a system restore point; if System Restore is disabled, the user must explicitly acknowledge before proceeding.
5. **Honest guarantees.** Locks *prevent* replacement through the standard install pipeline; the watcher *catches* anything that gets through by another path. UI copy never claims absolutes (see §6.1 for the normative sentence).
6. **Policy is the source of truth.** The app reads actual system state on every launch; its database is history and cache, never the authority. If an admin or another tool changed policy, the UI reflects reality.
7. **Native and accessible.** Standard Win32 controls, system theme and High Contrast, text scaling, full keyboard operation, screen-reader labels. No custom-drawn substitutes where a standard control exists.
8. **Safe Rust, narrow unsafe.** `unsafe` is confined to the FFI layer (`dg-win`), each block carrying a comment stating its invariants. Long work runs off the UI thread and is cancellable.

### 1.2 Scope

| Release | Name | Contents |
|---|---|---|
| v0.1 | See & Save | Inventory, one-click backup + manifest, History (local event log) |
| v0.2 | Lock | Per-device locks, unlock, unlock-everything, restore points, global WU-driver-exclusion toggle |
| v0.3 | Watchdog | Scheduled watcher, change detection + toast, one-click restore from backup |
| v1.0 | Polish | Accessibility audit, user guide + mechanism explainer docs, signed releases |

**Explicit non-goals:** downloading or installing *newer* drivers; updating drivers at all; blocking Windows Update as a whole; deleting packages from the Driver Store; supporting Windows older than 10 22H2; cloud lookups, accounts, or telemetry of any kind.

**Validation gate:** v0.1 ships with the community validation posts (research brief, §5 of the recommendation). Lock work (v0.2) proceeds only if v0.1 clears the agreed interest bar (200+ stars or 50+ substantive "I need this" responses within ~6 weeks); otherwise the project re-scopes before the expensive parts are built.

## 2. System architecture

### 2.1 Process model

A single signed executable, `driverguard.exe`, with three modes:

- **GUI mode (default)** — runs unelevated. Inventory, history, backup browsing, and settings are usable without admin rights.
- **`--watch` headless mode** — invoked by a Scheduled Task at logon (2-minute delay) and daily. Takes a snapshot, diffs against last-known-good, raises a toast only if a driver changed, records the event, exits. **No resident process, no Windows service.**
- **Elevated helper mode (`--elevated <op>`)** — the GUI spawns it via UAC (`ShellExecute` with `runas`) for privileged operations. The helper accepts a strict allowlist of operations — `export-drivers`, `apply-lock`, `remove-lock`, `install-driver`, `create-restore-point` — with typed parameters over a one-shot JSON request/response channel (inherited pipe, ACL'd to the invoking user). There is deliberately **no generic "run this command" operation.**

*Why no service:* every state change in this product is either user-initiated or periodic. A service would add attack surface, signing/AV scrutiny, and permanent maintenance for zero user benefit. Phasing: v0.1 needs only the `export-drivers` op; the broker hardens in v0.2 when policy writes land.

### 2.2 Crate layout (single Cargo workspace)

- **`dg-core`** — pure logic, no FFI: device/driver data model, four-part driver-version comparison, the diff engine, the lock-plan builder, backup manifest read/write. Fully unit-testable on any OS.
- **`dg-win`** — the only crate containing `unsafe`: RAII wrappers over CfgMgr32/SetupAPI, the `pnputil` process runner, policy registry read/write, restore-point API, toast notifications, Scheduled Task registration.
- **`dg-ui`** — Win32 GUI via `windows-rs`; view-models fed by `dg-core`. No business logic in the UI layer.
- **`driverguard`** (binary) — entry point and mode dispatch.

Dependency discipline (project standard: small, reputable, stdlib-first): `windows` (windows-rs), `rusqlite` with bundled SQLite, `serde`/`serde_json`, `thiserror`. Anything beyond this list requires a note in the PR explaining why the stdlib or an existing dependency cannot do the job.

### 2.3 Local data

Location: `%LOCALAPPDATA%\DriverGuard\` — `db.sqlite` (SQLite; shipped name — this plan originally said `state.db`, and since the file is a rebuildable cache the shipped name stands), `backups\<timestamp>\` folders, `logs\`.

Core tables:

- `devices` — instance ID, hardware IDs, class, friendly name.
- `driver_observations` — device, provider, version, date, INF name, source classification, observed-at timestamp. Append-only; this is the watcher's memory and the History view's data.
- `locks` — hardware ID, applied-at, prior policy state (exact values we found, so unlock restores them faithfully).
- `events` — type, device, before/after, timestamp, action taken. Doubles as the user-visible audit log.
- `backups` — ID, created-at, path, package count.

On every GUI start, live state (inventory + policy) is re-read and reconciled against the database; discrepancies are shown as system reality, with the database updated to match.

## 3. Core mechanisms

### 3.1 Inventory (v0.1)

- Enumerate present devices via CfgMgr32 (`CM_Get_Device_ID_ListW`, present-only filter). Per devnode, read device properties: friendly name / description, class, hardware IDs (`DEVPKEY_Device_HardwareIds`), instance ID.
- Read the *installed* driver per device from device properties: `DEVPKEY_Device_DriverProvider`, `DriverVersion`, `DriverDate`, `DriverInfPath`. This is read-only, needs no elevation, and reflects what is actually bound to the device. SetupAPI driver-info enumeration (`SetupDiBuildDriverInfoList` / `SetupDiEnumDriverInfo`) is the fallback for detail views.
- Source classification is by provider name: `Microsoft` → "Microsoft driver" badge; anything else → "Vendor driver" badge with the provider's name. No hidden scoring or reputation judgments.
- Refresh runs as a cancellable background job with progress; rows stream into the list as devices resolve.

### 3.2 Backup (v0.1)

- Implementation: elevated helper runs `pnputil /export-driver * <backup-folder>`, which exports every third-party package in the Driver Store.
- **Do not parse `pnputil`'s human output** — it is localized. Instead, verify results by folder diff (new `oem*.inf` package folders) and write the manifest from our own inventory data (INF → provider/version/devices mapping), recording the `pnputil` exit code. Partial failures are reported per package, never silently.
- Manifest (`manifest.json` in each backup folder): app version, timestamp, per-package `{inf, provider, version, devices[]}`, export result.
- **Backups are plain folders, usable without DriverGuard** (recoverable by hand with `pnputil /add-driver`). No proprietary container. This is a trust feature and will be stated in the README.

### 3.3 Lock (v0.2) — the core mechanism

Two documented levers, in order of preference:

**Lever 1 — per-device Device Installation Restrictions (primary).** Policy key:
`HKLM\SOFTWARE\Policies\Microsoft\Windows\DeviceInstall\Restrictions`

- `DenyDeviceIDs` (DWORD) = 1 enables the deny list.
- Subkey `DenyDeviceIDs` holds numbered string values, one hardware ID per entry.
- **`DenyDeviceIDsRetroactive` is never set by this product.** With retroactive application, restrictions reach devices that are *already installed* and can break the user's currently working device. With it unset (0), the installed driver keeps running, while *new* driver installations matching the hardware ID are denied — which is exactly the Windows-Update-replacement scenario. This rule is enforced in the lock-plan builder and covered by a unit test (§5).

Semantics to state plainly in docs and UI: Windows Update delivers drivers through the standard install pipeline, so a matching deny entry blocks the replacement. A side effect is that a *manual* "Update driver" in Device Manager for that device is also blocked while locked — that is the point of the feature, and the lock dialog says so. Unlocking restores manual control.

**Lever 2 — global Windows Update driver exclusion (secondary, optional).** `ExcludeWUDriversInQualityUpdate` (DWORD) = 1 under `HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate` stops Windows Update from offering driver updates at all. Coarse but complete. Exposed as a separate, clearly labeled switch in Settings, **off by default**, never bundled silently into a per-device lock.

**Lock-plan builder (`dg-core`).** Input: selected device + current policy state as read from the registry. Output: a minimal ordered operation list — create restore point → write policy values → record prior state. Prior-state fidelity rules:

- If the deny list already contains entries (an admin's, another tool's), we append ours and remember exactly which entries are ours. Unlock removes only ours.
- If `DenyDeviceIDs` did not exist before us, unlock deletes the values we created and restores the key to its prior shape.
- Multiple locked devices share the one policy list; `locks` rows map hardware ID → lock record.

**Unlock everything** removes all DriverGuard entries and restores prior policy values exactly. It is a first-class, tested feature (the panic button), not an afterthought.

### 3.4 Watch (v0.3)

- Scheduled Task `DriverGuard Watch`: triggers at logon (2-minute delay) and daily; runs `driverguard.exe --watch` with least privilege (reads need no elevation).
- A watch run snapshots current drivers and diffs against last-known-good observations. For each changed driver: an `events` record and one toast — title "A driver changed", body naming the device and old → new provider/version, click opens the app at History filtered to that event.
- **Silence is the default.** No change → no toast, no tray nag, exit code 0. A monitoring tool that cries wolf gets uninstalled; this one speaks only when it has evidence.

### 3.5 Restore (v0.3)

From a backup, per device, the restore plan (`dg-core`) is:

1. **If the device is locked by us, lift our deny entry for that hardware ID first** — otherwise our own lock would block the restore install. (This interaction is the easiest bug in the whole product to ship; it is called out here and covered by VM scenario (c).)
2. Create a restore point.
3. Elevated helper: `pnputil /add-driver <backup-folder>\*.inf /subdirs /install`.
4. Re-apply the lock entry if step 1 lifted it.
5. **Verify by re-reading the device.** Success is claimed only if the bound driver's version and INF match the backup. Otherwise the UI reports the actual state honestly — e.g., "Windows kept the newer driver; a restart may be required" — and offers a restart prompt. If a restart was requested, the watcher re-verifies at next logon and closes the loop in History.

## 4. Safety model

- **Harm model:** the worst outcome is a user whose working driver becomes unusable, or who cannot install a driver they need. Every design rule in §1.1 and §3.3 exists to make that outcome structurally difficult: retroactive restrictions never set, locks always reversible in-app, restore points before policy changes, unlock-everything as a tested path, Driver Store packages never deleted, driver-signing and Secure Boot settings never touched.
- **Transactional shape for privileged ops:** capture prior state → apply → verify by re-reading → on any failure, roll back to prior state and show the error with technical details available.
- **Auditability:** the `events` table is the user-visible History — everything DriverGuard did, and everything Windows did to drivers, timestamped with before/after values. If the tool ever misbehaves, its own log is the first place a user (or a bug report) can look.

## 5. Testing & verification

- **Unit tests (`dg-core`):** four-part version comparison (with driver-date tie-breaking); diff engine (added/removed/changed drivers, provider changes at equal version); lock-plan builder — including the invariant that no plan ever emits `DenyDeviceIDsRetroactive`, prior-state preservation when foreign deny entries exist, and unlock plans that restore the exact prior key shape; manifest round-trip.
- **FFI smoke tests (`dg-win`):** read-only calls run in CI on `windows-latest` — inventory returns at least one device; policy read on a clean runner returns "no restrictions".
- **VM integration matrix (manual, per release, checklist in the repo):** Windows 10 22H2 Home/Pro; Windows 11 23H2, 24H2, 25H2 Home/Pro. Scripted scenarios:
  - (a) Locked device: attempt to install an older driver package via `pnputil` → must be denied.
  - (b) Unlocked device: same install succeeds → next watcher run flags it and raises exactly one toast.
  - (c) Restore from backup with the lock active → lift–install–relock sequence verifies end state (backup driver bound, lock present).
  - (d) Unlock-everything → policy key is byte-identical to its pre-lock state.
  - (e) Backup on a clean machine → exports the expected package count; manifest matches inventory.
- **CI (GitHub Actions):** `cargo fmt --check`, `clippy -D warnings`, `cargo test`, release build. Release workflow produces signed binaries + SHA-256 checksums.
- **Honesty rule (project standard):** release notes list exactly which matrix cells were run for that release. Unrun cells are stated as unrun, never implied. No build is described as production-ready without the matrix and review behind it.

## 6. UX plan

### 6.1 Principles and copy rules

- **Plain English first.** Hardware IDs, registry paths, and INF names live behind a "Technical details" expander in dialogs and History entries — never in primary copy.
- **Normative honesty sentence** (all lock-related copy derives from this): *"Locking prevents Windows from installing a different driver for this device in most cases. If a driver changes anyway, DriverGuard will tell you and offer to restore yours."*
- **One primary action per view.** Privileged or policy-changing actions confirm with a plain-language consequence statement, and every completed action offers Undo.
- **No fear copy.** The app never uses "threat", "risk detected", or counts of scary things. It reports facts: what changed, when, and what the user can do about it.

### 6.2 Layout and views

Single window, left navigation — **Devices · Backups · History · Settings** — content area per view. Minimum size ≈ 900×560; layout survives 200% text scaling without truncation.

- **Devices (home view).** Grouped list: Graphics / Network / Audio / Other devices. Row: device name; current driver as "AMD · 31.0.15.3621 · Jun 2026"; badge **[Vendor driver]** or **[Microsoft driver]**; lock state icon + text; "Changed Oct 12" marker when applicable. Toolbar: Refresh, **Back up all drivers**, search filter. Row actions: Lock/Unlock…, Back up this driver, Details….
- **First run.** Inventory runs automatically. Summary header: *"36 devices · 5 using Microsoft-supplied drivers · 2 changed in the last 30 days"* with the primary call to action **Back up my drivers**. This screen is the v0.1 value moment — it must be useful before any lock exists.
- **Lock dialog.** Device name; what will happen (max three bullets, including the Device Manager side effect and the honesty sentence); **[Create restore point & lock]** / [Cancel]. Progress inline; success state shows **[Undo]**.
- **Backups.** List of backup folders with date, package count, and size; **Back up now**; per-backup **Restore a driver…** (v0.3) and Open folder.
- **History.** Chronological event list — driver changed / lock applied / lock removed / backup created / restore completed — each expandable to technical details. Driver-changed events carry **[Restore my driver]** and **[Keep the new driver]**.
- **Settings.** Watcher on/off + **Run a check now**; global Windows Update driver-exclusion toggle (Lever 2) with its own warning copy; backup location; **Unlock everything** in a clearly marked section with confirmation; About/version.

### 6.3 Watch toast (normative copy)

Title: **A driver changed** — Body: *"Graphics — AMD Radeon RX 6600: driver changed from AMD 31.0.15.3621 to Microsoft 30.0.14.9."* Click opens History at that event. No sound, no repeated reminders for the same event.

### 6.4 Accessibility & platform fit

- Every action reachable by keyboard with visible focus and logical tab order; the device list supports arrow-key navigation and type-ahead.
- UI Automation names on all controls; long-running operations announce start/completion (scan finished, backup saved) through accessibility events.
- Honors system theme, dark mode, and High Contrast; icons always paired with text labels; color is never the only carrier of meaning (badges and lock state are text, not just color).

## 7. Risks & mitigations

1. **Drivers bundled inside cumulative updates may reach a device by a path the deny list doesn't cover.** → The watcher is the safety net; copy never promises absolutes (§6.1); restore is one click.
2. **Our own lock blocks our own restore.** → Lift–install–relock sequence (§3.5); VM scenario (c) is a release gate.
3. **The retroactive footgun.** → Never set, enforced in the plan builder, unit-tested, on the code-review checklist.
4. **AV / SmartScreen friction** (a driver-adjacent tool from an unknown publisher). → Authenticode signing from v0.1, no packing or obfuscation, VirusTotal links published with each release.
5. **Policy behavior drifts across Windows builds.** → VM matrix per release (§5); stated support window (10 22H2+ / 11); the app reads live policy state rather than assuming it (§1.1, principle 6).

## 8. Milestones & exit criteria

- **v0.1 See & Save (weeks 1–3):** inventory, backup + manifest, History. Exit: scenario (e) passes on Win11 24H2 Home + Pro; validation posts published with the release.
- **v0.2 Lock (weeks 4–6):** lock-plan builder, hardened elevated helper, lock/unlock/unlock-everything, restore points, global toggle. Exit: scenarios (a) and (d) pass the full matrix. *Starts only if the §1.2 validation gate clears.*
- **v0.3 Watchdog (weeks 7–9):** Scheduled Task watcher, diff + toast, restore flow. Exit: scenarios (b) and (c) pass; two weeks of dogfooding on maintainer machines with zero false-positive toasts.
- **v1.0 (weeks 10–12):** accessibility audit against §6.4, user guide + mechanism explainer, signed releases. Exit: full matrix green; release notes pass the §5 honesty rule.

## 9. Distribution

- GitHub Releases: portable signed `.zip` (single exe, no installer required) + SHA-256 checksums + VirusTotal links. An installer is a post-1.0 consideration only if users ask for one.
- All releases Authenticode-signed from v0.1 (certificate cost is the project's one recurring expense; covered by GitHub Sponsors per the sustainability plan).

## 10. Open decisions (need Sean)

1. **Name.** "DriverGuard" is a working title. Before the repo is created: check GitHub name availability and obvious trademark collisions. Fallbacks: *DriverKeeper*, *DriverFence*. — **Decided 2026-10-20: DriverGuard is the final name** (GitHub repo created under it; no formal trademark clearance was performed).
2. **License.** Apache-2.0 as recommended in the research brief — confirm. — **Decided otherwise 2026-10-20: MIT**, matching the maintainer's other projects.
3. **Global WU-exclusion toggle timing.** This plan places Lever 2 in v0.2, off by default. Alternative: defer to v1.0 to keep v0.2 purely per-device.
4. **Watch frequency.** Logon + daily (assumed). Alternative: logon only, for an even quieter tool.
