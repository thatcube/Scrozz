//! Encodings required by S3 and the self-contained viewer.

/// Lowercase hexadecimal.
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

/// RFC 4648 base64 with padding.
#[must_use]
pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        output.push(TABLE[(a >> 2) as usize] as char);
        output.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            TABLE[(c & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

/// AWS URI encoding: uppercase hex, spaces as `%20`, and optional slash
/// preservation for canonical S3 paths.
#[must_use]
pub fn aws_uri_encode(value: &str, encode_slash: bool) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut output = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(*byte, b'-' | b'.' | b'_' | b'~')
            || (!encode_slash && *byte == b'/')
        {
            output.push(*byte as char);
        } else {
            output.push('%');
            output.push(HEX[(byte >> 4) as usize] as char);
            output.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    output
}

/// Sorts and encodes a SigV4 query string.
#[must_use]
pub fn canonical_query(pairs: &[(String, String)]) -> String {
    let mut encoded: Vec<(String, String)> = pairs
        .iter()
        .map(|(key, value)| (aws_uri_encode(key, true), aws_uri_encode(value, true)))
        .collect();
    encoded.sort_unstable();
    encoded
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// Escapes text inserted into HTML element content or attributes.
#[must_use]
pub fn html_escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }
    output
}

/// Removes leading separators and leaves either no prefix or one trailing slash.
#[must_use]
pub fn normalise_prefix(prefix: &str) -> String {
    let trimmed = prefix.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// Joins a public origin or path prefix to an object key.
#[must_use]
pub fn public_url(base: &str, key: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        aws_uri_encode(key.trim_start_matches('/'), false)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s3_paths_preserve_only_path_separators() {
        assert_eq!(
            aws_uri_encode("folder/a b+雪.png", false),
            "folder/a%20b%2B%E9%9B%AA.png"
        );
        assert_eq!(aws_uri_encode("a/b", true), "a%2Fb");
    }

    #[test]
    fn canonical_queries_sort_after_encoding() {
        assert_eq!(
            canonical_query(&[
                ("z".into(), "last".into()),
                ("a b".into(), "/".into()),
                ("a".into(), "~".into()),
            ]),
            "a=~&a%20b=%2F&z=last"
        );
    }

    #[test]
    fn base64_vectors_match_rfc_4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn prefixes_do_not_create_an_invisible_root_component() {
        assert_eq!(normalise_prefix(""), "");
        assert_eq!(normalise_prefix("/captures//"), "captures/");
    }
}
