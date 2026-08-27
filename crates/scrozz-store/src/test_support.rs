//! Fixtures and scratch directories shared by unit and integration tests.
//!
//! Exposed rather than `#[cfg(test)]` so the integration tests in `tests/` can
//! use the same helpers as the unit tests without a second copy. It is
//! `#[doc(hidden)]` and carries no stability promise.
//!
//! # Scratch directories are inside the build directory, never in the real one
//!
//! A test that writes to the platform data directory would put fabricated rows
//! into the user's actual capture history, and a test that then enforces
//! retention would delete their screenshots. Every helper here roots itself
//! under the test binary's own directory in `target/`, which is disposable by
//! definition, and removes it again on drop.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use scrozz_annotate::{Annotation, Background, Beautification, Document, Style};
use scrozz_core::{
    Capture, CaptureTarget, ColorSpace, DisplayId, Frame, LogicalPoint, LogicalRect, LogicalSize,
    PhysicalSize, PixelFormat, Provenance, ScaleFactor, WindowId,
};

use crate::{
    CaptureId,
    model::{FrameHeader, MediaKind, Timestamp},
    record::StoredRecord,
};

static SCRATCH_SEQ: AtomicU64 = AtomicU64::new(0);

/// A temporary directory that deletes itself.
#[derive(Debug)]
pub struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Gives up ownership, leaving the directory behind for inspection.
    #[must_use]
    pub fn keep(mut self) -> PathBuf {
        let path = std::mem::take(&mut self.path);
        std::mem::forget(self);
        path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if !self.path.as_os_str().is_empty() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Creates a unique scratch directory beneath the test binary's own directory.
///
/// # Panics
///
/// Panics if the directory cannot be created, which means the test could not
/// have run anyway.
#[must_use]
pub fn scratch_dir(label: &str) -> ScratchDir {
    let base = scratch_root();
    let unique = format!(
        "{label}-{}-{}",
        std::process::id(),
        SCRATCH_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let path = base.join(unique);
    fs::create_dir_all(&path).expect("scratch directory must be creatable");
    ScratchDir { path }
}

/// The directory scratch space is carved out of: alongside the test binary.
fn scratch_root() -> PathBuf {
    let exe = std::env::current_exe().expect("a test binary always has a path");
    // `<target>/<profile>/deps/<binary>` — two levels up is the profile
    // directory, which cargo owns and `cargo clean` removes.
    let profile_dir = exe
        .parent()
        .and_then(Path::parent)
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    profile_dir.join("scrozz-store-scratch")
}

/// A frame of `width` × `height` filled with a `seed`-dependent pattern.
///
/// Distinct seeds produce distinct bytes, so content addressing can be observed
/// deduplicating equal frames and separating different ones.
#[must_use]
pub fn sample_frame(width: u32, height: u32, seed: u8) -> Frame {
    let stride = width as usize * 4;
    let mut data = vec![0u8; stride * height as usize];
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = ((i as u32).wrapping_mul(31).wrapping_add(u32::from(seed)) % 251) as u8;
    }
    Frame {
        data,
        size: PhysicalSize::new(f64::from(width), f64::from(height)),
        stride,
        format: PixelFormat::Rgba8,
        color_space: ColorSpace::Srgb,
        scale: ScaleFactor::new(2.0),
    }
}

/// A window capture of the given size and content.
#[must_use]
pub fn sample_capture(width: u32, height: u32, seed: u8) -> Capture {
    Capture {
        frame: sample_frame(width, height, seed),
        provenance: Provenance::Window,
        target: CaptureTarget::Window(WindowId(format!("window-{seed}"))),
    }
}

/// A display capture, which unlike a window capture may be beautified.
#[must_use]
pub fn sample_display_capture(width: u32, height: u32, seed: u8) -> Capture {
    Capture {
        frame: sample_frame(width, height, seed),
        provenance: Provenance::Display,
        target: CaptureTarget::Display(DisplayId("built-in".into())),
    }
}

/// A document with `annotations` arrows on a `width` × `height` capture.
#[must_use]
pub fn sample_document(width: u32, height: u32, seed: u8, annotations: usize) -> Document {
    let mut document = Document::new(sample_capture(width, height, seed));
    for i in 0..annotations {
        document
            .add(
                Annotation::Arrow {
                    from: LogicalPoint::new(i as f64, 0.0),
                    to: LogicalPoint::new(i as f64 + 10.0, 10.0),
                },
                Style::stroked(),
            )
            .expect("annotation id space available");
    }
    document
}

/// A document carrying one of each simple annotation kind, plus framing.
///
/// Built on a *display* capture because decision D9 forbids beautifying a
/// window capture, and this fixture beautifies.
#[must_use]
pub fn richly_annotated_document(seed: u8) -> Document {
    let mut document = Document::new(sample_display_capture(32, 16, seed));
    document
        .add(
            Annotation::Rectangle(LogicalRect::new(
                LogicalPoint::new(1.0, 2.0),
                LogicalSize::new(3.0, 4.0),
            )),
            Style::stroked(),
        )
        .expect("annotation id space available");
    document
        .add_default(Annotation::Text {
            at: LogicalPoint::new(5.0, 6.0),
            content: "look here".into(),
        })
        .expect("annotation id space available");
    document
        .add_default(Annotation::Counter {
            at: LogicalPoint::new(7.0, 8.0),
            index: 1,
        })
        .expect("annotation id space available");
    document
        .set_beautification(Some(Beautification::padded(24.0, Background::default())))
        .expect("display captures may be beautified");
    document
}

/// A minimal durable record, for tests that need one without a store.
///
/// # Panics
///
/// Panics if the record cannot be built, which would be a bug in this helper.
#[must_use]
pub fn sample_record(app: &str, created_at: i64) -> StoredRecord {
    StoredRecord::from_parts(
        &crate::id::capture_id_at(created_at),
        Timestamp(created_at),
        Timestamp(created_at),
        MediaKind::Screenshot,
        false,
        Some(app.to_owned()),
        Some(format!("{app} — window")),
        Provenance::Window,
        &CaptureTarget::Window(WindowId("1".into())),
        FrameHeader {
            size: PhysicalSize::new(4.0, 2.0),
            stride: 16,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::Srgb,
            scale: ScaleFactor::IDENTITY,
        },
        None,
        0,
        None,
        &scrozz_annotate::DocumentData::default(),
    )
    .expect("sample record must build")
}

/// The identifier a capture inserted at `unix_millis` would sort as.
#[must_use]
pub fn id_at(unix_millis: i64) -> CaptureId {
    crate::id::capture_id_at(unix_millis)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_directories_live_under_the_build_directory() {
        let dir = scratch_dir("meta");
        let path = dir.path().to_path_buf();
        assert!(path.is_dir());
        assert!(
            path.to_string_lossy().contains("scrozz-store-scratch"),
            "scratch must be isolated: {path:?}"
        );
        drop(dir);
        assert!(!path.exists(), "scratch must clean up after itself");
    }

    #[test]
    fn scratch_directories_are_unique() {
        let a = scratch_dir("unique");
        let b = scratch_dir("unique");
        assert_ne!(a.path(), b.path());
    }

    #[test]
    fn distinct_seeds_produce_distinct_pixels() {
        assert_ne!(sample_frame(4, 4, 1).data, sample_frame(4, 4, 2).data);
        assert_eq!(sample_frame(4, 4, 1).data, sample_frame(4, 4, 1).data);
    }

    #[test]
    fn sample_frames_are_well_formed() {
        assert!(sample_frame(16, 9, 3).is_well_formed());
    }
}
