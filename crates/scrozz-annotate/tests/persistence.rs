//! Persistence: the invisible sidecar that makes annotations permanent-editable.
//!
//! Decision D14 promises that reopening a capture months later restores every
//! arrow exactly as it was. That promise is only as good as this round-trip.

mod common;

use common::{
    capture_with, checkerboard, document, every_annotation, rect, region_capture, window_capture,
};
use scrozz_annotate::{
    Annotation, ArrowStyle, Background, Beautification, Color, Document, DocumentData, RedactStyle,
    Renderer, SkiaRenderer, Style,
};
use scrozz_core::{LogicalPoint, Provenance};

#[test]
fn round_trip_preserves_every_annotation_exactly() {
    let mut doc = document(400, 300);
    for (annotation, style) in every_annotation() {
        doc.add(annotation, style);
    }
    let before = doc.data();

    let json = serde_json::to_string(&before).expect("serialise");
    let after: DocumentData = serde_json::from_str(&json).expect("deserialise");

    assert_eq!(
        before, after,
        "the sidecar must survive a JSON round trip byte for byte"
    );

    // And re-hydrating into a live document preserves ids, order and styles.
    let restored = Document::from_data(region_capture(400, 300), after).expect("rehydrate");
    assert_eq!(restored.len(), doc.len());
    for (original, reloaded) in doc.annotations().iter().zip(restored.annotations()) {
        assert_eq!(original.id, reloaded.id);
        assert_eq!(original.annotation, reloaded.annotation);
        assert_eq!(original.style, reloaded.style);
    }
}

#[test]
fn round_trip_preserves_fractional_coordinates() {
    let mut doc = document(400, 300);
    // Logical coordinates are f64 and routinely fractional on a 2x display.
    // Silently rounding them would drift an arrow every time the file reloads.
    let id = doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(10.123_456_789, 20.987_654_321),
            to: LogicalPoint::new(300.5, 199.25),
        },
        Style::stroked()
            .with_stroke_width(2.375)
            .with_opacity(0.625),
    );
    let json = serde_json::to_string(&doc.data()).unwrap();
    let back: DocumentData = serde_json::from_str(&json).unwrap();
    let restored = Document::from_data(region_capture(400, 300), back).unwrap();

    let Annotation::Arrow { from, to } = restored.get(id).unwrap().annotation else {
        panic!("expected arrow");
    };
    assert!((from.x - 10.123_456_789).abs() < 1e-12);
    assert!((from.y - 20.987_654_321).abs() < 1e-12);
    assert!((to.x - 300.5).abs() < 1e-12);
    assert!((restored.get(id).unwrap().style.stroke_width - 2.375).abs() < 1e-6);
    assert!((restored.get(id).unwrap().style.opacity - 0.625).abs() < 1e-6);
}

#[test]
fn arrow_style_bend_and_sketch_seed_round_trip_with_the_annotation_id() {
    let mut doc = document(400, 300);
    let id = doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(10.0, 20.0),
            to: LogicalPoint::new(300.0, 180.0),
        },
        Style::stroked()
            .with_arrow_style(ArrowStyle::Sketch)
            .with_arrow_bend(-0.35)
            .with_stroke_width(14.0),
    );
    let json = serde_json::to_string(&doc.data()).unwrap();
    let restored = Document::from_data(
        region_capture(400, 300),
        serde_json::from_str(&json).unwrap(),
    )
    .unwrap();
    let object = restored.get(id).unwrap();
    assert_eq!(object.id, id);
    assert_eq!(object.style.arrow_style, ArrowStyle::Sketch);
    assert_eq!(object.style.arrow_bend, -0.35);
    assert_eq!(object.style.stroke_width, 14.0);
}

#[test]
fn version_two_styles_migrate_to_bold_straight_arrows() {
    let mut doc = document(120, 90);
    doc.add(
        Annotation::Arrow {
            from: LogicalPoint::new(10.0, 20.0),
            to: LogicalPoint::new(90.0, 70.0),
        },
        Style::stroked(),
    );
    let mut value = serde_json::to_value(doc.data()).unwrap();
    value["version"] = serde_json::json!(2);
    let style = value["annotations"][0]["style"]
        .as_object_mut()
        .expect("style object");
    style.remove("arrow_style");
    style.remove("arrow_bend");
    style.remove("redact_intensity");

    let restored = Document::from_data(
        region_capture(120, 90),
        serde_json::from_value(value).unwrap(),
    )
    .unwrap();
    let style = restored.annotations()[0].style;
    assert_eq!(style.arrow_style, ArrowStyle::Bold);
    assert_eq!(style.arrow_bend, 0.0);
}

#[test]
fn secure_redaction_intensity_round_trips_exactly() {
    let mut doc = document(120, 90);
    let id = doc.add(
        Annotation::Redact {
            area: rect(10.0, 10.0, 80.0, 50.0),
            style: RedactStyle::Pixelate,
        },
        Style::secure_redaction(0.73),
    );
    let expected = SkiaRenderer.render(&doc).unwrap();
    let json = serde_json::to_string(&doc.data()).unwrap();
    let restored = Document::from_data(
        region_capture(120, 90),
        serde_json::from_str(&json).unwrap(),
    )
    .unwrap();
    assert_eq!(
        restored.get(id).unwrap().style.effective_redact_intensity(),
        Some(0.73)
    );
    assert_eq!(restored.data().version, DocumentData::VERSION);
    assert_eq!(SkiaRenderer.render(&restored).unwrap().data, expected.data);
}

#[test]
fn version_three_redactions_keep_their_exact_legacy_rendering() {
    let source = capture_with(checkerboard(120, 90, 2), Provenance::Region);
    let mut original = Document::new(source.clone());
    for (index, style) in [RedactStyle::Blur, RedactStyle::Pixelate, RedactStyle::Solid]
        .into_iter()
        .enumerate()
    {
        original.add(
            Annotation::Redact {
                area: rect(5.0 + index as f64 * 38.0, 10.0, 34.0, 60.0),
                style,
            },
            Style::redaction(),
        );
    }
    let expected = SkiaRenderer.render(&original).unwrap();
    let mut value = serde_json::to_value(original.data()).unwrap();
    value["version"] = serde_json::json!(3);
    for annotation in value["annotations"].as_array_mut().unwrap() {
        annotation["style"]
            .as_object_mut()
            .unwrap()
            .remove("redact_intensity");
    }
    let restored = Document::from_data(source, serde_json::from_value(value).unwrap()).unwrap();
    assert!(
        restored
            .annotations()
            .iter()
            .all(|object| object.style.effective_redact_intensity().is_none())
    );
    assert_eq!(SkiaRenderer.render(&restored).unwrap().data, expected.data);
}

#[test]
fn persisted_out_of_range_redaction_intensity_is_safely_clamped_on_use() {
    let mut doc = document(40, 40);
    doc.add(
        Annotation::Redact {
            area: rect(0.0, 0.0, 40.0, 40.0),
            style: RedactStyle::Pixelate,
        },
        Style::secure_redaction(0.5),
    );
    for (stored, expected) in [(-20.0, 0.0), (20.0, 1.0)] {
        let mut value = serde_json::to_value(doc.data()).unwrap();
        value["annotations"][0]["style"]["redact_intensity"] = serde_json::json!(stored);
        let data: DocumentData = serde_json::from_value(value).unwrap();
        let restored = Document::from_data(region_capture(40, 40), data).unwrap();
        assert_eq!(
            restored.data().annotations[0].style.redact_intensity,
            Some(expected)
        );
    }
}

#[test]
fn secure_redaction_marker_is_removed_from_non_redaction_objects_on_load() {
    let mut doc = document(40, 40);
    doc.add(
        Annotation::Rectangle(rect(2.0, 2.0, 20.0, 20.0)),
        Style::stroked().with_redact_intensity(0.9),
    );
    let restored = Document::from_data(region_capture(40, 40), doc.data()).unwrap();
    assert_eq!(restored.data().annotations[0].style.redact_intensity, None);
}

#[test]
fn round_trip_preserves_id_allocation() {
    let mut doc = document(200, 200);
    let a = doc.add(
        Annotation::Rectangle(rect(0.0, 0.0, 5.0, 5.0)),
        Style::stroked(),
    );
    let b = doc.add(
        Annotation::Rectangle(rect(0.0, 0.0, 5.0, 5.0)),
        Style::stroked(),
    );
    doc.remove(a);

    let json = serde_json::to_string(&doc.data()).unwrap();
    let mut restored = Document::from_data(
        region_capture(200, 200),
        serde_json::from_str(&json).unwrap(),
    )
    .unwrap();

    let c = restored.add(
        Annotation::Rectangle(rect(0.0, 0.0, 5.0, 5.0)),
        Style::stroked(),
    );
    assert_ne!(
        c, a,
        "reloading must not restart id allocation and reuse a dead id"
    );
    assert_ne!(c, b);
}

#[test]
fn round_trip_preserves_every_redaction_style() {
    for style in [RedactStyle::Blur, RedactStyle::Pixelate, RedactStyle::Solid] {
        let mut doc = document(100, 100);
        doc.add(
            Annotation::Redact {
                area: rect(10.0, 10.0, 30.0, 30.0),
                style,
            },
            Style::redaction(),
        );
        let json = serde_json::to_string(&doc.data()).unwrap();
        let back: DocumentData = serde_json::from_str(&json).unwrap();
        let Annotation::Redact {
            style: reloaded, ..
        } = back.annotations[0].annotation
        else {
            panic!("expected redaction");
        };
        assert_eq!(
            style, reloaded,
            "a redaction that reloads as the wrong style is a privacy bug, not a cosmetic one"
        );
    }
}

#[test]
fn round_trip_preserves_beautification() {
    let mut doc = document(200, 200);
    doc.set_beautification(Some(Beautification {
        padding: 48.5,
        corner_radius: 12.25,
        shadow: 30.0,
        background: Background::Gradient {
            start: Color::rgb(240, 120, 40),
            end: Color::rgb(30, 40, 160),
        },
    }))
    .expect("region captures may be beautified");

    let json = serde_json::to_string(&doc.data()).unwrap();
    let back: DocumentData = serde_json::from_str(&json).unwrap();
    let restored = Document::from_data(region_capture(200, 200), back).unwrap();

    let b = restored.beautification().expect("beautification survives");
    assert!((b.padding - 48.5).abs() < 1e-9);
    assert!((b.corner_radius - 12.25).abs() < 1e-9);
    assert_eq!(
        b.background,
        Background::Gradient {
            start: Color::rgb(240, 120, 40),
            end: Color::rgb(30, 40, 160),
        }
    );
}

#[test]
fn an_empty_document_round_trips() {
    let doc = document(10, 10);
    let json = serde_json::to_string(&doc.data()).unwrap();
    let back: DocumentData = serde_json::from_str(&json).unwrap();
    assert!(back.annotations.is_empty());
    assert!(back.beautification.is_none());
    assert_eq!(back.version, DocumentData::VERSION);
}

#[test]
fn a_sidecar_from_the_future_is_refused_rather_than_misread() {
    let data = DocumentData {
        version: DocumentData::VERSION + 1,
        ..DocumentData::default()
    };
    // Silently accepting a newer sidecar would drop whatever fields this build
    // does not know about, then write the loss back on the next save.
    assert!(Document::from_data(region_capture(50, 50), data).is_err());
}

#[test]
fn a_window_sidecar_carrying_beautification_is_refused() {
    // Decision D9. A document can arrive from disk, so refusing only at the
    // setter would leave a route in.
    let data = DocumentData {
        beautification: Some(Beautification::padded(
            40.0,
            Background::Solid(Color::WHITE),
        )),
        ..DocumentData::default()
    };

    assert!(Document::from_data(window_capture(50, 50), data.clone()).is_err());
    assert!(Document::from_data(region_capture(50, 50), data).is_ok());
}

#[test]
fn beautification_is_refused_for_window_captures() {
    let mut doc = Document::new(window_capture(100, 100));
    assert!(!doc.may_beautify());

    let err = doc
        .set_beautification(Some(Beautification::padded(
            32.0,
            Background::Solid(Color::WHITE),
        )))
        .expect_err("D9 forbids compositing onto a window capture");
    assert!(
        format!("{err}").to_lowercase().contains("window"),
        "the refusal should say why: {err}"
    );
    assert!(
        doc.beautification().is_none(),
        "the refusal must not half-apply"
    );

    // Clearing is always allowed, even for a window.
    assert!(doc.set_beautification(None).is_ok());
}

#[test]
fn beautification_is_permitted_for_every_other_provenance() {
    use scrozz_core::Provenance;

    for provenance in [
        Provenance::Display,
        Provenance::Region,
        Provenance::AllDisplays,
        Provenance::Stitched,
    ] {
        let capture = common::capture_with(common::flat(80, 60, [10, 20, 30, 255]), provenance);
        let mut doc = Document::new(capture);
        assert!(
            doc.may_beautify(),
            "{provenance:?} should allow beautification"
        );
        assert!(
            doc.set_beautification(Some(Beautification::padded(
                16.0,
                Background::Solid(Color::WHITE)
            )))
            .is_ok(),
            "{provenance:?} should allow beautification"
        );
        assert!(doc.beautification().is_some());
    }
}

#[test]
fn json_is_human_legible() {
    // Not decoration: this sidecar is the only representation of the user's
    // work, and a self-describing format is what makes it recoverable by hand
    // if anything ever goes wrong.
    let mut doc = document(100, 100);
    doc.add(
        Annotation::Redact {
            area: rect(1.0, 2.0, 3.0, 4.0),
            style: RedactStyle::Pixelate,
        },
        Style::redaction(),
    );
    let json = serde_json::to_string_pretty(&doc.data()).unwrap();
    assert!(json.contains("\"redact\""), "{json}");
    assert!(json.contains("\"pixelate\""), "{json}");
    assert!(json.contains("\"version\""), "{json}");
}
