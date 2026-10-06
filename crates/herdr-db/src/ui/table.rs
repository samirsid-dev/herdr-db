//! Result table shared by the data grid and the console: cell cursor,
//! horizontal and vertical scrolling, row selection, copy helpers.

use super::theme;
use herdr_db_core::cell::{self, Cell, ResultColumn};
use herdr_db_core::paging::SortDir;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_WIDTH: usize = 40;
const MIN_WIDTH: usize = 3;
const PREVIEW: usize = 200;

#[derive(Debug, Clone, Default)]
pub struct TableView {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Cell>>,
    pub row: usize,
    pub col: usize,
    row_offset: usize,
    col_offset: usize,
    /// Row where a visual selection started.
    pub anchor: Option<usize>,
    widths: Vec<usize>,
    /// Rows visible at the last render, for page moves.
    pub viewport: usize,
    /// Header decorations: sorted column, key columns.
    pub sort: Option<(String, SortDir)>,
    pub key_columns: Vec<String>,
    pub fk_columns: Vec<String>,
}

fn display_width(text: &str) -> usize {
    text.width()
}

/// Cuts `text` to `width` columns, with an ellipsis when cut.
fn fit(text: &str, width: usize) -> String {
    if display_width(text) <= width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

impl TableView {
    pub fn set(&mut self, columns: Vec<ResultColumn>, rows: Vec<Vec<Cell>>) {
        self.widths = columns
            .iter()
            .enumerate()
            .map(|(i, column)| {
                let header = display_width(&column.name) + 2;
                let content = rows
                    .iter()
                    .map(|r| r.get(i).map_or(0, |c| display_width(&c.preview(MAX_WIDTH))))
                    .max()
                    .unwrap_or(0);
                header.max(content).clamp(MIN_WIDTH, MAX_WIDTH)
            })
            .collect();
        self.columns = columns;
        self.rows = rows;
        self.row = self.row.min(self.rows.len().saturating_sub(1));
        self.col = self.col.min(self.columns.len().saturating_sub(1));
        self.anchor = None;
    }

    pub fn clear(&mut self) {
        *self = TableView {
            sort: self.sort.take(),
            key_columns: std::mem::take(&mut self.key_columns),
            fk_columns: std::mem::take(&mut self.fk_columns),
            ..TableView::default()
        };
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn move_row(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() as isize - 1;
        self.row = (self.row as isize + delta).clamp(0, last) as usize;
    }

    pub fn move_col(&mut self, delta: isize) {
        if self.columns.is_empty() {
            return;
        }
        let last = self.columns.len() as isize - 1;
        self.col = (self.col as isize + delta).clamp(0, last) as usize;
    }

    pub fn top(&mut self) {
        self.row = 0;
    }

    pub fn bottom(&mut self) {
        self.row = self.rows.len().saturating_sub(1);
    }

    pub fn page(&mut self, direction: isize) {
        let step = self.viewport.max(1) as isize;
        self.move_row(direction * step);
    }

    pub fn toggle_selection(&mut self) {
        self.anchor = match self.anchor {
            Some(_) => None,
            None => Some(self.row),
        };
    }

    /// Selected rows, or the current row.
    pub fn selected_rows(&self) -> Range<usize> {
        match self.anchor {
            Some(anchor) => anchor.min(self.row)..anchor.max(self.row) + 1,
            None => self.row..self.row + 1,
        }
    }

    pub fn current_cell(&self) -> Option<&Cell> {
        self.rows.get(self.row)?.get(self.col)
    }

    pub fn current_column(&self) -> Option<&ResultColumn> {
        self.columns.get(self.col)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    pub fn rows_csv(&self) -> String {
        let rows: Vec<&[Cell]> = self.rows[self.selected_rows()].iter().map(Vec::as_slice).collect();
        cell::to_csv(&self.columns, &rows)
    }

    pub fn rows_json(&self) -> String {
        let rows: Vec<&[Cell]> = self.rows[self.selected_rows()].iter().map(Vec::as_slice).collect();
        cell::to_json(&self.columns, &rows)
    }

    fn scroll_into_view(&mut self, height: usize, width: usize) {
        self.viewport = height;
        if self.row < self.row_offset {
            self.row_offset = self.row;
        } else if height > 0 && self.row >= self.row_offset + height {
            self.row_offset = self.row + 1 - height;
        }
        if self.col < self.col_offset {
            self.col_offset = self.col;
        }
        while self.col_offset < self.col {
            let span: usize = self.widths[self.col_offset..=self.col].iter().map(|w| w + 1).sum();
            if span <= width {
                break;
            }
            self.col_offset += 1;
        }
    }

    /// Visible column indexes and their widths, from `col_offset`.
    fn visible_columns(&self, width: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut used = 0;
        for (i, w) in self.widths.iter().enumerate().skip(self.col_offset) {
            if used >= width {
                break;
            }
            let w = (*w).min(width - used);
            out.push((i, w));
            used += w + 1;
        }
        out
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, focused: bool) {
        if area.height == 0 {
            return;
        }
        if self.columns.is_empty() {
            frame.render_widget(Paragraph::new(Line::styled("(aucune colonne)", theme::dim())), area);
            return;
        }
        let width = area.width as usize;
        self.scroll_into_view(area.height.saturating_sub(1) as usize, width);
        let visible = self.visible_columns(width);

        let mut header = Vec::new();
        for (i, w) in &visible {
            let column = &self.columns[*i];
            let mut name = column.name.clone();
            if let Some((sorted, dir)) = &self.sort
                && *sorted == column.name
            {
                name.push_str(if *dir == SortDir::Asc { " ▲" } else { " ▼" });
            }
            let mut style = Style::new().add_modifier(Modifier::BOLD);
            if self.key_columns.contains(&column.name) {
                style = style.fg(theme::GOLD);
            } else if self.fk_columns.contains(&column.name) {
                style = style.fg(theme::FK_BLUE);
            }
            if focused && *i == self.col {
                style = style.add_modifier(Modifier::UNDERLINED);
            }
            header.push(Span::styled(format!("{:<w$}", fit(&name, *w), w = *w), style));
            header.push(Span::raw(" "));
        }
        let mut lines = vec![Line::from(header)];

        let selected = self.selected_rows();
        let height = area.height.saturating_sub(1) as usize;
        for r in self.row_offset..(self.row_offset + height).min(self.rows.len()) {
            let row = &self.rows[r];
            let in_selection = self.anchor.is_some() && selected.contains(&r);
            let mut spans = Vec::new();
            for (i, w) in &visible {
                let cell = row.get(*i).unwrap_or(&Cell::Null);
                let text = cell.preview(PREVIEW);
                let text = fit(&text, *w);
                let padded = if self.columns[*i].is_numeric() {
                    format!("{text:>w$}", w = *w)
                } else {
                    format!("{text:<w$}", w = *w)
                };
                let mut style = match cell {
                    Cell::Null => theme::null_style(),
                    Cell::Binary { .. } => theme::dim(),
                    Cell::Json(_) => Style::new().fg(Color::LightYellow),
                    Cell::Text(_) => Style::new(),
                };
                if in_selection {
                    style = style.bg(Color::Indexed(238));
                }
                if r == self.row && (*i == self.col || !focused) {
                    style =
                        if focused { style.add_modifier(Modifier::REVERSED) } else { style.bg(Color::Indexed(236)) };
                } else if r == self.row {
                    style = style.bg(Color::Indexed(236));
                }
                spans.push(Span::styled(padded, style));
                spans.push(Span::styled(
                    " ",
                    if in_selection { Style::new().bg(Color::Indexed(238)) } else { Style::new() },
                ));
            }
            lines.push(Line::from(spans));
        }
        if self.rows.is_empty() {
            lines.push(Line::styled("(aucune ligne)", theme::dim()));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Mouse click inside the table area: selects the clicked cell.
    pub fn click(&mut self, area: Rect, x: u16, y: u16) -> bool {
        if y <= area.y || y >= area.y + area.height || x < area.x {
            return false;
        }
        let row = self.row_offset + (y - area.y - 1) as usize;
        if row >= self.rows.len() {
            return false;
        }
        let mut left = area.x as usize;
        for (i, w) in self.visible_columns(area.width as usize) {
            if (x as usize) < left + w + 1 {
                self.row = row;
                self.col = i;
                return true;
            }
            left += w + 1;
        }
        false
    }

    /// Fields of a row for the inspector: (column, type, value).
    pub fn row_fields(&self, row: usize) -> Vec<(String, Option<String>, Cell)> {
        let Some(values) = self.rows.get(row) else {
            return Vec::new();
        };
        self.columns.iter().zip(values).map(|(c, v)| (c.name.clone(), c.type_name.clone(), v.clone())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn table() -> TableView {
        let mut t = TableView::default();
        t.set(
            vec![
                ResultColumn::new("id", Some("integer".into())),
                ResultColumn::new("name", Some("text".into())),
                ResultColumn::new("note", Some("text".into())),
            ],
            vec![
                vec![Cell::Text("1".into()), Cell::Text("Ada".into()), Cell::Null],
                vec![Cell::Text("22".into()), Cell::Text("Grace".into()), Cell::Text(String::new())],
            ],
        );
        t.key_columns = vec!["id".into()];
        t
    }

    #[test]
    fn renders_null_distinct_from_empty() {
        let mut view = table();
        let mut terminal = Terminal::new(TestBackend::new(30, 4)).unwrap();
        terminal.draw(|f| view.render(f, f.area(), true)).unwrap();
        insta::assert_snapshot!(terminal.backend(), @r#"
        "id   name   note              "
        "   1 Ada    <null>            "
        "  22 Grace                    "
        "                              "
        "#);
    }

    #[test]
    fn selection_and_export() {
        let mut view = table();
        view.toggle_selection();
        view.move_row(1);
        assert_eq!(view.selected_rows(), 0..2);
        assert_eq!(view.rows_csv(), "id,name,note\n1,Ada,\n22,Grace,\n");
        view.toggle_selection();
        assert_eq!(view.selected_rows(), 1..2);
        view.move_col(5);
        assert_eq!(view.current_cell(), Some(&Cell::Text(String::new())));
    }

    #[test]
    fn fit_truncates_by_width() {
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
    }
}
