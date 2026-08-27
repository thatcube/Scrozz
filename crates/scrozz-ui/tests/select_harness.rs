//! Selector-focused regression tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use scrozz_ui::harness::{RenderSpec, Scenario, SceneRegistry, SoftwareRenderer, VirtualClock};

#[test]
fn selector_scenarios_are_registered_as_real_scenes() {
    let registry = SceneRegistry::production();
    for scenario in [
        Scenario::SelectorIdle,
        Scenario::SelectorDragging,
        Scenario::SelectorRemembered,
        Scenario::SelectorExact,
        Scenario::SelectorAspect,
        Scenario::SelectorMagnifier,
        Scenario::SelectorAllInOne,
        Scenario::SelectorMixedDpi,
    ] {
        let scene = registry.scene(scenario).unwrap();
        assert!(
            !scene.is_placeholder(),
            "{} should be real",
            scenario.slug()
        );
    }
}

#[test]
fn selector_scenes_render_non_empty_and_distinct_images() {
    let renderer = SoftwareRenderer::production();
    let idle = renderer
        .render(&RenderSpec::golden(
            Scenario::SelectorIdle,
            VirtualClock::ZERO,
        ))
        .unwrap();
    let dragging = renderer
        .render(&RenderSpec::golden(
            Scenario::SelectorDragging,
            VirtualClock::ZERO,
        ))
        .unwrap();
    let hud = renderer
        .render(&RenderSpec::golden(
            Scenario::SelectorAllInOne,
            VirtualClock::ZERO,
        ))
        .unwrap();

    assert!(idle.width() > 0 && idle.height() > 0);
    assert_ne!(idle.fingerprint(), dragging.fingerprint());
    assert_ne!(dragging.fingerprint(), hud.fingerprint());
}
