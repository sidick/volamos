# volamos — Setup & Phase 1 Plan

This document records the repo-preparation plan and Phase 1 task breakdown
agreed for volamos, a Rust-native successor to `vamos` (see the original
proposal for full background, architecture, non-goals, and later-phase
scope).

## Decisions made

- **License**: `MIT OR Apache-2.0` dual license (standard Rust convention).
  `LICENSE-MIT` + `LICENSE-APACHE` at repo root, `license = "MIT OR
  Apache-2.0"` in Cargo.toml.
- **CI**: GitHub Actions, hosted runners (`ubuntu-latest`, `macos-latest`).
  No self-hosted infra.
- **Phase 1 test fixture**: hand-authored 68k assembly program, assembled
  with vasm where available. Both the `.s` source and the built binary are
  committed under `fixtures/`, with a README explaining how to regenerate
  it. Not a CI-time build dependency.
- **Rust edition**: current stable edition (2021, or 2024 if toolchain
  support is confirmed unproblematic) — not a hard blocker either way.

## A. Repo setup

1. Cargo workspace with two crates:
   - `crates/volamos-core` (lib) — CPU wrapper, memory/address space, hunk
     loader, trap dispatch, library-stub registry.
   - `crates/volamos` (bin) — CLI entry point, depends on core.
2. Dependencies: the `m68k` crate (pinned, behind a thin internal `Cpu`
   trait so an `r68k` fallback is a swap, not a rewrite), `anyhow`/
   `thiserror`, `clap`, `log`/`env_logger` or `tracing`. Nothing else in
   Phase 1.
3. GitHub Actions workflow: matrix `{ubuntu-latest, macos-latest}` × stable
   Rust; `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`.
4. `.gitignore`, `LICENSE-MIT`, `LICENSE-APACHE`, expanded README.
5. `fixtures/` directory with README on how test binaries are produced.

## B. Phase 1 implementation — task breakdown

Dependency order: T1 → (T2, T3 in parallel) → T4, T5 → T6.

- **T1 — Workspace + skeleton + CI.** Includes the `Cpu` trait and a stub
  `AddressSpace` type so T2/T3 can start against stable interfaces.
  Sequential, do first.
- **T2 — CPU integration + memory model** (`volamos-core`): integrate the
  `m68k` crate, implement the memory bus/`AddressSpace` (flat RAM segment +
  reserved low region for fake library jump tables), run a hand-fed
  instruction sequence in a unit test, expose stepping. Explicit
  deliverable: a go/no-go on the `m68k` crate (API shape, trap/
  illegal-instruction hook support, license, maintenance) — fall back to
  `r68k` here if needed. Biggest risk in the project; front-loaded.
- **T3 — Hunk loader + test fixture**: minimal AmigaOS hunk-format loader
  (HUNK_HEADER/CODE/DATA/BSS/RELOC32/END, single-hunk binaries), plus the
  fixture binary itself — a few instructions calling one library function
  (JSR through A6 + negative LVO offset into the fake jump table), then
  exit. Runs parallel to T2; only needs T1's `AddressSpace` interface.
- **T4 — Trap-and-dispatch mechanism** (after T2): the core proof.
  Populate the fake library jump table so each LVO entry triggers a trap;
  on trap, decode PC → (library, offset), call a registered Rust handler,
  write result to D0, RTS back. Exactly one faked call for Phase 1, with
  observable output (e.g. stdout), plus a clean exit path.
- **T5 — CLI entry point** (after T2/T3): `volamos <binary> [args]` — load
  hunk file, set up stack/registers, run loop, propagate 68k exit code as
  process exit code, `-v` logging of trapped calls.
- **T6 — End-to-end smoke test** (after T4/T5): integration test running
  the fixture through the full stack, asserting the faked handler fired
  with expected args, output/exit code are correct, and termination is
  clean. Wired into CI — this is Phase 1's "done" criterion.

## Execution

Repo scaffolding (T1) and Phase 1 implementation (T2–T6) are being carried
out by Sonnet 5 coding-agent workers, orchestrated by a fable-model
coordinator that sequences dependencies, reviews each worker's diff, and
reports final status. No commits are made automatically — changes are left
for review before committing.

## Deferred to later phases (not in scope here)

- dos.library file I/O + volumes/assigns (Phase 2).
- exec.library essentials + utility.library (Phase 3).
- Three-oracle parity harness against vamos and Copperline/real Kickstart
  (Phase 4).
- JIT enablement via the `m68k` crate's Cranelift path (Phase 5).
- Static musl Linux container image (Phase 6).
- fd/SFD metadata provenance/licensing decision (needed starting Phase 2,
  not required for Phase 1's single hand-registered LVO).
