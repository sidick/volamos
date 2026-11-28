# rawkeyprobe

A small C program that records what `console.device`'s `RawKeyConvert` does
(issue #425): every raw key code under twenty qualifier sets (none, each
Shift, Caps Lock, Ctrl, each Alt, each Amiga key, the numeric-pad qualifier,
key repeat and some combinations), every raw code after each of the five dead
keys with and without Shift, key-up, non-key and out-of-range events, buffer
overflow, and three `ie_NextEvent` chains. Run the same binary on a real Kickstart and under
volamos and compare.

```sh
./build.sh                  # needs m68k-amigaos-gcc
volamos tools/rawkeyprobe/rawkeyprobe
```

Under volamos the run stops at the first chain, by design: volamos fails the
call rather than converting a chain (see `crates/volamos-core/src/console.rs`).
Everything before it matches real Kickstart 3.1 line for line.

On real Kickstart 3.1 with Copperline, run it from a `Startup-Sequence`
before any `SetMap`, so the ROM's default keymap is in force. Mount a host
directory holding the binary and an `S/Startup-Sequence` of

```
TEST:rawkeyprobe >TEST:RESULT.TXT
```

as a bootable `TEST` volume, plus an empty host directory as `ENV`:

```toml
[[filesys]]
path = "/tmp/rkp"
volume = "TEST"
bootpri = 6

[[filesys]]
path = "/tmp/env"
volume = "ENV"
```

```sh
copperline --model A600 --config rkp.toml --noaudio KICK31.rom
```

Some chain shapes hang real Kickstart 3.1, a key event followed by an
`IECLASS_NULL` event with code 0 among them, so the probe sticks to the three
that don't.

`crates/volamos-core/tests/data/console-rawkeyconvert-ks31.txt` is this
program's output on Kickstart 3.1 (40.63, A600, 68000). volamos's keymap
table was generated from it, and a unit test checks the table against every
row.
