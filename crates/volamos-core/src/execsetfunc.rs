//! `exec.library`'s `SetFunction` (LVO -420): patch a single entry in a
//! library's jump table at runtime, returning what used to be there.
//!
//! # Why this needed more than an LVO fill-in
//!
//! Every other registered LVO in this runtime is a 2-byte A-line opcode
//! word at `base + lvo` (see `crate::dispatch`'s module docs on the trap
//! flow) -- real AmigaOS jump tables use a full 6-byte `JMP abs.l` per
//! vector (`crate::execlib::LIB_VECTSIZE`), but volamos's fake libraries
//! never needed the extra 4 bytes since nothing ever *executes* at those
//! addresses; the host always intercepts the call first. `SetFunction`
//! breaks that assumption on purpose: its whole point is to install a
//! *real, executable* function pointer, which real guest code then
//! expects to actually run when called. Writing a real `JMP` there,
//! though, just works: the `m68k` backend already executes a bare `JMP`
//! as an ordinary instruction (no host involvement), and
//! `crate::execlib::make_library`'s `MakeLibrary` already proves the same
//! "fake jump table, real `JMP` entries" trick works for a genuinely
//! `LoadSeg`ed library's vectors.
//!
//! # The "old function" problem
//!
//! Real `SetFunction` always returns a real, directly-callable function
//! pointer -- the one a well-behaved patch chains to for default
//! behavior (log an allocation, then call the real `AllocMem`). That's
//! easy when the entry being replaced already holds a previous
//! `SetFunction`-installed `JMP` (just read the embedded target back
//! out), but the *first* `SetFunction` on a given LVO finds a 2-byte
//! A-line trap there, pointing at a host-native handler with no real
//! guest address at all.
//!
//! The fix: synthesize a tiny stub -- a verbatim copy of whatever 6 raw
//! bytes previously occupied the entry (the trap opcode plus whatever
//! padding followed it) -- in a dedicated scratch pool ([`STUB_POOL_BASE`]),
//! and hand back *its* address as the "old" function pointer. Calling
//! that address behaves exactly like calling the un-patched entry would
//! have: the CPU can't execute the copied A-line opcode, traps, and
//! dispatch resolves the slot number (embedded in the opcode's low 12
//! bits, independent of *which* address it was read from) to the
//! original handler, exactly as if the call had landed on the real
//! entry. No separate bookkeeping table is needed to recognize a
//! restore later, either: a stub is only ever synthesized once per LVO
//! (every subsequent `SetFunction` on that same entry finds a real `JMP`
//! there -- to a guest hook, or back to this same stub -- and just reads
//! its target back out), so recognizing "is this a stub" never comes up;
//! the entry's own content always says what to do next.
//!
//! # Known limitation
//!
//! A fake (vamos-escape-hatch) library's whole jump-table block is one
//! repeated trap opcode with no inter-entry padding (see
//! `crate::dispatch::FAKE_LIB_JUMP_TABLE_SIZE`'s docs) -- writing a
//! 6-byte `JMP` at one of its LVOs clobbers the next one's opcode word
//! two bytes over. Patching a *registered* library's LVO (every
//! `dos.library`/`exec.library`/etc. entry this runtime implements) is
//! safe: those are laid out on the real 6-byte-per-vector convention
//! with genuine padding between entries (see
//! `crate::dispatch::EXEC_LIBRARY_BASE`'s "Reserved-region memory map"
//! doc), so a 6-byte write only ever overwrites padding, never a
//! neighboring live entry.

use crate::cpu::{AddressRegister, Cpu, DataRegister};
use crate::dispatch::{DispatchError, EXEC_LIBRARY_BASE, HandlerContext, LibraryTable};
use crate::lvos::exec::EXEC_LVOS;
use crate::memory::AddressSpace;

/// The `JMP` opcode word for absolute-long addressing mode -- same value
/// and encoding as `crate::execlib::JMP_ABS_L_OPCODE`, duplicated rather
/// than shared (small, self-contained constant; see `crate::execlib`'s
/// own precedent for duplicating tiny cross-module constants like
/// `NT_LIBRARY` rather than threading a dependency between unrelated
/// phases).
const JMP_ABS_L_OPCODE: u16 = 0x4EF9;

/// Byte size of one stub slot: a `JMP abs.l` instruction's worth (one
/// opcode word, one longword), matching `crate::execlib::LIB_VECTSIZE`
/// -- big enough to hold a verbatim copy of any entry this runtime ever
/// writes, real `JMP abs.l` included.
const STUB_SLOT_SIZE: u32 = 6;

/// Number of distinct LVOs this runtime can `SetFunction`-patch for the
/// first time in one run. Generous for the kind of patching a real
/// debugging/hooking tool does (a handful of specific calls, not
/// hundreds) -- see the module doc's "known limitation" for why reuse
/// isn't bounded by *call* count, only by how many distinct entries ever
/// see a first patch.
const STUB_POOL_SLOTS: u32 = 64;

/// Guest address of the `SetFunction` stub pool -- scratch space, not a
/// library base. Placed in the chunk `crate::backend::TRAP_TABLE_SIZE`
/// was grown to fit, right after `mathieeesingtrans.library`'s chunk
/// (see that constant's doc for the full growth history).
pub const STUB_POOL_BASE: u32 = 0x2A00;

/// Byte size of the stub pool: [`STUB_POOL_SLOTS`] slots of
/// [`STUB_SLOT_SIZE`] bytes each, rounded up to the `0x200` chunk size
/// every other library base in this region uses (`64 * 6 = 384 =
/// 0x180`, rounded up to `0x200`).
const STUB_POOL_SIZE: u32 = 0x200;

/// Registers `exec.library`'s `SetFunction` (LVO -420).
///
/// Reads `A1` (library base), `A0` (new function entry), `D0` (signed
/// `funcOffset`, matching `register`'s own `base.wrapping_add(lvo as
/// u32)` convention for turning an LVO into an address). Always
/// installs a real 6-byte `JMP abs.l new_entry` at the target address;
/// returns in `D0` whatever function pointer was effectively being
/// called there before -- see the module doc for how that's recovered
/// or synthesized.
pub fn register_execsetfunc_handlers<C: Cpu + 'static>(
    table: &mut LibraryTable<C>,
    mem: &mut C::Memory,
) {
    let mut next_stub = STUB_POOL_BASE;

    table
        .register_by_name(
            mem,
            EXEC_LIBRARY_BASE,
            EXEC_LVOS,
            "exec.library",
            "SetFunction",
            move |ctx: &mut HandlerContext<'_, C>| -> Result<(), DispatchError> {
                let base = ctx.cpu.address_register(AddressRegister(1));
                let new_entry = ctx.cpu.address_register(AddressRegister(0));
                let func_offset = ctx.cpu.data_register(DataRegister(0)) as i32;
                let target = base.wrapping_add(func_offset as u32);

                let old_entry = if ctx.mem.read_u16(target) == JMP_ABS_L_OPCODE {
                    // Already a real JMP -- a previous SetFunction (ours or
                    // the guest's own earlier patch) installed it. Its
                    // embedded target is the real "old function" to chain
                    // to.
                    ctx.mem.read_u32(target.wrapping_add(2))
                } else {
                    // First-ever patch of this entry: synthesize a stub
                    // that's a verbatim copy of what's there now (a 2-byte
                    // trap opcode plus whatever padding follows it for
                    // every registered LVO this runtime handles natively).
                    if next_stub.wrapping_add(STUB_SLOT_SIZE) > STUB_POOL_BASE + STUB_POOL_SIZE {
                        return Err(DispatchError::HandlerFailed {
                            library: "exec.library".to_string(),
                            lvo: -420,
                            handler_name: "SetFunction".to_string(),
                            message: format!(
                                "stub pool exhausted -- only {STUB_POOL_SLOTS} distinct LVOs \
                                 can be SetFunction-patched for the first time in one run"
                            ),
                        });
                    }
                    let stub = next_stub;
                    next_stub += STUB_SLOT_SIZE;

                    let word0 = ctx.mem.read_u16(target);
                    let rest = ctx.mem.read_u32(target.wrapping_add(2));
                    ctx.mem.write_u16(stub, word0);
                    ctx.mem.write_u32(stub.wrapping_add(2), rest);
                    stub
                };

                ctx.mem.write_u16(target, JMP_ABS_L_OPCODE);
                ctx.mem.write_u32(target.wrapping_add(2), new_entry);

                ctx.cpu.set_data_register(DataRegister(0), old_entry);
                Ok(())
            },
        )
        .expect("SetFunction is in EXEC_LVOS");
}
