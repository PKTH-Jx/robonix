//! The Abstract Syntax Tree (AST) for the Sentinel Rule Definition Language (SRDL).

/// An SRDL file, which contains a list of rules.
#[derive(Debug)]
pub struct Program {
    /// The list of rules in the program.
    pub rules: Vec<Rule>,
}

/// A rule in the SRDL language.
#[derive(Debug)]
pub struct Rule {
    /// The name of the rule.
    pub name: Identifier,
    /// Inputs available to every clause, regardless of declaration order.
    pub inputs: Vec<InputClause>,
    /// Bindings to the Executor's immutable context for this capability call.
    pub contexts: Vec<ContextClause>,
    /// Alternative capability selectors; any match makes the rule applicable.
    pub triggers: Vec<TriggerClause>,
    /// Preconditions evaluated before capability dispatch.
    pub requires: Vec<RequireClause>,
    /// Postconditions checked before accepting the capability's reported success.
    pub ensures: Vec<EnsureClause>,
    /// The response declared for a violated safety condition.
    pub enforce: EnforceClause,
}

/// A source identifier used for names and selected fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identifier(
    /// The identifier spelling in the source.
    pub String,
);

/// One capability acquisition shared by a group of selected fields.
#[derive(Debug)]
pub struct InputClause {
    /// The source capability contract ID resolved by the input Adapter.
    pub capability: String,
    /// Fields extracted from one response and sharing its acquisition time.
    pub fields: Vec<InputField>,
}

/// One named value extracted from a shared capability response.
#[derive(Debug)]
pub struct InputField {
    /// The expected basic type of the selected response value.
    pub data_type: InputType,
    /// The name referenced by expressions in this rule.
    pub name: Identifier,
    /// Response-relative traversal; an empty path selects the complete response.
    pub select: Vec<SelectStep>,
    /// Maximum elapsed time from acquisition to evaluation, in seconds; None never expires.
    pub maxage: Option<f64>,
}

/// One typed binding to a field in the Executor's current call context.
#[derive(Debug)]
pub struct ContextClause {
    /// The basic type available to requirement expressions.
    pub data_type: InputType,
    /// The alias sharing the rule's scope with input fields.
    pub name: Identifier,
    /// Nonempty field traversal within the trusted call context, without filters.
    pub path: Vec<Identifier>,
}

/// The basic value types accepted by input and context declarations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputType {
    /// A finite numeric value with no distinction between integers and decimals.
    Number,
    /// Text supplied by an input or context binding.
    String,
    /// A true or false value.
    Boolean,
}

/// One operation for selecting a value from a capability response.
#[derive(Debug, PartialEq)]
pub enum SelectStep {
    /// Access a field of the current message.
    Field(Identifier),
    /// Select the unique array element whose field equals the literal.
    Filter {
        /// The element field to compare.
        field: Identifier,
        /// The constant used for equality matching.
        value: Literal,
    },
}

/// Selects the capability invocations to which a rule applies.
#[derive(Debug, PartialEq, Eq)]
pub enum TriggerClause {
    /// Match every capability invocation, without scheduling checks itself.
    Always,
    /// Apply when the call's contract ID exactly equals the declared string.
    Capability(String),
}

/// An unevaluated precondition for admitting a capability call.
#[derive(Debug)]
pub struct RequireClause {
    /// The Boolean condition checked before dispatch; false invokes the declared enforcement.
    pub expression: Expression,
}

/// An unevaluated postcondition for accepting a capability's reported success.
#[derive(Debug)]
pub struct EnsureClause {
    /// The expression to check before accepting successful completion.
    pub expression: Expression,
}

/// The rule's single response when a safety requirement is violated.
#[derive(Debug, PartialEq, Eq)]
pub struct EnforceClause {
    /// The declared disposition when a safety requirement is violated.
    pub behavior: EnforceBehavior,
    /// Optional static report text, with string escapes decoded during parsing.
    pub report: Option<String>,
}

/// A declared violation response; the experimental Enforcer supports allow and deny.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforceBehavior {
    /// Permit the capability call despite the violated requirement.
    Allow,
    /// Reject the capability call.
    Deny,
    /// Wait for user intervention before proceeding.
    Pend,
}

/// An unevaluated SRDL condition or operand.
#[derive(Debug, PartialEq)]
pub enum Expression {
    Literal(Literal),
    Identifier(Identifier),
    Field {
        base: Box<Expression>,
        field: Identifier,
    },
    Call {
        callee: Box<Expression>,
        arguments: Vec<Expression>,
    },
    Unary {
        operator: UnaryOperator,
        operand: Box<Expression>,
    },
    Binary {
        operator: BinaryOperator,
        left: Box<Expression>,
        right: Box<Expression>,
    },
}

/// A constant value written in a requirement expression or selection filter.
#[derive(Debug, PartialEq)]
pub enum Literal {
    Boolean(bool),
    /// A finite numeric value; integer and decimal spellings share this representation.
    Number(f64),
    String(String),
}

/// Prefix operations, applied before binary operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Not,
    Negate,
}

/// Binary operations in increasing precedence groups: logical, comparison, arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Or,
    And,
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
}
