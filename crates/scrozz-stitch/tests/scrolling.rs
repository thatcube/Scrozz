//! Deterministic end-to-end scrolling-capture fixtures.

mod common;

use std::collections::VecDeque;

use scrozz_core::{ManualScrollDriver, Provenance, ScaleFactor, ScrollAxis};
use scrozz_stitch::{
    AlignError, AlignmentConfig, CancelAction, CancelSignal, ChromeBands, ChromeConfig,
    CompletionReason, LumaPlane, NeverCancel, NoopPacer, PushOutcome, ScrollSession,
    ScrollStitcher, SideChromeBands, align_horizontal, align_vertical, detect_sticky_chrome,
    detect_sticky_side_chrome,
};

use common::{
    FixtureDriver, FixtureSource, compact_stitch, gray_column_frame, gray_frame,
    horizontal_session_config, horizontal_viewport, session_config, viewport,
};

#[test]
fn repeated_rows_remain_ambiguous_with_an_expected_delta_prior() {
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
    assert!(matches!(
        align_vertical(&first, &second, Some(6), &config),
        Err(AlignError::Ambiguous { margin: 0, .. })
    ));

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
    assert!(matches!(
        align_vertical(&long_first, &long_second, Some(80), &long_config),
        Err(AlignError::Ambiguous { margin: 0, .. })
    ));
}

#[test]
fn repeated_columns_remain_ambiguous_with_an_expected_delta_prior() {
    let columns: Vec<u8> = (0..36)
        .map(|column| if column % 6 < 3 { 25 } else { 215 })
        .collect();
    let first = LumaPlane::from_frame(&gray_column_frame(&columns[0..12], 9, 1.0)).expect("first");
    let second =
        LumaPlane::from_frame(&gray_column_frame(&columns[6..18], 9, 1.0)).expect("second");
    let config = AlignmentConfig {
        min_overlap: 4,
        row_buckets: 6,
        basin_radius: 2,
        min_confidence: 1,
        ..AlignmentConfig::default()
    };

    assert!(matches!(
        align_horizontal(&first, &second, None, &config),
        Err(AlignError::Ambiguous { .. })
    ));
    assert!(matches!(
        align_horizontal(&first, &second, Some(6), &config),
        Err(AlignError::Ambiguous { margin: 0, .. })
    ));
}

#[test]
fn fixed_top_and_bottom_toolbars_do_not_pull_horizontal_alignment_to_zero() {
    let columns: Vec<u8> = (0..24).map(|column| 17 + column * 9).collect();
    let mut first = gray_column_frame(&columns[0..10], 10, 1.0);
    let mut second = gray_column_frame(&columns[3..13], 10, 1.0);

    for frame in [&mut first, &mut second] {
        let width = frame.width() as usize;
        let height = frame.height() as usize;
        for y in [0, 1, height - 2, height - 1] {
            for x in 0..width {
                let value = (x as u8).wrapping_mul(19).wrapping_add(y as u8);
                frame.data[y * frame.stride + x * 4..y * frame.stride + x * 4 + 4]
                    .copy_from_slice(&[value, value, value, 255]);
            }
        }
    }

    let first = LumaPlane::from_frame(&first).expect("first");
    let second = LumaPlane::from_frame(&second).expect("second");
    let aligned = align_horizontal(&first, &second, Some(3), &compact_stitch(Some(3)).alignment)
        .expect("content rows should outweigh perpendicular fixed chrome");
    assert_eq!(aligned.delta, 3);
}

#[test]
fn stationary_edges_are_confirmed_at_the_selected_displacement() {
    let columns: Vec<u8> = (0..32)
        .map(|column| if column % 4 < 2 { 30 } else { 210 })
        .collect();
    let mut first = gray_column_frame(&columns[0..12], 10, 1.0);
    let mut second = gray_column_frame(&columns[1..13], 10, 1.0);
    for frame in [&mut first, &mut second] {
        let width = frame.width() as usize;
        let height = frame.height() as usize;
        for y in [0, 1, height - 2, height - 1] {
            for x in 0..width {
                let value = (x as u8).wrapping_mul(3);
                frame.data[y * frame.stride + x * 4..y * frame.stride + x * 4 + 4]
                    .copy_from_slice(&[value, value, value, 255]);
            }
        }
    }
    let first = LumaPlane::from_frame(&first).expect("first");
    let second = LumaPlane::from_frame(&second).expect("second");
    let config = AlignmentConfig {
        min_overlap: 4,
        row_buckets: 6,
        basin_radius: 1,
        min_confidence: 1,
        ..AlignmentConfig::default()
    };

    let alignment = align_horizontal(&first, &second, Some(1), &config)
        .expect("unrelated displacement probes must not manufacture ambiguity");
    assert_eq!(alignment.delta, 1);
    assert!(alignment.confidence >= config.min_confidence);
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
                output_extent: 11,
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
fn sticky_side_chrome_is_removed_instead_of_repeated() {
    let document: Vec<u8> = (0..20).map(|column| 40 + column * 8).collect();
    let first = horizontal_viewport(&document, 0, 8, &[3, 17], &[229, 247], 10, 1.0);
    let second = horizontal_viewport(&document, 3, 8, &[3, 17], &[229, 247], 10, 1.0);
    let first_luma = LumaPlane::from_frame(&first).unwrap();
    let second_luma = LumaPlane::from_frame(&second).unwrap();

    assert_eq!(
        detect_sticky_side_chrome(
            &first_luma,
            &second_luma,
            3,
            &ChromeConfig {
                min_band: 2,
                ..ChromeConfig::default()
            }
        ),
        SideChromeBands { left: 2, right: 2 }
    );

    let mut stitcher = ScrollStitcher::for_axis(ScrollAxis::Horizontal, compact_stitch(Some(3)));
    assert_eq!(stitcher.push_frame(first).unwrap(), PushOutcome::Started);
    let outcome = stitcher.push_frame(second).unwrap();
    assert!(
        matches!(
            &outcome,
            PushOutcome::Advanced {
                delta: 3,
                output_extent: 11,
                output_height: 10,
                ..
            }
        ),
        "{outcome:?}"
    );
    assert_eq!(
        stitcher.side_chrome(),
        SideChromeBands { left: 2, right: 2 }
    );
    let output = stitcher.finish_frame().unwrap();
    assert_eq!((output.width(), output.height()), (11, 10));
    let first_row: Vec<u8> = output.data.as_chunks::<4>().0[..output.width() as usize]
        .iter()
        .map(|pixel| pixel[0])
        .collect();
    assert!(
        first_row
            .iter()
            .all(|column| ![3, 17, 229, 247].contains(column)),
        "fixed columns leaked into the canvas: {first_row:?}"
    );
    assert_eq!(first_row, document[0..11]);
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
fn horizontal_seam_quality_and_fractional_dpi_metadata_are_preserved() {
    let document: Vec<u8> = (0..20).map(|column| 10 + column * 10).collect();
    let first = gray_column_frame(&document[0..9], 6, 1.25);
    let mut second = gray_column_frame(&document[3..12], 6, 1.25);
    for channel in &mut second.data[0..3] {
        *channel = channel.saturating_add(80);
    }

    let mut stitcher = ScrollStitcher::for_axis(ScrollAxis::Horizontal, compact_stitch(Some(3)));
    stitcher.push_frame(first).unwrap();
    let PushOutcome::Advanced { seam, .. } = stitcher.push_frame(second).unwrap() else {
        panic!("the noisy horizontal overlap should still align");
    };
    assert!(seam.mean_absolute_error > 0);
    let output = stitcher.finish_frame().unwrap();
    assert_eq!(output.scale, ScaleFactor::new(1.25));
    assert_eq!((output.width(), output.height()), (12, 6));
    assert!(output.is_well_formed());
}

#[test]
fn mixed_dpi_frames_are_rejected_before_mutating_the_horizontal_stitch() {
    let document: Vec<u8> = (0..20).map(|column| 10 + column * 10).collect();
    let mut stitcher = ScrollStitcher::for_axis(ScrollAxis::Horizontal, compact_stitch(Some(3)));
    stitcher
        .push_frame(gray_column_frame(&document[0..9], 6, 1.25))
        .unwrap();

    let error = stitcher
        .push_frame(gray_column_frame(&document[3..12], 6, 1.5))
        .expect_err("one output frame cannot truthfully carry two DPI scales");
    assert!(
        error.to_string().contains("pixel interpretation"),
        "{error}"
    );
    assert_eq!(stitcher.summary().frames, 1);
    assert_eq!(stitcher.side_chrome(), SideChromeBands::default());
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

#[test]
fn horizontal_stationary_probes_end_without_duplicate_columns() {
    let document: Vec<u8> = (0..20).map(|column| 10 + column * 10).collect();
    let end = gray_column_frame(&document[3..11], 7, 1.0);
    let source = FixtureSource {
        frames: VecDeque::from([
            gray_column_frame(&document[0..8], 7, 1.0),
            end.clone(),
            end.clone(),
            end,
        ]),
    };
    let output = ScrollSession::new(
        source,
        Box::<FixtureDriver>::default(),
        NoopPacer,
        horizontal_session_config(3.0, 10),
    )
    .run(&mut NeverCancel, |_| {})
    .expect("horizontal end-of-content");

    assert_eq!(output.reason, CompletionReason::EndOfContent);
    assert_eq!((output.frame.width(), output.frame.height()), (11, 7));
    assert_eq!(output.captured_frames, 4);
    assert_eq!(output.seams, 1);
}

struct CancelAfterFirstSeam {
    polls: usize,
    action: CancelAction,
}

impl CancelSignal for CancelAfterFirstSeam {
    fn cancellation(&mut self) -> Option<CancelAction> {
        self.polls += 1;
        (self.polls == 5).then_some(self.action)
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
        &mut CancelAfterFirstSeam {
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
        &mut CancelAfterFirstSeam {
            polls: 0,
            action: CancelAction::Abort,
        },
        |_| {},
    )
    .expect_err("abort");
    assert!(aborted.is_cancellation());
}

#[test]
fn horizontal_cancellation_keeps_or_aborts_the_same_partial_capture() {
    let document: Vec<u8> = (0..20).map(|column| 10 + column * 10).collect();
    let frames = || FixtureSource {
        frames: VecDeque::from([
            gray_column_frame(&document[0..8], 7, 1.0),
            gray_column_frame(&document[3..11], 7, 1.0),
        ]),
    };

    let kept = ScrollSession::new(
        frames(),
        Box::<FixtureDriver>::default(),
        NoopPacer,
        horizontal_session_config(3.0, 10),
    )
    .run(
        &mut CancelAfterFirstSeam {
            polls: 0,
            action: CancelAction::Keep,
        },
        |_| {},
    )
    .expect("keep horizontal partial");
    assert_eq!(kept.reason, CompletionReason::CancelledKeep);
    assert_eq!((kept.frame.width(), kept.frame.height()), (11, 7));

    let aborted = ScrollSession::new(
        frames(),
        Box::<FixtureDriver>::default(),
        NoopPacer,
        horizontal_session_config(3.0, 10),
    )
    .run(
        &mut CancelAfterFirstSeam {
            polls: 0,
            action: CancelAction::Abort,
        },
        |_| {},
    )
    .expect_err("abort horizontal partial");
    assert!(aborted.is_cancellation());
}

#[test]
fn manual_horizontal_mode_waits_for_movement_then_detects_the_end() {
    let document: Vec<u8> = (0..20).map(|column| 10 + column * 10).collect();
    let first = horizontal_viewport(&document, 0, 8, &[3, 17], &[229, 247], 7, 1.0);
    let second = horizontal_viewport(&document, 3, 8, &[3, 17], &[229, 247], 7, 1.0);
    let source = FixtureSource {
        frames: VecDeque::from([
            first.clone(),
            first.clone(),
            first,
            second.clone(),
            second.clone(),
            second,
        ]),
    };
    let mut config = horizontal_session_config(3.0, 8);
    config.manual_stall_limit = 2;
    let output = ScrollSession::new(
        source,
        Box::new(ManualScrollDriver::new("horizontal fixture")),
        NoopPacer,
        config,
    )
    .run(&mut NeverCancel, |_| {})
    .expect("manual horizontal session");

    assert_eq!(output.reason, CompletionReason::EndOfContent);
    assert_eq!((output.frame.width(), output.frame.height()), (11, 7));
}
