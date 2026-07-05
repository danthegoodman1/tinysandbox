//! Bash-compatible lexer and parser for the supported shell subset.

mod lex;
mod parse;

use std::error::Error;
use std::fmt;

pub use parse::parse;

/// Result type returned by shell parsing.
pub type ParseResult<T> = Result<T, ParseError>;

/// Parsed shell program.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Program {
    /// Top-level `&&` / `||` lists separated by `;` or newlines.
    pub lists: Vec<AndOrList>,
}

/// A pipeline followed by zero or more conditional pipelines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndOrList {
    /// First pipeline in the list.
    pub first: Pipeline,
    /// Remaining conditional pipelines.
    pub rest: Vec<AndOr>,
}

/// Conditional pipeline joined by `&&` or `||`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndOr {
    /// Conditional operator.
    pub op: AndOrOp,
    /// Pipeline executed when the operator condition passes.
    pub pipeline: Pipeline,
}

/// Conditional list operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndOrOp {
    /// `&&`.
    And,
    /// `||`.
    Or,
}

/// Pipeline of one or more commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    /// Commands connected left-to-right by pipes.
    pub commands: Vec<Command>,
}

/// Parsed command node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Simple command with assignments, words, and redirects.
    Simple(SimpleCommand),
}

/// Simple shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    /// Assignment prefixes before command words.
    pub assignments: Vec<Assignment>,
    /// Command name and arguments after parsing.
    pub words: Vec<Word>,
    /// Redirects attached to this command.
    pub redirects: Vec<Redirect>,
}

/// Assignment prefix such as `NAME=value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// Variable name.
    pub name: String,
    /// Parsed assignment value.
    pub value: Word,
}

/// A shell word before field splitting.
///
/// Bash field-splits unquoted expansion results after parsing. Tinysandbox keeps
/// that information here so the executor can apply splitting with the final
/// environment instead of flattening words during parsing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    /// Literal and expansion segments that make up the word.
    pub segments: Vec<Segment>,
}

/// Segment of a parsed shell word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    /// Literal text with whether it came from quoted syntax.
    Literal {
        /// Literal bytes decoded as UTF-8 text.
        value: String,
        /// Whether the segment was quoted.
        quoted: bool,
    },
    /// Parameter expansion with whether it came from quoted syntax.
    Expansion {
        /// Variable name, or `?` for last-status expansion.
        name: String,
        /// Whether the expansion was quoted.
        quoted: bool,
    },
}

/// Shell redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    /// Optional explicit file descriptor.
    pub fd: Option<u32>,
    /// Redirect operation.
    pub op: RedirectOp,
    /// Redirect target.
    pub target: RedirectTarget,
}

/// Redirect operation kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOp {
    /// Input redirect, `<`.
    Read,
    /// Output overwrite redirect, `>`.
    Write,
    /// Output append redirect, `>>`.
    Append,
}

/// Target of a redirect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectTarget {
    /// Path-like shell word target.
    Word(Word),
    /// File descriptor duplication target.
    Fd(u32),
}

/// Structured parse error with byte position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// Byte position where the error was detected.
    pub position: usize,
    /// Specific error kind.
    pub kind: ParseErrorKind,
}

impl ParseError {
    pub(crate) const fn new(position: usize, kind: ParseErrorKind) -> Self {
        Self { position, kind }
    }
}

/// Specific parse error reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// End of input before an expected token.
    UnexpectedEof {
        /// Human-readable expected token description.
        expected: &'static str,
    },
    /// Token did not match the parser expectation.
    UnexpectedToken {
        /// Human-readable expected token description.
        expected: &'static str,
        /// Human-readable found token description.
        found: &'static str,
    },
    /// Redirect operator was not followed by a target.
    MissingRedirectTarget,
    /// Quoted string was not closed.
    UnterminatedQuote {
        /// Quote kind that was left open.
        quote: QuoteKind,
    },
    /// Backslash escape ended at EOF.
    UnterminatedEscape,
    /// `${...` expansion was not closed.
    UnterminatedBracedExpansion,
    /// Parameter expansion syntax is invalid or outside the supported subset.
    InvalidParameterExpansion,
    /// File descriptor syntax is invalid.
    InvalidFileDescriptor,
    /// Redirect target expanded to an ambiguous value.
    AmbiguousRedirect,
    /// Syntax is recognized but intentionally unsupported.
    Unsupported(UnsupportedConstruct),
}

/// Quote kind used in parse errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    /// Single quotes.
    Single,
    /// Double quotes.
    Double,
}

/// Unsupported shell construct detected by the parser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedConstruct {
    /// Glob pattern syntax.
    Glob,
    /// `$(...)` command substitution.
    CommandSubstitution,
    /// Backtick command substitution.
    Backticks,
    /// Background execution with `&`.
    Background,
    /// Subshell syntax.
    Subshell,
    /// Brace expansion.
    BraceExpansion,
    /// Heredoc redirect.
    Heredoc,
    /// Tilde expansion.
    TildeExpansion,
    /// Unsupported parameter expansion form.
    ParameterExpansion,
    /// Unsupported file descriptor duplication form.
    RedirectFdDup,
    /// ANSI-C `$'...'` quoting.
    AnsiCString,
    /// Locale translation `$"..."` quoting.
    LocaleTranslation,
    /// Append assignment syntax such as `A+=b`.
    AppendAssignment,
    /// Pipeline negation with `!`.
    PipelineNegation,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "parse error at byte {}: ", self.position)?;

        match &self.kind {
            ParseErrorKind::UnexpectedEof { expected } => {
                write!(f, "expected {expected}, found end of input")
            }
            ParseErrorKind::UnexpectedToken { expected, found } => {
                write!(f, "expected {expected}, found {found}")
            }
            ParseErrorKind::MissingRedirectTarget => write!(f, "missing redirect target"),
            ParseErrorKind::UnterminatedQuote { quote } => match quote {
                QuoteKind::Single => write!(f, "unterminated single quote"),
                QuoteKind::Double => write!(f, "unterminated double quote"),
            },
            ParseErrorKind::UnterminatedEscape => write!(f, "unterminated escape"),
            ParseErrorKind::UnterminatedBracedExpansion => {
                write!(f, "unterminated braced parameter expansion")
            }
            ParseErrorKind::InvalidParameterExpansion => {
                write!(f, "invalid or unsupported parameter expansion")
            }
            ParseErrorKind::InvalidFileDescriptor => write!(f, "invalid file descriptor"),
            ParseErrorKind::AmbiguousRedirect => write!(f, "ambiguous redirect"),
            ParseErrorKind::Unsupported(construct) => write!(f, "unsupported {construct}"),
        }
    }
}

impl Error for ParseError {}

impl fmt::Display for UnsupportedConstruct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::Glob => "glob pattern",
            Self::CommandSubstitution => "command substitution",
            Self::Backticks => "backtick command substitution",
            Self::Background => "background execution",
            Self::Subshell => "subshell syntax",
            Self::BraceExpansion => "brace expansion",
            Self::Heredoc => "heredoc",
            Self::TildeExpansion => "tilde expansion",
            Self::ParameterExpansion => "parameter expansion",
            Self::RedirectFdDup => "redirect fd duplication",
            Self::AnsiCString => "ANSI-C quoting",
            Self::LocaleTranslation => "locale translation quoting",
            Self::AppendAssignment => "append assignment",
            Self::PipelineNegation => "pipeline negation",
        };

        f.write_str(name)
    }
}

#[cfg(test)]
mod tests {
    use super::{ParseError, ParseErrorKind, QuoteKind, UnsupportedConstruct as Unsupported};

    #[test]
    fn parse_error_display_messages_are_exact() {
        // These strings are part of the public parser diagnostics surfaced by
        // `Sandbox::exec`, so each variant gets pinned directly.
        let cases = [
            (
                ParseErrorKind::UnexpectedEof {
                    expected: "command",
                },
                "parse error at byte 7: expected command, found end of input",
            ),
            (
                ParseErrorKind::UnexpectedToken {
                    expected: "word",
                    found: "pipe",
                },
                "parse error at byte 7: expected word, found pipe",
            ),
            (
                ParseErrorKind::MissingRedirectTarget,
                "parse error at byte 7: missing redirect target",
            ),
            (
                ParseErrorKind::UnterminatedQuote {
                    quote: QuoteKind::Single,
                },
                "parse error at byte 7: unterminated single quote",
            ),
            (
                ParseErrorKind::UnterminatedQuote {
                    quote: QuoteKind::Double,
                },
                "parse error at byte 7: unterminated double quote",
            ),
            (
                ParseErrorKind::UnterminatedEscape,
                "parse error at byte 7: unterminated escape",
            ),
            (
                ParseErrorKind::UnterminatedBracedExpansion,
                "parse error at byte 7: unterminated braced parameter expansion",
            ),
            (
                ParseErrorKind::InvalidParameterExpansion,
                "parse error at byte 7: invalid or unsupported parameter expansion",
            ),
            (
                ParseErrorKind::InvalidFileDescriptor,
                "parse error at byte 7: invalid file descriptor",
            ),
            (
                ParseErrorKind::AmbiguousRedirect,
                "parse error at byte 7: ambiguous redirect",
            ),
        ];

        for (kind, expected) in cases {
            assert_eq!(ParseError::new(7, kind).to_string(), expected);
        }
    }

    #[test]
    fn unsupported_construct_display_messages_are_exact() {
        let cases = [
            (Unsupported::Glob, "glob pattern"),
            (Unsupported::CommandSubstitution, "command substitution"),
            (Unsupported::Backticks, "backtick command substitution"),
            (Unsupported::Background, "background execution"),
            (Unsupported::Subshell, "subshell syntax"),
            (Unsupported::BraceExpansion, "brace expansion"),
            (Unsupported::Heredoc, "heredoc"),
            (Unsupported::TildeExpansion, "tilde expansion"),
            (Unsupported::ParameterExpansion, "parameter expansion"),
            (Unsupported::RedirectFdDup, "redirect fd duplication"),
            (Unsupported::AnsiCString, "ANSI-C quoting"),
            (Unsupported::LocaleTranslation, "locale translation quoting"),
            (Unsupported::AppendAssignment, "append assignment"),
            (Unsupported::PipelineNegation, "pipeline negation"),
        ];

        for (construct, expected) in cases {
            assert_eq!(construct.to_string(), expected);
            assert_eq!(
                ParseError::new(3, ParseErrorKind::Unsupported(construct)).to_string(),
                format!("parse error at byte 3: unsupported {expected}")
            );
        }
    }
}
