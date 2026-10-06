//! Splits console text into statements, so the console runs the statement
//! under the cursor. A small lexer: quotes, comments and PostgreSQL dollar
//! quoting are skipped, `;` outside of them ends a statement.

use crate::model::Engine;
use std::ops::Range;

/// Byte ranges of the statements, without the `;` and surrounding blanks.
/// Blank and comment-only pieces are dropped.
pub fn split(engine: Engine, text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut i = 0;
    let mut has_code = false;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\'' | b'"' => {
                i = skip_quoted(bytes, i, c, engine == Engine::MySql);
                has_code = true;
                continue;
            }
            b'`' if engine == Engine::MySql => {
                i = skip_quoted(bytes, i, c, false);
                has_code = true;
                continue;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                i = skip_line(bytes, i);
                continue;
            }
            b'#' if engine == Engine::MySql => {
                i = skip_line(bytes, i);
                continue;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(bytes, i);
                continue;
            }
            b'$' if engine == Engine::Postgres => {
                if let Some(end) = skip_dollar_quote(text, i) {
                    i = end;
                    has_code = true;
                    continue;
                }
            }
            b';' => {
                if has_code {
                    ranges.push(trim(text, start..i));
                }
                start = i + 1;
                has_code = false;
                i += 1;
                continue;
            }
            _ => {}
        }
        if !c.is_ascii_whitespace() {
            has_code = true;
        }
        i += 1;
    }
    if has_code {
        ranges.push(trim(text, start..text.len()));
    }
    ranges
}

/// The statement containing `cursor` (a byte offset), or the closest one
/// before it when the cursor sits between statements.
pub fn statement_at(engine: Engine, text: &str, cursor: usize) -> Option<Range<usize>> {
    let ranges = split(engine, text);
    ranges
        .iter()
        .find(|r| cursor >= r.start && cursor <= r.end)
        .or_else(|| ranges.iter().rev().find(|r| r.end <= cursor))
        .or(ranges.first())
        .cloned()
}

fn trim(text: &str, range: Range<usize>) -> Range<usize> {
    let piece = &text[range.clone()];
    let leading = piece.len() - piece.trim_start().len();
    let trailing = piece.len() - piece.trim_end().len();
    range.start + leading..range.end - trailing
}

fn skip_quoted(bytes: &[u8], start: usize, quote: u8, backslash_escapes: bool) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        let c = bytes[i];
        if backslash_escapes && c == b'\\' {
            i += 2;
            continue;
        }
        if c == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    bytes.len()
}

fn skip_line(bytes: &[u8], start: usize) -> usize {
    bytes[start..].iter().position(|&b| b == b'\n').map_or(bytes.len(), |p| start + p + 1)
}

fn skip_block_comment(bytes: &[u8], start: usize) -> usize {
    let mut depth = 0;
    let mut i = start;
    while i + 1 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            depth += 1;
            i += 2;
        } else if bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    bytes.len()
}

/// `$tag$ ... $tag$`. Returns `None` for a `$1` parameter or a lone `$`.
fn skip_dollar_quote(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start + 1..];
    let tag_len = rest.find('$')?;
    let tag = &rest[..tag_len];
    if !tag.chars().all(|c| c.is_alphanumeric() || c == '_') || tag.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    let delimiter = format!("${tag}$");
    let body_start = start + delimiter.len();
    let end = text[body_start..].find(&delimiter)?;
    Some(body_start + end + delimiter.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pieces(engine: Engine, text: &str) -> Vec<&str> {
        split(engine, text).into_iter().map(|r| &text[r]).collect::<Vec<_>>()
    }

    #[test]
    fn splits_on_semicolons_outside_literals() {
        let text = "select 'a;b';\n  select \"x;\" from t ; -- c;\n\n";
        assert_eq!(pieces(Engine::Postgres, text), vec!["select 'a;b'", "select \"x;\" from t"]);
    }

    #[test]
    fn dollar_quotes_and_block_comments() {
        let text = "create function f() returns int as $body$ select 1; $body$ language sql; /* x; */ select $1";
        assert_eq!(
            pieces(Engine::Postgres, text),
            vec!["create function f() returns int as $body$ select 1; $body$ language sql", "/* x; */ select $1"]
        );
    }

    #[test]
    fn mysql_backticks_hash_comments_and_escapes() {
        let text = "select `a;b`, 'it\\'s;'; # x;\nselect 2";
        assert_eq!(pieces(Engine::MySql, text), vec!["select `a;b`, 'it\\'s;'", "# x;\nselect 2"]);
    }

    #[test]
    fn statement_under_cursor() {
        let text = "select 1;\n\nselect 2;\n";
        let at = |cursor| statement_at(Engine::Postgres, text, cursor).map(|r| &text[r]);
        assert_eq!(at(0), Some("select 1"));
        assert_eq!(at(3), Some("select 1"));
        assert_eq!(at(10), Some("select 1"));
        assert_eq!(at(12), Some("select 2"));
        assert_eq!(at(text.len()), Some("select 2"));
        assert_eq!(statement_at(Engine::Postgres, "  -- only\n", 0), None);
    }
}
