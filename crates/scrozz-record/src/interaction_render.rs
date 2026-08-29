//! Deterministic rendering for recording interaction overlays.

use std::time::Duration;

use scrozz_annotate::font;
use scrozz_core::{
    ColorSpace, Frame, LogicalPoint, PhysicalPoint, PhysicalSize, PixelFormat, ScaleFactor,
};
use scrozz_export::RgbaImage;
use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect, Stroke, Transform};

use crate::{
    OverlayLayer, OverlaySource,
    interaction::{
        CapturedKeystroke, InputMonitor, InteractionEdits, InteractionEvent, InteractionMapper,
        InteractionRecording, RetainedInteraction,
    },
    overlay::{ANIMATED_CLICK_LIFETIME, STATIC_CLICK_LIFETIME},
    settings::{ClickStyle, OverlayAnchor, OverlayTheme, Rgba8},
};

const CURSOR_WIDTH: u32 = 24;
const CURSOR_HEIGHT: u32 = 32;
const CURSOR_HOTSPOT_X: f64 = 2.0;
const CURSOR_HOTSPOT_Y: f64 = 2.0;

pub(crate) struct InteractionOverlaySource {
    monitor: Box<dyn InputMonitor>,
    mapper: InteractionMapper,
    recording: InteractionRecording,
    retain_for_editing: bool,
    overflow_reported: bool,
    archive_reported: bool,
}

impl InteractionOverlaySource {
    pub(crate) fn new(
        monitor: Box<dyn InputMonitor>,
        mapper: InteractionMapper,
        recording: InteractionRecording,
        retain_for_editing: bool,
    ) -> Self {
        Self {
            monitor,
            mapper,
            recording,
            retain_for_editing,
            overflow_reported: false,
            archive_reported: false,
        }
    }

    fn collect(&mut self, elapsed: Duration) {
        self.monitor.sync_media_time(elapsed);
        for event in self.monitor.drain() {
            match event {
                InteractionEvent::Click {
                    at,
                    position,
                    button,
                } => {
                    if let Some(position) = self.mapper.normalize(position) {
                        self.recording.push(RetainedInteraction::Click {
                            at,
                            position,
                            button,
                        });
                    }
                }
                InteractionEvent::Keystroke { at, key } => {
                    self.recording
                        .push(RetainedInteraction::Keystroke { at, key });
                }
            }
        }
        if self.recording.cursor_smoothing
            && let Some(position) = self.monitor.cursor_position()
            && let Some(position) = self.mapper.normalize(position)
        {
            self.recording.push(RetainedInteraction::Cursor {
                at: elapsed,
                position,
            });
        }
    }
}

impl OverlaySource for InteractionOverlaySource {
    fn layers(&mut self, elapsed: Duration, canvas: PhysicalSize) -> Vec<OverlayLayer> {
        self.recording.set_reference_size(canvas);
        self.collect(elapsed);
        render_layers(
            &self.recording,
            elapsed,
            canvas,
            self.recording.default_edits(),
        )
    }

    fn pause(&mut self) {
        self.monitor.pause();
    }

    fn resume(&mut self) {
        self.monitor.resume();
    }

    fn update_source_bounds(
        &mut self,
        bounds: scrozz_core::LogicalRect,
    ) -> scrozz_core::Result<()> {
        self.mapper = InteractionMapper::new(bounds)?;
        Ok(())
    }

    fn take_warning(&mut self) -> Option<String> {
        if let Some(warning) = self.monitor.take_warning() {
            return Some(warning);
        }
        if !self.overflow_reported {
            let dropped = self.monitor.take_dropped();
            if dropped > 0 {
                self.overflow_reported = true;
                return Some(format!(
                    "interaction capture queue overflowed; {dropped} input events were not rendered"
                ));
            }
        }
        if self.recording.is_truncated() && !self.archive_reported {
            self.archive_reported = true;
            return Some(
                "interaction timeline reached its memory limit; later events cannot be edited"
                    .to_owned(),
            );
        }
        None
    }

    fn retains_interactions(&self) -> bool {
        self.retain_for_editing
    }

    fn composites_cursor(&self) -> bool {
        self.recording.cursor_visible && self.recording.cursor_smoothing
    }

    fn finish(&mut self) -> Option<InteractionRecording> {
        self.retain_for_editing.then(|| self.recording.clone())
    }
}

pub(crate) fn render_layers(
    recording: &InteractionRecording,
    at: Duration,
    canvas: PhysicalSize,
    edits: InteractionEdits,
) -> Vec<OverlayLayer> {
    if !canvas.width.is_finite()
        || !canvas.height.is_finite()
        || canvas.width < 1.0
        || canvas.height < 1.0
    {
        return Vec::new();
    }
    let mut layers = Vec::new();
    let visual_scale = recording.visual_scale(canvas);
    if edits.clicks {
        layers.extend(click_layers(recording, at, canvas, visual_scale));
    }
    if edits.keystrokes
        && let Some(layer) = keystroke_layer(recording, at, canvas)
    {
        layers.push(layer);
    }
    if edits.cursor
        && let Some(position) = recording.cursor_at(at, canvas, edits.smooth_cursor)
    {
        layers.push(cursor_layer(position, visual_scale));
    }
    layers
}

pub(crate) fn composite_interactions(
    image: &mut RgbaImage,
    recording: &InteractionRecording,
    at: Duration,
    edits: InteractionEdits,
) {
    let canvas = PhysicalSize::new(f64::from(image.width), f64::from(image.height));
    for layer in render_layers(recording, at, canvas, edits) {
        blend_rgba(image, &layer);
    }
}

fn click_layers(
    recording: &InteractionRecording,
    at: Duration,
    canvas: PhysicalSize,
    visual_scale: f32,
) -> Vec<OverlayLayer> {
    let lifetime = if recording.clicks.animate {
        ANIMATED_CLICK_LIFETIME
    } else {
        STATIC_CLICK_LIFETIME
    };
    recording
        .clicks_at(at.saturating_sub(lifetime), at, canvas)
        .into_iter()
        .filter_map(|(click_at, position, _button)| {
            let age = at.saturating_sub(click_at);
            if age >= lifetime {
                return None;
            }
            let progress = (age.as_secs_f32() / lifetime.as_secs_f32()).clamp(0.0, 1.0);
            let base = 36.0 * recording.clicks.size.scale() * visual_scale;
            let (diameter, opacity) = if recording.clicks.animate {
                (base * (0.7 + progress * 0.8), 1.0 - progress)
            } else {
                (base, 1.0)
            };
            Some(click_layer(
                position,
                diameter,
                opacity,
                recording.clicks.color,
                recording.clicks.style,
            ))
        })
        .collect()
}

fn click_layer(
    position: PhysicalPoint,
    diameter: f32,
    opacity: f32,
    color: Rgba8,
    style: ClickStyle,
) -> OverlayLayer {
    let padding = (diameter / 9.0).max(1.0);
    let edge = (diameter + padding * 2.0).ceil().max(1.0) as u32;
    let mut pixmap = Pixmap::new(edge, edge).expect("bounded click overlay dimensions");
    let circle = PathBuilder::from_circle(edge as f32 / 2.0, edge as f32 / 2.0, diameter / 2.0)
        .expect("positive click radius");
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.a);
    paint.anti_alias = true;
    match style {
        ClickStyle::Filled => {
            pixmap.fill_path(
                &circle,
                &paint,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }
        ClickStyle::Outline => {
            pixmap.stroke_path(
                &circle,
                &paint,
                &Stroke {
                    width: (3.0 * (diameter / 36.0)).max(1.0),
                    line_cap: LineCap::Round,
                    line_join: LineJoin::Round,
                    ..Stroke::default()
                },
                Transform::identity(),
                None,
            );
        }
    }
    OverlayLayer {
        content: frame(pixmap),
        origin: PhysicalPoint::new(
            position.x - f64::from(edge) / 2.0,
            position.y - f64::from(edge) / 2.0,
        ),
        opacity,
        adaptive_contrast: false,
    }
}

fn cursor_layer(position: PhysicalPoint, visual_scale: f32) -> OverlayLayer {
    let width = ((CURSOR_WIDTH as f32 * visual_scale).round() as u32).max(5);
    let height = ((CURSOR_HEIGHT as f32 * visual_scale).round() as u32).max(7);
    let mut pixmap = Pixmap::new(width, height).expect("bounded cursor dimensions are valid");
    let mut path = PathBuilder::new();
    path.move_to(2.0, 1.5);
    path.line_to(2.0, 26.0);
    path.line_to(8.5, 20.0);
    path.line_to(13.0, 30.0);
    path.line_to(18.0, 27.5);
    path.line_to(13.5, 18.0);
    path.line_to(22.0, 17.5);
    path.close();
    let path = path.finish().expect("cursor path has area");
    let mut outline = Paint::default();
    outline.set_color_rgba8(255, 255, 255, 242);
    outline.anti_alias = true;
    let transform = Transform::from_scale(visual_scale, visual_scale);
    pixmap.stroke_path(
        &path,
        &outline,
        &Stroke {
            width: 4.0 * visual_scale,
            line_join: LineJoin::Round,
            ..Stroke::default()
        },
        transform,
        None,
    );
    let mut fill = Paint::default();
    fill.set_color_rgba8(20, 22, 28, 255);
    fill.anti_alias = true;
    pixmap.fill_path(&path, &fill, FillRule::Winding, transform, None);
    OverlayLayer {
        content: frame(pixmap),
        origin: PhysicalPoint::new(
            position.x - CURSOR_HOTSPOT_X * f64::from(visual_scale),
            position.y - CURSOR_HOTSPOT_Y * f64::from(visual_scale),
        ),
        opacity: 1.0,
        adaptive_contrast: false,
    }
}

fn keystroke_layer(
    recording: &InteractionRecording,
    at: Duration,
    canvas: PhysicalSize,
) -> Option<OverlayLayer> {
    let hold = Duration::try_from_secs_f64(f64::from(recording.keystrokes.hold_secs)).ok()?;
    let mut grouped: Vec<(&CapturedKeystroke, usize)> = Vec::new();
    for (event_at, key) in recording.keys_at(at.saturating_sub(hold), at) {
        if at.saturating_sub(event_at) >= hold {
            continue;
        }
        if let Some((previous, count)) = grouped.last_mut()
            && previous.label() == key.label()
            && (key.repeat || previous.repeat)
        {
            *count += 1;
        } else {
            grouped.push((key, 1));
        }
    }
    if grouped.len() > recording.keystrokes.max_visible {
        let remove = grouped.len() - recording.keystrokes.max_visible;
        grouped.drain(..remove);
    }
    if grouped.is_empty() {
        return None;
    }

    let scale = recording.keystrokes.size.scale() * recording.visual_scale(canvas);
    let font_size = 18.0 * f64::from(scale);
    let pad_x = 13.0 * f64::from(scale);
    let pad_y = 9.0 * f64::from(scale);
    let gap = 7.0 * f64::from(scale);
    let widths: Vec<f64> = grouped
        .iter()
        .map(|(key, count)| {
            let suffix = if *count > 1 {
                font::measure(&format!(" x{count}"), font_size).width
            } else {
                0.0
            };
            font::measure(key.label(), font_size).width + suffix + pad_x * 2.0
        })
        .collect();
    let height = font_size + pad_y * 2.0;
    let width = widths.iter().sum::<f64>() + gap * (grouped.len().saturating_sub(1) as f64);
    let width_u32 = width.ceil().clamp(1.0, canvas.width) as u32;
    let height_u32 = height.ceil().clamp(1.0, canvas.height) as u32;
    let mut pixmap = Pixmap::new(width_u32, height_u32)?;
    let (chrome, text, border) = theme_colors(recording.keystrokes.theme);
    let mut x = 0.0_f64;
    for ((key, count), chip_width) in grouped.iter().zip(widths) {
        draw_chip(
            &mut pixmap,
            x as f32,
            0.0,
            chip_width.min(f64::from(width_u32) - x) as f32,
            height_u32 as f32,
            chrome,
            border,
        );
        draw_label(
            &mut pixmap,
            key.label(),
            LogicalPoint::new(x + pad_x, pad_y),
            font_size,
            text,
        );
        if *count > 1 {
            let label_width = font::measure(key.label(), font_size).width;
            draw_label(
                &mut pixmap,
                &format!(" x{count}"),
                LogicalPoint::new(x + pad_x + label_width, pad_y),
                font_size,
                text,
            );
        }
        x += chip_width + gap;
    }
    let origin = anchored_origin(
        recording.keystrokes.position,
        canvas,
        PhysicalSize::new(f64::from(width_u32), f64::from(height_u32)),
        28.0 * f64::from(recording.visual_scale(canvas)),
    );
    Some(OverlayLayer {
        content: frame(pixmap),
        origin,
        opacity: 1.0,
        adaptive_contrast: recording.keystrokes.theme == OverlayTheme::Adaptive,
    })
}

fn theme_colors(theme: OverlayTheme) -> (Rgba8, Rgba8, Rgba8) {
    match theme {
        OverlayTheme::Light => (
            Rgba8::rgba(248, 249, 252, 232),
            Rgba8::rgb(29, 31, 38),
            Rgba8::rgba(24, 27, 36, 70),
        ),
        OverlayTheme::Dark => (
            Rgba8::rgba(24, 27, 36, 232),
            Rgba8::rgb(250, 250, 252),
            Rgba8::rgba(255, 255, 255, 72),
        ),
        OverlayTheme::Adaptive => (
            Rgba8::rgba(24, 27, 36, 224),
            Rgba8::rgb(255, 255, 255),
            Rgba8::rgba(255, 255, 255, 140),
        ),
    }
}

fn draw_chip(
    pixmap: &mut Pixmap,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    fill: Rgba8,
    border: Rgba8,
) {
    let Some(rect) = Rect::from_xywh(x, y, width.max(1.0), height.max(1.0)) else {
        return;
    };
    let path = PathBuilder::from_rect(rect);
    let mut paint = Paint::default();
    paint.set_color_rgba8(fill.r, fill.g, fill.b, fill.a);
    paint.anti_alias = true;
    pixmap.fill_path(
        &path,
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    paint.set_color_rgba8(border.r, border.g, border.b, border.a);
    pixmap.stroke_path(
        &path,
        &paint,
        &Stroke {
            width: 1.0,
            ..Stroke::default()
        },
        Transform::identity(),
        None,
    );
}

fn draw_label(
    pixmap: &mut Pixmap,
    label: &str,
    origin: LogicalPoint,
    font_size: f64,
    color: Rgba8,
) {
    let mut path = PathBuilder::new();
    for stroke in font::outline(label, origin, font_size) {
        let Some(first) = stroke.first() else {
            continue;
        };
        path.move_to(first.x as f32, first.y as f32);
        for point in &stroke[1..] {
            path.line_to(point.x as f32, point.y as f32);
        }
    }
    let Some(path) = path.finish() else {
        return;
    };
    let mut paint = Paint::default();
    paint.set_color_rgba8(color.r, color.g, color.b, color.a);
    paint.anti_alias = true;
    pixmap.stroke_path(
        &path,
        &paint,
        &Stroke {
            width: (font_size as f32 * 0.12).max(1.0),
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Stroke::default()
        },
        Transform::identity(),
        None,
    );
}

fn anchored_origin(
    anchor: OverlayAnchor,
    canvas: PhysicalSize,
    layer: PhysicalSize,
    margin: f64,
) -> PhysicalPoint {
    let (x, y) = anchor.unit();
    let x = f64::from(x);
    let y = f64::from(y);
    PhysicalPoint::new(
        (margin + (canvas.width - layer.width - margin * 2.0).max(0.0) * x)
            .clamp(0.0, (canvas.width - layer.width).max(0.0)),
        (margin + (canvas.height - layer.height - margin * 2.0).max(0.0) * y)
            .clamp(0.0, (canvas.height - layer.height).max(0.0)),
    )
}

fn frame(pixmap: Pixmap) -> Frame {
    let width = pixmap.width();
    let height = pixmap.height();
    Frame {
        data: pixmap.take(),
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride: width as usize * 4,
        format: PixelFormat::RgbaPremultiplied8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::IDENTITY,
    }
}

fn blend_rgba(destination: &mut RgbaImage, layer: &OverlayLayer) {
    let origin_x = layer.origin.x.round() as i64;
    let origin_y = layer.origin.y.round() as i64;
    let opacity = layer.opacity.clamp(0.0, 1.0);
    let source_width = layer.content.width() as usize;
    let source_height = layer.content.height() as usize;
    let destination_width = destination.width as usize;
    let destination_height = destination.height as usize;
    let invert = layer.adaptive_contrast
        && background_is_dark_rgba(destination, origin_x, origin_y, source_width, source_height);
    for source_y in 0..source_height {
        let destination_y = origin_y + source_y as i64;
        if !(0..destination_height as i64).contains(&destination_y) {
            continue;
        }
        for source_x in 0..source_width {
            let destination_x = origin_x + source_x as i64;
            if !(0..destination_width as i64).contains(&destination_x) {
                continue;
            }
            let source_offset = source_y * layer.content.stride + source_x * 4;
            let source = &layer.content.data[source_offset..source_offset + 4];
            let alpha = f32::from(source[3]) / 255.0 * opacity;
            let destination_offset =
                (destination_y as usize * destination_width + destination_x as usize) * 4;
            let target = &mut destination.data[destination_offset..destination_offset + 4];
            let inverse = 1.0 - alpha;
            for channel in 0..3 {
                let raw = if invert && layer.content.format.is_premultiplied() {
                    source[3].saturating_sub(source[channel])
                } else if invert {
                    255_u8.saturating_sub(source[channel])
                } else {
                    source[channel]
                };
                let source_channel = if layer.content.format.is_premultiplied() {
                    f32::from(raw) / 255.0 * opacity
                } else {
                    f32::from(raw) / 255.0 * alpha
                };
                target[channel] = (source_channel + f32::from(target[channel]) / 255.0 * inverse)
                    .clamp(0.0, 1.0)
                    .mul_add(255.0, 0.0)
                    .round() as u8;
            }
            target[3] = (alpha + f32::from(target[3]) / 255.0 * inverse)
                .clamp(0.0, 1.0)
                .mul_add(255.0, 0.0)
                .round() as u8;
        }
    }

    fn background_is_dark_rgba(
        image: &RgbaImage,
        origin_x: i64,
        origin_y: i64,
        width: usize,
        height: usize,
    ) -> bool {
        let x0 = origin_x.max(0) as usize;
        let y0 = origin_y.max(0) as usize;
        let x1 = (origin_x + width as i64).clamp(0, i64::from(image.width)) as usize;
        let y1 = (origin_y + height as i64).clamp(0, i64::from(image.height)) as usize;
        if x0 >= x1 || y0 >= y1 {
            return false;
        }
        let step_x = ((x1 - x0) / 16).max(1);
        let step_y = ((y1 - y0) / 8).max(1);
        let mut total = 0_u64;
        let mut count = 0_u64;
        for y in (y0..y1).step_by(step_y) {
            for x in (x0..x1).step_by(step_x) {
                let offset = (y * image.width as usize + x) * 4;
                let pixel = &image.data[offset..offset + 3];
                total +=
                    u64::from(pixel[0]) * 54 + u64::from(pixel[1]) * 183 + u64::from(pixel[2]) * 19;
                count += 256;
            }
        }
        count > 0 && total / count < 116
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::LogicalSize;

    use super::*;
    use crate::{
        interaction::{InputSecurity, PointerButton, interaction_channel},
        overlay::KeystrokeKind,
        settings::{ClickSettings, KeystrokeScope, KeystrokeSettings},
    };

    #[derive(Default)]
    struct SyntheticMonitor {
        events: Vec<InteractionEvent>,
        cursor: Option<LogicalPoint>,
        paused: bool,
    }

    impl InputMonitor for SyntheticMonitor {
        fn drain(&mut self) -> Vec<InteractionEvent> {
            if self.paused {
                Vec::new()
            } else {
                std::mem::take(&mut self.events)
            }
        }

        fn cursor_position(&self) -> Option<LogicalPoint> {
            (!self.paused).then_some(self.cursor).flatten()
        }

        fn pause(&mut self) {
            self.paused = true;
        }

        fn resume(&mut self) {
            self.paused = false;
        }

        fn take_dropped(&mut self) -> u64 {
            0
        }
    }

    fn source() -> InteractionOverlaySource {
        let clicks = ClickSettings {
            enabled: true,
            ..ClickSettings::default()
        };
        let keys = KeystrokeSettings {
            enabled: true,
            ..KeystrokeSettings::default()
        };
        let (_, mut consumer) = interaction_channel(8, KeystrokeScope::ModifiersOnly).unwrap();
        let _ = consumer.take_dropped();
        InteractionOverlaySource::new(
            Box::new(SyntheticMonitor {
                events: vec![
                    InteractionEvent::Click {
                        at: Duration::from_millis(10),
                        position: LogicalPoint::new(50.0, 25.0),
                        button: PointerButton::Primary,
                    },
                    InteractionEvent::Keystroke {
                        at: Duration::from_millis(20),
                        key: CapturedKeystroke::new("⌘K", KeystrokeKind::Modifier, false).unwrap(),
                    },
                ],
                cursor: Some(LogicalPoint::new(80.0, 40.0)),
                paused: false,
            }),
            InteractionMapper::new(scrozz_core::LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(100.0, 50.0),
            ))
            .unwrap(),
            InteractionRecording::new(clicks, keys, true, true),
            true,
        )
    }

    #[test]
    fn one_renderer_drives_live_and_export_pixels() {
        let mut source = source();
        let canvas = PhysicalSize::new(400.0, 200.0);
        let live = source.layers(Duration::from_millis(30), canvas);
        let recording = source.finish().unwrap();
        let exported = render_layers(
            &recording,
            Duration::from_millis(30),
            canvas,
            recording.default_edits(),
        );
        assert_eq!(live.len(), exported.len());
        for (left, right) in live.iter().zip(exported) {
            assert_eq!(left.origin, right.origin);
            assert_eq!(left.content.data, right.content.data);
        }
    }

    #[test]
    fn privacy_filter_operates_before_the_renderer() {
        let (producer, mut consumer) = interaction_channel(8, KeystrokeScope::All).unwrap();
        assert!(!producer.push_key(
            Duration::ZERO,
            CapturedKeystroke::new("secret", KeystrokeKind::Text, false).unwrap(),
            InputSecurity::Unknown,
            false,
        ));
        assert!(consumer.drain().is_empty());
    }

    #[test]
    fn repeat_events_compose_into_one_wider_chip_without_changing_timing() {
        let settings = KeystrokeSettings {
            enabled: true,
            scope: KeystrokeScope::All,
            ..KeystrokeSettings::default()
        };
        let mut single =
            InteractionRecording::new(ClickSettings::default(), settings, false, false);
        single.push(RetainedInteraction::Keystroke {
            at: Duration::from_millis(10),
            key: CapturedKeystroke::new("A", KeystrokeKind::Text, false).unwrap(),
        });
        let mut repeated = single.clone();
        repeated.push(RetainedInteraction::Keystroke {
            at: Duration::from_millis(20),
            key: CapturedKeystroke::new("A", KeystrokeKind::Text, true).unwrap(),
        });

        let canvas = PhysicalSize::new(800.0, 450.0);
        let one = render_layers(
            &single,
            Duration::from_millis(30),
            canvas,
            InteractionEdits {
                keystrokes: true,
                ..InteractionEdits::default()
            },
        );
        let two = render_layers(
            &repeated,
            Duration::from_millis(30),
            canvas,
            InteractionEdits {
                keystrokes: true,
                ..InteractionEdits::default()
            },
        );
        assert_eq!(one.len(), 1);
        assert_eq!(two.len(), 1);
        assert!(two[0].content.width() > one[0].content.width());
        assert_eq!(
            repeated.events[0].at(),
            Duration::from_millis(10),
            "repeat composition must not rewrite captured timing"
        );
    }

    #[test]
    fn edge_cursor_and_click_layers_remain_clip_safe() {
        let clicks = ClickSettings {
            enabled: true,
            ..ClickSettings::default()
        };
        let mut recording =
            InteractionRecording::new(clicks, KeystrokeSettings::default(), true, true);
        recording.push(RetainedInteraction::Click {
            at: Duration::ZERO,
            position: crate::interaction::NormalizedPoint { x: 0.0, y: 0.0 },
            button: PointerButton::Primary,
        });
        recording.push(RetainedInteraction::Cursor {
            at: Duration::ZERO,
            position: crate::interaction::NormalizedPoint { x: 1.0, y: 1.0 },
        });
        let layers = render_layers(
            &recording,
            Duration::from_millis(1),
            PhysicalSize::new(320.0, 180.0),
            recording.default_edits(),
        );
        assert_eq!(layers.len(), 2);
        assert!(layers.iter().all(|layer| layer.content.is_well_formed()));
        assert!(layers.iter().all(|layer| {
            layer.origin.x < 320.0
                && layer.origin.y < 180.0
                && layer.origin.x + layer.content.size.width > 0.0
                && layer.origin.y + layer.content.size.height > 0.0
        }));
    }

    #[test]
    fn adaptive_keystrokes_choose_opposite_contrast_for_dark_and_light_video() {
        let settings = KeystrokeSettings {
            enabled: true,
            scope: KeystrokeScope::All,
            theme: OverlayTheme::Adaptive,
            ..KeystrokeSettings::default()
        };
        let mut recording =
            InteractionRecording::new(ClickSettings::default(), settings, false, false);
        recording.push(RetainedInteraction::Keystroke {
            at: Duration::ZERO,
            key: CapturedKeystroke::new("K", KeystrokeKind::Text, false).unwrap(),
        });
        let edits = InteractionEdits {
            keystrokes: true,
            ..InteractionEdits::default()
        };
        let mut dark = RgbaImage {
            width: 320,
            height: 180,
            data: [0_u8, 0, 0, 255].repeat(320 * 180),
        };
        let mut light = RgbaImage {
            width: 320,
            height: 180,
            data: [255_u8, 255, 255, 255].repeat(320 * 180),
        };
        composite_interactions(&mut dark, &recording, Duration::from_millis(10), edits);
        composite_interactions(&mut light, &recording, Duration::from_millis(10), edits);
        let dark_changed: Vec<&[u8; 4]> = dark
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[..3] != [0, 0, 0])
            .collect();
        let light_changed: Vec<&[u8; 4]> = light
            .data
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|pixel| pixel[..3] != [255, 255, 255])
            .collect();
        assert!(!dark_changed.is_empty());
        assert!(!light_changed.is_empty());
        let average = |pixels: &[&[u8; 4]]| {
            pixels
                .iter()
                .map(|pixel| u64::from(pixel[0]) + u64::from(pixel[1]) + u64::from(pixel[2]))
                .sum::<u64>()
                / pixels.len() as u64
        };
        assert!(average(&dark_changed) > average(&light_changed));
    }

    #[test]
    fn preview_and_export_keep_overlay_geometry_proportional() {
        let mut recording = InteractionRecording::new(
            ClickSettings::default(),
            KeystrokeSettings::default(),
            true,
            true,
        );
        recording.set_reference_size(PhysicalSize::new(3_840.0, 2_160.0));
        recording.push(RetainedInteraction::Cursor {
            at: Duration::ZERO,
            position: crate::interaction::NormalizedPoint { x: 0.5, y: 0.5 },
        });
        let full = render_layers(
            &recording,
            Duration::ZERO,
            PhysicalSize::new(3_840.0, 2_160.0),
            recording.default_edits(),
        );
        let preview = render_layers(
            &recording,
            Duration::ZERO,
            PhysicalSize::new(960.0, 540.0),
            recording.default_edits(),
        );
        assert_eq!(full.len(), 1);
        assert_eq!(preview.len(), 1);
        assert_eq!(full[0].content.width(), preview[0].content.width() * 4);
        assert_eq!(full[0].content.height(), preview[0].content.height() * 4);
        assert!((full[0].origin.x / 3_840.0 - preview[0].origin.x / 960.0).abs() < 0.002);
        assert!((full[0].origin.y / 2_160.0 - preview[0].origin.y / 540.0).abs() < 0.002);
    }

    #[test]
    fn moving_window_updates_global_to_recording_coordinates() {
        let clicks = ClickSettings {
            enabled: true,
            ..ClickSettings::default()
        };
        let mut source = InteractionOverlaySource::new(
            Box::new(SyntheticMonitor {
                events: vec![InteractionEvent::Click {
                    at: Duration::ZERO,
                    position: LogicalPoint::new(150.0, 75.0),
                    button: PointerButton::Primary,
                }],
                cursor: None,
                paused: false,
            }),
            InteractionMapper::new(scrozz_core::LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(100.0, 100.0),
            ))
            .unwrap(),
            InteractionRecording::new(clicks, KeystrokeSettings::default(), false, false),
            true,
        );
        source
            .update_source_bounds(scrozz_core::LogicalRect::new(
                LogicalPoint::new(100.0, 25.0),
                LogicalSize::new(100.0, 100.0),
            ))
            .unwrap();
        let layers = source.layers(
            Duration::from_millis(1),
            PhysicalSize::new(1_000.0, 1_000.0),
        );
        assert_eq!(layers.len(), 1);
        let center_x = layers[0].origin.x + layers[0].content.size.width / 2.0;
        let center_y = layers[0].origin.y + layers[0].content.size.height / 2.0;
        assert!((center_x - 500.0).abs() < 1.0);
        assert!((center_y - 500.0).abs() < 1.0);
    }
}
