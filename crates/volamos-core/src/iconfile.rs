//! The classic `.info` file format: a host-side model, parser and
//! writer for `icon.library`'s `DiskObject` files (issue #313, phase 1).
//!
//! This module is pure bytes-in/bytes-out. It knows nothing about guest
//! memory, the heap or `DosState`; [`crate::icon`] will turn a
//! [`DiskObjectFile`] into a guest `struct DiskObject` (and back) in
//! the next phase. Keeping the two apart lets the format be tested
//! exhaustively with no emulator in the loop.
//!
//! # The format
//!
//! Field sizes and order are from `<workbench/workbench.h>` (NDK 3.2)
//! and were checked against ~6,300 real `.info` files (Workbench 1.3,
//! 3.1 and 3.1.4 installs, Aminet archives, ColorIcon sets; 1,500 of
//! them with a trailing `FORM ICON`): all parse, and all but a handful
//! end exactly where this layout says or at the start of that `FORM`.
//! The handful are empty or cut-off files and one icon with 64 stray
//! bytes after its picture, which are simply left unread.
//!
//! 1. `struct DiskObject`, 78 bytes: `do_Magic` ([`WB_DISKMAGIC`]),
//!    `do_Version`, the 44-byte embedded `struct Gadget`, `do_Type` and
//!    a pad byte, then `do_DefaultTool`, `do_ToolTypes`, `do_CurrentX`,
//!    `do_CurrentY`, `do_DrawerData`, `do_ToolWindow` and
//!    `do_StackSize`. The pointer fields are in-memory addresses from
//!    when the icon was written and mean nothing on disk: only whether
//!    each is zero matters, because that says whether its data follows.
//!    The same goes for `GadgetRender`/`SelectRender`.
//! 2. If `do_DrawerData` is set, the first 56 bytes of `struct
//!    DrawerData`: a 48-byte `struct NewWindow`, `dd_CurrentX` and
//!    `dd_CurrentY`. (`<workbench/workbench.h>` says the file holds
//!    `sizeof(struct DrawerData)`, i.e. 62 bytes -- but the real files
//!    don't: the OS 2.x `dd_Flags` and `dd_ViewModes` are *not* here,
//!    see step 7. A parser that follows the header comment misreads
//!    every drawer icon.)
//! 3. If `GadgetRender` is set, a 20-byte `struct Image` and its bitplane
//!    data, `((Width + 15) / 16) * 2 * Height * Depth` bytes.
//! 4. The same for `SelectRender`.
//! 5. If `do_DefaultTool` is set, a string: a big-endian `u32` length
//!    that counts the terminating NUL, then the bytes.
//! 6. If `do_ToolTypes` is set, a `u32` giving the size in bytes of the
//!    pointer array the strings were in -- `(count + 1) * 4`, the `+ 1`
//!    being its `NULL` terminator -- then `count` strings as in step 5.
//!    Then, if `do_ToolWindow` is set, one more string.
//! 7. If `do_DrawerData` is set, the OS 2.x drawer extension:
//!    `dd_Flags` (`u32`) and `dd_ViewModes` (`u16`). It is recognised
//!    when exactly six bytes remain, or when a `FORM` starts right
//!    after those six; any other trailing bytes are not taken for it.
//!    Workbench 1.3 drawer icons simply end after step 6, so the
//!    extension is optional and [`DrawerData::extension`] records
//!    whether it was there. A file cut off partway through its
//!    extension therefore reads as a 1.3 drawer with unread trailing
//!    bytes: the format has no length to tell the two apart, so a
//!    caller that cares compares the bytes used with the file's length.
//!
//! Anything after that is not this module's business. OS 3.5 ColorIcons
//! and PNG icons append an IFF `FORM ICON`, and NewIcons hide their
//! images in extra tool-type strings (`IM1=...`), which are ordinary
//! tool types here. [`DiskObjectFile::parse`] reports how many bytes it
//! used, so a caller can see the rest, and the classic part of such a
//! file reads exactly like any other -- which is all the headless
//! `icon.library` calls need: tool types, default tool, stack size,
//! position and drawer data. No pixel is ever interpreted, only
//! counted.
//!
//! # What is not modelled
//!
//! The in-memory pointer fields (see 1) are not kept; [`DiskObjectFile::
//! to_bytes`] writes [`POINTER_PRESENT`] for a set one. `Image`'s
//! `NextImage` is written as `0`. Which values a real `PutDiskObject`
//! writes is a phase 0 hardware check; no reader looks at them.
//!
//! Where this module has to decide something the files don't settle,
//! the decision is in the doc of the item that makes it, and goes with
//! what the real files do. Untested guesses are marked as such.

use std::fmt;

/// `do_Magic` of every `DiskObject` (`WB_DISKMAGIC`).
pub const WB_DISKMAGIC: u16 = 0xE310;
/// `do_Version` of every `DiskObject` (`WB_DISKVERSION`).
pub const WB_DISKVERSION: u16 = 1;
/// The revision in the low byte of `do_Gadget.UserData`
/// (`WB_DISKREVISION`).
pub const WB_DISKREVISION: u8 = 1;
/// Size of `struct DiskObject` on disk, step 1 above.
pub const DISK_OBJECT_SIZE: usize = 78;
/// Size of the `struct NewWindow` at the start of `DrawerData`.
pub const NEW_WINDOW_SIZE: usize = 48;
/// Size of the part of `struct DrawerData` stored after the
/// `DiskObject`: the `NewWindow` plus `dd_CurrentX`/`dd_CurrentY`.
pub const DRAWER_DATA_FILE_SIZE: usize = NEW_WINDOW_SIZE + 8;
/// Size of `struct Image`.
pub const IMAGE_HEADER_SIZE: usize = 20;
/// Size of the OS 2.x drawer extension, step 7 above.
pub const DRAWER_EXTENSION_SIZE: usize = 6;
/// The most bitplane bytes [`DiskObjectFile::to_bytes`] writes for one image.
const MAX_WRITTEN_IMAGE_BYTES: usize = 64 << 20;
/// What [`DiskObjectFile::to_bytes`] writes for a pointer field whose
/// data follows. Any non-zero value would do.
pub const POINTER_PRESENT: u32 = 1;

/// `do_Type` values, from `<workbench/workbench.h>`.
pub const WBDISK: u8 = 1;
pub const WBDRAWER: u8 = 2;
pub const WBTOOL: u8 = 3;
pub const WBPROJECT: u8 = 4;
pub const WBGARBAGE: u8 = 5;
pub const WBDEVICE: u8 = 6;
pub const WBKICK: u8 = 7;
pub const WBAPPICON: u8 = 8;

/// `NO_ICON_POSITION` (`0x80000000`): "the icon has no position yet",
/// what `do_CurrentX`/`do_CurrentY` hold until it is placed.
pub const NO_ICON_POSITION: i32 = i32::MIN;

/// Why a byte string is not a `.info` file this module can read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconFileError {
    /// The first word is not [`WB_DISKMAGIC`]: not an icon at all.
    BadMagic(u16),
    /// The data ends before the structure being read does: `needed`
    /// more bytes were required at `offset`.
    Truncated { offset: usize, needed: usize },
}

impl fmt::Display for IconFileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IconFileError::BadMagic(m) => {
                write!(
                    f,
                    "not an icon: magic {m:#06x}, expected {WB_DISKMAGIC:#06x}"
                )
            }
            IconFileError::Truncated { offset, needed } => {
                write!(
                    f,
                    "icon file truncated at byte {offset} ({needed} more needed)"
                )
            }
        }
    }
}

impl std::error::Error for IconFileError {}

/// The `struct Gadget` embedded in a `DiskObject`, minus its two image
/// pointers (see [`DiskObjectFile::gadget_image`]). The other pointer
/// fields are kept as read: they are meaningless on disk, but a
/// round trip should not invent values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GadgetFields {
    pub next_gadget: u32,
    pub left_edge: i16,
    pub top_edge: i16,
    pub width: i16,
    pub height: i16,
    pub flags: u16,
    pub activation: u16,
    pub gadget_type: u16,
    pub gadget_text: u32,
    pub mutual_exclude: u32,
    pub special_info: u32,
    pub gadget_id: u16,
    /// The low byte is the icon's revision ([`WB_DISKREVISION`]).
    pub user_data: u32,
}

/// A `struct Image` and its bitplanes: an icon's picture, kept only as
/// bytes. Nothing here draws it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IconImage {
    pub left_edge: i16,
    pub top_edge: i16,
    pub width: i16,
    pub height: i16,
    pub depth: i16,
    pub plane_pick: u8,
    pub plane_on_off: u8,
    /// The bitplane words, exactly as on disk (big-endian), `len ==`
    /// [`plane_data_size`] of the dimensions when parsed. Empty when the
    /// image's `ImageData` pointer was `NULL`.
    pub data: Vec<u8>,
}

/// The part of a drawer icon's `struct DrawerData` the file stores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrawerData {
    /// The `struct NewWindow`: where the drawer's window opens.
    pub new_window: [u8; NEW_WINDOW_SIZE],
    pub current_x: i32,
    pub current_y: i32,
    /// `dd_Flags` and `dd_ViewModes`, the OS 2.x extension; `None` for a
    /// Workbench 1.3 drawer, whose file has no extension.
    pub extension: Option<(u32, u16)>,
}

impl DrawerData {
    /// `dd_Flags`, `0` for a file without the extension.
    pub fn flags(&self) -> u32 {
        self.extension.map_or(0, |(flags, _)| flags)
    }

    /// `dd_ViewModes`, `0` (`DDVM_BYDEFAULT`) for a file without the
    /// extension.
    pub fn view_modes(&self) -> u16 {
        self.extension.map_or(0, |(_, modes)| modes)
    }
}

/// One `.info` file's classic content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskObjectFile {
    /// `do_Version`. Not checked on reading: only the magic is.
    pub version: u16,
    pub gadget: GadgetFields,
    /// `do_Type`: [`WBDISK`] and so on.
    pub object_type: u8,
    /// `do_CurrentX`/`do_CurrentY`; [`NO_ICON_POSITION`] when unplaced.
    pub current_x: i32,
    pub current_y: i32,
    pub stack_size: i32,
    /// The strings are the bytes up to (not including) their NUL.
    pub default_tool: Option<Vec<u8>>,
    /// `Some(vec![])` is a present-but-empty array, different from `None`
    /// (no `do_ToolTypes` at all): a guest sees a non-`NULL` pointer to
    /// an array holding only the terminator.
    pub tool_types: Option<Vec<Vec<u8>>>,
    pub tool_window: Option<Vec<u8>>,
    pub drawer: Option<DrawerData>,
    /// `GadgetRender`'s image: the normal picture.
    pub gadget_image: Option<IconImage>,
    /// `SelectRender`'s image: the picture when selected.
    pub select_image: Option<IconImage>,
}

impl Default for DiskObjectFile {
    /// A tool icon with no content: what `GetDiskObject(NULL)` would
    /// start from, apart from the type.
    fn default() -> Self {
        DiskObjectFile {
            version: WB_DISKVERSION,
            gadget: GadgetFields {
                user_data: WB_DISKREVISION as u32,
                ..GadgetFields::default()
            },
            object_type: WBTOOL,
            current_x: NO_ICON_POSITION,
            current_y: NO_ICON_POSITION,
            stack_size: 0,
            default_tool: None,
            tool_types: None,
            tool_window: None,
            drawer: None,
            gadget_image: None,
            select_image: None,
        }
    }
}

impl DiskObjectFile {
    /// What `GetDiskObject(NULL)` hands back: "a DiskObject structure
    /// allocated for you" with nothing in it but the magic, version and
    /// revision, and an icon that "does not really live anywhere"
    /// ([`NO_ICON_POSITION`]). Its type is `0`: the caller sets it. The
    /// V40 library's own values for these fields are unverified
    /// (phase 0 of #313).
    pub fn empty() -> Self {
        DiskObjectFile {
            object_type: 0,
            ..DiskObjectFile::default()
        }
    }

    /// A built-in default icon of `object_type` ([`WBDISK`] to
    /// [`WBKICK`]), for `GetDefDiskObject` and `GetDiskObjectNew` when
    /// no `ENV:Sys/def_<type>.info` exists. It has no images: nothing
    /// here draws an icon, and a default's pictures are not metadata
    /// any caller reads. What it does have is what a program branches
    /// on: the type, a drawer's `DrawerData` (disks, drawers, trashcans and Kickstart
    /// icons are drawers to Workbench), and a tool's default stack.
    pub fn builtin_default(object_type: u8) -> Self {
        let is_drawer = matches!(object_type, WBDISK | WBDRAWER | WBGARBAGE | WBKICK);
        DiskObjectFile {
            object_type,
            stack_size: if object_type == WBTOOL { 4096 } else { 0 },
            drawer: is_drawer.then_some(DrawerData {
                new_window: [0; NEW_WINDOW_SIZE],
                current_x: 0,
                current_y: 0,
                extension: Some((0, 0)),
            }),
            ..DiskObjectFile::default()
        }
    }
}

/// How many bytes of bitplane data an `Image` of this size has:
/// `((width + 15) / 16) * 2 * height * depth`. A negative dimension
/// counts as `0`, as for a drawing routine that draws nothing for it.
/// A `u64`, because three 16-bit dimensions can exceed a 32-bit host's
/// `usize` (at most ~4.4e12, which `u64` holds).
pub fn plane_data_size(width: i16, height: i16, depth: i16) -> u64 {
    let (w, h, d) = (
        width.max(0) as u64,
        height.max(0) as u64,
        depth.max(0) as u64,
    );
    w.div_ceil(16) * 2 * h * d
}

/// A cursor over the file's bytes that fails, rather than panics, at
/// the end.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], IconFileError> {
        let remaining = self.bytes.len() - self.pos;
        if n > remaining {
            return Err(IconFileError::Truncated {
                offset: self.pos,
                needed: n - remaining,
            });
        }
        let out = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, IconFileError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, IconFileError> {
        Ok(u16::from_be_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn i16(&mut self) -> Result<i16, IconFileError> {
        Ok(self.u16()? as i16)
    }

    fn u32(&mut self) -> Result<u32, IconFileError> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn i32(&mut self) -> Result<i32, IconFileError> {
        Ok(self.u32()? as i32)
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }

    /// A string as in step 5: `u32` length (counting the NUL), the bytes.
    /// The result stops at the first NUL, so a length that over-counts
    /// (or a missing terminator) still gives the text.
    fn string(&mut self) -> Result<Vec<u8>, IconFileError> {
        let len = self.u32()? as usize;
        let raw = self.take(len)?;
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        Ok(raw[..end].to_vec())
    }

    fn image(&mut self) -> Result<IconImage, IconFileError> {
        let left_edge = self.i16()?;
        let top_edge = self.i16()?;
        let width = self.i16()?;
        let height = self.i16()?;
        let depth = self.i16()?;
        let image_data = self.u32()?;
        let plane_pick = self.u8()?;
        let plane_on_off = self.u8()?;
        let _next_image = self.u32()?;
        // Only an image whose ImageData pointer was set has planes
        // after it. (Every real file with an image sets it; the
        // NULL case is the loader's reading, not seen in the wild.)
        let data = if image_data != 0 {
            // A size a host `usize` cannot hold is more than any file has.
            let size = usize::try_from(plane_data_size(width, height, depth)).unwrap_or(usize::MAX);
            self.take(size)?.to_vec()
        } else {
            Vec::new()
        };
        Ok(IconImage {
            left_edge,
            top_edge,
            width,
            height,
            depth,
            plane_pick,
            plane_on_off,
            data,
        })
    }
}

impl DiskObjectFile {
    /// Parses the classic part of a `.info` file. Returns the icon and
    /// how many bytes of `bytes` it used; whatever follows (a `FORM
    /// ICON`, or anything else) is left to the caller.
    ///
    /// Never panics, whatever the input: a short or corrupt file is an
    /// [`IconFileError`].
    pub fn parse(bytes: &[u8]) -> Result<(DiskObjectFile, usize), IconFileError> {
        let mut r = Reader { bytes, pos: 0 };
        let magic = r.u16()?;
        if magic != WB_DISKMAGIC {
            return Err(IconFileError::BadMagic(magic));
        }
        let version = r.u16()?;

        let next_gadget = r.u32()?;
        let left_edge = r.i16()?;
        let top_edge = r.i16()?;
        let width = r.i16()?;
        let height = r.i16()?;
        let flags = r.u16()?;
        let activation = r.u16()?;
        let gadget_type = r.u16()?;
        let gadget_render = r.u32()?;
        let select_render = r.u32()?;
        let gadget_text = r.u32()?;
        let mutual_exclude = r.u32()?;
        let special_info = r.u32()?;
        let gadget_id = r.u16()?;
        let user_data = r.u32()?;

        let object_type = r.u8()?;
        let _pad = r.u8()?;
        let default_tool = r.u32()?;
        let tool_types = r.u32()?;
        let current_x = r.i32()?;
        let current_y = r.i32()?;
        let drawer_data = r.u32()?;
        let tool_window = r.u32()?;
        let stack_size = r.i32()?;
        debug_assert_eq!(r.pos, DISK_OBJECT_SIZE);

        let mut drawer = if drawer_data != 0 {
            let new_window: [u8; NEW_WINDOW_SIZE] = r.take(NEW_WINDOW_SIZE)?.try_into().unwrap();
            Some(DrawerData {
                new_window,
                current_x: r.i32()?,
                current_y: r.i32()?,
                extension: None,
            })
        } else {
            None
        };

        let gadget_image = if gadget_render != 0 {
            Some(r.image()?)
        } else {
            None
        };
        let select_image = if select_render != 0 {
            Some(r.image()?)
        } else {
            None
        };

        let default_tool = if default_tool != 0 {
            Some(r.string()?)
        } else {
            None
        };
        let tool_types = if tool_types != 0 {
            let array_size = r.u32()? as usize;
            // `count` strings plus the NULL terminator, 4 bytes each.
            let count = (array_size / 4).saturating_sub(1);
            // Every string is at least its 4-byte length, so a count
            // the file cannot hold is corrupt, not a reason to allocate.
            if count > r.remaining().len() / 4 {
                return Err(IconFileError::Truncated {
                    offset: r.pos,
                    needed: count.saturating_mul(4) - r.remaining().len(),
                });
            }
            let mut types = Vec::with_capacity(count);
            for _ in 0..count {
                types.push(r.string()?);
            }
            Some(types)
        } else {
            None
        };
        let tool_window = if tool_window != 0 {
            Some(r.string()?)
        } else {
            None
        };

        if let Some(drawer) = drawer.as_mut() {
            let rest = r.remaining();
            if rest.len() == DRAWER_EXTENSION_SIZE
                || rest
                    .get(DRAWER_EXTENSION_SIZE..)
                    .is_some_and(|t| t.starts_with(b"FORM"))
            {
                drawer.extension = Some((r.u32()?, r.u16()?));
            }
        }

        let icon = DiskObjectFile {
            version,
            gadget: GadgetFields {
                next_gadget,
                left_edge,
                top_edge,
                width,
                height,
                flags,
                activation,
                gadget_type,
                gadget_text,
                mutual_exclude,
                special_info,
                gadget_id,
                user_data,
            },
            object_type,
            current_x,
            current_y,
            stack_size,
            default_tool,
            tool_types,
            tool_window,
            drawer,
            gadget_image,
            select_image,
        };
        Ok((icon, r.pos))
    }

    /// The classic `.info` bytes for this icon, in the order described
    /// in the module doc. `parse(to_bytes(x))` gives back `x`.
    pub fn to_bytes(&self) -> Vec<u8> {
        fn present(set: bool) -> u32 {
            if set { POINTER_PRESENT } else { 0 }
        }
        // A C string: it ends at its first NUL, as it did in the guest
        // memory it came from, and as the parser reads it back.
        fn string(out: &mut Vec<u8>, s: &[u8]) {
            let s = &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())];
            out.extend_from_slice(&(s.len() as u32 + 1).to_be_bytes());
            out.extend_from_slice(s);
            out.push(0);
        }
        fn image(out: &mut Vec<u8>, img: &IconImage) {
            for v in [
                img.left_edge,
                img.top_edge,
                img.width,
                img.height,
                img.depth,
            ] {
                out.extend_from_slice(&v.to_be_bytes());
            }
            // Planes are written only up to a size no real icon reaches;
            // an image claiming more (a caller-built `IconImage` with
            // absurd dimensions) is written without them rather than
            // allocating terabytes.
            let size = usize::try_from(plane_data_size(img.width, img.height, img.depth))
                .ok()
                .filter(|&n| n <= MAX_WRITTEN_IMAGE_BYTES);
            let planes = size.filter(|_| !img.data.is_empty());
            out.extend_from_slice(&present(planes.is_some()).to_be_bytes());
            out.push(img.plane_pick);
            out.push(img.plane_on_off);
            out.extend_from_slice(&0u32.to_be_bytes()); // NextImage
            // The planes are as long as the dimensions say, whatever
            // `data` holds, so a reader finds the next field where
            // it expects it: cut or zero-padded to fit.
            if let Some(size) = planes {
                out.extend_from_slice(&img.data[..img.data.len().min(size)]);
                out.resize(out.len() + size.saturating_sub(img.data.len()), 0);
            }
        }

        let g = &self.gadget;
        let mut out = Vec::with_capacity(DISK_OBJECT_SIZE);
        out.extend_from_slice(&WB_DISKMAGIC.to_be_bytes());
        out.extend_from_slice(&self.version.to_be_bytes());
        out.extend_from_slice(&g.next_gadget.to_be_bytes());
        for v in [g.left_edge, g.top_edge, g.width, g.height] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        for v in [g.flags, g.activation, g.gadget_type] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&present(self.gadget_image.is_some()).to_be_bytes());
        out.extend_from_slice(&present(self.select_image.is_some()).to_be_bytes());
        for v in [g.gadget_text, g.mutual_exclude, g.special_info] {
            out.extend_from_slice(&v.to_be_bytes());
        }
        out.extend_from_slice(&g.gadget_id.to_be_bytes());
        out.extend_from_slice(&g.user_data.to_be_bytes());
        out.push(self.object_type);
        out.push(0);
        out.extend_from_slice(&present(self.default_tool.is_some()).to_be_bytes());
        out.extend_from_slice(&present(self.tool_types.is_some()).to_be_bytes());
        out.extend_from_slice(&self.current_x.to_be_bytes());
        out.extend_from_slice(&self.current_y.to_be_bytes());
        out.extend_from_slice(&present(self.drawer.is_some()).to_be_bytes());
        out.extend_from_slice(&present(self.tool_window.is_some()).to_be_bytes());
        out.extend_from_slice(&self.stack_size.to_be_bytes());
        debug_assert_eq!(out.len(), DISK_OBJECT_SIZE);

        if let Some(drawer) = &self.drawer {
            out.extend_from_slice(&drawer.new_window);
            out.extend_from_slice(&drawer.current_x.to_be_bytes());
            out.extend_from_slice(&drawer.current_y.to_be_bytes());
        }
        for img in [&self.gadget_image, &self.select_image]
            .into_iter()
            .flatten()
        {
            image(&mut out, img);
        }
        if let Some(tool) = &self.default_tool {
            string(&mut out, tool);
        }
        if let Some(types) = &self.tool_types {
            out.extend_from_slice(&((types.len() as u32 + 1) * 4).to_be_bytes());
            for t in types {
                string(&mut out, t);
            }
        }
        if let Some(window) = &self.tool_window {
            string(&mut out, window);
        }
        if let Some((flags, modes)) = self.drawer.as_ref().and_then(|d| d.extension) {
            out.extend_from_slice(&flags.to_be_bytes());
            out.extend_from_slice(&modes.to_be_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: i16, height: i16, depth: i16) -> IconImage {
        IconImage {
            left_edge: 0,
            top_edge: 0,
            width,
            height,
            depth,
            plane_pick: (1u8 << depth) - 1,
            plane_on_off: 0,
            data: (0..plane_data_size(width, height, depth) as usize)
                .map(|i| i as u8)
                .collect(),
        }
    }

    /// A project icon with everything a tool-type reader cares about.
    fn project() -> DiskObjectFile {
        DiskObjectFile {
            object_type: WBPROJECT,
            stack_size: 8192,
            current_x: 40,
            current_y: -7,
            default_tool: Some(b"SYS:Utilities/MultiView".to_vec()),
            tool_types: Some(vec![b"FILETYPE=text".to_vec(), b"DONOTWAIT".to_vec()]),
            tool_window: Some(b"CON:0/0/640/200/Out".to_vec()),
            gadget_image: Some(image(30, 12, 2)),
            select_image: Some(image(30, 12, 2)),
            ..DiskObjectFile::default()
        }
    }

    fn drawer(extension: Option<(u32, u16)>) -> DiskObjectFile {
        let mut new_window = [0u8; NEW_WINDOW_SIZE];
        new_window[..8].copy_from_slice(&[0, 47, 0, 27, 1, 139, 0, 118]);
        DiskObjectFile {
            object_type: WBDRAWER,
            drawer: Some(DrawerData {
                new_window,
                current_x: 3,
                current_y: 4,
                extension,
            }),
            gadget_image: Some(image(40, 20, 1)),
            ..DiskObjectFile::default()
        }
    }

    #[test]
    fn plane_data_size_rounds_the_width_up_to_whole_words() {
        assert_eq!(plane_data_size(16, 1, 1), 2);
        assert_eq!(plane_data_size(17, 1, 1), 4);
        assert_eq!(plane_data_size(36, 17, 2), 3 * 2 * 17 * 2);
        assert_eq!(plane_data_size(0, 10, 2), 0);
        assert_eq!(plane_data_size(-5, 10, 2), 0, "negative counts as nothing");
        assert_eq!(plane_data_size(16, 10, -1), 0);
    }

    #[test]
    fn a_default_object_has_the_documented_sizes() {
        let bytes = DiskObjectFile::default().to_bytes();
        assert_eq!(bytes.len(), DISK_OBJECT_SIZE);
        assert_eq!(&bytes[..4], &[0xE3, 0x10, 0x00, 0x01]);
    }

    #[test]
    fn disk_object_fields_are_at_their_documented_offsets() {
        let bytes = project().to_bytes();
        assert_eq!(bytes[48], WBPROJECT, "do_Type");
        assert_eq!(
            &bytes[50..54],
            &POINTER_PRESENT.to_be_bytes(),
            "do_DefaultTool"
        );
        assert_eq!(
            &bytes[54..58],
            &POINTER_PRESENT.to_be_bytes(),
            "do_ToolTypes"
        );
        assert_eq!(&bytes[58..62], &40i32.to_be_bytes(), "do_CurrentX");
        assert_eq!(&bytes[62..66], &(-7i32).to_be_bytes(), "do_CurrentY");
        assert_eq!(&bytes[66..70], &[0; 4], "do_DrawerData");
        assert_eq!(
            &bytes[70..74],
            &POINTER_PRESENT.to_be_bytes(),
            "do_ToolWindow"
        );
        assert_eq!(&bytes[74..78], &8192i32.to_be_bytes(), "do_StackSize");
        // GadgetRender/SelectRender, inside the embedded Gadget.
        assert_eq!(
            &bytes[22..26],
            &POINTER_PRESENT.to_be_bytes(),
            "GadgetRender"
        );
        assert_eq!(
            &bytes[26..30],
            &POINTER_PRESENT.to_be_bytes(),
            "SelectRender"
        );
    }

    #[test]
    fn strings_and_tool_types_are_length_prefixed_with_the_nul_counted() {
        let icon = DiskObjectFile {
            default_tool: Some(b"abc".to_vec()),
            tool_types: Some(vec![b"A=1".to_vec()]),
            ..DiskObjectFile::default()
        };
        let bytes = icon.to_bytes();
        assert_eq!(
            &bytes[DISK_OBJECT_SIZE..],
            &[
                0, 0, 0, 4, b'a', b'b', b'c', 0, // DefaultTool
                0, 0, 0, 8, // pointer array: 1 string + terminator
                0, 0, 0, 4, b'A', b'=', b'1', 0, // the tool type
            ]
        );
    }

    #[test]
    fn a_project_icon_round_trips() {
        let icon = project();
        let bytes = icon.to_bytes();
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(parsed, icon);
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn an_empty_tool_type_array_is_not_the_same_as_none() {
        let some = DiskObjectFile {
            tool_types: Some(vec![]),
            ..DiskObjectFile::default()
        };
        let none = DiskObjectFile::default();
        assert_eq!(DiskObjectFile::parse(&some.to_bytes()).unwrap().0, some);
        assert_eq!(DiskObjectFile::parse(&none.to_bytes()).unwrap().0, none);
        assert_ne!(some.to_bytes(), none.to_bytes());
    }

    #[test]
    fn a_drawer_with_the_os2_extension_round_trips_and_stores_it_last() {
        let icon = drawer(Some((0x1234_5678, 3)));
        let bytes = icon.to_bytes();
        assert_eq!(&bytes[bytes.len() - 6..], &[0x12, 0x34, 0x56, 0x78, 0, 3]);
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(parsed, icon);
        assert_eq!(used, bytes.len());
        let d = parsed.drawer.unwrap();
        assert_eq!((d.flags(), d.view_modes()), (0x1234_5678, 3));
    }

    #[test]
    fn a_workbench_13_drawer_has_no_extension() {
        let icon = drawer(None);
        let bytes = icon.to_bytes();
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(parsed.drawer.as_ref().unwrap().extension, None);
        assert_eq!(used, bytes.len());
        assert_eq!(parsed.drawer.unwrap().view_modes(), 0);
    }

    #[test]
    fn drawer_data_in_the_file_is_56_bytes_not_62() {
        // The header comment says sizeof(DrawerData); real files store
        // the NewWindow and the two origin longs only. This pins the
        // layout the corpus showed.
        let bytes = drawer(None).to_bytes();
        let image_at = DISK_OBJECT_SIZE + DRAWER_DATA_FILE_SIZE;
        assert_eq!(DRAWER_DATA_FILE_SIZE, 56);
        // The Image header right after it starts with Left/Top = 0 and
        // has the width (40) at +4.
        assert_eq!(&bytes[image_at + 4..image_at + 6], &40i16.to_be_bytes());
    }

    #[test]
    fn a_trailing_form_is_left_alone_and_not_taken_for_a_drawer_extension() {
        let mut bytes = drawer(None).to_bytes();
        let classic = bytes.len();
        bytes.extend_from_slice(b"FORM\0\0\0\x10ICONFACE\0\0\0\x06");
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(used, classic, "the FORM is not consumed");
        assert_eq!(parsed.drawer.unwrap().extension, None);
        assert_eq!(&bytes[used..used + 4], b"FORM");
    }

    #[test]
    fn a_form_after_the_extension_is_left_alone_too() {
        let mut bytes = drawer(Some((1, 2))).to_bytes();
        let classic = bytes.len();
        bytes.extend_from_slice(b"FORM\0\0\0\0ICON");
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(used, classic);
        assert_eq!(parsed.drawer.unwrap().extension, Some((1, 2)));
    }

    #[test]
    fn a_tool_with_trailing_form_data_reads_its_tool_types() {
        // How an OS 3.5 ColorIcon or PNG icon looks to a headless
        // reader: the classic part first, an IFF chunk after.
        let mut bytes = project().to_bytes();
        let classic = bytes.len();
        bytes.extend_from_slice(b"FORM\0\0\0\x08ICONFACE\0\0\0\x06");
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(used, classic);
        assert_eq!(parsed, project());
    }

    #[test]
    fn newicons_image_lines_are_just_tool_types() {
        let icon = DiskObjectFile {
            tool_types: Some(vec![
                b"*** DON'T EDIT THE FOLLOWING LINES!! ***".to_vec(),
                b"IM1=BA9S1".to_vec(),
                b"FILETYPE=text".to_vec(),
            ]),
            ..DiskObjectFile::default()
        };
        let (parsed, _) = DiskObjectFile::parse(&icon.to_bytes()).unwrap();
        assert_eq!(parsed.tool_types.unwrap().len(), 3);
    }

    #[test]
    fn an_image_whose_data_pointer_is_null_has_no_planes() {
        let mut icon = project();
        icon.gadget_image.as_mut().unwrap().data.clear();
        let bytes = icon.to_bytes();
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert!(parsed.gadget_image.unwrap().data.is_empty());
    }

    #[test]
    fn a_string_with_an_overcounted_length_still_gives_the_text() {
        let mut bytes = DiskObjectFile {
            default_tool: Some(b"hi".to_vec()),
            ..DiskObjectFile::default()
        }
        .to_bytes();
        // Length 8 but only "hi\0" then padding zeros: the text is "hi".
        let at = DISK_OBJECT_SIZE;
        bytes[at + 3] = 8;
        bytes.extend_from_slice(&[0; 5]);
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(parsed.default_tool.as_deref(), Some(&b"hi"[..]));
        assert_eq!(used, bytes.len());
    }

    #[test]
    fn bad_magic_is_reported_with_the_value() {
        let mut bytes = project().to_bytes();
        bytes[0] = 0x4E;
        bytes[1] = 0x75;
        assert_eq!(
            DiskObjectFile::parse(&bytes),
            Err(IconFileError::BadMagic(0x4E75))
        );
        assert!(
            IconFileError::BadMagic(0x4E75)
                .to_string()
                .contains("0x4e75")
        );
    }

    #[test]
    fn an_empty_file_is_truncated_not_a_panic() {
        assert_eq!(
            DiskObjectFile::parse(&[]),
            Err(IconFileError::Truncated {
                offset: 0,
                needed: 2
            })
        );
    }

    #[test]
    fn every_proper_prefix_of_a_valid_icon_fails_cleanly() {
        for icon in [project(), drawer(Some((1, 2)))] {
            let bytes = icon.to_bytes();
            for len in 0..bytes.len() {
                let prefix = &bytes[..len];
                match DiskObjectFile::parse(prefix) {
                    Err(_) => {}
                    // A drawer's optional extension is the one thing a
                    // prefix may legitimately lack (a cut-off one is
                    // just trailing bytes, not part of the icon).
                    Ok((parsed, used)) => {
                        assert!(icon.drawer.is_some(), "len {len}: {parsed:?}");
                        assert_eq!(used, bytes.len() - DRAWER_EXTENSION_SIZE);
                        assert!(used < len || len == used, "len {len}");
                        assert_eq!(parsed.drawer.unwrap().extension, None);
                    }
                }
            }
        }
    }

    #[test]
    fn corrupt_bytes_never_panic() {
        // Flip every byte of a valid icon to a few awkward values: the
        // parser may accept or reject, but must neither panic nor
        // allocate absurdly (a huge tool-type count, a huge image).
        let base = project().to_bytes();
        for i in 0..base.len() {
            for v in [0x00, 0x7F, 0x80, 0xFF] {
                let mut bytes = base.clone();
                bytes[i] = v;
                let _ = DiskObjectFile::parse(&bytes);
            }
        }
    }

    #[test]
    fn a_tool_type_count_the_file_cannot_hold_is_truncation() {
        let mut bytes = DiskObjectFile {
            tool_types: Some(vec![]),
            ..DiskObjectFile::default()
        }
        .to_bytes();
        // Claim 0x4000_0000 bytes of pointer array: ~268M strings.
        let at = DISK_OBJECT_SIZE;
        bytes[at..at + 4].copy_from_slice(&0x4000_0000u32.to_be_bytes());
        assert!(matches!(
            DiskObjectFile::parse(&bytes),
            Err(IconFileError::Truncated { .. })
        ));
    }

    #[test]
    fn writing_what_was_parsed_is_stable() {
        // The in-memory pointer fields are not kept, so bytes from
        // another writer need not come back identical -- but a second
        // trip must not change anything.
        let mut foreign = project().to_bytes();
        foreign[50..54].copy_from_slice(&0x00C0_FFEEu32.to_be_bytes()); // junk but non-zero
        let once = DiskObjectFile::parse(&foreign).unwrap().0.to_bytes();
        let twice = DiskObjectFile::parse(&once).unwrap().0.to_bytes();
        assert_eq!(once, twice);
        assert_eq!(
            DiskObjectFile::parse(&foreign).unwrap().0,
            DiskObjectFile::parse(&once).unwrap().0
        );
    }

    #[test]
    fn stray_bytes_after_a_drawer_are_not_taken_for_its_extension() {
        let mut bytes = drawer(None).to_bytes();
        let classic = bytes.len();
        bytes.extend_from_slice(&[0xAB; 64]);
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(parsed.drawer.unwrap().extension, None);
        assert_eq!(used, classic);
    }

    #[test]
    fn a_cut_off_drawer_extension_is_unread_trailing_bytes() {
        let mut bytes = drawer(None).to_bytes();
        let classic = bytes.len();
        bytes.extend_from_slice(&[1, 2, 3]); // half an extension
        let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(parsed.drawer.unwrap().extension, None);
        assert_eq!(used, classic, "the caller sees 3 unread bytes");
    }

    #[test]
    fn an_images_planes_are_written_at_the_size_its_dimensions_say() {
        let mut icon = project();
        let size = plane_data_size(30, 12, 2) as usize;
        for data in [vec![7u8; size + 10], vec![7u8; size - 4]] {
            icon.gadget_image.as_mut().unwrap().data = data;
            let bytes = icon.to_bytes();
            let (parsed, used) = DiskObjectFile::parse(&bytes).unwrap();
            assert_eq!(used, bytes.len(), "later fields stay aligned");
            assert_eq!(parsed.gadget_image.as_ref().unwrap().data.len(), size);
            assert_eq!(parsed.tool_types, icon.tool_types);
            assert_eq!(parsed.tool_window, icon.tool_window);
        }
    }

    #[test]
    fn an_image_size_a_32_bit_host_cannot_hold_is_truncation_not_overflow() {
        assert_eq!(
            plane_data_size(i16::MAX, i16::MAX, i16::MAX),
            2048 * 2 * 32767 * 32767, // ~4.4e12: past a 32-bit usize
        );
        let mut bytes = project().to_bytes();
        // GadgetImage's header follows the 78-byte DiskObject; make it
        // 32767 x 32767 x 32767.
        for at in [
            DISK_OBJECT_SIZE + 4,
            DISK_OBJECT_SIZE + 6,
            DISK_OBJECT_SIZE + 8,
        ] {
            bytes[at..at + 2].copy_from_slice(&i16::MAX.to_be_bytes());
        }
        assert!(matches!(
            DiskObjectFile::parse(&bytes),
            Err(IconFileError::Truncated { .. })
        ));
    }

    #[test]
    fn a_string_is_written_up_to_its_first_nul() {
        let icon = DiskObjectFile {
            default_tool: Some(b"ab\0cd".to_vec()),
            tool_types: Some(vec![b"X=1\0junk".to_vec()]),
            ..DiskObjectFile::default()
        };
        let (parsed, used) = DiskObjectFile::parse(&icon.to_bytes()).unwrap();
        assert_eq!(parsed.default_tool.as_deref(), Some(&b"ab"[..]));
        assert_eq!(parsed.tool_types, Some(vec![b"X=1".to_vec()]));
        assert_eq!(used, icon.to_bytes().len());
    }

    #[test]
    fn builtin_defaults_have_drawer_data_only_for_drawer_like_types() {
        for t in [WBDISK, WBDRAWER, WBGARBAGE, WBKICK] {
            assert!(DiskObjectFile::builtin_default(t).drawer.is_some(), "{t}");
        }
        for t in [WBTOOL, WBPROJECT] {
            assert!(DiskObjectFile::builtin_default(t).drawer.is_none(), "{t}");
        }
        assert_eq!(DiskObjectFile::builtin_default(WBTOOL).stack_size, 4096);
        assert_eq!(
            DiskObjectFile::builtin_default(WBPROJECT).object_type,
            WBPROJECT
        );
    }

    #[test]
    fn an_image_with_absurd_dimensions_is_written_without_planes_not_allocated() {
        let icon = DiskObjectFile {
            gadget_image: Some(IconImage {
                width: i16::MAX,
                height: i16::MAX,
                depth: i16::MAX,
                data: vec![1, 2, 3],
                ..IconImage::default()
            }),
            ..DiskObjectFile::default()
        };
        let bytes = icon.to_bytes();
        let (back, used) = DiskObjectFile::parse(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert!(back.gadget_image.unwrap().data.is_empty());
    }

    #[test]
    fn the_empty_object_is_unplaced_and_untyped() {
        let e = DiskObjectFile::empty();
        assert_eq!(e.object_type, 0);
        assert_eq!(
            (e.current_x, e.current_y),
            (NO_ICON_POSITION, NO_ICON_POSITION)
        );
        assert!(e.default_tool.is_none() && e.tool_types.is_none() && e.drawer.is_none());
    }

    /// Runs the parser over a directory of real icons, for ground truth
    /// the repository cannot hold (real `.info` files are not ours to
    /// commit): `VOLAMOS_INFO_CORPUS=/some/dir cargo test -p volamos-core
    /// iconfile::tests::real_icons -- --ignored --nocapture`. Every file
    /// that starts with the magic must parse and write back stably, and a
    /// drawer's extension must be recognised (nothing unexplained after it).
    #[test]
    #[ignore = "needs VOLAMOS_INFO_CORPUS=<dir of real .info files>"]
    fn real_icons_parse_and_rewrite_stably() {
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
        let (mut icons, mut forms, mut skipped, mut failures) = (0, 0, 0, Vec::new());
        for path in &files {
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            if bytes.len() < 2 || bytes[..2] != WB_DISKMAGIC.to_be_bytes() {
                skipped += 1; // not an icon: some other kind of .info file
                continue;
            }
            match DiskObjectFile::parse(&bytes) {
                Ok((icon, used)) => {
                    icons += 1;
                    forms += bytes[used..].starts_with(b"FORM") as usize;
                    // A drawer's extension must have been recognised: what
                    // is left over is nothing, or a FORM.
                    if icon.drawer.is_some()
                        && used != bytes.len()
                        && !bytes[used..].starts_with(b"FORM")
                    {
                        failures.push(format!(
                            "{}: drawer with {} unexplained trailing bytes",
                            path.display(),
                            bytes.len() - used
                        ));
                    }
                    let once = icon.to_bytes();
                    let again = DiskObjectFile::parse(&once).map(|(i, _)| i.to_bytes());
                    if again.as_ref() != Ok(&once) {
                        failures.push(format!("{}: unstable rewrite", path.display()));
                    }
                }
                // An empty or cut-off file is a bad file, not a bad parser.
                Err(IconFileError::Truncated { .. }) => skipped += 1,
                Err(e) => failures.push(format!("{}: {e}", path.display())),
            }
        }
        eprintln!(
            "{icons} icons parsed ({forms} with a trailing FORM), {skipped} skipped, {} failed",
            failures.len()
        );
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert!(icons > 0, "no icons found under {root}");
    }
}
