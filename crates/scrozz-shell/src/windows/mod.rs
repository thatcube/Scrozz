//! Native Windows integration: the thin layer that talks to `user32`.
//!
//! Every decision has already been made in [`crate::win32`], which is pure and
//! unit-tested on any host. What is left here is FFI and nothing else, so that
//! the part which cannot be reached from the developer's Mac is also the part
//! with no arithmetic in it to get wrong.

pub mod apartment;
pub mod overlay;

// ---------------------------------------------------------------------------
// Proof that `win32_drag`'s hand-written offsets match the real structure.
//
// `crate::win32_drag` encodes a `FILEGROUPDESCRIPTORW` as bytes, which is what
// lets its layout be tested on a machine that is not Windows. The cost of that
// choice is that the offsets are written out by hand, and a wrong one produces
// a garbled file name rather than a compile error.
//
// So it produces a compile error here instead. `Win32_UI_Shell` is not enabled
// — the drag FFI is not in this slice — but the descriptor is built entirely
// from `Win32_Foundation` and `windows_core` types that are, so its size can be
// recomputed from the real members and compared.
// ---------------------------------------------------------------------------

const _: () = {
    use windows::Win32::Foundation::{FILETIME, POINTL, SIZE};

    // dwFlags + clsid + sizel + pointl + dwFileAttributes + three FILETIMEs
    // + nFileSizeHigh + nFileSizeLow + cFileName[MAX_PATH].
    let real = size_of::<u32>()
        + size_of::<windows::core::GUID>()
        + size_of::<SIZE>()
        + size_of::<POINTL>()
        + size_of::<u32>()
        + 3 * size_of::<FILETIME>()
        + size_of::<u32>()
        + size_of::<u32>()
        + crate::win32_drag::MAX_PATH * size_of::<u16>();
    assert!(crate::win32_drag::FILE_DESCRIPTOR_SIZE == real);

    // And that the two derived offsets sit where those members actually start.
    let name_at = real - crate::win32_drag::MAX_PATH * size_of::<u16>();
    assert!(crate::win32_drag::FILE_NAME_OFFSET == name_at);
    assert!(crate::win32_drag::FILE_SIZE_OFFSET == name_at - 2 * size_of::<u32>());
    assert!(
        crate::win32_drag::FILE_ATTRIBUTES_OFFSET
            == size_of::<u32>()
                + size_of::<windows::core::GUID>()
                + size_of::<SIZE>()
                + size_of::<POINTL>()
    );
};
