//! The command surface.
//!
//! # This is an API, not a convenience wrapper
//!
//! Per decision D11 every capture the GUI can take, the CLI can take headlessly.
//! Three consumers depend on the shape of what follows, and none of them is a
//! human typing at a prompt:
//!
//! 1. **wlroots compositors.** sway and Hyprland implement no `GlobalShortcuts`
//!    portal, so `xdg-desktop-portal-wlr` cannot give Scrozz a hotkey at all.
//!    The user binds a compositor keybinding to a Scrozz command instead. On
//!    those systems these strings *are* the hotkey system — see
//!    [`crate::hotkey_config`], which generates the config line.
//! 2. **Scripts.** `--json` output and the exit statuses in [`crate::exit`] are
//!    a contract; renaming a flag breaks someone's pipeline.
//! 3. **Agents**, who cannot click, and for whom this is the only interface.
//!
//! # Shape
//!
//! Subcommands, not a flag soup. Targets are a mutually exclusive group so
//! `--region` and `--window` cannot both be supplied; destinations are additive
//! because saving *and* copying is a real want; and `--stdout` conflicts with
//! `--json` because raw image bytes and a JSON document cannot share one stream.

use std::{
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, Instant},
};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use scrozz_annotate::{
    Alignment, AspectPreset, BeautificationPreset, BuiltInBackground, Color, ExactOutputSize,
};
use scrozz_core::{
    AspectLock, CrosshairMode, LogicalPoint, LogicalRect, LogicalSize, SelectionMode,
    SelectionOptions, SizeConstraint,
};
use scrozz_store::MediaKind;

use crate::{
    build_info::VERSION,
    fault::{CliError, CliResult},
    json::Json,
    shortcuts::ShortcutAction,
};

/// Scrozz — screenshots and screen recording for macOS, Windows and Linux.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "scrozz",
    version = VERSION,
    about = "Screenshots and screen recording for macOS, Windows and Linux.",
    long_about = "Screenshots and screen recording for macOS, Windows and Linux.\n\n\
                  Every capture the app can take is available here, headlessly. On sway and \
                  Hyprland this is not a convenience: those compositors implement no global \
                  shortcut portal, so binding a compositor keybinding to a Scrozz command is \
                  the only way hotkeys can work. Run `scrozz hotkey generate-config` to get \
                  the line to paste.\n\n\
                  Run with no subcommand to launch the menu-bar app.",
    propagate_version = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    /// Options that apply to every subcommand.
    #[command(flatten)]
    pub global: GlobalArgs,

    /// The subcommand, or `None` to launch the GUI.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Options accepted before or after any subcommand.
#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// Emit one machine-readable JSON document on stdout.
    ///
    /// The schema is stable and versioned; see the `schema` field.
    #[arg(long, global = true)]
    pub json: bool,

    /// Increase log verbosity. Repeatable. Logs always go to stderr.
    #[arg(short = 'v', long, global = true, action = ArgAction::Count)]
    pub verbose: u8,

    /// Suppress all non-essential output.
    #[arg(short = 'q', long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    /// Never hand the command to an already-running Scrozz instance.
    ///
    /// By default a capture taken while the menu-bar app is running is performed
    /// *by* that app while retaining explicit command-line destinations and
    /// bypassing ambient GUI After Capture actions. This forces the work to happen
    /// here without surprising scripts with an overlay or editor.
    #[arg(long, global = true)]
    pub no_ipc: bool,
}

/// A Scrozz subcommand.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Take a still capture.
    Capture(CaptureArgs),

    /// Record Screen.
    Record(RecordArgs),

    /// List what can be captured.
    List(ListArgs),

    /// Work with Capture History.
    History(HistoryArgs),

    /// Capture Text (OCR) from a capture or image file.
    Ocr(OcrArgs),

    /// Read and write settings.
    Settings(SettingsArgs),

    /// Hotkey helpers.
    Hotkey(HotkeyArgs),

    /// Launch the menu-bar app.
    Gui,
}

impl Command {
    /// The stable `command` slug reported in JSON output.
    #[must_use]
    pub fn slug(&self) -> String {
        match self {
            Self::Capture(_) => "capture".into(),
            Self::Record(args) => {
                if args.stop {
                    "record.stop".into()
                } else {
                    "record.start".into()
                }
            }
            Self::List(args) => match args.what {
                ListWhat::Displays => "list.displays".into(),
                ListWhat::Windows => "list.windows".into(),
            },
            Self::History(args) => match args.command {
                HistoryCommand::List { .. } => "history.list".into(),
                HistoryCommand::Get { .. } => "history.get".into(),
                HistoryCommand::Delete { .. } => "history.delete".into(),
                HistoryCommand::Pin { .. } => "history.pin".into(),
                HistoryCommand::UnlockPins => "history.unlock-pins".into(),
            },
            Self::Ocr(_) => "ocr".into(),
            Self::Settings(args) => match args.command {
                SettingsCommand::Get { .. } => "settings.get".into(),
                SettingsCommand::Set { .. } => "settings.set".into(),
            },
            Self::Hotkey(args) => match args.command {
                HotkeyCommand::GenerateConfig { .. } => "hotkey.generate-config".into(),
            },
            Self::Gui => "gui".into(),
        }
    }

    /// Whether this invocation will write raw bytes to stdout.
    ///
    /// Used to reject `--json` alongside it. clap's own `conflicts_with` only
    /// catches the conflict when `--json` is written after the subcommand; a
    /// global argument given *before* it is merged into the subcommand's matches
    /// after conflict validation has already run. Argument order is not something
    /// a user should have to think about, so the rule is enforced here as well.
    #[must_use]
    pub fn writes_raw_stdout(&self) -> bool {
        match self {
            Self::Capture(args) => args.stdout,
            Self::History(args) => matches!(args.command, HistoryCommand::Get { stdout: true, .. }),
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PathAliases {
    aliases: Vec<(String, String)>,
}

impl PathAliases {
    fn absolutize(&mut self, path: &mut PathBuf, base: &Path) {
        if path.is_absolute() {
            return;
        }
        let reported = path.display().to_string();
        *path = base.join(&*path);
        self.aliases.push((path.display().to_string(), reported));
    }

    pub(crate) fn restore_text(&self, value: &str) -> String {
        let mut aliases: Vec<_> = self.aliases.iter().collect();
        aliases.sort_by_key(|(absolute, _)| std::cmp::Reverse(absolute.len()));
        aliases
            .into_iter()
            .fold(value.to_owned(), |text, (absolute, reported)| {
                text.replace(absolute, reported)
            })
    }

    pub(crate) fn restore_json(&self, value: &mut Json) {
        match value {
            Json::Str(text) => *text = self.restore_text(text),
            Json::Arr(items) => {
                for item in items {
                    self.restore_json(item);
                }
            }
            Json::Obj(fields) => {
                for (_, value) in fields {
                    self.restore_json(value);
                }
            }
            Json::Null | Json::Bool(_) | Json::Int(_) | Json::Float(_) => {}
        }
    }
}

impl Cli {
    /// Checks the rules that span a global option and a subcommand.
    ///
    /// Per-subcommand rules live on the subcommand's own `validate`; this is only
    /// for the ones clap cannot express because the arguments live on different
    /// sides of the subcommand boundary.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] — exit code 2, matching what clap itself would
    /// have produced.
    pub fn validate(&self) -> CliResult<()> {
        if let Some(command) = &self.command
            && self.global.json
            && command.writes_raw_stdout()
        {
            return Err(CliError::usage(
                "--stdout and --json both want stdout: one stream cannot carry \
                 raw image bytes and a JSON document. Pick one, or send the image \
                 to a file with --output.",
            ));
        }
        Ok(())
    }

    /// Resolves file arguments against the directory of the process that
    /// submitted this command.
    ///
    /// IPC commands execute on a worker in the already-running app. Changing
    /// that process's global working directory would race captures on every
    /// other thread, so relative paths are made absolute before dispatch.
    pub(crate) fn absolutize_paths(&mut self, base: &Path) -> PathAliases {
        let mut aliases = PathAliases::default();

        match self.command.as_mut() {
            Some(Command::Capture(args)) => {
                if let Some(path) = &mut args.output {
                    aliases.absolutize(path, base);
                }
            }
            Some(Command::Record(args)) => {
                if let Some(path) = &mut args.output {
                    aliases.absolutize(path, base);
                }
            }
            Some(Command::History(HistoryArgs {
                command:
                    HistoryCommand::Get {
                        output: Some(path), ..
                    },
            })) => {
                aliases.absolutize(path, base);
            }
            Some(Command::Ocr(args)) => {
                if let Some(path) = &mut args.file {
                    aliases.absolutize(path, base);
                }
                if let Some(subject) = &mut args.subject {
                    let candidate = PathBuf::from(&*subject);
                    if candidate.is_relative() {
                        let candidate = base.join(candidate);
                        if candidate.is_file() {
                            aliases
                                .aliases
                                .push((candidate.display().to_string(), subject.clone()));
                            *subject = candidate.to_string_lossy().into_owned();
                        }
                    }
                }
            }
            _ => {}
        }
        aliases
    }
}

// ---------------------------------------------------------------------------
// Targets
// ---------------------------------------------------------------------------

/// What a capture or recording is aimed at.
///
/// A `clap` group with `multiple = false`, so supplying two of these is a parse
/// error rather than a silent precedence rule nobody can remember.
#[derive(Debug, Clone, Args)]
#[group(id = "target", multiple = false)]
pub struct TargetArgs {
    /// A rectangle in the global logical desktop, as `X,Y,W,H`.
    ///
    /// Coordinates are logical points, not pixels, and may be negative on a
    /// multi-monitor desktop where a display sits left of or above the primary.
    #[arg(long, value_name = "X,Y,W,H")]
    pub region: Option<RegionArg>,

    /// A window, by id or by title.
    #[arg(long, value_name = "ID|TITLE")]
    pub window: Option<String>,

    /// A display, by id, or `primary`, or `active` for the one under the pointer.
    #[arg(long, value_name = "ID|primary|active")]
    pub display: Option<String>,

    /// Every display, composited into one image.
    #[arg(long)]
    pub all_displays: bool,

    /// Pick the target on screen. Defaults to Capture Area (`region`).
    ///
    /// On Wayland, interactive window selection requires a separate portal-owned
    /// capture handoff and is refused until that handoff is available.
    #[arg(
        long,
        value_name = "MODE",
        num_args = 0..=1,
        default_missing_value = "region",
        value_enum
    )]
    pub interactive: Option<InteractiveMode>,
}

/// Which interactive picker to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum InteractiveMode {
    /// Capture Area by dragging out a rectangle.
    Region,
    /// Capture Window by clicking it.
    Window,
    /// Capture Fullscreen by clicking a display.
    Display,
    /// Open All-in-One… with every capture mode available.
    AllInOne,
}

impl InteractiveMode {
    /// The mode the selector opens in.
    #[must_use]
    pub const fn initial_mode(self) -> SelectionMode {
        match self {
            Self::Region | Self::AllInOne => SelectionMode::Region,
            Self::Window => SelectionMode::Window,
            Self::Display => SelectionMode::Display,
        }
    }

    /// Whether the all-in-one mode switcher should be visible.
    #[must_use]
    pub const fn shows_hud(self) -> bool {
        matches!(self, Self::AllInOne)
    }

    /// Whether region-only size and retake controls make sense.
    #[must_use]
    pub const fn supports_region_controls(self) -> bool {
        matches!(self, Self::Region | Self::AllInOne)
    }
}

/// How a display was named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplaySelector {
    /// The primary display.
    Primary,
    /// Whichever display currently contains the pointer.
    Active,
    /// A specific display id.
    Id(String),
}

/// A resolved capture target.
#[derive(Debug, Clone, PartialEq)]
pub enum TargetSpec {
    /// A fixed rectangle.
    Region(LogicalRect),
    /// A window named by id or title.
    Window(String),
    /// A named display.
    Display(DisplaySelector),
    /// Every display.
    AllDisplays,
    /// Chosen on screen.
    Interactive(InteractiveMode),
}

impl TargetArgs {
    /// Resolves the flags into exactly one target.
    ///
    /// With nothing specified the answer is an interactive region: that is what
    /// a bare hotkey should do, and a compositor keybinding is the main caller.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] if a selector is empty or unrecognised.
    /// Genuine mutual exclusion is enforced by `clap` before this runs.
    pub fn resolve(&self) -> CliResult<TargetSpec> {
        if let Some(region) = &self.region {
            return Ok(TargetSpec::Region(region.to_logical_rect()));
        }
        if let Some(window) = &self.window {
            if window.trim().is_empty() {
                return Err(CliError::usage(
                    "--window needs a window id or a title; \
                     run `scrozz list windows` to see what is available",
                ));
            }
            return Ok(TargetSpec::Window(window.clone()));
        }
        if let Some(display) = &self.display {
            return Ok(TargetSpec::Display(parse_display_selector(display)?));
        }
        if self.all_displays {
            return Ok(TargetSpec::AllDisplays);
        }
        Ok(TargetSpec::Interactive(
            self.interactive.unwrap_or(InteractiveMode::Region),
        ))
    }

    /// Whether the target requires on-screen interaction.
    ///
    /// Load-bearing for single-instance behaviour: an interactive capture needs
    /// the selection overlay, and only one process can own that.
    #[must_use]
    pub fn is_interactive(&self) -> bool {
        matches!(self.resolve(), Ok(TargetSpec::Interactive(_)))
    }

    /// Whether no target flag was given at all.
    ///
    /// [`Self::resolve`] answers "interactive region" for an empty set, which is
    /// right for a capture hotkey and wrong for a recording: `scrozz record`
    /// with no arguments should start recording, not open a picker. Recording
    /// asks this first and supplies its own default.
    #[must_use]
    pub const fn is_unspecified(&self) -> bool {
        self.region.is_none()
            && self.window.is_none()
            && self.display.is_none()
            && !self.all_displays
            && self.interactive.is_none()
    }
}

fn parse_display_selector(raw: &str) -> CliResult<DisplaySelector> {
    match raw.trim() {
        "" => Err(CliError::usage(
            "--display needs a display id, `primary` or `active`; \
             run `scrozz list displays` to see what is available",
        )),
        "primary" => Ok(DisplaySelector::Primary),
        "active" => Ok(DisplaySelector::Active),
        id => Ok(DisplaySelector::Id(id.to_string())),
    }
}

/// A `X,Y,W,H` rectangle supplied on the command line.
///
/// Validated at parse time rather than at capture time so a typo fails
/// immediately, with a message naming the format, instead of producing an empty
/// image several seconds later.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RegionArg {
    /// Left edge, in logical points.
    pub x: f64,
    /// Top edge, in logical points.
    pub y: f64,
    /// Width, in logical points. Always positive.
    pub width: f64,
    /// Height, in logical points. Always positive.
    pub height: f64,
}

impl RegionArg {
    /// Converts to the core geometry type.
    #[must_use]
    pub fn to_logical_rect(self) -> LogicalRect {
        LogicalRect::new(
            LogicalPoint::new(self.x, self.y),
            LogicalSize::new(self.width, self.height),
        )
    }
}

impl FromStr for RegionArg {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = raw.split(',').map(str::trim).collect();
        if parts.len() != 4 {
            return Err(format!(
                "expected four comma-separated numbers `X,Y,W,H`, got {} in {raw:?}",
                parts.len()
            ));
        }

        let mut values = [0.0f64; 4];
        for (slot, (name, text)) in values
            .iter_mut()
            .zip(["X", "Y", "W", "H"].into_iter().zip(parts))
        {
            let value: f64 = text
                .parse()
                .map_err(|_| format!("{name} in {raw:?} is not a number: {text:?}"))?;
            if !value.is_finite() {
                return Err(format!("{name} in {raw:?} must be finite, got {text:?}"));
            }
            *slot = value;
        }

        let [x, y, width, height] = values;
        // Size::new clamps a negative extent to zero, which would turn a typo
        // into a silently empty capture rather than an error.
        if width <= 0.0 || height <= 0.0 {
            return Err(format!(
                "region {raw:?} has no area: width and height must both be greater than zero"
            ));
        }

        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }
}

/// A positive `WIDTHxHEIGHT` size supplied on the command line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixedSizeArg {
    /// Width in logical points.
    pub width: f64,
    /// Height in logical points.
    pub height: f64,
}

impl FixedSizeArg {
    /// Converts to the core geometry type.
    #[must_use]
    pub fn to_logical_size(self) -> LogicalSize {
        LogicalSize::new(self.width, self.height)
    }
}

impl FromStr for FixedSizeArg {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (width, height) = parse_positive_pair(raw, ['x', 'X'], "WIDTHxHEIGHT")?;
        Ok(Self { width, height })
    }
}

/// A positive `WIDTH:HEIGHT` aspect ratio supplied on the command line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AspectArg {
    /// Ratio numerator.
    pub width: f64,
    /// Ratio denominator.
    pub height: f64,
}

impl AspectArg {
    /// Converts to the core ratio type.
    pub fn to_lock(self) -> scrozz_core::Result<AspectLock> {
        AspectLock::ratio(self.width, self.height)
    }
}

impl FromStr for AspectArg {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (width, height) = parse_positive_pair(raw, [':'], "WIDTH:HEIGHT")?;
        Ok(Self { width, height })
    }
}

fn parse_positive_pair<const N: usize>(
    raw: &str,
    separators: [char; N],
    expected: &str,
) -> Result<(f64, f64), String> {
    let Some((left, right)) = raw
        .char_indices()
        .find(|(_, ch)| separators.contains(ch))
        .map(|(at, ch)| (&raw[..at], &raw[at + ch.len_utf8()..]))
    else {
        return Err(format!("expected `{expected}`, got {raw:?}"));
    };
    if right.chars().any(|ch| separators.contains(&ch)) {
        return Err(format!(
            "expected one separator in `{expected}`, got {raw:?}"
        ));
    }

    let parse = |name: &str, text: &str| -> Result<f64, String> {
        let value = text
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("{name} in {raw:?} is not a number: {text:?}"))?;
        if !value.is_finite() || value <= 0.0 {
            return Err(format!(
                "{name} in {raw:?} must be finite and greater than zero"
            ));
        }
        Ok(value)
    };

    Ok((parse("width", left)?, parse("height", right)?))
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

/// An output image format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    /// Lossless, alpha-capable. The default.
    Png,
    /// Lossy, no alpha.
    Jpeg,
    /// Lossy or lossless, alpha-capable, much smaller than PNG.
    #[value(name = "webp")]
    WebP,
}

impl Format {
    /// The core export enum.
    #[must_use]
    pub const fn to_export(self) -> scrozz_export::ImageFormat {
        match self {
            Self::Png => scrozz_export::ImageFormat::Png,
            Self::Jpeg => scrozz_export::ImageFormat::Jpeg,
            Self::WebP => scrozz_export::ImageFormat::WebP,
        }
    }

    /// The stable slug used in JSON output and as a file extension.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpeg",
            Self::WebP => "webp",
        }
    }
}

/// Where a capture is sent.
///
/// Additive rather than exclusive: saving a file *and* putting it on the
/// clipboard is one of the most common things anyone wants from a screenshot
/// tool, and making the user pick one would be a worse tool for no gain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sink {
    /// A specific path.
    File(PathBuf),
    /// The configured capture folder (D18).
    DefaultFolder,
    /// The system clipboard.
    Clipboard,
    /// Raw encoded bytes on stdout, for piping.
    Stdout,
}

impl Sink {
    /// The stable slug used in JSON output.
    #[must_use]
    pub const fn slug(&self) -> &'static str {
        match self {
            Self::File(_) => "file",
            Self::DefaultFolder => "default-folder",
            Self::Clipboard => "clipboard",
            Self::Stdout => "stdout",
        }
    }
}

// ---------------------------------------------------------------------------
// Beautification and Smart Frame
// ---------------------------------------------------------------------------

/// A named beautification starting point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BeautifyPreset {
    /// Neutral cool-grey framing.
    Clean,
    /// Square iris gradient for social posts.
    Social,
    /// Tall 9:16 story/reel framing.
    Story,
    /// Warm restrained 4:5 editorial framing.
    Editorial,
}

impl BeautifyPreset {
    /// Converts the command-line value into the shared annotation model.
    #[must_use]
    pub const fn to_model(self) -> BeautificationPreset {
        match self {
            Self::Clean => BeautificationPreset::Clean,
            Self::Social => BeautificationPreset::Social,
            Self::Story => BeautificationPreset::Story,
            Self::Editorial => BeautificationPreset::Editorial,
        }
    }
}

/// Output aspect ratio for a beautified capture's presentation canvas.
///
/// Exposed as `--frame-aspect` rather than `--aspect`: that flag is already
/// spoken for by [`AspectArg`], which locks the *interactive selection*
/// rectangle while dragging. The two describe different rectangles — one on
/// screen before the pixels are read, one on the output canvas after framing —
/// and a single target can use both at once (drag out a square, then frame it
/// into a 16:9 card), so they need names that can coexist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BeautifyAspect {
    /// Preserve the capture's natural ratio.
    Original,
    /// 1:1 social post.
    Square,
    /// 4:5 portrait post.
    Portrait,
    /// 9:16 story/reel.
    Story,
    /// 16:9 landscape post or thumbnail.
    Landscape,
    /// 3:1 social header.
    Wide,
}

impl BeautifyAspect {
    /// Converts to the shared annotation model.
    #[must_use]
    pub const fn to_model(self) -> AspectPreset {
        match self {
            Self::Original => AspectPreset::Original,
            Self::Square => AspectPreset::Square,
            Self::Portrait => AspectPreset::Portrait,
            Self::Story => AspectPreset::Story,
            Self::Landscape => AspectPreset::Landscape,
            Self::Wide => AspectPreset::Wide,
        }
    }
}

/// Exact output canvas dimensions for `--size`, such as `1080x1350`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BeautifySize {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl BeautifySize {
    /// Converts to the shared model.
    #[must_use]
    pub const fn to_model(self) -> ExactOutputSize {
        ExactOutputSize::new(self.width, self.height)
    }
}

impl FromStr for BeautifySize {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let (width, height) = raw
            .trim()
            .split_once(['x', 'X', '\u{d7}'])
            .ok_or_else(|| format!("expected WIDTHxHEIGHT, got {raw:?}"))?;
        let width = width
            .parse::<u32>()
            .map_err(|_| format!("output width is not a positive integer: {width:?}"))?;
        let height = height
            .parse::<u32>()
            .map_err(|_| format!("output height is not a positive integer: {height:?}"))?;
        let size = Self { width, height };
        // Validated eagerly so a bad `--size` is reported at parse time, in the
        // same breath as every other malformed argument, instead of surfacing
        // later as a generic beautification error deep inside capture.
        let probe = scrozz_annotate::Beautification {
            output_size: Some(size.to_model()),
            ..scrozz_annotate::Beautification::default()
        };
        probe.validate().map_err(|error| error.to_string())?;
        Ok(size)
    }
}

/// Placement within an aspect- or size-expanded canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BeautifyAlignment {
    /// Top-left.
    TopLeft,
    /// Top-centre.
    Top,
    /// Top-right.
    TopRight,
    /// Centre-left.
    Left,
    /// Centre.
    Center,
    /// Centre-right.
    Right,
    /// Bottom-left.
    BottomLeft,
    /// Bottom-centre.
    Bottom,
    /// Bottom-right.
    BottomRight,
}

impl BeautifyAlignment {
    /// Converts to the shared annotation model.
    #[must_use]
    pub const fn to_model(self) -> Alignment {
        match self {
            Self::TopLeft => Alignment::TopLeft,
            Self::Top => Alignment::Top,
            Self::TopRight => Alignment::TopRight,
            Self::Left => Alignment::Left,
            Self::Center => Alignment::Center,
            Self::Right => Alignment::Right,
            Self::BottomLeft => Alignment::BottomLeft,
            Self::Bottom => Alignment::Bottom,
            Self::BottomRight => Alignment::BottomRight,
        }
    }
}

/// A background supplied on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeautifyBackground {
    /// Preserve alpha in the padded area.
    Transparent,
    /// A flat RGBA colour.
    Solid(Color),
    /// One of the bundled procedural backgrounds.
    BuiltIn(BuiltInBackground),
    /// A custom image loaded when capture runs.
    Image(PathBuf),
}

impl FromStr for BeautifyBackground {
    type Err = String;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let value = raw.trim();
        let lower = value.to_ascii_lowercase();
        let built_in = match lower.as_str() {
            "mist" => Some(BuiltInBackground::Mist),
            "iris" => Some(BuiltInBackground::Iris),
            "midnight" => Some(BuiltInBackground::Midnight),
            "sunrise" => Some(BuiltInBackground::Sunrise),
            "lagoon" => Some(BuiltInBackground::Lagoon),
            "sand" => Some(BuiltInBackground::Sand),
            _ => None,
        };
        if lower == "transparent" {
            return Ok(Self::Transparent);
        }
        if let Some(background) = built_in {
            return Ok(Self::BuiltIn(background));
        }
        if let Some(path) = value.strip_prefix("image:").filter(|path| !path.is_empty()) {
            return Ok(Self::Image(PathBuf::from(path)));
        }
        let color = value.strip_prefix("solid:").unwrap_or(value);
        if color.starts_with('#') {
            return parse_hex_color(color).map(Self::Solid);
        }
        Err(format!(
            "expected transparent, mist, iris, midnight, sunrise, lagoon, sand, \
             #RRGGBB[AA], or image:PATH; got {raw:?}"
        ))
    }
}

fn parse_hex_color(raw: &str) -> Result<Color, String> {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    if hex.len() != 6 && hex.len() != 8 {
        return Err(format!(
            "solid colour must be #RRGGBB or #RRGGBBAA, got {raw:?}"
        ));
    }
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&hex[range], 16)
            .map_err(|_| format!("solid colour contains a non-hex digit: {raw:?}"))
    };
    Ok(Color::rgba(
        channel(0..2)?,
        channel(2..4)?,
        channel(4..6)?,
        if hex.len() == 8 { channel(6..8)? } else { 255 },
    ))
}

/// Arguments to `scrozz capture`.
#[derive(Debug, Clone, Args)]
pub struct CaptureArgs {
    /// What to capture.
    #[command(flatten)]
    pub target: TargetArgs,

    /// Composite the pointer into the capture.
    #[arg(long)]
    pub cursor: bool,

    /// Self-Timer: wait this many seconds before capturing.
    ///
    /// `allow_hyphen_values` so that `--delay -1` reaches the validator and is
    /// rejected as a bad delay, rather than being reported by clap as an unknown
    /// flag named `-1`. Both are exit code 2; only one of them is a useful thing
    /// to read at three in the morning.
    #[arg(long, value_name = "SECS", allow_hyphen_values = true)]
    pub delay: Option<f64>,

    /// Hold an exact selection size, as `WIDTHxHEIGHT` logical points.
    #[arg(long, value_name = "WIDTHxHEIGHT")]
    pub fixed_size: Option<FixedSizeArg>,

    /// Hold a selection aspect ratio, as `WIDTH:HEIGHT`.
    #[arg(long, value_name = "WIDTH:HEIGHT")]
    pub aspect: Option<AspectArg>,

    /// Freeze region/display pixels while choosing. Window captures stay live.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    pub freeze: Option<bool>,

    /// Capture Previous Area, opening the last area for adjustment first.
    #[arg(long)]
    pub retake: bool,

    /// Show the pixel magnifier. Use `--magnifier=false` to hide it.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    pub magnifier: Option<bool>,

    /// Show full-width pointer guides. Use `--crosshair=false` to hide them.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        default_missing_value = "true",
        require_equals = true
    )]
    pub crosshair: Option<bool>,

    /// Write the image to this path.
    #[arg(long, short = 'o', value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Put the image on the system clipboard.
    #[arg(long)]
    pub clipboard: bool,

    /// Write raw encoded image bytes to stdout, for piping.
    ///
    /// Mutually exclusive with `--json`: one stream cannot carry both a PNG and
    /// a JSON document. Logs and diagnostics always go to stderr, so the byte
    /// stream is never polluted.
    #[arg(long, conflicts_with = "json")]
    pub stdout: bool,

    /// Image format. Defaults to PNG.
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub format: Option<Format>,

    /// Encoder quality for lossy formats.
    #[arg(long, value_name = "1-100", value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: Option<u8>,

    /// Apply a named non-destructive beautification preset.
    #[arg(long, value_enum, value_name = "PRESET")]
    pub beautify: Option<BeautifyPreset>,

    /// Analyse this capture and build an adaptive Smart Frame (D9-safe).
    #[arg(long, conflicts_with = "beautify")]
    pub smart_frame: bool,

    /// Override the beautification background.
    ///
    /// Accepts a built-in name, `transparent`, `#RRGGBB[AA]`, or `image:PATH`.
    #[arg(long, value_name = "BACKGROUND")]
    pub background: Option<BeautifyBackground>,

    /// Padding around the capture in logical points.
    #[arg(long, value_name = "POINTS", allow_hyphen_values = true)]
    pub padding: Option<f64>,

    /// Output canvas aspect ratio for beautification.
    ///
    /// Distinct from `--aspect`, which locks the interactive selection
    /// rectangle; see [`BeautifyAspect`] for why the two cannot share a flag.
    #[arg(long, value_enum, value_name = "ASPECT")]
    pub frame_aspect: Option<BeautifyAspect>,

    /// Exact presentation-canvas dimensions, such as 1080x1350.
    #[arg(long, value_name = "WIDTHxHEIGHT", conflicts_with = "frame_aspect")]
    pub size: Option<BeautifySize>,

    /// Capture placement within an aspect- or size-expanded canvas.
    #[arg(long, value_enum, value_name = "POSITION")]
    pub alignment: Option<BeautifyAlignment>,

    /// Centre visual weight rather than the capture's geometric bounds.
    #[arg(long)]
    pub auto_balance: bool,

    /// Rounded capture corner radius in logical points.
    #[arg(long, value_name = "POINTS", allow_hyphen_values = true)]
    pub corner_radius: Option<f64>,

    /// Drop-shadow depth in logical points.
    #[arg(long, value_name = "POINTS", allow_hyphen_values = true)]
    pub shadow: Option<f64>,

    /// Border width in logical points.
    #[arg(long, value_name = "POINTS", allow_hyphen_values = true)]
    pub border: Option<f64>,

    /// Drop the window's own shadow, for window targets.
    #[arg(long)]
    pub no_window_shadow: bool,

    /// Resolve everything and report the plan without capturing.
    ///
    /// Exists because the interesting part of a capture — which target, which
    /// sinks, which format — is decided before a single pixel is read, and that
    /// decision is worth being able to inspect. It makes the resolution logic
    /// testable on a machine with no screen-recording permission and no display,
    /// which is the situation of every CI runner and every agent.
    #[arg(long)]
    pub dry_run: bool,
}

impl CaptureArgs {
    /// Every destination this invocation asked for.
    ///
    /// With none specified the capture is saved to the configured folder, which
    /// is what an unattended compositor keybinding needs to do something useful.
    #[must_use]
    pub fn sinks(&self) -> Vec<Sink> {
        let mut sinks = Vec::new();
        if let Some(path) = &self.output {
            sinks.push(Sink::File(path.clone()));
        }
        if self.clipboard {
            sinks.push(Sink::Clipboard);
        }
        if self.stdout {
            sinks.push(Sink::Stdout);
        }
        if sinks.is_empty() {
            sinks.push(Sink::DefaultFolder);
        }
        sinks
    }

    /// The chosen format, defaulting to PNG.
    #[must_use]
    pub fn format(&self) -> Format {
        self.format.unwrap_or(Format::Png)
    }

    /// Builds the shared selector options for an interactive target.
    ///
    /// `remembered` comes from the app's remembered-region store. It is accepted
    /// separately so parsing remains pure and dry-runs never touch user state.
    ///
    /// # Errors
    ///
    /// Returns a usage error when a size, aspect, or retake option is attached to
    /// a target that cannot use region controls.
    pub fn selection_options(
        &self,
        remembered: Option<(LogicalRect, Option<scrozz_core::DisplayId>)>,
    ) -> CliResult<Option<SelectionOptions>> {
        let target = self.target.resolve()?;
        let TargetSpec::Interactive(mode) = target else {
            if self.has_selection_controls() {
                return Err(CliError::usage(
                    "--fixed-size, --aspect, --freeze, --retake, --magnifier and \
                     --crosshair apply only to --interactive captures",
                ));
            }
            return Ok(None);
        };

        if !mode.supports_region_controls()
            && (self.fixed_size.is_some() || self.aspect.is_some() || self.retake)
        {
            return Err(CliError::usage(
                "--fixed-size, --aspect and --retake require `--interactive region` \
                 or `--interactive all-in-one`",
            ));
        }
        if !mode.supports_region_controls()
            && (self.magnifier == Some(true) || self.crosshair == Some(true))
        {
            return Err(CliError::usage(
                "--magnifier and --crosshair require `--interactive region` \
                 or `--interactive all-in-one`",
            ));
        }
        if mode == InteractiveMode::Window && self.freeze == Some(true) {
            return Err(CliError::usage(
                "--freeze cannot preserve an isolated window capture; use \
                 `--interactive region` for frozen pixels or `--freeze=false`",
            ));
        }

        let mut constraint = SizeConstraint::free();
        if let Some(size) = self.fixed_size {
            constraint = constraint.with_exact(size.to_logical_size())?;
        }
        if let Some(aspect) = self.aspect {
            let lock = aspect.to_lock()?;
            if let Some(exact) = constraint.exact {
                let exact_ratio = exact.width / exact.height;
                let wanted = lock.value().expect("a parsed aspect is locked");
                if (exact_ratio - wanted).abs() > 1e-9 {
                    return Err(CliError::usage(format!(
                        "--fixed-size {}x{} does not satisfy --aspect {}:{}",
                        exact.width, exact.height, aspect.width, aspect.height
                    )));
                }
            }
            constraint = constraint.with_aspect(lock);
        }

        let defaults = SelectionOptions::for_mode(mode.initial_mode());
        let delay = self.delay.map(checked_delay).transpose()?;
        let (remembered, remembered_display) =
            remembered.map_or((None, None), |(rect, display)| (Some(rect), display));
        let crosshair = self.crosshair.unwrap_or(false);
        let magnifier = self.magnifier.unwrap_or(false);
        let crosshair_mode = if crosshair || magnifier {
            CrosshairMode::Always
        } else {
            CrosshairMode::Off
        };
        Ok(Some(SelectionOptions {
            remembered: self.retake.then_some(remembered).flatten(),
            remembered_display: self.retake.then_some(remembered_display).flatten(),
            constraint,
            freeze: self.freeze.unwrap_or(defaults.freeze),
            crosshair_mode,
            crosshair,
            magnifier,
            delay,
            hud: mode.shows_hud(),
            ..defaults
        }))
    }

    fn has_selection_controls(&self) -> bool {
        self.fixed_size.is_some()
            || self.aspect.is_some()
            || self.freeze.is_some()
            || self.retake
            || self.magnifier.is_some()
            || self.crosshair.is_some()
    }

    /// Whether any argument requests the beautification pipeline.
    #[must_use]
    pub fn requests_beautification(&self) -> bool {
        self.beautify.is_some()
            || self.smart_frame
            || self.background.is_some()
            || self.padding.is_some()
            || self.frame_aspect.is_some()
            || self.size.is_some()
            || self.alignment.is_some()
            || self.auto_balance
            || self.corner_radius.is_some()
            || self.shadow.is_some()
            || self.border.is_some()
    }

    /// Validates combinations `clap` cannot express.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] for a negative or non-finite delay, for
    /// `--quality` on a format that has no quality setting, or for a
    /// beautification measurement out of range. Returns
    /// [`CliError::Core`] with [`scrozz_core::Error::InvalidRequest`] when
    /// beautification is requested for a window target in a way that would
    /// composite onto native window pixels (decision D9) — a window may only
    /// gain an explicit, subject-preserving `--smart-frame` outer canvas.
    pub fn validate(&self) -> CliResult<()> {
        if let Some(delay) = self.delay {
            let _ = checked_delay(delay)?;
        }
        if self.quality.is_some() && self.format() == Format::Png {
            return Err(CliError::usage(
                "--quality has no meaning for PNG, which is lossless; \
                 use --format jpeg or --format webp",
            ));
        }

        for (name, value) in [
            ("--padding", self.padding),
            ("--corner-radius", self.corner_radius),
            ("--shadow", self.shadow),
            ("--border", self.border),
        ] {
            if let Some(value) = value
                && (!value.is_finite()
                    || !(0.0..=scrozz_annotate::Beautification::MAX_MEASUREMENT).contains(&value))
            {
                return Err(CliError::usage(format!(
                    "{name} must be between 0 and {}, got {value}",
                    scrozz_annotate::Beautification::MAX_MEASUREMENT
                )));
            }
        }

        let target = self.target.resolve()?;
        if self.requests_beautification()
            && matches!(
                target,
                TargetSpec::Window(_) | TargetSpec::Interactive(InteractiveMode::Window)
            )
            && (!self.smart_frame
                || self.beautify.is_some()
                || self.corner_radius.is_some_and(|value| value > 0.0)
                || self.shadow.is_some_and(|value| value > 0.0)
                || self.border.is_some_and(|value| value > 0.0))
        {
            return Err(CliError::Core(scrozz_core::Error::InvalidRequest(
                "window Smart Frame may add only an outer canvas; inset, corners, shadow, and \
                 border are disabled to preserve native pixels (decision D9)"
                    .to_owned(),
            )));
        }

        self.selection_options(None)?;
        Ok(())
    }
}

fn checked_delay(seconds: f64) -> CliResult<Duration> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(CliError::usage(format!(
            "--delay must be a non-negative number of seconds, got {seconds}"
        )));
    }
    let duration = Duration::try_from_secs_f64(seconds).map_err(|_| {
        CliError::usage(format!(
            "--delay is too large for this platform, got {seconds} seconds"
        ))
    })?;
    if Instant::now().checked_add(duration).is_none() {
        return Err(CliError::usage(format!(
            "--delay is too large for this platform, got {seconds} seconds"
        )));
    }
    Ok(duration)
}

// ---------------------------------------------------------------------------
// record
// ---------------------------------------------------------------------------

/// Arguments to `scrozz record`.
#[derive(Debug, Clone, Args)]
pub struct RecordArgs {
    /// What to record.
    #[command(flatten)]
    pub target: TargetArgs,

    /// Capture microphone input.
    #[arg(long)]
    pub microphone: bool,

    /// Capture system audio output.
    #[arg(long)]
    pub system_audio: bool,

    /// Frames per second.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=240))]
    pub fps: u32,

    /// Draw the pointer into the video.
    #[arg(long)]
    pub cursor: bool,

    /// Write the recording to this path.
    #[arg(long, short = 'o', value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Stop the recording already in progress.
    ///
    /// Recording is stateful across processes, so this is always handed to the
    /// running Scrozz instance; there is no session in a fresh process to stop.
    #[arg(
        long,
        conflicts_with_all = ["target", "microphone", "system_audio", "fps", "cursor", "output"]
    )]
    pub stop: bool,

    /// Resolve everything and report the plan without recording.
    #[arg(long, conflicts_with = "stop")]
    pub dry_run: bool,
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

/// Arguments to `scrozz list`.
#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    /// What to enumerate.
    #[command(subcommand)]
    pub what: ListWhat,
}

/// What `scrozz list` can enumerate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum ListWhat {
    /// Connected displays.
    Displays,
    /// Capturable windows, front-most first.
    ///
    /// Unavailable on Wayland, which has no window enumeration protocol.
    Windows,
}

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

/// Arguments to `scrozz history`.
#[derive(Debug, Clone, Args)]
pub struct HistoryArgs {
    /// The history operation.
    #[command(subcommand)]
    pub command: HistoryCommand,
}

/// Operations on the capture history.
#[derive(Debug, Clone, Subcommand)]
pub enum HistoryCommand {
    /// List stored captures, newest first.
    List {
        /// Show at most this many.
        #[arg(
            long,
            default_value_t = 50,
            value_name = "N",
            value_parser = clap::value_parser!(u32).range(1..=1000)
        )]
        limit: u32,

        /// Skip this many matching captures.
        #[arg(long, default_value_t = 0, value_name = "N")]
        offset: u32,

        /// Show one media kind.
        #[arg(
            long,
            visible_alias = "media-kind",
            value_name = "screenshot|video|gif",
            value_parser = parse_media_kind
        )]
        kind: Option<MediaKind>,

        /// Search application names, window titles, and recognised text.
        #[arg(long, visible_alias = "text", value_name = "TEXT")]
        search: Option<String>,

        /// Show captures from this application.
        #[arg(long, value_name = "APP")]
        app: Option<String>,

        /// Show captures taken at or after this UTC date, timestamp, or Unix time.
        #[arg(long, value_name = "DATE|TIMESTAMP", value_parser = parse_history_after)]
        after: Option<i64>,

        /// Show captures taken at or before this UTC date, timestamp, or Unix time.
        #[arg(long, value_name = "DATE|TIMESTAMP", value_parser = parse_history_before)]
        before: Option<i64>,

        /// Show only pinned captures.
        #[arg(long)]
        pinned: bool,

        /// Hide records whose source pixels were evicted.
        #[arg(long)]
        images_only: bool,
    },

    /// Write a stored capture's image out.
    Get {
        /// The capture id.
        id: String,

        /// Write the image to this path.
        #[arg(long, short = 'o', value_name = "PATH")]
        output: Option<PathBuf>,

        /// Write raw image bytes to stdout.
        #[arg(long, conflicts_with = "json")]
        stdout: bool,
    },

    /// Delete captures.
    Delete {
        /// The capture ids.
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
    },

    /// Exempt a capture from retention eviction.
    Pin {
        /// The capture id.
        id: String,

        /// Remove the pin instead of adding it.
        #[arg(long)]
        unpin: bool,
    },

    /// Unlock every on-screen pin without needing its capture id.
    UnlockPins,
}

fn parse_media_kind(value: &str) -> Result<MediaKind, String> {
    MediaKind::from_token(value).map_err(|err| err.to_string())
}

fn parse_history_after(value: &str) -> Result<i64, String> {
    parse_history_timestamp(value, false)
}

fn parse_history_before(value: &str) -> Result<i64, String> {
    parse_history_timestamp(value, true)
}

/// Parses an ISO-8601 date/time or Unix seconds/milliseconds into epoch millis.
///
/// A date-only upper bound means the end of that UTC day; a lower bound means
/// its beginning. Timestamps without an explicit offset are interpreted as UTC.
fn parse_history_timestamp(value: &str, end_of_date: bool) -> Result<i64, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("a history date cannot be empty".to_owned());
    }

    if let Ok(integer) = value.parse::<i64>() {
        return if integer.unsigned_abs() < 100_000_000_000 {
            integer
                .checked_mul(1_000)
                .ok_or_else(|| format!("Unix time {value:?} is out of range"))
        } else {
            Ok(integer)
        };
    }

    let (date, rest) = if let Some((date, time)) = value.split_once('T') {
        (date, Some(time))
    } else if let Some((date, time)) = value.split_once(' ') {
        (date, Some(time))
    } else {
        (value, None)
    };
    let (year, month, day) = parse_date(date)?;
    let days = days_from_civil(year, month, day)
        .ok_or_else(|| format!("invalid UTC date {date:?}; expected YYYY-MM-DD"))?;

    let Some(rest) = rest else {
        let base = days
            .checked_mul(86_400_000)
            .ok_or_else(|| format!("date {value:?} is out of range"))?;
        return Ok(if end_of_date {
            base.saturating_add(86_399_999)
        } else {
            base
        });
    };

    let (clock, offset_seconds) = split_utc_offset(rest)?;
    let (hour, minute, second, millis) = parse_clock(clock)?;
    let local_millis = days
        .checked_mul(86_400_000)
        .and_then(|base| base.checked_add(i64::from(hour) * 3_600_000))
        .and_then(|base| base.checked_add(i64::from(minute) * 60_000))
        .and_then(|base| base.checked_add(i64::from(second) * 1_000))
        .and_then(|base| base.checked_add(i64::from(millis)))
        .ok_or_else(|| format!("timestamp {value:?} is out of range"))?;
    local_millis
        .checked_sub(i64::from(offset_seconds) * 1_000)
        .ok_or_else(|| format!("timestamp {value:?} is out of range"))
}

fn parse_date(value: &str) -> Result<(i64, u32, u32), String> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes[..4].iter().all(u8::is_ascii_digit)
        || !bytes[5..7].iter().all(u8::is_ascii_digit)
        || !bytes[8..].iter().all(u8::is_ascii_digit)
    {
        return Err(format!(
            "invalid UTC date {value:?}; expected YYYY-MM-DD or an RFC 3339 timestamp"
        ));
    }
    let year = value[..4]
        .parse::<i64>()
        .map_err(|_| format!("invalid year in {value:?}"))?;
    let month = value[5..7]
        .parse::<u32>()
        .map_err(|_| format!("invalid month in {value:?}"))?;
    let day = value[8..]
        .parse::<u32>()
        .map_err(|_| format!("invalid day in {value:?}"))?;
    Ok((year, month, day))
}

fn parse_clock(value: &str) -> Result<(u32, u32, u32, u32), String> {
    let mut parts = value.split(':');
    let hour = parse_clock_part(parts.next(), "hour", value)?;
    let minute = parse_clock_part(parts.next(), "minute", value)?;
    let seconds = parts
        .next()
        .ok_or_else(|| format!("invalid UTC time {value:?}; expected HH:MM:SS"))?;
    if parts.next().is_some() {
        return Err(format!("invalid UTC time {value:?}; expected HH:MM:SS"));
    }
    let (seconds, fraction) = seconds
        .split_once('.')
        .map_or((seconds, None), |(whole, fraction)| (whole, Some(fraction)));
    let second = seconds
        .parse::<u32>()
        .map_err(|_| format!("invalid second in {value:?}"))?;
    let millis = fraction.map_or(Ok(0), parse_millis)?;
    if hour > 23 || minute > 59 || second > 59 {
        return Err(format!("invalid UTC time {value:?}"));
    }
    Ok((hour, minute, second, millis))
}

fn parse_clock_part(value: Option<&str>, name: &str, full: &str) -> Result<u32, String> {
    value
        .ok_or_else(|| format!("missing {name} in {full:?}"))?
        .parse::<u32>()
        .map_err(|_| format!("invalid {name} in {full:?}"))
}

fn parse_millis(value: &str) -> Result<u32, String> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("invalid fractional second {value:?}"));
    }
    let mut millis = 0;
    for byte in value.bytes().take(3) {
        millis = millis * 10 + u32::from(byte - b'0');
    }
    for _ in value.len().min(3)..3 {
        millis *= 10;
    }
    Ok(millis)
}

fn split_utc_offset(value: &str) -> Result<(&str, i32), String> {
    if let Some(clock) = value.strip_suffix('Z').or_else(|| value.strip_suffix('z')) {
        return Ok((clock, 0));
    }
    let Some(index) = value
        .char_indices()
        .rfind(|(_, ch)| matches!(ch, '+' | '-'))
        .map(|(index, _)| index)
    else {
        return Ok((value, 0));
    };
    let (clock, offset) = value.split_at(index);
    let bytes = offset.as_bytes();
    if bytes.len() != 6
        || bytes[3] != b':'
        || !bytes[1..3].iter().all(u8::is_ascii_digit)
        || !bytes[4..].iter().all(u8::is_ascii_digit)
    {
        return Err(format!(
            "invalid UTC offset {offset:?}; expected Z or +HH:MM"
        ));
    }
    let hours = offset[1..3]
        .parse::<i32>()
        .map_err(|_| format!("invalid UTC offset {offset:?}"))?;
    let minutes = offset[4..]
        .parse::<i32>()
        .map_err(|_| format!("invalid UTC offset {offset:?}"))?;
    if hours > 23 || minutes > 59 {
        return Err(format!("invalid UTC offset {offset:?}"));
    }
    let sign = if offset.starts_with('-') { -1 } else { 1 };
    Ok((clock, sign * (hours * 3_600 + minutes * 60)))
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let leap = year.rem_euclid(4) == 0 && (year.rem_euclid(100) != 0 || year.rem_euclid(400) == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day == 0 || day > days_in_month {
        return None;
    }

    let adjusted_year = year - i64::from(month <= 2);
    let era = adjusted_year.div_euclid(400);
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = i64::from(month) + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

// ---------------------------------------------------------------------------
// ocr
// ---------------------------------------------------------------------------

/// Arguments to `scrozz ocr`.
#[derive(Debug, Clone, Args)]
pub struct OcrArgs {
    /// A capture id, or a path to an image file.
    ///
    /// Resolved as a file if a file exists at that path, otherwise as a capture
    /// id. Use `--capture` or `--file` when a script needs no guessing.
    #[arg(value_name = "CAPTURE|FILE")]
    pub subject: Option<String>,

    /// Recognise text in a stored capture.
    #[arg(long, value_name = "ID", conflicts_with_all = ["subject", "file"])]
    pub capture: Option<String>,

    /// Recognise text in an image file.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["subject", "capture"])]
    pub file: Option<PathBuf>,

    /// Discard blocks the engine is less sure of than this.
    #[arg(long, value_name = "0.0-1.0")]
    pub min_confidence: Option<f32>,
}

/// What `scrozz ocr` should read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OcrSubject {
    /// A stored capture.
    Capture(String),
    /// An image file on disk.
    File(PathBuf),
}

impl OcrArgs {
    /// Resolves the positional and the explicit flags into one subject.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] if nothing was named. `clap` already rejects
    /// naming more than one.
    pub fn resolve(&self) -> CliResult<OcrSubject> {
        if let Some(id) = &self.capture {
            return Ok(OcrSubject::Capture(id.clone()));
        }
        if let Some(path) = &self.file {
            return Ok(OcrSubject::File(path.clone()));
        }
        match &self.subject {
            None => Err(CliError::usage(
                "scrozz ocr needs a capture id or an image file path",
            )),
            Some(subject) => Ok(Self::disambiguate(subject)),
        }
    }

    /// A path that exists is a file; anything else is a capture id.
    fn disambiguate(subject: &str) -> OcrSubject {
        let path = PathBuf::from(subject);
        if path.is_file() {
            OcrSubject::File(path)
        } else {
            OcrSubject::Capture(subject.to_string())
        }
    }

    /// Validates the confidence threshold.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] if it falls outside `0.0..=1.0`.
    pub fn validate(&self) -> CliResult<()> {
        if let Some(min) = self.min_confidence
            && !(0.0..=1.0).contains(&min)
        {
            return Err(CliError::usage(format!(
                "--min-confidence must be between 0.0 and 1.0, got {min}"
            )));
        }
        self.resolve().map(|_| ())
    }
}

// ---------------------------------------------------------------------------
// settings
// ---------------------------------------------------------------------------

/// Arguments to `scrozz settings`.
#[derive(Debug, Clone, Args)]
pub struct SettingsArgs {
    /// The settings operation.
    #[command(subcommand)]
    pub command: SettingsCommand,
}

/// Operations on settings.
#[derive(Debug, Clone, Subcommand)]
pub enum SettingsCommand {
    /// Read a setting, or every setting when no key is given.
    Get {
        /// The setting key, e.g. `capture.format`.
        key: Option<String>,
    },

    /// Write a setting.
    Set {
        /// The setting key.
        key: String,
        /// The new value.
        value: String,
    },
}

impl SettingsArgs {
    /// Whether this invocation modifies stored state.
    ///
    /// Drives the IPC forwarding policy: a write while the app is running has to
    /// happen inside that process, or the two disagree about the current value
    /// until one of them is restarted.
    #[must_use]
    pub const fn is_write(&self) -> bool {
        matches!(self.command, SettingsCommand::Set { .. })
    }
}

// ---------------------------------------------------------------------------
// hotkey
// ---------------------------------------------------------------------------

/// Arguments to `scrozz hotkey`.
#[derive(Debug, Clone, Args)]
pub struct HotkeyArgs {
    /// The hotkey operation.
    #[command(subcommand)]
    pub command: HotkeyCommand,
}

/// Hotkey helpers.
#[derive(Debug, Clone, Subcommand)]
pub enum HotkeyCommand {
    /// Emit compositor keybindings that invoke Scrozz.
    ///
    /// On sway and Hyprland there is no global-shortcut portal, so this is the
    /// only way Scrozz gets a hotkey. Per D26 onboarding shows this output and
    /// asks the user to paste it into their compositor config.
    GenerateConfig {
        /// Which compositor. Detected from the environment when omitted.
        #[arg(long, value_enum)]
        compositor: Option<Compositor>,

        /// Emit only this binding, rather than the full recommended set.
        #[arg(long, value_enum)]
        action: Option<HotkeyAction>,

        /// Override the key combination, e.g. `Super+Shift+4`.
        #[arg(long, requires = "action", value_name = "ACCELERATOR")]
        accelerator: Option<String>,

        /// The Scrozz executable to invoke.
        ///
        /// Defaults to a bare `scrozz`, which requires it on `PATH`. Pass an
        /// absolute path when it is not.
        #[arg(long, default_value = "scrozz", value_name = "PATH")]
        exec: String,
    },
}

/// A compositor whose keybinding syntax Scrozz can emit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Compositor {
    /// sway, and i3-compatible configs.
    Sway,
    /// Hyprland.
    Hyprland,
}

/// An action a compositor keybinding can invoke.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HotkeyAction {
    /// Drag out a region.
    CaptureRegion,
    /// Pick a window.
    CaptureWindow,
    /// Capture the display under the pointer.
    CaptureDisplay,
    /// Capture every display at once.
    CaptureAllDisplays,
    /// Record Screen.
    RecordStart,
    /// Stop the recording in progress.
    RecordStop,
}

impl HotkeyAction {
    /// Every action, in the order `generate-config` emits them.
    #[must_use]
    pub const fn all() -> &'static [Self] {
        &[
            Self::CaptureRegion,
            Self::CaptureWindow,
            Self::CaptureDisplay,
            Self::CaptureAllDisplays,
            Self::RecordStart,
            Self::RecordStop,
        ]
    }

    /// The stable slug used in JSON output and as a settings key suffix.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::CaptureRegion => "capture-region",
            Self::CaptureWindow => "capture-window",
            Self::CaptureDisplay => "capture-display",
            Self::CaptureAllDisplays => "capture-all-displays",
            Self::RecordStart => "record-start",
            Self::RecordStop => "record-stop",
        }
    }

    /// The Scrozz arguments this action invokes.
    ///
    /// Deliberately expressed as real CLI arguments rather than an internal
    /// action name: what a compositor runs is a process, and it must be possible
    /// to paste the generated line into a terminal to see exactly what the
    /// hotkey will do.
    #[must_use]
    pub const fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::CaptureRegion => &["capture", "--interactive", "region"],
            Self::CaptureWindow => &["capture", "--interactive", "window"],
            Self::CaptureDisplay => &["capture", "--display", "active"],
            Self::CaptureAllDisplays => &["capture", "--all-displays"],
            Self::RecordStart => &["record", "--interactive", "region"],
            Self::RecordStop => &["record", "--stop"],
        }
    }

    /// The default key combination.
    ///
    /// `Super` rather than `Ctrl` or `Alt` because on both sway and Hyprland the
    /// super key is the conventional compositor modifier and is least likely to
    /// already be taken by an application.
    ///
    /// The capture actions defer to [`ShortcutAction`] rather than repeating its
    /// table, so that changing a default changes it everywhere. That does mean
    /// generating a compositor config *from* a Mac emits the Mac defaults; the
    /// alternative is two tables that drift, which is the failure this delegation
    /// exists to prevent, and `generate-config` is realistically run on the
    /// machine the config is for.
    #[must_use]
    pub const fn default_accelerator(self) -> &'static str {
        match self {
            Self::CaptureRegion => ShortcutAction::CaptureRegion.default_accelerator_setting(),
            Self::CaptureWindow => ShortcutAction::CaptureWindow.default_accelerator_setting(),
            Self::CaptureDisplay => ShortcutAction::CaptureFullscreen.default_accelerator_setting(),
            Self::CaptureAllDisplays => {
                ShortcutAction::CaptureAllDisplays.default_accelerator_setting()
            }
            Self::RecordStart => "Super+Shift+R",
            Self::RecordStop => "Super+Shift+Escape",
        }
    }

    /// A one-line human description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::CaptureRegion => scrozz_core::product_copy::CAPTURE_AREA,
            Self::CaptureWindow => scrozz_core::product_copy::CAPTURE_WINDOW,
            Self::CaptureDisplay => scrozz_core::product_copy::CAPTURE_FULLSCREEN,
            Self::CaptureAllDisplays => scrozz_core::product_copy::CAPTURE_ALL_DISPLAYS,
            Self::RecordStart => scrozz_core::product_copy::RECORD_SCREEN,
            Self::RecordStop => "Stop recording",
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{:?} should parse: {e}", args))
    }

    fn reject(args: &[&str]) -> clap::Error {
        Cli::try_parse_from(args)
            .err()
            .unwrap_or_else(|| panic!("{:?} should not parse", args))
    }

    #[test]
    fn the_command_tree_is_internally_consistent() {
        // Catches duplicate arg ids, bad group references and malformed
        // conflicts at test time rather than on a user's first invocation.
        Cli::command().debug_assert();
    }

    #[test]
    fn help_uses_the_approved_capture_vocabulary_without_renaming_cli_values() {
        let mut command = Cli::command();
        let root_help = command.render_help().to_string();
        assert!(root_help.contains(scrozz_core::product_copy::RECORD_SCREEN));
        assert!(root_help.contains("Capture Text (OCR)"));
        assert!(root_help.contains("Capture History"));

        let capture = command
            .find_subcommand_mut("capture")
            .expect("capture subcommand");
        let capture_help = capture.render_long_help().to_string();
        for expected in [
            scrozz_core::product_copy::CAPTURE_AREA,
            scrozz_core::product_copy::CAPTURE_PREVIOUS_AREA,
            scrozz_core::product_copy::SELF_TIMER,
            scrozz_core::product_copy::ALL_IN_ONE,
        ] {
            assert!(
                capture_help.contains(expected),
                "{expected}\n{capture_help}"
            );
        }

        assert_eq!(
            InteractiveMode::Region.initial_mode(),
            SelectionMode::Region
        );
        assert_eq!(HotkeyAction::CaptureRegion.slug(), "capture-region");
        assert_eq!(
            HotkeyAction::CaptureRegion.arguments(),
            ["capture", "--interactive", "region"]
        );
    }

    // -- dispatch ---------------------------------------------------------

    #[test]
    fn a_bare_invocation_means_the_gui() {
        assert!(parse(&["scrozz"]).command.is_none());
    }

    #[test]
    fn gui_is_also_nameable_explicitly() {
        assert!(matches!(
            parse(&["scrozz", "gui"]).command,
            Some(Command::Gui)
        ));
    }

    #[test]
    fn unknown_subcommands_are_rejected() {
        reject(&["scrozz", "screenshot"]);
    }

    // -- global flags -----------------------------------------------------

    #[test]
    fn global_flags_work_before_and_after_the_subcommand() {
        assert!(parse(&["scrozz", "--json", "list", "displays"]).global.json);
        assert!(parse(&["scrozz", "list", "displays", "--json"]).global.json);
    }

    #[test]
    fn verbosity_counts_up() {
        assert_eq!(parse(&["scrozz", "list", "displays"]).global.verbose, 0);
        assert_eq!(
            parse(&["scrozz", "-v", "list", "displays"]).global.verbose,
            1
        );
        assert_eq!(
            parse(&["scrozz", "-vvv", "list", "displays"])
                .global
                .verbose,
            3
        );
    }

    #[test]
    fn quiet_and_verbose_are_mutually_exclusive() {
        reject(&["scrozz", "-q", "-v", "list", "displays"]);
    }

    #[test]
    fn ipc_can_be_disabled_globally() {
        assert!(parse(&["scrozz", "--no-ipc", "capture"]).global.no_ipc);
    }

    // -- capture targets --------------------------------------------------

    #[test]
    fn capture_defaults_to_an_interactive_region() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture"]).command else {
            panic!("expected capture")
        };
        assert_eq!(
            args.target.resolve().unwrap(),
            TargetSpec::Interactive(InteractiveMode::Region)
        );
        assert!(args.target.is_interactive());
    }

    #[test]
    fn interactive_takes_an_optional_mode() {
        let cases = [
            ("region", InteractiveMode::Region),
            ("window", InteractiveMode::Window),
            ("display", InteractiveMode::Display),
            ("all-in-one", InteractiveMode::AllInOne),
        ];
        for (name, want) in cases {
            let Some(Command::Capture(args)) =
                parse(&["scrozz", "capture", "--interactive", name]).command
            else {
                panic!("expected capture")
            };
            assert_eq!(
                args.target.resolve().unwrap(),
                TargetSpec::Interactive(want)
            );
        }
    }

    #[test]
    fn bare_interactive_means_region() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture", "--interactive"]).command
        else {
            panic!("expected capture")
        };
        assert_eq!(
            args.target.resolve().unwrap(),
            TargetSpec::Interactive(InteractiveMode::Region)
        );
    }

    #[test]
    fn an_unknown_interactive_mode_is_rejected() {
        reject(&["scrozz", "capture", "--interactive", "everything"]);
    }

    #[test]
    fn a_region_target_parses() {
        let Some(Command::Capture(args)) =
            parse(&["scrozz", "capture", "--region", "10,20,300,400"]).command
        else {
            panic!("expected capture")
        };
        let TargetSpec::Region(rect) = args.target.resolve().unwrap() else {
            panic!("expected a region")
        };
        assert_eq!(rect.origin.x, 10.0);
        assert_eq!(rect.origin.y, 20.0);
        assert_eq!(rect.size.width, 300.0);
        assert_eq!(rect.size.height, 400.0);
    }

    #[test]
    fn a_region_may_sit_at_negative_coordinates() {
        // A display left of or above the primary has negative global origins on
        // both Windows and macOS; rejecting them would make half a multi-monitor
        // desktop uncapturable.
        let region: RegionArg = "-1920,-100,800,600".parse().unwrap();
        assert_eq!(region.x, -1920.0);
        assert_eq!(region.y, -100.0);
    }

    #[test]
    fn a_region_accepts_fractional_points() {
        let region: RegionArg = "0.5,1.25,100,100".parse().unwrap();
        assert_eq!(region.x, 0.5);
        assert_eq!(region.y, 1.25);
    }

    #[test]
    fn a_region_tolerates_whitespace_between_fields() {
        assert_eq!(
            " 1 , 2 , 3 , 4 ".parse::<RegionArg>().unwrap(),
            RegionArg {
                x: 1.0,
                y: 2.0,
                width: 3.0,
                height: 4.0
            }
        );
    }

    #[test]
    fn a_zero_area_region_is_rejected_rather_than_silently_clamped() {
        // Size::new clamps negatives to zero, so without this check a typo
        // produces an empty image instead of an error.
        for raw in ["0,0,0,100", "0,0,100,0", "0,0,-5,100", "0,0,100,-5"] {
            let err = raw.parse::<RegionArg>().unwrap_err();
            assert!(err.contains("no area"), "{raw}: {err}");
        }
    }

    #[test]
    fn a_malformed_region_names_the_expected_format() {
        let err = "10,20,30".parse::<RegionArg>().unwrap_err();
        assert!(err.contains("X,Y,W,H"), "{err}");

        let err = "a,b,c,d".parse::<RegionArg>().unwrap_err();
        assert!(err.contains("not a number"), "{err}");

        let err = "10,20,30,40,50".parse::<RegionArg>().unwrap_err();
        assert!(err.contains("X,Y,W,H"), "{err}");
    }

    #[test]
    fn a_non_finite_region_is_rejected() {
        for raw in ["NaN,0,10,10", "inf,0,10,10"] {
            assert!(
                raw.parse::<RegionArg>().is_err(),
                "{raw} should be rejected"
            );
        }
    }

    #[test]
    fn a_bad_region_fails_at_the_parser() {
        reject(&["scrozz", "capture", "--region", "nonsense"]);
    }

    #[test]
    fn display_selectors_include_the_two_aliases() {
        let cases = [
            ("primary", DisplaySelector::Primary),
            ("active", DisplaySelector::Active),
            ("DP-1", DisplaySelector::Id("DP-1".into())),
        ];
        for (raw, want) in cases {
            let Some(Command::Capture(args)) =
                parse(&["scrozz", "capture", "--display", raw]).command
            else {
                panic!("expected capture")
            };
            assert_eq!(args.target.resolve().unwrap(), TargetSpec::Display(want));
        }
    }

    #[test]
    fn an_empty_selector_is_a_usage_error_not_a_silent_default() {
        for args in [
            vec!["scrozz", "capture", "--display", ""],
            vec!["scrozz", "capture", "--window", "   "],
        ] {
            let Some(Command::Capture(parsed)) = parse(&args).command else {
                panic!("expected capture")
            };
            assert!(parsed.target.resolve().is_err(), "{args:?}");
        }
    }

    #[test]
    fn a_window_target_accepts_a_title_with_spaces() {
        let Some(Command::Capture(args)) =
            parse(&["scrozz", "capture", "--window", "Safari — GitHub"]).command
        else {
            panic!("expected capture")
        };
        assert_eq!(
            args.target.resolve().unwrap(),
            TargetSpec::Window("Safari — GitHub".into())
        );
    }

    #[test]
    fn all_displays_is_its_own_target() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture", "--all-displays"]).command
        else {
            panic!("expected capture")
        };
        assert_eq!(args.target.resolve().unwrap(), TargetSpec::AllDisplays);
        assert!(!args.target.is_interactive());
    }

    #[test]
    fn two_targets_at_once_are_rejected() {
        // Exhaustive over the pairs a user might plausibly try.
        let targets = [
            vec!["--region", "0,0,1,1"],
            vec!["--window", "Safari"],
            vec!["--display", "primary"],
            vec!["--all-displays"],
            vec!["--interactive"],
        ];
        for (i, first) in targets.iter().enumerate() {
            for second in targets.iter().skip(i + 1) {
                let mut args = vec!["scrozz", "capture"];
                args.extend(first.iter().copied());
                args.extend(second.iter().copied());
                reject(&args);
            }
        }
    }

    // -- capture destinations ---------------------------------------------

    #[test]
    fn with_no_destination_a_capture_goes_to_the_configured_folder() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture"]).command else {
            panic!("expected capture")
        };
        assert_eq!(args.sinks(), vec![Sink::DefaultFolder]);
    }

    #[test]
    fn destinations_are_additive() {
        let Some(Command::Capture(args)) =
            parse(&["scrozz", "capture", "--output", "shot.png", "--clipboard"]).command
        else {
            panic!("expected capture")
        };
        assert_eq!(
            args.sinks(),
            vec![Sink::File(PathBuf::from("shot.png")), Sink::Clipboard]
        );
    }

    #[test]
    fn output_has_a_short_form() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture", "-o", "a.png"]).command
        else {
            panic!("expected capture")
        };
        assert_eq!(args.output, Some(PathBuf::from("a.png")));
    }

    #[test]
    fn stdout_and_json_cannot_share_a_stream() {
        // Raw PNG bytes and a JSON document on one file descriptor is garbage
        // that neither a viewer nor a parser can read.
        //
        // clap catches it when `--json` follows the subcommand; when it precedes
        // one, a global argument is merged into the subcommand's matches only
        // after conflict validation, so `Cli::validate` is the backstop. The user
        // must not have to care which order they typed.
        reject(&["scrozz", "capture", "--stdout", "--json"]);

        let cli = parse(&["scrozz", "--json", "capture", "--stdout"]);
        let err = cli.validate().unwrap_err();
        assert_eq!(err.exit(), crate::exit::Exit::Usage);
        assert!(err.to_string().contains("--stdout"), "{err}");
    }

    #[test]
    fn json_is_fine_when_nothing_wants_raw_stdout() {
        assert!(parse(&["scrozz", "--json", "capture"]).validate().is_ok());
        assert!(
            parse(&["scrozz", "--json", "list", "displays"])
                .validate()
                .is_ok()
        );
        assert!(parse(&["scrozz", "--json"]).validate().is_ok());
    }

    #[test]
    fn history_get_to_stdout_also_conflicts_with_json() {
        let cli = parse(&["scrozz", "--json", "history", "get", "abc", "--stdout"]);
        assert!(cli.validate().is_err());
    }

    #[test]
    fn stdout_composes_with_the_clipboard() {
        let Some(Command::Capture(args)) =
            parse(&["scrozz", "capture", "--stdout", "--clipboard"]).command
        else {
            panic!("expected capture")
        };
        assert_eq!(args.sinks(), vec![Sink::Clipboard, Sink::Stdout]);
    }

    // -- capture options --------------------------------------------------

    #[test]
    fn every_format_is_selectable_and_maps_to_the_export_enum() {
        let cases = [
            ("png", Format::Png, scrozz_export::ImageFormat::Png),
            ("jpeg", Format::Jpeg, scrozz_export::ImageFormat::Jpeg),
            ("webp", Format::WebP, scrozz_export::ImageFormat::WebP),
        ];
        for (name, want, exported) in cases {
            let Some(Command::Capture(args)) =
                parse(&["scrozz", "capture", "--format", name]).command
            else {
                panic!("expected capture")
            };
            assert_eq!(args.format(), want);
            assert_eq!(want.to_export(), exported);
            assert_eq!(want.slug(), name);
        }
    }

    #[test]
    fn the_default_format_is_png() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture"]).command else {
            panic!("expected capture")
        };
        assert_eq!(args.format(), Format::Png);
    }

    #[test]
    fn an_unknown_format_is_rejected() {
        reject(&["scrozz", "capture", "--format", "tiff"]);
    }

    #[test]
    fn quality_is_bounded() {
        reject(&["scrozz", "capture", "--format", "jpeg", "--quality", "0"]);
        reject(&["scrozz", "capture", "--format", "jpeg", "--quality", "101"]);
        parse(&["scrozz", "capture", "--format", "jpeg", "--quality", "100"]);
    }

    #[test]
    fn quality_on_a_lossless_format_is_a_usage_error() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture", "--quality", "80"]).command
        else {
            panic!("expected capture")
        };
        let err = args.validate().unwrap_err();
        assert!(err.to_string().contains("lossless"), "{err}");
    }

    #[test]
    fn a_negative_delay_is_a_usage_error() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture", "--delay", "-1"]).command
        else {
            panic!("expected capture")
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn an_unrepresentably_large_delay_is_a_usage_error() {
        let Some(Command::Capture(args)) =
            parse(&["scrozz", "capture", "--delay", "1e300"]).command
        else {
            panic!("expected capture")
        };
        let error = args.validate().unwrap_err();
        assert!(error.to_string().contains("too large"), "{error}");
    }

    #[test]
    fn a_fractional_delay_is_accepted() {
        let Some(Command::Capture(args)) = parse(&["scrozz", "capture", "--delay", "1.5"]).command
        else {
            panic!("expected capture")
        };
        assert_eq!(args.delay, Some(1.5));
        assert!(args.validate().is_ok());
    }

    #[test]
    fn cursor_and_shadow_flags_parse() {
        let Some(Command::Capture(args)) = parse(&[
            "scrozz",
            "capture",
            "--window",
            "Safari",
            "--cursor",
            "--no-window-shadow",
        ])
        .command
        else {
            panic!("expected capture")
        };
        assert!(args.cursor);
        assert!(args.no_window_shadow);
    }

    #[test]
    fn fixed_size_and_aspect_parse_as_positive_pairs() {
        let Some(Command::Capture(args)) = parse(&[
            "scrozz",
            "capture",
            "--interactive",
            "region",
            "--fixed-size",
            "1200x630",
            "--aspect",
            "40:21",
        ])
        .command
        else {
            panic!("expected capture")
        };

        assert_eq!(
            args.fixed_size,
            Some(FixedSizeArg {
                width: 1200.0,
                height: 630.0
            })
        );
        assert_eq!(
            args.aspect,
            Some(AspectArg {
                width: 40.0,
                height: 21.0
            })
        );
        assert!(args.validate().is_ok());
    }

    #[test]
    fn forwarded_relative_outputs_are_resolved_without_changing_process_state() {
        let mut cli = parse(&[
            "scrozz",
            "capture",
            "--output",
            "captures/shot.png",
            "--dry-run",
        ]);
        cli.absolutize_paths(std::path::Path::new("/caller/work"));
        let Some(Command::Capture(args)) = cli.command else {
            panic!("expected capture")
        };
        assert_eq!(
            args.output,
            Some(PathBuf::from("/caller/work/captures/shot.png"))
        );
    }

    #[test]
    fn malformed_size_and_aspect_values_are_rejected_at_parse_time() {
        for argv in [
            vec!["scrozz", "capture", "--fixed-size", "1200"],
            vec!["scrozz", "capture", "--fixed-size", "0x630"],
            vec!["scrozz", "capture", "--aspect", "16"],
            vec!["scrozz", "capture", "--aspect", "16:0"],
            vec!["scrozz", "capture", "--aspect", "16:9:1"],
        ] {
            reject(&argv);
        }
    }

    #[test]
    fn selection_options_reach_the_shared_contract() {
        let Some(Command::Capture(args)) = parse(&[
            "scrozz",
            "capture",
            "--interactive",
            "all-in-one",
            "--fixed-size",
            "800x600",
            "--aspect",
            "4:3",
            "--freeze=false",
            "--magnifier=false",
            "--crosshair=false",
            "--retake",
        ])
        .command
        else {
            panic!("expected capture")
        };
        let remembered = LogicalRect::new(
            LogicalPoint::new(10.0, 20.0),
            LogicalSize::new(800.0, 600.0),
        );
        let options = args
            .selection_options(Some((remembered, None)))
            .unwrap()
            .expect("interactive options");

        assert_eq!(options.mode, SelectionMode::Region);
        assert!(options.hud);
        assert_eq!(
            options.constraint.exact,
            Some(LogicalSize::new(800.0, 600.0))
        );
        assert_eq!(options.constraint.aspect.value(), Some(4.0 / 3.0));
        assert_eq!(options.remembered, Some(remembered));
        assert!(!options.freeze);
        assert!(!options.magnifier);
        assert!(!options.crosshair);
        assert_eq!(options.crosshair_mode, CrosshairMode::Off);
    }

    #[test]
    fn explicit_cli_selection_aids_activate_without_changing_gui_defaults() {
        let Some(Command::Capture(args)) = parse(&[
            "scrozz",
            "capture",
            "--interactive",
            "region",
            "--crosshair",
        ])
        .command
        else {
            panic!("expected capture")
        };
        let options = args.selection_options(None).unwrap().unwrap();

        assert_eq!(options.crosshair_mode, CrosshairMode::Always);
        assert!(options.crosshair);
        assert!(!options.magnifier);
    }

    #[test]
    fn incompatible_exact_size_and_aspect_are_a_usage_error() {
        let Some(Command::Capture(args)) = parse(&[
            "scrozz",
            "capture",
            "--fixed-size",
            "1200x630",
            "--aspect",
            "16:9",
        ])
        .command
        else {
            panic!("expected capture")
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn selection_controls_do_not_silently_apply_to_fixed_targets() {
        let Some(Command::Capture(args)) =
            parse(&["scrozz", "capture", "--region", "0,0,100,100", "--freeze"]).command
        else {
            panic!("expected capture")
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn region_only_controls_are_rejected_for_other_picker_modes() {
        for argv in [
            vec!["scrozz", "capture", "--interactive", "window", "--retake"],
            vec![
                "scrozz",
                "capture",
                "--interactive",
                "display",
                "--fixed-size",
                "100x100",
            ],
            vec![
                "scrozz",
                "capture",
                "--interactive",
                "display",
                "--magnifier",
            ],
            vec![
                "scrozz",
                "capture",
                "--interactive",
                "window",
                "--crosshair",
            ],
        ] {
            let Some(Command::Capture(args)) = parse(&argv).command else {
                panic!("expected capture")
            };
            assert!(args.validate().is_err(), "{argv:?}");
        }
    }

    #[test]
    fn window_selection_stays_live_to_preserve_native_window_capture() {
        let Some(Command::Capture(args)) =
            parse(&["scrozz", "capture", "--interactive", "window"]).command
        else {
            panic!("expected capture")
        };
        let options = args
            .selection_options(None)
            .unwrap()
            .expect("interactive options");
        assert!(!options.freeze);

        let Some(Command::Capture(args)) =
            parse(&["scrozz", "capture", "--interactive", "window", "--freeze"]).command
        else {
            panic!("expected capture")
        };
        assert!(args.validate().is_err());
    }

    // -- beautification and Smart Frame ------------------------------------

    #[test]
    fn beautification_presets_and_overrides_parse_into_typed_values() {
        let Some(Command::Capture(args)) = parse(&[
            "scrozz",
            "capture",
            "--region",
            "0,0,100,80",
            "--beautify",
            "social",
            "--background",
            "#11223380",
            "--padding",
            "24.5",
            "--frame-aspect",
            "wide",
            "--alignment",
            "bottom-right",
            "--auto-balance",
            "--corner-radius",
            "11",
            "--shadow",
            "9",
            "--border",
            "2",
        ])
        .command
        else {
            panic!("expected capture")
        };

        assert_eq!(args.beautify, Some(BeautifyPreset::Social));
        assert_eq!(
            args.background,
            Some(BeautifyBackground::Solid(Color::rgba(
                0x11, 0x22, 0x33, 0x80
            )))
        );
        assert_eq!(args.frame_aspect, Some(BeautifyAspect::Wide));
        assert_eq!(args.alignment, Some(BeautifyAlignment::BottomRight));
        assert!(args.auto_balance);
        assert!(args.requests_beautification());
        assert!(args.validate().is_ok());
    }

    #[test]
    fn every_background_kind_has_a_command_line_representation() {
        assert_eq!(
            "transparent".parse::<BeautifyBackground>().unwrap(),
            BeautifyBackground::Transparent
        );
        assert_eq!(
            "iris".parse::<BeautifyBackground>().unwrap(),
            BeautifyBackground::BuiltIn(BuiltInBackground::Iris)
        );
        assert_eq!(
            "image:/tmp/backdrop.png"
                .parse::<BeautifyBackground>()
                .unwrap(),
            BeautifyBackground::Image(PathBuf::from("/tmp/backdrop.png"))
        );
        assert!("#oops".parse::<BeautifyBackground>().is_err());
    }

    #[test]
    fn d9_refuses_beautification_for_explicit_or_interactive_windows() {
        for argv in [
            vec![
                "scrozz",
                "capture",
                "--window",
                "Safari",
                "--beautify",
                "clean",
            ],
            vec![
                "scrozz",
                "capture",
                "--interactive",
                "window",
                "--padding",
                "20",
            ],
        ] {
            let Some(Command::Capture(args)) = parse(&argv).command else {
                panic!("expected capture")
            };
            let err = args.validate().expect_err("D9 must refuse");
            assert!(err.to_string().contains("window"), "{err}");
            assert!(err.to_string().contains("D9"), "{err}");
        }
    }

    #[test]
    fn explicit_smart_frame_is_allowed_for_window_outer_canvas() {
        for argv in [
            vec!["scrozz", "capture", "--window", "Safari", "--smart-frame"],
            vec![
                "scrozz",
                "capture",
                "--interactive",
                "window",
                "--smart-frame",
                "--padding",
                "40",
            ],
        ] {
            let Some(Command::Capture(args)) = parse(&argv).command else {
                panic!("expected capture")
            };
            assert!(args.smart_frame);
            assert!(args.validate().is_ok(), "{argv:?}");
        }
    }

    #[test]
    fn exact_output_size_parses_and_conflicts_with_ratio() {
        let Some(Command::Capture(args)) = parse(&[
            "scrozz",
            "capture",
            "--region",
            "0,0,100,80",
            "--smart-frame",
            "--size",
            "1080x1350",
        ])
        .command
        else {
            panic!("expected capture")
        };
        assert_eq!(
            args.size.map(BeautifySize::to_model),
            Some(ExactOutputSize::new(1080, 1350))
        );
        assert!(args.validate().is_ok());
        reject(&[
            "scrozz",
            "capture",
            "--smart-frame",
            "--size",
            "1080x1080",
            "--frame-aspect",
            "square",
        ]);
        reject(&["scrozz", "capture", "--smart-frame", "--size", "0x10"]);
        reject(&[
            "scrozz",
            "capture",
            "--smart-frame",
            "--size",
            "50000x50000",
        ]);
    }

    #[test]
    fn window_smart_frame_rejects_subject_modifiers() {
        let Some(Command::Capture(args)) = parse(&[
            "scrozz",
            "capture",
            "--window",
            "Safari",
            "--smart-frame",
            "--shadow",
            "1",
        ])
        .command
        else {
            panic!("expected capture")
        };
        assert!(args.validate().is_err());
    }

    #[test]
    fn invalid_beautification_measurements_are_usage_errors() {
        for value in ["-1", "NaN", "20000"] {
            let Some(Command::Capture(args)) =
                parse(&["scrozz", "capture", "--padding", value]).command
            else {
                panic!("expected capture")
            };
            assert!(args.validate().is_err(), "{value} should fail");
        }
    }

    // -- record -----------------------------------------------------------

    #[test]
    fn record_defaults_to_thirty_fps() {
        let Some(Command::Record(args)) = parse(&["scrozz", "record"]).command else {
            panic!("expected record")
        };
        assert_eq!(args.fps, 30);
        assert!(!args.stop);
    }

    #[test]
    fn record_takes_audio_and_fps_options() {
        let Some(Command::Record(args)) = parse(&[
            "scrozz",
            "record",
            "--display",
            "primary",
            "--microphone",
            "--system-audio",
            "--fps",
            "60",
            "--cursor",
        ])
        .command
        else {
            panic!("expected record")
        };
        assert!(args.microphone);
        assert!(args.system_audio);
        assert_eq!(args.fps, 60);
        assert!(args.cursor);
    }

    #[test]
    fn an_absurd_frame_rate_is_rejected() {
        reject(&["scrozz", "record", "--fps", "0"]);
        reject(&["scrozz", "record", "--fps", "1000"]);
    }

    #[test]
    fn record_stop_takes_no_other_options() {
        // `--stop` addresses a session that already exists; every setting here
        // would be silently ignored, which is worse than refusing.
        reject(&["scrozz", "record", "--stop", "--fps", "60"]);
        reject(&["scrozz", "record", "--stop", "--display", "primary"]);
        reject(&["scrozz", "record", "--stop", "--microphone"]);
        reject(&["scrozz", "record", "--stop", "--output", "a.mp4"]);
        reject(&["scrozz", "record", "--stop", "--cursor"]);
        reject(&["scrozz", "record", "--stop", "--system-audio"]);
    }

    #[test]
    fn record_stop_alone_is_fine() {
        let Some(Command::Record(args)) = parse(&["scrozz", "record", "--stop"]).command else {
            panic!("expected record")
        };
        assert!(args.stop);
    }

    #[test]
    fn the_default_fps_does_not_count_as_a_conflict_with_stop() {
        // `--fps` has a default, and a default must not behave like a value the
        // user typed or `--stop` would be impossible to use.
        assert!(Cli::try_parse_from(["scrozz", "record", "--stop"]).is_ok());
    }

    // -- list -------------------------------------------------------------

    #[test]
    fn list_enumerates_displays_and_windows() {
        let Some(Command::List(args)) = parse(&["scrozz", "list", "displays"]).command else {
            panic!("expected list")
        };
        assert_eq!(args.what, ListWhat::Displays);

        let Some(Command::List(args)) = parse(&["scrozz", "list", "windows"]).command else {
            panic!("expected list")
        };
        assert_eq!(args.what, ListWhat::Windows);
    }

    #[test]
    fn list_requires_a_subject() {
        reject(&["scrozz", "list"]);
        reject(&["scrozz", "list", "monitors"]);
    }

    // -- history ----------------------------------------------------------

    #[test]
    fn history_list_takes_a_limit_and_a_pin_filter() {
        let Some(Command::History(args)) =
            parse(&["scrozz", "history", "list", "--limit", "5", "--pinned"]).command
        else {
            panic!("expected history")
        };
        let HistoryCommand::List { limit, pinned, .. } = args.command else {
            panic!("expected list")
        };
        assert_eq!(limit, 5);
        assert!(pinned);
    }

    #[test]
    fn history_list_parses_pagination_and_every_filter() {
        let Some(Command::History(args)) = parse(&[
            "scrozz",
            "history",
            "list",
            "--limit",
            "25",
            "--offset",
            "50",
            "--kind",
            "screenshots",
            "--search",
            "invoice",
            "--app",
            "Preview",
            "--after",
            "2025-01-01",
            "--before",
            "2025-01-31",
            "--images-only",
        ])
        .command
        else {
            panic!("expected history")
        };
        let HistoryCommand::List {
            limit,
            offset,
            kind,
            search,
            app,
            after,
            before,
            images_only,
            ..
        } = args.command
        else {
            panic!("expected list")
        };
        assert_eq!(limit, 25);
        assert_eq!(offset, 50);
        assert_eq!(kind, Some(MediaKind::Screenshot));
        assert_eq!(search.as_deref(), Some("invoice"));
        assert_eq!(app.as_deref(), Some("Preview"));
        assert_eq!(after, Some(1_735_689_600_000));
        assert_eq!(before, Some(1_738_367_999_999));
        assert!(images_only);
    }

    #[test]
    fn history_dates_accept_rfc3339_offsets_and_unix_times() {
        assert_eq!(
            parse_history_timestamp("2025-01-01T01:30:00+01:30", false).unwrap(),
            1_735_689_600_000
        );
        assert_eq!(
            parse_history_timestamp("2025-01-01T00:00:00.123Z", false).unwrap(),
            1_735_689_600_123
        );
        assert_eq!(
            parse_history_timestamp("1735689600", false).unwrap(),
            1_735_689_600_000
        );
        assert_eq!(
            parse_history_timestamp("1735689600123", false).unwrap(),
            1_735_689_600_123
        );
    }

    #[test]
    fn history_dates_reject_impossible_or_malformed_values() {
        for value in [
            "2025-02-29",
            "2025-01-01T25:00:00Z",
            "2025-01-01T00:00Z",
            "not-a-date",
            "🦀-01-01",
        ] {
            assert!(
                parse_history_timestamp(value, false).is_err(),
                "{value:?} should fail"
            );
        }
    }

    #[test]
    fn history_get_writes_somewhere() {
        let Some(Command::History(args)) =
            parse(&["scrozz", "history", "get", "abc", "-o", "out.png"]).command
        else {
            panic!("expected history")
        };
        let HistoryCommand::Get { id, output, stdout } = args.command else {
            panic!("expected get")
        };
        assert_eq!(id, "abc");
        assert_eq!(output, Some(PathBuf::from("out.png")));
        assert!(!stdout);
    }

    #[test]
    fn history_get_cannot_mix_raw_bytes_and_json() {
        reject(&["scrozz", "history", "get", "abc", "--stdout", "--json"]);
    }

    #[test]
    fn history_delete_takes_one_or_more_ids() {
        let Some(Command::History(args)) =
            parse(&["scrozz", "history", "delete", "a", "b", "c"]).command
        else {
            panic!("expected history")
        };
        let HistoryCommand::Delete { ids } = args.command else {
            panic!("expected delete")
        };
        assert_eq!(ids, ["a", "b", "c"]);

        reject(&["scrozz", "history", "delete"]);
    }

    #[test]
    fn history_pin_can_be_reversed() {
        let Some(Command::History(args)) = parse(&["scrozz", "history", "pin", "a"]).command else {
            panic!("expected history")
        };
        let HistoryCommand::Pin { id, unpin } = args.command else {
            panic!("expected pin")
        };
        assert_eq!(id, "a");
        assert!(!unpin);

        let Some(Command::History(args)) =
            parse(&["scrozz", "history", "pin", "a", "--unpin"]).command
        else {
            panic!("expected history")
        };
        let HistoryCommand::Pin { unpin, .. } = args.command else {
            panic!("expected pin")
        };
        assert!(unpin);
    }

    #[test]
    fn history_unlock_pins_needs_no_capture_id() {
        let Some(Command::History(args)) = parse(&["scrozz", "history", "unlock-pins"]).command
        else {
            panic!("expected history")
        };
        assert!(matches!(args.command, HistoryCommand::UnlockPins));
    }

    // -- ocr --------------------------------------------------------------

    #[test]
    fn ocr_takes_a_positional_subject() {
        let Some(Command::Ocr(args)) = parse(&["scrozz", "ocr", "abc123"]).command else {
            panic!("expected ocr")
        };
        // Nothing exists at that path, so it is a capture id.
        assert_eq!(
            args.resolve().unwrap(),
            OcrSubject::Capture("abc123".into())
        );
    }

    #[test]
    fn ocr_resolves_an_existing_path_as_a_file() {
        // This source file certainly exists; no fixture needed.
        let here = file!();
        let Some(Command::Ocr(args)) = parse(&["scrozz", "ocr", here]).command else {
            panic!("expected ocr")
        };
        let resolved = args.resolve().unwrap();
        if PathBuf::from(here).is_file() {
            assert_eq!(resolved, OcrSubject::File(PathBuf::from(here)));
        } else {
            assert_eq!(resolved, OcrSubject::Capture(here.to_string()));
        }
    }

    #[test]
    fn ocr_flags_remove_the_guessing() {
        let Some(Command::Ocr(args)) = parse(&["scrozz", "ocr", "--capture", "x"]).command else {
            panic!("expected ocr")
        };
        assert_eq!(args.resolve().unwrap(), OcrSubject::Capture("x".into()));

        let Some(Command::Ocr(args)) = parse(&["scrozz", "ocr", "--file", "x.png"]).command else {
            panic!("expected ocr")
        };
        assert_eq!(
            args.resolve().unwrap(),
            OcrSubject::File(PathBuf::from("x.png"))
        );
    }

    #[test]
    fn ocr_subject_flags_are_mutually_exclusive() {
        reject(&["scrozz", "ocr", "--capture", "a", "--file", "b.png"]);
        reject(&["scrozz", "ocr", "positional", "--capture", "a"]);
        reject(&["scrozz", "ocr", "positional", "--file", "b.png"]);
    }

    #[test]
    fn ocr_needs_a_subject() {
        let Some(Command::Ocr(args)) = parse(&["scrozz", "ocr"]).command else {
            panic!("expected ocr")
        };
        assert!(args.resolve().is_err());
    }

    #[test]
    fn ocr_confidence_is_bounded() {
        let Some(Command::Ocr(args)) =
            parse(&["scrozz", "ocr", "a", "--min-confidence", "1.5"]).command
        else {
            panic!("expected ocr")
        };
        assert!(args.validate().is_err());

        let Some(Command::Ocr(args)) =
            parse(&["scrozz", "ocr", "a", "--min-confidence", "0.8"]).command
        else {
            panic!("expected ocr")
        };
        assert!(args.validate().is_ok());
    }

    // -- settings ---------------------------------------------------------

    #[test]
    fn settings_get_takes_an_optional_key() {
        let Some(Command::Settings(args)) = parse(&["scrozz", "settings", "get"]).command else {
            panic!("expected settings")
        };
        assert!(matches!(args.command, SettingsCommand::Get { key: None }));

        let Some(Command::Settings(args)) =
            parse(&["scrozz", "settings", "get", "capture.format"]).command
        else {
            panic!("expected settings")
        };
        let SettingsCommand::Get { key } = args.command else {
            panic!("expected get")
        };
        assert_eq!(key.as_deref(), Some("capture.format"));
    }

    #[test]
    fn settings_set_requires_both_a_key_and_a_value() {
        reject(&["scrozz", "settings", "set"]);
        reject(&["scrozz", "settings", "set", "capture.format"]);

        let Some(Command::Settings(args)) =
            parse(&["scrozz", "settings", "set", "capture.format", "webp"]).command
        else {
            panic!("expected settings")
        };
        let SettingsCommand::Set { key, value } = args.command else {
            panic!("expected set")
        };
        assert_eq!(key, "capture.format");
        assert_eq!(value, "webp");
    }

    // -- hotkey -----------------------------------------------------------

    #[test]
    fn hotkey_generate_config_defaults_to_a_bare_scrozz_on_path() {
        let Some(Command::Hotkey(args)) = parse(&["scrozz", "hotkey", "generate-config"]).command
        else {
            panic!("expected hotkey")
        };
        let HotkeyCommand::GenerateConfig {
            compositor,
            action,
            accelerator,
            exec,
        } = args.command;
        assert!(compositor.is_none());
        assert!(action.is_none());
        assert!(accelerator.is_none());
        assert_eq!(exec, "scrozz");
    }

    #[test]
    fn hotkey_generate_config_accepts_both_compositors() {
        for (name, want) in [
            ("sway", Compositor::Sway),
            ("hyprland", Compositor::Hyprland),
        ] {
            let Some(Command::Hotkey(args)) =
                parse(&["scrozz", "hotkey", "generate-config", "--compositor", name]).command
            else {
                panic!("expected hotkey")
            };
            let HotkeyCommand::GenerateConfig { compositor, .. } = args.command;
            assert_eq!(compositor, Some(want));
        }
    }

    #[test]
    fn overriding_the_accelerator_requires_naming_the_action() {
        // Otherwise "which of the six bindings did you mean?" has no answer.
        reject(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--accelerator",
            "Super+P",
        ]);
        parse(&[
            "scrozz",
            "hotkey",
            "generate-config",
            "--action",
            "capture-region",
            "--accelerator",
            "Super+P",
        ]);
    }

    #[test]
    fn every_hotkey_action_is_nameable_on_the_command_line() {
        for action in HotkeyAction::all() {
            let Some(Command::Hotkey(args)) = parse(&[
                "scrozz",
                "hotkey",
                "generate-config",
                "--action",
                action.slug(),
            ])
            .command
            else {
                panic!("expected hotkey")
            };
            let HotkeyCommand::GenerateConfig { action: parsed, .. } = args.command;
            assert_eq!(parsed, Some(*action));
        }
    }

    #[test]
    fn every_hotkey_action_invokes_a_real_command() {
        // The generated line must be something a user can paste into a terminal.
        for action in HotkeyAction::all() {
            let mut argv = vec!["scrozz"];
            argv.extend(action.arguments().iter().copied());
            Cli::try_parse_from(&argv).unwrap_or_else(|e| {
                panic!("{} emits an invalid command {argv:?}: {e}", action.slug())
            });
        }
    }

    #[test]
    fn hotkey_action_slugs_and_defaults_are_unique() {
        let mut slugs: Vec<&str> = HotkeyAction::all().iter().map(|a| a.slug()).collect();
        slugs.sort_unstable();
        let before = slugs.len();
        slugs.dedup();
        assert_eq!(slugs.len(), before);

        let mut accels: Vec<&str> = HotkeyAction::all()
            .iter()
            .map(|a| a.default_accelerator())
            .collect();
        accels.sort_unstable();
        let before = accels.len();
        accels.dedup();
        assert_eq!(accels.len(), before, "two actions share a key combination");
    }

    // -- command slugs ----------------------------------------------------

    #[test]
    fn command_slugs_are_stable_and_distinct() {
        let cases = [
            (vec!["scrozz", "capture"], "capture"),
            (vec!["scrozz", "record"], "record.start"),
            (vec!["scrozz", "record", "--stop"], "record.stop"),
            (vec!["scrozz", "list", "displays"], "list.displays"),
            (vec!["scrozz", "list", "windows"], "list.windows"),
            (vec!["scrozz", "history", "list"], "history.list"),
            (vec!["scrozz", "history", "get", "a"], "history.get"),
            (vec!["scrozz", "history", "delete", "a"], "history.delete"),
            (vec!["scrozz", "history", "pin", "a"], "history.pin"),
            (vec!["scrozz", "ocr", "a"], "ocr"),
            (vec!["scrozz", "settings", "get"], "settings.get"),
            (vec!["scrozz", "settings", "set", "a", "b"], "settings.set"),
            (
                vec!["scrozz", "hotkey", "generate-config"],
                "hotkey.generate-config",
            ),
            (vec!["scrozz", "gui"], "gui"),
        ];
        let mut seen = Vec::new();
        for (argv, want) in cases {
            let command = parse(&argv).command.expect("a subcommand");
            assert_eq!(command.slug(), want, "{argv:?}");
            seen.push(want);
        }
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), before, "two commands share a slug");
    }

    #[test]
    fn help_and_version_are_available_everywhere() {
        for argv in [
            vec!["scrozz", "--help"],
            vec!["scrozz", "--version"],
            vec!["scrozz", "capture", "--help"],
            vec!["scrozz", "history", "get", "--help"],
            vec!["scrozz", "hotkey", "generate-config", "--help"],
        ] {
            let err = reject(&argv);
            assert!(
                matches!(
                    err.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ),
                "{argv:?} produced {:?}",
                err.kind()
            );
        }
    }
}
