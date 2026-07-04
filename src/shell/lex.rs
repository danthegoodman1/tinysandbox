use crate::shell::{
    ParseError, ParseErrorKind, QuoteKind, RedirectOp, Segment, UnsupportedConstruct, Word,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub(crate) kind: TokenKind,
    pub(crate) position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenKind {
    Word(Word),
    Pipe,
    AndIf,
    OrIf,
    Semi,
    Newline,
    Redirect(RedirectToken),
}

impl TokenKind {
    pub(crate) const fn description(&self) -> &'static str {
        match self {
            Self::Word(_) => "word",
            Self::Pipe => "`|`",
            Self::AndIf => "`&&`",
            Self::OrIf => "`||`",
            Self::Semi => "`;`",
            Self::Newline => "newline",
            Self::Redirect(_) => "redirect",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RedirectToken {
    pub(crate) fd: Option<u32>,
    pub(crate) op: RedirectOp,
    pub(crate) target_fd: Option<u32>,
}

pub(crate) fn lex(input: &str) -> Result<Vec<Token>, ParseError> {
    let mut tokens = Vec::new();
    let mut position = 0;

    while position < input.len() {
        position = skip_blanks(input, position);
        if position >= input.len() {
            break;
        }

        if peek_char(input, position) == Some('#') {
            position = skip_comment(input, position);
            continue;
        }

        if let Some((redirect, next_position)) = scan_redirect(input, position)? {
            tokens.push(Token {
                kind: TokenKind::Redirect(redirect),
                position,
            });
            position = next_position;
            continue;
        }

        match peek_char(input, position).expect("position is in bounds") {
            '\n' => {
                tokens.push(Token {
                    kind: TokenKind::Newline,
                    position,
                });
                position += 1;
            }
            ';' => {
                tokens.push(Token {
                    kind: TokenKind::Semi,
                    position,
                });
                position += 1;
            }
            '|' if input[position..].starts_with("||") => {
                tokens.push(Token {
                    kind: TokenKind::OrIf,
                    position,
                });
                position += 2;
            }
            '|' => {
                tokens.push(Token {
                    kind: TokenKind::Pipe,
                    position,
                });
                position += 1;
            }
            '&' if input[position..].starts_with("&&") => {
                tokens.push(Token {
                    kind: TokenKind::AndIf,
                    position,
                });
                position += 2;
            }
            '&' => {
                return Err(ParseError::new(
                    position,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::Background),
                ));
            }
            '<' if input[position..].starts_with("<<") => {
                return Err(ParseError::new(
                    position,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::Heredoc),
                ));
            }
            '<' | '>' => unreachable!("redirect scanner handles redirection operators"),
            _ => {
                let (word, next_position) = scan_word(input, position)?;
                if let Some(word) = word {
                    tokens.push(Token {
                        kind: TokenKind::Word(word),
                        position,
                    });
                }
                position = next_position;
            }
        }
    }

    Ok(tokens)
}

fn scan_redirect(
    input: &str,
    position: usize,
) -> Result<Option<(RedirectToken, usize)>, ParseError> {
    let mut operator_position = position;
    let mut fd = None;
    let mut scan = position;

    while matches!(peek_char(input, scan), Some(c) if c.is_ascii_digit()) {
        scan += 1;
    }

    if scan > position && matches!(peek_char(input, scan), Some('<' | '>')) {
        fd = Some(parse_fd(input, position, scan)?);
        operator_position = scan;
    } else {
        scan = position;
    }

    let Some(operator) = peek_char(input, scan) else {
        return Ok(None);
    };

    let (op, mut next_position) = match operator {
        '<' => {
            if input[scan..].starts_with("<<") {
                return Err(ParseError::new(
                    operator_position,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::Heredoc),
                ));
            }
            (RedirectOp::Read, scan + 1)
        }
        '>' => {
            if input[scan..].starts_with(">>") {
                (RedirectOp::Append, scan + 2)
            } else {
                (RedirectOp::Write, scan + 1)
            }
        }
        _ => return Ok(None),
    };

    let mut target_fd = None;
    if peek_char(input, next_position) == Some('&') {
        if op != RedirectOp::Write {
            return Err(ParseError::new(
                operator_position,
                ParseErrorKind::Unsupported(UnsupportedConstruct::RedirectFdDup),
            ));
        }

        let fd_start = next_position + 1;
        let mut fd_end = fd_start;
        while matches!(peek_char(input, fd_end), Some(c) if c.is_ascii_digit()) {
            fd_end += 1;
        }

        if fd_end == fd_start {
            return Err(ParseError::new(
                operator_position,
                ParseErrorKind::Unsupported(UnsupportedConstruct::RedirectFdDup),
            ));
        }

        if matches!(peek_char(input, fd_end), Some(ch) if !is_word_break(ch)) {
            return Err(ParseError::new(
                operator_position,
                ParseErrorKind::AmbiguousRedirect,
            ));
        }

        target_fd = Some(parse_fd(input, fd_start, fd_end)?);
        next_position = fd_end;
    }

    Ok(Some((RedirectToken { fd, op, target_fd }, next_position)))
}

fn scan_word(input: &str, position: usize) -> Result<(Option<Word>, usize), ParseError> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut cursor = position;
    let mut had_content = false;
    let mut last_unquoted_char = None;
    let mut has_unquoted_equals = false;

    while let Some(ch) = peek_char(input, cursor) {
        if is_word_break(ch) {
            break;
        }

        match ch {
            '\'' => {
                push_literal(&mut segments, std::mem::take(&mut literal), false);
                had_content = true;
                cursor = scan_single_quoted(input, cursor, &mut segments)?;
                last_unquoted_char = None;
            }
            '"' => {
                push_literal(&mut segments, std::mem::take(&mut literal), false);
                had_content = true;
                cursor = scan_double_quoted(input, cursor, &mut segments)?;
                last_unquoted_char = None;
            }
            '\\' => {
                let (next_position, escaped) = scan_unquoted_escape(input, cursor);
                if let Some(escaped) = escaped {
                    push_literal(&mut segments, std::mem::take(&mut literal), false);
                    push_literal(&mut segments, escaped.to_string(), true);
                    had_content = true;
                    last_unquoted_char = None;
                }
                cursor = next_position;
            }
            '$' => {
                had_content = true;
                cursor = scan_expansion(input, cursor, false, &mut segments, &mut literal)?;
                last_unquoted_char = None;
            }
            '*' | '?' => {
                return Err(ParseError::new(
                    cursor,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::Glob),
                ));
            }
            '[' if has_closing_bracket_before_word_end(input, cursor) => {
                return Err(ParseError::new(
                    cursor,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::Glob),
                ));
            }
            '`' => {
                return Err(ParseError::new(
                    cursor,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::Backticks),
                ));
            }
            '(' | ')' => {
                return Err(ParseError::new(
                    cursor,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::Subshell),
                ));
            }
            '{' | '}' => {
                return Err(ParseError::new(
                    cursor,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::BraceExpansion),
                ));
            }
            '~' if (!had_content && segments.is_empty() && literal.is_empty())
                || last_unquoted_char == Some('=')
                || (last_unquoted_char == Some(':') && has_unquoted_equals) =>
            {
                return Err(ParseError::new(
                    cursor,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::TildeExpansion),
                ));
            }
            _ => {
                had_content = true;
                literal.push(ch);
                cursor += ch.len_utf8();
                has_unquoted_equals |= ch == '=';
                last_unquoted_char = Some(ch);
            }
        }
    }

    push_literal(&mut segments, literal, false);
    if had_content {
        Ok((Some(Word { segments }), cursor))
    } else {
        Ok((None, cursor))
    }
}

fn scan_single_quoted(
    input: &str,
    position: usize,
    segments: &mut Vec<Segment>,
) -> Result<usize, ParseError> {
    let quote_position = position;
    let mut cursor = position + 1;
    let mut literal = String::new();

    while let Some(ch) = peek_char(input, cursor) {
        if ch == '\'' {
            push_literal(segments, literal, true);
            return Ok(cursor + 1);
        }

        literal.push(ch);
        cursor += ch.len_utf8();
    }

    Err(ParseError::new(
        quote_position,
        ParseErrorKind::UnterminatedQuote {
            quote: QuoteKind::Single,
        },
    ))
}

fn scan_double_quoted(
    input: &str,
    position: usize,
    segments: &mut Vec<Segment>,
) -> Result<usize, ParseError> {
    let quote_position = position;
    let mut cursor = position + 1;
    let mut literal = String::new();
    let mut had_content = false;

    while let Some(ch) = peek_char(input, cursor) {
        match ch {
            '"' => {
                if !literal.is_empty() || !had_content {
                    push_literal(segments, literal, true);
                }
                return Ok(cursor + 1);
            }
            '\\' => {
                let literal_len = literal.len();
                cursor = scan_double_quoted_escape(input, cursor, &mut literal)?;
                had_content |= literal.len() != literal_len;
            }
            '$' => {
                had_content = true;
                cursor = scan_expansion(input, cursor, true, segments, &mut literal)?;
            }
            '`' => {
                return Err(ParseError::new(
                    cursor,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::Backticks),
                ));
            }
            _ => {
                had_content = true;
                literal.push(ch);
                cursor += ch.len_utf8();
            }
        }
    }

    Err(ParseError::new(
        quote_position,
        ParseErrorKind::UnterminatedQuote {
            quote: QuoteKind::Double,
        },
    ))
}

fn scan_unquoted_escape(input: &str, position: usize) -> (usize, Option<char>) {
    let next_position = position + 1;
    let Some(next) = peek_char(input, next_position) else {
        return (next_position, None);
    };

    if next == '\n' {
        (next_position + 1, None)
    } else {
        (next_position + next.len_utf8(), Some(next))
    }
}

fn scan_double_quoted_escape(
    input: &str,
    position: usize,
    literal: &mut String,
) -> Result<usize, ParseError> {
    let next_position = position + 1;
    let Some(next) = peek_char(input, next_position) else {
        return Err(ParseError::new(
            position,
            ParseErrorKind::UnterminatedEscape,
        ));
    };

    if next == '\n' {
        return Ok(next_position + 1);
    }

    // Bash only gives backslash special meaning before these characters inside double quotes.
    if matches!(next, '$' | '`' | '"' | '\\') {
        literal.push(next);
    } else {
        literal.push('\\');
        literal.push(next);
    }

    Ok(next_position + next.len_utf8())
}

fn scan_expansion(
    input: &str,
    position: usize,
    quoted: bool,
    segments: &mut Vec<Segment>,
    literal: &mut String,
) -> Result<usize, ParseError> {
    let next_position = position + 1;
    let Some(next) = peek_char(input, next_position) else {
        literal.push('$');
        return Ok(next_position);
    };

    match next {
        '(' => Err(ParseError::new(
            position,
            ParseErrorKind::Unsupported(UnsupportedConstruct::CommandSubstitution),
        )),
        '\'' if !quoted => Err(ParseError::new(
            position,
            ParseErrorKind::Unsupported(UnsupportedConstruct::AnsiCString),
        )),
        '"' if !quoted => Err(ParseError::new(
            position,
            ParseErrorKind::Unsupported(UnsupportedConstruct::LocaleTranslation),
        )),
        '{' => scan_braced_expansion(input, position, quoted, segments, literal),
        '?' => {
            push_literal_before_expansion(segments, literal, quoted);
            segments.push(Segment::Expansion {
                name: "?".to_owned(),
                quoted,
            });
            Ok(next_position + 1)
        }
        ch if is_name_start(ch) => {
            let mut name_end = next_position + ch.len_utf8();
            while matches!(peek_char(input, name_end), Some(c) if is_name_continue(c)) {
                name_end += 1;
            }

            let name = &input[next_position..name_end];
            if name == "_" {
                return Err(ParseError::new(
                    position,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::ParameterExpansion),
                ));
            }

            push_literal_before_expansion(segments, literal, quoted);
            segments.push(Segment::Expansion {
                name: name.to_owned(),
                quoted,
            });
            Ok(name_end)
        }
        ch if ch.is_ascii_digit() || is_rejected_special_parameter(ch) => Err(ParseError::new(
            position,
            ParseErrorKind::Unsupported(UnsupportedConstruct::ParameterExpansion),
        )),
        _ => {
            literal.push('$');
            Ok(next_position)
        }
    }
}

fn scan_braced_expansion(
    input: &str,
    position: usize,
    quoted: bool,
    segments: &mut Vec<Segment>,
    literal: &mut String,
) -> Result<usize, ParseError> {
    let name_start = position + 2;
    let Some(first) = peek_char(input, name_start) else {
        return Err(ParseError::new(
            position,
            ParseErrorKind::UnterminatedBracedExpansion,
        ));
    };

    if first == '?' {
        let close_position = name_start + 1;
        if peek_char(input, close_position) != Some('}') {
            return Err(ParseError::new(
                position,
                ParseErrorKind::InvalidParameterExpansion,
            ));
        }

        push_literal_before_expansion(segments, literal, quoted);
        segments.push(Segment::Expansion {
            name: "?".to_owned(),
            quoted,
        });
        return Ok(close_position + 1);
    }

    if first.is_ascii_digit() || is_rejected_special_parameter(first) {
        return Err(ParseError::new(
            position,
            ParseErrorKind::Unsupported(UnsupportedConstruct::ParameterExpansion),
        ));
    }

    if !is_name_start(first) {
        return Err(ParseError::new(
            position,
            ParseErrorKind::InvalidParameterExpansion,
        ));
    }

    let mut name_end = name_start + first.len_utf8();
    while matches!(peek_char(input, name_end), Some(c) if is_name_continue(c)) {
        name_end += 1;
    }

    match peek_char(input, name_end) {
        Some('}') => {
            let name = &input[name_start..name_end];
            if name == "_" {
                return Err(ParseError::new(
                    position,
                    ParseErrorKind::Unsupported(UnsupportedConstruct::ParameterExpansion),
                ));
            }

            push_literal_before_expansion(segments, literal, quoted);
            segments.push(Segment::Expansion {
                name: name.to_owned(),
                quoted,
            });
            Ok(name_end + 1)
        }
        None => Err(ParseError::new(
            position,
            ParseErrorKind::UnterminatedBracedExpansion,
        )),
        Some(_) => Err(ParseError::new(
            position,
            ParseErrorKind::InvalidParameterExpansion,
        )),
    }
}

fn skip_blanks(input: &str, mut position: usize) -> usize {
    while matches!(peek_char(input, position), Some(' ' | '\t')) {
        position += peek_char(input, position)
            .expect("position checked by matches")
            .len_utf8();
    }

    position
}

fn skip_comment(input: &str, mut position: usize) -> usize {
    while matches!(peek_char(input, position), Some(ch) if ch != '\n') {
        position += peek_char(input, position)
            .expect("position checked by matches")
            .len_utf8();
    }

    position
}

fn push_literal(segments: &mut Vec<Segment>, literal: String, quoted: bool) {
    if literal.is_empty() && !quoted {
        return;
    }

    if !literal.is_empty()
        && let Some(Segment::Literal {
            value,
            quoted: existing_quoted,
        }) = segments.last_mut()
        && *existing_quoted == quoted
    {
        value.push_str(&literal);
    } else {
        segments.push(Segment::Literal {
            value: literal,
            quoted,
        });
    }
}

fn push_literal_before_expansion(segments: &mut Vec<Segment>, literal: &mut String, quoted: bool) {
    if !literal.is_empty() {
        push_literal(segments, std::mem::take(literal), quoted);
    }
}

fn parse_fd(input: &str, start: usize, end: usize) -> Result<u32, ParseError> {
    input[start..end]
        .parse()
        .map_err(|_| ParseError::new(start, ParseErrorKind::InvalidFileDescriptor))
}

fn has_closing_bracket_before_word_end(input: &str, position: usize) -> bool {
    let mut cursor = position + 1;

    while let Some(ch) = peek_char(input, cursor) {
        if ch == ']' {
            return true;
        }
        if is_word_break(ch) {
            return false;
        }
        cursor += ch.len_utf8();
    }

    false
}

fn is_name_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_'
}

fn is_name_continue(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn is_rejected_special_parameter(ch: char) -> bool {
    matches!(ch, '$' | '!' | '#' | '*' | '@' | '-')
}

fn is_word_break(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '|' | '&' | ';' | '<' | '>')
}

fn peek_char(input: &str, position: usize) -> Option<char> {
    input.get(position..)?.chars().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_separates_commands_and_comments_end_at_newline() {
        let tokens = lex("echo a # comment\necho b").expect("lex should succeed");

        assert!(matches!(tokens[0].kind, TokenKind::Word(_)));
        assert!(matches!(tokens[1].kind, TokenKind::Word(_)));
        assert_eq!(tokens[2].kind, TokenKind::Newline);
        assert!(matches!(tokens[3].kind, TokenKind::Word(_)));
        assert!(matches!(tokens[4].kind, TokenKind::Word(_)));
    }

    #[test]
    fn quoted_empty_is_preserved_before_bare_expansion() {
        let tokens = lex("\"\"$X $X").expect("lex should succeed");
        let [
            Token {
                kind: TokenKind::Word(first),
                ..
            },
            Token {
                kind: TokenKind::Word(second),
                ..
            },
        ] = tokens.as_slice()
        else {
            panic!("unexpected tokens: {tokens:?}");
        };

        assert_eq!(
            first.segments,
            vec![
                Segment::Literal {
                    value: String::new(),
                    quoted: true,
                },
                Segment::Expansion {
                    name: "X".to_owned(),
                    quoted: false,
                },
            ]
        );
        assert_eq!(
            second.segments,
            vec![Segment::Expansion {
                name: "X".to_owned(),
                quoted: false,
            }]
        );
    }

    #[test]
    fn line_continuation_alone_does_not_create_an_empty_word() {
        let tokens = lex("echo a \\\n b").expect("lex should succeed");
        let words = tokens
            .iter()
            .filter(|token| matches!(token.kind, TokenKind::Word(_)))
            .count();

        assert_eq!(words, 3);
    }

    #[test]
    fn trailing_unquoted_backslash_at_eof_is_dropped() {
        let tokens = lex("echo \\").expect("lex should succeed");
        let words = tokens
            .iter()
            .filter(|token| matches!(token.kind, TokenKind::Word(_)))
            .count();

        assert_eq!(words, 1);
    }

    #[test]
    fn carriage_return_stays_inside_words() {
        let tokens = lex("echo a\rb").expect("lex should succeed");
        let TokenKind::Word(word) = &tokens[1].kind else {
            panic!("unexpected tokens: {tokens:?}");
        };

        assert_eq!(
            word.segments,
            vec![Segment::Literal {
                value: "a\rb".to_owned(),
                quoted: false,
            }]
        );
    }

    #[test]
    fn fd_dup_rejects_trailing_word_characters() {
        let error = lex("cmd 2>&1x").expect_err("ambiguous redirect should fail");

        assert_eq!(error.position, 5);
        assert_eq!(error.kind, ParseErrorKind::AmbiguousRedirect);
    }
}
