//! The coordinator's recording state, and the video editor it can open.
//!
//! # Why this is a separate module
//!
//! [`crate::gui::App`] already owns the capture pipeline, the card surface, the
//! selector, the permission flow and the pin lifecycle. Recording adds a state
//! machine, a selector round trip, a finalisation thread, a preview worker, a
//! storyboard worker and a transcode job — six more things with lifetimes of
//! their own. Kept inline they would be indistinguishable from the capture
//! state they sit beside; kept here, "what does recording own" has one answer.
//!
//! # The seam
//!
//! Nothing in this module knows what a card looks like. It produces exactly one
//! artifact for the aggregate capture stack — a validated
//! [`FinalizedMediaHandoff`] — and the coordinator takes it through
//! [`App::take_finalized_media_handoff`](crate::gui::App::take_finalized_media_handoff).
//! That deliberate narrowness is what stops the old recording UI ever growing
//! back: there is no path from here to a card except a durable file, a poster,
//! and a duration.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{Receiver, Sender},
    },
    time::{Duration, Instant},
};

use scrozz_core::{CaptureTarget, Error as CoreError};
use scrozz_record::{
    MachineFailure, Recording, RecordingMachine, RecordingPhase, RecordingRequest,
    RecordingSettings,
    edit::{EditPlan, VideoDocument},
    handoff::FinalizedMediaHandoff,
    media::DecodedVideoFrame,
    playback::NativePlayback,
    storyboard::NativeStoryboard,
    transcode::{
        NativeTranscoder, TranscodeEvent, TranscodeFailure, TranscodeJob, TranscodeOutput,
        TranscodeStatus, Transcoder as _,
    },
};
use scrozz_ui::video_editor::{VideoEditorAction, VideoEditorSnapshot};

use crate::{
    cli::RecordArgs,
    fault::{CliError, CliResult},
    gui::server::Request,
    report::Report,
};

/// One finished recording, and the aggregate handoff built from it off-thread.
///
/// Both halves travel together because the poster decode must not happen on the
/// UI thread and must not happen twice.
pub struct FinalisedRecording {
    /// What the native finaliser returned.
    pub result: scrozz_core::Result<Recording>,
    /// The validated durable handoff, when the policy asked for a card.
    pub handoff: scrozz_core::Result<Option<FinalizedMediaHandoff>>,
}

/// The After Capture cells that apply to a finished recording.
///
/// Snapshotted when the recording starts, so a settings change made *during* a
/// recording applies to the next one rather than retroactively changing what
/// the user asked for when they pressed record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CompletionActions {
    /// Put the finished video into the Recent Captures Overlay.
    pub recent_captures_overlay: bool,
    /// Open the video editor over the finished video.
    pub open_editor: bool,
}

impl CompletionActions {
    /// Reads both cells out of the settings the machine is running with.
    #[must_use]
    pub const fn from_settings(settings: &RecordingSettings) -> Self {
        Self {
            recent_captures_overlay: settings.after_capture.recent_captures_overlay,
            open_editor: settings.after_capture.open_editor,
        }
    }
}

/// How a completed recording answered whoever was waiting for it.
#[derive(Clone)]
pub enum Completion {
    /// The recording finished and produced a report.
    Finished(Box<Report>),
    /// The recording failed, or was cancelled.
    Failed(String),
}

/// Everything a finished editor export needs to reach history and a card.
///
/// Produced by [`RecordingState::advance_export`] on the tick the export
/// settles, so the poster is taken from workers that are still alive and no
/// media is decoded a second time on the UI thread.
pub struct CompletedExport {
    /// The source document the export was made from.
    pub document: VideoDocument,
    /// The plan that decided container, codec, cadence and dimensions.
    pub plan: EditPlan,
    /// The completed artifact.
    pub output: TranscodeOutput,
    /// An already-decoded source frame for the aggregate card's poster.
    pub poster: Option<DecodedVideoFrame>,
}

/// A live video editor: a document, a preview worker, and any export in flight.
pub struct ActiveVideoEditor {
    /// The non-destructive source document.
    pub document: VideoDocument,
    /// The current edit plan.
    pub plan: EditPlan,
    /// Native preview playback, which owns the media clock.
    pub playback: NativePlayback,
    /// Incrementally decoded filmstrip and waveform.
    pub storyboard: NativeStoryboard,
    /// The export in flight, if any.
    pub transcode_job: Option<Box<dyn TranscodeJob>>,
    /// Last observed export status.
    pub transcode_status: Option<TranscodeStatus>,
    /// Last observed export progress.
    pub transcode_progress: f32,
    /// The complete exported artifact.
    pub transcode_output: Option<TranscodeOutput>,
    /// An explicit export failure, with any retained partial.
    pub transcode_failure: Option<TranscodeFailure>,
}

impl ActiveVideoEditor {
    /// Opens a playable recording, or reports that there is nothing to open.
    ///
    /// # Errors
    ///
    /// Returns whatever the media layer says about a source it cannot decode.
    /// Returns `Ok(None)` for output that is real but not playable — an
    /// initialisation-only partial has a file and no frames, and opening an
    /// editor over it would show a black rectangle rather than a failure.
    pub fn open(output: &Recording) -> scrozz_core::Result<Option<Self>> {
        output.require_native()?;
        if !output.is_playable() {
            return Ok(None);
        }
        let document = VideoDocument::open_native(output.clone())?;
        let plan = EditPlan::video(&document)?;
        let playback = NativePlayback::open(&document, plan)?;
        let storyboard = NativeStoryboard::start(&document)?;
        Ok(Some(Self {
            document,
            plan,
            playback,
            storyboard,
            transcode_job: None,
            transcode_status: None,
            transcode_progress: 0.0,
            transcode_output: None,
            transcode_failure: None,
        }))
    }

    /// The state one editor viewport pass needs.
    #[must_use]
    pub fn snapshot(&self) -> VideoEditorSnapshot {
        VideoEditorSnapshot {
            document: self.document.clone(),
            plan: self.plan,
            playback: self.playback.snapshot().clone(),
            storyboard: self.storyboard.snapshot(),
            transcode_status: self.transcode_status,
            transcode_progress: self.transcode_progress,
            transcode_output: self.transcode_output.clone(),
            transcode_failure: self.transcode_failure.clone(),
        }
    }

    /// Whether an export is running right now.
    #[must_use]
    pub const fn is_exporting(&self) -> bool {
        self.transcode_job.is_some()
    }
}

/// What a pending start will hand the machine once overlays are down.
pub enum PendingStart {
    /// A GUI recording using ambient settings and a chosen target.
    Settings {
        /// Resolved capture target.
        target: CaptureTarget,
        /// Durable destination reserved before the recording began.
        destination: PathBuf,
    },
    /// A fully prepared command-line request.
    Request(Box<RecordingRequest>),
}

/// A start held back for exactly one tick.
///
/// The selector and the card surface are still on screen in the frame that
/// chose a target. Starting the encoder in that frame records the overlay.
pub struct ArmedStart {
    /// What to start.
    pub start: PendingStart,
    /// The tick on which it was armed; it starts on a later one.
    pub armed_tick: u64,
}

/// What a selection round trip is choosing a target for.
pub enum SelectionStart {
    /// A GUI recording, which needs a destination reserved up front.
    Settings {
        /// Durable destination.
        destination: PathBuf,
    },
    /// A forwarded `record --interactive`, which carries its own arguments.
    Request(Box<RecordArgs>),
}

/// A selector running on its own thread, and what to do with its answer.
pub struct PendingSelection {
    /// What the target is for.
    pub start: SelectionStart,
    /// The selector's answer.
    pub result: Receiver<CliResult<CaptureTarget>>,
    /// Whether a stop or toggle asked for cancellation while it was open.
    pub cancel_requested: bool,
}

/// Everything the coordinator knows about recording.
pub struct RecordingState {
    /// The state machine, absent when no engine advertises video.
    pub machine: Option<RecordingMachine>,
    /// Why there is no machine, so the refusal can name the real reason.
    pub unavailable: Option<String>,
    /// Wall clock feeding the machine's virtual clock.
    pub tick: Instant,
    /// The finalisation thread's result channel.
    pub finalisation: Option<Receiver<FinalisedRecording>>,
    /// The last terminal outcome, used to answer forwarded waiters.
    pub completion: Option<Completion>,
    /// Forwarded requests waiting for a terminal outcome.
    pub replies: Vec<Request>,
    /// A selector round trip in progress.
    pub selection: Option<PendingSelection>,
    /// A start held back until the overlays are down.
    pub pending_start: Option<ArmedStart>,
    /// The open video editor.
    pub editor: Option<ActiveVideoEditor>,
    /// The completed video waiting to enter the aggregate capture stack.
    pub handoff: Option<FinalizedMediaHandoff>,
    /// Monotonic tick counter that the arming barrier compares against.
    pub sequence: u64,
    /// The last failure presented outside the machine's own lifecycle.
    pub preflight_failure: Option<MachineFailure>,
}

impl RecordingState {
    /// Builds the state, detecting the native engine once.
    ///
    /// A platform with no recording engine is not an error: the app runs, the
    /// menu item is disabled, and the reason is available to say out loud.
    #[must_use]
    pub fn new(settings: RecordingSettings) -> Self {
        let (machine, unavailable) = match RecordingMachine::native(settings) {
            Ok(machine) => (Some(machine), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            machine,
            unavailable,
            tick: Instant::now(),
            finalisation: None,
            completion: None,
            replies: Vec::new(),
            selection: None,
            pending_start: None,
            editor: None,
            handoff: None,
            sequence: 0,
            preflight_failure: None,
        }
    }

    /// The current lifecycle phase, or `None` when recording is unavailable.
    #[must_use]
    pub fn phase(&self) -> Option<RecordingPhase> {
        self.machine.as_ref().map(RecordingMachine::phase)
    }

    /// Whether a recording is actually capturing right now.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.machine
            .as_ref()
            .is_some_and(RecordingMachine::is_active)
    }

    /// Whether anything at all is in flight, including selection and arming.
    #[must_use]
    pub fn is_busy(&self) -> bool {
        self.selection.is_some()
            || self.pending_start.is_some()
            || self.finalisation.is_some()
            || self.is_active()
    }

    /// Whether the video editor is open.
    #[must_use]
    pub const fn editor_is_open(&self) -> bool {
        self.editor.is_some()
    }

    /// The error to report when there is no engine to record with.
    #[must_use]
    pub fn unavailable_error(&self) -> CoreError {
        CoreError::Unsupported {
            what: "screen recording".to_owned(),
            why: self.unavailable.clone().unwrap_or_else(|| {
                "no native recording engine is linked for this platform".to_owned()
            }),
        }
    }

    /// Applies the ambient After Capture policy to the machine.
    ///
    /// Called immediately before each recording starts so the two cells are
    /// read at the last honest moment, and each is carried independently.
    ///
    /// # Errors
    ///
    /// Returns whatever the machine says when it will not accept a change —
    /// which it refuses mid-recording, by design.
    pub fn apply_after_capture(
        &mut self,
        policy: scrozz_record::AfterCaptureSettings,
    ) -> scrozz_core::Result<()> {
        let Some(machine) = self.machine.as_mut() else {
            return Err(self.unavailable_error());
        };
        machine.set_after_capture_settings(policy)
    }

    /// The After Capture cells the current recording is running with.
    #[must_use]
    pub fn completion_actions(&self) -> CompletionActions {
        self.machine
            .as_ref()
            .map(|machine| CompletionActions::from_settings(machine.settings()))
            .unwrap_or_default()
    }

    /// Advances the export job, returning the export if it settled completely.
    ///
    /// Only a `Finished` export is returned: a cancelled or failed one has
    /// nothing to put in history, and its retained partial is already on the
    /// editor for the user to reveal.
    pub fn advance_export(&mut self) -> Option<CompletedExport> {
        let editor = self.editor.as_mut()?;
        editor.transcode_job.as_ref()?;
        let mut terminal = false;
        let mut finished = None;
        // Bounded: a job that produces events faster than they are consumed
        // must not be able to hold the UI thread for a whole frame.
        for _ in 0..MAX_EXPORT_EVENTS_PER_TICK {
            let Some(event) = editor
                .transcode_job
                .as_mut()
                .expect("the export job was checked above")
                .poll()
            else {
                break;
            };
            match event {
                TranscodeEvent::Progress(progress) => {
                    editor.transcode_progress = progress;
                    editor.transcode_status = Some(TranscodeStatus::Running { progress });
                }
                TranscodeEvent::Finished(output) => {
                    finished = Some(CompletedExport {
                        document: editor.document.clone(),
                        plan: editor.plan,
                        output: output.clone(),
                        poster: export_poster(editor),
                    });
                    editor.transcode_progress = 1.0;
                    editor.transcode_output = Some(output);
                    editor.transcode_failure = None;
                    editor.transcode_status = Some(TranscodeStatus::Finished);
                    terminal = true;
                    break;
                }
                TranscodeEvent::Failed(failure) => {
                    editor.transcode_output = None;
                    editor.transcode_failure = Some(failure);
                    editor.transcode_status = Some(TranscodeStatus::Failed);
                    terminal = true;
                    break;
                }
                TranscodeEvent::Cancelled(partial) => {
                    editor.transcode_output = partial;
                    editor.transcode_failure = None;
                    editor.transcode_status = Some(TranscodeStatus::Cancelled);
                    terminal = true;
                    break;
                }
            }
        }
        if terminal {
            editor.transcode_job = None;
        } else {
            editor.transcode_status = editor.transcode_job.as_ref().map(|job| job.status());
        }
        finished
    }

    /// Tears the editor down, returning any shutdown complaints to be noted.
    pub fn release_editor(&mut self) -> Vec<String> {
        let Some(mut editor) = self.editor.take() else {
            return Vec::new();
        };
        let mut problems = Vec::new();
        if let Err(error) = editor.playback.shutdown() {
            problems.push(format!("recording preview shutdown failed: {error}"));
        }
        if let Err(error) = editor.storyboard.shutdown() {
            problems.push(format!("recording timeline shutdown failed: {error}"));
        }
        problems
    }

    /// Answers every forwarded waiter once a terminal outcome exists.
    pub fn reply_waiters(&mut self) {
        if self.replies.is_empty() {
            return;
        }
        let Some(completion) = self.completion.clone() else {
            return;
        };
        for request in self.replies.drain(..) {
            let result = match &completion {
                Completion::Finished(report) => Ok((**report).clone()),
                Completion::Failed(message) => {
                    Err(CliError::Core(CoreError::Platform(message.clone())))
                }
            };
            request.answer(&result);
        }
    }

    /// Records a terminal failure and answers anyone waiting on it.
    pub fn fail(&mut self, error: &CliError) {
        self.completion = Some(Completion::Failed(error.to_string()));
        self.reply_waiters();
    }
}

/// The most export events one tick will drain before yielding the frame.
const MAX_EXPORT_EVENTS_PER_TICK: u8 = 64;

/// A source frame to use as the finished export's poster.
///
/// Preferred from the storyboard, which has already decoded the trimmed range
/// off-thread; the live preview frame is the fallback. Returning `None` is a
/// real answer: the export still succeeded, it simply has no card poster.
fn export_poster(editor: &ActiveVideoEditor) -> Option<DecodedVideoFrame> {
    let storyboard = editor.storyboard.snapshot();
    storyboard
        .frames
        .iter()
        .flatten()
        .filter(|slot| {
            slot.frame.timestamp >= editor.plan.trim.start
                && slot.frame.timestamp < editor.plan.trim.end
        })
        .min_by_key(|slot| slot.frame.timestamp)
        .map(|slot| slot.frame.as_ref().clone())
        .or_else(|| {
            editor
                .playback
                .snapshot()
                .frame
                .as_ref()
                .map(|preview| preview.frame.as_ref().clone())
        })
}

/// Runs the blocking native finalisation on its own thread.
///
/// The poster decode happens here too, and only when the overlay actually asked
/// for a card: a recording nobody will show has no reason to pay for a decode.
pub fn spawn_finalisation(
    session: Box<dyn scrozz_record::RecordingSession>,
    actions: CompletionActions,
    send: Sender<FinalisedRecording>,
) {
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.stop()))
            .unwrap_or_else(|_| {
                Err(CoreError::Platform(
                    "the native recording finaliser panicked".to_owned(),
                ))
            });
        let handoff = match result.as_ref() {
            Ok(output) if actions.recent_captures_overlay && !output.is_partial() => {
                FinalizedMediaHandoff::from_completed(output).map(Some)
            }
            Ok(_) | Err(_) => Ok(None),
        };
        let _ = send.send(FinalisedRecording { result, handoff });
    });
}

/// Whether an editor action is safe while an export is running.
#[must_use]
pub const fn action_allowed_during_export(action: &VideoEditorAction) -> bool {
    matches!(
        action,
        VideoEditorAction::CancelExport
            | VideoEditorAction::RevealOutput
            | VideoEditorAction::RevealPartialOutput
    )
}

/// A collision-free destination for an edited export beside its source.
///
/// # Errors
///
/// Returns [`CoreError::Storage`] when no free name exists.
pub fn edited_output_path(
    document: &VideoDocument,
    plan: &EditPlan,
) -> scrozz_core::Result<PathBuf> {
    let source = document.recording().path();
    let parent = source.parent().unwrap_or_else(|| Path::new("."));
    let stem = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("recording");
    let extension = plan.output.extension();
    for suffix in 0..MAX_EDITED_NAME_ATTEMPTS {
        let name = if suffix == 0 {
            format!("{stem}-edited.{extension}")
        } else {
            format!("{stem}-edited-{suffix}.{extension}")
        };
        let candidate = parent.join(name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(CoreError::Storage(
        "could not allocate a collision-free edited recording path".to_owned(),
    ))
}

const MAX_EDITED_NAME_ATTEMPTS: u32 = 1_000;

/// Starts an export of `plan` from `document`.
///
/// # Errors
///
/// Returns whatever the native transcoder says about a plan it cannot run.
pub fn start_export(
    document: &VideoDocument,
    plan: &EditPlan,
) -> scrozz_core::Result<Box<dyn TranscodeJob>> {
    let output = edited_output_path(document, plan)?;
    NativeTranscoder::new().start(document, plan, output)
}

/// Shows `path` in the platform file browser.
///
/// # Errors
///
/// Returns [`CoreError::Platform`] when the browser could not be launched or
/// exited unsuccessfully.
pub fn reveal_file(path: &Path) -> scrozz_core::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg("-R").arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("explorer");
        command.arg("/select,").arg(path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path.parent().unwrap_or(path));
        command
    };

    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(CoreError::Platform(format!(
            "the platform file browser exited with {status}"
        )))
    }
}

/// Opens `path` with the platform's registered viewer for its type.
///
/// The counterpart to [`reveal_file`]: a GIF export has no video editor to
/// open, so its card opens the artifact itself rather than pretending the
/// editor can decode it.
///
/// # Errors
///
/// Returns [`CoreError::Platform`] when the viewer could not be launched or
/// exited unsuccessfully.
pub fn open_file(path: &Path) -> scrozz_core::Result<()> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = std::process::Command::new("cmd");
        command.arg("/C").arg("start").arg("").arg(path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };

    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(CoreError::Platform(format!(
            "the platform viewer exited with {status}"
        )))
    }
}

/// A recoverable failure presented as if the machine had produced it.
#[must_use]
pub fn preflight_failure(error: &CliError) -> MachineFailure {
    MachineFailure {
        error: Arc::new(match error {
            CliError::Core(core) => core.clone(),
            other => CoreError::Platform(other.to_string()),
        }),
        partial: None,
        recovery_error: None,
    }
}

/// How long the app waits for a finaliser during shutdown before giving up.
pub const SHUTDOWN_FINALISE_TIMEOUT: Duration = Duration::from_secs(20);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_actions_are_two_independent_cells() {
        let mut settings = RecordingSettings::shipped();
        assert_eq!(
            CompletionActions::from_settings(&settings),
            CompletionActions {
                recent_captures_overlay: true,
                open_editor: false,
            },
            "the shipped default shows a card and does not open an editor"
        );

        settings.after_capture.open_editor = true;
        let both = CompletionActions::from_settings(&settings);
        assert!(both.recent_captures_overlay && both.open_editor);

        settings.after_capture.recent_captures_overlay = false;
        let editor_only = CompletionActions::from_settings(&settings);
        assert!(!editor_only.recent_captures_overlay && editor_only.open_editor);

        settings.after_capture.open_editor = false;
        assert_eq!(
            CompletionActions::from_settings(&settings),
            CompletionActions::default(),
            "both cells off is a legal state, not a fallback to defaults"
        );
    }

    #[test]
    fn only_settling_actions_survive_an_active_export() {
        for action in [
            VideoEditorAction::CancelExport,
            VideoEditorAction::RevealOutput,
            VideoEditorAction::RevealPartialOutput,
        ] {
            assert!(action_allowed_during_export(&action), "{action:?}");
        }
        for action in [
            VideoEditorAction::Play,
            VideoEditorAction::Pause,
            VideoEditorAction::Seek(Duration::from_secs(1)),
            VideoEditorAction::SetRate(2.0),
            VideoEditorAction::Close,
        ] {
            assert!(!action_allowed_during_export(&action), "{action:?}");
        }
    }

    #[test]
    fn edited_output_paths_match_format_and_never_replace_a_collision() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("scrozz-output-path-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&root).expect("scratch directory");
        let document = VideoDocument::open_fixture(
            Recording::synthetic(root.join("capture.mp4"), 2.0, "output path fixture")
                .expect("synthetic source"),
            scrozz_record::edit::SourceMetadata {
                width: 64,
                height: 48,
                fps: 10.0,
                audio_channels: 0,
            },
        )
        .expect("open fixture");

        // Each destination names itself: the extension comes from the plan's
        // container, never from the source file it was edited from.
        let gif = EditPlan::gif(&document).expect("gif plan");
        let first = edited_output_path(&document, &gif).expect("first gif path");
        assert_eq!(first.file_name().expect("name"), "capture-edited.gif");
        std::fs::write(&first, b"existing").expect("occupy the first name");
        assert_eq!(
            edited_output_path(&document, &gif)
                .expect("second gif path")
                .file_name()
                .expect("name"),
            "capture-edited-1.gif",
            "an existing export is never silently overwritten"
        );

        let webm = EditPlan::webm(&document).expect("webm plan");
        assert_eq!(
            edited_output_path(&document, &webm)
                .expect("webm path")
                .file_name()
                .expect("name"),
            "capture-edited.webm"
        );

        let video = EditPlan::video(&document).expect("video plan");
        assert_eq!(
            edited_output_path(&document, &video)
                .expect("mp4 path")
                .file_name()
                .expect("name"),
            "capture-edited.mp4"
        );

        std::fs::remove_dir_all(root).expect("clean up scratch directory");
    }
}
