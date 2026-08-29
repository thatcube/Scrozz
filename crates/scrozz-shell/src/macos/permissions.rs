//! macOS permission gates: Screen Recording, Microphone, Accessibility.
//!
//! Three different Apple APIs with three different shapes, and none of them is
//! a plain boolean:
//!
//! - **Screen Recording** — `CGPreflightScreenCaptureAccess` is a pure query
//!   with no side effects, which is exactly what D15 needs: Scrozz can check at
//!   the moment of capture without provoking a dialog.
//!   `CGRequestScreenCaptureAccess` shows the system prompt **once, ever**, and
//!   thereafter returns the current answer with no UI at all.
//! - **Microphone** — `AVCaptureDevice` reports a four-state enum rather than a
//!   boolean, and *not determined* is meaningfully different from *denied*: the
//!   first can still be resolved by asking, the second cannot.
//! - **Accessibility** — `AXIsProcessTrustedWithOptions` is both the query and
//!   the request depending on whether you pass the prompt option, and unlike
//!   the other two it never prompts again once the user has been asked.
//! - **Input Monitoring** — `IOHIDCheckAccess` is a pure query and
//!   `IOHIDRequestAccess` prompts only when click/keystroke capture is enabled.
//!
//! # The Info.plist hazard
//!
//! Calling `AVCaptureDevice.requestAccess(for:)` in a process whose `Info.plist`
//! has no `NSMicrophoneUsageDescription` **terminates the process** — that is
//! documented behaviour, not a bug. Reading `authorizationStatus(for:)` is
//! safe without it. Nothing in the test suite may therefore call
//! [`request`] for [`Capability::Microphone`], and the shipped app bundle must
//! carry the key. The same applies to `NSCameraUsageDescription` if a camera
//! overlay is ever added.

use std::ffi::{CStr, c_char, c_int, c_ulong, c_void};
use std::sync::OnceLock;

use objc2::msg_send;
use objc2::rc::Retained;
use objc2::runtime::{AnyClass, Bool};
use objc2_app_kit::NSWorkspace;
use objc2_core_graphics::{CGPreflightScreenCaptureAccess, CGRequestScreenCaptureAccess};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};
use scrozz_core::{Error, Result};

use crate::Capability;
use crate::permissions::{capability_name, settings_pane_url};

// AVFoundation is linked here rather than declared in Cargo.toml because the
// only thing Scrozz needs from it is the authorisation API, which is two
// messages to a class the framework registers when it loads. Naming a real
// symbol in the block — `AVMediaTypeAudio` — is what forces the linker to load
// it, which in turn is what makes `AVCaptureDevice` findable in the runtime.
#[link(name = "AVFoundation", kind = "framework")]
unsafe extern "C" {
    /// `AVMediaTypeAudio`, an `NSString *` constant whose value is `@"soun"`.
    static AVMediaTypeAudio: *const NSString;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    fn IOHIDRequestAccess(request_type: u32) -> u8;
}

const IO_HID_LISTEN_EVENT: u32 = 1;
const IO_HID_ACCESS_GRANTED: u32 = 0;

// `AXIsProcessTrusted` and friends live in HIServices, which is re-exported by
// the ApplicationServices umbrella framework.
#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Whether this process is trusted for Accessibility, without prompting.
    ///
    /// Returns CoreFoundation's `Boolean`, which is `unsigned char` and not a
    /// C `_Bool`, so it is declared as `u8` — a Rust `bool` here would be an
    /// ABI mismatch that happens to work today.
    fn AXIsProcessTrusted() -> u8;

    /// As above, but shows the "grant access" prompt when the options
    /// dictionary contains `kAXTrustedCheckOptionPrompt: true`.
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;

    /// `kAXTrustedCheckOptionPrompt`, a `CFStringRef` toll-free bridged to
    /// `NSString`.
    static kAXTrustedCheckOptionPrompt: *const NSString;
}

/// `AVAuthorizationStatus`, as declared in `AVCaptureDevice.h`.
///
/// Modelled as an enum rather than a boolean because *not determined* is the
/// only state where asking can still change the answer, and collapsing it into
/// "denied" would make Scrozz open System Settings at a user who has never been
/// asked and whose app is not yet in the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationStatus {
    /// The user has not been asked yet. A prompt will appear.
    NotDetermined,
    /// Withheld by policy — parental controls or an MDM profile. The user
    /// cannot grant it themselves, so pointing them at Settings is misleading.
    Restricted,
    /// The user said no. Only System Settings can change this.
    Denied,
    /// Granted.
    Authorized,
}

impl AuthorizationStatus {
    /// Maps the raw `NSInteger` AVFoundation returns.
    ///
    /// Unknown values are treated as [`Self::Denied`]: a future status Scrozz
    /// does not understand must not be assumed to mean "go ahead".
    #[must_use]
    pub const fn from_raw(raw: isize) -> Self {
        match raw {
            0 => Self::NotDetermined,
            1 => Self::Restricted,
            3 => Self::Authorized,
            _ => Self::Denied,
        }
    }
}

/// Whether a capability is currently granted, without prompting.
#[must_use]
pub fn is_granted(capability: Capability) -> bool {
    match capability {
        Capability::ScreenRecording => CGPreflightScreenCaptureAccess(),
        Capability::Microphone => microphone_status() == AuthorizationStatus::Authorized,
        // SAFETY: a nullary C query with no arguments, no allocation and no
        // thread requirement.
        Capability::Accessibility => (unsafe { AXIsProcessTrusted() }) != 0,
        // SAFETY: read-only process permission query available since macOS 10.15.
        Capability::InputMonitoring => {
            (unsafe { IOHIDCheckAccess(IO_HID_LISTEN_EVENT) }) == IO_HID_ACCESS_GRANTED
        }
    }
}

/// The microphone authorisation state.
///
/// Returns [`AuthorizationStatus::Denied`] if AVFoundation is somehow not
/// loaded, which cannot happen in a normally linked build but is not worth
/// crashing over.
#[must_use]
pub fn microphone_status() -> AuthorizationStatus {
    let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
        tracing::warn!("AVCaptureDevice is not registered; treating the microphone as denied");
        return AuthorizationStatus::Denied;
    };
    let Some(media_type) = audio_media_type() else {
        return AuthorizationStatus::Denied;
    };
    // SAFETY: `+[AVCaptureDevice authorizationStatusForMediaType:]` takes one
    // `AVMediaType` (an `NSString *`) and returns `AVAuthorizationStatus`, which
    // is an `NSInteger`. The receiver is the class object, as required for a
    // class method.
    let raw: isize = unsafe { msg_send![class, authorizationStatusForMediaType: media_type] };
    AuthorizationStatus::from_raw(raw)
}

/// The `AVMediaTypeAudio` constant.
fn audio_media_type() -> Option<&'static NSString> {
    // SAFETY: reading a framework-exported `NSString *` constant. It is
    // initialised before `main` by the dynamic loader and never mutated, and
    // the resulting object is immortal, hence the `'static` lifetime.
    unsafe {
        let pointer = AVMediaTypeAudio;
        if pointer.is_null() {
            None
        } else {
            Some(&*pointer)
        }
    }
}

/// Prompts for a capability, falling back to the relevant Settings pane.
///
/// # Errors
///
/// Returns [`Error::PermissionDenied`] when the OS refused and the user must go
/// to System Settings — the error carries the exact pane to name in the UI — and
/// [`Error::Platform`] if the Settings URL itself could not be opened.
pub fn request(capability: Capability) -> Result<()> {
    match capability {
        Capability::ScreenRecording => request_screen_recording(capability),
        Capability::Microphone => request_microphone(capability),
        Capability::Accessibility => request_accessibility(capability),
        Capability::InputMonitoring => request_input_monitoring(capability),
    }
}

fn request_input_monitoring(capability: Capability) -> Result<()> {
    if is_granted(capability) {
        return Ok(());
    }
    // SAFETY: the documented listen-event request is made only after a user
    // explicitly enables a recording interaction feature.
    if unsafe { IOHIDRequestAccess(IO_HID_LISTEN_EVENT) } != 0 {
        return Ok(());
    }
    open_settings_pane(capability)?;
    Err(crate::permissions::denied(capability))
}

fn request_screen_recording(capability: Capability) -> Result<()> {
    if CGPreflightScreenCaptureAccess() {
        return Ok(());
    }
    // Shows the system prompt the first time and nothing at all afterwards, so
    // its `false` return covers both "the user said no just now" and "the user
    // said no months ago". Either way the remedy is the same pane.
    if CGRequestScreenCaptureAccess() {
        return Ok(());
    }
    open_settings_pane(capability)?;
    Err(crate::permissions::denied(capability))
}

fn request_microphone(capability: Capability) -> Result<()> {
    match microphone_status() {
        AuthorizationStatus::Authorized => Ok(()),
        AuthorizationStatus::NotDetermined => {
            prompt_for_microphone();
            // The prompt is asynchronous: the user is looking at it right now
            // and the answer is not available on this call. Reporting denial is
            // the honest answer for *this* attempt — the caller retries, and by
            // then the status is real.
            Err(crate::permissions::denied(capability))
        }
        AuthorizationStatus::Restricted => Err(Error::Unsupported {
            what: capability_name(capability).to_owned(),
            why: "withheld by a configuration profile or parental controls, \
                  which the user cannot override in System Settings"
                .to_owned(),
        }),
        AuthorizationStatus::Denied => {
            open_settings_pane(capability)?;
            Err(crate::permissions::denied(capability))
        }
    }
}

fn request_accessibility(capability: Capability) -> Result<()> {
    // SAFETY: passing a toll-free-bridged `NSDictionary` where a
    // `CFDictionaryRef` is expected. The dictionary outlives the call, and
    // `AXIsProcessTrustedWithOptions` does not retain it.
    let trusted = {
        let key = accessibility_prompt_key();
        let value = NSNumber::new_bool(true);
        let options: Option<Retained<NSDictionary<NSString, NSNumber>>> =
            key.map(|key| NSDictionary::from_slices(&[key], &[&*value]));
        let pointer = options.as_ref().map_or(std::ptr::null(), |dict| {
            std::ptr::from_ref(&**dict).cast::<c_void>()
        });
        unsafe { AXIsProcessTrustedWithOptions(pointer) != 0 }
    };

    if trusted {
        return Ok(());
    }

    // Unlike the other two, the Accessibility prompt is only ever shown once
    // per app per install, and it is a "Open System Settings" alert rather than
    // a grant dialog — so opening the pane directly is not redundant.
    open_settings_pane(capability)?;
    Err(crate::permissions::denied(capability))
}

/// The `kAXTrustedCheckOptionPrompt` key.
fn accessibility_prompt_key() -> Option<&'static NSString> {
    // SAFETY: reading an immortal `CFStringRef` constant exported by
    // HIServices, reinterpreted through toll-free bridging as `NSString`.
    unsafe {
        let pointer = kAXTrustedCheckOptionPrompt;
        if pointer.is_null() {
            None
        } else {
            Some(&*pointer)
        }
    }
}

/// Opens the System Settings pane that grants a capability.
///
/// # Errors
///
/// Returns [`Error::Platform`] if the URL is malformed or Launch Services
/// refuses to open it.
pub fn open_settings_pane(capability: Capability) -> Result<()> {
    let raw = settings_pane_url(capability);
    let url = NSURL::URLWithString(&NSString::from_str(raw))
        .ok_or_else(|| Error::Platform(format!("could not parse settings URL {raw}")))?;
    if NSWorkspace::sharedWorkspace().openURL(&url) {
        Ok(())
    } else {
        Err(Error::Platform(format!(
            "System Settings refused to open {raw}"
        )))
    }
}

// ---------------------------------------------------------------------------
// The microphone completion block
// ---------------------------------------------------------------------------
//
// `+[AVCaptureDevice requestAccessForMediaType:completionHandler:]` takes an
// Objective-C block and there is no way around that: it is the only API that
// raises the microphone prompt. `block2` would provide one, but it is not a
// declared dependency of this crate, so the block is built by hand.
//
// A block is just a C struct with a function pointer. The one below captures
// nothing, so it can be a *global* block — flagged `BLOCK_IS_GLOBAL`, which
// makes `Block_copy` a no-op and `Block_release` a no-op, and therefore makes
// its lifetime a non-problem. It is leaked exactly once per process so that it
// is genuinely `'static`, because AVFoundation invokes it asynchronously long
// after this call returns.

/// `BLOCK_IS_GLOBAL` — tells libclosure not to copy or free this block.
const BLOCK_IS_GLOBAL: c_int = 1 << 28;
/// `BLOCK_HAS_SIGNATURE` — the descriptor carries a type-encoding string.
const BLOCK_HAS_SIGNATURE: c_int = 1 << 30;

/// Type encoding of `void (^)(BOOL)`: void return, 16 bytes of arguments, the
/// block pointer at offset 0 and the `BOOL` at offset 8.
const BLOCK_SIGNATURE: &CStr = c"v16@?0B8";

#[repr(C)]
struct BlockDescriptor {
    reserved: c_ulong,
    size: c_ulong,
    signature: *const c_char,
}

#[repr(C)]
struct BlockLiteral {
    isa: *const c_void,
    flags: c_int,
    reserved: c_int,
    invoke: extern "C" fn(*mut c_void, Bool),
    descriptor: *const BlockDescriptor,
}

unsafe extern "C" {
    /// The isa every no-capture block points at.
    static _NSConcreteGlobalBlock: [*const c_void; 32];
}

extern "C" fn microphone_access_answered(_block: *mut c_void, granted: Bool) {
    tracing::info!(granted = granted.as_bool(), "microphone access answered");
}

/// The process-wide completion block, built and leaked on first use.
///
/// Stored as a `usize` so the `OnceLock` stays `Sync` without wrapping a raw
/// pointer in a hand-written `unsafe impl`.
fn microphone_completion_block() -> *mut c_void {
    static BLOCK: OnceLock<usize> = OnceLock::new();
    let address = *BLOCK.get_or_init(|| {
        let descriptor: &'static BlockDescriptor = Box::leak(Box::new(BlockDescriptor {
            reserved: 0,
            size: size_of::<BlockLiteral>() as c_ulong,
            signature: BLOCK_SIGNATURE.as_ptr(),
        }));
        let literal: &'static mut BlockLiteral = Box::leak(Box::new(BlockLiteral {
            // Taking the address of an extern static is safe; only reading
            // through it would not be, and nothing here does.
            isa: (&raw const _NSConcreteGlobalBlock).cast::<c_void>(),
            flags: BLOCK_IS_GLOBAL | BLOCK_HAS_SIGNATURE,
            reserved: 0,
            invoke: microphone_access_answered,
            descriptor: std::ptr::from_ref(descriptor),
        }));
        std::ptr::from_mut(literal) as usize
    });
    address as *mut c_void
}

/// Raises the microphone permission prompt.
///
/// Does nothing observable if the status is not [`AuthorizationStatus::NotDetermined`];
/// AVFoundation short-circuits and simply invokes the handler with the existing
/// answer.
///
/// # Panics in a bundle-less process
///
/// See the [module docs](self): this call terminates a process whose
/// `Info.plist` lacks `NSMicrophoneUsageDescription`. It is never reached from
/// a test.
fn prompt_for_microphone() {
    let Some(class) = AnyClass::get(c"AVCaptureDevice") else {
        return;
    };
    let Some(media_type) = audio_media_type() else {
        return;
    };
    let block = microphone_completion_block();
    // SAFETY: the selector's shape is
    // `(AVMediaType, void (^)(BOOL)) -> void`; `media_type` is a live immortal
    // `NSString *`, and `block` is a `'static` global block whose `invoke` has
    // the matching `(void *, BOOL)` signature.
    unsafe {
        let _: () = msg_send![
            class,
            requestAccessForMediaType: media_type,
            completionHandler: block,
        ];
    }
}
