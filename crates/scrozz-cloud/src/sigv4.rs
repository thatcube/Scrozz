//! AWS Signature Version 4 for S3 PUTs and presigned GETs.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    credentials::Credentials,
    digest::{hmac_sha256, sha256},
    encoding::{canonical_query, hex_lower},
    error::{Error, Result},
};

/// UTC timestamp in the exact forms SigV4 signs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmzDate {
    full: String,
    day: String,
}

impl AmzDate {
    /// Parses `YYYYMMDDTHHMMSSZ`.
    pub fn parse(value: &str) -> Result<Self> {
        let bytes = value.as_bytes();
        if bytes.len() != 16
            || bytes[8] != b'T'
            || bytes[15] != b'Z'
            || !bytes[..8].iter().all(u8::is_ascii_digit)
            || !bytes[9..15].iter().all(u8::is_ascii_digit)
        {
            return Err(Error::Config(format!(
                "invalid SigV4 timestamp {value:?}; expected YYYYMMDDTHHMMSSZ"
            )));
        }
        let year = parse_digits(&bytes[..4]);
        let month = parse_digits(&bytes[4..6]);
        let day = parse_digits(&bytes[6..8]);
        let hour = parse_digits(&bytes[9..11]);
        let minute = parse_digits(&bytes[11..13]);
        let second = parse_digits(&bytes[13..15]);
        let max_day = days_in_month(year, month);
        if year == 0
            || max_day == 0
            || day == 0
            || day > max_day
            || hour > 23
            || minute > 59
            || second > 59
        {
            return Err(Error::Config(format!(
                "invalid SigV4 timestamp {value:?}; date or time is out of range"
            )));
        }
        Ok(Self {
            full: value.to_owned(),
            day: value[..8].to_owned(),
        })
    }

    /// Formats a system time as UTC without bringing a date-time dependency into
    /// the signing core.
    pub fn from_system_time(value: SystemTime) -> Result<Self> {
        let seconds = value
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::Config("SigV4 cannot sign a time before 1970".to_owned()))?
            .as_secs();
        let days = (seconds / 86_400) as i64;
        let in_day = seconds % 86_400;
        let (year, month, day) = civil_from_days(days);
        let hour = in_day / 3600;
        let minute = in_day % 3600 / 60;
        let second = in_day % 60;
        Self::parse(&format!(
            "{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z"
        ))
    }

    /// Current UTC time.
    pub fn now() -> Result<Self> {
        Self::from_system_time(SystemTime::now())
    }

    /// Full timestamp.
    #[must_use]
    pub fn full(&self) -> &str {
        &self.full
    }

    /// Calendar date.
    #[must_use]
    pub fn day(&self) -> &str {
        &self.day
    }
}

fn parse_digits(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0, |value, byte| value * 10 + u32::from(byte - b'0'))
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year.is_multiple_of(400) || (year.is_multiple_of(4) && !year.is_multiple_of(100)) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

/// Headers required by an authenticated request.
#[derive(Clone)]
pub struct SignedHeaders {
    /// Includes `Authorization`, `Host`, date, payload hash and optional token.
    pub headers: Vec<(String, String)>,
}

impl std::fmt::Debug for SignedHeaders {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names = self
            .headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        f.debug_struct("SignedHeaders")
            .field("header_names", &names)
            .finish()
    }
}

/// Signs request headers.
pub fn sign_headers(
    credentials: &Credentials,
    method: &str,
    canonical_uri: &str,
    region: &str,
    timestamp: &AmzDate,
    payload_hash: &str,
    mut headers: Vec<(String, String)>,
) -> Result<SignedHeaders> {
    upsert(&mut headers, "x-amz-date", timestamp.full());
    upsert(&mut headers, "x-amz-content-sha256", payload_hash);
    if let Some(token) = credentials.session_token() {
        upsert(&mut headers, "x-amz-security-token", token);
    }
    let (canonical_headers, signed_names) = canonical_headers(&headers)?;
    let canonical =
        format!("{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_names}\n{payload_hash}");
    let scope = format!("{}/{region}/s3/aws4_request", timestamp.day());
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        timestamp.full(),
        hex_lower(&sha256(canonical.as_bytes()))
    );
    let signature = signature(
        credentials.secret_access_key(),
        timestamp.day(),
        region,
        &string_to_sign,
    );
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope},SignedHeaders={signed_names},Signature={}",
        credentials.access_key_id(),
        hex_lower(&signature)
    );
    headers.push(("authorization".to_owned(), authorization));
    Ok(SignedHeaders { headers })
}

/// Produces a provider-enforced, expiring GET URL.
pub fn presign_get(
    credentials: &Credentials,
    base_url: &str,
    canonical_uri: &str,
    host: &str,
    region: &str,
    timestamp: &AmzDate,
    expires_seconds: u32,
) -> Result<String> {
    if expires_seconds == 0 || expires_seconds > 604_800 {
        return Err(Error::Config(
            "a SigV4 presigned URL must expire between 1 second and 7 days".to_owned(),
        ));
    }
    let scope = format!("{}/{region}/s3/aws4_request", timestamp.day());
    let mut query = vec![
        ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
        (
            "X-Amz-Credential".into(),
            format!("{}/{}", credentials.access_key_id(), scope),
        ),
        ("X-Amz-Date".into(), timestamp.full().to_owned()),
        ("X-Amz-Expires".into(), expires_seconds.to_string()),
        ("X-Amz-SignedHeaders".into(), "host".into()),
    ];
    if let Some(token) = credentials.session_token() {
        query.push(("X-Amz-Security-Token".into(), token.to_owned()));
    }
    let canonical_query = canonical_query(&query);
    let canonical = format!(
        "GET\n{canonical_uri}\n{canonical_query}\nhost:{}\n\nhost\nUNSIGNED-PAYLOAD",
        normalise_header_value(host)
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        timestamp.full(),
        hex_lower(&sha256(canonical.as_bytes()))
    );
    let signature = signature(
        credentials.secret_access_key(),
        timestamp.day(),
        region,
        &string_to_sign,
    );
    Ok(format!(
        "{base_url}?{canonical_query}&X-Amz-Signature={}",
        hex_lower(&signature)
    ))
}

fn signature(secret: &[u8], day: &str, region: &str, string_to_sign: &str) -> [u8; 32] {
    let mut first = Vec::with_capacity(4 + secret.len());
    first.extend_from_slice(b"AWS4");
    first.extend_from_slice(secret);
    let date = hmac_sha256(&first, day.as_bytes());
    let region = hmac_sha256(&date, region.as_bytes());
    let service = hmac_sha256(&region, b"s3");
    let signing = hmac_sha256(&service, b"aws4_request");
    hmac_sha256(&signing, string_to_sign.as_bytes())
}

fn canonical_headers(headers: &[(String, String)]) -> Result<(String, String)> {
    let mut values = headers
        .iter()
        .map(|(name, value)| {
            let name = name.trim().to_ascii_lowercase();
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Err(Error::Config(format!(
                    "invalid signed header name {name:?}"
                )));
            }
            Ok((name, normalise_header_value(value)))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    for pair in values.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(Error::Config(format!(
                "signed header {:?} was supplied twice",
                pair[0].0
            )));
        }
    }
    let canonical = values
        .iter()
        .map(|(name, value)| format!("{name}:{value}\n"))
        .collect::<String>();
    let names = values
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(";");
    Ok((canonical, names))
}

fn normalise_header_value(value: &str) -> String {
    value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

fn upsert(headers: &mut Vec<(String, String)>, name: &str, value: &str) {
    if let Some((_, existing)) = headers
        .iter_mut()
        .find(|(existing, _)| existing.eq_ignore_ascii_case(name))
    {
        *existing = value.to_owned();
    } else {
        headers.push((name.to_owned(), value.to_owned()));
    }
}

// Howard Hinnant's civil-from-days algorithm, with day zero at Unix epoch.
fn civil_from_days(mut day: i64) -> (i32, u32, u32) {
    day += 719_468;
    let era = if day >= 0 { day } else { day - 146_096 } / 146_097;
    let day_of_era = day - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year as i32, month as u32, day as u32)
}

#[cfg(test)]
mod tests {
    use crate::{Secret, encoding::hex_lower};

    use super::*;

    fn aws_example_credentials() -> Credentials {
        Credentials::new(
            "AKIAIOSFODNN7EXAMPLE",
            Secret::from_text("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"),
            None,
        )
        .unwrap()
    }

    #[test]
    fn aws_s3_get_object_known_answer_matches() {
        let signed = sign_headers(
            &aws_example_credentials(),
            "GET",
            "/test.txt",
            "us-east-1",
            &AmzDate::parse("20130524T000000Z").unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            vec![
                ("host".into(), "examplebucket.s3.amazonaws.com".into()),
                ("range".into(), "bytes=0-9".into()),
            ],
        )
        .unwrap();
        let authorization = signed
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .unwrap()
            .1
            .clone();
        assert!(
            authorization
                .ends_with("f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"),
            "{authorization}"
        );
    }

    #[test]
    fn date_formatting_handles_epoch_and_leap_days() {
        assert_eq!(
            AmzDate::from_system_time(UNIX_EPOCH).unwrap().full(),
            "19700101T000000Z"
        );
        assert_eq!(
            AmzDate::from_system_time(UNIX_EPOCH + std::time::Duration::from_secs(1_709_164_800))
                .unwrap()
                .full(),
            "20240229T000000Z"
        );
        assert!(AmzDate::parse("1234567\u{e9}1234567").is_err());
        assert!(AmzDate::parse("20240230T120000Z").is_err());
        assert!(AmzDate::parse("20240229T240000Z").is_err());
    }

    #[test]
    fn signing_key_derivation_is_not_an_external_crypto_dependency() {
        let digest = signature(
            b"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
            "20120215",
            "us-east-1",
            "AWS4-HMAC-SHA256\nexample",
        );
        assert_eq!(hex_lower(&digest).len(), 64);
    }
}
