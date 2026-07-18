use std::ops::Range;

#[derive(Debug)]
enum SqlLexState {
    Normal,
    SingleQuoted,
    DoubleQuoted,
    BacktickQuoted,
    LineComment,
    BlockComment(usize),
    DollarQuoted(Vec<u8>),
}

/// Splits a SQL document into executable statement byte ranges.
///
/// This is deliberately a dialect-neutral lexical parser. It recognizes the
/// quoting and comment forms used by the built-in JDBC targets without trying
/// to validate a statement against one database's grammar.
pub fn split_sql_statements(sql: &str) -> Vec<Range<usize>> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut statement_start = 0;
    let mut statement_has_code = false;
    let mut state = SqlLexState::Normal;
    let mut index = 0;

    while index < bytes.len() {
        match &mut state {
            SqlLexState::Normal => match bytes[index] {
                b'-' if bytes.get(index + 1) == Some(&b'-') => {
                    state = SqlLexState::LineComment;
                    index += 2;
                }
                b'/' if bytes.get(index + 1) == Some(&b'*') => {
                    state = SqlLexState::BlockComment(1);
                    index += 2;
                }
                b'\'' => {
                    statement_has_code = true;
                    state = SqlLexState::SingleQuoted;
                    index += 1;
                }
                b'"' => {
                    statement_has_code = true;
                    state = SqlLexState::DoubleQuoted;
                    index += 1;
                }
                b'`' => {
                    statement_has_code = true;
                    state = SqlLexState::BacktickQuoted;
                    index += 1;
                }
                b'$' => {
                    if let Some(tag) = dollar_quote_tag(bytes, index) {
                        statement_has_code = true;
                        index += tag.len();
                        state = SqlLexState::DollarQuoted(tag);
                    } else {
                        statement_has_code = true;
                        index += 1;
                    }
                }
                b';' => {
                    if statement_has_code
                        && let Some(range) = trim_range(sql, statement_start..index + 1)
                    {
                        statements.push(range);
                    }
                    statement_start = index + 1;
                    statement_has_code = false;
                    index += 1;
                }
                byte if byte.is_ascii_whitespace() => index += 1,
                _ => {
                    statement_has_code = true;
                    index += 1;
                }
            },
            SqlLexState::SingleQuoted => {
                if bytes[index] == b'\\' {
                    index = (index + 2).min(bytes.len());
                } else if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                    } else {
                        state = SqlLexState::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            SqlLexState::DoubleQuoted => {
                if bytes[index] == b'\\' {
                    index = (index + 2).min(bytes.len());
                } else if bytes[index] == b'"' {
                    if bytes.get(index + 1) == Some(&b'"') {
                        index += 2;
                    } else {
                        state = SqlLexState::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            SqlLexState::BacktickQuoted => {
                if bytes[index] == b'\\' {
                    index = (index + 2).min(bytes.len());
                } else if bytes[index] == b'`' {
                    if bytes.get(index + 1) == Some(&b'`') {
                        index += 2;
                    } else {
                        state = SqlLexState::Normal;
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            SqlLexState::LineComment => {
                if matches!(bytes[index], b'\n' | b'\r') {
                    state = SqlLexState::Normal;
                }
                index += 1;
            }
            SqlLexState::BlockComment(depth) => {
                if bytes[index] == b'/' && bytes.get(index + 1) == Some(&b'*') {
                    *depth += 1;
                    index += 2;
                } else if bytes[index] == b'*' && bytes.get(index + 1) == Some(&b'/') {
                    *depth -= 1;
                    index += 2;
                    if *depth == 0 {
                        state = SqlLexState::Normal;
                    }
                } else {
                    index += 1;
                }
            }
            SqlLexState::DollarQuoted(tag) => {
                if bytes[index..].starts_with(tag) {
                    index += tag.len();
                    state = SqlLexState::Normal;
                } else {
                    index += 1;
                }
            }
        }
    }

    if statement_has_code && let Some(range) = trim_range(sql, statement_start..bytes.len()) {
        statements.push(range);
    }

    statements
}

/// Finds the parsed statement at a cursor byte offset, or the next statement
/// when the cursor is in whitespace between statements.
pub fn sql_statement_at_offset(sql: &str, offset: usize) -> Option<Range<usize>> {
    let offset = offset.min(sql.len());
    let statements = split_sql_statements(sql);
    statements
        .iter()
        .find(|range| range.start <= offset && offset <= range.end)
        .or_else(|| statements.iter().find(|range| range.start > offset))
        .or_else(|| statements.last())
        .cloned()
}

fn dollar_quote_tag(bytes: &[u8], start: usize) -> Option<Vec<u8>> {
    let mut end = start + 1;
    while let Some(byte) = bytes.get(end)
        && (byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        end += 1;
    }
    (bytes.get(end) == Some(&b'$')).then(|| bytes[start..=end].to_vec())
}

fn trim_range(sql: &str, range: Range<usize>) -> Option<Range<usize>> {
    let text = &sql[range.clone()];
    let start = text
        .char_indices()
        .find(|(_, character)| !character.is_whitespace())
        .map(|(index, _)| range.start + index)?;
    let end = text
        .char_indices()
        .rev()
        .find(|(_, character)| !character.is_whitespace())
        .map(|(index, character)| range.start + index + character.len_utf8())?;
    Some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statements(sql: &str) -> Vec<&str> {
        split_sql_statements(sql)
            .into_iter()
            .map(|range| &sql[range])
            .collect()
    }

    #[test]
    fn splits_multiple_statements_and_skips_comment_only_fragments() {
        let sql = "-- setup; ignored\nselect 1; /* gap; */ select 2; -- trailing";
        assert_eq!(
            statements(sql),
            vec!["-- setup; ignored\nselect 1;", "/* gap; */ select 2;"]
        );
    }

    #[test]
    fn preserves_semicolons_inside_supported_quoted_forms() {
        let sql = "select ';', \"a;b\", `c;d`; select $$e;f$$, $tag$g;h$tag$;";
        assert_eq!(
            statements(sql),
            vec![
                "select ';', \"a;b\", `c;d`;",
                "select $$e;f$$, $tag$g;h$tag$;"
            ]
        );
    }

    #[test]
    fn supports_escaped_quotes_and_nested_block_comments() {
        let sql = "select 'it''s; ok'; /* outer; /* inner; */ done; */ select 2";
        assert_eq!(
            statements(sql),
            vec![
                "select 'it''s; ok';",
                "/* outer; /* inner; */ done; */ select 2"
            ]
        );
    }

    #[test]
    fn finds_current_or_next_statement_at_cursor() {
        let sql = "select 1;\n\nselect 2;";
        assert_eq!(&sql[sql_statement_at_offset(sql, 3).unwrap()], "select 1;");
        assert_eq!(
            &sql[sql_statement_at_offset(sql, "select 1;".len()).unwrap()],
            "select 1;"
        );
        assert_eq!(&sql[sql_statement_at_offset(sql, 11).unwrap()], "select 2;");
        assert_eq!(
            &sql[sql_statement_at_offset(sql, sql.len()).unwrap()],
            "select 2;"
        );
    }
}
