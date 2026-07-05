mod lex;
mod parse;

use std::error::Error;
use std::fmt;

pub use parse::parse;

pub type ParseResult<T> = Result<T, ParseError>;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Program {
    pub lists: Vec<AndOrList>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndOrList {
    pub first: Pipeline,
    pub rest: Vec<AndOr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndOr {
    pub op: AndOrOp,
    pub pipeline: Pipeline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AndOrOp {
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    pub commands: Vec<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Simple(SimpleCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleCommand {
    pub assignments: Vec<Assignment>,
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub name: String,
    pub value: Word,
}

/// A shell word before field splitting.
///
/// Bash field-splits unquoted expansion results after parsing. Tinysandbox keeps
/// that information here so the executor can apply splitting with the final
/// environment instead of flattening words during parsing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Literal { value: String, quoted: bool },
    Expansion { name: String, quoted: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    pub fd: Option<u32>,
    pub op: RedirectOp,
    pub target: RedirectTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectOp {
    Read,
    Write,
    Append,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectTarget {
    Word(Word),
    Fd(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub position: usize,
    pub kind: ParseErrorKind,
}

impl ParseError {
    pub(crate) const fn new(position: usize, kind: ParseErrorKind) -> Self {
        Self { position, kind }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    UnexpectedEof {
        expected: &'static str,
    },
    UnexpectedToken {
        expected: &'static str,
        found: &'static str,
    },
    MissingRedirectTarget,
    UnterminatedQuote {
        quote: QuoteKind,
    },
    UnterminatedEscape,
    UnterminatedBracedExpansion,
    InvalidParameterExpansion,
    InvalidFileDescriptor,
    AmbiguousRedirect,
    Unsupported(UnsupportedConstruct),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    Single,
    Double,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnsupportedConstruct {
    Glob,
    CommandSubstitution,
    Backticks,
    Background,
    Subshell,
    BraceExpansion,
    Heredoc,
    TildeExpansion,
    ParameterExpansion,
    RedirectFdDup,
    AnsiCString,
    LocaleTranslation,
    AppendAssignment,
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
