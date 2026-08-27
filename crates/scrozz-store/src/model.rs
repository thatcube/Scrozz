//! The types history is expressed in.
//!
//! # Why the pixels are not in here
//!
//! Decision D23 evicts source images while keeping annotation documents
//! forever, which means "a capture" and "a capture's pixels" have genuinely
//! different lifetimes. Every type below reflects that: a [`CaptureRecord`]
//! always exists, and [`ImageState`] says whether its pixels still do. Reading
//! a record can never fail because the image went away, because the record was
//! never holding the image.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use scrozz_core::{
    CaptureTarget, ColorSpace, DisplayId, Error, LogicalRect, PhysicalSize, PixelFormat,
    Provenance, Result, ScaleFactor, SourceApp, WindowId,
};
use serde::{Deserialize, Serialize};

use crate::CaptureId;

/// A point in time, as milliseconds since the Unix epoch.
///
/// Deliberately a plain integer rather than `SystemTime`: history is sorted,
/// range-queried and stored in SQLite, and all three want a number. Conversion
/// to and from `SystemTime` is explicit, and clamps rather than panics, because
/// a clock that has been set to 1904 must not take the history index with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The current instant.
    #[must_use]
    pub fn now() -> Self {
        Self::from_system_time(SystemTime::now())
    }

    /// Converts from a system clock reading.
    #[must_use]
    pub fn from_system_time(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(delta) => Self(i64::try_from(delta.as_millis()).unwrap_or(i64::MAX)),
            Err(err) => {
                Self(i64::try_from(err.duration().as_millis()).map_or(i64::MIN, |millis| -millis))
            }
        }
    }

    /// Converts back to a system clock reading.
    #[must_use]
    pub fn to_system_time(self) -> SystemTime {
        if self.0 >= 0 {
            UNIX_EPOCH + Duration::from_millis(self.0.unsigned_abs())
        } else {
            UNIX_EPOCH - Duration::from_millis(self.0.unsigned_abs())
        }
    }

    /// Milliseconds since the epoch.
    #[must_use]
    pub const fn as_millis(self) -> i64 {
        self.0
    }
}

/// Everything about a frame except its bytes.
///
/// Survives eviction. A capture whose pixels are gone still knows how big it
/// was, what colour space it was in and what scale it was captured at, which is
/// what history needs to render a placeholder at the right aspect ratio and
/// what a re-capture needs to reproduce it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FrameHeader {
    /// Dimensions in real pixels.
    pub size: PhysicalSize,
    /// Bytes per row in the stored blob.
    pub stride: usize,
    /// Sample layout.
    pub format: PixelFormat,
    /// Colour interpretation.
    pub color_space: ColorSpace,
    /// Scale of the display it came from.
    pub scale: ScaleFactor,
}

impl FrameHeader {
    /// Reads the header off a live frame.
    #[must_use]
    pub fn of(frame: &scrozz_core::Frame) -> Self {
        Self {
            size: frame.size,
            stride: frame.stride,
            format: frame.format,
            color_space: frame.color_space,
            scale: frame.scale,
        }
    }

    /// Bytes a well-formed blob for this header occupies.
    #[must_use]
    pub fn expected_len(&self) -> usize {
        self.stride
            .saturating_mul(self.size.height.round().max(0.0) as usize)
    }
}

/// Whether a capture's source pixels are still on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageState {
    /// The pixels are present.
    Present {
        /// Content address of the blob.
        hash: String,
        /// Size of the blob on disk.
        byte_len: u64,
    },
    /// The pixels were evicted under the size cap, per decision D23.
    ///
    /// The capture itself is **not** deleted and its edits are intact. This is
    /// the whole point of the split: a year-old screenshot still reopens with
    /// every arrow where it was, it simply no longer has a picture underneath.
    Evicted {
        /// When the pixels went away.
        at: Timestamp,
        /// What the blob used to be addressed as, for diagnostics and for the
        /// case where the same image is captured again and dedupes back in.
        was_hash: String,
    },
    /// The capture was recorded without pixels — an imported document, or a
    /// record recovered from a sidecar whose blob never existed.
    Absent,
}

impl ImageState {
    /// Whether the pixels can be loaded right now.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present { .. })
    }

    /// Bytes this capture contributes to the retention cap.
    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        match self {
            Self::Present { byte_len, .. } => *byte_len,
            Self::Evicted { .. } | Self::Absent => 0,
        }
    }
}

/// What kind of media a capture holds.
///
/// History's headline filter is All / Screenshots / Videos / GIFs, and the
/// distinction cannot be derived from [`Provenance`]: a region capture and a
/// region *recording* are both `Provenance::Region`. So it is recorded at
/// insert time and carried in its own indexed column.
///
/// A scrolling capture is a screenshot. It is stitched rather than grabbed in
/// one shot, but the user looking for "that long page" is looking under
/// screenshots, and [`Provenance::Stitched`] already records how it was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A still image.
    #[default]
    Screenshot,
    /// A screen recording with a video codec.
    Video,
    /// An animated GIF.
    Gif,
}

impl MediaKind {
    /// Every kind, in the order the filter bar shows them.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[Self::Screenshot, Self::Video, Self::Gif]
    }

    /// The stable token stored in the index and printed by the CLI.
    ///
    /// Never rename one: it is a value in a user's database and in the output
    /// of a script somebody has already written.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Video => "video",
            Self::Gif => "gif",
        }
    }

    /// The plural label the filter bar shows.
    #[must_use]
    pub const fn plural(self) -> &'static str {
        match self {
            Self::Screenshot => "Screenshots",
            Self::Video => "Videos",
            Self::Gif => "GIFs",
        }
    }

    /// Reads a token back.
    ///
    /// Accepts the plural spellings too, because a person typing
    /// `--kind screenshots` after reading the filter bar is not making a
    /// mistake worth an error message.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] naming every valid token.
    pub fn from_token(token: &str) -> Result<Self> {
        match token.trim().to_lowercase().as_str() {
            "screenshot" | "screenshots" | "image" | "images" => Ok(Self::Screenshot),
            "video" | "videos" | "recording" | "recordings" => Ok(Self::Video),
            "gif" | "gifs" => Ok(Self::Gif),
            other => Err(Error::InvalidRequest(format!(
                "unknown capture kind {other:?}; known kinds: screenshot, video, gif"
            ))),
        }
    }

    /// Whether this kind plays rather than sits still.
    #[must_use]
    pub const fn is_motion(self) -> bool {
        matches!(self, Self::Video | Self::Gif)
    }
}

/// How long source images are kept, independent of the size cap.
///
/// Decision D23 caps images by *size* and keeps documents forever. This is the
/// second, optional axis: a user who wants a week of pixels and nothing older
/// can say so. It evicts images exactly as the size cap does — the document
/// survives, the capture stays in history, and a pinned capture is never
/// touched — so turning it on can never cost an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionWindow {
    /// Keep images until the size cap says otherwise. The default.
    #[default]
    Forever,
    /// Keep one day of images.
    OneDay,
    /// Keep three days of images.
    ThreeDays,
    /// Keep one week of images.
    OneWeek,
    /// Keep one month of images, counted as 30 days.
    OneMonth,
}

impl RetentionWindow {
    /// Every window, longest-lived first, as the settings control lists them.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::Forever,
            Self::OneMonth,
            Self::OneWeek,
            Self::ThreeDays,
            Self::OneDay,
        ]
    }

    /// The stable token stored in settings and accepted by the CLI.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Forever => "forever",
            Self::OneDay => "1-day",
            Self::ThreeDays => "3-days",
            Self::OneWeek => "1-week",
            Self::OneMonth => "1-month",
        }
    }

    /// The label a settings control shows.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Forever => "Forever",
            Self::OneDay => "1 day",
            Self::ThreeDays => "3 days",
            Self::OneWeek => "1 week",
            Self::OneMonth => "1 month",
        }
    }

    /// Reads a token back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] naming every valid token.
    pub fn from_token(token: &str) -> Result<Self> {
        let folded = token.trim().to_lowercase().replace([' ', '_'], "-");
        match folded.as_str() {
            "forever" | "never" | "off" | "unlimited" => Ok(Self::Forever),
            "1-day" | "1d" | "day" | "24h" => Ok(Self::OneDay),
            "3-days" | "3d" => Ok(Self::ThreeDays),
            "1-week" | "1w" | "week" | "7-days" | "7d" => Ok(Self::OneWeek),
            "1-month" | "1m" | "month" | "30-days" | "30d" => Ok(Self::OneMonth),
            other => Err(Error::InvalidRequest(format!(
                "unknown retention window {other:?}; known windows: {}",
                Self::all()
                    .iter()
                    .map(|w| w.as_token())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// How long images live under this window, or `None` for forever.
    #[must_use]
    pub const fn max_age(self) -> Option<Duration> {
        const DAY: u64 = 60 * 60 * 24;
        match self {
            Self::Forever => None,
            Self::OneDay => Some(Duration::from_secs(DAY)),
            Self::ThreeDays => Some(Duration::from_secs(3 * DAY)),
            Self::OneWeek => Some(Duration::from_secs(7 * DAY)),
            Self::OneMonth => Some(Duration::from_secs(30 * DAY)),
        }
    }

    /// The instant before which images are stale, given `now`.
    ///
    /// Saturating rather than wrapping: a clock set to 1970 must produce "keep
    /// everything", not "evict everything".
    #[must_use]
    pub fn cutoff(self, now: Timestamp) -> Option<Timestamp> {
        let age = self.max_age()?;
        let millis = i64::try_from(age.as_millis()).unwrap_or(i64::MAX);
        Some(Timestamp(now.0.saturating_sub(millis)))
    }
}

/// One capture in history, without its pixels.
///
/// Deliberately not serialisable: this is a read model assembled from the
/// index, and the durable format is [`crate::record::StoredRecord`]. Two
/// serialisable shapes for the same thing is how they drift apart.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureRecord {
    /// Identity.
    pub id: CaptureId,
    /// When the capture was taken.
    pub created_at: Timestamp,
    /// Still, video or GIF.
    pub media_kind: MediaKind,
    /// Exempt from eviction. Decision D23: pinned captures are never evicted.
    pub pinned: bool,
    /// Application and window metadata captured with the pixels.
    pub source_app: SourceApp,
    /// Whether the captured window pixels include their native shadow.
    pub window_shadow: Option<bool>,
    /// How the capture was produced.
    pub provenance: Provenance,
    /// What it was aimed at.
    pub target: CaptureTarget,
    /// Frame geometry, which outlives the frame.
    pub frame: FrameHeader,
    /// Whether the pixels are still here.
    pub image: ImageState,
    /// Recognised text, if OCR has run.
    pub ocr_text: Option<String>,
    /// How many annotations the document holds.
    pub annotation_count: usize,
}

/// A page of results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Page {
    /// Maximum rows to return.
    pub limit: u32,
    /// Rows to skip.
    pub offset: u32,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
        }
    }
}

impl Page {
    /// A page of `limit` rows starting at `offset`.
    #[must_use]
    pub const fn new(limit: u32, offset: u32) -> Self {
        Self { limit, offset }
    }

    /// The page after this one.
    #[must_use]
    pub const fn next(self) -> Self {
        Self {
            limit: self.limit,
            offset: self.offset.saturating_add(self.limit),
        }
    }
}

/// Filters over history.
///
/// Every field is optional and they combine with AND. An empty query matches
/// everything, which makes this the single code path behind both "list history"
/// and "find that screenshot of the invoice".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SearchQuery {
    /// Matches the application name, the window title or the OCR text.
    pub text: Option<String>,
    /// Matches the application name alone.
    pub app_name: Option<String>,
    /// Matches the window title alone.
    pub window_title: Option<String>,
    /// Matches recognised text alone.
    pub ocr_text: Option<String>,
    /// Only captures taken at or after this instant.
    pub created_after: Option<Timestamp>,
    /// Only captures taken at or before this instant.
    pub created_before: Option<Timestamp>,
    /// Restricts results to one media kind.
    pub media_kind: Option<MediaKind>,
    /// Only pinned captures.
    pub pinned_only: bool,
    /// Exclude captures whose pixels have been evicted.
    ///
    /// Defaults to `false`, because D23's promise is that an evicted capture
    /// **stays in history**. Hiding them by default would reproduce the very
    /// behaviour the decision rejects.
    pub images_only: bool,
    /// Which slice of the results to return.
    pub page: Page,
}

impl SearchQuery {
    /// A query matching everything.
    #[must_use]
    pub fn all() -> Self {
        Self::default()
    }

    /// Free-text search across application, title and recognised text.
    #[must_use]
    pub fn text(mut self, needle: impl Into<String>) -> Self {
        self.text = Some(needle.into());
        self
    }

    /// Restricts to one application.
    #[must_use]
    pub fn app(mut self, app: impl Into<String>) -> Self {
        self.app_name = Some(app.into());
        self
    }

    /// Restricts to window titles containing `title`.
    #[must_use]
    pub fn titled(mut self, title: impl Into<String>) -> Self {
        self.window_title = Some(title.into());
        self
    }

    /// Restricts to recognised text containing `needle`.
    #[must_use]
    pub fn ocr(mut self, needle: impl Into<String>) -> Self {
        self.ocr_text = Some(needle.into());
        self
    }

    /// Only captures taken at or after `at`.
    #[must_use]
    pub const fn after(mut self, at: Timestamp) -> Self {
        self.created_after = Some(at);
        self
    }

    /// Only captures taken at or before `at`.
    #[must_use]
    pub const fn before(mut self, at: Timestamp) -> Self {
        self.created_before = Some(at);
        self
    }

    /// Restricts to a time range, either end of which may be open.
    #[must_use]
    pub const fn between(mut self, after: Option<Timestamp>, before: Option<Timestamp>) -> Self {
        self.created_after = after;
        self.created_before = before;
        self
    }

    /// Restricts results to one media kind.
    #[must_use]
    pub const fn kind(mut self, kind: MediaKind) -> Self {
        self.media_kind = Some(kind);
        self
    }

    /// Restricts to pinned captures.
    #[must_use]
    pub const fn pinned_only(mut self) -> Self {
        self.pinned_only = true;
        self
    }

    /// Hides captures whose pixels have been evicted.
    ///
    /// Opt-in, and it should stay that way: a gallery that needs a thumbnail has
    /// a reason to ask for this, but the history list does not, because decision
    /// D23 keeps an image-evicted capture in history with its edits intact.
    #[must_use]
    pub const fn images_only(mut self) -> Self {
        self.images_only = true;
        self
    }

    /// Sets the page.
    #[must_use]
    pub const fn paged(mut self, page: Page) -> Self {
        self.page = page;
        self
    }
}

/// What eviction actually did.
///
/// Returned rather than logged because the CLI reports it and the settings
/// screen shows it, and because "the cap could not be met" is a real outcome
/// that must not be silent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionReport {
    /// Captures whose pixels were dropped, oldest first.
    pub evicted: Vec<CaptureId>,
    /// Bytes reclaimed from disk.
    pub bytes_reclaimed: u64,
    /// Bytes of source imagery still stored afterwards.
    pub bytes_remaining: u64,
    /// Bytes held by pinned captures, which are never evictable.
    pub pinned_bytes: u64,
    /// The cap could not be met without evicting pinned captures, so it was not
    /// met. Decision D23 is unambiguous that pinned wins over the cap.
    pub cap_unreachable: bool,
}

impl RetentionReport {
    /// Whether anything was dropped.
    #[must_use]
    pub fn evicted_anything(&self) -> bool {
        !self.evicted.is_empty()
    }
}

/// Serialisable stand-in for [`Provenance`], which is a plain core enum.
///
/// Part of the durable record format, so its variant names are a compatibility
/// surface: renaming one makes old history unreadable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum ProvenanceRepr {
    Display,
    Window,
    Region,
    AllDisplays,
    Stitched,
}

impl From<Provenance> for ProvenanceRepr {
    fn from(value: Provenance) -> Self {
        match value {
            Provenance::Display => Self::Display,
            Provenance::Window => Self::Window,
            Provenance::Region => Self::Region,
            Provenance::AllDisplays => Self::AllDisplays,
            Provenance::Stitched => Self::Stitched,
        }
    }
}

impl From<ProvenanceRepr> for Provenance {
    fn from(value: ProvenanceRepr) -> Self {
        match value {
            ProvenanceRepr::Display => Self::Display,
            ProvenanceRepr::Window => Self::Window,
            ProvenanceRepr::Region => Self::Region,
            ProvenanceRepr::AllDisplays => Self::AllDisplays,
            ProvenanceRepr::Stitched => Self::Stitched,
        }
    }
}

impl ProvenanceRepr {
    /// Short stable token stored in its own indexed column.
    #[must_use]
    pub const fn as_token(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::Window => "window",
            Self::Region => "region",
            Self::AllDisplays => "all_displays",
            Self::Stitched => "stitched",
        }
    }

    /// Reads a token back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the token is not one this build knows.
    pub fn from_token(token: &str) -> Result<Self> {
        match token {
            "display" => Ok(Self::Display),
            "window" => Ok(Self::Window),
            "region" => Ok(Self::Region),
            "all_displays" => Ok(Self::AllDisplays),
            "stitched" => Ok(Self::Stitched),
            other => Err(Error::Storage(format!("unknown provenance {other:?}"))),
        }
    }
}

/// Serialisable stand-in for [`CaptureTarget`].
///
/// Part of the durable record format; see [`ProvenanceRepr`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(missing_docs)]
pub enum TargetRepr {
    Display { id: DisplayId },
    Window { id: WindowId },
    Region { bounds: LogicalRect },
    AllDisplays,
}

impl From<&CaptureTarget> for TargetRepr {
    fn from(value: &CaptureTarget) -> Self {
        match value {
            CaptureTarget::Display(id) => Self::Display { id: id.clone() },
            CaptureTarget::Window(id) => Self::Window { id: id.clone() },
            CaptureTarget::Region(bounds) => Self::Region { bounds: *bounds },
            CaptureTarget::AllDisplays => Self::AllDisplays,
        }
    }
}

impl From<TargetRepr> for CaptureTarget {
    fn from(value: TargetRepr) -> Self {
        match value {
            TargetRepr::Display { id } => Self::Display(id),
            TargetRepr::Window { id } => Self::Window(id),
            TargetRepr::Region { bounds } => Self::Region(bounds),
            TargetRepr::AllDisplays => Self::AllDisplays,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_round_trip_through_system_time() {
        let now = Timestamp::now();
        let round_tripped = Timestamp::from_system_time(now.to_system_time());
        assert_eq!(now, round_tripped);
    }

    #[test]
    fn timestamps_before_the_epoch_do_not_panic() {
        let ancient = Timestamp(-2_208_988_800_000);
        assert_eq!(
            Timestamp::from_system_time(ancient.to_system_time()),
            ancient
        );
    }

    #[test]
    fn provenance_tokens_round_trip() {
        for provenance in [
            Provenance::Display,
            Provenance::Window,
            Provenance::Region,
            Provenance::AllDisplays,
            Provenance::Stitched,
        ] {
            let repr = ProvenanceRepr::from(provenance);
            let token = repr.as_token();
            let back = ProvenanceRepr::from_token(token).expect("token must parse");
            assert_eq!(Provenance::from(back), provenance);
        }
    }

    #[test]
    fn unknown_provenance_token_is_a_storage_error_not_a_panic() {
        assert!(ProvenanceRepr::from_token("teleported").is_err());
    }

    #[test]
    fn targets_round_trip_through_their_serialisable_form() {
        let targets = [
            CaptureTarget::Display(DisplayId("built-in".into())),
            CaptureTarget::Window(WindowId("42".into())),
            CaptureTarget::Region(LogicalRect::new(
                scrozz_core::LogicalPoint::new(1.0, 2.0),
                scrozz_core::LogicalSize::new(3.0, 4.0),
            )),
            CaptureTarget::AllDisplays,
        ];
        for target in targets {
            let json = serde_json::to_string(&TargetRepr::from(&target)).expect("serialise");
            let back: TargetRepr = serde_json::from_str(&json).expect("deserialise");
            assert_eq!(CaptureTarget::from(back), target);
        }
    }

    #[test]
    fn evicted_images_contribute_nothing_to_the_cap() {
        let present = ImageState::Present {
            hash: "a".repeat(64),
            byte_len: 1024,
        };
        let evicted = ImageState::Evicted {
            at: Timestamp(0),
            was_hash: "a".repeat(64),
        };
        assert_eq!(present.byte_len(), 1024);
        assert_eq!(evicted.byte_len(), 0);
        assert!(present.is_present());
        assert!(!evicted.is_present());
    }

    #[test]
    fn pages_advance_without_overflowing() {
        let page = Page::new(10, u32::MAX - 5);
        assert_eq!(page.next().offset, u32::MAX);
    }
}
