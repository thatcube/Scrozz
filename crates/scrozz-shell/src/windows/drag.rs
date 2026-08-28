//! Dragging a capture out of the overlay, on Windows.
//!
//! # What a Windows drop target actually reads
//!
//! The same question the macOS backend answers, with a different answer. OLE
//! drag-and-drop offers formats through an `IDataObject`; what a target reads
//! depends entirely on what it is:
//!
//! - **`CF_HDROP`** is the universal one. Explorer, every Office application,
//!   every browser's file-upload drop zone, Slack, Discord, Teams — all of them
//!   look for a list of file paths first. Anything that accepts a file accepts
//!   `CF_HDROP`.
//! - **The registered `"PNG"` format** is what image-only surfaces read: a
//!   rich-text editor, a canvas, a chat box that wants an inline image rather
//!   than an attachment. Chromium registers and reads exactly this name.
//! - **`CF_UNICODETEXT`** carrying the path, for the plain-text targets that
//!   would otherwise take nothing at all. Cheap, and it is the difference
//!   between "pasted the path" and "nothing happened".
//!
//! Deliberately **not** offered: `CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`
//! delayed rendering. That pair is the elegant answer — it is how you drag a
//! file that does not exist yet — but the file *does* already exist by the time
//! the drag starts (see the artifact module: it is written eagerly, exactly as
//! on macOS), so a promise would buy nothing and cost the compatibility gap.
//! `CF_HDROP` from a real path is understood by strictly more targets.
//!
//! Also not offered: `CF_DIBV5`. Every target that reads DIB also reads
//! `CF_HDROP`, DIB cannot carry an alpha channel that survives round-tripping
//! through the older clipboard formats reliably, and building one means
//! decoding the PNG for no gain.
//!
//! # Why `DoDragDrop` blocking is correct
//!
//! `DoDragDrop` runs a modal loop and does not return until the drop lands or
//! the user gives up. That looks alarming next to the macOS backend, which
//! returns immediately, but it is the documented and only supported shape: the
//! OS owns the mouse for the duration. The source overlay is hidden around the
//! modal call so its full-work-area, always-on-top window cannot intercept its
//! own drop before the application underneath sees it. The [`DragSession`] is
//! therefore already settled by the time `begin` returns, which the session type
//! explicitly supports.
//!
//! # Lifetime
//!
//! Because the modal loop means `begin` does not return until the drag is over,
//! there is none of the macOS ownership problem here — the data object and drop
//! source live on the stack for exactly as long as they are needed, and Rust's
//! ordinary scoping is sufficient. The temporary file outlives the drag by the
//! usual retention window, handled by the shared artifact layer.
//!
//! # Status
//!
//! **The COM plumbing is type-checked but has never run; the byte layouts do
//! run, everywhere.** The `IDataObject`/`DoDragDrop` path here cross-compiles
//! clean against the documented contracts but has not executed on a Windows
//! machine, so it carries the usual caveat: every native step degrades rather
//! than panicking.
//!
//! One thing that *did* need correcting rather than caveating: the data object
//! is a read/write store, not a fixed list of flavours. The shell's drag-image
//! helper does not keep the thumbnail — it writes it into this object through
//! `SetData` and reads it back later. A `SetData` that declines therefore does
//! not merely degrade the drag image, it removes it. See [`CaptureData`].
//!
//! The `CF_HDROP` and `CF_UNICODETEXT` payload layouts are deliberately *not*
//! in that position. They live in [`crate::drag::hdrop`], are built without a
//! single `windows` type, and their tests run on macOS and Linux CI as well as
//! Windows — because a missing second NUL or a wrong header offset is invisible
//! to the type checker and fatal to a drop. The same is true of the format
//! bookkeeping, which lives in [`crate::drag::formats`] for the same reason:
//! matching rules and ownership transfers are logic, and logic can be tested
//! anywhere. The facts that need the real structures — `size_of::<DROPFILES>()`,
//! and that the portable constants equal the Windows ones — are asserted below.
//!
//! See `docs/drag-matrix.md` for what a human on Windows still has to check.

use std::cell::Cell;
use std::ffi::OsString;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use windows::Win32::Foundation::GlobalFree;
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DV_E_DVTARGETDEVICE, DV_E_FORMATETC, DV_E_TYMED, E_FAIL, E_INVALIDARG,
    E_NOTIMPL, E_OUTOFMEMORY, HANDLE, HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, POINT, S_FALSE,
    S_OK,
};
use windows::Win32::Graphics::Gdi::{
    CopyEnhMetaFileW, DeleteObject, GetObjectType, HBITMAP, HENHMETAFILE, HGDIOBJ, OBJ_BITMAP,
    OBJ_PAL, OBJ_TYPE,
};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, CoTaskMemFree, DVTARGETDEVICE, FORMATETC, IAdviseSink, IDataObject,
    IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0,
    TYMED, TYMED_ENHMF, TYMED_FILE, TYMED_GDI, TYMED_HGLOBAL, TYMED_ISTORAGE, TYMED_ISTREAM,
    TYMED_MFPICT, TYMED_NULL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{
    GHND, GLOBAL_ALLOC_FLAGS, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    CF_BITMAP, CF_HDROP, CF_METAFILEPICT, CF_PALETTE, CF_UNICODETEXT,
};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl,
    OleDuplicateData, OleInitialize, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::{CLSID_DragDropHelper, IDragSourceHelper, SHDRAGIMAGE};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindowVisible, SW_HIDE, SW_SHOWNOACTIVATE, ShowWindow,
};
use windows::core::{
    BOOL, Error as WinError, HRESULT, IUnknown, PCWSTR, PWSTR, Result as WinResult, implement,
};

use scrozz_core::{Error, Result};

use crate::drag::artifact::{ScratchFile, artifact_root, scratch_path};
use crate::drag::formats::{
    FormatKey, FormatStore, TARGET_DEVICE_HEADER, stored_medium, target_device_size,
    target_device_valid,
};
use crate::drag::{
    DragCapability, DragOperation, DragOrigin, DragOutcome, DragPayload, DragPreview, DragSession,
    DragSource, check_origin, preview_hotspot,
};

/// `DoDragDrop` returned because the user released over a target.
const DRAGDROP_S_DROP: HRESULT = HRESULT(0x0004_0100_u32 as i32);
/// `DoDragDrop` returned because the user pressed Escape or right-clicked.
const DRAGDROP_S_CANCEL: HRESULT = HRESULT(0x0004_0101_u32 as i32);
/// `GiveFeedback` asking the shell to draw the cursor itself.
const DRAGDROP_S_USEDEFAULTCURSORS: HRESULT = HRESULT(0x0004_0102_u32 as i32);

/// The left mouse button, as `DoDragDrop` reports key state.
const MK_LBUTTON: u32 = 0x0001;
/// The right mouse button. Pressing it mid-drag is a cancel.
const MK_RBUTTON: u32 = 0x0002;

/// The clipboard format name Chromium registers and reads for inline images.
///
/// A registered name rather than a numbered format, so it has to be looked up
/// at runtime. The lookup is cheap and the id is stable for the session.
fn png_format() -> u16 {
    // SAFETY: `RegisterClipboardFormatW` takes a NUL-terminated wide string and
    // is documented to be callable from any thread at any time. The literal
    // below is NUL-terminated.
    let id = unsafe { RegisterClipboardFormatW(PCWSTR(windows::core::w!("PNG").as_ptr())) };
    // 0 means the format table is full — vanishingly unlikely, and the only
    // consequence is that image-only targets fall back to the file.
    u16::try_from(id).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Bytes into an HGLOBAL
// ---------------------------------------------------------------------------

/// Copies `bytes` into a moveable `HGLOBAL` the receiver will own.
///
/// `GHND` is `GMEM_MOVEABLE | GMEM_ZEROINIT`: moveable because that is what
/// every clipboard format requires, zeroed because several of the structures
/// written here have trailing NUL terminators that are then simply left alone.
fn to_hglobal(bytes: &[u8]) -> WinResult<HGLOBAL> {
    // SAFETY: a non-zero size is passed and the result is checked.
    let handle = unsafe { GlobalAlloc(GHND, bytes.len().max(1))? };

    // SAFETY: `handle` was just allocated and is not yet locked.
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        // SAFETY: `handle` is a live allocation that nothing else owns yet.
        unsafe {
            let _ = GlobalFree(Some(handle));
        }
        return Err(WinError::from(E_OUTOFMEMORY));
    }

    // SAFETY: `ptr` addresses at least `bytes.len()` writable bytes, and the
    // source and destination cannot overlap — one is a fresh allocation.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(handle);
    }
    Ok(handle)
}

/// The `CF_HDROP` payload naming exactly one file.
///
/// A `DROPFILES` header followed by the path as UTF-16, then *two* NULs: one
/// ending the path and one ending the list. Getting that second NUL wrong is
/// the classic way to make a drop silently deliver nothing, so it is spelled
/// out rather than folded into an iterator.
fn hdrop_bytes(path: &Path) -> Vec<u8> {
    crate::drag::hdrop::hdrop(&wide(path))
}

/// The path as a NUL-terminated UTF-16 string, for `CF_UNICODETEXT`.
fn unicode_text_bytes(path: &Path) -> Vec<u8> {
    crate::drag::hdrop::unicode_text(&wide(path))
}

/// The path as UTF-16 code units, with no terminator.
///
/// `encode_wide` rather than a `String` conversion: Windows paths are WTF-16
/// and may hold unpaired surrogates that `to_string_lossy` would replace with
/// U+FFFD, naming a file that does not exist.
fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().collect()
}

use std::os::windows::ffi::OsStrExt;

// ---------------------------------------------------------------------------
// An owned storage medium
// ---------------------------------------------------------------------------

/// An `STGMEDIUM` this object owns and must release exactly once.
///
/// A bare `STGMEDIUM` is a handle with no destructor, so every path that can
/// lose one leaks it. Wrapping it means the ordinary Rust rules — a value
/// dropped when it goes out of scope, a value displaced from a collection
/// returned to its new owner — become the COM release rules for free, including
/// on the error paths, which are the ones that get this wrong.
struct OwnedMedium(STGMEDIUM);

impl Drop for OwnedMedium {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live medium this type owns, released exactly
        // once because `OwnedMedium` is neither `Copy` nor `Clone`.
        // `ReleaseStgMedium` does the right thing for every `tymed`, including
        // `TYMED_NULL` (nothing to free) and a medium carrying
        // `pUnkForRelease` (release that instead of the handle), and zeroes the
        // structure afterwards.
        unsafe { ReleaseStgMedium(&raw mut self.0) };
    }
}

/// A medium wrapping a handle this process owns outright.
///
/// `pUnkForRelease` is null because there is no third party to call back: the
/// handle is ours, and `ReleaseStgMedium` should free it directly.
///
/// The union is written through `hGlobal` whatever `tymed` says. That is
/// deliberate and sound rather than lazy — every handle-shaped arm
/// (`hBitmap`, `hEnhMetaFile`, `hMetaFilePict`, `hGlobal`) is one pointer-sized
/// handle at offset zero, so the bits written are the bits meant, and picking
/// the arm by `tymed` would need a match that produced identical code.
fn handle_medium(tymed: u32, handle: HANDLE) -> STGMEDIUM {
    handle_medium_owned_by(tymed, handle, None)
}

/// The same, for a medium whose provider stays responsible for the handle.
///
/// `controller` becomes `pUnkForRelease`, which switches `ReleaseStgMedium`
/// from "free this handle" to "leave the handle alone and release this
/// interface".
fn handle_medium_owned_by(tymed: u32, handle: HANDLE, controller: Option<IUnknown>) -> STGMEDIUM {
    STGMEDIUM {
        tymed,
        u: STGMEDIUM_0 {
            hGlobal: HGLOBAL(handle.0),
        },
        pUnkForRelease: ManuallyDrop::new(controller),
    }
}

/// Copies a medium, so the copy can be owned and released independently.
///
/// # The two kinds of copy
///
/// `ReleaseStgMedium` frees a medium by two different tables depending on
/// whether `pUnkForRelease` is set, so there are two different right answers
/// here.
///
/// When it is set, the *provider* owns the data and release only drops a
/// reference to it. A copy is then an alias: it carries the same controller,
/// `AddRef`ed, and independently owns nothing but the few things table one
/// still frees. When it is null the receiver owns the data outright, release
/// destroys it, and a copy has to be a genuinely separate thing — a second
/// allocation, a second GDI object, a second file.
///
/// # Errors
///
/// `DV_E_TYMED` for a medium that is not one of the documented kinds, or a GDI
/// handle that is not a kind this can safely duplicate; `E_OUTOFMEMORY` if the
/// copy cannot be allocated; `E_FAIL` if a `TYMED_FILE` copy cannot be written.
fn dup_medium(src: &STGMEDIUM) -> WinResult<STGMEDIUM> {
    // Cloning is an `AddRef` if a controller is set, and nothing if not; the
    // original is left untouched either way.
    let controller: Option<IUnknown> = (*src.pUnkForRelease).clone();

    match controller {
        Some(controller) => alias_medium(src, controller),
        None => own_medium(src),
    }
}

/// A second reference to a medium whose provider retains ownership.
///
/// With `pUnkForRelease` set, `ReleaseStgMedium` frees this much and no more:
///
/// | Medium | Freed |
/// | --- | --- |
/// | `HGLOBAL`, `GDI`, `MFPICT`, `ENHMF` | nothing |
/// | `FILE` | the name string — *not* the file |
/// | `ISTREAM`, `ISTORAGE` | one interface reference |
///
/// and then releases the controller once. So this copy aliases the handles,
/// takes its own allocation of the name string, `AddRef`s the interfaces, and
/// carries its own reference to the controller to balance the `Release` it will
/// eventually cause.
fn alias_medium(src: &STGMEDIUM, controller: IUnknown) -> WinResult<STGMEDIUM> {
    let tymed = src.tymed;
    let controller = Some(controller);

    if tymed == bits(TYMED_NULL)
        || tymed == bits(TYMED_HGLOBAL)
        || tymed == bits(TYMED_GDI)
        || tymed == bits(TYMED_MFPICT)
        || tymed == bits(TYMED_ENHMF)
    {
        // SAFETY: each of these arms is one pointer-sized handle at the union's
        // offset zero, so reading `hGlobal` reads the handle that is live.
        let raw = unsafe { src.u.hGlobal };
        return Ok(handle_medium_owned_by(tymed, HANDLE(raw.0), controller));
    }

    if tymed == bits(TYMED_FILE) {
        // SAFETY: `TYMED_FILE` means `lpszFileName` is the live arm.
        let name = match unsafe { file_name_of(src) } {
            Some(text) => task_wide(text)?,
            None => PWSTR(std::ptr::null_mut()),
        };
        return Ok(STGMEDIUM {
            tymed,
            u: STGMEDIUM_0 { lpszFileName: name },
            pUnkForRelease: ManuallyDrop::new(controller),
        });
    }

    if tymed == bits(TYMED_ISTREAM) {
        // SAFETY: `TYMED_ISTREAM` means `pstm` is the live arm.
        let stream = unsafe { (*src.u.pstm).clone() };
        return Ok(STGMEDIUM {
            tymed,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(stream),
            },
            pUnkForRelease: ManuallyDrop::new(controller),
        });
    }

    if tymed == bits(TYMED_ISTORAGE) {
        // SAFETY: `TYMED_ISTORAGE` means `pstg` is the live arm.
        let storage = unsafe { (*src.u.pstg).clone() };
        return Ok(STGMEDIUM {
            tymed,
            u: STGMEDIUM_0 {
                pstg: ManuallyDrop::new(storage),
            },
            pUnkForRelease: ManuallyDrop::new(controller),
        });
    }

    // Several bits at once, or a value that is not a `TYMED` at all. Either way
    // the medium cannot be identified, and guessing would free the wrong thing.
    Err(WinError::from(DV_E_TYMED))
}

/// A genuinely independent copy of a medium the receiver owns outright.
///
/// Every arm here produces something that can be destroyed without touching the
/// original, because `ReleaseStgMedium` with a null `pUnkForRelease` destroys:
/// global memory with `GlobalFree`, GDI objects with `DeleteObject`, metafiles
/// with `DeleteMetaFile`/`DeleteEnhMetaFile`, interfaces with `Release`, and
/// files by *deleting them from disk*. Two owners of one file name is two
/// deletions of one file.
fn own_medium(src: &STGMEDIUM) -> WinResult<STGMEDIUM> {
    let tymed = src.tymed;

    if tymed == bits(TYMED_NULL) {
        return Ok(handle_medium(tymed, HANDLE(std::ptr::null_mut())));
    }

    if tymed == bits(TYMED_HGLOBAL) {
        // SAFETY: `TYMED_HGLOBAL` means `hGlobal` is the live arm.
        let source = unsafe { src.u.hGlobal };
        let copy = dup_hglobal(source)?;
        return Ok(handle_medium(tymed, HANDLE(copy.0)));
    }

    if tymed == bits(TYMED_GDI) {
        // SAFETY: the union's handle arms coincide, so this reads the GDI
        // handle the medium claims to hold.
        let source = unsafe { src.u.hGlobal };
        let copy = dup_gdi(HGDIOBJ(source.0))?;
        return Ok(handle_medium(tymed, copy));
    }

    if tymed == bits(TYMED_MFPICT) {
        // SAFETY: as above — the metafile-picture handle.
        let source = unsafe { src.u.hGlobal };
        // `CF_METAFILEPICT` is one of the three formats `OleDuplicateData`
        // deep-copies, and it is being named because the medium *is* a metafile
        // picture, not because the clipboard format happens to say so.
        // SAFETY: a live `METAFILEPICT` handle; the result is checked.
        let copy =
            unsafe { OleDuplicateData(HANDLE(source.0), CF_METAFILEPICT, GLOBAL_ALLOC_FLAGS(0)) };
        if copy.is_invalid() {
            return Err(WinError::from(E_OUTOFMEMORY));
        }
        return Ok(handle_medium(tymed, copy));
    }

    if tymed == bits(TYMED_ENHMF) {
        // SAFETY: as above — the enhanced-metafile handle.
        let source = unsafe { src.u.hGlobal };
        // `OleDuplicateData` does *not* special-case `CF_ENHMETAFILE`; it would
        // copy the handle's bytes. `CopyEnhMetaFileW` with a null name is the
        // documented way to clone one in memory.
        // SAFETY: a live `HENHMETAFILE`; the result is checked.
        let copy = unsafe { CopyEnhMetaFileW(HENHMETAFILE(source.0), PCWSTR::null()) };
        if copy.is_invalid() {
            return Err(WinError::from(E_OUTOFMEMORY));
        }
        return Ok(handle_medium(tymed, HANDLE(copy.0)));
    }

    if tymed == bits(TYMED_FILE) {
        return dup_file_medium(src);
    }

    if tymed == bits(TYMED_ISTREAM) {
        // SAFETY: `TYMED_ISTREAM` means `pstm` is the live arm.
        let stream = unsafe { (*src.u.pstm).clone() };
        // A reference, not a `Clone()` of the stream. Release frees exactly one
        // reference, so one `AddRef` is the matching independent ownership; an
        // `IStream::Clone` would additionally give a separate seek pointer,
        // which nothing here needs and which is allowed to fail.
        return Ok(STGMEDIUM {
            tymed,
            u: STGMEDIUM_0 {
                pstm: ManuallyDrop::new(stream),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        });
    }

    if tymed == bits(TYMED_ISTORAGE) {
        // SAFETY: `TYMED_ISTORAGE` means `pstg` is the live arm.
        let storage = unsafe { (*src.u.pstg).clone() };
        return Ok(STGMEDIUM {
            tymed,
            u: STGMEDIUM_0 {
                pstg: ManuallyDrop::new(storage),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        });
    }

    // Several bits at once, or a value that is not a `TYMED` at all.
    Err(WinError::from(DV_E_TYMED))
}

/// Copies the bytes behind a global handle into a fresh one.
///
/// # Why not `OleDuplicateData`
///
/// Because it dispatches on the *clipboard format*, and the format here is
/// whatever private value the caller registered:
///
/// > The CF_METAFILEPICT, CF_PALETTE, or CF_BITMAP formats receive special
/// > handling. They are GDI handles and a new GDI object must be created
/// > instead of just copying the bytes. All other formats are duplicated
/// > byte-wise.
///
/// A private format is safe there — byte-wise is what global memory wants — but
/// only by luck, and the same call is wrong for the other media. Doing it here
/// makes every arm dispatch on the medium and none on the format.
fn dup_hglobal(handle: HGLOBAL) -> WinResult<HGLOBAL> {
    if handle.is_invalid() {
        return Err(WinError::from(E_INVALIDARG));
    }

    // SAFETY: a live global handle, as `TYMED_HGLOBAL` promises.
    let size = unsafe { GlobalSize(handle) };

    // SAFETY: as above; a null result means it could not be locked.
    let locked = unsafe { GlobalLock(handle) };
    if locked.is_null() {
        return Err(WinError::from(E_INVALIDARG));
    }

    // SAFETY: `GlobalSize` bytes are readable through the lock, and the slice
    // is dropped before the matching unlock.
    let copy = to_hglobal(unsafe { std::slice::from_raw_parts(locked.cast::<u8>(), size) });

    // SAFETY: balances the lock above. The documented failure return also means
    // "the lock count reached zero", so the result carries no error to report.
    let _ = unsafe { GlobalUnlock(handle) };

    copy
}

/// Copies a GDI object, working out what it is rather than being told.
///
/// `OleDuplicateData` picks its algorithm from the clipboard format, so a
/// private registered format on `TYMED_GDI` would take the byte-wise path:
/// `GlobalSize` on an `HBITMAP`, which is not a global handle. Asking GDI what
/// the object actually is, and naming the standard format that matches, makes
/// the call correct for any format id.
///
/// # Errors
///
/// `DV_E_TYMED` for a handle that is not live, or is a kind with no defined
/// clipboard duplication — refusing is the only honest answer, because copying
/// it by the wrong algorithm produces a handle that crashes on release.
fn dup_gdi(handle: HGDIOBJ) -> WinResult<HANDLE> {
    // SAFETY: a handle from a medium claiming `TYMED_GDI`. `GetObjectType`
    // returns zero for anything that is not a live GDI object, which is exactly
    // the check being made.
    let kind = OBJ_TYPE(unsafe { GetObjectType(handle) }.cast_signed());

    let format = if kind == OBJ_BITMAP {
        CF_BITMAP
    } else if kind == OBJ_PAL {
        CF_PALETTE
    } else {
        return Err(WinError::from(DV_E_TYMED));
    };

    // SAFETY: a live GDI object of the kind just named, so the special-cased
    // duplication path applies; the result is checked.
    let copy = unsafe { OleDuplicateData(HANDLE(handle.0), format, GLOBAL_ALLOC_FLAGS(0)) };
    if copy.is_invalid() {
        return Err(WinError::from(E_OUTOFMEMORY));
    }
    Ok(copy)
}

/// Copies a file medium — including the file.
///
/// Release deletes the file, so a copy that shared the path would be a second
/// deletion of it: the first `ReleaseStgMedium` would take the file out from
/// under every other holder. The copy therefore gets its own file, and the
/// blocking write that costs is affordable because `TYMED_FILE` never appears
/// on this path in practice.
///
/// The new file is guarded from before the copy starts until the moment the
/// medium is built around it. Everything in between can fail — a copy can die
/// after creating a partial destination, and the task allocation for the path
/// can die after a perfectly good copy — and an unguarded failure would leave a
/// temporary file that nothing in the process still knows about.
fn dup_file_medium(src: &STGMEDIUM) -> WinResult<STGMEDIUM> {
    // SAFETY: `TYMED_FILE` means `lpszFileName` is the live arm.
    let Some(text) = (unsafe { file_name_of(src) }) else {
        return Ok(STGMEDIUM {
            tymed: bits(TYMED_FILE),
            u: STGMEDIUM_0 {
                lpszFileName: PWSTR(std::ptr::null_mut()),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        });
    };

    let from = PathBuf::from(OsString::from_wide(text));
    let scratch =
        ScratchFile::copy(&from, scratch_path(&from)).map_err(|_| WinError::from(E_FAIL))?;

    let name = task_wide(&wide(scratch.path()))?;
    // Ownership passes here and not before: `release` is what stops the guard
    // deleting the file, and nothing between it and the return can fail.
    let _kept = scratch.release();
    Ok(STGMEDIUM {
        tymed: bits(TYMED_FILE),
        u: STGMEDIUM_0 { lpszFileName: name },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

/// The file name a `TYMED_FILE` medium carries, without its terminator.
///
/// # Safety
///
/// `src` must be a live medium whose `lpszFileName` arm is the live one.
unsafe fn file_name_of(src: &STGMEDIUM) -> Option<&[u16]> {
    // SAFETY: the caller promises `lpszFileName` is live.
    let name = unsafe { src.u.lpszFileName };
    if name.is_null() {
        return None;
    }

    // Measured and rebuilt by hand rather than through `PWSTR::as_wide`,
    // because that borrows the local `PWSTR` and the slice has to live as long
    // as the medium instead.
    let mut len = 0usize;
    // SAFETY: a NUL-terminated wide string, so the scan stops inside it.
    while unsafe { name.0.add(len).read() } != 0 {
        len += 1;
    }
    // SAFETY: `len` units precede the terminator just found, and they live for
    // as long as `src` does.
    Some(unsafe { std::slice::from_raw_parts(name.0, len) })
}

/// Copies a wide string into task memory, NUL-terminated.
///
/// `ReleaseStgMedium` frees a `TYMED_FILE` name with the standard allocator, so
/// the copy has to come from the matching one.
fn task_wide(text: &[u16]) -> WinResult<PWSTR> {
    let bytes = text
        .len()
        .checked_add(1)
        .and_then(|len| len.checked_mul(size_of::<u16>()))
        .ok_or_else(|| WinError::from(E_OUTOFMEMORY))?;

    // SAFETY: a non-zero size, and the result is checked before it is written.
    let dst = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
    if dst.is_null() {
        return Err(WinError::from(E_OUTOFMEMORY));
    }

    // SAFETY: `dst` addresses `text.len() + 1` writable `u16`s and was just
    // allocated, so it cannot overlap `text`.
    unsafe {
        std::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
        dst.add(text.len()).write(0);
    }
    Ok(PWSTR(dst))
}

/// Copies bytes into task memory, for a `FORMATETC` the caller will free.
fn task_bytes(bytes: &[u8]) -> WinResult<*mut u8> {
    // SAFETY: the size is non-zero — every target device is at least a header —
    // and the result is checked before it is written.
    let dst = unsafe { CoTaskMemAlloc(bytes.len().max(1)) }.cast::<u8>();
    if dst.is_null() {
        return Err(WinError::from(E_OUTOFMEMORY));
    }
    // SAFETY: `dst` addresses `bytes.len()` writable bytes and was just
    // allocated, so it cannot overlap `bytes`.
    unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len()) };
    Ok(dst)
}

/// A `TYMED` as the unsigned bitmask every structure field stores it in.
const fn bits(tymed: TYMED) -> u32 {
    tymed.0.cast_unsigned()
}

// ---------------------------------------------------------------------------
// The data object
// ---------------------------------------------------------------------------

/// A request, reduced to the fields that decide what answers it.
///
/// # Errors
///
/// `E_INVALIDARG` for a null pointer, which is the documented answer, and
/// `DV_E_DVTARGETDEVICE` for a target device whose header does not describe a
/// structure that could exist.
fn key_of(request: *const FORMATETC) -> WinResult<FormatKey> {
    if request.is_null() {
        return Err(WinError::from(E_INVALIDARG));
    }
    // SAFETY: checked non-null; OLE passes a valid pointer for the call.
    let request = unsafe { &*request };
    Ok(FormatKey {
        format: request.cfFormat,
        aspect: request.dwAspect,
        index: request.lindex,
        tymed: request.tymed,
        // SAFETY: `ptd` is null or a live `DVTARGETDEVICE`, as `FORMATETC`
        // requires; the copy is taken while the caller still owns it.
        device: unsafe { device_bytes(request.ptd) }?,
    })
}

/// A private copy of a target device, taken out of the caller's memory.
///
/// The blob is copied rather than aliased because the caller owns it only for
/// the duration of the call, and it is *part of the key* rather than an
/// ignorable extra: a null `ptd` means
///
/// > whenever the specified data format is independent of the target device or
/// > when the caller doesn't care what device is used
///
/// which is a representation in its own right, not a wildcard matching every
/// device. Dropping it would let two device-specific entries overwrite each
/// other and answer each other's requests.
///
/// # Safety
///
/// `ptd` must be null or address a live `DVTARGETDEVICE`.
///
/// # Errors
///
/// `DV_E_DVTARGETDEVICE` if `tdSize` or any of the four offsets is outside the
/// structure it claims to describe.
unsafe fn device_bytes(ptd: *const DVTARGETDEVICE) -> WinResult<Option<Arc<[u8]>>> {
    if ptd.is_null() {
        return Ok(None);
    }

    let base = ptd.cast::<u8>();
    // SAFETY: a live `DVTARGETDEVICE` is at least its own fixed header, which
    // is the only part read before `tdSize` has been validated.
    let header = unsafe { std::slice::from_raw_parts(base, TARGET_DEVICE_HEADER) };
    let size = target_device_size(header).ok_or_else(|| WinError::from(DV_E_DVTARGETDEVICE))?;

    // SAFETY: `tdSize` has been checked to be at least the header and within a
    // sane bound, and a well-formed structure is that many bytes long.
    let all = unsafe { std::slice::from_raw_parts(base, size) };
    // Only now can the offsets be checked against what they point at. A blob
    // that fails here would be handed on — into a key, out of `EnumFormatEtc`
    // as a fresh allocation — and read past its end by whoever trusted it.
    if !target_device_valid(all) {
        return Err(WinError::from(DV_E_DVTARGETDEVICE));
    }
    Ok(Some(Arc::from(all)))
}

/// The reverse, for enumeration.
///
/// The target device is handed back as a fresh task allocation, because the
/// receiver of an enumerated `FORMATETC` is required to free it and must not be
/// given a pointer into this object.
///
/// # Errors
///
/// `E_OUTOFMEMORY` if the target device copy cannot be allocated.
fn formatetc_of(key: &FormatKey) -> WinResult<FORMATETC> {
    let ptd = match &key.device {
        None => std::ptr::null_mut(),
        Some(device) => task_bytes(device)?.cast::<DVTARGETDEVICE>(),
    };
    Ok(FORMATETC {
        cfFormat: key.format,
        ptd,
        dwAspect: key.aspect,
        lindex: key.index,
        tymed: key.tymed,
    })
}

/// Frees the target device of a `FORMATETC` that [`formatetc_of`] produced.
///
/// # Safety
///
/// `at` must address a `FORMATETC` written by [`formatetc_of`] and not yet
/// handed to anyone else.
unsafe fn free_formatetc(at: *mut FORMATETC) {
    // SAFETY: the caller promises a live `FORMATETC` of this object's making.
    let ptd = unsafe { (*at).ptd };
    if !ptd.is_null() {
        // SAFETY: allocated by `CoTaskMemAlloc` in `formatetc_of`, and freed
        // exactly once because the caller promises it was not handed on.
        unsafe { CoTaskMemFree(Some(ptd.cast())) };
    }
}

/// An enumerator over a fixed list of formats.
///
/// # Why not `SHCreateStdEnumFmtEtc`
///
/// The shell's ready-made enumerator takes an array of `FORMATETC` and copies
/// it. What it does with `ptd` — a pointer into memory this object owns — is
/// not documented, and the two possibilities are "deep-copies it" and "keeps
/// the pointer and hands out a dangling one once this object dies". A page of
/// code removes the question, and lets every enumerated entry carry its own
/// task-allocated device for the caller to free.
#[implement(IEnumFORMATETC, Agile = false)]
struct FormatEnum {
    /// The formats, in the order [`CaptureData::format_list`] chose.
    keys: Vec<FormatKey>,
    /// How far through `keys` this enumerator has been walked.
    at: Cell<usize>,
}

impl FormatEnum {
    /// A new enumerator, positioned at `at`, as the interface OLE wants.
    fn make(keys: Vec<FormatKey>, at: usize) -> IEnumFORMATETC {
        Self {
            keys,
            at: Cell::new(at),
        }
        .into()
    }
}

#[allow(non_snake_case, reason = "COM vtable method names")]
impl IEnumFORMATETC_Impl for FormatEnum_Impl {
    fn Next(&self, count: u32, out: *mut FORMATETC, fetched: *mut u32) -> HRESULT {
        if !fetched.is_null() {
            // SAFETY: checked non-null; the caller owns writable storage here.
            unsafe { fetched.write(0) };
        }
        if out.is_null() {
            return E_INVALIDARG;
        }
        // Only the one-at-a-time form may omit the count-out, because only then
        // is the return value enough to say how many were written.
        if count != 1 && fetched.is_null() {
            return E_INVALIDARG;
        }

        let start = self.at.get();
        let wanted = count as usize;
        let take = wanted.min(self.keys.len().saturating_sub(start));

        for step in 0..take {
            let Ok(entry) = formatetc_of(&self.keys[start + step]) else {
                // Undo what has been written: the caller is told nothing was
                // fetched, so it will free nothing.
                for done in 0..step {
                    // SAFETY: `done < step`, so this entry was written by the
                    // loop above and has not been handed anywhere.
                    unsafe { free_formatetc(out.add(done)) };
                }
                return E_OUTOFMEMORY;
            };
            // SAFETY: the caller promises `count` writable entries at `out`,
            // and `step < take <= count`.
            unsafe { out.add(step).write(entry) };
        }

        self.at.set(start + take);
        if !fetched.is_null() {
            // SAFETY: checked non-null; the caller owns writable storage here.
            unsafe { fetched.write(take.try_into().unwrap_or(u32::MAX)) };
        }
        if take == wanted { S_OK } else { S_FALSE }
    }

    fn Skip(&self, count: u32) -> WinResult<()> {
        let start = self.at.get();
        let wanted = count as usize;
        let take = wanted.min(self.keys.len().saturating_sub(start));
        self.at.set(start + take);

        if take == wanted {
            Ok(())
        } else {
            // S_FALSE: fewer elements than asked for were skipped.
            Err(WinError::from(S_FALSE))
        }
    }

    fn Reset(&self) -> WinResult<()> {
        self.at.set(0);
        Ok(())
    }

    fn Clone(&self) -> WinResult<IEnumFORMATETC> {
        Ok(FormatEnum::make(self.keys.clone(), self.at.get()))
    }
}

/// Which half of a [`CaptureData`] answers a request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Source {
    /// Put here by `SetData`, owned as a medium.
    Extras,
    /// Scrozz's own flavour, rendered from bytes on demand.
    Offered,
}

/// What the drop target reads from — and what the shell writes into.
///
/// # Not a read-only bag
///
/// The obvious shape for a drag source is a fixed list of flavours and a
/// `SetData` that politely declines. That shape is wrong, and wrong in a way
/// that costs the feature rather than degrading it:
///
/// > To support the drag-and-drop helper object, the data object's `SetData`
/// > and `GetData` implementations must be able to accept and return arbitrary
/// > private formats.
///
/// [`IDragSourceHelper::InitializeFromBitmap`] does not hold the thumbnail
/// anywhere of its own. It *writes* it into this object, as
/// `CFSTR_DRAGIMAGEBITS` and a handful of companions, and the shell reads them
/// back out during the drag. A `SetData` returning `E_NOTIMPL` therefore does
/// not produce a slightly worse drag image; it produces none at all, every
/// time, because the helper's first write fails and it gives up. Nothing logs,
/// because [`attach_image`] is best-effort by design.
///
/// So there are two halves here. [`Self::offered`] is what Scrozz means to hand
/// over, byte-backed and fixed for the life of the drag. [`Self::extras`] is
/// whatever the shell put here, owned as media and released with the object.
///
/// # Which wins
///
/// A stored entry is preferred over an offered one *for an equally good match*.
/// That is the `IDataObject` contract — `SetData` sets the data — but it is a
/// tie-break, not a precedence: the device fit is compared first, so a stored
/// entry composed for some printer does not beat an offered one that needs no
/// device at all. In practice the two never collide: the helper writes private
/// registered formats, and Scrozz offers `CF_HDROP`, `CF_UNICODETEXT` and a
/// registered `"PNG"`, so both halves survive intact.
///
/// [`IDragSourceHelper::InitializeFromBitmap`]: https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-idragsourcehelper-initializefrombitmap
#[implement(IDataObject, Agile = false)]
struct CaptureData {
    /// Scrozz's own flavours: format, and the bytes to render on demand.
    ///
    /// Rendered per `GetData` rather than held as handles, because `GetData`
    /// hands ownership to the caller and would otherwise need a duplicate
    /// anyway. One `Vec` per flavour, one `GlobalAlloc` per request.
    offered: FormatStore<Vec<u8>>,
    /// Everything the shell stored through `SetData`.
    ///
    /// `RefCell` because the COM vtable hands out `&self` and this genuinely
    /// mutates. Single-threaded by construction: the object is created inside
    /// [`WinDragSource::begin`], passed to `DoDragDrop` on that same thread, and
    /// dropped before `begin` returns — a modal loop, so no other thread ever
    /// sees it.
    extras: std::cell::RefCell<FormatStore<OwnedMedium>>,
}

impl CaptureData {
    /// Everything this drag offers, in preference order.
    ///
    /// `png` empty means the payload offered no image — an MP4 or a JPEG
    /// capture — and the registered `"PNG"` format is then not advertised at
    /// all. Registering it over the file's bytes would hand a receiver that
    /// prefers pixels something that is not a PNG. `CF_HDROP` is unconditional
    /// because it names a real file whatever is in it.
    fn new(path: &Path, png: &[u8]) -> Self {
        let mut offered = FormatStore::new();
        offered.set(
            FormatKey::content(CF_HDROP.0, bits(TYMED_HGLOBAL)),
            hdrop_bytes(path),
        );
        let png_id = png_format();
        if png_id != 0 && !png.is_empty() {
            offered.set(
                FormatKey::content(png_id, bits(TYMED_HGLOBAL)),
                png.to_vec(),
            );
        }
        offered.set(
            FormatKey::content(CF_UNICODETEXT.0, bits(TYMED_HGLOBAL)),
            unicode_text_bytes(path),
        );
        Self {
            offered,
            extras: std::cell::RefCell::new(FormatStore::new()),
        }
    }

    /// A fresh copy of whatever the shell stored for this request, if anything.
    ///
    /// Copied rather than lent because `GetData` transfers ownership to its
    /// caller: handing back the stored medium itself would leave two owners and
    /// one `ReleaseStgMedium` too many.
    fn stored_copy(&self, request: &FormatKey) -> WinResult<Option<STGMEDIUM>> {
        // What is needed is read out and the borrow ended *before* duplicating,
        // because duplicating calls into COM — `AddRef` on a stream, GDI inside
        // `OleDuplicateData` — and nothing that calls out should do so while
        // this object is borrowed. Re-entering here is far-fetched; a panic if
        // it ever happened would not be.
        let found = {
            let extras = self.extras.borrow();
            // The medium is duplicated under the key it is *stored* under, not
            // the one asked for: a request may name several acceptable media,
            // and only the stored key says which one this actually is.
            extras
                .key_for(request)
                .zip(extras.get(request))
                .map(|(_stored, medium)| {
                    // SAFETY: a bitwise read used only as a source to copy
                    // from. `STGMEDIUM` has no destructor — every owning field
                    // is a `ManuallyDrop` — so this local is not a second owner
                    // and releases nothing when it goes out of scope.
                    unsafe { std::ptr::read(&raw const medium.0) }
                })
        };

        let Some(source) = found else {
            return Ok(None);
        };
        Ok(Some(dup_medium(&source)?))
    }

    /// Whether anything here can answer `request`.
    fn serves(&self, request: &FormatKey) -> bool {
        self.best_source(request).is_some()
    }

    /// Which store answers `request` best.
    ///
    /// Both stores rank their candidates by [`DeviceFit`], and a store consulted
    /// first would otherwise win with a worse one: a private format set by the
    /// drag helper for some printer would beat Scrozz's own device-independent
    /// entry for a request naming no device, even though the second is an exact
    /// match and the first only a picked default. Fit decides; the store only
    /// breaks a tie.
    ///
    /// Ties go to the shell's entries, because `SetData` is a promise that what
    /// was put in comes back out. Nothing Scrozz offers shares a format with the
    /// helper's private ones, so the tie is theoretical — but it has to resolve
    /// the same way in both callers, which is why they share this.
    fn best_source(&self, request: &FormatKey) -> Option<Source> {
        let extras = self.extras.borrow().fit(request);
        let offered = self.offered.fit(request);
        match (extras, offered) {
            (Some(extras), Some(offered)) => Some(if extras <= offered {
                Source::Extras
            } else {
                Source::Offered
            }),
            (Some(_), None) => Some(Source::Extras),
            (None, Some(_)) => Some(Source::Offered),
            (None, None) => None,
        }
    }

    /// The formats available, in enumeration order.
    ///
    /// Scrozz's own first, so a target walking the enumeration in order meets
    /// the file before anything private. The shell's entries follow because
    /// `EnumFormatEtc` is documented to list what `GetData` can supply, and an
    /// object that answers a request it would not enumerate is inconsistent in
    /// a way that is nobody's job to debug.
    fn format_list(&self) -> Vec<FormatKey> {
        let extras = self.extras.borrow();
        self.offered.keys().chain(extras.keys()).collect()
    }
}

#[allow(non_snake_case, reason = "COM vtable method names")]
impl IDataObject_Impl for CaptureData_Impl {
    fn GetData(&self, request: *const FORMATETC) -> WinResult<STGMEDIUM> {
        let key = key_of(request)?;

        match self.best_source(&key) {
            Some(Source::Extras) => self
                .stored_copy(&key)?
                .ok_or_else(|| WinError::from(DV_E_FORMATETC)),
            Some(Source::Offered) => {
                let bytes = self
                    .offered
                    .get(&key)
                    .ok_or_else(|| WinError::from(DV_E_FORMATETC))?;
                let handle = to_hglobal(bytes)?;
                Ok(handle_medium(bits(TYMED_HGLOBAL), HANDLE(handle.0)))
            }
            None => Err(WinError::from(DV_E_FORMATETC)),
        }
    }

    fn GetDataHere(&self, _request: *const FORMATETC, _into: *mut STGMEDIUM) -> WinResult<()> {
        // Caller-allocated storage. Nothing that reads a dragged file uses it,
        // and answering it wrongly is worse than declining.
        Err(WinError::from(E_NOTIMPL))
    }

    fn QueryGetData(&self, request: *const FORMATETC) -> HRESULT {
        let Ok(key) = key_of(request) else {
            return E_INVALIDARG;
        };
        if self.serves(&key) {
            return S_OK;
        }
        // Distinguish "I do not have that format" from "I have it, but not on a
        // medium you will take". Both are refusals, but only the second tells
        // the caller that asking differently would work.
        let any_medium = FormatKey {
            tymed: u32::MAX,
            ..key.clone()
        };
        if self.serves(&any_medium) {
            DV_E_TYMED
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(&self, _request: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        // No format here is a synonym for another, so the canonical form of any
        // request is the request. The documented way to say that is to null the
        // target device and return DATA_S_SAMEFORMATETC.
        if !out.is_null() {
            // SAFETY: checked non-null; OLE owns writable storage here.
            unsafe {
                (*out).ptd = std::ptr::null_mut();
            }
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        format: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> WinResult<()> {
        let requested = key_of(format)?;
        if medium.is_null() {
            return Err(WinError::from(E_INVALIDARG));
        }
        // SAFETY: checked non-null; OLE passes a valid medium for the call.
        let source = unsafe { &*medium };

        // `FORMATETC::tymed` is a *set* — the media the caller is willing to
        // use — while `STGMEDIUM::tymed` names the one thing this medium
        // actually is. The rule for reconciling them lives in the portable
        // format layer, where it can be exercised without a Windows host; see
        // [`stored_medium`] for why the answer is the medium and not the set.
        let Some(tymed) = stored_medium(requested.tymed, source.tymed) else {
            return Err(WinError::from(DV_E_TYMED));
        };
        let key = FormatKey { tymed, ..requested };

        let owned = if release.as_bool() {
            // `fRelease == TRUE` hands ownership over as-is, so the medium is
            // taken bitwise: copying the handle without an `AddRef` is exactly
            // right, because the caller has already given up its reference and
            // must not release it.
            //
            // SAFETY: `source` is a live, initialised medium, and this is the
            // only owner from here on.
            OwnedMedium(unsafe { std::ptr::read(source) })
        } else {
            // The caller keeps its medium, so this object needs one of its own
            // that will outlive the call.
            OwnedMedium(dup_medium(source)?)
        };

        // The displaced entry — if any — is released here, *after* the borrow
        // ends. `ReleaseStgMedium` on a medium carrying `pUnkForRelease` calls
        // out to code this object does not control, and that code is entitled
        // to call back in; dropping under a live `RefCell` borrow would turn
        // that into a panic.
        let displaced = self.extras.borrow_mut().set(key, owned);
        drop(displaced);

        Ok(())
    }

    fn EnumFormatEtc(&self, direction: u32) -> WinResult<IEnumFORMATETC> {
        // DATADIR_GET == 1. The other direction enumerates formats a caller may
        // *set*, and this object accepts anything, which is not a list.
        if direction != 1 {
            return Err(WinError::from(E_NOTIMPL));
        }
        Ok(FormatEnum::make(self.format_list(), 0))
    }

    fn DAdvise(
        &self,
        _format: *const FORMATETC,
        _flags: u32,
        _sink: windows::core::Ref<'_, IAdviseSink>,
    ) -> WinResult<u32> {
        Err(WinError::from(OLE_E_ADVISENOTSUPPORTED))
    }

    fn DUnadvise(&self, _connection: u32) -> WinResult<()> {
        Err(WinError::from(OLE_E_ADVISENOTSUPPORTED))
    }

    fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
        Err(WinError::from(OLE_E_ADVISENOTSUPPORTED))
    }
}

// ---------------------------------------------------------------------------
// The drop source
// ---------------------------------------------------------------------------

/// Decides, frame by frame, whether the drag continues.
#[implement(IDropSource)]
struct CaptureSource;

#[allow(non_snake_case, reason = "COM vtable method names")]
impl IDropSource_Impl for CaptureSource_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        // Escape, or the right button, cancels. Releasing the left button — the
        // one that started the gesture — is the drop. Any other combination
        // means the user is still dragging.
        if escape.as_bool() || keys.0 & MK_RBUTTON != 0 {
            DRAGDROP_S_CANCEL
        } else if keys.0 & MK_LBUTTON == 0 {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        // The shell draws a better cursor than we would, and with the drag
        // image helper installed it draws the thumbnail too.
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

// ---------------------------------------------------------------------------
// The drag image
// ---------------------------------------------------------------------------

/// Attaches the thumbnail that follows the pointer, if it can be built.
///
/// Best-effort by design. A drag with no image is a drag with the default file
/// cursor, which is ordinary Windows behaviour; a drag that failed to start
/// because a thumbnail could not be decoded would be a real regression. Every
/// failure here is logged and swallowed.
fn attach_image(data: &IDataObject, preview: Option<&DragPreview>, cursor: (f64, f64), hwnd: HWND) {
    let Some(preview) = preview else {
        return;
    };
    // SAFETY: `begin` validated that `hwnd` is its live origin window.
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let dpi = if dpi == 0 { 96 } else { dpi };
    if let Err(err) = try_attach_image(data, preview, cursor, dpi) {
        // Deliberately louder than the other best-effort paths. The data object
        // now accepts the helper's private formats, so the documented reasons
        // for this to fail are all environmental — no WIC decoder, a shell
        // without the helper — rather than "we declined to store it". If this
        // ever appears in a log it is worth reading, not routine.
        tracing::warn!(%err, "drag: no drag image; the shell will draw a default cursor");
    }
}

struct OwnedBitmap(Option<HBITMAP>);

impl OwnedBitmap {
    fn new(bitmap: HBITMAP) -> Self {
        Self(Some(bitmap))
    }

    fn release(mut self) -> HBITMAP {
        self.0.take().expect("an owned bitmap has a handle")
    }
}

impl Drop for OwnedBitmap {
    fn drop(&mut self) {
        if let Some(bitmap) = self.0.take() {
            // SAFETY: this guard uniquely owns the bitmap until the drag-image
            // helper explicitly accepts it.
            unsafe {
                let _ = DeleteObject(bitmap.into());
            }
        }
    }
}

/// The fallible half of [`attach_image`].
fn try_attach_image(
    data: &IDataObject,
    preview: &DragPreview,
    cursor: (f64, f64),
    dpi: u32,
) -> WinResult<()> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, HDC,
    };
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat32bppPBGRA,
        IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapInterpolationModeFant,
        WICBitmapPaletteTypeMedianCut, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};

    // SAFETY: both classes are standard in-process COM servers, and COM is
    // initialised on this thread before any of this runs.
    let helper: IDragSourceHelper =
        unsafe { CoCreateInstance(&CLSID_DragDropHelper, None, CLSCTX_INPROC_SERVER)? };
    let factory: IWICImagingFactory =
        unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)? };

    // SAFETY: the stream is created over a slice that outlives every use of it
    // within this function, and WIC is documented to read it synchronously.
    let stream = unsafe { factory.CreateStream()? };
    unsafe {
        stream.InitializeFromMemory(preview.png())?;
    }

    // SAFETY: a live stream is passed and the decoder is used only on success.
    let decoder = unsafe {
        factory.CreateDecoderFromStream(
            &stream,
            std::ptr::null(),
            WICDecodeMetadataCacheOnDemand,
        )?
    };
    let frame = unsafe { decoder.GetFrame(0)? };
    let (target_width, target_height, cursor) = preview_geometry(preview.size(), cursor, dpi);
    let scaler = unsafe { factory.CreateBitmapScaler()? };
    unsafe {
        scaler.Initialize(
            &frame,
            target_width,
            target_height,
            WICBitmapInterpolationModeFant,
        )?;
    }

    // Straight-alpha BGRA, *not* premultiplied. `InitializeFromBitmap` is
    // documented to perform the multiplication itself and to report no error if
    // handed premultiplied input — it simply multiplies again, halving a
    // half-alpha pixel a second time. For a card with a soft shadow that is a
    // visibly dark, visibly wrong thumbnail with nothing in any log.
    //
    // Asked of WIC directly rather than converted here: going via PBGRA and
    // dividing back out is lossy at low alpha, and there is no reason to pay
    // that when the decoder can hand over the flavour wanted. `super::alpha` is
    // the fallback for the pairs WIC declines.
    let mut converter = unsafe { factory.CreateFormatConverter()? };
    let straight = unsafe {
        converter.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )
    };
    let premultiplied = if straight.is_ok() {
        false
    } else {
        // A second converter: one that failed `Initialize` is not documented to
        // be reusable, and reusing it would be guessing.
        let retry = unsafe { factory.CreateFormatConverter()? };
        unsafe {
            retry.Initialize(
                &scaler,
                &GUID_WICPixelFormat32bppPBGRA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeMedianCut,
            )?;
        }
        converter = retry;
        true
    };

    let (mut width, mut height) = (0_u32, 0_u32);
    // SAFETY: both out-pointers address live locals.
    unsafe {
        converter.GetSize(&mut width, &mut height)?;
    }
    if width == 0 || height == 0 {
        return Err(WinError::from(E_INVALIDARG));
    }

    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: u32::try_from(size_of::<BITMAPINFOHEADER>()).unwrap_or(40),
            biWidth: i32::try_from(width).unwrap_or(i32::MAX),
            // Negative height is a top-down DIB, which matches how WIC hands
            // pixels over. Positive would render the thumbnail upside down.
            biHeight: -i32::try_from(height).unwrap_or(i32::MAX),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: `info` describes a valid 32-bit top-down DIB and `bits` receives
    // the pixel pointer. A null DC means the bitmap is not tied to a device.
    let bitmap = unsafe {
        CreateDIBSection(
            Some(HDC::default()),
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            Some(HANDLE::default()),
            0,
        )?
    };
    let bitmap = OwnedBitmap::new(bitmap);
    if bits.is_null() {
        return Err(WinError::from(E_OUTOFMEMORY));
    }

    let stride = width.saturating_mul(4);
    let total = stride.saturating_mul(height);
    // SAFETY: `bits` addresses exactly `total` bytes, which is what the stride
    // and height passed to `CopyPixels` describe.
    unsafe {
        converter.CopyPixels(
            std::ptr::null(),
            stride,
            std::slice::from_raw_parts_mut(bits.cast::<u8>(), total as usize),
        )?;
    }

    if premultiplied {
        // SAFETY: the DIB owns exactly `total` bytes, just written by WIC.
        let pixels = unsafe { std::slice::from_raw_parts_mut(bits.cast::<u8>(), total as usize) };
        crate::drag::alpha::unpremultiply_bgra(pixels);
    }

    // Microsoft documents that IDragSourceHelper takes ownership when called,
    // not only when the final HRESULT is success: it may adopt the bitmap and
    // then fail while publishing private formats to the data object.
    let bitmap = bitmap.release();
    let image = SHDRAGIMAGE {
        sizeDragImage: windows::Win32::Foundation::SIZE {
            cx: i32::try_from(width).unwrap_or(i32::MAX),
            cy: i32::try_from(height).unwrap_or(i32::MAX),
        },
        ptOffset: cursor,
        hbmpDragImage: bitmap,
        // Fully transparent colour key: the bitmap carries its own alpha, so
        // no colour should be treated as see-through.
        crColorKey: windows::Win32::Foundation::COLORREF(0xFFFF_FFFF),
    };

    // SAFETY: all fields are live and the bitmap ownership was transferred
    // immediately above exactly as this API requires.
    unsafe { helper.InitializeFromBitmap(&image, data) }
}

fn preview_geometry(
    size: scrozz_core::LogicalSize,
    cursor: (f64, f64),
    dpi: u32,
) -> (u32, u32, POINT) {
    let scale = f64::from(dpi.max(1)) / 96.0;
    let width = (size.width * scale).round().clamp(1.0, f64::from(u32::MAX)) as u32;
    let height = (size.height * scale)
        .round()
        .clamp(1.0, f64::from(u32::MAX)) as u32;
    let x = (cursor.0 * scale)
        .round()
        .clamp(0.0, f64::from(width.saturating_sub(1))) as i32;
    let y = (cursor.1 * scale)
        .round()
        .clamp(0.0, f64::from(height.saturating_sub(1))) as i32;
    (width, height, POINT { x, y })
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// The Windows drag backend.
#[derive(Debug)]
pub struct WinDragSource {
    _private: (),
}

struct HiddenOrigin {
    hwnd: HWND,
    restore: bool,
}

impl HiddenOrigin {
    fn new(hwnd: HWND) -> Self {
        // SAFETY: `begin` validated that this is its live origin window.
        let restore = unsafe { IsWindowVisible(hwnd).as_bool() };
        if restore {
            // Hidden synchronously before OLE hit-tests the destination. Merely
            // unregistering inbound drops is insufficient: the always-on-top
            // HWND would still sit between OLE and the real target.
            let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
        Self { hwnd, restore }
    }
}

impl Drop for HiddenOrigin {
    fn drop(&mut self) {
        if self.restore {
            // SAFETY: the origin window stays alive for the modal drag call.
            let _ = unsafe { ShowWindow(self.hwnd, SW_SHOWNOACTIVATE) };
        }
    }
}

impl WinDragSource {
    /// Creates the backend, initialising COM for this thread.
    ///
    /// # Errors
    ///
    /// Never fails. `OleInitialize` returning `S_FALSE` means COM was already
    /// initialised — which is the normal case, because winit does it — and an
    /// outright failure is left to surface at `begin`, where it can be reported
    /// as an ordinary refused drag rather than a failure to construct the app.
    pub fn new() -> Result<Self> {
        // SAFETY: callable on any thread; the documented failure modes are
        // returned as an HRESULT rather than raised.
        let hr = unsafe { OleInitialize(None) };
        if hr.is_err() {
            tracing::warn!(?hr, "drag: OleInitialize failed; drags may be refused");
        }
        Ok(Self { _private: () })
    }
}

impl DragSource for WinDragSource {
    fn name(&self) -> &str {
        "Windows/OLE"
    }

    fn capability(&self) -> DragCapability {
        DragCapability::EAGER_FILE_AND_IMAGE
    }

    fn begin(&self, payload: DragPayload, origin: DragOrigin) -> Result<DragSession> {
        check_origin(&origin)?;

        // Rejected before anything native happens, so the diagnosis names the
        // real problem rather than an HRESULT from three calls later.
        let hwnd = HWND(origin.surface().as_ptr().cast());
        if hwnd.is_invalid() {
            return Err(Error::InvalidRequest(
                "drag origin is not a live window handle".to_owned(),
            ));
        }

        // The one encode and the one write, exactly as on macOS.
        let (artifact, bytes) = payload.materialise(&artifact_root())?;
        let file_bytes = std::sync::Arc::new(bytes);

        // The registered "PNG" flavour comes from the image producer, never
        // from the file. Empty means the payload had no image to offer and the
        // format is withheld rather than filled with something that is not one.
        let png = payload
            .image_png(&file_bytes)?
            .unwrap_or_else(|| std::sync::Arc::new(Vec::new()));

        let session = DragSession::new();
        // Attached before anything can fail, so every exit path owns the file.
        session.attach_artifact(artifact);

        let path = session
            .artifact_path()
            .ok_or_else(|| Error::Platform("the drag file went missing".to_owned()))?;

        let data: IDataObject = CaptureData::new(&path, &png).into();
        let source: IDropSource = CaptureSource.into();

        // Where inside the thumbnail the pointer grabbed, so the image does not
        // jump to its own corner the instant the drag starts.
        let card = origin.card();
        let pointer = origin.pointer();
        let grab =
            scrozz_core::LogicalPoint::new(pointer.x - card.origin.x, pointer.y - card.origin.y);
        let cursor = payload.preview().map_or(grab, |preview| {
            preview_hotspot(card.size, grab, preview.size())
        });
        attach_image(&data, payload.preview(), (cursor.x, cursor.y), hwnd);

        let mut effect = DROPEFFECT_NONE;
        let hr = {
            let _hidden = HiddenOrigin::new(hwnd);
            // SAFETY: both interfaces are live for the whole call, and `effect`
            // addresses a live local. `DoDragDrop` runs a modal loop and returns
            // only when the drag is over.
            unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) }
        };

        let outcome = if hr == DRAGDROP_S_DROP {
            if effect == DROPEFFECT_NONE {
                // Released over something that would not take it. Per D21 the
                // card springs back; this is not an error.
                DragOutcome::Rejected
            } else {
                DragOutcome::Accepted(operation_of(effect))
            }
        } else if hr == DRAGDROP_S_CANCEL {
            DragOutcome::Cancelled
        } else {
            DragOutcome::Failed(format!("DoDragDrop failed: {hr:?}"))
        };

        session.finish(outcome);
        Ok(session)
    }
}

/// What the receiver said it did with the capture.
fn operation_of(effect: DROPEFFECT) -> DragOperation {
    use windows::Win32::System::Ole::{DROPEFFECT_LINK, DROPEFFECT_MOVE};
    if effect & DROPEFFECT_COPY != DROPEFFECT_NONE {
        DragOperation::Copy
    } else if effect & DROPEFFECT_MOVE != DROPEFFECT_NONE {
        // Reported, never honoured: the capture also lives in history (D14), so
        // a "move" out of Scrozz destroys nothing.
        DragOperation::Move
    } else if effect & DROPEFFECT_LINK != DROPEFFECT_NONE {
        DragOperation::Link
    } else {
        DragOperation::Generic
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::DVASPECT_CONTENT;
    use windows::Win32::UI::Shell::DROPFILES;

    #[test]
    fn preview_size_and_hotspot_follow_the_origin_windows_dpi() {
        let (width, height, cursor) = preview_geometry(
            scrozz_core::LogicalSize::new(168.0, 84.0),
            (84.0, 42.0),
            192,
        );
        assert_eq!((width, height), (336, 168));
        assert_eq!((cursor.x, cursor.y), (168, 84));

        let (_, _, clamped) = preview_geometry(
            scrozz_core::LogicalSize::new(100.0, 50.0),
            (-20.0, 100.0),
            96,
        );
        assert_eq!((clamped.x, clamped.y), (0, 49));
    }

    /// The one layout fact that can only be checked against the real struct.
    ///
    /// `crate::drag::hdrop` writes a hand-rolled 20-byte header so that its
    /// layout tests run on every platform. That is only sound if 20 really is
    /// `size_of::<DROPFILES>()`, and this is the assertion that ties the two
    /// together. Everything else about the byte layout is covered portably.
    #[test]
    fn the_portable_header_length_matches_the_real_struct() {
        assert_eq!(size_of::<DROPFILES>(), crate::drag::hdrop::DROPFILES_LEN);
    }

    /// And the bytes we hand to Windows really do start the path where the
    /// real struct would end.
    #[test]
    fn the_payload_puts_the_path_after_a_real_sized_header() {
        let path = r"C:\Users\Ann\Screenshot 2024.png";
        let bytes = hdrop_bytes(Path::new(path));

        let units = crate::drag::hdrop::read_units(&bytes[size_of::<DROPFILES>()..]);

        assert_eq!(String::from_utf16_lossy(&units), path);
    }

    #[test]
    fn unicode_text_is_nul_terminated() {
        let bytes = unicode_text_bytes(Path::new(r"C:\a.png"));
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]);
    }

    /// The portable layer spells the two constants it needs as plain integers,
    /// so that its matching rules can be tested on any platform. That is only
    /// sound if the integers are the right ones.
    #[test]
    fn the_portable_constants_match_the_real_ones() {
        assert_eq!(crate::drag::formats::DVASPECT_CONTENT, DVASPECT_CONTENT.0);
        assert_eq!(crate::drag::formats::TYMED_HGLOBAL, bits(TYMED_HGLOBAL));
    }

    #[test]
    fn a_key_survives_the_trip_through_a_formatetc() {
        let key = FormatKey::content(CF_HDROP.0, bits(TYMED_HGLOBAL));
        let mut as_formatetc = fmt_of(&key);
        let round_tripped = key_of(&raw const as_formatetc).expect("not null");
        assert_eq!(round_tripped, key);
        // SAFETY: produced by `formatetc_of` above and handed nowhere else.
        unsafe { free_formatetc(&raw mut as_formatetc) };
    }

    #[test]
    fn a_target_device_survives_the_trip_too() {
        // And as a *copy*: the enumerated entry must not point into this object,
        // because its receiver is required to free what it is given.
        let device = target_device(0xAB);
        let request = with_device(CF_HDROP.0, &device);
        let key = key_of(&raw const request).expect("not null");
        assert!(key.device.is_some(), "the device is part of the identity");

        let mut out = fmt_of(&key);
        assert!(!out.ptd.is_null());
        assert_ne!(out.ptd.cast::<u8>().cast_const(), device.as_ptr());
        // SAFETY: `formatetc_of` copied the whole validated blob.
        let seen = unsafe { std::slice::from_raw_parts(out.ptd.cast::<u8>(), device.len()) };
        assert_eq!(seen, device.as_slice());

        assert_eq!(key_of(&raw const out).expect("not null"), key);
        // SAFETY: produced by `formatetc_of` above and handed nowhere else.
        unsafe { free_formatetc(&raw mut out) };
    }

    #[test]
    fn a_target_device_that_could_not_exist_is_refused() {
        // Degrading it to "no device" would be worse than failing: the entry
        // would collide with the device-independent one.
        let mut broken = target_device(1);
        broken[0..4].copy_from_slice(&4u32.to_le_bytes());
        let request = with_device(CF_HDROP.0, &broken);
        assert_eq!(
            key_of(&raw const request).map_err(|err| err.code()),
            Err(DV_E_DVTARGETDEVICE)
        );
    }

    #[test]
    fn a_null_request_is_rejected_rather_than_read() {
        assert_eq!(key_of(std::ptr::null()).unwrap_err().code(), E_INVALIDARG);
    }

    #[test]
    fn the_file_flavour_is_offered_before_the_image_one() {
        // Order is preference. A target that reads both should attach the file
        // rather than paste a bitmap, because the file keeps the filename.
        let data = CaptureData::new(Path::new(r"C:\a.png"), b"\x89PNG\r\n\x1a\n");
        let keys: Vec<_> = data.offered.keys().collect();
        assert_eq!(keys[0].format, CF_HDROP.0);
        assert!(keys.len() >= 2);
        assert_eq!(
            keys.last().map(|k| k.format),
            Some(CF_UNICODETEXT.0),
            "plain text is the last resort"
        );
    }

    #[test]
    fn every_offered_format_is_enumerated() {
        // A target that enumerates rather than probing must see everything, or
        // it will decline a drag it could have accepted.
        let data = CaptureData::new(Path::new(r"C:\a.png"), b"\x89PNG\r\n\x1a\n");
        let listed = data.format_list();
        assert_eq!(listed.len(), data.offered.len());
        for (entry, key) in listed.iter().zip(data.offered.keys()) {
            assert_eq!(entry.format, key.format);
            assert_eq!(entry.tymed, bits(TYMED_HGLOBAL));
            assert_eq!(entry.index, -1);
            assert!(entry.device.is_none(), "the file flavours want no device");
        }
    }

    #[test]
    fn a_format_this_object_does_not_serve_is_not_matched() {
        let data = CaptureData::new(Path::new(r"C:\a.png"), b"\x89PNG\r\n\x1a\n");
        let bogus = FormatKey::content(0xBEEF, bits(TYMED_HGLOBAL));
        assert!(!data.serves(&bogus));
    }

    #[test]
    fn a_storage_medium_we_cannot_provide_is_declined() {
        // TYMED_ISTREAM only. Every offered flavour here is HGLOBAL, and
        // claiming otherwise would hand the receiver a handle of the wrong kind.
        let data = CaptureData::new(Path::new(r"C:\a.png"), b"\x89PNG\r\n\x1a\n");
        let stream_only = FormatKey::content(CF_HDROP.0, bits(TYMED_ISTREAM));
        assert!(!data.serves(&stream_only));
    }

    // -----------------------------------------------------------------------
    // The half the shell writes
    // -----------------------------------------------------------------------

    /// The format id the drag-image helper stores its bitmap under.
    ///
    /// Registered by name, exactly as the helper does, so this is the same id
    /// it would use — no constant is guessed at and no id is special-cased.
    fn drag_image_bits() -> u16 {
        // SAFETY: a NUL-terminated wide literal, as the call requires.
        unsafe {
            RegisterClipboardFormatW(windows::core::w!("DragImageBits"))
                .try_into()
                .unwrap_or(0)
        }
    }

    /// A `FORMATETC` for `key`, for a test that will not run out of memory.
    fn fmt_of(key: &FormatKey) -> FORMATETC {
        formatetc_of(key).expect("allocate")
    }

    /// A minimal well-formed `DVTARGETDEVICE`, tagged so two differ.
    fn target_device(tag: u8) -> Vec<u8> {
        let mut blob = vec![0u8; TARGET_DEVICE_HEADER + 4];
        let size: u32 = blob.len().try_into().expect("small");
        blob[0..4].copy_from_slice(&size.to_le_bytes());
        blob[TARGET_DEVICE_HEADER] = tag;
        blob
    }

    /// A request for `format` on global memory, aimed at `device`.
    ///
    /// The returned structure borrows `device`, which is what OLE does too: the
    /// caller owns the blob for the duration of the call.
    fn with_device(format: u16, device: &[u8]) -> FORMATETC {
        FORMATETC {
            cfFormat: format,
            ptd: device.as_ptr().cast::<DVTARGETDEVICE>().cast_mut(),
            dwAspect: 1,
            lindex: -1,
            tymed: bits(TYMED_HGLOBAL),
        }
    }

    /// A medium owning a fresh copy of `bytes`, as a caller would hand over.
    fn global_of(bytes: &[u8]) -> STGMEDIUM {
        let handle = to_hglobal(bytes).expect("allocate");
        handle_medium(bits(TYMED_HGLOBAL), HANDLE(handle.0))
    }

    /// Reads `len` bytes back out of a global-memory medium.
    fn read_global(medium: &STGMEDIUM, len: usize) -> Vec<u8> {
        // SAFETY: the medium is TYMED_HGLOBAL, so `hGlobal` is the live arm and
        // addresses at least `len` bytes in every use below.
        unsafe {
            let handle = medium.u.hGlobal;
            let base = GlobalLock(handle).cast::<u8>();
            let copy = std::slice::from_raw_parts(base, len).to_vec();
            let _ = GlobalUnlock(handle);
            copy
        }
    }

    #[test]
    fn the_shell_can_store_a_private_format_and_read_it_back() {
        // The whole point. The drag-image helper does not hold the thumbnail
        // itself — it puts it here and reads it back during the drag — so a
        // data object that cannot do this has no drag image at all.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let format = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
        let payload = b"thumbnail pixels".as_slice();

        // SAFETY: a live object and a live medium; ownership passes to the
        // object because fRelease is TRUE.
        let handed_over = global_of(payload);
        unsafe {
            data.SetData(&raw const format, &raw const handed_over, true)
                .expect("the shell's write must be accepted");
        }

        // SAFETY: as above; the returned medium is owned here and released
        // below.
        let mut back = unsafe { data.GetData(&raw const format) }.expect("and read back");
        assert_eq!(read_global(&back, payload.len()), payload);

        // SAFETY: `GetData` hands ownership over, so this is the release.
        unsafe { ReleaseStgMedium(&raw mut back) };
    }

    #[test]
    fn a_stored_format_is_handed_out_as_a_copy() {
        // GetData transfers ownership. Returning the stored handle itself would
        // leave the receiver and this object both believing they must free it.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let format = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
        let medium = global_of(b"pixels");
        // SAFETY: the union arm is HGLOBAL, matching the tymed just set.
        let stored_handle = unsafe { medium.u.hGlobal };

        // SAFETY: live object, live medium, ownership passed on.
        unsafe {
            data.SetData(&raw const format, &raw const medium, true)
                .expect("accepted");
        }
        // SAFETY: as above.
        let mut first = unsafe { data.GetData(&raw const format) }.expect("read");
        // SAFETY: as above.
        let mut second = unsafe { data.GetData(&raw const format) }.expect("read again");

        // SAFETY: both media are TYMED_HGLOBAL, so `hGlobal` is the live arm.
        unsafe {
            assert_ne!(
                first.u.hGlobal.0, stored_handle.0,
                "a caller must not be handed the object's own handle"
            );
            assert_ne!(
                first.u.hGlobal.0, second.u.hGlobal.0,
                "two reads must not share one handle"
            );
        }

        // SAFETY: both are owned here.
        unsafe {
            ReleaseStgMedium(&raw mut first);
            ReleaseStgMedium(&raw mut second);
        }
    }

    #[test]
    fn a_borrowed_medium_is_duplicated_rather_than_seized() {
        // fRelease == FALSE means the caller keeps its medium. Storing it as-is
        // would free memory that is still the caller's.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let format = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
        let mut mine = global_of(b"still mine");

        // SAFETY: live object and medium; ownership is explicitly retained.
        unsafe {
            data.SetData(&raw const format, &raw const mine, false)
                .expect("accepted");
        }
        // SAFETY: TYMED_HGLOBAL, so `hGlobal` is live in both.
        let stored_elsewhere = unsafe {
            let copy = data.GetData(&raw const format).expect("read");
            let differs = copy.u.hGlobal.0 != mine.u.hGlobal.0;
            let mut copy = copy;
            ReleaseStgMedium(&raw mut copy);
            differs
        };
        assert!(stored_elsewhere, "the object must not have taken my handle");

        // Still readable, because it was never freed.
        assert_eq!(read_global(&mine, 10), b"still mine");
        // SAFETY: ownership never left this scope.
        unsafe { ReleaseStgMedium(&raw mut mine) };
    }

    #[test]
    fn a_stored_format_is_enumerated_alongside_the_offered_ones() {
        let inner = CaptureData::new(Path::new(r"C:\a.png"), b"png");
        let offered = inner.offered.len();
        let data: IDataObject = inner.into();
        let format = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));

        // SAFETY: live object and medium, ownership passed on.
        let handed_over = global_of(b"px");
        unsafe {
            data.SetData(&raw const format, &raw const handed_over, true)
                .expect("accepted");
        }

        // SAFETY: DATADIR_GET; the returned enumerator is owned here.
        let listed = unsafe {
            let e = data.EnumFormatEtc(1).expect("enumerate");
            let mut all = Vec::new();
            let mut one = [FORMATETC::default()];
            let mut got = 0u32;
            while e.Next(&mut one, Some(&raw mut got)).is_ok() && got == 1 {
                all.push(one[0].cfFormat);
            }
            all
        };

        assert_eq!(
            listed.len(),
            offered + 1,
            "one more than we offer ourselves"
        );
        assert_eq!(
            listed.last().copied(),
            Some(drag_image_bits()),
            "the shell's own format comes after ours"
        );
    }

    #[test]
    fn a_wrong_medium_is_reported_differently_from_a_wrong_format() {
        // Both are refusals, but only one of them tells the caller that asking
        // differently would have worked.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();

        let wrong_medium = fmt_of(&FormatKey::content(CF_HDROP.0, bits(TYMED_ISTREAM)));
        let wrong_format = fmt_of(&FormatKey::content(0xBEEF, bits(TYMED_HGLOBAL)));

        // SAFETY: live object, live requests.
        unsafe {
            assert_eq!(data.QueryGetData(&raw const wrong_medium), DV_E_TYMED);
            assert_eq!(data.QueryGetData(&raw const wrong_format), DV_E_FORMATETC);
            assert_eq!(data.QueryGetData(std::ptr::null()), E_INVALIDARG);
        }
    }

    #[test]
    fn storing_a_format_twice_replaces_it() {
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let format = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));

        // SAFETY: live object and media; ownership passes on both times.
        let first = global_of(b"first ");
        let second = global_of(b"second");
        unsafe {
            data.SetData(&raw const format, &raw const first, true)
                .expect("accepted");
            data.SetData(&raw const format, &raw const second, true)
                .expect("accepted again");
        }

        // SAFETY: live object; the medium is owned and released here.
        let mut back = unsafe { data.GetData(&raw const format) }.expect("read");
        assert_eq!(read_global(&back, 6), b"second");
        // SAFETY: owned here.
        unsafe { ReleaseStgMedium(&raw mut back) };
    }

    #[test]
    fn a_medium_that_cannot_be_identified_is_refused_rather_than_guessed_at() {
        // Several tymed bits at once names no single medium, so there is no
        // handle to copy. Guessing would free the wrong kind of thing.
        let ambiguous = STGMEDIUM {
            tymed: bits(TYMED_HGLOBAL) | bits(TYMED_ISTREAM),
            u: windows::Win32::System::Com::STGMEDIUM_0 {
                hGlobal: HGLOBAL(std::ptr::null_mut()),
            },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        };
        let refused = dup_medium(&ambiguous).map_err(|err| err.code());
        assert_eq!(refused.err(), Some(DV_E_TYMED));
    }

    #[test]
    fn a_null_medium_is_rejected_before_it_is_read() {
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let format = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
        // SAFETY: live object; the null is the thing under test.
        let err = unsafe { data.SetData(&raw const format, std::ptr::null(), true) }
            .expect_err("a null medium is not storable");
        assert_eq!(err.code(), E_INVALIDARG);
    }

    #[test]
    fn a_copy_is_the_ordinary_outcome() {
        use windows::Win32::System::Ole::{DROPEFFECT_LINK, DROPEFFECT_MOVE};
        assert_eq!(operation_of(DROPEFFECT_COPY), DragOperation::Copy);
        assert_eq!(operation_of(DROPEFFECT_MOVE), DragOperation::Move);
        assert_eq!(operation_of(DROPEFFECT_LINK), DragOperation::Link);
        assert_eq!(operation_of(DROPEFFECT_NONE), DragOperation::Generic);
        assert_eq!(
            operation_of(DROPEFFECT_COPY | DROPEFFECT_MOVE),
            DragOperation::Copy,
            "a receiver offering both is taking a copy"
        );
    }
    // -----------------------------------------------------------------------
    // The format and the medium must agree
    // -----------------------------------------------------------------------

    #[test]
    fn a_medium_that_is_not_what_the_format_promised_is_refused() {
        // `FORMATETC::tymed` is what the caller will accept; `STGMEDIUM::tymed`
        // is what this actually is. Storing the promise rather than the fact
        // makes `QueryGetData` offer a stream and `GetData` hand over a handle.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let stream_promised = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_ISTREAM)));
        let really_global = global_of(b"pixels");

        // SAFETY: a live object and a live medium; the call is expected to
        // refuse, so ownership does not pass and the medium is released below.
        let refused =
            unsafe { data.SetData(&raw const stream_promised, &raw const really_global, true) };
        assert_eq!(refused.map_err(|err| err.code()), Err(DV_E_TYMED));

        let mut medium = really_global;
        // SAFETY: still owned here, because the call refused it.
        unsafe { ReleaseStgMedium(&raw mut medium) };
    }

    #[test]
    fn the_better_device_fit_wins_across_both_stores() {
        // The bug: `GetData` asked the shell's entries first and took whatever
        // they had, so a printer-specific private format answered a request that
        // named no device at all — beating Scrozz's own device-independent
        // entry, which is an exact answer to that request. Ranking has to run
        // across both stores, not inside each one separately.
        let png = b"\x89PNG\r\n\x1a\n";
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), png).into();
        let printer = target_device(1);
        let mut for_printer = with_device(png_format(), &printer);
        for_printer.tymed = bits(TYMED_HGLOBAL);
        let printer_bytes = global_of(b"printer-specific");

        // SAFETY: live object and medium; ownership passes on success.
        unsafe {
            data.SetData(&raw const for_printer, &raw const printer_bytes, true)
                .expect("a private device-specific entry is storable");
        }

        let indifferent = fmt_of(&FormatKey::content(png_format(), bits(TYMED_HGLOBAL)));

        // SAFETY: live object; both are ordinary reads, and each medium
        // returned is owned by this caller and released below.
        unsafe {
            data.QueryGetData(&raw const indifferent)
                .ok()
                .expect("somebody can answer a device-indifferent request");
            let mut answer = data.GetData(&raw const indifferent).expect("an answer");
            assert_eq!(
                read_global(&answer, png.len()),
                png,
                "the exact fit answered, not the printer's"
            );
            ReleaseStgMedium(&raw mut answer);

            data.QueryGetData(&raw const for_printer)
                .ok()
                .expect("and the printer's request is still answerable");
            let mut answer = data.GetData(&raw const for_printer).expect("an answer");
            assert_eq!(
                read_global(&answer, b"printer-specific".len()),
                b"printer-specific",
                "which is answered by the entry made for it"
            );
            ReleaseStgMedium(&raw mut answer);
        }
    }

    #[test]
    fn what_query_promises_is_what_get_hands_over() {
        // The consistency the shared ranking buys: whichever store wins, both
        // calls must reach the same one. An unknown printer can only be served
        // by the device-independent entry, and that has to be true of the
        // promise as well as the delivery.
        let png = b"\x89PNG\r\n\x1a\n";
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), png).into();
        let known = target_device(1);
        let mut for_known = with_device(png_format(), &known);
        for_known.tymed = bits(TYMED_HGLOBAL);
        let stored = global_of(b"printer-specific");

        // SAFETY: live object and medium; ownership passes on success.
        unsafe {
            data.SetData(&raw const for_known, &raw const stored, true)
                .expect("storable");
        }

        let other = target_device(2);
        let mut for_other = with_device(png_format(), &other);
        for_other.tymed = bits(TYMED_HGLOBAL);

        // SAFETY: live object; ordinary read, medium released below.
        unsafe {
            data.QueryGetData(&raw const for_other)
                .ok()
                .expect("the device-independent entry can stand in");
            let mut answer = data.GetData(&raw const for_other).expect("an answer");
            assert_eq!(
                read_global(&answer, png.len()),
                png,
                "and it is the one that stood in"
            );
            ReleaseStgMedium(&raw mut answer);
        }
    }

    #[test]
    fn a_medium_claiming_two_kinds_at_once_is_refused() {
        // Release has to pick one arm of the union, so a medium that is both a
        // handle and a stream cannot be freed correctly by anyone.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let format = fmt_of(&FormatKey::content(
            drag_image_bits(),
            bits(TYMED_HGLOBAL) | bits(TYMED_ISTREAM),
        ));
        let mut ambiguous = global_of(b"pixels");
        ambiguous.tymed = bits(TYMED_HGLOBAL) | bits(TYMED_ISTREAM);

        // SAFETY: live object, live medium; expected to refuse.
        let refused = unsafe { data.SetData(&raw const format, &raw const ambiguous, true) };
        assert_eq!(refused.map_err(|err| err.code()), Err(DV_E_TYMED));

        ambiguous.tymed = bits(TYMED_HGLOBAL);
        // SAFETY: still owned here; corrected back to its real kind so release
        // frees the right thing.
        unsafe { ReleaseStgMedium(&raw mut ambiguous) };
    }

    #[test]
    fn an_entry_answers_for_the_medium_it_is_not_the_media_offered() {
        // A caller may say "global memory or a stream, either will do". What is
        // stored is one of them, and only that one may be promised back.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let either = fmt_of(&FormatKey::content(
            drag_image_bits(),
            bits(TYMED_HGLOBAL) | bits(TYMED_ISTREAM),
        ));
        let global = global_of(b"pixels");

        // SAFETY: live object and medium; ownership passes on success.
        unsafe {
            data.SetData(&raw const either, &raw const global, true)
                .expect("global memory was one of the media offered");
        }

        let stream_only = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_ISTREAM)));
        let global_only = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));

        // SAFETY: live object; both are plain queries.
        unsafe {
            assert_eq!(
                data.QueryGetData(&raw const stream_only),
                DV_E_TYMED,
                "a stream was never stored, only offered"
            );
            assert_eq!(data.QueryGetData(&raw const global_only), S_OK);
        }

        // SAFETY: owned here once handed over.
        let mut back = unsafe { data.GetData(&raw const global_only) }.expect("read");
        assert_eq!(back.tymed, bits(TYMED_HGLOBAL));
        // SAFETY: as above.
        unsafe { ReleaseStgMedium(&raw mut back) };
    }

    // -----------------------------------------------------------------------
    // The target device is part of the identity
    // -----------------------------------------------------------------------

    #[test]
    fn two_devices_do_not_overwrite_each_other() {
        // A device-specific representation is a representation, not a variant
        // of one. Dropping the device would make the second write replace the
        // first and then answer both requests with it.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let first_device = target_device(1);
        let second_device = target_device(2);
        let first = with_device(drag_image_bits(), &first_device);
        let second = with_device(drag_image_bits(), &second_device);

        let one_bytes = global_of(b"one");
        let two_bytes = global_of(b"two");
        // SAFETY: live object and media; ownership passes on success.
        unsafe {
            data.SetData(&raw const first, &raw const one_bytes, true)
                .expect("accepted");
            data.SetData(&raw const second, &raw const two_bytes, true)
                .expect("accepted");
        }

        // SAFETY: live object; the returned media are owned and released here.
        let mut one = unsafe { data.GetData(&raw const first) }.expect("read one");
        let mut two = unsafe { data.GetData(&raw const second) }.expect("read two");
        assert_eq!(read_global(&one, 3), b"one");
        assert_eq!(read_global(&two, 3), b"two");
        // SAFETY: both owned here.
        unsafe {
            ReleaseStgMedium(&raw mut one);
            ReleaseStgMedium(&raw mut two);
        }
    }

    #[test]
    fn a_device_specific_entry_answers_an_indifferent_request() {
        // A null ptd in a *request* means "the caller doesn't care what device
        // is used", and the object "should pick an appropriate default device"
        // rather than report that the format does not exist.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let device = target_device(7);
        let specific = with_device(drag_image_bits(), &device);

        let bytes = global_of(b"dev");
        // SAFETY: live object and medium; ownership passes on success.
        unsafe {
            data.SetData(&raw const specific, &raw const bytes, true)
                .expect("accepted");
        }

        let anywhere = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
        // SAFETY: live object; a plain query.
        assert!(unsafe { data.QueryGetData(&raw const anywhere) }.is_ok());

        // And the data it hands back is the device-specific entry's.
        // SAFETY: live object; the query above says it answers.
        let mut got = unsafe { data.GetData(&raw const anywhere) }.expect("it answers");
        assert_eq!(read_global(&got, 3), b"dev");
        // SAFETY: ours to free.
        unsafe { ReleaseStgMedium(&raw mut got) };
    }

    #[test]
    fn a_null_medium_is_refused_rather_than_stored() {
        // "The type of medium specified in the pformatetc and pmedium
        // parameters must be the same": a format naming global memory and a
        // medium naming nothing do not agree.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let fmt = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
        let empty = STGMEDIUM {
            tymed: 0,
            u: STGMEDIUM_0 {
                hGlobal: HGLOBAL(std::ptr::null_mut()),
            },
            pUnkForRelease: ManuallyDrop::new(None),
        };

        // SAFETY: live object; the medium carries nothing to leak.
        let err = unsafe { data.SetData(&raw const fmt, &raw const empty, false) }
            .map_err(|err| err.code())
            .expect_err("a null medium is not data");

        assert_eq!(err, DV_E_TYMED);
        // SAFETY: live object; nothing was stored, so nothing answers.
        assert_eq!(unsafe { data.QueryGetData(&raw const fmt) }, DV_E_FORMATETC);
    }

    // -----------------------------------------------------------------------
    // Enumeration
    // -----------------------------------------------------------------------

    #[test]
    fn the_enumerator_walks_skips_resets_and_clones() {
        let data = CaptureData::new(Path::new(r"C:\a.png"), b"png");
        let all = data.format_list();
        let total = all.len();
        assert!(total >= 3, "the offered flavours are the fixture here");

        let enumerator = FormatEnum::make(all.clone(), 0);
        let mut one = [FORMATETC::default()];
        let mut fetched = 0u32;

        // SAFETY: a live enumerator and writable storage for one entry.
        unsafe {
            assert_eq!(
                enumerator.Next(&mut one, Some(&raw mut fetched)),
                S_OK,
                "one entry is available"
            );
            assert_eq!(fetched, 1);
            assert_eq!(one[0].cfFormat, all[0].format);
            free_formatetc(&raw mut one[0]);

            enumerator.Skip(1).expect("a second entry exists to skip");

            let mut rest = vec![FORMATETC::default(); total];
            assert_eq!(
                enumerator.Next(&mut rest, Some(&raw mut fetched)),
                S_FALSE,
                "fewer remain than were asked for"
            );
            assert_eq!(fetched as usize, total - 2);
            for entry in &mut rest[..fetched as usize] {
                free_formatetc(&raw mut *entry);
            }

            // Exhausted, and a clone starts from where this one stands.
            let clone = enumerator.Clone().expect("clone");
            assert_eq!(clone.Next(&mut one, Some(&raw mut fetched)), S_FALSE);
            assert_eq!(fetched, 0);

            enumerator.Reset().expect("reset");
            assert_eq!(enumerator.Next(&mut one, Some(&raw mut fetched)), S_OK);
            assert_eq!(fetched, 1);
            free_formatetc(&raw mut one[0]);
        }
    }

    #[test]
    fn the_enumerator_insists_on_a_count_when_asked_for_several() {
        // With one entry the return value says how many were written. With more
        // than one it cannot, so refusing is the only safe answer.
        let data = CaptureData::new(Path::new(r"C:\a.png"), b"png");
        let enumerator = FormatEnum::make(data.format_list(), 0);
        let mut several = [FORMATETC::default(); 2];

        // SAFETY: a live enumerator and writable storage for two entries.
        assert_eq!(unsafe { enumerator.Next(&mut several, None) }, E_INVALIDARG);
    }

    #[test]
    fn each_enumerated_device_is_the_callers_to_free() {
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let device = target_device(9);
        let specific = with_device(drag_image_bits(), &device);
        let bytes = global_of(b"dev");
        // SAFETY: live object and medium; ownership passes on success.
        unsafe {
            data.SetData(&raw const specific, &raw const bytes, true)
                .expect("accepted");
        }

        // SAFETY: DATADIR_GET.
        let enumerator = unsafe { data.EnumFormatEtc(1) }.expect("enumerate");
        let mut seen = Vec::new();
        let mut one = [FORMATETC::default()];
        let mut fetched = 0u32;
        // SAFETY: a live enumerator and storage for one entry at a time.
        unsafe {
            while enumerator.Next(&mut one, Some(&raw mut fetched)) == S_OK && fetched == 1 {
                seen.push((one[0].cfFormat, one[0].ptd));
                free_formatetc(&raw mut one[0]);
            }
        }

        let device_entry = seen
            .iter()
            .find(|(format, _)| *format == drag_image_bits())
            .expect("the stored format is enumerated");
        assert!(!device_entry.1.is_null(), "its device came with it");
        assert_ne!(
            device_entry.1.cast::<u8>().cast_const(),
            device.as_ptr(),
            "and it is a copy, not a pointer into the caller's blob"
        );
    }

    // -----------------------------------------------------------------------
    // Lifetimes
    // -----------------------------------------------------------------------

    #[test]
    fn a_borrowed_medium_outlives_the_callers_release() {
        // fRelease == FALSE leaves the caller owning its medium, and the caller
        // is entitled to release it the moment the call returns.
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let format = fmt_of(&FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
        let mut theirs = global_of(b"pixels!!");

        // SAFETY: live object and medium; ownership stays with the caller.
        unsafe {
            data.SetData(&raw const format, &raw const theirs, false)
                .expect("accepted");
            ReleaseStgMedium(&raw mut theirs);
        }

        // SAFETY: live object; the returned medium is owned and released here.
        let mut back = unsafe { data.GetData(&raw const format) }.expect("still there");
        assert_eq!(read_global(&back, 8), b"pixels!!");
        // SAFETY: owned here.
        unsafe { ReleaseStgMedium(&raw mut back) };
    }

    #[test]
    fn a_file_medium_is_copied_rather_than_shared() {
        // Release *deletes the file*, so a copy that shared the path would let
        // the first release take it away from everyone else.
        // The fixture stands in for a file a caller already owns, so it is
        // written directly rather than claimed — which means creating the
        // directory too. `scratch_path` names a file under the swept artifact
        // root, and on a machine that has never run a drag that root does not
        // exist yet. `ScratchFile::claim` makes it, but this fixture is written
        // before any claim happens.
        let original = scratch_path(Path::new("fixture.bin"));
        std::fs::create_dir_all(original.parent().expect("a named parent"))
            .expect("the swept root, which a fresh machine has never made");
        std::fs::write(&original, b"contents").expect("write fixture");

        let name = task_wide(&wide(&original)).expect("allocate");
        let mut source = STGMEDIUM {
            tymed: bits(TYMED_FILE),
            u: STGMEDIUM_0 { lpszFileName: name },
            pUnkForRelease: ManuallyDrop::new(None),
        };

        let mut copy = dup_medium(&source).expect("duplicate");
        // SAFETY: both are TYMED_FILE, so `lpszFileName` is the live arm.
        let copied_path = unsafe {
            let text = file_name_of(&copy).expect("a name");
            PathBuf::from(OsString::from_wide(text))
        };
        assert_ne!(copied_path, original, "a second file, not a second name");
        assert_eq!(
            std::fs::read(&copied_path).expect("readable"),
            b"contents",
            "with the same contents"
        );

        // SAFETY: owned here; this is the release that deletes the copy.
        unsafe { ReleaseStgMedium(&raw mut copy) };
        assert!(!copied_path.exists(), "release deletes its own file");
        assert!(original.exists(), "and not anyone else's");

        // SAFETY: owned here; deletes the fixture.
        unsafe { ReleaseStgMedium(&raw mut source) };
        assert!(!original.exists());
    }

    #[test]
    fn a_gdi_handle_that_cannot_be_identified_is_refused() {
        // Copying it by the wrong algorithm produces a handle that crashes on
        // release, which is strictly worse than declining.
        let refused = dup_gdi(HGDIOBJ(std::ptr::null_mut())).map_err(|err| err.code());
        assert_eq!(refused, Err(DV_E_TYMED));
    }
}
