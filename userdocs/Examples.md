# Examples

Task-oriented recipes beyond [Getting Started](Getting-Started.md)'s
tour — nested program execution, directory listing, scripting volamos
in a shell/CI pipeline, and diagnosing an unfamiliar binary. All
commands below use volamos's own test fixtures (`fixtures/`) and are
run from the repository root; see [CLI Reference](CLI-Reference.md) for
every flag and [Volumes and Assigns](Volumes-and-Assigns.md) for the
full `-V`/`-a` path model.

## Running a program that shells out to another program

Real AmigaOS programs routinely run other programs via `System()`/
`Execute()` (build tools invoking a compiler, a script invoking a
utility) or `RunCommand()` (running an already-`LoadSeg`'d segment
list directly). volamos runs the nested program to completion — same
volumes/assigns, same `--stack`/`--ram`/`--cpu`/`--fpu` — before
returning control to the parent, with the nested program's own stdout
interleaving correctly with the parent's:

```console
$ volamos -V TEST:fixtures fixtures/systest
sys arg
after system
$ echo "exit=$?"
exit=42
```

`fixtures/systest` calls `SystemTagList()` (the `System()` C-level
call's underlying LVO) to run `TEST:echoargs sys arg` — resolving
`TEST:echoargs` requires the volume mapping above, since the nested
program is loaded exactly like any other guest executable, by path.
It checks the nested run's own exit code, then prints its own message
and exits `42` itself.

`RunCommand()` is the lower-level `LoadSeg` + run + `UnLoadSeg`
pattern underneath `System()`/`Execute()`/the Shell — same idea, same
requirement that whatever it names actually resolves through your
volume/assign mapping:

```console
$ volamos -V TEST:fixtures fixtures/runcmdtest
run cmd
after runcommand
$ echo "exit=$?"
exit=43
```

## Listing a directory

`Lock`/`Examine`/`ExNext` (what `Dir`/`List`-style commands use under
the hood) walk a real host directory through the volume mapping:

```console
$ mkdir -p /tmp/mydir/dir
$ touch /tmp/mydir/dir/one.txt /tmp/mydir/dir/two.txt
$ volamos -V TEST:/tmp/mydir fixtures/dirtest
one.txt
two.txt
```

## Scripting volamos in a shell pipeline or CI

volamos propagates the guest program's own exit code unchanged (see
[CLI Reference](CLI-Reference.md#exit-codes)), so a script can check it
exactly as it would for a native command:

```sh
#!/bin/sh
set -e
if volamos -V TEST:fixtures fixtures/systest; then
    echo "guest program exited 0"
else
    echo "guest program failed: exit $?" >&2
    exit 1
fi
```

For a headless CI job running an original Amiga toolchain (an
assembler, a linker, a build script) against real input files on the
runner's filesystem, `-V`/`-a` map the job's working directories the
same way, and `--auto-assign` covers any volume name the toolchain
references but the job doesn't need to care about individually:

```sh
volamos -V SRC:$PWD/src -V OBJ:$PWD/build --auto-assign /opt/amiga-vols toolchain/PhxAss
```

There's no display, windowing system, or Amiga hardware dependency —
see the [README](https://github.com/sidick/volamos#platform-support)'s
"Platform support" section.

## Diagnosing an unfamiliar binary

Before mapping any volumes, run with `-s`/`--snoop` to see exactly
which libraries and files a binary you don't know actually touches:

```console
$ volamos -s fixtures/exectest
snoop: library "dos.library" -> base 0x00000800 (real)
snoop: library "utility.library" -> base 0x00000c00 (real)
exec ok
```

That tells you which libraries are in play before you need
[Supported Libraries](Supported-Libraries.md) to check whether a
specific call is implemented, or `-v`/`--verbose` for the full
per-call trace (library, LVO, handler) if something still isn't
behaving as expected.

## Giving a program more room to run

Some programs (deep recursion, large buffers, a bigger toolchain
pass) need more stack or address space than the defaults (64 KiB
stack, 16 MiB total guest RAM). Both are configurable, and volamos
fails cleanly rather than crashing if `--stack` doesn't actually fit
inside `--ram`:

```sh
volamos --stack 1M --ram 64M fixtures/hello
```

See [CLI Reference](CLI-Reference.md#-stack-size) and
[CLI Reference](CLI-Reference.md#-ram-size) for the exact defaults,
syntax, and error behavior.

## Next steps

- [CLI Reference](CLI-Reference.md) for every flag in full detail.
- [Volumes and Assigns](Volumes-and-Assigns.md) for multi-assign
  search order and the exact Amiga path-resolution rules.
- [Supported Libraries](Supported-Libraries.md) for what's implemented
  today, if you're pointing volamos at your own real Amiga binary.
