//! Explicit eframe/AppKit lifecycle harness; never runs as part of `cargo test`.

#[cfg(target_os = "macos")]
use std::{
    error::Error,
    fs::{self, OpenOptions},
    io::Write as _,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "macos")]
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
#[cfg(target_os = "macos")]
use objc2_foundation::{MainThreadMarker, NSProcessInfo};

#[cfg(target_os = "macos")]
const SETTINGS_CLOSE_SETTLE: Duration = Duration::from_millis(750);

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("the Settings lifecycle harness is macOS-only");
    std::process::exit(2);
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(error) = run() {
        eprintln!("Settings lifecycle harness failed: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn run() -> Result<(), Box<dyn Error>> {
    if std::env::var("SCROZZ_RUN_SETTINGS_LIFECYCLE_HARNESS").as_deref() != Ok("1") {
        return Err(invalid(
            "set SCROZZ_RUN_SETTINGS_LIFECYCLE_HARNESS=1 for this explicit native test",
        ));
    }
    let report_path = std::env::var_os("SCROZZ_LIFECYCLE_HARNESS_REPORT")
        .map(PathBuf::from)
        .ok_or_else(|| invalid("set SCROZZ_LIFECYCLE_HARNESS_REPORT to a durable output path"))?;
    let journal = HarnessJournal::new(report_path);
    journal
        .persist("starting", None)
        .map_err(|error| invalid(&error))?;

    let mtm = MainThreadMarker::new()
        .ok_or_else(|| invalid("the harness must start on the process main thread"))?;
    let app = NSApplication::sharedApplication(mtm);
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    NSProcessInfo::processInfo().setAutomaticTerminationSupportEnabled(true);

    let result = Arc::new(Mutex::new(None));
    let sink = Arc::clone(&result);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Scrozz Settings Lifecycle Harness Root")
            .with_inner_size([1.0, 1.0])
            .with_position([-100_000.0, -100_000.0])
            // Model eframe's production bootstrap explicitly: the off-screen
            // root is ordered in once, then the first UI pass orders it out.
            .with_visible(true),
        ..Default::default()
    };
    eframe::run_native(
        "Scrozz Settings Lifecycle Harness",
        options,
        Box::new(move |_cc| {
            let _ = scrozz_shell::macos::display::displays()?;
            let baseline = scrozz_shell::macos::activity::snapshot();
            Ok(Box::new(Harness {
                lease_acquired_at: None,
                automatic_termination: None,
                baseline,
                pointer_samples: 0,
                settings_open: false,
                settings_was_opened: false,
                settings_was_closed: false,
                settings_closed_at: None,
                initial_root_ordered_out: false,
                root_bootstrap_visible: false,
                root_bootstrap_pending: false,
                parked_root_ordered_out: false,
                idle_baseline: None,
                idle_wake_scheduled: false,
                exit_requested: false,
                failure: None,
                journal,
                sink,
            }))
        }),
    )?;

    let outcome = result
        .lock()
        .map_err(|_| invalid("the harness result lock was poisoned"))?
        .take()
        .ok_or_else(|| invalid("the harness exited without a lifecycle result"))?;
    outcome.map_err(|message| invalid(&message))?;
    let final_report = fs::read_to_string(
        std::env::var_os("SCROZZ_LIFECYCLE_HARNESS_REPORT")
            .map(PathBuf::from)
            .ok_or_else(|| invalid("the harness report path disappeared"))?,
    )?;
    let final_report: serde_json::Value = serde_json::from_str(&final_report)?;
    if final_report["state"] != "completed"
        || final_report["exit_reason"] != "explicit-harness-close"
    {
        return Err(invalid("the durable harness report is incomplete"));
    }
    println!("Settings lifecycle harness passed");
    Ok(())
}

#[cfg(target_os = "macos")]
struct HarnessJournal {
    path: PathBuf,
    temporary: PathBuf,
    pre_lease_tal: Option<bool>,
    lease_acquired: bool,
    settings_opened: bool,
    settings_closed: bool,
    idle_display_enumerations: Option<u64>,
    idle_root_redraws: Option<u64>,
}

#[cfg(target_os = "macos")]
impl HarnessJournal {
    fn new(path: PathBuf) -> Self {
        let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
        Self {
            path,
            temporary,
            pre_lease_tal: None,
            lease_acquired: false,
            settings_opened: false,
            settings_closed: false,
            idle_display_enumerations: None,
            idle_root_redraws: None,
        }
    }

    fn persist(&self, state: &str, exit_reason: Option<&str>) -> Result<(), String> {
        let activity = scrozz_shell::macos::activity::snapshot();
        let updated_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("system clock precedes Unix epoch: {error}"))?
            .as_millis();
        let document = serde_json::json!({
            "schema": 1,
            "pid": std::process::id(),
            "state": state,
            "updated_unix_ms": updated_unix_ms,
            "exit_reason": exit_reason,
            "pre_lease_tal": self.pre_lease_tal,
            "lease_acquired": self.lease_acquired,
            "settings_opened": self.settings_opened,
            "settings_closed": self.settings_closed,
            "idle_display_enumerations": self.idle_display_enumerations,
            "idle_root_redraws": self.idle_root_redraws,
            "native_activity": {
                "display_enumerations": activity.display_enumerations,
                "pointer_samples": activity.pointer_samples,
                "root_redraws": activity.root_redraws,
                "automatic_termination_disables": activity.automatic_termination_disables,
                "automatic_termination_enables": activity.automatic_termination_enables,
            },
        });
        let bytes = serde_json::to_vec_pretty(&document)
            .map_err(|error| format!("could not encode harness report: {error}"))?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| format!("harness report path has no parent: {}", self.path.display()))?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create harness report directory {}: {error}",
                parent.display()
            )
        })?;
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&self.temporary)
            .map_err(|error| {
                format!(
                    "could not open harness report {}: {error}",
                    self.temporary.display()
                )
            })?;
        file.write_all(&bytes).map_err(|error| {
            format!(
                "could not write harness report {}: {error}",
                self.temporary.display()
            )
        })?;
        file.sync_all().map_err(|error| {
            format!(
                "could not sync harness report {}: {error}",
                self.temporary.display()
            )
        })?;
        drop(file);
        fs::rename(&self.temporary, &self.path).map_err(|error| {
            format!(
                "could not replace harness report {}: {error}",
                self.path.display()
            )
        })?;
        eprintln!("HARNESS_EVENT unix_ms={updated_unix_ms} state={state}");
        Ok(())
    }
}

#[cfg(target_os = "macos")]
struct Harness {
    lease_acquired_at: Option<Instant>,
    automatic_termination: Option<scrozz_shell::macos::termination::AutomaticTerminationGuard>,
    baseline: scrozz_shell::macos::activity::NativeActivitySnapshot,
    pointer_samples: u64,
    settings_open: bool,
    settings_was_opened: bool,
    settings_was_closed: bool,
    settings_closed_at: Option<Instant>,
    initial_root_ordered_out: bool,
    root_bootstrap_visible: bool,
    root_bootstrap_pending: bool,
    parked_root_ordered_out: bool,
    idle_baseline: Option<scrozz_shell::macos::activity::NativeActivitySnapshot>,
    idle_wake_scheduled: bool,
    exit_requested: bool,
    failure: Option<String>,
    journal: HarnessJournal,
    sink: Arc<Mutex<Option<Result<(), String>>>>,
}

#[cfg(target_os = "macos")]
impl Harness {
    fn fail(&mut self, ctx: &egui::Context, message: impl Into<String>) {
        if self.failure.is_none() {
            let message = message.into();
            self.failure = Some(message.clone());
            if let Err(report_error) = self.journal.persist("failed", Some(&message)) {
                self.failure = Some(format!("{message}; {report_error}"));
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn tal_state() -> Result<Option<bool>, String> {
        let output = Command::new("/usr/bin/lsappinfo")
            .args([
                "info",
                "-only",
                "ApplicationWouldBeTerminatedByTALKey",
                &std::process::id().to_string(),
            ])
            .output()
            .map_err(|error| format!("lsappinfo could not read TAL state: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "lsappinfo failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("\"LSApplicationWouldBeTerminatedByTALKey\"=true")
            || stdout.contains("\"LSApplicationWouldBeTerminatedByTALKey\"=1")
        {
            Ok(Some(true))
        } else if stdout.contains("\"LSApplicationWouldBeTerminatedByTALKey\"=false")
            || stdout.contains("\"LSApplicationWouldBeTerminatedByTALKey\"=0")
        {
            Ok(Some(false))
        } else if stdout.contains("\"LSApplicationWouldBeTerminatedByTALKey\"=[ NULL ]") {
            Ok(None)
        } else {
            Err(format!(
                "lsappinfo returned no TAL state: {}",
                stdout.trim()
            ))
        }
    }
}

#[cfg(target_os = "macos")]
impl eframe::App for Harness {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.initial_root_ordered_out {
            ctx.request_repaint_after(Duration::from_millis(250));
            return;
        }

        if self.automatic_termination.is_none() {
            match Self::tal_state() {
                Ok(Some(state)) => self.journal.pre_lease_tal = Some(state),
                Ok(None) => {
                    if let Err(error) = self.journal.persist("awaiting-appkit-tal-key", None) {
                        self.fail(ctx, error);
                        return;
                    }
                    ctx.request_repaint_after(Duration::from_millis(100));
                    return;
                }
                Err(error) => {
                    self.fail(ctx, error);
                    return;
                }
            }
            if let Err(error) = self.journal.persist("appkit-bootstrap-completed", None) {
                self.fail(ctx, error);
                return;
            }
            match scrozz_shell::macos::termination::AutomaticTerminationGuard::acquire() {
                Ok(guard) => {
                    self.automatic_termination = Some(guard);
                    self.lease_acquired_at = Some(Instant::now());
                    self.journal.lease_acquired = true;
                    if let Err(error) = self.journal.persist("lease-acquired", None) {
                        self.fail(ctx, error);
                        return;
                    }
                }
                Err(error) => {
                    self.fail(ctx, error.to_string());
                    return;
                }
            }
        }

        if let Err(error) = scrozz_shell::macos::display::pointer_location() {
            self.fail(ctx, error.to_string());
            return;
        }
        self.pointer_samples += 1;

        if !self.root_bootstrap_visible {
            self.settings_open = true;
            self.root_bootstrap_visible = true;
            self.root_bootstrap_pending = true;
            if let Err(error) = self.journal.persist("root-bootstrap-visible", None) {
                self.fail(ctx, error);
                return;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.request_repaint();
            return;
        }
        let elapsed = self
            .lease_acquired_at
            .map_or(Duration::ZERO, |acquired| acquired.elapsed());
        if elapsed >= Duration::from_millis(750) && self.settings_open {
            self.settings_open = false;
            self.settings_was_closed = true;
            self.settings_closed_at = Some(Instant::now());
            self.journal.settings_closed = true;
            if let Err(error) = self.journal.persist("settings-closed", None) {
                self.fail(ctx, error);
                return;
            }
            ctx.send_viewport_cmd_to(
                egui::ViewportId::from_hash_of("scrozz-settings-lifecycle-harness"),
                egui::ViewportCommand::Close,
            );
            ctx.request_repaint_after(SETTINGS_CLOSE_SETTLE);
            return;
        }
        if self.settings_was_closed && self.idle_baseline.is_none() {
            let remaining = SETTINGS_CLOSE_SETTLE.saturating_sub(
                self.settings_closed_at
                    .expect("a closed Settings viewport has a timestamp")
                    .elapsed(),
            );
            if !remaining.is_zero() {
                ctx.request_repaint_after(remaining);
                return;
            }
            self.idle_baseline = Some(scrozz_shell::macos::activity::snapshot());
            self.idle_wake_scheduled = true;
            if let Err(error) = self.journal.persist("idle-observation-started", None) {
                self.fail(ctx, error);
                return;
            }
            let repaint = ctx.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(1500));
                repaint.request_repaint();
            });
            return;
        }
        if elapsed >= Duration::from_millis(2500) {
            if Self::tal_state() != Ok(Some(false)) {
                self.fail(ctx, "TAL was not held at 0 through Settings open/close");
                return;
            }
            let snapshot = scrozz_shell::macos::activity::snapshot();
            let activity = snapshot.since(self.baseline);
            let idle = snapshot.since(
                self.idle_baseline
                    .expect("the idle baseline precedes final verification"),
            );
            self.journal.idle_display_enumerations = Some(idle.display_enumerations);
            self.journal.idle_root_redraws = Some(idle.root_redraws);
            if activity.display_enumerations != 0 {
                self.fail(
                    ctx,
                    format!(
                        "idle Settings frames enumerated displays {} times",
                        activity.display_enumerations
                    ),
                );
                return;
            }
            if activity.pointer_samples != self.pointer_samples {
                self.fail(
                    ctx,
                    format!(
                        "pointer activity mismatch: expected {}, recorded {}",
                        self.pointer_samples, activity.pointer_samples
                    ),
                );
                return;
            }
            if idle.root_redraws > 1 {
                self.fail(
                    ctx,
                    format!(
                        "hidden-root lifecycle produced {} redraws during the settled idle window",
                        idle.root_redraws
                    ),
                );
                return;
            }
            if activity.automatic_termination_disables != 1 {
                self.fail(
                    ctx,
                    format!(
                        "expected one post-bootstrap lease, recorded {}",
                        activity.automatic_termination_disables
                    ),
                );
                return;
            }
            if !self.settings_was_opened || !self.settings_was_closed {
                self.fail(ctx, "Settings child did not complete its open/close cycle");
                return;
            }
            self.exit_requested = true;
            if let Err(error) = self
                .journal
                .persist("verified", Some("explicit-harness-close"))
            {
                self.fail(ctx, error);
                return;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        if self.settings_open {
            ctx.request_repaint_after(Duration::from_millis(500));
        } else if !self.idle_wake_scheduled {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        scrozz_shell::macos::activity::record_root_redraw();
        if !self.initial_root_ordered_out {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.initial_root_ordered_out = true;
            if let Err(error) = self.journal.persist("initial-root-ordered-out", None) {
                self.fail(ui.ctx(), error);
            }
            return;
        }
        if self.root_bootstrap_pending {
            self.root_bootstrap_pending = false;
            if let Err(error) = self.journal.persist("root-bootstrap-committed", None) {
                self.fail(ui.ctx(), error);
            }
            ui.ctx().request_repaint();
            return;
        }
        if !self.settings_open {
            return;
        }
        let callback_called = std::cell::Cell::new(false);
        ui.ctx().show_viewport_immediate(
            egui::ViewportId::from_hash_of("scrozz-settings-lifecycle-harness"),
            egui::ViewportBuilder::default()
                .with_title("Scrozz Settings Lifecycle Harness")
                .with_inner_size([320.0, 160.0])
                .with_visible(true),
            |ui, _class| {
                callback_called.set(true);
                ui.heading("Settings lifecycle harness");
                ui.label("This purpose-built window closes automatically.");
            },
        );
        if !callback_called.get() {
            self.fail(
                ui.ctx(),
                "Settings immediate viewport callback was not invoked",
            );
            return;
        }
        if !self.settings_was_opened {
            self.settings_was_opened = true;
            self.journal.settings_opened = true;
            if let Err(error) = self.journal.persist("settings-opened", None) {
                self.fail(ui.ctx(), error);
                return;
            }
        }
        if !self.parked_root_ordered_out {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.parked_root_ordered_out = true;
            if let Err(error) = self
                .journal
                .persist("settings-registered-root-ordered-out", None)
            {
                self.fail(ui.ctx(), error);
            }
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.settings_open = false;
        if !self.exit_requested && self.failure.is_none() {
            self.failure = Some("native-event-loop-before-explicit-close".to_owned());
        }
        let pre_release_reason = self.failure.as_deref().unwrap_or("explicit-harness-close");
        if let Err(error) = self
            .journal
            .persist("on-exit-before-release", Some(pre_release_reason))
        {
            self.failure = Some(error);
        }
        if let Some(mut guard) = self.automatic_termination.take() {
            guard.release();
        }
        let activity = scrozz_shell::macos::activity::snapshot().since(self.baseline);
        let outcome = if let Some(failure) = self.failure.take() {
            Err(failure)
        } else if activity.automatic_termination_enables != 1 {
            Err(format!(
                "expected one balanced lease release, recorded {}",
                activity.automatic_termination_enables
            ))
        } else {
            Ok(())
        };
        let final_state = if outcome.is_ok() {
            "completed"
        } else {
            "failed"
        };
        let final_reason = outcome
            .as_ref()
            .err()
            .map_or("explicit-harness-close", String::as_str);
        let persist_result = self.journal.persist(final_state, Some(final_reason));
        let outcome = match (outcome, persist_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(error), Err(report_error)) => Err(format!("{error}; {report_error}")),
        };
        if let Ok(mut sink) = self.sink.lock() {
            *sink = Some(outcome);
        }
    }
}

#[cfg(target_os = "macos")]
fn invalid(message: &str) -> Box<dyn Error> {
    Box::new(std::io::Error::other(message.to_owned()))
}
