//! JSON Pointer (RFC 6901) helpers.

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
    fn test_child_escapes_reserved_characters() {
        assert_eq!(child("/a", "b/c~d"), "/a/b~1c~0d");
        assert_eq!(child("", "x"), "/x");
    }
}
