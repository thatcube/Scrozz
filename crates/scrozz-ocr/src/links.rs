//! Link classification over recognised text and barcode payloads.

use scrozz_core::LogicalRect;

use crate::TextBlock;

/// A kind of actionable text recognised in an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LinkKind {
    /// An HTTP or HTTPS URL.
    Url,
    /// An email address.
    Email,
    /// A telephone number.
    Telephone,
}

impl LinkKind {
    /// Stable token used by machine-readable output.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Url => "url",
            Self::Email => "email",
            Self::Telephone => "tel",
        }
    }

    pub(crate) fn target(self, text: &str) -> String {
        match self {
            Self::Url if starts_ascii_case_insensitive(text, "www.") => {
                format!("https://{text}")
            }
            Self::Url => text.to_owned(),
            Self::Email if starts_ascii_case_insensitive(text, "mailto:") => text.to_owned(),
            Self::Email => format!("mailto:{text}"),
            Self::Telephone if starts_ascii_case_insensitive(text, "tel:") => text.to_owned(),
            Self::Telephone => {
                let dialable = text
                    .chars()
                    .filter(|character| {
                        character.is_ascii_digit()
                            || matches!(character, '+' | '*' | '#' | ',' | ';')
                    })
                    .collect::<String>();
                format!("tel:{dialable}")
            }
        }
    }
}

/// One actionable link found in recognised text.
#[derive(Debug, Clone, PartialEq)]
pub struct Link {
    /// Text as it appeared in the image or barcode.
    pub text: String,
    /// Safe target to hand to the operating system after explicit user action.
    pub target: String,
    /// The target family.
    pub kind: LinkKind,
    /// Bounds of the OCR block or barcode that contained the link.
    pub bounds: LogicalRect,
    /// Index of the originating OCR block, or `None` for a barcode payload.
    pub block: Option<usize>,
}

/// Finds URL, email and telephone targets in recognised text.
///
/// Bounds intentionally remain the containing OCR block's bounds. OCR engines do
/// not report per-character geometry consistently, so interpolating a smaller box
/// would imply precision the detector did not provide.
#[must_use]
pub fn links(blocks: &[TextBlock]) -> Vec<Link> {
    let mut found = Vec::new();
    for (block_index, block) in blocks.iter().enumerate() {
        for candidate in candidates(&block.text) {
            if let Some(kind) = classify(candidate) {
                found.push(Link {
                    text: candidate.to_owned(),
                    target: kind.target(candidate),
                    kind,
                    bounds: block.bounds,
                    block: Some(block_index),
                });
            }
        }
    }
    found
}

pub(crate) fn classify(text: &str) -> Option<LinkKind> {
    let text = trim_candidate(text);
    if is_url(text) {
        Some(LinkKind::Url)
    } else if is_email(text) {
        Some(LinkKind::Email)
    } else if is_telephone(text) {
        Some(LinkKind::Telephone)
    } else {
        None
    }
}

fn candidates(text: &str) -> Vec<&str> {
    let mut found: Vec<&str> = text
        .split_ascii_whitespace()
        .map(trim_url_or_email_candidate)
        .filter(|candidate| is_url(candidate) || is_email(candidate))
        .collect();
    let claimed = found.clone();
    found.extend(
        telephone_candidates(text)
            .into_iter()
            .filter(|candidate| !claimed.iter().any(|span| overlaps_slice(span, candidate))),
    );
    found.sort_by_key(|candidate| candidate.as_ptr() as usize);
    found.dedup();
    found
}

fn telephone_candidates(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut cursor = 0;

    while cursor < bytes.len() {
        let explicit = text
            .get(cursor..)
            .is_some_and(|remainder| starts_ascii_case_insensitive(remainder, "tel:"));
        let starts_number =
            bytes[cursor].is_ascii_digit() || matches!(bytes[cursor], b'+' | b'(' | b'*' | b'#');
        if !explicit && !starts_number {
            cursor += 1;
            continue;
        }

        let start = cursor;
        if explicit {
            cursor += 4;
        }
        while cursor < bytes.len()
            && (bytes[cursor].is_ascii_digit()
                || matches!(
                    bytes[cursor],
                    b'+' | b'-' | b'(' | b')' | b'.' | b' ' | b'\t' | b'*' | b'#' | b',' | b';'
                ))
        {
            cursor += 1;
        }
        let candidate = trim_candidate(text[start..cursor].trim());
        if is_telephone(candidate) {
            found.push(candidate);
        }
    }
    found
}

fn trim_candidate(text: &str) -> &str {
    text.trim_matches(|character: char| {
        matches!(
            character,
            '"' | '\'' | '<' | '>' | '[' | ']' | '{' | '}' | ',' | '.' | '!' | '?'
        )
    })
}

fn trim_url_or_email_candidate(mut text: &str) -> &str {
    text = trim_candidate(text);
    while let Some(inner) = text
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
    {
        text = trim_candidate(inner);
    }
    text
}

fn is_url(text: &str) -> bool {
    let remainder = ["https://", "http://"]
        .iter()
        .find_map(|prefix| strip_ascii_case_insensitive(text, prefix))
        .or_else(|| strip_ascii_case_insensitive(text, "www."));
    remainder.is_some_and(|remainder| {
        !remainder.is_empty()
            && !remainder.chars().any(char::is_whitespace)
            && remainder.contains('.')
    })
}

fn is_email(text: &str) -> bool {
    let text = strip_ascii_case_insensitive(text, "mailto:").unwrap_or(text);
    let Some((local, domain)) = text.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain.contains('.')
        && !domain.contains('@')
        && text
            .chars()
            .all(|character| !character.is_whitespace() && !character.is_control())
}

fn is_telephone(text: &str) -> bool {
    let explicit = strip_ascii_case_insensitive(text, "tel:");
    let number = explicit.unwrap_or(text);
    if explicit.is_none() && contains_date(number) {
        return false;
    }
    let digits = number
        .chars()
        .filter(|character| character.is_ascii_digit())
        .count();
    let valid_characters = number.chars().all(|character| {
        character.is_ascii_digit()
            || matches!(
                character,
                '+' | '-' | '(' | ')' | '.' | ' ' | '*' | '#' | ',' | ';'
            )
    });
    valid_characters
        && digits >= 7
        && (explicit.is_some()
            || number.starts_with('+')
            || number
                .chars()
                .any(|character| matches!(character, '-' | '(' | ')' | ' ')))
}

fn contains_date(text: &str) -> bool {
    text.split_ascii_whitespace().any(|part| {
        is_date(part.trim_matches(|character| matches!(character, '(' | ')' | ',' | ';')))
    })
}

fn is_date(text: &str) -> bool {
    ['-', '.'].into_iter().any(|separator| {
        let parts = text.split(separator).collect::<Vec<_>>();
        parts.len() == 3
            && parts.iter().all(|part| {
                !part.is_empty()
                    && part.len() <= 4
                    && part.chars().all(|character| character.is_ascii_digit())
            })
            && ((parts[0].len() == 4 && parts[1].len() <= 2 && parts[2].len() <= 2)
                || (parts[2].len() == 4 && parts[0].len() <= 2 && parts[1].len() <= 2))
    })
}

fn overlaps_slice(left: &str, right: &str) -> bool {
    let left_start = left.as_ptr() as usize;
    let left_end = left_start + left.len();
    let right_start = right.as_ptr() as usize;
    let right_end = right_start + right.len();
    left_start < right_end && right_start < left_end
}

fn starts_ascii_case_insensitive(text: &str, prefix: &str) -> bool {
    strip_ascii_case_insensitive(text, prefix).is_some()
}

fn strip_ascii_case_insensitive<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        .then(|| &text[prefix.len()..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrozz_core::{LogicalPoint, LogicalSize};

    fn block(text: &str) -> TextBlock {
        TextBlock {
            text: text.into(),
            bounds: LogicalRect::new(LogicalPoint::new(10.0, 20.0), LogicalSize::new(100.0, 30.0)),
            confidence: 0.9,
        }
    }

    #[test]
    fn classifies_supported_link_families() {
        assert_eq!(classify("https://example.org/a"), Some(LinkKind::Url));
        assert_eq!(classify("person@example.org"), Some(LinkKind::Email));
        assert_eq!(classify("+1 (212) 555-0199"), Some(LinkKind::Telephone));
        assert_eq!(classify("0123456789012"), None);
        assert_eq!(classify("2026-08-27"), None);
        assert_eq!(classify("27.08.2026"), None);
        assert_eq!(classify("2026-08-27 12"), None);
        assert_eq!(classify("2026-08-27)"), None);
    }

    #[test]
    fn dates_and_numbers_inside_urls_are_not_telephone_links() {
        let source = block(
            "Published 2026-08-27 12:34 at (https://example.org/archive/2026-08-27) \
             or (https://example.org/212-555-0199)",
        );
        let found = links(&[source]);

        assert_eq!(found.len(), 2, "{found:#?}");
        assert_eq!(found[0].kind, LinkKind::Url);
        assert_eq!(found[0].text, "https://example.org/archive/2026-08-27");
        assert_eq!(found[1].kind, LinkKind::Url);
        assert_eq!(found[1].text, "https://example.org/212-555-0199");
    }

    #[test]
    fn target_schemes_are_added_without_double_prefixing() {
        assert_eq!(
            LinkKind::Url.target("www.example.org"),
            "https://www.example.org"
        );
        assert_eq!(
            LinkKind::Email.target("person@example.org"),
            "mailto:person@example.org"
        );
        assert_eq!(
            LinkKind::Telephone.target("+1 (212) 555-0199"),
            "tel:+12125550199"
        );
    }

    #[test]
    fn detected_links_keep_their_source_bounds() {
        let source = block("See https://example.org.");
        let found = links(std::slice::from_ref(&source));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text, "https://example.org");
        assert_eq!(found[0].bounds, source.bounds);
        assert_eq!(found[0].block, Some(0));
    }

    #[test]
    fn detects_a_spaced_telephone_number_inside_prose() {
        let source = block("Call +1 (212) 555-0199 today or email person@example.org");
        let found = links(&[source]);

        assert_eq!(found.len(), 2);
        assert_eq!(found[0].text, "+1 (212) 555-0199");
        assert_eq!(found[0].target, "tel:+12125550199");
        assert_eq!(found[1].kind, LinkKind::Email);
    }
}
