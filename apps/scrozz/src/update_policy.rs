use std::{
    path::PathBuf,
    str::FromStr,
    sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use scrozz_core::Error as CoreError;
use scrozz_update::{
    ArtifactKind, ChannelEndpointStatus, CheckOutcome, EndpointCatalog, PinnedKeyRing,
    ResolvedChannel, UpdateChannel, UpdateChecker, inspect_state,
};
use semver::Version;

use crate::{
    fault::{CliError, CliResult},
    settings::{
        self, UPDATE_CHANNEL_KEY, UPDATE_CHECK_AUTOMATICALLY_KEY, UPDATE_CHECK_INTERVAL_HOURS_KEY,
    },
    settings_store::SettingsStore,
    system_integration::SystemContext,
};

pub(crate) const MINIMUM_CHECK_INTERVAL_HOURS: u64 = 1;
pub(crate) const MAXIMUM_CHECK_INTERVAL_HOURS: u64 = 168;

#[derive(Debug, Clone)]
pub(crate) struct UpdateConfiguration {
    automatic: bool,
    channel: UpdateChannel,
    interval_hours: u64,
    endpoints: EndpointCatalog,
    trusted_key_count: usize,
}

impl UpdateConfiguration {
    pub(crate) fn production(settings: &SettingsStore) -> CliResult<Self> {
        Self::from_store_with(
            settings,
            EndpointCatalog::production(),
            production_keys().len(),
        )
    }

    fn from_store_with(
        settings: &SettingsStore,
        endpoints: EndpointCatalog,
        trusted_key_count: usize,
    ) -> CliResult<Self> {
        let automatic = settings.boolean(UPDATE_CHECK_AUTOMATICALLY_KEY)?;
        let channel_setting = settings::lookup(UPDATE_CHANNEL_KEY)?;
        let channel = UpdateChannel::from_str(settings.get(channel_setting)?.value())
            .map_err(update_error)?;
        let interval_setting = settings::lookup(UPDATE_CHECK_INTERVAL_HOURS_KEY)?;
        let interval_hours = settings
            .get(interval_setting)?
            .value()
            .parse::<u64>()
            .map_err(|error| {
                configuration_error(format!(
                    "{UPDATE_CHECK_INTERVAL_HOURS_KEY} is not a valid integer: {error}"
                ))
            })?;
        Self::from_parts(
            automatic,
            channel,
            interval_hours,
            endpoints,
            trusted_key_count,
        )
    }

    fn from_parts(
        automatic: bool,
        channel: UpdateChannel,
        interval_hours: u64,
        endpoints: EndpointCatalog,
        trusted_key_count: usize,
    ) -> CliResult<Self> {
        if !(MINIMUM_CHECK_INTERVAL_HOURS..=MAXIMUM_CHECK_INTERVAL_HOURS).contains(&interval_hours)
        {
            return Err(configuration_error(format!(
                "{UPDATE_CHECK_INTERVAL_HOURS_KEY} must be between \
                 {MINIMUM_CHECK_INTERVAL_HOURS} and {MAXIMUM_CHECK_INTERVAL_HOURS}"
            )));
        }
        Ok(Self {
            automatic,
            channel,
            interval_hours,
            endpoints,
            trusted_key_count,
        })
    }

    pub(crate) const fn automatic(&self) -> bool {
        self.automatic
    }

    pub(crate) const fn channel(&self) -> UpdateChannel {
        self.channel
    }

    pub(crate) const fn interval_hours(&self) -> u64 {
        self.interval_hours
    }

    pub(crate) const fn trusted_key_count(&self) -> usize {
        self.trusted_key_count
    }

    pub(crate) fn endpoint_status(&self, channel: UpdateChannel) -> ChannelEndpointStatus {
        self.endpoints.status(channel)
    }

    pub(crate) fn resolve(&self, channel: UpdateChannel) -> scrozz_update::Result<ResolvedChannel> {
        self.endpoints.resolve(channel)
    }

    pub(crate) fn blocked_reason(&self, channel: UpdateChannel) -> Option<String> {
        let mut reasons = Vec::new();
        if let ChannelEndpointStatus::Disabled { reason, .. } = self.endpoint_status(channel) {
            reasons.push(reason.to_owned());
        }
        if self.trusted_key_count == 0 {
            reasons.push("no trusted production signing keys are pinned".to_owned());
        }
        (!reasons.is_empty()).then(|| reasons.join("; "))
    }
}

#[must_use]
pub(crate) fn production_keys() -> PinnedKeyRing {
    PinnedKeyRing::production()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AutomaticCheckEvent {
    checked_at_unix_seconds: Option<u64>,
    result: &'static str,
    detail: String,
}

impl AutomaticCheckEvent {
    pub(crate) const fn checked_at_unix_seconds(&self) -> Option<u64> {
        self.checked_at_unix_seconds
    }

    pub(crate) const fn result(&self) -> &'static str {
        self.result
    }

    pub(crate) fn detail(&self) -> &str {
        &self.detail
    }

    fn completed(result: Result<CheckOutcome, String>) -> Self {
        let (result, detail) = match result {
            Ok(CheckOutcome::Current { version, .. }) => {
                ("current", format!("Scrozz {version} is current"))
            }
            Ok(CheckOutcome::UpdateAvailable(update)) => (
                "update-available",
                format!(
                    "Scrozz {} is available; automatic checking did not download or install it",
                    update.version()
                ),
            ),
            Ok(CheckOutcome::PlatformUnavailable {
                version, platform, ..
            }) => (
                "platform-unavailable",
                format!("Scrozz {version} has no artifact for {platform}"),
            ),
            Err(error) => ("failed", error),
        };
        Self {
            checked_at_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_secs()),
            result,
            detail,
        }
    }
}

trait CheckRunner: Send + 'static {
    fn check(&mut self) -> Result<CheckOutcome, String>;
}

struct ProductionCheckRunner {
    state_path: PathBuf,
    keys: PinnedKeyRing,
    channel: ResolvedChannel,
    installed_version: Version,
    artifact_kind: ArtifactKind,
}

impl CheckRunner for ProductionCheckRunner {
    fn check(&mut self) -> Result<CheckOutcome, String> {
        let mut checker = UpdateChecker::open(&self.state_path, self.keys.clone())
            .map_err(|error| error.to_string())?;
        checker
            .check_for_kind(&self.channel, &self.installed_version, self.artifact_kind)
            .map_err(|error| error.to_string())
    }
}

struct CheckWorker {
    jobs: SyncSender<()>,
    results: Receiver<Result<CheckOutcome, String>>,
}

impl CheckWorker {
    fn spawn<R: CheckRunner>(mut runner: R) -> CliResult<Self> {
        let (jobs, job_receiver) = mpsc::sync_channel(1);
        let (result_sender, results) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("scrozz-update-check".to_owned())
            .spawn(move || {
                while job_receiver.recv().is_ok() {
                    if result_sender.send(runner.check()).is_err() {
                        break;
                    }
                }
            })
            .map_err(|error| {
                configuration_error(format!("could not start update worker: {error}"))
            })?;
        Ok(Self { jobs, results })
    }
}

pub(crate) struct AutomaticUpdateScheduler {
    configuration: Option<UpdateConfiguration>,
    worker: Option<CheckWorker>,
    blocked_reason: Option<String>,
    in_flight: bool,
    next_due: Option<Instant>,
    latest: Option<AutomaticCheckEvent>,
}

impl AutomaticUpdateScheduler {
    pub(crate) fn production(
        settings: &SettingsStore,
        context: &SystemContext,
        now: Instant,
    ) -> CliResult<Self> {
        let configuration = UpdateConfiguration::production(settings)?;
        if !configuration.automatic() {
            return Ok(Self::inactive(configuration, None));
        }
        let Some(blocked_reason) = configuration.blocked_reason(configuration.channel()) else {
            let channel = configuration
                .resolve(configuration.channel())
                .map_err(update_error)?;
            let installed_version = Version::parse(env!("CARGO_PKG_VERSION")).map_err(|error| {
                configuration_error(format!("invalid installed Scrozz version: {error}"))
            })?;
            let state = inspect_state(&context.update_state).map_err(update_error)?;
            let next_due = initial_check_due(
                now,
                state
                    .last_check()
                    .and_then(|check| check.completed_at_unix_seconds()),
                configuration.interval_hours(),
            );
            let worker = CheckWorker::spawn(ProductionCheckRunner {
                state_path: context.update_state.clone(),
                keys: production_keys(),
                channel,
                installed_version,
                artifact_kind: context.update_artifact_kind(),
            })?;
            return Ok(Self {
                configuration: Some(configuration),
                worker: Some(worker),
                blocked_reason: None,
                in_flight: false,
                next_due: Some(next_due),
                latest: None,
            });
        };
        Ok(Self::inactive(configuration, Some(blocked_reason)))
    }

    pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            configuration: None,
            worker: None,
            blocked_reason: Some(reason.into()),
            in_flight: false,
            next_due: None,
            latest: None,
        }
    }

    fn inactive(configuration: UpdateConfiguration, blocked_reason: Option<String>) -> Self {
        Self {
            configuration: Some(configuration),
            worker: None,
            blocked_reason,
            in_flight: false,
            next_due: None,
            latest: None,
        }
    }

    pub(crate) fn tick(&mut self, now: Instant) -> Option<AutomaticCheckEvent> {
        let mut completed = None;
        let worker_result = self.worker.as_ref().map(|worker| worker.results.try_recv());
        if let Some(worker_result) = worker_result {
            match worker_result {
                Ok(result) => {
                    self.in_flight = false;
                    if let Some(configuration) = self.configuration.as_ref() {
                        self.next_due =
                            Some(now + Duration::from_secs(configuration.interval_hours() * 3600));
                    }
                    completed = Some(AutomaticCheckEvent::completed(result));
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.in_flight = false;
                    self.worker = None;
                    self.next_due = None;
                    self.blocked_reason =
                        Some("the automatic update worker stopped unexpectedly".to_owned());
                    completed = Some(AutomaticCheckEvent::completed(Err(
                        "the automatic update worker stopped unexpectedly".to_owned(),
                    )));
                }
            }
        }
        if let Some(event) = completed.as_ref() {
            self.latest = Some(event.clone());
        }

        if !self.in_flight && self.next_due.is_some_and(|due| due <= now) {
            self.start_check();
        }
        completed
    }

    fn start_check(&mut self) {
        let Some(send_result) = self.worker.as_ref().map(|worker| worker.jobs.try_send(())) else {
            return;
        };
        match send_result {
            Ok(()) => {
                self.in_flight = true;
                self.next_due = None;
            }
            Err(TrySendError::Full(())) => {
                self.in_flight = true;
                self.next_due = None;
            }
            Err(TrySendError::Disconnected(())) => {
                self.worker = None;
                self.next_due = None;
                self.blocked_reason =
                    Some("the automatic update worker stopped unexpectedly".to_owned());
            }
        }
    }

    pub(crate) fn configuration(&self) -> Option<&UpdateConfiguration> {
        self.configuration.as_ref()
    }

    pub(crate) fn state(&self) -> &'static str {
        match (
            self.configuration
                .as_ref()
                .map(UpdateConfiguration::automatic),
            self.blocked_reason.as_ref(),
            self.in_flight,
        ) {
            (Some(false), _, _) => "off",
            (_, Some(_), _) => "blocked",
            (_, _, true) => "checking",
            (Some(true), None, false) => "scheduled",
            (None, None, false) => "unavailable",
        }
    }

    pub(crate) fn blocked_reason(&self) -> Option<&str> {
        self.blocked_reason.as_deref()
    }

    pub(crate) const fn in_flight(&self) -> bool {
        self.in_flight
    }

    pub(crate) fn latest(&self) -> Option<&AutomaticCheckEvent> {
        self.latest.as_ref()
    }
}

fn initial_check_due(now: Instant, last_check: Option<u64>, interval_hours: u64) -> Instant {
    let interval_seconds = interval_hours * 3600;
    let elapsed = last_check
        .zip(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .ok()
                .map(|duration| duration.as_secs()),
        )
        .map_or(interval_seconds, |(last, current)| {
            current.saturating_sub(last).min(interval_seconds)
        });
    now + Duration::from_secs(interval_seconds - elapsed)
}

fn configuration_error(message: impl Into<String>) -> CliError {
    CliError::Core(CoreError::Storage(message.into()))
}

fn update_error(error: scrozz_update::Error) -> CliError {
    CliError::Core(CoreError::Platform(format!(
        "signed update failed: {error}"
    )))
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    struct CountingRunner {
        calls: Arc<AtomicUsize>,
    }

    impl CheckRunner for CountingRunner {
        fn check(&mut self) -> Result<CheckOutcome, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CheckOutcome::Current {
                version: Version::new(1, 0, 0),
                generated: 1,
            })
        }
    }

    fn enabled_configuration() -> UpdateConfiguration {
        let endpoints = scrozz_update::UpdateEndpoints::new(
            "https://updates.example.test/stable/manifest.json",
            "https://updates.example.test/stable/manifest.sig",
        )
        .unwrap();
        UpdateConfiguration::from_parts(
            true,
            UpdateChannel::Stable,
            24,
            EndpointCatalog::new(Some(endpoints), None),
            1,
        )
        .unwrap()
    }

    #[test]
    fn scheduler_has_only_one_check_in_flight_and_reschedules_after_completion() {
        let calls = Arc::new(AtomicUsize::new(0));
        let now = Instant::now();
        let worker = CheckWorker::spawn(CountingRunner {
            calls: Arc::clone(&calls),
        })
        .unwrap();
        let mut scheduler = AutomaticUpdateScheduler {
            configuration: Some(enabled_configuration()),
            worker: Some(worker),
            blocked_reason: None,
            in_flight: false,
            next_due: Some(now),
            latest: None,
        };

        assert!(scheduler.tick(now).is_none());
        assert!(scheduler.in_flight());
        assert!(scheduler.tick(now).is_none());
        for _ in 0..100 {
            if scheduler.tick(now).is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!scheduler.in_flight());
        assert_eq!(scheduler.state(), "scheduled");
        assert!(
            scheduler
                .tick(now + Duration::from_secs(23 * 3600))
                .is_none()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            scheduler
                .tick(now + Duration::from_secs(24 * 3600))
                .is_none()
        );
        assert!(scheduler.in_flight());
    }

    #[test]
    fn automatic_checks_stay_blocked_without_both_trust_inputs() {
        let configuration = UpdateConfiguration::from_parts(
            true,
            UpdateChannel::Stable,
            24,
            EndpointCatalog::production(),
            0,
        )
        .unwrap();
        let reason = configuration.blocked_reason(UpdateChannel::Stable).unwrap();
        assert!(reason.contains("endpoint"));
        assert!(reason.contains("signing keys"));
    }

    #[test]
    fn persisted_check_time_prevents_restart_check_storms() {
        let now = Instant::now();
        let current = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let due = initial_check_due(now, Some(current - 3600), 24);
        assert!(due.duration_since(now) >= Duration::from_secs(23 * 3600 - 1));
        assert!(due.duration_since(now) <= Duration::from_secs(23 * 3600 + 1));
    }
}
