use crate::shell::lex::{RedirectToken, Token, TokenKind, lex};
use crate::shell::{
    AndOr, AndOrList, AndOrOp, Assignment, Command, ParseError, ParseErrorKind, Pipeline, Program,
    Redirect, RedirectTarget, Segment, SimpleCommand, UnsupportedConstruct, Word,
};

pub fn parse(input: &str) -> Result<Program, ParseError> {
    Parser::new(input, lex(input)?).parse_program()
}

struct Parser<'a> {
    input: &'a str,
    tokens: Vec<Token>,
    cursor: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, tokens: Vec<Token>) -> Self {
        Self {
            input,
            tokens,
            cursor: 0,
        }
    }

    fn parse_program(mut self) -> Result<Program, ParseError> {
        let mut lists = Vec::new();
        self.consume_newlines();

        while !self.is_eof() {
            lists.push(self.parse_and_or_list()?);

            if self.consume_if(|kind| matches!(kind, TokenKind::Semi)) {
                self.consume_newlines();
                if self.is_eof() {
                    break;
                }
            } else if self.consume_if(|kind| matches!(kind, TokenKind::Newline)) {
                self.consume_newlines();
            } else if !self.is_eof() {
                return self.unexpected("`;`, newline, or end of input");
            }
        }

        Ok(Program { lists })
    }

    fn parse_and_or_list(&mut self) -> Result<AndOrList, ParseError> {
        let first = self.parse_pipeline("command")?;
        let mut rest = Vec::new();

        while let Some(op) = self.consume_and_or() {
            self.consume_newlines();
            let pipeline = self.parse_pipeline(match op {
                AndOrOp::And => "command after `&&`",
                AndOrOp::Or => "command after `||`",
            })?;
            rest.push(AndOr { op, pipeline });
        }

        Ok(AndOrList { first, rest })
    }

    fn parse_pipeline(&mut self, expected: &'static str) -> Result<Pipeline, ParseError> {
        let mut commands = vec![self.parse_command(expected)?];

        while self.consume_if(|kind| matches!(kind, TokenKind::Pipe)) {
            self.consume_newlines();
            commands.push(self.parse_command("command after pipe")?);
        }

        Ok(Pipeline { commands })
    }

    fn parse_command(&mut self, expected: &'static str) -> Result<Command, ParseError> {
        if !self.starts_command() {
            return self.unexpected(expected);
        }

        if let Some(token) = self.peek()
            && is_pipeline_negation_word(&token.kind)
        {
            return Err(ParseError::new(
                token.position,
                ParseErrorKind::Unsupported(UnsupportedConstruct::PipelineNegation),
            ));
        }

        let mut assignments = Vec::new();
        let mut words = Vec::new();
        let mut redirects = Vec::new();
        let mut saw_element = false;

        while let Some(token) = self.peek() {
            match &token.kind {
                TokenKind::Word(word) => {
                    saw_element = true;
                    let word = word.clone();
                    let position = token.position;
                    self.cursor += 1;

                    if words.is_empty() {
                        if let Some(assignment) = assignment_from_word(self.input, &word, position)?
                        {
                            assignments.push(assignment);
                        } else {
                            words.push(word);
                        }
                    } else {
                        words.push(word);
                    }
                }
                TokenKind::Redirect(redirect) => {
                    saw_element = true;
                    let redirect = *redirect;
                    self.cursor += 1;
                    redirects.push(self.parse_redirect(redirect)?);
                }
                _ => break,
            }
        }

        if !saw_element {
            return self.unexpected(expected);
        }

        Ok(Command::Simple(SimpleCommand {
            assignments,
            words,
            redirects,
        }))
    }

    fn parse_redirect(&mut self, redirect: RedirectToken) -> Result<Redirect, ParseError> {
        let target = if let Some(fd) = redirect.target_fd {
            RedirectTarget::Fd(fd)
        } else {
            match self.peek() {
                Some(Token {
                    kind: TokenKind::Word(word),
                    ..
                }) => {
                    let word = word.clone();
                    self.cursor += 1;
                    RedirectTarget::Word(word)
                }
                _ => {
                    return Err(ParseError::new(
                        self.previous_position(),
                        ParseErrorKind::MissingRedirectTarget,
                    ));
                }
            }
        };

        Ok(Redirect {
            fd: redirect.fd,
            op: redirect.op,
            target,
        })
    }

    fn starts_command(&self) -> bool {
        matches!(
            self.peek().map(|token| &token.kind),
            Some(TokenKind::Word(_) | TokenKind::Redirect(_))
        )
    }

    fn consume_and_or(&mut self) -> Option<AndOrOp> {
        match self.peek().map(|token| &token.kind) {
            Some(TokenKind::AndIf) => {
                self.cursor += 1;
                Some(AndOrOp::And)
            }
            Some(TokenKind::OrIf) => {
                self.cursor += 1;
                Some(AndOrOp::Or)
            }
            _ => None,
        }
    }

    fn consume_if(&mut self, matches: impl FnOnce(&TokenKind) -> bool) -> bool {
        if self.peek().is_some_and(|token| matches(&token.kind)) {
            self.cursor += 1;
            true
        } else {
            false
        }
    }

    fn consume_newlines(&mut self) {
        while self.consume_if(|kind| matches!(kind, TokenKind::Newline)) {}
    }

    fn unexpected<T>(&self, expected: &'static str) -> Result<T, ParseError> {
        if let Some(token) = self.peek() {
            Err(ParseError::new(
                token.position,
                ParseErrorKind::UnexpectedToken {
                    expected,
                    found: token.kind.description(),
                },
            ))
        } else {
            Err(ParseError::new(
                self.input.len(),
                ParseErrorKind::UnexpectedEof { expected },
            ))
        }
    }

    fn previous_position(&self) -> usize {
        self.tokens
            .get(self.cursor.saturating_sub(1))
            .map_or(self.input.len(), |token| token.position)
    }

    fn is_eof(&self) -> bool {
        self.cursor >= self.tokens.len()
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.cursor)
    }
}

fn assignment_from_word(
    input: &str,
    word: &Word,
    position: usize,
) -> Result<Option<Assignment>, ParseError> {
    let Some(Segment::Literal {
        value,
        quoted: false,
    }) = word.segments.first()
    else {
        return Ok(None);
    };

    if let Some(append_index) = value.find("+=")
        && is_assignment_name(&value[..append_index])
    {
        return Err(ParseError::new(
            source_position_for_unquoted_literal_byte(input, position, append_index),
            ParseErrorKind::Unsupported(UnsupportedConstruct::AppendAssignment),
        ));
    }

    let Some(equals_index) = value.find('=') else {
        return Ok(None);
    };

    let name = &value[..equals_index];
    if !is_assignment_name(name) {
        return Ok(None);
    }

    let mut value_segments = Vec::new();
    let first_value = &value[equals_index + 1..];
    if !first_value.is_empty() {
        value_segments.push(Segment::Literal {
            value: first_value.to_owned(),
            quoted: false,
        });
    }
    value_segments.extend(word.segments.iter().skip(1).cloned());

    Ok(Some(Assignment {
        name: name.to_owned(),
        value: Word {
            segments: value_segments,
        },
    }))
}

fn source_position_for_unquoted_literal_byte(
    input: &str,
    position: usize,
    target_byte: usize,
) -> usize {
    let mut cursor = position;
    let mut literal_byte = 0;

    while let Some(ch) = input.get(cursor..).and_then(|rest| rest.chars().next()) {
        if literal_byte == target_byte || is_source_word_break(ch) {
            return cursor;
        }

        let next_position = cursor + ch.len_utf8();
        if ch == '\\'
            && input
                .get(next_position..)
                .is_some_and(|rest| rest.starts_with('\n'))
        {
            cursor = next_position + 1;
        } else {
            literal_byte += ch.len_utf8();
            cursor = next_position;
        }
    }

    cursor
}

fn is_assignment_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn is_source_word_break(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '|' | '&' | ';' | '<' | '>')
}

fn is_pipeline_negation_word(kind: &TokenKind) -> bool {
    let TokenKind::Word(word) = kind else {
        return false;
    };

    matches!(
        word.segments.as_slice(),
        [Segment::Literal {
            value,
            quoted: false
        }] if value == "!"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_assignment_is_a_simple_command_without_words() {
        let program = parse("VAR=x").expect("parse should succeed");
        let command = only_command(&program);
        let Command::Simple(command) = command;

        assert_eq!(command.assignments.len(), 1);
        assert_eq!(command.assignments[0].name, "VAR");
        assert!(command.words.is_empty());
    }

    #[test]
    fn quoted_assignment_like_words_are_not_assignments() {
        for input in ["'a=b'", "\"a\"=b", "a\\=b"] {
            let program = parse(input).expect("parse should succeed");
            let command = only_command(&program);
            let Command::Simple(command) = command;

            assert!(command.assignments.is_empty(), "input: {input}");
            assert_eq!(command.words.len(), 1, "input: {input}");
        }
    }

    #[test]
    fn append_assignment_is_rejected_in_assignment_position() {
        let error = parse("VAR+=x").expect_err("append assignment should fail");

        assert_eq!(error.position, 3);
        assert_eq!(
            error.kind,
            ParseErrorKind::Unsupported(UnsupportedConstruct::AppendAssignment)
        );
    }

    #[test]
    fn blank_newlines_do_not_create_empty_commands() {
        let program = parse("\n\necho a\n\n").expect("parse should succeed");

        assert_eq!(program.lists.len(), 1);
    }

    fn only_command(program: &Program) -> &Command {
        let [list] = program.lists.as_slice() else {
            panic!("expected one list: {program:?}");
        };
        let [command] = list.first.commands.as_slice() else {
            panic!("expected one command: {program:?}");
        };

        command
    }
}
