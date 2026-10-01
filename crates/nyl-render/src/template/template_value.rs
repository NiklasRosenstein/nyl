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
//! - All other text is literal, including `{{`, `{%`, and `{#` that survived
//!   the structural pass, for example through `{% raw %}`. It is never rendered
//!   as a template.
//! - A value needs no expression; whether a field requires one is the field's
//!   own rule.

use minijinja::{Environment, UndefinedBehavior};
use nyl_core::template_value::{parse, Segment};
use serde_json::Value;

use crate::{NylError, Result};

/// Expands template values; build it once and reuse it for every value.
pub struct TemplateValueExpander {
    /// The engine environment with strict undefined handling.
    expression: Environment<'static>,
}

impl TemplateValueExpander {
    pub(super) fn new(env: &Environment<'static>) -> Self {
        let mut expression = env.clone();
        expression.set_undefined_behavior(UndefinedBehavior::Strict);
        Self { expression }
    }

    /// Expand `template` for `field` with `context`.
    pub fn expand(&self, field: &str, template: &str, context: &Value) -> Result<String> {
        let segments =
            parse(template).map_err(|error| NylError::config(format!("{field} has {error}: {template:?}")))?;
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
        // Print values as the engine's template formatter does: booleans in
        // lower case, everything else through `Value`'s display.
        if value.kind() == minijinja::value::ValueKind::Bool {
            return Ok(if value.is_true() { "true" } else { "false" }.to_owned());
        }
        Ok(value.to_string())
    }
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
    fn test_expand_template_value_keeps_template_syntax_outside_expressions_literal() {
        assert_eq!(
            expand("${ release.metadata.name }-{{ target.metadata.name }}").unwrap(),
            "web-{{ target.metadata.name }}"
        );
        assert_eq!(expand("{# note #}{% x %}").unwrap(), "{# note #}{% x %}");
    }

    #[test]
    fn test_expand_template_value_rejects_malformed_and_undefined_values() {
        for value in [
            "${ release.metadata.name",
            "${ }",
            "${ release.metadata.name ++ }",
            "${ release.metadata.nmae }",
            "${ target.metadata.name }-${ release.metadata.nmae ~ 'x' }",
            "${ release.metadata.name | nosuchfilter }",
        ] {
            let error = expand(value).unwrap_err().to_string();
            assert!(error.contains("spec.field"), "{value}: {error}");
        }
    }
}
