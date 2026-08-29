//! Host seam for local sensitive-information review.
//!
//! The default is disabled. An After Capture configuration can request a
//! confirmation prompt, but cannot itself run OCR or open review UI. Only the
//! explicitly named [`SensitiveAnalysis::scan_confirmed`] entry point performs
//! work after the host has received user confirmation.

use scrozz_annotate::Document;
use scrozz_core::Result;
use scrozz_ocr::{
    CancellationToken, LocalSensitiveDetector, Ocr, SensitiveScanCache, SensitiveScanOptions,
    SensitiveSource,
};
use scrozz_ui::SensitiveReview;

/// Optional After Capture behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SensitiveAfterCaptureAction {
    /// Do nothing.
    #[default]
    Disabled,
    /// Offer a review; scanning still requires explicit confirmation.
    AskToScan,
}

/// What the host should do after a capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SensitiveAfterCapturePlan {
    /// No action.
    Disabled,
    /// Ask before running OCR and opening review.
    ConfirmationRequired {
        /// Visible action label.
        label: &'static str,
        /// Accessibility description.
        accessibility_label: &'static str,
    },
}

impl SensitiveAfterCaptureAction {
    /// Converts configuration into a non-executing host plan.
    #[must_use]
    pub const fn plan(self) -> SensitiveAfterCapturePlan {
        match self {
            Self::Disabled => SensitiveAfterCapturePlan::Disabled,
            Self::AskToScan => SensitiveAfterCapturePlan::ConfirmationRequired {
                label: "Scan for sensitive information",
                accessibility_label: "Ask before scanning this capture locally for information you may want to redact",
            },
        }
    }
}

/// Local detector and raw-free revision cache.
#[derive(Debug)]
pub struct SensitiveAnalysis {
    detector: LocalSensitiveDetector,
    cache: SensitiveScanCache,
    cached_options: Option<SensitiveScanOptions>,
    cached_profile: Option<SensitiveAnalysisProfile>,
}

/// Identity of the OCR/settings profile that produced a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SensitiveAnalysisProfile(u64);

impl SensitiveAnalysisProfile {
    /// Creates a profile identity from the host's settings revision.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }
}

impl SensitiveAnalysis {
    /// Creates the bounded analysis service.
    ///
    /// # Errors
    ///
    /// Returns an error for zero cache capacity.
    pub fn new(cache_capacity: usize) -> Result<Self> {
        Ok(Self {
            detector: LocalSensitiveDetector::new(),
            cache: SensitiveScanCache::new(cache_capacity)?,
            cached_options: None,
            cached_profile: None,
        })
    }

    /// Scans the exact current document revision after explicit confirmation.
    ///
    /// Repeated calls for an unchanged revision reuse only raw-free findings.
    /// The review remains inert until its selections are applied.
    ///
    /// # Errors
    ///
    /// Propagates OCR, malformed-frame, cancellation, and limit failures.
    pub fn scan_confirmed(
        &mut self,
        document: &Document,
        ocr: &dyn Ocr,
        profile: SensitiveAnalysisProfile,
        options: &SensitiveScanOptions,
        cancellation: &CancellationToken,
    ) -> Result<SensitiveReview> {
        if self.cached_options.as_ref() != Some(options) || self.cached_profile != Some(profile) {
            self.cache.clear();
            self.cached_options = Some(options.clone());
            self.cached_profile = Some(profile);
        }
        let revision = document.revision();
        let scan = if let Some(scan) = self.cache.get(revision) {
            (*scan).clone()
        } else {
            let scan = self.detector.scan_frame(
                ocr,
                &document.source().frame,
                revision,
                SensitiveSource::Image,
                options,
                cancellation,
            )?;
            (*self.cache.insert(scan)).clone()
        };
        Ok(SensitiveReview::new(scan, document.logical_size()))
    }

    /// Clears raw-free cached results.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.cached_options = None;
        self.cached_profile = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use scrozz_annotate::Annotation;
    use scrozz_core::{
        Capture, CaptureTarget, ColorSpace, Frame, LogicalPoint, LogicalRect, LogicalSize,
        PhysicalSize, PixelFormat, Provenance, ScaleFactor,
    };
    use scrozz_ocr::TextBlock;

    use super::*;

    struct FixedOcr {
        calls: Arc<AtomicUsize>,
    }

    impl Ocr for FixedOcr {
        fn recognize(&self, _frame: &Frame) -> Result<Vec<TextBlock>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(vec![TextBlock {
                text: "Email ada@example.invalid".to_owned(),
                bounds: LogicalRect::new(
                    LogicalPoint::new(10.0, 10.0),
                    LogicalSize::new(200.0, 20.0),
                ),
                confidence: 1.0,
            }])
        }
    }

    fn document() -> Document {
        Document::new(Capture {
            frame: Frame {
                data: vec![255; 320 * 200 * 4],
                size: PhysicalSize::new(320.0, 200.0),
                stride: 320 * 4,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::Srgb,
                scale: ScaleFactor::IDENTITY,
            },
            provenance: Provenance::Region,
            target: CaptureTarget::Region(LogicalRect::new(
                LogicalPoint::new(0.0, 0.0),
                LogicalSize::new(320.0, 200.0),
            )),
        })
    }

    #[test]
    fn after_capture_is_disabled_by_default() {
        assert_eq!(
            SensitiveAfterCaptureAction::default().plan(),
            SensitiveAfterCapturePlan::Disabled
        );
        assert!(matches!(
            SensitiveAfterCaptureAction::AskToScan.plan(),
            SensitiveAfterCapturePlan::ConfirmationRequired { .. }
        ));
    }

    #[test]
    fn confirmed_scans_are_keyed_to_the_document_revision() {
        let mut analysis = SensitiveAnalysis::new(2).unwrap();
        let mut document = document();
        let calls = Arc::new(AtomicUsize::new(0));
        let ocr = FixedOcr {
            calls: Arc::clone(&calls),
        };
        let first = analysis
            .scan_confirmed(
                &document,
                &ocr,
                SensitiveAnalysisProfile::new(1),
                &SensitiveScanOptions::default(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(first.scan().revision(), document.revision());
        let cached = analysis
            .scan_confirmed(
                &document,
                &ocr,
                SensitiveAnalysisProfile::new(1),
                &SensitiveScanOptions::default(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(cached.scan().revision(), first.scan().revision());
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        document.add_default(Annotation::Rectangle(LogicalRect::new(
            LogicalPoint::new(0.0, 0.0),
            LogicalSize::new(10.0, 10.0),
        )));
        let second = analysis
            .scan_confirmed(
                &document,
                &ocr,
                SensitiveAnalysisProfile::new(1),
                &SensitiveScanOptions::default(),
                &CancellationToken::new(),
            )
            .unwrap();
        assert_ne!(first.scan().revision(), second.scan().revision());
        assert_eq!(calls.load(Ordering::Relaxed), 2);

        analysis
            .scan_confirmed(
                &document,
                &ocr,
                SensitiveAnalysisProfile::new(1),
                &SensitiveScanOptions {
                    include_review_confidence: false,
                    ..SensitiveScanOptions::default()
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            3,
            "changing scan policy must invalidate the cached result"
        );

        analysis
            .scan_confirmed(
                &document,
                &ocr,
                SensitiveAnalysisProfile::new(2),
                &SensitiveScanOptions {
                    include_review_confidence: false,
                    ..SensitiveScanOptions::default()
                },
                &CancellationToken::new(),
            )
            .unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            4,
            "changing OCR/settings profile must invalidate the cached result"
        );
    }
}
