//! The SQLite-backed history store.

use std::path::{Path, PathBuf};

use rusqlite::{
    Connection, OptionalExtension as _, Row, ToSql, TransactionBehavior, params, params_from_iter,
};
use scrozz_annotate::{AnnotationObject, Document, DocumentData};
use scrozz_core::{Capture, Error, Frame, PinState, Result};

use crate::{
    CaptureId, RetentionPolicy, Store, db, hash,
    id::capture_id_at,
    layout::{PendingIndexUpdate, StoreLayout},
    model::{
        CaptureRecord, FrameHeader, ImageState, MediaKind, Page, ProvenanceRepr, RetentionReport,
        SearchQuery, TargetRepr, Timestamp, VideoMetadata,
    },
    record::StoredRecord,
    schema,
    sharing::{CaptureSharing, RemoteDeletionState, RemoteObjectStatus},
};

/// Columns every record query selects, in the order [`row_to_record`] reads.
const RECORD_COLUMNS: &str = "captures.id, captures.created_at, captures.media_kind, \
     captures.pinned, captures.app_name, captures.app_identifier, captures.window_title, \
     captures.window_shadow, captures.provenance, captures.target_json, captures.frame_json, \
     captures.video_json, captures.image_hash, captures.image_bytes, captures.image_evicted_at, \
     captures.ocr_text, captures.annotation_count, \
     (SELECT pin_json FROM capture_pins WHERE capture_pins.capture_id = captures.id), \
     capture_shares.sharing_json";

/// The table expression [`RECORD_COLUMNS`] is selected from.
///
/// A left join rather than another correlated subquery because sharing is read
/// on every history page; a capture that was never shared simply yields NULL.
const RECORD_SOURCE: &str =
    "captures LEFT JOIN capture_shares ON capture_shares.capture_id = captures.id";

/// Records that the one-time legacy sidecar/cache comparison has completed.
const PIN_CACHE_BOOTSTRAP_KEY: &str = "pin_cache_sidecars_v1";

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
    /// Still, video or GIF. Defaults to a screenshot.
    pub media_kind: MediaKind,
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
        Self::of_kind(document, MediaKind::Screenshot)
    }

    /// A capture of `document` with an explicit media kind.
    #[must_use]
    pub fn of_kind(document: &'a Document, media_kind: MediaKind) -> Self {
        Self {
            document,
            created_at: Timestamp::now(),
            media_kind,
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

/// A video recording on its way into history.
///
/// Unlike [`NewCapture`], which owns a document with inline pixels, a recording
/// refers to an **externally-owned durable media file** that history never
/// deletes. The path must be absolute, canonical, non-empty, and point to an
/// existing regular file at insert time.
#[derive(Debug, Clone)]
pub struct NewRecording {
    /// Typed metadata for the video file.
    pub video: VideoMetadata,
    /// When the recording was finalised. Defaults to now.
    pub created_at: Timestamp,
    /// Owning application, if known.
    pub app_name: Option<String>,
    /// Stable application identifier (e.g. bundle ID).
    pub app_identifier: Option<String>,
    /// Window title, if known.
    pub window_title: Option<String>,
    /// Whether the captured window included its native shadow.
    pub window_shadow: Option<bool>,
    /// How the capture was produced.
    pub provenance: scrozz_core::Provenance,
    /// What it was aimed at.
    pub target: scrozz_core::CaptureTarget,
    /// Whether to pin it immediately.
    pub pinned: bool,
}

impl NewRecording {
    /// A recording with the given video metadata, taken now.
    #[must_use]
    pub fn new(video: VideoMetadata) -> Self {
        Self {
            video,
            created_at: Timestamp::now(),
            app_name: None,
            app_identifier: None,
            window_title: None,
            window_shadow: None,
            provenance: scrozz_core::Provenance::Region,
            target: scrozz_core::CaptureTarget::AllDisplays,
            pinned: false,
        }
    }

    /// Records the owning application.
    #[must_use]
    pub fn from_app(mut self, app: impl Into<String>) -> Self {
        self.app_name = Some(app.into());
        self
    }

    /// Records the stable application identifier.
    #[must_use]
    pub fn with_app_identifier(mut self, id: impl Into<String>) -> Self {
        self.app_identifier = Some(id.into());
        self
    }

    /// Records the window title.
    #[must_use]
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.window_title = Some(title.into());
        self
    }

    /// Records whether the window shadow was included.
    #[must_use]
    pub const fn with_window_shadow(mut self, shadow: bool) -> Self {
        self.window_shadow = Some(shadow);
        self
    }

    /// Sets provenance.
    #[must_use]
    pub const fn with_provenance(mut self, provenance: scrozz_core::Provenance) -> Self {
        self.provenance = provenance;
        self
    }

    /// Sets target.
    #[must_use]
    pub fn with_target(mut self, target: scrozz_core::CaptureTarget) -> Self {
        self.target = target;
        self
    }

    /// Overrides the capture time.
    #[must_use]
    pub const fn taken_at(mut self, at: Timestamp) -> Self {
        self.created_at = at;
        self
    }

    /// Pins the recording on arrival.
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
    Complete(Box<Document>),
    /// The pixels were evicted under the size cap. Every edit is intact.
    ImageEvicted(Box<EvictedDocument>),
}

impl DocumentState {
    /// The document, if its pixels are still present.
    #[must_use]
    pub fn complete(self) -> Option<Document> {
        match self {
            Self::Complete(document) => Some(*document),
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

    /// Adds a video recording to history, returning its new identifier.
    ///
    /// The `video.path` must be absolute, canonical, non-empty, and point to an
    /// existing regular file. History never deletes this file — only its own
    /// sidecar, index row, and poster blob (if any) are removed on deletion or
    /// eviction.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the video path fails validation, or
    /// [`Error::Storage`] if the write fails.
    fn insert_recording(&mut self, recording: NewRecording) -> Result<CaptureId>;

    /// Everything known about one capture, without its pixels.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the index is unreadable.
    fn record(&self, id: &CaptureId) -> Result<Option<CaptureRecord>>;

    /// Persists or clears the state of an on-screen pinned window.
    ///
    /// Setting a state also sets the retention pin; clearing it removes both so
    /// a closed window cannot reappear after restart.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the capture is unknown or the write fails.
    fn set_screen_pin(&mut self, id: &CaptureId, state: Option<&PinState>) -> Result<()>;

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

    /// How many captures match `query`, ignoring its pagination.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the index is unreadable.
    fn count_matching(&self, query: &SearchQuery) -> Result<u64>;

    /// Distinct application names represented in history, case-insensitively sorted.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the index is unreadable.
    fn apps(&self) -> Result<Vec<String>>;

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

    /// Reads the capture's sharing metadata, if any.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the capture or metadata cannot be read.
    fn share_metadata(&self, id: &CaptureId) -> Result<Option<CaptureSharing>>;

    /// Replaces the capture's sharing metadata.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the capture is unknown or the write fails,
    /// or [`Error::InvalidRequest`] if the metadata is unsafe to persist.
    fn set_share_metadata(&mut self, id: &CaptureId, sharing: Option<CaptureSharing>)
    -> Result<()>;

    /// Updates only the remote-object status.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the capture has no sharing metadata or the
    /// write fails.
    fn set_share_remote_status(&mut self, id: &CaptureId, status: RemoteObjectStatus)
    -> Result<()>;

    /// Updates only the remote deletion state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the capture has no sharing metadata or the
    /// write fails.
    fn set_share_deletion_state(
        &mut self,
        id: &CaptureId,
        deletion: RemoteDeletionState,
    ) -> Result<()>;

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

        store.synchronize_index_on_open()?;
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin rebuild"))?;
        let pending = self.layout.pending_index_updates()?;
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
        mark_pin_cache_bootstrapped(&tx)?;

        tx.commit().map_err(store_err("cannot commit rebuild"))?;
        clear_index_markers(&self.layout, pending);
        Ok(report)
    }

    /// Repairs sidecar-authoritative caches after interrupted writes.
    ///
    /// Existing stores get one complete comparison to seed the cache safely.
    /// Thereafter, clean opens retain the original cheap record-count check while
    /// crash markers identify the exact sidecars that need refreshing.
    fn synchronize_index_on_open(&mut self) -> Result<()> {
        let on_disk = count_record_files(&self.layout)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin startup synchronization"))?;
        let pending = self.layout.pending_index_updates()?;
        let indexed = tx
            .query_row("SELECT COUNT(*) FROM captures", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|count| usize::try_from(count).unwrap_or(0))
            .map_err(store_err("cannot count indexed captures"))?;
        let cache_bootstrapped = pin_cache_bootstrapped(&tx)?;

        if indexed != on_disk {
            tx.rollback()
                .map_err(store_err("cannot end startup synchronization"))?;
            tracing::info!(
                on_disk,
                indexed,
                "history index disagrees with durable records; reconciling"
            );
            return self.reconcile().map(|_| ());
        }

        if pending.is_empty() && cache_bootstrapped {
            tx.rollback()
                .map_err(store_err("cannot end startup synchronization"))?;
            return Ok(());
        }

        if !pending.is_empty()
            && (!cache_bootstrapped || !repair_pending_index_updates(&tx, &self.layout, &pending)?)
        {
            tx.rollback()
                .map_err(store_err("cannot end startup synchronization"))?;
            tracing::info!(
                markers = pending.len(),
                "history has unresolved index recovery markers; fully reconciling"
            );
            return self.reconcile().map(|_| ());
        }

        if pending.is_empty() && !cache_bootstrapped {
            let (records, failures) = self.layout.scan_records()?;
            if records.len() + failures.len() != indexed || !synchronize_pin_records(&tx, &records)?
            {
                tx.rollback()
                    .map_err(store_err("cannot end startup synchronization"))?;
                tracing::info!(
                    sidecars = records.len(),
                    unreadable = failures.len(),
                    indexed,
                    "history index disagrees with durable records; reconciling"
                );
                return self.reconcile().map(|_| ());
            }
        }

        mark_pin_cache_bootstrapped(&tx)?;
        tx.commit()
            .map_err(store_err("cannot commit startup synchronization"))?;
        clear_index_markers(&self.layout, pending);
        Ok(())
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin garbage collection"))?;
        let pending = self.layout.pending_index_updates()?;
        if !repair_pending_index_updates(&tx, &self.layout, &pending)? {
            tx.rollback()
                .map_err(store_err("cannot end garbage collection"))?;
            self.reconcile()?;
            return self.collect_garbage();
        }
        let referenced: Vec<String> = {
            let mut stmt = tx
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
            tx.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])
                .map_err(store_err("cannot forget blob"))?;
        }
        tx.commit()
            .map_err(store_err("cannot commit garbage collection"))?;
        clear_index_markers(&self.layout, pending);
        Ok(reclaimed)
    }

    /// Unlocks every persisted on-screen pin and returns how many changed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if a sidecar or its cache cannot be updated.
    pub fn unlock_screen_pins(&mut self) -> Result<u64> {
        const PAGE_SIZE: u32 = 500;
        let mut unlocked = 0u64;
        let mut offset = 0;
        loop {
            let records = self.search(&SearchQuery {
                pinned_only: true,
                page: Page::new(PAGE_SIZE, offset),
                ..SearchQuery::default()
            })?;
            if records.is_empty() {
                break;
            }
            offset = offset.saturating_add(PAGE_SIZE);
            for record in records {
                let Some(mut state) = record.screen_pin else {
                    continue;
                };
                if !state.locked {
                    continue;
                }
                state.locked = false;
                self.set_screen_pin(&record.id, Some(&state))?;
                unlocked += 1;
            }
        }
        Ok(unlocked)
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

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin missing-image repair"))?;
        // An insert may have restored this content while this writer waited.
        if let Some(bytes) = self.layout.read_blob(hash)? {
            tx.rollback()
                .map_err(store_err("cannot end missing-image repair"))?;
            return Ok(Some(bytes));
        }

        tracing::warn!(
            capture = %id.0,
            %hash,
            "source image is missing from disk; recording it as evicted"
        );
        let now = Timestamp::now();
        let mut marker = None;
        if let Some(mut record) = self.layout.read_record(id)?
            && record.image_hash.as_deref() == Some(hash)
            && record.image_evicted_at.is_none()
        {
            let pending = self.layout.begin_index_update(id)?;
            marker = Some(pending);
            record.mark_evicted(now);
            if let Err(error) = self.layout.write_record(&record) {
                drop(tx);
                finish_unused_index_marker(&self.layout, marker.as_deref());
                return Err(error);
            }
            if let Err(error) = upsert_record(&tx, &record) {
                drop(tx);
                return self.recover_partial_index_update(error).map(|()| None);
            }
        }

        if let Err(error) = tx
            .execute("DELETE FROM blobs WHERE hash = ?1", params![hash])
            .map_err(store_err("cannot forget missing blob"))
        {
            drop(tx);
            if marker.is_some() {
                return self.recover_partial_index_update(error).map(|()| None);
            }
            return Err(error);
        }
        if let Err(error) = tx
            .commit()
            .map_err(store_err("cannot commit missing-image repair"))
        {
            if marker.is_some() {
                return self.recover_partial_index_update(error).map(|()| None);
            }
            return Err(error);
        }
        finish_committed_index_marker(&self.layout, marker.as_deref());
        Ok(None)
    }

    /// Mutates one authoritative sidecar and refreshes every cache derived from it.
    ///
    /// The write lock is acquired before the sidecar is read. This is important:
    /// taking it only before the SQLite update lets two processes both read the
    /// old document and then overwrite each other's pin, OCR, or annotation edit.
    fn update_record(
        &mut self,
        id: &CaptureId,
        mutate: impl FnOnce(&mut StoredRecord) -> Result<()>,
    ) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin capture update"))?;
        let Some(mut record) = self.layout.read_record(id)? else {
            return Err(Error::Storage(format!("no capture {} in history", id.0)));
        };
        mutate(&mut record)?;
        let marker = self.layout.begin_index_update(id)?;

        if let Err(error) = self.layout.write_record(&record) {
            drop(tx);
            finish_unused_index_marker(&self.layout, Some(&marker));
            return Err(error);
        }

        if let Err(error) = upsert_record(&tx, &record) {
            drop(tx);
            return self.recover_partial_index_update(error);
        }
        if let Err(error) = tx
            .commit()
            .map_err(store_err("cannot commit capture update"))
        {
            return self.recover_partial_index_update(error);
        }

        finish_committed_index_marker(&self.layout, Some(&marker));
        Ok(())
    }

    fn recover_partial_index_update(&mut self, original: Error) -> Result<()> {
        tracing::warn!(
            error = %original,
            "capture sidecar was written but its index update failed; reconciling"
        );
        self.synchronize_index_on_open().map_err(|recovery| {
            Error::Storage(format!(
                "{original}; the durable sidecar was written, but index reconciliation also failed: {recovery}"
            ))
        })
    }

    fn recover_retention_failure(&mut self, original: Error) -> Error {
        tracing::warn!(
            error = %original,
            "retention changed a durable sidecar before failing; reconciling immediately"
        );
        match self.synchronize_index_on_open() {
            Ok(()) => original,
            Err(recovery) => Error::Storage(format!(
                "{original}; retention changed a durable sidecar, but index reconciliation also failed: {recovery}"
            )),
        }
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
        self.update_record(id, |record| {
            record.pinned = pinned;
            if !pinned {
                record.screen_pin = None;
            }
            Ok(())
        })
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
            capture.media_kind,
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

        // Blob contents are immutable, so this first write can race safely. The
        // authoritative sidecar waits for the shared SQLite write lock below.
        self.layout.write_blob(&digest, &frame.data)?;

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
        let marker = self.layout.begin_index_update(&id)?;
        if let Err(error) = self.layout.write_record(&record) {
            drop(tx);
            finish_unused_index_marker(&self.layout, Some(&marker));
            return Err(error);
        }
        if let Err(error) = tx
            .execute(
                "INSERT INTO blobs (hash, byte_len, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (hash) DO UPDATE SET byte_len = excluded.byte_len",
                params![digest, i64::try_from(byte_len).unwrap_or(i64::MAX), now.0],
            )
            .map_err(store_err("cannot record blob"))
        {
            drop(tx);
            self.recover_partial_index_update(error)?;
            return Ok(id);
        }

        if let Err(error) = upsert_record(&tx, &record) {
            drop(tx);
            self.recover_partial_index_update(error)?;
            return Ok(id);
        }
        if let Err(error) = tx.commit().map_err(store_err("cannot commit insert")) {
            self.recover_partial_index_update(error)?;
            return Ok(id);
        }
        finish_committed_index_marker(&self.layout, Some(&marker));

        Ok(id)
    }

    fn insert_recording(&mut self, recording: NewRecording) -> Result<CaptureId> {
        recording.video.validate()?;
        // The container decides the history category: a GIF export is a GIF in
        // the filter bar, not a video that happens to end in `.gif`.
        let media_kind = recording.video.media_kind();

        let id = capture_id_at(recording.created_at.0);
        let now = Timestamp::now();

        let mut record = StoredRecord::from_parts(
            &id,
            recording.created_at,
            now,
            media_kind,
            recording.pinned,
            recording.app_name,
            recording.window_title,
            recording.provenance,
            &recording.target,
            // No still-frame geometry for native video rows.
            FrameHeader {
                size: scrozz_core::PhysicalSize::new(0.0, 0.0),
                stride: 0,
                format: scrozz_core::PixelFormat::Rgba8,
                color_space: scrozz_core::ColorSpace::Srgb,
                scale: scrozz_core::ScaleFactor::IDENTITY,
            },
            None, // no content-addressed image blob
            0,
            None,
            &scrozz_annotate::DocumentData::default(),
        )?;
        // Patch the typed fields that from_parts doesn't cover.
        record.frame = None;
        record.video = Some(serde_json::to_value(recording.video).map_err(|error| {
            Error::Storage(format!("cannot serialise recording metadata: {error}"))
        })?);
        record.app_identifier = recording.app_identifier;
        record.window_shadow = recording.window_shadow;

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin recording insert"))?;
        let marker = self.layout.begin_index_update(&id)?;
        if let Err(error) = self.layout.write_record(&record) {
            drop(tx);
            finish_unused_index_marker(&self.layout, Some(&marker));
            return Err(error);
        }

        if let Err(error) = upsert_record(&tx, &record) {
            drop(tx);
            self.recover_partial_index_update(error)?;
            return Ok(id);
        }
        if let Err(error) = tx
            .commit()
            .map_err(store_err("cannot commit recording insert"))
        {
            self.recover_partial_index_update(error)?;
            return Ok(id);
        }
        finish_committed_index_marker(&self.layout, Some(&marker));

        Ok(id)
    }

    fn record(&self, id: &CaptureId) -> Result<Option<CaptureRecord>> {
        self.conn
            .query_row(
                &format!("SELECT {RECORD_COLUMNS} FROM {RECORD_SOURCE} WHERE captures.id = ?1"),
                params![id.0],
                |row| Ok(row_to_record(row)),
            )
            .optional()
            .map_err(store_err("cannot read capture"))?
            .transpose()
    }

    fn set_screen_pin(&mut self, id: &CaptureId, state: Option<&PinState>) -> Result<()> {
        self.update_record(id, |record| {
            record.screen_pin = state.cloned();
            record.pinned = state.is_some();
            Ok(())
        })
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
            return Ok(Some(DocumentState::ImageEvicted(Box::new(
                EvictedDocument {
                    record: self
                        .record(id)?
                        .unwrap_or_else(|| record.to_capture_record()),
                    data,
                },
            ))));
        };

        let header = record.frame.as_ref().ok_or_else(|| {
            Error::Storage(format!(
                "capture {} has media bytes but no still-frame geometry",
                record.id
            ))
        })?;
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
        Ok(Some(DocumentState::Complete(Box::new(
            Document::from_data(capture, data)?,
        ))))
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

        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(store_err("cannot read search result"))?
            .into_iter()
            .collect()
    }

    fn count(&self) -> Result<u64> {
        self.conn
            .query_row("SELECT COUNT(*) FROM captures", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(store_err("cannot count history"))
    }

    fn count_matching(&self, query: &SearchQuery) -> Result<u64> {
        let (sql, args) = build_count(query);
        self.conn
            .query_row(&sql, params_from_iter(args.iter()), |row| {
                row.get::<_, i64>(0)
            })
            .map(|n| u64::try_from(n).unwrap_or(0))
            .map_err(store_err("cannot count matching history"))
    }

    fn apps(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT MIN(app_name) FROM captures
                 WHERE app_name IS NOT NULL AND TRIM(app_name) <> ''
                 GROUP BY app_fold
                 ORDER BY app_fold ASC",
            )
            .map_err(store_err("cannot list history applications"))?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(store_err("cannot list history applications"))?;
        rows.collect::<std::result::Result<_, _>>()
            .map_err(store_err("cannot list history applications"))
    }

    fn delete(&mut self, id: &CaptureId) -> Result<bool> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_err("cannot begin delete"))?;
        let pending = self.layout.pending_index_updates()?;
        if !repair_pending_index_updates(&tx, &self.layout, &pending)? {
            tx.rollback().map_err(store_err("cannot end delete"))?;
            self.reconcile()?;
            return self.delete(id);
        }
        let Some(record) = self.layout.read_record(id)? else {
            // The record may be gone while a stale row survives; clear it too.
            let removed = tx
                .execute("DELETE FROM captures WHERE id = ?1", params![id.0])
                .map_err(store_err("cannot delete capture"))?;
            tx.commit().map_err(store_err("cannot commit delete"))?;
            clear_index_markers(&self.layout, pending);
            return Ok(removed > 0);
        };

        let marker = self.layout.begin_index_update(id)?;
        if let Err(error) = self.layout.delete_record(id) {
            drop(tx);
            finish_unused_index_marker(&self.layout, Some(&marker));
            return Err(error);
        }
        let indexed = (|| -> Result<()> {
            tx.execute("DELETE FROM captures WHERE id = ?1", params![id.0])
                .map_err(store_err("cannot delete capture"))?;

            if let Some(hash) = record.image_hash.as_deref()
                && !blob_still_referenced(&tx, hash)?
            {
                tx.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])
                    .map_err(store_err("cannot forget blob"))?;
                self.layout.delete_blob(hash)?;
            }
            Ok(())
        })();
        if let Err(error) = indexed {
            drop(tx);
            self.recover_partial_index_update(error)?;
            return Ok(true);
        }
        if let Err(error) = tx.commit().map_err(store_err("cannot commit delete")) {
            self.recover_partial_index_update(error)?;
            return Ok(true);
        }

        finish_committed_index_marker(&self.layout, Some(&marker));
        clear_index_markers(&self.layout, pending);
        Ok(true)
    }

    fn save_document(&mut self, id: &CaptureId, document: &Document) -> Result<()> {
        self.save_edits(id, &document.data())
    }

    fn save_edits(&mut self, id: &CaptureId, data: &DocumentData) -> Result<()> {
        self.update_record(id, |record| record.set_document(data))
    }

    fn set_ocr_text(&mut self, id: &CaptureId, text: Option<&str>) -> Result<()> {
        self.update_record(id, |record| {
            record.ocr_text = text.map(ToOwned::to_owned);
            Ok(())
        })
    }

    fn share_metadata(&self, id: &CaptureId) -> Result<Option<CaptureSharing>> {
        Ok(self.record(id)?.and_then(|record| record.sharing))
    }

    fn set_share_metadata(
        &mut self,
        id: &CaptureId,
        sharing: Option<CaptureSharing>,
    ) -> Result<()> {
        self.update_record(id, |record| record.set_sharing(sharing))
    }

    fn set_share_remote_status(
        &mut self,
        id: &CaptureId,
        status: RemoteObjectStatus,
    ) -> Result<()> {
        self.update_record(id, |record| record.set_remote_status(status))
    }

    fn set_share_deletion_state(
        &mut self,
        id: &CaptureId,
        deletion: RemoteDeletionState,
    ) -> Result<()> {
        self.update_record(id, |record| record.set_deletion(deletion))
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
        let pending = self.layout.pending_index_updates()?;
        if !repair_pending_index_updates(&tx, &self.layout, &pending)? {
            tx.rollback()
                .map_err(store_err("cannot end retention recovery"))?;
            self.reconcile()?;
            return self.evict(policy);
        }
        if !pending.is_empty() {
            mark_pin_cache_bootstrapped(&tx)?;
        }

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

        let mut candidates = Vec::new();
        if let Some(cutoff) = policy.max_image_age.cutoff(now) {
            let stale = eviction_candidates(
                &tx,
                "created_at < ?1",
                params![cutoff.0],
                "cannot find stale captures",
            )?;
            candidates.extend(stale.into_iter().map(|(id, digest)| (id, digest, true)));
        }

        if total > policy.max_image_bytes {
            // Oldest first. `id` breaks ties inside a millisecond and is itself
            // chronological, so the order is total and stable across processes.
            let over_cap = eviction_candidates(&tx, "1 = 1", [], "cannot find evictable captures")?;
            candidates.extend(over_cap.into_iter().map(|(id, digest)| (id, digest, false)));
        }

        if candidates.is_empty() {
            report.bytes_remaining = total;
            report.cap_unreachable = total > policy.max_image_bytes;
            if pending.is_empty() {
                tx.rollback().map_err(store_err("cannot end retention"))?;
            } else {
                tx.commit().map_err(store_err("cannot commit retention"))?;
                clear_index_markers(&self.layout, pending);
            }
            return Ok(report);
        }

        let mut written_markers = Vec::new();
        let mut durable_update_started = false;
        let retained = (|| -> Result<()> {
            for (id, digest, age_forced) in candidates {
                if !age_forced && total <= policy.max_image_bytes {
                    break;
                }
                let capture = CaptureId(id.clone());
                let Some(mut record) = self.layout.read_record(&capture)? else {
                    return Err(Error::Storage(format!(
                        "cannot evict capture {id}: its authoritative record is missing"
                    )));
                };
                if record.image_hash.as_deref() != Some(digest.as_str())
                    || record.image_evicted_at.is_some()
                {
                    upsert_record(&tx, &record)?;
                    continue;
                }
                if record.pinned || record.screen_pin.is_some() {
                    if record.screen_pin.is_some() && !record.pinned {
                        record.pinned = true;
                        let marker = self.layout.begin_index_update(&capture)?;
                        durable_update_started = true;
                        self.layout.write_record(&record)?;
                        written_markers.push(marker);
                    }
                    upsert_record(&tx, &record)?;
                    continue;
                }

                let marker = self.layout.begin_index_update(&capture)?;
                durable_update_started = true;
                record.mark_evicted(now);
                self.layout.write_record(&record)?;
                upsert_record(&tx, &record)?;
                written_markers.push(marker);

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

                report.evicted.push(capture);
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
            Ok(())
        })();
        if let Err(error) = retained {
            drop(tx);
            if durable_update_started {
                return Err(self.recover_retention_failure(error));
            }
            return Err(error);
        }

        if let Err(error) = tx.commit().map_err(store_err("cannot commit retention")) {
            if durable_update_started {
                return Err(self.recover_retention_failure(error));
            }
            return Err(error);
        }
        clear_index_markers(&self.layout, pending);
        for marker in written_markers {
            finish_committed_index_marker(&self.layout, Some(&marker));
        }

        Ok(report)
    }
}

fn eviction_candidates<P>(
    conn: &Connection,
    extra_predicate: &str,
    params: P,
    context: &'static str,
) -> Result<Vec<(String, String)>>
where
    P: rusqlite::Params,
{
    let sql = format!(
        "SELECT id, image_hash FROM captures
         WHERE image_hash IS NOT NULL AND image_evicted_at IS NULL AND pinned = 0
           AND {extra_predicate}
         ORDER BY created_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql).map_err(store_err(context))?;
    let rows = stmt
        .query_map(params, |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(store_err(context))?;
    rows.collect::<std::result::Result<_, _>>()
        .map_err(store_err(context))
}

#[allow(clippy::too_many_arguments)]
fn evict_capture(
    conn: &Connection,
    layout: &StoreLayout,
    id: &str,
    digest: &str,
    now: Timestamp,
    total: &mut u64,
    report: &mut RetentionReport,
    rewritten: &mut Vec<CaptureId>,
) -> Result<()> {
    // The document is untouched. Only the pixels go. This one statement is the
    // whole of decision D23.
    conn.execute(
        "UPDATE captures SET image_evicted_at = ?2, image_bytes = 0 WHERE id = ?1",
        params![id, now.0],
    )
    .map_err(store_err("cannot evict image"))?;

    if !blob_still_referenced(conn, digest)? {
        let byte_len: u64 = conn
            .query_row(
                "SELECT byte_len FROM blobs WHERE hash = ?1",
                params![digest],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .map_err(store_err("cannot size blob"))?
            .map_or(0, |n| u64::try_from(n).unwrap_or(0));

        conn.execute("DELETE FROM blobs WHERE hash = ?1", params![digest])
            .map_err(store_err("cannot forget blob"))?;
        // Unlinked inside the transaction on purpose: a concurrent insert that
        // would dedupe onto this blob is blocked by the same write lock, so it
        // cannot observe the file mid-removal.
        layout.delete_blob(digest)?;
        *total = total.saturating_sub(byte_len);
        report.bytes_reclaimed += byte_len;
    }

    let id = CaptureId(id.to_owned());
    rewritten.push(id.clone());
    report.evicted.push(id);
    Ok(())
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
    let video_json = record
        .video
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| Error::Storage(format!("cannot serialise video metadata: {e}")))?;
    let pin_json = record
        .screen_pin
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| Error::Storage(format!("cannot serialise pin state: {e}")))?;

    conn.execute(
        "INSERT INTO captures (
            id, created_at, stored_at, media_kind, pinned, app_name, app_identifier,
            window_title, window_shadow, provenance, target_json, frame_json, video_json,
            image_hash, image_bytes, image_evicted_at, ocr_text, annotation_count,
            search_fold, app_fold, title_fold, ocr_fold
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
            ?17, ?18, ?19, ?20, ?21, ?22
         )
         ON CONFLICT (id) DO UPDATE SET
             created_at = excluded.created_at,
             stored_at = excluded.stored_at,
             media_kind = excluded.media_kind,
             pinned = excluded.pinned,
             app_name = excluded.app_name,
             app_identifier = excluded.app_identifier,
             window_title = excluded.window_title,
             window_shadow = excluded.window_shadow,
             provenance = excluded.provenance,
             target_json = excluded.target_json,
             frame_json = excluded.frame_json,
             video_json = excluded.video_json,
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
            record.media_kind.as_token(),
            i64::from(record.pinned),
            record.app_name,
            record.app_identifier,
            record.window_title,
            record.window_shadow.map(i64::from),
            record.provenance.as_token(),
            target_json,
            frame_json,
            video_json,
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
    cache_screen_pin(conn, &CaptureId(record.id.clone()), pin_json.as_deref())?;
    sync_share_row(conn, &CaptureId(record.id.clone()), record.sharing.as_ref())?;
    Ok(())
}

fn cache_screen_pin(conn: &Connection, id: &CaptureId, pin_json: Option<&str>) -> Result<()> {
    if let Some(pin_json) = pin_json {
        conn.execute(
            "INSERT INTO capture_pins (capture_id, pin_json) VALUES (?1, ?2)
             ON CONFLICT (capture_id) DO UPDATE SET pin_json = excluded.pin_json",
            params![id.0, pin_json],
        )
        .map_err(store_err("cannot cache screen pin"))?;
    } else {
        conn.execute(
            "DELETE FROM capture_pins WHERE capture_id = ?1",
            params![id.0],
        )
        .map_err(store_err("cannot clear screen pin cache"))?;
    }
    Ok(())
}

fn synchronize_pin_records(conn: &Connection, records: &[StoredRecord]) -> Result<bool> {
    for record in records {
        if !synchronize_pin_record(conn, record)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn synchronize_pin_record(conn: &Connection, record: &StoredRecord) -> Result<bool> {
    let cached = conn
        .query_row(
            "SELECT pinned,
                    (SELECT pin_json FROM capture_pins WHERE capture_id = captures.id)
             FROM captures WHERE id = ?1",
            params![record.id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(store_err("cannot read cached pin state"))?;
    let Some((cached_pinned, cached_pin_json)) = cached else {
        return Ok(false);
    };

    if cached_pinned != i64::from(record.pinned) {
        conn.execute(
            "UPDATE captures SET pinned = ?2 WHERE id = ?1",
            params![record.id, i64::from(record.pinned)],
        )
        .map_err(store_err("cannot synchronize pinned state"))?;
    }

    let pin_json = serialize_screen_pin(record.screen_pin.as_ref())?;
    if cached_pin_json.as_deref() != pin_json.as_deref() {
        cache_screen_pin(conn, &CaptureId(record.id.clone()), pin_json.as_deref())?;
    }
    Ok(true)
}

fn repair_pending_index_updates(
    conn: &Connection,
    layout: &StoreLayout,
    updates: &[PendingIndexUpdate],
) -> Result<bool> {
    let mut captures = Vec::with_capacity(updates.len());
    for update in updates {
        let Some(capture) = &update.capture else {
            return Ok(false);
        };
        if !captures.contains(capture) {
            captures.push(capture.clone());
        }
    }

    for capture in captures {
        let Some(record) = layout.read_record(&capture)? else {
            return Ok(false);
        };
        synchronize_record_cache(conn, layout, &record)?;
    }
    Ok(true)
}

fn synchronize_record_cache(
    conn: &Connection,
    layout: &StoreLayout,
    record: &StoredRecord,
) -> Result<()> {
    upsert_record(conn, record)?;
    let Some(hash) = record.image_hash.as_deref() else {
        return Ok(());
    };

    if record.image_evicted_at.is_none() {
        if let Some(byte_len) = layout.blob_len(hash)? {
            conn.execute(
                "INSERT INTO blobs (hash, byte_len, created_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (hash) DO UPDATE SET byte_len = excluded.byte_len",
                params![
                    hash,
                    i64::try_from(byte_len).unwrap_or(i64::MAX),
                    record.stored_at
                ],
            )
            .map_err(store_err("cannot repair blob cache"))?;
        }
    } else if !blob_still_referenced(conn, hash)? {
        conn.execute("DELETE FROM blobs WHERE hash = ?1", params![hash])
            .map_err(store_err("cannot repair evicted blob cache"))?;
    }
    Ok(())
}

fn pin_cache_bootstrapped(conn: &Connection) -> Result<bool> {
    conn.query_row(
        "SELECT 1 FROM store_meta WHERE key = ?1",
        params![PIN_CACHE_BOOTSTRAP_KEY],
        |_| Ok(()),
    )
    .optional()
    .map(|value| value.is_some())
    .map_err(store_err("cannot read pin cache bootstrap state"))
}

fn mark_pin_cache_bootstrapped(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO store_meta (key, value) VALUES (?1, '1')
         ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        params![PIN_CACHE_BOOTSTRAP_KEY],
    )
    .map_err(store_err("cannot record pin cache bootstrap"))?;
    Ok(())
}

fn serialize_screen_pin(state: Option<&PinState>) -> Result<Option<String>> {
    state
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| Error::Storage(format!("cannot serialise pin state: {error}")))
}

fn clear_index_markers(layout: &StoreLayout, markers: Vec<PendingIndexUpdate>) {
    for marker in markers {
        if let Err(error) = layout.finish_index_update(&marker.path) {
            tracing::warn!(
                marker = %marker.path.display(),
                %error,
                "could not clear a repaired index recovery marker"
            );
        }
    }
}

fn finish_unused_index_marker(layout: &StoreLayout, marker: Option<&Path>) {
    let Some(marker) = marker else {
        return;
    };
    if let Err(cleanup) = layout.finish_index_update(marker) {
        tracing::warn!(
            marker = %marker.display(),
            %cleanup,
            "could not clear an unused index recovery marker"
        );
    }
}

fn finish_committed_index_marker(layout: &StoreLayout, marker: Option<&Path>) {
    let Some(marker) = marker else {
        return;
    };
    if let Err(error) = layout.finish_index_update(marker) {
        tracing::warn!(
            marker = %marker.display(),
            %error,
            "capture update committed but its recovery marker remains"
        );
    }
}

fn sync_share_row(
    conn: &Connection,
    id: &CaptureId,
    sharing: Option<&CaptureSharing>,
) -> Result<usize> {
    match sharing {
        Some(sharing) => {
            sharing.validate_for_storage()?;
            let sharing_json = serde_json::to_string(sharing)
                .map_err(|e| Error::Storage(format!("cannot serialise sharing metadata: {e}")))?;
            conn.execute(
                "INSERT INTO capture_shares (capture_id, sharing_json) VALUES (?1, ?2)
                 ON CONFLICT (capture_id) DO UPDATE SET sharing_json = excluded.sharing_json",
                params![id.0, sharing_json],
            )
            .map_err(store_err("cannot write sharing metadata"))
        }
        None => conn
            .execute(
                "DELETE FROM capture_shares WHERE capture_id = ?1",
                params![id.0],
            )
            .map_err(store_err("cannot clear sharing metadata")),
    }
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
    let mut sql = format!("SELECT {RECORD_COLUMNS} FROM {RECORD_SOURCE} WHERE 1 = 1");
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    // Every filtered column lives on `captures` alone, so the same unqualified
    // predicates are valid against the joined source here and the plain table
    // `build_count` uses.
    push_search_filters(query, &mut sql, &mut args);

    sql.push_str(" ORDER BY captures.created_at DESC, captures.id DESC");
    args.push(Box::new(i64::from(query.page.limit)));
    sql.push_str(&format!(" LIMIT ?{}", args.len()));
    args.push(Box::new(i64::from(query.page.offset)));
    sql.push_str(&format!(" OFFSET ?{}", args.len()));

    (sql, args)
}

fn build_count(query: &SearchQuery) -> (String, Vec<Box<dyn ToSql>>) {
    let mut sql = "SELECT COUNT(*) FROM captures WHERE 1 = 1".to_owned();
    let mut args: Vec<Box<dyn ToSql>> = Vec::new();
    push_search_filters(query, &mut sql, &mut args);
    (sql, args)
}

fn push_search_filters(query: &SearchQuery, sql: &mut String, args: &mut Vec<Box<dyn ToSql>>) {
    let like = |sql: &mut String, column: &str, needle: &str, args: &mut Vec<Box<dyn ToSql>>| {
        args.push(Box::new(like_pattern(needle)));
        sql.push_str(&format!(" AND {column} LIKE ?{} ESCAPE '\\'", args.len()));
    };

    if let Some(text) = &query.text {
        like(sql, "search_fold", text, args);
    }
    if let Some(app) = &query.app_name {
        like(sql, "app_fold", app, args);
    }
    if let Some(title) = &query.window_title {
        like(sql, "title_fold", title, args);
    }
    if let Some(ocr) = &query.ocr_text {
        like(sql, "ocr_fold", ocr, args);
    }
    if let Some(after) = query.created_after {
        args.push(Box::new(after.0));
        sql.push_str(&format!(" AND created_at >= ?{}", args.len()));
    }
    if let Some(before) = query.created_before {
        args.push(Box::new(before.0));
        sql.push_str(&format!(" AND created_at <= ?{}", args.len()));
    }
    if let Some(kind) = query.media_kind {
        args.push(Box::new(kind.as_token().to_owned()));
        sql.push_str(&format!(" AND media_kind = ?{}", args.len()));
    }
    if query.pinned_only {
        sql.push_str(" AND pinned = 1");
    }
    if query.images_only {
        sql.push_str(" AND image_hash IS NOT NULL AND image_evicted_at IS NULL");
    }
}

/// Reads one row into a record. The inner `Result` carries decoding failures,
/// which are a storage problem rather than a SQLite one.
fn row_to_record(row: &Row<'_>) -> Result<CaptureRecord> {
    let id: String = get(row, 0)?;
    let created_at: i64 = get(row, 1)?;
    let media_kind: String = get(row, 2)?;
    let pinned: i64 = get(row, 3)?;
    let app_name: Option<String> = get(row, 4)?;
    let app_identifier: Option<String> = get(row, 5)?;
    let window_title: Option<String> = get(row, 6)?;
    let window_shadow: Option<i64> = get(row, 7)?;
    let provenance: String = get(row, 8)?;
    let target_json: String = get(row, 9)?;
    let frame_json: String = get(row, 10)?;
    let video_json: Option<String> = get(row, 11)?;
    let image_hash: Option<String> = get(row, 12)?;
    let image_bytes: i64 = get(row, 13)?;
    let image_evicted_at: Option<i64> = get(row, 14)?;
    let ocr_text: Option<String> = get(row, 15)?;
    let annotation_count: i64 = get(row, 16)?;
    let pin_json: Option<String> = get(row, 17)?;
    let sharing_json: Option<String> = get(row, 18)?;

    let target: TargetRepr = serde_json::from_str(&target_json)
        .map_err(|e| Error::Storage(format!("cannot read target for {id}: {e}")))?;
    let frame: Option<FrameHeader> = serde_json::from_str(&frame_json)
        .map_err(|e| Error::Storage(format!("cannot read frame for {id}: {e}")))?;
    let video: Option<crate::model::VideoMetadata> =
        video_json.and_then(|json| match serde_json::from_str(&json) {
            Ok(metadata) => Some(metadata),
            Err(error) => {
                tracing::debug!(
                    capture = %id,
                    %error,
                    "video metadata is newer or malformed; keeping the history row"
                );
                None
            }
        });
    let screen_pin = pin_json
        .map(|json| {
            serde_json::from_str(&json)
                .map_err(|e| Error::Storage(format!("cannot read screen pin for {id}: {e}")))
        })
        .transpose()?;

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
        id: CaptureId(id.clone()),
        created_at: Timestamp(created_at),
        media_kind: MediaKind::from_token(&media_kind).map_err(|_| {
            Error::Storage(format!(
                "cannot read media kind {media_kind:?} from history"
            ))
        })?,
        pinned: pinned != 0,
        screen_pin,
        app_name,
        app_identifier,
        window_title,
        window_shadow: window_shadow.map(|shadow| shadow != 0),
        provenance: ProvenanceRepr::from_token(&provenance)?.into(),
        target: target.into(),
        frame,
        video,
        image,
        ocr_text,
        annotation_count: usize::try_from(annotation_count).unwrap_or(0),
        sharing: sharing_json
            .map(|json| {
                serde_json::from_str(&json).map_err(|e| {
                    Error::Storage(format!("cannot read sharing metadata for {id}: {e}"))
                })
            })
            .transpose()?,
    })
}

fn get<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> Result<T> {
    row.get(index)
        .map_err(|e| Error::Storage(format!("cannot read column {index}: {e}")))
}
