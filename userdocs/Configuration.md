# Configuration Files

Typing out the same `-V`/`-a`/`--cwd`/`--stack`/etc. flags on every
invocation for a given project gets old fast. `~/.volamos` supplies
default values for volamos's own flags; a `.volamos` file in the
current directory overrides it for per-project settings; an explicit
flag on the command line always wins over both. Neither file can
choose *what to run* — `<program>`/`[args...]` are never config-file
settable, only the scaffolding flags around them.

## Grammar

`KEY=VALUE` per line. Blank lines and lines starting with `#` are
ignored; whitespace around the `=` is trimmed:

```
# ~/.volamos -- example global defaults
VOLUME=SYS:/home/me/amiga
VOLUME=WORK:/home/me/work
ASSIGN=LIBS:SYS:libs+SYS:libs2
CWD=SYS:
AUTO_ASSIGN=/home/me/amiga-volumes
STACK=256K
RAM=32M
CPU=68020
FPU=true
VERBOSE=false
SNOOP=false
```

Each key mirrors a [CLI Reference](CLI-Reference.md) flag directly:

| Key | Equivalent flag | Notes |
|---|---|---|
| `VOLUME` | `-V`/`--volume` | `NAME:hostdir`, repeatable |
| `ASSIGN` | `-a`/`--assign` | `NAME:target[+target...]`, repeatable |
| `CWD` | `--cwd` | |
| `AUTO_ASSIGN` | `--auto-assign` | |
| `STACK` | `--stack` | same `K`/`M`-suffixed `SIZE` syntax |
| `RAM` | `--ram` | same `K`/`M`-suffixed `SIZE` syntax |
| `CPU` | `--cpu` | same model names |
| `FPU` | `--fpu`/`--no-fpu` | `true`/`false` |
| `VERBOSE` | `-v`/`--verbose` | `true`/`false` |
| `SNOOP` | `-s`/`--snoop` | `true`/`false` |

`VOLUME`/`ASSIGN` are **repeatable** — one line per entry, same as
giving `-V`/`-a` more than once. Every other key is **singular**: if
the same key appears more than once in one file, the last line wins
(the same rule repeating a CLI flag already follows).

Relative `VOLUME`/`AUTO_ASSIGN` host directories resolve against
volamos's own process working directory, exactly like a CLI-supplied
relative path — *not* against the config file's own location. Prefer
absolute paths in `~/.volamos` for anything meant to work regardless
of where volamos is invoked from.

## Precedence

For `CWD`/`AUTO_ASSIGN`/`STACK`/`RAM`/`CPU`/`FPU`/`VERBOSE`/`SNOOP`:

```
command-line flag  >  ./.volamos  >  ~/.volamos  >  built-in default
```

For `VOLUME`/`ASSIGN`: entries from every source all apply — nothing
is silently dropped — but where the **same `NAME:`** appears in more
than one source, the higher-precedence source's mapping for that name
wins, in the same order as above. A worked example:

```
# ~/.volamos
VOLUME=SYS:/home/me/amiga
VOLUME=LIBS:/home/me/amiga/libs
```

```
# ./.volamos, in a project directory
VOLUME=SYS:/home/me/project-a/amiga
```

Running `volamos fixtures/hello` from that project directory resolves
`SYS:` to `/home/me/project-a/amiga` (the local file's mapping for the
same name wins) while `LIBS:` still resolves to
`/home/me/amiga/libs` (only the global file mentions it, so it applies
unchanged). Adding `-V SYS:/tmp/override` on the command line would
win over both, same as any other flag.

## Errors

A missing config file is not an error — most invocations won't have
one at either location. A config file that exists but can't be read,
or contains a malformed line, fails with a clear diagnostic naming the
file, the line number, and the problem:

```console
$ cat ~/.volamos
STACK=notanumber
$ volamos fixtures/hello
volamos: /home/me/.volamos: line 1: --stack expects a byte count (optionally K/M-suffixed), got "notanumber"
```

## Next steps

- [CLI Reference](CLI-Reference.md) for the exact flags these keys
  mirror.
- [Volumes and Assigns](Volumes-and-Assigns.md) for the full `-V`/`-a`
  path-resolution model `VOLUME`/`ASSIGN` entries feed into.
