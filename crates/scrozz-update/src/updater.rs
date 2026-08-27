use std::{
    fs::{self, File},
    io,
    path::{Path, PathBuf},
};

use semver::Version;

use crate::{
    ArtifactKind, ArtifactMetadata, CandidateMetadata, CurlFetcher, Error, FetchRequest, Fetcher,
    HttpsUrl, InstallPlan, ManifestVerification, Phase, PinnedKeyRing, ResolvedChannel, Result,
    StagedArtifact, UpdateChannel, UpdateCheckRecord, UpdateCheckResult, UpdateState,
    VerifiedArtifact, VerifiedDownload, VerifiedManifest, fsutil, state::StateFile,
    verify_artifact_file, verify_manifest,
};

const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SIGNATURE_ENVELOPE_BYTES: u64 = 16 * 1024;

/// A newer candidate plus its signed artifact capability.
///
/// Instances come from [`Updater::check`] or from a previously persisted
/// successful check through [`Updater::available_update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedUpdate {
    candidate: CandidateMetadata,
    artifact: VerifiedArtifact,
}

impl VerifiedUpdate {
    fn from_manifest(
        manifest: &VerifiedManifest,
        artifact: VerifiedArtifact,
        channel: Option<UpdateChannel>,
    ) -> Self {
        Self {
            candidate: CandidateMetadata::new(
                manifest.version().clone(),
                manifest.generated(),
                channel,
            ),
            artifact,
        }
    }

    fn from_persisted(candidate: CandidateMetadata, artifact: ArtifactMetadata) -> Self {
        Self {
            candidate,
            artifact: VerifiedArtifact::from_persisted(artifact),
        }
    }

    /// Returns the candidate semantic version.
    #[must_use]
    pub fn version(&self) -> &Version {
        self.candidate.version()
    }

    /// Returns the signed monotonic generation.
    #[must_use]
    pub const fn generated(&self) -> u64 {
        self.candidate.generated()
    }

    /// Returns the current-platform artifact capability.
    #[must_use]
    pub fn artifact(&self) -> &VerifiedArtifact {
        &self.artifact
    }

    /// Returns the endpoint-catalog channel that produced this update.
    ///
    /// `None` identifies checks made through [`Updater::check`] with raw URLs.
    #[must_use]
    pub const fn channel(&self) -> Option<UpdateChannel> {
        self.candidate.channel()
    }
}

/// Outcome of a signed update check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// The signed manifest version equals the installed version.
    Current {
        /// The version shared by the installation and manifest.
        version: Version,
        /// The signed generation, which is not accepted as a new candidate.
        generated: u64,
    },
    /// A newer signed manifest did not publish this build's platform key.
    PlatformUnavailable {
        /// The newer semantic version.
        version: Version,
        /// The accepted monotonic generation.
        generated: u64,
        /// The exact platform key that was absent.
        platform: String,
    },
    /// A newer signed candidate is available and usable on this platform.
    UpdateAvailable(VerifiedUpdate),
}

/// Persistent signed-update coordinator.
///
/// The coordinator owns one state file and one transport. It never installs
/// during [`Self::check`] or [`Self::download`]; staging and installation each
/// require their own explicit method call.
pub struct Updater<F = CurlFetcher> {
    fetcher: F,
    keys: PinnedKeyRing,
    state_file: StateFile,
    state: UpdateState,
}

/// Signed update checker with no download, staging, recovery, installation, or
/// rollback authority.
///
/// Unlike [`Updater::open`], opening this type never reconciles durable install
/// state. It therefore cannot turn an automatic check into an installation
/// after a prior process interruption.
pub struct UpdateChecker<F = CurlFetcher> {
    updater: Updater<F>,
}

impl UpdateChecker<CurlFetcher> {
    /// Opens check-only state with the default bounded curl transport.
    ///
    /// # Errors
    ///
    /// Returns an error when state cannot be read or is in a phase that needs
    /// explicit full-updater recovery.
    pub fn open(state_path: impl Into<PathBuf>, keys: PinnedKeyRing) -> Result<Self> {
        Self::with_fetcher(state_path, keys, CurlFetcher::new())
    }

    /// Opens with the intentionally empty production key ring.
    ///
    /// # Errors
    ///
    /// Returns an error when state cannot be read or is in a phase that needs
    /// explicit full-updater recovery.
    pub fn open_with_production_keys(state_path: impl Into<PathBuf>) -> Result<Self> {
        Self::open(state_path, PinnedKeyRing::production())
    }
}

impl<F: Fetcher> UpdateChecker<F> {
    /// Opens check-only state with a caller-provided transport.
    ///
    /// Only idle state, a pending checked update, and a failed check without
    /// candidate or install metadata are accepted. No recovery runs here.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PassiveCheckUnavailable`] for any lifecycle that could
    /// carry download or installation authority.
    pub fn with_fetcher(
        state_path: impl Into<PathBuf>,
        keys: PinnedKeyRing,
        fetcher: F,
    ) -> Result<Self> {
        let (state_file, state) = StateFile::open(state_path)?;
        if !check_only_state_is_safe(&state) {
            return Err(Error::PassiveCheckUnavailable(state.phase()));
        }
        Ok(Self {
            updater: Updater {
                fetcher,
                keys,
                state_file,
                state,
            },
        })
    }

    /// Returns the current durable state snapshot.
    #[must_use]
    pub fn state(&self) -> &UpdateState {
        self.updater.state()
    }

    /// Checks one resolved channel without acquiring installation authority.
    ///
    /// A previously verified pending candidate is returned without networking.
    /// A transport or verification failure may be retried on the same checker;
    /// its failed check state is safely reset before the next attempt.
    ///
    /// # Errors
    ///
    /// Returns a fetch, verification, state, or persistence error. It never
    /// downloads an artifact and never recovers or installs prior staged state.
    pub fn check(
        &mut self,
        channel: &ResolvedChannel,
        installed_version: &Version,
    ) -> Result<CheckOutcome> {
        self.check_for_kind(channel, installed_version, ArtifactKind::RawExecutable)
    }

    /// Checks one resolved channel for an explicit native distribution kind.
    ///
    /// This is the selection boundary for manifests that publish both portable
    /// and package-identity artifacts for the same OS and architecture.
    ///
    /// # Errors
    ///
    /// Returns the same bounded fetch, verification, and persistence errors as
    /// [`Self::check`].
    pub fn check_for_kind(
        &mut self,
        channel: &ResolvedChannel,
        installed_version: &Version,
        kind: ArtifactKind,
    ) -> Result<CheckOutcome> {
        let platform = scrozz_core::identity::platform_key();
        if let Some(update) = self.updater.available_update() {
            if update.channel() == Some(channel.channel())
                && update.version() > installed_version
                && update.artifact().metadata().kind() == kind
                && update.artifact().metadata().platform() == platform.as_str()
            {
                let mut checked = self.updater.state.clone();
                checked.last_check = Some(UpdateCheckRecord::success(
                    Some(channel.channel()),
                    UpdateCheckResult::UpdateAvailable,
                    update.version().clone(),
                    update.generated(),
                    Some(platform),
                ));
                self.updater.commit(checked)?;
                return Ok(CheckOutcome::UpdateAvailable(update));
            }
            self.updater.reset_to_idle()?;
        }
        if self.updater.state.phase == Phase::Failed {
            if !check_failure_is_safe(&self.updater.state) {
                return Err(Error::PassiveCheckUnavailable(self.updater.state.phase()));
            }
            self.updater.reset_to_idle()?;
        }
        self.updater.check_inner(
            channel.endpoints().manifest().as_str(),
            channel.endpoints().signature().as_str(),
            installed_version,
            Some(channel.channel()),
            kind,
        )
    }
}

impl Updater<CurlFetcher> {
    /// Opens persistent state with the default subprocess curl transport.
    ///
    /// Interrupted checks and downloads become durable failures. An
    /// `AwaitingRestart` installation is reconciled deterministically.
    ///
    /// # Errors
    ///
    /// Returns an error if state cannot be read, validated, persisted, or
    /// safely recovered.
    pub fn open(state_path: impl Into<PathBuf>, keys: PinnedKeyRing) -> Result<Self> {
        Self::with_fetcher(state_path, keys, CurlFetcher::new())
    }

    /// Opens with the intentionally empty production key ring.
    ///
    /// Checks remain fail-closed with [`Error::NoPinnedKeys`] until a human-held
    /// production key has been created and its public half deliberately pinned.
    ///
    /// # Errors
    ///
    /// Returns an error if state cannot be opened or recovered.
    pub fn open_with_production_keys(state_path: impl Into<PathBuf>) -> Result<Self> {
        Self::open(state_path, PinnedKeyRing::production())
    }
}

impl<F: Fetcher> Updater<F> {
    /// Opens persistent state with a caller-provided transport.
    ///
    /// # Errors
    ///
    /// Returns an error if state cannot be read, validated, persisted, or
    /// safely recovered.
    pub fn with_fetcher(
        state_path: impl Into<PathBuf>,
        keys: PinnedKeyRing,
        fetcher: F,
    ) -> Result<Self> {
        let (state_file, state) = StateFile::open(state_path)?;
        let mut updater = Self {
            fetcher,
            keys,
            state_file,
            state,
        };
        updater.recover_after_open()?;
        Ok(updater)
    }

    /// Returns the current durable state snapshot.
    #[must_use]
    pub fn state(&self) -> &UpdateState {
        &self.state
    }

    /// Returns the state document path.
    #[must_use]
    pub fn state_path(&self) -> &Path {
        self.state_file.path()
    }

    /// Reconstructs the accepted update capability after a process restart.
    ///
    /// The capability is available only while the state still carries signed
    /// candidate metadata and has not begun installation.
    #[must_use]
    pub fn available_update(&self) -> Option<VerifiedUpdate> {
        if !matches!(
            self.state.phase,
            Phase::UpdateAvailable | Phase::Downloading | Phase::Downloaded | Phase::Failed
        ) || self.state.install_plan.is_some()
        {
            return None;
        }
        Some(VerifiedUpdate::from_persisted(
            self.state.candidate.clone()?,
            self.state.artifact.clone()?,
        ))
    }

    /// Returns a failed pre-install candidate to `UpdateAvailable` for retry.
    ///
    /// Existing caller-chosen download or staging files are not deleted; their
    /// paths are simply no longer part of the new attempt. The accepted
    /// generation watermark and signed candidate metadata are retained.
    ///
    /// # Errors
    ///
    /// Returns an invalid transition if there is no retained signed candidate
    /// or installation has already begun, or a cleanup/persistence error.
    pub fn retry_available_update(&mut self) -> Result<VerifiedUpdate> {
        if self.state.phase != Phase::Failed || self.state.install_plan.is_some() {
            return Err(Error::InvalidTransition {
                from: self.state.phase,
                to: Phase::UpdateAvailable,
            });
        }
        let candidate = self
            .state
            .candidate
            .clone()
            .ok_or(Error::VerifiedUpdateMismatch)?;
        let artifact = self
            .state
            .artifact
            .clone()
            .ok_or(Error::VerifiedUpdateMismatch)?;
        if let Some(error) = cleanup_files(self.state.transient_paths.iter()) {
            return Err(error);
        }

        let update = VerifiedUpdate::from_persisted(candidate.clone(), artifact.clone());
        let mut available = self.state.clone();
        available.phase = Phase::UpdateAvailable;
        available.candidate = Some(candidate);
        available.artifact = Some(artifact);
        available.downloaded_path = None;
        available.staged_path = None;
        available.transient_paths.clear();
        available.failure = None;
        self.commit(available)?;
        Ok(update)
    }

    /// Abandons a pre-install or terminal lifecycle while preserving its
    /// generation watermark.
    ///
    /// This does not delete any caller-selected download, staging, previous, or
    /// failed-candidate file. A failed installation with a swap plan must be
    /// rolled back instead. For a failed accepted candidate, prefer
    /// [`Self::retry_available_update`] because checking the same generation
    /// again is correctly treated as a replay.
    ///
    /// # Errors
    ///
    /// Returns an invalid transition after an installation has been requested, or
    /// [`Error::Recovery`] when abandoning an installation would be unsafe.
    pub fn reset_to_idle(&mut self) -> Result<()> {
        if !matches!(
            self.state.phase,
            Phase::UpdateAvailable
                | Phase::Downloading
                | Phase::Downloaded
                | Phase::Staged
                | Phase::Installed
                | Phase::Failed
                | Phase::RolledBack
        ) {
            return Err(Error::InvalidTransition {
                from: self.state.phase,
                to: Phase::Idle,
            });
        }
        if self.state.rollback_requested
            || (self.state.phase == Phase::Failed && self.state.install_plan.is_some())
        {
            return Err(Error::Recovery(
                "an installation plan must be recovered or rolled back before reset".into(),
            ));
        }
        if let Some(error) = cleanup_files(self.state.transient_paths.iter()) {
            return Err(error);
        }

        let mut idle = self.state.clone();
        idle.phase = Phase::Idle;
        clear_active_lifecycle(&mut idle);
        self.commit(idle)
    }

    /// Fetches, exact-byte verifies, and evaluates a detached signed manifest.
    ///
    /// A successful newer candidate advances the persisted generation
    /// watermark. This call never downloads an artifact and never installs.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid transition, URL, transport, signature,
    /// manifest, rollback, replay, temporary file, or state persistence failure.
    pub fn check(
        &mut self,
        manifest_url: impl Into<String>,
        signature_url: impl Into<String>,
        installed_version: &Version,
    ) -> Result<CheckOutcome> {
        self.check_for_kind(
            manifest_url,
            signature_url,
            installed_version,
            ArtifactKind::RawExecutable,
        )
    }

    /// Checks raw endpoints for an explicit native distribution kind.
    ///
    /// # Errors
    ///
    /// Returns the same bounded fetch, signature, manifest, anti-rollback, and
    /// persistence errors as [`Self::check`].
    pub fn check_for_kind(
        &mut self,
        manifest_url: impl Into<String>,
        signature_url: impl Into<String>,
        installed_version: &Version,
        kind: ArtifactKind,
    ) -> Result<CheckOutcome> {
        self.check_inner(manifest_url, signature_url, installed_version, None, kind)
    }

    fn check_inner(
        &mut self,
        manifest_url: impl Into<String>,
        signature_url: impl Into<String>,
        installed_version: &Version,
        channel: Option<UpdateChannel>,
        kind: ArtifactKind,
    ) -> Result<CheckOutcome> {
        self.require_transition(Phase::Checking)?;
        if self.keys.is_empty() {
            return Err(Error::NoPinnedKeys);
        }
        let manifest_url = HttpsUrl::parse(manifest_url)?;
        let signature_url = HttpsUrl::parse(signature_url)?;
        let manifest_request = FetchRequest::for_scrozz(manifest_url, MAX_MANIFEST_BYTES)?;
        let signature_request =
            FetchRequest::for_scrozz(signature_url, MAX_SIGNATURE_ENVELOPE_BYTES)?;
        let state_directory = fsutil::parent(self.state_file.path())?;

        let manifest_reserved = fsutil::reserve_temp(state_directory, "manifest")?;
        let (manifest_temp, mut manifest_file) = manifest_reserved.into_parts();
        let signature_reserved = match fsutil::reserve_temp(state_directory, "signature") {
            Ok(reserved) => reserved,
            Err(error) => {
                drop(manifest_file);
                let _ = fsutil::remove_file_if_present(&manifest_temp);
                return Err(error);
            }
        };
        let (signature_temp, mut signature_file) = signature_reserved.into_parts();

        let mut checking = self.state.clone();
        checking.phase = Phase::Checking;
        clear_active_lifecycle(&mut checking);
        checking.transient_paths = vec![manifest_temp.clone(), signature_temp.clone()];
        if let Err(error) = self.commit(checking) {
            let _ = cleanup_files([&manifest_temp, &signature_temp]);
            return Err(error);
        }

        let fetched = (|| {
            self.fetcher.fetch(&manifest_request, &mut manifest_file)?;
            manifest_file
                .sync_all()
                .map_err(|error| Error::io("sync fetched manifest", &manifest_temp, error))?;
            fsutil::ensure_regular_file(&manifest_temp)?;
            self.fetcher
                .fetch(&signature_request, &mut signature_file)?;
            signature_file
                .sync_all()
                .map_err(|error| Error::io("sync fetched signature", &signature_temp, error))?;
            fsutil::ensure_regular_file(&signature_temp)?;
            let manifest_bytes = fs::read(&manifest_temp)
                .map_err(|error| Error::io("read fetched manifest", &manifest_temp, error))?;
            let signature_bytes = fs::read(&signature_temp)
                .map_err(|error| Error::io("read fetched signature", &signature_temp, error))?;
            Ok((manifest_bytes, signature_bytes))
        })();

        drop(manifest_file);
        drop(signature_file);
        let cleanup_error = cleanup_files([&manifest_temp, &signature_temp]);
        let (manifest_bytes, signature_bytes) = match fetched {
            Ok(bytes) if cleanup_error.is_none() => bytes,
            Ok(_) => {
                return self.record_check_failure(cleanup_error.expect("checked above"), channel);
            }
            Err(error) => return self.record_check_failure(error, channel),
        };

        let highest_accepted_generation = channel
            .map_or(self.state.highest_accepted_generation, |channel| {
                self.state.highest_accepted_generation_for(channel)
            });
        let verification = match verify_manifest(
            &manifest_bytes,
            &signature_bytes,
            &self.keys,
            installed_version,
            highest_accepted_generation,
        ) {
            Ok(verification) => verification,
            Err(error) => return self.record_check_failure(error, channel),
        };

        match verification {
            ManifestVerification::Current { version, generated } => {
                let mut idle = self.state.clone();
                idle.phase = Phase::Idle;
                clear_active_lifecycle(&mut idle);
                accept_generation(&mut idle, channel, generated);
                idle.last_check = Some(UpdateCheckRecord::success(
                    channel,
                    UpdateCheckResult::Current,
                    version.clone(),
                    generated,
                    None,
                ));
                self.commit(idle)?;
                Ok(CheckOutcome::Current { version, generated })
            }
            ManifestVerification::Update(manifest) => {
                let platform = scrozz_core::identity::platform_key();
                let artifact = manifest.artifact_for_kind(&platform, kind);
                let mut next = self.state.clone();
                accept_generation(&mut next, channel, manifest.generated());
                next.transient_paths.clear();
                next.failure = None;
                match artifact {
                    Some(artifact) => {
                        let update =
                            VerifiedUpdate::from_manifest(&manifest, artifact.clone(), channel);
                        next.phase = Phase::UpdateAvailable;
                        next.candidate = Some(update.candidate.clone());
                        next.artifact = Some(artifact.metadata().clone());
                        next.last_check = Some(UpdateCheckRecord::success(
                            channel,
                            UpdateCheckResult::UpdateAvailable,
                            manifest.version().clone(),
                            manifest.generated(),
                            Some(artifact.metadata().platform().to_owned()),
                        ));
                        self.commit(next)?;
                        Ok(CheckOutcome::UpdateAvailable(update))
                    }
                    None => {
                        next.phase = Phase::Idle;
                        clear_active_lifecycle(&mut next);
                        let outcome = CheckOutcome::PlatformUnavailable {
                            version: manifest.version().clone(),
                            generated: manifest.generated(),
                            platform: platform.clone(),
                        };
                        next.last_check = Some(UpdateCheckRecord::success(
                            channel,
                            UpdateCheckResult::PlatformUnavailable,
                            manifest.version().clone(),
                            manifest.generated(),
                            Some(platform),
                        ));
                        self.commit(next)?;
                        Ok(outcome)
                    }
                }
            }
        }
    }

    /// Downloads one accepted artifact to a caller-selected destination.
    ///
    /// Bytes first land in a reserved sibling temporary file. The temporary is
    /// size-checked, SHA-256 checked, synced, and only then atomically renamed
    /// to `destination`. A failed temporary is deleted. This call never stages
    /// or installs.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale capability, invalid transition, occupied or
    /// non-file path, fetch failure, verification mismatch, or persistence
    /// failure.
    pub fn download(
        &mut self,
        update: &VerifiedUpdate,
        destination: impl Into<PathBuf>,
    ) -> Result<VerifiedDownload> {
        self.require_transition(Phase::Downloading)?;
        self.ensure_update_matches(update)?;
        let destination = destination.into();
        let destination = fsutil::absolute(&destination, "resolve update download path")?;
        fsutil::ensure_absent(&destination)?;
        let directory = fsutil::parent(&destination)?;
        let reserved = fsutil::reserve_temp(directory, "download")?;
        let (temporary, mut temporary_file) = reserved.into_parts();
        let request = FetchRequest::for_scrozz(
            update.artifact.metadata().url().clone(),
            update.artifact.metadata().size(),
        )?;

        let mut downloading = self.state.clone();
        downloading.phase = Phase::Downloading;
        downloading.downloaded_path = Some(destination.clone());
        downloading.transient_paths = vec![temporary.clone()];
        downloading.failure = None;
        if let Err(error) = self.commit(downloading) {
            drop(temporary_file);
            let _ = fsutil::remove_file_if_present(&temporary);
            return Err(error);
        }

        let downloaded = (|| {
            self.fetcher.fetch(&request, &mut temporary_file)?;
            temporary_file
                .sync_all()
                .map_err(|error| Error::io("sync downloaded artifact", &temporary, error))?;
            drop(temporary_file);
            fsutil::ensure_regular_file(&temporary)?;
            verify_artifact_file(&temporary, &update.artifact)?;
            fsutil::ensure_absent(&destination)?;
            fsutil::rename_synced(&temporary, &destination)
        })();
        if let Err(error) = downloaded {
            let _ = fsutil::remove_file_if_present(&temporary);
            return self.record_failure(error);
        }
        if let Err(error) = verify_artifact_file(&destination, &update.artifact) {
            let _ = fsutil::remove_file_if_present(&destination);
            return self.record_failure(error);
        }

        let mut complete = self.state.clone();
        complete.phase = Phase::Downloaded;
        complete.transient_paths.clear();
        complete.failure = None;
        self.commit(complete)?;
        Ok(VerifiedDownload {
            path: destination,
            artifact: update.artifact.clone(),
        })
    }

    /// Revalidates and reconstructs a downloaded-artifact token after restart.
    ///
    /// # Errors
    ///
    /// Returns an invalid transition, inconsistent state, I/O error, or
    /// artifact mismatch.
    pub fn downloaded_artifact(&self) -> Result<VerifiedDownload> {
        if self.state.phase != Phase::Downloaded {
            return Err(Error::InvalidTransition {
                from: self.state.phase,
                to: Phase::Staged,
            });
        }
        let path = self
            .state
            .downloaded_path
            .clone()
            .ok_or_else(|| Error::InvalidState("download path is absent".into()))?;
        let artifact = VerifiedArtifact::from_persisted(
            self.state
                .artifact
                .clone()
                .ok_or_else(|| Error::InvalidState("artifact metadata is absent".into()))?,
        );
        verify_artifact_file(&path, &artifact)?;
        Ok(VerifiedDownload { path, artifact })
    }

    /// Copies a verified download to a distinct sibling staging path.
    ///
    /// The copy is reverified and synced before an atomic rename exposes the
    /// staging name. This call never mutates an installation path.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale token, invalid phase, non-sibling or
    /// occupied path, non-file input, verification mismatch, or persistence
    /// failure.
    pub fn stage(
        &mut self,
        download: &VerifiedDownload,
        staged_path: impl Into<PathBuf>,
    ) -> Result<StagedArtifact> {
        self.require_transition(Phase::Staged)?;
        self.ensure_download_matches(download)?;
        verify_artifact_file(download.path(), download.artifact())?;

        let staged_path = staged_path.into();
        let staged_path = fsutil::absolute(&staged_path, "resolve update staging path")?;
        fsutil::ensure_distinct_siblings(&[download.path(), &staged_path])?;
        fsutil::ensure_absent(&staged_path)?;
        let directory = fsutil::parent(&staged_path)?;
        let reserved = fsutil::reserve_temp(directory, "stage")?;
        let (temporary, mut temporary_file) = reserved.into_parts();

        let mut preparing = self.state.clone();
        preparing.staged_path = Some(staged_path.clone());
        preparing.transient_paths = vec![temporary.clone()];
        if let Err(error) = self.commit(preparing) {
            drop(temporary_file);
            let _ = fsutil::remove_file_if_present(&temporary);
            return Err(error);
        }

        let staged = (|| {
            let mut source = File::open(download.path()).map_err(|error| {
                Error::io(
                    "open downloaded artifact for staging",
                    download.path(),
                    error,
                )
            })?;
            io::copy(&mut source, &mut temporary_file).map_err(|error| {
                Error::io("copy artifact to staging temporary", &temporary, error)
            })?;
            let permissions = source
                .metadata()
                .map_err(|error| {
                    Error::io(
                        "read downloaded artifact permissions",
                        download.path(),
                        error,
                    )
                })?
                .permissions();
            temporary_file
                .set_permissions(permissions)
                .map_err(|error| {
                    Error::io("preserve staged artifact permissions", &temporary, error)
                })?;
            temporary_file
                .sync_all()
                .map_err(|error| Error::io("sync staged artifact", &temporary, error))?;
            drop(temporary_file);
            fsutil::ensure_regular_file(&temporary)?;
            verify_artifact_file(&temporary, download.artifact())?;
            fsutil::ensure_absent(&staged_path)?;
            fsutil::rename_synced(&temporary, &staged_path)
        })();
        if let Err(error) = staged {
            let _ = fsutil::remove_file_if_present(&temporary);
            return self.record_failure(error);
        }
        if let Err(error) = verify_artifact_file(&staged_path, download.artifact()) {
            let _ = fsutil::remove_file_if_present(&staged_path);
            return self.record_failure(error);
        }

        let mut complete = self.state.clone();
        complete.phase = Phase::Staged;
        complete.transient_paths.clear();
        complete.failure = None;
        self.commit(complete)?;
        Ok(StagedArtifact {
            path: staged_path,
            artifact: download.artifact.clone(),
        })
    }

    /// Revalidates and reconstructs a staged-artifact token after restart.
    ///
    /// # Errors
    ///
    /// Returns an invalid transition, inconsistent state, I/O error, or
    /// artifact mismatch.
    pub fn staged_artifact(&self) -> Result<StagedArtifact> {
        if self.state.phase != Phase::Staged {
            return Err(Error::InvalidTransition {
                from: self.state.phase,
                to: Phase::AwaitingRestart,
            });
        }
        let path = self
            .state
            .staged_path
            .clone()
            .ok_or_else(|| Error::InvalidState("staging path is absent".into()))?;
        let artifact = VerifiedArtifact::from_persisted(
            self.state
                .artifact
                .clone()
                .ok_or_else(|| Error::InvalidState("artifact metadata is absent".into()))?,
        );
        verify_artifact_file(&path, &artifact)?;
        Ok(StagedArtifact { path, artifact })
    }

    /// Explicitly installs a staged regular file through an atomic rename swap.
    ///
    /// `AwaitingRestart` and the full path plan are persisted before the live
    /// installation is touched. The live file is first renamed to `previous`,
    /// then the staged file to `installed`; `previous` remains after success.
    /// A crash after any step is reconciled when state is next opened.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale token, invalid phase, directory or symlink,
    /// non-sibling path, occupied retention path, verification mismatch, rename
    /// failure, or state persistence failure.
    pub fn install(&mut self, staged: &StagedArtifact, plan: InstallPlan) -> Result<()> {
        self.begin_install(staged, plan)?;
        self.perform_install(InstallPause::Never)
    }

    /// Reconciles a previously persisted `AwaitingRestart` swap immediately.
    ///
    /// Opening an updater already calls this. It is exposed so a caller can
    /// retry recovery after fixing a transient filesystem problem.
    ///
    /// # Errors
    ///
    /// Returns an invalid transition or a recovery/filesystem error. On a
    /// reconciliation error the durable phase becomes `Failed`.
    pub fn recover(&mut self) -> Result<()> {
        if self.state.phase != Phase::AwaitingRestart {
            return Err(Error::InvalidTransition {
                from: self.state.phase,
                to: Phase::Installed,
            });
        }
        match self.reconcile_install() {
            Ok(()) => Ok(()),
            Err(error) => self.record_failure(error),
        }
    }

    /// Explicitly restores the retained previous installation.
    ///
    /// A replaced live candidate is preserved at the plan's
    /// `failed_candidate` path; a candidate that never became live remains at
    /// its staging path. Neither is silently deleted. If an explicit install
    /// had not yet moved the old live file, rollback records that already-live
    /// file as the restored version without moving it.
    ///
    /// # Errors
    ///
    /// Returns an invalid transition, missing previous installation, unsafe
    /// path, occupied failed-candidate path, or filesystem/persistence error.
    pub fn rollback(&mut self) -> Result<()> {
        if !matches!(
            self.state.phase,
            Phase::AwaitingRestart | Phase::Installed | Phase::Failed
        ) {
            return Err(Error::InvalidTransition {
                from: self.state.phase,
                to: Phase::RolledBack,
            });
        }
        let plan = self
            .state
            .install_plan
            .clone()
            .ok_or(Error::NoPreviousInstallation)?;
        let previous_exists = regular_file_exists(plan.previous())?;
        let previous_is_still_live = regular_file_exists(plan.installed())?
            && !self.installed_matches_candidate(plan.installed())?;
        if !previous_exists && !previous_is_still_live {
            return Err(Error::NoPreviousInstallation);
        }
        let mut requested = self.state.clone();
        requested.rollback_requested = true;
        self.commit(requested)?;
        self.reconcile_rollback(&plan)
    }

    fn begin_install(&mut self, staged: &StagedArtifact, plan: InstallPlan) -> Result<()> {
        self.require_transition(Phase::AwaitingRestart)?;
        self.ensure_staged_matches(staged)?;
        plan.validate()?;
        fsutil::ensure_distinct_siblings(&[
            staged.path(),
            plan.installed(),
            plan.previous(),
            plan.failed_candidate(),
        ])?;
        fsutil::ensure_regular_file(staged.path())?;
        fsutil::ensure_regular_file(plan.installed())?;
        verify_artifact_file(staged.path(), staged.artifact())?;
        fsutil::ensure_absent(plan.previous())?;
        fsutil::ensure_absent(plan.failed_candidate())?;

        let mut awaiting = self.state.clone();
        awaiting.phase = Phase::AwaitingRestart;
        awaiting.install_plan = Some(plan);
        awaiting.failure = None;
        self.commit(awaiting)
    }

    fn perform_install(&mut self, pause: InstallPause) -> Result<()> {
        let plan = self
            .state
            .install_plan
            .clone()
            .ok_or_else(|| Error::InvalidState("installation plan is absent".into()))?;
        if pause == InstallPause::BeforeFirstRename {
            return Err(Error::Recovery("simulated process interruption".into()));
        }
        fsutil::rename_synced(plan.installed(), plan.previous())?;
        if pause == InstallPause::AfterFirstRename {
            return Err(Error::Recovery("simulated process interruption".into()));
        }
        let staged = self
            .state
            .staged_path
            .clone()
            .ok_or_else(|| Error::InvalidState("staging path is absent".into()))?;
        fsutil::rename_synced(&staged, plan.installed())?;
        self.require_valid_candidate(plan.installed())?;
        if pause == InstallPause::AfterSecondRename {
            return Err(Error::Recovery("simulated process interruption".into()));
        }
        self.mark_installed()
    }

    fn recover_after_open(&mut self) -> Result<()> {
        if self.state.rollback_requested {
            let plan = self
                .state
                .install_plan
                .clone()
                .ok_or_else(|| Error::InvalidState("rollback plan is absent".into()))?;
            return self.reconcile_rollback(&plan);
        }

        match self.state.phase {
            Phase::Checking => {
                let messages = cleanup_recorded_paths(&self.state.transient_paths);
                let message = interrupted_message("update check", &messages);
                self.persist_interrupted_failure(message)?;
            }
            Phase::Downloading => {
                let messages = cleanup_recorded_paths(&self.state.transient_paths);
                if !messages.is_empty() {
                    self.persist_interrupted_failure(interrupted_message(
                        "artifact download",
                        &messages,
                    ))?;
                    return Ok(());
                }

                let downloaded = self
                    .state
                    .downloaded_path
                    .clone()
                    .ok_or_else(|| Error::InvalidState("download path is absent".into()))?;
                match regular_file_exists(&downloaded).and_then(|exists| {
                    if exists {
                        self.candidate_matches(&downloaded)
                    } else {
                        Ok(false)
                    }
                }) {
                    Ok(true) => {
                        let mut complete = self.state.clone();
                        complete.phase = Phase::Downloaded;
                        complete.transient_paths.clear();
                        complete.failure = None;
                        self.commit(complete)?;
                    }
                    Ok(false) => {
                        self.persist_interrupted_failure(
                            "interrupted artifact download left no verified destination; any existing destination was left untouched"
                                .into(),
                        )?;
                    }
                    Err(error) => {
                        self.persist_interrupted_failure(format!(
                            "interrupted artifact download left its destination untouched: {error}"
                        ))?;
                    }
                }
            }
            Phase::Downloaded
                if !self.state.transient_paths.is_empty() || self.state.staged_path.is_some() =>
            {
                let messages = cleanup_recorded_paths(&self.state.transient_paths);
                if !messages.is_empty() {
                    self.persist_interrupted_failure(interrupted_message(
                        "artifact staging",
                        &messages,
                    ))?;
                    return Ok(());
                }

                let staged = self
                    .state
                    .staged_path
                    .clone()
                    .ok_or_else(|| Error::InvalidState("staging path is absent".into()))?;
                match regular_file_exists(&staged).and_then(|exists| {
                    if exists {
                        self.candidate_matches(&staged)
                    } else {
                        Ok(false)
                    }
                }) {
                    Ok(true) => {
                        let mut complete = self.state.clone();
                        complete.phase = Phase::Staged;
                        complete.transient_paths.clear();
                        complete.failure = None;
                        self.commit(complete)?;
                    }
                    Ok(false) if !staged.exists() => {
                        let mut cleaned = self.state.clone();
                        cleaned.transient_paths.clear();
                        cleaned.staged_path = None;
                        self.commit(cleaned)?;
                    }
                    Ok(false) => {
                        self.persist_interrupted_failure(
                            "interrupted artifact staging left an unverified destination untouched"
                                .into(),
                        )?;
                    }
                    Err(error) => {
                        self.persist_interrupted_failure(format!(
                            "interrupted artifact staging left its destination untouched: {error}"
                        ))?;
                    }
                }
            }
            Phase::AwaitingRestart => {
                if let Err(error) = self.reconcile_install() {
                    self.persist_interrupted_failure(error.to_string())?;
                }
            }
            _ if !self.state.transient_paths.is_empty() => {
                let messages = cleanup_recorded_paths(&self.state.transient_paths);
                if messages.is_empty() {
                    let mut cleaned = self.state.clone();
                    cleaned.transient_paths.clear();
                    self.commit(cleaned)?;
                } else {
                    self.persist_interrupted_failure(interrupted_message(
                        "temporary file cleanup",
                        &messages,
                    ))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn reconcile_install(&mut self) -> Result<()> {
        let plan = self
            .state
            .install_plan
            .clone()
            .ok_or_else(|| Error::InvalidState("installation plan is absent".into()))?;
        let staged = self
            .state
            .staged_path
            .clone()
            .ok_or_else(|| Error::InvalidState("staging path is absent".into()))?;
        let installed_exists = regular_file_exists(plan.installed())?;
        let staged_exists = regular_file_exists(&staged)?;
        let previous_exists = regular_file_exists(plan.previous())?;

        match (installed_exists, staged_exists, previous_exists) {
            (true, true, false) => {
                self.require_valid_candidate(&staged)?;
                fsutil::rename_synced(plan.installed(), plan.previous())?;
                fsutil::rename_synced(&staged, plan.installed())?;
                self.require_valid_candidate(plan.installed())?;
                self.mark_installed()
            }
            (false, true, true) => {
                if self.candidate_matches(&staged)? {
                    fsutil::rename_synced(&staged, plan.installed())?;
                    self.require_valid_candidate(plan.installed())?;
                    self.mark_installed()
                } else {
                    self.restore_previous(
                        &plan,
                        "staged candidate failed verification during recovery",
                    )
                }
            }
            (true, false, true) | (true, true, true) => {
                if self.installed_matches_candidate(plan.installed())? {
                    self.mark_installed()
                } else {
                    self.restore_previous(
                        &plan,
                        "live candidate failed verification during recovery",
                    )
                }
            }
            (false, false, true) => self.restore_previous(
                &plan,
                "candidate was absent, so recovery restored the retained previous installation",
            ),
            (true, false, false) => Err(Error::Recovery(
                "only the live file remains; it was left untouched".into(),
            )),
            (false, true, false) => Err(Error::Recovery(
                "only the staged candidate remains; it was left untouched".into(),
            )),
            (false, false, false) => Err(Error::Recovery(
                "no installation file remains to reconcile".into(),
            )),
        }
    }

    fn restore_previous(&mut self, plan: &InstallPlan, reason: &str) -> Result<()> {
        if !self.state.rollback_requested {
            let mut requested = self.state.clone();
            requested.rollback_requested = true;
            self.commit(requested)?;
        }
        fsutil::ensure_regular_file(plan.previous())?;
        if regular_file_exists(plan.installed())? {
            fsutil::ensure_absent(plan.failed_candidate())?;
            fsutil::rename_synced(plan.installed(), plan.failed_candidate())?;
        }
        fsutil::rename_synced(plan.previous(), plan.installed())?;
        let mut rolled_back = self.state.clone();
        rolled_back.phase = Phase::RolledBack;
        rolled_back.failure = Some(reason.to_owned());
        rolled_back.transient_paths.clear();
        rolled_back.rollback_requested = false;
        self.commit(rolled_back)
    }

    fn mark_installed(&mut self) -> Result<()> {
        let mut installed = self.state.clone();
        installed.phase = Phase::Installed;
        installed.failure = None;
        installed.transient_paths.clear();
        installed.rollback_requested = false;
        self.commit(installed)
    }

    fn reconcile_rollback(&mut self, plan: &InstallPlan) -> Result<()> {
        let installed_exists = regular_file_exists(plan.installed())?;
        let previous_exists = regular_file_exists(plan.previous())?;
        let failed_exists = regular_file_exists(plan.failed_candidate())?;

        if previous_exists {
            if installed_exists {
                if failed_exists {
                    return Err(Error::Recovery(
                        "both live and preserved failed candidates exist; neither was deleted"
                            .into(),
                    ));
                }
                fsutil::rename_synced(plan.installed(), plan.failed_candidate())?;
            }
            fsutil::rename_synced(plan.previous(), plan.installed())?;
            let mut rolled_back = self.state.clone();
            rolled_back.phase = Phase::RolledBack;
            rolled_back.failure =
                Some("explicit rollback restored the retained previous installation".into());
            rolled_back.transient_paths.clear();
            rolled_back.rollback_requested = false;
            return self.commit(rolled_back);
        }

        if installed_exists && !self.installed_matches_candidate(plan.installed())? {
            let mut rolled_back = self.state.clone();
            rolled_back.phase = Phase::RolledBack;
            rolled_back.failure =
                Some("explicit rollback confirmed the previous file was still live".into());
            rolled_back.transient_paths.clear();
            rolled_back.rollback_requested = false;
            return self.commit(rolled_back);
        }
        Err(Error::NoPreviousInstallation)
    }

    fn candidate_matches(&self, path: &Path) -> Result<bool> {
        let artifact = VerifiedArtifact::from_persisted(
            self.state
                .artifact
                .clone()
                .ok_or_else(|| Error::InvalidState("artifact metadata is absent".into()))?,
        );
        match verify_artifact_file(path, &artifact) {
            Ok(()) => Ok(true),
            Err(Error::ArtifactSizeMismatch { .. } | Error::ArtifactDigestMismatch { .. }) => {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    fn installed_matches_candidate(&self, path: &Path) -> Result<bool> {
        self.candidate_matches(path)
    }

    fn require_valid_candidate(&self, path: &Path) -> Result<()> {
        if self.candidate_matches(path)? {
            Ok(())
        } else {
            Err(Error::Recovery(format!(
                "candidate `{}` failed verification and was left untouched",
                path.display()
            )))
        }
    }

    fn ensure_update_matches(&self, update: &VerifiedUpdate) -> Result<()> {
        if self.state.candidate.as_ref() == Some(&update.candidate)
            && self.state.artifact.as_ref() == Some(update.artifact.metadata())
        {
            Ok(())
        } else {
            Err(Error::VerifiedUpdateMismatch)
        }
    }

    fn ensure_download_matches(&self, download: &VerifiedDownload) -> Result<()> {
        if self.state.downloaded_path.as_deref() == Some(download.path())
            && self.state.artifact.as_ref() == Some(download.artifact.metadata())
        {
            Ok(())
        } else {
            Err(Error::VerifiedUpdateMismatch)
        }
    }

    fn ensure_staged_matches(&self, staged: &StagedArtifact) -> Result<()> {
        if self.state.staged_path.as_deref() == Some(staged.path())
            && self.state.artifact.as_ref() == Some(staged.artifact.metadata())
        {
            Ok(())
        } else {
            Err(Error::VerifiedUpdateMismatch)
        }
    }

    fn require_transition(&self, next: Phase) -> Result<()> {
        if self.state.phase.can_transition_to(next) {
            Ok(())
        } else {
            Err(Error::InvalidTransition {
                from: self.state.phase,
                to: next,
            })
        }
    }

    fn commit(&mut self, next: UpdateState) -> Result<()> {
        if next.phase != self.state.phase && !self.state.phase.can_transition_to(next.phase) {
            return Err(Error::InvalidTransition {
                from: self.state.phase,
                to: next.phase,
            });
        }
        self.state_file.persist(&next)?;
        self.state = next;
        Ok(())
    }

    fn record_failure<T>(&mut self, error: Error) -> Result<T> {
        self.record_failure_with_check(error, None)
    }

    fn record_check_failure<T>(
        &mut self,
        error: Error,
        channel: Option<UpdateChannel>,
    ) -> Result<T> {
        let record = UpdateCheckRecord::failed(channel, error.to_string());
        self.record_failure_with_check(error, Some(record))
    }

    fn record_failure_with_check<T>(
        &mut self,
        error: Error,
        check: Option<UpdateCheckRecord>,
    ) -> Result<T> {
        let mut failed = self.state.clone();
        if failed.phase != Phase::Failed {
            if !failed.phase.can_transition_to(Phase::Failed) {
                return Err(error);
            }
            failed.phase = Phase::Failed;
        }
        failed.failure = Some(error.to_string());
        failed.transient_paths.clear();
        if let Some(check) = check {
            failed.last_check = Some(check);
        }
        match self.commit(failed) {
            Ok(()) => Err(error),
            Err(persistence_error) => Err(persistence_error),
        }
    }

    fn persist_interrupted_failure(&mut self, message: String) -> Result<()> {
        let mut failed = self.state.clone();
        if failed.phase != Phase::Failed {
            if !failed.phase.can_transition_to(Phase::Failed) {
                return Err(Error::InvalidTransition {
                    from: failed.phase,
                    to: Phase::Failed,
                });
            }
            failed.phase = Phase::Failed;
        }
        failed.failure = Some(message);
        failed.transient_paths.clear();
        self.commit(failed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstallPause {
    Never,
    BeforeFirstRename,
    AfterFirstRename,
    AfterSecondRename,
}

fn clear_active_lifecycle(state: &mut UpdateState) {
    state.candidate = None;
    state.artifact = None;
    state.downloaded_path = None;
    state.staged_path = None;
    state.install_plan = None;
    state.transient_paths.clear();
    state.rollback_requested = false;
    state.failure = None;
}

fn accept_generation(state: &mut UpdateState, channel: Option<UpdateChannel>, generated: u64) {
    match channel {
        Some(channel) => {
            state.channel_generations.insert(
                channel,
                state
                    .highest_accepted_generation_for(channel)
                    .max(generated),
            );
        }
        None => {
            state.highest_accepted_generation = state.highest_accepted_generation.max(generated);
        }
    }
}

fn check_only_state_is_safe(state: &UpdateState) -> bool {
    match state.phase {
        Phase::Idle => true,
        Phase::UpdateAvailable => {
            state.candidate.is_some()
                && state.artifact.is_some()
                && state.downloaded_path.is_none()
                && state.staged_path.is_none()
                && state.install_plan.is_none()
                && state.transient_paths.is_empty()
                && !state.rollback_requested
        }
        Phase::Failed => check_failure_is_safe(state),
        _ => false,
    }
}

fn check_failure_is_safe(state: &UpdateState) -> bool {
    state.candidate.is_none()
        && state.artifact.is_none()
        && state.downloaded_path.is_none()
        && state.staged_path.is_none()
        && state.install_plan.is_none()
        && state.transient_paths.is_empty()
        && !state.rollback_requested
}

fn cleanup_files<'a>(paths: impl IntoIterator<Item = &'a PathBuf>) -> Option<Error> {
    let mut first_error = None;
    for path in paths {
        if let Err(error) = fsutil::remove_file_if_present(path) {
            first_error.get_or_insert(error);
        }
    }
    first_error
}

fn cleanup_recorded_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .filter_map(|path| {
            fsutil::remove_file_if_present(path)
                .err()
                .map(|error| error.to_string())
        })
        .collect()
}

fn interrupted_message(operation: &str, cleanup_errors: &[String]) -> String {
    if cleanup_errors.is_empty() {
        format!("interrupted {operation} was recovered as a failure")
    } else {
        format!(
            "interrupted {operation} was recovered as a failure; cleanup: {}",
            cleanup_errors.join("; ")
        )
    }
}

fn regular_file_exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Err(Error::DirectoryUnsupported(path.to_path_buf())),
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(Error::NotRegularFile(path.to_path_buf())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::io("inspect update recovery path", path, error)),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::Value;

    use super::*;
    use crate::test_support::{
        ARTIFACT_URL, CANDIDATE_BYTES, FakeFetcher, FakeResponse, MANIFEST_URL, SIGNATURE_URL,
        ScratchDir, manifest_value, ring, signed_envelope, signing_key,
    };

    fn checked_updater(
        label: &str,
        artifact_response: FakeResponse,
    ) -> (
        ScratchDir,
        PinnedKeyRing,
        Updater<FakeFetcher>,
        VerifiedUpdate,
    ) {
        let scratch = ScratchDir::new(label);
        let key = signing_key(31);
        let keys = ring(&[("fixture", &key)]);
        let manifest = serde_json::to_vec_pretty(&manifest_value(
            "2.0.0",
            31,
            &scrozz_core::identity::platform_key(),
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let signature = signed_envelope(&manifest, "fixture", &key);
        let fetcher = FakeFetcher::from_responses([
            (MANIFEST_URL, FakeResponse::Bytes(manifest)),
            (SIGNATURE_URL, FakeResponse::Bytes(signature)),
            (ARTIFACT_URL, artifact_response),
        ]);
        let mut updater =
            Updater::with_fetcher(scratch.path().join("state.json"), keys.clone(), fetcher)
                .unwrap();
        let CheckOutcome::UpdateAvailable(update) = updater
            .check(MANIFEST_URL, SIGNATURE_URL, &Version::new(1, 0, 0))
            .unwrap()
        else {
            panic!("fixture must produce an update");
        };
        (scratch, keys, updater, update)
    }

    fn staged_updater(
        label: &str,
    ) -> (
        ScratchDir,
        PinnedKeyRing,
        Updater<FakeFetcher>,
        StagedArtifact,
    ) {
        let (scratch, keys, mut updater, update) =
            checked_updater(label, FakeResponse::Bytes(CANDIDATE_BYTES.to_vec()));
        let download = updater
            .download(&update, scratch.path().join("candidate.download"))
            .unwrap();
        let staged = updater
            .stage(&download, scratch.path().join("candidate.staged"))
            .unwrap();
        (scratch, keys, updater, staged)
    }

    #[test]
    fn check_and_download_do_not_touch_the_installation() {
        let (scratch, _keys, mut updater, update) = checked_updater(
            "no-auto-install",
            FakeResponse::Bytes(CANDIDATE_BYTES.to_vec()),
        );
        let live = scratch.path().join("scrozz");
        fs::write(&live, b"installed version").unwrap();

        let download = updater
            .download(&update, scratch.path().join("candidate.download"))
            .unwrap();

        assert_eq!(fs::read(&live).unwrap(), b"installed version");
        assert_eq!(fs::read(download.path()).unwrap(), CANDIDATE_BYTES);
        assert_eq!(updater.state().phase(), Phase::Downloaded);
    }

    #[test]
    fn passive_state_reads_and_check_only_open_never_recover_an_awaiting_installation() {
        let (scratch, keys, mut updater, staged) = staged_updater("passive-no-recovery");
        let live = scratch.path().join("scrozz");
        let previous = scratch.path().join("scrozz.previous");
        let failed = scratch.path().join("scrozz.failed");
        fs::write(&live, b"installed version").unwrap();
        updater
            .begin_install(
                &staged,
                InstallPlan::new(&live, &previous, &failed).unwrap(),
            )
            .unwrap();
        drop(updater);

        let inspected = crate::inspect_state(scratch.path().join("state.json")).unwrap();
        assert_eq!(inspected.phase(), Phase::AwaitingRestart);
        assert_eq!(fs::read(&live).unwrap(), b"installed version");
        assert!(staged.path().is_file());
        assert!(!previous.exists());
        assert!(!failed.exists());

        let checker = UpdateChecker::with_fetcher(
            scratch.path().join("state.json"),
            keys,
            FakeFetcher::default(),
        );

        assert!(matches!(
            checker,
            Err(Error::PassiveCheckUnavailable(Phase::AwaitingRestart))
        ));
        assert_eq!(fs::read(&live).unwrap(), b"installed version");
        assert!(staged.path().is_file());
        assert!(!previous.exists());
        assert!(!failed.exists());
    }

    #[test]
    fn check_only_checker_retries_a_failed_manifest_fetch() {
        let scratch = ScratchDir::new("passive-retry");
        let key = signing_key(32);
        let keys = ring(&[("fixture", &key)]);
        let channel = crate::EndpointCatalog::new(
            Some(crate::UpdateEndpoints::new(MANIFEST_URL, SIGNATURE_URL).unwrap()),
            None,
        )
        .resolve(UpdateChannel::Stable)
        .unwrap();
        let mut checker = UpdateChecker::with_fetcher(
            scratch.path().join("state.json"),
            keys,
            FakeFetcher::from_responses([(
                MANIFEST_URL,
                FakeResponse::PartialFailure(b"partial".to_vec()),
            )]),
        )
        .unwrap();

        assert!(checker.check(&channel, &Version::new(1, 0, 0)).is_err());
        assert_eq!(checker.state().phase(), Phase::Failed);
        assert!(check_failure_is_safe(checker.state()));
        let failed_check = checker.state().last_check().unwrap();
        assert_eq!(failed_check.channel(), Some(UpdateChannel::Stable));
        assert_eq!(failed_check.result(), UpdateCheckResult::Failed);
        assert!(failed_check.error().is_some());

        let manifest = serde_json::to_vec_pretty(&manifest_value(
            "2.0.0",
            32,
            &scrozz_core::identity::platform_key(),
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let signature = signed_envelope(&manifest, "fixture", &key);
        checker.updater.fetcher = FakeFetcher::from_responses([
            (MANIFEST_URL, FakeResponse::Bytes(manifest)),
            (SIGNATURE_URL, FakeResponse::Bytes(signature)),
        ]);

        assert!(matches!(
            checker.check(&channel, &Version::new(1, 0, 0)).unwrap(),
            CheckOutcome::UpdateAvailable(_)
        ));
        let successful_check = checker.state().last_check().unwrap();
        assert_eq!(successful_check.channel(), Some(UpdateChannel::Stable));
        assert_eq!(
            successful_check.result(),
            UpdateCheckResult::UpdateAvailable
        );
        assert_eq!(successful_check.version(), Some(&Version::new(2, 0, 0)));
        assert_eq!(successful_check.generation(), Some(32));
    }

    #[test]
    fn check_only_generations_and_candidates_are_scoped_by_channel() {
        const PREVIEW_MANIFEST: &str = "https://updates.example.test/preview.json";
        const PREVIEW_SIGNATURE: &str = "https://updates.example.test/preview.sig";
        const STABLE_MANIFEST: &str = "https://updates.example.test/stable.json";
        const STABLE_SIGNATURE: &str = "https://updates.example.test/stable.sig";

        let scratch = ScratchDir::new("passive-channel-switch");
        let key = signing_key(33);
        let keys = ring(&[("fixture", &key)]);
        let catalog = crate::EndpointCatalog::new(
            Some(crate::UpdateEndpoints::new(STABLE_MANIFEST, STABLE_SIGNATURE).unwrap()),
            Some(crate::UpdateEndpoints::new(PREVIEW_MANIFEST, PREVIEW_SIGNATURE).unwrap()),
        );
        let preview_manifest = serde_json::to_vec(&manifest_value(
            "3.0.0",
            100,
            &scrozz_core::identity::platform_key(),
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let preview_signature = signed_envelope(&preview_manifest, "fixture", &key);
        let mut checker = UpdateChecker::with_fetcher(
            scratch.path().join("state.json"),
            keys,
            FakeFetcher::from_responses([
                (PREVIEW_MANIFEST, FakeResponse::Bytes(preview_manifest)),
                (PREVIEW_SIGNATURE, FakeResponse::Bytes(preview_signature)),
            ]),
        )
        .unwrap();

        let preview = catalog.resolve(UpdateChannel::Preview).unwrap();
        let CheckOutcome::UpdateAvailable(update) =
            checker.check(&preview, &Version::new(1, 0, 0)).unwrap()
        else {
            panic!("preview update expected");
        };
        assert_eq!(update.channel(), Some(UpdateChannel::Preview));
        assert_eq!(
            checker
                .state()
                .highest_accepted_generation_for(UpdateChannel::Preview),
            100
        );

        let stable_manifest = serde_json::to_vec(&manifest_value(
            "2.0.0",
            1,
            &scrozz_core::identity::platform_key(),
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let stable_signature = signed_envelope(&stable_manifest, "fixture", &key);
        checker.updater.fetcher = FakeFetcher::from_responses([
            (STABLE_MANIFEST, FakeResponse::Bytes(stable_manifest)),
            (STABLE_SIGNATURE, FakeResponse::Bytes(stable_signature)),
        ]);

        let stable = catalog.resolve(UpdateChannel::Stable).unwrap();
        let CheckOutcome::UpdateAvailable(update) =
            checker.check(&stable, &Version::new(1, 0, 0)).unwrap()
        else {
            panic!("stable update expected");
        };
        assert_eq!(update.channel(), Some(UpdateChannel::Stable));
        assert_eq!(
            checker
                .state()
                .highest_accepted_generation_for(UpdateChannel::Stable),
            1
        );
        assert_eq!(
            checker
                .state()
                .highest_accepted_generation_for(UpdateChannel::Preview),
            100
        );
    }

    #[test]
    fn returning_a_cached_candidate_persists_a_fresh_completion_time() {
        let scratch = ScratchDir::new("passive-cached-completion");
        let state_path = scratch.path().join("state.json");
        let key = signing_key(35);
        let keys = ring(&[("fixture", &key)]);
        let channel = crate::EndpointCatalog::new(
            Some(crate::UpdateEndpoints::new(MANIFEST_URL, SIGNATURE_URL).unwrap()),
            None,
        )
        .resolve(UpdateChannel::Stable)
        .unwrap();
        let manifest = serde_json::to_vec(&manifest_value(
            "2.0.0",
            1,
            &scrozz_core::identity::platform_key(),
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let signature = signed_envelope(&manifest, "fixture", &key);
        let mut checker = UpdateChecker::with_fetcher(
            &state_path,
            keys.clone(),
            FakeFetcher::from_responses([
                (MANIFEST_URL, FakeResponse::Bytes(manifest)),
                (SIGNATURE_URL, FakeResponse::Bytes(signature)),
            ]),
        )
        .unwrap();
        assert!(matches!(
            checker.check(&channel, &Version::new(1, 0, 0)).unwrap(),
            CheckOutcome::UpdateAvailable(_)
        ));
        drop(checker);

        let mut persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        persisted["last_check"]["completed_at_unix_seconds"] = serde_json::json!(1);
        fs::write(&state_path, serde_json::to_vec_pretty(&persisted).unwrap()).unwrap();

        let mut checker = UpdateChecker::with_fetcher(
            &state_path,
            keys,
            FakeFetcher::from_responses(std::iter::empty()),
        )
        .unwrap();
        assert!(matches!(
            checker.check(&channel, &Version::new(1, 0, 0)).unwrap(),
            CheckOutcome::UpdateAvailable(_)
        ));
        assert!(
            checker
                .state()
                .last_check()
                .unwrap()
                .completed_at_unix_seconds()
                .is_some_and(|completed| completed > 1)
        );
    }

    #[test]
    fn check_only_does_not_reuse_a_candidate_for_another_platform() {
        let scratch = ScratchDir::new("passive-platform-switch");
        let state_path = scratch.path().join("state.json");
        let key = signing_key(34);
        let keys = ring(&[("fixture", &key)]);
        let channel = crate::EndpointCatalog::new(
            Some(crate::UpdateEndpoints::new(MANIFEST_URL, SIGNATURE_URL).unwrap()),
            None,
        )
        .resolve(UpdateChannel::Stable)
        .unwrap();
        let first_manifest = serde_json::to_vec(&manifest_value(
            "2.0.0",
            1,
            &scrozz_core::identity::platform_key(),
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let first_signature = signed_envelope(&first_manifest, "fixture", &key);
        let mut checker = UpdateChecker::with_fetcher(
            &state_path,
            keys.clone(),
            FakeFetcher::from_responses([
                (MANIFEST_URL, FakeResponse::Bytes(first_manifest)),
                (SIGNATURE_URL, FakeResponse::Bytes(first_signature)),
            ]),
        )
        .unwrap();
        assert!(matches!(
            checker.check(&channel, &Version::new(1, 0, 0)).unwrap(),
            CheckOutcome::UpdateAvailable(_)
        ));
        drop(checker);

        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state["artifact"]["platform"] = serde_json::Value::String("other-platform".into());
        fs::write(&state_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

        let next_manifest = serde_json::to_vec(&manifest_value(
            "3.0.0",
            2,
            &scrozz_core::identity::platform_key(),
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let next_signature = signed_envelope(&next_manifest, "fixture", &key);
        let mut checker = UpdateChecker::with_fetcher(
            state_path,
            keys,
            FakeFetcher::from_responses([
                (MANIFEST_URL, FakeResponse::Bytes(next_manifest)),
                (SIGNATURE_URL, FakeResponse::Bytes(next_signature)),
            ]),
        )
        .unwrap();

        let CheckOutcome::UpdateAvailable(update) =
            checker.check(&channel, &Version::new(1, 0, 0)).unwrap()
        else {
            panic!("a fresh platform candidate was expected");
        };
        assert_eq!(update.version(), &Version::new(3, 0, 0));
    }

    #[test]
    fn an_empty_key_ring_refuses_before_fetching() {
        let scratch = ScratchDir::new("no-pinned-keys");
        let mut updater = Updater::with_fetcher(
            scratch.path().join("state.json"),
            PinnedKeyRing::production(),
            FakeFetcher::default(),
        )
        .unwrap();

        assert!(matches!(
            updater.check(MANIFEST_URL, SIGNATURE_URL, &Version::new(1, 0, 0)),
            Err(Error::NoPinnedKeys)
        ));
        assert_eq!(updater.state().phase(), Phase::Idle);
    }

    #[test]
    fn the_state_lock_excludes_concurrent_updaters_and_releases_on_drop() {
        let scratch = ScratchDir::new("state-lock");
        let state_path = scratch.path().join("state.json");
        let updater = Updater::with_fetcher(
            &state_path,
            PinnedKeyRing::production(),
            FakeFetcher::default(),
        )
        .unwrap();

        assert!(matches!(
            Updater::with_fetcher(
                &state_path,
                PinnedKeyRing::production(),
                FakeFetcher::default()
            ),
            Err(Error::StateLocked(path)) if path == state_path
        ));

        drop(updater);
        assert!(
            Updater::with_fetcher(
                &state_path,
                PinnedKeyRing::production(),
                FakeFetcher::default()
            )
            .is_ok()
        );
    }

    #[test]
    fn reset_abandons_preinstall_phases_without_deleting_caller_files() {
        let (scratch, _keys, mut available, _update) = checked_updater(
            "reset-available",
            FakeResponse::Bytes(CANDIDATE_BYTES.to_vec()),
        );
        available.reset_to_idle().unwrap();
        assert_eq!(available.state().phase(), Phase::Idle);
        assert_eq!(available.state().highest_accepted_generation(), 31);
        drop(available);
        drop(scratch);

        let (scratch, _keys, mut downloaded, update) = checked_updater(
            "reset-downloaded",
            FakeResponse::Bytes(CANDIDATE_BYTES.to_vec()),
        );
        let download_path = scratch.path().join("candidate.download");
        downloaded.download(&update, &download_path).unwrap();
        downloaded.reset_to_idle().unwrap();
        assert_eq!(downloaded.state().phase(), Phase::Idle);
        assert_eq!(fs::read(&download_path).unwrap(), CANDIDATE_BYTES);
        drop(downloaded);
        drop(scratch);

        let (scratch, _keys, mut staged, staged_artifact) = staged_updater("reset-staged");
        let download_path = scratch.path().join("candidate.download");
        let staged_path = staged_artifact.path().to_path_buf();
        staged.reset_to_idle().unwrap();
        assert_eq!(staged.state().phase(), Phase::Idle);
        assert_eq!(fs::read(download_path).unwrap(), CANDIDATE_BYTES);
        assert_eq!(fs::read(staged_path).unwrap(), CANDIDATE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn staging_preserves_downloaded_file_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let (scratch, _keys, mut updater, update) = checked_updater(
            "staged-permissions",
            FakeResponse::Bytes(CANDIDATE_BYTES.to_vec()),
        );
        let download = updater
            .download(&update, scratch.path().join("candidate.download"))
            .unwrap();
        fs::set_permissions(download.path(), fs::Permissions::from_mode(0o751)).unwrap();

        let staged = updater
            .stage(&download, scratch.path().join("candidate.staged"))
            .unwrap();

        assert_eq!(
            fs::metadata(staged.path()).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }

    #[test]
    fn size_and_digest_mismatches_remove_the_temporary_and_destination() {
        let mut same_size_wrong_digest = CANDIDATE_BYTES.to_vec();
        same_size_wrong_digest[0] ^= 0xff;
        let cases = [
            ("size-mismatch", b"short".to_vec(), true),
            ("digest-mismatch", same_size_wrong_digest, false),
        ];

        for (label, response, is_size) in cases {
            let (scratch, _keys, mut updater, update) =
                checked_updater(label, FakeResponse::Bytes(response));
            let destination = scratch.path().join("candidate.download");
            let error = updater.download(&update, &destination).unwrap_err();
            assert_eq!(
                matches!(error, Error::ArtifactSizeMismatch { .. }),
                is_size,
                "{error}"
            );
            assert_eq!(
                matches!(error, Error::ArtifactDigestMismatch { .. }),
                !is_size,
                "{error}"
            );
            assert!(!destination.exists());
            assert_eq!(updater.state().phase(), Phase::Failed);
            let retry = updater.retry_available_update().unwrap();
            assert_eq!(retry.version(), &Version::new(2, 0, 0));
            assert_eq!(updater.state().phase(), Phase::UpdateAvailable);
            let leftovers: Vec<_> = fs::read_dir(scratch.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().ends_with(".part"))
                .collect();
            assert!(leftovers.is_empty(), "{leftovers:?}");
        }
    }

    #[test]
    fn a_partial_transport_failure_is_cleaned_up() {
        let (scratch, _keys, mut updater, update) = checked_updater(
            "partial-fetch",
            FakeResponse::PartialFailure(b"partial candidate".to_vec()),
        );
        let destination = scratch.path().join("candidate.download");
        assert!(matches!(
            updater.download(&update, &destination),
            Err(Error::FetchFailed { .. })
        ));
        assert!(!destination.exists());
        assert_eq!(updater.state().phase(), Phase::Failed);
    }

    #[test]
    fn accepted_state_survives_a_crash_safe_reload() {
        let (scratch, keys, updater, update) = checked_updater(
            "state-reload",
            FakeResponse::Bytes(CANDIDATE_BYTES.to_vec()),
        );
        assert_eq!(update.generated(), 31);
        drop(updater);

        let reopened = Updater::with_fetcher(
            scratch.path().join("state.json"),
            keys,
            FakeFetcher::default(),
        )
        .unwrap();
        assert_eq!(reopened.state().phase(), Phase::UpdateAvailable);
        assert_eq!(reopened.state().highest_accepted_generation(), 31);
        assert_eq!(
            reopened.available_update().unwrap().version(),
            &Version::new(2, 0, 0)
        );

        let state_bytes = fs::read(scratch.path().join("state.json")).unwrap();
        let state_json: Value = serde_json::from_slice(&state_bytes).unwrap();
        assert_eq!(state_json["schema"], 1);
        assert_eq!(state_json["phase"], "UpdateAvailable");
        assert!(
            fs::read_dir(scratch.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().ends_with(".part"))
        );
    }

    #[test]
    fn a_current_manifest_advances_the_generation_watermark() {
        let scratch = ScratchDir::new("current-generation-watermark");
        let key = signing_key(27);
        let manifest = serde_json::to_vec(&manifest_value(
            "1.0.0",
            100,
            &scrozz_core::identity::platform_key(),
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let signature = signed_envelope(&manifest, "current", &key);
        let fetcher = FakeFetcher::from_responses([
            (MANIFEST_URL, FakeResponse::Bytes(manifest)),
            (SIGNATURE_URL, FakeResponse::Bytes(signature)),
        ]);
        let mut updater = Updater::with_fetcher(
            scratch.path().join("state.json"),
            ring(&[("current", &key)]),
            fetcher,
        )
        .unwrap();

        assert!(matches!(
            updater
                .check(MANIFEST_URL, SIGNATURE_URL, &Version::new(1, 0, 0))
                .unwrap(),
            CheckOutcome::Current { generated: 100, .. }
        ));
        assert_eq!(updater.state().highest_accepted_generation(), 100);
        let check = updater.state().last_check().unwrap();
        assert_eq!(check.result(), UpdateCheckResult::Current);
        assert_eq!(check.version(), Some(&Version::new(1, 0, 0)));
        assert_eq!(check.generation(), Some(100));
    }

    #[test]
    fn interrupted_check_reloads_as_a_persisted_failure() {
        let scratch = ScratchDir::new("interrupted-check");
        let mut updater = Updater::with_fetcher(
            scratch.path().join("state.json"),
            PinnedKeyRing::production(),
            FakeFetcher::default(),
        )
        .unwrap();
        let reserved = fsutil::reserve_temp(scratch.path(), "manifest").unwrap();
        let temporary = reserved.path().to_path_buf();
        drop(reserved);
        fs::write(&temporary, b"partial manifest").unwrap();
        let mut checking = updater.state.clone();
        checking.phase = Phase::Checking;
        checking.transient_paths = vec![temporary.clone()];
        updater.commit(checking).unwrap();
        drop(updater);

        let mut reopened = Updater::with_fetcher(
            scratch.path().join("state.json"),
            PinnedKeyRing::production(),
            FakeFetcher::default(),
        )
        .unwrap();
        assert_eq!(reopened.state().phase(), Phase::Failed);
        assert!(reopened.state().failure().unwrap().contains("update check"));
        assert!(!temporary.exists());
        reopened.reset_to_idle().unwrap();
        assert_eq!(reopened.state().phase(), Phase::Idle);
    }

    #[test]
    fn interrupted_download_leaves_an_unverified_destination_untouched() {
        let (scratch, keys, mut updater, _update) = checked_updater(
            "interrupted-download",
            FakeResponse::Bytes(CANDIDATE_BYTES.to_vec()),
        );
        let reserved = fsutil::reserve_temp(scratch.path(), "download").unwrap();
        let temporary = reserved.path().to_path_buf();
        drop(reserved);
        let destination = scratch.path().join("candidate.download");
        fs::write(&temporary, b"partial").unwrap();
        fs::write(&destination, b"possibly renamed partial").unwrap();
        let mut downloading = updater.state.clone();
        downloading.phase = Phase::Downloading;
        downloading.downloaded_path = Some(destination.clone());
        downloading.transient_paths = vec![temporary.clone()];
        updater.commit(downloading).unwrap();
        drop(updater);

        let reopened = Updater::with_fetcher(
            scratch.path().join("state.json"),
            keys,
            FakeFetcher::default(),
        )
        .unwrap();
        assert_eq!(reopened.state().phase(), Phase::Failed);
        assert!(
            reopened
                .state()
                .failure()
                .unwrap()
                .contains("artifact download")
        );
        assert!(!temporary.exists());
        assert_eq!(fs::read(&destination).unwrap(), b"possibly renamed partial");

        let state_json: Value =
            serde_json::from_slice(&fs::read(scratch.path().join("state.json")).unwrap()).unwrap();
        assert_eq!(state_json["phase"], "Failed");
    }

    #[test]
    fn untrusted_transient_paths_are_rejected_without_deletion() {
        let scratch = ScratchDir::new("untrusted-transient-path");
        let victim = scratch.path().join("do-not-delete");
        fs::write(&victim, b"important").unwrap();
        let state_path = scratch.path().join("state.json");
        fs::write(
            &state_path,
            serde_json::to_vec(&serde_json::json!({
                "schema": 1,
                "phase": "Checking",
                "highest_accepted_generation": 0,
                "candidate": null,
                "artifact": null,
                "downloaded_path": null,
                "staged_path": null,
                "install_plan": null,
                "transient_paths": [victim],
                "rollback_requested": false,
                "failure": null
            }))
            .unwrap(),
        )
        .unwrap();

        let result = Updater::with_fetcher(
            state_path,
            PinnedKeyRing::production(),
            FakeFetcher::default(),
        );
        assert!(matches!(result, Err(Error::InvalidState(_))));
        assert_eq!(fs::read(victim).unwrap(), b"important");
    }

    #[test]
    fn every_install_crash_point_recovers_to_the_same_completed_swap() {
        for pause in [
            InstallPause::BeforeFirstRename,
            InstallPause::AfterFirstRename,
            InstallPause::AfterSecondRename,
        ] {
            let (scratch, keys, mut updater, staged) = staged_updater("install-crash");
            let live = scratch.path().join("scrozz");
            let previous = scratch.path().join("scrozz.previous");
            let failed = scratch.path().join("scrozz.failed");
            fs::write(&live, b"old installed bytes").unwrap();
            let plan = InstallPlan::new(&live, &previous, &failed).unwrap();

            updater.begin_install(&staged, plan).unwrap();
            assert!(matches!(
                updater.perform_install(pause),
                Err(Error::Recovery(_))
            ));
            assert_eq!(updater.state().phase(), Phase::AwaitingRestart);
            drop(updater);

            let recovered = Updater::with_fetcher(
                scratch.path().join("state.json"),
                keys,
                FakeFetcher::default(),
            )
            .unwrap();
            assert_eq!(recovered.state().phase(), Phase::Installed, "{pause:?}");
            assert_eq!(fs::read(&live).unwrap(), CANDIDATE_BYTES, "{pause:?}");
            assert_eq!(
                fs::read(&previous).unwrap(),
                b"old installed bytes",
                "{pause:?}"
            );
            assert!(!failed.exists());
        }
    }

    #[test]
    fn explicit_rollback_restores_old_file_and_preserves_candidate() {
        let (scratch, _keys, mut updater, staged) = staged_updater("rollback");
        let live = scratch.path().join("scrozz");
        let previous = scratch.path().join("scrozz.previous");
        let failed = scratch.path().join("scrozz.failed");
        fs::write(&live, b"old installed bytes").unwrap();
        updater
            .install(
                &staged,
                InstallPlan::new(&live, &previous, &failed).unwrap(),
            )
            .unwrap();
        assert_eq!(updater.state().phase(), Phase::Installed);

        updater.rollback().unwrap();

        assert_eq!(updater.state().phase(), Phase::RolledBack);
        assert_eq!(fs::read(&live).unwrap(), b"old installed bytes");
        assert_eq!(fs::read(&failed).unwrap(), CANDIDATE_BYTES);
        assert!(!previous.exists());
    }

    #[test]
    fn rollback_intent_recovers_across_each_rename_point() {
        for completed_renames in 0..=2 {
            let (scratch, keys, mut updater, staged) = staged_updater("rollback-crash");
            let live = scratch.path().join("scrozz");
            let previous = scratch.path().join("scrozz.previous");
            let failed = scratch.path().join("scrozz.failed");
            fs::write(&live, b"old installed bytes").unwrap();
            let plan = InstallPlan::new(&live, &previous, &failed).unwrap();
            updater.install(&staged, plan).unwrap();

            let mut requested = updater.state.clone();
            requested.rollback_requested = true;
            updater.commit(requested).unwrap();
            if completed_renames >= 1 {
                fsutil::rename_synced(&live, &failed).unwrap();
            }
            if completed_renames == 2 {
                fsutil::rename_synced(&previous, &live).unwrap();
            }
            drop(updater);

            let recovered = Updater::with_fetcher(
                scratch.path().join("state.json"),
                keys,
                FakeFetcher::default(),
            )
            .unwrap();
            assert_eq!(
                recovered.state().phase(),
                Phase::RolledBack,
                "after {completed_renames} renames"
            );
            assert_eq!(fs::read(&live).unwrap(), b"old installed bytes");
            assert_eq!(fs::read(&failed).unwrap(), CANDIDATE_BYTES);
            assert!(!previous.exists());
        }
    }

    #[test]
    fn recovery_never_deletes_the_only_good_copy() {
        let (scratch, keys, mut updater, staged) = staged_updater("only-copy");
        let live = scratch.path().join("scrozz");
        let previous = scratch.path().join("scrozz.previous");
        let failed = scratch.path().join("scrozz.failed");
        fs::write(&live, b"old installed bytes").unwrap();
        updater
            .begin_install(
                &staged,
                InstallPlan::new(&live, &previous, &failed).unwrap(),
            )
            .unwrap();
        assert!(
            updater
                .perform_install(InstallPause::AfterSecondRename)
                .is_err()
        );
        fs::remove_file(&previous).unwrap();
        drop(updater);

        let reopened = Updater::with_fetcher(
            scratch.path().join("state.json"),
            keys,
            FakeFetcher::default(),
        )
        .unwrap();
        assert_eq!(reopened.state().phase(), Phase::Failed);
        assert_eq!(fs::read(&live).unwrap(), CANDIDATE_BYTES);
        assert!(!failed.exists());
    }

    #[test]
    fn directories_are_rejected_instead_of_treated_as_bundles() {
        let (scratch, _keys, mut updater, staged) = staged_updater("directory");
        let live = scratch.path().join("Scrozz.bundle");
        fs::create_dir(&live).unwrap();
        let result = updater.install(
            &staged,
            InstallPlan::new(
                &live,
                scratch.path().join("previous"),
                scratch.path().join("failed"),
            )
            .unwrap(),
        );
        assert!(matches!(result, Err(Error::DirectoryUnsupported(path)) if path == live));
        assert_eq!(updater.state().phase(), Phase::Staged);
    }

    #[test]
    fn missing_platform_is_reported_separately_from_current_version() {
        let scratch = ScratchDir::new("missing-platform");
        let key = signing_key(41);
        let keys = ring(&[("fixture", &key)]);
        let manifest = serde_json::to_vec(&manifest_value(
            "2.0.0",
            41,
            "unsupported-architecture",
            CANDIDATE_BYTES,
        ))
        .unwrap();
        let signature = signed_envelope(&manifest, "fixture", &key);
        let fetcher = FakeFetcher::from_responses([
            (MANIFEST_URL, FakeResponse::Bytes(manifest)),
            (SIGNATURE_URL, FakeResponse::Bytes(signature)),
        ]);
        let mut updater =
            Updater::with_fetcher(scratch.path().join("state.json"), keys, fetcher).unwrap();

        assert!(matches!(
            updater
                .check(MANIFEST_URL, SIGNATURE_URL, &Version::new(1, 0, 0))
                .unwrap(),
            CheckOutcome::PlatformUnavailable { generated: 41, .. }
        ));
        assert_eq!(updater.state().phase(), Phase::Idle);
        assert_eq!(updater.state().highest_accepted_generation(), 41);
        let check = updater.state().last_check().unwrap();
        assert_eq!(check.result(), UpdateCheckResult::PlatformUnavailable);
        assert_eq!(check.version(), Some(&Version::new(2, 0, 0)));
        assert_eq!(check.generation(), Some(41));
        assert_eq!(
            check.platform(),
            Some(scrozz_core::identity::platform_key().as_str())
        );
    }
}
