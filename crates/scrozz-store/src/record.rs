//! The durable, human-readable record of one capture.
//!
//! # Why this exists when there is a database
//!
//! Because the database is a **cache of this**, not the other way round.
//!
//! Decision D23 keeps annotation documents forever. A promise like that cannot
//! rest on a single binary file that one bad shutdown can corrupt, so every
//! capture also writes a small JSON sidecar next to its pixels. The sidecar is
//! the record; the SQLite index exists to make history fast to query. If the
//! index is lost, [`crate::SqliteStore`] rebuilds it from these files and the
//! user loses nothing but a few milliseconds of startup.
//!
//! # Why the document is held as raw JSON here
//!
//! `scrozz_annotate::DocumentData` carries its own format version and its
//! `Annotation` enum is `#[non_exhaustive]` and still growing. Naming its
//! variants in *this* format would mean that adding a drawing tool breaks the
//! ability to read old history. Holding the document as opaque JSON and typing
//! it only on demand means a record whose annotation shape has drifted still
//! loads its metadata, still lists, still counts its edits, and reports exactly
//! what it could not decode.

use scrozz_annotate::DocumentData;
use scrozz_core::{CaptureTarget, Error, Provenance, Result};
use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    CaptureId,
    model::{
        CaptureRecord, FrameHeader, ImageState, MediaKind, ProvenanceRepr, TargetRepr, Timestamp,
        VideoMetadata,
    },
};

/// Current sidecar format. Bumped only when old files stop being readable,
/// which so far they never have.
pub const RECORD_FORMAT: u32 = 1;

/// Everything persisted about one capture except its pixels.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StoredRecord {
    /// Sidecar format version.
    #[serde(default = "default_format")]
    pub format: u32,
    /// Identity.
    pub id: String,
    /// When the capture was taken.
    pub created_at: i64,
    /// When it entered history, which may differ for an import.
    #[serde(default)]
    pub stored_at: i64,
    /// Exempt from eviction.
    #[serde(default)]
    pub pinned: bool,
    /// Screenshot, video, or GIF. Missing in legacy sidecars means screenshot.
    #[serde(default)]
    pub media_kind: MediaKind,
    /// Owning application.
    #[serde(default)]
    pub app_name: Option<String>,
    /// Stable application identifier retained for schema compatibility.
    #[serde(default)]
    pub app_identifier: Option<String>,
    /// Window title.
    #[serde(default)]
    pub window_title: Option<String>,
    /// Whether the captured window included its native shadow.
    #[serde(default)]
    pub window_shadow: Option<bool>,
    /// How the capture was produced.
    pub provenance: ProvenanceRepr,
    /// What it was aimed at.
    pub target: TargetRepr,
    /// Frame geometry.
    pub frame: Option<FrameHeader>,
    /// External recording metadata for video sidecars.
    #[serde(default)]
    pub video: Option<VideoMetadata>,
    /// Content address of the source pixels, if they were ever stored.
    #[serde(default)]
    pub image_hash: Option<String>,
    /// Size of those pixels on disk.
    #[serde(default)]
    pub image_bytes: u64,
    /// When the pixels were evicted, if they were.
    #[serde(default)]
    pub image_evicted_at: Option<i64>,
    /// Recognised text.
    #[serde(default)]
    pub ocr_text: Option<String>,
    /// The annotation document, exactly as `scrozz-annotate` serialises it.
    ///
    /// Opaque on purpose — see the module documentation. This is the field
    /// decision D23 promises to keep forever.
    #[serde(default = "empty_document")]
    pub document: serde_json::Value,
}

#[derive(Deserialize)]
struct StoredRecordWire {
    #[serde(default = "default_format")]
    format: u32,
    id: String,
    created_at: i64,
    #[serde(default)]
    stored_at: i64,
    #[serde(default)]
    pinned: bool,
    #[serde(default)]
    media_kind: MediaKind,
    #[serde(default)]
    source_app: Option<SourceAppWire>,
    #[serde(default)]
    app_name: Option<String>,
    #[serde(default)]
    app_identifier: Option<String>,
    #[serde(default)]
    window_title: Option<String>,
    #[serde(default)]
    window_shadow: Option<bool>,
    provenance: ProvenanceRepr,
    target: TargetRepr,
    frame: Option<FrameHeader>,
    #[serde(default)]
    video: Option<VideoMetadata>,
    #[serde(default)]
    image_hash: Option<String>,
    #[serde(default)]
    image_bytes: u64,
    #[serde(default)]
    image_evicted_at: Option<i64>,
    #[serde(default)]
    ocr_text: Option<String>,
    #[serde(default = "empty_document")]
    document: serde_json::Value,
}

#[derive(Default, Deserialize)]
struct SourceAppWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    identifier: Option<String>,
    #[serde(default)]
    window_title: Option<String>,
}

impl<'de> Deserialize<'de> for StoredRecord {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = StoredRecordWire::deserialize(deserializer)?;
        let source = wire.source_app.unwrap_or_default();
        Ok(Self {
            format: wire.format,
            id: wire.id,
            created_at: wire.created_at,
            stored_at: wire.stored_at,
            pinned: wire.pinned,
            media_kind: wire.media_kind,
            app_name: source.name.or(wire.app_name),
            app_identifier: source.identifier.or(wire.app_identifier),
            window_title: source.window_title.or(wire.window_title),
            window_shadow: wire.window_shadow,
            provenance: wire.provenance,
            target: wire.target,
            frame: wire.frame,
            video: wire.video,
            image_hash: wire.image_hash,
            image_bytes: wire.image_bytes,
            image_evicted_at: wire.image_evicted_at,
            ocr_text: wire.ocr_text,
            document: wire.document,
        })
    }
}

const fn default_format() -> u32 {
    RECORD_FORMAT
}

fn empty_document() -> serde_json::Value {
    serde_json::to_value(DocumentData::default()).unwrap_or(serde_json::Value::Null)
}

impl StoredRecord {
    /// Builds a record from a live document plus its history metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the document cannot be serialised.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        id: &CaptureId,
        created_at: Timestamp,
        stored_at: Timestamp,
        pinned: bool,
        app_name: Option<String>,
        window_title: Option<String>,
        provenance: Provenance,
        target: &CaptureTarget,
        frame: FrameHeader,
        image_hash: Option<String>,
        image_bytes: u64,
        ocr_text: Option<String>,
        document: &DocumentData,
    ) -> Result<Self> {
        Ok(Self {
            format: RECORD_FORMAT,
            id: id.0.clone(),
            created_at: created_at.0,
            stored_at: stored_at.0,
            pinned,
            media_kind: MediaKind::Screenshot,
            app_name,
            app_identifier: None,
            window_title,
            window_shadow: None,
            provenance: provenance.into(),
            target: TargetRepr::from(target),
            frame: Some(frame),
            video: None,
            image_hash,
            image_bytes,
            image_evicted_at: None,
            ocr_text,
            document: serde_json::to_value(document)
                .map_err(|e| Error::Storage(format!("cannot serialise document: {e}")))?,
        })
    }

    /// Builds a durable sidecar for an externally stored native recording.
    pub fn from_video(
        id: &CaptureId,
        created_at: Timestamp,
        stored_at: Timestamp,
        pinned: bool,
        provenance: Provenance,
        target: &CaptureTarget,
        video: VideoMetadata,
    ) -> Result<Self> {
        video.validate()?;
        let media_kind = video.media_kind();
        Ok(Self {
            format: RECORD_FORMAT,
            id: id.0.clone(),
            created_at: created_at.0,
            stored_at: stored_at.0,
            pinned,
            media_kind,
            app_name: None,
            app_identifier: None,
            window_title: None,
            window_shadow: None,
            provenance: provenance.into(),
            target: TargetRepr::from(target),
            frame: None,
            video: Some(video),
            image_hash: None,
            image_bytes: 0,
            image_evicted_at: None,
            ocr_text: None,
            document: empty_document(),
        })
    }

    /// Replaces the stored edits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the document cannot be serialised.
    pub fn set_document(&mut self, document: &DocumentData) -> Result<()> {
        self.document = serde_json::to_value(document)
            .map_err(|e| Error::Storage(format!("cannot serialise document: {e}")))?;
        Ok(())
    }

    /// Number of annotations without decoding them.
    ///
    /// Reaching into the opaque JSON for one integer is the deliberate trade:
    /// history can show "3 edits" on a capture whose annotation shape this build
    /// can no longer parse.
    #[must_use]
    pub fn annotation_count(&self) -> usize {
        self.document
            .get("annotations")
            .and_then(serde_json::Value::as_array)
            .map_or(0, Vec::len)
    }

    /// Decodes the stored edits.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the stored shape is no longer readable by
    /// the current `scrozz-annotate`. The rest of the record stays usable.
    pub fn document_data(&self) -> Result<DocumentData> {
        serde_json::from_value(self.document.clone())
            .map_err(|e| Error::Storage(format!("cannot read document for {}: {e}", self.id)))
    }

    /// The state of this capture's pixels.
    #[must_use]
    pub fn image_state(&self) -> ImageState {
        match (&self.image_hash, self.image_evicted_at) {
            (Some(hash), None) => ImageState::Present {
                hash: hash.clone(),
                byte_len: self.image_bytes,
            },
            (Some(hash), Some(at)) => ImageState::Evicted {
                at: Timestamp(at),
                was_hash: hash.clone(),
            },
            (None, Some(at)) => ImageState::Evicted {
                at: Timestamp(at),
                was_hash: String::new(),
            },
            (None, None) => ImageState::Absent,
        }
    }

    /// Marks the pixels gone while keeping every trace of what they were.
    pub fn mark_evicted(&mut self, at: Timestamp) {
        self.image_evicted_at = Some(at.0);
        self.image_bytes = 0;
    }

    /// The public view of this record.
    #[must_use]
    pub fn to_capture_record(&self) -> CaptureRecord {
        CaptureRecord {
            id: CaptureId(self.id.clone()),
            created_at: Timestamp(self.created_at),
            pinned: self.pinned,
            media_kind: self.media_kind,
            app_name: self.app_name.clone(),
            window_title: self.window_title.clone(),
            provenance: self.provenance.into(),
            target: self.target.clone().into(),
            frame: self.frame.clone(),
            video: self.video.clone(),
            image: self.image_state(),
            ocr_text: self.ocr_text.clone(),
            annotation_count: self.annotation_count(),
        }
    }

    /// Lower-cased haystack backing free-text search.
    ///
    /// Folded in Rust rather than by SQL's `lower()`, which only folds ASCII —
    /// searching history for "Präsentation" has to work.
    #[must_use]
    pub fn search_text(&self) -> String {
        let mut haystack = String::new();
        for part in [
            self.app_name.as_deref(),
            self.window_title.as_deref(),
            self.ocr_text.as_deref(),
            self.video.as_ref().and_then(|video| video.path.to_str()),
            self.video.as_ref().map(|video| video.engine.as_str()),
        ]
        .into_iter()
        .flatten()
        {
            haystack.push_str(&part.to_lowercase());
            haystack.push('\n');
        }
        haystack
    }

    /// Encodes to the bytes written to disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if encoding fails.
    pub fn to_json(&self) -> Result<Vec<u8>> {
        serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Storage(format!("cannot encode record {}: {e}", self.id)))
    }

    /// Decodes from disk bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the bytes are not a readable record.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let record: Self = serde_json::from_slice(bytes)
            .map_err(|e| Error::Storage(format!("cannot decode record: {e}")))?;
        if record.format > RECORD_FORMAT {
            return Err(Error::Storage(format!(
                "record {} was written by a newer Scrozz (format {}, this build reads {RECORD_FORMAT})",
                record.id, record.format
            )));
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use scrozz_annotate::{Annotation, Background, Beautification, Document, Style};
    use scrozz_core::{
        Capture, ColorSpace, DisplayId, Frame, LogicalPoint, LogicalRect, LogicalSize,
        PhysicalSize, PixelFormat, ScaleFactor,
    };

    use super::*;

    fn header() -> FrameHeader {
        FrameHeader {
            size: PhysicalSize::new(8.0, 4.0),
            stride: 32,
            format: PixelFormat::Rgba8,
            color_space: ColorSpace::DisplayP3,
            scale: ScaleFactor::new(2.0),
        }
    }

    fn document_data() -> DocumentData {
        let mut document = Document::new(Capture {
            frame: Frame {
                data: vec![0; 128],
                size: PhysicalSize::new(8.0, 4.0),
                stride: 32,
                format: PixelFormat::Rgba8,
                color_space: ColorSpace::DisplayP3,
                scale: ScaleFactor::new(2.0),
            },
            provenance: Provenance::Region,
            target: CaptureTarget::AllDisplays,
        });
        document.add(
            Annotation::Rectangle(LogicalRect::new(
                LogicalPoint::new(1.0, 1.0),
                LogicalSize::new(2.0, 2.0),
            )),
            Style::stroked(),
        );
        document
            .set_beautification(Some(Beautification::padded(8.0, Background::default())))
            .expect("region captures may be beautified");
        document.data()
    }

    fn record() -> StoredRecord {
        StoredRecord::from_parts(
            &CaptureId("01ABC".into()),
            Timestamp(1_700_000_000_000),
            Timestamp(1_700_000_000_001),
            false,
            Some("Safari".into()),
            Some("Invoice".into()),
            Provenance::Region,
            &CaptureTarget::Display(DisplayId("main".into())),
            header(),
            Some("a".repeat(64)),
            4096,
            Some("Total: 12.00".into()),
            &document_data(),
        )
        .expect("record builds")
    }

    #[test]
    fn records_round_trip_through_json() {
        let mut original = record();
        original.app_identifier = Some("com.apple.Safari".into());
        original.window_shadow = Some(false);
        let bytes = original.to_json().expect("encode");
        let back = StoredRecord::from_json(&bytes).expect("decode");
        assert_eq!(original, back);

        let data = back.document_data().expect("document");
        assert_eq!(data.annotations.len(), 1);
        assert_eq!(
            data.beautification,
            Some(Beautification::padded(8.0, Background::default()))
        );
    }

    #[test]
    fn nested_source_metadata_from_the_alternate_sidecar_schema_is_preserved() {
        let original = record();
        let mut value = serde_json::to_value(&original).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("app_name");
        object.remove("app_identifier");
        object.remove("window_title");
        object.insert(
            "source_app".into(),
            serde_json::json!({
                "name": "Preview",
                "identifier": "com.apple.Preview",
                "window_title": "Document"
            }),
        );

        let decoded = StoredRecord::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(decoded.app_name.as_deref(), Some("Preview"));
        assert_eq!(decoded.app_identifier.as_deref(), Some("com.apple.Preview"));
        assert_eq!(decoded.window_title.as_deref(), Some("Document"));

        let canonical: serde_json::Value =
            serde_json::from_slice(&decoded.to_json().unwrap()).unwrap();
        assert!(canonical.get("source_app").is_none());
        assert_eq!(canonical["app_identifier"], "com.apple.Preview");
    }

    #[test]
    fn a_record_from_a_newer_build_is_refused_rather_than_misread() {
        let mut future = record();
        future.format = RECORD_FORMAT + 1;
        let bytes = future.to_json().expect("encode");
        let err = StoredRecord::from_json(&bytes).expect_err("must refuse");
        assert!(format!("{err}").contains("newer Scrozz"), "{err}");
    }

    #[test]
    fn metadata_survives_annotations_this_build_cannot_read() {
        // The exact scenario D14's "serialization stays internal" allows: a tool
        // was added, then removed, and old history still mentions it.
        let mut drifted = record();
        drifted.document = serde_json::json!({
            "version": 1,
            "annotations": [{ "id": 1, "annotation": { "Sparkles": { "at": [1, 2] } } }],
            "beautification": null,
            "next_id": 2
        });

        let bytes = drifted.to_json().expect("encode");
        let back = StoredRecord::from_json(&bytes).expect("record still decodes");

        assert_eq!(back.app_name.as_deref(), Some("Safari"));
        assert_eq!(
            back.annotation_count(),
            1,
            "the count must come from the raw JSON, not from decoding"
        );
        assert!(
            back.document_data().is_err(),
            "the unreadable annotation should surface as an error, not a panic"
        );
        assert_eq!(
            back.to_capture_record().window_title.as_deref(),
            Some("Invoice")
        );
    }

    #[test]
    fn eviction_keeps_the_hash_and_zeroes_the_bytes() {
        let mut record = record();
        assert!(record.image_state().is_present());
        record.mark_evicted(Timestamp(1_700_000_100_000));

        match record.image_state() {
            ImageState::Evicted { at, was_hash } => {
                assert_eq!(at, Timestamp(1_700_000_100_000));
                assert_eq!(was_hash, "a".repeat(64));
            }
            other => panic!("expected eviction, got {other:?}"),
        }
        assert_eq!(record.image_state().byte_len(), 0);
        assert_eq!(
            record.annotation_count(),
            1,
            "D23: evicting pixels must not touch the document"
        );
        assert!(
            record
                .document_data()
                .expect("document")
                .beautification
                .is_some(),
            "D23: eviction must not touch framing either"
        );
    }

    #[test]
    fn search_text_folds_non_ascii_case() {
        let mut record = record();
        record.window_title = Some("PRÄSENTATION".into());
        assert!(record.search_text().contains("präsentation"));
    }

    #[test]
    fn missing_optional_fields_default_rather_than_failing() {
        let minimal = serde_json::json!({
            "id": "01ABC",
            "created_at": 1,
            "provenance": "region",
            "target": { "kind": "all_displays" },
            "frame": {
                "size": { "width": 4.0, "height": 4.0 },
                "stride": 16,
                "format": "Rgba8",
                "color_space": "Srgb",
                "scale": 1.0
            }
        });
        let record = StoredRecord::from_json(&serde_json::to_vec(&minimal).expect("encode"))
            .expect("decode");
        assert_eq!(record.annotation_count(), 0);
        assert_eq!(record.image_state(), ImageState::Absent);
        assert!(!record.pinned);
        assert!(
            record.document_data().is_ok(),
            "a record with no document should still yield an empty one"
        );
    }
}
