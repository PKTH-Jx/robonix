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
    let mut triggers = Vec::new();
    let mut excludes = Vec::new();
    // Preserve declaration errors instead of replacing them with trailing-input errors.
    while !input.tok.is_empty() {
        match input.tok[0].kind {
            TokenKind::Rule => {
                let (rest, rule) = rule(input)?;
                rules.push(rule);
                input = rest;
            }
            TokenKind::Trigger => {
                let (rest, clause) = trigger_clause(input)?;
                triggers.push(clause);
                (input, _) = match_token(TokenKind::Semicolon)(rest)?;
            }
            TokenKind::Exclude => {
                let (rest, clause) = exclude_clause(input)?;
                excludes.push(clause);
                (input, _) = match_token(TokenKind::Semicolon)(rest)?;
            }
            _ => return Err(Err::Error(expected(&input, "rule, trigger, or exclude"))),
        }
    }
    Ok((
        input,
        Program {
            rules,
            triggers,
            excludes,
        },
    ))
}

// Clauses may precede their bindings; references remain unresolved here.
// Rules require a condition and one enforcement response, independently of triggers.
fn rule(input: Tokens<'_>) -> IResult<'_, Rule> {
    let (mut input, (_, name, _)) = (
        match_token(TokenKind::Rule),
        identifier,
        match_token(TokenKind::LBrace),
    )
        .parse(input)?;
    let mut inputs = Vec::new();
    let mut contexts = Vec::new();
    let mut requires = Vec::new();
    let mut enforce = None;
    let mut input_names = std::collections::BTreeSet::new();
    // Preserve order within each clause kind, independently of declaration order.
    while input.tok.first().map(|t| t.kind) != Some(TokenKind::RBrace) {
        match input.tok.first().map(|t| t.kind) {
            Some(TokenKind::Input) => {
                let declaration = input.clone();
                let (rest, clause) = input_clause(input)?;
                for field in clause.fields() {
                    if !input_names.insert(field.name().0.clone()) {
                        return Err(Err::Error(at(
                            &declaration,
                            ErrorKind::DuplicateInput(field.name().0.clone()),
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
                    "input, context, require, or enforce",
                )));
            }
        }
        // Clause parsers leave the delimiter for this shared statement boundary.
        (input, _) = match_token(TokenKind::Semicolon)(input)?;
    }
    let closing = input.clone();
    let (input, _) = match_token(TokenKind::RBrace)(input)?;
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
            requires,
            enforce,
        },
    ))
}

// Context paths bind local call metadata; they do not acquire data or accept maxage.
fn context_clause(input: Tokens<'_>) -> IResult<'_, ContextClause> {
    let (input, (_, (data_type, allowed_values), name, _)) = (
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
            allowed_values,
            name,
            path,
        },
    ))
}

fn input_clause(input: Tokens<'_>) -> IResult<'_, InputClause> {
    let (input, _) = match_token(TokenKind::Input)(input)?;
    let (input, _) = match_token(TokenKind::From)(input)?;
    let listed = input.tok.first().map(|t| t.kind) == Some(TokenKind::LBracket);
    let (mut input, capabilities) = one_or_list(input, string_literal)?;
    let source = if listed {
        InputSource::List(capabilities)
    } else {
        InputSource::Single(capabilities.into_iter().next().unwrap())
    };
    let mut selects = Vec::new();
    let mut yields = Vec::new();
    loop {
        match input.tok.first().map(|t| t.kind) {
            Some(TokenKind::Select) => {
                input = input.take_from(1);
                let (rest, selected) = one_or_list(input, input_field)?;
                selects.extend(selected);
                input = rest;
            }
            Some(TokenKind::Yield) => {
                let (rest, clause) = yield_clause(input.take_from(1))?;
                yields.push(clause);
                input = rest;
            }
            _ => return Err(Err::Error(expected(&input, "select or yield"))),
        }
        if input.tok.first().map(|t| t.kind) != Some(TokenKind::Comma) {
            break;
        }
        input = input.take_from(1);
    }
    Ok((
        input,
        InputClause {
            source,
            selects,
            yields,
        },
    ))
}

fn yield_clause(input: Tokens<'_>) -> IResult<'_, YieldClause> {
    let (input, fields) = one_or_list(input, yield_field)?;
    let (input, _) = match_token(TokenKind::Using)(input)?;
    let (input, strategies) = one_or_list(input, strategy_call)?;
    Ok((input, YieldClause { fields, strategies }))
}

fn yield_field(input: Tokens<'_>) -> IResult<'_, YieldField> {
    let (input, ((data_type, allowed_values), name, _, description)) = (
        input_type,
        identifier,
        match_token(TokenKind::Eq),
        string_literal,
    )
        .parse(input)?;
    let (input, maxage) = maxage(input)?;
    Ok((
        input,
        YieldField {
            data_type,
            allowed_values,
            name,
            maxage,
            description,
        },
    ))
}

fn strategy_call(input: Tokens<'_>) -> IResult<'_, StrategyCall> {
    let (input, name) = identifier(input)?;
    let (input, _) = match_token(TokenKind::LParen)(input)?;
    let (input, arguments) =
        nom::multi::separated_list0(match_token(TokenKind::Comma), string_literal).parse(input)?;
    let (input, _) = match_token(TokenKind::RParen)(input)?;
    Ok((input, StrategyCall { name, arguments }))
}

// Omitting '= PATH' selects the root response, including scalar-valued interfaces.
fn input_field(input: Tokens<'_>) -> IResult<'_, SelectField> {
    let (input, ((data_type, allowed_values), name)) = (input_type, identifier).parse(input)?;
    let (input, select) = if input.tok.first().map(|t| t.kind) == Some(TokenKind::Eq) {
        select_path(input.take_from(1))?
    } else {
        (input, Vec::new())
    };
    let (input, maxage) = maxage(input)?;
    Ok((
        input,
        SelectField {
            data_type,
            allowed_values,
            name,
            select,
            maxage,
        },
    ))
}

fn maxage(input: Tokens<'_>) -> IResult<'_, Option<f64>> {
    if input.tok.first().map(|token| token.kind) != Some(TokenKind::Maxage) {
        return Ok((input, None));
    }
    let time = input.take_from(1);
    let (input, value) = number_literal(time.clone())?;
    if value <= 0.0 {
        return Err(Err::Error(at(&time, ErrorKind::InvalidMaxage)));
    }
    Ok((input, Some(value)))
}

fn input_type(input: Tokens<'_>) -> IResult<'_, (InputType, Vec<Literal>)> {
    if input
        .tok
        .first()
        .is_some_and(|t| t.kind == TokenKind::Option)
    {
        let (input, _) = match_token(TokenKind::Lt)(input.take_from(1))?;
        if input
            .tok
            .first()
            .is_some_and(|t| t.kind == TokenKind::Option)
        {
            return Err(Err::Failure(at(&input, ErrorKind::NestedOption)));
        }
        let (input, (inner, allowed)) = input_type(input)?;
        let (input, _) = match_token(TokenKind::Gt)(input)?;
        return Ok((input, (InputType::Option(Box::new(inner)), allowed)));
    }
    let enumeration = input.tok.first().is_some_and(|t| t.kind == TokenKind::Enum);
    let input = if enumeration {
        input.take_from(1)
    } else {
        input
    };
    let data_type = match input.tok.first().map(|token| token.text()) {
        Some("number") => InputType::Number,
        Some("string") => InputType::String,
        Some("boolean") if !enumeration => InputType::Boolean,
        _ => {
            return Err(Err::Error(expected(
                &input,
                "number, string, boolean, option<T>, or enum number/string",
            )));
        }
    };
    let input = input.take_from(1);
    if !enumeration {
        return Ok((input, (data_type, Vec::new())));
    }
    let (input, _) = match_token(TokenKind::LBrace)(input)?;
    let start = input.clone();
    let (input, allowed) =
        separated_list1(match_token(TokenKind::Comma), cut(literal)).parse(input)?;
    for (index, value) in allowed.iter().enumerate() {
        if !matches!(
            (&data_type, value),
            (InputType::Number, Literal::Number(_)) | (InputType::String, Literal::String(_))
        ) {
            return Err(Err::Failure(expected(
                &start,
                "enum literals matching the declared base type",
            )));
        }
        if allowed[..index].contains(value) {
            return Err(Err::Failure(expected(&start, "unique enum members")));
        }
    }
    let (input, _) = match_token(TokenKind::RBrace)(input)?;
    Ok((input, (data_type, allowed)))
}

// Start with a field, then append .field and [field = literal] operations in order.
fn select_path(input: Tokens<'_>) -> IResult<'_, Vec<SelectStep>> {
    let (mut input, field) = if input.tok.first().is_some_and(|token| {
        token.kind == TokenKind::NumberLiteral && token.text().bytes().all(|b| b.is_ascii_digit())
    }) {
        let field = Identifier(input.tok[0].text().to_owned());
        (input.take_from(1), field)
    } else {
        select_field(input)?
    };
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
    if is_identifier(token.kind)
        || matches!(
            token.kind,
            TokenKind::Yield
                | TokenKind::Using
                | TokenKind::True
                | TokenKind::False
                | TokenKind::Rule
                | TokenKind::Input
                | TokenKind::Trigger
                | TokenKind::Require
                | TokenKind::Enforce
                | TokenKind::From
                | TokenKind::Select
                | TokenKind::Maxage
        )
    {
        Ok((rest, Identifier(token.text().to_owned())))
    } else {
        Err(Err::Error(expected(&input, "a select field name")))
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

// Both lists form a Cartesian product; matching still selects each rule only once.
fn trigger_clause(input: Tokens<'_>) -> IResult<'_, TriggerClause> {
    let (input, _) = match_token(TokenKind::Trigger)(input)?;
    let (input, rules) = one_or_list(input, identifier)?;
    let timing = match input.tok.first().map(|token| token.kind) {
        Some(TokenKind::Before) => TriggerTiming::Before,
        Some(TokenKind::After) => TriggerTiming::After,
        Some(TokenKind::While) => TriggerTiming::While,
        _ => {
            return Err(Err::Error(expected(
                &input,
                "'before', 'after', or 'while'",
            )));
        }
    };
    let input = input.take_from(1);
    let (input, capabilities) = one_or_list(input, trigger_target)?;
    let (input, interval) =
        if input.tok.first().map(|token| token.kind) == Some(TokenKind::Interval) {
            if timing != TriggerTiming::While {
                return Err(Err::Error(expected(
                    &input,
                    "interval only on a while trigger",
                )));
            }
            let start = input.take_from(1);
            let (rest, interval) = number_literal(start.clone())?;
            if interval <= 0.0 {
                return Err(Err::Error(expected(&start, "a positive interval factor")));
            }
            (rest, interval)
        } else {
            (input, 5.0)
        };
    Ok((
        input,
        TriggerClause {
            rules,
            timing,
            capabilities,
            interval,
        },
    ))
}

fn one_or_list<'a, T>(
    input: Tokens<'a>,
    item: impl FnMut(Tokens<'a>) -> IResult<'a, T>,
) -> IResult<'a, Vec<T>> {
    if input.tok.first().map(|t| t.kind) != Some(TokenKind::LBracket) {
        return map(item, |value| vec![value]).parse(input);
    }
    let (input, items) =
        separated_list1(match_token(TokenKind::Comma), cut(item)).parse(input.take_from(1))?;
    let (input, _) = match_token(TokenKind::RBracket)(input)?;
    Ok((input, items))
}

fn trigger_target(input: Tokens<'_>) -> IResult<'_, TriggerTarget> {
    match input.tok.first().map(|t| t.kind) {
        Some(TokenKind::Always) => Ok((input.take_from(1), TriggerTarget::Always)),
        _ => map(capability_id, TriggerTarget::Capability).parse(input),
    }
}

fn capability_id(input: Tokens<'_>) -> IResult<'_, String> {
    if input.tok.first().map(|token| token.kind) == Some(TokenKind::StringLiteral) {
        string_literal(input)
    } else {
        map(identifier, |name| name.0).parse(input)
    }
}

fn exclude_clause(input: Tokens<'_>) -> IResult<'_, ExcludeClause> {
    let (input, _) = match_token(TokenKind::Exclude)(input)?;
    let mutual = input.tok.first().map(|token| token.kind) == Some(TokenKind::Mutual);
    let input = if mutual { input.take_from(1) } else { input };
    let (input, (source, target)) = (capability_id, capability_id).parse(input)?;
    Ok((
        input,
        ExcludeClause {
            source,
            target,
            mutual,
        },
    ))
}

fn require_clause(input: Tokens<'_>) -> IResult<'_, RequireClause> {
    let (input, _) = match_token(TokenKind::Require)(input)?;
    let (input, expression) = condition_expression(input)?;
    Ok((input, RequireClause { expression }))
}

// Bound the token count before allocating a potentially deep condition AST.
// The enclosing rule consumes the semicolon after expression parsing stops.
fn condition_expression(input: Tokens<'_>) -> IResult<'_, Expression> {
    let count = input
        .tok
        .iter()
        .position(|t| t.kind == TokenKind::Semicolon)
        .unwrap_or(input.tok.len());
    if count > 1024 {
        return Err(Err::Failure(at(&input, ErrorKind::ExpressionTooLong(1024))));
    }
    let (rest, value) = expression(input.clone(), 0, 0)?;
    // Postfix and left-associative operators can grow AST height without parser recursion.
    let mut pending = vec![(&value, 0)];
    while let Some((node, depth)) = pending.pop() {
        if depth >= MAX_EXPRESSION_DEPTH {
            return Err(Err::Failure(at(
                &input,
                ErrorKind::ExpressionTooDeep(MAX_EXPRESSION_DEPTH),
            )));
        }
        let depth = depth + 1;
        match node {
            Expression::Try(value)
            | Expression::Unary { operand: value, .. }
            | Expression::Field { base: value, .. } => pending.push((value, depth)),
            Expression::Binary { left, right, .. } => {
                pending.extend([(left.as_ref(), depth), (right.as_ref(), depth)])
            }
            Expression::Call { callee, arguments } => {
                pending.push((callee, depth));
                pending.extend(arguments.iter().map(|argument| (argument, depth)));
            }
            Expression::Match {
                value, some, none, ..
            } => pending.extend([
                (value.as_ref(), depth),
                (some.as_ref(), depth),
                (none.as_ref(), depth),
            ]),
            Expression::Literal(_) | Expression::Identifier(_) => {}
        }
    }
    Ok((rest, value))
}

fn enforce_clause(input: Tokens<'_>) -> IResult<'_, EnforceClause> {
    let (input, _) = match_token(TokenKind::Enforce)(input)?;
    let (input, hooks) = one_or_list(input, enforce_hook)?;
    Ok((input, EnforceClause { hooks }))
}

fn enforce_hook(input: Tokens<'_>) -> IResult<'_, EnforceBehavior> {
    match input.tok.first().map(|t| t.kind) {
        Some(TokenKind::Allow) => Ok((input.take_from(1), EnforceBehavior::Allow)),
        Some(TokenKind::Deny) => Ok((input.take_from(1), EnforceBehavior::Deny)),
        Some(TokenKind::Stop) => Ok((input.take_from(1), EnforceBehavior::Stop)),
        Some(TokenKind::Call) => {
            let (rest, capability) = string_literal(input.take_from(1))?;
            Ok((rest, EnforceBehavior::Call(capability)))
        }
        Some(TokenKind::Report) => {
            let (rest, message) = string_literal(input.take_from(1))?;
            Ok((rest, EnforceBehavior::Report(message)))
        }
        _ => Err(Err::Error(expected(
            &input,
            "'allow', 'deny', 'stop', 'call', or 'report'",
        ))),
    }
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

// Contextual keywords remain available as binding and field names.
fn is_identifier(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Identifier
            | TokenKind::Enum
            | TokenKind::Option
            | TokenKind::Some
            | TokenKind::None
            | TokenKind::Match
            | TokenKind::Exclude
            | TokenKind::Mutual
            | TokenKind::Context
            | TokenKind::Always
            | TokenKind::Before
            | TokenKind::After
            | TokenKind::While
            | TokenKind::Interval
            | TokenKind::Allow
            | TokenKind::Deny
            | TokenKind::Stop
            | TokenKind::Call
            | TokenKind::Report
    )
}

fn identifier(input: Tokens) -> IResult<Identifier> {
    match input.tok.first() {
        Some(token) if is_identifier(token.kind) => {
            Ok((input.take_from(1), Identifier(token.text().to_owned())))
        }
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

// Filters accept signed numeric constants directly; condition clauses represent unary minus
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
        // Fallback chains associate right; other operators associate left.
        let right_precedence = if operator == BinaryOperator::Fallback {
            precedence
        } else {
            precedence + 1
        };
        let (rest, right) = expression(input.take_from(1), right_precedence, depth + 1)?;
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
        // At an operator or delimiter, `match` is the binding itself.
        TokenKind::Match
            if rest.tok.first().is_some_and(|next| {
                is_identifier(next.kind)
                    || matches!(
                        next.kind,
                        TokenKind::LParen
                            | TokenKind::Not
                            | TokenKind::True
                            | TokenKind::False
                            | TokenKind::NumberLiteral
                            | TokenKind::StringLiteral
                    )
            }) =>
        {
            let (input, value) = match_expression(rest, depth)?;
            rest = input;
            value
        }
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
            Some(TokenKind::Question) => {
                value = Expression::Try(Box::new(value));
                rest = rest.take_from(1);
            }
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

fn match_expression(input: Tokens<'_>, depth: usize) -> IResult<'_, Expression> {
    let (input, value) = expression(input, 0, depth)?;
    let (mut input, _) = match_token(TokenKind::LBrace)(input)?;
    let mut some = None;
    let mut none = None;
    for arm in 0..2 {
        match input.tok.first().map(|t| t.kind) {
            Some(TokenKind::Some) if some.is_none() => {
                let (rest, (_, binding, _, _)) = (
                    match_token(TokenKind::LParen),
                    identifier,
                    match_token(TokenKind::RParen),
                    match_token(TokenKind::Arrow),
                )
                    .parse(input.take_from(1))?;
                let (rest, body) = expression(rest, 0, depth)?;
                some = Some((binding, body));
                input = rest;
            }
            Some(TokenKind::None) if none.is_none() => {
                let (rest, _) = match_token(TokenKind::Arrow)(input.take_from(1))?;
                let (rest, body) = expression(rest, 0, depth)?;
                none = Some(body);
                input = rest;
            }
            _ => {
                return Err(Err::Failure(expected(
                    &input,
                    "one some(name) arm and one none arm",
                )));
            }
        }
        if arm == 0
            || input
                .tok
                .first()
                .is_some_and(|t| t.kind == TokenKind::Comma)
        {
            (input, _) = match_token(TokenKind::Comma)(input)?;
        }
    }
    let (input, _) = match_token(TokenKind::RBrace)(input)?;
    let (binding, some) = some.unwrap();
    Ok((
        input,
        Expression::Match {
            value: Box::new(value),
            binding,
            some: Box::new(some),
            none: Box::new(none.unwrap()),
        },
    ))
}

// | < || < && < comparisons < addition/subtraction < multiplication/division/remainder.
fn binary(kind: TokenKind) -> Option<(BinaryOperator, u8)> {
    Some(match kind {
        TokenKind::Fallback => (BinaryOperator::Fallback, 0),
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
