//! Compositor keybindings, generated.
//!
//! # Why this is a feature and not a snippet in the README
//!
//! `xdg-desktop-portal-wlr` implements no `GlobalShortcuts` interface. On sway
//! and Hyprland that is not a rough edge to work around — it means **no
//! application can register a global hotkey at all**. Per decisions D8 and D11
//! the remedy is inverted: the *compositor* owns the binding and invokes the
//! Scrozz CLI, which is the reason the CLI is a platform requirement rather than
//! a convenience.
//!
//! D26 then makes this the one thing onboarding must do on those systems: a user
//! who is not told this concludes the app is broken, because they pressed the
//! hotkey and nothing happened. So Scrozz generates the exact line to paste.
//!
//! # What "correct" means here
//!
//! The generated line has to work when pasted, unmodified, into a config file
//! this program cannot see. That drives three choices:
//!
//! - **Literal modifier names, not `$mod`.** Most sway configs define `$mod`,
//!   but Scrozz cannot know whether it is bound to Super or Alt, and a binding
//!   that lands on the wrong modifier is worse than one that looks verbose.
//! - **Real CLI arguments, not an internal action name.** The user can paste the
//!   command half of the line into a terminal and watch it work, which is the
//!   fastest way to tell a broken binding from a broken app.
//! - **Shell quoting.** Both compositors hand `exec` to `/bin/sh`, so a path
//!   with a space silently becomes two arguments.

use std::fmt;

use crate::{
    cli::{Compositor, HotkeyAction},
    fault::{CliError, CliResult},
    json::Json,
};

/// A modifier key, normalised across the many names each one has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Modifier {
    /// The Super/Command/Windows/Meta key.
    Super,
    /// Control.
    Ctrl,
    /// Alt/Option.
    Alt,
    /// Shift.
    Shift,
}

impl Modifier {
    /// The canonical Scrozz name.
    const fn canonical(self) -> &'static str {
        match self {
            Self::Super => "Super",
            Self::Ctrl => "Ctrl",
            Self::Alt => "Alt",
            Self::Shift => "Shift",
        }
    }

    /// The sway/i3 name.
    ///
    /// `Mod4` and `Mod1` rather than `Super` and `Alt`: both sway and i3 accept
    /// the numbered forms, and a config that also has to work under i3 is common
    /// enough that the more portable spelling is worth the small opacity. The
    /// generated header says what they mean.
    const fn sway(self) -> &'static str {
        match self {
            Self::Super => "Mod4",
            Self::Ctrl => "Ctrl",
            Self::Alt => "Mod1",
            Self::Shift => "Shift",
        }
    }

    /// The Hyprland name.
    const fn hyprland(self) -> &'static str {
        match self {
            Self::Super => "SUPER",
            Self::Ctrl => "CTRL",
            Self::Alt => "ALT",
            Self::Shift => "SHIFT",
        }
    }

    /// Parses any of the names a platform or a user might use.
    fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            // "Cmd" appears because Scrozz's own defaults are written in macOS
            // terms and a user may copy one across.
            "super" | "cmd" | "command" | "meta" | "win" | "windows" | "logo" | "mod4" => {
                Some(Self::Super)
            }
            "ctrl" | "control" | "ctl" => Some(Self::Ctrl),
            "alt" | "option" | "opt" | "mod1" => Some(Self::Alt),
            "shift" => Some(Self::Shift),
            _ => None,
        }
    }
}

/// A parsed key combination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accelerator {
    /// Modifiers, deduplicated and in canonical order.
    pub modifiers: Vec<Modifier>,
    /// The X keysym name of the non-modifier key, e.g. `4`, `r`, `Escape`.
    pub key: String,
}

impl Accelerator {
    /// Parses an accelerator such as `Super+Shift+4`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError::Usage`] if the string is empty, names no key, names
    /// more than one key, or uses an unrecognised key name.
    pub fn parse(raw: &str) -> CliResult<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(CliError::usage("an accelerator cannot be empty"));
        }

        let tokens = split_tokens(trimmed);
        let mut modifiers = Vec::new();
        let mut key: Option<String> = None;

        for token in tokens {
            if let Some(modifier) = Modifier::parse(&token) {
                if !modifiers.contains(&modifier) {
                    modifiers.push(modifier);
                }
                continue;
            }
            let keysym = keysym_for(&token).ok_or_else(|| {
                CliError::usage(format!(
                    "{raw:?} names a key Scrozz does not recognise: {token:?}"
                ))
            })?;
            if let Some(existing) = &key {
                return Err(CliError::usage(format!(
                    "{raw:?} names two keys, {existing:?} and {keysym:?}; \
                     an accelerator has exactly one non-modifier key"
                )));
            }
            key = Some(keysym);
        }

        let key = key.ok_or_else(|| {
            CliError::usage(format!(
                "{raw:?} is only modifiers; an accelerator needs a key, e.g. `Super+Shift+4`"
            ))
        })?;

        modifiers.sort_unstable();
        Ok(Self { modifiers, key })
    }

    /// Renders in sway/i3 syntax, e.g. `Mod4+Shift+4`.
    #[must_use]
    pub fn to_sway(&self) -> String {
        let mut parts: Vec<&str> = self.modifiers.iter().map(|m| m.sway()).collect();
        parts.push(&self.key);
        parts.join("+")
    }

    /// Renders the modifier half of a Hyprland binding, e.g. `SUPER SHIFT`.
    #[must_use]
    pub fn to_hyprland_modifiers(&self) -> String {
        self.modifiers
            .iter()
            .map(|m| m.hyprland())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Renders the key half of a Hyprland binding.
    ///
    /// Single letters are upper-cased purely to match the house style of
    /// Hyprland's own documentation; it treats key names case-insensitively.
    #[must_use]
    pub fn to_hyprland_key(&self) -> String {
        if self.key.chars().count() == 1 && self.key.chars().all(|c| c.is_ascii_alphabetic()) {
            self.key.to_ascii_uppercase()
        } else {
            self.key.clone()
        }
    }
}

impl fmt::Display for Accelerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for modifier in &self.modifiers {
            write!(f, "{}+", modifier.canonical())?;
        }
        f.write_str(&self.key)
    }
}

/// Splits on `+` while letting `+` itself be the bound key.
///
/// `Super++` means Super plus the plus key. A naive `split('+')` yields an empty
/// final token, which would otherwise be reported as an unrecognised key.
fn split_tokens(raw: &str) -> Vec<String> {
    let mut tokens: Vec<String> = raw.split('+').map(|t| t.trim().to_string()).collect();
    if tokens.len() > 1 && tokens.last().is_some_and(String::is_empty) {
        tokens.pop();
        tokens.push("plus".to_string());
    }
    tokens.retain(|t| !t.is_empty());
    tokens
}

/// Maps a user-supplied key name to its X keysym.
///
/// Both sway and Hyprland resolve keys through xkb, so one table serves both.
fn keysym_for(token: &str) -> Option<String> {
    if token.is_empty() {
        return None;
    }

    // Function keys, F1 through F24.
    let lower = token.to_ascii_lowercase();
    if let Some(number) = lower.strip_prefix('f')
        && let Ok(n) = number.parse::<u8>()
        && (1..=24).contains(&n)
    {
        return Some(format!("F{n}"));
    }

    if token.chars().count() == 1 {
        let ch = token.chars().next()?;
        if ch.is_ascii_alphabetic() {
            return Some(ch.to_ascii_lowercase().to_string());
        }
        if ch.is_ascii_digit() {
            return Some(ch.to_string());
        }
        return punctuation_keysym(ch).map(str::to_string);
    }

    let named = match lower.as_str() {
        "escape" | "esc" => "Escape",
        "return" | "enter" => "Return",
        "space" => "space",
        "tab" => "Tab",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "print" | "printscreen" | "prtsc" | "prtscn" | "sysrq" => "Print",
        "up" => "Up",
        "down" => "Down",
        "left" => "Left",
        "right" => "Right",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" | "prior" => "Prior",
        "pagedown" | "pgdn" | "next" => "Next",
        "comma" => "comma",
        "period" | "dot" => "period",
        "slash" => "slash",
        "backslash" => "backslash",
        "minus" | "dash" => "minus",
        "equal" | "equals" => "equal",
        "plus" => "plus",
        "grave" | "backtick" => "grave",
        "semicolon" => "semicolon",
        "apostrophe" | "quote" => "apostrophe",
        "bracketleft" => "bracketleft",
        "bracketright" => "bracketright",
        _ => return None,
    };
    Some(named.to_string())
}

fn punctuation_keysym(ch: char) -> Option<&'static str> {
    Some(match ch {
        ',' => "comma",
        '.' => "period",
        '/' => "slash",
        '\\' => "backslash",
        '-' => "minus",
        '=' => "equal",
        '+' => "plus",
        '`' => "grave",
        ';' => "semicolon",
        '\'' => "apostrophe",
        '[' => "bracketleft",
        ']' => "bracketright",
        _ => return None,
    })
}

/// One generated keybinding.
#[derive(Debug, Clone)]
pub struct Binding {
    /// What it does.
    pub action: HotkeyAction,
    /// The key combination.
    pub accelerator: Accelerator,
    /// The shell command the compositor will run.
    pub command: String,
    /// The complete config line, ready to paste.
    pub line: String,
}

impl Binding {
    /// The JSON representation.
    #[must_use]
    pub fn to_json(&self) -> Json {
        Json::obj([
            ("action", Json::str(self.action.slug())),
            ("description", Json::str(self.action.description())),
            ("accelerator", Json::str(self.accelerator.to_string())),
            ("command", Json::str(&self.command)),
            ("line", Json::str(&self.line)),
        ])
    }
}

/// A complete generated config fragment.
#[derive(Debug, Clone)]
pub struct GeneratedConfig {
    /// The compositor the syntax targets.
    pub compositor: Compositor,
    /// Comment lines explaining what follows.
    pub header: Vec<String>,
    /// The bindings.
    pub bindings: Vec<Binding>,
}

impl GeneratedConfig {
    /// The text to paste into the compositor config.
    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for line in &self.header {
            out.push_str(line);
            out.push('\n');
        }
        for binding in &self.bindings {
            out.push_str(&binding.line);
            out.push('\n');
        }
        out
    }

    /// The JSON representation.
    #[must_use]
    pub fn to_json(&self) -> Json {
        Json::obj([
            ("compositor", Json::str(compositor_slug(self.compositor))),
            ("config_path", Json::str(config_path_hint(self.compositor))),
            (
                "bindings",
                Json::arr(self.bindings.iter().map(Binding::to_json)),
            ),
            ("config", Json::str(self.to_text())),
        ])
    }
}

/// The stable slug for a compositor.
#[must_use]
pub const fn compositor_slug(compositor: Compositor) -> &'static str {
    match compositor {
        Compositor::Sway => "sway",
        Compositor::Hyprland => "hyprland",
    }
}

/// Where the generated lines belong.
#[must_use]
pub const fn config_path_hint(compositor: Compositor) -> &'static str {
    match compositor {
        Compositor::Sway => "~/.config/sway/config",
        Compositor::Hyprland => "~/.config/hypr/hyprland.conf",
    }
}

/// Identifies the running compositor from the environment.
///
/// Returns `None` when nothing recognisable is running, which includes every
/// non-Linux system and every X11 session.
#[must_use]
pub fn detect_compositor() -> Option<Compositor> {
    detect_compositor_from(
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok().as_deref(),
        std::env::var("SWAYSOCK").ok().as_deref(),
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
    )
}

/// The detection rules, separated from the environment so they can be tested.
///
/// The instance-specific variables are checked first because they are set by the
/// compositor itself and cannot be inherited from a stale login session, unlike
/// `XDG_CURRENT_DESKTOP` which is frequently wrong inside nested sessions.
#[must_use]
pub fn detect_compositor_from(
    hyprland_signature: Option<&str>,
    swaysock: Option<&str>,
    current_desktop: Option<&str>,
) -> Option<Compositor> {
    if hyprland_signature.is_some_and(|v| !v.is_empty()) {
        return Some(Compositor::Hyprland);
    }
    if swaysock.is_some_and(|v| !v.is_empty()) {
        return Some(Compositor::Sway);
    }
    let desktop = current_desktop?.to_ascii_lowercase();
    if desktop.contains("hyprland") {
        return Some(Compositor::Hyprland);
    }
    if desktop.contains("sway") {
        return Some(Compositor::Sway);
    }
    None
}

/// Builds the config fragment.
///
/// `only` restricts output to one action; `override_accelerator` replaces its
/// key combination. With neither, every recommended binding is emitted, which is
/// what onboarding shows.
///
/// # Errors
///
/// Returns [`CliError::Usage`] if the override cannot be parsed.
pub fn generate(
    compositor: Compositor,
    exec: &str,
    only: Option<HotkeyAction>,
    override_accelerator: Option<&str>,
) -> CliResult<GeneratedConfig> {
    let actions: Vec<HotkeyAction> =
        only.map_or_else(|| HotkeyAction::all().to_vec(), |action| vec![action]);

    let mut bindings = Vec::with_capacity(actions.len());
    for action in actions {
        let accelerator = match override_accelerator {
            Some(raw) => Accelerator::parse(raw)?,
            None => Accelerator::parse(action.default_accelerator())?,
        };
        let command = shell_command(exec, action.arguments());
        let line = match compositor {
            Compositor::Sway => {
                format!("bindsym {} exec {command}", accelerator.to_sway())
            }
            Compositor::Hyprland => format!(
                "bind = {}, {}, exec, {command}",
                accelerator.to_hyprland_modifiers(),
                accelerator.to_hyprland_key()
            ),
        };
        bindings.push(Binding {
            action,
            accelerator,
            command,
            line,
        });
    }

    Ok(GeneratedConfig {
        compositor,
        header: header_for(compositor),
        bindings,
    })
}

fn header_for(compositor: Compositor) -> Vec<String> {
    let mut header = vec![
        "# Scrozz keybindings.".to_string(),
        format!(
            "# Paste into {}, then reload.",
            config_path_hint(compositor)
        ),
        "# Your compositor has no global-shortcut portal, so it owns these".to_string(),
        "# bindings and runs Scrozz itself. Each command below also works if".to_string(),
        "# you paste it straight into a terminal.".to_string(),
    ];
    if matches!(compositor, Compositor::Sway) {
        header.push("# Mod4 is Super/Logo; Mod1 is Alt.".to_string());
    }
    header
}

fn shell_command(exec: &str, arguments: &[&str]) -> String {
    let mut parts = Vec::with_capacity(arguments.len() + 1);
    parts.push(shell_quote(exec));
    parts.extend(arguments.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

/// Quotes an argument for `/bin/sh`, which is what both compositors use.
///
/// An unquoted path containing a space becomes two arguments and the binding
/// fails with an error the user sees nowhere.
fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '=' | ':'));
    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- accelerator parsing ----------------------------------------------

    #[test]
    fn a_plain_accelerator_parses() {
        let accel = Accelerator::parse("Super+Shift+4").unwrap();
        assert_eq!(accel.modifiers, [Modifier::Super, Modifier::Shift]);
        assert_eq!(accel.key, "4");
        assert_eq!(accel.to_string(), "Super+Shift+4");
    }

    #[test]
    fn every_spelling_of_a_modifier_normalises() {
        for name in [
            "Super", "super", "Cmd", "Command", "Meta", "Win", "Mod4", "logo",
        ] {
            let accel = Accelerator::parse(&format!("{name}+a")).unwrap();
            assert_eq!(accel.modifiers, [Modifier::Super], "{name}");
        }
        for name in ["Ctrl", "Control", "ctl"] {
            assert_eq!(
                Accelerator::parse(&format!("{name}+a")).unwrap().modifiers,
                [Modifier::Ctrl],
                "{name}"
            );
        }
        for name in ["Alt", "Option", "opt", "Mod1"] {
            assert_eq!(
                Accelerator::parse(&format!("{name}+a")).unwrap().modifiers,
                [Modifier::Alt],
                "{name}"
            );
        }
    }

    #[test]
    fn modifier_order_is_canonical_regardless_of_input_order() {
        let a = Accelerator::parse("Shift+Ctrl+Super+p").unwrap();
        let b = Accelerator::parse("Super+Ctrl+Shift+p").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.to_string(), "Super+Ctrl+Shift+p");
    }

    #[test]
    fn a_repeated_modifier_is_not_emitted_twice() {
        let accel = Accelerator::parse("Shift+Shift+a").unwrap();
        assert_eq!(accel.modifiers, [Modifier::Shift]);
    }

    #[test]
    fn letters_become_lowercase_keysyms() {
        assert_eq!(Accelerator::parse("Super+R").unwrap().key, "r");
        assert_eq!(Accelerator::parse("Super+r").unwrap().key, "r");
    }

    #[test]
    fn named_keys_map_to_x_keysyms() {
        let cases = [
            ("Escape", "Escape"),
            ("esc", "Escape"),
            ("Enter", "Return"),
            ("return", "Return"),
            ("Space", "space"),
            ("PrintScreen", "Print"),
            ("prtsc", "Print"),
            ("PageUp", "Prior"),
            ("pgdn", "Next"),
            ("Delete", "Delete"),
            ("BackSpace", "BackSpace"),
            ("Up", "Up"),
        ];
        for (input, want) in cases {
            assert_eq!(
                Accelerator::parse(&format!("Super+{input}")).unwrap().key,
                want,
                "{input}"
            );
        }
    }

    #[test]
    fn function_keys_parse_across_the_whole_range() {
        for n in 1u8..=24 {
            assert_eq!(
                Accelerator::parse(&format!("Super+F{n}")).unwrap().key,
                format!("F{n}")
            );
        }
        assert!(Accelerator::parse("Super+F25").is_err());
        assert!(Accelerator::parse("Super+F0").is_err());
    }

    #[test]
    fn punctuation_keys_map_to_their_keysym_names() {
        let cases = [
            (",", "comma"),
            (".", "period"),
            ("/", "slash"),
            ("-", "minus"),
            ("=", "equal"),
            ("[", "bracketleft"),
            ("]", "bracketright"),
            (";", "semicolon"),
            ("`", "grave"),
        ];
        for (input, want) in cases {
            assert_eq!(
                Accelerator::parse(&format!("Super+{input}")).unwrap().key,
                want,
                "{input}"
            );
        }
    }

    #[test]
    fn plus_can_itself_be_the_bound_key() {
        // `Super++` is the obvious way to write it and would otherwise parse as
        // an empty final token.
        let accel = Accelerator::parse("Super++").unwrap();
        assert_eq!(accel.modifiers, [Modifier::Super]);
        assert_eq!(accel.key, "plus");
    }

    #[test]
    fn malformed_accelerators_are_rejected_with_an_explanation() {
        let cases = [
            ("", "empty"),
            ("   ", "empty"),
            ("Super+Shift", "needs a key"),
            ("Super+a+b", "two keys"),
            ("Super+Nonsense", "does not recognise"),
        ];
        for (input, expected) in cases {
            let err = Accelerator::parse(input).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "{input:?} said {err:?}, expected it to mention {expected:?}"
            );
        }
    }

    #[test]
    fn a_key_with_no_modifier_is_allowed() {
        // Binding a bare Print key to a screenshot is a very common setup.
        let accel = Accelerator::parse("Print").unwrap();
        assert!(accel.modifiers.is_empty());
        assert_eq!(accel.key, "Print");
        assert_eq!(accel.to_sway(), "Print");
        assert_eq!(accel.to_hyprland_modifiers(), "");
    }

    // -- rendering ---------------------------------------------------------

    #[test]
    fn sway_lines_are_pasteable() {
        // The expected accelerator is derived, not spelled out: the default is
        // deliberately platform-specific (`Cmd+Shift+8` on macOS, `Super+Shift+4`
        // elsewhere), so a literal here would assert the authoring machine rather
        // than the format under test.
        let accelerator = Accelerator::parse(HotkeyAction::CaptureRegion.default_accelerator())
            .unwrap()
            .to_sway();
        let config = generate(
            Compositor::Sway,
            "scrozz",
            Some(HotkeyAction::CaptureRegion),
            None,
        )
        .unwrap();
        assert_eq!(
            config.bindings[0].line,
            format!("bindsym {accelerator} exec scrozz capture --interactive region")
        );
        assert!(
            config.bindings[0].line.contains("Mod4"),
            "a sway binding must name the modifier the way sway spells it"
        );
    }

    #[test]
    fn hyprland_lines_are_pasteable() {
        // Derived for the same reason as the sway case above.
        let accelerator =
            Accelerator::parse(HotkeyAction::CaptureRegion.default_accelerator()).unwrap();
        let config = generate(
            Compositor::Hyprland,
            "scrozz",
            Some(HotkeyAction::CaptureRegion),
            None,
        )
        .unwrap();
        assert_eq!(
            config.bindings[0].line,
            format!(
                "bind = {}, {}, exec, scrozz capture --interactive region",
                accelerator.to_hyprland_modifiers(),
                accelerator.key
            )
        );
        assert!(
            config.bindings[0].line.contains("SUPER"),
            "a Hyprland binding must name the modifier the way Hyprland spells it"
        );
    }

    #[test]
    fn hyprland_upper_cases_single_letters_only() {
        let config = generate(
            Compositor::Hyprland,
            "scrozz",
            Some(HotkeyAction::RecordStart),
            None,
        )
        .unwrap();
        assert!(
            config.bindings[0].line.contains(", R, exec,"),
            "{}",
            config.bindings[0].line
        );

        let config = generate(
            Compositor::Hyprland,
            "scrozz",
            Some(HotkeyAction::RecordStop),
            None,
        )
        .unwrap();
        assert!(
            config.bindings[0].line.contains(", Escape, exec,"),
            "{}",
            config.bindings[0].line
        );
    }

    #[test]
    fn generating_everything_covers_every_action_once() {
        for compositor in [Compositor::Sway, Compositor::Hyprland] {
            let config = generate(compositor, "scrozz", None, None).unwrap();
            assert_eq!(config.bindings.len(), HotkeyAction::all().len());
            for (binding, action) in config.bindings.iter().zip(HotkeyAction::all()) {
                assert_eq!(binding.action, *action);
            }
        }
    }

    #[test]
    fn an_override_replaces_the_default() {
        let config = generate(
            Compositor::Sway,
            "scrozz",
            Some(HotkeyAction::CaptureRegion),
            Some("Ctrl+Alt+P"),
        )
        .unwrap();
        assert_eq!(
            config.bindings[0].line,
            "bindsym Ctrl+Mod1+p exec scrozz capture --interactive region"
        );
    }

    #[test]
    fn a_bad_override_is_reported_rather_than_ignored() {
        let err = generate(
            Compositor::Sway,
            "scrozz",
            Some(HotkeyAction::CaptureRegion),
            Some("Super+Nonsense"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("Nonsense"));
    }

    #[test]
    fn an_exec_path_with_spaces_is_quoted() {
        let config = generate(
            Compositor::Sway,
            "/opt/My Apps/scrozz",
            Some(HotkeyAction::CaptureRegion),
            None,
        )
        .unwrap();
        assert!(
            config.bindings[0].line.contains("'/opt/My Apps/scrozz'"),
            "{}",
            config.bindings[0].line
        );
    }

    #[test]
    fn a_quote_in_the_exec_path_cannot_break_out() {
        let quoted = shell_quote("/opt/it's/scrozz");
        assert_eq!(quoted, r"'/opt/it'\''s/scrozz'");
    }

    #[test]
    fn an_ordinary_absolute_path_is_left_unquoted() {
        assert_eq!(shell_quote("/usr/bin/scrozz"), "/usr/bin/scrozz");
        assert_eq!(shell_quote("scrozz"), "scrozz");
    }

    #[test]
    fn the_header_explains_why_the_compositor_owns_the_binding() {
        let config = generate(Compositor::Sway, "scrozz", None, None).unwrap();
        let text = config.to_text();
        assert!(text.contains("global-shortcut portal"));
        assert!(text.contains("~/.config/sway/config"));
        // Mod4 is opaque unless it is explained.
        assert!(text.contains("Mod4 is Super"));
    }

    #[test]
    fn hyprland_gets_its_own_config_path() {
        let config = generate(Compositor::Hyprland, "scrozz", None, None).unwrap();
        assert!(config.to_text().contains("~/.config/hypr/hyprland.conf"));
        assert!(!config.to_text().contains("Mod4"));
    }

    #[test]
    fn every_generated_line_is_a_single_line() {
        for compositor in [Compositor::Sway, Compositor::Hyprland] {
            let config = generate(compositor, "scrozz", None, None).unwrap();
            for binding in &config.bindings {
                assert!(!binding.line.contains('\n'), "{}", binding.line);
            }
        }
    }

    #[test]
    fn generated_text_ends_with_a_newline() {
        let text = generate(Compositor::Sway, "scrozz", None, None)
            .unwrap()
            .to_text();
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn the_json_shape_is_stable() {
        let config = generate(
            Compositor::Hyprland,
            "scrozz",
            Some(HotkeyAction::RecordStop),
            None,
        )
        .unwrap();
        let Json::Obj(pairs) = config.to_json() else {
            panic!("expected an object")
        };
        let keys: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["compositor", "config_path", "bindings", "config"]);

        let Json::Obj(binding) = config.bindings[0].to_json() else {
            panic!("expected an object")
        };
        let keys: Vec<&str> = binding.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            keys,
            ["action", "description", "accelerator", "command", "line"]
        );
    }

    // -- detection ---------------------------------------------------------

    #[test]
    fn hyprland_is_detected_from_its_instance_signature() {
        assert_eq!(
            detect_compositor_from(Some("abc123"), None, None),
            Some(Compositor::Hyprland)
        );
    }

    #[test]
    fn sway_is_detected_from_its_socket() {
        assert_eq!(
            detect_compositor_from(None, Some("/run/user/1000/sway-ipc.sock"), None),
            Some(Compositor::Sway)
        );
    }

    #[test]
    fn the_desktop_variable_is_the_last_resort() {
        assert_eq!(
            detect_compositor_from(None, None, Some("sway")),
            Some(Compositor::Sway)
        );
        assert_eq!(
            detect_compositor_from(None, None, Some("Hyprland")),
            Some(Compositor::Hyprland)
        );
        assert_eq!(
            detect_compositor_from(None, None, Some("wlroots:Hyprland")),
            Some(Compositor::Hyprland)
        );
    }

    #[test]
    fn an_instance_variable_outranks_a_stale_desktop_variable() {
        // A nested session inherits XDG_CURRENT_DESKTOP from its parent, so the
        // compositor's own variable has to win.
        assert_eq!(
            detect_compositor_from(Some("sig"), None, Some("sway")),
            Some(Compositor::Hyprland)
        );
    }

    #[test]
    fn nothing_is_detected_off_wlroots() {
        assert_eq!(detect_compositor_from(None, None, None), None);
        assert_eq!(detect_compositor_from(None, None, Some("GNOME")), None);
        assert_eq!(detect_compositor_from(None, None, Some("KDE")), None);
        assert_eq!(detect_compositor_from(Some(""), Some(""), None), None);
    }

    #[test]
    fn compositor_slugs_and_paths_are_distinct() {
        assert_ne!(
            compositor_slug(Compositor::Sway),
            compositor_slug(Compositor::Hyprland)
        );
        assert_ne!(
            config_path_hint(Compositor::Sway),
            config_path_hint(Compositor::Hyprland)
        );
    }
}
