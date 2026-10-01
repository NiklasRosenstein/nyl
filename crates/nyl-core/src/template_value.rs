//! Parsing of template values: literal text with `${ … }` expressions.
//!
//! Contract: [Template values](../../../../design/release-inputs.md#template-values).
//!
//! - `${ expression }` is an expression. Its end is the first `}` that closes
//!   no brace opened inside it and lies outside string literals, so an
//!   expression may contain literals such as `{'a': 'x}'}`.
//! - `$${` writes a literal `${`.
//! - All other text is literal, including `{{`, `{%`, and `{#`.
//!
//! Static validation and expansion share this one parser, so a value accepted
//! by validation is split into the same expressions when it is expanded.

use std::fmt;

/// A piece of a template value, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment<'a> {
    /// Literal text, already unescaped.
    Literal(&'a str),
    /// The trimmed source of a `${ … }` expression.
    Expression(&'a str),
}

/// Why a template value cannot be parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// A `${` has no closing `}`.
    Unterminated,
    /// A `${ }` holds no expression.
    Empty,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unterminated => "an unterminated ${ … } template value",
            Self::Empty => "an empty ${ … } template value",
        })
    }
}

/// Split `template` into literal text and `${ … }` expressions.
pub fn parse(template: &str) -> Result<Vec<Segment<'_>>, ParseError> {
    let mut segments = Vec::new();
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        let after_dollar = &rest[start + 1..];
        if let Some(after) = after_dollar.strip_prefix("${") {
            segments.push(Segment::Literal(&rest[..start]));
            segments.push(Segment::Literal("${"));
            rest = after;
        } else if let Some(after) = after_dollar.strip_prefix('{') {
            segments.push(Segment::Literal(&rest[..start]));
            let end = expression_end(after).ok_or(ParseError::Unterminated)?;
            let expression = after[..end].trim();
            if expression.is_empty() {
                return Err(ParseError::Empty);
            }
            segments.push(Segment::Expression(expression));
            rest = &after[end + 1..];
        } else {
            segments.push(Segment::Literal(&rest[..=start]));
            rest = after_dollar;
        }
    }
    segments.push(Segment::Literal(rest));
    segments.retain(|segment| *segment != Segment::Literal(""));
    Ok(segments)
}

/// Byte offset of the `}` that closes an expression, skipping braces inside
/// string literals and nested literals such as `{'a': 1}`.
fn expression_end(expression: &str) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, character) in expression.char_indices() {
        if let Some(open) = quote {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == open {
                quote = None;
            }
            continue;
        }
        match character {
            '\'' | '"' => quote = Some(character),
            '{' => depth += 1,
            '}' if depth == 0 => return Some(index),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use Segment::{Expression, Literal};

    #[test]
    fn test_parse_splits_literals_and_expressions() {
        assert_eq!(
            parse("${ target.metadata.name }-${release.metadata.name}").unwrap(),
            [
                Expression("target.metadata.name"),
                Literal("-"),
                Expression("release.metadata.name")
            ]
        );
        assert_eq!(parse("plain-$name-$").unwrap(), [Literal("plain-$"), Literal("name-$")]);
    }

    #[test]
    fn test_parse_unescapes_dollar_dollar_brace() {
        assert_eq!(parse("$${ release }").unwrap(), [Literal("${"), Literal(" release }")]);
    }

    #[test]
    fn test_parse_skips_braces_in_string_and_nested_literals() {
        assert_eq!(
            parse("${ {'a': 'x}'}['a'] }").unwrap(),
            [Expression("{'a': 'x}'}['a']")]
        );
        assert_eq!(parse("${{'a': 'x'}['a']}").unwrap(), [Expression("{'a': 'x'}['a']")]);
        assert_eq!(parse(r#"${ "a\"}" }"#).unwrap(), [Expression(r#""a\"}""#)]);
    }

    #[test]
    fn test_parse_keeps_minijinja_delimiters_literal() {
        assert_eq!(
            parse("{{ x }}{% y %}{# z #}").unwrap(),
            [Literal("{{ x }}{% y %}{# z #}")]
        );
    }

    #[test]
    fn test_parse_rejects_unterminated_and_empty_expressions() {
        assert_eq!(parse("${ release.metadata.name"), Err(ParseError::Unterminated));
        assert_eq!(parse("ok-${ 'x}"), Err(ParseError::Unterminated));
        assert_eq!(parse("${ }"), Err(ParseError::Empty));
    }
}
