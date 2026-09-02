//! Small-window lifecycle stress for the macOS eframe/AppKit ownership boundary.
//!
//! This helper has no Scrozz application state, capture backend, tray, hotkeys,
//! fullscreen window, or recording surface. It repeatedly creates and closes
//! ordinary child viewports while a tiny root window exercises the retained
//! native adapter that production uses without changing winit's runtime class.

#[cfg(target_os = "macos")]
fn main() -> eframe::Result {
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use scrozz_shell::OverlayBehavior;

    const CYCLES: usize = 500;

    struct StressApp {
        native: Rc<RefCell<Option<scrozz_shell::macos::overlay::MacOverlay>>>,
        native_class: String,
        completed: Arc<AtomicUsize>,
        cycle: usize,
    }

    impl StressApp {
        fn return_root_to_winit(&self) {
            if let Some(mut native) = self.native.borrow_mut().take() {
                native
                    .restore_native_class()
                    .expect("the root window must return to winit before teardown");
                assert_eq!(
                    native.diagnostics().expect("root diagnostics").class_name,
                    self.native_class,
                    "Scrozz must never change the winit-owned runtime class"
                );
            }
        }
    }

    impl eframe::App for StressApp {
        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            use std::sync::atomic::Ordering;

            if self.completed.load(Ordering::Acquire) > self.cycle {
                self.cycle += 1;
            }
            if self.cycle == CYCLES {
                self.return_root_to_winit();
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }

            if let Some(native) = self.native.borrow_mut().as_mut() {
                native
                    .apply(&OverlayBehavior::capture_card())
                    .expect("apply tiny card behavior");
                assert!(
                    !native
                        .diagnostics()
                        .expect("input diagnostics")
                        .ignores_mouse_events,
                    "a visible capture-card root must never remain click-through"
                );
                native.set_visible(false).expect("order tiny root out");
                native
                    .apply(&OverlayBehavior::capture_card())
                    .expect("restore card input before reveal");
                native.set_visible(true).expect("reuse tiny root");
                let diagnostics = native.diagnostics().expect("root diagnostics");
                assert_eq!(
                    diagnostics.class_name, self.native_class,
                    "native class changed during lifecycle stress"
                );
                assert!(
                    !diagnostics.ignores_mouse_events,
                    "a revealed capture-card root must accept input"
                );
            }

            let cycle = self.cycle;
            let role = ["settings", "editor-ime", "permission", "card"][cycle % 4];
            let completed = Arc::clone(&self.completed);
            let viewport = egui::ViewportId::from_hash_of(("scrozz-lifecycle-stress", cycle));
            let builder = egui::ViewportBuilder::default()
                .with_title(format!("Scrozz lifecycle {role} {cycle}"))
                .with_inner_size([240.0, 120.0])
                .with_decorations(true)
                .with_resizable(false);
            ui.ctx()
                .show_viewport_deferred(viewport, builder, move |child, _class| {
                    egui::CentralPanel::default().show(child, |ui| {
                        ui.label(role);
                        let mut marked_text_target = String::from("IME teardown target");
                        ui.add(egui::TextEdit::singleline(&mut marked_text_target));
                    });
                    child.send_viewport_cmd(egui::ViewportCommand::Close);
                    completed.store(cycle + 1, Ordering::Release);
                });
            ui.label(format!("ordinary child lifecycle {}/{CYCLES}", cycle + 1));
            ui.ctx().request_repaint();
        }

        fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
            self.return_root_to_winit();
            assert_eq!(
                self.completed.load(Ordering::Acquire),
                CYCLES,
                "the helper exited before all lifecycle cycles completed"
            );
            println!("macOS lifecycle stress completed: {CYCLES} cycles");
        }
    }

    let native = Rc::new(RefCell::new(None));
    let creator_native = Rc::clone(&native);
    let completed = Arc::new(AtomicUsize::new(0));
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([260.0, 150.0])
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "Scrozz Lifecycle Stress",
        options,
        Box::new(move |cc| {
            let handle = cc.window_handle().expect("eframe root window handle");
            let RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
                unreachable!("the macOS helper must receive an AppKit window");
            };
            let mut overlay = unsafe {
                scrozz_shell::macos::overlay::MacOverlay::from_ns_view(
                    appkit.ns_view.as_ptr().cast(),
                )
            }
            .expect("adopt the eframe root window");
            let native_class = overlay
                .diagnostics()
                .expect("initial root diagnostics")
                .class_name;
            let report = overlay
                .apply(&OverlayBehavior::capture_card())
                .expect("configure the tiny root");
            assert!(
                !report.non_activating,
                "stable winit must retain its ordinary NSWindow identity"
            );
            assert_eq!(
                overlay
                    .diagnostics()
                    .expect("configured root diagnostics")
                    .class_name,
                native_class
            );
            *creator_native.borrow_mut() = Some(overlay);
            Ok(Box::new(StressApp {
                native,
                native_class,
                completed,
                cycle: 0,
            }))
        }),
    )
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos_lifecycle_stress only runs on macOS");
}
