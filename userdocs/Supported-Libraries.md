# Supported Libraries

volamos reimplements library calls at the API boundary — it doesn't
run real `.library` files, it intercepts the `jsr` through a library
base and services the call with native Rust code. This page lists
what's implemented today. If a guest program calls something not
listed here, volamos fails loudly with a diagnostic naming the exact
library and function, rather than silently misbehaving — that's a real
gap, not a bug in your program, and worth filing an issue for.

`--libs-dir HOSTDIR` is an escape hatch from this: any library whose
filename (matching the Amiga library name exactly) is found in
`HOSTDIR` loads and runs as real 68k code instead, for the whole run.
Useful for benchmarking a real library against this runtime's own
implementation, or testing a replacement library (e.g. a custom
`graphics.library`) against real guest programs. Opt-in only; see the
Changelog.

## dos.library

- File I/O: `Open`/`Read`/`Write`/`Seek`/`Close`/`SetFileSize`,
  `Input`/`Output`, `IoErr`/`SetIoErr`.
- `PIPE:` — named in-memory channels, as libnix's `pipe()` needs:
  `Open("PIPE:name", MODE_NEWFILE)` opens the write end and
  `MODE_OLDFILE` the read end (either may come first). A `Read` waits
  for data and returns 0 once every writer has closed; a `Write` waits
  while the 4096-byte buffer is full; both ends work from different
  tasks, through `Read`/`Write` and the buffered-stdio calls
  (`FGetC`/`FGets`/`FRead`/`FPutC`/`FWrite`/`FPuts`/`PutStr`/`Printf`/...)
  alike. There are no buffer-size/flag suffixes on the name and no
  `MODE_READWRITE`.
- `SystemTagList` with `SYS_Asynch`, as libnix's `popen()` uses it:
  the command runs as a process of its own *inside* the same run, so it
  can share `PIPE:` channels with the caller. It gets `SYS_Input`/
  `SYS_Output`/`SYS_Error` as its standard handles (and closes them when
  it ends), a real CLI so `GetProgramName` and `argv[0]` work, and
  `NP_ExitCode`/`NP_ExitData` are honored, with the command's return
  code in `cli_ReturnCode`. The command is looked up in the current
  directory, then `C:`, so run from the program's own directory (e.g.
  `-V WORK:. --cwd WORK:`) if the command is the program itself. A
  synchronous `SystemTagList` still runs the command in a separate
  nested run, with `SYS_Input`/`SYS_Output` read but not acted on.
- Locks and directory traversal: `Lock`/`UnLock`/`DupLock`/
  `DupLockFromFH`/`OpenFromLock`/`Examine`/`ExamineFH`/`ExNext`/`ExAll`/
  `ExAllEnd`/`CurrentDir`/`ParentDir`/`ParentOfFH`/`SameLock`/
  `ChangeMode`/`NameFromLock`/`GetCurrentDirName`.
- Filesystem links and record locking: `MakeLink`/`ReadLink` (real host
  symlinks/hard links — see the Changelog's 0.11 entry for the scope
  this covers), `LockRecord`/`LockRecords`/`UnLockRecord`/
  `UnLockRecords` (always-succeeds stubs — this runtime models no real
  concurrency).
- File comments and `Open` on directories: `SetComment` refuses a comment
  longer than 79 characters (`ERROR_COMMENT_TOO_BIG`, the size of
  `fib_Comment`), and `Open(…, MODE_OLDFILE)` of a
  directory fails with `ERROR_OBJECT_WRONG_TYPE`, as on AmigaOS.
- Pattern matching: `MatchFirst`/`MatchNext`/`MatchEnd`/`ParsePattern`/
  `MatchPattern`(`NoCase`). Matching is bounded (a work and nesting
  limit per match): a pathological pattern or a huge input with a
  nested repeat reports "no match" instead of hanging or crashing the
  host.
- Argument parsing: `ReadArgs`/`FreeArgs`/`FindArg` (the standard
  `ReadArgs` template syntax every real `C:` command uses, including
  keywords with any number of `=`-separated names and a host
  `KEY=value` argument matching the keyword `KEY`; a bare argument goes
  to the next slot not already filled by its keyword, and a surplus one
  is `ERROR_TOO_MANY_ARGS`, both checked against real Kickstart 3.1);
  `ReadArgs(NULL)`
  reads the command line from the process's `Input()` buffer, the way
  `RunCommand` actually populates it.
- Environment variables: `GetVar`/`SetVar`/`DeleteVar`/`FindVar`, backed by
  a real, directory-mapped `ENV:` volume. `FindVar` returns a guest
  `struct LocalVar` for a local (`LV_VAR`) variable; a later `SetVar`
  updates it in place and `DeleteVar` frees it. The structures are not linked into
  `pr_LocalVars`, and an `LV_ALIAS` lookup reports "not found". clib2's
  `setenv()`/`putenv()` need it.
- Date/time: `StrToDate`/`DateToStr`/`DateStamp`/`CompareDates`/`Delay`.
- Process/CLI: `Cli`/`GetProgramName`/`SetProgramName`/`MaxCli`/
  `GetArgStr`/`SetArgStr` (`pr_Arguments`, the same buffer startup
  code's `A0` points at)/`AllocDosObject`/`FreeDosObject`
  (`DOS_RDARGS`, `DOS_EXALLCONTROL`, `DOS_FIB`, and `DOS_FILEHANDLE` —
  a real `FileHandle`, the same allocation `Open` itself uses).
- `StartNotify`/`EndNotify`: `StartNotify` always fails cleanly with
  `ERROR_ACTION_NOT_KNOWN` (no filesystem-change notification
  machinery exists in this runtime at all) rather than crashing the
  run as an unhandled call; real callers that treat a failed
  `StartNotify` as "no notifications, poll instead" (e.g.
  `ixemul.library`'s `ENV:` watch) work unaffected. `EndNotify` always
  succeeds.
- Packet-level file I/O: a program that bypasses the library calls
  above and sends raw `DosPacket`s straight to a handler's message
  port (the real AmigaOS idiom some runtime libraries use, e.g.
  `ixemul.library`) is answered the same as the library calls:
  `ACTION_READ`/`ACTION_WRITE`/`ACTION_SEEK`/`ACTION_END`,
  `ACTION_LOCATE_OBJECT` (`Lock`), `ACTION_FINDINPUT`/
  `ACTION_FINDOUTPUT`/`ACTION_FINDUPDATE` (`Open`), `ACTION_EXAMINE_FH`,
  `ACTION_READ_LINK`, `ACTION_CREATE_DIR`/`ACTION_DELETE_OBJECT`/
  `ACTION_RENAME_OBJECT`/`ACTION_MAKE_LINK` (hard and soft),
  `ACTION_SET_PROTECT`/`ACTION_SET_DATE`/`ACTION_SET_OWNER`/
  `ACTION_SET_FILE_SIZE`, `ACTION_WAIT_CHAR`, and
  `ACTION_DISK_INFO`/`ACTION_INFO`/`ACTION_IS_FILESYSTEM` sent to a
  volume's own task. Every lock's `fl_Task` and `GetDeviceProc`'s
  `dvp_Port` point at this same synthetic handler port, so a caller
  that resolves it itself (as `ixemul.library`'s `__plock()` does)
  reaches it too. `GetDeviceProc`'s `dvp_Lock` is what the name is
  relative to: the volume (or assign) root for a `Vol:`-qualified name,
  the current directory for a bare or relative one (checked against real
  Kickstart 3.1; real Kickstart returns a `NULL` lock for a device,
  where volumes here share one port and the root lock identifies the
  volume). Any other packet type fails cleanly with
  `ERROR_ACTION_NOT_KNOWN`; an unknown action's `Res1` is `DOSFALSE`,
  not `-1`, matching real `dos.library` for callers that read it as a
  boolean.

  `DoPkt` answers that same full set — it is just the library call
  that builds and sends the packet for you — provided the port names
  something that answers packets here (a volume's task, or the handler
  port above). A name carrying its own device part works with or
  without it, as on real hardware, where `dos.library` passes the name
  through as written and the handler skips up to the first `:`. A port
  this runtime doesn't recognize still gets `ERROR_ACTION_NOT_KNOWN`
  rather than silent service.
- Errors: `Fault`/`PrintFault` render the standard AmigaDOS message,
  and `ErrorReport` always answers `DOSTRUE` — "cancel". There is no
  Intuition screen to put a Retry/Cancel requester on, and the autodoc
  makes cancel the documented answer when the requester can't be put
  up (or `pr_WindowPtr` is `-1`, or the code isn't one it has a
  requester for); `DOSFALSE` would tell the caller to *retry* an
  operation that cannot start working. `IoErr()` is set from the code
  only for the codes real `ErrorReport` has a requester for, which is
  the documented behaviour of every version volamos targets rather
  than an omission — a caller needing the general case is documented
  to `SetIoErr(code)` first.
- Path strings: `SplitName` (alongside `AddPart`/`FilePart`/`PathPart`).
- Formatted output: `VPrintf`/`VFPrintf` (C `printf` directives) and
  `VFWritef`/`FWritef` (the BCPL set: `%S %Tw %C %Ow %Xw %Iw %Uw %N %$`,
  with `w` one base-36 digit). Any other `%c` prints `c` and takes no
  argument, as on real Kickstart 3.1, which is also what the checked
  behaviour of `%X`/`%O` (zero-padded, low digits kept, hex uppercase)
  and `%I`/`%U` (space-padded, never truncated) comes from.
- The `DosList`: `LockDosList` (which nests, as on AmigaOS)/`UnLockDosList`/
  `FindDosEntry`/`Info`.
  `Info` and `ACTION_DISK_INFO`/`ACTION_INFO` report the real size and
  free space of the host filesystem behind the volume (in 512-byte
  blocks, larger when a huge disk would overflow a 32-bit block count),
  so two volumes on the same host disk show overlapping capacity. Where
  the host can't be asked (non-Unix), a fixed 100,000-block disk is
  reported instead.
- `LoadSeg`/`UnLoadSeg` (real `BPTR` seglists), `RunCommand`, and
  `SystemTagList`/`System`/`Execute` for tools that shell out to
  another guest program. Overlay executables (`HUNK_OVERLAY`) load node
  by node through the overlay manager's `LoadSeg(NULL, table, fh)`; a
  node can be `UnLoadSeg`ed when a sibling displaces it, and unloading
  the root frees every loaded node, the overlay tables, and closes the
  overlay file.
- Hunk memory flags: `LoadSeg` and overlay programs place each hunk in
  the kind of memory its size longword asks for. A `HUNKF_CHIP` hunk (data
  for the custom chips) is in chip RAM, so `TypeOfMem` on it reports a
  chip address as it would on hardware; a `HUNKF_FAST` hunk prefers fast
  RAM and falls back to any memory when there is none; the extended form
  names the `MEMF_*` attributes outright (and, like `AllocMem`, fails when
  they cannot be met); an unflagged hunk takes any memory, preferring fast
  RAM like `AllocMem(MEMF_ANY)`. A hunk that does not fit fails the
  `LoadSeg` and frees what it had allocated. The program you start from
  the command line is laid out in low memory, which is chip RAM, whatever
  its flags say, so its `HUNKF_FAST` hunks load into chip RAM too.
- `CheckSignal`.
- `CreateNewProc` — a real second task, cooperatively scheduled
  alongside the main one (see `exec.library`'s "Tasks and signals"
  below); unblocks both the libpthread and native-toolchain thread
  models under `m68k-amigaos-gcc`. `NP_Entry` (required)/`NP_StackSize`/
  `NP_Name`/`NP_Priority`/`NP_Input`/`NP_Output`/`NP_Error` (and their
  `NP_Close*` flags)/`NP_ConsoleTask`/`NP_WindowPtr`/`NP_Arguments` are
  honored, with the defaults `<dos/dostags.h>` documents; the standard
  handles are stored exactly as given, unchecked, matching real DOS
  (AmigaPorts' libpthread passes a thread's slot number through
  `NP_Input`, not a real filehandle, and reads it straight back out of
  `pr_CIS`). `NP_Seglist` (loading a seglist rather than a bare entry
  point) isn't supported — every known caller always supplies
  `NP_Entry` — and `NP_CurrentDir`/`NP_HomeDir` aren't read, since this
  runtime has one current directory for the whole run. Each process has
  its own `IoErr()` (`pr_Result2`): a child's failed call no longer
  changes its parent's. The rest of the per-process state (standard
  streams, current directory, `PROGDIR:`, local variables) is still
  shared between processes for now (#326).

## exec.library

- `OpenLibrary`/`OldOpenLibrary`/`CloseLibrary` — an unknown library on
  disk gets an auto-created fake base rather than failing outright
  (mirroring `vamos`); a handful of standard Workbench libraries
  (`mathtrans`, `mathieee*`, `locale`, `intuition`) are always present,
  matching real ROM-resident behavior.
- Memory: `AllocMem`/`FreeMem`/`AllocVec`/`FreeVec`/`AvailMem`
  (`AllocMem` blocks sit on exec's 8-byte boundary, and `AllocVec` keeps
  `byteSize + 4` in the 4 bytes before the block, so its result is 4 mod 8 --
  as on exec, and independent of the program's size), memory
  pools (`CreatePool`/`DeletePool`/`AllocPooled`/`FreePooled`),
  `AllocEntry`/`FreeEntry` (per-entry `MEMF_CLEAR` honoured; a partial
  failure backs out fully, the post-SetPatch/V36+ behavior, and returns
  the failed entry's requirements with bit 31 set), and the raw
  `Allocate`/`Deallocate` primitive over a real, coalescing
  `MemHeader`/`MemChunk` free list.
- Lists and nodes: `AddHead`/`AddTail`/`Remove`/`RemHead`/`RemTail`/
  `Insert`/`Enqueue`/`FindName`, plus message ports
  (`CreateMsgPort`/`DeleteMsgPort`/`PutMsg`/`GetMsg`/`ReplyMsg`/
  `WaitPort`). `CreateMsgPort` reserves a real signal bit; `PutMsg` and
  `ReplyMsg` signal the port's task with it, and `WaitPort` on an empty
  port blocks and switches to another task (a deadlock, with nothing
  else runnable and no pending timer request, fails loudly instead of
  hanging), so tasks started with `CreateNewProc` can pass messages.
- Tasks and signals: `FindTask`/`SetSignal`/`SetExcept`/`Wait`/
  `Signal`/`AllocSignal`/`FreeSignal`/`Forbid`/`Permit`/`SetTaskPri`,
  including host `SIGINT`/`SIGTERM` delivered to the guest as
  `SIGBREAKF_CTRL_C` (to the main task, even while a `CreateNewProc`
  task is the one running). Real cooperative multi-tasking once
  `dos.library`'s `CreateNewProc` has started a second task: `Wait`
  genuinely blocks and switches to another runnable task instead of
  always failing loudly, `Signal` wakes a blocked task, and `Permit`
  (after `Forbid`+`SysFlags |= SFB_SAR`)/same-priority `SetTaskPri`
  voluntarily yield — no preemption, a switch only ever happens at one
  of those explicit points. The outgoing task's `tc_Switch` and the
  incoming task's `tc_Launch` (`tc_Flags`' `TF_SWITCH`/`TF_LAUNCH`) are
  called on every switch, as real guest subroutines, matching real
  `exec.library` — real thread-local storage and newlib's per-task
  `errno` rely on this. A task's FPU registers (`FP0`-`FP7`, `FPCR`,
  `FPSR`, `FPIAR`) are saved and restored across a switch too, and an
  exiting task's `tc_Switch` runs before its `struct Process` and stack
  are freed.
- The full `SignalSemaphore` API: `InitSemaphore`/`ObtainSemaphore`/
  `ReleaseSemaphore`/`AttemptSemaphore`/`FindSemaphore`/`AddSemaphore`/
  `RemSemaphore`/`ObtainSemaphoreList`/`ReleaseSemaphoreList`/
  `ObtainSemaphoreShared`/`AttemptSemaphoreShared` (AROS semantics: a
  free semaphore obtained shared gets `ss_Owner = NULL`).
  `ObtainSemaphore`/`ObtainSemaphoreShared` genuinely block (and
  `ReleaseSemaphore` genuinely wakes) when a `CreateNewProc`'d task
  contends a semaphore another task holds — the exact shape
  libpthread's mutex/thread-table bookkeeping hits at its first lock.
  A blocked waiter is visible in the semaphore as on exec — a
  `SemaphoreRequest` on `ss_WaitQueue`, counted in `ss_QueueCount` —
  and a release hands the semaphore to waiters in arrival order.
  `ObtainSemaphoreList`/`ReleaseSemaphoreList` don't implement that
  same block-and-retry dance and still fail loudly on contention.
- I/O requests: `OpenDevice`/`CloseDevice`/`DoIO`/`SendIO`/`WaitIO`/
  `CheckIO`/`AbortIO`, `CreateIORequest`/`DeleteIORequest`.
- `Alert`, `RawDoFmt` (a field width or precision is capped at 4096).
- `SetFunction` — patches a single library jump-table entry with a real
  `JMP` to guest code and returns a function pointer standing in for
  whatever was there before (a synthesized stub for a host-handled
  entry, the real previous target if already patched), so a guest hook
  can chain through to the original behavior exactly like real
  `SetFunction` callers do. Doesn't support patching an auto-created
  fake (vamos-escape-hatch) library's jump table, since those don't
  have the real 6-byte-per-vector spacing a `JMP abs.l` needs. A patch
  is purely in-memory for the single guest run that installed it —
  there's no persistence across separate `volamos` invocations and no
  shared system-wide library state, so this is only useful for testing
  a program's own hooking logic against itself, not for anything
  resembling a real system-wide patch tool (`Enforcer`, `SnoopDos`) —
  see the "Deliberately out of scope" note below on why.
- Building libraries at runtime: `MakeLibrary` (including its `init`
  routine), `MakeFunctions`, `InitStruct`, `AddLibrary`, `SumLibrary`
  and `RemLibrary`. A library added this way (or by a disk library's
  own init code) is openable by name and its jump table holds real
  `JMP`s. `AddLibrary` sets `LIBF_SUMUSED` and computes `lib_Sum`;
  `SumLibrary` rechecks it (a mismatch is a recovery Alert on
  `stderr`) and `SetFunction` keeps it current. `RemLibrary` runs the
  library's Expunge vector and only acts on libraries loaded from disk
  or added with `AddLibrary` (built-in libraries have no Expunge, so it
  is a no-op for them).
- Software interrupts: `Cause` runs the node's handler for real
  (priority-ordered across exec's five levels, `NT_SOFTINT` guard
  against double-queuing, out-of-range `ln_Pri` rejected, nested `Cause` from a handler queued and run
  afterwards) before returning; `Disable` is not honoured as a
  deferral. `SetIntVector`/`AddIntServer`/`RemIntServer` are
  deliberate loud stubs for every `INTB_*` except `INTB_VERTB`: the
  run stops with a message naming the one requested, since this
  runtime never delivers hardware interrupts. `INTB_VERTB` is the one
  exception — `AddIntServer`/`RemIntServer` accept it (tracked, but
  still never fired; there's no VBlank tick model), since some
  runtime libraries (`ixemul.library`) add one unconditionally on
  every `OpenLibrary` and would otherwise fail to open at all.
- Resources: `AddResource`/`RemResource`/`OpenResource` over a real
  `ExecBase.ResourceList`. No built-in resources (`misc.resource`,
  `cia.resource`, ...) are modelled; `OpenResource` of one returns
  `NULL`.
- Memory regions: guest RAM is a set of regions at the addresses a real
  Amiga has them, chosen by `--cpu` and resizable with `--chip`/`--fast`/
  `--slow`/`--accel`/`--z3` (see the
  [memory map](CLI-Reference.md#guest-memory-map)); addresses outside
  every region are unmapped (reads give 0, writes are dropped).
  `AllocMem`/`AllocVec`/`AllocPooled` honour the memory-kind flags the
  way real exec does: a block is only taken from a region whose
  attributes include *every* requested bit, highest priority first. So
  `MEMF_CHIP` comes from chip RAM, `MEMF_FAST` from fast RAM,
  `MEMF_24BITDMA` skips RAM above the 24-bit space, no flags
  (`MEMF_ANY`) prefers fast RAM and falls back to chip, and
  `MEMF_CHIP|MEMF_FAST` (no such memory) or `MEMF_FAST` on a machine
  with no fast RAM returns `NULL`. `TypeOfMem` reports the owning
  region's real `mh_Attributes` word and `0` outside RAM:

  | Region | Priority | `TypeOfMem` |
  |---|---|---|
  | chip | -10 | `0x703` |
  | slow | -5 | `0x705` |
  | Zorro II fast | 0 | `0x605` |
  | CPU-board RAM | 30 | `0x505` (`0x705` below 16 MiB) |
  | Zorro III | 0 | `0x405` (assumed, not hardware-checked) |

  `AvailMem` counts only the requested kind (`MEMF_CHIP`, `MEMF_FAST`,
  plus `MEMF_LARGEST`), and `MEMF_TOTAL` reports the whole region size
  rather than the free part. The priorities and attribute words were
  measured on real Kickstart 3.1 (chip, slow, Zorro II and motherboard
  RAM).

  `ExecBase.MemList` holds one real `MemHeader` per region, highest
  priority first — the names (`"chip memory"`, `"memory"`,
  `"expansion memory"`), priorities, `mh_Attributes`, `mh_Lower`/
  `mh_Upper`, and `mh_Free` — with the `mh_First` chain of `MemChunk`s
  describing each region's free blocks (written into the free memory
  itself, as exec does). So a program that enumerates memory sees the
  RAM, and `ExecBase.MaxLocMem` is the chip size. The chain is brought
  up to date when the program calls `Forbid()` or `Disable()` — the
  idiom for walking the list — not on every allocation, so a guest that
  walks it without either may see a slightly stale free count. Free
  blocks smaller than a chunk (8 bytes) are left out of the chain and
  `mh_Free`. `AddMemList`, adding or removing headers, and `AllocAbs`
  are not modelled.
- CPU-detection plumbing: `AttnFlags` (a real, guest-readable
  `ExecBase` field, not a call), `CacheControl`, `Supervisor` (enters
  the routine through a real exception frame — a 68010+ format-0
  vector word included — so it may make further library calls and
  `RTE` anywhere, not just back to the caller).

## utility.library

- Tag-list handling: `FindTagItem`/`GetTagData`/`NextTagItem`/
  `AllocateTagItems`/`FreeTagItems`.
- `Stricmp`/`Strnicmp`/`ToUpper`/`ToLower`.
- 32-bit math helpers: `SMult32`/`UMult32`/`SDivMod32`/`UDivMod32`.
- Amiga date conversions: `Amiga2Date`/`Date2Amiga`/`CheckDate`.

## locale.library

Character classification (`IsAlpha`/`IsDigit`/`IsAlNum`/`IsCntrl`/
`IsGraph`/`IsLower`/`IsPrint`/`IsPunct`/`IsSpace`/`IsUpper`/
`IsXDigit`), case conversion (`ConvToUpper`/`ConvToLower`), a
locale-aware `StrnCmp`, and a minimal `OpenLocale`/`CloseLocale`. Every
function uses the classic built-in Amiga charset (Latin-1/ISO-8859-1)
regardless of what `Locale*` is passed in — this is **not** a real
multi-locale/catalog system (no `LC:`-directory scanning, no
translated catalogs), matching `vamos`'s own scope for this library.

`OpenLocale` returns a fully populated `struct Locale` holding the
built-in default locale — the United States one, exactly what a real
`locale.library` returns with no `ENV:Sys/locale.prefs` (values taken
from real Kickstart 3.1 hardware): `"."` decimal point, `","`
grouping, `$`/`USD` currency, US date and time formats, and so on.
Programs built with libnix therefore get a working `setlocale(LC_ALL,
"")`, so `strtod`/`sscanf("%lf")` parse `1.5` correctly.
`loc_GMTOffset` follows the host's time zone (minutes *west* of UTC, so
`-540` in Tokyo and `300` in New York in winter, `240` under daylight
saving), matching the host-local clock `DateStamp` reports. A guest's
`time()` therefore equals the host's, and setting `TZ` on the host runs
a program under another zone.

## intuition.library

Headless: real, inspectable `Screen`/`Window`/`RastPort` structures,
but no actual rendering or user input. `OpenWindow`/`CloseWindow`
attach a window to a single, lazily-created default public screen
(always `WBENCHSCREEN`-typed); `LockPubScreen`/`UnlockPubScreen`/
`GetScreenData` work against it; custom screens aren't supported.
`DisplayAlert` follows real `exec.library` alert semantics, and
`CurrentTime` reads the host clock. `AutoRequest`/`EasyRequestArgs`
(which report which button a real user pressed) return a fixed
default (`TRUE`/`0` respectively) rather than trying to guess, since
there's no display to show anything on in the first place.

## Math libraries

`mathffp`, `mathtrans`, `mathieeedoubbas`, `mathieeedoubtrans` — real
FFP/IEEE-754 arithmetic, not fake traps. Includes a faithful
reproduction of a genuine historical AmigaOS quirk: `mathffp.library`'s
`SPSub`/`SPDiv` compute their result with arguments effectively
reversed from what their names suggest (`SPSub(left, right)` actually
returns `right - left`) — this matches real `mathffp.library`, not a
bug in volamos.

## timer.device

Real time-arithmetic (`AddTime`/`SubTime`/`CmpTime`/`ReadEClock`/
`GetSysTime`) via the documented `io_Device`-as-library-base idiom
real AmigaOS code uses to call these functions. `dos.library`'s
`DOSBase->dl_TimeReq` is a real `timerequest` with its `io_Device`
pointing here too, for callers (e.g. libnix's `clock()`) that read it
directly instead of going through `OpenDevice`. Without `--clock-mhz`,
`GetSysTime`/`ReadEClock` (and Intuition's `CurrentTime`) resolve real
host microseconds, not 1/50 s ticks.

`SendIO(TR_ADDREQUEST)` is genuinely asynchronous: it registers the
request and returns immediately rather than blocking, so a timed
`Wait()` can race a condition variable's signal against the timer
exactly like real pthread/newlib `cond_timedwait` code expects.
Completion delivers through the request's own reply-port signal,
either when its real deadline passes (checked at `Wait`/`CheckSignal`)
or — once every task is blocked and nothing else could possibly make
progress first — fired early by actually sleeping out the real
remaining duration first, so a timed wait still measures its real
requested duration rather than resolving instantly.
`WaitIO`/`CheckIO`/`AbortIO` all recognize a still-pending request.
A synchronous `DoIO(TR_ADDREQUEST)` suspends only the calling task,
as `Delay()` does: the other tasks run until its time is up, measured
in microseconds, and a request that is due at once (`{0, 0}`) still
gives them a turn. Like `Delay()`, that relies on the other tasks
making library calls: volamos doesn't preempt, so a task that
busy-waits on memory without one keeps the CPU.

## expansion.library

The Zorro autoconfig bookkeeping library — and here, the truthful
answer that nothing is plugged in. It's in Kickstart on every real
Amiga, including a bare A500 with an empty expansion connector, so
returning `NULL` for it (as volamos did until
[issue #292](https://github.com/sidick/volamos/issues/292)) let a guest
read "no expansion bus" as "broken Kickstart" — the real Workbench
3.1.4 `BindDrivers` took the second reading and exited 20 in silence.

Implemented, and answering for real:

- `FindConfigDev` walks a real `ConfigDev` list, with `-1` wildcards for
  manufacturer and product. It finds nothing, because nothing is
  plugged in — and finds a board a guest added itself with
  `AddConfigDev`, which `RemConfigDev` takes back off.
- `ObtainConfigBinding`/`ReleaseConfigBinding` — no-ops. They exist so
  two driver binders don't claim the same board; with no boards there's
  nothing to serialize. `BindDrivers` calls them.
- `GetCurrentBinding`/`SetCurrentBinding` round-trip the
  `CurrentBinding` struct through `ExpansionBase`'s own private field,
  as on hardware.
- `AllocConfigDev`/`FreeConfigDev` really allocate and free a zeroed
  `ConfigDev`.
- `AllocExpansionMem`/`AllocBoardMem` fail (`0`): there is no
  autoconfig address space to carve a board window out of, and "cannot"
  is the honest answer. `FreeExpansionMem`/`FreeBoardMem` are no-ops.

`ExpansionBase` itself is real — `eb_Flags`, the private
`CurrentBinding`, and well-formed empty board and mount lists at their
documented offsets — so a guest walking `eb_MountList` finds an empty
list rather than garbage.

Deliberately *not* implemented, because each one **acts** and a silent
success would leave a guest believing something false:
`ConfigBoard`/`ConfigChain` (configure a board into the address map),
`ReadExpansionRom`/`ReadExpansionByte`/`WriteExpansionByte` (address a
board's autoconfig space — with no boards there's no address to pass),
and `MakeDosNode`/`AddDosNode`/`AddBootNode` (mount a filesystem;
volumes come from `-V`/`-a` instead). Each fails loudly, naming the
library and LVO.

## icon.library

ROM-resident on 3.x — it isn't on a Workbench 3.1.4 install disk at
all — so it always opens. Implemented are the calls whose answer
doesn't depend on reading or writing a `.info` file's contents:

- `GetDiskObject` on a name with no `name.info` returns `NULL`, with
  `IoErr()` set to why (`ERROR_OBJECT_NOT_FOUND` for a plain missing
  icon) — the routine "does this file have an icon?" question.
- `GetDefDiskObject` returns `NULL` + `IoErr()` when
  `ENV:Sys/def_<type>.info` is absent (`WBDEVICE`, which 3.1 has no
  default for, gives `ERROR_BAD_NUMBER`).
- `DeleteDiskObject` deletes `name.info` exactly as `DeleteFile` would.
- `FreeDiskObject(NULL)` is ignored.
- `FindToolType`, `MatchToolValue` and `BumpRevision` are implemented in
  full: case-insensitive tool-type lookup (a bare flag entry like
  `DONOTWAIT` is found), `|`-separated value matching, and
  `copy_of_`/`copy_<n>_of_` renaming truncated to 30 characters.

Reading or writing a real icon isn't supported yet
([issue #313](https://github.com/sidick/volamos/issues/313)):
`GetDiskObject`/`GetDefDiskObject` finding the file *present*,
`GetDiskObject(NULL)`, `GetDiskObjectNew`, `PutDiskObject`,
`PutDefDiskObject` and the free-list calls each fail the run loudly,
naming the call — rather than telling the guest "no icon" when there is
one, or that a write succeeded when nothing was written. The 3.5+ calls
(`GetIconTagList` and friends) aren't part of the 3.1 interface and fail
the same way.

## battclock.resource

The battery-backed clock chip, backed by the host clock — the one
system *resource* on `ExecBase`'s resource list from startup, so
`OpenResource("battclock.resource")` returns a real base:

- `ReadBattClock()` — seconds since 1978-01-01, the same host clock
  (and the same epoch) `DateStamp`/`GetSysTime` report, so an unwritten
  chip reads the same time as the system clock, and a `SetClock SAVE` /
  `SetClock LOAD` round trip doesn't shift the time. In the host's
  *local* time, as an Amiga clock reads — see
  [Differences from vamos](Differences-from-vamos.md), which reports
  UTC.
- `WriteBattClock(secs)` — sets the chip, which then keeps ticking from
  there for the rest of the run. Like `SetClock SAVE` on real AmigaOS,
  it doesn't change the system time, and nothing persists past the
  invocation (volamos is one process per run, with no boot state). Note
  that the reverse direction isn't available either: `timer.device`'s
  `TR_SETSYSTIME` is a no-op here (there's no virtual clock to set), so
  `SetClock LOAD` can't push a written chip value back into the system
  clock — after writing the chip to some distant time, `ReadBattClock`
  and `DateStamp` will disagree for the rest of the run.
- `ResetBattClock()` — puts the chip back to 1978-01-01, per the NDK
  autodoc.

Other resources (`misc.resource`, `ciaa.resource`/`ciab.resource`,
`disk.resource`, ...) are absent, so `OpenResource` returns `NULL` for
them. The CIA resources in particular would mean emulating the CIA
timers and interrupt wiring — see "Deliberately out of scope" below. A
guest can still `AddResource` a resource of its own and find it again
with `OpenResource`; `--snoop` logs every resource open alongside the
library and file opens.

Why this one matters in practice: the m68k-amigaos-gcc startup code
opens every library *and resource* on `__LIB_LIST__` before `main()`,
and the stock crt0 doesn't check the result — so a program that merely
references `BattClockBase` used to jump through `NULL` and die before
reaching `main`.

## Deliberately out of scope

- **Real windowing/GUI** — no `Screen`/`Window`/`Gadget` model, no
  display at all. volamos targets command-line tools, not Workbench
  applications or games.
- **Custom-chip access** (graphics, audio, disk hardware) — nothing to
  emulate here for a console tool.
- **Cross-process IPC/message-port bridging** — volamos runs one guest
  process at a time (`System()`/`Execute()`/`RunCommand` run a nested
  program to completion synchronously, not concurrently).
