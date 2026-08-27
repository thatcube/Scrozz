//! Typed adaptation from persisted OCR settings to runtime behavior.

use language_tags::LanguageTag;
use scrozz_core::{Error, Result};

use crate::{LineBreaks, Link, Options, TextBlock};

/// Persisted key for comma-separated BCP-47 language tags.
pub const LANGUAGES_KEY: &str = "ocr.languages";
/// Persisted key selecting image-based automatic language detection.
pub const AUTO_DETECT_LANGUAGE_KEY: &str = "ocr.auto-detect-language";
/// Persisted key controlling whether visual lines remain separate.
pub const KEEP_LINE_BREAKS_KEY: &str = "ocr.keep-line-breaks";
/// Persisted key controlling URL, email, and telephone classification.
pub const DETECT_LINKS_KEY: &str = "ocr.detect-links";

/// How the runtime chooses a recognition language.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageMode {
    /// Resolve the user's configured operating-system language.
    System,
    /// Infer the language from image content.
    Automatic,
    /// Use the configured BCP-47 tags in priority order.
    Configured,
}

/// Runtime OCR behavior derived from the persisted `ocr.*` rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    options: Options,
    detect_links: bool,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            options: Options::new(),
            detect_links: true,
        }
    }
}

impl RuntimeConfig {
    /// Adapts persisted values into one coherent runtime configuration.
    ///
    /// `languages` is a comma-separated BCP-47 list. Empty selects system
    /// languages. Automatic detection is explicit and cannot be combined with
    /// configured tags.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRequest`] when automatic detection and configured
    /// tags are both enabled or when a configured tag is not valid BCP-47.
    pub fn from_settings(
        languages: &str,
        automatic_language_detection: bool,
        keep_line_breaks: bool,
        detect_links: bool,
    ) -> Result<Self> {
        let languages = languages
            .split(',')
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if let Some(tag) = languages
            .iter()
            .find(|tag| LanguageTag::parse(tag.as_str()).is_err())
        {
            return Err(Error::InvalidRequest(format!(
                "{LANGUAGES_KEY} contains malformed BCP-47 language tag {tag:?}"
            )));
        }
        if automatic_language_detection && !languages.is_empty() {
            return Err(Error::InvalidRequest(format!(
                "{AUTO_DETECT_LANGUAGE_KEY} cannot be true while {LANGUAGES_KEY} contains tags"
            )));
        }

        Ok(Self {
            options: Options::new()
                .with_languages(languages)
                .with_automatic_language_detection(automatic_language_detection)
                .with_line_breaks(if keep_line_breaks {
                    LineBreaks::Preserve
                } else {
                    LineBreaks::Collapse
                }),
            detect_links,
        })
    }

    /// Recognition-engine options.
    #[must_use]
    pub const fn options(&self) -> &Options {
        &self.options
    }

    /// Consumes this adapter and returns recognition-engine options.
    #[must_use]
    pub fn into_options(self) -> Options {
        self.options
    }

    /// The selected language mode.
    #[must_use]
    pub const fn language_mode(&self) -> LanguageMode {
        if self.options.automatic_language_detection {
            LanguageMode::Automatic
        } else if self.options.languages.is_empty() {
            LanguageMode::System
        } else {
            LanguageMode::Configured
        }
    }

    /// Whether recognized links should be classified.
    #[must_use]
    pub const fn detects_links(&self) -> bool {
        self.detect_links
    }

    /// Formats recognized blocks using the configured line-break policy.
    #[must_use]
    pub fn text(&self, blocks: &[TextBlock]) -> String {
        crate::text(blocks, self.options.line_breaks)
    }

    /// Classifies links when enabled.
    #[must_use]
    pub fn links(&self, blocks: &[TextBlock]) -> Vec<Link> {
        if self.detect_links {
            crate::links(blocks)
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use scrozz_core::{LogicalPoint, LogicalRect, LogicalSize};

    use super::*;

    fn block(text: &str, y: f64) -> TextBlock {
        TextBlock {
            text: text.to_owned(),
            bounds: LogicalRect::new(LogicalPoint::new(1.0, y), LogicalSize::new(30.0, 10.0)),
            confidence: 1.0,
        }
    }

    #[test]
    fn an_empty_language_list_means_system_not_automatic() {
        let config = RuntimeConfig::from_settings("", false, true, true).unwrap();
        assert_eq!(config.language_mode(), LanguageMode::System);
        assert!(config.options().languages.is_empty());
        assert!(!config.options().automatic_language_detection);
    }

    #[test]
    fn configured_languages_are_trimmed_and_keep_priority() {
        let config =
            RuntimeConfig::from_settings(" de-DE, en-US ,, fr-FR ", false, true, true).unwrap();
        assert_eq!(config.language_mode(), LanguageMode::Configured);
        assert_eq!(config.options().languages, ["de-DE", "en-US", "fr-FR"]);
    }

    #[test]
    fn malformed_language_tags_are_rejected_at_the_settings_boundary() {
        let error = RuntimeConfig::from_settings("de-DE, en--US", false, true, true).unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(message) if
            message.contains(LANGUAGES_KEY) && message.contains("en--US")));
    }

    #[test]
    fn automatic_detection_is_distinct_and_exclusive() {
        let config = RuntimeConfig::from_settings("", true, true, true).unwrap();
        assert_eq!(config.language_mode(), LanguageMode::Automatic);
        assert!(config.options().automatic_language_detection);

        let error = RuntimeConfig::from_settings("en-US", true, true, true).unwrap_err();
        assert!(matches!(error, Error::InvalidRequest(message) if
            message.contains(AUTO_DETECT_LANGUAGE_KEY) && message.contains(LANGUAGES_KEY)));
    }

    #[test]
    fn line_break_and_link_settings_change_output() {
        let blocks = [block("one", 2.0), block("https://example.org", 22.0)];
        let preserve = RuntimeConfig::from_settings("", false, true, true).unwrap();
        assert_eq!(preserve.text(&blocks), "one\nhttps://example.org");
        assert_eq!(preserve.links(&blocks).len(), 1);

        let collapse = RuntimeConfig::from_settings("", false, false, false).unwrap();
        assert_eq!(collapse.text(&blocks), "one https://example.org");
        assert!(collapse.links(&blocks).is_empty());
    }
}
