# Volumes and Assigns

AmigaOS paths (`SYS:work/foo`) don't mean anything to a host OS. `-V`
and `-a` (see [CLI Reference](CLI-Reference.md)) build a mapping from
Amiga volume/assign names onto real host directories — the same way
mounting a device or running `ASSIGN` does on a real Amiga — so a guest
program's `Open`/`Lock`/`Examine`/etc. calls resolve to real files on
your machine.

If none of `-V`/`-a`/`--cwd`/`--auto-assign` are given at all, no
filesystem is installed: path-based calls fail cleanly with an
`IoErr()`, while everything else (`Input`/`Output`/`PutStr`/...) still
works — see [Getting Started](Getting-Started.md)'s first two examples.

## Amiga path syntax

An Amiga path is `[Vol:]component[/component...]`:

- A path containing `:` splits into a volume/assign name (before the
  `:`) and the rest of the path (after it). A colon with nothing
  before it (`:work`) means "root of the current volume" — the current
  directory's own volume/assign name is reused.
- A path with no `:` at all is relative to the current directory
  (`--cwd`, or wherever the guest last set it via `CurrentDir()`).
- The rest of the path is split on `/`. A non-empty component descends
  into that subdirectory. An **empty** component — from a leading `/`,
  a doubled `//`, or (for a relative path) any `/` before the first
  non-empty component — pops one level, i.e. means "parent directory".
  This is the real AmigaOS convention: `/` plays the role Unix gives to
  `..`, so `Vol:a/b//c` means `Vol:a/c` (from `a/b`, `//` pops back up
  to `a`, then descends into `c`).

## Mapping a volume directly: `-V`

```sh
volamos -V SRC:/home/me/project fixtures/hello
```

`SRC:` now resolves straight to `/home/me/project` on the host. Give
`-V` multiple times to map several volumes in one run:

```sh
volamos -V SRC:/home/me/project -V DEST:/tmp/out fixtures/hello
```

## Assigning a name to one or more Amiga paths: `-a`

`-a NAME:target[+target...]` maps a logical name onto one or more
*Amiga* path targets (each itself an already-mapped volume, another
assign, or a subdirectory of one) — not a host path directly. This is
the real `ASSIGN NAME: target1 ADD target2 ...` idiom: a single
logical name a guest program can reference, backed by a search order
across multiple real locations.

```sh
volamos -V SYS:/home/me/amiga -a LIBS:SYS:libsA+SYS:libsB fixtures/hello
```

Here `LIBS:` searches `SYS:libsA` first, then `SYS:libsB`, for any
path a guest program opens under `LIBS:`.

**Search order** (for reading/opening an existing file — `Open`,
`Lock`, `Examine`, ...): each target is tried in list order, and the
first one where the remaining path actually resolves wins. **Creating**
a new file (`Open(..., MODE_NEWFILE)`, `CreateDir`) always uses the
*first* target only — a multi-assign never searches to decide where a
brand-new file goes, matching `vamos`'s own behavior.

An assign target can itself be another assign, resolved recursively —
useful for layering (`SUBLIBS:` -> `LIBS:sub` -> `SYS:libs/sub`). A
cycle (an assign that, through however many levels, ends up
referencing itself) is detected and reported rather than looping
forever.

## Setting the initial current directory: `--cwd`

```sh
volamos -V SRC:/home/me/project --cwd SRC:subdir fixtures/hello
```

Default, if `--cwd` isn't given: the first `-V` volume's root if any
`-V` was given, else the first `-a` assign's root, else `root:`
(relying on `--auto-assign` to resolve it).

## Catch-all fallback: `--auto-assign`

```sh
volamos --auto-assign /home/me/amiga-volumes fixtures/hello
```

Any volume/assign name volamos doesn't otherwise know about resolves
to `<HOSTDIR>/<NAME>` automatically — so `LIBS:`, `SYS:`, `T:`, or
whatever names a guest program happens to reference all just work as
subdirectories of one fallback root, without needing an explicit
`-V`/`-a` for each one. This mirrors `vamos`'s own auto-assign
fallback. Without `--auto-assign` configured, referencing an unknown
volume/assign name is a clean `IoErr()`, not a crash.

## Case sensitivity

Real AmigaOS filesystems are case-insensitive; most host filesystems
(macOS's default, and Linux) are case-sensitive. volamos matches each
path component against a real directory listing on the host,
preferring (in order): an exact-case match, else a *unique*
case-insensitive match, else — if there are multiple case-insensitive
matches for the same name — a deterministic (byte-sorted) tie-break.
Creating a new file/directory that doesn't exist yet preserves
whatever case the guest program asked for, rather than erroring.

## Next steps

- [CLI Reference](CLI-Reference.md) for the exact flag syntax.
- [Supported Libraries](Supported-Libraries.md) for exactly which
  `dos.library` path-based calls this filesystem model backs.
