//! Review UI and state for possible sensitive information.
//!
//! The review never receives recognized text. Rows identify only a category,
//! confidence, and non-secret reason, while the canvas identifies the source
//! region. Nothing is selected by default and no button directly changes
//! pixels: the host explicitly applies selected findings as ordinary secure
//! redaction annotations after an exact revision check.

use std::collections::BTreeMap;

use egui::{Color32, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType};
use scrozz_annotate::{AnnotationId, Document};
use scrozz_core::{ContentRevision, LogicalSize, Result};
use scrozz_ocr::{
    FindingId, LocalSensitiveDetector, SensitiveFinding, SensitiveScan, SensitiveScanOptions,
    SensitiveSource, TextBlock,
};

use crate::{
    harness::{Scene, SceneCtx},
    theme::{Appearance, Radius, Space, Text, Theme, corner},
};

/// Review decision for one finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FindingDecision {
    /// Visible but not selected.
    Pending,
    /// Explicitly selected for redaction.
    Selected,
    /// Explicitly dismissed from this review.
    Ignored,
    /// Converted into an ordinary redaction annotation.
    Applied,
}

/// Actions requested by one UI pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SensitiveReviewResponse {
    /// The user asked to create redactions from selected findings.
    pub apply_requested: bool,
    /// The user asked to scan the current revision again.
    pub rescan_requested: bool,
    /// The review selection changed.
    pub changed: bool,
}

/// Ephemeral review state for one immutable scan.
#[derive(Debug, Clone)]
pub struct SensitiveReview {
    scan: SensitiveScan,
    source_size: LogicalSize,
    decisions: BTreeMap<FindingId, FindingDecision>,
    show_review_confidence: bool,
}

impl SensitiveReview {
    /// Starts an unselected review for one source revision.
    #[must_use]
    pub fn new(scan: SensitiveScan, source_size: LogicalSize) -> Self {
        let decisions = scan
            .findings()
            .iter()
            .map(|finding| (finding.id(), FindingDecision::Pending))
            .collect();
        Self {
            scan,
            source_size,
            decisions,
            show_review_confidence: false,
        }
    }

    /// Immutable scan under review.
    #[must_use]
    pub const fn scan(&self) -> &SensitiveScan {
        &self.scan
    }

    /// Whether lower-confidence candidates are visible.
    #[must_use]
    pub const fn shows_review_confidence(&self) -> bool {
        self.show_review_confidence
    }

    /// Shows or hides review-confidence candidates.
    pub fn set_show_review_confidence(&mut self, show: bool) {
        self.show_review_confidence = show;
    }

    /// Current decision for `id`.
    #[must_use]
    pub fn decision(&self, id: FindingId) -> Option<FindingDecision> {
        self.decisions.get(&id).copied()
    }

    /// Selects or deselects one non-ignored finding.
    pub fn set_selected(&mut self, id: FindingId, selected: bool) -> bool {
        let Some(decision) = self.decisions.get_mut(&id) else {
            return false;
        };
        if matches!(
            *decision,
            FindingDecision::Ignored | FindingDecision::Applied
        ) {
            return false;
        }
        let next = if selected {
            FindingDecision::Selected
        } else {
            FindingDecision::Pending
        };
        let changed = *decision != next;
        *decision = next;
        changed
    }

    /// Ignores one finding for this review.
    pub fn ignore(&mut self, id: FindingId) -> bool {
        let Some(decision) = self.decisions.get_mut(&id) else {
            return false;
        };
        if *decision == FindingDecision::Applied {
            return false;
        }
        let changed = *decision != FindingDecision::Ignored;
        *decision = FindingDecision::Ignored;
        changed
    }

    /// Selects every currently visible, non-ignored finding.
    pub fn select_all_visible(&mut self) -> usize {
        let visible: Vec<FindingId> = self.visible_findings().map(SensitiveFinding::id).collect();
        visible
            .into_iter()
            .filter(|id| self.set_selected(*id, true))
            .count()
    }

    /// Clears every unapplied selection.
    pub fn clear_selection(&mut self) {
        for decision in self.decisions.values_mut() {
            if *decision == FindingDecision::Selected {
                *decision = FindingDecision::Pending;
            }
        }
    }

    /// Number of findings explicitly selected for redaction.
    #[must_use]
    pub fn selected_count(&self) -> usize {
        self.decisions
            .values()
            .filter(|decision| **decision == FindingDecision::Selected)
            .count()
    }

    /// Whether the source document changed after this scan.
    #[must_use]
    pub fn is_stale_for(&self, document: &Document) -> bool {
        document.revision() != self.scan.revision()
    }

    /// Converts selected current findings into ordinary secure redactions.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for stale scans or invalid bounds.
    pub fn apply_selected(&mut self, document: &mut Document) -> Result<Vec<AnnotationId>> {
        let selected: Vec<(FindingId, _)> = self
            .scan
            .findings()
            .iter()
            .filter(|finding| self.decision(finding.id()) == Some(FindingDecision::Selected))
            .map(|finding| (finding.id(), finding.bounds()))
            .collect();
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        let ids = document.add_redactions_at_revision(
            self.scan.revision(),
            selected.iter().map(|(_, bounds)| *bounds),
        )?;
        for (finding, _) in selected {
            self.decisions.insert(finding, FindingDecision::Applied);
        }
        Ok(ids)
    }

    /// Paints numbered review outlines over a source-image rectangle.
    pub fn paint_overlays(&self, ui: &Ui, target: Rect, theme: &Theme) {
        let source_width = self.source_size.width.max(1.0) as f32;
        let source_height = self.source_size.height.max(1.0) as f32;
        for finding in self.visible_findings() {
            let decision = self
                .decision(finding.id())
                .unwrap_or(FindingDecision::Pending);
            if decision == FindingDecision::Ignored {
                continue;
            }
            let bounds = finding.bounds();
            let min = egui::pos2(
                target.left() + target.width() * (bounds.origin.x as f32 / source_width),
                target.top() + target.height() * (bounds.origin.y as f32 / source_height),
            );
            let max = egui::pos2(
                min.x + target.width() * (bounds.size.width as f32 / source_width),
                min.y + target.height() * (bounds.size.height as f32 / source_height),
            );
            let rect = Rect::from_min_max(min, max).intersect(target);
            if !rect.is_positive() {
                continue;
            }
            let selected = decision == FindingDecision::Selected;
            let color = if selected {
                theme.palette.accent
            } else {
                theme.palette.text_muted
            };
            ui.painter().rect_stroke(
                rect,
                corner(Radius::CHIP),
                Stroke::new(if selected { 2.0 } else { 1.0 }, color),
                StrokeKind::Inside,
            );
            ui.painter().text(
                rect.left_top() + egui::vec2(Space::XS, Space::XS),
                egui::Align2::LEFT_TOP,
                finding.id().get().to_string(),
                theme.font(Text::Caption),
                color,
            );
        }
    }

    /// Draws the review controls.
    pub fn show(&mut self, ui: &mut Ui, theme: &Theme, stale: bool) -> SensitiveReviewResponse {
        let mut response = SensitiveReviewResponse::default();
        ui.heading(
            egui::RichText::new("Possible sensitive information")
                .font(theme.font(Text::Title))
                .color(theme.palette.text),
        );
        ui.add_space(Space::XS);
        ui.label(
            egui::RichText::new(
                "Review each suggestion. Nothing changes until you create redactions.",
            )
            .font(theme.font(Text::Body))
            .color(theme.palette.text_muted),
        );
        if stale {
            ui.add_space(Space::MD);
            ui.colored_label(
                theme.palette.accent,
                "This scan is out of date because the image was edited.",
            );
            response.rescan_requested = ui.button("Scan current image").clicked();
            return response;
        }

        ui.add_space(Space::LG);
        ui.horizontal(|ui| {
            if ui.button("Select all").clicked() {
                response.changed |= self.select_all_visible() > 0;
            }
            if ui.button("Clear").clicked() {
                self.clear_selection();
                response.changed = true;
            }
        });
        ui.add_space(Space::SM);
        egui::ScrollArea::vertical()
            .max_height(360.0)
            .show(ui, |ui| {
                let findings: Vec<_> = self.visible_findings().cloned().collect();
                for finding in findings {
                    let id = finding.id();
                    let number = id.get();
                    let category = finding.category().label();
                    let mut selected = self.decision(id) == Some(FindingDecision::Selected);
                    ui.horizontal(|ui| {
                        let selection_label = format!("Suggestion {number}: {category}");
                        let selection = ui.checkbox(&mut selected, &selection_label);
                        selection.widget_info(|| {
                            WidgetInfo::selected(
                                WidgetType::Checkbox,
                                true,
                                selected,
                                selection_label.clone(),
                            )
                        });
                        if selection.changed() {
                            response.changed |= self.set_selected(id, selected);
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let ignore_label = format!("Ignore suggestion {number}, {category}");
                            let ignore = ui.small_button(format!("Ignore {number}"));
                            ignore.widget_info(|| {
                                WidgetInfo::labeled(WidgetType::Button, true, ignore_label.clone())
                            });
                            if ignore.clicked() {
                                response.changed |= self.ignore(id);
                            }
                            ui.label(
                                egui::RichText::new(format!(
                                    "{:.0}%",
                                    finding.confidence().as_f32() * 100.0
                                ))
                                .font(theme.font(Text::Caption))
                                .color(theme.palette.text_muted),
                            );
                        });
                    });
                    ui.indent(("sensitive-reason", id.get()), |ui| {
                        ui.label(
                            egui::RichText::new(finding.reason().label())
                                .font(theme.font(Text::Caption))
                                .color(theme.palette.text_muted),
                        );
                    });
                    ui.separator();
                }
            });

        let review_count = self
            .scan
            .findings()
            .iter()
            .filter(|finding| {
                !finding.confidence().is_high()
                    && !matches!(
                        self.decision(finding.id()),
                        Some(FindingDecision::Ignored | FindingDecision::Applied)
                    )
            })
            .count();
        if review_count > 0 {
            ui.add_space(Space::SM);
            if ui
                .checkbox(
                    &mut self.show_review_confidence,
                    format!("Show {review_count} lower-confidence suggestion(s)"),
                )
                .changed()
            {
                response.changed = true;
            }
        }
        if self.scan.is_truncated() {
            ui.label(
                egui::RichText::new("More suggestions exist. Narrow the image and scan again.")
                    .font(theme.font(Text::Caption))
                    .color(theme.palette.text_muted),
            );
        }
        ui.add_space(Space::LG);
        let count = self.selected_count();
        response.apply_requested = ui
            .add_enabled(
                count > 0,
                egui::Button::new(format!("Create redactions ({count})")),
            )
            .clicked();
        response
    }

    fn visible_findings(&self) -> impl Iterator<Item = &SensitiveFinding> {
        self.scan.findings().iter().filter(|finding| {
            (finding.confidence().is_high() || self.show_review_confidence)
                && self.decision(finding.id()) != Some(FindingDecision::Ignored)
        })
    }
}

/// Deterministic golden scene for the sensitive-information review.
#[derive(Debug, Clone, Copy, Default)]
pub struct SensitiveReviewScene;

impl Scene for SensitiveReviewScene {
    fn name(&self) -> &str {
        "sensitive-review"
    }

    fn setup(&self, context: &egui::Context) {
        crate::theme::install_fonts(context);
    }

    fn ui(&self, ui: &mut Ui, context: &SceneCtx<'_>) {
        let appearance = match context.theme {
            egui::Theme::Dark => Appearance::Dark,
            egui::Theme::Light => Appearance::Light,
        };
        let theme = Theme::for_appearance(appearance);
        crate::theme::install_style(ui.ctx(), &theme);
        ui.painter()
            .rect_filled(ui.max_rect(), 0.0, theme.palette.card_fill);
        let available = ui.available_size();
        let mut review = scene_review();
        review.select_all_visible();
        review.set_show_review_confidence(true);
        ui.horizontal(|ui| {
            let canvas_width = (available.x * 0.62).max(360.0);
            ui.allocate_ui_with_layout(
                Vec2::new(canvas_width, available.y),
                egui::Layout::top_down(egui::Align::Center),
                |ui| {
                    let (canvas, _) = ui.allocate_exact_size(
                        Vec2::new(canvas_width - Space::XXL, available.y - Space::XXL),
                        Sense::hover(),
                    );
                    ui.painter().rect_filled(
                        canvas,
                        corner(Radius::BAR),
                        theme.palette.card_fill_raised,
                    );
                    paint_synthetic_capture(ui, canvas, &theme);
                    review.paint_overlays(ui, canvas.shrink(36.0), &theme);
                },
            );
            ui.separator();
            ui.allocate_ui_with_layout(
                Vec2::new(
                    (available.x - canvas_width - Space::HUGE).max(280.0),
                    available.y,
                ),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    ui.add_space(Space::LG);
                    let _ = review.show(ui, &theme, false);
                },
            );
        });
    }
}

fn scene_review() -> SensitiveReview {
    let blocks = [
        TextBlock {
            text: "Email: ada@example.invalid".to_owned(),
            bounds: scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(66.0, 78.0),
                scrozz_core::LogicalSize::new(290.0, 28.0),
            ),
            confidence: 0.98,
        },
        TextBlock {
            text: "Payment card 4111 1111 1111 1111".to_owned(),
            bounds: scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(66.0, 146.0),
                scrozz_core::LogicalSize::new(360.0, 28.0),
            ),
            confidence: 0.99,
        },
        TextBlock {
            text: "https://example.invalid?token=A9b8C7d6E5f4".to_owned(),
            bounds: scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(66.0, 214.0),
                scrozz_core::LogicalSize::new(410.0, 28.0),
            ),
            confidence: 0.96,
        },
        TextBlock {
            text: "connect to 192.0.2.42".to_owned(),
            bounds: scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(66.0, 282.0),
                scrozz_core::LogicalSize::new(250.0, 28.0),
            ),
            confidence: 0.96,
        },
    ];
    let scan = LocalSensitiveDetector::new()
        .scan_blocks(
            ContentRevision::new(9),
            SensitiveSource::Image,
            &blocks,
            &SensitiveScanOptions {
                include_review_confidence: true,
                ..SensitiveScanOptions::default()
            },
            &scrozz_ocr::CancellationToken::new(),
        )
        .expect("synthetic sensitive-review fixture is valid");
    SensitiveReview::new(scan, LogicalSize::new(560.0, 380.0))
}

fn paint_synthetic_capture(ui: &Ui, canvas: Rect, theme: &Theme) {
    let inner = canvas.shrink(36.0);
    ui.painter()
        .rect_filled(inner, corner(Radius::BUTTON), theme.palette.chip_fill);
    for row in 0..5 {
        let y = inner.top() + 46.0 + row as f32 * 68.0;
        ui.painter().rect_filled(
            Rect::from_min_size(
                egui::pos2(inner.left() + 30.0, y),
                egui::vec2(inner.width() - 60.0, 20.0),
            ),
            corner(Radius::CHIP),
            Color32::from_rgba_unmultiplied(
                theme.palette.text.r(),
                theme.palette.text.g(),
                theme.palette.text.b(),
                24,
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_annotate::{Annotation, RedactStyle};
    use scrozz_core::{
        Capture, CaptureTarget, ColorSpace, Frame, PhysicalSize, PixelFormat, Provenance,
        ScaleFactor,
    };

    fn document() -> Document {
        Document::new(Capture {
            frame: Frame {
                data: vec![255; 600 * 400 * 4],
                size: PhysicalSize::new(600.0, 400.0),
                stride: 600 * 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            provenance: Provenance::Region,
            target: CaptureTarget::Region(scrozz_core::LogicalRect::new(
                scrozz_core::LogicalPoint::new(0.0, 0.0),
                scrozz_core::LogicalSize::new(600.0, 400.0),
            )),
        })
    }

    #[test]
    fn nothing_is_selected_or_applied_by_default() {
        let document = document();
        let review = scene_review();
        assert_eq!(review.selected_count(), 0);
        assert_eq!(review.visible_findings().count(), 3);
        assert_eq!(review.scan().findings().len(), 4);
        assert!(document.is_empty());
        assert_eq!(review.scan().revision(), ContentRevision::new(9));
    }

    #[test]
    fn explicit_selection_creates_ordinary_redactions() {
        let mut document = document();
        // The fixture scan is revision 9; a real host passes the document's
        // current revision. Re-scan the same safe fixture under revision 0.
        let scan = LocalSensitiveDetector::new()
            .scan_blocks(
                document.revision(),
                SensitiveSource::Image,
                &[TextBlock {
                    text: "Email: ada@example.invalid".to_owned(),
                    bounds: scrozz_core::LogicalRect::new(
                        scrozz_core::LogicalPoint::new(20.0, 20.0),
                        scrozz_core::LogicalSize::new(220.0, 24.0),
                    ),
                    confidence: 1.0,
                }],
                &SensitiveScanOptions::default(),
                &scrozz_ocr::CancellationToken::new(),
            )
            .unwrap();
        let mut review = SensitiveReview::new(scan, document.logical_size());
        review.select_all_visible();
        let ids = review.apply_selected(&mut document).unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(
            review.decision(review.scan().findings()[0].id()),
            Some(FindingDecision::Applied)
        );
        assert!(matches!(
            document.get(ids[0]).map(|object| &object.annotation),
            Some(Annotation::Redact {
                style: RedactStyle::Solid,
                ..
            })
        ));
        let persisted = serde_json::to_string(&document.data()).unwrap();
        assert!(!persisted.contains("ada@example"));
        assert!(persisted.contains("\"redact\""));
    }

    #[test]
    fn stale_review_cannot_apply_after_manual_edit() {
        let mut document = document();
        let scan = LocalSensitiveDetector::new()
            .scan_blocks(
                document.revision(),
                SensitiveSource::Image,
                &[TextBlock {
                    text: "Email: ada@example.invalid".to_owned(),
                    bounds: document.logical_bounds(),
                    confidence: 1.0,
                }],
                &SensitiveScanOptions::default(),
                &scrozz_ocr::CancellationToken::new(),
            )
            .unwrap();
        let mut review = SensitiveReview::new(scan, document.logical_size());
        review.select_all_visible();
        document.add_default(Annotation::Rectangle(document.logical_bounds()));
        let before = document.len();
        assert!(review.is_stale_for(&document));
        assert!(review.apply_selected(&mut document).is_err());
        assert_eq!(document.len(), before);
    }

    #[test]
    fn ignored_findings_never_apply() {
        let mut document = document();
        let mut review = scene_review();
        let finding = review.scan().findings()[0].id();
        assert!(review.ignore(finding));
        assert!(!review.set_selected(finding, true));
        assert_eq!(review.selected_count(), 0);
        assert!(review.apply_selected(&mut document).unwrap().is_empty());
        assert!(document.is_empty());
    }
}
