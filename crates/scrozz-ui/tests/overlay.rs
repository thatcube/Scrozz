//! The overlay window and the capture card, rendered headlessly.
//!
//! Two things are under test here, and only one of them is about pictures.
//!
//! 1. **The card paints, and hover changes it.** Baseline liveness: a card that
//!    silently draws nothing would pass every logic test in the crate.
//! 2. **Every provenance shares one fixed preview silhouette.** Exported pixels
//!    remain sacred (D9), while every transient thumbnail cover-fills the same
//!    frame and every hover wash follows that same outer curve.
//!
//! Everything renders through [`scrozz_ui::harness`]: pure CPU, virtual clock,
//! no window, no GPU, bit-identical on every machine. Nothing in this file
//! opens anything on screen.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scrozz_core::Provenance;
use scrozz_ui::card::{self, CardChrome, CardContent};
use scrozz_ui::harness::{
    Background, Image, Profile, RenderSpec, Scenario, Scene, SceneCtx, SceneRegistry,
    SoftwareRenderer, VirtualClock,
};
use scrozz_ui::icons::IconStore;
use scrozz_ui::motion::Motion;
use scrozz_ui::overlay_app::{self, OverlayGeometry, OverlayHandle, OverlayOptions, Passthrough};
use scrozz_ui::paint::Surface;
use scrozz_ui::stack::{CardFrame, CardId, CardState};
use scrozz_ui::theme::{self, Appearance, Theme};

use egui::{Rect, pos2, vec2};

// ---------------------------------------------------------------------------
// Geometry shared by every render in this file
// ---------------------------------------------------------------------------

/// The rendered surface, in logical points.
const SURFACE: (f32, f32) = (300.0, 220.0);

/// Physical pixels per point. 2x is what a Retina user sees, and it is what
/// makes a one-point rounding error visible instead of rounded away.
const SCALE: f32 = 2.0;

/// Where the card sits. Chosen with generous margins so a shadow has somewhere
/// to fall and a probe just outside the card is still inside the image.
const CARD_ORIGIN: (f32, f32) = (34.0, 34.0);

/// The stack's card size.
const CARD_SIZE: (f32, f32) = (210.0, 150.0);

fn card_rect() -> Rect {
    Rect::from_min_size(
        pos2(CARD_ORIGIN.0, CARD_ORIGIN.1),
        vec2(CARD_SIZE.0, CARD_SIZE.1),
    )
}

/// A logical point converted to a pixel in the rendered image.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn px(point: f32) -> u32 {
    (point * SCALE) as u32
}

/// The alpha of the pixel at a logical point.
fn alpha_at(image: &Image, x: f32, y: f32) -> u8 {
    image.pixel(px(x), px(y))[3]
}

// ---------------------------------------------------------------------------
// The scene under test
// ---------------------------------------------------------------------------

/// One card, drawn at a fixed rectangle, with everything that changes it
/// spelled out as a field.
///
/// No texture is uploaded. That is deliberate: a texture would make the
/// interesting pixels depend on the *contents* of a fixture image, and the
/// question here is about the card's silhouette, not its thumbnail. The
/// no-texture path draws a neutral holding fill with exactly the rounding the
/// real thumbnail would get, which is the geometry the corner probes read.
struct CardScene {
    name: &'static str,
    provenance: Provenance,
    source_px: (u32, u32),
    reveal: f32,
    lift: f32,
    angle: f32,
}

impl CardScene {
    const fn resting(provenance: Provenance) -> Self {
        Self {
            name: "capture-01.png",
            provenance,
            source_px: (1600, 1000),
            reveal: 0.0,
            lift: 0.0,
            angle: 0.0,
        }
    }

    const fn hovered(provenance: Provenance) -> Self {
        Self {
            name: "capture-01.png",
            provenance,
            source_px: (1600, 1000),
            reveal: 1.0,
            lift: 0.0,
            angle: 0.0,
        }
    }

    const fn with_metadata(mut self, name: &'static str, source_px: (u32, u32)) -> Self {
        self.name = name;
        self.source_px = source_px;
        self
    }
}

impl Scene for CardScene {
    fn name(&self) -> &str {
        "overlay-card"
    }

    fn setup(&self, ctx: &egui::Context) {
        theme::install_fonts(ctx);
        theme::install_style(ctx, &Theme::for_appearance(Appearance::Dark));
    }

    fn ui(&self, ui: &mut egui::Ui, ctx: &SceneCtx<'_>) {
        let theme = Theme::for_appearance(Appearance::Dark);
        let icons = IconStore::new(ui.ctx());
        let motion = Motion::at_ms(ctx.millis());
        // `still`, not `new`: the harness has no pointer, and a surface that
        // reads live hover would make these renders depend on where a mouse
        // that does not exist happens to be.
        let surface = Surface::still(&theme, &icons, motion);

        let frame = CardFrame {
            id: CardId(1),
            slot: 0,
            rect: card_rect(),
            alpha: 1.0,
            reveal: self.reveal,
            lift: self.lift,
            angle: self.angle,
            state: CardState::Resting,
        };
        let content = CardContent::new(self.name, self.source_px, self.provenance);

        card::draw_card(ui, &surface, &frame, &content);
    }
}

/// Renders one card state to an image.
fn render(scene: CardScene) -> Image {
    let mut registry = SceneRegistry::empty();
    registry.register(Scenario::StackSingle, Box::new(scene));
    let renderer = SoftwareRenderer::new(registry);

    let mut spec = RenderSpec::golden(Scenario::StackSingle, VirtualClock::ZERO);
    spec.profile = Profile::Golden;
    spec.pixels_per_point = SCALE;
    spec.size_pt = Some(SURFACE);
    spec.theme = egui::Theme::Dark;
    // Transparent, so "is there a shadow here" and "is this corner cut away"
    // are questions about alpha rather than about colour distance from a
    // backdrop.
    spec.background = Background::Transparent;

    renderer.render(&spec).expect("render card")
}

fn preview_rect(provenance: Provenance, source_px: (u32, u32)) -> Rect {
    CardChrome::for_provenance(provenance)
        .geometry(card_rect(), source_px)
        .container
}

// ---------------------------------------------------------------------------
// 1. The card paints at all
// ---------------------------------------------------------------------------

#[test]
fn a_card_paints_something() {
    let image = render(CardScene::resting(Provenance::Display));
    let rect = card_rect();

    let centre = alpha_at(&image, rect.center().x, rect.center().y);
    assert!(
        centre > 200,
        "the middle of a resting card should be opaque, got alpha {centre}"
    );

    let outside = alpha_at(&image, 4.0, 4.0);
    assert_eq!(
        outside, 0,
        "the overlay must be fully transparent away from its content, got alpha {outside}"
    );
}

#[test]
fn hover_changes_the_card() {
    let rest = render(CardScene::resting(Provenance::Display));
    let hover = render(CardScene::hovered(Provenance::Display));

    assert_ne!(
        rest.fingerprint(),
        hover.fingerprint(),
        "revealing the hover chrome must change the pixels; \
         if it does not, the chrome is not being drawn"
    );

    // And specifically: the chrome scrim darkens the capture's middle. Sampled
    // above centre, clear of the pills.
    let rect = card_rect();
    let y = rect.top() + 34.0;
    let before = rest.pixel(px(rect.center().x), px(y));
    let after = hover.pixel(px(rect.center().x), px(y));
    assert_ne!(
        before, after,
        "the hover scrim should darken the capture, but the pixel is unchanged"
    );
}

#[test]
fn card_actions_share_one_flat_neutral_treatment() {
    let hover = render(CardScene::hovered(Provenance::Display));
    let copy_fill = hover.pixel(px(90.0), px(80.0));
    let corner_fill = hover.pixel(px(45.0), px(45.0));

    assert_eq!(
        copy_fill, corner_fill,
        "large and corner actions must use the same background"
    );
    assert!(
        copy_fill[0] > 220
            && copy_fill[0].abs_diff(copy_fill[1]) <= 2
            && copy_fill[1].abs_diff(copy_fill[2]) <= 2,
        "card controls should be flat neutral white, got {copy_fill:?}"
    );
}

#[test]
fn the_probe_points_are_inside_the_rendered_image() {
    // Every D9 assertion below reads a specific pixel. If the render were
    // smaller than the coordinates being probed, `Image::pixel` would hand back
    // zeroes and half of this file would pass by accident.
    let image = render(CardScene::resting(Provenance::Display));
    assert_eq!(image.width(), px(SURFACE.0));
    assert_eq!(image.height(), px(SURFACE.1));

    let rect = card_rect();
    assert!(px(rect.bottom() + 5.0) < image.height());
    assert!(px(rect.right()) < image.width());
}

// ---------------------------------------------------------------------------
// 2. Shared, fixed card geometry
// ---------------------------------------------------------------------------

#[test]
fn every_provenance_gets_the_same_outer_preview_container() {
    for provenance in [
        Provenance::Display,
        Provenance::Window,
        Provenance::Region,
        Provenance::AllDisplays,
        Provenance::Stitched,
    ] {
        let chrome = CardChrome::for_provenance(provenance);
        assert!(chrome.has_preview_container(), "{provenance:?}");
        assert!(chrome.overlays_match_container(), "{provenance:?}");
        assert_eq!(chrome.padding, CardChrome::PADDING, "{provenance:?}");
    }

    let window = CardChrome::for_provenance(Provenance::Window);
    assert_eq!(window.thumb_radius, CardChrome::OUTER_RADIUS);
    assert!(window.capture_border);
}

#[test]
fn every_capture_aspect_uses_the_same_complete_frame() {
    for provenance in [Provenance::Display, Provenance::Window, Provenance::Region] {
        let chrome = CardChrome::for_provenance(provenance);
        for source in [(1600, 1000), (1920, 1080), (1000, 1000), (900, 1600)] {
            let geometry = chrome.geometry(card_rect(), source);
            assert_eq!(geometry.container, card_rect(), "{provenance:?} {source:?}");
            assert_eq!(geometry.capture, card_rect(), "{provenance:?} {source:?}");
            assert!(chrome.is_concentric());
        }
    }
}

#[test]
fn window_and_display_cards_share_the_outer_curve() {
    for provenance in [Provenance::Window, Provenance::Display] {
        let image = render(CardScene::resting(provenance));
        let rect = preview_rect(provenance, (1600, 1000));
        let corner = alpha_at(&image, rect.left() + 1.0, rect.top() + 1.0);
        assert!(
            corner < 64,
            "{provenance:?} preview has a filled outer corner (alpha {corner})"
        );
    }
}

#[test]
fn the_hover_scrim_does_not_square_the_container() {
    let rest = render(CardScene::resting(Provenance::Display));
    let hover = render(CardScene::hovered(Provenance::Display));
    let rect = preview_rect(Provenance::Display, (1600, 1000));

    for (name, x, y) in [
        ("bottom-left", rect.left() + 1.0, rect.bottom() - 1.0),
        ("bottom-right", rect.right() - 1.0, rect.bottom() - 1.0),
        ("top-left", rect.left() + 1.0, rect.top() + 1.0),
        ("top-right", rect.right() - 1.0, rect.top() + 1.0),
    ] {
        let at_rest = alpha_at(&rest, x, y);
        let revealed = alpha_at(&hover, x, y);
        assert!(at_rest < 64, "{name} is filled at rest (alpha {at_rest})");
        assert!(
            revealed < 64,
            "{name} was squared by hover (rest {at_rest}, hover {revealed})"
        );
    }
}

#[test]
fn resting_pixels_do_not_change_with_name_or_resolution() {
    let first =
        render(CardScene::resting(Provenance::Display).with_metadata("first.png", (1600, 1000)));
    let second = render(
        CardScene::resting(Provenance::Display)
            .with_metadata("a completely different name.png", (3840, 2160)),
    );
    assert_eq!(
        first.fingerprint(),
        second.fingerprint(),
        "idle cards must paint the thumbnail only, never name or resolution"
    );
}

#[test]
fn every_preview_container_casts_the_stack_shadow() {
    for provenance in [Provenance::Window, Provenance::Display] {
        let image = render(CardScene::resting(provenance));
        let rect = preview_rect(provenance, (1600, 1000));
        let shadow = alpha_at(&image, rect.center().x, rect.bottom() + 5.0);
        assert!(
            shadow > 0,
            "{provenance:?} preview should cast the shared stack shadow"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. Click-through
// ---------------------------------------------------------------------------

#[test]
fn clicks_pass_through_the_gaps_between_cards() {
    let a = Rect::from_min_size(pos2(10.0, 10.0), vec2(100.0, 60.0));
    let b = Rect::from_min_size(pos2(10.0, 110.0), vec2(100.0, 60.0));
    let hits = [a, b];

    assert!(!overlay_app::passes_through(Some(a.center()), &hits));
    assert!(!overlay_app::passes_through(Some(b.center()), &hits));
    // The gap between them is desktop, and must stay clickable.
    assert!(overlay_app::passes_through(Some(pos2(60.0, 90.0)), &hits));
    // So is everything outside.
    assert!(overlay_app::passes_through(Some(pos2(400.0, 400.0)), &hits));
}

#[test]
fn an_unknown_pointer_never_passes_through() {
    let hits = [Rect::from_min_size(pos2(10.0, 10.0), vec2(100.0, 60.0))];
    assert!(
        !overlay_app::passes_through(None, &hits),
        "with no pointer position the overlay must keep its clicks; the \
         opposite is unrecoverable, because a window that ignores mouse events \
         can never learn the pointer came back"
    );
}

#[test]
fn an_empty_overlay_is_entirely_click_through() {
    assert!(overlay_app::passes_through(Some(pos2(5.0, 5.0)), &[]));
    assert!(overlay_app::passes_through(None, &[]));
}

// ---------------------------------------------------------------------------
// 4. The window itself
// ---------------------------------------------------------------------------

#[test]
fn the_viewport_is_a_borderless_transparent_always_on_top_panel() {
    let work_area = Rect::from_min_size(pos2(0.0, 25.0), vec2(1440.0, 875.0));
    let geometry = OverlayGeometry::new(work_area);
    let builder = overlay_app::viewport(geometry);

    assert_eq!(builder.decorations, Some(false), "borderless");
    assert_eq!(builder.transparent, Some(true), "transparent");
    assert_eq!(builder.has_shadow, Some(false), "no window shadow");
    assert_eq!(builder.taskbar, Some(false), "no Dock or taskbar entry");
    assert_eq!(builder.resizable, Some(false));
    assert_eq!(
        builder.drag_and_drop,
        Some(false),
        "the Windows source overlay must not register as its own inbound drop target"
    );
    assert_eq!(
        builder.active,
        Some(false),
        "the overlay must never take focus when it appears"
    );
    assert_eq!(
        builder.window_level,
        Some(egui::WindowLevel::AlwaysOnTop),
        "always on top"
    );

    // Anchored to the work area, not the display bounds: the difference is
    // whether slot 0 sits above the Dock or behind it.
    assert_eq!(builder.position, Some(work_area.min));
    assert_eq!(builder.inner_size, Some(work_area.size()));
}

#[test]
fn the_overlay_covers_the_work_area_in_local_coordinates() {
    let work_area = Rect::from_min_size(pos2(120.0, 25.0), vec2(1200.0, 800.0));
    let geometry = OverlayGeometry::new(work_area);

    assert_eq!(geometry.position(), pos2(120.0, 25.0));
    assert_eq!(geometry.size(), vec2(1200.0, 800.0));
    assert_eq!(
        geometry.local(),
        Rect::from_min_size(pos2(0.0, 0.0), vec2(1200.0, 800.0)),
        "the stack lays out in window-local points, so the origin is zero"
    );
}

#[test]
fn native_options_carry_the_viewport_and_never_persist_geometry() {
    let geometry = OverlayGeometry::new(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 600.0)));
    let options = overlay_app::native_options(geometry);

    assert_eq!(options.viewport.decorations, Some(false));
    assert!(
        !options.persist_window,
        "a restored window position would drop the overlay somewhere other \
         than the corner of the work area"
    );
}

// ---------------------------------------------------------------------------
// 5. The handle the app drives
// ---------------------------------------------------------------------------

#[test]
fn the_handle_accepts_captures_before_the_window_exists() {
    let handle = OverlayHandle::new();
    handle.push(scrozz_ui::overlay_app::CaptureRequest::new(
        "early.png",
        Provenance::Display,
        (1920, 1080),
    ));
    assert!(
        !handle.is_attached(),
        "no app has taken this handle yet, and pushing still has to work: the \
         hotkey is live before the first frame"
    );
    // Draining before attachment yields nothing, and must not panic or block.
    assert!(handle.drain_events().is_empty());
}

#[test]
fn overlay_options_default_to_recoverable_click_through() {
    let options = OverlayOptions::default();
    assert_eq!(
        options.passthrough,
        Passthrough::Auto,
        "click-through is on by default, because an overlay that eats clicks \
         on empty desktop is worse than no overlay"
    );
    assert!(
        options.probe.is_none(),
        "no pointer probe unless the app supplies one"
    );
    assert!(options.panel.is_none(), "no panel hook unless supplied");
}
