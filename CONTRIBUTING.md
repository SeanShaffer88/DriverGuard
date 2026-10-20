# Contributing to Driver Guard

Thanks for your interest. Driver Guard is a small project with a deliberate
working style — please read this before opening a pull request.

## Before you write code

- Read the [Architecture & UX Plan](docs/ARCHITECTURE_AND_UX_PLAN.md). It is
  the design contract for this project; PRs that contradict it need a
  discussion first.
- For anything beyond a small fix, open an issue describing the problem
  before writing the implementation.

## Pull requests

- **Small and single-purpose.** One concern per PR. Small PRs get reviewed;
  large ones get delayed.
- **CI must be green.** Every PR runs, and must pass:
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo test --workspace`
  - `cargo build --workspace --release`
- **New dependencies need justification** in the PR description: what the
  standard library or an existing dependency cannot do. This project keeps
  its dependency set small on purpose.
- **Logic in `dg-core` needs unit tests.** If it can be tested without
  Windows, it must be.
- **`unsafe` only in `dg-win`**, and every `unsafe` block carries a comment
  stating the invariants it relies on.
- **Test on real Windows where it matters.** Changes touching policy,
  `pnputil`, restore points, or the watcher must say which Windows editions
  and builds they were exercised on (Home/Pro, build number). "It compiles"
  is not a test report.

## Hard rules

- **No network calls, no telemetry, no accounts.** PRs adding any of these
  will be declined, however useful they seem. This is the product's core
  promise.
- **No undocumented mechanisms** and **no kernel driver**, ever.
- **Policy changes must be reversible** and must capture prior state. If a
  change cannot be undone from inside the app, it does not ship.
- **Never set `DenyDeviceIDsRetroactive`.** See the plan, §3.3.
- **Honest copy.** User-facing text states what the software actually does,
  including its limits. No absolute claims about blocking, no fear language.

## Reporting bugs

Please include: Windows edition and build, what you did, what you expected,
what happened, and the relevant entry from the app's History view (it logs
every action with before/after values). Redact anything you consider
personal before posting — device names are usually safe to share, but the
choice is yours.
