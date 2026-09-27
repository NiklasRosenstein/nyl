//! Template values: `${ … }` expressions expanded after the structural pass.
//!
//! Contract: [Template values](../../../../design/release-inputs.md#template-values).
//!
//! A templated control resource is rendered as a whole before its fields are
//! used. A field that is expanded later, in a narrower context, such as
//! `ApplicationGroup.spec.applicationNameTemplate`, holds a template value:
//!
//! - `${ expression }` evaluates a MiniJinja expression, with filters, in the
//!   field's own context. Blocks are not available.
//! - `$${` writes a literal `${`.
//! - A value that still contains `{{` or `{%` after the structural pass, because
//!   it was protected with `{% raw %}`, is rendered as a template as before.
//! - A value that mixes both forms is an error.

use minijinja::Environment;
use serde_json::Value;

use crate::{NylError, Result};

/// Expand `template` for `field` with `context`.
pub(super) fn expand(env: &Environment<'static>, field: &str, template: &str, context: &Value) -> Result<String> {
    let has_expression = template.contains("${");
    let has_template = template.contains("{{") || template.contains("{%");
    if has_expression && has_template {
        return Err(NylError::config(format!(
            "{field} mixes ${{ … }} template values with {{{{ … }}}} template syntax; use only the ${{ … }} form"
        )));
    }
    if has_template {
        return Ok(env.render_str(template, context)?);
    }

    let mut output = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        output.push_str(&rest[..start]);
        rest = &rest[start..];
        if let Some(after) = rest.strip_prefix("$${") {
            output.push_str("${");
            rest = after;
        } else if let Some(after) = rest.strip_prefix("${") {
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
            output.push_str(&evaluate(env, field, expression, context)?);
            rest = &after[end + 1..];
        } else {
            output.push('$');
            rest = &rest[1..];
        }
    }
    output.push_str(rest);
    Ok(output)
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

fn evaluate(env: &Environment<'static>, field: &str, expression: &str, context: &Value) -> Result<String> {
    let value = env
        .compile_expression(expression)
        .and_then(|compiled| compiled.eval(context))
        .map_err(|error| NylError::config(format!("{field}: cannot evaluate ${{ {expression} }}: {error}")))?;
    // Format through a template so values print exactly as in `{{ … }}`.
    Ok(env.render_str("{{ value }}", minijinja::context! { value })?)
}

#[cfg(test)]
mod tests {
    use crate::template::TemplateEngine;
    use serde_json::json;

    fn expand(template: &str) -> crate::Result<String> {
        TemplateEngine::new().expand_template_value(
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
    }

    #[test]
    fn test_expand_template_value_rejects_mixed_and_malformed_values() {
        for value in [
            "${ release.metadata.name }-{{ target.metadata.name }}",
            "${ release.metadata.name",
            "${ }",
            "${ release.metadata.name ++ }",
        ] {
            let error = expand(value).unwrap_err().to_string();
            assert!(error.contains("spec.field"), "{value}: {error}");
        }
    }
}
