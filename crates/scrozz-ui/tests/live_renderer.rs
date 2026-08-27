//! Integration coverage for Scrozz's persistent CPU-rendered native surface.

use scrozz_core::Provenance;
use scrozz_ui::harness::{LiveRenderInput, LiveRenderer};
use scrozz_ui::{
    CaptureRequest, OverlayApp, OverlayHandle, OverlayOptions, PanelReport, layer_shell_geometry,
};

fn input(width: u32, height: u32, time: f64) -> LiveRenderInput {
    LiveRenderInput {
        width_px: width,
        height_px: height,
        pixels_per_point: 1.0,
        logical_size: egui::vec2(width as f32, height as f32),
        time,
        predicted_dt: 1.0 / 60.0,
        events: Vec::new(),
    }
}

fn scaled_input(width: u32, height: u32, scale: f32, time: f64) -> LiveRenderInput {
    LiveRenderInput {
        width_px: (width as f32 * scale) as u32,
        height_px: (height as f32 * scale) as u32,
        pixels_per_point: scale,
        logical_size: egui::vec2(width as f32, height as f32),
        time,
        predicted_dt: 1.0 / 60.0,
        events: Vec::new(),
    }
}

#[test]
fn live_renderer_returns_premultiplied_transparent_pixels() {
    let mut renderer = LiveRenderer::new(egui::Theme::Dark);
    let image = renderer
        .render(input(8, 8, 0.0), |ui| {
            ui.painter().rect_filled(
                egui::Rect::from_min_size(egui::pos2(2.0, 2.0), egui::vec2(4.0, 4.0)),
                0.0,
                egui::Color32::from_rgba_unmultiplied(200, 100, 50, 128),
            );
        })
        .expect("software frame");

    assert_eq!((image.width(), image.height()), (8, 8));
    assert_eq!(&image.as_rgba()[0..4], &[0, 0, 0, 0]);
    let center = ((3 * image.width() + 4) * 4) as usize;
    let pixel = &image.as_rgba()[center..center + 4];
    assert!((127..=129).contains(&pixel[3]), "{pixel:?}");
    assert!(pixel[0] <= 101 && pixel[1] <= 51 && pixel[2] <= 26);
}

#[test]
fn owned_surface_renders_the_real_capture_card_scene() {
    let geometry = layer_shell_geometry();
    assert_eq!(geometry.size(), egui::vec2(264.0, 962.0));
    let width = geometry.size().x as u32;
    let height = geometry.size().y as u32;
    let handle = OverlayHandle::new();
    handle.push(CaptureRequest::new(
        "Capture.png",
        Provenance::Display,
        (1920, 1080),
    ));

    let mut renderer = LiveRenderer::new(egui::Theme::Dark);
    let ctx = renderer.context().clone();
    let mut overlay = OverlayApp::new_owned(
        &ctx,
        handle,
        OverlayOptions {
            geometry,
            ..Default::default()
        },
        PanelReport::converted("owned layer-shell surface"),
    );
    let mut state = None;
    renderer
        .render(input(width, height, 0.25), |ui| {
            state = Some(overlay.render_frame(ui));
        })
        .expect("capture stack frame");
    let image = renderer
        .render(input(width, height, 0.75), |ui| {
            state = Some(overlay.render_frame(ui));
        })
        .expect("settled capture stack frame");
    let state = state.expect("overlay frame state");

    assert!(state.visible);
    assert_eq!(state.hit_regions.len(), 1);
    assert!(
        image
            .as_rgba()
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] != 0)
    );
}

#[test]
fn native_scale_changes_preserve_the_compositor_logical_extent() {
    let mut renderer = LiveRenderer::new(egui::Theme::Dark);
    renderer
        .render(input(8, 8, 0.0), |_| {})
        .expect("initial one-scale frame");

    let mut logical_extent = None;
    let image = renderer
        .render(scaled_input(8, 8, 2.0, 0.1), |ui| {
            logical_extent = Some(ui.max_rect().size());
            ui.painter().rect_filled(
                egui::Rect::from_min_size(egui::pos2(1.0, 6.0), egui::vec2(6.0, 2.0)),
                0.0,
                egui::Color32::WHITE,
            );
        })
        .expect("scaled frame");

    assert_eq!(logical_extent, Some(egui::vec2(8.0, 8.0)));
    let bottom = ((14 * image.width() + 8) * 4 + 3) as usize;
    assert_ne!(image.as_rgba()[bottom], 0, "bottom content was clipped");
}
