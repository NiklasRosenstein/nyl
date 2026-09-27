//! Template values: `${ … }` expressions expanded after the structural pass.
//!
//! Contract: [Template values](../../../../design/release-inputs.md#template-values).
//!
//! A templated control resource is rendered as a whole before its fields are
//! used. A field that is expanded later, in a narrower context, such as
//! `ApplicationGroup.spec.applicationNameTemplate`, holds a template value:
//!
//! - `${ expression }` evaluates a MiniJinja expression, with filters, in the
//!   field's own context. Blocks are not available. An undefined variable or
//!   attribute is an error, so a typo cannot produce a wrong but valid value.
//! - `$${` writes a literal `${`.
//! - A value whose text outside `${ … }` still contains `{{`, `{%`, or `{#`
//!   after the structural pass, because it was protected with `{% raw %}`, is
//!   rendered as a template as before.
//! - A value that mixes both forms is an error.

use minijinja::{Environment, UndefinedBehavior};
use serde_json::Value;

use crate::{NylError, Result};

/// Template syntax that marks a value protected from the structural pass.
const TEMPLATE_MARKERS: [&str; 3] = ["{{", "{%", "{#"];

/// Expands template values; build it once and reuse it for every value.
pub struct TemplateValueExpander {
    /// The engine environment, for values in the protected `{{ … }}` form.
    template: Environment<'static>,
    /// The same environment with strict undefined handling, for `${ … }`.
    expression: Environment<'static>,
}

enum Segment<'a> {
    Literal(&'a str),
    Expression(&'a str),
}

impl TemplateValueExpander {
    pub(super) fn new(env: &Environment<'static>) -> Self {
        let mut expression = env.clone();
        expression.set_undefined_behavior(UndefinedBehavior::Strict);
        Self {
            template: env.clone(),
            expression,
        }
    }

    /// Expand `template` for `field` with `context`.
    pub fn expand(&self, field: &str, template: &str, context: &Value) -> Result<String> {
        let segments = parse(field, template)?;
        let has_expression = segments.iter().any(|segment| matches!(segment, Segment::Expression(_)));
        let has_template = segments.iter().any(|segment| {
            matches!(segment, Segment::Literal(text) if TEMPLATE_MARKERS.iter().any(|marker| text.contains(marker)))
        });
        if has_expression && has_template {
            return Err(NylError::config(format!(
                "{field} mixes ${{ … }} template values with {{{{ … }}}} template syntax; use only the ${{ … }} form"
            )));
        }
        if has_template {
            return self
                .template
                .render_str(template, context)
                .map_err(|error| NylError::config(format!("{field}: cannot render {template:?}: {error:#}")));
        }

        let mut output = String::with_capacity(template.len());
        for segment in segments {
            match segment {
                Segment::Literal(text) => output.push_str(text),
                Segment::Expression(expression) => output.push_str(&self.evaluate(field, expression, context)?),
            }
        }
        Ok(output)
    }

    fn evaluate(&self, field: &str, expression: &str, context: &Value) -> Result<String> {
        let cannot_evaluate = |error: &dyn std::fmt::Display| {
            NylError::config(format!("{field}: cannot evaluate ${{ {expression} }}: {error}"))
        };
        let value = self
            .expression
            .compile_expression(expression)
            .and_then(|compiled| compiled.eval(context))
            .map_err(|error| cannot_evaluate(&format!("{error:#}")))?;
        if value.is_undefined() {
            return Err(cannot_evaluate(&"the value is undefined"));
        }
        // Print values as the engine's `{{ … }}` formatter does: booleans in
        // lower case, everything else through `Value`'s display.
        if value.kind() == minijinja::value::ValueKind::Bool {
            return Ok(if value.is_true() { "true" } else { "false" }.to_owned());
        }
        Ok(value.to_string())
    }
}

/// Split `template` into literal text and `${ … }` expressions; `$${` becomes a literal `${`.
fn parse<'a>(field: &str, template: &'a str) -> Result<Vec<Segment<'a>>> {
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
            let end = expression_end(after).ok_or_else(|| {
                NylError::config(format!(
                    "{field} has an unterminated ${{ … }} template value: {template:?}"
                ))
            })?;
            let expression = after[..end].trim();
            if expression.is_empty() {
                return Err(NylError::config(format!(
                    "{field} has an empty ${{ … }} template value: {template:?}"
                )));
            }
            segments.push(Segment::Expression(expression));
            rest = &after[end + 1..];
        } else {
            segments.push(Segment::Literal(&rest[..=start]));
            rest = after_dollar;
        }
    }
    segments.push(Segment::Literal(rest));
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
    use crate::template::TemplateEngine;
    use serde_json::json;

    fn expand(template: &str) -> crate::Result<String> {
        TemplateEngine::new().template_values().expand(
            "spec.field",
            template,
            &json!({"target": {"metadata": {"name": "dev"}}, "release": {"metadata": {"name": "web"}}, "ok": true}),
        )
    }

    #[test]
    fn test_expand_template_value_evaluates_expressions_and_filters() {
        assert_eq!(
            expand("${ target.metadata.name }-${ release.metadata.name | upper }").unwrap(),
            "dev-WEB"
        );
        assert_eq!(expand("${ok}").unwrap(), "true");
        assert_eq!(expand("${ {'a': 'x}'}['a'] }").unwrap(), "x}");
    }

    #[test]
    fn test_expand_template_value_allows_template_markers_inside_expressions() {
        assert_eq!(expand("${{'a': 'x'}['a']}").unwrap(), "x");
        assert_eq!(expand("${ release.metadata.name ~ '{%' }").unwrap(), "web{%");
    }

    #[test]
    fn test_expand_template_value_keeps_literal_text_and_escapes() {
        assert_eq!(expand("plain-$name-$").unwrap(), "plain-$name-$");
        assert_eq!(expand("$${ release }").unwrap(), "${ release }");
    }

    #[test]
    fn test_expand_template_value_renders_protected_template_syntax() {
        assert_eq!(
            expand("{{ target.metadata.name }}-{{ release.metadata.name }}").unwrap(),
            "dev-web"
        );
        assert_eq!(expand("{# note #}{{ release.metadata.name }}").unwrap(), "web");
    }

    #[test]
    fn test_expand_template_value_rejects_mixed_malformed_and_undefined_values() {
        for value in [
            "${ release.metadata.name }-{{ target.metadata.name }}",
            "{# note #}-${ release.metadata.name }",
            "${ release.metadata.name",
            "${ }",
            "${ release.metadata.name ++ }",
            "${ release.metadata.nmae }",
            "${ target.metadata.name }-${ release.metadata.nmae ~ 'x' }",
            "{{ release.metadata.name | nosuchfilter }}",
        ] {
            let error = expand(value).unwrap_err().to_string();
            assert!(error.contains("spec.field"), "{value}: {error}");
        }
    }
}
