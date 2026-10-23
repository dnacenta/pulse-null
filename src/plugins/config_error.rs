//! Plugin config failures, safe to log.
//!
//! A toml/serde type error repeats the offending value (`invalid type:
//! string "…", expected u16`), and a spanned parse error reprints the
//! source line. Either one can put a credential into the startup log.
//! Callers log [`describe`] instead: the key path and the expected type.

/// The key path and the expected type of a plugin config parse failure.
/// The configured value is never included.
pub(crate) fn describe(err: &toml::de::Error) -> String {
    let key = key_path(&err.to_string());
    let detail = safe_detail(err.message().trim());
    match (key, detail) {
        (Some(key), Some(detail)) => format!("{key}: {detail}"),
        (Some(key), None) => key,
        (None, Some(detail)) => detail,
        (None, None) => "invalid value".to_string(),
    }
}

/// `in \`server.port\`` is appended only when the error has no source span.
/// A spanned error reprints the document line, so its text is not a safe
/// place to look for the key.
fn key_path(rendered: &str) -> Option<String> {
    if rendered.contains("TOML parse error at line") {
        return None;
    }
    let line = rendered
        .lines()
        .rfind(|line| line.starts_with("in `") && line.ends_with('`'))?;
    let path = &line["in `".len()..line.len() - 1];
    if path.is_empty() || path.contains('`') {
        None
    } else {
        Some(path.to_string())
    }
}

fn safe_detail(message: &str) -> Option<String> {
    if let Some(expected) = expected_type(message) {
        return Some(format!("expected {expected}"));
    }
    if let Some(expected) = unknown_variant_expected(message) {
        return Some(format!("unknown variant, expected {expected}"));
    }
    if is_field_location(message) {
        return Some(message.to_string());
    }
    None
}

/// `invalid type: <unexpected>, expected <type>`, with `<unexpected>`
/// consumed so a value cannot survive by containing the separator.
fn expected_type(message: &str) -> Option<&str> {
    let rest = message
        .strip_prefix("invalid type: ")
        .or_else(|| message.strip_prefix("invalid value: "))?;
    let rest = skip_unexpected(rest)?;
    let expected = rest.strip_prefix(", expected ")?;
    if expected.is_empty() {
        None
    } else {
        Some(expected)
    }
}

fn skip_unexpected(input: &str) -> Option<&str> {
    const BARE: &[&str] = &[
        "unit variant",
        "unit value",
        "newtype variant",
        "newtype struct",
        "struct variant",
        "tuple variant",
        "Option value",
        "sequence",
        "map",
        "enum",
    ];
    for bare in BARE {
        if let Some(rest) = input.strip_prefix(bare) {
            if rest.is_empty() || rest.starts_with(',') {
                return Some(rest);
            }
        }
    }
    if let Some(rest) = input.strip_prefix("string ") {
        return skip_debug_string(rest);
    }
    if let Some(rest) = input.strip_prefix("byte array ") {
        return skip_byte_array(rest);
    }
    if let Some(rest) = input.strip_prefix("character ") {
        return skip_character(rest);
    }
    for prefix in ["boolean ", "integer ", "floating point "] {
        if let Some(rest) = input.strip_prefix(prefix) {
            return skip_scalar(rest);
        }
    }
    None
}

/// A `Debug` string: `"..."`, with escapes, so the value's own text can
/// contain `, expected ` without ending the token early.
fn skip_debug_string(input: &str) -> Option<&str> {
    let mut chars = input.chars();
    if chars.next() != Some('"') {
        return None;
    }
    loop {
        match chars.next()? {
            // Debug writes `\"` and `\\`; skipping the escaped character
            // keeps a quote inside the value from ending the token.
            '\\' => {
                chars.next()?;
            }
            '"' => return Some(chars.as_str()),
            _ => {}
        }
    }
}

fn skip_byte_array(input: &str) -> Option<&str> {
    let rest = input.strip_prefix('[')?;
    let end = rest.find(']')?;
    let inside = &rest[..end];
    if inside
        .chars()
        .all(|c| c.is_ascii_digit() || c == ',' || c == ' ')
    {
        Some(&rest[end + 1..])
    } else {
        None
    }
}

/// `character \`${c}\``. A backtick character is three backticks in a row.
fn skip_character(input: &str) -> Option<&str> {
    let rest = input.strip_prefix('`')?;
    let mut chars = rest.chars();
    chars.next()?;
    let after = chars.as_str();
    after.strip_prefix('`')
}

fn skip_scalar(input: &str) -> Option<&str> {
    let rest = input.strip_prefix('`')?;
    let end = rest.find('`')?;
    let inside = &rest[..end];
    if inside.is_empty()
        || !inside
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_'))
    {
        return None;
    }
    Some(&rest[end + 1..])
}

/// `unknown variant \`${value}\`, expected ${variants}`. The value is not
/// escaped, so a value that itself contains the separator is rejected and
/// the caller reports the key alone.
fn unknown_variant_expected(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("unknown variant `")?;
    let marker = "`, expected ";
    let index = rest.find(marker)?;
    let value = &rest[..index];
    if value.contains('`') {
        return None;
    }
    let expected = &rest[index + marker.len()..];
    if variant_list_is_safe(expected) {
        Some(expected)
    } else {
        None
    }
}

fn variant_list_is_safe(expected: &str) -> bool {
    if expected == "there are no variants" {
        return true;
    }
    let mut rest = expected;
    if !rest.starts_with('`') {
        return false;
    }
    loop {
        let Some(inner) = rest.strip_prefix('`') else {
            return false;
        };
        let Some(end) = inner.find('`') else {
            return false;
        };
        if !is_ident(&inner[..end]) {
            return false;
        }
        rest = &inner[end + 1..];
        if rest.is_empty() {
            return true;
        }
        if let Some(next) = rest.strip_prefix(", or ") {
            rest = next;
        } else if let Some(next) = rest.strip_prefix(", ") {
            rest = next;
        } else if let Some(next) = rest.strip_prefix(" or ") {
            rest = next;
        } else {
            return false;
        }
    }
}

fn is_ident(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// Field names are locations, not configured values. A message that quotes
/// a value with `"` is not one of these.
fn is_field_location(message: &str) -> bool {
    if message.contains('"') {
        return false;
    }
    message.starts_with("missing field `")
        || message.starts_with("duplicate field `")
        || message.starts_with("unknown field `")
}

#[cfg(test)]
#[allow(dead_code)] // fixture fields exist so deserialization has a type to reject
mod tests {
    use super::*;
    use serde::Deserialize;

    const SECRET: &str = "sk-live-credential-do-not-log-9f3a7c";

    #[derive(Debug, Deserialize)]
    struct Server {
        port: u16,
    }

    #[derive(Debug, Deserialize)]
    struct Nested {
        server: Server,
    }

    #[derive(Debug, Deserialize)]
    struct Token {
        token: String,
    }

    #[derive(Debug, Deserialize)]
    enum Mode {
        On,
        Off,
    }

    #[derive(Debug, Deserialize)]
    struct Flag {
        mode: Mode,
    }

    fn value_error<T: serde::de::DeserializeOwned + std::fmt::Debug>(
        value: toml::Value,
    ) -> toml::de::Error {
        value.try_into::<T>().unwrap_err()
    }

    #[test]
    fn names_the_key_and_the_expected_type_without_the_value() {
        let mut server = toml::map::Map::new();
        server.insert("port".into(), toml::Value::String(SECRET.into()));
        let mut root = toml::map::Map::new();
        root.insert("server".into(), toml::Value::Table(server));
        let err = value_error::<Nested>(toml::Value::Table(root));
        assert!(
            err.to_string().contains(SECRET),
            "precondition: the raw error echoes the value\n{err}"
        );
        assert_eq!(describe(&err), "server.port: expected u16");
    }

    #[test]
    fn a_value_containing_the_separator_is_still_omitted() {
        let secret = "sk-live-credential, expected u16, do-not-log-9f3a7c";
        let mut server = toml::map::Map::new();
        server.insert("port".into(), toml::Value::String(secret.into()));
        let mut root = toml::map::Map::new();
        root.insert("server".into(), toml::Value::Table(server));
        let err = value_error::<Nested>(toml::Value::Table(root));
        let described = describe(&err);
        assert!(!described.contains("sk-live"), "{described}");
        assert!(!described.contains("do-not-log"), "{described}");
        assert_eq!(described, "server.port: expected u16");
    }

    #[test]
    fn an_integer_value_is_omitted() {
        let secret = "8675309012345";
        let mut root = toml::map::Map::new();
        root.insert(
            "token".into(),
            toml::Value::Integer(secret.parse().unwrap()),
        );
        let err = value_error::<Token>(toml::Value::Table(root));
        assert!(err.to_string().contains(secret), "{err}");
        assert_eq!(describe(&err), "token: expected a string");
    }

    #[test]
    fn an_unknown_variant_value_is_omitted() {
        let mut root = toml::map::Map::new();
        root.insert("mode".into(), toml::Value::String(SECRET.into()));
        let err = value_error::<Flag>(toml::Value::Table(root));
        let described = describe(&err);
        assert!(!described.contains(SECRET), "{described}");
        assert!(described.contains("mode"), "{described}");
        assert!(described.contains("On"), "{described}");
        assert!(described.contains("Off"), "{described}");
    }

    #[test]
    fn a_variant_value_that_cannot_be_split_safely_drops_the_message() {
        let secret = "sk-live`, expected `On";
        let mut root = toml::map::Map::new();
        root.insert("mode".into(), toml::Value::String(secret.into()));
        let err = value_error::<Flag>(toml::Value::Table(root));
        let described = describe(&err);
        assert!(!described.contains("sk-live"), "{described}");
        assert_eq!(described, "mode");
    }

    #[test]
    fn a_missing_field_keeps_the_field_name() {
        let err = value_error::<Server>(toml::Value::Table(toml::map::Map::new()));
        assert_eq!(describe(&err), "missing field `port`");
    }

    #[test]
    fn a_spanned_error_does_not_reprint_the_source_line() {
        let src = format!("port = {SECRET:?}\n");
        let err = toml::from_str::<Server>(&src).unwrap_err();
        assert!(
            err.to_string().contains(SECRET),
            "precondition: the spanned error reprints the line\n{err}"
        );
        let described = describe(&err);
        assert!(!described.contains(SECRET), "{described}");
        assert!(described.contains("u16"), "{described}");
        assert!(!described.contains("TOML parse error"), "{described}");
    }
}
