//! App shell: builds the window, applies the material, renders the current
//! surface, and — in `--shot` mode — screenshots the real window and quits.

use crate::icons::IconStore;
use crate::surfaces::{QuickVariant, Surface};
use crate::theme::{self, Palette};
use eframe::egui;
use egui::Rect;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub struct Config {
    pub surface: Surface,
    pub quick_variant: QuickVariant,
    pub theme_dark: bool,
    pub backdrop: bool,
    pub material: crate::vibrancy::Material,
    pub shot: Option<PathBuf>,
    pub window_pos: (f32, f32),
}

impl Config {
    pub fn interactive(&self) -> bool {
        self.shot.is_none()
    }
    /// Transparent margin around the card (leaves room for the drop shadow when
    /// we draw our own backdrop; zero when sitting on real OS glass).
    pub fn pad(&self) -> f32 {
        if self.backdrop {
            32.0
        } else {
            0.0
        }
    }
    /// The card/scene size for the configured surface (the Quick overlay varies
    /// by drag-state variant).
    pub fn card_size(&self) -> egui::Vec2 {
        match self.surface {
            Surface::Quick => self.quick_variant.scene(),
            s => s.size(),
        }
    }
    /// Window inner size for a shot: card + shadow margin.
    pub fn shot_window_size(&self) -> egui::Vec2 {
        self.card_size() + egui::vec2(self.pad() * 2.0, self.pad() * 2.0)
    }
}

pub struct SpikeApp {
    cfg: Config,
    icons: IconStore,
    pub material: String,
    live_surface: Surface,
    live_variant: QuickVariant,
    live_dark: bool,
    live_backdrop: bool,
    frame: u64,
    captured: bool,
    stack: crate::stack::Stack,
    tuner_open: bool,
}

impl SpikeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, cfg: Config) -> Self {
        if cfg.shot.is_some() {
            crate::paint::set_screenshot(true);
        }
        theme::install_fonts(&cc.egui_ctx);
        let pal = if cfg.theme_dark { Palette::dark() } else { Palette::light() };
        theme::install_style(&cc.egui_ctx, &pal);
        let icons = IconStore::new(&cc.egui_ctx);
        let material = crate::vibrancy::apply(cc, cfg.material, theme::R_CARD as f64);
        eprintln!("material: {material}");

        Self {
            live_surface: cfg.surface,
            // Interactively we always open on the live, animated stack; a
            // `--shot` run keeps whatever static variant was asked for.
            live_variant: if cfg.interactive() && cfg.surface == Surface::Quick {
                QuickVariant::Live
            } else {
                cfg.quick_variant
            },
            live_dark: cfg.theme_dark,
            live_backdrop: cfg.backdrop,
            cfg,
            icons,
            material,
            frame: 0,
            captured: false,
            stack: crate::stack::Stack::new(),
            tuner_open: false,
        }
    }

    fn palette(&self) -> Palette {
        let mut pal = if self.live_dark { Palette::dark() } else { Palette::light() };
        pal.over_material = if self.cfg.interactive() {
            !self.live_backdrop
        } else {
            self.cfg.material != crate::vibrancy::Material::None
        };
        pal
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        let mut spawn = false;
        let mut dismiss = false;
        let mut replay = false;
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Num1) {
                self.live_surface = Surface::Quick;
                self.live_variant = QuickVariant::Live;
            }
            if i.key_pressed(egui::Key::Num2) {
                self.live_surface = Surface::Menu;
            }
            if i.key_pressed(egui::Key::Num3) {
                self.live_surface = Surface::Annotate;
            }
            if i.key_pressed(egui::Key::Num4) {
                self.live_surface = Surface::Onboard;
            }
            if i.key_pressed(egui::Key::V) {
                // Cycle the Quick overlay's state: the live stack first, then
                // the three approved stills.
                self.live_surface = Surface::Quick;
                self.live_variant = match self.live_variant {
                    QuickVariant::Live => QuickVariant::Stack,
                    QuickVariant::Stack => QuickVariant::Swipe,
                    QuickVariant::Swipe => QuickVariant::Drag,
                    QuickVariant::Drag => QuickVariant::Live,
                };
            }
            if i.key_pressed(egui::Key::L) {
                self.live_dark = !self.live_dark;
            }
            if i.key_pressed(egui::Key::G) {
                self.live_backdrop = !self.live_backdrop;
            }
            if i.key_pressed(egui::Key::M) {
                self.tuner_open = !self.tuner_open;
            }
            spawn = i.key_pressed(egui::Key::N);
            dismiss = i.key_pressed(egui::Key::Backspace) || i.key_pressed(egui::Key::Delete);
            replay = i.key_pressed(egui::Key::R);
            if i.key_pressed(egui::Key::Escape) || i.key_pressed(egui::Key::Q) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
        // A new capture always yanks you to the surface that shows it, so `N`
        // does something visible no matter where you are.
        if spawn {
            self.live_surface = Surface::Quick;
            self.live_variant = QuickVariant::Live;
            self.stack.spawn();
        }
        if dismiss {
            self.stack.dismiss_top();
        }
        if replay {
            self.stack.replay();
        }
        // Re-install style when theme flips (selection colors etc.).
        let pal = if self.live_dark { Palette::dark() } else { Palette::light() };
        theme::install_style(ctx, &pal);
    }

    fn capture(&self, ctx: &egui::Context, path: &PathBuf) {
        let rect = ctx.input(|i| {
            let vp = i.viewport();
            vp.inner_rect.or(vp.outer_rect)
        });
        let (x, y, w, h) = if let Some(r) = rect {
            (r.min.x, r.min.y, r.width(), r.height())
        } else {
            let s = self.cfg.shot_window_size();
            (self.cfg.window_pos.0, self.cfg.window_pos.1, s.x, s.y)
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Let the very last frame composite (material included) before grabbing.
        std::thread::sleep(Duration::from_millis(140));
        let region = format!("-R{:.0},{:.0},{:.0},{:.0}", x, y, w, h);
        let out = std::process::Command::new("screencapture")
            .arg("-x")
            .arg(region)
            .arg(path)
            .status();
        match out {
            Ok(s) if s.success() => eprintln!("captured -> {}", path.display()),
            Ok(s) => eprintln!("screencapture exited with {s}"),
            Err(e) => eprintln!("screencapture failed: {e}"),
        }
    }
}

impl eframe::App for SpikeApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.cfg.interactive() {
            self.handle_keys(&ctx);
        }
        let pal = self.palette();
        let screen = ui.max_rect();

        if self.live_backdrop {
            crate::paint::wallpaper(ui.painter(), screen, pal.is_dark);
        }

        let card_size = match self.live_surface {
            Surface::Quick => self.live_variant.scene(),
            s => s.size(),
        };
        let card = if self.cfg.interactive() {
            Rect::from_center_size(screen.center(), card_size)
        } else {
            screen.shrink(self.cfg.pad())
        };

        // `busy` is the whole repaint-scheduling story for hand-rolled physics:
        // egui's own `animate_*` helpers request their own repaints and stop
        // when they settle, but springs and coasting cards do not. We ask for
        // exactly one more frame while something is moving, and nothing at all
        // once everything is at rest — so the app is genuinely idle on the
        // desktop instead of pinning a core.
        let mut busy = false;

        match self.live_surface {
            Surface::Quick if self.live_variant == QuickVariant::Live => {
                busy |= self.stack.show(ui, &self.icons, &pal, card);
            }
            Surface::Quick => {
                crate::surfaces::quick(ui, &self.icons, &pal, card, self.live_variant)
            }
            s => s.show(ui, &self.icons, &pal, card),
        }

        if self.cfg.interactive() {
            hint_bar(ui, &pal, screen);
            if self.tuner_open {
                let act = crate::tuner::show(&ctx, &mut self.tuner_open, &pal);
                if act.replay {
                    self.stack.replay();
                }
                if act.spawn {
                    self.live_surface = Surface::Quick;
                    self.live_variant = QuickVariant::Live;
                    self.stack.spawn();
                }
                if act.dismiss {
                    self.stack.dismiss_top();
                }
            }
        }

        if busy {
            ctx.request_repaint();
        }

        // Shot mode: after the window has settled, grab it and quit.
        self.frame += 1;
        if self.cfg.shot.is_some() {
            ctx.request_repaint();
            if !self.captured && self.frame >= 36 {
                let path = self.cfg.shot.clone().unwrap();
                self.capture(&ctx, &path);
                self.captured = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

/// Small live legend so the interactive window explains its own hotkeys.
fn hint_bar(ui: &mut egui::Ui, pal: &Palette, screen: Rect) {
    let text = "1 Quick  2 Menu  3 Annotate  4 Onboard   V state   N new capture  ⌫ dismiss  R replay   M motion tuner   L light/dark  G glass  Q quit     ·  hover a card, drag it, flick to throw";
    let font = theme::ts_caption();
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, pal.text_muted);
    let size = galley.size() + egui::vec2(20.0, 12.0);
    let rect = Rect::from_center_size(
        egui::pos2(screen.center().x, screen.bottom() - size.y / 2.0 - 16.0),
        size,
    );
    crate::paint::soft_shadow(ui.painter(), rect, 10.0, pal, 0.6);
    ui.painter().rect_filled(rect, theme::cr(10.0), pal.card_fill_raised);
    ui.painter().rect_stroke(
        rect,
        theme::cr(10.0),
        egui::Stroke::new(1.0, pal.hairline),
        egui::StrokeKind::Inside,
    );
    ui.painter()
        .galley(rect.min + egui::vec2(10.0, 6.0), galley, pal.text_muted);
}
