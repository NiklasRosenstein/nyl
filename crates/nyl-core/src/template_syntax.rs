//! Detection of MiniJinja template syntax in authored strings.
//!
//! Static-envelope rules reject template syntax in fields that must be known
//! before rendering; they share this one detector.

/// Delimiters that open MiniJinja expressions, statements, and comments.
pub const TEMPLATE_DELIMITERS: [&str; 3] = ["{{", "{%", "{#"];

/// Whether `value` contains a MiniJinja expression, statement, or comment.
pub fn has_template_syntax(value: &str) -> bool {
    TEMPLATE_DELIMITERS.iter().any(|delimiter| value.contains(delimiter))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_has_template_syntax_detects_every_delimiter() {
        for value in ["{{ x }}", "{% if x %}", "{# note #}"] {
            assert!(has_template_syntax(value), "{value}");
        }
        assert!(!has_template_syntax("literal { value }"));
    }
}
