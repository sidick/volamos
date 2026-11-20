# iconprobe

A small C program that prints what `icon.library` does in the cases volamos
had to guess (issue #313): default icons with no `ENV:Sys/def_*.info`, bad and
missing `.info` files, `GetDiskObject(NULL)`, `GetDiskObjectNew`'s type test,
`PutDiskObject` paths, `FindToolType` flags and `BumpRevision` /
`BumpRevisionLength`. Run the same binary on a real Kickstart and under
volamos and compare.

```sh
./build.sh                  # needs m68k-amigaos-gcc
volamos --cpu 68020 -V SYS:/tmp/ip tools/iconprobe/iconprobe SYS:
```

On real Kickstart 3.1 with Copperline
(`ENV:` has to exist, so mount an empty host directory as it; the program
writes beside itself, and `--run-args` redirects its output into a file):

```sh
mkdir -p /tmp/hw /tmp/env && cp iconprobe /tmp/hw/
printf '[[filesys]]\npath = "/tmp/env"\nvolume = "ENV"\n' > /tmp/hw.toml
copperline --model A1200 --cpu 68020 --no-fpu --fast 8M KICK31.rom \
  --config /tmp/hw.toml --run /tmp/hw/iconprobe \
  --run-args 'RunProg: >RunProg:out.txt' --exit-on-return --noaudio
```

Kickstart 3.2 has no `icon.library` in ROM: mount a copy of a 3.2
installation's `Libs` as `LIBS:` as well (it needs `workbench.library` too).

`results-*.txt` are the outputs this produced, and what volamos's
`icon.library` was checked against. The pictures in the built-in default
icons are Commodore's and are not reproduced anywhere in the tree.
