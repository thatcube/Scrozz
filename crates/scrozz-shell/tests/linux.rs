//! Everything about the Linux overlay strategy that can be decided without a
//! compositor.
//!
//! # Why this file is worth its length
//!
//! The Linux overlay work has two halves with very different failure modes. The
//! half that talks to X11 and Wayland fails loudly — a request is refused, a
//! connection dies, and you find out. The half tested here fails *silently*: the
//! wrong exclusive zone hides the capture stack behind a panel, the wrong input
//! region eats every click on the desktop, the wrong plan claims success on a
//! compositor that placed the window somewhere else entirely. None of those
//! raise an error. All of them look, to a user, exactly like "the overlay is
//! broken".
//!
//! So the arithmetic and the decisions are pure, live in
//! `scrozz_shell::linux::{capability, layer, region, ewmh}`, and are tested
//! here — on every host, not just on Linux, because a macOS laptop can check
//! that GNOME gets a fallback just as well as a GNOME machine can.
//!
//! What this file cannot test is whether a real compositor agrees. That is
//! `tools/linux-smoke/`.

use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
use scrozz_shell::hotkey::{Compositor, DisplayServer};
use scrozz_shell::linux::capability::{
    Expectation, LayerShellProbe, OverlayBackend, Placement, adopted_plan, layer_shell_expectation,
    plan,
};
use scrozz_shell::linux::ewmh::{
    ALL_DESKTOPS, Managed, WM_HINTS_INPUT_FLAG, WM_HINTS_WORDS, WindowType, WmState,
    decode_wm_hints_input, encode_wm_hints_input, parse_work_area, plan_for, required_atoms,
};
use scrozz_shell::linux::layer::{
    Anchor, KeyboardInteractivity, Layer, LayerSurfaceConfig, NAMESPACE, layer_for,
};
use scrozz_shell::linux::region::{InputRegion, input_region};
use scrozz_shell::overlay::{OverlayBehavior, OverlayLevel, StackLayout, anchor_bottom_left};

/// A rectangle, spelled the short way, because these tests build a lot of them.
fn rect(x: f64, y: f64, w: f64, h: f64) -> LogicalRect {
    LogicalRect::new(LogicalPoint::new(x, y), LogicalSize::new(w, h))
}

// ---------------------------------------------------------------------------
// Compositor selection
// ---------------------------------------------------------------------------

mod choosing_a_backend {
    use super::*;

    #[test]
    fn x11_gets_the_retrofit_backend_regardless_of_what_the_probe_found() {
        // Layer-shell is a Wayland protocol. On X11 the probe result is not
        // merely irrelevant, it is meaningless — an XWayland session can be
        // sitting on a compositor that offers layer-shell, and the X11 window
        // still cannot use it. Letting the probe influence this would make the
        // backend depend on something that cannot affect the outcome.
        for probe in [
            LayerShellProbe::Present { version: 4 },
            LayerShellProbe::Absent,
            LayerShellProbe::NotProbed,
        ] {
            let chosen = plan(DisplayServer::X11, Compositor::Gnome, probe);
            assert_eq!(chosen.backend, OverlayBackend::X11Retrofit);
            assert_eq!(chosen.placement, Placement::Absolute);
        }
    }

    #[test]
    fn advertising_layer_shell_selects_scrozzs_owned_surface() {
        // Capability is paired with ownership: these plans select the native
        // host, never an attempt to promote eframe's xdg_toplevel.
        for compositor in [
            Compositor::Sway,
            Compositor::Kde,
            Compositor::Niri,
            Compositor::Other,
        ] {
            let chosen = plan(
                DisplayServer::Wayland,
                compositor,
                LayerShellProbe::Present { version: 4 },
            );
            assert_eq!(
                chosen.backend,
                OverlayBackend::LayerShell,
                "{compositor:?} advertised layer-shell"
            );
            assert_eq!(chosen.placement, Placement::Anchored);
            assert!(chosen.input_shaping);
            assert!(chosen.stays_above);
            assert!(chosen.controls_focus);
            assert!(chosen.detail.contains("rendered wlr-layer-shell"));
        }
    }

    #[test]
    fn gnome_keeps_d31_even_if_a_patched_mutter_advertises_layer_shell() {
        let chosen = plan(
            DisplayServer::Wayland,
            Compositor::Gnome,
            LayerShellProbe::Present { version: 4 },
        );
        assert_eq!(chosen.backend, OverlayBackend::CompositorPlaced);
        assert_eq!(chosen.placement, Placement::CompositorChosen);
        assert!(chosen.detail.contains("D31"));
    }

    #[test]
    fn a_compositor_that_was_asked_and_said_no_gets_the_fallback() {
        // The inverse, and the more important direction: an *answered* "no"
        // must not be overridden by the table saying KDE usually offers it.
        let chosen = plan(
            DisplayServer::Wayland,
            Compositor::Kde,
            LayerShellProbe::Absent,
        );
        assert_eq!(chosen.backend, OverlayBackend::CompositorPlaced);
        assert_eq!(chosen.placement, Placement::CompositorChosen);
    }

    #[test]
    fn gnome_falls_back_even_when_no_probe_was_possible() {
        // D31. Mutter's refusal is a stated position, not a version-dependent
        // gap, so it is the one case where the table is allowed to decide
        // without asking. Guessing optimistically here would mean opening a
        // window and only then discovering it landed in the middle of the
        // screen.
        let chosen = plan(
            DisplayServer::Wayland,
            Compositor::Gnome,
            LayerShellProbe::NotProbed,
        );
        assert_eq!(chosen.backend, OverlayBackend::CompositorPlaced);
        assert!(
            chosen.detail.contains("Mutter"),
            "the reason must name the component that refuses: {}",
            chosen.detail
        );
    }

    #[test]
    fn an_unprobed_wlroots_compositor_uses_the_fallback_and_says_why() {
        // "Expected to offer" is only a capability prior. Without a successful
        // live probe, the ordinary eframe window is the safe path.
        let chosen = plan(
            DisplayServer::Wayland,
            Compositor::Sway,
            LayerShellProbe::NotProbed,
        );
        assert_eq!(chosen.backend, OverlayBackend::CompositorPlaced);
        assert_eq!(chosen.placement, Placement::CompositorChosen);
        assert!(
            chosen.detail.contains("not been able to verify"),
            "an unverified plan must not read as a verified one: {}",
            chosen.detail
        );
    }

    #[test]
    fn an_unknown_unprobed_compositor_falls_back_rather_than_hoping() {
        // `Compositor::Other` is by definition unknown, so there is no prior to
        // lean on. Choosing the fallback means the overlay appears somewhere,
        // which beats choosing layer-shell and having the promotion refused.
        let chosen = plan(
            DisplayServer::Wayland,
            Compositor::Other,
            LayerShellProbe::NotProbed,
        );
        assert_eq!(chosen.backend, OverlayBackend::CompositorPlaced);
    }

    #[test]
    fn a_headless_session_draws_nothing_and_admits_it() {
        // CI and bare TTYs. The property that matters is `draws_anything`:
        // a no-op backend must never be reported as a working one.
        let chosen = plan(
            DisplayServer::Headless,
            Compositor::Other,
            LayerShellProbe::NotProbed,
        );
        assert_eq!(chosen.backend, OverlayBackend::Headless);
        assert!(!chosen.draws_anything());
        assert!(!chosen.is_fully_controlled());
    }

    #[test]
    fn the_expectation_table_matches_what_each_project_actually_ships() {
        // Pinned deliberately. If someone adds a compositor to the enum and
        // forgets the table, `Other`'s `Unknown` would quietly become the
        // answer for a compositor that does support layer-shell.
        assert_eq!(
            layer_shell_expectation(Compositor::Sway),
            Expectation::Implements
        );
        assert_eq!(
            layer_shell_expectation(Compositor::Hyprland),
            Expectation::Implements
        );
        assert_eq!(
            layer_shell_expectation(Compositor::River),
            Expectation::Implements
        );
        assert_eq!(
            layer_shell_expectation(Compositor::Niri),
            Expectation::Implements
        );
        assert_eq!(
            layer_shell_expectation(Compositor::Wayfire),
            Expectation::Implements
        );
        assert_eq!(
            layer_shell_expectation(Compositor::Kde),
            Expectation::Implements
        );
        assert_eq!(
            layer_shell_expectation(Compositor::Gnome),
            Expectation::Refuses
        );
        assert_eq!(
            layer_shell_expectation(Compositor::Other),
            Expectation::Unknown
        );
    }

    #[test]
    fn every_plan_explains_itself() {
        // A plan with an empty reason is indistinguishable from a bug, and this
        // string is what both the diagnostics and every error path show a user.
        for server in [
            DisplayServer::X11,
            DisplayServer::Wayland,
            DisplayServer::Headless,
            DisplayServer::Quartz,
            DisplayServer::Windows,
        ] {
            for compositor in [Compositor::Gnome, Compositor::Kde, Compositor::Other] {
                for probe in [
                    LayerShellProbe::Present { version: 1 },
                    LayerShellProbe::Absent,
                    LayerShellProbe::NotProbed,
                ] {
                    let chosen = plan(server, compositor, probe);
                    assert!(
                        chosen.detail.len() > 20,
                        "{server:?}/{compositor:?}/{probe:?} gave no usable reason"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Fallback reporting: never claim success from a backend that does nothing
// ---------------------------------------------------------------------------

mod reporting_the_fallback {
    use super::*;

    #[test]
    fn the_gnome_fallback_never_claims_to_control_placement() {
        // The whole point of D31's "visibly intentional rather than pretending
        // to be anchored". Scrozz still shows the capture stack — an ordinary
        // window beats no window — but `is_fully_controlled` is what the
        // diagnostics read, and it must not say yes.
        let chosen = plan(
            DisplayServer::Wayland,
            Compositor::Gnome,
            LayerShellProbe::Absent,
        );
        assert!(chosen.draws_anything(), "the fallback still shows a window");
        assert!(
            !chosen.is_fully_controlled(),
            "the fallback does not choose where the window lands"
        );
        assert!(!chosen.stays_above);
        assert_eq!(chosen.placement, Placement::CompositorChosen);
    }

    #[test]
    fn the_fallback_reason_distinguishes_asked_from_assumed() {
        // Two very different situations for the person reading the log. One is
        // "your compositor said no"; the other is "Scrozz could not ask". They
        // call for different actions, so they must not share a sentence.
        let asked = plan(
            DisplayServer::Wayland,
            Compositor::Other,
            LayerShellProbe::Absent,
        );
        let assumed = plan(
            DisplayServer::Wayland,
            Compositor::Other,
            LayerShellProbe::NotProbed,
        );
        assert_ne!(
            asked.detail, assumed.detail,
            "a probed refusal and an unprobed guess must not read alike"
        );
    }

    #[test]
    fn the_gnome_reason_says_region_selection_still_works() {
        // Otherwise the message reads as "Scrozz does not work on GNOME", which
        // is false and is the sort of thing that gets repeated. Region
        // selection is portal-owned and entirely unaffected.
        let chosen = plan(
            DisplayServer::Wayland,
            Compositor::Gnome,
            LayerShellProbe::Absent,
        );
        assert!(
            chosen.detail.contains("portal"),
            "the reason must scope the limitation: {}",
            chosen.detail
        );
    }

    #[test]
    fn advertised_layer_shell_is_fully_controlled_by_the_owned_surface() {
        // `is_fully_controlled` gates the "overlay: ready" line in diagnostics.
        // It turns green only because this plan selects the owned rendered
        // surface rather than the compositor-positioned eframe fallback.
        let x11 = plan(
            DisplayServer::X11,
            Compositor::Other,
            LayerShellProbe::NotProbed,
        );
        let layer = plan(
            DisplayServer::Wayland,
            Compositor::Sway,
            LayerShellProbe::Present { version: 4 },
        );
        assert!(x11.is_fully_controlled());
        assert!(layer.is_fully_controlled());
        assert_eq!(layer.backend, OverlayBackend::LayerShell);
        assert_eq!(layer.placement, Placement::Anchored);
    }

    #[test]
    fn an_adopted_wayland_toplevel_is_never_promoted_to_layer_shell() {
        let adopted = adopted_plan(
            DisplayServer::Wayland,
            Compositor::Sway,
            LayerShellProbe::Present { version: 4 },
        );

        assert_eq!(adopted.backend, OverlayBackend::CompositorPlaced);
        assert_eq!(adopted.placement, Placement::CompositorChosen);
        assert!(!adopted.is_fully_controlled());
        assert!(adopted.detail.contains("ordinary compositor-positioned"));
    }
}

// ---------------------------------------------------------------------------
// Layer-shell configuration
// ---------------------------------------------------------------------------

mod configuring_a_layer_surface {
    use super::*;

    #[test]
    fn the_capture_stack_anchors_bottom_left_and_sizes_itself() {
        // D28's geometry expressed in layer-shell terms: the compositor is told
        // an edge and a margin, because layer-shell has no coordinates to give
        // it. Both dimensions must be concrete — see the size-zero test below.
        let config = LayerSurfaceConfig::for_behavior(
            &OverlayBehavior::capture_card(),
            LogicalSize::new(320.0, 180.0),
            16.0,
        );
        assert_eq!(config.anchor, Anchor::BOTTOM_LEFT);
        assert_eq!(config.width, 320);
        assert_eq!(config.height, 180);
        assert_eq!(config.margins.bottom, 16);
        assert_eq!(config.margins.left, 16);
        assert_eq!(config.margins.top, 0);
        assert_eq!(config.margins.right, 0);
        assert_eq!(config.namespace, NAMESPACE);
        assert!(config.rejection_reason().is_none());
    }

    #[test]
    fn the_capture_stack_asks_to_be_pushed_off_panels_rather_than_covering_them() {
        // The single most consequential number in this file. Exclusive zone 0
        // means "move me out of the way of anything with a positive zone" —
        // i.e. respect the panel — and it is the layer-shell equivalent of
        // anchoring to `_NET_WORKAREA`. Sending -1 here would put the capture
        // stack *underneath* a KDE panel, which reads to a user as the overlay
        // never opening at all.
        let config = LayerSurfaceConfig::for_behavior(
            &OverlayBehavior::capture_card(),
            LogicalSize::new(320.0, 180.0),
            16.0,
        );
        assert_eq!(config.exclusive_zone, 0);
    }

    #[test]
    fn the_selection_overlay_covers_the_output_including_the_panel() {
        // The opposite case, and the only place -1 is right: a selection shield
        // that stopped at the panel would leave a strip of screen the user can
        // see but not select.
        let config = LayerSurfaceConfig::for_behavior(
            &OverlayBehavior::selection_overlay(),
            LogicalSize::new(1920.0, 1080.0),
            16.0,
        );
        assert_eq!(config.anchor, Anchor::ALL);
        assert_eq!(config.exclusive_zone, -1);
        assert_eq!(config.margins.bottom, 0, "a shield has no gap to leave");
    }

    #[test]
    fn a_fullscreen_surface_hands_both_dimensions_to_the_compositor() {
        // Size 0 means "you decide", which is legal precisely because opposite
        // edges are anchored — and correct, because it is the only way to cover
        // an output whose size Scrozz was never told.
        let config = LayerSurfaceConfig::for_behavior(
            &OverlayBehavior::selection_overlay(),
            LogicalSize::new(1920.0, 1080.0),
            0.0,
        );
        assert_eq!(config.width, 0);
        assert_eq!(config.height, 0);
        assert!(
            config.rejection_reason().is_none(),
            "spanning both axes makes size 0 legal"
        );
    }

    #[test]
    fn a_zero_size_without_opposite_edges_is_caught_before_it_is_sent() {
        // The protocol calls this a fatal error, and a fatal Wayland error
        // destroys the whole client connection — every Scrozz window, not just
        // the overlay. So it is checked here rather than discovered afterwards.
        let bad = LayerSurfaceConfig {
            width: 0,
            anchor: Anchor::BOTTOM_LEFT,
            ..LayerSurfaceConfig::for_behavior(
                &OverlayBehavior::capture_card(),
                LogicalSize::new(320.0, 180.0),
                16.0,
            )
        };
        let reason = bad.rejection_reason().expect("width 0 must be rejected");
        assert!(reason.contains("left"), "{reason}");
        assert!(reason.contains("right"), "{reason}");

        let bad = LayerSurfaceConfig {
            height: 0,
            anchor: Anchor::BOTTOM_LEFT,
            ..LayerSurfaceConfig::for_behavior(
                &OverlayBehavior::capture_card(),
                LogicalSize::new(320.0, 180.0),
                16.0,
            )
        };
        let reason = bad.rejection_reason().expect("height 0 must be rejected");
        assert!(reason.contains("top"), "{reason}");
        assert!(reason.contains("bottom"), "{reason}");
    }

    #[test]
    fn a_fractional_size_rounds_up_so_the_surface_never_clips_its_own_content() {
        // Half a pixel short is a visibly cut-off card border on a scaled
        // output. Half a pixel long is invisible.
        let config = LayerSurfaceConfig::for_behavior(
            &OverlayBehavior::capture_card(),
            LogicalSize::new(320.2, 179.6),
            16.0,
        );
        assert_eq!(config.width, 321);
        assert_eq!(config.height, 180);
    }

    #[test]
    fn a_nonsense_size_becomes_compositor_decides_rather_than_a_panic() {
        // Sizes arrive from layout arithmetic, which can produce NaN when a
        // display list is empty. A wrapped `u32` or a panic would both be worse
        // than an oddly sized card.
        let config = LayerSurfaceConfig::for_behavior(
            &OverlayBehavior::capture_card(),
            LogicalSize::new(f64::NAN, -5.0),
            16.0,
        );
        assert_eq!(config.width, 0);
        assert_eq!(config.height, 0);
        assert!(
            config.rejection_reason().is_some(),
            "and because it is not fullscreen, the guard catches it"
        );
    }

    #[test]
    fn a_card_that_takes_no_keyboard_asks_for_no_keyboard() {
        // D27: the capture stack must never steal focus. Layer surfaces get no
        // keyboard by default, and asking for none keeps it that way.
        let card = LayerSurfaceConfig::for_behavior(
            &OverlayBehavior::capture_card(),
            LogicalSize::new(320.0, 180.0),
            16.0,
        );
        assert_eq!(card.keyboard_interactivity, KeyboardInteractivity::None);
    }

    #[test]
    fn the_selection_overlay_takes_the_keyboard_because_escape_must_cancel() {
        let shield = LayerSurfaceConfig::for_behavior(
            &OverlayBehavior::selection_overlay(),
            LogicalSize::new(1920.0, 1080.0),
            0.0,
        );
        assert_eq!(
            shield.keyboard_interactivity,
            KeyboardInteractivity::Exclusive
        );
    }

    #[test]
    fn overlay_levels_map_onto_layers_without_landing_in_the_ordinary_band() {
        // Ordinary windows sit *between* Bottom and Top, in a band no layer
        // surface can occupy. `Normal` therefore has no honest mapping, and
        // maps to Bottom so that a surface which forgot to opt in is harmless
        // rather than mysteriously on top of everything.
        assert_eq!(layer_for(OverlayLevel::Normal), Layer::Bottom);
        assert_eq!(layer_for(OverlayLevel::Floating), Layer::Top);
        assert_eq!(layer_for(OverlayLevel::Status), Layer::Top);
        assert_eq!(layer_for(OverlayLevel::AboveMenuBar), Layer::Overlay);
        assert_eq!(layer_for(OverlayLevel::Shielding), Layer::Overlay);
    }

    #[test]
    fn anchor_bits_match_the_protocol_and_not_merely_each_other() {
        // These four values go on the wire. Getting them self-consistent but
        // wrong would anchor the stack to the top-right and pass every test
        // that only compared them to one another.
        assert_eq!(Anchor::TOP.bits(), 1);
        assert_eq!(Anchor::BOTTOM.bits(), 2);
        assert_eq!(Anchor::LEFT.bits(), 4);
        assert_eq!(Anchor::RIGHT.bits(), 8);
        assert_eq!(Anchor::BOTTOM_LEFT.bits(), 2 | 4);
        assert_eq!(Anchor::ALL.bits(), 15);
    }

    #[test]
    fn spanning_an_axis_needs_both_of_its_edges() {
        assert!(!Anchor::BOTTOM_LEFT.spans_horizontally());
        assert!(!Anchor::BOTTOM_LEFT.spans_vertically());
        assert!(Anchor::ALL.spans_horizontally());
        assert!(Anchor::ALL.spans_vertically());
        assert!(!Anchor::LEFT.spans_horizontally());
    }
}

// ---------------------------------------------------------------------------
// Input regions
// ---------------------------------------------------------------------------

mod shaping_input {
    use super::*;

    #[test]
    fn a_stack_with_no_cards_swallows_no_clicks_at_all() {
        // D27's invisibility at rest, stated as a property. The overlay window
        // spans a large part of the screen even when empty; without this, every
        // click on that part of the desktop would land on Scrozz instead of on
        // whatever the user was aiming at.
        let region = input_region(rect(0.0, 0.0, 1920.0, 1080.0), &[], true);
        assert_eq!(region, InputRegion::Nothing);
    }

    #[test]
    fn only_the_cards_accept_clicks_and_they_are_in_surface_local_coordinates() {
        // The translation is the part that is easy to get wrong and impossible
        // to spot by reading: a region in screen coordinates would put the
        // clickable area at the wrong end of the window.
        let window = rect(100.0, 200.0, 400.0, 300.0);
        let card = rect(120.0, 220.0, 80.0, 40.0);
        let InputRegion::Rects(rects) = input_region(window, &[card], true) else {
            panic!("a card inside the window must produce a rectangle");
        };
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].x, 20);
        assert_eq!(rects[0].y, 20);
        assert_eq!(rects[0].width, 80);
        assert_eq!(rects[0].height, 40);
    }

    #[test]
    fn a_surface_that_wants_every_click_is_told_so_explicitly() {
        // The selection overlay. `Everything` is not the same as one rectangle
        // covering the window: an unshaped window follows its own geometry when
        // it resizes, and a rectangle does not.
        let region = input_region(rect(0.0, 0.0, 1920.0, 1080.0), &[], false);
        assert_eq!(region, InputRegion::Everything);
        let region = input_region(
            rect(0.0, 0.0, 1920.0, 1080.0),
            &[rect(10.0, 10.0, 10.0, 10.0)],
            false,
        );
        assert_eq!(
            region,
            InputRegion::Everything,
            "click_through=false outranks the hit list"
        );
    }

    #[test]
    fn fractional_card_bounds_round_outward_so_no_clickable_pixel_is_lost() {
        // Rounding inward would leave a hairline along a card's edge that looks
        // clickable and is not — the most irritating possible bug, because it
        // is intermittent from the user's point of view.
        let window = rect(0.0, 0.0, 400.0, 300.0);
        let card = rect(10.4, 20.6, 30.3, 40.1);
        let InputRegion::Rects(rects) = input_region(window, &[card], true) else {
            panic!("expected a rectangle");
        };
        assert_eq!(rects[0].x, 10, "left edge floors");
        assert_eq!(rects[0].y, 20, "top edge floors");
        assert_eq!(rects[0].x + rects[0].width as i32, 41, "right edge ceils");
        assert_eq!(rects[0].y + rects[0].height as i32, 61, "bottom edge ceils");
    }

    #[test]
    fn a_card_hanging_off_the_edge_is_clipped_rather_than_sent_as_is() {
        // A region extending past the surface is not merely wasteful; some
        // compositors reject it. Clipping keeps the visible half clickable.
        let window = rect(0.0, 0.0, 100.0, 100.0);
        let card = rect(80.0, 80.0, 60.0, 60.0);
        let InputRegion::Rects(rects) = input_region(window, &[card], true) else {
            panic!("the overlapping part must survive");
        };
        assert_eq!(rects[0].x, 80);
        assert_eq!(rects[0].width, 20);
        assert_eq!(rects[0].height, 20);
    }

    #[test]
    fn a_card_entirely_outside_the_window_collapses_to_nothing_not_to_an_empty_list() {
        // A card animating out is legitimately outside its own window for a
        // frame or two. `Rects(vec![])` and `Nothing` mean the same thing, but
        // only one of them survives a round-trip through a protocol, so only
        // one is ever produced.
        let window = rect(0.0, 0.0, 100.0, 100.0);
        let gone = rect(500.0, 500.0, 50.0, 50.0);
        assert_eq!(input_region(window, &[gone], true), InputRegion::Nothing);
    }

    #[test]
    fn a_zero_area_card_contributes_nothing() {
        // Zero-width rectangles are rejected by X11's SHAPE extension and are
        // meaningless to Wayland. They arise from layout arithmetic, so they
        // are filtered rather than forwarded.
        let window = rect(0.0, 0.0, 100.0, 100.0);
        assert_eq!(
            input_region(window, &[rect(10.0, 10.0, 0.0, 20.0)], true),
            InputRegion::Nothing
        );
    }

    #[test]
    fn a_nonfinite_card_is_dropped_instead_of_swallowing_the_whole_window() {
        // This one found a real bug. `f64::max` and `f64::min` deliberately
        // ignore NaN and return the other operand, so clamping a NaN card
        // against the window silently produced the *window's own* edges — a
        // region covering everything, which is the precise opposite of what
        // D27 asks for. Checking after the clamp checked a laundered value.
        //
        // Sizes reach here from layout arithmetic, which produces NaN whenever
        // it divides by an empty display list, so this is not a theoretical
        // input.
        let window = rect(0.0, 0.0, 100.0, 100.0);
        for bad in [
            rect(f64::NAN, 10.0, 20.0, 20.0),
            rect(10.0, f64::NAN, 20.0, 20.0),
            rect(10.0, 10.0, f64::NAN, 20.0),
            rect(10.0, 10.0, 20.0, f64::NAN),
            rect(f64::INFINITY, 10.0, 20.0, 20.0),
            rect(10.0, 10.0, f64::INFINITY, 20.0),
        ] {
            assert_eq!(
                input_region(window, &[bad], true),
                InputRegion::Nothing,
                "{bad:?} must contribute nothing, not everything"
            );
        }
    }

    #[test]
    fn a_nonfinite_window_shapes_away_every_click_rather_than_none() {
        // The same argument for the other operand. A window whose own geometry
        // is not a number cannot be reasoned about, and the safe answer is the
        // one that gives the user their desktop back.
        assert_eq!(
            input_region(
                rect(f64::NAN, 0.0, 100.0, 100.0),
                &[rect(10.0, 10.0, 20.0, 20.0)],
                true
            ),
            InputRegion::Nothing
        );
    }

    #[test]
    fn every_card_in_a_stack_gets_its_own_rectangle() {
        // D28 stacks cards vertically. One merged bounding box would make the
        // gaps between them clickable, which is exactly what shaping exists to
        // prevent.
        let window = rect(0.0, 0.0, 400.0, 600.0);
        let cards = [
            rect(10.0, 10.0, 300.0, 100.0),
            rect(10.0, 130.0, 300.0, 100.0),
            rect(10.0, 250.0, 300.0, 100.0),
        ];
        let InputRegion::Rects(rects) = input_region(window, &cards, true) else {
            panic!("expected three rectangles");
        };
        assert_eq!(rects.len(), 3);
        assert_eq!(rects[1].y, 130);
    }
}

// ---------------------------------------------------------------------------
// Work-area anchoring
// ---------------------------------------------------------------------------

mod anchoring_to_the_work_area {
    use super::*;

    /// Builds a `_NET_WORKAREA` property: four CARD32s per desktop.
    fn work_area_bytes(desktops: &[[u32; 4]]) -> Vec<u8> {
        desktops
            .iter()
            .flat_map(|d| d.iter().flat_map(|v| v.to_ne_bytes()))
            .collect()
    }

    #[test]
    fn the_work_area_is_read_for_the_desktop_actually_in_use() {
        // `_NET_WORKAREA` holds one rectangle per virtual desktop. Reading the
        // first one on desktop 3 would anchor the card using another desktop's
        // panel layout — which usually looks right, and is wrong exactly when
        // the user has a per-desktop panel.
        let bytes = work_area_bytes(&[[0, 0, 1920, 1040], [0, 24, 1920, 1016]]);
        let first = parse_work_area(&bytes, 0).expect("desktop 0 is present");
        assert_eq!(
            (first.x, first.y, first.width, first.height),
            (0, 0, 1920, 1040)
        );
        let second = parse_work_area(&bytes, 1).expect("desktop 1 is present");
        assert_eq!(
            (second.x, second.y, second.width, second.height),
            (0, 24, 1920, 1016)
        );
    }

    #[test]
    fn a_desktop_index_past_the_end_reports_nothing_rather_than_the_wrong_desktop() {
        // Falling back to desktop 0 here would be a silent wrong answer. `None`
        // makes the caller use its default, which is honest.
        let bytes = work_area_bytes(&[[0, 0, 1920, 1040]]);
        assert!(parse_work_area(&bytes, 5).is_none());
    }

    #[test]
    fn a_truncated_or_absent_property_reports_nothing() {
        // Window managers that do not set `_NET_WORKAREA` are common enough
        // (several wlroots-adjacent X11 WMs skip it), and a short read from a
        // racing property change is possible at any time.
        assert!(parse_work_area(&[], 0).is_none());
        assert!(
            parse_work_area(&[0, 0, 0, 0, 0, 0], 0).is_none(),
            "six bytes is not four CARD32s"
        );
    }

    #[test]
    fn a_panel_at_the_bottom_moves_the_anchor_up_by_exactly_its_height() {
        // The property the whole `_NET_WORKAREA` read exists for. With a 48px
        // bottom panel the work area ends 48px early, and D28's bottom-anchored
        // stack must start from there rather than from the screen edge.
        let full = work_area_bytes(&[[0, 0, 1920, 1080]]);
        let with_panel = work_area_bytes(&[[0, 0, 1920, 1032]]);

        let full = parse_work_area(&full, 0).expect("present");
        let panelled = parse_work_area(&with_panel, 0).expect("present");

        let as_rect = |w: &scrozz_shell::linux::ewmh::WireRect| {
            rect(
                f64::from(w.x),
                f64::from(w.y),
                f64::from(w.width),
                f64::from(w.height),
            )
        };

        let size = LogicalSize::new(320.0, 180.0);
        let free = anchor_bottom_left(as_rect(&full), size, 16.0);
        let above_panel = anchor_bottom_left(as_rect(&panelled), size, 16.0);

        assert_eq!(
            free.origin.y - above_panel.origin.y,
            48.0,
            "the card must rise by exactly the panel's height"
        );
        assert_eq!(
            free.origin.x, above_panel.origin.x,
            "a bottom panel must not move the card sideways"
        );
    }

    #[test]
    fn a_left_panel_moves_the_anchor_right_by_exactly_its_width() {
        // The same argument on the other axis: `_NET_WORKAREA`'s origin is not
        // always (0, 0), and treating it as though it were is the specific bug
        // that puts the card underneath a vertical dock.
        let bytes = work_area_bytes(&[[72, 0, 1848, 1080]]);
        let area = parse_work_area(&bytes, 0).expect("present");
        let placed = anchor_bottom_left(
            rect(
                f64::from(area.x),
                f64::from(area.y),
                f64::from(area.width),
                f64::from(area.height),
            ),
            LogicalSize::new(320.0, 180.0),
            16.0,
        );
        assert_eq!(placed.origin.x, 72.0 + 16.0);
    }

    #[test]
    fn the_stack_grows_upward_from_the_work_area_floor() {
        // D28. Slot 0 sits at the bottom and each further slot must move
        // *towards the top of the screen*; growing downward would push the
        // second card off the bottom edge, where it would be invisible and
        // still eating clicks.
        let area = rect(0.0, 0.0, 1920.0, 1032.0);
        let layout = StackLayout {
            card: LogicalSize::new(320.0, 90.0),
            margin: 16.0,
            gap: 8.0,
            max_slots: 5,
        };

        let bottom = layout.slot_frame(area, 0);
        let second = layout.slot_frame(area, 1);
        let third = layout.slot_frame(area, 2);

        assert!(
            second.origin.y < bottom.origin.y,
            "slot 1 must sit above slot 0"
        );
        assert_eq!(
            bottom.origin.y - second.origin.y,
            98.0,
            "the pitch is one card plus one gap"
        );
        assert_eq!(
            second.origin.y - third.origin.y,
            98.0,
            "and it stays constant as the stack grows"
        );
        assert_eq!(
            bottom.origin.x, third.origin.x,
            "the stack is a column: every card shares the left edge"
        );
    }

    #[test]
    fn the_bottom_slot_sits_one_margin_above_the_work_area_floor() {
        // The number that makes a bottom panel matter. If the work area is
        // shortened by a panel, the whole stack must move with it — this is the
        // same assertion as the panel test above, expressed against the real
        // D28 layout rather than the anchor helper.
        let full = rect(0.0, 0.0, 1920.0, 1080.0);
        let panelled = rect(0.0, 0.0, 1920.0, 1032.0);
        let layout = StackLayout {
            card: LogicalSize::new(320.0, 90.0),
            margin: 16.0,
            gap: 8.0,
            max_slots: 5,
        };

        assert_eq!(
            layout.slot_frame(full, 0).origin.y + 90.0,
            1080.0 - 16.0,
            "the card's bottom edge is one margin above the work-area floor"
        );
        assert_eq!(
            layout.slot_frame(full, 0).origin.y - layout.slot_frame(panelled, 0).origin.y,
            48.0,
            "a 48px panel must lift the whole stack by 48px"
        );
    }

    #[test]
    fn a_shorter_work_area_holds_fewer_cards() {
        // Capacity is derived from the work area, so a panel does not merely
        // move the stack — it can also reduce how tall it is allowed to grow.
        let layout = StackLayout {
            card: LogicalSize::new(320.0, 90.0),
            margin: 16.0,
            gap: 8.0,
            max_slots: 32,
        };
        let tall = layout.capacity(rect(0.0, 0.0, 1920.0, 1080.0));
        let short = layout.capacity(rect(0.0, 0.0, 1920.0, 400.0));
        assert!(
            short < tall,
            "a 400px work area must not claim to hold as many cards as a 1080px one"
        );
        assert!(short >= 1, "there is always room for at least one card");
    }
}

// ---------------------------------------------------------------------------
// X11 property planning
// ---------------------------------------------------------------------------

mod planning_x11_properties {
    use super::*;

    #[test]
    fn an_override_redirect_window_is_told_that_ewmh_will_do_nothing() {
        // The finding that shaped this module. `scrozz-ui` creates the overlay
        // viewport with `override_redirect(true)`, and for such a window the
        // window manager never sees it at all — so `_NET_WM_STATE`,
        // `_NET_WM_WINDOW_TYPE` and `_NET_WM_DESKTOP` are all dead letters.
        // Sending them anyway would look like it worked.
        let plan = plan_for(&OverlayBehavior::capture_card(), Managed::OverrideRedirect);
        assert!(
            plan.states.is_empty(),
            "no window manager is listening, so nothing should be sent"
        );
        assert!(!plan.all_desktops);
        assert!(
            plan.client_restacks,
            "the client is the only stacking agent"
        );
        assert!(
            plan.notes.iter().any(|n| n.contains("override-redirect")),
            "the reason must be visible to a human reading diagnostics"
        );
    }

    #[test]
    fn a_managed_capture_card_asks_to_stay_above_and_out_of_the_way() {
        // The three behaviours the brief names for X11: above, tool-window
        // (skip-taskbar/skip-pager), and out of the alt-tab cycle.
        let plan = plan_for(&OverlayBehavior::capture_card(), Managed::ByWindowManager);
        assert!(plan.states.contains(&WmState::Above));
        assert!(plan.states.contains(&WmState::SkipTaskbar));
        assert!(plan.states.contains(&WmState::SkipPager));
        assert_eq!(
            plan.window_type,
            WindowType::Utility,
            "a capture card is a tool window, not an application window"
        );
    }

    #[test]
    fn a_card_that_takes_no_keys_does_not_ask_the_window_manager_for_focus() {
        // D27 again, in the ICCCM focus model: `WM_HINTS.input` cleared is how
        // a client says "do not focus me".
        let plan = plan_for(&OverlayBehavior::capture_card(), Managed::ByWindowManager);
        assert!(!plan.takes_focus);
        assert!(
            !plan.client_focuses,
            "a managed window must not grab focus behind the window manager's back"
        );
    }

    #[test]
    fn a_selection_shield_is_a_dock_and_is_fullscreen() {
        // A shield is not a tool palette: DOCK is what keeps it above panels on
        // window managers that stack by type, and FULLSCREEN is what stops the
        // window manager from reserving space around it.
        let plan = plan_for(
            &OverlayBehavior::selection_overlay(),
            Managed::ByWindowManager,
        );
        assert_eq!(plan.window_type, WindowType::Dock);
        assert!(plan.states.contains(&WmState::Fullscreen));
    }

    #[test]
    fn a_card_on_every_workspace_says_so_twice_because_window_managers_differ() {
        // `_NET_WM_STATE_STICKY` and `_NET_WM_DESKTOP = 0xFFFFFFFF` mean the
        // same thing to different window managers, and neither is universally
        // honoured. Sending both is not redundancy, it is coverage.
        let behavior = OverlayBehavior {
            join_all_spaces: true,
            ..OverlayBehavior::capture_card()
        };
        let plan = plan_for(&behavior, Managed::ByWindowManager);
        assert!(plan.states.contains(&WmState::Sticky));
        assert!(plan.all_desktops);
        assert_eq!(ALL_DESKTOPS, 0xFFFF_FFFF);
    }

    #[test]
    fn every_atom_the_backend_will_intern_is_declared_up_front() {
        // Interning is batched into one round-trip, so a missing name is not a
        // late error but a silently skipped property.
        let atoms = required_atoms();
        for expected in [
            "_NET_WM_STATE",
            "_NET_WM_STATE_ABOVE",
            "_NET_WM_STATE_SKIP_TASKBAR",
            "_NET_WM_STATE_SKIP_PAGER",
            "_NET_WM_STATE_STICKY",
            "_NET_WM_STATE_FULLSCREEN",
            "_NET_WM_WINDOW_TYPE",
            "_NET_WM_DESKTOP",
            "_NET_WORKAREA",
            "_NET_CURRENT_DESKTOP",
        ] {
            assert!(atoms.contains(&expected), "{expected} is never interned");
        }
    }

    #[test]
    fn no_atom_is_interned_twice() {
        // Duplicates are harmless to X but mean the list has been edited by
        // hand into disagreeing with itself.
        let mut atoms = required_atoms();
        let before = atoms.len();
        atoms.sort_unstable();
        atoms.dedup();
        assert_eq!(before, atoms.len());
    }
}

// ---------------------------------------------------------------------------
// WM_HINTS
// ---------------------------------------------------------------------------

mod rewriting_wm_hints {
    use super::*;

    fn hints(flags: u32, input: u32, group: u32) -> Vec<u8> {
        let mut words = [0u32; WM_HINTS_WORDS];
        words[0] = flags;
        words[1] = input;
        words[8] = group;
        words.iter().flat_map(|w| w.to_ne_bytes()).collect()
    }

    #[test]
    fn clearing_the_input_flag_preserves_the_window_group() {
        // The reason this function reads before it writes. Replacing `WM_HINTS`
        // wholesale would drop `window_group`, which is what tells the window
        // manager the overlay belongs to Scrozz — and losing it makes the
        // overlay a stray window in task switchers that group by application.
        let existing = hints(WM_HINTS_INPUT_FLAG | 64, 1, 0x00AB_CDEF);
        let rewritten = encode_wm_hints_input(Some(&existing), false);

        assert_eq!(decode_wm_hints_input(&rewritten), Some(false));

        let group = u32::from_ne_bytes(rewritten[32..36].try_into().expect("nine words"));
        assert_eq!(group, 0x00AB_CDEF, "the window group must survive");

        let flags = u32::from_ne_bytes(rewritten[0..4].try_into().expect("nine words"));
        assert_eq!(
            flags & 64,
            64,
            "unrelated flags must survive the rewrite too"
        );
    }

    #[test]
    fn setting_input_marks_the_field_meaningful() {
        // Writing `input` without setting its flag bit leaves the value there
        // to be ignored — which looks like the write succeeded and did nothing.
        let rewritten = encode_wm_hints_input(None, true);
        let flags = u32::from_ne_bytes(rewritten[0..4].try_into().expect("nine words"));
        assert_eq!(flags & WM_HINTS_INPUT_FLAG, WM_HINTS_INPUT_FLAG);
        assert_eq!(decode_wm_hints_input(&rewritten), Some(true));
    }

    #[test]
    fn a_missing_or_malformed_property_still_produces_a_well_formed_one() {
        // A truncated read or a property written by a different toolkit. Either
        // way, a malformed `WM_HINTS` is worse than a default one, so the
        // result is always exactly nine words.
        for existing in [None, Some(&[1u8, 2, 3][..]), Some(&[0u8; 64][..])] {
            let rewritten = encode_wm_hints_input(existing, false);
            assert_eq!(rewritten.len(), WM_HINTS_WORDS * 4);
            assert_eq!(decode_wm_hints_input(&rewritten), Some(false));
        }
    }

    #[test]
    fn a_property_whose_input_flag_is_clear_reports_no_opinion() {
        // Not the same as "input false": the field is simply not meaningful,
        // and reporting `Some(false)` would invent an assertion the client
        // never made.
        let existing = hints(0, 1, 0);
        assert_eq!(decode_wm_hints_input(&existing), None);
    }

    #[test]
    fn a_short_property_decodes_to_no_opinion_rather_than_panicking() {
        assert_eq!(decode_wm_hints_input(&[]), None);
        assert_eq!(decode_wm_hints_input(&[0, 0, 0]), None);
    }
}
