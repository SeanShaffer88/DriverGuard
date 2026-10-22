# Safe Windows validation — October 21, 2026

**Partially verified; v0.1 acceptance remains open.** Environment: Windows 11 Home build 26300, x64, standard user. Tested implementation: `d864f45d3fb5778ec3c1abaaa9fbd7c549ad884d`.

## Results

| Check | Result and scope |
| --- | --- |
| Formatting, Clippy with warnings denied, workspace tests, Release workspace build | Passed; 31 tests, including the real present-device inventory test |
| Release CLI `inventory` | Exit 0; 207 present devices returned; one local history snapshot recorded |
| Release GUI startup | Devices window loaded the real inventory, six visible columns and the device-count status line; no backup or policy controls activated |
| Release CLI `verify-backup` | Exit 0 for one synthetic INF package and matching manifest |
| Release CLI `backup` into that non-empty synthetic folder | Exit 1 with an actionable destination-not-empty error; existing fixture retained; refused before invoking driver export |

The synthetic INF contained inert text. It was never installed. Raw inventory includes hardware identifiers and stays local; it is not attached to public evidence.

## What verification means

Current backup verification compares package identities in the manifest with INF-containing folders. It does **not** hash every driver payload, validate signatures, prove that a driver can reinstall, or certify recovery. An accepted synthetic manifest is a format/consistency check, not a real driver-backup acceptance result.

## Remaining acceptance gates

- Elevated real third-party package export and verification on both Windows 11 Home and Pro; record exit status, package count, manifest and failure/cleanup behavior. This standard-user session did not attempt elevation.
- Restore/install and watcher behavior: planned functionality is not established by this session.
- Policy lock/unlock and rollback: no machine policy or Driver Store changes were performed.
- Clean-host install/uninstall, Windows 10, ARM64, screen reader, keyboard-only and high-contrast/text-scale matrix remain unverified.

Safe-session scope was explicitly selected by the maintainer. No devices were disabled; no drivers, policy, signing or Secure Boot settings were changed. Read these results alongside the architecture plan's acceptance checklist; do not mark Home/Pro backup acceptance complete.
