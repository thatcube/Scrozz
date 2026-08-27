//! A small, dependency-free JSON writer.
//!
//! # Why CLI output does not use `serde_json`
//!
//! Persistence uses `serde_json`, but per decision D11 CLI output is a
//! **scripting contract**. Its schema deserves to be a literal, greppable thing
//! rather than whatever a derive macro happened to emit from a struct that
//! someone later reorders.
//!
//! Two properties matter and both are enforced here:
//!
//! - **Key order is stable.** [`Json::Obj`] is an ordered `Vec` of pairs, not a
//!   map, so the bytes on stdout are deterministic. A diff of two runs is a diff
//!   of the data, not of a hash seed.
//! - **Output is compact and single-line.** One JSON document per invocation,
//!   newline-terminated, so `scrozz --json ... | jq` and line-oriented pipelines
//!   both work without buffering the whole stream.

use std::fmt::Write as _;

/// A JSON value.
///
/// Deliberately minimal: enough to express the output schema, and nothing more.
#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer. Emitted without a fractional part.
    Int(i64),
    /// A floating-point number.
    ///
    /// Non-finite values are emitted as `null`, because `NaN` and `Infinity` are
    /// not JSON and a parser downstream would simply reject the whole document.
    Float(f64),
    /// A string.
    Str(String),
    /// An array.
    Arr(Vec<Json>),
    /// An object with a fixed key order.
    Obj(Vec<(String, Json)>),
}

impl Json {
    /// A string value from anything string-like.
    pub fn str(value: impl Into<String>) -> Self {
        Self::Str(value.into())
    }

    /// An object from an ordered list of pairs.
    pub fn obj<K: Into<String>>(pairs: impl IntoIterator<Item = (K, Json)>) -> Self {
        Self::Obj(pairs.into_iter().map(|(k, v)| (k.into(), v)).collect())
    }

    /// An array from an iterator of values.
    pub fn arr(values: impl IntoIterator<Item = Json>) -> Self {
        Self::Arr(values.into_iter().collect())
    }

    /// `Null` for `None`, otherwise the mapped value.
    ///
    /// The schema keeps optional fields *present and null* rather than absent,
    /// so a consumer can index a key unconditionally.
    pub fn opt<T>(value: Option<T>, f: impl FnOnce(T) -> Json) -> Self {
        value.map_or(Self::Null, f)
    }

    /// Renders to compact JSON text.
    #[must_use]
    pub fn to_compact_string(&self) -> String {
        let mut out = String::new();
        self.write_compact(&mut out);
        out
    }

    fn write_compact(&self, out: &mut String) {
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(true) => out.push_str("true"),
            Self::Bool(false) => out.push_str("false"),
            Self::Int(n) => {
                let _ = write!(out, "{n}");
            }
            Self::Float(f) => {
                if f.is_finite() {
                    // `{:?}` rather than `{}` so `2.0` stays `2.0`: a scale
                    // factor that silently becomes `2` invites a consumer to
                    // parse it as an integer and then break on `1.5`.
                    let _ = write!(out, "{f:?}");
                } else {
                    out.push_str("null");
                }
            }
            Self::Str(s) => write_escaped(s, out),
            Self::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write_compact(out);
                }
                out.push(']');
            }
            Self::Obj(pairs) => {
                out.push('{');
                for (i, (key, value)) in pairs.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(key, out);
                    out.push(':');
                    value.write_compact(out);
                }
                out.push('}');
            }
        }
    }
}

/// Writes a JSON string literal, escaping everything RFC 8259 requires.
fn write_escaped(value: &str, out: &mut String) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            // Window titles and OCR results are arbitrary user text and do
            // reach this function; an unescaped control byte would produce a
            // document that every strict parser rejects.
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_render() {
        assert_eq!(Json::Null.to_compact_string(), "null");
        assert_eq!(Json::Bool(true).to_compact_string(), "true");
        assert_eq!(Json::Bool(false).to_compact_string(), "false");
        assert_eq!(Json::Int(-17).to_compact_string(), "-17");
    }

    #[test]
    fn floats_keep_their_fractional_part() {
        assert_eq!(Json::Float(2.0).to_compact_string(), "2.0");
        assert_eq!(Json::Float(1.5).to_compact_string(), "1.5");
    }

    #[test]
    fn non_finite_floats_become_null_rather_than_invalid_json() {
        assert_eq!(Json::Float(f64::NAN).to_compact_string(), "null");
        assert_eq!(Json::Float(f64::INFINITY).to_compact_string(), "null");
        assert_eq!(Json::Float(f64::NEG_INFINITY).to_compact_string(), "null");
    }

    #[test]
    fn strings_escape_json_metacharacters() {
        assert_eq!(
            Json::str("a\"b\\c").to_compact_string(),
            r#""a\"b\\c""#.to_string()
        );
    }

    #[test]
    fn strings_escape_control_characters() {
        assert_eq!(Json::str("a\nb").to_compact_string(), r#""a\nb""#);
        assert_eq!(Json::str("a\tb").to_compact_string(), r#""a\tb""#);
        assert_eq!(Json::str("a\rb").to_compact_string(), r#""a\rb""#);
        assert_eq!(Json::str("\u{08}").to_compact_string(), r#""\b""#);
        assert_eq!(Json::str("\u{0c}").to_compact_string(), r#""\f""#);
        assert_eq!(Json::str("\u{1}").to_compact_string(), r#""\u0001""#);
    }

    #[test]
    fn non_ascii_passes_through_as_utf8() {
        // Window titles are arbitrary text. Escaping them to \u sequences would
        // be legal but makes the output unreadable for no benefit.
        assert_eq!(
            Json::str("café — 日本語").to_compact_string(),
            "\"café — 日本語\""
        );
    }

    #[test]
    fn object_key_order_is_preserved_exactly() {
        let value = Json::obj([
            ("zebra", Json::Int(1)),
            ("apple", Json::Int(2)),
            ("mango", Json::Int(3)),
        ]);
        assert_eq!(
            value.to_compact_string(),
            r#"{"zebra":1,"apple":2,"mango":3}"#
        );
    }

    #[test]
    fn nesting_renders_compactly() {
        let value = Json::obj([
            ("list", Json::arr([Json::Int(1), Json::Null])),
            ("inner", Json::obj([("k", Json::str("v"))])),
        ]);
        assert_eq!(
            value.to_compact_string(),
            r#"{"list":[1,null],"inner":{"k":"v"}}"#
        );
    }

    #[test]
    fn empty_containers_render() {
        assert_eq!(Json::Arr(vec![]).to_compact_string(), "[]");
        assert_eq!(Json::Obj(vec![]).to_compact_string(), "{}");
    }

    #[test]
    fn opt_maps_none_to_null_and_some_to_the_value() {
        assert_eq!(
            Json::opt(None::<i64>, Json::Int).to_compact_string(),
            "null"
        );
        assert_eq!(Json::opt(Some(4), Json::Int).to_compact_string(), "4");
    }

    #[test]
    fn keys_are_escaped_too() {
        let value = Json::obj([("a\"b", Json::Null)]);
        assert_eq!(value.to_compact_string(), r#"{"a\"b":null}"#);
    }

    #[test]
    fn output_is_single_line() {
        let value = Json::obj([
            ("a", Json::arr([Json::obj([("b", Json::Int(1))])])),
            ("c", Json::str("plain")),
        ]);
        assert!(!value.to_compact_string().contains('\n'));
    }
}
