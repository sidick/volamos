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
- Pattern matching: `MatchFirst`/`MatchNext`/`MatchEnd`/`ParsePattern`/
  `MatchPattern`(`NoCase`).
- Argument parsing: `ReadArgs`/`FreeArgs`/`FindArg` (the standard
  `ReadArgs` template syntax every real `C:` command uses, including
  keywords with any number of `=`-separated names and a host
  `KEY=value` argument matching the keyword `KEY`); `ReadArgs(NULL)`
  reads the command line from the process's `Input()` buffer, the way
  `RunCommand` actually populates it.
- Environment variables: `GetVar`/`SetVar`/`DeleteVar`, backed by a
  real, directory-mapped `ENV:` volume.
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
  reaches it too. Any other packet type fails cleanly with
  `ERROR_ACTION_NOT_KNOWN`; an unknown action's `Res1` is `DOSFALSE`,
  not `-1`, matching real `dos.library` for callers that read it as a
  boolean.
- Path strings: `SplitName` (alongside `AddPart`/`FilePart`/`PathPart`).
- The `DosList`: `LockDosList`/`UnLockDosList`/`FindDosEntry`/`Info`.
- `LoadSeg`/`UnLoadSeg` (real `BPTR` seglists), `RunCommand`, and
  `SystemTagList`/`System`/`Execute` for tools that shell out to
  another guest program.
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
  runtime has one current directory for the whole run.

## exec.library

- `OpenLibrary`/`OldOpenLibrary`/`CloseLibrary` — an unknown library on
  disk gets an auto-created fake base rather than failing outright
  (mirroring `vamos`); a handful of standard Workbench libraries
  (`mathtrans`, `mathieee*`, `locale`, `intuition`) are always present,
  matching real ROM-resident behavior.
- Memory: `AllocMem`/`FreeMem`/`AllocVec`/`FreeVec`/`AvailMem`, memory
  pools (`CreatePool`/`DeletePool`/`AllocPooled`/`FreePooled`),
  `AllocEntry`/`FreeEntry` (per-entry `MEMF_CLEAR` honoured; a partial
  failure backs out fully, the post-SetPatch/V36+ behavior, and returns
  the failed entry's requirements with bit 31 set), and the raw
  `Allocate`/`Deallocate` primitive over a real, coalescing
  `MemHeader`/`MemChunk` free list.
- Lists and nodes: `AddHead`/`AddTail`/`Remove`/`RemHead`/`RemTail`/
  `Insert`/`Enqueue`/`FindName`, plus minimal single-threaded message
  ports (`CreateMsgPort`/`DeleteMsgPort`/`PutMsg`/`GetMsg`/`ReplyMsg`/
  `WaitPort`).
- Tasks and signals: `FindTask`/`SetSignal`/`SetExcept`/`Wait`/
  `Signal`/`AllocSignal`/`FreeSignal`/`Forbid`/`Permit`/`SetTaskPri`,
  including host `SIGINT`/`SIGTERM` delivered to the guest as
  `SIGBREAKF_CTRL_C`. Real cooperative multi-tasking once
  `dos.library`'s `CreateNewProc` has started a second task: `Wait`
  genuinely blocks and switches to another runnable task instead of
  always failing loudly, `Signal` wakes a blocked task, and `Permit`
  (after `Forbid`+`SysFlags |= SFB_SAR`)/same-priority `SetTaskPri`
  voluntarily yield — no preemption, a switch only ever happens at one
  of those explicit points. The outgoing task's `tc_Switch` and the
  incoming task's `tc_Launch` (`tc_Flags`' `TF_SWITCH`/`TF_LAUNCH`) are
  called on every switch, as real guest subroutines, matching real
  `exec.library` — real thread-local storage and newlib's per-task
  `errno` rely on this.
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
- `Alert`, `RawDoFmt`.
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
- `TypeOfMem`: addresses below 2 MiB (including the reserved low
  region) report `MEMF_PUBLIC|MEMF_CHIP`; 2 MiB up to the end of guest
  RAM reports `MEMF_PUBLIC|MEMF_FAST`; past the end of RAM (or the whole
  range beyond a `--ram` smaller than 2 MiB) reports `0`. `AllocMem`
  ignores the chip/fast flags, so an allocation's answer depends only
  on where the heap placed it.
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

## battclock.resource

The battery-backed clock chip, backed by the host clock — the one
system *resource* on `ExecBase`'s resource list from startup, so
`OpenResource("battclock.resource")` returns a real base:

- `ReadBattClock()` — seconds since 1978-01-01, the same host clock
  (and the same epoch) `DateStamp`/`GetSysTime` report, so an unwritten
  chip reads the same time as the system clock, and a `SetClock SAVE` /
  `SetClock LOAD` round trip doesn't shift the time.
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
