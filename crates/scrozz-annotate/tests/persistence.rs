//! Persistence: the invisible sidecar that makes annotations permanent-editable.
//!
//! Decision D14 promises that reopening a capture months later restores every
//! arrow exactly as it was. That promise is only as good as this round-trip.

mod common;

use common::{document, every_annotation, rect, region_capture, window_capture};
use scrozz_annotate::{
    Alignment, Annotation, AspectPreset, Background, BackgroundImage, Beautification, Color,
    Document, DocumentData, RedactStyle, Style,
};
use scrozz_core::{ColorSpace, LogicalPoint};

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
        alignment: Alignment::BottomRight,
        auto_balance: true,
        aspect: AspectPreset::Portrait,
        border_width: 2.5,
        border_color: Color::rgba(255, 255, 255, 170),
    }))
    .expect("region captures may be beautified");

    let json = serde_json::to_string(&doc.data()).unwrap();
    let back: DocumentData = serde_json::from_str(&json).unwrap();
    let restored = Document::from_data(region_capture(200, 200), back).unwrap();

    let b = restored.beautification().expect("beautification survives");
    assert!((b.padding - 48.5).abs() < 1e-9);
    assert!((b.corner_radius - 12.25).abs() < 1e-9);
    assert_eq!(b.alignment, Alignment::BottomRight);
    assert!(b.auto_balance);
    assert_eq!(b.aspect, AspectPreset::Portrait);
    assert!((b.border_width - 2.5).abs() < 1e-9);
    assert_eq!(
        b.background,
        Background::Gradient {
            start: Color::rgb(240, 120, 40),
            end: Color::rgb(30, 40, 160),
        }
    );
}

#[test]
fn custom_background_pixels_survive_the_invisible_sidecar() {
    let image = BackgroundImage::new(
        2,
        2,
        vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 128, 255, 255, 255, 0,
        ],
        ColorSpace::DisplayP3,
    )
    .unwrap();
    let mut doc = document(20, 20);
    doc.set_beautification(Some(Beautification {
        padding: 10.0,
        background: Background::Image(image.clone()),
        ..Beautification::default()
    }))
    .unwrap();

    let json = serde_json::to_vec(&doc.data()).unwrap();
    let encoded = std::str::from_utf8(&json).unwrap();
    assert!(encoded.contains("\"pixels_png\":"));
    assert!(
        !encoded.contains("\"pixels\":["),
        "current sidecars must not materialize one JSON value per image byte"
    );
    let data: DocumentData = serde_json::from_slice(&json).unwrap();
    let restored = Document::from_data(region_capture(20, 20), data).unwrap();
    assert_eq!(
        restored.beautification().unwrap().background,
        Background::Image(image)
    );
}

#[test]
fn legacy_raw_background_pixels_migrate_to_compact_png_storage() {
    let json = r#"{
        "version": 1,
        "annotations": [],
        "beautification": {
            "padding": 1.0,
            "corner_radius": 0.0,
            "shadow": 0.0,
            "background": {
                "image": {
                    "width": 1,
                    "height": 1,
                    "pixels": [10, 20, 30, 255],
                    "color_space": "Srgb"
                }
            }
        },
        "next_id": 1
    }"#;
    let data: DocumentData = serde_json::from_str(json).unwrap();
    let restored = Document::from_data(region_capture(1, 1), data).unwrap();
    let migrated = serde_json::to_string(&restored.data()).unwrap();
    assert!(migrated.contains("\"version\":2"));
    assert!(migrated.contains("\"pixels_png\":"));
    assert!(!migrated.contains("\"pixels\":["));
}

#[test]
fn custom_background_area_is_bounded_before_buffer_validation() {
    let error = BackgroundImage::new(5_000, 5_000, Vec::new(), ColorSpace::Srgb)
        .expect_err("25 million background pixels exceed the embedded-image limit");
    assert!(error.to_string().contains("pixels"));
}

#[test]
fn legacy_beautification_without_new_fields_uses_safe_defaults() {
    let json = r#"{
        "version": 1,
        "annotations": [],
        "beautification": {
            "padding": 12.0,
            "corner_radius": 4.0,
            "shadow": 0.0,
            "background": "transparent"
        },
        "next_id": 1
    }"#;
    let data: DocumentData = serde_json::from_str(json).unwrap();
    let beauty = data.beautification.as_ref().unwrap();
    assert_eq!(beauty.alignment, Alignment::Center);
    assert_eq!(beauty.aspect, AspectPreset::Original);
    assert!(!beauty.auto_balance);
    assert_eq!(beauty.border_width, 0.0);
    assert_eq!(beauty.border_color, Color::TRANSPARENT);
    let restored = Document::from_data(region_capture(20, 20), data).unwrap();
    assert_eq!(
        restored.data().version,
        DocumentData::VERSION,
        "saving a migrated v1 sidecar must write the current version"
    );
}

#[test]
fn expanded_beautification_writes_a_new_document_version() {
    assert_eq!(
        DocumentData::VERSION,
        2,
        "older builds must reject sidecars containing the expanded framing model"
    );
}

#[test]
fn malformed_custom_background_is_refused_on_rehydrate() {
    let json = r#"{
        "version": 1,
        "annotations": [],
        "beautification": {
            "padding": 10.0,
            "corner_radius": 0.0,
            "shadow": 0.0,
            "background": {
                "image": {
                    "width": 40,
                    "height": 40,
                    "pixels": [0, 0, 0, 0, 0, 0, 0],
                    "color_space": "Srgb"
                }
            }
        },
        "next_id": 1
    }"#;
    assert!(serde_json::from_str::<DocumentData>(json).is_err());
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
