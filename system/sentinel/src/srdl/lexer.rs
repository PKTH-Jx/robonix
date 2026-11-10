use super::{Error, ErrorKind};
use logos::{Lexer, Logos};
use nom::Input;
use std::iter::Enumerate;
use std::ops::Range;

#[derive(Logos, Debug, PartialEq, Eq, Clone, Copy)]
pub enum TokenKind {
    // Skip
    #[regex(r"[ \t\r\n\f]+", logos::skip)]
    Whitespace,
    #[regex(r"#[^\n\f]*", logos::skip)]
    Comment,

    // Identifier
    #[regex(r#"[_a-zA-Z][_a-zA-Z0-9]*"#)]
    Identifier,

    // Literals
    // Escape spelling is checked here; Unicode validity is checked during parsing.
    #[regex(r#""([^"\\\x00-\x1f]|\\["\\/bfnrt]|\\u[0-9a-fA-F]{4})*""#)]
    StringLiteral,
    // A leading minus is a separate token for unary expressions and signed filter literals.
    // Integer, decimal, and exponent forms use one format; leading zeros are decimal.
    #[regex(r"[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?")]
    NumberLiteral,
    #[token("true")]
    True,
    #[token("false")]
    False,

    // Rule definition
    #[token("rule")]
    Rule,

    // Clauses
    #[token("input")]
    Input,
    #[token("context")]
    Context,
    #[token("trigger")]
    Trigger,
    #[token("require")]
    Require,
    #[token("ensure")]
    Ensure,
    #[token("enforce")]
    Enforce,

    // Input syntax
    #[token("from")]
    From,
    #[token("select")]
    Select,
    #[token("maxage")]
    Maxage,

    // Trigger syntax
    #[token("@")]
    At,
    #[token("always")]
    Always,

    // Enforcement syntax
    #[token("allow")]
    Allow,
    #[token("deny")]
    Deny,
    #[token("pend")]
    Pend,
    #[token("report")]
    Report,

    // Operators
    #[token("=")]
    Eq,
    #[token("!=")]
    Neq,
    #[token("<")]
    Lt,
    #[token("<=")]
    Lte,
    #[token(">")]
    Gt,
    #[token(">=")]
    Gte,
    #[token("!")]
    Not,
    #[token("&&")]
    And,
    #[token("||")]
    Or,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token(".")]
    Dot,

    // Other markers
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token(";")]
    Semicolon,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token(",")]
    Comma,
}

/// A token and its byte range in the original source.
#[derive(Clone, PartialEq, Eq)]
pub struct Token<'a> {
    /// The complete source containing this token.
    pub source: &'a str,
    /// The lexical category.
    pub kind: TokenKind,
    /// The half-open byte range of the token.
    pub span: Range<usize>,
}

impl<'a> Token<'a> {
    pub fn text(&self) -> &'a str {
        &self.source[self.span.clone()]
    }
}

impl std::fmt::Display for Token<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token({:?}, {})", self.kind, self.text())
    }
}

impl std::fmt::Debug for Token<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Token({:?}, {})", self.kind, self.text())
    }
}

/// A borrowed token slice consumed by the parser.
#[derive(Debug, Clone)]
pub struct Tokens<'a> {
    /// The remaining tokens.
    pub tok: &'a [Token<'a>],
    /// The complete source, retained even when no tokens remain for EOF diagnostics.
    pub source: &'a str,
}

impl<'a> Input for Tokens<'a> {
    type Item = &'a Token<'a>;
    type Iter = std::slice::Iter<'a, Token<'a>>;
    type IterIndices = Enumerate<std::slice::Iter<'a, Token<'a>>>;

    #[inline]
    fn input_len(&self) -> usize {
        self.tok.len()
    }

    #[inline]
    fn take(&self, index: usize) -> Self {
        Tokens {
            tok: &self.tok[0..index],
            source: self.source,
        }
    }

    #[inline]
    fn take_from(&self, index: usize) -> Self {
        Tokens {
            tok: &self.tok[index..],
            source: self.source,
        }
    }

    #[inline]
    fn take_split(&self, index: usize) -> (Self, Self) {
        let (prefix, suffix) = self.tok.split_at(index);
        let first = Tokens {
            tok: prefix,
            source: self.source,
        };
        let second = Tokens {
            tok: suffix,
            source: self.source,
        };
        (second, first)
    }

    #[inline]
    fn iter_indices(&self) -> Self::IterIndices {
        self.tok.iter().enumerate()
    }

    #[inline]
    fn iter_elements(&self) -> Self::Iter {
        self.tok.iter()
    }

    #[inline]
    fn position<P>(&self, predicate: P) -> Option<usize>
    where
        P: Fn(Self::Item) -> bool,
    {
        self.tok.iter().position(predicate)
    }

    #[inline]
    fn slice_index(&self, count: usize) -> Result<usize, nom::Needed> {
        if self.tok.len() >= count {
            Ok(count)
        } else {
            Err(nom::Needed::Unknown)
        }
    }
}

/// A lexer retaining source ranges for each token.
pub struct Tokenizer<'a> {
    /// The complete source being tokenized.
    source: &'a str,
    /// The current Logos scanner state.
    lexer: Lexer<'a, TokenKind>,
}

impl<'a> Tokenizer<'a> {
    pub fn new(source: &'a str) -> Self {
        Tokenizer {
            source,
            lexer: TokenKind::lexer(source),
        }
    }
}

impl<'a> Iterator for Tokenizer<'a> {
    type Item = Result<Token<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.lexer.next() {
            Some(kind) => match kind {
                Ok(kind) => Some(Ok(Token {
                    source: self.source,
                    kind,
                    span: self.lexer.span(),
                })),
                Err(_) => Some(Err(Error::new(
                    self.source,
                    self.lexer.span(),
                    ErrorKind::InvalidToken,
                ))),
            },
            None => None,
        }
    }
}

/// Tokenizes a SRDL source string into a vector of tokens.
pub fn tokenize_srdl(srdl: &str) -> Result<Vec<Token<'_>>, Error> {
    Tokenizer::new(srdl).collect::<Result<Vec<_>, _>>()
}
