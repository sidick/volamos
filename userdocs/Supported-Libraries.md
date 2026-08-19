# Supported Libraries

volamos reimplements library calls at the API boundary — it doesn't
run real `.library` files, it intercepts the `jsr` through a library
base and services the call with native Rust code. This page lists
what's implemented today. If a guest program calls something not
listed here, volamos fails loudly with a diagnostic naming the exact
library and function, rather than silently misbehaving — that's a real
gap, not a bug in your program, and worth filing an issue for.

## dos.library

- File I/O: `Open`/`Read`/`Write`/`Seek`/`Close`, `Input`/`Output`,
  `IoErr`/`SetIoErr`.
- Locks and directory traversal: `Lock`/`UnLock`/`DupLock`/`Examine`/
  `ExNext`/`CurrentDir`/`ParentDir`/`SameLock`.
- Pattern matching: `MatchFirst`/`MatchNext`/`MatchEnd`/`ParsePattern`/
  `MatchPattern`(`NoCase`).
- Argument parsing: `ReadArgs`/`FreeArgs` (the standard `ReadArgs`
  template syntax every real `C:` command uses).
- Environment variables: `GetVar`/`SetVar`/`DeleteVar`, backed by a
  real, directory-mapped `ENV:` volume.
- Date/time: `StrToDate`/`DateToStr`/`DateStamp`.
- Process/CLI: `Cli`/`GetProgramName`/`MaxCli`/`AllocDosObject`/
  `FreeDosObject` (`DOS_RDARGS` only).
- The `DosList`: `LockDosList`/`UnLockDosList`/`FindDosEntry`/`Info`.
- `LoadSeg`/`UnLoadSeg` (real `BPTR` seglists), `RunCommand`, and
  `SystemTagList`/`System`/`Execute` for tools that shell out to
  another guest program.
- `CheckSignal`.

## exec.library

- `OpenLibrary`/`OldOpenLibrary`/`CloseLibrary` — an unknown library on
  disk gets an auto-created fake base rather than failing outright
  (mirroring `vamos`); a handful of standard Workbench libraries
  (`mathtrans`, `mathieee*`, `locale`, `intuition`) are always present,
  matching real ROM-resident behavior.
- Memory: `AllocMem`/`FreeMem`/`AllocVec`/`FreeVec`/`AvailMem`, memory
  pools (`CreatePool`/`DeletePool`/`AllocPooled`/`FreePooled`), and the
  raw `Allocate`/`Deallocate` primitive over a real, coalescing
  `MemHeader`/`MemChunk` free list.
- Lists and nodes: `AddHead`/`AddTail`/`Remove`/`RemHead`/`RemTail`/
  `Insert`/`Enqueue`/`FindName`, plus minimal single-threaded message
  ports (`CreateMsgPort`/`DeleteMsgPort`/`PutMsg`/`GetMsg`/`ReplyMsg`/
  `WaitPort`).
- Tasks and signals: `FindTask`/`SetSignal`/`SetExcept`/`Wait`/
  `Signal`/`AllocSignal`/`FreeSignal`/`Forbid`/`Permit`, including host
  `SIGINT`/`SIGTERM` delivered to the guest as `SIGBREAKF_CTRL_C`.
- The full `SignalSemaphore` API: `InitSemaphore`/`ObtainSemaphore`/
  `ReleaseSemaphore`/`AttemptSemaphore`/`FindSemaphore`/`AddSemaphore`/
  `RemSemaphore`/`ObtainSemaphoreList`/`ReleaseSemaphoreList`.
- I/O requests: `OpenDevice`/`CloseDevice`/`DoIO`/`SendIO`/`WaitIO`/
  `CheckIO`/`AbortIO`, `CreateIORequest`/`DeleteIORequest`.
- `Alert`, `RawDoFmt`.
- CPU-detection plumbing: `AttnFlags` (a real, guest-readable
  `ExecBase` field, not a call), `CacheControl`, `Supervisor`.

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

A thin stub — `DisplayAlert`, `AutoRequest`, `EasyRequestArgs`,
`CurrentTime` — just enough that a console tool's stray Intuition call
doesn't crash. **No real windowing or GUI**: `AutoRequest`/
`EasyRequestArgs` (which report which button a real user pressed)
return a fixed default rather than trying to guess, since there's no
display to show anything on in the first place.

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
real AmigaOS code uses to call these functions.

## Deliberately out of scope

- **Real windowing/GUI** — no `Screen`/`Window`/`Gadget` model, no
  display at all. volamos targets command-line tools, not Workbench
  applications or games.
- **Custom-chip access** (graphics, audio, disk hardware) — nothing to
  emulate here for a console tool.
- **Cross-process IPC/message-port bridging** — volamos runs one guest
  process at a time (`System()`/`Execute()`/`RunCommand` run a nested
  program to completion synchronously, not concurrently).
- **`exec.library`'s `MakeLibrary`/`SetFunction`** — creating a new
  library at runtime, or patching an existing library's jump table to
  point at guest code, needs a real architectural extension (a
  jump-table slot that means "call back into guest code") volamos
  doesn't have today.
