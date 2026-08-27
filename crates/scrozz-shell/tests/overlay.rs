//! Overlay geometry, layout and — behind `--ignored` — the real AppKit checks.
//!
//! Everything that runs by default is pure arithmetic and touches no window
//! server. That is deliberate: the coordinate flip and the capture-stack layout
//! are where the bugs actually are, and both are testable without putting a
//! single pixel on screen.
//!
//! The one test that *does* talk to AppKit is `#[ignore]`d, because it needs
//! the main thread (see [`appkit`]). It never orders a window front and closes
//! everything it makes.

use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
use scrozz_shell::Capability;
use scrozz_shell::overlay::{
    AppKitRect, OverlayBehavior, OverlayLevel, StackLayout, anchor_bottom_left, appkit_to_logical,
    logical_to_appkit,
};
use scrozz_shell::permissions::settings_pane_url;

/// Height of the primary display in every fixture below.
///
/// A deliberately un-round number: a flip that accidentally uses the *rect's*
/// height instead of the *screen's* still passes when they happen to match.
const SCREEN_HEIGHT: f64 = 1169.0;

fn rect(x: f64, y: f64, width: f64, height: f64) -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(width, height))
}

// ---------------------------------------------------------------------------
// Coordinate flip
// ---------------------------------------------------------------------------

#[test]
fn appkit_origin_is_the_bottom_left_of_the_screen() {
    // A 100x50 rect sitting exactly on the AppKit origin is, in Scrozz's
    // top-left space, at the *bottom* of the screen: its top edge is 50 points
    // above the bottom.
    let logical = appkit_to_logical(AppKitRect::new(0.0, 0.0, 100.0, 50.0), SCREEN_HEIGHT);
    assert_eq!(logical.origin.x, 0.0);
    assert_eq!(logical.origin.y, SCREEN_HEIGHT - 50.0);
    assert_eq!(logical.size.width, 100.0);
    assert_eq!(logical.size.height, 50.0);
}

#[test]
fn scrozz_origin_is_the_top_left_of_the_screen() {
    // The mirror image: a rect at Scrozz's origin is at the *top*, so its
    // AppKit y (the bottom edge) is the screen height minus its own height.
    let appkit = logical_to_appkit(rect(0.0, 0.0, 100.0, 50.0), SCREEN_HEIGHT);
    assert_eq!(appkit.x, 0.0);
    assert_eq!(appkit.y, SCREEN_HEIGHT - 50.0);
}

#[test]
fn the_flip_is_an_involution() {
    // Round-tripping must be exact, not approximately right: `set_frame` flips
    // on the way in and `diagnostics` flips on the way out, and a half-point
    // drift per cycle is precisely the sort of thing that shows up as an
    // overlay slowly walking off the bottom of the screen.
    for original in [
        rect(0.0, 0.0, 1.0, 1.0),
        rect(20.0, 999.0, 232.0, 150.0),
        rect(-1512.0, -300.0, 400.0, 900.0),
        rect(37.5, 1168.25, 0.5, 0.75),
    ] {
        let there = logical_to_appkit(original, SCREEN_HEIGHT);
        let back = appkit_to_logical(there, SCREEN_HEIGHT);
        assert_eq!(back, original, "round trip changed {original:?}");
    }
}

#[test]
fn flipping_preserves_size() {
    let appkit = logical_to_appkit(rect(10.0, 20.0, 232.0, 150.0), SCREEN_HEIGHT);
    assert_eq!(appkit.width, 232.0);
    assert_eq!(appkit.height, 150.0);
}

#[test]
fn secondary_displays_above_the_primary_get_negative_logical_y() {
    // AppKit puts a display stacked above the primary at a positive y beyond
    // the primary's height. Scrozz's origin is the primary's *top*, so that
    // display is at a negative y — and code that clamps coordinates to zero
    // "for safety" silently drops every overlay on it.
    let above = AppKitRect::new(0.0, SCREEN_HEIGHT, 1920.0, 1080.0);
    let logical = appkit_to_logical(above, SCREEN_HEIGHT);
    assert_eq!(logical.origin.y, -1080.0);
}

// ---------------------------------------------------------------------------
// Capture stack layout (D28)
// ---------------------------------------------------------------------------

/// The work area of a 16-inch MacBook Pro with the Dock at the bottom.
fn work_area() -> LogicalRect {
    rect(0.0, 38.0, 1512.0, 1069.0)
}

#[test]
fn slot_zero_sits_at_the_bottom_left_of_the_work_area() {
    let layout = StackLayout::default();
    let slot = layout.slot_frame(work_area(), 0);
    assert_eq!(slot.origin.x, work_area().origin.x + layout.left_margin);
    let bottom = slot.origin.y + slot.size.height;
    assert_eq!(
        bottom,
        work_area().origin.y + work_area().size.height - layout.margin
    );
}

#[test]
fn the_stack_grows_upward() {
    let layout = StackLayout::default();
    let area = work_area();
    let mut previous = layout.slot_frame(area, 0);
    for slot in 1..layout.max_slots {
        let current = layout.slot_frame(area, slot);
        assert!(
            current.origin.y < previous.origin.y,
            "slot {slot} is not above slot {}",
            slot - 1
        );
        assert_eq!(
            current.origin.x, previous.origin.x,
            "slots must stay aligned"
        );
        previous = current;
    }
}

#[test]
fn adjacent_slots_are_separated_by_exactly_one_gap() {
    let layout = StackLayout::default();
    assert_eq!(layout.gap, 8.0);
    assert_eq!(layout.margin, 8.0);
    assert_eq!(layout.left_margin, 40.0);
    assert_eq!(layout.card, LogicalSize::new(210.0, 150.0));
    let area = work_area();
    let lower = layout.slot_frame(area, 0);
    let upper = layout.slot_frame(area, 1);
    // The bottom of the upper card to the top of the lower card.
    let separation = lower.origin.y - (upper.origin.y + upper.size.height);
    assert!(
        (separation - layout.gap).abs() < f64::EPSILON,
        "expected a {} point gap, got {separation}",
        layout.gap
    );
}

#[test]
fn a_cards_position_never_moves_upward_as_the_stack_fills() {
    // The D28 invariant, stated as the property it actually protects: a card
    // that lands in slot 0 stays exactly where it is no matter how many arrive
    // after it. If slot geometry depended on occupancy, an incoming capture
    // would shove the card under the user's cursor out from under them.
    let layout = StackLayout::default();
    let area = work_area();
    let first = layout.slot_frame(area, 0);
    for _ in 0..20 {
        assert_eq!(layout.slot_frame(area, 0), first);
    }
}

#[test]
fn capacity_is_derived_from_work_area_height() {
    let layout = StackLayout::default();
    // 1069 points of work area, 16 of margin either side, 158 per card:
    // (1037 + 8) / 158 = 6.6 -> 6, which is also the cap.
    assert_eq!(layout.capacity(work_area()), 6);
}

#[test]
fn capacity_clamps_on_a_short_display() {
    let layout = StackLayout::default();
    // An old 1024x768 display with a Dock leaves room for four cards, not six.
    let short = rect(0.0, 25.0, 1024.0, 660.0);
    let capacity = layout.capacity(short);
    assert!(
        (1..=layout.max_slots).contains(&capacity),
        "capacity {capacity} outside 1..={}",
        layout.max_slots
    );
    assert!(
        capacity < layout.max_slots,
        "a 660pt work area cannot hold six 150pt cards"
    );
}

#[test]
fn capacity_is_never_zero() {
    let layout = StackLayout::default();
    // Absurd, but reachable: a projector mode, or a display being reconfigured
    // while a capture lands. One crowded card beats no visible result at all.
    for height in [0.0, 1.0, 40.0, 149.0] {
        let tiny = rect(0.0, 0.0, 1024.0, height);
        assert_eq!(layout.capacity(tiny), 1, "height {height} gave zero slots");
    }
}

#[test]
fn every_slot_within_capacity_fits_inside_the_work_area() {
    let layout = StackLayout::default();
    let area = work_area();
    for slot in 0..layout.capacity(area) {
        let frame = layout.slot_frame(area, slot);
        assert!(
            frame.origin.y >= area.origin.y,
            "slot {slot} escapes the top of the work area"
        );
        assert!(
            frame.origin.y + frame.size.height <= area.origin.y + area.size.height,
            "slot {slot} escapes the bottom of the work area"
        );
        assert!(
            frame.origin.x + frame.size.width <= area.origin.x + area.size.width,
            "slot {slot} escapes the right of the work area"
        );
    }
}

#[test]
fn the_stack_respects_the_dock() {
    // The whole point of using `work_area` rather than `bounds`. On this
    // fixture the Dock occupies the bottom 43 points; nothing may land there.
    let bounds = rect(0.0, 0.0, 1512.0, 1150.0);
    let area = work_area();
    let dock_top = area.origin.y + area.size.height;
    assert!(dock_top < bounds.origin.y + bounds.size.height);

    let layout = StackLayout::default();
    let slot = layout.slot_frame(area, 0);
    assert!(
        slot.origin.y + slot.size.height <= dock_top,
        "slot 0 overlaps the Dock"
    );
}

#[test]
fn the_stack_can_use_a_larger_left_inset_than_bottom_inset() {
    let layout = StackLayout::default();
    let area = work_area();
    let uniformly_anchored = anchor_bottom_left(area, layout.card, layout.margin);
    let slot = layout.slot_frame(area, 0);
    assert_eq!(uniformly_anchored.origin.y, slot.origin.y);
    assert_eq!(slot.origin.x - area.origin.x, 40.0);
    assert_eq!(uniformly_anchored.origin.x - area.origin.x, 8.0);
}

#[test]
fn anchoring_to_bounds_instead_of_work_area_lands_under_the_dock() {
    // Not testing Scrozz so much as pinning the reason the API insists on the
    // work area. If this ever stops being true the doc comments are wrong.
    let bounds = rect(0.0, 0.0, 1512.0, 1150.0);
    let area = work_area();
    let layout = StackLayout::default();
    let wrong = anchor_bottom_left(bounds, layout.card, layout.margin);
    let right = layout.slot_frame(area, 0);
    assert!(
        wrong.origin.y > right.origin.y,
        "anchoring to bounds should sit lower than anchoring to the work area"
    );
}

// ---------------------------------------------------------------------------
// Behaviour defaults (D27)
// ---------------------------------------------------------------------------

#[test]
fn the_default_behaviour_is_a_normal_window() {
    // D27: always-on-top is an explicit opt-in, never the default, because a
    // debug or spike window that cannot be moved out of the way is a genuine
    // hazard to whoever is running it.
    let behavior = OverlayBehavior::default();
    assert_eq!(behavior.level, OverlayLevel::Normal);
    assert!(!behavior.click_through);
    assert!(!behavior.join_all_spaces);
}

#[test]
fn a_capture_card_floats_without_taking_focus() {
    let card = OverlayBehavior::capture_card();
    assert_eq!(card.level, OverlayLevel::Floating);
    assert!(
        !card.accepts_key,
        "clicking a card must not pull keyboard focus out of the user's editor"
    );
    assert!(card.join_all_spaces, "a card must survive a Space switch");
    assert!(card.over_fullscreen);
    assert!(
        !card.movable,
        "D27: transient surfaces are not user-movable"
    );
    assert!(
        !card.hides_on_deactivate,
        "the card outlives Scrozz losing focus"
    );
}

#[test]
fn a_hidden_surface_cannot_intercept_pointer_input() {
    let hidden = OverlayBehavior::hidden_surface();
    assert!(hidden.click_through);
    assert!(
        !hidden.accepts_key,
        "an invisible overlay must relinquish both pointer and keyboard input"
    );
}

#[test]
fn the_selection_overlay_sits_above_the_menu_bar() {
    let overlay = OverlayBehavior::selection_overlay();
    assert_eq!(overlay.level, OverlayLevel::Shielding);
    assert!(
        overlay.accepts_key,
        "the selection overlay reads Escape and arrow keys"
    );
    assert!(!overlay.click_through, "it is the thing being dragged on");
}

#[test]
fn overlay_levels_are_ordered_the_way_the_window_server_stacks_them() {
    assert!(OverlayLevel::Normal < OverlayLevel::Floating);
    assert!(OverlayLevel::Floating < OverlayLevel::Status);
    assert!(OverlayLevel::Status < OverlayLevel::AboveMenuBar);
    assert!(OverlayLevel::AboveMenuBar < OverlayLevel::Shielding);
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

#[test]
fn each_capability_has_its_own_settings_pane() {
    // The URLs are what the user sees when the OS declines to prompt, so a
    // wrong one drops them on a page that does not contain the switch they
    // were told to flip.
    assert!(settings_pane_url(Capability::ScreenRecording).ends_with("Privacy_ScreenCapture"));
    assert!(settings_pane_url(Capability::Microphone).ends_with("Privacy_Microphone"));
    assert!(settings_pane_url(Capability::Accessibility).ends_with("Privacy_Accessibility"));
}

#[test]
fn settings_urls_use_the_system_preferences_scheme() {
    for capability in [
        Capability::ScreenRecording,
        Capability::Microphone,
        Capability::Accessibility,
    ] {
        let url = settings_pane_url(capability);
        assert!(
            url.starts_with("x-apple.systempreferences:"),
            "{url} is not a System Settings URL"
        );
    }
}

#[test]
fn settings_urls_are_distinct() {
    let screen = settings_pane_url(Capability::ScreenRecording);
    let microphone = settings_pane_url(Capability::Microphone);
    let accessibility = settings_pane_url(Capability::Accessibility);
    assert_ne!(screen, microphone);
    assert_ne!(microphone, accessibility);
    assert_ne!(screen, accessibility);
}

// ---------------------------------------------------------------------------
// Off-main-thread safety
// ---------------------------------------------------------------------------
//
// The AppKit surface itself is verified by doctests, not here. That is not a
// workaround: libtest runs every test on a spawned thread — including under
// `--test-threads=1`, which only serialises them — while `NSScreen`, `NSWindow`
// and `NSApplication` may only be touched from the main thread. A doctest is
// compiled into its own `fn main`, so it runs where AppKit requires. See the
// examples on `macos::overlay::make_nonactivating_panel` and
// `macos::display::displays`; run them with `cargo test -p scrozz-shell --doc`.
//
// What *is* worth testing in this harness is the thing the harness accidentally
// creates: a caller on the wrong thread. Scrozz will have background threads —
// the capture pipeline, the file writer — and a stray overlay call from one of
// them must produce an error, not a crash or silent corruption. Every test
// below is running off-main by construction, which makes this the one place
// that path can be exercised honestly.

#[cfg(target_os = "macos")]
mod off_main {
    use objc2::MainThreadMarker;
    use scrozz_core::Error;
    use scrozz_shell::macos::display;

    #[test]
    fn the_harness_really_is_off_the_main_thread() {
        // Guards the four tests below: if libtest ever starts running tests
        // inline on the main thread, they would pass for the wrong reason and
        // quietly stop testing anything.
        assert!(
            MainThreadMarker::new().is_none(),
            "libtest now runs on the main thread; these tests need rewriting \
             and the AppKit doctests could become ordinary tests"
        );
    }

    #[test]
    fn enumerating_displays_off_main_is_refused_rather_than_undefined() {
        match display::displays() {
            Err(Error::Platform(message)) => {
                assert!(
                    message.contains("main thread"),
                    "the error must say why: {message}"
                );
            }
            Err(other) => panic!("expected a platform error, got {other}"),
            Ok(_) => panic!("AppKit was touched from a spawned thread"),
        }
    }

    #[test]
    fn the_primary_display_is_refused_off_main() {
        assert!(matches!(
            display::primary_display(),
            Err(Error::Platform(_))
        ));
    }

    #[test]
    fn the_active_display_is_refused_off_main() {
        assert!(matches!(display::active_display(), Err(Error::Platform(_))));
    }

    #[test]
    fn the_pointer_location_is_refused_off_main() {
        assert!(matches!(
            display::pointer_location(),
            Err(Error::Platform(_))
        ));
    }

    #[test]
    fn a_point_lookup_is_refused_off_main() {
        use scrozz_core::LogicalPoint;
        assert!(matches!(
            display::display_at(LogicalPoint::new(0.0, 0.0)),
            Err(Error::Platform(_))
        ));
    }
}

// ---------------------------------------------------------------------------
// Permission queries
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
mod permission_queries {
    use scrozz_shell::macos::permissions;
    use scrozz_shell::{Capability, Permissions, SystemPermissions};

    #[test]
    fn querying_a_capability_never_prompts_and_never_panics() {
        // `is_granted` is called on every capture, so it has to be a pure
        // query: `CGPreflightScreenCaptureAccess` and `AXIsProcessTrusted` both
        // are, and `authorizationStatusForMediaType:` is too. Nothing here
        // shows UI, and nothing here needs the main thread.
        //
        // The values themselves are whatever this machine happens to be set to
        // — asserting on them would make the suite depend on the tester's
        // System Settings. What matters is that the calls return at all.
        let permissions = SystemPermissions;
        for capability in [
            Capability::ScreenRecording,
            Capability::Microphone,
            Capability::Accessibility,
        ] {
            let granted = permissions.is_granted(capability);
            println!("{capability:?}: {granted}");
        }
    }

    #[test]
    fn microphone_status_is_one_of_the_four_documented_states() {
        // Reading the status is safe without `NSMicrophoneUsageDescription`;
        // *requesting* it in a process without that key terminates the process,
        // which is why no test calls `request`.
        let status = permissions::microphone_status();
        println!("microphone: {status:?}");
    }

    #[test]
    fn unknown_authorization_values_are_treated_as_denied() {
        use scrozz_shell::macos::permissions::AuthorizationStatus;
        assert_eq!(
            AuthorizationStatus::from_raw(0),
            AuthorizationStatus::NotDetermined
        );
        assert_eq!(
            AuthorizationStatus::from_raw(1),
            AuthorizationStatus::Restricted
        );
        assert_eq!(
            AuthorizationStatus::from_raw(2),
            AuthorizationStatus::Denied
        );
        assert_eq!(
            AuthorizationStatus::from_raw(3),
            AuthorizationStatus::Authorized
        );
        // A status added in a future macOS must not be read as permission to
        // start recording.
        for unknown in [-1, 4, 99] {
            assert_eq!(
                AuthorizationStatus::from_raw(unknown),
                AuthorizationStatus::Denied,
                "unknown status {unknown} must not mean authorised"
            );
        }
    }
}
