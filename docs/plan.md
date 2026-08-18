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

Known deviations/deferrals, all documented in-module: no
`MemHeader`/`MemChunk` guest structures (flat allocator, per plan); no
public message-port registry (`FindPort` → NULL); signal exceptions
(`SetExcept`) tracked but never delivered; stack/host-break checks
happen at library-call granularity only; `ReadArgs` still deferred (real
startup code parses `A0`/`D0` itself); `NewStackSwap` and
`System()` I/O-redirection tags unimplemented pending corpus need.
The `since`-version field proposed above remains future work — Phase 3
picked its calls from the plan's own scope list rather than needing a
version-gated filter yet.

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
