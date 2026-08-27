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

use std::{path::PathBuf, str::FromStr};

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};
use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};

use crate::fault::{CliError, CliResult};

/// Scrozz — screenshots and screen recording for macOS, Windows and Linux.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "scrozz",
    version,
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
    /// *by* that app, so the result joins the existing capture stack instead of
    /// starting a second copy of Scrozz. This forces the work to happen here.
    #[arg(long, global = true)]
    pub no_ipc: bool,
}

/// A Scrozz subcommand.
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Take a still capture.
    Capture(CaptureArgs),

    /// Record the screen.
    Record(RecordArgs),

    /// List what can be captured.
    List(ListArgs),

    /// Work with the capture history.
    History(HistoryArgs),

    /// Recognise text in a capture or an image file.
    Ocr(OcrArgs),

    /// Read and write settings.
    Settings(SettingsArgs),

    /// Hotkey helpers.
    Hotkey(HotkeyArgs),

    /// Configure launch at login.
    Autostart(AutostartArgs),

    /// Register and handle fixed scrozz:// actions.
    Url(UrlArgs),

    /// Check, prepare, explicitly install, or roll back signed updates.
    Update(UpdateArgs),

    /// Inspect system integration or show a native notification.
    System(SystemArgs),

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
            },
            Self::Ocr(_) => "ocr".into(),
            Self::Settings(args) => match args.command {
                SettingsCommand::Get { .. } => "settings.get".into(),
                SettingsCommand::Set { .. } => "settings.set".into(),
                SettingsCommand::Reset { .. } => "settings.reset".into(),
                SettingsCommand::Path => "settings.path".into(),
            },
            Self::Hotkey(args) => match args.command {
                HotkeyCommand::GenerateConfig { .. } => "hotkey.generate-config".into(),
            },
            Self::Autostart(args) => match args.command {
                AutostartCommand::Status => "autostart.status".into(),
                AutostartCommand::Enable => "autostart.enable".into(),
                AutostartCommand::Disable => "autostart.disable".into(),
            },
            Self::Url(args) => match args.command {
                UrlCommand::Status => "url.status".into(),
                UrlCommand::Register => "url.register".into(),
                UrlCommand::Unregister => "url.unregister".into(),
                UrlCommand::Enable => "url.enable".into(),
                UrlCommand::Disable => "url.disable".into(),
                UrlCommand::Handle { .. } => "url.handle".into(),
            },
            Self::Update(args) => match args.command {
                UpdateCommand::Status => "update.status".into(),
                UpdateCommand::Check { .. } => "update.check".into(),
                UpdateCommand::Download { .. } => "update.download".into(),
                UpdateCommand::Stage { .. } => "update.stage".into(),
                UpdateCommand::Install { .. } => "update.install".into(),
                UpdateCommand::Recover => "update.recover".into(),
                UpdateCommand::Rollback => "update.rollback".into(),
                UpdateCommand::Reset => "update.reset".into(),
            },
            Self::System(args) => match args.command {
                SystemCommand::Status => "system.status".into(),
                SystemCommand::Notify { .. } => "system.notify".into(),
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

    /// Pick the target on screen. Defaults to a region.
    ///
    /// `--interactive window` is the documented path on Wayland, where windows
    /// cannot be enumerated and the desktop portal runs its own picker.
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
    /// Drag out a rectangle.
    Region,
    /// Click a window.
    Window,
    /// Click a display.
    Display,
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
    /// Repeated frames from one display, assembled while its content scrolls.
    Scrolling(DisplaySelector),
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

/// Arguments to `scrozz capture`.
#[derive(Debug, Clone, Args)]
pub struct CaptureArgs {
    /// What to capture.
    #[command(flatten)]
    pub target: TargetArgs,

    /// Capture a scrolling page on a display.
    ///
    /// With no value, uses the display under the pointer. A selector may be an
    /// id, `primary`, or `active`.
    #[arg(
        long,
        value_name = "ID|primary|active",
        num_args = 0..=1,
        default_missing_value = "active",
        conflicts_with = "target"
    )]
    pub scrolling: Option<String>,

    /// Composite the pointer into the capture.
    #[arg(long)]
    pub cursor: bool,

    /// Wait this many seconds before capturing.
    ///
    /// `allow_hyphen_values` so that `--delay -1` reaches the validator and is
    /// rejected as a bad delay, rather than being reported by clap as an unknown
    /// flag named `-1`. Both are exit code 2; only one of them is a useful thing
    /// to read at three in the morning.
    #[arg(long, value_name = "SECS", allow_hyphen_values = true)]
    pub delay: Option<f64>,

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
    /// Resolves the ordinary target flags or the scrolling-capture selector.
    pub fn target_spec(&self) -> CliResult<TargetSpec> {
        self.scrolling.as_ref().map_or_else(
            || self.target.resolve(),
            |selector| Ok(TargetSpec::Scrolling(parse_display_selector(selector)?)),
        )
    }

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

    /// Validates combinations `clap` cannot express.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] for a negative or non-finite delay, or for
    /// `--quality` on a format that has no quality setting.
    pub fn validate(&self) -> CliResult<()> {
        if let Some(delay) = self.delay
            && (!delay.is_finite() || delay < 0.0)
        {
            return Err(CliError::usage(format!(
                "--delay must be a non-negative number of seconds, got {delay}"
            )));
        }
        if self.quality.is_some() && self.format() == Format::Png {
            return Err(CliError::usage(
                "--quality has no meaning for PNG, which is lossless; \
                 use --format jpeg or --format webp",
            ));
        }
        self.target_spec()?;
        Ok(())
    }
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
    /// Unavailable on Wayland, which has no window enumeration protocol. There
    /// the answer is `scrozz capture --interactive window`, which asks the
    /// desktop portal to run its own picker.
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
        #[arg(long, value_name = "N")]
        limit: Option<usize>,

        /// Show only pinned captures.
        #[arg(long)]
        pinned: bool,
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

    /// Restore one setting, or every setting, to its default.
    Reset {
        /// The setting key. Omit it to reset every override.
        key: Option<String>,
    },

    /// Print the settings file path.
    Path,
}

impl SettingsArgs {
    /// Whether this invocation modifies stored state.
    ///
    /// Drives the IPC forwarding policy: a write while the app is running has to
    /// happen inside that process, or the two disagree about the current value
    /// until one of them is restarted.
    #[must_use]
    pub const fn is_write(&self) -> bool {
        matches!(
            self.command,
            SettingsCommand::Set { .. } | SettingsCommand::Reset { .. }
        )
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
    /// Start recording a region.
    RecordStart,
    /// Stop the recording in progress.
    RecordStop,
}

// ---------------------------------------------------------------------------
// system integration
// ---------------------------------------------------------------------------

/// Arguments to `scrozz autostart`.
#[derive(Debug, Clone, Args)]
pub struct AutostartArgs {
    /// The launch-at-login operation.
    #[command(subcommand)]
    pub command: AutostartCommand,
}

/// Launch-at-login operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum AutostartCommand {
    /// Report whether the installed entry matches this executable.
    Status,
    /// Start Scrozz when this user logs in.
    Enable,
    /// Remove Scrozz from this user's login.
    Disable,
}

/// Arguments to `scrozz url`.
#[derive(Debug, Clone, Args)]
pub struct UrlArgs {
    /// The URL-scheme operation.
    #[command(subcommand)]
    pub command: UrlCommand,
}

/// URL-scheme operations.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum UrlCommand {
    /// Report OS registration and the independent master toggle.
    Status,
    /// Register Scrozz as a handler without enabling automation.
    Register,
    /// Remove the handler and turn the master toggle off.
    Unregister,
    /// Allow registered, fixed URL actions.
    Enable,
    /// Refuse every incoming URL action.
    Disable,
    /// Handle one URL delivered by the operating system.
    #[command(hide = true)]
    Handle {
        /// The exact scrozz:// URL.
        url: String,
    },
}

impl UrlArgs {
    /// Whether this operation changes shared settings state.
    #[must_use]
    pub const fn writes_settings(&self) -> bool {
        matches!(
            self.command,
            UrlCommand::Enable | UrlCommand::Disable | UrlCommand::Unregister
        )
    }
}

/// Arguments to `scrozz update`.
#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    /// The signed-update operation.
    #[command(subcommand)]
    pub command: UpdateCommand,
}

/// Signed-update operations.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum UpdateCommand {
    /// Show the durable updater state.
    Status,
    /// Fetch and verify a signed manifest without downloading an artifact.
    Check {
        /// HTTPS JSON manifest URL.
        #[arg(long)]
        manifest_url: String,
        /// HTTPS detached-signature envelope URL.
        #[arg(long)]
        signature_url: String,
    },
    /// Download and verify the accepted platform artifact.
    Download {
        /// New file path. It must not already exist.
        #[arg(long)]
        output: PathBuf,
    },
    /// Copy a verified download into a sibling staging file.
    Stage {
        /// New staging path. It must not already exist.
        #[arg(long)]
        output: PathBuf,
    },
    /// Explicitly swap a staged regular file into place.
    Install {
        /// Current installed file.
        #[arg(long)]
        installed: PathBuf,
        /// Sibling path that retains the previous installed file.
        #[arg(long)]
        previous: PathBuf,
        /// Sibling path that preserves a candidate removed by rollback.
        #[arg(long)]
        failed_candidate: PathBuf,
    },
    /// Reconcile an explicitly started install after a recoverable failure.
    Recover,
    /// Restore the retained previous installation.
    Rollback,
    /// Abandon pre-install or terminal state while preserving the anti-replay watermark.
    Reset,
}

/// Arguments to `scrozz system`.
#[derive(Debug, Clone, Args)]
pub struct SystemArgs {
    /// The system-integration operation.
    #[command(subcommand)]
    pub command: SystemCommand,
}

/// System-integration operations.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum SystemCommand {
    /// Report identity, registration, consent, and updater state.
    Status,
    /// Show one native desktop notification.
    Notify {
        /// Notification title.
        #[arg(long)]
        title: String,
        /// Notification body.
        #[arg(long)]
        body: String,
    },
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
    #[must_use]
    pub const fn default_accelerator(self) -> &'static str {
        match self {
            Self::CaptureRegion => "Super+Shift+4",
            Self::CaptureWindow => "Super+Shift+5",
            Self::CaptureDisplay => "Super+Shift+3",
            Self::CaptureAllDisplays => "Super+Shift+6",
            Self::RecordStart => "Super+Shift+R",
            Self::RecordStop => "Super+Shift+Escape",
        }
    }

    /// A one-line human description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::CaptureRegion => "Capture a region",
            Self::CaptureWindow => "Capture a window",
            Self::CaptureDisplay => "Capture the display under the pointer",
            Self::CaptureAllDisplays => "Capture every display",
            Self::RecordStart => "Start recording a region",
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
    fn scrolling_defaults_to_the_active_display_and_accepts_an_id() {
        for (args, want) in [
            (
                vec!["scrozz", "capture", "--scrolling"],
                DisplaySelector::Active,
            ),
            (
                vec!["scrozz", "capture", "--scrolling=primary"],
                DisplaySelector::Primary,
            ),
            (
                vec!["scrozz", "capture", "--scrolling=DP-1"],
                DisplaySelector::Id("DP-1".to_owned()),
            ),
        ] {
            let Some(Command::Capture(parsed)) = parse(&args).command else {
                panic!("expected capture")
            };
            assert_eq!(parsed.target_spec().unwrap(), TargetSpec::Scrolling(want));
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
            vec!["--scrolling=active"],
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
        let HistoryCommand::List { limit, pinned } = args.command else {
            panic!("expected list")
        };
        assert_eq!(limit, Some(5));
        assert!(pinned);
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

    #[test]
    fn settings_reset_accepts_one_key_or_every_key() {
        let Some(Command::Settings(args)) =
            parse(&["scrozz", "settings", "reset", "capture.format"]).command
        else {
            panic!("expected settings")
        };
        assert!(matches!(
            args.command,
            SettingsCommand::Reset { key: Some(ref key) } if key == "capture.format"
        ));

        let Some(Command::Settings(args)) = parse(&["scrozz", "settings", "reset"]).command else {
            panic!("expected settings")
        };
        assert!(matches!(args.command, SettingsCommand::Reset { key: None }));
    }

    #[test]
    fn settings_path_is_a_read_only_command() {
        let Some(Command::Settings(args)) = parse(&["scrozz", "settings", "path"]).command else {
            panic!("expected settings")
        };
        assert!(matches!(args.command, SettingsCommand::Path));
        assert!(!args.is_write());
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
            (vec!["scrozz", "autostart", "status"], "autostart.status"),
            (vec!["scrozz", "autostart", "enable"], "autostart.enable"),
            (vec!["scrozz", "autostart", "disable"], "autostart.disable"),
            (vec!["scrozz", "url", "status"], "url.status"),
            (vec!["scrozz", "url", "register"], "url.register"),
            (vec!["scrozz", "url", "unregister"], "url.unregister"),
            (vec!["scrozz", "url", "enable"], "url.enable"),
            (vec!["scrozz", "url", "disable"], "url.disable"),
            (
                vec!["scrozz", "url", "handle", "scrozz://capture/region"],
                "url.handle",
            ),
            (vec!["scrozz", "update", "status"], "update.status"),
            (
                vec![
                    "scrozz",
                    "update",
                    "check",
                    "--manifest-url",
                    "https://updates.example/manifest.json",
                    "--signature-url",
                    "https://updates.example/manifest.sig",
                ],
                "update.check",
            ),
            (
                vec!["scrozz", "update", "download", "--output", "candidate"],
                "update.download",
            ),
            (
                vec!["scrozz", "update", "stage", "--output", "staged"],
                "update.stage",
            ),
            (
                vec![
                    "scrozz",
                    "update",
                    "install",
                    "--installed",
                    "scrozz",
                    "--previous",
                    "scrozz.previous",
                    "--failed-candidate",
                    "scrozz.failed",
                ],
                "update.install",
            ),
            (vec!["scrozz", "update", "recover"], "update.recover"),
            (vec!["scrozz", "update", "rollback"], "update.rollback"),
            (vec!["scrozz", "update", "reset"], "update.reset"),
            (vec!["scrozz", "system", "status"], "system.status"),
            (
                vec![
                    "scrozz",
                    "system",
                    "notify",
                    "--title",
                    "Ready",
                    "--body",
                    "Update downloaded",
                ],
                "system.notify",
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
