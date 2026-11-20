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

use crate::execlist::{LH_HEAD, LN_SUCC, LN_TYPE, add_tail_impl, init_list_header};
use crate::guestmem::{GuestHeap, write_c_string};
use crate::iconfile::{
    DRAWER_EXTENSION_SIZE, DiskObjectFile, IconImage, NEW_WINDOW_SIZE, WB_DISKMAGIC,
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
/// More entries than this in one `MemList`, or more nodes than this in
/// a list, is a corrupt structure, not an icon: stop rather than walk
/// forever.
const MAX_ENTRIES_PER_NODE: u16 = 256;
const MAX_NODES: usize = 1 << 20;

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
/// freed, or why the structure is not a free list this module can walk
/// (a loop, a nonsense entry count, a block the heap does not know).
pub fn free_free_list(
    heap: &mut GuestHeap,
    mem: &mut dyn AddressSpace,
    free_list: u32,
) -> Result<usize, String> {
    // Collect everything first: the FreeList itself is usually inside
    // the first block, which must not be freed while it is being read.
    let mut nodes = Vec::new();
    let mut blocks = Vec::new();
    let mut node = mem.read_u32(free_list + FL_MEMLIST + LH_HEAD);
    while mem.read_u32(node + LN_SUCC) != 0 {
        if nodes.len() >= MAX_NODES {
            return Err("the free list is too long or loops".to_string());
        }
        let entries = mem.read_u16(node + ML_NUMENTRIES);
        if entries > MAX_ENTRIES_PER_NODE {
            return Err(format!(
                "a MemList at {node:#010x} claims {entries} entries"
            ));
        }
        for i in 0..entries as u32 {
            let addr = mem.read_u32(node + ML_ME + 8 * i);
            if addr != 0 {
                blocks.push(addr);
            }
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
            for (i, &b) in d.new_window.iter().enumerate() {
                mem.write_u8(addr + i as u32, b);
            }
            for off in NEW_WINDOW_POINTERS {
                mem.write_u8(addr + off, 0);
            }
            debug_assert_eq!(NEW_WINDOW_SIZE, 48);
            mem.write_u32(addr + 48, d.current_x as u32);
            mem.write_u32(addr + 52, d.current_y as u32);
            mem.write_u32(addr + 56, d.flags());
            mem.write_u16(addr + 60, d.view_modes());
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
            free_free_list(&mut heap, &mut mem, obj + DISK_OBJECT_MEM_SIZE)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(heap.free_bytes(), before, "{} leaked", path.display());
            built += 1;
        }
        eprintln!("{built} icons built and freed");
        assert!(built > 0);
    }
}
