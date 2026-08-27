//! What the Windows overlay must do, tested from outside the crate.
//!
//! # Why these are integration tests
//!
//! The unit tests inside `win32.rs` check that each rule is internally
//! consistent. These check something different and, for this project, more
//! important: that the rules **compose into the overlay the product needs**,
//! reached only through the public API — the same surface
//! `crates/scrozz-shell/src/windows/overlay.rs` reaches for when it talks to
//! Windows.
//!
//! That matters because the native adapter cannot be tested at all on this
//! machine. Nothing here calls `SetWindowLongPtrW`. What it can establish is
//! that every value the adapter will pass to `SetWindowLongPtrW` is the right
//! one, so that when the FFI is finally executed the only thing left to be
//! wrong is the FFI.
//!
//! # What these do not establish
//!
//! That a window with these bits behaves as intended on Windows. `WS_EX_LAYERED`
//! without `SetLayeredWindowAttributes` is expected to render through DWM —
//! winit itself relies on exactly that — but expected is not observed. See the
//! bottom of the file.

use scrozz_core::{LogicalRect, Point, ScaleFactor, Size};
use scrozz_shell::overlay::{OverlayBehavior, OverlayLevel};
use scrozz_shell::win32::{
    ApartmentEntry, DeviceRect, HR_ACCESS_DENIED, HR_CO_E_NOTINITIALIZED, HR_E_HANDLE,
    HR_E_OUTOFMEMORY, HR_INVALID_WINDOW_HANDLE, HR_RPC_E_CHANGED_MODE, WS_EX_APPWINDOW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_EX_TRANSPARENT, ZOrder, classify_apartment_entry, classify_hresult,
    device_from_logical, enforced_ex_style_spec, ex_style_spec, logical_from_device,
    pointer_in_window, scale_from_dpi, work_area_logical, z_order,
};

/// The ex-style word winit hands over for a transparent, always-on-top,
/// undecorated, off-taskbar window — the viewport `scrozz-ui` asks for.
///
/// Notably it contains neither `WS_EX_NOACTIVATE` nor `WS_EX_TOOLWINDOW`:
/// winit has no concept of either, which is the entire problem.
const WINIT_BASELINE: u32 = WS_EX_TOPMOST;

// ---------------------------------------------------------------------------
// The capture card: the one window D27 is about
// ---------------------------------------------------------------------------

#[test]
fn a_capture_card_never_takes_the_keyboard() {
    // D27: a card that appears while the user is typing must not eat the next
    // keystroke. On Windows that is `WS_EX_NOACTIVATE`, and nothing else.
    let spec = ex_style_spec(&OverlayBehavior::capture_card());
    let style = spec.apply(WINIT_BASELINE);
    assert_ne!(style & WS_EX_NOACTIVATE, 0, "0x{style:08X}");
}

#[test]
fn a_capture_card_stays_out_of_the_taskbar_and_the_alt_tab_list() {
    let spec = ex_style_spec(&OverlayBehavior::capture_card());
    let style = spec.apply(WINIT_BASELINE);
    assert_ne!(style & WS_EX_TOOLWINDOW, 0, "tool windows are not alt-tabbed");
    assert_eq!(style & WS_EX_APPWINDOW, 0, "and must not be forced back in");
}

#[test]
fn a_capture_card_floats_above_ordinary_windows() {
    let behavior = OverlayBehavior::capture_card();
    assert_eq!(z_order(behavior.level), ZOrder::Topmost);
    assert_ne!(ex_style_spec(&behavior).apply(0) & WS_EX_TOPMOST, 0);
}

#[test]
fn a_translucent_card_is_layered_so_dwm_composites_its_corners() {
    // The card is a rounded, shadowed panel drawn with per-pixel alpha. That
    // needs `WS_EX_LAYERED`; what it must *not* get is
    // `SetLayeredWindowAttributes(LWA_ALPHA)`, which would replace the
    // per-pixel alpha with one uniform value and square the corners off.
    let behavior = OverlayBehavior::capture_card();
    assert!(!behavior.opaque, "the card is not opaque");
    assert_ne!(ex_style_spec(&behavior).apply(0) & WS_EX_LAYERED, 0);
}

#[test]
fn the_whole_capture_card_style_is_reached_in_one_step() {
    // Applied to winit's word, not to zero: the adapter never writes a style
    // from scratch, it corrects the one winit produced.
    let style = ex_style_spec(&OverlayBehavior::capture_card()).apply(WINIT_BASELINE);
    let expected = WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW | WS_EX_LAYERED;
    assert_eq!(style, expected, "got 0x{style:08X}, want 0x{expected:08X}");
}

// ---------------------------------------------------------------------------
// Surviving winit
// ---------------------------------------------------------------------------

#[test]
fn re_applying_a_style_changes_nothing() {
    // The subclass re-asserts the spec on every `WM_STYLECHANGING`, so it runs
    // constantly. If it were not idempotent the window would flicker between
    // two styles forever.
    let spec = ex_style_spec(&OverlayBehavior::capture_card());
    let once = spec.apply(WINIT_BASELINE);
    assert_eq!(spec.apply(once), once);
    assert!(spec.satisfied_by(once));
}

#[test]
fn the_enforced_subset_is_only_what_winit_would_destroy() {
    // egui asks for mouse passthrough every frame, eframe forwards it to
    // `set_cursor_hittest`, and winit answers by writing the *entire* ex-style
    // word from its own flags — erasing NOACTIVATE and TOOLWINDOW, which it
    // does not model. Those two are what the hook must restore.
    //
    // Restoring more would be worse than restoring less: LAYERED and
    // TRANSPARENT are exactly the bits winit is legitimately toggling, and
    // fighting it over them would break passthrough entirely.
    let enforced = enforced_ex_style_spec(&OverlayBehavior::capture_card());
    assert_eq!(enforced.required, WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW);
    assert_eq!(enforced.forbidden, WS_EX_APPWINDOW);
    assert_eq!(enforced.required & (WS_EX_LAYERED | WS_EX_TRANSPARENT), 0);
}

#[test]
fn a_clobbered_style_is_repaired_to_exactly_what_it_was() {
    let behavior = OverlayBehavior::capture_card();
    let good = ex_style_spec(&behavior).apply(WINIT_BASELINE);

    // What winit writes when it toggles passthrough on: its own flags, plus
    // TRANSPARENT|LAYERED, and nothing of ours.
    let clobbered = WINIT_BASELINE | WS_EX_TRANSPARENT | WS_EX_LAYERED;
    assert_eq!(clobbered & WS_EX_NOACTIVATE, 0, "the bug being repaired");

    let repaired = enforced_ex_style_spec(&behavior).apply(clobbered);
    assert_ne!(repaired & WS_EX_NOACTIVATE, 0, "focus protection restored");
    assert_ne!(repaired & WS_EX_TOOLWINDOW, 0);
    assert_ne!(
        repaired & WS_EX_TRANSPARENT,
        0,
        "and winit's own passthrough left alone"
    );
    assert_eq!(repaired & !WS_EX_TRANSPARENT, good, "nothing else moved");
}

#[test]
fn the_selection_overlay_is_allowed_to_take_focus() {
    // It reads Escape to cancel, so `WS_EX_NOACTIVATE` would make it
    // uncancellable. The distinction is the reason the spec is derived from
    // behaviour rather than hard-coded.
    let behavior = OverlayBehavior::selection_overlay();
    assert!(behavior.accepts_key);
    assert_eq!(ex_style_spec(&behavior).apply(WINIT_BASELINE) & WS_EX_NOACTIVATE, 0);
    assert_eq!(
        enforced_ex_style_spec(&behavior).required & WS_EX_NOACTIVATE,
        0,
        "and the hook must not put it back"
    );
}

// ---------------------------------------------------------------------------
// Click-through: empty regions pass clicks, cards do not
// ---------------------------------------------------------------------------

#[test]
fn passthrough_is_a_single_bit_that_toggles_cleanly() {
    let mut behavior = OverlayBehavior::capture_card();
    behavior.click_through = true;
    let through = ex_style_spec(&behavior).apply(WINIT_BASELINE);
    behavior.click_through = false;
    let solid = ex_style_spec(&behavior).apply(WINIT_BASELINE);

    assert_ne!(through & WS_EX_TRANSPARENT, 0, "empty regions pass clicks");
    assert_eq!(solid & WS_EX_TRANSPARENT, 0, "cards receive them");
    assert_eq!(
        through ^ solid,
        WS_EX_TRANSPARENT,
        "and nothing else changes with it"
    );
}

#[test]
fn the_pointer_is_reported_in_the_window_s_own_logical_points() {
    // egui compares the probe's answer against card rectangles it laid out in
    // logical, window-local points. Any other space silently mis-hits.
    let window = DeviceRect::new(100, 200, 356, 456); // 256x256 device
    let at = pointer_in_window((228, 328), window, scale_from_dpi(192)).expect("inside");
    assert_eq!(at, (64.0, 64.0), "128 device px at 2.0 scale is 64 points");
}

#[test]
fn a_pointer_outside_the_window_is_reported_as_outside() {
    let window = DeviceRect::new(0, 0, 100, 100);
    let one = ScaleFactor::new(1.0);
    assert!(pointer_in_window((-1, 50), window, one).is_none());
    assert!(pointer_in_window((50, -1), window, one).is_none());
    // `right`/`bottom` are exclusive, as in a Win32 RECT.
    assert!(pointer_in_window((100, 50), window, one).is_none());
    assert!(pointer_in_window((99, 99), window, one).is_some());
}

#[test]
fn a_window_on_a_monitor_left_of_the_primary_still_hit_tests() {
    // The virtual desktop has negative coordinates when a monitor is placed to
    // the left. Clamping them to zero \u2014 a very easy mistake \u2014 makes the whole
    // overlay unclickable on that monitor.
    let window = DeviceRect::new(-1920, -100, -1420, 400);
    let at = pointer_in_window((-1900, -80), window, ScaleFactor::new(1.0)).expect("inside");
    assert_eq!(at, (20.0, 20.0));
}

// ---------------------------------------------------------------------------
// Work-area geometry
// ---------------------------------------------------------------------------

#[test]
fn the_work_area_excludes_the_taskbar_and_is_measured_in_points() {
    // A 3840x2160 monitor at 200% with a 96-device-pixel taskbar at the bottom.
    let work = work_area_logical(DeviceRect::new(0, 0, 3840, 2064), 192);
    assert_eq!(work.origin, Point::new(0.0, 0.0));
    assert_eq!(work.size, Size::new(1920.0, 1032.0));
}

#[test]
fn a_secondary_monitor_keeps_its_negative_origin_through_the_conversion() {
    // Anchoring a card to the bottom-left of a work area whose origin was
    // clamped to zero puts the card on the wrong monitor entirely.
    let work = work_area_logical(DeviceRect::new(-2560, -400, 0, 1040), 96);
    assert_eq!(work.origin, Point::new(-2560.0, -400.0));
    assert_eq!(work.size.width, 2560.0);
    assert_eq!(work.size.height, 1440.0);
}

#[test]
fn placing_a_card_round_trips_through_device_space_exactly() {
    // `set_frame` takes logical points from the shared layout code and must
    // hand Windows device pixels. A drift of one pixel per placement would
    // walk the card across the screen over a session.
    let scale = scale_from_dpi(144); // 150%
    let wanted = LogicalRect::new(Point::new(-1200.0, 300.0), Size::new(360.0, 200.0));
    let device = device_from_logical(wanted, scale);
    assert_eq!(device, DeviceRect::new(-1800, 450, -1260, 750));
    assert_eq!(logical_from_device(device, scale), wanted);
}

#[test]
fn every_dpi_windows_actually_ships_maps_to_the_scale_users_see() {
    for (dpi, scale) in [(96, 1.0), (120, 1.25), (144, 1.5), (192, 2.0), (240, 2.5)] {
        assert_eq!(
            scale_from_dpi(dpi).get(),
            scale,
            "{dpi} dpi should be {scale}x"
        );
    }
}

#[test]
fn mixed_dpi_monitors_are_converted_independently() {
    // The reason `dpi_for_monitor` is called per monitor rather than once per
    // process: the same device rectangle means different points on each.
    let rect = DeviceRect::new(0, 0, 1000, 1000);
    let hidpi = logical_from_device(rect, scale_from_dpi(192));
    let lodpi = logical_from_device(rect, scale_from_dpi(96));
    assert_eq!(hidpi.size.width, 500.0);
    assert_eq!(lodpi.size.width, 1000.0);
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

#[test]
fn a_dead_window_is_reported_as_gone_not_as_a_platform_fault() {
    // The overlay races the window closing by design. `TargetGone` is an
    // ordinary outcome the caller ignores; `Platform` would be logged as a bug
    // every time a card was dismissed.
    for hr in [HR_INVALID_WINDOW_HANDLE, HR_E_HANDLE] {
        let err = classify_hresult(hr, "moving the card");
        assert!(
            matches!(err, scrozz_core::Error::TargetGone(_)),
            "0x{hr:08X} should be TargetGone, got {err:?}"
        );
    }
}

#[test]
fn an_access_denial_names_the_capability_and_the_remedy() {
    let err = classify_hresult(HR_ACCESS_DENIED, "adopting the overlay window");
    match err {
        scrozz_core::Error::PermissionDenied { capability, remedy } => {
            assert!(!capability.is_empty());
            assert!(!remedy.is_empty(), "D15: say what to do about it");
        }
        other => panic!("E_ACCESSDENIED should be PermissionDenied, got {other:?}"),
    }
}

#[test]
fn an_uninitialised_apartment_names_the_call_that_fixes_it() {
    let err = classify_hresult(HR_CO_E_NOTINITIALIZED, "probing capture support");
    let text = err.to_string();
    assert!(
        text.contains("RoInitialize"),
        "the message must name the fix: {text}"
    );
}

#[test]
fn an_unrecognised_hresult_keeps_its_code_and_its_context() {
    let err = classify_hresult(0x8007_1234_u32 as i32, "setting the window position");
    let text = err.to_string();
    assert!(text.contains("80071234"), "{text}");
    assert!(text.contains("setting the window position"), "{text}");
}

// ---------------------------------------------------------------------------
// COM apartment ownership
// ---------------------------------------------------------------------------

#[test]
fn entering_an_apartment_incurs_the_duty_to_leave_it() {
    let entry = classify_apartment_entry(0);
    assert_eq!(entry, ApartmentEntry::Entered);
    assert!(entry.is_usable());
    assert!(entry.owes_uninitialise());
}

#[test]
fn s_false_is_still_an_entry_and_still_owes_a_matching_exit() {
    // The classic COM leak: `S_FALSE` means "already initialised, count
    // incremented", which reads like "nothing happened" and is not. Skipping
    // the matching `RoUninitialize` pins the apartment for the process.
    let entry = classify_apartment_entry(1);
    assert_eq!(entry, ApartmentEntry::Entered);
    assert!(entry.owes_uninitialise(), "S_FALSE still took a reference");
}

#[test]
fn a_thread_already_in_the_other_model_is_usable_and_must_be_left_alone() {
    // winit calls `OleInitialize` on the event-loop thread, making it an STA.
    // Asking for MTA there returns RPC_E_CHANGED_MODE. WinRT works fine in an
    // STA, so this is not a failure \u2014 but uninitialising would tear down
    // winit's drag-and-drop registration from underneath it.
    let entry = classify_apartment_entry(HR_RPC_E_CHANGED_MODE);
    assert_eq!(entry, ApartmentEntry::AlreadyOtherModel);
    assert!(entry.is_usable(), "an STA is a perfectly good apartment");
    assert!(
        !entry.owes_uninitialise(),
        "leaving an apartment this call did not enter breaks the thread that did"
    );
}

#[test]
fn a_genuine_refusal_is_neither_usable_nor_owed_an_exit() {
    let entry = classify_apartment_entry(HR_E_OUTOFMEMORY);
    assert_eq!(entry, ApartmentEntry::Failed(HR_E_OUTOFMEMORY));
    assert!(!entry.is_usable());
    assert!(!entry.owes_uninitialise());
}

#[test]
fn usability_and_ownership_are_different_questions() {
    // Conflating them is the bug in both directions: treat "usable" as "owned"
    // and the STA gets torn down; treat "owned" as "usable" and a failed entry
    // is used anyway.
    let usable_but_not_owned = classify_apartment_entry(HR_RPC_E_CHANGED_MODE);
    assert!(usable_but_not_owned.is_usable() && !usable_but_not_owned.owes_uninitialise());

    for entry in [
        ApartmentEntry::Entered,
        ApartmentEntry::AlreadyOtherModel,
        ApartmentEntry::Failed(-1),
    ] {
        if entry.owes_uninitialise() {
            assert!(entry.is_usable(), "{entry:?}: owning implies usable");
        }
    }
}

// ---------------------------------------------------------------------------
// Level mapping
// ---------------------------------------------------------------------------

#[test]
fn windows_has_two_z_bands_and_the_mapping_says_so() {
    // macOS has a ladder of window levels; Windows has topmost and not. Every
    // level above Normal collapses to the same band, which means `Shielding`
    // does not actually shield. Stated here rather than discovered later.
    assert_eq!(z_order(OverlayLevel::Normal), ZOrder::Normal);
    for level in [
        OverlayLevel::Floating,
        OverlayLevel::Status,
        OverlayLevel::AboveMenuBar,
        OverlayLevel::Shielding,
    ] {
        assert_eq!(z_order(level), ZOrder::Topmost, "{level:?}");
    }
}

// ---------------------------------------------------------------------------
// What none of this establishes
//
// - That a window carrying these bits behaves as intended. Nothing here calls
//   Windows; the adapter that does is held up by `cargo check`, `clippy` and
//   `const` assertions against the real constants.
// - That `WS_EX_LAYERED` without `SetLayeredWindowAttributes` renders. winit
//   relies on exactly that for its own `set_cursor_hittest`, which is strong
//   evidence and is not observation.
// - That the `WM_STYLECHANGING` subclass survives winit's own subclassing, or
//   unhooks cleanly on `WM_NCDESTROY`.
// - That `MA_NOACTIVATE` keeps focus where it was during a real click.
//
// `tools/windows-smoke.ps1` is where those become answerable.
// ---------------------------------------------------------------------------
