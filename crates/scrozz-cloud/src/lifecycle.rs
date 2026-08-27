//! Provider-enforced link expiry and object-deletion lifecycle configuration.

use std::{collections::BTreeSet, str::FromStr, time::Duration};

use crate::{
    encoding::{canonical_query, html_escape},
    error::{Error, Result},
};

/// The reserved object tag used by generated lifecycle rules.
pub const EXPIRY_TAG: &str = "scrozz-expiry-days";
/// Reserved object-key directory for providers without object-tag support.
pub const EXPIRY_PREFIX: &str = "scrozz-expiry";

/// A presigned GET lifetime. SigV4 permits at most seven days.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Expiry {
    seconds: u32,
}

impl Expiry {
    /// The SigV4 maximum.
    pub const MAX_SECONDS: u32 = 7 * 24 * 60 * 60;

    /// Builds a nonzero expiry no longer than seven days.
    pub fn from_seconds(seconds: u32) -> Result<Self> {
        if seconds == 0 || seconds > Self::MAX_SECONDS {
            return Err(Error::Config(format!(
                "share expiry must be between 1 second and 7 days, got {seconds} seconds"
            )));
        }
        Ok(Self { seconds })
    }

    /// One day.
    #[must_use]
    pub const fn one_day() -> Self {
        Self { seconds: 86_400 }
    }

    /// Duration used by retry-independent callers.
    #[must_use]
    pub const fn duration(self) -> Duration {
        Duration::from_secs(self.seconds as u64)
    }

    /// Whole seconds for `X-Amz-Expires`.
    #[must_use]
    pub const fn seconds(self) -> u32 {
        self.seconds
    }

    /// The provider lifecycle granularity, rounded up honestly.
    #[must_use]
    pub const fn lifecycle_days(self) -> u32 {
        self.seconds.div_ceil(86_400)
    }

    /// The tag a PUT must carry for the generated lifecycle rule to match.
    pub fn lifecycle_tag(self) -> ObjectTag {
        ObjectTag {
            key: EXPIRY_TAG.to_owned(),
            value: self.lifecycle_days().to_string(),
        }
    }
}

impl FromStr for Expiry {
    type Err = Error;

    fn from_str(raw: &str) -> Result<Self> {
        let text = raw.trim().to_ascii_lowercase();
        let (number, multiplier) = match text.chars().last() {
            Some('s') => (&text[..text.len() - 1], 1u32),
            Some('m') => (&text[..text.len() - 1], 60),
            Some('h') => (&text[..text.len() - 1], 60 * 60),
            Some('d') => (&text[..text.len() - 1], 24 * 60 * 60),
            _ => (text.as_str(), 1),
        };
        let amount: u32 = number.parse().map_err(|_| {
            Error::Config(format!(
                "invalid expiry {raw:?}; use seconds or a suffix such as 30m, 24h or 7d"
            ))
        })?;
        let seconds = amount
            .checked_mul(multiplier)
            .ok_or_else(|| Error::Config(format!("expiry {raw:?} is too large")))?;
        Self::from_seconds(seconds)
    }
}

/// One S3 object tag.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectTag {
    key: String,
    value: String,
}

impl ObjectTag {
    /// Validates an S3 tag.
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Result<Self> {
        let key = key.into();
        let value = value.into();
        if key.is_empty() || key.encode_utf16().count() > 128 {
            return Err(Error::Config(
                "an object-tag key must contain 1 to 128 UTF-16 code units".to_owned(),
            ));
        }
        if value.encode_utf16().count() > 256 {
            return Err(Error::Config(
                "an object-tag value may contain at most 256 UTF-16 code units".to_owned(),
            ));
        }
        if key.to_ascii_lowercase().starts_with("aws:") {
            return Err(Error::Config(
                "object-tag keys beginning with `aws:` are reserved".to_owned(),
            ));
        }
        if !key.chars().all(is_s3_tag_character) || !value.chars().all(is_s3_tag_character) {
            return Err(Error::Config(
                "object tags may contain Unicode letters, numbers and spaces, plus \
                 `_ . : / = + - @`"
                    .to_owned(),
            ));
        }
        Ok(Self { key, value })
    }

    /// Validated tag key.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Validated tag value.
    #[must_use]
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Parses `KEY=VALUE`.
    pub fn parse(raw: &str) -> Result<Self> {
        let (key, value) = raw
            .split_once('=')
            .ok_or_else(|| Error::Config(format!("tag {raw:?} must be written as KEY=VALUE")))?;
        Self::new(key, value)
    }
}

fn is_s3_tag_character(character: char) -> bool {
    character.is_alphanumeric()
        || (character.is_whitespace() && !character.is_control())
        || matches!(character, '_' | '.' | ':' | '/' | '=' | '+' | '-' | '@')
}

/// Encodes the `x-amz-tagging` header, rejecting duplicates and S3's 10-tag cap.
pub fn tag_header(tags: &[ObjectTag]) -> Result<String> {
    if tags.len() > 10 {
        return Err(Error::Config(format!(
            "S3 permits at most 10 object tags; {} were supplied",
            tags.len()
        )));
    }
    let mut keys = BTreeSet::new();
    for tag in tags {
        if !keys.insert(&tag.key) {
            return Err(Error::Config(format!(
                "object tag {:?} was supplied more than once",
                tag.key
            )));
        }
    }
    let pairs = tags
        .iter()
        .map(|tag| (tag.key.clone(), tag.value.clone()))
        .collect::<Vec<_>>();
    Ok(canonical_query(&pairs))
}

/// Generates the bucket lifecycle XML that matches an expiry tag.
///
/// Scrozz does not apply this itself: doing so would require broad
/// `PutBucketLifecycleConfiguration` permission for a one-time bucket setup.
#[must_use]
pub fn lifecycle_rule_xml(expiry: Expiry) -> String {
    let days = expiry.lifecycle_days();
    rule_xml(
        days,
        &format!(
            "<Filter><Tag><Key>{}</Key><Value>{days}</Value></Tag></Filter>",
            html_escape(EXPIRY_TAG)
        ),
        true,
    )
}

/// Generates a lifecycle rule for providers that support prefix filters but not
/// object tags. Callers must place the object under [`expiry_prefix`].
#[must_use]
pub fn lifecycle_prefix_rule_xml(expiry: Expiry) -> String {
    let days = expiry.lifecycle_days();
    let prefix = expiry_prefix(expiry);
    rule_xml(
        days,
        &format!("<Filter><Prefix>{}</Prefix></Filter>", html_escape(&prefix)),
        false,
    )
}

/// Prefix lifecycle rules for a versioned provider such as Backblaze B2.
#[must_use]
pub fn lifecycle_versioned_prefix_rule_xml(expiry: Expiry) -> String {
    let days = expiry.lifecycle_days();
    let prefix = expiry_prefix(expiry);
    let filter = format!("<Filter><Prefix>{}</Prefix></Filter>", html_escape(&prefix));
    [
        b2_rule_xml(
            &format!("Scrozz {days}-day shares - hide current"),
            &filter,
            &format!("<Expiration><Days>{days}</Days></Expiration>"),
        ),
        b2_rule_xml(
            &format!("Scrozz {days}-day shares - remove delete markers"),
            &filter,
            "<Expiration><ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker></Expiration>",
        ),
        b2_rule_xml(
            &format!("Scrozz {days}-day shares - delete noncurrent"),
            &filter,
            "<NoncurrentVersionExpiration><NoncurrentDays>1</NoncurrentDays></NoncurrentVersionExpiration>",
        ),
    ]
    .join("\n")
}

fn b2_rule_xml(id: &str, filter: &str, action: &str) -> String {
    format!(
        "<Rule>\n\
         \x20 <ID>{}</ID>\n\
         \x20 {filter}\n\
         \x20 <Status>Enabled</Status>\n\
         \x20 {action}\n\
         </Rule>",
        html_escape(id)
    )
}

fn rule_xml(days: u32, filter: &str, delete_noncurrent: bool) -> String {
    let noncurrent = if delete_noncurrent {
        "\n  <NoncurrentVersionExpiration><NoncurrentDays>1</NoncurrentDays></NoncurrentVersionExpiration>"
    } else {
        ""
    };
    format!(
        "<Rule>\n\
         \x20 <ID>Scrozz {days}-day shares</ID>\n\
         \x20 {filter}\n\
         \x20 <Status>Enabled</Status>\n\
         \x20 <Expiration><Days>{days}</Days></Expiration>{noncurrent}\n\
         </Rule>"
    )
}

/// Object-key prefix corresponding to [`lifecycle_prefix_rule_xml`].
#[must_use]
pub fn expiry_prefix(expiry: Expiry) -> String {
    format!("{EXPIRY_PREFIX}-{}d/", expiry.lifecycle_days())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expiry_parser_enforces_the_sigv4_limit() {
        assert_eq!("30m".parse::<Expiry>().unwrap().seconds(), 1800);
        assert_eq!("7d".parse::<Expiry>().unwrap().seconds(), 604_800);
        assert!("8d".parse::<Expiry>().is_err());
        assert!("0".parse::<Expiry>().is_err());
    }

    #[test]
    fn lifecycle_rounds_up_without_claiming_subday_deletion() {
        let expiry = "90m".parse::<Expiry>().unwrap();
        assert_eq!(expiry.lifecycle_days(), 1);
        assert_eq!(expiry.lifecycle_tag().value(), "1");
        let xml = lifecycle_rule_xml(expiry);
        assert!(xml.contains("<Days>1</Days>"));
        assert!(xml.contains(EXPIRY_TAG));
        assert!(xml.contains("<NoncurrentDays>1</NoncurrentDays>"));
        assert!(!xml.contains("<LifecycleConfiguration"));

        let prefix_xml = lifecycle_prefix_rule_xml(expiry);
        assert!(prefix_xml.contains("<Prefix>scrozz-expiry-1d/</Prefix>"));
        assert!(!prefix_xml.contains("<NoncurrentVersionExpiration>"));
        assert!(
            lifecycle_versioned_prefix_rule_xml(expiry).contains("<NoncurrentVersionExpiration>")
        );
        let b2_xml = lifecycle_versioned_prefix_rule_xml(expiry);
        assert_eq!(b2_xml.matches("<Rule>").count(), 3);
        assert_eq!(
            b2_xml.matches("<Prefix>scrozz-expiry-1d/</Prefix>").count(),
            3
        );
        assert!(b2_xml.contains("<ExpiredObjectDeleteMarker>true</ExpiredObjectDeleteMarker>"));
    }

    #[test]
    fn duplicate_and_excess_tags_are_rejected() {
        let duplicate = vec![
            ObjectTag::new("team", "one").unwrap(),
            ObjectTag::new("team", "two").unwrap(),
        ];
        assert!(tag_header(&duplicate).is_err());
        let too_many = (0..11)
            .map(|index| ObjectTag::new(format!("k{index}"), "v").unwrap())
            .collect::<Vec<_>>();
        assert!(tag_header(&too_many).is_err());

        for (key, value) in [
            ("aws:system", "value"),
            ("team&project", "value"),
            ("team", "value?"),
            ("team", "value\n"),
        ] {
            assert!(ObjectTag::new(key, value).is_err(), "{key:?}={value:?}");
        }
        assert!(ObjectTag::new("t\u{e9}am", "\u{56e2}\u{961f} 1").is_ok());
        assert!(ObjectTag::new("\u{1f600}", "value").is_err());
        assert!(ObjectTag::new("x".repeat(128), "").is_ok());
        assert!(ObjectTag::new("x".repeat(129), "").is_err());
    }
}
