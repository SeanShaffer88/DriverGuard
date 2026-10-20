# AGENTS.md — working agreements for DriverGuard

This file records how work is done in this repository. It applies to human
and automated contributors alike.

## The contract

- The design contract is `docs/ARCHITECTURE_AND_UX_PLAN.md`. Read it before
  substantial work. If implementation reveals the plan is wrong, change the
  plan in the same PR and say why.
- Small, reviewable steps. Each PR does one thing and leaves CI green:
  `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`, `cargo build --workspace --release`.

## Engineering standards

- Safe Rust; `unsafe` only in `crates/dg-win`, narrowly scoped, each block
  commented with its invariants. `dg-core` and `dg-ui` carry
  `#![forbid(unsafe_code)]`.
- Small, reputable dependency set; stdlib first. A new dependency requires
  a written justification in the PR.
- Pure logic goes in `dg-core` with unit tests. Windows access goes in
  `dg-win`. The UI (`dg-ui`) holds no business logic.
- Long-running work runs off the UI thread and is cancellable.
- Local data only, offline-first. No networking, no telemetry, no accounts
  — not behind a flag, not "for diagnostics", not ever.

## Safety rules (product-specific, non-negotiable)

- Never set `DenyDeviceIDsRetroactive` (plan §3.3). The type system omits
  it on purpose; do not add it back.
- Every policy or driver mutation captures prior state first, verifies by
  re-reading afterwards, and rolls back on failure.
- A system restore point is attempted before any policy change.
- Backups are plain folders with a JSON manifest; never introduce a
  proprietary backup format.
- Never delete packages from the Driver Store. Never touch driver-signing
  or Secure Boot settings.

## Documentation and reporting

- Documentation names no third-party products. Refer to capabilities and
  Windows platform components, not other software.
- Report exactly what ran: which checks, on which Windows editions/builds,
  with what result. Never claim a scenario passed that did not run, and
  never describe a build as production-ready without review and the VM
  test matrix (plan §5) behind it.
- User-facing copy follows the plan's honesty rules (§6.1): plain language,
  no absolutes, no fear words.

---

## Development standard

This project follows SEAN's **Universal Windows Desktop Master Prompt** — the standing development standard for all of SEAN's Windows desktop projects, current and future. The full text is maintained by SEAN (ask him for the current version); this section is the in-repo summary, and the project's own instructions add project-specific requirements on top of it.

Core standards:

- Production quality only: no rough demos or placeholder features presented as finished; safe defaults for beginners, explained advanced controls for professionals.
- Accessibility is a requirement: keyboard navigation, accessible names, visible focus, sufficient contrast, no meaning by color alone, specific error messages with a fix path.
- Every consequential action explains its risks, limitations, and recovery options; destructive or irreversible actions are confirmed, previewed where feasible, and backed up or undoable where feasible.
- Data safety: local-first storage, offline-capable core features, versioned data formats, atomic writes, tested migrations, and no silent data loss.
- Verification honesty: compatibility and readiness claims are labeled Verified, Partially verified, or Unverified; never claim a check that did not run.

Standing defaults (a project brief may override a specific default):

- Product-family look: the black/red aesthetic matching InstallScope is preferred, not mandatory.
- Rust is the default for new projects and for new components where it fits; existing stacks are preserved — this is not a rewrite mandate.
- Support as many Windows versions as makes sense for the project; consider Windows 10 support when feasible without materially extending development time. Never claim support that has not been verified.
- Status and task reports include a Progress % and an ETA whenever they can be estimated responsibly.
- Public documentation avoids competitor comparisons by default (internal research may name competitors; license attributions for bundled components are kept).
- License: MIT for SEAN-owned projects; forks retain their upstream license.
