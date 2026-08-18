//! End-to-end sanity check: load `fixtures/hello` through the real
//! `volamos_core::loader` and verify the bytes landed where expected.
//!
//! This doesn't run the program (no CPU is wired up here); it just
//! proves the loader parses and lays out a real hunk executable
//! correctly: two hunks (CODE + DATA), one HUNK_RELOC32 fixup, and the
//! entry point's first few instruction words matching the hand-assembled
//! opcodes documented in `fixtures/README.md` and `fixtures/gen_hello.py`.

use volamos_core::loader::{self, HunkKind};
use volamos_core::memory::{AddressSpace, FlatMemory};

/// The committed fixture binary, embedded at compile time.
const HELLO: &[u8] = include_bytes!("../../../fixtures/hello");

/// dos.library's PutStr LVO offset, as a 16-bit two's-complement
/// displacement (see fixtures/README.md).
const LVO_PUTSTR_DISP16: u16 = 0xFC4C;

#[test]
fn hello_fixture_parses_as_two_hunks() {
    let file = loader::parse(HELLO).expect("fixtures/hello should be a well-formed hunk file");

    assert_eq!(
        file.hunks.len(),
        2,
        "expected one CODE hunk and one DATA hunk"
    );
    assert_eq!(file.hunks[0].kind, HunkKind::Code);
    assert_eq!(file.hunks[1].kind, HunkKind::Data);

    // Exactly one relocation: the immediate operand of `move.l #msg,d1`
    // at offset 2 in the code hunk, targeting the data hunk (index 1).
    assert_eq!(file.hunks[0].relocs.len(), 1);
    assert_eq!(file.hunks[0].relocs[0].offset, 2);
    assert_eq!(file.hunks[0].relocs[0].target_hunk, 1);
    assert!(file.hunks[1].relocs.is_empty());

    // The message, including its NUL terminator.
    assert_eq!(file.hunks[1].data, b"Hello from volamos\n\0");
}

#[test]
fn hello_fixture_loads_and_relocates_correctly() {
    let file = loader::parse(HELLO).unwrap();

    let base = 0x1000;
    let mut mem = FlatMemory::new(0x2000);
    let result = loader::load(&file, &mut mem, base).expect("fixture should load cleanly");

    assert_eq!(result.hunk_addrs.len(), 2);
    let code_addr = result.hunk_addrs[0];
    let data_addr = result.hunk_addrs[1];
    assert_eq!(
        result.entry, code_addr,
        "entry point is hunk 0's load address"
    );

    // Hunks are packed back to back: code hunk is 16 bytes (4
    // longwords), so the data hunk should immediately follow.
    assert_eq!(code_addr, base);
    assert_eq!(data_addr, base + 16);

    // --- Verify the instruction stream at the entry point ---
    //
    // move.l #<imm32>,d1  (opcode word, then the relocated address of
    // the message).
    assert_eq!(mem.read_u16(code_addr), 0x223C);
    assert_eq!(
        mem.read_u32(code_addr + 2),
        data_addr,
        "the move.l immediate should have been relocated to the data hunk's load address"
    );

    // jsr -948(a6)  (i.e. jsr _LVOPutStr(a6))
    assert_eq!(mem.read_u16(code_addr + 6), 0x4EAE);
    assert_eq!(mem.read_u16(code_addr + 8), LVO_PUTSTR_DISP16);

    // moveq #0,d0
    assert_eq!(mem.read_u16(code_addr + 10), 0x7000);

    // rts
    assert_eq!(mem.read_u16(code_addr + 12), 0x4E75);

    // --- Verify the relocated string is where the code now points ---
    let mut bytes = Vec::new();
    let mut addr = data_addr;
    loop {
        let b = mem.read_u8(addr);
        bytes.push(b);
        if b == 0 {
            break;
        }
        addr += 1;
    }
    assert_eq!(bytes, b"Hello from volamos\n\0");
}
