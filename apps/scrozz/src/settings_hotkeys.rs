//! Settings-time shortcut conflict detection.
//!
//! This belongs in the app rather than the schema or UI: it is the only layer
//! that can see persisted settings and `scrozz-shell`'s platform conflict table.

use std::collections::BTreeMap;

use scrozz_shell::{Accelerator, Conflict, GlobalHotkeys, Hotkey, HotkeyManager as _};

use crate::{
    fault::CliResult,
    settings::{Kind, SETTINGS},
    settings_store::SettingsStore,
    settings_support,
};

/// Checks a proposed shortcut against the OS table and every other Scrozz
/// shortcut without registering anything globally.
///
/// # Errors
///
/// Returns an invalid-request error when a stored or proposed accelerator does
/// not parse.
pub fn check_edit(store: &SettingsStore, key: &str, value: &str) -> CliResult<Option<Conflict>> {
    let setting = crate::settings::lookup(key)?;
    if setting.kind != Kind::Accelerator {
        return Ok(None);
    }
    if !settings_support::actionable_shortcut(key) {
        return Ok(None);
    }

    let mut hotkeys = GlobalHotkeys::detached_for_conflict_checks();
    for existing in SETTINGS.iter().filter(|existing| {
        existing.kind == Kind::Accelerator
            && existing.key != key
            && settings_support::actionable_shortcut(existing.key)
    }) {
        let (accelerator, _) = store.resolve(existing);
        let parsed = Accelerator::parse(accelerator)?;
        // `register` would correctly refuse a system-owned default, but it would
        // also prevent the remaining Scrozz bindings from being seeded. The
        // proposed value is checked against that same table below.
        if parsed.system_owner().is_some() {
            continue;
        }
        hotkeys.register(
            &Hotkey {
                accelerator: accelerator.to_owned(),
            },
            existing.key,
        )?;
    }

    hotkeys
        .check(&Hotkey {
            accelerator: value.to_owned(),
        })
        .map_err(Into::into)
}

/// Checks a complete shortcut form, including conflicts between unsaved edits.
///
/// The returned map contains only conflicting keys. Invalid accelerators are
/// returned as errors so the caller can attach the parser's message to the
/// relevant row before calling this function.
pub fn check_all(values: &[(String, String)]) -> CliResult<BTreeMap<String, Conflict>> {
    let parsed = values
        .iter()
        .map(|(key, value)| Accelerator::parse(value).map(|accelerator| (key, value, accelerator)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut conflicts = BTreeMap::new();

    for (index, (key, _, accelerator)) in parsed.iter().enumerate() {
        if let Some((other_key, _, _)) = parsed
            .iter()
            .enumerate()
            .find(|(other_index, (_, _, other))| *other_index != index && *other == *accelerator)
            .map(|(_, entry)| entry)
        {
            conflicts.insert(
                (*key).clone(),
                Conflict::AlreadyBound {
                    action: (*other_key).clone(),
                },
            );
        }
    }

    let mut hotkeys = GlobalHotkeys::detached_for_conflict_checks();
    for (key, value, _) in &parsed {
        if conflicts.contains_key(*key) {
            continue;
        }
        let hotkey = Hotkey {
            accelerator: (*value).clone(),
        };
        if let Some(conflict) = hotkeys.check(&hotkey)? {
            conflicts.insert((*key).clone(), conflict);
        } else {
            hotkeys.register(&hotkey, key)?;
        }
    }

    Ok(conflicts)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;

    fn store(name: &str) -> (std::path::PathBuf, SettingsStore) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "scrozz-hotkey-settings-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        let store = SettingsStore::open(directory.join("settings.json")).unwrap();
        (directory, store)
    }

    #[test]
    fn non_shortcut_settings_have_no_shortcut_conflict() {
        let (directory, store) = store("non-shortcut");
        assert_eq!(check_edit(&store, "capture.format", "webp").unwrap(), None);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn shortcuts_without_runtime_actions_do_not_create_false_conflicts() {
        let (directory, mut store) = store("inert-shortcut");
        store.set("hotkey.capture-region", "Ctrl+Alt+P").unwrap();
        assert_eq!(
            check_edit(&store, "hotkey.record-start", "Ctrl+Alt+P").unwrap(),
            None
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn another_scrozz_binding_is_reported_by_the_real_manager() {
        let (directory, mut store) = store("duplicate");
        store.set("hotkey.capture-display", "Ctrl+Alt+P").unwrap();
        let conflict = check_edit(&store, "hotkey.capture-region", "Ctrl+Alt+P").unwrap();
        assert_eq!(
            conflict,
            Some(Conflict::AlreadyBound {
                action: "hotkey.capture-display".to_owned()
            })
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn the_platform_reserved_table_is_used() {
        let Some(reserved) = scrozz_shell::hotkey::reserved_shortcuts().first() else {
            return;
        };
        let (directory, store) = store("reserved");
        let conflict = check_edit(&store, "hotkey.capture-region", reserved.accelerator).unwrap();
        assert!(matches!(conflict, Some(Conflict::SystemReserved { .. })));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn complete_form_marks_both_sides_of_an_unsaved_duplicate() {
        let values = vec![
            ("hotkey.capture-region".to_owned(), "Ctrl+Alt+P".to_owned()),
            ("hotkey.capture-window".to_owned(), "Alt+Ctrl+P".to_owned()),
        ];
        let conflicts = check_all(&values).unwrap();
        assert_eq!(conflicts.len(), 2);
        assert!(matches!(
            conflicts["hotkey.capture-region"],
            Conflict::AlreadyBound { .. }
        ));
        assert!(matches!(
            conflicts["hotkey.capture-window"],
            Conflict::AlreadyBound { .. }
        ));
    }
}
