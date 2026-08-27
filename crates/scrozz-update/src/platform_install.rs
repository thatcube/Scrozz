use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};

use crate::{
    ArtifactKind, ArtifactMetadata, Error, Result, StagedArtifact, VerifiedArtifact, fsutil,
    verify_artifact_file,
};

const HANDOFF_SCHEMA: u32 = 1;
const MAX_FAILURE_BYTES: usize = 8192;

/// Durable phase of an explicit native-package installation handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PlatformInstallPhase {
    /// A verified regular-file archive and target plan are ready.
    Prepared,
    /// The helper started an idempotent native installation.
    Applying,
    /// The native installation completed.
    Installed,
    /// A native installation or validation step failed without claiming success.
    Failed,
    /// An explicit rollback started.
    RollingBack,
    /// The retained prior installation was restored.
    RolledBack,
    /// The user accepted the current layout and removed retained rollback data.
    Accepted,
}

impl PlatformInstallPhase {
    /// Returns the stable status token.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::Applying => "applying",
            Self::Installed => "installed",
            Self::Failed => "failed",
            Self::RollingBack => "rolling-back",
            Self::RolledBack => "rolled-back",
            Self::Accepted => "accepted",
        }
    }
}

/// Same-volume paths used for a crash-safe directory replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SwapInstallPlan {
    installed: PathBuf,
    previous: PathBuf,
    failed_candidate: PathBuf,
    candidate: PathBuf,
    unpack: PathBuf,
}

impl SwapInstallPlan {
    fn new(installed: impl Into<PathBuf>) -> Result<Self> {
        let installed = fsutil::absolute(&installed.into(), "resolve native install path")?;
        let parent = fsutil::parent(&installed)?;
        let name = installed.file_name().ok_or_else(|| {
            Error::InvalidState(format!(
                "native install path `{}` has no final component",
                installed.display()
            ))
        })?;
        let derived = |suffix: &str| {
            let mut value = OsString::from(".");
            value.push(name);
            value.push(suffix);
            parent.join(value)
        };
        let previous = derived(".scrozz-previous");
        let failed_candidate = derived(".scrozz-failed");
        let candidate = derived(".scrozz-candidate");
        let unpack = derived(".scrozz-unpack");
        let plan = Self {
            installed,
            previous,
            failed_candidate,
            candidate,
            unpack,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Returns the live bundle or distribution directory.
    #[must_use]
    pub fn installed(&self) -> &Path {
        &self.installed
    }

    /// Returns the retained prior installation.
    #[must_use]
    pub fn previous(&self) -> &Path {
        &self.previous
    }

    /// Returns where a rolled-back candidate is preserved.
    #[must_use]
    pub fn failed_candidate(&self) -> &Path {
        &self.failed_candidate
    }

    fn validate(&self) -> Result<()> {
        if [
            &self.installed,
            &self.previous,
            &self.failed_candidate,
            &self.candidate,
            &self.unpack,
        ]
        .iter()
        .any(|path| !path.is_absolute())
        {
            return Err(Error::InvalidState(
                "native installation paths must be absolute".into(),
            ));
        }
        fsutil::ensure_distinct_siblings(&[
            &self.installed,
            &self.previous,
            &self.failed_candidate,
            &self.candidate,
            &self.unpack,
        ])?;
        if *self != Self::new_unchecked(self.installed.clone())? {
            return Err(Error::InvalidState(
                "native installation work paths were not derived from the live path".into(),
            ));
        }
        Ok(())
    }

    fn new_unchecked(installed: PathBuf) -> Result<Self> {
        let parent = fsutil::parent(&installed)?;
        let name = installed.file_name().ok_or_else(|| {
            Error::InvalidState(format!(
                "native install path `{}` has no final component",
                installed.display()
            ))
        })?;
        let derived = |suffix: &str| {
            let mut value = OsString::from(".");
            value.push(name);
            value.push(suffix);
            parent.join(value)
        };
        let previous = derived(".scrozz-previous");
        let failed_candidate = derived(".scrozz-failed");
        let candidate = derived(".scrozz-candidate");
        let unpack = derived(".scrozz-unpack");
        Ok(Self {
            installed,
            previous,
            failed_candidate,
            candidate,
            unpack,
        })
    }
}

/// Native installation target attached to a signed artifact kind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PlatformInstallTarget {
    /// Signed/notarized macOS application-bundle replacement.
    MacosApp {
        /// Same-volume paths used for the bundle swap.
        swap: SwapInstallPlan,
    },
    /// Windows package deployment, optionally retaining a prior MSIX for downgrade.
    WindowsMsix {
        /// Package name that must be present after deployment.
        package_identity: String,
        /// Prior signed package retained for an explicit downgrade.
        rollback_package: Option<PathBuf>,
    },
    /// Portable Windows distribution-directory replacement.
    WindowsPortable {
        /// Same-volume paths used for the distribution swap.
        swap: SwapInstallPlan,
    },
    /// Linux AppDir replacement.
    LinuxAppdir {
        /// Same-volume paths used for the AppDir swap.
        swap: SwapInstallPlan,
    },
}

impl PlatformInstallTarget {
    /// Creates a macOS `.app` swap target.
    pub fn macos_app(installed: impl Into<PathBuf>) -> Result<Self> {
        let target = Self::MacosApp {
            swap: SwapInstallPlan::new(installed)?,
        };
        target.validate()?;
        Ok(target)
    }

    /// Creates a portable Windows distribution-directory swap target.
    pub fn windows_portable(installed: impl Into<PathBuf>) -> Result<Self> {
        let target = Self::WindowsPortable {
            swap: SwapInstallPlan::new(installed)?,
        };
        target.validate()?;
        Ok(target)
    }

    /// Creates a Linux AppDir swap target.
    pub fn linux_appdir(installed: impl Into<PathBuf>) -> Result<Self> {
        let target = Self::LinuxAppdir {
            swap: SwapInstallPlan::new(installed)?,
        };
        target.validate()?;
        Ok(target)
    }

    /// Creates an MSIX package-deployment target.
    ///
    /// `rollback_package` is optional because Windows package deployment is
    /// transactional on failure. A post-success downgrade is exposed only when
    /// the caller deliberately retains a prior signed MSIX.
    pub fn windows_msix(
        package_identity: impl Into<String>,
        rollback_package: Option<PathBuf>,
    ) -> Result<Self> {
        let target = Self::WindowsMsix {
            package_identity: package_identity.into(),
            rollback_package: rollback_package
                .map(|path| fsutil::absolute(&path, "resolve retained MSIX path"))
                .transpose()?,
        };
        target.validate()?;
        Ok(target)
    }

    /// Returns the signed artifact kind this target accepts.
    #[must_use]
    pub const fn artifact_kind(&self) -> ArtifactKind {
        match self {
            Self::MacosApp { .. } => ArtifactKind::MacosAppZip,
            Self::WindowsMsix { .. } => ArtifactKind::WindowsMsix,
            Self::WindowsPortable { .. } => ArtifactKind::WindowsPortableZip,
            Self::LinuxAppdir { .. } => ArtifactKind::LinuxAppdirTarGz,
        }
    }

    /// Returns whether a post-success rollback has a retained source.
    #[must_use]
    pub fn rollback_available(&self) -> bool {
        match self {
            Self::WindowsMsix {
                rollback_package, ..
            } => rollback_package.is_some(),
            _ => true,
        }
    }

    /// Returns the stable target token.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::MacosApp { .. } => "macos-app",
            Self::WindowsMsix { .. } => "windows-msix",
            Self::WindowsPortable { .. } => "windows-portable",
            Self::LinuxAppdir { .. } => "linux-appdir",
        }
    }

    fn swap(&self) -> Option<&SwapInstallPlan> {
        match self {
            Self::MacosApp { swap }
            | Self::WindowsPortable { swap }
            | Self::LinuxAppdir { swap } => Some(swap),
            Self::WindowsMsix { .. } => None,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::MacosApp { swap } => {
                swap.validate()?;
                if swap.installed().file_name() != Some(OsStr::new("Scrozz.app")) {
                    return Err(Error::InvalidState(
                        "macOS native updates may replace only Scrozz.app".into(),
                    ));
                }
                Ok(())
            }
            Self::WindowsPortable { swap } => swap.validate(),
            Self::LinuxAppdir { swap } => {
                swap.validate()?;
                if swap.installed().file_name() != Some(OsStr::new("Scrozz.AppDir")) {
                    return Err(Error::InvalidState(
                        "Linux native updates may replace only Scrozz.AppDir".into(),
                    ));
                }
                Ok(())
            }
            Self::WindowsMsix {
                package_identity,
                rollback_package,
            } => {
                if package_identity.len() < 3
                    || package_identity.len() > 50
                    || !package_identity
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
                {
                    return Err(Error::InvalidState(
                        "MSIX package identity is not a valid package name".into(),
                    ));
                }
                if rollback_package
                    .as_ref()
                    .is_some_and(|path| !path.is_absolute())
                {
                    return Err(Error::InvalidState(
                        "retained MSIX path must be absolute".into(),
                    ));
                }
                if rollback_package.as_ref().is_some_and(|path| {
                    match path.extension().and_then(OsStr::to_str) {
                        Some(extension) => !extension.eq_ignore_ascii_case("msix"),
                        None => true,
                    }
                }) {
                    return Err(Error::InvalidState(
                        "retained Windows rollback package must end in .msix".into(),
                    ));
                }
                Ok(())
            }
        }
    }

    fn validate_live_installation(&self) -> Result<()> {
        match self {
            Self::MacosApp { swap } => {
                ensure_directory(swap.installed())?;
                fsutil::ensure_regular_file(&swap.installed().join("Contents/MacOS/Scrozz"))
            }
            Self::WindowsPortable { swap } => {
                ensure_directory(swap.installed())?;
                for required in ["scrozz.exe", "LICENSE", "README.md", "TRADEMARK.md"] {
                    fsutil::ensure_regular_file(&swap.installed().join(required))?;
                }
                Ok(())
            }
            Self::LinuxAppdir { swap } => {
                ensure_directory(swap.installed())?;
                fsutil::ensure_regular_file(&swap.installed().join("usr/bin/scrozz"))?;
                fsutil::ensure_regular_file(&swap.installed().join("scrozz.desktop"))
            }
            Self::WindowsMsix {
                rollback_package, ..
            } => rollback_package
                .as_deref()
                .map(fsutil::ensure_regular_file)
                .unwrap_or(Ok(())),
        }
    }
}

/// Durable native-package handoff contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformInstallState {
    schema: u32,
    phase: PlatformInstallPhase,
    archive: PathBuf,
    artifact: ArtifactMetadata,
    target: PlatformInstallTarget,
    failure: Option<String>,
}

impl PlatformInstallState {
    /// Returns the handoff phase.
    #[must_use]
    pub const fn phase(&self) -> PlatformInstallPhase {
        self.phase
    }

    /// Returns the verified regular-file archive.
    #[must_use]
    pub fn archive(&self) -> &Path {
        &self.archive
    }

    /// Returns the exact signed artifact metadata.
    #[must_use]
    pub const fn artifact(&self) -> &ArtifactMetadata {
        &self.artifact
    }

    /// Returns the native installation target.
    #[must_use]
    pub const fn target(&self) -> &PlatformInstallTarget {
        &self.target
    }

    /// Returns the durable failure, if any.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    fn validate(&self) -> Result<()> {
        if self.schema != HANDOFF_SCHEMA {
            return Err(Error::InvalidState(format!(
                "unsupported native handoff schema {}",
                self.schema
            )));
        }
        if !self.archive.is_absolute() {
            return Err(Error::InvalidState(
                "native handoff archive path must be absolute".into(),
            ));
        }
        self.target.validate()?;
        if self.artifact.kind() != self.target.artifact_kind() {
            return Err(Error::ArtifactInstallMismatch {
                artifact: self.artifact.kind().as_str(),
                target: self.target.as_str(),
            });
        }
        let failure_is_valid = match self.phase {
            PlatformInstallPhase::Failed | PlatformInstallPhase::RolledBack => self
                .failure
                .as_ref()
                .is_some_and(|failure| !failure.is_empty() && failure.len() <= MAX_FAILURE_BYTES),
            _ => self.failure.is_none(),
        };
        if !failure_is_valid {
            return Err(Error::InvalidState(
                "native handoff failure does not match its phase".into(),
            ));
        }
        Ok(())
    }
}

/// Applies and rolls back one platform installation plan.
///
/// Implementations must be idempotent: [`PlatformHandoff::apply`] calls
/// `install` again when it recovers a durable `Applying` phase.
pub trait PlatformInstallAdapter {
    /// Applies a verified archive to its native target.
    fn install(&self, state: &PlatformInstallState) -> Result<()>;

    /// Restores the retained previous installation.
    fn rollback(&self, state: &PlatformInstallState) -> Result<()>;

    /// Removes only the plan-derived retained rollback payload.
    fn accept(&self, state: &PlatformInstallState) -> Result<()>;
}

/// Locked, crash-safe native installation handoff.
pub struct PlatformHandoff {
    file: HandoffFile,
    state: PlatformInstallState,
}

impl PlatformHandoff {
    /// Persists an explicit native installation request.
    ///
    /// This verifies the staged regular file again. It never launches a helper
    /// or mutates the live installation.
    pub fn begin(
        path: impl Into<PathBuf>,
        staged: &StagedArtifact,
        target: PlatformInstallTarget,
    ) -> Result<Self> {
        let path = fsutil::absolute(&path.into(), "resolve native handoff path")?;
        fsutil::ensure_regular_file(staged.path())?;
        verify_artifact_file(staged.path(), staged.artifact())?;
        if staged.artifact().metadata().kind() != target.artifact_kind() {
            return Err(Error::ArtifactInstallMismatch {
                artifact: staged.artifact().metadata().kind().as_str(),
                target: target.as_str(),
            });
        }
        target.validate()?;
        target.validate_live_installation()?;
        if let Some(swap) = target.swap() {
            for path in [
                &swap.previous,
                &swap.failed_candidate,
                &swap.candidate,
                &swap.unpack,
            ] {
                fsutil::ensure_absent(path)?;
            }
        }
        let file = HandoffFile::create(path)?;
        let state = PlatformInstallState {
            schema: HANDOFF_SCHEMA,
            phase: PlatformInstallPhase::Prepared,
            archive: fsutil::absolute(staged.path(), "resolve staged native archive")?,
            artifact: staged.artifact().metadata().clone(),
            target,
            failure: None,
        };
        file.persist(&state)?;
        Ok(Self { file, state })
    }

    /// Opens an existing handoff without applying it.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let (file, state) = HandoffFile::open(path)?;
        Ok(Self { file, state })
    }

    /// Returns the handoff state document path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.file.path
    }

    /// Returns the coherent handoff state.
    #[must_use]
    pub const fn state(&self) -> &PlatformInstallState {
        &self.state
    }

    /// Applies or recovers an explicitly prepared native installation.
    pub fn apply(&mut self, adapter: &impl PlatformInstallAdapter) -> Result<()> {
        if !matches!(
            self.state.phase,
            PlatformInstallPhase::Prepared
                | PlatformInstallPhase::Applying
                | PlatformInstallPhase::Failed
        ) {
            return Err(Error::PlatformInstall(format!(
                "cannot apply a handoff in phase {:?}",
                self.state.phase
            )));
        }
        let artifact = VerifiedArtifact::from_persisted(self.state.artifact.clone());
        verify_artifact_file(&self.state.archive, &artifact)?;

        self.state.phase = PlatformInstallPhase::Applying;
        self.state.failure = None;
        self.file.persist(&self.state)?;
        match adapter.install(&self.state) {
            Ok(()) => {
                self.state.phase = PlatformInstallPhase::Installed;
                self.state.failure = None;
                self.file.persist(&self.state)
            }
            Err(error) => {
                self.record_failure(error.to_string())?;
                Err(error)
            }
        }
    }

    /// Explicitly restores the retained prior native installation.
    pub fn rollback(&mut self, adapter: &impl PlatformInstallAdapter) -> Result<()> {
        if !matches!(
            self.state.phase,
            PlatformInstallPhase::Installed
                | PlatformInstallPhase::Failed
                | PlatformInstallPhase::RollingBack
        ) {
            return Err(Error::PlatformInstall(format!(
                "cannot roll back a handoff in phase {:?}",
                self.state.phase
            )));
        }

        if !self.state.target.rollback_available() {
            return Err(Error::PlatformRollbackUnavailable);
        }
        let prior_failure = self
            .state
            .failure
            .clone()
            .unwrap_or_else(|| "explicit native update rollback".into());
        self.state.phase = PlatformInstallPhase::RollingBack;
        self.state.failure = None;
        self.file.persist(&self.state)?;
        match adapter.rollback(&self.state) {
            Ok(()) => {
                self.state.phase = PlatformInstallPhase::RolledBack;
                self.state.failure = Some(prior_failure);
                self.file.persist(&self.state)
            }
            Err(error) => {
                self.record_failure(error.to_string())?;
                Err(error)
            }
        }
    }

    /// Accepts the installed or rolled-back result and removes retained payloads.
    pub fn accept(&mut self, adapter: &impl PlatformInstallAdapter) -> Result<()> {
        if self.state.phase == PlatformInstallPhase::Accepted {
            return Ok(());
        }
        if !matches!(
            self.state.phase,
            PlatformInstallPhase::Installed | PlatformInstallPhase::RolledBack
        ) {
            return Err(Error::PlatformInstall(format!(
                "cannot accept a handoff in phase {:?}",
                self.state.phase
            )));
        }
        adapter.accept(&self.state)?;
        self.state.phase = PlatformInstallPhase::Accepted;
        self.state.failure = None;
        self.file.persist(&self.state)
    }

    fn record_failure(&mut self, mut failure: String) -> Result<()> {
        truncate_failure(&mut failure);
        self.state.phase = PlatformInstallPhase::Failed;
        self.state.failure = Some(failure);
        self.file.persist(&self.state)
    }
}

/// Production adapter using only fixed native tools and argument vectors.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativePlatformInstallAdapter;

impl PlatformInstallAdapter for NativePlatformInstallAdapter {
    fn install(&self, state: &PlatformInstallState) -> Result<()> {
        match state.target() {
            PlatformInstallTarget::MacosApp { swap } => {
                require_host("macos")?;
                install_swap(state.archive(), ArtifactKind::MacosAppZip, swap)
            }
            PlatformInstallTarget::WindowsMsix {
                package_identity, ..
            } => {
                require_host("windows")?;
                install_msix(state.archive(), package_identity)
            }
            PlatformInstallTarget::WindowsPortable { swap } => {
                require_host("windows")?;
                install_swap(state.archive(), ArtifactKind::WindowsPortableZip, swap)
            }
            PlatformInstallTarget::LinuxAppdir { swap } => {
                require_host("linux")?;
                install_swap(state.archive(), ArtifactKind::LinuxAppdirTarGz, swap)
            }
        }
    }

    fn rollback(&self, state: &PlatformInstallState) -> Result<()> {
        match state.target() {
            PlatformInstallTarget::MacosApp { swap } => {
                require_host("macos")?;
                rollback_swap(swap)
            }
            PlatformInstallTarget::WindowsMsix {
                package_identity,
                rollback_package: Some(package),
            } => {
                require_host("windows")?;
                fsutil::ensure_regular_file(package)?;
                install_msix(package, package_identity)
            }
            PlatformInstallTarget::WindowsMsix {
                rollback_package: None,
                ..
            } => Err(Error::PlatformRollbackUnavailable),
            PlatformInstallTarget::WindowsPortable { swap } => {
                require_host("windows")?;
                rollback_swap(swap)
            }
            PlatformInstallTarget::LinuxAppdir { swap } => {
                require_host("linux")?;
                rollback_swap(swap)
            }
        }
    }

    fn accept(&self, state: &PlatformInstallState) -> Result<()> {
        let Some(swap) = state.target().swap() else {
            return Ok(());
        };
        match state.phase() {
            PlatformInstallPhase::Installed => {
                remove_retained_tree(&swap.previous, swap)?;
            }
            PlatformInstallPhase::RolledBack => {
                remove_retained_tree(&swap.failed_candidate, swap)?;
            }
            _ => {
                return Err(Error::PlatformInstall(format!(
                    "cannot remove rollback data in phase {:?}",
                    state.phase()
                )));
            }
        }
        Ok(())
    }
}

fn install_swap(archive: &Path, kind: ArtifactKind, swap: &SwapInstallPlan) -> Result<()> {
    let installed = path_exists(&swap.installed)?;
    let previous = path_exists(&swap.previous)?;
    let candidate = path_exists(&swap.candidate)?;
    if path_exists(&swap.failed_candidate)? {
        return Err(Error::PlatformInstall(format!(
            "failed-candidate path already exists at `{}`",
            swap.failed_candidate.display()
        )));
    }
    if installed && previous && !candidate {
        validate_candidate(kind, &swap.installed)?;
        return Ok(());
    }
    if !candidate {
        prepare_candidate(archive, kind, swap)?;
    } else {
        validate_candidate(kind, &swap.candidate)?;
        if path_exists(&swap.unpack)? {
            remove_work_tree(&swap.unpack, swap)?;
        }
    }

    match (path_exists(&swap.installed)?, path_exists(&swap.previous)?) {
        (true, false) => fsutil::rename_synced(&swap.installed, &swap.previous)?,
        (false, true) => {}
        (true, true) => {
            return Err(Error::Recovery(
                "live and retained installations both exist before candidate activation".into(),
            ));
        }
        (false, false) => {
            return Err(Error::Recovery(
                "native installation lost both the live and retained copies".into(),
            ));
        }
    }

    if let Err(error) = fsutil::rename_synced(&swap.candidate, &swap.installed) {
        let restore = fsutil::rename_synced(&swap.previous, &swap.installed);
        return match restore {
            Ok(()) => Err(error),
            Err(restore) => Err(Error::Recovery(format!(
                "candidate activation failed ({error}); restoring the retained installation also failed ({restore})"
            ))),
        };
    }
    if let Err(error) = validate_candidate(kind, &swap.installed) {
        let _ = fsutil::rename_synced(&swap.installed, &swap.failed_candidate);
        let restore = fsutil::rename_synced(&swap.previous, &swap.installed);
        return match restore {
            Ok(()) => Err(error),
            Err(restore) => Err(Error::Recovery(format!(
                "activated candidate failed validation ({error}); restoring the retained installation also failed ({restore})"
            ))),
        };
    }
    Ok(())
}

fn rollback_swap(swap: &SwapInstallPlan) -> Result<()> {
    if path_exists(&swap.unpack)? {
        remove_work_tree(&swap.unpack, swap)?;
    }
    if path_exists(&swap.candidate)? {
        if path_exists(&swap.failed_candidate)? {
            if !path_exists(&swap.installed)? && !path_exists(&swap.previous)? {
                return Err(Error::Recovery(
                    "native rollback found no known-good copy beside candidate payloads".into(),
                ));
            }
            remove_work_tree(&swap.candidate, swap)?;
        } else {
            fsutil::rename_synced(&swap.candidate, &swap.failed_candidate)?;
        }
    }

    let installed = path_exists(&swap.installed)?;
    let previous = path_exists(&swap.previous)?;
    let failed = path_exists(&swap.failed_candidate)?;
    match (installed, previous, failed) {
        (true, true, false) => {
            fsutil::rename_synced(&swap.installed, &swap.failed_candidate)?;
            fsutil::rename_synced(&swap.previous, &swap.installed)
        }
        (false, true, true) => fsutil::rename_synced(&swap.previous, &swap.installed),
        (true, false, true) => Ok(()),
        (true, false, false) => Ok(()),
        _ => Err(Error::Recovery(
            "native rollback layout cannot identify one retained good copy".into(),
        )),
    }
}

fn prepare_candidate(archive: &Path, kind: ArtifactKind, swap: &SwapInstallPlan) -> Result<()> {
    if path_exists(&swap.unpack)? {
        remove_work_tree(&swap.unpack, swap)?;
    }
    fs::create_dir(&swap.unpack)
        .map_err(|error| Error::io("create native update unpack directory", &swap.unpack, error))?;

    let extracted = match kind {
        ArtifactKind::MacosAppZip => {
            run_command(
                Command::new("/usr/bin/ditto")
                    .args(["-x", "-k"])
                    .arg(archive)
                    .arg(&swap.unpack),
                "extract macOS application archive",
            )?;
            let distribution = sole_distribution_root(&swap.unpack)?;
            distribution.join("Scrozz.app")
        }
        ArtifactKind::WindowsPortableZip => {
            let script = concat!(
                "$ErrorActionPreference='Stop';",
                "Expand-Archive -LiteralPath $env:SCROZZ_UPDATE_ARCHIVE ",
                "-DestinationPath $env:SCROZZ_UPDATE_UNPACK"
            );
            run_command(
                Command::new("powershell.exe")
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        script,
                    ])
                    .env("SCROZZ_UPDATE_ARCHIVE", archive)
                    .env("SCROZZ_UPDATE_UNPACK", &swap.unpack),
                "extract portable Windows archive",
            )?;
            sole_distribution_root(&swap.unpack)?
        }
        ArtifactKind::LinuxAppdirTarGz => {
            run_command(
                Command::new("tar")
                    .arg("-xzf")
                    .arg(archive)
                    .arg("-C")
                    .arg(&swap.unpack),
                "extract Linux AppDir archive",
            )?;
            swap.unpack.join("Scrozz.AppDir")
        }
        ArtifactKind::RawExecutable | ArtifactKind::WindowsMsix => {
            return Err(Error::ArtifactInstallMismatch {
                artifact: kind.as_str(),
                target: "directory-swap",
            });
        }
    };

    ensure_directory(&extracted)?;
    fsutil::ensure_absent(&swap.candidate)?;
    fsutil::rename_synced(&extracted, &swap.candidate)?;
    remove_work_tree(&swap.unpack, swap)?;
    validate_candidate(kind, &swap.candidate)
}

fn validate_candidate(kind: ArtifactKind, candidate: &Path) -> Result<()> {
    ensure_directory(candidate)?;
    match kind {
        ArtifactKind::MacosAppZip => {
            let executable = candidate.join("Contents/MacOS/Scrozz");
            fsutil::ensure_regular_file(&executable)?;
            run_command(
                Command::new("/usr/bin/codesign")
                    .args(["--verify", "--deep", "--strict", "--verbose=2"])
                    .arg(candidate),
                "verify macOS application signature",
            )?;
            run_command(
                Command::new("/usr/sbin/spctl")
                    .args(["--assess", "--type", "execute", "--verbose=2"])
                    .arg(candidate),
                "assess macOS application trust",
            )?;
            run_command(
                Command::new("/usr/bin/xcrun")
                    .args(["stapler", "validate"])
                    .arg(candidate),
                "validate macOS notarization ticket",
            )
        }
        ArtifactKind::WindowsPortableZip => {
            let executable = candidate.join("scrozz.exe");
            for required in [
                "scrozz.exe",
                "LICENSE",
                "README.md",
                "TRADEMARK.md",
                "tesseract/tesseract.exe",
                "tesseract/tessdata/eng.traineddata",
            ] {
                fsutil::ensure_regular_file(&candidate.join(required))?;
            }
            let script = concat!(
                "$ErrorActionPreference='Stop';",
                "$s=Get-AuthenticodeSignature -LiteralPath $env:SCROZZ_UPDATE_EXE;",
                "if ($s.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {",
                "throw ('Authenticode status: '+$s.Status)",
                "}"
            );
            run_command(
                Command::new("powershell.exe")
                    .args([
                        "-NoLogo",
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        script,
                    ])
                    .env("SCROZZ_UPDATE_EXE", executable),
                "verify portable Windows Authenticode signature",
            )
        }
        ArtifactKind::LinuxAppdirTarGz => {
            fsutil::ensure_regular_file(&candidate.join("usr/bin/scrozz"))?;
            fsutil::ensure_regular_file(&candidate.join("scrozz.desktop"))
        }
        ArtifactKind::RawExecutable | ArtifactKind::WindowsMsix => {
            Err(Error::ArtifactInstallMismatch {
                artifact: kind.as_str(),
                target: "directory-swap",
            })
        }
    }
}

fn install_msix(archive: &Path, package_identity: &str) -> Result<()> {
    fsutil::ensure_regular_file(archive)?;
    let script = concat!(
        "$ErrorActionPreference='Stop';",
        "Add-AppxPackage -Path $env:SCROZZ_UPDATE_MSIX ",
        "-ForceApplicationShutdown -ForceUpdateFromAnyVersion;",
        "$p=Get-AppxPackage -Name $env:SCROZZ_UPDATE_IDENTITY;",
        "if ($null -eq $p) { throw 'Package deployment completed without the expected identity' }"
    );
    run_command(
        Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .env("SCROZZ_UPDATE_MSIX", archive)
            .env("SCROZZ_UPDATE_IDENTITY", package_identity),
        "deploy Windows package",
    )
}

fn sole_distribution_root(root: &Path) -> Result<PathBuf> {
    let mut roots = Vec::new();
    for entry in fs::read_dir(root)
        .map_err(|error| Error::io("read native update unpack directory", root, error))?
    {
        let entry =
            entry.map_err(|error| Error::io("read native update unpack entry", root, error))?;
        if entry.file_name() == OsStr::new("__MACOSX") {
            continue;
        }
        let metadata = entry.file_type().map_err(|error| {
            Error::io("inspect native update unpack entry", entry.path(), error)
        })?;
        if !metadata.is_dir() {
            return Err(Error::PlatformInstall(
                "native archive contains a loose top-level file".into(),
            ));
        }
        roots.push(entry.path());
    }
    if roots.len() != 1 {
        return Err(Error::PlatformInstall(format!(
            "native archive must contain one distribution root, found {}",
            roots.len()
        )));
    }
    Ok(roots.remove(0))
}

fn run_command(command: &mut Command, purpose: &str) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| Error::PlatformInstall(format!("{purpose}: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    truncate_failure(&mut stderr);
    Err(Error::PlatformInstall(format!(
        "{purpose} exited with status {}: {}",
        output
            .status
            .code()
            .map_or_else(|| "signal".into(), |code| code.to_string()),
        stderr.trim()
    )))
}

fn require_host(expected: &str) -> Result<()> {
    if std::env::consts::OS == expected {
        Ok(())
    } else {
        Err(Error::PlatformInstall(format!(
            "{} installation cannot run on {}",
            expected,
            std::env::consts::OS
        )))
    }
}

fn ensure_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| Error::io("inspect native installation directory", path, error))?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(Error::PlatformInstall(format!(
            "native installation path is not a real directory: `{}`",
            path.display()
        )))
    }
}

fn path_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io("inspect native installation path", path, error)),
    }
}

fn remove_work_tree(path: &Path, swap: &SwapInstallPlan) -> Result<()> {
    if path != swap.unpack() && path != swap.candidate() {
        return Err(Error::InvalidState(
            "refusing to remove an unrecognised native update work directory".into(),
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)
            .map_err(|error| Error::io("remove native update work directory", path, error)),
        Ok(_) => Err(Error::PlatformInstall(format!(
            "native update work path is not a real directory: `{}`",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(
            "inspect native update work directory",
            path,
            error,
        )),
    }
}

fn remove_retained_tree(path: &Path, swap: &SwapInstallPlan) -> Result<()> {
    if path != swap.previous() && path != swap.failed_candidate() {
        return Err(Error::InvalidState(
            "refusing to remove an unrecognised retained installation".into(),
        ));
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => fs::remove_dir_all(path)
            .map_err(|error| Error::io("remove retained native installation", path, error)),
        Ok(_) => Err(Error::PlatformInstall(format!(
            "retained native installation is not a real directory: `{}`",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::io(
            "inspect retained native installation",
            path,
            error,
        )),
    }
}

impl SwapInstallPlan {
    fn unpack(&self) -> &Path {
        &self.unpack
    }

    fn candidate(&self) -> &Path {
        &self.candidate
    }
}

fn truncate_failure(failure: &mut String) {
    const SUFFIX: &str = "...[truncated]";
    if failure.len() <= MAX_FAILURE_BYTES {
        return;
    }
    let mut end = MAX_FAILURE_BYTES - SUFFIX.len();
    while !failure.is_char_boundary(end) {
        end -= 1;
    }
    failure.truncate(end);
    failure.push_str(SUFFIX);
}

struct HandoffFile {
    path: PathBuf,
    _lock: File,
}

impl HandoffFile {
    fn create(path: PathBuf) -> Result<Self> {
        let parent = fsutil::parent(&path)?;
        fs::create_dir_all(parent)
            .map_err(|error| Error::io("create native handoff directory", parent, error))?;
        let file = Self::lock(path)?;
        fsutil::recover_atomic_write(&file.path)?;
        match fs::symlink_metadata(&file.path) {
            Ok(_) => {
                fsutil::ensure_regular_file(&file.path)?;
                let state = Self::read_state(&file.path)?;
                if state.phase() != PlatformInstallPhase::Accepted {
                    return Err(Error::DestinationExists(file.path));
                }
                fsutil::remove_file_if_present(&file.path)?;
                fsutil::remove_file_if_present(&fsutil::backup_path(&file.path)?)?;
                fsutil::sync_parent(&file.path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Error::io("inspect native handoff state", &file.path, error));
            }
        }
        Ok(file)
    }

    fn open(path: impl Into<PathBuf>) -> Result<(Self, PlatformInstallState)> {
        let path = fsutil::absolute(&path.into(), "resolve native handoff path")?;
        let file = Self::lock(path)?;
        fsutil::recover_atomic_write(&file.path)?;
        fsutil::ensure_regular_file(&file.path)?;
        let state = Self::read_state(&file.path)?;
        Ok((file, state))
    }

    fn lock(path: PathBuf) -> Result<Self> {
        let name = path.file_name().ok_or_else(|| {
            Error::InvalidState(format!("`{}` has no handoff file name", path.display()))
        })?;
        let mut lock_name = OsString::from(name);
        lock_name.push(".lock");
        let lock_path = fsutil::parent(&path)?.join(lock_name);
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| Error::io("open native handoff lock", &lock_path, error))?;
        fsutil::ensure_regular_file(&lock_path)?;
        match lock.try_lock() {
            Ok(()) => Ok(Self { path, _lock: lock }),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::StateLocked(path)),
            Err(std::fs::TryLockError::Error(error)) => {
                Err(Error::io("lock native handoff state", lock_path, error))
            }
        }
    }

    fn persist(&self, state: &PlatformInstallState) -> Result<()> {
        state.validate()?;
        let mut bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| Error::json("native handoff", error))?;
        bytes.push(b'\n');
        fsutil::atomic_write(&self.path, &bytes)
    }

    fn read_state(path: &Path) -> Result<PlatformInstallState> {
        let bytes =
            fs::read(path).map_err(|error| Error::io("read native handoff state", path, error))?;
        let state: PlatformInstallState =
            serde_json::from_slice(&bytes).map_err(|error| Error::json("native handoff", error))?;
        state.validate()?;
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use semver::Version;
    use serde_json::json;

    use super::*;
    use crate::{
        ManifestVerification,
        test_support::{
            ARTIFACT_URL, CANDIDATE_BYTES, ScratchDir, ring, sha256_hex, signed_envelope,
            signing_key,
        },
        verify_manifest,
    };

    struct FakeAdapter {
        installs: Cell<u32>,
        rollbacks: Cell<u32>,
        accepts: Cell<u32>,
        fail_install: bool,
    }

    impl PlatformInstallAdapter for FakeAdapter {
        fn install(&self, _state: &PlatformInstallState) -> Result<()> {
            self.installs.set(self.installs.get() + 1);
            if self.fail_install {
                Err(Error::PlatformInstall("simulated install failure".into()))
            } else {
                Ok(())
            }
        }

        fn rollback(&self, _state: &PlatformInstallState) -> Result<()> {
            self.rollbacks.set(self.rollbacks.get() + 1);
            Ok(())
        }

        fn accept(&self, _state: &PlatformInstallState) -> Result<()> {
            self.accepts.set(self.accepts.get() + 1);
            Ok(())
        }
    }

    fn staged_linux_artifact(scratch: &ScratchDir) -> StagedArtifact {
        let key = signing_key(42);
        let manifest_bytes = serde_json::to_vec(&json!({
            "schema": 1,
            "generated": 42,
            "version": "2.0.0",
            "artifacts": {
                "linux-x86_64-appdir": {
                    "platform": "linux-x86_64",
                    "kind": "linux-appdir-tar-gz",
                    "url": ARTIFACT_URL,
                    "sha256": sha256_hex(CANDIDATE_BYTES),
                    "size": CANDIDATE_BYTES.len(),
                }
            }
        }))
        .unwrap();
        let signature = signed_envelope(&manifest_bytes, "platform-test", &key);
        let ManifestVerification::Update(manifest) = verify_manifest(
            &manifest_bytes,
            &signature,
            &ring(&[("platform-test", &key)]),
            &Version::new(1, 0, 0),
            0,
        )
        .unwrap() else {
            panic!("newer manifest must produce an update");
        };
        let artifact = manifest
            .artifact_for_kind("linux-x86_64", ArtifactKind::LinuxAppdirTarGz)
            .unwrap();
        let path = scratch.path().join("candidate.tar.gz");
        fs::write(&path, CANDIDATE_BYTES).unwrap();
        StagedArtifact { path, artifact }
    }

    fn create_live_appdir(path: &Path) {
        fs::create_dir_all(path.join("usr/bin")).unwrap();
        fs::write(path.join("usr/bin/scrozz"), b"installed").unwrap();
        fs::write(path.join("scrozz.desktop"), b"[Desktop Entry]\n").unwrap();
    }

    #[test]
    fn derived_swap_paths_are_distinct_siblings() {
        let installed = std::env::temp_dir().join("Scrozz.AppDir");
        let plan = SwapInstallPlan::new(&installed).unwrap();
        assert_eq!(plan.installed(), installed);
        assert_eq!(
            plan.previous().file_name().unwrap(),
            ".Scrozz.AppDir.scrozz-previous"
        );
        assert_eq!(
            plan.failed_candidate().file_name().unwrap(),
            ".Scrozz.AppDir.scrozz-failed"
        );
        plan.validate().unwrap();
    }

    #[test]
    fn target_kinds_and_rollback_contract_are_explicit() {
        let root = std::env::temp_dir();
        let mac = PlatformInstallTarget::macos_app(root.join("Scrozz.app")).unwrap();
        let portable = PlatformInstallTarget::windows_portable(root.join("Scrozz")).unwrap();
        let linux = PlatformInstallTarget::linux_appdir(root.join("Scrozz.AppDir")).unwrap();
        let msix = PlatformInstallTarget::windows_msix("com.thatcube.Scrozz", None).unwrap();

        assert_eq!(mac.artifact_kind(), ArtifactKind::MacosAppZip);
        assert_eq!(portable.artifact_kind(), ArtifactKind::WindowsPortableZip);
        assert_eq!(linux.artifact_kind(), ArtifactKind::LinuxAppdirTarGz);
        assert_eq!(msix.artifact_kind(), ArtifactKind::WindowsMsix);
        assert!(mac.rollback_available());
        assert!(!msix.rollback_available());
    }

    #[test]
    fn msix_identity_is_restricted_before_it_reaches_powershell() {
        let result = PlatformInstallTarget::windows_msix("bad;Write-Host owned", None);
        assert!(matches!(result, Err(Error::InvalidState(_))));
        let result = PlatformInstallTarget::windows_msix(
            "com.thatcube.Scrozz",
            Some(std::env::temp_dir().join("rollback.zip")),
        );
        assert!(matches!(result, Err(Error::InvalidState(_))));
    }

    #[test]
    fn native_swap_targets_are_restricted_to_expected_layouts() {
        let root = std::env::temp_dir();
        assert!(PlatformInstallTarget::macos_app(root.join("Other.app")).is_err());
        assert!(PlatformInstallTarget::linux_appdir(root.join("scrozz")).is_err());
    }

    #[test]
    fn handoff_transitions_are_durable_and_never_apply_during_begin() {
        let scratch = ScratchDir::new("platform-handoff");
        let installed = scratch.path().join("Scrozz.AppDir");
        create_live_appdir(&installed);
        let staged = staged_linux_artifact(&scratch);
        let target = PlatformInstallTarget::linux_appdir(&installed).unwrap();
        let path = scratch.path().join("handoff.json");
        let mut handoff = PlatformHandoff::begin(&path, &staged, target).unwrap();
        let adapter = FakeAdapter {
            installs: Cell::new(0),
            rollbacks: Cell::new(0),
            accepts: Cell::new(0),
            fail_install: false,
        };

        assert_eq!(handoff.state().phase(), PlatformInstallPhase::Prepared);
        assert_eq!(adapter.installs.get(), 0);
        handoff.apply(&adapter).unwrap();
        assert_eq!(adapter.installs.get(), 1);
        assert_eq!(handoff.state().phase(), PlatformInstallPhase::Installed);
        drop(handoff);

        let mut reopened = PlatformHandoff::open(&path).unwrap();
        assert_eq!(reopened.state().phase(), PlatformInstallPhase::Installed);
        reopened.rollback(&adapter).unwrap();
        assert_eq!(adapter.rollbacks.get(), 1);
        assert_eq!(reopened.state().phase(), PlatformInstallPhase::RolledBack);
        reopened.accept(&adapter).unwrap();
        assert_eq!(adapter.accepts.get(), 1);
        assert_eq!(reopened.state().phase(), PlatformInstallPhase::Accepted);
        reopened.accept(&adapter).unwrap();
        assert_eq!(adapter.accepts.get(), 1);
    }

    #[test]
    fn failed_native_install_is_persisted_without_success_shape() {
        let scratch = ScratchDir::new("platform-failure");
        let installed = scratch.path().join("Scrozz.AppDir");
        create_live_appdir(&installed);
        let staged = staged_linux_artifact(&scratch);
        let target = PlatformInstallTarget::linux_appdir(installed).unwrap();
        let path = scratch.path().join("handoff.json");
        let mut handoff = PlatformHandoff::begin(&path, &staged, target).unwrap();
        let adapter = FakeAdapter {
            installs: Cell::new(0),
            rollbacks: Cell::new(0),
            accepts: Cell::new(0),
            fail_install: true,
        };

        assert!(handoff.apply(&adapter).is_err());
        assert_eq!(handoff.state().phase(), PlatformInstallPhase::Failed);
        assert_eq!(
            handoff.state().failure(),
            Some("native update installation failed: simulated install failure")
        );
        drop(handoff);
        let reopened = PlatformHandoff::open(path).unwrap();
        assert_eq!(reopened.state().phase(), PlatformInstallPhase::Failed);
    }

    #[test]
    fn rollback_quarantines_an_interrupted_candidate_and_partial_unpack() {
        let scratch = ScratchDir::new("platform-rollback-candidate");
        let installed = scratch.path().join("Scrozz.AppDir");
        create_live_appdir(&installed);
        let plan = SwapInstallPlan::new(installed).unwrap();
        fs::create_dir(&plan.candidate).unwrap();
        fs::create_dir(&plan.unpack).unwrap();

        rollback_swap(&plan).unwrap();

        assert!(plan.installed.exists());
        assert!(plan.failed_candidate.exists());
        assert!(!plan.candidate.exists());
        assert!(!plan.unpack.exists());
    }

    #[test]
    fn rollback_recovers_after_the_live_directory_was_already_moved() {
        let scratch = ScratchDir::new("platform-rollback-move");
        let installed = scratch.path().join("Scrozz.AppDir");
        let plan = SwapInstallPlan::new(&installed).unwrap();
        create_live_appdir(&plan.previous);
        fs::create_dir(&plan.failed_candidate).unwrap();
        fs::create_dir(&plan.candidate).unwrap();

        rollback_swap(&plan).unwrap();

        assert!(plan.installed.exists());
        assert!(!plan.previous.exists());
        assert!(plan.failed_candidate.exists());
        assert!(!plan.candidate.exists());
    }

    #[test]
    fn rollback_preserves_candidates_when_no_known_good_copy_exists() {
        let scratch = ScratchDir::new("platform-rollback-no-good-copy");
        let installed = scratch.path().join("Scrozz.AppDir");
        let plan = SwapInstallPlan::new(installed).unwrap();
        fs::create_dir(&plan.failed_candidate).unwrap();
        fs::create_dir(&plan.candidate).unwrap();

        assert!(matches!(rollback_swap(&plan), Err(Error::Recovery(_))));
        assert!(plan.failed_candidate.exists());
        assert!(plan.candidate.exists());
    }
}
