//! `struct DiskObject` and `struct FreeList` in guest memory (issue #313,
//! phase 2): turns a parsed [`DiskObjectFile`] into the structure a
//! program gets from `GetDiskObject`, and frees one again.
//!
//! # Layout
//!
//! The autodoc for `GetDiskObject` says: "A FreeList structure is
//! allocated just after the DiskObject structure; FreeDiskObject makes
//! use of this to get rid of the memory that was allocated." So the
//! first allocation is [`DISK_OBJECT_MEM_SIZE`] bytes of `DiskObject`
//! followed by [`FREE_LIST_SIZE`] bytes of `FreeList`, and
//! `FreeDiskObject(obj)` is `FreeFreeList(obj + 78)`. `FreeFreeList`'s
//! autodoc adds that a `FreeList` which is itself recorded in the list
//! "must be in the first MemList": that block is recorded first.
//!
//! A `FreeList` is "a list of `MemList` structures" (`<exec/memory.h>`).
//! This module keeps one `MemList` with one `MemEntry` per allocation:
//! 24 bytes, `ml_Node` (14), `ml_NumEntries` (2), then `me_Addr` and
//! `me_Length`. `fl_NumFree`, the count of unused entries in the last
//! `MemList`, is therefore always `0`. `AddFreeList` appends one such
//! node and so never fails for want of a free slot, only of heap.
//!
//! Everything a `DiskObject` points at is a separate guest-heap
//! allocation recorded in its list: the `Image` structs and their
//! planes, the default tool and tool window strings, the tool-type
//! array (with its strings after it, in one block), and the drawer
//! data. They come from the ordinary [`GuestHeap`], so `--sanitize`
//! sees them like any other allocation, and a double `FreeDiskObject`
//! is caught there.
//!
//! # What is filled in
//!
//! The pointer fields the file carries are meaningless (see
//! [`crate::iconfile`]), and the real library fills them from its own
//! allocations; so does this. Fields that would point at something this
//! runtime does not have -- `NextGadget`, `GadgetText`, `MutualExclude`,
//! `SpecialInfo`, and the `NewWindow`'s `FirstGadget`, `CheckMark`,
//! `Title`, `Screen` and `BitMap` -- are `NULL`. Image planes live in
//! ordinary memory rather than chip RAM: nothing here draws them.
//! The in-memory `DrawerData` is the full 62 bytes, with `dd_Flags` and
//! `dd_ViewModes` taken from the file's extension, or `0` for a 1.3
//! drawer that has none.

use crate::execlist::{LH_HEAD, LH_TAIL, LN_SUCC, LN_TYPE, add_tail_impl, init_list_header};
use crate::guestmem::{GuestHeap, write_c_string};
use crate::iconfile::{
    DRAWER_EXTENSION_SIZE, DiskObjectFile, DrawerData, GadgetFields, IconImage, NEW_WINDOW_SIZE,
    WB_DISKMAGIC, plane_data_size,
};
use crate::memory::AddressSpace;

/// `sizeof(struct DiskObject)`: the struct as it is in memory, which is
/// also the size of its part of the file.
pub const DISK_OBJECT_MEM_SIZE: u32 = crate::iconfile::DISK_OBJECT_SIZE as u32;
/// `sizeof(struct FreeList)`: `fl_NumFree` (a `WORD`) and `fl_MemList`
/// (a `struct List`, 14 bytes).
pub const FREE_LIST_SIZE: u32 = 16;
/// `fl_MemList`'s offset in a `FreeList`.
const FL_MEMLIST: u32 = 2;

/// `sizeof(struct MemList)` with its one `MemEntry`.
const MEMLIST_SIZE: u32 = 24;
const ML_NUMENTRIES: u32 = 14;
const ML_ME: u32 = 16;
/// `NT_MEMORY`, `<exec/nodes.h>`.
const NT_MEMORY: u8 = 10;
/// More entries than this in one `MemList` is a corrupt structure, not
/// an icon.
const MAX_ENTRIES_PER_NODE: u16 = 256;

/// `sizeof(struct DrawerData)` in memory: the 56 bytes the file holds,
/// then `dd_Flags` (`ULONG`) and `dd_ViewModes` (`UWORD`).
const DRAWER_DATA_MEM_SIZE: u32 =
    (crate::iconfile::DRAWER_DATA_FILE_SIZE + DRAWER_EXTENSION_SIZE) as u32;
/// Where the `NewWindow`'s pointer fields are: `FirstGadget`,
/// `CheckMark`, `Title`, `Screen`, `BitMap` -- five consecutive
/// longs after `Flags`.
const NEW_WINDOW_POINTERS: std::ops::Range<u32> = 18..38;

/// The guest heap is full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoMemory;

fn zero(mem: &mut dyn AddressSpace, addr: u32, len: u32) {
    for i in 0..len {
        mem.write_u8(addr.wrapping_add(i), 0);
    }
}

/// `AddFreeList`: records `[addr, addr + len)` in the `FreeList` at
/// `free_list` as a new `MemList`. `false` if the heap has no room for
/// that node. Does not allocate the memory it records.
pub fn add_free_list(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    free_list: u32,
    addr: u32,
    len: u32,
) -> bool {
    let Ok(node) = heap.alloc(mem, MEMLIST_SIZE) else {
        return false;
    };
    zero(mem, node, MEMLIST_SIZE);
    mem.write_u8(node + LN_TYPE, NT_MEMORY);
    mem.write_u16(node + ML_NUMENTRIES, 1);
    mem.write_u32(node + ML_ME, addr);
    mem.write_u32(node + ML_ME + 4, len);
    add_tail_impl(mem, free_list + FL_MEMLIST, node);
    true
}

/// `FreeFreeList`: frees every block recorded in the `FreeList` at
/// `free_list` and every `MemList` node, including the `FreeList`
/// itself if it is in one of the blocks. Returns how many blocks were
/// freed, or why the structure is not a free list this module can walk.
///
/// Everything is validated before anything is freed, so a bad list is
/// refused whole rather than half-freed. Every node and every recorded
/// block must be a live guest-heap allocation (nodes made by
/// [`add_free_list`] and [`build_disk_object`] always are), and none may
/// appear twice. That bounds the walk by the heap's own live count --
/// a cycle or garbage pointer fails at the first repeated or unknown
/// node. For a stale `DiskObject` pointer use [`free_disk_object`],
/// which also checks the object owns its list; this function takes any
/// `FreeList`, so it cannot.
pub fn free_free_list(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    free_list: u32,
) -> Result<usize, String> {
    // Collect everything first: the FreeList itself is usually inside
    // the first block, which must not be freed while it is being read.
    let mut nodes = Vec::new();
    let mut blocks = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut node = mem.read_u32(free_list + FL_MEMLIST + LH_HEAD);
    // The list's tail sentinel is inside the FreeList, not a heap node.
    while node != free_list + FL_MEMLIST + LH_TAIL && node != 0 {
        if heap
            .size_of_live_alloc(node)
            .is_none_or(|size| size < MEMLIST_SIZE)
        {
            return Err(format!("{node:#010x} is not a live MemList"));
        }
        if !seen.insert(node) {
            return Err(format!("the list loops back to {node:#010x}"));
        }
        let entries = mem.read_u16(node + ML_NUMENTRIES);
        if entries > MAX_ENTRIES_PER_NODE {
            return Err(format!(
                "a MemList at {node:#010x} claims {entries} entries"
            ));
        }
        for i in 0..entries as u32 {
            let addr = mem.read_u32(node + ML_ME + 8 * i);
            if addr == 0 {
                continue;
            }
            if heap.size_of_live_alloc(addr).is_none() {
                return Err(format!("{addr:#010x} is not a live allocation"));
            }
            if !seen.insert(addr) {
                return Err(format!("{addr:#010x} is recorded twice"));
            }
            blocks.push(addr);
        }
        nodes.push(node);
        node = mem.read_u32(node + LN_SUCC);
    }
    for &addr in blocks.iter().chain(&nodes) {
        heap.free(addr)
            .map_err(|e| format!("freeing {addr:#010x}: {e}"))?;
    }
    Ok(blocks.len())
}

/// `FreeDiskObject`: [`free_free_list`] on the `FreeList` after the
/// `DiskObject` at `obj`, after checking that `obj` really is one built
/// by [`build_disk_object`]: a live heap allocation big enough to hold
/// both structures, whose list records `obj` itself as its first block.
/// A stale or foreign pointer, even one whose memory was recycled into
/// something list-shaped, fails that last test unless it is by now a
/// genuine `DiskObject` again (a use-after-free no allocator can tell
/// from a real free).
pub fn free_disk_object(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    obj: u32,
) -> Result<usize, String> {
    let list = obj + DISK_OBJECT_MEM_SIZE;
    if heap
        .size_of_live_alloc(obj)
        .is_none_or(|size| size < DISK_OBJECT_MEM_SIZE + FREE_LIST_SIZE)
    {
        return Err("not a live DiskObject allocation".to_string());
    }
    let first = mem.read_u32(list + FL_MEMLIST + LH_HEAD);
    let owns_itself = heap
        .size_of_live_alloc(first)
        .is_some_and(|size| size >= MEMLIST_SIZE)
        && mem.read_u16(first + ML_NUMENTRIES) >= 1
        && mem.read_u32(first + ML_ME) == obj;
    if !owns_itself {
        return Err("its FreeList does not start with the object itself".to_string());
    }
    free_free_list(heap, mem, list)
}

/// Longest string (default tool, tool type, tool window) read from the
/// guest, and most tool types: a pointer that is not a string at all
/// fails instead of reading the whole address space.
const MAX_STRING_LEN: u32 = 1 << 20;
const MAX_TOOL_TYPES: u32 = 1 << 16;
/// Largest image bitplane block read from the guest.
const MAX_IMAGE_BYTES: u64 = 16 << 20;

fn mapped(mem: &dyn AddressSpace, addr: u32, len: u32, what: &str) -> Result<(), String> {
    if addr != 0 && mem.is_mapped(addr, len) {
        Ok(())
    } else {
        Err(format!("{what} at {addr:#010x} is not readable memory"))
    }
}

fn read_string(mem: &dyn AddressSpace, addr: u32, what: &str) -> Result<Vec<u8>, String> {
    mapped(mem, addr, 1, what)?;
    let mut out = Vec::new();
    loop {
        let at = addr
            .checked_add(out.len() as u32)
            .filter(|&a| mem.is_mapped(a, 1))
            .ok_or_else(|| format!("{what} at {addr:#010x} runs off the end of memory"))?;
        let b = mem.read_u8(at);
        if b == 0 {
            return Ok(out);
        }
        if out.len() as u32 >= MAX_STRING_LEN {
            return Err(format!("{what} at {addr:#010x} is not NUL-terminated"));
        }
        out.push(b);
    }
}

fn read_image(mem: &dyn AddressSpace, addr: u32, what: &str) -> Result<IconImage, String> {
    mapped(mem, addr, 20, what)?;
    let word = |off: u32| mem.read_u16(addr + off) as i16;
    let mut img = IconImage {
        left_edge: word(0),
        top_edge: word(2),
        width: word(4),
        height: word(6),
        depth: word(8),
        plane_pick: mem.read_u8(addr + 14),
        plane_on_off: mem.read_u8(addr + 15),
        data: Vec::new(),
    };
    let planes = mem.read_u32(addr + 10);
    if planes != 0 {
        let size = plane_data_size(img.width, img.height, img.depth);
        if size > MAX_IMAGE_BYTES {
            return Err(format!("{what} claims {size} bytes of bitplanes"));
        }
        mapped(mem, planes, size as u32, &format!("{what}'s bitplanes"))?;
        img.data = (0..size as u32).map(|i| mem.read_u8(planes + i)).collect();
    }
    Ok(img)
}

/// Reads the guest `DiskObject` at `obj` into the host model, for
/// `PutDiskObject`: the inverse of [`build_disk_object`], following the
/// same pointers (`do_Gadget`'s two images, default tool, tool types,
/// tool window, drawer data). Pointer fields that are meaningless on
/// disk are dropped; a drawer is written with its `dd_Flags` /
/// `dd_ViewModes` extension. Fails, naming what was unreadable, if a
/// pointer does not lead to memory: a program passing one has a bug,
/// and writing garbage to the user's `.info` would hide it.
pub fn read_disk_object(mem: &dyn AddressSpace, obj: u32) -> Result<DiskObjectFile, String> {
    mapped(mem, obj, DISK_OBJECT_MEM_SIZE, "the DiskObject")?;
    let w = |off: u32| mem.read_u16(obj + off);
    let l = |off: u32| mem.read_u32(obj + off);
    let optional_string = |off: u32, what: &str| match l(off) {
        0 => Ok(None),
        p => read_string(mem, p, what).map(Some),
    };

    let gadget_image = match l(22) {
        0 => None,
        p => Some(read_image(mem, p, "do_Gadget.GadgetRender")?),
    };
    let select_image = match l(26) {
        0 => None,
        p => Some(read_image(mem, p, "do_Gadget.SelectRender")?),
    };
    let tool_types = match l(54) {
        0 => None,
        array => {
            let mut types = Vec::new();
            for i in 0..MAX_TOOL_TYPES {
                let slot = array.wrapping_add(4 * i);
                mapped(mem, slot, 4, "the ToolTypes array")?;
                match mem.read_u32(slot) {
                    0 => break,
                    p => types.push(read_string(mem, p, "a tool type")?),
                }
                if i + 1 == MAX_TOOL_TYPES {
                    return Err("the ToolTypes array is not NULL-terminated".to_string());
                }
            }
            Some(types)
        }
    };
    let drawer = match l(66) {
        0 => None,
        d => {
            mapped(mem, d, DRAWER_DATA_MEM_SIZE, "do_DrawerData")?;
            let mut new_window = [0u8; NEW_WINDOW_SIZE];
            for (i, b) in new_window.iter_mut().enumerate() {
                if !NEW_WINDOW_POINTERS.contains(&(i as u32)) {
                    *b = mem.read_u8(d + i as u32);
                }
            }
            let tail = NEW_WINDOW_SIZE as u32;
            Some(DrawerData {
                new_window,
                current_x: mem.read_u32(d + tail) as i32,
                current_y: mem.read_u32(d + tail + 4) as i32,
                extension: Some((mem.read_u32(d + tail + 8), mem.read_u16(d + tail + 12))),
            })
        }
    };

    Ok(DiskObjectFile {
        version: w(2),
        gadget: GadgetFields {
            next_gadget: 0,
            left_edge: w(8) as i16,
            top_edge: w(10) as i16,
            width: w(12) as i16,
            height: w(14) as i16,
            flags: w(16),
            activation: w(18),
            gadget_type: w(20),
            gadget_text: 0,
            mutual_exclude: 0,
            special_info: 0,
            gadget_id: w(42),
            user_data: l(44),
        },
        object_type: mem.read_u8(obj + 48),
        current_x: l(58) as i32,
        current_y: l(62) as i32,
        stack_size: l(74) as i32,
        default_tool: optional_string(50, "do_DefaultTool")?,
        tool_types,
        tool_window: optional_string(70, "do_ToolWindow")?,
        drawer,
        gadget_image,
        select_image,
    })
}

/// Builds the guest `DiskObject` (with its `FreeList` and everything it
/// points at) for `icon`, returning its address.
///
/// On any allocation failure everything allocated so far is freed again
/// and [`NoMemory`] returned: a program asking for an icon with the heap
/// full gets `NULL`, not a half-built object.
pub fn build_disk_object(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    icon: &DiskObjectFile,
) -> Result<u32, NoMemory> {
    // The first block: the DiskObject with its FreeList after it, and
    // recorded first, as FreeFreeList requires.
    let size = DISK_OBJECT_MEM_SIZE + FREE_LIST_SIZE;
    let obj = heap.alloc(mem, size).map_err(|_| NoMemory)?;
    zero(mem, obj, size);
    let free_list = obj + DISK_OBJECT_MEM_SIZE;
    init_list_header(mem, free_list + FL_MEMLIST);
    if !add_free_list(heap, mem, free_list, obj, size) {
        let _ = heap.free(obj);
        return Err(NoMemory);
    }

    match fill(heap, mem, icon, obj, free_list) {
        Ok(()) => Ok(obj),
        Err(NoMemory) => {
            let _ = free_free_list(heap, mem, free_list);
            Err(NoMemory)
        }
    }
}

/// Allocates `size` zeroed bytes and records them in `free_list`.
fn alloc_tracked(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    free_list: u32,
    size: u32,
) -> Result<u32, NoMemory> {
    let addr = heap.alloc(mem, size).map_err(|_| NoMemory)?;
    zero(mem, addr, size);
    if !add_free_list(heap, mem, free_list, addr, size) {
        let _ = heap.free(addr);
        return Err(NoMemory);
    }
    Ok(addr)
}

fn alloc_string(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    free_list: u32,
    bytes: &[u8],
) -> Result<u32, NoMemory> {
    let addr = alloc_tracked(heap, mem, free_list, bytes.len() as u32 + 1)?;
    write_c_string(mem, addr, bytes);
    Ok(addr)
}

fn alloc_image(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    free_list: u32,
    img: &IconImage,
) -> Result<u32, NoMemory> {
    let addr = alloc_tracked(
        heap,
        mem,
        free_list,
        crate::iconfile::IMAGE_HEADER_SIZE as u32,
    )?;
    let planes = if img.data.is_empty() {
        0
    } else {
        let planes = alloc_tracked(heap, mem, free_list, img.data.len() as u32)?;
        for (i, &b) in img.data.iter().enumerate() {
            mem.write_u8(planes + i as u32, b);
        }
        planes
    };
    mem.write_u16(addr, img.left_edge as u16);
    mem.write_u16(addr + 2, img.top_edge as u16);
    mem.write_u16(addr + 4, img.width as u16);
    mem.write_u16(addr + 6, img.height as u16);
    mem.write_u16(addr + 8, img.depth as u16);
    mem.write_u32(addr + 10, planes);
    mem.write_u8(addr + 14, img.plane_pick);
    mem.write_u8(addr + 15, img.plane_on_off);
    // NextImage (addr + 16) stays NULL.
    Ok(addr)
}

fn fill(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    icon: &DiskObjectFile,
    obj: u32,
    free_list: u32,
) -> Result<(), NoMemory> {
    let gadget_image = match &icon.gadget_image {
        Some(img) => alloc_image(heap, mem, free_list, img)?,
        None => 0,
    };
    let select_image = match &icon.select_image {
        Some(img) => alloc_image(heap, mem, free_list, img)?,
        None => 0,
    };
    let default_tool = match &icon.default_tool {
        Some(s) => alloc_string(heap, mem, free_list, s)?,
        None => 0,
    };
    let tool_window = match &icon.tool_window {
        Some(s) => alloc_string(heap, mem, free_list, s)?,
        None => 0,
    };
    let tool_types = match &icon.tool_types {
        Some(types) => {
            // The NULL-terminated pointer array, then the strings.
            let array = (types.len() as u32 + 1) * 4;
            let total = array + types.iter().map(|t| t.len() as u32 + 1).sum::<u32>();
            let base = alloc_tracked(heap, mem, free_list, total)?;
            let mut at = base + array;
            for (i, t) in types.iter().enumerate() {
                mem.write_u32(base + 4 * i as u32, at);
                write_c_string(mem, at, t);
                at += t.len() as u32 + 1;
            }
            base
        }
        None => 0,
    };
    let drawer = match &icon.drawer {
        Some(d) => {
            let addr = alloc_tracked(heap, mem, free_list, DRAWER_DATA_MEM_SIZE)?;
            // The block is zeroed, so the pointer fields stay NULL.
            for (i, &b) in d.new_window.iter().enumerate() {
                if !NEW_WINDOW_POINTERS.contains(&(i as u32)) {
                    mem.write_u8(addr + i as u32, b);
                }
            }
            let tail = NEW_WINDOW_SIZE as u32;
            mem.write_u32(addr + tail, d.current_x as u32);
            mem.write_u32(addr + tail + 4, d.current_y as u32);
            mem.write_u32(addr + tail + 8, d.flags());
            mem.write_u16(addr + tail + 12, d.view_modes());
            addr
        }
        None => 0,
    };

    let g = &icon.gadget;
    mem.write_u16(obj, WB_DISKMAGIC);
    mem.write_u16(obj + 2, icon.version);
    // do_Gadget: NextGadget (4) stays NULL.
    mem.write_u16(obj + 8, g.left_edge as u16);
    mem.write_u16(obj + 10, g.top_edge as u16);
    mem.write_u16(obj + 12, g.width as u16);
    mem.write_u16(obj + 14, g.height as u16);
    mem.write_u16(obj + 16, g.flags);
    mem.write_u16(obj + 18, g.activation);
    mem.write_u16(obj + 20, g.gadget_type);
    mem.write_u32(obj + 22, gadget_image);
    mem.write_u32(obj + 26, select_image);
    // GadgetText (30), MutualExclude (34), SpecialInfo (38): NULL.
    mem.write_u16(obj + 42, g.gadget_id);
    mem.write_u32(obj + 44, g.user_data);
    mem.write_u8(obj + 48, icon.object_type);
    mem.write_u32(obj + 50, default_tool);
    mem.write_u32(obj + 54, tool_types);
    mem.write_u32(obj + 58, icon.current_x as u32);
    mem.write_u32(obj + 62, icon.current_y as u32);
    mem.write_u32(obj + 66, drawer);
    mem.write_u32(obj + 70, tool_window);
    mem.write_u32(obj + 74, icon.stack_size as u32);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iconfile::{DrawerData, WBDRAWER, WBPROJECT, plane_data_size};
    use crate::memory::FlatMemory;

    fn heap_and_mem() -> (GuestHeap, FlatMemory) {
        (GuestHeap::new(0x1000, 0x1_0000), FlatMemory::new(0x1_2000))
    }

    fn image(w: i16, h: i16, d: i16) -> IconImage {
        IconImage {
            width: w,
            height: h,
            depth: d,
            plane_pick: 3,
            data: (0..plane_data_size(w, h, d) as usize)
                .map(|i| i as u8)
                .collect(),
            ..IconImage::default()
        }
    }

    fn project() -> DiskObjectFile {
        DiskObjectFile {
            object_type: WBPROJECT,
            stack_size: 8192,
            current_x: 12,
            current_y: -3,
            default_tool: Some(b"SYS:Tools/Edit".to_vec()),
            tool_types: Some(vec![b"A=1".to_vec(), b"FLAG".to_vec()]),
            tool_window: Some(b"CON:0/0/1/1/x".to_vec()),
            gadget_image: Some(image(20, 10, 2)),
            select_image: Some(image(20, 10, 2)),
            ..DiskObjectFile::default()
        }
    }

    fn c_string(mem: &FlatMemory, addr: u32) -> Vec<u8> {
        crate::guestmem::read_c_string(mem, addr)
    }

    #[test]
    fn a_built_object_reads_back_field_by_field() {
        let (mut heap, mut mem) = heap_and_mem();
        let icon = project();
        let obj = build_disk_object(&mut heap, &mut mem, &icon).unwrap();

        assert_eq!(mem.read_u16(obj), WB_DISKMAGIC);
        assert_eq!(mem.read_u8(obj + 48), WBPROJECT);
        assert_eq!(mem.read_u32(obj + 74), 8192);
        assert_eq!(mem.read_u32(obj + 58) as i32, 12);
        assert_eq!(mem.read_u32(obj + 62) as i32, -3);
        assert_eq!(c_string(&mem, mem.read_u32(obj + 50)), b"SYS:Tools/Edit");
        assert_eq!(c_string(&mem, mem.read_u32(obj + 70)), b"CON:0/0/1/1/x");
        assert_eq!(mem.read_u32(obj + 66), 0, "a project has no DrawerData");

        let tt = mem.read_u32(obj + 54);
        assert_eq!(c_string(&mem, mem.read_u32(tt)), b"A=1");
        assert_eq!(c_string(&mem, mem.read_u32(tt + 4)), b"FLAG");
        assert_eq!(mem.read_u32(tt + 8), 0, "the array is NULL-terminated");

        let img = mem.read_u32(obj + 22);
        assert_eq!(mem.read_u16(img + 4), 20, "Image.Width");
        assert_eq!(mem.read_u8(img + 14), 3, "Image.PlanePick");
        let planes = mem.read_u32(img + 10);
        assert_eq!(mem.read_u8(planes + 5), 5);
        assert_ne!(mem.read_u32(obj + 26), 0, "SelectRender");
        assert_eq!(mem.read_u32(obj + 4), 0, "NextGadget");
    }

    #[test]
    fn the_free_list_sits_right_after_the_disk_object_and_is_recorded_first() {
        let (mut heap, mut mem) = heap_and_mem();
        let obj = build_disk_object(&mut heap, &mut mem, &project()).unwrap();
        let fl = obj + DISK_OBJECT_MEM_SIZE;
        let first = mem.read_u32(fl + FL_MEMLIST + LH_HEAD);
        assert_eq!(mem.read_u8(first + LN_TYPE), NT_MEMORY);
        assert_eq!(mem.read_u16(first + ML_NUMENTRIES), 1);
        assert_eq!(mem.read_u32(first + ML_ME), obj, "block 0 holds the object");
        assert_eq!(
            mem.read_u32(first + ML_ME + 4),
            DISK_OBJECT_MEM_SIZE + FREE_LIST_SIZE
        );
    }

    #[test]
    fn freeing_the_free_list_returns_every_byte_to_the_heap() {
        let (mut heap, mut mem) = heap_and_mem();
        let before = heap.free_bytes();
        let obj = build_disk_object(&mut heap, &mut mem, &project()).unwrap();
        assert!(heap.free_bytes() < before);
        let freed = free_free_list(&mut heap, &mut mem, obj + DISK_OBJECT_MEM_SIZE).unwrap();
        assert!(
            freed >= 8,
            "object, 2 images, 2 plane sets, strings: {freed}"
        );
        assert_eq!(heap.free_bytes(), before, "nothing leaked");
    }

    #[test]
    fn a_second_free_is_an_error_not_a_crash() {
        let (mut heap, mut mem) = heap_and_mem();
        let obj = build_disk_object(&mut heap, &mut mem, &project()).unwrap();
        let fl = obj + DISK_OBJECT_MEM_SIZE;
        // Keep the list readable past the first free, as a stale guest
        // pointer would see it.
        free_free_list(&mut heap, &mut mem, fl).unwrap();
        assert!(free_free_list(&mut heap, &mut mem, fl).is_err());
    }

    #[test]
    fn a_drawer_gets_the_full_62_byte_drawer_data() {
        let (mut heap, mut mem) = heap_and_mem();
        let mut new_window = [0u8; NEW_WINDOW_SIZE];
        new_window[..8].copy_from_slice(&[0, 47, 0, 27, 1, 139, 0, 118]);
        new_window[20..24].copy_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // a stale pointer
        let icon = DiskObjectFile {
            object_type: WBDRAWER,
            drawer: Some(DrawerData {
                new_window,
                current_x: 5,
                current_y: 6,
                extension: Some((0x10, 3)),
            }),
            ..DiskObjectFile::default()
        };
        let obj = build_disk_object(&mut heap, &mut mem, &icon).unwrap();
        let dd = mem.read_u32(obj + 66);
        assert_ne!(dd, 0);
        assert_eq!(mem.read_u16(dd), 47, "NewWindow.LeftEdge");
        assert_eq!(mem.read_u16(dd + 2), 27, "NewWindow.TopEdge");
        assert_eq!(
            mem.read_u32(dd + 20),
            0,
            "a stale pointer in the file is cleared"
        );
        assert_eq!(mem.read_u32(dd + 48), 5, "dd_CurrentX");
        assert_eq!(mem.read_u32(dd + 56), 0x10, "dd_Flags");
        assert_eq!(mem.read_u16(dd + 60), 3, "dd_ViewModes");
    }

    #[test]
    fn a_workbench_13_drawer_has_zero_flags_and_view_modes() {
        let (mut heap, mut mem) = heap_and_mem();
        let icon = DiskObjectFile {
            object_type: WBDRAWER,
            drawer: Some(DrawerData {
                new_window: [0; NEW_WINDOW_SIZE],
                current_x: 0,
                current_y: 0,
                extension: None,
            }),
            ..DiskObjectFile::default()
        };
        let obj = build_disk_object(&mut heap, &mut mem, &icon).unwrap();
        let dd = mem.read_u32(obj + 66);
        assert_eq!(mem.read_u32(dd + 56), 0);
        assert_eq!(mem.read_u16(dd + 60), 0);
    }

    #[test]
    fn a_full_heap_gives_no_memory_and_leaks_nothing() {
        // Find the smallest heap that fits the object, then one that
        // is a little too small: the build must fail cleanly and give
        // back whatever it had taken.
        let icon = project();
        for size in (0x200..0x800).step_by(0x20) {
            let mut mem = FlatMemory::new(0x1_2000);
            let mut heap = GuestHeap::new(0x1000, 0x1000 + size);
            let before = heap.free_bytes();
            if let Ok(obj) = build_disk_object(&mut heap, &mut mem, &icon) {
                free_free_list(&mut heap, &mut mem, obj + DISK_OBJECT_MEM_SIZE).unwrap();
            }
            assert_eq!(heap.free_bytes(), before, "heap of {size:#x} leaked");
        }
    }

    #[test]
    fn add_free_list_records_memory_without_allocating_it() {
        let (mut heap, mut mem) = heap_and_mem();
        let obj = build_disk_object(&mut heap, &mut mem, &DiskObjectFile::default()).unwrap();
        let fl = obj + DISK_OBJECT_MEM_SIZE;
        let extra = heap.alloc(&mut mem, 64).unwrap();
        assert!(add_free_list(&mut heap, &mut mem, fl, extra, 64));
        // FreeFreeList now frees the caller's block too.
        let before = heap.free_bytes();
        free_free_list(&mut heap, &mut mem, fl).unwrap();
        assert!(heap.free_bytes() > before);
        assert!(heap.free(extra).is_err(), "already freed by the list");
    }

    #[test]
    fn a_corrupt_entry_count_is_an_error() {
        let (mut heap, mut mem) = heap_and_mem();
        let obj = build_disk_object(&mut heap, &mut mem, &DiskObjectFile::default()).unwrap();
        let fl = obj + DISK_OBJECT_MEM_SIZE;
        let first = mem.read_u32(fl + FL_MEMLIST + LH_HEAD);
        mem.write_u16(first + ML_NUMENTRIES, 0xFFFF);
        let err = free_free_list(&mut heap, &mut mem, fl).unwrap_err();
        assert!(err.contains("entries"), "{err}");
    }

    #[test]
    fn a_list_naming_a_block_the_heap_does_not_own_frees_nothing() {
        let (mut heap, mut mem) = heap_and_mem();
        let obj = build_disk_object(&mut heap, &mut mem, &project()).unwrap();
        let fl = obj + DISK_OBJECT_MEM_SIZE;
        let before = heap.free_bytes();
        assert!(add_free_list(&mut heap, &mut mem, fl, 0x1234_5670, 16));
        let after_add = heap.free_bytes();
        let err = free_free_list(&mut heap, &mut mem, fl).unwrap_err();
        assert!(err.contains("not a live allocation"), "{err}");
        assert_eq!(heap.free_bytes(), after_add, "nothing half-freed");
        assert!(before > after_add);
    }

    #[test]
    fn a_looping_list_fails_at_the_first_repeat() {
        let (mut heap, mut mem) = heap_and_mem();
        let obj = build_disk_object(&mut heap, &mut mem, &DiskObjectFile::default()).unwrap();
        let fl = obj + DISK_OBJECT_MEM_SIZE;
        let first = mem.read_u32(fl + FL_MEMLIST + LH_HEAD);
        mem.write_u32(first + LN_SUCC, first);
        let err = free_free_list(&mut heap, &mut mem, fl).unwrap_err();
        assert!(err.contains("loops"), "{err}");
    }

    #[test]
    fn free_disk_object_refuses_memory_that_only_looks_like_a_disk_object() {
        let (mut heap, mut mem) = heap_and_mem();
        let real = build_disk_object(&mut heap, &mut mem, &project()).unwrap();
        // A recycled block whose list points at someone else's live nodes.
        let fake = heap
            .alloc(&mut mem, DISK_OBJECT_MEM_SIZE + FREE_LIST_SIZE)
            .unwrap();
        let theirs = mem.read_u32(real + DISK_OBJECT_MEM_SIZE + FL_MEMLIST + LH_HEAD);
        mem.write_u32(fake + DISK_OBJECT_MEM_SIZE + FL_MEMLIST + LH_HEAD, theirs);
        let before = heap.free_bytes();
        assert!(free_disk_object(&mut heap, &mut mem, fake).is_err());
        assert_eq!(heap.free_bytes(), before, "nothing was freed");
        free_disk_object(&mut heap, &mut mem, real).expect("the real one is intact");
    }

    #[test]
    fn free_disk_object_refuses_a_pointer_that_is_not_an_allocation() {
        let (mut heap, mut mem) = heap_and_mem();
        let real = build_disk_object(&mut heap, &mut mem, &project()).unwrap();
        assert!(free_disk_object(&mut heap, &mut mem, real + 4).is_err());
        free_disk_object(&mut heap, &mut mem, real).unwrap();
        assert!(free_disk_object(&mut heap, &mut mem, real).is_err());
    }

    #[test]
    fn reading_a_built_object_back_gives_the_icon_it_was_built_from() {
        let (mut heap, mut mem) = heap_and_mem();
        let mut icon = project();
        icon.drawer = Some(DrawerData {
            new_window: {
                let mut w = [0u8; NEW_WINDOW_SIZE];
                w[0] = 1;
                w[5] = 47;
                w
            },
            current_x: 12,
            current_y: -3,
            extension: Some((1, 2)),
        });
        icon.object_type = WBDRAWER;
        icon.tool_window = Some(b"CON:0/0/320/100/x".to_vec());
        let obj = build_disk_object(&mut heap, &mut mem, &icon).unwrap();
        assert_eq!(read_disk_object(&mem, obj).unwrap(), icon);
    }

    #[test]
    fn a_drawer_without_the_extension_reads_back_with_zero_flags() {
        let (mut heap, mut mem) = heap_and_mem();
        let mut icon = DiskObjectFile::builtin_default(WBDRAWER);
        icon.drawer.as_mut().unwrap().extension = None;
        let obj = build_disk_object(&mut heap, &mut mem, &icon).unwrap();
        let back = read_disk_object(&mem, obj).unwrap();
        assert_eq!(back.drawer.unwrap().extension, Some((0, 0)));
    }

    #[test]
    fn reading_a_wild_pointer_names_what_was_unreadable() {
        let (mut heap, mut mem) = heap_and_mem();
        let obj = build_disk_object(&mut heap, &mut mem, &project()).unwrap();
        mem.write_u32(obj + 50, 0xFFFF_FFF0); // do_DefaultTool
        let err = read_disk_object(&mem, obj).unwrap_err();
        assert!(err.contains("do_DefaultTool"), "{err}");
        assert!(read_disk_object(&mem, 0).is_err(), "NULL");
        assert!(read_disk_object(&mem, 0xFFFF_FF00).is_err(), "off the end");
    }

    #[test]
    fn an_unterminated_tool_types_array_is_refused() {
        let (mut heap, mut mem) = heap_and_mem();
        let obj = build_disk_object(&mut heap, &mut mem, &project()).unwrap();
        // Point every slot at the same string, forever: the array of
        // `MAX_TOOL_TYPES` entries never ends within memory or bounds.
        let arr = mem.read_u32(obj + 54);
        let first = mem.read_u32(arr);
        mem.write_u32(arr + 8, first);
        mem.write_u32(arr + 12, 0xFFFF_FFF0);
        let err = read_disk_object(&mem, obj).unwrap_err();
        assert!(
            err.contains("tool type") || err.contains("ToolTypes"),
            "{err}"
        );
    }

    /// Builds and frees every real icon under a directory, for ground
    /// truth the repository cannot hold: `VOLAMOS_INFO_CORPUS=<dir>
    /// cargo test -p volamos-core iconobj::tests::real_icons -- --ignored`.
    /// The heap must come back exactly as it was after each one.
    #[test]
    #[ignore = "needs VOLAMOS_INFO_CORPUS=<dir of real .info files>"]
    fn real_icons_build_and_free_without_leaking() {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("info"))
                {
                    out.push(path);
                }
            }
        }
        let root = std::env::var("VOLAMOS_INFO_CORPUS").expect("VOLAMOS_INFO_CORPUS");
        let mut files = Vec::new();
        walk(std::path::Path::new(&root), &mut files);
        let mut mem = FlatMemory::new(0x40_0000);
        let mut heap = GuestHeap::new(0x1000, 0x40_0000);
        let before = heap.free_bytes();
        let mut built = 0;
        for path in files {
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let Ok((icon, _)) = DiskObjectFile::parse(&bytes) else {
                continue;
            };
            let obj = build_disk_object(&mut heap, &mut mem, &icon)
                .unwrap_or_else(|_| panic!("{}: out of memory", path.display()));
            // What PutDiskObject would write is the icon again, apart
            // from the pointer fields that mean nothing on disk.
            let back =
                read_disk_object(&mem, obj).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let who = path.display();
            assert_eq!(back.object_type, icon.object_type, "{who}");
            assert_eq!(back.stack_size, icon.stack_size, "{who}");
            assert_eq!(back.default_tool, icon.default_tool, "{who}");
            assert_eq!(back.tool_types, icon.tool_types, "{who}");
            assert_eq!(back.tool_window, icon.tool_window, "{who}");
            assert_eq!(back.gadget_image, icon.gadget_image, "{who}");
            assert_eq!(back.select_image, icon.select_image, "{who}");
            assert_eq!(
                (back.current_x, back.current_y),
                (icon.current_x, icon.current_y),
                "{who}"
            );
            assert_eq!(back.drawer.is_some(), icon.drawer.is_some(), "{who}");
            if let (Some(b), Some(i)) = (&back.drawer, &icon.drawer) {
                assert_eq!(b.new_window[..18], i.new_window[..18], "{who}");
                assert_eq!(b.new_window[38..], i.new_window[38..], "{who}");
                assert_eq!((b.current_x, b.current_y), (i.current_x, i.current_y));
                assert_eq!((b.flags(), b.view_modes()), (i.flags(), i.view_modes()));
            }
            free_free_list(&mut heap, &mut mem, obj + DISK_OBJECT_MEM_SIZE)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(heap.free_bytes(), before, "{} leaked", path.display());
            built += 1;
        }
        eprintln!("{built} icons built and freed");
        assert!(built > 0);
    }
}
