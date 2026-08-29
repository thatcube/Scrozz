//! Native contracts that must remain true without changing the runner's TCC
//! database or presenting UI.

#![cfg(target_os = "macos")]

use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use scrozz_capture::{AppleContentPicker, ApplePickerAvailability, ScreenCaptureKitBackend};

#[test]
fn constructing_capture_services_never_requests_screen_access() {
    let before = CGPreflightScreenCaptureAccess();
    let _backend = ScreenCaptureKitBackend::new();
    let _availability = AppleContentPicker::availability();
    let after = CGPreflightScreenCaptureAccess();
    assert_eq!(
        before, after,
        "constructing the app's capture services must be a side-effect-free preflight"
    );
}

#[test]
fn picker_availability_is_weak_linked_instead_of_assumed() {
    match AppleContentPicker::availability() {
        ApplePickerAvailability::Available => {
            assert!(
                objc2::runtime::AnyClass::get(c"SCContentSharingPicker").is_some(),
                "availability requires the real runtime class"
            );
        }
        ApplePickerAvailability::OlderMacOs => {
            assert!(
                objc2::runtime::AnyClass::get(c"SCContentSharingPicker").is_none(),
                "an absent picker is reported rather than called"
            );
        }
        ApplePickerAvailability::Unavailable => {
            assert!(
                objc2::runtime::AnyClass::get(c"SCContentSharingPicker").is_some(),
                "partial availability requires the picker class to exist"
            );
        }
    }
}

#[test]
fn backend_gate_cannot_regress_into_raising_the_system_prompt() {
    let source = include_str!("../src/macos/sck.rs");
    assert!(
        source.contains("CGPreflightScreenCaptureAccess"),
        "the backend must still defend against revocation"
    );
    assert!(
        !source.contains("CGRequestScreenCaptureAccess"),
        "only the app's explained preflight may invoke the system request"
    );
}

#[test]
fn picker_capture_consumes_apples_filter_without_enumerating_a_replacement() {
    let source = include_str!("../src/macos/picker.rs");
    assert!(
        source.contains("sck::capture_image_async(filter, &configuration"),
        "the exact callback filter must reach SCScreenshotManager immediately"
    );
    assert!(
        !source.contains("shareable_content()"),
        "a picker token must never be replaced by app-side enumeration"
    );
    assert!(
        !source.contains("unsafe impl Send for PickerSelection"),
        "SCContentFilter is not documented as transferable between threads"
    );
}

#[test]
fn picker_capture_keeps_native_window_geometry_and_colour_contracts() {
    let picker = include_str!("../src/macos/picker.rs");
    let backend = include_str!("../src/macos/mod.rs");
    assert!(picker.contains("super::configure(filter"));
    assert!(picker.contains("image::to_frame(&self.image"));
    assert!(backend.contains("setIgnoreShadowsSingleWindow(!request.include_window_shadow)"));
    assert!(backend.contains("setIgnoreGlobalClipSingleWindow(true)"));
    assert!(backend.contains("setShouldBeOpaque(false)"));
    assert!(
        !backend.contains("setColorSpaceName"),
        "leaving colorSpaceName unset preserves the source profile"
    );
}
