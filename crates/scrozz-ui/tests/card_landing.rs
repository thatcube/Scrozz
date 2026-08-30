//! The landing glow: its clock, its schedule, and what it actually paints.
//!
//! The treatment is decoration, and decoration is exactly the kind of thing
//! that quietly becomes a battery leak or an accessibility failure. So the
//! contracts under test here are mostly about *not* running:
//!
//! * it starts only once a card's entry motion has settled, and never for a
//!   card the stack was seeded with;
//! * it finishes — completely — inside its own window, and asks for no frames
//!   afterwards, so an idle or hidden overlay is genuinely idle;
//! * reduce-motion and an open editor each switch it off entirely, in the
//!   painter and in the frame schedule alike.
//!
//! Everything renders through [`scrozz_ui::harness`]: pure CPU, virtual clock,
//! no window, no GPU. Nothing here opens anything on screen. The renders are
//! against a *transparent* background, which is what the real capture overlay
//! is, so "did the halo reach out here" is a question about alpha rather than
//! about colour distance from an invented backdrop.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use egui::{Rect, pos2, vec2};
use scrozz_core::Provenance;
use scrozz_ui::card::{self, CardContent, CardMedia, glow};
use scrozz_ui::harness::{
    Background, Image, Profile, RenderSpec, Scenario, Scene, SceneCtx, SceneRegistry,
    SoftwareRenderer, VirtualClock,
};
use scrozz_ui::icons::IconStore;
use scrozz_ui::motion::Motion;
use scrozz_ui::paint::Surface;
use scrozz_ui::stack::{CaptureStack, CardFrame, CardId, CardState, Timing};
use scrozz_ui::theme::{self, Appearance, Theme};

// ---------------------------------------------------------------------------
// Rendering one card, at one instant, in one appearance
// ---------------------------------------------------------------------------

/// Big enough that the halo's full outward reach lands inside the image.
const SURFACE: (f32, f32) = (420.0, 340.0);
const SCALE: f32 = 2.0;
const CARD_ORIGIN: (f32, f32) = (105.0, 95.0);
const CARD_SIZE: (f32, f32) = (210.0, 150.0);

fn card_rect() -> Rect {
    Rect::from_min_size(
        pos2(CARD_ORIGIN.0, CARD_ORIGIN.1),
        vec2(CARD_SIZE.0, CARD_SIZE.1),
    )
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn px(point: f32) -> u32 {
    (point * SCALE) as u32
}

fn alpha_at(image: &Image, x: f32, y: f32) -> u8 {
    image.pixel(px(x), px(y))[3]
}

/// One card at one instant of its landing, with everything that can switch the
/// treatment off spelled out as a field.
#[derive(Clone, Copy)]
struct LandingScene {
    landed: Option<f32>,
    editing: bool,
    reduce_motion: bool,
    appearance: Appearance,
}

impl LandingScene {
    const fn at(landed: f32, appearance: Appearance) -> Self {
        Self {
            landed: Some(landed),
            editing: false,
            reduce_motion: false,
            appearance,
        }
    }

    const fn never_landed(appearance: Appearance) -> Self {
        Self {
            landed: None,
            editing: false,
            reduce_motion: false,
            appearance,
        }
    }

    const fn editing(mut self) -> Self {
        self.editing = true;
        self
    }

    const fn calm(mut self) -> Self {
        self.reduce_motion = true;
        self
    }
}

impl Scene for LandingScene {
    fn name(&self) -> &str {
        "card-landing"
    }

    fn setup(&self, ctx: &egui::Context) {
        theme::install_fonts(ctx);
        theme::install_style(ctx, &Theme::for_appearance(self.appearance));
    }

    fn ui(&self, ui: &mut egui::Ui, _ctx: &SceneCtx<'_>) {
        let theme = Theme::for_appearance(self.appearance);
        let icons = IconStore::new(ui.ctx());
        // The glow's own clock is `CardFrame::landed`, not the surface's
        // instant, so this stays fixed: every difference between two renders
        // in this file is the landing time and nothing else.
        let motion = Motion::at(0.0).with_reduce_motion(self.reduce_motion);
        let surface = Surface::still(&theme, &icons, motion);

        let frame = CardFrame {
            id: CardId(1),
            slot: 0,
            rect: card_rect(),
            alpha: 1.0,
            reveal: 0.0,
            lift: 0.0,
            angle: 0.0,
            state: CardState::Resting,
            landed: self.landed,
        };
        let mut content = CardContent::new("capture-01.png", (1600, 1000), Provenance::Display)
            .with_media(CardMedia::Image);
        content.editing = self.editing;

        card::draw_card(ui, &surface, &frame, &content);
    }
}

fn render(scene: LandingScene) -> Image {
    let mut registry = SceneRegistry::empty();
    registry.register(Scenario::StackSingle, Box::new(scene));
    let renderer = SoftwareRenderer::new(registry);

    let mut spec = RenderSpec::golden(Scenario::StackSingle, VirtualClock::ZERO);
    spec.profile = Profile::Golden;
    spec.pixels_per_point = SCALE;
    spec.size_pt = Some(SURFACE);
    spec.theme = match scene.appearance {
        Appearance::Dark => egui::Theme::Dark,
        Appearance::Light => egui::Theme::Light,
    };
    // What the capture overlay actually is.
    spec.background = Background::Transparent;

    renderer.render(&spec).expect("render a landing card")
}

/// Total alpha in a ring of probes just outside the card, where only the halo
/// can put anything.
fn halo_alpha(image: &Image, out: f32) -> u32 {
    let card = card_rect();
    let probes = [
        (card.center().x, card.top() - out),
        (card.center().x, card.bottom() + out),
        (card.left() - out, card.center().y),
        (card.right() + out, card.center().y),
    ];
    probes
        .into_iter()
        .map(|(x, y)| u32::from(alpha_at(image, x, y)))
        .sum()
}

// ---------------------------------------------------------------------------
// 1. The timeline
// ---------------------------------------------------------------------------

#[test]
fn nothing_is_drawn_before_a_card_has_landed() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        let never = render(LandingScene::never_landed(appearance));
        let settled = render(LandingScene::at(glow::GLOW_WINDOW, appearance));
        assert_eq!(
            never.fingerprint(),
            settled.fingerprint(),
            "{appearance:?}: a card that never landed must look exactly like one \
             whose landing is over"
        );
    }
}

#[test]
fn the_halo_reaches_outside_the_card_while_the_glow_runs() {
    // Mid-window, once the rim has ignited and before it fades.
    let lit = render(LandingScene::at(2.0, Appearance::Dark));
    let dark = render(LandingScene::never_landed(Appearance::Dark));

    // Against the unlit card, not against zero: the card's own drop shadow
    // already puts a little alpha out here, and the question is whether the
    // rim adds to it.
    assert!(
        halo_alpha(&lit, 20.0) > halo_alpha(&dark, 20.0),
        "the rim must actually reach past the card's edge: lit {}, unlit {}",
        halo_alpha(&lit, 20.0),
        halo_alpha(&dark, 20.0)
    );
    assert_ne!(
        lit.fingerprint(),
        dark.fingerprint(),
        "a lit card must not render identically to an unlit one"
    );
}

#[test]
fn the_glow_finishes_completely_inside_its_own_window() {
    let dark = render(LandingScene::never_landed(Appearance::Dark));
    // Sampled right up to the last instant the treatment is allowed to draw.
    for landed in [
        glow::GLOW_WINDOW - 0.01,
        glow::GLOW_WINDOW,
        glow::GLOW_WINDOW + 5.0,
        600.0,
    ] {
        let late = render(LandingScene::at(landed, Appearance::Dark));
        if landed < glow::GLOW_WINDOW {
            // The very last frame is allowed to be dim, but it must be dim
            // *everywhere*: nothing may still be reaching out past the card.
            assert_eq!(
                halo_alpha(&late, 30.0),
                0,
                "at {landed}s the halo should have faded to nothing"
            );
        } else {
            assert_eq!(
                late.fingerprint(),
                dark.fingerprint(),
                "at {landed}s the treatment must be over, pixel for pixel"
            );
        }
    }
}

#[test]
fn the_treatment_moves_through_its_window() {
    // Not a golden: what matters is that consecutive instants differ, which is
    // what "animated" means and what a frozen or never-started effect fails.
    let frames: Vec<_> = [0.2, 0.8, 1.4, 2.5, 4.0]
        .into_iter()
        .map(|s| render(LandingScene::at(s, Appearance::Dark)).fingerprint())
        .collect();
    for window in frames.windows(2) {
        assert_ne!(
            window[0], window[1],
            "the glow must change between instants"
        );
    }
}

// ---------------------------------------------------------------------------
// 2. When it must not run at all
// ---------------------------------------------------------------------------

#[test]
fn reduce_motion_removes_the_landing_glow_entirely() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        let calm = render(LandingScene::at(2.0, appearance).calm());
        let none = render(LandingScene::never_landed(appearance).calm());
        assert_eq!(
            calm.fingerprint(),
            none.fingerprint(),
            "{appearance:?}: D13 — under reduce-motion there is no landing \
             treatment to see"
        );
    }
}

#[test]
fn an_editing_card_never_lights_up() {
    // The editing card has exactly one thing to say about itself, and the pill
    // is saying it. A glow underneath would be a second announcement on a card
    // that is not, in fact, newly arrived any more.
    let lit = render(LandingScene::at(2.0, Appearance::Dark).editing());
    let unlit = render(LandingScene::never_landed(Appearance::Dark).editing());
    assert_eq!(lit.fingerprint(), unlit.fingerprint());
}

#[test]
fn is_active_is_the_one_predicate_both_halves_read() {
    assert!(!glow::is_active(None, false, false));
    assert!(glow::is_active(Some(0.0), false, false));
    assert!(glow::is_active(
        Some(glow::GLOW_WINDOW - 0.001),
        false,
        false
    ));
    assert!(!glow::is_active(Some(glow::GLOW_WINDOW), false, false));
    assert!(!glow::is_active(Some(-0.5), false, false));
    assert!(!glow::is_active(Some(1.0), true, false), "editing");
    assert!(!glow::is_active(Some(1.0), false, true), "reduce-motion");
}

// ---------------------------------------------------------------------------
// 3. Both appearances
// ---------------------------------------------------------------------------

#[test]
fn the_glow_renders_in_both_appearances_and_is_damped_on_dark() {
    let dark = render(LandingScene::at(2.0, Appearance::Dark));
    let light = render(LandingScene::at(2.0, Appearance::Light));

    // Measured as the *gain* over the same card with no landing, so each
    // appearance is compared against its own drop shadow rather than against
    // the other's.
    let gain = |lit: &Image, unlit: &Image| {
        i64::from(halo_alpha(lit, 16.0)) - i64::from(halo_alpha(unlit, 16.0))
    };
    let dark_gain = gain(&dark, &render(LandingScene::never_landed(Appearance::Dark)));
    let light_gain = gain(
        &light,
        &render(LandingScene::never_landed(Appearance::Light)),
    );

    assert!(dark_gain > 0, "the rim must be visible on a dark overlay");
    assert!(light_gain > 0, "the rim must be visible on a light overlay");
    assert!(
        dark_gain < light_gain,
        "added light reads harder against a dark surface, so the dark \
         appearance takes less of it: dark {dark_gain}, light {light_gain}"
    );
}

// ---------------------------------------------------------------------------
// 4. Colour comes from the capture
// ---------------------------------------------------------------------------

fn image_of(pixels: impl Fn(usize, usize) -> egui::Color32) -> egui::ColorImage {
    let (w, h) = (32usize, 32usize);
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            out.push(pixels(x, y));
        }
    }
    egui::ColorImage {
        size: [w, h],
        source_size: egui::Vec2::new(w as f32, h as f32),
        pixels: out,
    }
}

#[test]
fn a_low_saturation_capture_lights_its_rim_white() {
    let grey = glow::sample_accent(&image_of(|x, _| {
        let v = 96 + u8::try_from(x).unwrap_or(0) * 2;
        egui::Color32::from_rgb(v, v, v)
    }));
    let colourful = glow::sample_accent(&image_of(|x, y| {
        egui::Color32::from_rgb(
            u8::try_from(x * 8).unwrap_or(255),
            u8::try_from(y * 8).unwrap_or(255),
            200,
        )
    }));
    assert!(
        grey.strength() < 0.05,
        "a grey screenshot has no hue to lend, got {}",
        grey.strength()
    );
    assert!(
        colourful.strength() > grey.strength(),
        "a colourful capture must report more colour than a grey one"
    );
}

// ---------------------------------------------------------------------------
// 5. The frame schedule
// ---------------------------------------------------------------------------

fn work_area() -> Rect {
    Rect::from_min_size(pos2(0.0, 37.0), vec2(1728.0, 1022.0))
}

fn never_suppressed(_: CardId) -> bool {
    false
}

#[test]
fn landing_starts_when_the_entry_animation_settles() {
    let mut stack = CaptureStack::for_work_area(work_area());
    let id = stack.push(&Motion::at(0.0));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let entry_ms = (Timing::default().enter.designed_secs() * 1000.0) as u64;

    let mid = stack.frame_of(id, &Motion::at_ms(entry_ms / 2)).unwrap();
    assert_eq!(
        mid.landed, None,
        "a card still sliding in has not landed yet"
    );

    let just_after = stack
        .frame_of(id, &Motion::at_ms(entry_ms + 10))
        .expect("still resident");
    let landed = just_after.landed.expect("the card has landed");
    assert!(
        (landed - 0.010).abs() < 0.002,
        "the clock starts at the settle instant, got {landed}"
    );
}

#[test]
fn a_seeded_card_never_announces_itself() {
    // Whatever was already there when the overlay opened did not just arrive.
    let mut stack = CaptureStack::for_work_area(work_area());
    let id = stack.push_settled(&Motion::at(0.0));
    for ms in [0, 100, 1_000, 10_000] {
        let frame = stack.frame_of(id, &Motion::at_ms(ms)).expect("resident");
        assert_eq!(frame.landed, None, "at {ms}ms");
        assert_eq!(frame.state, CardState::Resting, "at {ms}ms");
        assert!(
            (frame.alpha - 1.0).abs() < 1e-3,
            "seeded cards are not faded in"
        );
    }
    assert!(
        stack
            .glow_activity(&Motion::at(0.1), never_suppressed)
            .is_idle()
    );
}

#[test]
fn the_glow_asks_for_frames_only_while_it_is_running() {
    let mut stack = CaptureStack::for_work_area(work_area());
    stack.push(&Motion::at(0.0));
    let settle = Timing::default().enter.designed_secs();

    let during = Motion::at(f64::from(settle) + 1.0);
    assert!(
        stack
            .glow_activity(&during, never_suppressed)
            .is_animating(),
        "a glowing card needs frames"
    );

    let after = Motion::at(f64::from(settle) + f64::from(glow::GLOW_WINDOW) + 0.01);
    assert!(
        stack.glow_activity(&after, never_suppressed).is_idle(),
        "once the window is over the overlay must be allowed to sleep — this \
         is the difference between an animation and a 60 Hz idle leak"
    );
}

#[test]
fn a_hidden_or_empty_overlay_schedules_nothing() {
    let stack = CaptureStack::for_work_area(work_area());
    for seconds in [0.0, 1.0, 60.0] {
        assert!(
            stack
                .glow_activity(&Motion::at(seconds), never_suppressed)
                .is_idle(),
            "an overlay with no cards has nothing to animate at {seconds}s"
        );
    }
}

#[test]
fn reduce_motion_and_editing_each_take_the_glow_off_the_schedule() {
    let mut stack = CaptureStack::for_work_area(work_area());
    let id = stack.push(&Motion::at(0.0));
    let settle = f64::from(Timing::default().enter.designed_secs());
    let during = Motion::at(settle + 1.0);

    assert!(
        stack
            .glow_activity(&during, never_suppressed)
            .is_animating()
    );
    assert!(
        stack
            .glow_activity(&during.with_reduce_motion(true), never_suppressed)
            .is_idle(),
        "D13"
    );
    assert!(
        stack.glow_activity(&during, |card| card == id).is_idle(),
        "a card whose editor is open draws nothing, so it schedules nothing"
    );
}

#[test]
fn a_dismissed_card_takes_its_glow_off_the_schedule() {
    let mut stack = CaptureStack::for_work_area(work_area());
    let id = stack.push(&Motion::at(0.0));
    let settle = f64::from(Timing::default().enter.designed_secs());
    let during = Motion::at(settle + 0.5);
    assert!(
        stack
            .glow_activity(&during, never_suppressed)
            .is_animating()
    );

    assert!(stack.dismiss(id, &during));
    stack.advance(&Motion::at(settle + 30.0));
    assert!(
        stack
            .glow_activity(&Motion::at(settle + 30.0), never_suppressed)
            .is_idle(),
        "a card that has left cannot keep the overlay awake"
    );
    assert!(stack.frame_of(id, &Motion::at(settle + 30.0)).is_none());
}

/// Writes one PNG per instant so a person can look at the treatment.
///
/// Not an assertion, and never run by default. This is how the glow is
/// reviewed on the real transparent overlay in both appearances without
/// opening a window:
///
/// ```text
/// SCROZZ_GLOW_SHEET=/tmp/glow cargo test -p scrozz-ui --test card_landing ///     -- --ignored contact_sheet
/// ```
#[test]
#[ignore = "writes a contact sheet for a human to look at; not an assertion"]
fn contact_sheet() {
    let dir = std::env::var("SCROZZ_GLOW_SHEET")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("scrozz-glow"));
    std::fs::create_dir_all(&dir).expect("a place to write the sheet");
    for appearance in [Appearance::Dark, Appearance::Light] {
        for (i, s) in [0.0_f32, 0.5, 0.9, 1.1, 1.6, 2.4, 3.6, 5.0, 6.2]
            .into_iter()
            .enumerate()
        {
            let image = render(LandingScene::at(s, appearance));
            image
                .write_png(&dir.join(format!("glow-{appearance:?}-{i}-{s}.png")))
                .unwrap();
        }
    }
}
