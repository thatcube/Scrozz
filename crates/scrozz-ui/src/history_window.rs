//! Ordinary capture-history viewport with recording salvage details.

use egui::{RichText, ScrollArea, Stroke, Ui, ViewportBuilder, WindowLevel};
use scrozz_store::{
    CaptureId, CaptureRecord, MediaKind, Timestamp, VideoCompletion, VideoSalvageability,
};

use crate::{
    recording_controls::{button, caption, heading, panel, rule},
    theme::{Space, Text, Theme},
};

/// Stable identity of the ordinary history viewport.
#[must_use]
pub fn viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("scrozz-recording-history")
}

/// Native, movable, resizable history-window properties.
#[must_use]
pub fn viewport_builder() -> ViewportBuilder {
    ViewportBuilder::default()
        .with_title("Scrozz History")
        .with_inner_size([920.0, 680.0])
        .with_min_inner_size([620.0, 420.0])
        .with_resizable(true)
        .with_decorations(true)
        .with_taskbar(true)
        .with_active(true)
        .with_window_level(WindowLevel::Normal)
}

/// Owned history data safe to clone into a secondary viewport.
#[derive(Debug, Clone, Default)]
pub struct HistoryWindowSnapshot {
    /// Newest-first records.
    pub records: Vec<CaptureRecord>,
    /// Total records across every page.
    pub total: u64,
    /// Zero-based offset of the current page.
    pub offset: u32,
    /// Item awaiting explicit destructive confirmation.
    pub confirm_delete: Option<CaptureId>,
    /// Visible loading/storage failure.
    pub error: Option<String>,
}

/// Semantic history request returned to the product owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryWindowAction {
    /// Close the ordinary window.
    Close,
    /// Reload the current page.
    Refresh,
    /// Show the previous, newer page.
    PreviousPage,
    /// Show the next, older page.
    NextPage,
    /// Change retention protection.
    SetPinned {
        /// Capture identity.
        id: CaptureId,
        /// New pinned value.
        pinned: bool,
    },
    /// Ask for destructive confirmation before deleting.
    RequestDelete(CaptureId),
    /// Leave the pending item intact.
    CancelDelete,
    /// Permanently delete one history item.
    Delete(CaptureId),
    /// Reveal externally stored video/GIF media.
    Reveal(CaptureId),
}

/// Draws the current history page.
#[must_use]
pub fn show(
    ui: &mut Ui,
    snapshot: &HistoryWindowSnapshot,
    theme: &Theme,
) -> Vec<HistoryWindowAction> {
    let mut actions = Vec::new();
    panel(ui, theme, ui.available_width().max(600.0), |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                heading(ui, theme, "Capture history");
                caption(ui, theme, page_summary(snapshot));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if button(ui, theme, "Close", false, true).clicked() {
                    actions.push(HistoryWindowAction::Close);
                }
                if button(ui, theme, "Refresh", false, true).clicked() {
                    actions.push(HistoryWindowAction::Refresh);
                }
            });
        });
        if let Some(error) = &snapshot.error {
            ui.add_space(Space::MD);
            ui.colored_label(theme.palette.recording, error);
        }
        ui.add_space(Space::MD);
        rule(ui, theme);
        ui.add_space(Space::SM);
        if snapshot.records.is_empty() {
            ui.add_space(Space::XXL);
            ui.vertical_centered(|ui| {
                heading(ui, theme, "History starts with your first capture");
                caption(
                    ui,
                    theme,
                    "Screenshots, complete recordings, and retained partials appear here.",
                );
            });
            return;
        }
        ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for record in &snapshot.records {
                    draw_record(
                        ui,
                        theme,
                        record,
                        snapshot.confirm_delete.as_ref() == Some(&record.id),
                        &mut actions,
                    );
                    ui.add_space(Space::SM);
                }
            });
        ui.add_space(Space::SM);
        rule(ui, theme);
        ui.add_space(Space::SM);
        ui.horizontal(|ui| {
            let has_previous = snapshot.offset > 0;
            if button(ui, theme, "Newer", false, has_previous).clicked() {
                actions.push(HistoryWindowAction::PreviousPage);
            }
            let shown_end = u64::from(snapshot.offset)
                .saturating_add(u64::try_from(snapshot.records.len()).unwrap_or(u64::MAX));
            let has_next = shown_end < snapshot.total;
            if button(ui, theme, "Older", false, has_next).clicked() {
                actions.push(HistoryWindowAction::NextPage);
            }
        });
    });
    actions
}

fn draw_record(
    ui: &mut Ui,
    theme: &Theme,
    record: &CaptureRecord,
    confirm_delete: bool,
    actions: &mut Vec<HistoryWindowAction>,
) {
    egui::Frame::new()
        .fill(theme.palette.chip_fill)
        .stroke(Stroke::new(1.0, theme.palette.hairline))
        .corner_radius(10.0)
        .inner_margin(14.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(primary_label(record))
                            .font(theme.font(Text::Title))
                            .color(theme.palette.text),
                    );
                    caption(
                        ui,
                        theme,
                        format!(
                            "{}  ·  {}  ·  {}",
                            record.media_kind.as_token(),
                            record.id.0,
                            format_timestamp(record.created_at)
                        ),
                    );
                    if let Some(video) = &record.video {
                        let (color, summary) = match &video.completion {
                            VideoCompletion::Complete => (
                                theme.palette.success,
                                format!(
                                    "{:.2}s complete  ·  {}",
                                    video.duration_secs,
                                    video.path.display()
                                ),
                            ),
                            VideoCompletion::Partial {
                                salvageability,
                                reason,
                            } => (
                                theme.palette.warning,
                                format!(
                                    "{:.2}s partial ({})  ·  {}  ·  {reason}",
                                    video.duration_secs,
                                    salvage_label(*salvageability),
                                    video.path.display()
                                ),
                            ),
                        };
                        ui.colored_label(color, summary);
                        caption(
                            ui,
                            theme,
                            format!(
                                "{}  ·  {}",
                                video.codec.as_deref().unwrap_or("unknown codec"),
                                video
                                    .content_type
                                    .as_deref()
                                    .or_else(|| video.inferred_content_type())
                                    .unwrap_or("unknown content type")
                            ),
                        );
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if confirm_delete {
                        if button(ui, theme, "Delete forever", false, true).clicked() {
                            actions.push(HistoryWindowAction::Delete(record.id.clone()));
                        }
                        if button(ui, theme, "Cancel", false, true).clicked() {
                            actions.push(HistoryWindowAction::CancelDelete);
                        }
                    } else if button(ui, theme, "Delete", false, true).clicked() {
                        actions.push(HistoryWindowAction::RequestDelete(record.id.clone()));
                    }
                    if button(
                        ui,
                        theme,
                        if record.pinned { "Unpin" } else { "Pin" },
                        false,
                        true,
                    )
                    .clicked()
                    {
                        actions.push(HistoryWindowAction::SetPinned {
                            id: record.id.clone(),
                            pinned: !record.pinned,
                        });
                    }

                    if record.media_kind.is_motion()
                        && button(ui, theme, "Show file", false, record.video.is_some()).clicked()
                    {
                        actions.push(HistoryWindowAction::Reveal(record.id.clone()));
                    }
                });
            });
        });
}

fn page_summary(snapshot: &HistoryWindowSnapshot) -> String {
    page_summary_parts(snapshot.total, snapshot.offset, snapshot.records.len())
}

fn page_summary_parts(total: u64, offset: u32, visible: usize) -> String {
    if total == 0 {
        return "0 items".to_owned();
    }
    let start = u64::from(offset).saturating_add(1);
    let end = u64::from(offset)
        .saturating_add(u64::try_from(visible).unwrap_or(u64::MAX))
        .min(total);
    format!("Showing {start}-{end} of {total} items")
}

fn primary_label(record: &CaptureRecord) -> String {
    record
        .window_title
        .clone()
        .or_else(|| record.app_name.clone())
        .unwrap_or_else(|| match record.media_kind {
            MediaKind::Screenshot => "Screenshot".to_owned(),
            MediaKind::Video => "Screen recording".to_owned(),
            MediaKind::Gif => "Animated GIF".to_owned(),
        })
}

fn salvage_label(value: VideoSalvageability) -> &'static str {
    match value {
        VideoSalvageability::InitialisationOnly => "initialisation only",
        VideoSalvageability::Playable => "playable",
    }
}

fn format_timestamp(value: Timestamp) -> String {
    format!("{} ms", value.as_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_uses_an_ordinary_native_viewport() {
        let viewport = viewport_builder();
        assert_eq!(viewport.decorations, Some(true));
        assert_eq!(viewport.taskbar, Some(true));
        assert_eq!(viewport.resizable, Some(true));
        assert_eq!(viewport.active, Some(true));
        assert_eq!(viewport.window_level, Some(WindowLevel::Normal));
        assert_eq!(viewport.position, None);
    }

    #[test]
    fn page_summary_is_explicit_about_bounded_history_pages() {
        assert_eq!(
            page_summary_parts(225, 100, 50),
            "Showing 101-150 of 225 items"
        );
        assert_eq!(page_summary_parts(0, 0, 0), "0 items");
    }
}
