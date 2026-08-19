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
- **CPU speed: fastest possible, no throttling** (decided 2026-08-18).
  `Runtime::run` steps the CPU as fast as the host allows; there is no
  cycle-timing/pacing code anywhere in the runtime (the `m68k` crate
  does carry per-instruction cycle-count tables internally, ported from
  Musashi for its own correctness/JIT purposes, but volamos doesn't use
  them for wall-clock throttling). This is deliberate, not an oversight
  — it matches the project's core value proposition ("same as vamos,
  but faster"), and unlike a full-system emulator there's no hardware
  (CIA timers, Copper, audio) that emulated time needs to stay in sync
  with. Known edge case, not a blocker: software with a busy-wait delay
  loop calibrated to real 68000 speed (7.09/7.16 MHz) will see that
  loop finish near-instantly rather than after a real delay — harmless
  for the CLI-tool target audience (compilers/assemblers/linkers), but
  worth remembering if a corpus binary ever behaves oddly around
  timing. If that happens, the fix is a **per-invocation, opt-in** knob
  (e.g. a `--speed`/cycles-per-second CLI flag using the crate's
  existing cycle-count data to pace execution for just that run) —
  not a global default, since fastest-possible should stay the
  default for everything else.

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
- **AROS's `.conf` interface descriptions** — AROS's build generates
  `.sfd`/`.fd` files for its ROM libraries *from* a `.conf` source file
  per library (e.g. `rom/dos/dos.conf`); AROS does not check in core
  `.sfd` files at all (a GitHub code search found only 15 `.sfd` files
  in the whole tree, all in third-party/contrib components — SFD is an
  NDK-3.9-lineage format AROS barely uses). **Chosen as the reference
  source**: the `.conf` file's `##begin functionlist` block is the same
  facts an SFD would encode, under the AROS Public License (an
  MPL-1.1-derived, file-level copyleft license), with one important
  qualifier: we do **not** vendor the `.conf` file into this repo. A
  one-shot codegen tool (`tools/gen_lvos.py`, T7 below) reads it and
  emits our own Rust tables containing *only ABI facts* — function
  name, LVO offset, register list — with a provenance comment. No
  descriptive text, comments, or file structure is copied. The
  generated `.rs` files are checked in (codegen is not a build-time
  dependency), so contributors and CI never need AROS's source at all.
  (Earlier drafts of this document called this source "AROS SFD
  files" — corrected 2026-08-18 after a licensing due-diligence pass;
  see below.)

**Rationale (revised 2026-08-18 after a dedicated due-diligence
pass — see the "Licensing due diligence" subsection below for the full
report).** The original framing was "AROS's license is what makes this
safe." That was imprecise and has been corrected: AROS's `.conf` data
is not independently clean-room-verified against Commodore's ABI — it's
*deliberately identical* to it, because binary compatibility with real
Amiga software requires it. So AROS's permissive license doesn't, by
itself, "launder" the content; if the underlying facts were protectable
expression, AROS's own copy would have the same problem, license or
not. **The actual safety net is that bare ABI facts — a function name,
a signed offset, a short list of register letters — are themselves
very likely uncopyrightable** under both US law (17 U.S.C. §102(b);
*Feist Publications v. Rural Telephone*, 499 U.S. 340 (1991); *Lotus
Development v. Borland*, 49 F.3d 807 (1st Cir. 1995); the merger
doctrine, since there is only one way to express "Open is at LVO -30
taking D1, D2") and EU/UK law (*SAS Institute v. World Programming*,
CJEU C-406/10 (2012): "neither the functionality of a computer program
nor the programming language and the format of data files … constitute
a form of expression" protected by the Software Directive). Note for
the record: *Google v. Oracle*, 593 U.S. 1 (2021) is sometimes cited as
holding APIs uncopyrightable — it did not; the Court assumed
copyrightability arguendo and decided the case on **fair use**, for a
much larger taking (11,500 lines of declaring code) than anything at
stake here. Given that theory, AROS remains the right *source* to use
it against: if the "just facts" position were ever challenged, AROS is
an open, non-litigious counterparty whose worst case is APL compliance
obligations, which is a categorically safer position than the
alternative of extracting from Hyperion/Cloanto's proprietary,
no-stated-license NDK files (see the due-diligence subsection). If the
human disagrees with the "ABI facts" position, the fallback is
hand-typing tables from documentation as calls are needed — slower but
equally clean; see CLARIFYING QUESTIONS.

### Licensing due diligence (2026-08-18)

A dedicated research pass (fable agent, WebSearch/WebFetch, plus direct
inspection of a locally downloaded NDK 3.2 R4 archive from Aminet) was
run specifically to stress-test this decision rather than rubber-stamp
it. Summary of what it found and changed:

- **Verified the actual generator output is facts-only.**
  `tools/gen_lvos.py`'s emitted `.rs` tables were checked directly:
  name/offset/register data only, no argument names, no C types, no
  comments carried over from the source. Flagged risk to guard against:
  the `.conf`/`.fd`/`.sfd` *source* formats do carry comments and typed
  argument names, so a future careless edit to the generator could
  start pulling in expression rather than facts — worth a standing
  warning in the generator (added, see `tools/gen_lvos.py`'s docstring)
  and in code review for any change to it.
- **Examined the official NDK directly.** Downloaded and read NDK 3.2
  R4 (Hyperion Entertainment's own Aminet upload). Finding: there is no
  license at all — no `LICENSE` file, `ReadMe-NDK.txt` is a changelog
  with no legal terms, and neither the `.fd` nor `.sfd` files carry a
  copyright header. The C headers claim Hyperion copyright ("Developed
  under license") but grant no reuse rights. This is *not* better than
  "unknown" (as one open-source Amiga toolchain project's own README
  candidly labels NDK 3.2's license) — it is copyright-default
  all-rights-reserved, from a rightsholder lineage (Hyperion/Cloanto)
  with a real history of Amiga IP litigation. **Conclusion: the NDK's
  own files must never be used as a codegen source for committed
  tables** — see `tools/ndk_verify.py` below for the one sanctioned use
  of a local NDK copy.
- **GPL tool taint check**: running a GPL-licensed conversion tool
  (`fd2sfd`/`sfdc`) over NDK data doesn't change the NDK data's license
  status in either direction — GPL only covers a tool's own code, not
  transformed input data that doesn't embed pieces of the tool itself
  (GNU GPL FAQ, "In what cases is the output of a GPL program covered
  by the GPL?"). Moot for us anyway: `gen_lvos.py` needs no such tool.
- **Ecosystem precedent**: amitools/vamos (GPLv2) ships Commodore/NDK
  `.fd` files verbatim with no license grant found for them; the
  AmigaPorts m68k-amigaos-gcc project's own README calls NDK 3.2
  "unknown license" while distributing it anyway. The community norm is
  "ship it and rely on rightsholder tolerance." An MIT/Apache project
  should not adopt that norm; AROS's `.conf` files, under an open
  license from a cooperative counterparty, remain the better source
  even though the underlying facts are identical either way.

**Verdict: proceed as-is (AROS `.conf` as the generated, committed
source), reframed on uncopyrightability rather than AROS's license per
se, with the official NDK held in reserve strictly as a local,
never-vendored, never-committed verification oracle** — which is
exactly what `tools/ndk_verify.py` (added 2026-08-18) does; see below.
This is due-diligence research, not legal advice, and is US/EU-centric;
Simon's specific distribution jurisdiction hasn't been analyzed.

### `tools/ndk_verify.py` — NDK cross-check tool (not a codegen source)

Simon asked directly: since the facts themselves are uncopyrightable,
could a tool parse the *official* NDK `.fd`/`.sfd` files and keep only
our own derived facts in the repo, the same way `gen_lvos.py` does for
AROS? Mechanically yes — but the practical risk is asymmetric even
under an identical legal theory (see the due-diligence subsection
above): if the "just facts" position were ever challenged, AROS's
worst case is an open-source project's license-compliance ask, while
the NDK's worst case is a claim from a rightsholder with real
litigation history and *no* stated license to fall back on at all. So
the NDK is used for **verification only**, never as an input to any
committed, generated file:

- `tools/ndk_verify.py` takes `--ndk-fd <path>` (a local, personally
  obtained copy of an official NDK `_lib.fd` file, kept **outside**
  this repo — the tool refuses to run against a path inside the repo
  tree as a defense-in-depth check) and `--our-table <path>` (one of
  our committed `lvos/*.rs` files), parses both for the same bare
  facts, and prints a diff: agreements, real disagreements (offset or
  register mismatches — these are the ones worth investigating),
  NDK-only entries (not yet implemented, informational), and
  our-table-only entries (worth understanding — could be a genuine
  AROS extension beyond the real ABI, exactly the KS/WB 3.1-drift risk
  this document already flagged).
- **First real run, against NDK 3.2 R4's `dos_lib.fd`/`exec_lib.fd`**:
  `dos.library` — 154 of 162 committed entries overlap with the NDK
  file and **all 154 agree exactly** (offset and registers) — zero
  disagreements. The 8 entries in our table but not in NDK's file are
  AROS's own private-slot names (`OpenLib`/`CloseLib`/etc. — NDK names
  the *same* slots `dosPrivate1..7`, a naming difference, not a
  functional one) plus a few real AROS-only additions
  (`AssignAddToList`, `DisplayError`, `DosGetString`, `ScanVars`,
  `GetSegListInfo`, `CliInit`) to note as out-of-scope-for-3.1
  candidates if the exec/dos handler implementation ever reaches them.
  `exec.library` — 115 of 153 entries overlap and **all 115 agree**;
  38 of our table's entries (mostly `AVL_*` tree helpers and other
  AROS-internal additions) are confirmed **not** present in the real
  NDK 3.2 API at all — concrete evidence of the AROS-drift risk this
  document already anticipated, and a concrete list to exclude/flag
  when Phase 3 picks which exec.library calls to implement.
- **Bonus finding, not yet acted on**: a handful of NDK `.fd` comments
  record a minimum version in recognizable short phrasings (e.g. "added
  for V39 dos", "unimplemented until dos 36.147") — `ndk_verify.py`
  extracts just the version token from these (never the full sentence)
  and reports them as candidates for a future `since`-version field
  (see the next subsection). This is the seed of a fix for the
  "where does AROS end and 3.1 begin, and where will 3.2+ begin later"
  concern Simon raised — see below.
- Not wired into CI and not a build dependency — it's a manual,
  local, dev-time cross-check, run by hand when there's an official
  NDK copy available to check against.

### Future work: a `since`-version field (not yet implemented)

Simon's concern, restated: an AROS-derived table entry currently has
provenance (*which AROS source it came from*) but no statement of
*which real AmigaOS version it's actually valid for* — which matters
now for staying within the KS/WB 3.1 target, and will matter again
when the project later moves to targeting 3.2+. Proposed fix, not yet
built: add a `since: <version>` field to `LvoEntry`, populated from the
NDK Autodocs' own "available since Vxx" statements (a historical fact
with even less claim to expression than an offset+register list) —
cross-referenced via a tool in the same spirit as `ndk_verify.py`, but
note Autodocs are prose-heavy in a way `.fd` files are not, so that
tool needs the same "extract only the version token, never the
surrounding text" discipline applied even more carefully. Once
present, targeting a given OS version becomes a mechanical filter
(`since <= V40` for 3.1, `since <= V45` for 3.2) rather than a fresh
audit each time the target changes. Deferred rather than built now —
scope it when Phase 3 needs to start making since-version-gated
implementation decisions.

**3.1-compatibility note (added 2026-08-18, after the sourcing decision
above but relevant to it):** AROS's SFDs track AROS's own evolving API,
which is a superset of — and in places diverges past — genuine
Kickstart/Workbench 3.1 (V40). Now that Simon has set KS/WB 3.1 as the
first-stage compatibility target, LVO/register facts pulled from AROS
should be treated as *candidates* to cross-check against 3.1-era
documentation (NDK 3.1 Autodocs, RKRM) where a call's signature might
have changed since V40 — not assumed correct as-is. This doesn't change
the sourcing decision (still AROS SFDs, still facts-only), just adds a
verification step worth doing before trusting a generated table entry
for a call known to have shifted post-3.1.

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
real-library-backed base can slot in later. Math libraries (mathffp/mathieee) are deferred with that note.
**Resolved (2026-08-18, verified against the crate source, not just its
docs)**: the `m68k` crate has full FPU support — a complete `fpu` module
(80-bit extended-precision softfloat engine, FPCR rounding modes,
exception accumulation, transcendentals), always compiled in, not
feature-gated. `CpuCore.fpu_present: bool` is independent of `CpuType`,
covering both the discrete 68881/68882 coprocessor and the on-chip
68040/68060 FPUs (`decode.rs` explicitly handles the "68020/030 without
an attached 68881/68882" no-FPU case). `M68kCpu::new` currently selects
`CpuType::M68000` with no FPU wired up — that's a Phase 3 configuration
task (pick a `CpuType`/`fpu_present` combination once a call needs it),
not a crate-capability gap. Whether Phase 3 needs FPU emulation at all
is still a scope question (mathffp/mathieeesingbas are themselves
software-implemented and may not need one; real-library passthrough of
mathieee doubles might), not a tooling one.

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

## Phase 2 as built — verification notes (2026-08-18)

Phase 2 is complete, committed, and pushed, one commit per stage
(T7+T9 60184dd, T8 8d60f5a, T12 9ecfc69, T10 0c56974, T11 e792870,
T13 54a8542, T14 7d9c0c2). All of `cargo build`, `cargo test
--workspace` (124 core unit tests + 23 CLI unit tests + 15
integration tests), `cargo fmt --all --check`, and `cargo clippy
--all-targets -- -D warnings` are clean; still zero dependencies
beyond `m68k = "=0.10.14"`. Where the implementation diverged from
the T7-T14 text above, the code is authoritative:

- **T7 source substitution**: AROS does not check in a generated
  `dos_lib.sfd`; `tools/gen_lvos.py` reads the equivalent facts from
  `rom/dos/dos.conf` (and `rom/exec/exec.conf` for T12) at commit
  `d649ad4cd366bdcfe226ad70d5720c192cfe4653` instead — same
  name/bias/register facts, same facts-only extraction, provenance
  headers in the generated `lvos/dos.rs` (162 entries) and
  `lvos/exec.rs`. All spot-checked LVOs matched published values.
- **Module layout added**: `lvos/` (`LvoEntry`, `ArgReg`, generated
  tables), `guestmem.rs` (`GuestHeap`, 64 KiB stack top region,
  c-string/BPTR/BSTR helpers), `vfs.rs` (pure host-side volumes/
  assigns/auto-assign/cwd, `resolve_with_amiga_path`), `dosfile.rs`
  (`DosState`, file handles, IoErr, error-code mapping), `doslock.rs`
  (locks, Examine/ExNext, CurrentDir/ParentDir).
- **T12**: `Runtime::new(cpu, mem, StartConfig { entry, load_end,
  args })`; heap runs from `load_end` to the stack base; guest args
  are passed AmigaOS-style (space-joined, `\n`-terminated heap
  buffer, `A0`/`D0`). `EXEC_LIBRARY_BASE = 0x0F00`, AbsExecBase
  written at guest address 4; OpenLibrary/OldOpenLibrary/CloseLibrary
  registered from the generated exec table. The vamos escape hatch is
  ported: OpenLibrary of an unknown name auto-creates a fake base
  (4 KiB heap-carved jump-table block prefilled with a shared
  fake-vector slot) that fails with a named diagnostic only when a
  vector is actually called; `LibraryRegistry` is shaped for Phase
  3's real-library passthrough. **Deviation**: `A6` is still seeded
  with `DOS_LIBRARY_BASE` as a documented compatibility shim for the
  Phase 1 `hello` fixture — the real location-4 + OpenLibrary flow is
  fully functional (the three T14 fixtures use it) and the seed can
  be dropped once `hello` is retired or rebuilt.
- **T11 DateStamp**: fixed all-zero `ds_Days/ds_Minute/ds_Tick`, as
  planned, pending Phase 4's frozen virtual time. ExNext listings are
  byte-sorted for deterministic parity runs.
- **T13**: `-V/--volume`, `-a/--assign` (multi-target with `+`),
  `--cwd`, `--auto-assign`; a Vfs is only installed when at least one
  such flag is given. `--cwd` defaults to the first volume's root,
  else the first assign's root, else `root:`.
- **`RAM:` has no special treatment** (confirmed with Simon
  2026-08-18): it is just another volume name, resolved the same way
  as any other unmapped volume — via `-V`/`-a`/`--auto-assign`, or a
  clean `IoErr` if none covers it. This matches vamos's own behavior
  exactly (checked against vamos's docs: vamos aborts on an unmapped
  volume unless Auto Assign is enabled, with no ramdisk special-case
  either) and the project's general "port vamos's escape hatches, don't
  invent new ones" stance. Known practical wrinkle: real hardware
  auto-mounts `RAM:` at boot, so some real-world scripts/tools assume
  it exists without ever assigning it — under volamos those need an
  explicit `-V RAM:<hostdir>` or `--auto-assign` today. Considered and
  rejected: having volamos auto-provide a default host-backed `RAM:`
  (e.g. a managed temp directory) unless overridden — decided against,
  since it would mean volamos silently creating/managing filesystem
  state the user didn't ask for, which cuts against the explicit
  everything-is-mapped-by-the-invoker design used everywhere else in
  the volume/assign model.
- **T14**: fixtures `filetest`/`dirtest`/`echoargs` in the dual
  `.s` + `gen_*.py` style, sharing `fixtures/amiga_asm.py` (a small
  two-pass assembler helper the generators use; vasm still not
  required). 8 end-to-end tests drive the CLI binary across volume
  mapping, NEWFILE round trip, IoErr failure paths, multi-assign
  order, case-insensitive lookup, and args round trip. No
  volamos-core bugs surfaced while writing them.
- **ReadArgs**: stretch goal not taken; deferred to Phase 3 as
  recorded in the clarifying questions. `Open("CONSOLE:")`/`Open("*")`
  are likewise not yet special-cased; `Input()`/`Output()` defaults
  cover the current corpus.

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
add MemHeader emulation only when a corpus binary trips on it).

**Math libraries — corrected 2026-08-18 (ROM vs. disk residency
checked before committing to an approach).** Simon flagged that the
"just `LoadSeg` the original binaries" plan needed verifying: if a
library is ROM-resident, there is no standalone disk file to ever
`LoadSeg` in the first place, real Workbench 3.1 media included, and
the only way to obtain the actual code would be extracting it from a
Kickstart ROM dump — a much bigger licensing question than anything in
the fd/SFD analysis above (Kickstart ROM images are Cloanto/Hyperion
copyrighted binary code, not extractable facts), and one this project
has deliberately not taken on (see Phase 4's `REAL_ROM_B64` opt-in-only
treatment of ROM images). Checked against the V37-era RKRM's *Math
Libraries* chapter (explicit "resides in ROM" / "resides on disk"
statements per library) and cross-referenced against the NDK 3.2
autodoc set (covers V40–V47; no evidence of a residency change, and
confirms the complete real library list):

- **ROM-resident — no disk file exists at all; real-library
  passthrough is not possible for these**: `mathffp.library`;
  `mathieeesingbas.library` (ROM-resident since V36, i.e. still ROM in
  our V40/3.1 target). If a corpus binary ever needs these, the only
  options are a from-scratch Rust reimplementation (both are simple,
  fully public numeric formats — 32-bit FFP and IEEE-754 single, no
  Commodore source needed to reimplement the arithmetic itself) or
  requiring a user-supplied Kickstart ROM dump, at which point it's
  arguably simpler to lean on Phase 4's Copperline-with-real-Kickstart
  path instead of a bespoke ROM-extraction mechanism here.
- **Disk-resident under `LIBS:` — real-library passthrough via
  `LoadSeg` remains viable, copyable from a genuine Workbench 3.1
  install disk**: `mathtrans.library` (FFP transcendental),
  `mathieeesingtrans.library`, `mathieeedoubbas.library`,
  `mathieeedoubtrans.library`.
- **`mathieeeextbas.library` does not exist.** Absent from the
  complete NDK 3.2 autodoc library list (which does include all six
  real math libraries above) — the original proposal's mention of it
  was a mistake, not a deferred scope item. Drop it from consideration
  entirely rather than treating it as "not yet implemented."

This doesn't change Phase 3's task list: math library support was
already gated on corpus need, not built by default, and remains so —
it just means "when a corpus binary needs `mathffp`/`mathieeesingbas`
specifically, reimplement natively" rather than "LoadSeg it," while
the other four stay LoadSeg-viable as originally planned. The `m68k`
crate's FPU support (confirmed present, see the T12 note above) is
still relevant for a from-scratch `mathieeesingbas`/native-FFP
implementation choosing to use real FPU opcodes rather than a pure
software path, and remains available for LoadSeg'd passthrough of the
four disk-resident libraries too, if their disassembly turns out to
use 68881 instructions directly.

Metadata tables for exec/utility come from the same T7 `.conf`-reading
codegen tool (`exec.conf`, `utility.conf`).

## Phase 3 as built — verification notes (2026-08-18)

Phase 3 is complete, committed, and pushed, one commit per stage
(T15 ce6c3bc, T16 587b957, T17 c36172d, T18 875cf93, T19 3fb371a,
T20 2da8233, T21 a74fb9c, T22 dea1659). All of `cargo build`,
`cargo test --workspace` (219 core unit tests + 33 CLI unit tests + 22
integration tests across six files), `cargo fmt --all --check`, and
`cargo clippy --all-targets -- -D warnings` are clean; still zero
dependencies beyond `m68k = "=0.10.14"`. Math libraries were not
implemented, stubbed, or scaffolded, per the residency correction
above. Stage commits:

- **T15 (`ce6c3bc`)** — generated `lvos/utility.rs` (44
  entries) from AROS `rom/utility/utility.conf` via `tools/gen_lvos.py`,
  same provenance discipline as T7.
- **T16 (`587b957`)** — `execmem.rs`: `AllocMem`/`FreeMem`/`AllocVec`/
  `FreeVec`/`AvailMem` over the *same* `GuestHeap` T8 built (flat, no
  `MemHeader`/`MemChunk` emulation, exactly as scoped — "start flat").
  8-byte size rounding at the call boundary; `FreeMem` fails loudly on
  size mismatch or unknown/double-freed blocks (bug-catching posture);
  `AllocVec` uses an 8-byte in-guest size header. (This stage was
  recovered and committed by the coordinating session after the first
  orchestrator run died to a session limit; independently re-verified.)
- **T17 (`c36172d`)** — `utility.rs` handlers: `GetTagData`/
  `FindTagItem`/`NextTagItem` (full `TAG_MORE`/`TAG_SKIP`/`TAG_IGNORE`
  traversal, NULL list legal), `Stricmp`/`Strnicmp`/`ToUpper`/`ToLower`
  (Amiga "international" Latin-1 case convention, documented as the
  no-locale default), `Amiga2Date`/`Date2Amiga`/`CheckDate` (seconds
  since 1978-01-01, which was a Sunday; `wday` 0 = Sunday).
  `UTILITY_LIBRARY_BASE = 0x0C00`, in the reserved-region gap between
  the dos and exec jump tables; registered with `LibraryRegistry` so
  `OpenLibrary("utility.library")` resolves to it.
- **T18 (`875cf93`)** — `execlist.rs`: `AddHead`/`AddTail`/`Remove`/
  `RemHead`/`RemTail`/`Insert`/`Enqueue`/`FindName` operating directly
  on guest `struct List`/`Node` memory with the real sentinel layout
  (guest code that walks the structures itself sees real-AmigaOS bytes);
  single-threaded `CreateMsgPort`/`DeleteMsgPort`/`PutMsg`/`GetMsg`/
  `ReplyMsg` (non-blocking `GetMsg`, no signaling). `AddPort`/`RemPort`/
  `FindPort` are minimal (no public port registry; `FindPort` returns
  NULL) per the fixed "no cross-process IPC" non-goal. `init_list_header`
  is the public `NewList` equivalent.
- **T19 (`3fb371a`)** — `exectask.rs`: a real 92-byte guest `struct
  Task` allocated at `Runtime::new` time (guest memory is the sole
  signal-state authority; maintained fields: `ln_Type`/`ln_Name`,
  `tc_SigAlloc` init `0x0000FFFF`, `tc_SigWait`, `tc_SigRecvd`,
  `tc_SigExcept`, and — from T20 — `tc_SPLower`/`tc_SPUpper`).
  `FindTask(NULL)` returns it; non-NULL names return 0 (single-tasking).
  `SetSignal`/`SetExcept`/`AllocSignal`/`FreeSignal`/`Signal` per the
  real contracts; `Wait` returns the satisfied subset or **fails loudly
  if it would block forever** (vamos-style trap on unrunnable waits).
  Host SIGINT/SIGTERM → `SIGBREAKF_CTRL_C` via a hand-rolled
  `#[cfg(unix)]` `signal()` binding (no new dependency) setting an
  atomic that `Runtime::run` folds into `tc_SigRecvd` once per
  dispatched trap (documented granularity: compute-bound loops see it
  only at their next library call); installation is explicit
  (`install_host_break_handler()`, called by the CLI, never by
  `Runtime::new`). `dos.library` `CheckSignal` returns-and-clears
  masked pending signals. `CreateMsgPort` now fills `mp_SigTask`.
- **T20 (`2da8233`)** — stack handling: `StartConfig::stack_size`
  (default 64 KiB, clamped to a 4096-byte minimum), CLI `--stack SIZE`
  with `K`/`M` suffixes; `tc_SPLower`/`tc_SPUpper` populated;
  `StackSwap` (LVO -732) swaps `A7` + task bounds with the guest
  `StackSwapStruct`, keeping `stk_Pointer` values in "nothing pending"
  form (+4 adjustment) and re-pushing the return address onto the new
  stack so the generic post-dispatch RTS works — round-trip proven by
  test. `Runtime::run` checks `A7` against the (StackSwap-aware) task
  bounds once per dispatched trap, failing with `DispatchError::
  StackOverflow` naming `A7`, the bounds, and `--stack` — the vamos
  stack-overflow bug class, caught loudly. `NewStackSwap` left
  unregistered. This commit also serializes `exectask`'s host-break
  tests behind a test-only mutex, fixing a parallel-test race on the
  global pending-break atomic found during orchestrator re-verification.
- **T21 (`a74fb9c`)** — `dosseg.rs`: `LoadSeg` builds a **real BPTR
  seglist** (per-hunk `GuestHeap` blocks framed
  `[ULONG length][BPTR next][payload]`, BPTR addressing the link field,
  RELOC32 applied against payload addresses, BSS zero-filled), resolves
  paths through the Vfs with `Open(MODE_OLDFILE)` semantics, sets
  `IoErr` (`ERROR_OBJECT_NOT_FOUND`; `ERROR_FILE_NOT_OBJECT` (121) for
  a non-hunk file) and returns 0 on failure. `UnLoadSeg(0)` is a legal
  no-op; unknown seglists fail loudly. `SystemTagList`/`Execute` route
  through a host-side runner hook (`Runtime::set_system_runner`) the
  CLI installs to run the resolved program as a **fresh nested
  `Runtime`** sharing the parent's Vfs config (vamos-style); without a
  runner they fail cleanly per the real return-value contracts (`System`
  → command exit code or -1; `Execute` → BOOL). Documented scope cuts:
  no `SYS_Input`/`SYS_Output`/`Execute` redirection yet; nested output
  goes to process stdout. Proven per the plan's requirement by loading
  arbitrary small hunk executables (in-test synthesized binaries and the
  new `systest` fixture), not any real AmigaOS library.
- **T22 (`dea1659`)** — fixtures + e2e (the phase's "done" criterion):
  `exectest` (AllocMem/AllocVec round trips, real
  `OpenLibrary("utility.library")` + `Stricmp`/`GetTagData`/`Strnicmp`,
  `FindTask`/`SetSignal` + `CheckSignal`), `recurse` (deep `bsr`
  self-recursion tripping the stack-overflow guard for real), plus
  T21's `systest` (nested `SystemTagList` execution with output
  interleaving and exit-code propagation). 5 new `phase3_e2e.rs` tests +
  4 `dosseg_e2e.rs` tests drive the actual CLI binary; CI's existing
  `cargo test --all` picks them up (no workflow change needed). SIGINT
  delivery is deliberately unit-test-only (no long-running fixture
  exists to make real delivery observable rather than racy — rationale
  documented in `phase3_e2e.rs`).

**`ReadArgs`/`FreeArgs` — implemented 2026-08-18** (`crates/volamos-core/
src/dosargs.rs`), ahead of Phase 4 rather than left deferred: the
empirical-corpus decision above flagged it as one of the few named gaps
since every real `C:` command template-parses its arguments this way.
Full template syntax (`/A/K/S/T/N/M/F`, dual keyword names, the
documented `/M`+trailing-`/A` tail-borrowing rule from `Copy`'s
`FROM/A/M,TO/A`), a from-scratch `ReadItem`-equivalent tokenizer
(quoting, the `*"`/`*n`/`*e`/`**` escapes), and all six `ReadArgs`-
specific `IoErr()` codes (114–119). Default source only (`rdargs ==
NULL`, reading the same `A0`/`D0` command-line buffer `Runtime::new`
already builds); a caller-supplied `RDArgs` with its own `RDA_Source`
is accepted only as a `FreeArgs`-bookkeeping identity, not as an
alternate string to parse — no real `C:` command needs that. `/T`
(toggle) follows the NDK autodoc's simpler "flips on each occurrence"
behavior over an older RKRM passage describing an explicit `On`/`Off`
value, since no known real template uses `/T` either way. 14 tests
(13 direct-call unit tests over the parsing/materialization logic, one
real A-line trap-dispatch end-to-end test for the register convention).
**Flagged by Simon (2026-08-18) as needing real-oracle comparison
testing before being trusted** — implemented from RKRM/NDK docs alone,
not yet validated against real AmigaOS or an independent
implementation; revisit once Phase 4's real-Kickstart oracle
(Copperline/amiberry) is available.

**`ParsePattern`/`ParsePatternNoCase`/`MatchPattern`/
`MatchPatternNoCase` — implemented 2026-08-18**
(`crates/volamos-core/src/dospattern.rs`), the wildcard-matching engine
`List`/`Copy`/`Delete`/`Dir` and effectively every other `C:` command
with a filename argument rely on. Full grammar (`?`, `#atom` repeat,
`~atom` negation, `[...]`/`[~...]` classes with ranges, `(a|b|c)`
alternation, `%` empty alternative, `'` escaping -- correctly scoped to
only activate before an actual wildcard character, per the real
semantics rather than the naive reading). Tokenized pattern buffers use
this runtime's own self-delimiting byte encoding rather than the real
(explicitly "internal") one, since `ParsePattern`/`MatchPattern` are
only ever exchanged with each other; decoding reads directly from guest
memory rather than via a NUL-terminated string read, since the encoding
embeds raw `0x00` bytes a naive `read_c_string` truncates on (caught by
the end-to-end trap-dispatch test, not the pure-Rust unit tests -- a
good reminder that in-process unit tests over the parsing logic don't
exercise the guest-memory marshalling code path at all). Scoped
deviation: `~atom` matches against the *whole remainder* of the string
rather than some arbitrary prefix, matching every real-world `~` usage
this project's corpus is expected to hit (`~(#?.info)`, always the
pattern's tail) but not a `~` with trailing atoms after it. **Not yet
implemented**: `MatchFirst`/`MatchNext`/`MatchEnd` (the `AnchorPath`/
`AChain`-based recursive directory scanner built on top of this engine)
-- deliberately split out as its own follow-up, since real struct-layout
fidelity for `AnchorPath`/`AChain` (guest programs read `ap_Info`/
`ap_Current->an_Lock` directly, unlike `ReadArgs`'s opaque `RDArgs`) is
a distinct chunk of work from the matcher itself. 12 tests (11 direct
unit tests over the parser/matcher, one real A-line trap-dispatch
end-to-end test).

Known deviations/deferrals, all documented in-module: no
`MemHeader`/`MemChunk` guest structures (flat allocator, per plan); no
public message-port registry (`FindPort` → NULL); signal exceptions
(`SetExcept`) tracked but never delivered; stack/host-break checks
happen at library-call granularity only; `NewStackSwap` and
`System()` I/O-redirection tags unimplemented pending corpus need.
The `since`-version field proposed above remains future work — Phase 3
picked its calls from the plan's own scope list rather than needing a
version-gated filter yet.

**First empirical corpus run — 2026-08-18** (`crates/volamos-core/src/
{dosstr,execmem,execfmt,dosvar,dosprintf}.rs`): ran the real Workbench
3.1.4 `C:/Version` binary (extracted from `~/src/amibake/assets/
hyperion/AmigaOS-3.1.4-A500_A600_A2000.zip`'s `Workbench3_1_4.adf`,
kept outside this repo per the never-vendor policy) end to end for the
first time, adding whatever `dos.library`/`exec.library` calls it hit
that weren't implemented yet: `StrToLong` (decimal string parsing);
`CopyMem`/`CopyMemQuick` (raw memory copy, added to `execmem.rs`);
`RawDoFmt` (the `printf`-like formatter every AmigaOS C startup library
builds `sprintf`/`Printf` on top of -- the first handler in this
runtime that steps the CPU itself mid-handler to call back into a real
guest `PutChProc` subroutine, rather than only reading/writing
registers and memory); `SetVar`/`GetVar`/`DeleteVar` (local shell
variables only, no `ENV:`-backed global storage -- see `dosvar.rs`'s
module docs); `VPrintf`/`VFPrintf` (built on `RawDoFmt`'s shared
`render_format` core, but writing straight to `Output()`/a file handle
instead of a guest callback). `Version` now runs to completion (exit
code 0) and prints real formatted output via `VPrintf`, but the
version *numbers* themselves are wrong (`Kickstart 40960.40960,
Workbench 0.0` -- `40960` is `0xA000`, this runtime's "unknown call"
trap-table sentinel value): `Version` reads `lib_Version`/`lib_Revision`
directly off the fake `exec.library`/`dos.library` library `Node`
structs, which this runtime has never populated with real values.
That's a distinct, not-yet-scoped gap (real library-Node field
fidelity) surfaced by this run, not a defect in the calls just added.
20 new tests across the five modules (each with at least one real
A-line trap-dispatch end-to-end test, not just direct-call unit tests
-- `dospattern.rs`'s `ParsePattern`/`MatchPattern` work already showed
that in-process unit tests over parsing/formatting logic alone miss
guest-memory-marshalling bugs a real dispatch path catches).

**Follow-up, same day — fully resolved**: `Version` now prints
`Kickstart 40.10, Workbench 40.10`, matching the documented KS/WB 3.1
target, via four incremental fixes to `Runtime::new` (all in
`dispatch.rs`), each found by hand-disassembling the guest code around
the relevant call site (no disassembler is wired into this runtime
yet -- see the new `project-snoopdos-feature-idea` follow-up this
prompted, below):

1. `write_library_node` writes a real `struct Library` header
   (`lib_Node.ln_Type` = `NT_LIBRARY`, `lib_Version`/`lib_Revision` =
   40/10) at `DOS_LIBRARY_BASE`/`EXEC_LIBRARY_BASE`/
   `UTILITY_LIBRARY_BASE`, instead of leaving that memory sentinel-
   filled. Fixed the Kickstart *version* number alone (`40`, was
   `40960`/`0xA000`).
2. `TRAP_TABLE_SIZE` grew from `0x1000` to `0x1200` and
   `write_library_list_nodes` builds a real, walkable
   `ExecBase.LibList` (a real `struct List`, at the true NDK-documented
   offset 378 from `EXEC_LIBRARY_BASE` -- `EXEC_BASE_LIBLIST_OFFSET`,
   whose doc comment has the full field-by-field offset derivation)
   linking real nodes for `dos.library`/`exec.library`/
   `utility.library`, reusing `execlist.rs`'s own `List`/`Node`
   primitives (`init_list_header`/`add_tail_impl`, the latter now
   `pub(crate)`) rather than reimplementing list-splicing. Turned out
   not to be what `Version`'s own Kickstart-line report needed (see
   #4), but is real, generally useful `ExecBase` fidelity for any
   *other* corpus binary that walks library lists (confirmed via the
   disassembly that guest code doing exactly this exists and runs,
   just for a different purpose than first assumed).
3. `version.library` -- a real, if lesser-known, AmigaOS library
   `Version` opens (confirmed by temporarily logging every
   `OpenLibrary` call's requested name) purely to read its own
   `lib_Version`/`lib_Revision` as a stand-in for "the OS release
   version" -- is now a fourth real registered library base
   (`VERSION_LIBRARY_BASE`), with the same header treatment as #1. No
   LVOs registered for it since nothing was seen calling into it, only
   reading its header. Fixed the `Workbench 40.10` line completely.
4. The Kickstart *revision* number turned out to come from neither
   `lib_Revision` nor a `LibList` search: `SoftVer`
   (`EXEC_BASE_SOFTVER_OFFSET` = 34, right after `struct Library`),
   documented in the NDK as "kickstart release number (obs.)" -- a
   single legacy `UWORD`, not a version/revision pair -- is what
   `Version` actually reads for its second Kickstart number. Confirmed
   empirically by writing a distinguishing test value there. A useful
   general lesson recorded alongside this: `EXEC_LIBRARY_BASE`'s entire
   `struct ExecBase` span (`LIB_STRUCT_SIZE` through
   `EXEC_BASE_LIBLIST_OFFSET`) is now explicitly zeroed before any of
   the above overlays their own fields, so any *other* unpopulated
   `ExecBase` field a future corpus binary reads gets a clean,
   unsurprising `0` instead of the sentinel prefill's `0xA000` trap
   opcode pattern.

Also prompted, and immediately implemented (2026-08-19), a
`SnoopDos`-style CLI flag: `-s`/`--snoop` (`crates/volamos/src/
main.rs`), built on a new `CallInfo::detail: Option<String>` field
(`dispatch.rs`) and a `HandlerContext::call_detail` slot handlers can
fill in. `OpenLibrary`/`OldOpenLibrary` and dos.library's `Open` now
populate it (e.g. `library "version.library" -> base 0x00000200
(real)`); `--snoop` prints only those lines, `--verbose` shows them
inline alongside its existing per-call trace. Exactly the tool that
would have made the four fixes above faster to find instead of
hand-adding and removing temporary `eprintln!`s in `open_library_handler`/
`RawDoFmt`/`CopyMem` -- reach for it first on the next corpus binary
that needs this kind of diagnosis.

**`MatchFirst`/`MatchNext`/`MatchEnd` — implemented 2026-08-19**
(`crates/volamos-core/src/dosanchor.rs`), prompted by the real
Workbench 3.1.4 `C:/Type` binary: even a plain non-wildcard filename
argument goes through `MatchFirst` in real AmigaOS (per its own
documented behavior, a `Lock()` on the object directly), so `Type`
needed this before it could open anything at all. Real, byte-accurate
`struct AnchorPath`/`struct AChain` (NDK `dos/dosasl.h`) in guest
memory (unlike `ReadArgs`'s opaque `RDArgs` anchor -- real guest code
reads `ap_Info`/`ap_Current->an_Lock` directly). Scoped to a single
wildcard component (split at the pattern's last `/`/`:`) but with
*full* `APF_DODIR` recursive descent reapplying that same pattern at
every directory level -- covers the NDK's own `ScanDirectories()`
worked example completely. `APF_DIDDIR` restores `ap_Info` to the
just-exited directory's own descriptor (`ScanLevel::self_name`) before
signaling, matching the documented `"leaving %s"` example precisely --
an early version of this got that wrong (leaving stale info from the
last file matched inside the directory) and was caught only by a
dedicated recursive-descent test, not the simpler single-level ones.

**Found and fixed a real, pre-existing bug while building this**:
`doslock.rs`'s `fill_fib` wrote `fib_FileName`/`fib_Comment` as
length-prefixed BSTRs, but the real NDK `struct FileInfoBlock`
declares both as plain `NUL`-terminated `TEXT[]` buffers -- confirmed
against `dos/dos.h` directly. This silently affected `Examine`/`ExNext`
too, not just the new `MatchFirst` code; only surfaced now because
`Type`'s own algorithm (`MatchFirst` → `CurrentDir` into the matched
lock → `Open(ap_Info.fib_FileName, ...)`) was the first real corpus
binary to read `fib_FileName` back out as a plain string. The
Phase 2 `dirtest` fixture (`fixtures/dirtest.s`/`gen_dirtest.py`) had
independently been written to *expect* the BSTR encoding, so it never
caught this -- both the fixture and `fixtures/amiga_asm.py` (a new
`move_b_d_to_postinc` opcode helper, needed for the corrected copy
loop) were fixed and regenerated alongside the runtime fix. Recorded as
a reminder that a fixture can quietly enshrine an implementation bug
it was originally written *against*, rather than against the real
spec -- `docs/plan.md`'s Phase 4 three-oracle harness is exactly the
kind of check that would have caught this far earlier.

8 new tests in `dosanchor.rs` (unit-level non-wildcard/wildcard/
overflow/recursive-descent coverage, plus one real trap-dispatch
end-to-end test), plus the two corrected `doslock.rs` unit tests and
the regenerated `dirtest` fixture's own e2e coverage in
`crates/volamos/tests/phase2_e2e.rs`.

**`FGetC`/`FPutC`/`UnGetC`/`FRead`/`FWrite`/`FGets`/`FPuts`/
`WriteChars`/`Flush`/`SetVBuf` — implemented 2026-08-19**
(`crates/volamos-core/src/dosbuf.rs`), the last gap in `Type`'s own
call chain: after `MatchFirst` + the `fib_FileName` fix above, `Type`
opens the matched file and reads it via buffered I/O rather than raw
`Read()`. Real AmigaOS gives every `FileHandle` a `SetVBuf`-configurable
internal buffer purely as a host-round-trip optimization -- since this
runtime's `DosState::read`/`write` already reach the host file
immediately with no intermediate layer to bypass, `SetVBuf` and `Flush`
are correctness-preserving no-ops (always report success/`DOSTRUE`).
The one piece of real, observable state is `UnGetC`'s one-byte
pushback (`DosState::ungetc_buf`/`last_getc`), which `FGetC` consults
before touching the host file. `Type` now runs fully end-to-end against
the real corpus binary:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Type WORK:hello.txt
Hello from volamos!
This is a test file.
```
8 new end-to-end trap-dispatch tests in `dosbuf.rs` (`FGetC` first-byte
and EOF, `FPutC` to `Output()`, `UnGetC`→`FGetC` pushback round-trip,
`FGets` line read, `FWrite`/`FRead` block-record semantics,
`Flush`/`SetVBuf` no-op success).

**`SelectInput`/`SelectOutput` — implemented 2026-08-19**
(`crates/volamos-core/src/dosfile.rs`), found by running the real
`Type WORK:hello.txt TO WORK:out.txt` invocation: it opens the target
file, calls `SelectOutput` to make it the process's default output,
then writes every line via `VPrintf` (not `Write`/`FPutC` on an
explicit handle). `SelectOutput`/`SelectInput` didn't exist yet, and
worse, `VPrintf` and `PutStr` had both been hard-wired straight to
`ctx.out` (real host stdout) regardless of any selection -- a Phase 1
simplification that was correct only because nothing had ever redirected
`Output()` before. Fixed by splitting `DosState`'s single `output_handle`/
`input_handle` fields in two: the *default*, stdin/stdout-backed handle
(`input_handle`/`output_handle`, used by `is_output_default`/`Close`'s
no-op case -- must never be repointed, or a direct `Write()` to a
`SelectOutput`-selected real file would get hijacked back to stdout) and
the *currently selected* one (`current_input`/`current_output`, what
`Input()`/`Output()` actually return, and what `SelectInput`/
`SelectOutput` repoint). `VPrintf`/`PutStr`/`VFPrintf` were all switched
to route through the same `dosbuf::write_bytes` helper `FPutC`/`FWrite`
already used, rather than three separate ad-hoc `ctx.out`-vs-`dos.write`
branches. `Type ... TO file` now writes only to the file, with clean
(empty) stdout:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Type WORK:hello.txt TO WORK:out.txt
$ cat <vol>/out.txt
Hello from volamos!
This is a test file.
Line three here.
```
3 new tests: 2 unit-level (`select_output`/`select_input` redirect and
report the previous handle, `is_output_default` unaffected by
selection), 1 end-to-end trap-dispatch test (`Open` → `SelectOutput` →
`PutStr`, asserting both the file contents and that `ctx.out` stays
empty).

**`Fault`/`PrintFault` — implemented 2026-08-19**
(`crates/volamos-core/src/dosfault.rs`), found next by running the real
`Type` against a directory argument: after printing its own `"TYPE
can't open %s"`, `Type` calls `PrintFault(IoErr(), "Type")` as its
final command-level error report -- the same pattern the Shell itself
uses (`PrintFault(cli_Result2, cmd)`, per `shell.md`) to surface a
command's overall result. Real `dos.library` keeps this message table
as localized resource strings (`dl_Errors`); this runtime hardcodes the
standard English text for the codes this runtime can actually produce
(plus the wider well-known `dos/dos.h` `ERROR_*` set, for future
corpus binaries), falling back to a generic `"Error N"` for anything
unrecognized -- matching `Fault()`'s own documented fallback. No
separate `pr_CES` error stream exists in this runtime, so both always
write to the current `Output()` selection (matching real pre-V45
AmigaDOS's own fallback when no error stream is configured). Confirmed
against the real binary -- no crash, and a plausible two-line report:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Type WORK:subdir
TYPE can't open subdir
Object not found
```
5 new tests in `dosfault.rs` (message-table lookup for a known and an
unknown code, `PrintFault` end-to-end with a header and with a
zero/no-op code, `Fault`'s buffer-fill-and-truncate behavior).

**`List` gap chain — implemented 2026-08-19.** Moved to the next real
corpus binary (`C:/List`) once `Type` ran cleanly end-to-end; found and
fixed six gaps in sequence, each surfaced only by the next one being
fixed:

- **`DateStamp`** (`crates/volamos-core/src/dosdate.rs`): fills the
  real `struct DateStamp` (`ds_Days`/`ds_Minute`/`ds_Tick`) from the
  host wall clock, via a fixed 2922-day Unix-epoch-to-Amiga-epoch
  offset. `now_as_datestamp()` is reused by `DateToStr`'s `DTF_SUBST`
  substitution below.
- **`AddPart`/`FilePart`/`PathPart`** (`crates/volamos-core/src/
  dospath.rs`): pure path-string functions -- no `Vfs` involved, per
  their own documented contract. `AddPart` reproduces the classic
  algorithm's three cases (colon resets to the device root, each
  leading `/` in the appended name pops one trailing component, then
  the remainder is appended with a `/` separator only if needed).
- **`IsFileSystem`** (`crates/volamos-core/src/dosfs.rs`): collapses to
  "does `name`'s device/volume resolve through the `Vfs` at all"
  (`ResolveMode::ParentMustExist`, so the exact object needn't exist),
  since this runtime only ever backs a path with a real host-directory
  volume (always a file system) or stdin/stdout (never reachable by a
  device string here in the first place). `"*"` (the console) is never
  a file system, per the RKRM's own `IsFileSystem34` workaround.
- **`NameFromLock`** (added to `crates/volamos-core/src/doslock.rs`):
  reuses `LockEntry::amiga_path` (already tracked for `CurrentDir`,
  T11) directly -- the lock that produced it already recorded the
  absolute Amiga path. `lock == 0` resolves to the literal `"SYS:"`,
  matching the RKRM's own documented `ZERO`-lock quirk.
  - **`DateToStr`** (`crates/volamos-core/src/dosdatestr.rs`): renders
  `struct DateTime` (`dos/datetime.h`, 26 bytes, even-byte-packed) into
  weekday/date/time strings, all four `dat_Format` styles, and
  `DTF_SUBST`'s "Today"/"Tomorrow"/"Yesterday"/weekday-name/"Future"
  substitution (comparing against `dosdate::now_as_datestamp`).
- **`dosanchor.rs`: non-wildcard directory descent, and `APF_DirChanged`
  hygiene** -- two real bugs found only once `List` could get this far:
  1. A bare, non-wildcard directory argument (`List WORK:`, no
     wildcard at all) still needs to be descendable via `APF_DODIR` in
     real AmigaDOS -- confirmed directly against the real `List`
     binary, which sets `APF_DODIR` right after a plain `MatchFirst`
     match. The non-wildcard branch previously left `AnchorMatchState::
     pattern` as `None`, which gated descent off entirely. Fixed by
     giving it the `"#?"` (match-everything) pattern and marking the
     level's single synthetic "entry" (itself) via a new `direct_self`
     flag, since it has no real parent/name decomposition to reuse the
     normal recursive-descent path's `join_amiga(parent, name)` logic
     with -- a bare volume root like `"WORK:"` has no distinct "name"
     component to strip back off. First attempt (reusing `dir_part` as
     a fake parent) broke on exactly this case (`"WORK:WORK"`,
     `ERROR_OBJECT_NOT_FOUND`); the `direct_self` flag sidesteps the
     join entirely for both the descend step and the later
     `APF_DIDDIR` pop-cascade (which now uses the popped level's own
     stored path directly, not a re-derived join).
  2. `APF_DirChanged` (RKRM `pattern-matching.md`: "cleared if the
     directory is the same as in the previous iteration") was being set
     once on descent but never cleared for subsequent same-directory
     entries, nor set on the `APF_DIDDIR` pop -- `List` uses this flag
     to decide when to print a new `"Directory ..."` header, so every
     entry got its own header/summary until this was fixed.

`List WORK:` now runs fully end-to-end against the real corpus binary,
producing correctly-formatted output:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/List WORK:
Directory "WORK" on Wednesday 19-Aug-26
big.txt                    66669 ----rwed 01-Jan-78 00:00:00
deep                          Dir ----rwed 01-Jan-78 00:00:00
...
5 files - 2 directories - 141 blocks used
```
New tests: 2 (`dosdate.rs`), 14 (`dospath.rs`), 3 (`dosfs.rs`), 3
(`doslock.rs`, for `NameFromLock`), 10 (`dosdatestr.rs`), 3
(`dosanchor.rs`, for the non-wildcard-descent and `DirChanged` fixes).

**Known remaining `List` gap, not yet chased**: `List` of a
non-existent explicit path (`List WORK:nope`) prints a spurious empty
`Dir` entry instead of a clean error message, though it does still
report a nonzero exit code (20) with no crash -- `MatchFirst` correctly
returns `ERROR_OBJECT_NOT_FOUND`, but something in `List`'s own
failure-handling path still falls through to printing an entry from
unpopulated `ap_Info`. Lower priority than the working-directory case;
revisit if a future corpus binary trips on the same class of bug.

**`Copy` gap chain — implemented 2026-08-19.** Moved to the next real
corpus binary (`C:/Copy`) once `List` ran cleanly; found and fixed four
gaps in sequence:

- **`SetProtection`** (`crates/volamos-core/src/dosprotect.rs`):
  `Copy` calls this after copying a file, to replicate the source's
  protection bits onto the new copy. Scoped to just the `FIBB_WRITE`
  bit, mapped onto the host file's real writability
  (`std::fs::Permissions::set_readonly`) -- the only protection bit
  with a meaningful host-level equivalent. This is narrower than real
  AmigaDOS and one-way: `crate::doslock`'s `fill_fib` still always
  reports `fib_Protection == 0`, so the effect isn't reflected back
  through a later `Examine`/`ExNext`. Revisit (threading real
  permissions back through `fill_fib`) if a future corpus binary reads
  back what it just set.
- **`CreateDir`** (added to `crates/volamos-core/src/doslock.rs`):
  needed for `Copy ... ALL` (recursive directory copy) to create the
  destination directory tree. Reuses the existing `new_lock` machinery
  `Lock`/`DupLock`/`ParentDir` already share, resolving the parent via
  `ResolveMode::ParentMustExist` and failing with `ERROR_OBJECT_EXISTS`
  if the target already exists.
- **`SameLock`** (added to `crates/volamos-core/src/doslock.rs`):
  `LOCK_SAME`/`LOCK_SAME_VOLUME`/`LOCK_DIFFERENT` aren't defined
  anywhere in the RKRM skill's reference material (only described in
  words); their real numeric values (`0`/`1`/`-1` respectively) were
  confirmed against AmiBlitz3's public Amiga `dos.ab3`/`dos.h`
  translation on GitHub before implementing, rather than guessed --
  getting these wrong would have been a silent behavioral bug (the
  compiled guest binary compares against the real constant values, not
  our runtime's choice of them). Compares canonicalized host paths for
  `LOCK_SAME`, falling back to Amiga volume-name comparison for
  `LOCK_SAME_VOLUME`.

`Copy` now runs fully end-to-end against the real corpus binary, both
for a simple file copy and for `Copy ... ALL` (recursive directory
copy, exercising `CreateDir` + `SameLock` together):
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Copy WORK:deep WORK:subdir_copy ALL
   WORK:subdir_copy   [created]
        sub (Dir)   [created]
           d.txt..copied.
```
New tests: 3 (`dosprotect.rs`), 9 (`doslock.rs`, for `CreateDir` and
`SameLock`, including one end-to-end trap-dispatch test exercising
both together). Also fixed one now-stale pre-existing test
(`dispatch::tests::unknown_call_diagnostic_names_the_function_when_table_is_known`)
that had hardcoded `CreateDir` (-120) as an example of a permanently
unregistered LVO -- switched it to `Rename` (-78), still unimplemented.

**`Delete` gap chain — implemented 2026-08-19.** Moved to the next real
corpus binary (`C:/Delete`) once `Copy` ran cleanly; found and fixed
two gaps:

- **`GetDeviceProc`/`FreeDeviceProc`** (`crates/volamos-core/src/
  dosdevproc.rs`): real `GetDeviceProc` returns a `struct DevProc`
  whose `dvp_Port` is the live `MsgPort` of the handler process
  responsible for a path -- this runtime has no such processes (see
  `crate::execlist`'s "single-threaded" message-port scaffolding), so
  before implementing this there was a real risk `Delete` would go on
  to build and send a raw `DosPacket` to that port, which this runtime
  fundamentally can't answer. Implemented the *locking* half faithfully
  instead -- `dvp_Lock`, a `SHARED_LOCK` on the path's containing
  directory, confirmed as `GetDeviceProc`'s actual role for e.g.
  `CreateDir()` internally per the RKRM's own packet documentation --
  and left `dvp_Port` `NULL`, betting that `Delete`'s actual algorithm
  (like `Copy`'s) uses the higher-level `MatchFirst`/`MatchNext`/
  `DeleteFile` functions rather than raw packets. The bet paid off:
  running it confirmed `Delete` moves straight on to `MatchFirst` and
  never touches `dvp_Port` at all.
- **`DeleteFile`** (added to `crates/volamos-core/src/doslock.rs`):
  removes a file or empty directory; fails with
  `ERROR_DIRECTORY_NOT_EMPTY` for a non-empty one. This runtime never
  marks anything delete-protected (`fill_fib` always reports
  `fib_Protection == 0`), so unlike real `DeleteFile` there's no
  `ERROR_DELETE_PROTECTED` case here.

`Delete` now runs fully end-to-end against the real corpus binary:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Delete WORK:deleteme.txt
WORK:WORK:deleteme.txt  Deleted
```
(The doubled `WORK:WORK:` in its own status line is `Delete`'s own
message construction combining `NameFromLock` of the current directory
with the already-fully-qualified argument via `AddPart` -- `AddPart`
only resets to a device root on a *leading* colon in the appended
name, not one appearing mid-string, so this is a faithful reproduction
of what real `AddPart`'s documented algorithm would also produce given
the same already-qualified argument, not a bug introduced here.)

New tests: 8 (`dosdevproc.rs`: 4 unit-level for `get_device_proc`/
`free_device_proc` plus 3 end-to-end trap-dispatch, covering the
lock-and-wrap path, the missing-path failure, and the free/unlock
effect), 5 (`doslock.rs`, for `DeleteFile`, including one end-to-end
trap-dispatch test).

**`Dir` gap chain — implemented 2026-08-19.** Moved to the next real
corpus binary (`C:/Dir`) once `Delete` ran cleanly; found and fixed
three small gaps, all in `crates/volamos-core/src/dosfile.rs`:

- **`IsInteractive`**: reads `fh_Port` directly out of the guest
  `struct FileHandle` (offset 4 -- confirmed against the real struct
  layout already implicit in this runtime's existing
  `FH_ARG1_OFFSET = 36`, since both fields are part of the same
  11-`LONG` layout). `fh_Port` is now written non-zero only for the
  lazily-created `Input()`/`Output()` default handles (conceptually a
  console, always interactive); real host files opened via `Open()`
  leave it `0`, correctly non-interactive.
- **`SetMode`**: a no-op that always succeeds -- this runtime has no
  real `CON:`/`RAW:`/`AUX:` console handler for a buffer mode to apply
  to (`Input()`/`Output()` are backed directly by host stdin/stdout).
- **`WaitForChar`**: always reports "nothing available yet" rather
  than actually blocking (or attempting a non-blocking peek at host
  stdin, which isn't portably possible without more infrastructure).
  `Dir`'s own abort-on-keypress check during a long listing treats
  that as "no key was pressed" and carries on -- the correct behavior
  for this runtime's non-interactive/piped corpus-testing use.

`Dir` now runs fully end-to-end against the real corpus binary,
producing a columnar listing:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Dir WORK:
     deep (dir)
     destdir (dir)
     ...
  big.txt     copied.txt  empty.txt   hello.txt   hello2.txt  out.txt
```
(A leading `[0 q` in the captured output is `Dir`'s own ANSI/CSI
console escape sequence, meant to be silently interpreted by a real
terminal -- not a bug, just an artifact of capturing raw stdout instead
of connecting to an ANSI-aware terminal.)

New tests: 7, all in `dosfile.rs` (2 unit-level for the
interactive/non-interactive `fh_Port` distinction, 3 end-to-end
trap-dispatch for `IsInteractive`/`SetMode`/`WaitForChar`).

**`MakeDir`/`Rename` — 2026-08-19.** `MakeDir` already worked with no
new gaps (built entirely on `CreateDir`, from the `Copy` work above).
`Rename` needed the `Rename` LVO itself (added to `doslock.rs`,
mirroring `DeleteFile`'s shape: resolve both paths, fail with
`ERROR_OBJECT_EXISTS` if the target already exists, `std::fs::rename`)
-- but that surfaced a real, more interesting bug first.

**`dospattern.rs`'s tokenized-encoding redesign**: the real Workbench
3.1.4 `Rename` binary calls `ParsePattern` on its own source-name
argument, then passes *that same buffer's pointer* straight to
`Rename()`'s `oldName` parameter -- i.e. it reuses `ParsePattern`'s
output as a plain `STRPTR` when the name has no wildcard. This
runtime's original `dospattern.rs` (see `docs/plan.md`'s Phase 3 entry)
took the RKRM's "the byte encoding ... should be considered internal"
literally and used an arbitrary length-prefixed binary serialization
(`OP_LITERAL` + byte per character, `OP_SEQ` + count header, ...) --
which meant a plain 15-character path like `"WORK:hello2.txt"` encoded
to `04 0f 00 57 00 4f ...` (an `OP_SEQ` header, then each letter
individually prefixed by an `OP_LITERAL` tag byte) instead of the
literal text. `Rename` reading that back as a C string produced
garbage, and the rename silently failed with a spurious "Object not
found".

Root-caused via a temporary `eprintln!` dump of the raw bytes at the
argument pointer (no disassembler available, same technique as the
`Version`/`fib_FileName` investigations earlier). Once diagnosed, the
fix follows directly from real `ParsePattern`'s actual, empirically
necessary property: for a pattern with **no wildcards at all**, its
tokenized output must be byte-for-byte identical to the input (plus a
`NUL` terminator). Rewrote the encoding as a literal transliteration of
the original wildcard syntax instead of an opaque serialization:
ordinary characters pass through as themselves; only the wildcard
*operators* (`?`, `#`, `~`, `%`, `(`/`|`/`)`, `[`/`]`) become single
reserved bytes in the `0x80`-`0x9f` C1 control range, which (per the
RKRM's own `paths-and-filenames.md`) can never legally appear in a real
AmigaOS path character -- so the encoding needs no length prefixes
anywhere, self-terminating on a trailing `0x00` exactly like a normal
C string. A `#`/`~` prefix's inner atom needed one subtlety: since
`parse_group`'s single-branch collapse means `~(#?.info)` parses to
`Not(Seq([...]))` with no group wrapper left in the tree, a bare `Seq`
directly under `Not`/`Repeat` has to be *re*-wrapped in synthetic group
markers on encode (nothing else besides an explicit `(...)` could have
produced a multi-atom inner for a prefix operator, so this is the only
case needing it) -- found immediately by the pattern module's own
pre-existing round-trip test, which caught the regression before it
ever reached the corpus binary.

`Rename` now runs fully end-to-end against the real corpus binary:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Rename WORK:hello2.txt WORK:renamed.txt
$ ls <vol>/renamed.txt   # hello2.txt is gone, renamed.txt has its contents
```
New tests: 5 (`doslock.rs`, for `Rename`, including one end-to-end
trap-dispatch test), 1 (`dospattern.rs`, asserting the no-wildcard
byte-identical property directly, alongside the pre-existing
`encode_decode_round_trip` test that caught the `Not(Seq(...))`
regression during the rewrite itself).

**`Protect`/`Which` — 2026-08-19.** `Protect` already worked with no
new gaps (built entirely on `SetProtection`, from the `Copy` work
above) -- confirmed it actually flips the host file's real
writability. `Which` needed five small gaps in sequence, all reachable
only once the previous one was fixed:

- **`Cli`** (`dosfile.rs`): always returns `NULL` -- this runtime execs
  a guest binary directly, with no simulated Shell process wrapping it,
  so "the caller is not part of a shell" (real `Cli()`'s own documented
  return for that case, also true for programs launched from
  Workbench) is honestly correct here, not a missing feature.
- **`FindSegment`** (`dosseg.rs`): always reports "not found" -- this
  runtime has no list of resident segments (no `AddSegment`/`Resident`
  support), so nothing can ever be found.
- **`GetFileSysTask`/`SetFileSysTask`** (`dosfile.rs`): a fixed
  non-`NULL` `MsgPort*` sentinel, now backed by real mutable state
  (`DosState::current_file_sys_task`) so `SetFileSysTask` round-trips
  correctly -- unlike `GetDeviceProc`'s `dvp_Port` (deliberately `NULL`,
  see `crate::dosdevproc`'s module docs), real callers of
  `GetFileSysTask` never expect `NULL`, so a fixed non-zero sentinel is
  the correct choice here, not `0`.
- **`VFWritef`** (registered in `dosprintf.rs`): functionally identical
  to the already-implemented `VFPrintf` (same `D1`/`D2`/`D3` signature,
  same `RawDoFmt`-based formatting) -- confirmed via the RKRM's own
  `#define VWritef(format,argv) VFWritef(Output(),(format),(argv))`
  macro documentation, mirroring `WriteStr(s) = FPuts(Output(),s)`.
  Registered onto the same handler as `VFPrintf` rather than
  duplicating it.

`Which` now runs fully end-to-end against the real corpus binary, both
for a found command and a cleanly-reported "not found":
```
$ volamos -V "C:$HOME/amiga/wb314/full/C" ~/amiga/wb314/full/C/Which Which
C:Which
$ volamos -V "C:$HOME/amiga/wb314/full/C" ~/amiga/wb314/full/C/Which Nonexistent
[exit 5, no crash]
```
New tests: 4 (`dosfile.rs`, for `Cli`/`GetFileSysTask`/
`SetFileSysTask`), 1 (`dosseg.rs`, for `FindSegment`), 1 (`dosprintf.rs`,
end-to-end for `VFWritef` reached under its own LVO name rather than
via `VFPrintf`).

**`.uaem` metadata sidecars + `SetComment`/`Filenote` — 2026-08-19.**
Simon requested this directly, referencing the FS-UAE/Amiberry/
Copperline `.uaem` sidecar convention this project's own `~/src/
amisnap` tool already implements and interoperates with. Implemented
`crates/volamos-core/src/dosmeta.rs` (`read_sidecar`/`write_sidecar`,
the one-line `HSPARWED YYYY-MM-DD HH:MM:SS.CC comment` format) with the
byte-for-byte encoding taken directly from `~/src/amisnap/src/amiga/
applyuaem.c`/`tools/amisnap_reader.py` -- an independently-built
reference already checked against real captured Copperline output --
rather than re-derived from scratch, so sidecars this runtime writes
interoperate with those tools' own readers and vice versa. Unlike
`crate::dospattern`'s tokenized encoding (a genuinely private format
nothing outside this runtime ever reads), byte-compatibility here was
the entire point.

Wired into the existing pipeline:
- `crate::doslock`'s `fill_fib` now reads a target's `.uaem` sidecar
  (if any) for `fib_Protection`/`fib_Date`/`fib_Comment`, falling back
  to this runtime's original defaults (`0`, the AmigaOS epoch, no
  comment) when none exists -- required threading a new `host_path`
  parameter through `fill_fib` and `crate::dosanchor`'s
  `write_match_result` (4 call sites total across both modules).
  Confirmed this doesn't reintroduce the host-mtime non-determinism
  `crate::doslock`'s module docs deliberately avoid: a sidecar's date
  is explicit, checked-in data, not a live filesystem timestamp.
- `crate::dosprotect`'s `SetProtection` now also writes the *full*
  8-bit mask to the sidecar (merged onto whatever was already recorded
  there, so a prior comment survives), in addition to its existing
  real-`chmod`-on-`FIBB_WRITE` effect -- closing the "one-way, doesn't
  round-trip" gap that module's own docs flagged when first
  implemented for `Copy`.
- **`SetComment`** (new, `crates/volamos-core/src/dosnote.rs`, found
  missing running the real `C:/Filenote` binary): writes a comment to
  the sidecar, merged the same way.
- **Sidecar files are hidden from every directory-listing site**
  (`crate::doslock`'s `Examine`/`ExNext`, `crate::dosanchor`'s
  `MatchFirst`/`MatchNext`) via a new `dosmeta::is_sidecar_name`
  filter -- matching real FS-UAE/Amiberry behavior (a `.uaem` is a
  host-side implementation detail of the mount, not a real Amiga
  file); caught by Simon's own follow-up request to check `List`
  against a directory containing a commented file, which surfaced the
  sidecar showing up as a spurious extra directory entry.

**A second, independent bug found via the same `Filenote` run**: its
multi-word `COMMENT "test comment from Filenote"` argument arrived at
`SetComment` as the single word `"Filenote"` -- garbage, not a crash.
Root-caused to `crate::dispatch::Runtime::new`'s command-line
construction (`config.args.join(" ")`): this runtime's own CLI splits
host argv into separate elements *before* this join, so an argv
element that itself contains spaces (the host shell's quoting already
resolved) loses that boundary once rejoined with plain spaces --
`ReadArgs`, which parses this buffer as raw AmigaDOS-syntax command-
line text exactly like a real Shell prompt, then sees several separate
unquoted tokens instead of the one argument it actually is. Fixed with
a new `quote_arg_if_needed` (`dispatch.rs`) that re-wraps any argv
element containing whitespace/`;`/`=`/`"`/`*` (or that's empty) in
AmigaDOS double-quotes, escaping embedded `"`/`*`/newline exactly per
`crate::dosargs`'s own quoted-item decoder (`*"`/`**`/`*n`) -- the
overwhelming majority of ordinary single-word arguments are left
completely unaffected.

`Filenote` now runs fully end-to-end against the real corpus binary,
and `List`/`Dir` correctly display comments while hiding the sidecar
files themselves:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Filenote WORK:hello.txt COMMENT "test comment from Filenote"
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/List WORK:
...
hello.txt                     58 ----rwed 01-Jan-78 00:00:00
: test comment from Filenote
...
```
Also fixed a now-stale pre-existing test
(`dispatch::tests::unknown_call_diagnostic_names_the_function_when_table_is_known`)
that had hardcoded `SetComment` (-180) as an example of a permanently
unregistered LVO -- switched to `SetOwner` (-996), still unimplemented
(no host concept of an Amiga uid/gid to map it onto).

New tests: 9 (`dosmeta.rs`, including one parsing a real captured
`~/src/amisnap` fixture line verbatim), 2 (`dosprotect.rs`, sidecar
write + comment-preserving merge), 3 (`dosnote.rs`, including a
comment-preserving-protection merge test), 4 (`doslock.rs`, sidecar
metadata round-trip through `Examine` + sidecar-hiding), 1
(`dosanchor.rs`, sidecar-hiding through `MatchFirst`), 5 (`dispatch.rs`,
`quote_arg_if_needed` unit tests plus an end-to-end command-line
re-quoting test).

**`.uaem` sidecar lifecycle: `Rename`/`DeleteFile` — 2026-08-19.** Simon
asked directly whether the sidecar was tied into every implemented
comment/protection call; it was for the two *setters*
(`SetProtection`/`SetComment`), but not for the two calls that move or
remove the underlying object out from under it: `DeleteFile` left an
orphaned `.uaem` behind (harmless -- already hidden from listings -- but
a permanent disk-space leak), and `Rename` didn't move the sidecar at
all, so a renamed object silently reverted to default protection/no
comment and the old sidecar became an orphan under the stale name.

Fixed both in `crates/volamos-core/src/doslock.rs`: `delete_file` now
also removes `dosmeta::sidecar_path(&host_path)` after the primary
object is gone; `rename` now also renames the sidecar (if one exists)
to the new target's own sidecar path. Both are best-effort (a sidecar
operation failure doesn't fail the whole call, since the primary
file/directory operation -- the one with real `IoErr()` semantics --
already succeeded by that point) and both are silent no-ops when there
was no sidecar to begin with.

Confirmed against the real corpus binaries in sequence -- `Filenote`
(set a comment) → `Rename` (comment follows) → `List` (still shown on
the new name) → `Delete` (sidecar cleaned up, no orphan left):
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Filenote WORK:f.txt COMMENT "before rename"
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Rename WORK:f.txt WORK:renamed.txt
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/List WORK:renamed.txt
renamed.txt                    8 ----rwed 01-Jan-78 00:00:00
: before rename
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Delete WORK:renamed.txt
$ ls <vol>/renamed.txt*   # nothing -- no orphaned sidecar
```
New tests: 4 (`doslock.rs`: sidecar removed on delete, delete without a
sidecar still succeeds, sidecar relocated on rename, rename without a
sidecar still succeeds).

**Device list + `Info` — 2026-08-19.** Following "test info next", ran
the real `C:/Info` binary and worked through four successive gaps in
the established run→find→implement→verify loop, each its own new
module:

- `crates/volamos-core/src/dosdevlist.rs` — `LockDosList`/
  `NextDosEntry`/`FindDosEntry`/`UnLockDosList`. Only `LDF_VOLUMES`
  entries are ever materialized (this runtime has no separate device/
  assign `DosList` objects); struct `DosList` byte offsets (44 bytes
  total, `dol_misc` a 24-byte union) came from the AmiBlitz3
  `dosextens.ab3` include (RKRM prose doesn't give literal offsets),
  the same external source already used earlier this session for the
  `LOCK_SAME`-family constants. One footgun found and fixed along the
  way: the scan-start header node's `dol_Type` must be a real
  `DLT_PRIVATE` (`-1`) sentinel, not left zeroed -- a zeroed `dol_Type`
  reads as `DLT_DEVICE` (`0`) and would spuriously self-match a
  `FindDosEntry(header, NULL, LDF_DEVICES)` scan.
- `crates/volamos-core/src/dospkt.rs` — `DoPkt` (direct packet
  communication), needed because real `Info` bypasses the `Info()`
  library wrapper and sends `ACTION_DISK_INFO`/`ACTION_INFO`/
  `ACTION_IS_FILESYSTEM` packets straight to a volume's `dol_Task`.
  Answers all three for a known volume with a fixed, host-independent
  `InfoData` (100,000 512-byte blocks, `0` used) rather than querying
  real host free-space (no portable way to get that without a new
  dependency, and it would reintroduce host-state non-determinism this
  project has deliberately avoided elsewhere, e.g. `fib_Date`
  defaults). Any other packet type/unrecognized port fails cleanly
  with `ERROR_ACTION_NOT_KNOWN`, the real documented convention.
- Root-caused a non-obvious "`Object is not of required type`"
  failure (from real `PrintFault`, no crash) via a temporary debug
  trace: it wasn't `FindDosEntry` failing to match -- it was matching
  correctly, but real `Info` then rejects any `DosList` entry whose
  `dol_Task` is `NULL`, treating that as "not a live volume". Fixed by
  giving every volume entry `crate::dosfile::DEFAULT_FILE_SYS_TASK`
  (reused, doc comment extended) as a non-`NULL` sentinel `dol_Task` --
  never dereferenced, matching the existing `GetFileSysTask`/
  `GetDeviceProc` "no real handler processes" scope boundary. This in
  turn meant every volume needed a *distinct* task id (not one shared
  sentinel) so `DoPkt` could tell which volume a packet's `port`
  addressed: `DosState::volume_task_ids` + `dosdevlist::
  task_id_for_volume`/`volume_for_task_id`.
- Known cosmetic gap, documented and left as-is: real `Info`'s
  "Mounted disks" table (keyed by *device* unit, e.g. `DF0`) prints
  `Invalid/unknown` for a volume this runtime backs, since this
  runtime doesn't model device units distinct from volumes -- the
  "Volumes available" table (keyed by volume name, all `DoPkt`
  actually needs to answer) prints correctly and the command still
  exits `0`. Modeling fake per-volume device units was judged not
  worth it for a purely cosmetic gap.

Confirmed against the real corpus binary, both forms:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Info WORK:
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Info
Mounted disks:
Unit      Size       Used       Free Full Errs   Status   Name
WORK      Invalid/unknown

Volumes available:
WORK [Mounted]
$ echo $?
0
```
New tests: 13 (`dosdevlist.rs`: 8 unit + 2 e2e; `dospkt.rs`: 3 e2e,
including the unknown-port failure case).

**`Forbid`/`Permit`; unblock `Avail` — 2026-08-19.** Ran the real
`C:/Avail` binary next; its only gap was `exec.library`'s `Forbid`
(LVO -132)/`Permit` (-138), called around its walk of exec's
memory-pool list to protect against concurrent modification.
Implemented as true no-ops in `crates/volamos-core/src/exectask.rs`
(alongside the module's other task/signal primitives): this runtime
is single-threaded and never preempts the running guest task for
anything, so there is no critical section to protect and no
simplification involved -- a no-op *is* the correct behavior here,
not an approximation of it. `AvailMem` itself was already implemented
in Phase 3 (T16), so no further gaps surfaced.

Confirmed against the real corpus binary, both plain and with an
argument:
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Avail
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Avail FLUSH
Type  Available    In-Use   Maximum   Largest
chip     977624         0    977624    977624
fast     977624         0    977624    977624
total   1955248         0   1955248    977624
$ echo $?
0
```
New tests: 1 (`exectask.rs`: `forbid_then_permit_is_a_harmless_no_op`).

**`AssignLock`; unblock `Assign` — 2026-08-19.** `Assign` with no
arguments (list current volumes/assigns/devices) already worked with
no gaps -- it's built entirely out of `LockDosList`/`NextDosEntry`/
`UnLockDosList`, already implemented for `Info`. Its primary form,
`Assign NAME: TARGET`, needed one new call: `AssignLock` (`crates/
volamos-core/src/dosassign.rs`). Scoped to `AssignLock` only, not the
other four RKRM-documented assign functions (`AssignPath`/
`AssignLate`/`AssignAdd`/`RemAssignList`) -- non-binding/late-binding
assigns and multi-assign extension are real gaps, left as honest
unhandled-trap failures for a future corpus binary that needs them,
not silently stubbed.

Implementation deliberately doesn't build a real `DosList` entry (the
way `crate::dosdevlist::lock_dos_list` does for volumes) -- it reuses
this runtime's existing `Vfs` assign model directly (two new methods,
`Vfs::set_assign`/`remove_assign`, the same representation `-a`/
`--assign` on the CLI already produces), keyed on the Amiga path
string the target lock resolved to. This means an assign made with
`AssignLock` immediately works for every other path-resolving call in
the same process (confirmed with a direct `Vfs`-level test), which is
all a corpus binary needs -- but it does *not* show up in a later
`LockDosList(LDF_ASSIGNS)` walk, since `dosdevlist` only ever
materializes `LDF_VOLUMES` entries. A real, documented gap if a future
binary needs to *enumerate* assigns it just created, not just resolve
paths through them.

Confirmed against the real corpus binary, all three forms -- create,
cancel, and the "can't turn a volume into an assign" collision case
(`ERROR_OBJECT_EXISTS`, which real `Assign` renders as its own
`"Can't cancel %s"` client-side message):
```
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Assign FOO: WORK:libs
$ echo $?
0
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Assign FOO:
$ echo $?
0
$ volamos -V WORK:<vol> ~/amiga/wb314/full/C/Assign WORK: WORK:libs
Can't cancel WORK
$ echo $?
20
```
New tests: 5 (`dosassign.rs`: 2 e2e -- create, volume-name collision
-- + 3 unit tests against `Vfs` directly for set/remove/resolve).

**`StrToDate`/`OpenDevice`/`CloseDevice`/`SetFileDate`; `Date`/`SetDate`
— 2026-08-19.** Worked through both commands in the established
run→find→implement→verify loop.

- `Date` (no args, print current time) already worked -- built
  entirely on the pre-existing `DateToStr`. `Date DD-MMM-YY HH:MM:SS`
  (explicit args) needed two new pieces: `StrToDate` (`crates/
  volamos-core/src/dosdatestr.rs`, alongside `DateToStr` in the same
  module since they're the two directions of the same `DateTime`
  struct) -- the inverse of `DateToStr`'s existing format/weekday
  logic, supporting all four `FORMAT_DOS`/`INT`/`USA`/`CDN` styles plus
  relative words (`Today`/`Tomorrow`/`Yesterday`/weekday names, honored
  regardless of format, per the RKRM) -- and `exec.library`
  `OpenDevice`/`CloseDevice` (`crates/volamos-core/src/exectask.rs`),
  since real `Date` unconditionally opens `timer.device` at startup.
  This runtime has no real device drivers (same "no real handler
  processes" scope boundary as `crate::dospkt`'s `DoPkt`), so every
  device fails to open with the real, documented `IOERR_OPENFAIL` code
  -- not a stub pretending success. Confirmed via a temporary local
  experiment (faking a successful open) that a real, working
  `timer.device` is a genuine prerequisite for `Date`'s explicit-args
  form beyond just these three calls (it goes on to need real `DoIO`/
  `SendIO` timer-request handling and `utility.library`'s `UMult32`),
  so real device I/O stays out of scope and `Date` with explicit
  arguments is a known, documented gap: it fails cleanly with a real,
  correctly-formatted `"***Bad args"` usage message (exit 20) rather
  than crashing, while the no-argument form is fully correct.
- `SetDate` needed one new call, `SetFileDate` (`crates/volamos-core/
  src/dossetfiledate.rs`) -- closes a gap flagged (but not implemented)
  back when `.uaem` sidecars were first added: `date` was the only
  sidecar field with a reader (`crate::doslock`'s `fill_fib`) but no
  writer until now. `SetFileDate` itself is implemented correctly and
  verified both by direct unit/e2e tests and by confirming the real
  corpus binary actually writes the right date into a `.uaem` sidecar
  in practice. The real binary end-to-end run, however, still prints
  `"SetDate failed: Object is not of required type"` afterward (exit
  20) even though the date *was* set correctly -- root-caused via
  temporary debug tracing to a real, expected `ERROR_OBJECT_WRONG_TYPE`
  from `CurrentDir()` on a `DupLock()` of the *matched object itself*
  (real `SetDate`'s own algorithm: try `CurrentDir` into whatever
  `MatchFirst` found, tolerating a `ERROR_OBJECT_WRONG_TYPE` failure
  when the target is a plain file rather than a directory) combined
  with `IoErr()` never being reset before a later check reads that
  stale value. Every individual call involved (`DupLock`/`CurrentDir`/
  `SetFileDate`/`MatchFirst`/`MatchNext`/`IoErr`) matches its own
  documented per-function contract and is independently well-tested;
  this looks like an inherent quirk of the real `SetDate` binary's own
  end-to-end logic rather than a volamos bug, though confirming that
  fully would need a real-oracle comparison (out of scope here, see
  the `ReadArgs`/`ParsePattern` real-oracle-validation memory for the
  same caveat pattern). Documented as a known, non-crashing gap rather
  than "fixed", since there is nothing locally to fix.

New tests: 18 (`dosdatestr.rs`: 11 `parse_date_string`/
`parse_time_string`/`expand_two_digit_year` unit tests + 2 e2e;
`exectask.rs`: 2 e2e for `OpenDevice`/`CloseDevice`;
`dossetfiledate.rs`: 3 e2e).

**`Wait`/`Break` — 2026-08-19.** Worked through both in the established
loop.

- `Wait`'s template is real AmigaDOS syntax this project's `ReadArgs`
  hadn't seen before: `"/N,SEC=SECS/S,MIN=MINS/S,UNTIL/K,FILE=DIR/K"`
  -- the first item has an *empty* name (just `/N`, no name before the
  slash), which is legal (an anonymous, positional-only argument, real
  syntax used by several real AmigaDOS commands). `crates/volamos-core/
  src/dosargs.rs`'s `parse_template` was rejecting this outright
  (`ERROR_BAD_TEMPLATE`) -- fixed to allow an empty name, producing a
  `TemplateArg` with no keyword synonyms (it can only ever be filled
  positionally, which `matches_keyword` already handled correctly for
  free once the empty-names vector reached it). Once that parsed, `Wait
  1` needed two more calls: `utility.library`'s `SMult32` (Wait uses it
  to convert seconds/minutes into ticks) and `dos.library`'s `Delay`.
  Implemented all four `utility.library` 32-bit math primitives
  together while at it (`SMult32`/`UMult32`/`SDivMod32`/`UDivMod32`,
  `crates/volamos-core/src/utility.rs`) -- confirmed exact register
  convention (`D0`/`D1` in, `D0` [`D0`:`D1` for div/mod] out, truncated
  32-bit results not 64-bit) against the real NDK Autodoc text rather
  than guessing. `Delay` (`crates/volamos-core/src/dosdate.rs`,
  alongside `DateStamp`) is a genuine, faithfully-implementable
  `std::thread::sleep` -- unlike the `exec.library` device-I/O
  primitives that stay unimplemented (`crate::exectask`'s `OpenDevice`/
  `CloseDevice`), `Delay`'s contract is simple and fully specified, so
  there's no scope reason to fake it. `Wait 1`/`Wait 1 SEC` both now
  really block for ~1 real second and exit `0`.
- `Break` needed one call, `MaxCli` (`crates/volamos-core/src/
  dosfile.rs`, next to the existing `Cli` handler) -- returns `0`
  (empty CLI-process table), consistent with `Cli()`'s own existing
  choice to report "not running in a shell" (this runtime never
  simulates a `CommandLineInterface`/process table at all). `Break 1`
  now correctly reports `"Process 1 does not exist"` (exit 20) instead
  of crashing.

New tests: 9 (`dosargs.rs`: 2 for the empty-name template item;
`utility.rs`: 5 for the four math primitives; `dosdate.rs`: 1 for
`Delay`; `dosfile.rs`: 1 for `MaxCli`).

**Loader: `HUNK_DREL32` support — 2026-08-19.** For a change of pace,
tried a real third-party tool instead of a `C:` command: `PhxAss`
4.40, a real Amiga macro assembler, downloaded from its actual Aminet
package (`dev/asm/PhxAss.lha`, freeware, unlike the Workbench `C:`
corpus this isn't Commodore/Hyperion-copyrighted so no vendoring
concern either way -- still not committed to the repo, same opt-in
local-file treatment). Its own executable failed to load at all:
`crates/volamos-core/src/loader.rs`'s hunk parser didn't recognize
block type `0x3F7`.

That's `HUNK_DREL32` -- despite the name suggesting a self-relative
("data-relative") fixup, confirmed via
<https://amiga-dev.wikidot.com/file-format:hunk> that the real AmigaOS
ROM loader treats it identically to `HUNK_RELOC32SHORT`: the same
*absolute* `mem[loc] += target_hunk_addr` arithmetic as
`HUNK_RELOC32`, just a more compact on-disk encoding (`uint16`
count/hunk-number/offset fields instead of `HUNK_RELOC32`'s `uint32`,
realigned to a 4-byte boundary afterward since an odd-length list
leaves the read position mid-longword). Note this correction: the
first implementation attempt assumed real PC-relative subtraction
arithmetic and a same-width-as-RELOC32 list format, both wrong --
worth remembering that "DREL" is a misleading name for what's actually
implemented, if this comes up again for `HUNK_DREL16`/`HUNK_DREL8`
(not yet needed by any corpus binary).

With that fixed, `PhxAss` now loads and actually runs real code:
`OpenLibrary`s dos.library/utility.library/three math libraries
(`mathtrans`/`mathieeedoubbas`/`mathieeedoubtrans`, all auto-created
fake handles per this runtime's existing "unknown library" fallback)
and `locale.library`, then fails cleanly the first time it makes a
real call into `locale.library` (LVO -156) -- an entire subsystem this
project has never modeled (out of the established `dos.library`/
`exec.library`/`utility.library` scope), not a crash. A reasonable
stopping point for this detour; `locale.library` support would be a
much larger, separate scope decision if ever pursued.

New tests: 1 (`loader.rs`:
`inter_hunk_drel32_applies_like_reloc32_despite_the_name`, including
the alignment-padding edge case).

**`OpenLibrary`: fail for missing disk-based libraries — 2026-08-19.**
Simon raised a real design flaw in the "vamos escape hatch" fake-library
behavior (`crates/volamos-core/src/dispatch.rs`'s `open_library_common`):
auto-creating a fake base and always succeeding was ported from vamos
verbatim, but it doesn't hold for **disk-based** libraries the way it
does for **ROM-resident** ones. `exec.library`/`dos.library`/
`utility.library` (everything this runtime actually implements) are
unconditionally present on any real Kickstart -- always-succeed is
correct there. Everything else (`locale.library`, `mathtrans.library`,
...) is loaded from `LIBS:<name>` at `OpenLibrary` time on real
AmigaOS, and only succeeds if that file is actually present on the
disk being booted; real `OpenLibrary` genuinely fails (`NULL`) for a
disk that doesn't have it. Many real programs (`PhxAss` included, per
this session's own detour) `OpenLibrary` such libraries speculatively
and gracefully disable the corresponding feature if the open fails --
that's the normal, well-supported AmigaOS idiom, not an edge case, so
always-succeeding was actively hiding it and forcing every such
program down the "library did open" code path this runtime can't
actually service.

Fixed: unknown-library `OpenLibrary` now checks whether
`LIBS:<name>` resolves on the configured `Vfs` first. Found on disk
(or a real system genuinely can't tell without loading and running
arbitrary disk library code, which this runtime doesn't implement) ->
same fake-base-then-trap-on-first-real-call behavior as before. Not
found (including "no `Vfs` configured at all", matching every other
path-based call's established convention) -> `OpenLibrary` returns
`NULL`, same as real AmigaOS with that library missing.

Confirmed against the real `PhxAss` run from the `HUNK_DREL32` fix
above: previously it hit the fake-lib trap on its first genuine
`locale.library` call; now `OpenLibrary("mathtrans.library")` and
`OpenLibrary("locale.library")` both cleanly return `NULL` (neither
file exists in the scratch test volume's `LIBS:`), `PhxAss` degrades
gracefully exactly as real AmigaOS software is designed to, and gets
substantially further -- through `ReadArgs`, `Lock`/`Examine`, and
actually opening the real `WORK:hello.asm` source file -- before
hitting its next real gap (`dos.library`'s `NameFromFH`, an honest,
specific "implement this next" pointer, not a generic fake-lib wall).

New tests: 2 (`dispatch.rs`: found-on-disk succeeds as before,
not-found-on-disk/no-`Vfs` both return `NULL`).

**Continuing the `PhxAss` run: `NameFromFH`, `CreateIORequest`/
`DeleteIORequest`, `timer.device`, real hardware exception delivery,
standard Workbench math libraries, and a `PcOutOfBounds` diagnostic —
2026-08-19.** Picked the `PhxAss` gap-chain back up (Simon: "keep going
with phxass as it's one of the more commonly used assemblers").

- `dos.library`'s `NameFromFH` (LVO -154): constructs the absolute
  Amiga path for an open file handle, same truncation/
  `ERROR_LINE_TOO_LONG` contract as the existing `NameFromLock`.
- `exec.library`'s `CreateIORequest`/`DeleteIORequest`: allocate/free a
  zeroed `IORequest`-shaped block with `mn_ReplyPort`/`mn_Length`/
  `ln_Type=NT_MESSAGE` pre-filled.
- `timer.device`: the *one* Exec device this runtime backs for real
  (`OpenDevice`/`DoIO`/`SendIO`/`WaitIO`/`CheckIO`/`AbortIO`), unlike
  every other device (no real drivers modeled, so those still fail
  `OpenDevice` with `IOERR_OPENFAIL`) -- `TR_GETSYSTIME`/`TR_SETSYSTIME`/
  `TR_ADDREQUEST` have simple, fully host-implementable semantics (wall
  clock read + real `std::thread::sleep`). `struct timerequest` = a
  32-byte `IORequest` + `struct timeval` at offset 32/36; command
  numbers confirmed against AmiBlitz3's real includes
  (`TR_ADDREQUEST=9`, `TR_GETSYSTIME=10`, `TR_SETSYSTIME=11`).
- **Real M68K hardware exception delivery** (Simon: "take it on, sounds
  like it's worth doing," after `PhxAss` hit a genuine F-line/FPU-probe
  CPU exception past `DoIO`): `Cpu::take_hardware_exception` reads the
  guest's real exception vector table (`vector*4`, `VBR=0` for our
  M68000 config) and, if the guest installed a handler, pushes a real
  `[SR,PC]` frame and jumps to it -- exactly what real hardware does for
  F-line/illegal/BKPT/`TRAP #n`, distinct from this runtime's own
  A-line library-dispatch convention. Non-obvious hardware semantic:
  for F-line/illegal/BKPT (unlike `TRAP #n`), the CPU stacks the
  *trapping* instruction's own address, not the next one -- a
  well-behaved guest handler must advance the stacked PC itself before
  `RTE`, or it re-traps forever (confirmed the hard way: an early test
  handler infinite-looped until this was accounted for). Required
  fixing a bug this uncovered: `Runtime::new`'s low-memory trap-table
  prefill covered `[0x0000, 0x1200)`, which clobbered the real vector
  table's own address range (`vector*4`, all `<= 0xC0` through `TRAP
  #15`) with the "unknown call" sentinel *before* any real handler could
  ever be seen as installed -- excluded via a new
  `EXCEPTION_VECTOR_TABLE_SIZE = 0xC0` gap.
- **Standard Workbench 3.1 math libraries** (Simon: "it's strange, phxass
  doesn't list an fpu as a required feature" / "it sounds like the maths
  libraries could be worth implementing soon"): `mathtrans.library`,
  `mathieeesingbas.library`, `mathieeesingtrans.library`,
  `mathieeedoubbas.library`, `mathieeedoubtrans.library` are disk-based
  on real hardware but ship as a *mandatory* part of every Workbench 3.1
  install (unlike a genuinely optional third-party library) -- these
  math libraries are software-only (no physical FPU required to use
  them, resolving Simon's "doesn't list an FPU" observation: `PhxAss`
  needs the math *library*, not the coprocessor). `open_library_common`
  now treats this small allowlist (`STANDARD_WORKBENCH_LIBRARIES`) like
  the ROM-resident set: always succeeds, regardless of `Vfs` state. Also
  fixed the fake-library auto-create path to write a real `struct
  Library` header (`lib_Node.ln_Type`/`lib_Version`/`lib_Revision`) at
  the base's *positive* offsets, not just the negative-offset jump
  table -- `PhxAss` reads a header field before deciding how to
  dispatch through a library, and previously got whatever garbage
  happened to sit in the next heap allocation.
- **`Cpu::run`/`StopReason::PcOutOfBounds`**: found via a genuinely
  confusing failure mode while debugging the above -- a guest bug (a
  `JSR`/`JMP` through a bad address register still not root-caused, see
  below) sent PC to a huge out-of-range address. `AddressSpace`'s
  documented "out-of-range reads return `0`" convention meant the CPU
  silently decoded an endless stream of zero-word instructions and
  walked forward (`u32` wraparound included) for *thousands* of steps
  before coincidentally landing back on a real trap-table sentinel,
  producing an error that named a plausible-looking but totally wrong
  address. `Cpu::run`'s default loop now checks `pc >= mem.len()` before
  every step and reports the real faulting address immediately instead.
  General robustness fix, not `PhxAss`-specific -- any guest bug that
  computes a bad jump target now gets a useful diagnostic.

**Still open**: with all of the above, `PhxAss` gets through Pass 1 and
Pass 2 (prints both banners, calls `DoIO` for timing twice) and then
hits `PcOutOfBounds` at `0xFFFFFFD1`. Root cause traced (via targeted,
since-removed `eprintln!` instrumentation, not committed) to a `JSR
(d16,A6)` at guest address `0x3068` where `A6` was loaded from
`+0x14` of an internal `PhxAss` object (itself reached via a
frame-relative local at `+0x15C(A5)`) and came out as the literal value
`1` instead of a real pointer -- i.e. a `PhxAss`-internal data
structure this runtime doesn't populate correctly ends up holding a
small integer where a jump target (plausibly a math-library dispatch
vtable/selector) is expected. Not yet root-caused further: doing so
without symbols likely needs either real disassembly of `PhxAss`
itself or a side-by-side ground-truth comparison against a real
Kickstart/Workbench run (e.g. via the local Amiberry MCP tooling) to
see what that field should actually contain. Left as the next gap in
this chain.

New tests: 7 (`dosfile.rs`: `NameFromFH`; `execlist.rs`:
`CreateIORequest`/`DeleteIORequest`; `exectask.rs`: `timer.device`'s
`OpenDevice`/`DoIO`/`SendIO`/`WaitIO`/`CheckIO`/`AbortIO`, 7 cases;
`dispatch.rs`: F-line trap routed through a guest handler, F-line trap
with no handler installed, standard-Workbench-library
`OpenLibrary`-always-succeeds-with-real-header; `backend.rs`:
`PcOutOfBounds` reporting).

**Real `mathtrans.library`/`mathieeedoubbas.library`/
`mathieeedoubtrans.library` implementations — 2026-08-19.** Simon: "it
sounds like the maths libraries could be worth implementing soon."
Replaced the fake-trap stand-ins for these three libraries (still opened
via the `STANDARD_WORKBENCH_LIBRARIES` allowlist above for
`mathieeesingbas.library`/`mathieeesingtrans.library`, not yet given
real implementations) with genuine, real library bases backed by actual
math semantics, following the same `register_real`/`write_library_node`
pattern `dos.library`/`exec.library`/`utility.library` already use.

- **LVO tables**: extracted from AROS's own `workbench/libs/{mathtrans,
  mathieeedoubbas,mathieeedoubtrans}/*.conf` via `tools/gen_lvos.py`
  (same uncopyrightable-facts-only extraction as the existing
  `dos.rs`/`exec.rs`/`utility.rs` tables — see the "fd/SFD metadata
  decision" section). Required two small `gen_lvos.py` fixes: these
  `.conf` files omit the `OpenLib`/`CloseLib`/`.skip 2` preamble
  `dos.conf`/`exec.conf` spell out explicitly (needed `--start-bias
  24`, already documented in the script's own module doc), and their
  register lists include `double`-sized arguments spanning two
  registers (`D0/D1`), written `/`-joined in the source — the script's
  register parser only understood a flat comma-separated list, so it
  gained support for flattening `/`-joined groups in call order,
  plus an import-pruning fix (don't emit `use ...AddressRegister`
  for a table that never uses one, to keep `cargo clippy` clean).
  Every table's LVOs were cross-checked against independently published
  values (<https://anadoxin.org/blog/amigaos-stdlib-vector-tables.html/>)
  before trusting `--start-bias 24`: `SPAtan`/`IEEEDPFlt`/`IEEEDPFieee`/
  `SPFieee` all matched exactly. (First attempt at the `--sanity` flags
  for two of the three tables used guessed-not-verified LVO values for
  entries the independent source didn't cover, causing the generated
  tests to fail immediately — a useful, cheap catch of the mistake
  before it could reach a committed file; corrected against the
  tool's own (by-then-validated) derivation instead of guessing again.)
- **`crates/volamos-core/src/mathlibs.rs`** (new module): `double`
  arguments/results pack into a register pair per each library's real
  `.conf` (`D0`/`D1` for the first, `D2`/`D3` for a second); implemented
  `IEEEDPFix`/`Flt`/`Cmp`/`Tst`/`Abs`/`Neg`/`Add`/`Sub`/`Mul`/`Div`/
  `Floor`/`Ceil` (mathieeedoubbas) and `Atan`/`Sin`/`Cos`/`Tan`/
  `Sincos`/`Sinh`/`Cosh`/`Tanh`/`Exp`/`Log`/`Pow`/`Sqrt`/`Tieee`/
  `Fieee`/`Asin`/`Acos`/`Log10` (mathieeedoubtrans) directly on Rust
  `f64`, no format-conversion risk. `mathtrans.library` predates IEEE
  double support and operates on AmigaOS's own 32-bit FFP (Fast
  Floating Point) encoding instead: 1 sign bit, 7-bit excess-64
  exponent, 24-bit normalized `[0.5,1)` mantissa fraction (confirmed via
  <https://wiki.amigaos.net/wiki/Math_Libraries>). `ffp_to_f32`/
  `f32_to_ffp` convert by re-deriving the shared bit pattern from IEEE
  single precision's own `1.mantissa * 2^exp` layout (IEEE's 24-bit
  significand -- implicit leading 1 plus 23 explicit mantissa bits --
  turns out to be *exactly* FFP's 24-bit mantissa field, just
  interpreted as a different fixed-point format, so the exponent
  translation is a plain additive bias, `stored_field = raw_exp - 62`)
  rather than a `log2`/`powi` round trip, avoiding float-precision edge
  cases at power-of-two boundaries; unit-tested both by round-trip and
  against one independently hand-derived encoding (`1.0`). FFP's 7-bit
  exponent field has real, documented less range than IEEE single's
  8-bit one -- out-of-range results saturate rather than panic/wrap.
  Every `SP*` transcendental reduces to "convert FFP in, call the `f32`
  method, convert back" once that conversion is right.
- **New base addresses**: `MATHTRANS_LIBRARY_BASE`/
  `MATHIEEEDOUBBAS_LIBRARY_BASE`/`MATHIEEEDOUBTRANS_LIBRARY_BASE`
  needed real jump-table + `struct Library` header room the original
  `[0x0000,0x1200)` reserved region didn't have spare (the existing
  four real bases already use most of the gaps between each other).
  Grew `crate::backend::TRAP_TABLE_SIZE` from `0x1200` to `0x1800`
  (three new `0x200`-byte chunks, same "prefilled with the unknown-call
  sentinel until a real header/handler overwrites its own portion"
  treatment as the rest of the region) rather than trying to
  shoehorn them into the existing gaps.
- **Verified against the real `PhxAss` run**: `OpenLibrary` for all
  three now reports `(real)` instead of `(fake, unimplemented)`, with
  real, distinct base addresses. Not a fix for the still-open
  `PcOutOfBounds` crash from the previous entry, though -- that crash
  happens before any actual math LVO is ever called (confirmed: no
  math-library call appears in the trace before it), so it's
  independent of this work, exactly as the previous entry's "root cause
  ... needs real disassembly or ground-truth comparison" already
  anticipated. Real math semantics for these libraries stands on its
  own merits (any future corpus binary doing real floating-point work
  now gets correct answers instead of an "unimplemented" trap) and was
  explicitly requested regardless of whether it moved this specific
  crash.

New tests: 15 (`lvos/mathtrans.rs`/`mathieeedoubbas.rs`/
`mathieeedoubtrans.rs`: one `known_lvos_match_amigaos` sanity test each,
generated; `mathlibs.rs`: 3 FFP conversion tests -- round-trip over a
representative value set, exact-zero, and one independently-derived
bit-pattern check).

**Real hardware ground-truth via Copperline, and `ENV:`-backed
`GetVar`/`SetVar`/`DeleteVar` — 2026-08-19.** Simon: "Do it" (root-cause
the still-open `PcOutOfBounds` crash via real hardware). Got a genuine
ground-truth comparison working: Copperline (`copperline --run`, real
Kickstart 3.1 r40.68) plus `m68k-amigaos-gdb`/the Copperline control
protocol (CCP) for scripted register/memory inspection. Confirmed
`ExecBase`, computed `PhxAss`'s real code addresses from its own hunk 0
size (contiguous within one real allocation, so entry-relative offsets
transfer directly -- volamos's loader lays hunks out contiguously too,
`crates/volamos-core/src/loader.rs`'s `load` doc already says so), and
set a real breakpoint at the exact real address matching volamos's
crash site.

**Result: `PhxAss` runs to completion cleanly on real hardware** --
"Pass 1", "Pass 2", "00 No errors.", back at the CLI prompt, no
crash at all. Confirms the bug is volamos-side, not a `PhxAss` bug or
something needing unusual real-hardware setup.

Getting there needed unwinding a real environmental obstacle first: the
`--run` warp-boot path's minimal generated `Startup-Sequence` doesn't
assign `ENV:`, and something in the booted system (Shell/CLI startup,
before `PhxAss` itself even loads) blocks on a real "Please insert
volume ENV: in any drive" Intuition requester -- reproducible, and
confirmed via `capture.screenshot`, not a guess. Dismissing it
interactively (mouse click-through via CCP's `input.mouse_to`/
`input.mouse`) proved unreliable; the practical fix was mounting a
host directory as the `ENV:` volume directly in Copperline's own config
(`[[filesys]] volume = "ENV"`), which sidesteps the requester by making
`ENV:` genuinely resolve.

That environmental fix suggested a real, previously-undiscovered
volamos gap: `dosvar.rs`'s `GetVar`/`SetVar`/`DeleteVar` explicitly
never had any `ENV:`-backed storage at all (documented as an accepted
simplification since T12/T17-era work -- "most real `C:` commands... use
plain local variables, not global ones"). Simon: "we have a few
options... simply map the ENV: volume to a directory... feels like the
better choice" (matching vamos's own convention) -- implemented exactly
that. `GVF_GLOBAL_ONLY` variables are now real files under whatever
host directory the guest's `ENV:` assign points at (via the same `Vfs`
mechanism `Open`/`Lock` already use), one file per variable, case
preserved on creation and matched case-insensitively on lookup, same as
every other `Vfs` path. `GetVar` without `GVF_LOCAL_ONLY`/
`GVF_GLOBAL_ONLY` now correctly searches local first and falls back to
global (added `GVF_LOCAL_ONLY`, previously missing entirely), matching
real `GetVar`'s documented search order; `SetVar`/`DeleteVar` still pick
one scope by `GVF_GLOBAL_ONLY` with no fallback, matching real
semantics. No `ENV:` assign configured (no `Vfs`, or one without an
`ENV:` volume) still fails cleanly with `ERROR_OBJECT_NOT_FOUND` --
same as before, no regression, and exactly what a well-behaved caller
already has to handle (Simon, correctly skeptical of the initial
hypothesis: "normally if an env var isn't found programs continue on
with their defaults").

**That skepticism was right**: re-running `PhxAss` against volamos with
a real (empty) `-V ENV:hostdir` still hits the identical crash at the
identical spot. `GetVar` returning "not found" was never going to
change `PhxAss`'s behavior whether it came from the old hardcoded path
or a real, empty directory -- same outcome either way. So `ENV:`
backing is a real, independently valuable, well-tested fix (matches
real semantics, useful for any future corpus binary reading shell/global
variables), but it does **not** explain the `PcOutOfBounds` crash.
Decided (Simon): commit the `ENV:` work now; leave the crash itself
open for a future session rather than continuing to chase it here.

New tests: 6 (`dosvar.rs`: global set/get round-trip through a real
file, global delete removes the real file, `GetVar` falls back to
global when not found locally, `GetVar` prefers local over global when
both exist, `GetVar` with `GVF_LOCAL_ONLY` does not fall back, and the
existing no-`Vfs` "global still fails cleanly" case kept/renamed to
clarify it's specifically the no-`Vfs` case).

**`PcOutOfBounds` crash root-caused and fixed: `timer.device`'s
library-style vectors — 2026-08-19.** A review of the accumulated
evidence found the root cause without any further tracing -- the crash
site's own field offset was the give-away, matching Simon's
wrongly-initialized-field hypothesis exactly. The crashing instruction
sequence (captured earlier) was:

```
0x305a  movea.l (0x15C,a5),a0   ; a0 = the timerequest (a global)
0x305e  movea.l (0x14,a0),a6    ; a6 = io_Device (offset 20!)
0x3068  jsr     (-48,a6)        ; SubTime(TimerBase)
```

Offset `0x14` = 20 = `struct IORequest.io_Device` -- the exact field
`open_device_handler` filled with the placeholder
`TIMER_DEVICE_SENTINEL: u32 = 1`. `PhxAss` was executing the
RKRM-documented idiom, verbatim from the Devices book's Timer chapter
(`TimerBase = (struct Library *)TimerIO->tr_node.io_Device;`):
timer.device doubles as a *library*, its time-arithmetic functions
(`AddTime -42`/`SubTime -48`/`CmpTime -54`/`ReadEClock -60`/
`GetSysTime -66`, past the six standard device vectors; order confirmed
against AROS's own `rom/timer/timer.conf`) called via LVOs off
`io_Device`. `jsr -48(1)` = jump to `0xFFFFFFD1` -- the reported
`PcOutOfBounds` address, matching to the byte. The purpose also
matches: `PhxAss` diffs two `TR_GETSYSTIME` readings with `SubTime` for
its "N lines in X sec" stats line -- the very line real hardware
printed right after "Pass 2", exactly where volamos crashed instead.

Fixed with the same real-base pattern as the math libraries: a new
`TIMER_DEVICE_BASE` (`0x19B0`; `TRAP_TABLE_SIZE` grown `0x1800` ->
`0x1A00` for its chunk), a real `struct Library` header with `ln_Type`
= `NT_DEVICE` (not `NT_LIBRARY` -- a device base is a `struct Device`),
all five vectors registered as real handlers (`AddTime`/`SubTime` with
proper micro carry/borrow; `CmpTime`'s documented inverted-looking
convention -- `-1` when the *first* operand is later -- confirmed
against the RKRM chapter's own worked example; `GetSysTime` sharing the
`TR_GETSYSTIME` clock via a new `host_time_secs_micro` helper;
`ReadEClock` returning the PAL rate 709379 Hz in `D0` and a
64-bit tick count derived from the same wall clock, which is correct
for the only documented use -- differences between readings), and
`OpenDevice` writing this real base into `io_Device` where the
sentinel used to go.

**Result: `PhxAss` now runs end-to-end under volamos** -- exit 0,
output identical to the real-Kickstart-3.1 ground-truth run including
the stats line, and the assembled output file is a valid hunk
executable (`moveq #0,d0; rts`) that volamos itself then loads and runs
cleanly. The full loop -- volamos runs a real assembler, which produces
a real program, which volamos runs -- closes for the first time.

New tests: 3 (`exectask.rs`: the full `OpenDevice` -> `movea.l
(0x14,a0),a6` -> `jsr -48(a6)` TimerBase idiom, byte-for-byte the
`PhxAss` shape, with `SubTime`'s borrow path and `CmpTime`'s `-1`
convention asserted; `AddTime`'s micro-carry path; `GetSysTime`/
`ReadEClock` plausibility including a 64-bit-truncation guard on
`ev_hi`).

**Configurable CPU model/FPU: `--cpu`/`--fpu` — 2026-08-19.** Simon
asked how volamos ended up defaulting to a plain M68000, and how easy
configurability would be. Answer to the first: not a deliberated
choice — Phase 1's original scaffold default, already flagged in
`backend.rs`'s own doc comment as "later stages can expose a way to
pick a different `CpuType` if that's ever needed." The `m68k` crate
already carries everything needed (`CpuType::{M68000..M68060,
SCC68070}` via `set_cpu_type`, plus a public `fpu_present: bool`), so
this was genuinely small: `M68kCpu::new()` (still the M68000/no-FPU
default every existing test relies on) now delegates to a new
`M68kCpu::with_config(cpu_type, fpu_present)`; the CLI gained `--cpu
MODEL` (parses all eleven real models) and `--fpu`/`--no-fpu`
(default: no FPU), threaded through both the top-level `Runtime` and
`run_nested_program` (`System()`/`Execute()` reuses the parent run's
CPU config, same convention as `--stack`).

One real fact surfaced while implementing this, confirmed empirically
rather than assumed: the `m68k` crate models pre-68020 CPUs as having
no coprocessor interface *at all* (`has_coproc_interface =
!cpu.is_pre_68020` in its own decode logic) -- so `fpu_present` is a
no-op below `--cpu 68020`; F-line always traps regardless, matching
real 68000/68010 hardware. This is also why the F-line hardware
exception delivery work earlier in this project's history (the real
`Cpu::take_hardware_exception` plumbing, exercised by `PhxAss`'s own
FPU probe) was correct without ever having to think about
`fpu_present` explicitly -- volamos's hardcoded `CpuType::M68000`
always took that path regardless of the field's (previously untouched,
crate-default-`true`) value. Verified with three new unit tests
constructing the exact same coprocessor-ID-1 F-line opcode word across
`(M68000, fpu=true)`, `(M68020, fpu=false)`, and `(M68020, fpu=true)`
-- the first two trap, the third doesn't.

New tests: 9 (`backend.rs`: the three `with_config` FPU-trap-boundary
cases above; `main.rs`: default is 68000/no-FPU, `--cpu` parses every
documented model, unknown model is a clean error, missing value is a
clean error, `--fpu` sets it, last-flag-wins for `--fpu`/`--no-fpu`).

**First real `C:` command run against the new `--cpu`/`--fpu` flags:
`AttnFlags`, `CacheControl`, `Supervisor` — 2026-08-19.** Simon: "that
gives an obvious command to test next, the cpu command" -- the real
Workbench 3.1.4 `C:CPU` (`~/amiga/wb314/full/C/CPU`, the same
established empirical corpus disk), which reports/sets processor and
cache state and is the most direct real-binary exercise of the CPU/FPU
configurability just added. Three gaps found and fixed in the same
"run -> gap -> implement -> verify" loop this project already follows,
each confirmed against the real binary's actual printed output, not
just "it doesn't crash":

- **`ExecBase.AttnFlags`** (new `EXEC_BASE_ATTNFLAGS_OFFSET = 296`,
  same field-by-field NDK derivation as `EXEC_BASE_LIBLIST_OFFSET`):
  the real, documented way guest code detects the installed CPU/FPU
  (`AFF_68010`/`AFF_68020`/`AFF_68030`/`AFF_68040`/`AFF_68881`/
  `AFF_68882`/`AFF_FPU40`/`AFF_68060`, bit positions confirmed against
  a primary NDK `execbase.h` source, not guessed) -- there's no
  library call for this, `AttnFlags` itself *is* the interface. New
  `StartConfig.attn_flags: u16` field (defaults to `0`, i.e. unchanged
  behavior for every existing caller); the CLI's new `attn_flags_for`
  computes it cumulatively from `--cpu`/`--fpu` (each model's bit is
  documented as "also set for" every later model, e.g. a real 68040
  reports `AFF_68010`/`AFF_68020`/`AFF_68030`/`AFF_68040` together).
  Verified directly against the real command's own report format
  (`"System: 68030 68882 ..."`, `"System: 68040 68040/060-FPU ..."`)
  across every `--cpu` value -- exact match to the documented example
  shape.
- **`exec.library`'s `CacheControl`** (LVO -648, `crate::execmem`):
  `D0`/`D1` = `cacheBits`/`cacheMask`, `D0` returns the *previous*
  `CACRF_*` state; `cacheMask == 0` is a pure query. This runtime
  doesn't model a real cache, so it's pure bookkeeping -- the bits
  live in guest memory at a new `CACHE_BITS_ADDR = 0x00C0` (the first
  4 bytes of the "unused headroom" the reserved-region memory map
  already documented), single source of truth, no host-side mirror,
  same convention `crate::exectask` established for task/signal state.
  Seeded to an "everything enabled" default (`CACRF_EnableI`/`IBE`/
  `EnableD`/`DBE`/`EnableE`/`CopyBack`/`WriteAllocate`, bit values also
  confirmed against a primary source) at `Runtime::new` time.
- **`exec.library`'s `Supervisor`** (LVO -30, `crate::execfmt`, next to
  `RawDoFmt`'s existing "step the CPU mid-handler" `PutChProc`
  machinery, for the same underlying reason): runs a guest routine
  synchronously and returns its `D0`. This runtime has no user/
  supervisor privilege distinction to elevate, so it reduces to "run
  the routine for real" -- but the *first* implementation attempt
  pushed a plain `RTS`-style return address, and the real `CPU`
  command's own routine (which wraps its direct, privileged `CACR`
  `movec` access in `Supervisor` even though `CacheControl` already
  answers the query -- defensive real-world code) promptly executed a
  bare `rte` against that, sending the program counter straight off
  the end of guest memory (caught immediately and clearly by this
  session's own `PcOutOfBounds` diagnostic, doing exactly the job it
  was built for). Root cause: real `Supervisor`'s documented contract
  is that the routine **must terminate with `RTE`, not `RTS`** --
  fixed by pushing a real 6-byte exception-style stack frame (`SR`
  then `PC` = `EXIT_STUB_ADDR`, matching every other real 68000
  exception this runtime delivers) instead.

**Result**: every `--cpu`/`--fpu` combination (68000 through 68060,
with and without FPU) now runs the real `CPU` command to a clean exit
0, each reporting the real, correctly-formatted "System: ..." line for
that configuration -- the intended end-to-end validation of the
CPU/FPU configurability work.

New tests: 17 (`execmem.rs`: `CacheControl` query-only leaves state
unchanged, only-masked-bits set, query-then-set round-trip;
`execfmt.rs`: `Supervisor` runs the routine and returns `D0` via `RTE`,
preserves registers the routine didn't touch, a routine that never
`RTE`s is a clean error not a hang; `main.rs`: 11 `attn_flags_for`
cases, independently computed against the verified bit positions,
covering every `CpuType` and the FPU-model-specific bit choice
(`AFF_68881`/`AFF_68882` vs. `AFF_FPU40` vs. no bit at all for
`M68EC040`/`SCC68070`)).

**`Sort`/`Search`/`Join` — memory pools and a real `MatchFirst`/
`MatchNext` `IoErr()` bug — 2026-08-19.** Simon: "test sort, search and
join next" -- three more real Workbench 3.1.4 `C:` binaries from the
established empirical corpus. Two gaps found and fixed:

- **`exec.library`'s `CreatePool`/`DeletePool`/`AllocPooled`/
  `FreePooled`** (`crate::execmem`, LVOs -696/-702/-708/-714): `Sort`
  opens one unconditionally at startup (a real, common AmigaOS memory-
  management idiom -- allocate many same-lifetime items via a pool,
  `DeletePool` once instead of freeing each individually). This
  runtime's flat model has no puddle/threshold machinery to actually
  need, so `AllocPooled` is just a direct [`GuestHeap`] allocation
  (same size-rounding and `MEMF_CLEAR` handling as `AllocMem`, reading
  `requirements` back out of a small pool-header block `CreatePool`
  allocates, since `AllocPooled` itself takes no flags argument) and
  `FreePooled` is `FreeMem`'s same loud-failure-on-size-mismatch check.
  **Known, accepted simplification**: `DeletePool` only frees the
  header, not any still-outstanding `AllocPooled` blocks (this runtime
  doesn't track pool membership) -- a real leak within one process run,
  but harmless for a single CLI invocation that exits right after.
- **`MatchFirst`/`MatchNext` never set `IoErr()` on failure** -- only
  `D0` (`crate::dosanchor`). Both real functions *also* leave the same
  code in the global `IoErr()`, and `Sort`'s own source checks
  `IoErr() != ERROR_NO_MORE_ENTRIES` after a failing `MatchNext` rather
  than comparing `D0` directly -- a real, common AmigaDOS idiom (`D0`'s
  raw return value and `IoErr()`'s code are conventionally
  interchangeable for calls that document both). With `IoErr()` left
  stale from an earlier, unrelated call, that check read the wrong
  code and printed a bogus `PrintFault` ("Object is not of required
  type") for what should have been silent, successful loop
  termination -- `Sort` never actually wrote its output file as a
  result. This is the *same* stale-`IoErr()` failure shape the
  `Date`/`SetDate` investigation (see that entry above) first flagged
  as a debugging trap -- except this time genuinely a volamos bug, not
  a red herring, a useful reminder that the lesson cuts both ways.

**Result**: `Sort` now sorts and writes real output (verified against
its actual stdout, not just "didn't crash": `WORK:input.txt` ->
alphabetized `WORK:output.txt`); `Search` finds and reports the correct
line and number; `Join` concatenates files correctly. All three exit 0
with no unhandled calls.

New tests: 4 (`execmem.rs`: full `CreatePool`/`AllocPooled`/write/
`FreePooled`/`DeletePool` round trip proving the block is real,
writable guest memory; `AllocPooled(0)` returns `NULL`; `FreePooled`
size mismatch is a loud error, same as `FreeMem`'s; `dosanchor.rs`: an
end-to-end trap-dispatch test proving `IoErr()` after an exhausted
`MatchNext` reports `ERROR_NO_MORE_ENTRIES`, matching `D0`).

## Phase 4 — parity pass (three-oracle harness)

Scope: a test harness running the same fixture corpus against (1) this
runtime, (2) vamos, (3) Copperline `--run` with a real Kickstart, and
diffing normalised output. Fixture parity via AmiBake's
one-build-many-formats output (same tree as host directory for the HLE
pair, as HDF for the real-ROM column). Normalisations already agreed:
pin one Kickstart version as baseline — **Simon has set the target
compatibility level for the first stage: Kickstart/Workbench 3.1**
(2026-08-18), so the Phase 4 oracle baseline is KS 3.1, not a later
ROM revision. This also bounds which OS features are in-scope earlier
than Phase 4 would otherwise force a decision on (e.g. it argues
against depending on post-3.1 dos.library/exec.library additions
without a compatibility note). capture the Amiga return code,
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

## Future work: interactive shell mode (vamos's `-x` precedent) — 2026-08-19

Raised by Simon, pointing at vamos's own docs (amitools,
`docs/vamos.md#33-run-a-shell`): vamos can run a real Shell-Seg binary
interactively rather than just one program to completion -- `vamos -x
Shell-Seg` boots a genuine AmigaDOS Shell inside the emulator, which
then reads `S:Vamos-Startup` (if present) for its own startup
sequence and drops into an interactive `0.SYS:>`-style prompt for
further commands.

Not scoped into any phase yet -- volamos's `Runtime` is currently
built around "load one program, run it to completion, exit" (see
`crate::dispatch::Runtime::run`'s doc comment: "Returns the guest's
exit code ... on success"). An analogous `--shell`/`-x` mode would
need: (1) an interactive host<->guest I/O loop (this runtime's
`Input()`/`Output()` are currently backed by fixed `out: &mut dyn
Write` sinks per `run()` call, not a live terminal), (2) `System()`/
`Execute()` support for the Shell to invoke subsequent commands as
nested guest processes (already flagged as a gap in the T15-T22
verification notes -- `crate::dosseg`'s `system_runner` callback
exists but has no real "run a nested guest program" implementation
wired up yet), and (3) a real `C:/Shell` (or `Shell-Seg`) binary from
the same Workbench 3.1.4 corpus already in use for the C: command
testing this session has been doing. Worth revisiting once the
empirical C: command corpus (this session's ongoing work) covers
enough of `dos.library`/`exec.library` that Shell's own startup
sequence has a reasonable chance of running without hitting a wall of
gaps immediately.

## `AmiSnap` real-project run: `ExecBase.ThisTask` + `struct Process`/`pr_CLI` gap — 2026-08-19

Simon asked to run his own real AmigaOS project, `~/src/amisnap`
(built with `libnix`), through volamos as the next empirical test
after the `C:` command corpus. First attempt hit `exec.library`'s
`WaitPort` as the *very first* library call in the program, before
any `OpenLibrary`/`FindTask`, with `A0` pointing at the implausibly
low address `0x0000005c`.

Root cause traced via the `libnix` skill (confirming the universal
libnix/SAS-C Workbench-vs-CLI startup idiom: check `pr_CLI`, and only
if `NULL` -- a real Workbench launch -- `WaitPort(&pr_MsgPort)` for
the `WBStartup` message) plus a primary NDK source fetch
(`dos/dosextens.h`) for exact `struct Process`/`struct
CommandLineInterface` field layouts:

- `ExecBase.ThisTask` (offset 276) was never written by
  `Runtime::new` -- guest code that reads it inline (not via
  `FindTask()`) got `0`.
- The fake "current task" volamos builds was only `sizeof(struct
  Task)` (92 bytes), with no `pr_MsgPort`/`pr_CLI` fields at all.
- `0x5c` = 92 decimal = `pr_MsgPort`'s real offset within `struct
  Process` -- exactly what you get computing `NULL + pr_MsgPort`,
  confirming the guest read `ExecBase->ThisTask` (got `0`) then
  offset from it.

Fix (`crates/volamos-core/src/exectask.rs`,
`crates/volamos-core/src/dispatch.rs`,
`crates/volamos-core/src/dosfile.rs`):

- `create_current_task` now allocates a full `PROCESS_STRUCT_SIZE`
  (230-byte) `struct Process`, not just `TASK_STRUCT_SIZE`, and
  initializes the embedded `pr_MsgPort` (offset 92) as a real, valid,
  empty `MsgPort` (reusing `execlist`'s `init_msg_port_fields`,
  factored out of `CreateMsgPort`'s handler for this) and `pr_CLI`
  (offset 172) as a `BPTR` to a real, heap-allocated, zeroed `struct
  CommandLineInterface` (`CLI_STRUCT_SIZE` = 64 bytes) -- non-`NULL`,
  matching this runtime's CLI-style direct-execution model (never a
  Workbench launch).
- `dispatch.rs` gained `EXEC_BASE_THISTASK_OFFSET` (276, derived
  field-by-field like the existing `EXEC_BASE_ATTNFLAGS_OFFSET`/
  `EXEC_BASE_LIBLIST_OFFSET`) and now writes the real task address
  there in `Runtime::new`, right after `create_current_task`.
- `dosfile.rs`'s `Cli()` handler now reads and returns `pr_CLI`
  instead of hardcoding `0` -- its old doc comment's rationale ("this
  runtime execs a guest binary directly ... honestly reporting 'not
  part of a shell'") is exactly the bug: running a binary through
  this runtime *is* the CLI-launch case, not the Workbench case.

Verified: `AmiSnap` no longer touches `WaitPort` at all -- it now
runs well past process startup and hits a new, unrelated gap
(`unhandled library call: opcode 0xa000` -- an ambiguous ROM-vector
call at a fake-library address shared by several auto-created fake
libraries, ~`mathieeedoubbas.library`/`mathtrans.library`/etc.,
consistent with `AmiSnap`'s use of AmiSSL/floating-point code paths).
Full `cargo test --all` (485 passed), `cargo clippy --all-targets`,
`cargo fmt --all` clean. New unit tests: `pr_CLI` non-`NULL` and
`pr_MsgPort` a valid empty `MsgPort` after `create_current_task`;
`ExecBase.ThisTask` matches the real task address; `Cli()` returns
that same value via dispatch (existing `end_to_end_cli_returns_null`
test flipped to `end_to_end_cli_returns_non_null` since the old
`NULL` behavior was the bug).

The opcode-`0xa000` gap is a distinct, unrelated investigation, not
yet started.

## `AmiSnap` continued: `exec.library`'s `Alert` — 2026-08-19

Follow-on to the `ThisTask`/`pr_CLI` fix above. The opcode-`0xa000`
gap's "candidates" diagnostic listed several plausible-looking guesses
(math libraries, `AmiSSL`'s master library) -- **Simon corrected this:
`amisslmaster.library` is opened conditionally and volamos never even
attempts it, so it can't be the cause.** The real answer was in the
same candidate list: `PC - EXEC_LIBRARY_BASE == -108` exactly matches
`exec.library`'s `Alert` LVO, confirming the guest genuinely called
`Alert()` (not a math/SSL call at all -- the diagnostic just lists
every registered base's offset from `PC`, most of which are noise).

Implemented `Alert` (`crates/volamos-core/src/exectask.rs`,
`alert_handler`): `D7` = `alertNum` (per the existing, already-
verified `EXEC_LVOS` entry). `AT_DeadEnd` (bit 31, `0x80000000`,
verified against a primary NDK `exec/alerts.h`) decides the behavior:
clear (recoverable) logs via the normal `--verbose` `CallInfo`
mechanism and returns to the caller, matching real `Alert()`'s
documented "flashes and returns" case (this runtime has no Guru
Meditation display to show); set (dead-end) fails loudly with
`DispatchError::HandlerFailed` instead of pretending execution can
safely continue, since the guest itself declared system integrity
can't be guaranteed.

Verified: `AmiSnap` run with no arguments now runs to completion (exit
code 20, a real recoverable alert logged along the way) instead of
crashing -- no more "unhandled library call" at all for this binary.
Full `cargo test --all` (487 passed), `cargo clippy --all-targets`,
`cargo fmt --all` clean. New tests: a recoverable alert returns to the
caller (proven by a following instruction actually executing); a
dead-end alert fails with the expected `HandlerFailed` details.

Also added, per Simon's request: `Alert` now unconditionally
`eprintln!`s a decoded diagnostic (`AT_DeadEnd`/`AT_Recovery`,
`SubSysId`/`GeneralError`/`SpecificError` per `<exec/alerts.h>`'s
documented bit layout) straight to stderr, not gated behind
`--verbose`/`--snoop` like every other handler -- a real Guru
Meditation is never silent on real hardware, and a *recoverable*
alert in particular would otherwise vanish the instant the guest
continues past it (there's no later error for anything to surface it
through).

**Follow-up investigation, same session:** tried `AmiSnap --help`
(invented, not a real AmiSnap flag) and `AmiSnap ?` (the real
`RDA_ExtHelp` convention, confirmed against AmiSnap's own
`ReadArgs()`/`RDA_ExtHelp` usage) -- both printed nothing and exited
20. First guess (a `libnix` buffered-stdio flush bug) was wrong.
`dosargs.rs`'s own module docs already document `?`-extended-help as
a deliberate, out-of-scope gap (interactive-only, needs a real
console), so that alone wasn't the explanation either. The decoded
`Alert` diagnostic (added above) revealed the real cause immediately:
every invocation logged the exact same alert, `AG_IOError|AO_CIARsrc`
(`0x00068020`), regardless of arguments -- meaning something in
process startup, before `argv` is even parsed, was failing
identically every time.

Traced (via `m68k-amigaos-objdump`/`ar x` on the actual toolchain's
`libnix.a`, not just the public `adtools/libnix` GitHub source, which
turned out to differ) to `__cpucheck.o`: this toolchain's bundled
`libnix` startup calls `Alert()` directly (not `__request()`+`exit()`
like the current public source) when the running CPU doesn't meet the
binary's compiled-in floor. AmiSnap's own `Makefile` compiles with
`-m68020` (its documented target floor, `docs/proposal.md`'s
"Toolchain and testing"), but it was run under volamos's *default*
`--cpu 68000` -- not a volamos bug at all, a test-setup mismatch.
`--cpu 68020` makes the `Alert` disappear entirely and the run gets
substantially further, to a new, genuine, not-yet-implemented gap:
`PC - EXEC_LIBRARY_BASE == -558` exactly matches `exec.library`'s
`InitSemaphore`. Not yet investigated.

**Lesson for future gap-chasing**: when a real binary's early failure
looks argument-independent, suspect process-startup/environment
mismatch (CPU/FPU floor, missing assigns, wrong stack size) before
assuming a runtime bug -- and check the *actual* linked toolchain's
object code, not just a same-named upstream GitHub source, since
vendored forks can differ (confirmed here: this `amiga-gcc`
distribution's `__cpucheck.o` diverges from `adtools/libnix`
master's `__cpucheck.c`).

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
2. **Phase 4 external tools — decided 2026-08-18.** The three-oracle
   harness depends on AmiBake and Copperline, neither of which this
   repo controls (plus vamos, which is pip-installable).
   **Copperline** is a real, actively developed Rust Amiga emulator
   (cycle-accurate OCS/ECS/AGA, 68000-68040) with a JSON-RPC control
   protocol (`copperline-ctl`) aimed at scripts/agents — confirmed as
   the real-Kickstart oracle for Phase 4, as originally proposed (not
   swapped for the also-available `amiberry` MCP server; the two could
   be added as a cross-check later, but that's not required to start).
   **AmiBake** is Simon's own tool, in early private development at
   `~/src/amibake` (not yet public/released) — a manifest-driven Amiga
   test-image builder ("a Dockerfile for Amiga setups": pick a base
   OS, list packages/versions, a machine variant, emit a bootable
   image plus matching emulator config). It already targets emitting
   both `copperline` and `amiberry` configs from one manifest. Status
   as of 2026-08-18: `lint` and the resolver work; AROS-base
   build/boot is done; OS 3.x bases (needed for this project's KS/WB
   3.1 target) land around its milestone M8, still ahead. Consequence
   for this repo: Phase 4 cannot start in earnest until AmiBake's
   OS3.x-base milestone lands, since Phase 4's fixture parity depends
   on AmiBake producing real Workbench 3.1 images, not just AROS ones.
   Kickstart image sourcing for `REAL_ROM_B64` remains open — Simon
   holds that decision personally (Cloanto/Hyperion-copyrighted binary,
   never committed to this repo) and it isn't blocking today.
3. **Empirical corpus — decided 2026-08-18, disk source refined
   2026-08-18.** Primary real-world test corpus: the **AmigaDOS `C:`
   Shell commands** from a genuine Workbench disk (`List`, `Copy`,
   `Assign`, `Type`, `Dir`, `Echo`, etc.) — pure `dos.library`/
   `exec.library`/`utility.library` consumers with no GUI/custom-chip
   surface at all, ~70 small, independently-testable binaries giving
   broad `dos.library` coverage. Source disk: **Workbench 3.1.4**
   (dos.library V46), not the original 3.1 (V40) media — Simon's
   reasoning: 3.1.4 is Hyperion's own consolidated bug-fix-only re-issue
   of 3.1, so its `C:` binaries are a definitively consistent,
   single-source artifact, unlike original 3.1 media which shipped in
   several slightly different regional/patch revisions over the years.
   This is a source-of-binaries choice, not a change to the project's
   compatibility *target* — [[project-kickstart-target|KS/WB 3.1 (V40)
   stays the first-stage target]], and Phase 4's real-Kickstart oracle
   baseline is still pinned at 3.1; 3.1.4 is "3.1 plus fixes," so its
   `C:` commands are expected to behave identically for anything this
   runtime implements against the V40 API surface. Coverage
   (including `ReadArgs`, implemented 2026-08-18 — see Phase 3's entry
   — since every `C:` command uses the standard `ReadArgs` template
   convention), already the kind of corpus vamos itself is validated
   against
   ("general file utilities" in the original proposal). Preferred over
   a68k/vbcc as the *first* corpus: smaller, more numerous, and lower
   risk of hitting an unimplemented call deep into a multi-pass build.
   **Licensing note, same treatment as Phase 4's `REAL_ROM_B64`
   pattern**: real `C:` command binaries are Commodore/Hyperion
   copyrighted files that only exist on an actual Workbench disk —
   they must never be committed to this repo as fixtures. Testing
   against them is a **local, opt-in, user-supplied** activity (point
   the runtime at your own legitimate Workbench 3.1.4 disk image), not
   something CI can run unconditionally. **Follow-on, once the real
   `C:` commands work**: test AROS's own command-line equivalents next
   — same command set, functionally, but AROS's binaries are buildable
   from openly licensed source (APL 1.1), so unlike the real Workbench
   disk they could plausibly be built and even committed as CI fixtures
   later, without the opt-in/never-vendored constraint. Useful as a
   second, more-available corpus and as an interesting three-way check
   in its own right (real KS 3.1 binary vs. AROS binary vs. volamos),
   though that's a nice-to-have, not required. a68k/vbcc/SAS/C remain
   good *later* corpus candidates once `C:` commands pass, especially
   for exercising `System()`/`Execute` and multi-pass toolchain flows.

(Resolved, for the record: the `m68k` crate pin `=0.10.14` is still
the newest release as of 2026-08-18 — no churn action needed; and
ReadArgs placement is decided — stretch goal in T12, otherwise Phase
3.)
