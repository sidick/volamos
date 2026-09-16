# Differences from vamos

volamos is a spiritual successor to
[`vamos`](https://github.com/cnvogelg/amitools), the Python
implementation of the same idea, and the two are compared against each
other continuously — `tools/compare_vamos.py` and
`tools/compare_amitools_suite.py` run the same binaries through both and
fail on any unexplained disagreement.

Most disagreements that surface turn out to be volamos bugs, and get
fixed. This page documents the ones that went the other way: cases where
the two runtimes genuinely differ and **volamos's behaviour has been
verified against real Amiga hardware**.

These are recorded rather than "fixed" because there is nothing to fix on
volamos's side. If you are porting a test or a workflow from `vamos` and
hit one of these, this page is the explanation.

## How these were verified

Every entry below was checked against a real Kickstart running under
[Copperline](https://github.com/sidick/copperline), booting a real
Workbench 3.1.4 filesystem, with the guest's own Shell redirecting the
program's output to a host-visible file — so the comparison is against
actual `dos.library`/`utility.library`/`mathieeedoubbas.library` ROM
code, not against either runtime's idea of what the ROM does.

That matters more than it might sound. The NDK autodocs describe
*intent*, and at least one entry below is a case where the documentation
describes behaviour real AmigaOS does not implement — so
documentation alone was not treated as sufficient evidence.

`tools/compare_three_way.py` automates this three-way comparison
(volamos / `vamos` / real Kickstart). Note it needs a `--model` matching
the `--rom` you give it; a mismatched pair boots nothing and silently
produces an empty real-Kickstart column.

## The differences

### `WriteChars()` and a trailing NUL

```c
char msg[] = "Hello, world!?\n";
WriteChars(msg, sizeof(msg));      /* sizeof includes the implicit NUL */
```

`WriteChars` is a raw byte-count write. Real `dos.library` writes exactly
the count it is given, so the trailing NUL that `sizeof` legitimately
includes **is** written:

```
00000000: 4865 6c6c 6f2c 2077 6f72 6c64 213f 0a48  Hello, world!?.H
00000010: 656c 6c6f 2c20 776f 726c 6421 3f0a 00    ello, world!?..
```

volamos writes all 16 bytes, matching hardware. `vamos` drops the NUL.

### `CheckDate()` does not validate `wday`

`utility.library/CheckDate`'s own autodoc carries this under **BUGS**:

> The wday field of the ClockData structure is not checked.

So a `ClockData` with `wday = 7` — outside the valid 0–6 range — is still
accepted as a valid date. volamos matches that (and real Kickstart 3.1
confirms it); `vamos` rejects the date.

This is the entry worth remembering when weighing documentation against
hardware: here the documentation *documents a bug*, and the bug is the
real behaviour.

### Sign of a quiet NaN from a math domain error

For results that are mathematically undefined — `0.0/0.0`, `acos`/`asin`
outside `[-1,1]`, `log` of a negative number — both runtimes return a
quiet NaN, but with opposite sign bits. Real
`mathieeedoubbas.library`'s `IEEEDPDiv(0.0, 0.0)` returns a
**positive-signed** NaN regardless of input sign, which is what volamos
returns; `vamos` returns a negative-signed one.

The NaN *payload* bits below the sign differ between real hardware and
volamos, and that is treated as implementation-specific detail, in the
same category as ordinary transcendental-function rounding noise.

### The command line carries a trailing space

When a launched program has at least one argument, real AmigaOS's
command-line buffer carries a trailing space before the final newline —
`"foo bar baz \n"`, not `"foo bar baz\n"`. Confirmed against real
Kickstart 2.0, 3.0 and 3.1 hardware.

volamos reproduces this; `vamos` does not, so any test asserting on the
exact command-line bytes will differ by that one space.

Kickstart 3.2 alone drops the trailing space — a real, intentional
AmigaOS version difference rather than a bug. volamos targets 3.1 first
and follows 3.1 here.

### `APF_DirChanged` during a `MatchFirst`/`MatchNext` scan

`dos/dosasl.h` defines the flag as "`ap_Current->an_Lock` changed since
last `MatchNext` call". Real `dos.library` sets it on the first entry
reported after descending into a directory, clears it for subsequent
entries in that same directory, and sets it again when leaving.

volamos implements exactly that. `vamos` never sets the flag, so a
program that drives its behaviour off directory transitions — the real
`List` command uses it to decide when to print a new `Directory "..."`
header — has nothing to work from.

## What is *not* on this page

Differences where volamos is the one that is wrong get fixed rather than
documented, so they do not appear here. Some are still open as bugs — see
the issue tracker — and a few known ones are listed as visible failures
in the comparison harnesses' own tables rather than hidden.

`vamos` also does not read volamos's `.uaem` sidecar metadata (protection
bits, comments, timestamps), so any comparison whose output depends on
dates or protection bits is not meaningful between the two runtimes and
is scoped out of the harnesses rather than tracked here.
