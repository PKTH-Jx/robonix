//! Static checks for trigger references and binding types, without resolving source schemas.

use std::collections::{BTreeMap, HashMap};

use super::{adapter::strategy::Parts, enforcer::hooks};

use crate::srdl::{
    ast::*,
    visitor::{Visitor, walk_program, walk_rule},
};

/// A static type error located by rule and condition.
#[derive(Debug, PartialEq, Eq)]
pub struct TypeError {
    /// The rule containing the invalid declaration or expression.
    pub rule: String,
    /// The one-based condition index; None for declarations.
    pub requirement: Option<usize>,
    /// The failed type constraint.
    pub kind: TypeErrorKind,
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rule '{}'", self.rule)?;
        if let Some(requirement) = self.requirement {
            write!(f, ", require #{requirement}")?;
        }
        write!(f, ": {}", self.kind)
    }
}

impl std::error::Error for TypeError {}

/// Invalid rule references, declarations, or operations on scalar and optional values.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TypeErrorKind {
    #[error("duplicate rule '{0}'")]
    DuplicateRule(String),
    #[error("trigger references unknown rule '{0}'")]
    UnknownRule(String),
    #[error("duplicate {timing:?} trigger for capability '{capability}'")]
    DuplicateTrigger {
        /// Lifecycle timing for which this rule already has an overlapping declaration.
        timing: TriggerTiming,
        /// Contract selected twice, or always when both selectors match every capability.
        capability: String,
    },
    #[error("hook '{hook}' is not valid for {timing:?} triggers")]
    InvalidHookTiming {
        /// Built-in response whose timing constraint was violated.
        hook: &'static str,
        /// Trigger lifecycle event incompatible with the hook.
        timing: TriggerTiming,
    },
    #[error("{0}")]
    InvalidHook(String),
    #[error("invalid strategy argument: {0}")]
    InvalidStrategyArgument(String),
    #[error("duplicate binding '{0}'")]
    DuplicateBinding(String),
    #[error("unknown binding '{0}'")]
    UnknownBinding(String),
    #[error("expected an option, found {0:?}")]
    ExpectedOption(InputType),
    #[error("'?' must be paired with a fallback '|'")]
    UnpairedTry,
    #[error("'|' requires a try expression on its left, found {0:?}")]
    ExpectedTry(InputType),
    #[error("an expression cannot combine multiple unhandled try values")]
    MultipleTryValues,
    #[error("match arms have incompatible types: {some:?} and {none:?}")]
    MatchArms {
        /// Type returned by the present-value arm.
        some: InputType,
        /// Type returned by the absent-value arm.
        none: InputType,
    },
    #[error("require must be boolean, found {0:?}")]
    NonBooleanRequire(InputType),
    #[error("unary operator {operator:?} does not accept {operand:?}")]
    InvalidUnary {
        /// The prefix operation being checked.
        operator: UnaryOperator,
        /// The inferred operand type.
        operand: InputType,
    },
    #[error("binary operator {operator:?} does not accept {left:?} and {right:?}")]
    InvalidBinary {
        /// The operation being checked.
        operator: BinaryOperator,
        /// The inferred left operand type.
        left: InputType,
        /// The inferred right operand type.
        right: InputType,
    },
    #[error("cannot type-check {0} without type information")]
    UnsupportedExpression(&'static str),
}

/// Inferred value type, with optional absence introduced only by a '?' expression.
#[derive(Debug, PartialEq, Eq)]
enum ExpressionType {
    /// A declared value type whose evaluation does not propagate optional absence.
    Value(InputType),
    /// An intermediate result that can propagate optional absence to a fallback.
    Try(InputType),
}

impl ExpressionType {
    fn data_type(&self) -> &InputType {
        match self {
            Self::Value(data_type) | Self::Try(data_type) => data_type,
        }
    }

    fn combine(self, other: Self, result: InputType) -> Result<Self, TypeErrorKind> {
        match (self, other) {
            (Self::Try(_), Self::Try(_)) => Err(TypeErrorKind::MultipleTryValues),
            (Self::Try(_), _) | (_, Self::Try(_)) => Ok(Self::Try(result)),
            _ => Ok(Self::Value(result)),
        }
    }
}

/// Infers expression types using one shared input and context scope per rule.
#[derive(Default, Clone)]
pub struct TypeChecker {
    /// Input and context aliases with their declared types in the current rule.
    bindings: BTreeMap<String, InputType>,
    /// The rule name used in diagnostics.
    rule: String,
    /// The current condition index within this rule.
    requirement: Option<usize>,
}

impl TypeChecker {
    /// Check a complete program, returning the first static type error.
    pub fn check(program: &Program) -> Result<(), TypeError> {
        Self::default().visit_program(program)
    }

    fn error(&self, kind: TypeErrorKind) -> TypeError {
        TypeError {
            rule: self.rule.clone(),
            requirement: self.requirement,
            kind,
        }
    }

    fn declare(&mut self, name: &Identifier, data_type: InputType) -> Result<(), TypeError> {
        if self.bindings.insert(name.0.clone(), data_type).is_some() {
            return Err(self.error(TypeErrorKind::DuplicateBinding(name.0.clone())));
        }
        Ok(())
    }

    fn infer_expression(&self, expression: &Expression) -> Result<InputType, TypeError> {
        match self.infer(expression)? {
            ExpressionType::Value(data_type) => Ok(data_type),
            ExpressionType::Try(_) => Err(self.error(TypeErrorKind::UnpairedTry)),
        }
    }

    fn infer(&self, expression: &Expression) -> Result<ExpressionType, TypeError> {
        use BinaryOperator as B;
        use ExpressionType::{Try, Value};
        use InputType as T;
        match expression {
            Expression::Try(value) => match self.infer(value)? {
                Value(T::Option(inner)) => Ok(Try(*inner)),
                actual => {
                    Err(self.error(TypeErrorKind::ExpectedOption(actual.data_type().clone())))
                }
            },
            Expression::Match {
                value,
                binding,
                some,
                none,
            } => {
                let actual = self.infer(value)?;
                let Value(T::Option(inner)) = actual else {
                    return Err(
                        self.error(TypeErrorKind::ExpectedOption(actual.data_type().clone()))
                    );
                };
                let mut scoped = self.clone();
                scoped.bindings.insert(binding.0.clone(), *inner);
                let some = scoped.infer(some)?;
                let none = self.infer(none)?;
                if some.data_type() != none.data_type() {
                    return Err(self.error(TypeErrorKind::MatchArms {
                        some: some.data_type().clone(),
                        none: none.data_type().clone(),
                    }));
                }
                let result = some.data_type().clone();
                some.combine(none, result).map_err(|kind| self.error(kind))
            }
            Expression::Literal(Literal::Number(_)) => Ok(Value(T::Number)),
            Expression::Literal(Literal::String(_)) => Ok(Value(T::String)),
            Expression::Literal(Literal::Boolean(_)) => Ok(Value(T::Boolean)),
            Expression::Identifier(Identifier(name)) => self
                .bindings
                .get(name)
                .cloned()
                .map(Value)
                .ok_or_else(|| self.error(TypeErrorKind::UnknownBinding(name.clone()))),
            Expression::Unary { operator, operand } => {
                let operand_type = self.infer(operand)?;
                match (operator, operand_type.data_type()) {
                    (UnaryOperator::Negate, T::Number) | (UnaryOperator::Not, T::Boolean) => {
                        Ok(operand_type)
                    }
                    _ => Err(self.error(TypeErrorKind::InvalidUnary {
                        operator: *operator,
                        operand: operand_type.data_type().clone(),
                    })),
                }
            }
            Expression::Binary {
                operator,
                left,
                right,
            } => {
                let left = self.infer(left)?;
                let right = self.infer(right)?;
                let left_type = left.data_type();
                let right_type = right.data_type();
                if *operator == B::Fallback {
                    let Try(left_type) = left else {
                        return Err(self.error(TypeErrorKind::ExpectedTry(left_type.clone())));
                    };
                    if left_type == *right_type && !matches!(left_type, T::Option(_)) {
                        return Ok(right);
                    }
                    return Err(self.error(TypeErrorKind::InvalidBinary {
                        operator: *operator,
                        left: left_type,
                        right: right_type.clone(),
                    }));
                }
                let result = match operator {
                    B::Add | B::Subtract | B::Multiply | B::Divide | B::Remainder
                        if *left_type == T::Number && *right_type == T::Number =>
                    {
                        T::Number
                    }
                    B::Lt | B::Lte | B::Gt | B::Gte
                        if *left_type == T::Number && *right_type == T::Number =>
                    {
                        T::Boolean
                    }
                    B::And | B::Or if *left_type == T::Boolean && *right_type == T::Boolean => {
                        T::Boolean
                    }
                    B::Eq | B::Neq if left_type == right_type => T::Boolean,
                    _ => {
                        return Err(self.error(TypeErrorKind::InvalidBinary {
                            operator: *operator,
                            left: left_type.clone(),
                            right: right_type.clone(),
                        }));
                    }
                };
                left.combine(right, result).map_err(|kind| self.error(kind))
            }
            Expression::Field { .. } => {
                Err(self.error(TypeErrorKind::UnsupportedExpression("field access")))
            }
            Expression::Call { .. } => {
                Err(self.error(TypeErrorKind::UnsupportedExpression("function calls")))
            }
        }
    }
}

impl<'ast> Visitor<'ast> for TypeChecker {
    type Error = TypeError;

    fn visit_program(&mut self, program: &Program) -> Result<(), TypeError> {
        self.requirement = None;
        let mut names = BTreeMap::new();
        for rule in &program.rules {
            if names.insert(&rule.name.0, rule).is_some() {
                self.rule.clone_from(&rule.name.0);
                return Err(self.error(TypeErrorKind::DuplicateRule(rule.name.0.clone())));
            }
        }
        // Resolve after collecting all definitions so triggers may precede their rules.
        let mut associations = HashMap::<(&str, TriggerTiming), Vec<&TriggerTarget>>::new();
        for trigger in &program.triggers {
            for name in &trigger.rules {
                let Some(rule) = names.get(&name.0) else {
                    self.rule.clone_from(&name.0);
                    return Err(self.error(TypeErrorKind::UnknownRule(name.0.clone())));
                };
                // Lists form rule/capability pairs; always overlaps every specific capability.
                let targets = associations.entry((&name.0, trigger.timing)).or_default();
                for target in &trigger.capabilities {
                    if let Some(previous) = targets.iter().copied().find(|previous| {
                        *previous == target
                            || matches!(previous, TriggerTarget::Always)
                            || matches!(target, TriggerTarget::Always)
                    }) {
                        self.rule.clone_from(&name.0);
                        let capability = match (previous, target) {
                            (TriggerTarget::Capability(name), _)
                            | (_, TriggerTarget::Capability(name)) => name.clone(),
                            _ => "always".into(),
                        };
                        return Err(self.error(TypeErrorKind::DuplicateTrigger {
                            timing: trigger.timing,
                            capability,
                        }));
                    }
                    targets.push(target);
                }
                for hook in hooks::bind(&rule.enforce) {
                    if !hook.timings().contains(&trigger.timing) {
                        self.rule.clone_from(&name.0);
                        return Err(self.error(TypeErrorKind::InvalidHookTiming {
                            hook: hook.name(),
                            timing: trigger.timing,
                        }));
                    }
                }
            }
        }
        walk_program(self, program)
    }

    fn visit_rule(&mut self, rule: &Rule) -> Result<(), TypeError> {
        self.bindings.clear();
        self.rule.clone_from(&rule.name.0);
        self.requirement = None;
        for input in &rule.inputs {
            for clause in &input.yields {
                for argument in clause.strategies.iter().flat_map(|step| &step.arguments) {
                    Parts::parse(argument)
                        .and_then(|parts| parts.validate(&rule.contexts))
                        .map_err(|err| {
                            self.error(TypeErrorKind::InvalidStrategyArgument(err.to_string()))
                        })?;
                }
            }
        }
        walk_rule(self, rule)
    }

    fn visit_enforce(&mut self, enforce: &EnforceClause) -> Result<(), TypeError> {
        self.requirement = None;
        for hook in hooks::bind(enforce) {
            hook.validate()
                .map_err(|error| self.error(TypeErrorKind::InvalidHook(error.to_string())))?;
        }
        Ok(())
    }

    fn visit_input_field(&mut self, input: &SelectField) -> Result<(), TypeError> {
        self.declare(&input.name, input.data_type.clone())
    }

    fn visit_yield_field(&mut self, input: &YieldField) -> Result<(), TypeError> {
        self.declare(&input.name, input.data_type.clone())
    }

    fn visit_context(&mut self, context: &ContextClause) -> Result<(), TypeError> {
        self.declare(&context.name, context.data_type.clone())
    }

    fn visit_require(&mut self, require: &RequireClause) -> Result<(), TypeError> {
        self.requirement = Some(self.requirement.unwrap_or(0) + 1);
        let actual = self.infer_expression(&require.expression)?;
        if actual != InputType::Boolean {
            return Err(self.error(TypeErrorKind::NonBooleanRequire(actual)));
        }
        Ok(())
    }

    fn visit_expression(&mut self, expression: &Expression) -> Result<(), TypeError> {
        self.infer_expression(expression).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srdl::parse;

    fn check(expression: &str) -> Result<(), TypeError> {
        TypeChecker::check(
            &parse(&format!(
                r#"rule typed {{
            require {expression};
            input from "source" select [number x = value maxage 1,
                number y = value maxage 1, string label = label maxage 1,
                boolean ready = ready maxage 1];
            enforce deny;
        }}"#
            ))
            .unwrap(),
        )
    }

    #[test]
    fn checks_yield_types_duplicates_and_context_placeholders() {
        for (argument, valid) in [
            ("{target}", true),
            ("{{literal}} {target}", true),
            ("{missing}", false),
            ("{x}", false),
            ("{target", false),
            ("target}", false),
            ("{}", false),
        ] {
            let program = parse(&format!(
                r#"rule r {{
                context string target = arguments.target;
                input from "source" select [number x],
                    yield [boolean ready = "Ready"] using [Test("{argument}")];
                require ready && x > 0; enforce deny;
            }}"#
            ))
            .unwrap();
            assert_eq!(TypeChecker::check(&program).is_ok(), valid, "{argument}");
        }
        let program = parse(
            r#"rule r {
            context string x = arguments.target;
            input from "source" yield [boolean x = "Ready"] using [Test()];
            require x; enforce deny;
        }"#,
        )
        .unwrap();
        assert!(matches!(
            TypeChecker::check(&program).unwrap_err().kind,
            TypeErrorKind::DuplicateBinding(_)
        ));
    }

    #[test]
    fn checks_basic_types_operators_and_source_order_independence() {
        TypeChecker::check(&parse(include_str!("../../test.srdl")).unwrap()).unwrap();
        TypeChecker::check(&parse("").unwrap()).unwrap();
        for expression in ["!ready", "-x < y", "x < y && (ready || label = \"active\")"] {
            check(expression).unwrap();
        }
        for operator in ["+", "-", "*", "/", "%", "<", "<=", ">", ">="] {
            let expression = format!("x {operator} y");
            check(&format!("({expression}) = ({expression})")).unwrap();
            assert!(matches!(
                check(&format!("x {operator} ready")).unwrap_err().kind,
                TypeErrorKind::InvalidBinary {
                    left: InputType::Number,
                    right: InputType::Boolean,
                    ..
                }
            ));
        }
        for operator in ["<", "<=", ">", ">="] {
            assert!(matches!(
                check(&format!("label {operator} \"other\""))
                    .unwrap_err()
                    .kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
        for operator in ["=", "!="] {
            for (left, right) in [("x", "y"), ("label", "\"active\""), ("ready", "false")] {
                check(&format!("{left} {operator} {right}")).unwrap();
            }
            assert!(matches!(
                check(&format!("x {operator} label")).unwrap_err().kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
        for operator in ["&&", "||"] {
            check(&format!("ready {operator} true")).unwrap();
            assert!(matches!(
                check(&format!("ready {operator} x")).unwrap_err().kind,
                TypeErrorKind::InvalidBinary { .. }
            ));
        }
    }

    #[test]
    fn resolves_trigger_references_and_rejects_ambiguous_rule_names() {
        let definitions =
            "rule a { require true; enforce deny; } rule b { require true; enforce deny; }";
        let program = parse(&format!(
            "trigger [b, a] before [capA, capB]; {definitions} trigger b before capC;"
        ))
        .unwrap();
        TypeChecker::check(&program).unwrap();
        for (source, name, kind) in [
            (
                "trigger missing before capA;".to_owned(),
                "missing",
                TypeErrorKind::UnknownRule("missing".into()),
            ),
            (
                format!("{definitions} trigger [a, missing] before capA;"),
                "missing",
                TypeErrorKind::UnknownRule("missing".into()),
            ),
            (
                format!("{definitions} rule a {{ require true; enforce deny; }}"),
                "a",
                TypeErrorKind::DuplicateRule("a".into()),
            ),
        ] {
            assert_eq!(
                TypeChecker::check(&parse(&source).unwrap()).unwrap_err(),
                TypeError {
                    rule: name.into(),
                    requirement: None,
                    kind,
                }
            );
        }
    }

    #[test]
    fn rejects_duplicate_trigger_associations_at_each_timing() {
        let definition = "rule r { require true; enforce allow; }";
        for (keyword, timing) in [
            ("before", TriggerTiming::Before),
            ("while", TriggerTiming::While),
            ("after", TriggerTiming::After),
        ] {
            for declarations in [
                format!("trigger r {keyword} A; trigger r {keyword} A;"),
                format!("trigger r {keyword} [A, B]; trigger r {keyword} A;"),
                format!("trigger [r, r] {keyword} A;"),
                format!("trigger r {keyword} [A, A];"),
                format!("trigger r {keyword} always; trigger r {keyword} A;"),
                format!("trigger r {keyword} A; trigger r {keyword} always;"),
                format!("trigger r {keyword} [always, A];"),
                format!("trigger r {keyword} always; trigger r {keyword} always;"),
            ] {
                let error =
                    TypeChecker::check(&parse(&format!("{definition} {declarations}")).unwrap())
                        .unwrap_err();
                assert_eq!(error.rule, "r");
                assert!(
                    matches!(error.kind, TypeErrorKind::DuplicateTrigger { timing: actual, .. } if actual == timing),
                    "{declarations}"
                );
            }
        }
        let repeated = parse(&format!(
            "{definition} trigger r while A interval 5; trigger r while A interval 2;"
        ))
        .unwrap();
        assert_eq!(
            TypeChecker::check(&repeated).unwrap_err().kind,
            TypeErrorKind::DuplicateTrigger {
                timing: TriggerTiming::While,
                capability: "A".into()
            }
        );

        TypeChecker::check(
            &parse(&format!(
                r#"
            {definition}
            rule s {{ require true; enforce allow; }}
            trigger [r, s] before [A, B];
            trigger r after A;
            trigger r while A interval 5;
            trigger r while B interval 2;
        "#
            ))
            .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn rejects_invalid_unary_non_boolean_and_untyped_expressions() {
        for (expression, expected) in [
            (
                "!x",
                TypeErrorKind::InvalidUnary {
                    operator: UnaryOperator::Not,
                    operand: InputType::Number,
                },
            ),
            (
                "-ready",
                TypeErrorKind::InvalidUnary {
                    operator: UnaryOperator::Negate,
                    operand: InputType::Boolean,
                },
            ),
            ("x + 1", TypeErrorKind::NonBooleanRequire(InputType::Number)),
            ("label", TypeErrorKind::NonBooleanRequire(InputType::String)),
            (
                "ready()",
                TypeErrorKind::UnsupportedExpression("function calls"),
            ),
            (
                "x.value > 0",
                TypeErrorKind::UnsupportedExpression("field access"),
            ),
            (
                "true || missing",
                TypeErrorKind::UnknownBinding("missing".into()),
            ),
        ] {
            assert_eq!(
                check(expression).unwrap_err().kind,
                expected,
                "{expression}"
            );
        }
        assert_eq!(
            check("ready()").unwrap_err().to_string(),
            "rule 'typed', require #1: cannot type-check function calls without type information"
        );
    }

    #[test]
    fn checks_after_rules_with_the_same_condition_types_and_scope() {
        for (condition, valid) in [
            ("ready", true),
            ("ready + 1 > 0", false),
            ("missing", false),
            ("1", false),
        ] {
            let program = parse(&format!(
                "trigger r after capA; rule r {{ context boolean ready = arguments.ready; require {condition}; enforce allow; }}"
            )).unwrap();
            assert_eq!(TypeChecker::check(&program).is_ok(), valid, "{condition}");
        }
    }

    #[test]
    fn validates_hook_arguments_and_every_trigger_binding() {
        for source in [
            "trigger r before A; rule r { require true; enforce stop; }",
            "trigger r after A; rule r { require true; enforce stop; }",
            "trigger r while A; rule r { require true; enforce deny; }",
        ] {
            assert!(
                TypeChecker::check(&parse(source).unwrap()).is_err(),
                "{source}"
            );
        }
        TypeChecker::check(
            &parse("trigger r while A; rule r { require false; enforce stop; }").unwrap(),
        )
        .unwrap();
        for source in [
            "trigger r after A; rule r { require true; enforce deny; }",
            "trigger r before A; trigger r after B; rule r { require true; enforce deny; }",
            "rule r { require true; enforce deny; } trigger [r] after always;",
            "trigger r after A; rule r { require true; enforce [allow, report \"bad timing\", deny]; }",
        ] {
            let error = TypeChecker::check(&parse(source).unwrap()).unwrap_err();
            assert_eq!(error.rule, "r");
            assert_eq!(error.requirement, None);
            assert_eq!(
                error.kind,
                TypeErrorKind::InvalidHookTiming {
                    hook: "deny",
                    timing: TriggerTiming::After,
                }
            );
        }
        for behavior in [
            "allow",
            r#"call "robot/stop""#,
            r#"[report "observed", call "robot/stop", allow]"#,
        ] {
            TypeChecker::check(&parse(&format!(
                "trigger r before A; trigger r after B; rule r {{ require false; enforce {behavior}; }}"
            )).unwrap()).unwrap();
        }
        for behavior in [r#"call """#, r#"call "  ""#] {
            assert!(matches!(
                TypeChecker::check(
                    &parse(&format!("rule r {{ require false; enforce {behavior}; }}")).unwrap()
                )
                .unwrap_err()
                .kind,
                TypeErrorKind::InvalidHook(_)
            ));
        }
    }

    #[test]
    fn isolates_rule_scopes_and_reports_requirement_number() {
        let first = r#"rule first {
            input from "source" select number x = value maxage 1;
            require x > 0; enforce deny; }"#;
        let second = "rule second { require true; require x > 0; enforce deny; }";
        let program = parse(&format!("{first} {second}")).unwrap();
        assert_eq!(
            TypeChecker::check(&program).unwrap_err(),
            TypeError {
                rule: "second".into(),
                requirement: Some(2),
                kind: TypeErrorKind::UnknownBinding("x".into()),
            }
        );
        let second = r#"rule second {
            input from "source" select boolean x = value maxage 1;
            require x; enforce deny; }"#;
        TypeChecker::check(&parse(&format!("{first} {second}")).unwrap()).unwrap();
    }

    #[test]
    fn checks_context_types_and_a_shared_scope_independent_of_source_order() {
        let declarations = r#"context string user = identity.user_id;
            context boolean ready = identity.ready;
            context number limit = arguments.limit;
            input from "source" select number value maxage 1;"#;
        for (expression, valid) in [
            ("user = \"alice\" && ready && value < limit", true),
            ("user + 1 > 0", false),
            ("ready = limit", false),
            ("limit", false),
        ] {
            let program = parse(&format!(
                "rule r {{ require {expression}; {declarations} enforce deny; }}"
            ))
            .unwrap();
            let result = TypeChecker::check(&program);
            assert_eq!(result.is_ok(), valid, "{expression}");
            if let Err(error) = result {
                assert_eq!(error.requirement, Some(1));
            }
        }
        for declarations in [
            "context string user = identity.user_id; context boolean user = identity.ready;",
            r#"context string user = identity.user_id; input from "source" select number user maxage 1;"#,
            r#"input from "source" select number user maxage 1; context string user = identity.user_id;"#,
        ] {
            let program = parse(&format!(
                "rule r {{ {declarations} require true; enforce deny; }}"
            ))
            .unwrap();
            let error = TypeChecker::check(&program).unwrap_err();
            assert_eq!(error.kind, TypeErrorKind::DuplicateBinding("user".into()));
            assert_eq!(error.requirement, None);
        }
        let program = parse(
            r#"rule first { context string user = identity.user_id; require true; enforce deny; }
            rule second { require user = "alice"; enforce deny; }"#,
        )
        .unwrap();
        assert_eq!(
            TypeChecker::check(&program).unwrap_err().kind,
            TypeErrorKind::UnknownBinding("user".into())
        );
    }

    #[test]
    fn rejects_duplicate_inputs_in_constructed_ast() {
        let mut program = parse(
            r#"rule duplicate {
            input from "source" select [number x = value maxage 1, string y = value maxage 1];
            require true; enforce deny; }"#,
        )
        .unwrap();
        program.rules[0].inputs[0].selects[1].name = Identifier("x".into());
        assert_eq!(
            TypeChecker::check(&program).unwrap_err(),
            TypeError {
                rule: "duplicate".into(),
                requirement: None,
                kind: TypeErrorKind::DuplicateBinding("x".into()),
            }
        );
    }
    #[test]
    fn checks_enum_bases_and_optional_match_scopes() {
        let declarations = r#"
            input from "source" select [
                enum number { 1, 2 } n = n,
                enum string { "A", "B" } label = label,
                option<number> tilt = tilt,
                option<boolean> ready = ready
            ];
        "#;
        for (expression, valid) in [
            ("n + 1 > 0 && label = \"A\"", true),
            ("label = \"outside-enum\"", true),
            ("tilt? < n | false", true),
            ("ready? | false", true),
            (
                "true && match tilt { some(v) => v < n, none => false }",
                true,
            ),
            ("match tilt { some(n) => n < 1, none => n = 2 }", true),
            ("tilt < n", false),
            ("n? > 0", false),
            ("match n { some(v) => true, none => false }", false),
            ("match tilt { some(v) => v, none => false }", false),
            ("match tilt { some(v) => true, none => v < 1 }", false),
            (
                "(match tilt { some(v) => true, none => false }) && v > 0",
                false,
            ),
        ] {
            let program = parse(&format!(
                "rule r {{ {declarations} require {expression}; enforce deny; }}"
            ))
            .unwrap();
            assert_eq!(TypeChecker::check(&program).is_ok(), valid, "{expression}");
        }
    }

    #[test]
    fn checks_fallback_types_and_requires_paired_options() {
        let check = |expression| {
            let program = parse(&format!(
                r#"rule r {{
                    input from "source" select [
                        option<number> a, option<number> b,
                        option<string> label, option<boolean> ready,
                        enum number {{ 1, 2 }} n
                    ];
                    require {expression}; enforce deny;
                }}"#
            ))
            .unwrap();
            TypeChecker::check(&program)
        };
        for expression in [
            "a? <= 10 | false",
            "(a? | b? | 90) <= 10",
            "(-(a? | b?) | 90) <= 10",
            "((a? | b?) | 90) <= 10",
            "(a? | n) <= 10",
            r#"(label? | "unknown") = "cup""#,
            "ready? | false",
            "(a? <= 10 | false) && (b? <= 1 | false)",
            "false && a? > 0 | false",
            "(match a { some(v) => v, none => b? } | 0) >= 0",
        ] {
            check(expression).unwrap_or_else(|error| panic!("{expression}: {error}"));
        }
        for expression in ["a? <= 10", "(a? | b?) <= 10", "true || a? > 0"] {
            assert_eq!(
                check(expression).unwrap_err().kind,
                TypeErrorKind::UnpairedTry
            );
        }
        for expression in ["n > 0 | false", "(a? | 90) < 10 | false"] {
            assert_eq!(
                check(expression).unwrap_err().kind,
                TypeErrorKind::ExpectedTry(InputType::Boolean),
                "{expression}"
            );
        }
        for expression in [
            "a? + b? > 0 | false",
            "a? = b? | false",
            "(match ready { some(v) => a?, none => b? } | 90) > 0",
        ] {
            assert_eq!(
                check(expression).unwrap_err().kind,
                TypeErrorKind::MultipleTryValues,
                "{expression}"
            );
        }
        assert_eq!(
            check("((a? | b?)? | 90) < 10").unwrap_err().kind,
            TypeErrorKind::ExpectedOption(InputType::Number)
        );
        for expression in [r#"(a? | "unknown") > 0"#, "a? > 0 | 0", "(a? | a) > 0"] {
            assert!(
                matches!(
                    check(expression).unwrap_err().kind,
                    TypeErrorKind::InvalidBinary {
                        operator: BinaryOperator::Fallback,
                        ..
                    }
                ),
                "{expression}"
            );
        }
    }
}
