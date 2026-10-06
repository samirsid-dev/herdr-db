//! Minimal SQL highlighting for the DDL view and the console editor:
//! keywords, strings, comments, numbers. No parsing.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

const KEYWORDS: &[&str] = &[
    "ADD",
    "ALL",
    "ALTER",
    "ALWAYS",
    "AND",
    "AS",
    "ASC",
    "BEFORE",
    "AFTER",
    "BEGIN",
    "BETWEEN",
    "BY",
    "CASCADE",
    "CASE",
    "CHECK",
    "COLUMN",
    "COMMENT",
    "COMMIT",
    "CONSTRAINT",
    "CREATE",
    "CROSS",
    "DATA",
    "DEFAULT",
    "DELETE",
    "DESC",
    "DISTINCT",
    "DROP",
    "EACH",
    "ELSE",
    "END",
    "ENGINE",
    "EXECUTE",
    "EXISTS",
    "EXPLAIN",
    "FOR",
    "FOREIGN",
    "FROM",
    "FULL",
    "FUNCTION",
    "GENERATED",
    "GROUP",
    "HAVING",
    "IDENTITY",
    "IF",
    "IN",
    "INDEX",
    "INNER",
    "INSERT",
    "INTO",
    "IS",
    "JOIN",
    "KEY",
    "LEFT",
    "LIKE",
    "LIMIT",
    "MATERIALIZED",
    "NOT",
    "NULL",
    "OF",
    "OFFSET",
    "ON",
    "ONLY",
    "OR",
    "ORDER",
    "OUTER",
    "PARTITION",
    "PRIMARY",
    "PROCEDURE",
    "RANGE",
    "REFERENCES",
    "RETURNING",
    "RETURNS",
    "RIGHT",
    "ROLLBACK",
    "ROW",
    "SELECT",
    "SEQUENCE",
    "SET",
    "STORED",
    "TABLE",
    "THEN",
    "TO",
    "TRIGGER",
    "TRUNCATE",
    "UNION",
    "UNIQUE",
    "UPDATE",
    "USING",
    "VALUES",
    "VIEW",
    "VIRTUAL",
    "WHEN",
    "WHERE",
    "WITH",
];

fn keyword() -> Style {
    Style::new().fg(Color::LightBlue).add_modifier(Modifier::BOLD)
}
fn string() -> Style {
    Style::new().fg(Color::Green)
}
fn comment() -> Style {
    Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
}
fn number() -> Style {
    Style::new().fg(Color::Cyan)
}

#[derive(Clone, Copy, PartialEq)]
enum State {
    Code,
    String(char),
    BlockComment,
}

/// Highlights `text`, one `Line` per text line; state carries across lines
/// (multi-line strings and comments).
pub fn highlight(text: &str) -> Vec<Line<'static>> {
    let mut state = State::Code;
    text.split('\n').map(|line| highlight_line(line, &mut state)).collect()
}

fn highlight_line(line: &str, state: &mut State) -> Line<'static> {
    let chars: Vec<char> = line.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut i = 0;
    let mut plain = String::new();
    let flush = |plain: &mut String, spans: &mut Vec<Span<'static>>| {
        if !plain.is_empty() {
            spans.push(Span::raw(std::mem::take(plain)));
        }
    };
    while i < chars.len() {
        match *state {
            State::String(quote) => {
                let start = i;
                while i < chars.len() {
                    if chars[i] == quote {
                        if chars.get(i + 1) == Some(&quote) {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        *state = State::Code;
                        break;
                    }
                    i += 1;
                }
                spans.push(Span::styled(chars[start..i].iter().collect::<String>(), string()));
            }
            State::BlockComment => {
                let start = i;
                while i < chars.len() {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        i += 2;
                        *state = State::Code;
                        break;
                    }
                    i += 1;
                }
                spans.push(Span::styled(chars[start..i].iter().collect::<String>(), comment()));
            }
            State::Code => {
                let c = chars[i];
                if c == '-' && chars.get(i + 1) == Some(&'-') {
                    flush(&mut plain, &mut spans);
                    spans.push(Span::styled(chars[i..].iter().collect::<String>(), comment()));
                    i = chars.len();
                } else if c == '/' && chars.get(i + 1) == Some(&'*') {
                    flush(&mut plain, &mut spans);
                    *state = State::BlockComment;
                } else if c == '\'' {
                    flush(&mut plain, &mut spans);
                    let start = i;
                    i += 1;
                    *state = State::String('\'');
                    while i < chars.len() {
                        if chars[i] == '\'' {
                            if chars.get(i + 1) == Some(&'\'') {
                                i += 2;
                                continue;
                            }
                            i += 1;
                            *state = State::Code;
                            break;
                        }
                        i += 1;
                    }
                    spans.push(Span::styled(chars[start..i].iter().collect::<String>(), string()));
                } else if c.is_ascii_digit() && (i == 0 || !chars[i - 1].is_alphanumeric() && chars[i - 1] != '_') {
                    flush(&mut plain, &mut spans);
                    let start = i;
                    while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                        i += 1;
                    }
                    spans.push(Span::styled(chars[start..i].iter().collect::<String>(), number()));
                } else if c.is_alphabetic() || c == '_' {
                    let start = i;
                    while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                        i += 1;
                    }
                    let word: String = chars[start..i].iter().collect();
                    if KEYWORDS.contains(&word.to_ascii_uppercase().as_str()) {
                        flush(&mut plain, &mut spans);
                        spans.push(Span::styled(word, keyword()));
                    } else {
                        plain.push_str(&word);
                    }
                } else {
                    plain.push(c);
                    i += 1;
                }
            }
        }
    }
    flush(&mut plain, &mut spans);
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(line: &Line) -> Vec<(String, bool)> {
        line.spans.iter().map(|s| (s.content.to_string(), s.style == keyword())).collect()
    }

    #[test]
    fn keywords_strings_and_comments() {
        let lines = highlight("select 'a''b', x1 from t -- c\n/* multi\nline */ where");
        assert_eq!(lines.len(), 3);
        let first = kinds(&lines[0]);
        assert_eq!(first[0], ("select".into(), true));
        assert!(first.iter().any(|(t, _)| t == "'a''b'"));
        assert!(first.iter().any(|(t, k)| t == "from" && *k));
        assert_eq!(lines[1].spans[0].style, comment());
        assert_eq!(lines[2].spans[0].content, "line */");
        assert_eq!(lines[2].spans.last().unwrap().content, "where");
    }
}
