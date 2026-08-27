//! **Drag-out** — handing a capture straight to another application.
//!
//! Per decision D12 this is the hero interaction of the whole product, above
//! copy and above save. Per D21 the gesture that reaches it is *direction*:
//! right or up on a card means "give this to something else". `scrozz-ui`
//! classifies that gesture and reports [`Intent::DragOut`]; this module starts
//! where that report lands and ends when the file is sitting in the other app.
//!
//! [`Intent::DragOut`]: https://docs.rs/scrozz-ui
//!
//! # The one property that matters: promise, do not write
//!
//! A drag-out **must not write the file to disk before the drop**. The
//! receiving application chooses the destination — Slack uploads from memory,
//! Finder wants a real file in a real folder, Figma wants image bytes and no
//! file at all — and the only way to serve all three is to advertise *what*
//! will be produced and produce it only when someone asks.
//!
//! Writing eagerly to a temp directory and copying afterwards is how these
//! tools end up littering `/tmp` with orphaned captures and pasting a path that
//! points at a file the user cannot find. So the payload here carries a
//! [`ByteSource`] — a closure — and not a `Vec<u8>` and never a `PathBuf`.
//!
//! Each platform has a native mechanism for exactly this:
//!
//! | Platform | Promise mechanism | Image alongside |
//! |---|---|---|
//! | macOS | `NSFilePromiseProvider` + delegate | `public.png`, `public.tiff` |
//! | Windows | `CFSTR_FILEDESCRIPTOR` + `CFSTR_FILECONTENTS` delayed rendering | `CF_DIBV5`, `"PNG"` |
//! | Linux/X11 | XDND `text/uri-list` | `image/png` |
//! | Linux/Wayland | `wl_data_device` | `image/png` |
//!
//! Only macOS is implemented. See [`DragCapability`] for what each of the
//! others is missing and why, stated per D8 rather than half-built.
//!
//! # Offering the image too, not only the file
//!
//! A great many drop targets do not accept files at all. A Figma canvas, a
//! rich-text mail composer and a Slack message box all take *image data* on the
//! pasteboard. Offering only a file promise means the drag visibly refuses in
//! roughly half the places a user would try it, which reads as "drag-out is
//! broken" rather than "that app wanted a different flavour". So a payload
//! offers both, and lets the receiver pick.
//!
//! # What is testable without a mouse
//!
//! Everything except the drag itself: filename derivation and sanitising, the
//! type negotiation (extension ⇄ UTI ⇄ MIME), the promise callback producing
//! the right bytes, the coordinate flip that places the drag image under the
//! pointer, and the mapping from platform failure to [`scrozz_core::Error`].
//! `tests/drag.rs` covers those by default and keeps everything that needs a
//! window server behind `#[ignore]`.

use std::fmt;
use std::sync::{Arc, Mutex};

use scrozz_core::{Error, LogicalPoint, LogicalRect, LogicalSize, Result};

use crate::overlay::{AppKitRect, logical_to_appkit};

// ---------------------------------------------------------------------------
// What is being dragged
// ---------------------------------------------------------------------------

/// The file type a drag promises.
///
/// Deliberately a closed set rather than a free-form MIME string. Every drag
/// target on all three platforms wants the type in a *different* vocabulary —
/// a UTI on macOS, a filename extension on Windows, a MIME type on Linux — and
/// a closed enum is what makes those three views provably consistent with each
/// other. A `&str` here would let a caller advertise `image/png` while naming
/// the file `.jpg`, which is exactly the defect that makes a drop land as an
/// unopenable file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DragFormat {
    /// Lossless still image. The default, and what a screenshot should be.
    Png,
    /// Lossy still image, for when the user has chosen a smaller file.
    Jpeg,
    /// Modern still image.
    Webp,
    /// Animated capture.
    Gif,
    /// Recorded video.
    Mp4,
}

impl DragFormat {
    /// The filename extension, without the dot.
    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
        }
    }

    /// The Apple Uniform Type Identifier, as `NSFilePromiseProvider.fileType`
    /// wants it.
    #[must_use]
    pub const fn uti(self) -> &'static str {
        match self {
            Self::Png => "public.png",
            Self::Jpeg => "public.jpeg",
            Self::Webp => "org.webmproject.webp",
            Self::Gif => "com.compuserve.gif",
            Self::Mp4 => "public.mpeg-4",
        }
    }

    /// The MIME type, as XDND and `wl_data_device` want it.
    #[must_use]
    pub const fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
            Self::Mp4 => "video/mp4",
        }
    }

    /// Whether this is a still image, and so can also be offered as raw image
    /// data on the clipboard.
    ///
    /// [`Self::Mp4`] cannot: there is no "video on the pasteboard" flavour that
    /// any real app accepts, so a recording is file-or-nothing.
    #[must_use]
    pub const fn is_still_image(self) -> bool {
        matches!(self, Self::Png | Self::Jpeg | Self::Webp | Self::Gif)
    }

    /// The format an extension names, if it is one Scrozz drags.
    ///
    /// Case-insensitive, and accepts a leading dot, because this is fed by
    /// user-facing settings where both `PNG` and `.png` are what a person
    /// naturally types.
    #[must_use]
    pub fn from_extension(extension: &str) -> Option<Self> {
        let trimmed = extension
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        match trimmed.as_str() {
            "png" => Some(Self::Png),
            "jpg" | "jpeg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            "gif" => Some(Self::Gif),
            "mp4" | "m4v" => Some(Self::Mp4),
            _ => None,
        }
    }
}

impl fmt::Display for DragFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.extension())
    }
}

/// Produces the bytes of a promised file, on demand.
///
/// `Arc<dyn Fn>` rather than `Box<dyn FnOnce>` for two reasons that are both
/// forced by the platform APIs. The promise callback is handed to AppKit, which
/// may invoke it **on a background queue** (so `Send + Sync`), and may invoke it
/// **more than once** if a drop is delivered to several destinations (so `Fn`,
/// not `FnOnce`). A caller that must not re-encode should memoise inside the
/// closure; this module deliberately does not memoise for them, because holding
/// an encoded screenshot alive for the lifetime of a card is exactly the memory
/// cost the promise exists to avoid.
pub type ByteSource = Arc<dyn Fn() -> Result<Vec<u8>> + Send + Sync + 'static>;

/// Wraps a closure as a [`ByteSource`].
///
/// Convenience only — `Arc::new(f)` is the same thing, but requires the caller
/// to spell out the trait object.
pub fn byte_source<F>(f: F) -> ByteSource
where
    F: Fn() -> Result<Vec<u8>> + Send + Sync + 'static,
{
    Arc::new(f)
}

/// The longest filename Scrozz will hand to a drop target.
///
/// 255 **bytes** is the limit on APFS, HFS+, ext4 and NTFS alike. Bytes, not
/// characters: a name of 200 emoji is fine to a human and rejected by every one
/// of those filesystems.
pub const MAX_FILE_NAME_BYTES: usize = 255;

/// The name used when a caller supplies nothing usable.
pub const FALLBACK_STEM: &str = "Capture";

/// Reduces an arbitrary string to something safe to hand to a filesystem.
///
/// A capture's name can come from a window title, and window titles contain
/// every hostile character there is: `Finder — /Users/brandon/Desktop` has two
/// path separators and an em-dash, `Terminal — bash — 80×24` has a colon's
/// worth of trouble on HFS+, and a title can be empty or be nothing but spaces.
///
/// The rules, in order:
///
/// 1. Path separators (`/` and `\`), the classic-Mac separator `:`, the Windows
///    reserved set `<>:"|?*`, and every C0/C1 control character including NUL
///    become a space.
/// 2. Runs of whitespace collapse to one space, and the result is trimmed.
/// 3. Leading dots are stripped, so a capture cannot become a hidden file.
/// 4. Trailing dots and spaces are stripped, because Windows silently drops
///    them and two captures would then collide.
/// 5. An empty result becomes [`FALLBACK_STEM`].
///
/// Length is *not* enforced here: the extension has to be accounted for at the
/// same time, so [`PromisedFile::file_name`] does the truncation.
#[must_use]
pub fn sanitise_stem(raw: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*') {
                ' '
            } else {
                c
            }
        })
        .collect();

    let collapsed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim();

    if trimmed.is_empty() {
        FALLBACK_STEM.to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Truncates a stem so that `stem.extension` fits in [`MAX_FILE_NAME_BYTES`].
///
/// Truncation is on a `char` boundary, never mid-codepoint: a filename that is
/// invalid UTF-8 is refused outright by some drop targets and silently mangled
/// by others.
fn fit_stem(stem: &str, extension: &str) -> String {
    // The dot plus the extension are non-negotiable; the stem gets what is left.
    let budget = MAX_FILE_NAME_BYTES.saturating_sub(extension.len() + 1);
    if stem.len() <= budget {
        return stem.to_owned();
    }

    let mut end = budget;
    while end > 0 && !stem.is_char_boundary(end) {
        end -= 1;
    }
    let cut = stem[..end].trim_end_matches(['.', ' ']).trim();
    if cut.is_empty() {
        FALLBACK_STEM.to_owned()
    } else {
        cut.to_owned()
    }
}

/// A file offered to a drop target but not yet produced.
///
/// The whole point of this type is that constructing one is free: it holds a
/// name, a type, and a way to make the bytes *later*.
#[derive(Clone)]
pub struct PromisedFile {
    stem: String,
    format: DragFormat,
    bytes: ByteSource,
}

impl fmt::Debug for PromisedFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The closure has no useful Debug, and printing a placeholder beats
        // making the whole payload undebuggable.
        f.debug_struct("PromisedFile")
            .field("file_name", &self.file_name())
            .field("format", &self.format)
            .field("bytes", &"<producer>")
            .finish()
    }
}

impl PromisedFile {
    /// Promises a file named after `stem`, in `format`, produced by `bytes`.
    ///
    /// `stem` is sanitised on the way in — see [`sanitise_stem`] — so a raw
    /// window title is a perfectly good argument.
    #[must_use]
    pub fn new(stem: &str, format: DragFormat, bytes: ByteSource) -> Self {
        Self {
            stem: sanitise_stem(stem),
            format,
            bytes,
        }
    }

    /// The sanitised name, without extension.
    #[must_use]
    pub fn stem(&self) -> &str {
        &self.stem
    }

    /// The promised type.
    #[must_use]
    pub const fn format(&self) -> DragFormat {
        self.format
    }

    /// The complete filename the drop target will see.
    #[must_use]
    pub fn file_name(&self) -> String {
        let extension = self.format.extension();
        format!("{}.{extension}", fit_stem(&self.stem, extension))
    }

    /// Produces the bytes. Called by the platform layer, at drop time.
    ///
    /// # Errors
    ///
    /// Whatever the producer returns — typically [`Error::Codec`] from an
    /// encoder, or [`Error::TargetGone`] if the capture was dismissed between
    /// the drag starting and the drop landing.
    pub fn produce(&self) -> Result<Vec<u8>> {
        (self.bytes)()
    }

    /// The producer itself, for a platform layer that must hand it to a
    /// callback with a longer life than the payload.
    #[must_use]
    pub fn byte_source(&self) -> ByteSource {
        Arc::clone(&self.bytes)
    }
}

/// The thumbnail that follows the pointer during the drag.
///
/// Eager bytes, unlike the file: this is drawn the instant the drag begins, so
/// there is nothing to defer, and it is a card-sized thumbnail rather than a
/// full-resolution capture.
#[derive(Debug, Clone)]
pub struct DragPreview {
    png: Arc<Vec<u8>>,
    size: LogicalSize,
}

impl DragPreview {
    /// Creates a preview from encoded PNG bytes and the size to draw it at.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for empty bytes or an empty size. Both
    /// produce an invisible drag image, which looks to the user exactly like
    /// the drag failing to start.
    pub fn from_png(png: Vec<u8>, size: LogicalSize) -> Result<Self> {
        if png.is_empty() {
            return Err(Error::InvalidRequest(
                "drag preview has no PNG bytes; the drag image would be invisible".to_owned(),
            ));
        }
        if size.is_empty() {
            return Err(Error::InvalidRequest(format!(
                "drag preview size {}x{} encloses no area",
                size.width, size.height
            )));
        }
        Ok(Self {
            png: Arc::new(png),
            size,
        })
    }

    /// The encoded PNG.
    #[must_use]
    pub fn png(&self) -> &[u8] {
        &self.png
    }

    /// The size to draw it at, in logical points.
    #[must_use]
    pub const fn size(&self) -> LogicalSize {
        self.size
    }
}

/// Everything a drag offers to whatever it is dropped on.
#[derive(Clone)]
pub struct DragPayload {
    file: PromisedFile,
    image: Option<ByteSource>,
    preview: Option<DragPreview>,
}

impl fmt::Debug for DragPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DragPayload")
            .field("file", &self.file)
            .field("image", &self.image.is_some())
            .field("preview", &self.preview)
            .finish()
    }
}

impl DragPayload {
    /// A drag that offers only a promised file.
    #[must_use]
    pub const fn new(file: PromisedFile) -> Self {
        Self {
            file,
            image: None,
            preview: None,
        }
    }

    /// A still image drag: the file *and* the same PNG offered as image data.
    ///
    /// This is the ordinary case, and the one the capture stack uses. The same
    /// producer backs both flavours, so a drop into Finder and a drop into
    /// Figma cost exactly one encode between them.
    #[must_use]
    pub fn png_capture(stem: &str, png: ByteSource) -> Self {
        let file = PromisedFile::new(stem, DragFormat::Png, Arc::clone(&png));
        Self {
            file,
            image: Some(png),
            preview: None,
        }
    }

    /// Also offers raw PNG image data on the clipboard.
    ///
    /// The bytes must be PNG regardless of what the promised file is: PNG is
    /// the one still-image encoding every platform's clipboard understands, and
    /// on macOS `NSImage` re-derives TIFF from it for the apps that want that
    /// instead.
    #[must_use]
    pub fn with_image(mut self, png: ByteSource) -> Self {
        self.image = Some(png);
        self
    }

    /// Sets the thumbnail that follows the pointer.
    #[must_use]
    pub fn with_preview(mut self, preview: DragPreview) -> Self {
        self.preview = Some(preview);
        self
    }

    /// The promised file.
    #[must_use]
    pub const fn file(&self) -> &PromisedFile {
        &self.file
    }

    /// The clipboard image producer, if one was offered.
    #[must_use]
    pub fn image(&self) -> Option<&ByteSource> {
        self.image.as_ref()
    }

    /// The drag thumbnail, if one was supplied.
    #[must_use]
    pub const fn preview(&self) -> Option<&DragPreview> {
        self.preview.as_ref()
    }

    /// The drag thumbnail's PNG bytes, if one was supplied.
    ///
    /// A convenience for platform backends, which want the bytes rather than
    /// the wrapper.
    #[must_use]
    pub fn preview_png(&self) -> Option<&[u8]> {
        self.preview.as_ref().map(DragPreview::png)
    }
}

// ---------------------------------------------------------------------------
// Where the drag starts
// ---------------------------------------------------------------------------

/// An opaque handle to the native surface a drag begins from.
///
/// On macOS this is the `NSView *` that eframe reports as
/// `RawWindowHandle::AppKit { ns_view }` — the same handle
/// [`crate::macos::overlay::MacOverlay::from_ns_view`] takes, so the UI layer
/// passes the one pointer it already has and never names AppKit. On Windows it
/// is an `HWND`; on X11 an `xcb_window_t` widened to a pointer.
#[derive(Debug, Clone, Copy)]
pub struct NativeSurface(*mut std::ffi::c_void);

impl NativeSurface {
    /// Adopts a raw native handle.
    ///
    /// # Safety
    ///
    /// `handle` must be a live native surface of the platform's expected kind —
    /// an `NSView *` on macOS — and must outlive the drag begun from it.
    #[must_use]
    pub const unsafe fn from_raw(handle: *mut std::ffi::c_void) -> Self {
        Self(handle)
    }

    /// A handle that is deliberately absent.
    ///
    /// Safe, unlike [`Self::from_raw`], because nothing can be done with it:
    /// every entry point runs [`check_origin`] first and refuses this. It
    /// exists so callers that have not yet obtained a window can say so
    /// without reaching for `unsafe`.
    #[must_use]
    pub const fn null() -> Self {
        Self(std::ptr::null_mut())
    }

    /// The raw handle.
    #[must_use]
    pub const fn as_ptr(self) -> *mut std::ffi::c_void {
        self.0
    }

    /// Whether the handle is null, which is always a caller bug.
    #[must_use]
    pub fn is_null(self) -> bool {
        self.0.is_null()
    }
}

/// The geometry of the moment the user committed to a drag.
///
/// All coordinates are **window-local logical points with a top-left origin** —
/// Scrozz's convention everywhere, and the space `scrozz-ui` already works in.
/// The flip into AppKit's bottom-left space happens inside the macOS backend,
/// via [`card_rect_in_view`].
#[derive(Debug, Clone, Copy)]
pub struct DragOrigin {
    surface: NativeSurface,
    card: LogicalRect,
    pointer: LogicalPoint,
}

impl DragOrigin {
    /// Describes a drag beginning on `card`, with the pointer at `pointer`.
    ///
    /// `card` is [`scrozz_ui`'s `DragRelease::rect`][release] and `pointer` the
    /// release position, both already in window-local points.
    ///
    /// [release]: https://docs.rs/scrozz-ui
    #[must_use]
    pub const fn new(surface: NativeSurface, card: LogicalRect, pointer: LogicalPoint) -> Self {
        Self {
            surface,
            card,
            pointer,
        }
    }

    /// The surface the drag starts from.
    #[must_use]
    pub const fn surface(self) -> NativeSurface {
        self.surface
    }

    /// The card's rectangle, window-local, top-left origin.
    #[must_use]
    pub const fn card(self) -> LogicalRect {
        self.card
    }

    /// The pointer position, window-local, top-left origin.
    #[must_use]
    pub const fn pointer(self) -> LogicalPoint {
        self.pointer
    }
}

/// Places a card rectangle in a view's own coordinate system.
///
/// Scrozz measures downwards from a window's top-left. An `NSView` measures
/// **upwards from its bottom-left** unless it answers `YES` to `isFlipped`, and
/// getting this wrong does not fail loudly — the drag image simply appears
/// mirrored about the middle of the window, some distance from the pointer,
/// which reads as "the drag animation is broken".
///
/// The returned quadruple is in the view's own space. For an unflipped view
/// that is AppKit's usual bottom-left origin, which is what [`AppKitRect`]
/// documents; for a flipped view the rect passes through unchanged and the `y`
/// is measured from the top.
#[must_use]
pub fn card_rect_in_view(card: LogicalRect, view_height: f64, flipped: bool) -> AppKitRect {
    if flipped {
        AppKitRect::new(
            card.origin.x,
            card.origin.y,
            card.size.width,
            card.size.height,
        )
    } else {
        logical_to_appkit(card, view_height)
    }
}

// ---------------------------------------------------------------------------
// What comes back
// ---------------------------------------------------------------------------

/// What the receiving application did with the drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DragOperation {
    /// A copy was taken. Nearly always what a capture drop means.
    Copy,
    /// The receiver took ownership; the source is expected to remove it.
    ///
    /// Scrozz never honours this as a *move*: a capture also lives in history
    /// (D14), so nothing is destroyed by a drop.
    Move,
    /// A reference was made rather than a copy.
    Link,
    /// The receiver accepted without saying which of the above it did.
    Generic,
}

/// How a drag ended.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DragOutcome {
    /// A drop target took it.
    Accepted(DragOperation),
    /// The drag ended over something that would not take it.
    ///
    /// Not a failure: the card springs back, per D21's "cancel springs it
    /// back". The user aimed at a window that does not accept images.
    Rejected,
    /// The user let go over nothing, or pressed Escape.
    Cancelled,
    /// The platform refused to run the drag, or the promise could not be kept.
    Failed(String),
}

impl DragOutcome {
    /// Whether the capture actually reached another application.
    #[must_use]
    pub const fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }

    /// Whether the card should spring back to its slot.
    ///
    /// True for everything that is not an acceptance: per D21 a drag that did
    /// not land leaves the pile exactly as it was.
    #[must_use]
    pub const fn should_restore_card(&self) -> bool {
        !self.is_accepted()
    }
}

#[derive(Debug, Default)]
struct SessionState {
    outcome: Mutex<Option<DragOutcome>>,
}

/// A drag in flight.
///
/// Handed back by [`DragSource::begin`] and cheap to clone. The UI polls
/// [`Self::outcome`] each frame; until it returns `Some`, the card stays in
/// drag mode.
///
/// Deliberately poll-based rather than callback-based. The alternative is a
/// closure invoked from AppKit's drag callback, which would run *inside* the
/// platform's event dispatch and therefore inside `scrozz-ui`'s frame, with no
/// `&mut` access to the stack that needs updating. Polling costs one mutex
/// acquisition per frame and keeps every mutation on the UI's own terms.
#[derive(Debug, Clone)]
pub struct DragSession {
    state: Arc<SessionState>,
}

impl Default for DragSession {
    fn default() -> Self {
        Self::new()
    }
}

impl DragSession {
    /// A session that has not finished yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(SessionState::default()),
        }
    }

    /// A session that has already finished, for a backend that failed before
    /// the drag could start.
    #[must_use]
    pub fn finished(outcome: DragOutcome) -> Self {
        let session = Self::new();
        session.finish(outcome);
        session
    }

    /// Records the outcome. The first call wins; later ones are ignored.
    ///
    /// Later calls are ignored rather than overwriting because AppKit can
    /// deliver `draggingSession:endedAtPoint:operation:` after the promise has
    /// already reported a failure, and the *first* thing that went wrong is the
    /// one worth showing.
    pub fn finish(&self, outcome: DragOutcome) {
        // A poisoned lock here means a previous holder panicked while recording
        // an outcome. Recovering is strictly better than propagating: the drag
        // is over either way, and a panic in the UI thread over a finished drag
        // helps nobody.
        let mut slot = self
            .state
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_none() {
            *slot = Some(outcome);
        }
    }

    /// The outcome, once there is one.
    #[must_use]
    pub fn outcome(&self) -> Option<DragOutcome> {
        self.state
            .outcome
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the drag is still in flight.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.outcome().is_none()
    }
}

// ---------------------------------------------------------------------------
// The contract
// ---------------------------------------------------------------------------

/// What a platform's drag-out can actually do.
///
/// Queried, never assumed — the same rule D8 imposes on the capture layer, and
/// for the same reason: Wayland's restrictions must not leak into the API as
/// implicit expectations. Onboarding (D26) reads this to decide whether to
/// teach the drag gesture at all, because teaching a gesture that cannot work
/// on the user's compositor is worse than not mentioning it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DragCapability {
    /// Whether a file can be promised rather than written up front.
    pub promised_files: bool,
    /// Whether image bytes can ride along for apps that do not take files.
    pub image_data: bool,
    /// Whether a drag can be started at all.
    pub can_drag: bool,
    /// Why, in one line, for diagnostics and for onboarding copy.
    pub detail: &'static str,
}

impl DragCapability {
    /// Everything works.
    pub const FULL: Self = Self {
        promised_files: true,
        image_data: true,
        can_drag: true,
        detail: "promised files and image data",
    };

    /// Nothing works, for the stated reason.
    #[must_use]
    pub const fn none(detail: &'static str) -> Self {
        Self {
            promised_files: false,
            image_data: false,
            can_drag: false,
            detail,
        }
    }
}

/// Begins an OS drag carrying a capture.
///
/// The whole reason this trait exists is so `scrozz-ui` can act on
/// `Intent::DragOut` without a single `cfg(target_os)`: it holds a
/// `Box<dyn DragSource>`, hands it a payload and an origin, and polls the
/// returned [`DragSession`].
///
/// # Threading
///
/// `begin` must be called on the UI thread — the main thread on macOS, and the
/// thread that owns the window on Windows. Implementations return
/// [`Error::Platform`] rather than panicking if they are called elsewhere.
pub trait DragSource {
    /// Backend name, for diagnostics. Surfaced in bug reports, where "which
    /// drag backend" is the first thing worth knowing.
    fn name(&self) -> &str;

    /// What this backend can do here and now.
    fn capability(&self) -> DragCapability;

    /// Starts the drag.
    ///
    /// Returns as soon as the platform has taken over the pointer; the drop
    /// itself lands later, on the returned session.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidRequest`] for a null surface or an empty card rect.
    /// - [`Error::Platform`] off the UI thread, or if the platform refused to
    ///   begin a session.
    /// - [`Error::Unsupported`] where the platform has no drag mechanism Scrozz
    ///   can drive — see [`DragCapability`].
    fn begin(&self, payload: DragPayload, origin: DragOrigin) -> Result<DragSession>;
}

/// Rejects an origin that cannot produce a visible drag.
///
/// Shared by every backend so the diagnosis is identical on all three
/// platforms: an empty card rect yields a zero-size drag image, which looks
/// precisely like the drag silently failing to start.
///
/// # Errors
///
/// [`Error::InvalidRequest`] for a null surface or an empty card.
pub fn check_origin(origin: &DragOrigin) -> Result<()> {
    if origin.surface().is_null() {
        return Err(Error::InvalidRequest(
            "drag origin has a null native surface; the window handle was not \
             obtained from the live window"
                .to_owned(),
        ));
    }
    if origin.card().is_empty() {
        return Err(Error::InvalidRequest(format!(
            "drag origin card {}x{} encloses no area; the drag image would be invisible",
            origin.card().size.width,
            origin.card().size.height
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Platform selection
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub use crate::macos::drag::MacDragSource as NativeDragSource;

#[cfg(not(target_os = "macos"))]
pub use self::unimplemented_platform::PlannedDragSource as NativeDragSource;

/// The drag backend for this platform.
///
/// # Errors
///
/// Returns [`Error::Platform`] on macOS if called off the main thread. On the
/// platforms that are not implemented yet, construction *succeeds* — so the UI
/// can query [`DragSource::capability`] and explain itself — and only
/// [`DragSource::begin`] reports [`Error::Unsupported`].
pub fn native_drag_source() -> Result<NativeDragSource> {
    NativeDragSource::new()
}

/// The two backends that are designed but not built.
///
/// This module is not a placeholder that returns `todo!()`. Per D8 an
/// unavailable capability is an *outcome*, so it reports itself accurately, and
/// per the same decision the reason is written down rather than discovered
/// later. What is recorded here is exactly what each platform needs, so the
/// work is a matter of writing the FFI rather than re-deriving the design.
///
/// # Windows
///
/// The mechanism is settled: an `IDataObject` offering
///
/// - `CFSTR_FILEDESCRIPTORW` — an `FILEGROUPDESCRIPTORW` naming one file, with
///   `FD_PROGRESSUI` set and no size, so Explorer shows progress and does not
///   demand the length up front;
/// - `CFSTR_FILECONTENTS` at index 0 with `TYMED_ISTREAM` — the delayed
///   rendering that makes this a *promise*. An `IStream` implementation whose
///   first `Read` calls [`PromisedFile::produce`] is the whole trick; returning
///   `TYMED_HGLOBAL` instead would force the bytes to exist before the drop,
///   which is the thing this module is built to avoid;
/// - `CF_DIBV5` and the registered `"PNG"` format for the image flavour;
///
/// handed to `DoDragDrop` with an `IDropSource` that ends the drag on button
/// release, plus `IDragSourceHelper2::InitializeFromBitmap` for the drag image.
///
/// **What blocks it:** `scrozz-shell` declares the `windows` crate with only
/// `Win32_UI_WindowsAndMessaging`, `Win32_Foundation` and `Win32_Graphics_Gdi`.
/// `IDataObject`, `IStream`, `DoDragDrop`, `IDropSource`, `SHCreateMemStream`
/// and the shell clipboard formats live in `Win32_System_Com`,
/// `Win32_System_Ole` and `Win32_UI_Shell`, none of which are enabled. Those
/// three features — and a direct `windows-core` dependency for
/// `#[windows::core::implement]` to generate the COM vtables — are what this
/// needs. That is a `Cargo.toml` change, and is reported rather than made.
///
/// # Linux/X11
///
/// XDND is a client-side protocol over `ClientMessage`: advertise
/// `XdndAware`, publish `text/uri-list` and `image/png` in `XdndTypeList`,
/// grab the pointer, follow it with `XdndPosition`, and answer the
/// `SelectionRequest` that follows `XdndDrop`.
///
/// The honest wrinkle: **`text/uri-list` is a URI, so X11 has no promise.** The
/// bytes must exist at a path before the drop is answered. The nearest thing to
/// a promise is to serve `image/png` from memory — which the direct-transfer
/// targets accept — and to materialise a file under `$XDG_RUNTIME_DIR` only
/// when a target insists on `text/uri-list`, deleting it when the session ends.
/// That is a real difference from macOS and Windows and is stated rather than
/// papered over.
///
/// **What blocks it:** the XDND state machine and selection ownership described
/// above are not implemented yet. `x11rb` is available in this crate.
///
/// # Linux/Wayland
///
/// `wl_data_device::start_drag` takes an `origin` surface and — the part that
/// matters — a **serial from a real input event**. The compositor validates
/// that serial against an actual button press it delivered to this client. So:
///
/// - A Wayland client **can** initiate a drag, but **only** from inside the
///   handling of a pointer button-press it genuinely received. It cannot start
///   one on a timer, from a hotkey, or from any synthesised gesture.
/// - There is **no promise mechanism at all**. `wl_data_source` offers MIME
///   types and writes bytes into a pipe on request, which is genuinely lazy for
///   `image/png` — better than X11 — but `text/uri-list` again needs a real
///   path.
/// - GNOME and KDE both honour this; per D8 that is the supported ground.
///
/// The consequence for Scrozz is concrete and worth stating plainly: on
/// Wayland the drag must be started from the same input event `scrozz-ui`
/// classified as [`Intent::DragOut`], with that event's serial threaded through
/// [`DragOrigin`]. The current [`DragOrigin`] carries no serial. Adding one is a
/// small change and is deliberately deferred until the Wayland backend is
/// written, rather than guessing its shape now.
///
/// [`Intent::DragOut`]: https://docs.rs/scrozz-ui
#[cfg(not(target_os = "macos"))]
pub mod unimplemented_platform {
    use super::{DragCapability, DragOrigin, DragPayload, DragSession, DragSource, check_origin};
    use scrozz_core::{Error, Result};

    /// The drag backend on a platform whose FFI is designed but not written.
    ///
    /// Constructing one succeeds. Only [`DragSource::begin`] fails, and it
    /// fails with [`Error::Unsupported`], which per D8 is an ordinary handled
    /// outcome and not a crash.
    #[derive(Debug, Default)]
    pub struct PlannedDragSource {
        _private: (),
    }

    impl PlannedDragSource {
        /// Creates the backend.
        ///
        /// # Errors
        ///
        /// Never. The signature matches the macOS backend so
        /// [`super::native_drag_source`] has one shape everywhere.
        pub fn new() -> Result<Self> {
            Ok(Self { _private: () })
        }
    }

    /// Why this platform cannot drag yet, in the terms the module docs set out.
    #[cfg(target_os = "windows")]
    const WHY: &str = "the Windows IDataObject/DoDragDrop backend needs the \
                       Win32_System_Com, Win32_System_Ole and Win32_UI_Shell \
                       features of the `windows` crate, which scrozz-shell does \
                       not declare";

    /// Why this platform cannot drag yet, in the terms the module docs set out.
    #[cfg(target_os = "linux")]
    const WHY: &str = "the X11 XDND state machine is not implemented, and the \
                       Wayland backend additionally needs the input-event serial \
                       that authorised the gesture, which DragOrigin does not yet carry";

    /// Why this platform cannot drag yet, in the terms the module docs set out.
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    const WHY: &str = "Scrozz has no drag backend for this platform";

    impl DragSource for PlannedDragSource {
        fn name(&self) -> &str {
            "planned"
        }

        fn capability(&self) -> DragCapability {
            DragCapability::none(WHY)
        }

        fn begin(&self, payload: DragPayload, origin: DragOrigin) -> Result<DragSession> {
            // Validated even though the drag cannot run, so a caller developing
            // on this platform still finds a null handle or an empty card the
            // moment they introduce it, rather than only once they build for
            // macOS.
            check_origin(&origin)?;
            let _ = payload;
            Err(Error::Unsupported {
                what: "drag-out".to_owned(),
                why: WHY.to_owned(),
            })
        }
    }
}
