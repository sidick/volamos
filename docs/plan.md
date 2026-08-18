# volamos — Project Plan (Phase 1 complete; Phase 2 breakdown)

This document records the repo-preparation plan and Phase 1 task breakdown
agreed for volamos, a Rust-native successor to `vamos` (see the original
proposal for full background, architecture, non-goals, and later-phase
scope), plus — now that Phase 1 is implemented — verification notes on
what was actually built, the Phase 2 task breakdown, the fd/SFD metadata
decision, and phase-level summaries for Phases 3-6. Sections A/B below
are the original plan, kept for the record; "Phase 1 as built" corrects
the details that ended up differing.

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

## Phase 1 as built — verification notes (2026-08-18)

Phase 1 is complete, committed, and pushed. The sections above are kept
as the historical plan; where the implementation diverged, the code is
authoritative. Verified against the actual repo state:

- **Rust edition**: **2024** (workspace `edition = "2024"`, `resolver =
  "3"`), not 2021. The 2024 toolchain caused no problems.
- **Dependencies**: the *only* external dependency is `m68k = "=0.10.14"`
  (in `volamos-core`). The planned `anyhow`/`thiserror`, `clap`, and
  `log`/`env_logger`/`tracing` were **not** brought in: errors are
  hand-rolled enums (`LoadError`, `DispatchError`, `RuntimeError`) with
  `Display`/`Error` impls, and the CLI's argument parsing is hand-rolled
  in `crates/volamos/src/main.rs`. Keep this bias in Phase 2 — add a
  dependency only when a task genuinely needs it.
- **m68k crate pin**: `=0.10.14` is still the latest release on
  crates.io as of 2026-08-18, so the pre-1.0-churn risk has not
  materialised; the exact pin stands.
- **Module layout** (`crates/volamos-core/src/`): `cpu.rs` (the `Cpu`
  trait, `DataRegister`/`AddressRegister`, `StopReason`/`TrapInfo`/
  `TrapKind`), `backend.rs` (`M68kCpu` wrapping the `m68k` crate's
  `CpuCore`; also home of `TRAP_TABLE_BASE`/`TRAP_TABLE_SIZE`/
  `TRAP_TABLE_END`), `memory.rs` (`AddressSpace` trait + `FlatMemory`),
  `loader.rs` (`parse`/`load`, `HunkFile`/`LoadResult`), `dispatch.rs`
  (`LibraryTable`, `LibraryHandler`, `HandlerContext`, `Runtime`).
- **Trap mechanism actually used**: A-line opcodes. `LibraryTable::
  register` writes `0xA000 | slot` at `base + lvo`; the `m68k` crate
  surfaces A-line words as a `StepResult::AlineTrap` *without* taking
  the hardware exception, `Runtime::run` decodes the slot, calls the
  Rust handler, then performs the RTS itself (pop return address off
  A7, `set_pc`, which also invalidates the backend's prefetch queue).
  The reserved region is `[0x0000, 0x1000)`, prefilled with an
  "unknown call" sentinel opcode; the fake `DOS_LIBRARY_BASE` is
  `0x0800`, `LVO_PUTSTR = -948`. Clean exit is an address-based
  sentinel: `EXIT_STUB_ADDR` (last word of the reserved region) is
  pre-pushed as the outermost return address; a trap at that PC ends
  the run with D0 as the exit code.
- **Phase 1 shortcuts Phase 2 must replace or formalise**: `Runtime::
  new` pre-seeds `A6 = DOS_LIBRARY_BASE` (no `OpenLibrary` exists);
  guest memory is a fixed 1 MiB `FlatMemory` with the stack at the top
  and the program loaded at `TRAP_TABLE_END`; the CLI accepts guest
  `[args...]` but ignores them.
- **Fixture**: `fixtures/hello` is a *two*-hunk executable (CODE +
  DATA, one RELOC32) built from `hello.s` (vasm mot syntax), with
  `fixtures/gen_hello.py` as a toolchain-free byte-identical
  generator. The loader also skips `HUNK_SYMBOL`/`HUNK_DEBUG` — a bit
  broader than the plan's minimum.

None of these divergences is a defect; the plan text above is simply
superseded on those points.

## fd/SFD metadata decision (due start of Phase 2 — decided)

Every call implemented beyond Phase 1's hand-registered `PutStr` needs
per-function metadata: name, LVO offset, and argument-register
assignments. Three candidate sources were on the table; the repo is
`MIT OR Apache-2.0`, which constrains the choice:

- **NDK `.fd` files** — carry Amiga NDK licensing (proprietary,
  Cloanto/Hyperion lineage); not redistributable under MIT/Apache.
  **Rejected.**
- **amitools/vamos tables** — amitools is **GPLv2** (verified against
  the upstream repo on 2026-08-18; the proposal's "BSD-ish" assumption
  was wrong), and its bundled `.fd` files are the Commodore-supplied
  NDK ones anyway. Deriving our tables from them would GPL-encumber an
  MIT/Apache codebase and inherit the NDK provenance. **Rejected.**
- **AROS SFD files** — the independently maintained clean-room
  descriptions, under the AROS Public License (MPL-1.1-derived,
  file-level). **Chosen as the reference source**, with one important
  qualifier: we do **not** vendor the SFD files into this repo. A
  one-shot codegen tool (T7 below) reads them and emits our own Rust
  tables containing *only ABI facts* — function name, LVO offset,
  register list — with a provenance comment. Names, offsets, and
  register assignments are uncopyrightable interface facts (the same
  position every reimplementation project, AROS included, rests on);
  no descriptive text, comments, or file structure is copied. The
  generated `.rs` files are checked in (codegen is not a build-time
  dependency), so contributors and CI never need the SFDs.

Recorded rationale: it is the only one of the three sources that is
both licence-compatible in spirit and clean of NDK provenance, and the
facts-only extraction keeps the MIT/Apache grant honest. If the human
disagrees with the "ABI facts" position, the fallback is hand-typing
tables from documentation as calls are needed — slower but equally
clean; see CLARIFYING QUESTIONS.

## C. Phase 2 — dos.library file I/O + volumes/assigns: task breakdown

Scope: dos.library file I/O (`Open`/`Read`/`Write`/`Seek`/`Close`,
`Lock`/`UnLock`/`DupLock`, `Examine`/`ExNext`, `Input`/`Output`,
`IoErr`/`SetIoErr`, `CurrentDir`/`ParentDir`), the two-tier
volume/assign model (`-V` host-dir mappings, `-a` assigns, multi-assign
search order, auto-assign fallback), Amiga path semantics (`:`/`/`,
case-insensitive lookup over a case-sensitive host FS), BPTR/BSTR
handling, guest argument passing, and the minimal `exec.library`
`OpenLibrary`/`CloseLibrary` stub needed so real programs can obtain
the dos base instead of Phase 1's pre-seeded `A6`.

vamos escape hatches to port, not re-solve: (a) vamos auto-creates a
**fake library** for any `OpenLibrary` of a library it doesn't
implement, whose vectors all trap with a clear "unimplemented"
diagnostic — our `UNKNOWN_SLOT` prefill is the same idea at the vector
level; T12 extends it to whole libraries. (b) vamos can also load a
**real Amiga library binary** via its segment loader and run it on the
CPU (it uses this for the math libraries). That passthrough needs
`LoadSeg` + exec library-node plumbing and is explicitly deferred to
Phase 3 — but T12's fake-lib registry must be shaped so a
real-library-backed base can slot in later. Math libraries
(mathffp/mathieee) are deferred with that note; before Phase 3, check
whether the `m68k` crate emulates 68881/68882 FPU instructions at all
(`M68kCpu::new` currently selects `CpuType::M68000`, no FPU —
mathffp/mathieeesingbas are software-implemented and don't need one,
so real-library passthrough may suffice even without FPU coverage).

Dependency order: **T7, T8, T9 in parallel** (disjoint files, no shared
interfaces beyond what Phase 1 already exports) → **T10, T11, T12 in
parallel** (each depends on a subset of T7-T9) → **T13** → **T14**.

- **T7 — LVO metadata tables + registration by name.** New module
  `crates/volamos-core/src/lvos/` with an `LvoEntry` struct (`name`,
  `lvo: i32`, argument registers in order — reuse `DataRegister`/
  `AddressRegister` from `cpu.rs`) and a generated `lvos/dos.rs` table
  for dos.library (full table, not just implemented calls — unknown-
  call diagnostics get real names for free). Generator: a standalone
  script under `tools/` (Python or a small Rust bin, not a workspace
  member) that parses the AROS `dos_lib.sfd` and emits the Rust file
  with a provenance header, per the fd/SFD decision above. Extend
  `LibraryTable` with a `register_by_name(mem, base, table, "Open",
  handler)` convenience that looks up the LVO from the table, and make
  `DispatchError::UnknownCall` candidates resolve through the table to
  print `dos.library/Lock` instead of a raw offset. Keep the existing
  `register` untouched (tests use it).
- **T8 — Guest heap + BPTR/BSTR/C-string helpers.** New module
  `guestmem.rs`: a simple allocator (free-list or bump-with-free over
  a reserved guest heap region above the loaded program, below the
  stack) so handlers can allocate guest-visible structures
  (`FileHandle`, `FileInfoBlock`, string buffers). Not `AllocMem`
  fidelity — no `MemHeader` chains yet (Phase 3); just
  `alloc(size) -> u32` / `free(addr)` with 4-byte alignment. Move
  `read_c_string` out of `dispatch.rs` into it; add `write_c_string`,
  BPTR conversion (`bptr = addr >> 2` and back), and BSTR read/write
  (length-prefixed, 255 max). This forces the first real memory-layout
  decision (heap placement between load end and stack base in the
  1 MiB `FlatMemory`); `Runtime::new`'s stack setup moves onto it.
- **T9 — Volume/assign manager + path translation.** New module
  `vfs.rs`, pure host-side (no `Cpu`/`AddressSpace` dependency —
  fully unit-testable with tempdirs). `VfsConfig`: volume map
  (`name -> host dir`), assign map (`name -> list of Amiga paths`,
  multi-assign search order), auto-assign fallback root, current
  directory (an Amiga path). Resolution: split on `:`; leading `/`
  components as parent-dir; assign expansion (recursive, with a depth
  limit) before volume lookup; then per-component case-insensitive
  matching against real host `read_dir` listings (prefer exact match,
  else unique case-insensitive match, else not-found), preserving the
  host's on-disk case for created files. API sketch:
  `resolve(&self, amiga_path: &str, mode: ResolveMode) -> Result<HostPath, VfsError>`
  where `ResolveMode` distinguishes "must exist" (Open MODE_OLDFILE,
  Lock) from "parent must exist" (MODE_NEWFILE). Mirror vamos's
  semantics wherever this description is ambiguous — vamos is the
  behavioural reference until the Phase 4 oracle harness says
  otherwise.
- **T10 — File I/O handlers** (after T7+T8+T9). `dosfile.rs`: a
  host-side registry mapping guest `FileHandle` structs to
  `std::fs::File`/stdin/stdout, plus handlers for `Open` (MODE_OLDFILE
  1005 / MODE_NEWFILE 1006 / MODE_READWRITE 1004), `Read`, `Write`,
  `Seek` (OFFSET_BEGINNING/CURRENT/END, returns *old* position),
  `Close`, `Input`, `Output`, `IoErr`, `SetIoErr`. Per-run IoErr state
  lives with the registry; every handler sets it faithfully
  (ERROR_OBJECT_NOT_FOUND 205, ERROR_OBJECT_EXISTS 203, etc. — map
  from `std::io::ErrorKind` in one place). `Runtime`'s current
  `out: &mut dyn Write` becomes the `Output()` default handle rather
  than a PutStr-only sink; `PutStr` is reimplemented as a write to
  `Output()` and registered from the T7 table.
- **T11 — Locks, Examine/ExNext, directory traversal** (after T8+T9;
  parallel with T10). `Lock`/`UnLock`/`DupLock` (SHARED_LOCK/
  EXCLUSIVE_LOCK; a lock is a guest struct wrapping a resolved host
  path + our lock id), `Examine`/`ExNext` filling a guest
  `FileInfoBlock` (BSTR `fib_FileName`, `fib_DirEntryType` sign
  convention, `fib_Size`, `fib_Date` — use a fixed epoch-derived
  DateStamp now and note it: Phase 4 freezes virtual time anyway),
  `ParentDir`, `CurrentDir` (updates the VFS cwd). ExNext keeps a
  host `read_dir` iterator keyed by lock id, sorted deterministically
  so parity runs are stable.
- **T12 — Process startup, argument passing, exec OpenLibrary stub**
  (after T7+T8). Replace Phase 1's pre-seeded `A6`: reserve a fake
  ExecBase in the trap-table region, write its address at guest
  location 4 (inside the reserved region — layout must keep address
  4 clear of any jump table), implement `OpenLibrary` (LVO -552) /
  `OldOpenLibrary` (-408) / `CloseLibrary` (-414) returning registered
  bases by name, with the vamos-style escape hatch: unknown library
  names get an auto-created fake base whose vectors all produce named
  "unimplemented library" diagnostics. AmigaOS startup convention:
  `A0` = command-line buffer (args joined, `\n`-terminated), `D0` =
  its length; wire `Options::guest_args` (currently dead code in
  `main.rs`) through `Runtime::new` (signature grows a start-config
  struct — this is the one cross-cutting API change, so T12 owns
  editing `Runtime::new` and T10/T11 must not touch it). Update the
  `hello` fixture convention notes. `ReadArgs` itself is a stretch
  goal here, not a gate — real SAS/C-era binaries parse `A0`/`D0`
  themselves via startup code, which is the corpus Phase 2 targets.
- **T13 — CLI surface for volumes/assigns** (after T9+T12). Extend the
  hand-rolled parser in `crates/volamos/src/main.rs`: repeated
  `-V NAME:hostdir`, repeated `-a NAME:target[+target...]`,
  `--cwd AMIGAPATH`, `--auto-assign HOSTDIR`; defaults mirroring vamos
  (`SYS:`/`root:` conventions only insofar as tests need them — don't
  invent a config-file format this phase). Build the `VfsConfig`, pass
  guest args through. Usage text updated; the "args ignored" note
  deleted.
- **T14 — Phase 2 fixtures + end-to-end tests** (after everything).
  New fixtures in the `hello.s` + `gen_*.py` dual style: (1) write a
  file then read it back and print it; (2) Lock + Examine/ExNext a
  directory and print entries; (3) echo its command line. Integration
  tests in `crates/volamos/tests/` covering: volume mapping,
  assign resolution incl. multi-assign order, case-insensitive lookup
  hitting a mixed-case host tree, MODE_NEWFILE creation, IoErr on
  missing file (guest prints the code), args round-trip. Wired into
  the existing CI workflow. This is Phase 2's "done" criterion.

Worker guidance (same execution model as Phase 1): T7/T8/T9 can go to
three workers immediately; T10/T11/T12 to three workers once their
inputs land (T12 is the only one that edits `Runtime::new` and
`dispatch.rs` structurally — sequence T10/T11 merges around it or land
T12 first within the second wave); T13/T14 are small, sequential
finishers.

## Phase 3 — exec.library essentials + utility.library

Scope: real `AllocMem`/`FreeMem`/`AllocVec`/`FreeVec`/`AvailMem` over a
proper guest allocator (replacing T8's private heap; programs that
inspect `MemHeader`/`MemChunk` are the known edge case), list/node and
message-port primitives to the degree single-threaded CLI tools touch
them, task/signal basics (`FindTask(NULL)`, `SetSignal`, `Wait`,
`SetExcept` minimally), host SIGINT/SIGTERM → `SIGBREAKF_CTRL_C`
delivery and dos-side `CheckSignal` behaviour, stack-size handling
(`--stack`, `StackSwap`, the stack-overflow class of bugs vamos hits),
utility.library (tag lists `GetTagData`/`NextTagItem`/`FindTagItem`,
`Stricmp`/`Strnicmp`, date helpers), `LoadSeg`/`UnLoadSeg` (needed for
`System()`-adjacent work and for the real-library passthrough escape
hatch), and `System()`/`Execute` for tools that shell out. Key
decisions/risks: allocator fidelity vs simplicity (start flat,
add MemHeader emulation only when a corpus binary trips on it); math
libraries — resolve here via real-library passthrough (`LoadSeg` the
original mathffp/mathieee binaries) rather than reimplementing, after
confirming whether the `m68k` crate has any 68881 FPU coverage (the
backend currently runs `CpuType::M68000`; softfloat library variants
shouldn't need FPU opcodes at all); metadata tables for exec/utility
come from the same T7 SFD codegen (`exec_lib.sfd`, `utility_lib.sfd`).

## Phase 4 — parity pass (three-oracle harness)

Scope: a test harness running the same fixture corpus against (1) this
runtime, (2) vamos, (3) Copperline `--run` with a real Kickstart, and
diffing normalised output. Fixture parity via AmiBake's
one-build-many-formats output (same tree as host directory for the HLE
pair, as HDF for the real-ROM column). Normalisations already agreed:
pin one Kickstart version as baseline, capture the Amiga return code,
freeze/fix virtual time so DateStamps compare equal (T11's fixed
DateStamp anticipates this), align stack sizes across runners. CI:
HLE pair (volamos vs vamos) runs on both ubuntu and macos hosted
runners; the real-Kickstart column is gated behind an opt-in
`REAL_ROM_B64`-style secret so forks/PRs skip it. Key risks: this
phase depends on three external tools this repo doesn't control
(vamos/amitools — GPLv2, fine as a test-time tool dependency, nothing
links it; AmiBake; Copperline) — availability, installation method,
and version pinning need answers before the phase starts (see
CLARIFYING QUESTIONS); and oracle inheritance — disagreements resolve
toward the real-Kickstart column, never toward vamos.

## Phase 5 — JIT enablement

Scope: flip the `m68k` crate's Cranelift feature (`features = ["jit"]`)
— JIT lives inside the crate's own `run_batch()`, so no new design work
here beyond plumbing: `M68kCpu` (backend.rs) gains a batch-run path
behind a cargo feature, and `Runtime::run`'s loop uses it when enabled.
The interpreter remains the correctness reference; CI runs the Phase 4
corpus in both modes and diffs. Known divergence risks (self-modifying
code, precise exception timing) are the m68k crate's problem to solve,
ours only to detect. Precondition worth rechecking at phase start:
whether 0.10.x's JIT feature still has the same API shape, given the
crate's release cadence.

## Phase 6 — static Linux container image

Scope: `x86_64-unknown-linux-musl` (and likely aarch64) static builds
of the `volamos` bin, packaged `FROM scratch` or distroless;
Dockerfile + CI job publishing on tags. Validation is behavioural, not
just "it starts": run a real cross-compile job (an Amiga compiler from
the corpus building a program) inside the container. Small risks:
musl vs glibc differences in filesystem/locale behaviour surfacing in
the VFS case-insensitivity code, and keeping image size honest (static
binary + fixtures only).

## Out of scope for all phases (separate future proposals, unchanged)

GUI tier via AROS library ports; ARexx port bridging; native macOS
.app bundle generation. Also fixed non-goals: no GUI/Intuition, no
custom-chip access, no cross-process IPC/message-port bridging —
console tools only.

## CLARIFYING QUESTIONS (open as of start of Phase 2)

1. **fd/SFD decision — one-time sign-off.** The decision above (AROS
   SFDs as reference, codegen extracting *only* name/LVO/register
   facts into checked-in MIT/Apache Rust tables, SFDs never vendored)
   is made and Phase 2 proceeds on it. It rests on the position that
   ABI facts are not copyrightable expression. If you're not
   comfortable with that position for a project you distribute under
   MIT OR Apache-2.0, say so now — the fallback (hand-typed tables
   from documentation, call-by-call) changes T7's mechanics but
   nothing downstream. Note the proposal's premise that vamos's
   tables are "BSD-ish" was wrong: amitools is GPLv2, so that option
   was never actually clean.
2. **Phase 4 external tools.** The three-oracle harness depends on
   AmiBake and Copperline, neither of which this repo controls (plus
   vamos, which is pip-installable). Where do AmiBake and Copperline
   come from — are they available/installable today, should the
   harness pin specific versions, and do you have a Kickstart image
   (and which version to pin as baseline) for the `REAL_ROM_B64`
   secret? Doesn't block Phase 2/3, but should be answered before
   Phase 4 planning firms up.
3. **Empirical corpus.** The long tail of library calls is meant to be
   discovered by running real tools, not reading RKRM cover-to-cover.
   Which binaries should anchor Phase 2/3 acceptance — e.g. vbcc/vasm/
   vlink Amiga-hosted builds, SAS/C, lha? Phase 2's fixtures are
   hand-authored either way, but the choice decides which calls Phase
   3 must implement first (and whether math libraries can stay
   deferred).

(Resolved, for the record: the `m68k` crate pin `=0.10.14` is still
the newest release as of 2026-08-18 — no churn action needed; and
ReadArgs placement is decided — stretch goal in T12, otherwise Phase
3.)
