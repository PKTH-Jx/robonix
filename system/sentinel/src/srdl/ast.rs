//! The Abstract Syntax Tree (AST) for the Sentinel Rule Definition Language (SRDL).

/// An SRDL file containing reusable rules and their capability associations.
#[derive(Debug)]
pub struct Program {
    /// The list of rules in the program.
    pub rules: Vec<Rule>,
    /// Top-level associations selecting which rules apply to capability calls.
    pub triggers: Vec<TriggerClause>,
    /// Directed capability restrictions, independent of predicate rules.
    pub excludes: Vec<ExcludeClause>,
}

/// A directed exclusion, optionally expanded in both directions when loading.
#[derive(Debug, PartialEq, Eq)]
pub struct ExcludeClause {
    /// Admitting this contract forbids new calls to the target contract.
    pub source: String,
    /// Contract forbidden while a source invocation remains admitted.
    pub target: String,
    /// Also forbid the source while a target invocation remains admitted.
    pub mutual: bool,
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
    /// Conditions evaluated whenever a lifecycle trigger selects this rule.
    pub requires: Vec<RequireClause>,
    /// The response declared for a violated safety condition.
    pub enforce: EnforceClause,
}

/// A source identifier used for names and selected fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identifier(
    /// The identifier spelling in the source.
    pub String,
);

/// Capability observations shared by selections and independent yield pipelines.
#[derive(Debug)]
pub struct InputClause {
    /// Capability contracts and their JSON wrapping resolved by the input Adapter.
    pub source: InputSource,
    /// Deterministic selections from the acquired source JSON.
    pub selects: Vec<SelectField>,
    /// Independently executed pipelines producing named inputs.
    pub yields: Vec<YieldClause>,
}

/// Capability acquisition shape, preserving list wrapping for a single source.
#[derive(Debug)]
pub enum InputSource {
    /// Return this capability's JSON directly.
    Single(String),
    /// Wrap capability results under numbered object keys.
    List(Vec<String>),
}

impl InputSource {
    pub fn capabilities(&self) -> &[String] {
        match self {
            Self::Single(capability) => std::slice::from_ref(capability),
            Self::List(capabilities) => capabilities,
        }
    }
}

impl std::fmt::Display for InputSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Single(capability) => f.write_str(capability),
            Self::List(capabilities) => write!(f, "{capabilities:?}"),
        }
    }
}

impl InputClause {
    pub fn fields(&self) -> impl Iterator<Item = InputFieldRef<'_>> {
        self.selects.iter().map(InputFieldRef::Select).chain(
            self.yields
                .iter()
                .flat_map(|clause| &clause.fields)
                .map(InputFieldRef::Yield),
        )
    }
}

/// Required output bindings and their ordered processing pipeline.
#[derive(Debug)]
pub struct YieldClause {
    /// Fields read by name from the final strategy output.
    pub fields: Vec<YieldField>,
    /// Nonempty sequence of configured strategies.
    pub strategies: Vec<StrategyCall>,
}

/// A strategy name and its positional string configuration.
#[derive(Debug)]
pub struct StrategyCall {
    /// Registry name identifying the shared strategy implementation.
    pub name: Identifier,
    /// Strings interpolated using explicitly imported context bindings.
    pub arguments: Vec<String>,
}

/// One named value selected from the acquired source JSON.
#[derive(Debug)]
pub struct SelectField {
    /// Declared scalar or optional type of the selected response value.
    pub data_type: InputType,
    /// Permitted scalar literals for an enum; empty for unconstrained types.
    pub allowed_values: Vec<Literal>,
    /// The name referenced by expressions in this rule.
    pub name: Identifier,
    /// Response-relative traversal; an empty path selects the complete response.
    pub select: Vec<SelectStep>,
    /// Maximum elapsed time from acquisition to evaluation, in seconds; None never expires.
    pub maxage: Option<f64>,
}

/// One named observation produced by a strategy pipeline.
#[derive(Debug)]
pub struct YieldField {
    /// Declared scalar or optional type required from the final pipeline output.
    pub data_type: InputType,
    /// Permitted scalar literals for an enum; empty for unconstrained types.
    pub allowed_values: Vec<Literal>,
    /// Output key and binding name used by rule expressions.
    pub name: Identifier,
    /// Semantic observation requested from the pipeline.
    pub description: String,
    /// Maximum age from acquisition, in seconds; None never expires.
    pub maxage: Option<f64>,
}

/// Borrowed declaration metadata used when scheduling and validating inputs.
#[derive(Debug, Clone, Copy)]
pub enum InputFieldRef<'a> {
    /// Deterministic response selection.
    Select(&'a SelectField),
    /// Named strategy output.
    Yield(&'a YieldField),
}

impl<'a> InputFieldRef<'a> {
    pub fn name(self) -> &'a Identifier {
        match self {
            Self::Select(field) => &field.name,
            Self::Yield(field) => &field.name,
        }
    }

    pub fn data_type(self) -> InputType {
        match self {
            Self::Select(field) => field.data_type.clone(),
            Self::Yield(field) => field.data_type.clone(),
        }
    }

    pub fn allowed_values(self) -> &'a [Literal] {
        match self {
            Self::Select(field) => &field.allowed_values,
            Self::Yield(field) => &field.allowed_values,
        }
    }

    pub fn maxage(self) -> Option<f64> {
        match self {
            Self::Select(field) => field.maxage,
            Self::Yield(field) => field.maxage,
        }
    }
}

/// One typed binding to a field in the Executor's current call context.
#[derive(Debug)]
pub struct ContextClause {
    /// Declared type available to requirement expressions.
    pub data_type: InputType,
    /// Permitted scalar literals for an enum; empty for unconstrained types.
    pub allowed_values: Vec<Literal>,
    /// The alias sharing the rule's scope with input fields.
    pub name: Identifier,
    /// Nonempty field traversal within the trusted call context, without filters.
    pub path: Vec<Identifier>,
}

/// Scalar and optional types used by declarations and expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputType {
    /// A finite numeric value with no distinction between integers and decimals.
    Number,
    /// Text supplied by an input or context binding.
    String,
    /// A true or false value.
    Boolean,
    /// A present value of the contained type or an unavailable observation.
    Option(Box<InputType>),
}

impl std::fmt::Display for InputType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number => f.write_str("number"),
            Self::String => f.write_str("string"),
            Self::Boolean => f.write_str("boolean"),
            Self::Option(inner) => write!(f, "option<{inner}>"),
        }
    }
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

/// Associates every named rule with every listed capability selector.
#[derive(Debug, PartialEq)]
pub struct TriggerClause {
    /// Rule names resolved against the complete program when loading SRDL.
    pub rules: Vec<Identifier>,
    /// Lifecycle phase in which matching rules are checked.
    pub timing: TriggerTiming,
    /// Alternative selectors; any match applies all named rules to the call.
    pub capabilities: Vec<TriggerTarget>,
    /// Multiplier of average check duration for while checks; defaults to five.
    pub interval: f64,
}

/// The capability lifecycle event that starts an independent rule check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerTiming {
    /// Check conditions before permitting capability dispatch.
    Before,
    /// Report conditions after successful capability execution.
    After,
    /// Recheck conditions throughout capability execution.
    While,
}

/// A capability selector on the right side of a trigger declaration.
#[derive(Debug, PartialEq, Eq)]
pub enum TriggerTarget {
    /// Match every capability invocation, without scheduling checks itself.
    Always,
    /// Apply when the call's contract ID exactly equals the declared string.
    Capability(String),
}

/// An unevaluated condition used by either trigger timing.
#[derive(Debug)]
pub struct RequireClause {
    /// The Boolean condition whose violation is enforced before dispatch or reported afterward.
    pub expression: Expression,
}

/// Ordered inherent hooks applied when a safety requirement is violated.
#[derive(Debug, PartialEq, Eq)]
pub struct EnforceClause {
    /// Nonempty sequence of hooks, retained in source declaration order.
    pub hooks: Vec<EnforceBehavior>,
}

/// One inherent hook declaration, bound to its implementation when rules load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnforceBehavior {
    /// Permit the capability call despite the violated requirement.
    Allow,
    /// Reject the capability call.
    Deny,
    /// Cancel the plan containing the capability being checked during execution.
    Stop,
    /// Invoke this capability with empty arguments when a requirement is violated.
    Call(String),
    /// Append decoded static text to the enforcement feedback.
    Report(String),
}

/// An unevaluated SRDL condition or operand.
#[derive(Debug, PartialEq)]
pub enum Expression {
    /// Unwrap an option, propagating absence to a paired fallback.
    Try(Box<Expression>),
    /// Exhaustively evaluate one arm of an optional observation.
    Match {
        /// Expression yielding the optional observation.
        value: Box<Expression>,
        /// Arm-local alias for the present value.
        binding: Identifier,
        /// Expression evaluated when the observation is present.
        some: Box<Expression>,
        /// Expression evaluated when the observation is absent.
        none: Box<Expression>,
    },
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
#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(untagged)]
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

/// Binary operations in increasing precedence groups: fallback, logical, comparison, arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    /// Return the left value, evaluating the right side only on optional absence.
    Fallback,
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
