use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::{
    ArtifactMetadata, Error, Result,
    fsutil::{absolute, atomic_write, ensure_distinct_siblings, ensure_regular_file, parent},
};

const STATE_SCHEMA: u32 = 1;

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
}

impl CandidateMetadata {
    pub(crate) fn new(version: Version, generated: u64) -> Self {
        Self { version, generated }
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
    pub(crate) candidate: Option<CandidateMetadata>,
    pub(crate) artifact: Option<ArtifactMetadata>,
    pub(crate) downloaded_path: Option<PathBuf>,
    pub(crate) staged_path: Option<PathBuf>,
    pub(crate) install_plan: Option<InstallPlan>,
    pub(crate) transient_paths: Vec<PathBuf>,
    pub(crate) rollback_requested: bool,
    pub(crate) failure: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA,
            phase: Phase::Idle,
            highest_accepted_generation: 0,
            candidate: None,
            artifact: None,
            downloaded_path: None,
            staged_path: None,
            install_plan: None,
            transient_paths: Vec::new(),
            rollback_requested: false,
            failure: None,
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

    pub(crate) fn validate(&self) -> Result<()> {
        if self.schema != STATE_SCHEMA {
            return Err(Error::UnsupportedStateSchema(self.schema));
        }
        if self.transient_paths.len() > 8 {
            return Err(Error::InvalidState(
                "too many temporary paths are recorded".into(),
            ));
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
            if candidate.generated > self.highest_accepted_generation {
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
}
