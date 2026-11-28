#!/usr/bin/env python3
"""Generates fixtures/rawkeytest: opens `console.device` unit -1
(`CONU_LIBRARY`, no window) the way the Amiga Installer does, and
converts keys with `RawKeyConvert` through the `io_Device` it got back
(issue #425).

1. `OpenDevice("console.device", -1, ioreq, 0)`; exit 20 if it fails.
2. One `InputEvent`, re-used: Shift+`h` (raw 0x25), `i` (0x17) and
   Shift+`1` (0x01) converted one after another into `buf`, each with a
   default keymap (`A2` = NULL); each must return 1, or exit 10.
3. F1 (0x50, `CSI 0 ~`, three bytes) into a two-byte buffer must return
   -1 (buffer overflow), or exit 11.
4. `PutStr(buf)`, so stdout is `Hi!\\n`; `CloseDevice`; exit 0.

The `InputEvent` is the first thing in the data hunk: the hunk is
longword-aligned, `DataBuilder.u32s()` doesn't align, and the device
reads the event's words and longwords. Same output on real Kickstart 3.1
(40.63, A600, 68000) under Copperline.

Run directly to (re)write fixtures/rawkeytest:

    python3 fixtures/gen_rawkeytest.py
"""

import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from amiga_asm import CodeBuilder, DataBuilder, build_hunk_executable  # noqa: E402

HERE = pathlib.Path(__file__).resolve().parent
OUT_PATH = HERE / "rawkeytest"

# LVOs used (see crates/volamos-core/src/lvos/{dos,exec}.rs and NDK
# console_lib.i):
LVO_OPENLIBRARY = -552
LVO_OPENDEVICE = -444
LVO_CLOSEDEVICE = -450
LVO_PUTSTR = -948
LVO_RAWKEYCONVERT = -48

CONU_LIBRARY = -1
IO_DEVICE = 20  # struct IORequest's io_Device
IOSTDREQ_SIZE = 48
IE_CODE = 6  # struct InputEvent's ie_Code
IE_QUALIFIER = 8
IECLASS_RAWKEY = 0x01
IEQUALIFIER_LSHIFT = 0x0001

A0, A1, A2, A3, A4, A5, A6 = 0, 1, 2, 3, 4, 5, 6
D0, D1 = 0, 1


def build_program() -> bytes:
    data = DataBuilder()
    # struct InputEvent (22 bytes, padded to 24): ie_NextEvent = NULL,
    # ie_Class = IECLASS_RAWKEY, everything else filled in per key.
    data.u32s("event", [0, IECLASS_RAWKEY << 24, 0, 0, 0, 0])
    data.zeros("ioreq", IOSTDREQ_SIZE)
    data.cstr("dosname", "dos.library")
    data.cstr("consolename", "console.device")
    data.cstr("buf", "...\n")

    code = CodeBuilder()

    code.label("start")
    code.move_l_abs4_to_a(A6)  # A6 = AbsExecBase
    code.move_l_a_to_a(A5, A6)  # A5 = exec, for the device calls
    code.move_l_label_to_a(A1, "dosname")
    code.moveq(D0, 0)
    code.jsr_disp16_a(A6, LVO_OPENLIBRARY)
    code.move_l_d_to_a(A4, D0)  # A4 = dos.library

    # OpenDevice("console.device", -1, ioreq, 0)
    code.move_l_label_to_a(A0, "consolename")
    code.moveq(D0, CONU_LIBRARY)
    code.move_l_label_to_a(A1, "ioreq")
    code.moveq(D1, 0)
    code.jsr_disp16_a(A6, LVO_OPENDEVICE)
    code.tst_l_d(D0)
    code.branch(CodeBuilder.BEQ, "opened")
    code.moveq(D0, 20)
    code.rts()

    code.label("opened")
    code.move_l_label_to_a(A0, "ioreq")
    code.move_l_disp_a_to_d(A0, IO_DEVICE, D0)
    code.move_l_d_to_a(A6, D0)  # A6 = ConsoleDevice
    code.move_l_label_to_a(A3, "event")

    # Shift+h, i, Shift+1 -> "Hi!" in buf[0..3].
    keys = [(0x25, IEQUALIFIER_LSHIFT), (0x17, 0), (0x01, IEQUALIFIER_LSHIFT)]
    for i, (raw, qual) in enumerate(keys):
        code.move_w_imm_to_disp_a(A3, IE_CODE, raw)
        code.move_w_imm_to_disp_a(A3, IE_QUALIFIER, qual)
        code.move_l_a_to_a(A0, A3)
        code.move_l_label_to_a(A1, "buf", i)
        code.move_l_imm_to_d(D1, 8)
        code.move_l_imm_to_a(A2, 0)  # default keymap
        code.jsr_disp16_a(A6, LVO_RAWKEYCONVERT)
        code.cmpi_l_imm_to_d(D0, 1)
        code.branch(CodeBuilder.BNE, "badkey")

    # F1 into two bytes: -1, nothing written.
    code.move_w_imm_to_disp_a(A3, IE_CODE, 0x50)
    code.move_w_imm_to_disp_a(A3, IE_QUALIFIER, 0)
    code.move_l_a_to_a(A0, A3)
    code.move_l_label_to_a(A1, "buf", 3)
    code.move_l_imm_to_d(D1, 2)
    code.move_l_imm_to_a(A2, 0)
    code.jsr_disp16_a(A6, LVO_RAWKEYCONVERT)
    code.cmpi_l_imm_to_d(D0, -1)
    code.branch(CodeBuilder.BNE, "nooverflow")

    code.move_l_a_to_a(A6, A4)
    code.move_l_label_to_d(D1, "buf")
    code.jsr_disp16_a(A6, LVO_PUTSTR)

    code.move_l_a_to_a(A6, A5)
    code.move_l_label_to_a(A1, "ioreq")
    code.jsr_disp16_a(A6, LVO_CLOSEDEVICE)
    code.moveq(D0, 0)
    code.rts()

    code.label("badkey")
    code.moveq(D0, 10)
    code.rts()

    code.label("nooverflow")
    code.moveq(D0, 11)
    code.rts()

    return build_hunk_executable(code, data)


def main() -> None:
    program = build_program()
    OUT_PATH.write_bytes(program)
    print(f"wrote {OUT_PATH} ({len(program)} bytes)")


if __name__ == "__main__":
    main()
