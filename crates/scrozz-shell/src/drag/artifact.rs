//! The file a drag hands over, and exactly how long it is allowed to live.
//!
//! # Why there is a file at all
//!
//! The honest answer is that the promise mechanisms do not work where it
//! matters. `NSFilePromiseProvider` is the *correct* macOS API and the one
//! Apple documents, and Finder, Mail and TextEdit all honour it. Chromium does
//! not: every Electron app — Slack, Discord, VS Code, Notion — and every
//! browser drop zone reads `public.file-url` and ignores a promise, so a
//! promise-only drag lands nowhere in precisely the applications D12 names as
//! the point of the feature. A drag that visibly refuses in Slack is not a
//! drag.
//!
//! So a real file is written, once, at the moment the drag begins — not at
//! capture time, not speculatively, and never for a card nobody dragged. The
//! bytes still arrive through a [`ByteSource`], so nothing is encoded until a
//! drag actually starts.
//!
//! [`ByteSource`]: super::ByteSource
//!
//! # Why the file cannot simply be deleted at the drop
//!
//! The receiver reads asynchronously. Slack's uploader, Finder's copy engine
//! and a browser's `File` object all open the path *after* the drop completes
//! and after AppKit has already told the source the drag ended. Deleting on
//! `draggingSession:endedAtPoint:operation:` is the classic way to produce a
//! drag that "works" and delivers a zero-byte file.
//!
//! Hence a two-state life after the drop, which is what this module exists to
//! make testable without a mouse:
//!
//! - the drop was **refused, cancelled or failed** — nobody has the path, so
//!   the file goes immediately;
//! - the drop was **accepted** — the file is retained for [`RETENTION`] and
//!   then swept.
//!
//! Anything that outlives the process is caught by [`sweep_orphans`] on the
//! next launch, so a crash mid-drag costs one file until then rather than
//! forever.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use scrozz_core::{Error, Result};

use super::DragOutcome;

/// The directory, under the system temp directory, that holds every artifact.
///
/// Namespaced by bundle identifier so a sweep can never reach anything that is
/// not Scrozz's, and so a user who looks in `/tmp` can tell what put it there.
pub const ARTIFACT_DIR: &str = "com.thatcube.scrozz-drag";

/// How long an accepted artifact outlives its drop.
///
/// Long enough for a slow upload to open the file it was handed, short enough
/// that a session's worth of drags is not still on disk at lunchtime. Ninety
/// seconds is the number CleanShot-class tools settle on for the same reason.
pub const RETENTION: Duration = Duration::from_secs(90);

/// How old an artifact from a previous run must be before it is swept.
///
/// A drag cannot last an hour, so anything older than this belonged to a
/// process that is gone. Deliberately generous: deleting a file another
/// running instance is mid-drag on would be worse than leaving one behind.
pub const ORPHAN_MAX_AGE: Duration = Duration::from_secs(60 * 60);

/// Where an artifact is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArtifactState {
    /// The drag is happening. The file must exist.
    InFlight,
    /// The drop was accepted. The file is held until the receiver has had time
    /// to read it.
    Retained,
    /// The file is gone.
    Removed,
}

impl ArtifactState {
    /// Whether the file should be on disk in this state.
    #[must_use]
    pub const fn expects_file(self) -> bool {
        matches!(self, Self::InFlight | Self::Retained)
    }
}

/// The directory every artifact of this machine is written under.
#[must_use]
pub fn artifact_root() -> PathBuf {
    std::env::temp_dir().join(ARTIFACT_DIR)
}

/// What an outcome means for a file the receiver may or may not have read.
///
/// Pure, so the rule that "accepted means wait, everything else means delete
/// now" is a test rather than a comment. The first outcome decides: AppKit can
/// report an end and a failure for the same drag, and a late report must not
/// resurrect a deleted file or re-delete one a receiver is reading.
#[must_use]
pub fn state_after(current: ArtifactState, outcome: &DragOutcome) -> ArtifactState {
    match current {
        ArtifactState::Retained | ArtifactState::Removed => current,
        ArtifactState::InFlight => {
            if outcome.is_accepted() {
                ArtifactState::Retained
            } else {
                ArtifactState::Removed
            }
        }
    }
}

/// A temporary file handed to a drop target, and its remaining lifetime.
///
/// Each artifact owns a private directory so the file inside can keep the name
/// the user should see — two drags of `Screenshot.png` do not collide, and
/// nothing has to append `-1` to a filename the receiver will show.
#[derive(Debug)]
pub struct DragArtifact {
    dir: PathBuf,
    path: PathBuf,
    state: ArtifactState,
    expires: Option<Instant>,
}

impl DragArtifact {
    /// Writes `bytes` to a fresh private directory under `root`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Storage`] if the directory or the file could not be
    /// written, and [`Error::InvalidRequest`] for an empty file name. A drag
    /// that cannot produce its file must fail before AppKit is asked to start a
    /// session, not halfway through one.
    pub fn materialise(root: &Path, file_name: &str, bytes: &[u8]) -> Result<Self> {
        if file_name.is_empty() {
            return Err(Error::InvalidRequest(
                "a drag artifact needs a file name".to_owned(),
            ));
        }
        let dir = root.join(unique_token());
        fs::create_dir_all(&dir).map_err(|err| {
            Error::Storage(format!(
                "could not create the drag directory {}: {err}",
                dir.display()
            ))
        })?;
        let path = dir.join(file_name);
        if let Err(err) = fs::write(&path, bytes) {
            // Nothing else has seen this directory yet, so tidying up is free
            // and keeps a failed drag from leaving a husk behind.
            let _ = fs::remove_dir_all(&dir);
            return Err(Error::Storage(format!(
                "could not write the drag file {}: {err}",
                path.display()
            )));
        }
        Ok(Self {
            dir,
            path,
            state: ArtifactState::InFlight,
            expires: None,
        })
    }

    /// The file the receiver is being offered.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Where this artifact is in its life.
    #[must_use]
    pub const fn state(&self) -> ArtifactState {
        self.state
    }

    /// When a retained artifact becomes sweepable.
    #[must_use]
    pub const fn expires_at(&self) -> Option<Instant> {
        self.expires
    }

    /// Whether the file is still on disk.
    #[must_use]
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// A `file://` URL for the artifact.
    ///
    /// Used verbatim by X11's `text/uri-list`. macOS builds its own through
    /// `NSURL`, which is the only encoder AppKit fully agrees with, but this
    /// one is exercised by the tests and keeps the non-Apple backends honest.
    #[must_use]
    pub fn url(&self) -> String {
        format!("file://{}", percent_encode_path(&self.path))
    }

    /// Applies a drag outcome, deleting immediately unless the drop was taken.
    pub fn settle(&mut self, outcome: &DragOutcome) {
        self.settle_at(outcome, Instant::now());
    }

    /// [`Self::settle`] against a supplied clock, so tests need not sleep.
    pub fn settle_at(&mut self, outcome: &DragOutcome, now: Instant) {
        let next = state_after(self.state, outcome);
        if next == self.state {
            return;
        }
        match next {
            ArtifactState::Retained => {
                self.state = ArtifactState::Retained;
                self.expires = Some(now + RETENTION);
            }
            ArtifactState::Removed => self.remove(),
            ArtifactState::InFlight => unreachable!("nothing transitions back into flight"),
        }
    }

    /// Deletes a retained artifact whose grace period has passed.
    ///
    /// Returns `true` when the artifact is finished with, so a caller polling
    /// each frame knows when it can drop the session.
    pub fn sweep_at(&mut self, now: Instant) -> bool {
        match self.state {
            ArtifactState::Removed => true,
            ArtifactState::Retained => {
                if self.expires.is_some_and(|deadline| now >= deadline) {
                    self.remove_at(now);
                    self.state == ArtifactState::Removed
                } else {
                    false
                }
            }
            ArtifactState::InFlight => false,
        }
    }

    /// Deletes the artifact and its directory now, whatever state it was in.
    pub fn remove(&mut self) {
        self.remove_at(Instant::now());
    }

    fn remove_at(&mut self, retry_at: Instant) {
        if self.state == ArtifactState::Removed {
            return;
        }
        match fs::remove_dir_all(&self.dir) {
            Ok(()) => {
                self.state = ArtifactState::Removed;
                self.expires = None;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.state = ArtifactState::Removed;
                self.expires = None;
            }
            Err(err) => {
                // Keep it sweepable. Calling this "removed" would let the
                // session stop polling while sensitive image bytes remained.
                self.state = ArtifactState::Retained;
                self.expires = Some(retry_at);
                tracing::warn!(
                    dir = %self.dir.display(),
                    "could not remove a drag artifact; will retry: {err}"
                );
            }
        }
    }
}

impl Drop for DragArtifact {
    fn drop(&mut self) {
        // A retained artifact is deliberately left: the receiver may still be
        // reading it, and the orphan sweep will collect it. An in-flight one
        // being dropped means the drag never started, so it goes.
        if self.state == ArtifactState::InFlight {
            self.remove();
        }
    }
}

#[cfg(test)]
mod removal_tests {
    use super::*;

    #[test]
    fn a_failed_removal_stays_sweepable_until_the_path_is_gone() {
        let root = std::env::temp_dir().join(format!(
            "scrozz-artifact-removal-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let not_a_directory = root.join("file");
        fs::write(&not_a_directory, b"sensitive").unwrap();
        let now = Instant::now();
        let mut artifact = DragArtifact {
            dir: not_a_directory.clone(),
            path: not_a_directory.clone(),
            state: ArtifactState::InFlight,
            expires: None,
        };

        artifact.remove_at(now);
        assert_eq!(artifact.state(), ArtifactState::Retained);
        assert_eq!(artifact.expires_at(), Some(now));
        assert!(
            !artifact.sweep_at(now),
            "a second failed deletion must not report completion"
        );

        fs::remove_file(&not_a_directory).unwrap();
        assert!(artifact.sweep_at(now));
        assert_eq!(artifact.state(), ArtifactState::Removed);
        let _ = fs::remove_dir_all(root);
    }
}

/// A file that deletes itself unless it is explicitly handed on.
///
/// The narrow problem this solves lives in the Windows data object, where a
/// `TYMED_FILE` medium is duplicated by physically copying the file: the copy
/// belongs to nobody until a `STGMEDIUM` is successfully built around its path,
/// and every step between the two can fail. `std::fs::copy` can create the
/// destination and *then* fail partway, leaving a truncated file; the task
/// allocation for the path string can fail after a perfectly good copy. Neither
/// failure has anywhere to report the file it left behind, so without a guard
/// each one leaks a temporary file for the lifetime of the machine.
///
/// It lives here, in portable code, for two reasons: the logic is `std::fs` and
/// nothing else, and putting it here means its behaviour is exercised by tests
/// that run on every platform rather than only compiled on Windows.
#[derive(Debug)]
pub struct ScratchFile {
    /// `None` once ownership has been given away by [`Self::release`].
    path: Option<PathBuf>,
    /// The handle the path was created through, closed before any deletion.
    ///
    /// Held so the file is written through the same handle that reserved it,
    /// rather than by a second open that could land on a different file.
    handle: Option<fs::File>,
}

impl ScratchFile {
    /// Creates `to`, exclusively, and takes responsibility for it.
    ///
    /// The seam that makes the ordering testable, and the reason a copy that
    /// dies halfway does not leak: from here on the path belongs to the guard
    /// whether or not any bytes ever arrive.
    ///
    /// The reservation is the creation. Asking whether the path exists and then
    /// writing to it is two operations with a gap in between, and in that gap
    /// another process can take the name — after which this guard would either
    /// overwrite a file it did not create or delete one on the way out. There is
    /// no portable way to close that gap by checking harder, so the check is
    /// replaced by an exclusive create, which the filesystem resolves atomically
    /// and which fails outright if the name is taken.
    ///
    /// A taken name is refused rather than adopted. The paths used here are
    /// unique per process and serial, so a collision means an assumption is
    /// wrong, and deleting a stranger's file would be the worse failure. That
    /// refusal also keeps the ordering honest: claiming *after* writing would
    /// find the path occupied by its own output and fail.
    pub fn claim(to: PathBuf) -> Result<Self> {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        let handle = fs::File::create_new(&to)?;
        Ok(Self {
            path: Some(to),
            handle: Some(handle),
        })
    }

    /// Claims `to`, then copies `from` into it.
    ///
    /// The guard is armed *before* the copy is attempted, so a copy that fails
    /// halfway still takes its partial output with it. The bytes go through the
    /// handle the claim already holds, so nothing reopens the path and no second
    /// lookup can resolve it to something else.
    pub fn copy(from: &Path, to: PathBuf) -> Result<Self> {
        let mut guard = Self::claim(to)?;
        let mut source = fs::File::open(from)?;
        let into = guard
            .handle
            .as_mut()
            .expect("a freshly claimed guard holds its handle");
        io::copy(&mut source, into)?;
        into.sync_all()?;
        Ok(guard)
    }

    /// The guarded path.
    ///
    /// # Panics
    ///
    /// Never: the path is only taken by [`Self::release`], which consumes the
    /// guard.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.path.as_deref().expect("a live guard owns its path")
    }

    /// Gives the file up, so that dropping this guard no longer deletes it.
    ///
    /// Called once the receiving structure has taken responsibility for the
    /// file — and not a moment earlier, which is the whole point.
    #[must_use]
    pub fn release(mut self) -> PathBuf {
        // Closed here rather than left to the guard's own drop, so the file is
        // not still open by this process when the receiver goes to read it.
        self.handle = None;
        self.path.take().expect("a live guard owns its path")
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        // The handle goes first. On Windows an open handle is exactly what makes
        // a delete fail, and this process holding one would be a self-inflicted
        // sharing violation.
        self.handle = None;

        let Some(path) = self.path.take() else {
            return;
        };
        if fs::remove_file(&path).is_ok() {
            return;
        }
        // The delete can fail for a reason that is nobody's fault and will not
        // last: on Windows a receiver still holding the file open makes the
        // unlink fail until it lets go. Losing the path here would strand the
        // file forever, so the deletion is not the only chance to reclaim it —
        // scratch files live under the swept root, where `sweep_orphans` finds
        // them by age and tries again. The count is what makes that visible to
        // a test, and to anyone wondering whether it is happening.
        CLEANUP_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

/// How many scratch files could not be deleted by their guard.
///
/// Each one is left for [`sweep_orphans`] to reclaim on a later run. A number
/// that climbs during a session is not a leak on its own — a Windows receiver
/// holding a dragged file open is the ordinary cause — but a number that climbs
/// without the sweeper ever bringing it back down would be.
#[must_use]
pub fn scratch_cleanup_failures() -> usize {
    CLEANUP_FAILURES.load(Ordering::Relaxed)
}

/// Counts deletions the guard could not perform. See [`scratch_cleanup_failures`].
static CLEANUP_FAILURES: AtomicUsize = AtomicUsize::new(0);

/// A scratch path under the swept root, for a copy of `like`.
///
/// Under [`artifact_root`] rather than the bare temp directory, because that is
/// the one place [`sweep_orphans`] looks: a file left behind by a delete that
/// failed is only retryable if it is somewhere something will retry it. The
/// extension is carried over so a receiver that reads the copy sees the same
/// kind of file it was offered.
#[must_use]
pub fn scratch_path(like: &Path) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);

    let mut name = format!("scrozz-stg-{}-{serial}", std::process::id());
    if let Some(ext) = like.extension().and_then(|ext| ext.to_str()) {
        name.push('.');
        name.push_str(ext);
    }
    artifact_root().join(name)
}

/// Deletes artifacts left behind by a previous run.
///
/// Returns how many directories were removed. Errors are swallowed
/// deliberately: this runs at startup, and a temp directory that cannot be
/// read is not a reason to refuse to launch.
pub fn sweep_orphans(root: &Path, now: SystemTime, max_age: Duration) -> usize {
    let Ok(entries) = fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if !old {
            continue;
        }
        // Directories are drags; loose files are scratch copies whose guard
        // could not delete them — a Windows receiver still holding one open
        // makes the unlink fail, and this is the retry. Both are reclaimed by
        // their own age, so a live scratch file, seconds old, is never touched.
        let gone = if path.is_dir() {
            fs::remove_dir_all(&path).is_ok()
        } else {
            fs::remove_file(&path).is_ok()
        };
        if gone {
            removed += 1;
        }
    }
    removed
}

/// A directory name no other drag will pick.
///
/// Process id, wall clock and a counter: the first separates instances, the
/// second separates runs, the third separates drags within a run that landed
/// on the same nanosecond.
fn unique_token() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!("{:x}-{nanos:x}-{seq:x}", std::process::id())
}

/// Percent-encodes a path for use in a `file://` URL.
///
/// Everything outside RFC 3986's unreserved set is escaped, which is stricter
/// than required and therefore never wrong. Separators are preserved so the
/// result is still a path.
fn percent_encode_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let mut out = String::with_capacity(raw.len());
    for byte in raw.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(char::from(*byte));
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drag::DragOperation;

    /// A scratch root for one test.
    ///
    /// Unit tests do not get `CARGO_TARGET_TMPDIR` (only integration tests do),
    /// so this builds a uniquely named directory under the system temp dir and
    /// each test removes it.
    fn root() -> PathBuf {
        std::env::temp_dir()
            .join("scrozz-artifact-unit")
            .join(unique_token())
    }

    #[test]
    fn a_url_escapes_what_a_receiver_would_otherwise_misread() {
        let encoded = percent_encode_path(Path::new("/tmp/a b/Design #1 100%.png"));
        assert_eq!(encoded, "/tmp/a%20b/Design%20%231%20100%25.png");
    }

    #[test]
    fn an_accepted_drop_holds_the_file_and_a_refused_one_does_not() {
        assert_eq!(
            state_after(
                ArtifactState::InFlight,
                &DragOutcome::Accepted(DragOperation::Copy)
            ),
            ArtifactState::Retained
        );
        for outcome in [
            DragOutcome::Rejected,
            DragOutcome::Cancelled,
            DragOutcome::Failed("no".to_owned()),
        ] {
            assert_eq!(
                state_after(ArtifactState::InFlight, &outcome),
                ArtifactState::Removed,
                "{outcome:?}"
            );
        }
    }

    #[test]
    fn a_late_report_cannot_change_a_decided_artifact() {
        assert_eq!(
            state_after(ArtifactState::Retained, &DragOutcome::Cancelled),
            ArtifactState::Retained
        );
        assert_eq!(
            state_after(
                ArtifactState::Removed,
                &DragOutcome::Accepted(DragOperation::Copy)
            ),
            ArtifactState::Removed
        );
    }

    #[test]
    fn an_orphan_sweep_leaves_a_fresh_directory_alone() {
        let root = root().join("sweep");
        let _ = fs::remove_dir_all(&root);
        let artifact = DragArtifact::materialise(&root, "Fresh.png", b"bytes").unwrap();
        assert!(artifact.exists());

        assert_eq!(sweep_orphans(&root, SystemTime::now(), ORPHAN_MAX_AGE), 0);
        assert!(artifact.exists(), "a live drag was swept out from under it");

        // Anything older than the window is from a process that is gone.
        let swept = sweep_orphans(
            &root,
            SystemTime::now() + Duration::from_secs(2 * 60 * 60),
            ORPHAN_MAX_AGE,
        );
        assert_eq!(swept, 1);
        assert!(!artifact.exists());
    }

    #[test]
    fn a_nameless_artifact_is_refused_before_anything_is_written() {
        let err = DragArtifact::materialise(&root().join("nameless"), "", b"bytes")
            .expect_err("an empty file name must be refused");
        assert!(matches!(err, Error::InvalidRequest(_)), "{err}");
    }

    // -----------------------------------------------------------------------
    // A copied file belongs to nobody until somebody takes it
    // -----------------------------------------------------------------------

    /// A directory with one readable file in it, plus a free path beside it.
    fn scratch_pair() -> (PathBuf, PathBuf, PathBuf) {
        let dir = root();
        fs::create_dir_all(&dir).expect("a temp dir");
        let source = dir.join("source.png");
        fs::write(&source, b"pretend png").expect("a source file");
        let dest = dir.join("copy.png");
        (dir, source, dest)
    }

    #[test]
    fn a_guarded_copy_exists_while_the_guard_does() {
        let (dir, source, dest) = scratch_pair();

        let guard = ScratchFile::copy(&source, dest.clone()).expect("the copy succeeds");

        assert_eq!(guard.path(), dest);
        assert_eq!(fs::read(&dest).expect("readable"), b"pretend png");
        drop(guard);

        assert!(!dest.exists(), "dropping the guard takes the file with it");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_released_copy_outlives_its_guard() {
        // The success path: something else has taken responsibility, so the
        // guard must stop being one.
        let (dir, source, dest) = scratch_pair();
        let guard = ScratchFile::copy(&source, dest.clone()).expect("the copy succeeds");

        let kept = guard.release();

        assert_eq!(kept, dest);
        assert!(dest.exists(), "a released file is no longer the guard's");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_failure_after_the_copy_still_takes_the_file() {
        // This is the leak the guard exists for: in the Windows data object the
        // task allocation for the path string happens *after* the copy, and can
        // fail. Standing in for it here is any early return that drops the
        // guard without releasing it.
        let (dir, source, dest) = scratch_pair();

        let outcome: Result<PathBuf> = (|| {
            let guard = ScratchFile::copy(&source, dest.clone())?;
            assert!(guard.path().exists(), "the copy landed");
            Err(Error::Platform("allocating the path failed".into()))
        })();

        assert!(outcome.is_err());
        assert!(!dest.exists(), "the abandoned copy was cleaned up");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_partial_copy_is_still_the_guards_responsibility() {
        // The failure the ordering exists for: bytes land at the destination
        // and *then* the copy dies. Writing through the claimed path stands in
        // for a copy that got halfway, which is not something a test can ask
        // the filesystem to do on demand.
        let (dir, _source, dest) = scratch_pair();

        let guard = ScratchFile::claim(dest.clone()).expect("the path is free");
        fs::write(guard.path(), b"half a fi").expect("a partial write");
        drop(guard);

        assert!(!dest.exists(), "the half-written file went with the guard");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_copy_that_never_starts_leaves_nothing_behind() {
        // `fs::copy` can create the destination and then fail; guarding before
        // the attempt is what makes a partial file somebody's responsibility.
        let (dir, _source, dest) = scratch_pair();
        let missing = dir.join("not-there.png");

        let err = ScratchFile::copy(&missing, dest.clone()).expect_err("no such source");

        assert!(matches!(err, Error::Io(_)), "{err}");
        assert!(!dest.exists(), "nothing was left at the destination");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn an_occupied_path_is_refused_rather_than_adopted() {
        // Deleting a file this code did not create would be a worse failure
        // than refusing to copy.
        let (dir, source, dest) = scratch_pair();
        fs::write(&dest, b"somebody else's").expect("an occupant");

        let err = ScratchFile::copy(&source, dest.clone()).expect_err("occupied");

        assert!(matches!(err, Error::Io(_)), "{err}");
        assert_eq!(
            fs::read(&dest).expect("still there"),
            b"somebody else's",
            "the stranger's file survived untouched"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_claim_creates_the_file_it_reserves() {
        // The reservation and the creation are the same operation, which is
        // what makes it atomic: there is no window between deciding the name is
        // free and taking it.
        let (dir, _source, dest) = scratch_pair();

        let guard = ScratchFile::claim(dest.clone()).expect("the path is free");

        assert!(dest.exists(), "the claim itself put the file there");
        drop(guard);
        assert!(!dest.exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_second_claim_on_a_live_path_loses() {
        // Stands in for the race: two claimants, one name. The exclusive create
        // decides it in the filesystem, so exactly one wins however the two are
        // interleaved — and the loser gets an error rather than a guard over a
        // file somebody else is writing.
        let (dir, _source, dest) = scratch_pair();

        let first = ScratchFile::claim(dest.clone()).expect("the first claim wins");
        let second = ScratchFile::claim(dest.clone());

        assert!(second.is_err(), "the second claim was refused");
        assert!(dest.exists(), "and the refusal did not disturb the first");
        drop(first);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_refused_claim_does_not_delete_the_file_it_lost_to() {
        // The failure mode the refusal exists to prevent: adopting a stranger's
        // file and then deleting it on the way out.
        let (dir, _source, dest) = scratch_pair();
        fs::write(&dest, b"somebody else's work").expect("a stranger's file");

        drop(ScratchFile::claim(dest.clone()));

        assert_eq!(
            fs::read(&dest).expect("still there"),
            b"somebody else's work",
            "the refused claim left it alone"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_copy_goes_through_the_handle_that_reserved_the_path() {
        let (dir, source, dest) = scratch_pair();

        let guard = ScratchFile::copy(&source, dest.clone()).expect("the copy succeeds");

        assert_eq!(
            fs::read(guard.path()).expect("readable"),
            b"pretend png",
            "the bytes arrived through the claimed handle"
        );
        let kept = guard.release();
        assert_eq!(fs::read(&kept).expect("readable"), b"pretend png");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_claim_makes_the_directory_it_needs() {
        // A machine that has never run a drag has no artifact root, and the
        // first thing that wants one is a claim. Nothing else creates it, so a
        // claim that assumed the parent existed would fail on exactly the
        // machines that matter — a fresh CI runner, or a user's first drag.
        let dir = root();
        let missing = dir.join("never").join("existed");
        assert!(!missing.exists(), "the point of the test");

        let guard = ScratchFile::claim(missing.join("copy.png")).expect("a claim");

        assert!(guard.path().exists(), "the claim reserved a real file");

        drop(guard);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_scratch_path_lives_where_the_sweeper_will_look() {
        // The other half of the cleanup story: a delete that fails has to leave
        // the file somewhere something retries. That somewhere is the swept root.
        let path = scratch_path(Path::new("shot.png"));

        assert_eq!(
            path.parent(),
            Some(artifact_root().as_path()),
            "a scratch file outside the root would never be retried"
        );
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
    }

    #[test]
    fn two_scratch_paths_are_never_the_same() {
        let first = scratch_path(Path::new("a.png"));
        let second = scratch_path(Path::new("a.png"));

        assert_ne!(first, second);
    }

    #[test]
    fn the_sweeper_reclaims_a_stranded_scratch_file() {
        // What happens after a delete fails: the file is a loose, old entry
        // under the root, and the sweep is the retry.
        let dir = root();
        fs::create_dir_all(&dir).expect("a temp dir");
        let stranded = dir.join("scrozz-stg-1-0.png");
        fs::write(&stranded, b"could not be deleted").expect("a stranded file");

        let later = SystemTime::now() + Duration::from_secs(7200);
        let removed = sweep_orphans(&dir, later, Duration::from_secs(3600));

        assert_eq!(removed, 1);
        assert!(!stranded.exists(), "the retry got it");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_sweeper_leaves_a_live_scratch_file_alone() {
        // The risk of sweeping files as well as directories: a drag in progress
        // has a scratch file that is seconds old, and it must survive.
        let dir = root();
        fs::create_dir_all(&dir).expect("a temp dir");
        let live = dir.join("scrozz-stg-1-1.png");
        fs::write(&live, b"in use").expect("a live file");

        let removed = sweep_orphans(&dir, SystemTime::now(), Duration::from_secs(3600));

        assert_eq!(removed, 0);
        assert!(live.exists(), "a young file is not an orphan");
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_deletion_that_fails_is_counted_rather_than_forgotten() {
        // The seam. A guard whose delete fails must not swallow it, because the
        // file is then somebody else's problem — the sweeper's — and a count
        // that never moves is the difference between "this never happens" and
        // "this is happening silently". A read-only directory produces the
        // failure here; on Windows an open handle in the receiver does.
        use std::os::unix::fs::PermissionsExt;

        let (dir, _source, dest) = scratch_pair();
        let guard = ScratchFile::claim(dest.clone()).expect("the path is free");

        let before = scratch_cleanup_failures();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o500)).expect("sealed");
        drop(guard);
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("unsealed");

        assert_eq!(
            scratch_cleanup_failures(),
            before + 1,
            "the failed delete was recorded"
        );
        assert!(dest.exists(), "and the file is still there to be retried");

        let later = SystemTime::now() + Duration::from_secs(7200);
        assert_eq!(
            sweep_orphans(&dir, later, Duration::from_secs(3600)),
            2,
            "the sweeper reclaims it, and the source beside it"
        );
        let _ = fs::remove_dir_all(dir);
    }
}
