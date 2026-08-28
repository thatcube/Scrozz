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
use std::sync::atomic::{AtomicU64, Ordering};
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
                    self.remove();
                    true
                } else {
                    false
                }
            }
            ArtifactState::InFlight => false,
        }
    }

    /// Deletes the artifact and its directory now, whatever state it was in.
    pub fn remove(&mut self) {
        if self.state == ArtifactState::Removed {
            return;
        }
        if let Err(err) = fs::remove_dir_all(&self.dir)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            // Not fatal, and not worth failing a drag over: the orphan sweep on
            // the next launch is the backstop for exactly this.
            tracing::debug!(
                dir = %self.dir.display(),
                "could not remove a drag artifact: {err}"
            );
        }
        self.state = ArtifactState::Removed;
        self.expires = None;
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
}

impl ScratchFile {
    /// Takes responsibility for `to` before anything is written there.
    ///
    /// The seam that makes the ordering testable, and the reason a copy that
    /// dies halfway does not leak: from here on the path belongs to the guard
    /// whether or not any bytes ever arrive.
    ///
    /// Refuses if something is already at `to` rather than guarding a file it
    /// did not create — the paths used here are unique per process and serial,
    /// so a collision means an assumption is wrong, and deleting a stranger's
    /// file on the way out would be the worse failure. That refusal also keeps
    /// the ordering honest: claiming *after* writing would find the path
    /// occupied by its own output and fail.
    pub fn claim(to: PathBuf) -> Result<Self> {
        if to.exists() {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("scratch path is already occupied: {}", to.display()),
            )));
        }
        Ok(Self { path: Some(to) })
    }

    /// Claims `to`, then copies `from` into it.
    ///
    /// The guard is armed *before* the copy is attempted, so a copy that fails
    /// halfway still takes its partial output with it.
    pub fn copy(from: &Path, to: PathBuf) -> Result<Self> {
        let guard = Self::claim(to)?;
        fs::copy(from, guard.path())?;
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
        self.path.take().expect("a live guard owns its path")
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
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
        if !path.is_dir() {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= max_age);
        if old && fs::remove_dir_all(&path).is_ok() {
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
}
