#!/bin/sh
# Builds iconprobe for AmigaOS 3.x with Bebbo's m68k-amigaos-gcc.
set -e
cd "$(dirname "$0")"
export PATH=/opt/m68k-amigaos-gcc-16.2/bin:$PATH
m68k-amigaos-gcc -noixemul -O1 -m68020 -o iconprobe iconprobe.c
