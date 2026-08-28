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
//! OS owns the mouse for the duration. The overlay is a single-window
//! always-on-top surface with no animation that must keep running mid-drag, so
//! blocking its thread for the length of a drag is not observable. The
//! [`DragSession`] is therefore already settled by the time `begin` returns,
//! which the session type explicitly supports.
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

use std::ffi::c_void;
use std::path::Path;

use windows::Win32::Foundation::GlobalFree;
use windows::Win32::Foundation::{
    DATA_S_SAMEFORMATETC, DV_E_FORMATETC, DV_E_TYMED, E_INVALIDARG, E_NOTIMPL, E_OUTOFMEMORY,
    HANDLE, HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, POINT, S_OK,
};
use windows::Win32::System::Com::{
    CoTaskMemAlloc, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC,
    IEnumSTATDATA, STGMEDIUM, TYMED, TYMED_ENHMF, TYMED_FILE, TYMED_GDI, TYMED_HGLOBAL,
    TYMED_ISTORAGE, TYMED_ISTREAM, TYMED_MFPICT, TYMED_NULL,
};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{
    GHND, GLOBAL_ALLOC_FLAGS, GlobalAlloc, GlobalLock, GlobalUnlock,
};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::System::Ole::{
    DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, DoDragDrop, IDropSource, IDropSource_Impl,
    OleDuplicateData, OleInitialize, ReleaseStgMedium,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;
use windows::Win32::UI::Shell::{
    CLSID_DragDropHelper, IDragSourceHelper, SHCreateStdEnumFmtEtc, SHDRAGIMAGE,
};
use windows::core::{BOOL, Error as WinError, HRESULT, PCWSTR, Result as WinResult, implement};

use scrozz_core::{Error, Result};

use crate::drag::artifact::artifact_root;
use crate::drag::formats::{FormatKey, FormatStore};
use crate::drag::{
    DragCapability, DragOperation, DragOrigin, DragOutcome, DragPayload, DragSession, DragSource,
    check_origin,
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
    STGMEDIUM {
        tymed,
        u: windows::Win32::System::Com::STGMEDIUM_0 {
            hGlobal: HGLOBAL(handle.0),
        },
        pUnkForRelease: std::mem::ManuallyDrop::new(None),
    }
}

/// Copies a medium, so the copy can be owned and released independently.
///
/// # Why every `tymed` and not just `TYMED_HGLOBAL`
///
/// The shell's drag-image helper only ever stores global memory, so a
/// memory-only implementation would work today. It would also silently corrupt
/// the first time anything stored anything else: reading `hGlobal` out of a
/// medium that is really a `PWSTR` and handing it to `GlobalFree` is not an
/// error anyone sees, it is a crash somewhere later. Every documented medium is
/// handled, and an undocumented one is refused rather than guessed at.
///
/// # Errors
///
/// `DV_E_TYMED` for a medium that is not one of the documented kinds, and
/// `E_OUTOFMEMORY` if the copy cannot be allocated.
fn dup_medium(src: &STGMEDIUM, format: u16) -> WinResult<STGMEDIUM> {
    let tymed = src.tymed;

    if tymed == bits(TYMED_NULL) {
        // Nothing to copy. Not an error: a caller may legitimately store a
        // format with no medium behind it.
        return Ok(handle_medium(tymed, HANDLE(std::ptr::null_mut())));
    }

    if tymed == bits(TYMED_HGLOBAL)
        || tymed == bits(TYMED_GDI)
        || tymed == bits(TYMED_MFPICT)
        || tymed == bits(TYMED_ENHMF)
    {
        // SAFETY: every one of these media is a handle at the union's offset
        // zero, so reading `hGlobal` reads the handle whichever arm is live.
        let raw = unsafe { src.u.hGlobal };
        // SAFETY: `OleDuplicateData` takes a handle and the clipboard format it
        // belongs to, and special-cases the GDI formats itself. A null return
        // is its documented failure signal.
        let copy = unsafe {
            OleDuplicateData(
                HANDLE(raw.0),
                windows::Win32::System::Ole::CLIPBOARD_FORMAT(format),
                GLOBAL_ALLOC_FLAGS(0),
            )
        };
        if copy.is_invalid() {
            return Err(WinError::from(E_OUTOFMEMORY));
        }
        return Ok(handle_medium(tymed, copy));
    }

    if tymed == bits(TYMED_FILE) {
        // SAFETY: `TYMED_FILE` means `lpszFileName` is a NUL-terminated wide
        // string allocated with `CoTaskMemAlloc`.
        let name = unsafe { src.u.lpszFileName };
        if name.is_null() {
            return Ok(handle_medium(tymed, HANDLE(std::ptr::null_mut())));
        }
        // SAFETY: as above — a live NUL-terminated wide string.
        let text = unsafe { name.as_wide() };
        let bytes = (text.len() + 1) * std::mem::size_of::<u16>();
        // SAFETY: a non-zero size, and the result is checked before use. The
        // copy must come from the task allocator because `ReleaseStgMedium`
        // frees a `TYMED_FILE` name with `CoTaskMemFree`.
        let dst = unsafe { CoTaskMemAlloc(bytes) }.cast::<u16>();
        if dst.is_null() {
            return Err(WinError::from(E_OUTOFMEMORY));
        }
        // SAFETY: `dst` addresses `text.len() + 1` writable `u16`s, and cannot
        // overlap `text` because it was just allocated.
        unsafe {
            std::ptr::copy_nonoverlapping(text.as_ptr(), dst, text.len());
            *dst.add(text.len()) = 0;
        }
        return Ok(STGMEDIUM {
            tymed,
            u: windows::Win32::System::Com::STGMEDIUM_0 {
                lpszFileName: windows::core::PWSTR(dst),
            },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        });
    }

    if tymed == bits(TYMED_ISTREAM) {
        // SAFETY: `TYMED_ISTREAM` means `pstm` is the live arm. Cloning the
        // `Option<IStream>` calls `AddRef`, which is exactly the copy wanted:
        // the reference this object owns, released by `ReleaseStgMedium`.
        let stream = unsafe { (*src.u.pstm).clone() };
        return Ok(STGMEDIUM {
            tymed,
            u: windows::Win32::System::Com::STGMEDIUM_0 {
                pstm: std::mem::ManuallyDrop::new(stream),
            },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        });
    }

    if tymed == bits(TYMED_ISTORAGE) {
        // SAFETY: as above, for the storage arm.
        let storage = unsafe { (*src.u.pstg).clone() };
        return Ok(STGMEDIUM {
            tymed,
            u: windows::Win32::System::Com::STGMEDIUM_0 {
                pstg: std::mem::ManuallyDrop::new(storage),
            },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        });
    }

    // Several bits at once, or a value that is not a `TYMED` at all. Either way
    // the medium cannot be identified, and guessing would free the wrong thing.
    Err(WinError::from(DV_E_TYMED))
}

/// A `TYMED` as the unsigned bitmask every structure field stores it in.
const fn bits(tymed: TYMED) -> u32 {
    tymed.0.cast_unsigned()
}

// ---------------------------------------------------------------------------
// The data object
// ---------------------------------------------------------------------------

/// A request, reduced to the four fields that decide what answers it.
///
/// # Errors
///
/// `E_INVALIDARG` for a null pointer, which is the documented answer.
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
    })
}

/// The reverse, for enumeration.
///
/// `ptd` is null because this object is device-independent — see [`FormatKey`].
fn formatetc_of(key: FormatKey) -> FORMATETC {
    FORMATETC {
        cfFormat: key.format,
        ptd: std::ptr::null_mut(),
        dwAspect: key.aspect,
        lindex: key.index,
        tymed: key.tymed,
    }
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
/// A stored entry is preferred over an offered one for the same request. That
/// is the `IDataObject` contract — `SetData` sets the data — and it is what
/// every reference implementation does. In practice the two never collide: the
/// helper writes private registered formats, and Scrozz offers `CF_HDROP`,
/// `CF_UNICODETEXT` and a registered `"PNG"`, so both halves survive intact.
///
/// [`IDragSourceHelper::InitializeFromBitmap`]: https://learn.microsoft.com/en-us/windows/win32/api/shobjidl_core/nf-shobjidl_core-idragsourcehelper-initializefrombitmap
#[implement(IDataObject)]
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
                .map(|(stored, medium)| {
                    // SAFETY: a bitwise read used only as a source to copy
                    // from. `STGMEDIUM` has no destructor — every owning field
                    // is a `ManuallyDrop` — so this local is not a second owner
                    // and releases nothing when it goes out of scope.
                    (stored.format, unsafe {
                        std::ptr::read(&raw const medium.0)
                    })
                })
        };

        let Some((format, source)) = found else {
            return Ok(None);
        };
        Ok(Some(dup_medium(&source, format)?))
    }

    /// Whether anything here can answer `request`.
    fn serves(&self, request: &FormatKey) -> bool {
        self.extras.borrow().get(request).is_some() || self.offered.get(request).is_some()
    }

    /// The formats available, in the shape `SHCreateStdEnumFmtEtc` wants.
    ///
    /// Scrozz's own first, so a target walking the enumeration in order meets
    /// the file before anything private. The shell's entries follow because
    /// `EnumFormatEtc` is documented to list what `GetData` can supply, and an
    /// object that answers a request it would not enumerate is inconsistent in
    /// a way that is nobody's job to debug.
    fn format_list(&self) -> Vec<FORMATETC> {
        let extras = self.extras.borrow();
        self.offered
            .keys()
            .chain(extras.keys())
            .map(formatetc_of)
            .collect()
    }
}

#[allow(non_snake_case, reason = "COM vtable method names")]
impl IDataObject_Impl for CaptureData_Impl {
    fn GetData(&self, request: *const FORMATETC) -> WinResult<STGMEDIUM> {
        let key = key_of(request)?;

        if let Some(medium) = self.stored_copy(&key)? {
            return Ok(medium);
        }

        let bytes = self
            .offered
            .get(&key)
            .ok_or_else(|| WinError::from(DV_E_FORMATETC))?;
        let handle = to_hglobal(bytes)?;
        Ok(handle_medium(bits(TYMED_HGLOBAL), HANDLE(handle.0)))
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
            ..key
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
        let key = key_of(format)?;
        if medium.is_null() {
            return Err(WinError::from(E_INVALIDARG));
        }
        // SAFETY: checked non-null; OLE passes a valid medium for the call.
        let source = unsafe { &*medium };

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
            OwnedMedium(dup_medium(source, key.format)?)
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
        let formats = self.format_list();
        // SAFETY: `formats` is a live slice for the duration of the call, and
        // `SHCreateStdEnumFmtEtc` is documented to copy it.
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
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
fn attach_image(data: &IDataObject, preview: Option<&[u8]>, cursor: POINT) {
    let Some(png) = preview else {
        return;
    };
    if let Err(err) = try_attach_image(data, png, cursor) {
        // Deliberately louder than the other best-effort paths. The data object
        // now accepts the helper's private formats, so the documented reasons
        // for this to fail are all environmental — no WIC decoder, a shell
        // without the helper — rather than "we declined to store it". If this
        // ever appears in a log it is worth reading, not routine.
        tracing::warn!(%err, "drag: no drag image; the shell will draw a default cursor");
    }
}

/// The fallible half of [`attach_image`].
fn try_attach_image(data: &IDataObject, png: &[u8], cursor: POINT) -> WinResult<()> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, HDC,
    };
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat32bppBGRA, GUID_WICPixelFormat32bppPBGRA,
        IWICImagingFactory, WICBitmapDitherTypeNone, WICBitmapPaletteTypeMedianCut,
        WICDecodeMetadataCacheOnDemand,
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
        stream.InitializeFromMemory(png)?;
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
            &frame,
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
                &frame,
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

    // SAFETY: `image` owns a live bitmap; on success the helper takes it, and
    // on failure it is freed below.
    let taken = unsafe { helper.InitializeFromBitmap(&image, data) };
    if taken.is_err() {
        // SAFETY: the helper did not take ownership, so it is still ours.
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::DeleteObject(bitmap.into());
        }
    }
    taken
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// The Windows drag backend.
#[derive(Debug)]
pub struct WinDragSource {
    _private: (),
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
        DragCapability::FULL
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
        #[expect(
            clippy::cast_possible_truncation,
            reason = "screen coordinates, well inside i32"
        )]
        let cursor = POINT {
            x: (pointer.x - card.origin.x) as i32,
            y: (pointer.y - card.origin.y) as i32,
        };
        attach_image(&data, payload.preview_png(), cursor);

        let mut effect = DROPEFFECT_NONE;
        // SAFETY: both interfaces are live for the whole call, and `effect`
        // addresses a live local. `DoDragDrop` runs a modal loop and returns
        // only when the drag is over.
        let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };

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
        let as_formatetc = formatetc_of(key);
        let round_tripped = key_of(&raw const as_formatetc).expect("not null");
        assert_eq!(round_tripped, key);
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
            assert_eq!(entry.cfFormat, key.format);
            assert_eq!(entry.tymed, bits(TYMED_HGLOBAL));
            assert_eq!(entry.lindex, -1);
            assert!(entry.ptd.is_null(), "this object is device-independent");
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
        let format = formatetc_of(FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
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
        let format = formatetc_of(FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
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
        let format = formatetc_of(FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
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
        let format = formatetc_of(FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));

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

        let wrong_medium = formatetc_of(FormatKey::content(CF_HDROP.0, bits(TYMED_ISTREAM)));
        let wrong_format = formatetc_of(FormatKey::content(0xBEEF, bits(TYMED_HGLOBAL)));

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
        let format = formatetc_of(FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));

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
        let refused = dup_medium(&ambiguous, CF_HDROP.0).map_err(|err| err.code());
        assert_eq!(refused.err(), Some(DV_E_TYMED));
    }

    #[test]
    fn a_null_medium_is_rejected_before_it_is_read() {
        let data: IDataObject = CaptureData::new(Path::new(r"C:\a.png"), b"png").into();
        let format = formatetc_of(FormatKey::content(drag_image_bits(), bits(TYMED_HGLOBAL)));
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
}
