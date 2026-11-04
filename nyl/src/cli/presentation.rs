//! Shared terminal styles for human-readable output.

#[derive(Clone, Copy)]
pub(crate) enum Style {
    Bold,
    Dim,
    Warning,
    Error,
    Success,
    Attention,
    Failure,
}

pub(crate) fn styled(text: &str, style: Style) -> String {
    styled_with_color(text, style, colored::control::SHOULD_COLORIZE.should_colorize())
}

fn styled_with_color(text: &str, style: Style, color: bool) -> String {
    if !color || text.is_empty() {
        return text.to_owned();
    }
    let code = match style {
        Style::Bold => "1",
        Style::Dim => "2",
        Style::Warning => "1;33",
        Style::Error => "1;31",
        Style::Success => "32",
        Style::Attention => "33",
        Style::Failure => "31",
    };
    format!("\x1b[{code}m{text}\x1b[0m")
}

pub(crate) fn result_style(result: &nyl_core::orchestration::progress::UnitResult) -> Option<Style> {
    use nyl_core::orchestration::progress::UnitResult;
    match result {
        UnitResult::Executed { .. }
        | UnitResult::Imported { .. }
        | UnitResult::Recovered { .. }
        | UnitResult::Removed
        | UnitResult::Released { .. } => Some(Style::Success),
        UnitResult::Failed { .. } => Some(Style::Failure),
        UnitResult::Current | UnitResult::Retained => None,
        _ => Some(Style::Attention),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_preserve_text_and_follow_color_choice() {
        assert_eq!(styled_with_color("warning", Style::Warning, false), "warning");
        assert_eq!(
            styled_with_color("warning", Style::Warning, true),
            "\x1b[1;33mwarning\x1b[0m"
        );
        assert_eq!(styled_with_color("code", Style::Dim, true), "\x1b[2mcode\x1b[0m");
    }
}
