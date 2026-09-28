//! JSON Pointer (RFC 6901) helpers.

use serde_json::Value;

use crate::{CoreError, Result};

/// Validate pointer syntax: empty for the whole document, or `/`-separated
/// reference tokens where `~` appears only as `~0` or `~1`.
pub fn validate(field: &str, pointer: &str) -> Result<()> {
    let tokens_valid = pointer.split('/').skip(1).all(|token| {
        let mut characters = token.chars();
        while let Some(character) = characters.next() {
            if character == '~' && !matches!(characters.next(), Some('0' | '1')) {
                return false;
            }
        }
        true
    });
    if (pointer.is_empty() || pointer.starts_with('/')) && tokens_valid {
        Ok(())
    } else {
        Err(CoreError::config(format!(
            "{field} must be a JSON Pointer such as \"/image/tag\", or empty for the whole document; got {pointer:?}"
        )))
    }
}

/// Select the value at `pointer` inside `document`.
pub fn resolve<'a>(document: &'a Value, pointer: &str) -> Option<&'a Value> {
    document.pointer(pointer)
}

/// Append one reference token to `pointer`, escaping `~` and `/`.
pub fn child(pointer: &str, key: &str) -> String {
    format!("{pointer}/{}", escape_token(key))
}

/// Escape one reference token.
pub fn escape_token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_accepts_rfc6901_pointers() {
        for pointer in ["", "/", "/a/b", "/a~1b", "/~0"] {
            validate("p", pointer).unwrap();
        }
        for pointer in ["a", "/a~", "/a~2"] {
            assert!(validate("p", pointer).is_err(), "{pointer}");
        }
    }

    #[test]
    fn test_resolve_unescapes_tokens() {
        let document = serde_json::json!({"a/b": {"~": 1}});
        assert_eq!(resolve(&document, "/a~1b/~0"), Some(&serde_json::json!(1)));
        assert_eq!(resolve(&document, ""), Some(&document));
        assert_eq!(resolve(&document, "/missing"), None);
    }

    #[test]
    fn test_child_escapes_reserved_characters() {
        assert_eq!(child("/a", "b/c~d"), "/a/b~1c~0d");
        assert_eq!(child("", "x"), "/x");
    }
}
