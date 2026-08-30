//! Secret-bearing values and safe diagnostics.

use std::fmt;

/// Bytes whose formatting is always redacted and whose allocation is scrubbed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Vec<u8>);

impl Secret {
    /// Wraps secret bytes.
    #[must_use]
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Self(bytes.into())
    }

    /// Wraps UTF-8 secret text.
    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self(text.into().into_bytes())
    }

    /// Exposes the bytes only to the operation that needs them.
    #[must_use]
    pub fn expose(&self) -> &[u8] {
        &self.0
    }

    /// Whether the secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Exposes UTF-8 text.
    pub(crate) fn expose_text(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

/// Whether a header carries credentials or another bearer value.
#[must_use]
pub fn sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "x-amz-security-token" | "cookie" | "set-cookie"
    )
}

/// Redacts literal secret values from a diagnostic supplied by a dependency.
#[must_use]
pub fn redact_literals(mut text: String, secrets: &[&Secret]) -> String {
    for secret in secrets {
        if let Some(value) = secret.expose_text()
            && !value.is_empty()
        {
            text = text.replace(value, "[REDACTED]");
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_format_their_value() {
        let secret = Secret::from_text("super-secret-value");
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
        assert_eq!(secret.to_string(), "[REDACTED]");
    }

    #[test]
    fn known_bearer_headers_are_sensitive_case_insensitively() {
        assert!(sensitive_header("Authorization"));
        assert!(sensitive_header("X-Amz-Security-Token"));
        assert!(!sensitive_header("Content-Type"));
    }
}
