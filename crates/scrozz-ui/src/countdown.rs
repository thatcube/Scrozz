//! Value-driven pre-recording countdown overlay.

use std::time::Duration;

use egui::{Align2, Color32, Response, Sense, Stroke, Ui, WidgetInfo, WidgetType};
use scrozz_record::settings::CountdownSettings;

use crate::{
    harness::{RecordingFixture, Scene, SceneCtx},
    recording_controls::{install_scene_theme, scene_theme},
    theme::{Text, Theme},
};

/// A countdown at a caller-supplied virtual instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Countdown {
    /// Countdown preference in force.
    pub settings: CountdownSettings,
    /// Remaining virtual time.
    pub remaining: Duration,
}

impl Countdown {
    /// Creates a countdown from settings and current remaining time.
    #[must_use]
    pub const fn new(settings: CountdownSettings, remaining: Duration) -> Self {
        Self {
            settings,
            remaining,
        }
    }

    /// Whether the overlay should remain visible.
    ///
    /// Disabled and zero-second countdowns are inactive even if malformed input
    /// says `enabled = true`, so UI cannot hold a recording at zero.
    #[must_use]
    pub fn is_active(self) -> bool {
        self.settings.enabled && self.settings.seconds > 0 && !self.remaining.is_zero()
    }

    /// Whole count displayed now, rounded up so the final fraction reads `1`.
    #[must_use]
    pub fn displayed_count(self) -> Option<u8> {
        if !self.is_active() {
            return None;
        }
        let count = self.remaining.as_secs_f64().ceil();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Some(
            (count as u8)
                .max(1)
                .min(self.settings.seconds)
                .min(CountdownSettings::MAX_SECONDS),
        )
    }

    /// Draws a focused overlay. Rendering never advances the countdown.
    pub fn show(self, ui: &mut Ui, theme: &Theme) -> CountdownResponse {
        let Some(count) = self.displayed_count() else {
            return CountdownResponse {
                active: false,
                displayed_count: None,
                response: None,
            };
        };

        let rect = ui.max_rect();
        let response = ui.interact(rect, ui.id().with("recording-countdown"), Sense::hover());
        let accessible = format!("Recording starts in {count}");
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, accessible.clone()));

        let painter = ui.painter();
        painter.rect_filled(rect, 0.0, Color32::from_black_alpha(150));
        let center = rect.center();
        let radius = rect
            .width()
            .min(rect.height())
            .mul_add(0.12, 54.0)
            .min(112.0);
        painter.circle_filled(center, radius, theme.palette.card_fill_raised);
        painter.circle_stroke(center, radius, Stroke::new(2.0, theme.palette.accent_hi));
        painter.circle_stroke(
            center,
            radius + 9.0,
            Stroke::new(1.0, theme.palette.accent.linear_multiply(0.45)),
        );
        painter.text(
            center,
            Align2::CENTER_CENTER,
            count.to_string(),
            egui::FontId::new(64.0 * theme.text_scale, Text::Display.weight().family()),
            theme.palette.text,
        );
        painter.text(
            egui::pos2(center.x, center.y + radius + 30.0),
            Align2::CENTER_CENTER,
            "Recording starts",
            theme.font(Text::Title),
            theme.palette.text,
        );

        CountdownResponse {
            active: true,
            displayed_count: Some(count),
            response: Some(response),
        }
    }
}

/// Result of drawing a [`Countdown`].
#[derive(Debug)]
pub struct CountdownResponse {
    /// Whether countdown content was drawn.
    pub active: bool,
    /// Count presented to sighted and assistive-technology users.
    pub displayed_count: Option<u8>,
    /// Overlay accessibility/hit response, absent when inactive.
    pub response: Option<Response>,
}

/// Real countdown renderer used by the deterministic harness.
#[derive(Debug, Default)]
pub struct CountdownScene;

impl Scene for CountdownScene {
    fn name(&self) -> &str {
        "recording-countdown"
    }

    fn setup(&self, ctx: &egui::Context) {
        install_scene_theme(ctx);
    }

    fn ui(&self, ui: &mut Ui, ctx: &SceneCtx<'_>) {
        let Some(RecordingFixture::Countdown(fixture)) = ctx.fixture.recording.as_ref() else {
            return;
        };
        Countdown::new(fixture.settings, fixture.remaining).show(ui, &scene_theme(ctx.theme));
    }
}
