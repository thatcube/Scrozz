//! Starting, and then outliving, a drag out of the capture stack.
//!
//! # The timing problem this exists to solve
//!
//! A native drag is not something an application *reports*; it is something an
//! application *starts*, and every platform insists it be started while the
//! mouse button is still held down. AppKit is the strictest: `beginDragging
//! Session:` called after the button came up starts nothing and returns a
//! session that immediately ends. So the capture stack cannot tell us "that
//! card was dragged away" once the gesture finishes — by then it is too late,
//! and the visible symptom is a card that flies off the screen while the chat
//! window it was aimed at receives nothing at all.
//!
//! `scrozz-ui` therefore raises [`OverlayEvent::DragOutArmed`][armed] the
//! moment the pointer has travelled far enough to mean it, *mid*-gesture, and
//! this module turns that into a real [`DragSource::begin`]. From that instant
//! the operating system owns the gesture: it draws the image, it tracks the
//! destination, and it tells us what happened.
//!
//! [armed]: scrozz_ui::OverlayEvent::DragOutArmed
//!
//! # Why the card does not leave on its own
//!
//! Because the drop may be refused. A card that vanishes the moment the pointer
//! crosses a threshold has thrown away the capture whether or not anything
//! caught it, and there is no way back. So the stack springs the card home, and
//! the card is dismissed here — only once [`DragOutcome::Accepted`] says a
//! destination really took it.
//!
//! # Why the file outlives the gesture
//!
//! A drop target is entitled to read the file it was handed *after* the drag
//! ends: Finder copies on its own schedule, and an Electron app may not touch
//! the path until its renderer process gets around to it. Deleting on drop
//! would make Scrozz work in Finder and fail in Slack for reasons no log would
//! explain. So [`DragArtifact`] keeps the file for a retention window and this
//! host sweeps it afterwards, every tick, until it is gone.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use image::imageops::FilterType;
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};
use scrozz_export::{FrameEncoder, ImageFormat, RgbaImage};
use scrozz_shell::{
    DragOrigin, DragOutcome, DragPayload, DragPreview, DragSession, DragSource, NativeDragSource,
    NativeSurface, byte_source, drag::artifact,
};

use crate::gui::{card::CardId, pipeline::CaptureBytes};

/// Where a drag-out gesture was when it committed.
///
/// Window-local logical points, top-left origin — the coordinate space
/// `scrozz-ui` reports and [`DragOrigin`] expects. Kept as plain numbers so the
/// card layer never has to name a platform type.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DragSpot {
    /// The card's rectangle: x, y, width, height.
    pub card: [f32; 4],
    /// The pointer position: x, y.
    pub pointer: [f32; 2],
}

impl DragSpot {
    /// Converts to the shell's geometry, anchored at `surface`.
    fn origin(self, surface: NativeSurface) -> DragOrigin {
        let [x, y, w, h] = self.card;
        DragOrigin::new(
            surface,
            LogicalRect::new(
                LogicalPoint::new(f64::from(x), f64::from(y)),
                LogicalSize::new(f64::from(w), f64::from(h)),
            ),
            LogicalPoint::new(f64::from(self.pointer[0]), f64::from(self.pointer[1])),
        )
    }
}

/// What a drag that has been started is still waiting to find out.
struct InFlight {
    session: DragSession,
    /// Whether the outcome has already been handed to the caller, so a drag
    /// that is still being swept cannot be reported — or acted on — twice.
    reported: bool,
}

/// Owns the platform drag source and every drag still in flight.
///
/// # Threading
///
/// Main thread only, like everything else in [`crate::gui::App`]. The platform
/// source is built lazily on first use for exactly that reason: on macOS it
/// touches AppKit, which must not happen on the capture worker.
pub struct DragHost {
    source: Option<NativeDragSource>,
    /// `None` until the window exists. A drag asked for before then is refused
    /// with an explanation rather than silently doing nothing.
    surface: Option<NativeSurface>,
    live: HashMap<CardId, InFlight>,
    /// Replaced sessions whose outcomes must no longer affect the card, but
    /// whose retained files still need sweeping.
    retired: Vec<DragSession>,
    root: PathBuf,
    /// Set once the platform source has been tried and found wanting, so a
    /// hopeless platform is explained once rather than on every gesture.
    unavailable: Option<String>,
}

impl Default for DragHost {
    fn default() -> Self {
        Self::new()
    }
}

impl DragHost {
    /// A host with no window yet and nothing in flight.
    #[must_use]
    pub fn new() -> Self {
        Self {
            source: None,
            surface: None,
            live: HashMap::new(),
            retired: Vec::new(),
            root: artifact::artifact_root(),
            unavailable: None,
        }
    }

    /// Deletes drag files left behind by a previous run.
    ///
    /// A drag that was in flight when the process died leaves its file on disk,
    /// because the whole point of the retention window is that nothing deletes
    /// it early. Nothing will ever come back for it either, so it is swept at
    /// start-up. Returns how many were removed, for the log.
    pub fn sweep_orphans(&self) -> usize {
        artifact::sweep_orphans(
            &self.root,
            std::time::SystemTime::now(),
            artifact::ORPHAN_MAX_AGE,
        )
    }

    /// Tells the host which native surface drags start from.
    ///
    /// Called once the window exists. Until then [`Self::begin`] refuses.
    pub fn attach(&mut self, surface: NativeSurface) {
        self.surface = Some(surface);
    }

    /// Whether a window has been attached.
    #[must_use]
    pub const fn is_attached(&self) -> bool {
        self.surface.is_some()
    }

    /// How many drags are still waiting for an outcome or a sweep.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.live.len() + self.retired.len()
    }

    /// Starts a native drag carrying `card`'s capture.
    ///
    /// # Errors
    ///
    /// Returns a human-readable explanation if there is no window yet, no
    /// capture bytes for the card, or the platform refused to start a session.
    /// Every one of those leaves the card where it was.
    pub fn begin(
        &mut self,
        card: CardId,
        spot: DragSpot,
        bytes: &CaptureBytes,
    ) -> Result<(), String> {
        if let Some(why) = &self.unavailable {
            return Err(why.clone());
        }
        let surface = self
            .surface
            .ok_or_else(|| "the overlay window is not on screen yet".to_owned())?;

        // Re-dragging a card whose previous drag is still settling is fine, but
        // the old session's file must not be adopted by the new one.
        self.retire_existing(card);

        let payload = payload_for(card, bytes, spot);
        let source = self.source()?;
        match source.begin(payload, spot.origin(surface)) {
            Ok(session) => {
                self.live.insert(
                    card,
                    InFlight {
                        session,
                        reported: false,
                    },
                );
                Ok(())
            }
            Err(err) => Err(err.to_string()),
        }
    }

    /// Services every drag in flight. Never blocks.
    ///
    /// Returns **every** drag that has reached an outcome, with that outcome,
    /// exactly once each. Not just the accepted ones: a cancelled, refused or
    /// failed drag has to be reported too, because the surface that armed the
    /// gesture is still holding it. The native drag loop is modal and can
    /// swallow the mouse-up, so "the user let go" is not a signal the surface
    /// can be relied on to see for itself — this is the signal.
    ///
    /// Only [`DragOutcome::Accepted`] should take a card off the stack.
    pub fn poll(&mut self) -> Vec<(CardId, DragOutcome)> {
        let mut settled = Vec::new();

        self.live.retain(|card, flight| {
            if let Some(outcome) = flight.session.outcome()
                && !flight.reported
            {
                flight.reported = true;
                settled.push((*card, outcome));
            }

            // Keep servicing after the outcome: the file is deliberately still
            // on disk, and `sweep` is what eventually removes it.
            flight.session.sweep();
            !flight.session.is_settled()
        });
        self.retired.retain(|session| {
            session.sweep();
            !session.is_settled()
        });

        settled
    }

    /// What happened to `card`'s drag, if it has finished.
    #[must_use]
    pub fn outcome(&self, card: CardId) -> Option<DragOutcome> {
        self.live.get(&card).and_then(|f| f.session.outcome())
    }

    /// Adopts an already-finished session, as though the platform had run it.
    ///
    /// The only way to exercise outcome handling without a real drag, and a
    /// real drag is not something a test suite may start: it seizes the
    /// machine's pointer for as long as the button is held.
    #[cfg(test)]
    pub(super) fn adopt_finished(&mut self, card: CardId, outcome: DragOutcome) {
        self.live.insert(
            card,
            InFlight {
                session: DragSession::finished(outcome),
                reported: false,
            },
        );
    }

    fn retire_existing(&mut self, card: CardId) {
        if let Some(previous) = self.live.remove(&card) {
            self.retired.push(previous.session);
        }
    }

    fn source(&mut self) -> Result<&NativeDragSource, String> {
        if self.source.is_none() {
            match scrozz_shell::native_drag_source() {
                Ok(source) => self.source = Some(source),
                Err(err) => {
                    let why = err.to_string();
                    self.unavailable = Some(why.clone());
                    return Err(why);
                }
            }
        }
        Ok(self
            .source
            .as_ref()
            .expect("the source was just constructed"))
    }
}

/// Physical edge cap for a drag image prepared synchronously at hand-off.
const PREVIEW_MAX_EDGE: f64 = 1024.0;
const PREVIEW_SCALE: f64 = 2.0;

/// Builds the payload for one card.
fn payload_for(card: CardId, bytes: &CaptureBytes, spot: DragSpot) -> DragPayload {
    let full = Arc::clone(&bytes.full);
    let png = byte_source(move || Ok(full.as_ref().clone()));
    let mut payload = DragPayload::png_capture(&stem_for(card), png);

    if let Some(preview) = &bytes.preview {
        match preview_for_card(preview, spot) {
            Ok(preview) => payload = payload.with_preview(preview),
            Err(error) => {
                tracing::warn!(%error, "drag: card-matched preview could not be built");
            }
        }
    }
    payload
}

fn preview_for_card(png: &[u8], spot: DragSpot) -> scrozz_core::Result<DragPreview> {
    let [_, _, width, height] = spot.card;
    let size = LogicalSize::new(f64::from(width), f64::from(height));
    if size.is_empty() || !size.width.is_finite() || !size.height.is_finite() {
        return Err(scrozz_core::Error::InvalidRequest(
            "drag card geometry cannot produce a preview".to_owned(),
        ));
    }

    let scale = PREVIEW_SCALE.min(PREVIEW_MAX_EDGE / size.width.max(size.height));
    let pixel_width = (size.width * scale).round().max(1.0) as u32;
    let pixel_height = (size.height * scale).round().max(1.0) as u32;
    let decoded = image::load_from_memory(png)
        .map_err(|error| scrozz_core::Error::Codec(format!("preview PNG decode failed: {error}")))?
        .into_rgba8();
    let source_width = decoded.width();
    let source_height = decoded.height();
    let source_aspect = f64::from(source_width) / f64::from(source_height);
    let target_aspect = f64::from(pixel_width) / f64::from(pixel_height);
    let (crop_x, crop_y, crop_width, crop_height) = if source_aspect > target_aspect {
        let crop_width = (f64::from(source_height) * target_aspect)
            .round()
            .clamp(1.0, f64::from(source_width)) as u32;
        (
            (source_width - crop_width) / 2,
            0,
            crop_width,
            source_height,
        )
    } else {
        let crop_height = (f64::from(source_width) / target_aspect)
            .round()
            .clamp(1.0, f64::from(source_height)) as u32;
        (
            0,
            (source_height - crop_height) / 2,
            source_width,
            crop_height,
        )
    };
    let cropped =
        image::imageops::crop_imm(&decoded, crop_x, crop_y, crop_width, crop_height).to_image();
    let mut fitted =
        image::imageops::resize(&cropped, pixel_width, pixel_height, FilterType::Lanczos3);
    round_preview_corners(
        fitted.as_mut(),
        pixel_width,
        pixel_height,
        f64::from(scrozz_ui::card::CardChrome::OUTER_RADIUS) * scale,
    );
    let image = RgbaImage {
        width: pixel_width,
        height: pixel_height,
        data: fitted.into_raw(),
    };
    let png =
        FrameEncoder::new().encode_rgba(&image, scrozz_core::ColorSpace::Srgb, ImageFormat::Png)?;
    DragPreview::from_png(png, size)
}

fn round_preview_corners(pixels: &mut [u8], width: u32, height: u32, radius: f64) {
    let radius = radius
        .clamp(0.0, f64::from(width.min(height)) / 2.0)
        .max(0.5);
    let right = f64::from(width) - radius;
    let bottom = f64::from(height) - radius;
    for y in 0..height {
        let py = f64::from(y) + 0.5;
        let dy = if py < radius {
            radius - py
        } else if py > bottom {
            py - bottom
        } else {
            0.0
        };
        for x in 0..width {
            let px = f64::from(x) + 0.5;
            let dx = if px < radius {
                radius - px
            } else if px > right {
                px - right
            } else {
                0.0
            };
            if dx == 0.0 || dy == 0.0 {
                continue;
            }
            let coverage = (radius + 0.5 - dx.hypot(dy)).clamp(0.0, 1.0);
            let alpha = &mut pixels[(y as usize * width as usize + x as usize) * 4 + 3];
            *alpha = (f64::from(*alpha) * coverage).round() as u8;
        }
    }
}

/// The filename a drop target will see, without its extension.
///
/// Deliberately not the card number: a file called `card-3.png` in a chat is
/// meaningless a week later, and every screenshot tool in the world has settled
/// on a timestamp for the same reason.
fn stem_for(card: CardId) -> String {
    let _ = card;
    scrozz_export::NameTemplate::parse("Scrozz {date} at {time}").map_or_else(
        |_| "Scrozz capture".to_owned(),
        |template| {
            template.render(
                &scrozz_export::NamingContext::now(),
                &scrozz_export::NamePolicy::default(),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbaImage {
            width,
            height,
            data: (0..width * height)
                .flat_map(|_| [30, 180, 220, 255])
                .collect(),
        };
        FrameEncoder::new()
            .encode_rgba(&image, scrozz_core::ColorSpace::Srgb, ImageFormat::Png)
            .expect("encode fixture")
    }

    fn spot() -> DragSpot {
        DragSpot {
            card: [10.0, 20.0, 210.0, 150.0],
            pointer: [80.0, 90.0],
        }
    }

    fn capture() -> CaptureBytes {
        CaptureBytes {
            generation: None,
            revision: 0,
            full: Arc::new(b"full-resolution-png".to_vec()),
            preview: Some(Arc::new(png(400, 200))),
        }
    }

    #[test]
    fn a_drag_preview_matches_the_card_size_crop_and_rounding() {
        let preview = preview_for_card(&png(400, 200), spot()).expect("preview");
        assert_eq!(preview.size(), LogicalSize::new(210.0, 150.0));

        let frame = scrozz_export::decode(preview.png()).expect("decode preview");
        assert_eq!((frame.width(), frame.height()), (420, 300));
        let image = scrozz_export::to_straight_rgba8(&frame).expect("straight pixels");
        assert_eq!(image.pixel(0, 0).expect("corner")[3], 0);
        assert_eq!(image.pixel(210, 150), Some([30, 180, 220, 255]));
    }

    #[test]
    fn extreme_aspect_previews_crop_before_bounded_resize() {
        let preview = preview_for_card(&png(1, 512), spot()).expect("thin preview");
        let frame = scrozz_export::decode(preview.png()).expect("decode preview");
        assert_eq!((frame.width(), frame.height()), (420, 300));
        assert!(preview.png().len() < 420 * 300 * 4);
    }

    #[test]
    fn wide_previews_are_center_cropped_like_the_visible_card() {
        let width = 400;
        let height = 200;
        let image = RgbaImage {
            width,
            height,
            data: (0..height)
                .flat_map(|_| {
                    (0..width).flat_map(|x| {
                        if !(50..350).contains(&x) {
                            [220, 20, 20, 255]
                        } else {
                            [20, 220, 80, 255]
                        }
                    })
                })
                .collect(),
        };
        let png = FrameEncoder::new()
            .encode_rgba(&image, scrozz_core::ColorSpace::Srgb, ImageFormat::Png)
            .expect("encode fixture");
        let preview = preview_for_card(&png, spot()).expect("preview");
        let frame = scrozz_export::decode(preview.png()).expect("decode preview");
        let image = scrozz_export::to_straight_rgba8(&frame).expect("straight pixels");

        assert_eq!(image.pixel(0, 150), Some([20, 220, 80, 255]));
        assert_eq!(image.pixel(419, 150), Some([20, 220, 80, 255]));
    }

    #[test]
    fn the_payload_offers_the_full_resolution_bytes_as_a_png_file() {
        let bytes = capture();
        let payload = payload_for(CardId(7), &bytes, spot());

        assert!(
            payload.file().file_name().ends_with(".png"),
            "a drop target decides what it is from the extension"
        );
        let produced = payload.file().produce().expect("the bytes are in hand");
        assert_eq!(produced, *bytes.full, "the file must be the full capture");
    }

    #[test]
    fn the_payload_offers_the_same_bytes_as_image_data() {
        let bytes = capture();
        let payload = payload_for(CardId(1), &bytes, spot());

        let image = payload.image().expect("an image flavour is offered");
        assert_eq!(
            image().expect("the image bytes are in hand"),
            *bytes.full,
            "an app that pastes rather than saves must get the same picture"
        );
    }

    #[test]
    fn the_drag_image_is_a_card_matched_render_of_the_current_thumbnail() {
        let bytes = capture();
        let payload = payload_for(CardId(1), &bytes, spot());

        let preview = payload.preview_png().expect("a preview is offered");
        assert_ne!(preview, bytes.full.as_slice());
        assert_eq!(
            payload.preview().expect("preview").size(),
            LogicalSize::new(210.0, 150.0)
        );
    }

    #[test]
    fn a_capture_without_a_thumbnail_still_drags() {
        let bytes = CaptureBytes {
            generation: None,
            revision: 0,
            full: Arc::new(b"png".to_vec()),
            preview: None,
        };
        let payload = payload_for(CardId(1), &bytes, spot());

        assert!(payload.preview().is_none());
        assert!(
            payload.image().is_some(),
            "a missing picture costs the drag its looks, not its payload"
        );
    }

    #[test]
    fn a_drag_asked_for_before_the_window_exists_is_refused_not_ignored() {
        let mut host = DragHost::new();
        let spot = DragSpot {
            card: [0.0, 0.0, 100.0, 60.0],
            pointer: [10.0, 10.0],
        };

        let refusal = host
            .begin(CardId(1), spot, &capture())
            .expect_err("there is no window");
        assert!(
            refusal.contains("window"),
            "the refusal must say why: {refusal}"
        );
        assert_eq!(host.in_flight(), 0);
    }

    #[test]
    fn a_spot_becomes_window_local_geometry() {
        let spot = DragSpot {
            card: [12.0, 34.0, 56.0, 78.0],
            pointer: [20.0, 40.0],
        };
        // Safety: never dereferenced — `DragOrigin` only carries the handle.
        let surface = unsafe { NativeSurface::from_raw(std::ptr::null_mut()) };
        let origin = spot.origin(surface);

        assert!((origin.card().origin.x - 12.0).abs() < f64::EPSILON);
        assert!((origin.card().size.height - 78.0).abs() < f64::EPSILON);
        assert!((origin.pointer().y - 40.0).abs() < f64::EPSILON);
    }

    #[test]
    fn replacing_a_drag_keeps_sweeping_its_retained_artifact() {
        let root =
            std::env::temp_dir().join(format!("scrozz-retired-drag-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let artifact =
            artifact::DragArtifact::materialise(&root, "capture.png", b"sensitive").unwrap();
        let path = artifact.path().to_owned();
        let session = DragSession::new();
        session.attach_artifact(artifact);
        session.finish(DragOutcome::Accepted(scrozz_shell::DragOperation::Copy));
        let mut host = DragHost::new();
        host.live.insert(
            CardId(1),
            InFlight {
                session,
                reported: false,
            },
        );

        host.retire_existing(CardId(1));

        assert_eq!(host.live.len(), 0);
        assert_eq!(host.retired.len(), 1);
        assert_eq!(host.in_flight(), 1);
        assert!(
            host.poll().is_empty(),
            "a replaced session's old outcome must not affect the new drag"
        );
        assert!(path.exists(), "its retention window still owns the file");
        drop(host);
        let _ = std::fs::remove_dir_all(root);
    }
}
