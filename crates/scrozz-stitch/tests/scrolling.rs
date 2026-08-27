//! Deterministic end-to-end scrolling-capture fixtures.

mod common;

use std::collections::VecDeque;

use scrozz_core::{Provenance, ScaleFactor};
use scrozz_stitch::{
    AlignError, AlignmentConfig, CancelAction, CancelSignal, ChromeBands, ChromeConfig,
    CompletionReason, LumaPlane, NeverCancel, NoopPacer, PushOutcome, ScrollSession,
    ScrollStitcher, align_vertical, detect_sticky_chrome,
};

use common::{FixtureDriver, FixtureSource, compact_stitch, gray_frame, session_config, viewport};

#[test]
fn repeated_rows_need_the_expected_delta_prior() {
    let rows: Vec<u8> = (0..30)
        .map(|row| if row % 6 < 3 { 30 } else { 210 })
        .collect();
    let first = LumaPlane::from_frame(&gray_frame(&rows[0..12], 12, 1.0)).unwrap();
    let second = LumaPlane::from_frame(&gray_frame(&rows[6..18], 12, 1.0)).unwrap();
    let config = AlignmentConfig {
        min_overlap: 4,
        row_buckets: 6,
        basin_radius: 2,
        min_confidence: 1,
        ..AlignmentConfig::default()
    };

    assert!(matches!(
        align_vertical(&first, &second, None, &config),
        Err(AlignError::Ambiguous { .. })
    ));
    let resolved = align_vertical(&first, &second, Some(6), &config).expect("prior");
    assert_eq!(resolved.delta, 6);
    assert_eq!(resolved.confidence, 0);

    let long_rows: Vec<u8> = (0..400).map(|row| ((row % 20) * 11) as u8).collect();
    let plane = |rows: &[u8]| {
        LumaPlane::from_raw(
            4,
            rows.len() as u32,
            rows.iter()
                .flat_map(|&value| std::iter::repeat_n(value, 4))
                .collect(),
        )
    };
    let long_first = plane(&long_rows[0..200]);
    let long_second = plane(&long_rows[80..280]);
    let long_config = AlignmentConfig {
        min_overlap: 64,
        row_buckets: 4,
        basin_radius: 3,
        min_confidence: 1,
        ..AlignmentConfig::default()
    };
    assert!(matches!(
        align_vertical(&long_first, &long_second, None, &long_config),
        Err(AlignError::Ambiguous { .. })
    ));
    assert_eq!(
        align_vertical(&long_first, &long_second, Some(80), &long_config)
            .expect("long-frame prior")
            .delta,
        80
    );
}

#[test]
fn sticky_chrome_is_removed_instead_of_repeated() {
    let document: Vec<u8> = (0..20).map(|row| 40 + row * 8).collect();
    let first = viewport(&document, 0, 8, &[3, 17], &[229, 247], 10, 1.0);
    let second = viewport(&document, 3, 8, &[3, 17], &[229, 247], 10, 1.0);
    let first_luma = LumaPlane::from_frame(&first).unwrap();
    let second_luma = LumaPlane::from_frame(&second).unwrap();

    assert_eq!(
        detect_sticky_chrome(
            &first_luma,
            &second_luma,
            3,
            &ChromeConfig {
                min_band: 2,
                ..ChromeConfig::default()
            }
        ),
        ChromeBands { top: 2, bottom: 2 }
    );

    let mut stitcher = ScrollStitcher::new(compact_stitch(Some(3)));
    assert_eq!(stitcher.push_frame(first).unwrap(), PushOutcome::Started);
    let outcome = stitcher.push_frame(second).unwrap();
    assert!(
        matches!(
            &outcome,
            PushOutcome::Advanced {
                delta: 3,
                output_height: 11,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(stitcher.summary().chrome, ChromeBands { top: 2, bottom: 2 });
    let output = stitcher.finish_frame().unwrap();
    assert_eq!(output.height(), 11);
    let first_channel: Vec<u8> = output
        .data
        .as_chunks::<4>()
        .0
        .iter()
        .step_by(output.width() as usize)
        .map(|pixel| pixel[0])
        .collect();
    assert!(
        first_channel
            .iter()
            .all(|row| ![3, 17, 229, 247].contains(row)),
        "fixed rows leaked into the canvas: {first_channel:?}"
    );
}

#[test]
fn unrelated_frames_report_insufficient_overlap() {
    let mut config = compact_stitch(Some(4));
    config.alignment.max_mean_error = 5;
    let mut stitcher = ScrollStitcher::new(config);
    stitcher
        .push_frame(gray_frame(&[0, 1, 2, 3, 4, 5, 6, 7], 8, 1.0))
        .unwrap();
    let outcome = stitcher
        .push_frame(gray_frame(
            &[240, 241, 242, 243, 244, 245, 246, 247],
            8,
            1.0,
        ))
        .unwrap();
    assert!(matches!(outcome, PushOutcome::InsufficientOverlap { .. }));
}

#[test]
fn fractional_scale_and_nonzero_seam_quality_survive_the_stitch() {
    let document: Vec<u8> = (0..20).map(|row| 10 + row * 10).collect();
    let first = gray_frame(&document[0..9], 4, 1.25);
    let mut second = gray_frame(&document[3..12], 4, 1.25);
    second.data[0] = second.data[0].saturating_add(80);

    let mut stitcher = ScrollStitcher::new(compact_stitch(Some(3)));
    stitcher.push_frame(first).unwrap();
    let PushOutcome::Advanced { seam, .. } = stitcher.push_frame(second).unwrap() else {
        panic!("the noisy overlap should still align");
    };
    assert!(seam.mean_absolute_error > 0);
    let output = stitcher.finish_frame().unwrap();
    assert_eq!(output.scale, ScaleFactor::new(1.25));
    assert_eq!(output.height(), 12);
}

#[test]
fn stationary_probes_end_the_session_without_duplicate_rows() {
    let document: Vec<u8> = (0..20).map(|row| 10 + row * 10).collect();
    let end = gray_frame(&document[3..11], 8, 1.0);
    let source = FixtureSource {
        frames: VecDeque::from([
            gray_frame(&document[0..8], 8, 1.0),
            end.clone(),
            end.clone(),
            end,
        ]),
    };
    let output = ScrollSession::new(
        source,
        Box::<FixtureDriver>::default(),
        NoopPacer,
        session_config(3.0, 10),
    )
    .run(&mut NeverCancel, |_| {})
    .expect("end-of-content");

    assert_eq!(output.reason, CompletionReason::EndOfContent);
    assert_eq!(output.frame.height(), 11);
    assert_eq!(output.captured_frames, 4);
    let capture = output.into_capture(scrozz_core::CaptureTarget::AllDisplays);
    assert_eq!(capture.provenance, Provenance::Stitched);
}

struct CancelOnSecondBoundary {
    polls: usize,
    action: CancelAction,
}

impl CancelSignal for CancelOnSecondBoundary {
    fn cancellation(&mut self) -> Option<CancelAction> {
        self.polls += 1;
        (self.polls == 2).then_some(self.action)
    }
}

#[test]
fn cancellation_keeps_or_aborts_the_same_partial_capture() {
    let document: Vec<u8> = (0..20).map(|row| 10 + row * 10).collect();
    let frames = || FixtureSource {
        frames: VecDeque::from([
            gray_frame(&document[0..8], 8, 1.0),
            gray_frame(&document[3..11], 8, 1.0),
        ]),
    };

    let kept = ScrollSession::new(
        frames(),
        Box::<FixtureDriver>::default(),
        NoopPacer,
        session_config(3.0, 10),
    )
    .run(
        &mut CancelOnSecondBoundary {
            polls: 0,
            action: CancelAction::Keep,
        },
        |_| {},
    )
    .expect("keep");
    assert_eq!(kept.reason, CompletionReason::CancelledKeep);
    assert_eq!(kept.frame.height(), 11);

    let aborted = ScrollSession::new(
        frames(),
        Box::<FixtureDriver>::default(),
        NoopPacer,
        session_config(3.0, 10),
    )
    .run(
        &mut CancelOnSecondBoundary {
            polls: 0,
            action: CancelAction::Abort,
        },
        |_| {},
    )
    .expect_err("abort");
    assert!(aborted.is_cancellation());
}
