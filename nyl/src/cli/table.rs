use unicode_width::UnicodeWidthStr;

/// A table cell whose rendered representation may contain zero-width styling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cell {
    rendered: String,
    width: usize,
}

impl Cell {
    pub(crate) fn plain(value: impl ToString) -> Self {
        let rendered = value.to_string();
        let width = UnicodeWidthStr::width(rendered.as_str());
        Self { rendered, width }
    }

    pub(crate) fn styled(visible: impl AsRef<str>, rendered: impl ToString) -> Self {
        Self {
            rendered: rendered.to_string(),
            width: UnicodeWidthStr::width(visible.as_ref()),
        }
    }
}

/// Render an unwrapped table with a two-space column gap and no trailing whitespace.
pub(crate) fn render(headers: &[&str], rows: &[Vec<Cell>]) -> String {
    let mut widths = headers
        .iter()
        .map(|header| UnicodeWidthStr::width(*header))
        .collect::<Vec<_>>();

    for row in rows {
        assert_eq!(row.len(), headers.len());
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.width);
        }
    }

    let mut output = String::new();
    write_line(
        &mut output,
        headers.iter().map(|header| (*header, UnicodeWidthStr::width(*header))),
        &widths,
    );
    for row in rows {
        output.push('\n');
        write_line(
            &mut output,
            row.iter().map(|cell| (cell.rendered.as_str(), cell.width)),
            &widths,
        );
    }
    output
}

fn write_line<'a>(output: &mut String, cells: impl Iterator<Item = (&'a str, usize)>, widths: &[usize]) {
    for (index, ((rendered, visible_width), column_width)) in cells.zip(widths).enumerate() {
        output.push_str(rendered);
        if index + 1 < widths.len() {
            output.extend(std::iter::repeat_n(' ', column_width - visible_width + 2));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_cells_align_by_display_width_without_trailing_whitespace() {
        let rows = vec![
            vec![Cell::plain("猫"), Cell::plain("one")],
            vec![Cell::plain("café"), Cell::plain("two")],
        ];

        let output = render(&["NAME", "VALUE"], &rows);

        assert_eq!(output, "NAME  VALUE\n猫    one\ncafé  two");
        assert!(output.lines().all(|line| !line.ends_with(' ')));
    }

    #[test]
    fn ansi_styling_does_not_change_column_alignment() {
        let rows = vec![
            vec![Cell::styled("ok", "\x1b[32mok\x1b[0m"), Cell::plain(1)],
            vec![Cell::plain("failed"), Cell::plain(2)],
        ];

        assert_eq!(
            render(&["STATUS", "REVISION"], &rows),
            "STATUS  REVISION\n\x1b[32mok\x1b[0m      1\nfailed  2"
        );
    }

    #[test]
    fn cell_content_is_not_wrapped_or_truncated() {
        let value = "a-value-that-is-longer-than-a-narrow-terminal";
        let rows = vec![vec![Cell::plain(value)]];

        assert_eq!(render(&["VALUE"], &rows), format!("VALUE\n{value}"));
    }
}
