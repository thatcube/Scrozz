//! The SQLite-backed history store.

use std::path::{Path, PathBuf};

use rusqlite::{
    Connection, OptionalExtension as _, Row, ToSql, TransactionBehavior, params, params_from_iter,
};
use scrozz_annotate::{AnnotationObject, Document, DocumentData};
use scrozz_core::{Capture, Error, Frame, Result};

use crate::{
    CaptureId, RetentionPolicy, Store, db, hash,
    id::capture_id_at,
    layout::StoreLayout,
    model::{
        CaptureRecord, FrameHeader, ImageState, Page, ProvenanceRepr, RetentionReport, SearchQuery,
        TargetRepr, Timestamp,
    },
    record::StoredRecord,
    schema,
};

/// Columns every record query selects, in the order [`row_to_record`] reads.
const RECORD_COLUMNS: &str = "id, created_at, pinned, app_name, window_title, provenance, \
     target_json, frame_json, image_hash, image_bytes, image_evicted_at, ocr_text, \
     annotation_count";

/// A capture on its way into history.
///
/// Metadata the platform knows but the document does not — which application
/// owned the window, what its title was — arrives here rather than being dug
/// out of the capture, because on Wayland there may be no title at all and the
/// store must not care.
#[derive(Debug, Clone)]
pub struct NewCapture<'a> {
    /// The document to persist. Its source pixels become the stored image.
    pub document: &'a Document,
    /// When the capture was taken. Defaults to now.
    pub created_at: Timestamp,
    /// Owning application, if known.
    pub app_name: Option<String>,
    /// Window title, if known.
    pub window_title: Option<String>,
    /// Recognised text, if OCR has already run.
    pub ocr_text: Option<String>,
    /// Whether to pin it immediately, exempting it from eviction.
    pub pinned: bool,
}

impl<'a> NewCapture<'a> {
    /// A capture of `document`, taken now, with no platform metadata.
    #[must_use]
    pub fn new(document: &'a Document) -> Self {
        Self {
            document,
            created_at: Timestamp::now(),
            app_name: None,
            window_title: None,
            ocr_text: None,
            pinned: false,
        }
    }

    /// Records the owning application.
    #[must_use]
    pub fn from_app(mut self, app: impl Into<String>) -> Self {
        self.app_name = Some(app.into());
        self
    }

    /// Records the window title.
    #[must_use]
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.window_title = Some(title.into());
        self
    }

    /// Records recognised text.
    #[must_use]
    pub fn with_ocr(mut self, text: impl Into<String>) -> Self {
        self.ocr_text = Some(text.into());
        self
    }

    /// Overrides the capture time, for imports and for tests.
    #[must_use]
    pub const fn taken_at(mut self, at: Timestamp) -> Self {
        self.created_at = at;
        self
    }

    /// Pins the capture on arrival.
    #[must_use]
    pub const fn pinned(mut self) -> Self {
        self.pinned = true;
        self
    }
}

/// A document read back out of history.
///
/// Two variants because decision D23 gives documents and images different
/// lifetimes, and a caller that has not thought about the evicted case should
/// not be able to compile. The alternative — returning a `Document` with an
/// empty `Frame` — would have a year-old capture silently render as a black
/// rectangle instead of as what it is: a capture whose edits survived.
#[derive(Debug, Clone)]
pub enum DocumentState {
    /// The pixels are here; this is the full editable document.
    Complete(Document),
    /// The pixels were evicted under the size cap. Every edit is intact.
    ImageEvicted(EvictedDocument),
}

impl DocumentState {
    /// The document, if its pixels are still present.
    #[must_use]
    pub fn complete(self) -> Option<Document> {
        match self {
            Self::Complete(document) => Some(document),
            Self::ImageEvicted(_) => None,
        }
    }

    /// The edits, whichever state the capture is in.
    #[must_use]
    pub fn annotations(&self) -> &[AnnotationObject] {
        match self {
            Self::Complete(document) => document.annotations(),
            Self::ImageEvicted(evicted) => &evicted.data.annotations,
        }
    }

    /// The persisted edits, whichever state the capture is in.
    #[must_use]
    pub fn data(&self) -> DocumentData {
        match self {
            Self::Complete(document) => document.data(),
            Self::ImageEvicted(evicted) => evicted.data.clone(),
        }
    }
}

/// A capture whose pixels are gone but whose edits are not.
#[derive(Debug, Clone)]
pub struct EvictedDocument {
    /// Everything history knows about the capture.
    pub record: CaptureRecord,
    /// The edits, exactly as last saved.
    ///
    /// Not a [`Document`], because a `Document` owns a [`scrozz_core::Capture`]
    /// and there is no honest capture to give it. Handing back one wrapped
    /// around an empty frame is how a year-old screenshot ends up rendering as a
    /// black rectangle.
    pub data: DocumentData,
}

/// What rebuilding the index found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Captures re-indexed from their durable records.
    pub records_recovered: usize,
    /// Sidecars that could not be read, left in place for inspection.
    pub records_unreadable: usize,
    /// Distinct blobs found on disk.
    pub blobs_found: usize,
    /// Bytes those blobs occupy.
    pub bytes_found: u64,
    /// Captures whose record survived but whose pixels did not.
    pub images_missing: usize,
    /// Index rows dropped because no record backs them.
    pub rows_dropped: usize,
    /// Where a damaged index was moved to, if one was.
    pub quarantined: Option<PathBuf>,
}

/// Everything a real history needs beyond the minimal [`Store`] contract.
pub trait History: Store {
    /// Adds a capture, returning its new identifier.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the frame's geometry does not match
    /// its buffer, or [`Error::Storage`] if the write fails.
    fn insert(&mut self, capture: NewCapture<'_>) -> Result<CaptureId>;

    /// Everything known about one capture, without its pixels.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the index is unreadable.
    fn record(&self, id: &CaptureId) -> Result<Option<CaptureRecord>>;

    /// A capture's document, and whether its pixels survived.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the record or its pixels cannot be read.
    fn document(&mut self, id: &CaptureId) -> Result<Option<DocumentState>>;

    /// Raw source pixels, or `None` if the capture is unknown or evicted.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the blob cannot be read.
    fn image(&mut self, id: &CaptureId) -> Result<Option<Vec<u8>>>;

    /// One page of history, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the index is unreadable.
    fn page(&self, page: Page) -> Result<Vec<CaptureRecord>>;

    /// Captures matching `query`, newest first.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the index is unreadable.
    fn search(&self, query: &SearchQuery) -> Result<Vec<CaptureRecord>>;

    /// How many captures history holds.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the index is unreadable.
    fn count(&self) -> Result<u64>;

    /// Removes a capture entirely — record, pixels and all.
    ///
    /// Distinct from eviction: this is the user saying "forget this", so unlike
    /// [`Store::enforce_retention`] it *does* discard the document.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the deletion fails.
    fn delete(&mut self, id: &CaptureId) -> Result<bool>;

    /// Persists a document's edits.
    ///
    /// The source image is immutable per decision D14 — the untouched capture
    /// is what makes every annotation re-editable forever — so only the
    /// annotations and framing are written.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the capture is unknown or the write fails.
    fn save_document(&mut self, id: &CaptureId, document: &Document) -> Result<()>;

    /// Persists edits for a capture whose pixels are gone.
    ///
    /// The whole point of decision D23: an image-evicted capture is still a
    /// capture, and its edits are still editable.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the capture is unknown or the write fails.
    fn save_edits(&mut self, id: &CaptureId, data: &DocumentData) -> Result<()>;

    /// Attaches or clears recognised text.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the capture is unknown or the write fails.
    fn set_ocr_text(&mut self, id: &CaptureId, text: Option<&str>) -> Result<()>;

    /// Bytes of source imagery currently on disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the index is unreadable.
    fn stored_image_bytes(&self) -> Result<u64>;

    /// Enforces `policy` and reports exactly what it did.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if eviction could not complete.
    fn evict(&mut self, policy: &RetentionPolicy) -> Result<RetentionReport>;
}

/// Capture history backed by SQLite and a content-addressed blob directory.
#[derive(Debug)]
pub struct SqliteStore {
    layout: StoreLayout,
    conn: Connection,
}

impl SqliteStore {
    /// Opens history in the platform data directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if there is no data directory or the store
    /// cannot be opened.
    pub fn open_default() -> Result<Self> {
        Self::open(StoreLayout::default_location()?.root())
    }

    /// Opens history rooted at `root`, creating it if needed.
    ///
    /// A damaged index is quarantined and rebuilt from the durable records
    /// rather than being treated as fatal. An index that is merely *behind* is
    /// migrated forward.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the store cannot be opened even after
    /// recovery — a full disk, or a directory that is not writable.
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let layout = StoreLayout::new(root.as_ref());
        layout.ensure_dirs()?;

        let mut store = match Self::open_healthy(&layout) {
            Ok(store) => store,
            Err(err) if db::is_corruption_error(&err) => {
                tracing::error!(error = %err, "history index is damaged; rebuilding from records");
                Self::rebuild_from_records(&layout)?
            }
            Err(err) => return Err(err),
        };

        store.adopt_unindexed_records()?;
        Ok(store)
    }

    /// Opens an index that never touches the disk, for tests and dry runs.
    ///
    /// Blobs and records still land under `root`; only the query index is
    /// ephemeral.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the schema cannot be created.
    pub fn open_ephemeral(root: impl AsRef<Path>) -> Result<Self> {
        let layout = StoreLayout::new(root.as_ref());
        layout.ensure_dirs()?;
        let mut conn = db::open_in_memory()?;
        schema::migrate(&mut conn, schema::MIGRATIONS)?;
        Ok(Self { layout, conn })
    }

    fn open_healthy(layout: &StoreLayout) -> Result<Self> {
        let mut conn = db::open(&layout.index_path())?;

        if !db::is_healthy(&conn)? {
            return Err(Error::Storage(
                "history index failed its integrity check: database disk image is malformed".into(),
            ));
        }
        schema::migrate(&mut conn, schema::MIGRATIONS)?;

        Ok(Self {
            layout: layout.clone(),
            conn,
        })
    }

    fn rebuild_from_records(layout: &StoreLayout) -> Result<Self> {
        let stamp = Timestamp::now().0;
        let quarantined = layout.quarantine_index(stamp)?;

        let mut conn = db::open(&layout.index_path())?;
        schema::migrate(&mut conn, schema::MIGRATIONS)?;

        let mut store = Self {
            layout: layout.clone(),
            conn,
        };
        let mut report = store.reconcile()?;
        report.quarantined = quarantined;
        tracing::warn!(
            recovered = report.records_recovered,
            unreadable = report.records_unreadable,
            quarantined = ?report.quarantined,
            "rebuilt history index from durable records"
        );
        Ok(store)
    }

    /// The paths this store uses.
    #[must_use]
    pub fn layout(&self) -> &StoreLayout {
        &self.layout
    }

    /// The schema version of the open index.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the pragma cannot be read.
    pub fn schema_version(&self) -> Result<u32> {
        schema::schema_version(&self.conn)
    }

    /// Quarantines the current index and rebuilds it from durable records.
    ///
    /// Exposed so a user who suspects their history is wrong can ask for this
    /// without deleting anything: the old index is moved aside, not removed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the rebuild fails.
    pub fn recover(&mut self) -> Result<RecoveryReport> {
        // Replace the live connection first: the old one must be closed before
        // its file can be renamed on Windows, and must not be used afterwards.
        let dead = std::mem::replace(&mut self.conn, db::open_in_memory()?);
        drop(dead);

        let stamp = Timestamp::now().0;
        let quarantined = self.layout.quarantine_index(stamp)?;

        let mut conn = db::open(&self.layout.index_path())?;
        schema::migrate(&mut conn, schema::MIGRATIONS)?;
        self.conn = conn;

        let mut report = self.reconcile()?;
        report.quarantined = quarantined;
        Ok(report)
    }

    /// Makes the index agree with what is on disk.
    ///
    /// The durable records are authoritative: rows without one are dropped,
    /// records without a row are adopted, and a record whose blob has vanished
    /// is marked evicted rather than deleted — losing pixels is not losing a
    /// capture (decision D23).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the rebuild fails.
    pub fn reconcile(&mut self) -> Result<RecoveryReport> {
        let (records, failures) = self.layout.scan_records()?;
        let blobs = self.layout.scan_blobs()?;

        let mut report = RecoveryReport {
            records_unreadable: failures.len(),
            blobs_found: blobs.len(),
            bytes_found: blobs.iter().map(|(_, len)| *len).sum(),
            ..RecoveryReport::default()
        };
        for (path, why) in &failures {
            tracing::warn!(path = %path.display(), reason = %why, "unreadable capture record");
        }

        let now = Timestamp::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin rebuild"))?;

        tx.execute("DELETE FROM blobs", [])
            .map_err(store_err("cannot reset blob table"))?;
        for (hash, byte_len) in &blobs {
            tx.execute(
                "INSERT INTO blobs (hash, byte_len, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (hash) DO UPDATE SET byte_len = excluded.byte_len",
                params![hash, i64::try_from(*byte_len).unwrap_or(i64::MAX), now.0],
            )
            .map_err(store_err("cannot record blob"))?;
        }

        let mut keep: Vec<String> = Vec::with_capacity(records.len());
        for mut record in records {
            let present = match &record.image_hash {
                Some(hash) => blobs.iter().find(|(h, _)| h == hash).map(|(_, len)| *len),
                None => None,
            };
            match (record.image_hash.is_some(), present) {
                (true, Some(len)) if record.image_evicted_at.is_none() => record.image_bytes = len,
                (true, None) if record.image_evicted_at.is_none() => {
                    // The pixels are gone but the record is not. That is
                    // precisely the state D23 describes, so write it down
                    // rather than treating it as damage.
                    record.mark_evicted(now);
                    report.images_missing += 1;
                    self.layout.write_record(&record)?;
                }
                _ => {}
            }

            upsert_record(&tx, &record)?;
            keep.push(record.id.clone());
            report.records_recovered += 1;
        }

        report.rows_dropped = drop_rows_without_records(&tx, &keep)?;

        tx.execute(
            "INSERT INTO store_meta (key, value) VALUES ('last_reconcile', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![now.0.to_string()],
        )
        .map_err(store_err("cannot record reconcile time"))?;

        tx.commit().map_err(store_err("cannot commit rebuild"))?;
        Ok(report)
    }

    /// Adopts records written by a process that crashed before committing.
    ///
    /// Insert writes the durable record *before* the index row, so the only way
    /// the two can disagree after a crash is a record with no row — a capture
    /// the user took and would otherwise silently lose. Comparing counts is one
    /// directory listing, cheap enough to do on every open.
    fn adopt_unindexed_records(&mut self) -> Result<()> {
        let on_disk = count_record_files(&self.layout)?;
        let indexed = self.count()? as usize;
        if on_disk == indexed {
            return Ok(());
        }

        tracing::info!(
            on_disk,
            indexed,
            "history index disagrees with durable records; reconciling"
        );
        self.reconcile().map(|_| ())
    }

    /// Removes blobs no capture refers to any more.
    ///
    /// Orphans are produced by a crash between committing an eviction and
    /// unlinking its file. They are wasted space rather than damage, so they
    /// are swept here instead of being repaired urgently.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the sweep fails.
    pub fn collect_garbage(&mut self) -> Result<u64> {
        let referenced: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT DISTINCT image_hash FROM captures
                     WHERE image_hash IS NOT NULL AND image_evicted_at IS NULL",
                )
                .map_err(store_err("cannot list referenced blobs"))?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(store_err("cannot list referenced blobs"))?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(store_err("cannot list referenced blobs"))?
        };

        let mut reclaimed = 0u64;
        for (hash, byte_len) in self.layout.scan_blobs()? {
            if referenced.iter().any(|h| h == &hash) {
                continue;
            }
            if self.layout.delete_blob(&hash)? {
                reclaimed += byte_len;
            }
            self.conn
                .execute("DELETE FROM blobs WHERE hash = ?1", params![hash])
                .map_err(store_err("cannot forget blob"))?;
        }
        Ok(reclaimed)
    }

    /// The durable record for `id`, or `None`.
    fn stored_record(&self, id: &CaptureId) -> Result<Option<StoredRecord>> {
        self.layout.read_record(id)
    }

    /// Reads a blob, repairing the index if the file has vanished.
    ///
    /// A missing file can only mean an eviction that was interrupted between
    /// unlinking and committing. Treating it as eviction — which it is — keeps
    /// the capture and its edits and costs one `UPDATE`.
    fn read_blob_repairing(&mut self, id: &CaptureId, hash: &str) -> Result<Option<Vec<u8>>> {
        if let Some(bytes) = self.layout.read_blob(hash)? {
            return Ok(Some(bytes));
        }

        tracing::warn!(
            capture = %id.0,
            %hash,
            "source image is missing from disk; recording it as evicted"
        );
        let now = Timestamp::now();
        self.conn
            .execute(
                "UPDATE captures SET image_evicted_at = ?2, image_bytes = 0 WHERE id = ?1",
                params![id.0, now.0],
            )
            .map_err(store_err("cannot record missing image"))?;
        self.conn
            .execute("DELETE FROM blobs WHERE hash = ?1", params![hash])
            .map_err(store_err("cannot forget missing blob"))?;

        if let Some(mut record) = self.stored_record(id)? {
            record.mark_evicted(now);
            self.layout.write_record(&record)?;
        }
        Ok(None)
    }

    fn require_record(&self, id: &CaptureId) -> Result<StoredRecord> {
        self.stored_record(id)?
            .ok_or_else(|| Error::Storage(format!("no capture {} in history", id.0)))
    }
}

impl Store for SqliteStore {
    fn list(&self) -> Result<Vec<CaptureId>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM captures ORDER BY created_at DESC, id DESC")
            .map_err(store_err("cannot list history"))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0).map(CaptureId))
            .map_err(store_err("cannot list history"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(store_err("cannot list history"))
    }

    fn set_pinned(&mut self, id: &CaptureId, pinned: bool) -> Result<()> {
        let mut record = self.require_record(id)?;
        record.pinned = pinned;
        self.layout.write_record(&record)?;

        let changed = self
            .conn
            .execute(
                "UPDATE captures SET pinned = ?2 WHERE id = ?1",
                params![id.0, i64::from(pinned)],
            )
            .map_err(store_err("cannot change pinned state"))?;
        if changed == 0 {
            return Err(Error::Storage(format!("no capture {} in history", id.0)));
        }
        Ok(())
    }

    fn enforce_retention(&mut self, policy: &RetentionPolicy) -> Result<()> {
        self.evict(policy).map(|_| ())
    }
}

impl History for SqliteStore {
    fn insert(&mut self, capture: NewCapture<'_>) -> Result<CaptureId> {
        let frame = &capture.document.source.frame;
        if !frame.is_well_formed() {
            return Err(Error::InvalidRequest(format!(
                "frame declares {}×{} at stride {} but holds {} bytes",
                frame.width(),
                frame.height(),
                frame.stride,
                frame.data.len()
            )));
        }

        let id = capture_id_at(capture.created_at.0);
        let digest = hash::content_hash(&frame.data);
        let byte_len = frame.data.len() as u64;
        let now = Timestamp::now();

        let record = StoredRecord::from_parts(
            &id,
            capture.created_at,
            now,
            capture.pinned,
            capture.app_name,
            capture.window_title,
            capture.document.source.provenance,
            &capture.document.source.target,
            FrameHeader::of(frame),
            Some(digest.clone()),
            byte_len,
            capture.ocr_text,
            &capture.document.data(),
        )?;

        // Pixels, then the durable record, then the index row. Each step is
        // recoverable from the one before it: a blob with no record is swept as
        // garbage, and a record with no row is adopted on the next open. The
        // reverse order would let a crash produce a row pointing at nothing.
        self.layout.write_blob(&digest, &frame.data)?;
        self.layout.write_record(&record)?;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin insert"))?;

        // Re-check inside the write lock. Another process may have evicted this
        // exact blob between the write above and this transaction; eviction
        // deletes inside its own write transaction, so holding the lock here is
        // what makes the check conclusive.
        if !self.layout.blob_exists(&digest)? {
            self.layout.write_blob(&digest, &frame.data)?;
        }
        tx.execute(
            "INSERT INTO blobs (hash, byte_len, created_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (hash) DO UPDATE SET byte_len = excluded.byte_len",
            params![digest, i64::try_from(byte_len).unwrap_or(i64::MAX), now.0],
        )
        .map_err(store_err("cannot record blob"))?;

        upsert_record(&tx, &record)?;
        tx.commit().map_err(store_err("cannot commit insert"))?;

        Ok(id)
    }

    fn record(&self, id: &CaptureId) -> Result<Option<CaptureRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {RECORD_COLUMNS} FROM captures WHERE id = ?1"),
                params![id.0],
                |row| Ok(row_to_record(row)),
            )
            .optional()
            .map_err(store_err("cannot read capture"))?
            .transpose()
    }

    fn document(&mut self, id: &CaptureId) -> Result<Option<DocumentState>> {
        let Some(record) = self.stored_record(id)? else {
            return Ok(None);
        };
        let data = record.document_data()?;

        let pixels = match record.image_state() {
            ImageState::Present { ref hash, .. } => self.read_blob_repairing(id, hash)?,
            ImageState::Evicted { .. } | ImageState::Absent => None,
        };

        let Some(pixels) = pixels else {
            return Ok(Some(DocumentState::ImageEvicted(EvictedDocument {
                record: self
                    .record(id)?
                    .unwrap_or_else(|| record.to_capture_record()),
                data,
            })));
        };

        let header = &record.frame;
        let capture = Capture {
            frame: Frame {
                data: pixels,
                size: header.size,
                stride: header.stride,
                format: header.format,
                color_space: header.color_space,
                scale: header.scale,
            },
            provenance: record.provenance.into(),
            target: record.target.clone().into(),
        };
        Ok(Some(DocumentState::Complete(Document::from_data(
            capture, data,
        )?)))
    }

    fn image(&mut self, id: &CaptureId) -> Result<Option<Vec<u8>>> {
        let Some(record) = self.stored_record(id)? else {
            return Ok(None);
        };
        match record.image_state() {
            ImageState::Present { ref hash, .. } => self.read_blob_repairing(id, hash),
            ImageState::Evicted { .. } | ImageState::Absent => Ok(None),
        }
    }

    fn page(&self, page: Page) -> Result<Vec<CaptureRecord>> {
        self.search(&SearchQuery {
            page,
            ..SearchQuery::default()
        })
    }

    fn search(&self, query: &SearchQuery) -> Result<Vec<CaptureRecord>> {
        let (sql, args) = build_search(query);
        let mut stmt = self
            .conn
            .prepare(&sql)
            .map_err(store_err("cannot prepare search"))?;
        let rows = stmt
            .query_map(params_from_iter(args.iter()), |row| Ok(row_to_record(row)))
            .map_err(store_err("cannot run search"))?;

        let mut found = Vec::new();
        for row in rows {
            found.push(row.map_err(store_err("cannot read search result"))??);
        }
        Ok(found)
    }

    fn count(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM captures", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(store_err("cannot count history"))
    }

    fn delete(&mut self, id: &CaptureId) -> Result<bool> {
        let Some(record) = self.stored_record(id)? else {
            // The record may be gone while a stale row survives; clear it too.
            let removed = self
                .conn
                .execute("DELETE FROM captures WHERE id = ?1", params![id.0])
                .map_err(store_err("cannot delete capture"))?;
            return Ok(removed > 0);
        };

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin delete"))?;
        tx.execute("DELETE FROM captures WHERE id = ?1", params![id.0])
            .map_err(store_err("cannot delete capture"))?;

        if let Some(hash) = record.image_hash.as_deref()
            && !blob_still_referenced(&tx, hash)?
        {
            tx.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])
                .map_err(store_err("cannot forget blob"))?;
            self.layout.delete_blob(hash)?;
        }
        tx.commit().map_err(store_err("cannot commit delete"))?;

        self.layout.delete_record(id)?;
        Ok(true)
    }

    fn save_document(&mut self, id: &CaptureId, document: &Document) -> Result<()> {
        self.save_edits(id, &document.data())
    }

    fn save_edits(&mut self, id: &CaptureId, data: &DocumentData) -> Result<()> {
        let mut record = self.require_record(id)?;
        record.set_document(data)?;

        // The durable record first, then the index. The index only caches the
        // count, so the worst a crash between them can do is show a stale
        // number until the next reconcile.
        self.layout.write_record(&record)?;
        self.conn
            .execute(
                "UPDATE captures SET annotation_count = ?2 WHERE id = ?1",
                params![
                    id.0,
                    i64::try_from(data.annotations.len()).unwrap_or(i64::MAX)
                ],
            )
            .map_err(store_err("cannot record edit count"))?;
        Ok(())
    }

    fn set_ocr_text(&mut self, id: &CaptureId, text: Option<&str>) -> Result<()> {
        let mut record = self.require_record(id)?;
        record.ocr_text = text.map(ToOwned::to_owned);
        self.layout.write_record(&record)?;

        self.conn
            .execute(
                "UPDATE captures SET ocr_text = ?2, ocr_fold = ?3, search_fold = ?4 WHERE id = ?1",
                params![
                    id.0,
                    record.ocr_text,
                    record.ocr_text.as_ref().map(|t| t.to_lowercase()),
                    record.search_text()
                ],
            )
            .map_err(store_err("cannot record recognised text"))?;
        Ok(())
    }

    fn stored_image_bytes(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT COALESCE(SUM(byte_len), 0) FROM blobs", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(store_err("cannot measure stored images"))
    }

    fn evict(&mut self, policy: &RetentionPolicy) -> Result<RetentionReport> {
        let now = Timestamp::now();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin retention"))?;

        let mut total = sum_blob_bytes(&tx)?;
        let pinned_bytes: u64 = tx
            .query_row(
                "SELECT COALESCE(SUM(b.byte_len), 0) FROM blobs b
                 WHERE EXISTS (
                     SELECT 1 FROM captures c
                     WHERE c.image_hash = b.hash AND c.image_evicted_at IS NULL AND c.pinned = 1
                 )",
                [],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(store_err("cannot measure pinned images"))?;

        let mut report = RetentionReport {
            bytes_remaining: total,
            pinned_bytes,
            ..RetentionReport::default()
        };

        if total <= policy.max_image_bytes {
            tx.rollback().map_err(store_err("cannot end retention"))?;
            return Ok(report);
        }

        // Oldest first. `id` breaks ties inside a millisecond and is itself
        // chronological, so the order is total and stable across processes.
        let candidates: Vec<(String, String)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT id, image_hash FROM captures
                     WHERE image_hash IS NOT NULL AND image_evicted_at IS NULL AND pinned = 0
                     ORDER BY created_at ASC, id ASC",
                )
                .map_err(store_err("cannot find evictable captures"))?;
            let rows = stmt
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(store_err("cannot find evictable captures"))?;
            rows.collect::<std::result::Result<_, _>>()
                .map_err(store_err("cannot find evictable captures"))?
        };

        let mut rewritten = Vec::new();
        for (id, digest) in candidates {
            if total <= policy.max_image_bytes {
                break;
            }

            // The document is untouched. Only the pixels go. This one statement
            // is the whole of decision D23.
            tx.execute(
                "UPDATE captures SET image_evicted_at = ?2, image_bytes = 0 WHERE id = ?1",
                params![id, now.0],
            )
            .map_err(store_err("cannot evict image"))?;

            if !blob_still_referenced(&tx, &digest)? {
                let byte_len: u64 = tx
                    .query_row(
                        "SELECT byte_len FROM blobs WHERE hash = ?1",
                        params![digest],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map_err(store_err("cannot size blob"))?
                    .map_or(0, |n| u64::try_from(n).unwrap_or(0));

                tx.execute("DELETE FROM blobs WHERE hash = ?1", params![digest])
                    .map_err(store_err("cannot forget blob"))?;
                // Unlinked inside the transaction on purpose: a concurrent
                // insert that would dedupe onto this blob is blocked by the
                // same write lock, so it cannot observe the file mid-removal.
                self.layout.delete_blob(&digest)?;
                total = total.saturating_sub(byte_len);
                report.bytes_reclaimed += byte_len;
            }

            rewritten.push(CaptureId(id.clone()));
            report.evicted.push(CaptureId(id));
        }

        report.bytes_remaining = total;
        report.cap_unreachable = total > policy.max_image_bytes;
        if report.cap_unreachable {
            tracing::info!(
                cap = policy.max_image_bytes,
                remaining = total,
                pinned_bytes,
                "retention cap not reached; the remainder is pinned and pinned captures are never evicted"
            );
        }

        tx.commit().map_err(store_err("cannot commit retention"))?;

        // Durable records last: if this is interrupted, the next open finds a
        // record claiming pixels that are not there and marks it evicted, which
        // is the same end state.
        for id in rewritten {
            if let Some(mut record) = self.stored_record(&id)? {
                record.mark_evicted(now);
                self.layout.write_record(&record)?;
            }
        }

        Ok(report)
    }
}

fn store_err(context: &'static str) -> impl Fn(rusqlite::Error) -> Error {
    move |err| Error::Storage(format!("{context}: {err}"))
}

fn sum_blob_bytes(conn: &Connection) -> Result<u64> {
    conn.query_row("SELECT COALESCE(SUM(byte_len), 0) FROM blobs", [], |row| {
        row.get::<_, i64>(0)
    })
    .map(|n| u64::try_from(n).unwrap_or(0))
    .map_err(store_err("cannot measure stored images"))
}

fn blob_still_referenced(conn: &Connection, hash: &str) -> Result<bool> {
    conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM captures
             WHERE image_hash = ?1 AND image_evicted_at IS NULL
         )",
        params![hash],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n != 0)
    .map_err(store_err("cannot count blob references"))
}

fn upsert_record(conn: &Connection, record: &StoredRecord) -> Result<()> {
    let target_json = serde_json::to_string(&record.target)
        .map_err(|e| Error::Storage(format!("cannot serialise target: {e}")))?;
    let frame_json = serde_json::to_string(&record.frame)
        .map_err(|e| Error::Storage(format!("cannot serialise frame: {e}")))?;

    conn.execute(
        "INSERT INTO captures (
             id, created_at, stored_at, pinned, app_name, window_title, provenance,
             target_json, frame_json, image_hash, image_bytes, image_evicted_at,
             ocr_text, annotation_count, search_fold, app_fold, title_fold, ocr_fold
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18
         )
         ON CONFLICT (id) DO UPDATE SET
             created_at = excluded.created_at,
             stored_at = excluded.stored_at,
             pinned = excluded.pinned,
             app_name = excluded.app_name,
             window_title = excluded.window_title,
             provenance = excluded.provenance,
             target_json = excluded.target_json,
             frame_json = excluded.frame_json,
             image_hash = excluded.image_hash,
             image_bytes = excluded.image_bytes,
             image_evicted_at = excluded.image_evicted_at,
             ocr_text = excluded.ocr_text,
             annotation_count = excluded.annotation_count,
             search_fold = excluded.search_fold,
             app_fold = excluded.app_fold,
             title_fold = excluded.title_fold,
             ocr_fold = excluded.ocr_fold",
        params![
            record.id,
            record.created_at,
            record.stored_at,
            i64::from(record.pinned),
            record.app_name,
            record.window_title,
            record.provenance.as_token(),
            target_json,
            frame_json,
            record.image_hash,
            i64::try_from(record.image_bytes).unwrap_or(i64::MAX),
            record.image_evicted_at,
            record.ocr_text,
            i64::try_from(record.annotation_count()).unwrap_or(i64::MAX),
            record.search_text(),
            record.app_name.as_ref().map(|t| t.to_lowercase()),
            record.window_title.as_ref().map(|t| t.to_lowercase()),
            record.ocr_text.as_ref().map(|t| t.to_lowercase()),
        ],
    )
    .map_err(store_err("cannot write capture row"))?;
    Ok(())
}

fn drop_rows_without_records(conn: &Connection, keep: &[String]) -> Result<usize> {
    let existing: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT id FROM captures")
            .map_err(store_err("cannot list index rows"))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(store_err("cannot list index rows"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(store_err("cannot list index rows"))?
    };

    let mut dropped = 0;
    for id in existing {
        if keep.iter().any(|kept| kept == &id) {
            continue;
        }
        conn.execute("DELETE FROM captures WHERE id = ?1", params![id])
            .map_err(store_err("cannot drop orphan row"))?;
        dropped += 1;
    }
    Ok(dropped)
}

fn count_record_files(layout: &StoreLayout) -> Result<usize> {
    let dir = layout.documents_dir();
    if !dir.is_dir() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            count += 1;
        }
    }
    Ok(count)
}

/// Escapes a user's search text so `%` and `_` are literal.
///
/// Without this, searching for a window title containing an underscore matches
/// every title with any character in that position, which looks like the search
/// is broken rather than like a SQL wildcard leaked through.
fn like_pattern(needle: &str) -> String {
    let mut escaped = String::with_capacity(needle.len() + 2);
    escaped.push('%');
    for ch in needle.to_lowercase().chars() {
        if matches!(ch, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('%');
    escaped
}

fn build_search(query: &SearchQuery) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = format!("SELECT {RECORD_COLUMNS} FROM captures WHERE 1 = 1");
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();

    let like = |sql: &mut String, column: &str, needle: &str, args: &mut Vec<Box<dyn ToSql>>| {
        args.push(Box::new(like_pattern(needle)));
        sql.push_str(&format!(" AND {column} LIKE ?{} ESCAPE '\\'", args.len()));
    };

    if let Some(text) = &query.text {
        like(&mut sql, "search_fold", text, &mut args);
    }
    if let Some(app) = &query.app_name {
        like(&mut sql, "app_fold", app, &mut args);
    }
    if let Some(title) = &query.window_title {
        like(&mut sql, "title_fold", title, &mut args);
    }
    if let Some(ocr) = &query.ocr_text {
        like(&mut sql, "ocr_fold", ocr, &mut args);
    }
    if let Some(after) = query.created_after {
        args.push(Box::new(after.0));
        sql.push_str(&format!(" AND created_at >= ?{}", args.len()));
    }
    if let Some(before) = query.created_before {
        args.push(Box::new(before.0));
        sql.push_str(&format!(" AND created_at <= ?{}", args.len()));
    }
    if query.pinned_only {
        sql.push_str(" AND pinned = 1");
    }
    if query.images_only {
        sql.push_str(" AND image_hash IS NOT NULL AND image_evicted_at IS NULL");
    }

    sql.push_str(" ORDER BY created_at DESC, id DESC");
    args.push(Box::new(i64::from(query.page.limit)));
    sql.push_str(&format!(" LIMIT ?{}", args.len()));
    args.push(Box::new(i64::from(query.page.offset)));
    sql.push_str(&format!(" OFFSET ?{}", args.len()));

    (sql, args)
}

/// Reads one row into a record. The inner `Result` carries decoding failures,
/// which are a storage problem rather than a SQLite one.
fn row_to_record(row: &Row<'_>) -> Result<CaptureRecord> {
    let id: String = get(row, 0)?;
    let created_at: i64 = get(row, 1)?;
    let pinned: i64 = get(row, 2)?;
    let app_name: Option<String> = get(row, 3)?;
    let window_title: Option<String> = get(row, 4)?;
    let provenance: String = get(row, 5)?;
    let target_json: String = get(row, 6)?;
    let frame_json: String = get(row, 7)?;
    let image_hash: Option<String> = get(row, 8)?;
    let image_bytes: i64 = get(row, 9)?;
    let image_evicted_at: Option<i64> = get(row, 10)?;
    let ocr_text: Option<String> = get(row, 11)?;
    let annotation_count: i64 = get(row, 12)?;

    let target: TargetRepr = serde_json::from_str(&target_json)
        .map_err(|e| Error::Storage(format!("cannot read target for {id}: {e}")))?;
    let frame: FrameHeader = serde_json::from_str(&frame_json)
        .map_err(|e| Error::Storage(format!("cannot read frame for {id}: {e}")))?;

    let image = match (image_hash, image_evicted_at) {
        (Some(hash), None) => ImageState::Present {
            hash,
            byte_len: u64::try_from(image_bytes).unwrap_or(0),
        },
        (Some(hash), Some(at)) => ImageState::Evicted {
            at: Timestamp(at),
            was_hash: hash,
        },
        (None, Some(at)) => ImageState::Evicted {
            at: Timestamp(at),
            was_hash: String::new(),
        },
        (None, None) => ImageState::Absent,
    };

    Ok(CaptureRecord {
        id: CaptureId(id),
        created_at: Timestamp(created_at),
        pinned: pinned != 0,
        app_name,
        window_title,
        provenance: ProvenanceRepr::from_token(&provenance)?.into(),
        target: target.into(),
        frame,
        image,
        ocr_text,
        annotation_count: usize::try_from(annotation_count).unwrap_or(0),
    })
}

fn get<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T> {
    row.get(index)
        .map_err(|e| Error::Storage(format!("cannot read column {index}: {e}")))
}
