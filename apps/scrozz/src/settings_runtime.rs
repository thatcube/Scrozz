//! Settings changes that also touch live operating-system integration.
//!
//! Persistence alone is enough for most keys. Launch at login is different: the
//! JSON value and the platform registration must agree. These helpers update the
//! OS first, write the document second, and restore the old OS state if that
//! write fails.

use scrozz_core::Error as CoreError;
use scrozz_shell::{LaunchAtLogin, SystemLaunchAtLogin};

use crate::{
    fault::{CliError, CliResult},
    settings_hotkeys,
    settings_store::SettingsStore,
};

/// The launch-agent/registry identity.
///
/// The system-integration branch exposes this same value as
/// `scrozz_core::identity::AUTOSTART_LABEL`; keeping the call site parameterized
/// means that branch can replace this constant without changing scrozz-shell.
pub const AUTOSTART_LABEL: &str = "com.thatcube.Scrozz";

/// Validates and applies one setting.
///
/// # Errors
///
/// Returns a validation/conflict error before changing anything, a platform
/// error when launch-at-login registration fails, or a storage error when the
/// settings document cannot be replaced.
pub fn set(store: &mut SettingsStore, key: &str, value: &str) -> CliResult<()> {
    if let Some(conflict) = settings_hotkeys::check_edit(store, key, value)? {
        return Err(CliError::usage(format!(
            "{key} cannot use {value:?}: {conflict}"
        )));
    }

    if key != "system.launch-at-login" {
        store.set(key, value)?;
        return Ok(());
    }

    let login = system_login()?;
    set_with_login(store, key, value, &login)
}

/// Validates and atomically persists a complete settings-form edit.
///
/// Launch-at-login is changed before the document replacement and restored if
/// that replacement fails, so the OS and JSON file do not knowingly diverge.
pub fn apply(store: &mut SettingsStore, edits: &[(String, String)]) -> CliResult<()> {
    let login_edit = edits
        .iter()
        .find(|(key, _)| key == "system.launch-at-login");
    if let Some((key, value)) = login_edit {
        let login = system_login()?;
        return apply_with_login(store, edits, key, value, &login);
    }
    store.apply(edits)
}

/// Resets one setting and its associated platform state.
///
/// # Errors
///
/// Returns a platform or storage error without leaving the two states knowingly
/// divergent.
pub fn reset(store: &mut SettingsStore, key: &str) -> CliResult<bool> {
    if key != "system.launch-at-login" {
        return store.reset(key);
    }

    let previous = store.get(key)?.1.to_owned();
    let login = system_login()?;
    sync_login(&login, false)?;
    match store.reset(key) {
        Ok(changed) => Ok(changed),
        Err(error) => {
            restore_login(&login, &previous, error)?;
            unreachable!("restore_login always returns the original error")
        }
    }
}

/// Resets every setting and disables launch at login.
///
/// # Errors
///
/// Returns a platform or storage error without leaving launch-at-login state
/// knowingly divergent.
pub fn reset_all(store: &mut SettingsStore) -> CliResult<usize> {
    let previous = store.get("system.launch-at-login")?.1.to_owned();
    let login = system_login()?;
    sync_login(&login, false)?;
    match store.reset_all() {
        Ok(count) => Ok(count),
        Err(error) => {
            restore_login(&login, &previous, error)?;
            unreachable!("restore_login always returns the original error")
        }
    }
}

fn system_login() -> CliResult<SystemLaunchAtLogin> {
    Ok(SystemLaunchAtLogin::new(
        AUTOSTART_LABEL,
        std::env::current_exe()?,
    ))
}

fn set_with_login(
    store: &mut SettingsStore,
    key: &str,
    value: &str,
    login: &dyn LaunchAtLogin,
) -> CliResult<()> {
    let setting = crate::settings::lookup(key)?;
    setting.validate(value)?;
    let enabled = parse_bool(key, value)?;
    let previous = store.get(key)?.1.to_owned();

    sync_login(login, enabled)?;
    if let Err(error) = store.set(key, value) {
        restore_login(login, &previous, error)?;
    }
    Ok(())
}

fn apply_with_login(
    store: &mut SettingsStore,
    edits: &[(String, String)],
    key: &str,
    value: &str,
    login: &dyn LaunchAtLogin,
) -> CliResult<()> {
    for (edit_key, edit_value) in edits {
        crate::settings::lookup(edit_key)?.validate(edit_value)?;
    }
    let enabled = parse_bool(key, value)?;
    let previous = store.get(key)?.1.to_owned();

    sync_login(login, enabled)?;
    if let Err(error) = store.apply(edits) {
        restore_login(login, &previous, error)?;
    }
    Ok(())
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

fn sync_login(login: &dyn LaunchAtLogin, enabled: bool) -> CliResult<()> {
    if enabled {
        login.enable()
    } else {
        login.disable()
    }
    .map_err(CliError::from)
}

fn restore_login(login: &dyn LaunchAtLogin, previous: &str, original: CliError) -> CliResult<()> {
    let rollback = sync_login(login, parse_bool("system.launch-at-login", previous)?);
    match rollback {
        Ok(()) => Err(original),
        Err(rollback) => Err(CliError::Core(CoreError::Storage(format!(
            "{original}; restoring the previous launch-at-login registration also failed: {rollback}"
        )))),
    }
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

    struct FakeLogin {
        enabled: Cell<bool>,
        fail: Cell<bool>,
    }

    impl FakeLogin {
        fn new(enabled: bool) -> Self {
            Self {
                enabled: Cell::new(enabled),
                fail: Cell::new(false),
            }
        }
    }

    impl LaunchAtLogin for FakeLogin {
        fn is_enabled(&self) -> Result<bool> {
            Ok(self.enabled.get())
        }

        fn enable(&self) -> Result<()> {
            if self.fail.get() {
                return Err(Error::Platform("injected login failure".to_owned()));
            }
            self.enabled.set(true);
            Ok(())
        }

        fn disable(&self) -> Result<()> {
            if self.fail.get() {
                return Err(Error::Platform("injected login failure".to_owned()));
            }
            self.enabled.set(false);
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
        let (directory, mut store) = store("login");
        let login = FakeLogin::new(false);

        set_with_login(&mut store, "system.launch-at-login", "true", &login).unwrap();
        assert!(login.is_enabled().unwrap());
        assert_eq!(store.get("system.launch-at-login").unwrap().1, "true");
        assert_eq!(store.source("system.launch-at-login").unwrap(), "user");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn form_edits_are_written_together_with_launch_at_login() {
        let (directory, mut store) = store("form-login");
        let login = FakeLogin::new(false);
        let edits = vec![
            ("system.launch-at-login".to_owned(), "true".to_owned()),
            ("capture.format".to_owned(), "webp".to_owned()),
        ];

        apply_with_login(&mut store, &edits, "system.launch-at-login", "true", &login).unwrap();
        assert!(login.is_enabled().unwrap());
        assert_eq!(store.get("capture.format").unwrap().1, "webp");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn a_platform_failure_does_not_persist_the_requested_value() {
        let (directory, mut store) = store("platform-failure");
        let login = FakeLogin::new(false);
        login.fail.set(true);

        let error =
            set_with_login(&mut store, "system.launch-at-login", "true", &login).unwrap_err();
        assert_eq!(error.exit(), crate::exit::Exit::Platform);
        assert_eq!(store.get("system.launch-at-login").unwrap().1, "false");
        assert_eq!(store.source("system.launch-at-login").unwrap(), "default");
        let _ = fs::remove_dir_all(directory);
    }
}
