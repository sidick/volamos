# Getting Started

This walks through running a few of volamos's own test fixtures — small
pre-built AmigaOS executables — to show what a session actually looks
like: a program with no filesystem access at all, one that reads a
real file through a mapped volume, one that takes guest command-line
arguments, and one that demonstrates a runtime safety check (stack
overflow detection). It assumes `volamos` is on your `PATH` (see
[Installation](Installation.md)) and that you're running commands from
the repository root, where `fixtures/` lives.

See [CLI Reference](CLI-Reference.md) for every flag used below, and
[Volumes and Assigns](Volumes-and-Assigns.md) for the full `-V`/`-a`
model.

## 1. Run a program with no filesystem access

```console
$ volamos fixtures/hello
Hello from volamos
```

`fixtures/hello` only calls `dos.library`'s `PutStr` — it never touches
the filesystem, so no `-V`/`-a` is needed. This is the smallest
possible volamos session: load the executable, run it, print its
output, propagate its exit code.

## 2. Pass command-line arguments through

```console
$ volamos fixtures/echoargs foo bar
foo bar
```

Everything after the program path is passed straight through to the
guest program's own command line (`A0`/`D0`, the real AmigaOS startup
convention) — `fixtures/echoargs` just `PutStr`s whatever it was given.

## 3. Map a host directory onto an Amiga volume

Without a volume mapping, any `dos.library` call that touches a path
fails cleanly:

```console
$ volamos fixtures/filetest
ERR
```

(`fixtures/filetest` tries to `Open("TEST:out.txt", MODE_NEWFILE)`;
with no `TEST:` volume mapped, that fails, and the fixture prints its
own `"ERR"` marker and exits `1` — this is the guest program's own
documented failure path, not a volamos crash.)

`-V NAME:hostdir` maps an Amiga volume onto a real host directory:

```console
$ mkdir /tmp/mydemo
$ volamos -V TEST:/tmp/mydemo fixtures/filetest
hello from filetest
$ cat /tmp/mydemo/out.txt
hello from filetest
```

`fixtures/filetest` created `TEST:out.txt` (a real file at
`/tmp/mydemo/out.txt` on the host), wrote a message to it, closed it,
reopened it, read the message back, and printed it — a real
`Open`/`Write`/`Close`/`Open`/`Read`/`Close` round trip against your
host filesystem, exactly as it would run on real AmigaOS against a
real disk.

## 4. See a runtime safety check in action

volamos gives every guest program a real, bounded stack region and
checks `A7` against it on every dispatched call — a class of bug
`vamos` doesn't catch, since it never allocates a real bounded guest
stack region to overrun in the first place:

```console
$ volamos --stack 4096 fixtures/recurse
x
x
x
[... several hundred more lines, one per recursion level ...]
x
x
volamos: fixtures/recurse: stack overflow: A7 0x00ffeffc is outside the current task's stack bounds [0x00fff000, 0x01000000] -- try running with a larger --stack
```

`fixtures/recurse` recurses until it overflows its stack on purpose;
with a small `--stack` it hits the limit quickly and volamos reports
exactly where, rather than the guest silently corrupting memory past
the end of its stack region and failing confusingly far from the real
cause. (Run it with the default 64 KiB stack, or a larger explicit one,
and it runs for a very long time instead — this fixture exists
specifically to demonstrate the check.)

## 5. Watch what a program is actually doing

`-v`/`--verbose` logs every emulated library call as it happens:

```console
$ volamos -v fixtures/hello
Hello from volamos
volamos: dos.library(-948) -> PutStr
```

`-s`/`--snoop` is a lighter-weight, `SnoopDos`-style alternative that
only logs library/file *opens* — useful for quickly seeing which
libraries a real, unfamiliar binary actually depends on:

```console
$ volamos -s fixtures/echoargs foo bar
snoop: library "dos.library" -> base 0x00000800 (real)
foo bar
```

## Where to go next

- [CLI Reference](CLI-Reference.md) for every flag, including `--cpu`/
  `--fpu` (to run 68020+/FPU-requiring binaries) and `--stack`/`--ram`.
- [Volumes and Assigns](Volumes-and-Assigns.md) for multi-assign search
  order, `--auto-assign`, and the exact Amiga path semantics volamos
  implements.
- [Supported Libraries](Supported-Libraries.md) for what's implemented
  today, if you're pointing volamos at your own real Amiga binary.
