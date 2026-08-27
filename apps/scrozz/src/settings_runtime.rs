//! Settings changes that also touch live operating-system integration.
//!
//! Persistence alone is enough for most keys. Launch at login also owns
//! registration state outside the JSON document, so these helpers snapshot that
//! state, mutate the OS first, replace the settings document second, and restore
//! the prior enabled/disabled state if persistence fails. URL consent remains
//! independent from the separately managed OS URL-scheme registration.

use scrozz_core::Error as CoreError;
use scrozz_shell::{RegistrationStatus, autostart::AutostartPlan};

use crate::{
    fault::{CliError, CliResult},
    settings_hotkeys,
    settings_store::SettingsStore,
    system_integration::SystemContext,
};

const AUTOSTART_KEY: &str = "system.launch-at-login";
const URL_CONSENT_KEY: &str = "system.url-scheme-enabled";

trait Registration {
    fn status(&self) -> scrozz_core::Result<RegistrationStatus>;
    fn apply(&self) -> scrozz_core::Result<()>;
    fn remove(&self) -> scrozz_core::Result<()>;
}

impl Registration for AutostartPlan {
    fn status(&self) -> scrozz_core::Result<RegistrationStatus> {
        AutostartPlan::status(self)
    }

    fn apply(&self) -> scrozz_core::Result<()> {
        AutostartPlan::apply(self)
    }

    fn remove(&self) -> scrozz_core::Result<()> {
        AutostartPlan::remove(self)
    }
}

/// Validates and applies one setting.
///
/// # Errors
///
/// Returns a validation/conflict error before changing anything, a platform
/// error when registration fails, or a storage error when the settings document
/// cannot be replaced.
pub fn set(store: &mut SettingsStore, key: &str, value: &str) -> CliResult<()> {
    if let Some(conflict) = settings_hotkeys::check_edit(store, key, value)? {
        return Err(CliError::usage(format!(
            "{key} cannot use {value:?}: {conflict}"
        )));
    }

    match key {
        AUTOSTART_KEY => {
            let plan = SystemContext::current()?.autostart()?;
            set_autostart_with(store, value, &plan)
        }
        URL_CONSENT_KEY => {
            store.set(key, value)?;
            Ok(())
        }
        _ => {
            store.set(key, value)?;
            Ok(())
        }
    }
}

/// Validates and atomically persists a complete settings-form edit.
///
/// Every requested registration is changed before the one document replacement.
/// If a later registration or the write fails, earlier registration changes are
/// rolled back in reverse order.
pub fn apply(store: &mut SettingsStore, edits: &[(String, String)]) -> CliResult<()> {
    for (key, value) in edits {
        crate::settings::lookup(key)?.validate(value)?;
    }

    let autostart_value = edit_value(edits, AUTOSTART_KEY);
    if autostart_value.is_none() {
        store.apply(edits)?;
        return Ok(());
    }

    let context = SystemContext::current()?;
    let autostart = autostart_value.map(|_| context.autostart()).transpose()?;
    apply_with_registration(
        store,
        edits,
        autostart_value,
        autostart.as_ref().map(|plan| plan as &dyn Registration),
    )
}

/// Resets one setting and its associated platform state.
///
/// Resetting URL consent intentionally leaves registration installed; consent
/// and registration are separate controls.
pub fn reset(store: &mut SettingsStore, key: &str) -> CliResult<bool> {
    match key {
        AUTOSTART_KEY => {
            let plan = SystemContext::current()?.autostart()?;
            reset_autostart_with(store, &plan)
        }
        _ => store.reset(key),
    }
}

/// Resets every override and disables launch at login.
///
/// URL registration is not removed because reset only revokes its default-off
/// consent. The explicit `scrozz url unregister` command owns removal.
pub fn reset_all(store: &mut SettingsStore) -> CliResult<usize> {
    let plan = SystemContext::current()?.autostart()?;
    reset_all_with(store, &plan)
}

/// Reconciles the persisted launch-at-login toggle with the OS's actual state.
///
/// The OS is authoritative when it is clearly enabled or disabled, including a
/// user changing the setting in system preferences. Drift is returned without
/// overwriting either side so the settings window can explain and repair it.
pub fn reconcile_autostart(store: &mut SettingsStore) -> CliResult<RegistrationStatus> {
    let plan = SystemContext::current()?.autostart()?;
    reconcile_autostart_with(store, &plan)
}

fn edit_value<'a>(edits: &'a [(String, String)], key: &str) -> Option<&'a str> {
    edits
        .iter()
        .rev()
        .find_map(|(candidate, value)| (candidate == key).then_some(value.as_str()))
}

fn set_autostart_with(
    store: &mut SettingsStore,
    value: &str,
    registration: &dyn Registration,
) -> CliResult<()> {
    crate::settings::lookup(AUTOSTART_KEY)?.validate(value)?;
    let enabled = parse_bool(AUTOSTART_KEY, value)?;
    let previous = registration.status()?;
    set_registration(registration, enabled)?;
    if let Err(error) = store.set(AUTOSTART_KEY, value) {
        return rollback_one(registration, previous, error);
    }
    Ok(())
}

fn apply_with_registration(
    store: &mut SettingsStore,
    edits: &[(String, String)],
    autostart_value: Option<&str>,
    autostart: Option<&dyn Registration>,
) -> CliResult<()> {
    let autostart_enabled = autostart_value
        .map(|value| parse_bool(AUTOSTART_KEY, value))
        .transpose()?;
    let previous_autostart = autostart.map(Registration::status).transpose()?;
    let mut autostart_changed = false;

    if let (Some(enabled), Some(registration)) = (autostart_enabled, autostart) {
        set_registration(registration, enabled)?;
        autostart_changed = true;
    }

    if let Err(error) = store.apply(edits) {
        let mut failures = Vec::new();
        if autostart_changed
            && let Err(rollback) = restore_registration(
                autostart.expect("changed registration exists"),
                previous_autostart.expect("changed registration has a snapshot"),
            )
        {
            failures.push(rollback.to_string());
        }
        return Err(with_rollback_failures(error, &failures));
    }
    Ok(())
}

fn reset_autostart_with(
    store: &mut SettingsStore,
    registration: &dyn Registration,
) -> CliResult<bool> {
    let previous = registration.status()?;
    set_registration(registration, false)?;
    match store.reset(AUTOSTART_KEY) {
        Ok(changed) => Ok(changed),
        Err(error) => rollback_one(registration, previous, error),
    }
}

fn reset_all_with(store: &mut SettingsStore, registration: &dyn Registration) -> CliResult<usize> {
    let previous = registration.status()?;
    set_registration(registration, false)?;
    match store.reset_all() {
        Ok(count) => Ok(count),
        Err(error) => rollback_one(registration, previous, error),
    }
}

fn reconcile_autostart_with(
    store: &mut SettingsStore,
    registration: &dyn Registration,
) -> CliResult<RegistrationStatus> {
    let status = registration.status()?;
    let stored = store.boolean(AUTOSTART_KEY)?;
    let actual = match status {
        RegistrationStatus::Enabled => Some(true),
        RegistrationStatus::Disabled => Some(false),
        RegistrationStatus::Drifted => None,
    };
    if let Some(actual) = actual
        && actual != stored
    {
        store.set(AUTOSTART_KEY, if actual { "true" } else { "false" })?;
    }
    Ok(status)
}

fn parse_bool(key: &str, value: &str) -> CliResult<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(CliError::usage(format!(
            "{key} takes `true` or `false`, not {value:?}"
        ))),
    }
}

fn set_registration(registration: &dyn Registration, enabled: bool) -> scrozz_core::Result<()> {
    if enabled {
        registration.apply()?;
    } else {
        registration.remove()?;
    }
    let status = registration.status()?;
    let matches = matches!(
        (enabled, status),
        (true, RegistrationStatus::Enabled) | (false, RegistrationStatus::Disabled)
    );
    if matches {
        Ok(())
    } else {
        Err(CoreError::Platform(format!(
            "system registration reported {status:?} after it was asked to become {}",
            if enabled { "enabled" } else { "disabled" }
        )))
    }
}

fn restore_registration(
    registration: &dyn Registration,
    previous: RegistrationStatus,
) -> scrozz_core::Result<()> {
    set_registration(registration, previous != RegistrationStatus::Disabled)
}

fn rollback_one<T>(
    registration: &dyn Registration,
    previous: RegistrationStatus,
    original: CliError,
) -> CliResult<T> {
    match restore_registration(registration, previous) {
        Ok(()) => Err(original),
        Err(rollback) => Err(with_rollback_failures(original, &[rollback.to_string()])),
    }
}

fn with_rollback_failures(original: CliError, failures: &[String]) -> CliError {
    if failures.is_empty() {
        return original;
    }
    CliError::Core(CoreError::Storage(format!(
        "{original}; restoring previous system registration also failed: {}",
        failures.join("; ")
    )))
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use scrozz_core::{Error, Result};

    use super::*;

    struct FakeRegistration {
        status: Cell<RegistrationStatus>,
        fail: Cell<bool>,
    }

    impl FakeRegistration {
        fn new(status: RegistrationStatus) -> Self {
            Self {
                status: Cell::new(status),
                fail: Cell::new(false),
            }
        }
    }

    impl Registration for FakeRegistration {
        fn status(&self) -> Result<RegistrationStatus> {
            Ok(self.status.get())
        }

        fn apply(&self) -> Result<()> {
            if self.fail.get() {
                return Err(Error::Platform("injected registration failure".to_owned()));
            }
            self.status.set(RegistrationStatus::Enabled);
            Ok(())
        }

        fn remove(&self) -> Result<()> {
            if self.fail.get() {
                return Err(Error::Platform("injected registration failure".to_owned()));
            }
            self.status.set(RegistrationStatus::Disabled);
            Ok(())
        }
    }

    fn store(name: &str) -> (PathBuf, SettingsStore) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "scrozz-settings-runtime-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let store = SettingsStore::open(directory.join("settings.json")).unwrap();
        (directory, store)
    }

    #[test]
    fn launch_at_login_changes_the_platform_and_document_together() {
        let (directory, mut store) = store("autostart");
        let registration = FakeRegistration::new(RegistrationStatus::Disabled);

        set_autostart_with(&mut store, "true", &registration).unwrap();

        assert_eq!(registration.status().unwrap(), RegistrationStatus::Enabled);
        assert_eq!(store.get(AUTOSTART_KEY).unwrap().1, "true");
        assert_eq!(store.source(AUTOSTART_KEY).unwrap(), "user");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn url_disable_only_revokes_persisted_consent() {
        let (directory, mut store) = store("url-disable");
        store.set(URL_CONSENT_KEY, "true").unwrap();

        set(&mut store, URL_CONSENT_KEY, "false").unwrap();

        assert!(!store.boolean(URL_CONSENT_KEY).unwrap());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn url_enable_does_not_require_os_registration() {
        let (directory, mut store) = store("url-enable");

        set(&mut store, URL_CONSENT_KEY, "true").unwrap();

        assert!(store.boolean(URL_CONSENT_KEY).unwrap());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn one_atomic_form_write_coordinates_autostart_and_plain_settings() {
        let (directory, mut store) = store("form");
        let autostart = FakeRegistration::new(RegistrationStatus::Disabled);
        let edits = vec![
            (AUTOSTART_KEY.to_owned(), "true".to_owned()),
            (URL_CONSENT_KEY.to_owned(), "true".to_owned()),
            ("capture.format".to_owned(), "webp".to_owned()),
        ];

        apply_with_registration(&mut store, &edits, Some("true"), Some(&autostart)).unwrap();

        assert_eq!(autostart.status().unwrap(), RegistrationStatus::Enabled);
        assert!(store.boolean(URL_CONSENT_KEY).unwrap());
        assert_eq!(store.get("capture.format").unwrap().1, "webp");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn repeated_batch_keys_use_the_last_value_for_native_state() {
        let edits = vec![
            (AUTOSTART_KEY.to_owned(), "true".to_owned()),
            (AUTOSTART_KEY.to_owned(), "false".to_owned()),
        ];

        assert_eq!(edit_value(&edits, AUTOSTART_KEY), Some("false"));
    }

    #[test]
    fn a_platform_failure_does_not_persist_the_requested_value() {
        let (directory, mut store) = store("platform-failure");
        let registration = FakeRegistration::new(RegistrationStatus::Disabled);
        registration.fail.set(true);

        let error = set_autostart_with(&mut store, "true", &registration).expect_err("must fail");

        assert_eq!(error.exit(), crate::exit::Exit::Platform);
        assert_eq!(store.get(AUTOSTART_KEY).unwrap().1, "false");
        assert_eq!(store.source(AUTOSTART_KEY).unwrap(), "default");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn persistence_failure_restores_the_actual_previous_state() {
        let (directory, mut store) = store("storage-rollback");
        fs::remove_dir(&directory).unwrap();
        fs::write(&directory, b"blocks settings parent").unwrap();
        let registration = FakeRegistration::new(RegistrationStatus::Disabled);

        let error = set_autostart_with(&mut store, "true", &registration).expect_err("must fail");

        assert_eq!(error.exit(), crate::exit::Exit::Storage);
        assert_eq!(registration.status().unwrap(), RegistrationStatus::Disabled);
        let _ = fs::remove_file(directory);
    }

    #[test]
    fn clear_os_state_reconciles_a_stale_persisted_toggle() {
        let (directory, mut store) = store("reconcile");
        store.set(AUTOSTART_KEY, "true").unwrap();
        let registration = FakeRegistration::new(RegistrationStatus::Disabled);

        let status = reconcile_autostart_with(&mut store, &registration).unwrap();

        assert_eq!(status, RegistrationStatus::Disabled);
        assert!(!store.boolean(AUTOSTART_KEY).unwrap());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn drift_is_reported_without_claiming_either_boolean_state() {
        let (directory, mut store) = store("drift");
        let registration = FakeRegistration::new(RegistrationStatus::Drifted);

        let status = reconcile_autostart_with(&mut store, &registration).unwrap();

        assert_eq!(status, RegistrationStatus::Drifted);
        assert_eq!(store.source(AUTOSTART_KEY).unwrap(), "default");
        let _ = fs::remove_dir_all(directory);
    }
}
