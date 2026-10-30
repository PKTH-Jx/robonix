use super::ast::*;
use super::lexer::*;
use super::{Error, ErrorKind};
use nom::Err;
use nom::Finish;
use nom::combinator::{cut, map};
use nom::error::{ErrorKind as NomErrorKind, ParseError};
use nom::multi::separated_list1;
use nom::sequence::preceded;
use nom::{Input, Parser};

pub type IResult<'a, Output> = nom::IResult<Tokens<'a>, Output, Error>;

impl<'a> ParseError<Tokens<'a>> for Error {
    fn from_error_kind(input: Tokens<'a>, _: NomErrorKind) -> Self {
        expected(&input, "valid SRDL syntax")
    }

    fn append(_: Tokens<'a>, _: NomErrorKind, other: Self) -> Self {
        other
    }
}

fn at(input: &Tokens<'_>, kind: ErrorKind) -> Error {
    let span = input
        .tok
        .first()
        .map_or(input.source.len()..input.source.len(), |t| t.span.clone());
    Error::new(input.source, span, kind)
}

fn expected(input: &Tokens<'_>, expected: &str) -> Error {
    at(
        input,
        ErrorKind::Expected {
            expected: expected.to_owned(),
            found: input
                .tok
                .first()
                .map_or_else(|| "end of input".to_owned(), |t| format!("{:?}", t.text())),
        },
    )
}

/// Parse a complete SRDL program from a token slice, returning the model or the first error.
pub fn parse_srdl(input: Tokens<'_>) -> Result<Program, Error> {
    program(input).finish().map(|(_, model)| model)
}

fn program(mut input: Tokens<'_>) -> IResult<'_, Program> {
    let mut rules = Vec::new();
    // Do not backtrack over malformed rules or replace their errors with trailing-input errors.
    while !input.tok.is_empty() {
        let (rest, rule) = rule(input)?;
        rules.push(rule);
        input = rest;
    }
    Ok((input, Program { rules }))
}

// Parse clauses in any order, requiring unique input names, at least one trigger
// and requirement, and exactly one enforcement response. References remain unresolved.
fn rule(input: Tokens<'_>) -> IResult<'_, Rule> {
    let (mut input, (_, name, _)) = (
        match_token(TokenKind::Rule),
        identifier,
        match_token(TokenKind::LBrace),
    )
        .parse(input)?;
    let mut inputs = Vec::new();
    let mut contexts = Vec::new();
    let mut triggers = Vec::new();
    let mut requires = Vec::new();
    let mut enforce = None;
    let mut input_names = std::collections::BTreeSet::new();
    // Group clauses by kind so requirements may precede their input declarations.
    while input.tok.first().map(|t| t.kind) != Some(TokenKind::RBrace) {
        match input.tok.first().map(|t| t.kind) {
            Some(TokenKind::Input) => {
                let declaration = input.clone();
                let (rest, clause) = input_clause(input)?;
                for field in &clause.fields {
                    if !input_names.insert(field.name.0.clone()) {
                        return Err(Err::Error(at(
                            &declaration,
                            ErrorKind::DuplicateInput(field.name.0.clone()),
                        )));
                    }
                }
                inputs.push(clause);
                input = rest;
            }
            Some(TokenKind::Context) => {
                let (rest, clause) = context_clause(input)?;
                contexts.push(clause);
                input = rest;
            }
            Some(TokenKind::Trigger) => {
                let (rest, clause) = trigger_clause(input)?;
                triggers.push(clause);
                input = rest;
            }
            Some(TokenKind::Require) => {
                let (rest, clause) = require_clause(input)?;
                requires.push(clause);
                input = rest;
            }
            Some(TokenKind::Enforce) if enforce.is_none() => {
                let (rest, clause) = enforce_clause(input)?;
                enforce = Some(clause);
                input = rest;
            }
            Some(TokenKind::Enforce) => {
                return Err(Err::Error(at(&input, ErrorKind::DuplicateEnforce)));
            }
            None => return Err(Err::Error(expected(&input, "'}'"))),
            _ => {
                return Err(Err::Error(expected(
                    &input,
                    "input, context, trigger, require, or enforce",
                )));
            }
        }
        // Clause parsers leave the delimiter for this shared statement boundary.
        (input, _) = match_token(TokenKind::Semicolon)(input)?;
    }
    let closing = input.clone();
    let (input, _) = match_token(TokenKind::RBrace)(input)?;
    if triggers.is_empty() {
        return Err(Err::Error(at(
            &closing,
            ErrorKind::MissingClause("trigger"),
        )));
    }
    if requires.is_empty() {
        return Err(Err::Error(at(
            &closing,
            ErrorKind::MissingClause("require"),
        )));
    }
    let Some(enforce) = enforce else {
        return Err(Err::Error(at(
            &closing,
            ErrorKind::MissingClause("enforce"),
        )));
    };
    Ok((
        input,
        Rule {
            name,
            inputs,
            contexts,
            triggers,
            requires,
            enforce,
        },
    ))
}

// Context paths bind local call metadata; they do not acquire data or accept maxage.
fn context_clause(input: Tokens<'_>) -> IResult<'_, ContextClause> {
    let (input, (_, data_type, name, _)) = (
        match_token(TokenKind::Context),
        input_type,
        identifier,
        match_token(TokenKind::Eq),
    )
        .parse(input)?;
    let (input, path) =
        separated_list1(match_token(TokenKind::Dot), cut(select_field)).parse(input)?;
    Ok((
        input,
        ContextClause {
            data_type,
            name,
            path,
        },
    ))
}

// One acquisition owns a nonempty list of TYPE NAME [= PATH] [maxage NUMBER] fields.
// Commas separate fields; a comma inside a filter is not valid select syntax.
fn input_clause(input: Tokens<'_>) -> IResult<'_, InputClause> {
    let (input, (_, _, capability, _)) = (
        match_token(TokenKind::Input),
        match_token(TokenKind::From),
        string_literal,
        match_token(TokenKind::Select),
    )
        .parse(input)?;
    let (input, fields) =
        separated_list1(match_token(TokenKind::Comma), cut(input_field)).parse(input)?;
    Ok((input, InputClause { capability, fields }))
}

// Omitting '= PATH' selects the root response, including scalar-valued interfaces.
fn input_field(input: Tokens<'_>) -> IResult<'_, InputField> {
    let (input, (data_type, name)) = (input_type, identifier).parse(input)?;
    let (input, select) = if input.tok.first().map(|t| t.kind) == Some(TokenKind::Eq) {
        select_path(input.take_from(1))?
    } else {
        (input, Vec::new())
    };
    let (input, maxage) = if input.tok.first().map(|token| token.kind) == Some(TokenKind::Maxage) {
        // Once maxage appears, require a positive finite value in seconds without backtracking.
        let time = input.take_from(1);
        let (input, maxage) = number_literal(time.clone())?;
        if maxage <= 0.0 {
            return Err(Err::Error(at(&time, ErrorKind::InvalidMaxage)));
        }
        (input, Some(maxage))
    } else {
        (input, None)
    };
    Ok((
        input,
        InputField {
            data_type,
            name,
            select,
            maxage,
        },
    ))
}

fn input_type(input: Tokens<'_>) -> IResult<'_, InputType> {
    let data_type = match input.tok.first().map(|token| token.text()) {
        Some("number") => InputType::Number,
        Some("string") => InputType::String,
        Some("boolean") => InputType::Boolean,
        _ => {
            return Err(Err::Error(expected(
                &input,
                "'number', 'string', or 'boolean'",
            )));
        }
    };
    Ok((input.take_from(1), data_type))
}

// Start with a field, then append .field and [field = literal] operations in order.
fn select_path(input: Tokens<'_>) -> IResult<'_, Vec<SelectStep>> {
    let (mut input, field) = select_field(input)?;
    let mut steps = vec![SelectStep::Field(field)];
    loop {
        match input.tok.first().map(|t| t.kind) {
            Some(TokenKind::Dot) => {
                let (rest, field) = select_field(input.take_from(1))?;
                steps.push(SelectStep::Field(field));
                input = rest;
            }
            Some(TokenKind::LBracket) => {
                let (rest, (field, value)) = select_filter(input.take_from(1))?;
                steps.push(SelectStep::Filter { field, value });
                input = rest;
            }
            _ => break,
        }
    }
    Ok((input, steps))
}

// A response field can share a spelling with any SRDL keyword. Only select
// field positions use this parser; filter values still use literal parsing.
fn select_field(input: Tokens<'_>) -> IResult<'_, Identifier> {
    let Some(token) = input.tok.first() else {
        return Err(Err::Error(expected(&input, "a select field name")));
    };
    let rest = input.take_from(1);
    match token.kind {
        TokenKind::Identifier
        | TokenKind::True
        | TokenKind::False
        | TokenKind::Rule
        | TokenKind::Input
        | TokenKind::Context
        | TokenKind::Trigger
        | TokenKind::Require
        | TokenKind::Enforce
        | TokenKind::From
        | TokenKind::Select
        | TokenKind::Maxage
        | TokenKind::Always
        | TokenKind::Capability
        | TokenKind::Allow
        | TokenKind::Deny
        | TokenKind::Pend
        | TokenKind::Report => Ok((rest, Identifier(token.text().to_owned()))),
        _ => Err(Err::Error(expected(&input, "a select field name"))),
    }
}

fn select_filter(input: Tokens<'_>) -> IResult<'_, (Identifier, Literal)> {
    let (input, field) = select_field(input)?;
    let (input, _) = match_token(TokenKind::Eq)(input)?;
    let (input, value) = literal(input)?;
    let (input, _) = match_token(TokenKind::RBracket)(input)?;
    Ok((input, (field, value)))
}

// Decode JSON escapes after lexing, including validation of Unicode surrogate pairs.
fn string_literal(input: Tokens<'_>) -> IResult<'_, String> {
    let start = input.clone();
    let (input, token) = match_token(TokenKind::StringLiteral)(input)?;
    let value = serde_json::from_str(token.tok[0].text())
        .map_err(|_| Err::Error(at(&start, ErrorKind::InvalidString)))?;
    Ok((input, value))
}

// Grammar: trigger always | trigger capability "CONTRACT".
// These alternatives describe dispatch-time checks; the rule consumes the semicolon.
fn trigger_clause(input: Tokens<'_>) -> IResult<'_, TriggerClause> {
    let (input, _) = match_token(TokenKind::Trigger)(input)?;
    match input.tok.first().map(|t| t.kind) {
        Some(TokenKind::Always) => Ok((input.take_from(1), TriggerClause::Always)),
        Some(TokenKind::Capability) => {
            map(string_literal, TriggerClause::Capability).parse(input.take_from(1))
        }
        _ => Err(Err::Error(expected(&input, "'always' or 'capability'"))),
    }
}

// Bound the expression's token count before building its potentially deep AST.
// The enclosing rule consumes the semicolon after expression parsing stops.
fn require_clause(input: Tokens) -> IResult<RequireClause> {
    let (input, _) = match_token(TokenKind::Require)(input)?;
    let count = input
        .tok
        .iter()
        .position(|t| t.kind == TokenKind::Semicolon)
        .unwrap_or(input.tok.len());
    if count > 1024 {
        return Err(Err::Failure(at(&input, ErrorKind::ExpressionTooLong(1024))));
    }
    let (input, expression) = expression(input, 0, 0)?;
    Ok((input, RequireClause { expression }))
}

// Grammar: enforce (allow | deny | pend) [report "TEXT"].
// Reports are static strings; disposition and user intervention are deferred to runtime.
fn enforce_clause(input: Tokens<'_>) -> IResult<'_, EnforceClause> {
    let (input, _) = match_token(TokenKind::Enforce)(input)?;
    let behavior = match input.tok.first().map(|t| t.kind) {
        Some(TokenKind::Allow) => EnforceBehavior::Allow,
        Some(TokenKind::Deny) => EnforceBehavior::Deny,
        Some(TokenKind::Pend) => EnforceBehavior::Pend,
        _ => return Err(Err::Error(expected(&input, "'allow', 'deny', or 'pend'"))),
    };
    let input = input.take_from(1);
    // Once report is present, keep a malformed report's error instead of backtracking.
    let (input, report) = if input.tok.first().map(|t| t.kind) == Some(TokenKind::Report) {
        let (input, report) = string_literal(input.take_from(1))?;
        (input, Some(report))
    } else {
        (input, None)
    };
    Ok((input, EnforceClause { behavior, report }))
}

fn match_token(kind: TokenKind) -> impl FnMut(Tokens<'_>) -> IResult<'_, Tokens<'_>> {
    let label = match kind {
        TokenKind::LBrace => "'{'".to_owned(),
        TokenKind::RBrace => "'}'".to_owned(),
        TokenKind::RParen => "')'".to_owned(),
        TokenKind::RBracket => "']'".to_owned(),
        TokenKind::Semicolon => "';'".to_owned(),
        TokenKind::Comma => "','".to_owned(),
        TokenKind::Eq => "'='".to_owned(),
        TokenKind::Minus => "'-'".to_owned(),
        TokenKind::StringLiteral => "a string literal".to_owned(),
        TokenKind::NumberLiteral => "a number".to_owned(),
        _ => format!("'{kind:?}'").to_lowercase(),
    };
    move |input| {
        if input.tok.first().map(|token| token.kind) == Some(kind) {
            Ok((input.take_from(1), input.take(1)))
        } else {
            Err(Err::Error(expected(&input, &label)))
        }
    }
}

fn identifier(input: Tokens) -> IResult<Identifier> {
    let Some(token) = input.tok.first() else {
        return Err(Err::Error(expected(&input, "an identifier")));
    };
    let rest = input.take_from(1);
    match token.kind {
        // Contextual keywords remain available as binding and field names.
        TokenKind::Identifier
        | TokenKind::Context
        | TokenKind::Always
        | TokenKind::Capability
        | TokenKind::Allow
        | TokenKind::Deny
        | TokenKind::Pend
        | TokenKind::Report => Ok((rest, Identifier(token.text().to_string()))),
        _ => Err(Err::Error(expected(&input, "an identifier"))),
    }
}

// All numeric contexts share finite f64 parsing, including decimal leading zeros.
fn number_literal(input: Tokens<'_>) -> IResult<'_, f64> {
    let start = input.clone();
    let (input, token) = match_token(TokenKind::NumberLiteral)(input)?;
    let value = token.tok[0]
        .text()
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
        .ok_or_else(|| Err::Error(at(&start, ErrorKind::InvalidNumber)))?;
    Ok((input, value))
}

// Filters accept signed numeric constants directly; requirements represent unary minus
// as an expression node. Strings use the same escape decoding as other clauses.
fn literal(input: Tokens<'_>) -> IResult<'_, Literal> {
    match input.tok.first().map(|token| token.kind) {
        Some(TokenKind::NumberLiteral) => map(number_literal, Literal::Number).parse(input),
        Some(TokenKind::Minus) => map(
            preceded(match_token(TokenKind::Minus), number_literal),
            |value| Literal::Number(-value),
        )
        .parse(input),
        Some(TokenKind::StringLiteral) => map(string_literal, Literal::String).parse(input),
        Some(kind @ (TokenKind::True | TokenKind::False)) => Ok((
            input.take_from(1),
            Literal::Boolean(kind == TokenKind::True),
        )),
        _ => Err(Err::Error(expected(
            &input,
            "a boolean, number, or string literal",
        ))),
    }
}

// Bound recursive nesting before deeply nested untrusted rules exhaust the stack.
const MAX_EXPRESSION_DEPTH: usize = 128;

// Precedence climbing: larger precedence values bind more tightly.
// Each call consumes operators at or above min_precedence, leaving weaker ones to its caller.
fn expression(input: Tokens<'_>, min_precedence: u8, depth: usize) -> IResult<'_, Expression> {
    if depth >= MAX_EXPRESSION_DEPTH {
        return Err(Err::Failure(at(
            &input,
            ErrorKind::ExpressionTooDeep(MAX_EXPRESSION_DEPTH),
        )));
    }
    let (mut input, mut left) = operand(input, depth + 1)?;
    let mut compared = false;
    while let Some((operator, precedence)) = input.tok.first().and_then(|t| binary(t.kind)) {
        if precedence < min_precedence {
            break;
        }
        // Comparisons have precedence 3 and cannot chain: write a < b && b < c.
        if precedence == 3 && compared {
            return Err(Err::Error(at(&input, ErrorKind::ChainedComparison)));
        }
        compared |= precedence == 3;
        // The right operand absorbs tighter operators, so a + b * c becomes a + (b * c).
        // Excluding equal precedence makes a - b - c associate as (a - b) - c.
        let (rest, right) = expression(input.take_from(1), precedence + 1, depth + 1)?;
        left = Expression::Binary {
            operator,
            left: Box::new(left),
            right: Box::new(right),
        };
        input = rest;
    }
    Ok((input, left))
}

// Prefixes recurse at higher precedence; primary operands collect field access
// and calls before returning. Array filters belong only to select paths.
fn operand(input: Tokens<'_>, depth: usize) -> IResult<'_, Expression> {
    let Some(token) = input.tok.first() else {
        return Err(Err::Error(expected(&input, "an expression")));
    };
    let mut rest = input.take_from(1);
    let mut value = match token.kind {
        TokenKind::Not | TokenKind::Minus => {
            // Level 6 excludes binary operators while allowing nested prefixes and postfixes.
            let (rest, operand) = expression(rest, 6, depth)?;
            return Ok((
                rest,
                Expression::Unary {
                    operator: if token.kind == TokenKind::Not {
                        UnaryOperator::Not
                    } else {
                        UnaryOperator::Negate
                    },
                    operand: Box::new(operand),
                },
            ));
        }
        TokenKind::LParen => {
            // Parentheses reset the threshold so their contents form a complete expression.
            let (input, value) = expression(rest, 0, depth)?;
            (rest, _) = match_token(TokenKind::RParen)(input)?;
            value
        }
        TokenKind::True
        | TokenKind::False
        | TokenKind::NumberLiteral
        | TokenKind::StringLiteral => {
            let (input, value) = literal(input)?;
            rest = input;
            Expression::Literal(value)
        }
        // Resolve contextual keywords through the same identifier parser as declarations.
        _ => {
            let (input, name) = identifier(input.clone())
                .map_err(|_| Err::Error(expected(&input, "an expression")))?;
            rest = input;
            Expression::Identifier(name)
        }
    };
    // Build field access and calls before returning to prefix or binary parsing;
    // this makes -soma.velocity bind as -(soma.velocity).
    loop {
        match rest.tok.first().map(|t| t.kind) {
            Some(TokenKind::Dot) => {
                let (input, field) = identifier(rest.take_from(1))?;
                value = Expression::Field {
                    base: Box::new(value),
                    field,
                };
                rest = input;
            }
            Some(TokenKind::LParen) => {
                // Each argument starts at level 0; commas and ')' terminate its expression.
                rest = rest.take_from(1);
                let (input, arguments) =
                    if rest.tok.first().map(|t| t.kind) == Some(TokenKind::RParen) {
                        (rest, Vec::new())
                    } else {
                        // Commit to each argument so failures after a comma retain their location.
                        separated_list1(
                            match_token(TokenKind::Comma),
                            cut(|input| expression(input, 0, depth)),
                        )
                        .parse(rest)?
                    };
                (rest, _) = match_token(TokenKind::RParen)(input)?;
                value = Expression::Call {
                    callee: Box::new(value),
                    arguments,
                };
            }
            _ => break,
        }
    }
    Ok((rest, value))
}

// || < && < comparisons < addition/subtraction < multiplication/division/remainder.
fn binary(kind: TokenKind) -> Option<(BinaryOperator, u8)> {
    Some(match kind {
        TokenKind::Or => (BinaryOperator::Or, 1),
        TokenKind::And => (BinaryOperator::And, 2),
        TokenKind::Eq => (BinaryOperator::Eq, 3),
        TokenKind::Neq => (BinaryOperator::Neq, 3),
        TokenKind::Lt => (BinaryOperator::Lt, 3),
        TokenKind::Lte => (BinaryOperator::Lte, 3),
        TokenKind::Gt => (BinaryOperator::Gt, 3),
        TokenKind::Gte => (BinaryOperator::Gte, 3),
        TokenKind::Plus => (BinaryOperator::Add, 4),
        TokenKind::Minus => (BinaryOperator::Subtract, 4),
        TokenKind::Star => (BinaryOperator::Multiply, 5),
        TokenKind::Slash => (BinaryOperator::Divide, 5),
        TokenKind::Percent => (BinaryOperator::Remainder, 5),
        _ => return None,
    })
}
