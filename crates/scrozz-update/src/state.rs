use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactMetadata, Error, Result, UpdateChannel,
    fsutil::{absolute, atomic_write, ensure_distinct_siblings, ensure_regular_file, parent},
};

const STATE_SCHEMA: u32 = 1;
const MAX_CHECK_ERROR_BYTES: usize = 8192;

/// Durable phases in one update lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    /// No operation or accepted candidate is active.
    Idle,
    /// Manifest and signature bytes are being fetched or verified.
    Checking,
    /// A signed, newer candidate is available for this platform.
    UpdateAvailable,
    /// The candidate artifact is being fetched.
    Downloading,
    /// The downloaded artifact has passed size and digest verification.
    Downloaded,
    /// The verified artifact has been copied and synced to its staging path.
    Staged,
    /// An explicit installation swap was durably requested but not committed.
    AwaitingRestart,
    /// The candidate is installed and the prior file remains available.
    Installed,
    /// An operation stopped without claiming success.
    Failed,
    /// The retained prior installation was restored.
    RolledBack,
}

impl Phase {
    /// Returns whether `next` is an explicitly modelled transition.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Idle, Self::Checking)
                | (
                    Self::Checking,
                    Self::Idle | Self::UpdateAvailable | Self::Failed
                )
                | (Self::UpdateAvailable, Self::Downloading | Self::Failed)
                | (Self::UpdateAvailable, Self::Idle)
                | (
                    Self::Downloading,
                    Self::Idle | Self::Downloaded | Self::Failed
                )
                | (Self::Downloaded, Self::Idle | Self::Staged | Self::Failed)
                | (
                    Self::Staged,
                    Self::Idle | Self::AwaitingRestart | Self::Failed
                )
                | (
                    Self::AwaitingRestart,
                    Self::Installed | Self::Failed | Self::RolledBack
                )
                | (Self::Installed, Self::RolledBack | Self::Idle)
                | (
                    Self::Failed,
                    Self::RolledBack | Self::Idle | Self::UpdateAvailable
                )
                | (Self::RolledBack, Self::Idle)
        )
    }
}

/// Version and generation of the accepted candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateMetadata {
    pub(crate) version: Version,
    pub(crate) generated: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) channel: Option<UpdateChannel>,
}

impl CandidateMetadata {
    pub(crate) fn new(version: Version, generated: u64, channel: Option<UpdateChannel>) -> Self {
        Self {
            version,
            generated,
            channel,
        }
    }

    /// Returns the candidate semantic version.
    #[must_use]
    pub fn version(&self) -> &Version {
        &self.version
    }

    /// Returns the candidate manifest generation.
    #[must_use]
    pub const fn generated(&self) -> u64 {
        self.generated
    }

    /// Returns the endpoint-catalog channel that produced this candidate.
    ///
    /// `None` identifies a candidate checked through the raw URL API.
    #[must_use]
    pub const fn channel(&self) -> Option<UpdateChannel> {
        self.channel
    }
}

/// Durable result category for the most recently completed signed check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateCheckResult {
    /// The installed and signed versions matched.
    Current,
    /// A newer signed release omitted this platform.
    PlatformUnavailable,
    /// A newer signed artifact was accepted for this platform.
    UpdateAvailable,
    /// Fetching or verification failed without accepting a candidate.
    Failed,
}

impl UpdateCheckResult {
    /// Returns the stable JSON and status token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::PlatformUnavailable => "platform-unavailable",
            Self::UpdateAvailable => "update-available",
            Self::Failed => "failed",
        }
    }
}

/// Durable summary of the most recently completed signed update check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateCheckRecord {
    channel: Option<UpdateChannel>,
    completed_at_unix_seconds: Option<u64>,
    result: UpdateCheckResult,
    version: Option<Version>,
    generation: Option<u64>,
    platform: Option<String>,
    error: Option<String>,
}

impl UpdateCheckRecord {
    pub(crate) fn success(
        channel: Option<UpdateChannel>,
        result: UpdateCheckResult,
        version: Version,
        generation: u64,
        platform: Option<String>,
    ) -> Self {
        Self {
            channel,
            completed_at_unix_seconds: unix_time_now(),
            result,
            version: Some(version),
            generation: Some(generation),
            platform,
            error: None,
        }
    }

    pub(crate) fn failed(channel: Option<UpdateChannel>, mut error: String) -> Self {
        truncate_check_error(&mut error);
        Self {
            channel,
            completed_at_unix_seconds: unix_time_now(),
            result: UpdateCheckResult::Failed,
            version: None,
            generation: None,
            platform: None,
            error: Some(error),
        }
    }

    /// Returns the channel used by an endpoint-catalog check.
    ///
    /// `None` identifies a raw-URL check.
    #[must_use]
    pub const fn channel(&self) -> Option<UpdateChannel> {
        self.channel
    }

    /// Returns the completion time as Unix seconds when the host clock allows it.
    #[must_use]
    pub const fn completed_at_unix_seconds(&self) -> Option<u64> {
        self.completed_at_unix_seconds
    }

    /// Returns the stable result category.
    #[must_use]
    pub const fn result(&self) -> UpdateCheckResult {
        self.result
    }

    /// Returns the signed release version for a successful check.
    #[must_use]
    pub const fn version(&self) -> Option<&Version> {
        self.version.as_ref()
    }

    /// Returns the signed generation for a successful check.
    #[must_use]
    pub const fn generation(&self) -> Option<u64> {
        self.generation
    }

    /// Returns the platform key relevant to the successful outcome.
    #[must_use]
    pub fn platform(&self) -> Option<&str> {
        self.platform.as_deref()
    }

    /// Returns the transport or verification error from a failed check.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn validate(&self) -> Result<()> {
        let successful = self.version.is_some()
            && self.generation.is_some_and(|generation| generation > 0)
            && self.error.is_none();
        let valid = match self.result {
            UpdateCheckResult::Current => successful && self.platform.is_none(),
            UpdateCheckResult::PlatformUnavailable | UpdateCheckResult::UpdateAvailable => {
                successful
                    && self
                        .platform
                        .as_ref()
                        .is_some_and(|platform| !platform.is_empty())
            }
            UpdateCheckResult::Failed => {
                self.version.is_none()
                    && self.generation.is_none()
                    && self.platform.is_none()
                    && self.error.as_ref().is_some_and(|error| {
                        !error.is_empty() && error.len() <= MAX_CHECK_ERROR_BYTES
                    })
            }
        };
        if valid {
            Ok(())
        } else {
            Err(Error::InvalidState(
                "the latest update-check record is inconsistent".into(),
            ))
        }
    }
}

/// Three distinct sibling paths used by an installation swap.
///
/// `installed` is the live file, `previous` retains the old live file, and
/// `failed_candidate` receives a replaced candidate during rollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallPlan {
    pub(crate) installed: PathBuf,
    pub(crate) previous: PathBuf,
    pub(crate) failed_candidate: PathBuf,
}

impl InstallPlan {
    /// Creates a same-directory atomic swap plan.
    ///
    /// Existence is checked when installation begins. The three names must be
    /// distinct and share one parent directory.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PathsNotSiblings`] if an atomic rename cannot connect
    /// all three paths.
    pub fn new(
        installed: impl Into<PathBuf>,
        previous: impl Into<PathBuf>,
        failed_candidate: impl Into<PathBuf>,
    ) -> Result<Self> {
        let installed = installed.into();
        let previous = previous.into();
        let failed_candidate = failed_candidate.into();
        let plan = Self {
            installed: absolute(&installed, "resolve installed update path")?,
            previous: absolute(&previous, "resolve retained update path")?,
            failed_candidate: absolute(&failed_candidate, "resolve failed update path")?,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Returns the live installation path.
    #[must_use]
    pub fn installed(&self) -> &Path {
        &self.installed
    }

    /// Returns the path that retains the previous installation.
    #[must_use]
    pub fn previous(&self) -> &Path {
        &self.previous
    }

    /// Returns the path used to preserve a rolled-back candidate.
    #[must_use]
    pub fn failed_candidate(&self) -> &Path {
        &self.failed_candidate
    }

    pub(crate) fn validate(&self) -> Result<()> {
        ensure_distinct_siblings(&[self.installed(), self.previous(), self.failed_candidate()])
    }
}

/// Read-only snapshot of the durable updater state.
///
/// Fields are private so callers cannot bypass transition validation. The JSON
/// representation is versioned by an internal `schema` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateState {
    pub(crate) schema: u32,
    pub(crate) phase: Phase,
    pub(crate) highest_accepted_generation: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) channel_generations: BTreeMap<UpdateChannel, u64>,
    pub(crate) candidate: Option<CandidateMetadata>,
    pub(crate) artifact: Option<ArtifactMetadata>,
    pub(crate) downloaded_path: Option<PathBuf>,
    pub(crate) staged_path: Option<PathBuf>,
    pub(crate) install_plan: Option<InstallPlan>,
    pub(crate) transient_paths: Vec<PathBuf>,
    pub(crate) rollback_requested: bool,
    pub(crate) failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_check: Option<UpdateCheckRecord>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            phase: Phase::Idle,
            highest_accepted_generation: 0,
            channel_generations: BTreeMap::new(),
            candidate: None,
            artifact: None,
            downloaded_path: None,
            staged_path: None,
            install_plan: None,
            transient_paths: Vec::new(),
            rollback_requested: false,
            failure: None,
            last_check: None,
        }
    }
}

impl UpdateState {
    /// Returns the durable lifecycle phase.
    #[must_use]
    pub const fn phase(&self) -> Phase {
        self.phase
    }

    /// Returns the highest signed generation accepted by this installation.
    #[must_use]
    pub const fn highest_accepted_generation(&self) -> u64 {
        self.highest_accepted_generation
    }

    /// Returns the highest signed generation accepted for one release channel.
    #[must_use]
    pub fn highest_accepted_generation_for(&self, channel: UpdateChannel) -> u64 {
        self.channel_generations.get(&channel).copied().unwrap_or(0)
    }

    /// Returns the accepted candidate, if one is active or retained on failure.
    #[must_use]
    pub fn candidate(&self) -> Option<&CandidateMetadata> {
        self.candidate.as_ref()
    }

    /// Returns the signed platform artifact, if one is active.
    #[must_use]
    pub fn artifact(&self) -> Option<&ArtifactMetadata> {
        self.artifact.as_ref()
    }

    /// Returns the verified download destination, if selected.
    #[must_use]
    pub fn downloaded_path(&self) -> Option<&Path> {
        self.downloaded_path.as_deref()
    }

    /// Returns the caller-selected staging path, if selected.
    #[must_use]
    pub fn staged_path(&self) -> Option<&Path> {
        self.staged_path.as_deref()
    }

    /// Returns the installation swap paths, if installation was requested.
    #[must_use]
    pub fn install_plan(&self) -> Option<&InstallPlan> {
        self.install_plan.as_ref()
    }

    /// Returns whether an explicit rollback was durably requested.
    #[must_use]
    pub const fn rollback_requested(&self) -> bool {
        self.rollback_requested
    }

    /// Returns the durable failure explanation, if one was recorded.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Returns the most recently completed signed update check.
    #[must_use]
    pub const fn last_check(&self) -> Option<&UpdateCheckRecord> {
        self.last_check.as_ref()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema != STATE_SCHEMA {
            return Err(Error::UnsupportedStateSchema(self.schema));
        }
        if self.transient_paths.len() > 8 {
            return Err(Error::InvalidState(
                "too many temporary paths are recorded".into(),
            ));
        }
        if self
            .channel_generations
            .values()
            .any(|generation| *generation == 0)
        {
            return Err(Error::InvalidState(
                "accepted channel generations must be greater than zero".into(),
            ));
        }
        if let Some(last_check) = &self.last_check {
            last_check.validate()?;
        }
        let mut unique = BTreeSet::new();
        if self.transient_paths.iter().any(|path| !unique.insert(path)) {
            return Err(Error::InvalidState(
                "the same temporary path is recorded more than once".into(),
            ));
        }

        if self
            .downloaded_path
            .iter()
            .chain(self.staged_path.iter())
            .chain(self.transient_paths.iter())
            .chain(
                self.install_plan
                    .iter()
                    .flat_map(|plan| [&plan.installed, &plan.previous, &plan.failed_candidate]),
            )
            .any(|path| !path.is_absolute())
        {
            return Err(Error::InvalidState(
                "persisted update paths must be absolute".into(),
            ));
        }
        if self.candidate.is_some() != self.artifact.is_some() {
            return Err(Error::InvalidState(
                "candidate and artifact metadata must be present together".into(),
            ));
        }
        if let Some(candidate) = &self.candidate {
            if candidate.generated == 0 {
                return Err(Error::InvalidState(
                    "candidate generation must be greater than zero".into(),
                ));
            }
            let watermark = candidate
                .channel
                .map_or(self.highest_accepted_generation, |channel| {
                    self.highest_accepted_generation_for(channel)
                });
            if candidate.generated > watermark {
                return Err(Error::InvalidState(
                    "candidate generation exceeds the accepted watermark".into(),
                ));
            }
        }
        if self.rollback_requested
            && (!matches!(
                self.phase,
                Phase::AwaitingRestart | Phase::Installed | Phase::Failed
            ) || self.install_plan.is_none())
        {
            return Err(Error::InvalidState(
                "rollback intent requires an installation plan and recoverable phase".into(),
            ));
        }

        self.validate_phase_shape()?;
        if let Some(plan) = &self.install_plan {
            plan.validate()?;
            if let Some(staged) = &self.staged_path {
                ensure_distinct_siblings(&[
                    staged,
                    plan.installed(),
                    plan.previous(),
                    plan.failed_candidate(),
                ])?;
            }
        }
        if let (Some(downloaded), Some(staged)) = (&self.downloaded_path, &self.staged_path) {
            ensure_distinct_siblings(&[downloaded, staged])?;
        }
        Ok(())
    }

    fn validate_phase_shape(&self) -> Result<()> {
        let candidate = self.candidate.is_some();
        let downloaded = self.downloaded_path.is_some();
        let staged = self.staged_path.is_some();
        let install = self.install_plan.is_some();
        let transient = !self.transient_paths.is_empty();
        let rollback = self.rollback_requested;
        let failure = self.failure.is_some();

        let valid = match self.phase {
            Phase::Idle => {
                !candidate
                    && !downloaded
                    && !staged
                    && !install
                    && !transient
                    && !rollback
                    && !failure
            }
            Phase::Checking => {
                !candidate
                    && !downloaded
                    && !staged
                    && !install
                    && transient
                    && !rollback
                    && !failure
            }
            Phase::UpdateAvailable => {
                candidate
                    && !downloaded
                    && !staged
                    && !install
                    && !transient
                    && !rollback
                    && !failure
            }
            Phase::Downloading => {
                candidate && downloaded && !staged && !install && transient && !rollback && !failure
            }
            Phase::Downloaded => {
                candidate && downloaded && !install && staged == transient && !rollback && !failure
            }
            Phase::Staged => {
                candidate && downloaded && staged && !install && !transient && !rollback && !failure
            }
            Phase::AwaitingRestart | Phase::Installed => {
                candidate && downloaded && staged && install && !transient && !failure
            }
            Phase::Failed => {
                failure
                    && !transient
                    && (!install || (candidate && downloaded && staged))
                    && (!staged || (candidate && downloaded))
                    && (!downloaded || candidate)
            }
            Phase::RolledBack => {
                candidate && downloaded && staged && install && !transient && !rollback && failure
            }
        };
        if valid {
            Ok(())
        } else {
            Err(Error::InvalidState(format!(
                "{:?} carries fields that do not match its lifecycle phase",
                self.phase
            )))
        }
    }
}

/// Reads a coherent durable state snapshot without running recovery.
///
/// This acquires the ordinary state lock and may create a missing idle state
/// document, but it never downloads, stages, installs, rolls back, or reconciles
/// a previously interrupted operation.
///
/// # Errors
///
/// Returns an I/O, validation, or lock error.
pub fn inspect_state(path: impl Into<PathBuf>) -> Result<UpdateState> {
    let (_state_file, state) = StateFile::open(path)?;
    Ok(state)
}

fn unix_time_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn truncate_check_error(error: &mut String) {
    const SUFFIX: &str = "...[truncated]";
    if error.len() <= MAX_CHECK_ERROR_BYTES {
        return;
    }
    let mut end = MAX_CHECK_ERROR_BYTES - SUFFIX.len();
    while !error.is_char_boundary(end) {
        end -= 1;
    }
    error.truncate(end);
    error.push_str(SUFFIX);
}

pub(crate) struct StateFile {
    path: PathBuf,
    _lock: File,
}

impl StateFile {
    pub(crate) fn open(path: impl Into<PathBuf>) -> Result<(Self, UpdateState)> {
        let path = path.into();
        let path = absolute(&path, "resolve update state path")?;
        let directory = parent(&path)?;
        fs::create_dir_all(directory)
            .map_err(|error| Error::io("create update state directory", directory, error))?;
        let lock_path = Self::lock_path(&path)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| Error::io("open update state lock", &lock_path, error))?;
        ensure_regular_file(&lock_path)?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Error::StateLocked(path));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(Error::io("lock update state", &lock_path, error));
            }
        }
        crate::fsutil::recover_atomic_write(&path)?;
        let state_file = Self { path, _lock: lock };

        match fs::symlink_metadata(&state_file.path) {
            Ok(_) => {
                ensure_regular_file(&state_file.path)?;
                let bytes = fs::read(&state_file.path)
                    .map_err(|error| Error::io("read update state", &state_file.path, error))?;
                let state: UpdateState = serde_json::from_slice(&bytes)
                    .map_err(|error| Error::json("update state", error))?;
                state.validate()?;
                Self::validate_transient_paths(&state, &state_file.path)?;
                Ok((state_file, state))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let state = UpdateState::default();
                state_file.persist(&state)?;
                Ok((state_file, state))
            }
            Err(error) => Err(Error::io("inspect update state", &state_file.path, error)),
        }
    }

    fn lock_path(path: &Path) -> Result<PathBuf> {
        let file_name = path.file_name().ok_or_else(|| {
            Error::InvalidState(format!("`{}` has no state file name", path.display()))
        })?;
        let mut lock_name = OsString::from(file_name);
        lock_name.push(".lock");
        Ok(parent(path)?.join(lock_name))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn persist(&self, state: &UpdateState) -> Result<()> {
        state.validate()?;
        Self::validate_transient_paths(state, &self.path)?;
        let mut bytes =
            serde_json::to_vec_pretty(state).map_err(|error| Error::json("update state", error))?;
        bytes.push(b'\n');
        atomic_write(&self.path, &bytes)
    }

    fn validate_transient_paths(state: &UpdateState, state_path: &Path) -> Result<()> {
        let (expected_parent, expected_labels): (&Path, &[&str]) =
            match state.phase {
                Phase::Checking => (parent(state_path)?, &["manifest", "signature"]),
                Phase::Downloading => {
                    (
                        parent(state.downloaded_path.as_deref().ok_or_else(|| {
                            Error::InvalidState("download path is absent".into())
                        })?)?,
                        &["download"],
                    )
                }
                Phase::Downloaded if !state.transient_paths.is_empty() => {
                    (
                        parent(state.staged_path.as_deref().ok_or_else(|| {
                            Error::InvalidState("staging path is absent".into())
                        })?)?,
                        &["stage"],
                    )
                }
                _ => return Ok(()),
            };

        if state.transient_paths.iter().all(|path| {
            parent(path).is_ok_and(|path_parent| path_parent == expected_parent)
                && expected_labels
                    .iter()
                    .any(|label| Self::reserved_name(path, label))
        }) {
            Ok(())
        } else {
            Err(Error::InvalidState(
                "temporary update path is outside its reserved operation scope".into(),
            ))
        }
    }

    fn reserved_name(path: &Path, label: &str) -> bool {
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        let prefix = format!(".scrozz-{label}-");
        let Some(numbers) = name
            .strip_prefix(&prefix)
            .and_then(|name| name.strip_suffix(".part"))
        else {
            return false;
        };
        let Some((process, sequence)) = numbers.split_once('-') else {
            return false;
        };
        !process.is_empty()
            && process.bytes().all(|byte| byte.is_ascii_digit())
            && !sequence.is_empty()
            && sequence.bytes().all(|byte| byte.is_ascii_digit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PHASES: [Phase; 10] = [
        Phase::Idle,
        Phase::Checking,
        Phase::UpdateAvailable,
        Phase::Downloading,
        Phase::Downloaded,
        Phase::Staged,
        Phase::AwaitingRestart,
        Phase::Installed,
        Phase::Failed,
        Phase::RolledBack,
    ];

    #[test]
    fn only_explicit_state_transitions_are_valid() {
        let valid = [
            (Phase::Idle, Phase::Checking),
            (Phase::Checking, Phase::Idle),
            (Phase::Checking, Phase::UpdateAvailable),
            (Phase::Checking, Phase::Failed),
            (Phase::UpdateAvailable, Phase::Idle),
            (Phase::UpdateAvailable, Phase::Downloading),
            (Phase::UpdateAvailable, Phase::Failed),
            (Phase::Downloading, Phase::Idle),
            (Phase::Downloading, Phase::Downloaded),
            (Phase::Downloading, Phase::Failed),
            (Phase::Downloaded, Phase::Idle),
            (Phase::Downloaded, Phase::Staged),
            (Phase::Downloaded, Phase::Failed),
            (Phase::Staged, Phase::Idle),
            (Phase::Staged, Phase::AwaitingRestart),
            (Phase::Staged, Phase::Failed),
            (Phase::AwaitingRestart, Phase::Installed),
            (Phase::AwaitingRestart, Phase::Failed),
            (Phase::AwaitingRestart, Phase::RolledBack),
            (Phase::Installed, Phase::RolledBack),
            (Phase::Installed, Phase::Idle),
            (Phase::Failed, Phase::RolledBack),
            (Phase::Failed, Phase::Idle),
            (Phase::Failed, Phase::UpdateAvailable),
            (Phase::RolledBack, Phase::Idle),
        ];

        for from in PHASES {
            for to in PHASES {
                assert_eq!(
                    from.can_transition_to(to),
                    valid.contains(&(from, to)),
                    "unexpected transition {from:?} -> {to:?}"
                );
            }
        }
    }

    #[test]
    fn install_plan_requires_three_distinct_siblings() {
        assert!(InstallPlan::new("/one/live", "/one/old", "/one/failed").is_ok());
        assert!(InstallPlan::new("/one/live", "/two/old", "/one/failed").is_err());
        assert!(InstallPlan::new("/one/live", "/one/live", "/one/failed").is_err());
    }

    #[test]
    fn failed_check_details_are_bounded_without_breaking_utf8() {
        let record = UpdateCheckRecord::failed(
            Some(UpdateChannel::Stable),
            "failure \u{1f4a5}".repeat(MAX_CHECK_ERROR_BYTES),
        );
        let error = record.error().unwrap();

        assert!(error.len() <= MAX_CHECK_ERROR_BYTES);
        assert!(error.ends_with("...[truncated]"));
        record.validate().unwrap();
    }
}
