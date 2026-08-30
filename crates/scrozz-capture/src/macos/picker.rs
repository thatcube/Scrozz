//! Apple's privacy-preserving content picker.
//!
//! `SCContentSharingPicker` hands Scrozz an authorised `SCContentFilter`. That
//! filter is the capability: it is consumed immediately on the observer callback
//! thread, never widened through `SCShareableContent`, and never moved through an
//! unsafe `Send` wrapper. Only the resulting thread-safe `CGImage` crosses into
//! Scrozz's worker.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, NSObjectProtocol, ProtocolObject};
use objc2::{AnyThread, DeclaredClass, MainThreadMarker, define_class, msg_send, sel};
use objc2_core_graphics::CGImage;
use objc2_foundation::{NSArray, NSError, NSNumber, NSObject, NSString};
use objc2_screen_capture_kit::{
    SCContentFilter, SCContentSharingPicker, SCContentSharingPickerConfiguration,
    SCContentSharingPickerMode, SCContentSharingPickerObserver, SCShareableContentStyle, SCStream,
};
use scrozz_core::{
    Capture, CaptureRequest, CaptureTarget, CursorMode, DisplayId, Error, Provenance, Result,
    ScaleFactor, WindowId,
};

use super::{image, sck};

const SCROZZ_BUNDLE_ID: &str = "com.thatcube.Scrozz";
const PICKER_CAPTURE_TIMEOUT: Duration = Duration::from_secs(15);
const PICKER_PRESENT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Runtime support for Apple's picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplePickerAvailability {
    /// The picker classes are present.
    Available,
    /// The OS predates the macOS 14 picker API.
    OlderMacOs,
    /// A partial or restricted runtime is missing a required selector.
    Unavailable,
}

/// The least set of picker modes needed by one action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplePickerMode {
    /// Exactly one window.
    Window,
    /// Exactly one display.
    Display,
    /// One window or one display.
    WindowOrDisplay,
}

impl ApplePickerMode {
    fn flags(self) -> SCContentSharingPickerMode {
        match self {
            Self::Window => SCContentSharingPickerMode::SingleWindow,
            Self::Display => SCContentSharingPickerMode::SingleDisplay,
            Self::WindowOrDisplay => {
                SCContentSharingPickerMode::SingleWindow | SCContentSharingPickerMode::SingleDisplay
            }
        }
    }

    fn initial_style(self) -> Option<SCShareableContentStyle> {
        match self {
            Self::Window => Some(SCShareableContentStyle::Window),
            Self::Display => Some(SCShareableContentStyle::Display),
            Self::WindowOrDisplay => None,
        }
    }
}

/// One outcome from a presented Apple picker.
pub enum ApplePickerEvent {
    /// The completed image of the exact content Apple authorised.
    Captured(PickerCapture),
    /// The user closed the picker without selecting content.
    Cancelled,
    /// ScreenCaptureKit could not start the picker.
    Failed(Error),
}

impl fmt::Debug for ApplePickerEvent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Captured(capture) => formatter.debug_tuple("Captured").field(capture).finish(),
            Self::Cancelled => formatter.write_str("Cancelled"),
            Self::Failed(error) => formatter.debug_tuple("Failed").field(error).finish(),
        }
    }
}

/// A picker-authorised image ready for the ordinary capture worker.
pub struct PickerCapture {
    image: Retained<CGImage>,
    scale: ScaleFactor,
    target: CaptureTarget,
    provenance: Provenance,
}

impl fmt::Debug for PickerCapture {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PickerCapture")
            .field("scale", &self.scale)
            .field("target", &self.target)
            .field("provenance", &self.provenance)
            .finish_non_exhaustive()
    }
}

impl PickerCapture {
    /// Copies the captured Core Graphics pixels into Scrozz's owned frame.
    pub fn into_capture(self) -> Result<Capture> {
        Ok(Capture {
            frame: image::to_frame(&self.image, self.scale)?,
            provenance: self.provenance,
            target: self.target,
        })
    }
}

#[derive(Default)]
struct InboxState {
    session: u64,
    awaiting: bool,
    capturing: bool,
    presented_since: Option<Instant>,
    capturing_since: Option<Instant>,
    event: Option<ApplePickerEvent>,
}

#[derive(Default)]
struct PickerInbox {
    state: Mutex<InboxState>,
    /// Whether a window selected in this session keeps its shadow.
    ///
    /// Apple's picker answers on its own callback long after the caller
    /// returned, so the preference cannot be passed down the stack. It is
    /// latched here when the picker is presented, which is also the moment the
    /// ordinary capture path reads it — a change made while the picker is up
    /// applies to the next capture, not the one in flight.
    include_window_shadow: AtomicBool,
}

impl PickerInbox {
    fn begin(&self, include_window_shadow: bool) -> Result<u64> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.awaiting {
            return Err(Error::InvalidRequest(
                "Apple's content picker is already waiting for a selection".to_owned(),
            ));
        }
        self.include_window_shadow
            .store(include_window_shadow, Ordering::Relaxed);
        state.session = state.session.wrapping_add(1);
        state.awaiting = true;
        state.capturing = false;
        state.presented_since = Some(Instant::now());
        state.capturing_since = None;
        state.event = None;
        Ok(state.session)
    }

    fn claim_selection(&self) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.awaiting || state.capturing || state.event.is_some() {
            return None;
        }
        state.capturing = true;
        state.presented_since = None;
        state.capturing_since = Some(Instant::now());
        Some(state.session)
    }

    fn deliver(&self, session: u64, event: ApplePickerEvent) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if session != state.session || !state.awaiting || state.event.is_some() {
            return;
        }
        state.awaiting = false;
        state.capturing = false;
        state.presented_since = None;
        state.capturing_since = None;
        state.event = Some(event);
    }

    fn deliver_current(&self, event: ApplePickerEvent) {
        let session = self
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .session;
        self.deliver(session, event);
    }

    fn take(&self) -> Option<ApplePickerEvent> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(event) = state.event.take() {
            return Some(event);
        }
        if state
            .capturing_since
            .is_some_and(|started| started.elapsed() >= PICKER_CAPTURE_TIMEOUT)
        {
            state.awaiting = false;
            state.capturing = false;
            state.presented_since = None;
            state.capturing_since = None;
            state.session = state.session.wrapping_add(1);
            return Some(ApplePickerEvent::Failed(Error::Platform(
                "Apple's picker capture did not finish within 15 seconds".to_owned(),
            )));
        }
        if state
            .presented_since
            .is_some_and(|started| started.elapsed() >= PICKER_PRESENT_TIMEOUT)
        {
            state.awaiting = false;
            state.presented_since = None;
            state.session = state.session.wrapping_add(1);
            return Some(ApplePickerEvent::Failed(Error::Platform(
                "Apple's content picker did not answer within five minutes".to_owned(),
            )));
        }
        None
    }
}

struct PickerObserverIvars {
    inbox: Arc<PickerInbox>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = PickerObserverIvars]
    struct PickerObserver;

    unsafe impl NSObjectProtocol for PickerObserver {}

    unsafe impl SCContentSharingPickerObserver for PickerObserver {
        #[unsafe(method(contentSharingPicker:didCancelForStream:))]
        unsafe fn content_sharing_picker_did_cancel(
            &self,
            _picker: &SCContentSharingPicker,
            _stream: Option<&SCStream>,
        ) {
            self.ivars()
                .inbox
                .deliver_current(ApplePickerEvent::Cancelled);
        }

        #[unsafe(method(contentSharingPicker:didUpdateWithFilter:forStream:))]
        unsafe fn content_sharing_picker_did_update(
            &self,
            _picker: &SCContentSharingPicker,
            filter: &SCContentFilter,
            _stream: Option<&SCStream>,
        ) {
            self.start_capture(filter);
        }

        #[unsafe(method(contentSharingPickerStartDidFailWithError:))]
        unsafe fn content_sharing_picker_start_did_fail(&self, error: &NSError) {
            self.ivars()
                .inbox
                .deliver_current(ApplePickerEvent::Failed(Error::Platform(format!(
                    "Apple's content picker could not start: {} (code {})",
                    error.localizedDescription(),
                    error.code()
                ))));
        }
    }
);

impl PickerObserver {
    fn new(inbox: Arc<PickerInbox>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(PickerObserverIvars { inbox });
        // SAFETY: standard two-phase NSObject initialisation after the Rust
        // ivars have been installed.
        unsafe { msg_send![super(this), init] }
    }

    fn start_capture(&self, filter: &SCContentFilter) {
        let inbox = &self.ivars().inbox;
        let Some(session) = inbox.claim_selection() else {
            return;
        };
        let include_window_shadow = inbox.include_window_shadow.load(Ordering::Relaxed);
        let (configuration, scale, target, provenance) =
            match prepare_capture(filter, include_window_shadow) {
                Ok(prepared) => prepared,
                Err(error) => {
                    inbox.deliver(session, ApplePickerEvent::Failed(error));
                    return;
                }
            };
        let completion_inbox = Arc::clone(inbox);
        let start = sck::capture_image_async(filter, &configuration, move |result| {
            let event = match result {
                Ok(image) => ApplePickerEvent::Captured(PickerCapture {
                    image,
                    scale,
                    target,
                    provenance,
                }),
                Err(error) => ApplePickerEvent::Failed(error),
            };
            completion_inbox.deliver(session, event);
        });
        if let Err(error) = start {
            inbox.deliver(session, ApplePickerEvent::Failed(error));
        }
    }
}

/// Main-thread handle to Apple's picker singleton.
pub struct AppleContentPicker {
    picker: Retained<SCContentSharingPicker>,
    observer: Retained<PickerObserver>,
    inbox: Arc<PickerInbox>,
}

impl fmt::Debug for AppleContentPicker {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppleContentPicker")
            .finish_non_exhaustive()
    }
}

impl AppleContentPicker {
    /// Checks for the weak-linked macOS 14 classes without presenting UI.
    #[must_use]
    pub fn availability() -> ApplePickerAvailability {
        let Some(picker) = AnyClass::get(c"SCContentSharingPicker") else {
            return ApplePickerAvailability::OlderMacOs;
        };
        let Some(configuration) = AnyClass::get(c"SCContentSharingPickerConfiguration") else {
            return ApplePickerAvailability::OlderMacOs;
        };
        let picker_selectors = [
            sel!(setDefaultConfiguration:),
            sel!(setMaximumStreamCount:),
            sel!(setActive:),
            sel!(addObserver:),
            sel!(removeObserver:),
            sel!(present),
            sel!(presentPickerUsingContentStyle:),
        ];
        let configuration_selectors = [
            sel!(setAllowedPickerModes:),
            sel!(setExcludedBundleIDs:),
            sel!(setAllowsChangingSelectedContent:),
        ];
        if picker.metaclass().responds_to(sel!(sharedPicker))
            && picker_selectors
                .into_iter()
                .all(|selector| picker.responds_to(selector))
            && configuration_selectors
                .into_iter()
                .all(|selector| configuration.responds_to(selector))
        {
            ApplePickerAvailability::Available
        } else {
            ApplePickerAvailability::Unavailable
        }
    }

    /// Attaches one observer to the process-wide picker.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unsupported`] before macOS 14 and [`Error::Platform`]
    /// away from AppKit's main thread.
    pub fn new() -> Result<Self> {
        let availability = Self::availability();
        if availability != ApplePickerAvailability::Available {
            return Err(Error::Unsupported {
                what: "Apple's limited content picker".to_owned(),
                why: match availability {
                    ApplePickerAvailability::OlderMacOs => {
                        "SCContentSharingPicker requires macOS 14 or later"
                    }
                    ApplePickerAvailability::Unavailable => {
                        "the runtime is missing a required SCContentSharingPicker selector"
                    }
                    ApplePickerAvailability::Available => unreachable!(),
                }
                .to_owned(),
            });
        }
        if MainThreadMarker::new().is_none() {
            return Err(Error::Platform(
                "Apple's content picker must be created on the main thread".to_owned(),
            ));
        }

        // The class probes above make these typed weak-linked lookups safe.
        let picker = unsafe { SCContentSharingPicker::sharedPicker() };
        let inbox = Arc::new(PickerInbox::default());
        let observer = PickerObserver::new(Arc::clone(&inbox));
        let protocol: &ProtocolObject<dyn SCContentSharingPickerObserver> =
            ProtocolObject::from_ref(&*observer);
        // SAFETY: `observer` conforms to the generated protocol and is retained
        // by this handle for at least as long as it remains registered.
        unsafe { picker.addObserver(protocol) };

        Ok(Self {
            picker,
            observer,
            inbox,
        })
    }

    /// Presents one least-privilege picker.
    ///
    /// `include_window_shadow` is the user's persisted preference, latched for
    /// this presentation. It only reaches a window selection: a display has no
    /// shadow to drop.
    ///
    /// # Errors
    ///
    /// Returns an error if another picker is already in flight.
    pub fn present(&self, mode: ApplePickerMode, include_window_shadow: bool) -> Result<()> {
        let _session = self.inbox.begin(include_window_shadow)?;

        // SAFETY: both classes were probed in `new`. The configuration is new,
        // confined to this call, and copied by `setDefaultConfiguration:`.
        unsafe {
            let configuration: Retained<SCContentSharingPickerConfiguration> =
                SCContentSharingPickerConfiguration::new();
            configuration.setAllowedPickerModes(mode.flags());
            configuration.setAllowsChangingSelectedContent(false);

            let excluded = NSArray::from_retained_slice(&[NSString::from_str(SCROZZ_BUNDLE_ID)]);
            configuration.setExcludedBundleIDs(&excluded);

            let one = NSNumber::new_usize(1);
            self.picker.setMaximumStreamCount(Some(&one));
            self.picker.setDefaultConfiguration(&configuration);
            self.picker.setActive(true);
            if let Some(style) = mode.initial_style() {
                self.picker.presentPickerUsingContentStyle(style);
            } else {
                self.picker.present();
            }
        }
        Ok(())
    }

    /// Takes one picker outcome, if the observer has delivered it.
    pub fn poll(&self) -> Option<ApplePickerEvent> {
        let event = self.inbox.take();
        if event.is_some() {
            // SAFETY: a plain property update on the singleton. Leaving the
            // picker active would let Control Center initiate unrelated future
            // selections outside Scrozz's explicit action.
            unsafe { self.picker.setActive(false) };
        }
        event
    }
}

impl Drop for AppleContentPicker {
    fn drop(&mut self) {
        let protocol: &ProtocolObject<dyn SCContentSharingPickerObserver> =
            ProtocolObject::from_ref(&*self.observer);
        // SAFETY: the same live observer registered in `new`.
        unsafe {
            self.picker.setActive(false);
            self.picker.removeObserver(protocol);
        }
    }
}

/// Configures one screenshot from the exact callback filter.
///
/// Called synchronously by the observer so the undocumented filter never crosses
/// a thread boundary. The screenshot operation itself is asynchronous.
fn prepare_capture(
    filter: &SCContentFilter,
    include_window_shadow: bool,
) -> Result<(
    Retained<objc2_screen_capture_kit::SCStreamConfiguration>,
    ScaleFactor,
    CaptureTarget,
    Provenance,
)> {
    let style = unsafe { filter.style() };
    let (target, provenance) = match style {
        SCShareableContentStyle::Window => (
            CaptureTarget::Window(selected_window_id(filter)),
            Provenance::Window,
        ),
        SCShareableContentStyle::Display => (
            CaptureTarget::Display(selected_display_id(filter)),
            Provenance::Display,
        ),
        _ => {
            return Err(Error::Unsupported {
                what: "the content selected in Apple's picker".to_owned(),
                why: "Scrozz fallback accepts exactly one window or one display; \
                      application and multi-item filters are not widened or substituted"
                    .to_owned(),
            });
        }
    };

    let scale = picker_scale(filter);
    let request = CaptureRequest {
        target: target.clone(),
        cursor: CursorMode::Hidden,
        include_window_shadow: shadow_for(provenance, include_window_shadow),
    };
    let configuration = super::configure(filter, &request, scale, None)?;
    Ok((configuration, scale, target, provenance))
}

/// Whether this picker selection is captured with a shadow.
///
/// A display has no window shadow to include, so the preference only ever
/// removes one from a window.
const fn shadow_for(provenance: Provenance, preferred: bool) -> bool {
    matches!(provenance, Provenance::Window) && preferred
}

fn picker_scale(filter: &SCContentFilter) -> ScaleFactor {
    let raw = f64::from(unsafe { filter.pointPixelScale() });
    super::display::scale_from_ratio(raw)
}

fn selected_window_id(filter: &SCContentFilter) -> WindowId {
    if filter.respondsToSelector(sel!(includedWindows)) {
        let selected = unsafe { filter.includedWindows() };
        if let Some(window) = selected.iter().next() {
            return WindowId(unsafe { window.windowID() }.to_string());
        }
    }
    WindowId("apple-picker-selection".to_owned())
}

fn selected_display_id(filter: &SCContentFilter) -> DisplayId {
    if filter.respondsToSelector(sel!(includedDisplays)) {
        let selected = unsafe { filter.includedDisplays() };
        if let Some(display) = selected.iter().next() {
            return DisplayId(unsafe { display.displayID() }.to_string());
        }
    }
    DisplayId("apple-picker-selection".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picker_modes_never_admit_applications_or_multiple_items() {
        assert_eq!(
            ApplePickerMode::Window.flags(),
            SCContentSharingPickerMode::SingleWindow
        );
        assert_eq!(
            ApplePickerMode::Display.flags(),
            SCContentSharingPickerMode::SingleDisplay
        );
        let combined = ApplePickerMode::WindowOrDisplay.flags();
        assert!(combined.contains(SCContentSharingPickerMode::SingleWindow));
        assert!(combined.contains(SCContentSharingPickerMode::SingleDisplay));
        assert!(!combined.contains(SCContentSharingPickerMode::SingleApplication));
        assert!(!combined.contains(SCContentSharingPickerMode::MultipleWindows));
        assert!(!combined.contains(SCContentSharingPickerMode::MultipleApplications));
    }

    #[test]
    fn picker_capture_payload_is_send_without_an_unsafe_filter_wrapper() {
        fn assert_send<T: Send>() {}
        assert_send::<PickerCapture>();
    }

    #[test]
    fn a_window_picked_with_shadows_off_is_captured_without_one() {
        // The picker used to decide from provenance alone, so turning the
        // setting off had no effect the moment permissions fell back to Apple's
        // picker — the same window came back shadowed.
        assert!(shadow_for(Provenance::Window, true));
        assert!(!shadow_for(Provenance::Window, false));
        // A display never had one to include either way.
        assert!(!shadow_for(Provenance::Display, true));
        assert!(!shadow_for(Provenance::Display, false));
    }

    #[test]
    fn each_presentation_latches_the_preference_it_was_given() {
        // Apple answers on its own callback, so the value read when the
        // selection lands has to be the one in force when the picker opened.
        let inbox = PickerInbox::default();

        inbox.begin(false).unwrap();
        assert!(!inbox.include_window_shadow.load(Ordering::Relaxed));
        inbox.deliver_current(ApplePickerEvent::Cancelled);
        assert!(matches!(inbox.take(), Some(ApplePickerEvent::Cancelled)));

        inbox.begin(true).unwrap();
        assert!(inbox.include_window_shadow.load(Ordering::Relaxed));
    }

    #[test]
    fn a_late_capture_cannot_satisfy_a_new_picker_session() {
        let inbox = PickerInbox::default();
        let first = inbox.begin(true).unwrap();
        assert_eq!(inbox.claim_selection(), Some(first));
        inbox.state.lock().unwrap().capturing_since = Some(Instant::now() - PICKER_CAPTURE_TIMEOUT);
        assert!(matches!(inbox.take(), Some(ApplePickerEvent::Failed(_))));

        let second = inbox.begin(true).unwrap();
        assert_ne!(first, second);
        inbox.deliver(
            first,
            ApplePickerEvent::Failed(Error::Platform("late".to_owned())),
        );
        assert!(inbox.take().is_none(), "the stale event crossed sessions");

        inbox.deliver(second, ApplePickerEvent::Cancelled);
        assert!(matches!(inbox.take(), Some(ApplePickerEvent::Cancelled)));
    }
}
