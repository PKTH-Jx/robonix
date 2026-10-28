//! SRDL parsing and static type checking. Runtime binding and evaluation are not implemented.

pub mod ast;
mod lexer;
mod parser;

/// One SRDL error with a source range and a human-readable location.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("{kind} at line {line}, column {column}")]
pub struct Error {
    /// The specific lexical, syntax, or declaration failure.
    pub kind: ErrorKind,
    /// The half-open UTF-8 byte range; an empty range at source end denotes EOF.
    pub span: std::ops::Range<usize>,
    /// The one-based source line.
    pub line: usize,
    /// The one-based character column, rather than a byte or display-width column.
    pub column: usize,
}

/// Failures detected while tokenizing or parsing, before runtime binding.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum ErrorKind {
    #[error("invalid or unterminated token")]
    InvalidToken,
    #[error("expected {expected}, found {found}")]
    Expected {
        /// The required token spelling or grammatical element.
        expected: String,
        /// The escaped source token, or an explicit end-of-input description.
        found: String,
    },
    #[error("duplicate input '{0}'")]
    DuplicateInput(String),
    #[error("duplicate enforce clause")]
    DuplicateEnforce,
    #[error("missing required {0} clause")]
    MissingClause(&'static str),
    #[error("number must be finite")]
    InvalidNumber,
    #[error("invalid string escape or Unicode sequence")]
    InvalidString,
    #[error("maxage must be greater than zero in the default time unit (seconds)")]
    InvalidMaxage,
    #[error("comparisons cannot be chained; combine them with &&")]
    ChainedComparison,
    #[error("expression exceeds the token limit of {0}")]
    ExpressionTooLong(usize),
    #[error("expression exceeds the recursion limit of {0}")]
    ExpressionTooDeep(usize),
}

impl Error {
    fn new(source: &str, span: std::ops::Range<usize>, kind: ErrorKind) -> Self {
        let prefix = &source[..span.start];
        Self {
            kind,
            span,
            line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
            column: prefix
                .rsplit('\n')
                .next()
                .unwrap_or_default()
                .chars()
                .count()
                + 1,
        }
    }
}

/// Parse a complete SRDL source, rejecting invalid or trailing tokens.
pub fn parse(source: &str) -> Result<ast::Program, Error> {
    let tokens = lexer::tokenize_srdl(source)?;
    parser::parse_srdl(lexer::Tokens {
        tok: &tokens,
        source,
    })
}

#[cfg(test)]
mod tests;
