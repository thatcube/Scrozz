//! Naming saved captures.
//!
//! Users configure a pattern; this turns it into a filename that will actually
//! survive being written. Three things make that harder than it sounds, and each
//! is a real bug this module exists to prevent:
//!
//! - **The illegal-character set is not the local one.** Per decision D18 the
//!   save folder is deliberately allowed to be a Dropbox, iCloud or OneDrive
//!   directory, so a file written on macOS is very likely read on Windows. A
//!   name sanitised only for the local platform syncs into a file the other
//!   machine cannot open. Everything here is therefore sanitised for the *union*
//!   of all three platforms by default.
//! - **Titles are unbounded.** A window title is whatever the app felt like —
//!   a full document path, an entire tweet. Filesystems cap a path component at
//!   255 bytes and Windows caps a whole path at 260 characters, so a template
//!   containing `{title}` must be budgeted, not merely rendered.
//! - **Two captures can want the same name.** Burst-capturing within one second
//!   is normal, and `exists()`-then-`write()` is a race. The disambiguation
//!   suffix also has to fit inside the same length budget, which is precisely
//!   the case a naive implementation gets wrong.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use scrozz_core::{Error, Result, ScaleFactor};

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

/// Broken-down civil time.
///
/// # Why the caller may supply this
///
/// The standard library has no time zone database, so nothing in this crate can
/// convert an instant to *local* time — and a screenshot named with a UTC time
/// is confusing for anyone not on UTC. Rather than pull in a time zone crate
/// here, the components are a plain value the application layer can construct
/// from whatever local-time source it already has. [`Timestamp::now_utc`] is the
/// honest fallback when it has none, and is what makes tests deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    /// Full year, e.g. 2026.
    pub year: i64,
    /// Month, 1–12.
    pub month: u32,
    /// Day of month, 1–31.
    pub day: u32,
    /// Hour, 0–23.
    pub hour: u32,
    /// Minute, 0–59.
    pub minute: u32,
    /// Second, 0–59.
    pub second: u32,
}

impl Timestamp {
    /// A timestamp from its components, unvalidated.
    #[must_use]
    pub const fn new(year: i64, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Self {
        Self {
            year,
            month,
            day,
            hour,
            minute,
            second,
        }
    }

    /// The current time in UTC.
    #[must_use]
    pub fn now_utc() -> Self {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        Self::from_unix_seconds(seconds)
    }

    /// Converts Unix seconds to civil time, proleptic Gregorian.
    #[must_use]
    pub fn from_unix_seconds(seconds: i64) -> Self {
        let days = seconds.div_euclid(86_400);
        let rem = seconds.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        Self {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            minute: ((rem % 3600) / 60) as u32,
            second: (rem % 60) as u32,
        }
    }
}

/// Howard Hinnant's `civil_from_days`, era-based and branch-light.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

// ---------------------------------------------------------------------------
// Templates
// ---------------------------------------------------------------------------

/// Everything a template can interpolate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamingContext {
    /// When the capture was taken.
    pub timestamp: Option<Timestamp>,
    /// The captured application's name, if known.
    pub app: Option<String>,
    /// The captured window's title, if known.
    ///
    /// Wayland cannot supply this at all, which is why it is optional rather
    /// than an empty string: a template can then be told a value is missing.
    pub title: Option<String>,
    /// A monotonic counter, for templates that prefer numbering to timestamps.
    pub sequence: u64,
    /// Capture width in pixels.
    pub width: u32,
    /// Capture height in pixels.
    pub height: u32,
}

impl NamingContext {
    /// A context stamped with the current UTC time.
    #[must_use]
    pub fn now() -> Self {
        Self {
            timestamp: Some(Timestamp::now_utc()),
            ..Self::default()
        }
    }

    /// Sets the window title.
    #[must_use]
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Sets the application name.
    #[must_use]
    pub fn with_app(mut self, app: impl Into<String>) -> Self {
        self.app = Some(app.into());
        self
    }

    /// Sets the sequence number.
    #[must_use]
    pub const fn with_sequence(mut self, sequence: u64) -> Self {
        self.sequence = sequence;
        self
    }
}

/// One piece of a parsed template.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Literal(String),
    Field(Field),
}

/// A substitutable value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    /// `YYYY-MM-DD`.
    Date,
    /// `HH-MM-SS`. Hyphens, not colons: a colon is illegal on Windows and is
    /// displayed as a path separator by the macOS Finder.
    Time,
    App,
    Title,
    Sequence,
    Width,
    Height,
}

impl Field {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "year" => Self::Year,
            "month" => Self::Month,
            "day" => Self::Day,
            "hour" => Self::Hour,
            "minute" => Self::Minute,
            "second" => Self::Second,
            "date" => Self::Date,
            "time" => Self::Time,
            "app" => Self::App,
            "title" => Self::Title,
            "seq" => Self::Sequence,
            "width" => Self::Width,
            "height" => Self::Height,
            _ => return None,
        })
    }

    /// Whether the value comes from the captured window rather than the clock.
    ///
    /// These are the unbounded ones, and the only ones capped by
    /// [`NamePolicy::max_field_chars`].
    const fn is_from_window(self) -> bool {
        matches!(self, Self::App | Self::Title)
    }

    fn render(self, ctx: &NamingContext) -> String {
        let t = ctx.timestamp;
        match self {
            Self::Year => t.map_or_else(String::new, |t| format!("{:04}", t.year)),
            Self::Month => t.map_or_else(String::new, |t| format!("{:02}", t.month)),
            Self::Day => t.map_or_else(String::new, |t| format!("{:02}", t.day)),
            Self::Hour => t.map_or_else(String::new, |t| format!("{:02}", t.hour)),
            Self::Minute => t.map_or_else(String::new, |t| format!("{:02}", t.minute)),
            Self::Second => t.map_or_else(String::new, |t| format!("{:02}", t.second)),
            Self::Date => t.map_or_else(String::new, |t| {
                format!("{:04}-{:02}-{:02}", t.year, t.month, t.day)
            }),
            Self::Time => t.map_or_else(String::new, |t| {
                format!("{:02}-{:02}-{:02}", t.hour, t.minute, t.second)
            }),
            Self::App => ctx.app.clone().unwrap_or_default(),
            Self::Title => ctx.title.clone().unwrap_or_default(),
            Self::Sequence => ctx.sequence.to_string(),
            Self::Width => ctx.width.to_string(),
            Self::Height => ctx.height.to_string(),
        }
    }
}

/// A parsed, user-configurable filename pattern.
///
/// Fields are written `{date}`, `{app}` and so on; a literal brace is `{{` or
/// `}}`. The recognised fields are `year`, `month`, `day`, `hour`, `minute`,
/// `second`, `date`, `time`, `app`, `title`, `seq`, `width` and `height`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameTemplate {
    tokens: Vec<Token>,
    source: String,
}

impl NameTemplate {
    /// The out-of-the-box pattern.
    pub const DEFAULT: &'static str = "Screenshot {date} at {time}";

    /// Parses a pattern.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] for an unclosed brace or an unknown
    /// field. This is deliberately strict: an unrecognised field silently passed
    /// through would put `{titel}` into every filename the user saves, and they
    /// would find out days later. A settings screen can call this to validate as
    /// the user types.
    pub fn parse(source: &str) -> Result<Self> {
        let mut tokens = Vec::new();
        let mut literal = String::new();
        let mut chars = source.chars().peekable();

        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    literal.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    literal.push('}');
                }
                '{' => {
                    let mut name = String::new();
                    let mut closed = false;
                    for c in chars.by_ref() {
                        if c == '}' {
                            closed = true;
                            break;
                        }
                        name.push(c);
                    }
                    if !closed {
                        return Err(Error::InvalidRequest(format!(
                            "filename template has an unclosed '{{' before '{name}'"
                        )));
                    }
                    let field = Field::parse(&name).ok_or_else(|| {
                        Error::InvalidRequest(format!(
                            "unknown filename field '{{{name}}}'; known fields are \
                             year, month, day, hour, minute, second, date, time, \
                             app, title, seq, width, height"
                        ))
                    })?;
                    if !literal.is_empty() {
                        tokens.push(Token::Literal(std::mem::take(&mut literal)));
                    }
                    tokens.push(Token::Field(field));
                }
                '}' => {
                    return Err(Error::InvalidRequest(
                        "filename template has an unmatched '}'; write '}}' for a literal brace"
                            .into(),
                    ));
                }
                _ => literal.push(c),
            }
        }
        if !literal.is_empty() {
            tokens.push(Token::Literal(literal));
        }
        Ok(Self {
            tokens,
            source: source.to_owned(),
        })
    }

    /// The pattern as written by the user.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Substitutes fields. The result is *not* yet safe to use as a filename.
    #[must_use]
    pub fn render(&self, ctx: &NamingContext, policy: &NamePolicy) -> String {
        let mut out = String::new();
        for token in &self.tokens {
            match token {
                Token::Literal(s) => out.push_str(s),
                Token::Field(f) => {
                    let mut value = f.render(ctx);
                    if f.is_from_window()
                        && let Some(cap) = policy.max_field_chars
                    {
                        value = truncate_chars(&value, cap);
                    }
                    out.push_str(&value);
                }
            }
        }
        out
    }
}

impl Default for NameTemplate {
    fn default() -> Self {
        // The default template is a constant this module owns, so it parses.
        Self::parse(Self::DEFAULT).expect("the default template parses")
    }
}

impl fmt::Display for NameTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.source)
    }
}

// ---------------------------------------------------------------------------
// Sanitisation
// ---------------------------------------------------------------------------

/// Which platform's filename rules to obey.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FilenameRules {
    /// The union of every platform's restrictions.
    ///
    /// The default, and the right one, because captures are routinely written
    /// into a synced folder that another operating system will read.
    #[default]
    Portable,
    /// Only what the local filesystem actually rejects.
    ///
    /// For a folder the user has said is local-only and wants readable names in.
    Native,
}

/// How a rendered template becomes a usable filename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamePolicy {
    /// Which restrictions to apply.
    pub rules: FilenameRules,
    /// Maximum bytes in one path component.
    ///
    /// 255 on APFS, ext4, NTFS and effectively everything else. Counted in
    /// UTF-8 bytes, which is the strict reading: NTFS counts UTF-16 units, and
    /// no string has more UTF-16 units than UTF-8 bytes.
    pub max_component_bytes: usize,
    /// Maximum bytes in a whole path.
    ///
    /// 260 is the classic Windows `MAX_PATH`. It is opt-in-escapable on modern
    /// Windows, but plenty of software still trips over it, and a screenshot
    /// nobody can open is worse than a screenshot with a shorter name.
    pub max_path_bytes: usize,
    /// Cap on `{app}` and `{title}`, whose values are unbounded.
    pub max_field_chars: Option<usize>,
    /// Used when sanitising leaves nothing at all.
    pub fallback_stem: String,
}

impl Default for NamePolicy {
    fn default() -> Self {
        Self {
            rules: FilenameRules::default(),
            max_component_bytes: 255,
            max_path_bytes: 260,
            max_field_chars: Some(80),
            fallback_stem: "Screenshot".into(),
        }
    }
}

/// Reserved DOS device names, still reserved on current Windows.
///
/// `CON.png` is reserved too — the check is against the part before the first
/// dot, not the whole name.
const WINDOWS_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

impl NamePolicy {
    fn is_illegal(&self, c: char) -> bool {
        match self.rules {
            FilenameRules::Portable => {
                matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            }
            // A colon is legal in a POSIX filename but the macOS Finder renders
            // it as '/', so it is excluded on Unix too rather than producing a
            // name that looks like a path in every file dialog.
            FilenameRules::Native if cfg!(windows) => {
                matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
            }
            FilenameRules::Native => matches!(c, '/' | ':'),
        }
    }

    /// Makes an arbitrary string safe to use as a filename stem.
    ///
    /// Never returns an empty string: a name that sanitises away entirely — a
    /// title of `"???"`, say — falls back rather than producing a file called
    /// `.png`.
    #[must_use]
    pub fn sanitise(&self, raw: &str) -> String {
        let mut out = String::with_capacity(raw.len());
        for c in raw.chars() {
            if c.is_control() {
                // A tab or a newline is invisible but the user does see the
                // separation it creates, so it becomes a space; dropping it
                // outright would run two words together. Everything else
                // invisible is dropped rather than turned into a mark that was
                // never on screen in the first place.
                if c.is_whitespace() && !out.ends_with(' ') {
                    out.push(' ');
                }
                continue;
            }
            if self.is_illegal(c) {
                // Collapse runs, so "a//b" is "a-b" rather than "a--b".
                if !out.ends_with('-') {
                    out.push('-');
                }
            } else {
                out.push(c);
            }
        }

        // Windows silently strips trailing dots and spaces, which turns
        // "report." into "report" and breaks any code that expected the name
        // back. Leading dots would make the capture hidden on Unix.
        let trimmed = out.trim_matches(|c: char| c == '.' || c.is_whitespace());
        let mut stem = trimmed.to_owned();

        if stem.is_empty() {
            return self.fallback_stem.clone();
        }
        if self.reserves_device_names() {
            let head = stem.split('.').next().unwrap_or(&stem).to_ascii_uppercase();
            if WINDOWS_DEVICE_NAMES.contains(&head.as_str()) {
                stem.push('_');
            }
        }
        stem
    }

    const fn reserves_device_names(&self) -> bool {
        matches!(self.rules, FilenameRules::Portable) || cfg!(windows)
    }

    /// The byte budget for a stem, given where the file is going.
    ///
    /// `reserved` is space to keep free for a disambiguating suffix — the detail
    /// a naive implementation forgets, producing a truncated name that is then
    /// pushed back over the limit by ` 2`.
    fn stem_budget(&self, directory: Option<&Path>, extension: &str, reserved: usize) -> usize {
        let dot_extension = extension.len() + 1;
        let mut budget = self.max_component_bytes;
        if let Some(dir) = directory {
            // +1 for the separator between directory and filename.
            let prefix = dir.as_os_str().len() + 1;
            budget = budget.min(self.max_path_bytes.saturating_sub(prefix));
        }
        budget.saturating_sub(dot_extension + reserved)
    }

    /// Renders, sanitises and length-clamps a template into a bare filename.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if `directory` is so long that no
    /// filename fits inside the path limit. That is a real condition worth
    /// telling the user about — the remedy is a shorter save folder — and it is
    /// far better than writing a file whose name has been truncated to nothing.
    pub fn file_name(
        &self,
        template: &NameTemplate,
        ctx: &NamingContext,
        extension: &str,
        directory: Option<&Path>,
    ) -> Result<String> {
        self.file_name_with_suffix(
            template,
            ctx,
            extension,
            directory,
            "",
            ScaleFactor::IDENTITY,
        )
    }

    /// As [`NamePolicy::file_name`], adding `@2x` for Retina-scale captures.
    ///
    /// A small tolerance accounts for platform scale values represented just
    /// below two. An existing terminal `@2x` is retained rather than duplicated.
    ///
    /// # Errors
    ///
    /// As [`NamePolicy::file_name`].
    pub fn file_name_for_scale(
        &self,
        template: &NameTemplate,
        ctx: &NamingContext,
        extension: &str,
        directory: Option<&Path>,
        scale: ScaleFactor,
    ) -> Result<String> {
        self.file_name_with_suffix(template, ctx, extension, directory, "", scale)
    }

    fn file_name_with_suffix(
        &self,
        template: &NameTemplate,
        ctx: &NamingContext,
        extension: &str,
        directory: Option<&Path>,
        collision_suffix: &str,
        scale: ScaleFactor,
    ) -> Result<String> {
        let rendered = template.render(ctx, self);
        let sanitised = self.sanitise(&rendered);
        let retina_suffix = if scale.get() >= 1.95 { "@2x" } else { "" };
        let base = if retina_suffix.is_empty() {
            sanitised.as_str()
        } else {
            sanitised.strip_suffix(retina_suffix).unwrap_or(&sanitised)
        };
        let reserved = retina_suffix.len() + collision_suffix.len();
        let budget = self.stem_budget(directory, extension, reserved);
        if budget == 0 {
            return Err(Error::InvalidRequest(format!(
                "no filename fits in {} bytes under {}: choose a shorter save folder",
                self.max_path_bytes,
                directory.unwrap_or_else(|| Path::new("")).display(),
            )));
        }

        let mut stem = truncate_bytes(base, budget);
        // Truncation can expose a trailing dot or space that was legal mid-name,
        // so the trailing-character rule is re-applied afterwards.
        stem = stem.trim_end_matches(['.', ' ']).to_owned();
        if stem.is_empty() {
            stem = truncate_bytes(&self.fallback_stem, budget);
        }
        Ok(format!(
            "{stem}{retina_suffix}{collision_suffix}.{extension}"
        ))
    }

    /// Finds a path in `directory` that nothing occupies yet.
    ///
    /// `occupied` reports whether a path is taken. In production that is a
    /// filesystem probe; in tests it is a set, which is what makes collision
    /// behaviour testable without touching a disk.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] if the directory leaves no room for a
    /// filename, or [`Error::Storage`] if thousands of candidates are all taken
    /// — which means something is wrong that another attempt will not fix.
    pub fn unique_path(
        &self,
        directory: &Path,
        template: &NameTemplate,
        ctx: &NamingContext,
        extension: &str,
        occupied: &mut dyn FnMut(&Path) -> bool,
    ) -> Result<PathBuf> {
        self.unique_path_for_scale(
            directory,
            template,
            ctx,
            extension,
            ScaleFactor::IDENTITY,
            occupied,
        )
    }

    /// As [`NamePolicy::unique_path`], adding `@2x` for Retina-scale captures.
    ///
    /// Collision numbering follows the density marker: `Capture@2x 2.png`.
    ///
    /// # Errors
    ///
    /// As [`NamePolicy::unique_path`].
    pub fn unique_path_for_scale(
        &self,
        directory: &Path,
        template: &NameTemplate,
        ctx: &NamingContext,
        extension: &str,
        scale: ScaleFactor,
        occupied: &mut dyn FnMut(&Path) -> bool,
    ) -> Result<PathBuf> {
        const LIMIT: u32 = 10_000;
        for n in 1..=LIMIT {
            let suffix = if n == 1 {
                String::new()
            } else {
                format!(" {n}")
            };
            let name = self.file_name_with_suffix(
                template,
                ctx,
                extension,
                Some(directory),
                &suffix,
                scale,
            )?;
            let candidate = directory.join(name);
            if !occupied(&candidate) {
                return Ok(candidate);
            }
        }
        Err(Error::Storage(format!(
            "gave up after {LIMIT} name collisions in {}",
            directory.display()
        )))
    }
}

/// Truncates to at most `max` bytes, never splitting a character.
///
/// The realistic input is a CJK or emoji window title, where every character is
/// three or four bytes and a byte-wise cut produces invalid UTF-8.
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Truncates to at most `max` characters.
fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].trim_end().to_owned(),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_epoch_is_the_first_of_january_1970() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn leap_day_survives_the_round_trip() {
        // 2024-02-29T12:34:56Z
        let t = Timestamp::from_unix_seconds(1_709_210_096);
        assert_eq!((t.year, t.month, t.day), (2024, 2, 29));
        assert_eq!((t.hour, t.minute, t.second), (12, 34, 56));
    }

    #[test]
    fn truncating_never_splits_a_character() {
        let s = "日本語のタイトル";
        for max in 0..s.len() + 2 {
            let cut = truncate_bytes(s, max);
            assert!(cut.len() <= max.min(s.len()));
            assert!(s.starts_with(&cut));
        }
    }
}
